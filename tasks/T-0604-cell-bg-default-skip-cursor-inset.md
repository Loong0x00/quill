# T-0604 cell.bg default skip + CJK spacer + cursor inset (视觉对齐主流终端)

**Phase**: 6 (polish)
**Assigned**: writer-T0604
**Status**: in-review
**Budget**: tokenBudget=80k (单文件 src/wl/render.rs build_vertex_bytes / append_cursor_quads + 测试)
**Dependencies**: T-0407 (cell.bg 渲染路径) / T-0405 (CJK fallback) / T-0601 (cursor render)
**Priority**: P0 (user 实测视觉糟糕, 每字一块黑底, CJK 中间黑空隙, cursor 盖最后字)

## Bug

User 实测三个相关视觉 bug, 都跟主流终端 (alacritty / xterm / foot / kitty) 不一致:

### Bug 1: 每个 cell 都画黑色矩形
- alacritty `Term` 给 cell 默认 `bg = NamedColor::Background → quill DEFAULT_BG = #000000` 黑
- T-0407 D fix 让 draw_frame 走 cell.bg, 默认黑 cell 都画黑色矩形覆盖 #0a1030 清屏色
- 视觉上"每字一块黑底", user 截图 2026-04-26 08-08-40.png 实证

### Bug 2: CJK 字之间有黑色空隙
- alacritty 把 CJK 双宽字记成 (wide cell + WIDE_CHAR_SPACER cell), spacer cell 也有 default bg = 黑
- 渲染时 spacer cell 画黑矩形 → CJK 字之间 visually 黑空隙
- 跟 Bug 1 同源 (default bg 不该画)

### Bug 3: cursor 盖住最后字符右半
- T-0601 cursor render KISS 实心白方块覆盖整 cell
- 但字形 advance (cosmic-text 17pt DejaVu Sans Mono 实测 ~11 px) > hardcode CELL_W_PX (10 logical px)
- 字形像素溢出到下一 cell 边缘, cursor (在下一 cell, col=N+1) 左边缘正好覆盖溢出像素
- 视觉上 cursor 像盖住了字符右半 (实际 cursor.col 数据正确, vt100 协议正确, 是 cell 几何问题)
- User trace 验证: 输 'a' 后 cursor.col=18 (= prompt 17 + 1), 行为正确

## Goal

修三个相关视觉 bug, 让 quill 视觉跟主流终端一致:
1. cell.bg = DEFAULT_BG 时**跳过 vertex 生成**, 让 clear color 透出来
2. CJK spacer cell 同处理 (跟 Bug 1 同源, 自动修)
3. cursor cell **inset 1-2 logical px** (左/右边缘内缩, 不接触相邻 cell), 字形溢出像素不被 cursor 覆盖

完工后 user 实测 cargo run --release: 整片深蓝清屏色, 字直接画在上面, CJK 字间无黑空隙, cursor 紧贴最后字符但不盖字。

## Scope

### In

