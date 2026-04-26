//! Wayland `wl_pointer` 事件 → CSD hit-test → 决策 (T-0504).
//!
//! 职责: 把 compositor 推过来的鼠标 enter / leave / motion / button 事件翻译成
//! quill CSD (client-side decoration) 需要的动作 — titlebar 拖动 / 按钮点击 /
//! hover 高亮 redraw 触发. 不做副作用 (不真调 xdg_toplevel.move / 不重画) —
//! 副作用统一交给 [`crate::wl::window`] 的 `Dispatch<WlPointer>` 调用方按返回的
//! [`PointerAction`] 分派, 与 keyboard 模块 [`crate::wl::keyboard::handle_key_event`]
//! 同套路 (conventions §3 抽决策模式).
//!
//! ## 模块边界 (INV-010 类型隔离)
//!
//! 对外只暴露:
//! - [`PointerState`] — quill 自有 struct, 字段全私有 (HoverRegion / 累积坐标),
//!   下游构造不出来.
//! - [`HoverRegion`] / [`PointerAction`] / [`WindowButton`] — quill 自定义 enum,
//!   不实现 `From<wl_pointer::Event>` (避免下游 `event.into()` 偷渡 wayland
//!   类型路径, 与 keyboard 模块同决策).
//! - [`handle_pointer_event`] — 接 `wl_pointer::Event` (raw wayland-client 协议
//!   类型, 与 `wl/window.rs::Dispatch<WlPointer>` 边界一致), 返
//!   [`PointerAction`] (quill 自有). 没有 `pub use wl_pointer::*` re-export.
//! - [`hit_test`] — 纯逻辑 fn, 接 `(x, y, surface_w, surface_h)`, 返
//!   [`HoverRegion`]. 单测覆盖 (派单 In #F).
//!
//! ## 协议状态机概览
//!
//! ```text
//! wl_seat capabilities → 含 Pointer
//!   └→ get_pointer(qh, ()) → WlPointer
//!         │
//!         ├→ Event::Enter(serial, surface, x, y)   → 记 pos + serial, hover
//!         ├→ Event::Leave(serial, surface)         → 清 pos, HoverRegion=None
//!         ├→ Event::Motion(time, x, y)             → 更新 pos + hit_test → 可能 HoverChange
//!         ├→ Event::Button(serial, time, btn, st)  → press 对 hover 区分派动作
//!         ├→ Event::Axis(...)                      → 滚轮, 本 ticket 未消费 (Out)
//!         └→ Event::Frame                          → 一组事件结束, 不消费
//! ```
//!
//! ## CSD 视觉布局
//!
//! 顶部 28 logical px (= 56 physical, HIDPI×2) titlebar, 三按钮位于右上角:
//!
//! ```text
//! ┌──────────────────────────────────────────┬──┬──┬──┐
//! │  titlebar (28×width, drag area)          │Mn│Mx│Cl│   ← 28 logical px
//! ├──────────────────────────────────────────┴──┴──┴──┤
//! │                                                   │
//! │  text area (cells, terminal grid)                 │
//! │                                                   │
//! └───────────────────────────────────────────────────┘
//! ```
//!
//! 三按钮各 24×24 logical px, 紧贴右上角, 顺序 (右→左) Close / Maximize /
//! Minimize. 硬编码与 [`crate::wl::render`] 的 titlebar 渲染同源 (常数
//! [`TITLEBAR_H_LOGICAL_PX`] / [`BUTTON_W_LOGICAL_PX`] / [`BUTTON_H_LOGICAL_PX`]
//! 在 `render.rs` 顶部定义, hit_test 直接 import 用 — 单一来源, 改一处即视觉
//! 与逻辑同步).

use wayland_client::protocol::wl_pointer;
use wayland_client::WEnum;

use super::render::{BUTTON_H_LOGICAL_PX, BUTTON_W_LOGICAL_PX, TITLEBAR_H_LOGICAL_PX};

/// CSD 三按钮枚举. 对应 xdg_toplevel 协议 set_minimized / set_maximized
/// (toggle) / 关闭 (内部置 exit + loop_signal.stop).
///
/// **why enum 而非 bool / int**: 派单 In #B 三按钮决策, exhaustive match 在
/// 调用方 (`Dispatch<WlPointer>` 闭包) 编译期 catch 加新按钮 (Phase 6+ 可能加
/// "全屏" 按钮). 与 [`KeyboardAction`] 等 quill 决策枚举同套路.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowButton {
    /// 最小化按钮. 调用 `Window::set_minimized()` (sctk 0.19 已封装).
    Minimize,
    /// 最大化按钮. **toggle 语义**: 当前 maximized 状态由调用方 (State
    /// 字段) 跟踪, 本枚举仅指示按钮被点; 真 set_maximized / unset_maximized
    /// 在 Dispatch 闭包按 toggle 状态分派.
    Maximize,
    /// 关闭按钮. 走 `WindowEvent::Close` 路径 (与 compositor 发 close request
    /// 同出口, INV statemachine 不变).
    Close,
}

/// T-0701: quill 自有的 8 边角枚举, 与 wayland xdg_toplevel `resize_edge`
/// 协议 enum 一一对应 (Top / Bottom / Left / Right / TopLeft / TopRight /
/// BottomLeft / BottomRight). **本枚举不含 None** — `None` 在 wayland 协议里
/// 表示"compositor 决定边", 客户端发起 resize 必带具体边, 不允许 None
/// (xdg-shell.xml resize_edge enum entry "none"=0 仅作 default value, 客户端
/// 不该发).
///
/// **INV-010 类型隔离**: 此枚举是**单一边界点** —
/// `wayland_protocols::xdg::shell::client::xdg_toplevel::ResizeEdge` 仅在
/// `pointer.rs` 的 [`quill_edge_to_wayland`] 翻译 fn 内出现 (与
/// `from_alacritty` 同套路, 见 INV-010 + conventions §5). 调用方 (window.rs
/// `Dispatch<WlPointer>`) 通过翻译 fn 间接拿 wayland enum, 不直接 import
/// wayland::ResizeEdge 字面 path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResizeEdge {
    Top,
    Bottom,
    Left,
    Right,
    TopLeft,
    TopRight,
    BottomLeft,
    BottomRight,
}

/// quill ResizeEdge → wayland 协议 ResizeEdge 的**单一翻译边界**.
///
/// **why pub(crate) 而非 pub**: 翻译表只给 `wl/window.rs::Dispatch<WlPointer>`
/// 用, 跨 crate 边界不暴露 (INV-010). 与 `WindowCore` 字段 pub(crate) 同
/// 模块隔离套路.
///
/// **why exhaustive match 无 `_ =>`**: 上游 wayland-protocols 加新 variant
/// (例如 wayland 主版本若加 corner 细分) 编译期 catch — INV-010 验证条目硬
/// 要求.
pub(crate) fn quill_edge_to_wayland(
    edge: ResizeEdge,
) -> wayland_protocols::xdg::shell::client::xdg_toplevel::ResizeEdge {
    use wayland_protocols::xdg::shell::client::xdg_toplevel::ResizeEdge as WlEdge;
    match edge {
        ResizeEdge::Top => WlEdge::Top,
        ResizeEdge::Bottom => WlEdge::Bottom,
        ResizeEdge::Left => WlEdge::Left,
        ResizeEdge::Right => WlEdge::Right,
        ResizeEdge::TopLeft => WlEdge::TopLeft,
        ResizeEdge::TopRight => WlEdge::TopRight,
        ResizeEdge::BottomLeft => WlEdge::BottomLeft,
        ResizeEdge::BottomRight => WlEdge::BottomRight,
    }
}

