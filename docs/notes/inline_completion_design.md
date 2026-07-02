# Inline Completion (Composer Mode) 设计 v3

> 状态: 规划阶段 / 已通过 codex 二轮审查必改项修正
> 起源: 2026-05-11 user 与 Claude Opus 4.7 对话, vibecoding 模式
> 实现委托: Codex GPT-5.5
> 范围: 直接做完整版, 不分 MVP

---

## 1. 用户需求 (原话)

> "输入比如 git 然后 tab 出现候选但是按空格后不会更新候选, 能理解么. 我希望要的是 minecraft 的那种提示候选逻辑"
>
> "我想要的就只是 [Claude Code/Codex 截图] 这样, 没有那么多复杂的功能. 输入命令然后显示可以的输入用来替代 --help. 以及敲完整命令"
>
> "嗯难道这不是一次性全部实现的么, 毕竟嗯所有的工具几乎都支持各种形态的 help 难道不能自动索引么? 自动从工具那里搞来表然后. 就自动化"

核心: 实时 keystroke 触发 + popup 显示候选 + 描述 + 空格后自动更新 + 数据 100% 自动化 (`--help` 抓取)。

## 2. 已尝试方案及失败根因

| 方案 | 结果 |
|---|---|
| zsh `menu select` | 一次 tab 只做 prefix 扩展, 二次才进 menu, 候选无描述, 空格后不更新 |
| `LISTMAX=1000` | 解决阻断, 但显示仍占行 grid |
| `fzf-tab` | quill PTY 下卡死 + readline 架构无法解决空格后不更新 |
| `carapace-bin` | 数据层 OK, 显示层依然受 zsh 限制 |

**根因**: readline 是离散事件触发 (按 tab 才查), 不是实时键流。要 MC / Claude Code 那种体验必须由 quill 自己拥有 prompt 输入层。

## 3. 目标 / 非目标

**目标**:
- composer 模式: quill overlay 接管 prompt 行 keystroke
- 实时候选: 每 keystroke debounce 30ms 触发查询, 空格后自动切下个 token
- popup overlay: 候选浮窗在 prompt 光标下方
- 候选带描述
- 数据源: `<cmd> --help` 自动索引 (确定性 parser, 不引入 LLM — 用户 2026-05-11 决定砍 LLM 层避免隐私 / 网络依赖 / 延迟开销)
- dynamic hooks: ~10 个特例 (git/cd/ssh/kill/docker/kubectl/pacman/yay/systemctl)
- pipe segment 切换 + fuzzy 过滤 (skim 算法)
- 提交: enter 整条命令一次性 push 给 PTY shell

**非目标**:
- 替代 shell PROMPT (PS1) — 用户 shell 仍跑, composer 是 overlay 不抢 grid
- 多 shell 兼容 — 只支持 quill composer + 任意 shell 执行
- 跨平台 / AI 推荐 / 复杂 unicode 引号

## 4. 架构

### 4.1 数据流

```
keystroke (wl_keyboard 协议事件)
    ↓
src/wl/keyboard.rs::key_press_action() — 计算 KeyboardAction (返回 Composer/Hotkey/Paste/Nothing)
    ↓ 在此层判断: composer active && shell prompt && IME idle && !alt-screen → KeyboardAction::Composer
    ↓
src/wl/window.rs::dispatch(KeyboardAction) — 分派
    ↓ Composer 分支
composer::input — 追加到 buffer / 移光标 / 触发 debounce 重查
    ↓ 30ms debounce
completion::query(tokens) — 主线程命中内存 LRU 即同步返回
    ↓ 缺缓存 → 异步 worker pool (绝不阻塞 calloop)
       fork: <cmd> --help → 三层解析 → 写缓存 → notify composer (calloop channel)
    ↓
candidates: Vec<Suggestion>  (静态 spec + dynamic hooks 异步合流)
    ↓
render::FrameOverlays { preedit, cursor, selection, completion } — glyph pass 后追加顶点
    ↓ 用户继续敲 / 上下选 / esc / enter / right
    ↓
enter → composer::submit → PtyHandle::write (复用 paste 队列, 处理部分写入背压)
```

### 4.2 模块划分

