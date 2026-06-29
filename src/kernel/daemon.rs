//! 无头会话内核 daemon 的 calloop 接线 (Phase 7 T2, ADR-0015 Phase 1 §4)。
//!
//! T1 ([`crate::kernel::session`]) 给了纯数据层 [`Session`];这里把它挂到一个
//! **单线程** `calloop::EventLoop` 上,同一 loop 注册:
//! - 一个 shell tab 的 PTY master fd (出字节 → [`Session::on_pty_output`] 驱动 term);
//! - 一个 `UnixListener` —— 客户端连上即收当前 [`crate::kernel::proto::Snapshot`]
//!   的 JSON 行 (line-delimited);
//! - `SIGINT` / `SIGTERM` 信号源 (signalfd) → 停 loop → 退出时清理 socket 文件。
//!
//! **刻意的边界 (留 T3)**:无 tokio / tungstenite (零新依赖);单线程天然回避
//! [`Session`] 的 `Rc<RefCell<String>>` 非 `Send` 约束 (ADR-0015 头号约束)。
//! WS fan-out、dirty 帧增量广播、客户端 [`crate::kernel::proto::ClientMsg`] 回灌
//! (输入 / resize / tab 操作)、多 tab 动态增删 fd 全是后续 ticket。本切片只证
//! "被驱动的 tab → Snapshot → unix socket → 客户端" 这条 spine。

use std::cell::RefCell;
use std::io::{self, Write};
use std::os::fd::BorrowedFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::rc::Rc;

use anyhow::{anyhow, bail, Context, Result};
use calloop::generic::Generic;
use calloop::signals::{Signal, Signals};
use calloop::{EventLoop, Interest, LoopHandle, LoopSignal, Mode, PostAction};

use crate::kernel::proto::Snapshot;
use crate::kernel::session::Session;
use crate::tab::{TabInstance, TabList};

/// 默认 grid 尺寸,与 `wl::run_window` 启动期 + `run_headless_screenshot` 一致
/// (一个 PTY 一个尺寸,ADR-0015 "主控端定尺寸";多客户端尺寸协商留 T3)。
pub const DEFAULT_COLS: u16 = 80;
/// 见 [`DEFAULT_COLS`]。
pub const DEFAULT_ROWS: u16 = 24;

/// 默认 socket 文件名 (挂在 `$XDG_RUNTIME_DIR` 下)。
pub const DEFAULT_SOCKET_NAME: &str = "quill-kernel.sock";

/// PTY 单次 read 缓冲。与 `wl::window` 的 `PTY_READ_BUF` 同值。
const PTY_READ_BUF: usize = 4096;

/// daemon 启动参数。
pub struct DaemonConfig {
    /// `UnixListener` 绑定路径。
    pub socket_path: PathBuf,
    pub cols: u16,
    pub rows: u16,
}

impl DaemonConfig {
    /// 用给定 socket 路径 + 默认尺寸建配置。
    pub fn with_socket(socket_path: PathBuf) -> Self {
        Self {
            socket_path,
            cols: DEFAULT_COLS,
            rows: DEFAULT_ROWS,
        }
    }
}

/// `$XDG_RUNTIME_DIR/quill-kernel.sock`。`XDG_RUNTIME_DIR` 未设时报错 (让调用方
/// 用 `--socket=<path>` 显式指定,而非静默落到 `/tmp` 这种可被他人写的位置)。
pub fn default_socket_path() -> Result<PathBuf> {
    let dir = std::env::var_os("XDG_RUNTIME_DIR")
        .ok_or_else(|| anyhow!("XDG_RUNTIME_DIR 未设置;用 --socket=<path> 显式指定 socket 路径"))?;
    Ok(PathBuf::from(dir).join(DEFAULT_SOCKET_NAME))
}

/// 从 argv 抠 `--socket=<path>`(单一 `=` 形式,与 `main.rs` 手写解析同款,不引 clap)。
pub fn parse_socket_arg(args: &[String]) -> Option<PathBuf> {
    const PREFIX: &str = "--socket=";
    args.iter()
        .skip(1)
        .find_map(|a| a.strip_prefix(PREFIX).map(PathBuf::from))
}

