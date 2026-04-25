//! Wayland `wl_keyboard` 事件 → UTF-8 字节解码 (T-0501)。
//!
//! 职责: 把 compositor 推过来的 evdev scan code + modifier mask 翻译成
//! UTF-8 字符串, 再转 `Vec<u8>` 给 `PtyHandle::write` 喂子 shell stdin。
//! 翻译核心走 [`xkbcommon`] FFI (libxkbcommon), 详见 ADR 0006
//! `docs/adr/0006-xkbcommon-keyboard-decoder.md`。
//!
//! ## 模块边界 (INV-010 类型隔离)
//!
//! 对外只暴露:
//! - [`KeyboardState`] — quill 自有 struct, 内部包 `xkbcommon::xkb` 三件套
//!   (Context / Keymap / State), 但**字段全私有**, 下游构造不出来。
//! - [`handle_key_event`] — 接 `wl_keyboard::Event` (raw wayland-client 协议
//!   类型, 已在 quill 公共 API 边界 — `wl/window.rs::Dispatch<WlKeyboard>`
//!   接同样的类型), 返 `Option<Vec<u8>>` (普通 Rust 类型)。
//! - 没有 `pub use xkbcommon::*;` re-export (违反 INV-010)。
//!
//! ## 协议状态机概览
//!
//! ```text
//! wl_seat capabilities → 含 Keyboard
//!   └→ get_keyboard(qh, ()) → WlKeyboard
//!         │
//!         ├→ Event::Keymap(format=XkbV1, fd, size) → load keymap → State::new
//!         ├→ Event::Enter(serial, surface, keys[])  → focus on, 当前持有 keys
//!         ├→ Event::Leave(serial, surface)          → focus off, drop 待发字节
//!         ├→ Event::Key(serial, time, key, state)   → press/release → utf8
//!         ├→ Event::Modifiers(...)                  → State::update_mask
//!         └→ Event::RepeatInfo(rate, delay)         → 仅记录, 不真 repeat
//! ```
//!
//! ## 键盘 repeat (T-0603)
//!
//! T-0501 阶段仅记录 `RepeatInfo` (rate / delay), Phase 6 留 timerfd 接入口。
//! T-0603 真接 calloop `Timer` source: Pressed 时 [`handle_key_event`] 返
//! [`KeyboardAction::StartRepeat`], 调用方 (`wl/window.rs`) schedule 一个
//! `Timer::from_duration(delay_ms)`; timer fire 时调 [`tick_repeat`] 拿当前
//! repeat key 的字节副本写 PTY, 返回 `TimeoutAction::ToDuration(rate_ms)`
//! 自动 reschedule. Released 同 keycode 或任意 modifier 变化时返
//! [`KeyboardAction::StopRepeat`], 调用方 remove timer (或下次 tick 检查
//! `current_repeat == None` 自动 Drop).
//!
//! 派单 In #C: modifier 任意变化 cancel 当前 repeat (alacritty/foot 行为) —
//! 简化路径, 不区分 Shift / Ctrl / Alt.
//!
//! ## 安全边界
//!
//! `Event::Keymap` 传 `OwnedFd` + `size: u32`。我们走 `mmap` 读 size 字节
//! → utf8 String → `Keymap::new_from_string` (safe API 路径, 避开 xkbcommon
//! 的 `unsafe Keymap::new_from_fd`)。mmap 的 unsafe 集中在 [`load_keymap_from_fd`]
//! 单一函数, 配 `// SAFETY:` 注释块。

use std::os::fd::{AsRawFd, OwnedFd};

use anyhow::{anyhow, Context, Result};
use wayland_client::protocol::wl_keyboard::{self, KeyState, KeymapFormat};
use wayland_client::WEnum;
use xkbcommon::xkb;

/// quill 自有的键盘状态封装。
///
/// **字段全私有** (INV-010): 下游不能直接拿 `xkb::State` 出去, 全部走
/// [`handle_key_event`] 单点入口。
///
/// `xkb_state` 是 `Option` 因为 keymap 在 `wl_keyboard::Event::Keymap` 才到,
/// 早于 keymap 的 Key event 一律忽略 (`handle_key_event` 返 None) — 这是
/// Wayland 协议保证的事件序: Keymap 必先于 Enter, Enter 必先于 Key。
///
/// `repeat_rate` / `repeat_delay` 单位由 wl_keyboard `RepeatInfo` 给:
/// - rate (i32, keys/sec, 0 表示禁用 repeat)
/// - delay (i32, 首次按下到第一次 repeat 的延迟 ms)
///
/// T-0603 真接入 calloop Timer: Pressed → [`KeyboardAction::StartRepeat`],
/// Released / Modifier 变化 → [`KeyboardAction::StopRepeat`], timer fire 时
/// 调 [`tick_repeat`] 取字节副本.
pub struct KeyboardState {
    /// xkbcommon 库上下文。生命周期与 KeyboardState 同, 进程内单例 (一个 quill
    /// 一个 KeyboardState, 一份 Context)。无 unsafe, 纯 Rust safe wrapper。
    context: xkb::Context,
    /// 当前 layout keymap。`Option`: compositor 在 wl_keyboard 绑定后才推
    /// `Keymap` event, 早于此 None。
    /// 字段顺序无 drop 序敏感性 (xkbcommon 内部 Arc), 但保留 context → keymap →
    /// state 声明顺序便于 review (state 借 keymap 借 context, 同 ADR 0006 文档示例)。
    keymap: Option<xkb::Keymap>,
    /// 当前键盘状态 (含 modifier / layout / level), 由 keymap 派生。
    xkb_state: Option<xkb::State>,
    /// wl_keyboard `RepeatInfo` 给的 rate (keys/sec)。0 = compositor 禁用
    /// repeat, 非 0 = 期望连发频率。T-0603 timer reschedule 用此值算 interval.
    repeat_rate: i32,
    /// wl_keyboard `RepeatInfo` 给的 delay (ms, 首次按下 → 第一次 repeat 间隔)。
    /// T-0603 timer 首次 fire 用此值。
    repeat_delay: i32,
    /// T-0603: 当前正在 repeat 的键 (Pressed 但还没 Released, 且 modifier 未
    /// 变). `None` 表示无 repeat 进行 — [`tick_repeat`] 此时返 `None` 让
    /// timer callback 走 `TimeoutAction::Drop` 自然终止. 非 modifier-only /
    /// utf8 非空的 Pressed 键才会进入 repeat (modifier 单按 / utf8 空键忽略).
    current_repeat: Option<RepeatKey>,
    /// T-0603: 上次记录的 modifier mask (mods_depressed). 用于检测 modifier
    /// 变化 — `update_modifiers` 收到新 mask 与此值不同时 → cancel 当前
    /// repeat (派单 In #C alacritty 行为). 起步 0 (无 modifier 按下).
    last_modifier_mask: u32,
}

