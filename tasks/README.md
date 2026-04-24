# tasks/

任务队列目录。写码 teammate 在此抢未认领的 ticket,领取后在自己的 worktree 干活。

## 文件命名

`T-<phase><id>-<slug>.md`

- `<phase>` 两位,对应 ROADMAP.md 的 Phase(01 / 02 / ...)
- `<id>` 两位,同阶段内顺序编号(01 / 02 / ...)
- `<slug>` 短横线连字 ASCII,不超过 40 字符

示例:

- `T-0101-wayland-window.md`
- `T-0203-pty-bytes-trace.md`
- `T-0406-glyph-cache-lru.md`

## 抢 ticket 流程

1. 写码 扫描 `tasks/` 找 `Status: open` 的 ticket
2. 把自己的 agent 名字写进 `Assigned`,`Status` 改为 `claimed`,commit 这一改动到 main
   (单独 commit,不与实现代码混)
3. 在独立 worktree 开发(`git worktree add ../quill-impl-<name> -b feat/<ticket-id>`)
4. 完成后 `Status` 改为 `in-review`,commit 到 feature 分支并通知 审码
5. 审码 放行 → 写码 合并到 main → `Status` 改为 `merged`
6. 未通过 → `Status` 回退为 `claimed`,改完重提

**锁粒度是文件级,不是 git branch 级**。同一个 ticket 文件不允许两个 agent 同时 claim。
worktree 与 branch 只是隔离实现代码,ticket 状态机跑在 main 分支的 `tasks/` 目录上。

## Ticket 模板

新建 ticket 时抄下方骨架,按实际内容填写。

```markdown
# T-<id> <title>

**Phase**: <n>
**Assigned**: (空 或 agent 名)
**Status**: open | claimed | in-review | merged
**Budget**: tokenBudget=<n>k, walltime=<n>s, cost=$<n>

## Goal
<用户可见的产出>

## Scope
- In: <会改哪些文件/模块>
- Out: <显式排除>

## Acceptance
- [ ] cargo test 通过
- [ ] cargo clippy -- -D warnings 通过
- [ ] 审码 放行
- [ ] <特定行为>

## Context
<相关 CLAUDE.md 章节 / ADR / 关联 ticket>
```

## 注意

- Budget 超限必须停手报告 Lead,不要悄悄追加
- Acceptance checkbox 未全绿禁止改 `in-review`
- 新 ticket 的 Phase 字段必须对应 ROADMAP.md,跨阶段 ticket 先跟 Lead 确认
