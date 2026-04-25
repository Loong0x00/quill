//! Integration tests for glyph rasterization → atlas key + caching shape (T-0403).
//!
//! 因 [`crate::wl::render::Renderer`] 需要真 Wayland 连接 + GPU adapter, 集成测试
//! 不构造 Renderer; 改为锁住"text 层喂出的 ShapedGlyph + atlas_key + RasterizedGlyph"
//! 这条 API 契约 (派单 In #E "tests/glyph_atlas.rs 集成测试 (新文件)"):
//!
//! - `atlas_caches_glyph_after_first_render`: 同一字符两次 shape 给同 atlas_key
//!   (cache hit 路径锁住, 防 ShapedGlyph::atlas_key 改 stable contract)
//! - `atlas_handles_multiple_glyphs_no_collision`: 10+ 不同字符各自 atlas_key
//!   不撞 (uv allocate 阶段在 Renderer 内, 但 key 唯一是 Phase 4 假设的前提)
//!
//! Phase 4 视觉 milestone (Renderer::draw_frame 真画到屏幕) 由手测 acceptance
//! 第 6 项验证, agent 跑不动 (Wayland native 连接 + 5090 GPU 实测)。
//!
//! 沿袭 T-0306 `tests/resize_chain.rs` 同款 trade-off: 集成测试只跑能 headless
//! 部分, GPU 真路径走手测 + Phase 6 soak。

use quill::text::TextSystem;

#[test]
fn atlas_caches_glyph_after_first_render() {
    // 第一次 shape "a" → 拿 atlas_key
    let mut ts = TextSystem::new().expect("TextSystem::new on user machine");
    let glyphs1 = ts.shape_line("a");
    assert_eq!(glyphs1.len(), 1, "shape 'a' must give 1 glyph");
    let key1 = glyphs1[0].atlas_key();

    // 第二次 shape 'a'(模拟下一帧再画同字符)→ 同 atlas_key
    let glyphs2 = ts.shape_line("a");
    assert_eq!(glyphs2.len(), 1);
    let key2 = glyphs2[0].atlas_key();
    assert_eq!(
        key1, key2,
        "same glyph shape twice must give stable atlas_key (HashMap cache hit 前提)"
    );

    // 第一次 raster → 拿 bitmap
    let raster1 = ts
        .rasterize(&glyphs1[0])
        .expect("ASCII 'a' must rasterize on user machine");
    assert!(raster1.width > 0 && raster1.height > 0);
    let len1 = raster1.bitmap.len();

    // 第二次 raster (本 ticket 不走 SwashCache 内部 HashMap, 但接口契约是
    // raster 同字形给同尺寸的 bitmap)
    let raster2 = ts.rasterize(&glyphs2[0]).expect("second raster ok");
    assert_eq!(
        raster2.width, raster1.width,
        "second raster width must match first"
    );
    assert_eq!(
        raster2.height, raster1.height,
        "second raster height must match first"
    );
    assert_eq!(
        raster2.bitmap.len(),
        len1,
        "second raster bitmap length must match first"
    );
}

#[test]
fn atlas_handles_multiple_glyphs_no_collision() {
    // 10+ 不同字符的 atlas_key 互不相同 (Phase 4 单 monospace 面假设下, 不同
    // gid 必给不同 (gid, font_size_bits) tuple)。
    let mut ts = TextSystem::new().expect("TextSystem::new on user machine");
    // 选 ASCII printable, 必有不同 gid。空格 gid 也独一档 (face 内 space 单独 gid)。
    let text = "abcdefghij0123";
    let glyphs = ts.shape_line(text);
    assert_eq!(
        glyphs.len(),
        text.chars().count(),
        "ASCII printable shape must 1:1 to chars"
    );
    let keys: std::collections::HashSet<(u16, u32)> =
        glyphs.iter().map(|g| g.atlas_key()).collect();
    assert_eq!(
        keys.len(),
        glyphs.len(),
        "all distinct ASCII glyphs must have distinct atlas_key (no collision in Phase 4 \
         single-monospace face); got {} unique keys for {} glyphs",
        keys.len(),
        glyphs.len()
    );
    // 每个 glyph rasterize 不 panic (raster 路径 sanity)
    for g in &glyphs {
        let _ = ts.rasterize(g); // Some 或 None 都接受, 关键不 panic
    }
}
