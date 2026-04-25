# T-0504 CSD 自画 titlebar + 3 按钮 + wl_pointer 接入

**Phase**: 5 (CSD, GNOME 不支持 SSD 必须自画)
**Assigned**: (open)
**Status**: open
**Budget**: tokenBudget=200k (跨 wl_pointer 协议 + CSD 渲染 + hit-test + 测试)
**Dependencies**: T-0501 (wl_seat capabilities) / T-0503 (xdg-decoration ClientSide 路径) / T-0408 (headless screenshot)
**Priority**: P1 (user 用 GNOME, 没装饰窗口拖不了 / 关不掉只能 SIGTERM)

## Goal

接 `wl_pointer` 协议 + 自画 client-side decoration (titlebar 色块 + 3 按
钮 最小化/最大化/关闭) + hit-test 鼠标点击 → 触发 xdg_toplevel 协议
(`set_minimized` / `set_maximized` / `close`) + titlebar 拖动调用
`xdg_toplevel.move`. 完工后 user GNOME 下 cargo run --release 看到窗口
有正常的 titlebar + 3 按钮可点击 + 拖动 titlebar 移动窗口.

**为啥 CSD 而非 SSD**: T-0503 已验证 GNOME mutter 政策性不支持 SSD (CSD-only
设计哲学). user 主桌面 = GNOME, 必须自画.

## Scope

### In

#### A. 引 wayland-protocols 或用 sctk 已封装 wl_pointer
- `wl_pointer` 是 wayland 核心协议, sctk PointerHandler trait 应已封装
- 检查 sctk 0.19.2 是否已暴露 PointerHandler — 99% 是. 不引新 crate
- 如果需引新 crate, 写 ADR 0007

#### B. 新增 src/wl/pointer.rs
- `pub struct PointerState`: 包当前 pointer surface coords (f64, f64) + 当前
  hover 区域 (TitleBar / Button{Min,Max,Close} / TextArea / None)
- `pub fn handle_pointer_event(...) -> PointerAction` 纯逻辑决策
- `pub enum PointerAction`:
  - Nothing
  - StartMove (titlebar drag → xdg_toplevel.move)
  - ButtonClick(WindowButton) — Min / Max / Close
  - HoverChange(HoverRegion) — 触发 redraw 高亮按钮

#### C. src/wl/window.rs 接 wl_pointer
- SeatHandler::new_capability 监听 Pointer 出现 → bind wl_pointer (跟 keyboard 同套路)
- `Dispatch<WlPointer>` impl 转发 event 到 handle_pointer_event
- PointerAction 处理:
  - StartMove → `xdg_toplevel.move(seat, serial)`
  - ButtonClick(Close) → `xdg_toplevel.destroy()` 或 `loop_signal.stop()`
  - ButtonClick(Min) → `xdg_toplevel.set_minimized()`
  - ButtonClick(Max) → `xdg_toplevel.set_maximized()` (toggle)
  - HoverChange → request redraw (resize_dirty 类似)

