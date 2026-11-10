mod config;
mod fsindex;
mod integrations;
mod ngram;
mod protocol;
mod update;
mod util;
mod vocab;

use std::collections::HashMap;
use std::env;
use std::io::{self, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Command, ExitCode, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;
use parking_lot::{Condvar, Mutex};

use config::Config;
use fsindex::{FsIndex, Watcher};
use ngram::{is_agent_noise, line_shape, Model};
use protocol::{Reply, Request, Response};
use util::{data_dir, escape_path_suffix, expand_home, is_path_shaped, last_shell_word, log, now_millis, now_secs, wants_dirs};

const SESSION_GAP_SECS: u64 = 1800;
const SESSION_WORD_TTL_SECS: u64 = 600;
const SESSION_RECENT_LINES: usize = 4;

fn sock_path() -> PathBuf {
    env::var_os("FORESIGHT_SOCK").map(PathBuf::from).unwrap_or_else(|| data_dir().join("foresight.sock"))
}
fn model_path() -> PathBuf { data_dir().join("model.bin") }
fn fsindex_path() -> PathBuf { data_dir().join("fsindex.bin") }
fn lock_path() -> PathBuf { data_dir().join("daemon.pid") }
fn history_path() -> PathBuf { PathBuf::from(env::var_os("HOME").unwrap_or_default()).join(".bash_history") }
fn current_dir_string() -> Result<String, String> {
    env::current_dir().map(|p| p.to_string_lossy().into_owned()).map_err(|e| e.to_string())
}

struct AppState {
    model: Mutex<Model>,
    fsindex: Arc<Mutex<FsIndex>>,
    watcher: Mutex<Option<Watcher>>,
    home: String,
    dirty: AtomicBool,
    // Admission and persistence share one lock: no training can be acknowledged
    // between the shutdown flush and closing admission.
    mutations: Mutex<()>,
    persist: Mutex<()>,
    flushing: [AtomicBool; 2],
    stopping: AtomicBool,
    exit_ready: AtomicBool,
    reload: Mutex<u64>,
    wake: Condvar,
    sessions: Mutex<HashMap<String, SessionTail>>,
    predicts: AtomicU64,
    accepts: AtomicU64,
    train_burst_ms: u64,
}

#[derive(Default, Clone)]
struct SessionTail { last_shape: u64, last_time: u64, recent: Vec<(u64, String)>, last_train_ms: u64, burst_until_ms: u64 }

fn session_word_ids(model: &Model, recent: &[String]) -> Option<std::collections::HashSet<u32>> {
    if recent.is_empty() { return None; }
    let ids: std::collections::HashSet<u32> = recent.iter()
        .flat_map(|line| line.split_whitespace().skip(1))
        .filter(|w| w.len() >= ngram::SESSION_MIN_WORD_LEN && !w.starts_with('-'))
        .filter_map(|w| model.vocab.get(w))
        .collect();
    (!ids.is_empty()).then_some(ids)
}

fn main() -> ExitCode {
    match run(env::args().skip(1).collect()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => { eprintln!("foresight: {e}"); ExitCode::FAILURE }
    }
}

fn one_arg<'a>(args: &'a [String], usage: &str) -> Result<&'a str, String> {
    if args.len() != 1 { return Err(format!("usage: foresight {usage}")); }
    Ok(&args[0])
}
fn no_args(args: &[String]) -> Result<(), String> {
    if args.is_empty() { Ok(()) } else { Err("unexpected arguments".into()) }
}
fn text_reply(reply: Reply) -> Result<String, String> {
    match reply { Reply::Text(text) => Ok(text), Reply::List(_) => Err("unexpected list reply".into()) }
}