```
src/
├── composer/                  ← 新增
│   ├── mod.rs                  per-tab CommState + calloop event source
│   ├── tokenizer.rs            shell tokenize (引号/转义/pipe/redirect, ASCII MVP)
│   └── prompt_track.rs         OSC 133 segmented scanner
├── completion/                ← 新增
│   ├── mod.rs                  Suggestion + 异步 Provider trait + LRU cache + worker pool
│   ├── help_indexer.rs         spawn `<cmd> --help`, timeout/output cap/inflight dedup
│   ├── parser.rs               两层: 正则 (clap/argparse/docopt) → 失败 negative cache
│   ├── dynamic_hooks.rs        git/cd/ssh/kill/docker/kubectl/pacman/yay/systemctl 等 ~10 个 (各自异步)
│   └── cache.rs                ~/.cache/quill/completions/<bin_path>-<ver>.json, 7 天 TTL
└── wl/
    └── render.rs              ← 改: FrameOverlays 重构 + CompletionOverlay 顶点
```

**接入点 (现有文件改动, codex 二轮验证后)**:

| 文件 | 改动 |
|---|---|
| `src/wl/keyboard.rs` | 在 `key_press_action()` 内加 composer 判断分支, 新增 `KeyboardAction::Composer{kind: Char/Arrow/Enter/Esc/Tab}` 变体 |
| `src/wl/window.rs` | dispatch `KeyboardAction::Composer{..}` → 调 composer state machine |
| `src/wl/window.rs::pty_read_tick_inner` | PTY read 后, 调 `TermState::advance` 之前, 先过 `composer::prompt_track::scan(bytes)`, 拿到"净化字节 + marker events", 按字节顺序分段 advance |
| `src/term/mod.rs` | 暴露 `display_offset()` / `cursor_visible()` / `is_alt_screen()` 给 composer (可能已有 pub) |
| `src/wl/render.rs` | `draw_frame` 入参重构成 `FrameOverlays` 结构体 (preedit / cursor / selection / completion 四字段), 同步 live + headless 两条路径 |
| `src/tab/mod.rs` | `TabInstance` 加 `composer: composer::State` 字段 |
| `src/lib.rs` | 注册 composer / completion 模块 |
| `Cargo.toml` | 加 serde / serde_json (开 ADR 0013) |

### 4.3 关键数据结构

```rust
// src/completion/mod.rs
pub struct Suggestion {
    pub text: String,
    pub display: String,
    pub description: String,
    pub group: SuggestionGroup,
}

#[async_trait]
pub trait Provider: Send + Sync {
    async fn query(&self, ctx: QueryCtx, gen_id: GenerationId) -> Result<Vec<Suggestion>, ProviderErr>;
    fn cancel(&self, gen_id: GenerationId);  // 当 keystroke 让旧请求过期时
}

// src/composer/mod.rs (per-tab)
pub struct State {
    active: bool,
    buffer: String,
    cursor: usize,
    candidates: Vec<Suggestion>,
    selected: Option<usize>,
    last_query_at: Instant,
    pending_gen: GenerationId,  // 新 keystroke 自增, 旧请求结果到达时按 id 丢弃
}
```

### 4.4 OSC 133 segmented scanner (codex 必改第二项)

**核心**: alacritty vte 不识别 OSC 133。**不能整块扫完一次性 advance** — 同一 read buffer 里 prompt 文本和 OSC marker 混在一起时, marker 后的文本如果跟 marker 前的一起 advance 会拿到错误 cursor row/col。

**正确实现**:

```
fn scan(bytes: &[u8]) -> Vec<Segment>
where Segment = Bytes(&[u8]) | Marker(Osc133Event)
// 状态机: 维护 partial_buf 处理跨 read 分片
// 识别 OSC 终止符: \x07 (BEL) 和 \x1b\\ (ST), 两者都要支持
```

调用层:

```
let segments = composer::prompt_track::scan(read_bytes);
for seg in segments {
    match seg {
        Segment::Bytes(b) => {
            term_state.advance(b);   // 现在 cursor 是上一 marker 后的真实位置
        }
        Segment::Marker(Osc133::PromptStart) => prompt_state.in_prompt = true,
        Segment::Marker(Osc133::InputStart) => {
            prompt_state.input_pos = (term.cursor_row(), term.cursor_col());
        }
        Segment::Marker(Osc133::CommandStart) => prompt_state.in_prompt = false,
        Segment::Marker(Osc133::CommandDone(exit)) => prompt_state.last_exit = exit,
    }
}
```

**zsh 配置 (用户 zshrc)**:
```zsh
precmd()  { print -Pn "\e]133;A\a" }
preexec() { print -Pn "\e]133;C\a" }
# 在 PROMPT 内 input 起点处加 \e]133;B\a (大多 starship/p10k 自动支持)
TRAPEXIT() { print -Pn "\e]133;D;$?\a" }
```

