# T-0803 改 grid 占位判定为 unicode-width (PUA / nerd font 对齐 lsd)

**Phase**: 8 (polish++, terminal correctness)
**Assigned**: TBD (writer)
**Status**: open
**Budget**: tokenBudget=80k (text/mod.rs 改 ShapedGlyph + force_cjk_double_advance + 调用方签名 + 测试)
**Dependencies**: T-0801 (CJK forced double advance, 当前 natural-advance-ratio 判 wide)
**Priority**: P1 (user 实测 lsd nerd font 输出错位 + 复制错位, 截图 2026-05-02 17-39-23 / 17-39-55)

## Bug

User 实测 quill 里 `lsd --icon always` (或任何 starship / fzf 含 nerd font 图标的 prompt)
图标后续字符整行错位, 选区复制时实际选中范围与视觉不符.

视觉症状: 图标占 2 cell, 但 lsd 内部按 1 cell 算输出列宽, 后续 padding 短 1 cell,
整行往左偏 1 cell per icon.

## 真因

quill 当前 `force_cjk_double_advance` (src/text/mod.rs:506-535, T-0801 实装) 判 wide
用 **字形 natural advance ratio** (`natural > 1.5 × CELL_W_PX_phys`):

- 中文字形 ~1.67 cell → 算 wide → 2 cell ✓ (与 unicode-width 一致, 巧合)
- ASCII ~1.0 cell → 算 narrow → 1 cell ✓
- Nerd Font 图标 (PUA U+E000..F8FF) ~1.67 cell → 算 wide → 2 cell ✗

**主流工业标准** (alacritty / ghostty / kitty / wezterm / lsd / fzf / starship)
全用 unicode-width crate (POSIX wcwidth + Unicode East Asian Width 表):

- CJK W (Wide) → 2 cell
- ASCII Na (Narrow) → 1 cell
- PUA N (Neutral) → 1 cell (默认)
- Emoji → 2 cell

quill 当前判定与工业标准在 PUA 上分叉 → lsd / fzf / nerd font CLI 全错位.

**根本设计问题**: quill 把 "grid 占位" 与 "字形渲染宽度" 耦合 (字形宽度同时
决定二者). 工业界标准是解耦:

- grid 占位 = unicode-width (Unicode 标准, 跨工具一致)
- 字形渲染 = 字形原宽 (字形如果超 cell 由渲染层 squeeze)

## Goal

`force_cjk_double_advance` 改用 unicode-width 判 wide, grid 占位与 lsd / ghostty
/ alacritty 对齐. 字形渲染层 (T-0801 实装的 wide → 2 cell + 居中) 路径保留,
仅判定来源换. 完工后 user 实测 `lsd --icon always` 输出对齐, 选区复制坐标准.

## Scope

### In

#### A. ShapedGlyph 加 cluster 字段 (src/text/mod.rs)
- `pub cluster: usize` (从 cosmic-text `LayoutGlyph.start` 拿, 0.12 已稳)
- `from_cosmic_glyph` 赋值 `cluster: g.start`
- INV-010 守: cluster 是 quill 基础类型 (usize), 不暴露 cosmic-text 类型

#### B. shape_line 签名加原文 &str (src/text/mod.rs)
- `shape_line(&mut self, text: &str)` 已有 text 参数, 透传给
  `force_cjk_double_advance(emoji_filtered, text)`
- force_cjk_double_advance 改签名 `(glyphs, original: &str) -> Vec<ShapedGlyph>`

#### C. force_cjk_double_advance 改用 unicode-width (src/text/mod.rs)
```rust
let ch = original[g.cluster..].chars().next().unwrap_or(' ');
let is_wide = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(1) >= 2;
```
- 删除 natural advance ratio 判定
- wide threshold / double_w / center_pad 计算逻辑保留 (字形渲染层不变)

#### D. Cargo.toml 加 unicode-width
- `unicode-width = "0.1"` (alacritty / lsd / fzf / cosmic-text 全用此 crate)
- 走 ADR 0010 (新 dependency)

#### E. 测试
- `tests::shape_line_pua_nerd_font_icon_advance_eq_one_cell` — U+E0B0 (常见 nerd
  font 图标) advance = CELL_W_PX_phys
- `tests::shape_line_cjk_glyphs_have_forced_double_advance` (T-0801 已有) —
  断言不变 (中文仍 2 cell)
- `tests::shape_line_ascii_advance_equals_cell_w_phys` (T-0801 已有) — 不变
- `tests::shape_line_emoji_advance_eq_double_cell` — emoji 2 cell

### Out

- 字形 squeeze 渲染 (字形 metric > 1 cell 时 scale_x 压到 cell 宽). PUA 图标
  现在会画在 1 cell 内但字形原宽 ~1.67 cell, 视觉上字形会超出 cell 边界.
  squeeze 走另开 ticket (Phase 后期 polish), 本 ticket 不动渲染.
- IME preedit 渲染对齐 bug (截图 2026-05-02 17-39-23 cursor 错位). 走另开
  ticket T-0804.
- alacritty_terminal `Term` cursor advance 路径 — 它内部用自己的 wide 判定
  (跟 unicode-width 一致), 不在本 ticket 范围.

## Acceptance

1. `cargo test text::tests` 全过 (含新增 PUA + emoji 测试)
2. `cargo clippy --all-targets -- -D warnings` 无 warning
3. user 实测 `quill -e 'lsd --icon always /home/user'` 输出对齐 (跟 ghostty 视觉一致)
4. 三源 PNG verify (writer 跑 headless screenshot + Lead Read PNG)

## 已知 trade-off

- nerd font 图标会被压在 1 cell 内, 字形宽 ~1.67 cell 超出 cell 边界, 视觉
  上图标可能与下一字符字形重叠几个像素. 这是 ghostty / alacritty 都接受的
  trade (ghostty 走 squeeze 渲染消重叠, 见 Out 项延后 ticket).
- 若 user 偏好图标显示宽敞, 可改 nerd font 选 "Mono" 变体 (字形 1 cell),
  与本 ticket 配合视觉最佳.

## 相关

- T-0801 (CJK forced double advance, 本次修正其判定来源)
- ADR 0010 (待写, unicode-width crate)
- 业界对照: alacritty/src/grid/mod.rs, kitty/kitty/wcwidth-std.h, ghostty/src/terminal/Screen.zig
