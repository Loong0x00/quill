# T-0304 滚动 buffer 基础

**Phase**: 3
**Assigned**: 写码-T0304
**Status**: merged
**Budget**: tokenBudget=50k (lead 派单)
**Dependencies**: T-0302 (CellPos 已就绪) / T-0303 (cursor API 完整)

## Goal

让 `TermState` 能查询历史行 (滚动出 viewport 之外的行), 给后续 T-0305 渲染层 + 未来 scroll-up UI 用。alacritty 的 `Term.history_size()` + `Grid<>::iter()` 已经实装 scrollback storage, 本单只是给 quill 加 pub API 暴露。

## Scope

### In (都落到 `src/term/mod.rs`)

- **新类型**: `pub struct ScrollbackPos { pub row: usize }` — 独立类型, **不**扩 `CellPos` enum (审码 T-0303 P3-2 推荐: 不破坏现有 cells_iter / cursor_pos 调用方)
  - `row=0` 表示最旧的历史行 (top of scrollback)
  - `row=history_size()-1` 表示最新刚滚出 viewport 的那一行
  - viewport 行**不**用 ScrollbackPos, 仍用 `CellPos { col, line }` (line ∈ 0..rows)
- **新方法** (3 个):
  - `pub fn scrollback_size(&self) -> usize` — 当前历史行数 (alacritty `term.history_size()`)
  - `pub fn scrollback_line_text(&self, pos: ScrollbackPos) -> String` — 读某历史行的文本 (类似现有 `line_text` 但走 scrollback)
  - `pub fn scrollback_cells_iter(&self, pos: ScrollbackPos) -> impl Iterator<Item = CellRef> + '_` — 历史行的 cell 迭代 (返回 CellRef 复用现有类型, CellRef.pos 的 line 字段填 0 即可, **位置语义由 ScrollbackPos 单独承载**)
- **私有转换** (沿袭 T-0302/0303 类型隔离):
  - alacritty 内部用 `Line(i32)` 表 scrollback 行 (负值 = 历史), 私有 fn 转换 `quill ScrollbackPos { row: usize }` → alacritty 内部坐标
  - 不 re-export 任何 alacritty scrollback 类型
- **测试** (放 `#[cfg(test)] mod tests`):
  - `scrollback_size_zero_initially`
  - `scrollback_size_grows_after_overflow` (advance 大量字节让 viewport 滚出去)
  - `scrollback_line_text_returns_oldest_first`
  - `scrollback_cells_iter_yields_chars` (验证迭代 cells 字符匹配)

### Out

- **不做**: 渲染 (T-0305) / resize (T-0306) / scroll-up UI / 选择文本 / 复制
- **不动**: src/wl, src/pty, src/main.rs, src/event_loop.rs
- **不改 alacritty 配置**: 用默认 history size (`alacritty_terminal::term::Config` 默认 10000 行)
- **不引新 crate / 不写 ADR / 不动 docs/invariants.md**

## Acceptance

- [ ] 4 门全绿 (`cargo build` / `cargo test` / `cargo clippy --all-targets -- -D warnings` / `cargo fmt --check`)
- [ ] ScrollbackPos 是独立类型, 不扩 CellPos enum
- [ ] from_alacritty 模块私有 inherent fn (非 From trait), 沿袭 T-0302 P0-3 决策
- [ ] alacritty scrollback 类型零暴露 (grep `pub.*alacritty::` 零命中)
- [ ] 4 个新测试覆盖 scrollback API
- [ ] 审码放行 (P0/P1/P2 全过)

## 必读 baseline (fresh agent 启动顺序)

1. `/home/user/quill/CLAUDE.md` — 项目治理
2. `/home/user/quill/docs/conventions.md` — 写码 idiom (commit / 注释 / 测试 / 流程)
3. `/home/user/quill/docs/invariants.md` — INV-001..009
4. `/home/user/quill/docs/audit/2026-04-25-T-0202-T-0303-handoff.md` — 5 主题结晶 (类型隔离 §1 必读)
5. `/home/user/quill/docs/audit/2026-04-25-T-0303-review.md` — 上一单 audit, T-0304 P3-2 是 ScrollbackPos 推荐源头
6. `/home/user/quill/src/term/mod.rs` — 主要改动文件, 看现有 CellPos / cursor_pos / line_text 模式照抄

## 给 fresh agent 的提醒

- 你是新接班, 没有任何 quill 上下文。务必先按必读顺序读完 6 个文件 (~30 min)
- 类型隔离 SOP 是 P0 阻塞规则 — 绝对不 re-export alacritty 类型, 绝对不 `impl From` trait
- commit message 用 HEREDOC + body 4 段格式 (conventions.md §1)
- 4 门全绿才提审, 不全绿继续改
- 提审 SendMessage 给审码: 含分支 + commit hash + 改动文件 + 重点 review 项 + 4 门状态 + scope 对照表
