//! 单线程事件循环内核。
//!
//! 所有 IO fd(Wayland / PTY / timerfd / xkb / D-Bus)都通过这里注册,
//! 不跨线程做 IO。本模块只提供通用骨架,具体源的接入由各业务模块负责。

use std::time::Duration;

use anyhow::{Context, Result};
use calloop::{EventLoop, LoopHandle, LoopSignal};

/// 单线程事件循环内核。
///
/// `State` 是业务状态,回调通过 `&mut State` 写入。调用方用 [`Core::handle`]
/// 拿到 [`LoopHandle`] 去注册任意 `calloop::EventSource`。
pub struct Core<'l, State> {
    inner: EventLoop<'l, State>,
}

impl<'l, State> Core<'l, State> {
    /// 新建事件循环。底层使用 `polling` 的 epoll / kqueue 后端。
    pub fn new() -> Result<Self> {
        let inner = EventLoop::try_new().context("初始化 calloop EventLoop 失败")?;
        Ok(Self { inner })
    }

    /// 获取循环句柄,用于注册事件源。句柄可 clone,也可在回调中使用。
    pub fn handle(&self) -> LoopHandle<'l, State> {
        self.inner.handle()
    }

    /// 获取可跨业务模块持有的停机信号。调用 [`LoopSignal::stop`] 后,
    /// 下一次 dispatch 返回时 [`Core::run`] 会退出。
    pub fn signal(&self) -> LoopSignal {
        self.inner.get_signal()
    }

    /// 阻塞运行直至 `signal().stop()` 被调用。
    ///
    /// `idle_cb` 在每轮 dispatch 之间被触发,可用来把"没关联到具体 fd"的
    /// 轻量收尾工作塞进循环(例如渲染帧调度状态检查)。不要在其中做 IO。
    pub fn run<F>(&mut self, data: &mut State, mut idle_cb: F) -> Result<()>
    where
        F: FnMut(&mut State),
    {
        self.inner
            .run(None, data, |s| idle_cb(s))
            .context("事件循环运行失败")
    }

    /// 单轮 dispatch,`timeout = None` 时阻塞等待事件。
    ///
    /// 主要给集成测试与少数需要手动泵事件的场景使用。业务代码正常情况
    /// 应当用 [`Core::run`]。
    pub fn dispatch(&mut self, timeout: Option<Duration>, data: &mut State) -> Result<()> {
        self.inner
            .dispatch(timeout, data)
            .context("dispatch 一轮事件失败")
    }
}