/// T-0603: 当前 repeat 中的键的状态. 字段全私有 (INV-010), 仅本模块构造 / 读取.
#[derive(Debug, Clone)]
struct RepeatKey {
    /// evdev keycode (Pressed 时记录), 用于 `Released` 时判断"是否同一键"
    /// → 不同键不取消 (按住 a 又按 b 时 a release 才 stop, b release 不影响).
    keycode: u32,
    /// 对应字节 (xkbcommon 算出的 utf8 / terminal_keysym_override) 副本.
    /// timer fire 时直接 clone 给调用方 — 不重新走 xkbcommon (modifier
    /// 状态可能已变, 但派单 In #C 已规定 modifier 变化即 cancel, 字节
    /// 在 StartRepeat 一次性算定不变).
    bytes: Vec<u8>,
}

impl KeyboardState {
    /// 启动期建空 KeyboardState。Context 立即建好 (libxkbcommon 失败属严重
    /// 环境问题, 与 quill 启动期其他 wayland init 同等致命, 用 `?` 透传)。
    /// keymap / state 留 None, 等 wl_keyboard `Keymap` event 到时填。
    pub fn new() -> Result<Self> {
        let context = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
        // xkb::Context::new 不 Result; 健壮性靠后续 keymap load 时再失败 — 与
        // libxkbcommon C API 一致 (xkb_context_new 返 NULL 是 OOM, 实际罕见)。
        Ok(Self {
            context,
            keymap: None,
            xkb_state: None,
            repeat_rate: 0,
            repeat_delay: 0,
            current_repeat: None,
            last_modifier_mask: 0,
        })
    }

    /// 测试入口: 用内置 us layout 字符串建 keymap, 跳过 wl_keyboard `Keymap`
    /// event 路径。**仅 lib unit test + integration test (T-0603) 用**, 真
    /// 路径 (`handle_key_event` 收 Keymap event) 不走此函数。
    ///
    /// `#[doc(hidden)] pub`: 集成测试 `tests/keyboard_repeat_e2e.rs` (T-0603)
    /// 在 quill crate 外, 拿不到 `pub(crate)` 项; 改为 `pub` + `doc(hidden)`
    /// 让集成测试可调但不出现在 docs.rs (与 INV-010 类型隔离精神一致 — 不算
    /// 公共 API). T-0501 的 `#[cfg(test)]` 限定保留也行, 但 cfg(test) 在集成
    /// 测试 crate 编译时**不生效** (集成测试是独立 crate, dep on quill 是
    /// release / debug profile, 非 test profile), 必须 `pub` 才能跨 crate.
    #[doc(hidden)]
    pub fn load_default_us_keymap(&mut self) -> Result<()> {
        // xkbcommon `Keymap::new_from_names` 用 RMLVO (rules / model / layout /
        // variant / options) 系统默认值生成 keymap — 系统装了 X.Org keyboard
        // dataset (xkeyboard-config 包) 即可, Wayland session 必装。
        let keymap = xkb::Keymap::new_from_names(
            &self.context,
            "",   // rules: ""=默认 (evdev)
            "",   // model: ""=默认 (pc105)
            "us", // layout
            "",   // variant
            None, // options
            xkb::KEYMAP_COMPILE_NO_FLAGS,
        )
        .ok_or_else(|| anyhow!("xkb::Keymap::new_from_names(us) 失败 — 装 xkeyboard-config?"))?;
        let state = xkb::State::new(&keymap);
        self.keymap = Some(keymap);
        self.xkb_state = Some(state);
        Ok(())
    }

    /// 当前 RepeatInfo (rate, delay). T-0603 calloop Timer 调用方读此值算
    /// 首次 delay (`Duration::from_millis(delay as u64)`) + 后续 interval
    /// (`Duration::from_millis(1000 / rate)`).
    ///
    /// rate=0 (compositor 禁 repeat) 时调用方应**完全不 schedule timer** —
    /// 派单 In #B "wl_keyboard.repeat_info 给的值即可"。
    pub fn repeat_info(&self) -> (i32, i32) {
        (self.repeat_rate, self.repeat_delay)
    }

