# T-0701 窗口 4 边 + 4 角 resize 拖动

**Phase**: 7 (CSD 完整化)
**Assigned**: writer-T0701
**Status**: in-review
**Budget**: tokenBudget=120k (跨 wl_pointer hit-test + xdg_toplevel.resize 协议)
**Dependencies**: T-0504 (CSD titlebar + wl_pointer + hit_test)
**Priority**: P1 (CSD 完整性 — 当前窗口 size hardcode, 拖不动 resize)

## Goal

接 wl_pointer hit-test 窗口 4 边 + 4 角 (8 个 resize 区) → 鼠标按下时
调 `xdg_toplevel.resize(seat, serial, edge)` 让 compositor 处理 resize.
完工后用户能拖窗口边/角改大小, 跟正常 GNOME / KDE 窗口一致.

## Scope

### In

#### A. src/wl/pointer.rs hit_test 加 8 resize 区
- 当前 hit_test 只判 TitleBar / 3 按钮 / TextArea
- 加 ResizeEdge enum: Top / Bottom / Left / Right / TopLeft / TopRight / BottomLeft / BottomRight
- HoverRegion 加 ResizeEdge variant
- 边缘宽度 RESIZE_EDGE_PX = 4 logical (= 8 physical), 角落 corner = 8 logical (角覆盖范围比边大让用户好抓)
- 边缘检测: 顶部/左/右/底各 4 px 内是 edge (但避开 titlebar / 按钮区)
- 角落检测: 优先 corner (8×8 px), 否则 edge

#### B. PointerAction 加 StartResize(edge)
- 现有 PointerAction enum 加 StartResize(ResizeEdge) variant
- handle_pointer_event Pressed + ResizeEdge hover → 返 StartResize
- TitleBar drag 仍走 StartMove (T-0504 既定)

#### C. src/wl/window.rs Dispatch<WlPointer> 处理 StartResize
- ResizeEdge → wayland xdg_toplevel.resize(seat, serial, ResizeEdge)
- ResizeEdge enum 转 wayland_protocols::xdg::shell::client::xdg_toplevel::ResizeEdge:
  - Top → ResizeEdge::Top, BottomRight → ResizeEdge::BottomRight 等

#### D. 测试
- src/wl/pointer.rs lib 单测: hit_test 8 边角 → ResizeEdge 决策 (覆盖每个边角 + 边角交叠)
- src/wl/pointer.rs lib 单测: handle_pointer_event ResizeEdge hover + Pressed → StartResize action
- 集成测试可省 (resize 走 compositor side, mock 不易)

#### E. 手测 deliverable
- cargo run --release 在 GNOME 下拖窗口 4 边 + 4 角 → 窗口 resize, 跟 alacritty/foot 一致

### Out

- **不做**: cursor 形状切换 (T-0703 单)
- **不做**: 标题文字 (T-0702 单)
- **不做**: 双击 titlebar 切最大化 (Phase 7+)
- **不动**: src/text / src/pty / src/wl/render.rs / docs/invariants.md / Cargo.toml

### 跟其他并行 ticket 协调

- T-0702 (titlebar 标题) 改 src/wl/render.rs, 不冲突 (你不动 render)
- T-0703 (mouse cursor 形状) 改 src/wl/pointer.rs set_cursor 调用 + window.rs cursor theme 加载, 跟 hit-test 段不冲突 (你只改 hit_test + handle_pointer_event 决策)
- T-0604 (cell.bg + cursor inset) 改 src/wl/render.rs, 不冲突
- 顶部 imports 可能小冲突, Lead 合并时手解

## Acceptance

- [ ] 4 门 release 全绿
- [ ] hit_test 8 resize 区决策正确 (单测)
- [ ] StartResize → xdg_toplevel.resize 协议路径打通
- [ ] 总测试 265 + ≥10 ≈ 275+ pass
- [ ] **手测**: cargo run --release 拖窗口 4 边 + 4 角 → resize, GNOME mutter 实测
- [ ] 审码放行

## 必读

1. /home/user/quill-impl-resize/CLAUDE.md
2. /home/user/quill-impl-resize/docs/conventions.md
3. /home/user/quill-impl-resize/docs/invariants.md
4. /home/user/quill-impl-resize/tasks/T-0701-window-resize-edges-corners.md (派单)
5. /home/user/quill-impl-resize/docs/audit/2026-04-26-T-0504-review.md (CSD + wl_pointer 套路)
6. /home/user/quill-impl-resize/src/wl/pointer.rs (hit_test + handle_pointer_event)
7. /home/user/quill-impl-resize/src/wl/window.rs (Dispatch<WlPointer> + StartMove → xdg_toplevel.move 套路)
8. WebFetch https://wayland.app/protocols/xdg-shell#xdg_toplevel:request:resize
9. WebFetch https://docs.rs/wayland-protocols/latest/wayland_protocols/xdg/shell/client/xdg_toplevel/

## 已知陷阱

- xdg_toplevel.resize 接 wl_seat + serial + edge enum (跟 .move 同套路)
- ResizeEdge 是 wayland 协议 enum (8 variant + None), quill 自有 enum 翻译
- 边缘 hit-test 优先级: corner > edge > titlebar > button > textarea
- HIDPI: 边缘 4 logical = 8 physical, 实际拖区窄但 user 习惯 (GNOME 默认 4-6 px)
- 不要 git add -A
- HEREDOC commit
- ASCII name writer-T0701

## 路由

writer name = "writer-T0701"。

## 预算

token=120k, wallclock=2h.
