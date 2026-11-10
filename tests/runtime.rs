use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};
use serde_json::{json, Value};

struct Home { path: PathBuf, child: Option<Child> }
impl Home {
    fn new() -> Self { Self::with_burst(0) }
    fn with_burst(burst_ms: u64) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!("foresight-runtime-{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
        fs::create_dir_all(path.join(".config/foresight")).unwrap();
        fs::write(path.join(".config/foresight/config.toml"), format!("[scan]\nroots = [{}]\n[update]\nmode = 'manual'\n[train]\nburst_ms = {burst_ms}\n", serde_json::to_string(path.to_str().unwrap()).unwrap())).unwrap();
        Self { path, child: None }
    }
    fn socket(&self) -> PathBuf { self.path.join("daemon.sock") }
    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_foresight"));
        command.env("HOME", &self.path).env("FORESIGHT_SOCK", self.socket())
            .env("FORESIGHT_SESSION", "test-session").current_dir(&self.path);
        command
    }
    fn start(&mut self) {
        self.child = Some(self.command().arg("serve").stdout(Stdio::null()).stderr(Stdio::null()).spawn().unwrap());
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if UnixStream::connect(self.socket()).is_ok() { break; }
            assert!(Instant::now() < deadline, "daemon startup timeout");
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(self.request("PING", "")["Ok"], "PONG");
        assert!(self.request("REINDEX", "").get("Ok").is_some());
    }
    fn request(&self, kind: &str, payload: &str) -> Value {
        let mut socket = UnixStream::connect(self.socket()).unwrap();
        socket.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        writeln!(socket, "{}", json!({"kind":kind,"cwd":self.path.to_str().unwrap(),"session":"test-session","payload":payload})).unwrap();
        let mut response = String::new();
        BufReader::new(socket).read_line(&mut response).unwrap();
        serde_json::from_str(&response).unwrap()
    }
    fn stop(&mut self) {
        assert_eq!(self.request("SHUTDOWN", "")["Ok"], "OK");
        let deadline = Instant::now() + Duration::from_secs(10);
        let child = self.child.as_mut().unwrap();
        loop {
            if let Some(status) = child.try_wait().unwrap() { assert!(status.success()); break; }
            assert!(Instant::now() < deadline, "daemon shutdown timeout");
            std::thread::sleep(Duration::from_millis(10));
        }
        self.child = None;
    }
}
impl Drop for Home {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() { let _ = child.kill(); let _ = child.wait(); }
        let _ = fs::remove_dir_all(&self.path);
    }
}

#[test]
fn acknowledged_training_survives_shutdown_and_bare_history_beats_files() {
    let mut home = Home::new();
    fs::write(home.path.join("status-local"), b"").unwrap();
    home.start();
    assert_eq!(home.request("T", "git status")["Ok"], "OK");
    assert_eq!(home.request("P", "git sta")["Ok"], "L:tus");
    let list = home.request("PL", "git sta");
    assert_eq!(list["Ok"][0], "L:tus");
    assert_eq!(home.request("P", "cat ./sta")["Ok"], "P:tus-local");
    let multiline = "printf 'first\nsecond\t  value'";
    assert_eq!(home.request("T", multiline)["Ok"], "OK");
    home.stop();
    home.start();
    assert_eq!(home.request("P", "git sta")["Ok"], "L:tus");
    assert_eq!(home.request("P", "printf 'first")["Ok"], "L:\nsecond\t  value'");
    home.stop();
}

#[test]
fn failed_shutdown_reports_error_and_retains_dirty_training_for_retry() {
    let mut home = Home::new();
    home.start();
    let model = home.path.join(".local/share/foresight/model.bin");
    fs::remove_file(&model).unwrap();
    fs::create_dir(&model).unwrap();
    assert_eq!(home.request("T", "echo durable-training")["Ok"], "OK");
    assert!(home.request("SHUTDOWN", "").get("Err").is_some());
    assert_eq!(home.request("PING", "")["Ok"], "PONG");
    fs::remove_dir(&model).unwrap();
    home.stop();
    home.start();
    assert_eq!(home.request("P", "echo durable-")["Ok"], "L:training");
    home.stop();
}

