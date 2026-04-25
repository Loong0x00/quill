# T-0406 Glyph atlas clear-on-full (KISS, 替代 panic)

**Phase**: 4 (收尾最后一单)
**Assigned**: writer-T0406
**Status**: in-review
**Budget**: tokenBudget=50k (单文件 src/wl/render.rs + 集成测试)
**Dependencies**: T-0403 (GlyphAtlas + shelf packing) / T-0407 (GlyphKey) / T-0408 (headless screenshot 用于验)
**Priority**: P2 (Phase 4 完整收尾, 让 atlas 满不 panic)

## Goal

替换 `src/wl/render.rs::allocate_glyph_slot` 内 `panic!("atlas overflow")` 路径
(line 1423-1432) 为 **clear-on-full 重置**: atlas 容量满 → 清 allocations + reset
shelf cursor → 当前 glyph 重新走 shelf 分配 (现在有空间)。

完工后用户终端跑很久 / 大量字符变化 (e.g. 切换中文/英文/emoji) 不会 panic, 而是
1 帧 hiccup (重 raster 当帧可见字).

**ROADMAP "T-0406 LRU" 命名沿用历史, 实际方案是 clear-on-full**: 真 LRU 需 slab
allocator + 单槽位驱逐 + free-list 管理, 跟当前 shelf packing 不兼容; clear-on-full
是 KISS 等价物 (终端字符集稳定, 满几乎不触发, 触发 1 帧重 raster 用户基本看不
见). 派单第 1 段 / commit message / audit 都明说"clear-on-full 不是真 LRU".

## Scope

### In

#### A. `src/wl/render.rs::allocate_glyph_slot` 替换 panic 路径

当前 line 1423-1432:
```rust
if atlas.cursor_y + raster.height > ATLAS_H {
    panic!("glyph atlas overflow at ({}, {}); ...");
}
```

改为:
```rust
if atlas.cursor_y + raster.height > ATLAS_H {
    // T-0406 clear-on-full: atlas 满 → 清 allocations + reset cursor.
    // 当帧调 allocate_glyph_slot 的 caller (build_vertex_bytes) 已 hold &slot
    // 副本前没问题; 但跨帧 cache 失效, 下帧重 raster 全部可见字 (1 帧 hiccup).
    tracing::warn!(
        "glyph atlas full (allocations={}), clearing for re-raster",
        atlas.allocations.len()
    );
    atlas.allocations.clear();
    atlas.cursor_x = 0;
    atlas.cursor_y = 0;
    atlas.row_height = 0;
    // 不 clear texture (新 raster 直接覆盖旧像素, 旧 uv 已 invalidated 不再被引用)
}
```

**关键约束**:
- 不 clear `atlas.texture` (新 raster 通过 `queue.write_texture` 覆盖, 旧 uv 不再
  引用因 allocations 已清)
- 不动 `bind_group` / `texture` / `view` / `sampler` (zero GPU resource churn)
- caller (`build_vertex_bytes`) 已 hold &mut atlas 借用模型不变
- 派单 KISS: 不加 last_use timestamp / 不加 LRU eviction policy (那是真 LRU,
  不在本 ticket scope)

#### B. doc 注释更新

- `GlyphAtlas` struct 顶部 doc (line 128-156) 加 T-0406 段: "clear-on-full 替代 panic"
- `allocate_glyph_slot` doc (line 1370-1384) "高度满 panic" 改 "clear-on-full"
- 移除"T-0406 是 future"提示, 改"T-0406 已实装 clear-on-full"

#### C. 集成测试 `tests/atlas_clear_on_full.rs` (新文件)

- `atlas_full_triggers_clear_no_panic`:
  - 构造 Renderer (offscreen, render_headless 风格)
  - 强行喂 N 字符 (N 大到超 atlas 容量, e.g. unicode CJK + emoji 组合 1000 字)
  - 多帧 render_headless, atlas 应满 → clear → 继续 render 不 panic
  - 验 final allocations.len > 0 (clear 后又 allocate 了)