/// daemon 主循环。spawn shell tab、绑 socket、注册三源、`run` 阻塞到收信号 /
/// shell 退出,返回前清理 socket 文件。
pub fn run(config: DaemonConfig) -> Result<()> {
    let mut event_loop: EventLoop<'static, DaemonData> =
        EventLoop::try_new().context("calloop EventLoop::try_new 失败")?;
    let loop_handle = event_loop.handle();
    let loop_signal = event_loop.get_signal();

    // Source 1:SIGINT + SIGTERM → 停 loop(退出后清理 socket)。
    // **必须早于任何 spawn 线程的代码**:`calloop::Signals::new` 只在*当前(主)
    // 线程* block 这两个信号(`pthread_sigmask`)。紧接着的 `TabInstance::spawn`
    // → `ComposerState` → completion `WorkerPool` 会起 4 个工作线程,它们继承主
    // 线程**此刻**的 signal mask 才会一起 block;否则 SIGTERM 落到某个未 block
    // 的 worker 线程 → 走默认 terminate → 跳过下方 socket 清理(实测 exit 143)。
    // calloop signals doc 原话:"set up the signal event source before spawning
    // any thread"。
    let signals = Signals::new(&[Signal::SIGINT, Signal::SIGTERM])
        .context("calloop Signals::new(SIGINT, SIGTERM) 失败")?;
    loop_handle
        .insert_source(signals, |event, _meta, data: &mut DaemonData| {
            tracing::info!(signal = ?event.signal(), "收到终止信号,停止 daemon");
            data.loop_signal.stop();
        })
        .map_err(|e| anyhow!("calloop insert_source(signals) 失败: {e}"))?;

    // 信号已在主线程 block,现在才 spawn shell tab(其 worker 线程继承 block)。
    let tab =
        TabInstance::spawn(config.cols, config.rows).context("daemon 启动:spawn shell tab 失败")?;
    let tab_id = tab.id().raw();
    let session = Session::new(TabList::new(tab));

    prepare_socket_path(&config.socket_path)?;
    let listener = UnixListener::bind(&config.socket_path)
        .with_context(|| format!("bind UnixListener {} 失败", config.socket_path.display()))?;
    // INV-3 精神:listener 默认阻塞,Level 模式下无连接时 accept 会 stall 整个单线程
    // loop → 必须非阻塞 + accept 到 WouldBlock。
    listener
        .set_nonblocking(true)
        .context("UnixListener 设非阻塞失败")?;

    let mut data = DaemonData {
        session,
        loop_handle: loop_handle.clone(),
        loop_signal,
        tab_id,
    };

    // Source 2:PTY master fd。
    let pty_fd = data.session.tabs().active().pty().raw_fd();
    // SAFETY:
    // - pty_fd 来自 `PtyHandle::raw_fd()`(构造时 `as_raw_fd().ok_or_else` 校验过一次),
    //   PtyHandle 在 `data.session.tabs` 里,`run()` scope 内全程活着。
    // - drop 序:函数尾显式 `drop(event_loop)` 早于 `drop(data)`,即 Generic source 的
    //   `EPOLL_CTL_DEL` 在 pty fd 仍打开时执行;即便顺序错,calloop 0.14 对已关 fd 的
    //   `EPOLL_CTL_DEL` 返 EBADF 内部容忍(`wl/window.rs:2852` 同源),非 UB。
    // - `borrow_raw` 只取 int 不转移所有权;真正的 read 走 `PtyHandle::read` 自有的
    //   dup reader,从不碰这个 BorrowedFd。
    #[allow(unsafe_code)]
    let pty_borrowed: BorrowedFd<'static> = unsafe { BorrowedFd::borrow_raw(pty_fd) };
    loop_handle
        .insert_source(
            Generic::new(pty_borrowed, Interest::READ, Mode::Level),
            |_readiness, _meta, data: &mut DaemonData| pty_readable(data),
        )
        .map_err(|e| anyhow!("calloop insert_source(pty master fd) 失败: {e}"))?;

    // Source 3:UnixListener。owned 直接交给 Generic —— UnixListener 实现 AsFd,
    // calloop 持所有权并在 source drop 时关闭,无需 BorrowedFd unsafe。
    loop_handle
        .insert_source(
            Generic::new(listener, Interest::READ, Mode::Level),
            |_readiness, meta, data: &mut DaemonData| accept_ready(meta.as_ref(), data),
        )
        .map_err(|e| anyhow!("calloop insert_source(unix listener) 失败: {e}"))?;

    tracing::info!(
        socket = %config.socket_path.display(),
        tab_id,
        cols = config.cols,
        rows = config.rows,
        "quill-kernel daemon 就绪"
    );

    let run_result = event_loop
        .run(None, &mut data, |_data| {})
        .context("calloop EventLoop::run 失败");

    // 显式 drop 序:event_loop(持各 Generic source)先于 data(持 PtyHandle 的 fd),
    // 让 PTY source 的 EPOLL_CTL_DEL 在 fd 仍打开时执行(见上 SAFETY)。
    drop(event_loop);
    drop(data);

    remove_socket_quiet(&config.socket_path);

    run_result
}

