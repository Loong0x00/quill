//! xdg-toplevel 最小窗口 + wgpu 清屏 + 单 calloop 主循环。
//!
//! 演进脉络:
//! - T-0101 用 wl_shm 填一帧白占位。
//! - T-0107 抽出 [`WindowCore`] / [`WindowEvent`] / [`handle_event`] 纯逻辑。
//! - T-0102 wgpu surface + `LoadOp::Clear(深蓝)`,单色帧走真渲染通路。
//! - T-0104 关闭路径优雅退出:xdg close / SIGINT / SIGTERM 统一退出位。**最初**
//!   用手写 `rustix::event::poll` + signal-hook self-pipe(ADR 0003)—— **T-0108
//!   已推翻**,见下。
//! - T-0202..T-0206 PTY 接入(`PtyHandle` 五方法、calloop 一部分)。
//! - **T-0108(当前)事件循环统一**:TD-001 / TD-005 / TD-006 一次清掉。wayland fd、
//!   signal、pty fd 全部注册到同一 `calloop::EventLoop<LoopData>`,真正落实
//!   INV-005 "所有 IO fd 同一调度器"。三条 source:
//!   1. `calloop::generic::Generic` 包 wayland fd,callback `prepare_read → read →
//!      dispatch_pending → flush`
//!   2. `calloop::signals::Signals` 包 SIGINT/SIGTERM(signalfd 路径,消除
//!      TD-006 的 nanos 竞态)
//!   3. `Generic` 包 pty master fd,callback `pty_read_tick` 拿 `&mut LoopData`
//!      读字节 / 触发退出
//!
//!   不再有手写 rustix poll、signal-hook、self-pipe、`Arc<AtomicBool>`。退出统一
//!   走 `LoopSignal::stop()`。
//! - T-0301(同分支后续 commit)接入 `alacritty_terminal::Term`,pty callback
//!   里把字节 `term.advance(...)` 喂进去。渲染还是留给 Phase 3 后续 ticket。
//!
//! 关键不变式仍守:
//! - INV-001 `State` 字段声明顺序决定 wl 指针生命周期(renderer→window→conn)
//! - INV-005 所有 IO fd 同一 calloop::EventLoop(本 ticket 真做到了)
//! - INV-008 `PtyHandle` 内部 drop 序
//! - INV-009 master fd O_NONBLOCK

use std::ffi::c_void;
use std::os::fd::{AsRawFd, BorrowedFd};

use anyhow::{anyhow, Context, Result};
use calloop::generic::Generic;
use calloop::signals::{Signal, Signals};
use calloop::{EventLoop, Interest, LoopSignal, Mode, PostAction};

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
use wayland_backend::client::WaylandError;
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

/// 把 wayland `EventQueue<State>`、业务 `State`、calloop `LoopSignal` 三个
/// 运行时对象捆成一个结构,让 `calloop::EventLoop<'_, LoopData>` 一把 own。
/// 回调签名得到 `&mut LoopData`,在里面做字段级 split borrow:
/// - wayland source 回调:需要同时 `&mut event_queue` + `&mut state` 跑
///   `dispatch_pending`
/// - pty source 回调:`&mut state.pty` + `&loop_signal`(EOF 触发 stop)
/// - signal source 回调:只需 `&loop_signal`
///
/// `loop_signal` 放这里而非全局,因为业务退出(compositor close、shell 死、
/// SIGINT/SIGTERM、read 错)有多条触发路径,都从 `&mut LoopData` 拿同一把
/// 停机把手,不重复创建。
struct LoopData {
    event_queue: EventQueue<State>,
    state: State,
    loop_signal: LoopSignal,
}

// T-0108 删除:`run_main_loop` / `StepResult` / `install_signal_handlers` /
// `pump_once` / `drain_pipe` —— wayland / signal / pty 三条 source 现在都由
// `calloop::EventLoop` 统一 poll,不再手写 rustix poll 循环 + signal-hook
// self-pipe。退出统一走 `LoopSignal::stop()` 一个出口。TD-001 / TD-005 / TD-006
// 随之归档。

