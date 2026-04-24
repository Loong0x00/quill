//! xdg-toplevel 最小窗口。按 T-0101 的 Implementation notes:
//! - 照 SCTK `simple_window` 抄 delegate_* 宏,不自卷 Dispatch
//! - 第一次 configure 到达之前不 commit buffer(只 commit 空 surface 作 map 请求)
//! - 初始尺寸硬编码 800x600
//! - 事件循环用 `event_queue.blocking_dispatch`,T-0105 再换 calloop
//!
//! 本 ticket 不画终端内容,但某些 compositor 在 toplevel 没有附 buffer 时不会真正
//! 把窗口弹出来;所以 configure 回来后用 wl_shm 填一帧纯色(白)当占位,
//! T-0102 会用 wgpu 替掉。

use anyhow::{anyhow, Context, Result};
use smithay_client_toolkit::{
    compositor::{CompositorHandler, CompositorState},
    delegate_compositor, delegate_output, delegate_registry, delegate_shm, delegate_xdg_shell,
    delegate_xdg_window,
    output::{OutputHandler, OutputState},
    registry::{ProvidesRegistryState, RegistryState},
    registry_handlers,
    shell::{
        xdg::{
            window::{Window, WindowConfigure, WindowDecorations, WindowHandler},
            XdgShell,
        },
        WaylandSurface,
    },
    shm::{
        slot::{Buffer, SlotPool},
        Shm, ShmHandler,
    },
};
use wayland_client::{
    globals::registry_queue_init,
    protocol::{wl_output, wl_shm, wl_surface},
    Connection, QueueHandle,
};

const INITIAL_WIDTH: u32 = 800;
const INITIAL_HEIGHT: u32 = 600;
const APP_ID: &str = "io.github.loong0x00.quill";
const WINDOW_TITLE: &str = "quill";

/// 纯逻辑的窗口状态机核心(T-0107 抽离)。
///
/// 从 Wayland 回调里剥出四个 scalar 字段,使 headless 测试不需要真实 compositor
/// 就能驱动状态转移。活动回调持有一份 [`WindowCore`] 并通过 [`handle_event`]
/// 推进,确保测试路径和真路径是同一条。
#[derive(Debug, Clone)]
pub struct WindowCore {
    pub width: u32,
    pub height: u32,
    pub first_configure: bool,
    pub exit: bool,
    /// 尺寸变更后置位,调用方(当前是 [`State::configure`],未来是 T-0103 的
    /// swapchain 重建路径)读取后自行清零。布尔而非队列,天然把连续 resize
    /// 合并到单次脏标记。
    pub resize_dirty: bool,
}

impl WindowCore {
    pub fn new(initial_width: u32, initial_height: u32) -> Self {
        Self {
            width: initial_width,
            height: initial_height,
            first_configure: true,
            exit: false,
            resize_dirty: false,
        }
    }
}

/// 从 Wayland 层抽象出来的窗口事件。
///
/// `Configure` 的尺寸拍扁为 `Option<u32>`:compositor 未给新尺寸时对应 `None`,
/// 由 client 保留旧值;显式 0 由 client 侧防守吞掉(见 [`handle_event`])。
/// `Disconnect` 对应实际跑起来时 `blocking_dispatch` 返回 `Err` 的情形,headless
/// 测试里模拟 compositor 掉线,语义上等价于 `Close`——都应触发退出。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowEvent {
    Configure {
        new_width: Option<u32>,
        new_height: Option<u32>,
    },
    Close,
    Disconnect,
}

/// 状态机转移的副作用描述。告诉上层"要不要重画"——真回调据此决定要不要
/// 重绘占位 buffer / 重建 swapchain。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct WindowAction {
    pub needs_draw: bool,
}

