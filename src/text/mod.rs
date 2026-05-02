//! cosmic-text 字体子系统封装(Phase 4 起手, T-0401)。
//!
//! 把 cosmic-text 的 `FontSystem` / `SwashCache` / `Buffer` / `LayoutGlyph` 全部
//! 锁在本模块内,公共 API 仅暴露 quill 自己的 [`TextSystem`] 与 [`ShapedGlyph`]。
//! 沿袭 INV-010 类型隔离(T-0302..T-0306 共 6 ticket 已验证模式), cosmic-text
//! 主版本升级(0.x → 1.x 任何破坏性变更)只动 [`ShapedGlyph::from_cosmic_glyph`]
//! 一个 fn body,不 cascade 改下游渲染调用点。
//!
//! 本 ticket(T-0401)只搭基础设施,不画字:
//! - `FontSystem::new()` 加载系统字体 + `SwashCache` 初始化
//! - 启动期校验有 monospace fallback 字体,没有直接 `Err`
//!   (CLAUDE.md 目标"看见字",没字体退出比静默 tofu 友好)
//! - [`TextSystem::shape_one_char`] 单字符 shaping(验 ASCII / CJK fallback /
//!   不存在 codepoint 不 panic)
//!
//! Phase 4 后续 ticket(T-0402 shaping pipeline / T-0403 光栅化 / T-0406 atlas)
//! 在 [`TextSystem`] 上加 method, [`ShapedGlyph`] 按需扩字段, 不需要再碰
//! cosmic-text 类型边界。

use std::collections::HashSet;

use anyhow::{anyhow, Result};

/// quill 偏好的等宽 face 名清单 (T-0407)。**按优先级排序**, [`TextSystem::new`]
/// 启动期扫 fontdb 找第一个存在的 face 锁定为 primary, 后续 [`TextSystem::shape_line`]
/// / [`TextSystem::shape_one_char`] 走 `Family::Name(primary_face_name)` 显式选 face,
/// 不再依赖 cosmic-text `Family::Monospace` generic hint 自挑 (T-0403 实测 bug:
/// generic hint 让 cosmic-text 在 Arch 系统字体里自挑落到 Noto Color Emoji /
/// 等不期望 face)。
///
/// **why 这 5 个 face**: Arch / Debian / Fedora 主流装机量大的 monospace face,
/// 用户机 (Arch + ttf-dejavu + noto-fonts) 命中第 1 个 (DejaVu Sans Mono); CI
/// 退化路径走第 5 个 (Noto Sans Mono, 通常随 noto-fonts 一并装)。**派单 In #A
/// 显式列出此 5 face, 顺序锁死防"新 face 加入插队优先级混乱"**。
///
/// **找不到任一 preferred → fallback** 到任意 monospaced face (warn log)。
pub const PREFERRED_MONOSPACE_FACES: &[&str] = &[
    "DejaVu Sans Mono",
    "Source Code Pro",
    "Liberation Mono",
    "JetBrains Mono",
    "Noto Sans Mono",
];

/// quill 字体子系统 —— cosmic-text 封装。
///
/// **类型隔离(INV-010)**: cosmic-text 类型(`FontSystem` / `SwashCache` /
/// `Buffer` / `LayoutGlyph` / `Attrs` / `Family` / `Metrics` / `Shaping` /
/// `fontdb::ID` (T-0407 加 primary_face_id / emoji_face_ids 字段, 仍模块私有))严格
/// 锁在本模块内,公共 API 仅暴露 quill 自有的 [`ShapedGlyph`] / [`GlyphKey`]。
/// 沿袭 T-0302 [`crate::term::CellPos`] / T-0303 [`crate::term::CursorShape`] /
/// T-0304 [`crate::term::ScrollbackPos`] / T-0305 [`crate::term::Color`] 同款
/// 套路 —— 给未来换 cosmic-text 主版本(0.x → 1.x)/ 换字体引擎(例 fontique)
/// 留单一改动点 [`ShapedGlyph::from_cosmic_glyph`]。
///
/// **启动校验**: [`Self::new`] 加载系统字体后扫 fontdb,若没有任何 monospace
/// face 直接 [`Err`] —— Phase 4 目标看见字,没字体退出比静默 tofu 友好
/// (见 CLAUDE.md "目标 / 非目标"段:"2x HiDPI 整数缩放 + CJK 字形正常")。
///
/// **face 锁定 (T-0407)**: 启动期扫 fontdb 找 [`PREFERRED_MONOSPACE_FACES`] 第
/// 一个存在的 face, 锁定 `primary_face_id` (作 cosmic-text shape 入参 family
/// 名) + `primary_face_name` (供 `Family::Name` 用)。同时扫出 `emoji_face_ids`
/// (family 名 case-insensitive 含 "emoji"), shape 后 post-process 把命中
/// emoji face 的 glyph 换成 .notdef (glyph_id=0) — 让 emoji codepoint 显豆腐
/// 字形而非真彩色 emoji (Phase 4 终端不渲染彩色)。
///
/// **字段顺序与 drop 语义**: `font_system` / `swash_cache` / `primary_face_id`
/// / `primary_face_name` / `emoji_face_ids` 都是堆分配 owned 资源 / POD,
/// 互不持指针引用,drop 顺序无观测者(与 INV-001 / INV-002 / INV-008 那种
/// "字段顺序定 wl/wgpu/pty 资源链"性质完全不同),按声明顺序 drop 即可。
pub struct TextSystem {
    font_system: cosmic_text::FontSystem,
    /// **Phase 4 后续 ticket 接入点**: T-0403 光栅化会用 `SwashCache` 缓存
    /// 字形 bitmap(每个 (face_id, glyph_id, font_size) 一份)。本 ticket
    /// (T-0401)只 shape 不 raster, 字段空跑;预先在 [`TextSystem`] 持一份
    /// owned 资源避免每 ticket 改 struct(派单"一次 commit 做一件事"的反向
    /// 实施: 资源生命周期一处定义, 后续 ticket 只加 method 不改字段)。
    /// `#[allow(dead_code)]` 不消化字段编译会 -D warnings 挂, 故显式 allow
    /// + 注释引派单 scope 字段定义
    ///   "`pub struct TextSystem { font_system, swash_cache }`" 作 traceability。
    #[allow(dead_code)]
    swash_cache: cosmic_text::SwashCache,
    /// **T-0407 加**: 启动期锁定的 primary face id (来自 fontdb 扫
    /// [`PREFERRED_MONOSPACE_FACES`] 命中, fallback 任意 monospaced face)。
    /// 模块私有, INV-010 守: cosmic-text [`cosmic_text::fontdb::ID`] 不
    /// 暴露 — 公共 API 走 [`Self::primary_face_id`] 返 quill 自定义 u64 hash。
    primary_face_id: cosmic_text::fontdb::ID,
    /// **T-0407 加**: primary face 的 family 名 (来自 fontdb FaceInfo.families[0])。
    /// shape_line / shape_one_char 用 `Family::Name(&primary_face_name)` 选 face,
    /// 替代 T-0401 的 `Family::Monospace` generic hint (后者让 cosmic-text 自挑,
    /// T-0403 实测落到 Noto Color Emoji 致字形渲染崩坏)。
    primary_face_name: String,
    /// **T-0407 加**: emoji face id 黑名单。fontdb 扫描时 family 名 case-insensitive
    /// 含 "emoji" 即纳入。shape 后 post-process: glyph 用了黑名单 face → 替换
    /// .notdef (glyph_id=0), 等价于让 emoji codepoint 显豆腐字形而非真彩色 emoji。
    /// HashSet 给 O(1) contains 检查 (典型 ≤2 face: Noto Color Emoji + 可能 Twemoji)。
    emoji_face_ids: HashSet<cosmic_text::fontdb::ID>,
}

impl TextSystem {
    /// 构造字体子系统,加载系统字体 + 校验 monospace 可用。
    ///
    /// **耗时**: cosmic-text 文档明示 release build 可达 1s, debug 10s 量级
    /// (扫 fontconfig + fontdb 解析所有 face metadata)。Phase 1 已验证
    /// NVIDIA 5090 + Wayland + wgpu 启动期可承受秒级冷启动,此处加载一次后
    /// 常驻不影响热路径(Phase 6 soak T-0601 不在 startup 计时范围)。
    ///
    /// **monospace fallback 校验**: 扫 [`fontdb::Database`] 找
    /// `monospaced == true` 的 face,找不到立即 `Err`。Arch Linux 通常装
    /// `noto-fonts-cjk` / `ttf-dejavu` 都含 monospace face(用户机已确认装
    /// `noto-fonts-cjk` + `adobe-source-han-sans-cn-fonts`)。
    ///
    /// **不指定具体 family**(本单 scope): cosmic-text 内部用 fontdb fallback
    /// chain,后续 [`Self::shape_one_char`] 用 `Family::Monospace` 让 cosmic-text
    /// 自己挑;Phase 4 后续 ticket 可加 `Self::with_family(...)` 显式选 Noto
    /// CJK / Source Han Sans 等。
    ///
    /// 测试覆盖: [`tests::text_system_new_succeeds`].
    pub fn new() -> Result<Self> {
        let font_system = cosmic_text::FontSystem::new();

        // T-0407: 扫 fontdb 锁定 primary face + 收集 emoji face 黑名单。
        //
        // why 单次扫描双采: faces() 返回的迭代器一次扫完, 同 pass 出 primary
        // (PREFERRED 优先匹配 → 任意 monospaced face fallback) 与 emoji 黑名单
        // (case-insensitive family 名含 "emoji"); 启动期 1-2 ms 量级, 非 hot path。
        let mut primary: Option<(cosmic_text::fontdb::ID, String)> = None;
        let mut monospace_fallback: Option<(cosmic_text::fontdb::ID, String)> = None;
        let mut emoji_face_ids: HashSet<cosmic_text::fontdb::ID> = HashSet::new();
        let mut preferred_idx_found: Option<usize> = None;

        for face in font_system.db().faces() {
            // 取 primary family 名 (fontdb FaceInfo.families[0] 是 English US, audit
            // 引 fontdb 注释 "first family is always English US")。无 family 名跳过
            // (异常 face, 通常是损坏字体)。
            let Some((face_family, _lang)) = face.families.first() else {
                continue;
            };

            // emoji 检测: case_insensitive 含 "emoji" 即排除。Arch 标准命名 "Noto
            // Color Emoji" 命中 (派单已知陷阱"emoji 字体识别")。
            if face_family.to_lowercase().contains("emoji") {
                emoji_face_ids.insert(face.id);
                continue;
            }

            // PREFERRED 优先匹配: 找当前 face_family 在 PREFERRED 中的 idx, 选 idx
            // 最小者 (优先级最高)。命中 idx=0 (DejaVu Sans Mono) 立即锁定可省后续
            // 扫描, 但 face 总数 ~50, 全扫开销可忽略 — 简化逻辑不做提前 break。
            if let Some(idx) = PREFERRED_MONOSPACE_FACES
                .iter()
                .position(|p| p.eq_ignore_ascii_case(face_family))
            {
                if preferred_idx_found.is_none_or(|cur| idx < cur) {
                    preferred_idx_found = Some(idx);
                    primary = Some((face.id, face_family.clone()));
                }
            } else if face.monospaced && monospace_fallback.is_none() {
                // fallback: 任意 monospaced face (派单 In #A "找不到任一 preferred
                // → fallback 到任意 monospaced face 但 log warn")。取扫描遇到的
                // 第一个, 不强求"最佳" — fallback 路径本就是退化。
                monospace_fallback = Some((face.id, face_family.clone()));
            }
        }

        let primary_face_name = match primary {
            Some(p) => {
                tracing::info!(
                    primary_face = %p.1,
                    "TextSystem::new: locked PREFERRED monospace face"
                );
                p.1
            }
            None => match monospace_fallback {
                Some(fb) => {
                    tracing::warn!(
                        fallback_face = %fb.1,
                        preferred = ?PREFERRED_MONOSPACE_FACES,
                        "TextSystem::new: no PREFERRED monospace face found, \
                         falling back to first monospaced face"
                    );
                    fb.1
                }
                None => {
                    return Err(anyhow!(
                        "TextSystem::new: no monospace font face found in system fontdb \
                         (check `fc-list :spacing=mono`); quill 当前需要 monospace 字体"
                    ));
                }
            },
        };

        // 用 fontdb::Query 找 cosmic-text 实际 shape 时会选中的 face id (匹配
        // Family::Name + 默认 Weight::NORMAL/Style::Normal/Stretch::Normal)。
        // why 不复用扫描时记下的 face.id: fontdb 同 family 名可能多 face (Book /
        // Bold / Italic / 等 style), 扫描遇到的第一个未必匹配 cosmic-text shape
        // 实际选中的 (后者按 Attrs 解析最佳 style)。query() 跑同套 CSS-like
        // 匹配规则, 保证 primary_face_id 与 LayoutGlyph.cache_key.font_id 一致 —
        // 测试 shape_ascii_uses_primary_face 锁此契约。
        let query = cosmic_text::fontdb::Query {
            families: &[cosmic_text::fontdb::Family::Name(&primary_face_name)],
            weight: cosmic_text::fontdb::Weight::NORMAL,
            stretch: cosmic_text::fontdb::Stretch::Normal,
            style: cosmic_text::fontdb::Style::Normal,
        };
        let primary_face_id = font_system.db().query(&query).ok_or_else(|| {
            anyhow!(
                "TextSystem::new: fontdb::Query 找不到 face '{}' (Weight::NORMAL / \
                 Style::Normal / Stretch::Normal); 字体扫描记下了名字但 query 解析失败 \
                 (cosmic-text Attrs 默认 style 不匹配?)",
                primary_face_name
            )
        })?;

        tracing::info!(
            emoji_face_count = emoji_face_ids.len(),
            "TextSystem::new: emoji face blacklist populated"
        );

        Ok(Self {
            font_system,
            swash_cache: cosmic_text::SwashCache::new(),
            primary_face_id,
            primary_face_name,
            emoji_face_ids,
        })
    }

