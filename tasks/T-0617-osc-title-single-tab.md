# T-0617 OSC title + 单 tab 隐藏 tab bar + 死 fn 清理

**Phase**: 7+ (polish, daily-drive feel 最后一公里)
**Assigned**: writer-T0617
**Status**: in-review
**Budget**: tokenBudget=120k
**Dependencies**: T-0608 (multi-tab) / T-0615 (tab bar polish) / T-0610 part 2 (圆角)
**Priority**: P2 (daily-drive)

## Goal

3 件事一起干, 把 quill daily-drive 体验跟 ghostty / GNOME Terminal 对齐:

1. **OSC title** — shell `cd` / 跑命令时 titlebar 中央 + 当前 tab 标题动态更新 (现在永远 "quill")
2. **单 tab 隐藏 tab bar** — 只 1 个 tab 时不画 tab 条, 终端内容直接接 titlebar 下方 (跟 ghostty 一致)
3. **死 fn 清理** — Lead 预先 commit 已删 `append_border_vertices` 调用, fn 本身留着, 删了

## Bug / Pain

- **Title 静态**: shell prompt cd 切目录 / 跑长命令, ghostty 标题立刻更新, quill 永远显 "quill". 多 tab 时分不清哪个 tab 是哪个 session
- **永远占 tab 条**: 单 tab 时 tab 条还画一个孤零零的 + 按钮 + 空 tab 槽, 既冗余又少了 ~30 px 终端高度
- **Warning**: T-0617 Lead 预先去掉 border 调用 (commit X), 但 `append_border_vertices` fn 还在, 编译 warning

## Scope

### In

#### A. OSC title 接入

alacritty_terminal 的 `Term<L>::set_title()` 通过 `EventListener` trait 派发 OSC 0/1/2 (`\x1b]0;TITLE\x07` / `\x1b]2;TITLE\x07`). 现在 quill 用 `Term<VoidListener>`, 全 ignore.

实装:
- `src/term/mod.rs`: 加 `TermListener` 实现 `EventListener` trait, 接 `set_title(&self, title: &str)` 把 title 存到 `TermState` 内 `current_title: String` 字段
- `Term<TermListener>` 取代 `Term<VoidListener>`. listener 是 trait object, Cell 包装存 title (Term 的 listener 是 owned, 不能借)
- `TabInstance` 加 `title: String` 字段 (tab 各自独立 title), 默认 "" → fallback 显 "quill" 或 "~"
- `LoopData::active_tab` 把当前 tab title 同步到 `Renderer::title` (用于 titlebar 中央渲染)
- TTY 数据每次 dispatch 完, check active tab title 是否变, 变了就刷新 renderer

注意:
- alacritty_terminal `EventListener` trait 还有 `bell` / `clipboard_store` / `clipboard_load` / `write_to_pty` / `text_area_size_changed` / `cursor_blinking_changed` 这些 — 都用 `noop` impl 即可, 派单只关心 set_title
- listener 持有 `Rc<RefCell<...>>` 之类 共享态; 单线程 (calloop INV-005), 不需 Mutex

#### B. 单 tab 隐藏 tab bar

`Renderer::draw_frame` 里现在永远调 `append_tab_bar_vertices`. 改成:
- `if self.tab_count > 1 { append_tab_bar_vertices(...) }`
- terminal 渲染区高度: 现在 = surface_h - titlebar_h - tab_bar_h. 改成 `tab_bar_h = if tab_count > 1 { TAB_BAR_H } else { 0 }`
- titlebar 显示位置 / tab 渲染位置全靠这个高度算, 一处改全跟着对

注意:
- pointer hit_test 也要跟改 (单 tab 时 tab 区不存在, click 不应 fall-through 错位)
- 切回多 tab (Ctrl+T) 时 tab bar 立刻出现, 终端内容区压缩
- shell 已经 print 的内容不动 (alacritty term grid 不重排, 仅 viewport 缩)

#### C. 清理死 fn

- `src/wl/render.rs::append_border_vertices` (~1144 行) 整 fn 删
- `BORDER_PX` / `BORDER_COLOR` 两个 const 也删 (没人引)
- 单测如有 ref border 也清

