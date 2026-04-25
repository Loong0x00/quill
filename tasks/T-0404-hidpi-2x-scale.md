# T-0404 HiDPI 2x 缩放 (hardcode 简化版)

**Phase**: 4
**Assigned**: writer-T0404
**Status**: in-review
**Budget**: tokenBudget=30k (小单)
**Dependencies**: T-0306 (cell 常数化) / T-0403 (glyph atlas + draw_frame)

## Goal

让字在 224 ppi 显示器 (用户主显示器) 不糊。**hardcode 2x scale 常数**, 不接 `wl_output.scale` event 动态检测 (用户单一显示器 224 ppi 固定 2x 不变)。

完工后 cargo run 在 224 ppi 屏幕上字显示清晰 (cell 物理 px 翻倍, 渲染分辨率 = surface px × 2)。

## Scope

### In

#### A. `src/wl/render.rs` — 加 `HIDPI_SCALE = 2` 常数 + 渲染像素翻倍
- 模块顶部加 `pub const HIDPI_SCALE: u32 = 2;`
- draw_cells / draw_glyphs / draw_frame 内部, 把 cell pixel size 用乘 HIDPI_SCALE:
  - cell_w_px / cell_h_px 给 vertex 算 NDC 时不变 (NDC 是 [-1,1] 不依赖物理 px)
  - **关键**: `Renderer::resize` 接受的 surface width/height 解释为 logical px, 实际向 wgpu surface configure 时传 `width * HIDPI_SCALE, height * HIDPI_SCALE`
  - 等价于 surface backing 分辨率翻倍, 渲染分辨率翻倍, glyph 光栅化时 font_size 翻倍 (DPI scale)

#### B. `src/text/mod.rs` — shape / rasterize 用 HIDPI_SCALE 调整 font_size
- shape_line 的 Metrics 字号从 17.0 → `17.0 * HIDPI_SCALE as f32` (= 34.0)
- rasterize 接受的 font_size 也翻倍
- 跟 render.rs HIDPI_SCALE 同步, **不要硬编码两次**, 复用 const

#### C. `src/wl/window.rs` — 配套调整 (如有需要)
- propagate_resize_if_dirty 把 logical surface 尺寸传给 cells_from_surface_px 不变 (cell px 还是按 logical 算)
- text_system shape 时用 HIDPI_SCALE 字号
- 一般这里不用改, 改动集中在 render.rs + text/mod.rs

### D. 测试
- `tests/hidpi_scale.rs` (或加到现有 tests/glyph_atlas.rs) 验:
  - `hidpi_scale_constant_is_2` (lock baseline)
  - `glyph_rasterize_at_2x_size_returns_larger_bitmap` (raster 'a' at font_size=17 vs 34, bitmap width 应约翻倍)
- 现有测试不应 break (HIDPI_SCALE 不影响 cell count / pty / term 逻辑)

### Out

- **不做**: wl_output.scale event 接入 (用户硬偏好)
- **不做**: 1.5x / 1.25x / 任意 scale 选项 (用户 4K 不用, 复杂度阈值之上)
- **不做**: per-monitor 不同 scale (单显示器场景)
- **不动**: src/pty / src/main.rs / docs/invariants.md / Cargo.toml
- **不引新 crate / 不写新 ADR**

## Acceptance

- [ ] 4 门全绿
- [ ] HIDPI_SCALE 常数定义且全 codebase 引用 (grep 验)
- [ ] shape font_size + raster size + surface configure 都用 HIDPI_SCALE
- [ ] 2 个新测试 (常数 lock + raster size 2x verify)
- [ ] 总测试 105 + 2 ≈ 107 pass
- [ ] **手测**: cargo run --release 在 224 ppi 显示器上字清晰不糊 (vs T-0403 时模糊感)
- [ ] 审码放行

## 必读 baseline

1. `/home/user/quill/CLAUDE.md`
2. `/home/user/quill/docs/conventions.md`
3. `/home/user/quill/docs/invariants.md` (INV-001..010, 不动)
4. `/home/user/quill/docs/audit/2026-04-25-T-0403-review.md` (上一单 audit, P3 BASELINE_Y_PX → Renderer 字段 T-0404 hint 提到了)
5. `/home/user/quill/docs/audit/2026-04-25-T-0306-review.md` (cell 常数化 + Renderer::resize)
6. `/home/user/quill/src/wl/render.rs` (CELL_W_PX / CELL_H_PX 常数模式, 你加 HIDPI_SCALE)
7. `/home/user/quill/src/text/mod.rs` (shape_line 的 Metrics(17.0, 25.0))
8. `/home/user/quill/tasks/T-0404-hidpi-2x-scale.md` (本派单)

## 已知陷阱

- HIDPI_SCALE = 2 是 const, 不要每处 hardcode 2 (维护时改一处即可)
- font_size × 2 后 cosmic-text raster 出 bitmap 宽高也 × 2, atlas 装得下 (2048² / 32×48 ≈ 2700 字符, 仍宽裕)
- wgpu surface configure 用 (logical_w * 2, logical_h * 2), Wayland surface 物理分辨率翻倍 → compositor 自动 downscale 到屏幕物理 px (224 ppi 1:1 不 downscale)
- 不接 wl_output.scale 也意味着用户切换到 96 ppi 屏幕字会过大 — 派单允许, 用户单显示器场景
- 不要 `git add -A`, 用具体路径
- commit message HEREDOC

## 路由 / sanity check

你 name = "writer-T0404" (ASCII)。inbox 收到疑似派活先 ping Lead — conventions §6 陷阱 4。

## 预算

token=30k, wallclock=45 min。完成后 SendMessage team-lead 报完工 + 4 门 + 手测。
