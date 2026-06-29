//! 无头会话内核 daemon 的 calloop 接线 (Phase 7 T2, ADR-0015 Phase 1 §4)。
//!
//! T1 ([`crate::kernel::session`]) 给了纯数据层 [`Session`];这里把它挂到一个
//! **单线程** `calloop::EventLoop` 上,同一 loop 注册:
//! - 一个 shell tab 的 PTY master fd (出字节 → [`Session::on_pty_output`] 驱动 term);
//! - 一个 `UnixListener` —— 客户端连上即收当前 [`crate::kernel::proto::Snapshot`]
//!   的 JSON 行 (line-delimited),`quill-dump` 调试用;
//! - `SIGINT` / `SIGTERM` 信号源 (signalfd) → 停 loop → 退出时清理 socket 文件。
//!
//! **片1 (ADR-0015 R1 / T3c'+T4)**:WS 子系统从 T3a 的「连上发一帧网格快照即 close」
//! 改成**字节流直播 + 客户端本地渲染**:
//! - **载荷 = PTY 原始字节流**(不再是网格 `Snapshot`):`pty_readable` 读到的裸字节
//!   既继续喂 [`Session::on_pty_output`](维持服务端 term,供 unix-dump 取网格快照),
//!   又经 `std::sync::mpsc` 把 **owned `Vec<u8>`** 推给 WS 线程。
//! - WS 侧维护一个**有界字节环缓冲**([`ByteRing`])+ **已连客户端集合**(同一把锁,
//!   保证「重放 + 订阅」原子,无丢无重):新客户端连上先收环缓冲重放(重建当前屏),
//!   之后跟 live 字节流(连接保持)。**字节流非幂等,绝不丢/合帧**:慢客户端出站
//!   队列满即**断开**(它重连从环缓冲重放),不丢字节、不堆爆内存、不拖累别人。
//! - **同口 HTTP**:同一 7878 端口上,普通 `GET /` 返回内嵌的 xterm.js 页(`/vendor/*`
//!   服务 vendored xterm.js / css / fit addon),带 `Upgrade: websocket` 的才走 WS
//!   握手(`TcpStream::peek` 非破坏分流)。
//! - 连接数被 [`MAX_WS_CONNS`] 封顶(收口 T3a「每连接无上限起线程」遗留)。
//!
//! **跨线程边界只过 owned 字节**(`Rc` 非 Send 的 [`Session`] 全程钉在 calloop 线程,
//! 只有 `Vec<u8>` / [`Bytes`] 过线程边界 = ADR-0015 头号约束的结构性遵守)。
//!
//! **仍留后续 ticket**:客户端 [`crate::kernel::proto::ClientMsg`] 回灌(T5,输入 /
//! resize / tab 操作,需 `calloop::channel` 反向唤醒)、多 tab 动态增删 + per-tab 字节
//! 流封帧(T6)。

use std::cell::RefCell;
use std::collections::VecDeque;
use std::io::{self, Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::os::fd::BorrowedFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, SyncSender, TrySendError};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use calloop::generic::Generic;
use calloop::signals::{Signal, Signals};
use calloop::{EventLoop, Interest, LoopHandle, LoopSignal, Mode, PostAction};
use tungstenite::{Bytes, Message};

use crate::kernel::proto::Snapshot;
use crate::kernel::session::Session;
use crate::tab::{TabInstance, TabList};

/// 内嵌的浏览器镜像页与 vendored xterm.js 资产(`include_str!` 路径相对**本源文件**
/// `src/kernel/daemon.rs`)。同口 HTTP serve 用 —— 编译期嵌进二进制,运行时零文件
/// 系统依赖(开箱即用 + 手机经 WireGuard VPN 离线可达,不靠 CDN)。
const INDEX_HTML: &str = include_str!("../../assets/web/index.html");
const XTERM_JS: &str = include_str!("../../assets/web/vendor/xterm.js");
const XTERM_CSS: &str = include_str!("../../assets/web/vendor/xterm.css");
const XTERM_FIT_JS: &str = include_str!("../../assets/web/vendor/xterm-addon-fit.js");

/// 默认 grid 尺寸,与 `wl::run_window` 启动期 + `run_headless_screenshot` 一致
/// (一个 PTY 一个尺寸,ADR-0015 "主控端定尺寸";多客户端尺寸协商留 T5)。
pub const DEFAULT_COLS: u16 = 80;
/// 见 [`DEFAULT_COLS`]。
pub const DEFAULT_ROWS: u16 = 24;

/// 默认 socket 文件名 (挂在 `$XDG_RUNTIME_DIR` 下)。
pub const DEFAULT_SOCKET_NAME: &str = "quill-kernel.sock";

/// PTY 单次 read 缓冲。与 `wl::window` 的 `PTY_READ_BUF` 同值。
const PTY_READ_BUF: usize = 4096;

/// WS 监听默认端口。
pub const DEFAULT_WS_PORT: u16 = 7878;

/// WS acceptor 轮询关停的 sleep 间隔(非阻塞 accept WouldBlock 时)。
const WS_ACCEPT_POLL: Duration = Duration::from_millis(50);

/// 单连接握手 + 写的读/写超时,防静默/弱网客户端把连接线程挂死。
const WS_CONN_READ_TIMEOUT: Duration = Duration::from_secs(10);

/// 同时在跑的 per-conn 线程上限(HTTP + WS 合计)。收口 T3a 遗留「每连接无上限
/// 起线程」:超限直接 close 新连接(daily-drive 单用户,个位数客户端,16 富余)。
const MAX_WS_CONNS: usize = 16;

/// 每个 WS 客户端的出站有界队列深度(单位:字节批,一批 = 一次 PTY readiness 排空)。
///
/// **字节流非幂等**:不能像网格全量快照那样「队满丢旧帧」(丢任一字节 = VT 状态机
/// 错位/乱码)。故队满时**断开该客户端**(见 [`fan_out`]),它重连从环缓冲重放即可
/// 恢复当前屏。深度给 64:正常客户端排空快、队列常空;只有真跟不上的慢客户端(弱网
/// 卡死)才会撑满被断,不无界堆内存、不拖累别的客户端。
const WS_CLIENT_QUEUE: usize = 64;

/// 每个 PTY 字节流环缓冲容量。新客户端连上重放此环重建当前屏 + 一段近期 scrollback。
///
/// 256 KiB:远超「重建当前屏」所需(80×24 文本 ~2KB,带 VT 转义放大也就几十 KB),
/// 留足近期上下文;丢的永远是**最旧**字节,故最近输出(当前可见屏)恒完整。单 tab
/// 一份,daily-drive tab 数个位数,内存预算可忽略(对照 term scrollback 100K 行)。
const BYTE_RING_CAP: usize = 256 * 1024;

