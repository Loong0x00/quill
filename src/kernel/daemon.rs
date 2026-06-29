//! 无头会话内核 daemon 的 calloop 接线 (Phase 7 T2, ADR-0015 Phase 1 §4)。
//!
//! T1 ([`crate::kernel::session`]) 给了纯数据层 [`Session`];这里把它挂到一个
//! **单线程** `calloop::EventLoop` 上,同一 loop 注册:
//! - 一个 shell tab 的 PTY master fd (出字节 → [`Session::on_pty_output`] 驱动 term);
//! - 一个 `UnixListener` —— 客户端连上即收当前 [`crate::kernel::proto::Snapshot`]
//!   的 JSON 行 (line-delimited);
//! - `SIGINT` / `SIGTERM` 信号源 (signalfd) → 停 loop → 退出时清理 socket 文件。
//!
//! **T3a 增量 (ADR-0016)**:加一个**同步 `tungstenite` WS 子系统**(独立
//! `std::thread`),让浏览器 / 手机经 `ws://<lan>:<port>` 连上即收一帧
//! [`crate::kernel::proto::Snapshot`]。calloop 线程算快照 → `serde_json::to_vec`
//! → `std::sync::mpsc` → WS 线程广播 **owned 字节**(`Rc` 非 Send 的 [`Session`]
//! 全程钉在 calloop 线程,只有 `Vec<u8>` 过线程边界 = ADR-0015 头号约束的结构性
//! 遵守)。unix socket 路径不动(`quill-dump` 仍走裸 `Snapshot` JSON 行)。
//!
//! **仍留后续 ticket**:直播 / dirty 增量广播(T3b,当前只「连上发一次最新
//! 快照」)、客户端 [`crate::kernel::proto::ClientMsg`] 回灌(T3c,输入 / resize /
//! tab 操作,需 `calloop::channel` 反向唤醒)、多 tab 动态增删 fd。

use std::cell::RefCell;
use std::io::{self, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::os::fd::BorrowedFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use calloop::generic::Generic;
use calloop::signals::{Signal, Signals};
use calloop::{EventLoop, Interest, LoopHandle, LoopSignal, Mode, PostAction};
use tungstenite::Message;

use crate::kernel::proto::Snapshot;
use crate::kernel::session::Session;
use crate::tab::{TabInstance, TabList};

/// 默认 grid 尺寸,与 `wl::run_window` 启动期 + `run_headless_screenshot` 一致
/// (一个 PTY 一个尺寸,ADR-0015 "主控端定尺寸";多客户端尺寸协商留 T3)。
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

/// 单连接握手 + 首帧的读超时,防静默客户端把短命连接线程挂死。
const WS_CONN_READ_TIMEOUT: Duration = Duration::from_secs(10);

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

    // WS 子系统(ADR-0016):同样必须在 `Signals::new` 之后 spawn(继承已 block 的
    // SIGINT/SIGTERM mask)。出站走 `std::sync::mpsc` 发 owned `Vec<u8>`,WS 线程
    // 永远拿不到 `!Send` 的 `Session`。先 spawn(绑 TCP)再建 unix socket:WS bind
    // 失败(端口占用等)时早退,此刻还没落 unix socket 文件,无残留可清。
    let (snap_tx, snap_rx) = mpsc::channel::<Vec<u8>>();
    let ws = WsServer::spawn(config.ws_bind, snap_rx)
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
        snap_tx,
    };

    // 种入一帧初始快照:让在任何 PTY 输出前就连上的浏览器也能立刻拿到一帧
    // (否则 WS 线程的「最新字节」为空,早连客户端收不到东西)。
    broadcast_snapshot(&mut data, tab_id);

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
    // 先 drop data(连带丢掉唯一的 `snap_tx`)→ WS updater 线程的 `recv()` 返 Err
    // 而退出;再 `ws.shutdown()` 置关停标志 + join 两个 WS 线程,干净收尾。
    drop(data);
    ws.shutdown();

    remove_socket_quiet(&config.socket_path);

    run_result
}

/// 单线程 daemon 的 calloop `Data`。callback 拿 `&mut DaemonData` 走字段 split
/// borrow(与 `wl::window::LoopData` 同模式)。
struct DaemonData {
    session: Session,
    /// 运行期注册新 source(每个客户端连接一个 write source)用。
    loop_handle: LoopHandle<'static, DaemonData>,
    loop_signal: LoopSignal,
    /// 本切片唯一 tab 的 raw id(协议层 `u64`,INV-010 不能从 `u64` 重建 TabId)。
    tab_id: u64,
    /// 出站快照字节 → WS 线程(ADR-0016)。只过 owned `Vec<u8>`,绝不让 `!Send`
    /// 的 `Session` 越线程边界。
    snap_tx: Sender<Vec<u8>>,
}

