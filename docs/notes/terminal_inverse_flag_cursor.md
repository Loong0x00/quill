<!-- migrated from claude memory: terminal_inverse_flag_critical_for_cursor_2026-04-27.md on 2026-05-11 -->
---
name: 终端 emulator 必解析 INVERSE flag (SGR 7) — TUI cursor 可见性
description: claudecode (Ink) / vim / less 等 TUI 用 SGR 7 (反相) 画 cursor / selection / highlight cell, 终端 emulator 必解析 cell.flags.INVERSE 自动 swap fg/bg, 否则 cursor 不可见 (cell 看着跟普通 cell 一模一样). 实测 claude binary 55 处使用. quill T-0618 follow-up part 6 修
type: reference
originSessionId: 8250e730-19a5-479b-86d8-29f26fec010b
---
**症状**: claudecode 在 quill 看不到 input cursor. 在 ghostty / alacritty / foot 都正常. 排查 `\x1b[?25l/h` (DECTCEM 显隐) 没问题 (cursor mode 是 ON), cursor pos 也对.

**根因**: claudecode (Ink, React for terminals) 用 **SGR 7 反相** 画 cursor:
1. 不发 DECSCUSR / 不依赖系统 cursor 形状
2. 在 cursor 位置直接给 cell 加 INVERSE flag (SGR 7 on, 写 char, SGR 7 off)
3. 终端解析 INVERSE → swap fg/bg → cursor 位置 cell 反白 → 视觉上是个反色方块

**alacritty_terminal Flags** (`alacritty_terminal::term::cell::Flags`):
```
const INVERSE  = 0b...0001;  // SGR 7 (反相)
const BOLD     = ...;        // SGR 1
const ITALIC   = ...;        // SGR 3
const UNDERLINE = ...;       // SGR 4
const HIDDEN   = ...;        // SGR 8 (passwords)
```

**修法 (quill T-0618 follow-up)**: `CellsIter::next` 在 fg/bg 解析后检查 INVERSE flag, swap:
```rust
if indexed.cell.flags.contains(Flags::INVERSE) {
    std::mem::swap(&mut fg, &mut bg);
}
```

**实测验证**: 抓 claude binary `python3 -c "import re; print(len(re.findall(rb'\x1b\[7m', open('claude').read())))"` → 55 处使用 SGR 7. 修了之后 cursor 立即可见.

**跨项目复用**:
- 任何写终端 emulator: INVERSE 是 ANSI SGR 必实装的 5 个 flag 之一 (BOLD/ITALIC/UNDERLINE/INVERSE/HIDDEN). 别只画 fg/bg color 跳过 flags
- HIDDEN (SGR 8) 也别忘 — passwords 输入框靠它隐字符
- BOLD 影响 glyph 字重 (cosmic-text 重 shape)
- ITALIC 影响 glyph 倾斜 (同上)
- UNDERLINE 在 cell 底部加横线

**反例**: quill 之前只读 fg/bg, 完全忽略 flags 字段. 一年没写 TUI 没人撞 cursor 不可见这个 case (普通 shell 用 DECSCUSR 走系统 cursor 路径, 看不到 INVERSE 缺失). 撞 claudecode 才暴露.

**架构**: 在 iterator 处解析 (CellRef 不暴露 flags), 渲染层无感. 不用改 build_vertex_bytes / glyph pass. INV-010 类型隔离 + 单一来源决策.
