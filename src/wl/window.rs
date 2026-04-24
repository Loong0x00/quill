//! xdg-toplevel 最小窗口 + wgpu 清屏。
//!
//! 演进脉络:
//! - T-0101 用 wl_shm 填一帧白占位。
//! - T-0107 抽出 [`WindowCore`] / [`WindowEvent`] / [`handle_event`] 纯逻辑,
//!   headless 测试覆盖。
//! - T-0102 把占位绘制从 wl_shm 换成 wgpu `Surface` + `LoadOp::Clear(深蓝)`
//!   —— 单色帧走真渲染通路,为后续字形 pass 铺骨架。状态机仍由 `handle_event`
//!   驱动,只把"needs_draw 时要做什么"由 shm 换成 wgpu。
//! - T-0104(本 ticket)关闭路径优雅退出:
//!   1. compositor 发 xdg close / disconnect → `WindowHandler::request_close`
//!      驱动 [`handle_event`] 置 `core.exit`。
//!   2. `SIGINT` / `SIGTERM` → signal-hook 把同步置位 `Arc<AtomicBool>` 并写
//!      一个字节到 self-pipe,唤醒主循环的 poll。
//!   3. 主循环 `blocking_dispatch` 拆成手写 `flush + prepare_read + poll +
//!      read + dispatch_pending`,把 wayland fd 与 signal pipe fd 一起 poll,
//!      消除"信号在 flag-check 与 poll 进入之间到达"的竞态。
//!   4. 循环退出后按 INV-001 字段声明顺序正向 drop(renderer → window → conn)。
//!
//!   决策:signal-hook vs ctrlc 取舍见 `docs/adr/0003-signal-hook.md`。

use std::ffi::c_void;
use std::io::{ErrorKind, Read};
use std::os::fd::{AsFd, BorrowedFd, RawFd};
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use calloop::generic::Generic;
use calloop::{Interest, Mode, PostAction};
use rustix::event::{PollFd, PollFlags};
use rustix::io::Errno;

use crate::event_loop::Core;
use crate::pty::PtyHandle;
use smithay_client_toolkit::{
    compositor::{CompositorHandler, CompositorState},
    delegate_compositor, delegate_output, delegate_registry, delegate_xdg_shell,
    delegate_xdg_window,
    output::{OutputHandler, OutputState},
    registry::{ProvidesRegistryState, RegistryState},
    registry_handlers,
    shell::{
        xdg::{
            window::{Window, WindowConfigure, WindowDecorations, WindowHandler},
            XdgShell,
        },
        WaylandSurface,
    },
};
use wayland_client::{
    globals::registry_queue_init,
    protocol::{wl_output, wl_surface},
    Connection, EventQueue, Proxy, QueueHandle,
};

use super::render::Renderer;

const INITIAL_WIDTH: u32 = 800;
const INITIAL_HEIGHT: u32 = 600;
const APP_ID: &str = "io.github.loong0x00.quill";
const WINDOW_TITLE: &str = "quill";

/// 纯逻辑的窗口状态机核心(T-0107 抽离)。
///
/// 从 Wayland 回调里剥出四个 scalar 字段,使 headless 测试不需要真实 compositor
/// 就能驱动状态转移。活动回调持有一份 [`WindowCore`] 并通过 [`handle_event`]
/// 推进,确保测试路径和真路径是同一条。
#[derive(Debug, Clone)]
pub struct WindowCore {
    pub width: u32,
    pub height: u32,
    pub first_configure: bool,
    pub exit: bool,
    /// 尺寸变更后置位,调用方(当前是 [`State::configure`],未来是 T-0103 的
    /// swapchain 重建路径)读取后自行清零。布尔而非队列,天然把连续 resize
    /// 合并到单次脏标记。
    pub resize_dirty: bool,
}

impl WindowCore {
    pub fn new(initial_width: u32, initial_height: u32) -> Self {
        Self {
            width: initial_width,
            height: initial_height,
            first_configure: true,
            exit: false,
            resize_dirty: false,
        }
    }
}

/// 从 Wayland 层抽象出来的窗口事件。
///
/// `Configure` 的尺寸拍扁为 `Option<u32>`:compositor 未给新尺寸时对应 `None`,
/// 由 client 保留旧值;显式 0 由 client 侧防守吞掉(见 [`handle_event`])。
/// `Disconnect` 对应实际跑起来时 `blocking_dispatch` 返回 `Err` 的情形,headless
/// 测试里模拟 compositor 掉线,语义上等价于 `Close`——都应触发退出。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowEvent {
    Configure {
        new_width: Option<u32>,
        new_height: Option<u32>,
    },
    Close,
    Disconnect,
}

/// 状态机转移的副作用描述。告诉上层"要不要重画"——真回调据此决定要不要
/// 重绘占位 buffer / 重建 swapchain。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct WindowAction {
    pub needs_draw: bool,
}

