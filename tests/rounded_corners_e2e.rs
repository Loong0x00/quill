//! T-0610 part 2 集成测试: 窗口圆角 (shader corner radius mask) 端到端 PNG verify.
//!
//! **覆盖** (派单 In #F):
//! - 4 角应有 alpha=0 像素 (透明), corner_radius=8 logical = 16 physical
//! - 中心区域应有 alpha=255 像素 (不透明, headless alpha_live=1.0)
//! - PNG 端到端走 render_headless 出图
//!
//! **运行**: `cargo test --release --test rounded_corners_e2e`
//!
//! **PNG 三源 verify** (派单 Acceptance): writer 跑这套测试输出 /tmp/t0610_*.png,
//! Lead + reviewer 各自 Read PNG 文件再 verify.

use std::path::PathBuf;

use quill::term::TermState;
use quill::text::TextSystem;
use quill::wl::{render_headless, HIDPI_SCALE};

const LOGICAL_W: u32 = 800;
const LOGICAL_H: u32 = 600;
const PHYSICAL_W: u32 = LOGICAL_W * HIDPI_SCALE;
const PHYSICAL_H: u32 = LOGICAL_H * HIDPI_SCALE;
const COLS: u16 = 80;
const ROWS: u16 = 24;

/// 渲染一帧 (空 term, 无 shell, 无 prompt) → 返 (rgba, physical_w, physical_h).
/// 与 [`headless_screenshot`] 共享套路但跳 PtyHandle (避免 spawn shell 引依赖).
fn render_empty_frame() -> (Vec<u8>, u32, u32) {
    let mut text_system = TextSystem::new().expect("TextSystem::new failed (need monospace font)");
    let term = TermState::new(COLS, ROWS);
    let cells: Vec<_> = term.cells_iter().collect();
    let (cols_actual, rows_actual) = term.dimensions();
    let row_texts: Vec<String> = (0..rows_actual).map(|r| term.line_text(r)).collect();

    render_headless(
        &mut text_system,
        &cells,
        cols_actual,
        rows_actual,
        &row_texts,
        LOGICAL_W,
        LOGICAL_H,
        None,
        None,
        None,
    )
    .expect("render_headless failed")
}

/// 把 RGBA buffer 写到 /tmp/t0610_*.png, 让 Lead + reviewer Read 验.
fn write_png(rgba: &[u8], physical_w: u32, physical_h: u32, suffix: &str) -> PathBuf {
    use image::codecs::png::PngEncoder;
    use image::ExtendedColorType;
    use image::ImageEncoder;

    let mut path = PathBuf::from("/tmp");
    path.push(format!("t0610_{}_{}.png", suffix, std::process::id()));
    let file = std::fs::File::create(&path).expect("create PNG file");
    let encoder = PngEncoder::new(file);
    encoder
        .write_image(rgba, physical_w, physical_h, ExtendedColorType::Rgba8)
        .expect("PngEncoder write_image");
    path
}

/// 4 角顶点 (0,0) / (sw-1,0) / (0,sh-1) / (sw-1,sh-1) 应 alpha=0 (corner mask
/// fragment discard, clear=0 透明值保留). corner_radius=8 logical=16 physical,
/// 顶点距内嵌圆心 r*sqrt(2) ≈ 22.6 > r+1 = 17 → discard.
#[test]
fn rounded_corners_alpha_zero_at_four_corners() {
    let (rgba, physical_w, physical_h) = render_empty_frame();
    assert_eq!(physical_w, PHYSICAL_W);
    assert_eq!(physical_h, PHYSICAL_H);
    let path = write_png(&rgba, physical_w, physical_h, "corners");
    eprintln!("PNG output: {}", path.display());

    let row_stride = (physical_w as usize) * 4;
    // 4 顶角像素 alpha (RGBA byte 4 = alpha)
    let alpha_at = |x: u32, y: u32| -> u8 {
        let idx = (y as usize) * row_stride + (x as usize) * 4 + 3;
        rgba[idx]
    };

    let tl = alpha_at(0, 0);
    let tr = alpha_at(physical_w - 1, 0);
    let bl = alpha_at(0, physical_h - 1);
    let br = alpha_at(physical_w - 1, physical_h - 1);

    assert_eq!(tl, 0, "top-left corner alpha 应 = 0 (透明), got {tl}");
    assert_eq!(tr, 0, "top-right corner alpha 应 = 0 (透明), got {tr}");
    assert_eq!(bl, 0, "bottom-left corner alpha 应 = 0 (透明), got {bl}");
    assert_eq!(br, 0, "bottom-right corner alpha 应 = 0 (透明), got {br}");
}

