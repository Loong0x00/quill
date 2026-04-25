//! wgpu surface 绑定与每帧清屏。
//!
//! 本 ticket (T-0102) 只负责:从一对 `wl_display` / `wl_surface` 裸指针建出 wgpu
//! `Surface`,configure 为初始尺寸,每帧提交一个纯深蓝清屏 pass。resize 重配置、
//! GPU 资源优雅释放、字形渲染分别归 T-0103 / T-0104 / 后续 ticket。
//!
//! 设计取舍:
//! - `Instance` / `Surface` / `Device` / `Queue` 四件套全部打包进 [`Renderer`],
//!   供 `window.rs` 当不透明字段持有。上层不需要再碰 wgpu 类型。
//! - adapter / device 初始化是 async,用 `pollster::block_on` 同步化,不拉 tokio
//!   —— 与 CLAUDE.md "单线程事件循环" 不变式一致。
//! - SCTK 0.19 不实现 `raw_window_handle::HasDisplayHandle`,手工从 `Connection`
//!   与 `WlSurface` 的裸指针构造 handle,via `SurfaceTargetUnsafe::RawHandle`。

use std::collections::HashMap;
use std::ffi::c_void;
use std::ptr::NonNull;

use anyhow::{anyhow, Context, Result};
use raw_window_handle::{
    RawDisplayHandle, RawWindowHandle, WaylandDisplayHandle, WaylandWindowHandle,
};

use crate::term::CellRef;
use crate::text::{GlyphKey, ShapedGlyph, TextSystem};

/// 目标深蓝色,sRGB 空间的 `#0a1030`。本 ticket 的 acceptance 把它钉死:
/// ```text
/// R = 0x0a = 10, G = 0x10 = 16, B = 0x30 = 48
/// ```
/// 任何改动都应该同步改这里,同时 `color_matches_spec` 测试会挡回。
pub const CLEAR_COLOR_SRGB_U8: [u8; 4] = [0x0a, 0x10, 0x30, 0xff];

/// sRGB 分量 → 线性分量。LoadOp::Clear 的值在 sRGB 格式 view 上写入前会被当作
/// 线性空间,再做 sRGB 编码。所以如果 surface 是 `*UnormSrgb` 格式,我们要先
/// 把 `#0a1030` 解码到线性,清屏后 GPU 会再编码回来,最终像素就是期望的 sRGB。
fn srgb_to_linear(v: f64) -> f64 {
    if v <= 0.04045 {
        v / 12.92
    } else {
        ((v + 0.055) / 1.055).powf(2.4)
    }
}

fn clear_color_for(format: wgpu::TextureFormat) -> wgpu::Color {
    let [r8, g8, b8, _] = CLEAR_COLOR_SRGB_U8;
    let r = f64::from(r8) / 255.0;
    let g = f64::from(g8) / 255.0;
    let b = f64::from(b8) / 255.0;
    if format.is_srgb() {
        wgpu::Color {
            r: srgb_to_linear(r),
            g: srgb_to_linear(g),
            b: srgb_to_linear(b),
            a: 1.0,
        }
    } else {
        wgpu::Color { r, g, b, a: 1.0 }
    }
}

pub struct Renderer {
    // 字段声明顺序即 drop 顺序(Rust 按声明**正向**析构):
    // surface(第 1)先释放,instance(最后)最后。surface 依赖 instance 保持
    // Vulkan/GL 实例存活;device/queue 依赖 adapter(已被构造完 drop,device
    // 自带引用保持 GPU context)。见 docs/invariants.md INV-002。
    //
    // T-0305:`cell_pipeline` / `cell_vertex_buffer` 持 wgpu device 内部引用,
    // 必须**先于** `device` drop —— 放 surface 之后、device 之前。lazy 初始化
    // (Option),首次 [`draw_cells`] 时建好 pipeline + 预分配 vertex buffer,
    // 之后每帧 reuse(`queue.write_buffer` 写新 vertex 数据,不重建)。
    // 派单 "wgpu Pipeline / Layout / BindGroup 创建一次复用" 的硬约束。
    //
    // T-0403:`glyph_atlas` (含 Texture/View/Sampler/BindGroup/BindGroupLayout) /
    // `glyph_pipeline` / `glyph_vertex_buffer` 同样持 device 内部引用, 放 cell
    // 三件套之后、device 之前。lazy 初始化, 首次 [`draw_frame`] 建好。
    // INV-002 entry 同步更新 (T-0305 / T-0306 / T-0399 follow-up 模式)。
    surface: wgpu::Surface<'static>,
    cell_pipeline: Option<wgpu::RenderPipeline>,
    cell_vertex_buffer: Option<wgpu::Buffer>,
    /// 当前 vertex buffer 容量(以**顶点数**计,非字节)。增长策略:首次按
    /// `cols * rows * VERTS_PER_CELL` 分配,后续若 cell 总数超过容量则重建
    /// (Phase 3 不会变 — Wayland resize 在 T-0306 才接;留口子防回归)。
    cell_buffer_capacity: usize,
    /// T-0403 加: glyph 光栅化结果 atlas, R8Unorm 单通道 alpha mask 纹理
    /// (`ATLAS_W` × `ATLAS_H` = 2048×2048 = 4MiB GPU)。lazy init, 首次
    /// [`Self::draw_frame`] 建好后字典序 shelf-pack 累积。
    glyph_atlas: Option<GlyphAtlas>,
    glyph_pipeline: Option<wgpu::RenderPipeline>,
    glyph_vertex_buffer: Option<wgpu::Buffer>,
    /// 当前 glyph vertex buffer 容量 (顶点数计)。Phase 4 单帧字符上限粗估
    /// (24 行 × 80 col × 6 顶点 = 11520 vert), 首次分配后 buffer reuse。
    glyph_buffer_capacity: usize,
    device: wgpu::Device,
    queue: wgpu::Queue,
    config: wgpu::SurfaceConfiguration,
    clear: wgpu::Color,
    /// surface 是否 sRGB 格式。决定 vertex 颜色是否要 sRGB→linear 预补偿
    /// (sRGB surface 把写入值当 linear,GPU 会再编码回 sRGB 显示)。
    /// 与 `clear` 字段同源,但 `clear` 是预算好的常量、`color_for_vertex`
    /// 是每 vertex 调一次的 hot path,所以拆开存。
    surface_is_srgb: bool,
    // 持有 Instance 避免提前 drop 掉 Vulkan/GL 实例。
    #[allow(dead_code)]
    instance: wgpu::Instance,
}

/// glyph alpha-mask atlas (Phase 4, T-0403)。
///
/// **类型隔离 (INV-010 strict reading 第 9 次)**: `wgpu::Texture` /
/// `wgpu::TextureView` / `wgpu::Sampler` / `wgpu::BindGroup` / `wgpu::BindGroupLayout`
/// 严格锁本 struct (模块私有, 不出 src/wl/render.rs)。公共 API 暴露的
/// [`Renderer::draw_frame`] 只接 quill 自定义类型 (`ShapedGlyph` / `CellPos`/
/// `Color` / `&mut TextSystem`), 无 wgpu 类型外溢。
///
/// **shelf packing 算法**: 维护 `(cursor_x, cursor_y, row_height)` 三个 u32 状态。
/// 每来一个 glyph: 若 `cursor_x + width > ATLAS_W` 则换行
/// (`cursor_y += row_height; cursor_x = 0; row_height = 0`); 若新行 `cursor_y +
/// height > ATLAS_H` 则 **T-0406 clear-on-full** — 清 allocations + reset cursor,
/// 当前 glyph 重新走 shelf 分配 (atlas 远大于单 glyph 必装得下)。Phase 4 atlas
/// 容量足够 (2048² / 16×24 ≈ 10000 字符, 远超 ASCII + 常用 CJK), clear 触发条件
/// 罕见; 触发时 1 帧 hiccup 重 raster 当帧可见字, 用户基本看不见。
///
/// **why clear-on-full 不是真 LRU** (派单 KISS): 真 LRU 需 per-slot last_use
/// timestamp + slab allocator + free-list, 跟当前 shelf packing 不兼容; 终端字符
/// 集稳定 (~ASCII 95 + 常用 CJK 几千), atlas 满几乎不发生, clear 等价单帧 cache
/// reset。ROADMAP "T-0406 LRU" 命名沿用历史。
///
/// **R8Unorm 选择**: cosmic-text Mask 内容是 8-bit alpha 单通道, R8Unorm
/// 自然映射, 4MiB GPU 内存 (2048×2048×1 byte) RTX 5090 32GB 完全无压力。
/// 彩色 emoji (Color content) 在 `TextSystem::rasterize` 直接返 None 不上 atlas
/// (派单 Out 段 "subpixel anti-aliasing / 彩色 Phase 5+"); R8Unorm + alpha
/// mask + fg color tint 是 Phase 4 视觉 milestone 足够。
struct GlyphAtlas {
    // 字段 drop 顺序 (Rust 正向): bind_group → bind_group_layout → view → sampler →
    // texture。bind_group 持 view + sampler 内部 Arc, 先 drop 释放 view/sampler 的
    // refcount; layout 不持外部资源, 顺序无关; view 持 texture 内部 Arc, 先 drop;
    // sampler 不持 texture; texture 最后 drop。allocations / cursor* / row_height
    // 是 POD 顺序无关。
    bind_group: wgpu::BindGroup,
    bind_group_layout: wgpu::BindGroupLayout,
    /// view + sampler 已被 bind_group 内部引用 (wgpu Arc), 但显式持一份保住资源
    /// 所有权 + drop 顺序明确。`#[allow(dead_code)]` 不是 placeholder, 是"显式持
    /// 资源不依赖 bind_group 内部 Arc 计数"的设计 — INV-002 字段顺序一目了然,
    /// 且未来 atlas 重建路径 (T-0406 LRU) 可直接换 view/sampler 不重建 bind_group。
    #[allow(dead_code)]
    view: wgpu::TextureView,
    #[allow(dead_code)]
    sampler: wgpu::Sampler,
    texture: wgpu::Texture,
    /// (T-0407) [`GlyphKey`] (face_id u64 + glyph_id u16 + font_size_quantized u32)
    /// → 已分配 atlas 槽位。HashMap 用 std::collections (派单硬约束: 不引 ahash /
    /// fxhash)。
    ///
    /// **T-0407 修 T-0403 P3 跟进 (audit P2-2)**: T-0403 实装 `(u16, u32)` 不含
    /// face_id 维度, 跨 face 同 glyph_id 撞 key 互相覆盖 — 升级 [`GlyphKey`] 三维
    /// struct, atlas slot 正确隔离每 face。
    allocations: HashMap<GlyphKey, AtlasSlot>,
    cursor_x: u32,
    cursor_y: u32,
    row_height: u32,
}

/// atlas 内单字形的 uv 槽位 + bearing。
///
/// **uv 已归一化** (`uv_min` / `uv_max` 是 [0, 1]^2 内 f32), fragment shader 直接
/// `textureSample(t, s, uv)` 拿 alpha 不需 size 换算。`width` / `height` 仍存
/// 是因为 vertex 计算需要 (NDC 矩形覆盖 bitmap 像素范围)。
///
/// **bearing_x / bearing_y**: 来自 [`RasterizedGlyph`] (透传 cosmic-text
/// `placement.left` / `placement.top`)。bearing_y 正值表示 baseline 之上,
/// 渲染层用 `cell_top_y + ascent_y - bearing_y` 算 bitmap top-left。
#[derive(Debug, Clone, Copy)]
struct AtlasSlot {
    uv_min: [f32; 2],
    uv_max: [f32; 2],
    width: u32,
    height: u32,
    bearing_x: i32,
    bearing_y: i32,
}

/// glyph atlas 纹理像素宽度 (= 高度 = 2048)。派单 In #B "2048×2048 R8Unorm
/// (单通道 alpha, 4MB GPU 内存)"。
const ATLAS_W: u32 = 2048;
/// glyph atlas 纹理像素高度。
const ATLAS_H: u32 = 2048;

/// 每 glyph vertex 字节数: `pos[2 f32] + uv[2 f32] + color[3 f32]` = 28 字节。
/// WGSL [`GLYPH_WGSL`] 端必须一致。每字形 6 顶点 (两三角形 CCW, 与 cell
/// pipeline 同 topology, 见 [`Renderer::draw_frame`] 内 vertex 生成)。
const GLYPH_VERTEX_BYTES: usize = 7 * std::mem::size_of::<f32>();

/// 默认基线像素位置 (从 cell 顶部往下数到 baseline 的距离)。
///
/// Phase 4 占位: cosmic-text Metrics(17, 25) 下字体 ascent ≈ 14-17 px (不同 face
/// 略异), baseline 大致 cell 顶部往下 18 px (留 7 px 给下沉字符 g/p/q/y 等)。
/// 实测可调; Phase 4 后续 ticket 可改为字体真实 metrics 测出。
///
/// 与 [`CELL_H_PX`] 联动: BASELINE_Y_PX < CELL_H_PX, 否则字形会被切顶。
const BASELINE_Y_PX: f32 = 18.0;

/// glyph WGSL shader (T-0403 内联, 沿袭 [`CELL_WGSL`] 风格)。
///
/// vertex: pass-through pos + uv + color
/// fragment: `textureSample(atlas, sampler, uv).r` 作 alpha mask, 与 fg color 相乘
/// 输出 `vec4(color, alpha)`。alpha blending 配 BlendState::ALPHA_BLENDING 让字形
/// 与 cell 色块叠加 (T-0305 cell pass 用 BlendState::REPLACE 不透明)。
///
/// **bind group 布局**:
/// - `@group(0) @binding(0)`: texture_2d<f32> (R8Unorm, sample_type Float
///   filterable=false, sampler_type NonFiltering 配 mag/min FilterMode::Nearest)
/// - `@group(0) @binding(1)`: sampler (NonFiltering)
const GLYPH_WGSL: &str = r#"
struct VsIn {
    @location(0) pos: vec2<f32>,
    @location(1) uv: vec2<f32>,
    @location(2) color: vec3<f32>,
};

struct VsOut {
    @builtin(position) clip: vec4<f32>,
    @location(0) uv: vec2<f32>,
    @location(1) color: vec3<f32>,
};

@group(0) @binding(0) var atlas_tex: texture_2d<f32>;
@group(0) @binding(1) var atlas_samp: sampler;

@vertex
fn vs_main(v: VsIn) -> VsOut {
    var out: VsOut;
    out.clip = vec4<f32>(v.pos, 0.0, 1.0);
    out.uv = v.uv;
    out.color = v.color;
    return out;
}

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    let alpha = textureSample(atlas_tex, atlas_samp, in.uv).r;
    return vec4<f32>(in.color, alpha);
}
"#;

/// 单 cell 6 顶点(两三角形,无 index buffer)。`vertices = cols * rows *
/// VERTS_PER_CELL`。80×24 = 11520 顶点,5090 GPU 完全无压力,instancing 优化
/// 留 Phase 6 soak 验证有需要再说。
const VERTS_PER_CELL: usize = 6;

