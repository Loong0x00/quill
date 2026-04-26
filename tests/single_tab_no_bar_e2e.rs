//! T-0617 #B + #D 单 tab 隐藏 tab bar 端到端 PNG verify.
//!
//! 与 `multi_tab_e2e.rs` 同套路 — `render_headless` + `set_headless_tab_state`
//! 注入 tab_count, 抽 PNG 在 y = titlebar_h + 5 px 一行 (tab bar 应该所在的
//! y 范围内) 的像素颜色:
//! - 单 tab: 该 y 行应是 terminal bg (CLEAR 色, 与 tab bar bg #1c1c1c 不同)
//! - 多 tab: 该 y 行应是 tab bar bg (#1c1c1c)
//!
//! 派单 In #D 字面: "render_headless 单 tab → PNG → 抓 y=titlebar_h+5 px 一行,
//! RGB 应 == terminal bg (#1d1f21), 不是 tab bar bg".

use quill::term::{CellPos, CellRef, Color};
use quill::text::TextSystem;
use quill::wl::{render_headless, reset_headless_tab_state, set_headless_tab_state};

const HIDPI_SCALE: u32 = 2;
const COLS: usize = 80;
const ROWS: usize = 21;
const LOGICAL_W: u32 = 800;
const LOGICAL_H: u32 = 600;

const TITLEBAR_H_LOGICAL: usize = 28;

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

fn render_with_tab_count(tab_count: usize) -> (Vec<u8>, u32, u32) {
    set_headless_tab_state(tab_count, 0);
    let mut text_system = TextSystem::new().expect("TextSystem::new");
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
    reset_headless_tab_state();
    result
}

fn write_png(path: &str, rgba: &[u8], w: u32, h: u32) {
    use image::codecs::png::PngEncoder;
    use image::{ColorType, ImageEncoder};
    let file = std::fs::File::create(path).expect("create png");
    let encoder = PngEncoder::new(file);
    encoder
        .write_image(rgba, w, h, ColorType::Rgba8.into())
        .expect("png encode");
    eprintln!("writer-T0617 wrote PNG: {path}");
}

/// 在 (x, y) 物理像素位置取 RGBA. 越界返 None.
fn pixel_at(rgba: &[u8], physical_w: u32, x: u32, y: u32) -> Option<(u8, u8, u8, u8)> {
    let row_stride = (physical_w as usize) * 4;
    let idx = (y as usize) * row_stride + (x as usize) * 4;
    if idx + 3 >= rgba.len() {
        return None;
    }
    Some((rgba[idx], rgba[idx + 1], rgba[idx + 2], rgba[idx + 3]))
}

/// **T-0617 acceptance: 单 tab 时 tab bar 隐藏**.
/// 抓 y = titlebar_h + 5 px (本应是 tab bar 区域) 中段一行 RGB, 应 == terminal
/// bg (CLEAR_COLOR #1d1f21), 不是 tab bar bg (TAB_BAR_BG #1c1c1c).
///
/// **why 严格 == clear**: 两个颜色非常接近 (28 vs 29, 28 vs 31, 28 vs 33), 走
/// "≠ tab_bar_bg ±8" 阈值会被 corner mask / alpha 漂移撞翻. 改用"严格匹配
/// CLEAR ±2" 走单一来源 — 单 tab 视觉规则字面要求 cell area 直接接 titlebar
/// 下方, terminal bg = CLEAR_COLOR.
#[test]
fn single_tab_no_tab_bar_at_titlebar_below_y() {
    let (rgba, physical_w, physical_h) = render_with_tab_count(1);
    write_png(
        "/tmp/t0617_single_tab_no_bar.png",
        &rgba,
        physical_w,
        physical_h,
    );

    // 多 tab 时 tab bar 在 y = titlebar_h..titlebar_h+tab_bar_h. 单 tab 时此区
    // 应该已被 cell area / bg fill 占用, 应是 terminal bg #1d1f21.
    let probe_y = (TITLEBAR_H_LOGICAL * HIDPI_SCALE as usize) as u32 + 5;
    // 中段 x (避开圆角 corner mask 区) — surface 中心.
    let probe_x = physical_w / 2;
    let pixel = pixel_at(&rgba, physical_w, probe_x, probe_y).expect("pixel in range");
    eprintln!(
        "T-0617 single tab probe at ({probe_x},{probe_y}): RGBA = {:?}",
        pixel
    );

    // terminal bg (CLEAR) = #1d1f21 (29, 31, 33). 严格匹配 ±2 容忍 sRGB encode 漂移.
    let (r, g, b, _a) = pixel;
    let near_clear = (r as i32 - 0x1d).abs() <= 2
        && (g as i32 - 0x1f).abs() <= 2
        && (b as i32 - 0x21).abs() <= 2;
    assert!(
        near_clear,
        "T-0617 单 tab 时 y={probe_y} px (titlebar 下方 5 px) 应是 terminal bg #1d1f21 (cell area 接 titlebar 下方), got {:?}",
        pixel
    );
}