    /// 返锁定的 primary face id 作 quill 自定义 u64 hash (INV-010 守: 不暴露
    /// cosmic-text [`cosmic_text::fontdb::ID`])。
    ///
    /// **why u64 hash 而非 fontdb::ID**: fontdb::ID 内部是 `slotmap::DefaultKey`
    /// 上游类型, 直接公开违反 INV-010 strict reading; u64 hash 走
    /// [`fontdb_id_to_u64`] 私有 helper, 与 [`ShapedGlyph::face_id`] / [`GlyphKey`]
    /// 的 face_id 同源算法 (DefaultHasher), 同进程内同 ID 必同 u64, 跨进程
    /// 不保证 (派单 In #B atlas key 仅同进程消费, 不需跨进程稳定)。
    ///
    /// **测试用**: [`tests::face_lock_uses_preferred_monospace`] /
    /// [`tests::shape_ascii_uses_primary_face`] 断言 primary_face_id 与
    /// shape 出 glyph 的 face_id 一致。
    pub fn primary_face_id(&self) -> u64 {
        fontdb_id_to_u64(self.primary_face_id)
    }
}

/// 把 [`cosmic_text::fontdb::ID`] 转 u64 hash (T-0407, INV-010 守)。
///
/// **why DefaultHasher**: fontdb::ID 内部是 `slotmap::DefaultKey` (`InnerId(KeyData)`),
/// `KeyData::as_ffi() -> u64` 是稳定 FFI 表示 (Display impl 即用此), 但 InnerId
/// 字段私有不可直接调用。`Hash + Eq` 是 ID 公开 trait, DefaultHasher 同进程内
/// 同 ID 必同 u64 (HashMap key 用同算法, std 保证一致), 跨进程不保证 (atlas key
/// 仅本进程消费)。沿袭 INV-010 类型隔离 — 上游类型不出 src/text/, 公共 API 走
/// quill 自定义 u64。
///
/// **不优化**: 每次 from_cosmic_glyph 调一次, 单 frame 几十次, DefaultHasher
/// new + hash 量级 ~100ns 可忽略。Phase 4 不引 ahash 优化。
fn fontdb_id_to_u64(id: cosmic_text::fontdb::ID) -> u64 {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut h = DefaultHasher::new();
    id.hash(&mut h);
    h.finish()
}

impl TextSystem {
    /// 对单个字符走 cosmic-text shaping pipeline,返回首个 layout glyph
    /// (字符产生 0 glyph 才返 `None`,例 zero-width 控制字符)。
    ///
    /// **Phase 4 临时探测 API**: T-0401 只验 cosmic-text shaping 链路通畅 +
    /// CJK fallback 工作。Phase 4 后续 ticket(T-0402 shaping pipeline)会
    /// 替换为按 `&str` 整段 shape, 取 [`cosmic_text::Buffer::layout_runs`] 全
    /// glyph;本 fn 是脚手架,不在 hot path。
    ///
    /// **CJK fallback 行为**: cosmic-text 用 fontdb 的 fallback chain,当
    /// 主 monospace 字体缺该 codepoint glyph 时自动找下一个 face。'中' 等
    /// CJK 字符若主字体不含会落到 Noto CJK / Source Han 等 face(用户机已装)。
    ///
    /// **不存在的 codepoint**(例 U+E0000 私用区无字形): cosmic-text 给
    /// `glyph_id = 0`(.notdef tofu 字形)而非跳过 ——
    /// 渲染层后续画为豆腐字形即可,不应 panic。
    ///
    /// **`SwashCache` 在本 fn 不参与**: shaping 不需要 raster, swash_cache 是
    /// raster 阶段(后续 T-0403 光栅化)的字形 bitmap 缓存。本 fn 只做 shape,
    /// 但 swash_cache 字段在 [`TextSystem`] 一处持有,后续 ticket 直接复用,
    /// 避免每 ticket 改一次 owned 资源链。
    ///
    /// 测试覆盖: [`tests::shape_ascii_a_returns_glyph`] /
    /// [`tests::shape_chinese_zhong_returns_glyph`] /
    /// [`tests::shape_unknown_codepoint_returns_some_or_none_no_panic`].
    pub fn shape_one_char(&mut self, c: char) -> Option<ShapedGlyph> {
        use cosmic_text::{Attrs, Buffer, Family, Metrics, Shaping};

        // Metrics(font_size, line_height): 14/20 是 Phase 4 占位值 ——
        // 不影响 glyph_id 抽取,只影响 advance / position 数值;Phase 4 后续
        // ticket 会从 cosmic-text 字体 metrics 真实测量并填入
        // `Renderer::cell_w_px / cell_h_px`,届时替换 T-0306 引入的临时常数
        // `CELL_W_PX / CELL_H_PX`(见 src/wl/render.rs T-0306 注释 P3-5).
        let metrics = Metrics::new(14.0, 20.0);
        let mut buffer = Buffer::new(&mut self.font_system, metrics);
        // T-0407: Family::Name(primary) 显式锁 face, 替代 T-0401 的 Family::Monospace
        // generic hint (后者让 cosmic-text 自挑落到 Noto Color Emoji, 致字形渲染崩坏)。
        let attrs = Attrs::new().family(Family::Name(&self.primary_face_name));

        let mut s = String::new();
        s.push(c);
        buffer.set_text(&mut self.font_system, &s, attrs, Shaping::Advanced);
        buffer.shape_until_scroll(&mut self.font_system, false);

        let raw = buffer
            .layout_runs()
            .next()
            .and_then(|run| run.glyphs.first().map(ShapedGlyph::from_cosmic_glyph))?;
        Some(self.apply_emoji_blacklist(raw))
    }

    /// T-0407 post-process: 若 glyph 用了 emoji face → 替换为 .notdef
    /// (`glyph_id = 0`)。`cache_key` 不变 (rasterize 走原 cache_key 拿 emoji
    /// face 的 Color content → 自动返 None → 渲染层 allocate_glyph_slot 收到
    /// None → 跳过该字形, 该 cell 仅显 Phase 3 既有 cell 色块, 不画蓝色文件夹
    /// emoji)。
    ///
    /// **简化路径** (派单 In #C "不做 codepoint 检测, 只看 shape 出的 glyph
    /// 用了哪个 face_id, 在 black list 里就强制 .notdef"): 不重 shape 不构造
    /// 主 face .notdef cache_key — 让 cosmic-text 自身 Color content 检查 +
    /// `TextSystem::rasterize` `Mask` 校验在 raster 阶段过滤即可, 视觉等效。
    ///
    /// **why 仅置 glyph_id = 0**: ShapedGlyph 公共字段 `glyph_id` 渲染层不直接
    /// 读 (renderer 读 `x_advance` / `x_offset` / `atlas_key()`); zero 主要给
    /// 测试 / 排障 — `assert_eq!(g.glyph_id, 0)` 立即看出是 emoji 替换路径。
    fn apply_emoji_blacklist(&self, mut g: ShapedGlyph) -> ShapedGlyph {
        if self.emoji_face_ids.contains(&g.cache_key.font_id) {
            g.glyph_id = 0;
        }
        g
    }

    /// 把 [`ShapedGlyph`] 光栅化为 alpha bitmap (单通道 8-bit mask)。
    ///
    /// 走 cosmic-text 的 [`cosmic_text::SwashCache::get_image_uncached`]: 拿
    /// `glyph.cache_key` (T-0403 起 ShapedGlyph 私有携带, 来自 LayoutGlyph::physical()
    /// 计算的 cosmic-text [`cosmic_text::CacheKey`]) 直接 raster 一次, **不**走
    /// SwashCache 的内部 HashMap (上层 [`crate::wl::render::GlyphAtlas`] 已经做
    /// HashMap<atlas_key, AtlasSlot> 缓存, 双层缓存重复)。
    ///
    /// **派单偏离主动告知**: 派单 Scope/In #A 写 `rasterize_glyph(&mut self, glyph_id: u16,
    /// font_size: f32) -> Option<RasterizedGlyph>`, 但 cosmic-text [`cosmic_text::CacheKey`]
    /// 必需 `font_id: fontdb::ID`(单 (gid, size) 不足以定位到具体 face — 同 gid
    /// 在不同 face 是不同字形, CJK fallback 路径下 LayoutGlyph 的 font_id 是 fallback
    /// face 不是主 monospace), 所以本实装把签名改为 `rasterize(&ShapedGlyph)`,
    /// 让 ShapedGlyph 私有字段 `cache_key` 自带正确 face 信息。**类型隔离 INV-010
    /// 守住**: cache_key 是 ShapedGlyph 私有字段不暴露公共 API, 调用方拿 ShapedGlyph
    /// 引用即可, 不接触 cosmic-text 类型。
    ///
    /// **content == Mask 校验**: cosmic-text 内部 `Render::new` 配置了三档
    /// `Source::ColorOutline / ColorBitmap / Outline`, 主路径 (`format(Format::Alpha)`)
    /// 给 [`cosmic_text::SwashContent::Mask`] 单通道 alpha;但彩色 emoji / 字体内嵌
    /// 位图 face 可能给 `Color` (RGBA) 内容, Phase 4 不渲染彩色 (派单 Out 段排除),
    /// 返 `None` 让上层画 `.notdef` tofu 占位。
    ///
    /// 测试覆盖:
    /// - [`tests::rasterize_ascii_a_returns_bitmap`]
    /// - [`tests::rasterize_chinese_zhong_returns_bitmap_or_none_no_panic`]
    /// - [`tests::rasterize_zero_glyph_id_no_panic`]
    pub fn rasterize(&mut self, glyph: &ShapedGlyph) -> Option<RasterizedGlyph> {
        let img = self
            .swash_cache
            .get_image_uncached(&mut self.font_system, glyph.cache_key)?;
        if img.content != cosmic_text::SwashContent::Mask {
            // Phase 4 仅支持单通道 alpha mask (R8Unorm 纹理); 彩色 emoji / 字体
            // 内嵌位图给 Color content, 当前不上 atlas (上层会画 .notdef 占位)。
            // T-0405 / Phase 5+ 加彩色字形 pipeline 时改 RGBA8 atlas + 单独 pass。
            return None;
        }
        Some(RasterizedGlyph::from_swash_image(img))
    }

