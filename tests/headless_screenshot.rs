//! T-0408 集成测试: `--headless-screenshot` CLI flag 端到端 smoke + 像素验。
//!
//! **why 集成测试 (而非 unit)**: 走真 wgpu offscreen Texture 路径 (NVIDIA Vulkan
//! 5090, 跟 cargo run --release 同栈), 没法 mock device/queue。CI 路径需要 GPU
//! (Linux + Vulkan), 本项目目标用户单机有, CI 由 Lead 决定是否 gate (T-0407
//! face lock 后视觉稳定, 当下集成测无 flaky)。
//!
//! **覆盖**:
//! - PNG 文件存在 + 尺寸 (派单 In #D test 1)
//! - 字像素: prompt 区域至少有非深蓝像素 (派单 In #D test 2)
//! - 无彩色 emoji artifact: 全 frame 像素分布限制在 grayscale + 深蓝小集合
//!   (派单 In #D test 3)
//!
//! **运行**: `cargo test --test headless_screenshot`

use std::path::PathBuf;

use quill::pty::PtyHandle;
use quill::term::TermState;
use quill::text::TextSystem;
use quill::wl::{render_headless, HIDPI_SCALE};

/// logical (输入 render_headless 的 width/height)。
const LOGICAL_W: u32 = 800;
const LOGICAL_H: u32 = 600;
/// physical (PNG 实际尺寸 = logical × HIDPI_SCALE, T-0404 锁 2)。
const PHYSICAL_W: u32 = LOGICAL_W * HIDPI_SCALE;
const PHYSICAL_H: u32 = LOGICAL_H * HIDPI_SCALE;
const COLS: u16 = 80;
const ROWS: u16 = 24;
const PROMPT_WAIT_MS: u64 = 500;

/// 跑一遍跟 `src/main.rs::run_headless_screenshot` 相同的 PTY → Term →
/// render_headless 链路, 返 `(rgba, physical_w, physical_h)`。3 个测试共享
/// 此 helper, 避免重复 spawn shell + 等 prompt 的样板。
///
/// 失败路径用 `panic!` (test 路径允许 — `tests/` 不是 main / lib 代码,
/// CLAUDE.md "禁用 unwrap/expect" 仅约束 src/, 见 conventions.md §3 "真 IO
/// 测试允许 std::thread::sleep")。
fn render_headless_with_shell_prompt() -> (Vec<u8>, u32, u32) {
    let mut text_system = TextSystem::new().expect("TextSystem::new failed (need monospace font)");

    let mut term = TermState::new(COLS, ROWS);
    let mut pty =
        PtyHandle::spawn_shell(COLS, ROWS).expect("PtyHandle::spawn_shell(80, 24) failed");

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
        None, // T-0601: shell prompt 截图无光标, 与 --headless-screenshot CLI 同,
        None, // T-0607 selection
    )
    .expect("render_headless failed")
}

/// PNG 文件成功写盘 + decode 后尺寸正确 (T-0404: physical = logical × HIDPI_SCALE)
/// + 文件 byte size 合理 (>4 KiB, 8-bit RGBA 1600x1200 全清屏 deflate 后通常
///   10-100 KB)。
#[test]
fn headless_renders_prompt_to_png() {
    use image::codecs::png::PngEncoder;
    use image::ExtendedColorType;
    use image::ImageEncoder;

    let (rgba, physical_w, physical_h) = render_headless_with_shell_prompt();
    assert_eq!(physical_w, PHYSICAL_W);
    assert_eq!(physical_h, PHYSICAL_H);
    assert_eq!(
        rgba.len(),
        (PHYSICAL_W as usize) * (PHYSICAL_H as usize) * 4,
        "RGBA byte len should be physical_w*physical_h*4"
    );

    let mut tmp_path = std::env::temp_dir();
    tmp_path.push(format!("quill_t0408_test_{}.png", std::process::id()));
    let path: PathBuf = tmp_path;

    {
        let file = std::fs::File::create(&path).expect("create PNG file");
        let encoder = PngEncoder::new(file);
        encoder
            .write_image(&rgba, physical_w, physical_h, ExtendedColorType::Rgba8)
            .expect("PngEncoder write_image");
    }

    let metadata = std::fs::metadata(&path).expect("PNG file metadata");
    assert!(
        metadata.len() > 4096,
        "PNG file size {} should be > 4 KiB (sanity: deflated 1600x1200 RGBA \
         normally 10-100 KiB)",
        metadata.len()
    );

    // PNG signature: 89 50 4E 47 0D 0A 1A 0A
    let header = std::fs::read(&path).expect("read PNG header");
    assert!(
        header.len() >= 8 && &header[..8] == b"\x89PNG\r\n\x1a\n",
        "PNG header signature mismatch"
    );

    // decode round-trip 尺寸校验 (PNG 实际尺寸 = physical, T-0404 ×2)
    let img = image::load_from_memory(&header).expect("decode PNG");
    assert_eq!(
        img.width(),
        PHYSICAL_W,
        "decoded PNG width should be physical"
    );
    assert_eq!(
        img.height(),
        PHYSICAL_H,
        "decoded PNG height should be physical"
    );

    let _ = std::fs::remove_file(&path);
}

