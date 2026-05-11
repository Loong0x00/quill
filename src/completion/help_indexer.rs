use std::collections::{HashMap, HashSet};
use std::env;
use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::process::{ChildStdout, Command, ExitStatus, Stdio};
use std::sync::{mpsc, Arc, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
#[cfg(unix)]
use std::os::unix::process::CommandExt;

use regex::Regex;

use crate::completion::parser;
use crate::completion::{
    CacheKey, CompletionCache, GenerationId, Provider, ProviderErr, QueryCtx, Suggestion,
    SuggestionGroup,
};
use crate::composer::tokenizer::{tokenize, TokenKind};

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(2);
const DEFAULT_OUTPUT_CAP: usize = 256 * 1024;
const DEFAULT_NEGATIVE_TTL: Duration = Duration::from_secs(24 * 60 * 60);
const OUTPUT_READ_CHUNK: usize = 8192;
const DONE_POLL_INTERVAL: Duration = Duration::from_millis(10);
const KILL_REAP_WAIT: Duration = Duration::from_millis(250);
const OUTPUT_DRAIN_WAIT: Duration = Duration::from_millis(100);

pub struct HelpIndexerProvider {
    cache: Arc<Mutex<CompletionCache>>,
    inflight: Arc<Mutex<HashMap<CacheKey, Instant>>>,
    negative_cache: Arc<Mutex<HashMap<PathBuf, Instant>>>,
    deny_list: Vec<String>,
    config: HelpIndexerConfig,
}

#[derive(Debug, Clone)]
pub struct HelpIndexerConfig {
    pub timeout: Duration,
    pub output_cap: usize,
    pub negative_ttl: Duration,
    pub allow_paths: Vec<PathBuf>,
}

impl HelpIndexerProvider {
    pub fn new(cache: Arc<Mutex<CompletionCache>>, config: HelpIndexerConfig) -> Self {
        Self {
            cache,
            inflight: Arc::new(Mutex::new(HashMap::new())),
            negative_cache: Arc::new(Mutex::new(HashMap::new())),
            deny_list: Vec::new(),
            config,
        }
    }

    pub fn with_deny_list(
        cache: Arc<Mutex<CompletionCache>>,
        config: HelpIndexerConfig,
        deny_list: Vec<String>,
    ) -> Self {
        Self {
            cache,
            inflight: Arc::new(Mutex::new(HashMap::new())),
            negative_cache: Arc::new(Mutex::new(HashMap::new())),
            deny_list,
            config,
        }
    }

    fn query_sync(
        &self,
        ctx: QueryCtx,
        _gen_id: GenerationId,
    ) -> Result<Vec<Suggestion>, ProviderErr> {
        let binary_path = resolve_binary_path(&ctx)?;
        if !path_is_allowed(&binary_path, &self.config.allow_paths) {
            return Err(ProviderErr::NotFound);
        }
        if is_denied(&binary_path, &self.deny_list) {
            return Err(ProviderErr::NotFound);
        }
        if self.is_negative_cached(&binary_path, Instant::now())? {
            return Err(ProviderErr::NotFound);
        }

        let metadata = fs::metadata(&binary_path)
            .map_err(|err| ProviderErr::Io(format!("metadata {}: {err}", binary_path.display())))?;
        let binary_mtime = metadata
            .modified()
            .map_err(|err| ProviderErr::Io(format!("mtime {}: {err}", binary_path.display())))?;
        let key = CacheKey {
            binary_path: binary_path.clone(),
            binary_mtime,
            query_signature: query_signature(&ctx),
        };

        if let Some(suggestions) = self.cache_get(&key)? {
            return Ok(filter_suggestions(suggestions, &ctx.current_token));
        }

        let _guard = self.mark_inflight(key.clone())?;
        match run_help_command(&binary_path, &self.config) {
            Ok(help_output) => {
                let suggestions = parse_help_suggestions(&help_output);
                if suggestions.is_empty() {
                    self.mark_negative(&binary_path)?;
                    return Err(ProviderErr::NotFound);
                }
                self.cache_put(key, suggestions.clone())?;
                Ok(filter_suggestions(suggestions, &ctx.current_token))
            }
            Err(HelpRunError::Timeout) => {
                self.mark_negative(&binary_path)?;
                Err(ProviderErr::Timeout)
            }
            Err(HelpRunError::Io(err)) => {
                self.mark_negative(&binary_path)?;
                Err(ProviderErr::Io(err.to_string()))
            }
        }
    }

    fn cache_get(&self, key: &CacheKey) -> Result<Option<Vec<Suggestion>>, ProviderErr> {
        self.cache
            .lock()
            .map_err(|_| ProviderErr::Io("completion cache lock poisoned".to_string()))
            .map(|mut cache| cache.get(key))
    }

    fn cache_put(&self, key: CacheKey, suggestions: Vec<Suggestion>) -> Result<(), ProviderErr> {
        self.cache
            .lock()
            .map_err(|_| ProviderErr::Io("completion cache lock poisoned".to_string()))
            .map(|mut cache| cache.put(key, suggestions))
    }

    fn mark_inflight(&self, key: CacheKey) -> Result<InflightGuard, ProviderErr> {
        let mut inflight = self
            .inflight
            .lock()
            .map_err(|_| ProviderErr::Io("help indexer inflight lock poisoned".to_string()))?;
        if inflight.contains_key(&key) {
            return Err(ProviderErr::Cancelled);
        }
        inflight.insert(key.clone(), Instant::now());
        Ok(InflightGuard {
            inflight: Arc::clone(&self.inflight),
            key,
        })
    }

    fn is_negative_cached(&self, binary_path: &Path, now: Instant) -> Result<bool, ProviderErr> {
        let mut negative_cache = self.negative_cache.lock().map_err(|_| {
            ProviderErr::Io("help indexer negative cache lock poisoned".to_string())
        })?;
        match negative_cache.get(binary_path).copied() {
            Some(stored_at) if now.duration_since(stored_at) < self.config.negative_ttl => Ok(true),
            Some(_) => {
                negative_cache.remove(binary_path);
                Ok(false)
            }
            None => Ok(false),
        }
    }

    fn mark_negative(&self, binary_path: &Path) -> Result<(), ProviderErr> {
        self.negative_cache
            .lock()
            .map_err(|_| ProviderErr::Io("help indexer negative cache lock poisoned".to_string()))
            .map(|mut cache| {
                cache.insert(binary_path.to_path_buf(), Instant::now());
            })
    }
}

impl Default for HelpIndexerProvider {
    fn default() -> Self {
        Self::new(
            Arc::new(Mutex::new(CompletionCache::default())),
            HelpIndexerConfig::default(),
        )
    }
}

impl Default for HelpIndexerConfig {
    fn default() -> Self {
        Self {
            timeout: DEFAULT_TIMEOUT,
            output_cap: DEFAULT_OUTPUT_CAP,
            negative_ttl: DEFAULT_NEGATIVE_TTL,
            allow_paths: default_allow_paths(),
        }
    }
}

#[async_trait::async_trait]
impl Provider for HelpIndexerProvider {
    async fn query(
        &self,
        ctx: QueryCtx,
        gen_id: GenerationId,
    ) -> Result<Vec<Suggestion>, ProviderErr> {
        self.query_sync(ctx, gen_id)
    }

    fn cancel(&self, _gen_id: GenerationId) {}

    fn name(&self) -> &str {
        "help_indexer"
    }
}

struct InflightGuard {
    inflight: Arc<Mutex<HashMap<CacheKey, Instant>>>,
    key: CacheKey,
}

impl Drop for InflightGuard {
    fn drop(&mut self) {
        if let Ok(mut inflight) = self.inflight.lock() {
            inflight.remove(&self.key);
        }
    }
}

enum HelpRunError {
    Timeout,
    Io(io::Error),
}

pub fn simple_regex_parse(help_output: &str, current_token: &str) -> Vec<Suggestion> {
    static FLAG_RE: OnceLock<Regex> = OnceLock::new();
    static SUBCOMMAND_RE: OnceLock<Regex> = OnceLock::new();

    let flag_re = FLAG_RE.get_or_init(|| {
        Regex::new(
            r"^\s*(?:(?P<short>-\w),\s*)?(?P<flag>-\w|--\w[\w-]*)(?:\s+<\w+>)?(?:\s+(?P<desc>.*))?$",
        )
        .expect("help indexer flag regex must compile")
    });
    let subcommand_re = SUBCOMMAND_RE.get_or_init(|| {
        Regex::new(r"^\s{2,}(?P<cmd>\w[\w-]*)\s{2,}(?P<desc>.*)?$")
            .expect("help indexer subcommand regex must compile")
    });

    let mut suggestions = Vec::new();
    let mut seen = HashSet::new();
    for line in help_output.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        if let Some(captures) = flag_re.captures(line) {
            let description = captures
                .name("desc")
                .map(|m| m.as_str().trim().to_string())
                .unwrap_or_default();
            if let Some(short) = captures.name("short") {
                push_suggestion(
                    &mut suggestions,
                    &mut seen,
                    short.as_str(),
                    &description,
                    SuggestionGroup::Flag,
                    current_token,
                );
            }
            if let Some(flag) = captures.name("flag") {
                push_suggestion(
                    &mut suggestions,
                    &mut seen,
                    flag.as_str(),
                    &description,
                    SuggestionGroup::Flag,
                    current_token,
                );
            }
            continue;
        }

        if let Some(captures) = subcommand_re.captures(line) {
            let cmd = captures.name("cmd").map(|m| m.as_str()).unwrap_or_default();
            let description = captures
                .name("desc")
                .map(|m| m.as_str().trim().to_string())
                .unwrap_or_default();
            push_suggestion(
                &mut suggestions,
                &mut seen,
                cmd,
                &description,
                SuggestionGroup::Subcommand,
                current_token,
            );
        }
    }
    suggestions
}

