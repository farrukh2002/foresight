use std::collections::HashMap;
use std::fs::File;
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::path::Path;

use crate::util::{expand_home, fnv1a64, now_secs};
use crate::vocab::Vocab;

const MAGIC: &[u8; 4] = b"BPM4";
const LEGACY_MAGIC: &[u8; 4] = b"BPM2";
const LEGACY_MAGIC3: &[u8; 4] = b"BPM3";

const HALF_LIFE_DAYS: f64 = 14.0;
const CWD_BONUS: f64 = 2.0;
const CONTEXT_WEIGHT: f64 = 8.0;
const SEQ_BONUS: f64 = 8.0;

pub fn line_shape(text: &str) -> u64 {
    let mut toks = text.split_whitespace();
    let mut first = toks.next();
    if first == Some("sudo") {
        first = toks.next();
    }
    let Some(first) = first else { return 0 };
    let mut h = fnv1a64(first.as_bytes());
    if let Some(second) = toks.next() {
        h = h.wrapping_mul(31).wrapping_add(fnv1a64(second.as_bytes()));
    }
    if h == 0 { 1 } else { h }
}

pub fn recency_weight(last_used: u64, now: u64) -> f64 {
    let age_days = now.saturating_sub(last_used) as f64 / 86_400.0;
    0.5f64.powf(age_days / HALF_LIFE_DAYS)
}

pub struct LineEntry {
    pub text: String,
    pub count: u32,
    pub last_used: u64,
    pub cwd_counts: HashMap<String, u32>,
    pub shape: u64,
}

#[derive(Default)]
pub struct Model {
    pub vocab: Vocab,
    pub word_freq: Vec<u32>,
    pub total_words: u64,
    pub bigram: HashMap<u32, HashMap<u32, u32>>,
    pub trigram: HashMap<(u32, u32), HashMap<u32, u32>>,
    pub lines: Vec<LineEntry>,
    line_index: HashMap<String, usize>,
    sorted_line_ids: Vec<u32>,
    pub path_freq: HashMap<String, (u32, u64)>,
    pub shape_bigram: HashMap<(u64, u64), (u32, u64)>,
    pub version: u64,
}

impl Model {
    fn intern_word(&mut self, w: &str) -> u32 {
        let id = self.vocab.intern(w);
        if id as usize == self.word_freq.len() {
            self.word_freq.push(0);
        }
        self.word_freq[id as usize] += 1;
        self.total_words += 1;
        id
    }

    pub fn train(&mut self, line: &str, home: &str, cwd: &str, now: u64) {
        let line = line.trim();
        if line.is_empty() {
            return;
        }
        self.version += 1;

        if let Some(&idx) = self.line_index.get(line) {
            let entry = &mut self.lines[idx];
            entry.count += 1;
            entry.last_used = now;
            if !cwd.is_empty() {
                *entry.cwd_counts.entry(cwd.to_string()).or_insert(0) += 1;
            }
        } else {
            let mut cwd_counts = HashMap::new();
            if !cwd.is_empty() {
                cwd_counts.insert(cwd.to_string(), 1);
            }
            let idx = self.lines.len();
            self.line_index.insert(line.to_string(), idx);
            let pos = self.sorted_line_ids.partition_point(|&i| self.lines[i as usize].text.as_str() < line);
            self.sorted_line_ids.insert(pos, idx as u32);
            let shape = line_shape(line);
            self.lines.push(LineEntry { text: line.to_string(), count: 1, last_used: now, cwd_counts, shape });
        }

        let raw_tokens: Vec<&str> = line.split_whitespace().collect();
        for tok in &raw_tokens {
            let is_anchored = tok.starts_with('/') || tok.starts_with('~');
            if let Some(abs) = expand_home(tok, home, Some(cwd))
                && (is_anchored || Path::new(&abs).exists())
            {
                let e = self.path_freq.entry(abs).or_insert((0, now));
                e.0 += 1;
                e.1 = now;
            }
        }

        let tokens: Vec<u32> = raw_tokens.iter().map(|w| self.intern_word(w)).collect();
        self.add_token_votes(&tokens, 1);
    }

    fn add_token_votes(&mut self, tokens: &[u32], sign: i64) {
        for i in 0..tokens.len() {
            if i >= 1 {
                let e = self.bigram.entry(tokens[i - 1]).or_default().entry(tokens[i]).or_insert(0);
                *e = (*e as i64 + sign).max(0) as u32;
            }
            if i >= 2 {
                let e = self
                    .trigram
                    .entry((tokens[i - 2], tokens[i - 1]))
                    .or_default()
                    .entry(tokens[i])
                    .or_insert(0);
                *e = (*e as i64 + sign).max(0) as u32;
            }
        }
        if sign < 0 {
            self.total_words = self.total_words.saturating_sub(tokens.len() as u64);
        } else {
            self.total_words += tokens.len() as u64;
        }
    }