    /// T-0603 timer fire 入口: 检查当前是否仍有 repeat 进行, 是则返字节副本.
    /// 调用方 (`wl/window.rs` 的 timer callback) 据此 `pty.write(&bytes)`,
    /// 然后返 `TimeoutAction::ToDuration(rate_interval)` 让 calloop 自动
    /// reschedule. None 时调用方返 `TimeoutAction::Drop` 终止 timer.
    ///
    /// **why 不在此判时间到没到**: calloop `Timer::from_duration` 已经把
    /// "delay / interval" 调度交给 calloop 内核 (`TimeoutAction::ToDuration`
    /// 自动 reschedule), 此处仅校验"键是否仍按住", 不算时间窗口.
    ///
    /// **派单 In #A 描述**: `tick_repeat(state, now) -> Option<Vec<u8>>` —
    /// `now: Instant` 入参在最终设计里**用不到** (calloop 调度精度由
    /// `TimeoutAction::ToDuration` 提供, callback 不需自算 deadline). 偏离
    /// 项: 去掉 now 参数, 简化 API. 不引 `std::time::Instant` 依赖.
    pub fn tick_repeat(&self) -> Option<Vec<u8>> {
        self.current_repeat.as_ref().map(|r| r.bytes.clone())
    }

    /// T-0603: 当前是否有 repeat 正在进行. 仅给单测 / 调用方查询用 (调用方
    /// 通过 `KeyboardAction::StartRepeat` / `StopRepeat` 同步管理 timer 句柄,
    /// 不需要轮询此字段).
    #[cfg(test)]
    pub(crate) fn has_repeat(&self) -> bool {
        self.current_repeat.is_some()
    }
}

/// `handle_key_event` 的副作用描述: 翻译完一次 wl_keyboard event 后告诉调用方
/// 要不要往 PTY 写字节 / 调度 timer.
///
/// 抽 enum 而非直接 `Option<Vec<u8>>` 是 conventions §3 套路 (类比
/// `WindowAction`), 给将来扩展空间 — 例如 Phase 6 加 "焦点切走 → 清 IME
/// preedit" 时可 + variant, 不破坏 `handle_key_event` 签名。
///
/// T-0603 加 `StartRepeat` / `StopRepeat` (派单 In #A): 由调用方
/// (`wl/window.rs::Dispatch<WlKeyboard>`) 据此调度 / 取消 calloop
/// `Timer` source. `WriteToPty` 仍代表"立即写一次"语义 (Pressed 即时回显),
/// `StartRepeat` 携带相同 `bytes` (副本), 让 timer fire 时一致回放.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum KeyboardAction {
    /// 没事可做 — 例如 Modifiers/Enter/Leave/RepeatInfo/Keymap event, 或者
    /// Key event 但 keymap 未到, 或释放键 (release event 不匹配 current_repeat),
    /// 或 utf8 为空字符串。
    #[default]
    Nothing,
    /// 往 PTY master 写这串字节 (UTF-8)。空 vec 已被 `Nothing` 路径吃掉,
    /// 这里 vec 至少 1 字节。**单按一次**, 不含 repeat 语义 — repeat 由
    /// `StartRepeat` 走 timer 路径 (派单 In #A 分离即时写与 repeat 调度).
    ///
    /// **当前未构造路径** (T-0603): `key_press_action` 全部 utf8 非空 Pressed
    /// 走 `StartRepeat` (含立即写一次的 bytes). `WriteToPty` 留给将来 "写一次
    /// 但**不**进 repeat" 的键 (例如未来 F1-F12 escape sequence 决定不连发,
    /// 或 IME compose 中间态需要回显但不 repeat). `#[allow(dead_code)]` 显式
    /// 放行, 不删 variant 以保持 Dispatch<WlKeyboard> 路径 match 无 `_ =>`
    /// 兜底 (INV-010 enum 防御 — 加新 variant 时编译期 catch).
    #[allow(dead_code)]
    WriteToPty(Vec<u8>),
    /// T-0603: Pressed 且字节非空 (非 modifier-only) → 立即写一次 + schedule
    /// repeat timer (delay_ms 后首次 fire, 之后 1000/rate ms 周期). 调用方应:
    /// 1. `pty.write(&bytes)` (与 `WriteToPty` 同写一次)
    /// 2. cancel 已有 repeat timer (若任意, 防多键重叠)
    /// 3. 用 `KeyboardState::repeat_info()` 拿 (rate, delay), rate>0 时
    ///    `Timer::from_duration(Duration::from_millis(delay as u64))` 注册
    ///    新 source. rate=0 不 schedule (compositor 禁 repeat).
    StartRepeat { bytes: Vec<u8> },
    /// T-0603: Released 匹配 `current_repeat.keycode` / 或 modifier 任意变化.
    /// 调用方应 cancel 已注册 repeat timer (remove RegistrationToken).
    /// timer 即使下次仍 fire (race: stop 与 timer fire 在 calloop 同 tick),
    /// `tick_repeat()` 此时 `current_repeat=None` 返 `None`, callback 走
    /// `TimeoutAction::Drop` 自然终止 — 双保险.
    StopRepeat,
}