fn parse_help_suggestions(help_output: &str) -> Vec<Suggestion> {
    std::panic::catch_unwind(|| {
        let parsed = parser::parse(help_output);
        parser::to_suggestions(&parsed, "")
    })
    .unwrap_or_else(|_| simple_regex_parse(help_output, ""))
}

fn push_suggestion(
    suggestions: &mut Vec<Suggestion>,
    seen: &mut HashSet<String>,
    text: &str,
    description: &str,
    group: SuggestionGroup,
    current_token: &str,
) {
    if !current_token.is_empty() && !text.starts_with(current_token) {
        return;
    }
    if !seen.insert(text.to_string()) {
        return;
    }
    suggestions.push(Suggestion {
        text: text.to_string(),
        display: text.to_string(),
        description: description.to_string(),
        group,
    });
}

fn filter_suggestions(suggestions: Vec<Suggestion>, current_token: &str) -> Vec<Suggestion> {
    if current_token.is_empty() {
        return suggestions;
    }
    suggestions
        .into_iter()
        .filter(|suggestion| suggestion.text.starts_with(current_token))
        .collect()
}

fn resolve_binary_path(ctx: &QueryCtx) -> Result<PathBuf, ProviderErr> {
    let token = command_token(ctx).ok_or(ProviderErr::NotFound)?;
    if token.contains('/') {
        let path = PathBuf::from(&token);
        let path = if path.is_absolute() {
            path
        } else {
            ctx.working_dir.join(path)
        };
        return canonical_executable_path(&path);
    }

    let path_var = env::var_os("PATH").ok_or(ProviderErr::NotFound)?;
    for dir in env::split_paths(&path_var) {
        let candidate = dir.join(&token);
        if let Ok(path) = canonical_executable_path(&candidate) {
            return Ok(path);
        }
    }
    Err(ProviderErr::NotFound)
}

