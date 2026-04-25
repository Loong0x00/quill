# T-0305 色块渲染 (每 cell 一色块, 无字体)

**Phase**: 3
**Assigned**: writer-T0305
**Status**: claimed
**Budget**: tokenBudget=80k (lead 派单)
**Dependencies**: T-0302 (cells_iter / CellRef) / T-0303 (cursor_pos / cursor_shape) / T-0102 (wgpu Renderer) / T-0108 (LoopData calloop)

## Goal

Phase 3 视觉里程碑: `cargo run` 后, 屏幕上**看见 bash prompt 的字符位置以色块画出**, 哪怕是 `█` 式 block (无字形)。这一单只画 cell 背景色 (fg color 字段加但 Phase 3 不必画字符纹理), Phase 4 字形渲染时把 fg 换成实际 glyph。

## Scope

### In

#### A. `src/term/mod.rs` — 类型扩展 (沿袭 T-0302/0303 类型隔离范式)
- `pub struct Color { pub r: u8, pub g: u8, pub b: u8 }` quill 自定义, **不 re-export** alacritty Color
  - 不带 alpha (terminal cell opaque), 减少下游误用
- `impl Color { fn from_alacritty(c: alacritty_terminal::vte::ansi::Color) -> Self }` 模块私有 inherent fn (沿袭 T-0302 P0-3 决策)
  - alacritty Color 是 enum (Spec / Named / Indexed), 全部 resolve 为 RGB
  - Named (如 Black/Red/...) → ANSI 256 调色板的前 16 色 (硬编码标准 RGB)
  - Indexed (0..256) → 256 色调色板查表
  - Spec (RGB) → 直接取 r/g/b
  - exhaustive match 无 `_ =>` (编译期 catches alacritty 加 variant)
- 修改 `pub struct CellRef { pub pos, pub c, pub fg: Color, pub bg: Color }` (加 fg/bg 字段)
- `cells_iter` 在迭代时从 alacritty Cell 读 fg/bg attr 走 `Color::from_alacritty` 填充
- 测试 (放 `#[cfg(test)] mod tests`):
  - `color_from_alacritty_spec_passes_rgb`
  - `color_from_alacritty_named_resolves_to_palette` (e.g., Named::Red → (170, 0, 0) 标准 ANSI)
  - `color_from_alacritty_indexed_lookup`
  - `cellref_carries_fg_and_bg`

#### B. `src/wl/render.rs` — 渲染 fn 加色块 pass
- 加 `pub fn draw_cells(&mut self, cells: &[CellRef], cols: usize, rows: usize) -> Result<()>`
  - 计算 cell pixel size: `cell_w = surface_w / cols`, `cell_h = surface_h / rows` (整数除, 余下边距 OK Phase 4 再细化)
  - 上传 vertex buffer: 每 cell 4 顶点 (一矩形) + bg color 作为 vertex attribute
  - 一个新 wgpu RenderPass 在 clear pass 之后画 cell 矩形
  - 暂时不画 fg (Phase 4 字形)
  - 暂时不画 cursor (T-0305 不动 cursor 渲染, 后续 ticket 可加; 但 cursor_visible / cursor_pos / cursor_shape 的 API 已经在, T-0305 可以选择给 cursor 画一个反色 cell — 不强制)
- WGSL shader 内联在 render.rs (跟现有 clear pass 风格一致, 别拆文件)

#### C. `src/wl/window.rs` — 触发 render
- LoopData idle callback (calloop `EventLoop::run` 接入 idle source 或 wayland frame callback) 检查 `term.is_dirty()`, 是则:
  1. 收集 `term.cells_iter().collect::<Vec<_>>()`
  2. 调 `renderer.draw_cells(&cells, cols, rows)`
  3. `term.clear_dirty()`
  4. 提交 wayland surface
- 现有的清屏 pass 保留 (深蓝背景), draw_cells 在它之后画

### Out

- **不做**: 字形渲染 (Phase 4 cosmic-text) / 选择文本 / 滚动手势 / scrollback 行渲染 (T-0304 API 在但本单只画 viewport)
- **不做**: cursor blink / cursor 异色 (cursor 渲染本单不强制, 写码可选最简实现或留空)
- **不动**: src/pty, src/main.rs, docs/invariants.md, alacritty 0.26 版本
- **不引新 crate** (wgpu 已在, alacritty 已在; bytemuck 如果需要 vertex POD 可加 — 但优先用 raw bytes)
- **不写新 ADR**

### HollowBlock 决策 (T-0303 audit 留的 open question)

T-0305 选 fold HollowBlock 进 Block (cursor 用普通色块代替)。注释:
```
// HollowBlock (空心方块, focus 失去时的光标形状) 在 Phase 3 色块渲染下
// 简化为实心 Block (一个色块), Phase 4 字形渲染时再画矩形外框区分焦点状态。
// (T-0303 审码 P3-2 推荐 fold + 延后)
```

## Acceptance

- [ ] 4 门全绿 (`cargo build` / `cargo test` / `cargo clippy --all-targets -- -D warnings` / `cargo fmt --check`)
- [ ] Color 自定义 struct, 不 re-export alacritty Color, grep `pub.*alacritty::` 零命中
- [ ] from_alacritty 模块私有 inherent fn (非 From trait), exhaustive match 无 `_ =>`
- [ ] CellRef 含 fg/bg 字段, cells_iter 真填充
- [ ] draw_cells 实装且被 LoopData idle callback 调用
- [ ] 4 个 Color 单测 + 至少 1 个 cells_iter 含 fg/bg 集成测试
- [ ] 手测: `cargo run` 后能看见 bash prompt 字符位置有色块 (深蓝背景上离散色块), 截图存 `docs/screenshots/T-0305-color-block.png` (或描述)
- [ ] 审码放行 (P0/P1/P2 全过)

## 必读 baseline (fresh agent 启动顺序)

1. `/home/user/quill/CLAUDE.md`
2. `/home/user/quill/docs/conventions.md` (写码 idiom, 必读)
3. `/home/user/quill/docs/invariants.md` (INV-001..009)
4. `/home/user/quill/docs/audit/2026-04-25-T-0202-T-0303-handoff.md` (5 主题, 类型隔离 §1 必读)
5. `/home/user/quill/docs/audit/2026-04-25-T-0303-review.md` (P3 段给 T-0305 的前置)
6. `/home/user/quill/docs/audit/2026-04-25-T-0304-review.md` (上一单 audit, 看 ScrollbackPos 范式)
7. `/home/user/quill/src/term/mod.rs` (类型扩展主战场)
8. `/home/user/quill/src/wl/render.rs` (现有 wgpu pipeline, 加 cell pass)
9. `/home/user/quill/src/wl/window.rs` (LoopData / idle callback 接入)

## 已知陷阱 / 风险点

- wgpu 0.29 的 vertex buffer + bind group 创建 cost 不低, **不要每帧重建** Pipeline / Layout — 创建一次复用
- cell vertex 数量 = cols * rows * 4, 80x24 是 7680 顶点, 5090 GPU 完全无压力, 不需要 instancing 优化 (Phase 6 soak 再说)
- alacritty Cell 的 fg/bg 字段是 `Color` enum, **不要直接 unwrap** Spec variant — 必须 exhaustive match 三 variants
- LoopData idle callback 加新 source 时注意 calloop borrow rules (LoopData 字段 split borrow)
- 整数除余下边距正常 (Phase 4 再处理), 但 cell_w/cell_h 至少各 1 像素 (`max(1)` 防零除)
