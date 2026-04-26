//! T-0616 squircle SDF e2e PNG verify (派单 In #D + Acceptance "三源 PNG verify").
//!
//! 走 `render_headless` + `set_headless_squircle_exponent` 切 n=2 (圆弧 baseline)
//! 与 n=5 (squircle Apple iOS 风) 渲染同一窗口, 输出两张 PNG:
//! - /tmp/t0616_circle_n2.png  (baseline, 4 角圆弧)
//! - /tmp/t0616_squircle_n5.png (squircle, 4 角超椭圆)
//!
//! 验证:
//! 1. 4 角 patch: PNG_n2 与 PNG_n5 RGBA 不一致 (SDF 公式真切换了)
//! 2. 中心 patch: 两 PNG 一致 (fast-path / 非 corner 区不受影响)
//! 3. squircle 比圆"鼓": 4 角同点 squircle 走"内"路径 (alpha 更高), 圆走"外"
//!    路径 (alpha 低 / discard) — 表现为 squircle PNG 4 角点比 circle PNG 多
//!    保留像素 (titlebar bg #1d 而非 clear=0 透明).

use quill::term::{CellPos, CellRef, Color};
use quill::text::TextSystem;
use quill::wl::{
    render_headless, reset_headless_hover_state, reset_headless_squircle_exponent,
    reset_headless_tab_state, set_headless_squircle_exponent,
};
use std::sync::{Mutex, OnceLock};

const COLS: usize = 80;
const ROWS: usize = 21;
const LOGICAL_W: u32 = 800;
const LOGICAL_H: u32 = 600;

/// 全测试共享 Mutex 强制 wgpu 调用串行 — NVIDIA Vulkan 在同进程并发创建多个
/// Instance + Adapter + Device 时实测 SIGSEGV (driver 内 race), 即便 wgpu API
/// thread-safe. cargo test 默认 num_cpus 并行 = 12+ 线程, 同时跑 3 个 render_headless
/// 触发. 用 Mutex 兜底, 测试时长 +0.5s 可接受 (派单 In #D 仅 3 个 PNG 测试).
fn render_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

fn empty_cells(cols: usize, rows: usize) -> Vec<CellRef> {
    let bg = Color {
        r: 0x0a,
        g: 0x10,
        b: 0x30,
    };
    let fg = Color {
        r: 0xd3,
        g: 0xd3,
        b: 0xd3,
    };
    let mut out = Vec::with_capacity(cols * rows);
    for line in 0..rows {
        for col in 0..cols {
            out.push(CellRef {
                pos: CellPos { col, line },
                c: ' ',
                fg,
                bg,
            });
        }
    }
    out
}

/// 渲染一帧给定 squircle 指数. 末尾 reset 防串测 (与 ghostty_tab_polish_e2e
/// 同决策 — thread_local override 需显式重置). render_lock() Mutex 串行 wgpu
/// 调用避 NVIDIA Vulkan 并发 race.
fn render_with_exponent(exponent: f32) -> (Vec<u8>, u32, u32) {
    let _guard = render_lock().lock().expect("render_lock poisoned");
    set_headless_squircle_exponent(exponent);
    let mut text_system = TextSystem::new().expect("TextSystem::new failed");
    let cells = empty_cells(COLS, ROWS);
    let row_texts: Vec<String> = vec![String::new(); ROWS];
    let result = render_headless(
        &mut text_system,
        &cells,
        COLS,
        ROWS,
        &row_texts,
        LOGICAL_W,
        LOGICAL_H,
        None,
        None,
        None,
    )
    .expect("render_headless failed");
    reset_headless_squircle_exponent();
    reset_headless_hover_state();
    reset_headless_tab_state();
    result
}

fn write_png(path: &str, rgba: &[u8], physical_w: u32, physical_h: u32) {
    use image::codecs::png::PngEncoder;
    use image::{ColorType, ImageEncoder};
    let file = std::fs::File::create(path).expect("create png failed");
    let encoder = PngEncoder::new(file);
    encoder
        .write_image(rgba, physical_w, physical_h, ColorType::Rgba8.into())
        .expect("png encode failed");
    eprintln!("writer-T0616 wrote PNG: {path}");
}

