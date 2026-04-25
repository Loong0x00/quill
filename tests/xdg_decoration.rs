//! T-0503 xdg-decoration 协商集成测试。
//!
//! 不启 Wayland 连接, 只测 `WindowCore::decoration_logged` 字段语义 —— 这条
//! 字段是 T-0503 加的"一次性 log" 标记, 锁住"装饰协商结果在窗口生命周期内
//! 只 log 一次, 不被 configure 多次 fire 重复打到 trace"的承诺。
//!
//! 真协商行为 (sctk DecorationMode 转 log 决策) 由 `src/wl/window.rs` 内单测
//! `decoration_log_decision_*` 覆盖 (DecorationMode 是 sctk re-export, 按
//! INV-010 类型隔离不出 module 边界)。
//!
//! 不在此处测 "实际跑 cargo run 看到 titlebar" — 该路径需要真 compositor +
//! 桌面环境 (KDE/wlroots/Hyprland 才会 SSD; GNOME mutter 不导出
//! zxdg_decoration_manager_v1, 永远 CSD), 由 Lead 手测验。

use quill::wl::WindowCore;

#[test]
fn fresh_core_starts_with_decoration_unlogged() {
    // T-0503 加的字段。新建 core 默认 false, 表示"还没收到任何 configure / 还没
    // 决定 log 装饰协商结果"。WindowHandler::configure 首次 fire 时读 false,
    // 走 log 分支, 然后置 true。
    let core = WindowCore::new(800, 600);
    assert!(
        !core.decoration_logged,
        "新建 WindowCore 的 decoration_logged 应为 false (从未 log 过装饰协商)"
    );
}

#[test]
fn decoration_logged_field_is_independent_from_other_state() {
    // 装饰 log 标记跟 first_configure / exit / resize_dirty 是正交的状态位。
    // 验证 new(_) 后四个字段独立初始化, 锁住 WindowCore 字段语义不被
    // 后续重构无意耦合 (例如不应有"first_configure=true 时强制 decoration_logged=true"
    // 这种隐式不变式潜入)。
    let core = WindowCore::new(1920, 1080);
    assert!(core.first_configure, "新 core 应 first_configure=true");
    assert!(!core.exit, "新 core 应 exit=false");
    assert!(!core.resize_dirty, "新 core 应 resize_dirty=false");
    assert!(
        !core.decoration_logged,
        "新 core 应 decoration_logged=false"
    );
}

#[test]
fn decoration_logged_field_is_publicly_settable() {
    // 字段标 pub (跟其它 WindowCore 字段一致, 见 src/wl/window.rs WindowCore 定义),
    // 让真 WindowHandler::configure 在 log 后能置 true。锁住可见性不被无意改成
    // pub(crate) 而破坏 configure 路径的访问。
    let mut core = WindowCore::new(800, 600);
    core.decoration_logged = true;
    assert!(core.decoration_logged);
    core.decoration_logged = false;
    assert!(!core.decoration_logged);
}