fn run(args: Vec<String>) -> Result<(), String> {
    let Some((cmd, rest)) = args.split_first() else {
        return Err("usage: foresight <serve|ensure-daemon|init|predict|train|accept|list|explain|reindex|stats|prune|version|update|stream>".into());
    };
    match cmd.as_str() {
        "serve" => { no_args(rest)?; serve() }
        "ensure-daemon" => { no_args(rest)?; ensure_daemon() }
        "predict" | "train" | "accept" | "explain" => {
            let payload = one_arg(rest, &format!("{cmd} LINE"))?;
            let kind = match cmd.as_str() { "predict" => "P", "train" => "T", "accept" => "A", _ => "E" };
            let reply = text_reply(client_request(kind, &current_dir_string()?, payload)?)?;
            if cmd == "predict" { print!("{}", strip_tag(&reply)); }
            if cmd == "explain" { println!("{reply}"); }
            Ok(())
        }
        "list" => {
            let null = rest.first().is_some_and(|s| s == "--null");
            let payload = one_arg(if null { &rest[1..] } else { rest }, "list [--null] LINE")?;
            let Reply::List(items) = client_request("PL", &current_dir_string()?, payload)? else {
                return Err("unexpected non-list reply".into());
            };
            let mut out = io::stdout().lock();
            for item in items {
                out.write_all(strip_tag(&item).as_bytes()).map_err(|e| e.to_string())?;
                out.write_all(if null { b"\0" } else { b"\n" }).map_err(|e| e.to_string())?;
            }
            out.flush().map_err(|e| e.to_string())
        }
        "prune" => {
            let apply = match rest { [] => false, [flag] if flag == "--apply" => true, _ => return Err("usage: foresight prune [--apply]".into()) };
            println!("{}", text_reply(client_request("PRUNE", "", if apply { "apply" } else { "dry" })?)?);
            Ok(())
        }
        "reindex" | "stats" => {
            no_args(rest)?;
            println!("{}", text_reply(client_request(if cmd == "reindex" { "REINDEX" } else { "STATS" }, "", "")?)?);
            Ok(())
        }
        "version" => { no_args(rest)?; println!("foresight {}", update::CURRENT_VERSION); Ok(()) }
        "init" => integrations::run_init(rest),
        "update" => run_update_cmd(rest),
        "stream" => { no_args(rest)?; run_stream() }
        #[cfg(feature = "bench")]
        "bench" => {
            let n = if rest.is_empty() { 1_000_000 } else {
                one_arg(rest, "bench [N]")?.parse::<u64>().map_err(|_| "N must be a positive integer")?
            };
            if n == 0 { return Err("N must be a positive integer".into()); }
            run_bench(n)
        }
        _ => Err(format!("unknown command: {cmd}")),
    }
}

fn run_update_cmd(rest: &[String]) -> Result<(), String> {
    let home = env::var("HOME").map_err(|e| e.to_string())?;
    let mut check = false;
    let mut pin = None;
    let mut channel = None;
    let mut mode = None;
    let mut args = rest.iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--check" if !check => check = true,
            "--pin" if pin.is_none() => pin = Some(args.next().filter(|v| !v.starts_with('-')).ok_or("--pin requires VERSION")?.as_str()),
            "--channel" if channel.is_none() => channel = Some(args.next().filter(|v| !v.starts_with('-')).ok_or("--channel requires a channel")?.as_str()),
            "--enable-silent" if mode.is_none() => mode = Some("silent"),
            "--disable-silent" if mode.is_none() => mode = Some("manual"),
            _ => return Err(format!("unknown, duplicate, or conflicting option: {arg}")),
        }
    }
    if pin.is_some() && (channel.is_some() || check) || mode.is_some() && (pin.is_some() || channel.is_some() || check) {
        return Err("conflicting update options".into());
    }
    if let Some(channel) = channel {
        if !update::valid_channel(channel) { return Err(format!("invalid channel: {channel}")); }
    }
    if let Some(mode) = mode {
        update::set_update_mode(&home, mode).map_err(|e| e.to_string())?;
        notify_running("RELOAD")?;
        println!("silent auto-update {}", if mode == "silent" { "enabled" } else { "disabled" });
    } else if check {
        match update::check_only(channel)? {
            Some(tag) => println!("update available: {} -> {tag}", update::CURRENT_VERSION),
            None => println!("up to date ({})", update::CURRENT_VERSION),
        }
    } else {
        match update::apply_update(pin, channel)? {
            Some(outcome) => {
                println!("updated {} -> {}", outcome.from, outcome.to);
                notify_running("SHUTDOWN")?;
            }
            None => println!("already up to date ({})", update::CURRENT_VERSION),
        }
    }
    Ok(())
}

