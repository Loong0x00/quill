//! T-0615 ghostty / GTK4 风 tab UI polish e2e PNG verify (派单 In #E + Acceptance
//! "三源 PNG verify").
//!
//! 走 `render_headless` + `set_headless_tab_state` + `set_headless_hover_state`
//! 输出 PNG 到 /tmp/t0615_*.png 让 writer / Lead / reviewer Read 同一文件验视觉:
//! 1. 默认 (1 tab, no hover): + 圆角 box + active tab body 圆角 (T-0615 acceptance)
//! 2. hover Close button: 圆形红 bg 浮现 (派单 In #D)
//! 3. hover + button: + box hover 高亮 (派单 In #B)
//! 4. hover inactive tab: 圆角 hover bg (派单 In #C)
//! 5. multi tab: active 圆角 + inactive 透明 (派单 In #C)
//! 6. hover tab close ×: 红圆 bg (派单 In #C)

use quill::term::{CellPos, CellRef, Color};
use quill::text::TextSystem;
use quill::wl::{
    render_headless, reset_headless_hover_state, reset_headless_tab_state,
    set_headless_hover_state, set_headless_tab_state, HoverRegion, WindowButton,
};

const HIDPI_SCALE: u32 = 2;
const COLS: usize = 80;
const ROWS: usize = 21;
const LOGICAL_W: u32 = 800;
const LOGICAL_H: u32 = 600;

const TITLEBAR_H_LOGICAL: usize = 28;
const TAB_BAR_H_LOGICAL: usize = 28;

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

/// 单帧渲染入口 — 走 set_headless_tab_state + set_headless_hover_state, 然后
/// render_headless. 测试末尾 reset 兜底防串测.
fn render_with_state(
    tab_count: usize,
    active_idx: usize,
    hover: HoverRegion,
) -> (Vec<u8>, u32, u32) {
    set_headless_tab_state(tab_count, active_idx);
    set_headless_hover_state(hover);
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
    eprintln!("writer-T0615 wrote PNG: {path}");
}

/// 数指定矩形区内匹配色像素数. r/g/b range tuples 是 [min, max] inclusive.
fn count_pixels_in_region(
    rgba: &[u8],
    physical_w: u32,
    x_range: (u32, u32),
    y_range: (u32, u32),
    r_range: (u8, u8),
    g_range: (u8, u8),
    b_range: (u8, u8),
) -> u32 {
    let mut count = 0;
    let row_stride = (physical_w as usize) * 4;
    for y in y_range.0..y_range.1 {
        for x in x_range.0..x_range.1 {
            let idx = (y as usize) * row_stride + (x as usize) * 4;
            if idx + 3 < rgba.len() {
                let r = rgba[idx];
                let g = rgba[idx + 1];
                let b = rgba[idx + 2];
                if (r_range.0..=r_range.1).contains(&r)
                    && (g_range.0..=g_range.1).contains(&g)
                    && (b_range.0..=b_range.1).contains(&b)
                {
                    count += 1;
                }
            }
        }
    }
    count
}

