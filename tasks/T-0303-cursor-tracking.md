# T-0303 光标追踪 API 完善

**Phase**: 3
**Assigned**: 写码-close
**Status**: in-review
**Budget**: tokenBudget=40k(lead 派单)
**Dependencies**: T-0302(CellPos / cursor_visible 已就绪)

## Goal

清掉 T-0302 留的三个 cursor open question(见 `docs/audit/2026-04-25-T-0302-review.md`):
1. 加 `pub fn cursor_shape() -> CursorShape`(T-0302 删,T-0303 加回)
2. 改 `cursor_point() -> (usize, i32)` 为 `cursor_pos() -> CellPos`(消 i32 类型污染)
3. cursor_visible 已存在,保留不动

沿袭 T-0302 的类型隔离原则:**不 re-export** alacritty CursorShape,quill 自己定 enum;私有 `fn from_alacritty` 转换器(inherent fn,非 From trait)。

## Scope

- In(都落到 `src/term/mod.rs`):
  - `pub enum CursorShape { Block, Underline, Beam, HollowBlock, Hidden }` —— 完整映射 alacritty 0.26 的 5 个 variants
  - `impl CursorShape { fn from_alacritty(s: alacritty::CursorShape) -> Self }` 模块私有
  - `pub fn cursor_shape(&self) -> CursorShape` 读 `self.term.cursor_style().shape` 走转换
  - 删 `pub fn cursor_point() -> (usize, i32)`,新 `pub fn cursor_pos() -> CellPos` 复用 `CellPos::from_alacritty`
  - 改测试:删 `cursor_point` 相关,加 `cursor_shape_*` + `cursor_pos_*`
- Out:
  - 渲染(T-0305)/ resize(T-0306)/ scrollback(T-0304)
  - cursor blinking(`CursorStyle.blinking`)
  - 任何 src/wl, src/pty, src/main.rs

## Acceptance

- [ ] 4 门全绿
- [ ] CursorShape 5 个 variant 单测覆盖(default + DECSCUSR 切换)
- [ ] cursor_pos 替换 cursor_point,旧测试更新
- [ ] 审码 放行