/// (T-0407) cell 矩形染色源选择 — fg 还是 bg。
///
/// **why 引入**: T-0305 cell pass 只染 fg 色, T-0403 加 glyph pass 后字形也用 fg
/// 色 — 同色致 glyph alpha mask "涂同色等于不可见", 用户实测看到一片连续 fg 色
/// 矩形不见字 (T-0407 D 修)。draw_frame (Phase 4) 走 Bg 让字形可见, draw_cells
/// (Phase 3 fallback, text_system 未建好时降级) 走 Fg 维持原色块视觉契约。
///
/// T-0305 doc 早就预言此路径: "Phase 4 字形渲染时 fg 切回 glyph 色 + bg 画 cell
/// 全色块, API 已就位"。本 enum 把当时遗留的 fixup 1 行落地。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CellColorSource {
    Fg,
    Bg,
}

/// **cell 像素宽度** —— Phase 3 临时常数(T-0306)。Phase 4 字形渲染时改为
/// `cosmic-text` 测出的字体 advance 宽度替换。
///
/// 为啥 hardcode:T-0305 之前 `cell_w_px = surface_w / cols`,即 cells 拉伸填满
/// surface —— cols/rows 写死 80×24 时拖窗口 cells 跟着变大,但 grid 不能多显示。
/// T-0306 反过来:cell px 是常数,`cols = surface_w / CELL_W_PX`(window.rs
/// configure callback 算),拖窗口能显示更多 cells(bash 真能多显示行/列)。
///
/// 10×25 取自 GNOME 默认终端 monospace 字体的近似(DejaVu Sans Mono 11pt ≈
/// 10px advance × 22px ascent + 3px line gap)。Phase 4 字形测量后会被字体
/// 真实 metrics 替代,届时本常数删除,`Renderer` 持 `cell_w_px / cell_h_px: f32`
/// 字段动态更新(同时把 `crate::wl::window` configure callback 里的换算迁过来)。
pub const CELL_W_PX: f32 = 10.0;

/// **cell 像素高度** —— Phase 3 临时常数。配套见 [`CELL_W_PX`]。
pub const CELL_H_PX: f32 = 25.0;

/// **T-0505: preedit 下划线像素厚度** (logical px, render 内部 × HIDPI_SCALE).
///
/// 派单 In #D "底部 1-2 px 下划线 (跟主流 IME 风格)". 取 2 px logical →
/// 4 px physical (HIDPI ×2), 在 224 ppi 显示器上单像素细线偏窄, 2 px 是
/// fcitx5 / GTK 默认。
pub const PREEDIT_UNDERLINE_PX: u32 = 2;

/// T-0505: preedit overlay 入参。draw_frame / render_headless 收到 Some(_)
/// 时在 (cursor_col, cursor_line) cell 起点之后绘制 preedit 字 + 底部下划线。
/// None = 无 preedit (用户未组词 / IME disabled), 走标准 row_texts 路径。
///
/// **why pub struct 而非 pub fn 多参数**: 派单 In #D 描述 "preedit 渲染在
/// cursor 当前 cell 起点之后", 需要 text + cursor 位置一起传; 多个 i32/usize
/// 字段散在 fn 签名易混淆 (cursor_col vs cursor_line vs cursor_begin in
/// preedit), 抽 struct 让调用方一次传完整意图。
///
/// **类型隔离 (INV-010)**: 全字段 quill 自有类型 (String + usize), 不漏
/// wayland-protocols 类型。
#[derive(Debug, Clone)]
pub struct PreeditOverlay {
    /// 当前组词 UTF-8 字符串 (空字符串 = 无 preedit, 调用方应传 None 而非
    /// 空字符串 — 等价但 None 更清晰)。
    pub text: String,
    /// preedit 起绘点的 cell col (0-based)。通常是 term cursor col。
    pub cursor_col: usize,
    /// preedit 起绘点的 cell line (0-based, 不含 scrollback)。通常是 term
    /// cursor line。
    pub cursor_line: usize,
}

/// **HiDPI 整数缩放常数** (T-0404 简化版, hardcode 2x)。
///
/// **why hardcode 而非 wl_output.scale event**: 用户硬偏好 (派单 Out 段),
/// 单显示器 224 ppi 固定 2x 不变。多显示器 / 不同 ppi 切换是 Phase 5+ scope。
/// `wl_output.scale` 协议接入需要 wl_output Dispatch + per-output 状态机, 复杂度
/// 与本 ticket "字看清楚" 这条 acceptance 严重不匹配。
///
/// **数值含义**: surface backing 物理像素 = logical 像素 × HIDPI_SCALE。
/// - [`Renderer::resize`] / [`Renderer::new`] 接收 logical px (调用方
///   `cells_from_surface_px` 等仍用 logical), 内部 surface.configure 用
///   logical × HIDPI_SCALE
/// - shape / rasterize 走 `font_size × HIDPI_SCALE` (logical 17pt → physical 34pt),
///   bitmap 也按物理像素出, 上 atlas 后渲染清晰
/// - 顶点 NDC 计算用 `cell_w_px × HIDPI_SCALE` / `cell_h_px × HIDPI_SCALE`
///   (NDC 公式 `pos_px / surface_w × 2 - 1` 中 surface_w 已是 physical, cell px
///   也必须是 physical 才对齐)
///
/// **224 ppi 单显示器 1:1 显示**: Wayland compositor (mutter) 在 HiDPI 输出上
/// 把 client surface 的物理像素直接映射屏幕物理 px (1:1 不缩放), 字形清晰。
/// 96 ppi 屏幕上字会过大 — 派单 Out 段允许 (用户单一显示器场景)。
///
/// 测试覆盖: [`tests::hidpi_scale_is_2`] (常数 lock) +
/// [`crate::text::tests::rasterize_at_2x_font_size_doubles_bitmap_width`]
/// (raster 真翻倍验证)。
pub const HIDPI_SCALE: u32 = 2;

/// 每顶点字节数:`pos[2 f32] + color[3 f32]` = 20 字节。手算固化,WGSL 端
/// 与本常量必须一致(见 [`CELL_WGSL`])。
const VERTEX_BYTES: usize = 5 * std::mem::size_of::<f32>();

/// WGSL shader 内联(派单 "WGSL 内联在 render.rs,跟现有 clear pass 风格一致,
/// 别拆文件")。两个 stage:
/// - vertex: pass-through pos + color
/// - fragment: 输出 vec4(color, 1.0) 不透明
///
/// 颜色已在 CPU 侧做完 sRGB→linear 预补偿(`color_for_vertex`),WGSL 不再处理
/// gamma —— sRGB surface 会在 GPU 端把 linear 编码回 sRGB 显示,与
/// [`clear_color_for`] 的预补偿同套路。
const CELL_WGSL: &str = r#"
struct VsIn {
    @location(0) pos: vec2<f32>,
    @location(1) color: vec3<f32>,
};

struct VsOut {
    @builtin(position) clip: vec4<f32>,
    @location(0) color: vec3<f32>,
};

@vertex
fn vs_main(v: VsIn) -> VsOut {
    var out: VsOut;
    out.clip = vec4<f32>(v.pos, 0.0, 1.0);
    out.color = v.color;
    return out;
}

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    return vec4<f32>(in.color, 1.0);
}
"#;

impl Renderer {
    /// 从 Wayland 裸指针构造 Renderer,配置初始尺寸并返回。
    ///
    /// # Safety
    /// - `display_ptr` 必须是当前进程中一个活跃 libwayland `wl_display` 的合法指针
    ///   (通常来自 `Connection::backend().display_ptr()`,前提是 `wayland-backend`
    ///   启用了 `client_system` feature,使 wayland-client 走 libwayland 后端)。
    /// - `surface_ptr` 必须是属于同一 `wl_display` 的合法 `wl_surface` 指针。
    /// - 调用方必须保证这两个对象的生命周期至少与返回的 Renderer 一样长 —— 即
    ///   不要在 Renderer 仍在使用时销毁 `wl_surface` 或关闭连接。
    #[allow(unsafe_code)]
    pub unsafe fn new(
        display_ptr: *mut c_void,
        surface_ptr: *mut c_void,
        width: u32,
        height: u32,
    ) -> Result<Self> {
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());

        let display_nn =
            NonNull::new(display_ptr).ok_or_else(|| anyhow!("wl_display 指针为 null"))?;
        let surface_nn =
            NonNull::new(surface_ptr).ok_or_else(|| anyhow!("wl_surface 指针为 null"))?;
        let raw_display = RawDisplayHandle::Wayland(WaylandDisplayHandle::new(display_nn));
        let raw_window = RawWindowHandle::Wayland(WaylandWindowHandle::new(surface_nn));