/// 单步状态转移。纯逻辑、无副作用,是所有 Wayland 事件改状态的唯一入口。
///
/// 行为约定(由 `tests/state_machine.rs` 固化):
/// - 首次 Configure:吃下尺寸,清 `first_configure`,置 `resize_dirty`,要求重画。
/// - 后续 Configure 尺寸变化:更新尺寸,置 `resize_dirty`,要求重画。
/// - 后续 Configure 尺寸不变:不置脏,不重画(幂等)。
/// - 任一轴为 0:整条事件吞掉,保留旧尺寸(xdg-shell 语义里 0x0 是 client 决定,
///   到这层已经保底一次,防御性写法)。
/// - Close / Disconnect:置 `exit`。
pub fn handle_event(state: &mut WindowCore, ev: WindowEvent) -> WindowAction {
    let mut action = WindowAction::default();
    match ev {
        WindowEvent::Configure {
            new_width,
            new_height,
        } => {
            let w = new_width.unwrap_or(state.width);
            let h = new_height.unwrap_or(state.height);
            if w == 0 || h == 0 {
                return action;
            }
            if state.first_configure {
                state.width = w;
                state.height = h;
                state.first_configure = false;
                state.resize_dirty = true;
                action.needs_draw = true;
            } else if w != state.width || h != state.height {
                state.width = w;
                state.height = h;
                state.resize_dirty = true;
                action.needs_draw = true;
            }
        }
        WindowEvent::Close | WindowEvent::Disconnect => {
            state.exit = true;
        }
    }
    action
}

/// 启动 Wayland 连接、创建 xdg toplevel、阻塞 dispatch 直到窗口被关闭。
///
/// 本函数在用户点关闭时返回 `Ok(())`。T-0105 会替换内部循环到 calloop。
pub fn run_window() -> Result<()> {
    let conn = Connection::connect_to_env()
        .context("连接 Wayland compositor 失败(是否在 Wayland session 下?)")?;
    let (globals, mut event_queue) =
        registry_queue_init(&conn).context("初始化 Wayland registry 失败")?;
    let qh = event_queue.handle();

    let compositor =
        CompositorState::bind(&globals, &qh).map_err(|e| anyhow!("wl_compositor 不可用: {e}"))?;
    let xdg_shell = XdgShell::bind(&globals, &qh).map_err(|e| anyhow!("xdg_shell 不可用: {e}"))?;
    let shm = Shm::bind(&globals, &qh).map_err(|e| anyhow!("wl_shm 不可用: {e}"))?;

    let surface = compositor.create_surface(&qh);
    let window = xdg_shell.create_window(surface, WindowDecorations::RequestServer, &qh);
    window.set_title(WINDOW_TITLE);
    window.set_app_id(APP_ID);
    window.set_min_size(Some((INITIAL_WIDTH, INITIAL_HEIGHT)));

    // Implementation note: 第一次 configure 前只能 commit 空 surface(无 buffer 附加),
    // 这是 xdg-shell 的 map 请求语义。
    window.commit();

    let pool = SlotPool::new((INITIAL_WIDTH * INITIAL_HEIGHT * 4) as usize, &shm)
        .context("分配 SlotPool 失败")?;

    let mut state = State {
        registry_state: RegistryState::new(&globals),
        output_state: OutputState::new(&globals, &qh),
        shm,
        pool,
        window,
        core: WindowCore::new(INITIAL_WIDTH, INITIAL_HEIGHT),
        buffer: None,
    };

    tracing::info!(
        width = INITIAL_WIDTH,
        height = INITIAL_HEIGHT,
        "quill 窗口已请求创建"
    );

    while !state.core.exit {
        event_queue
            .blocking_dispatch(&mut state)
            .context("Wayland blocking_dispatch 失败")?;
    }

    tracing::info!("窗口关闭,退出事件循环");
    Ok(())
}

struct State {
    registry_state: RegistryState,
    output_state: OutputState,
    shm: Shm,
    pool: SlotPool,
    window: Window,
    core: WindowCore,
    buffer: Option<Buffer>,
}

impl State {
    /// 画一帧纯色占位。ticket 要求"窗口里可以是空白",但部分 compositor(Mutter)
    /// 在 toplevel 从未附 buffer 时会认为 surface 未就绪、不把窗口 map 出来,所以这里
    /// 填一次白,保证窗口真正可见。T-0102 会用 wgpu 画实际终端内容。
    fn draw_placeholder(&mut self) -> Result<()> {
        let w = self.core.width as i32;
        let h = self.core.height as i32;
        let stride = w * 4;
        let (buffer, canvas) = self
            .pool
            .create_buffer(w, h, stride, wl_shm::Format::Argb8888)
            .context("create_buffer 失败")?;

        for chunk in canvas.chunks_exact_mut(4) {
            // Argb8888 little-endian: [B, G, R, A]
            chunk[0] = 0xFF;
            chunk[1] = 0xFF;
            chunk[2] = 0xFF;
            chunk[3] = 0xFF;
        }

        let surface = self.window.wl_surface();
        buffer
            .attach_to(surface)
            .context("buffer 附加到 surface 失败")?;
        surface.damage_buffer(0, 0, w, h);
        self.window.commit();
        self.buffer = Some(buffer);
        Ok(())
    }
}