/// 单线程 daemon 的 calloop `Data`。callback 拿 `&mut DaemonData` 走字段 split
/// borrow(与 `wl::window::LoopData` 同模式)。
struct DaemonData {
    session: Session,
    /// 运行期注册新 source(每个客户端连接一个 write source)用。
    loop_handle: LoopHandle<'static, DaemonData>,
    loop_signal: LoopSignal,
    /// 本切片唯一 tab 的 raw id(协议层 `u64`,INV-010 不能从 `u64` 重建 TabId)。
    tab_id: u64,
}

/// 一个客户端连接待写出的快照字节 + 已写偏移(非阻塞 write 可能分多次)。
struct WritePending {
    buf: Vec<u8>,
    written: usize,
}

/// PTY read 结果的纯决策(无副作用,可单测)。镜像 `wl::window::pty_readable_action`
/// 的策略,但那是 `pub(crate)` 取不到,这里独立实现。
#[derive(Debug, PartialEq, Eq)]
enum PtyRead {
    /// `Ok(n>0)`:喂字节后继续读(Level 模式须排空到 WouldBlock)。
    Feed,
    /// `WouldBlock`:本轮排空,保留 source。
    Drained,
    /// `Interrupted`(EINTR):重试。
    Retry,
    /// `Ok(0)` / `EIO`:子 shell 退出。
    Closed,
    /// 其它 IO 错误:非预期,按致命处理。
    Fatal,
}

fn classify_pty_read(res: &io::Result<usize>) -> PtyRead {
    match res {
        Ok(0) => PtyRead::Closed,
        Ok(_) => PtyRead::Feed,
        Err(e) if e.kind() == io::ErrorKind::WouldBlock => PtyRead::Drained,
        Err(e) if e.kind() == io::ErrorKind::Interrupted => PtyRead::Retry,
        Err(e) if e.raw_os_error() == Some(libc::EIO) => PtyRead::Closed,
        Err(_) => PtyRead::Fatal,
    }
}

/// PTY master fd readable:drain 到 WouldBlock,每 chunk 喂 [`Session::on_pty_output`]。
/// 子 shell 退出(EOF/EIO)→ 收尸 + 停 loop(单 tab 切片:shell 死即 daemon 退)。
fn pty_readable(data: &mut DaemonData) -> io::Result<PostAction> {
    let tab_id = data.tab_id;
    let mut buf = [0u8; PTY_READ_BUF];
    loop {
        let read = data
            .session
            .tabs_mut()
            .active_mut()
            .pty_mut()
            .read(&mut buf);
        match classify_pty_read(&read) {
            PtyRead::Feed => {
                if let Ok(n) = read {
                    data.session.on_pty_output(tab_id, &buf[..n]);
                }
            }
            PtyRead::Retry => continue,
            PtyRead::Drained => return Ok(PostAction::Continue),
            PtyRead::Closed => {
                tracing::info!(tab_id, "PTY EOF/EIO:子 shell 退出,停止 daemon");
                let _ = data.session.tabs_mut().active_mut().pty_mut().try_wait();
                data.loop_signal.stop();
                return Ok(PostAction::Remove);
            }
            PtyRead::Fatal => {
                if let Err(e) = read {
                    tracing::error!(tab_id, ?e, "PTY read 非预期错误,停止 daemon");
                }
                data.loop_signal.stop();
                return Ok(PostAction::Remove);
            }
        }
    }
}