/// 鼠标当前在 surface 内的 hover 区域. 由 [`hit_test`] 算出, [`PointerState`]
/// 记一份方便 redraw 决策 (按钮 hover 变深 / Close hover 变红 — 派单 In #D).
///
/// **why 不复用 [`WindowButton`] 加 None / TitleBar / TextArea**: 语义不同 —
/// `WindowButton` 是"用户点了哪个", `HoverRegion` 是"鼠标在哪". 解耦让按钮
/// 列表加新成员 (例如 Phase 6 全屏按钮) 时 hit_test 与 button click 各自演化.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum HoverRegion {
    /// 鼠标不在 surface 内 (Leave 后) 或在 cell 区域之外的边距.
    #[default]
    None,
    /// 鼠标在 titlebar 区域内 (顶部 28 logical px), 不在三按钮上.
    /// press 后走 xdg_toplevel.move 拖窗口.
    TitleBar,
    /// 鼠标在三按钮区. 调用方据此渲染高亮 + click 时分派动作.
    Button(WindowButton),
    /// T-0701: 鼠标在 4 边 / 4 角 resize 区. press 后走 xdg_toplevel.resize
    /// 让 compositor 接管 resize. 边缘宽度 [`RESIZE_EDGE_PX`] (4 logical),
    /// 角宽度 [`RESIZE_CORNER_PX`] (8 logical, 优先 corner 让用户好抓).
    ResizeEdge(ResizeEdge),
    /// 鼠标在 text area (cell grid 区域). Phase 6+ 接选区 / 滚轮.
    TextArea,
}

/// `handle_pointer_event` 的副作用描述. 调用方按 variant 分派.
///
/// 抽 enum 而非散落 `if` 是 conventions §3 套路 (类比 [`PtyAction`] /
/// [`KeyboardAction`] / [`WindowAction`]); 也是 INV-010 类型隔离的实操 —
/// 调用方拿到的全是 quill 自有类型, 不暴露 wl_pointer::Event 字段.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum PointerAction {
    /// 没事可做 (motion 不跨区 / Frame / 未识别按键 / 触摸板未达整 line 阈值).
    #[default]
    Nothing,
    /// titlebar 区域内 press: 触发 `xdg_toplevel.move(seat, serial)`,
    /// compositor 接管拖动. serial 是 button event 的 serial (Wayland 协议
    /// 要求 move 必须传最近 input event 的 serial, compositor 验证防伪造).
    StartMove { serial: u32 },
    /// T-0701: 4 边 / 4 角 resize 区 press: 触发
    /// `xdg_toplevel.resize(seat, serial, edge)`, compositor 接管 resize 直到
    /// 鼠标 release. 与 StartMove 同套路 (xdg-shell 协议要求 resize 也带最近
    /// input event serial). `edge` 是 quill 自有 [`ResizeEdge`], 调用方需走
    /// [`quill_edge_to_wayland`] 翻译给 SCTK `Window::resize` (INV-010 单一
    /// 翻译边界).
    StartResize { serial: u32, edge: ResizeEdge },
    /// 三按钮 click. 调用方按 button 分派 set_minimized / 切换 maximize /
    /// 关闭. **press → click**: 本 ticket 简化按 press 触发 click (与多数终端
    /// CSD 一致, alacritty / foot 同), 不做"press 同区 + release 同区" 的
    /// drag-cancel 检测 (Phase 6+ 可加).
    ButtonClick(WindowButton),
    /// hover 区域变化 — 调用方走 redraw 路径 (按钮 hover 变深, Close 变红).
    /// 包含**新**区域 (旧区域已存于 [`PointerState`] 内不外漏).
    HoverChange(HoverRegion),
    /// T-0602: 滚轮 / 触摸板纵向滚动 → scrollback 偏移. **正值 = 向上滚 (看
    /// 更老历史), 负值 = 向下滚 (回最新)** — 与 alacritty `Scroll::Delta(i32)`
    /// 同方向语义, 调用方直接传给 `TermState::scroll_display`.
    ///
    /// 单位是 **整 line**. 离散滚轮 (传统鼠标, wl_pointer::Event::Axis 的 value
    /// 已是 line × 10 fixed-point) 一格 = 1 line; 触摸板连续 axis 走累积 /
    /// 阈值 (见 [`PointerState`] 的 `scroll_accum`), 累够 1 cell 高 (24 logical
    /// px) 才发一次 Scroll(±1).
    Scroll(i32),
}

/// quill 自有的指针状态封装. 内部跟踪当前 surface 坐标 + hover 区域 + 最近
/// button event 的 serial (用于 xdg_toplevel.move / set_cursor 协议).
///
/// **字段全私有** (INV-010): 下游不能直接读 wl_fixed 坐标 / WEnum<Button>
/// 出去, 全部走 [`handle_pointer_event`] 单点入口.
///
/// 当前 surface 尺寸 (`surface_w_logical` / `surface_h_logical`) 由调用方在
/// resize 时调 [`PointerState::set_surface_size`] 同步 — 必须 **logical px**,
/// 与 [`WindowCore::width`] / [`WindowCore::height`] 同源 (configure event 给的
/// 是 logical, surface configure 内部乘 HIDPI_SCALE 算 physical).
pub struct PointerState {
    /// 当前 surface 内坐标 (logical px). Enter / Motion 更新, Leave 清.
    /// `None` = 鼠标不在 surface (Leave 后或 Enter 前的初始态).
    pos: Option<(f64, f64)>,
    /// 当前 hover 区域. Motion 事件触发更新, 跨区时返 [`PointerAction::HoverChange`].
    hover: HoverRegion,
    /// 最近一次 button event 的 serial. xdg_toplevel.move / show_window_menu /
    /// resize 协议要求传最近 input event serial, compositor 据此验证防伪造
    /// move (例: 你不能在没收到 button event 时调 move, compositor 拒绝).
    ///
    /// `#[allow(dead_code)]`: T-0504 当前 PointerAction::StartMove 直接携带
    /// 触发 press 的 serial (而非读此字段), 此字段保留作 Phase 6+ show_window_menu /
    /// resize 路径预备 — 那些操作发生在 release 后, 调用时需读最近 serial.
    /// 移除会破坏前向兼容, 留作 forward-compat hook (与 `KeyboardState::repeat_info`
    /// 同 forward-compat 决策).
    #[allow(dead_code)]
    last_button_serial: u32,
    /// 当前 surface 尺寸 (logical). 由调用方 resize 时同步, hit_test 用此算
    /// 按钮位置 (按钮在右上角, x = w - n×BUTTON_W; y < TITLEBAR_H).
    surface_w_logical: u32,
    surface_h_logical: u32,
    /// T-0602: 触摸板连续 axis 累积值 (logical px). wl_pointer Axis 对触摸板
    /// 走 sub-line 连续 fixed-point 滚动 (一次 motion 可能 0.5 line), 累够
    /// [`SCROLL_ACCUM_LINE_PX`] (24 logical px ≈ 1 cell 行高) 才发一次
    /// Scroll(±1), 余量保留下次累加. 离散滚轮 (传统鼠标 wheel notch) 走
    /// `Event::Axis` 一格 value = 10.0 (1 line × 10 wl_fixed sub-units),
    /// 单格也走累积路径 (10 < 24 不会跨阈值, 实际触发由 `AxisDiscrete` 帧
    /// 的整 line 跳); 但本 ticket 简化 — 直接把 wl_pointer Axis value 当 px
    /// 累积, discrete vs continuous 一视同仁, **每 24 累积量发 1 line scroll**.
    /// 实测 5090 + Logitech MX 滚轮一格 ≈ 15 px (compositor 翻译), 两格出 1 line;
    /// touchpad 两指滑一指距离约 100-200 px 出 4-8 line. 体感与 foot/kitty 接近.
    scroll_accum_y: f64,
}