**Fallback (无 OSC 133)**:
- 多条件 AND: `display_offset() == 0 && cursor_visible() && !is_alt_screen() && 当前行末缀匹配 [$%>] $`
- 单条件不可靠, 必须组合
- 不确定时 composer 默认不抢 (保守优于误触发)

## 5. 数据源

### 5.1 三层 `--help` 索引 (codex 必改第三/七项)

**层级 (优先级降序)**:

1. **确定性 parser (覆盖率最高, 最便宜, 永远先试)**:
   - clap 风格: `^\s*(-\w|--\w[\w-]*)(\s+<\w+>)?\s+(.*)$`
   - argparse 风格: 相邻规则
   - docopt 风格: `Usage:` 块
   - 输出 80% 命令的可用 spec

2. **LLM 补强 (parser 输出可疑或缺失时)**:
   - 喂原始 --help + parser 部分输出
   - 要求 strict JSON schema 输出, validate 失败丢弃
   - 客户端可插拔: 远程 (默认 Haiku 4.5 API) / 本地 ollama / 关闭
   - **价格 / 模型名不写进 ADR**, runtime 配置 (`~/.config/quill/completion.toml`)

3. **失败 → negative cache**:
   - 命令存在但 --help 解析全失败 → 标记 `unsupported`, 24h 内不再尝试
   - composer 仍工作 (该命令位置 = 文件名 fallback)

**调用约束 (codex 必改第三项)**:
- timeout: 单次 --help 子进程 cap 2s
- output cap: 256 KB stdout, 超额截断
- inflight 去重: 同一 `<bin_path, version>` 同时只一个 worker
- negative cache: 失败结果也存, 防止反复尝试拖慢 keystroke
- 缓存 key: `<bin_path><mtime>` 而非命令名 (区分 /usr/bin/ls vs /opt/lsd/ls 等同名)

**已知不能处理 (接受)**:
- shell builtin / alias / function (没 --help) — fallback 文件名补全
- 子命令 help (`git checkout --help` vs `git --help`) — 二级查询自动触发
- 慢命令 (--help 自身要联网, 如 `aws --help`) — 命中 timeout 走 negative cache
- 分页器 / 彩色输出 — env `PAGER=cat NO_COLOR=1` 强制

**安全**:
- 任意 `<cmd> --help` 有副作用风险 (理论上 --help 应纯文本输出, 但有 buggy CLI)
- 缓解: 子进程沙盒 (无 stdin, /tmp 工作目录, 限制 fd), 只对 `$PATH` 内可执行 + git/yay/cargo workspace 内的 binary 索引
- 用户级 deny list 配置 (默认空)

### 5.2 Dynamic hooks 异步 Provider (codex 必改第四项)

每个 hook 是 async Provider 实现。**不能同步 spawn 子进程**, 否则 30ms debounce 会重复 fork 阻塞:

```rust
pub trait Provider: Send + Sync {
    async fn query(&self, ctx: QueryCtx, gen_id: GenerationId) -> Result<Vec<Suggestion>, ProviderErr>;
    fn cancel(&self, gen_id: GenerationId);
}
```

每个 hook 实现要做:
- 内部 cache (key: 命令 + 工作目录 + composer state hash, TTL: 1s 到 60s 视命令而定)
- inflight dedup: 同一 cache miss 只发一个 worker
- timeout: 500ms (动态命令应该快, 慢就直接放弃这次 query, 下次 keystroke 再来)
- cancel: 新 gen_id 进来, 旧请求标记取消, 结果到达时丢弃

| 命令 | 数据源 | cache TTL | 备注 |
|---|---|---|---|
| `cd <token>` | 当前目录 readdir | 1s | 本地, 快 |
| `git checkout/switch <token>` | `git branch --list --sort=-committerdate` | 5s | 外部进程 |
| `git add <token>` | `git status --porcelain` | 2s | 外部 |
| `ssh <token>` | parse `~/.ssh/config` Host | 60s | 本地, 文件 |
| `kill <token>` | `ps -eo pid,comm` | 2s | 外部, 快 |
| `docker <run|exec|stop> <token>` | `docker ps` | 5s | 外部, 可能慢 |
| `kubectl get <kind> <token>` | `kubectl get <kind> -o name` | 5s | 外部, 可能很慢 |
| `pacman -S <token>` | `pacman -Slq` | 600s | 外部, 大 |
| `yay -S <token>` | pacman + AUR 索引 | 600s | 外部 |
| `systemctl <verb> <token>` | `systemctl list-units` | 30s | 外部 |