/// writer 线程空闲时的 ping 间隔 = 探活 + 周期醒来查关停。无新字节时发 WS Ping,
/// 对端已断则 send 报错 → writer 退出 + 注册表剪除(否则空闲会话上的死连接会泄漏)。
const WS_PING_INTERVAL: Duration = Duration::from_secs(10);

/// 同口分流时 peek HTTP 请求头的上限(防超长头吃内存;正常请求头远小于此)。
const HTTP_PEEK_MAX: usize = 8192;

/// daemon 启动参数。
pub struct DaemonConfig {
    /// `UnixListener` 绑定路径。
    pub socket_path: PathBuf,
    /// WS (tungstenite) TCP 监听地址。默认 `0.0.0.0:7878` —— LAN 可达,手机经
    /// WireGuard VPN → 路由器 → `10.0.0.2:port` 连上;安全靠 VPN 把门 (ADR-0016)。
    pub ws_bind: SocketAddr,
    pub cols: u16,
    pub rows: u16,
}

impl DaemonConfig {
    /// 用给定 socket 路径 + 默认尺寸 + 默认 WS bind 建配置。
    pub fn with_socket(socket_path: PathBuf) -> Self {
        Self {
            socket_path,
            ws_bind: SocketAddr::from(([0, 0, 0, 0], DEFAULT_WS_PORT)),
            cols: DEFAULT_COLS,
            rows: DEFAULT_ROWS,
        }
    }
}

/// `$XDG_RUNTIME_DIR/quill-kernel.sock`。`XDG_RUNTIME_DIR` 未设时报错 (让调用方
/// 用 `--socket=<path>` 显式指定,而非静默落到 `/tmp` 这种可被他人写的位置)。
pub fn default_socket_path() -> Result<PathBuf> {
    let dir = std::env::var_os("XDG_RUNTIME_DIR")
        .ok_or_else(|| anyhow!("XDG_RUNTIME_DIR 未设置;用 --socket=<path> 显式指定 socket 路径"))?;
    Ok(PathBuf::from(dir).join(DEFAULT_SOCKET_NAME))
}

/// 从 argv 抠 `--socket=<path>`(单一 `=` 形式,与 `main.rs` 手写解析同款,不引 clap)。
pub fn parse_socket_arg(args: &[String]) -> Option<PathBuf> {
    const PREFIX: &str = "--socket=";
    args.iter()
        .skip(1)
        .find_map(|a| a.strip_prefix(PREFIX).map(PathBuf::from))
}

/// 从 argv 抠 `--ws-bind=<addr:port>`(如 `10.0.0.2:7878` / `[::]:7878`)。未给返
/// `Ok(None)`(调用方用默认);给了但解析失败返 `Err`(早失败,别静默吞掉错配)。
pub fn parse_ws_bind_arg(args: &[String]) -> Result<Option<SocketAddr>> {
    const PREFIX: &str = "--ws-bind=";
    match args.iter().skip(1).find_map(|a| a.strip_prefix(PREFIX)) {
        Some(s) => {
            let addr = s.parse::<SocketAddr>().with_context(|| {
                format!("解析 --ws-bind={s} 失败 (需 addr:port,如 0.0.0.0:7878)")
            })?;
            Ok(Some(addr))
        }
        None => Ok(None),
    }
}

/// daemon 主循环。spawn shell tab、绑 socket、注册三源、`run` 阻塞到收信号 /
/// shell 退出,返回前清理 socket 文件。
pub fn run(config: DaemonConfig) -> Result<()> {
    let mut event_loop: EventLoop<'static, DaemonData> =
        EventLoop::try_new().context("calloop EventLoop::try_new 失败")?;
    let loop_handle = event_loop.handle();
    let loop_signal = event_loop.get_signal();

    // Source 1:SIGINT + SIGTERM → 停 loop(退出后清理 socket)。
    // **必须早于任何 spawn 线程的代码**:`calloop::Signals::new` 只在*当前(主)
    // 线程* block 这两个信号(`pthread_sigmask`)。紧接着的 `TabInstance::spawn`
    // → `ComposerState` → completion `WorkerPool` 会起 4 个工作线程,它们继承主
    // 线程**此刻**的 signal mask 才会一起 block;否则 SIGTERM 落到某个未 block
    // 的 worker 线程 → 走默认 terminate → 跳过下方 socket 清理(实测 exit 143)。
    // calloop signals doc 原话:"set up the signal event source before spawning
    // any thread"。
    let signals = Signals::new(&[Signal::SIGINT, Signal::SIGTERM])
        .context("calloop Signals::new(SIGINT, SIGTERM) 失败")?;
    loop_handle
        .insert_source(signals, |event, _meta, data: &mut DaemonData| {
            tracing::info!(signal = ?event.signal(), "收到终止信号,停止 daemon");
            data.loop_signal.stop();
        })
        .map_err(|e| anyhow!("calloop insert_source(signals) 失败: {e}"))?;

    // 信号已在主线程 block,现在才 spawn shell tab(其 worker 线程继承 block)。
    let tab =
        TabInstance::spawn(config.cols, config.rows).context("daemon 启动:spawn shell tab 失败")?;
    let tab_id = tab.id().raw();
    let session = Session::new(TabList::new(tab));

    // WS 子系统(ADR-0016 / R1):同样必须在 `Signals::new` 之后 spawn(继承已 block
    // 的 SIGINT/SIGTERM mask)。出站走 `std::sync::mpsc` 发 owned `Vec<u8>`(PTY 原始
    // 字节),WS 线程永远拿不到 `!Send` 的 `Session`。先 spawn(绑 TCP)再建 unix
    // socket:WS bind 失败(端口占用等)时早退,此刻还没落 unix socket 文件,无残留可清。
    let (bytes_tx, bytes_rx) = mpsc::channel::<Vec<u8>>();
    let ws = WsServer::spawn(config.ws_bind, bytes_rx)
        .with_context(|| format!("启动 WS 服务 (bind {}) 失败", config.ws_bind))?;

    prepare_socket_path(&config.socket_path)?;
    let listener = UnixListener::bind(&config.socket_path)
        .with_context(|| format!("bind UnixListener {} 失败", config.socket_path.display()))?;
    // INV-3 精神:listener 默认阻塞,Level 模式下无连接时 accept 会 stall 整个单线程
    // loop → 必须非阻塞 + accept 到 WouldBlock。
    listener
        .set_nonblocking(true)
        .context("UnixListener 设非阻塞失败")?;

    let mut data = DaemonData {
        session,
        loop_handle: loop_handle.clone(),
        loop_signal,
        tab_id,
        bytes_tx,
    };

    // Source 2:PTY master fd。
    let pty_fd = data.session.tabs().active().pty().raw_fd();
    // SAFETY:
    // - pty_fd 来自 `PtyHandle::raw_fd()`(构造时 `as_raw_fd().ok_or_else` 校验过一次),
    //   PtyHandle 在 `data.session.tabs` 里,`run()` scope 内全程活着。
    // - drop 序:函数尾显式 `drop(event_loop)` 早于 `drop(data)`,即 Generic source 的
    //   `EPOLL_CTL_DEL` 在 pty fd 仍打开时执行;即便顺序错,calloop 0.14 对已关 fd 的
    //   `EPOLL_CTL_DEL` 返 EBADF 内部容忍(`wl/window.rs:2852` 同源),非 UB。
    // - `borrow_raw` 只取 int 不转移所有权;真正的 read 走 `PtyHandle::read` 自有的
    //   dup reader,从不碰这个 BorrowedFd。
    #[allow(unsafe_code)]
    let pty_borrowed: BorrowedFd<'static> = unsafe { BorrowedFd::borrow_raw(pty_fd) };
    loop_handle
        .insert_source(
            Generic::new(pty_borrowed, Interest::READ, Mode::Level),
            |_readiness, _meta, data: &mut DaemonData| pty_readable(data),
        )
        .map_err(|e| anyhow!("calloop insert_source(pty master fd) 失败: {e}"))?;

    // Source 3:UnixListener。owned 直接交给 Generic —— UnixListener 实现 AsFd,
    // calloop 持所有权并在 source drop 时关闭,无需 BorrowedFd unsafe。
    loop_handle
        .insert_source(
            Generic::new(listener, Interest::READ, Mode::Level),
            |_readiness, meta, data: &mut DaemonData| accept_ready(meta.as_ref(), data),
        )
        .map_err(|e| anyhow!("calloop insert_source(unix listener) 失败: {e}"))?;

    tracing::info!(
        socket = %config.socket_path.display(),
        tab_id,
        cols = config.cols,
        rows = config.rows,
        "quill-kernel daemon 就绪"
    );

    let run_result = event_loop
        .run(None, &mut data, |_data| {})
        .context("calloop EventLoop::run 失败");

    // 显式 drop 序:event_loop(持各 Generic source)先于 data(持 PtyHandle 的 fd),
    // 让 PTY source 的 EPOLL_CTL_DEL 在 fd 仍打开时执行(见上 SAFETY)。
    drop(event_loop);
    // 先 drop data(连带丢掉唯一的 `bytes_tx`)→ WS broadcaster 线程的 `recv()` 返
    // Err 而退出(退出时清空客户端注册表 → 各 writer 线程的 `recv` 返 Err 自退);
    // 再 `ws.shutdown()` 置关停标志 + join acceptor/broadcaster,干净收尾。
    drop(data);
    ws.shutdown();

    remove_socket_quiet(&config.socket_path);

    run_result
}

