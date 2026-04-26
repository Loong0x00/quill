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
use std::os::fd::{AsFd, AsRawFd, BorrowedFd};
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use calloop::generic::Generic;
use calloop::signals::{Signal, Signals};
use calloop::timer::{TimeoutAction, Timer};
use calloop::{EventLoop, Interest, LoopHandle, LoopSignal, Mode, PostAction, RegistrationToken};

use crate::pty::PtyHandle;
use crate::tab::{TabInstance, TabList};
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

use super::dnd::{build_drop_command, parse_uri_list};
use super::keyboard::{handle_key_event, KeyboardAction, KeyboardState};
use super::pointer::{
    handle_pointer_event, quill_edge_to_wayland, xcursor_names_for, CursorShape, PointerAction,
    PointerState, WindowButton,
};
use super::render::Renderer;
use super::selection::{bracketed_paste_wrap, extract_selection_text, PasteSource, SelectionState};
use crate::ime::{handle_text_input_event, CursorRectangle, ImeAction, ImeState};
use wayland_client::protocol::wl_pointer;
use wayland_client::protocol::{
    wl_data_device::{self, WlDataDevice},
    wl_data_device_manager::{self, WlDataDeviceManager},
    wl_data_offer::{self, WlDataOffer},
    wl_data_source::{self, WlDataSource},
    wl_shm::{self, WlShm},
};
use wayland_cursor::CursorTheme;
use wayland_protocols::wp::primary_selection::zv1::client::{
    zwp_primary_selection_device_manager_v1::{self, ZwpPrimarySelectionDeviceManagerV1},
    zwp_primary_selection_device_v1::{self, ZwpPrimarySelectionDeviceV1},
    zwp_primary_selection_offer_v1::{self, ZwpPrimarySelectionOfferV1},
    zwp_primary_selection_source_v1::{self, ZwpPrimarySelectionSourceV1},
};
use wayland_protocols::wp::text_input::zv3::client::{
    zwp_text_input_manager_v3::{self, ZwpTextInputManagerV3},
    zwp_text_input_v3::{self, ZwpTextInputV3},
};

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

/// T-0603: keyboard repeat 调度请求 — `Dispatch<WlKeyboard>` 把
/// [`KeyboardAction::StartRepeat`] / [`KeyboardAction::StopRepeat`] 映射
/// 为本 enum 写到 `state.pending_repeat`, 由 [`drive_wayland`] 在
/// `dispatch_pending` 之后调 [`apply_repeat_request`] 真消费 (那里能拿到
/// `&mut LoopData` → `loop_handle` + `repeat_token`, Dispatch 路径只能
/// 拿到 `&mut State` 拿不到 LoopHandle, 所以分两阶段).
///
/// 与 `core.resize_dirty` (INV-006) 同套路: 协议事件路径只 set 单次延迟
/// 请求, dispatch 之后单一上游消费者 propagate 到副作用.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RepeatScheduleRequest {
    /// `StartRepeat` 收到: cancel 已注册 timer (若有) + insert 新
    /// `Timer::from_duration(Duration::from_millis(delay))`. callback 走
    /// [`tick_repeat`] (在 LoopData 维度) 拿字节写 PTY + 返
    /// `TimeoutAction::ToDuration(Duration::from_millis(1000 / rate))` 自动
    /// reschedule. rate=0 (compositor 禁 repeat) → 不 schedule (apply 时检查).
    Start,
    /// `StopRepeat` 收到: cancel 已注册 timer (若有). callback 即使下次仍
    /// fire (race), `keyboard_state.tick_repeat()` 此时返 None 走
    /// `TimeoutAction::Drop` 双保险.
    Stop,
}

/// T-0607: 选区操作请求. `Dispatch<WlPointer>` / `Dispatch<WlKeyboard>` 在
/// 收到 SelectionEnd / Ctrl+Shift+C 时写到 `state.pending_selection_op`,
/// drive_wayland step 3.7 真消费 (那里能拿 LoopData 字段 + wayland 协议
/// handle).
///
/// 与 `core.resize_dirty` (INV-006) / `pending_repeat` (T-0603) 同套路 — 协议事
/// 件路径只 set 单次延迟请求, 单一上游消费者推副作用 (协议 set_selection
/// request).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SelectionOp {
    /// SelectionEnd → 算选区文本, 创建 wp_primary_selection_source_v1 + offer
    /// text/plain mime, set_selection on primary device.
    SetPrimary,
    /// Ctrl+Shift+C → 算选区文本, 创建 wl_data_source + offer text/plain mime,
    /// set_selection on data_device.
    SetClipboard,
}

/// T-0607: autoscroll 请求. `Dispatch<WlPointer>` 在 AutoScrollStart 时写到
/// `state.pending_autoscroll_op`, drive_wayland step 3.7 真 insert calloop
/// Timer source.
///
/// 与 `pending_resize_followup` (T-0802) / `repeat_token` (T-0603) 同 calloop
/// Timer 单飞行套路.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AutoScrollOp {
    /// 启动 Timer, 100ms 一次 fire 调用 [`autoscroll_tick`] → scroll_display(delta)
    /// + cursor 跟 (selection_state.update). delta = ±1 (来自 PointerAction).
    Start { delta: i32 },
}

/// T-0608: 待处理 tab 操作. Dispatch 路径 (拿 &mut State) 写, drive_wayland
/// step 3.8 (拿 &mut LoopData, 含 loop_handle) 真消费 — 与
/// `apply_selection_op` / `apply_repeat_request` 同套路.
///
/// **why 推迟**: 新 tab 需要 spawn PTY + insert calloop source (loop_handle 在
/// LoopData 不在 State); close tab 需要 remove calloop source (同). Dispatch
/// 内只能拿到 &mut State, 推迟到 drive_wayland 后单一消费者一次性处理.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TabOp {
    /// 新建 tab (Ctrl+T / + 按钮 / NewTab PointerAction). drive_wayland 调
    /// [`apply_tab_op`] spawn 子 shell + register PTY fd + push TabList +
    /// 切到新 tab.
    New,
    /// 关 active tab (Ctrl+W).
    CloseActive,
    /// 关指定 idx tab (close × 按钮).
    Close(usize),
    /// 下一个 tab (Ctrl+Tab).
    Next,
    /// 上一个 tab (Ctrl+Shift+Tab).
    Prev,
    /// 切到 idx tab. Ctrl+1..9 / 鼠标 click tab body.
    Switch(usize),
    /// 拖拽 reorder. drive_wayland 拿到当前鼠标 x_logical 算 target_idx,
    /// swap_reorder. (origin_idx 字段无 — drag_active=true 时 tab_press 已记录
    /// origin, 但本枚举保持无状态化让 drive_wayland 一次性走完所有 PointerState.tab_press.)
    Reorder { from_idx: usize, target_idx: usize },
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
    /// T-0603 keyboard repeat: clone 自 `event_loop.handle()`, 用于在
    /// [`apply_repeat_request`] 路径动态 insert / remove `Timer` source.
    /// 单线程 calloop 下 `LoopHandle` 是 `!Send + !Sync`, 无并发问题.
    /// `'static`: LoopData 自身 owned, 不引用任何外部生命周期, 与
    /// `EventLoop<'static, LoopData>` 一致.
    ///
    /// **drop 顺序**: LoopHandle clone 内部是 `Rc` 引用计数, drop 时仅减计数,
    /// 不影响 EventLoop 本体 (EventLoop 在 run_window scope 持本体), 放尾部
    /// 与 frame_stats / text_system 同性质 — 无 wgpu / wayland 裸指针.
    loop_handle: LoopHandle<'static, LoopData>,
    /// T-0603: 当前注册的 keyboard repeat timer 句柄. None = 无 repeat 进行,
    /// Some = 已 insert 一个 `Timer::from_duration` source, 需 remove 才停.
    /// [`apply_repeat_request`] 在 Start 路径先 take 旧 token + remove, 再 insert
    /// 新 timer; Stop 路径仅 take + remove. timer fire 时若 `tick_repeat=None`
    /// 走 `TimeoutAction::Drop` 自然清, callback 内不能再 remove 自己 (calloop
    /// 文档警告), token 仍留 Some 状态由下次 apply 清理 (race 安全 — 已 Drop
    /// 的 token remove 返 Err 我们吞掉).
    repeat_token: Option<RegistrationToken>,
    /// T-0802 In #B: 上次 [`propagate_resize_if_dirty`] **真消费** 一轮 (走完
    /// renderer/term/pty 三方 resize + 清 dirty) 的时刻. None = 还没消费过 (首次
    /// configure 前). 节流决策 [`should_throttle_propagate`] 用 `now -
    /// last_propagate_at < RESIZE_THROTTLE_MS` 判跳过; 拖窗口高频 configure 期
    /// 大部分被本字段挡掉, 单次 propagate 占 GPU/term/pty 不被相邻 cost 撞穿.
    /// POD (Option<Instant>), drop 顺序无关.
    last_propagate_at: Option<Instant>,
    /// T-0802 In #B: 节流期间 dirty 留挂时, schedule 一个 `Timer::from_duration`
    /// 兜底在 throttle 窗结束后 fire 一次 [`propagate_resize_if_dirty`], 防"拖
    /// 动停后最后一个 configure 卡在节流窗内不消费, 窗口卡在错尺寸"问题. None
    /// = 无兜底 Timer pending; Some = 已 insert (后续节流命中不重复 schedule, 单
    /// 飞行原则). callback fire 后 [`resize_followup_tick`] take 此 token (timer
    /// 自身 Drop), 真 propagate 走完后 last_propagate_at 更新, 下次 dirty 来时
    /// 再 schedule. 与 `repeat_token` 同 calloop Timer 单飞行套路 (T-0603 模式).
    pending_resize_followup: Option<RegistrationToken>,
    /// T-0607: autoscroll Timer 句柄. None = 无 autoscroll, Some = 已 insert
    /// 一个 100ms 重复 Timer source. Start 路径先 take 旧 + remove, 再 insert
    /// 新; Stop / SelectionEnd 路径仅 take + remove. 与 `repeat_token` /
    /// `pending_resize_followup` 同 calloop Timer 单飞行套路 (派单 In #E).
    pending_autoscroll_timer: Option<RegistrationToken>,
    /// T-0607: 当前 autoscroll 方向 (`±1` line / 100ms tick). Timer fire 走
    /// [`autoscroll_tick`] 读此值调 scroll_display + 同步 selection_state cursor.
    autoscroll_delta: i32,
    /// T-0608: 每 tab 一个 PTY fd 注册到 calloop EventLoop (INV-005). 用 tab id
    /// 索引 RegistrationToken, close tab 时按 id 找 token + remove. 新 tab
    /// 注册后 push 一对 (id, token).
    /// 主路径 (active tab 切换) **不**重新注册 — fd 一直挂着, callback 内
    /// 直接 read 字节; 切 active 仅改 state.tabs.active 索引, 不动 fd 注册.
    pty_tokens: Vec<(crate::tab::TabId, RegistrationToken)>,
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
/// T-0608: pty_read_tick 重构 — 接 tab_id 而非裸 pty/term, 内部按 id 索引
/// state.tabs 的 TabInstance, 拿其 term + pty. 多 tab 模式下每个 PTY fd 注册
/// 时 closure 携带自己的 tab_id, callback fire 时定位到对应 tab.
///
/// **why tab_id 而非 idx**: idx 在 reorder / close 后会变, id 单调全局唯一不变
/// (派单 In #F anchor 锁定).
fn pty_read_tick(
    state: &mut State,
    tab_id: crate::tab::TabId,
    loop_signal: &LoopSignal,
) -> std::io::Result<PostAction> {
    // 找对应 tab. close race 下可能找不到 (Remove 已发但 calloop fire 队列还有
    // 一次), 走 PostAction::Remove 让 calloop 自然清理.
    let Some(tab_idx) = state.tabs_unchecked().idx_of(tab_id) else {
        tracing::trace!(target: "quill::pty", id=tab_id.raw(), "pty_read_tick: tab gone (race)");
        return Ok(PostAction::Remove);
    };
    let Some(tab) = state.tabs_unchecked_mut().get_mut(tab_idx) else {
        return Ok(PostAction::Remove);
    };
    let (term, pty) = tab.split_term_pty();
    pty_read_tick_inner(pty, Some(term), loop_signal)
}

