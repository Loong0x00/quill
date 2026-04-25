# Mainline Audit: Phase 3 end (2026-04-25)

**审码人**: auditor-mainline (Phase 3 → Phase 4 转折点全局 audit, 跨 ticket fresh agent)
**范围**: main HEAD `c7c0dcd` (T-0307 Lead 跟进, Phase 3 ✅ 7/7 完工后)
**目的**: per-ticket reviewer 视野盲区, 找累积 / 漂移 / 死代码 / 同步度 bug
**checks**: `cargo build --release` ✅ / `cargo test --release` 89 tests ✅ / `cargo clippy -D warnings` ✅ / `cargo clippy -W clippy::pedantic -W clippy::nursery` 191 warnings (非 project policy 但有 6 类值得 P3) / `cargo audit` 数据库已加载, 211 deps 扫描中无 CVE 报警 / 无 TODO / FIXME / HACK / 生产代码零 unwrap / expect

---

## P0 阻塞 (Phase 4 起步前必修)

**0 项**。Phase 4 字形渲染可起。

---

## P1 严重 (Phase 4 内修)

### [P1-1] `FrameStats` 模块在生产路径完全未接入 — Phase 6 soak 前置缺失

**证据**:
```
$ grep -rn "FrameStats\|record_present\|record_and_log" src/
src/lib.rs:5:pub mod frame_stats;
src/frame_stats.rs:33:pub struct FrameStats { ... }
src/frame_stats.rs:61:    pub fn record_present(&mut self, now: Instant) -> Option<Snapshot> { ... }
src/frame_stats.rs:85:    pub fn record_and_log(&mut self, now: Instant) { ... }
# (后续仅 frame_stats.rs 的内部 cfg(test) mod)
```

**问题**: T-0106 完工 frame_stats 模块 + 本地 7 个单元测试. **但 `record_present` / `record_and_log` 在 `src/wl/window.rs` 的 idle / draw_cells 路径上零调用**. ROADMAP Phase 6 T-0601 "soak test 框架: 跑满 1h 监控 RSS 不增 >10%" + T-0106 模块 doc "Phase 6 的 soak test 需要一个长期稳定的信号来观察帧卡顿与 RSS 漂移" — 这个信号当前**永远不会产出**, 因为采集点没埋。

**根因 (跨 ticket 漂移)**: T-0106 完工时 T-0103 (resize wgpu swapchain) 是预期接入点, 但 T-0103 推迟到 Phase 3+; T-0305 引入 `draw_cells` (window.rs:725-732 idle callback 真渲染路径) 时**没有顺手加 `frame_stats.record_and_log()`**. 没人专门看这条跨段, per-ticket reviewer 也不背 "我得验之前 ticket 的产出还活着" 责任。

**修复路径** (Phase 4 内, 1 ticket / <1h):
1. `LoopData` 加 `frame_stats: FrameStats` 字段
2. window.rs:732 `t.clear_dirty();` 之前调 `frame_stats.record_and_log(Instant::now())`
3. 配套 `tests/frame_stats_integration.rs` 验 60 帧 dispatch 后 `quill::frame` target 出一行 (用 tracing test subscriber 截获)

不阻塞 Phase 4 起步, 但**必须在 Phase 5 fcitx5 接入前修**, 不然 Phase 6 起手第一步就发现采集点空着。

---

## P2 严重 (Phase 4 内宜修)

### [P2-1] `event_loop::Core<State>` 通用 wrapper 是生产路径死代码

**证据**:
```
$ grep -rn "Core<\|Core::" src/ tests/
src/event_loop.rs: pub struct Core<'l, State> { ... }   # 定义
tests/event_loop_smoke.rs:11: use quill::event_loop::Core;
tests/event_loop_smoke.rs:29: let mut core: Core<'_, State> = ...;
# src/wl/window.rs:602:  let mut event_loop: EventLoop<'_, LoopData> = EventLoop::try_new()...
#                       ^ 直接构造 calloop::EventLoop, 绕过 Core wrapper
```

