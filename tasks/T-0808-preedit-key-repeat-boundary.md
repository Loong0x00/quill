# T-0808 preedit + key-repeat 状态机边界 (M3 悬挂 repeat 修复)

**Phase**: 8 (polish++, terminal correctness)
**Assigned**: TBD (writer)
**Status**: open
**Budget**: tokenBudget=60k (keyboard.rs key_press_action + window.rs swallow path + repeat_timer_tick + 测试)
**Dependencies**: T-0806 已 merge (preedit guard 基础设施)
**Priority**: P2 (M3 是 codex 标 "疑虑" 而非确认 bug, 触发需 "key 在 IME preedit 升起前已 StartRepeat", 但代码逻辑确实漏)

## Bug

`src/wl/window.rs:4420-4425, 4495-4503` (T-0806 preedit guard) + `src/wl/keyboard.rs:361-369, 463-472` (current_repeat 状态机) + `src/wl/window.rs` 的 `repeat_timer_tick` (drive_wayland step 3.6).

T-0806 在 wl_keyboard.key event 命中 `key_event_swallowed_for_preedit` 时直接 `return`, 没动 `state.keyboard_state.current_repeat`. 后果:

时序触发:
1. user 按住某 key (例 'a') → `key_press_action` 走 Pressed 路径 → `state.current_repeat = Some(...)` + 返 `StartRepeat { bytes }` → drive_wayland schedule timer
2. user 触发 fcitx5 进 preedit (例 切 zh + 输入 'j' → preedit "j") — 注意 IME 升起 preedit 不会经 wl_keyboard.key 路径 (text-input-v3 通过自己的 preedit_string event)
3. 'a' 的 timer 继续 fire → `repeat_timer_tick` 调 write_keyboard_bytes → PTY 收到 'a', 'a', 'a'... 直到:
   - user 松 'a' → wl_keyboard.key release → 命中 `key_event_swallowed_for_preedit` (因 preedit 还非空) → `return` → **`current_repeat` 没清, `pending_repeat = Stop` 没设** → timer 永远 fire
   - 或 user 按其他物理键 → 同样被 swallow → 不影响 repeat

结果: preedit 期间 PTY 持续收到 'a' 字节, 直到 IME 退 preedit 后下次 release 才清。视觉上 user 在打中文, PTY 里却被乱写 'aaaa...'。

## 真因

T-0806 的 swallow 是"对外吞 key event 不送 PTY", 但 quill 自己的 keyboard 状态机 (`current_repeat` + repeat timer) 是另一路, swallow 没扯断这路。`key_event_swallowed_for_preedit` 是纯函数, 只判该不该吞当前 event, 不动状态; swallow 调用点 (window.rs:4420) 直接 return 也跳过了 release 路径里的 `current_repeat = None` + `StopRepeat` 转换。

`repeat_timer_tick` 同样不感知 IME state — 它从 `state.keyboard_state.current_repeat` 拿 bytes 写 PTY, 不 check `state.ime_state.is_preedit_active()`。

## Goal

preedit 升起或 swallow 命中时, 当前 repeat 必须 cancel; repeat timer 在 preedit active 时不写 PTY (双保险)。完工后即使触发顺序刁钻 (Pressed → preedit 升起 → Released 被 swallow), PTY 不会被 repeat 字节污染。

## Scope

### In

#### A. swallow 路径 cancel current_repeat (`src/wl/window.rs`)

window.rs:4420 现有 swallow 块:
```rust
if key_event_swallowed_for_preedit(&event, state.ime_state.is_preedit_active()) {
    tracing::trace!(...);
    return;
}
```

改为 (在 return 前清 repeat):
```rust
if key_event_swallowed_for_preedit(&event, state.ime_state.is_preedit_active()) {
    if state.keyboard_state.current_repeat.is_some() {
        state.keyboard_state.current_repeat = None;
        state.pending_repeat = Some(RepeatScheduleRequest::Stop);
        tracing::debug!(
            target: "quill::keyboard",
            "key swallowed (preedit active) + cancel pending repeat"
        );
    } else {
        tracing::trace!(
            target: "quill::keyboard",
            "key swallowed (preedit active)"
        );
    }
    return;
}
```

