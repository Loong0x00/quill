# 审码报告: Phase 2 规划 commit 8fc8a10

**审码人**: 审码 (quill-phase2 team, Haiku 4.5, Explore)
**日期**: 2026-04-25
**范围**: commit `8fc8a10` "Phase 2 计划: T-0201..T-0206 ticket + src/pty/ 骨架" — 8 文件 +426 行
- `tasks/T-0201-pty-spawn.md` .. `tasks/T-0206-integration-echo.md` 六条 ticket
- `src/pty/mod.rs` 88 行骨架 (PtyHandle + 6 个 `todo!()` 方法)
- `src/lib.rs` 加 `pub mod pty`
**判定**: ✅ 批准合并 (commit 已在 main)

## P0 / P1 / P2: 全部零发现

- ✅ `cargo build` / `test` / `clippy --all-targets -D warnings` / `fmt --check` 全绿
- ✅ 无 unsafe, 无 merge 冲突标记 (`<<<<<<<` / `=======` / `>>>>>>>`)
- ✅ `src/pty/mod.rs` 顶层 doc 明确 drop 顺序约束由 T-0201 在 `docs/invariants.md` 落地
- ✅ 未动改现有 invariants.md 条目 (只追加原则)
- ✅ 非测试代码零 `unwrap` / `expect` (全 `anyhow::Context`)
- ✅ 零 `println!`, 零未注释 unsafe, 零无 ADR 的架构改动
- ✅ 所有 `todo!()` 都挂注释指向具体 ticket (why 清晰)
- ✅ portable-pty 延后至 T-0201 装 (ADR 0002 已锁, 无需新 ADR)
- ✅ 六条 ticket 四段式完整 (Goal / Scope / Acceptance / Context + Impl notes)
- ✅ API 形状符合 CLAUDE.md 模块切分
- ✅ 依赖链清晰: T-0201 → T-0202 → T-0203 / T-0204 / T-0205; T-0206 依赖 T-0201+T-0203; T-0205 显式依赖 T-0104

## P3 建议 (不阻塞, Lead 已拍板采纳)

### [P3-1] T-0202 State.pty 字段位置

原 ticket 给了模糊二选一 "放 renderer 之前 或 conn/window 之后"。

**审码建议** (Lead 采纳, 对此前口头拍板的修正):
> 建议放 **conn 之后** (State 最后一位)。理由: pty 持有 Linux fd, 与 wl 指针/wgpu 资源生命周期**正交**。放最后 drop 有两点好处: (1) 不与 INV-001 的 renderer→window→conn 链条耦合 (不需要新增 INV); (2) 保证 wl 侧资源先释放干净, 避免 pty drop 触发 SIGHUP 时 wl 回调还在飞。

行动: Lead 更新 T-0202 ticket, 把"位置放在 renderer 之前"改为"位置放在 conn 之后 (State 最后一位)"。

### [P3-2] T-0203 O_NONBLOCK 归属

原 ticket 写"二选一" (T-0201 设 / T-0203 设)。

**审码建议** (Lead 采纳):
> 在 T-0201 的 `spawn_shell` / `spawn_program` 内统一 `fcntl(F_SETFL, O_NONBLOCK)`。理由: fd flag 应随 fd 创建立即设, 避免读字节时才发现未设导致阻塞整个事件循环。

行动: Lead 更新 T-0201 加 O_NONBLOCK scope 项 + INV-008; 更新 T-0203 改为 "sanity check 已置位" 不再负责设置。

## 给 Lead 的前置要求

### T-0205 条件性放行

T-0205 硬依赖 T-0104 `should_exit` 机制。当前 T-0104 在 `feat/T-0104` 分支 claimed 状态, 未合并到 main。

**建议** (Lead 采纳):
- T-0205 Acceptance 第一条加: `[ ] T-0104 已合并到 main, should_exit 机制可用`
- T-0104 合入 main 后 Lead 更新 T-0205 Status: open → 允许 claim
- 写码若误抢 T-0205 时遇到 should_exit 不存在, 自行 block 报回 Lead

## Lead 决策汇总 (写入决策日志)

| 决策点 | 规划原建议 | Lead 初次拍板 | 审码建议 | **最终决策** |
|---|---|---|---|---|
| State.pty 位置 | 二选一 | pty 放 renderer 之前 (最先 drop) | 放 conn 之后 (最后 drop) | **采纳审码**: 放 conn 之后, 正交 INV-001 |
| O_NONBLOCK 归属 | 二选一 | T-0203 做 | T-0201 做 | **采纳审码**: T-0201 统一设, T-0203 只 sanity check |
| T-0205 依赖 T-0104 | 硬依赖 | 保留硬依赖 | Acceptance 显式加一行 | **采纳审码** |

## 整体评价

规划完整, 约束清晰, dependencies 合理。两个 P3 小改定后 T-0201/0202/0203 即可由写码 teammate 开抢。src/pty/ 骨架不涉及架构级决策, 无 ADR 必要。