**问题**: T-0105 设计 `Core<'l, State>` 包装 `calloop::EventLoop` 提供 `new` / `handle` / `signal` / `run` / `dispatch`. T-0108 calloop 三源统一时 `run_window` 直接调 `EventLoop::try_new()` + `event_loop.handle()` + `get_signal()` + `event_loop.run(...)`, **完全没经过 `Core` wrapper**. 

`Core` 只在 `tests/event_loop_smoke.rs` (2 个 test) 还活着 — 这是孤儿测试, 验证的是 calloop 上游 API 不是 quill 自家代码。

**漂移路径**:
- T-0105 写码-?: 设计通用 `Core<State>` 让回调 / 测试都从 quill 模块走
- T-0108 写码-close: 直接拿 calloop, 没改 Core 也没删, 也没 ping Lead "Core 用不上了"
- per-ticket reviewer (T-0108): 看 diff 觉得 calloop unification 对的就过, 没核 "T-0105 留下的 Core wrapper 是否被消费"

**修复路径**:
- 选项 A: 删 `src/event_loop.rs` + `tests/event_loop_smoke.rs`, 减少 ~120 lines + 2 tests, 总测试 89 → 87 全绿
- 选项 B: 改造 `run_window` 走 `Core::new()` 路径, 把 `Core<LoopData>` 真正接入生产路径
- Lead 决策: 推荐 A. Phase 4-5 不再做事件循环结构性变化, Core wrapper 没有具体功能价值, 删它换零认知负担

### [P2-2] `unsafe { BorrowedFd::borrow_raw }` 的 SAFETY 注释 vs 实际 drop 序矛盾 (window.rs:610-614, 644-648)

**证据** (window.rs):
```rust
// 第 602 行
let mut event_loop: EventLoop<'_, LoopData> = EventLoop::try_new()...;
// ... 中间 source 注册期间用到 wayland_fd / pty_borrowed ...

// 第 685 行
let mut loop_data = LoopData { event_queue, state, ... };  // state 被 move 进 loop_data
event_loop.run(None, &mut loop_data, ...)?;
```

SAFETY 注释 (window.rs:613-614) 说: "Rust 反向 drop 保证 event_loop(以及它拥有的 source) **先于** state 被 drop"

**实际 drop 序** (Rust 反向声明顺序): `loop_data` (line 685) → `event_loop` (line 602). `state` 已被 move 入 `loop_data`, 所以 **state 先于 event_loop drop**, 与 SAFETY 注释**完全相反**.

具体到 fd 寿命:
1. `loop_data` drops first → 字段顺序 drop: `event_queue` → `state` (renderer→window→conn→core→pty) → `term` → `loop_signal`. 此时 wayland fd / pty fd 已**关闭**.
2. **THEN** `event_loop` drops → 内部 `Generic<BorrowedFd<'static>>` source drop → calloop 内部 `epoll_ctl(EPOLL_CTL_DEL, fd)` 对已关 fd 返 `EBADF`.

`epoll_ctl` 对 closed fd **不 UB** (kernel 返 EBADF, calloop 0.14 内部多半 silent ignore 或 log), 所以严格安全。但 **SAFETY 文字本身是错的** — 后续维护者读"event_loop 先 drop"会做错误推理。

**漂移路径** (T-0202 → T-0108):
- T-0202 时代 `pump_once` 手写 poll, fd 寿命由当时局部变量管理, SAFETY 文字与代码匹配
- T-0108 重构改成 calloop EventLoop + LoopData 持 state, fd 寿命语义彻底变, 但 SAFETY 注释**整段保留** (注释是从旧 commit copy 过来的)
- T-0306 audit 没复核 SAFETY (P0/P1 都跑得 OK, 注释精确性 P3 默默放过)

**修复路径** (P2, 修注释而非代码):
```rust
// SAFETY:
// - poll_fd 返回的 fd 是 wayland_backend Connection 内部 socket;state.conn
//   在 LoopData.state 内,event_loop 在 line 602 声明、loop_data 在 line 685
//   声明 → drop 顺序是 loop_data 先 (state.conn 关 fd) → event_loop 后
//   (Generic source 的 epoll_ctl(EPOLL_CTL_DEL) 对已关 fd 返 EBADF, 非 UB)。
//   ↑ 与原文字"event_loop 先 drop"相反, 此处刻意修正以反映 T-0108 后真实
//     drop 序;BorrowedFd<'static> 的"语法 'static"不依赖 fd 实际活到 event_loop
//     drop 那一刻, 而依赖 calloop 内部 syscall 容忍 EBADF
// - poll_fd.as_raw_fd() 只取 int,不涉资源转移
```

