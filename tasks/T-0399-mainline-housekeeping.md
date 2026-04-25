# T-0399 Mainline Housekeeping (Phase 3 → Phase 4 转折点)

**Phase**: 3 (转折点)
**Assigned**: writer-T0399
**Status**: in-review
**Budget**: tokenBudget=80k (lead 派单)
**Dependencies**: 全 Phase 1-3 ticket merged

## Goal

按 `docs/audit/2026-04-25-mainline-audit-phase3-end.md` 跨 ticket 全局 audit 报告, 一单清掉 **P1-1 + P2-1..7 共 8 项 housekeeping**。Phase 4 字形渲染起步前清理基线 (死代码 / 注释脱节 / INV 漏登记 / 死 dep / 漏单测 / TD 漏标), 让 fresh writer 接 Phase 4 时看的是干净基线, 不是负债。

P3 9 项不做 (long-tail, 后续 ticket 视情)。

## Scope (按 audit 报告 P1/P2 编号)

### A. P1-1 — FrameStats 接入生产路径
- LoopData 加 `frame_stats: FrameStats` 字段
- src/wl/window.rs 的 idle callback 在 `term.clear_dirty()` 之前调 `frame_stats.record_and_log(Instant::now())`
- 加 `tests/frame_stats_integration.rs` 验 dispatch 后 `quill::frame` target 出 trace 行 (用 tracing-subscriber test fixture)

### B. P2-1 — 删 `event_loop::Core` 死代码 (选项 A)
- 删 `src/event_loop.rs` 整个文件
- 删 `tests/event_loop_smoke.rs` 整个文件
- 删 `src/lib.rs` 里 `pub mod event_loop;` 一行
- 总测试 89 → 87 (-2 孤儿 smoke 测试)

### C. P2-2 — 修 SAFETY 注释与 drop 序对齐
- src/wl/window.rs:610-614 + 644-648 两处 `unsafe { BorrowedFd::borrow_raw }` 的 SAFETY 注释
- 当前文字描述 drop 顺序与 T-0108 重构后实际相反, 实际不 UB (NLL 借用结束时机正确), 但文字误导审码
- 改为对齐 T-0108 后真实 drop 序 (event_loop drop 释放 source → BorrowedFd 借用结束 → state.pty drop)

### D. P2-3 — 修 INV-006 doc 引用过时
- `docs/invariants.md` INV-006 当前引用 T-0103 (推迟到 Phase 3+ 的 ticket)
- 应改引 T-0306 实际实装的 `propagate_resize_if_dirty` (drive_wayland 内紧接 dispatch_pending)

### E. P2-4 — 加 INV-010 类型隔离原则
- 6 单 (T-0302 / T-0303 / T-0305 / T-0306 / T-0307) 都在用同模式: alacritty 类型锁在 `src/term/mod.rs`, 私有 inherent fn `from_alacritty`, 不 re-export, 不 From trait, exhaustive match 无 `_ =>`
- audit 报告明示这是 INV-010 候选 (治理价值高 — 违反则 cascade refactor)
- 写 INV-010 entry 进 `docs/invariants.md`: 位置 / 约束 / 为啥 / 违反后果 / 验证 (grep 命令)
- 对照前 9 个 INV 的格式

### F. P2-5 — 删 thiserror 死依赖
- `Cargo.toml` 含 `thiserror = "..."` 但 `grep -rn "thiserror" src/` 应零命中
- 删 Cargo.toml 里 thiserror 依赖
- 跑 `cargo build` 确认零 break

### G. P2-6 — 加 propagate_resize_if_dirty 单测
- T-0306 加了 propagate_resize_if_dirty 函数但**无单测** (只有 cells_from_surface_px 纯 fn 4 单测 + resize_chain.rs 2 集成测试)
- 加单测覆盖: dirty=true 时调用 + dirty=false 时跳过 + cells_from_surface_px 计算后传给 term/pty 链路 (mock 或纯函数测)
- 至少 2 个新测试