        // SAFETY: display_ptr / surface_ptr 已按 Renderer::new 的 # Safety 条款约束,
        // 调用方保证裸指针有效且生命周期覆盖 Renderer。wgpu 只保留指针副本,不会
        // 尝试 drop wl 对象。
        #[allow(unsafe_code)]
        let surface = unsafe {
            instance.create_surface_unsafe(wgpu::SurfaceTargetUnsafe::RawHandle {
                raw_display_handle: Some(raw_display),
                raw_window_handle: raw_window,
            })
        }
        .context("wgpu create_surface_unsafe 失败")?;

        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::default(),
            force_fallback_adapter: false,
            compatible_surface: Some(&surface),
        }))
        .map_err(|e| anyhow!("wgpu request_adapter 失败: {e}"))?;

        let info = adapter.get_info();
        tracing::info!(backend = ?info.backend, name = %info.name, "wgpu adapter 选中");

        // T-0409 hotfix: downlevel_defaults max_texture_dimension_2d=2048 在
        // HiDPI ×2 + compositor resize (e.g. 1600×1200 logical → 3200×2400
        // physical) 下触发 Surface::configure validation panic. 用
        // adapter.limits() 取实际硬件上限 (Vulkan 5090 = 16384+).
        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("quill-device"),
            required_features: wgpu::Features::empty(),
            required_limits: adapter.limits(),
            experimental_features: wgpu::ExperimentalFeatures::default(),
            memory_hints: wgpu::MemoryHints::default(),
            trace: wgpu::Trace::Off,
        }))
        .context("wgpu request_device 失败")?;

        let caps = surface.get_capabilities(&adapter);
        let format = caps
            .formats
            .iter()
            .copied()
            .find(|f| f.is_srgb())
            .or_else(|| caps.formats.first().copied())
            .ok_or_else(|| anyhow!("surface 无可用像素格式"))?;
        let alpha_mode = caps
            .alpha_modes
            .first()
            .copied()
            .ok_or_else(|| anyhow!("surface 无可用 alpha mode"))?;

        // T-0404: surface backing 像素 = logical × HIDPI_SCALE。Renderer 内部
        // self.config.width / height 始终是 physical px, NDC 换算 / cell 像素都
        // 走 physical (与 [`Self::resize`] 同语义)。`cells_from_surface_px` 在
        // window.rs 用 logical px 算 cols/rows, 不经过本配置。
        let physical_w = width.max(1).saturating_mul(HIDPI_SCALE);
        let physical_h = height.max(1).saturating_mul(HIDPI_SCALE);
        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format,
            width: physical_w,
            height: physical_h,
            present_mode: wgpu::PresentMode::Fifo,
            desired_maximum_frame_latency: 2,
            alpha_mode,
            view_formats: vec![],
        };
        surface.configure(&device, &config);

        let clear = clear_color_for(format);
        let surface_is_srgb = format.is_srgb();
        tracing::debug!(
            ?format,
            width = config.width,
            height = config.height,
            srgb = surface_is_srgb,
            "wgpu surface configured"
        );

        Ok(Self {
            surface,
            cell_pipeline: None,
            cell_vertex_buffer: None,
            cell_buffer_capacity: 0,
            // T-0403: glyph 三件套 lazy init, 首次 draw_frame 建好。INV-002 字段
            // 顺序: 放在 cell_buffer_capacity 之后, device 之前。
            glyph_atlas: None,
            glyph_pipeline: None,
            glyph_vertex_buffer: None,
            glyph_buffer_capacity: 0,
            device,
            queue,
            config,
            clear,
            surface_is_srgb,
            instance,
        })
    }

    /// Wayland configure 后把新 surface 像素尺寸推给 wgpu —— 更新 `self.config`
    /// 并 `surface.configure`。T-0306 接通 wayland resize 链路时由
    /// [`crate::wl::window`] 在 `core.resize_dirty` 被置后调用,与
    /// `term.resize` / `pty.resize` 同步。
    ///
    /// **幂等**:width/height 与当前 config 完全一致时跳过 surface.configure
    /// (避免无谓重建 swapchain)。`width == 0 || height == 0` 也跳过(SurfaceConfiguration
    /// 不接受 0,wgpu 内部 panic)—— 与 [`Self::new`] 的 `.max(1)` 同思路,但
    /// 这里直接 return 让调用方知道"什么都没做",而非静默 clamp 到 1×1
    /// (1×1 surface 几乎无用,跳过更老实)。
    ///
    /// **draw_cells 的 NDC 换算**(`x / surface_w * 2 - 1`)读 `self.config.width
    /// / height`,所以 resize 后下一次 draw_cells 自动用新尺寸,不需要额外通知。
    ///
    /// **T-0404 HiDPI**: 接收的 `width / height` 是 **logical** 像素
    /// (Wayland compositor configure 给的尺寸, `WindowCore` 也存 logical),
    /// surface.configure 实际用 `width × HIDPI_SCALE / height × HIDPI_SCALE`
    /// (physical 像素 = backing buffer)。Wayland compositor (mutter) 在 HiDPI
    /// 输出上把 surface 物理像素 1:1 映射到屏幕, 字形清晰。
    pub fn resize(&mut self, width: u32, height: u32) {
        if width == 0 || height == 0 {
            return;
        }
        // T-0404: physical = logical × HIDPI_SCALE; saturating_mul 防 overflow
        // (虽然 logical px 实际不会溢出, defense-in-depth)。
        let physical_w = width.saturating_mul(HIDPI_SCALE);
        let physical_h = height.saturating_mul(HIDPI_SCALE);
        if physical_w == self.config.width && physical_h == self.config.height {
            return;
        }
        self.config.width = physical_w;
        self.config.height = physical_h;
        self.surface.configure(&self.device, &self.config);
        tracing::debug!(
            logical_w = width,
            logical_h = height,
            physical_w,
            physical_h,
            "wgpu surface reconfigured (HIDPI scaled)"
        );
    }

    /// 拿到下一帧 texture,清屏,present。Acquire 失败(Outdated / Lost /
    /// Timeout / Occluded)作为非致命回到 `Ok(())`,让上层跳过这帧 —— resize
    /// 期间很常见。Validation 这种上游异常按错误抛。
    pub fn render(&mut self) -> Result<()> {
        let frame = match self.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(t) => t,
            wgpu::CurrentSurfaceTexture::Suboptimal(t) => {
                // Suboptimal 意味着 surface props 变了(比如尺寸),T-0103 再处理重配置;
                // 此时 texture 仍可用,照常 present。
                t
            }
            wgpu::CurrentSurfaceTexture::Outdated | wgpu::CurrentSurfaceTexture::Lost => {
                // Outdated / Lost: compositor 告知 surface 不再匹配或需重建。T-0103
                // 会按新尺寸重配置;本 ticket 先保守地按当前 config 重配一遍,跳过该帧。
                self.surface.configure(&self.device, &self.config);
                return Ok(());
            }
            wgpu::CurrentSurfaceTexture::Timeout => {
                tracing::warn!("surface acquire timeout, 跳过此帧");
                return Ok(());
            }
            wgpu::CurrentSurfaceTexture::Occluded => {
                // 被其他窗口完全遮挡,不渲染更省电。下一次 configure 会自动恢复。
                return Ok(());
            }
            wgpu::CurrentSurfaceTexture::Validation => {
                return Err(anyhow!("wgpu surface acquire 报 Validation 错误"));
            }
        };

        let view = frame
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("quill-frame-encoder"),
            });
        {
            let _pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("quill-clear-pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    resolve_target: None,
                    depth_slice: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(self.clear),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
        }
        self.queue.submit(std::iter::once(encoder.finish()));
        frame.present();
        Ok(())
    }

    /// 一帧色块渲染:在 clear pass 之上画每 cell(`c != ' '`)的 fg 色矩形。
    ///
    /// 流程:
    /// 1. lazy 初始化 `cell_pipeline` + `cell_vertex_buffer`(派单硬约束:
    ///    pipeline / layout / bind group 创建一次复用)
    /// 2. 算每 cell pixel size,生成顶点(`pos[2 f32] + color[3 f32]`)
    /// 3. acquire frame texture
    /// 4. 单 RenderPass:`LoadOp::Clear(深蓝)` + `set_pipeline` + `set_vertex_buffer`
    ///    + `draw(0..vertex_count)` —— clear 与 cell 同一 pass,不分两 encoder
    /// 5. submit + present
    ///
    /// **fg 着色而非 bg**(派单 scope 提到 "bg color 作为 vertex attribute",
    /// 但派单 Goal 要求"看见 bash prompt 的字符位置以色块画出"。bash prompt
    /// 默认 bg=Background 解析到黑,在深蓝清屏上画黑块 visually 几乎不可见。
    /// fg=Foreground 解析到 light gray (#d3d3d3),反差清晰。Phase 4 字形渲染
    /// 时 fg 切回 glyph 色 + bg 画 cell 全色块,API 已就位。本 ticket 视觉
    /// acceptance 优先,deviation 在此明示给审码,fixup 1 行可改 WGSL 切 bg)。
    ///
    /// `c == ' '` 的 cell 不上传顶点(稀疏渲染,80×24 满屏空白时 vertex_count = 0)。
    /// 这样深蓝清屏在空 cell 处显露,有字符的位置才出现 fg 色块,符合 acceptance
    /// "深蓝背景上离散色块"。
    ///
    /// 错误处理:Surface acquire 的 Outdated/Lost/Timeout/Occluded 与
    /// [`Self::render`] 同档,跳过该帧。Validation 上抛。
    pub fn draw_cells(&mut self, cells: &[CellRef], cols: usize, rows: usize) -> Result<()> {
        // 防御:cols/rows == 0 时无 cell 可画,直接 clear 一帧返回(对齐
        // `Self::render` 的清屏行为,不让上层崩)。
        if cols == 0 || rows == 0 {
            return self.render();
        }

        // Step 1: lazy init pipeline + vertex buffer(派单硬约束)。
        self.ensure_cell_pipeline();
        self.ensure_cell_buffer(cols, rows);

        // Step 2: cell pixel size 走常数 [`CELL_W_PX`] / [`CELL_H_PX`](T-0306
        // 改:Phase 3 用 hardcode,Phase 4 字形测量后换字体 metrics)。surface_w /
        // surface_h 仍是 NDC 换算的分母(像素 → clip space),不能省。
        // 调用方(window.rs configure callback)按 `cols = surface_w / CELL_W_PX`
        // 算 cols 推给 term.resize,cells 数量与 cell px 自然匹配 surface,
        // 余下边距(surface_w % CELL_W_PX)Phase 4 再细化(派单允许)。
        //
        // T-0404 HiDPI: self.config.width 是 physical px (Renderer::resize 已乘
        // HIDPI_SCALE), 所以 cell px 在 NDC 换算里也必须是 physical (× HIDPI_SCALE)
        // 才与 surface_w 单位一致 — 否则 80×24 cells 只占 surface 一半 (logical px
        // 数对上 physical px 分母)。cols/rows 由 window.rs cells_from_surface_px
        // 用 logical px 算, 与本处 physical 解耦 (logical_w / CELL_W_PX ==
        // physical_w / (CELL_W_PX × HIDPI_SCALE), 两侧自洽)。
        let surface_w = self.config.width.max(1) as f32;
        let surface_h = self.config.height.max(1) as f32;
        let cell_w_px = CELL_W_PX * HIDPI_SCALE as f32;
        let cell_h_px = CELL_H_PX * HIDPI_SCALE as f32;

        let vertex_bytes = self.build_vertex_bytes(
            cells,
            cell_w_px,
            cell_h_px,
            surface_w,
            surface_h,
            // Phase 3 fallback 路径 (text_system 未建好时降级): cell 染 fg 色作
            // 视觉锚点 (T-0305 acceptance "看见 prompt 字符位置以色块画出")。
            CellColorSource::Fg,
        );
        let vertex_count = (vertex_bytes.len() / VERTEX_BYTES) as u32;
        // 给手测 / 日志排障一个固定锚点:每次 draw_cells 报本帧 cell 矩形数。
        // debug 级,不污染默认 info 日志。空白 frame (vertex_count == 0) 也报,
        // 便于"为啥屏幕全清屏色"这种问题溯源。
        tracing::debug!(
            target: "quill::wl::render",
            cols,
            rows,
            cell_w_px,
            cell_h_px,
            vertex_count,
            "draw_cells frame"
        );

        // 上传到 GPU。queue.write_buffer 是 staging-free 的快路径,适合每帧
        // 写小量数据(80×24 满屏 = 11520 顶点 × 20 字节 = 230 KiB,5090 PCIe5
        // 带宽下零开销)。
        if vertex_count > 0 {
            let buf = self.cell_vertex_buffer.as_ref().ok_or_else(|| {
                anyhow!("cell_vertex_buffer 应已 lazy 初始化(ensure_cell_buffer 后)")
            })?;
            self.queue.write_buffer(buf, 0, &vertex_bytes);
        }

        // Step 3: acquire frame —— 与 `render` 同一套错误分类。
        let frame = match self.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(t) => t,
            wgpu::CurrentSurfaceTexture::Suboptimal(t) => t,
            wgpu::CurrentSurfaceTexture::Outdated | wgpu::CurrentSurfaceTexture::Lost => {
                self.surface.configure(&self.device, &self.config);
                return Ok(());
            }
            wgpu::CurrentSurfaceTexture::Timeout => {
                tracing::warn!("surface acquire timeout, 跳过 draw_cells 这帧");
                return Ok(());
            }
            wgpu::CurrentSurfaceTexture::Occluded => return Ok(()),
            wgpu::CurrentSurfaceTexture::Validation => {
                return Err(anyhow!("wgpu surface acquire 报 Validation 错误"));
            }
        };

        let view = frame
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("quill-cells-encoder"),
            });

        // Step 4: 单 RenderPass(clear + cells)。pipeline + vertex buffer 都在
        // self,引用即可;若 vertex_count == 0(全空白 cell)就只 clear。
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("quill-cells-pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    resolve_target: None,
                    depth_slice: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(self.clear),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });

            if vertex_count > 0 {
                let pipeline = self
                    .cell_pipeline
                    .as_ref()
                    .ok_or_else(|| anyhow!("cell_pipeline 应已 lazy 初始化"))?;
                let buf = self
                    .cell_vertex_buffer
                    .as_ref()
                    .ok_or_else(|| anyhow!("cell_vertex_buffer 应已 lazy 初始化"))?;
                pass.set_pipeline(pipeline);
                pass.set_vertex_buffer(0, buf.slice(..));
                pass.draw(0..vertex_count, 0..1);
            }
        }

        self.queue.submit(std::iter::once(encoder.finish()));
        frame.present();
        Ok(())
    }

    /// lazy 初始化 cell render pipeline。WGSL 内联 [`CELL_WGSL`],vertex layout
    /// 与 [`VERTEX_BYTES`] 对齐 —— 顶点结构 `pos: vec2<f32> + color: vec3<f32>`,
    /// stride 20 字节。
    ///
    /// 不取 bind group(无 uniform / texture 用,Phase 3 全靠 vertex attr 传)。
    /// 若已建好,no-op。
    fn ensure_cell_pipeline(&mut self) {
        if self.cell_pipeline.is_some() {
            return;
        }
        let shader = self
            .device
            .create_shader_module(wgpu::ShaderModuleDescriptor {
                label: Some("quill-cells-shader"),
                source: wgpu::ShaderSource::Wgsl(CELL_WGSL.into()),
            });
        let layout = self
            .device
            .create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some("quill-cells-pipeline-layout"),
                bind_group_layouts: &[],
                immediate_size: 0,
            });
        let vertex_attrs = [
            wgpu::VertexAttribute {
                offset: 0,
                shader_location: 0,
                format: wgpu::VertexFormat::Float32x2,
            },
            wgpu::VertexAttribute {
                offset: (2 * std::mem::size_of::<f32>()) as u64,
                shader_location: 1,
                format: wgpu::VertexFormat::Float32x3,
            },
        ];
        let pipeline = self
            .device
            .create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some("quill-cells-pipeline"),
                layout: Some(&layout),
                vertex: wgpu::VertexState {
                    module: &shader,
                    entry_point: Some("vs_main"),
                    compilation_options: wgpu::PipelineCompilationOptions::default(),
                    buffers: &[wgpu::VertexBufferLayout {
                        array_stride: VERTEX_BYTES as u64,
                        step_mode: wgpu::VertexStepMode::Vertex,
                        attributes: &vertex_attrs,
                    }],
                },
                fragment: Some(wgpu::FragmentState {
                    module: &shader,
                    entry_point: Some("fs_main"),
                    compilation_options: wgpu::PipelineCompilationOptions::default(),
                    targets: &[Some(wgpu::ColorTargetState {
                        format: self.config.format,
                        blend: Some(wgpu::BlendState::REPLACE),
                        write_mask: wgpu::ColorWrites::ALL,
                    })],
                }),
                primitive: wgpu::PrimitiveState {
                    topology: wgpu::PrimitiveTopology::TriangleList,
                    strip_index_format: None,
                    front_face: wgpu::FrontFace::Ccw,
                    cull_mode: None,
                    unclipped_depth: false,
                    polygon_mode: wgpu::PolygonMode::Fill,
                    conservative: false,
                },
                depth_stencil: None,
                multisample: wgpu::MultisampleState::default(),
                multiview_mask: None,
                cache: None,
            });
        self.cell_pipeline = Some(pipeline);
    }

    /// lazy 初始化 / 增长 cell vertex buffer。容量按当前 cols×rows 算,若已够
    /// (覆盖了 Phase 3 写死 80×24 的常见场景),no-op;否则销毁旧 buffer + 建新的。
    /// T-0306 Wayland resize 后若 cols×rows 变化,这条 if 自动重建。
    fn ensure_cell_buffer(&mut self, cols: usize, rows: usize) {
        let needed = cols * rows * VERTS_PER_CELL;
        if self.cell_buffer_capacity >= needed && self.cell_vertex_buffer.is_some() {
            return;
        }
        let size_bytes = (needed * VERTEX_BYTES) as u64;
        let buf = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("quill-cells-vertex-buffer"),
            size: size_bytes,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        self.cell_vertex_buffer = Some(buf);
        self.cell_buffer_capacity = needed;
    }

    /// 把 `cells` 转成 vertex bytes。空格 cell 跳过(稀疏渲染,见 [`Self::draw_cells`]
    /// fg/bg 决策注释)。
    ///
    /// 顶点布局(每 cell 6 顶点,两三角形,CCW):
    /// ```text
    ///  TL ─── TR        前三角:TL → BL → BR
    ///   │ ╲   │        后三角:TL → BR → TR
    ///   │  ╲  │        (Front face = CCW,见 pipeline primitive 配置)
    ///  BL ─── BR
    /// ```
    ///
    /// NDC 坐标系: x ∈ [-1, 1] 左→右,y ∈ [-1, 1] 下→上(像素 y=0 在顶,
    /// NDC y 翻转一次)。
    fn build_vertex_bytes(
        &self,
        cells: &[CellRef],
        cell_w_px: f32,
        cell_h_px: f32,
        surface_w: f32,
        surface_h: f32,
        color_source: CellColorSource,
    ) -> Vec<u8> {
        let mut out: Vec<u8> = Vec::with_capacity(cells.len() * VERTS_PER_CELL * VERTEX_BYTES);
        for cell in cells {
            // 稀疏渲染:空白 cell 不贡献顶点,深蓝清屏在该位置显露。
            if cell.c == ' ' {
                continue;
            }
            let x0_px = cell.pos.col as f32 * cell_w_px;
            let y0_px = cell.pos.line as f32 * cell_h_px;
            let x1_px = x0_px + cell_w_px;
            let y1_px = y0_px + cell_h_px;

            let left = x0_px / surface_w * 2.0 - 1.0;
            let right = x1_px / surface_w * 2.0 - 1.0;
            // y 翻转:像素 y=0 (top) → NDC y=+1
            let top = 1.0 - y0_px / surface_h * 2.0;
            let bottom = 1.0 - y1_px / surface_h * 2.0;

            // T-0407 D fix: cell pass 走 fg 还是 bg 决定 visual 与 glyph 共存:
            // - Fg (Phase 3 draw_cells fallback): 没字形, 单纯色块给 acceptance 看
            // - Bg (Phase 4 draw_frame): 字形覆盖在 cell 之上, 必须 cell 用 bg 色,
            //   glyph 用 fg 色, 否则同色 (T-0403 bug 真因) — 字 alpha mask 在 fg
            //   色块上"涂同色"等于不可见, 用户实测看到一片连续 fg 矩形不见字。
            //   T-0305 doc 早就预言此路径 ("Phase 4 字形渲染时 fg 切回 glyph 色 +
            //   bg 画 cell 全色块")。
            let color = match color_source {
                CellColorSource::Fg => self.color_for_vertex(cell.fg),
                CellColorSource::Bg => self.color_for_vertex(cell.bg),
            };

            // CCW 三角顺序: TL → BL → BR(下三角), TL → BR → TR(上三角)
            let verts: [[f32; 2]; 6] = [
                [left, top],
                [left, bottom],
                [right, bottom],
                [left, top],
                [right, bottom],
                [right, top],
            ];
            for v in verts {
                out.extend_from_slice(&v[0].to_ne_bytes());
                out.extend_from_slice(&v[1].to_ne_bytes());
                out.extend_from_slice(&color[0].to_ne_bytes());
                out.extend_from_slice(&color[1].to_ne_bytes());
                out.extend_from_slice(&color[2].to_ne_bytes());
            }
        }
        out
    }

    /// quill `Color`(sRGB 字节)→ shader 输入(linear f32)。sRGB surface 时
    /// 走 [`srgb_to_linear`] 预补偿,GPU 会再编码回 sRGB 显示;非 sRGB 时
    /// 直接 `byte / 255.0`。与 [`clear_color_for`] 同套路。
    fn color_for_vertex(&self, c: crate::term::Color) -> [f32; 3] {
        let r = f64::from(c.r) / 255.0;
        let g = f64::from(c.g) / 255.0;
        let b = f64::from(c.b) / 255.0;
        if self.surface_is_srgb {
            [
                srgb_to_linear(r) as f32,
                srgb_to_linear(g) as f32,
                srgb_to_linear(b) as f32,
            ]
        } else {
            [r as f32, g as f32, b as f32]
        }
    }

    /// **Phase 4 视觉里程碑入口** (T-0403): 单 RenderPass 内 clear → cells →
    /// glyphs → present, 一帧把字真画到屏幕。
    ///
    /// 调用方 ([`crate::wl::window`] idle callback) 准备:
    /// - `cells`: term grid (T-0305 既有)
    /// - `cols / rows`: grid 尺寸 (T-0306)
    /// - `text_system`: cosmic-text 字体子系统 (T-0401), 由 LoopData 持
    /// - `row_texts`: viewport 每行的纯字符 (`String`, 长度 `rows`)
    ///
    /// **派单 vs 实装偏离主动告知** (审码 重点 review 项, 沿袭 T-0305 fg vs bg
    /// 与 T-0306 Renderer::resize 模式)。派单 In #C 写"idle callback 在 draw_cells
    /// **之后**调 draw_glyphs", 但 wgpu Surface 的 `get_current_texture` 与
    /// `frame.present` 必须在同一帧成对调 — 字面 spec 实施需要两次 acquire/present,
    /// 视觉上撕裂 (cells 帧 N 与 glyphs 帧 N+1, 用户拖窗口快时观察到字飘忽)。
    /// **改为 `draw_frame` 单一入口同帧 clear+cells+glyphs**, Goal-driven
    /// correction (派单 Goal "屏幕显示真字符" 必须 cells 与 glyphs 同帧 present)。
    /// `draw_cells` 保留作 Phase 3 fallback 路径 (renderer init 失败 / text_system
    /// 未建好时的降级渲染), idle callback 默认走 `draw_frame`。
    ///
    /// **流程**:
    /// 1. 防御 (cols/rows == 0 退化到 [`Self::render`] 清屏)
    /// 2. lazy init 全部 GPU 资源 (cell pipeline/buffer + glyph atlas/pipeline/buffer)
    /// 3. 算 cell vertex bytes (复用 [`Self::build_vertex_bytes`])
    /// 4. shape 每行 + raster 每 glyph + atlas allocate, 算 glyph vertex bytes
    /// 5. queue.write_buffer 上传 cell + glyph vertex 数据
    /// 6. 单 RenderPass: LoadOp::Clear + cells draw + glyphs draw
    /// 7. submit + present
    pub fn draw_frame(
        &mut self,
        text_system: &mut TextSystem,
        cells: &[CellRef],
        cols: usize,
        rows: usize,
        row_texts: &[String],
        preedit: Option<&PreeditOverlay>,
    ) -> Result<()> {
        if cols == 0 || rows == 0 {
            return self.render();
        }

        // Step 1: lazy init 全部 GPU 资源。
        self.ensure_cell_pipeline();
        self.ensure_cell_buffer(cols, rows);
        self.ensure_glyph_atlas();
        self.ensure_glyph_pipeline();

        // Step 2: cell pixel size (与 draw_cells 同源)。
        // T-0404: physical px (× HIDPI_SCALE), 见 draw_cells 同段注释解释 cell
        // px 与 surface_w 单位必须同 (physical) 的理由。
        let surface_w = self.config.width.max(1) as f32;
        let surface_h = self.config.height.max(1) as f32;
        let cell_w_px = CELL_W_PX * HIDPI_SCALE as f32;
        let cell_h_px = CELL_H_PX * HIDPI_SCALE as f32;
        let baseline_y_px = BASELINE_Y_PX * HIDPI_SCALE as f32;

        // Step 3: cell vertex bytes (T-0407 D fix: 走 bg 色, 让 glyph fg 字形
        // 在 cell bg 块上可见; T-0403 用 fg 致字形被 cell fg 块"涂同色"不可见,
        // 用户实测看到一片连续 fg 色矩形不见字)。
        let mut cell_vertex_bytes = self.build_vertex_bytes(
            cells,
            cell_w_px,
            cell_h_px,
            surface_w,
            surface_h,
            CellColorSource::Bg,
        );
        // cell_vertex_count 在下面的 preedit underline append 后再算 (T-0505)。

        // Step 4: shape + raster + atlas allocate + build glyph vertex bytes。
        // 错位检查: row_texts 长度应等于 rows; 若上层传入截短的 row_texts (例
        // term 内部 dimensions() 与 row_texts 临时不同步), 取 min 防越界。
        let effective_rows = row_texts.len().min(rows);
        // 期望 fg 色: Phase 4 用 quill 默认 fg (#d3d3d3 light gray, term::Color
        // ::DEFAULT_FG, 但该常数模块私有, 此处内联值)。T-0405 后续会 per-glyph
        // 用 cell.fg (拿到对应 col / row 的 CellRef.fg), 本单先单一颜色, 视觉
        // milestone "看见字" 即可。
        let fg_default = crate::term::Color {
            r: 0xd3,
            g: 0xd3,
            b: 0xd3,
        };
        let glyph_color = self.color_for_vertex(fg_default);

        let mut glyph_vertex_bytes: Vec<u8> = Vec::new();
        for (row_idx, row_text) in row_texts.iter().take(effective_rows).enumerate() {
            if row_text.is_empty() {
                continue;
            }
            let glyphs = text_system.shape_line(row_text);
            for glyph in &glyphs {
                // 跳过零 advance / 零位置 (异常或 control char)
                if !glyph.x_advance.is_finite() || !glyph.x_offset.is_finite() {
                    continue;
                }
                // atlas allocate (lazy raster on cache miss)
                let slot = match self.allocate_glyph_slot(text_system, glyph) {
                    Some(s) => s,
                    // None 路径: rasterize 返 None (Color content emoji / 缺字形)
                    // 或 atlas 失败。当前实装跳过, 屏幕上该字形不显示 (Phase 4 接受,
                    // ASCII / 常用 CJK 路径不会触发)。
                    None => continue,
                };
                // 零尺寸 slot (空格 / 零宽字符): 跳过 vertex 生成
                if slot.width == 0 || slot.height == 0 {
                    continue;
                }

                // 世界坐标 (像素): cell 顶部 + baseline_y - bearing_y, cell 左 +
                // bearing_x。x_offset 是 line-relative 累积位置 (cosmic-text g.x),
                // 单行 shape 时等价于 col * cell_w_px (monospace 对齐), 直接用。
                //
                // T-0404: glyph.x_offset / bearing_x / bearing_y / slot.width /
                // slot.height 都已经是 physical px (shape_line 用 17 × HIDPI_SCALE
                // metrics, rasterize 出的 bitmap 也是 physical 尺寸), 与 cell_h_px /
                // baseline_y_px (× HIDPI_SCALE) 单位一致, 直接相加无需再乘。
                let x_left = glyph.x_offset + slot.bearing_x as f32;
                let y_top = (row_idx as f32) * cell_h_px + baseline_y_px - slot.bearing_y as f32;
                let x_right = x_left + slot.width as f32;
                let y_bot = y_top + slot.height as f32;

                // NDC (与 cell 同套路): x [-1,1] 左右, y [-1,1] 下上 (像素 y=0 顶
                // → NDC y=+1)。
                let ndc_left = x_left / surface_w * 2.0 - 1.0;
                let ndc_right = x_right / surface_w * 2.0 - 1.0;
                let ndc_top = 1.0 - y_top / surface_h * 2.0;
                let ndc_bot = 1.0 - y_bot / surface_h * 2.0;

                let uv_l = slot.uv_min[0];
                let uv_r = slot.uv_max[0];
                let uv_t = slot.uv_min[1];
                let uv_b = slot.uv_max[1];

                // CCW 同 cell pipeline: TL → BL → BR (下三角), TL → BR → TR (上三角)
                let verts: [([f32; 2], [f32; 2]); 6] = [
                    ([ndc_left, ndc_top], [uv_l, uv_t]),
                    ([ndc_left, ndc_bot], [uv_l, uv_b]),
                    ([ndc_right, ndc_bot], [uv_r, uv_b]),
                    ([ndc_left, ndc_top], [uv_l, uv_t]),
                    ([ndc_right, ndc_bot], [uv_r, uv_b]),
                    ([ndc_right, ndc_top], [uv_r, uv_t]),
                ];
                for (pos, uv) in verts {
                    glyph_vertex_bytes.extend_from_slice(&pos[0].to_ne_bytes());
                    glyph_vertex_bytes.extend_from_slice(&pos[1].to_ne_bytes());
                    glyph_vertex_bytes.extend_from_slice(&uv[0].to_ne_bytes());
                    glyph_vertex_bytes.extend_from_slice(&uv[1].to_ne_bytes());
                    glyph_vertex_bytes.extend_from_slice(&glyph_color[0].to_ne_bytes());
                    glyph_vertex_bytes.extend_from_slice(&glyph_color[1].to_ne_bytes());
                    glyph_vertex_bytes.extend_from_slice(&glyph_color[2].to_ne_bytes());
                }
            }
        }

        // T-0505: preedit overlay (派单 In #D). 在 cursor cell 起点之后绘制
        // preedit 字 + 底部下划线。preedit 字走 glyph pass (alpha-blended),
        // 下划线走 cell pass (REPLACE color rect, append 到 cell_vertex_bytes).
        // 颜色: preedit 文字浅灰 (cell.fg 默认值 #d3d3d3, 跟主流 IME 一致); 下
        // 划线略亮 #ffffff 让用户区分组词态 vs 已 commit 字。
        if let Some(p) = preedit {
            if !p.text.is_empty() {
                self.append_preedit_glyphs(
                    text_system,
                    &mut glyph_vertex_bytes,
                    &p.text,
                    p.cursor_col,
                    p.cursor_line,
                    cell_w_px,
                    cell_h_px,
                    baseline_y_px,
                    surface_w,
                    surface_h,
                    glyph_color,
                );
                let underline_color = self.color_for_vertex(crate::term::Color {
                    r: 0xff,
                    g: 0xff,
                    b: 0xff,
                });
                append_preedit_underline_to_cell_bytes(
                    &mut cell_vertex_bytes,
                    p.cursor_col,
                    p.cursor_line,
                    p.text.chars().count(),
                    cell_w_px,
                    cell_h_px,
                    surface_w,
                    surface_h,
                    underline_color,
                );
            }
        }
        // cell_vertex_count 重新算 (preedit underline 可能 append 了 cell 顶点)
        let cell_vertex_count = (cell_vertex_bytes.len() / VERTEX_BYTES) as u32;
        let glyph_vertex_count = (glyph_vertex_bytes.len() / GLYPH_VERTEX_BYTES) as u32;

        // 调试锚点 (debug 级, 不污染默认 info)
        tracing::debug!(
            target: "quill::wl::render",
            cols,
            rows,
            cell_vertex_count,
            glyph_vertex_count,
            atlas_count = self.glyph_atlas.as_ref().map(|a| a.allocations.len()).unwrap_or(0),
            "draw_frame stats"
        );

        // 上传 cell + glyph vertex 数据 (queue.write_buffer 是 staging-free 快路径)
        if cell_vertex_count > 0 {
            let buf = self.cell_vertex_buffer.as_ref().ok_or_else(|| {
                anyhow!("cell_vertex_buffer 应已 lazy 初始化(ensure_cell_buffer 后)")
            })?;
            self.queue.write_buffer(buf, 0, &cell_vertex_bytes);
        }
        if glyph_vertex_count > 0 {
            self.ensure_glyph_buffer(glyph_vertex_bytes.len());
            let buf = self.glyph_vertex_buffer.as_ref().ok_or_else(|| {
                anyhow!("glyph_vertex_buffer 应已 lazy 初始化(ensure_glyph_buffer 后)")
            })?;
            self.queue.write_buffer(buf, 0, &glyph_vertex_bytes);
        }

        // Step 5: acquire frame (与 draw_cells / render 同档错误分类)
        let frame = match self.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(t) => t,
            wgpu::CurrentSurfaceTexture::Suboptimal(t) => t,
            wgpu::CurrentSurfaceTexture::Outdated | wgpu::CurrentSurfaceTexture::Lost => {
                self.surface.configure(&self.device, &self.config);
                return Ok(());
            }
            wgpu::CurrentSurfaceTexture::Timeout => {
                tracing::warn!("surface acquire timeout, 跳过 draw_frame 这帧");
                return Ok(());
            }
            wgpu::CurrentSurfaceTexture::Occluded => return Ok(()),
            wgpu::CurrentSurfaceTexture::Validation => {
                return Err(anyhow!("wgpu surface acquire 报 Validation 错误"));
            }
        };

        let view = frame
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("quill-frame-encoder"),
            });

        // Step 6: 单 RenderPass — clear + cells (BlendState::REPLACE) + glyphs
        // (BlendState::ALPHA_BLENDING)。两 pipeline 切换在同 RenderPass 内做即可,
        // wgpu 内部允许 set_pipeline 在 pass 内多次调用。
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("quill-frame-pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    resolve_target: None,
                    depth_slice: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(self.clear),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });

            // cells pass
            if cell_vertex_count > 0 {
                let cell_pipeline = self
                    .cell_pipeline
                    .as_ref()
                    .ok_or_else(|| anyhow!("cell_pipeline 应已 lazy 初始化"))?;
                let cell_buf = self
                    .cell_vertex_buffer
                    .as_ref()
                    .ok_or_else(|| anyhow!("cell_vertex_buffer 应已 lazy 初始化"))?;
                pass.set_pipeline(cell_pipeline);
                pass.set_vertex_buffer(0, cell_buf.slice(..));
                pass.draw(0..cell_vertex_count, 0..1);
            }

            // glyphs pass
            if glyph_vertex_count > 0 {
                let glyph_pipeline = self
                    .glyph_pipeline
                    .as_ref()
                    .ok_or_else(|| anyhow!("glyph_pipeline 应已 lazy 初始化"))?;
                let atlas = self
                    .glyph_atlas
                    .as_ref()
                    .ok_or_else(|| anyhow!("glyph_atlas 应已 lazy 初始化"))?;
                let glyph_buf = self
                    .glyph_vertex_buffer
                    .as_ref()
                    .ok_or_else(|| anyhow!("glyph_vertex_buffer 应已 lazy 初始化"))?;
                pass.set_pipeline(glyph_pipeline);
                pass.set_bind_group(0, &atlas.bind_group, &[]);
                pass.set_vertex_buffer(0, glyph_buf.slice(..));
                pass.draw(0..glyph_vertex_count, 0..1);
            }
        }

        self.queue.submit(std::iter::once(encoder.finish()));
        frame.present();
        Ok(())
    }

    /// lazy 初始化 glyph atlas (R8Unorm 2048×2048 + view + sampler + bind group +
    /// bind group layout)。一次创建复用, 派单硬约束沿袭 T-0305 cell pipeline 模式。
    fn ensure_glyph_atlas(&mut self) {
        if self.glyph_atlas.is_some() {
            return;
        }
        let texture = self.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("quill-glyph-atlas"),
            size: wgpu::Extent3d {
                width: ATLAS_W,
                height: ATLAS_H,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            // R8Unorm: 单通道 8-bit alpha mask。fragment shader sample.r 即 alpha。
            format: wgpu::TextureFormat::R8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        let sampler = self.device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("quill-glyph-sampler"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            address_mode_w: wgpu::AddressMode::ClampToEdge,
            // Nearest filtering: Phase 4 整数像素对齐 monospace, 不需要 bilinear
            // (Phase 5+ HiDPI 缩放或 sub-pixel 时再考虑)。NonFiltering sampler 配
            // R8Unorm filterable=false 是 wgpu validation 友好路径。
            mag_filter: wgpu::FilterMode::Nearest,
            min_filter: wgpu::FilterMode::Nearest,
            mipmap_filter: wgpu::MipmapFilterMode::Nearest,
            ..Default::default()
        });
        let bind_group_layout =
            self.device
                .create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                    label: Some("quill-glyph-bgl"),
                    entries: &[
                        wgpu::BindGroupLayoutEntry {
                            binding: 0,
                            visibility: wgpu::ShaderStages::FRAGMENT,
                            ty: wgpu::BindingType::Texture {
                                sample_type: wgpu::TextureSampleType::Float { filterable: false },
                                view_dimension: wgpu::TextureViewDimension::D2,
                                multisampled: false,
                            },
                            count: None,
                        },
                        wgpu::BindGroupLayoutEntry {
                            binding: 1,
                            visibility: wgpu::ShaderStages::FRAGMENT,
                            ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::NonFiltering),
                            count: None,
                        },
                    ],
                });
        let bind_group = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("quill-glyph-bg"),
            layout: &bind_group_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(&sampler),
                },
            ],
        });
        self.glyph_atlas = Some(GlyphAtlas {
            bind_group,
            bind_group_layout,
            view,
            sampler,
            texture,
            allocations: HashMap::new(),
            cursor_x: 0,
            cursor_y: 0,
            row_height: 0,
        });
    }

    /// lazy 初始化 glyph render pipeline。WGSL 内联 [`GLYPH_WGSL`], vertex layout
    /// `pos[2 f32] + uv[2 f32] + color[3 f32]` (28 字节)。BlendState::ALPHA_BLENDING
    /// 让 alpha mask 在 cell 色块上叠加。
    ///
    /// **要求**: ensure_glyph_atlas 必须已跑 (本 fn 取 atlas.bind_group_layout
    /// 作 pipeline_layout)。本 fn 在 [`Self::draw_frame`] 内调, ensure_glyph_atlas
    /// 早一步即可。
    fn ensure_glyph_pipeline(&mut self) {
        if self.glyph_pipeline.is_some() {
            return;
        }
        // 取 atlas 的 bind_group_layout。如果 atlas 还没 init 应是 bug。
        let bgl = match self.glyph_atlas.as_ref() {
            Some(a) => &a.bind_group_layout,
            None => {
                tracing::error!(
                    "ensure_glyph_pipeline 调用前 atlas 必须已 init (ensure_glyph_atlas)"
                );
                return;
            }
        };
        let shader = self
            .device
            .create_shader_module(wgpu::ShaderModuleDescriptor {
                label: Some("quill-glyph-shader"),
                source: wgpu::ShaderSource::Wgsl(GLYPH_WGSL.into()),
            });
        let layout = self
            .device
            .create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some("quill-glyph-pipeline-layout"),
                bind_group_layouts: &[Some(bgl)],
                immediate_size: 0,
            });
        let vertex_attrs = [
            wgpu::VertexAttribute {
                offset: 0,
                shader_location: 0,
                format: wgpu::VertexFormat::Float32x2,
            },
            wgpu::VertexAttribute {
                offset: (2 * std::mem::size_of::<f32>()) as u64,
                shader_location: 1,
                format: wgpu::VertexFormat::Float32x2,
            },
            wgpu::VertexAttribute {
                offset: (4 * std::mem::size_of::<f32>()) as u64,
                shader_location: 2,
                format: wgpu::VertexFormat::Float32x3,
            },
        ];
        let pipeline = self
            .device
            .create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some("quill-glyph-pipeline"),
                layout: Some(&layout),
                vertex: wgpu::VertexState {
                    module: &shader,
                    entry_point: Some("vs_main"),
                    compilation_options: wgpu::PipelineCompilationOptions::default(),
                    buffers: &[wgpu::VertexBufferLayout {
                        array_stride: GLYPH_VERTEX_BYTES as u64,
                        step_mode: wgpu::VertexStepMode::Vertex,
                        attributes: &vertex_attrs,
                    }],
                },
                fragment: Some(wgpu::FragmentState {
                    module: &shader,
                    entry_point: Some("fs_main"),
                    compilation_options: wgpu::PipelineCompilationOptions::default(),
                    targets: &[Some(wgpu::ColorTargetState {
                        format: self.config.format,
                        // ALPHA_BLENDING: src.alpha 控制叠加权重 — alpha=0 完全透明,
                        // alpha=1 完全覆盖。fragment 输出 vec4(fg_color, atlas_alpha)
                        // 后, 黑深蓝 cell 色块上字形像素按 atlas_alpha 与 fg 混合。
                        blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                        write_mask: wgpu::ColorWrites::ALL,
                    })],
                }),
                primitive: wgpu::PrimitiveState {
                    topology: wgpu::PrimitiveTopology::TriangleList,
                    strip_index_format: None,
                    front_face: wgpu::FrontFace::Ccw,
                    cull_mode: None,
                    unclipped_depth: false,
                    polygon_mode: wgpu::PolygonMode::Fill,
                    conservative: false,
                },
                depth_stencil: None,
                multisample: wgpu::MultisampleState::default(),
                multiview_mask: None,
                cache: None,
            });
        self.glyph_pipeline = Some(pipeline);
    }

    /// lazy 初始化 / 增长 glyph vertex buffer。容量按 `needed_bytes / GLYPH_VERTEX_BYTES`
    /// 顶点数, 首次按当前 frame 实际需要分配, 后续 reuse (需要更大时重建)。
    fn ensure_glyph_buffer(&mut self, needed_bytes: usize) {
        let needed_verts = needed_bytes.div_ceil(GLYPH_VERTEX_BYTES);
        if self.glyph_buffer_capacity >= needed_verts && self.glyph_vertex_buffer.is_some() {
            return;
        }
        // 至少分配 1 vert 容量避免 0-size buffer panic
        let alloc_verts = needed_verts.max(1);
        let size_bytes = (alloc_verts * GLYPH_VERTEX_BYTES) as u64;
        let buf = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("quill-glyph-vertex-buffer"),
            size: size_bytes,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        self.glyph_vertex_buffer = Some(buf);
        self.glyph_buffer_capacity = alloc_verts;
    }

    /// atlas allocate (含 lazy raster 与 GPU 上传)。返 Some(slot) 当成功 (含 cache
    /// hit), None 当 rasterize 失败 (Color content / 缺字形)。
    ///
    /// **shelf packing**:
    /// - cache hit (allocations 已有): 直接返
    /// - 新字形: rasterize → 找空位 (`cursor_x + width <= ATLAS_W`, 否则换行
    ///   `cursor_y += row_height; cursor_x = 0; row_height = 0`)
    /// - 高度满 (`cursor_y + height > ATLAS_H`): **T-0406 clear-on-full** —
    ///   清 allocations + reset cursor 后 fall through 到下方 shelf packing,
    ///   当前 raster 在 (0, 0) 重新分配 (atlas 远大于单 glyph 必装得下)。
    ///   tracing::warn! 一行 hiccup 提示, 用户不可见。
    /// - 上传 bitmap 到 texture (queue.write_texture, R8Unorm 单通道, 行无 padding)
    ///
    /// 零尺寸 raster (空格 / zero-width): 仍 insert 一条 zero-size slot 进 HashMap
    /// 避免每帧重 raster。GPU 不上传 (queue.write_texture 不能传零字节 / 零尺寸,
    /// 派单"atlas 满了 panic"路径不需特别处理零尺寸)。
    fn allocate_glyph_slot(
        &mut self,
        text_system: &mut TextSystem,
        glyph: &ShapedGlyph,
    ) -> Option<AtlasSlot> {
        let key = glyph.atlas_key();
        // 拆借: atlas 与 queue 是不同字段, 同时 &mut atlas 与 &queue 即可
        // (write_texture 取 &queue 不变借)。
        if let Some(atlas) = self.glyph_atlas.as_ref() {
            if let Some(slot) = atlas.allocations.get(&key) {
                return Some(*slot);
            }
        }
        let raster = text_system.rasterize(glyph)?;
        let atlas = self.glyph_atlas.as_mut()?;

        // 零尺寸 (空格, 零宽字符): 不上传 GPU, 但记录 slot 防重复 raster 路径
        if raster.width == 0 || raster.height == 0 {
            let slot = AtlasSlot {
                uv_min: [0.0, 0.0],
                uv_max: [0.0, 0.0],
                width: 0,
                height: 0,
                bearing_x: raster.bearing_x,
                bearing_y: raster.bearing_y,
            };
            atlas.allocations.insert(key, slot);
            return Some(slot);
        }

        // shelf packing — 当前行装不下换行
        if atlas.cursor_x + raster.width > ATLAS_W {
            atlas.cursor_y += atlas.row_height;
            atlas.cursor_x = 0;
            atlas.row_height = 0;
        }
        // T-0406 clear-on-full: atlas 满 → 清 allocations + reset shelf cursor。
        // 当前 raster (clear 后) cursor=0,0, ATLAS_W×ATLAS_H 远大于单 glyph 必装得下,
        // 直接 fall through 到下方 `let x = atlas.cursor_x` 走 shelf packing。
        //
        // why clear-on-full 不是真 LRU: 真 LRU 需 per-slot last_use timestamp + slab
        // allocator + free-list, 跟当前 shelf packing 不兼容; clear-on-full 是 KISS
        // 等价物 (终端字符集稳定, 满几乎不触发, 触发时 1 帧 hiccup 重 raster 当帧
        // 可见字, 用户基本看不见)。ROADMAP "T-0406 LRU" 命名沿用历史。
        //
        // why 不 clear texture: 新 raster 通过 queue.write_texture 覆盖旧像素, 旧 uv
        // 已 invalidated (allocations 清了无 caller 引用), 不会有视觉残留。
        // bind_group / view / sampler 全保留 (zero GPU resource churn)。
        //
        // 跨帧 cache 失效后果: 下一帧重 raster 当帧所有可见字 (~1920 个 ASCII 满屏
        // 在 17pt × HIDPI 下, ~50ms 量级 hiccup)。终端使用场景下此触发条件极罕见。
        if atlas.cursor_y + raster.height > ATLAS_H {
            tracing::warn!(
                "glyph atlas full (allocations={}), clearing for re-raster",
                atlas.allocations.len()
            );
            atlas.allocations.clear();
            atlas.cursor_x = 0;
            atlas.cursor_y = 0;
            atlas.row_height = 0;
        }
        let x = atlas.cursor_x;
        let y = atlas.cursor_y;

        // GPU 上传 (queue.write_texture, staging-free 快路径)
        self.queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &atlas.texture,
                mip_level: 0,
                origin: wgpu::Origin3d { x, y, z: 0 },
                aspect: wgpu::TextureAspect::All,
            },
            &raster.bitmap,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                // R8Unorm: 1 byte / pixel, 一行 width bytes; 无 padding (cosmic-text
                // SwashImage 紧致 layout)。
                bytes_per_row: Some(raster.width),
                rows_per_image: Some(raster.height),
            },
            wgpu::Extent3d {
                width: raster.width,
                height: raster.height,
                depth_or_array_layers: 1,
            },
        );

        let slot = AtlasSlot {
            uv_min: [x as f32 / ATLAS_W as f32, y as f32 / ATLAS_H as f32],
            uv_max: [
                (x + raster.width) as f32 / ATLAS_W as f32,
                (y + raster.height) as f32 / ATLAS_H as f32,
            ],
            width: raster.width,
            height: raster.height,
            bearing_x: raster.bearing_x,
            bearing_y: raster.bearing_y,
        };
        atlas.allocations.insert(key, slot);
        atlas.cursor_x += raster.width;
        if raster.height > atlas.row_height {
            atlas.row_height = raster.height;
        }
        Some(slot)
    }

    /// T-0505: shape preedit text + raster + atlas allocate + append 到
    /// glyph_vertex_bytes. 与 [`Self::draw_frame`] 主路径中的行 shape 同套路,
    /// 但起绘位置从 (cursor_col × cell_w, cursor_line × cell_h) 而非 (0, row_idx
    /// × cell_h)。
    ///
    /// **why 单独 fn 而非内联**: 派单 In #D 强调 "preedit 在 cursor 当前位置
    /// 之后绘制"; 抽出来让 draw_frame / render_headless 两路共享同一逻辑
    /// (虽然 render_headless 也是独立 inline glyph loop, 此 method 仅 draw_frame
    /// 复用; render_headless 内联类似 loop 是 T-0408 的设计选择)。
    #[allow(clippy::too_many_arguments)]
    fn append_preedit_glyphs(
        &mut self,
        text_system: &mut TextSystem,
        glyph_vertex_bytes: &mut Vec<u8>,
        text: &str,
        cursor_col: usize,
        cursor_line: usize,
        cell_w_px: f32,
        cell_h_px: f32,
        baseline_y_px: f32,
        surface_w: f32,
        surface_h: f32,
        glyph_color: [f32; 3],
    ) {
        // shape preedit 行 (cosmic-text 走 CJK fallback, T-0405 实测)
        let glyphs = text_system.shape_line(text);
        // preedit 行起绘 x 偏移 (cursor cell 左上角的 surface px). y 同主路径
        // 用 cursor_line × cell_h_px。
        let base_x_px = cursor_col as f32 * cell_w_px;
        let base_y_px = cursor_line as f32 * cell_h_px;
        for glyph in &glyphs {
            if !glyph.x_advance.is_finite() || !glyph.x_offset.is_finite() {
                continue;
            }
            let slot = match self.allocate_glyph_slot(text_system, glyph) {
                Some(s) => s,
                None => continue,
            };
            if slot.width == 0 || slot.height == 0 {
                continue;
            }
            // 起绘位置: cursor cell 左 + glyph.x_offset (line-relative 累积) +
            // bearing_x. y: cursor_line × cell_h + baseline - bearing_y。
            let x_left = base_x_px + glyph.x_offset + slot.bearing_x as f32;
            let y_top = base_y_px + baseline_y_px - slot.bearing_y as f32;
            let x_right = x_left + slot.width as f32;
            let y_bot = y_top + slot.height as f32;
            let ndc_left = x_left / surface_w * 2.0 - 1.0;
            let ndc_right = x_right / surface_w * 2.0 - 1.0;
            let ndc_top = 1.0 - y_top / surface_h * 2.0;
            let ndc_bot = 1.0 - y_bot / surface_h * 2.0;
            let uv_l = slot.uv_min[0];
            let uv_r = slot.uv_max[0];
            let uv_t = slot.uv_min[1];
            let uv_b = slot.uv_max[1];
            let verts: [([f32; 2], [f32; 2]); 6] = [
                ([ndc_left, ndc_top], [uv_l, uv_t]),
                ([ndc_left, ndc_bot], [uv_l, uv_b]),
                ([ndc_right, ndc_bot], [uv_r, uv_b]),
                ([ndc_left, ndc_top], [uv_l, uv_t]),
                ([ndc_right, ndc_bot], [uv_r, uv_b]),
                ([ndc_right, ndc_top], [uv_r, uv_t]),
            ];
            for (pos, uv) in verts {
                glyph_vertex_bytes.extend_from_slice(&pos[0].to_ne_bytes());
                glyph_vertex_bytes.extend_from_slice(&pos[1].to_ne_bytes());
                glyph_vertex_bytes.extend_from_slice(&uv[0].to_ne_bytes());
                glyph_vertex_bytes.extend_from_slice(&uv[1].to_ne_bytes());
                glyph_vertex_bytes.extend_from_slice(&glyph_color[0].to_ne_bytes());
                glyph_vertex_bytes.extend_from_slice(&glyph_color[1].to_ne_bytes());
                glyph_vertex_bytes.extend_from_slice(&glyph_color[2].to_ne_bytes());
            }
        }
    }
}

