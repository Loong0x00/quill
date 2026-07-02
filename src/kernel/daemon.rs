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
use std::collections::{HashMap, HashSet, VecDeque};
use std::io::{self, Read, Write};
use std::net::{IpAddr, Shutdown, SocketAddr, TcpListener, TcpStream};
use std::os::fd::{AsRawFd, BorrowedFd, FromRawFd, IntoRawFd, OwnedFd, RawFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use calloop::generic::Generic;
use calloop::signals::{Signal, Signals};
use calloop::timer::{TimeoutAction, Timer};
use calloop::{EventLoop, Interest, LoopHandle, LoopSignal, Mode, PostAction, RegistrationToken};
use tungstenite::handshake::server::{Callback, ErrorResponse, Request, Response, ServerHandshake};
use tungstenite::handshake::MidHandshake;
use tungstenite::protocol::WebSocketConfig;
use tungstenite::{Bytes, Error as WsError, HandshakeError, Message, WebSocket};

use crate::kernel::feed::{
    decode_dims, decode_tab_list, encode_into, encode_tab_op, FeedDecoder, FeedFrame, FeedTabOp,
    FrameKind, FEED_HEADER_LEN,
};
use crate::kernel::proto::{
    ClientMsg, ServerMsg, Snapshot, TabMeta, TabOp, WorkspaceInfo, WorkspaceList, WorkspaceMeta,
};
use crate::kernel::session::{Lifecycle, Session};
use crate::tab::{TabInstance, TabList};

/// 内嵌的浏览器镜像页与 vendored xterm.js 资产(`include_str!` 路径相对**本源文件**
/// `src/kernel/daemon.rs`)。同口 HTTP serve 用 —— 编译期嵌进二进制,运行时零文件
/// 系统依赖(开箱即用 + 手机经 WireGuard VPN 离线可达,不靠 CDN)。
const INDEX_HTML: &str = include_str!("../../assets/web/index.html");
const XTERM_JS: &str = include_str!("../../assets/web/vendor/xterm.js");
const XTERM_CSS: &str = include_str!("../../assets/web/vendor/xterm.css");
const XTERM_FIT_JS: &str = include_str!("../../assets/web/vendor/xterm-addon-fit.js");
/// 砖2 W3:app-shell Service Worker(cache-first 缓存静态壳,弱网首屏快)。同 include_str!
/// 烤进二进制,由 [`serve_http_response`] 从根 `/sw.js` serve(scope 覆盖整站)。
const SW_JS: &str = include_str!("../../assets/web/sw.js");

/// 砖2 W3:app-shell 内容哈希 = Service Worker 缓存版本 key(FNV-1a 64-bit,编译期算)。
/// 任一内嵌资产(含 sw.js 自身)改动 → 哈希变 → serve /sw.js 时注入的 `__QUILL_VERSION__`
/// 变 → sw.js 字节变 → 浏览器检测到新 SW → activate 清旧版本缓存(重建后旧 UI 不卡)。
/// 用内容哈希而非 crate 版本:UI 常改不 bump 版本号,内容哈希才能自动作废旧缓存。
const ASSET_VERSION: u64 = {
    let h = fnv1a64(0xcbf2_9ce4_8422_2325, INDEX_HTML.as_bytes());
    let h = fnv1a64(h, XTERM_JS.as_bytes());
    let h = fnv1a64(h, XTERM_CSS.as_bytes());
    let h = fnv1a64(h, XTERM_FIT_JS.as_bytes());
    fnv1a64(h, SW_JS.as_bytes())
};

/// FNV-1a 64-bit 增量哈希(`const fn`,供 [`ASSET_VERSION`] 编译期折叠资产内容)。
const fn fnv1a64(mut hash: u64, bytes: &[u8]) -> u64 {
    let mut i = 0;
    while i < bytes.len() {
        hash ^= bytes[i] as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        i += 1;
    }
    hash
}

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

/// WS 握手显式收紧的单条消息 / 单帧上限(灭安全审计 #2/#14:默认 tungstenite `WebSocketConfig`
/// 是 64 MiB 消息 / 16 MiB 帧 → 单条大 Text 帧一次性 UTF-8 校验 + `serde_json` 全量解析可瞬时吃
/// 数十 MB + 卡顿单线程 daemon)。输入只有按键(Binary,web 端已 ≤64KiB 分块)与小控制 JSON
/// (Text),1 MiB 上限远超正常所需、又封死放大面。**必须在握手 config 里给** —— tungstenite
/// 用它初始化 `WebSocket`,之后 `read` 按此上限拒超大帧。
const WS_MAX_MESSAGE_SIZE: usize = 1 << 20;

/// 环境变量:kernel 启动时读取的共享鉴权 token(P0 CSWSH 修复)。owner 窗口(`quill --share`)从
/// 持久文件 `~/.config/quill/token` 读出后经此 env 传给 spawn 的 detached kernel(见 window.rs
/// `share_spawn_detached_kernel`);缺失时 kernel 生成临时 token 并 `warn`(绝不裸奔放行)。
const ENV_SHARE_TOKEN: &str = "QUILL_SHARE_TOKEN";

/// 环境变量:Host/Origin 允许集的**额外**主机名(逗号分隔,如 DDNS 域 `vpn.example.com`)。
/// 默认允许集已含 localhost + 任意 IP 字面量(见 [`host_is_allowed`]),此 env 仅用于放行用户自有
/// 域名(纵深防御,不误杀合法访问)。
const ENV_ALLOWED_HOSTS: &str = "QUILL_SHARE_ALLOWED_HOSTS";

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

/// Federated 启动宽限:owner 拉起 kernel 后须在此期限内 connect 成第一个 feeder,否则 kernel 判自己
/// 是孤儿(owner 崩 / connect 失败)退出免残留。10s 远超本机 bind + connect 的毫秒级往返。
const FED_STARTUP_GRACE_MS: u64 = 10_000;

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

/// daemon 的字节来源拓扑 (ADR-0018 E′ / ADR-0019 联邦)。决定字节从哪来、输入往哪去、是否 own PTY。
pub enum SourceConfig {
    /// **Local**(standalone `quill-kernel`):自 spawn shell + own 真 PTY,[`Session`] 驱动
    /// 一切(unix-dump 快照 + WS 直播 + 输入直写 PTY)。砖0 / 今天的 daemon。
    Local,
    /// **Fed**(E′ 共享子进程,单 feeder via 继承 pipe):字节从父 pipe 来、**不 spawn shell /
    /// 不开真 PTY / 不绑 unix socket**;WS 输入回灌父 back-channel(子自己不写 PTY,父 own PTY)。
    /// `read_fd` = 父→子(PtyOutput / FocusChange / TabList / Dims 帧),`write_fd` = 子→父(Input /
    /// TabOp 帧)。两 fd 由父在 spawn 时经继承传入(见 [`parse_fed_source`])。**启动即建一个
    /// [`Feeder`]**(= 一个 workspace);其 pipe EOF = 最后一个 feeder 断 → kernel 退。
    /// `tests/kernel_feed_slice.rs` 走这条(合成父经 pipe 喂一个 feeder)。
    Fed { read_fd: RawFd, write_fd: RawFd },
    /// **Federated**(ADR-0019 机器级单例 kernel):在 `rendezvous_path` 上 `bind` 一个
    /// [`UnixListener`],**accept N 个 feeder 连接**(每个 quill 窗口一条),**每个 feeder = 一个
    /// workspace**;手机看全部 workspace 聚合。feeder 连接是全双工 unix socket(读 = feed 帧,
    /// 写 = back-channel Input/TabOp)。启动时 feeders 空(窗口经会合陆续接入);**最后一个 feeder
    /// 断 → kernel 退**(清会合 socket + 释 7878)。第一个接入 = 锚(新 tab 落点,F3)。
    Federated { rendezvous_path: PathBuf },
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

/// 从 argv 抠 `--rendezvous=<path>`(ADR-0019 联邦会合 socket)。给了 → [`SourceConfig::Federated`]
/// (在该 path bind UnixListener accept N feeder);没给 → `None`(调用方按 `--fed-in/out` /
/// Local 决定)。与 `--socket`(Local unix-dump)正交,不共用。
pub fn parse_rendezvous_arg(args: &[String]) -> Option<PathBuf> {
    const PREFIX: &str = "--rendezvous=";
    args.iter()
        .skip(1)
        .find_map(|a| a.strip_prefix(PREFIX).map(PathBuf::from))
}

/// 从 argv 判断是否给了裸开关 `--detach`(ADR-0019:kernel 自 daemonize 双 fork 脱离发起窗口,
/// 成机器级单例、不随窗口退出而死、不再是窗口的直接子)。见 [`daemonize`]。
pub fn parse_detach_arg(args: &[String]) -> bool {
    args.iter().skip(1).any(|a| a == "--detach")
}

/// 双 fork daemonize(ADR-0019 F1a detach):使本进程脱离发起窗口 —— fork1 后原进程(窗口的
/// 直接子)`_exit`,窗口 `wait` 立即收尸(无僵尸);`setsid` 开新会话脱离控制终端;fork2 后
/// 会话首领 `_exit`(daemon 非会话首领,永不重获控制 tty);孙进程 = daemon 被 init 收养,**不
/// 随窗口退出而死、不再是窗口子进程**。最后把 stdin/stdout/stderr 重定向到 `/dev/null`(真
/// daemon,不占窗口的 tty/pipe、不吐噪声)。
///
/// **调用时机**:`quill-kernel` main **最早**调(尚单线程 —— 未 init tracing、未 spawn 任何
/// 线程),fork 与 exec 间只走 async-signal-safe 系统调用(`fork` / `setsid` / `_exit` /
/// `open` / `dup2` / `close`),不触发 fork-in-multithreaded UB。仅 Federated(`--detach`)走这条;
/// Local / Fed(测试)不 detach。
#[allow(unsafe_code)]
pub fn daemonize() -> Result<()> {
    // SAFETY: fork/setsid/_exit/open/dup2/close 均 async-signal-safe;本进程此刻单线程(main 最早
    // 调用,未 init tracing / 未起线程),父/会话首领分支只调 `_exit` 不跑析构。
    unsafe {
        match libc::fork() {
            -1 => bail!("daemonize fork1 失败: {}", io::Error::last_os_error()),
            0 => {}              // 子:继续
            _ => libc::_exit(0), // 父(窗口的直接子):退出 → 窗口 wait 收尸
        }
        if libc::setsid() < 0 {
            bail!("daemonize setsid 失败: {}", io::Error::last_os_error());
        }
        match libc::fork() {
            -1 => bail!("daemonize fork2 失败: {}", io::Error::last_os_error()),
            0 => {}              // 孙:daemon 本体
            _ => libc::_exit(0), // 会话首领退出(daemon 非首领,不重获 tty)
        }
        // 重定向标准流到 /dev/null(不占窗口 tty/pipe、不吐噪声)。失败容忍(daemon 照跑)。
        let devnull = libc::open(c"/dev/null".as_ptr(), libc::O_RDWR);
        if devnull >= 0 {
            libc::dup2(devnull, libc::STDIN_FILENO);
            libc::dup2(devnull, libc::STDOUT_FILENO);
            libc::dup2(devnull, libc::STDERR_FILENO);
            if devnull > libc::STDERR_FILENO {
                libc::close(devnull);
            }
        }
    }
    Ok(())
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
    // **无 channel / 无线程**(去掉了 ADR-0016 线程版的 mpsc + WsServer)。listener bind 前设
    // `SO_REUSEADDR`(见 [`bind_ws_listener`]):运行期共享开关 toggle-off→on 重绑同口时,放行
    // 仍在 TIME-WAIT 的旧 established 连接占着的本地 (addr,port),否则 ~60s 内新子撞 EADDRINUSE。
    let ws_listener = bind_ws_listener(config.ws_bind)
        .with_context(|| format!("WS TcpListener bind {} 失败", config.ws_bind))?;
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

    // 按拓扑建 字节来源:
    // - **Local**(ADR-0018):spawn shell + own 真 PTY(注册 PTY READ 源 + unix-dump listener),
    //   Session 在,feeders 空;
    // - **Fed**(ADR-0018,测试):启动即从继承 pipe 建【一个】feeder(= 一个 workspace),Session
    //   不在;其 pipe EOF = 最后 feeder 断 → kernel 退;
    // - **Federated**(ADR-0019):bind 会合 UnixListener,accept N 个 feeder;启动 feeders 空,窗口
    //   经会合陆续接入;每 feeder = 一个 workspace;最后 feeder 断 → kernel 退(清会合 socket)。
    let mut session: Option<Session> = None;
    let mut feeders: HashMap<u64, Feeder> = HashMap::new();
    let mut anchor_feeder: Option<u64> = None;
    let mut next_feeder_id: u64 = 1;
    let mut tab_id: u64 = 0;
    let mut workspace_id: u64 = 0;
    let mut dims: Option<(u16, u16)> = None;
    let mut local_socket: Option<PathBuf> = None;
    let mut rendezvous_socket: Option<PathBuf> = None;
    match &config.source {
        SourceConfig::Local => {
            // 信号已在主线程 block,现在才 spawn shell tab(其 worker 线程继承 block)。
            let tab = TabInstance::spawn(config.cols, config.rows)
                .context("daemon 启动:spawn shell tab 失败")?;
            tab_id = tab.id().raw();
            let mut sess = Session::new(TabList::new(tab));
            // standalone daemon = 自己是 anchor(谁 spawn 工作区谁是隐式锚,ADR-0018):anchor 在 →
            // WS 客户端全断 / 全显式关闭工作区都【不死】;只有子 shell PTY EOF 才退。
            workspace_id = sess.active_workspace_id();
            sess.set_anchor(workspace_id, true);
            dims = Some((config.cols, config.rows)); // Local own PTY → 启动尺寸已知。

            // Source: PTY master fd(从 local session 取,move 进 data 前)。
            let pty_fd = sess.tabs().active().pty().raw_fd();
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

            // Source: UnixListener(quill-dump 调试路径,与 WS 字节流独立,Local 限定)。
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

            session = Some(sess);
            local_socket = Some(config.socket_path.clone());
        }
        SourceConfig::Fed { read_fd, write_fd } => {
            // 单 feeder via 继承 pipe(测试 / 旧 E′):启动即建一个 feeder = 一个 workspace = 锚。
            // 两 fd 是父继承传入的独立 pipe 端(read_fd != write_fd),归 make_feeder 接管(契约:成功=
            // own、失败=已自行 close);本 caller 从不另行 close,故契约自洽、无双关。
            let id = next_feeder_id;
            next_feeder_id += 1;
            let feeder = make_feeder(&loop_handle, id, *read_fd, *write_fd)
                .context("Fed 模式建 feeder 失败")?;
            feeders.insert(id, feeder);
            anchor_feeder = Some(id);
        }
        SourceConfig::Federated { rendezvous_path } => {
            // 会合 UnixListener:accept N feeder(每窗口一条全双工 socket)。owner(第一个开共享的
            // 窗口)经 detach 拉起本 kernel、它自己随即 connect 成第一个 feeder = 锚(F3)。
            prepare_socket_path(rendezvous_path)?;
            let listener = UnixListener::bind(rendezvous_path).with_context(|| {
                format!("bind 会合 UnixListener {} 失败", rendezvous_path.display())
            })?;
            listener
                .set_nonblocking(true)
                .context("会合 UnixListener 设非阻塞失败")?;
            loop_handle
                .insert_source(
                    Generic::new(listener, Interest::READ, Mode::Level),
                    |_readiness, meta, data: &mut DaemonData| {
                        feeder_accept_ready(meta.as_ref(), data)
                    },
                )
                .map_err(|e| anyhow!("calloop insert_source(会合 listener) 失败: {e}"))?;
            rendezvous_socket = Some(rendezvous_path.clone());
        }
    };

    // P0 CSWSH 修复:WS 鉴权 token(env 下发的持久 token / 缺则临时 token)+ Host/Origin 允许集。
    // 在建 DaemonData 前解析,任一失败(极罕见的 getrandom 故障)早退,不半配置起服务。
    let ws_token = load_or_generate_token()?;
    let allowed_hosts = Rc::new(build_allowed_hosts(config.ws_bind));

    let mut data = DaemonData {
        session,
        feeders,
        anchor_feeder,
        next_feeder_id,
        federated: matches!(config.source, SourceConfig::Federated { .. }),
        loop_handle: loop_handle.clone(),
        loop_signal,
        tab_id,
        workspace_id,
        dims,
        rings: HashMap::new(),
        clients: HashMap::new(),
        next_client_id: 1,
        handshake_deadline,
        http_writers: HashMap::new(),
        next_http_writer_id: 1,
        http_write_deadline,
        ws_token,
        allowed_hosts,
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

    // Federated:一次性 startup grace timer。owner 拉起本 kernel 后应【立即】connect 成第一个
    // feeder;若 grace 到期仍零 feeder(owner 崩了 / connect 失败 = 孤儿 kernel),退出免残留
    // (清会合 socket + 释 7878)。正常路径 grace 前 feeder 已接入,此检查不触发;全 feeder 断
    // 由 [`feeder_teardown`] 的"最后一个断→退"兜(那条已 stop,grace 到期时 loop 已退不会再跑)。
    if data.federated {
        let grace = duration_from_env("QUILL_FED_STARTUP_GRACE_MS", FED_STARTUP_GRACE_MS);
        loop_handle
            .insert_source(
                Timer::from_duration(grace),
                |_deadline, _meta, data: &mut DaemonData| {
                    if data.feeders.is_empty() {
                        tracing::info!("Federated 启动宽限到期仍零 feeder(孤儿 kernel),退出");
                        data.loop_signal.stop();
                    }
                    TimeoutAction::Drop // 一次性
                },
            )
            .map_err(|e| anyhow!("calloop insert_source(federated grace timer) 失败: {e}"))?;
    }

    tracing::info!(
        ws_bind = %config.ws_bind,
        federated = data.federated,
        feeders = data.feeders.len(),
        "quill-kernel daemon 就绪 (单线程 calloop + WS 同一 loop)"
    );

    let run_result = event_loop
        .run(None, &mut data, |_data| {})
        .context("calloop EventLoop::run 失败");

    // 显式 drop 序:event_loop(持各 Generic source,含 WS 连接源 + PTY/feeder pipe 源)先于
    // data(持 PtyHandle / feeder back-channel / WS WebSocket 的 fd),让源的 EPOLL_CTL_DEL
    // 在对应 fd 仍打开时执行(见上 SAFETY)。WS 无线程可 join,drop 即收尾。
    drop(event_loop);
    drop(data);

    // 退出清理:Local 的 unix-dump socket + Federated 的会合 socket(最后 feeder 断 / 信号退时
    // 删文件,下次开共享的窗口 connect 失败 → 干净重拉 kernel)。
    if let Some(path) = local_socket {
        remove_socket_quiet(&path);
    }
    if let Some(path) = rendezvous_socket {
        remove_socket_quiet(&path);
    }

    run_result
}

/// 一个 **feeder**(ADR-0019):一个接入本 kernel 的 quill 窗口 = 一个 workspace。含父↔子(窗口↔
/// kernel)链的双向状态:**读端**(feed 帧:PtyOutput / FocusChange / Dims / TabList)由 calloop
/// Generic READ 源 own(见 [`make_feeder`]),源 remove / PostAction::Remove 时关其 fd,本结构不再
/// 单独持读端;**写端** [`Feeder::back`](back-channel:Input / TabOp → 窗口)+ 增量解码器 + 出站
/// 队列(绝不丢半帧,见 [`feeder_send_frame`] / [`feeder_back_writable`])+ 该 workspace 的元数据。
///
/// **两拓扑共用**:Fed(继承 pipe,读写两个独立 fd)与 Federated(会合 socket,读写同一 socket 的
/// 两个 dup)都经 [`make_feeder`] 建;差异仅"两 fd 是否同源",dup 逻辑吸收在内。
struct Feeder {
    /// 增量解码器(READ 源回调 [`feeder_readable`] 喂字节 → 逐帧解出)。
    decoder: FeedDecoder,
    /// back-channel 写端(Input / TabOp 帧 → 窗口)。非阻塞:[`feeder_send_frame`] 直写不完的
    /// 剩余字节入 [`Feeder::outbound`],可写时 [`feeder_back_writable`] drain(绝不丢半帧)。
    back: OwnedFd,
    /// 待写出的字节队列(kernel→窗口成帧流)。WouldBlock 写不完的尾部入此,保序;积压超
    /// [`FED_BACK_OUT_CAP`] = 窗口长期不读 → 断此 feeder(移除其 workspace)。**绝不丢半帧**。
    outbound: VecDeque<u8>,
    /// back-channel WRITE 源(`armed` = 当前是否 enable;有积压 arm、drain 空 disarm,不忙等)。
    write: Option<WriteReg>,
    /// 本 feeder READ 源(feed 帧入)的 token。feeder 移除时按此 remove(非自身回调路径)。
    read_token: RegistrationToken,
    /// 本 feeder 声明的 workspace id(帧头 `ws_id`,窗口每进程唯一;0 = 尚未声明过任何帧)。
    /// rings / client viewed / 广播都以此为该 workspace 身份;back-channel 路由据此定位 feeder。
    ws_id: u64,
    /// 该 workspace 的桌面焦点 tab id(FocusChange / TabList active 帧填)。
    focus_tab: u64,
    /// 该 workspace 的 tab 列表(TabList 帧整份重建)。构 [`WorkspaceInfo`] 广播给手机 tab 栏。
    fed_tabs: Vec<TabMeta>,
    /// 桌面焦点 tab 在 [`Feeder::fed_tabs`] 里的下标(WorkspaceInfo.active,标"哪个是桌面焦点")。
    fed_active: usize,
    /// 该 workspace 的桌面 PTY 尺寸(Dims 帧;`None` = 未收到,client 用默认渲染)。
    dims: Option<(u16, u16)>,
    /// 待自动 Select 的客户端 id:某客户端发 TabOp::New → 记其 id(New 路由到锚 feeder,记在锚),
    /// 锚随后发回含新 tab 的 TabList → [`feeder_tab_list_updated`] 把该客户端 viewed 自动 pin 到
    /// 新 tab(发起 New 的手机看到新 tab)。单槽(daily-drive 单手机够用)。
    pending_new_select: Option<u64>,
    /// 本 feeder 接入(accept / 构造)时刻。锚选举用(锚断后取最早接入的存活 feeder 当新锚)。
    created_at: Instant,
}

/// 建一个 [`Feeder`],**事务化**(契约):
/// - **成功** = 接管所有传入 fd(`read_fd` / `write_fd`,经 `OwnedFd` 或已注册的 calloop 源 own,
///   由本 kernel 负责最终 close);
/// - **失败** = **已自行 close 其接管的一切**(含为全双工 dup 出的写端)**并移除已注册的 calloop 源**
///   → 调用方(见 [`attach_feeder`] / Fed 拓扑)**不得再 close 传入 fd**。
///
/// 这条契约治的正是联邦 **fd 双关 bug**:旧实现里 READ 源注册成功(已 move-own read_owned)后,若
/// back-channel 注册 / disable 失败即 `?` 早退 —— `read_token` 被 drop 但 calloop 源**不随之移除**(源
/// 仍注册着、own 着 read_fd),而 [`attach_feeder`] 的 Err 臂又 `libc::close(raw)` → **双关 + epoll 悬挂
/// 已关 fd**;另外早期 `set_fd_nonblocking(read_fd)` 失败时,已 dup 出的写端无人接管 → 泄漏。
///
/// 实现顺序保证契约:先把 read_fd /(dup 后的)write_fd **立即**包成 `OwnedFd`(RAII),此后任何早退都
/// 由 `OwnedFd::drop` 关闭它们(灭 dup 泄漏);READ 源一旦注册成功,后续任一步失败前必
/// `loop_handle.remove(read_token)` 回滚(移除源 → drop read_owned → 关 read_fd,灭双关 + 悬挂)。
/// `read_fd == write_fd`(会合 socket 全双工)时 dup 写端,避免两个 `OwnedFd` 双关同一 fd。
/// `feeder_id` 捕获进两源闭包,派发时定位到本 feeder。
fn make_feeder(
    loop_handle: &LoopHandle<'static, DaemonData>,
    feeder_id: u64,
    read_fd: RawFd,
    write_fd: RawFd,
) -> Result<Feeder> {
    // ① 立即接管 read_fd(RAII):此后任何早退都由 read_owned::drop 关它,零泄漏。
    // SAFETY: read_fd 由调用方独占传入(Fed:父继承;Federated:accept 得到);from_raw_fd 接管其 close。
    #[allow(unsafe_code)]
    let read_owned = unsafe { OwnedFd::from_raw_fd(read_fd) };
    // ② 读写同 fd(会合 socket 全双工)→ dup 写端并【立即】接管(防两 OwnedFd 双关 + 防 dup 后早退泄漏)。
    let write_owned = if write_fd == read_fd {
        // SAFETY: dup 复制 fd,失败返 -1;成功得独立 fd,紧接 from_raw_fd 接管其 close。
        #[allow(unsafe_code)]
        let d = unsafe { libc::dup(write_fd) };
        if d < 0 {
            // read_owned::drop 关 read_fd(契约:失败已清理接管的一切)。
            return Err(
                anyhow::Error::new(io::Error::last_os_error()).context("dup feeder write fd 失败")
            );
        }
        // SAFETY: d 是刚 dup 出的独立 fd,本函数独占;from_raw_fd 接管其 close。
        #[allow(unsafe_code)]
        unsafe {
            OwnedFd::from_raw_fd(d)
        }
    } else {
        // SAFETY: write_fd 由调用方独占传入;from_raw_fd 接管其 close。
        #[allow(unsafe_code)]
        unsafe {
            OwnedFd::from_raw_fd(write_fd)
        }
    };
    // 此后 read_owned / write_owned 的 drop 保证两 fd 在任何早退被关(含下面 set_fd_nonblocking 失败)。
    set_fd_nonblocking(read_owned.as_raw_fd()).context("feeder read fd 设非阻塞失败")?;
    set_fd_nonblocking(write_owned.as_raw_fd()).context("feeder write fd 设非阻塞失败")?;

    // ③ READ 源(feed 帧入)。Generic own read_owned;闭包捕获 feeder_id + raw fd(int Copy)。
    let read_raw = read_owned.as_raw_fd();
    let read_token = loop_handle
        .insert_source(
            Generic::new(read_owned, Interest::READ, Mode::Level),
            move |_readiness, _meta, data: &mut DaemonData| {
                feeder_readable(data, feeder_id, read_raw)
            },
        )
        .map_err(|e| anyhow!("calloop insert_source(feeder read) 失败: {e}"))?;
    // ★ 自此 read_owned 已被 READ 源 own:任何失败必 `loop_handle.remove(read_token)` 回滚(见契约)。

    // 测试注入点(仅 debug 构建,release 编译不进 → 生产零足迹):模拟"READ 源注册成功、后段失败",
    // 走回滚路径,供 tests/kernel_federation_slice.rs 验无双关 / 无 fd 泄漏。
    #[cfg(debug_assertions)]
    if std::env::var_os("QUILL_TEST_FEEDER_FAIL_AFTER_READ").is_some() {
        loop_handle.remove(read_token); // 回滚 READ 源 → drop read_owned → 关 read_fd。write_owned 由下方 drop 关。
        return Err(anyhow!(
            "QUILL_TEST_FEEDER_FAIL_AFTER_READ:注入 make_feeder 后段失败(仅 debug)"
        ));
    }

    // ④ back-channel WRITE 源(初始 disable —— 无待写不忙等;有积压 arm、drain 空 disarm)。borrow
    // back_owned 的 raw(它进 Feeder.back 由 DaemonData own,run() scope 全程活;真写走 Feeder.back)。
    let back_raw = write_owned.as_raw_fd();
    // SAFETY: back_raw 借自下方 move 进 data.feeders 的 write_owned(run() scope 全程活);borrow_raw
    // 只取 int 不转移所有权;真正 write 走 Feeder.back。feeder 移除时先 remove 本 WRITE 源(fd 仍开
    // → 干净 EPOLL_CTL_DEL)再 drop Feeder 关 back;即便错序 calloop 对已关 fd 的 DEL 返 EBADF 容忍。
    #[allow(unsafe_code)]
    let back_borrowed: BorrowedFd<'static> = unsafe { BorrowedFd::borrow_raw(back_raw) };
    let write_token = match loop_handle.insert_source(
        Generic::new(back_borrowed, Interest::WRITE, Mode::Level),
        move |_readiness, _meta, data: &mut DaemonData| feeder_back_writable(data, feeder_id),
    ) {
        Ok(t) => t,
        Err(e) => {
            // 回滚 READ 源(关 read_fd);write_owned 由本函数尾 drop 关写端(WRITE 源未注册,无需拆)。
            loop_handle.remove(read_token);
            return Err(anyhow!(
                "calloop insert_source(feeder back-channel write) 失败: {e}"
            ));
        }
    };
    if let Err(e) = loop_handle.disable(&write_token) {
        // 回滚:WRITE 源(借 write_owned 的 fd,不关、仅 EPOLL_CTL_DEL)+ READ 源(关 read_fd);
        // write_owned 由本函数尾 drop 关写端。
        loop_handle.remove(write_token);
        loop_handle.remove(read_token);
        return Err(anyhow!("disable feeder back-channel WRITE 源失败: {e}"));
    }

    Ok(Feeder {
        decoder: FeedDecoder::new(),
        back: write_owned,
        outbound: VecDeque::new(),
        write: Some(WriteReg {
            token: write_token,
            armed: false,
        }),
        read_token,
        ws_id: 0,
        focus_tab: 0,
        fed_tabs: Vec::new(),
        fed_active: 0,
        dims: None,
        pending_new_select: None,
        created_at: Instant::now(),
    })
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
    /// **Local 拓扑**(standalone):own 多工作区 [`Session`](shell tab + 真 PTY)。**Fed /
    /// Federated 拓扑** = `None`:kernel 不 own term/PTY(窗口 own),控制面元数据/焦点由窗口经
    /// feeder 喂的帧维护(见 [`DaemonData::feeders`])。**为何不在联邦也用 Session**:`Session`
    /// 焊死在 `TabList<TabInstance>`,而 `TabInstance` 必持 `PtyHandle`(无 PTY 构造不出);故联邦
    /// 走轻量 [`Feeder`],复用 ring + fan_out + WS 子系统。
    session: Option<Session>,
    /// **Fed / Federated 拓扑** 的 feeder 表(ADR-0019:一个接入窗口 = 一个 feeder = 一个 workspace),
    /// key = kernel 分配的自增 feeder id。Local 恒空(用 [`DaemonData::session`])。Fed = 1 个;
    /// Federated = accept 的 N 个。
    feeders: HashMap<u64, Feeder>,
    /// 锚 feeder id(ADR-0019 F3:第一个接入 = 锚 = 新 tab 落点)。锚断后取最早接入的存活 feeder。
    /// `None` = 无 feeder(Local,或 Federated 尚无 feeder / 全断)。
    anchor_feeder: Option<u64>,
    /// feeder id 自增源。
    next_feeder_id: u64,
    /// 是否 Federated 拓扑(会合 accept N feeder)。区别于 Fed(单继承 pipe):仅用于"最后一个
    /// feeder 断 → 退"与 startup grace 的 gating(Local 恒不退;Fed / Federated 皆"无 feeder → 退")。
    federated: bool,
    /// 运行期注册 / 注销源(WS 连接的 READ/WRITE 源、unix 客户端写源、feeder 源)用。
    loop_handle: LoopHandle<'static, DaemonData>,
    loop_signal: LoopSignal,
    /// **Local 专用**:当前焦点 tab 的 raw id(字节流 [`ServerMsg::StreamFocus`] 标的那个)。
    /// 联邦拓扑焦点是 per-feeder(见 [`Feeder::focus_tab`]),本字段不用。
    tab_id: u64,
    /// **Local 专用**:本 daemon 的 active(且唯一)工作区 id。anchor 工作区 —— WS 连上即 hold 它。
    /// 联邦拓扑 workspace 是 per-feeder(见 [`Feeder::ws_id`]),本字段不用。
    workspace_id: u64,
    /// **Local 专用**:桌面 PTY 当前尺寸 `(cols, rows)`(A′ 增量1)。联邦拓扑尺寸是 per-feeder
    /// (见 [`Feeder::dims`])。Local = 启动尺寸,[`ClientMsg::Resize`] 改 PTY 时同步更新。
    dims: Option<(u16, u16)>,
    /// **每 tab** 一个 PTY 原始字节环缓冲(砖2 B3),key = `(ws_id, tab_id)`。连上 / Select 时重放
    /// 目标 tab 的环重建当前屏 + 之后 live。单线程,直接 fan-out 进各连接出站队列。tab 关闭时其环由
    /// [`feeder_tab_list_updated`] 清掉。**Local 拓扑**单 active tab = 单条目(退化,行为等价单 ring)。
    rings: HashMap<(u64, u64), ByteRing>,
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
    /// WS 握手鉴权 token(P0 CSWSH 修复)。每条连接握手时克隆进 [`AuthCallback`] 与请求 `?t=`
    /// 常量时间比较。来源见 [`load_or_generate_token`]。
    ws_token: Rc<str>,
    /// Host/Origin 允许集(纵深防线;见 [`build_allowed_hosts`] / [`host_is_allowed`])。
    allowed_hosts: Rc<HashSet<String>>,
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
    /// 本客户端当前【看】的 (ws_id, tab_id)(砖2 B3:每客户端独立视图)。连上默认 = 锚 workspace
    /// 焦点 tab;收 `TabOp::Select` 切到指定 tab。只有 viewed 该 tab 的字节流才发给本客户端(省网)。
    /// `viewed.0` = 所看 workspace 的 ws_id(= 所属 feeder 的 [`Feeder::ws_id`],联邦拓扑)。
    viewed: (u64, u64),
    /// **联邦拓扑**:本客户端当前 attach 的 feeder id(= 所看 workspace)。焦点/TabList 变更 & 断开
    /// 重定向据此定位(而非拿可能滞后的 `viewed.0` 比 ws_id —— feeder 声明 ws 前二者不一致)。
    /// Local 拓扑恒 `None`(单 Session,不走 feeder 路径)。
    feeder_id: Option<u64>,
    /// 是否"跟随桌面焦点"(砖2 B3)。连上默认 `true`(桌面焦点变则本客户端 viewed 自动跟到新焦点
    /// tab —— 覆盖"连上早于首个 FocusChange"的 race + 手机初见即镜像桌面);收 `TabOp::Select`
    /// 后置 `false`(pin 到用户选的 tab,桌面焦点变不再动它 = "绝不动/被动"独立)。
    follow_focus: bool,
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
    /// 回调 [`AuthCallback`] 在握手回复前校验 token(query `?t=`)+ Origin + Host(P0 CSWSH 修复)。
    Handshaking(Option<MidHandshake<ServerHandshake<PrefixStream, AuthCallback>>>),
    /// 握手完成,长命直播:`WebSocket` 持有 [`PrefixStream`](握手后前缀已耗尽 ≈ 裸 dup)。
    Live(WebSocket<PrefixStream>),
}

/// WS 握手鉴权回调(P0 CSWSH 修复,ADR-0016 "以后可加 token" 提前落地)。在 tungstenite 生成
/// 101 回复**之前**被调用,是唯一能拒绝跨站/未授权 WS 升级的钩子(旧码 `NoCallback` = 全放行 =
/// 教科书级 CSWSH:任意恶意网页 `new WebSocket('ws://<主机>:7878')` 即可读全部 PTY 字节 + 注入
/// 命令,浏览器对 WS 不施同源策略,VPN/LAN 边界被受害者自己的浏览器穿透)。
///
/// **三道校验(全过才放行)**:
/// 1. **token**(主防线):请求目标 query `?t=<token>` 与 kernel token **常量时间**比较(见
///    [`constant_time_eq`])。恶意网页拿不到 token(它不在页面里,只在用户手动构造的分享 URL
///    query 里)→ 连不上 = CSWSH 被堵死。缺失/不匹配 → 403。
/// 2. **Origin**(纵深):浏览器对 WS **会**带 Origin(本机 serve 的页面 origin);跨源(如
///    `http://evil.com`)拒。native/无浏览器客户端无 Origin → 放行(见 [`host_is_allowed`])。
/// 3. **Host**(纵深,防 DNS rebinding):Host 头主机须是 IP 字面量 / localhost / 允许集域名;
///    攻击者把域名 rebind 到本机 IP 时 Host = 攻击者域名(非 IP、不在允许集)→ 拒。
///
/// 字段用 `Rc`(单线程 daemon,每连接握手时从 [`DaemonData`] 克隆 Rc,零深拷贝)。
struct AuthCallback {
    /// kernel 的共享 token(与请求 `?t=` 常量时间比较)。
    token: Rc<str>,
    /// Host/Origin 允许集(额外域名;IP 字面量 + localhost 恒放行,见 [`host_is_allowed`])。
    allowed_hosts: Rc<HashSet<String>>,
}

impl Callback for AuthCallback {
    fn on_request(self, request: &Request, response: Response) -> Result<Response, ErrorResponse> {
        // 1) token(主防线)。请求目标是 origin-form(`/?t=...`);从 query 取 `t`。
        let supplied = request.uri().query().and_then(|q| query_param(q, "t"));
        let ok_token = supplied
            .map(|t| constant_time_eq(t.as_bytes(), self.token.as_bytes()))
            .unwrap_or(false);
        if !ok_token {
            tracing::warn!("WS 握手拒绝:token 缺失/不匹配(CSWSH 防线)");
            return Err(reject_response("missing or invalid token"));
        }
        // 2) Origin(纵深)。存在则其主机必须在允许集;不存在 = native/无浏览器客户端 → 放行。
        if let Some(origin) = request
            .headers()
            .get("origin")
            .and_then(|v| v.to_str().ok())
        {
            let host = origin_host(origin).unwrap_or("");
            if !host_is_allowed(host, &self.allowed_hosts) {
                tracing::warn!(%origin, "WS 握手拒绝:跨源 Origin(CSWSH 纵深)");
                return Err(reject_response("cross-origin websocket rejected"));
            }
        }
        // 3) Host(纵深,防 DNS rebinding)。Host 头存在则其主机必须在允许集。
        if let Some(host_hdr) = request.headers().get("host").and_then(|v| v.to_str().ok()) {
            let host = host_only(host_hdr);
            if !host_is_allowed(host, &self.allowed_hosts) {
                tracing::warn!(%host_hdr, "WS 握手拒绝:Host 不在允许集(防 DNS rebinding)");
                return Err(reject_response("host not allowed"));
            }
        }
        Ok(response)
    }
}

/// 从 URL query 串(`a=1&t=xxx&b=2`)取指定 key 的值。token 值域是 hex / base64url(无需
/// percent-decode)。
fn query_param<'a>(query: &'a str, key: &str) -> Option<&'a str> {
    query.split('&').find_map(|kv| {
        let (k, v) = kv.split_once('=')?;
        (k == key).then_some(v)
    })
}

/// 常量时间字节比较(避免 `==` 早退泄漏 token 前缀匹配长度这一计时侧信道)。长度不等直接
/// false(token 长度是公开定长,不算秘密)。
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// 从 authority(`host` / `host:port` / `[::1]` / `[::1]:port`)抠出裸主机(去端口、去 v6 方括号)。
fn host_only(authority: &str) -> &str {
    let a = authority.trim();
    if let Some(rest) = a.strip_prefix('[') {
        // v6 字面量:`[addr]` 或 `[addr]:port`。
        return rest.split(']').next().unwrap_or(rest);
    }
    // host / host:port(v4 / 域名;Host/Origin 里裸 v6 恒带方括号,故按首个 ':' 切端口安全)。
    a.split(':').next().unwrap_or(a)
}

/// 从 Origin 头(`scheme://host[:port]`)抠出裸主机。
fn origin_host(origin: &str) -> Option<&str> {
    let authority = origin.split_once("://")?.1;
    // origin 无 path,但防御性截断到首个 '/'。
    let authority = authority.split('/').next().unwrap_or(authority);
    Some(host_only(authority))
}

/// 主机是否允许(Host/Origin 纵深防线,防 DNS rebinding)。放行:① 任意 **IP 字面量**(v4/v6)
/// ——DNS rebinding 本质需要**域名**,直连 IP 既非 rebinding 又仍要过 token,故放行任意 IP 干净覆盖
/// 手机走 LAN IP / VPN IP 的合法访问;② `localhost`;③ 允许集里的额外域名(`QUILL_SHARE_ALLOWED_HOSTS`,
/// 放行用户自有 DDNS 域)。拒:`evil.com` 这种非 IP、不在允许集的域名(rebinding 到本机 IP 时 Host
/// 仍是攻击者域名)。**大小写不敏感**。以 token 为主防线,本函数宁宽勿误杀合法 IP/域名访问。
fn host_is_allowed(host: &str, allowed: &HashSet<String>) -> bool {
    let h = host.trim().to_ascii_lowercase();
    if h.is_empty() {
        return false;
    }
    if h == "localhost" {
        return true;
    }
    if h.parse::<IpAddr>().is_ok() {
        return true;
    }
    allowed.contains(&h)
}

/// 构造握手拒绝响应(403 + 短说明 body)。避免 `unwrap`:`Response::new` 建默认 200,再覆写状态。
fn reject_response(msg: &str) -> ErrorResponse {
    let mut resp = ErrorResponse::new(Some(msg.to_string()));
    *resp.status_mut() = tungstenite::http::StatusCode::FORBIDDEN;
    resp
}

/// 生成一个随机共享 token(32 字节 CSPRNG → 64 hex 字符)。P0 CSWSH 修复:kernel 缺 env token 时
/// 的临时 token、window owner 首次创建持久 token 都走它(见 window.rs)。走 `getrandom(2)` syscall;
/// 极罕见的失败(pool 未就绪等)**fail-closed** 返 `Err`(宁可开共享失败也绝不产弱/空 token)。
pub fn generate_ws_token() -> Result<String> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).map_err(|e| anyhow!("getrandom 生成 share token 失败: {e}"))?;
    let mut s = String::with_capacity(64);
    for b in bytes {
        use std::fmt::Write as _;
        // 写入 String 的 fmt 不会失败;忽略返回值(无 unwrap)。
        let _ = write!(s, "{b:02x}");
    }
    Ok(s)
}

