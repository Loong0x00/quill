# ADR 0008: 启 `wayland-protocols` `staging` feature 接 `wp_cursor_shape_v1` (T-0703)

## Status

**Superseded by ADR 0009 (T-0703-fix), 2026-04-26.**

原 Accepted 2026-04-26 (T-0703 合并). 同日上线后用户实测 mutter 在
wp_cursor_shape_v1 vs xdg_toplevel.resize 内部 cursor name 映射不一致 —
hover 用 ns-resize, 拖动用 size_ver, 同一交互期视觉两套 svg 切换. 撤回本
ADR, 改走 wl_cursor + xcursor theme 路径 (与 GTK4 / Qt / Electron / GNOME
自家 app 一致). 详见 `docs/adr/0009-wl-cursor-fallback.md`. 本 ADR 留作
历史记录 (CLAUDE.md ADR tombstone 准则), 不删.

---

(以下为原 Accepted 时记录, 仅供历史参考)

## Context

T-0703 给 quill 接 鼠标 cursor 形状切换 (resize 边角箭头 / titlebar 默认 / textarea
I-beam)。Wayland 时代有两条路径:

1. **传统 `wl_pointer.set_cursor` + wl_cursor + libwayland-cursor**: client 自己
   load cursor theme (XCursor / Adwaita), 自管 wl_buffer attach + damage + commit,
   每个 cursor shape 一份 buffer。**复杂** (theme 路径解析 / 主题热切换 / fallback
   chain / HiDPI scale 选 1x/2x/3x), 代码量 200+ 行 + libwayland-cursor C
   依赖。`alacritty` / `kitty` 走这条 (2018 年代实装)。

2. **现代 `wp_cursor_shape_v1` (cursor-shape-v1)**: 2023 staging 协议, client 只
   提交一个 enum (Default / Text / NorthResize 等), compositor 自己 load theme +
   attach buffer + 应用 HiDPI scale。client 仅 ~30 行实装。`foot 1.16+` /
   `Hyprland` / `cosmic-comp` / `wlroots 0.18+` 已用。

### compositor 支持矩阵 (2026-04 实测)

- ✅ **GNOME mutter 45+** (用户主桌面 GNOME 47.x)
- ✅ **KDE Plasma 6 / kwin 6+**
- ✅ **wlroots 0.18+** (sway / Hyprland / cosmic-comp 都基)
- ❌ **wlroots ≤ 0.17 / 老 sway** — 不导出 wp_cursor_shape_manager_v1 global,
  bind 失败 → log warn 退化到 "无 cursor 切换" (compositor 显示默认箭头, 视觉
  退化但功能不影响 — quill 仍正常输入 + 渲染)

### sctk 0.19.2 不封装 cursor-shape-v1

- sctk 0.19.2 docs.rs 检索: `cursor_shape` / `CursorShape` / `WpCursorShape`
  零命中 — sctk 0.19 仅封装 `wl_cursor` (传统路径), cursor-shape-v1 在
  sctk 0.21+ 路线图
- 与 ADR 0007 (text-input-v3) 同情形 — `wayland-protocols = "0.32"` 已含
  `wp/cursor_shape/v1`, 仅需 `staging` feature gate 启用

## Decision

`wayland-protocols = "0.32"` features 由 `["client", "unstable"]` (T-0505 引)
扩为 `["client", "unstable", "staging"]`。**不引新 crate**, **无新依赖图节点**。

`wp_cursor_shape_v1` 在 `wayland-protocols-0.32.12/src/wp.rs:413` 处 gated
在 `#[cfg(all(feature = "staging", feature = "unstable"))]` (源码确认), 启
`staging` 即解锁 `wp/cursor_shape/v1` 模块。

固定调用接口:

```rust
use wayland_protocols::wp::cursor_shape::v1::client::{
    wp_cursor_shape_manager_v1::WpCursorShapeManagerV1,
    wp_cursor_shape_device_v1::{WpCursorShapeDeviceV1, Shape as WpShape},
};

// registry bind manager (与 ADR 0007 ZwpTextInputManagerV3 同套路)
let manager: Option<WpCursorShapeManagerV1> = match globals.bind(&qh, 1..=1, ()) {
    Ok(m) => Some(m),
    Err(_) => None,  // 老 compositor 退化 (默认箭头)
};

// 每个 wl_pointer 一个 device (sctk 0.19 SeatHandler::new_capability 时建)
let device: WpCursorShapeDeviceV1 = manager.get_pointer(&wl_pointer, &qh, ());

// enter/hover 变化时调
device.set_shape(enter_serial, WpShape::Text);
```

