use std::collections::{HashMap, VecDeque};
use std::env;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

#[cfg(unix)]
use std::os::unix::ffi::OsStrExt;

use serde::{Deserialize, Serialize};

use crate::completion::Suggestion;

const DEFAULT_CAP: usize = 100;
const FORMAT_VERSION: u8 = 1;
const FNV_OFFSET: u64 = 0xcbf29ce484222325;
const FNV_PRIME: u64 = 0x100000001b3;

pub struct CompletionCache {
    entries: HashMap<CacheKey, CacheEntry>,
    cap: usize,
    base_dir: PathBuf,
    order: VecDeque<CacheKey>,
}

#[derive(Debug, Hash, PartialEq, Eq, Clone)]
pub struct CacheKey {
    pub binary_path: PathBuf,
    pub binary_mtime: SystemTime,
    pub query_signature: String,
}

#[derive(Debug, Clone)]
pub struct CacheEntry {
    pub suggestions: Vec<Suggestion>,
    pub stored_at: SystemTime,
}

impl CompletionCache {
    pub fn new(base_dir: PathBuf, cap: usize) -> Self {
        Self {
            entries: HashMap::new(),
            cap: if cap == 0 { DEFAULT_CAP } else { cap },
            base_dir,
            order: VecDeque::new(),
        }
    }

    pub fn get(&mut self, key: &CacheKey) -> Option<Vec<Suggestion>> {
        let suggestions = self.entries.get(key)?.suggestions.clone();
        self.touch(key);
        Some(suggestions)
    }

    pub fn put(&mut self, key: CacheKey, suggestions: Vec<Suggestion>) {
        let entry = CacheEntry {
            suggestions,
            stored_at: SystemTime::now(),
        };
        self.put_entry(key, entry);
    }

    pub fn save_to_disk(&self, key: &CacheKey) -> io::Result<()> {
        let entry = self.entries.get(key).ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotFound, "completion cache entry not found")
        })?;
        fs::create_dir_all(&self.base_dir)?;

        let disk_entry = DiskCacheEntry::from_cache_entry(key, entry);
        let bytes = serde_json::to_vec_pretty(&disk_entry)
            .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
        fs::write(self.cache_path(key), bytes)
    }

    pub fn load_from_disk(&mut self, key: &CacheKey) -> io::Result<Option<Vec<Suggestion>>> {
        let path = self.cache_path(key);
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(err) => return Err(err),
        };

        let disk_entry: DiskCacheEntry = serde_json::from_slice(&bytes)
            .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
        if disk_entry.format_version != FORMAT_VERSION || !disk_entry.key.matches(key) {
            return Ok(None);
        }

        let entry = disk_entry.into_cache_entry();
        let suggestions = entry.suggestions.clone();
        self.put_entry(key.clone(), entry);
        Ok(Some(suggestions))
    }

    fn put_entry(&mut self, key: CacheKey, entry: CacheEntry) {
        self.entries.insert(key.clone(), entry);
        self.touch(&key);
        self.evict_to_cap();
    }

    fn touch(&mut self, key: &CacheKey) {
        self.order.retain(|candidate| candidate != key);
        self.order.push_back(key.clone());
    }

    fn evict_to_cap(&mut self) {
        while self.entries.len() > self.cap {
            let Some(oldest) = self.order.pop_front() else {
                break;
            };
            self.entries.remove(&oldest);
        }
    }

    fn cache_path(&self, key: &CacheKey) -> PathBuf {
        self.base_dir
            .join(format!("{:016x}.json", stable_key_hash(key)))
    }
}

impl Default for CompletionCache {
    fn default() -> Self {
        Self::new(default_base_dir(), DEFAULT_CAP)
    }
}

#[derive(Debug, Deserialize, Serialize)]
struct DiskCacheEntry {
    format_version: u8,
    key: DiskCacheKey,
    suggestions: Vec<Suggestion>,
    stored_at_secs: u64,
    stored_at_nanos: u32,
}

impl DiskCacheEntry {
    fn from_cache_entry(key: &CacheKey, entry: &CacheEntry) -> Self {
        let (stored_at_secs, stored_at_nanos) = system_time_parts(entry.stored_at);
        Self {
            format_version: FORMAT_VERSION,
            key: DiskCacheKey::from_cache_key(key),
            suggestions: entry.suggestions.clone(),
            stored_at_secs,
            stored_at_nanos,
        }
    }

    fn into_cache_entry(self) -> CacheEntry {
        CacheEntry {
            suggestions: self.suggestions,
            stored_at: system_time_from_parts(self.stored_at_secs, self.stored_at_nanos),
        }
    }
}

