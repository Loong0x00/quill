# ADR 0003: SIGINT / SIGTERM 捕获用 `signal-hook`,并以 `rustix` 直连 POSIX 层

## Status

Accepted, 2026-04-25

## Context

T-0104 要求关闭路径对三个触发源都"优雅退出":compositor 发 xdg-toplevel
close、终端 Ctrl+C(SIGINT)、外部 `kill`(SIGTERM)。xdg close 已由 SCTK
`WindowHandler::request_close` 回调覆盖,缺的是进程级信号捕获。

Rust 标准库不提供可移植的信号处理 API。三条路:

1. 裸 `libc::sigaction` + 手糊 AtomicBool handler
2. `ctrlc` crate
3. `signal-hook` / `signal-hook-registry`

当前主循环仍用 `event_queue.blocking_dispatch`(wayland-client 0.31),未走
calloop(T-0105 只合了骨架)。因此"用 calloop::signals::Signals"这条路
需要先把 wayland 源接进 calloop,超出 T-0104 范围。

## Decision

引入 **`signal-hook = "0.4"`**(`default-features = false`,不拉 `iterator` /
`channel` / `extended-siginfo` 等扩展)作为直接依赖,用它的 `flag::register`
把 `SIGINT` / `SIGTERM` 挂到 `Arc<AtomicBool>`。

`signal-hook` 的句柄内部只做 `AtomicBool::store(true, Relaxed)` —— 这是
async-signal-safe 的最小副作用,完全符合 "signal handler 里不做 Wayland /
wgpu 调用" 的不变式。

## Alternatives

- **`ctrlc`** — Rejected。其内部 spawn 一个后台线程等待信号,违反本项目
  "所有 IO 单线程" 不变式(CLAUDE.md "架构 不变式" / INV-005)。即便该线程
  不做 IO,额外线程本身引入可见的 state machine 复杂度。
- **裸 `libc::sigaction` + 自写 AtomicBool handler** — Rejected。`sigaction`
  的 `sa_flags` / `sa_mask` 语义、以及 handler 函数必须是 `extern "C" fn`
  且只能调 async-signal-safe 接口,这些易错细节由 `signal-hook-registry`
  做过一次,自己重写纯属造轮子。
- **`calloop::signals::Signals`(signalfd)** — Rejected *本 ticket*。
  technically 更优(信号通过 fd 进入统一 ppoll,无 signal-handler race),
  但依赖 wayland event_queue 已经接入 calloop,那是 T-0105 的后续整合
  ticket 要做的。T-0104 落地后,这条路线可以作为纯粹的 refactor 过渡,
  无需再讨论信号捕获机制。

## Decision(续):`rustix` 作为 poll 原语直接依赖

主循环不再能用 `EventQueue::blocking_dispatch`(该方法 0.31 实现里内部 `poll()`
遇 `EINTR` 会 `continue` 吞掉,见 `wayland-client-0.31.14/src/conn.rs`
`blocking_read`),否则 SIGINT 只会置位 flag,poll 永远等不到下一帧就不醒。
所以本 ticket 把主循环拆成手写 `flush + prepare_read + poll(wayland_fd,
sig_pipe_fd) + read + dispatch_pending`。`poll` 这一步需要 POSIX 原语。

**用 `rustix = "1"`**(`features = ["event"]`):

- Cargo.lock **已经有** `rustix 1.1.4`(calloop / polling / wayland-backend 都依赖),
  加直接依赖不引入新 crate,也不会拉新版本。
- 编译期一等公民 `BorrowedFd` / `PollFd` API,不掉进 `unsafe libc::poll`。
- wayland-rs 自己的 `blocking_read` 就是 `rustix::event::poll`,这里是跟上游保持
  同一把锁的最小代价。

**Alternatives for this sub-decision**:

- **`libc::poll` + 裸 unsafe** — Rejected。ADR 0001 的 `unsafe` 策略要求 SAFETY
  注释逐处论证,而 `poll` 的安全不变式(fds 指针对齐、生命周期、超时参数语义)
  手工维护意义不大 —— 直接用 rustix。
- **`nix::poll::poll`** — Rejected。nix 虽然也在 tree 里(calloop `signals`
  feature 拉),但版本是 0.31,与 rustix 重复,功能上 rustix 已经是更现代的选择。

## Consequences

- Cargo.lock 新增两个 crate:`signal-hook` 与 `signal-hook-registry`
  (后者由前者 re-export;总计 < 500 SLOC)。两者都是 `wayland-rs` /
  `tokio` / `rayon` 生态级使用的老项目,审计负担可控。
- `rustix = "1"` 直接依赖仅在 `Cargo.toml` 里暴露 —— `Cargo.lock` 不新增
  crate(本项目通过 calloop / wayland-backend 已经在用 rustix 1.x)。
- 已知残留竞态:若 SIGINT 恰好在"主循环检查 flag 完毕 → 下一次
  `blocking_dispatch` 进入 poll" 之间的 user code 窗口被投递,handler
  跑完设 flag 后 poll 仍会阻塞到下一个 wayland 事件才醒(纳秒级窗口,
  UX 上等价于"再按一次 Ctrl+C")。彻底消除要等 T-0105 后续把 wayland
  fd 接入 calloop,届时改走 signalfd 即无此窗口。本 ticket 在 wake-up
  路径补一层 EINTR 退让(`blocking_dispatch` 返回 `Interrupted` 时不
  propagate,回到 loop 顶重新检查 flag),覆盖最常见场景(poll 期间
  信号到达)。
- `signal-hook` 的句柄注册是进程级副作用,测试里若要验证必须用独立
  线程 / 子进程,否则污染测试 harness。单测改走 "注入 AtomicBool 模拟
  signal 已被触发" 的路径,避开真 signal。
