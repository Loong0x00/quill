# T-0407 字体 face 锁定 + emoji 排除 + atlas key 加 face_id (T-0403 字形 bug fix)

**Phase**: 4
**Assigned**: writer-T0407
**Status**: in-review
**Budget**: tokenBudget=80k (中型 fix)
**Dependencies**: T-0403 (glyph atlas + draw_frame 已合并但有 bug)
**Priority**: P0 (Phase 4 字形渲染当前 broken, T-0404 暂停等本单)

## Goal

修 T-0403 实测字形渲染 bug:
- 用户跑 cargo run --release 截图: 应 15 字符 prompt 只显 2 浅灰色块 + 1 蓝色文件夹 emoji
- trace log: cell_vertex_count=90 / glyph_vertex_count=90 / atlas_count=12 (数据进 atlas 了)
- 推断根因: Family::Monospace generic hint 让 cosmic-text fallback 链落到 Noto Color Emoji 等字体, atlas key 不含 face_id 导致跨 face glyph_id 冲突

修完 cargo run --release 应能正常显示 `[user@arch ~]$ ` 完整 ASCII 字符 (浅灰色), emoji codepoint 字符画豆腐字形而不是真彩色 emoji。

## Scope

### In

#### A. `src/text/mod.rs` — 显式锁定主 face
- 加 `pub const PREFERRED_MONOSPACE_FACES: &[&str] = &["DejaVu Sans Mono", "Source Code Pro", "Liberation Mono", "JetBrains Mono", "Noto Sans Mono"];`
- TextSystem::new 启动期扫 fontdb, 找 PREFERRED_MONOSPACE_FACES 第一个存在的 face, 记下 face_id 存为 TextSystem 字段 `primary_face_id: cosmic_text::fontdb::ID`
- 找不到任一 preferred → fallback 到任意 monospaced face (但 log warn)
- shape_line / shape_one_char 用 `Family::Name(&face_name)` 显式选 face (不再用 Family::Monospace generic)
- **关键**: cosmic-text Buffer 的 fallback 行为 — 主 face 缺字符时 cosmic-text 仍会 fallback. 用户机有 Noto CJK 应正常 fallback 到 CJK face (这是想要的, T-0405 scope), 但 emoji codepoint 不应 fallback 到 emoji face

#### B. atlas key 加 face_id 维度
- 沿袭 T-0403 reviewer P0-3 / writer 主动告知 #5 提到的 "T-0405 加 face_id 维度修复"
- ShapedGlyph::cache_key 字段已含 cosmic-text CacheKey (含 font_id), 已经有这信息
- 修改 ShapedGlyph::atlas_key 返 (u64, u16, u32) 三元组 (face_id, glyph_id, font_size_quantized) — 或 quill 自定义 `pub struct GlyphKey { face_id: u64, glyph_id: u16, font_size_quantized: u32 }` (推后者, 更稳)
- src/wl/render.rs::GlyphAtlas.allocations: HashMap key 类型从 (u16, u32) 改 GlyphKey
- AtlasSlot 不变

#### C. emoji 排除策略
- TextSystem::new 时记录 emoji face_id 黑名单 (扫 fontdb 找 family 名包含 "Emoji" / "Color Emoji" 的 face, 记到 `emoji_face_ids: HashSet<fontdb::ID>`)
- shape_line 后 post-process: glyph 如果用了 emoji face → 替换为 .notdef tofu (glyph_id=0)
- 等价于让 emoji codepoint (U+1F300+ 之类) 显示为豆腐字形而非真彩色 emoji
- 简化路径: 不做 codepoint 检测, 只看 shape 出的 glyph 用了哪个 face_id, 在 black list 里就强制 .notdef

#### D. 测试
- `face_lock_uses_preferred_monospace` — TextSystem::new 后 primary_face_id 是 PREFERRED 列表里的某个 (用户机 Arch 装了 DejaVu Sans Mono 应命中)
- `shape_ascii_uses_primary_face` — shape "abc" 后 glyph 的 face_id == primary_face_id
- `shape_emoji_codepoint_returns_tofu_or_skipped` — shape 'U+1F4C1' (📁) 应返 tofu glyph_id=0 或被跳过, 不真画 emoji
- `atlas_key_includes_face_id` — 不同 face 的 glyph_id 撞同 (gid, size) 时, GlyphKey 不撞