impl CompositorHandler for State {
    fn scale_factor_changed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _new_factor: i32,
    ) {
    }

    fn transform_changed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _new_transform: wl_output::Transform,
    ) {
    }

    fn frame(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _time: u32,
    ) {
    }

    fn surface_enter(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _output: &wl_output::WlOutput,
    ) {
    }

    fn surface_leave(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _output: &wl_output::WlOutput,
    ) {
    }
}

impl OutputHandler for State {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.output_state
    }

    fn new_output(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _output: wl_output::WlOutput,
    ) {
    }

    fn update_output(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _output: wl_output::WlOutput,
    ) {
    }

    fn output_destroyed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _output: wl_output::WlOutput,
    ) {
    }
}

impl WindowHandler for State {
    fn request_close(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &Window) {
        tracing::info!("compositor 请求关闭窗口");
        let _ = handle_event(&mut self.core, WindowEvent::Close);
    }

    fn configure(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _window: &Window,
        configure: WindowConfigure,
        _serial: u32,
    ) {
        let new_w = configure.new_size.0.map(|v| v.get());
        let new_h = configure.new_size.1.map(|v| v.get());
        tracing::debug!(
            ?new_w,
            ?new_h,
            first = self.core.first_configure,
            "configure"
        );

        let was_first = self.core.first_configure;
        let action = handle_event(
            &mut self.core,
            WindowEvent::Configure {
                new_width: new_w,
                new_height: new_h,
            },
        );

        // resize 处理是 T-0103 的范围:本 ticket 仅在首次 configure 真去画占位,
        // 此后 `WindowCore::resize_dirty` 置位由 T-0103 消费并重建 SlotPool。
        if was_first && action.needs_draw {
            if let Err(err) = self.draw_placeholder() {
                tracing::error!(?err, "首帧占位绘制失败");
                self.core.exit = true;
            } else {
                self.core.resize_dirty = false;
            }
        }
    }
}

impl ShmHandler for State {
    fn shm_state(&mut self) -> &mut Shm {
        &mut self.shm
    }
}

impl ProvidesRegistryState for State {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }
    registry_handlers![OutputState];
}

delegate_compositor!(State);
delegate_output!(State);
delegate_shm!(State);
delegate_xdg_shell!(State);
delegate_xdg_window!(State);
delegate_registry!(State);

#[cfg(test)]
mod tests {
    use super::*;

    /// Smoke test:窗口模块只对外导出 [`run_window`],签名固定为 `fn() -> Result<()>`。
    /// 这里通过函数指针绑定把 contract 固化在编译期,防止后续重构误改签名(比如加参数或
    /// 返回 ())。实际 Wayland 连接的 runtime 行为依赖 compositor,留给集成测试与 soak。
    #[test]
    fn smoke_run_window_signature_is_stable() {
        let f: fn() -> Result<()> = run_window;
        // 仅保留引用,避免 dead_code;不调用 f(会阻塞事件循环)。
        let _ = &f;
    }

    #[test]
    fn smoke_initial_size_is_nonzero() {
        // 防止以后"顺手"把初始尺寸改成 0x0(某些 compositor 对 0 尺寸行为未定义)。
        const _: () = assert!(INITIAL_WIDTH >= 1);
        const _: () = assert!(INITIAL_HEIGHT >= 1);
    }

    #[test]
    fn smoke_app_id_and_title_are_set() {
        // 固化 ticket acceptance 里的 "标题为 quill" 要求,防漂移。
        assert_eq!(WINDOW_TITLE, "quill");
        assert!(!APP_ID.is_empty());
        assert!(APP_ID.contains('.'), "app_id 应为反向域名格式");
    }
}