/// 接 wl_keyboard 协议事件 → 算 [`KeyboardAction`]。
///
/// **纯逻辑** (无 IO, 不写 PTY, 不调 wl request): 调用方 (`wl/window.rs`
/// 的 `Dispatch<WlKeyboard>`) 据返回 action 决定是否调
/// `pty.write(&bytes)`。INV-005 calloop 单线程不阻塞: 调用方拿到字节后
/// 走 `pty.write` (master fd O_NONBLOCK, INV-009), WouldBlock 直接丢 (派单
/// 允许)。
///
/// 协议事件分派表:
/// - **Keymap(format, fd, size)**: 加载 keymap, 建 xkb::State; 失败仅 warn,
///   keyboard 退化到无解码 (相当于 keymap 未到)。返 `Nothing`。
/// - **Enter / Leave**: focus 切换, 仅 trace, 不动 keymap/state。返 `Nothing`。
/// - **Key(state=Pressed)**: state.key_get_utf8(keycode); UTF-8 非空 → `WriteToPty`,
///   空 → `Nothing` (例如 modifier-only press)。
/// - **Key(state=Released)**: 派单 In #B "单按只发一次" — release 不发字节,
///   返 `Nothing`。
/// - **Modifiers(...)**: state.update_mask, 同步 modifier。返 `Nothing`。
/// - **RepeatInfo(rate, delay)**: 记录 rate/delay, 不启 timer (Phase 6 做)。
///
/// 已知陷阱:
/// - **XKB keycode = evdev keycode + 8** (X11 历史: keycode 从 8 起, 0-7 留
///   modifier 字符)。`Event::Key.key` 给的是 evdev (KEY_A=30), `key_get_utf8`
///   要 XKB code (KEY_A → XKB 38)。漏 +8 整个 layout 错位。
/// - **`update_mask` 的 group**: wl_keyboard `Modifiers.group` 给 layout group
///   (主键盘 layout 切换索引), 直接喂 `update_mask` 的 `effective_group`
///   (与 `depressed_layout` / `latched_layout` / `locked_layout` 三个分开,
///   实际 wl_keyboard 简化只暴露一个 effective group)。我们对 depressed/
///   latched/locked layout 全填 0, group 落 effective — 这是 sway/foot/mako
///   等 client 的标准做法。
pub fn handle_key_event(event: wl_keyboard::Event, state: &mut KeyboardState) -> KeyboardAction {
    match event {
        wl_keyboard::Event::Keymap { format, fd, size } => {
            handle_keymap_event(state, format, fd, size);
            KeyboardAction::Nothing
        }
        wl_keyboard::Event::Enter { serial, .. } => {
            tracing::debug!(target: "quill::keyboard", serial, "wl_keyboard enter");
            KeyboardAction::Nothing
        }
        wl_keyboard::Event::Leave { serial, .. } => {
            tracing::debug!(target: "quill::keyboard", serial, "wl_keyboard leave");
            KeyboardAction::Nothing
        }
        wl_keyboard::Event::Key {
            key,
            state: key_state,
            ..
        } => key_press_action(state, key, key_state),
        wl_keyboard::Event::Modifiers {
            mods_depressed,
            mods_latched,
            mods_locked,
            group,
            ..
        } => {
            // T-0603 派单 In #C: 任何 modifier 变化 cancel 当前 repeat
            // (alacritty/foot 行为). 比较新旧 mods_depressed mask, 不一致即
            // cancel — 简化路径不区分 Shift/Ctrl/Alt 哪个变了.
            let prev_mask = state.last_modifier_mask;
            update_modifiers(state, mods_depressed, mods_latched, mods_locked, group);
            state.last_modifier_mask = mods_depressed;
            if prev_mask != mods_depressed && state.current_repeat.is_some() {
                state.current_repeat = None;
                tracing::debug!(
                    target: "quill::keyboard",
                    prev_mask,
                    new_mask = mods_depressed,
                    "modifier 变化, cancel current repeat"
                );
                return KeyboardAction::StopRepeat;
            }
            KeyboardAction::Nothing
        }
        wl_keyboard::Event::RepeatInfo { rate, delay } => {
            state.repeat_rate = rate;
            state.repeat_delay = delay;
            tracing::debug!(target: "quill::keyboard", rate, delay, "wl_keyboard repeat info");
            KeyboardAction::Nothing
        }
        // wayland-client 0.31 的 wl_keyboard::Event 无 #[non_exhaustive], 但为防
        // 上游加 variant 我们对未知 event 沉默 — 协议层加事件不应破坏 client。
        _ => KeyboardAction::Nothing,
    }
}

/// 处理 `wl_keyboard::Event::Keymap`: 加载 keymap fd → utf8 字符串 → xkbcommon。
///
/// 失败 (format 不识别 / mmap 错 / xkbcommon parse 错) 仅 `tracing::warn`, 不
/// panic — keyboard 退化到 "无解码" (后续 Key event 因 xkb_state=None 全部沉默)。
/// quill 仍可跑, 用户用鼠标关窗。
fn handle_keymap_event(
    state: &mut KeyboardState,
    format: WEnum<KeymapFormat>,
    fd: OwnedFd,
    size: u32,
) {
    let format_val = match format {
        WEnum::Value(KeymapFormat::XkbV1) => KeymapFormat::XkbV1,
        WEnum::Value(other) => {
            tracing::warn!(target: "quill::keyboard", ?other, "wl_keyboard keymap format 非 XkbV1, 忽略");
            return;
        }
        WEnum::Unknown(raw) => {
            tracing::warn!(target: "quill::keyboard", raw, "wl_keyboard keymap format 未知, 忽略");
            return;
        }
        // NoKeymap / 其他未来值: 不识别就忽略
        #[allow(unreachable_patterns)]
        _ => {
            tracing::warn!(target: "quill::keyboard", "wl_keyboard keymap format 不支持");
            return;
        }
    };
    debug_assert_eq!(format_val, KeymapFormat::XkbV1);

    let keymap_str = match load_keymap_from_fd(&fd, size as usize) {
        Ok(s) => s,
        Err(err) => {
            tracing::warn!(target: "quill::keyboard", ?err, "load_keymap_from_fd 失败, 键盘解码暂停");
            return;
        }
    };
    let keymap = match xkb::Keymap::new_from_string(
        &state.context,
        keymap_str,
        xkb::KEYMAP_FORMAT_TEXT_V1,
        xkb::KEYMAP_COMPILE_NO_FLAGS,
    ) {
        Some(k) => k,
        None => {
            tracing::warn!(target: "quill::keyboard", "xkb::Keymap::new_from_string 返 None — keymap 不合法?");
            return;
        }
    };
    let xkb_state = xkb::State::new(&keymap);
    state.keymap = Some(keymap);
    state.xkb_state = Some(xkb_state);
    tracing::info!(target: "quill::keyboard", "wl_keyboard keymap 加载成功");
}

