//! 无头会话内核 daemon 的 calloop 接线 (Phase 7 T2 / 片1 / 片2, ADR-0015 + 0016)。
//!
//! T1 ([`crate::kernel::session`]) 给了纯数据层 [`Session`];这里把它挂到一个
//! **单线程** `calloop::EventLoop` 上 —— **所有 fd(PTY / unix socket / 信号 / WS
//! TCP listener / 每条 WS 连接)都注册成同一个 loop 的源**,没有任何 `std::thread`
//! (INV-001:绝不为 IO 起线程池)。`Session` 含 `Rc<RefCell<…>>` 非 `Send`,正因
//! 全程钉在这一个线程,**不需要 Arc / 锁 / channel 桥**:PTY 出字节直接 fan-out 到
//! 各 WS 连接,WS 入字节直接 [`Session::on_input`] 写 PTY。
//!
//! **WS 子系统(片1 字节流直播 + 片2 输入回灌,全 calloop 化,ADR-0016 Alt 3)**:
//! - **listener**:`TcpListener` 注册成 `Generic` READ 源;可读→`accept` 排空→每个
//!   新连接设非阻塞 + 注册成它自己的 `Generic` READ 源。
//! - **同口 HTTP / WS 分流**:连接 READ 源可读时把请求头**消费**(`read`,非 `peek`)进
//!   per-conn 累积缓冲,头收全后分流:带 `Upgrade: websocket` 走 WS 握手(已消费的请求
//!   字节经 [`PrefixStream`] "退还"给 tungstenite 从头读完整握手),否则当普通 HTTP
//!   (serve 内嵌 xterm.js 页 / vendored 资产 / 404),响应排进一个非阻塞写源写完即关。
//!   **为何消费而非 peek**:READ 源是 `Mode::Level`,`peek`(MSG_PEEK)不排空内核缓冲 →
//!   半截头时 fd 恒可读 → calloop 每轮 ~0 超时重派 → 烧满一个核(实测 99.3%);消费式
//!   读把 fd 排空,Level 不再恒触发,半截头静默等下次新字节(不忙等、不 sleep)。
//! - **非阻塞握手**:`tungstenite` 的 `MidHandshake` 状态机 —— fd 再可读时
//!   `.handshake()` 续做,直到 `Ok(WebSocket)`(不阻塞 loop)。
//! - **输出(PTY → 浏览器)**:`pty_readable` 读到的裸字节既喂 [`Session::on_pty_output`]
//!   (维持服务端 term,供 unix-dump 取网格快照),又 append 进 [`ByteRing`] +
//!   fan-out 进每条 live 连接的出站队列,并按需打开其 WRITE 兴趣。连接可写→排空
//!   出站队列(非阻塞 write/flush)→排空后把 WRITE 兴趣关掉退回只 READ(**不忙等**)。
//!   新连接握手完成先把环缓冲重放排进出站(重建当前屏),之后跟 live 字节。
//! - **输入(浏览器 → PTY)**:连接 READ 源可读→`ws.read()` 排空→每条数据帧字节直接
//!   [`Session::on_input`] 写 active tab 的 PTY(同线程,无 channel)。
//! - **背压**:字节流非幂等(丢任一字节 = VT 状态机错位),故**绝不丢/合帧**;某条
//!   连接出站积压超 [`WS_CLIENT_OUT_CAP`] 即**断开该连接**(remove 源 + 关 fd 回收,
//!   它重连从环缓冲重放恢复)。
//! - **死客户端回收**:对端关闭 → 该连接 READ 源拿到 EOF/Close → 立即回收(单线程下
//!   无需"空闲也轮询剪除",结构性消灭了线程版的死 tx 泄漏)。
//! - **卡握手 / 半开连接收割**:一个周期 [`Timer`] 源(`reap_stale_clients`)扫描所有连接,
//!   回收卡在 Peeking/Handshaking 阶段超 `handshake_deadline`(自 accept 起的绝对期限,
//!   robust against slowloris 蚂蚁搬家)的连接;**live 连接不在收割列**(健康空闲会被误杀),
//!   其网络静默掉线(无 FIN/RST)的半开态靠 accept 时设的 `SO_KEEPALIVE` 探活 → 内核报错 →
//!   READ 源走既有 EOF/错误回收路径。
//!
//! **仍留后续 ticket**:多 tab 动态增删 + 输入按 tab 寻址 + resize 协商(T6)。

