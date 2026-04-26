//! T-0607 集成测试: render_headless + SelectionState (Linear / Block) 模拟
//! 鼠标拖选, 写出 PNG (`/tmp/t0607_*.png`) 给 Lead + reviewer Read 自验视觉.
//!
//! **三源 PNG verify SOP** (audit/2026-04-25-T-0405-review.md): writer (本测
//! 试) + Lead + reviewer 三方独立 Read PNG 描述视觉, 任一不一致即 P0.
//!
//! **覆盖** (派单 In #D + Acceptance):
//! - Linear 单行: cursor (5..=12, 10) 高亮
//! - Linear 跨行: anchor (75, 8) → cursor (10, 12) 起点行右半 + 中间整行 +
//!   终点行左半
//! - Block: anchor (5, 8) → cursor (12, 12) 矩形 8×5 = 40 cell
//! - 无选区: 不画任何 selection bg (回归保护)
//!
//! **几何**: cell px logical 10×25, physical 20×50 (HIDPI×2). titlebar 28
//! logical = 56 phys. 选中 cell 涂 SELECTION_BG = #3e6e9e.
//!
//! **运行**: `cargo test --test selection_e2e --release`

use std::path::PathBuf;

use quill::term::{CellPos, CellRef, Color};
use quill::text::TextSystem;
use quill::wl::{
    render_headless, selected_cells_block, selected_cells_linear, SelectionMode, SelectionState,
    HIDPI_SCALE,
};

const LOGICAL_W: u32 = 800;
const LOGICAL_H: u32 = 600;
const COLS: usize = 80;
const ROWS: usize = 22;

const CELL_W_LOGICAL: usize = 10;
const CELL_H_LOGICAL: usize = 25;
const TITLEBAR_H_LOGICAL: usize = 28;

/// 构造空 cell array (全 ' ', bg=#0a1030 深蓝, fg=#d3d3d3 浅灰).
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

/// 计算与 SELECTION_BG (#3e6e9e) 的距离 — 用于检 "selection 蓝" 像素.
fn distance_from_selection_bg(rgba: &[u8], idx: usize) -> i32 {
    let r = rgba[idx] as i32;
    let g = rgba[idx + 1] as i32;
    let b = rgba[idx + 2] as i32;
    (r - 0x3e).abs() + (g - 0x6e).abs() + (b - 0x9e).abs()
}

fn count_selection_pixels(
    rgba: &[u8],
    physical_w: u32,
    x_range: (usize, usize),
    y_range: (usize, usize),
    threshold: i32,
) -> u32 {
    let row_stride = (physical_w as usize) * 4;
    let mut hits: u32 = 0;
    for y in y_range.0..y_range.1.min(rgba.len() / row_stride) {
        for x in x_range.0..x_range.1.min(physical_w as usize) {
            let idx = y * row_stride + x * 4;
            // sRGB 路径下亮度可能漂移, threshold 100 给余量
            if distance_from_selection_bg(rgba, idx) < threshold {
                hits += 1;
            }
        }
    }
    hits
}

/// 把 cell (col, line) 转 physical bbox (含 titlebar offset).
fn cell_phys_bbox(col: usize, line: usize) -> (usize, usize, usize, usize) {
    let scale = HIDPI_SCALE as usize;
    let x0 = col * CELL_W_LOGICAL * scale;
    let x1 = (col + 1) * CELL_W_LOGICAL * scale;
    let y0 = (line * CELL_H_LOGICAL + TITLEBAR_H_LOGICAL) * scale;
    let y1 = ((line + 1) * CELL_H_LOGICAL + TITLEBAR_H_LOGICAL) * scale;
    (x0, x1, y0, y1)
}