**版本**: `wp_cursor_shape_manager_v1` 协议 v1 (上游 v2 加 dnd_ask + all_resize
两个 cursor 名, quill 不需要), 用 `1..=1` 锁住 v1 兼容更老 compositor
(Plasma 6.0 是 v1, 后期升 v2)。

**类型隔离 (INV-010)**: `WpShape` / `WpCursorShapeManagerV1` /
`WpCursorShapeDeviceV1` 协议类型**仅出现在 `src/wl/pointer.rs` 模块内 + 必要的
`src/wl/window.rs::Dispatch` 段**, **不**出现在 quill 公共 API 返回类型。

quill 自有 [`CursorShape`] enum (10 variants 覆盖 default/text + 4 边 + 4 角)
作公共类型, `wp_shape_for(quill_shape) -> WpShape` 模块私有 inherent fn 转换
(模块边界单点 — 与 ADR 0007 ImeAction 同套路)。

## Alternatives

### Alt 1: 等 sctk 上游封装 cursor-shape-v1

- 方案: 不启 staging feature, 等 sctk 0.21+ 内置 cursor wrapper
- Reject 主因:
  - **时间未知**: sctk 0.19→0.20→0.21 跨度合计 6-12 月, cursor-shape 优先级
    可能在 viewporter / fractional-scale 之后 (sctk 团队倾向先封装更基础的)
  - **派单 P1 CSD 完整性**: T-0701 resize 边角刚加, 用户没有 cursor 反馈
    根本不知道边能拖 (foot/alacritty 不接 = 用户骂, kitty 接早期)
  - **同 ADR 0007 时间不匹配 reasoning** — IME 与 cursor 都是 daily UX 项,
    不能等

### Alt 2: 走传统 `wl_pointer.set_cursor` + libwayland-cursor

- 方案: 不启 staging, 用 `wl_cursor_theme_load` + `wl_cursor_theme_get_cursor` +
  `wl_pointer.set_cursor(serial, surface, hotspot_x, hotspot_y)` 自管 buffer
- Reject 主因:
  - **代码量 5-10 倍**: wl_cursor_theme_load 需要 XDG_CURSOR_PATH /
    wl_cursor_theme_set_size / 加载 .png + .Xcursor 二进制格式 / fallback
    chain (Adwaita → default → 内置), 估 ~250 行
  - **HiDPI 复杂**: theme 1x/2x/3x 选哪个、compositor scale 改时 reload —
    cursor-shape-v1 让 compositor 自己处理
  - **C 依赖**: libwayland-cursor 是 C 库, transitive 拉 libxcursor — 与
    quill "纯 Rust 优先 (FFI 仅 xkbcommon / libwayland 必要)" 偏好不符
  - **不可移植主题**: 用户改系统 cursor theme (例 GNOME Settings 切 Bibata)
    时, client 走 wl_cursor 必须监听 D-Bus / 重 load; cursor-shape-v1
    compositor 自动处理
- 备选优势: 不依赖 staging feature — 但 staging 不是新 crate, 增量为 0

### Alt 3: 不接 cursor 切换 (永远默认箭头)

- 方案: 跳过本 ticket, 用户接受 resize 边角无 ↔ 反馈
- Reject 主因:
  - **CSD 不完整**: T-0701 加了 8 边角 hit-test + StartResize 路径, 没有
    cursor 反馈用户根本不知道哪能拖, hit-test 等于失效
  - **textarea I-beam 是基本 UX**: 用户在 cell 区域期待 I-beam (准备做选区 /
    复制), 默认箭头让用户以为 quill 不支持选区
  - 用户主桌面 GNOME 47, mutter 支持 cursor-shape-v1, **不接是浪费**

### Alt 4: 引 wayland-cursor crate (Rust 封装)

- 方案: cargo add wayland-cursor (Smithay 子项目 `wayland-cursor`), Rust 封装
  传统 wl_cursor 路径, 不依赖 libwayland-cursor C
- Reject 主因:
  - **新 crate dep**: 违反派单 "优先用现有 crate", 增编译时间 + 维护面
  - **仍是传统路径**: 不解决 Alt 2 的 HiDPI 复杂 / theme reload 复杂
  - **vs cursor-shape-v1 没赢**: cursor-shape-v1 协议层零代码 + compositor
    侧 unified, 远胜 client 自管