/// T-0505: 在 cell_vertex_bytes 上追加 preedit 下划线矩形 (cell pass REPLACE
/// 路径). 横跨 (cursor_col, cursor_line) 起 N 个 cell 宽度, 高 PREEDIT_UNDERLINE_PX
/// × HIDPI_SCALE 在 cell 底部。N = preedit char 数; ASCII 1 字符 = 1 cell, CJK
/// 1 字符 = 2 cell (但 char_count 取 chars().count() 偏小, Phase 6 接 east-asian
/// width 表精确算 cell 数, 当前 KISS 用 char count, 视觉上差不多)。
///
/// **why free fn 不是 method on Renderer**: 不需要 self.device / self.atlas,
/// 纯 vertex generation 数学; render_headless 也能复用 (但 headless 内联了类
/// 似 loop, 此 fn 仅 draw_frame 路径用 — render_headless preedit 走自己的
/// inline 实现, 与 T-0408 设计一致)。
#[allow(clippy::too_many_arguments)]
fn append_preedit_underline_to_cell_bytes(
    cell_vertex_bytes: &mut Vec<u8>,
    cursor_col: usize,
    cursor_line: usize,
    char_count: usize,
    cell_w_px: f32,
    cell_h_px: f32,
    surface_w: f32,
    surface_h: f32,
    underline_color: [f32; 3],
) {
    if char_count == 0 {
        return;
    }
    let underline_thickness_px = PREEDIT_UNDERLINE_PX as f32 * HIDPI_SCALE as f32;
    let x0_px = cursor_col as f32 * cell_w_px;
    let x1_px = x0_px + (char_count as f32) * cell_w_px;
    // y 在 cell 底部往上 underline_thickness_px
    let y1_px = (cursor_line + 1) as f32 * cell_h_px;
    let y0_px = y1_px - underline_thickness_px;

    let left = x0_px / surface_w * 2.0 - 1.0;
    let right = x1_px / surface_w * 2.0 - 1.0;
    let top = 1.0 - y0_px / surface_h * 2.0;
    let bottom = 1.0 - y1_px / surface_h * 2.0;

    let verts: [[f32; 2]; 6] = [
        [left, top],
        [left, bottom],
        [right, bottom],
        [left, top],
        [right, bottom],
        [right, top],
    ];
    for v in verts {
        cell_vertex_bytes.extend_from_slice(&v[0].to_ne_bytes());
        cell_vertex_bytes.extend_from_slice(&v[1].to_ne_bytes());
        cell_vertex_bytes.extend_from_slice(&underline_color[0].to_ne_bytes());
        cell_vertex_bytes.extend_from_slice(&underline_color[1].to_ne_bytes());
        cell_vertex_bytes.extend_from_slice(&underline_color[2].to_ne_bytes());
    }
}

