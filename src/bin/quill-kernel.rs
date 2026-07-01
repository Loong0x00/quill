//! `quill-kernel` —— 无头会话内核 daemon (Phase 7 T2, ADR-0015 Phase 1 §4)。
//!
//! 单线程 calloop:spawn 一个 shell tab,注册其 PTY master fd(出字节 →
//! `Session::on_pty_output` 驱动 term)+ 一个 `UnixListener`(`quill-dump` 连上收
//! 当前网格快照 `Snapshot` 的 JSON 行)。**片1 (ADR-0015 R1)** 另加一个同步
//! `tungstenite` WS 子系统(独立线程)+ **同口 HTTP**:浏览器 / 手机经
//! `http://<lan>:<port>/` 拿 xterm.js 页,WS 连上先收**字节环缓冲重放**(重建当前屏)
//! 再持续收 PTY **原始字节流**(连接保持,本地渲染 + 本地 reflow)。客户端输入回灌
//! (T5)/ 多 tab 动态增删(T6)仍是后续 ticket。
//!
//! 用法:`quill-kernel [--socket=<path>] [--ws-bind=<addr:port>]`
//! (默认 socket `$XDG_RUNTIME_DIR/quill-kernel.sock`,WS bind `0.0.0.0:7878`)。
//!
//! **E′ 共享子进程模式(ADR-0018,T6 砖1a)**:`--fed-in=<fd> --fed-out=<fd>` 让本 bin 当
//! 父监管的隔离子进程跑 —— 字节从父 pipe(`--fed-in`)来、**不 spawn shell / 不开真 PTY**,
//! WS 输入回灌父 back-channel(`--fed-out`)。父桌面接入(tee)是砖1b,本砖子可经合成喂料器
//! (测试当"父")独立验。两 fd 须成对给;给了即覆盖默认 Local 拓扑。

// ADR 0001:crate 根 deny,unsafe 须 `#[allow(unsafe_code)]` + `// SAFETY:` 显式豁免。
// 本 bin 自身无 unsafe(BorrowedFd 注册在 lib 的 kernel::daemon 里),但保持一致。
#![deny(unsafe_code)]

use anyhow::Result;
use tracing_subscriber::EnvFilter;

use quill::kernel::daemon::{self, DaemonConfig, SourceConfig};

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();

    // ADR-0019 F1a:`--detach` → **最早**(尚单线程,未 init tracing / 未起线程)双 fork daemonize,
    // 脱离发起窗口(机器级单例 kernel,不随窗口退出而死、不再是窗口子进程)。见 [`daemon::daemonize`]。
    if daemon::parse_detach_arg(&args) {
        daemon::daemonize()?;
    }

    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("quill=debug"));
    tracing_subscriber::fmt().with_env_filter(filter).init();

    let socket_path = match daemon::parse_socket_arg(&args) {
        Some(p) => p,
        None => daemon::default_socket_path()?,
    };

    let mut config = DaemonConfig::with_socket(socket_path);
    if let Some(addr) = daemon::parse_ws_bind_arg(&args)? {
        config.ws_bind = addr;
    }
    // 拓扑优先级:`--rendezvous`(ADR-0019 Federated:会合 accept N feeder)> `--fed-in/--fed-out`
    // (ADR-0018 Fed:单继承 pipe,测试用)> 默认 Local(standalone spawn shell)。
    if let Some(path) = daemon::parse_rendezvous_arg(&args) {
        config.source = SourceConfig::Federated {
            rendezvous_path: path,
        };
    } else if let Some(source) = daemon::parse_fed_source(&args)? {
        config.source = source;
    }

    daemon::run(config)
}