/// 处理一次 Key (press/release) → bytes / repeat 调度.
///
/// **Released 路径**:
/// - 匹配 `current_repeat.keycode` → 清 `current_repeat` + 返 `StopRepeat`
///   (调用方 cancel timer)
/// - 不匹配 (按住 a 又按 b 时 b 的 release) → `Nothing` (a 仍 repeat)
/// - 无 `current_repeat` → `Nothing` (单按 release / 早于 keymap 的 release)
///
/// **Pressed 路径**:
/// - 走 xkbcommon `key_get_utf8` (modifier composition 由 xkbcommon 内部
///   做: Ctrl+c → "\x03", Shift+a → "A", AltGr+e → "é", dead key + base
///   → composed)
/// - terminal_keysym_override 命中 (BackSpace/Delete) → 用 override bytes
/// - utf8 空 (modifier-only single press) → `Nothing` (不进 repeat)
/// - utf8 非空 → 记 `current_repeat = Some(RepeatKey { keycode, bytes })`
///   + 返 `StartRepeat { bytes }` (调用方写 PTY 一次 + schedule timer)
fn key_press_action(
    state: &mut KeyboardState,
    evdev_keycode: u32,
    key_state: WEnum<KeyState>,
) -> KeyboardAction {
    // Released 分支
    if matches!(key_state, WEnum::Value(KeyState::Released)) {
        if let Some(active) = state.current_repeat.as_ref() {
            if active.keycode == evdev_keycode {
                state.current_repeat = None;
                tracing::debug!(
                    target: "quill::keyboard",
                    evdev_keycode,
                    "released active repeat key"
                );
                return KeyboardAction::StopRepeat;
            }
        }
        return KeyboardAction::Nothing;
    }
    // 非 Pressed 也非 Released (例如 WEnum::Unknown / 未来 variant) → 沉默
    if !matches!(key_state, WEnum::Value(KeyState::Pressed)) {
        return KeyboardAction::Nothing;
    }
    let xkb_state = match state.xkb_state.as_mut() {
        Some(s) => s,
        None => {
            tracing::trace!(target: "quill::keyboard", evdev_keycode, "key event 早于 keymap, 忽略");
            return KeyboardAction::Nothing;
        }
    };

    // XKB keycode = evdev keycode + 8 (X11 历史偏移)
    let xkb_keycode = xkb::Keycode::new(evdev_keycode + 8);

    // 注: xkbcommon-rs 文档要求 update_key 仅在 Modifiers event 之外维护 latched
    // /locked 状态时才需要; modifier 状态由 Modifiers event 通过 update_mask
    // 同步, 我们这里不再 update_key — 与 wlroots/Sway/foot 的 client 套路一致。
    //
    // **terminal-style keysym 重写**: xkbcommon `key_get_utf8` 对 BackSpace 给
    // `\x08` (BS, 老 telnet 风格), 对 Delete 给 `\x7f` (DEL); 现代 unix
    // terminal (xterm / foot / alacritty / gnome-terminal) 反过来 — Backspace
    // 送 DEL (0x7f), Delete 键送 ESC[3~。这是 client 侧约定, xkbcommon 不做。
    // 命中早返 (xterm convention) 以下用 keysym 判断, 比覆盖整个 UTF-8 字符
    // 串更稳。其它键全走 xkbcommon utf8 默认 (Ctrl+letter / Shift+letter /
    // dead key / compose 全交给 xkbcommon)。
    let keysym = xkb_state.key_get_one_sym(xkb_keycode);
    let bytes = if let Some(b) = terminal_keysym_override(keysym) {
        b
    } else {
        let utf8 = xkb_state.key_get_utf8(xkb_keycode);
        if utf8.is_empty() {
            // modifier-only key (Shift / Ctrl / Alt 单按) → 无 UTF-8 输出,
            // 沉默 + 不进 repeat (按住 Shift 不该连发).
            return KeyboardAction::Nothing;
        }
        utf8.into_bytes()
    };

    // T-0603: 进入 repeat. 不论之前 current_repeat 是否为 Some — 直接覆盖
    // (用户切按下另一键时, 旧 repeat 被新 repeat 替换, alacritty/foot 同).
    // 调用方在 StartRepeat 路径 cancel 旧 timer + insert 新 timer, 不会留
    // 重复 source.
    state.current_repeat = Some(RepeatKey {
        keycode: evdev_keycode,
        bytes: bytes.clone(),
    });
    KeyboardAction::StartRepeat { bytes }
}

/// terminal-style keysym 重写表。命中返 `Some(bytes)`, 不命中返 `None` 让
/// xkbcommon `key_get_utf8` 走默认路径。
///
/// 当前覆盖:
/// - **BackSpace (keysym 0xff08)** → `\x7f` (DEL). xterm convention, foot/
///   alacritty 同。xkbcommon 默认给 \x08 (BS), 老 telnet 风格不适用。
/// - **Delete (keysym 0xffff)** → `\x1b[3~`. xterm escape sequence, foot/
///   alacritty 同。xkbcommon 默认给 \x7f, 与 BackSpace 撞且不是现代 terminal
///   习惯。
///
/// 不在此处覆盖 Tab (xkbcommon 默认 `\t` 已对) / Enter / Esc — xkbcommon 默
/// 认值与 xterm 一致。Phase 5/6 加 Function key (F1-F12) / Arrow / Home/End
/// 时再扩此表 (派单 Out: 鼠标 / 复杂 modifier 留 T-0506+)。
fn terminal_keysym_override(keysym: xkb::Keysym) -> Option<Vec<u8>> {
    // xkbcommon::xkb::Keysym 是 newtype wrapper around u32; 直接比较 raw 值。
    // 常量值见 xkeysym keysymdef.h: BackSpace=0xff08, Delete=0xffff
    let raw = keysym.raw();
    match raw {
        0xff08 => Some(vec![0x7f]),          // BackSpace → DEL
        0xffff => Some(b"\x1b[3~".to_vec()), // Delete → CSI 3 ~
        _ => None,
    }
}