use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::io::{self, Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::os::fd::{AsRawFd, BorrowedFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use calloop::generic::Generic;
use calloop::signals::{Signal, Signals};
use calloop::timer::{TimeoutAction, Timer};
use calloop::{EventLoop, Interest, LoopHandle, LoopSignal, Mode, PostAction, RegistrationToken};
use tungstenite::handshake::server::{NoCallback, ServerHandshake};
use tungstenite::handshake::MidHandshake;
use tungstenite::{Bytes, Error as WsError, HandshakeError, Message, WebSocket};

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
/// (一个 PTY 一个尺寸,ADR-0015 "主控端定尺寸";多客户端尺寸协商留 T6)。
pub const DEFAULT_COLS: u16 = 80;
/// 见 [`DEFAULT_COLS`]。
pub const DEFAULT_ROWS: u16 = 24;

/// 默认 socket 文件名 (挂在 `$XDG_RUNTIME_DIR` 下)。
pub const DEFAULT_SOCKET_NAME: &str = "quill-kernel.sock";

/// PTY 单次 read 缓冲。与 `wl::window` 的 `PTY_READ_BUF` 同值。
const PTY_READ_BUF: usize = 4096;

/// WS 监听默认端口。
pub const DEFAULT_WS_PORT: u16 = 7878;

/// 同时在册的 WS 连接上限(Peeking / Handshaking / Live 合计)。收口 T3a 遗留「每连接
/// 无上限起线程」:单线程下退化成「在册连接源数」上限,超限直接 close 新连接
/// (daily-drive 单用户,个位数客户端,16 富余)。
const MAX_WS_CONNS: usize = 16;

/// 每个 WS 客户端出站积压字节上限。超过即断开该客户端(它重连从环缓冲重放恢复)。
///
/// **字节流非幂等**:不能像网格全量快照那样「队满丢旧帧」(丢任一字节 = VT 状态机
/// 错位/乱码)。故积压超上限时**断开该客户端**,绝不丢字节。1 MiB 远超正常排空所需
/// (快客户端出站队列常空),只有真跟不上的慢客户端(弱网卡死 / 完全不读)才会触顶
/// 被断,不无界堆内存、不拖累别的客户端。
const WS_CLIENT_OUT_CAP: usize = 1 << 20;

/// 每个 PTY 字节流环缓冲容量。新客户端连上重放此环重建当前屏 + 一段近期 scrollback。
///
/// 256 KiB:远超「重建当前屏」所需(80×24 文本 ~2KB,带 VT 转义放大也就几十 KB),
/// 留足近期上下文;丢的永远是**最旧**字节,故最近输出(当前可见屏)恒完整。单 tab
/// 一份,daily-drive tab 数个位数,内存预算可忽略(对照 term scrollback 100K 行)。
const BYTE_RING_CAP: usize = 256 * 1024;

/// 同口分流时累积 HTTP 请求头的上限(防超长头吃内存;正常请求头远小于此)。到顶仍未
/// 收全则按已收字节强制分流(多半非合法 upgrade → 当 HTTP 处理后关闭)。
const HTTP_PEEK_MAX: usize = 8192;

/// 握手收割 Timer 的扫描周期。
const REAP_INTERVAL_MS: u64 = 5_000;

/// 连接从 accept 起必须在此期限内完成握手(转 Live),否则被收割。绝对期限(非"距上次
/// 活动"),故 slowloris 蚂蚁搬家式逐字节拖延也逃不掉。正常握手 ms 级,10s 极宽松。
const HANDSHAKE_DEADLINE_MS: u64 = 10_000;

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

/// daemon 主循环。spawn shell tab、绑 socket、注册全部 fd 源、`run` 阻塞到收信号 /
/// shell 退出,返回前清理 socket 文件。
pub fn run(config: DaemonConfig) -> Result<()> {
    let mut event_loop: EventLoop<'static, DaemonData> =
        EventLoop::try_new().context("calloop EventLoop::try_new 失败")?;
    let loop_handle = event_loop.handle();
    let loop_signal = event_loop.get_signal();

    // Source 1:SIGINT + SIGTERM → 停 loop(退出后清理 socket)。
    // **必须早于任何 spawn 线程的代码**:`calloop::Signals::new` 只在*当前(主)
    // 线程* block 这两个信号(`pthread_sigmask`)。紧接着的 `TabInstance::spawn`
    // → `ComposerState` → completion `WorkerPool` 会起几个工作线程(WS 已无线程,
    // 但 completion 仍有),它们继承主线程**此刻**的 signal mask 才会一起 block;
    // 否则 SIGTERM 落到某个未 block 的 worker 线程 → 走默认 terminate → 跳过下方
    // socket 清理(实测 exit 143)。calloop signals doc 原话:"set up the signal
    // event source before spawning any thread"。
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

    // 先绑 WS TCP listener(端口占用等错早退,此刻还没落 unix socket 文件,无残留可清),
    // 再建 unix socket。WS 全程在本 calloop 线程,**无 channel / 无线程**(去掉了 ADR-0016
    // 线程版的 mpsc + WsServer)。
    let ws_listener = TcpListener::bind(config.ws_bind)
        .with_context(|| format!("WS TcpListener::bind {} 失败", config.ws_bind))?;
    // INV:Level 模式下阻塞 accept 会 stall 整个单线程 loop → 必须非阻塞 + accept 到
    // WouldBlock 排空。
    ws_listener
        .set_nonblocking(true)
        .context("WS TcpListener 设非阻塞失败")?;

    prepare_socket_path(&config.socket_path)?;
    let listener = UnixListener::bind(&config.socket_path)
        .with_context(|| format!("bind UnixListener {} 失败", config.socket_path.display()))?;
    listener
        .set_nonblocking(true)
        .context("UnixListener 设非阻塞失败")?;

    // 收割参数:默认生产值,可经 env 覆盖(调参 / 测试用,无新依赖、不改 CLI)。
    let reap_interval = duration_from_env("QUILL_WS_REAP_MS", REAP_INTERVAL_MS);
    let handshake_deadline =
        duration_from_env("QUILL_WS_HANDSHAKE_DEADLINE_MS", HANDSHAKE_DEADLINE_MS);

    let mut data = DaemonData {
        session,
        loop_handle: loop_handle.clone(),
        loop_signal,
        tab_id,
        ring: ByteRing::new(BYTE_RING_CAP),
        clients: HashMap::new(),
        next_client_id: 1,
        handshake_deadline,
    };

    // Source 2:PTY master fd。
    let pty_fd = data.session.tabs().active().pty().raw_fd();
    // SAFETY:
    // - pty_fd 来自 `PtyHandle::raw_fd()`(构造时 `as_raw_fd().ok_or_else` 校验过一次),
    //   PtyHandle 在 `data.session.tabs` 里,`run()` scope 内全程活着。
    // - drop 序:函数尾显式 `drop(event_loop)` 早于 `drop(data)`,即 Generic source 的
    //   `EPOLL_CTL_DEL` 在 pty fd 仍打开时执行;即便顺序错,calloop 0.14 对已关 fd 的
    //   `EPOLL_CTL_DEL` 返 EBADF 内部容忍(`wl/window.rs` 同源),非 UB。
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

    // Source 3:UnixListener(quill-dump 调试路径,与 WS 字节流独立)。owned 直接交给
    // Generic —— UnixListener 实现 AsFd,calloop 持所有权并在 source drop 时关闭。
    loop_handle
        .insert_source(
            Generic::new(listener, Interest::READ, Mode::Level),
            |_readiness, meta, data: &mut DaemonData| accept_ready(meta.as_ref(), data),
        )
        .map_err(|e| anyhow!("calloop insert_source(unix listener) 失败: {e}"))?;

    // Source 4:WS TCP listener。owned 交 Generic;可读→accept 新连接注册成各自的源。
    loop_handle
        .insert_source(
            Generic::new(ws_listener, Interest::READ, Mode::Level),
            |_readiness, meta, data: &mut DaemonData| ws_accept_ready(meta.as_ref(), data),
        )
        .map_err(|e| anyhow!("calloop insert_source(ws listener) 失败: {e}"))?;

    // Source 5:握手收割 Timer。周期扫描,回收卡在 Peeking/Handshaking 阶段超
    // `handshake_deadline` 的连接(防半截头/卡握手长占 `clients` 槽 + slowloris)。live
    // 连接不在此列(健康空闲会被误杀)——其半开靠 accept 时设的 SO_KEEPALIVE 探活回收。
    // 收割时只 `loop_handle.remove` 别的源(连接的 READ/WRITE 源),不 remove 本 Timer
    // 自己(返 `ToDuration` 让 calloop 自动重排,calloop 禁止回调内 remove 自身源)。
    loop_handle
        .insert_source(
            Timer::from_duration(reap_interval),
            move |_deadline, _meta, data: &mut DaemonData| {
                reap_stale_clients(data);
                TimeoutAction::ToDuration(reap_interval)
            },
        )
        .map_err(|e| anyhow!("calloop insert_source(reap timer) 失败: {e}"))?;

    tracing::info!(
        socket = %config.socket_path.display(),
        ws_bind = %config.ws_bind,
        tab_id,
        cols = config.cols,
        rows = config.rows,
        "quill-kernel daemon 就绪 (单线程 calloop:PTY + unix + WS 同一 loop)"
    );

    let run_result = event_loop
        .run(None, &mut data, |_data| {})
        .context("calloop EventLoop::run 失败");

    // 显式 drop 序:event_loop(持各 Generic source,含 WS 连接源 + PTY BorrowedFd 源)
    // 先于 data(持 PtyHandle 的 fd / WS WebSocket 的 fd),让源的 EPOLL_CTL_DEL 在对应
    // fd 仍打开时执行(见上 SAFETY)。WS 无线程可 join,drop 即收尾。
    drop(event_loop);
    drop(data);

    remove_socket_quiet(&config.socket_path);

    run_result
}

/// 单线程 daemon 的 calloop `Data`。callback 拿 `&mut DaemonData` 走字段 split
/// borrow(与 `wl::window::LoopData` 同模式)。**WS 全在本线程,故 ring / clients
/// 都是普通字段,无 Arc / 锁 / channel。**
struct DaemonData {
    session: Session,
    /// 运行期注册 / 注销源(WS 连接的 READ/WRITE 源、unix 客户端写源)用。
    loop_handle: LoopHandle<'static, DaemonData>,
    loop_signal: LoopSignal,
    /// 本切片唯一 tab 的 raw id(协议层 `u64`,INV-010 不能从 `u64` 重建 TabId)。
    tab_id: u64,
    /// PTY 原始字节环缓冲(连上重放重建当前屏)。单线程,直接 fan-out 进各连接出站队列。
    ring: ByteRing,
    /// 在册 WS 连接(Peeking / Handshaking / Live)。key = 自增连接 id。
    clients: HashMap<u64, WsClient>,
    /// WS 连接 id 自增源。
    next_client_id: u64,
    /// 连接从 accept 起到完成握手的绝对期限;超期且未转 Live 的连接被收割 Timer 回收。
    handshake_deadline: Duration,
}

/// 一个 unix 客户端连接待写出的快照字节 + 已写偏移(非阻塞 write 可能分多次)。
/// HTTP 响应写源也复用此结构。
struct WritePending {
    buf: Vec<u8>,
    written: usize,
}

/// 一条 WS 连接的全部 per-client 状态(**全在 calloop 线程,无 `Send` 要求**)。
struct WsClient {
    stage: WsStage,
    /// 连接 accept 时刻;收割 Timer 用它判握手是否超 `handshake_deadline`(绝对期限)。
    created_at: Instant,
    /// 本连接 READ 源 token(可读:读头分流 / 续握手 / 收输入)。源 owns 一份 TcpStream
    /// (accept 出来的"original"),纯当可读信号 + 读头句柄(消费式累积请求头);真正的 WS
    /// IO 走 [`WsStage`] 内 `WebSocket` 持有的那一份 dup。
    read_token: RegistrationToken,
    /// 本连接 WRITE 源(只在 Live 阶段存在)。出站队列空时 disable(退回只 READ,不忙等),
    /// 有积压时 enable。源 owns 另一份 dup,纯当可写信号。
    write: Option<WriteReg>,
    /// 待写出的 PTY 字节(`Bytes` 多客户端共享同一帧不拷贝)。
    outbound: VecDeque<Bytes>,
    /// `outbound` 当前总字节(背压 cap 判定;`Bytes::len` 求和的缓存)。
    outbound_len: usize,
}

/// WRITE 源注册信息。`armed` = 该源当前是否处于 enable 状态(epoll 已注册),用来防止
/// 重复 enable(已 enable 再 register 会 EEXIST)。
struct WriteReg {
    token: RegistrationToken,
    armed: bool,
}

/// 一条 WS 连接的握手/直播阶段。
enum WsStage {
    /// 刚 accept,尚未分流:READ 源可读时把请求头**消费**进内含 `Vec`(累积缓冲),收全后
    /// 判 HTTP / WS。消费(非 peek)→ 排空内核缓冲 → `Mode::Level` 半截头不再恒触发(消灭
    /// 忙等)。
    Peeking(Vec<u8>),
    /// WS 握手中:非阻塞 `MidHandshake`,fd 再可读时 `.handshake()` 续做。`Option` 是为
    /// 了 `take()` 出来调 `handshake(self)`(按值消费),`Interrupted` 时把推进后的存回。
    /// 流是 [`PrefixStream`](已消费的请求字节 + socket dup),tungstenite 从头读完整握手。
    Handshaking(Option<MidHandshake<ServerHandshake<PrefixStream, NoCallback>>>),
    /// 握手完成,长命直播:`WebSocket` 持有 [`PrefixStream`](握手后前缀已耗尽 ≈ 裸 dup)。
    Live(WebSocket<PrefixStream>),
}

/// 同口分流时把**已消费进缓冲**的握手请求字节"退还"给 tungstenite 的链式流。
///
/// 忙等修复改成**消费式**读请求头(见 [`ws_read_head`]),WS 分支因此手里握着完整握手
/// 请求的字节(已从内核缓冲取走);tungstenite 的 `ServerHandshake::start` 要从一个流里
/// 读这些字节。`PrefixStream::read` 先吐 `prefix`(没吐完的请求字节)、耗尽后回落到底层
/// `TcpStream`(同一 OFD 的 dup,握手后的 WS 帧从这读);`write` 全程直委托底层流(101
/// 响应 + 之后的出站帧)。握手完成后 prefix 已耗尽,等价于裸 `TcpStream`(每次 read 仅多
/// 一次游标比较),故可安全沿用为 Live 阶段的流类型。
struct PrefixStream {
    prefix: io::Cursor<Vec<u8>>,
    stream: TcpStream,
}

impl PrefixStream {
    fn new(prefix: Vec<u8>, stream: TcpStream) -> Self {
        Self {
            prefix: io::Cursor::new(prefix),
            stream,
        }
    }

    /// 底层 socket(WRITE 源建 dup / 取 fd 用)。
    fn tcp(&self) -> &TcpStream {
        &self.stream
    }
}

impl io::Read for PrefixStream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let pos = self.prefix.position();
        let total = self.prefix.get_ref().len() as u64;
        if pos < total {
            // 前缀还没吐完:本次只从前缀读(不跨界到底层流,保持顺序简单)。
            return self.prefix.read(buf);
        }
        self.stream.read(buf)
    }
}

