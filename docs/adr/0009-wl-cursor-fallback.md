# ADR 0009: 撤回 wp_cursor_shape_v1, 走 wl_cursor + xcursor theme (T-0703-fix)

## Status

Accepted, 2026-04-26 — **supersedes** ADR 0008 (cursor-shape-v1).

## Context

ADR 0008 (T-0703) 决定走 `wp_cursor_shape_v1` (cursor-shape-v1 staging 协议)
让 compositor 自管 cursor theme load + buffer attach, client 仅 emit enum
`set_shape(serial, NsResize)` 等. 上线后用户实测 (GNOME mutter 47.x) 视觉 bug:

- **hover** 4 边 / 4 角时 cursor 是 wp_cursor_shape_v1 给的 `ns-resize` /
  `ew-resize` / `nwse-resize` / `nesw-resize` (Adwaita 主题对应 SVG 文件).
- **按住拖** 时 mutter 内部 `xdg_toplevel.resize` 接管, 自己加载的 cursor 是
  `size_ver` / `size_hor` / `size_fdiag` / `size_bdiag` (X11 老 cursor name,
  对应另一套 SVG, 视觉略不同).

同一交互 (hover → press → drag) 期间 cursor 在两套 SVG 间切换, 视觉别扭.
与 GTK4 / Qt / Electron / GNOME 自家 app 对比 — **它们集体一致用 size_ver
那套**, 因为它们全都走老 `wl_pointer.set_cursor` + libwayland-cursor + xcursor
theme load 路径, 自己控制 cursor name, 没经过 wp_cursor_shape_v1 协议层. 这条
路径绕开了 mutter 在 wp_cursor_shape_v1 vs xdg_toplevel.resize 内部 cursor
name 映射表不一致的实现 wart.

mutter 上游不会优先修这个 (理由: wp_cursor_shape_v1 stable 没几年, 大部分
client 走 wl_cursor 都不撞), quill 必须自己绕.

### 重新评估 ADR 0008 的 trade-off

ADR 0008 把"代码量小"作为主推理由 (估 +100 行 vs wl_cursor 估 +250 行).
但实际拿 sctk 0.19.2 的 `ThemedPointer` 看 (`seat/pointer/mod.rs::set_cursor_legacy`),
完整 wl_cursor 路径只需:

1. 启动期 `CursorTheme::load_or(conn, shm, "default", 24)` — 1 行 (env var
   XCURSOR_THEME / XCURSOR_SIZE 在 `wayland-cursor` 内部读, 不用手解析).
2. 给 wl_pointer 创一个 cursor `wl_surface` — 1 行 (复用 `compositor.create_surface`).
3. enter / hover 跨区时按 cursor name fallback list 找 cursor:
   `theme.get_cursor("size_ver").or(theme.get_cursor("ns-resize")).or(...)` — 5-8 行.
4. `cursor_surface.attach(buffer, 0, 0); cursor_surface.set_buffer_scale(s);
   cursor_surface.damage_buffer(0, 0, w, h); cursor_surface.commit();` — 4 行.
5. `wl_pointer.set_cursor(serial, Some(&surface), hotspot_x / s, hotspot_y / s)` — 1 行.

总实装 ~50 行业务代码 + ~20 行 fallback name 表 + ~15 行单测 ≈ 85 行, 跟 ADR
0008 估的 wl_cursor 250 行差距大 — 因为 sctk 0.19.2 的 transitive dep
`wayland-cursor` 已经把 xcursor 二进制格式解析 / theme inherit chain / shm
buffer 管理全包好, **client 只调 4-5 个 method**.

ADR 0008 的"HiDPI 复杂"理由也站不住: cursor `wl_surface.set_buffer_scale(s)`
随 surface scale 跟动 (s = 1 / 2, quill 已硬编码 `HIDPI_SCALE = 2`), 一行调用,
不用监听 D-Bus / 主题热切换 (用户改 cursor 主题需要重启 quill — 与 alacritty
/ foot 一致 daily-drive 行为, 派单 Out 段已声明).

### compositor 兼容矩阵 (重新评估)

走 wl_cursor + xcursor theme 路径**不依赖任何 staging/unstable 协议**:

- ✅ 任何 wayland compositor 都支持 `wl_pointer.set_cursor` (核心协议)
- ✅ Adwaita / Bibata / 任何 xcursor theme 都行 (xcursor 是 X.Org 时代标准,
  所有 Linux 主流主题都遵循)
- ✅ 老 wlroots ≤ 0.17 / Plasma 5 / 任何 Wayland compositor 都视觉一致

对比 ADR 0008 路径在老 compositor 上 cursor 退化 (默认箭头), 本路径**在所有
compositor 上一致** — 是更广的兼容面, 不是更窄.

