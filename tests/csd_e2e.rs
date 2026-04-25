//! T-0504 CSD 端到端 PNG verify (集成测试).
//!
//! 走 `render_headless` (T-0408 离屏渲染入口) 写 /tmp/csd_test.png, 验:
//! - PNG 文件存在 + 尺寸 = physical (logical × HIDPI_SCALE)
//! - titlebar 区域 (顶部 56 physical px = 28 logical × 2) 有非清屏像素
//!   (灰色 #2c2c2c 与清屏 #0a1030 区分明显)
//! - 三按钮区域 (右上角 3×24×24 logical = 3×48×48 physical) 有非 titlebar bg
//!   像素 (icon 浅灰 #d3d3d3, 与按钮 bg 区分)
//!
//! 派单 In #G "三源 PNG verify SOP": writer + Lead + reviewer 都 Read PNG 验视觉.
//! 本 helper 在 1600×1200 physical (800×600 logical) 跑, PNG 路径 /tmp/csd_test.png
//! (派单原文路径). writer Read PNG 自验通过即视觉 milestone 达成.
//!
//! **运行**: `cargo test --release --test csd_e2e`

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
// T-0504: cell rows 减 titlebar (28 logical / 25 cell_h = 1 行减) → 22 行.
// term 仍可建 80×24, render_headless 接受任意尺寸; 这里走 80×22 与 cell 区
// 实际可用行数对齐.
const ROWS: u16 = 22;
const PROMPT_WAIT_MS: u64 = 500;

/// 跑一遍 PTY → Term → render_headless 链路, 返 (rgba, physical_w, physical_h).
fn render_csd_with_shell_prompt() -> (Vec<u8>, u32, u32) {
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
    )
    .expect("render_headless failed")
}

/// 写 PNG 到 /tmp/csd_test_<pid>.png, 返 PathBuf. test 间互不干扰.
fn write_csd_png(rgba: &[u8], physical_w: u32, physical_h: u32) -> PathBuf {
    use image::codecs::png::PngEncoder;
    use image::ExtendedColorType;
    use image::ImageEncoder;

    let mut tmp_path = std::env::temp_dir();
    tmp_path.push(format!("csd_test_{}.png", std::process::id()));
    let path: PathBuf = tmp_path;

    let file = std::fs::File::create(&path).expect("create PNG file");
    let encoder = PngEncoder::new(file);
    encoder
        .write_image(rgba, physical_w, physical_h, ExtendedColorType::Rgba8)
        .expect("PngEncoder write_image");
    path
}

/// 派单 In #G test 1: PNG 文件存在 + 尺寸正确.
#[test]
fn csd_renders_titlebar_to_png() {
    let (rgba, physical_w, physical_h) = render_csd_with_shell_prompt();
    assert_eq!(physical_w, PHYSICAL_W);
    assert_eq!(physical_h, PHYSICAL_H);
    assert_eq!(
        rgba.len(),
        (PHYSICAL_W as usize) * (PHYSICAL_H as usize) * 4,
        "RGBA byte len = physical_w * physical_h * 4"
    );

    let path = write_csd_png(&rgba, physical_w, physical_h);
    let metadata = std::fs::metadata(&path).expect("PNG file metadata");
    assert!(metadata.len() > 4096, "PNG file > 4 KiB");

    // PNG signature
    let header = std::fs::read(&path).expect("read PNG");
    assert!(
        header.len() >= 8 && &header[..8] == b"\x89PNG\r\n\x1a\n",
        "PNG signature mismatch"
    );

    let _ = std::fs::remove_file(&path);
}

/// 派单 In #G test 2: titlebar 区域 (顶部 56 physical px) 有非清屏像素.
/// titlebar bg = #2c2c2c (44/44/44), clear bg = #0a1030 (10/16/48).
/// 两者 R / G 通道差异明显 (R 44 vs 10, G 44 vs 16).
#[test]
fn csd_titlebar_has_non_clear_pixels() {
    let (rgba, physical_w, _physical_h) = render_csd_with_shell_prompt();

    // titlebar 物理高度 = 28 logical × 2 = 56
    let titlebar_h_physical: u32 = 28 * HIDPI_SCALE;
    // 检查中段 (避开按钮区, 直接采 titlebar 中央 200×titlebar_h_physical 区).
    let region_x_start: u32 = 200;
    let region_x_end: u32 = 800;
    let region_y_start: u32 = 4; // 跳过最顶 4 px 防 anti-alias 边
    let region_y_end: u32 = titlebar_h_physical - 4;

    let mut titlebar_gray_pixels: u32 = 0;
    let row_stride = (physical_w as usize) * 4;
    for y in region_y_start..region_y_end {
        for x in region_x_start..region_x_end {
            let idx = (y as usize) * row_stride + (x as usize) * 4;
            let r = rgba[idx];
            let g = rgba[idx + 1];
            let b = rgba[idx + 2];
            // titlebar bg #2c2c2c ± 12: R, G, B 都接近 0x2c=44
            // sRGB encoding 后 gray 仍 R≈G≈B ~ 40-50 (有 sRGB 转换误差).
            let is_titlebar_gray = (r as i32 - 0x2c).abs() <= 16
                && (g as i32 - 0x2c).abs() <= 16
                && (b as i32 - 0x2c).abs() <= 16;
            if is_titlebar_gray {
                titlebar_gray_pixels += 1;
            }
        }
    }

    let region_total = (region_x_end - region_x_start) * (region_y_end - region_y_start);
    let gray_ratio = (titlebar_gray_pixels as f64) / (region_total as f64);
    // titlebar 中段 (避开按钮 + 边) 应大部分是 #2c2c2c 灰色, 80%+ 算通过
    assert!(
        gray_ratio > 0.80,
        "titlebar 中段灰色像素占比 {:.1}% < 80%; titlebar 没画? \
         titlebar_gray_pixels={}, region_total={}",
        gray_ratio * 100.0,
        titlebar_gray_pixels,
        region_total
    );
}