#### D. 测试

- `tests/osc_title_e2e.rs` (新): spawn shell 后 send `printf '\\033]0;hello\\x07'` → poll → assert `LoopData.tabs[0].title == "hello"`
- `tests/single_tab_no_bar_e2e.rs` (新): 启 quill (单 tab) → render_headless PNG → 验顶部 ~tab_bar_h 区域是 titlebar bg 色不是 tab bar bg 色; 然后 Ctrl+T 加 tab → render → 验 tab bar 出现
- src/term/mod.rs lib 单测: TermListener::set_title 接收 OSC + 存到 state

### Out

- **不动**: bezier squircle (T-0617 不碰算法)
- **不动**: tab drag ghost (P3)
- **不动**: 双击选词 / URL ctrl+click (后续 ticket)
- **不动**: src/wl/keyboard / src/pty / src/ime (本 ticket 仅 src/term + src/wl/render + src/wl/window 内部)

## Acceptance

- [ ] 4 门 release 全绿
- [ ] OSC title work: `printf '\\033]0;test\\x07'` 后 titlebar 中央显 "test" (PNG verify)
- [ ] cd 切目录 zsh prompt 自动 set_title (实际 shell config 决定, 我们只接 OSC)
- [ ] 单 tab: 无 tab 条 (PNG verify)
- [ ] Ctrl+T 加第二 tab: tab 条立刻出现, 终端内容区压缩 (PNG verify before/after)
- [ ] Ctrl+W 关到剩 1 tab: tab 条消失 (PNG verify)
- [ ] `append_border_vertices` / `BORDER_PX` / `BORDER_COLOR` 全删, 0 warning
- [ ] 总测试 ≥ 470 (T-0616 后 465 + 5 新测)
- [ ] 三源 PNG verify (writer + Lead + reviewer)
- [ ] **手测**: cargo run --release 跑一阵 cd 切目录, title 同步; Ctrl+T/W 切 tab 条出现/消失流畅
- [ ] 审码放行

## 必读

1. `/home/user/quill-impl-osc-title/CLAUDE.md`
2. `/home/user/quill-impl-osc-title/docs/conventions.md`
3. `/home/user/quill-impl-osc-title/docs/invariants.md` (重点 INV-002 23 字段, 本 ticket 应**不加字段** — title 已存 Renderer)
4. `/home/user/quill-impl-osc-title/tasks/T-0617-osc-title-single-tab.md` (派单)
5. `/home/user/quill-impl-osc-title/src/term/mod.rs` (Term<VoidListener> 现状, TermState wrapper)
6. `/home/user/quill-impl-osc-title/src/wl/render.rs` (draw_frame, append_tab_bar_vertices, Renderer::title)
7. `/home/user/quill-impl-osc-title/src/tab/mod.rs` (TabInstance struct)

## 重点 review 提示

1. **Term listener 生命周期**: alacritty_terminal `Term<L>` 的 L 是 owned. 共享 title 状态需 `Rc<RefCell<String>>` 或类似. 注意 calloop INV-005 单线程 — 不需 Mutex
2. **EventListener trait 全 method 列表**: bell / clipboard_store / clipboard_load / write_to_pty / text_area_size_changed / cursor_blinking_changed — 全 noop 实装. 写在 docs 里 reviewer 不会 surprised
3. **OSC 0 / 1 / 2 区别**: OSC 0 = both icon name + window title; OSC 1 = icon name only; OSC 2 = window title only. quill 不区分 icon, 三个全当 set window title
4. **Title 截断**: 长 title (>80 字符) 截断显省略号, 防 titlebar overflow
5. **Per-tab title race**: switch tab (Ctrl+1-9) 后 renderer.title 立即同步到 active_tab.title, 否则切了但 titlebar 不变
6. **单 tab 隐藏 tab bar 的 hit_test 跟改**: pointer.rs hit_test 区域计算也要 tab_count == 1 跳过 tab area
7. **Ctrl+T/W 高度立即变化**: terminal grid 行数会变 (height ÷ cell_h), 需触发 PtyHandle::resize 通知子进程 SIGWINCH