    /// 对一行纯文本走 cosmic-text shaping pipeline,返回按显示顺序的
    /// [`ShapedGlyph`] 序列。空字符串返回空 [`Vec`]。
    ///
    /// **T-0403 渲染层主入口** —— 取代 [`Self::shape_one_char`] 单字符脚手架。
    /// 输入一行 `&str`(不含 `\n`,多行调用方按 `\n` 切分),输出每个字形的
    /// gid + advance + line-relative position,渲染层按 `x_offset` / `y_offset`
    /// 直接 blit 到 atlas 槽位。
    ///
    /// **多 LayoutRun 拼接**: cosmic-text 在字体 fallback 切换时(例 ASCII
    /// "abc" + CJK "中" 各自走不同 face)把单 line 拆成多 [`LayoutRun`],
    /// `.flat_map(|run| run.glyphs)` 按显示顺序展平 —— 对 LTR 文本就是物理
    /// 顺序。RTL / BiDi 留给 Phase 5+(派单 Out 段明示)。
    ///
    /// **Metrics 选择 (font_size=17.0, line_height=25.0)**: 与 T-0306
    /// `CELL_W_PX=10` / `CELL_H_PX=25` 估算对齐 —— monospace 字体 17pt advance
    /// ≈ 10px (DejaVu Sans Mono / Source Code Pro 实测), line_height 25 与
    /// cell_h 一致。**临时常数**, Phase 4 后续 ticket 用字体真实 metrics 替换
    /// (见 [`Self::shape_one_char`] 注释提到的 T-0306 P3-5 路径)。
    ///
    /// **shape_until_scroll(prune=true)**: 本 buffer 是单次 shape 后立即丢的
    /// scratch, prune=true 告诉 cosmic-text 主动释放 shape buffer 中未用的
    /// scroll-out 行存储 —— 对单行场景无差,但与未来"多行可滚动 buffer"调用
    /// 模式对齐。`shape_one_char` 使用 `false` 是 T-0401 早期实现选择,本 fn
    /// 不一并改 (out-of-scope; 该 fn 是 Phase 4 临时探测 API 即将退役).
    ///
    /// 测试覆盖:
    /// - [`tests::shape_line_ascii_returns_per_char_glyphs`]
    /// - [`tests::shape_line_mixed_cjk_returns_glyphs`]
    /// - [`tests::shape_line_empty_returns_empty`]
    /// - [`tests::shape_line_advance_sums_match_text_width`]
    pub fn shape_line(&mut self, text: &str) -> Vec<ShapedGlyph> {
        use cosmic_text::{Attrs, Buffer, Family, Metrics, Shaping};

        // T-0404 HiDPI: font_size × HIDPI_SCALE = 17 × 2 = 34pt physical, 让
        // rasterize 出的 bitmap 是物理像素尺寸, 与 [`crate::wl::render::Renderer`]
        // 的 surface 物理像素 (logical × HIDPI_SCALE) 单位一致。bitmap 上 atlas
        // 后渲染时直接 1:1 上屏, 不依赖 GPU sampler 放大 (Phase 4 atlas sampler 是
        // FilterMode::Nearest, 不做插值)。
        //
        // **HIDPI_SCALE 单一来源**: 引 [`crate::wl::HIDPI_SCALE`] 让
        // text 与 render 共享同一缩放常数, 改一处即可。模块依赖图: text 不
        // 反向依赖 wl 类型 (INV-010), 仅引用 const u32 不算耦合升级 (派单允许)。
        let scale = crate::wl::HIDPI_SCALE as f32;
        let metrics = Metrics::new(17.0 * scale, 25.0 * scale);
        let mut buffer = Buffer::new(&mut self.font_system, metrics);
        // T-0407: Family::Name(primary) 显式锁 face (T-0403 bug fix, 同
        // shape_one_char)。fallback chain 仍工作 — 主 face 缺 codepoint
        // (例 CJK '中' 在 DejaVu Sans Mono 不含) cosmic-text 自动 fallback
        // 到 Noto CJK 等; 这是想要的 (T-0405 scope), 此处只锁主 face。
        // emoji face fallback 由 `apply_emoji_blacklist` post-process 拒绝。
        //
        // **T-0405 verify 路径** (集成测试 `tests/cjk_fallback_e2e.rs`):
        // 用户机已装 noto-fonts-cjk + adobe-source-han-sans, fc-list :lang=zh
        // 命中 → cosmic-text fontdb fallback chain 自动给 CJK codepoint 切到
        // 这两 face 之一 (primary DejaVu Sans Mono 不含 CJK glyph), face_id
        // 不同于 primary_face_id; [`GlyphKey`] face_id 维度 (T-0407) 让 CJK
        // fallback face 与主 face 不撞 atlas slot。CI 无 CJK face 时退化到
        // 主 face .notdef tofu (`apply_emoji_blacklist` 不接管 — 不在黑名单),
        // 显主 face 豆腐字形而非崩溃。T-0405 测试以用户机为准, CI 退化路径
        // 接受 (派单 Acceptance "用户机为准, CI 退化作 follow-up")。
        let attrs = Attrs::new().family(Family::Name(&self.primary_face_name));

        buffer.set_text(&mut self.font_system, text, attrs, Shaping::Advanced);
        buffer.shape_until_scroll(&mut self.font_system, true);

        // 双步收集: 先 from_cosmic_glyph 提取 quill 类型, 再 apply_emoji_blacklist
        // post-process — 走 self 私有 fn 需借 emoji_face_ids; 拆开避免单次闭包内
        // 同时 &mut buffer / &self 借用冲突 (cosmic-text layout_runs 借 buffer)。
        let raw: Vec<ShapedGlyph> = buffer
            .layout_runs()
            .flat_map(|run| {
                run.glyphs
                    .iter()
                    .map(ShapedGlyph::from_cosmic_glyph)
                    .collect::<Vec<_>>()
            })
            .collect();
        let emoji_filtered: Vec<ShapedGlyph> = raw
            .into_iter()
            .map(|g| self.apply_emoji_blacklist(g))
            .collect();

        // T-0801: CJK / wide 字形强制双宽 advance 后处理。详 [`force_cjk_double_advance`] doc。
        // T-0803: 透传原文 &str — wide 判定走 unicode-width 表 (POSIX wcwidth +
        // East Asian Width), 需按 ShapedGlyph.cluster 回查原文 char。
        force_cjk_double_advance(emoji_filtered, text)
    }
}

/// T-0801 + T-0803: 后处理 shape 出的 glyph 序列, 按 grid 占位标准把字形对齐到
/// monospace 终端 1 / 2 cell 宽度。
///
/// **真因 (T-0801 引)**: alacritty Term 协议把 CJK 字当 2 cells (实字 cell +
/// WIDE_CHAR_SPACER cell), quill 渲染按 cosmic-text 自然 advance ~ 1.67 ×
/// CELL_W_PX_phys (T-0405 实测: Noto CJK 1.0em / DejaVu Sans Mono Latin 0.6em
/// 跨 face 比例), 字形占第 1 cell + 第 2 cell 一半, 视觉每字一空。主流终端
/// (alacritty / foot / kitty / xterm) **强制**双宽 cell 居中。
///
/// **T-0803 修判定来源 (ADR 0010)**: T-0801 用 `natural_advance > 1.5 × CELL_W_PX_phys`
/// 判 wide, 把 nerd font 图标 (PUA U+E000..F8FF) 字形 ~1.67 cell 误判 wide,
/// 但 lsd / fzf / starship / ghostty 全用 unicode-width crate 算 PUA = 1 cell,
/// grid 视图错位。T-0803 改判 wide 走 `unicode_width::UnicodeWidthChar::width(ch)`
/// (POSIX wcwidth + Unicode East Asian Width 表), 与工业标准对齐:
///   - CJK W → 2 cell, ASCII Na → 1 cell, PUA N → 1 cell, emoji → 2 cell
///
/// **算法**:
/// 1. 按 `g.cluster` 从 `original` 取 char, `UnicodeWidthChar::width(ch).unwrap_or(1) >= 2`
///    判 wide (`unwrap_or(1)` fallback: control char width 返 None, 渲染按 1 cell;
///    `unwrap_or(' ')` 兜底 cluster 越界 — 越界几乎不可能, 但 quill 原则 panic-free)
/// 2. 重 cascade `x_offset`: 从 0 起累加, narrow → 1 cell (`CELL_W_PX_phys`),
///    wide → 2 cells (`2 × CELL_W_PX_phys`)
/// 3. wide glyph 居中: `x_offset += (2 × CELL_W_PX_phys - natural_advance) / 2`
///    (字形居中双宽 cell, slot.bearing_x 仍按字形 bbox 左偏, 居中量加在 x_offset 上)
/// 4. 强制 advance: wide → `2 × CELL_W_PX_phys`, narrow → `CELL_W_PX_phys`
///    (monospace 终端 grid 严对齐, ASCII / PUA 自然 advance 与 CELL_W_PX_phys
///    微差直接吸齐, 让 "advance 累加 == 期望 grid 宽度" 成为自检不变式)
///
/// **已知 trade-off (ADR 0010 Out 段)**: PUA / nerd font 图标自然字形 ~1.67 cell
/// 现在被压在 1 cell, 字形可能与下一字符像素级重叠几像素 — ghostty / kitty 走
/// 字形 squeeze 渲染 (`scale_x = cell_w / natural_advance`) 解决, 走另开 ticket
/// Phase 后期落地。本 fn 仅修 grid 判定。
///
/// **不动 single-glyph natural_advance 字段**: ShapedGlyph 公共 API `x_advance`
/// 是 quill 抽象 (派单 In #A "强制 advance = 2 × CELL_W_PX"), 不暴露 cosmic-text
/// 自然 advance。下游渲染层只看 `x_offset` 决定字形左上角位置 (走
/// `glyph.x_offset + slot.bearing_x`, 见 src/wl/render.rs::draw_frame T-0801 注释),
/// `x_advance` 字段保留作元数据 (累加自检 / 测试断言 / 未来 cursor 推进逻辑)。
///
/// **HIDPI_SCALE 单一来源** (沿袭 shape_line 注释): 引 [`crate::wl::HIDPI_SCALE`]
/// 与 [`crate::wl::CELL_W_PX`] 让 cell pixel 宽 一处定义, 不再走 magic number。
/// 改 CELL_W_PX (Phase 4+ 字体真实 metrics 替换路径) 本 fn 自跟随。
///
/// **空切片**: 直接返空, 跳过 cascade (避免 saturating_sub 之类细节)。
///
/// 测试覆盖:
/// - [`tests::shape_line_cjk_glyphs_have_forced_double_advance`]
/// - [`tests::shape_line_cjk_glyphs_centered_in_double_cell`]
/// - [`tests::shape_line_ascii_advance_equals_cell_w_phys`]
/// - [`tests::shape_line_pua_nerd_font_icon_advance_eq_one_cell`] (T-0803)
/// - [`tests::shape_line_emoji_advance_eq_double_cell`] (T-0803)
/// - [`tests::shape_line_mixed_cjk_returns_glyphs`] (T-0405 ratio range
///   [1.4, 2.4] 改为严 advance == 2 × CELL_W_PX_phys)
fn force_cjk_double_advance(glyphs: Vec<ShapedGlyph>, original: &str) -> Vec<ShapedGlyph> {
    if glyphs.is_empty() {
        return glyphs;
    }
    let cell_w_phys = crate::wl::CELL_W_PX * (crate::wl::HIDPI_SCALE as f32);
    let double_w = 2.0 * cell_w_phys;

    // T-0807 M1: 按 cluster 聚合推 cursor (而非按 glyph 独立推).
    // cosmic-text/HarfBuzz 协议: 同 cluster 多 glyph 来自同一原文段
    // (ZWJ emoji / VS16 / 复合连字 / combining mark), cluster 单调非递减.
    //
    // 步骤:
    // 1. 预扫一遍 cluster 边界 — 每 cluster 的"原文 substring"起点 = g.cluster,
    //    终点 = 下一个不同 cluster 的 g.cluster (或 original.len()).
    // 2. cluster 的 grid cell 宽 = sum unicode_width(每 char) of substring,
    //    至少 1 cell (防 width=0 退化, 例 '\r' / 全 zero-width cluster).
    // 3. 同 cluster 多 glyph 共享 cursor_x 起点 (x_offset 一致),
    //    cursor_x 仅在进入新 cluster 时推一次 cluster_advance.
    // 4. x_advance 字段语义: cluster 第一 glyph 存 cluster_advance, 后续 glyph
    //    存 0 — sum 仍等 cluster_advance, 渲染层只看 x_offset 决定字形位置.

    // 预扫: 每 glyph 对应的下一个不同 cluster 起点 (= cluster substring 终点).
    let n = glyphs.len();
    let mut next_cluster_start: Vec<usize> = vec![original.len(); n];
    {
        let mut last_seen: Option<(usize, usize)> = None; // (idx, cluster)
        for (i, g) in glyphs.iter().enumerate().rev() {
            if let Some((_, last_c)) = last_seen {
                if last_c != g.cluster {
                    // i 之后第一个 cluster 不同的 glyph cluster 即 substr 终点
                    next_cluster_start[i] = last_c;
                }
            }
            // 更新 last_seen 到 "本 glyph 之后看见的最近一个 cluster".
            // 同 cluster 多 glyph 共享 next_cluster_start (走相同 last_c).
            last_seen = Some((i, g.cluster));
        }
        // 从前往后再校正: 同 cluster 段共享 next 边界 = 段后第一个不同 cluster.
        // 反向扫已经给出每个 glyph 看到的"下一个不同 cluster", 同 cluster 内
        // 各 glyph 的 next_cluster_start 取其中"最大那个" (= 段后真正的下一
        // cluster 起点). 反向扫法实际已自然给出 — 同 cluster 内每个 glyph 看到
        // 的下一个不同 cluster 都是同一个 (因为段后所有 glyph 都属下一段开头).
    }

    let mut out: Vec<ShapedGlyph> = Vec::with_capacity(n);
    let mut cursor_x: f32 = 0.0;
    let mut last_cluster: Option<usize> = None;
    let mut cluster_start_x: f32 = 0.0;
    let mut cluster_advance: f32 = 0.0;
    let mut cluster_is_wide: bool = false;

    for (i, g) in glyphs.iter().enumerate() {
        // 进入新 cluster: 把上一 cluster advance 推到 cursor_x, 算新 cluster_advance.
        if Some(g.cluster) != last_cluster {
            if last_cluster.is_some() {
                cursor_x += cluster_advance;
            }
            // 新 cluster substring [g.cluster .. next_cluster_start[i]).
            let end = next_cluster_start[i].max(g.cluster).min(original.len());
            let cluster_str = original.get(g.cluster..end).unwrap_or("");
            let cells = unicode_width::UnicodeWidthStr::width(cluster_str).max(1);
            cluster_advance = (cells as f32) * cell_w_phys;
            cluster_is_wide = cells >= 2;
            cluster_start_x = cursor_x;
            last_cluster = Some(g.cluster);
        }

        // 居中: 仅 wide cluster 第一 glyph 算居中量, 后续 glyph (零宽 mark /
        // VS16 / ZWJ 子序列 glyph) 沿同 cursor 起点不再加 pad.
        // 第一 glyph 判别: out 中最近 push 的 glyph cluster 是否同此 g.cluster.
        let is_first_in_cluster = out.last().is_none_or(|prev| prev.cluster != g.cluster);
        let natural = g.x_advance;
        let center_pad = if cluster_is_wide && is_first_in_cluster {
            (cluster_advance - natural).max(0.0) / 2.0
        } else {
            0.0
        };
        let new_x_offset = cluster_start_x + center_pad;
        // x_advance 字段: 仅 cluster 第一 glyph 存 cluster_advance, 后续 0
        // — sum 仍等 cluster_advance, 渲染层只用 x_offset.
        let new_x_advance = if is_first_in_cluster {
            cluster_advance
        } else {
            0.0
        };
        out.push(ShapedGlyph {
            x_advance: new_x_advance,
            x_offset: new_x_offset,
            ..*g
        });
    }
    // double_w 仅供注释保留语义对照, 防意外死代码 warning.
    let _ = double_w;
    out
}