/// 单步状态转移。纯逻辑、无副作用,是所有 Wayland 事件改状态的唯一入口。
///
/// 行为约定(由 `tests/state_machine.rs` 固化):
/// - 首次 Configure:吃下尺寸,清 `first_configure`,置 `resize_dirty`,要求重画。
/// - 后续 Configure 尺寸变化:更新尺寸,置 `resize_dirty`,要求重画。
/// - 后续 Configure 尺寸不变:不置脏,不重画(幂等)。
/// - 任一轴为 0:整条事件吞掉,保留旧尺寸(xdg-shell 语义里 0x0 是 client 决定,
///   到这层已经保底一次,防御性写法)。
/// - Close / Disconnect:置 `exit`。
pub fn handle_event(state: &mut WindowCore, ev: WindowEvent) -> WindowAction {
    let mut action = WindowAction::default();
    match ev {
        WindowEvent::Configure {
            new_width,
            new_height,
        } => {
            let w = new_width.unwrap_or(state.width);
            let h = new_height.unwrap_or(state.height);
            if w == 0 || h == 0 {
                return action;
            }
            if state.first_configure {
                state.width = w;
                state.height = h;
                state.first_configure = false;
                state.resize_dirty = true;
                action.needs_draw = true;
            } else if w != state.width || h != state.height {
                state.width = w;
                state.height = h;
                state.resize_dirty = true;
                action.needs_draw = true;
            }
        }
        WindowEvent::Close | WindowEvent::Disconnect => {
            state.exit = true;
        }
    }
    action
}

/// 主循环单步的显式结果。由闭包返回,让循环外壳决定是否继续。
///
/// 刻意不复用 `WindowAction`:后者描述**状态转移的副作用**(要不要重画),
/// 而 `StepResult` 描述**控制流**(要不要再跑一轮),语义正交,合在一起
/// 会诱发调用方反复 double-check 字段的反模式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StepResult {
    /// 还有活要干,跑下一轮。
    Continue,
    /// 业务告知退出(例如窗口被关闭、compositor 断开)。
    Stop,
}

/// 主循环外壳。每轮先原子检查 `should_exit` 标志(signal handler 会置位),
/// 若已置位则直接退出;否则调 `step`,按其返回决定是否继续。
///
/// 该函数不触碰 Wayland / wgpu / 任何 IO,纯逻辑 —— 便于 headless 单测
/// "信号标志被置位后主循环应立即退出"这条 T-0104 acceptance(见
/// `tests/state_machine.rs` 里对 `run_main_loop` 的注入测试)。
pub(crate) fn run_main_loop<F>(should_exit: &AtomicBool, mut step: F) -> Result<()>
where
    F: FnMut() -> Result<StepResult>,
{
    loop {
        if should_exit.load(Ordering::Relaxed) {
            return Ok(());
        }
        match step()? {
            StepResult::Continue => {}
            StepResult::Stop => return Ok(()),
        }
    }
}

/// 安装 SIGINT / SIGTERM 捕获:同时置位 `should_exit` 原子标志,并写一字节到
/// `sig_w` self-pipe。两条路各有用:
/// - `flag::register` 让 [`run_main_loop`] 每轮一个原子 load 就能观察到,
///   无需等 poll 返回。
/// - `pipe::register` 让主循环 poll 的对侧 `sig_r` 变可读,立刻从阻塞 poll
///   里醒来;消除"信号在 flag-check 与 poll 进入之间到达"的竞态。
///
/// 两个 signal 号各 `try_clone` 一份写端,drop 原写端后让 handler 自持 fd。
fn install_signal_handlers(should_exit: &Arc<AtomicBool>, sig_w: &UnixStream) -> Result<()> {
    for sig in [signal_hook::consts::SIGINT, signal_hook::consts::SIGTERM] {
        let w = sig_w
            .try_clone()
            .context("复制 signal self-pipe 写端失败")?;
        // 注册顺序 = handler 触发顺序(signal-hook 内部 chain)。按 signal-hook
        // docs(`low_level::pipe` 模块):先置 flag,再写 pipe,这样读端从 poll
        // 醒来 → drain pipe → 读 flag 的流程里,读 flag 时一定能看到已置位。
        // 本项目单线程,pipe 写完才会回用户空间,理论上顺序对错都看不到差别;
        // 但遵循 docs 可读,后续若有多线程 /  多端订阅 flag 也不踩坑。
        signal_hook::flag::register(sig, Arc::clone(should_exit))
            .with_context(|| format!("注册 signal {sig} → flag 失败"))?;
        signal_hook::low_level::pipe::register(sig, w)
            .with_context(|| format!("注册 signal {sig} → pipe 失败"))?;
    }
    Ok(())
}

