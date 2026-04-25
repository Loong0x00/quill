//! Wayland 客户端封装。本阶段只拉起 xdg_toplevel 窗口并用 wgpu 画一块清屏背景
//! (T-0102)。对外只暴露 [`run_window`] 一个入口,隐藏 SCTK / wayland-client /
//! wgpu 的全部原始类型。后续 ticket 接 calloop / resize。

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
