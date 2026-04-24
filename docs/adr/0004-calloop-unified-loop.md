# ADR 0004: wayland / signal / pty 三源统一进 `calloop::EventLoop`

## Status

Accepted, 2026-04-25

## Supersedes

- ADR 0003 (signal-hook + rustix 手写 poll) — 本 ADR 完全取代其主循环机制

## Context

Phase 1-2 期间 quill 的主循环经历三层演进:
- T-0102/T-0104 时代: `event_queue.blocking_dispatch` 的 wayland-client 0.31 版本**吞 EINTR**, signal 处理必须绕开
- T-0104 实装 (ADR 0003): 自己手写 `flush + prepare_read + poll(wayland_fd, signal_pipe_fd) + read + dispatch_pending`, signal 用 signal-hook self-pipe 转为 fd 可读事件
- T-0105 合 `Core<'l, State>` calloop 骨架, 但 wayland 仍走手写 poll, **双 poll 并存**
- T-0202 接 PTY fd 进 calloop Generic source, 但 wayland/signal 继续手写 poll, **三源两套调度**

双 poll 带来的技术债:
- **TD-005** — 高频 wayland 事件时 pump_once 快路径常回 top 导致 PTY 数据堆积缓冲
- **TD-006** — signal-hook handler 置 Arc + 写 pipe 存在纳秒级用户空间竞态窗口 (handler 跑完但 poll 仍在"等 wayland socket", 要等下一个 wayland 事件才醒)
- **TD-001** — Core Data 泛型 = PtyHandle 是过渡, 若想在 callback 里访问 wayland state 或 Term 必须再升级

Phase 3 T-0301 alacritty_terminal 集成需要在 pty_read_callback 里拿到 Term 实例, 这触发一次性重构的时机: **把 wayland event_queue + signal pipe + pty fd 全部注册进同一个 `calloop::EventLoop`**, Data 升级到包含 Term 的聚合结构。

## Decision

采用 **`calloop::EventLoop<LoopData>` 统一调度三源** 的架构。

### 数据结构

```rust
struct LoopData {
    event_queue: EventQueue<State>,
    state: State,
    term: Option<TermState>,
    loop_signal: LoopSignal,
}
```

`LoopData` 是主循环数据层, 聚合所有 runtime 对象。pty callback 走 Rust 2021 字段级 split borrow:

```rust
let LoopData { state, term, loop_signal, .. } = &mut *data;
```

同时持 `&mut state`, `&mut term`, `&loop_signal`, 编译器 NLL 稳定支持。

### 三源注册

1. **wayland fd** (Generic source, READ, Level) — callback 走 `drive_wayland`: `prepare_read → guard.read → dispatch_pending → flush`
2. **signalfd** via `calloop::signals::Signals::new(&[SIGINT, SIGTERM])` — callback 直接 `loop_signal.stop()`
3. **pty master fd** (Generic source, READ, Level) — callback 从 split borrow 拿 &mut pty + &mut term + &loop_signal, 走 `pty_read_tick`

### 退出机制

**`LoopSignal::stop()`** 是唯一退出出口, 替代 T-0104/T-0205 的 `Arc<AtomicBool> should_exit`。

- xdg close → `state.core.exit = true` → `drive_wayland` 检测 → `loop_signal.stop()`
- SIGINT/SIGTERM → signalfd callback → `loop_signal.stop()`
- PTY EOF/EIO (shell 退) → `pty_read_tick` RequestExit → `loop_signal.stop()` + `pty.try_wait()` 收尸
- 三条路径走同一个 stop(), `event_loop.run()` 当前 dispatch 结束即返回

## Alternatives

### Alt 1: 保留 ADR 0003 手写 poll, 只升 Data 泛型
- 方案: Core<PtyHandle> → Core<State> 升级, wayland 继续手写 poll, signal 继续 self-pipe
- Reject: 解决不了 TD-005 (双 poll) 和 TD-006 (纳秒竞态), 只解决 TD-001
- 技术债累积, 下次 Phase 3 ticket 又要面对同样问题

