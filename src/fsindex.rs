use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{self, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;
use parking_lot::Mutex;

use inotify::{EventMask, Inotify, WatchDescriptor, WatchMask};

use crate::config::Config;
use crate::util::{atomic_write, data_dir, fnv1a64, log};

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


    #[cfg(any(test, feature = "bench"))]
    pub fn contains(&self, abs: &str) -> bool {
        self.paths.binary_search_by(|(p, _)| p.as_str().cmp(abs)).is_ok()
    }

    #[cfg(test)]
    fn remove(&mut self, abs: &str) {
        self.apply_batch(vec![], vec![abs.to_string()]);
    }

    pub fn apply_batch(&mut self, mut insert: Vec<(String, bool)>, mut remove: Vec<String>) {
        if insert.is_empty() && remove.is_empty() { return; }
        self.usage.resize(self.paths.len(), 0.0);
        remove.sort();
        remove.dedup();
        insert.retain(|(path, _)| !Path::new(path).ancestors().any(|ancestor| {
            ancestor.to_str().is_some_and(|p| remove.binary_search_by(|r| r.as_str().cmp(p)).is_ok())
        }));
        insert.sort();
        insert.dedup_by(|a, b| a.0 == b.0);
        let ranges = Self::removal_ranges(&self.paths, &remove);
        let capacity = self.paths.len() + insert.len();
        let old = std::mem::take(&mut self.paths);
        let scores = std::mem::take(&mut self.usage);
        let mut range = 0;
        let mut old = old.into_iter().zip(scores).enumerate().filter_map(|(i, entry)| {
            while range < ranges.len() && ranges[range].1 <= i { range += 1; }
            if range < ranges.len() && i >= ranges[range].0 { None } else { Some(entry) }
        }).peekable();
        let mut insert = insert.into_iter().peekable();
        let mut paths = Vec::with_capacity(capacity);
        let mut usage = Vec::with_capacity(capacity);
        while old.peek().is_some() || insert.peek().is_some() {
            let order = match (old.peek(), insert.peek()) {
                (Some((a, _)), Some(b)) => a.0.cmp(&b.0),
                (Some(_), None) => std::cmp::Ordering::Less,
                (None, _) => std::cmp::Ordering::Greater,
            };
            match order {
                std::cmp::Ordering::Less => {
                    let (path, score) = old.next().unwrap();
                    paths.push(path);
                    usage.push(score);
                }
                std::cmp::Ordering::Equal => {
                    let (_, score) = old.next().unwrap();
                    paths.push(insert.next().unwrap());
                    usage.push(score);
                }
                std::cmp::Ordering::Greater => {
                    paths.push(insert.next().unwrap());
                    usage.push(0.0);
                }
            }
        }
        self.paths = paths;
        self.usage = usage;
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
            let common = common_prefix_len(prev, p).min(u16::MAX as usize);
            let mut common = common;
            while !p.is_char_boundary(common) {
                common -= 1;
            }
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
        atomic_write(path, |w| {
            w.write_all(MAGIC)?;
            w.write_all(&body)?;
            w.write_all(&checksum.to_le_bytes())
        })
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

            if common > prev.len() || !prev.is_char_boundary(common) {
                return Err(io::Error::new(io::ErrorKind::InvalidData, "fsindex.bin bad front-coding offset"));
            }
            let mut s = String::with_capacity(common + suffix_len);
            s.push_str(&prev[..common]);
            s.push_str(std::str::from_utf8(&suffix).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?);
            paths.push((s.clone(), is_dir_buf[0] != 0));
            prev = s;
        }

        Ok(FsIndex { paths, usage: Vec::new(), usage_version: 0 })
    }
}