/// 一个客户端连接待写出的快照字节 + 已写偏移(非阻塞 write 可能分多次)。
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

/// PTY master fd readable:drain 到 WouldBlock,每 chunk 喂 [`Session::on_pty_output`]。
/// 一次 readable 唤醒可排空多个 chunk,但只在 **drain 完(WouldBlock)** 后取一帧
/// 快照下发(per-readiness 而非 per-chunk),否则一次大输出会发几十帧重复全量快照。
/// 子 shell 退出(EOF/EIO)→ 收尸 + 停 loop(单 tab 切片:shell 死即 daemon 退)。
fn pty_readable(data: &mut DaemonData) -> io::Result<PostAction> {
    let tab_id = data.tab_id;
    let mut buf = [0u8; PTY_READ_BUF];
    let mut dirty = false;
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
                    dirty |= data.session.on_pty_output(tab_id, &buf[..n]);
                }
            }
            PtyRead::Retry => continue,
            PtyRead::Drained => {
                if dirty {
                    broadcast_snapshot(data, tab_id);
                }
                return Ok(PostAction::Continue);
            }
            PtyRead::Closed => {
                tracing::info!(tab_id, "PTY EOF/EIO:子 shell 退出,停止 daemon");
                let _ = data.session.tabs_mut().active_mut().pty_mut().try_wait();
                data.loop_signal.stop();
                return Ok(PostAction::Remove);
            }
            PtyRead::Fatal => {
                if let Err(e) = read {
                    tracing::error!(tab_id, ?e, "PTY read 非预期错误,停止 daemon");
                }
                data.loop_signal.stop();
                return Ok(PostAction::Remove);
            }
        }
    }
}

