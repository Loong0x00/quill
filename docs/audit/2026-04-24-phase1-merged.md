# 审码报告: T-0101 + T-0105 合并后 main 分支

**审码人**: audit (quill-phase1 team, Haiku)
**日期**: 2026-04-24
**范围**: src/wl/window.rs + src/event_loop.rs + src/lib.rs + src/main.rs + tests/event_loop_smoke.rs

## P0: 正确性

### [P0-1] 潜在 Wayland 对象资源泄漏(长跑 session 风险)

**位置**: src/wl/window.rs:45-96(run_window 整体)
**严重性**: 🔴 高 — 违背项目核心目标"1h soak RSS 稳定"

**问题描述**:

1. **Buffer 生命周期黑盒**(window.rs:118-143)
   - `SlotPool::create_buffer()` 返回 Buffer(wl_buffer 代理)
   - 存储在 `state.buffer: Option<Buffer>`
   - Buffer drop 时是否正确 destroy wl_buffer?完全依赖 smithay-client-toolkit
   - 如果 server 端资源(wl_buffer 对象)未释放,长跑会导致服务端内存增长

2. **Window 和 Surface 释放不明确**(window.rs:57-58, 46-49)
   - `compositor.create_surface()` 和 `xdg_shell.create_window()` 返回的对象
   - 当 State drop 时,这些对象是否向 server 发送 destroy 请求?
   - 如果 server 持有未释放的窗口对象,N 个 reconnect 后可能堆积

3. **Connection 和 EventQueue 清理**(window.rs:46, 48-49)
   - 函数返回时这些对象自动 drop
   - `Connection::drop()` 是否正确关闭 socket?依赖 wayland-client 实现

**推荐修法**:
- 立即: T-0107 跑测 teammate 的 soak benchmark 必须加内存检测(valgrind memcheck 或定期采样 /proc/[pid]/status RSS/VSZ, 1h <5% 增长)
- T-0102 集成 calloop 时: 审码强制检查所有 Wayland fd 的正确注册和清理
- 文档: 在 ADR 中明确"Wayland 对象生命周期管理由 SCTK 负责,via Drop trait"

### [P0-2] Buffer 更新逻辑中的资源浪费(T-0103 陷阱)

**位置**: src/wl/window.rs:116-143 draw_placeholder() + 248-251 configure()
**严重性**: 🟡 中 — 当前 first_configure 保证只调一次,但 T-0103 改动有风险

**问题描述**:
- `first_configure` flag 目前强制 draw_placeholder 只执行一次(line 244)
- 注释说"resize 处理是 T-0103 的范围",暗示 T-0103 会多次调用或移除此 flag
- 如果 T-0103 移除 `first_configure` 检查,每次 configure 都会 create_buffer
- 旧 buffer 被 `state.buffer = Some(new_buffer)` 覆盖,如果 Buffer drop 不够及时,可能导致 wl_buffer 对象堆积

**推荐修法**:
- T-0103 ticket 的 acceptance criteria 必须明确:"multiple configure 下,Buffer pool 管理不出现 fd 泄漏"
- 强制测试: `lsof -p [pid] | grep wl` 检查 Wayland client fd 数量不增长

### [P0-3] 错误恢复不完整(静默失败)

**位置**: src/wl/window.rs:248-251
**严重性**: 🟡 中 — 影响故障诊断,不是功能 bug

**问题描述**:
```rust
if let Err(err) = self.draw_placeholder() {
    tracing::error!(?err, "首帧占位绘制失败");
    self.exit = true;  // 只设置 flag, 没有错误传播
}
```
- 绘制失败时只 log 并设置 exit flag
- 下一轮 `blocking_dispatch` 仍会执行,可能再收到 configure 事件
- 由于 `first_configure = false`(line 247),不会重试
- 但 Wayland state 可能不一致(surface 未正确 commit)

**推荐修法**:
- Option A(推荐): 改 configure 签名让其返回 `Result`,错误向上传播
- Option B(快速修复): exit 之前显式调用 surface.destroy() 或 window.destroy()

## P1: 规则违规

**无明显违规** ✅

- unwrap/expect: 全部使用 `?` 和 `.context()`,零非测试 unwrap
- unsafe: `lib.rs` 和 `main.rs` 都 `#![forbid(unsafe_code)]`,无突破
- println: 全用 `tracing::info!/debug!/error!`
- TODO/FIXME: 无悬挂 TODO
- 注释: 大多写 why,少 what
  - 小扣分: line 129 注释 "Argb8888 little-endian: [B, G, R, A]" 是 what,但必需(字节顺序易错)

## P2: 架构/可读性

### [P2-1] State struct 文档注释缺失

**位置**: src/wl/window.rs:99-110
**优先级**: 🔵 低 — 15 分钟工作量

每个字段补 doc comment 说明用途、生命周期、何时 None 等。

### [P2-2] Lifetime 参数 `'l` 未充分说明

**位置**: src/event_loop.rs:15

`Core<'l, State>` 直接转发 calloop 的 lifetime。在 test 中用 `Core<'_, State>` 省略说明可能有简化空间。建议 doc comment 解释 `'l` 约束什么。

### [P2-3] calloop 集成 Wayland 的设计前瞻

**位置**: src/main.rs:14 + src/wl/window.rs:1-5
**优先级**: 🔵 设计层面

当前代码:
- T-0105 提供了 `Core<State>` calloop 包装
- T-0101 的 `run_window()` 仍用 `event_queue.blocking_dispatch()`
- main.rs 未集成两者

建议 T-0102/0103 开始前新开 ADR 0003 明确:
1. WaylandEventQueue → calloop EventSource 的适配器设计
2. State 生命周期是否需要改为 `'static`(calloop 要求)

## 总结

| 维度 | 分数 |
|---|---|
| 架构完整性 | ⭐⭐⭐⭐ Smithay/calloop 分层清晰 |
| 代码可读性 | ⭐⭐⭐⭐ 变量名清晰,流线性 |
| 逻辑正确性 | ⭐⭐⭐ 单线程安全,但资源管理黑盒 |
| 文档充分性 | ⭐⭐ 缺 struct doc comment |
| 长跑可靠性 | ⭐⭐ ⚠️ 内存泄漏风险未测 |

### 关键发现(Must-Do)

1. **P0-1 内存泄漏风险** — 最高优先级
   - 立即: T-0107 必须设计 soak benchmark + valgrind
   - 目标: 1h 运行 RSS 增长 <5%

2. **P0-2 Buffer 生命周期** — T-0103 风险
   - Code review 时强制检查 first_configure 逻辑
   - 多次 configure 下 fd 泄漏

3. **P2-1 文档补全** — 快速修复

### 后续 Ticket 建议

- **T-0102** 审码清单:
  - 所有 Wayland fd 是否注册到 Core::handle()?
  - EventLoop signal 清理路径?

- **T-0103** 审码清单:
  - resize 多次 configure 下 Buffer pool 是否有泄漏?
  - `lsof -p [pid]` 验证 wl_buffer fd 数量

- **T-0107** 首个跑测 benchmark:
  - 1h soak: 采样 /proc/[pid]/status RSS/VSZ, <5% 增长

### 合并判定

✅ **条件批准合并** — 无阻塞性 bug

- 当前代码逻辑正确、规则遵循
- P0 风险都是数据依赖(依赖上游 crate 的正确实现),不是代码 bug
- 建议: 先合并,后续由 T-0601(Phase 6 soak test)验证长跑可靠性
