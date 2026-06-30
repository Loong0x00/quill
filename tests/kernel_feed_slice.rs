//! Phase 7 T6 砖1a 块D:合成喂料器端到端验收(Fed 拓扑,ADR-0018 E′ 子进程)。
//!
//! 测试**当"父"**:用一对匿名 pipe 当父↔子链,起真 `quill-kernel` 子进程(`--fed-in` /
//! `--fed-out`,Fed 模式 = 不 spawn shell / 不开真 PTY,字节从父 pipe 喂、输入回灌父
//! back-channel),验四条:
//! 1. **字节下行**:父往子 pipe 灌 `FocusChange` + `PtyOutput` 帧 → 浏览器(WS)侧收到
//!    那批 payload 字节(子把父喂的字节 fan-out 给 WS 客户端,本地渲染)。
//! 2. **输入回灌**:WS 发输入(Binary)→ 子 encode 成 `Input` 帧写父 back-channel → 父读出
//!    一个 `Input` 帧、payload 与 (workspace, tab) 标签都对(子→父闭环,子自己不写 PTY)。
//! 3. **半包**:把一帧拆成多块、跨 header/payload 边界分次灌 → 子的增量 decoder 不丢/不
//!    错位,浏览器仍收到完整 payload。
//! 4. **EOF 察觉**:父关写端 → 子 pipe 读到 EOF → 子停(ADR-0018:子靠 EOF-on-pipe 察觉
//!    父没了)。
//!
//! 父桌面真接入(tee)是砖1b;本砖子的"被父喂料"地基由本合成父独立验。
//! tests/ 允许 `unwrap`/`expect` + 裸 libc(CLAUDE.md 仅约束 src/)。

use std::net::{TcpListener, TcpStream};
use std::os::fd::RawFd;
use std::path::PathBuf;
use std::process::{Child, Command};
use std::time::{Duration, Instant};

use quill::kernel::feed::{FeedDecoder, FeedFrame, FrameKind};
use tungstenite::stream::MaybeTlsStream;
use tungstenite::{Bytes, Message, WebSocket};

const MARKER: &[u8] = b"FEED_MARKER_payload_42";

fn free_port() -> u16 {
    let l = TcpListener::bind("127.0.0.1:0").expect("bind 临时端口");
    l.local_addr().expect("local_addr").port()
}

fn send_signal(pid: u32, sig: i32) {
    // SAFETY: kill 只对已知子进程 pid 发信号,不涉内存安全。
    unsafe {
        libc::kill(pid as i32, sig);
    }
}

/// 建匿名 pipe (无 CLOEXEC → 子进程经 fork/exec 继承)。返 (read_fd, write_fd)。
fn make_pipe() -> (RawFd, RawFd) {
    let mut fds = [0 as RawFd; 2];
    // SAFETY: pipe 写两个 fd 进我们栈上的数组;返回值检查。
    let r = unsafe { libc::pipe(fds.as_mut_ptr()) };
    assert_eq!(r, 0, "pipe() 失败: {}", std::io::Error::last_os_error());
    (fds[0], fds[1])
}

fn close_fd(fd: RawFd) {
    if fd >= 0 {
        // SAFETY: 关一个本测试持有的 fd。
        unsafe {
            libc::close(fd);
        }
    }
}

fn set_nonblock(fd: RawFd) {
    // SAFETY: fcntl 只读写 fd 的 OFD status flags。
    unsafe {
        let f = libc::fcntl(fd, libc::F_GETFL);
        libc::fcntl(fd, libc::F_SETFL, f | libc::O_NONBLOCK);
    }
}

/// 给 fd 设 FD_CLOEXEC(父保留端不被子继承 —— 否则子持着写端,父关写端时读端永不 EOF)。
fn set_cloexec(fd: RawFd) {
    // SAFETY: fcntl 只读写 fd 的 FD_CLOEXEC flag。
    unsafe {
        let f = libc::fcntl(fd, libc::F_GETFD);
        libc::fcntl(fd, libc::F_SETFD, f | libc::FD_CLOEXEC);
    }
}

/// 阻塞写全到 pipe(父→子 data 端,小帧,父保留端不设非阻塞)。
fn write_all_fd(fd: RawFd, mut bytes: &[u8]) {
    while !bytes.is_empty() {
        // SAFETY: write 到本测试持有的 pipe 写端,bytes 是活切片。
        let n = unsafe { libc::write(fd, bytes.as_ptr().cast::<libc::c_void>(), bytes.len()) };
        assert!(n > 0, "write pipe 失败 n={n}");
        bytes = &bytes[n as usize..];
    }
}

