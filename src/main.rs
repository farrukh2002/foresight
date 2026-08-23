mod config;
mod fsindex;
mod ngram;
mod update;
mod util;
mod vocab;

use std::collections::HashMap;
use std::env;
use std::io::{self, BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;
#[cfg(feature = "bench")]
use std::time::Instant;

use config::Config;
use fsindex::FsIndex;
use ngram::{line_shape, Model};
use util::{data_dir, expand_home, is_path_shaped, last_shell_word, log, now_secs, wants_dirs};

const SESSION_GAP_SECS: u64 = 1800;

fn sock_path() -> PathBuf {
    data_dir().join("foresight.sock")
}
fn model_path() -> PathBuf {
    data_dir().join("model.bin")
}
fn fsindex_path() -> PathBuf {
    data_dir().join("fsindex.bin")
}
fn lock_path() -> PathBuf {
    data_dir().join("daemon.pid")
}
fn history_path() -> PathBuf {
    PathBuf::from(env::var("HOME").expect("HOME not set")).join(".bash_history")
}

fn current_dir_string() -> String {
    env::current_dir().map(|p| p.to_string_lossy().to_string()).unwrap_or_default()
}

struct AppState {
    model: Mutex<Model>,
    fsindex: Arc<Mutex<FsIndex>>,
    home: String,
    dirty: Mutex<bool>,
    sessions: Mutex<HashMap<String, SessionTail>>,
    predicts: AtomicU64,
    accepts: AtomicU64,
}

#[derive(Default, Clone, Copy)]
struct SessionTail {
    last_shape: u64,
    last_time: u64,
}

fn main() {
    let args: Vec<String> = env::args().collect();
    let cmd = args.get(1).map(|s| s.as_str()).unwrap_or("");
    let arg = args.get(2).map(|s| s.as_str()).unwrap_or("");
    match cmd {
        "serve" => serve(),
        "ensure-daemon" => ensure_daemon(),
        "predict" => print!("{}", strip_tag(&client_request("P", &current_dir_string(), arg).unwrap_or_default())),
        "train" => {
            client_request("T", &current_dir_string(), arg);
        }
        "accept" => {
            client_request("A", &current_dir_string(), arg);
        }
        "list" => {
            if let Some(r) = client_request("PL", &current_dir_string(), arg) {
                for item in r.split('\t') {
                    println!("{}", strip_tag(item));
                }
            }
        }
        "explain" => {
            if let Some(r) = client_request("E", &current_dir_string(), arg) {
                println!("{r}");
            }
        }
        "reindex" => {
            if let Some(r) = client_request("REINDEX", "", "") {
                println!("{r}");
            }
        }
        "stats" => {
            if let Some(r) = client_request("STATS", "", "") {
                println!("{r}");
            }
        }
        #[cfg(feature = "bench")]
        "bench" => {
            let n: u64 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(1_000_000);
            run_bench(n);
        }
        "version" => println!("foresight {}", update::CURRENT_VERSION),
        "update" => run_update_cmd(&args[2..]),
        _ => eprintln!(
            "usage: foresight <serve|ensure-daemon|predict LINE|train LINE|accept LINE|list LINE|explain LINE|reindex|stats|version|update [--check|--pin VERSION|--channel stable|beta|dev|--enable-silent|--disable-silent]>{}",
            if cfg!(feature = "bench") { "\n       foresight bench [N]  (bench feature build only)" } else { "" }
        ),
    }
}

fn run_update_cmd(rest: &[String]) {
    let home = env::var("HOME").expect("HOME not set");
    let mut check_only = false;
    let mut pin: Option<String> = None;
    let mut channel: Option<String> = None;
    let mut enable_silent = false;
    let mut disable_silent = false;
    let mut i = 0;
    while i < rest.len() {
        match rest[i].as_str() {
            "--check" => check_only = true,
            "--pin" => {
                i += 1;
                pin = rest.get(i).cloned();
            }
            "--channel" => {
                i += 1;
                channel = rest.get(i).cloned();
            }
            "--enable-silent" => enable_silent = true,
            "--disable-silent" => disable_silent = true,
            _ => {}
        }
        i += 1;
    }

    if let Some(c) = &channel
        && !update::valid_channel(c)
    {
        eprintln!("invalid channel '{c}', expected one of: {}", update::CHANNELS.join(", "));
        return;
    }

    if enable_silent {
        match update::set_update_mode(&home, "silent") {
            Ok(()) => println!("silent auto-update enabled"),
            Err(e) => eprintln!("failed to update config: {e}"),
        }
        return;
    }
    if disable_silent {
        match update::set_update_mode(&home, "manual") {
            Ok(()) => println!("silent auto-update disabled"),
            Err(e) => eprintln!("failed to update config: {e}"),
        }
        return;
    }
    if check_only {
        match update::check_only(channel.as_deref()) {
            Some(tag) => println!("update available: {} -> {tag}", update::CURRENT_VERSION),
            None => println!(
                "up to date ({}, channel: {})",
                update::CURRENT_VERSION,
                channel.as_deref().unwrap_or_else(|| update::current_channel())
            ),
        }
        return;
    }

    match update::apply_update(pin.as_deref(), channel.as_deref()) {
        Ok(Some(o)) => {
            println!("updated {} -> {}", o.from, o.to);
            if let Some(p) = &pin
                && let Err(e) = update::set_pinned_version(&home, p)
            {
                eprintln!("update installed but failed to persist pin: {e}");
            }
            client_request("SHUTDOWN", "", "");
        }
        Ok(None) => println!("already up to date ({})", update::CURRENT_VERSION),
        Err(e) => eprintln!("update failed: {e}"),
    }
}

fn strip_tag(reply: &str) -> &str {
    let b = reply.as_bytes();
    if b.len() >= 2 && b[1] == b':' && matches!(b[0], b'L' | b'T' | b'B' | b'W' | b'P') {
        &reply[2..]
    } else {
        reply
    }
}

#[cfg(unix)]
fn tighten_permissions() {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(data_dir(), std::fs::Permissions::from_mode(0o700));
}

fn acquire_singleton_lock() -> bool {
    let path = lock_path();
    loop {
        match std::fs::OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(mut f) => {
                let _ = f.write_all(std::process::id().to_string().as_bytes());
                return true;
            }
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                let alive = std::fs::read_to_string(&path)
                    .ok()
                    .and_then(|s| s.trim().parse::<u32>().ok())
                    .map(|pid| Path::new(&format!("/proc/{pid}")).exists())
                    .unwrap_or(false);
                if alive {
                    return false;
                }
                let _ = std::fs::remove_file(&path);
            }
            Err(_) => return false,
        }
    }
}