/// 主循环单步:dispatch 已缓冲的 wayland 事件 → flush → 若已干净则 poll
/// (wayland_fd + sig_pipe_fd + pty_fd)。信号 pipe 可读时 drain 字节,让下一轮
/// `run_main_loop` 顶部的原子 check 观察到 flag。PTY 可读时通过
/// `pty_core.dispatch(Duration::ZERO, ...)` 让 calloop 的 Generic source 回调
/// 跑一次(T-0202:目前回调仅 `trace!("pty readable")`)。
///
/// **双 poll 是过渡设计**:wayland / signal 仍走 rustix poll(T-0104 遗留),
/// PTY 经 calloop;两个机制都看 PTY fd,所以 rustix 的 poll 醒了 →
/// `core.dispatch(ZERO)` 让 calloop 的内部 poll 也看到 ready → 触发回调。
/// T-0105 的后续 refactor 会把 wayland / signal 也迁进同一个 Core,届时本函数
/// 整体被 `EventLoop::run` 替换。
///
/// 返回 [`StepResult::Stop`] 的唯一来源:`state.core.exit`(compositor 发
/// `request_close`、compositor 断开、或 renderer 初始化失败)。
fn pump_once(
    conn: &Connection,
    event_queue: &mut EventQueue<State>,
    state: &mut State,
    sig_r: &mut UnixStream,
    pty_core: &mut Core<'_, PtyHandle>,
    pty_fd: RawFd,
) -> Result<StepResult> {
    // 1. 排空已缓冲的事件(首次 configure 就是在这里触发 init_renderer_and_draw)。
    let dispatched = event_queue
        .dispatch_pending(state)
        .context("Wayland dispatch_pending 失败")?;
    if state.core.exit {
        return Ok(StepResult::Stop);
    }

    // 2. 把 client 的 request 真推到 socket。
    conn.flush().context("Wayland flush 失败")?;

    // 刚刚 dispatch 过事件,缓冲区里可能已经有新货 —— 不 poll 直接回 top。
    // 注意:即便走了快路径,下一轮 run_main_loop 又进 pump_once,会再走完整 poll,
    // PTY readable 最多延迟一轮,Level-triggered 不会丢事件。
    if dispatched > 0 {
        return Ok(StepResult::Continue);
    }

    // 3. 准备读:None 表示别的线程已 prepare/read,我们也不该走 poll 路径,
    //    回 top 让 dispatch_pending 收尾。本项目单线程,`None` 基本对应
    //    "已有事件在 queue 里,无需再读 socket" 的情况。
    let guard = match conn.prepare_read() {
        Some(g) => g,
        None => return Ok(StepResult::Continue),
    };

    // 4. Poll wayland socket + signal pipe + PTY master。PTY 加进来是为了让单只
    //    PTY 可读也能唤醒 poll(否则会卡在等 wayland 事件的 rustix poll 里,PTY
    //    字节堆在内核缓冲)。
    let wayland_fd = guard.connection_fd();
    let sig_fd = sig_r.as_fd();
    // SAFETY: pty_fd 来自 spawn_shell 返回值的 pty.raw_fd() 缓存(PtyHandle
    // 的 master_fd 字段,由 spawn 时 as_raw_fd().ok_or_else 校验过),
    // state.pty 持有该 PtyHandle 到 run_window 结束;pump_once 调用链完全
    // 嵌套在 state 生命期内,BorrowedFd 不会在 master fd close 后残留。
    #[allow(unsafe_code)]
    let pty_borrowed: BorrowedFd<'_> = unsafe { BorrowedFd::borrow_raw(pty_fd) };
    let mut fds = [
        PollFd::new(&wayland_fd, PollFlags::IN | PollFlags::ERR),
        PollFd::new(&sig_fd, PollFlags::IN),
        PollFd::new(&pty_borrowed, PollFlags::IN),
    ];
    match rustix::event::poll(&mut fds, None) {
        Ok(_) => {}
        Err(Errno::INTR) => {
            // SIGINT handler 已跑过(置 flag + 写 pipe),poll 返回 EINTR 前
            // 就释放 guard —— 下轮 run_main_loop 顶 atomic 观察到 flag 后 Stop。
            drop(guard);
            return Ok(StepResult::Continue);
        }
        Err(e) => return Err(anyhow!("poll(wayland + signal + pty) 失败: {e}")),
    }

    // 5a. PTY 可读 → 把 &mut PtyHandle 从 state.pty 借给 calloop 的 Generic
    //      source 回调。T-0203 起 Data 从 `()` 升级为 PtyHandle,回调 `pty_read_callback`
    //      用 `pty.read()` 读字节并 tracing。Duration::ZERO = 非阻塞 tick。
    //
    //      `if let` 守卫:正常路径 state.pty 必 Some(run_window 里 spawn_shell 成功
    //      才组装 pump_once 闭包);这里防御式 `None` 跳过,不 panic 不 expect,
    //      符合 CLAUDE.md "非 main/tests 禁用 unwrap/expect"。
    if fds[2].revents().contains(PollFlags::IN) {
        if let Some(pty) = state.pty.as_mut() {
            pty_core
                .dispatch(Some(Duration::ZERO), pty)
                .context("pty_core.dispatch(pty bytes read) 失败")?;
        }
    }

    // 5b. 信号 pipe 可读 → drain 字节(防止 pipe 满、下次 handler 写入被
    //     silently drop),然后回 top 让 atomic check 观察 flag。
    if fds[1].revents().contains(PollFlags::IN) {
        drain_pipe(sig_r);
        drop(guard);
        return Ok(StepResult::Continue);
    }

    // 5c. Wayland 可读 / 报错 → 消耗 guard 真读取。出错走 err 分支。
    if fds[0].revents().intersects(PollFlags::IN | PollFlags::ERR) {
        guard.read().context("Wayland ReadEventsGuard::read 失败")?;
    } else {
        // 只 PTY 醒了(5a 已处理)或其他意外 — 释放 guard,让 wayland 源下轮再跑。
        drop(guard);
    }
    Ok(StepResult::Continue)
}

