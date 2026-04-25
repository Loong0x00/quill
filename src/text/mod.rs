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

use anyhow::{anyhow, Result};

/// quill 字体子系统 —— cosmic-text 封装。
///
/// **类型隔离(INV-010)**: cosmic-text 类型(`FontSystem` / `SwashCache` /
/// `Buffer` / `LayoutGlyph` / `Attrs` / `Family` / `Metrics` / `Shaping`)严格
/// 锁在本模块内,公共 API 仅暴露 quill 自有的 [`ShapedGlyph`]。沿袭 T-0302
/// [`crate::term::CellPos`] / T-0303 [`crate::term::CursorShape`] / T-0304
/// [`crate::term::ScrollbackPos`] / T-0305 [`crate::term::Color`] 同款套路 ——
/// 给未来换 cosmic-text 主版本(0.x → 1.x)/ 换字体引擎(例 fontique)留单一
/// 改动点 [`ShapedGlyph::from_cosmic_glyph`]。
///
/// **启动校验**: [`Self::new`] 加载系统字体后扫 fontdb,若没有任何 monospace
/// face 直接 [`Err`] —— Phase 4 目标看见字,没字体退出比静默 tofu 友好
/// (见 CLAUDE.md "目标 / 非目标"段:"2x HiDPI 整数缩放 + CJK 字形正常")。
///
/// **字段顺序与 drop 语义**: `font_system` / `swash_cache` 都是堆分配 owned
/// 资源,互不持指针引用,drop 顺序无观测者(与 INV-001 / INV-002 / INV-008
/// 那种"字段顺序定 wl/wgpu/pty 资源链"性质完全不同),按声明顺序 drop 即可。
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
        let has_monospace = font_system.db().faces().any(|f| f.monospaced);
        if !has_monospace {
            return Err(anyhow!(
                "TextSystem::new: no monospace font face found in system fontdb \
                 (check `fc-list :spacing=mono`); quill 当前需要 monospace 字体"
            ));
        }
        Ok(Self {
            font_system,
            swash_cache: cosmic_text::SwashCache::new(),
        })
    }

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
        let attrs = Attrs::new().family(Family::Monospace);

        let mut s = String::new();
        s.push(c);
        buffer.set_text(&mut self.font_system, &s, attrs, Shaping::Advanced);
        buffer.shape_until_scroll(&mut self.font_system, false);

        buffer
            .layout_runs()
            .next()
            .and_then(|run| run.glyphs.first().map(ShapedGlyph::from_cosmic_glyph))
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

        let metrics = Metrics::new(17.0, 25.0);
        let mut buffer = Buffer::new(&mut self.font_system, metrics);
        let attrs = Attrs::new().family(Family::Monospace);

        buffer.set_text(&mut self.font_system, text, attrs, Shaping::Advanced);
        buffer.shape_until_scroll(&mut self.font_system, true);

        buffer
            .layout_runs()
            .flat_map(|run| {
                run.glyphs
                    .iter()
                    .map(ShapedGlyph::from_cosmic_glyph)
                    .collect::<Vec<_>>()
            })
            .collect()
    }
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
            cache_key: physical.cache_key,
        }
    }

    /// Atlas slot HashMap key (派单 Scope/In #B "atlas key = (glyph_id,
    /// font_size_quantized)").
    ///
    /// 返 `(glyph_id, font_size_bits)` 作 quill 自定义 key, 渲染层
    /// [`crate::wl::render::GlyphAtlas`] 用此 key 决定该字形是否已上 atlas、
    /// uv 槽位在哪。`font_size_bits` 是 `f32::to_bits(font_size_px)` 的稳定
    /// 量化表示 (相同 font_size 必同 bits, 不会因浮点比较抖动)。
    ///
    /// **INV-010**: 返 quill 自定义 `(u16, u32)` tuple, 不暴露 cosmic-text
    /// `CacheKey` / `font_size_bits: u32`(虽然两者底层数值相等, quill 这条
    /// 接口承诺仅是 "u32 量化值", 不允许下游假设是 cosmic-text bits 表达)。
    ///
    /// **Phase 4 假设**: 单 monospace 主面 + cosmic-text fallback chain。同
    /// gid 跨 face (例 Latin face 的 'a' = gid 65 vs CJK face 内某符号也恰为
    /// gid 65) atlas 会**冲突 (复用错误字形)** — T-0405 加 face_id 维度修
    /// 复 (届时本 fn 改返 `(u64, u16, u32)` 或 quill 自定义 GlyphKey struct)。
    /// Phase 4 prompt 路径 ASCII 主导, 实测无观测, 派单 Out 段 "CJK fallback
    /// 规则细化 T-0405" 已显式覆盖。
    pub fn atlas_key(&self) -> (u16, u32) {
        (self.cache_key.glyph_id, self.cache_key.font_size_bits)
    }
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
    /// CJK 双宽性质 (东亚字 advance ≈ 2x ASCII) 由字体设计保证, **本测试
    /// 不做 advance 数值断言** (CI 退化到 tofu 时 advance 可能 ≈ ASCII 宽,
    /// 不能用 advance 区分 CJK / ASCII)。
    #[test]
    fn shape_line_mixed_cjk_returns_glyphs() {
        let mut ts = TextSystem::new().expect("TextSystem::new on user machine");
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

    /// T-0403: ShapedGlyph::atlas_key 返 (u16, u32), glyph_id 与 font_size_bits
    /// 稳定 (相同 font_size 必同 bits)。锁 atlas_key 接口形状, 防 Phase 5+
    /// 改 (u16, u32) → (u64, u16, u32) 时无注解默改。
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
            key1.0, 0,
            "ASCII 'a' glyph_id should be non-zero (got {})",
            key1.0
        );
    }
}
