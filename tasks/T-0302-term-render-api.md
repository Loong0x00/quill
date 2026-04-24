# T-0302 Term 渲染 API 准备

**Phase**: 3
**Assigned**: 写码-close
**Status**: in-review
**Budget**: tokenBudget=80k(lead 派单未明)
**Dependencies**: T-0301(Term 已集成)

## Goal

给 T-0305 色块渲染准备好的 API 层:cells_iter / dirty tracking / cursor 可见性。
**不做渲染本身**,只在 `src/term/mod.rs` 添加公开方法,让 Phase 3 的渲染代码
(T-0305 起)有明确入口,不用绕到 alacritty_terminal 的内部类型。

## Scope

- In(都落到 `src/term/mod.rs`):
  - `pub fn cells_iter() -> impl Iterator<Item = (Point, Cell)>`:遍历当前
    viewport 的所有 cell,渲染可以 `for (pt, cell) in term.cells_iter() { render cell.c at pt.col, pt.line }`
  - Dirty tracking:alacritty 自带 `term.damage()` 返回 `TermDamageIterator`;
    我们包一层 `pub fn damaged_lines() -> Vec<usize>` 或 iterator,给 T-0305
    用来判断"只重画哪些行"
  - `pub fn cursor_visible() -> bool`:从 `term.mode()` 读 `TermMode` 的
    `SHOW_CURSOR` bit
  - `pub fn cursor_shape() -> CursorShape`:alacritty 的 cursor 有 block /
    beam / underline 三种;暴露给渲染选择
  - 不破坏现有 `cursor_point` / `line_text` / `advance`
- Out:
  - 渲染(T-0305)
  - resize 同步(T-0306)
  - 具体 cell 属性(fg/bg color)—— 暂不暴露,T-0305 再看

## Acceptance

- [ ] 四门全绿
- [ ] 新 API 各至少一条单测(spawn 测字节,检查 iter 输出 / damage / cursor 可见)
- [ ] 审码 放行