/// **回归保险 / 多 tab 路径**: tab_count >= 2 时同 y 抓应该看到 tab bar bg.
#[test]
fn multi_tab_shows_tab_bar_at_titlebar_below_y() {
    let (rgba, physical_w, physical_h) = render_with_tab_count(3);
    write_png(
        "/tmp/t0617_multi_tab_has_bar.png",
        &rgba,
        physical_w,
        physical_h,
    );

    let probe_y = (TITLEBAR_H_LOGICAL * HIDPI_SCALE as usize) as u32 + 5;
    // 中段 x: 避开 + 按钮 (左 28 logical) 与 tab body active 高亮 (idx=0).
    // surface_w = 1600 phys, mid = 800 → 落 active tab body / inactive tab body 区,
    // 对应 inactive tab 视觉 = 透明 (无圆角 box bg) → 看到 tab bar bg #1c1c1c.
    // active idx=0 的 tab body 高亮 #444 在 idx=0 区段, 我们抓 idx=2 (右段) 的
    // inactive 区: x ≈ plus_w + 2 * body_w = 56 + 2 * 400 = 856, 但 active=0
    // 默认 idx 0/1/2 → 抓 idx=2 中段 x ≈ 600 phys 的中段.
    // 实测 `multi_tab_e2e.rs` 同 800×600 surface 用 mid 抓 active bg, 这里抓
    // tab 间 4 px gap 区即 tab bar bg.
    // 简化: 找一个不是 active tab 圆角 box 的 x — 用最右 tab 的右 close 区前的
    // tab 间 gap 区 (~plus_w + 2.0 * body_w - gap = 56 + 2*200 - 4 = 452 logical
    // = 904 physical — clamp to surface).
    // 为简单起见 + 防像素细微对齐, 我们扫一行像素, 数 RGB 接近 #1c1c1c 的像素数,
    // 多 tab 应有大量, 单 tab 应有 0.
    // tab bar bg = #1c1c1c (28, 28, 28). 严格匹配 ±2 与 CLEAR (29, 31, 33) 区分.
    let mut tab_bar_bg_pixels: u32 = 0;
    let row_stride = (physical_w as usize) * 4;
    for x in 0..physical_w {
        let idx = (probe_y as usize) * row_stride + (x as usize) * 4;
        if idx + 3 < rgba.len() {
            let r = rgba[idx];
            let g = rgba[idx + 1];
            let b = rgba[idx + 2];
            if (r as i32 - 0x1c).abs() <= 2
                && (g as i32 - 0x1c).abs() <= 2
                && (b as i32 - 0x1c).abs() <= 2
            {
                tab_bar_bg_pixels += 1;
            }
        }
    }
    eprintln!(
        "T-0617 multi tab probe row y={}: tab_bar_bg pixels = {} / total {}",
        probe_y, tab_bar_bg_pixels, physical_w
    );
    // 期望: 多 tab 时 tab bar 横跨 surface 大部分宽度 (除 + 按钮 / active 圆角 /
    // close hover 红圆), 应至少 100 像素是 tab bar bg.
    assert!(
        tab_bar_bg_pixels >= 100,
        "多 tab 时 y={probe_y} 一行应有 >= 100 个 tab_bar_bg 像素, got {}",
        tab_bar_bg_pixels
    );
}

/// 单 tab 路径 **同一行** 应几乎无 tab bar bg 像素 — 与上 multi 测对称, 直接锁
/// "单 tab 不画 tab bar".
#[test]
fn single_tab_zero_tab_bar_bg_on_titlebar_below_row() {
    let (rgba, physical_w, _physical_h) = render_with_tab_count(1);
    let probe_y = (TITLEBAR_H_LOGICAL * HIDPI_SCALE as usize) as u32 + 5;
    let row_stride = (physical_w as usize) * 4;
    let mut tab_bar_bg_pixels: u32 = 0;
    for x in 0..physical_w {
        let idx = (probe_y as usize) * row_stride + (x as usize) * 4;
        if idx + 3 < rgba.len() {
            let r = rgba[idx];
            let g = rgba[idx + 1];
            let b = rgba[idx + 2];
            // 严格匹配 #1c1c1c ±2.
            if (r as i32 - 0x1c).abs() <= 2
                && (g as i32 - 0x1c).abs() <= 2
                && (b as i32 - 0x1c).abs() <= 2
            {
                tab_bar_bg_pixels += 1;
            }
        }
    }
    eprintln!(
        "T-0617 single tab row y={}: tab_bar_bg pixels = {}",
        probe_y, tab_bar_bg_pixels
    );
    // 单 tab 时这一行应是 CLEAR bg (#1d1f21) 不是 TAB_BAR_BG (#1c1c1c). 严格 0 或
    // 极少 (corner mask 边缘可能有小漂移, 极保守留 < 50).
    assert!(
        tab_bar_bg_pixels < 50,
        "单 tab 时 y={probe_y} 一行不该出现 tab_bar_bg, got {}",
        tab_bar_bg_pixels
    );
}

/// `hit_test_with_tabs` tab_count=1 时不返 TabBarPlus / Tab(idx) / TabClose(idx)
/// — 派单 In #B 红线 "单 tab 时 tab area 不存在".
#[test]
fn hit_test_with_tabs_single_tab_returns_text_area_in_former_tab_bar() {
    use quill::wl::HoverRegion;
    // y=35 (>= titlebar 28) 在原 tab bar 区, 单 tab 时应直接落 TextArea.
    let h = quill::wl::hit_test_with_tabs(100.0, 35.0, 800, 600, 1);
    assert_eq!(
        h,
        HoverRegion::TextArea,
        "T-0617 单 tab 时 y=35 应是 TextArea (无 tab bar)"
    );
    // 同一坐标 tab_count=2 (多 tab) → 应是 Tab(0) (派单 In #B 不破多 tab 行为).
    let h2 = quill::wl::hit_test_with_tabs(100.0, 35.0, 800, 600, 2);
    assert!(
        matches!(
            h2,
            HoverRegion::Tab(_) | HoverRegion::TabBarPlus | HoverRegion::TabClose(_)
        ),
        "T-0617 多 tab 时 y=35 应是 tab bar 区 (Tab/Plus/Close), got {:?}",
        h2
    );
}