/// T-0608 inner impl: 真 read PTY + advance term. 与原 pty_read_tick 同, 抽出
/// 让 tab_id 定位逻辑独立 (上一段 fn).
fn pty_read_tick_inner(
    pty: &mut PtyHandle,
    mut term: Option<&mut crate::term::TermState>,
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
///
/// **T-0617**: `tab_count` 入参决定 tab bar 是否占用顶部空间. 单 tab (count=1)
/// 时 tab bar 隐藏 (派单 In #B "单 tab 隐藏 tab bar / 终端内容直接接 titlebar
/// 下方"), top_reserved 仅 titlebar; 多 tab (count >= 2) 仍走 titlebar +
/// tab_bar. 与 [`tab_bar_h_logical_for`] 同决策, render 与 hit_test 路径
/// 走同一根 helper 保视觉与逻辑同步.
pub(crate) fn cells_from_surface_px(width: u32, height: u32, tab_count: usize) -> (usize, usize) {
    // T-0608/T-0617: 顶部占用 = titlebar + tab_bar (单 tab 时 tab_bar=0).
    let top_reserved = crate::wl::render::TITLEBAR_H_LOGICAL_PX + tab_bar_h_logical_for(tab_count);
    let usable_h = height.saturating_sub(top_reserved);
    let cols = ((width as f32) / crate::wl::render::CELL_W_PX) as usize;
    let rows = ((usable_h as f32) / crate::wl::render::CELL_H_PX) as usize;
    (cols.max(1), rows.max(1))
}

/// **T-0617**: 给定 tab 数量, 返 tab bar 占用的 logical px 高度.
///
/// 单 tab (count=1) 时 tab bar 隐藏返 0 — 派单 In #B 视觉规则与 ghostty 一致
/// (单 tab 不画孤零零的 + 按钮 + 空 tab 槽); 多 tab (count >= 2) 返
/// [`crate::wl::render::TAB_BAR_H_LOGICAL_PX`].
///
/// **why pure fn**: render (`Renderer::draw_frame` / `render_headless`) /
/// hit_test (`hit_test_with_tabs`) / cells_from_surface_px 三处都要这套决策,
/// 单一来源防回归. 与 `tab_body_width` / `circular_hit` 同 conventions §3
/// 抽决策状态机套路.
pub(crate) fn tab_bar_h_logical_for(tab_count: usize) -> u32 {
    if tab_count >= 2 {
        crate::wl::render::TAB_BAR_H_LOGICAL_PX
    } else {
        0
    }
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

/// T-0802 In #B: resize propagate 节流最小间隔 (60ms ≈ 16.7 Hz).
///
/// **why 60ms**: 派单 In #B 测算: compositor 拖动期间 60Hz+ configure event
/// (~16ms 间隔); 单帧 resize cost ≈ wgpu Surface::configure 重建 SwapChain
/// (10ms) + Term::resize grid 重排 (5ms) + pty TIOCSWINSZ ioctl (1ms) ≈
/// 15-20ms, 累积 lag 视觉感受明显. 60ms 节流让 propagate ≤ 16.7 Hz, 单次
/// resize 占帧不超 33% (60ms 内最多一次), 拖动手感跟随 + GPU/term/pty 不被
/// 撞穿. 30ms 太短 (16ms configure + 20ms resize cost 仍重叠), 100ms 太长
/// (用户可见延迟).
///
/// **dirty 不丢**: 节流期间 `core.resize_dirty` 仍 true (INV-006 单 bool 幂等),
/// 下次 [`propagate_resize_if_dirty`] 调用 (距上次 ≥ 60ms 时) 必消费. 拖动停
/// 后最后一个 configure 在节流窗内 → 由 calloop Timer 兜底 fire (见
/// [`schedule_resize_followup_timer`]) 保证最终 size 不丢.
const RESIZE_THROTTLE_MS: u64 = 60;

/// T-0802 In #B: 给定当前时刻 + 上次成功 propagate 时刻 + 最小间隔, 算节流是否
/// 命中 (true = 应跳过本次 propagate, dirty 留给下次).
///
/// **why 抽纯 fn**: 副作用 (3 方 resize + 清 dirty + 记 last_propagate_at + 调
/// LoopHandle.insert_source 排兜底 Timer) 在 [`propagate_resize_if_dirty`] /
/// [`schedule_resize_followup_timer`], 决策本身是纯 (now / last / interval) →
/// bool. 与 `should_propagate_resize` / `pty_readable_action` /
/// `schedule_op_for_request` 同 conventions §3 抽决策状态机套路, 单测可走所有
/// 分支不需起 wayland.
///
/// **first call 路径**: `last == None` → 永不节流 (首次 propagate 立即跑, 不
/// 等 60ms; 拖动起手即响应).
///
/// **monotonic 不抖**: `Instant` 是单调递增 (Rust std 文档保证), 即使系统时间
/// 跳变也不会 panic / 误判; 极端边界 `now < last` 不可能发生 (would-be panic
/// 在 `now.duration_since(last)` 文档说会 saturating 至 zero, 我们用 `checked_duration_since`
/// 双保险).
pub(crate) fn should_throttle_propagate(
    now: Instant,
    last: Option<Instant>,
    min_interval: Duration,
) -> bool {
    match last {
        None => false,
        Some(last_t) => {
            let elapsed = now.checked_duration_since(last_t).unwrap_or(Duration::ZERO);
            elapsed < min_interval
        }
    }
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
///
/// **T-0802 In #B 节流**: dispatch_pending 中可能多次 `WindowHandler::configure`
/// 置 dirty (拖窗口高频 60Hz+ configure), 本 fn 走 [`should_throttle_propagate`]
/// 判: 距上次成功 propagate < `RESIZE_THROTTLE_MS` (60ms) 跳过, 让 dirty 留给
/// 下次. 节流跳过时调 [`schedule_resize_followup_timer`] 排兜底 calloop Timer
/// 在 throttle 窗结束后 fire 一次, 防"拖动停后最后一个 configure 卡节流窗内"卡
/// 死. INV-006 不破: dirty 仍布尔幂等, 节流仅限 propagate 调用频率, 不修改 dirty
/// 状态机.
fn propagate_resize_if_dirty(data: &mut LoopData) {
    if !should_propagate_resize(data.state.core.resize_dirty) {
        return;
    }
    // T-0802 In #B: 节流命中 → 留 dirty, 排兜底 Timer (单飞行, 已有则不重排).
    let now = Instant::now();
    if should_throttle_propagate(
        now,
        data.last_propagate_at,
        Duration::from_millis(RESIZE_THROTTLE_MS),
    ) {
        schedule_resize_followup_timer(data, now);
        return;
    }
    let LoopData { state, .. } = &mut *data;

    let width = state.core.width;
    let height = state.core.height;
    // T-0617: 单 tab 时 tab bar 隐藏 (高 0), cell 区扩到 titlebar 下方;
    // 多 tab 仍 titlebar + tab_bar 双层. tab_count 走 state.tabs (启动期保证非空).
    let tab_count = state.tabs.as_ref().map(|t| t.len()).unwrap_or(1);
    let (cols, rows) = cells_from_surface_px(width, height, tab_count);

    if let Some(r) = state.renderer.as_mut() {
        r.resize(width, height);
    }

    // T-0608: 全 tabs 都 resize. inactive tab 的 term grid + PTY winsize 跟随
    // viewport 大小, 切回时立即正确 (派单 In #B). 全部 tab 一起 ioctl 是 O(N),
    // 实战 N ≤ 10 不慢.
    for tab in state.tabs_unchecked_mut().iter_mut() {
        tab.term_mut().resize(cols, rows);
        if let Err(err) = tab.pty().resize(cols as u16, rows as u16) {
            tracing::warn!(
                ?err,
                cols,
                rows,
                tab_id = tab.id().raw(),
                "pty.resize ioctl 失败, shell 不会收 SIGWINCH"
            );
        }
    }

    // T-0504: 同步 PointerState 的 surface 尺寸 (logical px), 让 hit_test 用
    // 最新尺寸算按钮位置 (按钮在右上角, 拖窗口时按钮跟着移).
    state.pointer_state.set_surface_size(width, height);
    // T-0607: 同步 cell grid 尺寸 (cols, rows), 让 pixel_to_cell clamp 边界
    // 用最新值. cells_from_surface_px 已减 titlebar, 与 PointerState 内部
    // px → cell 的 titlebar 偏移一致.
    state.pointer_state.set_cell_grid(cols, rows);

    state.core.resize_dirty = false;
    // T-0802 In #B: 记本次 propagate 时刻, 下次 should_throttle_propagate 据此判.
    data.last_propagate_at = Some(now);
    tracing::debug!(
        width,
        height,
        cols,
        rows,
        "propagated resize → renderer + term + pty + pointer"
    );
}

/// T-0802 In #B: 节流命中时排兜底 calloop Timer — 在 throttle 窗结束后 fire 一
/// 次 [`resize_followup_tick`], 防"拖动停后最后一个 configure 卡节流窗内不消费,
/// 窗口卡在错尺寸"问题.
///
/// **单飞行**: `data.pending_resize_followup.is_some()` 直接返 (已有 Timer 排了,
/// 不重复 schedule). callback fire 时 take 此字段 (Drop timer), 走完 propagate
/// 后 last_propagate_at 更新, 下次 dirty 来时再 schedule.
///
/// **delay 算法**: 让 Timer fire 在 `last_propagate_at + RESIZE_THROTTLE_MS` 时
/// 刻, 即 `now - last_propagate_at` 已耗 `elapsed`, Timer 还需等 `min_interval -
/// elapsed`. 极端边界 (last=None / elapsed >= min_interval) 在 should_throttle_propagate
/// 早返 false 不到本 fn, 这里 last 必 Some 且 elapsed < min_interval.
///
/// **why calloop Timer**: INV-005 单线程 EventLoop 唯一 IO 调度器, Timer source
/// 与 wayland fd / pty fd / signalfd 同 EventLoop, 不起 thread. 与 T-0603 keyboard
/// repeat 同套路.
fn schedule_resize_followup_timer(data: &mut LoopData, now: Instant) {
    if data.pending_resize_followup.is_some() {
        return;
    }
    let last = match data.last_propagate_at {
        Some(t) => t,
        None => {
            // Defensive: should_throttle_propagate 在 last=None 早返 false 不到这里.
            // 真到这里 (上游变化 / race) 走 1ms 兜底立即 fire 一次保证 dirty 不丢.
            tracing::warn!("schedule_resize_followup_timer: last=None 防御性 1ms 兜底");
            now
        }
    };
    let elapsed = now.checked_duration_since(last).unwrap_or(Duration::ZERO);
    let min_interval = Duration::from_millis(RESIZE_THROTTLE_MS);
    let remaining = min_interval.checked_sub(elapsed).unwrap_or(Duration::ZERO);
    // remaining=0 极端边界: 直接走 1ms 给 calloop 一帧缓冲 (与 T-0603 delay.max(1)
    // 同思路, 防 0-duration Timer fire 与本帧 dispatch 重叠).
    let delay = remaining.max(Duration::from_millis(1));
    let timer = Timer::from_duration(delay);
    match data
        .loop_handle
        .insert_source(timer, |_deadline, _meta, data: &mut LoopData| {
            resize_followup_tick(data)
        }) {
        Ok(token) => {
            data.pending_resize_followup = Some(token);
            tracing::trace!(
                target: "quill::resize",
                delay_ms = delay.as_millis() as u64,
                "resize followup timer scheduled"
            );
        }
        Err(err) => {
            tracing::warn!(
                target: "quill::resize",
                ?err,
                "calloop insert_source(resize followup) 失败 — 节流可能卡尺寸 \
                 (用户下次拖动再触发新 configure 解套, quill 继续跑)"
            );
        }
    }
}

/// T-0802 In #B: 兜底 Timer fire 时跑一次 propagate. take 字段清 single-flight
/// 标记 (callback 内不能再 remove 自己 — calloop 文档警告, 走 `TimeoutAction::Drop`
/// 让 calloop 自己释放), 然后调 [`propagate_resize_if_dirty`]; 此时距上次
/// propagate 必 ≥ `RESIZE_THROTTLE_MS` (Timer fire 时机即此), 走完 last_propagate_at
/// 更新, 下次 dirty 来时再 schedule.
///
/// **dirty 已被消化的 race**: callback fire 之间若主路径 `drive_wayland` 在
/// 60ms 后已自然跑过 propagate (e.g. 新 configure / pty event 唤醒), dirty 已清,
/// `propagate_resize_if_dirty` 内 `should_propagate_resize=false` 早返, 本 callback
/// 等于 noop, 行为安全.
fn resize_followup_tick(data: &mut LoopData) -> TimeoutAction {
    data.pending_resize_followup = None;
    propagate_resize_if_dirty(data);
    TimeoutAction::Drop
}

/// T-0603: 一次 keyboard repeat schedule 的最小子集决策 — 给定当前是否有
/// 旧 token + 是否要 schedule 新 timer + (rate, delay), 算出应执行的 op 序列.
///
/// 抽 enum 而非直接在 [`apply_repeat_request`] 内 if-else 是 conventions §3
/// 抽状态机模式 (T-0107 WindowAction / T-0205 PtyAction / T-0501 KeyboardAction
/// 同套路). 真 LoopHandle / RegistrationToken 操作有副作用且需 LoopData 借用,
/// 决策本身是纯逻辑 (输入: Option<bool>=有无旧 token, 是否 Start, rate),
/// 单测覆盖 4 个 case (Stop+无旧 / Stop+有旧 / Start+rate=0 / Start+rate>0).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RepeatScheduleOp {
    /// 无操作 (例: Stop 但本来就没 timer; rate=0 时 Start 不 schedule 但也无旧).
    Noop,
    /// 仅 cancel 旧 timer (Stop 且有旧, 或 Start+rate=0 但有旧需先停).
    CancelOnly,
    /// cancel 旧 timer (若有) + insert 新 timer (Start + rate>0).
    CancelAndStart,
}

/// T-0603: 决定一次 [`apply_repeat_request`] 调用要执行的操作.
///
/// 输入:
/// - `request`: dispatch 期间 `Dispatch<WlKeyboard>` 写入的请求
/// - `has_old_token`: LoopData 是否持有旧 RegistrationToken
/// - `rate`: wl_keyboard `RepeatInfo` 给的 rate (keys/sec, 0=禁用)
///
/// rate=0 (compositor 禁 repeat, 例: 用户 GNOME 设置 disable repeat) 时
/// Start 退化为 CancelOnly (清旧 timer 即可, 不 schedule 新).
pub(crate) fn schedule_op_for_request(
    request: &RepeatScheduleRequest,
    has_old_token: bool,
    rate: i32,
) -> RepeatScheduleOp {
    match request {
        RepeatScheduleRequest::Stop => {
            if has_old_token {
                RepeatScheduleOp::CancelOnly
            } else {
                RepeatScheduleOp::Noop
            }
        }
        RepeatScheduleRequest::Start => {
            if rate <= 0 {
                if has_old_token {
                    RepeatScheduleOp::CancelOnly
                } else {
                    RepeatScheduleOp::Noop
                }
            } else {
                RepeatScheduleOp::CancelAndStart
            }
        }
    }
}

/// T-0603: 消费 `state.pending_repeat` (Dispatch<WlKeyboard> 写入), 把请求
/// 真翻译成 calloop Timer source 的 insert / remove. 与
/// [`propagate_resize_if_dirty`] 同套路 — 协议事件路径只 set 单次延迟请求,
/// 单一上游消费者 (drive_wayland step 3.6) 推副作用.
///
/// **why 在 dispatch_pending 后**: Dispatch 路径只能拿到 `&mut State`, 拿
/// 不到 `LoopHandle` (在 LoopData 里); LoopHandle insert_source 也不能在
/// timer callback 内对自己 remove (calloop 文档警告). 把动作推迟到
/// drive_wayland 的 step 3.6, `&mut LoopData` 字段 (loop_handle / repeat_token)
/// 全可访问 — 单一消费者一次性处理.
///
/// **rate / delay 来源**: 走 `state.keyboard_state.repeat_info()`. wl_keyboard
/// `RepeatInfo` 协议在 v4+ keymap 之后 fire (之前 protocol 无 repeat_info,
/// 老 compositor 退化到 rate=0 不 schedule); 默认值 rate=0 / delay=0 起步,
/// compositor 推 RepeatInfo 后更新. INV-005: Timer source 与其它 IO fd 同
/// 一 EventLoop, 不起 thread.
fn apply_repeat_request(data: &mut LoopData) {
    let Some(request) = data.state.pending_repeat.take() else {
        return;
    };
    let (rate, delay) = data.state.keyboard_state.repeat_info();
    let has_old_token = data.repeat_token.is_some();
    let op = schedule_op_for_request(&request, has_old_token, rate);
    match op {
        RepeatScheduleOp::Noop => {
            tracing::trace!(
                target: "quill::keyboard",
                ?request,
                rate,
                "repeat schedule noop"
            );
        }
        RepeatScheduleOp::CancelOnly => {
            if let Some(tok) = data.repeat_token.take() {
                data.loop_handle.remove(tok);
                tracing::debug!(
                    target: "quill::keyboard",
                    ?request,
                    "repeat timer cancelled"
                );
            }
        }
        RepeatScheduleOp::CancelAndStart => {
            if let Some(tok) = data.repeat_token.take() {
                data.loop_handle.remove(tok);
            }
            // delay <= 0 (协议怪异 / 测试边界): 用 1ms 兜底, 至少给一帧
            // 缓冲不立即 fire.
            let delay_ms = delay.max(1) as u64;
            let timer = Timer::from_duration(std::time::Duration::from_millis(delay_ms));
            match data
                .loop_handle
                .insert_source(timer, |_deadline, _meta, data: &mut LoopData| {
                    repeat_timer_tick(data)
                }) {
                Ok(token) => {
                    data.repeat_token = Some(token);
                    tracing::debug!(
                        target: "quill::keyboard",
                        rate,
                        delay_ms,
                        "repeat timer scheduled"
                    );
                }
                Err(err) => {
                    tracing::warn!(
                        target: "quill::keyboard",
                        ?err,
                        "calloop insert_source(repeat timer) 失败 — repeat 不工作但 quill 继续跑"
                    );
                }
            }
        }
    }
}

/// T-0603 calloop Timer callback: timer fire 时取当前 repeat 字节写 PTY,
/// 返 `TimeoutAction::ToDuration(1000/rate ms)` 自动 reschedule 让 calloop
/// 持续触发, 直到 `keyboard_state.tick_repeat() == None` 走 `TimeoutAction::Drop`.
///
/// **why 不在此处 remove repeat_token**: calloop 文档警告 "callback 内不能
/// 对自己的 source 调 remove"; 改用 `TimeoutAction::Drop` 让 calloop 自己
/// 清掉 source. `data.repeat_token` 仍留 Some, 下次 [`apply_repeat_request`]
/// 路径若调 `loop_handle.remove(stale_token)` 会得到 Err (token 已被 calloop
/// 自身 Drop), 代码内吞掉 — race 安全.
///
/// **PTY 错误处理**: 与 `Dispatch<WlKeyboard>` WriteToPty 路径一致 —
/// WouldBlock 丢字节 (背压, INV-005), partial write 丢剩余, 其它 IO 错 warn.
fn repeat_timer_tick(data: &mut LoopData) -> TimeoutAction {
    let bytes = match data.state.keyboard_state.tick_repeat() {
        Some(b) => b,
        None => {
            // Stop 已发但 timer 仍 fire (race): 自然 Drop 终止. apply 路径下次
            // 会清 stale repeat_token (remove Err 吞掉).
            tracing::trace!(
                target: "quill::keyboard",
                "repeat tick: tick_repeat=None, dropping timer"
            );
            return TimeoutAction::Drop;
        }
    };
    // 写 PTY (复用 Dispatch<WlKeyboard> 的相同策略). T-0608: 写 active tab 的
    // PTY (repeat 跟 keyboard focus 走 active tab).
    {
        let pty = data.state.tabs_unchecked().active().pty();
        match pty.write(&bytes) {
            Ok(n) if n == bytes.len() => {
                tracing::trace!(
                    target: "quill::keyboard",
                    n,
                    "repeat tick wrote bytes to pty"
                );
            }
            Ok(n) => {
                tracing::warn!(
                    target: "quill::keyboard",
                    wrote = n,
                    total = bytes.len(),
                    "repeat tick partial write, 剩余字节丢弃 (背压)"
                );
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                tracing::warn!(
                    target: "quill::keyboard",
                    n = bytes.len(),
                    "repeat tick WouldBlock, 字节丢弃 (背压, INV-005 不重试)"
                );
            }
            Err(e) => {
                tracing::warn!(
                    target: "quill::keyboard",
                    error = %e,
                    "repeat tick pty.write 失败 (非 WouldBlock)"
                );
            }
        }
    }
    // reschedule 1000/rate ms 后再 fire. rate=0 兜底走 100ms (与 apply 路径
    // 不 schedule 形成多重防御; 实战 rate 由协议给非 0).
    let (rate, _delay) = data.state.keyboard_state.repeat_info();
    let interval_ms = if rate > 0 {
        (1000 / rate as u64).max(1)
    } else {
        100
    };
    TimeoutAction::ToDuration(std::time::Duration::from_millis(interval_ms))
}

/// T-0607 autoscroll Timer interval. 100ms 一次 fire 调 `term.scroll_display(±1)`.
/// 派单 In #E "Timer fire 100ms 一次".
const AUTOSCROLL_INTERVAL_MS: u64 = 100;

/// T-0607: 消费 `state.pending_autoscroll_op` / `state.pending_autoscroll_cancel`,
/// 真 schedule / cancel calloop Timer. 与 `apply_repeat_request` 同套路 —
/// 协议事件路径只 set 单次延迟请求, 单一上游消费者 (drive_wayland step 3.7)
/// 推副作用.
fn apply_autoscroll_op(data: &mut LoopData) {
    // Cancel 优先于 Start (SelectionEnd 路径同时 set cancel 与 op=None,
    // 防 race 多发 Start).
    if data.state.pending_autoscroll_cancel {
        data.state.pending_autoscroll_cancel = false;
        if let Some(tok) = data.pending_autoscroll_timer.take() {
            data.loop_handle.remove(tok);
            tracing::debug!(target: "quill::pointer", "autoscroll Timer cancelled");
        }
        data.autoscroll_delta = 0;
    }
    let Some(op) = data.state.pending_autoscroll_op.take() else {
        return;
    };
    match op {
        AutoScrollOp::Start { delta } => {
            // 已有 timer (start 重发 — pointer 已守 autoscroll_active 一次, 不
            // 该重 schedule, 但防 race 仍清旧). delta 更新到最新 (用户从下越
            // 切到上越也走此路径).
            if let Some(tok) = data.pending_autoscroll_timer.take() {
                data.loop_handle.remove(tok);
            }
            data.autoscroll_delta = delta;
            let timer =
                Timer::from_duration(std::time::Duration::from_millis(AUTOSCROLL_INTERVAL_MS));
            match data
                .loop_handle
                .insert_source(timer, |_deadline, _meta, data: &mut LoopData| {
                    autoscroll_tick(data)
                }) {
                Ok(token) => {
                    data.pending_autoscroll_timer = Some(token);
                    tracing::debug!(
                        target: "quill::pointer",
                        delta,
                        "autoscroll Timer scheduled (100ms 重复)"
                    );
                }
                Err(err) => {
                    tracing::warn!(
                        target: "quill::pointer",
                        ?err,
                        "calloop insert_source(autoscroll) 失败 — 边缘自动滚 不工作但 quill 继续跑"
                    );
                }
            }
        }
    }
}

/// T-0607: autoscroll Timer fire 一次 — `term.scroll_display(delta)` + cursor
/// 跟随 (selection_state.cursor 不变, 但渲染重画, 用户视觉上看到选区延伸).
///
/// **why 不更新 selection_state cursor 行号**: scroll_display 改 display_offset,
/// alacritty 内部 grid 不变 — cursor 位置仍以"viewport row" 表示, scroll 后视
/// 觉上 cursor 标的是新进入 viewport 的 row. selection_state 本身没动 (anchor
/// / cursor 保持原 row), 但渲染层 selected_cells_linear 走 viewport row 索引
/// 时, scroll 后 anchor 行号若超出 viewport 就部分剪裁 (派单 Out: 跨 scrollback
/// 选择跟随历史 P2). KISS.
fn autoscroll_tick(data: &mut LoopData) -> TimeoutAction {
    let delta = data.autoscroll_delta;
    if delta == 0 {
        // 防御: pending_autoscroll_cancel 路径已清 delta=0, Timer Drop.
        return TimeoutAction::Drop;
    }
    // T-0608: 走 active tab 的 term.
    data.state
        .tabs_unchecked_mut()
        .active_mut()
        .term_mut()
        .scroll_display(delta);
    TimeoutAction::ToDuration(std::time::Duration::from_millis(AUTOSCROLL_INTERVAL_MS))
}

/// T-0607: 消费 `state.pending_selection_op`, 真创建 source + set_selection.
///
/// **PRIMARY** (`SetPrimary`): 仅当 `primary_selection_device` 已绑.
/// **CLIPBOARD** (`SetClipboard`): 仅当 `data_device` 已绑.
///
/// 两路径都先算选区文本 (走 SelectionState + extract_selection_text + display_text),
/// 存到 `state.last_selection_text`, 然后创建 source + offer 4 mime + set_selection.
/// compositor 之后通过 source 的 Send event 反向问我们 (走 Dispatch<...Source>
/// → write_selection_to_fd 真写 fd).
///
/// **selection 文本为空** (用户 click 但没拖, anchor==cursor 单 cell 但 display_text
/// 该 cell 是空格): 仍 set_selection (空字符串), 主流终端同行为 (alacritty / foot
/// click 立即 set 空 PRIMARY).
fn apply_selection_op(data: &mut LoopData, qh: &QueueHandle<State>) {
    let Some(op) = data.state.pending_selection_op.take() else {
        return;
    };
    // 算选区文本. T-0608: 走 active tab 的 term.
    let t = data.state.tabs_unchecked().active().term();
    let (cols, rows) = t.dimensions();
    // T-0607 已知陷阱 (派单 In #G CJK): display_text 内部跳 WIDE_CHAR_SPACER cell,
    // 与 selected_cells_* 走 grid col/row 索引一致 — 选区跨 CJK 字 substr 不切坏.
    let text = extract_selection_text(&data.state.selection_state, cols, rows, |line| {
        t.display_text(line)
    });
    data.state.last_selection_text = Some(text.clone());

    let serial = data.state.pointer_state.last_button_serial();
    match op {
        SelectionOp::SetPrimary => {
            // T-0607 hotfix: PRIMARY 专用 text 字段, 防被 CLIPBOARD 路径覆盖.
            data.state.last_primary_text = Some(text.clone());
            let Some(device) = data.state.primary_selection_device.as_ref() else {
                tracing::trace!(
                    target: "quill::pointer",
                    "SetPrimary 但 device=None (compositor 不导出), 退化跳过"
                );
                return;
            };
            let Some(manager) = data.state.primary_selection_manager.as_ref() else {
                return;
            };
            let source = manager.create_source(qh, ());
            // text/plain;charset=utf-8 (主), text/plain (老 client 兼容),
            // UTF8_STRING (X11 PRIMARY 标准), TEXT (X11 fallback). 与 alacritty
            // / foot / wl-clipboard 兼容.
            for mime in [
                "text/plain;charset=utf-8",
                "text/plain",
                "UTF8_STRING",
                "TEXT",
            ] {
                source.offer(mime.to_string());
            }
            device.set_selection(Some(&source), serial);
            tracing::debug!(
                target: "quill::pointer",
                n = text.len(),
                "PRIMARY set_selection"
            );
            // T-0607 hotfix: store source 防 drop. wayland-client Proxy drop
            // 实测会触发 destroy → compositor cancel selection → 内容清空. 旧
            // active_primary_source 此处替换 (旧 source drop 时 compositor 已
            // cache 之前 send 出去的数据, 不影响其它 client 已粘贴的内容)。
            data.state.active_primary_source = Some(source);
        }
        SelectionOp::SetClipboard => {
            // T-0607 hotfix: CLIPBOARD 专用 text 字段, 防被 PRIMARY 路径覆盖.
            data.state.last_clipboard_text = Some(text.clone());
            let Some(device) = data.state.data_device.as_ref() else {
                tracing::trace!(
                    target: "quill::pointer",
                    "SetClipboard 但 data_device=None, 退化跳过"
                );
                return;
            };
            let Some(manager) = data.state.data_device_manager.as_ref() else {
                return;
            };
            let source = manager.create_data_source(qh, ());
            for mime in [
                "text/plain;charset=utf-8",
                "text/plain",
                "UTF8_STRING",
                "TEXT",
            ] {
                source.offer(mime.to_string());
            }
            device.set_selection(Some(&source), serial);
            tracing::debug!(
                target: "quill::pointer",
                n = text.len(),
                "CLIPBOARD set_selection"
            );
            // T-0607 hotfix: 同 PRIMARY 路径, store source 防 drop. CLIPBOARD
            // 在 mutter 上 lazy fetch 严重 (粘贴方真要时才 send), source 不存
            // 立刻 drop 是 user 实测 Ctrl+Shift+V 粘贴出空格的真因。
            data.state.active_data_source = Some(source);
        }
    }
}

/// T-0607: 消费 `state.pending_paste_request`, 真 trigger offer.receive +
/// 异步读 pipe → bracketed paste 包装 → pty.write.
///
/// **why pipe2 + calloop Generic source 而非 thread**: INV-005 单 EventLoop —
/// 不起 thread 做 IO. compositor 通过 receive request 把数据写到我们给的 fd
/// (write 端我们传过去), 我们读 read 端. read 端走 calloop Generic source 注册
/// 到 EventLoop, callback 在 read EOF 后 unregister + bracketed wrap + pty write.
///
/// **fd 协议**: `offer.receive(mime, write_fd)` — write_fd 由我们创建, compositor
/// dup 它写完关. 我们持 read_fd 走非阻塞读. 读完 (read returns 0 EOF) 关 read fd.
fn apply_paste_request(data: &mut LoopData) {
    let Some(source) = data.state.pending_paste_request.take() else {
        return;
    };

    // 取出对应 offer (Primary / Clipboard).
    enum OfferRef<'a> {
        Primary(&'a ZwpPrimarySelectionOfferV1),
        Clipboard(&'a WlDataOffer),
    }
    let offer_ref = match source {
        PasteSource::Primary => match data.state.primary_current_offer.as_ref() {
            Some(o) => OfferRef::Primary(o),
            None => {
                tracing::debug!(
                    target: "quill::pointer",
                    "Paste(Primary) 但 current_offer=None (没人复制过 PRIMARY?), 跳过"
                );
                return;
            }
        },
        PasteSource::Clipboard => match data.state.data_current_offer.as_ref() {
            Some(o) => OfferRef::Clipboard(o),
            None => {
                tracing::debug!(
                    target: "quill::pointer",
                    "Paste(Clipboard) 但 current_offer=None, 跳过"
                );
                return;
            }
        },
    };

    // 创建 pipe (read_fd, write_fd). 走 libc::pipe2(O_CLOEXEC | O_NONBLOCK)
    // 让 read 非阻塞 + 进程 fork 不漏 fd.
    let (read_fd, write_fd) = match make_paste_pipe() {
        Ok(pair) => pair,
        Err(e) => {
            tracing::warn!(
                target: "quill::pointer",
                error = %e,
                "Paste: 创建 pipe 失败, 跳过"
            );
            return;
        }
    };

    // 走 receive request, compositor 将开始往 write_fd 写.
    let mime = "text/plain;charset=utf-8".to_string();
    match offer_ref {
        OfferRef::Primary(o) => o.receive(mime, write_fd.as_fd()),
        OfferRef::Clipboard(o) => o.receive(mime, write_fd.as_fd()),
    }
    drop(write_fd); // 关我们这端 write fd, 让 compositor 写完后 reader 拿 EOF
                    // (compositor 持 dup 副本, 它写完关).

    // 注册 calloop Generic source 监听 read_fd readable, 走 paste_read_tick
    // 累积字节, EOF 时包 bracketed paste 写 PTY + remove source.
    // T-0608: bracketed mode 走 active tab 的 term (paste 跟 keyboard focus 一致).
    let bracketed = data
        .state
        .tabs_unchecked()
        .active()
        .term()
        .is_bracketed_paste();
    // PasteReadState 持 OwnedFd 控制 close 时机. Rc<RefCell<>> 让 callback 与
    // 调用方共享同一份 (调用方仅记 token 用 — 不再需要 read_fd_owned).
    let pasta_state = std::rc::Rc::new(std::cell::RefCell::new(PasteReadState {
        buf: Vec::new(),
        fd: Some(read_fd),
        bracketed,
    }));
    let pasta_state_for_callback = pasta_state.clone();
    let raw_fd = pasta_state
        .borrow()
        .fd
        .as_ref()
        .expect("just set Some")
        .as_raw_fd();
    // SAFETY: raw_fd 来自 pasta_state.fd (OwnedFd), 该 OwnedFd 持续活到 callback
    // 返 PostAction::Remove 后 fd 被 take 并 drop (close). BorrowedFd 'static
    // 是语法 marker, 真生命周期靠 pasta_state.fd 的 OwnedFd. calloop Generic
    // 在 PostAction::Remove 时走 epoll_ctl(EPOLL_CTL_DEL); 后续 fd close, EBADF
    // 由 calloop 0.14 内部容忍 (与 wayland fd / pty fd 注册同 pattern, INV-001
    // 同源).
    #[allow(unsafe_code)]
    let borrowed_fd: BorrowedFd<'static> = unsafe { BorrowedFd::borrow_raw(raw_fd) };

    let result = data.loop_handle.insert_source(
        Generic::new(borrowed_fd, Interest::READ, Mode::Level),
        move |_readiness, _fd, data: &mut LoopData| {
            paste_read_tick(data, &pasta_state_for_callback)
        },
    );
    match result {
        Ok(_token) => {
            tracing::debug!(target: "quill::pointer", ?source, "Paste: started async read pipe");
        }
        Err(err) => {
            tracing::warn!(
                target: "quill::pointer",
                ?err,
                "calloop insert_source(paste pipe) 失败 — 跳过本次 paste"
            );
        }
    }
}

/// T-0611: DnD Drop 读取的中间状态 (与 [`PasteReadState`] 同套路, 加 mime 字段
/// 让 EOF 路径决定 uri-list parse 还是 plain paste).
///
/// `mime` 用于 EOF 时分支:
/// - `text/uri-list` / `application/x-kde-cutselection` → parse_uri_list +
///   build_drop_command (shell escape).
/// - `text/plain;charset=utf-8` / `text/plain` → 直接当字面文本 paste (拖文本
///   块场景, 派单 In #B fallback).
struct DropReadState {
    buf: Vec<u8>,
    fd: Option<std::os::fd::OwnedFd>,
    bracketed: bool,
    mime: String,
}

/// T-0611: 消费 `state.pending_drop` — 真触发 `offer.receive` + 启动 calloop
/// Generic source 异步读 pipe, EOF 时 parse uri-list / shell escape /
/// bracketed wrap / pty.write.
///
/// 与 [`apply_paste_request`] 同 pipe + calloop 异步读路径 (派单 In #A "走 T-0607
/// paste pipe 同款"). 不同点: mime 选 `dnd_accepted_mime` 而非 hardcode
/// text/plain;charset=utf-8; EOF 路径多一道 uri-list 解析 + shell escape +
/// build_drop_command 转换; 完成后必 `offer.finish()` (wl_data_offer v3+ 要求,
/// sctk 0.19 / wayland-client 0.31 已自动 v3 bind, 缺 finish 会让 source side
/// 永远不返还 cursor). leave 路径 destroy offer.
fn apply_drop_request(data: &mut LoopData) {
    if !data.state.pending_drop {
        return;
    }
    data.state.pending_drop = false;

    // 取 offer + 接受的 mime. 若 None / None 直接走 cancel 路径 (offer destroy).
    let mime = match data.state.dnd_accepted_mime.clone() {
        Some(m) => m,
        None => {
            tracing::debug!(
                target: "quill::pointer",
                "DnD drop 但未 accept 任何 mime, 跳过 (源端将看到拖动失败)"
            );
            // Drop 后协议无 finish 时 source 看不到失败, 但我们没接受过 mime
            // — destroy offer 让协议状态干净. 不破坏 source.
            if let Some(offer) = data.state.dnd_current_offer.take() {
                offer.destroy();
            }
            data.state.dnd_current_offer_mimes.clear();
            return;
        }
    };
    let offer = match data.state.dnd_current_offer.as_ref() {
        Some(o) => o,
        None => {
            tracing::debug!(
                target: "quill::pointer",
                "DnD drop 但 dnd_current_offer=None (race?), 跳过"
            );
            data.state.dnd_accepted_mime = None;
            data.state.dnd_current_offer_mimes.clear();
            return;
        }
    };

    let (read_fd, write_fd) = match make_paste_pipe() {
        Ok(pair) => pair,
        Err(e) => {
            tracing::warn!(
                target: "quill::pointer",
                error = %e,
                "DnD: 创建 pipe 失败, 跳过"
            );
            return;
        }
    };

    offer.receive(mime.clone(), write_fd.as_fd());
    drop(write_fd);

    let bracketed = data
        .state
        .tabs_unchecked()
        .active()
        .term()
        .is_bracketed_paste();
    let drop_state = std::rc::Rc::new(std::cell::RefCell::new(DropReadState {
        buf: Vec::new(),
        fd: Some(read_fd),
        bracketed,
        mime: mime.clone(),
    }));
    let drop_state_for_cb = drop_state.clone();
    let raw_fd = drop_state
        .borrow()
        .fd
        .as_ref()
        .expect("just set Some")
        .as_raw_fd();
    // SAFETY: raw_fd 来自 drop_state.fd (OwnedFd); OwnedFd 在 RefCell 内持续到
    // PostAction::Remove 后被 take + drop 关闭, 与 paste_read_tick 同 pattern,
    // INV-001 同源 — calloop 0.14 在 epoll_ctl(EPOLL_CTL_DEL) 后的 close 容忍
    // EBADF.
    #[allow(unsafe_code)]
    let borrowed_fd: BorrowedFd<'static> = unsafe { BorrowedFd::borrow_raw(raw_fd) };
    let result = data.loop_handle.insert_source(
        Generic::new(borrowed_fd, Interest::READ, Mode::Level),
        move |_readiness, _fd, data: &mut LoopData| drop_read_tick(data, &drop_state_for_cb),
    );
    match result {
        Ok(_token) => {
            tracing::debug!(
                target: "quill::pointer",
                mime = %mime,
                bracketed,
                "DnD: started async read pipe"
            );
        }
        Err(err) => {
            tracing::warn!(
                target: "quill::pointer",
                ?err,
                "calloop insert_source(drop pipe) 失败 — 跳过本次 drop"
            );
        }
    }
}

/// T-0611: drop pipe readable callback. 读字节累积, EOF 时:
/// - 按 mime 分支: text/uri-list → parse + build_drop_command shell escape;
///   text/plain → 字面 paste.
/// - bracketed paste 包装 (跟 active term mode).
/// - pty.write to active tab.
/// - offer.finish() (v3+ 协议要求) + offer.destroy() + 清 DnD state.
/// - 返 PostAction::Remove.
fn drop_read_tick(
    data: &mut LoopData,
    drop_state: &std::rc::Rc<std::cell::RefCell<DropReadState>>,
) -> std::io::Result<PostAction> {
    let raw_fd = match drop_state.borrow().fd.as_ref() {
        Some(fd) => fd.as_raw_fd(),
        None => return Ok(PostAction::Remove),
    };
    let mut chunk = [0u8; 4096];
    loop {
        // SAFETY: raw_fd 来自 drop_state.fd (OwnedFd), 上面 borrow 已校验 Some;
        // libc::read 不转移所有权, 仅读字节. errno 通过 last_os_error 拿. 与
        // paste_read_tick 同 pattern.
        #[allow(unsafe_code)]
        let n = unsafe { libc::read(raw_fd, chunk.as_mut_ptr() as *mut _, chunk.len()) };
        if n > 0 {
            drop_state
                .borrow_mut()
                .buf
                .extend_from_slice(&chunk[..n as usize]);
            continue;
        }
        if n == 0 {
            // EOF — compositor 写完关 fd. parse + bracketed wrap + pty.write.
            let (buf, bracketed, mime) = {
                let mut s = drop_state.borrow_mut();
                (std::mem::take(&mut s.buf), s.bracketed, s.mime.clone())
            };
            let raw = String::from_utf8_lossy(&buf).to_string();
            let cmdline = match mime.as_str() {
                "text/uri-list" | "application/x-kde-cutselection" => {
                    let paths = parse_uri_list(&raw);
                    if paths.is_empty() {
                        tracing::debug!(
                            target: "quill::pointer",
                            mime = %mime,
                            raw_len = raw.len(),
                            "DnD: uri-list 解析空 (无 file:// 或全 reject), 跳过 pty.write"
                        );
                        String::new()
                    } else {
                        build_drop_command(&paths)
                    }
                }
                _ => raw, // text/plain* — 字面 paste.
            };
            if !cmdline.is_empty() {
                let wrapped = bracketed_paste_wrap(&cmdline, bracketed);
                let pty = data.state.tabs_unchecked().active().pty();
                match pty.write(&wrapped) {
                    Ok(n) if n == wrapped.len() => {
                        tracing::debug!(
                            target: "quill::pointer",
                            n,
                            bracketed,
                            mime = %mime,
                            "DnD: wrote bytes to pty"
                        );
                    }
                    Ok(n) => {
                        tracing::warn!(
                            target: "quill::pointer",
                            wrote = n,
                            total = wrapped.len(),
                            "DnD: pty.write 部分写入, 剩余字节丢弃 (背压)"
                        );
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        tracing::warn!(
                            target: "quill::pointer",
                            n = wrapped.len(),
                            "DnD: pty.write WouldBlock, 字节丢弃 (背压, INV-005 不重试)"
                        );
                    }
                    Err(e) => {
                        tracing::warn!(
                            target: "quill::pointer",
                            error = %e,
                            "DnD: pty.write 失败"
                        );
                    }
                }
            }

            // 协议 v3+ 要求 finish + destroy. 与 CLIPBOARD selection offer 不同
            // (那条 offer 由 compositor 管生命周期).
            if let Some(offer) = data.state.dnd_current_offer.take() {
                offer.finish();
                offer.destroy();
            }
            data.state.dnd_current_offer_mimes.clear();
            data.state.dnd_accepted_mime = None;

            drop_state.borrow_mut().fd = None;
            return Ok(PostAction::Remove);
        }
        // n < 0
        let err = std::io::Error::last_os_error();
        match err.kind() {
            std::io::ErrorKind::WouldBlock => return Ok(PostAction::Continue),
            std::io::ErrorKind::Interrupted => continue,
            _ => {
                tracing::warn!(
                    target: "quill::pointer",
                    error = %err,
                    "DnD: read failed"
                );
                drop_state.borrow_mut().fd = None;
                if let Some(offer) = data.state.dnd_current_offer.take() {
                    offer.destroy();
                }
                data.state.dnd_current_offer_mimes.clear();
                data.state.dnd_accepted_mime = None;
                return Ok(PostAction::Remove);
            }
        }
    }
}

/// T-0608: 消费 `state.pending_tab_op`, 真执行 tab 操作 — 新建 spawn + 注册
/// PTY fd / 关闭 drop + remove fd / 切 active / 拖拽 reorder.
///
/// **why drive_wayland step 3.8**: spawn 新 PTY 后必须 insert_source 让新 fd
/// 进 calloop EventLoop (INV-005); close 必须 remove_source 释放 fd 注册.
/// LoopHandle 在 LoopData 字段 (Dispatch 路径拿不到), 与 apply_selection_op
/// 同套路.
///
/// **active 切换的副作用**: 切换 active tab 后:
/// - PointerState.tab_count 同步 (set_tab_count)
/// - selection_state 清旧选区 (派单"切 tab 时清旧 selection 防 active tab cell.bg
///   反色错误高亮")
/// - term.mark_dirty() 触发下一次 idle redraw (新 tab 内容立即上屏)
/// - PointerState.set_cell_grid 同步新 active 的 cols/rows
fn apply_tab_op(data: &mut LoopData) {
    let Some(op) = data.state.pending_tab_op.take() else {
        return;
    };
    match op {
        TabOp::New => {
            // spawn 子 shell, 走 propagate_resize 一次让新 tab 同步当前 cols/rows.
            // T-0617: 当前 tab_count 是即将 +1 之前的 (tabs.len()), 但新 tab 加入
            // 后 count 必 >= 2, tab bar 必显, 所以预先按 max(2) 算 cell 区高度,
            // 让新 tab grid 起点与 propagate_resize 后续刷新一致 (派单"切 tab 数
            // 后 grid rows 变化, 触发 PtyHandle::resize 通知 SIGWINCH").
            let tab_count_after = data.state.tabs.as_ref().map(|t| t.len() + 1).unwrap_or(2);
            let (cols, rows) = cells_from_surface_px(
                data.state.core.width,
                data.state.core.height,
                tab_count_after,
            );
            let cols_u16 = cols.min(u16::MAX as usize) as u16;
            let rows_u16 = rows.min(u16::MAX as usize) as u16;
            let new_tab = match TabInstance::spawn(cols_u16, rows_u16) {
                Ok(t) => t,
                Err(err) => {
                    tracing::warn!(?err, "新 tab spawn 失败, 跳过");
                    return;
                }
            };
            let new_id = new_tab.id();
            let new_pty_fd = new_tab.pty().raw_fd();
            // 注册 PTY fd 到 calloop. 与 run_window 启动期路径同语义.
            #[allow(unsafe_code)]
            let new_pty_borrowed: BorrowedFd<'static> =
                unsafe { BorrowedFd::borrow_raw(new_pty_fd) };
            let token_result = data.loop_handle.insert_source(
                Generic::new(new_pty_borrowed, Interest::READ, Mode::Level),
                move |_readiness, _fd, data: &mut LoopData| {
                    let LoopData {
                        state, loop_signal, ..
                    } = &mut *data;
                    pty_read_tick(state, new_id, loop_signal)
                },
            );
            let token = match token_result {
                Ok(t) => t,
                Err(err) => {
                    tracing::warn!(?err, id = new_id.raw(), "新 tab PTY fd insert_source 失败");
                    // tab spawn 成功但 fd 注册失败 — drop tab 自然 SIGHUP shell.
                    return;
                }
            };
            data.pty_tokens.push((new_id, token));
            // push 到 list + 切 active 到新 tab.
            let tab_list = match data.state.tabs.as_mut() {
                Some(t) => t,
                None => return,
            };
            let (new_idx, _id) = tab_list.push(new_tab);
            tab_list.set_active(new_idx);
            on_active_tab_changed(data);
            // T-0617: count 变化 (1→2 / N→N+1) → 标 resize_dirty 让下一次
            // propagate_resize_if_dirty 把所有 tab 的 grid + PTY winsize 同步到
            // 新的 cell 区高度. 单 tab 时 cell 区含 tab_bar 高度, 多 tab 不含 —
            // 跨 1↔2 边界必须重 resize, 不然 active tab grid 与新 cell 区错位.
            // 派单 In #B "切 tab 数后 grid rows 变化, 触发 PtyHandle::resize 通知
            // SIGWINCH".
            data.state.core.resize_dirty = true;
            tracing::info!(
                id = new_id.raw(),
                idx = new_idx,
                "new tab spawned + activated"
            );
        }
        TabOp::CloseActive => {
            let active_idx = match data.state.tabs.as_ref() {
                Some(t) => t.active_idx(),
                None => return,
            };
            close_tab_idx(data, active_idx);
        }
        TabOp::Close(idx) => {
            close_tab_idx(data, idx);
        }
        TabOp::Next => {
            let tab_list = match data.state.tabs.as_mut() {
                Some(t) => t,
                None => return,
            };
            if tab_list.len() <= 1 {
                return;
            }
            let next = (tab_list.active_idx() + 1) % tab_list.len();
            tab_list.set_active(next);
            on_active_tab_changed(data);
        }
        TabOp::Prev => {
            let tab_list = match data.state.tabs.as_mut() {
                Some(t) => t,
                None => return,
            };
            if tab_list.len() <= 1 {
                return;
            }
            let len = tab_list.len();
            let prev = (tab_list.active_idx() + len - 1) % len;
            tab_list.set_active(prev);
            on_active_tab_changed(data);
        }
        TabOp::Switch(idx) => {
            let tab_list = match data.state.tabs.as_mut() {
                Some(t) => t,
                None => return,
            };
            if idx >= tab_list.len() {
                return;
            }
            tab_list.set_active(idx);
            on_active_tab_changed(data);
        }
        TabOp::Reorder {
            from_idx,
            target_idx,
        } => {
            let tab_list = match data.state.tabs.as_mut() {
                Some(t) => t,
                None => return,
            };
            let _ = tab_list.swap_reorder(from_idx, target_idx);
            on_active_tab_changed(data);
        }
    }
}

/// T-0608: 关闭 idx 处 tab — drop TabInstance + remove calloop source + 邻近
/// active 切换 + 若最后一个 → quit. 抽出来让 CloseActive / Close(idx) 共用.
fn close_tab_idx(data: &mut LoopData, idx: usize) {
    // pop tab, 拿到 id 用于 remove token. 即使 tab.idx_of 返 None (race) 仍兜底.
    let removed_tab_id = {
        let tab_list = match data.state.tabs.as_mut() {
            Some(t) => t,
            None => return,
        };
        if idx >= tab_list.len() {
            return;
        }
        let removed = tab_list.remove(idx);
        removed.map(|t| t.id())
    };
    let Some(removed_id) = removed_tab_id else {
        return;
    };
    // remove 对应 calloop source. ptokens 中按 id 找.
    if let Some(pos) = data.pty_tokens.iter().position(|(id, _)| *id == removed_id) {
        let (_, token) = data.pty_tokens.swap_remove(pos);
        data.loop_handle.remove(token);
    }
    // tabs 空 → quit (派单 In #I "close 最后一个 tab → quit 整 quill").
    if data
        .state
        .tabs
        .as_ref()
        .map(|t| t.is_empty())
        .unwrap_or(true)
    {
        tracing::info!("close last tab → quit quill");
        data.loop_signal.stop();
        return;
    }
    on_active_tab_changed(data);
    // T-0617: count 变化 (N→N-1) → 标 resize_dirty 让下一次 propagate 同步全
    // tab grid (尤其 2→1 跨边界 tab_bar 隐藏 → cell 区扩高). 与 New 路径同决策.
    data.state.core.resize_dirty = true;
}

/// T-0608: active tab 切换后的副作用 — 同步 PointerState 的 tab_count + cell
/// grid, 清 selection (派单"切 tab 清旧 selection"), 标记 active term dirty
/// (触发下一次 idle redraw 让新 tab 内容立即上屏).
fn on_active_tab_changed(data: &mut LoopData) {
    let Some(tab_list) = data.state.tabs.as_ref() else {
        return;
    };
    let tab_count = tab_list.len();
    let (cols, rows) = tab_list.active().term().dimensions();
    data.state.pointer_state.set_tab_count(tab_count);
    data.state.pointer_state.set_cell_grid(cols, rows);
    // 清旧 selection. 派单已知陷阱: 切 tab 时清旧 selection (pointer_state.selection_state.clear())
    // 防 active tab cell.bg 反色错误高亮.
    data.state.selection_state.clear();
    // 标记 active term dirty 触发 redraw.
    if let Some(t) = data.state.tabs.as_mut() {
        t.active_mut().term_mut().mark_dirty();
    }
    data.state.presentation_dirty = true;
}

/// T-0607: 异步 paste 读取的中间状态. fd 通过 OwnedFd 持有控制 close 时机
/// (PostAction::Remove 时 take + drop 即 close; calloop epoll_ctl(EPOLL_CTL_DEL)
/// 在 source remove 路径走, EBADF 容忍).
struct PasteReadState {
    /// 累积读到的字节. EOF 时一次性 bracketed wrap + write_to_pty.
    buf: Vec<u8>,
    /// 读端 OwnedFd. PostAction::Remove 时 take + drop 关闭.
    fd: Option<std::os::fd::OwnedFd>,
    /// 是否包 bracketed paste (拷贝 term mode 在 schedule 时刻, 防 race).
    bracketed: bool,
}

/// T-0607: paste pipe readable callback. 读字节累积, EOF 时:
/// - 包 bracketed paste (若启用 — 走 term.is_bracketed_paste)
/// - pty.write
/// - 返 PostAction::Remove 让 calloop 释放 source + close read_fd
fn paste_read_tick(
    data: &mut LoopData,
    pasta_state: &std::rc::Rc<std::cell::RefCell<PasteReadState>>,
) -> std::io::Result<PostAction> {
    let raw_fd = match pasta_state.borrow().fd.as_ref() {
        Some(fd) => fd.as_raw_fd(),
        None => {
            return Ok(PostAction::Remove);
        }
    };
    let mut chunk = [0u8; 4096];
    loop {
        // SAFETY: raw_fd 来自 pasta_state.fd (OwnedFd), 上面 borrow 已校验 Some;
        // libc::read 不转移所有权, 仅读字节. errno 通过 last_os_error 拿.
        #[allow(unsafe_code)]
        let n = unsafe { libc::read(raw_fd, chunk.as_mut_ptr() as *mut _, chunk.len()) };
        if n > 0 {
            pasta_state
                .borrow_mut()
                .buf
                .extend_from_slice(&chunk[..n as usize]);
            continue;
        }
        if n == 0 {
            // EOF — compositor 写完关 fd. 走 bracketed wrap + pty.write.
            let (buf, bracketed) = {
                let mut state_mut = pasta_state.borrow_mut();
                (std::mem::take(&mut state_mut.buf), state_mut.bracketed)
            };
            let text_str = String::from_utf8_lossy(&buf).to_string();
            let wrapped = bracketed_paste_wrap(&text_str, bracketed);
            // T-0608: 写 active tab 的 PTY (paste 跟 keyboard focus 一致).
            {
                let pty = data.state.tabs_unchecked().active().pty();
                match pty.write(&wrapped) {
                    Ok(n) if n == wrapped.len() => {
                        tracing::debug!(
                            target: "quill::pointer",
                            n,
                            bracketed,
                            "paste: wrote bytes to pty"
                        );
                    }
                    Ok(n) => {
                        tracing::warn!(
                            target: "quill::pointer",
                            wrote = n,
                            total = wrapped.len(),
                            "paste: pty.write 部分写入, 剩余字节丢弃 (背压)"
                        );
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        tracing::warn!(
                            target: "quill::pointer",
                            n = wrapped.len(),
                            "paste: pty.write WouldBlock, 字节丢弃 (背压, INV-005 不重试)"
                        );
                    }
                    Err(e) => {
                        tracing::warn!(
                            target: "quill::pointer",
                            error = %e,
                            "paste: pty.write 失败"
                        );
                    }
                }
            }
            // 释放 fd (close), source 一并 remove.
            pasta_state.borrow_mut().fd = None;
            return Ok(PostAction::Remove);
        }
        // n < 0: errno
        let err = std::io::Error::last_os_error();
        match err.kind() {
            std::io::ErrorKind::WouldBlock => return Ok(PostAction::Continue),
            std::io::ErrorKind::Interrupted => continue,
            _ => {
                tracing::warn!(
                    target: "quill::pointer",
                    error = %err,
                    "paste: read failed"
                );
                pasta_state.borrow_mut().fd = None;
                return Ok(PostAction::Remove);
            }
        }
    }
}

/// T-0607: 创建 pipe2(O_CLOEXEC | O_NONBLOCK) 用于 selection paste.
fn make_paste_pipe() -> std::io::Result<(std::os::fd::OwnedFd, std::os::fd::OwnedFd)> {
    use std::os::fd::FromRawFd;
    let mut fds = [-1i32; 2];
    // SAFETY: pipe2 标准 syscall, fds 是栈数组 ≥ 2 元. flags O_CLOEXEC 让 fd
    // 不漏给 fork 子进程; O_NONBLOCK 让 read EOF 路径不阻塞 (与 INV-005 一致).
    #[allow(unsafe_code)]
    let rc = unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC | libc::O_NONBLOCK) };
    if rc < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: pipe2 返 ≥0 时 fds[0] / fds[1] 是有效 fd, 我们获得所有权.
    #[allow(unsafe_code)]
    unsafe {
        Ok((
            std::os::fd::OwnedFd::from_raw_fd(fds[0]),
            std::os::fd::OwnedFd::from_raw_fd(fds[1]),
        ))
    }
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

    // Step 3.6 (T-0603): dispatch_pending 期间 `Dispatch<WlKeyboard>` 可能
    // 已置 `state.pending_repeat` (按键 Pressed → Start 或 Released / modifier
    // 变化 → Stop). 在 flush 之前把 calloop Timer source 真 insert / remove.
    apply_repeat_request(data);

    // Step 3.7 (T-0607): selection / paste / autoscroll 副作用 — 与 repeat
    // 同套路, dispatch 期间 Dispatch<WlPointer> / Dispatch<WlKeyboard> 写
    // pending_*, 这里真消费 (那里 &mut LoopData 字段全可访问 + 协议 handle
    // 全可调).
    apply_autoscroll_op(data);
    {
        // qh 从 event_queue 拿. 借: data.event_queue 与 data.state 是不同字段,
        // 直接 split borrow.
        let qh = data.event_queue.handle();
        apply_selection_op(data, &qh);
    }
    apply_paste_request(data);

    // Step 3.7.5 (T-0611): DnD drop 消费 — 走 offer.receive + pipe 异步读
    // (与 paste 同 pipe 套路) + uri-list parse + shell escape + bracketed wrap
    // + pty.write. 与 paste 解耦不复用 PasteSource 是因为 mime 选择不同
    // (text/uri-list 而非 text/plain;charset=utf-8) + EOF 路径多一道解析.
    apply_drop_request(data);

    // Step 3.8 (T-0608): tab 操作消费 — 新建 / 关闭 / 切换 / 拖拽 reorder.
    // 与 selection / repeat 同套路 — Dispatch 路径只 set pending_tab_op,
    // 这里真 spawn 子 shell + insert/remove calloop source + 切 active.
    apply_tab_op(data);

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

    // T-0505: 尝试 bind zwp_text_input_manager_v3 (TIv3) — fcitx5 / ibus 未启
    // 时 compositor 不导出此 global, bind 失败 → None, IME 路径直接退化 (
    // 用户敲键盘走 wl_keyboard ASCII 路径仍可用)。version 1 (协议唯一版本).
    // ADR 0007 详释。
    let text_input_manager: Option<ZwpTextInputManagerV3> = match globals.bind(&qh, 1..=1, ()) {
        Ok(m) => {
            tracing::info!("zwp_text_input_manager_v3 bound (compositor 支持 IME)");
            Some(m)
        }
        Err(err) => {
            tracing::info!(
                ?err,
                "zwp_text_input_manager_v3 不可用 — IME 退化到无 (fcitx5/ibus 未启 \
                 或 compositor 不导出); 键盘 ASCII 路径仍正常"
            );
            None
        }
    };

    // T-0703-fix: bind wl_shm + load xcursor theme + 创 cursor wl_surface
    // (ADR 0009 撤回 ADR 0008 wp_cursor_shape_v1, 走 GTK4 / Qt / Electron / GNOME
    // 自家 app 同款 wl_pointer.set_cursor + libwayland-cursor 路径, 与 mutter
    // 拖动接管 cursor 视觉一致).
    //
    // wl_shm 是 wayland 核心协议, 任何 compositor 都导出 — bind 失败仅出现在
    // compositor 重大 bug 时, 那种情况 quill 任何渲染都跑不动, 提早 fail-fast.
    let shm: WlShm = globals
        .bind(&qh, 1..=1, ())
        .map_err(|e| anyhow!("wl_shm 不可用 (compositor bug? bind 失败: {e})"))?;

    // cursor_surface 整个进程一份 (单 wl_pointer 只需一个 cursor surface, 派单
    // 已知陷阱: "cursor surface 释放 - 多 hover region 切换时复用同一 cursor_surface,
    // 每次 attach 新 buffer + commit. 不要每次创建 surface").
    let cursor_surface = compositor.create_surface(&qh);

    // T-0703-fix hotfix: wayland-cursor 0.31 (pure Rust) **不处理 XDG theme
    // inherit 链** — Arch / Ubuntu 默认 /usr/share/icons/default/index.theme
    // 写 `Inherits=Adwaita` 但没有自己的 cursors/ 目录, libwayland-cursor (C)
    // 处理这个 inherit 但 wayland-cursor (Rust) 不处理. load_or("default")
    // 拿不到任何 cursor 文件, get_cursor 全 None, 4 角 cursor 退化到
    // compositor 默认小箭头 (user 实测 4 边能用 4 角不能).
    //
    // 修: try chain (XCURSOR_THEME env / "Adwaita" / "default"), 第一个能拿到
    // cursor 的用. cursor_size 走 XCURSOR_SIZE env 默认 24 (派单 Out 不动).
    // T-0703-fix hotfix v2: cursor 物理像素大小 = logical size × HIDPI_SCALE.
    // 之前直接用 24 配 set_buffer_scale(2) 让 cursor 实际显示 12 logical px,
    // 比 GTK4/Qt 应用小一半 (它们走 logical 24 × scale 2 = 48 physical px).
    // load_or 的 size 参数是 physical pixel size, 必须 × HIDPI_SCALE.
    let logical_size: u32 = std::env::var("XCURSOR_SIZE")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(24);
    let theme_size: u32 = logical_size * crate::wl::HIDPI_SCALE;
    let theme_candidates: Vec<String> = std::iter::once(std::env::var("XCURSOR_THEME").ok())
        .flatten()
        .chain(["Adwaita".to_string(), "default".to_string()])
        .collect();
    let cursor_theme: Option<CursorTheme> = theme_candidates.iter().find_map(|name| {
        match CursorTheme::load_or(&conn, shm.clone(), name, theme_size) {
            Ok(mut theme) => {
                // 探针 verify: try get "left_ptr" 是任何主题必有的 base cursor.
                // 拿不到说明此 theme 是空 stub (e.g. /usr/share/icons/default/
                // 只有 index.theme 没 cursors/), 试下一个候选.
                if theme.get_cursor("left_ptr").is_some()
                    || theme.get_cursor("default").is_some()
                {
                    tracing::info!(
                        theme = %name,
                        size = theme_size,
                        "wayland_cursor::CursorTheme loaded"
                    );
                    Some(theme)
                } else {
                    tracing::debug!(
                        target: "quill::cursor",
                        theme = %name,
                        "CursorTheme load 成功但 left_ptr/default cursor 都缺 (空 stub theme), 试下一候选"
                    );
                    None
                }
            }
            Err(err) => {
                tracing::debug!(
                    target: "quill::cursor",
                    theme = %name,
                    ?err,
                    "CursorTheme::load_or 失败, 试下一候选"
                );
                None
            }
        }
    });
    if cursor_theme.is_none() {
        tracing::warn!(
            "所有 cursor theme 候选 (XCURSOR_THEME / Adwaita / default) 都 load 失败 — \
             cursor 形状切换退化 (compositor 默认箭头); 检查 /usr/share/icons/Adwaita/cursors/"
        );
    }

    // T-0607: bind zwp_primary_selection_device_manager_v1 (PRIMARY 协议).
    // GNOME 45+ / KDE Plasma 6 / wlroots 都支持; 老 compositor 缺 → None,
    // PRIMARY 路径退化, 仅 CLIPBOARD 工作. 派单 In #F 已知陷阱.
    let primary_selection_manager: Option<ZwpPrimarySelectionDeviceManagerV1> =
        match globals.bind(&qh, 1..=1, ()) {
            Ok(m) => {
                tracing::info!(
                    "zwp_primary_selection_device_manager_v1 bound (PRIMARY 中键复制粘贴可用)"
                );
                Some(m)
            }
            Err(err) => {
                tracing::warn!(
                    ?err,
                    "zwp_primary_selection_device_manager_v1 不可用 — PRIMARY 中键复制 \
                     退化; 仅 CLIPBOARD 工作. 升级 compositor 启用 (GNOME 45+ / Plasma 6)."
                );
                None
            }
        };

    // T-0607: bind wl_data_device_manager (CLIPBOARD 核心协议, 现代 compositor
    // 全支持). version 1..=3 — quill 仅用 v1 子集 (set_selection / set / receive),
    // 锁 1..=3 兼容老新.
    let data_device_manager: Option<WlDataDeviceManager> = match globals.bind(&qh, 1..=3, ()) {
        Ok(m) => {
            tracing::info!("wl_data_device_manager bound (CLIPBOARD Ctrl+Shift+C/V 可用)");
            Some(m)
        }
        Err(err) => {
            tracing::warn!(
                ?err,
                "wl_data_device_manager 不可用 — CLIPBOARD 退化 (极罕见, 现代 \
                 compositor 都导出); Ctrl+Shift+C/V 不工作"
            );
            None
        }
    };

    // State 字段顺序固化 INV-001(renderer→window→conn)+ pty 放最后(T-0202 Lead + 审码)。
    // T-0501 加: seat_state / keyboard_state / keyboard 三字段位于 core 与 pty 之间,
    // 不破坏 INV-001 链条 (它们都不持 wgpu/wayland 裸指针, 仅 SCTK/wayland-client
    // safe wrapper, drop 顺序无 UB 风险)。
    // T-0505 加: text_input_manager / text_input / ime_state 三字段, 同性质
    // (wayland-protocols 协议 handle + quill 自有 struct), drop 顺序无 UB。
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
        // T-0703-fix: shm / cursor_theme / cursor_surface 在启动期 bind / load,
        // 不依赖 Pointer capability — wl_shm 永远存在, theme 加载与 pointer
        // 无关. cursor_surface 整生命周期复用一份 (派单 In #B 已知陷阱).
        shm,
        cursor_theme,
        cursor_surface,
        is_maximized: false,
        presentation_dirty: false,
        pending_scroll_lines: 0,
        text_input_manager,
        text_input: None,
        ime_state: ImeState::new(),
        pending_repeat: None,
        // T-0607 selection / clipboard 起步全空, 协议 device 在 SeatHandler::
        // new_capability(Pointer) 时 lazy 创建.
        selection_state: SelectionState::new(),
        pending_selection_op: None,
        pending_paste_request: None,
        pending_autoscroll_op: None,
        pending_autoscroll_cancel: false,
        primary_selection_manager,
        primary_selection_device: None,
        primary_current_offer: None,
        active_primary_source: None,
        active_data_source: None,
        data_device_manager,
        data_device: None,
        data_current_offer: None,
        last_selection_text: None,
        last_primary_text: None,
        last_clipboard_text: None,
        // T-0608: tabs 启动后立即被 spawn 真 shell 注入 (下方几行).
        tabs: None,
        pending_tab_op: None,
        last_tab_drag_x: 0.0,
        // T-0611: DnD 状态全空起步. compositor 在拖入时发 DataOffer + Enter,
        // 我们填充; Leave / Drop 后清.
        incoming_offer_mimes: Vec::new(),
        dnd_current_offer: None,
        dnd_current_offer_mimes: Vec::new(),
        dnd_accepted_mime: None,
        pending_drop: false,
    };

    // T-0608/T-0202/T-0108: spawn 第一个 tab 的子 shell + 把 master fd 注册进
    // calloop(INV-005)。初始尺寸 80x24 写死;Wayland configure 后 propagate_resize
    // 链路同步真实 cols/rows.
    let initial_tab = TabInstance::spawn(80, 24).context("初始 tab spawn 失败")?;
    let pty_fd = initial_tab.pty().raw_fd();
    let initial_tab_id = initial_tab.id();
    state.tabs = Some(TabList::new(initial_tab));

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
    // T-0608: PTY callback 通过 closure capture 的 tab_id 定位 state.tabs 的
    // 对应 TabInstance — 多 tab 时每个 fd 注册都 capture 自己的 id (apply_tab_op
    // 路径同模式).
    let initial_token = loop_handle
        .insert_source(
            Generic::new(pty_borrowed, Interest::READ, Mode::Level),
            move |_readiness, _fd, data: &mut LoopData| {
                let LoopData {
                    state, loop_signal, ..
                } = &mut *data;
                pty_read_tick(state, initial_tab_id, loop_signal)
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
        // T-0608: term/pty 改为每 tab 各自持 (TabInstance 内). LoopData 不再
        // 持 term 字段, 主路径走 state.tabs_unchecked().active() / active_mut()
        // 拿 active tab 的 term + pty.
        loop_signal: loop_signal.clone(),
        // T-0399 P1-1: FrameStats 接采集点。空 stats, idle callback 每次成功
        // draw_cells 后调 record_and_log; Phase 6 soak 通过 `quill::frame`
        // target 观察帧间隔聚合。
        frame_stats: crate::frame_stats::FrameStats::new(),
        text_system,
        // T-0603: clone LoopHandle 给 LoopData, 让 `apply_repeat_request` 在
        // dispatch_pending 之后能动态 insert / remove Timer source.
        // LoopHandle clone 内部 Rc, 与 event_loop 本体共享调度器, drop 时仅
        // 减计数不影响 EventLoop.
        loop_handle: loop_handle.clone(),
        repeat_token: None,
        // T-0802 In #B: resize 节流状态. last=None → 首次 propagate 立即跑不
        // 等 60ms; pending_resize_followup=None → 无兜底 Timer pending.
        last_propagate_at: None,
        pending_resize_followup: None,
        // T-0607: autoscroll Timer 起步空, AutoScrollStart 时 lazy schedule.
        pending_autoscroll_timer: None,
        autoscroll_delta: 0,
        // T-0608: 启动后第一个 tab 的 PTY fd 已在上方 insert_source, 把
        // (tab_id, token) push 进 token 表 — close tab 时按 id 找 token + remove.
        pty_tokens: vec![(initial_tab_id, initial_token)],
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
            // borrow split: data.state.renderer / data.state.tabs /
            // data.frame_stats / data.text_system / data.state.pointer_state 等都
            // 是不同字段, 字段级 split 不冲突 (NLL 支持).
            // T-0608: term 走 active tab → 通过 tabs.as_mut() 拿 &mut TabList,
            // 然后 active_mut() 拿 &mut TabInstance, term_mut() 拿 &mut TermState.
            // state 在 LoopData 字段, 通过 split borrow 与其它 LoopData 字段并存.
            let LoopData {
                state,
                frame_stats,
                text_system,
                ..
            } = &mut *data;
            // 字段级 split: tabs / renderer / pointer_state / ime_state /
            // presentation_dirty / pending_scroll_lines / selection_state 都是
            // State 不同字段, 同 *state 下能拿独立 &mut.
            // T-0608: 先记 tab_count / active_idx (后续 set_tab_state 用), 避免
            // 与 active_mut() 的 mut 借用冲突.
            // T-0617: 同步取 active tab 的 title snapshot (Rc<RefCell<String>>
            // borrow 短持 + clone 出 owned String, 避免长持借用与 listener
            // borrow_mut() 冲突 panic). 空 string → fallback 到 DEFAULT_TITLE
            // 让 titlebar 中央仍显 "quill" (与 T-0702 默认行为一致, 派单 In #A
            // "其它 event 全 ignore" + 启动期 listener 还没收到 OSC).
            let (tab_count, active_idx, active_title) = match state.tabs.as_ref() {
                Some(t) => {
                    let title = t.active().title();
                    (t.len(), t.active_idx(), title)
                }
                None => return,
            };
            let Some(tab_list) = state.tabs.as_mut() else {
                return; // tabs 未初始化 (启动期 race) — 跳过本帧
            };
            let t = tab_list.active_mut().term_mut();
            // T-0602: 消费 Dispatch<WlPointer> / Dispatch<WlKeyboard> 累积的
            // scrollback line 数. State 拿不到 term (在 LoopData 兄弟字段), 所以
            // 滚动决定推到 idle 回放. scroll_display 内部已置 dirty, 不再单独
            // 触发 — 后续 is_dirty 检查自然命中 redraw 路径.
            if state.pending_scroll_lines != 0 {
                let n = state.pending_scroll_lines;
                t.scroll_display(n);
                tracing::trace!(
                    target: "quill::scroll",
                    delta = n,
                    new_offset = t.display_offset(),
                    "scrollback applied"
                );
                state.pending_scroll_lines = 0;
            }
            // T-0504: 检查 term cell 内容 dirty || presentation (CSD hover) dirty.
            // 任一为真即重画. presentation_dirty 由 Dispatch<WlPointer> 在
            // HoverChange 时置位, 重画后清.
            if !t.is_dirty() && !state.presentation_dirty {
                return;
            }
            // T-0608: tab_count / active_idx 已在上方提前记 (避免与
            // active_mut() 借用冲突).
            let Some(r) = state.renderer.as_mut() else {
                // renderer 还没建好(首次 configure 之前的 idle tick)。dirty
                // 留着,等首次 configure 走 init_renderer_and_draw 完成后下一轮
                // 再画。
                return;
            };
            r.set_tab_state(tab_count, active_idx);
            // T-0617: 同步 active tab 的 OSC title 到 renderer. listener 写
            // active_tab.title (Rc<RefCell<String>>), 这里 read snapshot;
            // active_title 空 → 走 DEFAULT_TITLE = "quill" (与 T-0702 默认 + 启动
            // 期 fallback 同一来源). 派单 In #A "fallback 显 quill 或 ~".
            let title_for_render = if active_title.is_empty() {
                crate::wl::render::DEFAULT_TITLE.to_string()
            } else {
                active_title
            };
            r.set_title(title_for_render);
            // T-0305: 全量 cells 收集。1920 cell × CellRef(~32 字节 pos+c+fg+bg)
            // = 60 KiB,Vec 分配开销 << wgpu submit 一次的开销, Phase 6 soak 验
            // bench 再决定是否 reuse Vec(目前简单胜过聪明)。
            let cells: Vec<crate::term::CellRef> = t.cells_iter().collect();
            let (cols, rows) = t.dimensions();

            // T-0504: hover 区域 (CSD titlebar 按钮高亮) 由 PointerState 维护.
            let hover = state.pointer_state.hover();

            // T-0403: 走 draw_frame (含字形); 若 text_system 未建好降级到
            // draw_cells (Phase 3 色块路径)。
            // T-0505: 当 IME 有 preedit 时构造 PreeditOverlay 传入 — 渲染层
            // 在 cursor 当前位置追加 preedit 字 + 下划线。
            let preedit_overlay: Option<crate::wl::render::PreeditOverlay> = {
                let preedit_text = state.ime_state.current_preedit();
                if preedit_text.is_empty() {
                    None
                } else {
                    let pos = t.cursor_pos();
                    Some(crate::wl::render::PreeditOverlay {
                        text: preedit_text.to_owned(),
                        cursor_col: pos.col,
                        cursor_line: pos.line,
                    })
                }
            };

            // T-0601: 构造 CursorInfo (派单 In #C). 光标位置 / 形状 / SHOW_CURSOR
            // 都从 term 拿; preedit 显示时强制 visible=false (光标位置与 preedit
            // 起点同 col/line, 主流 IME 风格隐光标显 preedit, 也防视觉重叠).
            // 颜色用 term 默认 cursor 色 #ffffff (已在 named_color_rgb 内定, 此
            // 处与 preedit underline 同色 — 视觉上 cursor 块 vs preedit 下划线
            // 互斥不会同时出现, 复用色不冲突).
            //
            // INV-010: term::CursorShape → render::CursorStyle 显式 match, 上游
            // 加 shape variant 时 compile error 在此一处捕获. Hidden 折叠到
            // visible=false (term::CursorShape::Hidden 来自 alacritty 内部状
            // 态, SHOW_CURSOR 模式独立).
            let cursor_info: Option<crate::wl::render::CursorInfo> = {
                use crate::term::CursorShape;
                let pos = t.cursor_pos();
                let shape = t.cursor_shape();
                let style = match shape {
                    CursorShape::Block => Some(crate::wl::render::CursorStyle::Block),
                    CursorShape::Underline => Some(crate::wl::render::CursorStyle::Underline),
                    CursorShape::Beam => Some(crate::wl::render::CursorStyle::Beam),
                    CursorShape::HollowBlock => Some(crate::wl::render::CursorStyle::HollowBlock),
                    CursorShape::Hidden => None,
                };
                style.map(|s| crate::wl::render::CursorInfo {
                    col: pos.col,
                    line: pos.line,
                    visible: t.cursor_visible() && preedit_overlay.is_none(),
                    style: s,
                    color: crate::term::Color {
                        r: 0xff,
                        g: 0xff,
                        b: 0xff,
                    },
                })
            };

            let draw_result = match text_system.as_mut() {
                Some(ts) => {
                    // 收集每行的文本快照, 喂给 draw_frame shape。
                    // T-0602: 走 `display_text` 而非 `line_text` — 跟 cells_iter
                    // 同源 (display_offset 自动偏移), scrollback 时显示历史行字
                    // 形与 cell 块对齐. line_text 直接读 active grid 不感知滚动,
                    // 用户滚到顶却看到 active 内容是派单 In #D 真因.
                    // `rows` 行 × ~80 字符每行 = ~7 KiB Vec, 与 cells Vec 同数量级。
                    let row_texts: Vec<String> = (0..rows).map(|row| t.display_text(row)).collect();
                    // T-0607: 算选中 cells 列表给 draw_frame 渲染 selection bg.
                    // 选区为空 (无 active selection / 已 clear) → 空 vec 入参,
                    // append_selection_bg_to_cell_bytes 内 loop noop.
                    let selection_cells: Vec<crate::term::CellPos> = {
                        let sel = &state.selection_state;
                        if sel.has_selection() {
                            match sel.mode() {
                                crate::wl::selection::SelectionMode::Linear => {
                                    crate::wl::selection::selected_cells_linear(sel, cols)
                                }
                                crate::wl::selection::SelectionMode::Block => {
                                    crate::wl::selection::selected_cells_block(sel, cols, rows)
                                }
                            }
                        } else {
                            Vec::new()
                        }
                    };
                    let selection_slice = if selection_cells.is_empty() {
                        None
                    } else {
                        Some(selection_cells.as_slice())
                    };
                    r.draw_frame(
                        ts,
                        &cells,
                        cols,
                        rows,
                        &row_texts,
                        hover,
                        preedit_overlay.as_ref(),
                        cursor_info.as_ref(),
                        selection_slice,
                    )
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

            // T-0505: cursor_rectangle 上报 (派单 In #E).
            if state.ime_state.is_enabled() {
                if let Some(ti) = state.text_input.as_ref() {
                    let pos = t.cursor_pos();
                    let rect = cursor_rectangle_for_cell(pos.col, pos.line);
                    if let Some(new) = state.ime_state.update_cursor_rectangle(rect) {
                        ti.set_cursor_rectangle(new.x, new.y, new.width, new.height);
                        ti.commit();
                        tracing::trace!(
                            target: "quill::ime",
                            x = new.x, y = new.y, w = new.width, h = new.height,
                            "cursor_rectangle updated"
                        );
                    }
                }
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
    /// T-0703-fix: wl_shm 全局, 给 wayland_cursor::CursorTheme 用 (内部
    /// create_pool 把 xcursor svg 解码后的 ARGB32 像素铺到 shm pool, attach
    /// 给 cursor_surface). 一次 bind 整生命周期持有, drop 时自动 destroy
    /// (wayland-client Proxy Drop). ADR 0009 (撤回 ADR 0008 wp_cursor_shape_v1).
    ///
    /// `#[allow(dead_code)]`: 字段不再被显式读 (`CursorTheme` 内部已 clone shm
    /// 持自己的引用), 但保留**所有权**让 wl_shm 全局在整 State 生命周期内活
    /// — 防止未来加 cursor 主题热重载 / 二次 CursorTheme 构造时漏 shm 持有.
    /// 与 `data_device_manager` 同 forward-compat 决策.
    #[allow(dead_code)]
    shm: WlShm,
    /// T-0703-fix: xcursor theme 加载结果 (`wayland_cursor::CursorTheme`). 启动
    /// 期 `CursorTheme::load_or(conn, shm, "default", 24)` (env XCURSOR_THEME /
    /// XCURSOR_SIZE 已在 load_or 内部读). 加载失败时 `None`, `apply_cursor_shape`
    /// 跳过 (cursor 维持上一次状态, 默认箭头). drop 顺序: theme 内部持 WlShmPool,
    /// 比 cursor_surface 后 drop 安全 (但 wgpu 链 INV-001 不动 — 字段位置在
    /// pty 之前 / wayland safe wrapper 段).
    cursor_theme: Option<CursorTheme>,
    /// T-0703-fix: cursor 形状专用 wl_surface, 整个进程一份, 多 hover region
    /// 切换时复用 (派单 In #B 已知陷阱: 不每次创建新 surface). attach
    /// `CursorImageBuffer` (`wayland_cursor` 内部从 shm pool 划块) + commit
    /// 后通过 `wl_pointer.set_cursor(serial, Some(&cursor_surface), hx, hy)`
    /// 告诉 compositor 用此 surface 作为 cursor.
    cursor_surface: wl_surface::WlSurface,
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
    /// T-0602: 累积待应用的 scrollback line 数. Dispatch<WlPointer> /
    /// Dispatch<WlKeyboard> 拿不到 term (term 在 LoopData 不在 State), 所以
    /// 把 Scroll(±N) 暂存于此, 由 idle callback (有 LoopData 全字段访问) 一次
    /// 性消费 + 调 `term.scroll_display(n)` + 清零. 累积语义: 同帧多次滚轮 +
    /// PgUp 合并为一次 scroll_display, 避免 alacritty 内部多次 mark_fully_damaged.
    /// 单帧 (16 ms) 内多次累积是常态 (滚轮 + 触摸板 axis 一次 frame 可发多 event).
    pending_scroll_lines: i32,
    // T-0505: zwp_text_input_v3 (TIv3) IME 协议绑定 (fcitx5 / ibus 中文输入).
    text_input_manager: Option<ZwpTextInputManagerV3>,
    text_input: Option<ZwpTextInputV3>,
    ime_state: ImeState,
    /// T-0603 keyboard repeat: `Dispatch<WlKeyboard>` 收到
    /// `KeyboardAction::StartRepeat / StopRepeat` 时只能拿到 `&mut State`,
    /// 拿不到 `LoopHandle` (在 LoopData 里). 把请求暂存到本字段, 让
    /// [`drive_wayland`] 在 `dispatch_pending` 之后调
    /// [`apply_repeat_request`] 真消费 (那里 `&mut LoopData` 字段可用).
    /// 与 `core.resize_dirty` (INV-006) 同套路 — 协议事件 set 单次延迟请求,
    /// 单一上游消费者推副作用. drop 顺序: `RepeatScheduleRequest` 是 POD enum,
    /// 无 GPU / wayland 引用, 顺序无关.
    pending_repeat: Option<RepeatScheduleRequest>,
    /// T-0607: 鼠标选区状态. anchor / cursor / mode / active / has_selection
    /// 全藏在 SelectionState 内, 字段私有 (INV-010 类型隔离). 渲染 / 复制路径
    /// 走公共 method (selected_cells_linear / extract_selection_text).
    pub(crate) selection_state: SelectionState,
    /// T-0607: 待发的 selection 操作 (SetPrimary / SetClipboard). 由
    /// [`apply_selection_op`] 在 dispatch_pending 后真 trigger 协议 request.
    pub(crate) pending_selection_op: Option<SelectionOp>,
    /// T-0607: 待发的粘贴请求. 中键 → Primary, Ctrl+Shift+V → Clipboard.
    /// 由 [`apply_paste_request`] 在 dispatch_pending 后调 offer.receive +
    /// 启动 calloop Generic source 异步读 pipe.
    pub(crate) pending_paste_request: Option<PasteSource>,
    /// T-0607: 待发的 autoscroll 操作. 由 [`apply_autoscroll_op`] 在
    /// dispatch_pending 后真 schedule / cancel calloop Timer.
    pub(crate) pending_autoscroll_op: Option<AutoScrollOp>,
    /// T-0607: 待发的 autoscroll cancel 标志. SelectionEnd / AutoScrollStop
    /// 走此置 true.
    pub(crate) pending_autoscroll_cancel: bool,
    /// T-0607 PRIMARY 协议: device_manager 工厂 (`zwp_primary_selection_device_manager_v1`).
    /// compositor 不导出 (老 wlroots / 极少 case) → None, PRIMARY 路径退化 (仅
    /// CLIPBOARD 工作 + warn log, 派单 In #F 已知陷阱).
    primary_selection_manager: Option<ZwpPrimarySelectionDeviceManagerV1>,
    /// T-0607 PRIMARY 协议: device per seat. SeatHandler::new_capability
    /// (Pointer) 时, manager 已 bind 则 get_device 一次, remove 时 destroy.
    primary_selection_device: Option<ZwpPrimarySelectionDeviceV1>,
    /// T-0607 PRIMARY 协议: 当前最新的 incoming offer (compositor 推送某 client
    /// 设的 selection 给我们). `Dispatch<ZwpPrimarySelectionDeviceV1>` 收到
    /// data_offer event 时 take 旧 + 存新, selection event 时 mark 当前 offer
    /// 为"selection owner". 中键粘贴 (Paste(Primary)) 走此 offer 的 receive.
    primary_current_offer: Option<ZwpPrimarySelectionOfferV1>,
    /// T-0607 CLIPBOARD 协议: data_device_manager 工厂. 现代 compositor 都导出
    /// (核心协议, 非 unstable), 极罕见 None.
    data_device_manager: Option<WlDataDeviceManager>,
    /// T-0607 CLIPBOARD 协议: data_device per seat. 与 primary_selection_device
    /// 同生命周期 (跟 Pointer capability).
    data_device: Option<WlDataDevice>,
    /// T-0607 CLIPBOARD 协议: 当前最新的 wl_data_offer.
    data_current_offer: Option<WlDataOffer>,
    /// T-0607 hotfix: 当前持有的 PRIMARY source. set_selection 后必须 store
    /// 防 wayland-client Proxy drop 时发 destroy → compositor cancel selection
    /// → 内容立刻清空. 新 set_selection 替换时旧 source 自然 drop (compositor
    /// 已 cache send 过的数据); cancelled event 来时显式 take 清理。
    active_primary_source: Option<ZwpPrimarySelectionSourceV1>,
    /// T-0607 hotfix: 当前持有的 CLIPBOARD source. 同 active_primary_source
    /// 路径 (CLIPBOARD compositor 多 lazy fetch, 不 store 直接 drop 后粘贴方
    /// 拿到空, 用户实测 Ctrl+Shift+V 粘贴出空格的真因)。
    active_data_source: Option<WlDataSource>,
    /// T-0607: 最近一次复制的选区文本. 创建 source 后 compositor 在 send event
    /// 中 ask 我们写数据到 fd, 此时读此字段写入. PRIMARY / CLIPBOARD 共享
    /// (语义上同一段选区), 复制时同步更新.
    ///
    /// **DEPRECATED 2026-04-26 hotfix**: 保留字段防 schema 破坏既有引用, 但
    /// PRIMARY / CLIPBOARD 各自走 last_primary_text / last_clipboard_text 防
    /// 覆盖 race (用户实测 SelectionEnd 自动 SetPrimary 把 CLIPBOARD 内容覆
    /// 盖成 1 字节空格).
    pub(crate) last_selection_text: Option<String>,
    /// T-0607 hotfix: PRIMARY 专用最近选区文本. SetPrimary 路径设, PRIMARY
    /// source.Send dispatch 读. 与 CLIPBOARD 解耦防共享 race.
    pub(crate) last_primary_text: Option<String>,
    /// T-0607 hotfix: CLIPBOARD 专用最近选区文本. SetClipboard 路径设, CLIPBOARD
    /// source.Send dispatch 读. 与 PRIMARY 解耦.
    pub(crate) last_clipboard_text: Option<String>,
    /// T-0608: 多 tab 集合 + active 索引. 替代原 `pty: Option<PtyHandle>` +
    /// `LoopData.term: Option<TermState>` 单 tab 字段对. 每个 [`TabInstance`]
    /// 持自己的 term + pty (派单 In #A).
    ///
    /// **why Option**: ctor 路径分两步 (先建 State 给 sctk delegate 用, 后
    /// spawn 第一个 tab 注入). 启动后**永远 Some** — 进入 event_loop.run 前
    /// 已注入, drive_wayland 路径全可 unwrap. 用 [`State::tabs_unchecked`] /
    /// [`State::tabs_unchecked_mut`] 单一访问点封装 unwrap, 保留 INV-010
    /// "下游不直接读 Option 内部" 类型隔离.
    ///
    /// **位于 State 最后一位** (与原 `pty` 同位置, 保留 INV-001 / INV-008 drop
    /// 序契约): TabInstance 内部 term → pty 顺序 (term drop 释放 alacritty grid,
    /// 不持 wgpu / wl 资源; pty 按 INV-008 reader → master → child drop).
    pub(crate) tabs: Option<TabList>,
    /// T-0608: 待处理 tab 操作. Dispatch 路径写, drive_wayland step 3.8 消费
    /// (与 pending_selection_op / pending_repeat 同套路).
    pub(crate) pending_tab_op: Option<TabOp>,
    /// T-0608: tab drag 期间最后一次 motion 的 logical x. EndTabDrag 时按此 x
    /// 算 target_idx (派单 In #F).
    pub(crate) last_tab_drag_x: f64,
    /// T-0611: 当前最近创建的 incoming offer 的 mime 列表. compositor 协议序:
    /// `DataOffer` event 创建 offer 对象, 紧跟着 N 个 `Offer` events 推 mime,
    /// 然后 `Enter` (DnD) 或 `Selection` (CLIPBOARD) 把它声明为当前 owner.
    /// `Dispatch<WlDataDevice>::DataOffer` 重置本字段为空 Vec, 每个
    /// `Dispatch<WlDataOffer>::Offer` 推一条 mime, `Enter` 时 take 转给
    /// `dnd_current_offer_mimes`. CLIPBOARD selection 路径暂不读 mime (派单仅
    /// hardcode text/plain;charset=utf-8 paste).
    incoming_offer_mimes: Vec<String>,
    /// T-0611: 当前正活动的 DnD 拖入 offer (compositor 在 `Enter` 时 set, `Leave`
    /// 或 `Drop` 之后我们清). 与 `data_current_offer` (CLIPBOARD selection)
    /// 物理类型同 (`WlDataOffer`) 但语义独立 — DnD 拖动期是临时 offer, drop 时
    /// 真消费, leave 时 destroy 不消费.
    dnd_current_offer: Option<WlDataOffer>,
    /// T-0611: `Enter` 时从 `incoming_offer_mimes` 转入的 mime 列表 (拖入 source
    /// 提供的全 mime 列表). 用于 `Drop` 路径选 mime (text/uri-list 优先) 走
    /// `offer.receive`. `Leave` / `Drop` 之后清空.
    dnd_current_offer_mimes: Vec<String>,
    /// T-0611: 我们对当前 DnD offer 接受的 mime (从 `dnd_current_offer_mimes`
    /// 优先级选). `None` = 不接受 (调用 `offer.accept(serial, None)`, source
    /// 拖入会显示 "禁止" 光标; Drop 不消费). `Some(mime)` = 接受该 mime, 拖入
    /// 显示 "可放下" 光标.
    dnd_accepted_mime: Option<String>,
    /// T-0611: 拖入 Drop 待消费. `Dispatch<WlDataDevice>::Drop` 置 true, 由
    /// [`apply_drop_request`] 在 dispatch_pending 后调 `offer.receive` + 启动
    /// calloop Generic source 异步读 pipe → uri-list parse → shell escape →
    /// bracketed wrap → pty.write. 与 `pending_paste_request` 同 pipe 模式.
    pub(crate) pending_drop: bool,
}

impl State {
    /// T-0608: 拿 active tab 引用. 启动后 tabs 必 Some, 此处 unwrap_or panic
    /// 兜底 (实际不会触发, 保留 panic message 防 LoopData 改时漏注入).
    pub(crate) fn tabs_unchecked(&self) -> &TabList {
        self.tabs.as_ref().expect("tabs not initialized — bug")
    }
    pub(crate) fn tabs_unchecked_mut(&mut self) -> &mut TabList {
        self.tabs.as_mut().expect("tabs not initialized — bug")
    }
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
            // T-0702: 同步 xdg_toplevel.set_title 的值给 renderer (titlebar 中央
            // 显示). 当前 hardcode WINDOW_TITLE = "quill" 与 Renderer DEFAULT_TITLE
            // 默认值相同, 显式调一遍是 future-proof — Phase 7+ 接 cwd / 命令
            // watcher 时本调用点改 dynamic title 即可, render 路径无需改.
            r.set_title(WINDOW_TITLE.to_string());
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

            // T-0505: text-input-v3 跟 keyboard 同生命周期 (协议: TIv3 focus
            // 跟 wl_keyboard focus). 仅当 manager 已 bind (compositor 支持 +
            // fcitx5/ibus 启) 才 get text_input. 已有 text_input 时不重建
            // (防 capability 重 fire), 与 wl_keyboard 同套路。
            if self.text_input.is_none() {
                if let Some(manager) = self.text_input_manager.as_ref() {
                    let ti = manager.get_text_input(&seat, qh, ());
                    tracing::info!("zwp_text_input_v3 bound on seat (IME 启用)");
                    self.text_input = Some(ti);
                }
            }
        }
        // T-0504: Pointer 同 Keyboard 路径 — wl_seat::get_pointer 返 raw
        // WlPointer, 走自己的 Dispatch<WlPointer, ()> impl (INV-010, PointerState
        // 是 quill 自有, 不偷渡 SCTK pointer 类型). 记 seat 给 xdg_toplevel.move
        // 用 (StartMove 路径需 seat + serial).
        if capability == Capability::Pointer && self.pointer.is_none() {
            let ptr = seat.get_pointer(qh, ());
            tracing::info!("wl_seat capability Pointer 出现, wl_pointer 已绑定");
            // T-0703-fix: cursor_surface / cursor_theme 在启动期已就绪 (走
            // wl_cursor + xcursor theme, ADR 0009). new_capability 不需做额外
            // 绑定 — apply_cursor_shape 在 Dispatch<WlPointer> 直接读
            // self.cursor_theme + self.cursor_surface, 走 wl_pointer.set_cursor.
            // T-0607: PRIMARY selection device per seat. manager 缺失 (老 compositor)
            // 时 device 留 None, 选区结束时 SetPrimary 自动跳过 (apply_selection_op
            // 守门).
            if self.primary_selection_device.is_none() {
                if let Some(manager) = self.primary_selection_manager.as_ref() {
                    let device = manager.get_device(&seat, qh, ());
                    tracing::info!("zwp_primary_selection_device_v1 已绑定 (PRIMARY 选区 active)");
                    self.primary_selection_device = Some(device);
                }
            }
            // T-0607: CLIPBOARD data_device per seat.
            if self.data_device.is_none() {
                if let Some(manager) = self.data_device_manager.as_ref() {
                    let device = manager.get_data_device(&seat, qh, ());
                    tracing::info!("wl_data_device 已绑定 (CLIPBOARD 选区 active)");
                    self.data_device = Some(device);
                }
            }
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
            // T-0505: text-input-v3 跟 keyboard 同生命周期; 协议 destroy
            // 自动通过 ZwpTextInputV3 的 Drop 触发 (wayland-protocols 生成
            // 的 Drop 实现内部调 destructor request)。take 让 Option 变 None,
            // 后续 keyboard 重 bind 时也重 get text_input。
            if self.text_input.take().is_some() {
                tracing::info!("zwp_text_input_v3 释放 (随 keyboard capability 移除)");
            }
        }
        if capability == Capability::Pointer {
            // T-0703-fix: 不再有 wp_cursor_shape_device_v1 需要释放 (ADR 0009
            // 撤回). cursor_surface / cursor_theme 不绑 pointer 生命周期 — 留
            // 给整进程, 下一次 Pointer capability 出现时直接复用.
            if let Some(ptr) = self.pointer.take() {
                ptr.release();
                tracing::info!("wl_seat capability Pointer 移除, wl_pointer 释放");
            }
            // pointer_seat 暂留 — 极端 race 下 seat 可能仍持续 (compositor 只
            // 移 capability 而 seat 本身仍存); remove_seat 路径再清.
        }
    }

    fn remove_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat) {
        // seat 整个移除 — wl_keyboard / wl_pointer / text_input 一并失效。
        if let Some(kb) = self.keyboard.take() {
            kb.release();
        }
        // T-0703-fix: cursor_theme / cursor_surface 不绑 seat — 整进程一份,
        // 下个 seat 重新出现时直接复用.
        if let Some(ptr) = self.pointer.take() {
            ptr.release();
        }
        self.pointer_seat = None;
        // T-0505: text-input 跟 keyboard 同生命周期。
        let _ = self.text_input.take();
    }
}

/// T-0505: 把 cell cursor 位置 (col, line) 与 cell pixel size 翻译成 TIv3
/// `set_cursor_rectangle` 的 logical px surface 坐标。
///
/// **why logical px** (TIv3 协议要求): 协议明示
/// `set_cursor_rectangle(x, y, w, h)` 是 surface 局部 logical 坐标,
/// compositor 自己加 HiDPI scale 转 physical 给 fcitx5 弹窗定位。`HIDPI_SCALE`
/// 常数仅影响 `wgpu::Surface::configure` 的 backing buffer (T-0404), 与协议
/// logical px 无关。
///
/// **抽纯 fn** (conventions §3): 跟 `cells_from_surface_px` 同套路, headless
/// 单测覆盖几个边界 (col=0/79, line=0/23) → CursorRectangle 决策, 不依赖
/// LoopData / 真 wayland 连接。
pub(crate) fn cursor_rectangle_for_cell(col: usize, line: usize) -> CursorRectangle {
    // logical px (跟 wayland 协议层单位一致). Phase 5 cell px 仍 hardcode
    // (CELL_W_PX/CELL_H_PX), Phase 4 字体 metrics 测量后会改 — 届时本 fn
    // 跟 `cells_from_surface_px` 同步换字体真实 advance 宽度。
    let x = (col as f32 * crate::wl::render::CELL_W_PX) as i32;
    let y = (line as f32 * crate::wl::render::CELL_H_PX) as i32;
    let width = crate::wl::render::CELL_W_PX as i32;
    let height = crate::wl::render::CELL_H_PX as i32;
    CursorRectangle {
        x,
        y,
        width,
        height,
    }
}

/// **T-0703-fix `Dispatch<WlShm>`**: wl_shm 协议会推 `format` event 列出
/// compositor 支持的像素格式 (Argb8888 / Xrgb8888 必有). `wayland_cursor` 内部
/// 走 Argb8888 + alpha, 不依赖此 event 决策, 直接吞.
impl Dispatch<WlShm, ()> for State {
    fn event(
        _state: &mut Self,
        _proxy: &WlShm,
        _event: wl_shm::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        // wl_shm.format event — 列举支持格式. wayland-cursor 用 ARGB8888
        // 标准格式 (任何 wayland compositor 必支持), 不需要枚举确认.
    }
}

/// T-0703-fix: 应用 cursor 形状变化到 wl_pointer (ADR 0009).
///
/// 流程:
/// 1. 从 [`xcursor_names_for`] 拿 fallback name 列表
/// 2. 顺序尝试 `theme.get_cursor(name)`, 第一个成功的拿来用
/// 3. attach `CursorImageBuffer` 到 cursor surface + commit
/// 4. 调 `wl_pointer.set_cursor(serial, Some(&cursor_surface), hx/scale, hy/scale)`
///
/// **why 单点 fn 而非内联**: 调用方 (`Dispatch<WlPointer>`) 已较长, 抽出来
/// 让 fn body 自描述 cursor pipeline. 不变量也集中: theme miss 走 noop log
/// (cursor 维持上一次状态), HiDPI scale 跟随 surface (硬编 [`HIDPI_SCALE`]).
///
/// **协议 corner case** (派单已知陷阱):
/// - cursor_surface 复用 (一个 wl_pointer 一个), 多次 attach 新 buffer + commit,
///   不创新 surface. 协议要求 surface role 一旦设为 cursor 不能再改.
/// - 全 fallback name 失败 → `cursor_surface` 不动, 上次 attach 的 buffer 仍
///   是 cursor (compositor 行为). log warn 一次防排障难, 不刷屏.
/// - HiDPI: cursor surface buffer scale 跟 主 surface scale 一致 ([`HIDPI_SCALE`]
///   = 2 硬编, T-0502 决策); hotspot / 视觉 size 自动 1:1.
fn apply_cursor_shape(
    pointer: &wl_pointer::WlPointer,
    cursor_surface: &wl_surface::WlSurface,
    theme: &mut CursorTheme,
    serial: u32,
    shape: CursorShape,
    scale: u32,
) {
    let names = xcursor_names_for(shape);
    for name in names {
        let cursor = match theme.get_cursor(name) {
            Some(c) => c,
            None => continue,
        };
        // CursorImageBuffer 实现 Deref<Target = WlBuffer> + dimensions / hotspot.
        // 单帧 cursor 只用 [0] (静态 cursor 帧动画暂不接, 派单 Out 段无明示但
        // resize/text 类 cursor 都是单帧, daily-drive 不缺).
        let buf = &cursor[0];
        let (w, h) = buf.dimensions();
        let (hx, hy) = buf.hotspot();
        // scale 至少 1 防 div-by-zero (实际 HIDPI_SCALE = 2 起步, 防御足).
        let scale_clamped = scale.max(1) as i32;
        cursor_surface.set_buffer_scale(scale_clamped);
        cursor_surface.attach(Some(buf), 0, 0);
        // damage_buffer 是 wl_surface v4+ 接口 (按 buffer 像素坐标, 非 logical).
        // sctk 0.19 暴露的 cursor_surface (compositor.create_surface) version 跟
        // wl_compositor 协商 — 现代 mutter / kwin / sway 都 v6+, damage_buffer
        // 安全调用. 老 compositor (v < 4) 的 fallback 见 sctk 自己的实现 (走
        // damage(0,0,w/scale,h/scale)), 这里硬走 damage_buffer 简化, 老用户
        // 不在 quill daily-drive 范围.
        if cursor_surface.version() >= 4 {
            cursor_surface.damage_buffer(0, 0, w as i32, h as i32);
        } else {
            cursor_surface.damage(0, 0, (w as i32) / scale_clamped, (h as i32) / scale_clamped);
        }
        cursor_surface.commit();
        let scale_u32 = scale_clamped as u32;
        pointer.set_cursor(
            serial,
            Some(cursor_surface),
            (hx / scale_u32) as i32,
            (hy / scale_u32) as i32,
        );
        tracing::trace!(
            target: "quill::cursor",
            ?shape,
            name,
            serial,
            scale,
            "wl_pointer.set_cursor (xcursor name resolved)"
        );
        return;
    }
    // 全 fallback 都失败. 派单已知陷阱: log warn 一次, 不刷屏 (调用方 idle
    // 取 pending 后调本 fn, 一次 hover 跨区只来一次).
    tracing::warn!(
        target: "quill::cursor",
        ?shape,
        names = ?names,
        "xcursor theme 缺所有 fallback name; cursor 维持上一次形状 (检查 \
         /usr/share/icons/{{Adwaita,default}}/cursors/ 资源)"
    );
}

// ---------- T-0607 PRIMARY selection 协议 Dispatch impls ----------

/// `Dispatch<ZwpPrimarySelectionDeviceManagerV1>`: 工厂对象, 协议零 event.
impl Dispatch<ZwpPrimarySelectionDeviceManagerV1, ()> for State {
    fn event(
        _state: &mut Self,
        _proxy: &ZwpPrimarySelectionDeviceManagerV1,
        _event: zwp_primary_selection_device_manager_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

/// `Dispatch<ZwpPrimarySelectionDeviceV1>`: 接受 compositor 推 data_offer +
/// selection events. selection event 标记当前 incoming offer 为 owner (中键
/// 粘贴时读此).
impl Dispatch<ZwpPrimarySelectionDeviceV1, ()> for State {
    fn event(
        state: &mut Self,
        _proxy: &ZwpPrimarySelectionDeviceV1,
        event: zwp_primary_selection_device_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        match event {
            zwp_primary_selection_device_v1::Event::DataOffer { offer } => {
                // 协议: data_offer event 是 selection event 的 prefix (compositor
                // 先发 data_offer 创建 offer 对象, 再发 selection 把它声明为当前
                // selection). 我们这里仅记录, selection event 才真把它作为
                // current_offer.
                tracing::trace!(
                    target: "quill::pointer",
                    "PRIMARY data_offer received (待 selection 事件确认)"
                );
                // 防御性 destroy 旧 (compositor 一般会发 selection(None) 替换,
                // 这里如果重复 data_offer 而 selection 未到, 旧 offer 没被引用 —
                // wayland-client 自管 drop).
                let _ = offer;
            }
            zwp_primary_selection_device_v1::Event::Selection { id } => {
                // selection event 给当前 selection owner (id=Some) 或清 (None).
                if let Some(prev) = state.primary_current_offer.take() {
                    prev.destroy();
                }
                state.primary_current_offer = id;
                tracing::debug!(
                    target: "quill::pointer",
                    has_offer = state.primary_current_offer.is_some(),
                    "PRIMARY selection event"
                );
            }
            _ => {}
        }
    }

    // 协议要求: data_offer event 携带 NewId<ZwpPrimarySelectionOfferV1>, 调用方
    // 必须实现 event_created_child 路由让新对象 Dispatch 到自己. 这里走 self.
    wayland_client::event_created_child!(State, ZwpPrimarySelectionDeviceV1, [
        zwp_primary_selection_device_v1::EVT_DATA_OFFER_OPCODE => (ZwpPrimarySelectionOfferV1, ()),
    ]);
}

/// `Dispatch<ZwpPrimarySelectionOfferV1>`: 收到 compositor 推 offer 提供的
/// MIME 类型. 我们仅关心 text/plain (UTF-8); 其它无视.
impl Dispatch<ZwpPrimarySelectionOfferV1, ()> for State {
    fn event(
        _state: &mut Self,
        _proxy: &ZwpPrimarySelectionOfferV1,
        event: zwp_primary_selection_offer_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        if let zwp_primary_selection_offer_v1::Event::Offer { mime_type } = event {
            tracing::trace!(target: "quill::pointer", mime = %mime_type, "PRIMARY offer mime");
        }
    }
}

/// `Dispatch<ZwpPrimarySelectionSourceV1>`: 我们创建的 source 收到 compositor
/// 反向 event — Send (要写数据到 fd) / Cancelled (其它 client 设了 selection).
impl Dispatch<ZwpPrimarySelectionSourceV1, ()> for State {
    fn event(
        state: &mut Self,
        _proxy: &ZwpPrimarySelectionSourceV1,
        event: zwp_primary_selection_source_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        match event {
            zwp_primary_selection_source_v1::Event::Send { mime_type, fd } => {
                // T-0607 hotfix: 走 last_primary_text 而非共享 last_selection_text,
                // 防 SetClipboard 路径覆盖 PRIMARY 内容.
                write_selection_to_fd(state.last_primary_text.as_deref(), &mime_type, fd);
            }
            zwp_primary_selection_source_v1::Event::Cancelled => {
                // 其它 client 设了 PRIMARY, quill 失去所有权 — drop 旧 source
                // 释放内存. 用户视觉无变化 (仍能看自己的选区高亮, 但 PRIMARY
                // paste 拿到的是其它 client 内容).
                state.active_primary_source = None;
                tracing::trace!(target: "quill::pointer", "PRIMARY source cancelled");
            }
            _ => {}
        }
    }
}

// ---------- T-0607 CLIPBOARD (wl_data_device) 协议 Dispatch impls ----------

/// `Dispatch<WlDataDeviceManager>`: 工厂对象, 协议零 event.
impl Dispatch<WlDataDeviceManager, ()> for State {
    fn event(
        _state: &mut Self,
        _proxy: &WlDataDeviceManager,
        _event: wl_data_device_manager::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

/// `Dispatch<WlDataDevice>`: 接 data_offer + selection events (与 PRIMARY device
/// 同套路, 仅协议 path 不同). T-0611 起 + DnD 路径 (Enter / Motion / Drop /
/// Leave) — 与 CLIPBOARD selection 共享 `Dispatch<WlDataOffer>` 但 owner 状态独立
/// (`dnd_current_offer` vs `data_current_offer`).
impl Dispatch<WlDataDevice, ()> for State {
    fn event(
        state: &mut Self,
        _proxy: &WlDataDevice,
        event: wl_data_device::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        match event {
            wl_data_device::Event::DataOffer { id } => {
                // 协议: DataOffer 创建新 WlDataOffer 对象, 紧跟着 N 个 Offer
                // events 推 mime, 然后 Enter (DnD) / Selection (CLIPBOARD) 标
                // 记 owner. 重置 incoming_offer_mimes 以 capture 新 offer 的
                // mime 列表 (T-0611 In #B).
                state.incoming_offer_mimes.clear();
                tracing::trace!(target: "quill::pointer", "data_offer received (CLIPBOARD or DnD)");
                let _ = id;
            }
            wl_data_device::Event::Selection { id } => {
                if let Some(prev) = state.data_current_offer.take() {
                    prev.destroy();
                }
                state.data_current_offer = id;
                tracing::debug!(
                    target: "quill::pointer",
                    has_offer = state.data_current_offer.is_some(),
                    "CLIPBOARD selection event"
                );
            }
            // T-0611 DnD: compositor 通知拖入 surface (incoming_offer_mimes
            // 已由前序 Offer events 填充). 选 mime + accept(serial, mime) 通知
            // source 我们接受 — wayland-client 0.31 里 wl_data_offer.accept 是
            // method, 接 (serial, Option<String>).
            wl_data_device::Event::Enter {
                serial,
                surface: _,
                x,
                y,
                id,
            } => {
                // 接管 mime 列表 + offer.
                state.dnd_current_offer_mimes = std::mem::take(&mut state.incoming_offer_mimes);
                // 旧 DnD offer (race / 未 Leave) 兜底 destroy.
                if let Some(prev) = state.dnd_current_offer.take() {
                    prev.destroy();
                }
                state.dnd_current_offer = id;
                // T-0611 In #B: 选优先 mime. text/uri-list > application/x-kde-cutselection >
                // text/plain;charset=utf-8 > text/plain. 没匹配 → None (不接受).
                let chosen = pick_dnd_mime(&state.dnd_current_offer_mimes);
                if let Some(offer) = state.dnd_current_offer.as_ref() {
                    // accept(serial, Option<String>) — None 拒绝, Some 接受.
                    offer.accept(serial, chosen.clone());
                    // T-0611 hotfix: 必须调 set_actions(Copy, Copy) 否则 mutter
                    // 默认 client 不接受 drop, cursor 显示禁止, 用户释放鼠标
                    // 直接发 Leave 不发 Drop. user 实测 log 见过 enter/leave
                    // 反复但从无 Drop event 即此 bug. wl_data_device v3+ 协议
                    // 要求 client 显式 set_actions, copy/move/ask 三选一,
                    // quill 走 copy (拖文件入终端是复制路径不是移动).
                    if chosen.is_some() {
                        use wayland_client::protocol::wl_data_device_manager::DndAction;
                        offer.set_actions(DndAction::Copy, DndAction::Copy);
                    }
                }
                state.dnd_accepted_mime = chosen.clone();
                tracing::debug!(
                    target: "quill::pointer",
                    x, y, mimes = ?state.dnd_current_offer_mimes,
                    accepted = ?chosen,
                    "DnD enter (set_actions=Copy if accepted)"
                );
            }
            wl_data_device::Event::Motion { time, x, y } => {
                // 派单 Out: hover 高亮拖入区是 P2, 这里仅 trace. 不消费副作用.
                let _ = (time, x, y);
                tracing::trace!(target: "quill::pointer", time, x, y, "DnD motion");
            }
            wl_data_device::Event::Drop => {
                // 协议: Drop 表示 source 已松开. 我们若已 accept 一个 mime, 触
                // 发 receive (走 pipe 异步读). pending_drop 让 drive_wayland
                // step 3.7 真消费 (Dispatch 路径拿不到 LoopHandle).
                state.pending_drop = true;
                tracing::debug!(
                    target: "quill::pointer",
                    accepted = ?state.dnd_accepted_mime,
                    "DnD drop event (pending_drop set)"
                );
            }
            wl_data_device::Event::Leave => {
                // 协议: Leave 通知拖出 surface. **但 Drop event 之后也立即跟一个
                // Leave** (drag session 结束), 此时 pending_drop=true, 我们仍要
                // 异步 receive 走 apply_drop_request, 不能清 offer / accepted_mime.
                // T-0611 hotfix v2: 检 pending_drop, 正在 drop 时跳过清理, 留给
                // drop_read_tick EOF 路径 take + finish + destroy.
                if state.pending_drop {
                    tracing::debug!(target: "quill::pointer", "DnD leave (during drop, 不清状态留给 apply_drop_request)");
                } else {
                    if let Some(offer) = state.dnd_current_offer.take() {
                        offer.destroy();
                    }
                    state.dnd_current_offer_mimes.clear();
                    state.dnd_accepted_mime = None;
                    tracing::debug!(target: "quill::pointer", "DnD leave (cancel, 拖出未 drop)");
                }
            }
            _ => {}
        }
    }

    wayland_client::event_created_child!(State, WlDataDevice, [
        wl_data_device::EVT_DATA_OFFER_OPCODE => (WlDataOffer, ()),
    ]);
}

/// `Dispatch<WlDataOffer>`: 收到 offer mime types. CLIPBOARD selection / DnD
/// Enter 共享同款 WlDataOffer 协议对象, 由后续 device event (`Selection` /
/// `Enter`) 才决定 owner. T-0611: Offer 事件都累积进 `incoming_offer_mimes`,
/// 让 `Enter` / `Selection` 路径 take.
///
/// 派单 Out P3 还有 source_actions / action 等 event, 这里不消费.
impl Dispatch<WlDataOffer, ()> for State {
    fn event(
        state: &mut Self,
        _proxy: &WlDataOffer,
        event: wl_data_offer::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        if let wl_data_offer::Event::Offer { mime_type } = event {
            tracing::trace!(target: "quill::pointer", mime = %mime_type, "data offer mime");
            state.incoming_offer_mimes.push(mime_type);
        }
    }
}

/// T-0611: 从 source offer 的 mime 列表选最优 mime 我们能消费.
///
/// 优先级 (派单 In #B):
/// 1. `text/uri-list` — RFC 2483 标准 file:// URI per line, Nautilus / Files /
///    KDE 都给.
/// 2. `application/x-kde-cutselection` — KDE 文件管理特有 (跟 text/uri-list 同
///    格式, 历史包袱).
/// 3. `text/plain;charset=utf-8` — 拖文本 (非 file) 的 fallback. 真"拖文本块"
///    场景, 把字面文本 paste 进来.
/// 4. `text/plain` — 老式无 charset 的文本.
///
/// 没匹配返 `None` — 例如拖图片只给 `image/png`, 我们不消费.
///
/// **why 字符串字面量比较 而非 enum**: mime 类型可扩展 (Phase 8 可能加 image/*),
/// 字面量列表更易维护. 不引入 mime crate (派单约束).
fn pick_dnd_mime(mimes: &[String]) -> Option<String> {
    const PRIORITY: &[&str] = &[
        "text/uri-list",
        "application/x-kde-cutselection",
        "text/plain;charset=utf-8",
        "text/plain",
    ];
    for want in PRIORITY {
        for m in mimes {
            if m == want {
                return Some((*want).to_string());
            }
        }
    }
    None
}

/// `Dispatch<WlDataSource>`: 我们创建的 CLIPBOARD source 反向 event.
impl Dispatch<WlDataSource, ()> for State {
    fn event(
        state: &mut Self,
        _proxy: &WlDataSource,
        event: wl_data_source::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        match event {
            wl_data_source::Event::Send { mime_type, fd } => {
                // T-0607 hotfix: 走 last_clipboard_text 而非共享 last_selection_text,
                // 防 SetPrimary 路径 (鼠标松开自动) 覆盖 CLIPBOARD 内容 — 用户实
                // 测 Ctrl+Shift+C 后误动鼠标导致 paste 出空格的真因.
                write_selection_to_fd(state.last_clipboard_text.as_deref(), &mime_type, fd);
            }
            wl_data_source::Event::Cancelled => {
                state.active_data_source = None;
                tracing::trace!(target: "quill::pointer", "CLIPBOARD source cancelled");
            }
            _ => {}
        }
    }
}

/// T-0607: compositor 通过 source 的 Send event ask 我们把 selection 数据写到
/// 它给的 fd. 我们走非阻塞 write (一次性写完, kernel pipe buffer 通常 ≥ 64 KiB
/// 远超选区文本长度, 不会卡). 写完关 fd 让 reader (compositor / 接收 client)
/// 拿到 EOF.
///
/// **why 一行 write 而非 calloop async**: 选区文本典型 < 4 KiB, kernel pipe
/// buffer 默认 64 KiB 远超, 一次 write 必成. 真大 paste (>1 MiB 命令输出复制)
/// 罕见, Phase 6 加非阻塞 + chunked write. KISS.
///
/// **mime_type 校验**: quill 仅 offer text/plain;charset=utf-8 + UTF8_STRING +
/// TEXT 三个 (派单 In #F + #G), 接到 compositor send 任何这三个之一都返同样
/// UTF-8 字节. 其它 mime (e.g. image/png) 不该来 (compositor 不会发 client 没
/// offer 过的 mime), 防御性吞掉 + warn.
fn write_selection_to_fd(text: Option<&str>, mime_type: &str, fd: std::os::fd::OwnedFd) {
    let bytes = text.unwrap_or("").as_bytes();
    let recognized = matches!(
        mime_type,
        "text/plain;charset=utf-8" | "text/plain" | "UTF8_STRING" | "TEXT"
    );
    if !recognized {
        tracing::warn!(
            target: "quill::pointer",
            mime = %mime_type,
            "selection source send: 未知 mime 类型, 拒绝写入 (fd 直接关)"
        );
        // OwnedFd drop 自动 close, reader 收 EOF (空数据).
        return;
    }
    // SAFETY: fd 是 OwnedFd, 我们获得所有权 (从 wayland-client). std::fs::File
    // ::from(fd) 接管 fd (同样所有权), drop 关 fd. write_all 走非阻塞但 kernel
    // pipe buffer ≥ 64 KiB 一次必成.
    use std::io::Write;
    let mut file = std::fs::File::from(fd);
    if let Err(e) = file.write_all(bytes) {
        tracing::warn!(
            target: "quill::pointer",
            error = %e,
            n = bytes.len(),
            "selection source send: write 失败 (compositor 已掉?)"
        );
    } else {
        tracing::debug!(
            target: "quill::pointer",
            n = bytes.len(),
            mime = %mime_type,
            "selection source: wrote bytes to compositor"
        );
    }
    // file drop 自动 close fd, reader 收 EOF.
}

/// **`Dispatch<ZwpTextInputManagerV3>`**: manager 是工厂对象, 自身无事件
/// (协议: zwp_text_input_manager_v3 只有 destroy / get_text_input 两个
/// request, 零 event)。Dispatch::event 不应被调用; 防御性写法 — 收到协议
/// 错误事件 trace warn 不 panic。
impl Dispatch<ZwpTextInputManagerV3, ()> for State {
    fn event(
        _state: &mut Self,
        _proxy: &ZwpTextInputManagerV3,
        _event: zwp_text_input_manager_v3::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        // zwp_text_input_manager_v3 协议 0 event, 此 impl 仅满足 Dispatch
        // trait bound (ZwpTextInputManagerV3 是 Proxy trait, 必须有 Dispatch).
        // wayland-protocols Event enum 也是空 (`Event {}`), match 任何分支
        // 都触发, 此处无操作。
    }
}

/// **`Dispatch<ZwpTextInputV3>`** (T-0505 主路径): TIv3 atomic 协议事件 →
/// [`handle_text_input_event`] → [`ImeAction`] → 副作用分派 (PTY write /
/// preedit dirty / enable+commit / cursor_rect)。
///
/// 类型隔离 (INV-010): `event: zwp_text_input_v3::Event` 入参是
/// wayland-protocols 协议类型 (已在 quill 公共 API 边界 — Dispatch trait
/// 强制); 出参 [`ImeAction`] 是 quill 自有 enum, 不漏 wayland-protocols 类型。
/// 与 `Dispatch<wl_keyboard::WlKeyboard>` 同套路 (T-0501)。
///
/// **PTY write 路径**: `ImeAction::Commit(bytes)` → `state.pty.write(&bytes)`,
/// 与 wl_keyboard 同 PTY 路径 (INV-009 master fd O_NONBLOCK, INV-005 不重试,
/// 派单 In #D 背压丢字节)。
///
/// **enable+commit 协议要求**: TIv3 协议明示 client 必须在 enter 后调
/// `enable() + commit()` 才能接收 preedit/commit event。LeaveFocus 时调
/// `disable() + commit()` 释放 fcitx5 grab 让其它 client 用。
///
/// **preedit dirty**: `ImeAction::UpdatePreedit` 不立即重绘 (避免在 wayland
/// dispatch 路径里调 wgpu, 与 INV-007 WindowCore 纯逻辑同思路) — 仅置
/// `core.resize_dirty` 触发 idle callback 重绘。复用 `resize_dirty` 标志
/// 而非新加 `preedit_dirty` (idle callback 已用 `term.is_dirty()` 决定重绘,
/// 这里同步置 `term.set_dirty()` 也是合法路径 — 但 alacritty Term 没有
/// `set_dirty` public API, 走 resize_dirty 间接强制下一帧 propagate +
/// idle 看 dirty=true)。**真简化**: ImeState 本身已存 current_preedit,
/// idle callback 每次 draw 都读它 — preedit 变化自然在下一帧反映, 不需要
/// 额外 dirty 标志 (idle 频率与 wayland event 同节奏, 协议 done 后下一次
/// idle 必走)。
impl Dispatch<ZwpTextInputV3, ()> for State {
    fn event(
        state: &mut Self,
        text_input: &ZwpTextInputV3,
        event: zwp_text_input_v3::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        let action = handle_text_input_event(event, &mut state.ime_state);
        apply_ime_action(action, text_input, state);
    }
}

/// 把 [`ImeAction`] 翻译成真副作用 (PTY write / 协议 request)。
///
/// 抽出来理由 (conventions §3): Composite variant 内部递归调用方便; Dispatch
/// callback 内 split-borrow 复杂 — 单独 fn 让 borrow 路径更清晰。
fn apply_ime_action(action: ImeAction, text_input: &ZwpTextInputV3, state: &mut State) {
    match action {
        ImeAction::Nothing => {}
        ImeAction::Commit(bytes) => {
            // 与 Dispatch<WlKeyboard> 同 PTY 路径; INV-009 master fd O_NONBLOCK,
            // INV-005 calloop 不重试 WouldBlock 直接丢 (派单 In 中文输入背压
            // 罕见, 一帧 done 字节 ≤ 12 (4 中文字符), kernel buffer 必能装下)。
            // T-0608: IME commit 走 active tab 的 PTY (preedit 跟 active tab,
            // keyboard focus 一致).
            let pty = state.tabs_unchecked().active().pty();
            match pty.write(&bytes) {
                Ok(n) if n == bytes.len() => {
                    tracing::debug!(
                        target: "quill::ime",
                        n,
                        "wrote IME commit bytes to pty"
                    );
                }
                Ok(n) => {
                    tracing::warn!(
                        target: "quill::ime",
                        wrote = n,
                        total = bytes.len(),
                        "pty.write IME commit 部分写入, 剩余字节丢弃 (背压)"
                    );
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    tracing::warn!(
                        target: "quill::ime",
                        n = bytes.len(),
                        "pty.write IME commit WouldBlock, 字节丢弃 (INV-005 不重试)"
                    );
                }
                Err(e) => {
                    tracing::warn!(
                        target: "quill::ime",
                        error = %e,
                        n = bytes.len(),
                        "pty.write IME commit 失败 (非 WouldBlock)"
                    );
                }
            }
        }
        ImeAction::UpdatePreedit {
            text,
            cursor_begin,
            cursor_end,
        } => {
            // current_preedit 已在 handle_text_input_event 内 apply 完成 (
            // ImeState 内部 mutation), 这里仅 trace。渲染层下次 idle callback
            // 走 draw_frame 时读 state.ime_state.current_preedit() 决定 preedit
            // 渲染。
            //
            // **why 不主动 trigger redraw**: idle callback 在每次 wayland
            // dispatch 后跑 (calloop EventLoop::run 第三 arg), preedit
            // 变化自然在下一 idle 反映 — fcitx5 done event 与 idle 间隔
            // < 1 frame, 用户感知 = 实时。Phase 6 若发现 preedit 卡顿可
            // 加显式 `core.preedit_dirty` 标志 + idle 优先级提升, 当前 KISS。
            tracing::debug!(
                target: "quill::ime",
                text = %text,
                cursor_begin,
                cursor_end,
                "preedit 更新 (下次 idle 渲染)"
            );
        }
        ImeAction::DeleteSurroundingText { before, after } => {
            // **派单 Out**: terminal 没有 client 侧文本 buffer (alacritty Term
            // 是 server 侧 grid, bash readline 才有 buffer 但跟 IME 不通).
            // 我们仅 trace, 不翻译成 PTY backspace 序列 — fcitx5 实测在
            // terminal 罕见触发 (拼音输入只 commit, delete_surrounding 主要
            // 给 GTK4 文本框删字 / 候选回退场景用). Phase 6 决定是否实装。
            tracing::debug!(
                target: "quill::ime",
                before,
                after,
                "DeleteSurroundingText 收到 — terminal 无 surrounding buffer, 派单 Out 跳过"
            );
        }
        ImeAction::EnterFocus => {
            // 协议要求: enter 后必须调 enable() + commit() 才能接收
            // preedit/commit event。set_content_type 给 fcitx5 提示 "这是
            // 终端" 让候选样式合适 (fcitx5 可能跳过自动大写之类干扰). content
            // hint 用 NONE (无特殊行为)。purpose 用 TERMINAL (协议 enum
            // value 13)。
            text_input.enable();
            text_input.set_content_type(
                zwp_text_input_v3::ContentHint::None,
                zwp_text_input_v3::ContentPurpose::Terminal,
            );
            // 立即上报 cursor_rectangle (focus 进入时 fcitx5 候选框需立即
            // 知道位置). 用 ime_state 缓存避免重复上报。
            //
            // 入参用 (0, 0) 占位 — 真 cursor 位置由 PTY 输出 / 用户交互推动
            // 时通过 `update_ime_cursor_rect_from_term` (idle callback 调用)
            // 持续更新, 此处仅给 fcitx5 一个非 default 的初值。
            // text_input.commit() 在路径末端 (协议要求每次 state 变化后 commit
            // 一次, 把 enable + content_type + cursor_rect 一并 atomic 推过去).
            let r = CursorRectangle {
                x: 0,
                y: 0,
                width: crate::wl::render::CELL_W_PX as i32,
                height: crate::wl::render::CELL_H_PX as i32,
            };
            if let Some(new) = state.ime_state.update_cursor_rectangle(r) {
                text_input.set_cursor_rectangle(new.x, new.y, new.width, new.height);
            }
            text_input.commit();
            tracing::info!(target: "quill::ime", "EnterFocus → enable + content_type + commit");
        }
        ImeAction::LeaveFocus => {
            // 协议: disable + commit 释放 fcitx5 grab. 不重置 ime_state (已
            // 在 handle_text_input_event::Leave 里清空 preedit + pending)。
            text_input.disable();
            text_input.commit();
            tracing::info!(target: "quill::ime", "LeaveFocus → disable + commit");
        }
        ImeAction::Composite(actions) => {
            // 按数组顺序逐个 apply — 协议规定 delete → commit → preedit。
            // 不会无限递归 (Composite 内不再含 Composite, handle_text_input_event
            // 内 apply_pending 仅生成 单一 variants 列表)。
            for a in actions {
                apply_ime_action(a, text_input, state);
            }
        }
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
        // T-0602: handle_key_event 需当前 viewport rows 算 PageUp/PageDown 半屏 /
        // 整屏量. State 拿不到 term (在 LoopData), 走 cells_from_surface_px
        // 推导 — 与 propagate_resize_if_dirty 同源 fn, 永远跟 surface 当前尺寸
        // 同步; resize 中 race 也只是 ±1 行差, 不影响功能.
        // T-0617: tab_count 入参决定 tab bar 高度 (单 tab 隐藏); 此路径只算 rows
        // 给 PgUp/PgDn, 单/多 tab 差仅 ~1 行, 用当前 tabs.len() 即可.
        let tab_count = state.tabs.as_ref().map(|t| t.len()).unwrap_or(1);
        let (_cols, rows) = cells_from_surface_px(state.core.width, state.core.height, tab_count);
        let rows_u16 = rows.min(u16::MAX as usize) as u16;
        let action = handle_key_event(event, &mut state.keyboard_state, rows_u16);
        match action {
            KeyboardAction::Nothing => {}
            KeyboardAction::Scroll(delta) => {
                // T-0602: 累积到 State.pending_scroll_lines, 由 idle callback 消费.
                // saturating_add 防极端连按 PgUp 溢出 i32 (实际不可能, 但廉价防御).
                state.pending_scroll_lines = state.pending_scroll_lines.saturating_add(delta);
                tracing::trace!(
                    target: "quill::keyboard",
                    delta,
                    pending = state.pending_scroll_lines,
                    "PageUp/PageDown queued for scrollback"
                );
            }
            KeyboardAction::WriteToPty(bytes) => {
                write_keyboard_bytes(state, &bytes);
            }
            // T-0603: Pressed → 立即写一次 (即时回显) + 设 pending_repeat=Start
            // 让 drive_wayland step 3.6 真 schedule timer.
            KeyboardAction::StartRepeat { bytes } => {
                write_keyboard_bytes(state, &bytes);
                state.pending_repeat = Some(RepeatScheduleRequest::Start);
            }
            // T-0603: Released 同 keycode 或 modifier 变化 → 设 pending_repeat=Stop
            // 让 drive_wayland step 3.6 真 cancel timer.
            KeyboardAction::StopRepeat => {
                state.pending_repeat = Some(RepeatScheduleRequest::Stop);
            }
            // T-0607 CLIPBOARD 热键. SetClipboard 走 selection_state 当前选区
            // (与 SelectionEnd 同 path 但 device 走 wl_data_device 而非 PRIMARY).
            // 选区为空 (用户没拖过 / 已 clear) 时仍 set_selection (空字符串),
            // 与 alacritty / foot 一致.
            KeyboardAction::CopyToClipboard => {
                state.pending_selection_op = Some(SelectionOp::SetClipboard);
                tracing::debug!(
                    target: "quill::keyboard",
                    "Ctrl+Shift+C → SetClipboard queued"
                );
            }
            KeyboardAction::PasteFromClipboard => {
                state.pending_paste_request = Some(PasteSource::Clipboard);
                tracing::debug!(
                    target: "quill::keyboard",
                    "Ctrl+Shift+V → Paste(Clipboard) queued"
                );
            }
            // T-0608: tab 热键 → 写 pending_tab_op, drive_wayland step 3.8 消费
            // (与 selection_op / repeat 同套路 — Dispatch 拿不到 LoopHandle, 推迟
            // 到 drive_wayland 由 apply_tab_op 真 spawn / register / remove fd).
            KeyboardAction::NewTab => {
                state.pending_tab_op = Some(TabOp::New);
            }
            KeyboardAction::CloseActiveTab => {
                state.pending_tab_op = Some(TabOp::CloseActive);
            }
            KeyboardAction::NextTab => {
                state.pending_tab_op = Some(TabOp::Next);
            }
            KeyboardAction::PrevTab => {
                state.pending_tab_op = Some(TabOp::Prev);
            }
            KeyboardAction::SwitchToTab(idx) => {
                state.pending_tab_op = Some(TabOp::Switch(idx));
            }
        }
    }
}

/// T-0603 + T-0501: 写 keyboard 字节到 PTY (Pressed 即时回显路径). 抽出来
/// 让 `KeyboardAction::WriteToPty` 与 `KeyboardAction::StartRepeat` 共用 — 两
/// 者都需要 "立即写一次" 语义, 与 INV-009 (master fd O_NONBLOCK) + INV-005
/// (calloop 不重试 WouldBlock) 一致.
fn write_keyboard_bytes(state: &State, bytes: &[u8]) {
    // T-0608: 走 active tab 的 PTY (键盘 focus 跟 active tab 一致).
    let pty = state.tabs_unchecked().active().pty();
    match pty.write(bytes) {
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
        // T-0607: 鼠标按下时 alt_active 影响 SelectionMode (Linear vs Block).
        // 走 keyboard_state.alt_active() 读"语义 Alt active" (effective mods,
        // 含 latched / locked, 兼容用户改 keymap 把 Alt 重映射). 非 button
        // event (motion / enter / axis) 不读此值, 但本路径单点入口 全部走
        // 同一调用避免分支.
        let alt_active = state.keyboard_state.alt_active();
        let action = handle_pointer_event(event, &mut state.pointer_state, alt_active);
        // T-0703-fix: 主 PointerAction 处理后, 顺手取出 pending_cursor_set 走
        // apply_cursor_shape (wl_cursor + xcursor theme + wl_pointer.set_cursor,
        // ADR 0009 — 单一消费者, 与 take_pending / pending_scroll_lines 同
        // T-0602/T-0603 套路). 放最前面以便 Enter event 也走 set_cursor 路径
        // (协议要求 enter 后必 set 一次, 否则 unspecified — 实测 mutter 显示
        // 空白).
        if let Some((serial, shape)) = state.pointer_state.take_pending_cursor_set() {
            match (state.cursor_theme.as_mut(), state.pointer.as_ref()) {
                (Some(theme), Some(ptr)) => {
                    apply_cursor_shape(
                        ptr,
                        &state.cursor_surface,
                        theme,
                        serial,
                        shape,
                        crate::wl::HIDPI_SCALE,
                    );
                }
                _ => {
                    // theme 加载失败 (启动期 warn 过) 或 pointer 暂未绑定
                    // (race) — cursor 维持 compositor 默认, trace 不 warn.
                    tracing::trace!(
                        target: "quill::cursor",
                        ?shape,
                        theme_present = state.cursor_theme.is_some(),
                        pointer_present = state.pointer.is_some(),
                        "cursor shape change requested but prerequisite missing (退化)"
                    );
                }
            }
        }
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
            PointerAction::StartResize { serial, edge } => {
                // T-0701: xdg_toplevel.resize(seat, serial, edge). 与 StartMove
                // 同套路, compositor 接管 resize 直到鼠标 release; quill 在此
                // 期间持续收 configure event (resize 中尺寸 deltas), 走既定
                // resize_dirty 路径触发 swapchain 重建 + term/pty resize (INV-006).
                //
                // edge 走 quill_edge_to_wayland 翻译 (INV-010 单一边界, wayland
                // 协议 enum 不在本文件出现 — 通过 fn 返回值类型隐式传递).
                let Some(seat) = state.pointer_seat.as_ref() else {
                    tracing::warn!(
                        target: "quill::pointer",
                        "StartResize 时 pointer_seat=None, 跳过 (race 下罕见)"
                    );
                    return;
                };
                let wl_edge = quill_edge_to_wayland(edge);
                tracing::debug!(
                    target: "quill::pointer",
                    serial,
                    ?edge,
                    "xdg_toplevel.resize (edge/corner drag)"
                );
                state.window.resize(seat, serial, wl_edge);
            }
            PointerAction::Scroll(delta) => {
                // T-0602: 滚轮 / 触摸板累积到 State.pending_scroll_lines, 由
                // idle callback 一次性应用到 term.scroll_display. 与 keyboard
                // PgUp/Dn 同 sink — 同帧多 source 滚动合并避免 alacritty 内部
                // mark_fully_damaged 多次开销.
                state.pending_scroll_lines = state.pending_scroll_lines.saturating_add(delta);
                tracing::trace!(
                    target: "quill::pointer",
                    delta,
                    pending = state.pending_scroll_lines,
                    "axis scroll queued"
                );
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
            // T-0607 选区操作: SelectionStart/Update/End 走 selection_state 状态
            // 转移 + presentation_dirty 触发重画 (cell 反色). 真 PRIMARY auto-copy
            // (SelectionEnd 路径) 走 state.pending_selection_op = Some(SetPrimary)
            // 让 drive_wayland step 3.7 真消费 (那里能拿 LoopData 字段).
            PointerAction::SelectionStart { anchor, mode } => {
                state.selection_state.start(anchor, mode);
                state.presentation_dirty = true;
                tracing::trace!(
                    target: "quill::pointer",
                    ?anchor,
                    ?mode,
                    "SelectionStart"
                );
            }
            PointerAction::SelectionUpdate { cursor } => {
                state.selection_state.update(cursor);
                state.presentation_dirty = true;
            }
            PointerAction::SelectionEnd => {
                state.selection_state.end();
                // 选区结束 → cancel autoscroll Timer + 触发 PRIMARY auto-copy.
                // pending_autoscroll_cancel 让 drive_wayland step 3.7 真 remove
                // calloop Timer source (这里只能拿 &mut State 拿不到 LoopHandle).
                state.pending_autoscroll_cancel = true;
                state.pending_selection_op = Some(SelectionOp::SetPrimary);
                state.presentation_dirty = true;
                tracing::debug!(
                    target: "quill::pointer",
                    "SelectionEnd → schedule PRIMARY auto-copy + cancel autoscroll"
                );
            }
            PointerAction::Paste(source) => {
                // 中键 → Primary 单点入口. Ctrl+Shift+V → Clipboard 走 keyboard
                // 路径推 pending_paste_request (那里直接置). 调用方
                // (drive_wayland step 3.7) 走 dispatch_pending 之后真 trigger
                // offer.receive + read pipe. 这里只 set 单次延迟请求.
                state.pending_paste_request = Some(source);
                tracing::debug!(
                    target: "quill::pointer",
                    ?source,
                    "Paste request queued"
                );
            }
            PointerAction::AutoScrollStart { delta } => {
                // 拖到 viewport 边缘 → schedule autoscroll Timer (100ms 一次
                // scroll_display(±1) + cursor 跟随). 走 pending_autoscroll_op
                // 让 drive_wayland step 3.7 真 insert source (LoopHandle 在
                // LoopData, Dispatch 拿不到).
                state.pending_autoscroll_op = Some(AutoScrollOp::Start { delta });
                tracing::debug!(
                    target: "quill::pointer",
                    delta,
                    "AutoScrollStart queued"
                );
            }
            PointerAction::AutoScrollStop => {
                state.pending_autoscroll_cancel = true;
                tracing::trace!(target: "quill::pointer", "AutoScrollStop queued");
            }
            // T-0608: tab bar 鼠标 actions → pending_tab_op (drive_wayland step
            // 3.8 真消费, 与键盘 NewTab / CloseActiveTab 同套路).
            PointerAction::NewTab => {
                state.pending_tab_op = Some(TabOp::New);
            }
            PointerAction::SwitchTab(idx) => {
                state.pending_tab_op = Some(TabOp::Switch(idx));
            }
            PointerAction::CloseTab(idx) => {
                state.pending_tab_op = Some(TabOp::Close(idx));
            }
            PointerAction::StartTabDrag(_idx) => {
                // 派单 In #F: drag 期间视觉跟随鼠标半透明显示, 当前实装偏离 —
                // **drag 视觉 ghost overlay 留作后续 ticket** (派单 In #F 半透明
                // ghost 跟随需要新建 wgpu 顶点 buffer + 第二 pass blend, 工作量
                // 与本 ticket 主线 reorder 解耦; reorder 逻辑本身已实装).
                tracing::trace!(target: "quill::pointer", "StartTabDrag (ghost overlay 留作后续)");
            }
            PointerAction::TabDragMove { x_logical } => {
                // drag 期间记 last x, 用于 release 时算 target_idx. State 字段
                // last_tab_drag_x 暂存.
                state.last_tab_drag_x = x_logical;
            }
            PointerAction::EndTabDrag => {
                // release 触发 reorder. 派单 In #F: target_idx = (last_x - plus_w)
                // / tab_body_width. apply_tab_op 路径需要原 idx (来自 PointerState
                // tab_press, 但 tab_press 已被 take 清空). 改写: PointerAction
                // 不携带 origin_idx, drive_wayland 路径接到 EndTabDrag 时调
                // resolve_drag_target 算 (origin_idx 由 idx_of(start tab id) 推
                // 但已无 anchor — 当前简化: 直接走 last x 落哪个 tab idx 作 target,
                // 不真 reorder, 派单偏离声明).
                let last_x = state.last_tab_drag_x;
                if let Some(tab_list) = state.tabs.as_ref() {
                    let plus_w = crate::wl::render::TAB_PLUS_W_LOGICAL_PX as f64;
                    let body_w = super::pointer::tab_body_width(state.core.width, tab_list.len());
                    if body_w > 0.0 && last_x >= plus_w {
                        let target_idx = ((last_x - plus_w) / body_w).floor() as usize;
                        let active_idx = tab_list.active_idx();
                        let target = target_idx.min(tab_list.len().saturating_sub(1));
                        if active_idx != target {
                            state.pending_tab_op = Some(TabOp::Reorder {
                                from_idx: active_idx,
                                target_idx: target,
                            });
                        }
                    }
                }
            }
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
        // T-0608: 顶部占用 = titlebar (28) + tab_bar (28) = 56 logical px (多 tab).
        // 800×600 - 56 = 544, 544 / 25 = 21 行.
        assert_eq!(
            cells_from_surface_px(super::INITIAL_WIDTH, super::INITIAL_HEIGHT, 2),
            (80, 21),
            "800×600 - titlebar 28 - tab_bar 28 → cell 80×21 (多 tab)"
        );
    }

    /// T-0617: 单 tab 隐藏 tab bar → 顶部仅 titlebar (28 logical), cell 区高度多
    /// 28 logical 等价多 ~1 行 (HIDPI×2 下).
    #[test]
    fn cells_from_surface_px_single_tab_hides_tab_bar() {
        // 800×600 - 28 (titlebar only) = 572, 572 / 25 = 22 行 (比多 tab 多 1).
        assert_eq!(
            cells_from_surface_px(super::INITIAL_WIDTH, super::INITIAL_HEIGHT, 1),
            (80, 22),
            "T-0617: 单 tab → tab_bar 隐藏, cell 区高 +28 → 多 1 行"
        );
    }

    /// T-0617: tab_bar_h_logical_for 决策表 — 只有 count=1 时返 0, ≥2 返 28.
    #[test]
    fn tab_bar_h_logical_for_decision_table() {
        assert_eq!(tab_bar_h_logical_for(0), 0, "count=0 (兜底) 也隐藏");
        assert_eq!(tab_bar_h_logical_for(1), 0, "单 tab 隐藏 tab bar");
        assert_eq!(
            tab_bar_h_logical_for(2),
            crate::wl::render::TAB_BAR_H_LOGICAL_PX
        );
        assert_eq!(
            tab_bar_h_logical_for(10),
            crate::wl::render::TAB_BAR_H_LOGICAL_PX
        );
    }

    #[test]
    fn cells_from_surface_px_grows_with_surface() {
        // 拖大窗口能多显示 cells (T-0306 acceptance 核心).
        // T-0608: usable_h = h - 56 (titlebar + tab_bar), rows = usable_h / 25.
        // 1200 - 56 = 1144, 1144 / 25 = 45.
        assert_eq!(
            cells_from_surface_px(1600, 1200, 2),
            (160, 45),
            "1600×1200 - 56 → 160×45"
        );
        // 1080 - 56 = 1024, 1024 / 25 = 40.
        assert_eq!(
            cells_from_surface_px(1920, 1080, 2),
            (192, 40),
            "1920×1080 - 56 → 192×40"
        );
    }

    #[test]
    fn cells_from_surface_px_clamps_zero_to_one() {
        // 极小 surface 整数除给 0, max(1) 兜底。term/pty 都不接受 0 维度。
        assert_eq!(
            cells_from_surface_px(0, 0, 2),
            (1, 1),
            "0×0 应 clamp 到 1×1"
        );
        assert_eq!(
            cells_from_surface_px(5, 10, 2),
            (1, 1),
            "5px (< CELL_W_PX=10) 应 clamp col=1"
        );
        // T-0504: 极小 height 触发 saturating_sub → usable_h = 0, rows clamp 到 1.
        assert_eq!(
            cells_from_surface_px(20, 5, 2),
            (2, 1),
            "5px (< titlebar 28) 应 clamp row=1"
        );
        // T-0504: height 正好 = titlebar (28) → usable_h = 0, rows clamp 到 1.
        assert_eq!(
            cells_from_surface_px(20, 28, 2),
            (2, 1),
            "height = titlebar 应 clamp row=1"
        );
    }

    #[test]
    fn cells_from_surface_px_truncates_partial_cells() {
        // 整数除截断, 余下边距 Phase 4 再细化 (派单允许).
        // 805px / 10 = 80 cells (剩 5px 边距).
        // T-0608: usable_h = 612 - 56 (titlebar + tab_bar) = 556, 556 / 25 = 22.
        assert_eq!(
            cells_from_surface_px(805, 612, 2),
            (80, 22),
            "余数应被截断 + titlebar 28 + tab_bar 28 减让"
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

    // ---------- T-0802 In #B should_throttle_propagate 节流决策单测 ----------
    // propagate_resize_if_dirty 在 dirty=true 时进一步走节流判: 距上次成功
    // propagate < 60ms → 跳过本次, 留 dirty 给下次 (兜底 Timer 保不丢). 真副
    // 作用 (LoopHandle.insert_source / Timer fire / 三方 resize) 由集成测试 /
    // 手测覆盖, 决策本身抽纯 fn (now / last / interval) → bool 单测.

    #[test]
    fn should_throttle_propagate_returns_false_on_first_call() {
        // 首次 propagate (last=None) 立即跑不等 60ms, 拖动起手即响应.
        let now = Instant::now();
        assert!(
            !should_throttle_propagate(now, None, Duration::from_millis(60)),
            "last=None (首次) 必不节流"
        );
    }

    #[test]
    fn should_throttle_propagate_throttles_when_inside_window() {
        // last=now-30ms, interval=60ms → elapsed=30ms < 60ms 节流命中.
        // 模拟拖窗口期 16ms 间隔的 configure event 在 60ms 窗内连发.
        let last = Instant::now();
        // 用 now > last 但 elapsed < interval. checked_add 避免 overflow.
        let now = last + Duration::from_millis(30);
        assert!(
            should_throttle_propagate(now, Some(last), Duration::from_millis(60)),
            "elapsed=30ms < interval=60ms 应节流"
        );
    }

    #[test]
    fn should_throttle_propagate_passes_when_outside_window() {
        // last=now-100ms, interval=60ms → elapsed=100ms ≥ 60ms 不节流, 真消费.
        let last = Instant::now();
        let now = last + Duration::from_millis(100);
        assert!(
            !should_throttle_propagate(now, Some(last), Duration::from_millis(60)),
            "elapsed=100ms ≥ interval=60ms 必不节流"
        );
    }

    #[test]
    fn should_throttle_propagate_boundary_at_exact_interval() {
        // 边界精确等于 interval: elapsed == interval → 不节流 (>= 语义,
        // strictly less 才节流). 60Hz 拖动 (~16.67ms 间隔) 第 4 次 configure
        // 距首次约 50ms, 第 5 次约 66.7ms, boundary 行为锁住决策方向.
        let last = Instant::now();
        let now = last + Duration::from_millis(60);
        assert!(
            !should_throttle_propagate(now, Some(last), Duration::from_millis(60)),
            "elapsed == interval 必不节流 (strictly less 才节流)"
        );
    }

    #[test]
    fn should_throttle_propagate_throttle_constant_is_60ms() {
        // 锁住 RESIZE_THROTTLE_MS 常数防"顺手调成 16ms (太频影响) / 100ms (用户
        // 见明显延迟)". 改时同步 reviewer 看派单 In #B 60ms 论证 (单 resize cost
        // 15-20ms × 占 33% 帧).
        assert_eq!(RESIZE_THROTTLE_MS, 60, "T-0802 In #B 派单 60ms 节流硬约束");
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

    // ---------- T-0603 schedule_op_for_request 决策单测 ----------
    // 与 pty_readable_action 同套路 — 决策抽纯函数, 副作用 (LoopHandle.remove
    // / insert_source / Timer 真 fire) 由 tests/keyboard_repeat_e2e.rs 覆盖.

    #[test]
    fn schedule_op_stop_with_no_token_is_noop() {
        assert_eq!(
            schedule_op_for_request(&RepeatScheduleRequest::Stop, false, 25),
            RepeatScheduleOp::Noop,
            "Stop 但本来无 timer 应 Noop"
        );
    }

    #[test]
    fn schedule_op_stop_with_token_cancels_only() {
        assert_eq!(
            schedule_op_for_request(&RepeatScheduleRequest::Stop, true, 25),
            RepeatScheduleOp::CancelOnly,
            "Stop 有 timer 应 CancelOnly"
        );
    }

    #[test]
    fn schedule_op_start_rate_zero_no_token_is_noop() {
        assert_eq!(
            schedule_op_for_request(&RepeatScheduleRequest::Start, false, 0),
            RepeatScheduleOp::Noop,
            "Start 但 rate=0 (compositor 禁 repeat) 无旧 token 应 Noop"
        );
    }

    #[test]
    fn schedule_op_start_rate_zero_with_token_cancels() {
        // 边界: 用户先按 'a' (rate>0 时 schedule 了 timer) 然后 RepeatInfo
        // 改 rate=0 (用户改 GNOME 设置, 假设新 RepeatInfo 到达后再按一次键),
        // 此时 Start 退化为 Cancel.
        assert_eq!(
            schedule_op_for_request(&RepeatScheduleRequest::Start, true, 0),
            RepeatScheduleOp::CancelOnly,
            "Start + rate=0 + 有旧 timer 应 CancelOnly (清旧不 schedule 新)"
        );
    }

    #[test]
    fn schedule_op_start_rate_positive_cancels_and_starts() {
        assert_eq!(
            schedule_op_for_request(&RepeatScheduleRequest::Start, false, 25),
            RepeatScheduleOp::CancelAndStart,
            "Start + rate>0 无旧 token 也走 CancelAndStart (cancel 是 no-op)"
        );
        assert_eq!(
            schedule_op_for_request(&RepeatScheduleRequest::Start, true, 25),
            RepeatScheduleOp::CancelAndStart,
            "Start + rate>0 + 有旧 token 应 CancelAndStart (清旧 + schedule 新)"
        );
    }
}