/// **T-0408 加** — 离屏渲染入口。**不接 Wayland surface**, 直接 wgpu 渲染到内存
/// `Texture`, readback 像素返 RGBA8 `Vec<u8>`。
///
/// **why** (派单 trigger): T-0403 字形 bug 一周内 3 次诊断错位 (emoji / atlas key
/// / cell+glyph 同色) 全部 because **agent 没法看屏幕** — 每次靠 user 手动跑
/// `cargo run` + 截图发回, Lead 读图推根因, writer 修, 反复极慢。一次性投入
/// `render_headless` 永久收益: 后续每个视觉 ticket reviewer + Phase 5/6 视觉
/// 改动均可走 `cargo run -- --headless-screenshot=/tmp/x.png` + Read PNG 自动
/// verify, 不依赖 GNOME / Wayland / portal / 任何 GUI 工具。
///
/// **why 完全独立于 [`Renderer`] struct** (派单"INV-002 字段顺序如有动 → 不破
/// 不动最好"): `Renderer` 持 [`wgpu::Surface`] (Wayland-bound), 离屏路径不需
/// Surface, 也不能挂 `Renderer` 字段 (否则 INV-002 14 字段链需扩展)。本 fn
/// 自建 Instance/Adapter/Device/Queue + 离屏 Texture + 自有 atlas/pipeline,
/// 函数返回时全部 drop, 不污染 [`Renderer`]。GPU 资源逻辑与 [`Renderer`] 内
/// 部 `ensure_*` 方法相似但 **inline 不 share** — 派单 "抽 draw_frame 公共逻辑"
/// 我读为指核心算法 (cell pass + glyph pass + WGSL shader + atlas shelf packing),
/// 实装走"复用常量与 free fn ([`CELL_WGSL`] / [`GLYPH_WGSL`] / [`ATLAS_W`] /
/// [`CELL_W_PX`] / [`srgb_to_linear`] / [`clear_color_for`]), pipeline / atlas /
/// 顶点生成 inline" 防与 `Renderer` struct 耦合 + 防 T-0407 并行分支合并冲突
/// (T-0407 改 [`GlyphAtlas`] HashMap key 类型, 离屏路径走自有 local atlas state)。
///
/// **流程** (与 [`Renderer::draw_frame`] 同骨架, target 换离屏 Texture):
/// 1. wgpu Instance/Adapter/Device/Queue (无 Surface, headless 路径)
/// 2. 离屏 Texture: 入参 `width × height` 是 **logical**, 内部 ×
///    [`HIDPI_SCALE`] 算 physical (与 [`Renderer::resize`] 同套路, T-0404 适配)
///    -- texture 实创建于 `width × HIDPI_SCALE` × `height × HIDPI_SCALE`
///    physical 像素, format `Rgba8UnormSrgb`, usage `RENDER_ATTACHMENT |
///    COPY_SRC`
/// 3. cell pipeline + glyph atlas + glyph pipeline (本地构造, 不挂 [`Renderer`])
/// 4. cell vertex bytes (走 `cell.bg` 染色, 与 [`Renderer::draw_frame`] T-0407
///    fix 同源) + glyph vertex bytes (shape / raster / atlas allocate)。
///    cell px 用 `CELL_W_PX × HIDPI_SCALE` / `CELL_H_PX × HIDPI_SCALE`,
///    BASELINE_Y_PX 同乘 (与 draw_frame 同, T-0404)。glyph 来自 shape_line 已
///    是 physical px (cosmic-text Metrics × HIDPI_SCALE), 直接相加无需再乘
/// 5. 单 RenderPass: clear + cells (`BlendState::REPLACE`) + glyphs
///    (`BlendState::ALPHA_BLENDING`)
/// 6. `copy_texture_to_buffer` → `MAP_READ` 暂存 buffer (`bytes_per_row` 256
///    对齐, [`wgpu::COPY_BYTES_PER_ROW_ALIGNMENT`])
/// 7. `buffer.slice().map_async` + `Device::poll(PollType::wait_indefinitely())`
///    阻塞等待。INV-005 calloop 单线程禁阻塞 — 但本 fn **不在 calloop 路径**,
///    headless 路径独立 (`src/main.rs` 走 main fn 直接调, 不挂 EventLoop), 无冲突
/// 8. 去除 padding (每行 physical_width × 4 字节, `padded_bytes_per_row` 取
///    256 对齐)
/// 9. 返 `(rgba_bytes, physical_width, physical_height)` —— 长度
///    `physical_w × physical_h × 4`, 行优先 row-major, 第 0 行在 PNG 顶部。
///    **why 返 tuple 不只 Vec<u8>** (T-0404 适配): physical 尺寸是 HIDPI_SCALE
///    × logical, 调用方写 PNG 需要这两值。返 tuple 让调用方不依赖
///    `crate::wl::HIDPI_SCALE` 常量推导, decoupling 清晰。`(width, height)` 入参
///    单位变化 (logical) 不破坏 caller — 之前没用 `(width, height)` 写 PNG
///    (旧 API 直接用 width 写) 的 caller 现在编译失败 (返回类型变), 显式提示
///    迁移
///
/// **PNG encoding** 在调用方 (`src/main.rs::run_headless_screenshot`) 走
/// [`image::PngEncoder::write_image`] (见 ADR 0005), 用 tuple 返回的
/// physical_w / physical_h 作 PNG header 尺寸。
///
/// **错误处理**: wgpu `request_adapter` / `request_device` / `device.poll` /
/// buffer mapping 失败时 [`anyhow::Error`] 抛出, 调用方退出 code != 0。
///
/// **集成测试**: `tests/headless_screenshot.rs` 3 测试覆盖 PNG 文件存在 / 尺寸
/// / 字像素 / 无 emoji artifact (派单 In #D)。
///
/// T-0505: 8 args 因 preedit overlay 加进来了。函数签名是 quill 离屏渲染入口
/// (T-0408 单点), 入参语义都正交 (text_system / cells / dimensions / preedit),
/// 抽 builder struct 收益小代价大 — 派单 KISS 接受 8 args + clippy allow。
#[allow(clippy::too_many_arguments)]
pub fn render_headless(
    text_system: &mut TextSystem,
    cells: &[CellRef],
    cols: usize,
    rows: usize,
    row_texts: &[String],
    width: u32,
    height: u32,
    preedit: Option<&PreeditOverlay>,
) -> Result<(Vec<u8>, u32, u32)> {
    if width == 0 || height == 0 {
        return Err(anyhow!(
            "render_headless: width 与 height 必须 > 0 (got {}×{} logical)",
            width,
            height
        ));
    }

    // T-0404: surface backing 像素 = logical × HIDPI_SCALE。Renderer::resize
    // 同套路 — 入参 logical, 内部乘 HIDPI_SCALE 算 physical 给 wgpu Texture
    // / NDC 换算分母用。saturating_mul 防 overflow (HIDPI_SCALE = 2 + max
    // logical = 6K = 6144 仍远低于 u32::MAX, 防御性写法)。
    let physical_w = width.saturating_mul(HIDPI_SCALE);
    let physical_h = height.saturating_mul(HIDPI_SCALE);

    let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());
    let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
        power_preference: wgpu::PowerPreference::default(),
        force_fallback_adapter: false,
        compatible_surface: None,
    }))
    .map_err(|e| anyhow!("wgpu request_adapter (headless) 失败: {e}"))?;

    let info = adapter.get_info();
    tracing::info!(backend = ?info.backend, name = %info.name, "wgpu adapter (headless) 选中");

    // T-0409 hotfix: 同 Renderer::new (line ~394), 用 adapter.limits() 取实际
    // 硬件上限. headless 路径未来若加 6K 渲染需求 (--headless-screenshot 大尺寸)
    // 同样需要 > 2048 max_texture_dimension_2d.
    let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
        label: Some("quill-headless-device"),
        required_features: wgpu::Features::empty(),
        required_limits: adapter.limits(),
        experimental_features: wgpu::ExperimentalFeatures::default(),
        memory_hints: wgpu::MemoryHints::default(),
        trace: wgpu::Trace::Off,
    }))
    .context("wgpu request_device (headless) 失败")?;

    // why Rgba8UnormSrgb 锁死: PNG output 是 RGBA byte 顺序, 选 Rgba8 避免 BGRA
    // swizzle; sRGB encoding 与用户屏幕 (Bgra8UnormSrgb 主流 path) 同 gamma
    // 处理, 视觉一致。RENDER_ATTACHMENT | COPY_SRC: render pass write + 后续
    // copy_texture_to_buffer 读出。
    let format = wgpu::TextureFormat::Rgba8UnormSrgb;
    let target_texture = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("quill-headless-target"),
        // T-0404: physical_w/h = logical × HIDPI_SCALE。target 与窗口 surface
        // (Renderer::resize 内部走 width × HIDPI_SCALE) 同尺寸语义, 视觉对齐。
        size: wgpu::Extent3d {
            width: physical_w,
            height: physical_h,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let target_view = target_texture.create_view(&wgpu::TextureViewDescriptor::default());

    let clear = clear_color_for(format);
    let is_srgb = format.is_srgb();

    let cell_pipeline = create_headless_cell_pipeline(&device, format);

    // glyph atlas (local, R8Unorm 2048×2048 — 与 Renderer::ensure_glyph_atlas
    // 同尺寸), 函数返回时全部 drop, 不影响 Renderer
    let glyph_texture = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("quill-headless-glyph-atlas"),
        size: wgpu::Extent3d {
            width: ATLAS_W,
            height: ATLAS_H,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::R8Unorm,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    });
    let glyph_view = glyph_texture.create_view(&wgpu::TextureViewDescriptor::default());
    let glyph_sampler = device.create_sampler(&wgpu::SamplerDescriptor {
        label: Some("quill-headless-glyph-sampler"),
        address_mode_u: wgpu::AddressMode::ClampToEdge,
        address_mode_v: wgpu::AddressMode::ClampToEdge,
        address_mode_w: wgpu::AddressMode::ClampToEdge,
        mag_filter: wgpu::FilterMode::Nearest,
        min_filter: wgpu::FilterMode::Nearest,
        mipmap_filter: wgpu::MipmapFilterMode::Nearest,
        ..Default::default()
    });
    let glyph_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some("quill-headless-glyph-bgl"),
        entries: &[
            wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Texture {
                    sample_type: wgpu::TextureSampleType::Float { filterable: false },
                    view_dimension: wgpu::TextureViewDimension::D2,
                    multisampled: false,
                },
                count: None,
            },
            wgpu::BindGroupLayoutEntry {
                binding: 1,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::NonFiltering),
                count: None,
            },
        ],
    });
    let glyph_bg = device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("quill-headless-glyph-bg"),
        layout: &glyph_bgl,
        entries: &[
            wgpu::BindGroupEntry {
                binding: 0,
                resource: wgpu::BindingResource::TextureView(&glyph_view),
            },
            wgpu::BindGroupEntry {
                binding: 1,
                resource: wgpu::BindingResource::Sampler(&glyph_sampler),
            },
        ],
    });
    let glyph_pipeline = create_headless_glyph_pipeline(&device, &glyph_bgl, format);

    // local atlas state (与 GlyphAtlas struct 字段同语义, 但不复用 struct 防
    // T-0407 合并冲突 — T-0407 改 HashMap key 类型 GlyphKey, 此处用 _ 类型推导
    // 跟 ShapedGlyph::atlas_key() 当前 (u16, u32) 自动对齐, T-0407 合并后零改动)
    let mut allocations: HashMap<_, AtlasSlot> = HashMap::new();
    let mut atlas_cursor_x: u32 = 0;
    let mut atlas_cursor_y: u32 = 0;
    let mut atlas_row_height: u32 = 0;

    // T-0404: NDC 换算分母用 physical (与 target_texture 同尺寸); cell_w/h_px
    // / baseline_y_px 同乘 HIDPI_SCALE — 与 Renderer::draw_frame 同套路。
    // glyph 已是 physical (cosmic-text shape_line Metrics × HIDPI_SCALE,
    // rasterize bitmap 也 physical), 后面直接相加无需再乘。
    let surface_w = physical_w as f32;
    let surface_h = physical_h as f32;
    let cell_w_px = CELL_W_PX * HIDPI_SCALE as f32;
    let cell_h_px = CELL_H_PX * HIDPI_SCALE as f32;
    let baseline_y_px = BASELINE_Y_PX * HIDPI_SCALE as f32;

    let mut cell_vertex_bytes: Vec<u8> =
        Vec::with_capacity(cells.len() * VERTS_PER_CELL * VERTEX_BYTES);
    for cell in cells {
        if cell.c == ' ' {
            continue;
        }
        let x0_px = cell.pos.col as f32 * cell_w_px;
        let y0_px = cell.pos.line as f32 * cell_h_px;
        let x1_px = x0_px + cell_w_px;
        let y1_px = y0_px + cell_h_px;
        let left = x0_px / surface_w * 2.0 - 1.0;
        let right = x1_px / surface_w * 2.0 - 1.0;
        let top = 1.0 - y0_px / surface_h * 2.0;
        let bottom = 1.0 - y1_px / surface_h * 2.0;
        // T-0407 D fix 同源: 走 cell.bg 让 glyph fg 在 bg 块上 alpha-blend 可见;
        // 走 fg 会致 glyph (fg) 与 cell (fg) 同色 alpha mask 涂同色等于不可见
        // (T-0403 真因)。
        let color = color_for_vertex_with_srgb(cell.bg, is_srgb);
        let verts: [[f32; 2]; 6] = [
            [left, top],
            [left, bottom],
            [right, bottom],
            [left, top],
            [right, bottom],
            [right, top],
        ];
        for v in verts {
            cell_vertex_bytes.extend_from_slice(&v[0].to_ne_bytes());
            cell_vertex_bytes.extend_from_slice(&v[1].to_ne_bytes());
            cell_vertex_bytes.extend_from_slice(&color[0].to_ne_bytes());
            cell_vertex_bytes.extend_from_slice(&color[1].to_ne_bytes());
            cell_vertex_bytes.extend_from_slice(&color[2].to_ne_bytes());
        }
    }
    // cell_vertex_count 在 preedit underline append 后再算 (T-0505)。

    let effective_rows = row_texts.len().min(rows);
    // 默认 fg #d3d3d3 light gray (与 Renderer::draw_frame 同源, term::Color
    // ::DEFAULT_FG 模块私有不能引用, 内联值, T-0405 后续 per-glyph cell.fg
    // 时改)。
    let fg_default = crate::term::Color {
        r: 0xd3,
        g: 0xd3,
        b: 0xd3,
    };
    let glyph_color = color_for_vertex_with_srgb(fg_default, is_srgb);

    let mut glyph_vertex_bytes: Vec<u8> = Vec::new();
    for (row_idx, row_text) in row_texts.iter().take(effective_rows).enumerate() {
        if row_text.is_empty() {
            continue;
        }
        let glyphs = text_system.shape_line(row_text);
        for glyph in &glyphs {
            if !glyph.x_advance.is_finite() || !glyph.x_offset.is_finite() {
                continue;
            }
            let key = glyph.atlas_key();
            let slot_opt = if let Some(slot) = allocations.get(&key).copied() {
                Some(slot)
            } else if let Some(raster) = text_system.rasterize(glyph) {
                if raster.width == 0 || raster.height == 0 {
                    let slot = AtlasSlot {
                        uv_min: [0.0, 0.0],
                        uv_max: [0.0, 0.0],
                        width: 0,
                        height: 0,
                        bearing_x: raster.bearing_x,
                        bearing_y: raster.bearing_y,
                    };
                    allocations.insert(key, slot);
                    Some(slot)
                } else {
                    if atlas_cursor_x + raster.width > ATLAS_W {
                        atlas_cursor_y += atlas_row_height;
                        atlas_cursor_x = 0;
                        atlas_row_height = 0;
                    }
                    // T-0406 clear-on-full (同 Renderer::allocate_glyph_slot 路径
                    // 语义对齐): atlas 满 → 清 allocations + reset shelf cursor。
                    // headless 路径与 Renderer 路径共享语义, T-0406 之前是 anyhow Err
                    // (调用方退出 code != 0); 改 clear-on-full 让 headless screenshot
                    // 在大字符集场景仍能产 PNG (派单 In #D acceptance + 派单"完工
                    // 后大量字符变化不会 panic, 而是 1 帧 hiccup")。
                    if atlas_cursor_y + raster.height > ATLAS_H {
                        tracing::warn!(
                            "headless glyph atlas full (allocations={}), clearing for re-raster",
                            allocations.len()
                        );
                        allocations.clear();
                        atlas_cursor_x = 0;
                        atlas_cursor_y = 0;
                        atlas_row_height = 0;
                    }
                    let x = atlas_cursor_x;
                    let y = atlas_cursor_y;
                    queue.write_texture(
                        wgpu::TexelCopyTextureInfo {
                            texture: &glyph_texture,
                            mip_level: 0,
                            origin: wgpu::Origin3d { x, y, z: 0 },
                            aspect: wgpu::TextureAspect::All,
                        },
                        &raster.bitmap,
                        wgpu::TexelCopyBufferLayout {
                            offset: 0,
                            bytes_per_row: Some(raster.width),
                            rows_per_image: Some(raster.height),
                        },
                        wgpu::Extent3d {
                            width: raster.width,
                            height: raster.height,
                            depth_or_array_layers: 1,
                        },
                    );
                    let slot = AtlasSlot {
                        uv_min: [x as f32 / ATLAS_W as f32, y as f32 / ATLAS_H as f32],
                        uv_max: [
                            (x + raster.width) as f32 / ATLAS_W as f32,
                            (y + raster.height) as f32 / ATLAS_H as f32,
                        ],
                        width: raster.width,
                        height: raster.height,
                        bearing_x: raster.bearing_x,
                        bearing_y: raster.bearing_y,
                    };
                    allocations.insert(key, slot);
                    atlas_cursor_x += raster.width;
                    if raster.height > atlas_row_height {
                        atlas_row_height = raster.height;
                    }
                    Some(slot)
                }
            } else {
                None
            };
            let Some(slot) = slot_opt else { continue };
            if slot.width == 0 || slot.height == 0 {
                continue;
            }

            let x_left = glyph.x_offset + slot.bearing_x as f32;
            // T-0404: baseline_y_px 已 × HIDPI_SCALE (与 cell_h_px 同单位 physical),
            // glyph.x_offset / bearing_x / bearing_y / slot.width / slot.height
            // 都 physical (shape_line Metrics × HIDPI_SCALE), 单位一致直接相加
            let y_top = (row_idx as f32) * cell_h_px + baseline_y_px - slot.bearing_y as f32;
            let x_right = x_left + slot.width as f32;
            let y_bot = y_top + slot.height as f32;
            let ndc_left = x_left / surface_w * 2.0 - 1.0;
            let ndc_right = x_right / surface_w * 2.0 - 1.0;
            let ndc_top = 1.0 - y_top / surface_h * 2.0;
            let ndc_bot = 1.0 - y_bot / surface_h * 2.0;
            let uv_l = slot.uv_min[0];
            let uv_r = slot.uv_max[0];
            let uv_t = slot.uv_min[1];
            let uv_b = slot.uv_max[1];
            let verts: [([f32; 2], [f32; 2]); 6] = [
                ([ndc_left, ndc_top], [uv_l, uv_t]),
                ([ndc_left, ndc_bot], [uv_l, uv_b]),
                ([ndc_right, ndc_bot], [uv_r, uv_b]),
                ([ndc_left, ndc_top], [uv_l, uv_t]),
                ([ndc_right, ndc_bot], [uv_r, uv_b]),
                ([ndc_right, ndc_top], [uv_r, uv_t]),
            ];
            for (pos, uv) in verts {
                glyph_vertex_bytes.extend_from_slice(&pos[0].to_ne_bytes());
                glyph_vertex_bytes.extend_from_slice(&pos[1].to_ne_bytes());
                glyph_vertex_bytes.extend_from_slice(&uv[0].to_ne_bytes());
                glyph_vertex_bytes.extend_from_slice(&uv[1].to_ne_bytes());
                glyph_vertex_bytes.extend_from_slice(&glyph_color[0].to_ne_bytes());
                glyph_vertex_bytes.extend_from_slice(&glyph_color[1].to_ne_bytes());
                glyph_vertex_bytes.extend_from_slice(&glyph_color[2].to_ne_bytes());
            }
        }
    }

    // T-0505: preedit overlay (派单 In #D + #H 测试覆盖). 在 cursor cell 起点
    // 之后绘制 preedit 字 + 底部下划线。逻辑与 Renderer::draw_frame 同语义,
    // 但 inline 实装 (T-0408 设计选择: render_headless 不复用 Renderer 内部
    // 方法, 防 GPU 资源耦合)。
    if let Some(p) = preedit {
        if !p.text.is_empty() {
            let base_x_px = p.cursor_col as f32 * cell_w_px;
            let base_y_px = p.cursor_line as f32 * cell_h_px;
            let preedit_glyphs = text_system.shape_line(&p.text);
            for glyph in &preedit_glyphs {
                if !glyph.x_advance.is_finite() || !glyph.x_offset.is_finite() {
                    continue;
                }
                let key = glyph.atlas_key();
                let slot_opt = if let Some(slot) = allocations.get(&key).copied() {
                    Some(slot)
                } else if let Some(raster) = text_system.rasterize(glyph) {
                    if raster.width == 0 || raster.height == 0 {
                        let slot = AtlasSlot {
                            uv_min: [0.0, 0.0],
                            uv_max: [0.0, 0.0],
                            width: 0,
                            height: 0,
                            bearing_x: raster.bearing_x,
                            bearing_y: raster.bearing_y,
                        };
                        allocations.insert(key, slot);
                        Some(slot)
                    } else {
                        if atlas_cursor_x + raster.width > ATLAS_W {
                            atlas_cursor_y += atlas_row_height;
                            atlas_cursor_x = 0;
                            atlas_row_height = 0;
                        }
                        if atlas_cursor_y + raster.height > ATLAS_H {
                            tracing::warn!("headless preedit atlas full, clearing for re-raster");
                            allocations.clear();
                            atlas_cursor_x = 0;
                            atlas_cursor_y = 0;
                            atlas_row_height = 0;
                        }
                        let x = atlas_cursor_x;
                        let y = atlas_cursor_y;
                        queue.write_texture(
                            wgpu::TexelCopyTextureInfo {
                                texture: &glyph_texture,
                                mip_level: 0,
                                origin: wgpu::Origin3d { x, y, z: 0 },
                                aspect: wgpu::TextureAspect::All,
                            },
                            &raster.bitmap,
                            wgpu::TexelCopyBufferLayout {
                                offset: 0,
                                bytes_per_row: Some(raster.width),
                                rows_per_image: Some(raster.height),
                            },
                            wgpu::Extent3d {
                                width: raster.width,
                                height: raster.height,
                                depth_or_array_layers: 1,
                            },
                        );
                        let slot = AtlasSlot {
                            uv_min: [x as f32 / ATLAS_W as f32, y as f32 / ATLAS_H as f32],
                            uv_max: [
                                (x + raster.width) as f32 / ATLAS_W as f32,
                                (y + raster.height) as f32 / ATLAS_H as f32,
                            ],
                            width: raster.width,
                            height: raster.height,
                            bearing_x: raster.bearing_x,
                            bearing_y: raster.bearing_y,
                        };
                        allocations.insert(key, slot);
                        atlas_cursor_x += raster.width;
                        if raster.height > atlas_row_height {
                            atlas_row_height = raster.height;
                        }
                        Some(slot)
                    }
                } else {
                    None
                };
                let Some(slot) = slot_opt else { continue };
                if slot.width == 0 || slot.height == 0 {
                    continue;
                }
                let x_left = base_x_px + glyph.x_offset + slot.bearing_x as f32;
                let y_top = base_y_px + baseline_y_px - slot.bearing_y as f32;
                let x_right = x_left + slot.width as f32;
                let y_bot = y_top + slot.height as f32;
                let ndc_left = x_left / surface_w * 2.0 - 1.0;
                let ndc_right = x_right / surface_w * 2.0 - 1.0;
                let ndc_top = 1.0 - y_top / surface_h * 2.0;
                let ndc_bot = 1.0 - y_bot / surface_h * 2.0;
                let uv_l = slot.uv_min[0];
                let uv_r = slot.uv_max[0];
                let uv_t = slot.uv_min[1];
                let uv_b = slot.uv_max[1];
                let verts: [([f32; 2], [f32; 2]); 6] = [
                    ([ndc_left, ndc_top], [uv_l, uv_t]),
                    ([ndc_left, ndc_bot], [uv_l, uv_b]),
                    ([ndc_right, ndc_bot], [uv_r, uv_b]),
                    ([ndc_left, ndc_top], [uv_l, uv_t]),
                    ([ndc_right, ndc_bot], [uv_r, uv_b]),
                    ([ndc_right, ndc_top], [uv_r, uv_t]),
                ];
                for (pos, uv) in verts {
                    glyph_vertex_bytes.extend_from_slice(&pos[0].to_ne_bytes());
                    glyph_vertex_bytes.extend_from_slice(&pos[1].to_ne_bytes());
                    glyph_vertex_bytes.extend_from_slice(&uv[0].to_ne_bytes());
                    glyph_vertex_bytes.extend_from_slice(&uv[1].to_ne_bytes());
                    glyph_vertex_bytes.extend_from_slice(&glyph_color[0].to_ne_bytes());
                    glyph_vertex_bytes.extend_from_slice(&glyph_color[1].to_ne_bytes());
                    glyph_vertex_bytes.extend_from_slice(&glyph_color[2].to_ne_bytes());
                }
            }

            // 下划线: append 到 cell_vertex_bytes (cell pass REPLACE, 颜色 #ffffff
            // 让用户区分组词态)
            let underline_color = color_for_vertex_with_srgb(
                crate::term::Color {
                    r: 0xff,
                    g: 0xff,
                    b: 0xff,
                },
                is_srgb,
            );
            append_preedit_underline_to_cell_bytes(
                &mut cell_vertex_bytes,
                p.cursor_col,
                p.cursor_line,
                p.text.chars().count(),
                cell_w_px,
                cell_h_px,
                surface_w,
                surface_h,
                underline_color,
            );
        }
    }

    // 重新算 vertex count (preedit 已可能 append cell underline + glyph)
    let cell_vertex_count = (cell_vertex_bytes.len() / VERTEX_BYTES) as u32;
    let glyph_vertex_count = (glyph_vertex_bytes.len() / GLYPH_VERTEX_BYTES) as u32;

    let cell_vbuf = if cell_vertex_count > 0 {
        let buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("quill-headless-cell-vertex"),
            size: cell_vertex_bytes.len() as u64,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        queue.write_buffer(&buf, 0, &cell_vertex_bytes);
        Some(buf)
    } else {
        None
    };

    let glyph_vbuf = if glyph_vertex_count > 0 {
        let buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("quill-headless-glyph-vertex"),
            size: glyph_vertex_bytes.len() as u64,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        queue.write_buffer(&buf, 0, &glyph_vertex_bytes);
        Some(buf)
    } else {
        None
    };

    tracing::debug!(
        target: "quill::wl::render",
        cols, rows,
        logical_w = width, logical_h = height,
        physical_w, physical_h,
        cell_vertex_count, glyph_vertex_count,
        atlas_count = allocations.len(),
        "render_headless stats"
    );

    let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
        label: Some("quill-headless-encoder"),
    });
    {
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("quill-headless-pass"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: &target_view,
                resolve_target: None,
                depth_slice: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(clear),
                    store: wgpu::StoreOp::Store,
                },
            })],
            depth_stencil_attachment: None,
            timestamp_writes: None,
            occlusion_query_set: None,
            multiview_mask: None,
        });
        if let Some(buf) = cell_vbuf.as_ref() {
            pass.set_pipeline(&cell_pipeline);
            pass.set_vertex_buffer(0, buf.slice(..));
            pass.draw(0..cell_vertex_count, 0..1);
        }
        if let Some(buf) = glyph_vbuf.as_ref() {
            pass.set_pipeline(&glyph_pipeline);
            pass.set_bind_group(0, &glyph_bg, &[]);
            pass.set_vertex_buffer(0, buf.slice(..));
            pass.draw(0..glyph_vertex_count, 0..1);
        }
    }

    // bytes_per_row 必须 256 对齐 (wgpu COPY_BYTES_PER_ROW_ALIGNMENT)。
    // 例: physical 1600 px × 4 bytes = 6400, 已 256 对齐; 1366 × 4 = 5464 不
    // 对齐 → padded = 5632。每行尾部 padding 在 readback 后剥除。
    // T-0404: readback 用 physical 尺寸 (target_texture 的真尺寸)。
    let unpadded_bytes_per_row = physical_w
        .checked_mul(4)
        .ok_or_else(|| anyhow!("physical_w × 4 溢出 u32 (physical_w = {})", physical_w))?;
    let align = wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
    let padded_bytes_per_row = unpadded_bytes_per_row.div_ceil(align) * align;
    let buffer_size = u64::from(padded_bytes_per_row) * u64::from(physical_h);
    let readback_buf = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("quill-headless-readback"),
        size: buffer_size,
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    encoder.copy_texture_to_buffer(
        wgpu::TexelCopyTextureInfo {
            texture: &target_texture,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        wgpu::TexelCopyBufferInfo {
            buffer: &readback_buf,
            layout: wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(padded_bytes_per_row),
                rows_per_image: Some(physical_h),
            },
        },
        wgpu::Extent3d {
            width: physical_w,
            height: physical_h,
            depth_or_array_layers: 1,
        },
    );
    queue.submit(std::iter::once(encoder.finish()));

    // map_async + Device::poll(Wait): 阻塞等 GPU 完成 + buffer 内存映射就绪。
    // INV-005 calloop 单线程禁阻塞 — 但本 fn 不在 calloop 路径 (headless 路径
    // 走 main.rs 直接调, 无 EventLoop)。
    let slice = readback_buf.slice(..);
    let (sender, receiver) = std::sync::mpsc::channel();
    slice.map_async(wgpu::MapMode::Read, move |result| {
        // 发送失败 (receiver 早已 drop) 接受静默 — 调用方已经退出 readback 路径
        let _ = sender.send(result);
    });
    device
        .poll(wgpu::PollType::wait_indefinitely())
        .map_err(|e| anyhow!("device poll wait 失败: {e:?}"))?;
    receiver
        .recv()
        .context("readback receiver recv 失败 (sender 已 drop?)")?
        .map_err(|e| anyhow!("buffer map_async 失败: {e:?}"))?;

    let mapped = slice.get_mapped_range();
    let mut out: Vec<u8> =
        Vec::with_capacity((unpadded_bytes_per_row as usize) * (physical_h as usize));
    let row_stride = padded_bytes_per_row as usize;
    let row_unpadded = unpadded_bytes_per_row as usize;
    for row in 0..(physical_h as usize) {
        let row_start = row * row_stride;
        let row_end = row_start + row_unpadded;
        out.extend_from_slice(&mapped[row_start..row_end]);
    }
    drop(mapped);
    readback_buf.unmap();

    Ok((out, physical_w, physical_h))
}

