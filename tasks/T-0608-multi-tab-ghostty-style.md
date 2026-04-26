# T-0608 多 tab ghostty 风 (单进程多 PTY/Term, 标签条 UI, 拖拽换序)

**Phase**: 6+ (架构改动, daily-drive feel)
**Assigned**: writer-T0608
**Status**: in-review
**Budget**: tokenBudget=500k (跨 LoopData 重构 + 多 PTY fd 注册 + 多 Term
状态 + 标签条 UI + hit_test + 拖拽换序 + 切换键)
**Dependencies**: T-0504 (PointerState + hit_test) / T-0603 (keyboard
modifier) / T-0604 (cell.bg) / T-0607 (clipboard, 跨 tab 自动 work)
**Priority**: P0 (user 实测 daily drive 需求, ghostty 风顺眼)

## Goal

撕 CLAUDE.md "多标签 → tmux"早期非目标条款 (2026-04-26 user 否决, tmux
在 GUI 终端是叠床架屋), 走 ghostty 风 native multi-tab. 单进程多 PTY +
多 Term, 标签条 UI 在 titlebar 下面一行, 视觉跟 ghostty 截图同款 (左上
+ 按钮 / 中间 tab 列表带 close x / active 高亮)。

User 论点: "tmux 是 tty 时代单 stream 限制的产物, Wayland 窗口能开无
数, 在 GUI 终端再叠一个虚拟终端复用器是叠床架屋. 视觉 + 鼠标划拉一下比
敲命令快得多. ClaudeCode + 游戏 + CLI 启动器并行常态."

## Scope

### In

#### A. TabInstance 数据结构
- src/tab/mod.rs (新模块) 或 src/term/tab.rs:
  ```rust
  pub struct TabInstance {
      pub term: TermState,
      pub pty: PtyHandle,
      pub title: String,        // 跟 OSC 0/2 自动更新
      pub dirty: bool,          // per-tab dirty
      pub id: TabId,            // u64 单调递增, 拖拽换序识别用
  }
  ```
- TabId 单调递增 (NextTabId atomic counter), 拖拽换序时 anchor 不变

#### B. LoopData 重构
- 当前 LoopData.term: Option<TermState> + state.pty: Option<PtyHandle>
  → LoopData.tabs: Vec<TabInstance> + active_tab: usize
- 多 PTY fd 全部注册同 calloop EventLoop (INV-005 不破), 每个 PTY 有独
  立 RegistrationToken
- inactive PTY 仍 read 消费 (防管道 backpressure 卡 shell), term.advance
  在后台跑, 状态累积. 切回 active 时重画
- per-tab dirty: 仅 active tab dirty 触发渲染, inactive 累积不重画 (节省
  GPU)

#### C. 标签条 UI (titlebar 下面一行)
- 新增 TAB_BAR_HEIGHT_PX (~28 logical px, ghostty 风窄高), titlebar +
  tab_bar 共占顶部. cell 区起点 = titlebar_h + tab_bar_h
