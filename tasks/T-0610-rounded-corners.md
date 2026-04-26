# T-0610 part 2 窗口圆角 (shader corner radius mask)

**Phase**: 7+ (polish, daily-drive feel)
**Assigned**: writer-T0610
**Status**: in-review
**Budget**: tokenBudget=200k (跨 wgpu shader corner mask + uniform binding +
vertex layout 兼容 + 4 角圆形 alpha discard + 跟 T-0610 part 1 半透明
alpha=0.85 兼容)
**Dependencies**: T-0610 part 1 (surface 半透明 alpha=0.85, 已合 main a6b40f0)
**Priority**: P2 (polish, user 实测想要)

## Goal

给 quill window 4 角加圆角 (radius ~8 logical px), 跟 mutter / GTK4 / ghostty
现代窗口风格一致. 走 wgpu fragment shader corner radius mask: 角外 alpha
discard / alpha=0 让 compositor 视为透明, 角内仍走 T-0610 part 1 半透明
alpha=0.85.

## Bug / Pain

User 实测 quill 当前是直角 surface, 跟桌面其它 GTK4 / Adwaita 应用 (圆角)
不一致, 视觉违和. mutter compositor 不强制圆角 client surface (是 client
自己负责), 必须 quill 自己画。

## Scope

### In

#### A. corner radius 常数 + uniform
- 新增 `CORNER_RADIUS_PX: f32 = 8.0` (logical px, × HIDPI_SCALE 拿 physical)
- wgpu uniform binding 传 `[surface_w, surface_h, corner_radius_px, alpha_live]`
  (4 × f32 = 16 字节 std140 兼容) 给 cell pipeline + glyph pipeline + 边框
  pipeline 共享
- per-frame upload uniform (resize 时也更新)

#### B. fragment shader corner mask
- cell.wgsl + glyph.wgsl fragment stage 加:
  ```wgsl
  let pos = frag_coord.xy; // 物理 px
  let corner_dist = corner_distance(pos, surface_size, corner_radius);
  if corner_dist > corner_radius {
      discard; // 角外 alpha=0, compositor 透明
  }
  let alpha_factor = smoothstep(corner_radius - 1.0, corner_radius, corner_dist);
  out.color.a *= (1.0 - alpha_factor); // 角边缘 anti-alias
  ```
- corner_distance fn: 算 frag 到最近的 4 角 (top-left/top-right/bottom-left/bottom-right)
  内圆心的距离 (圆心在角内 radius 像素处)
- 角内 (距圆心 < radius): keep, alpha 不变
- 角外 (距圆心 > radius): discard
- 边缘 (≈ radius): smoothstep AA 抗锯齿

#### C. 兼容 T-0610 part 1 alpha
- corner discard 让"角外像素 alpha=0", compositor 视为完全透明 (桌面透出)
- corner 内像素仍走 clear color alpha=0.85, compositor 半透明 blend
- alpha_mode = PreMultiplied 已选, alpha 计算正确

#### D. titlebar / tab_bar / cell / glyph 全 pipeline 接 corner mask
- titlebar (cell pipeline) 顶部圆角生效 — 视觉上 titlebar 圆角不切割 cell 区
- 底部 cell 区 / tab_bar 也圆角
- 4 角全圆 (top-left/top-right/bottom-left/bottom-right 都 radius=8)
- glyph 字形如果落在圆角区也 discard (字形不超角自然不触发, 但 shader 必加
  corner mask 防 edge case)

#### E. resize 时 uniform 更新
- Renderer::resize 走 surface_w/surface_h 更新, uniform 跟随重写
- T-0802 节流路径同步 (节流命中跳过 propagate, uniform 也跳, 下次再写)

#### F. 测试
- src/wl/render.rs lib 单测:
  - corner_distance fn 4 角 case + 中心 case + 边缘 case
  - corner mask discard 决策 (radius=8, 不同 frag pos)
  - uniform layout struct std140 兼容性