/// kernel 启动时决定 WS 鉴权 token:优先 env `QUILL_SHARE_TOKEN`(owner 窗口下发的持久 token,
/// 手机存的 URL 重启不失效);缺失 → 生成临时 token + `warn`(WS 仍强制鉴权,不裸奔)。
fn load_or_generate_token() -> Result<Rc<str>> {
    if let Ok(t) = std::env::var(ENV_SHARE_TOKEN) {
        if !t.is_empty() {
            return Ok(Rc::from(t.as_str()));
        }
    }
    let t = generate_ws_token()?;
    tracing::warn!(
        "{ENV_SHARE_TOKEN} 未设置:已生成临时 token(重启后失效;WS 仍强制鉴权,不裸奔放行)。\
         经 quill 窗口开共享会自动下发持久 token。"
    );
    Ok(Rc::from(t.as_str()))
}

/// 构建 Host/Origin 允许集:localhost + IP 字面量恒由 [`host_is_allowed`] 放行(不入集);此集只
/// 装**额外域名** —— `QUILL_SHARE_WS_BIND` 的 host(若是域名)+ env `QUILL_SHARE_ALLOWED_HOSTS`
/// 逗号分隔项。全部小写归一化。
fn build_allowed_hosts(ws_bind: SocketAddr) -> HashSet<String> {
    let mut set = HashSet::new();
    // bind host 若是域名(极少;通常是 0.0.0.0 / IP)也放行。IP 会被 host_is_allowed 直接放行,入不入集无所谓。
    set.insert(ws_bind.ip().to_string().to_ascii_lowercase());
    if let Ok(extra) = std::env::var(ENV_ALLOWED_HOSTS) {
        for h in extra.split(',') {
            let h = h.trim().to_ascii_lowercase();
            if !h.is_empty() {
                set.insert(h);
            }
        }
    }
    set
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
    // Local 单 active tab:所有字节归 (workspace_id, tab_id) 这一条环 / 一组 viewed 该 tab 的客户端。
    let ws_id = data.workspace_id;
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
                    fan_out_tab_bytes(data, ws_id, tab_id, batch);
                }
                return Ok(PostAction::Continue);
            }
            PtyRead::Closed => {
                tracing::info!(tab_id, "PTY EOF/EIO:子 shell 退出,停止 daemon");
                if !batch.is_empty() {
                    fan_out_tab_bytes(data, ws_id, tab_id, batch);
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
                    fan_out_tab_bytes(data, ws_id, tab_id, batch);
                }
                data.loop_signal.stop();
                return Ok(PostAction::Remove);
            }
        }
    }
}