### H. P2-7 — TD 同步度
- `docs/tech-debt.md` 检查:
  - TD-002 / TD-003 是否已经 OBSOLETE (T-0108 重构后场景不存在了?)
  - TD-012 是否已经 RESOLVED (某 Phase 3 ticket 顺手清掉?)
- 按 audit 报告 P2-7 段说明标 OBSOLETE / RESOLVED, 加日期 + 引用 commit hash

### Out

- **不做**: P3 9 项 (long-tail, 后续 ticket 视情)
- **不动**: src/term/mod.rs (除非 INV-010 引用其中代码) / src/pty/mod.rs / Cargo.lock / 公共 API 任何 breaking change
- **不引新 crate** (反过来删 thiserror)
- **不写新 ADR** (INV-010 是 INV 不是 ADR)

## Acceptance

- [ ] 4 门全绿 (`cargo build` / `cargo test` / `cargo clippy --all-targets -- -D warnings` / `cargo fmt --check`)
- [ ] 8 项 (A-H) 全部完成
- [ ] 测试数 89 - 2 (P2-1 删孤儿) + N (P1-1 + P2-6 加测试) ≈ 90+
- [ ] grep 验证: `grep -rn "thiserror" src/ Cargo.toml | grep -v "^Cargo.lock"` 零命中 (Cargo.lock 不删, 让 cargo 自然清)
- [ ] grep 验证: `grep -rn "FrameStats\|record_and_log" src/wl/window.rs` 命中 idle callback 真接入
- [ ] grep 验证: `grep -rn "INV-010" docs/invariants.md` 命中新 entry
- [ ] 审码放行 (P0/P1/P2 全过)

## 必读 baseline (fresh agent 启动顺序)

1. `/home/user/quill/CLAUDE.md`
2. `/home/user/quill/docs/conventions.md` (写码 idiom)
3. `/home/user/quill/docs/invariants.md` (INV-001..009 全文, 你要加 INV-010)
4. `/home/user/quill/docs/tech-debt.md` (TD-001..013, 你要标几条状态)
5. **`/home/user/quill/docs/audit/2026-04-25-mainline-audit-phase3-end.md`** (你的 ticket source-of-truth, 详读 P1-1 + P2-1..7 全文)
6. `/home/user/quill/docs/audit/2026-04-25-T-0306-review.md` (P3-1 INV-002 同步先例 + Renderer::resize 实装)
7. `/home/user/quill/docs/audit/2026-04-25-T-0202-T-0303-handoff.md` (5 主题, 类型隔离 §1 是 INV-010 来源)
8. `/home/user/quill/src/event_loop.rs` (你要删)
9. `/home/user/quill/src/wl/window.rs` (你要改 SAFETY 注释 + 接 frame_stats)
10. `/home/user/quill/src/frame_stats.rs` (你要接 caller, 看现有 record_and_log 签名)
11. `/home/user/quill/Cargo.toml` (你要删 thiserror)

## 已知陷阱

- 不要 `git add -A` (会误添 logs/), 用 `git add <具体路径>`
- 删文件前确认无生产路径引用 (P2-1 grep 已 verify, 但 P2-5 thiserror 也 verify)
- INV-010 entry 格式严格对照 INV-002 (T-0306 follow-up 后字段全列样式)
- frame_stats 接入要 LoopData 加字段, 注意字段顺序 (POD, 跟 cell_buffer_capacity 同类无 GPU 引用)
- propagate_resize_if_dirty 测试如果难直接测 (LoopData 含 wgpu 资源不好 mock), 可以抽出纯 fn `should_propagate_resize(state.resize_dirty, surface_size, term.dimensions())` 测 — 跟 conventions §3 抽状态机模式一致
- commit message 用 HEREDOC, 标题 ≤70 字符
- A-H 8 项**可分多个 commit** (清晰), 也可合并 1 commit (Option C squash 看 writer 选择)

## 预算

token=80k, wallclock=2h。比 T-0306 略大 (8 项 housekeeping), 但每项独立简单。完成后 SendMessage **team-lead** 报完工 + scope 对照表逐项 ✅。

## Phase 4 起步路径

T-0399 合并后, Lead 直接派 T-0401 cosmic-text 字体加载初始化 (Phase 4 首单)。