- tab_bar 渲染: 左侧 "+" 按钮 (新 tab), 中间 tab 列表横向排, 每个 tab:
  - title 文本 (cosmic-text shape, 截短显示)
  - close "x" 按钮 (右侧)
  - active 高亮 (cell.bg #404060 灰青底色, inactive 透明)
  - hover 高亮 (鼠标 hover 时 #303040 浅灰底色)
- 标签宽度自适应: 总宽 / tab 数, 上限 ~200 px, 下限 ~80 px (太挤截短 title)

#### D. hit_test 扩展 (T-0504 路径)
- HoverRegion 加 enum 分支: `TabBarPlus`, `Tab(usize)`, `TabClose(usize)`
- pixel_to_region 算法:
  - y 在 titlebar 区 → 走原 titlebar hit_test
  - y 在 tab_bar 区 → 算 x 落哪 tab + 是否 close `x` 区
  - y 在 cell 区 → TextArea (走 T-0607 selection)
  - y 在 surface 边 → ResizeEdge (走 T-0701)
- cursor_shape_for: TabBarPlus / Tab / TabClose → CursorShape::Default
  (跟 titlebar 同款, 不变 I-beam / resize)

#### E. PointerAction 扩展
- `NewTab` (点击 + 按钮)
- `SwitchTab(usize)` (点击 tab body)
- `CloseTab(usize)` (点击 x)
- `StartTabDrag(usize)` (按下 tab body, 准备拖拽)
- `TabDragMove(f32)` (拖动期间, x logical px)
- `EndTabDrag` (松开)

#### F. 拖拽换序 (drag-and-drop reorder)
- 按下 tab body → StartTabDrag, 记 origin_idx + drag_x_offset
- 拖动期间 (鼠标 motion):
  - 计算 cursor_x 落哪个 tab 槽位 (按 tab 平均宽度算 target_idx)
  - 视觉: 拖中 tab 跟随鼠标半透明显示, 其它 tab 留位置 + 滑动让位
- 松开 → reorder: tabs.swap(origin_idx, target_idx), active_tab 跟拖中
  tab id 同步更新 (用 TabId 锁定, 不依赖 idx)
- 阈值: 拖动 < 5 logical px → 视为 click 走 SwitchTab (区分 click vs drag)

#### G. 切换 / 创建 / 关闭键位
- `Ctrl+T` 新 tab
- `Ctrl+W` 关 active tab
- `Ctrl+Tab` 下一个 tab (循环)
- `Ctrl+Shift+Tab` 上一个 tab
- `Ctrl+1..9` 跳第 N 个 tab (alacritty / kitty 同款)
- 这些键位 quill 拦截不发 PTY (跟 T-0603 keyboard override 路径同套路)

#### H. tab title 自动跟 shell
- alacritty Term 已经解析 OSC 0 (icon name) / OSC 2 (window title) /
  OSC 1 (icon title), 直接读 `term.title()` 更新 TabInstance.title
- 默认 title = "shell" 或 PTY argv[0] basename
- title 改变 → tab_bar dirty (重画标签条)

#### I. close 流程
- CloseTab(idx) → 关 PTY fd (drop PtyHandle), term drop, tabs.remove(idx)
- 若 active_tab == idx → active 切到邻近 (idx > 0 ? idx - 1 : 0)
- 若 tabs.is_empty() → quit 整 quill (与单 tab 时关闭等价)
- PTY drop 自动发 SIGHUP 给 shell

#### J. 测试
- src/tab/mod.rs lib 单测:
  - TabInstance new/title 更新
  - Vec<TabInstance> swap reorder 后 active id 跟随
  - close 后 active 邻近选择 (idx > 0 / idx == 0 / 最后 1 个)
- src/wl/pointer.rs lib 单测:
  - hit_test 新 region 翻译 (TabBarPlus / Tab(N) / TabClose(N))
  - drag 阈值: < 5 px click vs ≥ 5 px drag
- src/wl/render.rs lib 单测:
  - tab_bar 渲染顶点 (active 高亮 / inactive / hover)
- 集成测试 tests/multi_tab_e2e.rs PNG verify:
  - 渲染 3 个 tab + 第 2 个 active, 截图 verify
  - "+" 按钮 + close x 视觉 verify
- 手测:
  - cargo run --release, Ctrl+T 开新 tab, 拖拽换序, 跨 tab 复制粘贴 (T-0607
    PRIMARY 自动 cross-tab)

### Out

- **不做**: 分屏 (CLAUDE.md 仍 Out, P3+)
- **不做**: tab 拖出独立窗口 (Phase 后期 P3)
- **不做**: tab 历史保留 (close 后不可恢复, 跟 ghostty 同款)
- **不做**: tab group / pinning (P3)

### 跟 T-0607 协调

- T-0607 实装 PRIMARY/CLIPBOARD, 自然跨 tab work (Wayland selection
  per-seat 全局共享)
- T-0608 改 LoopData.tabs 后 T-0607 已合的 selection state 需迁到 active
  tab — Lead 合并时手解, 或 T-0607 留个 hook (selection_state 在 LoopData
  level 不在 tab level, 切 tab 时清旧选区)

## Acceptance

- [ ] 4 门 release 全绿
- [ ] 多 tab 数据结构 + LoopData 重构 (单测覆盖)
- [ ] 标签条 UI 渲染 (PNG verify, ghostty 风视觉)
- [ ] hit_test 新 region 翻译 (单测)
- [ ] 拖拽换序 (单测 + 手测)
- [ ] 切换键位 Ctrl+T/W/Tab/Shift+Tab/1-9 (单测)
- [ ] tab title 跟 OSC 自动更新
- [ ] close 流程 + active 邻近切换 (单测)
- [ ] 总测试 320 + ≥15 ≈ 335+ pass
- [ ] **手测**: Ctrl+T 开 tab / 鼠标拖换序 / 跨 tab 粘贴 / Ctrl+W 关
- [ ] 三源 PNG verify
- [ ] 审码放行

## 必读

1. /home/user/quill-impl-multi-tab/CLAUDE.md (含本 ticket 否决"多标签 → tmux")
2. /home/user/quill-impl-multi-tab/docs/conventions.md
3. /home/user/quill-impl-multi-tab/docs/invariants.md (重点 INV-005 单 EventLoop, 多 PTY fd 仍同 loop)
4. /home/user/quill-impl-multi-tab/tasks/T-0608-multi-tab-ghostty-style.md (派单)
5. /home/user/quill-impl-multi-tab/docs/audit/2026-04-26-T-0504-review.md (PointerState + hit_test)
6. /home/user/quill-impl-multi-tab/docs/audit/2026-04-26-T-0603-review.md (keyboard modifier + override)
7. /home/user/quill-impl-multi-tab/src/wl/window.rs (LoopData 现状)
8. /home/user/quill-impl-multi-tab/src/wl/pointer.rs (hit_test 现状)
9. /home/user/quill-impl-multi-tab/src/wl/keyboard.rs (override 现状)
10. /home/user/quill-impl-multi-tab/src/wl/render.rs (titlebar 渲染参考)
11. /home/user/quill-impl-multi-tab/src/term/mod.rs (TermState API, 含 .title() OSC 解析)
12. /home/user/quill-impl-multi-tab/src/pty/mod.rs (PtyHandle 现状, 多实例 fd 注册)

## 已知陷阱

- **PTY fd 多实例**: 每个 tab 一个 PTY master fd, 全部 calloop insert_source.
  上限 file descriptor 软限 ~1024 默认, 50 个 tab 没问题. 但要确保 close
  tab 时 RegistrationToken 也 remove (calloop API: loop_handle.remove(token))
- **inactive PTY backpressure**: shell 写 PTY 太多 quill 不消费会卡, inactive
  也必须 read 消费. 直接 advance term 即可 (CPU 极低), 不渲染就行
- **OSC title 跟 tab id**: alacritty Term 的 title 是 active 概念, quill 多
  tab 各自维护 TermState 各自有 title, 不冲突
- **tab 切换时 cursor / IME**: 切 tab 后键盘焦点跟 IME 状态切到新 tab 的
  TermState. ime_state 应在 tab level (per-tab preedit), 不是全局. T-0505
  ime_state 现是 LoopData level, 改成 per-tab
- **键位冲突**: Ctrl+T / Ctrl+W 在 vim / shell 里有意义, 但 IDE 风优先 (跟
  ghostty / kitty / wezterm 同), 用户 vim 时按 Ctrl+T 进 quill tab 不进 vim
  tag. 这是 trade-off, 派单接受 — 用户后续可加 keybind config (T-0606+)
- **拖拽 click 区分**: < 5 logical px 视为 click, ≥ 5 视为 drag. 防"鼠标抖
  动误触 drag"
- **INV-010**: TabId 类型 quill 自有 (newtype u64), 不漏到 wayland 侧
- 不要 git add -A
- HEREDOC commit
- ASCII name writer-T0608

## 路由

writer name = "writer-T0608"。

## 预算

token=500k, wallclock=8h.