/// 单线程 daemon 的 calloop `Data`。callback 拿 `&mut DaemonData` 走字段 split
/// borrow(与 `wl::window::LoopData` 同模式)。
struct DaemonData {
    session: Session,
    /// 运行期注册新 source(每个 unix 客户端连接一个 write source)用。
    loop_handle: LoopHandle<'static, DaemonData>,
    loop_signal: LoopSignal,
    /// 本切片唯一 tab 的 raw id(协议层 `u64`,INV-010 不能从 `u64` 重建 TabId)。
    tab_id: u64,
    /// 出站 PTY **原始字节** → WS 线程(ADR-0015 R1)。只过 owned `Vec<u8>`,绝不让
    /// `!Send` 的 `Session` 越线程边界。
    bytes_tx: Sender<Vec<u8>>,
}

/// 一个 unix 客户端连接待写出的快照字节 + 已写偏移(非阻塞 write 可能分多次)。
struct WritePending {
    buf: Vec<u8>,
    written: usize,
}

/// PTY read 结果的纯决策(无副作用,可单测)。镜像 `wl::window::pty_readable_action`
/// 的策略,但那是 `pub(crate)` 取不到,这里独立实现。
#[derive(Debug, PartialEq, Eq)]
enum PtyRead {
    /// `Ok(n>0)`:喂字节后继续读(Level 模式须排空到 WouldBlock)。
    Feed,
    /// `WouldBlock`:本轮排空,保留 source。
    Drained,
    /// `Interrupted`(EINTR):重试。
    Retry,
    /// `Ok(0)` / `EIO`:子 shell 退出。
    Closed,
    /// 其它 IO 错误:非预期,按致命处理。
    Fatal,
}

fn classify_pty_read(res: &io::Result<usize>) -> PtyRead {
    match res {
        Ok(0) => PtyRead::Closed,
        Ok(_) => PtyRead::Feed,
        Err(e) if e.kind() == io::ErrorKind::WouldBlock => PtyRead::Drained,
        Err(e) if e.kind() == io::ErrorKind::Interrupted => PtyRead::Retry,
        Err(e) if e.raw_os_error() == Some(libc::EIO) => PtyRead::Closed,
        Err(_) => PtyRead::Fatal,
    }
}