// T-0108 删除 pump_once —— 双 poll 过渡设计废弃,wayland/signal/pty 各自的
// source 现在都挂在同一 EventLoop 上,由 calloop 内部 poll 统一调度。参见
// `drive_wayland` / `drive_pty` / signal handler 闭包。

/// 单次 calloop 回调里的 PTY read buffer 大小。4 KiB 覆盖 Linux PTY master 的典型
/// 内核缓冲,一次 read 基本能吞完 bash prompt / ANSI escape;满了就循环再读一次。
const PTY_READ_BUF: usize = 4096;

/// `pty_readable_action` 的返回:告诉 [`pty_read_tick`] 下一步该做什么。
///
/// 抽成显式 enum(而非散落 `if` 分支)是 T-0107 / T-0205 的抽状态机路子 ——
/// 纯逻辑决策能用 headless 单测覆盖,避免"PTY 真死了但主循环没退出"这类
/// 回归(ticket T-0205 acceptance 显式要求单测这条路径)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PtyAction {
    /// `Ok(n > 0)`:读到字节,trace 后继续循环 read。
    /// `Err(EINTR)`:被 signal 打断,重试本轮 read(也走 Continue)。
    ContinueReading,
    /// `Err(WouldBlock / EAGAIN)`:暂时没更多数据 —— 跳出循环,回 calloop
    /// 等下一次 readable。
    ReturnContinue,
    /// `Ok(0)` EOF / `Err(EIO)` slave 关闭 / 其它未知 IO 错:shell 死了,
    /// 主循环退出路径要跑 —— 触发 `should_exit` + `try_wait` 收尸。
    RequestExit,
}

/// 纯逻辑决策:看一次 `PtyHandle::read` 的结果,算出 [`PtyAction`]。
///
/// **无副作用、不碰 IO**,便于 headless 单测走所有分支。真 trace / reap /
/// set-flag 都放在 [`pty_read_tick`] 的调用方里,按 action 分派。
///
/// T-0205 acceptance 对这条路径的要求:"read 返回 0 字节 → 触发 should_exit"
/// —— 对应本函数 `Ok(0) -> PtyAction::RequestExit`,配合 `pty_read_tick` 里
/// `RequestExit → should_exit.store(true)` 走完。
pub(crate) fn pty_readable_action(result: &std::io::Result<usize>) -> PtyAction {
    match result {
        // Ok(0) = EOF。Linux 在 slave 关闭后通常给 EIO,但 BSD / macOS 或某些
        // 路径会给 EOF;两者语义等价(shell 死了),都走退出路径。
        Ok(0) => PtyAction::RequestExit,
        Ok(_) => PtyAction::ContinueReading,
        Err(e) => {
            // 优先看 ErrorKind 的语义分类 —— std 帮我们把 errno 翻译成
            // 跨平台 Kind,命中 WouldBlock / Interrupted 走快路径。
            match e.kind() {
                std::io::ErrorKind::WouldBlock => return PtyAction::ReturnContinue,
                std::io::ErrorKind::Interrupted => return PtyAction::ContinueReading,
                _ => {}
            }
            // ErrorKind 没匹配到(多见于 Kind::Uncategorized):再看 raw errno。
            // EAGAIN 在 Linux 上 == EWOULDBLOCK(值都是 11),理论上 Kind 应是
            // WouldBlock;防御性再匹配一次,不依赖具体 std 版本。
            match e.raw_os_error() {
                Some(errno) if errno == libc::EAGAIN || errno == libc::EWOULDBLOCK => {
                    PtyAction::ReturnContinue
                }
                Some(errno) if errno == libc::EINTR => PtyAction::ContinueReading,
                // EIO:slave 已关闭或 tty 设备错。T-0205 acceptance 明确要求
                // 视同 EOF 走退出路径。
                Some(errno) if errno == libc::EIO => PtyAction::RequestExit,
                // 其它未知 IO 错(EBADF / EFAULT 等):按"读不动了"处理 —— 保守
                // 触发退出,避免卡 main loop 死等一个永远不再 ready 的 fd。
                _ => PtyAction::RequestExit,
            }
        }
    }
}