/// 中心区域应 alpha=255 (不透明). headless alpha_live=1.0 锁让 PNG center
/// alpha 充满, 与现有 PNG verify 套路 (headless_screenshot.rs) 兼容.
#[test]
fn rounded_corners_alpha_full_at_center() {
    let (rgba, physical_w, physical_h) = render_empty_frame();
    let row_stride = (physical_w as usize) * 4;
    let cx = physical_w / 2;
    let cy = physical_h / 2;
    let idx = (cy as usize) * row_stride + (cx as usize) * 4 + 3;
    let alpha = rgba[idx];
    assert_eq!(
        alpha, 255,
        "中心点 alpha 应 = 255 (headless alpha_live=1.0, bg fill 满 alpha), got {alpha}"
    );
}

/// 顶角附近 16x16 物理 px 区 (corner_radius_phys = 16) 内应有"显著"的 alpha=0
/// 像素 (圆形外的"角耳"). 不严格要 ≥ 某数, 而是抽样验"corner ear 真存在 alpha=0
/// 像素". corner = r * (1 - π/4) / 2 ≈ 1.7 / 16 ≈ 10% 区域是"角耳".
#[test]
fn rounded_corners_top_left_16x16_has_alpha_zero_pixels() {
    let (rgba, physical_w, _physical_h) = render_empty_frame();
    let row_stride = (physical_w as usize) * 4;
    let mut zero_alpha_count: u32 = 0;
    let region_w: u32 = 16;
    let region_h: u32 = 16;
    for y in 0..region_h {
        for x in 0..region_w {
            let idx = (y as usize) * row_stride + (x as usize) * 4 + 3;
            if rgba[idx] == 0 {
                zero_alpha_count += 1;
            }
        }
    }
    // r=16 phys, 顶角 16×16 = 256 总像素, 圆形外 "角耳" ≈ 256 × (1 - π/4) ≈ 55 px.
    // 留余量阈值 30+ (smoothstep AA 边缘可能算非 0).
    assert!(
        zero_alpha_count >= 30,
        "顶左 16×16 区应至少有 30 个 alpha=0 像素 (corner ear), 实测 {zero_alpha_count}"
    );
}

/// 中心区 100×100 px alpha 全部 ≥ 250 (允许 sRGB encode 边界小漂移 ≤ 5).
/// 防 corner mask 误用到中心区.
#[test]
fn rounded_corners_center_100x100_all_opaque() {
    let (rgba, physical_w, physical_h) = render_empty_frame();
    let row_stride = (physical_w as usize) * 4;
    let cx = physical_w / 2;
    let cy = physical_h / 2;
    for y in (cy - 50)..(cy + 50) {
        for x in (cx - 50)..(cx + 50) {
            let idx = (y as usize) * row_stride + (x as usize) * 4 + 3;
            let alpha = rgba[idx];
            assert!(
                alpha >= 250,
                "中心区 ({x}, {y}) alpha ({alpha}) 应 ≥ 250 (corner mask 不应触发, sw={physical_w} sh={physical_h})"
            );
        }
    }
}

/// PNG 文件落盘成功且 ≥ 4 KiB (派单 三源 PNG verify 路径).
#[test]
fn rounded_corners_png_saved_to_tmp() {
    let (rgba, physical_w, physical_h) = render_empty_frame();
    let path = write_png(&rgba, physical_w, physical_h, "saved");
    let metadata = std::fs::metadata(&path).expect("PNG metadata");
    assert!(
        metadata.len() > 4096,
        "PNG file size {} 应 > 4 KiB",
        metadata.len()
    );
    eprintln!("PNG saved: {}", path.display());
    // 不删 — Lead + reviewer 走 Read 验
}
