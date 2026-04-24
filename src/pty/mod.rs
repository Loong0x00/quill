//! PTY 子进程 + master fd 封装(Phase 2 骨架)。
//!
//! 本模块在 Phase 1 末期由规划 teammate 挖好骨架,所有方法当前均为 `todo!()`,
//! 由 T-0201..T-0205 写码 teammate 依次填实。外部入口只有 [`PtyHandle`] 这个
//! 结构体 + 五个方法,语义上对应 "shell 子进程 + 非阻塞 master fd" 抽象。
//!
//! 设计要点(给后续写码的提示,不是实装):
//! - 整个 crate 里 **只有本模块** 能持有 PTY 相关 fd;其它模块通过 [`PtyHandle::raw_fd`]
//!   拿 `RawFd` 注册 calloop,但 **不负责 close**(INV-005 配套)。
//! - 字段声明顺序必须保证 drop 时 "reader → master → child" —— 即 reader 先 drop
//!   释放 dup 出的 fd,master drop 关闭主 fd 触发 slave 端 SIGHUP,child 最后被
//!   回收。T-0201 写码时必须在此追加一条 INV 到 `docs/invariants.md`。
//! - `spawn_shell` 与 `spawn_program` 共享 openpty + slave drop 逻辑;前者内部调后者。
//!   分两个出口是为了让 T-0206 的集成测试能直接 spawn `echo` 而不需要经过 shell。

use std::io;
use std::os::fd::RawFd;

/// 子进程 + master 端封装。
///
/// Phase 2 骨架:T-0201 把本结构体改为真实 `PtyPair` / `Child` / reader 的持有者,
/// 并接管 drop 顺序不变式。当前 unit struct 仅为编译占位。
pub struct PtyHandle;

impl PtyHandle {
    /// 起一个登录 shell(当前写死 `bash -l`),返回持有 master 端的句柄。
    ///
    /// why:Phase 2 的产出是"窗口打开后 spawn shell";这是唯一被 `main.rs` 调用
    /// 的构造入口。`cols`/`rows` 在 Phase 2 由 T-0202 硬编码 80x24,Phase 3 T-0306
    /// 接窗口 resize 后才会真动态传入。
    pub fn spawn_shell(cols: u16, rows: u16) -> anyhow::Result<Self> {
        todo!("T-0201: openpty + CommandBuilder(\"bash -l\") + slave.spawn_command(), cols={cols} rows={rows}")
    }

    /// 起任意程序(给 T-0206 集成测试用,将来也可能服务 Phase 6 的 shell 配置)。
    ///
    /// why:bash 作为登录 shell 会进入交互态,不便于集成测试断言具体 stdout。
    /// 暴露通用 spawn 让测试能直接跑 `echo hello` 这类一次性命令。`spawn_shell`
    /// 内部就是调本函数。
    pub fn spawn_program(
        program: &str,
        args: &[&str],
        cols: u16,
        rows: u16,
    ) -> anyhow::Result<Self> {
        todo!(
            "T-0201: openpty + CommandBuilder({program:?}).args({args:?}) + spawn, cols={cols} rows={rows}"
        )
    }

    /// 返回 master fd,供 calloop `Generic` source 注册。
    ///
    /// why:INV-005 要求所有 IO fd 进同一 `calloop::EventLoop`。本方法是 PTY 这
    /// 条 fd 接入事件循环的唯一入口。**所有权仍在 `PtyHandle`**,调用方不得 close
    /// 这个 fd,drop 由 `PtyHandle::drop` 负责。
    pub fn raw_fd(&self) -> RawFd {
        todo!("T-0202: return master.as_raw_fd() and assert O_NONBLOCK has been set")
    }

    /// 从 master 端非阻塞读字节。返回读到的字节数;0 表示 EOF(通常意味着子进程退出)。
    ///
    /// why:Phase 2 只把字节 `tracing::trace!` 出来(T-0203);Phase 3 T-0302 才把
    /// 字节喂 `alacritty_terminal::Term`。保持读字节与后续消费解耦,是本方法的
    /// 职责分界。master fd 必须已置 `O_NONBLOCK`,`WouldBlock` 由调用方自行忽略。
    pub fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        todo!(
            "T-0203: reader.read(buf), must be non-blocking, buf len = {}",
            buf.len()
        )
    }

    /// 把新尺寸推给 master,底层触发 TIOCSWINSZ + SIGWINCH 给前台进程组。
    ///
    /// why:Phase 2 暂不接窗口 resize(T-0204 只开 API,Phase 3 T-0306 才接 Wayland
    /// configure → cell 尺寸换算 → 此方法)。`&self` 而非 `&mut self` 是因为
    /// `portable-pty::MasterPty::resize` 的签名就是这样。
    pub fn resize(&self, cols: u16, rows: u16) -> anyhow::Result<()> {
        todo!("T-0204: master.resize(PtySize {{ rows: {rows}, cols: {cols}, .. }})")
    }

    /// 非阻塞查子进程退出状态。`Ok(None)` = 仍在跑,`Ok(Some(code))` = 已退出。
    ///
    /// why:T-0205 需要在 PTY EOF / EIO 时把子进程收尸并往 `should_exit` 置位。
    /// 禁止阻塞 `wait()`,否则整个事件循环卡死(INV-005)。
    pub fn try_wait(&mut self) -> anyhow::Result<Option<i32>> {
        todo!("T-0205: child.try_wait() -> Option<ExitStatus> -> Option<i32>")
    }
}