#[test]
fn existing_pid_text_is_not_a_lock_and_concurrent_serve_does_not_replace_daemon() {
    let mut home = Home::new();
    let data = home.path.join(".local/share/foresight");
    fs::create_dir_all(&data).unwrap();
    fs::write(data.join("daemon.pid"), std::process::id().to_string()).unwrap();
    home.start();
    assert_eq!(home.request("T", "echo singleton-observation")["Ok"], "OK");
    let mut contenders: Vec<_> = (0..4).map(|_| home.command().arg("serve").spawn().unwrap()).collect();
    for child in &mut contenders { assert!(child.wait().unwrap().success()); }
    assert_eq!(home.request("P", "echo singleton-")["Ok"], "L:observation");
    home.stop();
}

#[test]
fn cli_errors_are_not_successful_empty_replies() {
    let home = Home::new();
    for args in [vec!["predict"], vec!["unknown"], vec!["stats", "extra"], vec!["update", "--pin"],
        vec!["update", "--channel"], vec!["update", "--bogus"], vec!["update", "--pin", "v1.0.0", "--channel", "stable"],
        vec!["update", "--enable-silent", "--disable-silent"]] {
        assert!(!home.command().args(&args).output().unwrap().status.success(), "{args:?}");
    }
    let listener = UnixListener::bind(home.socket()).unwrap();
    let server = std::thread::spawn(move || {
        let (mut socket, _) = listener.accept().unwrap();
        let mut line = String::new();
        BufReader::new(socket.try_clone().unwrap()).read_line(&mut line).unwrap();
        writeln!(socket, "{}", json!({"Err":"deliberate failure"})).unwrap();
    });
    let output = home.command().args(["predict", "anything"]).output().unwrap();
    server.join().unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8(output.stderr).unwrap().contains("deliberate failure"));
    assert!(output.stdout.is_empty());
}

#[test]
fn nul_stream_reconnects_after_eof_and_preserves_fields_and_replies() {
    let home = Home::new();
    let listener = UnixListener::bind(home.socket()).unwrap();
    let field = "cwd\t\n  ";
    let payload = "payload\n\t  ";
    let server = std::thread::spawn(move || {
        // ensure-daemon PING, failed P, reconnect PING, successful P.
        for (attempt, expected) in ["PING", "P", "PING", "P"].into_iter().enumerate() {
            let (mut socket, _) = listener.accept().unwrap();
            let mut line = String::new();
            BufReader::new(socket.try_clone().unwrap()).read_line(&mut line).unwrap();
            let request: Value = serde_json::from_str(&line).unwrap();
            assert_eq!(request["kind"], expected);
            if expected == "PING" { writeln!(socket, "{}", json!({"Ok":"PONG"})).unwrap(); }
            else {
                assert_eq!(request["cwd"], field);
                assert_eq!(request["payload"], payload);
                // The first prediction receives EOF, not a successful empty suffix.
                if attempt == 3 {
                    writeln!(socket, "{}", json!({"Ok":"L:reply\n\t  "})).unwrap();
                }
            }
        }
    });
    let mut child = home.command().arg("stream").stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
    child.stdin.take().unwrap().write_all(format!("{field}\0{payload}\0").as_bytes()).unwrap();
    let output = child.wait_with_output().unwrap();
    server.join().unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(output.stdout, b"reply\n\t  \0");
}

#[test]
fn list_null_does_not_split_multiline_or_tab_suffixes() {
    let home = Home::new();
    let listener = UnixListener::bind(home.socket()).unwrap();
    let server = std::thread::spawn(move || {
        let (mut socket, _) = listener.accept().unwrap();
        let mut line = String::new();
        BufReader::new(socket.try_clone().unwrap()).read_line(&mut line).unwrap();
        writeln!(socket, "{}", json!({"Ok":["L:first\n\t  ","P:second"]})).unwrap();
    });
    let output = home.command().args(["list", "--null", "x"]).output().unwrap();
    server.join().unwrap();
    assert!(output.status.success());
    assert_eq!(output.stdout, b"first\n\t  \0second\0");
}

