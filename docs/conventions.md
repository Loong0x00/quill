# quill 写码 conventions

**来源**: 写码-close 在 Phase 1-3 共 13 ticket (T-0104 / T-0201..T-0206 / T-0108 / T-0301..T-0303) 沉淀的实战 idiom。
**目的**: per-ticket fresh agent 模式下, 接班写码读这一份 + CLAUDE.md + 上一单 audit 即可上手。
**所有 idiom 均来自实战, 不是"理想风格"。**

---

## 1. Commit message 风格

### 标题 (必须)
- ≤ 70 字符
- 格式 `<ticket>: <动作> <主题>` 或 `<ticket> <角色>: <说明>` (`fixup`/`followup` 类)
- 中英混排 OK, 但**避免标题里塞特殊符号** (`<<<<<<` `→` 这种) — 防 git log pipe 解析挂

例子:
```
T-0203: PtyHandle::read + calloop callback 真消费字节 + debug_assert O_NONBLOCK
T-0205: 子进程退出 → quill 退出 (方案 3 Arc<AtomicBool> + POLLHUP fix)
T-0302 fixup2: 恢复私有 from_alacritty (非 From trait), 保留 saturating cast
Merge T-0108+T-0301: calloop 统一 wayland/signal/pty + alacritty_terminal Term 集成
```

### Body (强烈建议)
四个必有段 (顺序固定, 缺 1 个 = 审码 P3 建议):

1. **What 改了** — 删 / 加 / 改三类列清楚, 不混
2. **Why 这么改** — 引用 ticket / audit / ADR / INV 编号 (traceability)
3. **副作用 / Drop-in 兼容** — API 行为是否变化、调用方要不要跟改
4. **测试 + diff stat** — `N tests pass, +X / -Y lines, clippy/fmt/build 全绿`

### HEREDOC 是必须

```bash
git commit -m "$(cat <<'EOF'
T-0xxx: 标题

body line 1
body line 2 含 → 或 `code`
EOF
)"
```

直接 `-m "..."` 含 `\r` `\n` 或 中文符号会被 shell 转义错。**用 single-quoted heredoc (`<<'EOF'`) 避免变量展开**。

---

## 2. 注释 idiom (CLAUDE.md "注释只写 why" 的实操)

### 三条铁律

**A. why, 不写 what** — 自描述代码不加注释。例:
- ❌ `// 增加计数` 配 `count += 1`
- ✅ `// alacritty 上游 advance 处理空切片是 no-op, 我们仍置 dirty 是因为调用方依赖 'advance 后 is_dirty=true' 做 frame trigger`

**B. 引 audit / ticket / ADR / INV 编号作 traceability** — 不让决策湮没在 git log:
```rust
/// 走 `CellPos::from_alacritty` (模块私有 saturating cast), scrollback
/// 历史的负 line 在本 API 路径下不会触发 ...
///
/// **刻意保留为模块私有 inherent fn, 不作为 `From<alacritty::Point>`
/// trait impl 对外暴露** — ... 是"单一绑定点"的真正落实 (审码 2026-04-25
/// T-0302 重审 P0-3 原话: "比 From trait 建议更严更好")。
```

**C. 引 audit 时用原话** — 防转述失真。审码报告的 P0/P1/P2/P3 标号 + 原话片段是金标准。

### Block-level 注释模板

```rust
/// <一句话功能>。
///
/// why: <为啥这样设计而不是替代方案>
/// <可能的踩坑、约束、上游 quirk>
///
/// **关键不变式** (如有): INV-XXX。违反后果: <UB / 死锁 / 数据损坏>
///
/// 测试覆盖: <指向哪个 test fn>
pub fn something() { ... }
```

### Inline `// SAFETY:` 块格式 (unsafe 必须)

```rust
// SAFETY:
// - <条件 1, 谁保证>
// - <条件 2, 怎么验证>
// - <returns / 资源所有权说明>
#[allow(unsafe_code)]
unsafe { libc::xxx(...) }
```

参 `src/pty/mod.rs::set_nonblocking` 4 点风格。审码会硬挡少于 3 点的 SAFETY (P3 建议级别)。

---

## 3. 测试组织

### 命名: `<动词>_<对象>_when_<条件>` 或 `<对象>_<行为>_<条件>`

```
spawn_program_true_succeeds_and_exits_cleanly
read_returns_wouldblock_when_no_data_yet
cursor_shape_reacts_to_decscusr
cellpos_from_alacritty_viewport_and_negative_line
```

避免 `test_xxx` / `it_xxx` — Rust 已经有 `#[test]` 注解, 前缀冗余。

### 位置

- **纯逻辑单测**: `#[cfg(test)] mod tests` 在同文件内 (可访问私有 fn)
- **集成测试**: `tests/<topic>_<verb>.rs` (只测 pub API, 模拟下游用法)
- **真 IO**: 集成测试, 允许 `std::thread::sleep` (CLAUDE.md 准许)
- **wayland / wgpu**: 不写自动化测试 (没 mock 设施), 只手测 + 状态机抽离测纯逻辑 (参 T-0107 `WindowCore::handle_event`)