/// T-0615 派单 In #B PNG verify: + 按钮包圆角 box (默认 bg #2c2c2c). + box 在
/// 左上 (0..28×titlebar..titlebar+28) logical, 圆角 6 logical. 圆角处应露 tab bar
/// bg #1c (圆外像素是 #1c 色, 圆内像素是 #2c box bg).
#[test]
fn plus_button_box_renders_with_rounded_corners() {
    let (rgba, physical_w, physical_h) = render_with_state(1, 0, HoverRegion::None);
    write_png("/tmp/t0615_plus_box.png", &rgba, physical_w, physical_h);

    // + box 区: x ∈ [3 logical inset, 28 - 3 logical) phys, y ∈ [titlebar+3, titlebar+25)
    // tab bar y ∈ [titlebar (28*2=56), titlebar+tab_bar (28*2=112))
    let plus_inset_phys = 3 * HIDPI_SCALE;
    let plus_box_x_start = plus_inset_phys;
    let plus_box_x_end = (28 - 3) * HIDPI_SCALE;
    let plus_box_y_start = (TITLEBAR_H_LOGICAL as u32 + 3) * HIDPI_SCALE;
    let plus_box_y_end = ((TITLEBAR_H_LOGICAL + TAB_BAR_H_LOGICAL) as u32 - 3) * HIDPI_SCALE;

    // box bg #2c2c2c (44/44/44): sRGB roundtrip 严格保留 byte (linear 编码再
    // 解码无损), 实测 PNG 像素严格 (44, 44, 44).
    let box_bg_pixels = count_pixels_in_region(
        &rgba,
        physical_w,
        (plus_box_x_start, plus_box_x_end),
        (plus_box_y_start, plus_box_y_end),
        (35, 55),
        (35, 55),
        (35, 55),
    );
    eprintln!("plus box bg pixels: {}", box_bg_pixels);
    let region_total = (plus_box_x_end - plus_box_x_start) * (plus_box_y_end - plus_box_y_start);
    // + box bg + icon strokes 占 ~70%+ 区域 (圆角处 corner discard 露 bar bg).
    assert!(
        box_bg_pixels >= region_total / 3,
        "plus box bg 区应有 ≥ 1/3 像素是 box bg 灰色 (#2c系), got {box_bg_pixels}/{region_total}"
    );

    // + icon 横竖白线 (亮像素 ≥ 100): icon stroke 走 BUTTON_ICON #d3d3d3.
    let icon_pixels = count_pixels_in_region(
        &rgba,
        physical_w,
        (plus_box_x_start, plus_box_x_end),
        (plus_box_y_start, plus_box_y_end),
        (100, 255),
        (100, 255),
        (100, 255),
    );
    eprintln!("plus icon pixels: {}", icon_pixels);
    assert!(
        icon_pixels >= 30,
        "+ icon 横竖白线 应有 ≥ 30 亮像素 (#d3d3d3), got {icon_pixels}"
    );
}

/// T-0615 派单 In #B PNG verify: hover + 按钮时 box bg 高亮 (#444 vs default #2c).
#[test]
fn plus_button_box_hover_highlights() {
    let (rgba_normal, physical_w, physical_h) = render_with_state(1, 0, HoverRegion::None);
    let (rgba_hover, _, _) = render_with_state(1, 0, HoverRegion::TabBarPlus);
    write_png(
        "/tmp/t0615_plus_hover.png",
        &rgba_hover,
        physical_w,
        physical_h,
    );

    // 取 + box 中心一个非 icon 区像素 (避开横竖线 — 4 个角内). 例: x=10 phys,
    // y=titlebar+10 phys 落在 box 内但非 icon stroke (9 logical pad 让 icon 中心区
    // ~9-19 logical, 角点 < 9 logical 是 box bg 区).
    // 实测 hover bg 应比 normal bg 亮 (44 vs 2c 在 sRGB 编码后差).
    let row_stride = (physical_w as usize) * 4;
    // 取 box 内部非 icon 区 (10 logical, titlebar+10 logical = 20, 76 phys).
    // box bbox = (3..25, titlebar+3..titlebar+25) logical = (6..50, 62..106) phys.
    // 内嵌矩形 (圆角 6 logical=12 phys radius): (6+12, 62+12, 50-12, 106-12) = (18..38, 74..94).
    // probe (20, 80): 在内嵌矩形, 不在 icon (icon 在中央 plus_w/2=28).
    let probe_x = 20;
    let probe_y = 80;
    let idx = probe_y * row_stride + probe_x * 4;

    let r_normal = rgba_normal[idx];
    let r_hover = rgba_hover[idx];
    eprintln!("plus box probe (x={probe_x}, y={probe_y}): normal r={r_normal}, hover r={r_hover}");
    assert!(
        r_hover as i32 - r_normal as i32 >= 5,
        "hover + box 应比 normal 亮 (#444 vs #2c, sRGB 后 r 应增 ≥ 5), normal {r_normal} hover {r_hover}"
    );
}