/// T-0701: 边缘 resize hit-test 厚度 (logical px). 4 logical = 8 physical (HIDPI×2),
/// 与 GNOME mutter / KDE kwin 默认 4-6 px 一致. 鼠标在 surface 边缘此带内
/// 走 ResizeEdge.
///
/// **why 4 而非 8**: 边缘太宽吃掉 cell 边距 — 用户拖文字选区时容易误触发
/// resize. GNOME / KDE / foot / alacritty 实测均 4-6 logical, 取 4 与
/// compositor server-side 装饰最小窄边一致 (T-0503 GNOME 政策性 SSD off).
pub(crate) const RESIZE_EDGE_PX: f64 = 4.0;

/// T-0701: 角 resize hit-test 边长 (logical px). 8 logical 矩形覆盖左上 / 右上
/// / 左下 / 右下四角, 优先 corner > edge 让用户好抓 (角对角拖最常用,
/// alacritty / foot / GNOME 默认角带均 ≥ 边带).
///
/// **why 8 而非更大**: 与 titlebar 高度 28 / 按钮宽 24 同尺度便心算; 太大
/// (>16) 在按钮区会与 Close 按钮抢 hit-test (Close 区右上 24×24, 与右上
/// corner 8×8 区接壤但不重叠 — 派单 In #A "角覆盖优先 edge", 但不抢按钮).
pub(crate) const RESIZE_CORNER_PX: f64 = 8.0;

/// T-0602: 触摸板 / 滚轮累积阈值 (logical px). 累够此值发一次 line 滚动.
///
/// 取 24 logical px 与 [`crate::wl::render::CELL_H_PX`] 25 接近 (整数 24 便于
/// 心算 / 测试断言), 视觉上"鼠标滚一行 = 屏幕滚一行" 对齐. 派单"滚轮 axis
/// discrete 用 line 单位 / 触摸板连续 axis 阈值转 line" 的硬约束实操 —
/// 单一阈值同时覆盖 wheel notch (10 px/格) 与 touchpad (px 累积), 避免分支
/// 走两套阈值表 (KISS).
pub(crate) const SCROLL_ACCUM_LINE_PX: f64 = 24.0;

impl PointerState {
    /// 启动期建空 PointerState. 初始尺寸由调用方紧接 [`Self::set_surface_size`]
    /// 同步 (与 `WindowCore::new` 拿 INITIAL_WIDTH/HEIGHT 同).
    pub fn new(initial_w_logical: u32, initial_h_logical: u32) -> Self {
        Self {
            pos: None,
            hover: HoverRegion::None,
            last_button_serial: 0,
            surface_w_logical: initial_w_logical,
            surface_h_logical: initial_h_logical,
            scroll_accum_y: 0.0,
        }
    }

    /// 同步 surface 尺寸 (logical px). [`crate::wl::window::propagate_resize_if_dirty`]
    /// 在 resize chain 末尾调一次, 让 hit_test 用最新尺寸算按钮位置.
    pub fn set_surface_size(&mut self, w_logical: u32, h_logical: u32) {
        self.surface_w_logical = w_logical;
        self.surface_h_logical = h_logical;
        // 尺寸变化后重新算一次 hover (按钮挪了位置, 鼠标可能落到不同区).
        // pos 不变, hit_test 用新 surface 尺寸即可.
        if let Some((x, y)) = self.pos {
            self.hover = hit_test(x, y, self.surface_w_logical, self.surface_h_logical);
        }
    }

    /// 当前 hover 区域. CSD 渲染 ([`crate::wl::render::Renderer::draw_frame`])
    /// 据此画按钮高亮.
    pub fn hover(&self) -> HoverRegion {
        self.hover
    }
}