## Consequences

### 正面

- **代码量小**: 估 +100 行 (src/wl/pointer.rs CursorShape enum + 映射 + 转换 ~50
  + window.rs registry bind + Dispatch impl + SetCursor 路径 ~40 + 单测 ~10)
- **HiDPI / theme reload 全免费**: compositor 处理, client 仅传 enum
- **与 ADR 0007 wayland-protocols 复用**: 同 crate, 同 features 加一个 flag,
  零新增 transitive dep
- **类型隔离 (INV-010) 守得住**: WpShape 局限 src/wl/pointer.rs, CursorShape
  enum 全 quill 自有
- **fallback 优雅**: 老 compositor (Plasma 5 / 老 sway) bind 失败 → log warn,
  cursor 仍是 compositor 默认 (用户视觉退化但 quill 功能完整)

### 负面 / 代价

- **staging feature 标志启用**: wayland-protocols 文档警示 staging 协议
  "可能 backward incompatible"; cursor-shape-v1 自 2023 稳定未变,
  v1 → v2 仅加新 cursor 名 (back-compat addition), 不破。升级到未来 stable v1
  (若发布) 时改 use 路径 (单点 `src/wl/pointer.rs` 顶部 import 段)
- **wlroots ≤ 0.17 / 老 sway / Plasma 5 上 cursor 退化**: 用户主桌面 GNOME 47
  / Plasma 6 (家里 / 公司) 不影响, 但社区用户跑老 wlroots 时 fallback 行为是
  "无 cursor 切换" — log warn 提示但不阻塞
- **PointerState 加 4 字段** (current_cursor_shape / last_enter_serial / 加新
  PointerAction::SetCursor variant): drop 顺序无影响 (POD), 但增加单测维护面

### 已知残留 (非本 ADR scope)

- **自定义 cursor 主题**: Phase 7+ 配置时若加 `cursor_theme = "Bibata"` 设
  置, 需要重新评估 — wp_cursor_shape_v1 协议**不支持** client 选 theme
  (compositor 全权决定)。届时若想自定义需双协议: cursor-shape-v1 给系统默认,
  wl_cursor 走 client-loaded buffer override (复杂, 不在本 ticket scope)
- **cursor 隐藏 (gaming / fullscreen)**: Phase 7+ 加 `set_cursor(serial, null
  surface, 0, 0)` 路径 (cursor-shape-v1 不支持 hide, 必须用 wl_pointer.set_cursor
  null), 本 ticket 不做
- **tablet / 触屏 cursor**: wp_cursor_shape_manager_v1 也支持
  zwp_tablet_tool_v2, 本 ticket 仅接 wl_pointer, tablet 留 Phase 7+

## 实装验证

- T-0703 commit 实装本 ADR
- `src/wl/pointer.rs` 加 `CursorShape` enum + `cursor_shape_for` mapping fn +
  `wp_shape_for` 私有转换
- `src/wl/window.rs` 加 registry bind WpCursorShapeManagerV1 + 在
  SeatHandler::new_capability 拿 device + Dispatch impl + SetCursor 路径
- ≥ 5 个 lib 单测覆盖 cursor_shape_for 全 HoverRegion 分支 + apply_enter
  emit SetCursor + apply_motion 跨区 cursor 变化
- 4 门绿 (cargo build / test / clippy / fmt)
- 手测 deliverable: cargo run --release + 鼠标 hover titlebar / textarea
  → cursor 切换 (Lead 主导, ResizeEdge 4 边 4 角等 T-0701 合并后)

## 相关文档

- 派单: `tasks/T-0703-mouse-cursor-shape.md`
- 主体实装: `src/wl/pointer.rs::CursorShape` + `src/wl/window.rs::Dispatch<WpCursorShapeDeviceV1>` 段
- 协议参考: `https://wayland.app/protocols/cursor-shape-v1` +
  本地 `~/.cargo/registry/src/.../wayland-protocols-0.32.12/protocols/staging/cursor-shape/cursor-shape-v1.xml`
- 相关 ADR: 0007 (wayland-protocols, 同 crate 不同 feature; text-input-v3 同
  staging-like 协议路径)
- 相关 INV: INV-005 (calloop 单线程, set_shape 是非阻塞 wayland request) +
  INV-010 (类型隔离, WpShape / WpCursorShape* 不出 src/wl/pointer.rs +
  Dispatch 段)
