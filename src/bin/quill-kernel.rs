//! `quill-kernel` —— 无头会话内核 daemon (Phase 7 T2, ADR-0015 Phase 1 §4)。
//!
//! 单线程 calloop:spawn 一个 shell tab,注册其 PTY master fd(出字节 →
//! `Session::on_pty_output` 驱动 term)+ 一个 `UnixListener`;客户端连上即收当前
//! 快照(`Snapshot`)的 JSON 行。**T3a (ADR-0016)** 另加一个同步 `tungstenite` WS
//! 端点(独立线程),浏览器 / 手机经 `ws://<lan>:<port>` 连上即收一帧快照。多 tab
//! 动态增删 / 客户端输入回灌(T3c)仍是后续 ticket。
//!
//! 用法:`quill-kernel [--socket=<path>] [--ws-bind=<addr:port>]`
//! (默认 socket `$XDG_RUNTIME_DIR/quill-kernel.sock`,WS bind `0.0.0.0:7878`)。

// ADR 0001:crate 根 deny,unsafe 须 `#[allow(unsafe_code)]` + `// SAFETY:` 显式豁免。
// 本 bin 自身无 unsafe(BorrowedFd 注册在 lib 的 kernel::daemon 里),但保持一致。
#![deny(unsafe_code)]

use anyhow::Result;
use tracing_subscriber::EnvFilter;

use quill::kernel::daemon::{self, DaemonConfig};

fn main() -> Result<()> {
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("quill=debug"));
    tracing_subscriber::fmt().with_env_filter(filter).init();

    let args: Vec<String> = std::env::args().collect();
    let socket_path = match daemon::parse_socket_arg(&args) {
        Some(p) => p,
        None => daemon::default_socket_path()?,
    };

    let mut config = DaemonConfig::with_socket(socket_path);
    if let Some(addr) = daemon::parse_ws_bind_arg(&args)? {
        config.ws_bind = addr;
    }

    daemon::run(config)
}
