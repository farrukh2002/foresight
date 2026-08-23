use std::io::Write;
use std::path::{Component, Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

pub fn data_dir() -> PathBuf {
    PathBuf::from(std::env::var("HOME").expect("HOME not set")).join(".local/share/foresight")
}

pub fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
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
        Some(lexical_normalize(&Path::new(cwd).join(token)).to_string_lossy().to_string())
    }
}

pub fn last_shell_word(line: &str) -> String {
    let mut words: Vec<String> = vec![String::new()];
    let mut chars = line.chars().peekable();
    let mut quote: Option<char> = None;
    let mut started = false;

    while let Some(c) = chars.next() {
        if let Some(q) = quote {
            if c == q {
                quote = None;
            } else if q == '"' && c == '\\' && matches!(chars.peek(), Some('"' | '\\' | '$')) {
                words.last_mut().unwrap().push(chars.next().unwrap());
            } else {
                words.last_mut().unwrap().push(c);
            }
            started = true;
        } else if c == '\'' || c == '"' {
            quote = Some(c);
            started = true;
        } else if c == '\\' {
            if let Some(next) = chars.next() {
                words.last_mut().unwrap().push(next);
                started = true;
            }
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
    words.pop().unwrap_or_default()
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
}