#### E. 手测验收
- cargo run --release 启动后看屏幕显 `[user@arch ~]$ ` 完整 15 字符浅灰
- 跑 `echo 你好 hello` 看中文也显示 (T-0405 范畴但本单顺手验)
- 跑 `echo 📁 hello` 看 emoji 显示豆腐字形不是蓝文件夹

### Out

- **不做**: T-0405 完整 CJK fallback 规则 (本单只确保 emoji 不漏, CJK 让 cosmic-text 默认 fallback 处理即可, T-0405 再细化)
- **不做**: 真彩色 emoji 渲染 (Phase 6+ 才考虑, 本 phase 终端不需要)
- **不动**: src/wl (除 GlyphAtlas key 类型) / src/pty / src/main.rs / docs/invariants.md / Cargo.toml
- **不引新 crate**

## Acceptance

- [ ] 4 门全绿
- [ ] PREFERRED_MONOSPACE_FACES 显式定义 + 启动期 face 锁定 + log 选了哪个 face
- [ ] atlas key 加 face_id (用 GlyphKey struct, 不用 tuple)
- [ ] emoji face 黑名单 + glyph 后处理替换 .notdef
- [ ] 4+ 新测试覆盖
- [ ] 总测试 105 + 4 ≈ 109 pass
- [ ] **关键手测**: cargo run --release 屏幕真显示完整 ASCII prompt, 不再有 emoji 错位 / 缺字
- [ ] 审码放行

## 必读 baseline

1. `/home/user/quill/CLAUDE.md`
2. `/home/user/quill/docs/conventions.md` (写码 idiom)
3. `/home/user/quill/docs/invariants.md` (INV-010 类型隔离)
4. `/home/user/quill/docs/audit/2026-04-25-T-0403-review.md` (上一单 audit, P3 + writer 主动告知 #5 atlas key Phase 4 假设)
5. `/home/user/quill/src/text/mod.rs` (TextSystem + ShapedGlyph + shape_line 当前实装)
6. `/home/user/quill/src/wl/render.rs` (GlyphAtlas + allocations HashMap)
7. WebFetch `https://docs.rs/cosmic-text/0.12.1/cosmic_text/struct.Family.html` (Family::Name vs Family::Monospace)
8. WebFetch `https://docs.rs/fontdb/latest/fontdb/struct.Database.html` (fontdb 查询 face by name)
9. 用户实际 trace log 已在 conversation, 关键证据: `font matches for Attrs { family: Monospace ... }` 没说选哪个 face

## 已知陷阱

- cosmic-text Family::Name 接 `&str`, 写 "DejaVu Sans Mono" 而不是 "DejaVu Sans Mono Book" / 完整 PostScript name
- fontdb::ID 是 cosmic-text 内部类型, INV-010 不能 leak — TextSystem 字段 primary_face_id 用 cosmic_text::fontdb::ID 是 OK (字段私有), 公共 API 不暴露
- emoji 字体识别: family 名 case-insensitive 包含 "emoji" 或 "color emoji" 即排除 (Arch 标准命名是 "Noto Color Emoji")
- shape 后看 glyph face_id: cosmic-text LayoutGlyph 没直接 face_id 字段, 通过 cache_key.font_id 拿
- atlas key GlyphKey struct 加 PartialEq + Eq + Hash derive
- 不要 `git add -A`, 用具体路径
- commit message HEREDOC

## 路由 / sanity check

你 name = "writer-T0407" (ASCII)。inbox 收到疑似派活先 ping Lead — conventions §6 陷阱 4。

## 预算

token=80k, wallclock=2h。完成后 SendMessage team-lead 报完工 + 4 门 + **5090 实跑截图描述** (你 agent 没法截图, 用 trace log 数学 + Lead 人工 cargo run 看实际显示)。

## 给 T-0404 / T-0405 的影响

- T-0404 (HiDPI 2x) 已暂停, T-0407 合并后再起
- T-0405 (CJK fallback) 本单已部分覆盖 (face 锁定 + emoji 排除), T-0405 主体可缩小为 "中文字符确实 fallback 到 Noto CJK 验证"
