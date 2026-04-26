//! T-0602 集成测试: scrollback 滚动端到端 (term + render_headless PNG verify).
//!
//! **覆盖** (派单 In #F "tests/scrollback_e2e.rs 集成测试: 喂 100 行字,
//! scroll_display(50), render_headless PNG, 验顶部显示是 50 行前的内容"):
//!
//! - **scroll_changes_render_and_reset_returns_active**: 单 test 三阶段断言
//!   (active → scroll(50) → reset_display) — active 渲染 vs scroll 渲染像素
//!   分布不同 (滚动真改变 viewport 内容); reset_display 后 PNG 应与原 active
//!   完全一致 (语义闭环).
//! - **advance_after_scroll_auto_resets**: 喂新字节后 display_offset 自动归零
//!   (advance hook 行为, 见 term::tests::advance_with_nonempty_bytes_*).
//!
//! **why 把 3 个 render 整合到 1 test**: 多 test 并行调 `render_headless` 会
//! 同时创建多个 wgpu Instance, NVIDIA Vulkan 驱动在并发 6+ 个 instance 时
//! 偶发 SIGSEGV (实测 SIGSEGV 在 4-test 并行版重现, 单线程版稳定). 整合到
//! 1 test 避免并发, 与 T-0408 `headless_screenshot` 走 helper 复用同精神.
//!
//! **why 集成测试 (而非 unit)**: render_headless 走真 wgpu offscreen Texture +
//! cosmic-text shape + atlas raster, 端到端覆盖 cells_iter remap + display_text
//! 与 cell 几何对齐. unit 测能锁 term API 但锁不住"渲染层真用了对的 row text".
//!
//! 走 release build (与 T-0408 集成测试同级别), CI 路径需要 GPU. 实际依赖
//! `quill::wl::render_headless` 公共 API + `TermState` 滚动 API.

use quill::term::TermState;
use quill::text::TextSystem;
use quill::wl::{render_headless, HIDPI_SCALE};

const COLS: u16 = 80;
const ROWS: u16 = 24;
const LOGICAL_W: u32 = 800;
const LOGICAL_H: u32 = 600;
const PHYSICAL_W: u32 = LOGICAL_W * HIDPI_SCALE;
const PHYSICAL_H: u32 = LOGICAL_H * HIDPI_SCALE;

/// 喂 100 行 "L_NN_<filler>\r\n" 的 term, 每行有可识别的行号 + 中等字符密度
/// (不会全空行, render 走 glyph 路径). 100 行远超 24 viewport, scrollback 累积
/// 76+ 行.
fn populate_term_with_100_lines() -> TermState {
    let mut term = TermState::new(COLS, ROWS);
    for i in 0..100 {
        // 每行写 ~30 字符: "L00_xxx...yyy" 至 "L99_xxx...yyy"  (空格 + 数字 + 字母)
        let line = format!("L{:02}_{}{}\r\n", i, "x".repeat(20), "y".repeat(5));
        term.advance(line.as_bytes());
    }
    term
}

/// 把 term 当前 viewport 渲染成 PNG RGBA (走 render_headless, 与 T-0408 同
/// 路径). 返 (rgba bytes, physical_w, physical_h).
fn render_term_to_rgba(term: &TermState, ts: &mut TextSystem) -> (Vec<u8>, u32, u32) {
    let cells: Vec<_> = term.cells_iter().collect();
    let (cols, rows) = term.dimensions();
    let row_texts: Vec<String> = (0..rows).map(|r| term.display_text(r)).collect();
    render_headless(
        ts, &cells, cols, rows, &row_texts, LOGICAL_W, LOGICAL_H, None, None,
        None, // T-0607 selection
    )
    .expect("render_headless failed")
}