/// 接 wl_pointer 协议事件 → 算 [`PointerAction`].
///
/// **纯逻辑** (无 IO, 不调 wl request, 不真画): 调用方
/// (`wl/window.rs::Dispatch<WlPointer>`) 据返回 action 决定调
/// xdg_toplevel.move / set_minimized 等. 与 [`crate::wl::keyboard::handle_key_event`]
/// 同套路, 决策与副作用分离 (conventions §3).
///
/// 协议事件分派表:
/// - **Enter(serial, x, y)**: 记 pos + hit_test, 返 HoverChange (从 None →
///   新区).
/// - **Leave(serial)**: 清 pos, hover → None, 返 HoverChange(None) 让调用方
///   redraw 清按钮高亮.
/// - **Motion(time, x, y)**: 更新 pos + hit_test; 区域变化才返
///   HoverChange, 同区返 Nothing (避免 redraw 风暴).
/// - **Button(serial, time, btn, state=Pressed)**: 记 serial, 按 hover 分派:
///   - TitleBar → StartMove { serial }
///   - Button(b) → ButtonClick(b)
///   - 其它 → Nothing
/// - **Button(state=Released)**: 不消费 (本 ticket 简化, press = click).
/// - **Axis / AxisStop / AxisDiscrete / AxisSource / AxisRelativeDirection /
///   AxisValue120 / Frame**: Nothing (Out, Phase 6+ 滚轮选区).
///
/// 已知陷阱:
/// - wl_pointer 坐标是 wl_fixed (24.8 fixed-point, wayland-client 已转 f64).
///   单位是 logical px (与 surface 尺寸 logical 一致).
/// - Button event 的 button code 是 evdev (BTN_LEFT=0x110), 与 wl_keyboard
///   evdev keycode 同源.
/// - Enter 事件**也带 serial**, 但通常不用作 move 的 serial — 只有 Button
///   event 的 serial 被 compositor 接受作 grab serial.
///
/// **why 拆 [`apply_enter`] / [`apply_leave`] / [`apply_motion`] /
/// [`apply_button`] 子 fn**: wl_pointer::Event 含 WlSurface 字段 (Enter /
/// Leave), 单测构造 WlSurface 需真 Connection (无可行 mock 路径). 拆纯标量
/// 入参的子 fn 让单测覆盖决策矩阵 (conventions §3 SOP), 本 fn 仅负责协议
/// 字段拆解 + 子 fn 转发, 自身行为薄, 单测对子 fn 即等价覆盖.
pub fn handle_pointer_event(event: wl_pointer::Event, state: &mut PointerState) -> PointerAction {
    match event {
        wl_pointer::Event::Enter {
            surface_x,
            surface_y,
            ..
        } => apply_enter(state, surface_x, surface_y),
        wl_pointer::Event::Leave { .. } => apply_leave(state),
        wl_pointer::Event::Motion {
            surface_x,
            surface_y,
            ..
        } => apply_motion(state, surface_x, surface_y),
        wl_pointer::Event::Button {
            serial,
            button,
            state: btn_state,
            ..
        } => {
            let pressed = matches!(btn_state, WEnum::Value(wl_pointer::ButtonState::Pressed));
            apply_button(state, serial, button, pressed)
        }
        // T-0602: 纵向滚轮 / 触摸板 axis. 横向 (axis=1 horizontal) 不消费 — quill
        // 终端无横向滚 (alacritty 同). value 是 wl_fixed-point logical px (滚轮一
        // 格约 10-15 px, 触摸板连续小步).
        wl_pointer::Event::Axis { axis, value, .. } => {
            if matches!(axis, WEnum::Value(wl_pointer::Axis::VerticalScroll)) {
                apply_axis_vertical(state, value)
            } else {
                PointerAction::Nothing
            }
        }
        // AxisStop / AxisDiscrete / AxisSource / AxisRelativeDirection /
        // AxisValue120 / Frame: 派单 Out (本 ticket 用累积 px 阈值已覆盖
        // wheel + touchpad 两种, 不依赖 discrete 帧). wl_pointer Event 在
        // wayland-client 0.31 无 #[non_exhaustive], 但防上游升级加 variant —
        // 默认沉默 (与 keyboard 模块同决策).
        _ => PointerAction::Nothing,
    }
}

/// Enter 决策子 fn — 单测入口 (避免构造 WlSurface). 见 [`handle_pointer_event`]
/// 文档头 "why 拆子 fn".
pub(crate) fn apply_enter(state: &mut PointerState, x: f64, y: f64) -> PointerAction {
    state.pos = Some((x, y));
    let new_hover = hit_test(x, y, state.surface_w_logical, state.surface_h_logical);
    if new_hover != state.hover {
        state.hover = new_hover;
        return PointerAction::HoverChange(new_hover);
    }
    PointerAction::Nothing
}

/// Leave 决策子 fn. 清 pos + hover, 返 HoverChange(None) 让调用方 redraw 清按钮高亮.
pub(crate) fn apply_leave(state: &mut PointerState) -> PointerAction {
    state.pos = None;
    if state.hover != HoverRegion::None {
        state.hover = HoverRegion::None;
        return PointerAction::HoverChange(HoverRegion::None);
    }
    PointerAction::Nothing
}

/// Motion 决策子 fn. 区域变化才返 HoverChange, 同区返 Nothing 避免 redraw 风暴.
pub(crate) fn apply_motion(state: &mut PointerState, x: f64, y: f64) -> PointerAction {
    state.pos = Some((x, y));
    let new_hover = hit_test(x, y, state.surface_w_logical, state.surface_h_logical);
    if new_hover != state.hover {
        state.hover = new_hover;
        return PointerAction::HoverChange(new_hover);
    }
    PointerAction::Nothing
}

/// Button 决策子 fn. press + 左键 + hover 区分派 StartMove / ButtonClick;
/// 否则 Nothing. release 不消费 (派单简化 press = click, Phase 6+ 可加 drag-cancel).
///
/// `BTN_LEFT = 0x110` (linux/input-event-codes.h). 中 / 右键 (BTN_MIDDLE 0x112 /
/// BTN_RIGHT 0x111) 暂忽略 — Phase 6+ 上下文菜单 / 粘贴 时再扩.
pub(crate) fn apply_button(
    state: &mut PointerState,
    serial: u32,
    button: u32,
    pressed: bool,
) -> PointerAction {
    state.last_button_serial = serial;
    if !pressed {
        return PointerAction::Nothing;
    }
    const BTN_LEFT: u32 = 0x110;
    if button != BTN_LEFT {
        return PointerAction::Nothing;
    }
    match state.hover {
        HoverRegion::TitleBar => PointerAction::StartMove { serial },
        HoverRegion::Button(b) => PointerAction::ButtonClick(b),
        HoverRegion::ResizeEdge(edge) => PointerAction::StartResize { serial, edge },
        HoverRegion::TextArea | HoverRegion::None => PointerAction::Nothing,
    }
}

/// T-0602: 纵向 axis (滚轮 / 触摸板) → Scroll(±N) 决策子 fn.
///
/// **方向语义**: wl_pointer Axis vertical `value` **正 = 向下滚** (compositor /
/// libinput 协议规约: 用户手势"向下" → 内容应往下走 = 看更新内容). quill
/// scrollback 反向 — `Scroll(+N) = 向上滚 (看更老历史)`, 与 alacritty
/// `Scroll::Delta(+N)` 一致 (Delta(+) 增 display_offset). 故**取负**:
/// value > 0 (用户手势向下 = 看新) → Scroll(负) = 减 display_offset 跳到底.
///
/// **累积阈值** ([`SCROLL_ACCUM_LINE_PX`] = 24 logical px ≈ 1 cell 行高):
/// wl_pointer Axis 一次 event value 可能很小 (触摸板 1.5 px/帧), 直接发
/// Scroll(0) 无意义; 累够 24 px 才发整 line. 余量 (`accum % SCROLL_ACCUM_LINE_PX`)
/// 留给下次累加. 累积带符号 (向上累 + / 向下累 -), 跨 0 时余量自然清理.
///
/// **离散滚轮兼容**: wl_pointer Axis 一格 wheel notch value ≈ 10-15 px (依 compositor
/// 翻译, sway/wlroots 默认 10). 两格累 20-30 跨 24 阈值, 出 1 line scroll;
/// 三格出 1-2 line. 体感与 foot/kitty/alacritty 接近 (派单"daily-drive 必需"
/// acceptance 实测: cat 长文件后滚轮三格能看历史).
pub(crate) fn apply_axis_vertical(state: &mut PointerState, value: f64) -> PointerAction {
    if !value.is_finite() {
        // compositor 不该发 NaN/Inf, 防御 — 累积器不染污.
        return PointerAction::Nothing;
    }
    state.scroll_accum_y += value;
    // 累够整 line, 触发 Scroll. trunc 避免 f64 → i32 round half-away-from-zero
    // (用户感知方向稳定: 累 23.9 不出 line, 累 24.0 出 1, 与离散滚轮一格一动一致).
    let lines = (state.scroll_accum_y / SCROLL_ACCUM_LINE_PX).trunc() as i32;
    if lines == 0 {
        return PointerAction::Nothing;
    }
    state.scroll_accum_y -= (lines as f64) * SCROLL_ACCUM_LINE_PX;
    // 取负: wl 协议 +Y = 用户手势向下 = 看新; quill `Scroll(+)` = 看老历史.
    PointerAction::Scroll(-lines)
}