impl io::Write for PrefixStream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.stream.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.stream.flush()
    }
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
/// (WouldBlock / EOF / 错误)把整批字节 [`fan_out_bytes`] 进环缓冲 + 各 live WS 连接。
///
/// **字节流逐 chunk 不可丢/不可去重**:一次 readiness 内多 chunk **拼接**成一批一次性
/// fan-out(少分帧,字节等价),顺序 = PTY 出字节顺序;跨 readiness 顺序由 calloop 串行
/// 保证。子 shell 退出(EOF/EIO)→ 收尸 + 停 loop(单 tab 切片:shell 死即 daemon 退);
/// 退出前 flush 已读出的尾部字节(字节流不可丢)。
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
                    data.session.on_pty_output(tab_id, &buf[..n]);
                    batch.extend_from_slice(&buf[..n]);
                }
            }
            PtyRead::Retry => continue,
            PtyRead::Drained => {
                if !batch.is_empty() {
                    fan_out_bytes(data, batch);
                }
                return Ok(PostAction::Continue);
            }
            PtyRead::Closed => {
                tracing::info!(tab_id, "PTY EOF/EIO:子 shell 退出,停止 daemon");
                if !batch.is_empty() {
                    fan_out_bytes(data, batch);
                }
                let _ = data.session.tabs_mut().active_mut().pty_mut().try_wait();
                data.loop_signal.stop();
                return Ok(PostAction::Remove);
            }
            PtyRead::Fatal => {
                if let Err(e) = read {
                    tracing::error!(tab_id, ?e, "PTY read 非预期错误,停止 daemon");
                }
                if !batch.is_empty() {
                    fan_out_bytes(data, batch);
                }
                data.loop_signal.stop();
                return Ok(PostAction::Remove);
            }
        }
    }
}

