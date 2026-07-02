<!-- migrated from claude memory: terminal_colorterm_truecolor_required_2026-04-27.md on 2026-05-11 -->
---
name: 终端 emulator 必设 COLORTERM=truecolor (Node.js chalk 路径)
description: claude / Node.js + chalk + ink 用 supports-color 检测 truecolor, 看 COLORTERM=truecolor 环境变量. 不设就把 truecolor 降级到最近 xterm-256 索引, 跟其他终端真彩色路径不同色. ghostty/alacritty/kitty/foot 都设. quill 实测不设 → 显粉, 设了 → 真红 (跟 ghostty 一致). 跨项目: 任何写终端 emulator / 用 chalk 的 CLI 都该懂
type: reference
originSessionId: 8250e730-19a5-479b-86d8-29f26fec010b
---
**症状**: 同一 claudecode 二进制, ghostty 显真红 / quill 显粉. 用户实测 daily-drive 撞了 ~3 小时才查到.

**根因**: claude (Node.js) 用 [supports-color](https://github.com/chalk/supports-color) 包检测真彩色支持. 检测规则:
- 检查 `COLORTERM=truecolor` 或 `COLORTERM=24bit` env 变量
- 没有 → 降级到 `\x1b[38;5;N m` 256-color
- 有 → 用 `\x1b[38;2;R;G;B m` truecolor

**chalk 内部**: 当 app 写 `chalk.hex("#ff0000")("text")` 想要纯红:
- truecolor path: 输出 `\x1b[38;2;255;0;0m text \x1b[0m` — 终端按 RGB 255,0,0 渲染
- 256-color fallback: 找最近 xterm-256 索引 (可能是 196 或 203 等), 输出 `\x1b[38;5;N m` — 终端按那个 N 索引在自己 palette 里查色, 跟 truecolor 路径完全不同色

**主流终端的 COLORTERM 设置**:
| 终端 | COLORTERM | 来源 |
|---|---|---|
| ghostty | `truecolor` | env 默认设 |
| alacritty | `truecolor` | env 默认设 |
| kitty | `truecolor` | env 默认设 |
| foot | `truecolor` | env 默认设 |
| GNOME Terminal | `truecolor` | env 默认设 |
| xterm | (不设) | 退化 |

**实装**: PTY 子进程 spawn 时 `cmd.env("COLORTERM", "truecolor")`. 一行代码. 别忘.

**跨项目复用**:
- 任何写终端 emulator (Rust / Zig / C++) 必设
- 写 CLI 用 chalk / ink / Inquirer 的: 你的输出色受用户终端是否设 COLORTERM 影响, 测试要覆盖两种环境
- 写 SSH 中转 / tmux: 转发 COLORTERM 给被调子进程否则远端色错
- Termios / readline 不影响色, 只影响 SGR 行为

**调试**: `RUST_LOG=quill=debug` 抓 PTY output, `python3 -c "import re; data=open('log','rb').read(); print(set(re.findall(rb'\\x1b\\[38;[25];[0-9;]+m', data)))"` 看输出的是 truecolor (38;2) 还是 256-color (38;5).