### Alt 2: 用 `tokio` 或 `async-io` 异步运行时
- 方案: async fn + executor, wayland/signal/pty 全异步
- Reject:
  - 违反 CLAUDE.md 技术栈锁 (calloop 是锁定的事件循环选择)
  - tokio 引入大量依赖 (tokio-macros / futures / pin-project / socket2 等 10+ crate), 违反"生态最小化"原则
  - async + wayland-client 0.31 的现有 Dispatch trait 需要 runtime adapter, 复杂度陡增
  - 单线程 calloop 已满足所有需求

### Alt 3: 手写 epoll 循环不用 calloop
- 方案: 直接用 rustix 或 libc epoll_create/epoll_ctl/epoll_wait
- Reject:
  - calloop 已封装 epoll + 生命周期 + PostAction 语义, 重造轮子无意义
  - 失去 sctk 生态的 WaylandSource helper (虽然 T-0108 最终没用, 但其它 Phase 可能用)
  - T-0105 已建 Core 骨架, calloop 已是锁定选择

### Alt 4: T-0301 分两步做 (先 T-0108 refactor 后 T-0301)
- 方案: 拆两个独立 ticket
- Reject 程度: 轻度 — 可行但多一次 context switch 成本
- 选择合并做的理由: T-0301 的 callback 升级本来就要动 Data type, 合并做一次性消除所有累积技术债 (TD-001/005/006), 避免 T-0301 做一半又被 T-0108 refactor 推翻

## Consequences

### 正面
- **TD-001 / TD-005 / TD-006 三条技术债一次性归档** (至 docs/tech-debt.md)
- **signal-hook 直接依赖删除** (Cargo.toml 少一个 crate, 且 ADR 0003 的 pipe 机制整套谢幕)
- **rustix 直接依赖删除** (成 transitive only)
- **TD-006 signal 纳秒竞态彻底消除** (signalfd 走 kernel 级同步)
- **src/wl/window.rs 净减 241 行** (删 run_main_loop / StepResult / install_signal_handlers / pump_once / drain_pipe 共约 440 行, 加 LoopData + drive_wayland + 三源注册约 200 行)
- **架构 idiomatic** — 符合 calloop 库作者意图 (LoopSignal + 聚合 Data + 字段 split borrow)

### 负面 / 代价
- **Cargo.lock 新增 alacritty_terminal 0.26 transitive** (vte 0.15 / rustix-openpty / signal-hook / miow / parking_lot / piper / polling / base64 / home 等 10+ crate), 审计负担小幅增加
- **`calloop::signals` 平台限制** — 依赖 Linux signalfd, 不支持 macOS/Windows。本项目 CLAUDE.md 声明仅 Linux Wayland, 不违反约束
- **T-0305 色块渲染触发位置需预先决策** — Term 放 LoopData 意味着 Dispatch 回调拿不到 Term, 需要 render 在 LoopData 级 idle callback 触发, 或者 T-0305 ticket 决定把 Term 挪 State (两条路都通, 归档为 Open Question 记 T-0305 里)

### 已知残留 (非本 ADR scope)
- LoopData.term 类型 `Option<TermState>` 但总是 Some, Option 守卫防御冗余。未来 T-0306 resize 若真 take/put 才有必要, 现状可接受
- wayland_fd BorrowedFd<'static> 的 SAFETY 注释可补 Rust 反向 drop 明说, 对齐 pty_fd 风格 (TD-014 登记)

## 实装验证

- T-0108 commit `0ffabea` + T-0301 commit `22718f0` + followup `d2bb6db` 实装本 ADR
- 58 tests pass (33 unit + 2 event_loop + 3 frame_stats + 2 pty_calloop + 2 pty_echo + 3 pty_resize + 2 pty_to_term + 11 state_machine)
- 四退出路径 (xdg close / SIGINT / SIGTERM / pkill bash) 手测 <1s 退码 0
- `col=17 line=0` 匹配 bash prompt 长度 17 字符, 证 Processor 正确解析 OSC/DECSET 转义
- 审码-opus audit 报告 `docs/audit/2026-04-25-T-0108-T-0301-review.md` 独立复判通过

## 相关文档

- 被取代的 ADR: `docs/adr/0003-signal-hook.md`
- 归档的技术债: `docs/tech-debt.md` TD-001 / TD-005 / TD-006
- 实装 audit 报告: `docs/audit/2026-04-25-T-0108-T-0301-review.md`
- 依赖不变式: `docs/invariants.md` INV-005 (本 ADR 强化了此条: calloop 确实成为唯一 IO 调度器, 无双 poll)