#### A. src/wl/render.rs::build_vertex_bytes 跳过 default bg
- 当前 build_vertex_bytes (CellColorSource::Bg 路径) 对每个 cell 生成 6 vertex 画矩形
- 加判断: `if cell.bg == crate::term::Color::DEFAULT_BG { continue; }` 跳过
- 也可走更准的判断 `if cell.bg == clear_color_rgb { continue; }` (配 clear color #0a1030)
  — 但 alacritty 标准是"default bg 不画", 跟具体 clear color 无关; 实装走 DEFAULT_BG
- WIDE_CHAR_SPACER cell 同走此路径 (spacer cell.bg 也是 default → 跳过, Bug 2 自动修)

#### B. cell.bg 路径 cursor cell **不**跳过
- cursor cell 仍画 cell.bg (默认黑, 但被 cursor block 覆盖)
- cursor 用反色填 (block 走 cell.fg = 浅灰), 实装时跟 append_cursor_quads 路径协同
- 实际 cursor cell 默认 bg 跳过后, cursor block 直接画在 clear color 上, 视觉对

#### C. src/wl/render.rs::append_cursor_quads cursor inset
- cursor cell 顶点 inset 2 physical px (= 1 logical) 左/右边缘
- `cell_x0 += inset_px, cell_x1 -= inset_px`
- block / hollow / underline / beam 都 inset, 字形溢出像素不被覆盖
- inset 量取 1 logical px (= 2 physical) 即可, 用户视觉无感

#### D. 测试
- src/wl/render.rs lib 测试: build_vertex_bytes 喂 default bg cell → 0 vertex; explicit bg cell → 6 vertex
- src/wl/render.rs lib 测试: append_cursor_quads cell_x0/x1 inset 验证
- tests/视觉 e2e 集成测试 (可选): render_headless PNG 验整片深蓝清屏 + 字符无黑底

#### E. 三源 PNG verify (T-0408 SOP)
- writer 跑 /tmp/t0604_test.png 自验
- Lead Read PNG 第 2 源
- reviewer 第 3 源

### Out

- **不做**: 校准 CELL_W_PX 到字体真实 advance (跨 render/window/term 大改, Phase 6+ 单独 ticket 如需)
- **不做**: explicit bg cell (ls --color, vim 高亮) 渲染优化
- **不做**: cursor 闪烁 (Phase 6+ 独立 timerfd ticket)
- **不动**: src/term/mod.rs (DEFAULT_BG 常数不动, 标识"alacritty 默认 bg" 语义保持) /
  src/pty / src/text / docs/invariants.md / Cargo.toml

## Acceptance

- [ ] 4 门 release 全绿
- [ ] cell.bg = DEFAULT_BG 时跳过 vertex (grep 验)
- [ ] cursor cell inset 1 logical px 左/右
- [ ] 总测试 257 + ≥3 ≈ 260+ pass
- [ ] **手测 deliverable**: cargo run --release 视觉 — 整片深蓝清屏 + 字无黑底 + CJK 无黑空隙 + cursor 不盖字
- [ ] 三源 PNG verify
- [ ] 审码放行

## 必读 baseline

1. /home/user/quill-impl-cellbg/CLAUDE.md
2. /home/user/quill-impl-cellbg/docs/conventions.md
3. /home/user/quill-impl-cellbg/docs/invariants.md
4. /home/user/quill-impl-cellbg/tasks/T-0604-cell-bg-default-skip-cursor-inset.md (本派单)
5. /home/user/quill-impl-cellbg/docs/audit/2026-04-26-T-0601-review.md (cursor render audit, KISS 偏离)
6. /home/user/quill-impl-cellbg/docs/audit/2026-04-25-T-0407-review.md (cell.bg 渲染路径来源)
7. /home/user/quill-impl-cellbg/docs/audit/2026-04-26-codebase-overview-review.md (整体审码)
8. /home/user/quill-impl-cellbg/docs/audit/2026-04-25-T-0408-review.md (headless screenshot SOP)
9. /home/user/quill-impl-cellbg/docs/audit/2026-04-25-T-0405-review.md (三源 PNG SOP)
10. /home/user/quill-impl-cellbg/src/wl/render.rs (build_vertex_bytes + append_cursor_quads)
11. /home/user/quill-impl-cellbg/src/term/mod.rs (Color::DEFAULT_BG 常数)

## 已知陷阱

- DEFAULT_BG 常数在 src/term/mod.rs::Color, render.rs 用 `crate::term::Color::DEFAULT_BG`
- 不能直接比较 cell.bg vs clear color, 走 DEFAULT_BG 比较 (alacritty 标准)
- cursor inset 量太大 (e.g. 4 px) 会让 cursor 看起来"飘", 1-2 px 即可
- inset 单位是 logical 还是 physical 注意 (HIDPI_SCALE × 2 转换)
- 不要 git add -A
- HEREDOC commit
- ASCII name writer-T0604

## 路由

writer name = "writer-T0604"。

## 预算

token=80k, wallclock=1.5h.
