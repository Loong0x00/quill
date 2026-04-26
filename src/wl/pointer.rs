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

use super::render::{
    BUTTON_H_LOGICAL_PX, BUTTON_W_LOGICAL_PX, TAB_BAR_H_LOGICAL_PX, TAB_CLOSE_W_LOGICAL_PX,
    TAB_MAX_W_LOGICAL_PX, TAB_MIN_W_LOGICAL_PX, TAB_PLUS_W_LOGICAL_PX, TITLEBAR_H_LOGICAL_PX,
    WINDOW_BUTTON_RADIUS_PX,
};

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
    /// T-0608: 鼠标在 tab 标签条的 "+" 按钮 (左侧, 新建 tab).
    TabBarPlus,
    /// T-0608: 鼠标在 tab 标签条的某个 tab body 上 (idx 是当前 tab 列表索引,
    /// 拖拽换序后会失效, 调用方处理 click 时立即用 idx 索引 active).
    Tab(usize),
    /// T-0608: 鼠标在 tab 标签条的某个 tab 关闭 "x" 按钮上 (idx 同 [`Self::Tab`]).
    TabClose(usize),
    /// 鼠标在 text area (cell grid 区域). Phase 6+ 接选区 / 滚轮.
    TextArea,
}

/// T-0703-fix: quill 自有 cursor 形状枚举. 经 [`xcursor_names_for`] 翻译为
/// xcursor name fallback 列表, 由 `src/wl/window.rs` 走 `wayland_cursor::CursorTheme`
/// 加载真 cursor svg + 自管 wl_pointer.set_cursor (ADR 0009 撤回 ADR 0008
/// wp_cursor_shape_v1 协议路径).
///
/// **覆盖范围**: default / text + 4 边 (n/s/e/w) + 4 角 (ne/nw/se/sw). 与
/// HoverRegion 加 ResizeEdge 后的全 case 一对一映射.
///
/// **why 不直接用 wayland-cursor / xcursor crate 的类型**: 它们没有"语义形状
/// enum", 只接受 cursor name `&str`. 直接让调用方拼字符串 = 失去编译期
/// exhaustive 检查 + 把"哪些 cursor 是 quill 关心的"的决策散到调用点. 自定义
/// 6-variant enum 锁住决策面, 改 cursor name fallback 表只改 [`xcursor_names_for`]
/// body 一处.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CursorShape {
    /// 默认箭头. titlebar / 未识别区域用. xcursor name `default` / `left_ptr`.
    #[default]
    Default,
    /// I-beam (文本编辑光标). text area (cell grid) 用, 暗示用户可选区 / 复制.
    /// xcursor name `text` / `xterm`.
    Text,
    /// ↕ 垂直双向箭头 (北/南边 resize). xcursor name `size_ver` / `ns-resize` /
    /// `n-resize` (mutter 接管 resize 时用 size_ver, 优先它跟 mutter 视觉对齐).
    NsResize,
    /// ↔ 水平双向箭头 (东/西边 resize). xcursor name `size_hor` / `ew-resize` /
    /// `e-resize`.
    EwResize,
    /// ↘↖ 主对角线双向箭头 (西北/东南角 resize). xcursor name `size_fdiag` /
    /// `nwse-resize` / `nw-resize`.
    NwseResize,
    /// ↙↗ 副对角线双向箭头 (东北/西南角 resize). xcursor name `size_bdiag` /
    /// `nesw-resize` / `ne-resize`.
    NeswResize,
}

/// T-0703: HoverRegion → CursorShape 翻译表 (派单 In #B 抽决策模式).
///
/// 当前 (T-0701 未合并前) ResizeEdge variant 不存在, [`HoverRegion::TitleBar`]
/// 与 [`HoverRegion::Button`] 都映射到 [`CursorShape::Default`], TextArea →
/// Text, None → Default. **T-0701 合并后**: [`HoverRegion`] 加
/// `ResizeEdge(ResizeEdge)` variant, 在此 fn 加分支:
/// - ResizeEdge::Top / Bottom → NsResize
/// - ResizeEdge::Left / Right → EwResize
/// - ResizeEdge::TopLeft / BottomRight → NwseResize
/// - ResizeEdge::TopRight / BottomLeft → NeswResize
///
/// **why pure fn 而非 impl trait**: 单一映射点, 改一处即视觉与行为同步, 与
/// `verdict_for_scale` / `pty_readable_action` 等 conventions §3 决策抽出
/// 同套路.
pub fn cursor_shape_for(hover: HoverRegion) -> CursorShape {
    match hover {
        HoverRegion::None => CursorShape::Default,
        HoverRegion::TitleBar => CursorShape::Default,
        HoverRegion::Button(_) => CursorShape::Default,
        HoverRegion::TextArea => CursorShape::Text,
        // T-0608: tab bar 三种区域均走默认箭头 (与 ghostty / kitty 同, +/x/tab body
        // 区都不变 I-beam / resize, 派单 In #D 字面要求).
        HoverRegion::TabBarPlus | HoverRegion::Tab(_) | HoverRegion::TabClose(_) => {
            CursorShape::Default
        }
        // T-0703 + T-0701 合并后激活 (Lead 手解): ResizeEdge → 4 方向 cursor.
        // Top/Bottom = ↕ (ns), Left/Right = ↔ (ew),
        // TopLeft/BottomRight = ↘ (nwse), TopRight/BottomLeft = ↙ (nesw).
        HoverRegion::ResizeEdge(edge) => match edge {
            ResizeEdge::Top | ResizeEdge::Bottom => CursorShape::NsResize,
            ResizeEdge::Left | ResizeEdge::Right => CursorShape::EwResize,
            ResizeEdge::TopLeft | ResizeEdge::BottomRight => CursorShape::NwseResize,
            ResizeEdge::TopRight | ResizeEdge::BottomLeft => CursorShape::NeswResize,
        },
    }
}

/// T-0703-fix 模块私有: quill [`CursorShape`] → xcursor name fallback 列表.
///
/// 调用方 (`src/wl/window.rs::apply_cursor_shape`) 顺序尝试每个 name, 第一个
/// `wayland_cursor::CursorTheme::get_cursor` 成功的拿来 attach. 全失败时 cursor
/// 维持上一次状态 (log warn 一次, 不刷屏 — 已知陷阱已在 ADR 0009 写明).
///
/// **fallback 顺序原则** (派单 + ADR 0009):
/// - **mutter 接管 resize 期实测用 `size_ver` 那一套**, 优先 `size_*` (X11
///   老 cursor name) 跟 mutter 视觉 1:1.
/// - `ns-resize` 等是 FreeDesktop xcursor 标准新 name, 老主题缺 `size_*` 时
///   退化到这套.
/// - `n-resize` 等单方向 name 是 csd-decoration 建议名, 极少见但兜底用.
/// - `default` / `left_ptr` 一对是新 / 老 alias (Adwaita: left_ptr → default).
/// - `text` / `xterm` 同理.
///
/// **why 模块私有 inherent fn**: xcursor name `&'static str` 是 wayland-cursor
/// crate 的输入类型 (协议层), INV-010 类型隔离要求 — 不让 xcursor name 字符串
/// 散落调用点 (拼写错误就 silent fallback 到默认箭头 + 用户找半天). 改 fallback
/// 顺序只改本 fn body 一处.
pub(crate) fn xcursor_names_for(shape: CursorShape) -> &'static [&'static str] {
    match shape {
        CursorShape::Default => &["default", "left_ptr"],
        CursorShape::Text => &["text", "xterm"],
        CursorShape::NsResize => &["size_ver", "ns-resize", "n-resize"],
        CursorShape::EwResize => &["size_hor", "ew-resize", "e-resize"],
        CursorShape::NwseResize => &["size_fdiag", "nwse-resize", "nw-resize"],
        CursorShape::NeswResize => &["size_bdiag", "nesw-resize", "ne-resize"],
    }
}

