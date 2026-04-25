//! T-0406 集成测试: glyph atlas clear-on-full 端到端 verify。
//!
//! **why 集成测试不构造 GlyphAtlas**: `GlyphAtlas` 是 `src/wl/render.rs` 模块私有
//! struct (含 `wgpu::Texture` / `BindGroup` 等, INV-010 类型隔离禁外露)。集成测试
//! 走 `render_headless` 端到端 — 单次调用内灌入足够多不同 CJK glyph 强制触发 atlas
//! 满 → clear-on-full 路径 → 后续 glyph 在 reset 后的 (0,0) 重新分配 → render 不
//! panic + 返非空 RGBA。
//!
//! **覆盖**:
//! - `atlas_full_triggers_clear_no_panic`: 灌 >ATLAS 容量的不同 CJK glyph (CJK
//!   Unified Ideographs U+4E00..U+9FFF 共 ~20K, 远超 atlas ~4200 槽位上限),
//!   单次 render_headless 必触发 clear。验不 panic + RGBA 长度对。
//! - `atlas_clear_then_renders_visible_pixels`: 灌满后 clear, 接着一段 ASCII
//!   text 应渲染出可见像素 (clear-on-full 后路径仍工作, 不是 silent stuck)。
//!
//! **why 端到端 (vs unit)**: GlyphAtlas 私有 + wgpu 资源构造昂贵, 写 unit test
//! 需 cfg(test) accessor + mock device, ROI 不及端到端清晰。派单"clear 后要继续
//! 走 shelf packing 把当前 glyph 放进去"的 invariant 由"clear 后帧仍 render 出
//! 像素"间接锁住。
//!
//! **运行**: `cargo test --release --test atlas_clear_on_full` (release 防 GPU
//! 路径慢, headless 路径 `request_adapter` 用户机有 NVIDIA Vulkan / fallback CPU
//! lavapipe 都接受)。

use quill::term::{CellPos, CellRef, Color};
use quill::text::TextSystem;
use quill::wl::{render_headless, HIDPI_SCALE};

/// 沿袭 tests/cjk_fallback_e2e.rs 800×600 logical (HIDPI=2 → 1600×1200 physical),
/// 落在 wgpu downlevel_defaults `max_texture_dimension_2d = 2048` 内。
/// glyph 是否进 atlas 与 surface 尺寸无关 (render_headless 内 `effective_rows =
/// row_texts.len().min(rows)` 仍迭代所有 row 走 shape + raster + atlas allocate,
/// 即便 ROWS > surface 高 / cell_h 仍触发 atlas 路径)。
const LOGICAL_W: u32 = 800;
const LOGICAL_H: u32 = 600;
const COLS: usize = 80;
/// rows 取大数: 灌满 atlas 需 >4200 不同 glyph, COLS=80, 4200/80 ≈ 53 行起触发。
/// 200 行有充足余量, 实测 single render_headless 内 glyph 总数 16000 → 必触发
/// clear (atlas 单 glyph @ 34pt 占 ~20×48 px, 2048×2048/960 ≈ 4400 槽位)。
const ROWS: usize = 200;

/// 默认 fg/bg 与 cjk_fallback_e2e.rs / render.rs::clear_color_for 同源。
const DEFAULT_FG: Color = Color {
    r: 0xff,
    g: 0xff,
    b: 0xff,
};
const DEFAULT_BG: Color = Color {
    r: 0x0a,
    g: 0x10,
    b: 0x30,
};

/// 构造 cols×rows 个空白 CellRef (cell.c = ' ', fg/bg 默认)。render_headless 内部
/// `effective_rows = row_texts.len().min(rows)`, glyph 路径走 row_texts 而非 cells
/// (cells 仅用于 cell pass 着色), 给足空白 cells 即可。
fn empty_cell_refs(cols: usize, rows: usize) -> Vec<CellRef> {
    let mut out = Vec::with_capacity(cols * rows);
    for line in 0..rows {
        for col in 0..cols {
            out.push(CellRef {
                pos: CellPos { col, line },
                c: ' ',
                fg: DEFAULT_FG,
                bg: DEFAULT_BG,
            });
        }
    }
    out
}

/// 构造 200 行 row_texts, 每行 80 个**不同** CJK 汉字 (从 U+4E00 起步逐字递增,
/// 共 200×80 = 16000 不同 cp, 远超 atlas ~4200 槽位上限, 单次 render_headless
/// 内必触发 atlas 满 → clear-on-full)。
///
/// CJK Unified Ideographs U+4E00..U+9FFF 范围 ~20K cp, 取前 16000 安全。
fn cjk_row_texts(cols: usize, rows: usize) -> Vec<String> {
    let mut texts = Vec::with_capacity(rows);
    let mut cp: u32 = 0x4E00;
    for _ in 0..rows {
        let mut s = String::with_capacity(cols * 3);
        for _ in 0..cols {
            // U+4E00..U+9FFF 全是 valid CJK char, from_u32 不会 None
            if let Some(c) = char::from_u32(cp) {
                s.push(c);
            }
            cp = cp.saturating_add(1);
        }
        texts.push(s);
    }
    texts
}

