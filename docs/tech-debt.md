# quill 技术债 (已识别未修)

**用途**: 记录项目演进中已识别但当前阶段不修的风险点, 供未来 agent / Lead 接手时一眼看见 "这些是要改的, 不是遗漏"。

**为啥独立文件** (与 `docs/invariants.md` 对比):
- `invariants.md` 是**必须维持**的硬约束 (违反则 UB / 数据损坏 / 死锁 / 泄露)
- `tech-debt.md` 是**已知欠债**的集合 (当前不修, 但标记了触发修理的条件)
- git log / commit message 会随时间湮没, 文件是稳定索引

**登记原则**: 任何"已拍板选了 A 但未来会推翻"的决策、"审码报告里 P3 建议不阻塞所以归档"的条目、"ticket 字面与架构冲突选了架构偏离"的伏笔, 都在这登记, 不让这些沉到 SendMessage / commit message 里。

**条目格式**:
- `TD-<id>`: 标题
- **识别日期 + 识别者**
- **代码/ticket 位置**: 文件 + 行号或 ticket id
- **当前状态**: 为啥暂不修
- **触发修理的条件**: 什么 ticket 或 phase 节点到了必改
- **解决路径**: 怎么改 (只要有思路就写一行)

---

## TD-001: T-0203 走 A 方案 Core<PtyHandle>, T-0105 refactor 会推翻

**识别日期**: 2026-04-25
**识别者**: Lead + 审码-opus (T-0202 audit 报告 "给 T-0203 的前置要求")

**代码位置**:
- `src/event_loop.rs::Core<'l, State>` — T-0105 已泛型化, T-0202 用 `Core<()>`, T-0203 改为 `Core<PtyHandle>` (event_loop.rs 本体零改动, 只换 instantiate)
- `src/wl/window.rs::pump_once` — calloop 回调从 `fn(event, metadata, &mut ())` 换成 `fn(event, metadata, &mut PtyHandle)`, 回调里走 `pty.read(&mut buf)` 取代 T-0202 stopgap 的 `rustix::io::read(master_fd)`

**当前状态**: 不修
- B 方案 (T-0203 合并 T-0105 refactor, Core Data 直接升 `State`) 超出单 ticket 预算, 违反 "一次 commit 做一件事"
- A 方案代价: T-0105 refactor 时把 Data type 从 `PtyHandle` 再升到 `State` (或 `&mut State`), 回调签名改一次
- 注 (2026-04-25 Lead 实际 派单措辞不准确): 原派单写 `Core<&mut PtyHandle>`, 但 T-0105 的 `Core<'l, State>` 泛型 + calloop 原生 `dispatch(timeout, &mut Data)` 签名下, 直接 instantiate `State = PtyHandle` 更干净, 借用期限于单次 dispatch 调用。写码-close 选对了, Lead 确认批准

**触发修理的条件**:
- T-0105 refactor 动作 (把 wayland event_queue + signal pipe 也注册进 calloop, 消除双 poll)
- 计划节点: Phase 2 结束或 Phase 3 前评估

**解决路径**:
- T-0105 refactor 单开 ticket (编号待命名)
- 把 Core instantiate 从 `PtyHandle` 改为 `State` (或设计一个 EventLoopCtx 聚合 wayland/signal/pty), 回调从 `&mut state.pty` 拿 PtyHandle
- 删掉 `src/wl/window.rs` 里的 `rustix::event::poll` 手写循环 (T-0104 建, 当前与 calloop 并存)
- signal 处理从 signal-hook self-pipe 改 `calloop::signals::Signals` (signalfd 路径, 消除 T-0104 ADR 0003 承认的纳秒级竞态窗口)

---

## TD-002: T-0202 pump_once BorrowedFd SAFETY 注释详略差异

**识别日期**: 2026-04-25
**识别者**: 审码-opus (T-0202 audit 报告 P3-2)

**代码位置**: `src/wl/window.rs:272-275`

**当前状态**: 不修
- 注释 2 行, 覆盖 fd 来源 + 活性 (关键点到位)
- 比 T-0201 `set_nonblocking` 的 4 点风格简, 但非 UB 风险
- P3 偏好建议, 审码放行, Lead 决议不改

**触发修理的条件**:
- T-0105 refactor 时顺手展开, 因为那时回调路径会重写
- 或任何 audit 发现 SAFETY 注释被新 agent 误读的事件

**解决路径**: 展开为 T-0201 set_nonblocking 风格 4 点: (1) fd 来源 (2) fd 活性 (3) 借用生命周期 (4) syscall 副作用

---

## TD-003: T-0202 run_window BorrowedFd<'static> 未明写 Rust 反向 drop 保证

**识别日期**: 2026-04-25
**识别者**: 审码-opus (T-0202 audit 报告 P3-3)

