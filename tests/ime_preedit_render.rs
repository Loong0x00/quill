//! T-0505 集成测试: render_headless + PreeditOverlay 模拟 IME preedit 状态,
//! 写出 PNG (`/tmp/ime_test.png`) 给 reviewer + Lead Read 自验视觉。
//!
//! **三源 PNG verify SOP** (audit/2026-04-25-T-0405-review.md): writer (本测
//! 试) + reviewer + Lead 三方独立 Read PNG 描述视觉, 任一不一致即 P0。
//!
//! **覆盖**:
//! - preedit text 在 cursor 当前 cell 之后绘制 (派单 In #D)
//! - 底部下划线像素可见 (派单 In #D PREEDIT_UNDERLINE_PX = 2 logical → 4 phys)
//! - 无 preedit 时 (None) 路径不画下划线 (回归保护)
//!
//! **不真起 fcitx5**: 用 PreeditOverlay 直接传 ImeState.current_preedit() 等价值。
//!
//! **运行**: `cargo test --test ime_preedit_render`

use std::path::PathBuf;

use quill::term::{CellPos, CellRef, Color};
use quill::text::TextSystem;
use quill::wl::{render_headless, PreeditOverlay, HIDPI_SCALE};

const LOGICAL_W: u32 = 800;
const LOGICAL_H: u32 = 600;
const PHYSICAL_W: u32 = LOGICAL_W * HIDPI_SCALE;
const PHYSICAL_H: u32 = LOGICAL_H * HIDPI_SCALE;
const COLS: usize = 80;
const ROWS: usize = 24;

/// 构造空 cell array (全 ' ' bg=#0a1030 深蓝, fg=#d3d3d3 浅灰), 与
/// `tests/atlas_clear_on_full.rs::empty_cell_refs` 同源。
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
    let mut cells = Vec::with_capacity(cols * rows);
    for line in 0..rows {
        for col in 0..cols {
            cells.push(CellRef {
                pos: CellPos { col, line },
                c: ' ',
                fg,
                bg,
            });
        }
    }
    cells
}

/// 把 RGBA byte 距 clear color #0a1030 (深蓝) 多远。0 = 完全相同, 大 = 偏离 (
/// preedit 字像素 / 下划线像素).
fn distance_from_clear(rgba: &[u8], idx: usize) -> i32 {
    let r = rgba[idx] as i32;
    let g = rgba[idx + 1] as i32;
    let b = rgba[idx + 2] as i32;
    (r - 0x0a).abs() + (g - 0x10).abs() + (b - 0x30).abs()
}

/// 写 PNG 到 /tmp/ime_test.png (与派单 In #H 路径一致). 失败 panic 测试失败。
fn write_png(rgba: &[u8], physical_w: u32, physical_h: u32, name: &str) -> PathBuf {
    use image::codecs::png::PngEncoder;
    use image::ExtendedColorType;
    use image::ImageEncoder;

    let mut path = std::env::temp_dir();
    path.push(name);
    let file = std::fs::File::create(&path).expect("create png file");
    let encoder = PngEncoder::new(file);
    encoder
        .write_image(rgba, physical_w, physical_h, ExtendedColorType::Rgba8)
        .expect("png encode");
    path
}

