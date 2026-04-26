//! T-0601 集成测试: render_headless + CursorInfo 模拟 4 种光标形状, 写出 PNG
//! (`/tmp/cursor_test_*.png`) 给 reviewer + Lead Read 自验视觉.
//!
//! **三源 PNG verify SOP** (audit/2026-04-25-T-0405-review.md): writer (本测
//! 试) + reviewer + Lead 三方独立 Read PNG 描述视觉, 任一不一致即 P0.
//!
//! **覆盖** (派单 In #D + Acceptance):
//! - Block 在 cursor 位置画整 cell 实心方块 (主路径, 用户日常 daily-drive 看到)
//! - Underline 在 cursor cell 底部画横线 (DECSCUSR 3/4)
//! - Beam 在 cursor cell 左侧画竖线 (DECSCUSR 5/6)
//! - HollowBlock 在 cursor cell 画 4 边框 (失焦风格 / Phase 6+ focus-aware)
//! - visible=false 路径 (DECRST 25 / IME preedit) 不画任何 cursor 像素 (回归保护)
//!
//! **几何** (与 ime_preedit_render.rs 对照): cursor (col=5, line=10), cell px
//! logical (50, 250), physical (100, 500). 但 T-0504 起 cell 区起绘 y 加
//! TITLEBAR_H_LOGICAL_PX (28 logical, 56 phys) — 故 cursor cell 真物理 y 在
//! [500+56, 550+56) = [556, 606). 测试取该区检亮像素.
//!
//! **运行**: `cargo test --test cursor_render_e2e`

use std::path::PathBuf;

use quill::term::{CellPos, CellRef, Color};
use quill::text::TextSystem;
use quill::wl::{
    render_headless, CursorInfo, CursorStyle, CURSOR_INSET_PX, CURSOR_THICKNESS_PX, HIDPI_SCALE,
};

const LOGICAL_W: u32 = 800;
const LOGICAL_H: u32 = 600;
const PHYSICAL_W: u32 = LOGICAL_W * HIDPI_SCALE;
const PHYSICAL_H: u32 = LOGICAL_H * HIDPI_SCALE;
const COLS: usize = 80;
const ROWS: usize = 24;

// cell 几何 (logical → phys 换算硬编码与 render.rs CELL_W_PX / CELL_H_PX 同
// 源, 与 ime_preedit_render.rs 同套路 — 测试不引私有常数, 显式数字易读)。
const CELL_W_LOGICAL: usize = 10;
const CELL_H_LOGICAL: usize = 25;
const TITLEBAR_H_LOGICAL: usize = 28;

const CURSOR_COL: usize = 5;
const CURSOR_LINE: usize = 10;

/// 构造空 cell array (全 ' ' bg=#0a1030 深蓝, fg=#d3d3d3 浅灰), 与
/// ime_preedit_render.rs::empty_cells 同源.
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

/// distance to clear color #0a1030 (sum of |Δr|+|Δg|+|Δb|). Cursor 色 #ffffff
/// distance ~700, clear color 自身 0 — 阈值 100 / 200 区分明显 .
fn distance_from_clear(rgba: &[u8], idx: usize) -> i32 {
    let r = rgba[idx] as i32;
    let g = rgba[idx + 1] as i32;
    let b = rgba[idx + 2] as i32;
    (r - 0x0a).abs() + (g - 0x10).abs() + (b - 0x30).abs()
}

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

fn cursor_at(col: usize, line: usize, style: CursorStyle, visible: bool) -> CursorInfo {
    CursorInfo {
        col,
        line,
        visible,
        style,
        color: Color {
            r: 0xff,
            g: 0xff,
            b: 0xff,
        },
    }
}

/// cursor cell 物理像素范围 (含 titlebar offset). 返 (x_start, x_end, y_start, y_end).
fn cursor_cell_phys_bbox() -> (usize, usize, usize, usize) {
    let phys_x0 = CURSOR_COL * CELL_W_LOGICAL * HIDPI_SCALE as usize;
    let phys_x1 = (CURSOR_COL + 1) * CELL_W_LOGICAL * HIDPI_SCALE as usize;
    let phys_y0 = (CURSOR_LINE * CELL_H_LOGICAL + TITLEBAR_H_LOGICAL) * HIDPI_SCALE as usize;
    let phys_y1 = ((CURSOR_LINE + 1) * CELL_H_LOGICAL + TITLEBAR_H_LOGICAL) * HIDPI_SCALE as usize;
    (phys_x0, phys_x1, phys_y0, phys_y1)
}