或修代码: 把 `event_loop` 移入 `LoopData` 字段或反转声明顺序, 让 event_loop 真的先 drop. 但代价是改回调签名, 性价比不高。**改 SAFETY 注释更便宜**。

同样问题在 pty_borrowed (window.rs:644-648), 同款修法。

### [P2-3] INV-006 doc 引用已不存在的 caller

**证据** (`docs/invariants.md:112`):
```
**清零**:上层(T-0103 wgpu swapchain 重建者)**必须**在每次 resize 处理完
**显式** `core.resize_dirty = false`
```

**实际**: T-0103 推迟到 Phase 3+, 真消费者是 T-0306 的 `propagate_resize_if_dirty` (window.rs:437-476). T-0306 audit (`docs/audit/2026-04-25-T-0306-review.md:48-52`) 已经独立指出"INV-006 消费者单一性增强 — 现在'上层'具体落到 propagate_resize_if_dirty 一处", 但 invariants.md 文字未同步。

**修复路径** (P2, 1 行改):
```diff
-**清零**:上层(T-0103 wgpu swapchain 重建者)**必须**在每次 resize 处理完
+**清零**:上层(`propagate_resize_if_dirty` 在 `drive_wayland` `dispatch_pending`
+ 之后调一次, 见 `src/wl/window.rs:437`) **必须** 在每次 resize 处理完
**显式** `core.resize_dirty = false`
```

### [P2-4] INV-010 类型隔离原则 候选未落地, 9 条 INV 未保护项目最重要 SOP

**证据**:
- `docs/audit/2026-04-25-T-0202-T-0303-handoff.md:90`: "**INV-010 候选**: 类型隔离原则 (P0 违规则未来 cascade refactor) 治理价值高, 建议 Lead 在 T-0304 起步前考虑追加进 `docs/invariants.md` 或 CLAUDE.md '禁止清单'"
- T-0302 / 0303 / 0304 / 0305 / 0306 / 0307 audit 报告**每一份**都有 "类型隔离 SOP 第 N 次应用" 段落
- `docs/invariants.md` 仍只有 INV-001..009 (T-0307 reviewer 按 SOP 验过 6 次零违规)

**问题**: 类型隔离 (alacritty/wgpu/wayland 类型不外泄到 quill 公共 API; 转换走模块私有 `from_alacritty` / `from_cosmic` inherent fn 而非 `From` trait) 是 quill 项目**最重要单一 invariant** — Phase 4 cosmic-text 接入 + Phase 5 fcitx5 接入都依赖. 但 9 条 INV 没保护这条, 完全靠 conventions.md §5 + handoff §1 的"软约束"。

新写码 fresh agent 看 invariants.md 不会知道这条; 看 conventions.md 会知道但是"风格" 而非 "硬约束". 风险: Phase 4 写码-T0401 引入 cosmic-text 时 `pub use cosmic_text::Buffer` 一行就违反, 没 INV 保护就只能靠审码兜底。

**修复路径** (P2, 写一条 INV):
```markdown
## INV-010: 上游 crate 类型不出公共 API 边界

**位置**: `src/term/mod.rs` (alacritty), 未来 `src/text/mod.rs` (cosmic-text),
`src/ime/mod.rs` (fcitx5/wayland-protocols), `src/wl/render.rs` (wgpu).

**约束**: 凡是上游 crate 暴露的 enum / struct (例: `alacritty_terminal::index::Point`,
`cosmic_text::Buffer`, `wgpu::TextureFormat`, `wayland_protocols::xdg::*`), 一律
不得作为本 crate `pub` 类型 (函数参数 / 返回值 / 字段) 直接暴露:
- ❌ `pub use alacritty::Point;`
- ❌ `pub fn cells_iter() -> impl Iterator<Item = alacritty::Cell>`
- ❌ `impl From<alacritty::Point> for CellPos`  ← trait 公开等于偷渡
- ✅ `pub struct CellPos { col, line }` + `fn from_alacritty(p) -> Self` (模块私有 inherent)

**违反后果**: 换 VT 库 / wgpu 升级 / wayland-protocols 大版本时 cascade 改下游
所有调用点; 类型外泄一次后续永远撤不回 (semver-major break).

**验证**:
- `grep -nE 'pub use (alacritty|wgpu|wayland|cosmic)' src/` 应零命中
- `grep -nE 'impl From<(alacritty|wgpu|wayland|cosmic)' src/` 应零命中
- conventions.md §5 类型隔离 SOP 是日常实施手册
```

