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

use std::ffi::c_void;
use std::ptr::NonNull;

use anyhow::{anyhow, Context, Result};
use raw_window_handle::{
    RawDisplayHandle, RawWindowHandle, WaylandDisplayHandle, WaylandWindowHandle,
};

use crate::term::CellRef;

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
    // 自带引用保持 GPU context)。见 docs/invariants.md。
    //
    // T-0305:`cell_pipeline` / `cell_vertex_buffer` 持 wgpu device 内部引用,
    // 必须**先于** `device` drop —— 放 surface 之后、device 之前。lazy 初始化
    // (Option),首次 [`draw_cells`] 时建好 pipeline + 预分配 vertex buffer,
    // 之后每帧 reuse(`queue.write_buffer` 写新 vertex 数据,不重建)。
    // 派单 "wgpu Pipeline / Layout / BindGroup 创建一次复用" 的硬约束。
    surface: wgpu::Surface<'static>,
    cell_pipeline: Option<wgpu::RenderPipeline>,
    cell_vertex_buffer: Option<wgpu::Buffer>,
    /// 当前 vertex buffer 容量(以**顶点数**计,非字节)。增长策略:首次按
    /// `cols * rows * VERTS_PER_CELL` 分配,后续若 cell 总数超过容量则重建
    /// (Phase 3 不会变 — Wayland resize 在 T-0306 才接;留口子防回归)。
    cell_buffer_capacity: usize,
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

/// 单 cell 6 顶点(两三角形,无 index buffer)。`vertices = cols * rows *
/// VERTS_PER_CELL`。80×24 = 11520 顶点,5090 GPU 完全无压力,instancing 优化
/// 留 Phase 6 soak 验证有需要再说。
const VERTS_PER_CELL: usize = 6;

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

        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("quill-device"),
            required_features: wgpu::Features::empty(),
            required_limits: wgpu::Limits::downlevel_defaults(),
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

        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format,
            width: width.max(1),
            height: height.max(1),
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
            device,
            queue,
            config,
            clear,
            surface_is_srgb,
            instance,
        })
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

        // Step 2: 算 cell pixel size + 构建顶点。整数除余下边距 Phase 4 再细化
        // (派单允许),但 cell_w/cell_h 至少各 1 像素防零除(派单提示)。
        let surface_w = self.config.width.max(1) as f32;
        let surface_h = self.config.height.max(1) as f32;
        let cell_w_px = (self.config.width as usize / cols).max(1) as f32;
        let cell_h_px = (self.config.height as usize / rows).max(1) as f32;

        let vertex_bytes =
            self.build_vertex_bytes(cells, cell_w_px, cell_h_px, surface_w, surface_h);
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

            let color = self.color_for_vertex(cell.fg);

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
}