**分类拆 ticket**: T10a 本地轻量 (cd/ssh/readdir 类) vs T10b 外部慢命令 (kubectl/docker/pacman 类)。

## 6. UI 行为

```
> git ch│
        ┌──────────────────────────────────────────────────┐
        │ checkout    Switch branches or restore working   │
        │ cherry      Apply changes from existing commits  │
        │ cherry-pick Apply commits from another branch    │
        │ check-attr  Display gitattributes information    │
        └──────────────────────────────────────────────────┘
```

- 触发: 30ms debounce
- 位置: prompt 光标下方 (优先) / 上方 (空间不够)
- 宽度: max(候选最长, 描述最长, 40 col), cap 80 col
- 滚动: 候选 > 10 行时显前 10
- 选中: 上下键 / Tab / Shift-Tab
- 提交: Enter 选当前并执行 / Right Arrow 接受不退出 (继续敲下个 token) / Esc 退出
- fuzzy: subsequence match (skim 算法 port)

## 7. Ticket 拆分 (codex 必改第五项)

每 ticket <300 行 (CLAUDE.md 约束):

| # | Ticket | 依赖 | 估行 |
|---|---|---|---|
| T1 | **ADR 0013**: inline completion 架构决策 (引入 serde/serde_json/async_trait, calloop 异步约束, 渲染 overlay 模式) | — | 文档 |
| T2 | OSC 133 segmented scanner (`composer/prompt_track.rs`) + 跨分片 + ST 终止 + 单测 | T1 | ~280 |
| T3 | shell tokenizer (ASCII, 引号/转义/pipe/redirect) + 单测 | T1 | ~200 |
| T4 | composer state machine (per-tab, 含 IME/alt-screen gate) + TabInstance 集成 | T2, T3 | ~280 |
| T5a | `wl/keyboard.rs` 加 `KeyboardAction::Composer{..}` 计算分支 | T4 | ~180 |
| T5b | `wl/window.rs` dispatch Composer + 接 pty_read_tick_inner segmented advance | T5a | ~200 |
| T6 | `render.rs` FrameOverlays 重构 (live + headless 双路径同步) + CompletionOverlay 几何 + 顶点生成 | T1 | ~350⚠ |
| T7 | completion async Provider trait + LRU + worker pool + cache 文件 IO | T1 | ~280 |
| T8 | help_indexer worker (子进程沙盒, timeout, output cap, inflight dedup, negative cache) | T7 | ~300 |
| T9a | parser.rs 确定性正则 (clap/argparse/docopt) + JSON schema validate | T8 | ~280 |
| ~~T9b~~ | **撤销 (2026-05-11 用户决定)**: runtime LLM 调用整个路线砍 (隐私 / 网络 / 延迟 / 复杂度全方位负面)。改架构: T12 (将来) build-time 索引, 包管理器 hook + 本地 llama.cpp 子进程 + Qwen3-4B GGUF, 模型仅在新命令首次解析时调一次, runtime 100% 查表 0 模型依赖。spec 表持久化跨机可分发 (类 fig spec 但自动生成)。当前 T9a 三种正则 + heuristic 80% 覆盖率够用 | — | — |
| **T10c** (合并, 2026-05-11 用户二次洞察) | **bootstrap: 启动时扫 $PATH 所有 binary + 并发抓 --help + T9a parser + 写表** — 直接扫包简单粗暴, 不依赖 carapace / zsh _xxx import (那两个是优化路径, 留 T12 之后)。这是 write-through cache 的 lazy-init 阶段, 必须在 T11 串通前完成 | T8, T9a | ~250 |
| T12 (将来) | quill-indexer 子命令 + pacman/apt/dnf/brew post-install hook + llama.cpp 子进程 + **Gemma 4 E2B Q5_K_M GGUF** (路径 `/home/user/gguf_gemma/gemma-4-E2B-it-Q5_K_M.gguf`, 1.8GB, MatFormer 架构, 5090 上 70-90 tok/s, 单命令解析 1-2s) 解析非标准 help, 写持久 spec 表 — 仅当模式 1-4 都未覆盖时调用 | T11 后规划 | TBD |
| T10a | dynamic_hooks 本地组 (cd / ssh config / readdir) | T7 | ~200 |
| T10b | dynamic_hooks 外部命令组 (git / kill / docker / kubectl / pacman / yay / systemctl) | T7 | ~280 |
| T11 | 串通: composer ↔ completion ↔ render 闭环 + fuzzy filter (skim port) + 多 provider 异步合流 + **T7 cache.rs 删 7 天 TTL 改 mtime-only 失效** (spec 表语义不是 cache) | T4-T10 | ~300 |