/// 合成"父":持父↔子双向 pipe + 子进程句柄。
struct FedChild {
    child: Child,
    /// 父→子 data 写端(灌 PtyOutput / FocusChange / WorkspaceAdd 帧)。
    to_child: RawFd,
    /// 子→父 input 读端(收 Input 帧),非阻塞。
    from_child: RawFd,
    dir: PathBuf,
}

impl FedChild {
    fn spawn(port: u16) -> Self {
        let dir = std::env::temp_dir().join(format!("quill-feed-{}-{}", std::process::id(), port));
        std::fs::create_dir_all(&dir).expect("建临时目录");
        let sock = dir.join("kernel.sock"); // Fed 不绑,仅占位(避免 default 依赖 XDG)

        // pipe1: 父→子 data。父写 p_write,子读 c_read(经 --fed-in)。
        let (c_read, p_write) = make_pipe();
        // pipe2: 子→父 input。子写 c_write(经 --fed-out),父读 p_read。
        let (p_read, c_write) = make_pipe();

        // 父保留端设 CLOEXEC → 不被子继承(让子持的写端唯一 → 父关写端时子读端 EOF)。
        set_cloexec(p_write);
        set_cloexec(p_read);
        set_nonblock(p_read);

        let child = Command::new(env!("CARGO_BIN_EXE_quill-kernel"))
            .arg(format!("--socket={}", sock.display()))
            .arg(format!("--ws-bind=127.0.0.1:{port}"))
            .arg(format!("--fed-in={c_read}"))
            .arg(format!("--fed-out={c_write}"))
            .env("RUST_LOG", "quill=warn")
            .spawn()
            .expect("spawn quill-kernel (Fed)");

        // 父不需要子的两端(子已继承)→ 关掉,避免父也持着影响 EOF 语义。
        close_fd(c_read);
        close_fd(c_write);

        FedChild {
            child,
            to_child: p_write,
            from_child: p_read,
            dir,
        }
    }

    fn feed(&self, frame: &FeedFrame) {
        write_all_fd(self.to_child, &frame.encode());
    }

    fn feed_raw(&self, bytes: &[u8]) {
        write_all_fd(self.to_child, bytes);
    }

    /// 在 deadline 内从 back-channel 读字节喂 decoder,解出一帧即返。
    fn recv_frame(&self, dec: &mut FeedDecoder, timeout: Duration) -> Option<FeedFrame> {
        let deadline = Instant::now() + timeout;
        let mut buf = [0u8; 4096];
        loop {
            match dec.next_frame() {
                Ok(Some(f)) => return Some(f),
                Ok(None) => {}
                Err(e) => panic!("back-channel decode 错位: {e}"),
            }
            if Instant::now() >= deadline {
                return None;
            }
            // SAFETY: read 本测试持有的 pipe 读端进栈上 buf。
            let n = unsafe {
                libc::read(
                    self.from_child,
                    buf.as_mut_ptr().cast::<libc::c_void>(),
                    buf.len(),
                )
            };
            if n > 0 {
                dec.push(&buf[..n as usize]);
            } else if n == 0 {
                return None; // EOF
            } else {
                std::thread::sleep(Duration::from_millis(20)); // WouldBlock/EINTR
            }
        }
    }

