# ADR 0006: 引 `xkbcommon` crate 作 Wayland 键盘事件解码器 (T-0501)

## Status

Accepted, 2026-04-25

## Context

T-0501 给 quill 接 Wayland `wl_keyboard` 协议, 让用户在窗口里能真打字到 PTY
(键盘事件 → UTF-8 字节 → `PtyHandle::write` → 子 shell stdin)。**没接键盘前
quill 不能 daily drive** — `cargo run --release` 出窗口但敲键盘没反应,
派单 P0 优先级。

`wl_keyboard` 协议给 client 的不是字符, 而是 **Linux evdev scan code**
(`linux/input-event-codes.h` 里那一套, 例: `KEY_A=30`)。Compositor 单独发
`Keymap` event 把 XKB keymap 文件描述符 (text v1 格式) 推过来, client
自己用 keymap + 当前 modifier 状态 (`Modifiers` event 推 `mods_depressed` /
`mods_latched` / `mods_locked` / `group` 四个 mask) 把 scan code 翻译成
keysym + UTF-8。

这个翻译过程包含:
- Layout (us / fr / cn / dvorak / colemak / ...) 决定哪个 scan code 对应哪个
  keysym
- Modifier composition (Shift+a → A, Ctrl+c → 0x03, AltGr+e → é, dead_acute+e
  → é 通过两键 compose, 等)
- Multi-key sequence (compose key serialization, X11 .XCompose)
- Numpad (NumLock 在/不在时 KP_1 → "1" / "End")

**手写不可行**: keymap 格式自身是 ~5K 行 C-style DSL, X.Org 维护数十年累积
所有 layout / variant / option 编译规则。重写等于 fork X.Org 一个子系统。
唯一现实路径是 **依赖 libxkbcommon** (Wayland 时代 X11 keymap 编译器的标准
拆分: 由 daniels@collabora 主导, KDE / GNOME / Sway / Weston / Hyprland / 几乎
所有 Wayland compositor / 客户端共用).

CLAUDE.md "依赖加新 crate → 必须 ADR" 硬约束触发本 ADR。

## Decision

引 `xkbcommon = "0.8"` 作 dep (非 dev-dep, main 路径用)。**动态链接** libxkbcommon
(Arch/Debian/Fedora 包名 `libxkbcommon` / `libxkbcommon0`, Wayland session 必装),
避免静态链接时 binary 体积膨胀 + 安全更新滞后。

固定调用接口:
```rust
use xkbcommon::xkb;

// 启动期建 Context
let context = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);

// 收到 wl_keyboard::Event::Keymap(format=XkbV1, fd, size) 时
let keymap_str = /* mmap fd, 读 size 字节 utf8 */;
let keymap = xkb::Keymap::new_from_string(
    &context,
    keymap_str,
    xkb::KEYMAP_FORMAT_TEXT_V1,
    xkb::KEYMAP_COMPILE_NO_FLAGS,
).ok_or(...)?;
let state = xkb::State::new(&keymap);

// 收到 wl_keyboard::Event::Modifiers(mods_depressed, mods_latched, mods_locked, group)
state.update_mask(mods_depressed, mods_latched, mods_locked, 0, 0, group);

// 收到 wl_keyboard::Event::Key(key=evdev_code, state=Pressed)
// XKB keycode = evdev keycode + 8 (历史原因: X11 keycode 从 8 起步)
let xkb_keycode = xkb::Keycode::new(evdev_key + 8);
let utf8: String = state.key_get_utf8(xkb_keycode);  // Ctrl+C 直返 "\x03"
let bytes = utf8.into_bytes();
```

**版本**: `xkbcommon = "0.8"` 是 docs.rs 当前列出的版本; API (Context / Keymap
/ State / KeyboardCode / KeymapFormat / KeymapCompileFlags) 自 0.7 起稳定,
0.8 加 `Keycode` newtype 包 u32, 强类型化键码。

**feature**: 默认 features 即可 (无 `wayland` / `x11` 子 feature, 都通用), 不开
optional `memmap2` / `as-raw-xcb-connection` (我们用 `Keymap::new_from_string`
而非 `unsafe new_from_fd`, mmap 自己手做 + read_to_string 后立即 unmap)。

## Alternatives

### Alt 1: 用 SCTK `xkbcommon` feature 走 SCTK `KeyboardHandler`