**代码位置**: `src/wl/window.rs:408-412`

**当前状态**: 不修
- 注释说 "event_loop 的生命周期", 但真正的保证是 Rust 反向 drop (`pty_core` 声明在 `state.pty` 之后, 反向先 drop)
- 非 UB, 信息不足但不误导

**触发修理的条件**: T-0105 refactor 时顺手补

**解决路径**: SAFETY 明写 "pty_core 在 state 之后声明, Rust 反向 drop 保证 pty_core EventLoop/Generic source drop 先于 state.pty PtyHandle drop, 因此 BorrowedFd 伪造的 'static 实际被 drop-order 约束在 state.pty 寿命内"

---

## TD-004: T-0202 测试闭包内 "SAFETY 提醒" 关键字稀释

**识别日期**: 2026-04-25
**识别者**: 审码-opus (T-0202 audit 报告 P3-5)

**代码位置**: `tests/pty_calloop_smoke.rs:38-44` (现已随 P3-1 修正部分调整, 但 "SAFETY 提醒" 段仍在)

**当前状态**: 不修
- 该回调本身非 unsafe 块, "SAFETY 提醒" 字样稀释 unsafe SAFETY 关键字的语义
- 不影响正确性

**触发修理的条件**: 任何新 audit 发现有人把该段误读成 unsafe 前置条件

**解决路径**: 改为 "// 不 drain 的原因:..." 普通注释, 去掉 "SAFETY" 字样

---

## TD-005: T-0104 + T-0202 双 poll 共存 — wayland 高频事件下 PTY 数据堆积

**识别日期**: 2026-04-25
**识别者**: 审码-opus (T-0202 audit 报告 P2-2 退化风险段)

**代码位置**: `src/wl/window.rs::pump_once` 快路径 (`dispatched > 0` 时立刻回 top 不走 poll)

**当前状态**: 不修
- Phase 2 wayland 事件是偶发 (resize / focus 切换), 正常负载不触发退化
- 极端场景: 高频 wayland 事件下 pump_once 快路径常回 top → PTY master fd 数据堆积内核 buffer → 用户看到的 "pty bytes" trace 会延迟
- 不影响正确性 (内核 buffer 4K+, 足够缓冲)

**触发修理的条件**:
- T-0105 refactor (把 wayland/signal 全进 calloop 后, 所有 fd 一视同仁由 calloop dispatcher 公平轮询, 退化消失)
- 或 Phase 6 soak 测试发现 PTY 响应延迟 (目前不预期触发)

**解决路径**: T-0105 refactor 后自然消除

---

## TD-006: ADR 0003 纳秒级竞态窗口 — signal 投递到响应的 poll 阻塞 gap

**识别日期**: 2026-04-24 (T-0104 写码-close 自揭)
**识别者**: 写码-close (ADR 0003 Consequences 段)

**代码位置**: `src/wl/window.rs::pump_once` 主循环 — "检查 flag 完毕 → 下一次 poll 进入" 之间的 user code 窗口

**当前状态**: 不修
- signal 恰好在 "flag-check 后 / poll 进入前" 的纳秒级窗口投递 → handler 跑完置 flag + 写 self-pipe → poll 仍会被 self-pipe fd 可读唤醒
- ADR 0003 在 wake-up 路径补了 EINTR 退让 (`blocking_dispatch` 返回 Interrupted 时回 loop 顶重检 flag), 覆盖最常见场景 (poll 期间信号到达)
- UX 等效 "再按一次 Ctrl+C 就能退", 不卡死

**触发修理的条件**:
- T-0105 refactor 把 signal 处理从 signal-hook self-pipe 改 `calloop::signals::Signals` (signalfd 路径, 信号通过 fd 进入统一 ppoll, 无 signal-handler race)
- 当用户抱怨"Ctrl+C 偶发不响应"时 (目前未预期)

**解决路径**: T-0105 refactor 切 signalfd, 消除 signal handler + fd readable 两步的原子性 gap

---

## TD-007: T-0201 `spawn_shell_returns_ok_and_drops_cleanly` 单测依赖 init 收养孤儿 bash

**识别日期**: 2026-04-25 (T-0201 实装时)
**识别者**: 写码-close (src/pty/mod.rs 测试注释)

**代码位置**: `src/pty/mod.rs` `#[test] fn spawn_shell_returns_ok_and_drops_cleanly`

**当前状态**: 不修
- 测试 spawn `bash -l` 后只 `drop(handle)` 不 `wait()`, 因 bash 交互式登录 shell 收 SIGHUP 前可能不退出, wait 会卡死单测
- Drop 触发 master close → slave EOF + SIGHUP → bash 几百 ms 内退出, 残留僵尸由 test harness 退出后 init 收养
- 风险场景: `cargo test` 并行 runner 进程自身被 kill -9, 孤儿 bash 的父进程不是 init 而是 shell ancestor → 僵尸积累