    fn cleanup(mut self) {
        send_signal(self.child.id(), libc::SIGTERM);
        let _ = self.child.wait();
        close_fd(self.to_child);
        close_fd(self.from_child);
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn connect_ws(port: u16, timeout: Duration) -> Option<WebSocket<MaybeTlsStream<TcpStream>>> {
    let url = format!("ws://127.0.0.1:{port}/");
    let deadline = Instant::now() + timeout;
    loop {
        if Instant::now() >= deadline {
            return None;
        }
        match tungstenite::connect(&url) {
            Ok((ws, _resp)) => {
                if let MaybeTlsStream::Plain(s) = ws.get_ref() {
                    let _ = s.set_read_timeout(Some(Duration::from_millis(250)));
                }
                return Some(ws);
            }
            Err(_) => std::thread::sleep(Duration::from_millis(50)),
        }
    }
}

/// 读 WS 二进制帧累积,直到出现 `needle` 或超时;返回是否命中。
fn ws_recv_until(
    ws: &mut WebSocket<MaybeTlsStream<TcpStream>>,
    needle: &[u8],
    timeout: Duration,
) -> bool {
    let deadline = Instant::now() + timeout;
    let mut acc = Vec::new();
    while Instant::now() < deadline {
        match ws.read() {
            Ok(Message::Binary(b)) => {
                acc.extend_from_slice(&b);
                if acc.windows(needle.len()).any(|w| w == needle) {
                    return true;
                }
                if acc.len() > 64 * 1024 {
                    let keep = needle.len().max(1);
                    acc.drain(..acc.len() - keep);
                }
            }
            Ok(Message::Close(_)) => return false,
            Ok(_) => {}
            Err(_) => {}
        }
    }
    false
}

/// 块D-1:父灌 PtyOutput 帧 → 浏览器(WS)收到那批 payload 字节。
#[test]
fn fed_child_streams_pty_output_frames_to_ws() {
    let port = free_port();
    let fc = FedChild::spawn(port);

    let mut ws = match connect_ws(port, Duration::from_secs(15)) {
        Some(w) => w,
        None => {
            fc.cleanup();
            panic!("15s 内未能连上 Fed 子进程 WS");
        }
    };

    // 父喂焦点 + 一帧 PtyOutput(payload=MARKER)。子 fan-out 给 WS 客户端。
    fc.feed(&FeedFrame {
        kind: FrameKind::FocusChange,
        ws_id: 1,
        tab_id: 1,
        payload: Vec::new(),
    });
    fc.feed(&FeedFrame {
        kind: FrameKind::PtyOutput,
        ws_id: 1,
        tab_id: 1,
        payload: MARKER.to_vec(),
    });

    let got = ws_recv_until(&mut ws, MARKER, Duration::from_secs(10));
    let _ = ws.close(None);
    fc.cleanup();
    assert!(
        got,
        "父灌的 PtyOutput payload 应经子 fan-out 到达 WS 浏览器侧(二进制帧)"
    );
}

/// 块D-2:WS 发输入 → 子 encode Input 帧回灌父 back-channel(payload + (ws,tab) 标签都对)。
#[test]
fn fed_child_round_trips_input_to_back_channel() {
    let port = free_port();
    let fc = FedChild::spawn(port);

    let mut ws = match connect_ws(port, Duration::from_secs(15)) {
        Some(w) => w,
        None => {
            fc.cleanup();
            panic!("15s 内未能连上 Fed 子进程 WS");
        }
    };

    // 先设焦点 (ws=5, tab=7),让子回灌的 Input 帧带这对标签。睡一会儿确保子先处理完
    // FocusChange(经 pipe)再收 WS 输入(避免两 readiness 乱序)。
    fc.feed(&FeedFrame {
        kind: FrameKind::FocusChange,
        ws_id: 5,
        tab_id: 7,
        payload: Vec::new(),
    });
    std::thread::sleep(Duration::from_millis(300));

    // WS 发输入(裸字节 Binary,与浏览器 onData→ws.send 同形)。
    ws.send(Message::Binary(Bytes::from_static(b"RUNDONE\n")))
        .expect("WS send 输入");
    let _ = ws.flush();

    // 父从 back-channel 读出一个 Input 帧。
    let mut dec = FeedDecoder::new();
    let frame = fc.recv_frame(&mut dec, Duration::from_secs(10));

    let _ = ws.close(None);
    fc.cleanup();

    let frame = frame.expect("应从 back-channel 收到子回灌的 Input 帧");
    assert_eq!(frame.kind, FrameKind::Input, "回灌帧 kind 应为 Input");
    assert_eq!(frame.payload, b"RUNDONE\n", "回灌 payload 应为 WS 发的输入");
    assert_eq!(frame.ws_id, 5, "Input 帧应带焦点 workspace 标签");
    assert_eq!(frame.tab_id, 7, "Input 帧应带焦点 tab 标签");
}

/// 块D-5(Lead 补,验 Bug1 修复):大输入(> 默认 pipe 缓冲 64KiB)经 back-channel 回灌父,
/// 强制子端【分帧 + 出站队列 + 可写时 drain】。父侧逐帧重组须【逐字节完整、无错位】——
/// 证 write_frame 背压下绝不丢半帧(若丢,recv_frame 会撞解码错位 panic 或重组短/不等)。
#[test]
fn fed_child_back_channel_survives_large_input_backpressure() {
    const N: usize = 256 * 1024; // > 默认 pipe 缓冲(64KiB),且分成 4 个 64KiB Input 帧

    let port = free_port();
    let fc = FedChild::spawn(port);

    let mut ws = match connect_ws(port, Duration::from_secs(15)) {
        Some(w) => w,
        None => {
            fc.cleanup();
            panic!("15s 内未能连上 Fed 子进程 WS");
        }
    };

    fc.feed(&FeedFrame {
        kind: FrameKind::FocusChange,
        ws_id: 5,
        tab_id: 7,
        payload: Vec::new(),
    });
    std::thread::sleep(Duration::from_millis(300));

    // 位置相关图样:任何丢字节 / 错位 / 乱序都会让重组不等。
    let sent: Vec<u8> = (0..N).map(|i| (i % 251) as u8).collect();
    ws.send(Message::Binary(Bytes::from(sent.clone())))
        .expect("WS send 大输入");
    let _ = ws.flush();

    // 父循环收齐:子把 256KiB 分成 ≤64KiB Input 帧,pipe 满 → 入队 → 可写时 drain。
    let mut dec = FeedDecoder::new();
    let mut got: Vec<u8> = Vec::with_capacity(N);
    let deadline = Instant::now() + Duration::from_secs(20);
    while got.len() < N && Instant::now() < deadline {
        match fc.recv_frame(&mut dec, Duration::from_secs(10)) {
            Some(f) => {
                assert_eq!(f.kind, FrameKind::Input, "回灌帧 kind 应为 Input");
                assert_eq!(f.ws_id, 5, "Input 帧应带焦点 workspace 标签");
                assert_eq!(f.tab_id, 7, "Input 帧应带焦点 tab 标签");
                got.extend_from_slice(&f.payload);
            }
            None => break,
        }
    }

    let _ = ws.close(None);
    fc.cleanup();

    assert_eq!(
        got.len(),
        N,
        "应从 back-channel 收齐全部输入字节(无丢半帧 / 截断)"
    );
    assert!(got == sent, "重组字节须与发送逐字节相等(无错位 / 乱序)");
}

/// 块D-3:半包 —— 一帧拆多块跨 header/payload 边界分次灌,子 decoder 不丢/不错位。
#[test]
fn fed_child_handles_partial_framed_feed() {
    let port = free_port();
    let fc = FedChild::spawn(port);

    let mut ws = match connect_ws(port, Duration::from_secs(15)) {
        Some(w) => w,
        None => {
            fc.cleanup();
            panic!("15s 内未能连上 Fed 子进程 WS");
        }
    };

    fc.feed(&FeedFrame {
        kind: FrameKind::FocusChange,
        ws_id: 1,
        tab_id: 1,
        payload: Vec::new(),
    });

    // 一帧 PtyOutput(payload=MARKER)拆三块灌:头中段 / 跨进 payload / 剩余,各隔一会儿。
    let frame = FeedFrame {
        kind: FrameKind::PtyOutput,
        ws_id: 1,
        tab_id: 1,
        payload: MARKER.to_vec(),
    }
    .encode();
    // 5 字节(头中段)→ 25 字节(跨过 21 字节头进 payload)→ 剩余。
    let a = 5.min(frame.len());
    let b = 25.min(frame.len());
    fc.feed_raw(&frame[..a]);
    std::thread::sleep(Duration::from_millis(120));
    fc.feed_raw(&frame[a..b]);
    std::thread::sleep(Duration::from_millis(120));
    fc.feed_raw(&frame[b..]);

    let got = ws_recv_until(&mut ws, MARKER, Duration::from_secs(10));
    let _ = ws.close(None);
    fc.cleanup();
    assert!(
        got,
        "半包分次灌入子 decoder 后,完整 payload 仍应无丢/无错位到达 WS"
    );
}

/// 块D-4:父关写端 → 子读到 pipe EOF → 子退出(ADR-0018 子靠 EOF 察觉父没了)。
#[test]
fn fed_child_exits_on_parent_pipe_eof() {
    let port = free_port();
    let mut fc = FedChild::spawn(port);

    // 连一下确保子就绪(WS 起来 = loop 在跑)。
    let ws = connect_ws(port, Duration::from_secs(15));
    assert!(ws.is_some(), "子进程应起 WS(loop 在跑)");
    if let Some(mut w) = ws {
        let _ = w.close(None);
    }

    // 关父→子写端:子的 read fd 无其它写者 → 读到 EOF → 停 loop → 进程退出。
    close_fd(fc.to_child);
    fc.to_child = -1;

    // 等子退出(EOF 后 calloop stop → run 返回 → 进程退出)。
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut exited = false;
    while Instant::now() < deadline {
        match fc.child.try_wait() {
            Ok(Some(_status)) => {
                exited = true;
                break;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(50)),
            Err(_) => break,
        }
    }

    fc.cleanup();
    assert!(
        exited,
        "父关 pipe 写端后,子应读到 EOF 并退出(ADR-0018 EOF 察觉父没了)"
    );
}