/// 单次 calloop 回调里的 PTY read buffer 大小。4 KiB 覆盖 Linux PTY master 的典型
/// 内核缓冲,一次 read 基本能吞完 bash prompt / ANSI escape;满了就循环再读一次。
const PTY_READ_BUF: usize = 4096;

/// calloop Generic source 的回调,从 PTY master fd 读字节并 `tracing::trace!`。
///
/// T-0203 实装:取代 T-0202 的 trace-only + drain-discard stopgap。循环读到
/// `WouldBlock`(master 被 O_NONBLOCK 设过,无更多数据)才返回,每批字节用
/// `escape_ascii` 渲染成可读 trace。错误处理:
/// - `WouldBlock` → 正常,回到 calloop 等下一次 readable
/// - `Interrupted` → 被 signal 打断,重试本轮 read
/// - `Ok(0)` / 其它 IO 错(常见 EIO:slave 端关闭)→ `tracing::warn!`。**本 ticket 不退
///   出主循环**(退出检测由 T-0205 的 `try_wait` + child-exit 路径负责),返回
///   `Continue` 让 calloop 继续。
fn pty_read_callback(
    _readiness: calloop::Readiness,
    _fd: &mut calloop::generic::NoIoDrop<BorrowedFd<'static>>,
    pty: &mut PtyHandle,
) -> std::io::Result<PostAction> {
    // INV-009 sanity check:master fd 必须 O_NONBLOCK。由 T-0201 的 `spawn_program`
    // 在构造时 fcntl 一次设好,本 ticket **不重复** F_SETFL(会覆盖其它 flag 破
    // 坏不变式)。debug build panic 拦回归;release build 0 开销。
    // SAFETY: pty 持有 master fd,fd 此刻肯定有效;fcntl(F_GETFL) 是只读操作,
    // 不改资源所有权、不与 INV-008 drop 序交互。
    #[cfg(debug_assertions)]
    {
        #[allow(unsafe_code)]
        let flags = unsafe { libc::fcntl(pty.raw_fd(), libc::F_GETFL) };
        debug_assert!(
            flags >= 0 && (flags & libc::O_NONBLOCK) != 0,
            "INV-009 破坏:master fd 未置 O_NONBLOCK(T-0201 的 set_nonblocking 应已设;\
             fcntl 返回 flags={flags:#x},errno={})",
            std::io::Error::last_os_error()
        );
    }

    let mut buf = [0u8; PTY_READ_BUF];
    loop {
        match pty.read(&mut buf) {
            Ok(0) => {
                // EOF:slave 端关闭。常见:子进程退出后 kernel 在 drain 完 buffer 后
                // 给出 0 字节 read。本 ticket 仅 warn 不改控制流 —— 退出由 T-0205
                // 的子进程 try_wait + should_exit 路径处理。
                tracing::warn!(
                    target: "quill::pty",
                    "master fd 读到 EOF(0 字节);T-0205 的 try_wait 路径会处理子进程退出"
                );
                return Ok(PostAction::Continue);
            }
            Ok(n) => {
                // `escape_ascii` 是 u8 稳定方法(Rust 1.60+),把不可打印字符变成
                // \xNN / \e / \n 形式,trace 里可一眼看出终端转义序列而不污染 log。
                let preview = buf[..n].escape_ascii().to_string();
                tracing::trace!(
                    target: "quill::pty",
                    n,
                    bytes = %preview,
                    "pty bytes"
                );
                // 读满 buffer:肯定还有更多字节,继续循环。
                // 读不满:可能 drained;继续循环交给下一轮 read 去 WouldBlock 兜底,
                //         不用提前 break —— Linux PTY 把小块 write 合并的概率高,
                //         多一次 syscall 换一次"确定性清空"。
                continue;
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                return Ok(PostAction::Continue);
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => {
                // 常见 EIO:slave 关闭后继续读。也可能 EBADF(fd 意外关了)等,
                // 这里不区分 —— 都归为"读不动了",warn + continue。T-0205 的
                // try_wait 会发现子进程退出,触发主循环退出。
                tracing::warn!(
                    target: "quill::pty",
                    error = %e,
                    kind = ?e.kind(),
                    "pty read 报错(T-0205 的 try_wait 路径会处理)"
                );
                return Ok(PostAction::Continue);
            }
        }
    }
}

/// 非阻塞地把 signal pipe 里堆积的字节读干净。pipe 已被 `set_nonblocking(true)`,
/// 返回 `WouldBlock` 即停;`Interrupted` 继续重试;0 字节(EOF)也停。
fn drain_pipe(sig_r: &mut UnixStream) {
    let mut buf = [0u8; 32];
    loop {
        match sig_r.read(&mut buf) {
            Ok(0) => return,
            Ok(_) => continue,
            Err(ref e) if e.kind() == ErrorKind::WouldBlock => return,
            Err(ref e) if e.kind() == ErrorKind::Interrupted => continue,
            // 其它错误极罕见(fd 被关之类),吞掉 —— 下轮 atomic flag 仍会让
            // run_main_loop 干净退出。不抛 error 是因为信号路径本身就是"退出
            // 在即"的节奏,没有可挽回行为。
            Err(_) => return,
        }
    }
}

/// 启动 Wayland 连接、创建 xdg toplevel、首次 configure 后建 wgpu renderer、
/// 安装 SIGINT / SIGTERM 捕获,跑主循环直到窗口被关闭或信号到达。
///
/// 退出路径(ticket T-0104 acceptance):
/// - 用户点关闭十字 → `WindowHandler::request_close` → `core.exit = true` →
///   下一轮 [`pump_once`] 返回 [`StepResult::Stop`]
/// - `SIGINT` / `SIGTERM` → signal handler 置 `should_exit` + 写 pipe 唤醒 poll →
///   下一轮 [`run_main_loop`] 顶部 atomic check 退出
/// - compositor 断开(读 socket 返回 IO 错)→ err 从 [`pump_once`] 抛回
///
/// 退出后按 INV-001 声明顺序(renderer → window → conn)正向 drop,保证 wgpu
/// surface 先放掉 wl_surface 裸指针再关连接,不给 compositor 留 "client didn't
/// release surface" 告警。
pub fn run_window() -> Result<()> {
    let conn = Connection::connect_to_env()
        .context("连接 Wayland compositor 失败(是否在 Wayland session 下?)")?;
    let (globals, mut event_queue) =
        registry_queue_init(&conn).context("初始化 Wayland registry 失败")?;
    let qh = event_queue.handle();

    let compositor =
        CompositorState::bind(&globals, &qh).map_err(|e| anyhow!("wl_compositor 不可用: {e}"))?;
    let xdg_shell = XdgShell::bind(&globals, &qh).map_err(|e| anyhow!("xdg_shell 不可用: {e}"))?;

    let surface = compositor.create_surface(&qh);
    let window = xdg_shell.create_window(surface, WindowDecorations::RequestServer, &qh);
    window.set_title(WINDOW_TITLE);
    window.set_app_id(APP_ID);
    window.set_min_size(Some((INITIAL_WIDTH, INITIAL_HEIGHT)));

    // Implementation note: 第一次 configure 前只能 commit 空 surface(无 buffer 附加),
    // 这是 xdg-shell 的 map 请求语义。
    window.commit();

    // State 字段顺序固化 INV-001(renderer→window→conn)+ pty 放最后(T-0202 Lead + 审码)。
    // 初始化时 pty 为 None,后面拿到 spawn_shell 结果再填 Some。
    let mut state = State {
        registry_state: RegistryState::new(&globals),
        output_state: OutputState::new(&globals, &qh),
        renderer: None,
        window,
        conn: conn.clone(),
        core: WindowCore::new(INITIAL_WIDTH, INITIAL_HEIGHT),
        pty: None,
    };

    // Signal self-pipe + handlers 装在一起:
    // - `should_exit` 传入 [`run_main_loop`],signal-hook flag 端置位。
    // - `sig_r` 留给 [`pump_once`] poll;`sig_w` handler dup 后丢弃,防止 read
    //   端永远收到 EOF。
    let should_exit = Arc::new(AtomicBool::new(false));
    let (mut sig_r, sig_w) = UnixStream::pair().context("创建 signal self-pipe 失败")?;
    sig_r
        .set_nonblocking(true)
        .context("signal pipe read 端设非阻塞失败")?;
    install_signal_handlers(&should_exit, &sig_w)?;
    drop(sig_w); // handler 各自持 dup,原件不再需要

    // T-0202: spawn login shell + 把 master fd 注册进 calloop Core(INV-005:所有
    // IO fd 同一 EventLoop)。初始尺寸 80x24 按 ticket scope 写死;Phase 3 T-0306
    // 才接 Wayland configure → cell 尺寸换算。
    let pty = PtyHandle::spawn_shell(80, 24).context("PtyHandle::spawn_shell(80, 24) 失败")?;
    let pty_fd = pty.raw_fd();
    state.pty = Some(pty);

    // T-0203:Core 的 Data 从 `()` 升级为 `PtyHandle` —— 回调现在真读字节
    // 需要 `PtyHandle::read` 的出口,计数转给调用方。Lead 2026-04-25 拍板走
    // 方案 A(独立 Data 升级),不把 wayland / signal 也合并进来(那是 T-0105
    // refactor 的 scope)。dispatch 时从 state.pty 拿 `&mut PtyHandle` 传进去。
    let mut pty_core: Core<'_, PtyHandle> = Core::new().context("calloop Core::new")?;
    // SAFETY: pty_fd 来自 spawn_shell 返回值的 pty.raw_fd() 缓存(PtyHandle
    // 的 master_fd 字段,spawn 时 as_raw_fd().ok_or_else 校验 Some 一次),
    // state.pty 持有该 PtyHandle 到 run_window 结束;BorrowedFd 的生命周期
    // (通过 borrow_raw 擦成 'static)被闭包捕获到 Source 里,但底层 fd 的
    // 实际有效期 ≥ pty_core 与 event_loop 的生命周期,满足合约。
    #[allow(unsafe_code)]
    let pty_borrowed: BorrowedFd<'static> = unsafe { BorrowedFd::borrow_raw(pty_fd) };
    pty_core
        .handle()
        .insert_source(
            Generic::new(pty_borrowed, Interest::READ, Mode::Level),
            pty_read_callback,
        )
        .map_err(|e| anyhow!("calloop insert_source(pty master fd) 失败: {e}"))?;

    tracing::info!(
        width = INITIAL_WIDTH,
        height = INITIAL_HEIGHT,
        "quill 窗口已请求创建"
    );

    // 主循环:闭包借 &mut event_queue / &mut state / &conn / &mut sig_r /
    // &mut pty_core(互不冲突的字段借用)。run_main_loop 只看 should_exit 原子,
    // 不触碰 wayland / pty 资源,便于单测。
    run_main_loop(&should_exit, || {
        pump_once(
            &conn,
            &mut event_queue,
            &mut state,
            &mut sig_r,
            &mut pty_core,
            pty_fd,
        )
    })?;

    tracing::info!("窗口关闭,退出事件循环");
    Ok(())
}

struct State {
    registry_state: RegistryState,
    output_state: OutputState,
    // Drop 顺序敏感:`renderer` 持有 wgpu `Surface`,后者内部保留了 wl_surface 裸指针。
    // Rust 按字段声明顺序**正向**析构 —— 第一个声明的字段先 drop。所以 renderer
    // 必须排在 `window` / `conn` 之前,这样析构顺序是 renderer → window → conn,
    // renderer 先释放 GPU 资源,窗口与连接才关闭,指针在 Renderer 生命周期内
    // 保持有效。若把 renderer 挪到 window/conn 后面会立刻 UB。见 docs/invariants.md
    // INV-001。
    renderer: Option<Renderer>,
    window: Window,
    conn: Connection,
    core: WindowCore,
    // `pty` **位于 State 最后一位**(Lead + 审码 2026-04-25 拍板,见 T-0202 ticket):
    // - PTY 持 Linux fd + 子进程句柄,与 wl / wgpu 资源生命周期正交,放最后避免
    //   跟 INV-001 的 renderer→window→conn 链条耦合,不需要新建 INV。
    // - 保证 wl / wgpu drop 先跑完,再 drop pty;PtyHandle 自身按 INV-008
    //   (reader → master → child)正向 drop,master 关闭时 slave 端 SIGHUP 已
    //   无风险打扰 wl 回调(wl 侧早没了)。
    pty: Option<PtyHandle>,
}

impl State {
    /// 从 Connection / WlSurface 提取 libwayland 裸指针,初始化 wgpu Renderer,
    /// 渲染一帧清屏。指针有效性依赖 `wayland-backend` 的 `client_system` feature
    /// (在 `Cargo.toml` 中显式启用),否则 `as_ptr()` 会返回 null,构造会报错返回。
    fn init_renderer_and_draw(&mut self) -> Result<()> {
        let display_ptr = self.conn.backend().display_ptr() as *mut c_void;
        if display_ptr.is_null() {
            return Err(anyhow!(
                "Connection::backend().display_ptr() == null —— \
                 wayland-backend 的 `client_system` feature 未启用?"
            ));
        }
        let surface_id = self.window.wl_surface().id();
        let surface_ptr = surface_id.as_ptr() as *mut c_void;
        if surface_ptr.is_null() {
            return Err(anyhow!(
                "wl_surface ObjectId::as_ptr() == null —— \
                 wayland-backend 的 `client_system` feature 未启用?"
            ));
        }

        // SAFETY: display_ptr / surface_ptr 来自本进程活跃的 Connection 与 Window。
        // Window (及其 WlSurface) 与 Connection 都被 State 持有;`renderer` 字段
        // 声明位置在 `window` / `conn` 之前,Rust 按声明顺序**正向**析构 →
        // renderer(第 3 个)先于 window(第 4)/ conn(第 5)被 drop,两枚指针
        // 在 Renderer 生命周期内始终指向活对象。见 docs/invariants.md。
        #[allow(unsafe_code)]
        let renderer =
            unsafe { Renderer::new(display_ptr, surface_ptr, self.core.width, self.core.height)? };
        self.renderer = Some(renderer);

        if let Some(r) = self.renderer.as_mut() {
            r.render().context("首帧渲染失败")?;
        }
        Ok(())
    }
}

impl CompositorHandler for State {
    fn scale_factor_changed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _new_factor: i32,
    ) {
    }

    fn transform_changed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _new_transform: wl_output::Transform,
    ) {
    }

    fn frame(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _time: u32,
    ) {
    }

    fn surface_enter(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _output: &wl_output::WlOutput,
    ) {
    }

    fn surface_leave(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _output: &wl_output::WlOutput,
    ) {
    }
}

