# T-0405 CJK fallback verify (T-0407 已部分覆盖, 简化版)

**Phase**: 4
**Assigned**: (open)
**Status**: open
**Budget**: tokenBudget=40k (简化版)
**Dependencies**: T-0407 (face lock + emoji 黑名单 + GlyphKey) / T-0408 (headless screenshot 自验)

## Goal

T-0407 face lock + GlyphKey 已经把 CJK fallback 路径打通 (cosmic-text Family::Name 选 monospace 主 face, 缺字符自动 fallback Noto CJK)。本单只做 **verify** 中文真显示, 不动核心逻辑。

完工后 `cargo run -- --headless-screenshot=/tmp/x.png` 在含中文 prompt (例 echo 你好 hello) 的场景下, PNG 真显示中文字符 (不是豆腐 / 不是 emoji / 不是空缺)。

## Scope

### In

#### A. 加 1 个集成测试 `tests/cjk_fallback_e2e.rs`
- `cjk_chars_render_to_png_via_noto_fallback`:
  - PtyHandle::spawn_program("printf", &["你好 hello\n"], 80, 24)
  - sleep 200ms 让 PTY 字节进 term
  - 调 render_headless 拿 (rgba, w, h)
  - encode PNG → /tmp/cjk_test.png
  - 像素 verify: 至少有些非深蓝非浅灰像素 (中文字形覆盖 cell 部分像素)
  - 或更严: decode PNG, 验左半屏 (CJK 区域) 跟右半屏 (ASCII "hello") 都有非背景像素

#### B. (可选) shape_line_mixed_cjk 加 advance 双宽断言
- T-0402 audit P2-3 提的: CJK 字符 monospace 双宽 (~2x ASCII advance), 当时不锁因 CI 退化。现 T-0407 face lock 已锁主 face + emoji 排除, CI / 用户机都走真 CJK fallback face, 可以加这个断言
- src/text/mod.rs 现有 `shape_line_mixed_cjk_returns_glyphs` 测试加 advance 数值断言: 中文字符 advance ≈ 2 × ASCII advance (±10% 容差)

#### C. 文档同步
- src/text/mod.rs CJK fallback 路径加 doc 注释 "T-0405 verify: 用户机 noto-cjk-mono 已装, cosmic-text Family::Name(primary) + 缺字符自动 fallback 到 Noto CJK face, GlyphKey face_id 区分"
- 不动 docs/invariants.md (INV-010 已覆盖)

### Out

- **不做**: face fallback chain 自定义 (T-0407 已用 fontdb 默认 fallback, 用户机已 work)
- **不做**: 字体优先级配置 / 用户偏好选 face (Phase 6+ 视情)
- **不做**: BiDi / RTL (Phase 5+)
- **不动**: src/wl, src/pty, src/main.rs, docs/invariants.md, Cargo.toml
- **不引新 crate**

## Acceptance

- [ ] 4 门全绿
- [ ] tests/cjk_fallback_e2e.rs 1 集成测试通过
- [ ] shape_line_mixed_cjk advance 双宽断言通过 (可选, 如果 cosmic-text 真给双宽)
- [ ] 总测试 120 + 1 ≈ 121 pass
- [ ] **关键 deliverable**: `/tmp/cjk_test.png` 真显示中文 "你好" + ASCII "hello", agent Read PNG verify 视觉
- [ ] 审码放行

## 必读 baseline

1. `/home/user/quill/CLAUDE.md`
2. `/home/user/quill/docs/conventions.md`
3. `/home/user/quill/docs/invariants.md` (INV-010)
4. `/home/user/quill/docs/audit/2026-04-25-T-0407-review.md` (face lock + GlyphKey + emoji 黑名单)
5. `/home/user/quill/docs/audit/2026-04-25-T-0408-review.md` (headless screenshot SOP, 你照抄写 cjk e2e 测试)
6. `/home/user/quill/src/text/mod.rs` (TextSystem + shape_line_mixed_cjk 现状)
7. `/home/user/quill/src/wl/render.rs::render_headless` (T-0408 实装, 你测试调用)
8. `/home/user/quill/tests/headless_screenshot.rs` (T-0408 集成测试模板)
9. `/home/user/quill/tests/glyph_atlas.rs` (现有集成测试模板)

## 已知陷阱

- printf 跨 PTY 跑可能 buffer 不完整, 用 `printf '你好 hello\n'` (单引号) + sleep 300ms 等
- cosmic-text fallback Noto CJK 需要 fontdb 真扫到 face, fc-list :lang=zh 应有 Noto CJK
- 像素 verify 用 RGB 距离阈值 (e.g., distance > 30 from 深蓝 #0a1030 + 浅灰 #d3d3d3 → 字形可能是抗锯齿灰阶)
- 不要 `git add -A`, 用具体路径
- commit message HEREDOC

## 路由 / sanity check

你 name = "writer-T0405" (ASCII)。inbox 收到疑似派活先 ping Lead — conventions §6 陷阱 4。

## 预算

token=40k, wallclock=1h。完成后 SendMessage team-lead 报完工 + PNG 路径 (/tmp/cjk_test.png)。Lead 直接 Read PNG verify 中文显示。