fn serve() {
    std::fs::create_dir_all(data_dir()).ok();
    tighten_permissions();
    if !acquire_singleton_lock() {
        log("another daemon instance already holds the lock - exiting");
        return;
    }

    let home = env::var("HOME").expect("HOME not set");
    let config = Config::load_or_create(&home);
    log(&format!("daemon starting, scan mode = {}", config.scan.mode));

    let mut model = Model::load(&model_path()).unwrap_or_else(|e| {
        log(&format!("model.bin unreadable ({e}), starting fresh"));
        Model::default()
    });
    if model.lines.is_empty() {
        let now = now_secs();
        model.bootstrap_from_history(&history_path(), &home, now);
        model.import_zsh_history(&home, now);
    } else {
        model.seed_transitions_from_history(&history_path(), now_secs());
    }
    let _ = model.save(&model_path());

    let fsindex = Arc::new(Mutex::new(FsIndex::load(&fsindex_path()).unwrap_or_else(|_| FsIndex::empty())));

    let state = Arc::new(AppState {
        model: Mutex::new(model),
        fsindex: fsindex.clone(),
        home: home.clone(),
        dirty: Mutex::new(false),
        sessions: Mutex::new(HashMap::new()),
        predicts: AtomicU64::new(0),
        accepts: AtomicU64::new(0),
    });

    {
        let fsindex = fsindex.clone();
        let config = config.clone();
        let home = home.clone();
        thread::spawn(move || {
            let fresh = FsIndex::build(&config, &home);
            log(&format!("fs index built: {} paths", fresh.paths.len()));
            let _ = fresh.save(&fsindex_path());
            *fsindex.lock().unwrap() = fresh;

            let (names, paths) = config.effective_excludes();
            for root in config.resolved_roots(&home) {
                let fsindex = fsindex.clone();
                let names = names.clone();
                let paths = paths.clone();
                thread::spawn(move || fsindex::watch(fsindex, root, names, paths));
            }
        });
    }

    {
        let fsindex = fsindex.clone();
        thread::spawn(move || loop {
            thread::sleep(Duration::from_secs(300));
            let snapshot = {
                let idx = fsindex.lock().unwrap();
                idx.paths.clone()
            };
            let _ = FsIndex { paths: snapshot, usage: Vec::new(), usage_version: 0 }.save(&fsindex_path());
        });
    }

    {
        let state = state.clone();
        thread::spawn(move || loop {
            thread::sleep(Duration::from_secs(2));
            let mut dirty = state.dirty.lock().unwrap();
            if *dirty {
                *dirty = false;
                drop(dirty);
                let _ = state.model.lock().unwrap().save(&model_path());
            }
        });
    }

    if config.update.mode == "silent" {
        let home = home.clone();
        thread::spawn(move || loop {
            let cfg = Config::load_or_create(&home);
            let pin = if cfg.update.pinned_version.is_empty() { None } else { Some(cfg.update.pinned_version) };
            match update::apply_update(pin.as_deref(), None) {
                Ok(Some(o)) => {
                    log(&format!("silent update applied: {} -> {}, restarting", o.from, o.to));
                    std::process::exit(0);
                }
                Ok(None) => {}
                Err(e) => log(&format!("silent update check failed: {e}")),
            }
            thread::sleep(Duration::from_secs(cfg.update.check_interval_hours.max(1) * 3600));
        });
    }

    if sock_path().exists() {
        std::fs::remove_file(sock_path()).ok();
    }
    let listener = UnixListener::bind(sock_path()).expect("failed to bind socket");
    log("daemon ready, socket bound");

    for conn in listener.incoming().flatten() {
        let state = state.clone();
        thread::spawn(move || handle_conn(conn, state));
    }
}

