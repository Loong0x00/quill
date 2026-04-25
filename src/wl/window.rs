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
    delegate_compositor, delegate_output, delegate_registry, delegate_seat, delegate_xdg_shell,
    delegate_xdg_window,
    output::{OutputHandler, OutputState},
    registry::{ProvidesRegistryState, RegistryState},
    registry_handlers,
    seat::{Capability, SeatHandler, SeatState},
    shell::{
        xdg::{
            window::{DecorationMode, Window, WindowConfigure, WindowDecorations, WindowHandler},
            XdgShell,
        },
        WaylandSurface,
    },
};
use wayland_backend::client::WaylandError;
use wayland_client::{
    globals::registry_queue_init,
    protocol::{wl_keyboard, wl_output, wl_seat, wl_surface},
    Connection, Dispatch, EventQueue, Proxy, QueueHandle,
};

use super::keyboard::{handle_key_event, KeyboardAction, KeyboardState};
use super::pointer::{handle_pointer_event, PointerAction, PointerState, WindowButton};
use super::render::Renderer;
use wayland_client::protocol::wl_pointer;

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
    /// T-0503 装饰协商一次性 log 标记。`WindowHandler::configure` 在每次 compositor
    /// 发 configure (focus / resize / state 变化) 都会跑, 但装饰协商结果在窗口
    /// 生命周期内一般不变 — 只在首次 configure 后 log 一次, 避免 trace 噪音。
    /// `false` = 还没 log 过, log 后置 `true`。
    pub decoration_logged: bool,
}

