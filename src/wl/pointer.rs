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
    /// 没事可做 (motion 不跨区 / Frame / Axis / 未识别按键).
    #[default]
    Nothing,
    /// titlebar 区域内 press: 触发 `xdg_toplevel.move(seat, serial)`,
    /// compositor 接管拖动. serial 是 button event 的 serial (Wayland 协议
    /// 要求 move 必须传最近 input event 的 serial, compositor 验证防伪造).
    StartMove { serial: u32 },
    /// 三按钮 click. 调用方按 button 分派 set_minimized / 切换 maximize /
    /// 关闭. **press → click**: 本 ticket 简化按 press 触发 click (与多数终端
    /// CSD 一致, alacritty / foot 同), 不做"press 同区 + release 同区" 的
    /// drag-cancel 检测 (Phase 6+ 可加).
    ButtonClick(WindowButton),
    /// hover 区域变化 — 调用方走 redraw 路径 (按钮 hover 变深, Close 变红).
    /// 包含**新**区域 (旧区域已存于 [`PointerState`] 内不外漏).
    HoverChange(HoverRegion),
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
}

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
        // Axis / AxisStop / AxisDiscrete / AxisSource / AxisRelativeDirection /
        // AxisValue120 / Frame: 派单 Out, Phase 6+ 滚轮选区接入.
        // wl_pointer Event 在 wayland-client 0.31 无 #[non_exhaustive], 但防
        // 上游升级加 variant — 默认沉默 (与 keyboard 模块同决策).
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
        HoverRegion::TextArea | HoverRegion::None => PointerAction::Nothing,
    }
}

/// 纯逻辑 hit-test: 给定 surface 内坐标 (logical px) 与 surface 尺寸 (logical),
/// 算 [`HoverRegion`]. 派单 In #F 抽决策模式硬约束, 单测覆盖 ≥6 case
/// (titlebar / 3 按钮 / text area / 边界).
///
/// **CSD 视觉布局** (单一来源, 与 [`crate::wl::render`] titlebar 渲染同源):
/// - 顶部 [`TITLEBAR_H_LOGICAL_PX`] (28 logical) 是 titlebar.
/// - 三按钮位于 titlebar 右端, 各 [`BUTTON_W_LOGICAL_PX`] × [`BUTTON_H_LOGICAL_PX`]
///   (24×24 logical), 顺序 (右→左) Close / Maximize / Minimize.
/// - titlebar 之下是 text area (cell grid).
/// - 超出 surface (x < 0 / y < 0 / x ≥ w / y ≥ h) → None.
///
/// **why 接 f64 而非 i32**: wl_pointer 坐标是 wl_fixed, wayland-client 转 f64.
/// 边界判断用 < / >= 而非 ≤, 与 NDC / 像素坐标系一致 (像素中心在整数偏 0.5,
/// 但 hit_test 不需精度到 sub-pixel, 直接用浮点比较即可).
///
/// **单一来源**: 三常数 (TITLEBAR_H / BUTTON_W / BUTTON_H) 都从 `render.rs`
/// 顶部 import. 改一处即视觉与逻辑同步, 无漂移风险.
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
    fn hit_test_close_right_edge_inclusive() {
        // x=799 仍在 Close 内 (< 800)
        assert_eq!(
            hit_test(799.0, 0.0, 800, 600),
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
    fn hit_test_extremely_narrow_surface_button_overflow_falls_to_titlebar() {
        // 50×600: Close x ∈ [26, 50), Max x ∈ [2, 26), Min x ∈ [-22, 2) 越界
        // → x=10 应落 Maximize (在 [2, 26) 内)
        assert_eq!(
            hit_test(10.0, 10.0, 50, 600),
            HoverRegion::Button(WindowButton::Maximize)
        );
        // x=0 在 Min 越界外, 落 titlebar
        assert_eq!(hit_test(0.0, 10.0, 50, 600), HoverRegion::TitleBar);
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