fn common_prefix_len(a: &str, b: &str) -> usize {
    let mut n = a.bytes().zip(b.bytes()).take_while(|(x, y)| x == y).count();
    while !a.is_char_boundary(n) {
        n -= 1;
    }
    n
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

/// Owns the only writer of live filesystem events. Drop before replacing the
/// index/configuration; joining never takes place under the index mutex.
pub struct Watcher {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Watcher {
    pub fn start(index: Arc<Mutex<FsIndex>>, config: Config, home: &str) -> io::Result<Self> {
        let stop = Arc::new(AtomicBool::new(false));
        let mut live = LiveIndex::new(config, home, stop.clone())?;
        let fresh = live.rebuild()?;
        *index.lock() = fresh;
        let thread = thread::spawn(move || {
            let mut buffer = [0u8; 65536];
            while !live.stop.load(Ordering::Acquire) {
                match live.inotify.read_events(&mut buffer) {
                    Ok(events) => {
                        let mut overflow = false;
                        let mut changes = Vec::new();
                        for event in events {
                            if event.mask.contains(EventMask::Q_OVERFLOW) {
                                overflow = true;
                            }
                            if event.mask.contains(EventMask::IGNORED) {
                                live.watches.remove(&event.wd);
                                continue;
                            }
                            if let Some(parent) = live.watches.get(&event.wd) {
                                let path = event.name.map_or_else(|| parent.clone(), |name| parent.join(name));
                                changes.push((path, event.name.is_none() || event.mask.contains(EventMask::ISDIR)));
                            }
                        }
                        if overflow {
                            match live.rebuild() {
                                Ok(fresh) => *index.lock() = fresh,
                                Err(e) => log(&format!("fs index overflow resync failed: {e}")),
                            }
                        } else {
                            live.reconcile_batch(&index, changes);
                        }
                    }
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(20));
                    }
                    Err(e) => {
                        log(&format!("inotify read failed: {e}"));
                        match live.rebuild() {
                            Ok(fresh) => *index.lock() = fresh,
                            Err(e) => {
                                log(&format!("fs index resync failed: {e}"));
                                thread::sleep(Duration::from_secs(1));
                            }
                        }
                    }
                }
            }
        });
        Ok(Self { stop, thread: Some(thread) })
    }
}

impl Drop for Watcher {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

struct LiveIndex {
    inotify: Inotify,
    watches: HashMap<WatchDescriptor, PathBuf>,
    roots: Vec<PathBuf>,
    names: Vec<String>,
    excludes: Vec<String>,
    own_data: PathBuf,
    cap: usize,
    stop: Arc<AtomicBool>,
}

impl LiveIndex {
    fn new(config: Config, home: &str, stop: Arc<AtomicBool>) -> io::Result<Self> {
        let (names, excludes) = config.effective_excludes();
        Ok(Self {
            inotify: Inotify::init()?, watches: HashMap::new(),
            roots: config.resolved_roots(home), names, excludes,
            own_data: data_dir(), cap: config.max_entries(), stop,
        })
    }

    fn allowed(&self, path: &Path) -> bool {
        if path.starts_with(&self.own_data) {
            return false;
        }
        self.roots.iter().any(|root| {
            let Ok(rel) = path.strip_prefix(root) else { return false };
            !rel.components().any(|c| self.names.iter().any(|n| c.as_os_str() == std::ffi::OsStr::new(n)))
                && !self.excludes.iter().any(|p| rel.starts_with(p))
        })
    }

    fn add_watch(&mut self, path: &Path) {
        let mask = WatchMask::CREATE | WatchMask::DELETE | WatchMask::MOVED_FROM
            | WatchMask::MOVED_TO | WatchMask::DELETE_SELF | WatchMask::MOVE_SELF;
        match self.inotify.watches().add(path, mask) {
            Ok(wd) => { self.watches.insert(wd, path.to_path_buf()); }
            Err(e) => log(&format!("cannot watch {}: {e}", path.display())),
        }
    }

    fn scan(&mut self, path: &Path, include_self: bool, budget: usize) -> Vec<(String, bool)> {
        let mut paths = Vec::new();
        // Keep only one lazy directory iterator per depth, not one allocated
        // PathBuf per child. A small cap bounds work even in enormous directories.
        let mut stack: Vec<fs::ReadDir> = Vec::new();
        let mut next = Some((path.to_path_buf(), include_self));
        while let Some((path, include)) = next.take() {
            if self.stop.load(Ordering::Acquire) || paths.len() >= budget { break; }
            if self.allowed(&path) {
                if let Ok(meta) = fs::symlink_metadata(&path) {
                    let is_dir = if meta.is_symlink() { path.is_dir() } else { meta.is_dir() };
                    if include { paths.push((path.to_string_lossy().into_owned(), is_dir)); }
                    if is_dir && !meta.is_symlink() {
                        // Watch before enumeration so concurrent changes remain queued.
                        self.add_watch(&path);
                        if let Ok(entries) = fs::read_dir(&path) { stack.push(entries); }
                    }
                }
            }
            while let Some(entries) = stack.last_mut() {
                match entries.next() {
                    Some(Ok(entry)) => { next = Some((entry.path(), true)); break; }
                    Some(Err(_)) => {}
                    None => { stack.pop(); }
                }
                if self.stop.load(Ordering::Acquire) { break; }
            }
        }
        paths
    }

    fn rebuild(&mut self) -> io::Result<FsIndex> {
        self.inotify = Inotify::init()?;
        self.watches.clear();
        let mut fresh = FsIndex::empty();
        for root in self.roots.clone() {
            // A parent watch notices deletion/recreation or movement of the root.
            if let Some(parent) = root.parent() { self.add_watch(parent); }
            let entries = self.scan(&root, false, self.cap.saturating_sub(fresh.paths.len()));
            fresh.apply_batch(entries, vec![]);
        }
        Ok(fresh)
    }

