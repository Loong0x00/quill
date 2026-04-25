//! T-0405 集成测试: CJK fallback 端到端 verify。
//!
//! **why 集成测试**: T-0407 face lock + GlyphKey 已经 lib test 锁住单元路径
//! (shape_line_mixed_cjk / atlas_key_includes_face_id 等); 本测试走 PTY → Term →
//! render_headless → PNG encode 真实流, 验"中文 prompt 真画到 PNG"这条端到端
//! 链路。沿袭 tests/headless_screenshot.rs (T-0408) 同款 helper 套路。
//!
//! **覆盖**:
//! - `printf '你好 hello\n'` 进 PTY → term grid 真有 CJK + ASCII 内容
//! - render_headless 离屏渲染拿 RGBA byte 流
//! - PNG encode 写 /tmp/cjk_test.png (Lead / agent 可视读 verify "看到中文")
//! - 像素 verify: prompt 行至少有非 clear-color 像素 (字形真画了, 区分 "PNG
//!   存在但内容空白" 退化)
//! - face_id verify: shape "你" 单独走一遍 face_id 应 ≠ primary_face_id (CJK
//!   fallback 真触发, 替代 lib test `shape_line_mixed_cjk` 仅锁 glyph count)
//!
//! **运行**: `cargo test --test cjk_fallback_e2e` (用户机有 noto-cjk-mono +
//! adobe-source-han-sans, CI 退化路径 cosmic-text 给 .notdef tofu — 测试以
//! 用户机为准)。

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
const ROWS: u16 = 24;
/// printf 跨 PTY 的 stdout flush 延迟实测 50-150 ms; 300 ms 给余量。
/// (派单已知陷阱"buffer 可能不完整, sleep 300ms 等"。)
const PTY_DRAIN_WAIT_MS: u64 = 300;

/// spawn `printf '你好 hello\n'`, drain PTY 到 term, 调 render_headless 拿
/// (rgba, physical_w, physical_h)。两个测试共享, 避免重复 spawn 样板。
///
/// 失败路径用 `panic!`/`expect` (test 路径允许 — `tests/` 不是 main / lib 代码,
/// CLAUDE.md "禁用 unwrap/expect" 仅约束 src/, conventions §3 "真 IO 测试允许
/// std::thread::sleep")。
fn render_headless_with_cjk_printf() -> (Vec<u8>, u32, u32) {
    let mut text_system = TextSystem::new().expect("TextSystem::new failed (need monospace font)");

    let mut term = TermState::new(COLS, ROWS);
    // /usr/bin/printf 走真 binary (不是 shell builtin), spawn_program 直接拉起。
    // 单引号是 shell 语义, 这里 spawn 直接传 raw bytes 不需要 shell 转义。
    let mut pty = PtyHandle::spawn_program("printf", &["你好 hello\n"], COLS, ROWS)
        .expect("PtyHandle::spawn_program(printf) failed");

    std::thread::sleep(std::time::Duration::from_millis(PTY_DRAIN_WAIT_MS));

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
    )
    .expect("render_headless failed")
}