fn strip_tag(reply: &str) -> &str {
    let b = reply.as_bytes();
    if b.len() >= 2 && b[1] == b':' && matches!(b[0], b'L' | b'T' | b'B' | b'W' | b'F' | b'P') { &reply[2..] } else { reply }
}

fn acquire_singleton_lock() -> io::Result<Option<std::fs::File>> {
    let mut file = std::fs::OpenOptions::new().read(true).write(true).create(true).truncate(false).open(lock_path())?;
    match file.try_lock() {
        Ok(()) => {
            file.set_len(0)?;
            write!(file, "{}", std::process::id())?;
            Ok(Some(file))
        }
        Err(std::fs::TryLockError::WouldBlock) => Ok(None),
        Err(std::fs::TryLockError::Error(e)) => Err(e),
    }
}

fn flush_model(state: &AppState) -> Result<(), String> {
    let _persist = state.persist.lock();
    let bytes = {
        let model = state.model.lock();
        if !state.dirty.swap(false, Ordering::AcqRel) { return Ok(()); }
        model.encode()
    };
    bytes.and_then(|bytes| util::write_snapshot(&model_path(), &bytes)).map_err(|e| {
        state.dirty.store(true, Ordering::Release);
        let message = format!("model persistence failed: {e}");
        log(&message);
        message
    })
}
fn flush_index(state: &AppState) -> Result<(), String> {
    let _persist = state.persist.lock();
    let bytes = state.fsindex.lock().encode();
    bytes.and_then(|bytes| util::write_snapshot(&fsindex_path(), &bytes)).map_err(|e| {
        let message = format!("filesystem index persistence failed: {e}");
        log(&message);
        message
    })
}
fn flush_in_background(state: &Arc<AppState>, index: bool) {
    if state.flushing[usize::from(index)].swap(true, Ordering::AcqRel) { return; }
    let state = state.clone();
    thread::spawn(move || {
        let _ = if index { flush_index(&state) } else { flush_model(&state) };
        state.flushing[usize::from(index)].store(false, Ordering::Release);
    });
}
fn shutdown(state: &AppState) -> Result<(), String> {
    let _guard = state.mutations.lock();
    flush_model(state)?;
    flush_index(state)?;
    state.stopping.store(true, Ordering::Release);
    state.wake.notify_all();
    Ok(())
}
fn reindex(state: &AppState) -> Result<usize, String> {
    let mut owner = state.watcher.lock();
    drop(owner.take());
    let config = Config::load_or_create(&state.home);
    *owner = Some(Watcher::start(state.fsindex.clone(), config, &state.home).map_err(|e| e.to_string())?);
    flush_index(state)?;
    Ok(state.fsindex.lock().paths.len())
}

