# quill

极简 Wayland 终端模拟器, Rust 写, 单用户 daily driver.

不是通用终端 — 针对 6K + NVIDIA + Wayland + Claude Code 工作流定制. 作者忍不了现有 Linux 终端的硬伤 (Ghostty memory leak / Foot 没扩展性 / Alacritty 太极简 / Kitty 哲学激进), 所以自己写一个.

## 现状

- ~30K LOC, 491 个测试通过
- Wayland 客户端 (smithay-client-toolkit) + wgpu 渲染 + alacritty_terminal 状态机 + cosmic-text shaping
- IME (text-input-v3) / OSC 133 / fcitx5 / CJK / 半透明 / 圆角 / 多 tab / 滚动选区都已实装
- 内置 inline completion popup (不依赖 readline / fzf-tab / carapace), 见下

## inline completion

替代 zsh 自带补全的方案. 不在 shell 内, 在 terminal 内做. 跟 Minecraft 命令栏 / fish autosuggest 同思路.

**触发**: 用户敲字符 → 200ms debounce → query 多个 Provider 异步 fetch → popup 出来.

**Provider 列表**:
- `PathBinariesProvider` — 命令名 prefix 补全 (扫 `$PATH` cache 60s)
- `HelpIndexerProvider` — flag / subcommand 补全 (`<cmd> --help` subprocess + parser)
- `CdProvider` / `ReaddirProvider` — 路径 / 文件名补全
- `GitBranchProvider` / `GitStatusProvider` / `KillProvider` / `DockerProvider` / `KubectlProvider` / `PacmanProvider` / `SystemctlProvider` — 命令特化动态钩子

**popup 交互** (跟 Minecraft 一致):
| 键 | 行为 |
|---|---|
| Char | 双写 (PTY + composer.buffer), 触发 popup query |
| Backspace / Delete | 双写删字, 不重 sync |
| Up / Down | popup 可见时 cycle 候选 + 同步 buffer/PTY; buffer 空时透传 zsh 翻历史 |
| Tab / Shift+Tab | 第一次按接受当前 selected (第一项), 后续 cycle |
| Enter | popup 可见 + 有 selected → 接受候选 (不执行); 否则透传 zsh 执行 |
| Esc | popup 可见 → 关; 否则透传 zsh |

**Bootstrap**: 启动后 5 秒延迟开始扫 `$PATH`, 并发 4 个 subprocess 抓 `--help`, 解析后写持久 cache (`~/.cache/quill/completions/`). 700+ binary 30-60 秒扫完. cache key 含 binary mtime, binary 升级自动 invalidate.

**OSC 133 必需**: composer 靠 OSC 133 prompt boundary 知道用户在 prompt 里. 用户 zshrc 加 hook:
```zsh
autoload -Uz add-zsh-hook
_quill_osc133_precmd() { print -n $'\e]133;A\a' }
_quill_osc133_preexec() { print -n $'\e]133;C\a' }
add-zsh-hook precmd _quill_osc133_precmd
add-zsh-hook preexec _quill_osc133_preexec
```

**ANSI escape strip**: GNU coreutils 9+ 的 `--help` 输出嵌 OSC 8 hyperlink + ANSI bold (终端鼠标点 `-a` 跳网页 man), parser 必须 strip 才能识别 flag 行. 见 `src/completion/parser.rs::strip_ansi`.

## 已知 issue

- `alias ls=lsd` 之类 zsh alias 不解析 — quill 查的是真 binary `ls` 的 --help, 不会跟着 alias 走
- `pacman -Syu` 等 composite short flag, popup 只显示 `-S` 描述 ("反向 prefix" match), 不拆 `-S/-y/-u` 单独显示
- popup BG quad 用合并 draw workaround — 原本 split draw (popup 在 base_glyph 之后画) 时 popup quad 不画到 fb, 疑似 wgpu pipeline 切换 bind group state 残留. 副作用: popup 区下方若有终端字符会浮在 popup 上 (实测 popup 弹空 cell 行不暴露)

## 开发

```bash
cargo build --release
cargo test
cargo run --release   # NVIDIA 5090 自动选 Vulkan, 无需 env
```

设计文档 `docs/notes/inline_completion_design.md`. ADR `docs/adr/0013-inline-completion-composer.md`.

## License

不打算开 license, 个人自用. 看到了想用就用, 不接 issue / PR.