**注 T6**: 350 行超 CLAUDE.md 300 行约束, 实施时如确实超限拆 T6a (FrameOverlays 重构) + T6b (CompletionOverlay 几何/顶点) 两个。

**总增量预估**: 3,200-4,200 行
**Wall clock**: codex agent 工作 + 用户 review 闭环 ~5 天

可并行 (codex 多 agent):
- T2 / T3 / T6 / T7 互不依赖
- T9a / T10a / T10b 在 T7/T8 后并行
- T9b 在 T9a 后, 可与 T10 并行

## 8. 风险 (codex 二轮补充第六项)

| 风险 | 缓解 |
|---|---|
| OSC 133 用户环境不可靠 | 默认提供 zshrc 一键安装片段 + 多条件 AND fallback |
| composer 拦截破坏现有快捷键 | keyboard.rs 内严格白名单: 字符 + 方向键 + Enter + Esc + Tab; 控制键/修饰键/F 键全透传 |
| popup 渲染层和 PTY grid 冲突 | 复用 PreeditOverlay 模式, glyph pass 后追加顶点, 不开新 wgpu pass; 同步 live + headless |
| 子进程 --help / 动态 hook 阻塞 calloop | 严格 INV-005, 必须 spawn 异步 worker (std thread + channel 或 tokio 局部 runtime) |
| LLM 解析输出非 JSON / 结构幻觉 | strict JSON schema validate + 失败回退确定性 parser + negative cache |
| 远程 LLM 隐私 / prompt injection | --help 内容是公开文本, 风险低; 但可选关闭远程, 用 ollama 或纯正则 |
| LLM API key 管理 / 限流 | runtime 配置文件 + 读 env, 限流时降级到正则 |
| 缓存污染 | 缓存 key 含 binary path + version + checksum, 升级自动失效 |
| 任意 `<cmd> --help` 安全/副作用 | 子进程沙盒 (无 stdin, /tmp cwd, fd 限制), 只索引 $PATH 内可执行, 用户级 deny list |
| TUI 程序 (vim/less/Claude Code) 内误触发 | OSC 133 + alt-screen + cursor visibility 多条件 AND, 默认保守 |
| IME (fcitx5+rime) 与 composer 冲突 | preedit 非空时 composer 暂停, IME commit 后字符进 composer buffer (T4 实现) |
| shell alias / function 与 composer buffer 不一致 | 不解析 alias, 让 shell 自己 expand; composer 只补全可见 token |
| submit 时 quoting 注入 | tokenizer 反向把 buffer escape 回 shell-safe 命令字符串, 防 alias / glob 副作用 |
| draw_frame 9 入参腐烂 | T6 重构成 FrameOverlays 结构体一次性梳理 |
| build LOC 暴增违反 quill "架构读得懂"目标 | T1 ADR 一并更新 CLAUDE.md LOC 目标 5K → 30K (实际已 23K) |
| 单 ticket 超 300 行 | T6 已标 ⚠, 实施时拆 T6a/T6b |

## 9. 不在本设计内

- 替代 PROMPT (PS1) — composer 是 overlay 不抢 grid
- 命令历史搜索 (Ctrl-R) — 沿用 shell
- AI 推荐管道 (`ls -l | <??>`) — 后续 paradigm
- 多语言/复杂 unicode 引号 — ASCII first, 复杂 fallback 不补全
- pipe segment 跨命令上下文推断 (从 awk/jq 推列名) — 第一版只做"pipe 后当新命令"

## 10. 参考

- Mojang Brigadier: `github.com/Mojang/brigadier` MIT — 命令树算法 (本设计不直接 port, LLM 生成扁平 spec 更省事)
- OSC 133: `gitlab.freedesktop.org/Per_Bothner/specifications`
- skim: `github.com/lotabout/skim` — fuzzy match
- MEMORY: `terminal_popup_GUI_paradigm_jump_2026-05-10`
- quill `invariants.md` INV-005: calloop 线程禁止阻塞 IO
- quill `docs/adr/0011-...md` 已用 — 本设计 ADR 编号 0013

## 11. 已无 binary 决策待确认

v3 之前的 Q1 / Q2 (LLM 选型 / CLAUDE.md LOC 目标更新) 已直接写进设计:
- LLM: 三层降级 (正则优先 + LLM 补强 + 失败 negative cache), 客户端运行时配置不入 ADR
- LOC 目标: T1 ADR 0013 一并更新 CLAUDE.md 5K → 30K

可以直接进 T1 (ADR 0013)。