/// PTY master fd readable:drain 到 WouldBlock。每 chunk 既喂 [`Session::on_pty_output`]
/// (驱动服务端 term,供 unix-dump 取网格快照),又**按序累积进一个 batch**;drain 完
/// (WouldBlock)把整个 batch 作为 owned `Vec<u8>` 经 `bytes_tx` 推给 WS 线程。
///
/// **字节流逐 chunk 不可丢/不可去重**:把一次 readiness 内的多 chunk **拼接**成一个
/// batch 一次性发(少发 WS 消息,字节等价),而非像旧网格那样「排空多 chunk 只发末态」。
/// batch 内顺序 = PTY 出字节顺序,跨 readiness 顺序由 calloop 串行保证。
/// 子 shell 退出(EOF/EIO)→ 收尸 + 停 loop(单 tab 切片:shell 死即 daemon 退)。
fn pty_readable(data: &mut DaemonData) -> io::Result<PostAction> {
    let tab_id = data.tab_id;
    let mut buf = [0u8; PTY_READ_BUF];
    let mut batch: Vec<u8> = Vec::new();
    loop {
        let read = data
            .session
            .tabs_mut()
            .active_mut()
            .pty_mut()
            .read(&mut buf);
        match classify_pty_read(&read) {
            PtyRead::Feed => {
                if let Ok(n) = read {
                    // 喂服务端 term(维持网格能力 / unix-dump);返回的 dirty 在字节流
                    // 模型下不再用作下发门控(字节每 chunk 都要转发,见模块 doc)。
                    data.session.on_pty_output(tab_id, &buf[..n]);
                    batch.extend_from_slice(&buf[..n]);
                }
            }
            PtyRead::Retry => continue,
            PtyRead::Drained => {
                if !batch.is_empty() {
                    // best-effort:仅在 WS 子系统已关停(daemon 即将退出)时失败,忽略。
                    let _ = data.bytes_tx.send(batch);
                }
                return Ok(PostAction::Continue);
            }
            PtyRead::Closed => {
                tracing::info!(tab_id, "PTY EOF/EIO:子 shell 退出,停止 daemon");
                // why: 退出前 flush 本次 readiness 已读出的尾部字节,否则 shell 退出的
                // 最后一批输出会丢(字节流不可丢)。broadcaster 在 bytes_tx drop 前会先
                // 收完通道里这批再退,best-effort 送达仍存活的客户端。
                if !batch.is_empty() {
                    let _ = data.bytes_tx.send(batch);
                }
                let _ = data.session.tabs_mut().active_mut().pty_mut().try_wait();
                data.loop_signal.stop();
                return Ok(PostAction::Remove);
            }
            PtyRead::Fatal => {
                if let Err(e) = read {
                    tracing::error!(tab_id, ?e, "PTY read 非预期错误,停止 daemon");
                }
                // why: 同 Closed —— 出错前已读出的字节也要 flush,不静默丢弃。
                if !batch.is_empty() {
                    let _ = data.bytes_tx.send(batch);
                }
                data.loop_signal.stop();
                return Ok(PostAction::Remove);
            }
        }
    }
}