/// listener readable:accept 到 WouldBlock,每个新连接发一帧当前快照。
fn accept_ready(listener: &UnixListener, data: &mut DaemonData) -> io::Result<PostAction> {
    loop {
        match listener.accept() {
            Ok((stream, _addr)) => {
                if let Err(e) = serve_snapshot(stream, data) {
                    tracing::warn!(?e, "新客户端首帧快照下发失败");
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
/// **仅 unix socket 路径用**(行分隔)。WS 路径走帧分隔,不加换行(见 [`broadcast_snapshot`])。
fn snapshot_line(snap: &Snapshot) -> Result<Vec<u8>> {
    let mut bytes = serde_json::to_vec(snap).context("序列化 Snapshot 为 JSON 失败")?;
    bytes.push(b'\n');
    Ok(bytes)
}

/// 取 active tab 快照 → 序列化成裸 JSON(无换行,WS 帧已自分隔)→ 经 `snap_tx`
/// 推给 WS 线程 → 清 dirty(与置脏对称,防每 tick 重发)。
///
/// **跨线程边界只过 `Vec<u8>`**(ADR-0015 头号约束):`Session` 含 `Rc`,`!Send`,
/// 序列化在本(calloop)线程做完。`snap_tx.send` 失败仅发生在 WS 子系统已关停
/// (daemon 即将退出)时,best-effort 忽略。线缆体保持**裸 `Snapshot`**(与
/// `quill-dump` / unix 路径一致;`ServerMsg` 信封等 T3b 发 Workspace 帧时再切)。
fn broadcast_snapshot(data: &mut DaemonData, tab_id: u64) {
    let snap = data.session.snapshot_active();
    match serde_json::to_vec(&snap) {
        Ok(bytes) => {
            let _ = data.snap_tx.send(bytes);
            data.session.clear_dirty(tab_id);
        }
        Err(e) => tracing::warn!(tab_id, ?e, "广播快照:序列化失败,跳过本帧"),
    }
}

/// 同步 WebSocket 服务(ADR-0016)。两条 `std::thread`:
/// - **updater**:`recv()` calloop 线程发来的快照字节,存进 `latest`(共享「最新
///   帧」)。`snap_tx` 全丢弃后 `recv()` 返 Err → 线程退出。
/// - **acceptor**:非阻塞轮询 `TcpListener::accept()`,每个新连接起一个**短命**
///   线程做 tungstenite 握手 + 发一帧 `latest` + 关闭(慢客户端不阻塞 accept)。
///
/// 本切片只「连上发一次最新快照」;直播(dirty 增量持续推)留 T3b。
struct WsServer {
    shutdown: Arc<AtomicBool>,
    acceptor: Option<JoinHandle<()>>,
    updater: Option<JoinHandle<()>>,
}

/// WS 线程共享的「最新一帧」字节(`None` = 还没有任何快照)。
type LatestFrame = Arc<Mutex<Option<Vec<u8>>>>;

impl WsServer {
    /// 绑 TCP + spawn updater/acceptor 线程。**调用方须保证已在 `Signals::new` 之后**
    /// 调用(线程继承已 block 的 SIGINT/SIGTERM mask;见 [`run`] 信号顺序注释)。
    fn spawn(bind: SocketAddr, snap_rx: Receiver<Vec<u8>>) -> Result<Self> {
        let listener =
            TcpListener::bind(bind).with_context(|| format!("WS TcpListener::bind {bind} 失败"))?;
        // 非阻塞 + 轮询关停:让 acceptor 能周期检查 shutdown 标志而非永久阻塞在
        // accept() 上(否则退出时 join 不回来)。
        listener
            .set_nonblocking(true)
            .context("WS TcpListener 设非阻塞失败")?;

        let latest: LatestFrame = Arc::new(Mutex::new(None));
        let shutdown = Arc::new(AtomicBool::new(false));

        let latest_u = Arc::clone(&latest);
        let updater = thread::Builder::new()
            .name("quill-ws-update".to_string())
            .spawn(move || {
                while let Ok(bytes) = snap_rx.recv() {
                    set_latest(&latest_u, bytes);
                }
            })
            .context("spawn WS updater 线程失败")?;

        let latest_a = Arc::clone(&latest);
        let shutdown_a = Arc::clone(&shutdown);
        let acceptor = thread::Builder::new()
            .name("quill-ws-accept".to_string())
            .spawn(move || ws_accept_loop(listener, latest_a, shutdown_a))
            .context("spawn WS acceptor 线程失败")?;

        tracing::info!(%bind, "WS 服务就绪 (tungstenite, ws://)");
        Ok(Self {
            shutdown,
            acceptor: Some(acceptor),
            updater: Some(updater),
        })
    }

    /// 置关停标志 + join 两个线程(干净收尾)。调用前应已 drop `snap_tx`,否则
    /// updater 的 `recv()` 不会返回、join 会卡住。
    fn shutdown(mut self) {
        self.shutdown.store(true, Ordering::SeqCst);
        if let Some(h) = self.acceptor.take() {
            let _ = h.join();
        }
        if let Some(h) = self.updater.take() {
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

/// Mutex 上锁不 panic:中毒(别的线程持锁时 panic)也恢复内部值继续用 —— 这里只
/// 是覆盖「最新帧」,旧值无所谓一致性,恢复即可(CLAUDE.md 禁 unwrap/expect)。
fn set_latest(latest: &LatestFrame, bytes: Vec<u8>) {
    let mut guard = latest
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    *guard = Some(bytes);
}

/// 读「最新帧」的克隆(同上,锁中毒可恢复)。
fn latest_clone(latest: &LatestFrame) -> Option<Vec<u8>> {
    latest
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone()
}

/// acceptor 线程主体:非阻塞轮询 accept,每连接起短命线程发一帧。
fn ws_accept_loop(listener: TcpListener, latest: LatestFrame, shutdown: Arc<AtomicBool>) {
    while !shutdown.load(Ordering::SeqCst) {
        match listener.accept() {
            Ok((stream, peer)) => {
                let latest = Arc::clone(&latest);
                // 短命 per-conn 线程:握手 + 发一帧可能阻塞(弱网),不能卡住 accept。
                // 一次性发完即退,无需 join(进程退出兜底)。
                if let Err(e) = thread::Builder::new()
                    .name("quill-ws-conn".to_string())
                    .spawn(move || serve_ws_connection(stream, latest))
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

/// 单连接:tungstenite 握手 → 发一帧最新快照(text)→ 关闭。
fn serve_ws_connection(stream: TcpStream, latest: LatestFrame) {
    // 限握手 + 写的阻塞时长,防静默/弱网客户端把本线程挂死。
    let _ = stream.set_read_timeout(Some(WS_CONN_READ_TIMEOUT));
    let _ = stream.set_write_timeout(Some(WS_CONN_READ_TIMEOUT));
    let mut ws = match tungstenite::accept(stream) {
        Ok(ws) => ws,
        Err(e) => {
            tracing::debug!(?e, "WS 握手失败");
            return;
        }
    };
    match latest_clone(&latest) {
        Some(bytes) => match String::from_utf8(bytes) {
            // JSON 必是合法 UTF-8;from_utf8 仅防御性。
            Ok(text) => {
                if let Err(e) = ws.send(Message::text(text)) {
                    tracing::debug!(?e, "WS 发送快照失败");
                }
                let _ = ws.flush();
            }
            Err(e) => tracing::warn!(?e, "WS 快照字节非 UTF-8,跳过"),
        },
        None => tracing::debug!("WS 连接时尚无快照可发"),
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
}
