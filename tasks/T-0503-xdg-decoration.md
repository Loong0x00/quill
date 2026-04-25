# T-0503 xdg-decoration server-side (最小化/最大化/关闭)

**Phase**: 5
**Assigned**: (open)
**Status**: open
**Budget**: tokenBudget=70k (小中, 单 protocol 接入 + 单文件 src/wl/window.rs)
**Dependencies**: Phase 1-4 全合
**Priority**: P1 (没装饰窗口拖不动 / 关不掉, 但功能不影响)

## Goal

接 `xdg-decoration-unstable-v1` 协议请求 server-side decoration (SSD), 让
GNOME / KDE 等 compositor 自己画 titlebar + 最小化/最大化/关闭按钮 + 边框
拖动. 完工后 quill 窗口跟正常 GNOME 程序一样有 titlebar, 能用按钮/快捷键
最小化最大化关闭.

**为啥 SSD 而非 CSD**: client-side decoration 要自己画 titlebar + 处理鼠标
hit-test + 拖动逻辑, 100+ 行复杂代码. SSD 让 compositor 画, 我们只发请求,
~30 行. 唯一缺点: 部分极简 compositor (sway 默认) 不支持 SSD, 但用户用 GNOME
支持没问题.

## Scope

### In

#### A. 检查 sctk 是否提供 xdg-decoration 封装
- smithay-client-toolkit 0.19+ 提供 `WindowDecorations` enum 跟 SSD 请求 API
- 如果 sctk 已封装 → 直接用 (推荐)
- 如果没封装 → 引 `wayland-protocols = "0.32"` 走 ZxdgDecorationManagerV1
- **决定权 writer**: 看现有 sctk 版本, 选最简路径, 写 ADR 0007 if 引新 crate

#### B. src/wl/window.rs surface init 加 SSD 请求
- `WaylandState::new` 或 surface init 处:
  - bind XdgDecorationManager (sctk 自动 / 手动 registry)
  - 对 xdg_toplevel 调 set_mode(ServerSide)
- compositor 不支持 → 收到 unsupported event → 退化 CSD 自画 (本单 Out, 仅 log warn)

#### C. (可选) 加 ESC / Ctrl+Q 关窗口快捷键
- 这跟 T-0501 wl_keyboard 联动, 不在本单 scope, 留 Phase 6+

#### D. 测试
- `tests/xdg_decoration.rs`:
  - 单测验 ZxdgDecorationManagerV1 bind 成功 (mock registry)
  - 单测验 set_mode(ServerSide) 调用一次
  - 集成测试不易 (需真 GNOME compositor), Lead 手测验

### Out

- **不做**: client-side decoration 自画 (SSD 退化时仅 log warn, 用户用 GNOME 不需)
- **不做**: titlebar 自定义颜色 / 文字 (compositor 默认就行)
- **不做**: 窗口图标 (Phase 6+)
- **不动**: src/text / src/pty / src/wl/render.rs / docs/invariants.md
- **不引新 crate** 除非 sctk 没封装 (那时引 wayland-protocols + 写 ADR 0007)

### 跟其他并行 ticket 的协调

- T-0501 (wl_keyboard) 改 src/wl/window.rs 但 wl_seat 段, 跟 decoration init 段不冲突
- T-0502 (set_buffer_scale) 改 src/wl/window.rs surface init 段, **可能小冲突**, git 自动 merge 多半 OK

## Acceptance

- [ ] 4 门 release 全绿
- [ ] xdg-decoration ServerSide 请求成功 (log 里看 compositor reply)
- [ ] (如果引 wayland-protocols) ADR 0007 落盘
- [ ] 总测试 124 + ≥1 ≈ 125+ pass
- [ ] **手测 deliverable**: cargo run --release 窗口有 GNOME titlebar + 3 按钮 (最小化/最大化/关闭)
- [ ] 关闭按钮真退出 quill (xdg_toplevel close event 已处理? 若否一并接)
- [ ] 审码放行

## 必读 baseline

1. /home/user/quill-impl-decoration/CLAUDE.md
2. /home/user/quill-impl-decoration/docs/conventions.md
3. /home/user/quill-impl-decoration/docs/invariants.md
4. /home/user/quill-impl-decoration/Cargo.toml (看 sctk 版本)
5. /home/user/quill-impl-decoration/src/wl/window.rs (现状 surface init + xdg_toplevel handler)
6. WebFetch https://wayland.app/protocols/xdg-decoration-unstable-v1
7. WebFetch https://docs.rs/smithay-client-toolkit/latest/smithay_client_toolkit/ (查 WindowDecorations / xdg-decoration 封装)

## 已知陷阱

- sctk 0.19 `Window::request_decorations(WindowDecorations::ServerDefault)` 是推荐 API
- compositor 可能 reply ClientSide unsupported, 必须处理 — 否则窗口无装饰看起来怪
- 关闭按钮: xdg_toplevel close event → main loop 退出, 现有代码可能没接, 一并加
- 不要 git add -A
- HEREDOC commit
- ASCII name writer-T0503

## 路由

writer name = "writer-T0503"。

## 预算

token=70k, wallclock=1.5h. 完工 SendMessage team-lead 报 4 门 + diff stat + 手测描述。
