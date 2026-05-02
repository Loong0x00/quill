# T-0806 key dispatch preedit guard (防 backspace 误删 PTY 已 commit 字符)

**Phase**: 8 (polish++, terminal correctness)
**Assigned**: TBD (writer)
**Status**: open
**Budget**: tokenBudget=60k (window.rs key dispatch + 测试)
**Dependencies**: 无 (独立)
**Priority**: P1 (user 实测 IME preedit 非空时按 backspace 删了之前 commit 的 PTY 内容, destructive 数据丢失风险)

## Bug

User 实测: fcitx5-rime + 自改 ascii_composer (Shift_L/R noop, Caps_Lock 走默认
inline_ascii) 配置下:

1. 输入拼音 `jin tian tian qi` → preedit 显示
2. 按 Caps Lock 切英文 → Rime inline_ascii 模式: **不 commit 当前候选** + 后续英文
   字母嵌入 preedit ("今天天气Z" → "今天天气ZHENHAO")
3. 按 Backspace 想删 ZHENHAO
4. **fcitx5 不响应 backspace** (preedit 不变, log 持续返同 string)
5. **wl_keyboard.key 14 (Backspace) event 仍到 quill**, quill 没检查 IME preedit
   是否非空, 直接送 PTY → 删了之前 commit 的 "你好" (PTY 中已是普通 utf-8 字节)

数据丢失现象: user 看到屏幕上的 "你好" 被一个一个 backspace 干掉, 而 IME preedit
"今天天气ZHENHAO" 不动, 看起来像 backspace "穿越" 了 preedit 删历史输入.

## 真因

text-input-v3 协议规定:
- IME 不 grab 物理按键事件 (跟 IBus / X11 XIM 模型不同)
- compositor 仍发 wl_keyboard.key 给 client (quill)
- IME 同时也看到这些 key, 决定要不要 produce preedit_string / commit_string
- **client 自己负责防御**: preedit 非空时, key 该吞掉不送 PTY (IME 通过 text-input-v3
  自己处理)

quill 当前 (src/wl/window.rs key dispatch handler) **没做这个防御**, key event
直接走 alacritty Term 路径写 PTY. fcitx5 没消费 backspace + quill 没拦, 双重失误
造成 destructive 数据丢失.

## Goal

quill key dispatch 加 preedit_active guard. preedit 非空时**吞掉所有 key event 不送
PTY** (除了少数 IME 外的紧急 control 如 Ctrl+C 终止进程, 见 Out 段). 完工后 user
实测拼音输入中按 backspace, 即使 fcitx5 不响应, PTY 中的内容也不被误删.

## Scope

### In

#### A. preedit_active 状态访问
- src/wl/window.rs / src/ime/ 已有 IME state, 包含 preedit_text 字段 (T-0703 实装)
- key dispatch handler 拿到 IME state.preedit_text, 检查 is_empty()
- 加一个 helper method `IMEState::is_preedit_active() -> bool { !self.preedit_text.is_empty() }` (按 quill 风格放在 ime 模块)

#### B. key dispatch 加 guard (src/wl/window.rs)
- 找到 wl_keyboard Dispatch impl 的 KeyEvent::Press 分支
- 在已有的 keymap_lookup / send_to_pty 路径之前加判断:
  ```rust
  if data.state.ime_state.is_preedit_active() {
      tracing::trace!(
          target: "quill::keyboard",
          "key swallowed (preedit active) keysym={} evdev_keycode={}",
          keysym, evdev_keycode
      );
      return;
  }
  ```
- key release event 同样吞 (避免 release 引发奇怪状态)

#### C. 测试 (src/wl/window.rs::tests 或 src/ime/mod.rs::tests)
- `key_swallowed_when_preedit_active` — mock IME state preedit_text="某拼音", 模拟
  KeyPress event (任何 key), 验证 PTY write buffer 仍然空 + log trace 含 "swallowed"
- `key_passed_to_pty_when_preedit_empty` — mock preedit_text="" + KeyPress, 验证
  PTY write buffer 收到对应 byte (维持原行为)
- `key_release_swallowed_when_preedit_active` — release 也吞

### Out

- **Ctrl+C / Ctrl+\ 等紧急 control 不防御** — 这些是终端必需的进程控制, 即使 IME
  在用也应该让 user 中断 PTY 进程. 实装时排除 keysym Control_L+c / Control_L+backslash
  (硬编码白名单, 后续如果 user 反馈再扩) 或者更简单: **本 ticket 范围只防 backspace
  + arrow keys + delete 等"删除 / 编辑 PTY 历史"的 key**, 普通字母数字 / Ctrl+C
  仍透传. 让 writer judge 边界, 倾向更严格 (preedit 时全吞), 用户体验后续再调.
- 修 fcitx5-rime 配置让 Caps Lock 切英文时 commit 已选词 — 上游配置问题, user 自己
  改 default.custom.yaml. 不在 quill scope.
- IME 内部状态机重写 — 不在 scope.

## Acceptance

1. `cargo test --lib window::` (或相应模块) + `cargo test --lib ime::` 全过 (含新增 3 测试)
2. `cargo clippy --all-targets -- -D warnings` 无 warning
3. `cargo fmt --check` 跳过 main 既有漂移
4. user 实测: 拼音输入 `nihao` → preedit "ni hao" → 按 Backspace 多次 → PTY 中之前
   的内容**不丢**, IME preedit 不变 (符合 fcitx5 不响应的现象). user 按 Esc 取消
   preedit 后, backspace 恢复送 PTY 删字符.

## INV-010

- IMEState::is_preedit_active 是 quill 自定义 method, 返 bool, 不暴露 fcitx5 / wayland
  protocol 类型
- guard 实现纯 quill 内部状态机判断, 不引入新依赖

## 相关

- T-0703 (IME preedit state 实装, 提供 preedit_text 字段)
- memory `input_method_setup.md` (user fcitx5-rime 配置: Shift_L/R noop, Caps_Lock
  默认 inline_ascii, 触发本 bug 的上游配置)
- text-input-v3 spec: client 负责 key event 路由判断
- alacritty 对照: alacritty 同样有 preedit guard, src/event.rs 类似位置