fn command_token(ctx: &QueryCtx) -> Option<String> {
    let command = ctx.command.trim();
    if !command.is_empty() {
        let tokenized = tokenize(command, command.len());
        if let Some(token) = tokenized
            .tokens
            .iter()
            .find(|token| matches!(token.kind, TokenKind::Word | TokenKind::Unterminated))
        {
            if !token.text.is_empty() {
                return Some(token.text.clone());
            }
        }
    }
    ctx.previous_tokens
        .iter()
        .find(|token| !token.is_empty())
        .cloned()
}

fn canonical_executable_path(path: &Path) -> Result<PathBuf, ProviderErr> {
    let canonical = fs::canonicalize(path).map_err(|_| ProviderErr::NotFound)?;
    let metadata = fs::metadata(&canonical).map_err(|_| ProviderErr::NotFound)?;
    if !metadata.is_file() || !is_executable(&metadata) {
        return Err(ProviderErr::NotFound);
    }
    Ok(canonical)
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

fn path_is_allowed(binary_path: &Path, allow_paths: &[PathBuf]) -> bool {
    allow_paths.iter().any(|allowed| {
        let allowed = fs::canonicalize(allowed).unwrap_or_else(|_| allowed.clone());
        binary_path.starts_with(allowed)
    })
}

fn is_denied(binary_path: &Path, deny_list: &[String]) -> bool {
    let path = binary_path.to_string_lossy();
    let file_name = binary_path
        .file_name()
        .map(|name| name.to_string_lossy())
        .unwrap_or_default();
    deny_list
        .iter()
        .any(|denied| denied == path.as_ref() || denied == file_name.as_ref())
}

fn query_signature(ctx: &QueryCtx) -> String {
    let command = command_token(ctx).unwrap_or_default();
    format!("help:{command}")
}

fn run_help_command(
    binary_path: &Path,
    config: &HelpIndexerConfig,
) -> Result<String, HelpRunError> {
    let mut command = Command::new(binary_path);
    command
        .arg("--help")
        .env_clear()
        .current_dir("/tmp")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    if let Some(path) = env::var_os("PATH") {
        command.env("PATH", path);
    }
    if let Some(home) = env::var_os("HOME") {
        command.env("HOME", home);
    }
    command
        .env("PAGER", "cat")
        .env("NO_COLOR", "1")
        .env("TERM", "dumb");

    install_setsid(&mut command);

    let mut child = command.spawn().map_err(HelpRunError::Io)?;
    let child_pid = child.id();
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| HelpRunError::Io(io::Error::new(io::ErrorKind::Other, "missing stdout")))?;

    let (output_tx, output_rx) = mpsc::channel();
    let (done_tx, done_rx) = mpsc::channel();

    spawn_stdout_reader(stdout, config.output_cap, output_tx);
    let _ = thread::Builder::new()
        .name("quill-help-indexer-wait".to_string())
        .spawn(move || {
            let _ = done_tx.send(child.wait());
        });

    collect_help_output(
        child_pid,
        config.timeout,
        output_rx,
        done_rx,
        config.output_cap,
    )
}