### 抽状态机模式 (T-0107 / T-0205 / T-0302 一致)

公共代码里"复杂决策"抽成纯函数:
```rust
enum PtyAction { ContinueReading, ReturnContinue, RequestExit }
fn pty_readable_action(result: &io::Result<usize>) -> PtyAction { ... }
```

测试覆盖每个 enum variant (8-10 个 case 是常态)。`pty_read_tick` 真函数按 action 分派副作用 (trace / try_wait / loop_signal.stop)。审码会显式赞这个套路。

---

## 4. 派单接收 → 合并 完整流程

### Step 1: Claim
```bash
# 改 tasks/T-XXXX-*.md 的 Status: open → claimed, 加 Assigned
git add tasks/T-XXXX-*.md
git commit -m "T-XXXX: claim by <写码 name>"
git worktree add ../quill-impl-<slug> -b feat/T-XXXX
```

### Step 2: 实装
- 在 worktree 干活, 不污染 main
- 改动期间频繁跑 `cargo build` + `cargo test --lib <module>`
- 每个改动顺手写测试 (先写 `todo!()` 也行)

### Step 3: 四门
```bash
cargo build
cargo test
cargo clippy --all-targets -- -D warnings
cargo fmt --check
```
**全绿才进 step 4**, 不全绿继续改。

### Step 4: in-review
```bash
# Status: claimed → in-review
git add <具体文件>     # 不用 git add -A (踩过坑, 见 §6)
git commit -m "T-XXXX: <本 commit 主题>"

# Status 变更**独立 commit**, 语义清晰:
git add tasks/T-XXXX-*.md
git commit -m "T-XXXX: Status claimed -> in-review"

# 然后 SendMessage 审码, 内容必含:
# - 分支名 + commit hash 列表
# - 改动文件清单
# - 一句话 diff 摘要
# - "请重点 review 的点" 列表 (3-6 条)
# - 4 门跑通明细 (test 数, clippy/fmt 状态)
# - 硬约束核查清单 (对照 ticket scope)
```

### Step 5: 等审码

`SendMessage` 返回 success **不等于** 审码已读 / 已审。`idle_notification` 摘要的 `[to 审码]` 字段不是送达信号。

**强烈建议**: 发完 review 请求后, 5-10 分钟 follow-up 一次问 "收到了吗", 确认对方进 review 状态; 若没回, 通过 lead 转发兜底。

### Step 6: 通过 → 合并 (Option C squash 推荐)

如果有多个 fixup commits 来回 (像 T-0302 4 commits), 用 **Option C squash**:

```bash
cd ../quill-impl-<slug>
git reset --soft main
# 现在工作区有所有改动 staged, 但 commit 历史回到 main

# 把 src/* tests/* 的改动打成 1 个 code commit:
git reset HEAD -- tasks/T-XXXX-*.md
git commit -m "<HEREDOC: T-XXXX: <主标题>>"

# Status 改动单独 commit:
git add tasks/T-XXXX-*.md
git commit -m "T-XXXX: Status claimed -> in-review"

cd /home/user/quill
git merge --no-ff feat/T-XXXX -m "Merge T-XXXX: <一句话功能>"

# Status → merged + 收 audit:
git add tasks/T-XXXX-*.md docs/audit/2026-04-25-T-XXXX-review.md
git commit -m "T-XXXX: in-review -> merged + audit 报告存档"

# 清理:
git worktree remove ../quill-impl-<slug>
git branch -d feat/T-XXXX
```

### Step 7: SendMessage Lead 报完工

格式: 5 步流程逐条 [x] checklist + main HEAD hash + tests pass count + 下一步 idle / 准备接哪单。

---

## 5. 心理 SOP (跨 ticket 共享)

### 类型隔离 (T-0302 / T-0303 沉淀的硬规矩)

引入上游 crate 类型时 (`alacritty_terminal::Point` / `CursorShape` 等):

- ❌ `pub use alacritty_terminal::index::Point` re-export
- ❌ `impl From<alacritty::Point> for OurType` 公开 trait (下游 `p.into()` 绕过 wrapper)
- ✅ `pub struct OurType { ... }` 自定义
- ✅ `impl OurType { fn from_alacritty(p: alacritty::Point) -> Self { ... } }` **模块私有 inherent fn**, 下游构造不出来

理由: 换 VT 库 / 上游版本升级时, quill 渲染层只改 `from_alacritty` body, 不 cascade 改每个调用点。

### 派单 vs 实装对照

每次实装完准备提审, **写一张 Markdown 表格** 对比派单 scope 每条 vs 实际产出。差异 (无论加 / 减 / 改) 单独列, 主动告知 Lead, **不静默偏离**。

### Audit 报告是金标准