#[derive(Debug, Deserialize, Serialize)]
struct DiskCacheKey {
    binary_path_hex: String,
    binary_mtime_secs: u64,
    binary_mtime_nanos: u32,
    query_signature: String,
}

impl DiskCacheKey {
    fn from_cache_key(key: &CacheKey) -> Self {
        let (binary_mtime_secs, binary_mtime_nanos) = system_time_parts(key.binary_mtime);
        Self {
            binary_path_hex: hex_encode(&path_bytes(&key.binary_path)),
            binary_mtime_secs,
            binary_mtime_nanos,
            query_signature: key.query_signature.clone(),
        }
    }

    fn matches(&self, key: &CacheKey) -> bool {
        let expected = Self::from_cache_key(key);
        self.binary_path_hex == expected.binary_path_hex
            && self.binary_mtime_secs == expected.binary_mtime_secs
            && self.binary_mtime_nanos == expected.binary_mtime_nanos
            && self.query_signature == expected.query_signature
    }
}

fn default_base_dir() -> PathBuf {
    if let Ok(cache_home) = env::var("XDG_CACHE_HOME") {
        return PathBuf::from(cache_home).join("quill/completions");
    }
    if let Ok(home) = env::var("HOME") {
        return PathBuf::from(home).join(".cache/quill/completions");
    }
    PathBuf::from(".cache/quill/completions")
}

fn stable_key_hash(key: &CacheKey) -> u64 {
    let mut hash = FNV_OFFSET;
    update_hash(&mut hash, &path_bytes(&key.binary_path));
    update_hash(&mut hash, &[0xff]);
    let (secs, nanos) = system_time_parts(key.binary_mtime);
    update_hash(&mut hash, &secs.to_le_bytes());
    update_hash(&mut hash, &nanos.to_le_bytes());
    update_hash(&mut hash, &[0xfe]);
    update_hash(&mut hash, key.query_signature.as_bytes());
    hash
}

fn update_hash(hash: &mut u64, bytes: &[u8]) {
    for byte in bytes {
        *hash ^= u64::from(*byte);
        *hash = hash.wrapping_mul(FNV_PRIME);
    }
}

fn system_time_parts(time: SystemTime) -> (u64, u32) {
    match time.duration_since(UNIX_EPOCH) {
        Ok(duration) => (duration.as_secs(), duration.subsec_nanos()),
        Err(_) => (0, 0),
    }
}

fn system_time_from_parts(secs: u64, nanos: u32) -> SystemTime {
    UNIX_EPOCH + Duration::new(secs, nanos.min(999_999_999))
}

fn path_bytes(path: &Path) -> Vec<u8> {
    #[cfg(unix)]
    {
        path.as_os_str().as_bytes().to_vec()
    }

    #[cfg(not(unix))]
    {
        path.to_string_lossy().as_bytes().to_vec()
    }
}

fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::completion::worker::{WorkItem, WorkerPool};
    use crate::completion::{GenerationId, Provider, ProviderErr, QueryCtx, SuggestionGroup};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{mpsc, Arc};
    use std::thread;

    fn key(path: &str, mtime_secs: u64, signature: &str) -> CacheKey {
        CacheKey {
            binary_path: PathBuf::from(path),
            binary_mtime: UNIX_EPOCH + Duration::from_secs(mtime_secs),
            query_signature: signature.to_string(),
        }
    }

    fn suggestion(text: &str) -> Suggestion {
        Suggestion {
            text: text.to_string(),
            display: text.to_string(),
            description: format!("{text} description"),
            group: SuggestionGroup::Flag,
        }
    }

    fn temp_cache_dir(name: &str) -> PathBuf {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        env::temp_dir().join(format!("quill-{name}-{}-{stamp}", std::process::id()))
    }

    #[test]
    fn test_cache_put_get() {
        let mut cache = CompletionCache::new(temp_cache_dir("put-get"), 100);
        let key = key("/usr/bin/git", 1, "git:");
        let suggestions = vec![suggestion("--help")];

        cache.put(key.clone(), suggestions.clone());

        assert_eq!(cache.get(&key), Some(suggestions));
    }

    #[test]
    fn test_cache_lru_evicts_oldest_when_full() {
        let mut cache = CompletionCache::new(temp_cache_dir("lru"), 2);
        let first = key("/bin/a", 1, "a");
        let second = key("/bin/b", 1, "b");
        let third = key("/bin/c", 1, "c");

        cache.put(first.clone(), vec![suggestion("a")]);
        cache.put(second.clone(), vec![suggestion("b")]);
        assert!(cache.get(&first).is_some());
        cache.put(third.clone(), vec![suggestion("c")]);

        assert!(cache.get(&first).is_some());
        assert!(cache.get(&second).is_none());
        assert!(cache.get(&third).is_some());
    }

    #[test]
    fn test_cache_ignores_entry_age() {
        let mut cache = CompletionCache::new(temp_cache_dir("mtime-only"), 100);
        let key = key("/usr/bin/git", 1, "old-but-valid");
        let suggestions = vec![suggestion("--still-valid")];
        cache.put_entry(
            key.clone(),
            CacheEntry {
                suggestions: suggestions.clone(),
                stored_at: SystemTime::now() - Duration::from_secs(10 * 24 * 60 * 60),
            },
        );

        assert_eq!(cache.get(&key), Some(suggestions));
    }

    #[test]
    fn test_cache_save_load_disk() {
        let dir = temp_cache_dir("disk");
        let key = key("/usr/bin/git", 1, "disk");
        let suggestions = vec![suggestion("checkout"), suggestion("commit")];
        let mut writer = CompletionCache::new(dir.clone(), 100);

        writer.put(key.clone(), suggestions.clone());
        writer.save_to_disk(&key).unwrap();

        let mut reader = CompletionCache::new(dir.clone(), 100);
        assert_eq!(reader.load_from_disk(&key).unwrap(), Some(suggestions));

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn test_cache_key_binary_mtime_change_invalidates() {
        let dir = temp_cache_dir("mtime");
        let old_key = key("/usr/bin/git", 1, "same-query");
        let new_key = key("/usr/bin/git", 2, "same-query");
        let mut writer = CompletionCache::new(dir.clone(), 100);

        writer.put(old_key.clone(), vec![suggestion("old")]);
        writer.save_to_disk(&old_key).unwrap();

        let mut reader = CompletionCache::new(dir.clone(), 100);
        assert!(reader.load_from_disk(&new_key).unwrap().is_none());

        let _ = fs::remove_dir_all(dir);
    }

    struct TestProvider {
        text: &'static str,
        delay: Duration,
        cancelled: Arc<AtomicBool>,
    }

    #[async_trait::async_trait]
    impl Provider for TestProvider {
        async fn query(
            &self,
            _ctx: QueryCtx,
            _gen_id: GenerationId,
        ) -> Result<Vec<Suggestion>, ProviderErr> {
            if !self.delay.is_zero() {
                thread::sleep(self.delay);
            }
            Ok(vec![suggestion(self.text)])
        }

        fn cancel(&self, _gen_id: GenerationId) {
            self.cancelled.store(true, Ordering::SeqCst);
        }

        fn name(&self) -> &'static str {
            "test"
        }
    }

    fn query_ctx() -> QueryCtx {
        QueryCtx {
            command: "git".to_string(),
            current_token: "ch".to_string(),
            previous_tokens: vec!["git".to_string()],
            working_dir: PathBuf::from("/tmp"),
        }
    }

    #[test]
    fn test_worker_pool_submit_returns_result() {
        let pool = WorkerPool::new(1);
        let (sender, receiver) = mpsc::channel();
        let cancelled = Arc::new(AtomicBool::new(false));

        pool.submit(WorkItem {
            provider: Arc::new(TestProvider {
                text: "checkout",
                delay: Duration::ZERO,
                cancelled,
            }),
            ctx: query_ctx(),
            gen_id: GenerationId(1),
            result_sender: sender,
        });

        let (gen_id, result, provider) = receiver.recv_timeout(Duration::from_secs(1)).unwrap();
        assert_eq!(gen_id, GenerationId(1));
        assert_eq!(provider, "test");
        assert_eq!(result, vec![suggestion("checkout")]);
    }

    #[test]
    fn test_worker_pool_cancel_drops_result() {
        let pool = WorkerPool::new(1);
        let (sender, receiver) = mpsc::channel();
        let cancelled = Arc::new(AtomicBool::new(false));

        pool.submit(WorkItem {
            provider: Arc::new(TestProvider {
                text: "commit",
                delay: Duration::from_millis(100),
                cancelled: Arc::clone(&cancelled),
            }),
            ctx: query_ctx(),
            gen_id: GenerationId(2),
            result_sender: sender,
        });
        pool.cancel(GenerationId(2));

        assert!(cancelled.load(Ordering::SeqCst));
        assert!(receiver.recv_timeout(Duration::from_millis(300)).is_err());
    }
}