    fn add_line_votes(&mut self, text: &str, sign: i64) {
        let ids: Vec<u32> = text.split_whitespace().filter_map(|w| self.vocab.get(w)).collect();
        for &id in &ids {
            if (id as usize) < self.word_freq.len() {
                let v = self.word_freq[id as usize];
                self.word_freq[id as usize] = (v as i64 + sign).max(0) as u32;
            }
        }
        self.add_token_votes(&ids, sign);
    }

    pub fn record_transition(&mut self, prev: u64, cur: u64, now: u64) {
        if prev == 0 || cur == 0 {
            return;
        }
        let e = self.shape_bigram.entry((prev, cur)).or_insert((0, now));
        e.0 += 1;
        e.1 = now;
    }

    fn seq_strength(&self, prev: u64, cur: u64, now: u64) -> f64 {
        if prev == 0 {
            return 0.0;
        }
        self.shape_bigram.get(&(prev, cur)).map_or(0.0, |&(c, last)| c as f64 * recency_weight(last, now))
    }

    pub fn holdout_predict(&mut self, idx: usize, partial: &str, cwd: &str, now: u64, prev_shape: Option<u64>) -> (char, String) {
        if idx >= self.lines.len() {
            return ('\0', String::new());
        }
        let text = self.lines[idx].text.clone();
        self.add_line_votes(&text, -1);
        self.lines[idx].count = self.lines[idx].count.saturating_sub(1);
        let result = self.predict_with(partial, cwd, now, true, prev_shape);
        self.lines[idx].count += 1;
        self.add_line_votes(&text, 1);
        result
    }

    #[cfg(test)]
    pub fn predict(&self, partial: &str, cwd: &str, now: u64) -> (char, String) {
        self.predict_with(partial, cwd, now, true, None)
    }

    pub fn predict_with(&self, partial: &str, cwd: &str, now: u64, allow_word_prefix: bool, prev_shape: Option<u64>) -> (char, String) {
        if partial.is_empty() {
            return ('\0', String::new());
        }

        if let Some(cont) = self.best_line_continuation(partial, cwd, now, prev_shape) {
            return ('L', cont);
        }

        let ends_with_space = partial.ends_with(' ');
        let tokens: Vec<&str> = partial.split_whitespace().collect();

        if ends_with_space || tokens.is_empty() {
            let ctx_ids: Vec<u32> = tokens.iter().filter_map(|w| self.vocab.get(w)).collect();
            if let Some((tag, word_id)) = self.backoff_next_word(&ctx_ids) {
                return (tag, self.vocab.text(word_id).to_string());
            }
            return ('\0', String::new());
        }

        if !allow_word_prefix {
            return ('\0', String::new());
        }

        let prefix = tokens[tokens.len() - 1];
        let prev_id = if tokens.len() >= 2 { self.vocab.get(tokens[tokens.len() - 2]) } else { None };
        if let Some(best) = self.best_word_prefix(prefix, prev_id) {
            return ('W', best[prefix.len()..].to_string());
        }
        ('\0', String::new())
    }

    fn line_score(entry: &LineEntry, cwd: &str, now: u64, seq: f64) -> f64 {
        let mut score = entry.count as f64 * recency_weight(entry.last_used, now);
        if seq > 0.0 {
            score *= 1.0 + SEQ_BONUS * (seq / (1.0 + seq));
        }
        if !cwd.is_empty() && entry.cwd_counts.keys().any(|c| cwd == c || cwd.starts_with(&format!("{c}/")) || c.starts_with(&format!("{cwd}/"))) {
            score += CWD_BONUS;
        }
        score
    }

    fn best_line_continuation(&self, partial: &str, cwd: &str, now: u64, prev_shape: Option<u64>) -> Option<String> {
        self.topk_line_continuations(partial, cwd, now, prev_shape, 1).into_iter().next()
    }