/// **T-0505 主路径**: render preedit "你好" 在 cursor (col=5, line=10) 后, 写
/// /tmp/ime_test.png; 验下划线区域有非 clear-color 像素 (白色 underline).
#[test]
fn preedit_overlay_renders_underline_to_png() {
    let mut text_system = TextSystem::new().expect("TextSystem::new failed (need monospace font)");
    let cells = empty_cells(COLS, ROWS);
    let row_texts: Vec<String> = vec![String::new(); ROWS];

    let preedit = PreeditOverlay {
        text: "你好".into(),
        cursor_col: 5,
        cursor_line: 10,
    };

    let (rgba, physical_w, physical_h) = render_headless(
        &mut text_system,
        &cells,
        COLS,
        ROWS,
        &row_texts,
        LOGICAL_W,
        LOGICAL_H,
        Some(&preedit),
        None, // T-0601: 本测试聚焦 preedit, 不画 cursor,
        None, // T-0607 selection
    )
    .expect("render_headless preedit failed");

    assert_eq!(physical_w, PHYSICAL_W);
    assert_eq!(physical_h, PHYSICAL_H);
    assert_eq!(
        rgba.len(),
        (PHYSICAL_W as usize) * (PHYSICAL_H as usize) * 4
    );

    // 写 PNG (T-0505 派单 deliverable: /tmp/ime_test.png)
    let path = write_png(&rgba, physical_w, physical_h, "ime_test.png");
    eprintln!("writer-T0505 wrote PNG: {}", path.display());

    // 验证 underline 区域 (cursor cell 底部 4 phys px) 有非 clear-color 像素。
    // cursor (5, 10) → cell px logical (50, 250), physical (100, 500).
    // cell h logical 25 → phys 50; underline 在 cell 底部, y ∈ [500+50-4, 500+50)
    // = [546, 550)。preedit "你好" 2 char × 2 cell wide = 4 cell logical wide
    // ≈ 80 logical px → 160 physical px。x ∈ [100, 100+80)（按 char_count=2 算 2
    // cell 但实际中文 1 char = 2 cell wide; 我们 char count 在 underline_to_cell_bytes
    // 用 chars().count() = 2 char → 2 cell 宽度 underline）
    //
    // why 用 4 cell 而不是 2: 派单 In #D char 数即 cell 数 (Phase 5 KISS, 不接
    // east-asian width 表). "你好" 2 chars → 2 cells underline = 100..120 logical
    // → 200..240 physical. 验该区有亮像素即可。

    let row_stride = (physical_w as usize) * 4;
    let mut underline_bright_pixels: u32 = 0;

    // physical underline y range: cell_line=10, cell_h_phys=50, underline phys=4
    // y ∈ [10*50+50-4, 10*50+50) = [546, 550)
    // Phase 4/5 cell px hardcode: CELL_H_PX=25 logical → 50 physical
    let underline_y_start = (10 * 25 + 25 - 2) as usize * 2; // logical 273 → phys 546
    let underline_y_end = (10 * 25 + 25) as usize * 2; // logical 275 → phys 550
    let underline_x_start = (5 * 10) as usize * 2; // logical 50 → phys 100
    let underline_x_end = ((5 + 2) * 10) as usize * 2; // 2 chars × 10 logical → phys 140

    for y in underline_y_start..underline_y_end.min(physical_h as usize) {
        for x in underline_x_start..underline_x_end.min(physical_w as usize) {
            let idx = y * row_stride + x * 4;
            // underline 颜色 #ffffff (非 clear color #0a1030); distance > 100
            // 即明显非清屏。
            if distance_from_clear(&rgba, idx) > 100 {
                underline_bright_pixels += 1;
            }
        }
    }

    eprintln!(
        "underline region [{}..{}) × [{}..{}): {} bright pixels",
        underline_x_start,
        underline_x_end,
        underline_y_start,
        underline_y_end,
        underline_bright_pixels
    );

    assert!(
        underline_bright_pixels >= 50,
        "preedit underline 区应有 >= 50 非 clear-color 像素 (4 phys px tall × 40 phys \
         wide ≈ 160 px 满, 50 是宽松下限防 anti-alias 边界); got {underline_bright_pixels}"
    );

    // 还要验 preedit 字像素也存在 (preedit "你好" glyph 在 underline 上方):
    // cell vertical center ≈ phys y in (10*50+5, 10*50+45) = (505, 545)
    let glyph_y_start = (10 * 25 + 5) as usize * 2; // logical 255 → phys 510
    let glyph_y_end = (10 * 25 + 22) as usize * 2; // logical 272 → phys 544
    let glyph_x_start = (5 * 10) as usize * 2;
    let glyph_x_end = ((5 + 2) * 10) as usize * 2;

    let mut glyph_pixels: u32 = 0;
    for y in glyph_y_start..glyph_y_end.min(physical_h as usize) {
        for x in glyph_x_start..glyph_x_end.min(physical_w as usize) {
            let idx = y * row_stride + x * 4;
            if distance_from_clear(&rgba, idx) > 100 {
                glyph_pixels += 1;
            }
        }
    }
    eprintln!("preedit glyph region: {} bright pixels", glyph_pixels);
    assert!(
        glyph_pixels >= 30,
        "preedit '你好' 字像素应 >= 30 (CJK 笔画密集, 在 cursor 后那段); got {glyph_pixels}"
    );
}