/// prompt 区域 (左上 400×120 physical px, 覆盖 [user@userPC ~]$ 字符) 至少
/// 有一些非深蓝像素 — 字真的画了。T-0404 × HIDPI_SCALE 后 prompt 字号也 ×2
/// (cosmic-text Metrics 17→34), 区域同步从 logical 200×60 变 physical 400×120。
/// 不写绝对色比对 (浮点 sRGB 转换有数值漂移), 用 "至少 X 个像素跟 clear color
/// 不同" 作 sanity gate。
#[test]
fn headless_png_pixels_have_glyph_on_dark_bg() {
    let (rgba, physical_w, _physical_h) = render_headless_with_shell_prompt();

    // 取左上 prompt 区域 (T-0404 后 ×2): 400×120 physical px ~ 17 字符 prompt
    // × CELL_W_PX 10 × 2 = 340 px + 余量, 高度覆盖第一行 25 × 2 = 50 px + 余量
    let region_w: u32 = 400;
    let region_h: u32 = 120;

    let mut non_clear_pixels: u32 = 0;
    let row_stride = (physical_w as usize) * 4;
    for y in 0..(region_h as usize) {
        for x in 0..(region_w as usize) {
            let idx = y * row_stride + x * 4;
            let r = rgba[idx];
            let g = rgba[idx + 1];
            let b = rgba[idx + 2];
            // clear 色是 #0a1030 sRGB; allow ±8 浮点 / sRGB encode 误差
            let near_clear = (r as i32 - 0x0a).abs() <= 8
                && (g as i32 - 0x10).abs() <= 8
                && (b as i32 - 0x30).abs() <= 8;
            if !near_clear {
                non_clear_pixels += 1;
            }
        }
    }

    // 400 像素是宽松下限 — T-0404 ×2 后字形面积 ×4, 17 字符 prompt × ~24 px
    // 笔画总像素 ≈ 400+。视觉无字时 0 像素, 这里 400 是 robust 阈值 (logical
    // 100 ×4 = 400) 防 CI 抖动。
    assert!(
        non_clear_pixels >= 400,
        "prompt 区域 {}×{} (physical) 应有至少 400 个非清屏像素 (字形真画了), 实测 {}",
        region_w,
        region_h,
        non_clear_pixels
    );
}

/// 全 frame 像素 RGB 分布限制 — 不应出现"非灰阶 / 非深蓝 / 非 fg-gray" 大块
/// 区域。Phase 4 视觉 contract: cell.bg = #000000 黑 (深蓝 clear 上几乎不可见),
/// cell.fg = #d3d3d3 light gray, glyph fg 同 light gray; **不应有彩色** (T-0403
/// emoji bug 真因 — Noto Color Emoji 渲染会出现红/绿/黄等)。
///
/// 检测路径: 把每像素分类成 "near_dark_blue" (clear) / "near_gray" (fg / cursor) /
/// "other"; "other" 比例应极小 (< 1% of frame), 因为 sRGB anti-aliasing 边界
/// 像素属于过渡色, 数量有限。若 "other" 超过 5% 说明大概率出现彩色 artifact。
#[test]
fn headless_png_no_emoji_color_artifact() {
    let (rgba, physical_w, physical_h) = render_headless_with_shell_prompt();

    let mut other_pixels: u32 = 0;
    let total_pixels = physical_w * physical_h;
    for chunk in rgba.chunks_exact(4) {
        let r = chunk[0];
        let g = chunk[1];
        let b = chunk[2];

        // near_dark_blue: clear color #0a1030 ± 12
        let near_dark_blue = (r as i32 - 0x0a).abs() <= 12
            && (g as i32 - 0x10).abs() <= 12
            && (b as i32 - 0x30).abs() <= 12;

        // near_gray: |r - g| 与 |g - b| 都小, 且 r+g+b 适中 (排除黑 / 白)
        let max_chan = r.max(g).max(b) as i32;
        let min_chan = r.min(g).min(b) as i32;
        let near_gray = (max_chan - min_chan) <= 24;

        // near_pure_black: 所有通道 < 16 (cell.bg = #000000 在 sRGB encode 后
        // 几乎为 0, alpha-blend 边界上有少量 dark mix)
        let near_black = r < 16 && g < 16 && b < 16;

        if !near_dark_blue && !near_gray && !near_black {
            other_pixels += 1;
        }
    }

    let other_ratio = (other_pixels as f64) / (total_pixels as f64);
    // 5% 是"明显彩色 artifact"阈值。T-0407 face lock 后视觉稳定, 实测此值
    // 应 < 1%; 留 5% 余量给字形 anti-alias 边界过渡像素 (sRGB → linear 转
    // 换数值漂移可能让某些边界像素超出 ±12 / ±24 容差) — 防 flaky CI。
    assert!(
        other_ratio < 0.05,
        "彩色 artifact 检测: other 像素 {}/{} = {:.2}% > 5%, T-0403 emoji \
         regression? T-0407 face lock 应阻止此路径",
        other_pixels,
        total_pixels,
        other_ratio * 100.0
    );
}
