# T-0205 子进程退出 → 窗口关闭

**Phase**: 2
**Assigned**: 写码-close
**Status**: claimed
**Budget**: tokenBudget=100k, walltime=3600s, cost=$5
**Dependencies**: T-0201(`PtyHandle` 存在)、T-0202(calloop 已注册 PTY fd)、T-0104(`should_exit` / 优雅退出路径已落地)

## Goal

用户在跑起来的 quill 窗口里退出 shell(输入 `exit` 或按 Ctrl+D),quill 进程会在 1 秒内干净退出:退出码 0,无 wgpu validation 警告,无残留子进程。验证路径:

1. `cargo run`
2. 如果未来 Phase 3 已接键盘输入,直接输 `exit<Enter>`;Phase 2 当前无键盘输入,改走 `pkill -P <quill-pid> bash` 模拟 shell 死掉
3. 1 秒内 quill 进程消失,`echo $?` 为 0,`pgrep -f quill` 无输出

关键:复用 T-0104 建立的"`should_exit` 标志 + 主循环清理"统一路径,**不** 引入第二套退出机制。

## Scope

- In:
  - 实装 `PtyHandle::try_wait(&mut self) -> anyhow::Result<Option<i32>>`:包装 `child.try_wait()`,返回 `None` 表示仍在运行,`Some(code)` 表示已退出(`code` 取 `ExitStatus::exit_code()`,取不到时用 128 + signal 或 -1 占位)
  - 在 T-0202 注册的 PTY calloop 回调里,检测 "读到 0 字节" 或 "`Errno::EIO`":这两种情况都表示 master 端的 slave 被关了,即子进程死了
    - 触发 `state.should_exit = true`(或等价于 T-0104 约定的机制;若 T-0104 使用 `LoopSignal::stop()` 就调之)
    - 调用 `pty_handle.try_wait()` 把子进程收尸,`tracing::info!(target: "quill::pty", ?exit_code, "shell exited")`
  - 如果 T-0104 用的是 `State::should_exit: bool`,本 ticket 在同一字段上 set;如果 T-0104 用的是 `calloop::LoopSignal`,本 ticket 调 `signal.stop()`。**统一走同一条路径**,禁止新建退出变量
  - 退出时 `PtyHandle` 的 drop 顺序:必须在 `renderer` / `window` / `conn` 之间合适位置,遵守 INV-001 扩展(具体位置由 T-0202 已定,本 ticket 不再动)
- Out:
  - Ctrl+C / SIGTERM 的信号路径(T-0104 的事)
  - 键盘输入让用户能自己 `exit`(Phase 3+)
  - 退出时的屏幕淡出效果(绝不做)

## Acceptance

- [ ] **T-0104 已合并到 main**, `should_exit` / `LoopSignal::stop()` 机制可用 (审码 2026-04-25 要求显式登记)
- [ ] `cargo build` 通过
- [ ] `cargo test` 通过
- [ ] `cargo clippy --all-targets -- -D warnings` 通过
- [ ] `cargo fmt --check` 通过
- [ ] 审码 放行
- [ ] 手测:`cargo run` 后在另一终端 `pkill -P $(pgrep -f 'target/.*quill') bash`,1 秒内 quill 进程消失,退出码 0
- [ ] `RUST_LOG=wgpu_core=warn` 下上述手测无 wgpu 警告
- [ ] `pgrep -f quill && pgrep -f bash` 退出后均无残留
- [ ] 单元测试:对 "read 返回 0 字节 → 触发 should_exit" 这条纯逻辑路径有单测,可用 mock(或把该判断提成 `fn pty_readable_action(n_bytes_read: usize, errno: Option<i32>) -> PtyAction` 这样的纯函数,仿 T-0107 抽状态机的做法)

## Context

- `CLAUDE.md` —— 关闭路径不能起新线程;禁 `unwrap`
- `docs/invariants.md` —— INV-001(State drop 顺序,`PtyHandle` 位置由 T-0202 定),INV-005(calloop 唯一调度器,本 ticket 退出走 calloop)
- `tasks/T-0104-close-handling.md` —— 了解退出路径的字段 / signal 约定
- `ROADMAP.md` Phase 2 T-0205
- `portable-pty::Child::try_wait`:https://docs.rs/portable-pty/latest/portable_pty/trait.Child.html#tymethod.try_wait

## Implementation notes

- Unix 下 master 端读到 0(EOF)通常意味着 slave 被 drop;但某些 kernel 会返 `EIO` 而非 EOF——两种都要判。`io::Error::raw_os_error() == Some(libc::EIO)` 可识别
- `Child::try_wait()` 需要轮询;最简单方式:PTY EOF 触发时紧接着调一次 try_wait,若此时子进程可能刚退出未被内核标记,可以 `sleep(1ms)` 再试一次,或接受 `None` 并在稍后的 idle tick 里再 try。**避免阻塞地 `wait()`**
- **严禁** 在 signal handler 里调 `try_wait` / `tracing`:那是 async-signal-unsafe。SIGCHLD 如果要捕获就用 calloop 的 Signals 源(T-0104 已铺路)
- 如果 T-0104 尚未合并,本 ticket **阻塞**:先 merge T-0104,再来抢。写码 若发现 T-0104 未合,通知 Lead 并把 Status 改回 open,不要自己占坑