/// 派单 In #G test 3: 三按钮区域有 icon 像素 (浅灰 #d3d3d3, 与 titlebar bg
/// 灰 #2c2c2c 区分明显).
///
/// 三按钮位于 titlebar 右端: Close (右) → Maximize (中) → Minimize (左),
/// 各 24×24 logical = 48×48 physical. 总区 144 logical 宽 × 24 logical 高.
/// 区域内应有 icon 像素 (浅灰 stroke), 非 icon 则是 titlebar bg 灰.
#[test]
fn csd_buttons_have_icon_pixels() {
    let (rgba, physical_w, _physical_h) = render_csd_with_shell_prompt();

    // 按钮区: x ∈ [w - 3*24*2, w] = [w - 144, w], y ∈ [0, 24*2] = [0, 48]
    let btn_w_physical: u32 = 24 * HIDPI_SCALE;
    let btn_h_physical: u32 = 24 * HIDPI_SCALE;
    let region_x_start: u32 = physical_w.saturating_sub(3 * btn_w_physical);
    let region_x_end: u32 = physical_w;
    let region_y_start: u32 = 0;
    let region_y_end: u32 = btn_h_physical;

    let mut icon_pixels: u32 = 0;
    let row_stride = (physical_w as usize) * 4;
    for y in region_y_start..region_y_end {
        for x in region_x_start..region_x_end {
            let idx = (y as usize) * row_stride + (x as usize) * 4;
            let r = rgba[idx];
            let g = rgba[idx + 1];
            let b = rgba[idx + 2];
            // icon #d3d3d3 (211) ± 24 在 sRGB 编码后应 ~150-220 (gamma curve 内
            // mid-gray 解码值在 196 附近, encode 回是 #d3 = 211; sRGB-aware shader
            // 已预补偿). 用更宽松阈值 ≥ 100 (high-gray) 抓 icon stroke.
            let is_icon = r >= 100 && g >= 100 && b >= 100 && (r as i32 - g as i32).abs() <= 32;
            if is_icon {
                icon_pixels += 1;
            }
        }
    }

    // 三按钮 icon 总 stroke 像素估算:
    // - Close (+): 一横 ~ 36×4 + 一竖 ~ 4×36 = 144 + 144 = 288 (重叠中央 16 px)
    // - Maximize (□): 4 边各 36×4 ≈ 4 × 144 = 576 (角重叠少)
    // - Minimize (-): 一横 ~ 36×4 = 144
    // 总 ~ 1000+ icon pixels. 阈值用 200 防 sRGB / anti-alias 计数偏差.
    assert!(
        icon_pixels >= 200,
        "三按钮 icon 像素 {} < 200; 按钮没画? region {}×{}",
        icon_pixels,
        region_x_end - region_x_start,
        region_y_end - region_y_start
    );
}

/// 派单 In #G + 派单"三源 PNG verify deliverable": 写 /tmp/csd_test.png 到固定
/// 路径 (非 pid 后缀), 让 writer + Lead + reviewer 三方 Read 同一文件验视觉.
///
/// **why 单独 test 写固定路径**: 派单原文 "/tmp/csd_test.png 路径 (PNG 你自验
/// 视觉描述)". 其它 test 用 pid 后缀防并发, 本 test 专门给"deliverable" — 跑
/// 一次 cargo test --test csd_e2e --release 后 PNG 落 /tmp/csd_test.png, 三源
/// Read 用. test 间不冲突 (本 test 最后跑或单独跑均可, 文件覆盖即可).
#[test]
fn csd_writes_deliverable_png_to_fixed_path() {
    let (rgba, physical_w, physical_h) = render_csd_with_shell_prompt();

    use image::codecs::png::PngEncoder;
    use image::ExtendedColorType;
    use image::ImageEncoder;

    let path: PathBuf = "/tmp/csd_test.png".into();
    let file = std::fs::File::create(&path).expect("create /tmp/csd_test.png");
    let encoder = PngEncoder::new(file);
    encoder
        .write_image(&rgba, physical_w, physical_h, ExtendedColorType::Rgba8)
        .expect("PngEncoder write_image");

    let metadata = std::fs::metadata(&path).expect("PNG file metadata");
    assert!(
        metadata.len() > 4096,
        "/tmp/csd_test.png 大小 {} < 4 KiB (deliverable PNG 应至少 4 KiB)",
        metadata.len()
    );
    // 不删 — 派单要求 writer + Lead + reviewer Read 验视觉, 文件留给后续 step.
}
