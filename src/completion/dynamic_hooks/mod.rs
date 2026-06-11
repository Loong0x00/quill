pub mod cd;
pub mod docker;
pub mod git_branch;
pub mod git_status;
pub mod kill_proc;
pub mod kubectl;
pub mod pacman;
pub mod readdir;
pub mod ssh;
pub mod systemctl;

use std::env;
use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::process::{ChildStdout, Command, Stdio};
use std::sync::{mpsc, Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

#[cfg(unix)]
use std::os::unix::process::CommandExt;

pub use cd::CdProvider;
pub use docker::DockerProvider;
pub use git_branch::GitBranchProvider;
pub use git_status::GitStatusProvider;
pub use kill_proc::KillProvider;
pub use kubectl::KubectlProvider;
pub use pacman::PacmanProvider;
pub use readdir::ReaddirProvider;
pub use ssh::SshProvider;
pub use systemctl::SystemctlProvider;

use crate::completion::{ProviderErr, ProviderRegistry, QueryCtx, Suggestion, SuggestionGroup};

/// 外部 hook(git / docker / kubectl …)的 TTL 缓存:`(上次结果, 取到的时刻)`。
/// 抽别名是为读得懂 + 消 clippy::type_complexity —— 同一类型在 10 个 provider
/// 的 `cache` 字段及 `cached_values` / `spawn_external_refresh` 签名里复用。
pub(crate) type ExternalCache = Arc<Mutex<Option<(Vec<String>, Instant)>>>;

const EXTERNAL_TIMEOUT: Duration = Duration::from_millis(500);
const EXTERNAL_OUTPUT_CAP: usize = 2 * 1024 * 1024;
const EXTERNAL_KILL_REAP_WAIT: Duration = Duration::from_millis(50);
const EXTERNAL_CACHE_SIGNATURE_PREFIX: &str = "\0quill-external-hook:";

pub fn register_local_hooks(registry: &mut ProviderRegistry) {
    registry.register(Arc::new(CdProvider));
    registry.register(Arc::new(SshProvider::new(default_ssh_config_path())));
    registry.register(Arc::new(ReaddirProvider::new_default()));
}

pub fn register_external_hooks(registry: &mut ProviderRegistry) {
    registry.register(Arc::new(GitBranchProvider::new()));
    registry.register(Arc::new(GitStatusProvider::new()));
    registry.register(Arc::new(KillProvider::new()));
    registry.register(Arc::new(DockerProvider::new()));
    registry.register(Arc::new(KubectlProvider::new()));
    registry.register(Arc::new(PacmanProvider::new()));
    registry.register(Arc::new(SystemctlProvider::new()));
}

pub fn default_ssh_config_path() -> PathBuf {
    env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default()
        .join(".ssh/config")
}

pub(crate) struct PathPrefix {
    pub read_dir: PathBuf,
    pub prefix: String,
    pub text_prefix: String,
}

pub(crate) fn path_prefix(working_dir: &Path, token: &str) -> PathPrefix {
    let (dir_token, prefix, text_prefix) = match token.rsplit_once('/') {
        Some((dir_part, prefix)) => {
            let dir_token = if dir_part.is_empty() && token.starts_with('/') {
                "/"
            } else if dir_part.is_empty() {
                "."
            } else {
                dir_part
            };
            let text_prefix = token[..token.len() - prefix.len()].to_string();
            (dir_token.to_string(), prefix.to_string(), text_prefix)
        }
        None => (".".to_string(), token.to_string(), String::new()),
    };

    PathPrefix {
        read_dir: expand_path(working_dir, &dir_token),
        prefix,
        text_prefix,
    }
}

pub(crate) fn file_suggestion(text: String, display: String) -> Suggestion {
    Suggestion {
        text,
        display,
        description: String::new(),
        group: SuggestionGroup::File,
    }
}

pub(crate) fn dynamic_suggestion(text: String, display: String, description: String) -> Suggestion {
    Suggestion {
        text,
        display,
        description,
        group: SuggestionGroup::Dynamic,
    }
}

pub(crate) fn read_dir(path: &Path) -> Result<Option<fs::ReadDir>, ProviderErr> {
    match fs::read_dir(path) {
        Ok(entries) => Ok(Some(entries)),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(ProviderErr::Io(err.to_string())),
    }
}

pub(crate) fn ctx_tokens(ctx: &QueryCtx) -> Vec<String> {
    let command_tokens = ctx
        .command
        .split_whitespace()
        .map(str::to_string)
        .collect::<Vec<_>>();

    if command_tokens.len() > ctx.previous_tokens.len() {
        return command_tokens;
    }

    if ctx.previous_tokens.is_empty() && !ctx.command.trim().is_empty() {
        return vec![ctx.command.trim().to_string()];
    }

    ctx.previous_tokens.clone()
}

pub(crate) fn cached_values(
    cache: &ExternalCache,
    ttl: Duration,
    now: Instant,
) -> Result<Option<Vec<String>>, ProviderErr> {
    let cache = cache
        .lock()
        .map_err(|_| ProviderErr::Io("external hook cache lock poisoned".to_string()))?;

    Ok(cache.as_ref().and_then(|(values, loaded_at)| {
        (now.duration_since(*loaded_at) < ttl).then(|| values.clone())
    }))
}

pub(crate) fn external_cache_signature(ctx: &QueryCtx, provider: &str, extra: &str) -> String {
    format!("{provider}:{}:{extra}", ctx.working_dir.display())
}

pub(crate) fn matching_cache_values(values: Vec<String>, signature: &str) -> Option<Vec<String>> {
    let expected = format!("{EXTERNAL_CACHE_SIGNATURE_PREFIX}{signature}");
    let mut values = values.into_iter();
    match values.next() {
        Some(first) if first == expected => Some(values.collect()),
        _ => None,
    }
}

pub(crate) fn spawn_external_refresh<F>(
    thread_name: &str,
    cache: ExternalCache,
    working_dir: PathBuf,
    program: &str,
    args: Vec<String>,
    cache_signature: String,
    parse: F,
) -> Result<(), ProviderErr>
where
    F: FnOnce(&str) -> Vec<String> + Send + 'static,
{
    let empty_values = values_with_signature(&cache_signature, Vec::new());
    cache
        .lock()
        .map_err(|_| ProviderErr::Io("external hook cache lock poisoned".to_string()))?
        .replace((empty_values.clone(), Instant::now()));

    let program = program.to_string();
    let thread_name = thread_name.to_string();
    thread::Builder::new()
        .name(thread_name)
        .spawn(move || {
            let values = run_external_command(&program, &args, &working_dir)
                .map(|output| values_with_signature(&cache_signature, parse(&output)))
                .unwrap_or(empty_values);
            if let Ok(mut cache) = cache.lock() {
                cache.replace((values, Instant::now()));
            }
        })
        .map(|_| ())
        .map_err(|err| ProviderErr::Io(err.to_string()))
}

fn values_with_signature(signature: &str, values: Vec<String>) -> Vec<String> {
    let mut signed = Vec::with_capacity(values.len() + 1);
    signed.push(format!("{EXTERNAL_CACHE_SIGNATURE_PREFIX}{signature}"));
    signed.extend(values);
    signed
}

fn run_external_command(
    program: &str,
    args: &[String],
    working_dir: &Path,
) -> Result<String, ProviderErr> {
    let mut command = Command::new(program);
    command
        .args(args)
        .current_dir(working_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .env("PAGER", "cat")
        .env("NO_COLOR", "1")
        .env("TERM", "dumb");
    install_external_setsid(&mut command);

    let mut child = command
        .spawn()
        .map_err(|err| ProviderErr::Io(err.to_string()))?;
    let child_pid = child.id();
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| ProviderErr::Io("external hook process stdout missing".to_string()))?;
    let output = Arc::new(Mutex::new(Vec::new()));
    let reader = spawn_external_stdout_reader(stdout, Arc::clone(&output))?;

    let (done_tx, done_rx) = mpsc::channel();
    thread::Builder::new()
        .name("quill-external-hook-wait".to_string())
        .spawn(move || {
            let _ = done_tx.send(child.wait());
        })
        .map_err(|err| ProviderErr::Io(err.to_string()))?;

    match done_rx.recv_timeout(EXTERNAL_TIMEOUT) {
        Ok(status) => {
            status.map_err(|err| ProviderErr::Io(err.to_string()))?;
            let _ = reader.join();
            let output = output
                .lock()
                .map_err(|_| ProviderErr::Io("external hook output lock poisoned".to_string()))?
                .clone();
            Ok(String::from_utf8_lossy(&output).into_owned())
        }
        Err(mpsc::RecvTimeoutError::Timeout) => {
            kill_external_process_group(child_pid);
            let _ = done_rx.recv_timeout(EXTERNAL_KILL_REAP_WAIT);
            Err(ProviderErr::Timeout)
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => Err(ProviderErr::Io(
            "external hook wait channel closed".to_string(),
        )),
    }
}

fn spawn_external_stdout_reader(
    mut stdout: ChildStdout,
    output: Arc<Mutex<Vec<u8>>>,
) -> Result<JoinHandle<()>, ProviderErr> {
    thread::Builder::new()
        .name("quill-external-hook-stdout".to_string())
        .spawn(move || {
            let mut buf = [0u8; 8192];
            loop {
                match stdout.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        if let Ok(mut output) = output.lock() {
                            append_capped_external_output(&mut output, &buf[..n]);
                        } else {
                            break;
                        }
                    }
                    Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
                    Err(_) => break,
                }
            }
        })
        .map_err(|err| ProviderErr::Io(err.to_string()))
}

