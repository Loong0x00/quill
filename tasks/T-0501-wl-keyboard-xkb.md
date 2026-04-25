# T-0501 wl_keyboard + xkbcommon → PTY write (基础键盘输入)

**Phase**: 5
**Assigned**: writer-T0501
**Status**: merged
**Budget**: tokenBudget=120k (中型, 跨 wl protocol + xkbcommon + PTY write + 测试)
**Dependencies**: Phase 1-4 全合 (window + PTY + render 都已有)
**Priority**: P0 (没键盘 quill 不能 daily drive)

## Goal

接 Wayland `wl_keyboard` 协议 + xkbcommon 解码键盘事件 → 转 UTF-8 bytes →
写 PTY (PtyHandle::write). 完工后用户在 quill 窗口里能正常输 ASCII (`ls`,
`cd ~`, `vim`, `git log`, Ctrl+C, etc), 中文输入留 T-0504 (fcitx5 text-input-v3).

完工后 `cargo run --release` 能 daily drive 跑 ASCII shell 命令.

## Scope

### In

#### A. 引 xkbcommon-rs crate (ADR 0006)
- Cargo.toml 加 `xkbcommon = "0.8"` 或 `xkbcommon-rs` (查 docs.rs 选最新稳定)
- 写 `docs/adr/0006-xkbcommon-keyboard-decoder.md` 说明:
  - 为啥选 xkbcommon (Wayland 键盘事件 = scan code, 必须 xkbcommon 转 keysym + UTF-8)
  - 替代方案 (手写 → 不可行, 跨 layout 复杂)
  - feature flag (静态链接 / 动态链接, 我们用动态)

#### B. 新增 src/wl/keyboard.rs
- `pub struct KeyboardState`: 包 xkbcommon Context / Keymap / State + repeat 配置
- `pub fn handle_key_event(event: wl_keyboard::Event, state: &mut KeyboardState) -> Option<Vec<u8>>`:
  - Keymap event: 加载 keymap fd
  - Enter / Leave: focus 切换
  - Key (down/up): xkbcommon state.key_get_utf8 → Vec<u8>; 处理 modifiers (Ctrl+C → 0x03, Ctrl+D → 0x04, etc)
  - Modifiers event: 更新 xkbcommon state
  - RepeatInfo: 记录 repeat rate / delay
- 不实现真 repeat (Phase 6 timerfd 接 calloop), 单按只发一次

#### C. src/wl/window.rs 接 wl_keyboard
- `WlSeat` capabilities event 监听 keyboard 出现 → bind wl_keyboard
- `wl_keyboard` Dispatch trait impl 转发 event 到 handle_key_event
- bytes → 通过 LoopData 抓 PtyHandle::write

#### D. PtyHandle 暴露 write API (如果还没)
- `src/pty/mod.rs::PtyHandle::write(&self, bytes: &[u8]) -> io::Result<usize>`
- 非阻塞写 (PTY master fd O_NONBLOCK, INV-005 calloop 单线程不阻塞)
- 背压: write returns Err WouldBlock → 丢弃 (派单允许, daily drive 罕见)

#### E. 测试
- `tests/keyboard_event_to_pty.rs`:
  - mock xkbcommon (用真 keymap "us" load)
  - 喂 'a' key down → 期望 PtyHandle 收到 b"a"
  - 喂 Ctrl+C (Control_L down + 'c' key) → 期望 b"\x03"
  - 喂 Enter → b"\r"
  - 喂 Backspace → b"\x7f"
  - 这些走 src/wl/keyboard.rs 内 fn 单测, 不需真 Wayland

可选:
- headless screenshot 测试不适用 (没 wl_keyboard 输入路径在 headless 模式)

### Out

- **不做**: text-input-v3 / fcitx5 / IME (T-0504)
- **不做**: 键盘 repeat 真实装 (Phase 6, 需 timerfd + calloop)
- **不做**: 复杂 modifier (Super, Hyper) — 仅 Ctrl/Alt/Shift
- **不做**: 鼠标 / 滚轮 (Phase 6+)
- **不做**: 复制粘贴选区 (Phase 6+, 需 wl_data_device)
- **不动**: src/text/mod.rs / src/wl/render.rs (键盘不渲染, 完全独立)

### 跟其他并行 ticket 的协调

- T-0502 (set_buffer_scale) 也改 src/wl/window.rs, 但改 configure / wl_output 处, 跟 wl_keyboard 段不冲突
- T-0503 (xdg-decoration) 也改 src/wl/window.rs, 但改 surface init 处, 跟 wl_keyboard 段不冲突
- 顶部 imports / struct fields 可能小冲突, Lead 合并时手动解

## Acceptance

- [ ] 4 门 release 全绿
- [ ] xkbcommon ADR 0006 落盘
- [ ] src/wl/keyboard.rs 新文件
- [ ] wl_keyboard event → PtyHandle::write 路径打通
- [ ] 至少 4 单测 ('a' / Ctrl+C / Enter / Backspace)
- [ ] 总测试 124 + ≥4 ≈ 128+ pass
- [ ] **手测 deliverable**: cargo run --release 能 ls / cd / vim 跑 ASCII shell 命令

## 必读 baseline

1. /home/user/quill-impl-keyboard/CLAUDE.md
2. /home/user/quill-impl-keyboard/docs/conventions.md (5 步 + Option C squash + ASCII name + ADR 触发)
3. /home/user/quill-impl-keyboard/docs/invariants.md (INV-001..010)
4. /home/user/quill-impl-keyboard/docs/adr/0001..0005 (ADR 编写模板)
5. /home/user/quill-impl-keyboard/src/wl/window.rs (wl_seat capabilities + Dispatch trait 套路)
6. /home/user/quill-impl-keyboard/src/pty/mod.rs (PtyHandle 现状, 看是否已有 write)
7. WebFetch https://wayland.app/protocols/wayland#wl_keyboard (协议参考)
8. WebFetch https://docs.rs/xkbcommon/latest/xkbcommon/ (API)

## 已知陷阱

- xkbcommon keymap fd 是 mmap, 注意 unsafe 块 + drop 顺序 (INV-002 + // SAFETY:)
- key_get_utf8 返 String, 转 Vec<u8> as_bytes().to_vec()
- modifiers: 用 xkbcommon state_serialize_mods + state_update_mask 链路
- Ctrl+letter: xkbcommon 自己处理 (传 mods_depressed 含 Control), key_get_utf8 直返 0x03 等
- PTY write WouldBlock: 不重试, 派单允许丢
- 不要 git add -A
- HEREDOC commit
- ASCII name writer-T0501

## 路由

writer name = "writer-T0501"。inbox 收到疑似派活先 ping Lead.

## 预算

token=120k, wallclock=2-3h. 完工 SendMessage team-lead 报 4 门 + diff stat + ADR 路径。