/// **T-0607 Linear 单行选区**: anchor (5, 10) → cursor (12, 10) 应在 cells
/// (5..=12, 10) 上画 SELECTION_BG. cell 物理尺寸 20×50, 8 cell × 1000 phys
/// px = 8000 phys px. 检 ≥ 80% 命中 selection 蓝.
#[test]
fn selection_linear_single_line_renders_selection_bg() {
    let mut text_system = TextSystem::new().expect("TextSystem::new");
    let cells = empty_cells(COLS, ROWS);
    let row_texts: Vec<String> = vec![String::new(); ROWS];

    let mut sel = SelectionState::new();
    sel.start(CellPos { col: 5, line: 10 }, SelectionMode::Linear);
    sel.update(CellPos { col: 12, line: 10 });
    let selected = selected_cells_linear(&sel, COLS);
    assert_eq!(selected.len(), 8, "Linear 5..=12 单行 应 8 cells");

    let (rgba, physical_w, physical_h) = render_headless(
        &mut text_system,
        &cells,
        COLS,
        ROWS,
        &row_texts,
        LOGICAL_W,
        LOGICAL_H,
        None,
        None,
        Some(&selected),
    )
    .expect("render_headless");

    let path = write_png(&rgba, physical_w, physical_h, "t0607_linear_single.png");
    eprintln!("writer-T0607 wrote PNG: {}", path.display());

    // 检 cell (5, 10) 中心区 (避开 sRGB 边缘像素) 应是 selection 色.
    let (x0, x1, y0, y1) = cell_phys_bbox(5, 10);
    let hits = count_selection_pixels(&rgba, physical_w, (x0, x1), (y0, y1), 50);
    let total = (x1 - x0) * (y1 - y0);
    eprintln!("Linear single-line selection_bg pixels at cell (5,10): {hits}/{total}");
    assert!(
        hits >= (total as u32) * 8 / 10,
        "cell (5,10) 应 ≥80% selection 色, got {hits}/{total}"
    );

    // 检 cell (12, 10) (右端) 也是 selection 色.
    let (x0, x1, y0, y1) = cell_phys_bbox(12, 10);
    let hits = count_selection_pixels(&rgba, physical_w, (x0, x1), (y0, y1), 50);
    let total = (x1 - x0) * (y1 - y0);
    assert!(
        hits >= (total as u32) * 8 / 10,
        "cell (12,10) 应 ≥80% selection 色, got {hits}/{total}"
    );

    // 检 cell (4, 10) (选区外) 不该是 selection 色.
    let (x0, x1, y0, y1) = cell_phys_bbox(4, 10);
    let hits = count_selection_pixels(&rgba, physical_w, (x0, x1), (y0, y1), 50);
    let total = (x1 - x0) * (y1 - y0);
    eprintln!("Cell (4,10) selection_bg hits (应低): {hits}/{total}");
    assert!(
        hits <= (total as u32) / 10,
        "cell (4,10) 选区外 不该是 selection 色, got {hits}/{total}"
    );
}

/// **T-0607 Linear 跨多行**: anchor (75, 8) → cursor (10, 12) Linear.
/// 起点行 8: cells (75..80) = 5; 中间行 9/10/11: 80×3 = 240; 终点行 12:
/// cells (0..=10) = 11. 总 256 cells.
#[test]
fn selection_linear_multi_line_spans_rows() {
    let mut text_system = TextSystem::new().expect("TextSystem::new");
    let cells = empty_cells(COLS, ROWS);
    let row_texts: Vec<String> = vec![String::new(); ROWS];

    let mut sel = SelectionState::new();
    sel.start(CellPos { col: 75, line: 8 }, SelectionMode::Linear);
    sel.update(CellPos { col: 10, line: 12 });
    let selected = selected_cells_linear(&sel, COLS);
    assert_eq!(selected.len(), 5 + 80 * 3 + 11, "Linear 跨行 总 cell 数");

    let (rgba, physical_w, physical_h) = render_headless(
        &mut text_system,
        &cells,
        COLS,
        ROWS,
        &row_texts,
        LOGICAL_W,
        LOGICAL_H,
        None,
        None,
        Some(&selected),
    )
    .expect("render_headless");

    let path = write_png(&rgba, physical_w, physical_h, "t0607_linear_multi.png");
    eprintln!("writer-T0607 wrote PNG: {}", path.display());

    // 中间行 (10) 整行应是 selection 色.
    let (x0, _x1, y0, y1) = cell_phys_bbox(0, 10);
    let row_x_end = COLS * CELL_W_LOGICAL * (HIDPI_SCALE as usize);
    let hits = count_selection_pixels(&rgba, physical_w, (x0, row_x_end), (y0, y1), 50);
    let total = row_x_end * (y1 - y0);
    eprintln!("Linear multi-line full mid row 10 selection_bg: {hits}/{total}");
    assert!(
        hits >= (total as u32) * 8 / 10,
        "中间行 10 应 ≥80% selection 色"
    );

    // 起点行 8 cell (75) 应被选中, cell (74) 不该.
    let (x0, x1, y0, y1) = cell_phys_bbox(75, 8);
    let hits = count_selection_pixels(&rgba, physical_w, (x0, x1), (y0, y1), 50);
    let total = (x1 - x0) * (y1 - y0);
    assert!(
        hits >= (total as u32) * 8 / 10,
        "cell (75,8) 起点行右半应选中"
    );
    let (x0, x1, y0, y1) = cell_phys_bbox(74, 8);
    let hits = count_selection_pixels(&rgba, physical_w, (x0, x1), (y0, y1), 50);
    let total = (x1 - x0) * (y1 - y0);
    assert!(
        hits <= (total as u32) / 10,
        "cell (74,8) 起点行左半 不该选中"
    );
}