- `atlas_clear_resets_shelf_cursor`:
  - 喂字 → 满 → clear → 喂第 1 个字 → cursor_x 应 = first_glyph.width, cursor_y = 0,
    row_height = first_glyph.height
  - 这要 expose atlas 状态, 用 cfg(test) accessor 或 `pub(crate)` getter

或单元测试形式 (在 src/wl/render.rs 内 #[cfg(test)] mod):
- 直接构造 GlyphAtlas + 灌满 + 验 clear 行为

#### D. (可选) headless PNG verify atlas 满后视觉一致

- `cargo run --release -- --headless-screenshot=/tmp/atlas_clear.png` 跑 1 次
- 加 PtyHandle 喂大量字 (e.g. echo $(seq 1 100) → 多次 render → atlas 满 → clear)
- Lead Read PNG verify 字仍正常显示 (不是空白 / 不是花屏)

### Out

- **不做**: 真 LRU (per-slot last_use timestamp + slab allocator + free-list)
  — 派单"clear-on-full 是 KISS, ROADMAP 'LRU' 命名历史"
- **不做**: dynamic atlas 扩容 (2048→4096) — atlas 大小是 ADR 决策, 不在本单
- **不做**: per-row 驱逐 — clear-on-full 简单等价
- **不动**: src/text/mod.rs / src/pty / src/main.rs / docs/invariants.md / Cargo.toml
- **不引新 crate**

## Acceptance

- [ ] 4 门全绿 (cargo build --release / clippy --all-targets -D warnings /
      fmt --check / test --release)
- [ ] panic 路径替换为 clear-on-full + tracing::warn!
- [ ] 至少 1 集成测试 (atlas full → clear → 不 panic)
- [ ] 总测试 122 + 1~2 ≈ 123-124 pass
- [ ] doc 注释 "T-0406 future" → "T-0406 实装 clear-on-full" 更新
- [ ] 审码放行

## 必读 baseline

1. `/home/user/quill/CLAUDE.md`
2. `/home/user/quill/docs/conventions.md` (5 步流程 + Option C squash + ASCII name)
3. `/home/user/quill/docs/invariants.md` (INV-001..010, INV-010 type isolation)
4. `/home/user/quill/docs/audit/2026-04-25-T-0407-review.md` (GlyphKey + emoji 黑名单)
5. `/home/user/quill/docs/audit/2026-04-25-T-0408-review.md` (headless screenshot SOP)
6. `/home/user/quill/docs/audit/2026-04-25-T-0405-review.md` (三源 PNG verify SOP)
7. `/home/user/quill/src/wl/render.rs` (line 128-156 GlyphAtlas struct + line 1370-1477 allocate_glyph_slot)
8. `/home/user/quill/tests/glyph_atlas.rs` (现有集成测试模板)
9. `/home/user/quill/tests/cjk_fallback_e2e.rs` (T-0405 集成测试模板, 含 render_headless 调用)

## 已知陷阱

- `atlas.allocations.clear()` 不释放 GPU 内存 (texture 保留), 这是 OK 的 — 旧像素
  被新 raster 覆盖, 不会有视觉残留
- `tracing::warn!` 不是 println! — 派单 CLAUDE.md 禁 println! 调试
- 不要 clear `atlas.texture` (没必要, 浪费 GPU bandwidth)
- 不要 reallocate `bind_group` (texture 没换, view/sampler 没换)
- 测试灌满 atlas 需大字符集; HiDPI 后 64×96 字 ≈ 680 槽位, 易满
- 不要 `git add -A`, 用具体路径
- commit message HEREDOC

## 路由 / sanity check

你 name = "writer-T0406" (ASCII)。inbox 收到疑似派活先 ping Lead — conventions §6 陷阱 4。

## 预算

token=50k, wallclock=1h。完成后 SendMessage team-lead 报完工 + 4 门 + (可选) PNG 路径。