    fn reconcile_batch(&mut self, index: &Mutex<FsIndex>, changes: Vec<(PathBuf, bool)>) {
        let mut files = std::collections::BTreeSet::new();
        for (path, directory) in changes {
            if self.stop.load(Ordering::Acquire) { return; }
            if !self.allowed(&path) { continue; }
            if directory {
                self.reconcile(index, &path);
            } else {
                files.insert(path);
            }
        }
        let mut insert = Vec::new();
        let mut remove = Vec::new();
        for path in files {
            let text = path.to_string_lossy().into_owned();
            match fs::symlink_metadata(&path) {
                Ok(meta) => insert.push((text, if meta.is_symlink() { path.is_dir() } else { meta.is_dir() })),
                Err(e) if e.kind() == io::ErrorKind::NotFound => remove.push(text),
                Err(e) => log(&format!("cannot reconcile {}: {e}", path.display())),
            }
        }
        if !insert.is_empty() || !remove.is_empty() {
            // One merge for the entire kernel batch. Metadata reflects the final
            // filesystem state, so delete/recreate and recreate/delete both work.
            let mut idx = index.lock();
            idx.apply_batch(insert, remove);
            idx.paths.truncate(self.cap);
            idx.usage.truncate(self.cap);
            idx.usage_version = u64::MAX;
        }
    }