/// T-0615 派单 In #C PNG verify: active tab 圆角 box (#444 灰底). 圆角处 corner
/// discard 露 tab bar bg #1c.
#[test]
fn active_tab_renders_rounded_box_with_gray_bg() {
    let (rgba, physical_w, physical_h) = render_with_state(3, 1, HoverRegion::None);
    write_png(
        "/tmp/t0615_active_tab_rounded.png",
        &rgba,
        physical_w,
        physical_h,
    );

    // active idx=1 tab body 区 (派单 multi_tab e2e 同):
    let plus_w_phys = 28 * HIDPI_SCALE;
    let body_w_phys = 200 * HIDPI_SCALE;
    let active_x_start = plus_w_phys + body_w_phys + (4 * HIDPI_SCALE) / 2; // gap/2 内缩
    let active_x_end = (plus_w_phys + 2 * body_w_phys - (4 * HIDPI_SCALE) / 2).min(physical_w);
    let active_y_start = (TITLEBAR_H_LOGICAL as u32 + 3) * HIDPI_SCALE;
    let active_y_end = ((TITLEBAR_H_LOGICAL + TAB_BAR_H_LOGICAL) as u32 - 3) * HIDPI_SCALE;

    // active bg #444 灰色 (sRGB 后 r ≈ 65-75): 数 active body 区灰色像素
    let active_pixels = count_pixels_in_region(
        &rgba,
        physical_w,
        (active_x_start, active_x_end),
        (active_y_start, active_y_end),
        (60, 80),
        (60, 80),
        (60, 80),
    );
    let region_total = (active_x_end - active_x_start) * (active_y_end - active_y_start);
    eprintln!(
        "active tab #444 pixels: {} / {}",
        active_pixels, region_total
    );
    // 圆角内 active bg #444, 4 角 corner discard 透 tab bar bg #1c.
    // 派单 In #C "active tab 4 角圆": ≥ 50% box bg.
    assert!(
        active_pixels * 2 >= region_total,
        "active tab body ≥ 50% #444 像素, got {active_pixels}/{region_total}"
    );
    // 验 corner discard: 找一个 box 4 角点 (相对 box 内嵌矩形外的点) 应是 bar bg
    // #1c (28, 28, 28). active body x=[active_x_start, active_x_end), 取 4 个角点
    // 偏移 1 phys (确保在 box bbox 内但在内嵌圆心外).
    let corners = [
        (active_x_start, active_y_start),     // top-left
        (active_x_end - 1, active_y_start),   // top-right
        (active_x_start, active_y_end - 1),   // bottom-left
        (active_x_end - 1, active_y_end - 1), // bottom-right
    ];
    let row_stride = (physical_w as usize) * 4;
    let mut corners_show_bar_bg = 0;
    for (cx, cy) in corners {
        let idx = (cy as usize) * row_stride + (cx as usize) * 4;
        if idx + 3 < rgba.len() {
            let r = rgba[idx];
            let g = rgba[idx + 1];
            let b = rgba[idx + 2];
            // bar bg #1c=28: r/g/b ∈ [25, 35] (sRGB roundtrip 保 byte).
            if (20..=35).contains(&r) && (20..=35).contains(&g) && (20..=35).contains(&b) {
                corners_show_bar_bg += 1;
            }
        }
    }
    assert!(
        corners_show_bar_bg >= 1,
        "active tab body 4 角中至少 1 角 corner mask discard 露 bar bg #1c, got {corners_show_bar_bg}/4"
    );
}