### [P2-5] `thiserror` 是死依赖

**证据**:
```
$ grep -rn "thiserror\|use thiserror" src/ tests/
# 零命中
```
`Cargo.toml:16` 列出 `thiserror = "2"`, **整个 crate 零导入零使用**.

**问题**: 增加构建时间 + 依赖图复杂度 + cargo audit 噪音. 当前代码用 `anyhow` 做 application errors, `std::io::Error` 做 IO, 不需要 derive Error trait.

**修复路径**: `cargo rm thiserror` (或 `Cargo.toml` 删一行), `cargo build` 验, 减小依赖。或登记 TD-014 等 Phase 5 fcitx5 / Phase 6 config 错误类型时再决定是否拉回。**推荐删**: 减少 fresh agent 看 Cargo.toml 时的 "这是干啥的" 认知负担。

### [P2-6] `propagate_resize_if_dirty` 私有 fn 无单测覆盖

**证据**: window.rs:437-476 实装了 INV-006 唯一消费路径 (early-return 早返 + split borrow + r/t/p 三方 resize + 清 dirty). 包括:
- `if !data.state.core.resize_dirty { return; }` 早返
- `state.core.resize_dirty = false;` 清零 (INV-006)
- `let LoopData { state, term, .. } = &mut *data;` split borrow
- `r.resize / t.resize / p.resize` 三方调用顺序
- `pty.resize` 失败 warn-only 不 panic

**测试覆盖**:
- `tests/state_machine.rs` 11 单测: 只测 `handle_event` 纯逻辑, 不碰 propagate
- `tests/resize_chain.rs` 2 单测: 只验 term + pty lockstep, 不调 propagate (`propagate_resize_if_dirty` 是私有 fn 集成测试访问不到)
- `src/wl/window.rs::tests::cells_from_surface_px_*` 4 单测: 只验换算, 不碰 propagate
- 整个 `propagate_resize_if_dirty` **行为契约**没自动化测试

**风险**: 改 propagate (例: 加 renderer.resize 错误处理 / 调换三方顺序 / 不小心删清 dirty 那行) 不会被 cargo test 拦截, 只在手测 cargo run + 拖窗口时显形.

**修复路径** (P2, 1 个 ticket):
- 提升 `propagate_resize_if_dirty` 为 `pub(crate)` 或抽 inner pure fn, 测试构造 mock LoopData (renderer 字段 None / pty 字段 None) 验:
  - `dirty=false` → 早返不动 fields
  - `dirty=true + renderer=None + pty=None` → 只 term.resize + 清 dirty
  - `dirty=true + pty.resize 故意失败` → warn 不 panic, term/dirty 仍 OK
- 或不改可见性, 写 `tests/propagate_resize_smoke.rs` 通过 `run_window` 间接驱动 (但需 mock Wayland, 复杂度过高)

### [P2-7] tech-debt.md 同步度: 3 项过期, 应标 OBSOLETE / RESOLVED

**TD-002** (T-0202 pump_once BorrowedFd SAFETY 注释详略差异): 引用 `src/wl/window.rs:272-275`, 但 `pump_once` 整段在 T-0108 已**删除** (window.rs:186-194 注释明示). TD 指向不存在代码, 应标 OBSOLETE 而非"待办"。

