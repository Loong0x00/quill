<!-- migrated from claude memory: terminal_pty_output_no_auto_scroll_to_bottom_2026-04-26.md on 2026-05-11 -->
---
name: 终端 PTY 输出不该 auto-scroll-to-bottom (用户操作才跳底)
description: 主流终端 (alacritty/foot/kitty/iTerm2/ghostty) 一致行为: 用户在 scrollback 滚上去时, 子进程 PTY 输出**不动 viewport**; 仅用户键盘/粘贴/End 跳底. quill T-0602 误读 alacritty PtyWrite event (那是用户输入路径不是 PTY 输出), 致 pacman -Syu 长输出每行强制跳底, 用户没法看历史
type: feedback
originSessionId: 8250e730-19a5-479b-86d8-29f26fec010b
---
**主流终端约定** (alacritty / foot / kitty / iTerm2 / ghostty / GNOME Terminal):

| 操作 | viewport 行为 |
|---|---|
| 用户键盘类型 (含 repeat) | ✅ 跳到底 (用户能看到自己输入) |
| 用户 paste (Ctrl+Shift+V / 中键 / DnD) | ✅ 跳到底 |
| End / Ctrl+End | ✅ 跳到底 |
| **PTY 子进程输出** (cargo build / pacman -Syu / find /) | ❌ **不动 viewport**, 用户保持滚动位置 |

**反例 (quill T-0602 写错)**:
```rust
pub fn advance(&mut self, bytes: &[u8]) {
    self.processor.advance(&mut self.term, bytes);
    if !bytes.is_empty() {
        self.reset_display();  // ← 错: PTY 输出强制跳底
    }
}
```
T-0602 注释自承误读 alacritty `Event::PtyWrite` (那是用户输入路径, 不是
子进程输出). 实测 `pacman -Syu` 几百行输出每行都把用户拉回底, 完全无法看
历史 — daily-drive 体验毁掉.

**正解 (T-0618 修)**:
```rust
pub fn advance(&mut self, bytes: &[u8]) {
    self.processor.advance(&mut self.term, bytes);
    self.dirty = true;
    // PTY 输出不动 viewport. alacritty Grid 内部新行 push 时自动维护
    // display_offset (ring buffer 加 top, viewport 起点跟着 offset 不变).
}

// 跳底逻辑显式放用户输入路径:
fn write_keyboard_bytes(state: &mut State, bytes: &[u8]) {
    state.tabs_unchecked_mut().active_mut().term_mut().reset_display();  // ← 跳底
    pty.write(bytes);
}
fn paste_read_tick(...) {
    state.tabs_unchecked_mut().active_mut().term_mut().reset_display();  // ← 跳底
    pty.write(&wrapped);
}
```

**为啥 alacritty grid 自动维护 display_offset**:
- Grid 内部 ring buffer, 新行 push 时不重置 display_offset
- 假设 user 已滚 N 行 (display_offset = N), 新行入 buffer top 时 display_offset
  自动维持 N, viewport 起点跟着相对位置不变 → user 视图静止不动
- 这是 alacritty 设计的物理正确性, quill 直接复用即可

**How to apply**:
- 任何终端 emulator 实装 PTY 输出处理: **不要在 advance / write hook 里调
  reset_display 类操作**
- 跳底逻辑显式放在: 键盘 input handler / paste handler / End key handler
- 测试: `seq 1000 | less` 然后 PgUp 滚上去, 在另一 tab 跑 `cargo build`,
  看本 tab viewport 是否仍在 PgUp 位置 (应该是)

**Why 主流这么干**: 终端用户需求 = 看历史时**不被打断**. 子进程输出本身
没"重要性优先"信号 (cron 日志 / shell prompt 重画 / debug print 都一样),
强制跳底 = 把"刚才滚上去看的东西"瞬间冲走 = 用户被迫 PgUp 找回. 信息流
该让用户主导, 不是子进程主导.

跨项目复用: 任何"流式数据 + viewport 滚动" UI 组件 (chat app / log viewer /
监控面板 / IRC client) 都该按"用户操作 jump-to-end / 数据流不动 viewport"
设计. 反例 (chat app 强制跳到最新消息): 用户翻历史时新消息进来被拉回,
体验差到改 issue. quill T-0602 错的就是这个范式.