/// calloop Generic source 每次 readable 时跑一圈:循环 `pty.read` → 分派
/// [`PtyAction`] → 处理。一个 tick 要么读尽(ReturnContinue)要么触发退出
/// (RequestExit)。
///
/// T-0203 实装 trace。T-0205 接入 `try_wait`(EOF / EIO 时)。**T-0108 改签名**:
/// 原来走 `&AtomicBool` 置位 T-0104 的 `Arc<AtomicBool>` 走主循环顶的
/// `run_main_loop` 检查;现在 `Arc<AtomicBool>` 整个 scheme 没了,退出统一经
/// `LoopSignal::stop()` —— 本函数收 `&LoopSignal`(由外层 closure `&data.loop_signal`
/// 传入)。行为等价:`stop()` 后 `EventLoop::run` 当前或下一次 dispatch 结束返回。
///
/// EOF / EIO 分支仍是 `pty.try_wait()` 尝试 reap 一下并 `tracing::info!` 一个
/// exit_code;`Ok(None)` (race) 不 sleep 重试,接受延迟,zombie 由 `PtyHandle::Drop`
/// / init-adopt 兜底。
fn pty_read_tick(pty: &mut PtyHandle, loop_signal: &LoopSignal) -> std::io::Result<PostAction> {
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
        let result = pty.read(&mut buf);
        match pty_readable_action(&result) {
            PtyAction::ContinueReading => {
                // Ok(n>0) 路径:trace 字节。Err(EINTR) 路径:不 trace,沉默重试。
                if let Ok(n) = result {
                    debug_assert!(
                        n > 0,
                        "pty_readable_action::ContinueReading 分支不应对应 Ok(0)"
                    );
                    let preview = buf[..n].escape_ascii().to_string();
                    tracing::trace!(
                        target: "quill::pty",
                        n,
                        bytes = %preview,
                        "pty bytes"
                    );
                }
                continue;
            }
            PtyAction::ReturnContinue => return Ok(PostAction::Continue),
            PtyAction::RequestExit => {
                // 记一次退出原因,方便日志排障(WARN 级是因为 shell 死是不常
                // 见事件,不该被 trace 淹)。
                match &result {
                    Ok(0) => tracing::info!(
                        target: "quill::pty",
                        "master EOF:slave 端关闭,shell 退出"
                    ),
                    Err(e) => tracing::info!(
                        target: "quill::pty",
                        error = %e,
                        errno = ?e.raw_os_error(),
                        "master read IO 错:shell 退出"
                    ),
                    Ok(n) => tracing::warn!(
                        target: "quill::pty",
                        n,
                        "pty_readable_action::RequestExit 对应 Ok(n>0),实现 bug"
                    ),
                }
                // 非阻塞收尸。Ok(None) 接受:race 下 try_wait 可能还没见到 exit
                // status,主循环反正要退,PtyHandle 的 Drop 路径再配合 init
                // adopt zombie 兜底。
                match pty.try_wait() {
                    Ok(Some(code)) => {
                        tracing::info!(
                            target: "quill::pty",
                            exit_code = code,
                            "shell exited"
                        );
                    }
                    Ok(None) => {
                        tracing::info!(
                            target: "quill::pty",
                            "shell 正在退出,try_wait 尚未见到 exit status(init 兜底收养)"
                        );
                    }
                    Err(e) => {
                        tracing::warn!(
                            target: "quill::pty",
                            error = ?e,
                            "try_wait 失败 —— 仍触发 should_exit"
                        );
                    }
                }
                // 触发主循环退出:统一路径是 `calloop::LoopSignal::stop()`,
                // run_window 尾 `event_loop.run` 见到 stop 信号后当前 dispatch
                // 结束就返回,进入 state Drop(INV-001 正向:renderer → window
                // → conn → core → pty)。不建第二套机制。
                loop_signal.stop();
                return Ok(PostAction::Continue);
            }
        }
    }
}

