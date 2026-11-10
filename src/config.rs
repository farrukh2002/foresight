use serde::Deserialize;
use std::path::PathBuf;

use crate::util::log;

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
pub struct Config {
    pub scan: ScanConfig,
    pub update: UpdateConfig,
    pub train: TrainConfig,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct TrainConfig {
    pub burst_ms: u64,
}

impl Default for TrainConfig {
    fn default() -> Self {
        TrainConfig { burst_ms: 250 }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct UpdateConfig {
    pub mode: String,
    pub channel: String,
    pub pinned_version: String,
    pub check_interval_hours: u64,
}

impl Default for UpdateConfig {
    fn default() -> Self {
        UpdateConfig {
            mode: "manual".into(),
            channel: String::new(),
            pinned_version: String::new(),
            check_interval_hours: 24,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct ScanConfig {
    pub mode: String,
    pub max_entries_normal: usize,
    pub max_entries_deep: usize,
    pub roots: Vec<String>,
    pub exclude_names: Vec<String>,
    pub exclude_paths: Vec<String>,
    pub normal_mode_exclude_names: Vec<String>,
    pub normal_mode_exclude_paths: Vec<String>,
    pub max_dir_entries: usize,
}

impl Default for ScanConfig {
    fn default() -> Self {
        ScanConfig {
            mode: "normal".into(),
            max_entries_normal: 200_000,
            max_entries_deep: 5_000_000,
            roots: vec!["~".into()],
            exclude_names: strs(&[
                "node_modules", ".git", ".cache", "Cache", "Code Cache", "GPUCache",
                "__pycache__", ".venv", "venv", "site-packages", "target",
                ".gradle", ".m2", "build", ".dart_tool",
            ]),
            exclude_paths: strs(&[
                ".cargo/registry", ".rustup/toolchains", ".npm/_cacache", ".pub-cache",
                "go/pkg/mod", "Android/Sdk", ".android/avd", "snap",
            ]),
            normal_mode_exclude_names: vec![],
            normal_mode_exclude_paths: strs(&["Android", "go", "flutter"]),
            max_dir_entries: 4000,
        }
    }
}

const BUILTIN_EXCLUDE_NAMES: &[&str] = &[
    "out", "out_*", "out-*", ".next", ".turbo", "Crashpad", "Service Worker", "IndexedDB",
    "Local Storage", "Session Storage", "blob_storage", "CacheStorage", "ShaderCache",
    "GrShaderCache", "DawnCache", "component_crx_cache",
];

const BUILTIN_EXCLUDE_PATHS: &[&str] = &[
    ".config/google-chrome*", ".config/BraveSoftware", ".config/chromium",
    ".config/microsoft-edge", ".config/Code", ".config/Cursor", ".config/Antigravity",
    ".config/t3code*", ".config/Codex", ".config/mozilla", ".mozilla", ".var",
    ".local/share/Trash", ".bun/install/cache",
];

fn strs(items: &[&str]) -> Vec<String> {
    items.iter().map(|s| s.to_string()).collect()
}

pub const TEMPLATE: &str = r#"# foresight filesystem index config.

[scan]
# scan mode: "normal" or "deep"
mode = "normal"

# entry cap in normal mode
max_entries_normal = 200000

# entry cap in deep mode
max_entries_deep = 5000000

# directories to walk ("~" = $HOME)
roots = ["~"]

# directory names skipped at any depth, both modes
exclude_names = [
  "node_modules", ".git", ".cache", "Cache", "Code Cache", "GPUCache",
  "__pycache__", ".venv", "venv", "site-packages", "target",
  ".gradle", ".m2", "build", ".dart_tool",
]

# paths (relative to a root) skipped in both modes
exclude_paths = [
  ".cargo/registry", ".rustup/toolchains", ".npm/_cacache", ".pub-cache",
  "go/pkg/mod", "Android/Sdk", ".android/avd", "snap",
]

# extra directory names skipped only in normal mode
normal_mode_exclude_names = []

# extra paths skipped only in normal mode
normal_mode_exclude_paths = ["Android", "go", "flutter"]

# most entries indexed from one directory; larger directories are cut off
max_dir_entries = 4000

# build output (out, out_*), browser profiles and editor data are always skipped;
# a trailing * in a name or path component matches any suffix

[train]
# lines from one shell session arriving closer together than this are treated
# as scripted input (agent tools, pasted automation) and are not learned; 0 = off
burst_ms = 250

[update]
# update mode: "manual" or "silent"
mode = "manual"

# release channel to track: "stable", "beta", "dev", or "" (blank = track current binary channel)
channel = ""

# exact release tag to stay on, e.g. "v0.2.0" (blank = always latest)
pinned_version = ""

# how often the daemon checks for updates in silent mode
check_interval_hours = 24
"#;

impl Config {
    pub fn load_or_create(home: &str) -> Config {
        let path = PathBuf::from(home).join(".config/foresight/config.toml");
        if let Ok(text) = std::fs::read_to_string(&path) {
            match toml::from_str(&text) {
                Ok(cfg) => return cfg,
                Err(e) => log(&format!("config.toml parse error, using defaults: {e}")),
            }
        } else if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).ok();
            let _ = crate::util::atomic_write(&path, |writer| {
                use std::io::Write;
                writer.write_all(TEMPLATE.as_bytes())?;
                writer.flush()
            });
        }
        Config::default()
    }

    /// Read-only loader for update operations: never creates the config file.
    /// Missing file => defaults; malformed/unreadable existing file => error.
    pub fn load_readonly(home: &str) -> Result<Config, String> {
        let path = PathBuf::from(home).join(".config/foresight/config.toml");
        match std::fs::read_to_string(&path) {
            Ok(text) => toml::from_str(&text)
                .map_err(|e| format!("config.toml parse error: {e}")),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Config::default()),
            Err(e) => Err(format!("could not read {}: {e}", path.display())),
        }
    }

    pub fn resolved_roots(&self, home: &str) -> Vec<PathBuf> {
        self.scan
            .roots
            .iter()
            .map(|r| {
                if r == "~" {
                    PathBuf::from(home)
                } else if let Some(rest) = r.strip_prefix("~/") {
                    PathBuf::from(home).join(rest)
                } else {
                    PathBuf::from(r)
                }
            })
            .collect()
    }

    pub fn effective_excludes(&self) -> (Vec<String>, Vec<String>) {
        let deep = self.scan.mode == "deep";
        let mut names = self.scan.exclude_names.clone();
        let mut paths = self.scan.exclude_paths.clone();
        if !deep {
            names.extend(self.scan.normal_mode_exclude_names.iter().cloned());
            paths.extend(self.scan.normal_mode_exclude_paths.iter().cloned());
        }
        names.extend(strs(BUILTIN_EXCLUDE_NAMES));
        paths.extend(strs(BUILTIN_EXCLUDE_PATHS));
        (names, paths)
    }

    pub fn max_entries(&self) -> usize {
        if self.scan.mode == "deep" {
            self.scan.max_entries_deep
        } else {
            self.scan.max_entries_normal
        }
    }
}
