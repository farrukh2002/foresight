use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use inotify::{EventMask, Inotify, WatchDescriptor, WatchMask};

use crate::config::Config;
use crate::util::{data_dir, fnv1a64, log};

const MAGIC: &[u8; 4] = b"BPFS";

const CAND_LIMIT: usize = 4096;
static SCAN_LIMIT_WARNED: AtomicBool = AtomicBool::new(false);

pub struct FsIndex {
    pub paths: Vec<(String, bool)>,
    pub(crate) usage: Vec<f64>,
    pub usage_version: u64,
}

impl FsIndex {
    pub fn empty() -> FsIndex {
        FsIndex { paths: Vec::new(), usage: Vec::new(), usage_version: 0 }
    }

    pub fn build(config: &Config, home: &str) -> FsIndex {
        let cap = config.max_entries();
        let (exclude_names, exclude_paths) = config.effective_excludes();
        let own_data_dir = data_dir().to_string_lossy().to_string();
        let mut paths = Vec::new();
        let mut truncated = false;

        'roots: for root in config.resolved_roots(home) {
            let mut stack = vec![PathBuf::new()];
            while let Some(rel) = stack.pop() {
                if paths.len() >= cap {
                    truncated = true;
                    break 'roots;
                }
                let abs_dir = root.join(&rel);
                let Ok(entries) = fs::read_dir(&abs_dir) else { continue };
                for entry in entries.flatten() {
                    if paths.len() >= cap {
                        truncated = true;
                        break 'roots;
                    }
                    let name = entry.file_name().to_string_lossy().to_string();
                    if exclude_names.iter().any(|n| n == &name) {
                        continue;
                    }
                    let Ok(file_type) = entry.file_type() else { continue };

                    let child_rel = if rel.as_os_str().is_empty() { PathBuf::from(&name) } else { rel.join(&name) };
                    let child_rel_str = child_rel.to_string_lossy().to_string();
                    if exclude_paths.iter().any(|p| child_rel_str == *p || child_rel_str.starts_with(&format!("{p}/"))) {
                        continue;
                    }

                    let is_symlink = file_type.is_symlink();
                    let is_dir = if is_symlink {
                        fs::metadata(entry.path()).map(|m| m.is_dir()).unwrap_or(false)
                    } else {
                        file_type.is_dir()
                    };
                    let abs_str = root.join(&child_rel).to_string_lossy().to_string();
                    if abs_str == own_data_dir || abs_str.starts_with(&format!("{own_data_dir}/")) {
                        continue;
                    }
                    paths.push((abs_str, is_dir));
                    if is_dir && !is_symlink {
                        stack.push(child_rel);
                    }
                }
            }
        }

        if truncated {
            log(&format!(
                "fs index hit the {cap}-entry cap ({} mode) and stopped early - index is partial",
                config.scan.mode
            ));
        }