/// 把一批 PTY 字节 append 进环缓冲,并 fan-out 进每条 live WS 连接的出站队列(打开其
/// WRITE 兴趣)。某连接积压超 [`WS_CLIENT_OUT_CAP`] → 断开它(重连重放恢复)。
///
/// Peeking / Handshaking 阶段的连接**不**入队 live 字节:它们握手完成时会先收环缓冲
/// 重放(此刻 append 的字节已在环里),不会丢也不会双发(单线程串行保证「ring.snapshot
/// + 订阅」相对本函数原子)。
fn fan_out_bytes(data: &mut DaemonData, batch: Vec<u8>) {
    data.ring.push(&batch);
    if data.clients.is_empty() {
        return;
    }
    // move 进 Bytes(refcount);各 live 连接 clone 只 +1 引用,不拷贝字节。
    let frame = Bytes::from(batch);
    let ids: Vec<u64> = data.clients.keys().copied().collect();
    for id in ids {
        let mut is_live = false;
        let mut over_cap = false;
        if let Some(c) = data.clients.get_mut(&id) {
            if matches!(c.stage, WsStage::Live(_)) {
                is_live = true;
                c.outbound.push_back(frame.clone());
                c.outbound_len += frame.len();
                over_cap = c.outbound_len > WS_CLIENT_OUT_CAP;
            }
        }
        if over_cap {
            tracing::warn!(
                id,
                cap = WS_CLIENT_OUT_CAP,
                "WS 客户端出站积压超上限,断开(重连重放恢复)"
            );
            drop_client_external(data, id);
        } else if is_live {
            arm_write(data, id);
        }
    }
}

