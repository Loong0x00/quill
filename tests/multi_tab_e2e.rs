//! T-0608 multi-tab e2e PNG verify (派单 In #J + Acceptance "三源 PNG verify").
//!
//! 走 `render_headless` + `set_headless_tab_state` 注入 tab_count + active_idx,
//! 输出 PNG 到 /tmp/t0608_*.png 让 writer / Lead / reviewer Read 同一文件验视觉:
//! 1. 3 个 tab + active 第 2 个高亮 + + 按钮 + close × 视觉
//! 2. 单 tab 仍正常 (回归保险)
//! 3. 多 tab 视觉验证: tab body 高亮颜色 / 分隔线 / + 按钮位置.

use quill::tab::TabInstance;
use quill::term::{CellPos, CellRef, Color};
use quill::text::TextSystem;
use quill::wl::{
    render_headless, reset_headless_tab_state, set_headless_tab_state, CursorInfo, CursorStyle,
};

const HIDPI_SCALE: u32 = 2;
const COLS: usize = 80;
const ROWS: usize = 21;
const LOGICAL_W: u32 = 800;
const LOGICAL_H: u32 = 600;

const TITLEBAR_H_LOGICAL: usize = 28;
const TAB_BAR_H_LOGICAL: usize = 28;

/// 构造空 cell array. 与 selection_e2e / cursor_render_e2e 同套路.
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

fn render_with_tabs(tab_count: usize, active_idx: usize) -> (Vec<u8>, u32, u32) {
    set_headless_tab_state(tab_count, active_idx);
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
    eprintln!("writer-T0608 wrote PNG: {path}");
}

/// T-0608 acceptance: 渲染 3 个 tab + active=1 (第 2 个), PNG 写到固定路径让
/// writer / Lead / reviewer Read 验视觉.
///
/// 视觉验证点 (派单 In #J + Acceptance):
/// - 3 个 tab body 平均分布
/// - active idx=1 高亮 (#404060 灰青) 显著区别于 inactive (#1c1c1c 深灰)
/// - 左上 + 按钮 (28×28 logical, 顶部 28..56 logical y 范围)
/// - 每 tab 右侧 close × 按钮 (16 logical 宽)
#[test]
fn three_tabs_active_second_renders_with_highlight() {
    let (rgba, physical_w, physical_h) = render_with_tabs(3, 1);

    write_png("/tmp/t0608_three_tabs.png", &rgba, physical_w, physical_h);

    // tab bar y 范围 (physical px): titlebar (56) ~ titlebar + tab_bar (112).
    let tab_bar_y_start = (TITLEBAR_H_LOGICAL * HIDPI_SCALE as usize) as u32;
    let tab_bar_y_end = ((TITLEBAR_H_LOGICAL + TAB_BAR_H_LOGICAL) * HIDPI_SCALE as usize) as u32;
    assert!(
        tab_bar_y_end <= physical_h,
        "tab bar y_end {} 应 <= physical_h {}",
        tab_bar_y_end,
        physical_h
    );

    // active tab body 区 (idx=1): plus_w 28 logical = 56 phys 起, 之后是 tab body.
    // tab_body_width = (surface_w - plus_w) / 3 = (800 - 28) / 3 ≈ 257 logical (clamp [80,200] → 200).
    // active idx=1 → x ∈ [plus_w + body_w, plus_w + 2*body_w]
    //   = [56 + 400, 56 + 800] phys, 但 clamp 到 surface ∈ [56+400=456, min(756, surface_w_phys)]
    let plus_w_phys = 28 * HIDPI_SCALE;
    let body_w_phys = 200 * HIDPI_SCALE; // tab_max=200 logical
    let active_x_start = plus_w_phys + body_w_phys;
    let active_x_end = (plus_w_phys + 2 * body_w_phys).min(physical_w);

    // 数 active tab 区高亮像素 (#404060 = 64,64,96): r ≈ 64, g ≈ 64, b ≈ 96.
    // sRGB 编码后的 R 值大致 ≥ 50, B > R (因 B 高 = 96).
    let mut active_pixels: u32 = 0;
    let row_stride = (physical_w as usize) * 4;
    for y in tab_bar_y_start..tab_bar_y_end {
        for x in active_x_start..active_x_end {
            let idx = (y as usize) * row_stride + (x as usize) * 4;
            if idx + 3 < rgba.len() {
                let r = rgba[idx];
                let g = rgba[idx + 1];
                let b = rgba[idx + 2];
                // T-0613 hotfix: active bg 改 #444444 中性灰 (之前 #404060 紫调).
                // 判 R≈G≈B 灰色范围 [55, 75] (sRGB encode 后 #44 ≈ 68).
                let is_active_bg = (55..=80).contains(&r)
                    && (r as i32 - g as i32).abs() <= 6
                    && (r as i32 - b as i32).abs() <= 6;
                if is_active_bg {
                    active_pixels += 1;
                }
            }
        }
    }
    let region_total = (tab_bar_y_end - tab_bar_y_start) * (active_x_end - active_x_start);
    eprintln!(
        "active tab bg pixels: {} / region {} ({}%)",
        active_pixels,
        region_total,
        (active_pixels as f32 / region_total as f32) * 100.0
    );
    // 至少 30% 区域是 active bg 色 (避开 close icon / tab 分隔线 / antialias 边).
    assert!(
        active_pixels * 100 / region_total >= 30,
        "active tab body 应有 >= 30% 像素是 #404060 高亮色, got {}/{}",
        active_pixels,
        region_total
    );

    // + 按钮区 (左上, plus_w_phys × tab_bar_h_phys) 应有亮色 icon (#d3d3d3).
    let mut plus_icon_pixels: u32 = 0;
    for y in tab_bar_y_start..tab_bar_y_end {
        for x in 0..plus_w_phys {
            let idx = (y as usize) * row_stride + (x as usize) * 4;
            if idx + 3 < rgba.len() {
                let r = rgba[idx];
                let g = rgba[idx + 1];
                let b = rgba[idx + 2];
                // BUTTON_ICON #d3d3d3 (211/211/211): r >= 100, g >= 100, b >= 100.
                let is_icon = r >= 100 && g >= 100 && b >= 100;
                if is_icon {
                    plus_icon_pixels += 1;
                }
            }
        }
    }
    eprintln!("+ button icon pixels: {}", plus_icon_pixels);
    // + icon 是横竖两条线 (~2 stroke × ~12 logical len × HIDPI = ~48 phys 每条),
    // 总像素 ~96-120. 阈值 30 防 antialias 偏差.
    assert!(
        plus_icon_pixels >= 30,
        "+ button icon 应有 >= 30 亮像素, got {}",
        plus_icon_pixels
    );
}

