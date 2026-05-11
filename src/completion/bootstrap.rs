use std::collections::{HashMap, HashSet};
use std::{env, fs, io};
use std::path::{Path, PathBuf};
use std::sync::{atomic::{AtomicBool, Ordering}, mpsc, Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

use crate::completion::help_indexer::{parse_help_suggestions, run_help_command, HelpIndexerConfig, HelpRunError};
use crate::completion::{CacheKey, CompletionCache};

const DEFAULT_MAX_CONCURRENT: usize = 4;
const DEFAULT_START_DELAY: Duration = Duration::from_secs(5);
const DEFAULT_BINARY_TIMEOUT: Duration = Duration::from_secs(2);
const DEFAULT_NEGATIVE_TTL: Duration = Duration::from_secs(24 * 60 * 60);

#[derive(Clone, Debug)]
#[rustfmt::skip]
pub struct BootstrapConfig { pub max_concurrent: usize, pub start_delay: Duration, pub binary_timeout: Duration, pub allow_paths: Vec<PathBuf>, pub deny_list: Vec<String> }

pub struct Bootstrapper {
    config: BootstrapConfig,
    cache: Arc<Mutex<CompletionCache>>,
    progress: Arc<Mutex<BootstrapProgress>>,
    cancel: Arc<AtomicBool>,
    negative_cache: Arc<Mutex<HashMap<PathBuf, Instant>>>,
}

#[derive(Clone, Debug)]
#[rustfmt::skip]
pub struct BootstrapProgress { pub total: usize, pub completed: usize, pub succeeded: usize, pub failed: usize, pub current_binary: Option<String>, pub started_at: Option<Instant>, pub finished_at: Option<Instant>, pub state: BootstrapState }

#[derive(Clone, Debug, PartialEq)]
pub enum BootstrapState {
    NotStarted,
    Delayed,
    Scanning,
    Indexing,
    Completed,
    Failed(String),
}

impl Bootstrapper {
    pub fn new(config: BootstrapConfig, cache: Arc<Mutex<CompletionCache>>) -> Self {
        Self {
            config,
            cache,
            progress: Arc::new(Mutex::new(BootstrapProgress::default())),
            cancel: Arc::new(AtomicBool::new(false)),
            negative_cache: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub fn start(&self) -> Arc<Mutex<BootstrapProgress>> {
        let runner = self.clone();
        runner.cancel.store(false, Ordering::SeqCst);
        set_progress(&runner.progress, |p| {
            *p = BootstrapProgress::default();
            p.started_at = Some(Instant::now());
            p.state = BootstrapState::Delayed;
        });
        let handle = Arc::clone(&self.progress);
        let _ = thread::Builder::new()
            .name("quill-completion-bootstrap".to_string())
            .spawn(move || {
                thread::sleep(runner.config.start_delay);
                if runner.cancel.load(Ordering::SeqCst) {
                    runner.fail("cancelled");
                } else {
                    let _ = runner.run_sync_inner();
                }
            });
        handle
    }

    pub fn run_sync(&self) -> Result<BootstrapStats, BootstrapErr> {
        self.cancel.store(false, Ordering::SeqCst);
        self.run_sync_inner()
    }

    pub fn cancel(&self) { self.cancel.store(true, Ordering::SeqCst); }

    fn run_sync_inner(&self) -> Result<BootstrapStats, BootstrapErr> {
        let started_at = Instant::now();
        set_progress(&self.progress, |p| {
            *p = BootstrapProgress::default();
            p.started_at = Some(started_at);
            p.state = BootstrapState::Scanning;
        });
        let binaries = self.scan_binaries().map_err(|err| {
            self.fail(&format!("{err:?}"));
            err
        })?;
        set_progress(&self.progress, |p| {
            p.total = binaries.len();
            p.state = BootstrapState::Indexing;
        });
        let counters = Arc::new(Mutex::new(IndexCounters::default()));
        self.index_binaries(binaries, Arc::clone(&counters));
        if self.cancel.load(Ordering::SeqCst) {
            self.fail("cancelled");
            return Err(BootstrapErr::CancelledByUser);
        }
        let counters = counters.lock().map(|c| *c).unwrap_or_default();
        let stats = BootstrapStats {
            total_scanned: self.progress.lock().map(|p| p.total).unwrap_or(0),
            already_cached: counters.already_cached,
            indexed: counters.indexed,
            failed: counters.failed,
            elapsed: started_at.elapsed(),
        };
        set_progress(&self.progress, |p| {
            p.current_binary = None;
            p.finished_at = Some(Instant::now());
            p.state = BootstrapState::Completed;
        });
        tracing::info!(total_scanned = stats.total_scanned, already_cached = stats.already_cached, indexed = stats.indexed, failed = stats.failed, elapsed_ms = stats.elapsed.as_millis(), "completion bootstrap finished");
        Ok(stats)
    }

    fn scan_binaries(&self) -> Result<Vec<Binary>, BootstrapErr> {
        let mut seen = HashSet::new();
        let mut out = Vec::new();
        for dir in scan_paths(&self.config)? {
            let entries = match fs::read_dir(&dir) {
                Ok(entries) => entries,
                Err(err) if err.kind() == io::ErrorKind::PermissionDenied => {
                    return Err(BootstrapErr::PermissionDenied(dir));
                }
                Err(_) => continue,
            };
            for entry in entries.flatten() {
                let path = entry.path();
                let Ok(metadata) = entry.metadata() else { continue };
                if !metadata.is_file() || !is_executable(&metadata) {
                    continue;
                }
                let Some(name) = path.file_name().and_then(|n| n.to_str()).map(str::to_string)
                else { continue };
                let canonical = fs::canonicalize(&path).unwrap_or(path);
                if is_denied(&canonical, &self.config.deny_list) || !seen.insert(name.clone()) {
                    continue;
                }
                if let Ok(mtime) = metadata.modified() {
                    out.push(Binary { name, path: canonical, mtime });
                }
            }
        }
        out.sort_by(|a, b| a.name.cmp(&b.name).then_with(|| a.path.cmp(&b.path)));
        Ok(out)
    }

    fn index_binaries(&self, binaries: Vec<Binary>, counters: Arc<Mutex<IndexCounters>>) {
        let (tx, rx) = mpsc::channel();
        let rx = Arc::new(Mutex::new(rx));
        let mut handles = Vec::new();
        for i in 0..self.config.max_concurrent.max(1).min(binaries.len().max(1)) {
            let ctx = WorkerCtx {
                config: self.config.clone(),
                cache: Arc::clone(&self.cache),
                progress: Arc::clone(&self.progress),
                cancel: Arc::clone(&self.cancel),
                negative_cache: Arc::clone(&self.negative_cache),
                counters: Arc::clone(&counters),
            };
            let rx = Arc::clone(&rx);
            if let Ok(handle) = thread::Builder::new()
                .name(format!("quill-bootstrap-index-{i}"))
                .spawn(move || worker_loop(ctx, rx))
            {
                handles.push(handle);
            }
        }
        for binary in binaries {
            if self.cancel.load(Ordering::SeqCst) || tx.send(binary).is_err() {
                break;
            }
        }
        drop(tx);
        for handle in handles {
            let _ = handle.join();
        }
    }

    fn fail(&self, reason: &str) {
        set_progress(&self.progress, |p| {
            p.current_binary = None;
            p.finished_at = Some(Instant::now());
            p.state = BootstrapState::Failed(reason.to_string());
        });
    }
}

impl Clone for Bootstrapper {
    fn clone(&self) -> Self {
        Self { config: self.config.clone(), cache: Arc::clone(&self.cache), progress: Arc::clone(&self.progress), cancel: Arc::clone(&self.cancel), negative_cache: Arc::clone(&self.negative_cache) }
    }
}

impl Default for BootstrapConfig {
    fn default() -> Self {
        Self { max_concurrent: DEFAULT_MAX_CONCURRENT, start_delay: DEFAULT_START_DELAY, binary_timeout: DEFAULT_BINARY_TIMEOUT, allow_paths: default_path_entries(), deny_list: Vec::new() }
    }
}

impl Default for BootstrapProgress {
    fn default() -> Self {
        Self { total: 0, completed: 0, succeeded: 0, failed: 0, current_binary: None, started_at: None, finished_at: None, state: BootstrapState::NotStarted }
    }
}

#[derive(Clone, Debug)]
#[rustfmt::skip]
struct Binary { name: String, path: PathBuf, mtime: SystemTime }

#[derive(Clone, Copy, Default)]
#[rustfmt::skip]
struct IndexCounters { already_cached: usize, indexed: usize, failed: usize }

struct WorkerCtx {
    config: BootstrapConfig,
    cache: Arc<Mutex<CompletionCache>>,
    progress: Arc<Mutex<BootstrapProgress>>,
    cancel: Arc<AtomicBool>,
    negative_cache: Arc<Mutex<HashMap<PathBuf, Instant>>>,
    counters: Arc<Mutex<IndexCounters>>,
}

fn worker_loop(ctx: WorkerCtx, rx: Arc<Mutex<mpsc::Receiver<Binary>>>) {
    loop {
        if ctx.cancel.load(Ordering::SeqCst) {
            return;
        }
        let Some(binary) = rx.lock().ok().and_then(|rx| rx.recv().ok()) else {
            return;
        };
        if ctx.cancel.load(Ordering::SeqCst) {
            return;
        }
        index_one(&ctx, binary);
    }
}

fn index_one(ctx: &WorkerCtx, binary: Binary) {
    set_progress(&ctx.progress, |p| p.current_binary = Some(binary.name.clone()));
    if negative_cached(&ctx.negative_cache, &binary.path) {
        finish(ctx, false, true, false);
        return;
    }
    let key = CacheKey { binary_path: binary.path.clone(), binary_mtime: binary.mtime, query_signature: format!("help:{}", binary.name) };
    if ctx.cache.lock().ok().is_some_and(|mut cache| cache.get(&key).is_some() || cache.load_from_disk(&key).ok().flatten().is_some()) {
        finish(ctx, true, false, true);
        return;
    }
    let help_config = HelpIndexerConfig { timeout: ctx.config.binary_timeout, allow_paths: ctx.config.allow_paths.clone(), ..HelpIndexerConfig::default() };
    let suggestions = match run_help_command(&binary.path, &help_config) {
        Ok(output) => parse_help_suggestions(&output),
        Err(HelpRunError::Timeout | HelpRunError::Io(_)) => Vec::new(),
    };
    let saved = !suggestions.is_empty()
        && ctx.cache.lock().ok().is_some_and(|mut cache| {
            cache.put(key.clone(), suggestions);
            cache.save_to_disk(&key).is_ok()
        });
    if saved {
        finish(ctx, false, false, true);
    } else {
        mark_negative(&ctx.negative_cache, &binary.path);
        finish(ctx, false, true, false);
    }
}

fn finish(ctx: &WorkerCtx, cached: bool, failed: bool, succeeded: bool) {
    if let Ok(mut c) = ctx.counters.lock() {
        c.already_cached += usize::from(cached);
        c.indexed += usize::from(!cached && succeeded);
        c.failed += usize::from(failed);
    }
    set_progress(&ctx.progress, |p| {
        p.completed += 1;
        p.succeeded += usize::from(succeeded);
        p.failed += usize::from(failed);
    });
}

pub struct BootstrapStats {
    pub total_scanned: usize,
    pub already_cached: usize,
    pub indexed: usize,
    pub failed: usize,
    pub elapsed: Duration,
}

#[derive(Debug)]
pub enum BootstrapErr {
    NoPath,
    PermissionDenied(PathBuf),
    CancelledByUser,
}

fn scan_paths(config: &BootstrapConfig) -> Result<Vec<PathBuf>, BootstrapErr> {
    if !config.allow_paths.is_empty() {
        return Ok(config.allow_paths.clone());
    }
    env::var_os("PATH").map(|p| env::split_paths(&p).collect()).filter(|p: &Vec<PathBuf>| !p.is_empty()).ok_or(BootstrapErr::NoPath)
}

fn default_path_entries() -> Vec<PathBuf> {
    env::var_os("PATH")
        .map(|path| env::split_paths(&path).collect())
        .unwrap_or_default()
}

fn is_denied(binary_path: &Path, deny_list: &[String]) -> bool {
    let path = binary_path.to_string_lossy();
    let file = binary_path.file_name().map(|n| n.to_string_lossy()).unwrap_or_default();
    deny_list.iter().any(|denied| denied == path.as_ref() || denied == file.as_ref())
}

fn is_executable(metadata: &fs::Metadata) -> bool {
    #[cfg(unix)]
    {
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        metadata.is_file()
    }
}

fn mark_negative(cache: &Arc<Mutex<HashMap<PathBuf, Instant>>>, path: &Path) {
    if let Ok(mut cache) = cache.lock() {
        cache.insert(path.to_path_buf(), Instant::now());
    }
}

fn negative_cached(cache: &Arc<Mutex<HashMap<PathBuf, Instant>>>, path: &Path) -> bool {
    let Ok(mut cache) = cache.lock() else { return false };
    match cache.get(path).copied() {
        Some(stored_at) if stored_at.elapsed() < DEFAULT_NEGATIVE_TTL => true,
        Some(_) => {
            cache.remove(path);
            false
        }
        None => false,
    }
}

fn set_progress<F: FnOnce(&mut BootstrapProgress)>(progress: &Arc<Mutex<BootstrapProgress>>, f: F) {
    if let Ok(mut progress) = progress.lock() {
        f(&mut progress);
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::completion::{Suggestion, SuggestionGroup};
    use std::os::unix::fs::PermissionsExt;
    use std::time::UNIX_EPOCH;

    fn tmp(name: &str) -> PathBuf {
        let stamp = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
        env::temp_dir().join(format!("quill-bootstrap-{name}-{}-{stamp}", std::process::id()))
    }
    fn write_bin(dir: &Path, name: &str, body: &str) -> PathBuf {
        fs::create_dir_all(dir).unwrap();
        let path = dir.join(name);
        fs::write(&path, body).unwrap();
        let mut mode = fs::metadata(&path).unwrap().permissions();
        mode.set_mode(0o755);
        fs::set_permissions(&path, mode).unwrap();
        path
    }
    fn cache() -> Arc<Mutex<CompletionCache>> { Arc::new(Mutex::new(CompletionCache::new(tmp("cache"), 100))) }
    fn config(paths: Vec<PathBuf>) -> BootstrapConfig {
        BootstrapConfig { max_concurrent: 4, start_delay: Duration::ZERO, binary_timeout: Duration::from_secs(2), allow_paths: paths, deny_list: Vec::new() }
    }
    fn bootstrap(paths: Vec<PathBuf>) -> Bootstrapper { Bootstrapper::new(config(paths), cache()) }
    fn wait_for<F: FnMut() -> bool>(mut f: F) -> bool {
        let start = Instant::now();
        while start.elapsed() < Duration::from_secs(2) {
            if f() { return true; }
            thread::sleep(Duration::from_millis(5));
        }
        false
    }
    fn suggestion(text: &str) -> Suggestion {
        Suggestion { text: text.to_string(), display: text.to_string(), description: String::new(), group: SuggestionGroup::Flag }
    }
    #[test]
    fn test_scan_path_collects_executables() {
        let dir = tmp("scan");
        write_bin(&dir, "ok", "#!/bin/sh\nprintf '  --ok  ok\\n'\n");
        fs::write(dir.join("plain"), "no").unwrap();
        let names: Vec<_> = bootstrap(vec![dir.clone()]).scan_binaries().unwrap().into_iter().map(|b| b.name).collect();
        assert_eq!(names, vec!["ok"]);
        let _ = fs::remove_dir_all(dir);
    }
    #[test]
    fn test_dedup_keeps_first_in_path() {
        let (a, b) = (tmp("dedup-a"), tmp("dedup-b"));
        write_bin(&a, "dup", "#!/bin/sh\nprintf '  --a  a\\n'\n");
        write_bin(&b, "dup", "#!/bin/sh\nprintf '  --b  b\\n'\n");
        let bins = bootstrap(vec![a.clone(), b.clone()]).scan_binaries().unwrap();
        assert_eq!(bins.len(), 1);
        assert!(bins[0].path.starts_with(&a));
        let _ = fs::remove_dir_all(a);
        let _ = fs::remove_dir_all(b);
    }
    #[test]
    fn test_skip_deny_list() {
        let dir = tmp("deny");
        write_bin(&dir, "skip", "#!/bin/sh\nprintf '  --skip  skip\\n'\n");
        let mut cfg = config(vec![dir.clone()]);
        cfg.deny_list = vec!["skip".to_string()];
        assert!(Bootstrapper::new(cfg, cache()).scan_binaries().unwrap().is_empty());
        let _ = fs::remove_dir_all(dir);
    }
    #[test]
    fn test_progress_updates_during_indexing() {
        let dir = tmp("progress");
        write_bin(&dir, "slow", "#!/bin/sh\nsleep 0.2\nprintf '  --slow  slow\\n'\n");
        write_bin(&dir, "slower", "#!/bin/sh\nsleep 0.2\nprintf '  --slower  slower\\n'\n");
        let boot = Arc::new(bootstrap(vec![dir.clone()]));
        let run = Arc::clone(&boot);
        let handle = thread::spawn(move || run.run_sync().unwrap());
        assert!(wait_for(|| boot.progress.lock().unwrap().state == BootstrapState::Indexing && boot.progress.lock().unwrap().current_binary.is_some()));
        let stats = handle.join().unwrap();
        assert_eq!(stats.indexed, 2);
        assert_eq!(boot.progress.lock().unwrap().state, BootstrapState::Completed);
        let _ = fs::remove_dir_all(dir);
    }
    #[test]
    fn test_already_cached_skipped() {
        let dir = tmp("cached");
        let bin = write_bin(&dir, "cached", "#!/bin/sh\nexit 99\n");
        let cache = cache();
        let key = CacheKey { binary_path: fs::canonicalize(&bin).unwrap(), binary_mtime: fs::metadata(&bin).unwrap().modified().unwrap(), query_signature: "help:cached".to_string() };
        { let mut cache = cache.lock().unwrap(); cache.put(key.clone(), vec![suggestion("--old")]); cache.save_to_disk(&key).unwrap(); }
        let stats = Bootstrapper::new(config(vec![dir.clone()]), cache).run_sync().unwrap();
        assert_eq!((stats.already_cached, stats.indexed, stats.failed), (1, 0, 0));
        let _ = fs::remove_dir_all(dir);
    }
    #[test]
    fn test_failed_binary_marked_in_negative_cache() {
        let dir = tmp("negative");
        let bin = write_bin(&dir, "bad", "#!/bin/sh\nexit 1\n");
        let boot = bootstrap(vec![dir.clone()]);
        let stats = boot.run_sync().unwrap();
        assert_eq!(stats.failed, 1);
        assert!(boot.negative_cache.lock().unwrap().contains_key(&fs::canonicalize(&bin).unwrap()));
        let _ = fs::remove_dir_all(dir);
    }
    #[test]
    fn test_max_concurrent_respected() {
        let dir = tmp("concurrent");
        let body = format!("#!/bin/sh\nD={}\nwhile ! mkdir \"$D/lock\" 2>/dev/null; do sleep 0.01; done\na=$(cat \"$D/active\" 2>/dev/null || echo 0); a=$((a+1)); echo $a > \"$D/active\"; m=$(cat \"$D/max\" 2>/dev/null || echo 0); [ $a -gt $m ] && echo $a > \"$D/max\"; rmdir \"$D/lock\"\nsleep 0.15\nwhile ! mkdir \"$D/lock\" 2>/dev/null; do sleep 0.01; done\na=$(cat \"$D/active\"); echo $((a-1)) > \"$D/active\"; rmdir \"$D/lock\"\nprintf '  --ok  ok\\n'\n", dir.display());
        for i in 0..6 { write_bin(&dir, &format!("cmd{i}"), &body); }
        let mut cfg = config(vec![dir.clone()]);
        cfg.max_concurrent = 2;
        Bootstrapper::new(cfg, cache()).run_sync().unwrap();
        let max_seen: usize = fs::read_to_string(dir.join("max")).unwrap().trim().parse().unwrap();
        assert!(max_seen <= 2, "max_seen={max_seen}");
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn test_cancel_stops_indexing() {
        let dir = tmp("cancel");
        for i in 0..6 { write_bin(&dir, &format!("cmd{i}"), "#!/bin/sh\nsleep 0.25\nprintf '  --ok  ok\\n'\n"); }
        let boot = Arc::new(bootstrap(vec![dir.clone()]));
        let run = Arc::clone(&boot);
        let handle = thread::spawn(move || run.run_sync());
        assert!(wait_for(|| boot.progress.lock().unwrap().state == BootstrapState::Indexing));
        boot.cancel();
        assert!(matches!(handle.join().unwrap(), Err(BootstrapErr::CancelledByUser)));
        let progress = boot.progress.lock().unwrap().clone();
        assert!(progress.completed < progress.total);
        let _ = fs::remove_dir_all(dir);
    }
}