// T-0108 删除 drain_pipe —— self-pipe signal 机制废弃,calloop::signals::Signals
// 走 signalfd 由 calloop 内部 source 直接读。

/// calloop wayland source 的回调:**prepare_read → read → dispatch_pending →
/// flush** 四步拆清楚,之间有若干退出 / 错误分支要处理。
///
/// T-0108 把这段从 `pump_once` 的 rustix poll 手写循环搬到 calloop `Generic`
/// source 的回调里,主循环的调度权交给 `EventLoop::run`。
///
/// 关键点:
/// - `prepare_read` 返回 `None` 不 panic,表示 queue 里已有事件或别的线程
///   在读(本项目单线程,`None` = 已有事件缓冲,直接 dispatch_pending 消化)
/// - `guard.read()` 的 `WouldBlock` 不是错:level-triggered fd 刚被 epoll 唤
///   醒,但 socket 真正 read 时可能已被上一轮消化干净;跳过即可
/// - `dispatch_pending` 的错走上抛(io::Error 转换);其他正常路径返回 Continue
/// - `state.core.exit` 由 `WindowHandler::request_close` 置(compositor 发
///   xdg close)—— dispatch 完后检查一下,`true` 则 `loop_signal.stop()`
/// - 结尾 `conn.flush()` 把我们响应 configure 产生的 ack_configure / surface
///   commit 真推到 compositor
fn drive_wayland(data: &mut LoopData) -> std::io::Result<PostAction> {
    // Step 1:如果当前 queue 里已经有缓冲事件,prepare_read 返回 None,跳过 read
    // 直接 dispatch。否则拿 guard 读 socket。
    if let Some(guard) = data.state.conn.prepare_read() {
        match guard.read() {
            Ok(_) => {}
            Err(WaylandError::Io(e)) if e.kind() == std::io::ErrorKind::WouldBlock => {
                // level-triggered fd 刚唤醒,但数据已经被上一轮 read 吃完 ——
                // 正常情况。继续 dispatch_pending 看有没有事情可做。
            }
            Err(e) => {
                return Err(std::io::Error::other(format!("wayland read: {e}")));
            }
        }
    }

    // Step 2:split borrow event_queue + state,跑 Dispatch 回调。handle_event /
    // WindowHandler / CompositorHandler 们都在这里 fire。
    let LoopData {
        event_queue, state, ..
    } = &mut *data;
    if let Err(e) = event_queue.dispatch_pending(state) {
        return Err(std::io::Error::other(format!(
            "wayland dispatch_pending: {e}"
        )));
    }

    // Step 3:若 xdg close 或其它路径置了 core.exit,触发停机。保持与 signal /
    // PTY EOF 同一个出口(loop_signal.stop()),不建第二套标志。
    if data.state.core.exit {
        data.loop_signal.stop();
    }

    // Step 4:把 ack_configure / surface commit 这些响应真推给 compositor。
    if let Err(e) = data.state.conn.flush() {
        return Err(std::io::Error::other(format!("wayland flush: {e}")));
    }

    Ok(PostAction::Continue)
}