    fn reconcile(&mut self, index: &Mutex<FsIndex>, path: &Path) {
        if !self.allowed(path) { return; }
        let obsolete: Vec<_> = self.watches.iter()
            .filter(|(_, p)| p.starts_with(path)).map(|(wd, _)| wd.clone()).collect();
        for wd in obsolete {
            self.watches.remove(&wd);
            let _ = self.inotify.watches().remove(wd);
        }
        let budget = {
            let mut idx = index.lock();
            idx.apply_batch(vec![], vec![path.to_string_lossy().into_owned()]);
            self.cap.saturating_sub(idx.paths.len())
        };
        let include = !self.roots.iter().any(|root| root == path);
        let entries = self.scan(path, include, budget);
        let mut idx = index.lock();
        idx.apply_batch(entries, vec![]);
        // New entries must receive current model usage on the next prediction.
        idx.usage_version = u64::MAX;
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

    struct TempRoot(PathBuf);
    impl TempRoot {
        fn new() -> Self {
            static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!("foresight-watch-{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
            fs::create_dir(&path).unwrap();
            Self(path)
        }
        fn config(&self, cap: usize) -> Config {
            let mut config = Config::default();
            config.scan.roots = vec![self.0.to_string_lossy().into_owned()];
            config.scan.max_entries_normal = cap;
            config
        }
    }
    impl Drop for TempRoot {
        fn drop(&mut self) { let _ = fs::remove_dir_all(&self.0); }
    }


    fn eventually(index: &Mutex<FsIndex>, predicate: impl Fn(&FsIndex) -> bool) {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            if predicate(&index.lock()) { return; }
            assert!(std::time::Instant::now() < deadline, "watcher did not converge: {:?}", index.lock().paths);
            thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn inotify_tracks_descendants_after_populated_subtree_rename() {
        let root = TempRoot::new();
        let old = root.0.join("old");
        let new = root.0.join("new");
        fs::create_dir_all(old.join("sub")).unwrap();
        fs::write(old.join("sub/child"), b"").unwrap();
        let index = Arc::new(Mutex::new(FsIndex::empty()));
        let watcher = Watcher::start(index.clone(), root.config(100), "").unwrap();
        fs::rename(&old, &new).unwrap();
        eventually(&index, |idx| idx.contains(new.join("sub/child").to_str().unwrap())
            && !idx.contains(old.join("sub/child").to_str().unwrap()));
        // This event originates on a descendant watch. Stale descriptors would
        // publish it under old/sub instead of the moved directory.
        fs::write(new.join("sub/later"), b"").unwrap();
        eventually(&index, |idx| idx.contains(new.join("sub/later").to_str().unwrap()));
        fs::remove_file(new.join("sub/child")).unwrap();
        fs::write(new.join("sub/child"), b"recreated").unwrap();
        fs::remove_file(new.join("sub/later")).unwrap();
        fs::write(new.join("sub/marker"), b"").unwrap();
        eventually(&index, |idx| idx.contains(new.join("sub/marker").to_str().unwrap())
            && idx.contains(new.join("sub/child").to_str().unwrap())
            && !idx.contains(new.join("sub/later").to_str().unwrap()));
        drop(watcher);
        assert!(!index.lock().paths.iter().any(|(p, _)| Path::new(p).starts_with(&old)));
    }

    #[test]
    fn inotify_file_bursts_respect_cap_and_reconfigured_exclusions() {
        let root = TempRoot::new();
        let index = Arc::new(Mutex::new(FsIndex::empty()));
        let watcher = Watcher::start(index.clone(), root.config(10), "").unwrap();
        for n in 0..128 { fs::write(root.0.join(format!("file-{n:03}")), b"").unwrap(); }
        eventually(&index, |idx| idx.paths.len() == 10);
        drop(watcher);
        assert_eq!(index.lock().paths.len(), 10);
        let mut config = root.config(10);
        config.scan.exclude_names.push("blocked".into());
        for n in 0..128 { fs::remove_file(root.0.join(format!("file-{n:03}"))).unwrap(); }
        let watcher = Watcher::start(index.clone(), config, "").unwrap();
        fs::create_dir(root.0.join("blocked")).unwrap();
        fs::write(root.0.join("blocked/hidden"), b"").unwrap();
        fs::write(root.0.join("allowed"), b"").unwrap();
        eventually(&index, |idx| idx.contains(root.0.join("allowed").to_str().unwrap()));
        drop(watcher);
        assert_eq!(index.lock().paths, vec![(root.0.join("allowed").to_string_lossy().into_owned(), false)]);
    }
    #[test]
    fn ordered_delete_recreate_and_subtree_move_reconcile_final_state() {
        let root = TempRoot::new();
        let old = root.0.join("old");
        fs::create_dir(&old).unwrap();
        fs::write(old.join("child"), b"").unwrap();
        let mut live = LiveIndex::new(root.config(100), "", Arc::new(AtomicBool::new(false))).unwrap();
        let index = Mutex::new(live.rebuild().unwrap());
        fs::remove_file(old.join("child")).unwrap();
        fs::write(old.join("child"), b"recreated").unwrap();
        live.reconcile_batch(&index, vec![(old.join("child"), false), (old.join("child"), false)]);
        assert!(index.lock().contains(old.join("child").to_str().unwrap()));
        let new = root.0.join("new");
        fs::rename(&old, &new).unwrap();
        live.reconcile(&index, &old);
        live.reconcile(&index, &new);
        assert!(!index.lock().contains(old.join("child").to_str().unwrap()));
        assert!(index.lock().contains(new.join("child").to_str().unwrap()));
        fs::write(new.join("later"), b"").unwrap();
        live.reconcile_batch(&index, vec![(new.join("later"), false)]);
        assert!(index.lock().contains(new.join("later").to_str().unwrap()));
    }

    #[test]
    fn live_cap_exclusions_and_resynchronization() {
        let root = TempRoot::new();
        let mut config = root.config(2);
        config.scan.exclude_names.push("skip".into());
        let mut live = LiveIndex::new(config, "", Arc::new(AtomicBool::new(false))).unwrap();
        let index = Mutex::new(live.rebuild().unwrap());
        for name in ["a", "b", "c", "skip"] {
            let path = root.0.join(name);
            fs::write(&path, b"").unwrap();
            live.reconcile_batch(&index, vec![(path, false)]);
        }
        assert_eq!(index.lock().paths.len(), 2);
        assert!(!index.lock().contains(root.0.join("skip").to_str().unwrap()));
        fs::remove_file(root.0.join("a")).unwrap();
        fs::remove_file(root.0.join("b")).unwrap();
        *index.lock() = live.rebuild().unwrap();
        assert_eq!(index.lock().paths, vec![(root.0.join("c").to_string_lossy().into_owned(), false)]);
    }

    #[test]
    fn stopped_watcher_cannot_modify_reconfigured_index() {
        let old = TempRoot::new();
        let new = TempRoot::new();
        let index = Arc::new(Mutex::new(FsIndex::empty()));
        let watcher = Watcher::start(index.clone(), old.config(10), "").unwrap();
        drop(watcher);
        fs::write(old.0.join("obsolete"), b"").unwrap();
        fs::write(new.0.join("current"), b"").unwrap();
        let watcher = Watcher::start(index.clone(), new.config(10), "").unwrap();
        drop(watcher);
        assert_eq!(index.lock().paths, vec![(new.0.join("current").to_string_lossy().into_owned(), false)]);
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
    fn unicode_front_coding_round_trip() {
        let index = idx(&[("/h/è", false), ("/h/é", false), ("/h/😀/é", true)]);
        let tmp = std::env::temp_dir().join(format!("bp-fsindex-unicode-{}.bin", std::process::id()));
        index.save(&tmp).unwrap();
        assert_eq!(FsIndex::load(&tmp).unwrap().paths, index.paths);
        std::fs::remove_file(tmp).unwrap();
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