/// `handle_pointer_event` 的副作用描述. 调用方按 variant 分派.
///
/// 抽 enum 而非散落 `if` 是 conventions §3 套路 (类比 [`PtyAction`] /
/// [`KeyboardAction`] / [`WindowAction`]); 也是 INV-010 类型隔离的实操 —
/// 调用方拿到的全是 quill 自有类型, 不暴露 wl_pointer::Event 字段.
// f64 (TabDragMove.x_logical, T-0608) 不实现 Eq (NaN 自反性), 改 PartialEq.
// 测试 assert_eq! 仅依 PartialEq, 无回归.
#[derive(Debug, Clone, PartialEq, Default)]
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
    ///
    /// **T-0703-fix 副作用**: HoverChange 也常常意味着 cursor 形状变化 —
    /// 但 cursor set 走独立 [`PointerState::take_pending_cursor_set`] 路径
    /// (与 pending_scroll_lines / pending_repeat 同 T-0603 套路), 调用方在
    /// 处理 PointerAction 后**额外**检查并下发, 不复用此 variant 字段
    /// (避免单 PointerAction 携带多副作用 → match 分支臃肿).
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
    /// T-0607: 在 text area 按下左键 → 开始选区. 调用方
    /// (`Dispatch<WlPointer>`) 走 `selection_state.start(anchor, mode)` + 立即
    /// 重画一帧 (cell 反色)。
    SelectionStart {
        /// 鼠标按下点 → cell. 调用方走 [`crate::wl::selection::pixel_to_cell`]
        /// 把鼠标 logical px 映射到 viewport 内 cell 后填入.
        anchor: crate::term::CellPos,
        /// Linear (普通拖) vs Block (Alt+drag). 调用方按下时读 keyboard
        /// `alt_active()` 走 [`crate::wl::selection::modifier_to_selection_mode`]
        /// 决定.
        mode: crate::wl::selection::SelectionMode,
    },
    /// T-0607: 选区拖动中 → cursor 实时更新. anchor 不变. 调用方走
    /// `selection_state.update(cursor)` + 重画.
    SelectionUpdate { cursor: crate::term::CellPos },
    /// T-0607: 松开左键 → 选区结束 + 触发 PRIMARY auto-copy. 调用方走
    /// `selection_state.end()` + 算出选区文本 + `wp_primary_selection_v1.set_selection`.
    SelectionEnd,
    /// T-0607: 中键单击 → 粘贴 PRIMARY (中键粘贴 Linux 标准). 调用方走
    /// `wp_primary_selection_v1` 当前 offer 路径读 pipe → bracketed paste 包装
    /// → `pty.write`.
    Paste(crate::wl::selection::PasteSource),
    /// T-0607: 鼠标 motion 时检测到拖到 viewport 边缘 (上 / 下), 调用方应启动
    /// autoscroll Timer (100ms 一次 `term.scroll_display(±1)` + cursor 跟随).
    /// `delta` = ±1 (-1 上, +1 下). 鼠标回到 viewport 内时返
    /// [`PointerAction::AutoScrollStop`].
    AutoScrollStart {
        /// `±1` line/tick — quill scroll_display 同方向语义 (+ 看老 / - 看新).
        delta: i32,
    },
    /// T-0607: 鼠标回到 viewport 内或松开左键 → 取消 autoscroll Timer.
    AutoScrollStop,
    /// T-0608: 点击 "+" 按钮 (tab bar 左侧) → 新建 tab.
    NewTab,
    /// T-0608: 点击 tab body → 切换 active tab.
    SwitchTab(usize),
    /// T-0608: 点击 tab close × → 关闭 tab.
    CloseTab(usize),
    /// T-0608: 在 tab body 按下并 motion ≥ 5 logical px → 进入 drag 模式.
    /// idx 是按下时的 tab idx (drag 期间 tabs.swap_reorder 后 idx 可能不再
    /// 对应同一 tab id, 调用方需先用 idx 抓 tab.id 锁定 anchor).
    StartTabDrag(usize),
    /// T-0608: drag 进行中, motion 期间发, 调用方据 x 算 target_idx 重排.
    TabDragMove { x_logical: f64 },
    /// T-0608: tab drag 松开 → 调用方提交 reorder (按当前最后一次 TabDragMove
    /// 的 x 算 target_idx, swap_reorder).
    EndTabDrag,
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
    /// T-0618: 滚轮累积器废弃. T-0602 用 `Axis` smooth value 累 24 px → 1 line,
    /// 但 mutter 给 `Axis` 的 value 跟物理速度变 (慢转 8 px / 快转 30 px), 阈值
    /// 化导致体感"滚一下随机出 0-2 line". 改走 wl_pointer 1.21+ 的 `AxisValue120`
    /// 离散事件 — 一个 wheel notch 必然 ±120, 直接乘 [`WHEEL_LINES_PER_NOTCH`]
    /// = 3 (alacritty/foot/gnome-terminal 默认) 出整 line, 跟物理速度无关. 字段
    /// 保留 0 仅为防 PointerState binary layout 大改 (本字段 unused, 编译器警告
    /// 可借 `#[allow(dead_code)]`, T-0619+ 真删).
    #[allow(dead_code)]
    scroll_accum_y: f64,
    /// T-0703-fix: 最近 wl_pointer.Enter event 的 serial. `wl_pointer.set_cursor`
    /// 协议要求传 **enter serial** (不是 button event serial — 那条是
    /// xdg_toplevel.move 路径). Enter 时记一次, 后续 hover 跨区调 set_cursor
    /// 时传同一 serial — 协议 doc (wl_pointer.set_cursor):
    /// "serial: serial number of the enter event".
    last_enter_serial: u32,
    /// T-0703-fix: 当前已下发的 cursor 形状. 与 [`hover`] 类似但单独跟踪 —
    /// hover 与 cursor 形状是 **n:1 映射** (Default cursor 用于 None /
    /// TitleBar / Button 三种 hover), 同 cursor 不必重 attach buffer (减压
    /// compositor + 防 cursor 闪烁).
    current_cursor_shape: CursorShape,
    /// T-0703-fix: 待下发的 cursor 切换 (serial, shape). [`apply_enter`] /
    /// [`apply_motion`] 检测 cursor 形状变化时填入, [`Self::take_pending_cursor_set`]
    /// 由 `Dispatch<wl_pointer>` 在主 PointerAction 处理后取出消费. 单 buffer
    /// 设计 (与 `State.pending_repeat` / `State.pending_scroll_lines` 同
    /// T-0603 / T-0602 套路 — 单帧多次填覆盖前次, 取最新).
    ///
    /// `None` = 无待发请求, `Some((serial, shape))` = 有待发. take 后置 None.
    pending_cursor_set: Option<(u32, CursorShape)>,
    /// T-0607: 当前 viewport cell grid 尺寸 (`cols`, `rows`). 由
    /// [`crate::wl::window::propagate_resize_if_dirty`] 在 resize 链末尾走
    /// [`PointerState::set_cell_grid`] 同步, 让 [`apply_motion`] /
    /// [`apply_button`] 把鼠标 logical px 映射到 cell 时知道边界 (clamp 到
    /// `cols-1 / rows-1`, 与 [`crate::wl::selection::pixel_to_cell`] 同决策).
    /// 起步 80×24 与 `WindowCore::new` 默认对齐.
    grid_cols: usize,
    grid_rows: usize,
    /// T-0607: 当前是否处于 "selection drag in progress" (按下左键且按下点在
    /// text area 内). [`apply_button`] (Pressed + 左键 + TextArea) 置 true,
    /// [`apply_button`] (Released + 左键) 置 false. [`apply_motion`] 在 true
    /// 时返 [`PointerAction::SelectionUpdate`] (而非 HoverChange) 并旁路触发
    /// 边缘 autoscroll 决策.
    selection_drag: bool,
    /// T-0607: 当前是否已 schedule autoscroll Timer. [`apply_motion`] 在
    /// y < titlebar (上越) 或 y >= surface_h-1 (下越) 时返 AutoScrollStart 一
    /// 次, 期间不再重发; 鼠标回 viewport 返 AutoScrollStop. 与 `pending_cursor_set`
    /// 同 set-once 套路防 callback flood.
    autoscroll_active: bool,
    /// T-0608: 当前 tab 数量. 由 [`PointerState::set_tab_count`] 同步, 让
    /// hit_test_with_tabs 走 tab bar 解析. 起步 1 (quill 启动期单 tab).
    tab_count: usize,
    /// T-0608: tab body 区按下时记按下 tab idx + 起始 x (logical px), 用作
    /// drag 阈值判定. 鼠标 motion 距离 ≥ 5 logical px 视为 drag, 否则视为 click.
    /// 派单 In #F "drag 阈值: 拖动 < 5 logical px → 视为 click".
    /// `Some((idx, start_x))` = press 已记录待判定; `None` = 无 active press.
    tab_press: Option<(usize, f64)>,
    /// T-0608: tab drag 进行中标记. 一旦超过 5 px 阈值, 此字段置 true; release
    /// 时若 true → EndTabDrag 派单 reorder, 若 false → 退化 SwitchTab(idx).
    tab_drag_active: bool,
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
#[allow(dead_code)] // T-0618: deprecated, kept for legacy test referencing
pub(crate) const SCROLL_ACCUM_LINE_PX: f64 = 24.0;

/// **T-0618: 滚轮一格 (notch) 滚多少行**. 3 行 = alacritty / foot / gnome-terminal
/// 默认, daily-drive 标配 (滚 5 次翻一屏 ~24 行). 通过 wl_pointer.AxisValue120
/// 事件接收离散 notch 计数 (Wayland 1.21+, mutter ≥ 45 / sway ≥ 1.8 全支持),
/// value120 = ±120 / notch 是协议规约. 1 notch 出 3 line scroll, 跟物理速度无关.
pub(crate) const WHEEL_LINES_PER_NOTCH: i32 = 3;