**TD-003** (T-0202 run_window BorrowedFd<'static> 未明写 Rust 反向 drop 保证): 引用 `src/wl/window.rs:408-412`, 实际现行代码 (window.rs:613-619, 645-650) **已加 SAFETY 注释明写 drop 序** — 虽然 [P2-2] 指出该注释**写错了方向**, 但 "未明写" 这条 TD 已 RESOLVED. TD-003 应改为 OBSOLETE + 新登 TD-017 (SAFETY drop 序文字错位).

**TD-012** (pty master EOF 导致 pump_once busy-warn): 描述的是 T-0203 时代 pump_once + Level + 不 stop 的死循环。T-0205 实装时 `pty_readable_action::RequestExit` → `loop_signal.stop()` 闭环已修, T-0108 又把 pump_once 整段删除, **fdr 已不存在 busy-warn 路径**。TD-012 应标 RESOLVED (实施在 T-0205, 路径在 T-0108 完全消除)。

新登记建议:
- **TD-014**: thiserror 死依赖 (P2-5)
- **TD-015**: FrameStats 模块未接入 draw 路径 (P1-1)
- **TD-016**: event_loop::Core 死代码 (P2-1)
- **TD-017**: SAFETY 注释 drop 序错位 (P2-2)
- **TD-018**: INV-006 doc 引用过时 caller (P2-3)
- **TD-019**: propagate_resize_if_dirty 无单测 (P2-6)

---

## P3 long-tail (Phase 4-5 视情况, 不阻塞)

### [P3-1] `pty.resize(cols as u16, rows as u16)` 截断 (window.rs:456)

`cells_from_surface_px` 算出 cols/rows 是 usize, 经 `as u16` 投到 PTY. 64-bit 平台 usize → u16 silent truncate. 实际场景: 6K 屏幕 6144 / 10 = 614 cols 距离 65535 还有 100 倍裕量, 不会触发. P3 lint。

### [P3-2] `cells_from_surface_px` f32 cast 精度链

window.rs:409-410: `((width as f32) / CELL_W_PX) as usize`. width 是 u32. f32 mantissa 23-bit, 超过 ~16M 像素的 width (2^24 / 10 ≈ 1.6M cols) f32 表示开始有精度丢失。实际不会触发, P3 lint。

### [P3-3] TD-004 仍存在: `tests/pty_calloop_smoke.rs:38-49` 内 "SAFETY 提醒" 关键字稀释 unsafe SAFETY 语义

T-0202 audit 报告的 P3-1, 已登记 TD-004, 当前文件未改。改名"// 不 drain 的原因..."消除 P3 关键字稀释。

### [P3-4] `tests/pty_echo.rs::drop_cleans_up_long_running_child` 依赖 pgrep + 沙盒兼容性

`pgrep -f "sleep 600.5"` 在容器 / 沙盒环境 (例: Docker / nspawn / GitHub Actions runner) 行为可能与 Arch host 不同 (PID namespace 隔离, 父进程不是 init 而是 docker init)。当前 TD-007 已登记同类问题, 本 case 与之相关。Phase 6 CI 配置时再验。

### [P3-5] alacritty `Config::default()` scrolling_history=10000 行 Phase 6 RSS 上限固定

src/term/mod.rs:494 用 `Config::default()`. alacritty 0.26 默认 10000 行. 80×24 grid, 每 cell ~32 字节 (alacritty 内部 `Cell` struct), 10000 × 80 × 32 = ~25 MiB scrollback memory ceiling. Phase 6 soak 验 RSS 时这个上限固定, 但 ROADMAP Phase 6 T-0606 配置文件入口可纳入 `scrolling_history` 调节, 给重度 / 轻度用户 trade-off 空间。

### [P3-6] `TermState` pub API 表面积 在 src/ 内零生产 caller (preparation 状态)

未在 src/ 调用 (仅 tests):
- `line_text` / `cursor_visible` / `cursor_shape` / `scrollback_size` / `scrollback_line_text` / `scrollback_cells_iter`

**性质**: Phase 4 字形渲染 / cursor 渲染 / scroll-up UI 显式准备 hook, 现阶段 unused 是设计选择, 不是漏洞. 但 `cargo doc` / `cargo public-api` 工具会报"unused public API", 容易在 Phase 4 ticket scope 决策时混淆 "我加新字段还是消费这条 hook". 提醒, 不是修复。

### [P3-7] SIGINT/SIGTERM e2e 测试缺失

- `event_loop_smoke.rs::core_runs_three_source_kinds_and_exits_cleanly` 注册 SIGUSR1 source 但**不 raise**
- `state_machine.rs::close_sets_exit_flag` 是 `WindowEvent::Close` mock, 不走真 signalfd
- 真 SIGINT → calloop signalfd 回调 → loop_signal.stop() → run() 返回的端到端路径**只在手测覆盖**

**修复路径**: Phase 6 soak harness 起来时, 写一个 `tests/signal_e2e.rs` spawn quill 子进程 + sleep + kill -INT pid + waitpid 验 exit code. 现在不阻塞。

### [P3-8] "SIGTERM 中途 PTY EOF 同时发生" 这种 race 测试缺失

prompt §9 提到。当前不覆盖 — 两条 stop() 路径同时进入 calloop dispatch 是否安全 (LoopSignal::stop 多次幂等). calloop 0.14 上游应该容忍, 但 quill 测试没断。Phase 6 soak 期间观察自然覆盖。

### [P3-9] `PtyHandle::resize` 用 `&self` 但 ioctl 失败仅 warn — 无 short-circuit

window.rs:455-466: `p.resize(cols as u16, rows as u16)` 失败 `tracing::warn!` 后本帧静默继续. **下一次** wayland resize 触发 propagate 仍调同样 fd, 仍同错, 仍 warn — 死循环式 warn flood 风险 (Phase 6 soak 才显形). 修复路径: 把 `pty.resize` 错误用 `RefCell<Option<bool>>` 标记, 一次失败后跳过剩余 resize. 不阻塞 Phase 4-5。

---

## 跨 ticket 漂移分析 (per-ticket reviewer 视野盲区)

### 漂移 1: T-0202 SAFETY 注释 → T-0108 重构后语义错位

T-0202 (pump_once 手写 poll 时代) 写的 SAFETY 注释配的是当时局部变量管理的 fd 寿命. T-0108 改 calloop EventLoop + LoopData 持 state, **fd 寿命语义彻底变 (loop_data 先 drop, state.conn 已关 → event_loop 后 drop, Generic source 看到的是 already-closed fd)**, 但 T-0108 写码 + 审码都没复核 SAFETY 文字. T-0306 audit 复核到了 propagate 和 INV-006 的衔接, 但 SAFETY 注释精确性是审码 P3 默默放过, 没人挑。**当前 SAFETY 文字与代码语义相反** (P2-2)。

### 漂移 2: T-0105 → T-0108 设计否定但代码留存

T-0105 设计 `Core<State>` wrapper, 主张"业务代码用 Core 而不直接碰 calloop". T-0108 实装时直接用 calloop EventLoop, 没经 Core. T-0108 audit 报告聚焦"三源统一+calloop unified loop"决策, 没反向问"T-0105 留下的 Core wrapper 现在还应不应该存在". 结果: `src/event_loop.rs` 整模块 + `tests/event_loop_smoke.rs` 是"设计否定后没清"的代码 (P2-1)。

### 漂移 3: T-0106 → T-0103 推迟 → T-0305 接入失误 → Phase 6 启动时才会被发现

T-0106 模块 doc 显式说"Phase 6 soak test 需要这个信号". 计划接入点是 T-0103 (resize swapchain) 或 T-0102 (clear pass) — 后者已合, 前者推迟. T-0305 引入真渲染路径 `draw_cells` 时 (window.rs idle callback) **没注意到 frame_stats 还没被任何人接入**, 因为 T-0305 写码-T0305 / 审码-T0305 都聚焦在"draw_cells 出 pixel"的视觉验收, 没核 "之前 ticket 的产出还活着吗". P1-1 是 16 ticket 累积漂移最严重的一条 — Phase 6 soak 起手第一天就会发现采集点空着。

### 漂移 4: TD 同步度滑坡

13 条 TD 中, **3 条过期** (TD-002 / TD-003 / TD-012, 都是 T-0108 删除路径残留). 每条 ticket 完工时 Lead 跟进只 +1 RESOLVED 进度, 没系统复核"是否有 TD 因本 ticket 间接消失". 长期下来 tech-debt.md 不再 single-source-of-truth, 新 agent 读 TD-002 还在认真想 "T-0202 pump_once SAFETY 怎么改", 实际代码已没了。

---

## tech-debt 同步建议

| TD | 现状 | 建议 | 理由 |
|---|---|---|---|
| TD-001 | RESOLVED ✅ | 保持 | T-0108 |
| TD-002 | "未修" | **OBSOLETE** | pump_once 已删 |
| TD-003 | "未修" | **OBSOLETE** + 新 TD-017 | run_window SAFETY 已加但写错方向 |
| TD-004 | "未修" | 保持 | 测试注释关键字稀释仍存在 |
| TD-005 | RESOLVED ✅ | 保持 | T-0108 |
| TD-006 | RESOLVED ✅ | 保持 | T-0108 |
| TD-007 | "未修" | 保持 | bash -l 收尸假设, 未触发 |
| TD-008 | "未修" | 保持 | INV-009 release 无验证, 未触发 |
| TD-009 | "未修" | 保持 | xdg close E2E 测试缺失 |
| TD-010 | "未修" | 保持 | bash cwd 配置, Phase 6 |
| TD-011 | "未修" | 保持 | sanity check SAFETY 详略 |
| TD-012 | "T-0205 必接手" | **RESOLVED** | T-0205 + T-0108 双重消除 |
| TD-013 | "未修" | 保持 | sanity check 抽 fn 风格 |
| **TD-014 (新)** | — | thiserror 死依赖 (P2-5) |
| **TD-015 (新)** | — | FrameStats 未接入 draw 路径 (P1-1) |
| **TD-016 (新)** | — | event_loop::Core 死代码 (P2-1) |
| **TD-017 (新)** | — | SAFETY 注释 drop 序错位 (P2-2) |
| **TD-018 (新)** | — | INV-006 doc 引用过时 caller (P2-3) |
| **TD-019 (新)** | — | propagate_resize_if_dirty 无单测 (P2-6) |

---

## INV 同步建议

| INV | 现状 | 建议 |
|---|---|---|
| INV-001..005, 007..009 | 文字与实装一致 | 保持 |
| **INV-006** | doc 引用 "T-0103 wgpu swapchain 重建者" 但实际是 propagate_resize_if_dirty | **修文字** (P2-3) |
| **INV-010 (新)** | 未登记 | **追加**: 上游 crate 类型不出公共 API 边界 (P2-4); 是 quill 项目最重要 SOP, 无 INV 保护是高风险 |

---

## Phase 4 起步建议 (基于本次 audit)

### 起步前应修 (1-2 个 housekeeping ticket, <半天)

**T-0399 (建议命名) housekeeping 单**, 一并清:
1. P1-1 FrameStats 接入 `draw_cells` 后 `record_and_log` (不阻塞 P0, 但 Phase 5 之前必须)
2. P2-1 删 `src/event_loop.rs` + `tests/event_loop_smoke.rs` (或决策保留, Lead 拍)
3. P2-2 修 window.rs:613-614, 645-648 的 SAFETY 注释 drop 序文字
4. P2-3 修 invariants.md INV-006 文字
5. P2-4 追加 INV-010 类型隔离
6. P2-5 删 thiserror dep
7. P2-7 同步 tech-debt.md (TD-002/003 OBSOLETE, TD-012 RESOLVED, 新增 TD-014..019)

P2-6 (propagate_resize_if_dirty 无单测) 可 fold 进 Phase 4 T-0401 起步前自然加测试。

### Phase 4 字形渲染 specific 注意事项

1. **类型隔离 SOP 第 7 次应用** (cosmic-text 接入). 建议:
   - cosmic-text 类型 (`Buffer`, `FontSystem`, `SwashCache`) 锁在 `src/text/mod.rs` 内
   - quill 自定义 `pub struct ShapedRun { glyphs: Vec<Glyph>, ... }`, 私有 `from_cosmic` inherent
   - INV-010 落地后, 写码-T0401 派单可直接引用
2. **INV-002 字段顺序追加准备**: T-0403 glyph atlas 引入 `wgpu::Texture` + sampler + bind_group. Renderer 字段顺序需要扩展, 复核 surface → cell_pipeline → cell_vertex_buffer → atlas → device → queue → ... → instance 的反向 drop. Phase 4 起步前 ADR / 派单中明示。
3. **Renderer.resize 现签名兼容性**: 当前 `(width, height: u32)` 是 surface 像素. Phase 4 HiDPI 接入 `wl_output.scale` 后, Renderer 内部 cell px 常数 (`CELL_W_PX = 10.0`) 应升级为 `cell_w_logical_px * scale_factor`. 改 Renderer 内部 state 而不改 resize 签名, 上游不用感知。
4. **frame_stats 接入位置**: Phase 4 引入 glyph atlas 后, draw 时机会从单 dirty 触发 (T-0305) 变多源 (resize / atlas miss / scale change). 先把 frame_stats 拼装好, Phase 4 / 5 接入新 source 时不会忘。
5. **cells_iter 性能复核**: 当前每帧 `t.cells_iter().collect::<Vec<_>>()` (window.rs:724) 在 80×24 是 1920 cells, 60 fps 下 115K alloc/sec. Phase 4 字形渲染加 glyph 缓存 + atlas lookup 后 frame budget 紧, Phase 6 soak 必须 bench. 现在不优化 (TD 准备), 但 P3-6 提到的 Vec 复用接口先想好。

### Phase 5 fcitx5 specific 注意事项

text-input-v3 是新 source 进 calloop, 沿用 T-0108 LoopData 字段 split borrow 套路, 不动现有 wayland / signal / pty 三 source。Phase 5 起步前确认 P2-2 SAFETY 注释已修, 否则新 ime fd 加 BorrowedFd 时复制错误模板。

---

## 整体评价

quill 项目当前状态: **A-** (健康, 个别累积漂移, 无 P0 阻塞, Phase 4 可立即起)

- **架构**: ⭐⭐⭐⭐⭐ — calloop 三源统一 (INV-005) 真落实, `LoopData` 字段 split borrow 范式优雅, Renderer / TermState / PtyHandle 三 module 公共 API 干净, alacritty 类型彻底锁在 `src/term/mod.rs` (类型隔离 SOP 第 6 次连续零违规)
- **测试**: ⭐⭐⭐⭐ — 89 tests 全绿 (release 也过), 抽状态机 (`pty_readable_action` / `handle_event`) 纯函数 + `tests/state_machine.rs` 11 单测 + `tests/ls_la_e2e.rs` ls -la 端到端覆盖度好。1 P2 (`propagate_resize_if_dirty` 无测) + 1 P3 (SIGINT e2e 缺) 是已知坑
- **文档**: ⭐⭐⭐ — invariants.md / tech-debt.md / conventions.md / 18 份 audit 报告归档完整, 但**同步度滑坡** — INV-006 doc 过期, 3 条 TD 过期, INV-010 候选未落地. 当前每单 audit 都是高质量, 但跨 ticket 索引层有累积债
- **代码质量**: ⭐⭐⭐⭐ — 生产代码零 unwrap / expect, unsafe 块都有 SAFETY 注释 (虽然 P2-2 文字错位但形式齐), tracing 32 处合理. clippy `-D warnings` 全过. clippy pedantic 191 warnings 多数是 `must_use` / docs-backticks / 不阻塞风格
- **Phase 1-3 履约率**: 7/7 + 6/6 + 7/7 = **20/20 ticket 全合**, per-ticket fresh agent 范式 6 单连续无返工 (T-0304 起切换至今)
- **跨 ticket 累积债**: 1 P1 + 7 P2 + 9 P3, 都是 per-ticket reviewer 视野盲区, 这正是 mainline audit 的核心价值

16 小时 16+ ticket 是**可重复的高速度**: per-ticket fresh agent + 结构化 docs + 类型隔离 SOP + calloop 单调度器是 4 个真正放大器。Phase 4 字形渲染前清掉 P1 + P2 (~半天 1 ticket), 然后用同样节奏继续。

**审码 mainline auditor 签字**, Phase 4 可起。
