# T-0203 PTY 字节流 tracing::trace! 打印

**Phase**: 2
**Assigned**: 写码-close
**Status**: in-review
**Budget**: tokenBudget=100k, walltime=3600s, cost=$5
**Dependencies**: T-0202(calloop 已能监听 readable)

## Goal

`RUST_LOG=quill::pty=trace cargo run` 启动后,日志里看到 bash 启动时输出的 prompt 字节流,以 `tracing::trace!` 形式逐批打印(一次回调读到的字节合成一行 trace,用 escape 形式显示不可打印字符)。这是 Phase 2 "PTY 输出能进 ppoll" 的闭环验收:Wayland 还是纯深蓝窗口,但日志证明 `shell 输出 → master fd → calloop → 字节` 这条通路通了。

## Scope

- In:
  - 实装 `PtyHandle::read(&mut self, buf: &mut [u8]) -> std::io::Result<usize>`:包装 `reader.read(buf)`
  - 修改 T-0202 注册的 calloop 回调:从占位 `"pty readable"` 改为真的读字节;一次回调循环读到 `WouldBlock` 或单次 buffer 满为止(buf 大小 4 KiB 够用,定 `const PTY_READ_BUF: usize = 4096;`)
  - 每次 read 成功后 `tracing::trace!(target: "quill::pty", n = n, bytes = ?buf[..n].escape_ascii().to_string(), "pty bytes")`(或等价的 hex dump)
  - 错误处理:
    - `WouldBlock` → `Ok(PostAction::Continue)`,正常
    - `io::ErrorKind::Interrupted` → continue 循环再读
    - 其它错误(EIO 等子进程挂了)→ `tracing::warn!`,并把 state 置 `exit`(或等 T-0205 补,本 ticket 可先只 warn 不退出,在 Implementation 里标 TODO link T-0205)
  - master fd 的 **O_NONBLOCK 已由 T-0201 置位并登记为 INV-009**, 本 ticket 仅需:
    - 在读循环入口 `debug_assert!(fcntl(F_GETFL) & O_NONBLOCK != 0, "T-0201 未设置 O_NONBLOCK, INV-009 破坏")`, debug build 破则 panic, release 无开销
    - **不重复** `F_SETFL` (误设会覆盖其它 flag, 破坏 T-0201 的不变式)
- Out:
  - 把字节喂 `alacritty_terminal::Term`(Phase 3 T-0302)
  - 关窗口(T-0205)
  - 屏幕渲染

## Acceptance

- [ ] `cargo build` 通过
- [ ] `cargo test` 通过
- [ ] `cargo clippy --all-targets -- -D warnings` 通过
- [ ] `cargo fmt --check` 通过
- [ ] 审码 放行
- [ ] 手测:`RUST_LOG=quill::pty=trace cargo run` 启动后 2 秒内日志至少出现 1 行包含字节流的 "pty bytes" trace(典型内容:`bash-5.x$` 或 `\x1b[...`)
- [ ] 回调里读到 `WouldBlock` 能正确跳出,`cargo run` 后 CPU 不 100%(busy loop 自查)
- [ ] 读循环入口 `debug_assert!` 检查 O_NONBLOCK 已置位 (T-0201 设置, 本 ticket 只 sanity check; INV-009)
- [ ] 单元测试(可在 `src/pty/mod.rs` 内或新 `tests/pty_read_nonblocking.rs`):spawn `echo hi`,循环 `read` 到 EOF,聚合 buffer 断言 `b"hi\n"` 或 `b"hi\r\n"`(PTY 会把 \n 转 \r\n)

## Context

- `CLAUDE.md` —— 禁 `println!`,用 `tracing`;禁 `unwrap`;架构不变式 3:PTY 读取必须非阻塞,否则 event loop starve
- `docs/invariants.md` —— INV-005(唯一调度器), INV-009 (PTY master O_NONBLOCK, T-0201 已加; 本 ticket 只引用)
- `ROADMAP.md` Phase 2 T-0203
- 上游 bug 参考:Ghostty 的 event starvation(CLAUDE.md "为什么存在"章节)—— 本 ticket 是反面避坑

## Implementation notes

- `nix` crate **未** 被锁定。若为了 `fcntl` 引入 nix,必须写 ADR(ADR 0002 约束)。替代:直接 `libc::fcntl` 通过 `#[allow(unsafe_code)]` + `// SAFETY:` 块(类似 `src/wl/render.rs` 的处理)。**推荐后者,不引 nix**
- `escape_ascii()` 是 `u8` 的稳定方法(Rust 1.60+),返回的 iterator 可 collect 到 String 用于 trace 显示
- `tracing::trace!` 的 `?bytes` 会走 Debug,字段可能太长;用 `bytes = %format!(...)` 或自定义。或者用 `len=n` 字段 + 前 64 字节 preview
- 4 KiB buffer 够用:PTY master 的内核 buffer 典型 4K,一次 read 吞完即可;若单次读 4K 仍 `Ok(n=4096)`,循环再来一次
- 若 PTY 永远不 readable(shell hang),别让回调空转:calloop Level 模式只在 fd "真有数据" 时触发,无需自己 sleep