fn collect_help_output(
    child_pid: u32,
    timeout: Duration,
    output_rx: mpsc::Receiver<Vec<u8>>,
    done_rx: mpsc::Receiver<io::Result<ExitStatus>>,
    output_cap: usize,
) -> Result<String, HelpRunError> {
    let started_at = Instant::now();
    let mut output = Vec::with_capacity(output_cap.min(OUTPUT_READ_CHUNK));
    let mut output_closed = false;

    loop {
        drain_available_output(&output_rx, &mut output, output_cap);
        match done_rx.try_recv() {
            Ok(status) => {
                status.map_err(HelpRunError::Io)?;
                drain_output_after_done(&output_rx, &mut output, output_cap, &mut output_closed);
                return Ok(String::from_utf8_lossy(&output).into_owned());
            }
            Err(mpsc::TryRecvError::Disconnected) => {
                return Err(HelpRunError::Io(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "help process wait channel closed",
                )));
            }
            Err(mpsc::TryRecvError::Empty) => {}
        }

        if started_at.elapsed() >= timeout {
            kill_help_process_group(child_pid);
            let _ = done_rx.recv_timeout(KILL_REAP_WAIT);
            return Err(HelpRunError::Timeout);
        }

        let remaining = timeout.saturating_sub(started_at.elapsed());
        let wait_for = remaining.min(DONE_POLL_INTERVAL);
        if output_closed {
            match done_rx.recv_timeout(wait_for) {
                Ok(status) => {
                    status.map_err(HelpRunError::Io)?;
                    return Ok(String::from_utf8_lossy(&output).into_owned());
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return Err(HelpRunError::Io(io::Error::new(
                        io::ErrorKind::BrokenPipe,
                        "help process wait channel closed",
                    )));
                }
            }
        } else {
            match output_rx.recv_timeout(wait_for) {
                Ok(chunk) => append_capped(&mut output, &chunk, output_cap),
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => output_closed = true,
            }
        }
    }
}