fn serve() -> Result<(), String> {
    std::fs::create_dir_all(data_dir()).map_err(|e| e.to_string())?;
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(data_dir(), std::fs::Permissions::from_mode(0o700)).map_err(|e| e.to_string())?;
    let Some(_singleton) = acquire_singleton_lock().map_err(|e| e.to_string())? else { return Ok(()); };
    util::remove_stale_temp_files(&data_dir());
    let home = env::var("HOME").map_err(|e| e.to_string())?;
    let mut model = Model::load(&model_path()).unwrap_or_else(|e| {
        log(&format!("model.bin unreadable ({e}), starting fresh"));
        Model::default()
    });
    if model.lines.is_empty() {
        model.bootstrap_from_history(&history_path(), &home, now_secs());
        model.import_zsh_history(&home, now_secs());
    } else {
        model.seed_transitions_from_history(&history_path(), now_secs());
    }
    let train_burst_ms = Config::load_or_create(&home).train.burst_ms;
    let state = Arc::new(AppState {
        model: Mutex::new(model),
        fsindex: Arc::new(Mutex::new(FsIndex::load(&fsindex_path()).unwrap_or_else(|_| FsIndex::empty()))),
        watcher: Mutex::new(None), home, dirty: AtomicBool::new(true), mutations: Mutex::new(()), persist: Mutex::new(()), flushing: [AtomicBool::new(false), AtomicBool::new(false)],
        stopping: AtomicBool::new(false), exit_ready: AtomicBool::new(false),
        reload: Mutex::new(0), wake: Condvar::new(), sessions: Mutex::new(HashMap::new()),
        predicts: AtomicU64::new(0), accepts: AtomicU64::new(0),
        train_burst_ms,
    });
    { let _guard = state.mutations.lock(); let _ = flush_model(&state); }
    // Bind before the potentially long initial scan. The singleton lock is held
    // for the entire lifetime; the lock file is never unlinked.
    match std::fs::remove_file(sock_path()) {
        Ok(()) => {}, Err(e) if e.kind() == io::ErrorKind::NotFound => {}, Err(e) => return Err(e.to_string()),
    }
    let listener = UnixListener::bind(sock_path()).map_err(|e| e.to_string())?;
    listener.set_nonblocking(true).map_err(|e| e.to_string())?;
    let scan_state = state.clone();
    let initial_scan = thread::spawn(move || {
        if let Err(e) = reindex(&scan_state) { log(&format!("initial reindex failed: {e}")); }
    });
    let scheduler_state = state.clone();
    let scheduler = thread::spawn(move || update_scheduler(scheduler_state));
    log("daemon ready, socket bound");
    let mut last_flush = std::time::Instant::now();
    let mut last_index_flush = std::time::Instant::now();
    while !state.exit_ready.load(Ordering::Acquire) {
        match listener.accept() {
            Ok((stream, _)) => {
                let state = state.clone();
                thread::spawn(move || handle_conn(stream, state));
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => thread::sleep(Duration::from_millis(10)),
            Err(e) => { log(&format!("socket accept failed: {e}")); thread::sleep(Duration::from_millis(100)); }
        }
        if last_flush.elapsed() >= Duration::from_secs(2) {
            flush_in_background(&state, false);
            last_flush = std::time::Instant::now();
        }
        if last_index_flush.elapsed() >= Duration::from_secs(300) {
            flush_in_background(&state, true);
            last_index_flush = std::time::Instant::now();
        }
    }
    state.wake.notify_all();
    let _ = initial_scan.join();
    drop(state.watcher.lock().take());
    let _ = scheduler.join();
    { let _guard = state.mutations.lock(); flush_model(&state)?; flush_index(&state)?; }
    std::fs::remove_file(sock_path()).map_err(|e| e.to_string())?;
    Ok(())
}

fn update_scheduler(state: Arc<AppState>) {
    let mut restart_pending = false;
    loop {
        let generation = *state.reload.lock();
        if state.stopping.load(Ordering::Acquire) { return; }
        let config = Config::load_or_create(&state.home);
        if config.update.mode == "silent" && !restart_pending {
            // apply_update resolves policy afresh, including stored channel/pin.
            match update::apply_update(None, None) {
                Ok(Some(outcome)) => {
                    log(&format!("silent update applied: {} -> {}", outcome.from, outcome.to));
                    restart_pending = true;
                }
                Ok(None) => {},
                Err(e) => log(&format!("silent update failed: {e}")),
            }
        }
        if restart_pending {
            match shutdown(&state) {
                Ok(()) => { state.exit_ready.store(true, Ordering::Release); return; }
                Err(e) => log(&format!("update restart postponed: {e}")),
            }
        }
        let mut current = state.reload.lock();
        if state.stopping.load(Ordering::Acquire) { return; }
        if *current != generation { continue; }
        let interval = Duration::from_secs(if restart_pending { 2 } else { config.update.check_interval_hours.max(1).saturating_mul(3600) });
        state.wake.wait_for(&mut current, interval);
        // A timeout or RELOAD always re-reads settings before the next check.
    }
}

fn handle_conn(stream: UnixStream, state: Arc<AppState>) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(30)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(30)));
    let mut reader = BufReader::new(&stream);
    loop {
        let request: Request = match protocol::read_json(&mut reader) {
            Ok(request) => request,
            Err(e) => {
                if e.kind() != io::ErrorKind::UnexpectedEof {
                    let response: Response = Err(format!("invalid request: {e}"));
                    let _ = protocol::write_json(&stream, &response);
                }
                return;
            }
        };
        let response = handle_request(&request, &state);
        let shutdown = request.kind == "SHUTDOWN" && response.is_ok();
        let written = protocol::write_json(&stream, &response);
        if shutdown {
            state.exit_ready.store(true, Ordering::Release);
            return;
        }
        if written.is_err() || state.stopping.load(Ordering::Acquire) { return; }
    }
}