/// listener readable:accept 到 WouldBlock,每个新连接发一帧当前快照。
fn accept_ready(listener: &UnixListener, data: &mut DaemonData) -> io::Result<PostAction> {
    loop {
        match listener.accept() {
            Ok((stream, _addr)) => {
                if let Err(e) = serve_snapshot(stream, data) {
                    tracing::warn!(?e, "新客户端首帧快照下发失败");
                }
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(PostAction::Continue),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => {
                tracing::warn!(?e, "UnixListener accept 失败");
                return Ok(PostAction::Continue);
            }
        }
    }
}

/// 把当前 active tab 的快照 JSON 行排进一个新的 write source(非阻塞写,分次写完
/// 即 `PostAction::Remove` 自动注销 + 关连接)。
fn serve_snapshot(stream: UnixStream, data: &mut DaemonData) -> Result<()> {
    stream
        .set_nonblocking(true)
        .context("客户端 stream 设非阻塞失败")?;
    let snap = data.session.snapshot_active();
    let line = snapshot_line(&snap)?;
    let pending = Rc::new(RefCell::new(WritePending {
        buf: line,
        written: 0,
    }));
    data.loop_handle
        .insert_source(
            Generic::new(stream, Interest::WRITE, Mode::Level),
            move |_readiness, meta, _data: &mut DaemonData| write_pending(meta.as_ref(), &pending),
        )
        .map_err(|e| anyhow!("calloop insert_source(client write) 失败: {e}"))?;
    Ok(())
}

/// 客户端连接 writable:把剩余字节写出。全部写完 / 对端关闭 / 出错 → `Remove`
/// (calloop 自动注销 source 并 drop 掉 owned 的 UnixStream = 关连接)。
fn write_pending(
    stream: &UnixStream,
    pending: &Rc<RefCell<WritePending>>,
) -> io::Result<PostAction> {
    let mut p = pending.borrow_mut();
    // `impl Write for &UnixStream`:用不可变引用即可写(NoIoDrop 只给 &)。
    let mut writer: &UnixStream = stream;
    loop {
        if p.written >= p.buf.len() {
            return Ok(PostAction::Remove);
        }
        match writer.write(&p.buf[p.written..]) {
            Ok(0) => return Ok(PostAction::Remove),
            Ok(n) => p.written += n,
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(PostAction::Continue),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => return Ok(PostAction::Remove),
        }
    }
}

/// 一条线缆帧 = `Snapshot` 的 JSON + 换行(line-delimited;ADR-0015 先 JSON 后 bincode)。
fn snapshot_line(snap: &Snapshot) -> Result<Vec<u8>> {
    let mut bytes = serde_json::to_vec(snap).context("序列化 Snapshot 为 JSON 失败")?;
    bytes.push(b'\n');
    Ok(bytes)
}

/// 启动期处理 socket 路径:已有活跃 daemon 监听则拒绝覆盖,否则删掉残留 socket 文件。
fn prepare_socket_path(path: &Path) -> Result<()> {
    match UnixStream::connect(path) {
        Ok(_) => bail!("socket {} 已有活跃 daemon 监听,拒绝覆盖", path.display()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        // ConnectionRefused = socket 文件在但无人监听(上次没清干净)→ 删。
        Err(e) if e.kind() == io::ErrorKind::ConnectionRefused => {
            remove_socket_quiet(path);
            Ok(())
        }
        // 其它错误(权限等):尝试删,真错由后续 bind 暴露更准确的信息。
        Err(_) => {
            remove_socket_quiet(path);
            Ok(())
        }
    }
}

fn remove_socket_quiet(path: &Path) {
    if let Err(e) = std::fs::remove_file(path) {
        if e.kind() != io::ErrorKind::NotFound {
            tracing::warn!(path = %path.display(), ?e, "删除 socket 文件失败");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kernel::proto::{CursorShapeWire, CursorWire};

    #[test]
    fn classify_pty_read_policy() {
        assert_eq!(classify_pty_read(&Ok(5)), PtyRead::Feed);
        assert_eq!(classify_pty_read(&Ok(0)), PtyRead::Closed);
        assert_eq!(
            classify_pty_read(&Err(io::Error::from(io::ErrorKind::WouldBlock))),
            PtyRead::Drained
        );
        assert_eq!(
            classify_pty_read(&Err(io::Error::from(io::ErrorKind::Interrupted))),
            PtyRead::Retry
        );
        assert_eq!(
            classify_pty_read(&Err(io::Error::from_raw_os_error(libc::EIO))),
            PtyRead::Closed
        );
        assert_eq!(
            classify_pty_read(&Err(io::Error::from_raw_os_error(libc::EPERM))),
            PtyRead::Fatal
        );
    }

    #[test]
    fn snapshot_line_is_json_plus_newline() {
        let snap = Snapshot {
            tab_id: 1,
            cols: 1,
            rows: 1,
            cells: vec![],
            row_texts: vec![String::new()],
            cursor: CursorWire {
                col: 0,
                line: 0,
                visible: true,
                shape: CursorShapeWire::Block,
            },
            title: String::new(),
        };
        let line = snapshot_line(&snap).expect("snapshot_line");
        assert_eq!(*line.last().expect("非空"), b'\n', "最后一字节应是换行");
        let parsed: Snapshot =
            serde_json::from_slice(&line[..line.len() - 1]).expect("去换行后反序列化");
        assert_eq!(parsed, snap, "去换行后应往返回等价 Snapshot");
    }

    #[test]
    fn parse_socket_arg_extracts_path() {
        let args = vec![
            "quill-kernel".to_string(),
            "--socket=/run/user/x.sock".to_string(),
        ];
        assert_eq!(
            parse_socket_arg(&args),
            Some(PathBuf::from("/run/user/x.sock"))
        );
        assert_eq!(parse_socket_arg(&["quill-kernel".to_string()]), None);
    }
}
