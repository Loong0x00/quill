# T-0603 keyboard repeat (长按重发字节)

**Phase**: 6
**Assigned**: writer-T0603
**Status**: in-review
**Budget**: tokenBudget=150k (timerfd + calloop 集成 + repeat 状态机)
**Dependencies**: T-0501 (wl_keyboard + RepeatInfo 已记录 rate/delay)
**Priority**: P1 (user 实测反馈长按 backspace 不连续, daily-drive 必需)

## Goal

接 Wayland `wl_keyboard.repeat_info` event (T-0501 已记录 rate/delay) +
calloop timerfd → 按键 Pressed 时 schedule timer (delay ms 后首次 fire,
之后每 rate ms 重 fire 一次) → 每次 fire 重发同字节给 PTY → Released
时 cancel timer. 完工后用户长按 backspace / 字母 / 方向键能连续输入 (跟
alacritty / foot 一致).

## Scope

### In

#### A. src/wl/keyboard.rs 加 repeat 状态
- `KeyboardState` 加 current_repeat: Option<RepeatKey { keycode, bytes, last_fire: Instant }>
- `RepeatInfo` event 已 store rate (ms 间隔) + delay (ms 首次), T-0501 已做
- handle_key_event Pressed 时返 KeyboardAction 加新 variant `StartRepeat { keycode, bytes }`
- handle_key_event Released 时返 `StopRepeat { keycode }` (仅当 keycode 匹配 current_repeat)
- 新 fn `tick_repeat(state: &mut KeyboardState, now: Instant) -> Option<Vec<u8>>` — 主循环 timerfd fire 时调, 检查是否到 fire 时机, 返 bytes 让调用方 PTY write

#### B. src/main.rs / src/wl/window.rs 接 timerfd
- 加 timerfd 注册 calloop (跟 PTY fd 同 EventLoop, INV-005)
- timerfd 配置: 初始 delay (e.g. 500ms), 之后 interval (e.g. 30ms = ~33 Hz)
- StartRepeat 时 timerfd_settime 设 timer
- StopRepeat 时 timerfd_settime 关 timer
- timerfd readable callback: read 8 字节计数 → tick_repeat 取 bytes → PtyHandle::write

#### C. modifier release 处理
- 按 'a' (start repeat) → 按 Shift (modifier 变化) → 应该 cancel 'a' repeat (alacritty 行为)
- 任何 modifier 变化都 cancel 当前 repeat — 简化路径

#### D. 测试
- src/wl/keyboard.rs lib 测试 StartRepeat / StopRepeat / tick_repeat 决策
- src/wl/keyboard.rs lib 测试 modifier 变化 cancel repeat
- tests/keyboard_repeat_e2e.rs 集成测试 (mock timerfd, fire 模拟, 验 bytes 累积)

#### E. 不做闪烁
- 派单 In #A: timerfd 仅用于 keyboard repeat, 不接 cursor 闪烁 (cursor 闪烁 Phase 6+ 单独 ticket)

### Out

- **不做**: cursor 闪烁 (Phase 6+, 独立 timerfd)
- **不做**: 自动 polling fcitx5 状态 (Phase 6+)
- **不做**: 复杂 repeat 加速 / 减速 (匀速 repeat 即可)
- **不做**: 配置文件读 rate/delay (用 wl_keyboard.repeat_info 给的值即可)
- **不动**: src/text / src/pty / src/wl/render.rs / src/wl/pointer.rs / docs/invariants.md
- **不引新 crate** (calloop 已有 timerfd 支持)

### 跟其他并行 ticket 的协调

- T-0601 (cursor) 改 src/wl/render.rs, 不冲突
- T-0602 (scrollback) 改 src/wl/keyboard.rs PageUp/PageDown 段, 跟 repeat 段不冲突 (新加字段 current_repeat)
- 顶部 imports 可能小冲突, Lead 合并时手解

## Acceptance

- [ ] 4 门 release 全绿
- [ ] 长按字母 'a' 连续输出 'aaaa...' (delay 500ms 后开始, ~33 Hz)
- [ ] 长按 backspace 连续删
- [ ] 长按方向键连续移
- [ ] 释放时停 (modifier 变化也停)
- [ ] 总测试 199 + ≥4 ≈ 203+ pass
- [ ] **手测 deliverable**: cargo run --release, 在 vim / shell 里长按 backspace / 字母 / 方向键, 连续输入正常
- [ ] 审码放行

## 必读 baseline

1. /home/user/quill-impl-repeat/CLAUDE.md
2. /home/user/quill-impl-repeat/docs/conventions.md
3. /home/user/quill-impl-repeat/docs/invariants.md (INV-005 calloop 单线程)
4. /home/user/quill-impl-repeat/tasks/T-0603-keyboard-repeat.md
5. /home/user/quill-impl-repeat/docs/audit/2026-04-25-T-0501-review.md (KeyboardState + handle_key_event)
6. /home/user/quill-impl-repeat/docs/adr/0006-xkbcommon-keyboard-decoder.md (xkbcommon 模型)
7. /home/user/quill-impl-repeat/src/wl/keyboard.rs (KeyboardState 现状)
8. /home/user/quill-impl-repeat/src/wl/window.rs (calloop 集成示例)
9. /home/user/quill-impl-repeat/src/main.rs (calloop EventLoop)
10. WebFetch https://docs.rs/calloop/latest/calloop/timer/ (calloop timer source)

## 已知陷阱

- timerfd 是 Linux 专用 syscall (跨平台用 calloop::timer::Timer 抽象更稳)
- 实际推荐 calloop::timer::Timer, 不用 raw timerfd
- read timerfd 必须读 8 字节 (u64 expiry count), 否则 epoll 一直唤醒
- repeat 的字节是按下时 xkbcommon 算出的 UTF-8, modifier 变化时无效要 cancel
- 不要 git add -A
- HEREDOC commit
- ASCII name writer-T0603

## 路由

writer name = "writer-T0603"。

## 预算

token=150k, wallclock=2-3h.