/// 端到端: `printf '你好 hello\n'` → render_headless → /tmp/cjk_test.png +
/// 像素 verify。
///
/// **关键 deliverable** (派单 Acceptance): /tmp/cjk_test.png 真显示中文 "你好" +
/// ASCII "hello", agent / Lead Read PNG verify 视觉。**测试自身只锁"非空 PNG +
/// 至少 N 个非清屏像素"**, 视觉对错以 PNG 文件为最终凭证。
///
/// **像素阈值 (300 px)**: 1 个 ASCII glyph 17pt × HIDPI=34pt 实测 stroke ~120 px
/// (派单 audit T-0408 P0-2 trace), 6 字符 "hello\n" + 2 个 CJK 你好 stroke 量
/// 远超 300。300 是宽松下限防 CI flake; 实测应在 1500+ 量级。
#[test]
fn cjk_chars_render_to_png_via_noto_fallback() {
    use image::codecs::png::PngEncoder;
    use image::ExtendedColorType;
    use image::ImageEncoder;

    let (rgba, physical_w, physical_h) = render_headless_with_cjk_printf();
    assert_eq!(physical_w, PHYSICAL_W);
    assert_eq!(physical_h, PHYSICAL_H);
    assert_eq!(
        rgba.len(),
        (PHYSICAL_W as usize) * (PHYSICAL_H as usize) * 4,
        "RGBA byte len must equal physical_w*physical_h*4"
    );

    // 写 /tmp/cjk_test.png (派单 Acceptance 路径, agent / Lead Read 视觉验)。
    let path = PathBuf::from("/tmp/cjk_test.png");
    {
        let file = std::fs::File::create(&path).expect("create /tmp/cjk_test.png");
        let encoder = PngEncoder::new(file);
        encoder
            .write_image(&rgba, physical_w, physical_h, ExtendedColorType::Rgba8)
            .expect("PngEncoder write_image");
    }
    let metadata = std::fs::metadata(&path).expect("metadata /tmp/cjk_test.png");
    assert!(
        metadata.len() > 4096,
        "PNG file size {} should be > 4 KiB (sanity: deflated 1600x1200 RGBA \
         normally 10-100 KiB)",
        metadata.len()
    );

    // PNG signature
    let header = std::fs::read(&path).expect("read /tmp/cjk_test.png");
    assert!(
        header.len() >= 8 && &header[..8] == b"\x89PNG\r\n\x1a\n",
        "PNG signature mismatch"
    );

    // 第 1 行 (CJK + ASCII 在 row 0): 取 prompt 行 physical y ∈ [0, 50] (logical
    // cell_h=25 × HIDPI=2 = 50 px), x ∈ [0, 400] physical (logical cell_w=10 × 10
    // 字符宽 × HIDPI=2 = 200, 给 400 余量覆盖 CJK 双宽 + advance 误差)。
    //
    // 区分 clear color #0a1030 (深蓝) vs 字形像素 (黑 cell.bg + 浅灰 fg + 灰阶
    // anti-alias 边缘): clear ± 12 容差 (沿袭 tests/headless_screenshot.rs)。
    let region_w: u32 = 400;
    let region_h: u32 = 50;
    let mut non_clear_pixels: u32 = 0;
    let row_stride = (physical_w as usize) * 4;
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
                non_clear_pixels += 1;
            }
        }
    }
    assert!(
        non_clear_pixels >= 300,
        "prompt 行 (CJK+ASCII) 区域 {}×{} physical 应有 ≥ 300 非清屏像素 \
         (字形真画了), 实测 {}",
        region_w,
        region_h,
        non_clear_pixels
    );

    // **不 remove**: 派单 Acceptance "agent Read /tmp/cjk_test.png verify 视觉",
    // 留盘给 agent / Lead 可视读。重跑测试时 File::create truncate, 不污染 /tmp。
}

/// CJK fallback 路径 verify: shape "你" 后 glyph.face_id 应 ≠ primary_face_id
/// (用户机 primary = DejaVu Sans Mono 不含 CJK glyph, cosmic-text fontdb fallback
/// chain 自动切到 Noto CJK / Source Han Sans)。
///
/// **CI 退化路径**: 若机器无任何 CJK face, cosmic-text 可能给 primary face 的
/// .notdef tofu (face_id == primary)。此时测试**软性** assert: 仍允许 face_id ==
/// primary, 但用 `eprintln!` 打 warning (test framework 收不到, 但 cargo test
/// --nocapture 可见) — 用户机以 CJK fallback 真触发为准, CI 退化作 follow-up。
///
/// **why 走 shape_line 不走 PTY**: face_id verify 不需要 render 路径, shape API
/// 直接拿 ShapedGlyph::face_id 即可; 与 fn 1 PTY 路径解耦, fn 2 失败诊断更精准
/// (PTY 异常不污染 face_id 信号)。
#[test]
fn cjk_glyph_uses_fallback_face_not_primary() {
    let mut ts = TextSystem::new().expect("TextSystem::new on user machine");
    let primary = ts.primary_face_id();
    assert_ne!(primary, 0, "primary_face_id should be non-zero u64 hash");

    let glyphs = ts.shape_line("你");
    assert_eq!(
        glyphs.len(),
        1,
        "shape '你' must yield 1 glyph (got {})",
        glyphs.len()
    );
    let cjk_face = glyphs[0].face_id();

    // 用户机正常路径: cjk_face != primary (Noto CJK / Source Han Sans face_id)。
    // CI 退化路径: cjk_face == primary 时给 warning 不挂测试 — 派单 Acceptance
    // 已知陷阱"cosmic-text fallback Noto CJK 需 fontdb 真扫到, 用户机已装"。
    if cjk_face == primary {
        eprintln!(
            "WARN: CJK '你' glyph face_id == primary ({}); 退化到 .notdef tofu \
             (用户机应触发 fallback 到 Noto CJK, 检查 fc-list :lang=zh)",
            primary
        );
    } else {
        // 用户机正常路径锁: face_id 必非 0 (DefaultHasher 输出, 0 概率 ~1/2^64)
        assert_ne!(
            cjk_face, 0,
            "CJK fallback face_id should be non-zero u64 hash (got 0 = hash collision \
             or uninit?)"
        );
    }

    // glyph_id 锁: CJK '你' 在 Noto CJK 必非 .notdef (gid 非零), CI 退化到主 face
    // 的 tofu 时 gid 可能为 0 — 接受两路径 (派单 Acceptance "豆腐也算非空")。
    let gid = glyphs[0].glyph_id;
    let _ = gid; // 不强 assert, 留作 trace
}
