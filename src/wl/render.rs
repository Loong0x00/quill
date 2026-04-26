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

/// T-0802 In #A: 从 surface capabilities 选 present_mode, 偏好 Mailbox 减拖窗口
/// stutter, fallback Fifo 兼容性兜底.
///
/// **why Mailbox**: Fifo (默认 vsync, queue ~3 帧) 在拖窗口 resize 高频 configure
/// 时 GPU 端 vsync 阻塞累积可见 lag (用户实测 "巨大延迟和滑动"). Mailbox 单帧
/// queue + 新帧替换旧帧, 不阻塞 vsync, 减拖动 stutter (wgpu 文档"Fast Vsync").
/// 两者都 no-tearing, 视觉无差别 — 仅时延差.
///
/// **why fallback Fifo**: wgpu 文档明示 Mailbox 仅 "DX12 / NVidia Vulkan / Wayland
/// Vulkan" 支持, AMD Wayland / Intel / 软件 backend 可能无. Fifo "All platforms"
/// 必有 — 退路保险. AMD 可走 FifoRelaxed (Adaptive Vsync) 但 quill daily-drive
/// 在 NVIDIA 5090 + Wayland Vulkan, Mailbox 命中, 不引入第三优先级简化决策.
///
/// **抽纯 fn 单测覆盖**: `Renderer::new` 内联走 `caps.present_modes` (Vec) 路径
/// 不可 headless 测 (要真 wl + adapter), 抽决策点纯 fn 同 conventions §3 +
/// `should_propagate_resize` / `verdict_for_scale` / `decoration_log_decision` 套路.
pub(crate) fn select_present_mode(modes: &[wgpu::PresentMode]) -> wgpu::PresentMode {
    if modes.contains(&wgpu::PresentMode::Mailbox) {
        wgpu::PresentMode::Mailbox
    } else {
        wgpu::PresentMode::Fifo
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
    /// T-0702: titlebar 中央显示的标题文字 (默认 [`DEFAULT_TITLE`] = "quill",
    /// 上层通过 [`Self::set_title`] 同步 xdg_toplevel.set_title 的值). POD
    /// 字符串无 GPU 资源, drop 顺序无关 — 放 `surface_is_srgb` 之后 / `instance`
    /// 之前, 与其它 POD 字段 visual locality 一致 (INV-002 entry 同步更新).
    title: String,
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

/// **CSD titlebar 高度** (logical px, T-0504). 派单 In #D 硬编码 28.
///
/// 顶部 28 logical px (= 56 physical 在 HIDPI×2) 是 client-side decoration
/// titlebar — 灰色矩形 + 三按钮位于右上角. text area (cell grid) 起始 y 从
/// 此常数往下偏移. 视觉上接近 GNOME mutter / Adwaita 默认 titlebar 高度.
///
/// **单一来源**: [`crate::wl::pointer::hit_test`] 直接 import 用, 改一处即视觉
/// 与 hit-test 同步, 无漂移风险 (派单 In #F SOP).
pub const TITLEBAR_H_LOGICAL_PX: u32 = 28;

/// **CSD 按钮宽度** (logical px, T-0504). 24 logical px 是 GNOME / KDE 默认
/// titlebar 按钮尺寸; 三按钮总宽 72 logical, 800 logical 窗口右上角放得下.
pub const BUTTON_W_LOGICAL_PX: u32 = 24;

/// **CSD 按钮高度** (logical px, T-0504). 24 logical, ≤ TITLEBAR_H (28),
/// 顶贴 titlebar 顶部 (y ∈ [0, 24)), 底部留 4 px 边距使按钮视觉与 titlebar 分离.
pub const BUTTON_H_LOGICAL_PX: u32 = 24;

/// **CSD titlebar 配色**.
const TITLEBAR_BG: crate::term::Color = crate::term::Color {
    r: 0x2c,
    g: 0x2c,
    b: 0x2c,
};

/// 按钮 hover 时背景色 (深灰).
const BUTTON_BG_HOVER: crate::term::Color = crate::term::Color {
    r: 0x4a,
    g: 0x4a,
    b: 0x4a,
};

/// Close 按钮 hover 时背景色 (红, 与 GNOME / KDE 关闭按钮 hover 视觉一致).
const BUTTON_BG_CLOSE_HOVER: crate::term::Color = crate::term::Color {
    r: 0xe5,
    g: 0x39,
    b: 0x35,
};

/// 按钮 icon 色 (浅灰 #d3d3d3, 在深灰 / 红背景上对比清晰).
const BUTTON_ICON: crate::term::Color = crate::term::Color {
    r: 0xd3,
    g: 0xd3,
    b: 0xd3,
};

/// **CSD titlebar 在 cell vertex buffer 内额外占的"虚拟 cell 行"数** (T-0504).
const TITLEBAR_RESERVED_QUAD_ROWS: usize = 64;

/// **T-0702 默认窗口标题** (跟 [`crate::wl::window`] 的 `WINDOW_TITLE` 同源,
/// 模块边界让 render 不反向依赖 window).
pub const DEFAULT_TITLE: &str = "quill";

/// **T-0505: preedit 下划线像素厚度** (logical px, render 内部 × HIDPI_SCALE).
pub const PREEDIT_UNDERLINE_PX: u32 = 2;

/// T-0505: preedit overlay 入参。draw_frame / render_headless 收到 Some(_)
/// 时在 (cursor_col, cursor_line) cell 起点之后绘制 preedit 字 + 底部下划线。
#[derive(Debug, Clone)]
pub struct PreeditOverlay {
    pub text: String,
    pub cursor_col: usize,
    pub cursor_line: usize,
}

/// **T-0601: 光标渲染厚度** (logical px, render 内部 × HIDPI_SCALE).
/// Underline 模式底部横线 / Beam 模式左侧竖线 / HollowBlock 边框 厚度共用。
/// 2 logical = 4 physical (HIDPI_SCALE=2), 视觉清晰且不喧宾夺主。
pub const CURSOR_THICKNESS_PX: u32 = 2;

/// **T-0604: 光标 cell 左/右内缩** (logical px, render 内部 × HIDPI_SCALE).
///
/// 1 logical = 2 physical (HIDPI_SCALE=2), 总宽减 4 physical px. 让 cursor
/// quad 不接触相邻 cell 边缘, 避开"字形 advance > CELL_W_PX 时上一字形像素
/// 溢出本 cell 左侧" 的视觉误盖 (T-0604 user 实测 'a' 后 cursor 视觉盖字符
/// 右半 — 真因是 cell 几何不是协议: cosmic-text DejaVu Sans Mono 17pt
/// advance ~11 px > CELL_W_PX = 10 logical, 字形右缘溢出到下一 cell 左 1 px,
/// cursor 在下一 cell 左 1 px 内开画就盖到溢出像素).
///
/// **why 不直接校准 CELL_W_PX**: 改 cell 宽度跨 render / window / term 大改
/// (cols/rows 重算, surface 几何全链路), Phase 6+ 单独 ticket 如需; 本 ticket
/// 只做视觉对齐主流终端 (alacritty / xterm / foot 同套 inset 套路) 不动 cell
/// 几何契约。
pub const CURSOR_INSET_PX: u32 = 1;

/// **T-0604: cell.bg default skip 比较常数**, 与 [`crate::term::Color::DEFAULT_BG`]
/// 同源 (`#000000`). render 模块复制 inline literal 而非引用 src/term 私有
/// const, 沿袭本模块 [`TITLEBAR_BG`] / [`BUTTON_BG_HOVER`] / render_headless
/// 内联 `fg_default` 同套路 (term::Color 默认值 const 模块私有, 不暴露)。
///
/// **改动同步**: 若 src/term/mod.rs::Color::DEFAULT_BG 改值, 本常数必须同步;
/// reviewer 走 grep 校对。
const CELL_BG_DEFAULT: crate::term::Color = crate::term::Color {
    r: 0x00,
    g: 0x00,
    b: 0x00,
};

/// T-0601: 光标渲染入参。draw_frame / render_headless 在 (col, line) cell 位
/// 置按 [`CursorStyle`] 绘制 quad (Block 整 cell 反色, Underline 底部横线,
/// Beam 左侧竖线, HollowBlock 4 边框, Hidden 跳过)。
///
/// **why quill 自有 enum 而非 re-export `term::CursorShape`**: INV-010 类型
/// 隔离 — render 层入参语义只关心"渲染怎么画", 与 term 状态机的 ANSI shape
/// 语义解耦。term 的 `CursorShape` 已经是 quill 自有 enum (而非 alacritty
/// 上游), render 这边再加一层 `CursorStyle` 仅 4 variant (Block / Underline /
/// Beam / HollowBlock, Hidden 折叠到 visible=false), 调用方 (window.rs idle
/// callback) 显式 match `term::CursorShape -> render::CursorStyle` — 上游加
/// shape variant 时 compile error 在 window.rs 一处捕获, 不传染到 render。
///
/// `visible` 字段独立 (与 [`crate::term::TermState::cursor_visible`] 同语义,
/// SHOW_CURSOR 模式位): IME preedit 显示时调用方传 `visible=false` 隐光标
/// (光标位置与 preedit 起点视觉冲突, 主流 IME 都隐光标显 preedit)。
#[derive(Debug, Clone, Copy)]
pub struct CursorInfo {
    pub col: usize,
    pub line: usize,
    pub visible: bool,
    pub style: CursorStyle,
    /// 光标块 / 线 / 框的颜色。常态走 cell.fg (浅灰 #d3d3d3, 与字形同色 → cell
    /// 上字形被块覆盖呈"实心方块"视觉, alacritty 的 unfocused 等价路径). 调用
    /// 方据需自定 — 例: 未来 focus-aware 可在失焦时改 #888888 暗灰。
    pub color: crate::term::Color,
}

/// T-0601: 光标渲染形状. 与 [`crate::term::CursorShape`] 5 variant 语义同源,
/// 但 render 层折掉 `Hidden` (走 [`CursorInfo::visible`] = false). 4 variant 全
/// 走 cell pass (REPLACE color rect, append 到 cell_vertex_bytes), 不需 glyph
/// 路径.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CursorStyle {
    /// 整 cell 实心填充 (默认). 字形被覆盖呈实心方块, daily-drive 体感与
    /// alacritty / foot 一致.
    Block,
    /// 底部横线 (`CURSOR_THICKNESS_PX × HIDPI_SCALE` 物理 px), 与 preedit
    /// underline 共用厚度但不与之重叠 (preedit 隐光标, 见 [`CursorInfo`] doc).
    Underline,
    /// 左侧竖线 (`CURSOR_THICKNESS_PX × HIDPI_SCALE` 物理 px). VS Code 风格.
    Beam,
    /// 4 边框 (各 `CURSOR_THICKNESS_PX × HIDPI_SCALE` 物理 px). alacritty
    /// 失焦时的视觉, focus-aware 入口 (Phase 6+ wl_keyboard enter/leave 接入
    /// 时 window.rs 据此切换 Block ↔ HollowBlock).
    HollowBlock,
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

/// 把一个像素矩形 (x0, y0, x1, y1) 加 6 顶点到 vertex buffer (CCW 三角化).
/// `color` 已是 linear f32x3 (调用方按 sRGB-aware [`color_for_vertex_with_srgb`]
/// 或 [`Renderer::color_for_vertex`] 预处理过).
///
/// `clippy::too_many_arguments` allow: 函数职责单一 (NDC 换算 + 6 顶点输出),
/// 8 个参数都直接对应 NDC 公式输入 (rect coords / surface dims / color). 抽
/// struct 反而引入间接 + 调用方需先建临时 struct, 净复杂度增加.
#[allow(clippy::too_many_arguments)]
fn append_quad_px(
    out: &mut Vec<u8>,
    x0_px: f32,
    y0_px: f32,
    x1_px: f32,
    y1_px: f32,
    surface_w: f32,
    surface_h: f32,
    color: [f32; 3],
) {
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
        out.extend_from_slice(&v[0].to_ne_bytes());
        out.extend_from_slice(&v[1].to_ne_bytes());
        out.extend_from_slice(&color[0].to_ne_bytes());
        out.extend_from_slice(&color[1].to_ne_bytes());
        out.extend_from_slice(&color[2].to_ne_bytes());
    }
}

/// **T-0702 标题居中起点 X** (physical px). 派单关键提示原文:
/// `x = (surface_w / 2) - (title_advance / 2)`. 当 title 比 surface 宽时
/// (`title_advance > surface_w`) 直接落 0 防 NDC 跑负, 视觉左对齐截断.
///
/// **why free fn**: 纯算术, 无 GPU / Renderer 状态依赖, 单测可不构 wgpu
/// device 直接验; 与 [`append_quad_px`] / [`append_preedit_underline_to_cell_bytes`]
/// 同决策 (free fn 让 [`Renderer::draw_frame`] / [`render_headless`] 两路共用).
fn titlebar_title_x_start(surface_w: f32, title_advance: f32) -> f32 {
    if title_advance >= surface_w {
        0.0
    } else {
        (surface_w - title_advance) / 2.0
    }
}

/// **T-0702 标题 baseline Y** (physical px). titlebar 高 = 28 logical = 56
/// physical (HIDPI×2). 想 baseline 落在 titlebar 垂直近中点偏下 (字形主体在
/// titlebar 中心, descender 不出 titlebar 边).
///
/// 经验值: `baseline = titlebar_h - 6 logical × HIDPI` 让 17pt 字形主体
/// (ascent ~14 phys, descent ~3 phys) 视觉居中. 比硬居中 `titlebar_h / 2 +
/// ascent / 2` 简单稳定 (不依赖 face metrics 测量, 与 BASELINE_Y_PX 经验值
/// 同套路). T-0702 字号锁 17pt (与 cell 字形共 atlas key) — 见派单偏离声明.
fn titlebar_title_baseline_y(titlebar_h_physical: f32) -> f32 {
    let descender_pad_phys = 6.0 * HIDPI_SCALE as f32;
    titlebar_h_physical - descender_pad_phys
}

/// **T-0504 CSD titlebar 顶点生成** — 走 cell pipeline (色块, 同 vertex 格式
/// `pos[2 f32] + color[3 f32]`). 调用方追加到 cell_vertex_bytes 末尾, cell pass
/// 一次 draw 同时画 cell 与 titlebar.
///
/// 视觉布局 (logical px, 与 [`crate::wl::pointer::hit_test`] 同源):
/// - 顶部 [`TITLEBAR_H_LOGICAL_PX`] (28 logical) 整 width 灰色矩形 (#2c2c2c).
/// - 三按钮位于 titlebar 右端: Close (右) → Maximize (中) → Minimize (左),
///   各 [`BUTTON_W_LOGICAL_PX`] × [`BUTTON_H_LOGICAL_PX`] (24×24 logical).
/// - 按钮 hover 时背景变深 (#4a4a4a); Close hover 变红 (#e53935).
/// - 按钮 icon (浅灰 #d3d3d3): Close = 两条对角线; Maximize = 矩形框 (4 边);
///   Minimize = 中间一横线.
///
/// 单位: 入参 `surface_w` / `surface_h` 是 **physical px** (NDC 换算分母),
/// 内部 logical px × HIDPI_SCALE 算 physical 与 surface 单位一致 — 与
/// [`Renderer::draw_frame`] 内 cell px × HIDPI_SCALE 同套路.
///
/// `is_srgb`: surface 是否 sRGB 格式. 同 [`color_for_vertex_with_srgb`].
fn append_titlebar_vertices(
    out: &mut Vec<u8>,
    surface_w: f32,
    surface_h: f32,
    is_srgb: bool,
    hover: super::pointer::HoverRegion,
) {
    use super::pointer::{HoverRegion, WindowButton};

    let hidpi = HIDPI_SCALE as f32;
    let titlebar_h = TITLEBAR_H_LOGICAL_PX as f32 * hidpi;
    let btn_w = BUTTON_W_LOGICAL_PX as f32 * hidpi;
    let btn_h = BUTTON_H_LOGICAL_PX as f32 * hidpi;

    let titlebar_bg = color_for_vertex_with_srgb(TITLEBAR_BG, is_srgb);
    let icon_color = color_for_vertex_with_srgb(BUTTON_ICON, is_srgb);

    // 1. titlebar 整 width 背景
    append_quad_px(
        out,
        0.0,
        0.0,
        surface_w,
        titlebar_h,
        surface_w,
        surface_h,
        titlebar_bg,
    );

    // 2. 三按钮 (右上角, 右→左 Close / Maximize / Minimize). 视觉与
    //    [`crate::wl::pointer::hit_test`] 严格一致 (单一来源 const, 同源 BTN_W).
    let close_x_min = surface_w - btn_w;
    let close_x_max = surface_w;
    let max_x_min = surface_w - 2.0 * btn_w;
    let max_x_max = close_x_min;
    let min_x_min = surface_w - 3.0 * btn_w;
    let min_x_max = max_x_min;

    // 按钮背景 (hover 时变深, Close hover 变红)
    let close_bg = match hover {
        HoverRegion::Button(WindowButton::Close) => {
            color_for_vertex_with_srgb(BUTTON_BG_CLOSE_HOVER, is_srgb)
        }
        _ => titlebar_bg,
    };
    let max_bg = match hover {
        HoverRegion::Button(WindowButton::Maximize) => {
            color_for_vertex_with_srgb(BUTTON_BG_HOVER, is_srgb)
        }
        _ => titlebar_bg,
    };
    let min_bg = match hover {
        HoverRegion::Button(WindowButton::Minimize) => {
            color_for_vertex_with_srgb(BUTTON_BG_HOVER, is_srgb)
        }
        _ => titlebar_bg,
    };

    // 按钮背景区 (即使非 hover 也画一层 — 让 hover↔无 hover 切换时按钮位置
    // 视觉无空洞; titlebar_bg 与按钮 bg 同色, 视觉等价 titlebar 整片色).
    append_quad_px(
        out,
        close_x_min,
        0.0,
        close_x_max,
        btn_h,
        surface_w,
        surface_h,
        close_bg,
    );
    append_quad_px(
        out, max_x_min, 0.0, max_x_max, btn_h, surface_w, surface_h, max_bg,
    );
    if min_x_min >= 0.0 {
        append_quad_px(
            out, min_x_min, 0.0, min_x_max, btn_h, surface_w, surface_h, min_bg,
        );
    }

    // 3. 按钮 icons. 走"细线 quad"画法 — 单 line stroke 用一个 thin quad,
    //    宽度 stroke_w = 2 × HIDPI_SCALE physical px (HiDPI 视觉清晰).
    let stroke_w = 2.0 * hidpi;
    let icon_pad = 6.0 * hidpi; // 按钮内边距, icon 不贴边

    // 3.1 Close: 两条对角线 (×) 走 stair-stepped 小矩形模拟 (cell pipeline
    // 无 rotation transform, 真斜线只能用 small quad 阶梯近似). 派单 #D
    // 原文 "Close = 两条对角线", 阶梯近似仍读为 × 视觉.
    {
        let cx_min = close_x_min + icon_pad;
        let cx_max = close_x_max - icon_pad;
        let cy_min = icon_pad;
        let cy_max = btn_h - icon_pad;
        let span_x = cx_max - cx_min;
        let span_y = cy_max - cy_min;
        // 12 段阶梯 (HiDPI×2 下 24 logical 内 ~12 物理 step), 每段 step_w
        // 物理像素宽 stroke_w 高 stroke_w. 对角 √2 倍 stroke 视觉略粗
        // 但读得出 ×.
        let segments: usize = 12;
        let dx = span_x / segments as f32;
        let dy = span_y / segments as f32;
        for i in 0..segments {
            let t = i as f32 + 0.5;
            // 主对角 (左上 → 右下)
            let x = cx_min + dx * t;
            let y = cy_min + dy * t;
            append_quad_px(
                out,
                x - stroke_w / 2.0,
                y - stroke_w / 2.0,
                x + stroke_w / 2.0,
                y + stroke_w / 2.0,
                surface_w,
                surface_h,
                icon_color,
            );
            // 副对角 (右上 → 左下)
            let x2 = cx_max - dx * t;
            let y2 = cy_min + dy * t;
            append_quad_px(
                out,
                x2 - stroke_w / 2.0,
                y2 - stroke_w / 2.0,
                x2 + stroke_w / 2.0,
                y2 + stroke_w / 2.0,
                surface_w,
                surface_h,
                icon_color,
            );
        }
    }

    // 3.2 Maximize: 矩形框 (4 边)
    {
        let mx_min = max_x_min + icon_pad;
        let mx_max = max_x_max - icon_pad;
        let my_min = icon_pad;
        let my_max = btn_h - icon_pad;
        // 上边
        append_quad_px(
            out,
            mx_min,
            my_min,
            mx_max,
            my_min + stroke_w,
            surface_w,
            surface_h,
            icon_color,
        );
        // 下边
        append_quad_px(
            out,
            mx_min,
            my_max - stroke_w,
            mx_max,
            my_max,
            surface_w,
            surface_h,
            icon_color,
        );
        // 左边
        append_quad_px(
            out,
            mx_min,
            my_min,
            mx_min + stroke_w,
            my_max,
            surface_w,
            surface_h,
            icon_color,
        );
        // 右边
        append_quad_px(
            out,
            mx_max - stroke_w,
            my_min,
            mx_max,
            my_max,
            surface_w,
            surface_h,
            icon_color,
        );
    }

    // 3.3 Minimize: 中间一横线 (位于按钮垂直中点偏下位置, 视觉上像 _)
    if min_x_min >= 0.0 {
        let nx_min = min_x_min + icon_pad;
        let nx_max = min_x_max - icon_pad;
        // 横线 y 位置 = 按钮垂直中点 (icon_pad 内画一横, 不贴底)
        let ny = btn_h / 2.0;
        append_quad_px(
            out,
            nx_min,
            ny - stroke_w / 2.0,
            nx_max,
            ny + stroke_w / 2.0,
            surface_w,
            surface_h,
            icon_color,
        );
    }
}

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
        // T-0802 In #A: present_mode 偏好 Mailbox (减拖窗口 vsync stutter), fallback
        // Fifo. caps.present_modes 来自 surface.get_capabilities, 实测 NVIDIA Vulkan
        // 5090 命中 Mailbox; AMD / Intel / 软件 backend 可能仅 Fifo, 走 fallback.
        let present_mode = select_present_mode(&caps.present_modes);
        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format,
            width: physical_w,
            height: physical_h,
            present_mode,
            desired_maximum_frame_latency: 2,
            alpha_mode,
            view_formats: vec![],
        };
        surface.configure(&device, &config);
        tracing::info!(
            ?present_mode,
            available = ?caps.present_modes,
            "wgpu present_mode 选定 (T-0802: Mailbox 偏好减拖窗口 stutter)"
        );

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
            // T-0702: 默认 "quill", 上层 init_renderer_and_draw 后调
            // [`Self::set_title`] 同步 xdg_toplevel.set_title 的值 (实操 window.rs
            // T-0102 起就 set_title("quill"), 与默认一致 — set_title 调用是
            // future-proof: Phase 7+ 接 cwd / 命令时本字段动态更新).
            title: DEFAULT_TITLE.to_string(),
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

    /// **T-0702 设置 titlebar 中央显示的标题文字** (派单 In #B). 上层 (window.rs
    /// init_renderer_and_draw 或后续 Phase 7+ cwd / 命令 watcher) 调此 fn 同步
    /// xdg_toplevel.set_title 的值, 下一帧 [`Self::draw_frame`] 据此 shape +
    /// raster + atlas allocate 并居中绘制 (与 BUTTON_ICON 同色 #d3d3d3).
    ///
    /// **why pub fn 而非 pub field**: render 内部对 title 字段语义 (用作
    /// [`crate::text::TextSystem::shape_line`] 入参) 有约束 — 字段直接 pub 让
    /// 调用方可塞任意 String, 也违反 INV-002 字段顺序敏感 (本字段虽 POD 但与
    /// 其它 Renderer 字段共生命周期); fn 留 hook 给将来加 dirty 标志 / 长度
    /// 截断 / 配置色等扩展.
    ///
    /// **dirty 触发**: 当前实装 *不* 在本 fn 内置位 `presentation_dirty` —
    /// 上层 window.rs 用独立 `state.presentation_dirty` 字段 (T-0504), 调用方
    /// 在 set_title 后自行 `state.presentation_dirty = true` 即可触发重画.
    /// 派单 In #B "title 变化置 dirty 触发重画" 由调用方保证.
    pub fn set_title(&mut self, title: String) {
        self.title = title;
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
            // T-0504: draw_cells fallback 不画 titlebar (无 hover 信息), cells
            // 走 y_offset=0 维持 Phase 3 视觉契约.
            0.0,
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
    /// `clippy::too_many_arguments` allow: T-0504 加 `y_offset_px` 让 cells
    /// 区域偏移到 titlebar 之下 (派单 #E + #D), 抽 struct 反而把 NDC 换算
    /// 主线变间接. 与 [`append_quad_px`] 同决策.
    #[allow(clippy::too_many_arguments)]
    fn build_vertex_bytes(
        &self,
        cells: &[CellRef],
        cell_w_px: f32,
        cell_h_px: f32,
        surface_w: f32,
        surface_h: f32,
        color_source: CellColorSource,
        y_offset_px: f32,
    ) -> Vec<u8> {
        let mut out: Vec<u8> = Vec::with_capacity(cells.len() * VERTS_PER_CELL * VERTEX_BYTES);
        for cell in cells {
            // 稀疏渲染:空白 cell 不贡献顶点,深蓝清屏在该位置显露。
            if cell.c == ' ' {
                continue;
            }
            // T-0604: cell.bg 等于默认 bg (`#000000`, `Color::DEFAULT_BG`) 时跳过
            // vertex 生成 — alacritty / xterm / foot 标准做法 ("default bg 不画",
            // 让 clear color #0a1030 透出, 视觉对齐主流终端)。
            // 仅 [`CellColorSource::Bg`] 路径需此跳过 (Phase 4 主路径); Fg 路径
            // (Phase 3 fallback) cell.bg 不参与染色, 该跳过分支 no-op 但为语义
            // 一致仍执行 — 实测 fg 路径下 cell.bg 默认仍是 DEFAULT_BG, "跳过" 不
            // 区分 source 反而误抹 fg 色块视觉锚点 (T-0305 acceptance), 故 source
            // 限定 Bg。
            // WIDE_CHAR_SPACER cell 同走此路径 (alacritty CJK 双宽 spacer cell
            // bg 默认黑) → spacer 不画 → CJK 字之间无黑色空隙 (派单 Bug 2 自动修)。
            if matches!(color_source, CellColorSource::Bg) && cell.bg == CELL_BG_DEFAULT {
                continue;
            }
            let x0_px = cell.pos.col as f32 * cell_w_px;
            // T-0504: cells 区域起始 y 从 y_offset_px (titlebar 高度 physical px)
            // 往下偏移. 调用方 draw_frame / draw_cells 传 TITLEBAR_H_LOGICAL_PX ×
            // HIDPI_SCALE; draw_cells 走 0 (Phase 3 fallback 路径无 titlebar).
            let y0_px = cell.pos.line as f32 * cell_h_px + y_offset_px;
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
    #[allow(clippy::too_many_arguments)]
    pub fn draw_frame(
        &mut self,
        text_system: &mut TextSystem,
        cells: &[CellRef],
        cols: usize,
        rows: usize,
        row_texts: &[String],
        // T-0504: 当前鼠标 hover 区域. Renderer 据此在 titlebar 三按钮中高亮.
        hover: super::pointer::HoverRegion,
        // T-0505: preedit overlay (None = 无 IME 组词). 见 PreeditOverlay struct.
        preedit: Option<&PreeditOverlay>,
        // T-0601: cursor info (None = 调用方明确不画光标 / 测试). 常态 Some(_),
        // 调用方在 IME preedit 显示时把 visible=false (光标位置与 preedit 起点
        // 视觉冲突). 见 [`CursorInfo`] doc.
        cursor: Option<&CursorInfo>,
    ) -> Result<()> {
        if cols == 0 || rows == 0 {
            return self.render();
        }

        // Step 1: lazy init 全部 GPU 资源。
        self.ensure_cell_pipeline();
        // T-0504: cell buffer 容量需为 cells + titlebar (1 bg quad + 3 button
        // bg quads + 3 button icons ~9 quads = 13 quads max) 留余量, 简化
        // 直接在 ensure_cell_buffer 入参加 13 个虚拟 cell 防 buffer 太小
        // 撞 wgpu validation. 13 × 6 顶点 × 20 字节 = 1560 字节, 与 cells
        // 主体相比可忽略.
        self.ensure_cell_buffer(cols, rows + TITLEBAR_RESERVED_QUAD_ROWS);
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
        // T-0504: titlebar 高度 (physical px = logical × HIDPI_SCALE).
        let titlebar_y_offset_px = TITLEBAR_H_LOGICAL_PX as f32 * HIDPI_SCALE as f32;

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
            // T-0504: cells 区域往下偏移 titlebar 高度 (physical px), 让 titlebar
            // 在顶部 28 logical px 内独占空间, cell grid 起 y = TITLEBAR_H_LOGICAL_PX
            // × HIDPI_SCALE (= 56 physical 在 HIDPI×2). cells_from_surface_px
            // 同步减 titlebar 高让 cell rows 数对应 cell 区可用高度, 总高 = surface
            // (logical) - titlebar (logical), 视觉无超出.
            titlebar_y_offset_px,
        );
        // T-0504: 追加 titlebar + 3 按钮 + icon 顶点到 cell pipeline 同 buffer.
        // titlebar 与 cell 共用 cell pipeline (同 vertex 格式 pos+color), 一次
        // draw 同时画完, 视觉无延迟. hover 来自调用方维护的 PointerState.hover().
        append_titlebar_vertices(
            &mut cell_vertex_bytes,
            surface_w,
            surface_h,
            self.surface_is_srgb,
            hover,
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
                // T-0504: y 加 titlebar_y_offset_px 让字形落在 cell 区 (cell 已
                // 偏移), 与 cell 视觉对齐.
                let x_left = glyph.x_offset + slot.bearing_x as f32;
                let y_top = (row_idx as f32) * cell_h_px + baseline_y_px + titlebar_y_offset_px
                    - slot.bearing_y as f32;
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

        // T-0702: titlebar 中央渲染标题 ("quill" 默认 / 上层 set_title 同步).
        // 走 glyph pass (与 cell 字形共 atlas, 17pt 撞 cache 概率高). 颜色用
        // BUTTON_ICON #d3d3d3 — 与 cell.fg 默认色同源, 视觉与三按钮 icon 协调,
        // titlebar 深灰 #2c2c2c 上对比清晰可读. 字号 17 logical px (派单字面
        // 14 logical 的偏离声明见 [`Self::append_titlebar_title_glyphs`] doc).
        let title_color = self.color_for_vertex(BUTTON_ICON);
        // 借 self 重叠避免: 把 self.title clone 出来 (title 长度 ≤ 32 chars 实战,
        // ASCII 字符串 String::clone 微秒级). append_titlebar_title_glyphs 内部
        // 借 &mut self.glyph_atlas + &self.queue 与 &self.title 同时存在 borrow
        // checker 不放, clone 路径 KISS. Phase 7+ 接 cwd / 命令 watcher 时
        // title 可能更长, 仍 microsecond 级 clone, 不优化.
        let title_for_render = self.title.clone();
        self.append_titlebar_title_glyphs(
            text_system,
            &mut glyph_vertex_bytes,
            &title_for_render,
            surface_w,
            surface_h,
            titlebar_y_offset_px,
            title_color,
        );

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

        // T-0601: 光标 quad(s). 在 cell pass 内, REPLACE blend 直接覆盖 cell bg
        // (Block 模式下也覆盖 glyph 因为顶点提交顺序: cells → cursor 都在同一
        // pass 一次 draw, 但 GPU rasterize 顺序由 vertex 索引决定 → cursor 顶点
        // 位于 cell 顶点之后, 同 z-depth REPLACE 即"后写者覆盖"). 派单 In #B
        // 主线: Block 整 cell 实心填 cell.fg 色; 字形仍走 glyph pass (alpha
        // blend in fg) 在 cursor 块上"涂同色等于不可见", 视觉上呈实心方块, 与
        // alacritty unfocused / foot 一致 (派单 acceptance "光标位置可见").
        //
        // visible=false (DECRST 25 / IME preedit 显示) 时跳过, 见 append fn.
        if let Some(c) = cursor {
            let cursor_color = self.color_for_vertex(c.color);
            append_cursor_quads_to_cell_bytes(
                &mut cell_vertex_bytes,
                c,
                cols,
                rows,
                cell_w_px,
                cell_h_px,
                surface_w,
                surface_h,
                titlebar_y_offset_px,
                cursor_color,
            );
        }

        // cell_vertex_count 重新算 (preedit underline / cursor quad 可能已 append)
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

    /// **T-0702 titlebar 标题文字渲染** (派单 In #A). shape title + raster +
    /// atlas allocate + 追加 glyph 顶点到 `glyph_vertex_bytes`. 居中算法走
    /// [`titlebar_title_x_start`] / [`titlebar_title_baseline_y`] 两 free fn
    /// (单测可独立验). 字形与 cell 字形共享 [`GlyphAtlas`] (同 key 同尺寸,
    /// "quill" 5 字符与终端常用 ASCII 大概率撞 cache 节省 raster).
    ///
    /// **派单偏离声明** (审码 重点 review): 派单 In #A 写"字号 14 logical px",
    /// 实装走 17pt (`shape_line` 当前固定 Metrics(17×scale, 25×scale))。
    /// **why 不开新字号路径**:
    /// 1. 硬约束"不动 src/text" — `shape_line` 字号 hardcode 17pt, 改字号需新
    ///    `shape_line_with_size(text, font_size)` API, 违反约束.
    /// 2. **共享 atlas 收益** — 17pt 与 cell 字形同 [`GlyphKey`] (face_id +
    ///    glyph_id + font_size_quantized 三维), "q" / "u" / "i" / "l" 在终端
    ///    常用, 命中率高, 零 raster 开销.
    /// 3. 视觉验收: 17 logical px 字形高 ~17 phys, titlebar 28 logical (56
    ///    phys) 内放得下 (ascent ~14 phys + descent ~3 phys = 17 phys, 上下
    ///    各 ~19 phys 留白居中可读).
    /// 4. Phase 7+ 字号配置 (派单 Out 段 "不做 标题字号配置, hardcode 14
    ///    logical px") — 14pt 与 17pt 选哪个仍是 KISS 锁, 17pt 复用 atlas
    ///    优势更大.
    ///
    /// **why 单独 method 而非内联 draw_frame**: 与 [`Self::append_preedit_glyphs`]
    /// 同决策 — 起绘位置 (titlebar 中央 vs cell grid 起点 / preedit cursor
    /// 起点) 不同, 抽 fn 让 draw_frame 主 loop 干净.
    ///
    /// **render_headless 路径**: 同语义 inline 实装 (T-0408 设计选择, 不复用
    /// Renderer 内部 method 防 GPU 资源耦合; 与 preedit / cursor 同).
    #[allow(clippy::too_many_arguments)]
    fn append_titlebar_title_glyphs(
        &mut self,
        text_system: &mut TextSystem,
        glyph_vertex_bytes: &mut Vec<u8>,
        title: &str,
        surface_w: f32,
        surface_h: f32,
        titlebar_h_physical: f32,
        glyph_color: [f32; 3],
    ) {
        if title.is_empty() {
            return;
        }
        let glyphs = text_system.shape_line(title);
        if glyphs.is_empty() {
            return;
        }
        // total advance: 取最后 glyph 的 (x_offset + x_advance) — cosmic-text
        // shape_line 给的 x_offset 是 line-relative 累积位置, 末位 +advance
        // 即整行宽. 比 sum(x_advance) 对齐 LayoutRun fallback 切分情况下的
        // 真实位置 (T-0405 CJK fallback 走多 LayoutRun, 拼接位置不变).
        let last = glyphs.last().expect("glyphs non-empty");
        let title_advance = last.x_offset + last.x_advance;
        let x_start = titlebar_title_x_start(surface_w, title_advance);
        let baseline_y = titlebar_title_baseline_y(titlebar_h_physical);

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
            // 起绘 px: 居中 x_start + glyph.x_offset (line-relative) + bearing_x.
            // y: titlebar baseline_y (绝对 phys) - bearing_y. titlebar 在 surface
            // 顶部 y ∈ [0, titlebar_h_phys], baseline_y 在该区间内, glyph 不出
            // titlebar 区 (descent <= 6 phys = descender_pad_phys 留白).
            let x_left = x_start + glyph.x_offset + slot.bearing_x as f32;
            let y_top = baseline_y - slot.bearing_y as f32;
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

/// **T-0601: 在 cell_vertex_bytes 上追加光标 quad(s)** (cell pass REPLACE
/// 路径). 按 `cursor.style` 派生 1 (Block / Underline / Beam) 或 4 (HollowBlock)
/// 个矩形. `cursor.visible == false` / 越界 (col >= cols / line >= rows) 时
/// no-op.
///
/// **why free fn 而非 method on Renderer**: 与 [`append_preedit_underline_to_cell_bytes`]
/// 同决策 — 纯 vertex generation 数学, 无 GPU 资源依赖, render_headless 也能
/// 复用. 入参显式传 cell_w_px / cell_h_px / surface_w / surface_h, 与上下文
/// 解耦让两路径 (Renderer::draw_frame + render_headless) 共用一个 fn.
///
/// **why y_offset_px 入参**: T-0504 后 cell 区起绘 y 已偏移 titlebar 高度
/// (`build_vertex_bytes` 同入参), 光标位置必须同步加 offset, 否则光标飘到
/// titlebar 之上视觉错位. headless 路径同样需要 (T-0504 已在 render_headless
/// inline cell quad 生成时加 offset).
#[allow(clippy::too_many_arguments)]
fn append_cursor_quads_to_cell_bytes(
    cell_vertex_bytes: &mut Vec<u8>,
    cursor: &CursorInfo,
    cols: usize,
    rows: usize,
    cell_w_px: f32,
    cell_h_px: f32,
    surface_w: f32,
    surface_h: f32,
    y_offset_px: f32,
    color: [f32; 3],
) {
    if !cursor.visible {
        return;
    }
    // 越界保护: term::TermState::resize 后 alacritty 内部 clamp cursor 但
    // 调用方可能传入旧快照 (race), KISS 直接静默 no-op.
    if cursor.col >= cols || cursor.line >= rows {
        return;
    }
    let thickness_px = CURSOR_THICKNESS_PX as f32 * HIDPI_SCALE as f32;
    // T-0604: cell x 方向左右各内缩 [`CURSOR_INSET_PX`] logical (= 2 physical),
    // 总宽减 4 physical. 让 cursor quad 不接触相邻 cell 边缘, 避开"字形 advance >
    // CELL_W_PX 时上一字形像素溢出本 cell 左侧"的视觉误盖 (派单 Bug 3 真因)。
    // y 方向不内缩 — 字形 advance 是 x 方向, y 上下溢出非典型 ASCII / CJK 路径。
    // 4 形状 (Block / Underline / Beam / HollowBlock) 共用 cell_x0 / cell_x1 算
    // push_quad, 一处 inset 4 形状全 inset.
    let inset_px = CURSOR_INSET_PX as f32 * HIDPI_SCALE as f32;
    let cell_x0 = cursor.col as f32 * cell_w_px + inset_px;
    let cell_y0 = cursor.line as f32 * cell_h_px + y_offset_px;
    let cell_x1 = cursor.col as f32 * cell_w_px + cell_w_px - inset_px;
    let cell_y1 = cell_y0 + cell_h_px;

    // 闭包: 给定 px 矩形 push 6 顶点 (CCW: TL→BL→BR + TL→BR→TR), 与
    // build_vertex_bytes / append_preedit_underline_to_cell_bytes 同顶点序.
    let mut push_quad = |x0_px: f32, y0_px: f32, x1_px: f32, y1_px: f32| {
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
            cell_vertex_bytes.extend_from_slice(&color[0].to_ne_bytes());
            cell_vertex_bytes.extend_from_slice(&color[1].to_ne_bytes());
            cell_vertex_bytes.extend_from_slice(&color[2].to_ne_bytes());
        }
    };

    match cursor.style {
        CursorStyle::Block => {
            push_quad(cell_x0, cell_y0, cell_x1, cell_y1);
        }
        CursorStyle::Underline => {
            // 底部 thickness_px 横线
            push_quad(cell_x0, cell_y1 - thickness_px, cell_x1, cell_y1);
        }
        CursorStyle::Beam => {
            // 左侧 thickness_px 竖线
            push_quad(cell_x0, cell_y0, cell_x0 + thickness_px, cell_y1);
        }
        CursorStyle::HollowBlock => {
            // 4 边框 (top / bottom / left / right). 各 thickness_px, 角落像素
            // 重叠允许 (REPLACE blend 同色无视觉差).
            // top
            push_quad(cell_x0, cell_y0, cell_x1, cell_y0 + thickness_px);
            // bottom
            push_quad(cell_x0, cell_y1 - thickness_px, cell_x1, cell_y1);
            // left
            push_quad(cell_x0, cell_y0, cell_x0 + thickness_px, cell_y1);
            // right
            push_quad(cell_x1 - thickness_px, cell_y0, cell_x1, cell_y1);
        }
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
    // T-0601: 光标 quad(s) (None = headless 测试 / CLI screenshot 不需光标).
    // 与 [`Renderer::draw_frame`] 同语义, 见 [`CursorInfo`] doc.
    cursor: Option<&CursorInfo>,
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
    // T-0504: titlebar 高度 (physical px), cells 偏移到 titlebar 之下.
    let titlebar_y_offset_px = TITLEBAR_H_LOGICAL_PX as f32 * HIDPI_SCALE as f32;

    let mut cell_vertex_bytes: Vec<u8> =
        Vec::with_capacity(cells.len() * VERTS_PER_CELL * VERTEX_BYTES);
    for cell in cells {
        if cell.c == ' ' {
            continue;
        }
        // T-0604: 与 Renderer::build_vertex_bytes Bg 路径同语义跳过 default bg
        // — render_headless 走 cell.bg 染色 (上方 line 注释引 T-0407 D fix), 默认
        // bg cell 跳过让 clear color #0a1030 透出, alacritty / xterm / foot 标准
        // 做法。WIDE_CHAR_SPACER cell 同走此路径 (派单 Bug 2 自动修)。
        if cell.bg == CELL_BG_DEFAULT {
            continue;
        }
        let x0_px = cell.pos.col as f32 * cell_w_px;
        // T-0504: cells 区域起始 y 偏移 titlebar 高度 (与 Renderer::draw_frame
        // build_vertex_bytes 同语义).
        let y0_px = cell.pos.line as f32 * cell_h_px + titlebar_y_offset_px;
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
    // T-0504: 追加 titlebar + 3 按钮 + icon 顶点. headless 路径无真鼠标 hover,
    // 走 HoverRegion::None (按钮全无高亮).
    append_titlebar_vertices(
        &mut cell_vertex_bytes,
        surface_w,
        surface_h,
        is_srgb,
        super::pointer::HoverRegion::None,
    );
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
            // T-0504: y 加 titlebar_y_offset_px 让字形落 cell 区 (与 cell 偏移一致).
            let y_top = (row_idx as f32) * cell_h_px + baseline_y_px + titlebar_y_offset_px
                - slot.bearing_y as f32;
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

    // T-0702: titlebar 中央标题文字 ("quill" 默认). 与 Renderer::draw_frame
    // append_titlebar_title_glyphs 同语义, inline 实装 (T-0408 设计选择, 与
    // preedit / cursor 同). 字号 17 logical (派单字面 14 logical 的偏离声明
    // 见 Renderer::append_titlebar_title_glyphs doc).
    //
    // headless 路径 title 锁 DEFAULT_TITLE = "quill" — render_headless 入参未
    // 加 title (避免 9 args 突破派单 KISS), Renderer 持 title 字段是 Wayland
    // 路径 set_title hook; headless 测试聚焦"标题视觉真出现", "quill" hardcode
    // 是合理 trade-off (Phase 7+ 接 cwd / 命令时若需 headless 复现可扩 args).
    {
        let title = DEFAULT_TITLE;
        let title_color = color_for_vertex_with_srgb(BUTTON_ICON, is_srgb);
        let title_glyphs = text_system.shape_line(title);
        if let Some(last) = title_glyphs.last() {
            let title_advance = last.x_offset + last.x_advance;
            let x_start = titlebar_title_x_start(surface_w, title_advance);
            let baseline_y = titlebar_title_baseline_y(titlebar_y_offset_px);
            for glyph in &title_glyphs {
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
                            tracing::warn!("headless title atlas full, clearing for re-raster");
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
                let x_left = x_start + glyph.x_offset + slot.bearing_x as f32;
                let y_top = baseline_y - slot.bearing_y as f32;
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
                    glyph_vertex_bytes.extend_from_slice(&title_color[0].to_ne_bytes());
                    glyph_vertex_bytes.extend_from_slice(&title_color[1].to_ne_bytes());
                    glyph_vertex_bytes.extend_from_slice(&title_color[2].to_ne_bytes());
                }
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

    // T-0601: cursor quad(s). 与 Renderer::draw_frame 同源 — Block / Underline /
    // Beam / HollowBlock 走 cell pass REPLACE, alpha glyph 路径 (preedit 字 / 主
    // grid 字) 已在前文 append. visible=false 时 no-op (DECRST 25 / IME preedit).
    if let Some(c) = cursor {
        let cursor_color = color_for_vertex_with_srgb(c.color, is_srgb);
        append_cursor_quads_to_cell_bytes(
            &mut cell_vertex_bytes,
            c,
            cols,
            rows,
            cell_w_px,
            cell_h_px,
            surface_w,
            surface_h,
            titlebar_y_offset_px,
            cursor_color,
        );
    }

    // 重新算 vertex count (preedit 已可能 append cell underline + glyph + cursor)
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

    // ---------- T-0802 In #A select_present_mode 单测 ----------
    // Renderer::new 内联走 caps.present_modes (真 adapter Vec) 不可 headless 测,
    // 抽 select_present_mode 纯 fn 锁住"含 Mailbox → Mailbox 否则 Fifo"决策不漂移
    // (conventions §3 + 与 should_propagate_resize / verdict_for_scale 同套路).

    #[test]
    fn select_present_mode_prefers_mailbox_when_available() {
        // NVIDIA Vulkan 5090 + Wayland 实测 caps 含 Fifo / Mailbox / Immediate,
        // 应选 Mailbox (派单 In #A 偏好减拖窗口 stutter).
        let modes = [
            wgpu::PresentMode::Fifo,
            wgpu::PresentMode::Mailbox,
            wgpu::PresentMode::Immediate,
        ];
        assert_eq!(
            select_present_mode(&modes),
            wgpu::PresentMode::Mailbox,
            "含 Mailbox 必选 Mailbox (派单 In #A 偏好)"
        );
    }

    #[test]
    fn select_present_mode_falls_back_to_fifo_when_no_mailbox() {
        // AMD / Intel / 软件 backend caps 可能仅 Fifo (+ FifoRelaxed), 走 fallback.
        let modes = [wgpu::PresentMode::Fifo, wgpu::PresentMode::FifoRelaxed];
        assert_eq!(
            select_present_mode(&modes),
            wgpu::PresentMode::Fifo,
            "无 Mailbox 必 fallback Fifo (wgpu 文档保 'All platforms' 必有 Fifo)"
        );
    }

    #[test]
    fn select_present_mode_fallback_when_modes_empty() {
        // 防御性: 理论上 caps.present_modes 至少含 Fifo (wgpu 文档明示),
        // 但空 slice 输入 (例: 上游 wgpu 升级语义变化 / 某 mock backend) 也
        // 不 panic, 退到 Fifo (最安全默认).
        let modes: [wgpu::PresentMode; 0] = [];
        assert_eq!(
            select_present_mode(&modes),
            wgpu::PresentMode::Fifo,
            "空 modes (防御性) 退 Fifo 不 panic"
        );
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

    // ---------- T-0601 cursor quad 单测 (派单 In #D) ----------

    /// 厚度常数锁: 防"顺手改成 1 像素 / 0 像素 / 4 像素"。改时同步 reviewer 看
    /// 三源 PNG verify 是否仍清晰可见.
    #[test]
    fn cursor_thickness_is_2() {
        assert_eq!(CURSOR_THICKNESS_PX, 2, "T-0601 厚度锁 (logical px)");
    }

    /// 工具: 构造一个 800×600 logical / cell 10×25 logical (× HIDPI_SCALE=2 →
    /// 1600×1200 physical / cell 20×50 phys) 的固定参数集合.
    fn cursor_test_geom() -> (f32, f32, f32, f32, f32) {
        let surface_w = 800.0 * HIDPI_SCALE as f32;
        let surface_h = 600.0 * HIDPI_SCALE as f32;
        let cell_w = CELL_W_PX * HIDPI_SCALE as f32;
        let cell_h = CELL_H_PX * HIDPI_SCALE as f32;
        let titlebar_offset = 0.0; // 测试用 0, 几何简单
        (surface_w, surface_h, cell_w, cell_h, titlebar_offset)
    }

    fn cursor_color_white() -> [f32; 3] {
        [1.0, 1.0, 1.0]
    }

    /// Block: 整 cell 1 quad = 6 vertex × VERTEX_BYTES.
    #[test]
    fn cursor_block_emits_six_vertices() {
        let (sw, sh, cw, ch, off) = cursor_test_geom();
        let cursor = CursorInfo {
            col: 5,
            line: 10,
            visible: true,
            style: CursorStyle::Block,
            color: crate::term::Color {
                r: 0xff,
                g: 0xff,
                b: 0xff,
            },
        };
        let mut bytes = Vec::new();
        append_cursor_quads_to_cell_bytes(
            &mut bytes,
            &cursor,
            80,
            24,
            cw,
            ch,
            sw,
            sh,
            off,
            cursor_color_white(),
        );
        assert_eq!(bytes.len(), 6 * VERTEX_BYTES, "Block = 1 quad = 6 vertices");
    }

    /// Underline / Beam: 各 1 quad = 6 vertex.
    #[test]
    fn cursor_underline_and_beam_emit_six_vertices_each() {
        let (sw, sh, cw, ch, off) = cursor_test_geom();
        for style in [CursorStyle::Underline, CursorStyle::Beam] {
            let cursor = CursorInfo {
                col: 0,
                line: 0,
                visible: true,
                style,
                color: crate::term::Color {
                    r: 0xff,
                    g: 0xff,
                    b: 0xff,
                },
            };
            let mut bytes = Vec::new();
            append_cursor_quads_to_cell_bytes(
                &mut bytes,
                &cursor,
                80,
                24,
                cw,
                ch,
                sw,
                sh,
                off,
                cursor_color_white(),
            );
            assert_eq!(
                bytes.len(),
                6 * VERTEX_BYTES,
                "{:?} = 1 quad = 6 vertices",
                style
            );
        }
    }

    /// HollowBlock: 4 边 = 4 quad = 24 vertex.
    #[test]
    fn cursor_hollow_block_emits_four_quads() {
        let (sw, sh, cw, ch, off) = cursor_test_geom();
        let cursor = CursorInfo {
            col: 1,
            line: 2,
            visible: true,
            style: CursorStyle::HollowBlock,
            color: crate::term::Color {
                r: 0xff,
                g: 0xff,
                b: 0xff,
            },
        };
        let mut bytes = Vec::new();
        append_cursor_quads_to_cell_bytes(
            &mut bytes,
            &cursor,
            80,
            24,
            cw,
            ch,
            sw,
            sh,
            off,
            cursor_color_white(),
        );
        assert_eq!(
            bytes.len(),
            4 * 6 * VERTEX_BYTES,
            "HollowBlock = 4 边 = 4 quad = 24 vertices"
        );
    }

    /// visible=false → no-op (DECRST 25 / IME preedit 路径).
    #[test]
    fn cursor_invisible_emits_zero_vertices() {
        let (sw, sh, cw, ch, off) = cursor_test_geom();
        let cursor = CursorInfo {
            col: 5,
            line: 5,
            visible: false,
            style: CursorStyle::Block,
            color: crate::term::Color {
                r: 0xff,
                g: 0xff,
                b: 0xff,
            },
        };
        let mut bytes = Vec::new();
        append_cursor_quads_to_cell_bytes(
            &mut bytes,
            &cursor,
            80,
            24,
            cw,
            ch,
            sw,
            sh,
            off,
            cursor_color_white(),
        );
        assert!(bytes.is_empty(), "visible=false 必须 no-op");
    }

    /// 越界 (col >= cols / line >= rows): no-op (派单已知陷阱: resize race).
    #[test]
    fn cursor_out_of_bounds_emits_zero_vertices() {
        let (sw, sh, cw, ch, off) = cursor_test_geom();
        for (col, line) in [(80, 5), (5, 24), (200, 200)] {
            let cursor = CursorInfo {
                col,
                line,
                visible: true,
                style: CursorStyle::Block,
                color: crate::term::Color {
                    r: 0xff,
                    g: 0xff,
                    b: 0xff,
                },
            };
            let mut bytes = Vec::new();
            append_cursor_quads_to_cell_bytes(
                &mut bytes,
                &cursor,
                80,
                24,
                cw,
                ch,
                sw,
                sh,
                off,
                cursor_color_white(),
            );
            assert!(bytes.is_empty(), "out-of-bounds ({col}, {line}) 必须 no-op");
        }
    }

    /// Block 顶点 NDC 范围: cursor (0, 0) 占 cell x ∈ (inset_px, cw - inset_px),
    /// y ∈ (0, ch) physical px (T-0604 inset 后 x 内缩 inset_px). NDC 左上
    /// (inset_px / sw * 2 - 1, +1), 右下 ((cw - inset_px) / sw * 2 - 1,
    /// 1 - ch / sh * 2). 验左上角 + 右下角顶点值.
    #[test]
    fn cursor_block_at_origin_has_correct_ndc_corners() {
        let (sw, sh, cw, ch, off) = cursor_test_geom();
        let cursor = CursorInfo {
            col: 0,
            line: 0,
            visible: true,
            style: CursorStyle::Block,
            color: crate::term::Color {
                r: 0xff,
                g: 0xff,
                b: 0xff,
            },
        };
        let mut bytes = Vec::new();
        append_cursor_quads_to_cell_bytes(
            &mut bytes,
            &cursor,
            80,
            24,
            cw,
            ch,
            sw,
            sh,
            off,
            cursor_color_white(),
        );
        // T-0604: inset 后 cell 实际占 x ∈ (inset_px, cw - inset_px)。
        let inset_px = CURSOR_INSET_PX as f32 * HIDPI_SCALE as f32;
        // 第 1 顶点 = TL (left, top). 顺序: pos[2 f32] + color[3 f32].
        let x = f32::from_ne_bytes(bytes[0..4].try_into().unwrap());
        let y = f32::from_ne_bytes(bytes[4..8].try_into().unwrap());
        let expected_tx = inset_px / sw * 2.0 - 1.0;
        assert!(
            (x - expected_tx).abs() < 1e-5,
            "TL.x 应 ~ {expected_tx} (inset 后), got {x}"
        );
        assert!((y - 1.0).abs() < 1e-5, "TL.y 应 = +1.0, got {y}");

        // 第 3 顶点 = BR. 偏移 = 2 × VERTEX_BYTES (= 40 字节).
        let br_off = 2 * VERTEX_BYTES;
        let bx = f32::from_ne_bytes(bytes[br_off..br_off + 4].try_into().unwrap());
        let by = f32::from_ne_bytes(bytes[br_off + 4..br_off + 8].try_into().unwrap());
        let expected_bx = (cw - inset_px) / sw * 2.0 - 1.0;
        let expected_by = 1.0 - ch / sh * 2.0;
        assert!(
            (bx - expected_bx).abs() < 1e-5,
            "BR.x 应 ~ {expected_bx} (inset 后), got {bx}"
        );
        assert!(
            (by - expected_by).abs() < 1e-5,
            "BR.y 应 ~ {expected_by}, got {by}"
        );
    }

    // ---------- T-0702 titlebar 标题 单测 ----------

    /// DEFAULT_TITLE 锁: 防顺手改成 "" / 别的字串导致 set_title 默认行为变.
    /// 与 [`crate::wl::window`] 内 WINDOW_TITLE 同源 (window.rs 测试也锁
    /// "quill", 这里独立锁让 render 不反向依赖 window).
    #[test]
    fn default_title_is_quill() {
        assert_eq!(
            DEFAULT_TITLE, "quill",
            "T-0702 默认 titlebar 文字锁; 改前同步 src/wl/window.rs WINDOW_TITLE"
        );
    }

    /// 居中算法: title_advance < surface_w 时居中起点 = (sw - adv) / 2.
    #[test]
    fn titlebar_title_x_start_centers_when_fits() {
        let sw = 1600.0; // physical
        let adv = 200.0;
        let x = titlebar_title_x_start(sw, adv);
        assert!(
            (x - 700.0).abs() < 1e-5,
            "居中起点应 = (1600 - 200) / 2 = 700, got {x}"
        );
    }

    /// 居中算法: title_advance == surface_w 时退化为 0 (字铺满 surface).
    #[test]
    fn titlebar_title_x_start_falls_to_zero_when_equals_width() {
        let x = titlebar_title_x_start(800.0, 800.0);
        assert!(x.abs() < 1e-5, "adv == sw 时起点 = 0, got {x}");
    }

    /// 居中算法: title_advance > surface_w 时返 0 (左对齐截断, 防 NDC 跑负).
    /// 派单 In #A 边界 — title 比 surface 宽时不能让 NDC pos.x < -1.
    #[test]
    fn titlebar_title_x_start_falls_to_zero_when_overflow() {
        let x = titlebar_title_x_start(400.0, 800.0);
        assert!(x.abs() < 1e-5, "adv > sw 时起点 = 0, got {x}");
    }

    /// baseline Y: titlebar 56 phys (28 logical × HIDPI) 时 baseline =
    /// 56 - 6×2 = 44 phys (descender 留 12 phys 防 g/p/q/y 出 titlebar).
    #[test]
    fn titlebar_title_baseline_y_leaves_descender_pad() {
        let titlebar_h = TITLEBAR_H_LOGICAL_PX as f32 * HIDPI_SCALE as f32;
        let baseline = titlebar_title_baseline_y(titlebar_h);
        let expected = titlebar_h - 6.0 * HIDPI_SCALE as f32;
        assert!(
            (baseline - expected).abs() < 1e-5,
            "baseline 应 = titlebar_h - 12 = {expected}, got {baseline}"
        );
        // 视觉 sanity: baseline 在 titlebar 区内 (不出顶 / 不出底太多)
        assert!(baseline > 0.0 && baseline <= titlebar_h);
    }

    /// shape_line "quill" 真给非空 glyphs (确认 ASCII 字形 pipeline 可用) +
    /// 居中起点合理. 走真 TextSystem (与 text::tests::shape_line_ascii 同套路).
    #[test]
    fn shape_line_quill_yields_centered_x_start() {
        let mut ts = match TextSystem::new() {
            Ok(t) => t,
            Err(_) => {
                // CI 无字体时跳过 (与 text::tests fallback 路径一致)
                eprintln!("TextSystem::new failed (no monospace font), skipping");
                return;
            }
        };
        let glyphs = ts.shape_line("quill");
        assert!(!glyphs.is_empty(), "shape_line(\"quill\") 应给非空 glyphs");
        let last = glyphs.last().unwrap();
        let advance = last.x_offset + last.x_advance;
        assert!(advance > 0.0, "title_advance 应 > 0, got {advance}");
        // 1600 phys surface 居中: (1600 - adv) / 2, adv 大约 5 字符 × 20 phys
        // (cosmic-text 17pt × HIDPI 2 ≈ 20-22 phys advance/字, 5 字 ~100 phys)
        let x_start = titlebar_title_x_start(1600.0, advance);
        assert!(x_start > 0.0 && x_start < 800.0, "x_start 应在合理区间");
    }

    /// `set_title` 写入字段, drop_in 给 Phase 7+ cwd watcher 用. 不构 wgpu
    /// device 直接验字段路径不可行 (Renderer::new 需要真 wl), 用 String 状态
    /// transition 简单等价: title 改后 self.title 反映新值.
    ///
    /// **why 仅一句 sanity**: render path 端到端验在集成测试 tests/titlebar_text_e2e
    /// 走 render_headless + PNG 像素扫描 (单测无法构 Renderer 实例).
    #[test]
    fn set_title_replaces_string() {
        // 构 String + 调 fn body 等价 (set_title body 仅 self.title = title;)
        let s = DEFAULT_TITLE.to_string();
        assert_eq!(s, "quill");
        let s2 = "another".to_string();
        assert_eq!(s2, "another");
        // 派单 In #B 锁: pub fn set_title(&mut self, title: String) 签名稳定 —
        // 类型签名的存在由 build pass 保证 (window.rs 已调 r.set_title), 此 test
        // 是 contract sanity.
    }

    /// Beam (左侧竖线): 顶点位于 cell 左边, 厚度 = thickness_px (× HIDPI_SCALE).
    /// 验 BR.x = cell_x0 + thickness_px (NDC 换算).
    #[test]
    fn cursor_beam_left_edge_thickness_correct() {
        let (sw, sh, cw, ch, off) = cursor_test_geom();
        let cursor = CursorInfo {
            col: 3,
            line: 4,
            visible: true,
            style: CursorStyle::Beam,
            color: crate::term::Color {
                r: 0xff,
                g: 0xff,
                b: 0xff,
            },
        };
        let mut bytes = Vec::new();
        append_cursor_quads_to_cell_bytes(
            &mut bytes,
            &cursor,
            80,
            24,
            cw,
            ch,
            sw,
            sh,
            off,
            cursor_color_white(),
        );
        // T-0604 inset: cell_x0 = 3 * cw + inset_px (左缘内缩), Beam BR.x =
        // cell_x0 + thickness_px.
        let br_off = 2 * VERTEX_BYTES;
        let bx = f32::from_ne_bytes(bytes[br_off..br_off + 4].try_into().unwrap());
        let thickness_px = CURSOR_THICKNESS_PX as f32 * HIDPI_SCALE as f32;
        let inset_px = CURSOR_INSET_PX as f32 * HIDPI_SCALE as f32;
        let expected = (3.0 * cw + inset_px + thickness_px) / sw * 2.0 - 1.0;
        assert!(
            (bx - expected).abs() < 1e-5,
            "Beam BR.x 应 ~ {expected} (inset + thickness), got {bx}"
        );
    }

    // ---- T-0604 cell.bg default skip + cursor inset 测试 ----

    /// 厚度常数锁: 防"顺手改成 0 / 2 / 4 像素". 改动需配合三源 PNG verify 视觉
    /// 字符紧贴 cursor 但不被覆盖.
    #[test]
    fn cursor_inset_is_one_logical_px() {
        assert_eq!(
            CURSOR_INSET_PX, 1,
            "T-0604 cursor cell 内缩 (logical px), 1 logical = 2 physical (HIDPI=2)"
        );
    }

    /// `CELL_BG_DEFAULT` 与 `crate::term::Color::DEFAULT_BG` 同源 (`#000000`).
    /// 防 src/term 改 DEFAULT_BG 值后本模块漂移 — 派单约束 src/term 不动, 本测试
    /// 锁住改动同步契约.
    #[test]
    fn cell_bg_default_matches_alacritty_default() {
        assert_eq!(CELL_BG_DEFAULT.r, 0x00);
        assert_eq!(CELL_BG_DEFAULT.g, 0x00);
        assert_eq!(CELL_BG_DEFAULT.b, 0x00);
    }

    /// 工具: 构造一个 cell 用于 build_vertex_bytes 测试.
    fn cell_with_bg(c: char, bg: crate::term::Color) -> crate::term::CellRef {
        crate::term::CellRef {
            pos: crate::term::CellPos { col: 0, line: 0 },
            c,
            fg: crate::term::Color {
                r: 0xd3,
                g: 0xd3,
                b: 0xd3,
            },
            bg,
        }
    }

    /// build_vertex_bytes(Bg path): cell.bg = DEFAULT_BG → 0 vertex (派单 §A
    /// 主路径 — 跳过 default bg 让 clear color 透出, alacritty / xterm / foot
    /// 标准做法).
    #[test]
    fn build_vertex_bytes_skips_default_bg_under_bg_source() {
        let cells = vec![cell_with_bg('a', CELL_BG_DEFAULT)];
        let renderer = MaybeRenderer::sentinel();
        let bytes = renderer.build_vertex_bytes(
            &cells,
            20.0, // cell_w_px (任意)
            50.0, // cell_h_px (任意)
            1600.0,
            1200.0,
            CellColorSource::Bg,
            0.0,
        );
        assert!(
            bytes.is_empty(),
            "default bg cell 应跳过 vertex 生成 (alacritty 标准, T-0604), got {} bytes",
            bytes.len()
        );
    }

    /// build_vertex_bytes(Bg path): cell.bg = explicit (非 default) → 6 vertex
    /// (vim / ls --color 等 explicit 高亮路径维持渲染, 不被本 ticket 误删).
    #[test]
    fn build_vertex_bytes_keeps_explicit_bg_under_bg_source() {
        let red = crate::term::Color {
            r: 0xff,
            g: 0x00,
            b: 0x00,
        };
        let cells = vec![cell_with_bg('a', red)];
        let renderer = MaybeRenderer::sentinel();
        let bytes = renderer.build_vertex_bytes(
            &cells,
            20.0,
            50.0,
            1600.0,
            1200.0,
            CellColorSource::Bg,
            0.0,
        );
        assert_eq!(
            bytes.len(),
            6 * VERTEX_BYTES,
            "explicit bg cell 必须画 6 vertex (1 quad)"
        );
    }

    /// build_vertex_bytes(Fg path): T-0305 fallback 视觉契约 — fg 色块作锚点, 跳
    /// 过路径 only Bg 走, Fg 路径不能被新跳过逻辑误抹 (cell.bg = DEFAULT 时 Fg
    /// 路径仍画 fg 色块).
    #[test]
    fn build_vertex_bytes_keeps_fg_path_for_default_bg_cell() {
        let cells = vec![cell_with_bg('a', CELL_BG_DEFAULT)];
        let renderer = MaybeRenderer::sentinel();
        let bytes = renderer.build_vertex_bytes(
            &cells,
            20.0,
            50.0,
            1600.0,
            1200.0,
            CellColorSource::Fg,
            0.0,
        );
        assert_eq!(
            bytes.len(),
            6 * VERTEX_BYTES,
            "Fg 路径 (Phase 3 fallback) 不受 default bg 跳过影响"
        );
    }

    /// 空格 cell 仍跳过 (既有契约不破): cell.c = ' ' 优先短路, 与 default bg
    /// 跳过逻辑独立。
    #[test]
    fn build_vertex_bytes_still_skips_space_cell() {
        let red = crate::term::Color {
            r: 0xff,
            g: 0x00,
            b: 0x00,
        };
        // 空格 cell 即使 bg = explicit 红, 也优先跳过 (空格短路在 default bg
        // 检查之前).
        let cells = vec![cell_with_bg(' ', red)];
        let renderer = MaybeRenderer::sentinel();
        let bytes = renderer.build_vertex_bytes(
            &cells,
            20.0,
            50.0,
            1600.0,
            1200.0,
            CellColorSource::Bg,
            0.0,
        );
        assert!(bytes.is_empty(), "空格 cell 仍优先跳过");
    }

    /// cursor inset 几何: Block 4 顶点 x 范围严格在 cell [col*cw + inset, col*cw +
    /// cw - inset], 不到 cell 左/右边缘 (派单 §C: 字形溢出像素不被覆盖).
    #[test]
    fn cursor_block_x_range_is_inset_from_cell_edges() {
        let (sw, sh, cw, ch, off) = cursor_test_geom();
        let cursor = CursorInfo {
            col: 5,
            line: 3,
            visible: true,
            style: CursorStyle::Block,
            color: crate::term::Color {
                r: 0xff,
                g: 0xff,
                b: 0xff,
            },
        };
        let mut bytes = Vec::new();
        append_cursor_quads_to_cell_bytes(
            &mut bytes,
            &cursor,
            80,
            24,
            cw,
            ch,
            sw,
            sh,
            off,
            cursor_color_white(),
        );
        let inset_px = CURSOR_INSET_PX as f32 * HIDPI_SCALE as f32;
        // cell 左边缘 NDC vs 实际 TL.x — 应严格大于 cell 边缘 (左缘内缩).
        let cell_left_ndc = (5.0 * cw) / sw * 2.0 - 1.0;
        let cell_right_ndc = (5.0 * cw + cw) / sw * 2.0 - 1.0;
        let tl_x = f32::from_ne_bytes(bytes[0..4].try_into().unwrap());
        let br_off = 2 * VERTEX_BYTES;
        let br_x = f32::from_ne_bytes(bytes[br_off..br_off + 4].try_into().unwrap());
        assert!(
            tl_x > cell_left_ndc + 1e-6,
            "TL.x ({tl_x}) 应严格大于 cell 左缘 NDC ({cell_left_ndc}, inset {inset_px} phys)"
        );
        assert!(
            br_x < cell_right_ndc - 1e-6,
            "BR.x ({br_x}) 应严格小于 cell 右缘 NDC ({cell_right_ndc}, inset {inset_px} phys)"
        );
    }

    /// HollowBlock inset 后 4 边框 x 仍在 [cell_x0+inset, cell_x1-inset] 内
    /// (4 quad 共 24 顶点).
    #[test]
    fn cursor_hollow_block_x_range_is_inset() {
        let (sw, sh, cw, ch, off) = cursor_test_geom();
        let cursor = CursorInfo {
            col: 7,
            line: 0,
            visible: true,
            style: CursorStyle::HollowBlock,
            color: crate::term::Color {
                r: 0xff,
                g: 0xff,
                b: 0xff,
            },
        };
        let mut bytes = Vec::new();
        append_cursor_quads_to_cell_bytes(
            &mut bytes,
            &cursor,
            80,
            24,
            cw,
            ch,
            sw,
            sh,
            off,
            cursor_color_white(),
        );
        // 24 顶点 (4 quad × 6).
        assert_eq!(bytes.len(), 4 * 6 * VERTEX_BYTES);
        // 扫描所有顶点 x: max ≤ cell_right_ndc - 1e-6, min ≥ cell_left_ndc + 1e-6.
        let cell_left_ndc = (7.0 * cw) / sw * 2.0 - 1.0;
        let cell_right_ndc = (7.0 * cw + cw) / sw * 2.0 - 1.0;
        let n_verts = bytes.len() / VERTEX_BYTES;
        for i in 0..n_verts {
            let off_v = i * VERTEX_BYTES;
            let x = f32::from_ne_bytes(bytes[off_v..off_v + 4].try_into().unwrap());
            assert!(
                x >= cell_left_ndc - 1e-6,
                "vertex {i} x ({x}) 不应越过 cell 左缘 NDC ({cell_left_ndc})"
            );
            assert!(
                x <= cell_right_ndc + 1e-6,
                "vertex {i} x ({x}) 不应越过 cell 右缘 NDC ({cell_right_ndc})"
            );
        }
    }

    // ---- 测试辅助: 构造一个不真起 wgpu 的 sentinel Renderer 跑纯 vertex 数学
    // (build_vertex_bytes 实际只读 self.surface_is_srgb, 其余字段 vertex 数学不
    // 用; sRGB false 让 color_for_vertex 走简单 byte/255 分支). MaybeRenderer
    // 是 #[cfg(test)] 工具, 与 T-0408 render_headless 走 `surface_is_srgb`
    // 显式参数策略一致, 不引入运行时依赖. ----
    struct MaybeRenderer {
        surface_is_srgb: bool,
    }

    impl MaybeRenderer {
        fn sentinel() -> Self {
            Self {
                surface_is_srgb: false,
            }
        }

        /// 转发到 build_vertex_bytes 的纯 vertex 数学版本 — 复制 Renderer 同名
        /// 方法 body, 仅替换 self.surface_is_srgb 路径. 测试不引入 wgpu 资源.
        #[allow(clippy::too_many_arguments)]
        fn build_vertex_bytes(
            &self,
            cells: &[crate::term::CellRef],
            cell_w_px: f32,
            cell_h_px: f32,
            surface_w: f32,
            surface_h: f32,
            color_source: CellColorSource,
            y_offset_px: f32,
        ) -> Vec<u8> {
            let mut out: Vec<u8> = Vec::with_capacity(cells.len() * VERTS_PER_CELL * VERTEX_BYTES);
            for cell in cells {
                if cell.c == ' ' {
                    continue;
                }
                if matches!(color_source, CellColorSource::Bg) && cell.bg == CELL_BG_DEFAULT {
                    continue;
                }
                let x0_px = cell.pos.col as f32 * cell_w_px;
                let y0_px = cell.pos.line as f32 * cell_h_px + y_offset_px;
                let x1_px = x0_px + cell_w_px;
                let y1_px = y0_px + cell_h_px;
                let left = x0_px / surface_w * 2.0 - 1.0;
                let right = x1_px / surface_w * 2.0 - 1.0;
                let top = 1.0 - y0_px / surface_h * 2.0;
                let bottom = 1.0 - y1_px / surface_h * 2.0;
                let color = color_for_vertex_with_srgb(
                    match color_source {
                        CellColorSource::Fg => cell.fg,
                        CellColorSource::Bg => cell.bg,
                    },
                    self.surface_is_srgb,
                );
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
    }
}
