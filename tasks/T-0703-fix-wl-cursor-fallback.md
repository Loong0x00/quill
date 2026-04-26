# T-0703-fix wl_cursor fallback (绕开 mutter wp_cursor_shape_v1 mapping bug)

**Phase**: 7+ (polish, daily-drive feel)
**Assigned**: writer-T0703-fix
**Status**: in-review
**Budget**: tokenBudget=200k (sctk wl_cursor + xcursor theme load + cursor surface attach + 4 边 4 角 cursor 文件加载 + fallback 切换)
**Dependencies**: T-0703 (cursor-shape-v1 现状) / T-0701 (ResizeEdge enum)
**Priority**: P1 (user 实测 hover resize 边时 cursor 跟拖动接管时不一致, 视觉别扭)

## Bug

User 实测 hover 窗口 4 边 / 4 角时 cursor 形状是 wp_cursor_shape_v1 给的
"ns-resize / ew-resize / nwse-resize / nesw-resize" (Adwaita 主题对应
svg 文件), 但按住拖时 compositor 接管走 "size_ver / size_hor / size_fdiag /
size_bdiag" (X11 老 cursor name, 不同 svg 文件), **同一动作两套 cursor
来回切**, daily drive 看着别扭。

GTK4 / Qt / Electron / GNOME 自家应用全部走老 wl_cursor + xcursor theme
load 路径, 自动绕开了 mutter 在 wp_cursor_shape_v1 vs xdg_toplevel.resize
内部 mapping 不一致的实现 wart, **大家集体一致用 size_ver.svg 那套**。

quill 用了 sctk + wp_cursor_shape_v1 (2023 才稳的新协议), 撞上协议生态
过渡期。mutter 不会优先修 (上游不动力), quill 自己绕。

## Goal

走 wl_cursor + libwayland-cursor (sctk 已封装 cursor_theme + cursor_surface
模块) 加载 Adwaita 主题 cursor 文件, 自管 cursor surface attach. 完工后
hover / 拖动 cursor 视觉一致, 跟 GTK4 应用同款。

## Scope

### In

#### A. sctk cursor_theme load
- 检 sctk 0.19.2 是否封装 `cursor_theme` (sctk::seat::pointer::CursorTheme).
  如有: 直接用. 没则手 wrap libwayland-cursor (新 crate `wayland-cursor` 或
  写 ADR 0009 论证)
- 启动期加载默认 cursor theme (XCURSOR_THEME 环境变量 / 默认 Adwaita)
- size 走 XCURSOR_SIZE 环境变量, 默认 24

#### B. cursor surface 自管
- 给 wl_pointer 创建独立 wl_surface 作 cursor_surface
- HoverChange 时:
  - quill CursorShape → xcursor name (NorthResize → "ns-resize", NwseResize → "size_fdiag" 优先 / fallback "nwse-resize")
  - cursor_theme.get_cursor(name).image[0] → wl_buffer
  - cursor_surface.attach(buffer, 0, 0); cursor_surface.commit()
  - wl_pointer.set_cursor(serial, cursor_surface, hotspot_x, hotspot_y)

#### C. fallback name 列表 (老/新 cursor name 双轨)
- ns-resize / size_ver
- ew-resize / size_hor
- nwse-resize / size_fdiag
- nesw-resize / size_bdiag
- text / xterm
- default / left_ptr
按主流终端 (alacritty / foot / GTK4) 同款 fallback 顺序, 防止 theme 缺
新 name 时退化到 default

#### D. wp_cursor_shape_v1 是否完全删除
- 选项 A: 完全删除 wp_cursor_shape_v1 路径, 只走 wl_cursor (跟 alacritty
  / GTK4 同决策)
- 选项 B: wp_cursor_shape_v1 优先 + wl_cursor fallback (理论"兼顾未来",
  但实际 mutter 不修 mapping 也意义不大)
- **推 选项 A** (KISS, 保留两路径增加测试矩阵)

#### E. INV-010 类型隔离
- xcursor name str 是 quill 模块内 const, 不漏到 pointer.rs 公共 API
- WlBuffer / WlSurface 仍仅在 src/wl/{pointer, window}.rs 模块私有

#### F. ADR 0009 (若引新 crate)
- sctk 0.19.2 含 cursor_theme → 不引新 crate, ADR 不需
- 若需 wayland-cursor crate → 写 ADR 0009 论证 (sctk 不封装的 fallback)