fn resolve_prev_shape(state: &AppState, session: &str, now: u64) -> Option<u64> {
    let sessions = state.sessions.lock();
    let tail = sessions.get(session)?;
    if now.saturating_sub(tail.last_time) > SESSION_GAP_SECS || tail.last_shape == 0 { None } else { Some(tail.last_shape) }
}
fn session_recent(state: &AppState, session: &str, now: u64) -> Vec<String> {
    if session.is_empty() { return Vec::new(); }
    let sessions = state.sessions.lock();
    sessions.get(session)
        .filter(|t| now.saturating_sub(t.last_time) <= SESSION_GAP_SECS)
        .map(|t| t.recent.iter().filter(|(at, _)| now.saturating_sub(*at) <= SESSION_WORD_TTL_SECS).map(|(_, line)| line.clone()).collect())
        .unwrap_or_default()
}
fn scripted_input(state: &AppState, session: &str, now_ms: u64) -> bool {
    if state.train_burst_ms == 0 || session.is_empty() { return false; }
    let mut sessions = state.sessions.lock();
    let tail = sessions.entry(session.to_owned()).or_default();
    let scripted = now_ms < tail.burst_until_ms || now_ms.saturating_sub(tail.last_train_ms) < state.train_burst_ms;
    tail.last_train_ms = now_ms;
    if scripted { tail.burst_until_ms = now_ms + state.train_burst_ms * 4; }
    scripted
}
fn session_advance(state: &AppState, session: &str, shape: u64, now: u64, line: &str) -> Option<u64> {
    if session.is_empty() || shape == 0 { return None; }
    let mut sessions = state.sessions.lock();
    if sessions.len() > 64 { sessions.retain(|_, t| now.saturating_sub(t.last_time) < SESSION_GAP_SECS * 2); }
    let tail = sessions.entry(session.to_owned()).or_default();
    let prev = if now.saturating_sub(tail.last_time) <= SESSION_GAP_SECS && tail.last_shape != 0 { Some(tail.last_shape) } else { None };
    tail.last_shape = shape;
    tail.last_time = now;
    tail.recent.push((now, line.to_owned()));
    if tail.recent.len() > SESSION_RECENT_LINES { tail.recent.remove(0); }
    prev
}

