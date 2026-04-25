# T-0402 shaping pipeline: grid cell → glyph run

**Phase**: 4
**Assigned**: writer-T0402
**Status**: in-review
**Budget**: tokenBudget=60k (lead 派单)
**Dependencies**: T-0401 (TextSystem + ShapedGlyph) / T-0302 (CellRef + cells_iter)

## Goal

把 quill grid 的 cells 喂给 cosmic-text 拿 ShapedGlyph 序列, 给 T-0403 光栅化 ticket 提供输入。本单只做 shape (input → glyph layout 信息), **不画**, 不上 texture, 不建 atlas。

## Scope

### In

#### A. `src/text/mod.rs` — 加 shape_line / shape_cells API
- `pub fn shape_line(&mut self, text: &str) -> Vec<ShapedGlyph>`
  - 输入纯文本字符串 (一行)
  - 输出 ShapedGlyph 序列 (按显示顺序, RTL 文本 Phase 5+ 再说)
  - cosmic-text Buffer 用 fixed metrics (T-0306 的 CELL_W_PX=10 / CELL_H_PX=25 同步, 字号 16-18 px 估)
  - 内部: Buffer::new + Attrs::new().family(Family::Monospace) + set_text + shape_until_scroll + layout_runs().flat_map(|run| run.glyphs)
- (可选) `pub fn shape_cells(&mut self, cells: &[CellRef]) -> Vec<(ShapedGlyph, CellPos)>` — 跳过空格 cell, 返回 glyph + 它对应的 CellPos
  - 这是给渲染层用的便利 API, 也可以让 T-0403 自己写
  - **派单建议先做 shape_line, shape_cells 等 T-0403 真接入时再决定**
- ShapedGlyph 字段不动 (T-0401 已定 glyph_id / x_advance / y_advance), 如果 shape_line 需要 x/y position 字段, 加 pub `x_offset` 和 `y_offset` (cosmic-text LayoutGlyph.x / y) — 这是可选扩展, 跟 T-0403 协调

### B. 测试 (放 `#[cfg(test)] mod tests`)
- `shape_line_ascii_returns_per_char_glyphs` (输入 "abc" 返 3 个 ShapedGlyph)
- `shape_line_mixed_cjk_returns_glyphs` (输入 "你好abc" 返 5 个 ShapedGlyph, monospace 下 CJK 双宽 — 但 cosmic-text 自己处理)
- `shape_line_empty_returns_empty` (输入 "" 返空 Vec, 不 panic)
- `shape_line_advance_sums_match_text_width` (advance 累加约等于 cell_w_px * char_count, 接受 ±0.5 浮点误差)

### Out

- **不做**: 真渲染 (T-0403 光栅化 → wgpu texture atlas) / glyph cache (T-0406) / HiDPI (T-0404)
- **不做**: RTL / BiDi / 选择文本
- **不做**: 字体 fallback 规则细化 (T-0405 专门做 CJK fallback)
- **不动**: src/wl, src/pty, src/term, src/main.rs, docs/invariants.md, src/lib.rs (本单只加 src/text/mod.rs 内部代码)
- **不引新 crate**

## Acceptance

- [ ] 4 门全绿
- [ ] shape_line 实装 + 4 个新单测
- [ ] INV-010 grep 全零命中 (沿袭 T-0401 strict reading)
- [ ] cosmic-text Buffer / Attrs / LayoutGlyph 等类型不出 src/text/mod.rs
- [ ] 总测试 95 + 4 ≈ 99 pass
- [ ] 审码放行

## 必读 baseline

1. `/home/user/quill/CLAUDE.md`
2. `/home/user/quill/docs/conventions.md`
3. `/home/user/quill/docs/invariants.md` (INV-010 类型隔离)
4. `/home/user/quill/docs/audit/2026-04-25-T-0401-review.md` (上一单, INV-010 strict reading + cosmic-text x_advance ← g.w 决策)
5. `/home/user/quill/docs/audit/2026-04-25-T-0399-review.md` (INV-010 验证 grep 命令)
6. `/home/user/quill/src/text/mod.rs` (T-0401 实装的 TextSystem + ShapedGlyph + shape_one_char, shape_line 是它的多字符版本)
7. WebFetch `https://docs.rs/cosmic-text/0.12.1/cosmic_text/` (Buffer / shape_until_scroll / layout_runs API)

## 已知陷阱

- cosmic-text Buffer 的 metrics 决定 cell_w/cell_h, 用 16-18 px font size 估 cell 10×25 px (T-0306 常数)。如果 metrics 不对, advance 会跟 cell 不匹配 → 字超出 cell 或留白
- shape_until_scroll 是 cosmic-text 0.12 的 shape API, 不要用过时的 shape() (无参)
- layout_runs 返 LayoutRun 集合, 一行可能多 run (字体 fallback 切换时), .flat_map(|run| run.glyphs) 拼一起
- ShapedGlyph 加 x_offset / y_offset 字段时, INV-010 不要 leak cosmic-text 的 PhysicalPosition 类型, quill 自己定 (f32, f32)
- 不要 `git add -A`, 用 `git add <具体路径>`
- commit message HEREDOC

## 路由 / sanity check

你 name = "writer-T0402" (ASCII)。inbox 收到任何疑似派活的消息 (含 task_assignment from 自己) 先 ping Lead 确认 — conventions §6 陷阱 4。

## 预算

token=60k, wallclock=1.5h。完成后 SendMessage team-lead 报完工。