    fn topk_line_continuations(&self, partial: &str, cwd: &str, now: u64, prev_shape: Option<u64>, k: usize) -> Vec<String> {
        let start = self.sorted_line_ids.partition_point(|&i| self.lines[i as usize].text.as_str() < partial);
        let mut scored: Vec<(f64, &str)> = Vec::new();
        for &id in &self.sorted_line_ids[start..] {
            let entry = &self.lines[id as usize];
            if !entry.text.starts_with(partial) {
                break;
            }
            if entry.count == 0 || entry.text == partial {
                continue;
            }
            let seq = prev_shape.map_or(0.0, |p| self.seq_strength(p, entry.shape, now));
            scored.push((Self::line_score(entry, cwd, now, seq), entry.text.as_str()));
        }
        scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap().then_with(|| a.1.cmp(b.1)));
        scored.truncate(k);
        scored.into_iter().map(|(_, t)| t[partial.len()..].to_string()).collect()
    }

    fn backoff_next_word(&self, ctx_ids: &[u32]) -> Option<(char, u32)> {
        let tri = if ctx_ids.len() >= 2 { self.trigram.get(&(ctx_ids[ctx_ids.len() - 2], ctx_ids[ctx_ids.len() - 1])) } else { None };
        let bi = ctx_ids.last().and_then(|&last| self.bigram.get(&last));
        if tri.is_none() && bi.is_none() {
            return None;
        }
        let tri_total: u32 = tri.map_or(0, |m| m.values().sum());
        let bi_total: u32 = bi.map_or(0, |m| m.values().sum());

        let mut candidates: Vec<u32> = Vec::new();
        if let Some(m) = tri {
            candidates.extend(m.keys().copied());
        }
        if let Some(m) = bi {
            candidates.extend(m.keys().copied());
        }
        candidates.sort_unstable();
        candidates.dedup();

        let uni_total = self.total_words.max(1) as f64;
        let mut best: Option<(f64, u32, char)> = None;
        for &w in &candidates {
            let c3 = tri.map_or(0, |m| m.get(&w).copied().unwrap_or(0));
            let c2 = bi.map_or(0, |m| m.get(&w).copied().unwrap_or(0));
            let uni = self.word_freq.get(w as usize).copied().unwrap_or(0);
            let score = 0.6 * (if tri_total > 0 { c3 as f64 / tri_total as f64 } else { 0.0 })
                + 0.3 * (if bi_total > 0 { c2 as f64 / bi_total as f64 } else { 0.0 })
                + 0.1 * (uni as f64 / uni_total);
            let tag = if c3 > 0 { 'T' } else { 'B' };
            let better = match best {
                None => true,
                Some((bs, _, _)) => score > bs,
            };
            if better {
                best = Some((score, w, tag));
            }
        }
        best.map(|(_, w, tag)| (tag, w))
    }

    fn best_word_prefix(&self, prefix: &str, prev_id: Option<u32>) -> Option<String> {
        let bi = prev_id.and_then(|p| self.bigram.get(&p));
        let (start, end) = self.vocab.prefix_range(prefix);
        let mut best: Option<(f64, &str)> = None;
        for &id in &self.vocab.sorted_ids[start..end] {
            let word = self.vocab.text(id);
            if word == prefix {
                continue;
            }
            let ctx = bi.map_or(0, |m| m.get(&id).copied().unwrap_or(0));
            let score = ctx as f64 * CONTEXT_WEIGHT + self.word_freq.get(id as usize).copied().unwrap_or(0) as f64;
            let better = match best {
                None => true,
                Some((bs, _)) => score > bs,
            };
            if better {
                best = Some((score, word));
            }
        }
        best.map(|(_, w)| w.to_string())
    }

    #[cfg(feature = "bench")]
    pub fn top_cwd(&self, idx: usize) -> Option<&str> {
        self.lines.get(idx).and_then(|e| {
            e.cwd_counts.iter().max_by(|a, b| a.1.cmp(b.1).then_with(|| a.0.cmp(b.0))).map(|(c, _)| c.as_str())
        })
    }

    pub fn touch_line(&mut self, line: &str, now: u64) -> bool {
        match self.line_index.get(line) {
            Some(&i) => {
                self.lines[i].last_used = now;
                true
            }
            None => false,
        }
    }

    pub fn path_score(&self, p: &str, now: u64) -> f64 {
        match self.path_freq.get(p) {
            Some(&(count, last_used)) => count as f64 * recency_weight(last_used, now),
            None => 0.0,
        }
    }

    pub fn explain(&self, partial: &str, cwd: &str, now: u64, prev_shape: Option<u64>) -> String {
        let mut parts = Vec::new();
        let conts = self.topk_line_continuations(partial, cwd, now, prev_shape, 3);
        if conts.is_empty() {
            parts.push("L: none".to_string());
        } else {
            parts.push(format!("L: {}", conts.iter().map(|c| format!("\"{c}\"")).collect::<Vec<_>>().join(" ")));
        }
        let tokens: Vec<&str> = partial.split_whitespace().collect();
        let ctx_ids: Vec<u32> = tokens.iter().filter_map(|w| self.vocab.get(w)).collect();
        match self.backoff_next_word(&ctx_ids) {
            Some((tag, id)) => parts.push(format!("{tag}: \"{}\"", self.vocab.text(id))),
            None => parts.push("T/B: none".to_string()),
        }
        if !partial.ends_with(' ') && !tokens.is_empty() {
            let prefix = tokens[tokens.len() - 1];
            let prev_id = if tokens.len() >= 2 { self.vocab.get(tokens[tokens.len() - 2]) } else { None };
            let bi = prev_id.and_then(|p| self.bigram.get(&p));
            let (start, end) = self.vocab.prefix_range(prefix);
            let mut scored: Vec<(f64, &str)> = self.vocab.sorted_ids[start..end]
                .iter()
                .filter_map(|&id| {
                    let word = self.vocab.text(id);
                    if word == prefix {
                        return None;
                    }
                    let ctx = bi.map_or(0, |m| m.get(&id).copied().unwrap_or(0));
                    let score = ctx as f64 * CONTEXT_WEIGHT + self.word_freq.get(id as usize).copied().unwrap_or(0) as f64;
                    Some((score, word))
                })
                .collect();
            scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap().then_with(|| a.1.cmp(b.1)));
            if scored.is_empty() {
                parts.push("W: none".to_string());
            } else {
                parts.push(format!(
                    "W: {}",
                    scored.iter().take(3).map(|(s, w)| format!("\"{w}\"({s:.1})")).collect::<Vec<_>>().join(" ")
                ));
            }
        }
        parts.join("; ")
    }

    pub fn topk(&self, partial: &str, cwd: &str, now: u64, k: usize, prev_shape: Option<u64>) -> Vec<(char, String)> {
        let mut out: Vec<(char, String)> = Vec::new();
        for c in self.topk_line_continuations(partial, cwd, now, prev_shape, k) {
            if out.len() >= k {
                break;
            }
            out.push(('L', c));
        }
        if out.is_empty() {
            let tokens: Vec<&str> = partial.split_whitespace().collect();
            let ctx_ids: Vec<u32> = tokens.iter().filter_map(|w| self.vocab.get(w)).collect();
            if let Some((tag, id)) = self.backoff_next_word(&ctx_ids) {
                out.push((tag, self.vocab.text(id).to_string()));
            }
            if !partial.ends_with(' ') && !tokens.is_empty() {
                let prefix = tokens[tokens.len() - 1];
                let prev_id = if tokens.len() >= 2 { self.vocab.get(tokens[tokens.len() - 2]) } else { None };
                if let Some(w) = self.best_word_prefix(prefix, prev_id) {
                    out.push(('W', w[prefix.len()..].to_string()));
                }
            }
        }
        out
    }

    pub fn bootstrap_from_history(&mut self, path: &Path, home: &str, now: u64) {
        let Ok(file) = File::open(path) else { return };
        let reader = BufReader::new(file);
        let mut prev_shape = 0u64;
        for line in io::BufRead::lines(reader).map_while(Result::ok) {
            self.train(&line, home, home, now);
            let shape = line_shape(&line);
            self.record_transition(prev_shape, shape, now);
            prev_shape = shape;
        }
    }

    pub fn seed_transitions_from_history(&mut self, path: &Path, now: u64) {
        if !self.shape_bigram.is_empty() {
            return;
        }
        let Ok(file) = File::open(path) else { return };
        let mut prev_shape = 0u64;
        for line in io::BufRead::lines(BufReader::new(file)).map_while(Result::ok) {
            let shape = line_shape(&line);
            self.record_transition(prev_shape, shape, now);
            prev_shape = shape;
        }
    }

    pub fn import_zsh_history(&mut self, home: &str, now: u64) {
        let Ok(file) = File::open(Path::new(home).join(".zsh_history")) else { return };
        let reader = BufReader::new(file);
        let mut prev_shape = 0u64;
        for line in io::BufRead::lines(reader).map_while(Result::ok) {
            let cmd = match (line.starts_with(": "), line.find(';')) {
                (true, Some(i)) => &line[i + 1..],
                _ => line.as_str(),
            };
            self.train(cmd, home, home, now);
            let shape = line_shape(cmd);
            self.record_transition(prev_shape, shape, now);
            prev_shape = shape;
        }
    }

    pub fn save(&self, path: &Path) -> io::Result<()> {
        let mut body = Vec::new();

        write_u32(&mut body, self.vocab.strings.len() as u32)?;
        for s in &self.vocab.strings {
            write_str_u16(&mut body, s)?;
        }

        for &c in &self.word_freq {
            write_u32(&mut body, c)?;
        }

        write_u32(&mut body, self.bigram.len() as u32)?;
        for (ctx, edges) in &self.bigram {
            write_u32(&mut body, *ctx)?;
            write_u32(&mut body, edges.len() as u32)?;
            for (&next, &count) in edges {
                write_u32(&mut body, next)?;
                write_u32(&mut body, count)?;
            }
        }

        write_u32(&mut body, self.trigram.len() as u32)?;
        for (&(c1, c2), edges) in &self.trigram {
            write_u32(&mut body, c1)?;
            write_u32(&mut body, c2)?;
            write_u32(&mut body, edges.len() as u32)?;
            for (&next, &count) in edges {
                write_u32(&mut body, next)?;
                write_u32(&mut body, count)?;
            }
        }

        write_u32(&mut body, self.lines.len() as u32)?;
        for entry in &self.lines {
            write_u32(&mut body, entry.count)?;
            write_u64(&mut body, entry.last_used)?;
            write_str_u16(&mut body, &entry.text)?;
            write_u32(&mut body, entry.cwd_counts.len() as u32)?;
            for (cwd, &cnt) in &entry.cwd_counts {
                write_str_u16(&mut body, cwd)?;
                write_u32(&mut body, cnt)?;
            }
        }

        write_u32(&mut body, self.path_freq.len() as u32)?;
        for (path, &(count, last_used)) in &self.path_freq {
            write_u32(&mut body, count)?;
            write_u64(&mut body, last_used)?;
            write_str_u16(&mut body, path)?;
        }

        write_u32(&mut body, self.shape_bigram.len() as u32)?;
        for (&(prev, next), &(count, last_used)) in &self.shape_bigram {
            write_u64(&mut body, prev)?;
            write_u64(&mut body, next)?;
            write_u32(&mut body, count)?;
            write_u64(&mut body, last_used)?;
        }

        let checksum = fnv1a64(&body);
        let tmp = path.with_extension("bin.tmp");
        {
            let mut w = BufWriter::new(File::create(&tmp)?);
            w.write_all(MAGIC)?;
            w.write_all(&body)?;
            w.write_all(&checksum.to_le_bytes())?;
            w.flush()?;
        }
        std::fs::rename(tmp, path)
    }

    pub fn load(path: &Path) -> io::Result<Model> {
        let mut raw = BufReader::new(File::open(path)?);
        let mut magic = [0u8; 4];
        raw.read_exact(&mut magic)?;
        let (legacy2, has_seq) = match &magic {
            MAGIC => (false, true),
            LEGACY_MAGIC3 => (false, false),
            LEGACY_MAGIC => (true, false),
            _ => return Err(io::Error::new(io::ErrorKind::InvalidData, "bad magic")),
        };
        let legacy_now = std::fs::metadata(path).and_then(|m| m.modified()).map_or(now_secs(), |t| {
            t.duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(now_secs())
        });
        let mut rest = Vec::new();
        raw.read_to_end(&mut rest)?;
        if rest.len() < 8 {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "model.bin truncated"));
        }
        let split = rest.len() - 8;
        let stored = u64::from_le_bytes(rest[split..].try_into().unwrap());
        let body = &rest[..split];
        if fnv1a64(body) != stored {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "model.bin checksum mismatch - file corrupted"));
        }
        let mut r = body;

        let mut model = Model::default();

        let vocab_len = read_u32(&mut r)?;
        for _ in 0..vocab_len {
            let s = read_str_u16(&mut r)?;
            let id = model.vocab.intern(&s);
            if id as usize != model.vocab.strings.len() - 1 {
                return Err(io::Error::new(io::ErrorKind::InvalidData, "model.bin vocab table corrupted"));
            }
        }
        model.vocab.rebuild_sorted();

        model.word_freq = vec![0u32; vocab_len as usize];
        for c in model.word_freq.iter_mut() {
            *c = read_u32(&mut r)?;
        }

        let n_bigram = read_u32(&mut r)?;
        for _ in 0..n_bigram {
            let ctx = read_u32(&mut r)?;
            let n_edges = read_u32(&mut r)?;
            let bucket = model.bigram.entry(ctx).or_default();
            for _ in 0..n_edges {
                let next = read_u32(&mut r)?;
                let count = read_u32(&mut r)?;
                bucket.insert(next, count);
            }
        }

        let n_trigram = read_u32(&mut r)?;
        for _ in 0..n_trigram {
            let c1 = read_u32(&mut r)?;
            let c2 = read_u32(&mut r)?;
            let n_edges = read_u32(&mut r)?;
            let bucket = model.trigram.entry((c1, c2)).or_default();
            for _ in 0..n_edges {
                let next = read_u32(&mut r)?;
                let count = read_u32(&mut r)?;
                bucket.insert(next, count);
            }
        }

        let n_lines = read_u32(&mut r)?;
        for i in 0..n_lines {
            let count = read_u32(&mut r)?;
            let last_used = if legacy2 { legacy_now } else { read_u64(&mut r)? };
            let text = read_str_u16(&mut r)?;
            let mut cwd_counts = HashMap::new();
            if !legacy2 {
                let n_cwds = read_u32(&mut r)?;
                for _ in 0..n_cwds {
                    let cwd = read_str_u16(&mut r)?;
                    let cnt = read_u32(&mut r)?;
                    cwd_counts.insert(cwd, cnt);
                }
            }
            let shape = line_shape(&text);
            model.line_index.insert(text.clone(), i as usize);
            model.lines.push(LineEntry { text, count, last_used, cwd_counts, shape });
        }

        let n_paths = read_u32(&mut r)?;
        for _ in 0..n_paths {
            let count = read_u32(&mut r)?;
            let last_used = if legacy2 { legacy_now } else { read_u64(&mut r)? };
            let path = read_str_u16(&mut r)?;
            model.path_freq.insert(path, (count, last_used));
        }

        if has_seq {
            let n_trans = read_u32(&mut r)?;
            for _ in 0..n_trans {
                let prev = read_u64(&mut r)?;
                let next = read_u64(&mut r)?;
                let count = read_u32(&mut r)?;
                let last_used = read_u64(&mut r)?;
                model.shape_bigram.insert((prev, next), (count, last_used));
            }
        }

        model.total_words = model.word_freq.iter().map(|&c| c as u64).sum();
        model.rebuild_sorted_lines();
        model.version = 1;
        Ok(model)
    }

    fn rebuild_sorted_lines(&mut self) {
        let mut ids: Vec<u32> = (0..self.lines.len() as u32).collect();
        ids.sort_by(|&a, &b| self.lines[a as usize].text.cmp(&self.lines[b as usize].text));
        self.sorted_line_ids = ids;
    }
}