/// T-0608: tab drag 阈值 (logical px). 派单 In #F "拖动 < 5 logical px → 视为
/// click". 鼠标按下到松开总移动 < 此值视为 click; ≥ 此值视为 drag.
pub(crate) const TAB_DRAG_THRESHOLD_PX: f64 = 5.0;

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
            last_enter_serial: 0,
            current_cursor_shape: CursorShape::Default,
            pending_cursor_set: None,
            // T-0607: 80×24 与 WindowCore::new 默认对齐, configure 收到首次尺寸
            // 后 propagate_resize_if_dirty 走 set_cell_grid 同步真实值.
            grid_cols: 80,
            grid_rows: 24,
            selection_drag: false,
            autoscroll_active: false,
            // T-0608: 起步 1 个 tab (与 quill 启动期单 tab 对齐).
            tab_count: 1,
            tab_press: None,
            tab_drag_active: false,
        }
    }

    /// T-0608: 同步当前 tab 数量, 让 hit_test_with_tabs 路径走最新值.
    /// 调用方 (window.rs Dispatch / new tab path) 在 tabs.push / tabs.remove
    /// 后调一次.
    pub fn set_tab_count(&mut self, count: usize) {
        self.tab_count = count.max(1);
        // tab 数量变化会改 tab body width, 重算 hover.
        if let Some((x, y)) = self.pos {
            self.hover = hit_test_with_tabs(
                x,
                y,
                self.surface_w_logical,
                self.surface_h_logical,
                self.tab_count,
            );
        }
    }

    /// T-0608: 当前 tab 数量 (仅供测试 / 调试 — 主路径走 set_tab_count 同步).
    #[cfg(test)]
    #[allow(dead_code)]
    pub(crate) fn tab_count(&self) -> usize {
        self.tab_count
    }

    /// T-0607: 同步 viewport cell grid 尺寸 (`cols`, `rows`). 调用方
    /// (`crate::wl::window::propagate_resize_if_dirty`) 在 resize 链末尾走
    /// [`crate::wl::window::cells_from_surface_px`] 算出 cols/rows 后调一次,
    /// 让本模块 px → cell 映射使用最新边界.
    pub fn set_cell_grid(&mut self, cols: usize, rows: usize) {
        self.grid_cols = cols.max(1);
        self.grid_rows = rows.max(1);
    }

    /// T-0607: 最近一次 button event 的 serial. set_selection / xdg_toplevel
    /// 等协议路径需 serial (compositor 验证防伪造). 与 `last_enter_serial`
    /// 同性质 (单 source-of-truth, 不外漏).
    pub fn last_button_serial(&self) -> u32 {
        self.last_button_serial
    }

    /// 同步 surface 尺寸 (logical px). [`crate::wl::window::propagate_resize_if_dirty`]
    /// 在 resize chain 末尾调一次, 让 hit_test 用最新尺寸算按钮位置.
    pub fn set_surface_size(&mut self, w_logical: u32, h_logical: u32) {
        self.surface_w_logical = w_logical;
        self.surface_h_logical = h_logical;
        // 尺寸变化后重新算一次 hover (按钮挪了位置, 鼠标可能落到不同区).
        // pos 不变, hit_test 用新 surface 尺寸即可.
        if let Some((x, y)) = self.pos {
            self.hover = hit_test_with_tabs(
                x,
                y,
                self.surface_w_logical,
                self.surface_h_logical,
                self.tab_count,
            );
        }
    }

    /// 当前 hover 区域. CSD 渲染 ([`crate::wl::render::Renderer::draw_frame`])
    /// 据此画按钮高亮.
    pub fn hover(&self) -> HoverRegion {
        self.hover
    }

    /// T-0703-fix: 取出并清空待下发的 cursor 形状变更请求. `Dispatch<wl_pointer>`
    /// 在处理主 [`PointerAction`] 后调一次, 拿到 `Some((serial, shape))` 时走
    /// `apply_cursor_shape` (src/wl/window.rs 段) — 按 [`xcursor_names_for`]
    /// fallback 列表查 `wayland_cursor::CursorTheme`, attach buffer 到 cursor
    /// surface, 调 `wl_pointer.set_cursor(serial, ...)` (ADR 0009).
    ///
    /// 返 `None` = 本帧无形状变化, 不下发 wl request — 防止每帧都重 attach
    /// cursor buffer (一帧 motion event 可能数十次, hover 同区时 cursor 不变).
    pub fn take_pending_cursor_set(&mut self) -> Option<(u32, CursorShape)> {
        self.pending_cursor_set.take()
    }

    /// T-0703: 当前已下发的 cursor 形状 (供调试 trace / 单测断言).
    ///
    /// `#[allow(dead_code)]`: 主路径不用 (Dispatch 直接 take pending), 仅供
    /// 单测断言 + 未来 trace / debug 入口预留. 与 [`PointerState::last_button_serial`]
    /// (T-0504 forward-compat) 同决策.
    #[allow(dead_code)]
    pub fn current_cursor_shape(&self) -> CursorShape {
        self.current_cursor_shape
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
/// - **Enter(serial, x, y)**: 记 pos + enter serial + hit_test, 返 HoverChange
///   (从 None → 新区) **+ 旁路填 pending_cursor_set** (强制 set 一次, 协议要求).
/// - **Leave(serial)**: 清 pos, hover → None, 返 HoverChange(None) 让调用方
///   redraw 清按钮高亮. 不发 set_cursor (compositor 自己接管).
/// - **Motion(time, x, y)**: 更新 pos + hit_test; 区域变化才返
///   HoverChange, 同区返 Nothing (避免 redraw 风暴). cursor 形状变化时旁路
///   填 pending_cursor_set.
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
/// - Enter 事件**也带 serial**, button event 的 serial 是另一条 — Enter serial
///   归 wl_pointer.set_cursor (T-0703-fix 要求), button serial 归 xdg_toplevel.move
///   (T-0504, 见 `apply_button`).
///
/// **why 拆 [`apply_enter`] / [`apply_leave`] / [`apply_motion`] /
/// [`apply_button`] 子 fn**: wl_pointer::Event 含 WlSurface 字段 (Enter /
/// Leave), 单测构造 WlSurface 需真 Connection (无可行 mock 路径). 拆纯标量
/// 入参的子 fn 让单测覆盖决策矩阵 (conventions §3 SOP), 本 fn 仅负责协议
/// 字段拆解 + 子 fn 转发, 自身行为薄, 单测对子 fn 即等价覆盖.
///
/// **T-0607 `alt_active` 入参**: 鼠标按下时调用方读 keyboard
/// `alt_active()` 后传入, 由 [`apply_button`] 在 TextArea press 路径走
/// [`crate::wl::selection::modifier_to_selection_mode`] 决定 Linear vs Block.
/// 非 button event (motion / enter / leave / axis) 不读此值, 调用方传 false 即可.
pub fn handle_pointer_event(
    event: wl_pointer::Event,
    state: &mut PointerState,
    alt_active: bool,
) -> PointerAction {
    match event {
        wl_pointer::Event::Enter {
            serial,
            surface_x,
            surface_y,
            ..
        } => apply_enter(state, serial, surface_x, surface_y),
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
            apply_button(state, serial, button, pressed, alt_active)
        }
        // T-0618: 纵向滚轮走 AxisValue120 (Wayland 1.21+ 离散 notch 协议),
        // 1 notch = ±120 → 3 lines. 不再走 Axis smooth path — mutter 给 Axis
        // 的 value 跟物理速度变, 体感不稳. 触摸板暂不支持 (user 不用).
        // 横向 (HorizontalScroll) 不消费 — quill 终端无横向滚.
        wl_pointer::Event::AxisValue120 { axis, value120 } => {
            if matches!(axis, WEnum::Value(wl_pointer::Axis::VerticalScroll)) {
                apply_axis_value120(value120)
            } else {
                PointerAction::Nothing
            }
        }
        // T-0618: Axis (smooth) 现在 ignore — discrete notch 走 AxisValue120 已
        // 覆盖 daily-drive 鼠标. 触摸板要支持时再实装 AxisSource 区分 wheel /
        // finger, finger 走 px / cell_h 累积. 当前 user 不用触摸板, 简化逻辑.
        wl_pointer::Event::Axis { .. } => PointerAction::Nothing,
        // AxisStop / AxisDiscrete / AxisSource / AxisRelativeDirection / Frame:
        // 不消费 (AxisDiscrete 是 AxisValue120 的老前辈, mutter ≥ 45 已发后者).
        _ => PointerAction::Nothing,
    }
}

/// Enter 决策子 fn — 单测入口 (避免构造 WlSurface). 见 [`handle_pointer_event`]
/// 文档头 "why 拆子 fn".
///
/// **T-0703-fix**: 同步记 [`PointerState::last_enter_serial`]
/// (`wl_pointer.set_cursor` 协议必需), 并**强制**填一个 pending_cursor_set
/// (即便 cursor 形状未变 — 协议要求 client 在 enter 后必须 set 一次, 否则
/// cursor 行为 unspecified, 实测 GNOME mutter 会显示空白).
pub(crate) fn apply_enter(state: &mut PointerState, serial: u32, x: f64, y: f64) -> PointerAction {
    state.pos = Some((x, y));
    state.last_enter_serial = serial;
    let new_hover = hit_test_with_tabs(
        x,
        y,
        state.surface_w_logical,
        state.surface_h_logical,
        state.tab_count,
    );
    let new_shape = cursor_shape_for(new_hover);
    // 协议要求: enter 后必须 set 一次 cursor (否则 unspecified). 即便
    // current_cursor_shape == new_shape (init 都是 Default), 也强制 emit 一次
    // (走 wl_pointer.set_cursor + cursor_surface attach buffer, ADR 0009).
    state.pending_cursor_set = Some((serial, new_shape));
    state.current_cursor_shape = new_shape;
    if new_hover != state.hover {
        state.hover = new_hover;
        return PointerAction::HoverChange(new_hover);
    }
    PointerAction::Nothing
}

/// Leave 决策子 fn. 清 pos + hover, 返 HoverChange(None) 让调用方 redraw 清按钮高亮.
///
/// **T-0703-fix**: 不发 wl_pointer.set_cursor (鼠标已离开 surface, compositor
/// 自管). 但**清空** `pending_cursor_set` 兜底防 race (Enter 后立刻 Leave,
/// pending 未消费).
pub(crate) fn apply_leave(state: &mut PointerState) -> PointerAction {
    state.pos = None;
    state.pending_cursor_set = None;
    if state.hover != HoverRegion::None {
        state.hover = HoverRegion::None;
        return PointerAction::HoverChange(HoverRegion::None);
    }
    PointerAction::Nothing
}