impl OutputHandler for State {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.output_state
    }

    fn new_output(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _output: wl_output::WlOutput,
    ) {
    }

    fn update_output(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _output: wl_output::WlOutput,
    ) {
    }

    fn output_destroyed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _output: wl_output::WlOutput,
    ) {
    }
}

impl WindowHandler for State {
    fn request_close(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &Window) {
        tracing::info!("compositor 请求关闭窗口");
        let _ = handle_event(&mut self.core, WindowEvent::Close);
    }

    fn configure(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _window: &Window,
        configure: WindowConfigure,
        _serial: u32,
    ) {
        let new_w = configure.new_size.0.map(|v| v.get());
        let new_h = configure.new_size.1.map(|v| v.get());
        tracing::debug!(
            ?new_w,
            ?new_h,
            first = self.core.first_configure,
            "configure"
        );

        let was_first = self.core.first_configure;
        let action = handle_event(
            &mut self.core,
            WindowEvent::Configure {
                new_width: new_w,
                new_height: new_h,
            },
        );

        // resize 重配置是 T-0103 的范围:本 ticket 仅在首次 configure 建 renderer
        // 并画一次;之后 `WindowCore::resize_dirty` 的消费者留给 T-0103。
        if was_first && action.needs_draw {
            if let Err(err) = self.init_renderer_and_draw() {
                tracing::error!(?err, "wgpu renderer 初始化或首帧失败");
                self.core.exit = true;
            } else {
                self.core.resize_dirty = false;
            }
        }
    }
}