fn handle_conn(stream: UnixStream, state: Arc<AppState>) {
    let mut reader = BufReader::new(&stream);
    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) | Err(_) => return,
            Ok(_) => {}
        }
        if !handle_request(&line, &stream, &state) {
            return;
        }
    }
}

fn resolve_prev_shape(state: &AppState, field: &str, now: u64) -> Option<u64> {
    if let Some(hex) = field.strip_prefix('#') {
        return u64::from_str_radix(hex, 16).ok().filter(|&s| s != 0);
    }
    if field.is_empty() {
        return None;
    }
    let sessions = state.sessions.lock().unwrap();
    let tail = sessions.get(field)?;
    if now.saturating_sub(tail.last_time) > SESSION_GAP_SECS || tail.last_shape == 0 {
        None
    } else {
        Some(tail.last_shape)
    }
}

fn session_advance(state: &AppState, session: &str, shape: u64, now: u64) -> Option<u64> {
    if session.is_empty() || shape == 0 {
        return None;
    }
    let mut sessions = state.sessions.lock().unwrap();
    if sessions.len() > 64 {
        sessions.retain(|_, t| now.saturating_sub(t.last_time) < SESSION_GAP_SECS * 2);
    }
    let tail = sessions.entry(session.to_string()).or_default();
    let prev = if now.saturating_sub(tail.last_time) <= SESSION_GAP_SECS && tail.last_shape != 0 {
        Some(tail.last_shape)
    } else {
        None
    };
    tail.last_shape = shape;
    tail.last_time = now;
    prev
}