- 集成测试 tests/rounded_corners_e2e.rs PNG verify:
  - 渲染空 surface, 4 角应有 alpha=0 像素 (透明), 中心区域 alpha=0.85
  - radius 8 logical = 16 physical, 顶角 (0..16, 0..16) 范围内有 alpha=0 像素
- 手测: cargo run --release, 看 quill 4 角是否圆形

### Out

- **不做**: 圆角 radius 用户配置 (硬编 8 logical, Phase 7+ config 时再开)
- **不做**: 不同角不同 radius (4 角统一 radius)
- **不做**: 阴影 (drop shadow, mutter 自己处理 server-side, quill 不画)
- **不做**: GTK4 风毛玻璃 background blur (compositor 协议 ext-background-effect 还
  未稳定, 派单 Out)
- **不动**: src/text / src/pty / src/ime / src/wl/keyboard / src/wl/pointer / docs/invariants.md / docs/adr / Cargo.toml

## Acceptance

- [ ] 4 门 release 全绿
- [ ] 4 角圆形 (radius 8 logical px) 视觉验
- [ ] 角外像素 alpha=0 透明 (PNG verify)
- [ ] 角内像素 alpha=0.85 半透明 (PNG verify)
- [ ] resize 时 corner radius 跟随保持
- [ ] cell / glyph / titlebar / tab_bar 全 pipeline 接 mask
- [ ] 总测试 379 + ≥5 ≈ 385+ pass
- [ ] **手测**: cargo run --release, quill 4 角圆形 + 半透明效果跟 GTK4 应用一致
- [ ] 三源 PNG verify (writer + Lead + reviewer)
- [ ] 审码放行

## 必读

1. /home/user/quill-impl-rounded-corners/CLAUDE.md
2. /home/user/quill-impl-rounded-corners/docs/conventions.md
3. /home/user/quill-impl-rounded-corners/docs/invariants.md (重点 INV-002 Renderer 字段顺序)
4. /home/user/quill-impl-rounded-corners/tasks/T-0610-rounded-corners.md (派单)
5. /home/user/quill-impl-rounded-corners/src/wl/render.rs (Renderer + cell/glyph pipeline + shader)
6. /home/user/quill-impl-rounded-corners/src/wl/window.rs (Renderer::resize 调用方)
7. /home/user/quill-impl-rounded-corners/tests/headless_screenshot.rs (PNG verify 现有套路)

## 已知陷阱

- **discard vs alpha=0**: discard 让 fragment 完全跳过, 不写 framebuffer; alpha=0
  写 alpha=0 给 compositor. 圆角外用 discard 更彻底, 但 anti-alias 边缘需要 alpha=0
  写入. 选 hybrid: corner_dist > radius+1 → discard, radius..=radius+1 → alpha 渐变写
- **uniform binding group 共享**: cell pipeline + glyph pipeline 都需要 corner mask
  uniform, 但它们当前各自 binding group 独立. 抽 shared bind group layout (group=1
  专门给 corner mask uniform), pipeline 都 bind 同一个
- **HiDPI corner_radius**: 8 logical × HIDPI_SCALE = 16 physical px. shader 内 frag_coord
  是 physical, uniform 传 physical radius
- **resize race**: T-0802 节流可能让 uniform 跟新 surface 尺寸短暂不一致, 接受 (圆角
  位置短暂不准 < 60ms 不影响 daily drive)
- **Renderer 字段加 uniform_buffer + bind_group**: INV-002 字段顺序需更新 (17 → 19),
  写 docs/invariants.md sync 留 Lead follow-up (派单 Out 不动 docs)
- **wgpu Buffer std140 layout**: uniform struct [f32; 4] 直接 16 字节对齐, 不需要 padding
- 不要 git add -A
- HEREDOC commit
- ASCII name writer-T0610

## 路由

writer name = "writer-T0610"。

## 预算

token=200k, wallclock=3-4h.