impl ProvidesRegistryState for State {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }
    registry_handlers![OutputState];
}

delegate_compositor!(State);
delegate_output!(State);
delegate_xdg_shell!(State);
delegate_xdg_window!(State);
delegate_registry!(State);

#[cfg(test)]
mod tests {
    use super::*;

    /// Smoke test:窗口模块只对外导出 [`run_window`],签名固定为 `fn() -> Result<()>`。
    /// 这里通过函数指针绑定把 contract 固化在编译期,防止后续重构误改签名(比如加参数或
    /// 返回 ())。实际 Wayland 连接的 runtime 行为依赖 compositor,留给集成测试与 soak。
    #[test]
    fn smoke_run_window_signature_is_stable() {
        let f: fn() -> Result<()> = run_window;
        // 仅保留引用,避免 dead_code;不调用 f(会阻塞事件循环)。
        let _ = &f;
    }

    #[test]
    fn smoke_initial_size_is_nonzero() {
        // 防止以后"顺手"把初始尺寸改成 0x0(某些 compositor 对 0 尺寸行为未定义)。
        const _: () = assert!(INITIAL_WIDTH >= 1);
        const _: () = assert!(INITIAL_HEIGHT >= 1);
    }

    #[test]
    fn smoke_app_id_and_title_are_set() {
        // 固化 ticket acceptance 里的 "标题为 quill" 要求,防漂移。
        assert_eq!(WINDOW_TITLE, "quill");
        assert!(!APP_ID.is_empty());
        assert!(APP_ID.contains('.'), "app_id 应为反向域名格式");
    }

