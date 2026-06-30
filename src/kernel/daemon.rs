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
//! - **卡死 HTTP 响应写源收割**:同一个 [`Timer`] 还扫 [`DaemonData::http_writers`],回收超
//!   `http_write_deadline` 仍没把响应写完(drain)的 HTTP 写源。HTTP 分支建写源前已把连接摘出
//!   `clients`(写源既不受 [`MAX_WS_CONNS`] 限、上面那支握手收割也扫不到),且写源 `WouldBlock`
//!   时回调不再被派发(fd 不可写 → Level 不触发,无法靠写回调自查超时)→ **必须** Timer 兜底,
//!   否则 slowloris-read(要了大页却永不读 / 通告极小窗口)的写源 + dup fd 永挂 → 堆爆。
//!
//! **仍留后续 ticket**:多 tab 动态增删 + 输入按 tab 寻址 + resize 协商(T6)。

use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::io::{self, Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::os::fd::{AsRawFd, BorrowedFd, FromRawFd, OwnedFd, RawFd};
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

use crate::kernel::feed::{encode_into, FeedDecoder, FeedFrame, FrameKind, FEED_HEADER_LEN};
use crate::kernel::proto::{ClientMsg, ServerMsg, Snapshot, WorkspaceList, WorkspaceMeta};
use crate::kernel::session::{Lifecycle, Session};
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

/// Fed back-channel(子→父 Input 帧)出站字节队列上限。WS 客户端输入字节是**长度前缀成帧
/// 流**(非 PTY 无帧字节流)→ 丢半帧会让父侧 [`FeedDecoder`] framing 错位级联(读 len=N 只
/// 收 M<N → 吃下一帧字节凑数 → `InvalidKind` 杀子),故**绝不丢半帧**:写不完入队。队列积压
/// 超此上限 = 父长期不读 back-channel → **断子降级**(停子 daemon,父读 EOF 重建干净子),
/// 既不无界堆内存、又不丢半帧污染流。1 MiB 远超人手速输入排空所需(正常队列常空)。
const FED_BACK_OUT_CAP: usize = 1 << 20;

/// Fed `Input` 帧分帧上界。单条 WS 输入(粘贴)payload 可超 `MAX_FEED_PAYLOAD`(16 MiB)甚至
/// 理论上 `u32` 截断 → 切成 ≤ 此大小的多个 `Input` 帧(各带同 (ws,tab) 标签),保证没有任何
/// `Input` 帧 payload 超 feed 协议上限,父侧逐帧重组回完整输入。64 KiB 远低于上限、又够大到
/// 正常键入/小粘贴单帧装下。
const FED_INPUT_CHUNK: usize = 64 * 1024;

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

/// HTTP 响应写源从建立起必须在此期限内把响应写完(drain),否则被收割 Timer 回收。绝对
/// 期限(非"距上次写进度"),故 slowloris-read(要了大页却永不读响应 / 通告极小 TCP 窗口)
/// 让写源恒 `WouldBlock` 挂着也逃不掉。正常浏览器 ms 级读完即 drain 自删,10s 极宽松,只杀
/// 真卡死的。**为何也要它**:HTTP 分支在建写源前已把连接从 `clients` 摘掉 → 写源既不受
/// `MAX_WS_CONNS` 限、握手收割也扫不到;且写源 `WouldBlock` 时回调根本不再被派发(fd 不可写
/// → Level 不触发),无法靠"写回调里自查超时"兜底,必须靠独立 Timer 扫。
const HTTP_WRITE_DEADLINE_MS: u64 = 10_000;

/// HTTP 响应 socket 的 send 缓冲上限(`setsockopt SO_SNDBUF`)。**双重作用**:① 把单条慢/恶意
/// HTTP 客户端能钉住的内核 send 内存从默认自动调优上限(`net.ipv4.tcp_wmem` max,常达数 MB)
/// 压到几十 KB;② 让大响应(如 ~283KB 的 xterm.js)发给"永不读"的客户端时 `write` 真的撞
/// `WouldBlock` 滞留 —— 否则整页一次性灌进内核的大 send 缓冲 → 写源立刻 drain 自删,反把上面
/// 那个写超时收割**绕过**(资源转移到内核滞留 socket,收割 Timer 看不见)。压小后写源真卡住 →
/// 可见 → 被 `http_write_deadline` 收割回收 fd + 内核缓冲。**只影响一次性静态页下发**(WS
/// 字节流直播走各连接自己的 socket,不受此限),localhost/VPN 下 283KB 页仍亚秒级加载,无感。
/// 内核会把设定值翻倍并夹到 `SOCK_MIN_SNDBUF` 以上;16KiB 远小于最大资产 → 必触 `WouldBlock`。
const HTTP_SEND_BUF_BYTES: libc::c_int = 16 * 1024;

/// daemon 的字节来源拓扑 (ADR-0018 E′)。决定字节从哪来、输入往哪去、是否 own PTY。
pub enum SourceConfig {
    /// **Local**(standalone `quill-kernel`):自 spawn shell + own 真 PTY,[`Session`] 驱动
    /// 一切(unix-dump 快照 + WS 直播 + 输入直写 PTY)。砖0 / 今天的 daemon。
    Local,
    /// **Fed**(E′ 共享子进程):字节从父 pipe 来、**不 spawn shell / 不开真 PTY / 不绑 unix
    /// socket**;WS 输入回灌父 back-channel(子自己不写 PTY,父 own PTY)。`read_fd` = 父→子
    /// (PtyOutput / FocusChange / WorkspaceAdd/Remove 帧),`write_fd` = 子→父(Input 帧)。
    /// 两 fd 由父在 spawn 时经继承传入(见 [`parse_fed_source`]);砖1a 子复用砖0 的 ring +
    /// fan_out + WS 子系统,仅把"字节从哪来 / 输入往哪去"两端换掉。
    Fed { read_fd: RawFd, write_fd: RawFd },
}

/// daemon 启动参数。
pub struct DaemonConfig {
    /// `UnixListener` 绑定路径(仅 [`SourceConfig::Local`] 用;Fed 不绑)。
    pub socket_path: PathBuf,
    /// WS (tungstenite) TCP 监听地址。默认 `0.0.0.0:7878` —— LAN 可达,手机经
    /// WireGuard VPN → 路由器 → `10.0.0.2:port` 连上;安全靠 VPN 把门 (ADR-0016)。
    pub ws_bind: SocketAddr,
    pub cols: u16,
    pub rows: u16,
    /// 字节来源拓扑 (E′)。默认 [`SourceConfig::Local`](standalone)。
    pub source: SourceConfig,
}

impl DaemonConfig {
    /// 用给定 socket 路径 + 默认尺寸 + 默认 WS bind 建配置(默认 Local 拓扑)。
    pub fn with_socket(socket_path: PathBuf) -> Self {
        Self {
            socket_path,
            ws_bind: SocketAddr::from(([0, 0, 0, 0], DEFAULT_WS_PORT)),
            cols: DEFAULT_COLS,
            rows: DEFAULT_ROWS,
            source: SourceConfig::Local,
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

/// 从 argv 抠 E′ 子进程的父↔子 pipe fd:`--fed-in=<fd>` (父→子) + `--fed-out=<fd>`
/// (子→父)。父 spawn 子时把继承下来的 pipe fd 号经命令行传入。两者都给 → `Ok(Some(Fed))`;
/// 都没给 → `Ok(None)`(调用方用 Local);**只给一个 / 解析失败 → `Err`**(早失败,别半配)。
pub fn parse_fed_source(args: &[String]) -> Result<Option<SourceConfig>> {
    let read = find_fd_arg(args, "--fed-in=")?;
    let write = find_fd_arg(args, "--fed-out=")?;
    match (read, write) {
        (Some(read_fd), Some(write_fd)) => Ok(Some(SourceConfig::Fed { read_fd, write_fd })),
        (None, None) => Ok(None),
        _ => bail!("--fed-in 与 --fed-out 必须成对给出 (E′ 子进程父↔子双向 pipe)"),
    }
}

/// 抠 `<prefix><fd>` 形式的非负 fd 整数。缺省返 `Ok(None)`;给了但非法返 `Err`。
fn find_fd_arg(args: &[String], prefix: &str) -> Result<Option<RawFd>> {
    match args.iter().skip(1).find_map(|a| a.strip_prefix(prefix)) {
        Some(s) => {
            let fd = s
                .parse::<RawFd>()
                .with_context(|| format!("解析 {prefix}{s} 失败 (需 fd 整数)"))?;
            if fd < 0 {
                bail!("{prefix}{s} 非法 (fd 须 >= 0)");
            }
            Ok(Some(fd))
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

    // 先绑 WS TCP listener(两拓扑都 serve WS:Local = standalone,Fed = E′ 子即 WS-kernel)。
    // 端口占用等错早退,此刻还没落 unix socket 文件,无残留可清。WS 全程在本 calloop 线程,
    // **无 channel / 无线程**(去掉了 ADR-0016 线程版的 mpsc + WsServer)。
    let ws_listener = TcpListener::bind(config.ws_bind)
        .with_context(|| format!("WS TcpListener::bind {} 失败", config.ws_bind))?;
    // INV:Level 模式下阻塞 accept 会 stall 整个单线程 loop → 必须非阻塞 + accept 到
    // WouldBlock 排空。
    ws_listener
        .set_nonblocking(true)
        .context("WS TcpListener 设非阻塞失败")?;

    // 收割参数:默认生产值,可经 env 覆盖(调参 / 测试用,无新依赖、不改 CLI)。
    let reap_interval = duration_from_env("QUILL_WS_REAP_MS", REAP_INTERVAL_MS);
    let handshake_deadline =
        duration_from_env("QUILL_WS_HANDSHAKE_DEADLINE_MS", HANDSHAKE_DEADLINE_MS);
    let http_write_deadline =
        duration_from_env("QUILL_WS_HTTP_WRITE_DEADLINE_MS", HTTP_WRITE_DEADLINE_MS);

    // 按拓扑(ADR-0018 E′)建 字节来源 + 输入 sink,顺带注册各自专属的源:
    // - **Local**:spawn shell + own 真 PTY(注册 PTY READ 源 + unix-dump listener),Session 在;
    // - **Fed**:从父 pipe 读(注册父 pipe READ 源),不 spawn shell / 不开真 PTY / 不绑 unix
    //   socket,Session 不在(子不 own term,见 [`DaemonData::session`] doc),输入回灌父
    //   back-channel(块C)。
    let (session, fed, tab_id, workspace_id, local_socket) = match &config.source {
        SourceConfig::Local => {
            // 信号已在主线程 block,现在才 spawn shell tab(其 worker 线程继承 block)。
            let tab = TabInstance::spawn(config.cols, config.rows)
                .context("daemon 启动:spawn shell tab 失败")?;
            let tab_id = tab.id().raw();
            let mut session = Session::new(TabList::new(tab));
            // standalone daemon = 自己是 anchor(谁 spawn 工作区谁是隐式锚,ADR-0018):anchor 在 →
            // WS 客户端全断 / 全显式关闭工作区都【不死】;只有子 shell PTY EOF 才退。E′ 里换成
            // 父进程是锚(Fed 子不自 anchor,其工作区生命周期归父),那是砖1b 的事。
            let workspace_id = session.active_workspace_id();
            session.set_anchor(workspace_id, true);

            // Source: PTY master fd(从 local session 取,move 进 data 前)。
            let pty_fd = session.tabs().active().pty().raw_fd();
            // SAFETY:
            // - pty_fd 来自 `PtyHandle::raw_fd()`(构造时 `as_raw_fd().ok_or_else` 校验过一次),
            //   PtyHandle 在 `data.session.tabs` 里(下方 move 进 data),`run()` scope 内全程活着。
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

            // Source: UnixListener(quill-dump 调试路径,与 WS 字节流独立,Local 限定 ——
            // Fed 子无 server term 可 dump)。owned 直接交给 Generic(实现 AsFd,source drop 时关)。
            prepare_socket_path(&config.socket_path)?;
            let listener = UnixListener::bind(&config.socket_path).with_context(|| {
                format!("bind UnixListener {} 失败", config.socket_path.display())
            })?;
            listener
                .set_nonblocking(true)
                .context("UnixListener 设非阻塞失败")?;
            loop_handle
                .insert_source(
                    Generic::new(listener, Interest::READ, Mode::Level),
                    |_readiness, meta, data: &mut DaemonData| accept_ready(meta.as_ref(), data),
                )
                .map_err(|e| anyhow!("calloop insert_source(unix listener) 失败: {e}"))?;

            (
                Some(session),
                None,
                tab_id,
                workspace_id,
                Some(config.socket_path.clone()),
            )
        }
        SourceConfig::Fed { read_fd, write_fd } => {
            let mut link =
                FedLink::new(*read_fd, *write_fd).context("Fed 模式建父↔子 pipe 链失败")?;
            // 父 pipe **读端** 注册成 calloop Generic READ 源(像 Local 的 PTY 源);闭包捕获
            // raw fd(int Copy),源 own 的 `OwnedFd` 在 loop 生命期内不删 → fd 全程有效。
            let read_raw = link.read_owned.as_raw_fd();
            loop_handle
                .insert_source(
                    Generic::new(link.read_owned, Interest::READ, Mode::Level),
                    move |_readiness, _meta, data: &mut DaemonData| {
                        fed_pipe_readable(data, read_raw)
                    },
                )
                .map_err(|e| anyhow!("calloop insert_source(fed pipe read) 失败: {e}"))?;

            // back-channel **写端** 注册成 WRITE 源(初始 disable —— 启动无待写,有积压才 arm,
            // drain 空再 disarm,不忙等)。**绝不丢半帧**:[`fed_send_frame`] 队空时尽量直写,
            // WouldBlock 写不完的剩余字节入队 + arm,可写时 [`fed_back_writable`] drain(子→父是
            // 长度前缀成帧流,半帧 → 父 FeedDecoder framing 错位杀子,见 [`FED_BACK_OUT_CAP`])。
            // fd 由 `FedState.back`(OwnedFd)own,这里只借 raw(像 PTY 源借 BorrowedFd)。
            let back_raw = link.back.back.as_raw_fd();
            // SAFETY: back_raw 来自 `link.back.back`(OwnedFd,下方 move 进 `data.fed`,run() scope
            // 全程活);`borrow_raw` 只取 int 不转移所有权;真正的 write 走 `FedState.back` 自身的
            // raw,从不碰这个 BorrowedFd。drop 序:函数尾 `drop(event_loop)` 早于 `drop(data)`,
            // 即 WRITE 源的 EPOLL_CTL_DEL 在 back fd 仍开时执行(即便错序,calloop 对已关 fd 的
            // DEL 返 EBADF 内部容忍,非 UB;同 PTY 源 SAFETY)。
            #[allow(unsafe_code)]
            let back_borrowed: BorrowedFd<'static> = unsafe { BorrowedFd::borrow_raw(back_raw) };
            let write_token = loop_handle
                .insert_source(
                    Generic::new(back_borrowed, Interest::WRITE, Mode::Level),
                    move |_readiness, _meta, data: &mut DaemonData| fed_back_writable(data),
                )
                .map_err(|e| anyhow!("calloop insert_source(fed back-channel write) 失败: {e}"))?;
            // 初始无待写 → disable,使 `armed=false` 与 epoll 实态一致(否则 WRITE+Level 源在
            // 空闲可写 fd 上恒触发烧核;且 arm 时对"实已 enable"的源再 enable 有风险)。
            loop_handle
                .disable(&write_token)
                .map_err(|e| anyhow!("disable Fed back-channel WRITE 源失败: {e}"))?;
            link.back.write = Some(WriteReg {
                token: write_token,
                armed: false,
            });

            // Fed 焦点(workspace, tab)初始未知,等父发 FocusChange 帧填(块B)。
            (None, Some(link.back), 0u64, 0u64, None)
        }
    };

    let mut data = DaemonData {
        session,
        fed,
        loop_handle: loop_handle.clone(),
        loop_signal,
        tab_id,
        workspace_id,
        ring: ByteRing::new(BYTE_RING_CAP),
        clients: HashMap::new(),
        next_client_id: 1,
        handshake_deadline,
        http_writers: HashMap::new(),
        next_http_writer_id: 1,
        http_write_deadline,
    };

    // Source: WS TCP listener。owned 交 Generic;可读→accept 新连接注册成各自的源(两拓扑共享)。
    loop_handle
        .insert_source(
            Generic::new(ws_listener, Interest::READ, Mode::Level),
            |_readiness, meta, data: &mut DaemonData| ws_accept_ready(meta.as_ref(), data),
        )
        .map_err(|e| anyhow!("calloop insert_source(ws listener) 失败: {e}"))?;

    // Source: 收割 Timer。周期扫描,回收 ① 卡在 Peeking/Handshaking 阶段超
    // `handshake_deadline` 的连接(防半截头/卡握手长占 `clients` 槽 + slowloris);② 超
    // `http_write_deadline` 仍没把响应写完(drain)的 HTTP 响应写源(防 slowloris-read:要了
    // 大页却永不读 → 写源恒 WouldBlock 挂着,不受 `MAX_WS_CONNS` 限、握手收割也扫不到)。live
    // 连接不在此列(健康空闲会被误杀)——其半开靠 accept 时设的 SO_KEEPALIVE 探活回收。
    // 收割时只 `loop_handle.remove` 别的源(连接的 READ/WRITE 源、HTTP 写源),不 remove 本
    // Timer 自己(返 `ToDuration` 让 calloop 自动重排,calloop 禁止回调内 remove 自身源)。
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
        ws_bind = %config.ws_bind,
        tab_id,
        fed = data.fed.is_some(),
        "quill-kernel daemon 就绪 (单线程 calloop:{} + WS 同一 loop)",
        if data.fed.is_some() { "Fed 父 pipe" } else { "PTY + unix" }
    );

    let run_result = event_loop
        .run(None, &mut data, |_data| {})
        .context("calloop EventLoop::run 失败");

    // 显式 drop 序:event_loop(持各 Generic source,含 WS 连接源 + PTY/父pipe 源)先于
    // data(持 PtyHandle / FedState back-channel / WS WebSocket 的 fd),让源的 EPOLL_CTL_DEL
    // 在对应 fd 仍打开时执行(见上 SAFETY)。WS 无线程可 join,drop 即收尾。
    drop(event_loop);
    drop(data);

    if let Some(path) = local_socket {
        remove_socket_quiet(&path);
    }

    run_result
}

/// E′ 子进程(Fed 模式)的父↔子链构造结果:**读端** [`OwnedFd`](交 calloop Generic 源
/// own) + **写端** [`FedState`](back-channel,进 [`DaemonData::fed`])。`new` 取走父传入的
/// 两 fd 所有权 + 设非阻塞;`read_fd == write_fd`(socketpair 单 fd 全双工)时 dup 写端,
/// 避免两个 `OwnedFd` 重复 close 同一 fd。
struct FedLink {
    read_owned: OwnedFd,
    back: FedState,
}

impl FedLink {
    fn new(read_fd: RawFd, write_fd: RawFd) -> Result<Self> {
        // socketpair 单 fd 全双工时,写端 dup 一份独立 fd(防两个 OwnedFd 双关同一 fd)。
        let write_fd = if write_fd == read_fd {
            // SAFETY: dup 复制 fd,失败返 -1;成功得独立 fd 由下方 OwnedFd 接管 close。
            #[allow(unsafe_code)]
            let d = unsafe { libc::dup(write_fd) };
            if d < 0 {
                return Err(
                    anyhow::Error::new(io::Error::last_os_error()).context("dup Fed write fd 失败")
                );
            }
            d
        } else {
            write_fd
        };
        set_fd_nonblocking(read_fd).context("Fed read fd 设非阻塞失败")?;
        set_fd_nonblocking(write_fd).context("Fed write fd 设非阻塞失败")?;
        // SAFETY: read_fd / write_fd 由父经 fork/exec 继承传入,子独占这两端;包成 OwnedFd
        // 接管 close(drop 时关)。父在 spawn 后关其副本 → 父退出时子读端得 EOF(子据此察觉
        // 父没了,ADR-0018)。
        #[allow(unsafe_code)]
        let read_owned = unsafe { OwnedFd::from_raw_fd(read_fd) };
        #[allow(unsafe_code)]
        let write_owned = unsafe { OwnedFd::from_raw_fd(write_fd) };
        Ok(Self {
            read_owned,
            back: FedState {
                decoder: FeedDecoder::new(),
                back: write_owned,
                outbound: VecDeque::new(),
                write: None,
            },
        })
    }
}

/// E′ 子进程(Fed 模式)的父↔子链 **写侧** 状态(进 [`DaemonData::fed`])。父 pipe 读端由
/// calloop Generic 源 own(见 [`FedLink`]);这里只持 back-channel **写端**(子→父 Input 帧)、
/// 增量解码器(读端回调 [`fed_pipe_readable`] 用)、出站字节队列(写不完入队、绝不丢半帧,
/// 见 [`fed_send_frame`] / [`fed_back_writable`])。
struct FedState {
    decoder: FeedDecoder,
    /// 子→父 back-channel 写端(Input 帧)。非阻塞:[`fed_send_frame`] 直写不完的剩余字节入
    /// [`FedState::outbound`],可写时 [`fed_back_writable`] drain(绝不丢半帧)。
    back: OwnedFd,
    /// 待写出的字节队列(子→父成帧流)。WouldBlock 写不完的尾部入此,保序;积压超
    /// [`FED_BACK_OUT_CAP`] = 父长期不读 → 断子降级(停子 daemon)。**绝不丢半帧**(半帧 →
    /// 父 [`FeedDecoder`] framing 错位级联杀子)。
    outbound: VecDeque<u8>,
    /// back-channel WRITE 源(`armed` = 当前是否 enable;有积压 arm、drain 空 disarm,不忙等)。
    /// `None` 仅存在于构造到 `run()` 注册之间的瞬态;`run()` 内恒 `Some`。
    write: Option<WriteReg>,
}

/// 把一个 raw fd 设 `O_NONBLOCK`(Fed 父↔子 pipe 两端,子侧自设防阻塞 loop)。
#[allow(unsafe_code)]
fn set_fd_nonblocking(fd: RawFd) -> io::Result<()> {
    // SAFETY: `fcntl` F_GETFL/F_SETFL 只读写 OFD status flags(O_NONBLOCK),不动 fd 所有权;
    // 对已关 fd 返 EBADF(非 UB)。两次返回值都按 `< 0` 判错并转 io::Error。
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFL);
        if flags < 0 {
            return Err(io::Error::last_os_error());
        }
        if libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) < 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

/// 单线程 daemon 的 calloop `Data`。callback 拿 `&mut DaemonData` 走字段 split
/// borrow(与 `wl::window::LoopData` 同模式)。**WS 全在本线程,故 ring / clients
/// 都是普通字段,无 Arc / 锁 / channel。**
struct DaemonData {
    /// **Local 拓扑**(standalone):own 多工作区 [`Session`](shell tab + 真 PTY)。**Fed
    /// 拓扑**(E′ 子)= `None`:子不 own term/PTY(父 own),控制面元数据/焦点由父喂的帧维护
    /// (砖1a 最小;砖1b 接 WorkspaceAdd 帧)。**为何不在 Fed 也用 Session**:`Session` 焊死在
    /// `TabList<TabInstance>`,而 `TabInstance` 必持 `PtyHandle`(无 PTY 构造不出),违反 ADR-0018
    /// "Fed 不开真 PTY";改 `pty` 为 Option 又破 `window.rs` 的 `pty()->&PtyHandle` 契约(本砖
    /// 不碰 window.rs)→ 故 Fed 走轻量 [`DaemonData::fed`],复用 ring + fan_out + WS 子系统。
    session: Option<Session>,
    /// **Fed 拓扑**(E′ 子)的父↔子链写侧 + 解码器;`None` = Local。与 [`DaemonData::session`]
    /// 恰好一个为 `Some`(拓扑二选一)。
    fed: Option<FedState>,
    /// 运行期注册 / 注销源(WS 连接的 READ/WRITE 源、unix 客户端写源)用。
    loop_handle: LoopHandle<'static, DaemonData>,
    loop_signal: LoopSignal,
    /// 当前焦点 tab 的 raw id(字节流 [`ServerMsg::StreamFocus`] 标的那个)。Local:启动期
    /// 唯一 tab;Fed:父发 [`FrameKind::FocusChange`] 帧更新。
    tab_id: u64,
    /// 本 daemon 的 active(且唯一,砖0)工作区 id。anchor 工作区 —— WS 连上即 hold 它、
    /// 字节流 [`ServerMsg::StreamFocus`] 标它;client X 关闭 / 断线只动 holder,anchor 在
    /// 故它【不被销毁】(砖0 daemon 字节泵注册的就是它的 PTY fd,绝不能被 refcount 销毁掉)。
    workspace_id: u64,
    /// PTY 原始字节环缓冲(连上重放重建当前屏)。单线程,直接 fan-out 进各连接出站队列。
    ring: ByteRing,
    /// 在册 WS 连接(Peeking / Handshaking / Live)。key = 自增连接 id。
    clients: HashMap<u64, WsClient>,
    /// WS 连接 id 自增源。
    next_client_id: u64,
    /// 连接从 accept 起到完成握手的绝对期限;超期且未转 Live 的连接被收割 Timer 回收。
    handshake_deadline: Duration,
    /// 在飞的 HTTP 响应写源(同口 HTTP 分支建的非阻塞写源)。key = 自增 id。写完(drain)
    /// 即在 [`http_write_pending`] 里摘除;没 drain 完且超 `http_write_deadline` 的由收割
    /// Timer [`reap_stale_clients`] `loop_handle.remove` 回收(关其 dup fd)。**独立于
    /// `clients`**:HTTP 分支建写源前已把连接摘出 `clients`,故必须单独登记 + 单独扫,否则
    /// slowloris-read 写源永不超时堆爆(must-fix)。
    http_writers: HashMap<u64, HttpWriter>,
    /// HTTP 响应写源 id 自增源。
    next_http_writer_id: u64,
    /// HTTP 响应写源从建立起把响应写完的绝对期限;超期未 drain 的被收割 Timer 回收。
    http_write_deadline: Duration,
}

/// 一个在飞 HTTP 响应写源的登记项(收割 Timer 用):源 token(回收时 `loop_handle.remove`)
/// + 建立时刻(判是否超 `http_write_deadline`,绝对期限,故 slowloris-read 逃不掉)。
struct HttpWriter {
    token: RegistrationToken,
    created_at: Instant,
}

/// 一个 unix 客户端连接待写出的快照字节 + 已写偏移(非阻塞 write 可能分多次)。
/// HTTP 响应写源也复用此结构。
struct WritePending {
    buf: Vec<u8>,
    written: usize,
}

/// 一条 WS 连接的出站帧(两平面,T6):**数据面** PTY 原始字节(发 WS Binary 帧)/
/// **控制面** JSON `ServerMsg`(发 WS Text 帧,如工作区列表 / 字节流标签)。同一出站
/// 队列里按 push 顺序穿插,可写时各按其类型写出。
enum OutFrame {
    /// PTY 原始字节(数据面;`Bytes` 多客户端共享同一帧不拷贝)。
    Bytes(Bytes),
    /// 控制面 JSON(`ServerMsg` 序列化后的文本)。
    Text(String),
}

impl OutFrame {
    /// 背压 cap 计字节用(`outbound_len` 累计)。
    fn len(&self) -> usize {
        match self {
            OutFrame::Bytes(b) => b.len(),
            OutFrame::Text(s) => s.len(),
        }
    }
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
    /// 本连接持有的工作区 id(引用计数 holder,T6 块C)。转 Live 时 = active 工作区
    /// (隐式 Hold);收 [`ClientMsg::Hold`] 可改。X 显式关闭(`Release`/`Close`)= explicit
    /// 释放;断线 = 非事件释放(都经 [`release_holder`] 消费此字段,`take` 后幂等)。
    held_ws: Option<u64>,
    /// 待写出的帧(数据面字节 + 控制面 JSON,见 [`OutFrame`])。
    outbound: VecDeque<OutFrame>,
    /// `outbound` 当前总字节(背压 cap 判定;各帧 `len` 求和的缓存)。
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
        // PTY 源仅 Local 拓扑注册 → session 必 Some;防御性 guard 不用 unwrap。
        let read = match data.session.as_mut() {
            Some(s) => s.tabs_mut().active_mut().pty_mut().read(&mut buf),
            None => return Ok(PostAction::Remove),
        };
        match classify_pty_read(&read) {
            PtyRead::Feed => {
                if let Ok(n) = read {
                    if let Some(s) = data.session.as_mut() {
                        s.on_pty_output(tab_id, &buf[..n]);
                    }
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
                if let Some(s) = data.session.as_mut() {
                    let _ = s.tabs_mut().active_mut().pty_mut().try_wait();
                }
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
                c.outbound_len += frame.len();
                c.outbound.push_back(OutFrame::Bytes(frame.clone()));
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

/// 父 pipe 读端可读(Fed 拓扑,块B;镜像 [`pty_readable`] 的 drain 语义):drain 到 WouldBlock,
/// 喂 [`FeedDecoder`] 解帧,逐帧 [`route_fed_frame`]。父 pipe **EOF** = 父进程退出 → 停子
/// daemon(ADR-0018:子靠 EOF-on-pipe 察觉父没了);**流错位**(坏帧)= 致命同样停(不从错位
/// 流猜帧边界)。
///
/// **drain-then-route**:先把本轮可读字节全 drain + 解出所有完整帧到本地 `frames`(其间只借
/// `data.fed`),再逐帧路由(`route_fed_frame` 自由借 `data`)—— 规避"持 fed 借用又调 fan_out
/// 借 data"的冲突。
fn fed_pipe_readable(data: &mut DaemonData, fd: RawFd) -> io::Result<PostAction> {
    let mut frames: Vec<FeedFrame> = Vec::new();
    let mut buf = [0u8; PTY_READ_BUF];
    let mut stop = false;
    loop {
        // SAFETY: fd 是 Fed read 源 own 的 OwnedFd 的 raw(run() 注册时活着,源在 loop 生命期
        // 内不删);libc::read 只调 syscall 不动 fd 所有权,buf 是栈数组。
        #[allow(unsafe_code)]
        let n = unsafe { libc::read(fd, buf.as_mut_ptr().cast::<libc::c_void>(), buf.len()) };
        if n > 0 {
            let Some(fed) = data.fed.as_mut() else {
                return Ok(PostAction::Remove); // 不该发生:Fed 源仅 Fed 拓扑注册
            };
            fed.decoder.push(&buf[..n as usize]);
            loop {
                match fed.decoder.next_frame() {
                    Ok(Some(f)) => frames.push(f),
                    Ok(None) => break,
                    Err(e) => {
                        tracing::error!(%e, "Fed 喂料流解码错位 (致命),停止子 daemon");
                        stop = true;
                        break;
                    }
                }
            }
            if stop {
                break;
            }
            continue;
        }
        if n == 0 {
            tracing::info!("Fed 父 pipe EOF:父进程退出,停止子 daemon");
            stop = true;
            break;
        }
        // n < 0
        let err = io::Error::last_os_error();
        match err.kind() {
            io::ErrorKind::WouldBlock => break,     // 本轮排空
            io::ErrorKind::Interrupted => continue, // EINTR 重试
            _ => {
                tracing::error!(?err, "Fed 父 pipe read 错误,停止子 daemon");
                stop = true;
                break;
            }
        }
    }

    for f in frames {
        route_fed_frame(data, f);
    }

    if stop {
        data.loop_signal.stop();
        return Ok(PostAction::Remove);
    }
    Ok(PostAction::Continue)
}

/// 路由一条父喂来的帧(Fed 拓扑,块B):
/// - `PtyOutput` → 焦点流字节,经砖0 [`fan_out_bytes`] 进 ring + 各 live WS 客户端(本地渲染)。
/// - `FocusChange` → 更新焦点 (workspace, tab) + [`broadcast_stream_focus`] 给所有 live 客户端。
/// - `WorkspaceAdd` / `WorkspaceRemove` → 砖1a 仅留接口(控制面工作区元数据接线在砖1b)。
/// - `Input` → 子→父方向,父不该往子发,收到忽略(防协议误用)。
fn route_fed_frame(data: &mut DaemonData, frame: FeedFrame) {
    match frame.kind {
        FrameKind::PtyOutput => {
            if !frame.payload.is_empty() {
                fan_out_bytes(data, frame.payload);
            }
        }
        FrameKind::FocusChange => {
            data.workspace_id = frame.ws_id;
            data.tab_id = frame.tab_id;
            broadcast_stream_focus(data);
        }
        FrameKind::WorkspaceAdd | FrameKind::WorkspaceRemove => {
            tracing::debug!(
                ws_id = frame.ws_id,
                "Fed 收到 WorkspaceAdd/Remove 帧 (砖1a 预留接口,砖1b 接控制面元数据)"
            );
        }
        FrameKind::Input => {
            tracing::debug!("Fed 收到 Input 帧 (方向应为子→父),忽略");
        }
    }
}

/// 向所有 live WS 客户端推一条 [`ServerMsg::StreamFocus`] 控制帧(Text),声明此后 Binary 字节
/// 属于哪个 (workspace, active tab)。Fed 焦点切换(父发 [`FrameKind::FocusChange`])后调。
fn broadcast_stream_focus(data: &mut DaemonData) {
    let msg = ServerMsg::StreamFocus {
        workspace_id: data.workspace_id,
        tab_id: data.tab_id,
    };
    let text = match serde_json::to_string(&msg) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(?e, "序列化 StreamFocus 控制帧失败,跳过");
            return;
        }
    };
    let ids: Vec<u64> = data.clients.keys().copied().collect();
    for id in ids {
        let mut armed = false;
        if let Some(c) = data.clients.get_mut(&id) {
            if matches!(c.stage, WsStage::Live(_)) {
                c.outbound_len += text.len();
                c.outbound.push_back(OutFrame::Text(text.clone()));
                armed = true;
            }
        }
        if armed {
            arm_write(data, id);
        }
    }
}

/// 把 WS 客户端输入字节路由到 input sink(块C —— 在 `on_input` 调用点岔开两条数据流):
/// - **Local**(`session` 在)= 直写 active tab 的 PTY([`Session::on_input`]);
/// - **Fed**(`fed` 在)= **分帧**成 ≤ [`FED_INPUT_CHUNK`] 的多个 [`FrameKind::Input`] 帧(各带
///   同 (`ws_id`, `tab_id`) 标签)写父 back-channel(子自己不写 PTY,父 own PTY,父据 (ws, tab)
///   写对应 PTY)。分帧防单条大输入(粘贴)超 feed 协议 payload 上限 / `u32` 截断(Bug2)。
///
/// `ws_id`:**热路径 Binary** 用焦点 ws(无显式寻址);**寻址 [`ClientMsg::Input`]** 用消息里的
/// `workspace_id`(Bug3 —— 让 Input 帧 ws 标签填对值,而非一律焦点 ws)。Local 的 `on_input` 按
/// `tab_id` 寻址、不使用 `ws_id`。
fn route_input(data: &mut DaemonData, ws_id: u64, tab_id: u64, bytes: &[u8]) {
    if let Some(session) = data.session.as_mut() {
        if let Err(e) = session.on_input(tab_id, bytes) {
            tracing::warn!(?e, tab_id, "WS 输入写 PTY 失败");
        }
        return;
    }
    if data.fed.is_none() {
        return;
    }
    // Fed:分帧 + 逐帧经 back-channel 队列保序回灌父(空输入 → chunks 无项 → 不发帧)。复用
    // 同一 `frame` buffer(clear 后重填)减分配。
    let mut frame = Vec::with_capacity(FEED_HEADER_LEN + bytes.len().min(FED_INPUT_CHUNK));
    for chunk in bytes.chunks(FED_INPUT_CHUNK) {
        frame.clear();
        encode_into(&mut frame, FrameKind::Input, ws_id, tab_id, chunk);
        fed_send_frame(data, &frame);
    }
}

/// 把一帧 encode 好的字节经 back-channel 写父(子→父 Input 帧)。**绝不丢半帧**:队空时尽量
/// 直写(WouldBlock 写不完的剩余字节入队),队非空时直接追加(保序,不直写以免乱序);有积压
/// 则 arm WRITE 源,可写时 [`fed_back_writable`] drain。出站积压超 [`FED_BACK_OUT_CAP`] = 父长期
/// 不读 → **断子降级**(停子 daemon,父读 EOF 重建干净子),绝不无界堆 + 绝不丢半帧污染流。
/// **绝不阻塞 loop**(WouldBlock 即入队返回,不自旋等父读)。
fn fed_send_frame(data: &mut DaemonData, frame: &[u8]) {
    let mut fatal = false;
    let mut over_cap = false;
    let mut need_arm = false;
    if let Some(fed) = data.fed.as_mut() {
        let fd = fed.back.as_raw_fd();
        let mut start = 0;
        // 仅队空时直写(队非空直写会与待 drain 的字节乱序);写不完的入队。
        if fed.outbound.is_empty() {
            while start < frame.len() {
                // SAFETY: fd 来自本结构持有的活 OwnedFd;libc::write 只调 syscall 不动 fd 所有权,
                // frame 是调用方栈上活着的切片。
                #[allow(unsafe_code)]
                let n = unsafe {
                    libc::write(
                        fd,
                        frame[start..].as_ptr().cast::<libc::c_void>(),
                        frame.len() - start,
                    )
                };
                if n > 0 {
                    start += n as usize;
                    continue;
                }
                if n == 0 {
                    tracing::warn!("Fed back-channel 直写返回 0,停止子 daemon");
                    fatal = true;
                    break;
                }
                let err = io::Error::last_os_error();
                match err.kind() {
                    io::ErrorKind::Interrupted => continue,
                    io::ErrorKind::WouldBlock => break, // 写不完的入队
                    _ => {
                        tracing::warn!(?err, "Fed back-channel 直写失败,停止子 daemon");
                        fatal = true;
                        break;
                    }
                }
            }
        }
        if !fatal && start < frame.len() {
            fed.outbound.extend(&frame[start..]);
            over_cap = fed.outbound.len() > FED_BACK_OUT_CAP;
            need_arm = !over_cap;
        }
    }
    if fatal || over_cap {
        if over_cap {
            tracing::error!(
                cap = FED_BACK_OUT_CAP,
                "Fed back-channel 出站积压超上限 (父长期不读),停止子 daemon 降级"
            );
        }
        data.loop_signal.stop();
        return;
    }
    if need_arm {
        fed_arm_write(data);
    }
}

/// 打开 Fed back-channel WRITE 兴趣(出站有积压时)。已 armed 则跳过(防对已 enable 的源重复
/// enable)。从**非该 WRITE 源自身**的 callback 调([`fed_send_frame`],经 input sink),故
/// `enable` 立即生效。镜像 [`arm_write`](WS 客户端版)。
fn fed_arm_write(data: &mut DaemonData) {
    let token = match data.fed.as_ref().and_then(|f| f.write.as_ref()) {
        Some(w) if !w.armed => w.token,
        _ => return, // 无 WRITE 源(瞬态)/ 已 armed
    };
    if let Err(e) = data.loop_handle.enable(&token) {
        tracing::warn!(?e, "enable Fed back-channel WRITE 源失败");
        return;
    }
    if let Some(w) = data.fed.as_mut().and_then(|f| f.write.as_mut()) {
        w.armed = true;
    }
}

/// Fed back-channel WRITE 源可写:drain 出站字节队列到父 pipe。每次写当前 front 连续切片,
/// 推进游标;`WouldBlock` → 保留 WRITE 兴趣(`Continue`)下次再来;**队列排空 → 关 WRITE 兴趣
/// 退回不轮询(`Disable`,不忙等)**;写 0 / 其它错误(父读端没了)→ 停子 daemon(与父 pipe
/// EOF 察觉同义)。**绝不丢字节**(半帧致父 framing 错位)。
fn fed_back_writable(data: &mut DaemonData) -> io::Result<PostAction> {
    enum Outcome {
        Disable,
        Continue,
        Stop,
    }
    let outcome = {
        let Some(fed) = data.fed.as_mut() else {
            return Ok(PostAction::Remove); // 不该发生:WRITE 源仅 Fed 拓扑注册
        };
        let fd = fed.back.as_raw_fd();
        loop {
            // VecDeque 非空时 `as_slices().0`(front 连续段)必非空;空则 drain 完毕。
            let front = fed.outbound.as_slices().0;
            if front.is_empty() {
                if let Some(w) = fed.write.as_mut() {
                    w.armed = false;
                }
                break Outcome::Disable;
            }
            // SAFETY: fd 来自本结构持有的活 OwnedFd;front 是 `outbound` 当前内容的活切片;
            // libc::write 只调 syscall 不动 fd 所有权。
            #[allow(unsafe_code)]
            let n = unsafe { libc::write(fd, front.as_ptr().cast::<libc::c_void>(), front.len()) };
            if n > 0 {
                fed.outbound.drain(..n as usize);
                continue;
            }
            if n == 0 {
                tracing::warn!("Fed back-channel write 返回 0,停止子 daemon");
                break Outcome::Stop;
            }
            let err = io::Error::last_os_error();
            match err.kind() {
                io::ErrorKind::Interrupted => continue,
                io::ErrorKind::WouldBlock => break Outcome::Continue,
                _ => {
                    tracing::warn!(?err, "Fed back-channel write 失败,停止子 daemon");
                    break Outcome::Stop;
                }
            }
        }
    };
    match outcome {
        Outcome::Disable => Ok(PostAction::Disable),
        Outcome::Continue => Ok(PostAction::Continue),
        Outcome::Stop => {
            data.loop_signal.stop();
            Ok(PostAction::Remove)
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
            held_ws: None,
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

    // 转 Live = 隐式 Hold 本 daemon 的 active 工作区(引用计数 +1,T6 块C)。即便客户端
    // 不发 ClientMsg::Hold(如旧 web / 测试)也登记为 holder → 断线/关闭都经统一回收路径
    // 释放,无 holder 泄漏。anchor 在(daemon 自锚)故这不会让工作区"该死不死"。
    let ws_id = data.workspace_id;
    // 控制面连上引导帧:全部工作区列表 + 当前工作区结构 + 字节流 (workspace,tab) 标签。
    let control = build_control_text(data, ws_id);
    let replay = data.ring.snapshot();
    if let Some(c) = data.clients.get_mut(&id) {
        c.stage = WsStage::Live(ws);
        // holder 仅 Local(有 Session)登记:先确认连接在册再登记 holder + held_ws(二者一起
        // 设)→ 杜绝登记了 holder 却没记 held_ws、回收时无从释放的孤立态。Fed 子的工作区生命
        // 周期归父(refcount→父信号留砖1b),子只用 `clients` 表做 fan-out + 回收 → held_ws 留
        // None,回收路径 [`release_holder`] 自然 no-op。
        if let Some(session) = data.session.as_mut() {
            session.hold(ws_id, id);
            c.held_ws = Some(ws_id);
        }
        // 刚 insert 即 enable 状态;有重放/控制帧就让它去排空,无则首次可写发现队空自 Disable。
        c.write = Some(WriteReg {
            token: write_token,
            armed: true,
        });
        // 控制面 (Text 帧) 先于数据面字节排出:客户端先知道工作区/焦点再收字节流。
        for s in control {
            c.outbound_len += s.len();
            c.outbound.push_back(OutFrame::Text(s));
        }
        // 数据面:环缓冲重放重建当前屏 (Binary 帧)。
        if !replay.is_empty() {
            c.outbound_len += replay.len();
            c.outbound.push_back(OutFrame::Bytes(Bytes::from(replay)));
        }
    }
    Ok(PostAction::Continue)
}

/// 连上时下发的控制面引导帧。按拓扑分发:**Local** 用真 [`Session`] 元数据
/// ([`build_control_text_local`]);**Fed** 用父喂的焦点合成最小引导 ([`build_control_text_fed`])。
fn build_control_text(data: &DaemonData, ws_id: u64) -> Vec<String> {
    match (&data.session, &data.fed) {
        (Some(session), _) => build_control_text_local(session, ws_id),
        (None, Some(_)) => build_control_text_fed(data, ws_id),
        (None, None) => Vec::new(),
    }
}

/// Fed 拓扑连上引导帧(块B):砖1a 子无 Session,控制面工作区元数据(tab 明细)接线在砖1b
/// (父经 `WorkspaceAdd` 帧喂)。当前最小引导 = 一条单工作区列表 + 字节流 (workspace, tab) 标签
/// ([`ServerMsg::StreamFocus`]),让客户端先知道焦点;数据面忠实字节流兜底渲染。
fn build_control_text_fed(data: &DaemonData, ws_id: u64) -> Vec<String> {
    let mut out = Vec::new();
    push_server_json(
        &mut out,
        &ServerMsg::Workspaces(WorkspaceList {
            workspaces: vec![WorkspaceMeta {
                id: ws_id,
                title: String::new(),
                tab_count: 1,
                active: true,
            }],
            active: ws_id,
        }),
    );
    push_server_json(
        &mut out,
        &ServerMsg::StreamFocus {
            workspace_id: ws_id,
            tab_id: data.tab_id,
        },
    );
    out
}

/// Local 拓扑连上引导帧 (T6 块C,用起 `ServerMsg::Workspaces/Workspace/StreamFocus`):
/// ① 全部工作区列表 ② 当前工作区结构 (tab 元数据) ③ 字节流标签 (此后 Binary 帧属于哪个
/// (workspace, active tab))。各序列化成 JSON Text 帧;序列化失败的单条跳过 (不致命)。
fn build_control_text_local(session: &Session, ws_id: u64) -> Vec<String> {
    let mut out = Vec::new();
    push_server_json(&mut out, &ServerMsg::Workspaces(session.workspace_list()));
    if let Some(info) = session.workspace_info(ws_id) {
        let focus_tab = info.tabs.get(info.active).map(|t| t.tab_id);
        push_server_json(&mut out, &ServerMsg::Workspace(info));
        if let Some(tab_id) = focus_tab {
            push_server_json(
                &mut out,
                &ServerMsg::StreamFocus {
                    workspace_id: ws_id,
                    tab_id,
                },
            );
        }
    }
    out
}

/// 序列化一条 [`ServerMsg`] 进控制帧文本列表;失败仅 warn 跳过 (控制面非幂等丢一条
/// 不致命,客户端有忠实字节流兜底)。
fn push_server_json(out: &mut Vec<String>, msg: &ServerMsg) {
    match serde_json::to_string(msg) {
        Ok(s) => out.push(s),
        Err(e) => tracing::warn!(?e, "序列化 ServerMsg 控制帧失败,跳过"),
    }
}

/// Live READ 可读:`ws.read()` 排空到 WouldBlock,每条数据帧字节直接 [`Session::on_input`]
/// 写 active tab PTY(同线程,无 channel)。Close / 致命错 → 断开。
fn ws_live_read(data: &mut DaemonData, id: u64) -> io::Result<PostAction> {
    let tab = data.tab_id;
    // 热路径 Binary 无显式寻址 → 用焦点 (workspace, tab)(Bug3:仅带 workspace_id 字段的
    // ClientMsg::Input 才用消息字段,见 handle_client_msg)。
    let ws = data.workspace_id;
    loop {
        let res = match data.clients.get_mut(&id) {
            Some(c) => match &mut c.stage {
                WsStage::Live(ws) => ws.read(),
                _ => return Ok(PostAction::Continue),
            },
            None => return Ok(PostAction::Remove),
        };
        match res {
            // 数据面:浏览器输入裸字节(WS Binary)→ input sink(块C):Local 直写焦点 tab PTY,
            // Fed encode Input 帧回灌父 back-channel(子自己不写 PTY)。焦点 tab = StreamFocus 标的。
            Ok(Message::Binary(b)) => {
                route_input(data, ws, tab, &b);
            }
            // 控制面:WS Text = JSON [`ClientMsg`](Hold / Release / 寻址 Input / Resize)。
            // 与数据面 Binary 分流(T6)。Release 触发显式关闭回收 → 返 drop action。
            Ok(Message::Text(t)) => {
                if let Some(action) = handle_client_msg(data, id, t.as_str()) {
                    return Ok(action);
                }
            }
            // WS Close 帧 = 对端优雅关闭(浏览器关 tab / 导航离开)= **显式关闭** → explicit
            // 释放 holder(anchor 在则不销毁,只关这个 view)→ 回收连接。区别于**断线**
            // (RST / 超时,无 Close 帧 → 走下面错误分支 = 非事件释放)。
            Ok(Message::Close(_)) => {
                release_holder(data, id, true);
                return Ok(drop_client_read_self(data, id));
            }
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

/// 处理一条控制面 [`ClientMsg`](WS Text 帧的 JSON)。返回 `Some(action)` 表示该连接应
/// 按此 `PostAction` 回收(仅 `Release` 走此路),`None` 表示继续。解析失败仅 debug 跳过
/// (控制面坏消息不该拖垮连接)。
///
/// **生命周期(T6 块C)**:`Hold` = 登记 holder;`Release` = **显式 X 关闭** → explicit
/// 释放该连接所持工作区的 holder(anchor 在则不销毁,只关这个 view)+ 回收连接。寻址
/// `Input` / `Resize` 直接落 Session。`TabOp` 砖0 不接线(daemon 字节泵单 tab,tab 增删的
/// PTY 路由是砖1 tee/多泵的事)。
fn handle_client_msg(data: &mut DaemonData, id: u64, text: &str) -> Option<PostAction> {
    let msg: ClientMsg = match serde_json::from_str(text) {
        Ok(m) => m,
        Err(e) => {
            tracing::debug!(?e, "WS 收到无法解析的控制消息,忽略");
            return None;
        }
    };
    match msg {
        ClientMsg::Hold { workspace_id } => {
            // 仅 Local(有 Session)登记 holder;且仅 hold 成功(工作区存在)才记 held_ws,否则
            // 会把已持有的真工作区记录覆盖成无效 id → 原 holder 孤立、refcount 永不归 0(砖0 单
            // 工作区+恒 anchored 触发不到,砖1 多 workspace 会咬)。held_ws 单槽的真·多持有语义留
            // 砖1 升 HashSet。Fed 子无 Session,工作区生命周期归父 → Hold no-op。
            if let Some(session) = data.session.as_mut() {
                if session.hold(workspace_id, id) {
                    if let Some(c) = data.clients.get_mut(&id) {
                        c.held_ws = Some(workspace_id);
                    }
                }
            }
            None
        }
        ClientMsg::Release { .. } => {
            // 显式 X 关闭:释放本连接所持工作区的 holder(explicit;anchor 在则不销毁),
            // 然后回收连接。用连接实际持有的 held_ws(而非消息里的 id),砖0 单工作区两者一致。
            release_holder(data, id, true);
            Some(drop_client_read_self(data, id))
        }
        ClientMsg::Input {
            workspace_id,
            tab_id,
            bytes,
            ..
        } => {
            // 寻址输入走同一 input sink(块C):Local 写 PTY,Fed 回灌父 back-channel。
            // 用消息里的 workspace_id(Bug3:寻址路径让 Input 帧 ws 标签填对值,而非一律焦点 ws)。
            route_input(data, workspace_id, tab_id, &bytes);
            None
        }
        ClientMsg::Resize {
            workspace_id,
            cols,
            rows,
        } => {
            if let Some(session) = data.session.as_mut() {
                if let Err(e) = session.resize(workspace_id, cols, rows) {
                    tracing::warn!(?e, workspace_id, "WS resize 失败");
                }
            } else {
                // Fed:resize 须回灌父(父 own PTY)。砖1a 帧集不含 Resize 帧,留砖1b。
                tracing::debug!(cols, rows, "Fed 拓扑 resize 暂不回灌父(砖1b)");
            }
            None
        }
        ClientMsg::TabOp { .. } => {
            tracing::debug!("收到 TabOp 控制消息:砖0 daemon 单 tab 字节泵不接线(砖1 多 tab 泵)");
            None
        }
    }
}

/// 释放一条连接持有的工作区 holder(引用计数 −1,T6 块C)。`explicit` = 是否显式关闭
/// (`Release`/`Close` 为 `true` → 归 0 销毁工作区;断线为 `false` → 非事件,绝不销毁)。
///
/// 用 `held_ws.take()` 消费连接持有记录:① 幂等(显式关闭路径先调一次、紧跟的 drop 路径
/// 再调一次 → 第二次 `take()` 得 `None` 无操作);② 防 holder 泄漏(每条断开路径都经此释放)。
/// 销毁(仅当无 anchor + 无其它 holder)记 info 日志。daemon 自锚的工作区 refcount 恒 ≥ 1,
/// 故其 PTY(字节泵注册的 fd)绝不会被本路径销毁。
fn release_holder(data: &mut DaemonData, id: u64, explicit: bool) {
    let held = match data.clients.get_mut(&id) {
        Some(c) => c.held_ws.take(),
        None => None,
    };
    if let Some(ws_id) = held {
        // 仅 Local 有 Session 可释放;Fed 子 held_ws 恒 None(见 ws_go_live),故走不到这里。
        if let Some(session) = data.session.as_mut() {
            if session.release(ws_id, id, explicit) == Lifecycle::Destroyed {
                tracing::info!(
                    ws_id,
                    id,
                    "工作区引用计数归 0,已销毁(drop TabList → PTY SIGHUP)"
                );
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
        // 3. 灌进 ws(回到循环顶 flush 它)。数据面字节走 Binary 帧、控制面 JSON 走 Text 帧。
        let write_res = match data.clients.get_mut(&id) {
            Some(c) => match &mut c.stage {
                WsStage::Live(ws) => match frame {
                    OutFrame::Bytes(b) => ws.write(Message::Binary(b)),
                    OutFrame::Text(s) => ws.write(Message::Text(s.into())),
                },
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
    // 释放 holder(非 explicit = 断线语义,绝不销毁;显式关闭路径已先 explicit 释放过、
    // held_ws 被 take 空,这里 no-op)→ 防 holder 泄漏(T6 块C)。
    release_holder(data, id, false);
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
    release_holder(data, id, false); // 断线语义释放 holder,防泄漏(见 read_self 同注)
    if let Some(conn) = data.clients.remove(&id) {
        data.loop_handle.remove(conn.read_token); // READ 源:DEL + 关 original
    }
    PostAction::Remove
}

/// 从**与该连接无关的** callback(如 pty_readable 背压断开 / 收割 Timer)回收一条连接:
/// 两个源都经 `loop_handle.remove` 注销 + 摘登记项。
fn drop_client_external(data: &mut DaemonData, id: u64) {
    release_holder(data, id, false); // 断线语义释放 holder(cap 断开 / 收割都算断线,非 X)
    if let Some(conn) = data.clients.remove(&id) {
        data.loop_handle.remove(conn.read_token);
        if let Some(w) = conn.write {
            data.loop_handle.remove(w.token);
        }
    }
}

/// 收割 Timer 回调:回收 ① 卡在 Peeking/Handshaking 阶段超 `handshake_deadline`(自 accept
/// 起的绝对期限)的连接,防半截头/卡握手/slowloris 长占 `clients` 槽;② 超 `http_write_deadline`
/// 仍没把响应写完(drain)的 HTTP 响应写源,防 slowloris-read(要了大页却永不读 → 写源恒
/// WouldBlock 挂着 + dup fd 不回收,不受 `MAX_WS_CONNS` 限、①那支也扫不到)。两者皆**绝对
/// 期限**(非"距上次活动"),蚂蚁搬家/极小窗口都逃不掉。**live 连接不收割**(健康但安静的
/// 终端无收发也属正常,按空闲超时会误杀)——其半开掉线靠 SO_KEEPALIVE。
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

    // ② HTTP 响应写源:超期未 drain 的 → remove 写源(关其 dup fd)+ 摘登记。正常响应早已在
    // http_write_pending drain 时自删,扫到的都是真卡死的(slowloris-read)。先收集再删,避免
    // 借 http_writers 的同时改它;remove 对已失效 token 是 no-op(calloop 内部容忍)。
    let http_deadline = data.http_write_deadline;
    let stale_http: Vec<(u64, RegistrationToken)> = data
        .http_writers
        .iter()
        .filter(|(_, w)| now.duration_since(w.created_at) > http_deadline)
        .map(|(id, w)| (*id, w.token))
        .collect();
    for (id, token) in stale_http {
        tracing::debug!(
            id,
            "HTTP 响应写源未在期限内写完,收割(防 slowloris-read 堆积)"
        );
        data.loop_handle.remove(token);
        data.http_writers.remove(&id);
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

/// 给 HTTP 响应 socket 设 `SO_SNDBUF` 上限(见 [`HTTP_SEND_BUF_BYTES`]:封顶单连接内核 send
/// 内存 + 让大响应发给"永不读"客户端时真撞 `WouldBlock`,从而写超时收割能见到并回收它)。
/// 尽力而为,失败容忍(老内核/受限环境拒绝也不影响主功能,只是退回内核默认 send 缓冲)。
#[allow(unsafe_code)]
fn cap_http_send_buffer(stream: &TcpStream) {
    let fd = stream.as_raw_fd();
    let val = HTTP_SEND_BUF_BYTES;
    // SAFETY: setsockopt 只读取栈上一个 c_int 的 size_of 字节(只读不写),fd 来自本函数参数里
    // 活着的 TcpStream;返回值忽略(尽力而为)。
    unsafe {
        libc::setsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_SNDBUF,
            std::ptr::addr_of!(val).cast::<libc::c_void>(),
            std::mem::size_of::<libc::c_int>() as libc::socklen_t,
        );
    }
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
///
/// 写源登记进 [`DaemonData::http_writers`](带建立时刻),让收割 Timer 能扫到并回收**永不
/// drain** 的写源(slowloris-read:要了大页却不读 / 通告极小窗口 → 写源恒 WouldBlock 挂着);
/// drain 完即在 [`http_write_pending`] 里摘登记。**没这条登记 + 扫描,该写源不受
/// `MAX_WS_CONNS` 限、握手收割也扫不到(分流时已 `clients.remove`),会被 slowloris-read 堆爆。**
fn serve_http_response(data: &mut DaemonData, stream: TcpStream, head: &[u8]) {
    let _ = stream.set_nonblocking(true); // dup 已继承 original 的非阻塞,稳妥再设一次
    cap_http_send_buffer(&stream); // 见 HTTP_SEND_BUF_BYTES:封顶内核 send 内存 + 让卡死写源可见可收割
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
    let id = data.next_http_writer_id;
    data.next_http_writer_id = data.next_http_writer_id.wrapping_add(1);
    match data.loop_handle.insert_source(
        Generic::new(stream, Interest::WRITE, Mode::Level),
        move |_readiness, meta, data: &mut DaemonData| {
            http_write_pending(data, id, meta.as_ref(), &pending)
        },
    ) {
        Ok(token) => {
            data.http_writers.insert(
                id,
                HttpWriter {
                    token,
                    created_at: Instant::now(),
                },
            );
        }
        Err(e) => tracing::warn!(?e, "注册 HTTP 响应写源失败"),
    }
}

/// HTTP 响应写源回调:复用 [`write_pending`] 写字节,但在它返 `Remove`(全部写完 / 出错)
/// 时**顺手摘掉 `http_writers` 登记**——这样 drain 完的正常响应不会被收割 Timer 再扫到
/// (源已自删,token 失效),收割只剩真卡死的写源。
fn http_write_pending(
    data: &mut DaemonData,
    id: u64,
    stream: &TcpStream,
    pending: &Rc<RefCell<WritePending>>,
) -> io::Result<PostAction> {
    let action = write_pending(stream, pending)?;
    if matches!(action, PostAction::Remove) {
        data.http_writers.remove(&id);
    }
    Ok(action)
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
    // unix-dump 仅 Local 拓扑注册(Fed 子无 server term 可 dump)→ session 必 Some。
    let snap = match data.session.as_ref() {
        Some(s) => s.snapshot_active(),
        None => bail!("Fed 拓扑无 server term,unix-dump 快照不可用"),
    };
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
            workspace_id: 1,
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