/// **T-0607 Block 矩形选区**: anchor (5, 8) → cursor (12, 12) Block.
/// 矩形 (5..=12) × (8..=12) = 8 × 5 = 40 cells.
#[test]
fn selection_block_rectangle() {
    let mut text_system = TextSystem::new().expect("TextSystem::new");
    let cells = empty_cells(COLS, ROWS);
    let row_texts: Vec<String> = vec![String::new(); ROWS];

    let mut sel = SelectionState::new();
    sel.start(CellPos { col: 5, line: 8 }, SelectionMode::Block);
    sel.update(CellPos { col: 12, line: 12 });
    let selected = selected_cells_block(&sel, COLS, ROWS);
    assert_eq!(selected.len(), 8 * 5, "Block 矩形 8×5 = 40");

    let (rgba, physical_w, physical_h) = render_headless(
        &mut text_system,
        &cells,
        COLS,
        ROWS,
        &row_texts,
        LOGICAL_W,
        LOGICAL_H,
        None,
        None,
        Some(&selected),
    )
    .expect("render_headless");

    let path = write_png(&rgba, physical_w, physical_h, "t0607_block.png");
    eprintln!("writer-T0607 wrote PNG: {}", path.display());

    // 检角 cell (5, 8) 选中.
    let (x0, x1, y0, y1) = cell_phys_bbox(5, 8);
    let hits = count_selection_pixels(&rgba, physical_w, (x0, x1), (y0, y1), 50);
    let total = (x1 - x0) * (y1 - y0);
    assert!(
        hits >= (total as u32) * 8 / 10,
        "Block 角 cell (5,8) 应选中"
    );
    // 检角 cell (12, 12) 选中.
    let (x0, x1, y0, y1) = cell_phys_bbox(12, 12);
    let hits = count_selection_pixels(&rgba, physical_w, (x0, x1), (y0, y1), 50);
    let total = (x1 - x0) * (y1 - y0);
    assert!(
        hits >= (total as u32) * 8 / 10,
        "Block 角 cell (12,12) 应选中"
    );
    // 检矩形外 cell (4, 8) 不选.
    let (x0, x1, y0, y1) = cell_phys_bbox(4, 8);
    let hits = count_selection_pixels(&rgba, physical_w, (x0, x1), (y0, y1), 50);
    let total = (x1 - x0) * (y1 - y0);
    assert!(
        hits <= (total as u32) / 10,
        "Block 矩形外 cell (4,8) 不该选中"
    );
    // 检矩形外 cell (12, 13) 不选 (Block 不跨行流).
    let (x0, x1, y0, y1) = cell_phys_bbox(12, 13);
    let hits = count_selection_pixels(&rgba, physical_w, (x0, x1), (y0, y1), 50);
    let total = (x1 - x0) * (y1 - y0);
    assert!(
        hits <= (total as u32) / 10,
        "Block 矩形外 cell (12,13) 不该选中"
    );
}

/// **T-0607 无选区**: 传 None 给 render_headless 应不画任何 selection bg.
/// 回归保护: 防"selection_bg quad 未守门导致空选区也画"路径.
#[test]
fn selection_none_renders_no_selection_bg() {
    let mut text_system = TextSystem::new().expect("TextSystem::new");
    let cells = empty_cells(COLS, ROWS);
    let row_texts: Vec<String> = vec![String::new(); ROWS];

    let (rgba, physical_w, physical_h) = render_headless(
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
    .expect("render_headless");

    let path = write_png(&rgba, physical_w, physical_h, "t0607_no_selection.png");
    eprintln!("writer-T0607 wrote PNG: {}", path.display());

    // 任意 cell (例 (5, 10)) 不应是 selection 色.
    let (x0, x1, y0, y1) = cell_phys_bbox(5, 10);
    let hits = count_selection_pixels(&rgba, physical_w, (x0, x1), (y0, y1), 50);
    let total = (x1 - x0) * (y1 - y0);
    eprintln!("No-selection PNG cell (5,10) selection_bg hits (应近 0): {hits}/{total}");
    assert!(
        hits <= (total as u32) / 10,
        "无选区时 不该有 selection 色像素"
    );
}
