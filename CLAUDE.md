# quill

极简 Wayland 终端模拟器。Rust。个人 daily driver。

**名字由来**:quill = 羽毛笔,写字工具。隐喻:终端 = 文字流的载体。

---

## 为什么存在

现有 Linux 终端在 6K + NVIDIA + Wayland 组合下都有硬伤:
- **Ghostty**: GTK4 event starvation hang + PageList 内存泄露 3 年没修
- **Foot**: 作为保底没问题,但没法加用户想要的新功能
- **Alacritty**: 稳定但极简,想加 feature 得 fork
- **Kitty**: 作者哲学激进,上游扯皮多

所以自己写一个:针对单用户(Lead = user + Claude Code 主 session)的工作流,不追求通用。

---

## 目标(Phase 1-6 必做)

- [ ] 跑 Claude Code 长时间无 memory leak(soak 1h RSS 稳定)
- [ ] 2x HiDPI 整数缩放 + CJK 字形正常
- [ ] fcitx5 输入法(Wayland `text-input-v3` 协议)
- [ ] 事件循环零 starvation(单线程 `ppoll` 绑所有 fd)
- [ ] 3-5K LOC 左右,架构读得懂

## 非目标(暂不做,不纠结)

- 多标签 / 分屏 / 滚动搜索(交给 tmux)
- ligature(可以以后加)
- Windows / macOS(Wayland-only)
- 任何"省显存/省 CPU"的优化技巧 —— 先正确,再性能

---

## 技术栈(锁死,非 ADR 不改)

| 层 | crate | 为啥 |
|---|---|---|
| Wayland 客户端 | `smithay-client-toolkit` | Wayland 生态主流 |
| 事件循环 | `calloop` | Smithay 配套,单线程 epoll |
| 渲染 | `wgpu` | 6K 分辨率需要 GPU |
| 终端状态机 | `alacritty_terminal` | Alacritty 拆出来的核心,成熟 |
| 字体 + CJK shaping | `cosmic-text` | COSMIC 桌面用,CJK / fallback 都做好 |
| PTY | `portable-pty` | fork + openpty 封装 |
| 输入法 | 手实现 `text-input-v3` | 没现成 crate,wayland-scanner 生成绑定 |
| 日志 | `tracing` + `tracing-subscriber` | Rust 主流 |

---

## 架构(50-ft 鸟瞰)

```
┌────────────────┐          ┌──────────────────┐
│ Wayland        │          │ 子进程 (shell)   │
│ compositor     │          │                  │
└────────┬───────┘          └────────┬─────────┘
         │ wl events                 │ bytes
         │                           │
         ▼                           ▼
    WinBackend ───► main ppoll ◄── PtyHandle
         ▲         (calloop loop)    ▲
         │                           │
       draw() ◄── StateMachine ◄── VtParser
                  (grid, cursor,    (alacritty_terminal)
                   scrollback)
```

**不变式**:
1. 所有 IO fd(wayland / pty / timerfd / xkb / d-bus)全部注册到同一个 `calloop::EventLoop`,绝不起 thread pool 做 IO
2. 渲染线程不做 >1ms 的任何计算,长任务分帧
3. PTY 写入必须非阻塞,背压时丢帧而非 stall

**模块切分**(暂定,Phase 0 完后细化):
- `wl/` — Wayland client + 渲染 surface
- `pty/` — 子进程 + fd 读写
- `vt/` — 解析 + 屏幕状态(包 alacritty_terminal)
- `text/` — cosmic-text 字体 cache + shaping
- `ime/` — fcitx5 text-input-v3
- `main.rs` — ppoll 绑所有源,事件 dispatch

---

## 开发准则(强制,审码 会挡)