/// [`render_headless`] 用的 sRGB-aware 色彩转换 (`Color` → linear `[f32; 3]`)。
/// 与 [`Renderer::color_for_vertex`] 等价但 free fn (无 `&self`), 让 headless
/// 路径不挂 `Renderer` 实例。`is_srgb=true` 时走 [`srgb_to_linear`] 预补偿
/// (sRGB 表面把写入值当 linear, GPU 编码回 sRGB 显示, 跟 [`clear_color_for`]
/// 同套路)。
fn color_for_vertex_with_srgb(c: crate::term::Color, is_srgb: bool) -> [f32; 3] {
    let r = f64::from(c.r) / 255.0;
    let g = f64::from(c.g) / 255.0;
    let b = f64::from(c.b) / 255.0;
    if is_srgb {
        [
            srgb_to_linear(r) as f32,
            srgb_to_linear(g) as f32,
            srgb_to_linear(b) as f32,
        ]
    } else {
        [r as f32, g as f32, b as f32]
    }
}

/// [`render_headless`] 用的 cell render pipeline (与 [`Renderer::ensure_cell_pipeline`]
/// 同骨架, 只多 `format` 入参让 headless 路径锁 [`wgpu::TextureFormat::Rgba8UnormSrgb`])。
fn create_headless_cell_pipeline(
    device: &wgpu::Device,
    format: wgpu::TextureFormat,
) -> wgpu::RenderPipeline {
    let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some("quill-headless-cells-shader"),
        source: wgpu::ShaderSource::Wgsl(CELL_WGSL.into()),
    });
    let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some("quill-headless-cells-pipeline-layout"),
        bind_group_layouts: &[],
        immediate_size: 0,
    });
    let vertex_attrs = [
        wgpu::VertexAttribute {
            offset: 0,
            shader_location: 0,
            format: wgpu::VertexFormat::Float32x2,
        },
        wgpu::VertexAttribute {
            offset: (2 * std::mem::size_of::<f32>()) as u64,
            shader_location: 1,
            format: wgpu::VertexFormat::Float32x3,
        },
    ];
    device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some("quill-headless-cells-pipeline"),
        layout: Some(&layout),
        vertex: wgpu::VertexState {
            module: &shader,
            entry_point: Some("vs_main"),
            compilation_options: wgpu::PipelineCompilationOptions::default(),
            buffers: &[wgpu::VertexBufferLayout {
                array_stride: VERTEX_BYTES as u64,
                step_mode: wgpu::VertexStepMode::Vertex,
                attributes: &vertex_attrs,
            }],
        },
        fragment: Some(wgpu::FragmentState {
            module: &shader,
            entry_point: Some("fs_main"),
            compilation_options: wgpu::PipelineCompilationOptions::default(),
            targets: &[Some(wgpu::ColorTargetState {
                format,
                blend: Some(wgpu::BlendState::REPLACE),
                write_mask: wgpu::ColorWrites::ALL,
            })],
        }),
        primitive: wgpu::PrimitiveState {
            topology: wgpu::PrimitiveTopology::TriangleList,
            strip_index_format: None,
            front_face: wgpu::FrontFace::Ccw,
            cull_mode: None,
            unclipped_depth: false,
            polygon_mode: wgpu::PolygonMode::Fill,
            conservative: false,
        },
        depth_stencil: None,
        multisample: wgpu::MultisampleState::default(),
        multiview_mask: None,
        cache: None,
    })
}