#### G. 测试
- src/wl/pointer.rs lib 单测:
  - quill CursorShape → xcursor name 翻译表全覆盖
  - fallback name 列表顺序 (新 name miss → 老 name)
- 集成测试不易 (cursor 是 compositor 侧渲染), 单测 + 手测 verify
- 删 wp_cursor_shape_v1 相关测试 (Renderer init / wp_shape_for / 等等)

#### H. 手测 deliverable
- cargo run --release, 鼠标 hover 4 边 / 4 角 → cursor 跟按住拖动时同款
  (GNOME mutter 视觉 1:1)

### Out

- **不做**: 自定义 cursor 主题 (Phase 7+ config)
- **不做**: cursor 大小动态切 (用 XCURSOR_SIZE 环境变量决定, 改要重启)
- **不做**: cursor 隐藏 (gaming/fullscreen)
- **不动**: src/text / src/pty / src/wl/render.rs / src/wl/keyboard.rs / docs/invariants.md

## Acceptance

- [ ] 4 门 release 全绿
- [ ] hover 4 边 4 角 cursor 跟拖动接管时**视觉一致** (mutter 同款 svg)
- [ ] xcursor 翻译 + fallback 单测全覆盖
- [ ] wp_cursor_shape_v1 路径决策清晰 (删 / 留 fallback, 走 选项 A)
- [ ] ADR 0009 (若引新 crate, sctk 已封装则跳过)
- [ ] 总测试 310 + ≥5 ≈ 315+ pass
- [ ] **手测**: hover 4 边 / 4 角 / titlebar / textarea cursor 切换一致
- [ ] 审码放行

## 必读

1. /home/user/quill-impl-cursor-fallback/CLAUDE.md
2. /home/user/quill-impl-cursor-fallback/docs/conventions.md
3. /home/user/quill-impl-cursor-fallback/docs/invariants.md (INV-010)
4. /home/user/quill-impl-cursor-fallback/tasks/T-0703-fix-wl-cursor-fallback.md (派单)
5. /home/user/quill-impl-cursor-fallback/docs/audit/2026-04-26-T-0703-review.md (cursor-shape-v1 现状)
6. /home/user/quill-impl-cursor-fallback/docs/adr/0008-cursor-shape-v1.md (旧 ADR, 本单可能撤回部分决策)
7. /home/user/quill-impl-cursor-fallback/src/wl/pointer.rs (CursorShape / wp_shape_for 现状)
8. /home/user/quill-impl-cursor-fallback/src/wl/window.rs (cursor_shape_device dispatch)
9. /home/user/quill-impl-cursor-fallback/Cargo.toml (sctk 版本, 检查 cursor_theme 封装)
10. WebFetch https://docs.rs/smithay-client-toolkit/0.19.2/smithay_client_toolkit/seat/pointer/index.html (cursor_theme API)
11. WebFetch https://docs.rs/wayland-cursor (备选 crate)

## 已知陷阱

- **xcursor theme 加载失败**: theme 文件不在 /usr/share/icons/Adwaita/cursors/
  时退化到内置 cursor (sctk 一般 fallback 到 right_ptr 简单图标), tracing
  warn 一次不刷屏
- **HiDPI cursor**: XCURSOR_SIZE 默认 24, HiDPI 用户可能 set 32/48. cursor
  surface buffer scale 跟随 surface scale, 不要硬 1.0
- **enter event 必须 set_cursor**: 跟 wp_cursor_shape_v1 同样要求, enter
  后 quill 必 set 一次否则空白
- **cursor surface 释放**: 多 hover region 切换时复用同一 cursor_surface,
  每次 attach 新 buffer + commit. 不要每次创建 surface (协议不允许 + 性能差)
- **mutter cursor theme 路径**: GNOME 的 cursor theme 在 /usr/share/icons/Adwaita/
  cursors/ , 不是 /usr/share/cursors/. libwayland-cursor 自动用 XDG 标准
  路径搜索, 但用户自定义 theme (~/.icons/) 也要覆盖
- **INV-010**: xcursor name 是 const &str 模块私有, WlSurface / WlBuffer
  类型仍仅 src/wl/{pointer, window}.rs 模块私有
- 不要 git add -A
- HEREDOC commit
- ASCII name writer-T0703-fix

## 路由

writer name = "writer-T0703-fix"。

## 预算

token=200k, wallclock=3-4h.