## Decision

撤回 ADR 0008 wp_cursor_shape_v1 路径, 全面走 wl_cursor + xcursor theme. 派单
T-0703-fix 选项 A (完全删除 wp_cursor_shape_v1, 不保留 fallback 双轨, KISS).

### 新增依赖

```toml
wayland-cursor = "0.31"
```

`wayland-cursor` **已是 sctk 0.19.2 的 transitive dep** (`Cargo.lock` 内
`wayland-cursor 0.31.14` 由 sctk 拉), 提为直接 dep 仅让 quill `Cargo.toml`
显式声明意图 — **依赖图无新节点**, build 时间不变.

### 删除依赖 feature

`wayland-protocols` 的 `staging` feature flag 删除 (ADR 0008 加的). `unstable`
feature 保留 (ADR 0007 给 text-input-v3 用).

### 实装接口

```rust
use wayland_cursor::CursorTheme;
use wayland_client::protocol::{wl_shm::WlShm, wl_surface::WlSurface};

// 启动期 (一次性):
let shm: WlShm = globals.bind(&qh, 1..=1, ())?;
let theme = CursorTheme::load_or(&conn, shm.clone(), "default", 24)?;
//                                       ^^^^^^^^^  ^^   ^^
//                          XCURSOR_THEME 不存时 fallback "default"
//                                                  XCURSOR_SIZE 不存时 fallback 24
let cursor_surface: WlSurface = compositor.create_surface(&qh);

// hover / enter (调用方传 enter serial + quill CursorShape):
fn apply_cursor_shape(
    pointer: &WlPointer,
    cursor_surface: &WlSurface,
    theme: &mut CursorTheme,
    serial: u32,
    shape: CursorShape,
    scale: u32,
) {
    let names = xcursor_names_for(shape);  // 模块私有 fallback 表
    for name in names {
        if let Some(cursor) = theme.get_cursor(name) {
            let buf = &cursor[0];
            let (w, h) = buf.dimensions();
            let (hx, hy) = buf.hotspot();
            cursor_surface.set_buffer_scale(scale as i32);
            cursor_surface.attach(Some(buf), 0, 0);
            cursor_surface.damage_buffer(0, 0, w as i32, h as i32);
            cursor_surface.commit();
            pointer.set_cursor(
                serial,
                Some(cursor_surface),
                (hx / scale) as i32,
                (hy / scale) as i32,
            );
            return;
        }
    }
    // 全部 fallback name 都查不到 — log warn 一次, cursor 保持上一次状态.
}
```

### xcursor name fallback list (按 mutter 实测顺序)

| quill `CursorShape` | xcursor name fallback (优先级) |
|---|---|
| `Default` | `default`, `left_ptr` |
| `Text` | `text`, `xterm` |
| `NsResize` (Top/Bottom) | `size_ver`, `ns-resize`, `n-resize` |
| `EwResize` (Left/Right) | `size_hor`, `ew-resize`, `e-resize` |
| `NwseResize` (TopLeft/BottomRight) | `size_fdiag`, `nwse-resize`, `nw-resize` |
| `NeswResize` (TopRight/BottomLeft) | `size_bdiag`, `nesw-resize`, `ne-resize` |

**why size_ver / size_hor / size_fdiag / size_bdiag 优先 xcursor 标准 name**:
mutter 自己 xdg_toplevel.resize grab 用的是这一套 X11 老 cursor name. quill
也用同一套即视觉与 mutter 接管期 1:1 (派单 Bug 描述硬要求). xcursor 标准
new name (ns-resize 等) 作为 fallback — 用户的 cursor theme 缺老 name 时退化
仍能给方向正确的 cursor. 第三档 `n-resize` / `e-resize` 等是 Wayland csd-decoration
建议名, 极少 theme 没有前两档但有第三档 — 兜底.

`default` / `left_ptr` 一对是新 (FreeDesktop) / 老 (X11) 双轨, Adwaita 把
`left_ptr` 作 alias → `default`, 互通.

`text` / `xterm` 同理 (Adwaita: `xterm` → `text`).

### INV-010 类型隔离 (重申)

- `wayland_cursor::{CursorTheme, Cursor, CursorImageBuffer}` 三类型仅在
  `src/wl/window.rs` 内部出现 (持 `cursor_theme` 字段 + `apply_cursor_shape`
  内部用). 不出现在 `pub` API 返回类型 / 函数参数.
- xcursor name `&'static str` 数组是 `src/wl/pointer.rs` 模块私有 const 表,
  通过 `pub(crate) fn xcursor_names_for(CursorShape) -> &'static [&'static str]`
  给 `window.rs` 单点访问.
