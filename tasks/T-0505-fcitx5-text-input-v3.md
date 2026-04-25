# T-0505 fcitx5 text-input-v3 (中文输入法)

**Phase**: 5 (Phase 5 最后一单)
**Assigned**: (open)
**Status**: open
**Budget**: tokenBudget=200k (跨 protocol bind + IME 状态机 + preedit 渲染 + cursor rect + 测试)
**Dependencies**: T-0501 (wl_keyboard, IME bypass 时 fallback 路径) / T-0408 (headless screenshot) / T-0504 (CSD 并行, 都改 src/wl/window.rs 不同段)
**Priority**: P1 (中文输入是 user daily-drive 必需, 但 ASCII 已可用不算 P0)

## Goal

接 `text-input-unstable-v3` Wayland 协议, 让 fcitx5-rime 在 quill 窗口里
能输中文: preedit 显示候选, commit 转字节给 PTY, cursor_rectangle 上报让
fcitx5 候选框定位正确. 完工后 user 在 quill 窗口里能切 fcitx5 输中文段落.

**派单 ROADMAP 原文 Phase 5 关键 ticket**:
- T-0501 wayland-scanner 生成绑定 (本单 — 因 sctk 不封装 text-input-v3)
- T-0502 ZwpTextInputV3 绑主 surface
- T-0503 preedit string 渲染
- T-0504 commit → PTY
- T-0505 cursor_rectangle 上报 (fcitx5 候选框定位)
- T-0506 焦点切换不丢 IME 状态
- T-0507 手测验证

本单**整合**这些路径 (sctk 已抽 wayland 通信, 不需独立 wayland-scanner ticket).

## Scope

### In

#### A. 引 wayland-protocols crate (ADR 0007)
- text-input-unstable-v3 是 Wayland unstable 协议, sctk **可能没封装** (查
  sctk 0.19.2 docs, 99% 没)
- 引 `wayland-protocols = "0.32"` (或 wayland-protocols-misc), feature 加
  `unstable` 让 ZwpTextInputV3 可用
- 写 docs/adr/0007-wayland-protocols-text-input-v3.md:
  - 为啥引 (text-input-v3 是 IME 唯一现代协议, 没封装必引)
  - 替代方案 (sctk 等待 / 手写 wayland-scanner / fcitx5 IPC 旁路)
  - feature flag (unstable / staging)

#### B. 新增 src/ime/ 模块
- `pub struct ImeState`: 持 ZwpTextInputManagerV3 + ZwpTextInputV3 (per-surface)
  + 当前 preedit (text + cursor + anchor) + 当前 cursor rect (x/y/w/h logical px)
- `pub fn handle_text_input_event(event, state) -> ImeAction` 纯逻辑决策
- `pub enum ImeAction`:
  - Nothing
  - UpdatePreedit { text, cursor, anchor } — preedit string 变化, 触发 redraw
  - Commit(String) — fcitx5 提交, 转 UTF-8 bytes 写 PTY
  - DeleteSurroundingText { before, after } — 前后删字符 (回退键交互)
  - EnterFocus / LeaveFocus — 切窗口
- 单测覆盖各 event 类型 → ImeAction 映射

#### C. src/wl/window.rs 接 ZwpTextInputV3
- registry 收到 zwp_text_input_manager_v3 → bind manager
- surface 创建后 manager.get_text_input(seat, surface) → ZwpTextInputV3
- `Dispatch<ZwpTextInputV3>` impl 转发 event 到 handle_text_input_event
- ImeAction 处理:
  - UpdatePreedit → 存 ImeState + request redraw (resize_dirty 类似)
  - Commit(s) → PtyHandle::write(s.as_bytes()) (跟 wl_keyboard 同 PTY 路径)
  - EnterFocus → text_input.enable() + commit (协议要求)
  - LeaveFocus → text_input.disable() + commit
- 加 `set_cursor_rectangle(x, y, w, h)` 调用让 fcitx5 候选框定位 (cell cursor 当前位置)

