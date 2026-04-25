# T-0602 scrollback 滚动 (滚轮 + PageUp/PageDown)

**Phase**: 6
**Assigned**: writer-T0602
**Status**: merged
**Budget**: tokenBudget=150k (跨 wl_pointer Axis + keyboard PageUp/Down + Term::scroll_display + render rows 来源)
**Dependencies**: T-0501 (wl_keyboard) / T-0504 (wl_pointer) / T-0408 (headless screenshot)
**Priority**: P1 (user 实测反馈无法看历史, daily-drive 必需)

## Goal

接 wl_pointer Axis event (滚轮) + Wayland keyboard PageUp/PageDown → 调用
alacritty_terminal `Term::scroll_display(N)` 滚动 scrollback grid → render
时取 display rows (scrollback 偏移后) 而非 active rows. 完工后用户能滚轮
向上看历史输出 / PageUp 翻页.

## Scope

### In

#### A. src/wl/pointer.rs 加 Axis event 处理
- handle_pointer_event 加 wl_pointer::Event::Axis 分支
- 返 PointerAction::Scroll(delta_lines) — quill 自有 enum, delta>0 = 向上滚 (看更老历史), <0 = 向下滚 (看更新)
- wl_pointer Axis discrete 用 line 单位; 连续 axis (touchpad) 用 fixed point → 转 line
- 单测覆盖 axis event → PointerAction::Scroll(N) 决策

#### B. src/wl/keyboard.rs 加 PageUp/PageDown 处理
- terminal_keysym_override 加:
  - PageUp (0xff55) → KeyboardAction::Scroll(+rows/2)  (半屏)
  - PageDown (0xff56) → KeyboardAction::Scroll(-rows/2)
  - Shift+PageUp/PageDown → 整屏滚 (×2)
- 不发 PTY (传统终端 PageUp/Down 不写 stdin, 是终端自处理)
- 单测覆盖 PageUp keysym → KeyboardAction::Scroll(N) 决策

#### C. src/term/mod.rs 加 scroll_display API
- 检查 alacritty_terminal Term::scroll_display 是否已暴露
- 加 `TermState::scroll_display(delta: i32)` 包装
- 加 `TermState::display_offset() -> usize` (当前 scrollback 偏移)
- 加 `TermState::reset_display()` (跳到底部, 等同新输入到达时自动回底)

#### D. src/wl/render.rs 渲染 display rows 而非 active rows
- TermState::display_text(row) 替代 line_text(row), 内部加 scrollback 偏移
- 或: render_headless 接受 row_texts 不变, 调用方 (window.rs idle) 改用 display_text
- cursor 渲染时 — scrollback 时 cursor 不显示 (跟 alacritty 一致)

#### E. window.rs Dispatch 路径接入
- Dispatch<WlPointer> 处理 PointerAction::Scroll(N) → t.scroll_display(N)
- Dispatch<WlKeyboard> 处理 KeyboardAction::Scroll(N) → 同上
- PTY 收到新字节时自动 reset_display (跳到底部)

#### F. 测试
- src/wl/pointer.rs lib 测试 axis → Scroll(N)
- src/wl/keyboard.rs lib 测试 PageUp/PageDown → Scroll(N)
- src/term/mod.rs 测试 scroll_display 行为
- tests/scrollback_e2e.rs 集成测试: 喂 100 行字, scroll_display(50), render_headless PNG, 验顶部显示是 50 行前的内容

### Out

- **不做**: 鼠标拖选区滚动 (Phase 6+ wl_data_device)
- **不做**: 鼠标点击位置移光标 (跟 cursor 没关系, vim 模式自处理)
- **不做**: 滚动条渲染 (Phase 6+, 现在用滚动条空间太奢侈)
- **不做**: 滚轮加速 / 平滑滚 (整 line 跳即可)
- **不动**: src/pty / src/text / docs/invariants.md

### 跟其他并行 ticket 的协调

- T-0601 (cursor) 改 src/wl/render.rs draw_frame, **不同段** (cursor cell vs row 来源)
- T-0603 (keyboard repeat) 改 src/wl/keyboard.rs, **不同段** (PageUp keysym 处理 vs RepeatInfo)
- 顶部 imports 可能小冲突, Lead 合并时手解

## Acceptance

- [ ] 4 门 release 全绿
- [ ] 滚轮向上滚显示历史
- [ ] PageUp/PageDown 翻页
- [ ] 新输入自动 reset_display 到底
- [ ] 总测试 199 + ≥6 ≈ 205+ pass
- [ ] **手测 deliverable**: cargo run --release 跑大量字 (cat 长文件), 滚轮往上滚能看历史
- [ ] 三源 PNG verify
- [ ] 审码放行

## 必读 baseline

1. /home/user/quill-impl-scrollback/CLAUDE.md
2. /home/user/quill-impl-scrollback/docs/conventions.md
3. /home/user/quill-impl-scrollback/docs/invariants.md
4. /home/user/quill-impl-scrollback/tasks/T-0602-scrollback.md
5. /home/user/quill-impl-scrollback/docs/audit/2026-04-25-T-0501-review.md (keyboard SeatHandler)
6. /home/user/quill-impl-scrollback/docs/audit/2026-04-25-T-0504-review.md (pointer SeatHandler + handle_pointer_event)
7. /home/user/quill-impl-scrollback/docs/audit/2026-04-25-T-0408-review.md (headless screenshot)
8. /home/user/quill-impl-scrollback/docs/audit/2026-04-25-T-0405-review.md (三源 PNG SOP)
9. /home/user/quill-impl-scrollback/src/term/mod.rs (Term wrapper)
10. /home/user/quill-impl-scrollback/src/wl/keyboard.rs (terminal_keysym_override)
11. /home/user/quill-impl-scrollback/src/wl/pointer.rs (handle_pointer_event)
12. /home/user/quill-impl-scrollback/src/wl/render.rs (draw_frame + render_headless)
13. /home/user/quill-impl-scrollback/src/wl/window.rs (Dispatch impl)

## 已知陷阱

- alacritty_terminal Term::scroll_display 接受 i32 delta, 正向上 (看老内容)
- Term::display_offset 返当前偏移, 0 = 在底部 (active grid)
- wl_pointer Axis discrete value: 1 = 1 line scroll
- 触摸板 axis 连续 fixed point: discrete=0 axis 是连续, 累积 / 阈值转 line
- INV-010: alacritty_terminal Term API 不出 src/term/mod.rs 模块
- 不要 git add -A
- HEREDOC commit
- ASCII name writer-T0602

## 路由

writer name = "writer-T0602"。

## 预算

token=150k, wallclock=2-3h.