fn write_str_u16<W: Write>(w: &mut W, s: &str) -> io::Result<()> {
    if s.len() > u16::MAX as usize {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "string too long for model.bin (u16 length prefix)"));
    }
    w.write_all(&(s.len() as u16).to_le_bytes())?;
    w.write_all(s.as_bytes())
}

fn write_u64<W: Write>(w: &mut W, v: u64) -> io::Result<()> {
    w.write_all(&v.to_le_bytes())
}
fn write_u32<W: Write>(w: &mut W, v: u32) -> io::Result<()> {
    w.write_all(&v.to_le_bytes())
}
fn read_u64<R: Read>(r: &mut R) -> io::Result<u64> {
    let mut b = [0u8; 8];
    r.read_exact(&mut b)?;
    Ok(u64::from_le_bytes(b))
}
fn read_u32<R: Read>(r: &mut R) -> io::Result<u32> {
    let mut b = [0u8; 4];
    r.read_exact(&mut b)?;
    Ok(u32::from_le_bytes(b))
}
fn read_str_u16<R: Read>(r: &mut R) -> io::Result<String> {
    let mut lb = [0u8; 2];
    r.read_exact(&mut lb)?;
    let mut buf = vec![0u8; u16::from_le_bytes(lb) as usize];
    r.read_exact(&mut buf)?;
    Ok(String::from_utf8_lossy(&buf).to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn save_load_round_trip() {
        let mut m = Model::default();
        m.train("cd ~/projects/foo", "/home/u", "/home/u", 1000);
        m.train("git status", "/home/u", "/home/u/projects/foo", 2000);
        m.train("git status", "/home/u", "/home/u/projects/foo", 3000);
        m.record_transition(line_shape("git add"), line_shape("git status"), 3000);
        let tmp = std::env::temp_dir().join(format!("bp-model-test-{}.bin", std::process::id()));

        m.save(&tmp).unwrap();
        let loaded = Model::load(&tmp).unwrap();

        assert_eq!(loaded.lines.len(), m.lines.len());
        assert_eq!(loaded.vocab.strings.len(), m.vocab.strings.len());
        assert_eq!(loaded.path_freq.get("/home/u/projects/foo"), m.path_freq.get("/home/u/projects/foo"));
        assert_eq!(loaded.shape_bigram, m.shape_bigram);
        let git_status = loaded.lines.iter().find(|e| e.text == "git status").unwrap();
        assert_eq!(git_status.count, 2);
        assert_eq!(git_status.last_used, 3000);
        assert_eq!(git_status.cwd_counts.get("/home/u/projects/foo"), Some(&2));
        assert_eq!(git_status.shape, line_shape("git status"));
        std::fs::remove_file(&tmp).ok();
    }

    #[test]
    fn shape_normalizes_sudo_and_keeps_subcommands() {
        assert_eq!(line_shape("sudo apt update"), line_shape("apt update"));
        assert_eq!(line_shape("apt update"), line_shape("apt update -y"));
        assert_eq!(line_shape("git"), line_shape("sudo git"));
        assert_ne!(line_shape("apt update"), line_shape("apt upgrade"));
        assert_ne!(line_shape("git add"), line_shape("git commit"));
        assert_eq!(line_shape(""), 0);
        assert_eq!(line_shape("   "), 0);
        assert_eq!(line_shape("sudo"), 0);
    }

    #[test]
    fn sequence_bonus_flips_ranking() {
        let mut m = Model::default();
        let now = now_secs();
        for _ in 0..15 {
            m.train("sudo apt update", "/home/u", "/home/u", now);
        }
        for _ in 0..3 {
            m.train("sudo apt upgrade", "/home/u", "/home/u", now);
        }
        let list_shape = line_shape("apt list --upgradable");
        for _ in 0..3 {
            m.record_transition(list_shape, line_shape("sudo apt upgrade"), now);
        }

        let (_, s) = m.predict("sudo apt up", "", now);
        assert_eq!(s, "date");
        let (tag, s) = m.predict_with("sudo apt up", "", now, true, Some(list_shape));
        assert_eq!((tag, s.as_str()), ('L', "grade"));
    }

    #[test]
    fn transitions_persist_and_decay() {
        let mut m = Model::default();
        m.train("make build", "/home/u", "/home/u", 1000);
        m.train("make test", "/home/u", "/home/u", 1000);
        let (build, test) = (line_shape("make build"), line_shape("make test"));
        m.record_transition(build, test, 1000);

        let fresh = m.seq_strength(build, test, 1000);
        let stale = m.seq_strength(build, test, now_secs());
        assert!(fresh > stale);
        assert!(stale < 0.01);
    }

    #[test]
    fn load_rejects_corrupted_file() {
        let mut m = Model::default();
        m.train("echo hi", "/home/u", "/home/u", 1000);
        let tmp = std::env::temp_dir().join(format!("bp-model-corrupt-{}.bin", std::process::id()));
        m.save(&tmp).unwrap();

        let mut bytes = std::fs::read(&tmp).unwrap();
        let last = bytes.len() - 9;
        bytes[last] ^= 0xFF;
        std::fs::write(&tmp, &bytes).unwrap();

        assert!(Model::load(&tmp).is_err());
        std::fs::remove_file(&tmp).ok();
    }

    #[test]
    fn train_relative_path_needs_cwd_and_existence() {
        let mut m = Model::default();
        let cwd = env!("CARGO_MANIFEST_DIR");
        m.train("cat Cargo.toml", "/nonexistent-home", cwd, 1000);
        let expected = format!("{cwd}/Cargo.toml");
        assert_eq!(m.path_freq.get(&expected).map(|e| e.0), Some(1));

        m.train("grep needle haystack-does-not-exist", "/nonexistent-home", cwd, 1000);
        assert!(!m.path_freq.contains_key("needle"));
    }

    #[test]
    fn recency_breaks_count_ties_toward_recent() {
        let mut m = Model::default();
        m.train("git add old-file", "/home/u", "/home/u", 1000);
        m.train("git add new-file", "/home/u", "/home/u", now_secs());
        let (_, suffix) = m.predict("git add ", "", now_secs());
        assert_eq!(suffix, "new-file");
    }

    #[test]
    fn cwd_bonus_prefers_line_from_this_directory() {
        let mut m = Model::default();
        m.train("cargo run --release", "/home/u", "/home/u/proj-a", now_secs());
        m.train("cargo run --tests", "/home/u", "/home/u/proj-b", now_secs());
        let (_, from_a) = m.predict("cargo run ", "/home/u/proj-a", now_secs());
        let (_, from_b) = m.predict("cargo run ", "/home/u/proj-b", now_secs());
        assert_eq!(from_a, "--release");
        assert_eq!(from_b, "--tests");
    }

    #[test]
    fn interpolated_backoff_mixes_evidence() {
        let mut m = Model::default();
        for _ in 0..10 {
            m.train("git pull", "/home/u", "/home/u", 1000);
        }
        m.train("git push origin main", "/home/u", "/home/u", 1000);
        let ids: Vec<u32> = ["git"].iter().filter_map(|t| m.vocab.get(t)).collect();
        let (_, id) = m.backoff_next_word(&ids).unwrap();
        assert_eq!(m.vocab.text(id), "pull");
        let (_, word) = m.predict("git ", "", now_secs());
        assert_eq!(word, "pull");
    }

    #[test]
    fn word_prefix_context_beats_global_frequency() {
        let mut m = Model::default();
        for _ in 0..50 {
            m.train("systemctl stop nginx", "/home/u", "/home/u", 1000);
        }
        for _ in 0..5 {
            m.train("git status", "/home/u", "/home/u", 1000);
        }
        let (_, suffix) = m.predict("git st", "", now_secs());
        assert_eq!(suffix, "atus");
    }

    #[test]
    fn holdout_excludes_line_and_restores_model() {
        let mut m = Model::default();
        m.train("cargo build --release", "/home/u", "/home/u", 1000);
        m.train("cargo build", "/home/u", "/home/u", 1000);
        m.train("cargo test", "/home/u", "/home/u", 1000);

        let idx = m.lines.iter().position(|e| e.text == "cargo build --release").unwrap();
        let (_, seen) = m.predict("cargo build", "", now_secs());
        assert_eq!(seen, " --release");
        let (_, hidden) = m.holdout_predict(idx, "cargo build", "", now_secs(), None);
        assert_ne!(hidden, " --release");

        let (_, seen_again) = m.predict("cargo build", "", now_secs());
        assert_eq!(seen_again, " --release");
        let idx2 = m.lines.iter().position(|e| e.text == "cargo test").unwrap();
        let (_, _) = m.holdout_predict(idx2, "cargo", "", now_secs(), None);
        assert_eq!(m.lines[idx].count, 1);
    }

    #[test]
    fn save_rejects_oversized_line() {
        let mut m = Model::default();
        let huge = format!("echo {}", "x".repeat(70_000));
        m.train(&huge, "/home/u", "/home/u", 1000);
        let tmp = std::env::temp_dir().join(format!("bp-model-huge-{}.bin", std::process::id()));
        assert!(m.save(&tmp).is_err());
        std::fs::remove_file(&tmp).ok();
    }

    #[test]
    fn loads_legacy_bpm2() {
        let mut body = Vec::new();
        body.extend(2u32.to_le_bytes());
        for s in ["git", "status"] {
            body.extend((s.len() as u16).to_le_bytes());
            body.extend(s.as_bytes());
        }
        body.extend(1u32.to_le_bytes());
        body.extend(0u32.to_le_bytes());
        body.extend(0u32.to_le_bytes());
        body.extend(0u32.to_le_bytes());
        body.extend(1u32.to_le_bytes());
        body.extend(1u32.to_le_bytes());
        body.extend(3u16.to_le_bytes());
        body.extend(b"git");
        body.extend(0u32.to_le_bytes());

        let tmp = std::env::temp_dir().join(format!("bp-model-bpm2-{}.bin", std::process::id()));
        let mut file = Vec::new();
        file.extend(b"BPM2");
        file.extend(&body);
        file.extend(fnv1a64(&body).to_le_bytes());
        std::fs::write(&tmp, &file).unwrap();

        let m = Model::load(&tmp).unwrap();
        assert_eq!(m.lines.len(), 1);
        assert_eq!(m.lines[0].text, "git");
        assert!(m.lines[0].last_used > 0);
        assert!(m.path_freq.is_empty());
        assert_eq!(m.total_words, 1);
        std::fs::remove_file(&tmp).ok();
    }

    #[test]
    fn loads_legacy_bpm3() {
        let mut body = Vec::new();
        body.extend(1u32.to_le_bytes());
        body.extend(3u16.to_le_bytes());
        body.extend(b"git");
        body.extend(1u32.to_le_bytes());
        body.extend(0u32.to_le_bytes());
        body.extend(0u32.to_le_bytes());
        body.extend(1u32.to_le_bytes());
        body.extend(1u32.to_le_bytes());
        body.extend(42u64.to_le_bytes());
        body.extend(3u16.to_le_bytes());
        body.extend(b"git");
        body.extend(0u32.to_le_bytes());
        body.extend(0u32.to_le_bytes());

        let tmp = std::env::temp_dir().join(format!("bp-model-bpm3-{}.bin", std::process::id()));
        let mut file = Vec::new();
        file.extend(b"BPM3");
        file.extend(&body);
        file.extend(fnv1a64(&body).to_le_bytes());
        std::fs::write(&tmp, &file).unwrap();

        let m = Model::load(&tmp).unwrap();
        assert_eq!(m.lines.len(), 1);
        assert_eq!(m.lines[0].text, "git");
        assert_eq!(m.lines[0].last_used, 42);
        assert_eq!(m.lines[0].shape, line_shape("git"));
        assert!(m.shape_bigram.is_empty());
        std::fs::remove_file(&tmp).ok();
    }

    #[test]
    fn word_prefix_skipped_when_disallowed() {
        let mut m = Model::default();
        m.train("alpha beta", "/home/u", "/home/u", 1000);
        let (tag, suffix) = m.predict("xx be", "", now_secs());
        assert_eq!((tag, suffix.as_str()), ('W', "ta"));
        let (tag, suffix) = m.predict_with("xx be", "", now_secs(), false, None);
        assert_eq!(tag, '\0');
        assert!(suffix.is_empty());
    }

    #[test]
    fn line_continuation_still_allowed_without_word_prefix() {
        let mut m = Model::default();
        m.train("cargo build --release", "/home/u", "/home/u", 1000);
        let (tag, suffix) = m.predict_with("cargo bu", "", now_secs(), false, None);
        assert_eq!((tag, suffix.as_str()), ('L', "ild --release"));
    }

    #[test]
    fn sorted_views_stay_consistent() {
        let mut m = Model::default();
        for i in 0..50 {
            m.train(&format!("cmd-{i:02} arg{i}", i = i), "/home/u", "/home/u", 1000 + i);
        }
        m.rebuild_sorted_lines();
        let mut texts: Vec<&str> = m.lines.iter().map(|e| e.text.as_str()).collect();
        texts.sort();
        let via_sorted: Vec<&str> = m.sorted_line_ids.iter().map(|&i| m.lines[i as usize].text.as_str()).collect();
        assert_eq!(texts, via_sorted);

        let (start, end) = m.vocab.prefix_range("cmd-");
        assert!(end > start);
        for &id in &m.vocab.sorted_ids[start..end] {
            assert!(m.vocab.text(id).starts_with("cmd-"));
        }
    }
}
