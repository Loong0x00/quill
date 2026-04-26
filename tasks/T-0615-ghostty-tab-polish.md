# T-0615 ghostty / GTK4 风 tab UI polish (圆角 tab + + 按钮 box + 圆形 window button)

**Phase**: 7+ (polish, 跟 ghostty / libadwaita / GNOME 50 视觉对齐)
**Assigned**: writer-T0615
**Status**: in-review
**Budget**: tokenBudget=300k (跨 wgpu shader per-element corner mask + uniform
buffer 数组 + hit_test 圆形 button + 渲染层 quad → rounded rect 重构)
**Dependencies**: T-0608 (multi-tab 现状) / T-0610 part 2 (corner mask shader 现有)
**Priority**: P2 (polish, daily-drive 视觉)

## Goal

quill tab UI 跟 ghostty / libadwaita / GNOME 50 风对齐 (user 实测截图):
- **+ 按钮包圆角 box** (~6 px radius, hover 高亮)
- **Active tab 圆角矩形** (~6 px radius, 灰底 #444444 已有)
- **Inactive tab 透明背景** 仅 title + close × (已经基本对)
- **Min/Max/Close 圆形 button** (~12 px radius, hover 不同色)

## Bug / Pain

User 实测 quill 当前 tab UI 是 ghostty 风**简化版** (T-0608 KISS): 矩形
quad / 直角 / 裸 icon / 矩形按钮区. ghostty / GNOME 自带应用都是圆角化
+ 半透明 + button capsule. 视觉上 quill 像未完成品。

## Scope

### In

#### A. wgpu shader per-element corner mask
- 当前 `corner_uniform_buffer` 仅整 surface 4 角. 扩成 element list:
  ```wgsl
  struct CornerElement {
      x_min: f32, y_min: f32, x_max: f32, y_max: f32,
      radius: f32,
  }
  struct CornerMask {
      surface_size: vec2<f32>,
      surface_radius: f32,
      alpha_live: f32,
      element_count: u32,
      elements: array<CornerElement, 32>,  // upper bound
  }
  ```
- fragment shader: 算 frag 是否在任何 element 内, 在则按 element radius 算 corner
  distance, 外则 keep alpha. 不在任何 element 内走 surface corner mask 决策

#### B. + 按钮圆角 box
- 渲染: 圆角矩形 (~6 px radius) 作 background, + icon 居中
- hover 高亮 box bg 颜色变 (e.g. #555555 → #6a6a6a)
- 走 corner mask shader element

#### C. Active tab 圆角矩形
- 渲染: tab body 圆角 6 px radius (跟 + 按钮 box 一致)
- close × 居中右侧
- inactive tab 不画 box, 只画 title (透明背景)
- hover inactive tab 显示半透明圆角 box

#### D. Min/Max/Close 圆形 button
- 渲染: 圆形 (~12 px radius) 而非现有矩形区
- hover 高亮 (e.g. close hover 红 #cc4444)
- icon 居中 (Min: ─ / Max: ▢ / Close: ×)
- hit_test 改: 矩形区 → 圆形 (按 distance to center, < radius 视为 hit)

#### E. 测试
- src/wl/render.rs lib 单测: per-element corner uniform layout / shader 决策
- src/wl/pointer.rs lib 单测: 圆形 button hit_test (中心 hit / 边缘 hit / 角外 miss)
- 集成测试 tests/ghostty_tab_polish_e2e.rs PNG verify:
  - + 按钮 box 4 角圆 + hover 高亮
  - Active tab 4 角圆 + close × 居中右侧
  - Inactive tab 透明 + 只 title
  - Min/Max/Close 圆形 + 颜色梯度

### Out

- **不做**: 字体 ligature (派单已 Out)
- **不做**: tab drop animation / 拖拽 ghost 跟随 (T-0608 留 P3)
- **不做**: titlebar 居中显示路径 (Phase 7+ OSC title sync 时再考虑)
- **不做**: 主题 config (Phase 7+ config 时)
- **不动**: src/text / src/pty / src/ime / src/wl/keyboard / docs/invariants.md (除 INV-002 字段计数追加 follow-up) / docs/adr / Cargo.toml

## Acceptance

- [ ] 4 门 release 全绿
- [ ] + 按钮包圆角 box (PNG verify)
- [ ] Active tab 4 角圆 (PNG verify)
- [ ] Inactive tab 透明无 box (PNG verify)
- [ ] Min/Max/Close 圆形 button (PNG verify + hit_test 单测)
- [ ] 总测试 426 + ≥10 ≈ 436+ pass
- [ ] **手测**: cargo run --release, 视觉跟 ghostty / GNOME terminal 一致
- [ ] 三源 PNG verify
- [ ] 审码放行

## 必读

1. /home/user/quill-impl-ghostty-tab/CLAUDE.md
2. /home/user/quill-impl-ghostty-tab/docs/conventions.md
3. /home/user/quill-impl-ghostty-tab/docs/invariants.md (重点 INV-002 字段顺序)
4. /home/user/quill-impl-ghostty-tab/tasks/T-0615-ghostty-tab-polish.md (派单)
5. /home/user/quill-impl-ghostty-tab/src/wl/render.rs (corner mask shader 现状, append_tab_bar_vertices, append_titlebar_vertices, 主审码点)
6. /home/user/quill-impl-ghostty-tab/src/wl/pointer.rs (hit_test 现状, button 矩形区)
7. /home/user/quill-impl-ghostty-tab/src/tab/mod.rs (TabInstance 现状)

## 已知陷阱

- **wgpu uniform 数组上限**: std140 layout array max ~64 KB, 32 element × 5 f32 (20 bytes)
  + padding ≈ 1 KB 远低. 但更新频率每帧 (resize / hover / tab 增删) 走 queue.write_buffer
- **shader 分支 cost**: fragment 在 element list 循环找命中, 32 element × O(1) cmp = ~32 ops/frag, HiDPI 6K 分辨率 ~24M frag, 总 ~768M ops/frame, 5090 GPU 不卡
- **corner mask 优先级**: surface corner > element corner. element 在 surface corner 区
  内的部分 (e.g. tab 在 surface 顶部右上角) 走 surface 圆角 discard
- **per-element radius**: 不同 element 不同 radius (window button 12 px / tab body 6 px /
  + button box 6 px), uniform 数组每 element 独立
- **hit_test 圆形**: 按 distance to center, < radius 视为 hit. 跟矩形 hit_test 解耦
- **Tab spacing**: 圆角 tab 之间需要 gap (~4 px), 跟 ghostty 视觉一致
- **INV-002 字段计数**: corner_uniform_buffer 内容变 (变大), 字段顺序不变, INV-002 entry
  doc 不需要更新 (字段数仍 20)
- 不要 git add -A
- HEREDOC commit
- ASCII name writer-T0615

## 路由

writer name = "writer-T0615"。

## 预算

token=300k, wallclock=4-5h.
