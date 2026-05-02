# ADR 0010 — 引入 unicode-width crate (grid 占位与字形渲染解耦)

**Status**: Accepted
**Date**: 2026-05-02
**Phase**: 8 (terminal correctness)
**Related**: T-0801 (CJK forced double advance), T-0803 (本次落地)

## Context

T-0801 实装 CJK 强制双宽 advance 时, 判 wide/narrow 用了 **字形 natural advance
ratio** (`natural > 1.5 × CELL_W_PX_phys`), 当时 KISS 跳过 unicode-width crate.

实战暴露 corner case: Nerd Font 图标 (Unicode PUA U+E000..F8FF) 字形宽
~1.67 cell, 触发 wide → quill 渲染 2 cell. 但 lsd / fzf / starship / ghostty
全用 unicode-width 算 PUA = 1 cell, grid 视图错位.

## Decision

引入 `unicode-width = "0.1"` 作为 grid 占位判定的唯一权威来源, 把"判定 (1 vs
2 cell)"与"字形渲染 (字形画多宽)"解耦:

- **判定**: `unicode_width::UnicodeWidthChar::width(ch)` (POSIX wcwidth +
  Unicode East Asian Width 表)
- **渲染**: 字形按 cosmic-text 自然 advance 画, T-0801 的居中 + 强制 advance
  逻辑保留 (但 wide/narrow 来源换)

## Alternatives 考察

### A. 沿用 natural advance ratio (现状)

- ✓ 无新 dependency
- ✗ PUA / 部分 emoji 误判, 与所有主流终端 + CLI 不对齐
- ✗ 跨字体不稳: 同一 codepoint 在不同 face 字形宽度可能不同, 判定漂移
- ✗ 字体 fallback 切到 tofu 时 advance 退化, wide CJK 被误判 narrow

### B. 引入 unicode-width crate (本 ADR 选择)

- ✓ 与 alacritty / lsd / fzf / starship / ghostty / cosmic-text 全部对齐
- ✓ 跨字体稳定, 不依赖字形 metric
- ✓ POSIX wcwidth 兼容, 处理 emoji / ZWJ / VS15/16 等 corner case 已成熟
- ✓ 维护活跃 (unicode 14.0+ 块覆盖到位)
- ✗ 加一个 crate (但 cosmic-text 自己已 transitive 依赖, 无新二进制开销)

### C. 自己 hardcode East Asian Width 表

- ✓ 无新 dependency
- ✗ Unicode 每年加新块, 自维护表必滞后
- ✗ ZWJ / emoji modifier / VS 序列处理复杂
- ✗ 非战略价值, 重复造轮

## Consequences

### 正向

- grid 占位与工业标准对齐 (核心目标)
- 解决 T-0803 lsd / nerd font 错位 bug
- 为未来 emoji ZWJ 序列 / VS15-16 selector 处理打基础 (unicode-width 0.1.13+
  支持)

### 负向 / 待跟进

- nerd font 图标在 1 cell 内字形可能超出边界 (字形 ~1.67 cell vs cell 1.0).
  视觉上邻字符可能像素级重叠. 工业界 (ghostty / kitty) 走 squeeze 渲染解决
  (字形 scale_x = cell_w / natural_advance), quill 走另开 ticket Phase 后期
  落地. 本 ADR 范围仅 grid 判定.
- 用户若想图标显示宽敞, 推荐 "Nerd Font Mono" 变体 (字形原生 1 cell).

## 落地

- 加 `unicode-width = "0.1"` 到 Cargo.toml [dependencies]
- 改 `force_cjk_double_advance` 用 `UnicodeWidthChar::width`
- ShapedGlyph 加 `cluster: usize` 字段 (从 cosmic-text `LayoutGlyph.start`)
- 改 shape_line 把原文 &str 透传给 force_cjk_double_advance
- 详 T-0803 ticket