impl WindowCore {
    pub fn new(initial_width: u32, initial_height: u32) -> Self {
        Self {
            width: initial_width,
            height: initial_height,
            first_configure: true,
            exit: false,
            resize_dirty: false,
            decoration_logged: false,
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

/// 把 wayland `EventQueue<State>`、业务 `State`、T-0301 的 `TermState`、
/// calloop `LoopSignal` 四个运行时对象捆成一个结构,让
/// `calloop::EventLoop<'_, LoopData>` 一把 own。回调签名得到 `&mut LoopData`,
/// 在里面做字段级 split borrow:
/// - wayland source 回调:`&mut event_queue` + `&mut state` 跑 `dispatch_pending`
/// - pty source 回调:`&mut state.pty` + `&mut term` + `&loop_signal`
/// - signal source 回调:只需 `&loop_signal`
///
/// `term` 放 LoopData 而非 State 的理由:wayland `Dispatch` 回调(compositor /
/// output / xdg-window)不需要 term,放 State 里会污染 Dispatch 的 mental model;
/// PTY callback 需要跟 pty **同时** borrow term(喂字节),LoopData 级的字段
/// split borrow 刚好覆盖这对。
///
/// `loop_signal` 放这里而非全局,因为业务退出(compositor close、shell 死、
/// SIGINT/SIGTERM、read 错)有多条触发路径,都从 `&mut LoopData` 拿同一把
/// 停机把手,不重复创建。
struct LoopData {
    event_queue: EventQueue<State>,
    state: State,
    term: Option<crate::term::TermState>,
    loop_signal: LoopSignal,
    /// T-0399 P1-1 接入: idle callback 每次成功 draw 后调 `record_and_log`,
    /// 满 [`crate::frame_stats::FRAME_WINDOW`] 帧通过 `tracing::info!`
    /// (target=`quill::frame`) 打一行 — Phase 6 soak 用此采集点观察帧卡顿
    /// 与 RSS 漂移。POD (无 GPU 引用), drop 顺序无关, 放尾部不与 INV-001
    /// 链条 (renderer→window→conn) 耦合。
    frame_stats: crate::frame_stats::FrameStats,
    /// T-0403 加: cosmic-text 字体子系统 (T-0401), Phase 4 idle callback 调
    /// `text_system.shape_line(row_text)` shape 每行 viewport 文本, 然后传给
    /// `Renderer::draw_frame`。Lazy init (None 起步, 首次 idle draw 时建好);
    /// 若 `TextSystem::new()` 失败 (CI 无 monospace 字体) 仍 None, idle callback
    /// 退化到 [`Renderer::draw_cells`] 走色块 fallback (派单接受降级路径)。
    ///
    /// **drop 顺序无关**: cosmic-text `FontSystem` / `SwashCache` 是 owned 堆资源,
    /// 不持 wgpu / wayland 句柄, 与 INV-001 / INV-002 资源链解耦, 放尾部 POD-like
    /// 顺序无关 (与 `frame_stats` 同位置)。
    text_system: Option<crate::text::TextSystem>,
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
fn pty_read_tick(
    pty: &mut PtyHandle,
    term: &mut Option<crate::term::TermState>,
    loop_signal: &LoopSignal,
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
        let result = pty.read(&mut buf);
        match pty_readable_action(&result) {
            PtyAction::ContinueReading => {
                // Ok(n>0) 路径:trace 字节 + T-0301 喂进 alacritty Term 状态机。
                // Err(EINTR) 路径:不 trace,沉默重试。
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
                    // T-0301: 喂 alacritty_terminal 的 `vte::ansi::Processor`。
                    // Option<&mut TermState> 保险:正常路径 term 是 Some(ctor 设好),
                    // None 跳过(未来可能 T-0305+ 在 resize 时临时 take/put)。
                    if let Some(t) = term.as_mut() {
                        t.advance(&buf[..n]);
                        // T-0303:`cursor_point` → `cursor_pos`,返 CellPos 而非
                        // `(usize, i32)`。trace 字段保持 col / line 命名兼容
                        // 旧日志工具,但读自 `pos.col / pos.line` 都是 usize。
                        let pos = t.cursor_pos();
                        tracing::trace!(
                            target: "quill::term",
                            n,
                            col = pos.col,
                            line = pos.line,
                            "term advanced"
                        );
                    }
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

/// T-0503 装饰协商决策。
///
/// `WindowConfigure::decoration_mode` 是 compositor 对我们 `RequestServer` 的应答。
/// 协商规则 (xdg-decoration-unstable-v1 + sctk 0.19 双重保证):
/// - sctk `XdgShell::bind` 自动尝试 bind `zxdg_decoration_manager_v1`; 不存在则
///   `GlobalProxy::Err`, sctk 在 `create_window` 时跳过 `set_mode` 调用, 任何后续
///   `WindowConfigure::decoration_mode` 字段固定 `Client` (sctk 文档明示)
/// - manager 存在但 compositor 拒绝 SSD: 也回 `Client`
/// - manager 存在且 compositor 同意 SSD: 回 `Server`
///
/// **GNOME mutter 50.1 实测不导出 `zxdg_decoration_manager_v1` global**
/// (政策性 CSD-only, GNOME 设计哲学不让 SSD 进, 多年争议无解)。
/// 故 GNOME 桌面下本字段恒 `Client`, quill 当前阶段不自画 CSD (派单 Out C),
/// 结果是窗口无 titlebar — **派单"cargo run 看到 titlebar"在 GNOME 不成立**,
/// 需 KDE / wlroots / Hyprland 等支持 SSD 的 compositor 验证。
///
/// 抽 enum 而非 bool 让未来扩展 (CSD fallback / hybrid 装饰) 不破 ABI;
/// 抽纯 fn 走 conventions §3 套路, 单测覆盖三种输入 → 三种 log 决策。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DecorationLogDecision {
    /// `decoration_mode == Server`: SSD 协商成功, 窗口由 compositor 画装饰。
    /// log 路径: tracing::info! "got server-side decoration"。
    ServerSideAccepted,
    /// `decoration_mode == Client`: 协商被拒 / manager 不存在 (GNOME 路径)。
    /// log 路径: tracing::warn! "fell back to client-side; quill 暂不自画" — 提示
    /// 用户当前 compositor 不支持 SSD, quill 显示无装饰是符合预期的。
    ClientSideFallback,
}

/// 把 sctk `DecorationMode` 转成本模块的决策 enum。INV-010 类型隔离: `DecorationMode`
/// 是 sctk re-export 的 enum, 不流出本 module 边界 (本 fn pub(crate), 上游唯一调
/// 用方 `WindowHandler::configure`); `DecorationLogDecision` 是 quill 自有 enum,
/// 不实现 `From<DecorationMode>` trait (避免下游 `mode.into()` 偷渡公开类型路径)。
///
/// **exhaustive match 无 `_ =>`**: sctk 升级加新 `DecorationMode` variant 时编译
/// 期 catch (例如未来 hybrid / off 模式)。conventions §5 + INV-010 双重要求。
pub(crate) fn decoration_log_decision(mode: DecorationMode) -> DecorationLogDecision {
    match mode {
        DecorationMode::Server => DecorationLogDecision::ServerSideAccepted,
        DecorationMode::Client => DecorationLogDecision::ClientSideFallback,
    }
}

/// surface 像素 → grid cell 计数。Wayland configure 给的是 surface 像素尺寸,
/// 终端 grid 用 cell 计数;两者通过 cell 像素常数 [`crate::wl::render::CELL_W_PX`]
/// / [`crate::wl::render::CELL_H_PX`] 换算(Phase 4 字形测量后会改成字体真实
/// metrics)。
///
/// **`max(1)` 防 0**:整数除在极小 surface(width < CELL_W_PX)时给 0;
/// term / pty 都不接受 0 维度(alacritty `Term::resize` 内部除零 panic,
/// `TIOCSWINSZ` 给 winsize.col=0 也 EINVAL),clamp 到 1 是不可见但合法的最小
/// grid。
///
/// **T-0504**: 可用高度从 `height` 减 [`crate::wl::render::TITLEBAR_H_LOGICAL_PX`]
/// (titlebar 占用顶部 28 logical px), 让 cell rows 数对应 cell 区可用高度. 高度
/// 不足以放完整 titlebar (height < TITLEBAR_H) 时, saturating_sub 防 underflow,
/// cell rows 落到 max(1) 兜底; 视觉上极小窗口仅显示 titlebar 头部 + 1 行 cell,
/// 仍可工作.
///
/// 抽成纯 fn(无副作用、不碰 LoopData)是 conventions §3 "复杂决策抽纯函数 +
/// 单测" 套路的复用 —— resize 数学决策能 headless 单测覆盖,与
/// `propagate_resize_if_dirty` 的真副作用解耦。
///
/// 测试覆盖见 `tests::cells_from_surface_px_*`。
pub(crate) fn cells_from_surface_px(width: u32, height: u32) -> (usize, usize) {
    let usable_h = height.saturating_sub(crate::wl::render::TITLEBAR_H_LOGICAL_PX);
    let cols = ((width as f32) / crate::wl::render::CELL_W_PX) as usize;
    let rows = ((usable_h as f32) / crate::wl::render::CELL_H_PX) as usize;
    (cols.max(1), rows.max(1))
}

/// 纯逻辑决策:看 `core.resize_dirty` 决定 [`propagate_resize_if_dirty`]
/// 是否真要消费一轮 (renderer/term/pty 三方同步) 还是早返。
///
/// 抽出来理由 (T-0399 P2-6, conventions §3 抽状态机模式):
/// `propagate_resize_if_dirty` 含 wgpu Surface / PtyHandle / TermState 三大
/// owned 资源, headless 单测构造 LoopData 成本 = 起真 wayland 连接 + spawn
/// 子进程, 不可行。**抽决策点为纯 bool fn 单独测**, 副作用链 (renderer/term/
/// pty 三方 resize 顺序 + 清 dirty) 由 `tests/resize_chain.rs` 集成测试 +
/// `cells_from_surface_px_*` 4 个单测覆盖。
///
/// 与 [`pty_readable_action`] / [`handle_event`] 同款套路:决策与副作用分离,
/// 决策 headless 测, 副作用 trace + 集成测试覆盖。
pub(crate) fn should_propagate_resize(resize_dirty: bool) -> bool {
    resize_dirty
}

/// 把 `core.resize_dirty` 触发的 resize 同步推给 renderer / term / pty 三方。
/// `drive_wayland` 在 `dispatch_pending` 之后调一次 —— configure event 在 dispatch
/// 时跑 `WindowHandler::configure` → `handle_event` 置 dirty,本 fn 紧接消费。
///
/// 三方同步顺序(无强约束,但顺序固定便于排障):
/// 1. **renderer.resize**:重 configure wgpu surface (新 width/height);
///    NDC 换算的 surface_w/h 也跟随更新,下一次 draw_cells 自动用新尺寸
/// 2. **term.resize**:alacritty Term grid resize,内部 clamp cursor / 调
///    selection / scroll_region / damage,置 dirty 触发下一次 idle 重画
/// 3. **pty.resize**:`ioctl(TIOCSWINSZ)` 推新 winsize 给 PTY master,kernel
///    给前台进程组发 `SIGWINCH`,bash / vim 重新 query winsize 自适应
///
/// **INV-006 消费者职责**:清 `state.core.resize_dirty = false`。本 fn 是 dirty
/// 标记的唯一上游消费者(T-0306 改:原 `WindowHandler::configure` 的 init 路径
/// 清零职责迁过来,使消费者单一)。
///
/// **错误处理**:pty.resize 走 ioctl 极少失败,失败仅 `tracing::warn` 不 panic /
/// 不退出 —— terminal grid 已变,UI 仍能继续工作,shell 看不到新 winsize 是
/// 退化但非致命。term/renderer.resize 自身是 infallible(panic-safe)。
///
/// **split borrow**:`LoopData { state, term, .. } = &mut *data;` 同时拿
/// `&mut state.pty / &mut state.renderer / &mut state.core / &mut term` 四份;
/// LoopData 不同字段间 NLL OK,且 state.* 与 term 是独立 LoopData 字段。
fn propagate_resize_if_dirty(data: &mut LoopData) {
    if !should_propagate_resize(data.state.core.resize_dirty) {
        return;
    }
    let LoopData { state, term, .. } = &mut *data;

    let width = state.core.width;
    let height = state.core.height;
    let (cols, rows) = cells_from_surface_px(width, height);

    if let Some(r) = state.renderer.as_mut() {
        r.resize(width, height);
    }

    if let Some(t) = term.as_mut() {
        t.resize(cols, rows);
    }

    if let Some(p) = state.pty.as_ref() {
        if let Err(err) = p.resize(cols as u16, rows as u16) {
            // ioctl(TIOCSWINSZ) 极少失败,但 fd 提前关 / EBADF 时不 panic ——
            // shell 收不到 SIGWINCH 是退化, UI 仍走 (term 已 resize, 渲染正常)。
            tracing::warn!(
                ?err,
                cols,
                rows,
                "pty.resize ioctl 失败, shell 不会收 SIGWINCH"
            );
        }
    }

    // T-0504: 同步 PointerState 的 surface 尺寸 (logical px), 让 hit_test 用
    // 最新尺寸算按钮位置 (按钮在右上角, 拖窗口时按钮跟着移).
    state.pointer_state.set_surface_size(width, height);

    state.core.resize_dirty = false;
    tracing::debug!(
        width,
        height,
        cols,
        rows,
        "propagated resize → renderer + term + pty + pointer"
    );
}

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

    // Step 3.5(T-0306):dispatch_pending 期间 `WindowHandler::configure` 可能
    // 已置 `core.resize_dirty`(尺寸变化)。在 flush 之前把 renderer / term /
    // pty 三方同步推完,使本轮事件结束前一切就绪。flush 后下一次 idle callback
    // 看到 term.is_dirty (resize 副作用) 会立刻 draw_cells 用新 cols/rows 重画。
    propagate_resize_if_dirty(data);

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

    // T-0502: 告诉 compositor 我们自己处理 HiDPI, 不要 double-scale。
    //
    // 背景: T-0404 引 `HIDPI_SCALE = 2` 把 surface backing buffer (wgpu)
    // 翻倍到 physical px, 但**没**调 `wl_surface.set_buffer_scale(2)`。
    // compositor 默认假设 client buffer 是 logical scale=1, 在 HiDPI 输出
    // (224 ppi mutter scale=2) 上又自动放大一遍 → 视觉 ×4 ("有点大的不正常")。
    // 调 set_buffer_scale(2) 后 compositor 知道 buffer 已是 physical, 不再
    // double-scale, 视觉回归 1:1。
    //
    // **why hardcode 而非接 `wl_output.scale` event**: 用户硬偏好 (T-0404 派单
    // Out 段, T-0502 派单 Out 段重申)。单显示器 224 ppi 固定 2x, 多显示器 /
    // 不同 ppi 切换是 ROADMAP 永久不接 scope。`OutputHandler::new_output` /
    // `update_output` 仅 log compositor 上报的 scale 与 HIDPI_SCALE 不一致时
    // warn (诊断用), 不做动态适配。
    //
    // **why 此处而非 init_renderer_and_draw**: 协议要求 set_buffer_scale 在
    // attach buffer **之前** 调 (否则下一次 commit 才生效)。本项目首次 attach
    // 由 `init_renderer_and_draw` 内 wgpu surface.configure + r.render() 触发,
    // 此处放在 `window.commit()` (空 surface map 请求) 之前、surface 创建之后,
    // 满足"attach 前已设"的协议要求, 且 set_buffer_scale 是 pending state, 与
    // 后续 commit 一并生效。
    //
    // SCTK `WaylandSurface::set_buffer_scale(u32)` 内部 version >= 3 才发请求,
    // 否则返 `Unsupported` (老 compositor 不支持 v3, 我们直接吞错并 warn)。
    // 现代 mutter / kwin / sway 都 v3+, 实战不触发。
    if let Err(err) = window.set_buffer_scale(crate::wl::HIDPI_SCALE) {
        tracing::warn!(
            ?err,
            scale = crate::wl::HIDPI_SCALE,
            "wl_surface.set_buffer_scale 失败 (compositor wl_surface < v3?), \
             视觉可能 double-scale; 升级 compositor 修复"
        );
    }

    // Implementation note: 第一次 configure 前只能 commit 空 surface(无 buffer 附加),
    // 这是 xdg-shell 的 map 请求语义。本次 commit 同时把上面 set_buffer_scale 的
    // pending state 推给 compositor (协议: scale 是 double-buffered, commit 生效)。
    window.commit();

    // State 字段顺序固化 INV-001(renderer→window→conn)+ pty 放最后(T-0202 Lead + 审码)。
    // T-0501 加: seat_state / keyboard_state / keyboard 三字段位于 core 与 pty 之间,
    // 不破坏 INV-001 链条 (它们都不持 wgpu/wayland 裸指针, 仅 SCTK/wayland-client
    // safe wrapper, drop 顺序无 UB 风险)。
    let mut state = State {
        registry_state: RegistryState::new(&globals),
        output_state: OutputState::new(&globals, &qh),
        renderer: None,
        window,
        conn: conn.clone(),
        core: WindowCore::new(INITIAL_WIDTH, INITIAL_HEIGHT),
        seat_state: SeatState::new(&globals, &qh),
        keyboard_state: KeyboardState::new()
            .context("KeyboardState::new 失败 (xkbcommon Context 初始化)")?,
        keyboard: None,
        // T-0504: PointerState 起步用 INITIAL_WIDTH/HEIGHT (与 WindowCore 同步),
        // configure 收到首次尺寸后 propagate_resize_if_dirty 调 set_surface_size
        // 同步.
        pointer_state: PointerState::new(INITIAL_WIDTH, INITIAL_HEIGHT),
        pointer: None,
        pointer_seat: None,
        is_maximized: false,
        presentation_dirty: false,
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
    // 擦成 'static 由 drop 序 + calloop 内部对已关 fd 容忍保障。
    // SAFETY:
    // - poll_fd 返回的 fd 是 wayland_backend Connection 内部 socket;state.conn
    //   持有该 Connection 的 Arc 引用,在 run_window scope 内一直活
    // - 实际 drop 序 (T-0108 重构后, T-0399 housekeeping 校正): event_loop 在
    //   line 602 声明、loop_data 在 line 685 声明 → Rust 反向声明顺序 →
    //   loop_data 先 drop (loop_data.state.conn 关 wayland fd) → event_loop
    //   后 drop (event_loop 内 Generic source 的 epoll_ctl(EPOLL_CTL_DEL)
    //   对此时已关闭的 fd 调用)。Linux kernel 对 EPOLL_CTL_DEL 已关 fd 返
    //   EBADF, calloop 0.14 内部容忍 (silent ignore / log), **非 UB**
    // - 即 `BorrowedFd<'static>` 的"语法 'static"不依赖 fd 实际活到 event_loop
    //   drop 那一刻;依赖 calloop 内部 syscall 容忍 EBADF (drop-time race
    //   safe by design of epoll API + calloop)
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
    // SAFETY:
    // - pty_fd 来自 state.pty.as_ref().raw_fd()(PtyHandle 构造时
    //   as_raw_fd().ok_or_else 校验 Some 一次)。state.pty 持有 PtyHandle 在
    //   run_window scope 内一直活
    // - 实际 drop 序 (T-0108 重构后, T-0399 housekeeping 校正): event_loop 在
    //   line 602 声明、loop_data 在 line 685 声明, state 已被 move 进
    //   loop_data.state → Rust 反向声明顺序 → loop_data 先 drop (按字段顺序
    //   关 state.pty 的 master fd) → event_loop 后 drop (Generic source 的
    //   epoll_ctl(EPOLL_CTL_DEL) 对此时已关闭的 pty fd 调用)。Linux kernel
    //   对 EPOLL_CTL_DEL 已关 fd 返 EBADF, calloop 0.14 内部容忍, **非 UB**
    // - 即 `BorrowedFd<'static>` 的"语法 'static"不依赖 fd 实际活到 event_loop
    //   drop 那一刻;依赖 calloop 内部 syscall 容忍 EBADF。fcntl / read 都不
    //   涉所有权转移
    #[allow(unsafe_code)]
    let pty_borrowed: BorrowedFd<'static> = unsafe { BorrowedFd::borrow_raw(pty_fd) };
    loop_handle
        .insert_source(
            Generic::new(pty_borrowed, Interest::READ, Mode::Level),
            |_readiness, _fd, data: &mut LoopData| {
                // split borrow: pty 藏在 data.state.pty,term 是 data.term 自己,
                // loop_signal 也是 data 自己的字段 —— 三个字段互相不冲突,
                // 可同时 &mut 拿出来。`&mut *data` 强制重借用,让编译器看见
                // 字段级 split。
                let LoopData {
                    state,
                    term,
                    loop_signal,
                    ..
                } = &mut *data;
                let pty = match state.pty.as_mut() {
                    Some(p) => p,
                    // 极罕见:state.pty = None(spawn 后被谁 take 了)。为了不 panic,
                    // 跳过本轮。正常路径到不了这里。
                    None => return Ok(PostAction::Continue),
                };
                pty_read_tick(pty, term, loop_signal)
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
    // T-0403: lazy init TextSystem。CI 无 monospace 字体 / 加载失败也允许 (warn
    // + None), idle callback 退化到 draw_cells (色块 fallback) — 派单 In #C
    // "lazy init, 第一次 draw 时建" 描述; 这里启动期建避免每帧检查。
    let text_system = match crate::text::TextSystem::new() {
        Ok(ts) => Some(ts),
        Err(err) => {
            tracing::warn!(
                ?err,
                "TextSystem::new 失败 — Phase 4 字形渲染降级到 Phase 3 色块 (cargo run \
                 仍可见深蓝清屏 + 浅灰色块)。check `fc-list :spacing=mono`."
            );
            None
        }
    };

    let mut loop_data = LoopData {
        event_queue,
        state,
        // T-0301: 初始 80x24,与 `PtyHandle::spawn_shell(80, 24)` 对齐。
        // Phase 3 T-0306 接窗口 resize 时会重建 Term 或调用 resize method。
        term: Some(crate::term::TermState::new(80, 24)),
        loop_signal: loop_signal.clone(),
        // T-0399 P1-1: FrameStats 接采集点。空 stats, idle callback 每次成功
        // draw_cells 后调 record_and_log; Phase 6 soak 通过 `quill::frame`
        // target 观察帧间隔聚合。
        frame_stats: crate::frame_stats::FrameStats::new(),
        text_system,
    };
    event_loop
        .run(None, &mut loop_data, |data| {
            if data.state.core.exit {
                data.loop_signal.stop();
                return;
            }
            // T-0305 / T-0403: 渲染触发点。`event_loop.run` 的第三参数是每轮
            // dispatch 之后的 idle / post-tick callback (calloop 称 "before next
            // iter"),正是 "wayland fd / pty fd / signalfd 任一 ready 跑完 dispatch
            // 后" 的时机 —— PTY 字节进 term.advance 触发 dirty,本闭包看到 dirty
            // 就 draw_frame 一帧 (含 clear + cells + glyphs, T-0403) 或 draw_cells
            // (text_system 未建好时 fallback) + clear_dirty。
            //
            // borrow split: data.term / data.state.renderer / data.frame_stats /
            // data.text_system 都是 LoopData 不同字段, 一次解构同时拿四个 &mut
            // 不冲突。
            let LoopData {
                state,
                term,
                frame_stats,
                text_system,
                ..
            } = &mut *data;
            let Some(t) = term.as_mut() else {
                return;
            };
            // T-0504: 检查 term cell 内容 dirty || presentation (CSD hover) dirty.
            // 任一为真即重画. presentation_dirty 由 Dispatch<WlPointer> 在
            // HoverChange 时置位, 重画后清.
            if !t.is_dirty() && !state.presentation_dirty {
                return;
            }
            let Some(r) = state.renderer.as_mut() else {
                // renderer 还没建好(首次 configure 之前的 idle tick)。dirty
                // 留着,等首次 configure 走 init_renderer_and_draw 完成后下一轮
                // 再画。
                return;
            };
            // T-0305: 全量 cells 收集。1920 cell × CellRef(~32 字节 pos+c+fg+bg)
            // = 60 KiB,Vec 分配开销 << wgpu submit 一次的开销, Phase 6 soak 验
            // bench 再决定是否 reuse Vec(目前简单胜过聪明)。
            let cells: Vec<crate::term::CellRef> = t.cells_iter().collect();
            let (cols, rows) = t.dimensions();

            // T-0504: hover 区域 (CSD titlebar 按钮高亮) 由 PointerState 维护.
            let hover = state.pointer_state.hover();

            // T-0403: 走 draw_frame (含字形); 若 text_system 未建好降级到
            // draw_cells (Phase 3 色块路径)。
            let draw_result = match text_system.as_mut() {
                Some(ts) => {
                    // 收集每行的文本快照, 喂给 draw_frame shape。t.line_text(row)
                    // 给完整一行的 String (含末尾空白)。`rows` 行 × ~80 字符每行
                    // = ~7 KiB Vec, 与 cells Vec 同数量级。
                    let row_texts: Vec<String> = (0..rows).map(|row| t.line_text(row)).collect();
                    r.draw_frame(ts, &cells, cols, rows, &row_texts, hover)
                }
                None => r.draw_cells(&cells, cols, rows),
            };
            if let Err(err) = draw_result {
                tracing::warn!(
                    ?err,
                    "draw_frame / draw_cells 失败, 跳过本帧 (dirty 仍清, 避免下轮再撞同样错)"
                );
            }
            // T-0399 P1-1: 记录本帧 present 时间; 每满 FRAME_WINDOW (60) 帧
            // 走一次 tracing::info! (target=quill::frame), Phase 6 soak 用此
            // 信号观察帧间隔聚合 + RSS 漂移。失败路径也记 — 帧"尝试"算一次
            // present (与 dirty 清零节奏对齐, 一次 idle 一次 record)。
            frame_stats.record_and_log(std::time::Instant::now());
            t.clear_dirty();
            state.presentation_dirty = false;
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
    // T-0501: SeatState + KeyboardState + 当前绑定的 wl_keyboard。
    // SeatState 是 SCTK helper, 监听 wl_seat 全局 + capabilities 变化, 不持
    // wgpu / wayland 裸指针 (仅 wayland-client safe wrapper); 放此处不破坏
    // INV-001 链条 — 与 OutputState / RegistryState 同性质 (POD-like, 上游
    // 自管 drop)。
    seat_state: SeatState,
    keyboard_state: KeyboardState,
    /// 当前绑定的 wl_keyboard (capabilities 含 Keyboard 时由 SeatHandler::
    /// new_capability 创建); 移除 keyboard capability 时 Some→None。
    /// drop 顺序: WlKeyboard 是 wayland Proxy (handle), drop 时发 release
    /// request 给 compositor, 不依赖 wgpu, 放尾部安全。
    keyboard: Option<wl_keyboard::WlKeyboard>,
    /// T-0504: 鼠标状态封装 (PointerState 自有 struct, INV-010 类型隔离, 字段
    /// 全私有). 与 keyboard_state 同性质 — wayland safe wrapper, drop 顺序
    /// 无 UB 风险, 放此处不破坏 INV-001 链条.
    pointer_state: PointerState,
    /// 当前绑定的 wl_pointer (capabilities 含 Pointer 时由 SeatHandler::
    /// new_capability 创建). 与 keyboard 同性质.
    pointer: Option<wl_pointer::WlPointer>,
    /// T-0504: 当前绑定 wl_pointer 时关联的 wl_seat (最后一次新 Pointer
    /// capability 出现时记). xdg_toplevel.move 协议要求传 wl_seat + serial,
    /// PointerAction::StartMove 路径需读此字段. drop 顺序: WlSeat 也是
    /// wayland Proxy, 与 keyboard / pointer 同等放尾部安全.
    pointer_seat: Option<wl_seat::WlSeat>,
    /// T-0504: 当前是否最大化 (toggle 状态). ButtonClick(Maximize) 时反转 →
    /// 调 set_maximized / unset_maximized. WindowConfigure event (configure
    /// 携带 state 数组含 Maximized) 也可同步, 但 sctk 0.19 WindowConfigure
    /// 暴露的是 Vec<State>, 接入复杂; 简化: 客户端自跟踪, 与 Adwaita /
    /// alacritty 等 CSD 客户端实践一致.
    is_maximized: bool,
    /// T-0504: presentation-only dirty (CSD titlebar 按钮 hover 高亮变化).
    /// cell 内容未变, term.is_dirty 不会置位, 但 hover 切换需要重画 titlebar.
    /// idle callback 检查 `term.is_dirty() || state.presentation_dirty` 决定
    /// 是否重画; 重画后置 false. 与 INV-006 resize_dirty 同布尔脏标记套路.
    presentation_dirty: bool,
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
        output: wl_output::WlOutput,
    ) {
        // T-0502: 仅记录 compositor 上报的 scale, **不**做动态适配 (派单 Out 段,
        // 用户单显示器 224 ppi 固定 2x)。HIDPI_SCALE 是 hardcode const, 与
        // compositor 上报 scale 不一致时仅 warn 提示用户 / 排障 (例如插了
        // 96 ppi 副屏, 或换 6K HiDPI 屏想升 3x)。
        log_output_scale(&self.output_state, &output, "new_output");
    }

    fn update_output(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        output: wl_output::WlOutput,
    ) {
        // T-0502: 同 new_output, compositor 后续更新 (例如 hot-plug 新 monitor /
        // 用户改显示设置) 时再 log 一次。仍**不**响应 (HIDPI_SCALE const)。
        log_output_scale(&self.output_state, &output, "update_output");
    }

    fn output_destroyed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _output: wl_output::WlOutput,
    ) {
    }
}

/// T-0502: `log_output_scale` 的纯逻辑决策 — 给定 compositor 上报 scale 与
/// hardcode `HIDPI_SCALE`, 算出 `OutputScaleVerdict` 决定 trace 走 debug 还是
/// warn 路径。
///
/// 抽出来理由 (conventions §3 抽状态机模式): `log_output_scale` 内部需
/// OutputState 加 WlOutput 真 wayland 对象, headless 单测构造成本高 (起
/// compositor)。本枚举把"匹配 vs 不匹配"决策剥成 i32 到 enum 纯映射, 配
/// `verdict_for_scale` 单测锁住决策, 上层 (`log_output_scale`) 改 trace 字段
/// 格式或加新分支时这组单测拦决策回归。
///
/// 与 `pty_readable_action` / `should_propagate_resize` / `cells_from_surface_px`
/// 同款套路: 决策与副作用分离, 决策 headless 测, 副作用 trace + 集成测试覆盖。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OutputScaleVerdict {
    /// compositor scale == HIDPI_SCALE, 走 `tracing::debug` 仅记录。
    Match,
    /// compositor scale != HIDPI_SCALE, 走 `tracing::warn` 提示用户改 const
    /// 重编译 (派单 Out 段不做动态)。
    Mismatch,
}

/// T-0502: 给定 compositor 上报 scale 与我们 hardcode 的 scale, 决策 trace 级别。
pub(crate) fn verdict_for_scale(compositor_scale: i32, our_scale: i32) -> OutputScaleVerdict {
    if compositor_scale == our_scale {
        OutputScaleVerdict::Match
    } else {
        OutputScaleVerdict::Mismatch
    }
}

/// T-0502: 把 compositor 上报的 wl_output scale 与 hardcode `HIDPI_SCALE`
/// 对比, 一致 `tracing::debug`, 不一致 `tracing::warn` 提示用户。
///
/// **why 不动态响应**: 派单 Out 段 (用户硬偏好, 单一 224 ppi 显示器场景)。
/// 仅诊断用 — 用户切显示器 / 接副屏时日志能看出 mismatch, 决定是否手动改
/// `HIDPI_SCALE` 重编译。`OutputState::info` 在 wl_output 信息尚未到齐时返
/// `None` (例: `new_output` 在 done event 之前先触发), 此时跳过。
fn log_output_scale(output_state: &OutputState, output: &wl_output::WlOutput, event: &'static str) {
    let Some(info) = output_state.info(output) else {
        return;
    };
    let compositor_scale = info.scale_factor;
    let our_scale = crate::wl::HIDPI_SCALE as i32;
    match verdict_for_scale(compositor_scale, our_scale) {
        OutputScaleVerdict::Match => {
            tracing::debug!(
                event,
                name = ?info.name,
                scale = compositor_scale,
                "wl_output scale 与 HIDPI_SCALE 匹配"
            );
        }
        OutputScaleVerdict::Mismatch => {
            tracing::warn!(
                event,
                name = ?info.name,
                compositor_scale,
                our_scale,
                "wl_output scale 与 hardcode HIDPI_SCALE 不一致; \
                 视觉可能偏大或偏小, 改 src/wl/render.rs::HIDPI_SCALE 重编译适配"
            );
        }
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

        // T-0503: 装饰协商结果一次性 log。configure 每帧 fire (focus/resize/state),
        // 装饰模式生命周期内不变, 只 log 第一次。GNOME mutter 不导出
        // zxdg_decoration_manager_v1, 永远 ClientSideFallback (warn);
        // KDE/wlroots/Hyprland 通常 ServerSideAccepted (info, 用户能看 titlebar)。
        if !self.core.decoration_logged {
            match decoration_log_decision(configure.decoration_mode) {
                DecorationLogDecision::ServerSideAccepted => {
                    tracing::info!(
                        target: "quill::wl::decoration",
                        "xdg-decoration negotiated: ServerSide \
                         (compositor 画 titlebar + 最小化/最大化/关闭按钮)"
                    );
                }
                DecorationLogDecision::ClientSideFallback => {
                    // T-0504: quill 已自画 CSD (titlebar + 最小化/最大化/关闭),
                    // ClientSide 路径不再无装饰. 改 warn → info, 描述自画行为.
                    tracing::info!(
                        target: "quill::wl::decoration",
                        "xdg-decoration: ClientSide (compositor 不支持 SSD 或未导出 \
                         zxdg_decoration_manager_v1, e.g. GNOME mutter); \
                         quill 自画 CSD (titlebar + 最小化/最大化/关闭按钮)"
                    );
                }
            }
            self.core.decoration_logged = true;
        }

        let was_first = self.core.first_configure;
        let action = handle_event(
            &mut self.core,
            WindowEvent::Configure {
                new_width: new_w,
                new_height: new_h,
            },
        );

        // 首次 configure 建 renderer 并画一次清屏占位帧;之后的 size 同步走
        // [`propagate_resize_if_dirty`](`drive_wayland` 在 dispatch_pending 后
        // 调一次)—— 那里同时推 renderer.resize / term.resize / pty.resize,
        // 然后清 `core.resize_dirty`(INV-006 的"显式清零"由 propagate 承担)。
        //
        // T-0306 改:**不**在此处清 `resize_dirty`。原 T-0103 临时让 init 路径
        // 清零,T-0306 把"resize → 三方同步"统一到 propagate, INV-006 的消费者
        // 责任单一。init 失败仍 panic 退出(renderer 起不来 quill 没意义跑下去)。
        if was_first && action.needs_draw {
            if let Err(err) = self.init_renderer_and_draw() {
                tracing::error!(?err, "wgpu renderer 初始化或首帧失败");
                self.core.exit = true;
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
delegate_seat!(State);
delegate_xdg_shell!(State);
delegate_xdg_window!(State);
delegate_registry!(State);

impl SeatHandler for State {
    fn seat_state(&mut self) -> &mut SeatState {
        &mut self.seat_state
    }

    fn new_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat) {
        // 新 seat 出现 (compositor 启动期 / 用户插入新键盘 hub) — 不立即 bind
        // keyboard, 等 new_capability(Keyboard) 才 bind。这是 SCTK 标准模式,
        // 让 capability 路径单一。
    }

    fn new_capability(
        &mut self,
        _conn: &Connection,
        qh: &QueueHandle<Self>,
        seat: wl_seat::WlSeat,
        capability: Capability,
    ) {
        if capability == Capability::Keyboard && self.keyboard.is_none() {
            // wl_seat::get_keyboard 返 raw WlKeyboard, 我们走自己的
            // Dispatch<WlKeyboard, ()> impl (派单 In #C, 不用 SCTK keyboard
            // 模块的 KeyboardHandler — INV-010 类型隔离, KeyboardState 是
            // quill 自有, 不偷渡 SCTK keyboard 类型)。
            //
            // user_data = () : 我们用 State 字段 self.keyboard 跟踪当前绑定,
            // 不需要 per-keyboard 用户数据。
            let kb = seat.get_keyboard(qh, ());
            tracing::info!("wl_seat capability Keyboard 出现, wl_keyboard 已绑定");
            self.keyboard = Some(kb);
        }
        // T-0504: Pointer 同 Keyboard 路径 — wl_seat::get_pointer 返 raw
        // WlPointer, 走自己的 Dispatch<WlPointer, ()> impl (INV-010, PointerState
        // 是 quill 自有, 不偷渡 SCTK pointer 类型). 记 seat 给 xdg_toplevel.move
        // 用 (StartMove 路径需 seat + serial).
        if capability == Capability::Pointer && self.pointer.is_none() {
            let ptr = seat.get_pointer(qh, ());
            tracing::info!("wl_seat capability Pointer 出现, wl_pointer 已绑定");
            self.pointer = Some(ptr);
            self.pointer_seat = Some(seat);
        }
    }

    fn remove_capability(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: wl_seat::WlSeat,
        capability: Capability,
    ) {
        if capability == Capability::Keyboard {
            // capability 移除 (用户拔键盘 hub / compositor 切 seat 配置) —
            // drop 当前 wl_keyboard, 后续 Key event 不会再来。新 capability
            // 出现时 new_capability 会重 bind。
            if let Some(kb) = self.keyboard.take() {
                kb.release();
                tracing::info!("wl_seat capability Keyboard 移除, wl_keyboard 释放");
            }
        }
        if capability == Capability::Pointer {
            if let Some(ptr) = self.pointer.take() {
                ptr.release();
                tracing::info!("wl_seat capability Pointer 移除, wl_pointer 释放");
            }
            // pointer_seat 暂留 — 极端 race 下 seat 可能仍持续 (compositor 只
            // 移 capability 而 seat 本身仍存); remove_seat 路径再清.
        }
    }

    fn remove_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat) {
        // seat 整个移除 — 当前 wl_keyboard / wl_pointer 一并失效, 走 take 释放。
        if let Some(kb) = self.keyboard.take() {
            kb.release();
        }
        if let Some(ptr) = self.pointer.take() {
            ptr.release();
        }
        self.pointer_seat = None;
    }
}

/// wl_keyboard 协议事件 → 转 [`handle_key_event`] → bytes → `PtyHandle::write`。
///
/// **why 自己实现 Dispatch 而非 SCTK KeyboardHandler** (INV-010 + 派单 In #C):
/// SCTK 0.19 的 keyboard 模块虽然封装了 keymap 加载 / modifier 同步, 但它的
/// `KeyEvent` struct 把 `xkbcommon::xkb::Keysym` 字段暴露在 trait 边界, 让
/// quill 必须 import xkbcommon 类型走过 SCTK 这一层 — 类型隔离半破。本项目
/// 走 raw `Dispatch<WlKeyboard>` + 自己持 `KeyboardState` (内部封 xkbcommon),
/// quill 公共 API (`handle_key_event` 入参 `wl_keyboard::Event`, 出参
/// `KeyboardAction`) 全 quill 自有 / wayland-client 协议类型, 不漏 xkbcommon。
///
/// **PTY write 路径**: KeyboardAction::WriteToPty(bytes) → self.pty.write(&bytes)。
/// master fd O_NONBLOCK (INV-009), WouldBlock 视为背压**丢字节** (派单 In #D
/// 允许)。daily drive 罕见, paste 大段时可能丢 — Phase 6 加 paste throttle 解。
impl Dispatch<wl_keyboard::WlKeyboard, ()> for State {
    fn event(
        state: &mut Self,
        _proxy: &wl_keyboard::WlKeyboard,
        event: wl_keyboard::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        let action = handle_key_event(event, &mut state.keyboard_state);
        match action {
            KeyboardAction::Nothing => {}
            KeyboardAction::WriteToPty(bytes) => {
                let Some(pty) = state.pty.as_ref() else {
                    tracing::warn!(
                        target: "quill::keyboard",
                        n = bytes.len(),
                        "WriteToPty 时 pty=None, 丢字节"
                    );
                    return;
                };
                match pty.write(&bytes) {
                    Ok(n) if n == bytes.len() => {
                        tracing::trace!(
                            target: "quill::keyboard",
                            n,
                            "wrote keyboard bytes to pty"
                        );
                    }
                    Ok(n) => {
                        // partial write — PTY 内核 buffer 几乎满。剩余字节
                        // **派单允许丢** (背压策略, 不阻塞主循环)。
                        tracing::warn!(
                            target: "quill::keyboard",
                            wrote = n,
                            total = bytes.len(),
                            "pty.write 部分写入, 剩余字节丢弃 (背压)"
                        );
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        // O_NONBLOCK fd buffer 满 — daily drive 罕见, paste 大
                        // 段可能撞到。派单 In #D 接受丢字节, Phase 6 paste
                        // throttle 解。
                        tracing::warn!(
                            target: "quill::keyboard",
                            n = bytes.len(),
                            "pty.write WouldBlock, 字节丢弃 (背压, INV-005 不重试)"
                        );
                    }
                    Err(e) => {
                        // 其它 IO 错 (EBADF / EIO 等) — pty 可能已死, 主循环
                        // 由 pty_read_tick EOF 路径处理退出, 这里仅 warn。
                        tracing::warn!(
                            target: "quill::keyboard",
                            error = %e,
                            n = bytes.len(),
                            "pty.write 失败 (非 WouldBlock)"
                        );
                    }
                }
            }
        }
    }
}

/// T-0504: wl_pointer 协议事件 → 转 [`handle_pointer_event`] → [`PointerAction`]
/// 分派 → xdg_toplevel.move / set_minimized / set_maximized / close.
///
/// **why 自己实现 Dispatch 而非 SCTK PointerHandler** (INV-010 + 派单 In #C 同
/// keyboard 同决策): SCTK 0.19 pointer 模块的 `PointerEvent` struct 把内部坐标
/// / 滚轮帧 / cursor shape 等揉在一起, 暴露需要 `import` SCTK 类型走 trait 边界.
/// 本项目走 raw `Dispatch<WlPointer>` + 自己持 [`PointerState`] (内部封 hover /
/// pos / serial), quill 公共 API (`handle_pointer_event` 入参 `wl_pointer::Event`,
/// 出参 `PointerAction`) 全 quill 自有 / wayland-client 协议类型, 不漏 SCTK.
///
/// **redraw 路径**: PointerAction::HoverChange 触发 redraw — 走 `term.set_dirty()`
/// 让下一次 idle callback 重画. cell 内容未变, 但 titlebar 三按钮 hover 状态
/// 在 `Renderer::draw_frame` 入参 hover 中读, 重画时按钮颜色更新.
impl Dispatch<wl_pointer::WlPointer, ()> for State {
    fn event(
        state: &mut Self,
        _proxy: &wl_pointer::WlPointer,
        event: wl_pointer::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        let action = handle_pointer_event(event, &mut state.pointer_state);
        match action {
            PointerAction::Nothing => {}
            PointerAction::HoverChange(_new_hover) => {
                // hover 变化 → 触发下一次 idle callback 重画 (按钮高亮更新).
                // term 内容未变, 走 state.presentation_dirty (新增, 与 term
                // .dirty 解耦避免污染 cell 渲染节奏). idle callback 检查
                // `term.is_dirty() || state.presentation_dirty` 决定是否重画.
                state.presentation_dirty = true;
                tracing::trace!(
                    target: "quill::pointer",
                    ?_new_hover,
                    "hover changed, presentation_dirty=true"
                );
            }
            PointerAction::StartMove { serial } => {
                // xdg_toplevel.move(seat, serial). compositor 接管拖动直到鼠标
                // release; quill 期间不收 motion event (compositor grab pointer).
                let Some(seat) = state.pointer_seat.as_ref() else {
                    tracing::warn!(
                        target: "quill::pointer",
                        "StartMove 时 pointer_seat=None, 跳过 (race 下罕见)"
                    );
                    return;
                };
                tracing::debug!(
                    target: "quill::pointer",
                    serial,
                    "xdg_toplevel.move (titlebar drag)"
                );
                state.window.move_(seat, serial);
            }
            PointerAction::ButtonClick(button) => match button {
                WindowButton::Minimize => {
                    tracing::info!(target: "quill::pointer", "click Minimize → set_minimized");
                    state.window.set_minimized();
                }
                WindowButton::Maximize => {
                    // toggle: 当前 maximized → unset; 否则 set.
                    if state.is_maximized {
                        tracing::info!(
                            target: "quill::pointer",
                            "click Maximize (toggle) → unset_maximized"
                        );
                        state.window.unset_maximized();
                        state.is_maximized = false;
                    } else {
                        tracing::info!(
                            target: "quill::pointer",
                            "click Maximize (toggle) → set_maximized"
                        );
                        state.window.set_maximized();
                        state.is_maximized = true;
                    }
                }
                WindowButton::Close => {
                    tracing::info!(target: "quill::pointer", "click Close → exit");
                    let _ = handle_event(&mut state.core, WindowEvent::Close);
                }
            },
        }
    }
}

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

    // ---------- T-0306 cells_from_surface_px 纯逻辑单测 ----------
    // surface 像素 → grid cell 计数的换算决策, 抽成纯 fn 让测试覆盖整数除 +
    // max(1) clamp 两条分支, 不需要构造 LoopData / 真 wayland 连接。

    #[test]
    fn cells_from_surface_px_default_800x600_matches_80x22() {
        // 初始尺寸 800×600 + cell 10×25 + T-0504 titlebar 28 → usable 572 → 22 行.
        // 之前 (T-0306 时无 titlebar): 80×24. T-0504 起 cell rows 减为 22 给 titlebar
        // 让出顶部 28 px logical 空间.
        assert_eq!(
            cells_from_surface_px(super::INITIAL_WIDTH, super::INITIAL_HEIGHT),
            (80, 22),
            "800×600 - titlebar 28 → cell 80×22"
        );
    }

    #[test]
    fn cells_from_surface_px_grows_with_surface() {
        // 拖大窗口能多显示 cells (T-0306 acceptance 核心)
        // T-0504: usable_h = h - 28, rows = usable_h / 25.
        // 1200 - 28 = 1172, 1172 / 25 = 46.
        assert_eq!(
            cells_from_surface_px(1600, 1200),
            (160, 46),
            "1600×1200 - titlebar 28 → 160×46"
        );
        // 1080 - 28 = 1052, 1052 / 25 = 42.
        assert_eq!(
            cells_from_surface_px(1920, 1080),
            (192, 42),
            "1920×1080 - titlebar 28 → 192×42"
        );
    }

    #[test]
    fn cells_from_surface_px_clamps_zero_to_one() {
        // 极小 surface 整数除给 0, max(1) 兜底。term/pty 都不接受 0 维度。
        assert_eq!(cells_from_surface_px(0, 0), (1, 1), "0×0 应 clamp 到 1×1");
        assert_eq!(
            cells_from_surface_px(5, 10),
            (1, 1),
            "5px (< CELL_W_PX=10) 应 clamp col=1"
        );
        // T-0504: 极小 height 触发 saturating_sub → usable_h = 0, rows clamp 到 1.
        assert_eq!(
            cells_from_surface_px(20, 5),
            (2, 1),
            "5px (< titlebar 28) 应 clamp row=1"
        );
        // T-0504: height 正好 = titlebar (28) → usable_h = 0, rows clamp 到 1.
        assert_eq!(
            cells_from_surface_px(20, 28),
            (2, 1),
            "height = titlebar 应 clamp row=1"
        );
    }

    #[test]
    fn cells_from_surface_px_truncates_partial_cells() {
        // 整数除截断, 余下边距 Phase 4 再细化 (派单允许)。
        // 805px / 10 = 80 cells (剩 5px 边距), 不向上取 81。
        // T-0504: usable_h = 612 - 28 = 584, 584 / 25 = 23 行.
        assert_eq!(
            cells_from_surface_px(805, 612),
            (80, 23),
            "余数应被截断 + titlebar 28 减让"
        );
    }

    // ---------- T-0399 P2-6 should_propagate_resize 纯逻辑单测 ----------
    // propagate_resize_if_dirty 含 wgpu/PtyHandle/TermState 三大 owned 资源,
    // headless 单测构造成本太高 (审码 P2-6 派单允许抽决策点纯 fn 测)。本组
    // 锁住"dirty 决定是否消费一轮"这条 INV-006 关键不变式 — 上层 (T-0306
    // propagate_resize_if_dirty) 改 early-return 条件时, 这两条单测会拦回归。

    #[test]
    fn should_propagate_resize_returns_true_when_dirty() {
        // INV-006 置位路径:handle_event(Configure) 在尺寸变化时置 dirty=true,
        // 紧接 propagate_resize_if_dirty 应消费一轮 (renderer/term/pty 三方
        // resize + 清 dirty)。
        assert!(should_propagate_resize(true), "dirty=true 时应消费一轮");
    }

    #[test]
    fn should_propagate_resize_returns_false_when_clean() {
        // INV-006 早返路径:无 resize event 时 dirty=false, propagate 应早返
        // 不动 renderer/term/pty (避免空跑 wgpu surface.configure / TIOCSWINSZ
        // ioctl, 与 INV-006 "布尔脏标记不是队列" 语义对齐)。
        assert!(
            !should_propagate_resize(false),
            "dirty=false 时应早返不消费"
        );
    }

    // ---------- T-0502 verdict_for_scale 纯逻辑单测 ----------
    // OutputHandler 收到 wl_output.scale event 后, log_output_scale 内部用
    // verdict_for_scale 决策 trace 走 debug (匹配) 还是 warn (不匹配)。决策
    // 抽出来便于 headless 单测覆盖, 与 cells_from_surface_px / should_propagate_resize
    // 同套路 (conventions §3)。

    #[test]
    fn verdict_for_scale_match_when_compositor_equals_hardcode() {
        // 用户 224 ppi mutter scale=2 + HIDPI_SCALE=2 一致, 走 debug 路径。
        assert_eq!(verdict_for_scale(2, 2), OutputScaleVerdict::Match);
        // 边界:1=1 也算 match (假想低 ppi 显示器场景, 但派单 hardcode=2 不会触发,
        // 仅锁住"compositor==our 必返 Match"决策)。
        assert_eq!(verdict_for_scale(1, 1), OutputScaleVerdict::Match);
    }

    #[test]
    fn verdict_for_scale_mismatch_when_compositor_differs() {
        // 用户插了 96 ppi 副屏 (scale=1) + HIDPI_SCALE=2, 应触发 warn。
        assert_eq!(verdict_for_scale(1, 2), OutputScaleVerdict::Mismatch);
        // 6K HiDPI 屏 compositor 上报 scale=3 + 我们仍 hardcode=2, warn 提示
        // 用户改 const 重编译 (派单 Out 段不动态适配)。
        assert_eq!(verdict_for_scale(3, 2), OutputScaleVerdict::Mismatch);
        // fractional scale 不属本 ticket scope (派单 Out 段), 但 compositor
        // 上报整数 4 (e.g. 8K) 也走 mismatch 路径。
        assert_eq!(verdict_for_scale(4, 2), OutputScaleVerdict::Mismatch);
    }

    #[test]
    fn verdict_for_scale_hardcode_locks_to_hidpi_scale_constant() {
        // T-0502 设计 invariant: log_output_scale 用 `crate::wl::HIDPI_SCALE as i32`
        // 作 our_scale 实参。HIDPI_SCALE 是 const u32 = 2 (T-0404 设, ROADMAP
        // 永久不接动态 wl_output.scale)。本测固化"hardcode 实参 = HIDPI_SCALE"
        // 这条耦合, 若未来 HIDPI_SCALE 改 (例如新 ticket 升 3x) 而 log_output_scale
        // 忘改 our_scale 入参, 本测会拦回归。
        let our_scale = crate::wl::HIDPI_SCALE as i32;
        assert_eq!(our_scale, 2, "HIDPI_SCALE 应为 2 (T-0404 hardcode)");
        assert_eq!(
            verdict_for_scale(our_scale, our_scale),
            OutputScaleVerdict::Match,
            "compositor 上报 scale 与 HIDPI_SCALE 一致时必走 debug"
        );
    }

    // ---------- T-0503 decoration_log_decision 纯逻辑单测 ----------
    // 抽 enum 转换 + 纯 fn 测 (conventions §3 + INV-010 类型隔离实践)。
    // sctk DecorationMode 升级加 variant 时, exhaustive match 在 callsite 编译
    // 期 catches; 本组单测固化"两种已知 variant → 两种 log 决策"的映射不漂移。

    #[test]
    fn decoration_log_decision_server_is_accepted() {
        // KDE / wlroots / Hyprland 等支持 SSD 的 compositor, 协商成功 → info log
        // "got titlebar"。锁住"Server → ServerSideAccepted"映射。
        assert_eq!(
            decoration_log_decision(DecorationMode::Server),
            DecorationLogDecision::ServerSideAccepted,
        );
    }

    #[test]
    fn decoration_log_decision_client_is_fallback() {
        // GNOME mutter (无 zxdg_decoration_manager_v1) 或拒绝 SSD 的 compositor,
        // 协商失败 → warn log "no titlebar, quill 不自画 CSD"。
        // sctk 文档明示: manager 不存在时 decoration_mode 字段恒 Client。
        assert_eq!(
            decoration_log_decision(DecorationMode::Client),
            DecorationLogDecision::ClientSideFallback,
        );
    }

    #[test]
    fn window_core_decoration_logged_starts_false() {
        // WindowCore::new 初始 decoration_logged=false; configure 首次 fire 后
        // 置 true, 后续 configure 不重复 log。锁住"一次性 log"语义不漂移。
        let core = WindowCore::new(800, 600);
        assert!(
            !core.decoration_logged,
            "新建 WindowCore 应未 log 过装饰协商"
        );
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
