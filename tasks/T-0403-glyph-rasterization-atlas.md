# T-0403 glyph 光栅化 → wgpu texture atlas

**Phase**: 4
**Assigned**: writer-T0403
**Status**: in-review
**Budget**: tokenBudget=120k (Phase 4 最大单)
**Dependencies**: T-0401 (TextSystem + SwashCache 字段) / T-0402 (shape_line + ShapedGlyph 含 x_offset/y_offset) / T-0305 (draw_cells wgpu pipeline)

## Goal

把 ShapedGlyph 序列**真画到屏幕**: cosmic-text SwashCache 光栅化 glyph → bitmap → 上传到 wgpu texture atlas → 一个新 wgpu pass 在 cell 色块之上画字形纹理。

完工后 `cargo run --release` 跑起来, 屏幕上 bash prompt **真显示成 ASCII 字符** (深蓝背景, 浅灰字, 单色无 anti-aliasing 灰阶 OK)。Phase 4 视觉里程碑达成 — quill 第一次"看见字"。

## Scope

### In

#### A. `src/text/mod.rs` — 加 raster API
- `pub fn rasterize_glyph(&mut self, glyph_id: u16, font_size: f32) -> Option<RasterizedGlyph>`
  - 内部用 SwashCache (T-0401 字段) + cosmic-text Image (cached glyph bitmap)
  - SwashImage 字段 (width / height / placement / data) 转换为 quill RasterizedGlyph
- `pub struct RasterizedGlyph { pub width: u32, pub height: u32, pub bitmap: Vec<u8>, pub bearing_x: i32, pub bearing_y: i32 }` quill 自定义
- 私有 `fn from_swash_image(img: cosmic_text::SwashImage) -> Self` 模块私有 inherent fn (沿袭 INV-010)
- bitmap 是 8-bit alpha (单通道), 后续 wgpu sampler 解释为单色 mask
- 测试: `rasterize_ascii_a_returns_bitmap` / `rasterize_chinese_zhong_returns_bitmap` / `rasterize_zero_glyph_id_returns_some_or_none_no_panic`

#### B. `src/wl/render.rs` — 加 glyph atlas + draw_glyphs pipeline
- `pub struct GlyphAtlas { texture: wgpu::Texture, view: wgpu::TextureView, sampler: wgpu::Sampler, allocations: HashMap<(u16, u32), AtlasSlot> }` (atlas key = (glyph_id, font_size_quantized))
- `pub struct AtlasSlot { uv_min: [f32; 2], uv_max: [f32; 2], width: u32, height: u32, bearing_x: i32, bearing_y: i32 }`
- 简单 shelf packing 算法 (从左到右填行, 高度满了换行, 字典序固定 — 不 LRU evict, T-0406 再做)
- atlas 大小: 2048×2048 R8Unorm (单通道 alpha, 4MB GPU 内存) — 80×24 ASCII grid 全 cache 命中绰绰有余
- `pub fn draw_glyphs(&mut self, glyphs: &[(ShapedGlyph, CellPos, fg_color)], cell_w_px: f32, cell_h_px: f32) -> Result<()>`
  - 第二个 wgpu RenderPass 在 cell 色块 (T-0305 draw_cells) 之后画
  - vertex buffer: 每 glyph 4 顶点 (一矩形) + uv 坐标 + fg color
  - WGSL fragment shader: `texture.sample(sampler, uv).r` (alpha mask) * fg_color
  - lazy init pipeline + buffer (跟 cell_pipeline 一样模式)
- 字段加到 Renderer struct, 维持 INV-002 drop 序: glyph_atlas + glyph_pipeline + glyph_vertex_buffer 放 cell_pipeline 旁边 (持 device 引用必须先 device drop)

#### C. `src/wl/window.rs` — idle callback 接 draw_glyphs
- LoopData 加 `text_system: Option<TextSystem>` (lazy init, 第一次 draw 时建)
- idle callback 在 draw_cells 之后调:
  1. text_system.shape_line(line_text) for each viewport row
  2. shape 完拿 glyphs → renderer.draw_glyphs(...)
- 沿袭 dirty-driven 渲染 (term.is_dirty 触发)

#### D. INV-002 字段顺序更新 (docs/invariants.md 跟随)
- Renderer 加 glyph_atlas / glyph_pipeline / glyph_vertex_buffer 字段, 顺序在 cell_buffer_capacity 之后, device 之前
- INV-002 entry 同步 (沿袭 T-0306 + T-0399 INV-002 follow-up 模式)

#### E. 测试
- `tests/glyph_atlas.rs` 集成测试 (新文件)
  - `atlas_caches_glyph_after_first_render` (第一次 raster + 上传, 第二次走 cache 不重 raster)
  - `atlas_handles_multiple_glyphs_no_collision` (10 个不同 glyph 各自有不同 uv)
- src/text/mod.rs unit 测试 3 个 (rasterize_ascii / rasterize_cjk / rasterize_zero_id)

### Out