/// 把一批 **某 (ws, tab) 的** PTY 字节 append 进该 tab 的环缓冲,并 fan-out 进【viewed 该 tab】的
/// 每条 live WS 连接的出站队列(打开其 WRITE 兴趣)。某连接积压超 [`WS_CLIENT_OUT_CAP`] → 断开它
/// (重连重放恢复)。砖2 B3:每 tab 独立环 + 每客户端只收自己 viewed 的 tab(手机各看各的、省网)。
///
/// **viewed 该 tab 的** live 连接才入队;viewed 别的 tab / Peeking / Handshaking 的连接不入队 ——
/// Peeking/Handshaking 转 Live 时会先收目标 tab 环缓冲重放(此刻 append 的已在环里,单线程串行
/// 保证「ring.snapshot + 订阅」相对本函数原子,不丢不双发)。
fn fan_out_tab_bytes(data: &mut DaemonData, ws: u64, tab: u64, batch: Vec<u8>) {
    data.rings
        .entry((ws, tab))
        .or_insert_with(|| ByteRing::new(BYTE_RING_CAP))
        .push(&batch);
    if data.clients.is_empty() {
        return;
    }
    // move 进 Bytes(refcount);各订阅连接 clone 只 +1 引用,不拷贝字节。
    let frame = Bytes::from(batch);
    let ids: Vec<u64> = data.clients.keys().copied().collect();
    for id in ids {
        let mut is_target = false;
        let mut over_cap = false;
        if let Some(c) = data.clients.get_mut(&id) {
            if matches!(c.stage, WsStage::Live(_)) && c.viewed == (ws, tab) {
                is_target = true;
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
        } else if is_target {
            arm_write(data, id);
        }
    }
}

/// 会合 UnixListener 可读(Federated 拓扑):accept 到 WouldBlock,每个新连接接入成一个 feeder
/// (= 一个 workspace)。第一个接入 = 锚(F3)。accept 失败仅 warn(不拖垮 loop)。
fn feeder_accept_ready(listener: &UnixListener, data: &mut DaemonData) -> io::Result<PostAction> {
    loop {
        match listener.accept() {
            Ok((stream, _addr)) => {
                if let Err(e) = attach_feeder(data, stream) {
                    tracing::warn!(?e, "会合新 feeder 接入失败");
                }
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(PostAction::Continue),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => {
                tracing::warn!(?e, "会合 UnixListener accept 失败");
                return Ok(PostAction::Continue);
            }
        }
    }
}

/// 把一条 accept 的会合 socket 接入成 feeder(全双工:同一 socket 的 fd 用作读 = feed 帧、写 =
/// back-channel;[`make_feeder`] 内 dup 成两个独立 OwnedFd)。首个 feeder = 锚;把此前无 feeder 可
/// attach 的悬空客户端(`feeder_id == None`)接到新 feeder(其 viewed 待 feeder 声明 ws 后由
/// focus/tablist 重定向补上)。广播更新后的工作区列表。
fn attach_feeder(data: &mut DaemonData, stream: UnixStream) -> Result<()> {
    let raw = stream.into_raw_fd(); // 转 raw:make_feeder 接管(dup + OwnedFd),同 fd 读写全双工。
    let id = data.next_feeder_id;
    // make_feeder 契约(见其 doc):成功=接管 raw;失败=已自行 close raw(+ 移除已注册源)。故此处
    // **不再** `libc::close(raw)` —— 旧代码在失败臂关 raw,而 make_feeder 后段失败时 READ 源已 own raw
    // 未拆除 → 双关 + epoll 悬挂已关 fd(联邦 #3 顺带修的 fd 双关 bug)。现全交 make_feeder 事务化处理。
    let feeder = make_feeder(&data.loop_handle, id, raw, raw)?;
    data.next_feeder_id = data.next_feeder_id.wrapping_add(1);
    data.feeders.insert(id, feeder);
    if data.anchor_feeder.is_none() {
        data.anchor_feeder = Some(id);
    }
    // 悬空客户端(连在 feeder 之前)attach 到本 feeder(其 viewed 由 feeder 声明 ws 后补齐)。
    let dangling: Vec<u64> = data
        .clients
        .iter()
        .filter(|(_, c)| c.feeder_id.is_none() && matches!(c.stage, WsStage::Live(_)))
        .map(|(cid, _)| *cid)
        .collect();
    for cid in dangling {
        if let Some(c) = data.clients.get_mut(&cid) {
            c.feeder_id = Some(id);
            c.follow_focus = true;
        }
    }
    tracing::info!(feeder = id, anchor = ?data.anchor_feeder, "会合 feeder 接入 (= 一个 workspace)");
    broadcast_workspaces_list(data);
    Ok(())
}

/// feeder READ 源可读(镜像 [`pty_readable`] 的 drain 语义):drain 到 WouldBlock,喂本 feeder 的
/// [`FeedDecoder`] 解帧,逐帧 [`route_feeder_frame`]。feeder pipe/socket **EOF** = 窗口退出 → 移除
/// 该 feeder(= 移除其 workspace;若最后一个 → kernel 退,ADR-0019);**流错位**(坏帧)= 致命同样
/// 移除。**只移除本 feeder,不停整个 kernel**(除非它是最后一个)。
///
/// **drain-then-route**:先 drain + 解出所有完整帧(其间只借本 feeder),再逐帧路由(`route_feeder_frame`
/// 自由借 `data`)。EOF/错误走 [`feeder_teardown`](本 READ 源自身 → 返 `Remove`,不在回调内 remove 自身)。
fn feeder_readable(data: &mut DaemonData, feeder_id: u64, fd: RawFd) -> io::Result<PostAction> {
    let mut frames: Vec<FeedFrame> = Vec::new();
    let mut buf = [0u8; PTY_READ_BUF];
    let mut down = false;
    loop {
        // SAFETY: fd 是本 feeder READ 源 own 的 OwnedFd 的 raw(注册期间活着);libc::read 只调
        // syscall 不动 fd 所有权,buf 是栈数组。
        #[allow(unsafe_code)]
        let n = unsafe { libc::read(fd, buf.as_mut_ptr().cast::<libc::c_void>(), buf.len()) };
        if n > 0 {
            let Some(feeder) = data.feeders.get_mut(&feeder_id) else {
                return Ok(PostAction::Remove); // feeder 已移除(race)
            };
            feeder.decoder.push(&buf[..n as usize]);
            loop {
                match feeder.decoder.next_frame() {
                    Ok(Some(f)) => frames.push(f),
                    Ok(None) => break,
                    Err(e) => {
                        tracing::error!(feeder = feeder_id, %e, "feeder 喂料流解码错位 (致命),移除该 feeder");
                        down = true;
                        break;
                    }
                }
            }
            if down {
                break;
            }
            continue;
        }
        if n == 0 {
            tracing::info!(feeder = feeder_id, "feeder EOF:窗口退出,移除该 feeder");
            down = true;
            break;
        }
        // n < 0
        let err = io::Error::last_os_error();
        match err.kind() {
            io::ErrorKind::WouldBlock => break,     // 本轮排空
            io::ErrorKind::Interrupted => continue, // EINTR 重试
            _ => {
                tracing::error!(feeder = feeder_id, ?err, "feeder read 错误,移除该 feeder");
                down = true;
                break;
            }
        }
    }

    for f in frames {
        route_feeder_frame(data, feeder_id, f);
    }

    if down {
        // 本回调 = 该 feeder 的 READ 源自身 → 返 Remove 让 calloop 注销它(勿在回调内 remove 自身);
        // teardown 只移 WRITE 源 + rings + 重定向客户端 + 选新锚 + 最后一个→退。
        feeder_teardown(data, feeder_id, false, true);
        return Ok(PostAction::Remove);
    }
    Ok(PostAction::Continue)
}

/// 路由一条 feeder 喂来的帧(联邦拓扑;记该 feeder 声明的 `ws_id`):
/// - `PtyOutput` → 该 (ws, tab) 的字节,经 [`fan_out_tab_bytes`] 进该 tab 环 + viewed 该 tab 的客户端。
/// - `FocusChange` → 更新该 feeder 桌面焦点 + [`feeder_focus_changed`](重定向 attach 本 feeder 且跟随
///   焦点的客户端 + 刷 active 标记)。
/// - `Dims` → 记该 feeder 桌面尺寸 + 广播给 attach 本 feeder 的客户端。
/// - `TabList` → 重建该 feeder tab 元数据 + 广播 + 处理 viewed 被关 tab 的客户端 + New 自动 Select。
/// - `WorkspaceAdd/Remove` → 单工作区/feeder 用 TabList 代之(留接口)。
/// - `Input` / `TabOp` → 窗口→kernel 方向,不该反向收到,忽略。
fn route_feeder_frame(data: &mut DaemonData, feeder_id: u64, frame: FeedFrame) {
    match frame.kind {
        FrameKind::PtyOutput => {
            if let Some(f) = data.feeders.get_mut(&feeder_id) {
                f.ws_id = frame.ws_id; // 每帧携带窗口进程唯一 ws;首帧即声明。
            }
            if !frame.payload.is_empty() {
                fan_out_tab_bytes(data, frame.ws_id, frame.tab_id, frame.payload);
            }
        }
        FrameKind::FocusChange => {
            feeder_focus_changed(data, feeder_id, frame.ws_id, frame.tab_id);
        }
        FrameKind::Dims => match decode_dims(&frame.payload) {
            Some((cols, rows)) => {
                if let Some(f) = data.feeders.get_mut(&feeder_id) {
                    f.ws_id = frame.ws_id;
                    f.dims = Some((cols, rows));
                }
                broadcast_dims_for_feeder(data, feeder_id);
            }
            None => tracing::warn!(
                len = frame.payload.len(),
                "feeder Dims 帧 payload 长度非法,忽略"
            ),
        },
        FrameKind::TabList => match decode_tab_list(&frame.payload) {
            Some((active, tabs)) => {
                feeder_tab_list_updated(data, feeder_id, frame.ws_id, active, tabs)
            }
            None => tracing::warn!(
                len = frame.payload.len(),
                "feeder TabList 帧 payload 非法,忽略"
            ),
        },
        FrameKind::WorkspaceAdd | FrameKind::WorkspaceRemove => {
            tracing::debug!(
                ws_id = frame.ws_id,
                "feeder WorkspaceAdd/Remove 帧 (用 TabList 代之)"
            );
        }
        FrameKind::Input | FrameKind::TabOp => {
            tracing::debug!(kind = ?frame.kind, "feeder 收到 kernel→窗口方向帧 (不该反向收到),忽略");
        }
    }
}

/// 移除一个 feeder(= 移除其 workspace,ADR-0019 F2b refcount 生命周期):
/// - 按 `remove_read` / `remove_write` 决定是否 `loop_handle.remove` 各源(**自身回调路径须传 false**
///   由调用方返 `PostAction::Remove` 注销自身,calloop 禁回调内 remove 自身源);
/// - 从 `feeders` 移除(drop [`Feeder`] 关 back OwnedFd)+ 清该 workspace 的 rings;
/// - 锚若是它 → 选最早接入的存活 feeder 当新锚;attach 本 feeder 的客户端 → 重定向到新锚(或悬空);
/// - 广播更新后的工作区列表;
/// - **Federated / Fed 下 feeders 归零 → kernel 退**(清会合 socket + 释 7878,ADR-0019)。
fn feeder_teardown(data: &mut DaemonData, feeder_id: u64, remove_read: bool, remove_write: bool) {
    let Some(feeder) = data.feeders.remove(&feeder_id) else {
        return;
    };
    let ws = feeder.ws_id;
    if remove_read {
        data.loop_handle.remove(feeder.read_token);
    }
    if remove_write {
        if let Some(w) = &feeder.write {
            data.loop_handle.remove(w.token);
        }
    }
    drop(feeder); // 关 back OwnedFd(READ 源 own 的读 fd 由源注销/PostAction::Remove 关)。
    data.rings.retain(|(rws, _), _| *rws != ws);

    // 锚选举:锚断 → 取最早接入(min created_at,平手取 min id)的存活 feeder。
    if data.anchor_feeder == Some(feeder_id) {
        data.anchor_feeder = data
            .feeders
            .iter()
            .min_by_key(|(fid, f)| (f.created_at, **fid))
            .map(|(fid, _)| *fid);
    }
    // attach 本(已移除)feeder 的客户端 → 重定向到新锚(在看的 workspace 没了);无锚则悬空。
    let new_anchor = data.anchor_feeder;
    let orphans: Vec<u64> = data
        .clients
        .iter()
        .filter(|(_, c)| c.feeder_id == Some(feeder_id))
        .map(|(cid, _)| *cid)
        .collect();
    for cid in orphans {
        if let Some(c) = data.clients.get_mut(&cid) {
            c.feeder_id = new_anchor;
            c.follow_focus = true;
        }
        if let Some(a) = new_anchor {
            let (aws, atab) = data
                .feeders
                .get(&a)
                .map(|f| (f.ws_id, f.focus_tab))
                .unwrap_or((0, 0));
            fed_point_client(data, cid, aws, atab, true);
        }
    }
    broadcast_workspaces_list(data);

    // 最后一个 feeder 断 → kernel 退(仅联邦/Fed;Local 无 feeder 恒不走此)。
    if data.session.is_none() && data.feeders.is_empty() {
        tracing::info!("最后一个 feeder 断开,kernel 退出 (清会合 socket + 释 7878)");
        data.loop_signal.stop();
    }
}

/// 向所有 live WS 客户端推一条 [`ServerMsg`] 控制帧(Text)。序列化失败仅 warn 跳过(控制面
/// 非幂等丢一条不致命,客户端有忠实字节流兜底)。Peeking/Handshaking 阶段连接不入队 —— 它们
/// 转 Live 时会先收 [`build_control_text`] 引导帧拿到当前状态。
fn broadcast_control(data: &mut DaemonData, msg: &ServerMsg) {
    let text = match serde_json::to_string(msg) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(?e, "序列化控制帧失败,跳过广播");
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

/// 把一条 live WS 客户端的 viewed 切到 `(ws, tab)`(砖2 B3):发 [`ServerMsg::StreamFocus`](让 web
/// 端 reset 终端)+ 重放该 tab 环缓冲(关键帧 = 重建当前屏)+ 之后 live 由 [`fan_out_tab_bytes`] 送。
/// `follow` = 是否"跟随桌面焦点"(见 [`WsClient::follow_focus`])。仅对 Live 连接有效;非 Live / 不
/// 存在则 no-op。**每客户端独立、不动桌面焦点、不影响别的客户端**(纯改本连接 viewed + 出站队列)。
fn fed_point_client(data: &mut DaemonData, id: u64, ws: u64, tab: u64, follow: bool) {
    let replay = data
        .rings
        .get(&(ws, tab))
        .map(|r| r.snapshot())
        .unwrap_or_default();
    let focus_json = serde_json::to_string(&ServerMsg::StreamFocus {
        workspace_id: ws,
        tab_id: tab,
    })
    .ok();
    let mut armed = false;
    if let Some(c) = data.clients.get_mut(&id) {
        if !matches!(c.stage, WsStage::Live(_)) {
            return;
        }
        c.viewed = (ws, tab);
        c.follow_focus = follow;
        if let Some(j) = focus_json {
            c.outbound_len += j.len();
            c.outbound.push_back(OutFrame::Text(j));
        }
        if !replay.is_empty() {
            c.outbound_len += replay.len();
            c.outbound.push_back(OutFrame::Bytes(Bytes::from(replay)));
        }
        armed = true;
    }
    if armed {
        arm_write(data, id);
    }
}

/// 向【某一条】live WS 客户端推一条 [`ServerMsg`] 控制帧(Text)。序列化失败仅 warn 跳过。
/// 非 Live / 不存在 = no-op。per-client 元数据(每客户端看各自 attach 的 workspace)用。
fn send_control_to_client(data: &mut DaemonData, id: u64, msg: &ServerMsg) {
    let text = match serde_json::to_string(msg) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(?e, "序列化 per-client 控制帧失败,跳过");
            return;
        }
    };
    let mut armed = false;
    if let Some(c) = data.clients.get_mut(&id) {
        if matches!(c.stage, WsStage::Live(_)) {
            c.outbound_len += text.len();
            c.outbound.push_back(OutFrame::Text(text));
            armed = true;
        }
    }
    if armed {
        arm_write(data, id);
    }
}

/// 某 feeder 桌面焦点变(其窗口发 [`FrameKind::FocusChange`])的处理(F2):更新该 feeder 焦点 tab +
/// active 标记,把 attach 本 feeder 且**跟随焦点**的客户端 viewed 重定向到新焦点 tab(pin 过的不动),
/// 再广播元数据。**每客户端独立、只动 attach 本 feeder 的跟随者**(别的 workspace / pin 的不受影响)。
fn feeder_focus_changed(data: &mut DaemonData, feeder_id: u64, ws: u64, tab: u64) {
    {
        let Some(f) = data.feeders.get_mut(&feeder_id) else {
            return;
        };
        f.ws_id = ws;
        f.focus_tab = tab;
        if let Some(i) = f.fed_tabs.iter().position(|t| t.tab_id == tab) {
            f.fed_active = i;
        }
    }
    let followers: Vec<u64> = data
        .clients
        .iter()
        .filter(|(_, c)| {
            c.feeder_id == Some(feeder_id) && c.follow_focus && matches!(c.stage, WsStage::Live(_))
        })
        .map(|(id, _)| *id)
        .collect();
    for id in followers {
        fed_point_client(data, id, ws, tab, true);
    }
    broadcast_feeder_metadata(data, feeder_id);
}

/// 某 feeder 整份 tab 列表更新(其窗口发 [`FrameKind::TabList`])的处理(F2 + B4):重建该 feeder 的
/// `fed_tabs` / `fed_active` / 焦点 tab,清掉已关 tab 的环缓冲,把 attach 本 feeder 且 viewed 于**已关
/// tab** 的客户端回落到跟随焦点,广播元数据,最后把发起 `New` 的客户端自动 Select 到新出现的 tab。
fn feeder_tab_list_updated(
    data: &mut DaemonData,
    feeder_id: u64,
    ws: u64,
    active: usize,
    tabs: Vec<(u64, String)>,
) {
    let old_ids: std::collections::HashSet<u64>;
    let new_ids: std::collections::HashSet<u64> = tabs.iter().map(|(id, _)| *id).collect();
    let focus_tab;
    {
        let Some(f) = data.feeders.get_mut(&feeder_id) else {
            return;
        };
        old_ids = f.fed_tabs.iter().map(|t| t.tab_id).collect();
        f.ws_id = ws;
        f.fed_tabs = tabs
            .into_iter()
            .map(|(id, title)| TabMeta { tab_id: id, title })
            .collect();
        f.fed_active = active.min(f.fed_tabs.len().saturating_sub(1));
        focus_tab = f
            .fed_tabs
            .get(f.fed_active)
            .map(|t| t.tab_id)
            .unwrap_or(f.focus_tab);
        f.focus_tab = focus_tab;
    }
    // 清掉本 workspace 里已不存在 tab 的环缓冲(tab 已关,内存回收)。
    data.rings
        .retain(|(rws, rtab), _| *rws != ws || new_ids.contains(rtab));
    // attach 本 feeder 且 viewed 于已关 tab 的客户端 → 回落到跟随桌面焦点(它看的 tab 没了)。
    let orphans: Vec<u64> = data
        .clients
        .iter()
        .filter(|(_, c)| {
            c.feeder_id == Some(feeder_id)
                && matches!(c.stage, WsStage::Live(_))
                && !new_ids.contains(&c.viewed.1)
        })
        .map(|(id, _)| *id)
        .collect();
    for id in orphans {
        fed_point_client(data, id, ws, focus_tab, true);
    }
    broadcast_feeder_metadata(data, feeder_id);
    // B4:发起 New 的客户端 → 新 tab 一出现就自动 Select 到它(pin)。新 tab = 本次列表里旧集合没有
    // 的 id;优先桌面焦点 tab(窗口 New 后切到新 tab)。新 tab 尚未出现则保留 pending 待下次 TabList。
    let pending = data
        .feeders
        .get_mut(&feeder_id)
        .and_then(|f| f.pending_new_select.take());
    if let Some(cid) = pending {
        let target = if !old_ids.contains(&focus_tab) {
            Some(focus_tab)
        } else {
            data.feeders.get(&feeder_id).and_then(|f| {
                f.fed_tabs
                    .iter()
                    .map(|t| t.tab_id)
                    .find(|id| !old_ids.contains(id))
            })
        };
        match target {
            Some(tab) => {
                // F4:auto-Select 也须把 feeder_id 对齐到锚 feeder(= 本次 TabList 的 `feeder_id`,其
                // `ws_id == ws`)。否则:客户端先跨窗口 Select 到非锚窗口(feeder_id=那个)、再点 + →
                // New 落锚 + 自动 Select 到锚新 tab,只 `viewed` 切到锚、`feeder_id` 仍停在旧非锚窗口
                // → feeder_id 与 viewed 分叉,dims 广播 / orphan 回收 / 焦点跟随按 feeder_id 过滤时漏
                // 掉本客户端。对齐 handle_tab_op 里 Select 同步 feeder_id 的做法(get_mut 即释放后再
                // fed_point_client,借用不重叠)。
                if let Some(c) = data.clients.get_mut(&cid) {
                    c.feeder_id = Some(feeder_id);
                }
                fed_point_client(data, cid, ws, tab, false);
            }
            None => {
                if let Some(f) = data.feeders.get_mut(&feeder_id) {
                    f.pending_new_select = Some(cid);
                }
            }
        }
    }
}

/// 向所有 live WS 客户端推一条 [`ServerMsg::Dims`](A′ 增量1;Local Resize 用,广播到全部客户端)。
fn broadcast_dims(data: &mut DaemonData, cols: u16, rows: u16) {
    broadcast_control(data, &ServerMsg::Dims { cols, rows });
}

/// 向 attach 某 feeder 的客户端推该 feeder 的 [`ServerMsg::Dims`](联邦拓扑每 workspace 独立尺寸)。
fn broadcast_dims_for_feeder(data: &mut DaemonData, feeder_id: u64) {
    let Some((cols, rows)) = data.feeders.get(&feeder_id).and_then(|f| f.dims) else {
        return;
    };
    let clients: Vec<u64> = data
        .clients
        .iter()
        .filter(|(_, c)| c.feeder_id == Some(feeder_id))
        .map(|(id, _)| *id)
        .collect();
    for id in clients {
        send_control_to_client(data, id, &ServerMsg::Dims { cols, rows });
    }
}

/// 构某 feeder(= workspace)的 [`WorkspaceInfo`](tab 明细 + 桌面焦点 active + 尺寸)。
fn feeder_workspace_info(f: &Feeder) -> WorkspaceInfo {
    let (cols, rows) = f
        .dims
        .map(|(c, r)| (c as usize, r as usize))
        .unwrap_or((DEFAULT_COLS as usize, DEFAULT_ROWS as usize));
    WorkspaceInfo {
        workspace_id: f.ws_id,
        tabs: f.fed_tabs.clone(),
        active: f.fed_active,
        cols,
        rows,
    }
}

/// 构联邦拓扑的**聚合**工作区列表(ADR-0019:每个已声明 ws 的 feeder = 一个 workspace 摘要;锚标
/// `active=true`,`WorkspaceList.active` = 锚 ws_id)。尚未声明 ws(刚 accept 未收帧)的 feeder 跳过。
fn aggregate_workspace_list(data: &DaemonData) -> WorkspaceList {
    let anchor_ws = data
        .anchor_feeder
        .and_then(|a| data.feeders.get(&a))
        .map(|f| f.ws_id)
        .unwrap_or(0);
    let mut workspaces: Vec<WorkspaceMeta> = data
        .feeders
        .values()
        .filter(|f| f.ws_id != 0)
        .map(|f| WorkspaceMeta {
            id: f.ws_id,
            title: f
                .fed_tabs
                .get(f.fed_active)
                .map(|t| t.title.clone())
                .unwrap_or_default(),
            tab_count: f.fed_tabs.len(),
            active: f.ws_id == anchor_ws,
        })
        .collect();
    workspaces.sort_by_key(|w| w.id); // 稳定顺序(HashMap 迭代无序)。
    WorkspaceList {
        workspaces,
        active: anchor_ws,
    }
}

/// 向所有 live WS 客户端广播【聚合】工作区列表(ADR-0019:全部 feeder 的 workspace 摘要)。feeder
/// 接入/断开 / 任一 workspace 元数据变时调,让手机知道有哪些 workspace(F4 分段 UI 用;F1-F3 web
/// 只用 `active` 学锚 ws)。
fn broadcast_workspaces_list(data: &mut DaemonData) {
    let list = aggregate_workspace_list(data);
    broadcast_control(data, &ServerMsg::Workspaces(list));
}

/// 某 feeder 元数据变(焦点 / tab 列表)后广播:① 聚合工作区列表给所有客户端(知道有哪些 workspace);
/// ② 该 feeder 的 [`WorkspaceInfo`] 发给 **所有 live 客户端**(K1b/F4:每个客户端都要能跟到【每个
/// 窗口】的 tab 增删 / 焦点变化 → 分段栏,不再只发 attach 本 feeder 的那批)。
/// **单 feeder 时** = 所有客户端都 attach 它 → 与砖2 的"广播 Workspaces + Workspace 给全部"等价。
fn broadcast_feeder_metadata(data: &mut DaemonData, feeder_id: u64) {
    broadcast_workspaces_list(data);
    let info = match data.feeders.get(&feeder_id) {
        Some(f) => feeder_workspace_info(f),
        None => return,
    };
    let clients: Vec<u64> = data
        .clients
        .iter()
        .filter(|(_, c)| matches!(c.stage, WsStage::Live(_)))
        .map(|(id, _)| *id)
        .collect();
    for id in clients {
        send_control_to_client(data, id, &ServerMsg::Workspace(info.clone()));
    }
}

/// 把 WS 客户端输入字节路由到 input sink:
/// - **Local**(`session` 在)= 直写 active tab 的 PTY([`Session::on_input`]);
/// - **联邦**(feeders)= 按 `ws_id` 定位 owning feeder → **分帧**成 ≤ [`FED_INPUT_CHUNK`] 的多个
///   [`FrameKind::Input`] 帧(各带同 (`ws_id`, `tab_id`) 标签)经 back-channel 写该窗口(窗口 own
///   PTY,据 tab_id 写对应 PTY)。分帧防单条大输入(粘贴)超 feed payload 上限 / `u32` 截断。
///
/// `ws_id` = 客户端 viewed workspace 的 ws(= 所属 feeder 的 ws_id);找不到对应 feeder(race)→ 丢弃。
fn route_input(data: &mut DaemonData, ws_id: u64, tab_id: u64, bytes: &[u8]) {
    if let Some(session) = data.session.as_mut() {
        if let Err(e) = session.on_input(tab_id, bytes) {
            tracing::warn!(?e, tab_id, "WS 输入写 PTY 失败");
        }
        return;
    }
    let Some(feeder_id) = feeder_id_for_ws(data, ws_id) else {
        tracing::debug!(ws_id, tab_id, "输入无匹配 feeder(workspace 已断?),丢弃");
        return;
    };
    // 分帧 + 逐帧经 back-channel 队列保序回灌窗口(空输入 → chunks 无项 → 不发帧)。复用同一
    // `frame` buffer(clear 后重填)减分配。
    let mut frame = Vec::with_capacity(FEED_HEADER_LEN + bytes.len().min(FED_INPUT_CHUNK));
    for chunk in bytes.chunks(FED_INPUT_CHUNK) {
        frame.clear();
        encode_into(&mut frame, FrameKind::Input, ws_id, tab_id, chunk);
        feeder_send_frame(data, feeder_id, &frame);
    }
}

/// 按 workspace id 找 owning feeder id(联邦拓扑;窗口进程唯一 ws → 至多一个 feeder 匹配)。
fn feeder_id_for_ws(data: &DaemonData, ws_id: u64) -> Option<u64> {
    data.feeders
        .iter()
        .find(|(_, f)| f.ws_id == ws_id)
        .map(|(id, _)| *id)
}

/// 把一帧 encode 好的字节经某 feeder 的 back-channel 写窗口(kernel→窗口 Input / TabOp 帧)。**绝不
/// 丢半帧**:队空时尽量直写(WouldBlock 写不完的剩余字节入队),队非空时直接追加(保序);有积压
/// 则 arm WRITE 源,可写时 [`feeder_back_writable`] drain。出站积压超 [`FED_BACK_OUT_CAP`] = 窗口长期
/// 不读 → **移除该 feeder**(= 移除其 workspace),绝不无界堆 + 绝不丢半帧污染流。**绝不阻塞 loop**。
fn feeder_send_frame(data: &mut DaemonData, feeder_id: u64, frame: &[u8]) {
    let mut fatal = false;
    let mut over_cap = false;
    let mut need_arm = false;
    if let Some(feeder) = data.feeders.get_mut(&feeder_id) {
        let fd = feeder.back.as_raw_fd();
        let mut start = 0;
        // 仅队空时直写(队非空直写会与待 drain 的字节乱序);写不完的入队。
        if feeder.outbound.is_empty() {
            while start < frame.len() {
                // SAFETY: fd 来自本 feeder 持有的活 OwnedFd;libc::write 只调 syscall 不动 fd 所有权,
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
                    tracing::warn!(
                        feeder = feeder_id,
                        "feeder back-channel 直写返回 0,移除该 feeder"
                    );
                    fatal = true;
                    break;
                }
                let err = io::Error::last_os_error();
                match err.kind() {
                    io::ErrorKind::Interrupted => continue,
                    io::ErrorKind::WouldBlock => break, // 写不完的入队
                    _ => {
                        tracing::warn!(
                            feeder = feeder_id,
                            ?err,
                            "feeder back-channel 直写失败,移除该 feeder"
                        );
                        fatal = true;
                        break;
                    }
                }
            }
        }
        if !fatal && start < frame.len() {
            feeder.outbound.extend(&frame[start..]);
            over_cap = feeder.outbound.len() > FED_BACK_OUT_CAP;
            need_arm = !over_cap;
        }
    } else {
        return; // feeder 已移除(race)
    }
    if fatal || over_cap {
        if over_cap {
            tracing::error!(
                feeder = feeder_id,
                cap = FED_BACK_OUT_CAP,
                "feeder back-channel 出站积压超上限 (窗口长期不读),移除该 feeder"
            );
        }
        // 从非该 feeder 自身回调(input sink)进入 → 两源都由 loop_handle.remove 拆(external)。
        feeder_teardown(data, feeder_id, true, true);
        return;
    }
    if need_arm {
        feeder_arm_write(data, feeder_id);
    }
}