- 方案: `smithay-client-toolkit = { features = ["xkbcommon"] }` (SCTK 0.19),
  实现 `SeatHandler` + `KeyboardHandler`, SCTK 内部已封 keymap 加载 / modifier
  同步, 直接给 `KeyEvent { utf8, keysym, raw_code }` 字段
- Reject 主因:
  - **类型偷渡**: `KeyboardHandler` trait 把 `KeyEvent` (SCTK 自定义 struct,
    内部 `xkbcommon::xkb::Keysym` 字段) 拉进 quill 公共 Dispatch 路径,
    INV-010 类型隔离要求"上游 crate 类型不出公共 API 边界"。即便 KeyEvent
    是 SCTK 类型、不是 xkbcommon 自身类型, 但 SCTK keyboard 模块的存在
    本质就是 wrapper, 等于在 SCTK 与 xkbcommon 两个上游 crate 同时绑死。
  - **黑盒 modifier 时序**: SCTK 内部如何处理 `Enter` event 携带的 `keys`
    数组 (focus 切回时已按键的 list, "press_key" 不会重发但要算到 modifier)
    + `update_modifiers` 与 `press_key` 的派发顺序, 都是 SCTK private impl,
    Phase 6 加 IME (text-input-v3) 要拦在 keysym 之前做 preedit 处理时,
    SCTK 黑盒拦不住。
  - **repeat 通过 calloop 走 SCTK 的 LoopHandle 内部 timer source**: 与本
    项目的 INV-005 calloop 单线程统一架构耦合度反而更高 (SCTK 的 timer
    source 与我们自己未来 T-0506 加的 timerfd repeat 机制冲突)。
- 备选优势: 代码量少 ~150 行 (SCTK 处理 keymap fd / modifier 同步), 但代价是
  long-term flexibility 损失。

### Alt 2: 手写 keymap parser + 自实现 keysym 表

- 方案: 直接 parse `/usr/share/X11/xkb/symbols/us` 文本, 自己维护 us / fr /
  几个常见 layout 的 scan code → keysym 映射表
- Reject 主因:
  - keymap text 格式是图灵完备 DSL (include / xkb_symbols / xkb_compatibility
    / level3 modifiers / virtual modifiers / interpret rules), 写一个最简
    parser 就 ~3K 行
  - 单个 layout 可写, 但 quill 用户切到非 us layout (用户自己 fcitx5 + cn
    layout) 立刻坏
  - keysym 表 ~2000 entries, X.Org `keysymdef.h` 持续增 (新 emoji / key)
- 技术上不可行, 不是省略 ADR 走的 boring 路径

### Alt 3: 写 `keymap-rs` 纯 Rust 重实现

- 方案: 找/写一个纯 Rust 的 xkbcommon 替代品, 例如 `xkb-rs` (无此 crate) 或
  `keymap-rs` (假设)
- Reject 主因: 没有 production-tested 纯 Rust 替代品。`xkbcommon-rs`
  (本 ADR 选的 crate) 自身就是 libxkbcommon 的 FFI binding, 不是 Pure Rust。
  Pure Rust XKB 实现是未做工作 (一个潜在 Rust ecosystem 项目, 不在 quill scope)
- 备选优势: Rust 内存安全 + 跨编译, 但 Wayland session 都已强依赖 libxkbcommon
  动态库, 链接它没增加运行时 dependency

### Alt 4: 用 X11 keysym 表 + libc readline 风格 Ctrl 处理

- 方案: 干脆走 ASCII-only, Ctrl + 字母 → 字母-0x40 (Ctrl+a → 0x01 等), 不支持
  非 us layout / 死键 / compose
- Reject 主因: quill 目标是 daily driver, 派单 acceptance 明确写 "ASCII shell
  命令 ls / cd / vim 跑通"。但用户机器 layout 可能是 us 但仍需要 AltGr / dead
  key (例: `dead_acute + e → é`)。降级到这个方案后 fcitx5 (Phase 6 T-0504)
  的 `text-input-v3` 接入也要重做底层 modifier 路径 — 现在偷工长期还款

## Consequences

### 正面
- **完整 Linux 键盘协议支持**: 任意 layout (us / cn / fr / de / dvorak /
  colemak / Arabic / 等)、modifier composition、dead key、compose key、numpad
  全套行为来自 libxkbcommon production-tested 实现