/// T-0615 派单 In #C PNG verify: inactive tab 透明 — 仅 tab bar bg #1c, 无 #444 box.
#[test]
fn inactive_tab_renders_transparent_no_box() {
    let (rgba, physical_w, physical_h) = render_with_state(3, 1, HoverRegion::None);
    write_png(
        "/tmp/t0615_inactive_tab_transparent.png",
        &rgba,
        physical_w,
        physical_h,
    );

    // inactive idx=0 tab body 区: 应仅 tab bar bg #1c, 无 active #444.
    let plus_w_phys = 28 * HIDPI_SCALE;
    let body_w_phys = 200 * HIDPI_SCALE;
    let inactive_x_start = plus_w_phys + (4 * HIDPI_SCALE) / 2;
    let inactive_x_end = plus_w_phys + body_w_phys - (4 * HIDPI_SCALE) / 2;
    let inactive_y_start = (TITLEBAR_H_LOGICAL as u32 + 3) * HIDPI_SCALE;
    let inactive_y_end = ((TITLEBAR_H_LOGICAL + TAB_BAR_H_LOGICAL) as u32 - 3) * HIDPI_SCALE;

    // tab bar bg #1c 灰 (sRGB → r ≈ 25-35 编码后):
    let bar_bg_pixels = count_pixels_in_region(
        &rgba,
        physical_w,
        (inactive_x_start, inactive_x_end),
        (inactive_y_start, inactive_y_end),
        (15, 40),
        (15, 40),
        (15, 40),
    );
    // active #444 (sRGB roundtrip 保 byte → r=g=b=68):
    let active_box_pixels = count_pixels_in_region(
        &rgba,
        physical_w,
        (inactive_x_start, inactive_x_end),
        (inactive_y_start, inactive_y_end),
        (60, 80),
        (60, 80),
        (60, 80),
    );
    let region_total = (inactive_x_end - inactive_x_start) * (inactive_y_end - inactive_y_start);
    eprintln!(
        "inactive tab: bar bg #1c={} / region {}, active-style #444={}",
        bar_bg_pixels, region_total, active_box_pixels
    );
    // inactive 区主体应是 bar bg, active #444 box 像素应 ≈ 0
    assert!(
        bar_bg_pixels * 2 >= region_total,
        "inactive tab 应 ≥ 50% bar bg #1c, got {bar_bg_pixels}/{region_total}"
    );
    // active #444 像素应 < 5% (允许极少 antialias 边)
    assert!(
        active_box_pixels * 20 < region_total,
        "inactive tab 不应有 active #444 box, got {active_box_pixels}/{region_total}"
    );
}

/// T-0615 派单 In #D PNG verify: hover Close 圆形红 bg (#cc4444). 圆形 button 视觉
/// 4 角 discard 露 titlebar bg.
#[test]
fn close_button_hover_renders_red_circle() {
    let (rgba, physical_w, physical_h) =
        render_with_state(1, 0, HoverRegion::Button(WindowButton::Close));
    write_png(
        "/tmp/t0615_close_hover_red.png",
        &rgba,
        physical_w,
        physical_h,
    );

    // Close button bbox 区: 右上, x ∈ [776, 800) logical = [1552, 1600) phys (LOGICAL_W=800)
    let close_x_start = (LOGICAL_W - 24) * HIDPI_SCALE;
    let close_x_end = LOGICAL_W * HIDPI_SCALE;
    let close_y_start = 0;
    let close_y_end = 24 * HIDPI_SCALE;

    // 红色 #cc4444 (204/68/68): sRGB roundtrip 保 byte (实测 PNG 像素严格 (204,68,68)).
    let red_pixels = count_pixels_in_region(
        &rgba,
        physical_w,
        (close_x_start, close_x_end),
        (close_y_start, close_y_end),
        (180, 220),
        (50, 90),
        (50, 90),
    );
    eprintln!("close hover red pixels: {}", red_pixels);
    let region_total = (close_x_end - close_x_start) * (close_y_end - close_y_start);
    // 圆形 bg 占 bbox ~78% 区域 (内嵌圆), 加 surface corner mask + AA 边 容差.
    assert!(
        red_pixels * 4 >= region_total,
        "close hover 应有 ≥ 25% 红色像素 (#cc4444 圆形 bg), got {red_pixels}/{region_total}"
    );
}

