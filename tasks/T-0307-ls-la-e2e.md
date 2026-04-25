# T-0307 端到端测试: ls -la → Term grid 内容

**Phase**: 3 (Phase 3 收尾)
**Assigned**: writer-T0307
**Status**: merged
**Budget**: tokenBudget=40k (lead 派单)
**Dependencies**: T-0201 (PtyHandle::spawn_program) / T-0301 (TermState::advance) / T-0302 (cells_iter / line_text) / T-0306 (term.resize)

## Goal

Phase 3 收尾验证: 端到端跑 `ls -la /`, 验证 PTY → Term → grid 整链路真的能解析 ANSI/VT100 输出, grid 内容里能找到 ls 特征字符 (`drwx`, `total`, 文件名等)。

不接 calloop EventLoop (前面 ticket 已验证), 直接同步 spawn → read loop → advance → assert, 集成测试方式。

## Scope

### In

#### A. `tests/ls_la_e2e.rs` (新文件)
- `ls_la_root_grid_contains_total_and_drwx`
  - `PtyHandle::spawn_program("ls", &["-la", "/"], 80, 24)` (cols=80 rows=24)
  - `TermState::new(80, 24)`
  - `loop { read pty bytes; if Ok(0) || timeout break; term.advance(&buf[..n]); }`
  - 用 std::time::Instant 做 timeout (3-5 秒, ls 应秒级完成)
  - assert: `term.line_text(N).contains("total")` for some N
  - assert: 存在某行包含 `drwx`
  - assert: term.is_dirty() == true (有数据进过)
  - assert: cursor 位置合理 (不在 (0,0))
- `ls_la_smaller_grid_truncates_lines` (可选)
  - cols=40 时, ls 输出长行被 alacritty 截到 40 cols, 验证截断正确

#### B. (可选) 暴露 helper
如果 PtyHandle 没暴露同步 read loop 的方便 helper, 可以在 src/pty/mod.rs 加一个 `#[cfg(any(test, feature = "test-helpers"))]` 的 `pub fn read_until_eof_or_timeout` (但只在 test cfg 编译, 不污染生产 API) — 优先不加, 让测试自己写 loop。

### Out

- **不做**: 真窗口 + 真渲染 (前面 ticket 已验证 5090 跑通); 接 calloop EventLoop (前面 ticket 已验证)
- **不动**: src/term, src/wl, src/main.rs (本单只加 tests/), docs/invariants.md, Cargo.toml
- **不引新 crate** (用 std::time + std::thread::sleep + raw read loop 即可)
- **不写新 ADR**

## Acceptance

- [ ] 4 门全绿 (`cargo build` / `cargo test` / `cargo clippy --all-targets -- -D warnings` / `cargo fmt --check`)
- [ ] 至少 1 个集成测试 `ls_la_root_grid_contains_total_and_drwx` pass
- [ ] 测试在 CI 预算内 (3-5 秒 timeout 之内 ls -la 完成)
- [ ] 87 + N tests pass (T-0306 时 87)
- [ ] 审码放行 (P0/P1/P2 全过)

## 必读 baseline

1. `/home/user/quill/CLAUDE.md`
2. `/home/user/quill/docs/conventions.md` §3 测试组织 + §4 流程
3. `/home/user/quill/docs/audit/2026-04-25-T-0306-review.md` (上一单)
4. `/home/user/quill/tests/pty_to_term.rs` (已有的 PTY → Term 集成测试模板)
5. `/home/user/quill/tests/pty_echo.rs` (已有的 PTY 集成测试 echo 风格)
6. `/home/user/quill/src/pty/mod.rs` (PtyHandle::spawn_program / read API 签名)
7. `/home/user/quill/src/term/mod.rs` (TermState API)

## 已知陷阱

- PTY 是异步 IO (O_NONBLOCK), read 可能返 WouldBlock — loop 内要 sleep ~10ms 等数据
- ls 输出量小 (几 KB), 一次 read 通常拿完, 但 worst case 多次 read
- timeout 取 3-5 秒留余量 (CI 慢机器 OK), 不要太短
- 别忘了 try_wait 检查子进程退出 (Ok(Some(_)) 时停止 read)
- 子进程 reaping: PtyHandle drop 自动 SIGHUP + waitpid, 不手动管
- 测试的 cwd 不重要 (用 / 绝对路径), 但环境变量 LANG=C 可能影响 ls 输出 (中文 locale 会改 "total" 翻译) — 在 spawn_program 前 std::env::set_var("LANG", "C") 或在 ls 命令前加, 防 locale-dependent 测试
