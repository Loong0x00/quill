//! Wayland 客户端封装。本阶段只拉起 xdg_toplevel 窗口并用 wgpu 画一块清屏背景
//! (T-0102)。对外只暴露 [`run_window`] 一个入口,隐藏 SCTK / wayland-client /
//! wgpu 的全部原始类型。后续 ticket 接 calloop / resize。

mod keyboard;
mod pointer;
mod render;
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

// T-0408 离屏渲染入口 (offscreen render → RGBA8 Vec<u8>)。`src/main.rs`
// `--headless-screenshot` CLI flag + `tests/headless_screenshot.rs` 集成测试
// 都走此 fn。入参 / 出参全 quill 自有类型, 不漏 wgpu 内部 (INV-010 类型隔离),
// 详见 `render.rs::render_headless` 文档头。
pub use render::render_headless;

// T-0505: PreeditOverlay 入参 (集成测试 tests/ime_preedit_render.rs 用), quill
// 自有 struct (INV-010, 不漏 wayland-protocols 类型). PREEDIT_UNDERLINE_PX
// 常数同步 re-export 给测试 / Phase 6 计算用。
pub use render::{PreeditOverlay, PREEDIT_UNDERLINE_PX};

// T-0603: 集成测试 tests/keyboard_repeat_e2e.rs 需要构造 KeyboardState +
// 喂 wl_keyboard::Event + 拿 KeyboardAction. 这三个都是 quill 自有类型
// (INV-010: KeyboardState 字段全私有, KeyboardAction 是 std 类型 enum, 不
// 漏 xkbcommon / wayland-client 内部类型). 入参 wl_keyboard::Event 是
// wayland-client 协议类型 (已在 Dispatch trait 边界, 集成测试用同一类型
// 构造 event 是协议层防御测试惯例, 与 Dispatch<WlKeyboard> 同等暴露).
pub use keyboard::{handle_key_event, KeyboardAction, KeyboardState};