/// WS TCP listener readable:accept 到 WouldBlock,每个新连接接入(设非阻塞 + 注册 READ 源)。
fn ws_accept_ready(listener: &TcpListener, data: &mut DaemonData) -> io::Result<PostAction> {
    loop {
        match listener.accept() {
            Ok((stream, _peer)) => {
                if let Err(e) = accept_ws_client(stream, data) {
                    tracing::warn!(?e, "WS 新连接接入失败");
                }
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(PostAction::Continue),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => {
                tracing::warn!(?e, "WS accept 失败");
                return Ok(PostAction::Continue);
            }
        }
    }
}

/// 接入一条新 WS/HTTP 连接:[`MAX_WS_CONNS`] 闸门 → 设非阻塞 + SO_KEEPALIVE → 注册成 READ
/// 源(owns stream,纯当可读信号 + 读头句柄)→ 进 `clients` 登记为 Peeking。
fn accept_ws_client(stream: TcpStream, data: &mut DaemonData) -> Result<()> {
    if data.clients.len() >= MAX_WS_CONNS {
        tracing::warn!(max = MAX_WS_CONNS, "WS 连接数达上限,拒绝新连接");
        let _ = stream.shutdown(Shutdown::Both);
        return Ok(());
    }
    stream
        .set_nonblocking(true)
        .context("WS stream 设非阻塞失败")?;
    // 半开探活硬化:网络静默掉线(无 FIN/RST)的 live 连接靠 TCP keepalive 让内核探测对端
    // 死活 → 死则 socket 报错 → epoll 唤醒 READ 源走既有回收路径。比"按空闲超时收割 live"
    // 安全(不误杀健康但安静的终端)。尽力而为,失败不影响主功能。
    enable_keepalive(&stream);
    let id = data.next_client_id;
    data.next_client_id = data.next_client_id.wrapping_add(1);
    let token = data
        .loop_handle
        .insert_source(
            Generic::new(stream, Interest::READ, Mode::Level),
            move |_readiness, meta, data: &mut DaemonData| {
                ws_client_readable(data, id, meta.as_ref())
            },
        )
        .map_err(|e| anyhow!("calloop insert_source(ws client read) 失败: {e}"))?;
    data.clients.insert(
        id,
        WsClient {
            stage: WsStage::Peeking(Vec::new()),
            created_at: Instant::now(),
            read_token: token,
            write: None,
            outbound: VecDeque::new(),
            outbound_len: 0,
        },
    );
    Ok(())
}

/// WS 连接 READ 源可读:按阶段分发。`original` = 该 READ 源 own 的 TcpStream(读头/可读信号)。
fn ws_client_readable(
    data: &mut DaemonData,
    id: u64,
    original: &TcpStream,
) -> io::Result<PostAction> {
    match data.clients.get(&id).map(stage_tag) {
        Some(StageTag::Peeking) => ws_read_head(data, id, original),
        Some(StageTag::Handshaking) => ws_drive_handshake(data, id),
        Some(StageTag::Live) => ws_live_read(data, id),
        None => Ok(PostAction::Remove),
    }
}

/// 阶段判别(Copy 标签,避免在分发时长借 `data.clients`)。
#[derive(Clone, Copy)]
enum StageTag {
    Peeking,
    Handshaking,
    Live,
}

fn stage_tag(c: &WsClient) -> StageTag {
    match c.stage {
        WsStage::Peeking(_) => StageTag::Peeking,
        WsStage::Handshaking(_) => StageTag::Handshaking,
        WsStage::Live(_) => StageTag::Live,
    }
}

/// Peeking:把请求头**消费**(`read`)进 per-conn 累积缓冲(`WsStage::Peeking` 内的 `Vec`),
/// 头收全后分流。**消费而非 peek 是忙等修复的关键**:READ 源 `Mode::Level` 下,`peek`
/// (MSG_PEEK)不排空内核缓冲 → 半截头时 fd 恒可读 → calloop 每轮 ~0 超时重派本函数 →
/// 烧满一个核;消费式读把 fd 读空,半截头时返 `WouldBlock` → `Continue` 静默等下次新字节
/// (**不 sleep、不忙等**)。
///
/// WS 分支:已消费的完整握手请求字节经 [`PrefixStream`] "退还"给 tungstenite(它先读完
/// 前缀再回落到 socket dup),从头读到完整握手 → 写 101 → 转 Live。
fn ws_read_head(data: &mut DaemonData, id: u64, original: &TcpStream) -> io::Result<PostAction> {
    let mut reader: &TcpStream = original;
    loop {
        let acc_len = match data.clients.get(&id) {
            Some(WsClient {
                stage: WsStage::Peeking(buf),
                ..
            }) => buf.len(),
            // 阶段已变 / 连接已没(不该在本路径发生):交回 loop。
            _ => return Ok(PostAction::Continue),
        };
        if acc_len >= HTTP_PEEK_MAX {
            // 头超长仍未收全:按已收字节强制分流(防超长头吃内存)。
            break;
        }
        let want = (HTTP_PEEK_MAX - acc_len).min(4096);
        let mut chunk = [0u8; 4096];
        match reader.read(&mut chunk[..want]) {
            Ok(0) => return Ok(drop_client_read_self(data, id)), // 对端没发就关了
            Ok(n) => {
                let done = match data.clients.get_mut(&id) {
                    Some(WsClient {
                        stage: WsStage::Peeking(buf),
                        ..
                    }) => {
                        buf.extend_from_slice(&chunk[..n]);
                        header_complete(buf)
                    }
                    _ => return Ok(PostAction::Continue),
                };
                if done {
                    break;
                }
                // 没收全:继续读(内核可能还有更多字节),直到收全 / WouldBlock / 超长。
            }
            // 半截头:已消费的字节进了缓冲,fd 排空 → Level 不再恒触发 → 静默等下次可读。
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(PostAction::Continue),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => {
                tracing::debug!(?e, "WS 读请求头失败");
                return Ok(drop_client_read_self(data, id));
            }
        }
    }

    // 头收全(或超长强制):取出累积字节分流。
    let head = match data.clients.get(&id) {
        Some(WsClient {
            stage: WsStage::Peeking(buf),
            ..
        }) => buf.clone(),
        _ => return Ok(PostAction::Continue),
    };

    if request_is_ws_upgrade(&head) {
        // WS:转 Handshaking。tungstenite 要 own 一个流 → 给它 [`PrefixStream`]:已消费的
        // 请求字节(head)当前缀 + original 的 dup(同一 OFD,握手后的 WS 帧从这读)。
        let io_stream = match original.try_clone() {
            Ok(s) => s,
            Err(e) => {
                tracing::debug!(?e, "WS clone IO stream 失败");
                return Ok(drop_client_read_self(data, id));
            }
        };
        let mid = ServerHandshake::start(PrefixStream::new(head, io_stream), NoCallback, None);
        if let Some(c) = data.clients.get_mut(&id) {
            c.stage = WsStage::Handshaking(Some(mid));
        }
        // 立即驱一轮握手(请求字节已在前缀里,多半一轮完成)。
        ws_drive_handshake(data, id)
    } else {
        // HTTP:在 original 的 dup 上排响应写完即关;READ 源自删(关 original)。dup 与
        // original 是不同 fd 号,故同时注册不会 EPOLL_CTL_ADD EEXIST。
        match original.try_clone() {
            Ok(http_stream) => serve_http_response(data, http_stream, &head),
            Err(e) => tracing::debug!(?e, "HTTP clone stream 失败"),
        }
        // Peeking 阶段还没 WRITE 源,直接摘登记 + 自删 READ 源。
        data.clients.remove(&id);
        Ok(PostAction::Remove)
    }
}

/// 续做非阻塞 WS 握手:`take` 出 `MidHandshake`(`handshake(self)` 按值消费),`Interrupted`
/// 存回推进后的状态等下次可读;`Ok` 转 Live(建 WRITE 源 + 排环缓冲重放);`Failure` 断开。
fn ws_drive_handshake(data: &mut DaemonData, id: u64) -> io::Result<PostAction> {
    let mid = match data.clients.get_mut(&id) {
        Some(WsClient {
            stage: WsStage::Handshaking(slot),
            ..
        }) => slot.take(),
        _ => return Ok(PostAction::Continue),
    };
    let Some(mid) = mid else {
        // 不该发生(slot 被取空又没存回);防御性保留连接等下次。
        return Ok(PostAction::Continue);
    };
    match mid.handshake() {
        Ok(ws) => ws_go_live(data, id, ws),
        Err(HandshakeError::Interrupted(m)) => {
            if let Some(WsClient {
                stage: WsStage::Handshaking(slot),
                ..
            }) = data.clients.get_mut(&id)
            {
                *slot = Some(m);
            }
            Ok(PostAction::Continue)
        }
        Err(HandshakeError::Failure(e)) => {
            tracing::debug!(?e, "WS 握手失败,断开");
            Ok(drop_client_read_self(data, id))
        }
    }
}

/// 握手完成转 Live:为出站建一个 WRITE 源(owns 另一份 dup 当可写信号)+ 把当前环缓冲
/// 重放排进出站(重建当前屏)。
///
/// **重放 + 订阅原子性**:单线程串行,本函数与 [`fan_out_bytes`] 绝不交错 —— `ring.snapshot()`
/// 之后到达的字节都在本连接转 Live 之后由 fan-out 经出站送达(无丢);之前的都在重放里
/// (无双发)。
fn ws_go_live(
    data: &mut DaemonData,
    id: u64,
    ws: WebSocket<PrefixStream>,
) -> io::Result<PostAction> {
    // WRITE 源借另一份 dup(与 READ 源的 original、ws 的 IO dup 都不同 fd 号,避免同一 fd
    // 在 epoll 里重复注册)。`get_ref()` 给 `&PrefixStream`,从中取底层 socket 再 dup。
    let write_stream = match ws.get_ref().tcp().try_clone() {
        Ok(s) => s,
        Err(e) => {
            tracing::debug!(?e, "WS clone WRITE stream 失败,断开");
            return Ok(drop_client_read_self(data, id));
        }
    };
    let write_token = match data.loop_handle.insert_source(
        Generic::new(write_stream, Interest::WRITE, Mode::Level),
        move |_readiness, _meta, data: &mut DaemonData| ws_client_writable(data, id),
    ) {
        Ok(t) => t,
        Err(e) => {
            tracing::warn!(?e, "注册 WS WRITE 源失败,断开");
            return Ok(drop_client_read_self(data, id));
        }
    };

    let replay = data.ring.snapshot();
    if let Some(c) = data.clients.get_mut(&id) {
        c.stage = WsStage::Live(ws);
        // 刚 insert 即 enable 状态;有重放就让它去排空,无重放则它首次可写发现队空自 Disable。
        c.write = Some(WriteReg {
            token: write_token,
            armed: true,
        });
        if !replay.is_empty() {
            let len = replay.len();
            c.outbound.push_back(Bytes::from(replay));
            c.outbound_len += len;
        }
    }
    Ok(PostAction::Continue)
}

/// Live READ 可读:`ws.read()` 排空到 WouldBlock,每条数据帧字节直接 [`Session::on_input`]
/// 写 active tab PTY(同线程,无 channel)。Close / 致命错 → 断开。
fn ws_live_read(data: &mut DaemonData, id: u64) -> io::Result<PostAction> {
    let tab = data.tab_id;
    loop {
        let res = match data.clients.get_mut(&id) {
            Some(c) => match &mut c.stage {
                WsStage::Live(ws) => ws.read(),
                _ => return Ok(PostAction::Continue),
            },
            None => return Ok(PostAction::Remove),
        };
        match res {
            Ok(Message::Binary(b)) => {
                if let Err(e) = data.session.on_input(tab, &b) {
                    tracing::warn!(?e, "WS 输入写 PTY 失败");
                }
            }
            // 浏览器发的是 Binary(裸字节);Text 是防御性兜底(按 UTF-8 字节投递)。
            Ok(Message::Text(t)) => {
                if let Err(e) = data.session.on_input(tab, t.as_str().as_bytes()) {
                    tracing::warn!(?e, "WS 输入写 PTY 失败");
                }
            }
            Ok(Message::Close(_)) => return Ok(drop_client_read_self(data, id)),
            // Ping → tungstenite 已排队 Pong,需要 flush 才发出 → 打开 WRITE 兴趣。
            Ok(_) => arm_write(data, id),
            Err(WsError::Io(e)) if e.kind() == io::ErrorKind::WouldBlock => {
                return Ok(PostAction::Continue)
            }
            Err(WsError::ConnectionClosed) | Err(WsError::AlreadyClosed) => {
                return Ok(drop_client_read_self(data, id))
            }
            Err(e) => {
                tracing::debug!(?e, "WS read 错误,断开客户端");
                return Ok(drop_client_read_self(data, id));
            }
        }
    }
}

/// Live WRITE 可写:先 flush ws 内部缓冲(发出已排队帧 + 自动 Pong),再把出站队列逐帧
/// 灌进 ws 并 flush。WouldBlock → 保留 WRITE 兴趣(`Continue`)下次再来;**队列排空 →
/// 关 WRITE 兴趣退回只 READ(`Disable`,不忙等)**;致命错 → 断开。
///
/// 每轮只灌一帧再 flush:ws 内部写缓冲峰值 ≤ 1 帧,其余压在自管的有界 `outbound`
/// (背压 cap 在 [`fan_out_bytes`] 守),内存有界。
fn ws_client_writable(data: &mut DaemonData, id: u64) -> io::Result<PostAction> {
    loop {
        // 1. flush ws 内部缓冲(发出上轮 WouldBlock 残留 + 自动 Pong)。
        let flush_res = match data.clients.get_mut(&id) {
            Some(c) => match &mut c.stage {
                WsStage::Live(ws) => ws.flush(),
                _ => return Ok(PostAction::Remove),
            },
            None => return Ok(PostAction::Remove),
        };
        match flush_res {
            Ok(()) => {}
            Err(WsError::Io(e)) if e.kind() == io::ErrorKind::WouldBlock => {
                return Ok(PostAction::Continue)
            }
            Err(WsError::ConnectionClosed) | Err(WsError::AlreadyClosed) => {
                return Ok(drop_client_write_self(data, id))
            }
            Err(e) => {
                tracing::debug!(?e, "WS flush 错误,断开客户端");
                return Ok(drop_client_write_self(data, id));
            }
        }
        // 2. ws 缓冲已空,取下一帧出站。
        let frame = match data.clients.get_mut(&id) {
            Some(c) => match c.outbound.pop_front() {
                Some(f) => {
                    c.outbound_len = c.outbound_len.saturating_sub(f.len());
                    f
                }
                None => {
                    // 全部排空 → 关 WRITE 兴趣(退回只 READ,不忙等)。
                    if let Some(w) = &mut c.write {
                        w.armed = false;
                    }
                    return Ok(PostAction::Disable);
                }
            },
            None => return Ok(PostAction::Remove),
        };
        // 3. 灌进 ws(回到循环顶 flush 它)。
        let write_res = match data.clients.get_mut(&id) {
            Some(c) => match &mut c.stage {
                WsStage::Live(ws) => ws.write(Message::Binary(frame)),
                _ => return Ok(PostAction::Remove),
            },
            None => return Ok(PostAction::Remove),
        };
        match write_res {
            Ok(()) => continue,
            // 帧已被 tungstenite 收进内部写缓冲(不丢),下次可写再 flush。
            Err(WsError::Io(e)) if e.kind() == io::ErrorKind::WouldBlock => {
                return Ok(PostAction::Continue)
            }
            Err(WsError::ConnectionClosed) | Err(WsError::AlreadyClosed) => {
                return Ok(drop_client_write_self(data, id))
            }
            Err(e) => {
                // 含 WriteBufferFull —— 默认 max_write_buffer_size = usize::MAX 不会触发,
                // 防御性当致命断开。
                tracing::debug!(?e, "WS write 错误,断开客户端");
                return Ok(drop_client_write_self(data, id));
            }
        }
    }
}

/// 打开某连接的 WRITE 兴趣(出站有积压 / Pong 待发时)。已 armed 则跳过(防重复 enable
/// 触发 EPOLL_CTL_ADD EEXIST)。从**非该 WRITE 源自身**的 callback 调(pty_readable /
/// Live READ),故 `enable` 立即生效(不在该源自己的 dispatch 借用内)。
fn arm_write(data: &mut DaemonData, id: u64) {
    let token = match data.clients.get(&id) {
        Some(c) => match &c.write {
            Some(w) if !w.armed => w.token,
            _ => return, // 还没 WRITE 源(非 Live)/ 已 armed
        },
        None => return,
    };
    if let Err(e) = data.loop_handle.enable(&token) {
        tracing::warn!(?e, id, "enable WS WRITE 源失败");
        return;
    }
    if let Some(c) = data.clients.get_mut(&id) {
        if let Some(w) = &mut c.write {
            w.armed = true;
        }
    }
}

/// 回收一条连接,其中**READ 源是当前 callback 的源**:摘掉(另一个)WRITE 源 + 登记项,
/// 返 `Remove` 让 loop 在本 callback 返回后注销 READ 源(此刻其 fd 仍开 → 干净 DEL)。
fn drop_client_read_self(data: &mut DaemonData, id: u64) -> PostAction {
    if let Some(conn) = data.clients.remove(&id) {
        if let Some(w) = conn.write {
            data.loop_handle.remove(w.token); // WRITE 源:DEL + 关其 dup
        }
        // conn drop:关 ws 的 IO dup。READ 源 own 的 original 由下方 Remove 注销时关。
    }
    PostAction::Remove
}

/// 回收一条连接,其中**WRITE 源是当前 callback 的源**:摘掉(另一个)READ 源 + 登记项,
/// 返 `Remove` 让 loop 注销 WRITE 源。
fn drop_client_write_self(data: &mut DaemonData, id: u64) -> PostAction {
    if let Some(conn) = data.clients.remove(&id) {
        data.loop_handle.remove(conn.read_token); // READ 源:DEL + 关 original
    }
    PostAction::Remove
}

/// 从**与该连接无关的** callback(如 pty_readable 背压断开 / 收割 Timer)回收一条连接:
/// 两个源都经 `loop_handle.remove` 注销 + 摘登记项。
fn drop_client_external(data: &mut DaemonData, id: u64) {
    if let Some(conn) = data.clients.remove(&id) {
        data.loop_handle.remove(conn.read_token);
        if let Some(w) = conn.write {
            data.loop_handle.remove(w.token);
        }
    }
}

/// 收割 Timer 回调:回收卡在 Peeking/Handshaking 阶段超 `handshake_deadline`(自 accept 起
/// 的绝对期限)的连接,防半截头/卡握手/slowloris 长占 `clients` 槽。**live 连接不收割**
/// (健康但安静的终端无收发也属正常,按空闲超时会误杀)——其半开掉线靠 SO_KEEPALIVE。
fn reap_stale_clients(data: &mut DaemonData) {
    let now = Instant::now();
    let deadline = data.handshake_deadline;
    let stale: Vec<u64> = data
        .clients
        .iter()
        .filter(|(_, c)| {
            !matches!(c.stage, WsStage::Live(_)) && now.duration_since(c.created_at) > deadline
        })
        .map(|(id, _)| *id)
        .collect();
    for id in stale {
        tracing::debug!(id, "WS 连接握手未在期限内完成,收割(防卡握手/半截头占槽)");
        drop_client_external(data, id);
    }
}

/// 给一条接受进来的 TCP 连接打开内核 TCP keepalive 探活(尽力而为的半开硬化)。默认
/// idle 7200s 太长,显式调短:idle 60s 后开始探,每 10s 探一次,3 次无应答判死。死后内核
/// 让该 fd 报错 → epoll 唤醒 READ 源 → 走既有 EOF/错误回收路径。**比按空闲超时收割 live
/// 连接安全**(不误杀健康但安静的会话)。失败容忍(老内核缺某 sockopt 不影响主功能)。
#[allow(unsafe_code)]
fn enable_keepalive(stream: &TcpStream) {
    let fd = stream.as_raw_fd();
    let set = |level: libc::c_int, name: libc::c_int, val: libc::c_int| {
        // SAFETY: setsockopt 只读取我们栈上 c_int 的 size_of 字节(只读不写),fd 来自本函数
        // 参数里活着的 TcpStream;返回值忽略(尽力而为,失败不影响主功能)。
        unsafe {
            libc::setsockopt(
                fd,
                level,
                name,
                std::ptr::addr_of!(val).cast::<libc::c_void>(),
                std::mem::size_of::<libc::c_int>() as libc::socklen_t,
            );
        }
    };
    set(libc::SOL_SOCKET, libc::SO_KEEPALIVE, 1);
    set(libc::IPPROTO_TCP, libc::TCP_KEEPIDLE, 60);
    set(libc::IPPROTO_TCP, libc::TCP_KEEPINTVL, 10);
    set(libc::IPPROTO_TCP, libc::TCP_KEEPCNT, 3);
}

/// 读 env 里的毫秒数(调参 / 测试用),解析失败或缺省落回 `default_ms`。
fn duration_from_env(key: &str, default_ms: u64) -> Duration {
    let ms = std::env::var(key)
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(default_ms);
    Duration::from_millis(ms)
}

/// 普通 HTTP 请求:在给定 stream 上排响应(`GET /` / `/index.html` 返 xterm.js 页,
/// `/vendor/*` 返 vendored 资产,其余 404)写完即关。非阻塞写源(分次写完 →
/// `PostAction::Remove` 自动注销 + 关连接,即"推完即关")。
fn serve_http_response(data: &mut DaemonData, stream: TcpStream, head: &[u8]) {
    let _ = stream.set_nonblocking(true); // dup 已继承 original 的非阻塞,稳妥再设一次
    let path = request_path(head).unwrap_or_default();
    let (status, ctype, body) = http_response_parts(&path);
    let header = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\n\
         Connection: close\r\nCache-Control: no-store\r\n\r\n",
        body.len()
    );
    let mut full = Vec::with_capacity(header.len() + body.len());
    full.extend_from_slice(header.as_bytes());
    full.extend_from_slice(body);
    let pending = Rc::new(RefCell::new(WritePending {
        buf: full,
        written: 0,
    }));
    if let Err(e) = data.loop_handle.insert_source(
        Generic::new(stream, Interest::WRITE, Mode::Level),
        move |_readiness, meta, _data: &mut DaemonData| write_pending(meta.as_ref(), &pending),
    ) {
        tracing::warn!(?e, "注册 HTTP 响应写源失败");
    }
}