fn handle_request(request: &Request, state: &AppState) -> Response {
    let Request { kind, cwd, session, payload } = request;
    if state.stopping.load(Ordering::Acquire) { return Err("daemon is shutting down".into()); }
    let reply = match kind.as_str() {
        "PING" => "PONG".into(),
        "SHUTDOWN" => { shutdown(state)?; "OK".into() }
        "RELOAD" => {
            *state.reload.lock() += 1;
            state.wake.notify_all();
            "OK".into()
        }
        "P" => {
            state.predicts.fetch_add(1, Ordering::Relaxed);
            let now = now_secs();
            let prev = resolve_prev_shape(state, session, now);
            let recent = session_recent(state, session, now);
            combined_predict(state, cwd, payload, prev, &recent)
        }
        "T" | "A" => {
            let _guard = state.mutations.lock();
            if state.stopping.load(Ordering::Acquire) { return Err("daemon is shutting down".into()); }
            let now = now_secs();
            if kind == "T" {
                if is_agent_noise(payload) || scripted_input(state, session, now_millis()) { return Ok(Reply::Text("OK".into())); }
                let shape = line_shape(payload.trim());
                let prev = session_advance(state, session, shape, now, payload.trim());
                let mut model = state.model.lock();
                model.train(payload, &state.home, cwd, now);
                if let Some(prev) = prev { model.record_transition(prev, shape, now); }
            } else {
                state.accepts.fetch_add(1, Ordering::Relaxed);
                state.model.lock().accept_boost(payload.trim(), now);
            }
            state.dirty.store(true, Ordering::Release);
            "OK".into()
        }
        "PL" => {
            let now = now_secs();
            let prev = resolve_prev_shape(state, session, now);
            let recent = session_recent(state, session, now);
            let model = state.model.lock();
            let session_words = session_word_ids(&model, &recent);
            let mut items: Vec<String> = model.topk(payload, cwd, now, 5, prev, session_words.as_ref()).into_iter()
                .map(|(tag, suffix)| format!("{tag}:{suffix}")).collect();
            let paths = index_predictions(state, &model, cwd, payload, now, 3);
            if is_path_shaped(&last_shell_word(payload)) {
                items.splice(0..0, paths);
            } else { items.extend(paths); }
            let mut seen = std::collections::HashSet::new();
            items.retain(|item| seen.insert(strip_tag(item).to_owned()));
            return Ok(Reply::List(items));
        }
        "E" => {
            let now = now_secs();
            let prev = resolve_prev_shape(state, session, now);
            let model = state.model.lock();
            let mut text = model.explain(payload, cwd, now, prev);
            let paths = index_predictions(state, &model, cwd, payload, now, 3);
            text.push_str(&format!("; P: {paths:?}"));
            text
        }
        "PRUNE" => {
            let _guard = state.mutations.lock();
            if state.stopping.load(Ordering::Acquire) { return Err("daemon is shutting down".into()); }
            prune_model(state, payload == "apply")?
        }
        "REINDEX" => {
            let _guard = state.mutations.lock();
            if state.stopping.load(Ordering::Acquire) { return Err("daemon is shutting down".into()); }
            format!("OK paths={}", reindex(state)?)
        }
        "STATS" => {
            let model = state.model.lock();
            format!("lines={} words={} bigram_edges={} trigram_edges={} cmd_transitions={} fs_paths={} sessions={} predicts={} accepts={}",
                model.lines.len(), model.vocab.strings.len(),
                model.bigram.values().map(|m| m.len()).sum::<usize>(), model.trigram.values().map(|m| m.len()).sum::<usize>(),
                model.shape_bigram.len(), state.fsindex.lock().paths.len(), state.sessions.lock().len(),
                state.predicts.load(Ordering::Relaxed), state.accepts.load(Ordering::Relaxed))
        }
        _ => return Err(format!("unknown request kind: {kind}")),
    };
    Ok(Reply::Text(reply))
}

fn prune_model(state: &AppState, apply: bool) -> Result<String, String> {
    let mut model = state.model.lock();
    let ids = model.scripted_line_ids();
    let mut report: Vec<String> = ids.iter().map(|&i| {
        let line = &model.lines[i];
        let text: String = line.text.chars().take(90).collect();
        format!("{:>5}  {}", line.count, text)
    }).collect();
    report.sort_by(|a, b| b.cmp(a));
    let events: u64 = ids.iter().map(|&i| model.lines[i].count as u64).sum();
    if apply && !ids.is_empty() {
        let backup = data_dir().join(format!("model.bin.pre-prune-{}", now_secs()));
        if model_path().exists() { std::fs::copy(model_path(), &backup).map_err(|e| format!("backup failed: {e}"))?; }
        let removed = model.remove_lines(&ids);
        state.dirty.store(true, Ordering::Release);
        drop(model);
        flush_model(state)?;
        report.push(format!("removed {removed} lines ({events} training events); backup: {}", backup.display()));
    } else {
        report.push(format!("{} lines ({events} training events) {}", ids.len(), if apply { "nothing to remove" } else { "would be removed; run `foresight prune --apply`" }));
    }
    Ok(report.join("\n"))
}