/// [`render_headless`] 用的 glyph render pipeline (与 [`Renderer::ensure_glyph_pipeline`]
/// 同骨架, `format` 锁 [`wgpu::TextureFormat::Rgba8UnormSrgb`], `bgl` 由调用方
/// 传入 — 与 atlas 的 bind_group_layout 匹配)。
fn create_headless_glyph_pipeline(
    device: &wgpu::Device,
    bgl: &wgpu::BindGroupLayout,
    format: wgpu::TextureFormat,
) -> wgpu::RenderPipeline {
    let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some("quill-headless-glyph-shader"),
        source: wgpu::ShaderSource::Wgsl(GLYPH_WGSL.into()),
    });
    let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some("quill-headless-glyph-pipeline-layout"),
        bind_group_layouts: &[Some(bgl)],
        immediate_size: 0,
    });
    let vertex_attrs = [
        wgpu::VertexAttribute {
            offset: 0,
            shader_location: 0,
            format: wgpu::VertexFormat::Float32x2,
        },
        wgpu::VertexAttribute {
            offset: (2 * std::mem::size_of::<f32>()) as u64,
            shader_location: 1,
            format: wgpu::VertexFormat::Float32x2,
        },
        wgpu::VertexAttribute {
            offset: (4 * std::mem::size_of::<f32>()) as u64,
            shader_location: 2,
            format: wgpu::VertexFormat::Float32x3,
        },
    ];
    device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some("quill-headless-glyph-pipeline"),
        layout: Some(&layout),
        vertex: wgpu::VertexState {
            module: &shader,
            entry_point: Some("vs_main"),
            compilation_options: wgpu::PipelineCompilationOptions::default(),
            buffers: &[wgpu::VertexBufferLayout {
                array_stride: GLYPH_VERTEX_BYTES as u64,
                step_mode: wgpu::VertexStepMode::Vertex,
                attributes: &vertex_attrs,
            }],
        },
        fragment: Some(wgpu::FragmentState {
            module: &shader,
            entry_point: Some("fs_main"),
            compilation_options: wgpu::PipelineCompilationOptions::default(),
            targets: &[Some(wgpu::ColorTargetState {
                format,
                blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                write_mask: wgpu::ColorWrites::ALL,
            })],
        }),
        primitive: wgpu::PrimitiveState {
            topology: wgpu::PrimitiveTopology::TriangleList,
            strip_index_format: None,
            front_face: wgpu::FrontFace::Ccw,
            cull_mode: None,
            unclipped_depth: false,
            polygon_mode: wgpu::PolygonMode::Fill,
            conservative: false,
        },
        depth_stencil: None,
        multisample: wgpu::MultisampleState::default(),
        multiview_mask: None,
        cache: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn color_matches_spec() {
        // Ticket acceptance 把 #0a1030 写死;这个测试防止以后"顺手调亮一点"。
        assert_eq!(CLEAR_COLOR_SRGB_U8, [0x0a, 0x10, 0x30, 0xff]);
    }

    #[test]
    fn srgb_linear_roundtrip_endpoints() {
        // 数值 sanity:黑 → 0,白 → 1。
        assert!((srgb_to_linear(0.0)).abs() < 1e-9);
        assert!((srgb_to_linear(1.0) - 1.0).abs() < 1e-9);
    }

    #[test]
    fn srgb_path_darkens_midtones() {
        // `#30 = 48`,sRGB 0.188 → 线性约 0.031。只要是"比 sRGB 暗"即过关,
        // 避免硬编码一串浮点。
        let v_srgb = 48.0_f64 / 255.0;
        let v_lin = srgb_to_linear(v_srgb);
        assert!(v_lin < v_srgb, "sRGB→linear 必须使中间灰变暗");
        assert!(v_lin > 0.0);
    }

    #[test]
    fn non_srgb_format_uses_raw_components() {
        // Unorm (非 sRGB) 格式下不做 gamma,直接取 byte/255。
        let c = clear_color_for(wgpu::TextureFormat::Bgra8Unorm);
        assert!((c.r - 10.0 / 255.0).abs() < 1e-9);
        assert!((c.g - 16.0 / 255.0).abs() < 1e-9);
        assert!((c.b - 48.0 / 255.0).abs() < 1e-9);
        assert!((c.a - 1.0).abs() < 1e-9);
    }

    #[test]
    fn srgb_format_applies_gamma() {
        let c = clear_color_for(wgpu::TextureFormat::Bgra8UnormSrgb);
        // sRGB 输出期望 "GPU 编码回去后" ≈ #0a1030;所以存进 wgpu::Color 的
        // 必然是更小(被 decode 过的)值。
        assert!(c.r < 10.0 / 255.0);
        assert!(c.g < 16.0 / 255.0);
        assert!(c.b < 48.0 / 255.0);
    }

    /// T-0404: HIDPI_SCALE 是 hardcode 2x 简化版 (派单 In #A 模块顶部 const)。
    /// 锁住此常数防 Phase 4+ 顺手改回 1 (字会糊) / 改成 1.5 (cosmic-text raster
    /// 尺寸非整数, atlas 装载浮点 trade-off 复杂)。Phase 5+ 真接 wl_output.scale
    /// 时本测试改为 dynamic 路径 + 删此常数。
    #[test]
    fn hidpi_scale_is_2() {
        assert_eq!(
            HIDPI_SCALE, 2,
            "T-0404 hardcode 2x 简化版 — 派单 Out 段明示不接 wl_output.scale event \
             (用户硬偏好); 改此常数前先看 docs/audit/2026-04-25-T-0404-review.md"
        );
    }
}