/// 同步 wl_keyboard `Modifiers` event 给 xkbcommon。
///
/// 4 个 mask + group 直接喂 `xkb::State::update_mask`。depressed/latched/locked
/// layout 三个填 0, effective layout (group) 落到 `effective_group` — sway/foot
/// 的标准做法。
fn update_modifiers(
    state: &mut KeyboardState,
    mods_depressed: u32,
    mods_latched: u32,
    mods_locked: u32,
    group: u32,
) {
    let xkb_state = match state.xkb_state.as_mut() {
        Some(s) => s,
        None => {
            tracing::trace!(target: "quill::keyboard", "modifiers event 早于 keymap, 忽略");
            return;
        }
    };
    xkb_state.update_mask(mods_depressed, mods_latched, mods_locked, 0, 0, group);
}

/// mmap fd 的 size 字节 → UTF-8 String。失败任意一步返 anyhow Error, 调用方
/// (`handle_keymap_event`) 仅 warn, 不 panic。
///
/// **why mmap 而非 read**: wl_keyboard 协议保证 fd 内容是 size 字节的 keymap
/// 文本; 但有些 compositor (mutter / weston) 用 `MAP_PRIVATE` shm fd, read
/// 不一定合法 (可能 ENOSYS 或返 0)。mmap 是 wayland 客户端事实上的协议
/// 处理方式 (libwayland-client / SCTK / wlroots 客户端示例都 mmap)。
fn load_keymap_from_fd(fd: &OwnedFd, size: usize) -> Result<String> {
    if size == 0 {
        return Err(anyhow!("keymap size = 0"));
    }
    // SAFETY:
    // - `fd` 是 wl_keyboard 协议保证活的 OwnedFd, 我们仅借用 raw fd 不夺所有权
    // - mmap PROT_READ + MAP_PRIVATE: 进程私有只读映射, 改不破坏其他进程
    // - size 直接来自 wl_keyboard 协议 (compositor 推); 若 compositor 撒谎超
    //   实际 fd 大小, mmap 在 read 越界时给 SIGBUS, 但该协议层假设 compositor
    //   守约 (与 SCTK / wlroots / cosmic-comp 假设一致)
    // - munmap 在 mmap 成功后必跑 (defer 通过 scope 末尾显式 libc::munmap),
    //   `?` 早返路径不触达 mmap 后, 无 leak
    // - 本块**不**长持映射: 立即拷贝 size 字节到 Vec, mmap 区域随后 munmap 释放
    #[allow(unsafe_code)]
    let bytes = unsafe {
        let raw_fd = fd.as_raw_fd();
        let ptr = libc::mmap(
            std::ptr::null_mut(),
            size,
            libc::PROT_READ,
            libc::MAP_PRIVATE,
            raw_fd,
            0,
        );
        if ptr == libc::MAP_FAILED {
            return Err(anyhow!(
                "mmap keymap fd 失败: {}",
                std::io::Error::last_os_error()
            ));
        }
        // SAFETY: ptr 是有效 PROT_READ 映射区, 长度 size; from_raw_parts 只读
        // 一次后立即拷贝到 Vec, 之后 munmap, 借用不外泄
        let slice = std::slice::from_raw_parts(ptr as *const u8, size);
        let bytes = slice.to_vec();
        if libc::munmap(ptr, size) != 0 {
            // munmap 失败极罕见, 仅 warn 不返错 — bytes 已拿到, 调用方可用
            tracing::warn!(target: "quill::keyboard", "munmap keymap 区失败: {}", std::io::Error::last_os_error());
        }
        bytes
    };
    // wl_keyboard keymap 字符串可能含末尾 \0 (libxkbcommon C 习惯), 先 trim
    let trimmed_len = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    let s = std::str::from_utf8(&bytes[..trimmed_len])
        .context("keymap fd 字节非 UTF-8")?
        .to_owned();
    Ok(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 集成单测帮助: 把 evdev keycode 喂进 handle_key_event, 走真 us keymap。
    /// 直接构造 wl_keyboard::Event::Key 模拟 wayland event 派发。
    fn press(state: &mut KeyboardState, evdev_key: u32) -> KeyboardAction {
        // wl_keyboard::Event::Key 的字段有 serial/time/key/state, 我们只关心 key/state。
        let event = wl_keyboard::Event::Key {
            serial: 0,
            time: 0,
            key: evdev_key,
            state: WEnum::Value(KeyState::Pressed),
        };
        handle_key_event(event, state)
    }

    fn release(state: &mut KeyboardState, evdev_key: u32) -> KeyboardAction {
        let event = wl_keyboard::Event::Key {
            serial: 0,
            time: 0,
            key: evdev_key,
            state: WEnum::Value(KeyState::Released),
        };
        handle_key_event(event, state)
    }

    fn set_modifiers(state: &mut KeyboardState, depressed: u32) {
        let event = wl_keyboard::Event::Modifiers {
            serial: 0,
            mods_depressed: depressed,
            mods_latched: 0,
            mods_locked: 0,
            group: 0,
        };
        let _ = handle_key_event(event, state);
    }

    /// 'a' key (evdev KEY_A = 30) 单按 → StartRepeat { b"a" }. T-0603 起改为
    /// `StartRepeat` (Pressed 即时回显由调用方 `pty.write(&bytes)`, repeat
    /// 由调用方 schedule timer). 派单 acceptance 第一条 (字节内容仍是 "a").
    #[test]
    fn press_a_starts_repeat_with_lowercase_a() {
        let mut state = KeyboardState::new().expect("ctx new");
        state
            .load_default_us_keymap()
            .expect("us keymap 应能加载 (装了 xkeyboard-config 包)");
        // KEY_A = 30 (linux/input-event-codes.h)
        let action = press(&mut state, 30);
        assert_eq!(
            action,
            KeyboardAction::StartRepeat {
                bytes: b"a".to_vec()
            },
            "us layout 'a' 单按应 StartRepeat 携带 lowercase 'a'"
        );
        assert!(state.has_repeat(), "Pressed 后应进入 repeat 状态");
    }

    /// Ctrl+c → StartRepeat { 0x03 } (ETX, shell SIGINT). xkbcommon 内部对
    /// Ctrl+letter 做 modifier composition 直返控制字符. 派单 acceptance 第二条.
    #[test]
    fn ctrl_c_starts_repeat_with_etx_0x03() {
        let mut state = KeyboardState::new().expect("ctx new");
        state.load_default_us_keymap().expect("keymap");
        // 设 Control mask. xkbcommon 默认 Control_L 在 mod_index Control (=2),
        // 即 mods_depressed bit 2 = 0b0100 = 4。
        set_modifiers(&mut state, 1 << 2);
        // KEY_C = 46
        let action = press(&mut state, 46);
        assert_eq!(
            action,
            KeyboardAction::StartRepeat { bytes: vec![0x03] },
            "Ctrl+C 应 StartRepeat 携带 ETX 0x03"
        );
    }

    /// Enter (return) → StartRepeat { b"\r" } (CR). PTY raw mode 期望 \r,
    /// 由 termios icrnl 转换为 \n (我们不在 client 侧做转换).
    /// KEY_ENTER = 28
    #[test]
    fn enter_starts_repeat_with_carriage_return() {
        let mut state = KeyboardState::new().expect("ctx new");
        state.load_default_us_keymap().expect("keymap");
        let action = press(&mut state, 28);
        assert_eq!(
            action,
            KeyboardAction::StartRepeat {
                bytes: b"\r".to_vec()
            },
            "Enter 应 StartRepeat 携带 \\r (PTY termios 自己转 \\n)"
        );
    }

    /// Backspace → StartRepeat { 0x7f } (DEL). 现代 unix terminal 约定 (xterm /
    /// gnome-terminal / alacritty 都送 DEL). T-0603 长按 backspace 必须连续删
    /// — 用户实测反馈的核心场景.
    /// KEY_BACKSPACE = 14
    #[test]
    fn backspace_starts_repeat_with_del_0x7f() {
        let mut state = KeyboardState::new().expect("ctx new");
        state.load_default_us_keymap().expect("keymap");
        let action = press(&mut state, 14);
        assert_eq!(
            action,
            KeyboardAction::StartRepeat { bytes: vec![0x7f] },
            "Backspace 应 StartRepeat 携带 DEL 0x7f (xterm convention)"
        );
    }

    /// release 同 keycode 应触发 StopRepeat (派单 In #A: Released 时调用方应
    /// cancel timer). T-0603 改: release 是 stop 信号而非 Nothing.
    #[test]
    fn release_after_press_stops_repeat() {
        let mut state = KeyboardState::new().expect("ctx new");
        state.load_default_us_keymap().expect("keymap");
        let _start = press(&mut state, 30); // KEY_A → StartRepeat
        assert!(state.has_repeat());
        let action = release(&mut state, 30);
        assert_eq!(
            action,
            KeyboardAction::StopRepeat,
            "release 同键应 StopRepeat"
        );
        assert!(!state.has_repeat(), "release 后 current_repeat 应清");
    }

    /// release 不在 current_repeat 时返 Nothing (无前置 press / 早于 keymap
    /// 的 release / 已被 modifier 变化清掉的 repeat).
    #[test]
    fn release_without_repeat_is_nothing() {
        let mut state = KeyboardState::new().expect("ctx new");
        state.load_default_us_keymap().expect("keymap");
        let action = release(&mut state, 30); // KEY_A 未先 press
        assert_eq!(
            action,
            KeyboardAction::Nothing,
            "无 current_repeat 时 release 应 Nothing"
        );
    }

    /// keymap 未到时所有 Key event 沉默 (wl_keyboard 协议保证 Keymap 先于 Key,
    /// 但万一 compositor 乱序 / 我们漏处理 — 防御)。
    #[test]
    fn key_event_before_keymap_is_silent() {
        let mut state = KeyboardState::new().expect("ctx new");
        // 不调 load_default_us_keymap → state.xkb_state = None
        let action = press(&mut state, 30);
        assert_eq!(
            action,
            KeyboardAction::Nothing,
            "无 keymap 时 Key event 应沉默"
        );
    }

    /// Shift+a → StartRepeat { 'A' } (capital). Modifier composition 走 xkbcommon.
    #[test]
    fn shift_a_starts_repeat_with_capital_a() {
        let mut state = KeyboardState::new().expect("ctx new");
        state.load_default_us_keymap().expect("keymap");
        // Shift mod index = 0 (Shift), bit 0 = 1
        set_modifiers(&mut state, 1 << 0);
        let action = press(&mut state, 30); // KEY_A
        assert_eq!(
            action,
            KeyboardAction::StartRepeat {
                bytes: b"A".to_vec()
            },
            "Shift+a 应 StartRepeat 携带 'A'"
        );
    }

    /// Tab key → StartRepeat { b"\t" }. 常用编辑器 / shell completion 触发,
    /// 派单 ASCII 范畴内. KEY_TAB = 15
    #[test]
    fn tab_starts_repeat_with_tab() {
        let mut state = KeyboardState::new().expect("ctx new");
        state.load_default_us_keymap().expect("keymap");
        let action = press(&mut state, 15);
        assert_eq!(
            action,
            KeyboardAction::StartRepeat {
                bytes: b"\t".to_vec()
            },
            "Tab 应 StartRepeat 携带 \\t (HT)"
        );
    }

    // === T-0603 新加测试 (派单 In #D) ===

    /// modifier 任意变化 cancel 当前 repeat (派单 In #C alacritty 行为).
    /// 按住 'a' 进入 repeat → 按 Shift (modifier mask 变化) → 应返 StopRepeat
    /// 且 current_repeat 被清.
    #[test]
    fn modifier_change_cancels_repeat() {
        let mut state = KeyboardState::new().expect("ctx new");
        state.load_default_us_keymap().expect("keymap");
        let _ = press(&mut state, 30); // KEY_A → StartRepeat
        assert!(state.has_repeat());
        // 按 Shift: modifier mask 从 0 变 1
        let event = wl_keyboard::Event::Modifiers {
            serial: 0,
            mods_depressed: 1 << 0,
            mods_latched: 0,
            mods_locked: 0,
            group: 0,
        };
        let action = handle_key_event(event, &mut state);
        assert_eq!(
            action,
            KeyboardAction::StopRepeat,
            "modifier 变化应 StopRepeat"
        );
        assert!(!state.has_repeat(), "modifier 变化后 current_repeat 应清");
    }

    /// modifier 不变时 (重复同 mask) 不应误 cancel repeat. 防御 compositor
    /// 在 focus 切回 / 别的事件携带 Modifiers 但 mask 没动的场景.
    #[test]
    fn modifier_unchanged_keeps_repeat() {
        let mut state = KeyboardState::new().expect("ctx new");
        state.load_default_us_keymap().expect("keymap");
        // 起步 set_modifiers(0) (与初始 last_modifier_mask 同), 不动 repeat
        set_modifiers(&mut state, 0);
        let _ = press(&mut state, 30); // KEY_A → StartRepeat
        assert!(state.has_repeat());
        // 再发一次 mods_depressed=0 (无变化)
        set_modifiers(&mut state, 0);
        assert!(state.has_repeat(), "mask 不变不应清 repeat");
    }

    /// 按住 'a' 又按 'b' (新 Pressed): 旧 repeat 被新 repeat 覆盖. 调用方
    /// 在 StartRepeat 路径会 cancel 旧 timer + insert 新 timer, 因此新键
    /// 接管 repeat. release 'b' 时 stop, release 'a' 时 (current_repeat 已
    /// 是 b, keycode 不匹配 a) 返 Nothing.
    #[test]
    fn second_press_replaces_first_repeat() {
        let mut state = KeyboardState::new().expect("ctx new");
        state.load_default_us_keymap().expect("keymap");
        let _ = press(&mut state, 30); // KEY_A
        let action_b = press(&mut state, 48); // KEY_B = 48
        assert_eq!(
            action_b,
            KeyboardAction::StartRepeat {
                bytes: b"b".to_vec()
            },
            "第二个 Pressed 应覆盖前 repeat 给 StartRepeat"
        );
        // release 'a' (旧键, 已不在 current_repeat) → Nothing
        let release_a = release(&mut state, 30);
        assert_eq!(release_a, KeyboardAction::Nothing, "释放旧键应 Nothing");
        assert!(state.has_repeat(), "旧键 release 不应清 b 的 repeat");
        // release 'b' → StopRepeat
        let release_b = release(&mut state, 48);
        assert_eq!(release_b, KeyboardAction::StopRepeat);
    }

    /// modifier-only 单按 (Shift_L) 不应进 repeat — utf8 空, 无 StartRepeat.
    /// 派单隐式: 按住 Shift 不该连发任何字节.
    #[test]
    fn shift_only_does_not_enter_repeat() {
        let mut state = KeyboardState::new().expect("ctx new");
        state.load_default_us_keymap().expect("keymap");
        let action = press(&mut state, 42); // KEY_LEFTSHIFT
        assert_eq!(
            action,
            KeyboardAction::Nothing,
            "Shift 单按 (无后续 letter) 应沉默"
        );
        assert!(!state.has_repeat(), "modifier-only press 不进 repeat");
    }

    /// `tick_repeat` 在 current_repeat=Some 时返字节副本; None 时返 None.
    /// timer callback 走此入口, None 触发 TimeoutAction::Drop.
    #[test]
    fn tick_repeat_returns_bytes_when_repeating() {
        let mut state = KeyboardState::new().expect("ctx new");
        state.load_default_us_keymap().expect("keymap");
        assert_eq!(state.tick_repeat(), None, "无 repeat 时返 None");
        let _ = press(&mut state, 30); // KEY_A
        assert_eq!(
            state.tick_repeat(),
            Some(b"a".to_vec()),
            "repeat 中应返字节副本"
        );
        // 多次 tick 拿同一字节 (不 mutate state)
        assert_eq!(state.tick_repeat(), Some(b"a".to_vec()));
        let _ = release(&mut state, 30);
        assert_eq!(state.tick_repeat(), None, "release 后返 None");
    }

    /// RepeatInfo 事件应更新 repeat_rate / repeat_delay 不返字节.
    /// T-0603 timer 调度调用方读 `repeat_info()` 的 (rate, delay) 算
    /// `Timer::from_duration(Duration::from_millis(delay))` + reschedule
    /// `1000/rate` ms.
    #[test]
    fn repeat_info_updates_state() {
        let mut state = KeyboardState::new().expect("ctx new");
        let event = wl_keyboard::Event::RepeatInfo {
            rate: 25,
            delay: 600,
        };
        let action = handle_key_event(event, &mut state);
        assert_eq!(action, KeyboardAction::Nothing);
        assert_eq!(state.repeat_info(), (25, 600), "RepeatInfo 应被记录");
    }
}