#### B. repeat_timer_tick 在 preedit active 时跳过 write (`src/wl/window.rs`)

找 `repeat_timer_tick` (或等价 fn, drive_wayland step 3.6 处理 RepeatScheduleRequest), 在写 PTY 之前加判断:

```rust
if state.ime_state.is_preedit_active() {
    tracing::trace!(target: "quill::keyboard", "repeat timer fire 但 preedit active, skip PTY write");
    return;  // 或 continue, 视实际控制流
}
```

不 cancel timer (preedit 退后能恢复 — 但实际 A 段已经 cancel, 此处只是冗余防御)。

#### C. IME enter preedit 时主动 cancel (可选, 看实现成本)

text-input-v3 preedit_string event handler (`src/ime/`?). 收到首个非空 preedit_string 时, 也调:
```rust
if state.keyboard_state.current_repeat.is_some() {
    state.keyboard_state.current_repeat = None;
    state.pending_repeat = Some(RepeatScheduleRequest::Stop);
}
```

如果 IME state 跨模块访问麻烦, 此段可省 (A + B 已覆盖, 只是触发延后到下次 swallow / timer fire)。**writer 自行判断, 不强制**。

#### D. 测试 (`src/wl/keyboard.rs::tests` 或 `src/wl/window.rs::tests`)

由于 repeat_timer_tick 涉及 calloop / 实际 PTY, 测试用 mock 风:

- `swallow_with_active_repeat_clears_current_repeat` — mock keyboard_state.current_repeat = Some(...) + ime_state preedit 非空, 模拟 wl_keyboard.key release event → 验 swallow 后 current_repeat == None 且 pending_repeat == Some(Stop)
- `swallow_without_active_repeat_no_state_change` — mock current_repeat = None + preedit 非空, swallow → current_repeat 仍 None, pending_repeat 不动
- `repeat_tick_skipped_when_preedit_active` — mock helper fn (如果 repeat_timer_tick 抽得出纯逻辑) 验 preedit_active=true 时不调 write_keyboard_bytes; preedit_active=false 时正常调

### Out

- 重构 keyboard ↔ ime ↔ repeat timer 三方状态共享 — 本 ticket 只补漏, 不动模块边界
- IME 完全独立的 preedit grab / release 协议 — text-input-v3 自带, 不在 quill scope
- 修 fcitx5 行为 — 上游

## Acceptance

1. `cargo test --lib` 全过 (含新增测试 ≥ 2 个)
2. `cargo clippy --all-targets -- -D warnings` 无 warning
3. `cargo fmt --check` 跳过 main 既有漂移 (本 ticket 改动文件 fmt 干净)
4. **user 实测** (e2e):
   - 切 fcitx5 中文模式
   - 按住物理键 'a' (任何会 trigger repeat 的 ASCII 键, 在 fcitx5 还没 grab 之前) — 实际触发条件刁钻, 简化版: 按 't' 不松, 立即按 'j' 触发拼音 → 'tj' preedit; 'tj' 期间松 't' → PTY 不应有任何 't' repeat 字节 (cat 一个 stdout 验证)
   - 不可复现也接受 (如代码 review 通过 + 测试覆盖即可)

## INV-010

- swallow 路径访问 `state.keyboard_state.current_repeat` 与 `state.pending_repeat` 是 quill 内部类型, 不引新依赖
- B 段 ime_state.is_preedit_active() 是 T-0806 引入 helper, 复用

## 相关

- T-0806 (preedit guard 基础, 本 ticket 是其延伸)
- T-0603 (key repeat 实装, current_repeat 状态机)
- text-input-v3 spec (preedit 升起/退出 event)
- alacritty src/event.rs preedit + repeat 处理 (对照实现)
- Codex review 报告 (本 session, 2026-05-02; M3 列为 "疑虑")