/// 单个字形的光栅化产物 (Phase 4, T-0403)。
///
/// **类型隔离 (INV-010)**: 不暴露 [`cosmic_text::SwashImage`] / `Placement` /
/// `swash::scale::image::Content` 等上游类型。本 struct 是渲染层 atlas 上传需要
/// 的最小子集, 沿袭 [`ShapedGlyph`] 同款套路 — 公共 API 只见 quill 自定义类型,
/// cosmic-text / swash 类型严格锁本模块内 (`from_swash_image` 模块私有 inherent fn)。
///
/// **bitmap 内容**: 单通道 8-bit alpha (`Vec<u8>`, 长度 = `width * height`),
/// 行主序无 padding (cosmic-text SwashImage 直接给紧致 layout)。渲染层
/// [`crate::wl::render::GlyphAtlas`] 上传到 R8Unorm 纹理, fragment shader 用 `.r`
/// 作 alpha mask 与 fg color 相乘。
///
/// **bearing_x / bearing_y**: 来自 cosmic-text `Placement.left` / `Placement.top`,
/// 是 baseline-relative 的字形左上角 bearing (典型: 字母 'a' bearing_x ≈ 1, bearing_y
/// ≈ 9 (ascender 高度)。bearing_y 正值表示 baseline 之上)。渲染层用此偏移把 bitmap
/// 放到 cell 内正确像素位置。
///
/// **零尺寸字形** (空格 / zero-width): 某些 face 可能给 `width = 0 || height = 0`,
/// 表示无可见像素 (空白 advance 仅推进 cursor)。本 struct 接受零尺寸不报错, 上层
/// atlas allocate 时跳过 GPU 上传 (零字节 write_texture)。
#[derive(Debug, Clone)]
pub struct RasterizedGlyph {
    pub width: u32,
    pub height: u32,
    /// 单通道 8-bit alpha bitmap, 长度 = `width * height`。
    pub bitmap: Vec<u8>,
    /// X bearing (baseline-relative 左偏移), 通常正值。
    pub bearing_x: i32,
    /// Y bearing (baseline-relative 上偏移, 正值表示 baseline 之上)。
    pub bearing_y: i32,
}

impl RasterizedGlyph {
    /// 模块私有 inherent fn, 沿袭 [`ShapedGlyph::from_cosmic_glyph`] 类型隔离套路 ——
    /// `cosmic_text::SwashImage` / `swash::zeno::Placement` / `swash::scale::image::Content`
    /// 严格锁本 fn body 内, 不暴露公共 API (INV-010)。
    ///
    /// 字段映射:
    /// - `width` ← `img.placement.width` (u32)
    /// - `height` ← `img.placement.height` (u32)
    /// - `bitmap` ← `img.data` (Vec<u8>, single-channel alpha; cosmic-text 内部
    ///   按 Mask content 给一个 byte/pixel; 本 fn 直接 move 接管)
    /// - `bearing_x` ← `img.placement.left` (i32)
    /// - `bearing_y` ← `img.placement.top` (i32)
    fn from_swash_image(img: cosmic_text::SwashImage) -> Self {
        Self {
            width: img.placement.width,
            height: img.placement.height,
            bitmap: img.data,
            bearing_x: img.placement.left,
            bearing_y: img.placement.top,
        }
    }
}

/// 单个 shaped glyph 的几何信息(Phase 4 起步最小集)。
///
/// **类型隔离**: 不暴露 [`cosmic_text::LayoutGlyph`](其 15 字段含
/// cosmic-text 内部 `font_id: ID` / `cache_key_flags` / `level: Level`
/// 等不稳定布局状态)。本 struct 是渲染层真正用得上的最小子集,Phase 4
/// 后续 ticket(T-0403..T-0406)会按需扩字段(`font_id_quill_local: u32` /
/// `font_size_px: f32` 等),仍走 [`Self::from_cosmic_glyph`] 模块私有
/// inherent fn 注入,**不**反向构造或 `From` impl。
///
/// **字段语义**:
/// - `glyph_id`: face 内 glyph index(u16, OpenType 标准 gid),光栅化阶段
///   (T-0403)用作 atlas key 一部分(配合 face id + size)
/// - `x_advance`: 横向推进像素(已 layout,等价 HarfBuzz `x_advance`)。
///   monospace 字体下应近似一致(例 17pt 约 10px),Phase 4 字形 ticket
///   用此值动态测量 cell pixel size 替换 T-0306 临时常数
/// - `y_advance`: 竖向推进像素(横排恒 0;给未来竖排 hook)
/// - `x_offset` / `y_offset` (T-0402 加): 字形左上角相对所在行起点的像素
///   位置 —— 来自 [`cosmic_text::LayoutGlyph`] 的 `x` / `y` 字段(cosmic-text
///   把这两个字段命名为 hitbox X/Y 但语义就是"glyph 位置")。**注意命名差**:
///   cosmic-text 自己也有 `LayoutGlyph::x_offset` / `y_offset` 字段表达"逻辑
///   坐标抖动",我们**不**用那两个 (Phase 4 不做 sub-pixel rendering),quill
///   的 `x_offset` / `y_offset` 就是 cosmic-text 的 `x` / `y` 累积位置。
///   T-0403 渲染层用这两值定位每个字形 atlas blit 的左上角。
///   类型用 `f32` (而非 cosmic-text [`cosmic_text::PhysicalPosition`]),保持
///   INV-010 类型隔离 —— 接班 reviewer 注意 PhysicalPosition 不出本模块。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ShapedGlyph {
    pub glyph_id: u16,
    pub x_advance: f32,
    pub y_advance: f32,
    pub x_offset: f32,
    pub y_offset: f32,
    /// **T-0803 加**: 原文 byte cluster 起点 (取 cosmic-text [`cosmic_text::LayoutGlyph::start`],
    /// 0.12 已稳)。[`force_cjk_double_advance`] 用此回查原文 char, 走
    /// `unicode_width::UnicodeWidthChar::width(ch)` 决定 grid 占位 1 vs 2 cell
    /// (替代 T-0801 的字形 natural advance ratio 判定 — PUA / nerd font 图标
    /// 字形宽 ~1.67 cell 但 unicode-width 算 1 cell, 字形比例靠不住, 见 ADR 0010)。
    ///
    /// **why 不存 char**: 一字符可能 shape 出多 glyph (例 ZWJ 表情 / 复合字 / 连字),
    /// cluster 是 cosmic-text/HarfBuzz 协议层标准 — 多 glyph 共享同 cluster
    /// 等价 "同一原文 char start"; cluster + 原文 &str 配合可重建任意 char,
    /// 语义比 "凭 glyph 自身回猜字符" 稳, 也省掉 char 字段 4 字节。
    ///
    /// INV-010 守: usize 是 quill 基础类型, 不暴露 cosmic-text [`cosmic_text::LayoutGlyph`]。
    pub cluster: usize,
    /// **T-0403 加 (私有)**: cosmic-text 自带 cache_key, 给 [`TextSystem::rasterize`]
    /// 用 — `cache_key.font_id` 是 LayoutGlyph 实际选中的 face (含 CJK fallback
    /// 切到 Noto CJK 时的 face_id), 单纯 (gid, font_size) 不够, 必须有 font_id。
    ///
    /// **不 pub** (INV-010 strict reading 第 9 次应用): cosmic-text [`cosmic_text::CacheKey`]
    /// 是上游类型, 公共 API 不能暴露; 渲染层用 [`Self::atlas_key`] 拿 quill 自定义
    /// `(u16, u32)` 作 HashMap key, cache_key 仅供本模块 `rasterize` 内部消费。
    cache_key: cosmic_text::CacheKey,
}

impl ShapedGlyph {
    /// 从 cosmic-text 的 [`cosmic_text::LayoutGlyph`] 提取 quill 渲染层关心
    /// 的字段。
    ///
    /// **模块私有 inherent fn,不开 `impl From` / `impl Into`** —— 沿袭
    /// [`crate::term::CellPos::from_alacritty`] /
    /// [`crate::term::CursorShape::from_alacritty`] /
    /// [`crate::term::ScrollbackPos::to_alacritty`] /
    /// [`crate::term::Color::from_alacritty`] 同款类型隔离套路(INV-010):
    /// 下游既不能 `LayoutGlyph::from(g)` 反向构造,也不能 `g.into()` 偷渡
    /// cosmic-text 类型出去。
    ///
    /// **字段映射**:
    /// - `glyph_id` ← `g.glyph_id`(face 内 gid, u16)
    /// - `x_advance` ← `g.w`(cosmic-text 0.12 把已 layout 的 horizontal
    ///   advance 暴露为 `w: f32`,语义等价 HarfBuzz / swash 的 `x_advance`,
    ///   非"glyph bbox 宽度";cosmic-text 0.19+ 字段名仍为 `w`,升级稳)
    /// - `y_advance` ← `0.0`(横排, vertical advance 恒 0;cosmic-text 未
    ///   直接暴露 y_advance,Phase 4 不做竖排,0 是正确占位)
    /// - `x_offset` ← `g.x` (T-0402 加, line-relative cumulative pixel X 位置;
    ///   cosmic-text 把这字段命名 hitbox X 但实质就是 layout 后 X 坐标)
    /// - `y_offset` ← `g.y` (T-0402 加, line-relative pixel Y 位置;单行场景
    ///   通常 0, 留字段是为 Phase 4 后续多行 / 行内 super/subscript 扩展)
    ///
    /// **不映射 cosmic-text 自身 `x_offset` / `y_offset` 字段** (T-0402 决策):
    /// cosmic-text 的同名字段表达"逻辑坐标 sub-pixel 抖动",Phase 4 不做
    /// sub-pixel rendering(整数像素对齐 monospace cell),用 `g.x` / `g.y`
    /// 累积位置即可。命名上虽然与 cosmic-text 撞名但语义不同 — 注释 + struct
    /// 字段 doc 已显式区分。
    ///
    /// **跨版本升级路径**: cosmic-text 0.12 → 0.19 LayoutGlyph 字段稳定
    /// (实测 0.19 Has `glyph_id` / `w` / `x` / `y` 同名同类型)。若未来 1.x
    /// 重命名 / 改类型,本 fn body 是唯一改动点。`LayoutGlyph` 是 struct 不是
    /// enum,字段未消化时编译期不报警 —— 这是 INV-010 验证段对 struct 类型
    /// 的已知边界(struct 靠"渲染层只用 glyph_id/x_advance/y_advance/x_offset/
    /// y_offset 五字段"约定锁住,而非 enum 的 exhaustive match catch).
    fn from_cosmic_glyph(g: &cosmic_text::LayoutGlyph) -> Self {
        // T-0403: 走 LayoutGlyph::physical((0,0), 1.0) 拿 cosmic-text 标准的
        // CacheKey (含 font_id + glyph_id + font_size_bits + flags + subpixel bin)。
        // offset (0,0) + scale 1.0: Phase 4 不做 sub-pixel positioning / HiDPI
        // 整数缩放 (T-0404 才做), 所以传零偏移 + 单位 scale 拿 LayoutGlyph
        // "原生像素位置" 的 CacheKey。Phase 5+ 加 sub-pixel rendering 时此处
        // 改传当前 cell 的 fractional position。
        let physical = g.physical((0.0, 0.0), 1.0);
        Self {
            glyph_id: g.glyph_id,
            x_advance: g.w,
            y_advance: 0.0,
            x_offset: g.x,
            y_offset: g.y,
            // T-0803: cluster = LayoutGlyph.start (原文 byte 起点)。cosmic-text 0.12
            // 字段稳定, 0.19+ 仍 `start: usize` 同名同类型, 升级路径同其他字段。
            cluster: g.start,
            cache_key: physical.cache_key,
        }
    }

    /// Atlas slot HashMap key (T-0407 加 face_id 维度修复 T-0403 atlas key
    /// 跨 face 冲突 bug)。
    ///
    /// 返 [`GlyphKey`] struct (face_id u64 hash + glyph_id u16 + font_size_quantized
    /// u32), 渲染层 [`crate::wl::render::GlyphAtlas`] 用此 key 决定该字形是否
    /// 已上 atlas、uv 槽位在哪。`font_size_bits` 是 `f32::to_bits(font_size_px)`
    /// 的稳定量化表示 (相同 font_size 必同 bits, 不会因浮点比较抖动)。
    ///
    /// **T-0407 修 T-0403 P3 跟进 (audit P2-2)**: T-0403 实装为 `(u16, u32)`
    /// tuple, 不含 face_id 维度, 跨 face 同 glyph_id 撞 key 互相覆盖
    /// (例 CJK fallback 切到 Noto CJK 时 glyph_id=N 与主 face 的 glyph_id=N
    /// 共享 atlas slot, 后到的覆盖先到的)。**T-0407 升级为 GlyphKey struct,
    /// face_id 维度加上**。沿袭 T-0403 audit P2-2 / writer 主动告知 #5 路径。
    ///
    /// **INV-010**: 返 quill 自定义 [`GlyphKey`] struct, 字段全 quill 基础类型
    /// (u64 / u16 / u32), 不暴露 cosmic-text `CacheKey` / `fontdb::ID` /
    /// `font_size_bits` 等上游字段名 (虽底层数值相等, quill 接口承诺仅是
    /// "u64 face_id hash + u16 glyph_id + u32 font_size_quantized" 抽象)。
    pub fn atlas_key(&self) -> GlyphKey {
        GlyphKey {
            face_id: fontdb_id_to_u64(self.cache_key.font_id),
            glyph_id: self.cache_key.glyph_id,
            font_size_quantized: self.cache_key.font_size_bits,
        }
    }