合并前必须读 Lead 落盘的 `docs/audit/<日期>-T-XXXX-review.md`, 确认审码评级 (P0/P1/P2 全过)。**不要因 SendMessage success 就默认通过**。T-0302 4 轮反复就是因为 Lead 转发 审码 initial 建议 vs 重审改口的时序错位 — 虽然抓回了, 但代价是 4 个 commits + 一次 Lead 介入仲裁。

---

## 6. 已知陷阱 (历史踩坑, 接班绕开)

### 陷阱 1: POLLHUP 漏检 (T-0205)
**情境**: rustix poll PTY fd 时只查 `revents().contains(PollFlags::IN)`。

**问题**: Linux slave 端关闭时 master 得 POLLHUP **不必带 POLLIN**, 旧条件漏过 → callback 不跑 → 子进程死了主循环也不退。

**抓回**: 手测 `pkill bash` 后 quill 没退, 看到 "master EOF" trace 没出现, 意识到 callback 根本没触发。

**修法**: `.contains(IN)` → `.intersects(IN | HUP | ERR)`。

**预防**: **自动化测试覆盖不了的 IO 边界一定要手测**。`cargo test` 全绿不等于行为对。

### 陷阱 2: `git add -A` 误添 logs/ (T-0205)
**情境**: 合并后改 Status, 用 `git add -A` 一把 add。

**问题**: `logs/` 下 MCP 工具的 audit log 被纳入跟踪, 违反 CLAUDE.md "NEVER git add -A" 准则。

**抓回**: `git show --stat HEAD` 检查实际 diff, 看到 `logs/.xxx-audit.json` 不该在那。

**修法**: 新 commit 用 `git rm --cached <files>` + `.gitignore` 加 `logs/`。

**预防**: **永远用 `git add <具体路径>`, 不用 `-A` / `.`**。审码 + Lead 都会查 commit diff 是否含意外文件。

### 陷阱 3: fixup1 regression (T-0302)
**情境**: Lead 转发 审码 第一轮建议 "用 `impl From<Point> for CellPos`", 写码照做。但 审码 第二轮 review 改口 "私有 inherent fn 比 From trait 更严", Lead approval 实际针对的是私有版本, 不是后来改的 From 版本。

**问题**: 静默 merge `229f5da` (From trait 版) = 把 审码 重审赞许的设计打回去。

**抓回**: Lead 消息说 "批准 fc00dfd" (fixup 之前的 commit hash), 写码意识到 hash 对不上 branch head, 主动停手 + `SendMessage 路 1/2/3 哪个`。

**修法**: `fixup2` 恢复私有 inherent + 保留 saturating cast 改进 (两路融合)。

**预防**: **Lead approval 引用具体 commit hash 时, 核对 branch head 是否该 hash**。不一致就 ping, 不要默认 "我后来的 commit 也算 approve 范围"。

---

## 7. 不要做的事

- 不要在非 `main` / `tests/` 里 `unwrap` / `expect` (用 `?` 或 `anyhow::Context`)
- 不要在 IO 线程做 >1ms 的工作 (INV-005)
- 不要在 wayland 回调里 panic (compositor disconnect 走 `Disconnect` event)
- 不要 `git push` (本项目本地开发, push 由 Lead 决定)
- 不要 skip git hooks (`--no-verify`) — hook fail 是信号, 不是噪音
- 不要 `git rebase -i` 在交互终端外 (用 `git reset --soft` + 多 commit 替代)
- 不要在 commit message 标题用 `\r\n` `→` 等特殊符号 (body 可以)
- 不要在 SAFETY 注释外用 "SAFETY" 关键字 (稀释, 审码会挡)
- 不要 re-export alacritty / wgpu / wayland 内部类型到 quill 公共 API
- 不要把 wayland Dispatch 状态 (State) 和 LoopData 混在一起 — Dispatch 不需要 term / signal 等

---

## 8. 项目当前状态 (2026-04-25, T-0303 合并后)

- main HEAD: T-0303 已合 (`b663b55` merge + `6db9648` Lead 跟进 + `d7dad65` 审码 handoff + `397a613` handoff v2)
- Phase 3 进度 3/7
- 65-69 tests pass (随 ticket 略有变化)
- 已合 ADR: 0001 / 0002 / 0003 / 0004 (0003 被 0004 supersede)
- INV-001..009 已登记
- TD-001 / 005 / 006 已 RESOLVED; TD-002 / 003 / 004 / 007 / 008 / 009 / 011 / 012 / 013 待
- Phase 2 6/6 全合; Phase 3 待: T-0304 scrollback / T-0305 渲染 / T-0306 resize / T-0307 ls -la

---

## 9. 接班路径

按本文档 + CLAUDE.md + `docs/invariants.md` + `docs/audit/2026-04-25-T-0202-T-0303-handoff.md` (审码侧) + 上一单 audit 报告, 1-2 小时上手。

具体 ticket 怎么打, 翻 git log 找 `T-0xxx:` 标头, commit message body 有完整决策记录。