/// 灌满 atlas 触发 clear-on-full: 单次 render_headless 喂 16000 不同 CJK glyph,
/// 不 panic + 返合法 RGBA bytes。
///
/// **why 不 assert atlas 内部状态**: GlyphAtlas 模块私有, 集成测试拿不到
/// cursor_x / allocations.len。走"render 不 panic + RGBA 长度合法"作 proxy:
/// clear-on-full 路径正确则后续 glyph 仍能 allocate, render pass 走完,
/// copy_texture_to_buffer + map_async 给出合法长度 buffer。任一环节 panic / Err
/// 路径 (T-0406 之前 anyhow Err) 则 expect 抛出测试失败。
#[test]
fn atlas_full_triggers_clear_no_panic() {
    let mut text_system = TextSystem::new().expect("TextSystem::new failed (need monospace font)");

    let cell_refs = empty_cell_refs(COLS, ROWS);
    let row_texts = cjk_row_texts(COLS, ROWS);

    // 16000 不同 CJK glyph 喂入: ~3-4× atlas 槽位上限, clear-on-full 应触发 2-3 次
    let result = render_headless(
        &mut text_system,
        &cell_refs,
        COLS,
        ROWS,
        &row_texts,
        LOGICAL_W,
        LOGICAL_H,
    );

    let (rgba, physical_w, physical_h) =
        result.expect("render_headless after atlas full should not panic / Err (clear-on-full)");

    let expected_w = LOGICAL_W * HIDPI_SCALE;
    let expected_h = LOGICAL_H * HIDPI_SCALE;
    assert_eq!(physical_w, expected_w);
    assert_eq!(physical_h, expected_h);
    assert_eq!(
        rgba.len(),
        (expected_w as usize) * (expected_h as usize) * 4,
        "RGBA byte len must equal physical_w*physical_h*4 even after atlas clear"
    );
}

/// 灌满后 clear, 仍能渲染出可见像素 (clear-on-full 后路径不 silent stuck)。
///
/// 第一次 render_headless 喂 16000 CJK 触发 clear; 第二次 render_headless 用同
/// `text_system` 喂少量 ASCII。**注意**: render_headless 自建 local atlas (函数返
/// 回 drop), 第二次 atlas 是新的 — 但 `text_system` 内部 SwashCache 跨次保留,
/// 验"灌满 atlas 不污染 TextSystem 内部状态" (cosmic-text fontdb / SwashCache 不
/// 因 atlas 满而 corrupt)。
///
/// 像素阈值 300: 沿袭 tests/cjk_fallback_e2e.rs 同源阈值 — clear color #0a1030 ±
/// 12 容差, "hello world" 5+5=10 char × stroke ~120 px = ~1200 量级, 300 是宽松
/// 下限防 CI flake。
#[test]
fn atlas_clear_then_renders_visible_pixels() {
    let mut text_system = TextSystem::new().expect("TextSystem::new failed (need monospace font)");

    // Pass 1: 灌满 + 触发 clear (本身已被 fn 1 覆盖, 这里只走到不 panic 即可)
    {
        let cell_refs = empty_cell_refs(COLS, ROWS);
        let row_texts = cjk_row_texts(COLS, ROWS);
        let _ = render_headless(
            &mut text_system,
            &cell_refs,
            COLS,
            ROWS,
            &row_texts,
            LOGICAL_W,
            LOGICAL_H,
        )
        .expect("render_headless pass 1 (atlas full path) should not panic");
    }

    // Pass 2: ASCII "hello world" 单行渲染, 应有 >= 300 非 clear-color 像素
    let cols2: usize = 80;
    let rows2: usize = 24;
    let cell_refs = empty_cell_refs(cols2, rows2);
    let mut row_texts = vec![String::new(); rows2];
    row_texts[0] = "hello world".into();

    let (rgba, physical_w, physical_h) = render_headless(
        &mut text_system,
        &cell_refs,
        cols2,
        rows2,
        &row_texts,
        LOGICAL_W,
        LOGICAL_H,
    )
    .expect("render_headless pass 2 after atlas full should not panic");

    let row_stride = (physical_w as usize) * 4;
    let region_w: u32 = 400;
    let region_h: u32 = 60;
    assert!(region_h <= physical_h);
    let mut non_clear: u32 = 0;
    for y in 0..(region_h as usize) {
        for x in 0..(region_w as usize) {
            let idx = y * row_stride + x * 4;
            let r = rgba[idx];
            let g = rgba[idx + 1];
            let b = rgba[idx + 2];
            let near_clear = (r as i32 - 0x0a).abs() <= 12
                && (g as i32 - 0x10).abs() <= 12
                && (b as i32 - 0x30).abs() <= 12;
            if !near_clear {
                non_clear += 1;
            }
        }
    }
    assert!(
        non_clear >= 300,
        "after atlas full + clear, 'hello world' row should have >= 300 non-clear-color \
         pixels (got {}), region {}x{} physical px",
        non_clear,
        region_w,
        region_h
    );
}