    // ---------- T-0104 run_main_loop 注入式单测 ----------
    // ticket acceptance:"should_exit 标志被置位后主循环应立即退出"。这里通过给
    // [`run_main_loop`] 注入一个假 step 闭包,把真正的 wayland / wgpu / signal
    // 依赖全绕开,单纯验证控制流。每个 case 用不同的 step 行为覆盖一条路径。

    #[test]
    fn run_main_loop_exits_immediately_when_flag_already_set() {
        // flag 在进入前就已经 true —— step 绝不该被调一次,避免做任何 IO。
        let flag = AtomicBool::new(true);
        let mut called = 0u32;
        let result = run_main_loop(&flag, || {
            called += 1;
            Ok(StepResult::Continue)
        });
        assert!(result.is_ok());
        assert_eq!(called, 0, "flag 已置位时不应进入 step");
    }

    #[test]
    fn run_main_loop_exits_after_signal_raises_flag_mid_run() {
        // 模拟 signal handler:step 跑到第 3 次把 flag 置位,第 4 次进入
        // run_main_loop 循环顶时原子 check 命中,应干净退出。
        let flag = Arc::new(AtomicBool::new(false));
        let flag_step = Arc::clone(&flag);
        let mut iters = 0u32;
        run_main_loop(&flag, || {
            iters += 1;
            if iters == 3 {
                flag_step.store(true, Ordering::Relaxed);
            }
            Ok(StepResult::Continue)
        })
        .expect("run_main_loop 应干净返回");
        assert_eq!(
            iters, 3,
            "第 3 次 step 置 flag;第 4 次不该进来,循环顶 atomic check 拦住"
        );
    }