- **UTF-8 直出**: `state.key_get_utf8(keycode)` 直返 UTF-8 String — Ctrl+C
  返 `"\x03"`, Shift+a 返 `"A"`, AltGr+e 返 `"é"`, 不需要 quill 二次处理
- **未来 IME 路径打通**: T-0504 fcitx5 + text-input-v3 把 preedit / commit
  字符串拦在 wl_keyboard event 之前时, xkbcommon State 仍是 quill 这侧
  保存的 modifier ground truth, 与 IME 状态分层
- **代码量小**: 本 ticket 估 +250 行 (keyboard.rs + window.rs 改动 + 测试),
  对比 SCTK abstraction 多 ~50 行但换来全程 quill 控制

### 负面 / 代价
- **Cargo.lock 新增 2 transitive crate** (`xkbcommon`, `xkeysym`) — 都是 image-rs
  生态外的专项 crate, xkeysym 是无 dep 的 pure-Rust keysym 表 (X.Org `keysymdef.h`
  Rust port), xkbcommon 是 unsafe FFI binding。审计成本小
- **运行时依赖 libxkbcommon.so**: Wayland 用户必装 (Arch / Debian / Fedora 包
  管自动拉入), 但若 quill 在某些 minimal container 跑 (无 X.Org 数据), 启动期
  `xkb::Context::new` 会失败 — 由 keyboard.rs 的 `KeyboardState::new` 返
  `anyhow::Error` 透传到 main, tracing::error! 后**降级为无键盘 quill**
  (PTY 仍跑, shell 仍出 prompt, 用户用鼠标关窗口)。不是 hard fail
- **`Keymap::new_from_fd` 是 unsafe fn**: 我们绕开走 `new_from_string`
  路径 — wl_keyboard `Keymap` event 给 `OwnedFd`, 用 `mmap` (libc + unsafe
  block 带 SAFETY) 读 size 字节 → utf8 → `new_from_string` (safe)。trade-off:
  自己写 mmap 会有一处 unsafe 块, 但比 `new_from_fd` 的 unsafe 边界更明确
  (mmap 我们 own, fd 我们用完即关; new_from_fd 把 OwnedFd 喂进 xkbcommon
  内部, 它持续 mmap 到 Drop)

### 已知残留 (非本 ADR scope)
- 真键盘 repeat (按住 'a' 不放连续吐 'aaaaa') 不在本 ADR 实装范围。Phase 6
  会接 calloop timerfd source + RepeatInfo (rate / delay) 配置, 单独 ticket。
  本 ADR 仅处理单按一次发一次。
- Compose key (`<Multi_key> <a> <e>` → "æ") xkbcommon 支持但需要 `xkb::compose::Table`,
  本 ADR 不实装 — 单按 + modifier composition 已覆盖 95% 日常场景
- 中文输入 (fcitx5) 走 T-0504 text-input-v3, 与本 ADR 解耦 — IME 在 wl_keyboard
  之前介入 (compositor 先把按键发 IME, IME commit text 通过 zwp_text_input_v3
  发回 client)

## 实装验证

- T-0501 commit 实装本 ADR
- `src/wl/keyboard.rs::KeyboardState` + `handle_key_event` API 落盘
- `src/wl/window.rs` 接 `wl_seat` capabilities + `wl_keyboard` Dispatch
- ≥ 4 个集成单测 ('a' / Ctrl+C / Enter / Backspace) 走真 us keymap (xkbcommon
  内部加载 default layout text)
- 4 门绿 (cargo build / test / clippy / fmt)
- 手测 deliverable: `cargo run --release` 能 `ls` / `cd` / `vim` 走 ASCII shell

## 相关文档

- 派单: `tasks/T-0501-wl-keyboard-xkb.md`
- 主体实装: `src/wl/keyboard.rs` + `src/wl/window.rs::SeatHandler` 段
- 集成测试: `tests/keyboard_event_to_pty.rs`
- 相关 ADR: 0002 (技术栈锁) — xkbcommon 不进 ADR 0002 主干清单, 仅本 ADR 单点
  登记 (与 ADR 0005 image crate 同套路)
- 相关 INV: INV-005 (calloop 单线程禁阻塞) + INV-010 (类型隔离, xkbcommon 类型
  不出 quill 公共 API)