fn index_predictions(state: &AppState, model: &Model, cwd: &str, partial: &str, now: u64, count: usize) -> Vec<String> {
    if partial.is_empty() { return Vec::new(); }
    let last = last_shell_word(partial);
    let Some(abs) = expand_home(&last, &state.home, Some(cwd)) else { return Vec::new(); };
    let mut idx = state.fsindex.lock();
    if idx.usage_version != model.version { idx.refresh_usage(model, now, model.version); }
    let cmd = partial.split_whitespace().next().unwrap_or("");
    let candidates = if count == 1 { idx.predict(&abs, wants_dirs(cmd)).into_iter().collect() }
        else { idx.best_components(&abs, wants_dirs(cmd), count) };
    candidates.into_iter().filter_map(|suffix| escape_path_suffix(partial, &suffix).map(|suffix| format!("P:{suffix}"))).collect()
}

fn combined_predict(state: &AppState, cwd: &str, partial: &str, prev_shape: Option<u64>, recent: &[String]) -> String {
    let now = now_secs();
    let model = state.model.lock();
    let session_words = session_word_ids(&model, recent);
    let explicit = is_path_shaped(&last_shell_word(partial));
    if explicit {
        if let Some(path) = index_predictions(state, &model, cwd, partial, now, 1).pop() { return path; }
    }
    let (tag, suffix) = model.predict_with_session(partial, cwd, now, !explicit, prev_shape, session_words.as_ref());
    if tag != '\0' && (tag != 'F' || explicit) { return format!("{tag}:{suffix}"); }
    if let Some(path) = index_predictions(state, &model, cwd, partial, now, 1).pop() { return path; }
    if tag == '\0' { String::new() } else { format!("{tag}:{suffix}") }
}

struct Connection { reader: BufReader<UnixStream> }
impl Connection {
    fn connect() -> io::Result<Self> {
        let stream = UnixStream::connect(sock_path())?;
        stream.set_read_timeout(Some(Duration::from_secs(60)))?;
        stream.set_write_timeout(Some(Duration::from_secs(5)))?;
        Ok(Self { reader: BufReader::new(stream) })
    }
    fn request(&mut self, kind: &str, cwd: &str, payload: &str) -> io::Result<Response> {
        let request = Request { kind: kind.into(), cwd: cwd.into(), payload: payload.into(), session: env::var("FORESIGHT_SESSION").unwrap_or_default() };
        protocol::write_json(self.reader.get_mut(), &request)?;
        protocol::read_json(&mut self.reader)
    }
}
fn client_request(kind: &str, cwd: &str, payload: &str) -> Response {
    integrations::materialize_if_stale();
    let mut connection = match Connection::connect() {
        Ok(connection) => connection,
        Err(e) if matches!(e.kind(), io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused) => {
            spawn_daemon_and_wait()?;
            Connection::connect().map_err(|e| e.to_string())?
        }
        Err(e) => return Err(e.to_string()),
    };
    // Never retry an ambiguously acknowledged mutation.
    connection.request(kind, cwd, payload).map_err(|e| e.to_string())?
}
fn notify_running(kind: &str) -> Result<(), String> {
    match Connection::connect() {
        Ok(mut connection) => { connection.request(kind, "", "").map_err(|e| e.to_string())??; Ok(()) }
        Err(e) if matches!(e.kind(), io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused) => Ok(()),
        Err(e) => Err(e.to_string()),
    }
}
fn spawn_daemon_and_wait() -> Result<(), String> {
    Command::new(env::current_exe().map_err(|e| e.to_string())?).arg("serve")
        .stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).process_group(0)
        .spawn().map_err(|e| e.to_string())?;
    for _ in 0..100 {
        thread::sleep(Duration::from_millis(50));
        if let Ok(mut connection) = Connection::connect() {
            if matches!(connection.request("PING", "", ""), Ok(Ok(Reply::Text(text))) if text == "PONG") { return Ok(()); }
        }
    }
    Err("daemon did not become ready".into())
}
fn ensure_daemon() -> Result<(), String> {
    integrations::materialize_if_stale();
    match Connection::connect() {
        Ok(mut connection) => {
            let reply = text_reply(connection.request("PING", "", "").map_err(|e| e.to_string())??)?;
            if reply == "PONG" { Ok(()) } else { Err("invalid daemon ping reply".into()) }
        }
        Err(e) if matches!(e.kind(), io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused) => spawn_daemon_and_wait(),
        Err(e) => Err(e.to_string()),
    }
}
fn run_stream() -> Result<(), String> {
    let mut input = io::stdin().lock();
    let mut output = io::stdout().lock();
    let mut connection = None;
    while let Some(cwd) = protocol::read_nul(&mut input).map_err(|e| e.to_string())? {
        let payload = protocol::read_nul(&mut input).map_err(|e| e.to_string())?.ok_or("stream ended without payload")?;
        let mut reply = None;
        for attempt in 0..2 {
            if connection.is_none() {
                ensure_daemon()?;
                connection = Some(Connection::connect().map_err(|e| e.to_string())?);
            }
            match connection.as_mut().unwrap().request("P", &cwd, &payload) {
                Ok(response) => { reply = Some(text_reply(response?)?); break; }
                Err(e) => {
                    connection = None;
                    if attempt == 1 { return Err(e.to_string()); }
                }
            }
        }
        let reply = reply.ok_or("prediction transport failed")?;
        output.write_all(strip_tag(&reply).as_bytes()).map_err(|e| e.to_string())?;
        output.write_all(b"\0").map_err(|e| e.to_string())?;
        output.flush().map_err(|e| e.to_string())?;
    }
    Ok(())
}

