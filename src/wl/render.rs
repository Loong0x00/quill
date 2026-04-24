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
    // surface(第 1)先释放,instance(第 6)最后。surface 依赖 instance 保持
    // Vulkan/GL 实例存活;device/queue 依赖 adapter(已被构造完 drop,device
    // 自带引用保持 GPU context)。见 docs/invariants.md。
    surface: wgpu::Surface<'static>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    config: wgpu::SurfaceConfiguration,
    clear: wgpu::Color,
    // 持有 Instance 避免提前 drop 掉 Vulkan/GL 实例。
    #[allow(dead_code)]
    instance: wgpu::Instance,
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
        tracing::debug!(
            ?format,
            width = config.width,
            height = config.height,
            "wgpu surface configured"
        );

        Ok(Self {
            surface,
            device,
            queue,
            config,
            clear,
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