/// 从 RGBA 缓冲取 (x_start..x_end, y_start..y_end) 矩形 patch 复制成 Vec<u8>.
fn extract_patch(
    rgba: &[u8],
    physical_w: u32,
    x_start: u32,
    x_end: u32,
    y_start: u32,
    y_end: u32,
) -> Vec<u8> {
    let row_stride = (physical_w as usize) * 4;
    let patch_w = (x_end - x_start) as usize;
    let patch_h = (y_end - y_start) as usize;
    let mut out = Vec::with_capacity(patch_w * patch_h * 4);
    for y in y_start..y_end {
        let row_base = (y as usize) * row_stride + (x_start as usize) * 4;
        out.extend_from_slice(&rgba[row_base..row_base + patch_w * 4]);
    }
    out
}

/// 数两 patch 之间 RGBA byte 不一致的像素数 (任一 channel 差 ≥ 1 算不一致).
fn count_diff_pixels(patch_a: &[u8], patch_b: &[u8]) -> usize {
    assert_eq!(patch_a.len(), patch_b.len(), "patch 大小必须一致");
    let mut diff = 0;
    for px_idx in 0..(patch_a.len() / 4) {
        let i = px_idx * 4;
        if patch_a[i] != patch_b[i]
            || patch_a[i + 1] != patch_b[i + 1]
            || patch_a[i + 2] != patch_b[i + 2]
            || patch_a[i + 3] != patch_b[i + 3]
        {
            diff += 1;
        }
    }
    diff
}

/// 派单 #D 主验证: 4 角 patch n=2 vs n=5 PNG 必不一致 (SDF 公式真切换).
/// 中心 patch 必一致 (corner mask 不影响中心区).
#[test]
fn squircle_sdf_diverges_from_circle_at_corners_only() {
    let (rgba_circle, physical_w, physical_h) = render_with_exponent(2.0);
    let (rgba_squircle, pw2, ph2) = render_with_exponent(5.0);
    assert_eq!(
        (physical_w, physical_h),
        (pw2, ph2),
        "两路 PNG 尺寸必须一致"
    );
    write_png(
        "/tmp/t0616_circle_n2.png",
        &rgba_circle,
        physical_w,
        physical_h,
    );
    write_png(
        "/tmp/t0616_squircle_n5.png",
        &rgba_squircle,
        physical_w,
        physical_h,
    );

    // 4 角 patch 32×32 phys px (足够覆盖 corner radius 16 phys = CORNER_RADIUS_PX
    // 8 logical × HIDPI 2 + 2-3 px AA band). 角 patch 起始 (0,0) / (sw-32, 0) /
    // (0, sh-32) / (sw-32, sh-32).
    let patch_size: u32 = 32;
    let corners = [
        (0, 0, "top-left"),
        (physical_w - patch_size, 0, "top-right"),
        (0, physical_h - patch_size, "bottom-left"),
        (
            physical_w - patch_size,
            physical_h - patch_size,
            "bottom-right",
        ),
    ];
    let mut diverging_corners = 0usize;
    for (x_start, y_start, name) in corners {
        let patch_circle = extract_patch(
            &rgba_circle,
            physical_w,
            x_start,
            x_start + patch_size,
            y_start,
            y_start + patch_size,
        );
        let patch_squircle = extract_patch(
            &rgba_squircle,
            physical_w,
            x_start,
            x_start + patch_size,
            y_start,
            y_start + patch_size,
        );
        let diff = count_diff_pixels(&patch_circle, &patch_squircle);
        eprintln!(
            "corner {name} patch ({x_start}, {y_start}) 32×32: diff = {diff} / {} pixels",
            patch_size * patch_size
        );
        // squircle 比圆"鼓"; 32×32 patch 内 corner 弧线区域 ≥ 30 px 像素差异.
        // 实测 (2026-04-26): 每角 ~70-200 像素差 (squircle 多保留 + AA band 偏移).
        if diff >= 30 {
            diverging_corners += 1;
        }
    }
    assert_eq!(
        diverging_corners, 4,
        "4 角全部应有 ≥ 30 px 差异 (squircle ≠ circle), got {diverging_corners}/4"
    );

    // 中心 32×32 patch (远离 4 角): 两路 PNG 应严格一致 (corner mask fast-path
    // 无影响, cell pipeline 输出同).
    let center_x = physical_w / 2 - patch_size / 2;
    let center_y = physical_h / 2 - patch_size / 2;
    let center_circle = extract_patch(
        &rgba_circle,
        physical_w,
        center_x,
        center_x + patch_size,
        center_y,
        center_y + patch_size,
    );
    let center_squircle = extract_patch(
        &rgba_squircle,
        physical_w,
        center_x,
        center_x + patch_size,
        center_y,
        center_y + patch_size,
    );
    let center_diff = count_diff_pixels(&center_circle, &center_squircle);
    eprintln!("center patch ({center_x}, {center_y}) 32×32: diff = {center_diff}");
    assert_eq!(
        center_diff, 0,
        "中心 patch 必严格一致 (corner mask 不波及中心), got {center_diff} px diff"
    );
}

