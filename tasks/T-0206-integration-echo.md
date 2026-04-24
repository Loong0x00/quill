# T-0206 集成测试:spawn echo hello 捕获 stdout

**Phase**: 2
**Assigned**:
**Status**: open
**Budget**: tokenBudget=60k, walltime=1800s, cost=$3
**Dependencies**: T-0201(spawn)、T-0203(`read` 非阻塞 + O_NONBLOCK)

## Goal

`tests/pty_echo.rs` 集成测试通过 `PtyHandle::spawn_program("echo", &["hello"], 80, 24)` 真起一个子进程,循环 `PtyHandle::read` 直到拿到 EOF(返回 0 字节 或 `EIO`),把累计 buffer 断言包含 `b"hello"`(行尾可能是 `\n` 或 `\r\n`,PTY 默认 onlcr 会把 `\n` 转成 `\r\n`,两者都接受)。

这个测试是 Phase 2 整个通路的 **端到端冒烟**:不依赖 Wayland,不依赖 calloop,只测 `PtyHandle` 本身把字节从子进程送出来。`cargo test` 在 headless CI 上应该能跑。

## Scope

- In:
  - 新建 `tests/pty_echo.rs`:
    - `PtyHandle::spawn_program("echo", &["hello"], 80, 24)`
    - 循环 `read` 累计,遇到 `WouldBlock` 短暂 `sleep(10ms)` 再试,总超时 2 秒(超时 panic 失败)
    - 直到读到 0 字节 或 `EIO` 后 `try_wait()`(T-0205 若尚未实装,此处用 `portable-pty::Child::wait` 或 drop 即可;但推荐 T-0206 等 T-0205 合完再写)
    - `assert!(buffer.windows(5).any(|w| w == b"hello"))`
  - 若 `echo` 子进程退出后 master 仍未 EOF(内核调度问题),测试 `sleep(50ms)` 再读一次
  - 同一文件内加第二个测试:spawn 一个永不退出的进程(例:`sleep 60`)然后立即 drop `PtyHandle`,断言 drop 返回后 100ms 内 `pgrep -f "^sleep 60$"` 不再命中(证明 drop 清理了子进程)
- Out:
  - Wayland / calloop / wgpu 任一组件的集成(那是 T-0202 的范围)
  - 跨平台兼容(Linux only)

## Acceptance

- [ ] `cargo build` 通过
- [ ] `cargo test --test pty_echo` 通过(本地 Arch Linux)
- [ ] `cargo test` 全量通过(不引入其他回归)
- [ ] `cargo clippy --all-targets -- -D warnings` 通过
- [ ] `cargo fmt --check` 通过
- [ ] 审码 放行
- [ ] 第二个 drop-cleanup 测试通过,证实 `PtyHandle::drop` 会让子进程收到 SIGHUP 退出(或内核自动回收)

## Context

- `CLAUDE.md` —— "先写测试再写实现"、集成测试放 `tests/`
- `ROADMAP.md` Phase 2 T-0206
- `docs/invariants.md` —— INV-005(本测试不触发,但风格:任何读 fd 都走 non-blocking)
- 相邻 ticket:T-0203(`read` 的 O_NONBLOCK 保证)、T-0205(`try_wait` 存在性)

## Implementation notes

- `echo hello` 输出仅 `hello\r\n` 6 字节,一次 read 就完;单测 buffer `[u8; 256]` 绰绰有余
- Arch Linux 默认有 `echo` (coreutils),无需加 dep
- `PtyHandle::drop` 的清理若 **未** 自动杀子进程(portable-pty 的 drop semantic 因 PTY 实现而异),可能需要在 T-0201 显式 `child.kill()` 到 Drop 里 —— 但 **这是 T-0201 的事,不是 T-0206**。如果 drop-cleanup 测试失败,写码 应开 issue 扔回 T-0201 而非在此修
- 测试用 `std::thread::sleep(Duration::from_millis(10))` 不违 CLAUDE.md 的 "IO 线程禁令" —— 禁令针对业务代码,测试内线程允许
- 跑完测试 **不能** 留残留 bash / sleep / echo 子进程:`cargo test` 本身有 watchdog,但还是要确认
