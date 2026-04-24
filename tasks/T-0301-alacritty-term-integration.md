# T-0301 alacritty_terminal Term 集成 + process_byte_slice 喂字节

**Phase**: 3
**Assigned**: 写码-close
**Status**: claimed
**Budget**: tokenBudget=100k(与 T-0108 共享)
**Dependencies**: T-0108(Core<State> 已就绪,可在 pty callback 里 &mut state.term)

## Goal

在 `State` 里持有一个 `alacritty_terminal::Term<TermListener>`,PTY callback 里拿到字节后 `term.advance(...)` / `processor.advance(&mut term, bytes)`(视 alacritty_terminal 0.26 API)让终端状态机消费字节。日志 `tracing::trace!` 打印行数 × 列数 + cursor 位置,证明 grid 在更新。**仍不渲染**,屏幕继续深蓝。

## Scope

- In:
  - `Cargo.toml` 添加 `alacritty_terminal = "0.26"`(ADR 0002 锁定,不需新 ADR)
  - 新模块 `src/term/mod.rs`:封装 `Term<TermListener>` 起手、`advance(bytes)` 出口
  - `State` 字段追加 `term: Option<TermState>`(放 `pty` 之后或之前,Rust 反向 drop 不依赖顺序,随 INV-001 不冲突即可)
  - PTY callback 拿到 `&buf[..n]` 后 `term.advance(&buf[..n])`,之后 `tracing::trace!` 打 grid cursor 位置
- Out:
  - 渲染 cell(T-0305)
  - resize 同步给 Term(T-0306)
  - 光标位置追踪的 API 输出(T-0303)

## Acceptance

- [ ] 4 门全绿
- [ ] 手测:`RUST_LOG=quill::term=trace cargo run --release` 启动后日志里至少一行 "term advanced" 含 cursor (col, row) 字段
- [ ] 审码 放行