    #[test]
    fn run_main_loop_exits_when_step_returns_stop() {
        // compositor 发 close → WindowHandler 置 state.core.exit → pump_once
        // 返回 Stop 这条路径。flag 永远没被置位。
        let flag = AtomicBool::new(false);
        let mut iters = 0u32;
        run_main_loop(&flag, || {
            iters += 1;
            if iters >= 2 {
                Ok(StepResult::Stop)
            } else {
                Ok(StepResult::Continue)
            }
        })
        .expect("run_main_loop 应干净返回");
        assert_eq!(iters, 2);
        assert!(!flag.load(Ordering::Relaxed), "stop 路径不依赖 flag");
    }

    #[test]
    fn run_main_loop_propagates_step_error_verbatim() {
        // pump_once 内 anyhow 错误必须透传,否则 IO 失败被吞会让上层误以为干净退出。
        let flag = AtomicBool::new(false);
        let err =
            run_main_loop(&flag, || Err::<StepResult, _>(anyhow!("boom"))).expect_err("错误应上抛");
        assert!(
            err.to_string().contains("boom"),
            "错误消息应保留 (got: {err})"
        );
    }

    #[test]
    fn run_main_loop_flag_atomic_can_be_raised_from_another_thread() {
        // signal handler 实际上跑在同线程(POSIX signal 语义),但 signal-hook
        // 的 flag::register 承诺 SeqCst 原子写,我们的 loader 用 Relaxed 是因为
        // step 每轮都会 yield 让出观察机会。这里用多线程冒烟:另一个 thread 置
        // flag,主 thread 在有限 step 内观察到并退出,保证 Relaxed load 至少能
        // 跨 loom-free 的 x86/ARM 常见 memory model 看到变化。
        use std::thread;
        use std::time::Duration;

        let flag = Arc::new(AtomicBool::new(false));
        let flag_thread = Arc::clone(&flag);
        let handle = thread::spawn(move || {
            thread::sleep(Duration::from_millis(10));
            flag_thread.store(true, Ordering::SeqCst);
        });

        let mut iters = 0u32;
        run_main_loop(&flag, || {
            iters += 1;
            // 让出一点时间给 wake thread,但不要无限忙转。
            thread::sleep(Duration::from_millis(1));
            if iters > 1000 {
                // 保险丝:万一 flag 没被看到也不死锁测试。
                return Ok(StepResult::Stop);
            }
            Ok(StepResult::Continue)
        })
        .expect("应干净返回");
        handle.join().expect("wake thread 应干净结束");

        assert!(flag.load(Ordering::SeqCst), "wake thread 最终置 flag");
        assert!(
            iters <= 1000,
            "应被 atomic flag 拦下,而非走保险丝 (iters={iters})"
        );
    }

    #[test]
    fn drain_pipe_consumes_all_bytes() {
        // drain_pipe 的两条退出路径:读到 0(EOF,本测试通过 drop writer 触发)
        // 与 WouldBlock(空 pipe 非阻塞 read)。两条都该让 pipe 净空。
        use std::io::Write;

        let (mut r, mut w) = UnixStream::pair().expect("UnixStream pair");
        r.set_nonblocking(true).expect("set_nonblocking");

        // 写入一些字节,然后 drop writer → 再 drain 应读到 0 字节后返回。
        w.write_all(b"wakeupX3").expect("write");
        drop(w);

        drain_pipe(&mut r);

        // 二次 drain 应立刻 return(EOF / WouldBlock),不卡住。
        drain_pipe(&mut r);
    }

    #[test]
    fn drain_pipe_returns_on_wouldblock_without_writer_closed() {
        // writer 还开着、pipe 没数据 —— 非阻塞 read 立即 WouldBlock,drain 应返回。
        let (mut r, _w) = UnixStream::pair().expect("UnixStream pair");
        r.set_nonblocking(true).expect("set_nonblocking");
        drain_pipe(&mut r); // 不卡住即过
    }
}
