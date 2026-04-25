# T-0502 wl_output.scale 接入 + set_buffer_scale (修双重 HiDPI 放大)

**Phase**: 5
**Assigned**: writer-T0502
**Status**: in-review
**Budget**: tokenBudget=80k (中小, 主要 src/wl/window.rs + src/wl/render.rs)
**Dependencies**: T-0404 (HIDPI_SCALE=2 hardcode), T-0409 (wgpu adapter limits 已修)
**Priority**: P0 (修当前窗口大小不正常 bug)

## Bug

User 实测 cargo run --release 窗口"有点大的不正常". 真因: T-0403 引 wgpu +
T-0404 加 HIDPI_SCALE=2 把 surface 翻倍, 但**没调 wl_surface.set_buffer_scale(2)
告诉 compositor "我自己处理 HiDPI"**. compositor 不知道 quill 是 HiDPI 程序,
默认按"普通 DPI 在 HiDPI 屏"再放大一倍 → 最终视觉 ×4. 表现: 800×600 logical
请求, compositor 真发 1600×1200 logical configure (它认为这才是合适大小),
HIDPI_SCALE×2 后变 3200×2400 physical, T-0409 hotfix 让 wgpu 接受了, 但**视觉
依然过大**.

## Goal

接 `wl_output.scale` event + `wl_surface.set_buffer_scale(scale)` 协议, 让
compositor 知道 quill 自己处理 HiDPI, compositor 不再 double-scale. 完工后
窗口视觉大小回归 1:1 (800×600 logical = 显示器上正常 800×600 看起来大小).

**保持 hardcode 简化**: 不真接动态 wl_output.scale (用户单显示器固定 ×2),
只调 set_buffer_scale(HIDPI_SCALE) 一次, 在 surface init 后. 这跟 T-0404
"不接 wl_output.scale" 派单约束兼容 (那是禁动态, 本单只静态调一次).

## Scope

### In

#### A. src/wl/window.rs surface init 后调 set_buffer_scale
- 在 `xdg_surface` ack_configure 之前 (或 surface creation 之后) 调:
  ```rust
  surface.set_buffer_scale(HIDPI_SCALE as i32);
  ```
- 用 `crate::wl::HIDPI_SCALE` (T-0404 已 export)
- doc 注释: 引 T-0502 + 解释为何 hardcode (单显示器场景, ROADMAP 永久不接动态)

#### B. configure handler 修正 logical size 解读
- compositor 收到 set_buffer_scale=2 后, 后续 configure 发 logical px 不变
  (e.g. 800×600), 但 buffer 我们要画 1600×1200 (×HIDPI_SCALE)
- 当前 configure handler 把 new_w / new_h 当 surface buffer size 用 — 这其实
  就对了, 因为我们之前就是手动翻倍。set_buffer_scale 加上后行为应保持一致
- **关键 verify**: 加 set_buffer_scale 后, compositor 不再 double-scale, configure
  回 800×600 (而不是 1600×1200), Renderer::resize(800, 600) → ×HIDPI 给 wgpu
  surface 1600×1200 — 跟之前一致

#### C. (可选) 接 wl_output Dispatch trait 仅记录 scale (不响应)
- 监听 wl_output.scale event, log "compositor reports scale={}"
- 如果 != HIDPI_SCALE 加 tracing::warn "compositor scale 不匹配 hardcode HIDPI_SCALE"
- 不做动态适配 (用户单显示器, 派单约束)

#### D. 测试
- `tests/buffer_scale.rs` (或扩 tests/wayland_init.rs):
  - 单测验 set_buffer_scale 调用次数 = 1, value = HIDPI_SCALE
  - 集成测试不易 (需要真 Wayland compositor), Lead 手测验视觉

#### E. headless render 不影响
- T-0408 render_headless 完全独立 wl_surface, 不调 set_buffer_scale
- 不动 src/wl/render.rs::render_headless

### Out

- **不做**: 真动态 wl_output.scale 接入 (用户硬偏好, T-0404 派单已说)
- **不做**: 多显示器 per-output scale (单显示器场景)
- **不做**: 1.5x / fractional scale (用户 224 ppi 单显示器固定 2x)
- **不做**: HIDPI_SCALE 改成 runtime 变量 (保 const, 一处改全部, T-0404 既定)
- **不动**: src/text/mod.rs / src/pty / src/main.rs / docs/invariants.md / Cargo.toml
- **不引新 crate**

### 跟其他并行 ticket 的协调

- T-0501 (wl_keyboard) 改 src/wl/window.rs 但是 wl_seat 段, 跟 set_buffer_scale 段不冲突
- T-0503 (xdg-decoration) 改 src/wl/window.rs surface init 段, **可能小冲突**, 但 git 自动 merge 多半 OK (不同行)

## Acceptance

- [ ] 4 门 release 全绿
- [ ] set_buffer_scale(HIDPI_SCALE) 在 surface init 后调 1 次
- [ ] 总测试 124 + 1~2 ≈ 125-126 pass
- [ ] **手测 deliverable**: cargo run --release 窗口视觉大小正常 (不再"大的不正常")
- [ ] tracing log 看 wl_output scale event (调试用)
- [ ] 审码放行

## 必读 baseline

1. /home/user/quill-impl-bufferscale/CLAUDE.md
2. /home/user/quill-impl-bufferscale/docs/conventions.md
3. /home/user/quill-impl-bufferscale/docs/invariants.md
4. /home/user/quill-impl-bufferscale/tasks/T-0404-hidpi-2x-scale.md (HIDPI_SCALE 由来)
5. /home/user/quill-impl-bufferscale/tasks/T-0409-hotfix-wgpu-limits.md (前个 hotfix 上下文)
6. /home/user/quill-impl-bufferscale/src/wl/window.rs (configure handler + WaylandState)
7. /home/user/quill-impl-bufferscale/src/wl/render.rs (HIDPI_SCALE const + Renderer::resize)
8. WebFetch https://wayland.app/protocols/wayland#wl_surface:request:set_buffer_scale
9. WebFetch https://wayland.app/protocols/wayland#wl_output:event:scale

## 已知陷阱

- set_buffer_scale 是 wl_surface 请求, 不是 xdg_surface
- 必须在 attach buffer **之前**调 (协议要求, 否则下次 commit 才生效)
- HIDPI_SCALE 是 u32, set_buffer_scale 接受 i32, cast as i32
- compositor 可能不发 wl_output.scale event (老 compositor), 不能依赖 — 仅 log
- wgpu Surface configure 当前 ×HIDPI_SCALE 在 Renderer::new/resize, 不需改
- 不要 git add -A
- HEREDOC commit
- ASCII name writer-T0502

## 路由

writer name = "writer-T0502"。

## 预算

token=80k, wallclock=1.5h. 完工 SendMessage team-lead 报 4 门 + diff stat + 手测视觉描述。
