//! Wayland 客户端封装。本阶段只拉起 xdg_toplevel 窗口并用 wgpu 画一块清屏背景
//! (T-0102)。对外只暴露 [`run_window`] 一个入口,隐藏 SCTK / wayland-client /
//! wgpu 的全部原始类型。后续 ticket 接 calloop / resize。

mod keyboard;
mod pointer;
mod render;
mod selection;
mod window;

pub use window::run_window;

// T-0107 为让 headless 测试能直接单测状态转移,把纯逻辑状态机
// ([`WindowCore`] + [`WindowEvent`] + [`handle_event`])公开,内部活动回调
// 也走同一条路径(见 `window.rs::State::configure`)。
pub use window::{handle_event, WindowAction, WindowCore, WindowEvent};

// T-0404: HiDPI 整数缩放常数, text 子系统 (shape_line font_size) 与 render
// (Renderer::resize / cell px) 共享单一来源, 改一处即可。
// `mod render` 自身保持私有 (INV-010: wgpu 类型不出本模块), 仅 const u32 通过
// 此 re-export 以 `crate::wl::HIDPI_SCALE` 路径暴露给 text 模块。
pub use render::HIDPI_SCALE;

// T-0801: cell 像素宽度常数, text 子系统 (force_cjk_double_advance) 与 render
// (draw_frame cell px) 共享单一来源 — CJK 字形强制双宽 advance = 2 × CELL_W_PX,
// 改一处即可。沿袭 HIDPI_SCALE 同款 re-export 路径 (mod render 私有, 仅 const f32
// 暴露)。INV-010 守: 仅 quill 自有 const, 不漏 wgpu 类型。
pub use render::CELL_W_PX;

// T-0408 离屏渲染入口 (offscreen render → RGBA8 Vec<u8>)。`src/main.rs`
// `--headless-screenshot` CLI flag + `tests/headless_screenshot.rs` 集成测试
// 都走此 fn。入参 / 出参全 quill 自有类型, 不漏 wgpu 内部 (INV-010 类型隔离),
// 详见 `render.rs::render_headless` 文档头。
pub use render::render_headless;

// T-0505: PreeditOverlay 入参 (集成测试 tests/ime_preedit_render.rs 用), quill
// 自有 struct (INV-010, 不漏 wayland-protocols 类型). PREEDIT_UNDERLINE_PX
// 常数同步 re-export 给测试 / Phase 6 计算用。
pub use render::{PreeditOverlay, PREEDIT_UNDERLINE_PX};

// T-0601: 光标 quad 入参 (集成测试 tests/cursor_render_e2e.rs + window.rs
// idle callback 用). quill 自有 struct + enum (INV-010, 不漏 alacritty
// CursorShape).
// T-0604: 加 CURSOR_INSET_PX 给集成测试同步算 cell 内缩 strip 范围 (派单
// In #C 让 cursor 不接触相邻 cell 边缘, 字形溢出像素不被覆盖).
pub use render::{CursorInfo, CursorStyle, CURSOR_INSET_PX, CURSOR_THICKNESS_PX};

// T-0603: 集成测试 tests/keyboard_repeat_e2e.rs 用 KeyboardState +
// wl_keyboard::Event + KeyboardAction. quill 自有类型 (INV-010).
pub use keyboard::{handle_key_event, KeyboardAction, KeyboardState};

// T-0607: 鼠标拖选 + 复制 / 粘贴 状态机. 集成测试 tests/selection_e2e.rs 用
// SelectionState + SelectionMode + selected_cells_* + extract_selection_text +
// bracketed_paste_wrap. quill 自有类型 (INV-010, 不漏 wayland-protocols).
pub use selection::{
    bracketed_paste_wrap, extract_selection_text, modifier_to_selection_mode, pixel_to_cell,
    selected_cells_block, selected_cells_linear, PasteSource, SelectionMode, SelectionState,
};