/// 启动 Wayland 连接、创建 xdg toplevel、spawn login shell、把 wayland / signal
/// / pty 三条 source 注册到同一 `calloop::EventLoop`,跑主循环直到有任一路径
/// 触发 `LoopSignal::stop()`。
///
/// 退出路径(T-0108 统一后):
/// - 用户点关闭十字 → `WindowHandler::request_close` 置 `core.exit = true`
///   → 下一轮 wayland source 回调 [`drive_wayland`] 检查 `core.exit` → `stop()`
/// - `SIGINT` / `SIGTERM` → `calloop::signals::Signals` source 回调 → `stop()`
///   (signalfd 路径,TD-006 竞态消除)
/// - shell 退出(PTY EOF / EIO)→ PTY source 回调 [`pty_read_tick`]
///   `pty_readable_action == RequestExit` → `stop()`
///
/// 退出后按 INV-001 声明顺序(renderer → window → conn)正向 drop,保证 wgpu
/// surface 先放掉 wl_surface 裸指针再关连接,不给 compositor 留 "client didn't
/// release surface" 告警。
pub fn run_window() -> Result<()> {
    let conn = Connection::connect_to_env()
        .context("连接 Wayland compositor 失败(是否在 Wayland session 下?)")?;
    let (globals, event_queue) =
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
    let mut state = State {
        registry_state: RegistryState::new(&globals),
        output_state: OutputState::new(&globals, &qh),
        renderer: None,
        window,
        conn: conn.clone(),
        core: WindowCore::new(INITIAL_WIDTH, INITIAL_HEIGHT),
        pty: None,
    };

    // T-0202/T-0108:spawn login shell + 把 master fd 注册进 calloop(INV-005)。
    // 初始尺寸 80x24 按 ticket scope 写死;Phase 3 T-0306 才接 Wayland
    // configure → cell 尺寸换算。
    let pty = PtyHandle::spawn_shell(80, 24).context("PtyHandle::spawn_shell(80, 24) 失败")?;
    let pty_fd = pty.raw_fd();
    state.pty = Some(pty);

    // 在进 event_loop 之前把 initial request 推给 compositor,否则第一次唤醒等不到
    // configure。registry_queue_init 里已经 flush 过 wl_display.get_registry,但
    // window.commit() 之后还有 toplevel / app_id 等需要落到 socket。
    conn.flush().context("Wayland 初始 flush 失败")?;

    // 构造 calloop EventLoop。Data = LoopData 把 event_queue + state + loop_signal
    // 三样拎一块儿,callback 拿 `&mut LoopData` 走字段 split borrow。
    let mut event_loop: EventLoop<'_, LoopData> =
        EventLoop::try_new().context("calloop EventLoop::try_new 失败")?;
    let loop_handle = event_loop.handle();
    let loop_signal = event_loop.get_signal();

    // Source 1:wayland fd。用 conn.backend().poll_fd() 拿 BorrowedFd。生命周期
    // 擦成 'static 由 drop 序保障(event_loop 本地变量在 state 之前 drop,
    // 其内持有的 Generic source 也随之 drop,之后 state.conn 才关闭)。
    // SAFETY:
    // - poll_fd 返回的 fd 是 wayland_backend Connection 内部 socket,state.conn
    //   持有该 Connection 的 Arc 引用到 run_window 结束 → fd 活
    // - Rust 反向 drop 保证 event_loop(以及它拥有的 source)先于 state 被 drop;
    //   BorrowedFd<'static> 的生命期在代码流上不超过 event_loop 本身
    // - poll_fd.as_raw_fd() 只取 int,不涉资源转移
    #[allow(unsafe_code)]
    let wayland_fd: BorrowedFd<'static> = unsafe {
        let raw = conn.backend().poll_fd().as_raw_fd();
        BorrowedFd::borrow_raw(raw)
    };
    loop_handle
        .insert_source(
            Generic::new(wayland_fd, Interest::READ, Mode::Level),
            |_readiness, _fd, data: &mut LoopData| drive_wayland(data),
        )
        .map_err(|e| anyhow!("calloop insert_source(wayland fd) 失败: {e}"))?;

    // Source 2:SIGINT + SIGTERM。calloop::signals::Signals 内部起一个 signalfd,
    // 信号通过 fd 进 calloop 统一 poll —— 消除 TD-006 的 "handler 跑完 vs poll
    // 进入" 的 nanos 竞态。
    let signals = Signals::new(&[Signal::SIGINT, Signal::SIGTERM])
        .context("calloop Signals::new(SIGINT, SIGTERM) 失败")?;
    loop_handle
        .insert_source(signals, |event, _meta, data: &mut LoopData| {
            tracing::info!(
                signal = ?event.signal(),
                "received termination signal, stopping event loop"
            );
            data.loop_signal.stop();
        })
        .map_err(|e| anyhow!("calloop insert_source(signals) 失败: {e}"))?;

    // Source 3:PTY master fd。回调在 pty_read_tick 里做 read + trace + 退出判定。
    // SAFETY: pty_fd 来自 state.pty.as_ref().raw_fd()(PtyHandle 构造时
    // as_raw_fd().ok_or_else 校验 Some 一次);state.pty 持有 PtyHandle 到
    // run_window 结束,Rust 反向 drop 保证 event_loop(含 Generic source)先
    // 于 state.pty 被 drop,BorrowedFd<'static> 的实际生命期被 drop 序约束在
    // state.pty 寿命内。fcntl / read 都不涉所有权转移。
    #[allow(unsafe_code)]
    let pty_borrowed: BorrowedFd<'static> = unsafe { BorrowedFd::borrow_raw(pty_fd) };
    loop_handle
        .insert_source(
            Generic::new(pty_borrowed, Interest::READ, Mode::Level),
            |_readiness, _fd, data: &mut LoopData| {
                let pty = match data.state.pty.as_mut() {
                    Some(p) => p,
                    // 极罕见:state.pty = None(spawn 后被谁 take 了)。为了不 panic,
                    // 跳过本轮。正常路径到不了这里。
                    None => return Ok(PostAction::Continue),
                };
                pty_read_tick(pty, &data.loop_signal)
            },
        )
        .map_err(|e| anyhow!("calloop insert_source(pty master fd) 失败: {e}"))?;

    tracing::info!(
        width = INITIAL_WIDTH,
        height = INITIAL_HEIGHT,
        "quill 窗口已请求创建"
    );

    // 组装 LoopData 塞进 event_loop.run。run 阻塞直到三源中任一触发 `stop()`。
    // idle 回调用作"每轮 dispatch 之间顺带检查一下 core.exit",兜底 drive_wayland
    // 里万一没命中到的边界(目前应该总是覆盖,留着防回归)。
    let mut loop_data = LoopData {
        event_queue,
        state,
        loop_signal: loop_signal.clone(),
    };
    event_loop
        .run(None, &mut loop_data, |data| {
            if data.state.core.exit {
                data.loop_signal.stop();
            }
        })
        .context("calloop EventLoop::run 失败")?;

    tracing::info!("quill 事件循环退出(INV-001 drop: renderer → window → conn → core → pty)");
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

    // T-0108 删除 7 个单测:`run_main_loop_*` × 5 + `drain_pipe_*` × 2。
    // 这两个函数已随 T-0108 calloop 统一 refactor 一并删除 ——
    // wayland/signal/pty 三条 source 现在全由 `calloop::EventLoop` 统一 poll,
    // 退出走 `LoopSignal::stop()`,不再有手写控制流外壳 `run_main_loop`,也
    // 不再有 signal self-pipe 的 `drain_pipe` 路径。对 calloop 本身的测试
    // 由 calloop 上游负责;本项目只保留**业务逻辑**(`handle_event` /
    // `pty_readable_action`)的 headless 单测。
    //
    // 对应记录:tech-debt.md TD-001 / TD-005 / TD-006 随本 refactor 归档。

    // ---------- T-0205 pty_readable_action 纯逻辑单测 ----------
    // T-0205 acceptance:对"read 返回 0 字节 → 触发 should_exit"这条路径有单测。
    // 我们把决策抽成纯函数 [`pty_readable_action`],单测覆盖全部分支 —— 仿
    // T-0107 抽 `handle_event` 纯逻辑 + `tests/state_machine.rs` 的套路。
    //
    // 真 `pty_read_tick` 会调 `should_exit.store` + `tracing` + `try_wait`,
    // 这些副作用不在纯函数里,所以本组测试不需要构造 PtyHandle / AtomicBool。

    #[test]
    fn pty_readable_action_ok_nonzero_continues_reading() {
        assert_eq!(
            pty_readable_action(&Ok(1)),
            PtyAction::ContinueReading,
            "读到 1 字节应继续循环"
        );
        assert_eq!(
            pty_readable_action(&Ok(PTY_READ_BUF)),
            PtyAction::ContinueReading,
            "读满 buffer 必然还有更多数据,也继续循环"
        );
    }

    #[test]
    fn pty_readable_action_ok_zero_is_request_exit() {
        // T-0205 acceptance 最直接一条:EOF(Ok(0))应触发退出路径。
        assert_eq!(pty_readable_action(&Ok(0)), PtyAction::RequestExit);
    }

    #[test]
    fn pty_readable_action_wouldblock_is_return_continue() {
        // 正常的"没更多数据"语义 —— 跳出 tick 回 calloop 等下一次 ready。
        let err = std::io::Error::from(std::io::ErrorKind::WouldBlock);
        assert_eq!(pty_readable_action(&Err(err)), PtyAction::ReturnContinue);
    }

    #[test]
    fn pty_readable_action_eagain_errno_is_return_continue() {
        // 万一 std 版本或底层驱动把 EAGAIN 映射为 Kind::Uncategorized,raw errno
        // 还是能兜住。防御性测试,对应 `pty_readable_action` 里的二级 raw_os_error
        // 分支。
        let err = std::io::Error::from_raw_os_error(libc::EAGAIN);
        assert_eq!(pty_readable_action(&Err(err)), PtyAction::ReturnContinue);
    }

    #[test]
    fn pty_readable_action_interrupted_is_continue_reading() {
        // EINTR:signal 打断 syscall,重试本轮 read。
        let err = std::io::Error::from(std::io::ErrorKind::Interrupted);
        assert_eq!(pty_readable_action(&Err(err)), PtyAction::ContinueReading);
    }

    #[test]
    fn pty_readable_action_eio_is_request_exit() {
        // T-0205 Implementation notes:某些 kernel 返 EIO 而非 EOF 通知 slave 关闭。
        // 与 Ok(0) 走同一退出路径。
        let err = std::io::Error::from_raw_os_error(libc::EIO);
        assert_eq!(pty_readable_action(&Err(err)), PtyAction::RequestExit);
    }

    #[test]
    fn pty_readable_action_unknown_errno_is_request_exit() {
        // 保守策略:未知 IO 错不继续死等,触发退出避免 main loop 卡死。
        // 测两个典型不幸的 errno —— EBADF(fd 被关)和 EFAULT(buf 指针非法)。
        for errno in [libc::EBADF, libc::EFAULT] {
            let err = std::io::Error::from_raw_os_error(errno);
            assert_eq!(
                pty_readable_action(&Err(err)),
                PtyAction::RequestExit,
                "errno {errno} 应保守触发退出"
            );
        }
    }

    #[test]
    fn pty_readable_action_exhaustive_coverage() {
        // Meta-test:确保所有 PtyAction 变体都至少被一条真实 I/O 结果映射到过。
        // 如果未来加了新的 PtyAction 变体却忘了加对应的映射,这条 meta-test 不
        // 会失败(它只验已有的三种都可达),但 Rust 的 non_exhaustive match 会
        // 在 callsite 编译期报错 —— 双重保险。
        let samples = [
            pty_readable_action(&Ok(1)),
            pty_readable_action(&Ok(0)),
            pty_readable_action(&Err(std::io::Error::from(std::io::ErrorKind::WouldBlock))),
        ];
        assert!(samples.contains(&PtyAction::ContinueReading));
        assert!(samples.contains(&PtyAction::RequestExit));
        assert!(samples.contains(&PtyAction::ReturnContinue));
    }
}