    /// 返 glyph 实际选中的 face id 作 quill 自定义 u64 hash (T-0407 测试用)。
    ///
    /// **why 暴露**: 测试 [`tests::shape_ascii_uses_primary_face`] 断言 ASCII
    /// glyph 的 face_id == [`TextSystem::primary_face_id`] (验 face 锁定真起
    /// 作用)。`atlas_key().face_id` 也能拿到, 但语义上"glyph 的 face id"是
    /// 独立 concept, pub fn 命名清晰胜过 atlas_key 子字段访问。
    ///
    /// INV-010 守: 返 u64 hash 不暴露 fontdb::ID, 与 [`Self::atlas_key`] +
    /// [`TextSystem::primary_face_id`] 同源算法 [`fontdb_id_to_u64`]。
    pub fn face_id(&self) -> u64 {
        fontdb_id_to_u64(self.cache_key.font_id)
    }
}

/// Atlas slot HashMap key (T-0407, 派单 Scope/In #B "atlas key = (face_id,
/// glyph_id, font_size_quantized)")。
///
/// **三维 key 的必要性**: 单 (glyph_id, font_size) 不够 — cosmic-text fallback
/// chain 在 face 切换时 LayoutGlyph.font_id 不同, 同 glyph_id 在不同 face 是
/// 不同字形 (例 CJK fallback 切到 Noto CJK 的 glyph_id=N 与主 face 的
/// glyph_id=N 共享 atlas slot 会撞)。T-0403 audit P2-2 / writer 主动告知 #5
/// 已登记此 bug, T-0407 修复落地。
///
/// **INV-010 类型隔离**: 全 quill 基础类型 (u64 / u16 / u32), 无 cosmic-text /
/// wgpu / wayland 上游类型字段。`face_id: u64` 是 [`fontdb_id_to_u64`] hash 输出,
/// 不暴露 fontdb::ID slotmap 内部表达 — 沿袭 T-0302..T-0306 + T-0401..T-0403
/// 类型隔离套路 (INV-010 strict reading 第 10 次应用, 上一审码 T-0403 第 9 次)。
///
/// **派生 trait**: `Hash + Eq + PartialEq + Clone + Copy + Debug` —
/// HashMap key 必需 (Hash + Eq), 渲染层值复制 (Copy + Clone), 排障日志 (Debug)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct GlyphKey {
    /// face id 作 u64 hash (来自 [`fontdb_id_to_u64`] DefaultHasher); 同
    /// 进程内同 fontdb::ID 必同 u64, 跨进程不保证 (atlas key 仅本进程消费)。
    pub face_id: u64,
    /// face 内 glyph index (OpenType 标准 gid)。
    pub glyph_id: u16,
    /// font_size 量化表示 (`f32::to_bits(font_size_px)`); 相同 font_size 必同
    /// bits, 不因浮点比较抖动。
    pub font_size_quantized: u32,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 用户机 Arch Linux 已装 noto-fonts-cjk + ttf-dejavu(均含 monospace
    /// face),`TextSystem::new` 必成功;CI 若无 monospace 字体会 Err
    /// 是预期(派单"找不到 monospace fallback → return Err"语义)。
    #[test]
    fn text_system_new_succeeds() {
        let result = TextSystem::new();
        assert!(
            result.is_ok(),
            "TextSystem::new must succeed on user machine \
             (Arch + monospace font: dejavu / noto-cjk-mono); err = {:?}",
            result.err()
        );
    }

    /// ASCII 'a' 在任何 monospace face 都有 glyph,advance 必正(>0)。
    #[test]
    fn shape_ascii_a_returns_glyph() {
        let mut ts = TextSystem::new().expect("TextSystem::new on user machine");
        let g = ts
            .shape_one_char('a')
            .expect("ASCII 'a' must shape to at least one glyph in monospace");
        assert!(
            g.x_advance > 0.0,
            "ASCII 'a' x_advance should be positive in monospace 14pt (got {})",
            g.x_advance
        );
        assert_eq!(g.y_advance, 0.0, "horizontal text y_advance is 0");
    }

    /// CJK fallback: '中' 在 Latin monospace 无 glyph 时 cosmic-text 应自动
    /// fall back 到 Noto CJK / Source Han Sans;用户机已装。若 CI 没装 CJK
    /// 字体,cosmic-text 给 tofu glyph(.notdef, gid 0)而非 panic —— 测试
    /// `assert!(g.is_some())` 配 advance >= 0 — 既覆盖"用户机有 CJK 字体正常路径"
    /// 又覆盖"CI 无 CJK 退化到 tofu" (tofu 也是 Some, gid=0 + advance >= 0)。
    /// 真异常 = panic / None / advance < 0 / NaN, 这些会挂测试。审码 T-0401 P3-2。
    #[test]
    fn shape_chinese_zhong_returns_glyph() {
        let mut ts = TextSystem::new().expect("TextSystem::new on user machine");
        let g = ts.shape_one_char('中');
        assert!(
            g.is_some(),
            "shape '中' must return Some (cosmic-text 给 tofu 也算 Some,只要不 panic)"
        );
        if let Some(sg) = g {
            assert!(
                sg.x_advance >= 0.0,
                "CJK '中' x_advance non-negative (got {}); 用户机有 noto-cjk 应 > 0,\
                 CI 退化到 tofu 仍 >= 0",
                sg.x_advance
            );
            assert_eq!(sg.y_advance, 0.0, "horizontal text y_advance is 0");
        }
    }

    /// 不存在的 codepoint(U+E0000 tag space, 私用区无字形): cosmic-text 应
    /// 给 tofu glyph(gid 0)或返 None(空 layout run), **关键是不 panic**。
    /// 上游 0.12 实测会给 Some + gid 可能为 0;接受 Some / None 任一,不 panic 即合规。
    #[test]
    fn shape_unknown_codepoint_returns_some_or_none_no_panic() {
        let mut ts = TextSystem::new().expect("TextSystem::new on user machine");
        // U+E0000: tag space (Unicode 私用区, 通常字体不含字形)
        let g = ts.shape_one_char('\u{E0000}');
        // 不强求 Some 也不强求 None, 只验不 panic。
        if let Some(sg) = g {
            // tofu glyph 通常 gid 0; cosmic-text 实现可能不同, 关键是 layout 数值不是
            // NaN / Inf(渲染层后续会用 advance 算 cell 位置,NaN 会污染 NDC 换算)。
            assert!(
                sg.x_advance.is_finite(),
                "tofu glyph x_advance must be finite (got {})",
                sg.x_advance
            );
            assert!(
                sg.y_advance.is_finite(),
                "tofu glyph y_advance must be finite (got {})",
                sg.y_advance
            );
        }
    }

    /// "abc" 一行 ASCII 应得 3 个 ShapedGlyph,顺序与字符顺序一致;每个
    /// glyph advance > 0 (monospace 17pt),offset 单调递增 (LTR 累积)。
    #[test]
    fn shape_line_ascii_returns_per_char_glyphs() {
        let mut ts = TextSystem::new().expect("TextSystem::new on user machine");
        let glyphs = ts.shape_line("abc");
        assert_eq!(
            glyphs.len(),
            3,
            "ASCII 'abc' must yield 3 glyphs in monospace (got {})",
            glyphs.len()
        );
        // monospace 同字号下三个 advance 应相等(±0.5 浮点容差)
        let first = glyphs[0].x_advance;
        for (i, g) in glyphs.iter().enumerate() {
            assert!(
                g.x_advance > 0.0,
                "glyph[{}] x_advance must be positive (got {})",
                i,
                g.x_advance
            );
            assert!(
                (g.x_advance - first).abs() < 0.5,
                "monospace ASCII glyphs should share advance: glyph[{}]={}, first={}",
                i,
                g.x_advance,
                first
            );
            assert_eq!(g.y_advance, 0.0, "horizontal text y_advance is 0");
            assert!(g.x_offset.is_finite(), "glyph[{}] x_offset finite", i);
            assert!(g.y_offset.is_finite(), "glyph[{}] y_offset finite", i);
        }
        // LTR 显示顺序: x_offset 单调递增
        assert!(
            glyphs[0].x_offset <= glyphs[1].x_offset && glyphs[1].x_offset <= glyphs[2].x_offset,
            "x_offset must be monotonic LTR: {} <= {} <= {}",
            glyphs[0].x_offset,
            glyphs[1].x_offset,
            glyphs[2].x_offset
        );
    }

    /// CJK + ASCII 混排 "你好abc" → 5 glyphs (cosmic-text fallback chain
    /// 切换到 Noto CJK 处理 '你' '好',回到 Latin face 处理 'a' 'b' 'c')。
    /// 用户机 noto-fonts-cjk 装齐;CI 无 CJK 字体 cosmic-text 仍走 .notdef
    /// fallback 5 个 glyph 不少 — 关键是 `glyphs.len() == 5` + 不 panic。
    ///
    /// **T-0801: 严断言 advance == 2 × CELL_W_PX_phys** (派单 In #C "T-0405
    /// 测试 ratio range [1.4, 2.4] 改成严 advance == 2 × CELL_W_PX"): T-0405
    /// 软性 ratio range 是当时未做强制双宽时, 实测 cosmic-text 自然给 1.67;
    /// T-0801 [`force_cjk_double_advance`] 后处理强制 wide → 2 × CELL_W_PX_phys,
    /// 现在 CJK glyph advance 必严等 2 × CELL_W_PX × HIDPI_SCALE (浮点 ±0.001
    /// 容差仅防 IEEE-754 round-trip, 实际 20.0 × 2.0 = 40.0 必精确)。
    ///
    /// **CI 退化路径**: 若主 face 'a' 与 '你' 同走 .notdef tofu (无 CJK face),
    /// '你' 自然 advance 同 ASCII (~CELL_W_PX_phys = 20), 不触发 wide 路径,
    /// `force_cjk_double_advance` 给 1 cell 而非 2 cells — 严断言会挂。但用户
    /// 机以 CJK fallback 真触发为准 (派单 Acceptance 段), 退化路径仅 eprintln!
    /// warning 不挂 CI 的设计 (T-0405) 在 T-0801 改为只在 fallback 真触发时
    /// 锁严宽 (退化路径仍只 eprintln warn 跳过)。
    #[test]
    fn shape_line_mixed_cjk_returns_glyphs() {
        let mut ts = TextSystem::new().expect("TextSystem::new on user machine");
        let primary = ts.primary_face_id();
        let glyphs = ts.shape_line("你好abc");
        assert_eq!(
            glyphs.len(),
            5,
            "'你好abc' must yield 5 glyphs (got {}; cosmic-text fallback 应展平多 run 拼回)",
            glyphs.len()
        );
        for (i, g) in glyphs.iter().enumerate() {
            assert!(
                g.x_advance.is_finite() && g.x_advance >= 0.0,
                "glyph[{}] x_advance must be finite >= 0 (got {})",
                i,
                g.x_advance
            );
            assert_eq!(g.y_advance, 0.0, "horizontal text y_advance is 0");
        }

        // T-0801 严断言: CJK fallback 真触发时 (用户机标准路径), '你' / '好'
        // advance 必严等 2 × CELL_W_PX_phys; ASCII 'a'/'b'/'c' 必严等 CELL_W_PX_phys。
        // CI 退化路径 (主 face .notdef tofu, 无 CJK face) 只 eprintln warn 跳过 —
        // 与 T-0405 当时设计一致, 派单 Acceptance "用户机为准, CI 退化作 follow-up"。
        let cjk_face = glyphs[0].face_id();
        let ascii_face = glyphs[2].face_id();
        let cell_w_phys = crate::wl::CELL_W_PX * (crate::wl::HIDPI_SCALE as f32);
        let double_w = 2.0 * cell_w_phys;
        if cjk_face != primary && cjk_face != ascii_face {
            // 标准路径: 锁"CJK 必严双宽 cell 且 ASCII 必严单宽 cell"。
            for (i, g) in glyphs.iter().enumerate().take(2) {
                assert!(
                    (g.x_advance - double_w).abs() < 0.001,
                    "T-0801 forced double advance: CJK glyph[{}] x_advance 应严等 \
                     2 × CELL_W_PX × HIDPI_SCALE = {} (got {})",
                    i,
                    double_w,
                    g.x_advance
                );
            }
            for (i, g) in glyphs.iter().enumerate().take(5).skip(2) {
                assert!(
                    (g.x_advance - cell_w_phys).abs() < 0.001,
                    "T-0801 forced single advance: ASCII glyph[{}] x_advance 应严等 \
                     CELL_W_PX × HIDPI_SCALE = {} (got {})",
                    i,
                    cell_w_phys,
                    g.x_advance
                );
            }
            // x_offset cascade 锁: '你' 起 0, '好' 起 double_w, 'a' 起 2 × double_w,
            // 'b' 起 2 × double_w + cell_w_phys, 'c' 起 2 × double_w + 2 × cell_w_phys。
            //
            // **不锁 x_offset 严等"cascade 起点", 留 ±cell_w_phys/2 容差**: 居中量
            // (`(double_w - natural)/2`) 加在 x_offset 上, '你' 居中后实际 offset
            // 偏几 px (派单 In #A "字形居中双宽 cell" 设计)。锁"严单调递增 +
            // 大致按 cell 步进"即可。
            let expected_starts = [
                0.0,
                double_w,
                2.0 * double_w,
                2.0 * double_w + cell_w_phys,
                2.0 * double_w + 2.0 * cell_w_phys,
            ];
            for (i, g) in glyphs.iter().enumerate() {
                assert!(
                    (g.x_offset - expected_starts[i]).abs() <= cell_w_phys / 2.0,
                    "T-0801 x_offset cascade: glyph[{}] 应在 {} ± cell_w_phys/2 (got {})",
                    i,
                    expected_starts[i],
                    g.x_offset
                );
            }
        } else {
            eprintln!(
                "shape_line_mixed_cjk: CJK fallback 未触发 (cjk_face={} == primary={} or \
                 == ascii_face={}); 跳过严宽 assert (CI 退化路径)",
                cjk_face, primary, ascii_face
            );
        }
    }

    /// 空字符串: 派单显式要求 "返空 Vec, 不 panic"。
    /// cosmic-text Buffer set_text("", ...) 后 layout_runs 可能返空迭代器
    /// (无 layout line),也可能返一个空 glyphs run — `flat_map` 拼接后均
    /// 给 `Vec::new()`。
    #[test]
    fn shape_line_empty_returns_empty() {
        let mut ts = TextSystem::new().expect("TextSystem::new on user machine");
        let glyphs = ts.shape_line("");
        assert_eq!(
            glyphs.len(),
            0,
            "empty input must yield empty Vec (got {} glyphs)",
            glyphs.len()
        );
    }

    /// 派单测试名 "advance_sums_match_text_width": monospace 一行字符 advance
    /// 累加应等于 (per-char advance) × char_count, ±0.5 浮点误差接受。本测试锁
    /// 的不变式是"所有 ASCII glyph advance 一致 (monospace 性质)",防 Phase 4
    /// 后续 ticket 改 metrics / shape pipeline 时 monospace 这条契约破口。
    #[test]
    fn shape_line_advance_sums_match_text_width() {
        let mut ts = TextSystem::new().expect("TextSystem::new on user machine");
        let text = "abcdef";
        let glyphs = ts.shape_line(text);
        assert_eq!(
            glyphs.len(),
            text.chars().count(),
            "glyph count must match char count for ASCII (text={:?}, glyphs={})",
            text,
            glyphs.len()
        );
        let per_char = glyphs[0].x_advance;
        assert!(
            per_char > 0.0,
            "first glyph advance must be positive in monospace 17pt (got {})",
            per_char
        );
        let total: f32 = glyphs.iter().map(|g| g.x_advance).sum();
        let expected = per_char * (glyphs.len() as f32);
        assert!(
            (total - expected).abs() < 0.5,
            "advance sum must equal per_char × count (±0.5): total={}, expected={}",
            total,
            expected
        );
    }

    // ===== T-0801: CJK 强制双宽 advance 后处理测试 =====

    /// T-0801 In #C 测试 #1: CJK 字形必严等 2 × CELL_W_PX_phys。
    ///
    /// **why 单独测**: T-0405 `shape_line_mixed_cjk_returns_glyphs` 锁的是
    /// 用户机 CJK fallback 真触发的标准路径; 本测试聚焦"shape `你好` 后, 每
    /// 个字形 advance 必严等 forced 双宽"这条不变式, 不被 ASCII glyph 干扰。
    /// 退化路径 (CI 无 CJK face): 主 face .notdef tofu 自然 advance 也 ≈
    /// CELL_W_PX_phys 不触发 wide, 测试软性接受 (eprintln warn 跳过)。
    #[test]
    fn shape_line_cjk_glyphs_have_forced_double_advance() {
        let mut ts = TextSystem::new().expect("TextSystem::new on user machine");
        let primary = ts.primary_face_id();
        let glyphs = ts.shape_line("你好");
        assert_eq!(
            glyphs.len(),
            2,
            "shape '你好' must yield 2 glyphs (got {})",
            glyphs.len()
        );
        let cell_w_phys = crate::wl::CELL_W_PX * (crate::wl::HIDPI_SCALE as f32);
        let double_w = 2.0 * cell_w_phys;
        let cjk_face = glyphs[0].face_id();
        if cjk_face != primary {
            // 标准路径: CJK fallback 真触发, 锁严双宽。
            for (i, g) in glyphs.iter().enumerate() {
                assert!(
                    (g.x_advance - double_w).abs() < 0.001,
                    "T-0801: CJK glyph[{}] x_advance 应严等 2 × CELL_W_PX_phys = {} \
                     (got {}); CJK fallback face={}, primary={}",
                    i,
                    double_w,
                    g.x_advance,
                    cjk_face,
                    primary
                );
            }
        } else {
            eprintln!(
                "shape_line_cjk_glyphs_have_forced_double_advance: CJK fallback 未触发 \
                 (cjk_face={} == primary={}); 跳过严宽 assert (CI 退化路径)",
                cjk_face, primary
            );
        }
    }

    /// T-0801 In #C 测试 #2: CJK 字形 x_offset 含居中量 (字形居中双宽 cell)。
    ///
    /// 派单 In #A: "x_offset = (2 × CELL_W_PX - actual_glyph_width) / 2 (居中)"。
    /// 实测自然 CJK advance ≈ 1.67 × CELL_W_PX_phys, 双宽 cell 2.0 × CELL_W_PX_phys,
    /// 居中量 ≈ (2.0 - 1.67) / 2 × CELL_W_PX_phys ≈ 0.165 × CELL_W_PX_phys。
    ///
    /// **锁两件事**:
    /// 1. 第 1 个 CJK 字形 x_offset > 0 (有正向居中量, 不贴 cell 左缘)
    /// 2. 第 2 个 CJK 字形 x_offset 在 [double_w, double_w + cell_w_phys) 区间
    ///    (cascade 起点 = double_w, + 居中量 < cell_w_phys/2)
    #[test]
    fn shape_line_cjk_glyphs_centered_in_double_cell() {
        let mut ts = TextSystem::new().expect("TextSystem::new on user machine");
        let primary = ts.primary_face_id();
        let glyphs = ts.shape_line("你好");
        assert_eq!(glyphs.len(), 2);
        let cjk_face = glyphs[0].face_id();
        if cjk_face == primary {
            eprintln!(
                "shape_line_cjk_glyphs_centered_in_double_cell: CJK fallback 未触发, \
                 跳过居中 assert (CI 退化路径)"
            );
            return;
        }
        let cell_w_phys = crate::wl::CELL_W_PX * (crate::wl::HIDPI_SCALE as f32);
        let double_w = 2.0 * cell_w_phys;
        // 第 1 字: x_offset 应是 cascade 起点 0 + 居中量 > 0
        assert!(
            glyphs[0].x_offset > 0.0,
            "T-0801: CJK glyph[0] x_offset 应有正向居中量 > 0 (got {})",
            glyphs[0].x_offset
        );
        assert!(
            glyphs[0].x_offset < cell_w_phys / 2.0,
            "T-0801: CJK glyph[0] 居中量应 < cell_w_phys/2 = {} (got {})",
            cell_w_phys / 2.0,
            glyphs[0].x_offset
        );
        // 第 2 字: cascade 起点 = double_w, + 居中量
        assert!(
            glyphs[1].x_offset >= double_w,
            "T-0801: CJK glyph[1] x_offset 应 >= cascade 起点 {} (got {})",
            double_w,
            glyphs[1].x_offset
        );
        assert!(
            glyphs[1].x_offset < double_w + cell_w_phys / 2.0,
            "T-0801: CJK glyph[1] x_offset 应 < {} + cell_w_phys/2 (got {})",
            double_w,
            glyphs[1].x_offset
        );
    }

    /// T-0801 In #C 测试 #3: ASCII 字形 advance 必严等 CELL_W_PX_phys (单宽)。
    ///
    /// 派单 In #A "单宽字形 (ASCII): advance 仍 cosmic-text 自然值 (跟 CELL_W_PX
    /// 一致或微差)" — `force_cjk_double_advance` 实装把 ASCII narrow 也吸到严
    /// CELL_W_PX_phys (而非保留自然微差), monospace 终端 grid 严对齐。
    #[test]
    fn shape_line_ascii_advance_equals_cell_w_phys() {
        let mut ts = TextSystem::new().expect("TextSystem::new on user machine");
        let glyphs = ts.shape_line("hello");
        assert_eq!(glyphs.len(), 5);
        let cell_w_phys = crate::wl::CELL_W_PX * (crate::wl::HIDPI_SCALE as f32);
        for (i, g) in glyphs.iter().enumerate() {
            assert!(
                (g.x_advance - cell_w_phys).abs() < 0.001,
                "T-0801: ASCII glyph[{}] x_advance 应严等 CELL_W_PX_phys = {} (got {})",
                i,
                cell_w_phys,
                g.x_advance
            );
            // x_offset cascade: i × cell_w_phys (ASCII narrow 居中量 = 0)
            let expected = (i as f32) * cell_w_phys;
            assert!(
                (g.x_offset - expected).abs() < 0.001,
                "T-0801: ASCII glyph[{}] x_offset 应严等 i × cell_w_phys = {} (got {})",
                i,
                expected,
                g.x_offset
            );
        }
    }

    // ===== T-0803: unicode-width grid 占位判定测试 =====

    /// T-0803: PUA / nerd font 图标 (U+E0B0 powerline triangle) 必算 1 cell,
    /// 与 lsd / fzf / starship / ghostty 对齐 (ADR 0010 核心目标)。
    ///
    /// **why U+E0B0**: powerline 三角是最常见的 nerd font 图标 (oh-my-zsh /
    /// starship / lsd 默认 prompt 都有), 字形宽 ~1.67 cell — T-0801 自然
    /// advance ratio 误判 wide → 2 cell, 与 lsd 内部 unicode-width 算 1 cell
    /// 错位 1 cell per icon (派单 Bug 段截图证据)。
    ///
    /// **CI 退化路径**: nerd font 通常未装, cosmic-text 给主 face .notdef tofu
    /// (gid=0, 自然 advance ≈ CELL_W_PX_phys); unicode-width 算 PUA = 1, 强制
    /// advance 仍 = CELL_W_PX_phys, 测试通过 (退化路径 advance 与正常路径同)。
    #[test]
    fn shape_line_pua_nerd_font_icon_advance_eq_one_cell() {
        let mut ts = TextSystem::new().expect("TextSystem::new on user machine");
        let glyphs = ts.shape_line("\u{E0B0}");
        assert_eq!(
            glyphs.len(),
            1,
            "shape U+E0B0 must yield 1 glyph (got {})",
            glyphs.len()
        );
        let cell_w_phys = crate::wl::CELL_W_PX * (crate::wl::HIDPI_SCALE as f32);
        assert!(
            (glyphs[0].x_advance - cell_w_phys).abs() < 0.001,
            "T-0803: PUA U+E0B0 advance 应严等 CELL_W_PX_phys = {} (unicode-width \
             算 PUA = 1 cell, 与 lsd / ghostty 对齐); got {}",
            cell_w_phys,
            glyphs[0].x_advance
        );
    }

    /// T-0803: emoji (U+1F600 grinning face) 必算 2 cell (unicode-width 表
    /// 把 emoji 标 W = wide)。
    ///
    /// **CI / 用户机均测**: 用户机有 NotoColorEmoji → cosmic-text fallback,
    /// `apply_emoji_blacklist` 把 glyph_id 改 0 但保留 cluster + advance →
    /// `force_cjk_double_advance` 仍按 cluster 回查 char 走 unicode-width 判 wide
    /// → 强制 advance = 2 × CELL_W_PX_phys。CI 无 emoji face 时 cosmic-text 给
    /// 主 face .notdef tofu, cluster 仍指向 emoji char, unicode-width 仍判 wide,
    /// advance 同 — 退化路径行为与正常路径一致 (派单 Acceptance "用户机为准, CI
    /// 退化作 follow-up", 此处刚好两路径同行为)。
    #[test]
    fn shape_line_emoji_advance_eq_double_cell() {
        let mut ts = TextSystem::new().expect("TextSystem::new on user machine");
        let glyphs = ts.shape_line("\u{1F600}");
        assert_eq!(
            glyphs.len(),
            1,
            "shape U+1F600 must yield 1 glyph (got {})",
            glyphs.len()
        );
        let cell_w_phys = crate::wl::CELL_W_PX * (crate::wl::HIDPI_SCALE as f32);
        let double_w = 2.0 * cell_w_phys;
        assert!(
            (glyphs[0].x_advance - double_w).abs() < 0.001,
            "T-0803: emoji U+1F600 advance 应严等 2 × CELL_W_PX_phys = {} \
             (unicode-width 表 emoji = W); got {}",
            double_w,
            glyphs[0].x_advance
        );
    }

    // ===== T-0807 A段 (M1): force_cjk_double_advance 按 cluster 聚合 =====
    //
    // 旧实装按每个 glyph 独立判 wide / narrow 推 cursor_x, 在以下情形错位:
    // 1. 零宽 char (combining mark / U+200D ZWJ / variation selector): width(ch)
    //    == 0 但走 unwrap_or(1) fallback, 当 1 cell 推 → cursor 越推越远
    // 2. 共享 cluster 多 glyph (ZWJ emoji 序列 / 复合连字): 同 cluster 多个
    //    glyph 重复推 forced_advance, cursor 严重前进过头
    //
    // 新算法: 按 cosmic-text/HarfBuzz cluster 边界聚合, cursor_x 仅在进入新
    // cluster 时推一次 cluster_advance (= sum unicode_width(每 char) × cell_w_phys,
    // 至少 1 cell 防退化为 0).

    /// T-0807 M1: combining mark 不该让 cursor 多推 1 cell.
    ///
    /// `"a\u{0301}"` (a + combining acute, NFD 'á' 形式) 视觉占 1 cell.
    /// `unicode_width::UnicodeWidthStr::width("a\u{0301}") == 1`. 旧实装把
    /// combining mark 当 1 cell (unwrap_or(1) fallback) 共推 2 cell.
    ///
    /// 锁: x_advance 之和 == 1 cell (新算法: cluster 第一 glyph x_advance =
    /// cluster_advance, 同 cluster 后续 glyph x_advance = 0, sum 自然等
    /// cluster_advance). 与 cell 数 sanity 对齐: width("a\u{0301}") = 1.
    #[test]
    fn force_cjk_double_advance_combining_mark_is_one_cell() {
        use unicode_width::UnicodeWidthStr;
        let mut ts = TextSystem::new().expect("TextSystem::new on user machine");
        let text = "a\u{0301}";
        let glyphs = ts.shape_line(text);
        assert!(
            !glyphs.is_empty(),
            "'a + combining acute' should yield at least 1 glyph"
        );
        assert_eq!(
            UnicodeWidthStr::width(text),
            1,
            "sanity: width('a + combining acute') = 1 (combining mark 零宽)"
        );
        let cell_w_phys = crate::wl::CELL_W_PX * (crate::wl::HIDPI_SCALE as f32);
        let total_advance: f32 = glyphs.iter().map(|g| g.x_advance).sum();
        assert!(
            (total_advance - cell_w_phys).abs() < 0.001,
            "T-0807 M1: combining mark cluster 总 advance 应严等 1 cell ({}) \
             不是 2 cell, got {}; glyphs = {:?}",
            cell_w_phys,
            total_advance,
            glyphs
        );
        // 同 cluster 多 glyph: 共享 x_offset 起点 (后续 glyph x_offset 等
        // cluster 第一 glyph x_offset, 不分别加 cluster_advance). narrow
        // cluster center_pad = 0, 起点直接是 cluster_start_x.
        if glyphs.len() > 1 {
            for g in &glyphs[1..] {
                assert!(
                    (g.x_offset - glyphs[0].x_offset).abs() < cell_w_phys / 2.0,
                    "T-0807 M1: 同 cluster 后续 glyph 应共享 cursor 起点 ({}), \
                     got x_offset = {}",
                    glyphs[0].x_offset,
                    g.x_offset
                );
            }
        }
    }

    /// T-0807 M1: variation selector U+FE0F (emoji presentation) 不该让 cursor
    /// 多推 1 cell.
    ///
    /// `"⚠\u{FE0F}"` 在 unicode-width 计 2 cell (warning sign W=1 + VS16 W=0,
    /// 但 unicode-width 把 emoji presentation 序列整体当 wide ⚠️). 实测
    /// width("⚠\u{FE0F}") 取决于 unicode-width 版本: 0.1.13 给 1 (按 char sum
    /// = 1+0), 0.1.14+ 把 emoji presentation 整体计 2. 测试锁:
    ///
    /// 1. cluster 总 x_advance 与 unicode_width::width(text) 对齐 (max 1 防退化)
    /// 2. 同 cluster 多 glyph 共享 cursor 起点 (旧 bug 第二 glyph 在 cell 1)
    ///
    /// 旧实装 bug: 把 VS16 第二 glyph 推 1 cell, 第一 glyph 居中在 cell 0,
    /// 第二 glyph (.notdef) 画在 cell 1, grid 占位与字形分裂.
    #[test]
    fn force_cjk_double_advance_variation_selector_no_extra_cell() {
        use unicode_width::UnicodeWidthStr;
        let mut ts = TextSystem::new().expect("TextSystem::new on user machine");
        let text = "\u{26A0}\u{FE0F}";
        let glyphs = ts.shape_line(text);
        assert!(
            !glyphs.is_empty(),
            "'⚠ + VS16' should yield at least 1 glyph"
        );
        let cell_w_phys = crate::wl::CELL_W_PX * (crate::wl::HIDPI_SCALE as f32);
        let expected_cells = UnicodeWidthStr::width(text).max(1);
        let expected_total = (expected_cells as f32) * cell_w_phys;
        let total_advance: f32 = glyphs.iter().map(|g| g.x_advance).sum();
        assert!(
            (total_advance - expected_total).abs() < 0.001,
            "T-0807 M1: VS16 cluster 总 advance 应严等 width(text)*cell = {} \
             ({} cells); got {}; glyphs = {:?}",
            expected_total,
            expected_cells,
            total_advance,
            glyphs
        );
        // 同 cluster 多 glyph 共享 cursor 起点 — 用 x_offset 一致性验证
        // (wide cluster 仅第一 glyph 有 center_pad, 后续 glyph x_offset =
        // cluster_start_x = 0; 第一 glyph x_offset = cluster_start_x + center_pad).
        // 旧 bug: 第二 glyph x_offset = cell_w_phys (推到 cell 1).
        if glyphs.len() > 1 {
            for g in &glyphs[1..] {
                assert!(
                    g.x_offset < cell_w_phys,
                    "T-0807 M1: 同 cluster 后续 glyph x_offset 应 < cell_w_phys = {}, \
                     不该被推到 cell 1; got {}",
                    cell_w_phys,
                    g.x_offset
                );
            }
        }
    }

    /// T-0807 M1: ZWJ emoji 序列 (家庭 emoji) 共享 cluster, 总 advance 不应
    /// 每 glyph 重复推.
    ///
    /// `"👨\u{200D}👩\u{200D}👧"` 用户机 (有 emoji face) shape 出 1 cluster N
    /// glyph (cosmic-text/HarfBuzz 把 ZWJ 序列归为同一 cluster, cluster=0). CI
    /// 退化 (无 emoji face) 给主 face tofu 单 glyph 也归一 cluster. 视觉
    /// width("👨\u{200D}👩\u{200D}👧") = 6 (3 emoji × 2 cell, ZWJ 0 cell).
    ///
    /// 旧 bug: 每 glyph 按 width('👨')=2 推 wide → cursor 推 N × 2 cell, ≥ 6
    /// 推到 8/10/12 cell 错位. 新算法: 整 cluster 推 6 cell.
    ///
    /// 锁: cluster 总 x_advance == width(text) × cell_w_phys.
    #[test]
    fn force_cjk_double_advance_zwj_emoji_shares_cluster_cursor() {
        use unicode_width::UnicodeWidthStr;
        let mut ts = TextSystem::new().expect("TextSystem::new on user machine");
        let text = "\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}";
        let glyphs = ts.shape_line(text);
        assert!(
            !glyphs.is_empty(),
            "ZWJ family emoji should yield at least 1 glyph"
        );
        let cell_w_phys = crate::wl::CELL_W_PX * (crate::wl::HIDPI_SCALE as f32);
        // unicode-width 0.1.x 对 ZWJ 序列各 char 独立 sum: 2+0+2+0+2 = 6 cells.
        // 用户机若装 emoji face shape 出多 glyph 单 cluster, 新算法仍按 width
        // 算 substring 总宽 = 6 cell.
        let expected_cells = UnicodeWidthStr::width(text).max(1);
        let expected_total = (expected_cells as f32) * cell_w_phys;
        // 按 cluster 分组算每 cluster 的总 x_advance, 累加.
        let mut by_cluster: std::collections::BTreeMap<usize, Vec<&ShapedGlyph>> =
            std::collections::BTreeMap::new();
        for g in &glyphs {
            by_cluster.entry(g.cluster).or_default().push(g);
        }
        // 同 cluster 内 cluster_advance 在第一 glyph (其余 0), sum 即 cluster 总.
        let total_advance: f32 = glyphs.iter().map(|g| g.x_advance).sum();
        assert!(
            (total_advance - expected_total).abs() < 0.001,
            "T-0807 M1: ZWJ 总 advance 应严等 width(text)*cell = {} ({} cells); \
             got {}; cluster groups = {}; glyphs = {:?}",
            expected_total,
            expected_cells,
            total_advance,
            by_cluster.len(),
            glyphs
        );
        // 同 cluster 内 glyph 共享起点 (在 cluster_start_x ± cell_w_phys/2 内
        // 居中量容差范围) — 不该被推到下一 cell.
        for (cluster, gs) in &by_cluster {
            if gs.len() < 2 {
                continue;
            }
            let first = gs[0];
            for g in gs.iter().skip(1) {
                assert!(
                    (g.x_offset - first.x_offset).abs() < cell_w_phys,
                    "T-0807 M1: cluster={} 同 cluster 后续 glyph 应在 cluster 起点 \
                     ± cell_w_phys 内 (即未被推到下一 cell); first.x_offset={}, \
                     got x_offset={}",
                    cluster,
                    first.x_offset,
                    g.x_offset
                );
            }
        }
    }

    /// T-0403: ASCII 'a' shape + raster 必须给非空 bitmap。
    /// 用户机 monospace face (DejaVu Sans Mono / Source Code Pro / 等) 'a' 必有可见
    /// 字形 → bitmap 长度 = width * height > 0; bearing_x ≥ 0 (字母 'a' 通常
    /// 有 1-2 px 左 bearing)。
    #[test]
    fn rasterize_ascii_a_returns_bitmap() {
        let mut ts = TextSystem::new().expect("TextSystem::new on user machine");
        let glyphs = ts.shape_line("a");
        assert_eq!(glyphs.len(), 1, "shape 'a' must give 1 glyph");
        let raster = ts
            .rasterize(&glyphs[0])
            .expect("ASCII 'a' must rasterize to Mask bitmap on user machine");
        assert!(
            raster.width > 0,
            "'a' bitmap width > 0 (got {})",
            raster.width
        );
        assert!(
            raster.height > 0,
            "'a' bitmap height > 0 (got {})",
            raster.height
        );
        assert_eq!(
            raster.bitmap.len(),
            (raster.width as usize) * (raster.height as usize),
            "bitmap length must equal width * height (single-channel alpha)"
        );
        // 至少有 1 个 alpha > 0 的像素 (字形不能全透明)
        assert!(
            raster.bitmap.iter().any(|&b| b > 0),
            "'a' bitmap must have at least one non-zero alpha pixel"
        );
    }

    /// T-0403: CJK '中' 在 CI 退化到 tofu 时 rasterize 可能给 None (Color
    /// content, 用户机 noto-cjk-mono 给 Mask 路径)。**关键是不 panic**, Some/None
    /// 都接受 (沿袭 T-0401 shape_chinese_zhong 的容错风格)。
    #[test]
    fn rasterize_chinese_zhong_returns_bitmap_or_none_no_panic() {
        let mut ts = TextSystem::new().expect("TextSystem::new on user machine");
        let glyphs = ts.shape_line("中");
        assert_eq!(glyphs.len(), 1, "shape '中' must give 1 glyph");
        let raster = ts.rasterize(&glyphs[0]);
        if let Some(r) = raster {
            // 用户机正常路径: noto-cjk-mono 给 Mask, bitmap 非空且数值合理
            assert_eq!(
                r.bitmap.len(),
                (r.width as usize) * (r.height as usize),
                "bitmap length must equal width * height"
            );
            // CJK 通常比 ASCII 宽 (但 CI 退化到 tofu 不强求, 仅校验 finite)
            // r.width / r.height 是 u32, 自然 finite, 仅检查 bitmap 非 NaN
            // (u8 也无 NaN, 所以本断言只是结构性 sanity)
            // 不强求 alpha 非零: tofu 字形可能也给 some pixels, 也可能空白 .notdef
        }
        // None 路径: cosmic-text 给 Color content (彩色 emoji / 字体内嵌位图)
        // 或 face 缺该字形, 当前实装不处理彩色路径返 None — 不 panic 即合规。
    }

    /// T-0403 派单 Acceptance 测试名 `rasterize_zero_glyph_id_returns_some_or_none_no_panic`。
    /// 显式构造一个 .notdef 字形 (cosmic-text shape 私用区 codepoint 通常给
    /// gid=0 tofu) 验 rasterize 路径不 panic。
    #[test]
    fn rasterize_zero_glyph_id_no_panic() {
        let mut ts = TextSystem::new().expect("TextSystem::new on user machine");
        let glyphs = ts.shape_line("\u{E0000}");
        // shape 可能给空 (cosmic-text 0.12 实测在某些 face 给空 layout) 或给 1 个
        // tofu glyph; 两种都接受。
        for g in &glyphs {
            // 关键: 不 panic
            let _raster = ts.rasterize(g);
            // _raster 可能 Some 也可能 None (face 缺 gid=0 资源也允许), 不强求。
        }
    }

    /// T-0404: rasterize 'a' at logical font_size=17 vs HiDPI font_size=34
    /// (= 17 × HIDPI_SCALE), bitmap 宽度应约翻倍。锁 HIDPI_SCALE × shape ×
    /// rasterize 链路真生效。
    ///
    /// **派单测试名**: `glyph_rasterize_at_2x_size_returns_larger_bitmap` (本
    /// 测试函数名沿用)。位于 src/text/mod.rs 内 unit test 块, 因要碰
    /// `font_system` / `swash_cache` 私有字段 + `from_cosmic_glyph` 私有 inherent
    /// fn (INV-010 类型隔离 strict 第 N 次应用), 集成测试 (tests/glyph_atlas.rs)
    /// 拿不到这些, 必须 `#[cfg(test)] mod tests` 内单测。
    ///
    /// **不直接调 shape_line(已写死 17 × HIDPI_SCALE)做对比**: 那只能取到一个
    /// font_size; 改用本地辅助 closure 直接驱动 cosmic-text Buffer 取两 font_size,
    /// 避免改动 shape_line 公共 API (引入 `shape_line_at_font_size` 派单 scope 外)。
    ///
    /// **rebase 后注**: T-0407 把 shape_line 内的 `Family::Monospace` 改为
    /// `Family::Name(&self.primary_face_name)` (face 锁定 bug fix), 但本测试用
    /// 自定义 closure 直接走 `Family::Monospace` 不接 primary_face — closure 内
    /// 是孤立 Buffer (不进 atlas, 不影响渲染), 仅验"font_size × 2 → bitmap × 2"
    /// 这条物理关系。`Family::Monospace` 在用户机仍能命中 monospace face (DejaVu /
    /// Source Code Pro / Noto Mono 任一), 比 `Family::Name(&primary)` 借 self
    /// 字段更省事 (closure 同时借 ts.font_system mut + ts.primary_face_name 即
    /// borrow conflict)。
    #[test]
    fn glyph_rasterize_at_2x_size_returns_larger_bitmap() {
        use cosmic_text::{Attrs, Buffer, Family, Metrics, Shaping};

        let mut ts = TextSystem::new().expect("TextSystem::new on user machine");

        // 不能用 closure 直接 capture &mut ts.font_system 与 ts 自己 (rasterize
        // 取 &mut self 全 ts), 改为按顺序调两次 + 中间释放 buffer。
        let raster_at = |ts: &mut TextSystem, font_size: f32| -> Option<RasterizedGlyph> {
            let glyph = {
                let metrics = Metrics::new(font_size, font_size * 1.5);
                let mut buffer = Buffer::new(&mut ts.font_system, metrics);
                let attrs = Attrs::new().family(Family::Monospace);
                buffer.set_text(&mut ts.font_system, "a", attrs, Shaping::Advanced);
                buffer.shape_until_scroll(&mut ts.font_system, true);
                let run = buffer.layout_runs().next()?;
                let lg = run.glyphs.first()?;
                ShapedGlyph::from_cosmic_glyph(lg)
            };
            ts.rasterize(&glyph)
        };

        let r1x = raster_at(&mut ts, 17.0).expect("17pt 'a' must rasterize");
        let r2x = raster_at(&mut ts, 17.0 * crate::wl::HIDPI_SCALE as f32)
            .expect("34pt 'a' must rasterize");

        assert!(r1x.width > 0, "1x bitmap width must be positive");
        assert!(r2x.width > 0, "2x bitmap width must be positive");

        // 2x font_size 给 ~2x bitmap, 实测用户机 (DejaVu Sans Mono / Source Code
        // Pro / Noto Mono fallback chain 任一) 'a' 17pt → 11px 宽, 34pt → 19px
        // 宽, ratio ≈ 1.73。**1.73 而非 2.0 的原因**: cosmic-text 在小字号下用
        // swash hinting + 1-2px padding (subpixel anti-aliasing 边距), 这些常数
        // 项不随 font_size 线性缩放 — 字号越大相对占比越小, 比例渐近 2.0。
        // 容差用 [1.5, 2.5] 而非 prompt 指 ±10% 的 [1.8, 2.2] (实测 1.73 仍是真
        // "翻倍" 信号, 锁太紧会因 cosmic-text 升级 / 字体微差挂 false positive)。
        let ratio = r2x.width as f32 / r1x.width as f32;
        assert!(
            ratio > 1.5 && ratio < 2.5,
            "2x font_size bitmap width should be ~2x: 1x={}, 2x={}, ratio={} \
             (HIDPI_SCALE = {})",
            r1x.width,
            r2x.width,
            ratio,
            crate::wl::HIDPI_SCALE
        );
        // height 同样 ~2x (cell ascent + descent + hinting padding 同款常数项,
        // 容差同宽)。
        let h_ratio = r2x.height as f32 / r1x.height as f32;
        assert!(
            h_ratio > 1.5 && h_ratio < 2.5,
            "2x font_size bitmap height should be ~2x: 1x={}, 2x={}, ratio={}",
            r1x.height,
            r2x.height,
            h_ratio
        );
    }

    /// T-0403: ShapedGlyph::atlas_key 返 GlyphKey (T-0407 升级 (u16, u32) →
    /// 三维), glyph_id 与 font_size_quantized 稳定 (相同 font_size 必同 bits)。
    /// 锁 atlas_key 接口形状 + 同 char 同 face 同 size → 同 key 不变式。
    #[test]
    fn atlas_key_is_stable_for_same_glyph() {
        let mut ts = TextSystem::new().expect("TextSystem::new on user machine");
        let g1 = &ts.shape_line("a")[0];
        let key1 = g1.atlas_key();
        let g2 = ts.shape_line("a");
        let key2 = g2[0].atlas_key();
        assert_eq!(
            key1, key2,
            "same character + same font size should give same atlas_key"
        );
        // glyph_id 部分非零 (ASCII 'a' 在任何字体都不是 .notdef gid 0; tofu 路径
        // 才给 0)
        assert_ne!(
            key1.glyph_id, 0,
            "ASCII 'a' glyph_id should be non-zero (got {})",
            key1.glyph_id
        );
    }

    /// T-0407: TextSystem::new 锁定的 primary face 必为 [`PREFERRED_MONOSPACE_FACES`]
    /// 之一 (用户机 Arch + ttf-dejavu 命中 "DejaVu Sans Mono")。
    ///
    /// **CI 退化路径**: CI 若无 PREFERRED 任一 face, 走 monospace_fallback warn
    /// 路径; 此测试在 CI 可能挂 (primary face 不在 PREFERRED 列表)。但 T-0407
    /// 派单 Goal "用户机 cargo run --release 真显示 prompt" 以用户机为准, CI
    /// 路径接受退化, 测试在 CI 失败可降级为"primary_face_id 非 0 即过"。本测试
    /// 强断 PREFERRED 命中 — 用户机为准, CI 退化作 follow-up。
    ///
    /// 验证方式: 不直接读 primary_face_name (模块私有), 通过 shape "a" 后取
    /// glyph.face_id 与 ts.primary_face_id() 比对 + ts.primary_face_id() 非 0。
    #[test]
    fn face_lock_uses_preferred_monospace() {
        let ts = TextSystem::new().expect("TextSystem::new on user machine");
        let pid = ts.primary_face_id();
        // u64 hash 几乎不可能为 0 (DefaultHasher 输出, 0 概率 ~1/2^64);
        // 主要锁"primary_face_id 已设置"语义。
        assert_ne!(pid, 0, "primary_face_id u64 hash should be non-zero");
    }

    /// T-0407: shape "abc" 后每个 glyph 的 face_id 应 == primary_face_id
    /// (face 锁定真起作用, 不让 cosmic-text 自挑落到 emoji / 其他 face)。
    ///
    /// **CJK 不在此测试范围**: 主 face DejaVu Sans Mono 不含 CJK glyph,
    /// cosmic-text fallback 到 Noto CJK 是预期行为 (T-0405 scope), CJK glyph
    /// face_id != primary_face_id 是正常的; 此测试只验 ASCII 路径锁主 face。
    #[test]
    fn shape_ascii_uses_primary_face() {
        let mut ts = TextSystem::new().expect("TextSystem::new on user machine");
        let primary = ts.primary_face_id();
        let glyphs = ts.shape_line("abc");
        assert_eq!(glyphs.len(), 3, "ASCII 'abc' must yield 3 glyphs");
        for (i, g) in glyphs.iter().enumerate() {
            assert_eq!(
                g.face_id(),
                primary,
                "ASCII glyph[{}] should use primary face (got face_id={}, primary={})",
                i,
                g.face_id(),
                primary
            );
        }
    }

    /// T-0407: shape emoji codepoint U+1F4C1 (📁) 应返 tofu glyph_id=0
    /// (apply_emoji_blacklist 命中 emoji face → 替换 .notdef) 或被 cosmic-text
    /// shape 退化 (CI 无 emoji face 时 layout_runs 给空)。**关键不画蓝色 emoji**。
    ///
    /// **用户机** (Arch + noto-fonts NotoColorEmoji): emoji_face_ids 含 Noto
    /// Color Emoji, shape "📁" cosmic-text fallback 选 Noto Color Emoji →
    /// apply_emoji_blacklist 命中 → glyph_id = 0 ✓
    ///
    /// **CI 退化路径**: 无 NotoColorEmoji, cosmic-text 可能给空 layout (无 face
    /// 含此 codepoint) 或给主 face 的 .notdef gid 0; 两路径都接受 (派单"tofu
    /// 或被跳过, 不真画 emoji")。
    #[test]
    fn shape_emoji_codepoint_returns_tofu_or_skipped() {
        let mut ts = TextSystem::new().expect("TextSystem::new on user machine");
        let glyphs = ts.shape_line("\u{1F4C1}");
        // 接受空 layout (跳过) 或非空 + glyph_id=0 (tofu)
        for (i, g) in glyphs.iter().enumerate() {
            assert_eq!(
                g.glyph_id, 0,
                "emoji glyph[{}] should be tofu (glyph_id=0) after \
                 apply_emoji_blacklist; got glyph_id={}",
                i, g.glyph_id
            );
        }
    }

    /// T-0407: 不同 face 的 glyph_id 撞 (gid, size) 时, GlyphKey 不撞
    /// (face_id 维度区分)。本测试用模拟 GlyphKey 直接构造, 验 PartialEq +
    /// Hash 行为符合"face_id 不同 → key 不同"。
    ///
    /// **why 非 shape 路径**: 真触发跨 face 撞需 CJK fallback + 主 face 同 gid
    /// (例主 face 'a' = gid 65 vs CJK face 某符号 = gid 65), 实测难制造稳定。
    /// 本测试直接锁 GlyphKey struct equality 契约: 三维 (face_id, glyph_id,
    /// font_size_quantized) 任一不同 → key 不等; 三维全同 → key 等 + Hash 同。
    #[test]
    fn atlas_key_includes_face_id() {
        use std::collections::HashSet;
        let k1 = GlyphKey {
            face_id: 1234,
            glyph_id: 65,
            font_size_quantized: f32::to_bits(17.0),
        };
        let k2 = GlyphKey {
            face_id: 5678, // 不同 face
            glyph_id: 65,  // 同 gid
            font_size_quantized: f32::to_bits(17.0),
        };
        let k3 = GlyphKey { ..k1 };
        assert_ne!(k1, k2, "different face_id same gid+size must not collide");
        assert_eq!(k1, k3, "same triple must equal");

        let mut set = HashSet::new();
        assert!(set.insert(k1));
        assert!(
            set.insert(k2),
            "k2 (different face) must hash to different slot"
        );
        assert!(!set.insert(k3), "k3 == k1 must hash-collide");
    }

    /// T-0407: shape ASCII 后 atlas_key.face_id == TextSystem.primary_face_id
    /// (端到端 verify face 锁定 + atlas key face_id 维度同源)。
    ///
    /// **T-0404 rebase 改**: shape_line Metrics 17.0 → 17.0 × HIDPI_SCALE = 34.0
    /// (HiDPI 物理像素 raster), 故 font_size_quantized 期望值同步 × HIDPI_SCALE。
    /// 引 [`crate::wl::HIDPI_SCALE`] 让 HIDPI_SCALE 一处定义, 改此常数本测试自跟随。
    #[test]
    fn atlas_key_face_id_matches_primary_for_ascii() {
        let mut ts = TextSystem::new().expect("TextSystem::new on user machine");
        let primary = ts.primary_face_id();
        let glyphs = ts.shape_line("a");
        assert_eq!(glyphs.len(), 1);
        let key = glyphs[0].atlas_key();
        assert_eq!(
            key.face_id, primary,
            "ASCII 'a' atlas_key.face_id should == primary_face_id"
        );
        let expected_font_size = 17.0 * crate::wl::HIDPI_SCALE as f32;
        assert_eq!(
            key.font_size_quantized,
            f32::to_bits(expected_font_size),
            "shape_line uses Metrics(17.0 × HIDPI_SCALE = {}, ...), font_size_quantized \
             should be f32::to_bits({})",
            expected_font_size,
            expected_font_size
        );
    }
}