fn handle_request(line: &str, mut stream: &UnixStream, state: &Arc<AppState>) -> bool {
    let line = line.trim_end_matches('\n');
    let mut parts = line.splitn(4, '\t');
    let kind = parts.next().unwrap_or("");
    let cwd = parts.next().unwrap_or("");
    let session = parts.next().unwrap_or("");
    let payload = parts.next().unwrap_or("");

    let reply = match kind {
        "PING" => "PONG".to_string(),
        "SHUTDOWN" => {
            stream.write_all(b"OK\n").ok();
            log("daemon shutting down for update");
            std::process::exit(0);
        }
        "P" => {
            state.predicts.fetch_add(1, Ordering::Relaxed);
            let prev = resolve_prev_shape(state, session, now_secs());
            combined_predict(state, cwd, payload, prev)
        }
        "PH" => {
            state.predicts.fetch_add(1, Ordering::Relaxed);
            let mut f = payload.splitn(2, '\t');
            let idx: usize = f.next().and_then(|s| s.parse().ok()).unwrap_or(usize::MAX);
            let partial = f.next().unwrap_or("");
            let prev = resolve_prev_shape(state, session, now_secs());
            let (tag, suffix) = state.model.lock().unwrap().holdout_predict(idx, partial, cwd, now_secs(), prev);
            if tag == '\0' {
                String::new()
            } else {
                format!("{tag}:{suffix}")
            }
        }
        "T" => {
            let now = now_secs();
            let shape = line_shape(payload.trim());
            let prev = session_advance(state, session, shape, now);
            {
                let mut model = state.model.lock().unwrap();
                model.train(payload, &state.home, cwd, now);
                if let Some(prev) = prev {
                    model.record_transition(prev, shape, now);
                }
            }
            *state.dirty.lock().unwrap() = true;
            "OK".to_string()
        }
        "A" => {
            state.accepts.fetch_add(1, Ordering::Relaxed);
            log(&format!("ACCEPT\t{}", payload.replace('\t', " ")));
            state.model.lock().unwrap().touch_line(payload.trim(), now_secs());
            "OK".to_string()
        }
        "PL" => {
            let now = now_secs();
            let prev = resolve_prev_shape(state, session, now);
            let model = state.model.lock().unwrap();
            let mut items: Vec<String> = model
                .topk(payload, cwd, now, 5, prev)
                .into_iter()
                .map(|(tag, suffix)| format!("{tag}:{suffix}"))
                .collect();
            if !payload.is_empty() && !payload.ends_with(' ') {
                let last = last_shell_word(payload);
                if let Some(abs) = expand_home(&last, &state.home, Some(cwd)) {
                    let cmd = payload.split_whitespace().next().unwrap_or("");
                    let mut idx = state.fsindex.lock().unwrap();
                    if idx.usage_version != model.version {
                        idx.refresh_usage(&model, now, model.version);
                    }
                    for comp in idx.best_components(&abs, wants_dirs(cmd), 3) {
                        items.push(format!("P:{comp}"));
                    }
                }
            }
            items.join("\t")
        }
        "E" => {
            let now = now_secs();
            let prev = resolve_prev_shape(state, session, now);
            let model = state.model.lock().unwrap();
            let mut out = model.explain(payload, cwd, now, prev);
            if !payload.is_empty() && !payload.ends_with(' ') {
                let last = last_shell_word(payload);
                if let Some(abs) = expand_home(&last, &state.home, Some(cwd)) {
                    let cmd = payload.split_whitespace().next().unwrap_or("");
                    let mut idx = state.fsindex.lock().unwrap();
                    if idx.usage_version != model.version {
                        idx.refresh_usage(&model, now, model.version);
                    }
                    let comps = idx.best_components(&abs, wants_dirs(cmd), 3);
                    drop(idx);
                    if comps.is_empty() {
                        out.push_str("; P: none");
                    } else {
                        out.push_str(&format!("; P: {}", comps.iter().map(|c| format!("\"{c}\"")).collect::<Vec<_>>().join(" ")));
                    }
                }
            }
            out
        }
        "REINDEX" => {
            let config = Config::load_or_create(&state.home);
            let fresh = FsIndex::build(&config, &state.home);
            let _ = fresh.save(&fsindex_path());
            let n = fresh.paths.len();
            *state.fsindex.lock().unwrap() = fresh;
            log(&format!("reindex: {n} paths, mode={}", config.scan.mode));
            format!("OK paths={n} mode={}", config.scan.mode)
        }
        "STATS" => {
            let model = state.model.lock().unwrap();
            let idx = state.fsindex.lock().unwrap();
            let sessions = state.sessions.lock().unwrap();
            let bigram_edges: usize = model.bigram.values().map(|m| m.len()).sum();
            let trigram_edges: usize = model.trigram.values().map(|m| m.len()).sum();
            format!(
                "lines={} words={} bigram_edges={} trigram_edges={} cmd_transitions={} fs_paths={} sessions={} predicts={} accepts={}",
                model.lines.len(),
                model.vocab.strings.len(),
                bigram_edges,
                trigram_edges,
                model.shape_bigram.len(),
                idx.paths.len(),
                sessions.len(),
                state.predicts.load(Ordering::Relaxed),
                state.accepts.load(Ordering::Relaxed)
            )
        }
        _ => String::new(),
    };

    stream.write_all(format!("{reply}\n").as_bytes()).is_ok()
}

