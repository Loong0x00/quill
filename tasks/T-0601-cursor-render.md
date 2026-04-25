# T-0601 光标渲染 (cursor block / underline)

**Phase**: 6 (daily-drive polish)
**Assigned**: writer-T0601
**Status**: merged
**Budget**: tokenBudget=80k (单文件 src/wl/render.rs + Term::cursor 接入)
**Dependencies**: T-0301 (alacritty_terminal Term) / T-0407 (cell pipeline) / T-0408 (headless screenshot)
**Priority**: P0 (user 实测反馈窗口缺光标, daily-drive 体感不对)

## Goal

接 alacritty_terminal `Term::cursor()` 拿当前 (col, line) + style → quill 在
draw_frame 里画 cursor block (反色填充) 在 cursor cell 位置. 完工后用户在
quill 窗口里能看到光标位置 (跟 alacritty / foot 等终端一致).

## Scope

### In

#### A. src/term/mod.rs 加 TermState::cursor_pos (如果还没)
- 检查 src/term/mod.rs 是否已暴露 cursor_pos / cursor_style API
- T-0505 实装时已用 `t.cursor_pos()` (window.rs 内 IME cursor_rectangle 路径), 应已有
- 如果只有位置没 style: 加 `cursor_style() -> CursorStyle` enum (Block / Underline / Bar 三种, alacritty_terminal 默认 Block)
- INV-010 type isolation: CursorStyle 是 quill 自有 enum, 不暴露 alacritty_terminal::ansi::CursorShape 类型

#### B. src/wl/render.rs::draw_frame 加 cursor 渲染
- draw_frame 接受 cursor 入参 (CursorInfo struct: { col, line, visible, style })
- 在 cell pipeline 同 RenderPass 内, cursor cell 位置画 cursor block:
  - Block style (默认): 整 cell 反色填充 (cell.bg ↔ cell.fg 互换), 字形仍渲染但走原 cell.fg 而 cursor 块走 cell.fg 当背景 — 实际简化: cursor 块走 fg 色, 字形走 bg 色 (或反色 swap)
  - Underline style: cursor cell 底部 2-3 px 横线 (用 cell.fg)
  - Bar style: cursor cell 左侧 2 px 竖线
- visible=false 跳过渲染 (alacritty Term::cursor 给 visible 字段, 闪烁 / hide 时为 false)

#### C. window.rs idle callback 调 draw_frame 时传 cursor
- t.cursor_pos() + t.cursor_style() → CursorInfo → draw_frame 入参
- IME 启用 + preedit 显示时 cursor 隐 (preedit 起点跟 cursor 同位置, 视觉冲突, fcitx5 风格是 cursor 隐 + preedit 显示)

#### D. 测试
- src/term/mod.rs 单测 cursor_pos / cursor_style 正确
- src/wl/render.rs 单测 hit_test 类似 — cursor block vertices 生成正确 (位置 + 颜色)
- tests/cursor_render_e2e.rs 集成测试: render_headless + cursor 在某位置 → PNG 验 cursor 区有 fg 色块

#### E. 三源 PNG verify (T-0408 SOP)
- writer 跑 /tmp/cursor_test.png 自验
- Lead 后续 Read PNG 第 2 源
- reviewer 第 3 源

### Out

- **不做**: 闪烁动画 (静态显示 block 即可, Phase 6+ 加 timerfd 闪烁)
- **不做**: 光标颜色自定义 (默认 cell.fg 即可)
- **不做**: 切换 Block/Underline/Bar 命令 (esc seq DECSCUSR, alacritty Term 自动收, 我们渲染端只读 style)
- **不动**: src/pty / src/text / docs/invariants.md / Cargo.toml

### 跟其他并行 ticket 的协调

- T-0602 (scrollback) 也改 src/wl/render.rs draw_frame, **不同段** (cursor 在 cell pipeline / scrollback 改 row 来源)
- T-0603 (keyboard repeat) 改 src/wl/keyboard.rs + main.rs, 不冲突
- 顶部 imports 可能小冲突, Lead 合并时手解

## Acceptance

- [ ] 4 门 release 全绿
- [ ] cursor block 在 cell cursor 位置可见 (反色)
- [ ] cursor visible=false 时不渲染 (闪烁 hide / IME preedit 显示时)
- [ ] 总测试 199 + ≥3 ≈ 202+ pass
- [ ] **手测 deliverable**: cargo run --release 看到光标位置闪 (静态 block 即可, 不闪烁)
- [ ] 三源 PNG verify
- [ ] 审码放行

## 必读 baseline

1. /home/user/quill-impl-cursor/CLAUDE.md
2. /home/user/quill-impl-cursor/docs/conventions.md
3. /home/user/quill-impl-cursor/docs/invariants.md (INV-001..010)
4. /home/user/quill-impl-cursor/tasks/T-0601-cursor-render.md (本派单)
5. /home/user/quill-impl-cursor/docs/audit/2026-04-25-T-0408-review.md (headless screenshot SOP)
6. /home/user/quill-impl-cursor/docs/audit/2026-04-25-T-0405-review.md (三源 PNG verify SOP)
7. /home/user/quill-impl-cursor/src/term/mod.rs (cursor_pos / Term wrapper)
8. /home/user/quill-impl-cursor/src/wl/render.rs (draw_frame cell pipeline)
9. /home/user/quill-impl-cursor/src/wl/window.rs (idle callback draw_frame 调用)

## 已知陷阱

- alacritty_terminal Term::cursor 返 Cursor struct, point + shape + visible
- Cursor visible 是 bool, 但闪烁需要 timerfd, 本单不接闪烁就当 always visible (除非 IME preedit)
- INV-010: alacritty_terminal::ansi::CursorShape 不出 src/term/mod.rs 模块边界, quill CursorStyle enum 包装
- 不要 git add -A
- HEREDOC commit
- ASCII name writer-T0601

## 路由

writer name = "writer-T0601"。

## 预算

token=80k, wallclock=1.5h.