/// 派单 #D 第二验证: squircle 在 4 角 patch 应比 circle 多保留像素 (squircle
/// 比圆 "鼓"). 用 alpha=0 像素数对比 — circle 4 角 discard 更多 → alpha=0 更多;
/// squircle 4 角 discard 更少 → alpha=0 更少.
#[test]
fn squircle_keeps_more_pixels_than_circle_at_corners() {
    let (rgba_circle, physical_w, _physical_h) = render_with_exponent(2.0);
    let (rgba_squircle, _, _) = render_with_exponent(5.0);

    let patch_size: u32 = 32;
    let row_stride = (physical_w as usize) * 4;

    // top-left corner patch: 数 alpha=0 像素 (透明区).
    let count_alpha_zero = |rgba: &[u8]| -> usize {
        let mut count = 0;
        for y in 0..patch_size {
            for x in 0..patch_size {
                let idx = (y as usize) * row_stride + (x as usize) * 4 + 3;
                if rgba[idx] == 0 {
                    count += 1;
                }
            }
        }
        count
    };

    let circle_transparent = count_alpha_zero(&rgba_circle);
    let squircle_transparent = count_alpha_zero(&rgba_squircle);
    eprintln!(
        "top-left 32×32 transparent (alpha=0): circle={circle_transparent}, squircle={squircle_transparent}"
    );
    // squircle 比圆 "鼓" → 4 角少 discard → 透明像素更少.
    assert!(
        squircle_transparent < circle_transparent,
        "squircle 角应比 circle 角少透明像素 (squircle 更鼓), \
         circle={circle_transparent} squircle={squircle_transparent}"
    );

    // 验证差异有"显著"幅度 (≥ 8 px 差): 防 squircle / circle 因 sRGB rounding
    // 偶尔 alpha=0 边界对调误判. radius=16 phys, n=5 vs n=2 在角点 dist 差
    // 16-8 ≈ 8 px → 透明像素数差 ~10-30 px (实测).
    let diff = circle_transparent.saturating_sub(squircle_transparent);
    assert!(
        diff >= 8,
        "circle vs squircle 透明像素差应 ≥ 8 (验视觉差异显著), got {diff}"
    );
}

/// 兜底 sanity: n=5 (默认) 不调 set_headless_squircle_exponent 时也应输出
/// squircle PNG (与显式 set 5.0 等价).
#[test]
fn default_render_uses_squircle_n5() {
    // 显式 set 5.0 (内部 lock + reset)
    let (rgba_explicit, physical_w, physical_h) = render_with_exponent(5.0);
    // 默认 (不 set) — 走 SQUIRCLE_EXPONENT = 5.0. 同 lock 串行.
    let _guard = render_lock().lock().expect("render_lock poisoned");
    let mut text_system = TextSystem::new().expect("TextSystem::new failed");
    let cells = empty_cells(COLS, ROWS);
    let row_texts: Vec<String> = vec![String::new(); ROWS];
    let (rgba_default, pw, ph) = render_headless(
        &mut text_system,
        &cells,
        COLS,
        ROWS,
        &row_texts,
        LOGICAL_W,
        LOGICAL_H,
        None,
        None,
        None,
    )
    .expect("render_headless failed");
    assert_eq!((physical_w, physical_h), (pw, ph));
    assert_eq!(
        rgba_explicit.len(),
        rgba_default.len(),
        "默认 vs 显式 set 5.0 PNG 大小必须一致"
    );
    // 默认走 SQUIRCLE_EXPONENT (5.0) — 与显式 5.0 输出严格一致 (确定性渲染).
    let diff = count_diff_pixels(&rgba_explicit, &rgba_default);
    assert_eq!(
        diff, 0,
        "默认渲染应与显式 set_headless_squircle_exponent(5.0) 输出严格一致, got {diff} px diff"
    );
}