**触发修理的条件**:
- 发现 CI 环境累积 bash 僵尸进程 (Phase 6 soak 前)
- 或切换 CI runner 到 不 re-parent 到 init 的环境 (nspawn / docker 可能没 PID 1 init)

**解决路径**:
- 改用 `CommandBuilder::new("bash").arg("-c").arg("exit 0")` 非交互立退, `wait()` 不卡
- 或 spawn + 发 kill -HUP 给 child 再 wait

---

## TD-008: INV-009 O_NONBLOCK 断言仅 debug build, release 无验证

**识别日期**: 2026-04-25 (T-0203 派单时)
**识别者**: Lead (T-0203 派单明确 debug_assert)

**代码位置**: `src/pty/mod.rs::PtyHandle::read` (T-0203 实装后) — 读循环入口 `debug_assert!(fcntl(F_GETFL) & O_NONBLOCK != 0)`

**当前状态**: 不修
- debug_assert 只在 debug build 跑, release build 被优化掉
- 理由: F_GETFL 有 syscall 开销, release 不想付, 且 INV-009 由 T-0201 单一入口保证不会被意外清除
- 若未来有代码路径 (手动 fcntl / 库升级改内部 flag 管理) 误清 O_NONBLOCK, release 不会 fail, T-0203 的 read 会阻塞整个事件循环 → Ghostty starve 坑重现

**触发修理的条件**:
- 添加任何新 fcntl 代码路径 (必须走 code review 对齐 INV-009)
- portable-pty 升级到更新版本 (核对是否改了 MasterPty 内部 flag 管理)
- Phase 6 soak 发现 event loop 有 >1s 阻塞 (对应 starve)

**解决路径**:
- 方案 A: 在 CI 跑一轮 release build + 手动测 O_NONBLOCK 生效 (低成本)
- 方案 B: 改成 `assert!` 硬检查 (每 read 调用付 F_GETFL syscall, 成本高)
- 方案 C: 集成测试 tests/pty_nonblock.rs 注入一个"故意清 O_NONBLOCK 的 mock path", 验证后果 (测试基础设施投入)

---

## TD-009: "点窗口右上角叉"退出路径无 E2E 测试

**识别日期**: 2026-04-25 (T-0104 audit)
**识别者**: 审码 (T-0104 audit 报告 "未测项说明")

**代码位置**: `src/wl/window.rs::WindowHandler::request_close` + `handle_event(WindowEvent::CloseRequest)`

**当前状态**: 不修
- GNOME mutter 不暴露通用关窗 IPC (wlroots foreign-toplevel 协议 mutter 没实现)
- SIGINT / SIGTERM / disconnect 三条退出路径与 xdg close 共用 `should_exit` + `run_main_loop`, 已被 `tests/state_machine.rs::close_sets_exit_flag` 单测覆盖
- 手测在 GNOME 下无法自动化

**触发修理的条件**:
- 切到 sway / Hyprland (wlroots-based) 开发环境时可做 E2E
- 或引入虚拟 compositor 测试 harness (Wayland 测试基础设施投入)

**解决路径**:
- 方案 A: 跑 CI 在 sway 环境下 fire foreign-toplevel close 信号
- 方案 B: 在 `wayland-backend` test-rig 里 mock XdgToplevel 发 `close` 事件, 不起真 compositor
- 短期: 保持单测覆盖 + 手测清单

---

## TD-010: T-0201 bash -l 的 cwd 默认为进程 cwd (未做配置)

**识别日期**: 2026-04-25 (T-0201 实装)
**识别者**: 写码-close (src/pty/mod.rs CommandBuilder 注释)

**代码位置**: `src/pty/mod.rs::PtyHandle::spawn_program` 构造 `CommandBuilder` 时不设 cwd

**当前状态**: 不修
- `CommandBuilder::new("bash")` 默认 cwd = 当前进程 cwd
- quill 从 Desktop 启动时 cwd = `$HOME` (GNOME 默认), 从 terminal 启动时 cwd = 调用 shell 的 cwd
- UX 不一致, 但 Phase 2 不修 (没配置系统)

**触发修理的条件**:
- Phase 6 T-0606 配置文件格式 (TOML 支持 font-size / font-family / color + **可以一起加 shell-cwd**)
- 或用户反馈半夜 Desktop 启动 quill 出来 cwd 奇怪

**解决路径**: Phase 6 配置系统引入 `[shell]` section, 支持 `cwd = "$HOME"` / `cwd = "$PWD"` / `cwd = "/abs/path"`, 默认 `$HOME` 符合大多数终端习惯

