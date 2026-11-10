use std::io::Write;
use std::path::{Component, Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

pub fn data_dir() -> PathBuf {
    PathBuf::from(std::env::var("HOME").expect("HOME not set")).join(".local/share/foresight")
}

pub fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

pub fn now_millis() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

pub fn wants_dirs(command: &str) -> bool {
    matches!(command, "cd" | "pushd" | "popd" | "mkdir" | "rmdir" | "tree" | "md" | "j")
}

pub fn is_path_shaped(token: &str) -> bool {
    token.starts_with('/') || token.starts_with('~') || token.starts_with("./") || token.starts_with("../") || token.contains('/')
}

pub fn lexical_normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for comp in path.components() {
        match comp {
            Component::CurDir => {}
            Component::ParentDir => {
                if !matches!(out.components().next_back(), None | Some(Component::RootDir)) {
                    out.pop();
                }
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

pub fn expand_home(token: &str, home: &str, cwd: Option<&str>) -> Option<String> {
    if token == "~" {
        Some(home.to_string())
    } else if let Some(rest) = token.strip_prefix("~/") {
        Some(format!("{home}/{rest}"))
    } else if token.starts_with('/') {
        Some(token.to_string())
    } else if token.is_empty() || token.starts_with('-') {
        None
    } else {
        let cwd = cwd?;
        let mut abs = lexical_normalize(&Path::new(cwd).join(token)).to_string_lossy().to_string();
        if token.ends_with('/') && !abs.ends_with('/') {
            abs.push('/');
        }
        Some(abs)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum QuoteStyle {
    Single,
    Double,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ShellLineState {
    words: Vec<String>,
    open_quote: Option<QuoteStyle>,
    trailing_escape: bool,
    started: bool,
}

fn parse_shell_line(line: &str) -> ShellLineState {
    let mut words: Vec<String> = vec![String::new()];
    let mut chars = line.chars().peekable();
    let mut open_quote: Option<QuoteStyle> = None;
    let mut trailing_escape = false;
    let mut started = false;

    while let Some(c) = chars.next() {
        trailing_escape = false;
        match open_quote {
            Some(QuoteStyle::Single) => {
                if c == '\'' {
                    open_quote = None;
                } else {
                    words.last_mut().unwrap().push(c);
                }
                started = true;
            }
            Some(QuoteStyle::Double) => {
                if c == '"' {
                    open_quote = None;
                } else if c == '\\' {
                    if let Some(&next) = chars.peek() {
                        if matches!(next, '"' | '\\' | '$' | '`') {
                            words.last_mut().unwrap().push(chars.next().unwrap());
                        } else if next == '\n' {
                            chars.next();
                        } else {
                            words.last_mut().unwrap().push('\\');
                        }
                    } else {
                        trailing_escape = true;
                    }
                } else {
                    words.last_mut().unwrap().push(c);
                }
                started = true;
            }
            None => {
                if c == '\'' {
                    open_quote = Some(QuoteStyle::Single);
                    started = true;
                } else if c == '"' {
                    open_quote = Some(QuoteStyle::Double);
                    started = true;
                } else if c == '\\' {
                    if let Some(next) = chars.next() {
                        if next != '\n' {
                            words.last_mut().unwrap().push(next);
                        }
                    } else {
                        trailing_escape = true;
                    }
                    started = true;
                } else if c.is_whitespace() {
                    if started {
                        words.push(String::new());
                        started = false;
                    }
                } else {
                    words.last_mut().unwrap().push(c);
                    started = true;
                }
            }
        }
    }

    ShellLineState {
        words,
        open_quote,
        trailing_escape,
        started,
    }
}

pub fn last_shell_word(line: &str) -> String {
    let mut state = parse_shell_line(line);
    state.words.pop().unwrap_or_default()
}

fn needs_unquoted_escape(c: char) -> bool {
    c.is_whitespace()
        || matches!(
            c,
            '\\' | '\'' | '"' | '$' | '`' | ';' | '&' | '|' | '<' | '>' | '(' | ')' | '*' | '?' | '[' | ']' | '{' | '}' | '~' | '#' | '=' | '!' | '^'
        )
}

pub fn escape_path_suffix(line: &str, suffix: &str) -> Option<String> {
    if suffix.contains('\0') {
        return None;
    }

    let state = parse_shell_line(line);
    let mut out = String::new();

    match state.open_quote {
        Some(QuoteStyle::Single) => {
            for c in suffix.chars() {
                if c == '\'' {
                    out.push_str(r"'\''");
                } else {
                    out.push(c);
                }
            }
            out.push('\'');
            Some(out)
        }
        Some(QuoteStyle::Double) => {
            let mut chars = suffix.chars();
            if state.trailing_escape {
                let first = chars.next()?;
                if matches!(first, '"' | '\\' | '$' | '`') {
                    out.push(first);
                } else {
                    return None;
                }
            }
            let mut in_dq = true;
            for c in chars {
                if c == '!' {
                    if in_dq {
                        out.push('"');
                        in_dq = false;
                    }
                    out.push('\'');
                    out.push('!');
                    out.push('\'');
                } else {
                    if !in_dq {
                        out.push('"');
                        in_dq = true;
                    }
                    if matches!(c, '"' | '\\' | '$' | '`') {
                        out.push('\\');
                    }
                    out.push(c);
                }
            }
            if in_dq {
                out.push('"');
            }
            Some(out)
        }
        None => {
            let mut chars = suffix.chars();
            if state.trailing_escape {
                let first = chars.next()?;
                if first == '\n' {
                    return None;
                }
                out.push(first);
            }
            for c in chars {
                if c == '\n' {
                    out.push_str("'\n'");
                } else if needs_unquoted_escape(c) {
                    out.push('\\');
                    out.push(c);
                } else {
                    out.push(c);
                }
            }
            Some(out)
        }
    }
}

pub fn atomic_write(
    path: &Path,
    write: impl FnOnce(&mut std::io::BufWriter<std::fs::File>) -> std::io::Result<()>,
) -> std::io::Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    if !parent.as_os_str().is_empty() {
        std::fs::create_dir_all(parent)?;
    }
    let file_stem = path.file_name().and_then(|s| s.to_str()).unwrap_or("foresight");
    let pid = std::process::id();

    let mut tmp_path = None;
    let mut file = None;
    let mut attempts = 0;
    while attempts < 100 {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let candidate = parent.join(format!(".{file_stem}.tmp.{pid}.{nanos}.{attempts}"));
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&candidate)
        {
            Ok(f) => {
                tmp_path = Some(candidate);
                file = Some(f);
                break;
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                attempts += 1;
            }
            Err(e) => return Err(e),
        }
    }

    let tmp_path = tmp_path.ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            "failed to create unique temporary file for atomic write after 100 attempts",
        )
    })?;
    let file = file.unwrap();

    struct TempCleanup<'a>(&'a Path, bool);
    impl<'a> Drop for TempCleanup<'a> {
        fn drop(&mut self) {
            if self.1 {
                let _ = std::fs::remove_file(self.0);
            }
        }
    }

    let mut cleanup = TempCleanup(&tmp_path, true);

    {
        let mut writer = std::io::BufWriter::new(file);
        write(&mut writer)?;
        writer.flush()?;
        writer.get_ref().sync_all()?;
    }

    std::fs::rename(&tmp_path, path)?;
    cleanup.1 = false;
    Ok(())
}

pub fn write_snapshot(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    atomic_write(path, |w| w.write_all(bytes))
}

pub fn remove_stale_temp_files(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some(rest) = name.strip_prefix('.').and_then(|n| n.split_once(".tmp.")).map(|(_, r)| r.to_owned()) else { continue };
        let Some(pid) = rest.split('.').next().and_then(|p| p.parse::<u32>().ok()) else { continue };
        if pid != std::process::id() && !Path::new(&format!("/proc/{pid}")).exists() {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

pub fn fnv1a64(data: &[u8]) -> u64 {
    const OFFSET: u64 = 0xcbf29ce484222325;
    const PRIME: u64 = 0x100000001b3;
    let mut hash = OFFSET;
    for &b in data {
        hash ^= b as u64;
        hash = hash.wrapping_mul(PRIME);
    }
    hash
}

pub fn log(msg: &str) {
    let ts = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(data_dir().join("foresight.log")) {
        let _ = f.write_all(format!("[{ts}] {msg}\n").as_bytes());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stale_temp_files_of_dead_processes_are_removed() {
        let dir = std::env::temp_dir().join(format!("foresight-stale-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let dead = dir.join(".model.bin.tmp.4294967.123.0");
        let mine = dir.join(format!(".model.bin.tmp.{}.123.0", std::process::id()));
        let other = dir.join("model.bin");
        for f in [&dead, &mine, &other] { std::fs::write(f, b"x").unwrap(); }
        remove_stale_temp_files(&dir);
        assert!(!dead.exists());
        assert!(mine.exists() && other.exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn lexical_normalize_collapses_dots() {
        assert_eq!(lexical_normalize(Path::new("/a/b/../c")), PathBuf::from("/a/c"));
        assert_eq!(lexical_normalize(Path::new("/a/./b")), PathBuf::from("/a/b"));
        assert_eq!(lexical_normalize(Path::new("/../a")), PathBuf::from("/a"));
    }

    #[test]
    fn expand_home_absolute_and_tilde() {
        assert_eq!(expand_home("~", "/home/u", None), Some("/home/u".to_string()));
        assert_eq!(expand_home("~/x", "/home/u", None), Some("/home/u/x".to_string()));
        assert_eq!(expand_home("/etc/passwd", "/home/u", None), Some("/etc/passwd".to_string()));
    }

    #[test]
    fn expand_home_relative_needs_cwd() {
        assert_eq!(expand_home("Documents", "/home/u", None), None);
        assert_eq!(
            expand_home("Documents", "/home/u", Some("/home/u")),
            Some("/home/u/Documents".to_string())
        );
        assert_eq!(
            expand_home("../x", "/home/u", Some("/home/u/sub")),
            Some("/home/u/x".to_string())
        );
    }

    #[test]
    fn expand_home_flags_are_not_paths() {
        assert_eq!(expand_home("-la", "/home/u", Some("/home/u")), None);
    }

    #[test]
    fn last_shell_word_handles_quotes_and_escapes() {
        assert_eq!(last_shell_word("cd foo"), "foo");
        assert_eq!(last_shell_word(r#"cd "My Documents"#), "My Documents");
        assert_eq!(last_shell_word(r"cd My\ Documents"), "My Documents");
        assert_eq!(last_shell_word("cd 'a b'"), "a b");
        assert_eq!(last_shell_word("ls -la"), "-la");
    }

    #[test]
    fn is_path_shaped_distinguishes_paths_from_bare_words() {
        assert!(is_path_shaped("/etc/passwd"));
        assert!(is_path_shaped("~/Documents"));
        assert!(is_path_shaped("~"));
        assert!(is_path_shaped("./run.sh"));
        assert!(is_path_shaped("../lib"));
        assert!(is_path_shaped("src/main.rs"));
        assert!(!is_path_shaped("status"));
        assert!(!is_path_shaped("st"));
        assert!(!is_path_shaped("Cargo.toml"));
    }

    #[test]
    fn fnv1a64_is_deterministic() {
        assert_eq!(fnv1a64(b"hello"), fnv1a64(b"hello"));
        assert_ne!(fnv1a64(b"hello"), fnv1a64(b"hellp"));
    }

    fn bash_last_arg(cmd: &str) -> String {
        let output = std::process::Command::new("bash")
            .arg("-c")
            .arg(format!("printf '%s\\0' {cmd}"))
            .output()
            .expect("bash execution failed");
        let stdout = output.stdout;
        let mut last = String::new();
        for part in stdout.split(|&b| b == 0) {
            if !part.is_empty() {
                last = String::from_utf8_lossy(part).to_string();
            }
        }
        last
    }

    #[test]
    fn escape_path_suffix_real_shell_argv_assertions() {
        let cases = [
            // Unquoted with spaces and shell punctuation
            ("cat foo", " bar;baz&qux|zap$1", "foo bar;baz&qux|zap$1"),
            // Unquoted with embedded newline
            ("cat foo", " bar\nbaz", "foo bar\nbaz"),
            // Open single quote with internal single quote and spaces
            ("cat 'foo", " bar's", "foo bar's"),
            // Open double quote with $ and ! (protects against interactive history expansion)
            ("cat \"foo", " bar!baz$1", "foo bar!baz$1"),
            // Closed quotes followed by unquoted space
            ("cat \"foo\"", " bar", "foo bar"),
            // Unquoted trailing escape continuation
            ("cat foo\\", " bar", "foo bar"),
        ];
        for (line, suffix, expected) in cases {
            let esc = escape_path_suffix(line, suffix).expect("should escape");
            let full = format!("{line}{esc}");
            assert_eq!(bash_last_arg(&full), expected);
        }
    }

    #[test]
    fn escape_path_suffix_dangling_escape_and_unrepresentable() {
        // Valid continuation after trailing backslash
        let esc = escape_path_suffix("cat foo\\", " bar").unwrap();
        assert_eq!(bash_last_arg(&format!("cat foo\\{esc}")), "foo bar");

        // Dangling backslash with empty suffix cannot represent empty append
        assert_eq!(escape_path_suffix("cat foo\\", ""), None);
        // Dangling backslash followed by newline is line continuation, impossible for literal newline
        assert_eq!(escape_path_suffix("cat foo\\", "\n"), None);
        // Null byte impossible in shell
        assert_eq!(escape_path_suffix("cat foo", "bar\0baz"), None);
        // Double quote with trailing escape and invalid char
        assert_eq!(escape_path_suffix("cat \"foo\\", "a"), None);
    }

    #[test]
    fn atomic_write_creates_and_overwrites() {
        let pid = std::process::id();
        let nanos = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_nanos();
        let tmp_dir = std::env::temp_dir().join(format!("foresight_test_{pid}_{nanos}"));
        let target = tmp_dir.join("test_file.txt");
        atomic_write(&target, |w| {
            w.write_all(b"hello atomic")?;
            Ok(())
        }).expect("atomic write should succeed");

        let contents = std::fs::read_to_string(&target).expect("target must exist");
        assert_eq!(contents, "hello atomic");

        // Overwrite
        atomic_write(&target, |w| {
            w.write_all(b"overwritten")?;
            Ok(())
        }).expect("atomic overwrite should succeed");
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "overwritten");

        let _ = std::fs::remove_dir_all(&tmp_dir);
    }

    #[test]
    fn atomic_write_cleans_up_on_failure() {
        let pid = std::process::id();
        let nanos = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_nanos();
        let tmp_dir = std::env::temp_dir().join(format!("foresight_fail_{pid}_{nanos}"));
        let target = tmp_dir.join("target.txt");
        std::fs::create_dir_all(&tmp_dir).unwrap();
        std::fs::write(&target, "original").unwrap();

        let res = atomic_write(&target, |_| {
            Err(std::io::Error::new(std::io::ErrorKind::Other, "write error"))
        });
        assert!(res.is_err());
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "original");

        // Check no tmp files left in directory
        let entries: Vec<_> = std::fs::read_dir(&tmp_dir).unwrap().flatten().collect();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].file_name(), "target.txt");

        let _ = std::fs::remove_dir_all(&tmp_dir);
    }
}