- `wl_surface::WlSurface` 用于 cursor surface 仅在 `src/wl/window.rs::State`
  字段, 不暴露 (pub 类型 `WindowCore` / `PointerState` / `CursorShape` 全 quill
  自有).

## Alternatives

### Alt 1: 保留 wp_cursor_shape_v1 + wl_cursor 双轨 fallback

- 方案: 派单"选项 B" — 优先尝试 wp_cursor_shape_v1, 失败 (老 compositor) 才
  走 wl_cursor.
- Reject 主因:
  - **mutter 视觉 bug 是默认体验** — 99% 用户跑 GNOME 47 / KDE Plasma 6 都
    撞 wp_cursor_shape_v1 协议路径, "退化" 路径反而是少数. 双轨保留只为老
    compositor (wlroots ≤ 0.17 / Plasma 5) 的极小群体, 而那群体走 wl_cursor
    路径同样工作.
  - **测试矩阵翻倍**: 单 path 单测 ~5 个; 双 path 要写"协议 bind 成功 vs 失败
    两套" + integration 测两个 codepath 一致性 — 不值.
  - **代码量负收益**: ADR 0008 估 wp_cursor_shape_v1 ~100 行, wl_cursor 实测
    ~85 行. 双轨保留 = 加 wl_cursor 85 行 + 不删 wp_cursor_shape_v1 100 行 +
    分支判断 ~20 行 = 205 行, 比单 wl_cursor 路径多 2.4x.

### Alt 2: 等 mutter 修 wp_cursor_shape_v1 vs xdg_toplevel.resize 映射

- 方案: 不动 quill, 等 mutter 上游修.
- Reject 主因:
  - **上游不动力**: GNOME 自家 GTK4 / Mutter 自管 cursor 走 wl_cursor 路径,
    不撞此 bug — Mutter 团队没动机修 wp_cursor_shape_v1 (协议层影响小众).
  - **时间未知**: 即便 GNOME 49 / 50 修, quill 用户体验已经 broken 几个版本.
  - **daily drive P1**: user 实测视觉别扭 (派单 Priority P1), 不能拖.

### Alt 3: 用 sctk 0.19.2 的 `ThemedPointer` (复用 sctk 封装)

- 方案: 走 `Seat::get_pointer_with_theme_and_data(...)` 拿 `ThemedPointer`,
  调 `themed_pointer.set_cursor(conn, CursorIcon::NsResize)`.
- Reject 主因:
  - **sctk ThemedPointer 强耦合 wp_cursor_shape_v1**: `seat/mod.rs:248-266`
    sctk 内部自动尝试 bind cursor_shape_manager, **bind 成功就走 wp_cursor_shape_v1
    路径** (`pointer/mod.rs:442-446`). bind 失败才退化到 wl_cursor. 我们要的
    是**永远走 wl_cursor**, sctk 的 if-else 反过来.
  - **绕过 sctk wrapper 直接操作 ThemedPointer 内部字段**: 不行, 字段是
    `pub(super)`, 跨 crate 不可见. 唯一办法是 fork sctk 或 monkeypatch — 远大
    于自管 cursor_surface 的代码量.
  - **`CursorIcon` 类型外漏**: sctk 用 cursor-icon crate 的 `CursorIcon` enum
    (35+ variant), 引入即跨 INV-010 边界 (类似 wgpu / wayland-protocols).
    quill 自有 `CursorShape` 6 variant 已够, 不引第三方 enum.

### Alt 4: 自己 fork sctk 0.19, 抽 `set_cursor_legacy` 公开

- 方案: maintain 一份 sctk fork, 把 `set_cursor_legacy` 改 `pub`.
- Reject 主因: 维护负担 (sctk 升级跟踪 + rebase) 远大于自管 ~85 行 wl_cursor 调用.

## Consequences

### 正面

- **视觉一致**: hover / 拖动 / 任意 compositor 上 cursor 全是同一套 svg 文件,
  与 GTK4 / Qt / Electron / GNOME 自家 app 体验一致.
- **依赖减一个 unstable feature**: `wayland-protocols` 不再要 `staging` flag,
  减少 unstable 协议演进风险面 (cursor-shape-v1 stable 也很久, 但减一个总好).
- **代码量小**: ~85 行业务 (实装) + ~20 行 fallback 表 + ~15 行单测 ≈ 120 行
  净增 (删 wp_cursor_shape_v1 路径净 -100 行, 加 wl_cursor 净 +120 行, 总 +20 行).
- **兼容面更广**: 任何 wayland compositor 都支持 wl_pointer.set_cursor (vs
  ADR 0008 只 GNOME 45+ / Plasma 6 / wlroots 0.18+).