/// 纯逻辑 hit-test: 给定 surface 内坐标 (logical px) 与 surface 尺寸 (logical),
/// 算 [`HoverRegion`]. 派单 In #F 抽决策模式硬约束, 单测覆盖 ≥6 case
/// (titlebar / 3 按钮 / text area / 边界 / T-0701 4 边 + 4 角 resize).
///
/// **CSD 视觉布局** (单一来源, 与 [`crate::wl::render`] titlebar 渲染同源):
/// - 顶部 [`TITLEBAR_H_LOGICAL_PX`] (28 logical) 是 titlebar.
/// - 三按钮位于 titlebar 右端, 各 [`BUTTON_W_LOGICAL_PX`] × [`BUTTON_H_LOGICAL_PX`]
///   (24×24 logical), 顺序 (右→左) Close / Maximize / Minimize.
/// - titlebar 之下是 text area (cell grid).
/// - 超出 surface (x < 0 / y < 0 / x ≥ w / y ≥ h) → None.
///
/// **T-0701 hit-test 优先级** (高 → 低):
/// 1. 边界外 → None
/// 2. **4 角** ([`RESIZE_CORNER_PX`] = 8 logical 方块, 4 个) → ResizeEdge(corner) —
///    优先于 edge / button (派单"角覆盖优先 edge"); 角与右上 Close 按钮区重叠
///    (右上 8×8 在 Close 24×24 内), 用户在最角即拖窗口, 内移 ≥8 logical 即落
///    Close — 实测体感与 GNOME / foot 一致.
/// 3. **4 边** ([`RESIZE_EDGE_PX`] = 4 logical 厚 strip) → ResizeEdge(edge) —
///    避开 4 角 (角已 step 2 接管). titlebar 顶边 (y < 4) 走 Top 而非 titlebar
///    drag — 避免拖窗口时误抓窗口顶边过细 (GNOME 同决策).
/// 4. titlebar 段 (y < TITLEBAR_H, 排除上方边):
///    - 三按钮 hit (Close / Maximize / Minimize) → Button(...)
///    - 否则 → TitleBar
/// 5. text area 段 → TextArea
///
/// **why 接 f64 而非 i32**: wl_pointer 坐标是 wl_fixed, wayland-client 转 f64.
/// 边界判断用 < / >= 而非 ≤, 与 NDC / 像素坐标系一致 (像素中心在整数偏 0.5,
/// 但 hit_test 不需精度到 sub-pixel, 直接用浮点比较即可).
///
/// **单一来源**: 五常数 (TITLEBAR_H / BUTTON_W / BUTTON_H / RESIZE_EDGE_PX /
/// RESIZE_CORNER_PX) 都从模块顶部 / `render.rs` import. 改一处即视觉与逻辑
/// 同步, 无漂移风险.
pub fn hit_test(x: f64, y: f64, surface_w: u32, surface_h: u32) -> HoverRegion {
    // 边界外
    if x < 0.0 || y < 0.0 {
        return HoverRegion::None;
    }
    let w = surface_w as f64;
    let h = surface_h as f64;
    if x >= w || y >= h {
        return HoverRegion::None;
    }

    // T-0701 优先级 step 2: 4 角 (角优先 edge, 派单 In #A).
    // surface 太小 (w < 2 * RESIZE_CORNER_PX 或 h < 同) 时角会重叠 — 此处不
    // 防御 (INITIAL_WIDTH/HEIGHT 800/600 远超, 用户拉到 ≤16 自找死, 与 alacritty
    // 同决策不防 minimum-size).
    let in_left = x < RESIZE_CORNER_PX;
    let in_right = x >= w - RESIZE_CORNER_PX;
    let in_top = y < RESIZE_CORNER_PX;
    let in_bottom = y >= h - RESIZE_CORNER_PX;
    if in_top && in_left {
        return HoverRegion::ResizeEdge(ResizeEdge::TopLeft);
    }
    if in_top && in_right {
        return HoverRegion::ResizeEdge(ResizeEdge::TopRight);
    }
    if in_bottom && in_left {
        return HoverRegion::ResizeEdge(ResizeEdge::BottomLeft);
    }
    if in_bottom && in_right {
        return HoverRegion::ResizeEdge(ResizeEdge::BottomRight);
    }

    // T-0701 优先级 step 3: 4 边 (4 logical 厚 strip).
    // 顶边 4 logical 走 Top resize 优先于 titlebar drag — 见上方 priority 注释.
    if y < RESIZE_EDGE_PX {
        return HoverRegion::ResizeEdge(ResizeEdge::Top);
    }
    if y >= h - RESIZE_EDGE_PX {
        return HoverRegion::ResizeEdge(ResizeEdge::Bottom);
    }
    if x < RESIZE_EDGE_PX {
        return HoverRegion::ResizeEdge(ResizeEdge::Left);
    }
    if x >= w - RESIZE_EDGE_PX {
        return HoverRegion::ResizeEdge(ResizeEdge::Right);
    }

    let titlebar_h = TITLEBAR_H_LOGICAL_PX as f64;
    let btn_w = BUTTON_W_LOGICAL_PX as f64;
    let btn_h = BUTTON_H_LOGICAL_PX as f64;

    // 不在 titlebar 段 → 必在 text area 段
    if y >= titlebar_h {
        return HoverRegion::TextArea;
    }

    // titlebar 段, 检查三按钮 (右→左 Close / Maximize / Minimize)
    // 按钮高度 ≤ titlebar (24 ≤ 28), 顶部居中: 上 2px 边距.
    // 简化: 按钮 y 范围 [0, BUTTON_H_LOGICAL_PX) — 整 titlebar 都按按钮上下
    // 边占满, 视觉上按钮顶贴 titlebar 顶, 4 px 底边距 (28 - 24 = 4).
    if y < btn_h {
        let close_x_min = w - btn_w;
        let close_x_max = w;
        let max_x_min = w - 2.0 * btn_w;
        let max_x_max = close_x_min;
        let min_x_min = w - 3.0 * btn_w;
        let min_x_max = max_x_min;

        if x >= close_x_min && x < close_x_max {
            return HoverRegion::Button(WindowButton::Close);
        }
        if x >= max_x_min && x < max_x_max {
            return HoverRegion::Button(WindowButton::Maximize);
        }
        // min_x_min 可能 < 0 (极小 surface 装不下三按钮); 落到 TitleBar 兜底
        if min_x_min >= 0.0 && x >= min_x_min && x < min_x_max {
            return HoverRegion::Button(WindowButton::Minimize);
        }
    }

    // 不在按钮 → titlebar 拖动区
    HoverRegion::TitleBar
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 派单 In #F 单测覆盖: titlebar / 3 按钮 / text area / 边界 ≥6 case.

    #[test]
    fn hit_test_titlebar_left_returns_titlebar() {
        // 800×600 surface, 左半 titlebar, x=100 y=10 显然在 titlebar drag area.
        assert_eq!(hit_test(100.0, 10.0, 800, 600), HoverRegion::TitleBar);
    }

    #[test]
    fn hit_test_close_button_top_right() {
        // 800×600: Close 区 x ∈ [776, 800), y ∈ [0, 24)
        assert_eq!(
            hit_test(790.0, 12.0, 800, 600),
            HoverRegion::Button(WindowButton::Close)
        );
    }

    #[test]
    fn hit_test_maximize_button() {
        // Maximize 区 x ∈ [752, 776), y ∈ [0, 24)
        assert_eq!(
            hit_test(760.0, 12.0, 800, 600),
            HoverRegion::Button(WindowButton::Maximize)
        );
    }

    #[test]
    fn hit_test_minimize_button() {
        // Minimize 区 x ∈ [728, 752), y ∈ [0, 24)
        assert_eq!(
            hit_test(740.0, 12.0, 800, 600),
            HoverRegion::Button(WindowButton::Minimize)
        );
    }

    #[test]
    fn hit_test_text_area_below_titlebar() {
        // titlebar 28 px 高, y=100 必在 text area
        assert_eq!(hit_test(400.0, 100.0, 800, 600), HoverRegion::TextArea);
    }

    #[test]
    fn hit_test_text_area_at_titlebar_boundary() {
        // y=28 (= TITLEBAR_H_LOGICAL_PX) 是 text area 起点 (>= titlebar_h)
        assert_eq!(hit_test(400.0, 28.0, 800, 600), HoverRegion::TextArea);
    }

    #[test]
    fn hit_test_titlebar_just_above_boundary() {
        // y=27.9 仍在 titlebar
        assert_eq!(hit_test(400.0, 27.9, 800, 600), HoverRegion::TitleBar);
    }

    #[test]
    fn hit_test_outside_surface_negative() {
        // 负坐标 (compositor 不该发, 防御)
        assert_eq!(hit_test(-1.0, 10.0, 800, 600), HoverRegion::None);
        assert_eq!(hit_test(10.0, -1.0, 800, 600), HoverRegion::None);
    }

    #[test]
    fn hit_test_outside_surface_overflow() {
        // x >= w 或 y >= h
        assert_eq!(hit_test(800.0, 10.0, 800, 600), HoverRegion::None);
        assert_eq!(hit_test(10.0, 600.0, 800, 600), HoverRegion::None);
    }

    #[test]
    fn hit_test_close_button_inner_returns_close() {
        // **T-0701 优先级修正**: 原测试名 hit_test_close_right_edge_inclusive 测
        // x=799 y=0 → Close, 但右边 4 px 现走 Right resize edge / 顶 4 px 走 Top
        // edge / 右上 8×8 走 TopRight corner — 边角全优先按钮 (派单"角覆盖优先
        // edge, edge 优先 button"). 改测 Close 内**避开 4 边 / 8 角**的内点
        // (x=790 出右边 4 edge & 出 8 corner; y=12 出顶边).
        assert_eq!(
            hit_test(790.0, 12.0, 800, 600),
            HoverRegion::Button(WindowButton::Close)
        );
    }

    #[test]
    fn hit_test_button_y_below_button_height_falls_to_titlebar() {
        // y=24 (== BUTTON_H_LOGICAL_PX) 离开按钮区, 但仍在 titlebar (28)
        // → titlebar drag area (按钮在右上角紧贴顶部, 24-28 是 titlebar 边距).
        assert_eq!(hit_test(790.0, 24.0, 800, 600), HoverRegion::TitleBar);
    }

    #[test]
    fn hit_test_narrow_surface_button_overflow_falls_to_titlebar() {
        // 100×600: corner 8 / edge 4 后, Close x ∈ [76, 100), Max x ∈ [52, 76),
        // Min x ∈ [28, 52), titlebar 缝隙 x ∈ [8, 28).
        // x=10 y=10 应落 TitleBar (出 corner / edge / 三按钮 — Min 起点 28 > 10).
        assert_eq!(hit_test(10.0, 10.0, 100, 600), HoverRegion::TitleBar);
        // **T-0701 优先级修正**: 原测试 50×600 x=0 期望 TitleBar, 但新优先级
        // 下 (x<4) 必落 Left edge resize. 改测 100×600 x=10 出 corner / edge
        // 后落 TitleBar — 与原测试"窄 surface button 越界 fallback titlebar" 同
        // 语义, 仅迁出 4-edge / 8-corner 区域以适配 T-0701.
    }

    // ---- apply_* 决策子 fn 测试 (绕开 wl_surface 构造难题, 见
    // handle_pointer_event 文档头 "why 拆子 fn") ----

    fn fresh_state() -> PointerState {
        PointerState::new(800, 600)
    }

    #[test]
    fn enter_records_position_and_hovers_titlebar() {
        let mut state = fresh_state();
        let action = apply_enter(&mut state, 100.0, 10.0);
        assert_eq!(action, PointerAction::HoverChange(HoverRegion::TitleBar));
        assert_eq!(state.hover(), HoverRegion::TitleBar);
    }

    #[test]
    fn motion_within_same_region_returns_nothing() {
        let mut state = fresh_state();
        let _ = apply_enter(&mut state, 100.0, 10.0);
        // 在 titlebar 内移动
        let action = apply_motion(&mut state, 200.0, 15.0);
        assert_eq!(action, PointerAction::Nothing);
    }

    #[test]
    fn motion_crosses_to_close_button_emits_hover_change() {
        let mut state = fresh_state();
        let _ = apply_enter(&mut state, 100.0, 10.0);
        let action = apply_motion(&mut state, 790.0, 12.0);
        assert_eq!(
            action,
            PointerAction::HoverChange(HoverRegion::Button(WindowButton::Close))
        );
    }

    #[test]
    fn leave_clears_hover() {
        let mut state = fresh_state();
        let _ = apply_enter(&mut state, 100.0, 10.0);
        let action = apply_leave(&mut state);
        assert_eq!(action, PointerAction::HoverChange(HoverRegion::None));
        assert_eq!(state.hover(), HoverRegion::None);
    }

    #[test]
    fn left_button_press_on_titlebar_starts_move() {
        let mut state = fresh_state();
        let _ = apply_enter(&mut state, 100.0, 10.0);
        let action = apply_button(&mut state, 42, 0x110, true);
        assert_eq!(action, PointerAction::StartMove { serial: 42 });
    }

    #[test]
    fn left_button_press_on_close_emits_button_click() {
        let mut state = fresh_state();
        let _ = apply_enter(&mut state, 790.0, 12.0);
        let action = apply_button(&mut state, 42, 0x110, true);
        assert_eq!(action, PointerAction::ButtonClick(WindowButton::Close));
    }

    #[test]
    fn left_button_press_on_minimize_emits_click() {
        let mut state = fresh_state();
        let _ = apply_enter(&mut state, 740.0, 12.0);
        let action = apply_button(&mut state, 42, 0x110, true);
        assert_eq!(action, PointerAction::ButtonClick(WindowButton::Minimize));
    }

    #[test]
    fn left_button_press_on_maximize_emits_click() {
        let mut state = fresh_state();
        let _ = apply_enter(&mut state, 760.0, 12.0);
        let action = apply_button(&mut state, 42, 0x110, true);
        assert_eq!(action, PointerAction::ButtonClick(WindowButton::Maximize));
    }

    #[test]
    fn right_button_press_does_nothing() {
        let mut state = fresh_state();
        let _ = apply_enter(&mut state, 790.0, 12.0);
        let action = apply_button(&mut state, 42, 0x111, true);
        assert_eq!(action, PointerAction::Nothing);
    }

    #[test]
    fn release_does_not_trigger_action() {
        let mut state = fresh_state();
        let _ = apply_enter(&mut state, 790.0, 12.0);
        let action = apply_button(&mut state, 42, 0x110, false);
        assert_eq!(action, PointerAction::Nothing);
    }

    #[test]
    fn left_button_press_on_text_area_does_nothing() {
        let mut state = fresh_state();
        let _ = apply_enter(&mut state, 100.0, 200.0);
        let action = apply_button(&mut state, 42, 0x110, true);
        assert_eq!(action, PointerAction::Nothing);
    }

    // ---- T-0602 axis 滚动测试 ----

    /// 单次 axis value 不够 1 line 阈值 → Nothing, 累积器进位 (下次累加可凑够).
    #[test]
    fn axis_below_threshold_returns_nothing_and_accumulates() {
        let mut state = fresh_state();
        let action = apply_axis_vertical(&mut state, 10.0);
        assert_eq!(action, PointerAction::Nothing, "10 px < 24 阈值, 不发 line");
        assert!(
            (state.scroll_accum_y - 10.0).abs() < 1e-9,
            "累积器应记 10.0, 实际 {}",
            state.scroll_accum_y
        );
    }

    /// 累计跨阈值 → Scroll(±1). 24 px 正好阈值, 触发 1 line 反向 scroll
    /// (wl + value = 用户向下 = quill scroll(-1) 跳到底).
    #[test]
    fn axis_value_at_threshold_triggers_one_line_negative() {
        let mut state = fresh_state();
        let action = apply_axis_vertical(&mut state, SCROLL_ACCUM_LINE_PX);
        assert_eq!(
            action,
            PointerAction::Scroll(-1),
            "+24 px (用户向下手势) 应给 Scroll(-1) 看更新内容"
        );
    }

    /// 负 value (用户向上手势, 滚老内容) → Scroll(正), 与 alacritty
    /// `Scroll::Delta(+)` 同方向 (增 display_offset).
    #[test]
    fn axis_negative_value_at_threshold_gives_positive_scroll() {
        let mut state = fresh_state();
        let action = apply_axis_vertical(&mut state, -SCROLL_ACCUM_LINE_PX);
        assert_eq!(
            action,
            PointerAction::Scroll(1),
            "-24 px (用户向上手势) 应给 Scroll(+1) 看更老历史"
        );
    }

    /// 累积两格 (10 + 14 = 24), 第二格触发 1 line; 余量为 0.
    #[test]
    fn axis_two_steps_accumulate_to_one_line() {
        let mut state = fresh_state();
        assert_eq!(
            apply_axis_vertical(&mut state, 10.0),
            PointerAction::Nothing
        );
        let action = apply_axis_vertical(&mut state, 14.0);
        assert_eq!(action, PointerAction::Scroll(-1));
        assert!(
            state.scroll_accum_y.abs() < 1e-9,
            "余量应清零, 实际 {}",
            state.scroll_accum_y
        );
    }

    /// 一次大 value (触摸板快速滑) → 多 line 一次发. 72 px = 3 line.
    #[test]
    fn axis_large_value_emits_multi_line() {
        let mut state = fresh_state();
        let action = apply_axis_vertical(&mut state, 3.0 * SCROLL_ACCUM_LINE_PX);
        assert_eq!(action, PointerAction::Scroll(-3), "72 px 应给 Scroll(-3)");
    }

    /// 余量保留: 30 px 触发 1 line, 余 6 px; 再来 18 px 累积 24 触发 1 line.
    #[test]
    fn axis_remainder_carries_to_next_call() {
        let mut state = fresh_state();
        let action1 = apply_axis_vertical(&mut state, 30.0);
        assert_eq!(action1, PointerAction::Scroll(-1));
        assert!(
            (state.scroll_accum_y - 6.0).abs() < 1e-9,
            "余量应 6.0, 实际 {}",
            state.scroll_accum_y
        );
        let action2 = apply_axis_vertical(&mut state, 18.0);
        assert_eq!(
            action2,
            PointerAction::Scroll(-1),
            "30 + 18 = 48 = 2 line, 第二格出"
        );
    }

    /// 防御: NaN / Inf 不污染累积器.
    #[test]
    fn axis_nan_or_inf_is_ignored() {
        let mut state = fresh_state();
        let _ = apply_axis_vertical(&mut state, 10.0); // 进位 10
        let action = apply_axis_vertical(&mut state, f64::NAN);
        assert_eq!(action, PointerAction::Nothing);
        assert!(
            (state.scroll_accum_y - 10.0).abs() < 1e-9,
            "NaN 不该污染累积器"
        );
        let action_inf = apply_axis_vertical(&mut state, f64::INFINITY);
        assert_eq!(action_inf, PointerAction::Nothing);
        assert!((state.scroll_accum_y - 10.0).abs() < 1e-9);
    }

    /// handle_pointer_event 整体路径覆盖: VerticalScroll axis 走 apply_axis,
    /// 横向 axis (HorizontalScroll) 沉默 (quill 无横滚).
    #[test]
    fn handle_event_dispatches_vertical_axis_and_silences_horizontal() {
        let mut state = fresh_state();
        let event_v = wl_pointer::Event::Axis {
            time: 0,
            axis: WEnum::Value(wl_pointer::Axis::VerticalScroll),
            value: SCROLL_ACCUM_LINE_PX,
        };
        let action = handle_pointer_event(event_v, &mut state);
        assert_eq!(
            action,
            PointerAction::Scroll(-1),
            "VerticalScroll +24 应给 Scroll(-1)"
        );

        let event_h = wl_pointer::Event::Axis {
            time: 0,
            axis: WEnum::Value(wl_pointer::Axis::HorizontalScroll),
            value: 100.0,
        };
        let action_h = handle_pointer_event(event_h, &mut state);
        assert_eq!(
            action_h,
            PointerAction::Nothing,
            "HorizontalScroll 应沉默 (quill 无横滚)"
        );
    }

    // ---- T-0701 hit_test 8 边角 + apply_button StartResize 单测 ----

    /// 4 角各 8×8 logical, 优先 edge / button.

    #[test]
    fn hit_test_top_left_corner_returns_resize_topleft() {
        // 800×600: TopLeft 区 x ∈ [0, 8), y ∈ [0, 8)
        assert_eq!(
            hit_test(2.0, 2.0, 800, 600),
            HoverRegion::ResizeEdge(ResizeEdge::TopLeft)
        );
    }

    #[test]
    fn hit_test_top_right_corner_returns_resize_topright() {
        // 800×600: TopRight 区 x ∈ [792, 800), y ∈ [0, 8) — **优先 Close 按钮**
        // (Close x ∈ [776, 800) y ∈ [0, 24) 与右上 corner 区重叠 [792,800)×[0,8)).
        assert_eq!(
            hit_test(795.0, 3.0, 800, 600),
            HoverRegion::ResizeEdge(ResizeEdge::TopRight)
        );
    }

    #[test]
    fn hit_test_bottom_left_corner_returns_resize_bottomleft() {
        // 800×600: BottomLeft x ∈ [0, 8), y ∈ [592, 600)
        assert_eq!(
            hit_test(3.0, 595.0, 800, 600),
            HoverRegion::ResizeEdge(ResizeEdge::BottomLeft)
        );
    }

    #[test]
    fn hit_test_bottom_right_corner_returns_resize_bottomright() {
        // 800×600: BottomRight x ∈ [792, 800), y ∈ [592, 600)
        assert_eq!(
            hit_test(795.0, 598.0, 800, 600),
            HoverRegion::ResizeEdge(ResizeEdge::BottomRight)
        );
    }

    /// 4 边各 4 logical 厚, 避开 4 角.

    #[test]
    fn hit_test_top_edge_returns_resize_top() {
        // 800×600: 顶边 y ∈ [0, 4) 中段 (避开角 x ∈ [8, 792))
        assert_eq!(
            hit_test(400.0, 2.0, 800, 600),
            HoverRegion::ResizeEdge(ResizeEdge::Top)
        );
    }

    #[test]
    fn hit_test_bottom_edge_returns_resize_bottom() {
        // 800×600: 底边 y ∈ [596, 600), x 中段
        assert_eq!(
            hit_test(400.0, 598.0, 800, 600),
            HoverRegion::ResizeEdge(ResizeEdge::Bottom)
        );
    }

    #[test]
    fn hit_test_left_edge_returns_resize_left() {
        // 800×600: 左边 x ∈ [0, 4), y 中段 (避开角 y ∈ [8, 592))
        assert_eq!(
            hit_test(2.0, 300.0, 800, 600),
            HoverRegion::ResizeEdge(ResizeEdge::Left)
        );
    }

    #[test]
    fn hit_test_right_edge_returns_resize_right() {
        // 800×600: 右边 x ∈ [796, 800), y 中段
        assert_eq!(
            hit_test(798.0, 300.0, 800, 600),
            HoverRegion::ResizeEdge(ResizeEdge::Right)
        );
    }

    /// 边角越界 + 邻接验证 (优先级硬约束).

    #[test]
    fn hit_test_corner_overlaps_close_button_corner_wins() {
        // 800×600: x=795 y=3 落 TopRight (corner 优先, 派单 In #A "角覆盖优先 edge");
        // 内移到 x=795 y=10 (出 corner, 入 Close 区) 应落 Close.
        assert_eq!(
            hit_test(795.0, 10.0, 800, 600),
            HoverRegion::Button(WindowButton::Close)
        );
    }

    #[test]
    fn hit_test_top_edge_above_titlebar_buttons_returns_top() {
        // 800×600: y=3 在顶边 (RESIZE_EDGE_PX=4), 即使 x 在 Close 范围 (790)
        // 但避开 corner (x=790 < 792), 应落 Top edge — 优先 Top resize 而非
        // Close (派单 In #A "顶边走 Top 而非 titlebar drag" 推广到按钮).
        assert_eq!(
            hit_test(790.0, 3.0, 800, 600),
            HoverRegion::ResizeEdge(ResizeEdge::Top)
        );
    }

    #[test]
    fn hit_test_just_below_top_edge_falls_to_titlebar_button() {
        // y=4 (== RESIZE_EDGE_PX) 离开顶边, x=790 在 Close — 应落 Close.
        assert_eq!(
            hit_test(790.0, 4.0, 800, 600),
            HoverRegion::Button(WindowButton::Close)
        );
    }

    /// apply_button StartResize 路径覆盖.

    #[test]
    fn left_button_press_on_resize_edge_starts_resize() {
        let mut state = fresh_state();
        // 进入左边 (x=2 y=300) 触发 hover ResizeEdge::Left
        let _ = apply_enter(&mut state, 2.0, 300.0);
        assert_eq!(
            state.hover(),
            HoverRegion::ResizeEdge(ResizeEdge::Left),
            "前置: enter 应记 Left edge hover"
        );
        let action = apply_button(&mut state, 99, 0x110, true);
        assert_eq!(
            action,
            PointerAction::StartResize {
                serial: 99,
                edge: ResizeEdge::Left
            }
        );
    }

    #[test]
    fn left_button_press_on_resize_corner_starts_resize_with_corner_edge() {
        let mut state = fresh_state();
        // 进入右下角 (x=795 y=595) 触发 ResizeEdge::BottomRight
        let _ = apply_enter(&mut state, 795.0, 595.0);
        let action = apply_button(&mut state, 7, 0x110, true);
        assert_eq!(
            action,
            PointerAction::StartResize {
                serial: 7,
                edge: ResizeEdge::BottomRight
            }
        );
    }

    /// quill_edge_to_wayland 翻译表全 8 variant 覆盖 (INV-010 单一翻译边界).
    #[test]
    fn quill_edge_to_wayland_translates_all_variants() {
        use wayland_protocols::xdg::shell::client::xdg_toplevel::ResizeEdge as WlEdge;
        assert_eq!(quill_edge_to_wayland(ResizeEdge::Top), WlEdge::Top);
        assert_eq!(quill_edge_to_wayland(ResizeEdge::Bottom), WlEdge::Bottom);
        assert_eq!(quill_edge_to_wayland(ResizeEdge::Left), WlEdge::Left);
        assert_eq!(quill_edge_to_wayland(ResizeEdge::Right), WlEdge::Right);
        assert_eq!(quill_edge_to_wayland(ResizeEdge::TopLeft), WlEdge::TopLeft);
        assert_eq!(
            quill_edge_to_wayland(ResizeEdge::TopRight),
            WlEdge::TopRight
        );
        assert_eq!(
            quill_edge_to_wayland(ResizeEdge::BottomLeft),
            WlEdge::BottomLeft
        );
        assert_eq!(
            quill_edge_to_wayland(ResizeEdge::BottomRight),
            WlEdge::BottomRight
        );
    }

    #[test]
    fn set_surface_size_updates_hover_recomputation() {
        let mut state = fresh_state();
        // 鼠标在 800 surface 的 x=790 (Close 按钮内)
        let _ = apply_enter(&mut state, 790.0, 12.0);
        assert_eq!(state.hover(), HoverRegion::Button(WindowButton::Close));
        // 拖大 surface 到 1000, 同 x=790 已不在 Close 内 (新 Close x ∈ [976, 1000))
        // → titlebar drag area.
        state.set_surface_size(1000, 600);
        assert_eq!(state.hover(), HoverRegion::TitleBar);
    }
}
