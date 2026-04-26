# T-0703 mouse cursor 形状切换 (resize 边箭头 / titlebar 默认 / textarea I-beam)

**Phase**: 7
**Assigned**: writer-T0703
**Status**: merged
**Budget**: tokenBudget=180k (跨 wl_pointer.set_cursor + cursor theme 加载 + cursor-shape-v1 协议)
**Dependencies**: T-0504 (wl_pointer + hit_test) / T-0701 (resize hit-test)
**Priority**: P1 (CSD 完整性 — 鼠标 hover resize 边时应该变 ↔, hover textarea 时应该变 I-beam)

## Goal

接 wl_pointer.set_cursor + Wayland cursor-shape-v1 协议 (现代) 或 wl_shm
cursor theme (老 fallback), 让鼠标在不同 hover region 显示不同 cursor:
- titlebar / 按钮 → default 箭头
- TextArea → I-beam (text)
- ResizeEdge Top/Bottom → ↕ (north / south resize)
- ResizeEdge Left/Right → ↔ (east / west resize)
- ResizeEdge corners → ↘↙↗↖ (corner resize)

完工后用户鼠标 hover 不同区域看到对应 cursor 形状, 跟正常 CSD 窗口一致.

## Scope

### In

#### A. 引 cursor-shape-v1 协议 (推荐) 或 wl_cursor (fallback)
- **优先 wp_cursor_shape_v1**: 新协议 (2023+), 让 compositor 自己加载 cursor theme + 切形状, client 只需给 enum (Default / Text / NorthResize 等)
- compositor 不支持 cursor-shape-v1 → fallback wl_cursor (libwayland-cursor + Adwaita theme load + 自管 wl_buffer attach)
- 检查 sctk 0.19.2 是否封装 cursor-shape-v1, 没则引 wayland-protocols-misc + 写 ADR 0008
- (可选) wl_cursor fallback 可省 (现代 GNOME mutter 50.x / KDE Plasma 6 / wlroots 已支持 cursor-shape-v1)

#### B. src/wl/pointer.rs 加 cursor shape 状态
- `PointerState` 加 current_cursor_shape: WpCursorShape enum
- HoverRegion → CursorShape 翻译表 (TitleBar=Default / TextArea=Text / ResizeEdge::Top=NorthResize 等)
- HoverChange 时算新 cursor shape, 变化 → PointerAction::SetCursor(shape)

#### C. src/wl/window.rs Dispatch<WlPointer> 处理 SetCursor
- SetCursor → wp_cursor_shape_device_v1.set_shape(serial, shape) (协议调用)
- enter event 时 set 一次 default

#### D. 测试
- src/wl/pointer.rs lib 单测: HoverRegion → CursorShape 翻译表全覆盖
- 集成测试不易 (cursor 是 compositor 侧渲染), 单测 + 手测 verify

#### E. 手测 deliverable
- cargo run --release, 鼠标 hover titlebar / textarea / 4 边 / 4 角 — cursor 形状切换正常

### Out

- **不做**: 自定义 cursor 主题 (Phase 7+ config 时)
- **不做**: cursor 隐藏 (gaming / fullscreen, Phase 7+)
- **不做**: cursor 大小配置 (compositor 主题决定)
- **不动**: src/text / src/pty / src/wl/render.rs / src/wl/keyboard.rs / docs/invariants.md
- **不引新 crate** 优先 (sctk + wayland-protocols 现有), 引则写 ADR 0008

### 跟其他并行 ticket 协调

- T-0701 (resize 边角) 改 src/wl/pointer.rs hit_test + window.rs Dispatch StartResize, 跟你改 set_cursor 段**不同段**, 但**强相关** (你 hit_test → ResizeEdge → CursorShape::NorthResize 等). T-0701 完工后 ResizeEdge enum 已有, 你 mapping 用即可
- 实际依赖: T-0703 强依赖 T-0701 的 ResizeEdge enum. 实装策略:
  - 选项 A: 等 T-0701 合 main 后再派 T-0703 (串行)
  - 选项 B: 先实装 T-0703 不含 ResizeEdge (只 TitleBar=Default / TextArea=Text), Lead 合并 T-0701 时手加 ResizeEdge mapping (并行)
- T-0702 (titlebar 标题) 改 src/wl/render.rs, 不冲突
- T-0604 (cell.bg + cursor inset) 改 src/wl/render.rs, 不冲突

## Acceptance

- [ ] 4 门 release 全绿
- [ ] hover region → cursor shape 翻译路径打通
- [ ] cursor-shape-v1 协议 bind (compositor 支持, fallback log warn)
- [ ] 总测试 265 + ≥5 ≈ 270+ pass
- [ ] **手测**: cargo run --release, 鼠标 hover titlebar / textarea — cursor 切换 (4 边 / 4 角等 T-0701 合后才能完整测)
- [ ] 审码放行

## 必读

1. /home/user/quill-impl-cursor-shape/CLAUDE.md
2. /home/user/quill-impl-cursor-shape/docs/conventions.md
3. /home/user/quill-impl-cursor-shape/docs/invariants.md
4. /home/user/quill-impl-cursor-shape/tasks/T-0703-mouse-cursor-shape.md (派单)
5. /home/user/quill-impl-cursor-shape/docs/audit/2026-04-26-T-0504-review.md (PointerState + handle_pointer_event)
6. /home/user/quill-impl-cursor-shape/Cargo.toml (sctk 版本)
7. /home/user/quill-impl-cursor-shape/src/wl/pointer.rs (PointerState 现状)
8. /home/user/quill-impl-cursor-shape/src/wl/window.rs (Dispatch<WlPointer>)
9. WebFetch https://wayland.app/protocols/cursor-shape-v1
10. WebFetch https://docs.rs/wayland-protocols/latest/wayland_protocols/wp/cursor_shape/v1/

## 已知陷阱

- cursor-shape-v1 是 staging 协议 (相对 stable), 多数 compositor 已支持 (GNOME 45+ / KDE Plasma 6 / wlroots 0.18+)
- enter event 必须 set 一次 cursor (否则空白默认)
- set_shape 需要 serial (来自 enter event), 类似 xdg_toplevel.move 的 serial
- INV-010: WpCursorShape (wayland-protocols enum) 不出 src/wl/pointer.rs 模块, quill CursorShape enum 包装
- 不要 git add -A
- HEREDOC commit
- ASCII name writer-T0703

## 路由

writer name = "writer-T0703"。

## 预算

token=180k, wallclock=2-3h.
