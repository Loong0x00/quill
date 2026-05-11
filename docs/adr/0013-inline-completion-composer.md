# ADR 0013 — Inline Completion (Composer Mode) 架构

**Status**: Accepted
**Date**: 2026-05-11
**Phase**: 9 (inline completion)
**Related**: INV-005 (calloop 异步约束), OSC 133 规范, design doc `/home/user/quill/docs/notes/inline_completion_design.md`

## Context

### 用户需求

用户原话:

> "输入比如 git 然后 tab 出现候选但是按空格后不会更新候选,能理解么。我希望要的是 minecraft 的那种提示候选逻辑"
>
> "我想要的就只是 [Claude Code/Codex 截图] 这样,没有那么多复杂的功能。输入命令然后显示可以的输入用来替代 --help。以及敲完整命令"
>
> "嗯难道这不是一次性全部实现的么,毕竟嗯所有的工具几乎都支持各种形态的 help 难道不能自动索引么?自动从工具那里搞来表然后。就自动化"

核心:实时 keystroke 触发 + popup 浮窗显示候选 + 候选带描述 + 空格后自动切下一个 token + 数据 100% 自动化(`--help` 抓取)。

### 已尝试方案与失败根因

| 方案 | 结果 |
|---|---|
| zsh `menu select` | 一次 tab 只做 prefix 扩展,二次才进 menu;候选无描述;空格后不更新 |
| `LISTMAX=1000` | 解决阻断,但显示仍占行 grid |
| `fzf-tab` | quill PTY 下卡死 + readline 架构无法解决空格后不更新 |
| `carapace-bin` | 数据层 OK,显示层依然受 zsh readline 限制 |

**根本问题**:readline 是离散事件触发(按 tab 才查),不是实时键流。要 Minecraft/Claude Code 那种体验,必须由 quill 自身拥有 prompt 输入层。任何依赖 readline 的方案都触及同一架构天花板。

## Decision

采用 **composer 模式**:quill overlay 接管 prompt 行 keystroke,实现实时候选浮窗。

### 三个接入点

| 文件 | 改动 |
|---|---|
| `src/wl/keyboard.rs::key_press_action()` | 新增 `KeyboardAction::Composer{kind}` 变体(Char/Arrow/Enter/Esc/Tab),在此层判断:composer active && shell prompt && IME idle && !alt-screen |
| `src/wl/window.rs::dispatch()` | 分派 `KeyboardAction::Composer{..}` → 调 composer state machine |
| `src/wl/window.rs::pty_read_tick_inner` | PTY read 后,`TermState::advance` 之前,先过 `composer::prompt_track::scan(bytes)`,拿到"净化字节 + marker events",按字节顺序分段 advance |

### 数据源:三层降级

1. **确定性 parser(覆盖率最高,最便宜,永远先试)**
   - clap 风格:`^\s*(-\w|--\w[\w-]*)(\s+<\w+>)?\s+(.*)$`
   - argparse/docopt 风格:相邻规则 + `Usage:` 块
   - 覆盖约 80% 命令的可用 spec

2. **LLM 补强(parser 输出可疑或缺失时)**
   - 喂原始 `--help` + parser 部分输出,要求 strict JSON schema 输出
   - validate 失败丢弃,客户端可插拔(远程 / 本地 ollama / 关闭)
   - 具体模型与价格不写入 ADR,走 runtime 配置(`~/.config/quill/completion.toml`)

3. **失败 → negative cache**
   - 命令存在但 `--help` 解析全失败 → 标记 `unsupported`,24h 内不再尝试
   - composer 仍工作(该命令位置退化到文件名 fallback)

### Dynamic hooks

async Provider trait + worker pool,约 10 个特例:

| 命令 | 数据源 | cache TTL |
|---|---|---|
| `cd <token>` | 当前目录 readdir | 1s |
| `git checkout/switch <token>` | `git branch --list --sort=-committerdate` | 5s |
| `git add <token>` | `git status --porcelain` | 2s |
| `ssh <token>` | parse `~/.ssh/config` Host | 60s |
| `kill <token>` | `ps -eo pid,comm` | 2s |
| `docker <run\|exec\|stop> <token>` | `docker ps` | 5s |
| `kubectl get <kind> <token>` | `kubectl get <kind> -o name` | 5s |
| `pacman -S <token>` | `pacman -Slq` | 600s |
| `yay -S <token>` | pacman + AUR 索引 | 600s |
| `systemctl <verb> <token>` | `systemctl list-units` | 30s |

每个 hook 实现必须:内部 cache + inflight dedup + 500ms timeout + cancel(新 gen_id 进来时旧请求标记取消)。

### 渲染

`draw_frame` 入参重构为 `FrameOverlays` 结构体:

```rust
pub struct FrameOverlays {
    pub preedit: Option<PreeditOverlay>,
    pub cursor: Option<CursorOverlay>,
    pub selection: Option<SelectionOverlay>,
    pub completion: Option<CompletionOverlay>,
}
```

复用 PreeditOverlay 模式,glyph pass 后追加顶点,**不开新 wgpu pass**。live + headless 两条路径同步更新。

### OSC 133 Segmented Scanner

alacritty vte 不识别 OSC 133。不能整块扫完一次性 advance — 同一 read buffer 里 prompt 文本和 OSC marker 混在一起时,marker 后的文本如果跟 marker 前的一起 advance 会拿到错误 cursor row/col。

正确实现:状态机识别 `\x07`(BEL)和 `\x1b\\`(ST)两种 OSC 终止符,处理跨 read 分片,输出 `Vec<Segment>` 逐段喂给 `term_state.advance()`。

**zsh 配置(用户 zshrc 推荐)**:
```zsh
precmd()  { print -Pn "\e]133;A\a" }
preexec() { print -Pn "\e]133;C\a" }
TRAPEXIT() { print -Pn "\e]133;D;$?\a" }
# PROMPT 内 input 起点处加 \e]133;B\a (starship/p10k 大多自动支持)
```

**Fallback(无 OSC 133)**:多条件 AND — `display_offset() == 0 && cursor_visible() && !is_alt_screen() && 当前行末缀匹配 [$%>]\ $`。单条件不可靠,不确定时 composer 默认不抢(保守优于误触发)。

### Per-tab 状态

`TabInstance` 加 `composer: composer::State` 字段:

```rust
pub struct State {
    active: bool,
    buffer: String,
    cursor: usize,
    candidates: Vec<Suggestion>,
    selected: Option<usize>,
    last_query_at: Instant,
    pending_gen: GenerationId,
}
```

### 异步约束

严格遵守 **INV-005**:calloop 线程禁止阻塞 IO。所有 `--help` 子进程、动态 hook、LLM 调用一律走 worker pool(std thread + channel),calloop 线程纯逻辑。

### `--help` 子进程安全约束

- timeout: 单次 2s
- output cap: 256 KB stdout,超额截断
- inflight 去重:`<bin_path, mtime>` 同时只一个 worker
- 缓存 key:`<bin_path><mtime>`,区分同名不同路径的 binary
- 子进程沙盒:无 stdin,`/tmp` 工作目录,限制 fd
- 只索引 `$PATH` 内可执行 + git/cargo workspace 内 binary
- env 注入:`PAGER=cat NO_COLOR=1`,防分页器/彩色输出干扰解析

### 模块划分

```
src/
├── composer/
│   ├── mod.rs          per-tab State + calloop event source
│   ├── tokenizer.rs    shell tokenize (引号/转义/pipe/redirect, ASCII MVP)
│   └── prompt_track.rs OSC 133 segmented scanner
├── completion/
│   ├── mod.rs          Suggestion + async Provider trait + LRU cache + worker pool
│   ├── help_indexer.rs spawn <cmd> --help, timeout/output cap/inflight dedup
│   ├── parser.rs       三层:正则 → LLM 补强 → negative cache
│   ├── llm_client.rs   可插拔客户端(远程 / ollama / 关闭)
│   ├── dynamic_hooks.rs ~10 个异步 hook
│   └── cache.rs        ~/.cache/quill/completions/<bin_path>-<ver>.json, 7 天 TTL
└── wl/
    └── render.rs       FrameOverlays 重构 + CompletionOverlay 顶点
```

