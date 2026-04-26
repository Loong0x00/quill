//! T-0702 titlebar 标题文字端到端 PNG verify (集成测试).
//!
//! 走 `render_headless` (T-0408 离屏渲染入口) 写 /tmp/t0702_test.png, 验:
//! - PNG 文件存在 + 尺寸正确 (physical = logical × HIDPI_SCALE)
//! - titlebar 中央 (避开三按钮 + 边距) 有 BUTTON_ICON #d3d3d3 浅灰像素
//!   (在 titlebar bg #2c2c2c 深灰背景上, 浅 vs 深对比明显) — 即 "quill" 字
//!   的 stroke 像素
//!
//! 派单 In #D + #E "三源 PNG verify SOP": writer + Lead + reviewer 都 Read
//! 同一 PNG 验视觉. /tmp/t0702_test.png 是 deliverable, 跑完 `cargo test
//! --release --test titlebar_text_e2e` 后留在 /tmp 给 Lead / reviewer Read.
//!
//! **运行**: `cargo test --release --test titlebar_text_e2e`

use std::path::PathBuf;

use quill::pty::PtyHandle;
use quill::term::TermState;
use quill::text::TextSystem;
use quill::wl::{render_headless, HIDPI_SCALE};

const LOGICAL_W: u32 = 800;
const LOGICAL_H: u32 = 600;
const PHYSICAL_W: u32 = LOGICAL_W * HIDPI_SCALE;
const PHYSICAL_H: u32 = LOGICAL_H * HIDPI_SCALE;
const COLS: u16 = 80;
// T-0504: cell rows 减 titlebar (28 logical / 25 cell_h ≈ 1 行减) → 22 行.
const ROWS: u16 = 22;
const PROMPT_WAIT_MS: u64 = 500;

/// 跑一遍 PTY → Term → render_headless 链路, 返 (rgba, physical_w, physical_h).
/// render_headless 内 title 锁 DEFAULT_TITLE = "quill" (派单 hardcode).
fn render_with_titlebar() -> (Vec<u8>, u32, u32) {
    let mut text_system = TextSystem::new().expect("TextSystem::new failed (need monospace font)");

    let mut term = TermState::new(COLS, ROWS);
    let mut pty = PtyHandle::spawn_shell(COLS, ROWS).expect("PtyHandle::spawn_shell failed");

    std::thread::sleep(std::time::Duration::from_millis(PROMPT_WAIT_MS));

    let mut buf = [0u8; 4096];
    loop {
        match pty.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => term.advance(&buf[..n]),
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        }
    }

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
        None, // T-0607 selection
    )
    .expect("render_headless failed")
}

/// 派单 In #D test 1: titlebar 中央 (避开按钮 + 居中带宽 200 px) 有非 bg
/// 像素 — 即 "quill" 字形 stroke 落在该区. 字 stroke 用 BUTTON_ICON #d3d3d3,
/// 显著亮于 titlebar bg #2c2c2c.
#[test]
fn titlebar_text_has_icon_color_pixels_in_center() {
    let (rgba, physical_w, _physical_h) = render_with_titlebar();

    // titlebar 物理 56 phys (28 logical × HIDPI). 字形 baseline ~44 phys, 字
    // 主体 y 区 ~26-46 phys. x: 居中 ~ surface_w/2 ± 100 phys (5 字 × 20 phys
    // advance 的字宽估算).
    let titlebar_h_physical: u32 = 28 * HIDPI_SCALE;
    let cx = physical_w / 2;
    // 居中 ± 200 phys (留 margin, 防 5 字字宽估算偏差) 横向; 纵向跳过最顶
    // 4 phys 防 antialias 边, 取 titlebar 整高.
    let region_x_start: u32 = cx.saturating_sub(200);
    let region_x_end: u32 = cx + 200;
    let region_y_start: u32 = 4;
    let region_y_end: u32 = titlebar_h_physical - 2;

    let mut icon_pixels: u32 = 0;
    let row_stride = (physical_w as usize) * 4;
    for y in region_y_start..region_y_end {
        for x in region_x_start..region_x_end {
            let idx = (y as usize) * row_stride + (x as usize) * 4;
            let r = rgba[idx];
            let g = rgba[idx + 1];
            let b = rgba[idx + 2];
            // BUTTON_ICON #d3d3d3 (211/211/211). sRGB 编码后 mid-high gray
            // ≥ 100 (与 csd_e2e::csd_buttons_have_icon_pixels 同阈值). gray
            // 三通道接近 (R-G 差 ≤ 32).
            let is_icon = r >= 100 && g >= 100 && b >= 100 && (r as i32 - g as i32).abs() <= 32;
            if is_icon {
                icon_pixels += 1;
            }
        }
    }

    // "quill" 5 字 × ~17 phys 高 × ~3 phys 平均 stroke 宽 ≈ 250-400 字形像素.
    // 阈值用 80 防 face / antialias 计数偏差.
    assert!(
        icon_pixels >= 80,
        "titlebar 中央 'quill' icon 像素 {} < 80; 标题没画? region {}×{}",
        icon_pixels,
        region_x_end - region_x_start,
        region_y_end - region_y_start
    );
}