/// **回归保护**: preedit=None 时 (无 IME 状态) 不应画下划线像素 — cursor
/// cell 底部全是 clear color (深蓝)。
#[test]
fn no_preedit_means_no_underline() {
    let mut text_system = TextSystem::new().expect("TextSystem::new failed (need monospace font)");
    let cells = empty_cells(COLS, ROWS);
    let row_texts: Vec<String> = vec![String::new(); ROWS];

    let (rgba, physical_w, _physical_h) = render_headless(
        &mut text_system,
        &cells,
        COLS,
        ROWS,
        &row_texts,
        LOGICAL_W,
        LOGICAL_H,
        None,
        None, // T-0601: 本测试是 preedit=None 回归保护, cursor 也设 None,
        None, // T-0607 selection
    )
    .expect("render_headless no preedit failed");

    let row_stride = (physical_w as usize) * 4;
    // 同 cursor cell 底部区域 — 应全是 clear color (depth = 0 ± 几 because
    // sRGB rounding)
    let underline_y_start = (10 * 25 + 23) as usize * 2;
    let underline_y_end = (10 * 25 + 25) as usize * 2;
    let underline_x_start = (5 * 10) as usize * 2;
    let underline_x_end = ((5 + 2) * 10) as usize * 2;

    let mut bright_pixels: u32 = 0;
    for y in underline_y_start..underline_y_end {
        for x in underline_x_start..underline_x_end {
            let idx = y * row_stride + x * 4;
            if distance_from_clear(&rgba, idx) > 100 {
                bright_pixels += 1;
            }
        }
    }
    assert!(
        bright_pixels < 5,
        "无 preedit 时 cursor 底部不应有 underline 亮像素 (< 5 防 sRGB 边界); \
         got {bright_pixels}"
    );
}

/// 空 preedit (text="") 等价 None: 不画 underline。
#[test]
fn empty_preedit_text_means_no_underline() {
    let mut text_system = TextSystem::new().expect("TextSystem::new failed (need monospace font)");
    let cells = empty_cells(COLS, ROWS);
    let row_texts: Vec<String> = vec![String::new(); ROWS];

    let preedit = PreeditOverlay {
        text: String::new(),
        cursor_col: 5,
        cursor_line: 10,
    };

    let (rgba, physical_w, _physical_h) = render_headless(
        &mut text_system,
        &cells,
        COLS,
        ROWS,
        &row_texts,
        LOGICAL_W,
        LOGICAL_H,
        Some(&preedit),
        None, // T-0601: 本测试聚焦 empty preedit no-op, cursor 也设 None,
        None, // T-0607 selection
    )
    .expect("render_headless empty preedit failed");

    let row_stride = (physical_w as usize) * 4;
    let underline_y_start = (10 * 25 + 23) as usize * 2;
    let underline_y_end = (10 * 25 + 25) as usize * 2;
    let underline_x_start = (5 * 10) as usize * 2;
    let underline_x_end = ((5 + 2) * 10) as usize * 2;

    let mut bright: u32 = 0;
    for y in underline_y_start..underline_y_end {
        for x in underline_x_start..underline_x_end {
            let idx = y * row_stride + x * 4;
            if distance_from_clear(&rgba, idx) > 100 {
                bright += 1;
            }
        }
    }
    assert!(bright < 5, "空 preedit text 不应画 underline; got {bright}");
}