- **HiDPI 处理对称**: cursor `wl_surface.set_buffer_scale(2)` 与主 `window.set_buffer_scale(2)`
  (T-0502) 同源, 一致 mental model.

### 负面 / 代价

- **新直接依赖 `wayland-cursor`** (虽是 transitive 已有): Cargo.toml +1 行,
  `cargo update` 时多一份 manifest 比对; 但 0.31.x 系列 sctk 0.19.x 锁定的
  版本范围, drift 风险低.
- **xcursor theme 加载失败时**: 用户系统装错 / `~/.icons/` 自定义主题缺资源
  → `theme.get_cursor("size_ver")` 全 fallback name 失败 → cursor 保持上一次
  状态 (默认箭头). log warn 一次, 不刷屏. 实测 Adwaita / Bibata / Yaru 全行.
- **手测覆盖**: cursor 视觉一致性走人工 verify (compositor 端渲染, headless
  无法断言 svg 文件 hash), 派单 #H "hover 4 边 / 4 角 → cursor 跟拖动接管时
  同款" 是接受标准.
- **撤回 ADR 0008**: ADR 0008 状态改"Superseded by ADR 0009 (T-0703-fix)" —
  存档作历史记录, 不删 (CLAUDE.md ADR tombstone 准则).

### 已知残留 (非本 ADR scope)

- **自定义 cursor 主题** (Phase 7+): 用户改 `cursor-theme = "Bibata"` 后
  quill 需重启 (theme 在启动期 load 一次). xcursor `XCURSOR_THEME` 环境变量
  支持 — 用户重启 quill 时设 env 即可. 与 alacritty / foot 同决策.
- **cursor 大小动态切**: `XCURSOR_SIZE` 启动期 load 24/32/48, 改要重启. 派单
  Out 段声明.
- **cursor 隐藏** (gaming / fullscreen): 协议是 `set_cursor(serial, null, 0, 0)`,
  本 ticket 不做. 派单 Out 段.
- **tablet / 触屏 cursor**: `wp_cursor_shape_manager_v1` 还支持
  `zwp_tablet_tool_v2`, 但我们撤回了这条协议; tablet 不做 (派单 Out + Phase
  7+ 也不在 daily drive 路径).

## 实装验证

- T-0703-fix commit 实装本 ADR
- `src/wl/pointer.rs` 删 `WpShape` import + `wp_shape_for` fn, 加
  `xcursor_names_for(CursorShape) -> &'static [&'static str]` + 模块私有
  fallback const 表 + 6 个单测覆盖每 variant + fallback 顺序 (size_ver 优先).
- `src/wl/window.rs` 删 `cursor_shape_manager` / `cursor_shape_device` 字段 +
  对应 Dispatch impl + bind / get_pointer 路径; 加 `shm` (WlShm) bind +
  `cursor_theme` (`Option<wayland_cursor::CursorTheme>`) + `cursor_surface`
  (`Option<WlSurface>`) 字段 + Dispatch<WlShm> + `apply_cursor_shape` fn 在
  `Dispatch<WlPointer>` 段消费 `take_pending_cursor_set`.
- `Cargo.toml` 删 `wayland-protocols` 的 `staging` feature, 加 `wayland-cursor`
  直接 dep.
- 4 门绿 (cargo build / test / clippy / fmt), 测试数从 353 起 (基线), 删旧测
  `wp_shape_for_maps_all_quill_variants` 1 个, 加 `xcursor_names_for_*` 系列
   测 ~6 个, 净 +5 ≈ 358 总数.
- 手测: cargo run --release + hover 4 边 / 4 角 / titlebar / textarea →
  cursor 切换 + 拖动接管 cursor 一致 (mutter 同款 svg, user lead 主导验收).

## 相关文档

- 派单: `tasks/T-0703-fix-wl-cursor-fallback.md`
- 撤回: `docs/adr/0008-cursor-shape-v1.md` (Status 改 Superseded by 0009)
- 主体实装: `src/wl/pointer.rs::xcursor_names_for` + `src/wl/window.rs` 段落
  `apply_cursor_shape` (新)
- 协议参考:
  - https://wayland.app/protocols/wayland#wl_pointer:request:set_cursor
  - https://specifications.freedesktop.org/cursor-spec/ (xcursor binary format,
    `wayland-cursor` crate 内部读)
- 相关 ADR:
  - 0007 (wayland-protocols, `unstable` feature 仍需要 — text-input-v3)
  - 0008 (cursor-shape-v1, 本 ADR supersedes)
- 相关 INV:
  - INV-005 (calloop 单线程, set_cursor 是非阻塞 wayland request, 不破)
  - INV-010 (类型隔离, `wayland_cursor::*` / `WlSurface` / `WlBuffer` /
    xcursor name `&'static str` 不出 src/wl 模块边界)