#[cfg(feature = "bench")]
fn run_bench(n: u64) -> Result<(), String> {
    use std::time::Instant;
    let home = env::var("HOME").map_err(|e| e.to_string())?;
    let history = std::fs::read_to_string(history_path()).map_err(|e| e.to_string())?;
    let lines: Vec<&str> = history.lines().filter(|line| !line.trim().is_empty() && !line.starts_with('#')).collect();
    if !lines.iter().any(|line| line.chars().count() >= 2) { return Err("history has no replayable commands".into()); }
    let mut performed = 0u64;
    let mut model_correct = 0u64;
    let mut baseline_correct = 0u64;
    let mut model_offered = 0u64;
    let mut baseline_offered = 0u64;
    let start = Instant::now();
    while performed < n {
        // Every pass starts empty. Repetitions within the chronological history
        // are observations, never collapsed into a unique-command set.
        let mut model = Model::default();
        let mut prior = Vec::<&str>::new();
        let mut prev = None;
        for (i, line) in lines.iter().enumerate() {
            let now = i as u64 + 1;
            if let Some((cut, _)) = line.char_indices().nth(line.chars().count() / 2).filter(|(cut, _)| *cut > 0) {
                let prefix = &line[..cut];
                let truth = &line[cut..];
                let (tag, suffix) = model.predict_with_session(prefix, &home, now, true, prev, None);
                if tag != '\0' && !suffix.is_empty() {
                    model_offered += 1;
                    model_correct += u64::from(truth.starts_with(&suffix));
                }
                if let Some(candidate) = prior.iter().rev().find(|candidate| **candidate != prefix && candidate.starts_with(prefix)) {
                    baseline_offered += 1;
                    baseline_correct += u64::from(truth.starts_with(&candidate[prefix.len()..]));
                }
                performed += 1;
                if performed == n { break; }
            }
            model.train(line, &home, &home, now);
            let shape = line_shape(line);
            if let Some(previous) = prev { model.record_transition(previous, shape, now); }
            prev = Some(shape);
            prior.push(line);
        }
    }
    println!("offline chronological replay; identical prior observations; no daemon requests");
    println!("iterations: {performed}\nelapsed: {:.3}s", start.elapsed().as_secs_f64());
    for (name, offered, correct) in [("foresight", model_offered, model_correct), ("zsh-autosuggestions", baseline_offered, baseline_correct)] {
        println!("{name}: coverage={:.2}% accuracy={:.2}% ({correct}/{performed})", offered as f64 * 100.0 / performed as f64, correct as f64 * 100.0 / performed as f64);
    }
    Ok(())
}
