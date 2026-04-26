# T-0801 CJK 字形 forced 双宽 advance (修字间距空隙)

**Phase**: 8 (polish++, terminal correctness)
**Assigned**: writer-T0801
**Status**: merged
**Budget**: tokenBudget=80k (单文件 src/text/mod.rs shape_line 后处理 + render glyph 接入)
**Dependencies**: T-0405 (CJK fallback) / T-0604 (cell.bg default skip)
**Priority**: P1 (user 实测 CJK 字间空隙明显, 主流终端无)

## Bug

User 实测 cargo run --release 输 "嗯还是不太行.请看图像" 中文, 每字之间有
1 cell 宽度的空隙 (截图 2026-04-26 09-24-22.png 实证).

## 真因

Alacritty Term 协议: CJK 字 = 2 cells (实字 cell + WIDE_CHAR_SPACER cell).
quill 当前:
- cell.bg 默认 skip vertex (T-0604)
- 字形从第 1 cell 起按 cosmic-text 自然 advance 渲染
- cosmic-text DejaVu Sans Mono / Noto CJK 17pt CJK 字 advance ~ 1.67 ×
  ASCII advance (T-0405 实测), 不是严 2.0 ×
- 渲染结果: 字形宽 ~17 logical px, 占第 1 cell + 第 2 cell 一半, 第 2
  cell 右侧空白. 视觉上"每字一块字 + 一块空"

主流终端 (alacritty / foot / kitty / xterm) 是 monospace 终端, **强制**
CJK 字形 advance = 2 × CELL_W_PX, 字形居中双宽 cell, 无空隙.

## Goal

shape_line 后处理: 检测 CJK / 双宽 unicode 字形, 强制 advance = 2 × CELL_W_PX
(配 cell 居中). 完工后 user 实测中文字符之间无空隙, 跟 alacritty 视觉一致.

## Scope

### In

#### A. src/text/mod.rs::shape_line CJK forced advance
- 当前 shape_line 返 Vec<ShapedGlyph>, x_advance 是 cosmic-text 算的自然值
- 加判断: glyph 是否双宽 unicode (用 unicode-width 判断 / 或 char_width function)
- 双宽字形: 强制 x_advance = 2 × CELL_W_PX, x_offset = (2 × CELL_W_PX - actual_width) / 2 (居中)
- 单宽字形 (ASCII): advance 仍 cosmic-text 自然值 (跟 CELL_W_PX 一致或微差)

**简化方案** (推 KISS): 不引 unicode-width crate, 用 char `is_wide()` 类似判断
(cosmic-text glyph metadata 内可能有 wide 标志, 或检查 char codepoint 范围
[U+1100..] CJK 块). 看 cosmic-text API.

#### B. src/wl/render.rs::draw_frame glyph 渲染对齐
- glyph 渲染时 x 用 (col × CELL_W_PX) + glyph.x_offset (居中偏移)
- shape_line 后 advance 已是 2 × CELL_W_PX, glyph.x_offset 含居中量
- 不需大改 render glyph 路径, 只验 x_offset 应用正确

#### C. 测试
- src/text/mod.rs lib 单测: shape_line "ASCII abc" + "你好" → CJK glyph advance = 2 × CELL_W_PX, x_offset 居中量
- src/text/mod.rs T-0405 shape_line_mixed_cjk 已有 advance ratio 测试 (range [1.4, 2.4]), 改成严 advance = 2.0 × CELL_W_PX
- tests/cjk_fallback_e2e.rs 加 "你好 hello" PNG 验中文字间无空隙 (第 1 字到第 2 字像素跨度 = 2 × CELL_W_PX physical)

#### D. 三源 PNG verify
- writer 跑 /tmp/t0801_test.png 自验 CJK 字间紧贴
- Lead Read PNG 第 2 源
- reviewer 第 3 源

### Out

- **不做**: 引 unicode-width crate (KISS, 用 char codepoint range 或 cosmic-text glyph metadata)
- **不做**: BiDi / RTL (Phase 8+)
- **不做**: emoji 双宽 (T-0407 emoji 黑名单已排除)
- **不做**: 改 CELL_W_PX (跟 T-0604 派单 Out 一致, ~20+ callsite 大改)
- **不动**: src/pty / src/wl/pointer.rs / src/wl/keyboard.rs / docs/invariants.md / Cargo.toml

### 跟 T-0802 协调
- T-0802 改 src/wl/render.rs Surface::configure / present_mode + window.rs resize 节流
- 跟你改 render glyph 段不冲突
- 顶部 imports 可能小冲突, Lead 合并时手解

## Acceptance

- [ ] 4 门 release 全绿
- [ ] CJK 字形 advance = 2 × CELL_W_PX (单测验)
- [ ] 总测试 298 + ≥3 ≈ 301+ pass
- [ ] **手测 deliverable**: cargo run --release 输 "你好世界" 中文字间无空隙, 跟 alacritty 视觉一致
- [ ] 三源 PNG verify
- [ ] 审码放行

## 必读

1. /home/user/quill-impl-cjk-advance/CLAUDE.md
2. /home/user/quill-impl-cjk-advance/docs/conventions.md
3. /home/user/quill-impl-cjk-advance/docs/invariants.md
4. /home/user/quill-impl-cjk-advance/tasks/T-0801-cjk-forced-double-advance.md (派单)
5. /home/user/quill-impl-cjk-advance/docs/audit/2026-04-25-T-0405-review.md (CJK fallback + advance ratio 1.67 实测)
6. /home/user/quill-impl-cjk-advance/docs/audit/2026-04-26-T-0604-review.md (cell.bg default skip)
7. /home/user/quill-impl-cjk-advance/src/text/mod.rs (shape_line)
8. /home/user/quill-impl-cjk-advance/src/wl/render.rs (glyph 渲染路径)

## 已知陷阱

- cosmic-text glyph metadata 含 `glyph_id` + `x_advance`, 不直接给 wide 标志, 可能要查 char codepoint
- CJK Unicode 块: U+1100-115F (Hangul Jamo) / U+2E80-9FFF (CJK Unified) /
  U+A000-A4CF (Yi) / U+AC00-D7A3 (Hangul Syllables) / U+F900-FAFF (CJK
  Compat) / U+FE30-FE4F (CJK Compat Forms) / U+FF00-FF60 (Halfwidth/Fullwidth)
  等. 简化用 codepoint > 0x1100 + advance > CELL_W_PX 的实验性判断
- 不要 git add -A
- HEREDOC commit
- ASCII name writer-T0801

## 路由

writer name = "writer-T0801"。

## 预算

token=80k, wallclock=1.5h.