fn combined_predict(state: &AppState, cwd: &str, partial: &str, prev_shape: Option<u64>) -> String {
    let now = now_secs();
    if !partial.is_empty() && !partial.ends_with(' ') {
        let last = last_shell_word(partial);
        if let Some(abs) = expand_home(&last, &state.home, Some(cwd)) {
            let model = state.model.lock().unwrap();
            let mut idx = state.fsindex.lock().unwrap();
            if idx.usage_version != model.version {
                idx.refresh_usage(&model, now, model.version);
            }
            let cmd = partial.split_whitespace().next().unwrap_or("");
            let result = idx.predict(&abs, wants_dirs(cmd));
            drop(idx);
            drop(model);
            if let Some(suffix) = result {
                return format!("P:{suffix}");
            }
            let path_shaped = is_path_shaped(&last);
            let (tag, suffix) = state.model.lock().unwrap().predict_with(partial, cwd, now, !path_shaped, prev_shape);
            if tag == '\0' {
                return String::new();
            }
            return format!("{tag}:{suffix}");
        }
    }
    let (tag, suffix) = state.model.lock().unwrap().predict_with(partial, cwd, now, true, prev_shape);
    if tag == '\0' {
        String::new()
    } else {
        format!("{tag}:{suffix}")
    }
}

fn try_request(kind: &str, cwd: &str, payload: &str) -> Option<String> {
    let session = env::var("FORESIGHT_SESSION").unwrap_or_default();
    let mut stream = UnixStream::connect(sock_path()).ok()?;
    stream.set_read_timeout(Some(Duration::from_millis(500))).ok()?;
    stream.set_write_timeout(Some(Duration::from_millis(500))).ok()?;
    stream.write_all(format!("{kind}\t{cwd}\t{session}\t{payload}\n").as_bytes()).ok()?;
    let mut reader = BufReader::new(&stream);
    let mut line = String::new();
    reader.read_line(&mut line).ok()?;
    Some(line.trim_end_matches('\n').to_string())
}

fn client_request(kind: &str, cwd: &str, payload: &str) -> Option<String> {
    if let Some(r) = try_request(kind, cwd, payload) {
        return Some(r);
    }
    spawn_daemon_and_wait();
    try_request(kind, cwd, payload)
}

fn spawn_daemon_and_wait() {
    let exe = match env::current_exe() {
        Ok(e) => e,
        Err(_) => return,
    };
    let _ = Command::new(exe)
        .arg("serve")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn();

    for _ in 0..40 {
        thread::sleep(Duration::from_millis(50));
        if try_request("PING", "", "").as_deref() == Some("PONG") {
            return;
        }
    }
}

fn ensure_daemon() {
    if try_request("PING", "", "").as_deref() == Some("PONG") {
        return;
    }
    spawn_daemon_and_wait();
}

#[cfg(feature = "bench")]
struct Xorshift64(u64);
#[cfg(feature = "bench")]
impl Xorshift64 {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
    fn range(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

#[cfg(feature = "bench")]
fn snap_to_char_boundary(s: &str, mut i: usize) -> usize {
    while i < s.len() && !s.is_char_boundary(i) {
        i += 1;
    }
    i
}

#[cfg(feature = "bench")]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Category {
    Line,
    PathGeneral,
    PathUsed,
}

#[cfg(feature = "bench")]
impl Category {
    fn idx(self) -> usize {
        match self {
            Category::Line => 0,
            Category::PathGeneral => 1,
            Category::PathUsed => 2,
        }
    }
    fn label(self) -> &'static str {
        match self {
            Category::Line => "commands",
            Category::PathGeneral => "paths (never typed)",
            Category::PathUsed => "paths (previously typed)",
        }
    }
}

#[cfg(feature = "bench")]
#[derive(Default, Clone, Copy)]
struct Tally {
    total: u64,
    offered: u64,
    correct: u64,
    exact: u64,
}

#[cfg(feature = "bench")]
impl Tally {
    fn report(&self, label: &str) {
        if self.total == 0 {
            return;
        }
        println!(
            "  {:<26} n={:<8} coverage={:>5.1}%  accuracy={:>5.1}%  exact={:>5.1}%",
            label,
            self.total,
            self.offered as f64 / self.total as f64 * 100.0,
            self.correct as f64 / self.total as f64 * 100.0,
            self.exact as f64 / self.total as f64 * 100.0,
        );
    }
}

#[cfg(feature = "bench")]
struct Query {
    text: String,
    true_suffix: String,
    category: Category,
    line_idx: Option<usize>,
    prev_shape: Option<u64>,
}