/// 打开某 feeder back-channel WRITE 兴趣(出站有积压时)。已 armed 则跳过(防重复 enable)。从
/// **非该 WRITE 源自身**的 callback 调([`feeder_send_frame`]),故 `enable` 立即生效。
fn feeder_arm_write(data: &mut DaemonData, feeder_id: u64) {
    let token = match data.feeders.get(&feeder_id).and_then(|f| f.write.as_ref()) {
        Some(w) if !w.armed => w.token,
        _ => return, // 无 WRITE 源(瞬态)/ 已 armed
    };
    if let Err(e) = data.loop_handle.enable(&token) {
        tracing::warn!(
            ?e,
            feeder = feeder_id,
            "enable feeder back-channel WRITE 源失败"
        );
        return;
    }
    if let Some(w) = data
        .feeders
        .get_mut(&feeder_id)
        .and_then(|f| f.write.as_mut())
    {
        w.armed = true;
    }
}

/// 某 feeder back-channel WRITE 源可写:drain 其出站字节队列到窗口。`WouldBlock` → 保留 WRITE 兴趣
/// (`Continue`);**队列排空 → 关 WRITE 兴趣(`Disable`,不忙等)**;写 0 / 其它错误(窗口读端没了)
/// → 移除该 feeder(本 WRITE 源自身 → 返 `Remove`;teardown 只移 READ 源)。**绝不丢字节**。
fn feeder_back_writable(data: &mut DaemonData, feeder_id: u64) -> io::Result<PostAction> {
    enum Outcome {
        Disable,
        Continue,
        Down,
    }
    let outcome = {
        let Some(feeder) = data.feeders.get_mut(&feeder_id) else {
            return Ok(PostAction::Remove); // feeder 已移除(race)
        };
        let fd = feeder.back.as_raw_fd();
        loop {
            let front = feeder.outbound.as_slices().0;
            if front.is_empty() {
                if let Some(w) = feeder.write.as_mut() {
                    w.armed = false;
                }
                break Outcome::Disable;
            }
            // SAFETY: fd 来自本 feeder 持有的活 OwnedFd;front 是 `outbound` 当前内容的活切片;
            // libc::write 只调 syscall 不动 fd 所有权。
            #[allow(unsafe_code)]
            let n = unsafe { libc::write(fd, front.as_ptr().cast::<libc::c_void>(), front.len()) };
            if n > 0 {
                feeder.outbound.drain(..n as usize);
                continue;
            }
            if n == 0 {
                tracing::warn!(
                    feeder = feeder_id,
                    "feeder back-channel write 返回 0,移除该 feeder"
                );
                break Outcome::Down;
            }
            let err = io::Error::last_os_error();
            match err.kind() {
                io::ErrorKind::Interrupted => continue,
                io::ErrorKind::WouldBlock => break Outcome::Continue,
                _ => {
                    tracing::warn!(
                        feeder = feeder_id,
                        ?err,
                        "feeder back-channel write 失败,移除该 feeder"
                    );
                    break Outcome::Down;
                }
            }
        }
    };
    match outcome {
        Outcome::Disable => Ok(PostAction::Disable),
        Outcome::Continue => Ok(PostAction::Continue),
        Outcome::Down => {
            // 本回调 = 该 feeder WRITE 源自身 → 只移 READ 源,返 Remove 注销 WRITE 源自身。
            feeder_teardown(data, feeder_id, true, false);
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
            // viewed / follow / feeder_id 在 Peeking/Handshaking 阶段无意义,转 Live 时(ws_go_live)设真值。
            viewed: (0, 0),
            feeder_id: None,
            follow_focus: true,
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
        // P0 CSWSH 修复:带鉴权回调(token + Origin + Host)+ 收紧的 WebSocketConfig(灭 64MiB
        // 默认消息上限,#2/#14)。callback 从 DaemonData 克隆 token / 允许集(Rc,零深拷贝)。
        let callback = AuthCallback {
            token: Rc::clone(&data.ws_token),
            allowed_hosts: Rc::clone(&data.allowed_hosts),
        };
        let ws_config = WebSocketConfig::default()
            .max_message_size(Some(WS_MAX_MESSAGE_SIZE))
            .max_frame_size(Some(WS_MAX_MESSAGE_SIZE));
        let mid = ServerHandshake::start(
            PrefixStream::new(head, io_stream),
            callback,
            Some(ws_config),
        );
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

/// 握手完成转 Live:为出站建一个 WRITE 源(owns 另一份 dup 当可写信号)+ 把**初始 viewed tab**
/// 的环缓冲重放排进出站(重建当前屏)。初始 viewed = 桌面焦点 tab、`follow_focus=true`(砖2 B3)。
///
/// **重放 + 订阅原子性**:单线程串行,本函数与 [`fan_out_tab_bytes`] 绝不交错 —— `ring.snapshot()`
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

    // 初始 attach 的 workspace:Local = 本 daemon 唯一工作区;联邦 = 锚 feeder(F3:新连接默认看锚
    // workspace 焦点 tab)。无锚(Federated 尚无 feeder)→ 悬空(feeder_id None),待 feeder 接入
    // ([`attach_feeder`])attach 它。
    let (attach_feeder, ws_id, viewed_tab) = if data.session.is_some() {
        (None, data.workspace_id, data.tab_id)
    } else {
        match data
            .anchor_feeder
            .and_then(|a| data.feeders.get(&a).map(|f| (a, f)))
        {
            Some((a, f)) => (Some(a), f.ws_id, f.focus_tab),
            None => (None, 0, 0),
        }
    };
    let viewed = (ws_id, viewed_tab);
    // 控制面连上引导帧:工作区列表 + 所看工作区结构 + 字节流 (workspace,tab) 标签 + Dims。
    let control = build_control_text(data, attach_feeder, ws_id, viewed_tab);
    let replay = data
        .rings
        .get(&viewed)
        .map(|r| r.snapshot())
        .unwrap_or_default();
    if let Some(c) = data.clients.get_mut(&id) {
        c.stage = WsStage::Live(ws);
        // 本连接初始视图 = 所看 workspace 焦点 tab,跟随焦点(未 pin);Select 后 pin。
        c.viewed = viewed;
        c.feeder_id = attach_feeder;
        c.follow_focus = true;
        // holder 仅 Local(有 Session)登记:先确认连接在册再登记 holder + held_ws(二者一起
        // 设)→ 杜绝登记了 holder 却没记 held_ws、回收时无从释放的孤立态。联邦拓扑 workspace 生命
        // 周期归 feeder refcount(F2b),客户端只用 `clients` 表做 fan-out + 回收 → held_ws 留 None,
        // 回收路径 [`release_holder`] 自然 no-op。
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
/// ([`build_control_text_local`]);**联邦** 用 attach 的 feeder 元数据 ([`build_control_text_feeder`])。
/// `attach_feeder` = 该连接初始 attach 的 feeder(联邦;`None` = Local 或悬空);`ws_id` / `viewed_tab`
/// = 该连接初始 viewed。
fn build_control_text(
    data: &DaemonData,
    attach_feeder: Option<u64>,
    ws_id: u64,
    viewed_tab: u64,
) -> Vec<String> {
    let mut out = if let Some(session) = &data.session {
        build_control_text_local(session, ws_id)
    } else {
        build_control_text_feeder(data, attach_feeder, ws_id, viewed_tab)
    };
    // A′ 增量1:已知尺寸 → 末尾追一条 ServerMsg::Dims,客户端 term.resize 到桌面宽渲染。Local 用
    // data.dims;联邦用 attach feeder 的 dims(未到则此连接靠后续 broadcast_dims_for_feeder 补)。
    let dims = if data.session.is_some() {
        data.dims
    } else {
        attach_feeder
            .and_then(|a| data.feeders.get(&a))
            .and_then(|f| f.dims)
    };
    if let Some((cols, rows)) = dims {
        push_server_json(&mut out, &ServerMsg::Dims { cols, rows });
    }
    out
}

/// 联邦拓扑连上引导帧:① 聚合工作区列表 ② **每个**已声明 ws 的 feeder(= 每个窗口)的 tab 明细
/// (K1a/F4:客户端一次拿到【全部窗口】的 tab 列表 → 分段栏;非 attach 的窗口也拿得到)③ attach 的
/// feeder(= 所看 workspace)的字节流 (workspace, tab) 标签。无 attach feeder(悬空)→ 仍发聚合列表
/// + 各 feeder 明细(可能空),仅省 StreamFocus,待 feeder 接入 + 声明后补齐。
///
/// 顺序:先 Workspaces 列表,再各 Workspace 明细(按 ws_id 稳定排),再 StreamFocus。
fn build_control_text_feeder(
    data: &DaemonData,
    attach_feeder: Option<u64>,
    ws_id: u64,
    viewed_tab: u64,
) -> Vec<String> {
    let mut out = Vec::new();
    push_server_json(
        &mut out,
        &ServerMsg::Workspaces(aggregate_workspace_list(data)),
    );
    // K1a(F4):对【每个】已声明 ws 的 feeder 都发一条 Workspace 明细。HashMap 迭代无序 → 按 ws_id
    // 排(与 aggregate_workspace_list 一致),使客户端分段顺序稳定。尚未声明 ws(刚 accept)的跳过。
    let mut infos: Vec<WorkspaceInfo> = data
        .feeders
        .values()
        .filter(|f| f.ws_id != 0)
        .map(feeder_workspace_info)
        .collect();
    infos.sort_by_key(|i| i.workspace_id);
    for info in infos {
        push_server_json(&mut out, &ServerMsg::Workspace(info));
    }
    // StreamFocus 仍只声明 attach/viewed 的那个(此后 Binary 字节流属于哪个 (ws,tab))。
    if attach_feeder.and_then(|a| data.feeders.get(&a)).is_some() {
        push_server_json(
            &mut out,
            &ServerMsg::StreamFocus {
                workspace_id: ws_id,
                tab_id: viewed_tab,
            },
        );
    }
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
/// 写 PTY(同线程,无 channel)。Close / 致命错 → 断开。
fn ws_live_read(data: &mut DaemonData, id: u64) -> io::Result<PostAction> {
    loop {
        let res = match data.clients.get_mut(&id) {
            Some(c) => match &mut c.stage {
                WsStage::Live(ws) => ws.read(),
                _ => return Ok(PostAction::Continue),
            },
            None => return Ok(PostAction::Remove),
        };
        match res {
            // 数据面:浏览器输入裸字节(WS Binary)→ 写本客户端**当前 viewed tab**(砖2 B3:每客户端
            // 各看各的 → 各写各的 viewed tab,非全局焦点)。Local 直写该 tab PTY;Fed encode Input 帧
            // (带 viewed (ws,tab) 标签)回灌父 back-channel(父据标写对应 tab PTY,子自己不写 PTY)。
            Ok(Message::Binary(b)) => {
                let (vws, vtab) = data
                    .clients
                    .get(&id)
                    .map(|c| c.viewed)
                    .unwrap_or((data.workspace_id, data.tab_id));
                route_input(data, vws, vtab, &b);
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
                } else {
                    // A′ 增量1:Local PTY 尺寸变了 → 同步 dims + 广播给其它客户端(同尺寸渲染)。
                    data.dims = Some((cols, rows));
                    broadcast_dims(data, cols, rows);
                }
            } else {
                // 联邦:resize 须回灌窗口(窗口 own PTY)。feed 帧集不含 Resize 帧,留后续。
                tracing::debug!(
                    cols,
                    rows,
                    "联邦拓扑 resize 暂不回灌窗口(feed 帧集无 Resize)"
                );
            }
            None
        }
        ClientMsg::TabOp { workspace_id, op } => {
            handle_tab_op(data, id, workspace_id, op);
            None
        }
    }
}

/// 处理一条 [`ClientMsg::TabOp`]。**仅联邦拓扑**接线(Local standalone 单 active tab 字节泵不支持
/// 多 tab 视图,debug 忽略)。`workspace_id` = 消息里客户端点的那个窗口(分段栏每个段一个 window):
/// - `Select { idx }` = **K2/F4 跨窗口**:据消息 `workspace_id` 定位目标 feeder(**不依赖客户端当前
///   attach 的那个 feeder**),把本客户端 viewed 切到该窗口第 idx 个 tab(不动桌面焦点、不影响别的
///   客户端)+ 重放目标 tab 环缓冲(`follow=false` pin);并把本客户端 `feeder_id` 更新为目标窗口
///   → 随后 Close / 焦点跟随对准"当前所看窗口";
/// - `New` = **回灌消息 `workspace_id` 那个窗口 spawn**(F4+:分段栏每窗口各有 +,新 tab 落你点的
///   那个窗口);找不到该窗口回落锚 feeder(F3 默认 home = 第一个开共享的窗口),再无锚回落 attach;
/// - `Close` / `Reorder` = 据消息 `workspace_id` 定位窗口回灌(择稳:关/换哪个窗口的 tab 由客户端
///   点的段决定,独立于本客户端在看哪个);
/// - `SetTitle` = 暂不接(桌面无对应操作,留后续)。
///
/// 越界 / 找不到目标 feeder → debug 忽略不 panic。
fn handle_tab_op(data: &mut DaemonData, id: u64, workspace_id: u64, op: TabOp) {
    if data.feeders.is_empty() {
        tracing::debug!(?op, "Local / 无 feeder,TabOp 不接线,忽略");
        return;
    }
    match op {
        // K2(F4):跨窗口 Select —— 用消息 workspace_id 定位目标 feeder(现在参数不再被忽略)。
        TabOp::Select { idx } => {
            let Some(target_feeder) = feeder_id_for_ws(data, workspace_id) else {
                tracing::debug!(
                    workspace_id,
                    "TabOp::Select 无匹配 feeder(workspace 已断?),忽略"
                );
                return;
            };
            let target = data
                .feeders
                .get(&target_feeder)
                .and_then(|f| f.fed_tabs.get(idx).map(|t| (f.ws_id, t.tab_id)));
            let Some((ws, tab_id)) = target else {
                tracing::debug!(
                    idx,
                    workspace_id,
                    "TabOp::Select idx 越界(tab 列表未同步?),忽略"
                );
                return;
            };
            // 更新 attach feeder = 目标窗口 → 随后 Close / 焦点跟随对准"当前所看窗口"(见函数 doc)。
            if let Some(c) = data.clients.get_mut(&id) {
                c.feeder_id = Some(target_feeder);
            }
            fed_point_client(data, id, ws, tab_id, false);
        }
        // F3:New → 回灌**锚 feeder**(= 第一个开共享的窗口 = home),**不是**客户端在看的那个
        // workspace —— "新 tab 落共享锚窗口"是有语义的 #0(ADR-0019 §4)。记发起客户端到锚 feeder →
        // 锚窗口发回含新 tab 的 TabList 时把该客户端自动 Select 到新 tab(见 feeder_tab_list_updated)。
        // 无锚(不该发生:feeders 非空必有锚)回落到客户端 attach 的 feeder。
        TabOp::New => {
            // F4+:手机在分段栏点【某个窗口】的 + → 新 tab 落那个窗口(消息 workspace_id)。找不到该
            // 窗口(race / 未声明 ws)回落锚 feeder(F3 默认 home = 开共享的窗口),再无锚回落客户端
            // attach 的 feeder。落点 feeder 记 pending_new_select → 其窗口发回含新 tab 的 TabList 时把
            // 发起客户端自动 Select 到新 tab(feeder_tab_list_updated,含 feeder_id 对齐)。
            let Some(target) = feeder_id_for_ws(data, workspace_id)
                .or(data.anchor_feeder)
                .or_else(|| data.clients.get(&id).and_then(|c| c.feeder_id))
            else {
                tracing::debug!("TabOp::New 无匹配窗口 / 无锚 / 客户端未 attach feeder,忽略");
                return;
            };
            if let Some(f) = data.feeders.get_mut(&target) {
                f.pending_new_select = Some(id);
            }
            forward_tab_op_to_feeder(data, target, FeedTabOp::New);
        }
        // Close / Reorder 按消息 workspace_id 路由到该窗口的 feeder(择稳:不依赖客户端 viewed 状态)。
        TabOp::Close { tab_id } => {
            // 按消息 workspace_id 定位目标窗口;找不到(Fed-child 拓扑 feeder 的 ws≠消息值 / race)
            // 回退到客户端 attach 的 feeder(= pre-F4 行为)。**安全**:不用 anchor(会关错窗口),
            // 回退窗口若无此 tab_id 其 apply_tab_op Close 找不到即 no-op,不会误关。
            let Some(feeder_id) = feeder_id_for_ws(data, workspace_id)
                .or_else(|| data.clients.get(&id).and_then(|c| c.feeder_id))
            else {
                tracing::debug!(workspace_id, "TabOp::Close 无匹配 feeder / 未 attach,忽略");
                return;
            };
            forward_tab_op_to_feeder(data, feeder_id, FeedTabOp::Close { tab_id });
        }
        TabOp::Reorder { origin, target } => {
            // 同 Close:workspace_id 定位不到回退 attach feeder(reorder 索引仅在该窗口内有意义)。
            let Some(feeder_id) = feeder_id_for_ws(data, workspace_id)
                .or_else(|| data.clients.get(&id).and_then(|c| c.feeder_id))
            else {
                tracing::debug!(workspace_id, "TabOp::Reorder 无匹配 feeder / 未 attach,忽略");
                return;
            };
            forward_tab_op_to_feeder(
                data,
                feeder_id,
                FeedTabOp::Reorder {
                    origin: origin as u32,
                    target: target as u32,
                },
            );
        }
        TabOp::SetTitle { .. } => {
            tracing::debug!("TabOp::SetTitle 暂不接线(桌面无对应操作),忽略");
        }
    }
}

/// 把手机发起的 tab 操作(New / Close / Reorder)编成 [`FrameKind::TabOp`] 帧,经 back-channel 回灌
/// **指定 feeder** 的窗口执行(该 workspace 的 PTY / TabList 归它,kernel 只转发)。复用
/// [`feeder_send_frame`] 的绝不丢半帧 + 背压超限降级路径。帧头 `ws_id` = 该 feeder 声明的 ws;
/// `tab_id` = Close 目标(便利,权威在 payload),其余填 0。
fn forward_tab_op_to_feeder(data: &mut DaemonData, feeder_id: u64, op: FeedTabOp) {
    let Some(ws) = data.feeders.get(&feeder_id).map(|f| f.ws_id) else {
        return;
    };
    let tab_hdr = match op {
        FeedTabOp::Close { tab_id } => tab_id,
        FeedTabOp::New | FeedTabOp::Reorder { .. } => 0,
    };
    let payload = encode_tab_op(op);
    let mut frame = Vec::with_capacity(FEED_HEADER_LEN + payload.len());
    encode_into(&mut frame, FrameKind::TabOp, ws, tab_hdr, &payload);
    feeder_send_frame(data, feeder_id, &frame);
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

/// WS listener 的 `listen(2)` backlog。与 Rust `std::net::TcpListener::bind` 的默认值一致
/// (128),保持替换前后行为等价;单用户 + WireGuard VPN 把门,远够用。
const WS_LISTEN_BACKLOG: libc::c_int = 128;

/// 建 WS TCP listener,**bind 前设 `SO_REUSEADDR`**(std 的 `TcpListener::bind` 在 Linux 上
/// 不设此选项,这是本函数存在的唯一理由)。
///
/// **为何需要**:运行期共享开关 toggle-off 杀子进程时,手机那些 established 连接进 TIME-WAIT
/// (本地 `(addr, 7878)`),~60s 内 toggle-on 的新子若用裸 `bind` 会撞 `EADDRINUSE` → 子退出 →
/// 父读 back-channel EOF → 静默拆 share(icon 闪绿即回灰)。`SO_REUSEADDR` 是服务器重启重绑的
/// 标准解:**只放行被 TIME-WAIT 占着的 (addr,port),不破坏正常独占**(同地址有 active listener
/// 仍 `EADDRINUSE`,见单测)。对 server listener 普适有益,两拓扑(Local / Fed)一致生效,无需 gate。
///
/// **为何手搓 libc 而非引 socket2**:契合本文件既有 libc/unsafe 风格(`enable_keepalive` /
/// `cap_http_send_buffer` 同款 setsockopt;`make_feeder` 同款 socket fd 手管),零新依赖、零 ADR。
///
/// **fd 不泄漏**:`socket(2)` 成功后立刻包成 [`OwnedFd`] —— 之后任何一步(setsockopt / bind /
/// listen)失败 `?`/`return` 时 `OwnedFd` 析构即 `close(2)`,不漏 fd;全部成功才 `OwnedFd` →
/// [`TcpListener`](所有权转移,后续由 `TcpListener` 关闭)。返回的 listener 仍是阻塞的,调用方
/// 照旧 `set_nonblocking(true)`(Level 模式下阻塞 accept 会 stall 单线程 loop)。
fn bind_ws_listener(addr: SocketAddr) -> Result<TcpListener> {
    let domain = match addr {
        SocketAddr::V4(_) => libc::AF_INET,
        SocketAddr::V6(_) => libc::AF_INET6,
    };
    // SAFETY: socket(2) 用合法 domain + SOCK_STREAM 建新 fd,失败返 -1。SOCK_CLOEXEC 与 std
    // 一致(防 listener fd 泄漏进随后 spawn 的 shell / Fed 子进程)。
    #[allow(unsafe_code)]
    let fd = unsafe { libc::socket(domain, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0) };
    if fd < 0 {
        return Err(anyhow::Error::new(io::Error::last_os_error()).context("WS socket(2) 创建失败"));
    }
    // SAFETY: fd 是本函数刚建、独占的合法 socket fd;立刻交 OwnedFd 接管 close —— 下面任何
    // 失败路径 return 时析构即关,不漏 fd。raw `fd`(int,Copy)在 owned 存活期间一直有效,
    // 仅用于下面几个 syscall;成功路径末尾 owned move 进 TcpListener(不 double-close)。
    #[allow(unsafe_code)]
    let owned = unsafe { OwnedFd::from_raw_fd(fd) };

    let reuse: libc::c_int = 1;
    // SAFETY: setsockopt 只读栈上一个 c_int(`size_of` 字节,只读不写),fd 为上面活着的 socket。
    #[allow(unsafe_code)]
    let rc = unsafe {
        libc::setsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_REUSEADDR,
            std::ptr::addr_of!(reuse).cast::<libc::c_void>(),
            std::mem::size_of::<libc::c_int>() as libc::socklen_t,
        )
    };
    if rc < 0 {
        return Err(anyhow::Error::new(io::Error::last_os_error())
            .context("WS setsockopt(SO_REUSEADDR) 失败"));
    }

    // 构 sockaddr 并 bind。端口走 `to_be()`(htons:内存按网络序大端);地址 octets 已是网络序
    // (`Ipv4Addr/Ipv6Addr::octets`),V4 用 `from_ne_bytes` 让 s_addr 内存字节恰等于 octets、
    // V6 直接拷 16 字节。`zeroed()` 把 sin_zero / 任何 padding 清零(POD,全零是合法初值)。
    let bind_rc = match addr {
        SocketAddr::V4(v4) => {
            // SAFETY: sockaddr_in 是 POD,全零是合法初值(且为清 sin_zero 的惯用法)。
            #[allow(unsafe_code)]
            let mut sa: libc::sockaddr_in = unsafe { std::mem::zeroed() };
            sa.sin_family = libc::AF_INET as libc::sa_family_t;
            sa.sin_port = v4.port().to_be();
            sa.sin_addr.s_addr = u32::from_ne_bytes(v4.ip().octets());
            // SAFETY: bind 只读 sa 的 size_of::<sockaddr_in> 字节(不写),sa 此刻栈上活着;
            // fd 为上面活着的 socket。
            #[allow(unsafe_code)]
            unsafe {
                libc::bind(
                    fd,
                    std::ptr::addr_of!(sa).cast::<libc::sockaddr>(),
                    std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t,
                )
            }
        }
        SocketAddr::V6(v6) => {
            // SAFETY: sockaddr_in6 是 POD,全零是合法初值。
            #[allow(unsafe_code)]
            let mut sa: libc::sockaddr_in6 = unsafe { std::mem::zeroed() };
            sa.sin6_family = libc::AF_INET6 as libc::sa_family_t;
            sa.sin6_port = v6.port().to_be();
            sa.sin6_flowinfo = v6.flowinfo();
            sa.sin6_addr.s6_addr = v6.ip().octets();
            sa.sin6_scope_id = v6.scope_id();
            // SAFETY: bind 只读 sa 的 size_of::<sockaddr_in6> 字节(不写),sa 此刻栈上活着;
            // fd 为上面活着的 socket。
            #[allow(unsafe_code)]
            unsafe {
                libc::bind(
                    fd,
                    std::ptr::addr_of!(sa).cast::<libc::sockaddr>(),
                    std::mem::size_of::<libc::sockaddr_in6>() as libc::socklen_t,
                )
            }
        }
    };
    if bind_rc < 0 {
        return Err(anyhow::Error::new(io::Error::last_os_error())
            .context(format!("WS bind {addr} 失败(端口占用?)")));
    }

    // SAFETY: listen 只对上面活着的 socket fd 操作,失败返 -1。
    #[allow(unsafe_code)]
    let listen_rc = unsafe { libc::listen(fd, WS_LISTEN_BACKLOG) };
    if listen_rc < 0 {
        return Err(anyhow::Error::new(io::Error::last_os_error())
            .context(format!("WS listen {addr} 失败")));
    }

    // 全部成功:OwnedFd → TcpListener(所有权转移,无 double-close)。
    Ok(TcpListener::from(owned))
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
    // 砖2 W3:`/sw.js` 单独走(注入 app-shell 内容哈希当缓存版本 key → 动态 body);其余
    // 静态资产零拷贝返 `&'static [u8]`。`sw_body` String 撑住 sw.js 情形的 body 生命周期。
    let sw_body;
    let (status, ctype, body): (&str, &str, &[u8]) = if path == "/sw.js" {
        sw_body = sw_js_served();
        (
            "200 OK",
            "text/javascript; charset=utf-8",
            sw_body.as_bytes(),
        )
    } else {
        http_response_parts(&path)
    };
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

/// 砖2 W3:serve 的 Service Worker 脚本 = 注入 app-shell 内容哈希版本(`ASSET_VERSION`)+
/// 内嵌 sw.js 源。`__QUILL_VERSION__` 随资产改动而变 → sw.js 字节变 → 浏览器重装 SW 清旧缓存。
fn sw_js_served() -> String {
    // "use strict" 置于 prologue 首条 → SW 严格模式生效。注入版本行若前置会把 sw.js
    // 自身的 "use strict" 顶出 prologue 位置使其退化成无副作用字符串,故这里在 wrapper
    // 首行补一条 "use strict";(sw.js 里那条留作 standalone 有效性,此处第二条为无害 no-op)。
    format!("\"use strict\";\nself.__QUILL_VERSION__=\"{ASSET_VERSION:016x}\";\n{SW_JS}")
}

/// path → (status line, content-type, body)。纯函数,便于单测。`/sw.js` 不走这里(动态注入
/// 版本,见 [`sw_js_served`] / [`serve_http_response`])。
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

    /// 砖2 W3:serve 的 sw.js = 注入的版本行 + 内嵌 SW 源;版本 = app-shell 内容哈希(稳定、
    /// 非零、随资产变)。缓存 key 前缀在两处一致(注入行 → sw.js 用它拼 `quill-shell-<ver>`)。
    #[test]
    fn sw_js_served_injects_version_and_shell() {
        let js = sw_js_served();
        // "use strict" 置顶(严格模式);紧随注入版本行,十六进制 = ASSET_VERSION(16 位定宽)。
        let expect = format!("self.__QUILL_VERSION__=\"{ASSET_VERSION:016x}\";");
        assert!(
            js.starts_with("\"use strict\";"),
            "SW 应以 use strict 指令开头(严格模式)"
        );
        assert!(js.contains(&expect), "应注入版本行");
        // 内嵌 SW 源被带上(cache-first shell 逻辑在里头)。
        assert!(js.contains("quill-shell-"), "应含缓存命名空间前缀");
        assert!(js.contains("addEventListener(\"fetch\""), "应含 fetch 处理");
        // 哈希非退化(FNV-1a 初值被资产扰动过)。
        assert_ne!(ASSET_VERSION, 0);
        assert_ne!(ASSET_VERSION, 0xcbf2_9ce4_8422_2325);
    }

    /// 读一个 fd 的 `SO_REUSEADDR` 当前值(测试辅助)。
    fn so_reuseaddr(l: &TcpListener) -> libc::c_int {
        let fd = l.as_raw_fd();
        let mut val: libc::c_int = -1;
        let mut len = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
        // SAFETY: getsockopt 往栈上活着的 c_int `val` 写至多 `len` 字节并回填 `len`;fd 为
        // 参数里活着的 TcpListener。tests 部分仍受 crate `deny(unsafe_code)` 约束,故显式 allow。
        #[allow(unsafe_code)]
        let rc = unsafe {
            libc::getsockopt(
                fd,
                libc::SOL_SOCKET,
                libc::SO_REUSEADDR,
                std::ptr::addr_of_mut!(val).cast::<libc::c_void>(),
                &mut len,
            )
        };
        assert_eq!(rc, 0, "getsockopt(SO_REUSEADDR) 应成功");
        val
    }

    /// `bind_ws_listener` 绑临时端口成功,且 `SO_REUSEADDR` 真被设上(getsockopt 返非 0)。
    #[test]
    fn bind_ws_listener_sets_reuseaddr() {
        let addr: SocketAddr = "127.0.0.1:0".parse().expect("解析回环临时地址");
        let listener = bind_ws_listener(addr).expect("bind_ws_listener 应成功");
        let bound = listener.local_addr().expect("local_addr");
        assert_ne!(bound.port(), 0, "OS 应分配了真实端口");
        assert!(bound.ip().is_loopback(), "应绑回环地址");
        assert_ne!(
            so_reuseaddr(&listener),
            0,
            "SO_REUSEADDR 应已设上(getsockopt 返非 0)"
        );
    }

    /// P0 CSWSH:query 取 `t`。
    #[test]
    fn query_param_extracts_token() {
        assert_eq!(query_param("t=abc", "t"), Some("abc"));
        assert_eq!(query_param("a=1&t=xyz&b=2", "t"), Some("xyz"));
        assert_eq!(query_param("a=1&b=2", "t"), None);
        assert_eq!(query_param("", "t"), None);
        assert_eq!(query_param("t=", "t"), Some("")); // 空值(会被 token 比较拒)
    }

    /// P0 CSWSH:常量时间比较对/错/长度差。
    #[test]
    fn constant_time_eq_matches() {
        assert!(constant_time_eq(b"deadbeef", b"deadbeef"));
        assert!(!constant_time_eq(b"deadbeef", b"deadbeee"));
        assert!(!constant_time_eq(b"short", b"longer-value"));
        assert!(!constant_time_eq(b"", b"x"));
        assert!(constant_time_eq(b"", b""));
    }

    /// P0 CSWSH:authority / origin 抠裸主机(去端口 + v6 方括号)。
    #[test]
    fn host_extraction() {
        assert_eq!(host_only("10.0.0.2:7878"), "10.0.0.2");
        assert_eq!(host_only("localhost"), "localhost");
        assert_eq!(host_only("[::1]:7878"), "::1");
        assert_eq!(host_only("[::1]"), "::1");
        assert_eq!(origin_host("http://10.0.0.2:7878"), Some("10.0.0.2"));
        assert_eq!(origin_host("http://evil.com"), Some("evil.com"));
        assert_eq!(origin_host("https://[::1]:7878"), Some("::1"));
        assert_eq!(origin_host("not-a-url"), None);
    }

    /// P0 CSWSH:允许集 —— 任意 IP 字面量 + localhost 放行(防 rebinding 但不误杀合法 IP/域名访问),
    /// 非 IP 且不在集里的域名(evil.com)拒;env 扩展的域名放行。
    #[test]
    fn host_allow_predicate() {
        let mut allowed = HashSet::new();
        allowed.insert("vpn.example.com".to_string());
        assert!(host_is_allowed("127.0.0.1", &allowed));
        assert!(host_is_allowed("10.0.0.2", &allowed));
        assert!(host_is_allowed("::1", &allowed));
        assert!(host_is_allowed("localhost", &allowed));
        assert!(host_is_allowed("LOCALHOST", &allowed)); // 大小写不敏感
        assert!(host_is_allowed("vpn.example.com", &allowed));
        assert!(!host_is_allowed("evil.com", &allowed)); // rebinding 域名:拒
        assert!(!host_is_allowed("", &allowed));
    }

    /// P0 CSWSH:随机 token 非空、64 hex 字符、两次不同(熵)。
    #[test]
    fn generate_token_is_random_hex() {
        let a = generate_ws_token().expect("gen token a");
        let b = generate_ws_token().expect("gen token b");
        assert_eq!(a.len(), 64, "32 字节 → 64 hex");
        assert!(a.bytes().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, b, "两次生成应不同(CSPRNG)");
    }

    /// P0 CSWSH:拒绝响应是 403 + body。
    #[test]
    fn reject_response_is_403() {
        let r = reject_response("nope");
        assert_eq!(r.status(), tungstenite::http::StatusCode::FORBIDDEN);
        assert_eq!(r.body().as_deref(), Some("nope"));
    }

    /// P0 CSWSH:构建一个测试请求(origin-form target + 可选 origin/host 头)。
    fn mk_request(target: &str, origin: Option<&str>, host: Option<&str>) -> Request {
        let mut b = tungstenite::http::Request::builder().uri(target);
        if let Some(o) = origin {
            b = b.header("origin", o);
        }
        if let Some(h) = host {
            b = b.header("host", h);
        }
        b.body(()).expect("build test request")
    }

    fn mk_callback(token: &str) -> AuthCallback {
        AuthCallback {
            token: Rc::from(token),
            allowed_hosts: Rc::new(HashSet::new()),
        }
    }

    /// P0 CSWSH 正向:对 token + 同源 Origin + IP Host → 放行。
    #[test]
    fn auth_callback_accepts_valid() {
        let req = mk_request(
            "/?t=secret",
            Some("http://10.0.0.2:7878"),
            Some("10.0.0.2:7878"),
        );
        let resp = tungstenite::http::Response::new(());
        assert!(mk_callback("secret").on_request(&req, resp).is_ok());
    }

    /// P0 CSWSH 正向:native 客户端(无 Origin)+ 正确 token + localhost Host → 放行。
    #[test]
    fn auth_callback_accepts_no_origin() {
        let req = mk_request("/?t=secret", None, Some("localhost:7878"));
        let resp = tungstenite::http::Response::new(());
        assert!(mk_callback("secret").on_request(&req, resp).is_ok());
    }

    /// P0 CSWSH 负向:token 缺失 → 拒。
    #[test]
    fn auth_callback_rejects_missing_token() {
        let req = mk_request("/", None, Some("10.0.0.2:7878"));
        let resp = tungstenite::http::Response::new(());
        let err = mk_callback("secret").on_request(&req, resp).unwrap_err();
        assert_eq!(err.status(), tungstenite::http::StatusCode::FORBIDDEN);
    }

    /// P0 CSWSH 负向:token 错误 → 拒。
    #[test]
    fn auth_callback_rejects_wrong_token() {
        let req = mk_request("/?t=wrong", None, Some("10.0.0.2:7878"));
        let resp = tungstenite::http::Response::new(());
        assert!(mk_callback("secret").on_request(&req, resp).is_err());
    }

    /// P0 CSWSH 负向:token 对但跨源 Origin(evil.com)→ 拒(CSWSH 核心场景)。
    #[test]
    fn auth_callback_rejects_cross_origin() {
        let req = mk_request("/?t=secret", Some("http://evil.com"), Some("10.0.0.2:7878"));
        let resp = tungstenite::http::Response::new(());
        assert!(mk_callback("secret").on_request(&req, resp).is_err());
    }

    /// P0 CSWSH 负向:token 对但 Host 是攻击者域名(DNS rebinding)→ 拒。
    #[test]
    fn auth_callback_rejects_rebinding_host() {
        let req = mk_request("/?t=secret", None, Some("evil.com"));
        let resp = tungstenite::http::Response::new(());
        assert!(mk_callback("secret").on_request(&req, resp).is_err());
    }

    /// SO_REUSEADDR **不破坏正常独占**:同地址端口上已有 active listener 时,再 bind 仍须失败
    /// (SO_REUSEADDR 只放行 TIME-WAIT,不放行同地址 active 监听 —— 也证用的是 REUSEADDR 而非
    /// REUSEPORT)。不依赖真 TIME-WAIT 计时,确定性、不 flaky。
    #[test]
    fn bind_ws_listener_still_excludes_active_listener() {
        let addr: SocketAddr = "127.0.0.1:0".parse().expect("解析回环临时地址");
        let first = bind_ws_listener(addr).expect("首个 bind 应成功");
        let bound = first.local_addr().expect("local_addr");
        // 第一个仍 open 监听中,占同 (addr,port) 再 bind 必须失败。
        let second = bind_ws_listener(bound);
        assert!(
            second.is_err(),
            "active listener 占用时再 bind 应失败(SO_REUSEADDR 不放行同地址 active 监听)"
        );
    }
}