/// unix listener readable:accept 到 WouldBlock,每个新连接发一帧当前网格快照
/// (`quill-dump` 调试路径,line-delimited JSON;与 WS 字节流路径独立、不受片1 影响)。
fn accept_ready(listener: &UnixListener, data: &mut DaemonData) -> io::Result<PostAction> {
    loop {
        match listener.accept() {
            Ok((stream, _addr)) => {
                if let Err(e) = serve_snapshot(stream, data) {
                    tracing::warn!(?e, "新 unix 客户端首帧快照下发失败");
                }
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(PostAction::Continue),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => {
                tracing::warn!(?e, "UnixListener accept 失败");
                return Ok(PostAction::Continue);
            }
        }
    }
}

/// 把当前 active tab 的快照 JSON 行排进一个新的 write source(非阻塞写,分次写完
/// 即 `PostAction::Remove` 自动注销 + 关连接)。
fn serve_snapshot(stream: UnixStream, data: &mut DaemonData) -> Result<()> {
    stream
        .set_nonblocking(true)
        .context("客户端 stream 设非阻塞失败")?;
    let snap = data.session.snapshot_active();
    let line = snapshot_line(&snap)?;
    let pending = Rc::new(RefCell::new(WritePending {
        buf: line,
        written: 0,
    }));
    data.loop_handle
        .insert_source(
            Generic::new(stream, Interest::WRITE, Mode::Level),
            move |_readiness, meta, _data: &mut DaemonData| write_pending(meta.as_ref(), &pending),
        )
        .map_err(|e| anyhow!("calloop insert_source(client write) 失败: {e}"))?;
    Ok(())
}

/// 客户端连接 writable:把剩余字节写出。全部写完 / 对端关闭 / 出错 → `Remove`
/// (calloop 自动注销 source 并 drop 掉 owned 的 UnixStream = 关连接)。
fn write_pending(
    stream: &UnixStream,
    pending: &Rc<RefCell<WritePending>>,
) -> io::Result<PostAction> {
    let mut p = pending.borrow_mut();
    // `impl Write for &UnixStream`:用不可变引用即可写(NoIoDrop 只给 &)。
    let mut writer: &UnixStream = stream;
    loop {
        if p.written >= p.buf.len() {
            return Ok(PostAction::Remove);
        }
        match writer.write(&p.buf[p.written..]) {
            Ok(0) => return Ok(PostAction::Remove),
            Ok(n) => p.written += n,
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(PostAction::Continue),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => return Ok(PostAction::Remove),
        }
    }
}

/// 一条线缆帧 = `Snapshot` 的 JSON + 换行(line-delimited;ADR-0015 先 JSON 后 bincode)。
/// **仅 unix socket 路径用**(行分隔,`quill-dump`)。WS 路径走二进制字节流,不经此。
fn snapshot_line(snap: &Snapshot) -> Result<Vec<u8>> {
    let mut bytes = serde_json::to_vec(snap).context("序列化 Snapshot 为 JSON 失败")?;
    bytes.push(b'\n');
    Ok(bytes)
}

/// 有界字节环缓冲:append PTY 原始字节,超容量丢**最旧**字节(保证最近输出 = 当前
/// 可见屏恒完整)。新 WS 客户端连上重放此环重建当前屏。**钉在 WS 子系统(broadcaster
/// 线程)**,只装 `u8` / `Vec<u8>`(Send),与 `!Send` 的 `Session` 无关。
struct ByteRing {
    buf: VecDeque<u8>,
    cap: usize,
}

impl ByteRing {
    fn new(cap: usize) -> Self {
        Self {
            buf: VecDeque::new(),
            cap,
        }
    }

    /// append 一批字节,随后从**前端**(最旧)裁到不超过 `cap`。单批超 `cap` 时只
    /// 保留其末尾 `cap` 字节(等价于"刚被后续字节挤掉了前面")。
    fn push(&mut self, bytes: &[u8]) {
        self.buf.extend(bytes.iter().copied());
        if self.buf.len() > self.cap {
            let overflow = self.buf.len() - self.cap;
            self.buf.drain(..overflow);
        }
    }

    /// 环内现有字节的有序拷贝(连上重放用;只在 connect 时做一次)。
    fn snapshot(&self) -> Vec<u8> {
        self.buf.iter().copied().collect()
    }
}

/// 同步 WebSocket **字节流直播** 服务(ADR-0015 R1)。两条常驻 `std::thread`:
/// - **broadcaster**:`recv()` calloop 线程发来的 PTY 原始字节(owned `Vec<u8>`),在
///   [`Shared`] 的锁内既 append 进 [`ByteRing`],又 fan-out 给注册表里每个客户端的有界
///   通道。`bytes_tx` 全丢后 `recv()` 返 Err → 清空注册表(各 writer 醒来自退)→ 退出。
/// - **acceptor**:非阻塞轮询 `TcpListener::accept()`,每个新连接起一条 per-conn 线程跑
///   [`serve_connection`](同口 HTTP / WS 分流)。WS 连接的 per-conn 线程在锁内**原子地**
///   {重放环缓冲快照 + 注册自己的出站通道},随后转为**长命** writer loop 持续推 live
///   字节;慢客户端只阻塞自己这条线程,不卡 broadcaster(ADR-0016 Alt3 拒单线程裸注册
///   的同一理由)。
///
/// 线程数被 [`MAX_WS_CONNS`] 封顶。跨线程仍只过 owned 字节(`Vec<u8>` 进 broadcaster,
/// 转 [`Bytes`] 后在 WS 子系统内 refcount 流转),`!Send` 的 [`Session`] 永远钉在 calloop。
struct WsServer {
    shutdown: Arc<AtomicBool>,
    acceptor: Option<JoinHandle<()>>,
    broadcaster: Option<JoinHandle<()>>,
}

/// WS 子系统共享态:**字节环缓冲 + 已连客户端出站通道注册表,同一把锁**。
///
/// 一把锁保「重放 + 订阅」原子(字节流非幂等的命根):新客户端在锁内一次性
/// {`ring.snapshot()` 取重放 + `clients.push(tx)` 订阅},broadcaster 在锁内一次性
/// {`ring.push()` + `fan_out()`}。⟹ 任一字节批要么落在重放快照里(注册前已 append),
/// 要么经 live 通道送达(注册后才 fan-out),**绝不丢、绝不双发**。
struct Shared {
    ring: ByteRing,
    clients: Vec<SyncSender<Bytes>>,
}

type SharedState = Arc<Mutex<Shared>>;

impl WsServer {
    /// 绑 TCP + spawn broadcaster/acceptor 线程。**调用方须保证已在 `Signals::new`
    /// 之后**调用(线程继承已 block 的 SIGINT/SIGTERM mask;见 [`run`] 信号顺序注释)。
    fn spawn(bind: SocketAddr, bytes_rx: Receiver<Vec<u8>>) -> Result<Self> {
        let listener =
            TcpListener::bind(bind).with_context(|| format!("WS TcpListener::bind {bind} 失败"))?;
        // 非阻塞 + 轮询关停:让 acceptor 能周期检查 shutdown 标志而非永久阻塞在
        // accept() 上(否则退出时 join 不回来)。
        listener
            .set_nonblocking(true)
            .context("WS TcpListener 设非阻塞失败")?;

        let shared: SharedState = Arc::new(Mutex::new(Shared {
            ring: ByteRing::new(BYTE_RING_CAP),
            clients: Vec::new(),
        }));
        let shutdown = Arc::new(AtomicBool::new(false));
        let active_conns = Arc::new(AtomicUsize::new(0));

        let shared_b = Arc::clone(&shared);
        let broadcaster = thread::Builder::new()
            .name("quill-ws-cast".to_string())
            .spawn(move || {
                while let Ok(bytes) = bytes_rx.recv() {
                    let mut g = lock_recover(&shared_b);
                    g.ring.push(&bytes);
                    let frame = Bytes::from(bytes); // move 进 Bytes,fan-out 只 refcount
                    fan_out(&mut g.clients, &frame);
                }
                // bytes_tx 全丢 → 关停:清空注册表 → 各 writer 的 recv 返 Disconnected 自退。
                lock_recover(&shared_b).clients.clear();
            })
            .context("spawn WS broadcaster 线程失败")?;

        let shared_a = Arc::clone(&shared);
        let shutdown_a = Arc::clone(&shutdown);
        let acceptor = thread::Builder::new()
            .name("quill-ws-accept".to_string())
            .spawn(move || ws_accept_loop(listener, shared_a, shutdown_a, active_conns))
            .context("spawn WS acceptor 线程失败")?;

        tracing::info!(%bind, "WS 字节流直播服务就绪 (tungstenite, ws:// + 同口 http)");
        Ok(Self {
            shutdown,
            acceptor: Some(acceptor),
            broadcaster: Some(broadcaster),
        })
    }

    /// 置关停标志 + join acceptor/broadcaster(干净收尾)。调用前应已 drop `bytes_tx`,
    /// 否则 broadcaster 的 `recv()` 不会返回、join 会卡住。per-conn writer 线程不
    /// join(detach):broadcaster 清表后它们靠 `recv` 返 Err 自退,进程退出兜底。
    fn shutdown(mut self) {
        self.shutdown.store(true, Ordering::SeqCst);
        if let Some(h) = self.acceptor.take() {
            let _ = h.join();
        }
        if let Some(h) = self.broadcaster.take() {
            let _ = h.join();
        }
    }
}

impl Drop for WsServer {
    fn drop(&mut self) {
        // 安全网:即便未显式 shutdown()(如启动错误路径提前 drop),也让 acceptor
        // 轮询循环尽快退出。join 只在 shutdown() 里做,Drop 不阻塞。
        self.shutdown.store(true, Ordering::SeqCst);
    }
}

/// 每个 per-conn 线程持一个,Drop 时递减在跑连接计数(收口「无上限起线程」的闸门)。
/// 无论线程因握手失败 / writer 退出 / panic 哪条路径结束,计数都被精确归还。
struct ConnGuard {
    count: Arc<AtomicUsize>,
}

impl Drop for ConnGuard {
    fn drop(&mut self) {
        self.count.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Mutex 上锁不 panic:中毒(别的线程持锁时 panic)也恢复内部值继续用。WS 共享态
/// 即便短暂不一致也能靠重连 + 重放自愈,恢复即可(CLAUDE.md 禁 unwrap/expect)。
fn lock_recover<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// 把一批字节 fan-out 给所有在连客户端,顺带剪除「断开 / 慢到撑满」的(`try_send`
/// 永不阻塞 broadcaster):
/// - `Ok`:入对端有界队列,保留。
/// - `Full`:慢客户端队列满 → **断开**(retain 返 false 剪除 tx → 对端 writer 的
///   `recv` 返 Disconnected 自退 → 关连接)。**字节流非幂等,绝不能丢帧/留陈帧**;
///   慢客户端重连从环缓冲重放即恢复。这是与网格全量快照(可丢旧帧)的本质区别。
/// - `Disconnected`:对端 writer 已退(Receiver drop)→ 剪除。
fn fan_out(clients: &mut Vec<SyncSender<Bytes>>, frame: &Bytes) {
    clients.retain(|tx| match tx.try_send(frame.clone()) {
        Ok(()) => true,
        Err(TrySendError::Full(_)) => false,
        Err(TrySendError::Disconnected(_)) => false,
    });
}

/// acceptor 线程主体:非阻塞轮询 accept,每连接起一条 per-conn 线程跑
/// [`serve_connection`]。[`MAX_WS_CONNS`] 闸门:超限直接 close,不再无上限起线程。
fn ws_accept_loop(
    listener: TcpListener,
    shared: SharedState,
    shutdown: Arc<AtomicBool>,
    active_conns: Arc<AtomicUsize>,
) {
    while !shutdown.load(Ordering::SeqCst) {
        match listener.accept() {
            Ok((stream, peer)) => {
                // 先占名额:超限即回退 + 立即关连接(避免握手前就无上限起线程)。
                if active_conns.fetch_add(1, Ordering::SeqCst) >= MAX_WS_CONNS {
                    active_conns.fetch_sub(1, Ordering::SeqCst);
                    tracing::warn!(?peer, max = MAX_WS_CONNS, "WS 连接数达上限,拒绝新连接");
                    let _ = stream.shutdown(Shutdown::Both);
                    continue;
                }
                // guard move 进闭包:线程任意路径结束(含 spawn 失败时闭包被 drop)
                // 都精确归还名额。
                let guard = ConnGuard {
                    count: Arc::clone(&active_conns),
                };
                let shared = Arc::clone(&shared);
                if let Err(e) = thread::Builder::new()
                    .name("quill-ws-conn".to_string())
                    .spawn(move || {
                        let _guard = guard;
                        serve_connection(stream, shared);
                    })
                {
                    tracing::warn!(?peer, ?e, "spawn WS 连接线程失败");
                }
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                // 无新连接:歇一会儿再轮询关停标志(轻量 busy-wait,50ms 足够)。
                thread::sleep(WS_ACCEPT_POLL);
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => {
                tracing::warn!(?e, "WS accept 失败");
                thread::sleep(WS_ACCEPT_POLL);
            }
        }
    }
}

/// 单连接入口:`peek` 首部分流 —— 带 `Upgrade: websocket` 走 WS 字节流直播,否则当
/// 普通 HTTP 请求(serve 内嵌 index.html / vendored xterm 资产 / 404)。同一 7878 端口
/// 「一个 URL」既出网页又直播。`peek`(MSG_PEEK)非破坏:WS 分支把**同一未消费的
/// stream** 交 tungstenite 握手,它从头完整再读一遍请求(同口分流最不易出错的路径,
/// 见 ADR-0016)。
fn serve_connection(stream: TcpStream, shared: SharedState) {
    // 限握手 + 写的阻塞时长,防静默/弱网客户端把本线程挂死。
    let _ = stream.set_read_timeout(Some(WS_CONN_READ_TIMEOUT));
    let _ = stream.set_write_timeout(Some(WS_CONN_READ_TIMEOUT));
    let head = match peek_request_head(&stream) {
        Ok(h) => h,
        Err(e) => {
            tracing::debug!(?e, "读取请求头 (peek) 失败");
            return;
        }
    };
    if request_is_ws_upgrade(&head) {
        serve_ws_live(stream, shared);
    } else {
        serve_http(stream, &head);
    }
}

/// 非破坏地 `peek` 出 HTTP 请求头(到 `\r\n\r\n` 或上限 / 超时)。`peek` 不消费内核
/// 缓冲,故 WS 分支能把同一 stream 原样交给 `tungstenite::accept`。
fn peek_request_head(stream: &TcpStream) -> io::Result<Vec<u8>> {
    let deadline = Instant::now() + WS_CONN_READ_TIMEOUT;
    let mut buf = vec![0u8; HTTP_PEEK_MAX];
    loop {
        let n = stream.peek(&mut buf)?;
        if n == 0 {
            return Ok(Vec::new()); // 对端没发数据就关了
        }
        // 头收全 / 缓冲满 / 超时 → 用现有字节决策。
        if header_complete(&buf[..n]) || n >= buf.len() || Instant::now() >= deadline {
            buf.truncate(n);
            return Ok(buf);
        }
        // 头还没收全:peek 不消费,有旧数据时会立即返回同样字节 → 歇会儿等更多。
        thread::sleep(Duration::from_millis(5));
    }
}

/// 请求头是否已含结束序列 `\r\n\r\n`。
fn header_complete(buf: &[u8]) -> bool {
    buf.windows(4).any(|w| w == b"\r\n\r\n")
}

/// 大小写不敏感判断请求是否为 WebSocket upgrade(找 `Upgrade:` 头含 `websocket`)。
fn request_is_ws_upgrade(head: &[u8]) -> bool {
    let text = String::from_utf8_lossy(head).to_ascii_lowercase();
    text.lines().any(|l| {
        let l = l.trim();
        l.starts_with("upgrade:") && l.contains("websocket")
    })
}

/// 从请求行 `GET /path?q HTTP/1.1` 抠出 path(去 query/fragment)。
fn request_path(head: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(head);
    let line = text.lines().next()?;
    let mut parts = line.split_whitespace();
    let _method = parts.next()?;
    let target = parts.next()?;
    let path = target.split(['?', '#']).next().unwrap_or(target);
    Some(path.to_string())
}

/// 普通 HTTP 请求:`GET /` / `/index.html` 返 xterm.js 页,`/vendor/*` 返 vendored
/// xterm 资产,其余 404。手搓最小响应(tungstenite 不含 HTTP 文件服务;带
/// `Content-Length` + `Connection: close`)。
fn serve_http(mut stream: TcpStream, head: &[u8]) {
    // 消费掉已 peek 的请求字节,避免带未读数据 close 触发 RST 截断响应(GET 无 body,
    // 单次 read 足以排空小请求,best-effort)。
    let mut scratch = vec![0u8; head.len()];
    let mut reader: &TcpStream = &stream;
    let _ = reader.read(&mut scratch);

    let path = request_path(head).unwrap_or_default();
    let js = "text/javascript; charset=utf-8";
    let (status, ctype, body): (&str, &str, &[u8]) = match path.as_str() {
        "/" | "/index.html" => ("200 OK", "text/html; charset=utf-8", INDEX_HTML.as_bytes()),
        "/vendor/xterm.js" => ("200 OK", js, XTERM_JS.as_bytes()),
        "/vendor/xterm-addon-fit.js" => ("200 OK", js, XTERM_FIT_JS.as_bytes()),
        "/vendor/xterm.css" => ("200 OK", "text/css; charset=utf-8", XTERM_CSS.as_bytes()),
        _ => ("404 Not Found", "text/plain; charset=utf-8", b"not found\n"),
    };
    let header = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\n\
         Connection: close\r\nCache-Control: no-store\r\n\r\n",
        body.len()
    );
    if stream.write_all(header.as_bytes()).is_ok() {
        let _ = stream.write_all(body);
        let _ = stream.flush();
    }
    let _ = stream.shutdown(Shutdown::Write);
}

/// WS 字节流直播分支:握手 → 锁内{重放环缓冲 + 注册出站通道}→ 发重放 → 长命 writer
/// loop 持续推 live 字节。慢客户端只阻塞本线程(写超时 [`WS_CONN_READ_TIMEOUT`] 兜底)
/// 或被 broadcaster 断开,不卡别人。
fn serve_ws_live(stream: TcpStream, shared: SharedState) {
    let mut ws = match tungstenite::accept(stream) {
        Ok(ws) => ws,
        Err(e) => {
            tracing::debug!(?e, "WS 握手失败");
            return;
        }
    };

    // 每客户端一条有界通道:writer(本线程)持 rx,broadcaster 持 tx(注册进 clients)。
    let (tx, rx) = mpsc::sync_channel::<Bytes>(WS_CLIENT_QUEUE);
    // **原子**地在同一把锁内取环缓冲重放 + 注册 tx:保证重放快照之后到达的字节都经
    // live 通道送达(无丢);快照之前的字节都在重放里(无双发)。见 [`Shared`] doc。
    let replay = {
        let mut g = lock_recover(&shared);
        let snap = g.ring.snapshot();
        g.clients.push(tx);
        snap
    };
    // 关键:立即释放共享态 Arc。writer 绝不长期持 shared,否则关停时
    // 「broadcaster 清表 → senders drop → writer 醒」的链条会因 writer 持表成环。
    drop(shared);

    // 先发环缓冲重放(重建当前屏);非空才发。
    if !replay.is_empty() && ws.send(Message::Binary(Bytes::from(replay))).is_err() {
        return;
    }

    // 长命 writer loop:阻塞等 live 字节 → 推送;空闲超时发 Ping 探活。
    loop {
        match rx.recv_timeout(WS_PING_INTERVAL) {
            Ok(first) => {
                // **拼接**积压的所有批(顺序保留、一字节不丢)成一个 Binary 帧再发
                // (少发 WS 消息、字节等价)。绝不像网格那样「只发末帧」丢中间字节。
                let mut buf: Vec<u8> = first.to_vec();
                while let Ok(more) = rx.try_recv() {
                    buf.extend_from_slice(&more);
                }
                if ws.send(Message::Binary(Bytes::from(buf))).is_err() {
                    break;
                }
            }
            // 空闲:发 Ping 探活 + 顺带让本线程周期醒来。对端已断 → send 报错 → 退出。
            Err(RecvTimeoutError::Timeout) => {
                if ws.send(Message::Ping(Bytes::new())).is_err() {
                    break;
                }
            }
            // broadcaster 清表丢 tx / 被 fan_out 因队满断开 → 退出。
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
    // 主动 close:enqueue close 帧 + flush 送出;随后连接 drop 关闭 TCP。
    let _ = ws.close(None);
    let _ = ws.flush();
}

/// 启动期处理 socket 路径:已有活跃 daemon 监听则拒绝覆盖,否则删掉残留 socket 文件。
fn prepare_socket_path(path: &Path) -> Result<()> {
    match UnixStream::connect(path) {
        Ok(_) => bail!("socket {} 已有活跃 daemon 监听,拒绝覆盖", path.display()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        // ConnectionRefused = socket 文件在但无人监听(上次没清干净)→ 删。
        Err(e) if e.kind() == io::ErrorKind::ConnectionRefused => {
            remove_socket_quiet(path);
            Ok(())
        }
        // 其它错误(权限等):尝试删,真错由后续 bind 暴露更准确的信息。
        Err(_) => {
            remove_socket_quiet(path);
            Ok(())
        }
    }
}

fn remove_socket_quiet(path: &Path) {
    if let Err(e) = std::fs::remove_file(path) {
        if e.kind() != io::ErrorKind::NotFound {
            tracing::warn!(path = %path.display(), ?e, "删除 socket 文件失败");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kernel::proto::{CursorShapeWire, CursorWire};
    use tungstenite::stream::MaybeTlsStream;
    use tungstenite::WebSocket;

    #[test]
    fn classify_pty_read_policy() {
        assert_eq!(classify_pty_read(&Ok(5)), PtyRead::Feed);
        assert_eq!(classify_pty_read(&Ok(0)), PtyRead::Closed);
        assert_eq!(
            classify_pty_read(&Err(io::Error::from(io::ErrorKind::WouldBlock))),
            PtyRead::Drained
        );
        assert_eq!(
            classify_pty_read(&Err(io::Error::from(io::ErrorKind::Interrupted))),
            PtyRead::Retry
        );
        assert_eq!(
            classify_pty_read(&Err(io::Error::from_raw_os_error(libc::EIO))),
            PtyRead::Closed
        );
        assert_eq!(
            classify_pty_read(&Err(io::Error::from_raw_os_error(libc::EPERM))),
            PtyRead::Fatal
        );
    }

    #[test]
    fn snapshot_line_is_json_plus_newline() {
        let snap = Snapshot {
            tab_id: 1,
            cols: 1,
            rows: 1,
            cells: vec![],
            row_texts: vec![String::new()],
            cursor: CursorWire {
                col: 0,
                line: 0,
                visible: true,
                shape: CursorShapeWire::Block,
            },
            title: String::new(),
        };
        let line = snapshot_line(&snap).expect("snapshot_line");
        assert_eq!(*line.last().expect("非空"), b'\n', "最后一字节应是换行");
        let parsed: Snapshot =
            serde_json::from_slice(&line[..line.len() - 1]).expect("去换行后反序列化");
        assert_eq!(parsed, snap, "去换行后应往返回等价 Snapshot");
    }

    #[test]
    fn parse_socket_arg_extracts_path() {
        let args = vec![
            "quill-kernel".to_string(),
            "--socket=/run/user/x.sock".to_string(),
        ];
        assert_eq!(
            parse_socket_arg(&args),
            Some(PathBuf::from("/run/user/x.sock"))
        );
        assert_eq!(parse_socket_arg(&["quill-kernel".to_string()]), None);
    }

    #[test]
    fn parse_ws_bind_arg_extracts_and_validates() {
        // 给了合法 addr:port → Some。
        let args = vec![
            "quill-kernel".to_string(),
            "--ws-bind=10.0.0.2:7878".to_string(),
        ];
        assert_eq!(
            parse_ws_bind_arg(&args).expect("合法 ws-bind 解析"),
            Some("10.0.0.2:7878".parse().expect("test addr"))
        );
        // IPv6 形式也支持。
        let v6 = vec![
            "quill-kernel".to_string(),
            "--ws-bind=[::1]:9000".to_string(),
        ];
        assert_eq!(
            parse_ws_bind_arg(&v6).expect("合法 v6 ws-bind"),
            Some("[::1]:9000".parse().expect("test v6 addr"))
        );
        // 没给 → Ok(None)(用默认)。
        assert_eq!(
            parse_ws_bind_arg(&["quill-kernel".to_string()]).expect("缺省"),
            None
        );
        // 给了但非法 → Err(早失败)。
        let bad = vec![
            "quill-kernel".to_string(),
            "--ws-bind=not-an-addr".to_string(),
        ];
        assert!(parse_ws_bind_arg(&bad).is_err());
    }

    #[test]
    fn default_ws_bind_is_lan_reachable() {
        // 默认绑 0.0.0.0:DEFAULT_WS_PORT —— LAN 可达(非 loopback),手机经 VPN 可连。
        let cfg = DaemonConfig::with_socket(PathBuf::from("/tmp/x.sock"));
        assert_eq!(
            cfg.ws_bind,
            SocketAddr::from(([0, 0, 0, 0], DEFAULT_WS_PORT))
        );
        assert!(!cfg.ws_bind.ip().is_loopback(), "默认不应绑 loopback");
    }

    /// 环缓冲超容量丢**最旧**字节,最近字节恒完整。
    #[test]
    fn byte_ring_trims_oldest_keeps_recent() {
        let mut ring = ByteRing::new(4);
        ring.push(b"ab");
        assert_eq!(ring.snapshot(), b"ab");
        ring.push(b"cdef"); // 总 "abcdef" 超 4 → 丢最旧 → "cdef"
        assert_eq!(ring.snapshot(), b"cdef");
        // 单批超 cap:只留末尾 cap 字节。
        ring.push(b"0123456789");
        assert_eq!(ring.snapshot(), b"6789");
    }

    /// fan_out 背压语义:队满 → 断开(剪除),与网格「丢旧帧保连接」相反。
    #[test]
    fn fan_out_disconnects_on_full_not_drop() {
        let (tx, _rx) = mpsc::sync_channel::<Bytes>(1);
        let mut clients = vec![tx];
        fan_out(&mut clients, &Bytes::from_static(b"a")); // 入队(占满)
        assert_eq!(clients.len(), 1, "未满前保留");
        fan_out(&mut clients, &Bytes::from_static(b"b")); // 满 → 断开剪除
        assert!(clients.is_empty(), "队满应断开慢客户端而非丢字节");
    }

    fn free_port() -> u16 {
        let l = TcpListener::bind("127.0.0.1:0").expect("bind 临时端口");
        l.local_addr().expect("local_addr").port()
    }

    fn connect_ws_retry(url: &str, timeout: Duration) -> WebSocket<MaybeTlsStream<TcpStream>> {
        let deadline = Instant::now() + timeout;
        loop {
            match tungstenite::connect(url) {
                Ok((ws, _resp)) => {
                    if let MaybeTlsStream::Plain(s) = ws.get_ref() {
                        let _ = s.set_read_timeout(Some(Duration::from_millis(200)));
                    }
                    return ws;
                }
                Err(_) if Instant::now() < deadline => {
                    thread::sleep(Duration::from_millis(50));
                }
                Err(e) => panic!("连不上 {url}: {e}"),
            }
        }
    }

    /// 读 WS binary 帧累积,直到出现 `needle` 或超时;返回累积字节(供断言)。
    fn read_binary_until(
        ws: &mut WebSocket<MaybeTlsStream<TcpStream>>,
        needle: &[u8],
        timeout: Duration,
    ) -> Vec<u8> {
        let deadline = Instant::now() + timeout;
        let mut acc = Vec::new();
        while Instant::now() < deadline {
            match ws.read() {
                Ok(Message::Binary(b)) => {
                    acc.extend_from_slice(&b);
                    if acc.windows(needle.len()).any(|w| w == needle) {
                        return acc;
                    }
                }
                Ok(_) => {}                                         // Ping/Pong/Text 忽略
                Err(_) => thread::sleep(Duration::from_millis(20)), // read timeout 等
            }
        }
        acc
    }

    /// 片1 核心:WS 连上先收环缓冲**重放**,之后持续收 **live** 字节;**多客户端
    /// fan-out**。直接喂 `bytes_tx`(模拟 PTY 出字节),不依赖真 PTY 时序,确定性。
    #[test]
    fn ws_replays_ring_then_streams_live_to_clients() {
        let port = free_port();
        let addr = SocketAddr::from(([127, 0, 0, 1], port));
        let url = format!("ws://127.0.0.1:{port}/");
        let (tx, rx) = mpsc::channel::<Vec<u8>>();
        let ws = WsServer::spawn(addr, rx).expect("spawn WS server");

        // 客户端 A 先连(此刻环为空)。
        let mut a = connect_ws_retry(&url, Duration::from_secs(5));

        // 喂 AAAA:A 应**live**收到。A 收到即证 broadcaster 已 ring.push(AAAA)
        //(push 与 fan_out 同锁内,A 拿到时环必已含 AAAA)。
        tx.send(b"AAAA".to_vec()).expect("send AAAA");
        let got_a = read_binary_until(&mut a, b"AAAA", Duration::from_secs(5));
        assert!(
            got_a.windows(4).any(|w| w == b"AAAA"),
            "A 应 live 收到 AAAA;实际 {got_a:?}"
        );

        // 客户端 B 现在才连:AAAA 已在环里 → B 应在**重放**里收到 AAAA。
        let mut b = connect_ws_retry(&url, Duration::from_secs(5));
        let replay_b = read_binary_until(&mut b, b"AAAA", Duration::from_secs(5));
        assert!(
            replay_b.windows(4).any(|w| w == b"AAAA"),
            "B 应在重放中收到 AAAA;实际 {replay_b:?}"
        );

        // 喂 BBBB:A、B 都应 live 收到(fan-out 到多客户端)。
        tx.send(b"BBBB".to_vec()).expect("send BBBB");
        let live_a = read_binary_until(&mut a, b"BBBB", Duration::from_secs(5));
        let live_b = read_binary_until(&mut b, b"BBBB", Duration::from_secs(5));
        assert!(
            live_a.windows(4).any(|w| w == b"BBBB"),
            "A 应 live 收到 BBBB;实际 {live_a:?}"
        );
        assert!(
            live_b.windows(4).any(|w| w == b"BBBB"),
            "B 应 live 收到 BBBB;实际 {live_b:?}"
        );

        let _ = a.close(None);
        let _ = b.close(None);
        drop(tx); // broadcaster recv 返 Err → 清表 → writer 自退
        ws.shutdown();
    }
}
