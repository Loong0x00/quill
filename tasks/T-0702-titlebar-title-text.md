# T-0702 titlebar 标题文字渲染

**Phase**: 7
**Assigned**: writer-T0702
**Status**: claimed
**Budget**: tokenBudget=80k (单文件 src/wl/render.rs titlebar pipeline 加字形)
**Dependencies**: T-0504 (titlebar 渲染) / T-0405 (字形 atlas / shape_line)
**Priority**: P1 (CSD 完整性 — 正常窗口 titlebar 显示标题, quill 当前空白灰条)

## Goal

在 titlebar 中央渲染窗口标题文字 (默认 "quill" 或 cwd 或当前命令).
完工后用户看到 titlebar 不再是空灰条, 跟正常 GNOME / KDE 窗口一致.

## Scope

### In

#### A. src/wl/render.rs::append_titlebar_vertices 加字形
- 现 append_titlebar_vertices 只画 titlebar bg + 3 按钮 + button icons
- 加 title text rendering: 在 titlebar 中央 (居中或左对齐) 画 "quill" 字形
- 走现有 cosmic-text shape + glyph atlas 路径 (跟 cell glyph 同 pipeline)
- 字形颜色用 BUTTON_ICON 浅灰 #d3d3d3 (跟 button icon 同, titlebar bg 深灰对比清晰)
- 字号小一点 (titlebar 28 logical 高, 字形 14 logical px ≈ titlebar 一半)

#### B. src/wl/render.rs::Renderer 加 title 字段
- pub fn set_title(&mut self, title: String) 让上游传新标题
- 默认 "quill", 上游 (window.rs init / xdg_toplevel.set_title 调用时) 同步
- title 变化置 dirty 触发重画

#### C. (可选) src/wl/window.rs 同步 xdg_toplevel.set_title
- xdg_toplevel.set_title("quill") 已有 (T-0102), 加 renderer.set_title 同步
- Phase 7+ 加 cwd / 命令显示, 当前 hardcode "quill" 即可

#### D. 测试
- src/wl/render.rs lib 单测: titlebar 含 title text vertices (vertex count > base, 字形 quad 加进去)
- tests/titlebar_text_e2e.rs 集成测试: render_headless PNG, titlebar 中央有 fg 色像素 (字形)

#### E. 三源 PNG verify
- writer 跑 /tmp/t0702_test.png 自验
- Lead Read PNG 第 2 源
- reviewer 第 3 源

### Out

- **不做**: 标题动态切换 (cwd / 命令 watcher, Phase 7+ 单)
- **不做**: 标题居中 vs 左对齐 配置 (默认居中即可)
- **不做**: 标题字号配置 (hardcode 14 logical px)
- **不做**: emoji 标题字符 (T-0407 emoji 黑名单仍生效)
- **不动**: src/wl/pointer.rs / src/wl/keyboard.rs / src/text / src/pty / docs/invariants.md / Cargo.toml

### 跟其他并行 ticket 协调

- T-0701 (边角 resize) 改 src/wl/pointer.rs / window.rs, 不冲突 (你不动 pointer)
- T-0703 (mouse cursor 形状) 改 src/wl/pointer.rs / window.rs, 不冲突
- T-0604 (cell.bg + cursor inset) 改 src/wl/render.rs build_vertex_bytes / append_cursor_quads — 跟你改 append_titlebar_vertices **同文件不同段**, git auto-merge 多半 OK
- 顶部 imports / Renderer 字段可能小冲突, Lead 合并时手解

## Acceptance

- [ ] 4 门 release 全绿
- [ ] titlebar 中央显示 "quill" 浅灰字
- [ ] 总测试 265 + ≥3 ≈ 268+ pass
- [ ] **手测 deliverable**: cargo run --release 看 titlebar 显示 "quill" 标题
- [ ] 三源 PNG verify
- [ ] 审码放行

## 必读

1. /home/user/quill-impl-titlebar/CLAUDE.md
2. /home/user/quill-impl-titlebar/docs/conventions.md
3. /home/user/quill-impl-titlebar/docs/invariants.md
4. /home/user/quill-impl-titlebar/tasks/T-0702-titlebar-title-text.md (派单)
5. /home/user/quill-impl-titlebar/docs/audit/2026-04-26-T-0504-review.md (titlebar pipeline)
6. /home/user/quill-impl-titlebar/docs/audit/2026-04-25-T-0405-review.md (字形 + 三源 PNG SOP)
7. /home/user/quill-impl-titlebar/docs/audit/2026-04-25-T-0408-review.md (headless screenshot)
8. /home/user/quill-impl-titlebar/src/wl/render.rs (append_titlebar_vertices 现状 + cell glyph pipeline)
9. /home/user/quill-impl-titlebar/src/text/mod.rs (cosmic-text shape_line)

## 已知陷阱

- 字形渲染走现有 glyph atlas + draw_frame glyph pipeline (titlebar 字形跟 cell 字形共享 atlas, 减少 GPU upload)
- title 颜色 BUTTON_ICON #d3d3d3 跟 cell.fg 同, 浅灰
- title 居中算: x = (surface_w / 2) - (title_advance / 2), 走 shape_line 拿 advance
- 不要 git add -A
- HEREDOC commit
- ASCII name writer-T0702

## 路由

writer name = "writer-T0702"。

## 预算

token=80k, wallclock=1.5h.
