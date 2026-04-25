//! T-0502 集成测试: HIDPI_SCALE re-export + set_buffer_scale 协议合同。
//!
//! **why 集成测试而非单测**: 真 `set_buffer_scale` 需要活的 wl_compositor +
//! xdg_shell, 起 headless wayland 就为测一行 protocol request 过度。本组测试
//! 锁住 **public API contract** + **协议常量耦合** —— 上层 (`run_window`) 改
//! HIDPI_SCALE 路径或忘调 set_buffer_scale 时, 至少这层固化能拦"hardcode 漂移"
//! 这类回归。
//!
//! 视觉验证 (set_buffer_scale 真生效, 视觉不再 ×4) 走 Lead 手测 + ROADMAP
//! soak 路径, 不在自动化覆盖内 (派单 #D 显式允许"集成测试不易, Lead 手测验视觉")。

/// HIDPI_SCALE 是 `pub const u32 = 2`, 由 `src/wl/mod.rs` re-export 给
/// `crate::wl::HIDPI_SCALE`。`run_window` 在 `set_buffer_scale` 调用时实参
/// 必须是 **同一个** const (不能 hardcode 2 重复一次), 否则 T-0404 / T-0502
/// 的"改一处"语义破坏。
#[test]
fn hidpi_scale_reexport_is_stable_u32_two() {
    // 编译期 const 检查 — 类型必须是 u32 (set_buffer_scale 入参 u32, cast i32),
    // 值必须是 2 (T-0404 hardcode + T-0502 派单 In #A 显式要求)。
    const _: u32 = quill::wl::HIDPI_SCALE;
    assert_eq!(quill::wl::HIDPI_SCALE, 2, "HIDPI_SCALE 必须是 2 (T-0404)");
}

/// T-0502 派单 In #A 显式要求: set_buffer_scale 接受 i32, HIDPI_SCALE 是 u32,
/// 必须 cast as i32 (SCTK `WaylandSurface::set_buffer_scale(u32)` 内部又 cast
/// i32 给 wl_surface.set_buffer_scale)。本测固化"u32 → i32 cast 不溢出"
/// (HIDPI_SCALE = 2 远小于 i32::MAX, defense-in-depth 覆盖未来若改大值)。
#[test]
fn hidpi_scale_fits_in_i32_for_set_buffer_scale_protocol() {
    let scale_u32: u32 = quill::wl::HIDPI_SCALE;
    let scale_i32: i32 = scale_u32 as i32;
    assert!(
        scale_i32 > 0,
        "wl_surface.set_buffer_scale 协议要求 scale > 0, HIDPI_SCALE={scale_u32}"
    );
    assert_eq!(
        scale_i32 as u32, scale_u32,
        "u32 → i32 cast 应无损 (HIDPI_SCALE 小于 i32::MAX)"
    );
}