/// 单 tab 仍正常 (回归保险). 派单 Out 段 "close 最后一个 tab → quit" 不在此
/// 测覆盖 (需要 LoopData level), 这里只验单 tab 渲染不破.
#[test]
fn single_tab_renders_default() {
    let (rgba, physical_w, physical_h) = render_with_tabs(1, 0);

    write_png("/tmp/t0608_single_tab.png", &rgba, physical_w, physical_h);

    // PNG 应非全 0 — 单 tab + titlebar + tab bar 都画了, 总有像素.
    let nonzero = rgba.iter().filter(|&&b| b > 8).count();
    assert!(
        nonzero > 1000,
        "单 tab PNG 应有非空像素 (titlebar + tab bar + clear bg), got {}",
        nonzero
    );
}

/// TabList 数据结构 + spawn 真子 shell 的集成路径 (smoke test). 验证 spawn /
/// push / set_active / remove 链路完整 (派单 Acceptance "多 tab 数据结构 + LoopData
/// 重构 (单测覆盖)").
#[test]
fn tab_list_spawn_real_shell_smoke() {
    use quill::tab::TabList;

    // spawn 真子 shell — CI 必装 bash. spawn 失败说明环境严重出问题, 集成测试
    // 失败合理.
    let initial = TabInstance::spawn(80, 24).expect("spawn first tab failed");
    let _initial_id = initial.id();
    let mut list = TabList::new(initial);
    assert_eq!(list.len(), 1);
    assert_eq!(list.active_idx(), 0);

    // push 第二个
    let second = TabInstance::spawn(80, 24).expect("spawn second tab failed");
    let second_id = second.id();
    let (idx, ret_id) = list.push(second);
    assert_eq!(idx, 1);
    assert_eq!(ret_id, second_id);
    assert_eq!(list.len(), 2);

    // 切到第二个
    assert!(list.set_active(1));
    assert_eq!(list.active_idx(), 1);
    assert_eq!(list.active().id(), second_id);

    // 关掉 active (idx=1, 邻近选择 prev=0)
    let removed = list.remove(1);
    assert!(removed.is_some());
    assert_eq!(list.len(), 1);
    assert_eq!(list.active_idx(), 0);
}

/// hit_test_with_tabs 与 render 视觉一致: + 按钮 / tab body / close × 区域
/// hit_test 都返对应 HoverRegion. 派单 In #D + In #J 单测覆盖.
#[test]
fn hit_test_tab_bar_regions_match_render() {
    use quill::wl::{HoverRegion, WindowButton};
    // 800×600 surface, 3 tabs.
    // tab bar y 范围 = [28, 56) logical.
    // + 按钮 = x ∈ [0, 28), tab bar y.
    let h = quill::wl::hit_test_with_tabs(10.0, 35.0, 800, 600, 3);
    assert_eq!(h, HoverRegion::TabBarPlus, "x=10 y=35 应是 + 按钮");

    // tab body width = (800 - 28) / 3 ≈ 257 → clamp 200.
    // tab idx=0 = x ∈ [28, 228), tab idx=1 = [228, 428), tab idx=2 = [428, 628).
    // close 按钮 width 16, 在 tab 右侧.
    // tab idx=0 click 区 (避开 close): x ∈ [28, 212), y=35 → Tab(0).
    let h = quill::wl::hit_test_with_tabs(100.0, 35.0, 800, 600, 3);
    assert_eq!(h, HoverRegion::Tab(0));

    // tab idx=0 close ×: x ∈ [212, 228), y=35 → TabClose(0).
    let h = quill::wl::hit_test_with_tabs(220.0, 35.0, 800, 600, 3);
    assert_eq!(h, HoverRegion::TabClose(0));

    // tab idx=1 click 区: x ∈ [228, 412).
    let h = quill::wl::hit_test_with_tabs(300.0, 35.0, 800, 600, 3);
    assert_eq!(h, HoverRegion::Tab(1));

    // titlebar 区域不变 (y < 28).
    let h = quill::wl::hit_test_with_tabs(100.0, 10.0, 800, 600, 3);
    assert_eq!(h, HoverRegion::TitleBar);

    // text area 区域 (y >= 56).
    let h = quill::wl::hit_test_with_tabs(100.0, 100.0, 800, 600, 3);
    assert_eq!(h, HoverRegion::TextArea);

    // close button 仍在右上 titlebar 区 (y < 24).
    let h = quill::wl::hit_test_with_tabs(790.0, 12.0, 800, 600, 3);
    assert_eq!(h, HoverRegion::Button(WindowButton::Close));
}

/// 防止未使用警告 — CursorInfo + CursorStyle 是 import 留作未来加 cursor 验证用.
#[allow(dead_code)]
fn _unused_imports_pin() {
    let _ = CursorStyle::Block;
    let _: Option<&CursorInfo> = None;
}