fn count_bright_pixels(
    rgba: &[u8],
    physical_w: u32,
    x_range: (usize, usize),
    y_range: (usize, usize),
    threshold: i32,
) -> u32 {
    let row_stride = (physical_w as usize) * 4;
    let mut bright: u32 = 0;
    for y in y_range.0..y_range.1.min(rgba.len() / row_stride) {
        for x in x_range.0..x_range.1.min(physical_w as usize) {
            let idx = y * row_stride + x * 4;
            if distance_from_clear(rgba, idx) > threshold {
                bright += 1;
            }
        }
    }
    bright
}

/// **T-0601 主路径**: Block cursor 在 (5, 10) 应填满整 cell ~1000 物理 px
/// (cell 20×50 phys = 1000 px). 验该区有 >= 800 亮像素 (留余量防 sRGB rounding).
#[test]
fn cursor_block_renders_filled_cell() {
    let mut text_system = TextSystem::new().expect("TextSystem::new failed");
    let cells = empty_cells(COLS, ROWS);
    let row_texts: Vec<String> = vec![String::new(); ROWS];
    let cursor = cursor_at(CURSOR_COL, CURSOR_LINE, CursorStyle::Block, true);

    let (rgba, physical_w, physical_h) = render_headless(
        &mut text_system,
        &cells,
        COLS,
        ROWS,
        &row_texts,
        LOGICAL_W,
        LOGICAL_H,
        None,
        Some(&cursor),
        None, // T-0607 selection
    )
    .expect("render_headless block cursor failed");

    assert_eq!(physical_w, PHYSICAL_W);
    assert_eq!(physical_h, PHYSICAL_H);

    // 派单 deliverable: PNG 写出供 Lead / reviewer Read 自验
    let path = write_png(&rgba, physical_w, physical_h, "cursor_test_block.png");
    eprintln!("writer-T0601 wrote PNG: {}", path.display());

    let (x0, x1, y0, y1) = cursor_cell_phys_bbox();
    let bright = count_bright_pixels(&rgba, physical_w, (x0, x1), (y0, y1), 200);
    let cell_pixels = (x1 - x0) * (y1 - y0); // 20 × 50 = 1000
    eprintln!(
        "Block cursor bright pixels: {}/{} (cell phys px)",
        bright, cell_pixels
    );
    assert!(
        bright >= (cell_pixels as u32) * 8 / 10,
        "Block cursor 应填满 cell (>= 80% 亮像素), got {bright}/{cell_pixels}"
    );
}

/// Underline cursor: 仅 cell 底部 thickness × HIDPI_SCALE phys px 行有亮像素.
#[test]
fn cursor_underline_renders_bottom_strip() {
    let mut text_system = TextSystem::new().expect("TextSystem::new failed");
    let cells = empty_cells(COLS, ROWS);
    let row_texts: Vec<String> = vec![String::new(); ROWS];
    let cursor = cursor_at(CURSOR_COL, CURSOR_LINE, CursorStyle::Underline, true);

    let (rgba, physical_w, _physical_h) = render_headless(
        &mut text_system,
        &cells,
        COLS,
        ROWS,
        &row_texts,
        LOGICAL_W,
        LOGICAL_H,
        None,
        Some(&cursor),
        None, // T-0607 selection
    )
    .expect("render_headless underline cursor failed");

    let (x0, x1, _y0, y1) = cursor_cell_phys_bbox();
    let thickness_phys = CURSOR_THICKNESS_PX as usize * HIDPI_SCALE as usize; // 4
                                                                              // 底部 4 行
    let bottom_strip_y = (y1 - thickness_phys, y1);
    let bottom_bright = count_bright_pixels(&rgba, physical_w, (x0, x1), bottom_strip_y, 200);
    let strip_pixels = (x1 - x0) * thickness_phys; // 20 × 4 = 80
    eprintln!(
        "Underline bottom strip bright: {}/{}",
        bottom_bright, strip_pixels
    );
    assert!(
        bottom_bright >= (strip_pixels as u32) * 8 / 10,
        "Underline 底部 strip 应 >= 80% 亮, got {bottom_bright}/{strip_pixels}"
    );

    // 上半 cell (cell_y0 .. cell_y1 - thickness) 应几乎无亮像素 (clear color
    // depth 0; cell.bg #0a1030 与 clear 完全相同 distance=0)
    let upper_y = (y1 - 50, y1 - thickness_phys); // 上面 46 行
    let upper_bright = count_bright_pixels(&rgba, physical_w, (x0, x1), upper_y, 200);
    eprintln!("Underline upper region bright: {}", upper_bright);
    assert!(
        upper_bright < 20,
        "Underline 上半 cell 不应有亮像素 (< 20 防 anti-alias 边界); got {upper_bright}"
    );

    let path = write_png(&rgba, physical_w, _physical_h, "cursor_test_underline.png");
    eprintln!("writer-T0601 wrote PNG: {}", path.display());
}