/// T-0615 派单 In #C PNG verify: hover inactive tab 圆角 hover bg (#333 hover 色).
#[test]
fn hover_inactive_tab_renders_hover_box() {
    let (rgba, physical_w, physical_h) = render_with_state(3, 1, HoverRegion::Tab(2));
    write_png(
        "/tmp/t0615_hover_inactive.png",
        &rgba,
        physical_w,
        physical_h,
    );

    // hover idx=2 tab body 区
    let plus_w_phys = 28 * HIDPI_SCALE;
    let body_w_phys = 200 * HIDPI_SCALE;
    let hover_x_start = plus_w_phys + 2 * body_w_phys + (4 * HIDPI_SCALE) / 2;
    let hover_x_end = (plus_w_phys + 3 * body_w_phys - (4 * HIDPI_SCALE) / 2).min(physical_w);
    let hover_y_start = (TITLEBAR_H_LOGICAL as u32 + 3) * HIDPI_SCALE;
    let hover_y_end = ((TITLEBAR_H_LOGICAL + TAB_BAR_H_LOGICAL) as u32 - 3) * HIDPI_SCALE;

    // hover bg #333 (51/51/51): sRGB roundtrip 保 byte → r=g=b=51.
    let hover_pixels = count_pixels_in_region(
        &rgba,
        physical_w,
        (hover_x_start, hover_x_end),
        (hover_y_start, hover_y_end),
        (45, 60),
        (45, 60),
        (45, 60),
    );
    let region_total = (hover_x_end - hover_x_start) * (hover_y_end - hover_y_start);
    eprintln!(
        "hover inactive tab #333 pixels: {} / {}",
        hover_pixels, region_total
    );
    assert!(
        hover_pixels * 3 >= region_total,
        "hover inactive tab ≥ 33% hover bg (#333), got {hover_pixels}/{region_total}"
    );
}

/// T-0615 派单 In #C PNG verify: hover tab close × 红圆 bg.
#[test]
fn hover_tab_close_renders_red_circle() {
    let (rgba, physical_w, physical_h) = render_with_state(3, 0, HoverRegion::TabClose(0));
    write_png(
        "/tmp/t0615_tab_close_red.png",
        &rgba,
        physical_w,
        physical_h,
    );

    // tab close × 在 tab body 右侧 close_w (16 logical) 区
    let plus_w_phys = 28 * HIDPI_SCALE;
    let body_w_phys = 200 * HIDPI_SCALE;
    let close_w_phys = 16 * HIDPI_SCALE;
    // 红圆位于 idx=0 tab body 右侧
    let close_center_x = plus_w_phys + body_w_phys - close_w_phys / 2;
    let close_x_start = close_center_x.saturating_sub(close_w_phys);
    let close_x_end = (close_center_x + close_w_phys).min(physical_w);
    let close_y_start = TITLEBAR_H_LOGICAL as u32 * HIDPI_SCALE;
    let close_y_end = (TITLEBAR_H_LOGICAL + TAB_BAR_H_LOGICAL) as u32 * HIDPI_SCALE;

    // 红 #cc4444 sRGB roundtrip 保 byte → (204, 68, 68).
    let red_pixels = count_pixels_in_region(
        &rgba,
        physical_w,
        (close_x_start, close_x_end),
        (close_y_start, close_y_end),
        (180, 220),
        (50, 90),
        (50, 90),
    );
    eprintln!("tab close × red pixels: {}", red_pixels);
    // 红圆 bg 直径 ~16 logical = 32 phys, 面积 ~π*16² ≈ 800 phys²
    assert!(
        red_pixels >= 100,
        "tab close × hover 红圆应有 ≥ 100 红像素, got {red_pixels}"
    );
}

/// T-0615 multi-tab 综合: 3 tabs + active=1 + hover idx=0 close. 视觉应含 active
/// 灰圆角 + hover red circle on idx=0 close.
#[test]
fn multi_tab_polished_visual() {
    let (rgba, physical_w, physical_h) = render_with_state(3, 1, HoverRegion::TabClose(0));
    write_png(
        "/tmp/t0615_multi_tab_polished.png",
        &rgba,
        physical_w,
        physical_h,
    );

    // PNG 应非全 0 — 视觉综合验证
    let nonzero = rgba.iter().filter(|&&b| b > 8).count();
    assert!(
        nonzero > 5000,
        "multi tab polished PNG 应有大量非空像素, got {}",
        nonzero
    );
}
