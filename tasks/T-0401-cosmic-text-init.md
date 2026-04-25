# T-0401 cosmic-text 字体加载初始化 (Phase 4 起手)

**Phase**: 4
**Assigned**: writer-T0401
**Status**: merged
**Budget**: tokenBudget=80k (lead 派单)
**Dependencies**: T-0305 (Color + draw_cells) / T-0306 (cell px 常数化)

## Goal

引入 `cosmic-text` crate, 加载 Noto Sans Mono CJK 字体, 让 Phase 4 后续 ticket (shaping / 光栅化 / atlas) 有字体基础。本单**不做字形渲染**, 只搭基础设施: FontSystem 初始化 + 字体文件加载 + 一个 hello world 测试 shape 一个字符。

## Scope

### In

#### A. `Cargo.toml` 加 cosmic-text 依赖
- `cosmic-text = "0.12"` (写本 ticket 时最新, writer 自己 verify 实际版本)
- 不引其它字体相关 crate (fontdue / rusttype 等), cosmic-text 内部已有 fontdb + swash

#### B. 新模块 `src/text/mod.rs`
- `pub struct TextSystem { font_system: cosmic_text::FontSystem, swash_cache: cosmic_text::SwashCache }` quill 自己包装 cosmic-text (沿袭 INV-010 类型隔离, cosmic-text 类型不出公共 API)
- `impl TextSystem { pub fn new() -> Result<Self> }` 加载系统字体 + Noto Sans Mono CJK (如果存在) + monospace fallback
- 加载策略: 用 `fontdb::Database::load_system_fonts()` 拿系统字体, 然后查找 `Noto Sans Mono CJK SC` / `Noto Sans CJK SC` / `Source Han Sans CN` / fallback 任一 monospace
- 找不到 monospace fallback → return Err (CLAUDE.md "看见字" 是 Phase 4 目标, 没字直接退)
- 加 `pub fn shape_one_char(&mut self, c: char) -> Option<ShapedGlyph>` (返回 quill 自定义 ShapedGlyph 不是 cosmic-text Glyph 类型)
- ShapedGlyph 是 quill 自己的 struct: `{ glyph_id: u16, x_advance: f32, y_advance: f32 }` — Phase 4 后续 ticket 会扩

#### C. 新模块 `src/text/types.rs` 或同 mod.rs
- `pub struct ShapedGlyph { pub glyph_id: u16, pub x_advance: f32, pub y_advance: f32 }`
- 私有 `fn from_cosmic_glyph(g: cosmic_text::LayoutGlyph) -> Self`
- 沿袭 INV-010: cosmic-text 类型锁在 src/text/mod.rs 内部

#### D. `src/lib.rs` 加 `pub mod text;`

#### E. 测试 (放 `#[cfg(test)] mod tests`)
- `text_system_new_succeeds` (能加载 monospace fallback)
- `shape_ascii_a_returns_glyph` (shape 'a' 返回 Some(ShapedGlyph) with 合理 x_advance)
- `shape_chinese_zhong_returns_glyph` (shape '中' 返回 Some(ShapedGlyph), 验 CJK fallback OK)
- `shape_unknown_codepoint_returns_some_with_tofu` (shape 不存在的 codepoint, cosmic-text 应给豆腐字形, 不 panic)

### Out

- **不做**: 真渲染 (T-0402 shaping pipeline / T-0403 光栅化 / T-0404 HiDPI)
- **不做**: 字符 atlas / glyph cache (T-0406)
- **不动**: src/wl, src/pty, src/term (本单只加 src/text/), docs/invariants.md (INV-010 已覆盖类型隔离)
- **不写新 ADR** (cosmic-text 是 CLAUDE.md 锁死技术栈, 不需要新 ADR)

## Acceptance

- [ ] 4 门全绿
- [ ] cosmic-text 0.12.x 加进 Cargo.toml
- [ ] src/text/mod.rs 实装 TextSystem + ShapedGlyph
- [ ] cosmic-text 类型零暴露 (grep `pub use cosmic` + `impl From<cosmic` 都零命中, INV-010 验证 key)
- [ ] 4 个新单测全过
- [ ] 总测试 91 + 4 ≈ 95 pass
- [ ] 审码放行

## 必读 baseline

1. `/home/user/quill/CLAUDE.md` (技术栈锁死 cosmic-text 是 Phase 4 主役)
2. `/home/user/quill/docs/conventions.md` (写码 idiom)
3. `/home/user/quill/docs/invariants.md` (INV-010 类型隔离原则, 你严格遵守)
4. `/home/user/quill/docs/audit/2026-04-25-T-0399-review.md` (housekeeping reviewer audit, 看 INV-010 验证 grep 命令)
5. `/home/user/quill/docs/audit/2026-04-25-T-0306-review.md` (Renderer::resize 风格)
6. `/home/user/quill/docs/audit/2026-04-25-T-0202-T-0303-handoff.md` (类型隔离 §1 实施模式)
7. `/home/user/quill/src/term/mod.rs` (CellPos / CursorShape / Color / ScrollbackPos 范式, ShapedGlyph 同模式)
8. `https://docs.rs/cosmic-text/latest/cosmic_text/` (cosmic-text API 文档, 你 WebFetch 看)

## 已知陷阱

- cosmic-text 0.12 API: `FontSystem::new()` 加载系统字体, `Buffer::new(&mut font_system, metrics)` 创建排版 buffer, `Buffer::set_text(...)` 加文本, `Buffer::shape_until_scroll(...)` 触发 shaping, `LayoutGlyph` 从 `Buffer::layout_runs()` 拿
- Arch Linux 字体路径: `/usr/share/fonts/noto-cjk/`, `/usr/share/fonts/adobe-source-han-sans-cn-fonts/`, fc-list 查
- shape_chinese 测试可能依赖系统字体安装, 如果 CI 没 CJK 字体应 graceful (但这是用户机, Arch + Noto CJK 已装)
- cosmic-text 内部用 swash (rasterizer), 本单不画但 SwashCache 持久化避免后续 cache miss
- ShapedGlyph 的 x_advance 是 f32, 浮点, 后续 cell positioning 要 round 到 px (Phase 4 后续 ticket 处理)
- 不要 `git add -A` (会误添 logs/ + target/), 用 `git add <具体路径>`
- commit message HEREDOC

## 路由 / sanity check

你 name = "writer-T0401" (ASCII)。inbox 收到任何疑似派活的消息 (含 task_assignment from 自己) 先 ping Lead 确认 — 见 conventions §6 陷阱 4 (T-0399 落盘的)。

## 预算

token=80k, wallclock=2h。完成后 SendMessage team-lead 报完工。