#[cfg(feature = "bench")]
fn build_query_pool(model: &Model, fsindex: &FsIndex) -> Vec<Query> {
    let mut rng = Xorshift64(0x9e3779b97f4a7c15);
    let mut pool = Vec::new();

    let hist: Vec<String> = std::fs::read_to_string(history_path())
        .map(|t| t.lines().map(str::to_string).collect())
        .unwrap_or_default();
    let mut first_at: HashMap<&str, usize> = HashMap::new();
    for (i, h) in hist.iter().enumerate() {
        first_at.entry(h.as_str()).or_insert(i);
    }

    for (idx, entry) in model.lines.iter().enumerate() {
        if entry.text.len() < 2 {
            continue;
        }
        let cut = snap_to_char_boundary(&entry.text, 1 + rng.range(entry.text.len() - 1));
        let prev_shape = first_at
            .get(entry.text.as_str())
            .and_then(|&i| i.checked_sub(1))
            .map(|i| line_shape(&hist[i]));
        pool.push(Query {
            text: entry.text[..cut].to_string(),
            true_suffix: entry.text[cut..].to_string(),
            category: Category::Line,
            line_idx: Some(idx),
            prev_shape,
        });
    }

    let step = (fsindex.paths.len() / 1000).max(1);
    for (p, _) in fsindex.paths.iter().step_by(step) {
        if p.is_empty() || p.len() < 2 || p.contains(char::is_whitespace) {
            continue;
        }
        let cut = snap_to_char_boundary(p, 1 + rng.range(p.len() - 1));
        pool.push(Query {
            text: p[..cut].to_string(),
            true_suffix: p[cut..].to_string(),
            category: Category::PathGeneral,
            line_idx: None,
            prev_shape: None,
        });
    }

    for (path, _) in model.path_freq.iter() {
        if path.is_empty() || path.len() < 2 || path.contains(char::is_whitespace) || !fsindex.contains(path) {
            continue;
        }
        let cut = snap_to_char_boundary(path, 1 + rng.range(path.len() - 1));
        pool.push(Query {
            text: path[..cut].to_string(),
            true_suffix: path[cut..].to_string(),
            category: Category::PathUsed,
            line_idx: None,
            prev_shape: None,
        });
    }

    if pool.is_empty() {
        pool.push(Query { text: String::new(), true_suffix: String::new(), category: Category::Line, line_idx: None, prev_shape: None });
    }
    pool
}

#[cfg(feature = "bench")]
fn zsh_predict(text: &str, lines: &[&str]) -> Option<String> {
    if text.is_empty() {
        return None;
    }
    lines.iter().rev().find(|line| **line != text && line.starts_with(text)).map(|line| line[text.len()..].to_string())
}

#[cfg(feature = "bench")]
fn percentile_ns(sorted: &[u128], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let idx = (((p / 100.0) * (sorted.len() - 1) as f64).round() as usize).min(sorted.len() - 1);
    sorted[idx] as f64
}

#[cfg(feature = "bench")]
fn parse_reply(reply: &str) -> (char, &str) {
    let b = reply.as_bytes();
    if b.len() >= 2 && b[1] == b':' && matches!(b[0], b'L' | b'T' | b'B' | b'W' | b'P') {
        (b[0] as char, &reply[2..])
    } else {
        ('\0', reply)
    }
}