/// Beam cursor: 仅 cell 左侧 thickness × HIDPI_SCALE phys px 列有亮像素.
#[test]
fn cursor_beam_renders_left_strip() {
    let mut text_system = TextSystem::new().expect("TextSystem::new failed");
    let cells = empty_cells(COLS, ROWS);
    let row_texts: Vec<String> = vec![String::new(); ROWS];
    let cursor = cursor_at(CURSOR_COL, CURSOR_LINE, CursorStyle::Beam, true);

    let (rgba, physical_w, _physical_h) = render_headless(
        &mut text_system,
        &cells,
        COLS,
        ROWS,
        &row_texts,
        LOGICAL_W,
        LOGICAL_H,
        None,
        Some(&cursor),
        None, // T-0607 selection
    )
    .expect("render_headless beam cursor failed");

    let (x0, _x1, y0, y1) = cursor_cell_phys_bbox();
    let thickness_phys = CURSOR_THICKNESS_PX as usize * HIDPI_SCALE as usize; // 4
                                                                              // T-0604: cursor cell x 内缩 inset_phys (= CURSOR_INSET_PX × HIDPI_SCALE)
                                                                              // 让字形溢出像素不被覆盖 — Beam 左侧竖线起点跟着内缩, strip 范围从
                                                                              // (x0+inset, x0+inset+thickness).
    let inset_phys = CURSOR_INSET_PX as usize * HIDPI_SCALE as usize; // 2
    let left_strip_x = (x0 + inset_phys, x0 + inset_phys + thickness_phys);
    let left_bright = count_bright_pixels(&rgba, physical_w, left_strip_x, (y0, y1), 200);
    let strip_pixels = thickness_phys * (y1 - y0); // 4 × 50 = 200
    eprintln!("Beam left strip bright: {}/{}", left_bright, strip_pixels);
    assert!(
        left_bright >= (strip_pixels as u32) * 8 / 10,
        "Beam 左侧 strip 应 >= 80% 亮, got {left_bright}/{strip_pixels}"
    );

    let path = write_png(&rgba, physical_w, _physical_h, "cursor_test_beam.png");
    eprintln!("writer-T0601 wrote PNG: {}", path.display());
}

/// **回归保护**: visible=false (DECRST 25 / IME preedit) 不应画任何 cursor
/// 像素 — cursor cell 区域应全是 clear color (cell.bg = clear color #0a1030).
#[test]
fn cursor_invisible_renders_nothing() {
    let mut text_system = TextSystem::new().expect("TextSystem::new failed");
    let cells = empty_cells(COLS, ROWS);
    let row_texts: Vec<String> = vec![String::new(); ROWS];
    let cursor = cursor_at(CURSOR_COL, CURSOR_LINE, CursorStyle::Block, false);

    let (rgba, physical_w, _physical_h) = render_headless(
        &mut text_system,
        &cells,
        COLS,
        ROWS,
        &row_texts,
        LOGICAL_W,
        LOGICAL_H,
        None,
        Some(&cursor),
        None, // T-0607 selection
    )
    .expect("render_headless invisible cursor failed");

    let (x0, x1, y0, y1) = cursor_cell_phys_bbox();
    let bright = count_bright_pixels(&rgba, physical_w, (x0, x1), (y0, y1), 100);
    eprintln!("Invisible cursor bright pixels: {}", bright);
    assert!(
        bright < 5,
        "visible=false 时 cursor cell 不应有亮像素 (< 5 防 sRGB 边界); got {bright}"
    );
}

