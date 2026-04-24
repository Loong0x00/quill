# T-0105 事件循环骨架

**Phase**: 1
**Assigned**:
**Status**: open
**Budget**: tokenBudget=100k, walltime=3600s, cost=$5
**Dependencies**: 无

## Goal

程序内部只有一个事件循环在跑。Wayland 的事件、未来 PTY 的读写、未来信号通知、未来定时器的 tick,全部都通过这一个循环分发,不另外开线程做 IO。外部看不出来任何差别(跟 T-0101 的窗口表现一样),但内部结构变得干净统一,后续 Phase 2 接 PTY 时直接往这个循环上注册 fd 就行。

本 ticket 的产出更像是一次结构改造:把 T-0101 里的 SCTK 自带 dispatch 替换成 `calloop::EventLoop`,后续 ticket 的 fd 注册都通过这个 loop。

## Scope

- In:
  - 新增 `src/event_loop.rs`(或放到 `main.rs`,视规模决定):创建 `calloop::EventLoop<State>`,把 Wayland connection 的 fd 通过 `calloop-wayland-source` 注册进去
  - 主循环变成 `event_loop.run(None, &mut state, |_state| { ... })`
  - 让 T-0101、T-0102、T-0103、T-0104 的窗口 / 渲染 / 关闭逻辑都以"回调 / 状态改变"形式接入这个循环,不再用独立 dispatch
  - 修改 `Cargo.toml`:加 `calloop`、`calloop-wayland-source`
- Out:
  - PTY fd 注册(Phase 2)
  - timerfd / 定时器业务逻辑(只要接入机制在,不需要真的用起来)
  - D-Bus fd 注册(Phase 5 fcitx5 时)
  - xkb / 键盘事件处理(Phase 2 或 Phase 5 再看)
  - 信号 fd 具体接入(T-0104 有占位,本 ticket 可把 ctrlc 换成 `calloop::signals::Signals`,这个算 in-scope)

## Acceptance

- [ ] `cargo build` 通过
- [ ] `cargo test` 通过
- [ ] `cargo clippy -- -D warnings` 通过
- [ ] `cargo fmt --check` 通过
- [ ] 审码 放行
- [ ] `cargo run` 表现与 T-0101~T-0104 合并后一致:窗口开、变色、缩放、关闭、Ctrl+C 全部正常
- [ ] 代码里全局搜 `blocking_dispatch` / `dispatch_pending` 这类 SCTK 自带调度,只应出现在 `event_loop.rs`(或其同等文件)的少数几个位置,业务模块不直接调
- [ ] 不存在 `std::thread::spawn` 或 `tokio::spawn`(IO 线程禁令)
- [ ] 单元测试:对"事件循环在 should_exit 置位后退出"这条逻辑有测试(构造一个无窗口的 mock state,推入退出信号,验证 `run` 返回)

## Context

- `CLAUDE.md` — "架构 不变式 1":所有 IO fd 全部注册到同一个 `calloop::EventLoop`,绝不起 thread pool 做 IO(本 ticket 就是把这句话落实到代码结构)
- `CLAUDE.md` — "模块切分":暂定 `main.rs` 负责 ppoll 绑所有源
- `docs/adr/0002-stack-lock.md` — 为啥 `calloop` 不 `winit`
- `calloop` docs:https://docs.rs/calloop
- `calloop-wayland-source` docs:https://docs.rs/calloop-wayland-source
- SCTK 官方例子 `themed_window.rs` 用了 calloop 集成,可参考:https://github.com/Smithay/client-toolkit/blob/master/examples/themed_window.rs

## Implementation notes

- `calloop::EventLoop` 的 `State` 泛型参数就是整个程序的状态结构体,里面放 Wayland、渲染、退出标志,回调通过 `&mut State` 改状态
- `calloop-wayland-source` 帮你把 `WlDisplay` 的 fd 包成 calloop source,不自己搞 epoll
- 本 ticket 落地后 T-0104 的 ctrlc 可以换成 `calloop::signals::Signals`,更统一,但这个替换属于本 ticket 的 scope,不算 scope creep
- 事件循环的 timeout 传 `None` 就是无事件时阻塞等,不要 busy-loop