        paths.sort();
        FsIndex { paths, usage: Vec::new(), usage_version: 0 }
    }

    #[cfg(any(test, feature = "bench"))]
    pub fn contains(&self, abs: &str) -> bool {
        self.paths.binary_search_by(|(p, _)| p.as_str().cmp(abs)).is_ok()
    }

    #[cfg(test)]
    fn remove(&mut self, abs: &str) {
        self.apply_batch(vec![], vec![abs.to_string()]);
    }

    fn insert_one(&mut self, abs: String, is_dir: bool) {
        match self.paths.binary_search_by(|(p, _)| p.as_str().cmp(abs.as_str())) {
            Ok(i) => self.paths[i].1 = is_dir,
            Err(i) => {
                self.paths.insert(i, (abs, is_dir));
                self.usage.insert(i, 0.0);
            }
        }
    }

    fn remove_one(&mut self, abs: &str) {
        if let Ok(i) = self.paths.binary_search_by(|(p, _)| p.as_str().cmp(abs)) {
            self.paths.remove(i);
            self.usage.remove(i);
        }
        let prefix = format!("{abs}/");
        let start = self.paths.partition_point(|(p, _)| p.as_str() < prefix.as_str());
        let mut end = start;
        while end < self.paths.len() && self.paths[end].0.starts_with(&prefix) {
            end += 1;
        }
        if end > start {
            self.paths.drain(start..end);
            self.usage.drain(start..end);
        }
    }

    pub fn apply_batch(&mut self, insert: Vec<(String, bool)>, remove: Vec<String>) {
        if self.usage.len() != self.paths.len() {
            self.usage.resize(self.paths.len(), 0.0);
        }
        if insert.len() + remove.len() <= 32 {
            for (abs, is_dir) in insert {
                self.insert_one(abs, is_dir);
            }
            for r in remove {
                self.remove_one(&r);
            }
            return;
        }

        let mut remove = remove;
        let mut insert = insert;
        remove.sort();
        remove.dedup();
        let rm = &remove;
        insert.retain(|(p, _)| !rm.iter().any(|r| p == r || p.starts_with(&format!("{r}/"))));
        let ranges = Self::removal_ranges(&self.paths, &remove);
        if !ranges.is_empty() {
            let mut kept = Vec::with_capacity(self.paths.len());
            let mut kept_usage = Vec::with_capacity(self.usage.len());
            let mut ri = 0;
            for (i, item) in self.paths.drain(..).enumerate() {
                while ri < ranges.len() && ranges[ri].1 <= i {
                    ri += 1;
                }
                let in_range = ri < ranges.len() && i >= ranges[ri].0;
                if !in_range {
                    kept_usage.push(self.usage[i]);
                    kept.push(item);
                }
            }
            self.usage = kept_usage;
            self.paths = kept;
        }

        if insert.is_empty() {
            return;
        }
        insert.sort();
        insert.dedup_by(|a, b| a.0 == b.0);

        let mut merged: Vec<(String, bool)> = Vec::with_capacity(self.paths.len() + insert.len());
        let mut merged_usage: Vec<f64> = Vec::with_capacity(self.usage.len() + insert.len());
        let mut ai = 0;
        let mut bi = 0;
        while ai < self.paths.len() || bi < insert.len() {
            let take_b = ai >= self.paths.len()
                || (bi < insert.len() && insert[bi].0.as_str() < self.paths[ai].0.as_str());
            let same = ai < self.paths.len() && bi < insert.len() && insert[bi].0 == self.paths[ai].0;
            if same {
                merged.push(insert[bi].clone());
                merged_usage.push(self.usage[ai]);
                ai += 1;
                bi += 1;
            } else if take_b {
                merged.push(insert[bi].clone());
                merged_usage.push(0.0);
                bi += 1;
            } else {
                merged.push(self.paths[ai].clone());
                merged_usage.push(self.usage[ai]);
                ai += 1;
            }
        }
        self.paths = merged;
        self.usage = merged_usage;
    }

    fn removal_ranges(paths: &[(String, bool)], remove: &[String]) -> Vec<(usize, usize)> {
        let mut ranges = Vec::new();
        for r in remove {
            if let Ok(i) = paths.binary_search_by(|(p, _)| p.as_str().cmp(r.as_str())) {
                ranges.push((i, i + 1));
            }
            let subtree_prefix = format!("{r}/");
            let start = paths.partition_point(|(p, _)| p.as_str() < subtree_prefix.as_str());
            let mut end = start;
            while end < paths.len() && paths[end].0.starts_with(&subtree_prefix) {
                end += 1;
            }
            if end > start {
                ranges.push((start, end));
            }
        }
        ranges.sort();
        let mut merged: Vec<(usize, usize)> = Vec::new();
        for (s, e) in ranges {
            match merged.last_mut() {
                Some(last) if s <= last.1 => last.1 = last.1.max(e),
                _ => merged.push((s, e)),
            }
        }
        merged
    }

    pub fn set_usage(&mut self, path: &str, score: f64) -> bool {
        match self.paths.binary_search_by(|(p, _)| p.as_str().cmp(path)) {
            Ok(i) => {
                if self.usage.len() != self.paths.len() {
                    self.usage.resize(self.paths.len(), 0.0);
                }
                self.usage[i] = score;
                true
            }
            Err(_) => false,
        }
    }

    pub fn refresh_usage(&mut self, model: &crate::ngram::Model, now: u64, version: u64) {
        if self.usage.len() != self.paths.len() {
            self.usage.resize(self.paths.len(), 0.0);
        }
        for p in model.path_freq.keys() {
            self.set_usage(p, model.path_score(p, now));
        }
        self.usage_version = version;
    }

    pub fn best_components(&self, abs_prefix: &str, dirs_only: bool, k: usize) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();

        if let Ok(i) = self.paths.binary_search_by(|(p, _)| p.as_str().cmp(abs_prefix))
            && self.paths[i].1
        {
            out.push("/".to_string());
            if k <= 1 {
                return out;
            }
        }

        let start = self.paths.partition_point(|(p, _)| p.as_str() < abs_prefix);
        let mut best: Vec<(String, String, f64, usize)> = Vec::new();
        let mut scanned = 0usize;
        for (j, (p, is_dir)) in self.paths[start..].iter().enumerate() {
            if !p.starts_with(abs_prefix) {
                break;
            }
            scanned += 1;
            if scanned > CAND_LIMIT {
                if !SCAN_LIMIT_WARNED.swap(true, Ordering::Relaxed) {
                    log(&format!("path prediction scan capped at {CAND_LIMIT} candidates - result may be partial"));
                }
                break;
            }
            if p == abs_prefix {
                continue;
            }
            if dirs_only && !is_dir {
                continue;
            }
            let suffix = &p[abs_prefix.len()..];
            let comp_end = suffix.find('/').unwrap_or(suffix.len());
            let name = &suffix[..comp_end];
            let display = if name.is_empty() {
                "/".to_string()
            } else if comp_end == suffix.len() && *is_dir {
                format!("{name}/")
            } else {
                name.to_string()
            };
            let score = self.usage.get(start + j).copied().unwrap_or(0.0);
            match best.iter().position(|(n, _, _, _)| n == name) {
                Some(i) => {
                    let e = &mut best[i];
                    let wins = score > e.2 || (score == e.2 && p.len() < e.3);
                    if wins {
                        e.1 = display;
                        e.2 = score;
                        e.3 = p.len();
                    } else if e.1 == *name && display.ends_with('/') {
                        e.1 = display;
                    }
                }
                None => best.push((name.to_string(), display, score, p.len())),
            }
        }

        best.sort_by(|a, b| b.2.partial_cmp(&a.2).unwrap().then_with(|| a.3.cmp(&b.3)));
        for (name, display, _, _) in best {
            if out.len() >= k {
                break;
            }
            if name.is_empty() && out.iter().any(|d| d == "/") {
                continue;
            }
            out.push(display);
        }
        out
    }

    pub fn predict(&self, abs_prefix: &str, dirs_only: bool) -> Option<String> {
        if let Ok(i) = self.paths.binary_search_by(|(p, _)| p.as_str().cmp(abs_prefix)) {
            if self.paths[i].1 {
                return Some("/".to_string());
            }
        }
        let start = self.paths.partition_point(|(p, _)| p.as_str() < abs_prefix);
        let mut best: Option<(f64, usize)> = None;
        let mut scanned = 0usize;
        for (j, (p, is_dir)) in self.paths[start..].iter().enumerate() {
            if !p.starts_with(abs_prefix) {
                break;
            }
            scanned += 1;
            if scanned > CAND_LIMIT {
                if !SCAN_LIMIT_WARNED.swap(true, Ordering::Relaxed) {
                    log(&format!("path prediction scan capped at {CAND_LIMIT} candidates - result may be partial"));
                }
                break;
            }
            if p == abs_prefix || (dirs_only && !is_dir) {
                continue;
            }
            let score = self.usage.get(start + j).copied().unwrap_or(0.0);
            let better = match best {
                None => true,
                Some((bs, bi)) => score > bs || (score == bs && p.len() < self.paths[bi].0.len()),
            };
            if better {
                best = Some((score, start + j));
            }
        }
        let idx = best.map(|(_, i)| i)?;
        let (p, is_dir) = &self.paths[idx];
        let suffix = &p[abs_prefix.len()..];
        let comp_end = suffix.find('/').unwrap_or(suffix.len());
        if suffix.is_empty() {
            None
        } else if comp_end == suffix.len() && *is_dir {
            Some(format!("{}/", &suffix[..comp_end]))
        } else {
            Some(suffix[..comp_end].to_string())
        }
    }

    pub fn save(&self, path: &Path) -> io::Result<()> {
        let mut body = Vec::new();
        body.write_all(&(self.paths.len() as u32).to_le_bytes())?;

        let mut prev = "";
        for (p, is_dir) in &self.paths {
            let common = common_prefix_len(prev, p);
            let suffix = &p[common..];
            if suffix.len() > u16::MAX as usize {
                return Err(io::Error::new(io::ErrorKind::InvalidData, "path suffix too long for fsindex.bin (u16 length prefix)"));
            }
            body.write_all(&(common as u16).to_le_bytes())?;
            body.write_all(&(suffix.len() as u16).to_le_bytes())?;
            body.write_all(suffix.as_bytes())?;
            body.write_all(&[*is_dir as u8])?;
            prev = p;
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
        fs::rename(tmp, path)
    }

    pub fn load(path: &Path) -> io::Result<FsIndex> {
        let mut raw = BufReader::new(File::open(path)?);
        let mut magic = [0u8; 4];
        raw.read_exact(&mut magic)?;
        if &magic != MAGIC {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "bad magic"));
        }
        let mut rest = Vec::new();
        raw.read_to_end(&mut rest)?;
        if rest.len() < 8 {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "fsindex.bin truncated"));
        }
        let split = rest.len() - 8;
        let stored = u64::from_le_bytes(rest[split..].try_into().unwrap());
        let body = &rest[..split];
        if fnv1a64(body) != stored {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "fsindex.bin checksum mismatch - file corrupted"));
        }
        let mut r = body;

        let count = read_u32(&mut r)?;
        let mut paths = Vec::with_capacity(count as usize);
        let mut prev = String::new();
        for _ in 0..count {
            let common = read_u16(&mut r)? as usize;
            let suffix_len = read_u16(&mut r)? as usize;
            let mut suffix = vec![0u8; suffix_len];
            r.read_exact(&mut suffix)?;
            let mut is_dir_buf = [0u8; 1];
            r.read_exact(&mut is_dir_buf)?;

            if common > prev.len() {
                return Err(io::Error::new(io::ErrorKind::InvalidData, "fsindex.bin bad front-coding offset"));
            }
            let mut s = String::with_capacity(common + suffix_len);
            s.push_str(&prev[..common]);
            s.push_str(&String::from_utf8_lossy(&suffix));
            paths.push((s.clone(), is_dir_buf[0] != 0));
            prev = s;
        }

        Ok(FsIndex { paths, usage: Vec::new(), usage_version: 0 })
    }
}