/// HollowBlock: 4 边框, 中心区域 (内部 thickness 之外) 应 cell.bg 色无亮像素.
#[test]
fn cursor_hollow_block_renders_4_borders() {
    let mut text_system = TextSystem::new().expect("TextSystem::new failed");
    let cells = empty_cells(COLS, ROWS);
    let row_texts: Vec<String> = vec![String::new(); ROWS];
    let cursor = cursor_at(CURSOR_COL, CURSOR_LINE, CursorStyle::HollowBlock, true);

    let (rgba, physical_w, _physical_h) = render_headless(
        &mut text_system,
        &cells,
        COLS,
        ROWS,
        &row_texts,
        LOGICAL_W,
        LOGICAL_H,
        None,
        Some(&cursor),
        None, // T-0607 selection
    )
    .expect("render_headless hollow_block cursor failed");

    let (x0, x1, y0, y1) = cursor_cell_phys_bbox();
    let thickness_phys = CURSOR_THICKNESS_PX as usize * HIDPI_SCALE as usize; // 4
                                                                              // T-0604: cursor cell x 方向内缩 inset_phys, HollowBlock 4 边框 left / right
                                                                              // strip 起点 / 终点跟内缩走 (top / bottom 仍跨整个 cell 宽减 inset, 但因内
                                                                              // 缩量 inset_phys = 2 phys vs cell width 20 phys ≈ 10% 损失, top/bottom 视
                                                                              // strip 还是从 x0..x1 但 strip 宽内可能边缘 inset 区缺像素 — 阈值放宽 70%).
    let inset_phys = CURSOR_INSET_PX as usize * HIDPI_SCALE as usize; // 2
    let inner_x0 = x0 + inset_phys;
    let inner_x1 = x1 - inset_phys;

    // 4 边各自应 >= 80% 亮像素 (top / bottom / left / right strip).
    // top / bottom strip 取内缩后的 x 范围 (inner_x0..inner_x1, T-0604 inset).
    let top_bright = count_bright_pixels(
        &rgba,
        physical_w,
        (inner_x0, inner_x1),
        (y0, y0 + thickness_phys),
        200,
    );
    let bottom_bright = count_bright_pixels(
        &rgba,
        physical_w,
        (inner_x0, inner_x1),
        (y1 - thickness_phys, y1),
        200,
    );
    let left_bright = count_bright_pixels(
        &rgba,
        physical_w,
        (inner_x0, inner_x0 + thickness_phys),
        (y0, y1),
        200,
    );
    let right_bright = count_bright_pixels(
        &rgba,
        physical_w,
        (inner_x1 - thickness_phys, inner_x1),
        (y0, y1),
        200,
    );

    let edge_top_bottom = ((inner_x1 - inner_x0) * thickness_phys) as u32;
    let edge_left_right = (thickness_phys * (y1 - y0)) as u32; // 200

    eprintln!(
        "HollowBlock top={} bot={} left={} right={}",
        top_bright, bottom_bright, left_bright, right_bright
    );
    assert!(top_bright >= edge_top_bottom * 8 / 10, "top edge sparse");
    assert!(
        bottom_bright >= edge_top_bottom * 8 / 10,
        "bottom edge sparse"
    );
    assert!(left_bright >= edge_left_right * 8 / 10, "left edge sparse");
    assert!(
        right_bright >= edge_left_right * 8 / 10,
        "right edge sparse"
    );

    // 中心 inset (cell 内部去掉 inset + thickness 边框) 应几乎无亮像素 — 验
    // "hollow". T-0604: x 方向加 inset_phys 偏移与 cursor cell 内缩同步。
    let inner_x = (inner_x0 + thickness_phys, inner_x1 - thickness_phys);
    let inner_y = (y0 + thickness_phys, y1 - thickness_phys);
    let inner_bright = count_bright_pixels(&rgba, physical_w, inner_x, inner_y, 200);
    eprintln!("HollowBlock inner region bright: {}", inner_bright);
    assert!(
        inner_bright < 20,
        "HollowBlock 中心 inset 应几乎无亮像素 (验 hollow); got {inner_bright}"
    );

    let path = write_png(&rgba, physical_w, _physical_h, "cursor_test_hollow.png");
    eprintln!("writer-T0601 wrote PNG: {}", path.display());
}

/// **回归保护**: cursor=None (派单允许的 None 入参, --headless-screenshot CLI
/// 路径 + 老测试都走 None) — cursor cell 应无亮像素.
#[test]
fn no_cursor_means_no_cursor_pixels() {
    let mut text_system = TextSystem::new().expect("TextSystem::new failed");
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
        None,
        None, // T-0607 selection
    )
    .expect("render_headless no cursor failed");

    let (x0, x1, y0, y1) = cursor_cell_phys_bbox();
    let bright = count_bright_pixels(&rgba, physical_w, (x0, x1), (y0, y1), 100);
    assert!(bright < 5, "cursor=None 时不应画 cursor 像素; got {bright}");
}
