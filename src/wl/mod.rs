//! Wayland 客户端封装。本阶段(T-0101)只拉起 xdg_toplevel 窗口,后续 ticket 会接
//! calloop / wgpu / resize。对外只暴露 [`run_window`] 一个入口,隐藏 SCTK 与
//! wayland-client 的全部原始类型。

mod window;

pub use window::run_window;

// T-0107 为让 headless 测试能直接单测状态转移,把纯逻辑状态机
// ([`WindowCore`] + [`WindowEvent`] + [`handle_event`])公开,内部活动回调
// 也走同一条路径(见 `window.rs::State::configure`)。
pub use window::{handle_event, WindowAction, WindowCore, WindowEvent};