fn spawn_stdout_reader(
    mut stdout: ChildStdout,
    output_cap: usize,
    output_tx: mpsc::Sender<Vec<u8>>,
) {
    let _ = thread::Builder::new()
        .name("quill-help-indexer-stdout".to_string())
        .spawn(move || {
            let mut sent = 0usize;
            let mut buf = [0u8; OUTPUT_READ_CHUNK];
            loop {
                match stdout.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        if sent < output_cap {
                            let take = (output_cap - sent).min(n);
                            sent += take;
                            if output_tx.send(buf[..take].to_vec()).is_err() {
                                sent = output_cap;
                            }
                        }
                    }
                    Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
                    Err(_) => break,
                }
            }
        });
}

fn drain_available_output(
    output_rx: &mpsc::Receiver<Vec<u8>>,
    output: &mut Vec<u8>,
    output_cap: usize,
) {
    while let Ok(chunk) = output_rx.try_recv() {
        append_capped(output, &chunk, output_cap);
    }
}

fn drain_output_after_done(
    output_rx: &mpsc::Receiver<Vec<u8>>,
    output: &mut Vec<u8>,
    output_cap: usize,
    output_closed: &mut bool,
) {
    let drain_started = Instant::now();
    while !*output_closed && drain_started.elapsed() < OUTPUT_DRAIN_WAIT {
        match output_rx.recv_timeout(Duration::from_millis(1)) {
            Ok(chunk) => append_capped(output, &chunk, output_cap),
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => *output_closed = true,
        }
    }
}

fn append_capped(output: &mut Vec<u8>, chunk: &[u8], output_cap: usize) {
    let remaining = output_cap.saturating_sub(output.len());
    if remaining == 0 {
        return;
    }
    let take = remaining.min(chunk.len());
    output.extend_from_slice(&chunk[..take]);
}

#[cfg(unix)]
fn install_setsid(command: &mut Command) {
    // SAFETY:pre_exec只在fork后的子进程、exec前运行。闭包只调用setsid并把
    // errno转成io::Error,不触碰Rust共享状态。
    #[allow(unsafe_code)]
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() < 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
}

#[cfg(not(unix))]
fn install_setsid(_command: &mut Command) {}

#[cfg(unix)]
fn kill_help_process_group(child_pid: u32) {
    let pid = child_pid as libc::pid_t;
    // SAFETY:killpg/kill只向内核传递pid和信号,不访问进程内存。pid来自刚spawn的
    // child id;setsid成功时它也是进程组id。fallback kill覆盖setsid失败路径。
    #[allow(unsafe_code)]
    unsafe {
        let _ = libc::killpg(pid, libc::SIGKILL);
        let _ = libc::kill(pid, libc::SIGKILL);
    }
}

#[cfg(not(unix))]
fn kill_help_process_group(_child_pid: u32) {}