/// 派单 In #D + 派单关键提示 "三源 PNG verify": 写 /tmp/t0702_test.png 到固定
/// 路径让 writer + Lead + reviewer Read 同一文件验视觉.
///
/// **why 单独 test 写固定路径**: 派单原文 "writer 跑 /tmp/t0702_test.png 自验"
/// + "Lead Read PNG 第 2 源" + "reviewer 第 3 源". 其它 test 用 pid 后缀防
///   并发, 本 test 专门给 deliverable.
#[test]
fn titlebar_text_writes_deliverable_png_to_fixed_path() {
    let (rgba, physical_w, physical_h) = render_with_titlebar();

    use image::codecs::png::PngEncoder;
    use image::ExtendedColorType;
    use image::ImageEncoder;

    let path: PathBuf = "/tmp/t0702_test.png".into();
    let file = std::fs::File::create(&path).expect("create /tmp/t0702_test.png");
    let encoder = PngEncoder::new(file);
    encoder
        .write_image(&rgba, physical_w, physical_h, ExtendedColorType::Rgba8)
        .expect("PngEncoder write_image");

    let metadata = std::fs::metadata(&path).expect("PNG file metadata");
    assert!(
        metadata.len() > 4096,
        "/tmp/t0702_test.png 大小 {} < 4 KiB (deliverable PNG 应至少 4 KiB)",
        metadata.len()
    );
    // 不删 — 派单要求三源 Read 验视觉, 文件留给后续 step.

    // PNG signature sanity
    let header = std::fs::read(&path).expect("read PNG");
    assert!(
        header.len() >= 8 && &header[..8] == b"\x89PNG\r\n\x1a\n",
        "PNG signature mismatch"
    );

    assert_eq!(physical_w, PHYSICAL_W);
    assert_eq!(physical_h, PHYSICAL_H);
}

/// 派单 In #D test 2 (sanity): titlebar 区与 cells 区视觉分隔 — titlebar 外
/// 区不应大量出现 BUTTON_ICON 浅灰 (字应在 titlebar 内, 不溢出).
///
/// **why 加此 test**: 防 baseline_y 算错把字画到 titlebar 之外 cell 区, 视觉
/// 上叠在 bash prompt 上面 / 字飘到 cells 区. 取 titlebar 紧下方 4 phys 行,
/// 中央 200 phys 横带, 应几乎全 cell 区 bg / clear color (非 #d3d3d3 浅灰).
#[test]
fn titlebar_text_does_not_bleed_into_cell_area() {
    let (rgba, physical_w, _physical_h) = render_with_titlebar();

    let titlebar_h_physical: u32 = 28 * HIDPI_SCALE;
    let cx = physical_w / 2;
    let region_x_start: u32 = cx.saturating_sub(200);
    let region_x_end: u32 = cx + 200;
    // 紧贴 titlebar 下方 8 phys 行 (cell 区起点 y = titlebar_h_physical, 取
    // 头几行验字形 descender 不溢出).
    let region_y_start: u32 = titlebar_h_physical;
    let region_y_end: u32 = titlebar_h_physical + 8;

    let mut icon_bleed_pixels: u32 = 0;
    let row_stride = (physical_w as usize) * 4;
    for y in region_y_start..region_y_end {
        for x in region_x_start..region_x_end {
            let idx = (y as usize) * row_stride + (x as usize) * 4;
            let r = rgba[idx];
            let g = rgba[idx + 1];
            let b = rgba[idx + 2];
            let is_icon = r >= 100 && g >= 100 && b >= 100 && (r as i32 - g as i32).abs() <= 32;
            if is_icon {
                icon_bleed_pixels += 1;
            }
        }
    }

    // 紧下 8 phys 行 × 400 phys 宽 = 3200 像素. cell 区可能有 bash prompt
    // (浅灰文字), 用宽阈值 ≤ 1500 防 prompt 字落在中央带; 防 titlebar 字
    // descender 大量溢出 (那种 case 会满 region 即 ~3000+).
    assert!(
        icon_bleed_pixels <= 1500,
        "titlebar 字 descender 似乎溢出到 cell 区 (icon_bleed={}, > 1500); \
         baseline_y 算错? region {}×{}",
        icon_bleed_pixels,
        region_x_end - region_x_start,
        region_y_end - region_y_start
    );
}