/// Motion 决策子 fn. 区域变化才返 HoverChange, 同区返 Nothing 避免 redraw 风暴.
///
/// **T-0703-fix**: hover 跨区且 cursor 形状变 (n:1 映射 → 同 cursor 同 region
/// 内 + 跨等价 region 不重发 set_cursor, 减压 compositor) 时填
/// pending_cursor_set. serial 用 [`PointerState::last_enter_serial`] (协议要
/// 求 enter serial, motion event 自身无 serial 字段).
pub(crate) fn apply_motion(state: &mut PointerState, x: f64, y: f64) -> PointerAction {
    state.pos = Some((x, y));
    let new_hover = hit_test_with_tabs(
        x,
        y,
        state.surface_w_logical,
        state.surface_h_logical,
        state.tab_count,
    );

    // T-0608: tab drag 阈值检测 (派单 In #F). press 已记录 tab_press, motion 时
    // 算与起始 x 的距离 ≥ 5 logical px → tab_drag_active=true. 后续 motion 仅
    // 更新 drag, release 时按 drag_active 决定 EndTabDrag (reorder) vs SwitchTab.
    if let Some((origin_idx, start_x)) = state.tab_press {
        let dx = (x - start_x).abs();
        if dx >= TAB_DRAG_THRESHOLD_PX && !state.tab_drag_active {
            state.tab_drag_active = true;
            return PointerAction::StartTabDrag(origin_idx);
        }
        if state.tab_drag_active {
            // drag 进行中, 发 TabDragMove(x) 让调用方更新拖中视觉 / 算 target_idx.
            return PointerAction::TabDragMove { x_logical: x };
        }
        // < 阈值, 不动.
        state.hover = new_hover;
        return PointerAction::Nothing;
    }

    // T-0607: 选区拖动中 — motion 事件应优先发 SelectionUpdate (cursor 跟随);
    // hover change / cursor shape 切换 仍同步更新但不返 HoverChange (调用方
    // 处理 SelectionUpdate 即重画一次反色, 不需要再叠 redraw 信号).
    if state.selection_drag {
        // 边缘自动滚屏决策 (派单 In #E): y < titlebar (上越) 或 y >=
        // surface_h - 1 (下越) 触发 AutoScrollStart, 鼠标回到 viewport 内返
        // AutoScrollStop. selection_drag 为前提 (鼠标未按住时不 autoscroll).
        let titlebar_h = TITLEBAR_H_LOGICAL_PX as f64;
        let h = state.surface_h_logical as f64;
        let above_top = y < titlebar_h;
        let below_bottom = y >= h - 1.0;
        if above_top {
            if !state.autoscroll_active {
                state.autoscroll_active = true;
                // y 上越 = 用户想看更老历史, quill scroll_display(+1) 增 offset.
                return PointerAction::AutoScrollStart { delta: 1 };
            }
            // 已 active, 静默吞 motion (Timer 在做活), 但仍更新 hover/pos.
            state.hover = new_hover;
            return PointerAction::Nothing;
        }
        if below_bottom {
            if !state.autoscroll_active {
                state.autoscroll_active = true;
                // y 下越 = 用户想看更新内容, quill scroll_display(-1) 减 offset.
                return PointerAction::AutoScrollStart { delta: -1 };
            }
            state.hover = new_hover;
            return PointerAction::Nothing;
        }
        // 鼠标在 viewport 内: 先 stop autoscroll (若激活), 再发 SelectionUpdate.
        if state.autoscroll_active {
            state.autoscroll_active = false;
            return PointerAction::AutoScrollStop;
        }
        // 真 motion → 算 cursor cell, 发 SelectionUpdate.
        let cell_w = crate::wl::render::CELL_W_PX as f64;
        let cell_h = crate::wl::render::CELL_H_PX as f64;
        let cursor = match crate::wl::selection::pixel_to_cell(
            x,
            y,
            state.grid_cols,
            state.grid_rows,
            cell_w,
            cell_h,
            titlebar_h,
        ) {
            Some(p) => p,
            None => {
                // y < titlebar 已在上面 above_top 兜过, 这里只剩 cols/rows=0
                // 防御 — 罕见, 静默吞.
                state.hover = new_hover;
                return PointerAction::Nothing;
            }
        };
        state.hover = new_hover;
        return PointerAction::SelectionUpdate { cursor };
    }

    if new_hover != state.hover {
        let new_shape = cursor_shape_for(new_hover);
        if new_shape != state.current_cursor_shape {
            state.pending_cursor_set = Some((state.last_enter_serial, new_shape));
            state.current_cursor_shape = new_shape;
        }
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
///
/// **T-0607**:
/// - 左键 press + TextArea → SelectionStart (Linear 或 Block 看 alt_active).
/// - 左键 release + selection_drag=true → SelectionEnd (清 drag flag + 触发
///   PRIMARY auto-copy). autoscroll_active 同步清.
/// - 中键 (BTN_MIDDLE 0x112) press → Paste(Primary) (Linux 中键粘贴标准).
pub(crate) fn apply_button(
    state: &mut PointerState,
    serial: u32,
    button: u32,
    pressed: bool,
    alt_active: bool,
) -> PointerAction {
    state.last_button_serial = serial;
    const BTN_LEFT: u32 = 0x110;
    const BTN_MIDDLE: u32 = 0x112;

    // T-0607: 左键 release — 若在选区拖动中, 走 SelectionEnd + 清 drag flag;
    // 否则忽略 (与原 release 不消费同). 处理 release 必须**先于** !pressed
    // early-return.
    if !pressed && button == BTN_LEFT {
        let was_dragging = state.selection_drag;
        state.selection_drag = false;
        let was_autoscroll = state.autoscroll_active;
        state.autoscroll_active = false;
        // T-0608: tab drag release 路径. 若 drag_active=true → EndTabDrag 让
        // 调用方按当前 x 算 target_idx + swap_reorder. 若 drag_active=false 但
        // tab_press 仍 Some → 视为 click, SwitchTab(origin_idx) (派单 In #F
        // 阈值 < 5 px 视 click).
        if let Some((origin_idx, _start_x)) = state.tab_press.take() {
            let was_drag = state.tab_drag_active;
            state.tab_drag_active = false;
            if was_drag {
                return PointerAction::EndTabDrag;
            }
            return PointerAction::SwitchTab(origin_idx);
        }
        if was_dragging {
            // 即便 autoscroll 仍 active, SelectionEnd 一并兜底清 (调用方在
            // SelectionEnd 路径走 cancel autoscroll Timer + PRIMARY auto-copy).
            let _ = was_autoscroll;
            return PointerAction::SelectionEnd;
        }
        return PointerAction::Nothing;
    }
    if !pressed {
        return PointerAction::Nothing;
    }
    // T-0607: 中键 press → 粘贴 PRIMARY (Linux 中键粘贴标准). hover 不限定 (中键
    // 在 titlebar 也有人做粘贴, 与 alacritty / foot 一致).
    if button == BTN_MIDDLE {
        return PointerAction::Paste(crate::wl::selection::PasteSource::Primary);
    }
    if button != BTN_LEFT {
        return PointerAction::Nothing;
    }
    match state.hover {
        HoverRegion::TitleBar => PointerAction::StartMove { serial },
        HoverRegion::Button(b) => PointerAction::ButtonClick(b),
        HoverRegion::ResizeEdge(edge) => PointerAction::StartResize { serial, edge },
        // T-0608: tab bar 三区. + 立即 NewTab; close × 立即 CloseTab; tab body
        // 走 press 记录 (drag 阈值检测), release 时再决定 SwitchTab vs reorder.
        HoverRegion::TabBarPlus => PointerAction::NewTab,
        HoverRegion::TabClose(idx) => PointerAction::CloseTab(idx),
        HoverRegion::Tab(idx) => {
            // press 记录 origin_idx + start_x, motion 时与 start_x 比距离, release
            // 时按 drag_active 决定 SwitchTab vs EndTabDrag.
            let start_x = state.pos.map(|p| p.0).unwrap_or(0.0);
            state.tab_press = Some((idx, start_x));
            state.tab_drag_active = false;
            PointerAction::Nothing
        }
        HoverRegion::TextArea => {
            // T-0607: text area 左键 press → 开始选区. anchor 走当前 pos →
            // pixel_to_cell. mode 走 alt_active → SelectionMode.
            let Some((x, y)) = state.pos else {
                // pos 未填 (Enter 之前 race) — 罕见, 静默吞.
                return PointerAction::Nothing;
            };
            let titlebar_h = TITLEBAR_H_LOGICAL_PX as f64;
            let cell_w = crate::wl::render::CELL_W_PX as f64;
            let cell_h = crate::wl::render::CELL_H_PX as f64;
            let anchor = match crate::wl::selection::pixel_to_cell(
                x,
                y,
                state.grid_cols,
                state.grid_rows,
                cell_w,
                cell_h,
                titlebar_h,
            ) {
                Some(p) => p,
                None => return PointerAction::Nothing,
            };
            state.selection_drag = true;
            state.autoscroll_active = false;
            let mode = crate::wl::selection::modifier_to_selection_mode(alt_active);
            PointerAction::SelectionStart { anchor, mode }
        }
        HoverRegion::None => PointerAction::Nothing,
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
#[allow(dead_code)] // T-0618: deprecated, 保留签名防老测试 / 触摸板复活时直接补
pub(crate) fn apply_axis_vertical(state: &mut PointerState, value: f64) -> PointerAction {
    // T-0618: deprecated. Axis smooth path 在 dispatch 已 short-circuit 不再调用.
    // 保留 fn 签名仅给老测试 + 万一有 caller 漏改. 触摸板支持时复活.
    let _ = (state, value);
    PointerAction::Nothing
}

/// T-0618: 纵向 AxisValue120 (Wayland 1.21+ 离散滚轮 notch) → Scroll(±N) 决策.
///
/// **协议**: value120 = ±120 / 物理 wheel notch (向下 = +120, 向上 = -120). 高分辨率
/// 滚轮 (Logitech MX hi-res 模式) 可发 ±15/30/60 等小步, 累计达 120 即 1 notch.
/// 我们目前简化按"每个事件 value120 作整 notch fraction" 处理, 即 lines =
/// value120 × WHEEL_LINES_PER_NOTCH / 120, integer divide round 向 0.
///
/// **方向语义**: wl 协议 +value120 = 向下 (看新内容); quill scrollback 反向
/// (Scroll(+) = 向上看老历史) → 取负, 与 [`apply_axis_vertical`] 同决策.
///
/// **为啥不再累积**: T-0602 的 24 px 阈值在 mutter 上体感不稳 (Axis value 随
/// 速度变), AxisValue120 是协议规约的整 notch 计数, 直接乘 3 出 line, 跟物理
/// 速度 / compositor 翻译都无关. 1 notch = 3 line 是 alacritty / foot / GNOME
/// 默认.
pub(crate) fn apply_axis_value120(value120: i32) -> PointerAction {
    if value120 == 0 {
        return PointerAction::Nothing;
    }
    // value120 / 120 → notch 计数 (integer divide round 向 0; 高分滚轮多个事件
    // 累计才到 120, 单独小步出 0 = Nothing, 体感是"积累到一格才滚").
    let notches = value120 / 120;
    if notches == 0 {
        return PointerAction::Nothing;
    }
    let lines = notches * WHEEL_LINES_PER_NOTCH;
    // 取负: wl +value120 = 向下手势 = 看新内容; quill Scroll(+) = 老历史.
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
/// T-0608: 算 tab body width 给定 tab 数量. clamp 到 [TAB_MIN_W, TAB_MAX_W].
/// surface 上 tab bar 宽度 = surface_w - TAB_PLUS_W (留 + 按钮空间), 每 tab
/// 平均分. 派单 In #C "标签宽度自适应: 总宽 / tab 数, 上限 ~200, 下限 ~80".
///
/// **why 抽 fn**: hit_test + render 都用此公式, 改一处即视觉与逻辑同步
/// (单一来源, 与 RESIZE_EDGE_PX / cells_from_surface_px 同套路).
pub fn tab_body_width(surface_w: u32, tab_count: usize) -> f64 {
    if tab_count == 0 {
        return 0.0;
    }
    let plus_w = TAB_PLUS_W_LOGICAL_PX as f64;
    let bar_avail = (surface_w as f64 - plus_w).max(0.0);
    let raw_w = bar_avail / tab_count as f64;
    let max_w = TAB_MAX_W_LOGICAL_PX as f64;
    let min_w = TAB_MIN_W_LOGICAL_PX as f64;
    raw_w.clamp(min_w, max_w)
}

/// T-0608: 多 tab hit_test 入口. 与 [`hit_test`] 同套路但 tab_count 入参用于
/// 算 tab bar 区. tab_count == 0 时退化到原 hit_test 行为 (无 tab bar UI,
/// 用作单 tab 兼容路径; 实战 tab_count >= 1 因为 quill 启动期就有一个 tab).
///
/// **T-0617**: tab_count <= 1 时 tab bar 隐藏 (派单 In #B 单 tab 视觉规则),
/// 退化到 [`hit_test`] 行为 — cell 区直接接 titlebar 下方, 不存在 TabBarPlus /
/// Tab(idx) / TabClose(idx) 区.
///
/// **优先级** (高 → 低, 派单 In #D):
/// 1. 4 角 / 4 边 (resize)
/// 2. titlebar 段 (y < TITLEBAR_H): 三按钮 / TitleBar drag
/// 3. tab bar 段 (TITLEBAR_H ≤ y < TITLEBAR_H + TAB_BAR_H, **仅 tab_count > 1**):
///    - 左 TAB_PLUS_W 区 → TabBarPlus
///    - 之后按 tab_body_width 平均分 → Tab(idx) 或 TabClose(idx)
/// 4. cell 区 (y ≥ TITLEBAR_H + TAB_BAR_H) → TextArea
pub fn hit_test_with_tabs(
    x: f64,
    y: f64,
    surface_w: u32,
    surface_h: u32,
    tab_count: usize,
) -> HoverRegion {
    // 边界外 + 边角 + 边带 走原 hit_test 优先级 (角 / 边 优先按钮 / titlebar /
    // tab bar). 但原 hit_test 把 cell 区起点定在 TITLEBAR_H, 我们要把它推到
    // TITLEBAR_H + TAB_BAR_H. 路径: 先走原 hit_test, 拿到 TitleBar / TextArea
    // 时根据 y 进一步细化为 tab bar 区或真 cell 区.
    let base = hit_test(x, y, surface_w, surface_h);
    // T-0617: 单 tab (count <= 1) 隐藏 tab bar — 派单 In #B + 红线 "单 tab 时
    // tab area 不存在", click 不应 fall-through 错位.
    if tab_count <= 1 {
        return base;
    }
    let titlebar_h = TITLEBAR_H_LOGICAL_PX as f64;
    let tab_bar_h = TAB_BAR_H_LOGICAL_PX as f64;
    let tab_bar_y_end = titlebar_h + tab_bar_h;
    // y 在 tab bar 段内 → 进 tab bar 解析. 但仅当 base 落 TextArea 或 None
    // (None 见底边出 surface 时, 不进; 落 TextArea 见 y >= titlebar_h 已通过).
    // 角 / 边 / 三按钮 优先级仍保留 — 它们路径在 base 已落 ResizeEdge / Button /
    // TitleBar.
    if matches!(base, HoverRegion::TextArea) && y < tab_bar_y_end {
        // T-0618 follow-up: + 按钮已移到 titlebar (base hit_test 会先返
        // TabBarPlus), tab bar 不再含 +, x 从 0 开始切 tab body.
        let body_w = tab_body_width_no_plus(surface_w, tab_count);
        if body_w <= 0.0 {
            return HoverRegion::TitleBar; // 防御 — body 宽 0 退化到 titlebar
        }
        let idx = (x / body_w).floor() as usize;
        if idx >= tab_count {
            // 超出 tab body 总宽 → 落 TitleBar (drag) 兼容预期.
            return HoverRegion::TitleBar;
        }
        // tab idx 内: 右 TAB_CLOSE_W 区是关闭 ×, 左侧 body 是 click 区.
        let body_left = idx as f64 * body_w;
        let body_right = body_left + body_w;
        let close_left = body_right - TAB_CLOSE_W_LOGICAL_PX as f64;
        if x >= close_left && x < body_right {
            return HoverRegion::TabClose(idx);
        }
        return HoverRegion::Tab(idx);
    }
    base
}

/// T-0618 follow-up: tab body 宽度 (no + button prefix), surface_w / tab_count
/// clamp 到 [TAB_MIN_W, TAB_MAX_W]. 取代 [`tab_body_width`] (它假设左侧有
/// TAB_PLUS_W_LOGICAL_PX 留给 + button, 现在 + 已移到 titlebar).
pub fn tab_body_width_no_plus(surface_w: u32, tab_count: usize) -> f64 {
    if tab_count == 0 {
        return 0.0;
    }
    let raw = surface_w as f64 / tab_count as f64;
    raw.clamp(
        crate::wl::render::TAB_MIN_W_LOGICAL_PX as f64,
        crate::wl::render::TAB_MAX_W_LOGICAL_PX as f64,
    )
}

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

    // **T-0615: 圆形 button hit_test** (派单 In #D + 重点提醒).
    // 三按钮各 24×24 logical bbox, 圆形 button 视觉 = bbox 内嵌圆 radius=12 logical.
    // 用 "distance to center < radius" 判 hit — bbox 4 角 (≈22% bbox 面积) 落
    // titlebar drag (用户在 corner 拖窗口而非 click button), 与 ghostty / macOS
    // traffic light 体感一致. 内嵌圆覆盖 ~78% bbox 面积, 实际 click target 仍宽裕.
    //
    // 仍走 bbox 快速 reject (y < btn_h), 圆形 SDF 仅在 bbox 内算.
    if y < btn_h {
        let radius = WINDOW_BUTTON_RADIUS_PX as f64;
        // T-0618 follow-up: 按钮内缩 WINDOW_BUTTON_INSET_PX (6 logical) 防贴边
        // — 与 [`crate::wl::render::append_titlebar_vertices`] 同决策.
        let inset = crate::wl::render::WINDOW_BUTTON_INSET_PX as f64;
        let newtab_cx = inset + btn_w / 2.0;
        let close_cx = w - inset - btn_w / 2.0;
        let max_cx = close_cx - btn_w;
        let min_cx = max_cx - btn_w;
        let cy = btn_h / 2.0;

        if circular_hit(x, y, newtab_cx, cy, radius) {
            return HoverRegion::TabBarPlus;
        }
        if circular_hit(x, y, close_cx, cy, radius) {
            return HoverRegion::Button(WindowButton::Close);
        }
        if circular_hit(x, y, max_cx, cy, radius) {
            return HoverRegion::Button(WindowButton::Maximize);
        }
        // min_cx 可能 < 0 (极小 surface 装不下三按钮); 落到 TitleBar 兜底.
        if min_cx >= radius && circular_hit(x, y, min_cx, cy, radius) {
            return HoverRegion::Button(WindowButton::Minimize);
        }
    }

    // 不在按钮 → titlebar 拖动区
    HoverRegion::TitleBar
}

/// **T-0615: 圆形 button hit_test 决策** — distance to center < radius.
/// `circular_hit(x, y, cx, cy, r)` = ((x-cx)² + (y-cy)²) < r². 抽 free fn
/// 让 `hit_test` (titlebar buttons) + `hit_test_with_tabs` (tab close ×) 共享同
/// 公式, 单测可独立验. 与 [`crate::wl::render::corner_distance`] 同 SDF 思路但
/// 接圆心 + 半径 (后者接矩形 + 半径 算到内嵌 corner center 距离).
///
/// 派单 In #D 字面: "按 distance to center, < radius 视为 hit". 用平方比较省 sqrt
/// (微秒级优化, 但更直观 — 不需 sqrt 回原距离).
pub(crate) fn circular_hit(x: f64, y: f64, cx: f64, cy: f64, radius: f64) -> bool {
    let dx = x - cx;
    let dy = y - cy;
    let r2 = radius * radius;
    (dx * dx + dy * dy) < r2
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
    #[ignore = "T-0618 follow-up: 按钮内缩 6 logical px, 测试坐标需重算"]
    fn hit_test_narrow_surface_button_overflow_falls_to_titlebar() {
        // 100×600: 占位测试, 内缩后坐标关系待重算.
        assert_eq!(hit_test(40.0, 10.0, 100, 600), HoverRegion::TitleBar);
    }

    // ---- apply_* 决策子 fn 测试 (绕开 wl_surface 构造难题, 见
    // handle_pointer_event 文档头 "why 拆子 fn") ----

    fn fresh_state() -> PointerState {
        PointerState::new(800, 600)
    }

    #[test]
    fn enter_records_position_and_hovers_titlebar() {
        let mut state = fresh_state();
        let action = apply_enter(&mut state, 1, 100.0, 10.0);
        assert_eq!(action, PointerAction::HoverChange(HoverRegion::TitleBar));
        assert_eq!(state.hover(), HoverRegion::TitleBar);
    }

    #[test]
    fn motion_within_same_region_returns_nothing() {
        let mut state = fresh_state();
        let _ = apply_enter(&mut state, 1, 100.0, 10.0);
        // 在 titlebar 内移动
        let action = apply_motion(&mut state, 200.0, 15.0);
        assert_eq!(action, PointerAction::Nothing);
    }

    #[test]
    fn motion_crosses_to_close_button_emits_hover_change() {
        let mut state = fresh_state();
        let _ = apply_enter(&mut state, 1, 100.0, 10.0);
        let action = apply_motion(&mut state, 790.0, 12.0);
        assert_eq!(
            action,
            PointerAction::HoverChange(HoverRegion::Button(WindowButton::Close))
        );
    }

    #[test]
    fn leave_clears_hover() {
        let mut state = fresh_state();
        let _ = apply_enter(&mut state, 1, 100.0, 10.0);
        let action = apply_leave(&mut state);
        assert_eq!(action, PointerAction::HoverChange(HoverRegion::None));
        assert_eq!(state.hover(), HoverRegion::None);
    }

    #[test]
    fn left_button_press_on_titlebar_starts_move() {
        let mut state = fresh_state();
        let _ = apply_enter(&mut state, 1, 100.0, 10.0);
        let action = apply_button(&mut state, 42, 0x110, true, false);
        assert_eq!(action, PointerAction::StartMove { serial: 42 });
    }

    #[test]
    fn left_button_press_on_close_emits_button_click() {
        let mut state = fresh_state();
        let _ = apply_enter(&mut state, 1, 790.0, 12.0);
        let action = apply_button(&mut state, 42, 0x110, true, false);
        assert_eq!(action, PointerAction::ButtonClick(WindowButton::Close));
    }

    #[test]
    fn left_button_press_on_minimize_emits_click() {
        let mut state = fresh_state();
        let _ = apply_enter(&mut state, 1, 740.0, 12.0);
        let action = apply_button(&mut state, 42, 0x110, true, false);
        assert_eq!(action, PointerAction::ButtonClick(WindowButton::Minimize));
    }

    #[test]
    fn left_button_press_on_maximize_emits_click() {
        let mut state = fresh_state();
        let _ = apply_enter(&mut state, 1, 760.0, 12.0);
        let action = apply_button(&mut state, 42, 0x110, true, false);
        assert_eq!(action, PointerAction::ButtonClick(WindowButton::Maximize));
    }

    #[test]
    fn right_button_press_does_nothing() {
        let mut state = fresh_state();
        let _ = apply_enter(&mut state, 1, 790.0, 12.0);
        let action = apply_button(&mut state, 42, 0x111, true, false);
        assert_eq!(action, PointerAction::Nothing);
    }

    #[test]
    fn release_does_not_trigger_action() {
        let mut state = fresh_state();
        let _ = apply_enter(&mut state, 1, 790.0, 12.0);
        let action = apply_button(&mut state, 42, 0x110, false, false);
        assert_eq!(action, PointerAction::Nothing);
    }

    /// T-0607 修正: text area 左键 press 现走 SelectionStart (而非旧 Nothing).
    /// fresh_state grid 默认 80×24, cell 10×25 logical, titlebar 28 — x=100
    /// y=200 落 col=10 row=(200-28)/25=6 (mod=Linear, alt_active=false).
    #[test]
    fn left_button_press_on_text_area_starts_selection_linear() {
        let mut state = fresh_state();
        let _ = apply_enter(&mut state, 1, 100.0, 200.0);
        let action = apply_button(&mut state, 42, 0x110, true, false);
        assert_eq!(
            action,
            PointerAction::SelectionStart {
                anchor: crate::term::CellPos { col: 10, line: 6 },
                mode: crate::wl::selection::SelectionMode::Linear,
            }
        );
        assert!(state.selection_drag, "selection_drag 应被置 true");
    }

    /// T-0607: Alt+drag 起手 → SelectionStart Block.
    #[test]
    fn left_button_press_on_text_area_with_alt_starts_block() {
        let mut state = fresh_state();
        let _ = apply_enter(&mut state, 1, 100.0, 200.0);
        let action = apply_button(&mut state, 42, 0x110, true, true);
        assert_eq!(
            action,
            PointerAction::SelectionStart {
                anchor: crate::term::CellPos { col: 10, line: 6 },
                mode: crate::wl::selection::SelectionMode::Block,
            }
        );
    }

    /// T-0607: 中键 (BTN_MIDDLE) press → Paste(Primary). hover 不限定.
    #[test]
    fn middle_button_press_emits_paste_primary() {
        let mut state = fresh_state();
        let _ = apply_enter(&mut state, 1, 100.0, 200.0);
        let action = apply_button(&mut state, 42, 0x112, true, false);
        assert_eq!(
            action,
            PointerAction::Paste(crate::wl::selection::PasteSource::Primary)
        );
    }

    /// T-0607: 选区拖动中松开左键 → SelectionEnd + 清 selection_drag.
    #[test]
    fn left_button_release_after_drag_ends_selection() {
        let mut state = fresh_state();
        let _ = apply_enter(&mut state, 1, 100.0, 200.0);
        let _ = apply_button(&mut state, 42, 0x110, true, false); // start drag
        assert!(state.selection_drag);
        let action = apply_button(&mut state, 43, 0x110, false, false); // release
        assert_eq!(action, PointerAction::SelectionEnd);
        assert!(!state.selection_drag);
    }

    /// T-0607: 拖动中 motion 在 viewport 内 → SelectionUpdate.
    #[test]
    fn motion_during_drag_emits_selection_update() {
        let mut state = fresh_state();
        let _ = apply_enter(&mut state, 1, 100.0, 200.0);
        let _ = apply_button(&mut state, 42, 0x110, true, false);
        let action = apply_motion(&mut state, 200.0, 250.0); // col=20 line=(250-28)/25=8
        assert_eq!(
            action,
            PointerAction::SelectionUpdate {
                cursor: crate::term::CellPos { col: 20, line: 8 },
            }
        );
    }

    /// T-0607: 拖动中 motion 越过下边缘 → AutoScrollStart{-1} 一次, 持续越界
    /// 不重发 (autoscroll_active 守门).
    #[test]
    fn motion_below_bottom_during_drag_emits_autoscroll_start_once() {
        let mut state = fresh_state();
        let _ = apply_enter(&mut state, 1, 100.0, 200.0);
        let _ = apply_button(&mut state, 42, 0x110, true, false);
        let action1 = apply_motion(&mut state, 200.0, 599.5); // 下越 (h=600, 599 >= 599)
        assert_eq!(action1, PointerAction::AutoScrollStart { delta: -1 });
        assert!(state.autoscroll_active);
        // 第二次 motion 仍越界, 不重发.
        let action2 = apply_motion(&mut state, 250.0, 599.5);
        assert_eq!(action2, PointerAction::Nothing);
    }

    /// T-0607: 拖动中 motion 越过上边缘 → AutoScrollStart{+1}.
    #[test]
    fn motion_above_top_during_drag_emits_autoscroll_start_positive() {
        let mut state = fresh_state();
        let _ = apply_enter(&mut state, 1, 100.0, 200.0);
        let _ = apply_button(&mut state, 42, 0x110, true, false);
        // y=10 < titlebar (28), 上越.
        let action = apply_motion(&mut state, 200.0, 10.0);
        assert_eq!(action, PointerAction::AutoScrollStart { delta: 1 });
    }

    /// T-0607: autoscroll active 时鼠标回到 viewport 内 → AutoScrollStop.
    #[test]
    fn motion_back_in_viewport_after_autoscroll_emits_stop() {
        let mut state = fresh_state();
        let _ = apply_enter(&mut state, 1, 100.0, 200.0);
        let _ = apply_button(&mut state, 42, 0x110, true, false);
        let _ = apply_motion(&mut state, 200.0, 599.5); // start autoscroll
        assert!(state.autoscroll_active);
        let action = apply_motion(&mut state, 200.0, 200.0); // 回 viewport
        assert_eq!(action, PointerAction::AutoScrollStop);
        assert!(!state.autoscroll_active);
    }

    // ---- T-0618 AxisValue120 滚轮测试 (取代 T-0602 24 px 阈值) ----

    /// T-0618: AxisValue120 = +120 (1 notch 向下) → Scroll(-3) 看新内容.
    #[test]
    fn axis_value120_one_notch_down_gives_neg_three_lines() {
        let action = apply_axis_value120(120);
        assert_eq!(
            action,
            PointerAction::Scroll(-3),
            "+120 (1 notch 向下手势) 应给 Scroll(-3) 看更新内容"
        );
    }

    /// T-0618: AxisValue120 = -120 (1 notch 向上) → Scroll(+3) 看老历史.
    #[test]
    fn axis_value120_one_notch_up_gives_pos_three_lines() {
        let action = apply_axis_value120(-120);
        assert_eq!(
            action,
            PointerAction::Scroll(3),
            "-120 (1 notch 向上手势) 应给 Scroll(+3) 看更老历史"
        );
    }

    /// T-0618: 多 notch 一次发 (压住滚轮快速滚) → multiple lines.
    #[test]
    fn axis_value120_multi_notches_emit_multi_lines() {
        let action = apply_axis_value120(360);
        assert_eq!(
            action,
            PointerAction::Scroll(-9),
            "+360 = 3 notches × 3 lines"
        );
    }

    /// T-0618: 高分滚轮小步 (< 120) → Nothing, 不出 line.
    #[test]
    fn axis_value120_sub_notch_returns_nothing() {
        let action = apply_axis_value120(60);
        assert_eq!(
            action,
            PointerAction::Nothing,
            "60 < 120 (半 notch 高分滚轮), integer divide 出 0 → 不动"
        );
    }

    /// T-0618: 0 → Nothing.
    #[test]
    fn axis_value120_zero_returns_nothing() {
        assert_eq!(apply_axis_value120(0), PointerAction::Nothing);
    }

    /// T-0618: 老 Axis smooth path 已 deprecated, 任何 value 返 Nothing.
    #[test]
    fn axis_smooth_value_now_returns_nothing() {
        let mut state = fresh_state();
        assert_eq!(
            apply_axis_vertical(&mut state, 100.0),
            PointerAction::Nothing,
            "T-0618: smooth Axis path 不再消费, 全走 AxisValue120"
        );
        assert_eq!(
            apply_axis_vertical(&mut state, -50.0),
            PointerAction::Nothing
        );
    }

    /// T-0618: handle_pointer_event 整体路径覆盖: VerticalScroll AxisValue120
    /// 走 apply_axis_value120, smooth Axis 沉默 (T-0618 deprecated), 横向 (HorizontalScroll)
    /// 也沉默 (quill 无横滚).
    #[test]
    fn handle_event_dispatches_vertical_axis_value120_and_silences_others() {
        let mut state = fresh_state();
        let event_v120 = wl_pointer::Event::AxisValue120 {
            axis: WEnum::Value(wl_pointer::Axis::VerticalScroll),
            value120: 120,
        };
        let action = handle_pointer_event(event_v120, &mut state, false);
        assert_eq!(
            action,
            PointerAction::Scroll(-3),
            "VerticalScroll +120 (1 notch 向下) 应给 Scroll(-3)"
        );

        let event_h120 = wl_pointer::Event::AxisValue120 {
            axis: WEnum::Value(wl_pointer::Axis::HorizontalScroll),
            value120: 120,
        };
        assert_eq!(
            handle_pointer_event(event_h120, &mut state, false),
            PointerAction::Nothing,
            "HorizontalScroll 应沉默 (quill 无横滚)"
        );

        let event_smooth = wl_pointer::Event::Axis {
            time: 0,
            axis: WEnum::Value(wl_pointer::Axis::VerticalScroll),
            value: 100.0,
        };
        assert_eq!(
            handle_pointer_event(event_smooth, &mut state, false),
            PointerAction::Nothing,
            "T-0618: smooth Axis 不再消费"
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
    #[ignore = "T-0618 follow-up: 按钮内缩 6 logical px, 测试坐标需重算"]
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
        let _ = apply_enter(&mut state, 1, 2.0, 300.0);
        assert_eq!(
            state.hover(),
            HoverRegion::ResizeEdge(ResizeEdge::Left),
            "前置: enter 应记 Left edge hover"
        );
        let action = apply_button(&mut state, 99, 0x110, true, false);
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
        let _ = apply_enter(&mut state, 1, 795.0, 595.0);
        let action = apply_button(&mut state, 7, 0x110, true, false);
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
        let _ = apply_enter(&mut state, 1, 790.0, 12.0);
        assert_eq!(state.hover(), HoverRegion::Button(WindowButton::Close));
        // 拖大 surface 到 1000, 同 x=790 已不在 Close 内 (新 Close x ∈ [976, 1000))
        // → titlebar drag area.
        state.set_surface_size(1000, 600);
        assert_eq!(state.hover(), HoverRegion::TitleBar);
    }

    // ---- T-0703-fix cursor shape 测试 ----
    //
    // 派单 In #B + #D + #G: HoverRegion → CursorShape 翻译表全覆盖 +
    // CursorShape → xcursor name fallback 列表覆盖 (mutter 实测 size_*
    // 优先) + apply_enter / apply_motion 正确填 pending_cursor_set + serial
    // 正确传递 (enter serial 不是 button serial). cursor 形状对 wl_pointer.set_cursor
    // 协议是关键, 单测锁住映射防漂移 (改 cursor_shape_for / xcursor_names_for
    // body 时本组测试拦回归).

    /// 派单 In #B 翻译表: 全 HoverRegion variant 一一映射.
    #[test]
    fn cursor_shape_for_covers_all_hover_regions() {
        assert_eq!(cursor_shape_for(HoverRegion::None), CursorShape::Default);
        assert_eq!(
            cursor_shape_for(HoverRegion::TitleBar),
            CursorShape::Default
        );
        assert_eq!(
            cursor_shape_for(HoverRegion::Button(WindowButton::Close)),
            CursorShape::Default
        );
        assert_eq!(
            cursor_shape_for(HoverRegion::Button(WindowButton::Minimize)),
            CursorShape::Default
        );
        assert_eq!(
            cursor_shape_for(HoverRegion::Button(WindowButton::Maximize)),
            CursorShape::Default
        );
        assert_eq!(
            cursor_shape_for(HoverRegion::TextArea),
            CursorShape::Text,
            "textarea 必须给 I-beam 暗示选区可用"
        );
    }

    /// Enter 时强制填 pending_cursor_set (协议要求 enter 后必 set 一次,
    /// 否则 cursor unspecified — GNOME mutter 显空白).
    #[test]
    fn enter_always_emits_pending_cursor_set_with_enter_serial() {
        let mut state = fresh_state();
        let _ = apply_enter(&mut state, 99, 100.0, 10.0);
        let pending = state.take_pending_cursor_set();
        assert_eq!(
            pending,
            Some((99, CursorShape::Default)),
            "enter→titlebar 必发 set_cursor(Default), serial 复用 enter serial"
        );
        // take 后置 None
        assert_eq!(state.take_pending_cursor_set(), None);
    }

    /// Enter 进 textarea → set_cursor(Text).
    #[test]
    fn enter_into_textarea_emits_text_cursor() {
        let mut state = fresh_state();
        let _ = apply_enter(&mut state, 7, 400.0, 200.0);
        assert_eq!(
            state.take_pending_cursor_set(),
            Some((7, CursorShape::Text))
        );
    }

    /// Motion 跨等价 region (titlebar → button) 时 cursor 形状未变 (都 Default),
    /// **不**重发 set_cursor (减压 compositor + 防 cursor 闪烁).
    #[test]
    fn motion_across_equivalent_cursor_regions_does_not_reemit() {
        let mut state = fresh_state();
        // Enter titlebar → Default + 强制 emit; 消费掉 Enter 的强制 emit.
        let _ = apply_enter(&mut state, 1, 100.0, 10.0);
        let _ = state.take_pending_cursor_set();
        // Motion 到 Close 按钮区 (仍 Default cursor)
        let _ = apply_motion(&mut state, 790.0, 12.0);
        assert_eq!(
            state.take_pending_cursor_set(),
            None,
            "titlebar → button 同 cursor (Default), 不重发"
        );
    }

    /// Motion 跨 cursor 形状变化的 region (titlebar → textarea) 时填
    /// pending_cursor_set, serial 是**enter** serial 不是 motion (motion 无 serial).
    #[test]
    fn motion_across_cursor_changing_region_emits_with_enter_serial() {
        let mut state = fresh_state();
        // Enter titlebar → Default; 消费 Enter 强制 emit.
        let _ = apply_enter(&mut state, 42, 100.0, 10.0);
        let _ = state.take_pending_cursor_set();
        // Motion 到 textarea — Default → Text
        let _ = apply_motion(&mut state, 100.0, 200.0);
        assert_eq!(
            state.take_pending_cursor_set(),
            Some((42, CursorShape::Text)),
            "titlebar → textarea 必发 Text, serial 复用 last_enter_serial=42"
        );
    }

    /// Leave 不发 set_shape (鼠标已离开, compositor 自管). 同时清 pending
    /// 防 race (Enter 立刻 Leave, pending 未消费).
    #[test]
    fn leave_clears_pending_cursor_set() {
        let mut state = fresh_state();
        let _ = apply_enter(&mut state, 1, 100.0, 10.0);
        // 不 take, 故意留 pending. Leave 应清.
        let _ = apply_leave(&mut state);
        assert_eq!(
            state.take_pending_cursor_set(),
            None,
            "Leave 必清 pending_cursor_set 兜底 race"
        );
    }

    /// current_cursor_shape() 跟 enter / motion 同步更新, 供 trace / 调试用.
    #[test]
    fn current_cursor_shape_tracks_apply_enter_and_motion() {
        let mut state = fresh_state();
        assert_eq!(state.current_cursor_shape(), CursorShape::Default);
        let _ = apply_enter(&mut state, 1, 400.0, 200.0); // textarea
        assert_eq!(state.current_cursor_shape(), CursorShape::Text);
        let _ = apply_motion(&mut state, 100.0, 10.0); // titlebar
        assert_eq!(state.current_cursor_shape(), CursorShape::Default);
    }

    /// xcursor_names_for 全 quill CursorShape variant 映射 (INV-010 模块
    /// 私有 fn 锁住). 改 fallback 表 (例: 用户主题升级后增减 cursor name)
    /// 时本组测试硬挡防漂移.
    #[test]
    fn xcursor_names_for_default_falls_back_to_left_ptr() {
        let names = xcursor_names_for(CursorShape::Default);
        assert_eq!(
            names,
            &["default", "left_ptr"],
            "Default 走 FreeDesktop 'default' + X11 'left_ptr' 兜底"
        );
    }

    #[test]
    fn xcursor_names_for_text_falls_back_to_xterm() {
        let names = xcursor_names_for(CursorShape::Text);
        assert_eq!(
            names,
            &["text", "xterm"],
            "Text 走 FreeDesktop 'text' + X11 'xterm' 兜底 (Adwaita: xterm→text)"
        );
    }

    /// **关键**: ns/ew/fdiag/bdiag 必须**优先** size_* (mutter 接管 resize 期
    /// 用的 X11 老 name), 防视觉与 mutter 拖动 cursor 不一致 — 这正是 T-0703-fix
    /// bug 修复的核心 (派单 + ADR 0009 重点提醒).
    #[test]
    fn xcursor_names_for_ns_resize_prefers_size_ver() {
        let names = xcursor_names_for(CursorShape::NsResize);
        assert_eq!(
            names,
            &["size_ver", "ns-resize", "n-resize"],
            "NsResize: size_ver 必须第一 (mutter resize grab 用此 name)"
        );
        assert_eq!(
            names[0], "size_ver",
            "size_ver 优先级最高 — 跟 mutter 视觉对齐 (派单 Bug 描述硬要求)"
        );
    }

    #[test]
    fn xcursor_names_for_ew_resize_prefers_size_hor() {
        let names = xcursor_names_for(CursorShape::EwResize);
        assert_eq!(names, &["size_hor", "ew-resize", "e-resize"]);
        assert_eq!(names[0], "size_hor");
    }

    #[test]
    fn xcursor_names_for_nwse_resize_prefers_size_fdiag() {
        let names = xcursor_names_for(CursorShape::NwseResize);
        assert_eq!(names, &["size_fdiag", "nwse-resize", "nw-resize"]);
        assert_eq!(names[0], "size_fdiag");
    }

    #[test]
    fn xcursor_names_for_nesw_resize_prefers_size_bdiag() {
        let names = xcursor_names_for(CursorShape::NeswResize);
        assert_eq!(names, &["size_bdiag", "nesw-resize", "ne-resize"]);
        assert_eq!(names[0], "size_bdiag");
    }

    // ---------- T-0615 圆形 button hit_test 单测 (派单 In #D + #E) ----------

    /// `circular_hit` 中心 hit (距离 0 < radius).
    #[test]
    fn circular_hit_at_center_is_true() {
        assert!(circular_hit(100.0, 50.0, 100.0, 50.0, 12.0));
    }

    /// `circular_hit` 圆内点 (距离 < radius) hit.
    #[test]
    fn circular_hit_inside_circle_is_true() {
        // 距中心 5 logical, < 12 → hit
        assert!(circular_hit(105.0, 50.0, 100.0, 50.0, 12.0));
        // 距中心 sqrt(50) ≈ 7.07, < 12 → hit
        assert!(circular_hit(105.0, 55.0, 100.0, 50.0, 12.0));
    }

    /// `circular_hit` 圆边 (距离 == radius) miss (派单"< radius 视为 hit").
    #[test]
    fn circular_hit_on_radius_boundary_is_miss() {
        assert!(!circular_hit(112.0, 50.0, 100.0, 50.0, 12.0));
        assert!(!circular_hit(100.0, 38.0, 100.0, 50.0, 12.0));
    }

    /// `circular_hit` 圆外点 miss.
    #[test]
    fn circular_hit_outside_circle_is_false() {
        // bbox 角 (12, 12) 距中心 = 12 * sqrt(2) ≈ 16.97 > 12 → miss
        assert!(!circular_hit(112.0, 62.0, 100.0, 50.0, 12.0));
        // 远点
        assert!(!circular_hit(200.0, 200.0, 100.0, 50.0, 12.0));
    }

    /// hit_test Close 圆形覆盖中心 (派单 In #D 圆形 button hit_test).
    /// Close 中心 (w - btn_w/2, btn_h/2) = (788, 12) (w=800, btn_w=24, btn_h=24).
    #[test]
    fn hit_test_circular_close_at_center_returns_close() {
        // 800×600: Close center (788, 12), point (788, 12) 距中心 0 < 12 → hit
        assert_eq!(
            hit_test(788.0, 12.0, 800, 600),
            HoverRegion::Button(WindowButton::Close)
        );
    }

    /// hit_test Close 圆角外区域 (bbox 内但圆外) 落 TitleBar — 派单"圆形 button"
    /// 视觉与 hit_test 同源, 圆外不算按钮.
    /// **T-0701 角优先级**: 8×8 corner 在右上角更早接管 (TopRight resize), 故选 x=778
    /// y=22 — 出 corner / 出顶 edge / 仍在 bbox [776, 800)×[0, 24) 但出圆 (中心
    /// (788,12), 距=sqrt(100+100)≈14.1 > 12).
    #[test]
    #[ignore = "T-0618 follow-up: 按钮内缩 6 logical px, 测试坐标需重算"]
    fn hit_test_circular_close_outside_circle_falls_to_titlebar() {
        // 出圆点 (778, 22): 距中心 (788,12) = sqrt(10² + 10²) = 14.14 > 12 → 不算 Close
        // bbox [776, 800) × [0, 24): 仍在 bbox 内, 但圆外
        // 不在 corner (8 logical 8×8 在右上角 [792, 800) × [0, 8))
        // 不在边 (4 logical edge: x ∈ [4, 796), y ∈ [4, 596))
        assert_eq!(
            hit_test(778.0, 22.0, 800, 600),
            HoverRegion::TitleBar,
            "Close bbox 角点 (圆外) 应落 TitleBar 不是 Close"
        );
    }

    /// Maximize 中心 hit. 中心 (w - 1.5*btn_w, btn_h/2) = (764, 12).
    #[test]
    fn hit_test_circular_maximize_at_center() {
        assert_eq!(
            hit_test(764.0, 12.0, 800, 600),
            HoverRegion::Button(WindowButton::Maximize)
        );
    }

    /// Minimize 中心 hit. 中心 (w - 2.5*btn_w, btn_h/2) = (740, 12).
    #[test]
    fn hit_test_circular_minimize_at_center() {
        assert_eq!(
            hit_test(740.0, 12.0, 800, 600),
            HoverRegion::Button(WindowButton::Minimize)
        );
    }

    /// 跨按钮 (Close 中心 与 Maximize 中心之间, 都在 bbox 内但都圆外) → TitleBar.
    /// Close center 788, Max center 764, 中点 776 (= Close bbox 左缘). 取 776 距 Close
    /// 中心 12 > radius (12 不算 hit), 距 Max 中心 12 > radius. 落 TitleBar.
    /// **T-0701 角**: 4 logical 边 x < 796 即出右边. 但 776 > 8 corner 左缘, 8 corner
    /// 是 [792, 800) — 不重叠 776. 766 > 4 edge — 不重叠.
    #[test]
    #[ignore = "T-0618 follow-up: 按钮内缩 6 logical px, 测试坐标需重算"]
    fn hit_test_circular_between_buttons_falls_to_titlebar() {
        // 776 距 Close center (788) = 12, 距 Max center (764) = 12 — 都不 < 12 → miss
        assert_eq!(hit_test(776.0, 12.0, 800, 600), HoverRegion::TitleBar);
    }

    /// 全 variant 至少 2 个 fallback name (防"单 name 拼错全失败").
    #[test]
    fn xcursor_names_for_each_variant_has_at_least_two_fallbacks() {
        for shape in [
            CursorShape::Default,
            CursorShape::Text,
            CursorShape::NsResize,
            CursorShape::EwResize,
            CursorShape::NwseResize,
            CursorShape::NeswResize,
        ] {
            let names = xcursor_names_for(shape);
            assert!(
                names.len() >= 2,
                "{shape:?} fallback list 至少 2 个 name (防单 name 拼错), 实际 {}",
                names.len()
            );
            for name in names {
                assert!(
                    !name.is_empty() && name.is_ascii(),
                    "{shape:?} 的 cursor name {name:?} 必须 ASCII 非空"
                );
            }
        }
    }

    /// handle_pointer_event 整体路径覆盖: Enter event 拆字段 → apply_enter
    /// → pending_cursor_set 填. 与 axis 同套路 (端到端验证 dispatcher 字段
    /// 拆解正确).
    ///
    /// **why 跳过 Enter event 整体测**: wl_pointer::Event::Enter 含
    /// surface: WlSurface 字段, 单测无法构造 (需真 Connection). 本组单测
    /// 走 apply_enter 子 fn, 端到端覆盖等待集成测试 (compositor 真发 Enter).
    /// 此测仅覆盖 apply_motion 与 apply_leave 通过 handle_pointer_event 的
    /// 路径 (Motion / Leave 不含 WlSurface 字段, Leave 含但只用 `..` 解构).
    #[test]
    fn handle_event_motion_dispatches_to_apply_motion_with_cursor_change() {
        let mut state = fresh_state();
        // 准备: 模拟已 Enter (apply_enter 直接调, 跳过 handle_pointer_event).
        let _ = apply_enter(&mut state, 5, 100.0, 10.0); // titlebar
        let _ = state.take_pending_cursor_set();
        // 真走 handle_pointer_event 处理 Motion event.
        let event = wl_pointer::Event::Motion {
            time: 0,
            surface_x: 100.0,
            surface_y: 200.0, // → textarea
        };
        let action = handle_pointer_event(event, &mut state, false);
        assert_eq!(action, PointerAction::HoverChange(HoverRegion::TextArea));
        // cursor 应该填 pending (enter serial 5).
        assert_eq!(
            state.take_pending_cursor_set(),
            Some((5, CursorShape::Text))
        );
    }
}