- **一次 commit 做一件事**,diff < 300 行(大改拆成多个)
- 禁止"顺手优化" —— 性能改动必须单独 commit,附 bench 证据
- 非 `main`/`tests` 代码**禁用 `unwrap()` / `expect()`**,用 `?` 或 `anyhow`
- 任何 `unsafe` 必须有 `// SAFETY:` 注释说明不变式
- 依赖加新 crate → 必须 ADR(architecture decision record,存 `docs/adr/`)
- 注释只写 **why**,不写 what;自描述代码不加注释
- **先写测试再写实现**,骨架可以先挖坑(`#[test] fn name() { todo!() }`)
- 单 feature 打开 1 周没合 → 关闭 feature flag 重新切分

---

## Multi-agent 协议

**Lead = user + Claude Code 主 session**。不写码,做 规划 / 合并 / 最终裁决。

**强制 Agent Team 模式**(与用户全局 CLAUDE.md 一致,本项目硬约束):

核心价值(按重要性排):
1. **可审计** — 每个 agent 有 name + 共享任务板,Lead 随时能看每个 agent 进度到哪、输入输出啥
2. **并发可控** — team 级预算 / 状态一致性,agent 不私自疯跑

通信(`SendMessage`)是**副作用**,不是目的。Lead 介入优先,agent 之间握手能不做就不做。

硬约束:
- 所有并行 / 复杂任务必须 `TeamCreate` + `team_name` + `name` 寻址
- **禁止无 `team_name` 的一次性 fan-out** —— 只有单行无状态查询(例: "列出当前 git 分支")可例外
- team 名格式 `quill-phase<N>`,阶段结束 `TeamDelete` 清理

**Teammate 角色**(spawn 时明确,一个 teammate 只担一个角色):

| 角色 | 职责 | 实例数 |
|---|---|---|
| **规划** | 一次性:写 ADR / 定模块切分 / 定接口形状 | 阶段起始 1 个,然后退 |
| **写码** | 从 `tasks/` 抢未认领 ticket,在自己 worktree 干 | 2 个并行 |
| **审码** | pre-commit 审查 diff 是否违规 | 1 个常驻 |
| **跑测** | main 每次更新后跑 full test + soak + bench,发现回归开 issue | 1 个常驻 |

**Spawn prompt 必须自包含**(teammate 不继承 lead 对话):
- 任务目的(why)+ 边界(don'ts)+ 交付物(具体文件路径 / 测试)
- 相关背景文件路径(CLAUDE.md / 相关源码 / ADR)
- 接受标准(`cargo test && cargo clippy -- -D warnings && 审码 放行`)
- 硬预算:`tokenBudget=100k / wallClockTimeout=3600s / costBudget=$5 / recursionDepth=10`

**Worktree 约定**:每个 写码 用独立 worktree:
```bash
git worktree add ../quill-impl-<name> -b feat/<ticket-id>
```

**合并流程**:
1. 写码 本地 commit → 通知 审码
2. 审码 review diff:通过 → 写码 merge 到 main;不过 → 改了再来
3. 跑测 监听 main 新 commit → 自动跑 full test + 1h soak
4. 回归 → 开 issue 扔回 task queue

**禁止按组件分 agent**(反模式):不要"UI agent / DB agent / Docs agent",组件间天天卡接口。按**任务类型**分。

---

## 常用命令(占位,Phase 0 之后完善)

```bash
# 构建 + 测试(目前还没代码)
cargo build
cargo test
cargo clippy -- -D warnings
cargo fmt --check

# 以后
# cargo run --release                  # 启动 quill
# cargo test --test soak_1h            # 1h 稳定性测试
# cargo bench --bench render_frame     # 渲染性能 bench
```

---

## 禁止清单

- `println!` 调试(用 `tracing::debug!`)
- 非受控 `unwrap` / `expect`
- 架构级改动(模块切、数据流变)跳过 ADR
- IO 塞进渲染线程
- "以后再说" 的 TODO,要么现在解决,要么开 issue

---

## 相关文档

- `ROADMAP.md` — 分阶段任务
- `docs/adr/` — 所有架构决策(Phase 0 末建立)
- `tasks/` — 任务 queue(Phase 1 建立)