fn append_capped_external_output(output: &mut Vec<u8>, chunk: &[u8]) {
    let remaining = EXTERNAL_OUTPUT_CAP.saturating_sub(output.len());
    if remaining == 0 {
        return;
    }
    output.extend_from_slice(&chunk[..remaining.min(chunk.len())]);
}

#[cfg(unix)]
fn install_external_setsid(command: &mut Command) {
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
fn install_external_setsid(_command: &mut Command) {}

#[cfg(unix)]
fn kill_external_process_group(child_pid: u32) {
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
fn kill_external_process_group(_child_pid: u32) {}

fn expand_path(working_dir: &Path, path: &str) -> PathBuf {
    if path == "~" {
        return env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| working_dir.to_path_buf());
    }

    if let Some(rest) = path.strip_prefix("~/") {
        return env::var_os("HOME")
            .map(|home| PathBuf::from(home).join(rest))
            .unwrap_or_else(|| working_dir.join(path));
    }

    let path = Path::new(path);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        working_dir.join(path)
    }
}

#[cfg(test)]
pub(crate) fn test_temp_dir(name: &str) -> PathBuf {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    env::temp_dir().join(format!(
        "quill-dynamic-hooks-{name}-{}-{stamp}",
        std::process::id()
    ))
}

#[cfg(test)]
pub(crate) fn test_query_ctx(command: &str, working_dir: &Path, current_token: &str) -> QueryCtx {
    QueryCtx {
        command: command.to_string(),
        current_token: current_token.to_string(),
        previous_tokens: vec![command.to_string()],
        working_dir: working_dir.to_path_buf(),
    }
}

#[cfg(test)]
pub(crate) fn test_texts(suggestions: &[Suggestion]) -> Vec<&str> {
    suggestions
        .iter()
        .map(|suggestion| suggestion.text.as_str())
        .collect()
}