/// path → (status line, content-type, body)。纯函数,便于单测。
fn http_response_parts(path: &str) -> (&'static str, &'static str, &'static [u8]) {
    let js = "text/javascript; charset=utf-8";
    match path {
        "/" | "/index.html" => ("200 OK", "text/html; charset=utf-8", INDEX_HTML.as_bytes()),
        "/vendor/xterm.js" => ("200 OK", js, XTERM_JS.as_bytes()),
        "/vendor/xterm-addon-fit.js" => ("200 OK", js, XTERM_FIT_JS.as_bytes()),
        "/vendor/xterm.css" => ("200 OK", "text/css; charset=utf-8", XTERM_CSS.as_bytes()),
        _ => ("404 Not Found", "text/plain; charset=utf-8", b"not found\n"),
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
/// (calloop 自动注销 source 并 drop 掉 owned 的 stream = 关连接)。泛型支持
/// `UnixStream`(unix 快照)与 `TcpStream`(HTTP 响应):两者的 `&S` 都实现 `Write`。
fn write_pending<S>(stream: &S, pending: &Rc<RefCell<WritePending>>) -> io::Result<PostAction>
where
    for<'a> &'a S: Write,
{
    let mut p = pending.borrow_mut();
    let mut writer: &S = stream;
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
/// 可见屏恒完整)。新 WS 客户端连上重放此环重建当前屏。单线程,直接在 [`DaemonData`]
/// 字段里,只装 `u8`(与 `!Send` 的 [`Session`] 无关)。
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
        let args = vec![
            "quill-kernel".to_string(),
            "--ws-bind=10.0.0.2:7878".to_string(),
        ];
        assert_eq!(
            parse_ws_bind_arg(&args).expect("合法 ws-bind 解析"),
            Some("10.0.0.2:7878".parse().expect("test addr"))
        );
        let v6 = vec![
            "quill-kernel".to_string(),
            "--ws-bind=[::1]:9000".to_string(),
        ];
        assert_eq!(
            parse_ws_bind_arg(&v6).expect("合法 v6 ws-bind"),
            Some("[::1]:9000".parse().expect("test v6 addr"))
        );
        assert_eq!(
            parse_ws_bind_arg(&["quill-kernel".to_string()]).expect("缺省"),
            None
        );
        let bad = vec![
            "quill-kernel".to_string(),
            "--ws-bind=not-an-addr".to_string(),
        ];
        assert!(parse_ws_bind_arg(&bad).is_err());
    }

    #[test]
    fn default_ws_bind_is_lan_reachable() {
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

    /// 同口分流:WS upgrade 头识别 + path 解析 + header_complete。
    #[test]
    fn http_dispatch_helpers() {
        let ws_req =
            b"GET / HTTP/1.1\r\nHost: x\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\r\n";
        assert!(request_is_ws_upgrade(ws_req), "应识别 WS upgrade");
        assert!(header_complete(ws_req), "应识别头收全");

        let plain = b"GET /vendor/xterm.js?v=1 HTTP/1.1\r\nHost: x\r\n\r\n";
        assert!(!request_is_ws_upgrade(plain), "普通 GET 不是 upgrade");
        assert_eq!(
            request_path(plain).as_deref(),
            Some("/vendor/xterm.js"),
            "应去掉 query"
        );

        let partial = b"GET / HTTP/1.1\r\nHost: x\r\n";
        assert!(!header_complete(partial), "头未收全");
    }

    /// HTTP 响应路由:`/` 出 xterm.js 页(含 WebSocket 接线),未知路径 404。
    #[test]
    fn http_response_parts_routes() {
        let (status, ctype, body) = http_response_parts("/");
        assert!(status.starts_with("200"));
        assert!(ctype.contains("text/html"));
        let page = String::from_utf8_lossy(body);
        assert!(page.contains("WebSocket") && page.contains("term.write"));

        let (status, _ctype, _body) = http_response_parts("/vendor/xterm.js");
        assert!(status.starts_with("200"));

        let (status, _ctype, body) = http_response_parts("/nope");
        assert!(status.starts_with("404"));
        assert_eq!(body, b"not found\n");
    }
}
