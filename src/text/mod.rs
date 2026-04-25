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
}

/// 单个 shaped glyph 的几何信息(Phase 4 起步最小集)。
///
/// **类型隔离**: 不暴露 [`cosmic_text::LayoutGlyph`](其 15 字段含
/// cosmic-text 内部 `font_id: ID` / `cache_key_flags` / `level: Level`
/// 等不稳定布局状态)。本 struct 是渲染层真正用得上的最小子集,Phase 4
/// 后续 ticket(T-0402..T-0406)会按需扩字段(`x_offset` / `y_offset` /
/// `font_id_quill_local: u32` / `font_size_px: f32` 等),仍走
/// [`Self::from_cosmic_glyph`] 模块私有 inherent fn 注入,**不**反向
/// 构造或 `From` impl。
///
/// **字段语义**:
/// - `glyph_id`: face 内 glyph index(u16, OpenType 标准 gid),光栅化阶段
///   (T-0403)用作 atlas key 一部分(配合 face id + size)
/// - `x_advance`: 横向推进像素(已 layout,等价 HarfBuzz `x_advance`)。
///   monospace 字体下应近似一致(例 14pt 约 8-10px),Phase 4 字形 ticket
///   用此值动态测量 cell pixel size 替换 T-0306 临时常数
/// - `y_advance`: 竖向推进像素(横排恒 0;给未来竖排 hook)
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ShapedGlyph {
    pub glyph_id: u16,
    pub x_advance: f32,
    pub y_advance: f32,
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
    ///
    /// **跨版本升级路径**: cosmic-text 0.12 → 0.19 LayoutGlyph 字段稳定
    /// (实测 0.19 Has `glyph_id` / `w` 同名同类型)。若未来 1.x 重命名 / 改
    /// 类型,本 fn body 是唯一改动点。`LayoutGlyph` 是 struct 不是 enum,
    /// 字段未消化时编译期不报警 —— 这是 INV-010 验证段对 struct 类型的已知
    /// 边界(struct 靠"渲染层只用 glyph_id/x_advance/y_advance 三字段"约定锁住,
    /// 而非 enum 的 exhaustive match catch).
    fn from_cosmic_glyph(g: &cosmic_text::LayoutGlyph) -> Self {
        Self {
            glyph_id: g.glyph_id,
            x_advance: g.w,
            y_advance: 0.0,
        }
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
}