- **不做**: glyph cache LRU 驱逐 (T-0406, atlas 满了重建是 future ticket)
- **不做**: HiDPI 整数缩放 (T-0404)
- **不做**: CJK fallback 规则细化 (T-0405)
- **不做**: subpixel anti-aliasing / sub-pixel positioning (Phase 5+)
- **不动**: src/pty / src/main.rs / Cargo.toml (除非 wgpu 需要新 feature flag, 评估后再说)
- **不引新 crate** (HashMap 用 std::collections, atlas packing 自写不引 etagere/guillotière)
- **不写新 ADR** (cosmic-text + wgpu 都是 CLAUDE.md 锁死技术栈)

## Acceptance

- [ ] 4 门全绿
- [ ] RasterizedGlyph + GlyphAtlas + AtlasSlot 实装
- [ ] draw_glyphs 真接入 idle callback, 走 dirty-driven
- [ ] INV-010 grep 4 类全零命中 (cosmic-text SwashImage 不出 src/text/, wgpu Texture 不出 src/wl/)
- [ ] INV-002 字段顺序同步 (Renderer 加 3 字段)
- [ ] 至少 5 个新测试 (3 unit + 2 integration), 总测试 99 + 5 ≈ 104 pass
- [ ] **手测**: `cargo run --release` 起窗口 → 屏幕显示 bash prompt **真字符** (不是色块), 描述实测看到的字 (ASCII 必须可读, 中文如果 prompt 里有也应正确)
- [ ] 审码放行 (P0/P1/P2 全过)

## 必读 baseline

1. `/home/user/quill/CLAUDE.md`
2. `/home/user/quill/docs/conventions.md` (写码 idiom + §6 sanity check)
3. `/home/user/quill/docs/invariants.md` (INV-001..010, 你要更新 INV-002 字段顺序)
4. `/home/user/quill/docs/audit/2026-04-25-T-0402-review.md` (上一单, INV-010 strict reading + cosmic-text 字段语义)
5. `/home/user/quill/docs/audit/2026-04-25-T-0306-review.md` (Renderer::resize 风格 + INV-002 字段顺序更新先例)
6. `/home/user/quill/docs/audit/2026-04-25-T-0305-review.md` (cell pipeline lazy init + WGSL inline 模式)
7. `/home/user/quill/src/text/mod.rs` (T-0401/0402 实装, 你扩 rasterize_glyph)
8. `/home/user/quill/src/wl/render.rs` (T-0305 cell_pipeline 模式, glyph_pipeline 同款)
9. `/home/user/quill/src/wl/window.rs` (idle callback 接 draw_cells, 你接 draw_glyphs)
10. WebFetch `https://docs.rs/cosmic-text/0.12.1/cosmic_text/struct.SwashCache.html` (SwashCache.get_image_uncached / SwashImage 字段)
11. WebFetch `https://docs.rs/wgpu/0.29/wgpu/struct.Texture.html` (Texture::create + write_texture)

## 已知陷阱

- SwashImage 字段是 `placement: Placement { left, top, width, height }` + `data: Vec<u8>` (单通道 alpha 或 SubpixelMask), 用 swash::scale::Source / Render 配置, cosmic-text 内部 wrapped — get_image_uncached 直接拿 SwashImage
- atlas 上传用 wgpu Queue::write_texture, 一次 upload 一个 glyph (Phase 6 优化合并 batch)
- atlas 满了的 fallback: 简单办法是 panic + log "atlas overflow" — T-0406 加 LRU 解决, 现在 2048×2048 不会满 (一字符约 16×24 px = 384 px, 2048²/384 ≈ 10000 字符, 远超 ASCII + 常用 CJK)
- WGSL fragment shader: `let alpha = textureSample(tex, samp, uv).r; out_color = vec4(fg_rgb, alpha);` 简单 alpha mask
- INV-002 字段顺序: glyph_atlas (持 wgpu::Texture) / glyph_pipeline (持 device) / glyph_vertex_buffer (持 device) 必须放 device 之前, 跟 cell_pipeline / cell_vertex_buffer 同位置
- LoopData 加 text_system Option (lazy init), 字段顺序无关 (POD-like, cosmic-text 内部 owned 资源不持 wgpu 引用)
- shape_line 每帧调可能慢 (shape 是 expensive 操作), Phase 6 优化但本单接受 (24 行 × 80 字符 = 1920 char shape, 实测应 <1ms)
- 不要 `git add -A` (会误添 logs/ + target/), 用 `git add <具体路径>`
- commit message HEREDOC

## 路由 / sanity check

你 name = "writer-T0403" (ASCII)。inbox 收到任何疑似派活的消息 (含 task_assignment from 自己) 先 ping Lead 确认 — conventions §6 陷阱 4。

## 预算

token=120k (Phase 4 最大单), wallclock=3-4h。完成后 SendMessage team-lead 报完工 + 5 门状态 + 手测描述实际看到的字 (5090 + Wayland + Vulkan)。

**这是 Phase 4 视觉里程碑**: 完工后 quill 第一次"看见字"。