fn default_allow_paths() -> Vec<PathBuf> {
    env::var_os("PATH")
        .map(|path| env::split_paths(&path).collect())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::ErrorKind;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_dir(name: &str) -> PathBuf {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        env::temp_dir().join(format!(
            "quill-help-indexer-{name}-{}-{stamp}",
            std::process::id()
        ))
    }

    fn write_executable(dir: &Path, name: &str, body: &str) -> PathBuf {
        fs::create_dir_all(dir).unwrap();
        let path = dir.join(name);
        fs::write(&path, body).unwrap();
        #[cfg(unix)]
        {
            let mut permissions = fs::metadata(&path).unwrap().permissions();
            permissions.set_mode(0o755);
            fs::set_permissions(&path, permissions).unwrap();
        }
        path
    }

    fn provider_with_config(config: HelpIndexerConfig) -> HelpIndexerProvider {
        HelpIndexerProvider::new(
            Arc::new(Mutex::new(CompletionCache::new(temp_dir("cache"), 100))),
            config,
        )
    }

    fn query_ctx(binary: &Path, current_token: &str) -> QueryCtx {
        let command = binary.to_string_lossy().to_string();
        QueryCtx {
            command: command.clone(),
            current_token: current_token.to_string(),
            previous_tokens: vec![command],
            working_dir: env::temp_dir(),
        }
    }

    fn wait_until<F>(timeout: Duration, mut predicate: F) -> bool
    where
        F: FnMut() -> bool,
    {
        let started_at = Instant::now();
        while started_at.elapsed() < timeout {
            if predicate() {
                return true;
            }
            thread::sleep(Duration::from_millis(5));
        }
        false
    }

    #[test]
    fn test_simple_regex_parse_clap_flags() {
        let help = "\
Usage: demo [OPTIONS]
  -h, --help          Print help
  -V, --version       Print version
      --color <WHEN>  Color mode
";

        let suggestions = simple_regex_parse(help, "");
        let texts: Vec<_> = suggestions
            .iter()
            .map(|suggestion| suggestion.text.as_str())
            .collect();

        assert!(texts.contains(&"-h"));
        assert!(texts.contains(&"--help"));
        assert!(texts.contains(&"-V"));
        assert!(texts.contains(&"--version"));
        assert!(texts.contains(&"--color"));
        assert!(suggestions
            .iter()
            .filter(|suggestion| suggestion.text == "--help")
            .all(|suggestion| suggestion.group == SuggestionGroup::Flag));
    }

    #[test]
    fn test_simple_regex_parse_subcommands() {
        let help = "\
Commands:
  checkout    Switch branches or restore files
  commit      Record changes
";

        let suggestions = simple_regex_parse(help, "");
        let checkout = suggestions
            .iter()
            .find(|suggestion| suggestion.text == "checkout")
            .unwrap();

        assert_eq!(checkout.group, SuggestionGroup::Subcommand);
        assert_eq!(checkout.description, "Switch branches or restore files");
        assert!(suggestions
            .iter()
            .any(|suggestion| suggestion.text == "commit"));
    }

    #[test]
    fn test_simple_regex_parse_filter_by_prefix() {
        let help = "\
  --help       Print help
  --version    Print version
  checkout     Switch branches
";

        let suggestions = simple_regex_parse(help, "--ver");

        assert_eq!(suggestions.len(), 1);
        assert_eq!(suggestions[0].text, "--version");
    }

    #[test]
    fn test_inflight_dedup_returns_cancelled() {
        let dir = temp_dir("inflight");
        let binary = write_executable(
            &dir,
            "slow-help",
            "#!/bin/sh\nsleep 0.25\nprintf '  --slow  slow flag\\n'\n",
        );
        let provider = Arc::new(provider_with_config(HelpIndexerConfig {
            timeout: Duration::from_secs(2),
            allow_paths: vec![dir.clone()],
            ..HelpIndexerConfig::default()
        }));
        let first_provider = Arc::clone(&provider);
        let first_ctx = query_ctx(&binary, "");

        let handle = thread::spawn(move || {
            futures::executor::block_on(first_provider.query(first_ctx, GenerationId(1)))
        });

        assert!(wait_until(Duration::from_secs(1), || {
            provider
                .inflight
                .lock()
                .map(|inflight| !inflight.is_empty())
                .unwrap_or(false)
        }));

        let second =
            futures::executor::block_on(provider.query(query_ctx(&binary, ""), GenerationId(2)));
        assert_eq!(second, Err(ProviderErr::Cancelled));

        let first = handle.join().unwrap().unwrap();
        assert!(first.iter().any(|suggestion| suggestion.text == "--slow"));

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn test_negative_cache_blocks_24h() {
        let dir = temp_dir("negative");
        let binary = write_executable(
            &dir,
            "negative-help",
            "#!/bin/sh\nprintf '  --fresh  fresh flag\\n'\n",
        );
        let provider = provider_with_config(HelpIndexerConfig {
            negative_ttl: Duration::from_secs(24 * 60 * 60),
            allow_paths: vec![dir.clone()],
            ..HelpIndexerConfig::default()
        });
        let canonical = fs::canonicalize(&binary).unwrap();
        provider
            .negative_cache
            .lock()
            .unwrap()
            .insert(canonical, Instant::now());

        let result =
            futures::executor::block_on(provider.query(query_ctx(&binary, ""), GenerationId(1)));

        assert_eq!(result, Err(ProviderErr::NotFound));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn test_output_cap_truncates() {
        let dir = temp_dir("output-cap");
        let first_line = "  --first  first flag\n";
        let binary = write_executable(
            &dir,
            "cap-help",
            "#!/bin/sh\nprintf '  --first  first flag\\n  --second  second flag\\n'\n",
        );
        let provider = provider_with_config(HelpIndexerConfig {
            output_cap: first_line.len(),
            allow_paths: vec![dir.clone()],
            ..HelpIndexerConfig::default()
        });

        let suggestions =
            futures::executor::block_on(provider.query(query_ctx(&binary, ""), GenerationId(1)))
                .unwrap();

        assert!(suggestions
            .iter()
            .any(|suggestion| suggestion.text == "--first"));
        assert!(!suggestions
            .iter()
            .any(|suggestion| suggestion.text == "--second"));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn test_timeout_kills_child() {
        let dir = temp_dir("timeout");
        let pid_file = dir.join("child.pid");
        let binary = write_executable(
            &dir,
            "timeout-help",
            &format!("#!/bin/sh\necho $$ > '{}'\nsleep 30\n", pid_file.display()),
        );
        let provider = provider_with_config(HelpIndexerConfig {
            timeout: Duration::from_millis(100),
            allow_paths: vec![dir.clone()],
            ..HelpIndexerConfig::default()
        });

        let result =
            futures::executor::block_on(provider.query(query_ctx(&binary, ""), GenerationId(1)));

        assert_eq!(result, Err(ProviderErr::Timeout));
        let pid = read_pid_with_retry(&pid_file).unwrap();
        assert!(
            wait_until(Duration::from_secs(1), || !process_exists(pid)),
            "timeout child process {pid} still exists"
        );
        assert!(provider
            .negative_cache
            .lock()
            .unwrap()
            .contains_key(&fs::canonicalize(&binary).unwrap()));

        let _ = fs::remove_dir_all(dir);
    }

    fn read_pid_with_retry(path: &Path) -> io::Result<i32> {
        let started_at = Instant::now();
        loop {
            match fs::read_to_string(path) {
                Ok(contents) => {
                    return contents
                        .trim()
                        .parse()
                        .map_err(|err| io::Error::new(ErrorKind::InvalidData, err));
                }
                Err(err)
                    if err.kind() == ErrorKind::NotFound
                        && started_at.elapsed() < Duration::from_secs(1) =>
                {
                    thread::sleep(Duration::from_millis(5));
                }
                Err(err) => return Err(err),
            }
        }
    }

    #[cfg(unix)]
    fn process_exists(pid: i32) -> bool {
        // SAFETY:kill(pid,0)只做存在性检查,不发送信号也不访问进程内存。
        #[allow(unsafe_code)]
        let rc = unsafe { libc::kill(pid, 0) };
        if rc == 0 {
            return true;
        }
        io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
    }

    #[cfg(not(unix))]
    fn process_exists(_pid: i32) -> bool {
        false
    }
}