## 新增 Crates

| Crate | 用途 |
|---|---|
| `serde` | `--help` 解析输出 + 缓存文件序列化 |
| `serde_json` | JSON schema validate (LLM 补强层输出) + 缓存文件格式 |
| `async_trait` | Provider trait 异步方法(Rust stable 不支持 async fn in trait) |
| ollama / anthropic-sdk(可选) | 具体 LLM 客户端,**不锁入 ADR**,runtime 配置决定 |

## CLAUDE.md 同步

本 ADR 合入时同步更新 `CLAUDE.md`:

- 第 27 行 `3-5K LOC 左右` → `~30K LOC 左右`(项目已达 23K,inline composer 预计增 3-4K)
- 目标列表加一条:`inline composer + popup 候选不依赖 readline`

## Alternatives Considered

### A. 沿用 readline(zsh / fzf-tab / carapace)

已全部尝试。根本架构限制:readline 离散事件触发,空格后不更新候选,无法实现实时键流体验。任何在 readline 层面的修补都无法绕过这个天花板。**Rejected。**

### B. 替换 PS1,让 quill 全管 prompt

quill 直接渲染 prompt,完全绕开 shell 的 readline。

- ✓ 完全控制 prompt 行
- ✗ 破坏用户现有 shell 主题(starship / p10k / oh-my-zsh),迁移成本极高
- ✗ shell alias/function/history 跨进程管理复杂度爆炸
- ✗ 风险高于收益

**推迟,不在本 ADR 范围。**

### C. 手写 fig 风格 spec(700+ 命令)

由人工或 AI 批量生成每个命令的 YAML/JSON spec 文件(类似 Fig/Warp 做法)。

- ✓ 确定性最高
- ✗ 维护成本爆炸:CLI 每次版本升级都要同步更新 spec
- ✗ LLM 时代,`--help` 自动索引 + LLM 补强是更可持续的路径
- ✗ 700+ 命令的初始建库本身就是高成本一次性工作

**Rejected。**

## Implementation Tickets

见 design doc §7 ticket 表(T1-T11),不在本 ADR 重复列举。总增量预估 3,200-4,200 行。

可并行的 ticket 组:
- T2 / T3 / T6 / T7 互不依赖(OSC scanner / tokenizer / render overlay / completion core)
- T9a / T10a / T10b 在 T7/T8 后并行(parser + dynamic hooks)

## Consequences

### 正面
- 解决 readline 不能实时更新的根本架构问题,用户体验对齐 Minecraft/Claude Code 标准
- 数据 100% 自动化(`--help` 抓取),无手写 spec 维护成本,CLI 升级自动失效重建缓存
- OSC 133 segmented scanner 同时改善 prompt 边界检测的准确性,为后续 prompt 主题集成打基础

### 负面
- 增加约 3-4K LOC,跨 5+ 文件接入点,需要协调 IME / alt-screen / OSC 133 多重终端状态
- 子进程沙盒和 LLM 调用引入新攻击面:`--help` 子进程须严格隔离;LLM 远程调用可关闭
- `draw_frame` 签名重构(FrameOverlays)影响 live + headless 两条渲染路径,需同步更新

### 中性
- 用户 zshrc 推荐配 OSC 133 snippet,但不强制;fallback heuristic 兜底保证无 OSC 133 环境也可用
- `async_trait` crate 在 Rust 的 async fn in trait 稳定前是必要依赖,稳定后可移除
- composer 模式对 shell 透明:shell 仍跑,composer 是 overlay 不替换 shell prompt

## References

- design doc: `/home/user/quill/docs/notes/inline_completion_design.md`
- `docs/invariants.md` INV-005: calloop 线程禁止阻塞 IO
- OSC 133 规范: `gitlab.freedesktop.org/Per_Bothner/specifications`(VTE / iTerm2 / kitty shell integration)
- ADR 0004: calloop 统一事件循环
