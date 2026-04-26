# T-0607 鼠标拖选 + 复制粘贴 (Linear + Block + 边缘滚屏 + PRIMARY/CLIPBOARD)

**Phase**: 6+ (daily-drive feel, 选择粘贴)
**Assigned**: writer-T0607
**Status**: in-review
**Budget**: tokenBudget=300k (跨 wl_data_device + zwp_primary_selection_v1
+ pointer drag + selection state + cell.bg render + bracketed paste +
auto-scroll Timer)
**Dependencies**: T-0504 (PointerState + handle_pointer_event) / T-0603
(keyboard modifier tracking) / T-0604 (cell.bg render path) / T-0602
(scroll_display)
**Priority**: P0 (user 实测 daily drive 必需)

## Goal

接 Wayland selection 协议 + 鼠标拖选 + 键位复制粘贴, 让 quill 跟主流终端
一致 (alacritty / foot / kitty 同款). 完工后 user 鼠标拖选文本 → 自动 PRIMARY
复制, 中键粘贴, Ctrl+Shift+C/V 走 CLIPBOARD, Alt+drag 矩形选择, 拖到
viewport 边缘自动滚屏。

## Bug / Pain

User 实测无法用鼠标选文本, 跟 IDE 工作流断层. 派 T-0607 一并接齐:
- 鼠标拖选 (cell-level, 滑到哪选到哪, **不 snap word/line**)
- 跨多行流式选择 (起点行右半 + 中间整行 + 终点行左半)
- 矩形选择 (Alt+drag, vim visual block 风)
- 拖到 viewport 上/下边缘 → viewport 自动滚 + 选区终点跟随
- PRIMARY (Linux 中键粘贴标准): 松开左键自动复制, 中键单击粘贴
- CLIPBOARD: Ctrl+Shift+C 复制选区, Ctrl+Shift+V 粘贴
- bracketed paste (DECSET 2004): 粘贴内容包 ESC[200~ ... ESC[201~ 让 shell
  区分粘贴 vs 真键入

## Scope

### In

#### A. SelectionState 状态
- src/wl/pointer.rs (或新 src/wl/selection.rs) 加 `SelectionState`:
  - `mode: SelectionMode { Linear, Block }`
  - `anchor: CellPos` (按下左键的起点 cell)
  - `cursor: CellPos` (当前鼠标 cell)
  - `active: bool` (按下未松开)
- 拖动期间 cursor 实时更新, anchor 不变
- 松开左键 → active=false, anchor/cursor 保留 (用于 Ctrl+Shift+C 后续复制)
- 新一次按下 → 清旧选区, 新 anchor

#### B. Modifier 检测 (Alt+drag → Block)
- 鼠标按下时查 keyboard 当前 modifier mask (T-0603 last_modifier_mask)
- Alt 按下 → SelectionMode::Block, 否则 Linear
- Modifier 状态 keyboard.rs → window.rs 走 LoopData 共享, 不跨模块直引

#### C. PointerAction enum 扩展
- `SelectionStart(CellPos, SelectionMode)`
- `SelectionUpdate(CellPos)`
- `SelectionEnd` (松开左键, 触发 PRIMARY 自动复制)
- `Paste(PasteSource { Primary, Clipboard })` (中键 → Primary)
- pixel_to_cell helper 把鼠标 logical px → CellPos (CELL_W_PX / CELL_H_PX)

#### D. 渲染 - 选中区域 cell.bg 反色
- src/wl/render.rs::build_vertex_bytes 收 SelectionState 入参 (Option)
- Linear 路径: `selected_cells_linear(anchor, cursor, cols)` 返 `impl Iterator<CellPos>`
  - 起点行 ≤ 终点行: 起点行 (anchor.col..cols) + 中间整行 (0..cols) + 终点行 (0..cursor.col+1)
  - 起点行 > 终点行: 反过来 (anchor 在下, cursor 在上)
- Block 路径: `selected_cells_block(anchor, cursor)` 返矩形 (min_row..max_row+1) × (min_col..max_col+1)
- 选中 cell 的 bg 反色: 复用 T-0604 cell.bg 路径, selected cell 强制画 bg=#ffffff (或 invert fg/bg)

#### E. 自动滚屏 (拖到边缘)
- 鼠标 y < 0 (titlebar 下边缘以上) 或 y > surface_h-1 → 启动 calloop Timer
- Timer fire 100ms 一次 → `term.scroll_display(±1)` + cursor 跟随更新
- 鼠标 y 回到 viewport 内 → cancel Timer
- Timer 单飞行模式 (T-0603 repeat_token / T-0802 pending_resize_followup 同套路), pending_autoscroll_timer: Option<RegistrationToken> in LoopData

#### F. PRIMARY selection (zwp_primary_selection_v1)
- bind manager + 给 wl_seat create primary device
- 松开左键 → SelectionEnd → 算出选中文本 (走 D 同款 cells 迭代 → row text → join)
  - Linear: 起点行 trail + 中间行整行 + 终点行 lead, 行间 \n
  - Block: 每行 (col_min..col_max+1), 行间 \n (按列剪)
- 创建 wp_primary_selection_source, 提供 text/plain;charset=utf-8 + UTF8_STRING + TEXT
- set_selection on device with serial
- sctk 0.19.2 不一定封装 → 直接 wayland-protocols 拉 wp/primary_selection/v1, 写 ADR 0009 不引新 crate

#### G. CLIPBOARD (wl_data_device)
- bind wl_data_device_manager (sctk 0.19.2 已封装)
- Ctrl+Shift+C → SelectionEnd 同款文本 → wl_data_source set + set_selection on data_device
- Ctrl+Shift+V → 当前 wl_data_offer 检索 text/plain mime → read pipe fd → bracketed paste 包装写 PTY
- 中键单击 → PRIMARY 同款 read pipe fd 路径

#### H. Bracketed paste
- 检查 term `is_bracketed_paste_enabled()` (alacritty Term `mode().contains(TermMode::BRACKETED_PASTE)`)
- 启用时粘贴内容前后包 \x1b[200~ ... \x1b[201~
- 不启用 (默认 cat / dumb shell) 直接写原文

#### I. 测试
- src/wl/pointer.rs lib 单测:
  - SelectionState transition (start/update/end/clear)
  - selected_cells_linear (单行 / 跨 2 行 / 跨 N 行 / anchor 在 cursor 之后)
  - selected_cells_block (1×1 / N×M / negative direction)
  - pixel_to_cell 边界 (px=0 / px=cell_w-1 / px=surface_w 越界)
  - modifier_to_selection_mode (Alt → Block / 无 modifier → Linear)
- src/wl/render.rs lib 单测:
  - build_vertex_bytes 含 SelectionState 时 selected cells 走反色 path
- 集成测试:
  - tests/selection_e2e.rs PNG verify: 模拟 SelectionState 渲染, 选中区域反色像素 count
- 手测 deliverable:
  - cargo run --release 拖选 / Alt+拖选 / 中键粘贴 / Ctrl+Shift+C/V

### Out

- **不做**: 双击选词 / 三击选行 (user 明确 "不要 iPhone 自作主张 snap")
- **不做**: 跨 scrollback 选择跟随历史 (P2 边界多)
- **不做**: DnD 拖文件入终端 (P3)
- **不动**: src/text / src/pty / src/ime / docs/invariants.md (新增 INV 走另派)

## Acceptance

- [ ] 4 门 release 全绿
- [ ] 鼠标拖选 cell-level 滑到哪选到哪 (单测验)
- [ ] 跨多行流式选择 (单测 + PNG 验)
- [ ] 矩形选择 Alt+drag (单测 + PNG 验)
- [ ] 边缘自动滚屏 (Timer 单飞行)
- [ ] PRIMARY 自动复制 + 中键粘贴 (Wayland 协议路径打通)
- [ ] CLIPBOARD Ctrl+Shift+C/V (协议路径)
- [ ] Bracketed paste 跟随 term mode
- [ ] 总测试 310 + ≥10 ≈ 320+ pass
- [ ] **手测**: 拖选 / 跨行 / Alt 矩形 / 边缘滚 / 中键 / Ctrl+Shift+C/V 全顺
- [ ] 三源 PNG verify (writer + Lead + reviewer)
- [ ] 审码放行

## 必读

1. /home/user/quill-impl-mouse-selection/CLAUDE.md
2. /home/user/quill-impl-mouse-selection/docs/conventions.md
3. /home/user/quill-impl-mouse-selection/docs/invariants.md (INV-005 单 EventLoop / INV-010 type isolation)
4. /home/user/quill-impl-mouse-selection/tasks/T-0607-mouse-selection-clipboard.md (派单)
5. /home/user/quill-impl-mouse-selection/docs/audit/2026-04-26-T-0504-review.md (PointerState + handle_pointer_event)
6. /home/user/quill-impl-mouse-selection/docs/audit/2026-04-26-T-0603-review.md (keyboard modifier tracking)
7. /home/user/quill-impl-mouse-selection/src/wl/pointer.rs (PointerState 现状)
8. /home/user/quill-impl-mouse-selection/src/wl/keyboard.rs (modifier mask)
9. /home/user/quill-impl-mouse-selection/src/wl/render.rs (cell.bg path)
10. /home/user/quill-impl-mouse-selection/src/wl/window.rs (LoopData / 协议 dispatch)
11. WebFetch https://wayland.app/protocols/primary-selection-unstable-v1
12. WebFetch https://docs.rs/smithay-client-toolkit/0.19.2/smithay_client_toolkit/data_device_manager/index.html
13. /home/user/quill-impl-mouse-selection/Cargo.toml (sctk / wayland-protocols 现状)

## 已知陷阱

- **Wayland selection 异步**: set_selection 后 compositor 可能要 round-trip
  才生效, 测试不易 (集成测要起 wayland mock compositor 太重), 单测覆盖纯 fn
  决策 (cells_iter / mode 切换 / paste 内容包装) + 手测验真协议
- **PRIMARY 协议非 stable**: zwp_primary_selection_v1 是 unstable, 多数
  compositor 已支持 (GNOME mutter 45+ / KDE Plasma 6 / wlroots), wayland-protocols
  crate features 启 unstable. compositor 不支持 fallback 仅 CLIPBOARD + 警告 log
- **bracketed paste 跟 alacritty Term mode 联动**: shell 启动后 readline /
  zsh 自动发 ESC[?2004h 启 bracketed paste, 检查 term.mode() 即可, 不走自维护
  bool
- **Modifier mask cross-module**: keyboard.rs 跟踪 mask, pointer.rs 按下时
  读, 走 LoopData 字段共享 (state.keyboard_state.last_modifier_mask), 不直
  引模块
- **拖动 + 边缘 + 滚动 race**: 自动滚屏 Timer fire 时主线程可能在处理
  PointerEvent::Motion, 走 calloop 单线程 EventLoop 自然串行 (INV-005), 无
  需 mutex
- **Block 选择空格 trim**: 复制 Block 文本时尾随空格 trim 还是保留, alacritty
  默认 trim, kitty 默认保留. 派单选 **trim** (用户期望"剪表格列"行为)
- **INV-010**: wp_primary_selection / wl_data_offer 等 wayland 类型不出
  src/wl/{pointer,window}.rs 模块, quill 自有 SelectionState / PasteSource
  enum 包装
- 不要 git add -A
- HEREDOC commit
- ASCII name writer-T0607

## 路由

writer name = "writer-T0607"。

## 预算

token=300k, wallclock=5-6h.
