# T-0108 事件循环统一:wayland / signal / pty 全进 calloop

**Phase**: 1 (refactor)
**Assigned**: 写码-close
**Status**: claimed
**Budget**: tokenBudget=100k(与 T-0301 共享)
**Dependencies**: T-0104 / T-0105 / T-0206 已 merged

## Goal

清 TD-001 / TD-005 / TD-006 三条技术债。把 T-0104 的手写 `rustix::event::poll` 主循环 + signal-hook self-pipe 拆掉,全部移交给 `calloop::EventLoop<State>`:

- wayland fd → `calloop::generic::Generic`,callback 调 `event_queue.prepare_read + read + dispatch_pending + flush`
- SIGINT / SIGTERM → `calloop::signals::Signals`(signalfd,消除 TD-006 的 nanos 竞态窗口)
- PTY master fd → 仍走 `calloop::generic::Generic`,从 `Core<PtyHandle>` 升为 `Core<State>` 的 callback 里 `&mut state.pty`

INV-005 "所有 IO fd 同一 EventLoop" 真落地。

## Scope

- In:
  - 删 `run_main_loop` / `StepResult` / `pump_once` / `install_signal_handlers` / `drain_pipe` + 对应单测
  - 删 `signal-hook` 依赖(Cargo.toml / Cargo.lock)
  - `State` 新增 `should_exit: Arc<AtomicBool>` 或 LoopSignal 持有,统一退出位
  - 引入 `LoopData` 包装 `{ event_queue, state, ... }`
  - `run_window` 重写为 `EventLoop::try_new → insert_source × 3 → event_loop.run`
- Out:
  - alacritty_terminal 集成(T-0301)
  - ADR 0003 的文本调整(留给后续 cleanup)

## Acceptance

- [ ] 4 门全绿
- [ ] 手测:SIGINT / SIGTERM / pkill bash / xdg close 四条退出路径均 <1s 退码 0,无 wgpu 告警
- [ ] 审码 放行
- [ ] TD-001 / TD-005 / TD-006 可归档

## Context

- TD-001 / TD-005 / TD-006:`docs/tech-debt.md`
- `src/event_loop.rs::Core<'l, State>` 本体零改动,只换 State 类型实例化
- Lead 2026-04-25 决定:T-0108 + T-0301 同分支两 commit 一起做
