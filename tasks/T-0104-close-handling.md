# T-0104 关闭窗口时优雅退出

**Phase**: 1
**Assigned**: 写码-close
**Status**: in-review
**Budget**: tokenBudget=100k, walltime=3600s, cost=$5
**Dependencies**: T-0101

## Goal

运行 `cargo run` 后,无论是点窗口右上角叉、compositor 发关闭信号、还是终端按 Ctrl+C 给进程 SIGINT,程序都能在 1 秒内干净退出:退出码为 0(或 Ctrl+C 的 130),不出现 panic 堆栈,不出现 wgpu 的 validation error,`journalctl` 里不留 compositor 抱怨 "client didn't release surface" 这类警告。程序退出后 `pgrep quill` 无残留进程。

## Scope

- In:
  - 修改 `src/wl/window.rs`:处理 `XdgToplevel` 的 `close` 事件,把一个 `should_exit` 标志置 true
  - 修改 `src/main.rs`:主循环检查 `should_exit`,为 true 则跳出循环,按正确顺序 drop 资源(renderer 先,wayland connection 后)
  - 安装一个轻量 SIGINT / SIGTERM 处理:设置同一个 `should_exit` 标志(可用 `ctrlc` crate 或 `signal-hook`,二选一,由 写码 决定)
  - 在 `main.rs` 末尾加 `tracing::info!("quill exited cleanly")`
- Out:
  - 进程因 panic 退出时的资源清理(Phase 6 再看)
  - 子进程 PTY 的回收(Phase 2)
  - 渲染中途被打断的帧取消(只要不崩就 OK)
  - 保存任何状态到磁盘

## Acceptance

- [ ] `cargo build` 通过
- [ ] `cargo test` 通过
- [ ] `cargo clippy -- -D warnings` 通过
- [ ] `cargo fmt --check` 通过
- [ ] 审码 放行
- [ ] 点窗口右上角叉后 1 秒内进程退出,退出码 0
- [ ] 终端里 Ctrl+C 触发同样的干净退出路径
- [ ] `RUST_LOG=wgpu_core=warn cargo run` 后关闭不产生任何 wgpu 警告
- [ ] `pgrep -f quill` 在退出后无输出
- [ ] 单元测试:对"should_exit 标志被置位后主循环应立刻退出"这一逻辑有单元测试(把循环体函数化,注入标志)

## Context

- `CLAUDE.md` — "开发准则":禁止 `unwrap` / `expect`,错误路径必须 `?`
- `CLAUDE.md` — "架构 不变式":单线程事件循环,关闭路径不能起新线程做清理
- `ROADMAP.md` Phase 1 `T-0104`
- SCTK `XdgToplevel::close` 事件:https://docs.rs/smithay-client-toolkit/latest/smithay_client_toolkit/shell/xdg/window/trait.WindowHandler.html
- `ctrlc` docs:https://docs.rs/ctrlc
- `signal-hook` docs:https://docs.rs/signal-hook

## Implementation notes

- Drop 顺序:渲染相关的 `Surface` / `Device` / `Queue` 必须在 Wayland `Connection` 之前 drop,否则 wgpu 持有的 `WlSurface` 会在 connection 关闭后仍然 RAII,产生警告
- 如果 `should_exit` 是 `Arc<AtomicBool>`,signal handler 里只做 `store(true, Ordering::Relaxed)`,不要在 handler 里做任何 Wayland / wgpu 调用
- 主循环可以阻塞等事件,信号来了要能唤醒 —— 用 `signal-hook-registry` 写一个 fd 注入到 calloop(若 T-0105 已合)或在 T-0105 合入前先用 `ctrlc` 占位,T-0105 写码 会整合