/// **派单 In #F 主 acceptance 三阶段闭环**: active → scroll(50) → reset.
///
/// Stage 1 (active): 100 行喂完的 term 渲染 RGBA (display_offset = 0, viewport
/// 显示最末 24 行, 含 L76..L99).
///
/// Stage 2 (scroll 50): scroll_display(50) 后 viewport 应显 L26..L49 区段
/// (50 行前的内容). RGBA 与 stage 1 像素分布不同 — 派单原文"顶部显示是 50 行
/// 前的内容". 用像素 diff > 0.1% 阈值断言 (1920x1200 frame, glyph 笔画占比
/// ~5%, 滚 50 行改大部分行字符 → 笔画位置不同, 实测 0.34%, 阈值留 3× 余量).
///
/// Stage 3 (reset): reset_display 后 RGBA 应与 stage 1 **位等同** — 语义闭
/// 环锁 reset_display 真把 display_offset 置 0 (而非只 mark dirty).
///
/// **why 不 hard-code 顶行文本断言**: render_headless 内部 atlas raster 失败
/// 时跳字符 (T-0407 路径), pixel-perfect 顶部行字符位置脆 (字体抖动 / atlas
/// 重排). 派单原文"顶部显示是 50 行前的内容" 由 `term::tests::display_text_*`
/// 与 `term::tests::cells_iter_after_scroll_*` 已锁; 本集成测对端到端 render
/// 路径补一道兜.
#[test]
fn scroll_changes_render_and_reset_returns_active() {
    let mut ts = TextSystem::new().expect("TextSystem::new (need monospace face)");
    let mut term = populate_term_with_100_lines();
    assert!(
        term.scrollback_size() >= 50,
        "前置: 100 行写完 scrollback 应 >= 50, 实际 {}",
        term.scrollback_size()
    );

    // Stage 1: active render
    let (rgba_active, w, h) = render_term_to_rgba(&term, &mut ts);
    assert_eq!(w, PHYSICAL_W);
    assert_eq!(h, PHYSICAL_H);
    let total_pixels = (w as usize) * (h as usize);
    assert_eq!(
        rgba_active.len(),
        total_pixels * 4,
        "active RGBA byte len should be physical_w*physical_h*4"
    );

    // Stage 2: scroll 50 行看历史
    term.scroll_display(50);
    assert_eq!(
        term.display_offset(),
        50,
        "scroll_display(50) 后偏移应 = 50"
    );
    let (rgba_scrolled, _, _) = render_term_to_rgba(&term, &mut ts);

    // 像素 diff: 期望至少 0.1% 像素不同 (滚 50 行改大部分行内容 + 字形).
    // 1920x1200 frame 中 glyph 笔画 + cell.bg 块覆盖约 5-10% 像素 (24 行 ×
    // ~30 字符 × ~20 px² 笔画 = 14k-30k px), 滚 50 行改大部分行字符 → glyph
    // 笔画位置不同, diff 至少 1k+ px. 0.1% = 1920 px 是 robust 下限 (实测
    // 0.34%, 留 3× 余量防 atlas 抖动).
    let diff_pixels = rgba_active
        .chunks_exact(4)
        .zip(rgba_scrolled.chunks_exact(4))
        .filter(|(a, b)| a != b)
        .count();
    let diff_ratio = (diff_pixels as f64) / (total_pixels as f64);
    assert!(
        diff_ratio > 0.001,
        "scroll_display(50) 后像素分布应明显不同 (滚 50 行 → 大部分行字符变 → \
         glyph 笔画位置不同), 实测 diff {}/{} = {:.4}% < 0.1% 阈值",
        diff_pixels,
        total_pixels,
        diff_ratio * 100.0
    );

    // Stage 3: reset_display → 位等同 active
    term.reset_display();
    assert_eq!(term.display_offset(), 0, "reset_display 后应回 0");
    let (rgba_after_reset, _, _) = render_term_to_rgba(&term, &mut ts);
    assert_eq!(
        rgba_active, rgba_after_reset,
        "scroll(50) 后 reset_display, 渲染应与原 active 完全位等同"
    );
}

/// **T-0618 反转 T-0602**: scroll → advance(non-empty) **不**自动 reset 到底.
/// 主流终端 (alacritty / foot / kitty / iTerm2 / ghostty) 全一致: PTY 输出不动
/// 用户 viewport, 用户键盘类型 / 粘贴才跳底 (走 write_keyboard_bytes 路径).
///
/// 此处加端到端 render 路径走一遍验整链不挂掉 (display_offset 保持 + render
/// 仍能产生有效 RGBA).
#[test]
fn advance_after_scroll_keeps_display_offset_in_render_chain() {
    let mut ts = TextSystem::new().expect("TextSystem::new");
    let mut term = populate_term_with_100_lines();

    term.scroll_display(20);
    assert_eq!(term.display_offset(), 20);

    // 喂 1 字节 (模拟 PTY 子进程输出) → display_offset 应保持 20
    term.advance(b"Z");
    assert_eq!(
        term.display_offset(),
        20,
        "T-0618: PTY 输出不该 reset_display, viewport 应保持用户滚动位置"
    );

    // render 整链不挂; 视觉上由其它 test (scroll_changes_render_*) 端到端覆盖.
    let (rgba, w, h) = render_term_to_rgba(&term, &mut ts);
    assert_eq!(rgba.len(), (w as usize) * (h as usize) * 4);
}