#[cfg(feature = "bench")]
fn run_bench(n: u64) {
    let model = Model::load(&model_path()).unwrap_or_default();
    let fsindex = FsIndex::load(&fsindex_path()).unwrap_or_else(|_| FsIndex::empty());
    let pool = build_query_pool(&model, &fsindex);
    eprintln!(
        "query pool: {} samples (from {} history lines, {} indexed paths)",
        pool.len(),
        model.lines.len(),
        fsindex.paths.len()
    );

    let mut stream = match UnixStream::connect(sock_path()) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("cannot connect to daemon - is it running? try: foresight ensure-daemon ({e})");
            return;
        }
    };
    let read_half = match stream.try_clone() {
        Ok(s) => s,
        Err(e) => {
            eprintln!("cannot clone socket: {e}");
            return;
        }
    };
    let mut reader = BufReader::new(read_half);

    let mut overall = Tally::default();
    let mut by_cat = [Tally::default(); 3];
    let mut holdout = Tally::default();
    let mut seq_off = Tally::default();
    let mut by_strategy: HashMap<char, Tally> = HashMap::new();

    let all_lines: Vec<&str> = model.lines.iter().map(|e| e.text.as_str()).collect();
    let mut zsh_overall = Tally::default();
    let mut zsh_by_cat = [Tally::default(); 3];

    let sample_every = (n / 2000).max(1);
    let mut sample_latencies_ns: Vec<u128> = Vec::with_capacity(2000);
    let mut line_buf = String::new();
    let mut holdout_buf = String::new();
    let mut abl_buf = String::new();
    let mut requests_sent: u64 = 0;

    let t_start = Instant::now();
    for i in 0..n {
        let query = &pool[(i as usize) % pool.len()];
        let timed = i % sample_every == 0;
        let req_start = if timed { Some(Instant::now()) } else { None };

        let cwd = query.line_idx.and_then(|idx| model.top_cwd(idx)).unwrap_or("");
        let seq_field = query.prev_shape.map_or(String::new(), |p| format!("#{p:x}"));
        if stream.write_all(format!("P\t{cwd}\t{seq_field}\t{}\n", query.text).as_bytes()).is_err() {
            eprintln!("write failed at request {i} - daemon may have died");
            break;
        }
        requests_sent += 1;
        line_buf.clear();
        if reader.read_line(&mut line_buf).is_err() {
            eprintln!("read failed at request {i} - daemon may have died");
            break;
        }
        if let Some(t0) = req_start {
            sample_latencies_ns.push(t0.elapsed().as_nanos());
        }
        let (tag, suggestion) = parse_reply(line_buf.trim_end_matches('\n'));
        if tag == 'W' && std::env::var_os("FORESIGHT_DEBUG_W").is_some() {
            eprintln!("W-debug: query={:?} truth={:?}", query.text, query.true_suffix);
        }
        let cat = &mut by_cat[query.category.idx()];
        overall.total += 1;
        cat.total += 1;
        if !suggestion.is_empty() {
            overall.offered += 1;
            cat.offered += 1;
            if query.true_suffix.starts_with(suggestion) {
                overall.correct += 1;
                cat.correct += 1;
                if suggestion == query.true_suffix {
                    overall.exact += 1;
                    cat.exact += 1;
                }
            }
        }
        if tag != '\0' {
            let s = by_strategy.entry(tag).or_default();
            s.total += 1;
            if !suggestion.is_empty() {
                s.offered += 1;
                if query.true_suffix.starts_with(suggestion) {
                    s.correct += 1;
                    if suggestion == query.true_suffix {
                        s.exact += 1;
                    }
                }
            }
        }

        if let Some(idx) = query.line_idx {
            if stream.write_all(format!("PH\t{cwd}\t{seq_field}\t{idx}\t{}\n", query.text).as_bytes()).is_err() {
                eprintln!("holdout write failed at request {i}");
                break;
            }
            requests_sent += 1;
            holdout_buf.clear();
            if reader.read_line(&mut holdout_buf).is_err() {
                eprintln!("holdout read failed at request {i}");
                break;
            }
            let (_, h_suggestion) = parse_reply(holdout_buf.trim_end_matches('\n'));
            holdout.total += 1;
            if !h_suggestion.is_empty() {
                holdout.offered += 1;
                if query.true_suffix.starts_with(h_suggestion) {
                    holdout.correct += 1;
                    if h_suggestion == query.true_suffix {
                        holdout.exact += 1;
                    }
                }
            }

            if stream.write_all(format!("P\t{cwd}\t\t{}\n", query.text).as_bytes()).is_err() {
                eprintln!("ablation write failed at request {i}");
                break;
            }
            requests_sent += 1;
            abl_buf.clear();
            if reader.read_line(&mut abl_buf).is_err() {
                eprintln!("ablation read failed at request {i}");
                break;
            }
            let (_, off_suggestion) = parse_reply(abl_buf.trim_end_matches('\n'));
            seq_off.total += 1;
            if !off_suggestion.is_empty() {
                seq_off.offered += 1;
                if query.true_suffix.starts_with(off_suggestion) {
                    seq_off.correct += 1;
                    if off_suggestion == query.true_suffix {
                        seq_off.exact += 1;
                    }
                }
            }
        }

        let zsh_lines: &[&str] = match query.line_idx {
            Some(idx) => &all_lines[..idx],
            None => &all_lines[..],
        };
        let zsh_suggestion = zsh_predict(&query.text, zsh_lines);
        let zcat = &mut zsh_by_cat[query.category.idx()];
        zsh_overall.total += 1;
        zcat.total += 1;
        if let Some(s) = &zsh_suggestion {
            zsh_overall.offered += 1;
            zcat.offered += 1;
            if query.true_suffix.starts_with(s.as_str()) {
                zsh_overall.correct += 1;
                zcat.correct += 1;
                if *s == query.true_suffix {
                    zsh_overall.exact += 1;
                    zcat.exact += 1;
                }
            }
        }

        if i > 0 && i % 100_000 == 0 {
            eprintln!("  {i}/{n}...");
        }
    }
    let elapsed = t_start.elapsed();

    sample_latencies_ns.sort_unstable();
    let p50 = percentile_ns(&sample_latencies_ns, 50.0);
    let p99 = percentile_ns(&sample_latencies_ns, 99.0);

    println!();
    println!("iterations:  {n}");
    println!("requests:    {requests_sent} (line queries also run holdout + no-seq ablation)");
    println!("elapsed:     {:.2}s", elapsed.as_secs_f64());
    println!("throughput:  {:.0} req/s", requests_sent as f64 / elapsed.as_secs_f64());
    println!("avg latency: {:.2} us", elapsed.as_secs_f64() * 1_000_000.0 / requests_sent as f64);
    println!(
        "p50 latency: {:.2} us (sampled, n={})",
        p50 / 1000.0,
        sample_latencies_ns.len()
    );
    println!("p99 latency: {:.2} us (sampled)", p99 / 1000.0);

    if let Ok(exe) = env::current_exe() {
        let mut e2e_total = Duration::ZERO;
        let runs = 5u32;
        for _ in 0..runs {
            let t0 = Instant::now();
            let _ = Command::new(&exe).arg("predict").arg("git st").stdout(Stdio::null()).stderr(Stdio::null()).status();
            e2e_total += t0.elapsed();
        }
        println!("client e2e:  {:.2} ms avg (fork+exec+socket, {runs} runs)", e2e_total.as_secs_f64() * 1000.0 / runs as f64);
    }

    println!();
    println!(
        "coverage:    {:.1}% ({}/{n} got any suggestion)",
        overall.offered as f64 / n as f64 * 100.0,
        overall.offered
    );
    println!(
        "accuracy:    {:.1}% ({}/{n} suggestions matched what actually came next)",
        overall.correct as f64 / n as f64 * 100.0,
        overall.correct
    );
    println!(
        "precision:   {:.1}% ({}/{} of offered suggestions were correct)",
        if overall.offered > 0 { overall.correct as f64 / overall.offered as f64 * 100.0 } else { 0.0 },
        overall.correct,
        overall.offered
    );
    println!(
        "exact match: {:.1}% ({}/{n} completed the rest of the text exactly)",
        overall.exact as f64 / n as f64 * 100.0,
        overall.exact
    );
    println!();
    println!("by category:");
    for cat in [Category::Line, Category::PathGeneral, Category::PathUsed] {
        by_cat[cat.idx()].report(cat.label());
    }
    holdout.report("commands (holdout)");
    if seq_off.total > 0 {
        println!();
        println!("sequence bonus (same command queries, prev-command context):");
        by_cat[Category::Line.idx()].report("  with ctx (live behavior)");
        seq_off.report("  without ctx (ablation)");
    }

    println!();
    println!("by strategy (which predictor produced the reply):");
    for (tag, label) in [
        ('L', "line continuation"),
        ('T', "trigram"),
        ('B', "bigram"),
        ('W', "word prefix"),
        ('P', "path component"),
    ] {
        if let Some(s) = by_strategy.get(&tag) {
            s.report(label);
        }
    }

    println!();
    println!("--- zsh-autosuggestions baseline (same query pool, same ground truth) ---");
    println!(
        "coverage:    {:.1}% ({}/{n})",
        zsh_overall.offered as f64 / n as f64 * 100.0,
        zsh_overall.offered
    );
    println!(
        "accuracy:    {:.1}% ({}/{n})",
        zsh_overall.correct as f64 / n as f64 * 100.0,
        zsh_overall.correct
    );
    println!(
        "precision:   {:.1}% ({}/{})",
        if zsh_overall.offered > 0 { zsh_overall.correct as f64 / zsh_overall.offered as f64 * 100.0 } else { 0.0 },
        zsh_overall.correct,
        zsh_overall.offered
    );
    println!(
        "exact match: {:.1}% ({}/{n})",
        zsh_overall.exact as f64 / n as f64 * 100.0,
        zsh_overall.exact
    );
    println!("by category:");
    for cat in [Category::Line, Category::PathGeneral, Category::PathUsed] {
        zsh_by_cat[cat.idx()].report(cat.label());
    }
}