#### D. preedit 渲染 (src/wl/render.rs)
- preedit 在 cursor 当前 cell 起点之后绘制, 用下划线样式 (跟 fcitx5 主流 IME 风格一致)
- 加 `PREEDIT_UNDERLINE_PX: u32 = 2` 常数 (logical px, ×HIDPI=2)
- preedit 字形走现有 cosmic-text shape + glyph atlas 路径 (CJK fallback 已 work T-0405)
- preedit 颜色用 cell.fg (浅灰), 下方加 1 行下划线 (浅灰)
- redraw 频率: ImeAction::UpdatePreedit 触发 resize_dirty 类似的 preedit_dirty 标志

#### E. cursor_rectangle 上报 (派单 T-0505 原 ROADMAP)
- 用户敲 fcitx5 候选时, 候选框需要知道光标屏幕坐标
- 跟踪 cell cursor 位置 → 转 surface 坐标 (logical px) → text_input.set_cursor_rectangle(x, y, w, h)
- cursor 移动时 (PTY 输出新字符 / 用户用方向键) 更新

#### F. 焦点切换 (派单 T-0506 原 ROADMAP)
- wl_keyboard.enter / leave 跟 ZwpTextInputV3.enter / leave 同步
- 切窗口 (e.g. Alt-Tab) → text_input.disable + commit (释放 fcitx5 grab)
- 回 quill → text_input.enable + commit (重新 grab)

#### G. wl_keyboard 协调
- IME enabled 时 fcitx5 拦截 keyboard event, 不需 quill 自处理
- IME disabled 时走 wl_keyboard 路径 (T-0501 实装), 直接发 PTY
- 跟 wl_keyboard handler 协调: 如果 ImeState.enabled 且 keysym 不是 modifier/control,
  让 IME 处理; 否则走 keyboard 路径
- 实际 fcitx5 protocol 自己 grab 不需 client 判断 — 验证后决定

#### H. 测试 + 三源 PNG verify (派单 T-0507 原 ROADMAP)
- src/ime/mod.rs lib 单测: handle_text_input_event 各 event → ImeAction
- tests/ime_e2e.rs 集成测试: mock ZwpTextInputV3 event → 验 ImeAction → PtyHandle 收到 commit bytes
- tests/ime_preedit_render.rs: render_headless 模拟 preedit 状态, PNG 显示 preedit 文字 + 下划线
- 三源 PNG verify SOP: writer + Lead + reviewer Read /tmp/ime_test.png
- **集成测试不能真起 fcitx5** (CI 不可控), 用 mock event sequence

#### I. 手测验证 (派单 T-0507)
- writer 在自己环境用 fcitx5-rime 测一段中文输入, log + 截图描述 (本派单不强制)
- Lead 在 user GNOME 上手测最终验证 (合并后)

### Out

- **不做**: text-input-v1 / v2 (deprecated, 仅 v3)
- **不做**: zwp_input_method_unstable_v2 (这是 IME server 侧协议, 我们是 client)
- **不做**: 候选框自画 (fcitx5 自己画)
- **不做**: BiDi / RTL (Phase 6+)
- **不做**: 中日韩切换快捷键 (fcitx5 自处理)
- **不动**: src/text/mod.rs / src/pty/mod.rs (PtyHandle::write 已 T-0501 实装) /
  docs/invariants.md
- **不引新 crate** 除 wayland-protocols (写 ADR 0007)

### 跟其他并行 ticket 的协调

- T-0504 (CSD titlebar + wl_pointer) 也改 src/wl/window.rs, **不同段** (pointer vs
  text-input). 也都改 src/wl/render.rs (CSD titlebar pipeline vs preedit rendering),
  **不同段** (top titlebar vs cursor 当前 cell). git auto merge 多半 OK, 真冲突
  Lead 手解 (跟 T-0501/0502/0503 三并行经验同套路).