#[cfg(feature = "bench")]
#[test]
fn benchmark_replays_repetitions_offline_and_reports_performed_count() {
    let home = Home::new();
    fs::write(home.path.join(".bash_history"), "echo repeated\necho repeated\necho repeated\n").unwrap();
    let data = home.path.join(".local/share/foresight");
    fs::create_dir_all(&data).unwrap();
    fs::write(data.join("model.bin"), b"untouched-live-model").unwrap();
    let output = home.command().args(["bench", "5"]).output().unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let report = String::from_utf8(output.stdout).unwrap();
    assert!(report.contains("iterations: 5"));
    assert_eq!(report.matches("(3/5)").count(), 2, "{report}");
    assert_eq!(fs::read(data.join("model.bin")).unwrap(), b"untouched-live-model");
    assert!(!home.socket().exists());
}

#[test]
fn filesystem_suffixes_append_as_one_literal_shell_word() {
    let mut home = Home::new();
    let name = "a b;$literal";
    fs::write(home.path.join(name), b"").unwrap();
    home.start();
    for partial in ["cat ./a", "cat './a "] {
        let response = home.request("P", partial);
        let tagged = response["Ok"].as_str().unwrap();
        let suffix = tagged.strip_prefix("P:").expect("filesystem prediction");
        let script = format!("set -- {partial}{suffix}; printf '%s\\0' \"$@\"");
        let parsed = Command::new("bash").env("HOME", &home.path).env_remove("BASH_ENV")
            .current_dir(&home.path).args(["--noprofile", "--norc", "-c", &script]).output().unwrap();
        assert!(parsed.status.success(), "{}", String::from_utf8_lossy(&parsed.stderr));
        assert_eq!(parsed.stdout, format!("cat\0./{name}\0").as_bytes());
    }
    home.stop();
}

#[test]
fn scripted_bursts_and_agent_noise_are_not_learned() {
    let mut home = Home::with_burst(300);
    home.start();
    assert_eq!(home.request("T", "echo typed-by-hand")["Ok"], "OK");
    for line in ["echo scripted-one", "echo scripted-two", "echo scripted-three"] {
        assert_eq!(home.request("T", line)["Ok"], "OK");
    }
    std::thread::sleep(Duration::from_millis(1500));
    assert_eq!(home.request("T", "head -n 1000 >> \"$SNAPSHOT_FILE\"")["Ok"], "OK");
    std::thread::sleep(Duration::from_millis(400));
    assert_eq!(home.request("T", "echo typed-later")["Ok"], "OK");
    assert_eq!(home.request("P", "echo typed-b")["Ok"], "L:y-hand");
    assert_eq!(home.request("P", "echo typed-l")["Ok"], "L:ater");
    assert_eq!(home.request("P", "echo scripted-")["Ok"], "");
    assert_eq!(home.request("P", "head -n 1000 >> ")["Ok"], "");
    home.stop();
}

#[test]
fn relative_directory_with_trailing_slash_completes_a_child_not_a_slash() {
    let mut home = Home::new();
    fs::create_dir_all(home.path.join("docs/alpha")).unwrap();
    fs::create_dir_all(home.path.join("docs/beta")).unwrap();
    home.start();
    let reply = home.request("P", "cd docs/")["Ok"].as_str().unwrap().to_string();
    assert!(reply == "P:alpha/" || reply == "P:beta/", "{reply}");
    home.stop();
}

#[test]
fn directory_prediction_ranks_children_by_usage_beyond_the_first_alphabetical_ones() {
    let mut home = Home::new();
    fs::create_dir_all(home.path.join("Documents/GitHub")).unwrap();
    for n in 0..300 { fs::write(home.path.join(format!("Documents/GitHub/file-{n:03}")), b"").unwrap(); }
    fs::create_dir_all(home.path.join("Documents/kernel-builder/kernel")).unwrap();
    home.start();
    for _ in 0..4 { assert_eq!(home.request("T", "ls Documents/kernel-builder/kernel")["Ok"], "OK"); }
    assert_eq!(home.request("P", "cd Documents/")["Ok"], "P:kernel-builder/");
    home.stop();
}