#### D. CSD 渲染 (src/wl/render.rs)
- 加 `TITLEBAR_H_PX: u32 = 28` (logical px, ×HIDPI=2 = 56 physical) 常数
- `Renderer::draw_frame` 内 cell + glyph pipeline 之上加 **titlebar pipeline**:
  - 灰色矩形 backgroun 占顶部 28×width logical px (浅灰 #2c2c2c 类似 GNOME)
  - 3 个按钮区 (右上角): 最小化 / 最大化 / 关闭, 每个 24×24 logical px
  - 按钮 hover 时背景色变深 (浅灰 → 深灰)
  - 关闭按钮 hover 红 (~#e53935)
  - 按钮内画简单 icon (色块 line, e.g. 最小化 = 中间一横线, 最大化 = 矩形框, 关闭 = 叉)
- cell 区域起始 y 从 0 → TITLEBAR_H_PX (cell area 缩 28px logical)
- HIDPI_SCALE 适配 (titlebar physical = TITLEBAR_H_PX × HIDPI_SCALE)

#### E. resize 逻辑修
- `cells_from_surface_px` 把可用高度从 `height` 改为 `height - TITLEBAR_H_PX`
- propagate_resize_if_dirty 把 cell area 高度传给 term/pty

#### F. hit-test 决策
- 纯逻辑 fn `hit_test(x: f64, y: f64, surface_w: u32, ...) -> HoverRegion`
- 单测覆盖: titlebar / button min / button max / button close / text area / 边界
- conventions §3 抽决策模式 (跟 verdict_for_scale / decoration_log_decision 同套路)

#### G. 测试
- src/wl/pointer.rs lib 测试: handle_pointer_event 各 event 类型 → 期望 PointerAction
- src/wl/render.rs lib 测试: hit_test 决策表
- tests/csd_e2e.rs 集成测试: render_headless 写 PNG, 验 titlebar 区域有非清屏像素 + 3 按钮位置有非 titlebar bg 像素 (按钮可见)
- 三源 PNG verify SOP: writer + Lead + reviewer 都 Read PNG 验视觉

#### H. xdg-decoration log 更新 (T-0503 跟进)
- T-0503 在 ClientSide 退化时 warn "quill 不自画 CSD". T-0504 已自画, 改 log 为
  "quill 自画 CSD (titlebar + 最小化/最大化/关闭)"

### Out

- **不做**: titlebar 自定义颜色 / 字体 (硬编码 GNOME-like 即可)
- **不做**: 双击 titlebar 切最大化 (Phase 6+)
- **不做**: titlebar 显示窗口标题 (Phase 6+, 需要 xdg_toplevel.set_title 反向读)
- **不做**: 边框拖动 resize (Phase 6+, xdg_toplevel.resize 协议)
- **不做**: 鼠标滚轮选区滚动 (Phase 6+, wl_data_device)
- **不做**: 鼠标光标样式切换 (Phase 6+, wl_pointer.set_cursor)
- **不动**: src/text / src/pty / docs/invariants.md
- **不引新 crate** 除非 sctk wl_pointer 缺 (写 ADR 0007)

## Acceptance

- [ ] 4 门 release 全绿
- [ ] src/wl/pointer.rs 新文件 + handle_pointer_event 决策逻辑
- [ ] CSD titlebar + 3 按钮渲染在 src/wl/render.rs draw_frame
- [ ] hit_test 纯逻辑 fn + ≥6 单测 (覆盖 titlebar / 3 按钮 / text area / 边界)
- [ ] xdg_toplevel.move / set_minimized / set_maximized / close 触发路径打通
- [ ] cells_from_surface_px 减 TITLEBAR_H_PX 高度
- [ ] tests/csd_e2e.rs 集成测试 (render_headless PNG, titlebar 像素 verify)
- [ ] 总测试 149 + ≥10 ≈ 159+ pass
- [ ] **手测 deliverable**: cargo run --release 在 GNOME 下窗口有 titlebar + 3 按钮可点 + 拖动 titlebar 移动 + 关闭按钮真退出
- [ ] 三源 PNG verify (writer + Lead + reviewer)
- [ ] 审码放行

## 必读 baseline

1. /home/user/quill-impl-csd/CLAUDE.md
2. /home/user/quill-impl-csd/docs/conventions.md (5 步 + Option C squash + ASCII name + ADR 触发)
3. /home/user/quill-impl-csd/docs/invariants.md (INV-001..010, 特别 INV-010 type isolation)
4. /home/user/quill-impl-csd/tasks/T-0504-csd-titlebar-pointer.md (本派单)
5. /home/user/quill-impl-csd/docs/audit/2026-04-25-T-0501-review.md (wl_keyboard SeatHandler 套路, 你照搬给 wl_pointer)
6. /home/user/quill-impl-csd/docs/audit/2026-04-25-T-0503-review.md (xdg-decoration ClientSide 退化路径)
7. /home/user/quill-impl-csd/docs/audit/2026-04-25-T-0408-review.md (headless screenshot SOP, 集成测试模板)
8. /home/user/quill-impl-csd/docs/audit/2026-04-25-T-0405-review.md (三源 PNG verify SOP)
9. /home/user/quill-impl-csd/src/wl/keyboard.rs (KeyboardState + handle_key_event 模板, 你照写 PointerState)
10. /home/user/quill-impl-csd/src/wl/window.rs (SeatHandler + Dispatch<WlKeyboard> 模板, 你加 Dispatch<WlPointer>)
11. /home/user/quill-impl-csd/src/wl/render.rs (draw_frame cell + glyph pipeline 套路, 你加 titlebar pipeline)
12. /home/user/quill-impl-csd/Cargo.toml (sctk 0.19.2)
13. WebFetch https://wayland.app/protocols/wayland#wl_pointer
14. WebFetch https://wayland.app/protocols/xdg-shell#xdg_toplevel:request:move
15. WebFetch https://docs.rs/smithay-client-toolkit/0.19/smithay_client_toolkit/seat/pointer/

## 已知陷阱

- wl_pointer 坐标是 wl_fixed (f64), x = 鼠标 x 在 surface 内
- xdg_toplevel.move 必须传 wl_seat + serial (来自最近的 button event)
- titlebar 渲染要用独立 pipeline 还是 cell pipeline 复用? 推荐独立 (色块) 防 cell+glyph cache 污染
- 按钮 hover 高亮要 redraw 触发 → resize_dirty 类似的标志
- HIDPI: titlebar 物理 = logical × HIDPI_SCALE
- cells_from_surface_px 改后 cell 行数减少, term resize 路径要同步
- 不要 git add -A
- HEREDOC commit
- ASCII name writer-T0504

## 路由

writer name = "writer-T0504"。

## 预算

token=200k, wallclock=3-4h. 完工 SendMessage team-lead 报 4 门 + diff stat + 三源 PNG verify deliverable.