- T-0504 加 TITLEBAR_H_PX 减 cell area 高度. T-0505 preedit 渲染要 cell cursor
  坐标, 应该用 T-0504 修过的 cells_from_surface_px 算 cell 在 surface 内的位置.
  这个 dependency 通过 git merge 自动同步 — 谁先合 main 谁定 cell area baseline.

## Acceptance

- [ ] 4 门 release 全绿
- [ ] ADR 0007 wayland-protocols 落盘
- [ ] src/ime/ 新模块 + handle_text_input_event 决策逻辑
- [ ] ZwpTextInputV3 bind + Dispatch + Commit/Preedit/cursor_rectangle 路径打通
- [ ] preedit 渲染在 src/wl/render.rs cursor 当前 cell 之后 + 下划线
- [ ] 总测试 149 + ≥10 ≈ 159+ pass
- [ ] **手测 deliverable**: cargo run --release + fcitx5-rime 切中文 → 输 "你好"
      → preedit 显示候选 → 选中 → quill cell 真显示 "你好"
- [ ] 三源 PNG verify (writer + Lead + reviewer Read /tmp/ime_test.png)
- [ ] 审码放行

## 必读 baseline

1. /home/user/quill-impl-ime/CLAUDE.md
2. /home/user/quill-impl-ime/docs/conventions.md
3. /home/user/quill-impl-ime/docs/invariants.md (INV-001..010)
4. /home/user/quill-impl-ime/tasks/T-0505-fcitx5-text-input-v3.md (本派单)
5. /home/user/quill-impl-ime/docs/adr/0006-xkbcommon-keyboard-decoder.md (ADR 0007 模板)
6. /home/user/quill-impl-ime/docs/audit/2026-04-25-T-0501-review.md (wl_keyboard SeatHandler 套路)
7. /home/user/quill-impl-ime/docs/audit/2026-04-25-T-0408-review.md (headless screenshot SOP)
8. /home/user/quill-impl-ime/docs/audit/2026-04-25-T-0405-review.md (三源 PNG verify SOP)
9. /home/user/quill-impl-ime/src/wl/keyboard.rs (KeyboardState + handle_key_event 模板)
10. /home/user/quill-impl-ime/src/wl/window.rs (SeatHandler + Dispatch<WlKeyboard> 模板)
11. /home/user/quill-impl-ime/src/wl/render.rs (cell + glyph pipeline 套路)
12. /home/user/quill-impl-ime/src/text/mod.rs (cosmic-text shape + CJK fallback)
13. /home/user/quill-impl-ime/Cargo.toml (sctk 0.19.2)
14. WebFetch https://wayland.app/protocols/text-input-unstable-v3
15. WebFetch https://docs.rs/wayland-protocols/latest/wayland_protocols/wp/text_input/zv3/

## 已知陷阱

- text-input-v3 vs v1/v2: v3 是 atomic protocol (一组 event 后 done event 标志一帧),
  client 必须等 done 才应用所有变化 (preedit + commit + delete_surrounding 一起)
- preedit cursor / anchor 是 byte offset (UTF-8) 不是 char offset
- text_input.enable/disable 必须配 commit 协议要求
- cursor_rectangle 是 surface 坐标 (logical px), 不是 buffer 像素
- fcitx5 用 wl_keyboard grab + text-input-v3 双管协议 — 我们仅响应 text-input
- 焦点切换 race: enter/leave 可能跟 fcitx5 的 enable/disable 时序冲突
- 不要 git add -A
- HEREDOC commit
- ASCII name writer-T0505

## 路由

writer name = "writer-T0505"。

## 预算

token=200k, wallclock=4-5h. 完工最终响应回 Lead 报 4 门 + diff stat + ADR 路径 +
PNG deliverable + 偏离.

## 其他

如果 sctk 0.19.2 后续版本封装了 text-input-v3, 可以走 sctk 不引 wayland-protocols
— writer 决定. 但当前 (2026-04) sctk 99% 没.