fn common_prefix_len(a: &str, b: &str) -> usize {
    a.bytes().zip(b.bytes()).take_while(|(x, y)| x == y).count()
}

fn read_u16<R: Read>(r: &mut R) -> io::Result<u16> {
    let mut b = [0u8; 2];
    r.read_exact(&mut b)?;
    Ok(u16::from_le_bytes(b))
}
fn read_u32<R: Read>(r: &mut R) -> io::Result<u32> {
    let mut b = [0u8; 4];
    r.read_exact(&mut b)?;
    Ok(u32::from_le_bytes(b))
}

pub fn watch(index: Arc<Mutex<FsIndex>>, root: PathBuf, exclude_names: Vec<String>, exclude_paths: Vec<String>) {
    let mut inotify = match Inotify::init() {
        Ok(i) => i,
        Err(e) => {
            log(&format!("inotify init failed, live index disabled: {e}"));
            return;
        }
    };

    let mask = WatchMask::CREATE | WatchMask::DELETE | WatchMask::MOVED_FROM | WatchMask::MOVED_TO;
    let mut wd_to_abs: HashMap<WatchDescriptor, String> = HashMap::new();
    let own_data_dir = data_dir().to_string_lossy().to_string();
    let added = std::cell::Cell::new(0usize);
    let failed = std::cell::Cell::new(0usize);

    let add_watch = |inotify: &mut Inotify, abs: &Path, wd_to_abs: &mut HashMap<WatchDescriptor, String>| {
        match inotify.watches().add(abs, mask) {
            Ok(wd) => {
                wd_to_abs.insert(wd, abs.to_string_lossy().to_string());
                added.set(added.get() + 1);
            }
            Err(_) => failed.set(failed.get() + 1),
        }
    };

    add_watch(&mut inotify, &root, &mut wd_to_abs);
    let root_str = root.to_string_lossy().to_string();
    {
        let idx = index.lock().unwrap();
        for (p, is_dir) in &idx.paths {
            if *is_dir && p.starts_with(&root_str) && !Path::new(p).is_symlink() {
                add_watch(&mut inotify, Path::new(p), &mut wd_to_abs);
            }
        }
    }
    if failed.get() > 0 {
        log(&format!(
            "inotify: {} watches added, {} failed under {root_str} - consider raising fs.inotify.max_user_watches",
            added.get(),
            failed.get()
        ));
    }

    let mut buffer = [0u8; 4096];
    loop {
        let events = match inotify.read_events_blocking(&mut buffer) {
            Ok(e) => e,
            Err(e) => {
                log(&format!("inotify read error, live index stopped: {e}"));
                return;
            }
        };

        let mut inserts: Vec<(String, bool)> = Vec::new();
        let mut removes: Vec<String> = Vec::new();
        for event in events {
            let Some(name) = event.name else { continue };
            let name = name.to_string_lossy().to_string();
            let Some(parent_abs) = wd_to_abs.get(&event.wd).cloned() else { continue };
            let abs = format!("{parent_abs}/{name}");
            let rel = abs.strip_prefix(&format!("{root_str}/")).unwrap_or(&abs).to_string();

            if abs == own_data_dir
                || abs.starts_with(&format!("{own_data_dir}/"))
                || exclude_names.iter().any(|n| n == &name)
                || exclude_paths.iter().any(|p| rel == *p || rel.starts_with(&format!("{p}/")))
            {
                continue;
            }

            if event.mask.contains(EventMask::CREATE) || event.mask.contains(EventMask::MOVED_TO) {
                let path = Path::new(&abs);
                let is_symlink = path.is_symlink();
                let is_dir = path.is_dir();
                inserts.push((abs.clone(), is_dir));
                if is_dir && !is_symlink {
                    add_watch(&mut inotify, path, &mut wd_to_abs);
                }
            } else if event.mask.contains(EventMask::DELETE) || event.mask.contains(EventMask::MOVED_FROM) {
                removes.push(abs);
            }
        }
        if !inserts.is_empty() || !removes.is_empty() {
            index.lock().unwrap().apply_batch(inserts, removes);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn idx(paths: &[(&str, bool)]) -> FsIndex {
        let mut paths: Vec<(String, bool)> = paths.iter().map(|(p, d)| (p.to_string(), *d)).collect();
        paths.sort();
        let n = paths.len();
        FsIndex { paths, usage: vec![0.0; n], usage_version: 0 }
    }

    #[test]
    fn save_load_round_trip() {
        let index = idx(&[("/home/u/a", false), ("/home/u/a/b", true), ("/home/u/abc", false), ("/home/u/z", true)]);
        let tmp = std::env::temp_dir().join(format!("bp-fsindex-test-{}.bin", std::process::id()));

        index.save(&tmp).unwrap();
        let loaded = FsIndex::load(&tmp).unwrap();

        assert_eq!(loaded.paths, index.paths);
        std::fs::remove_file(&tmp).ok();
    }

    #[test]
    fn load_rejects_corrupted_file() {
        let index = idx(&[("/home/u/a", false), ("/home/u/b", false)]);
        let tmp = std::env::temp_dir().join(format!("bp-fsindex-corrupt-{}.bin", std::process::id()));

        index.save(&tmp).unwrap();

        let mut bytes = std::fs::read(&tmp).unwrap();
        let last = bytes.len() - 9;
        bytes[last] ^= 0xFF;
        std::fs::write(&tmp, &bytes).unwrap();

        assert!(FsIndex::load(&tmp).is_err());
        std::fs::remove_file(&tmp).ok();
    }

    #[test]
    fn predict_ranks_by_usage_then_shortest() {
        let mut index = idx(&[("/h/proj-a", false), ("/h/proj-b", false), ("/h/proj-c", false)]);
        index.set_usage("/h/proj-b", 5.0);
        assert_eq!(index.predict("/h/proj", false), Some("-b".to_string()));
    }

    #[test]
    fn completes_one_component_with_dir_slash() {
        let index = idx(&[
            ("/h/dog", true),
            ("/h/dog/puppy.txt", false),
            ("/h/dolphin", false),
        ]);
        assert_eq!(index.predict("/h/do", false), Some("g/".to_string()));
        let index2 = idx(&[("/h/dog", true), ("/h/dolphin", false)]);
        assert_eq!(index2.predict("/h/dol", false), Some("phin".to_string()));
    }

    #[test]
    fn exact_directory_match_completes_into_itself() {
        let index = idx(&[("/h/proj", true), ("/h/proj-a", false), ("/h/proj/src", false)]);
        assert_eq!(index.predict("/h/proj", false), Some("/".to_string()));
    }

    #[test]
    fn dirs_only_skips_files() {
        let mut index = idx(&[("/h/x-file", false), ("/h/x-dir", true)]);
        assert_eq!(index.predict("/h/x", true), Some("-dir/".to_string()));
        index.set_usage("/h/x-file", 5.0);
        assert_eq!(index.predict("/h/x", false), Some("-file".to_string()));
    }

    #[test]
    fn usage_stays_aligned_through_batches() {
        let mut index = idx(&[("/h/a", false), ("/h/c", true)]);
        index.set_usage("/h/a", 3.0);
        index.apply_batch(vec![("/h/b".to_string(), false)], vec![]);
        assert_eq!(index.usage.len(), index.paths.len());
        let i = index.paths.binary_search_by(|(p, _)| p.as_str().cmp("/h/a")).unwrap();
        assert_eq!(index.usage[i], 3.0);
        let ins: Vec<(String, bool)> = (0..64).map(|n| (format!("/h/z{n:03}"), false)).collect();
        index.apply_batch(ins, vec!["/h/a".to_string()]);
        assert_eq!(index.usage.len(), index.paths.len());
        assert!(!index.contains("/h/a"));
        assert!(index.contains("/h/z000"));
    }

    #[test]
    fn apply_batch_merges_and_removes_subtrees() {
        let mut index = idx(&[("/h/a", false), ("/h/c", true)]);
        index.apply_batch(
            vec![("/h/b".to_string(), false), ("/h/b2".to_string(), true)],
            vec![],
        );
        let paths: Vec<&str> = index.paths.iter().map(|(p, _)| p.as_str()).collect();
        assert_eq!(paths, vec!["/h/a", "/h/b", "/h/b2", "/h/c"]);

        index.apply_batch(vec![("/h/b2/sub".to_string(), false)], vec!["/h/b2".to_string()]);
        let paths: Vec<&str> = index.paths.iter().map(|(p, _)| p.as_str()).collect();
        assert_eq!(paths, vec!["/h/a", "/h/b", "/h/c"]);
    }

    #[test]
    fn contains_and_remove() {
        let mut index = idx(&[("/h/a", false), ("/h/a/b", false), ("/h/c", false)]);
        assert!(index.contains("/h/a/b"));
        index.remove("/h/a");
        assert!(!index.contains("/h/a"));
        assert!(!index.contains("/h/a/b"));
        assert!(index.contains("/h/c"));
    }
}
