//! Phase 7 F1/F2/F2b 联邦内核垂直切片验收(ADR-0019 机器级单例 kernel + 多 feeder)。
//!
//! 测试**当会合窗口们**:起真 `quill-kernel --rendezvous=<sock>`(**不 --detach**,便于测试当
//! 子进程管理),经会合 unix socket 接入 N 个 feeder(每个 = 一个 workspace),验:
//! 1. **多 feeder 汇入多 workspace**:两 feeder 各声明不同 ws → WS 客户端连上收到【聚合】
//!    Workspaces 列表(含两个 workspace 标题)。
//! 2. **per-feeder 隔离 fan-out**:WS 客户端默认看锚(第一个接入的 feeder)workspace →只收锚的
//!    PtyOutput,收不到另一个 feeder 的字节。
//! 3. **feeder refcount 生命周期(F2b)**:断一个 feeder(非最后)→ kernel 存活;断最后一个
//!    feeder → kernel 退出 + 清会合 socket 文件。
//! 4. **锚新 tab 落点(F3)**:手机 `TabOp::New` → 回灌【锚 feeder】的 back-channel(非客户端在看
//!    的 workspace);非锚 feeder 收不到。
//!
//! tests/ 允许 `unwrap`/`expect` + 裸 libc。

use std::io::Read;
use std::net::{TcpListener, TcpStream};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::{Child, Command};
use std::time::{Duration, Instant};

use quill::kernel::feed::{
    decode_tab_op, encode_tab_list, FeedDecoder, FeedFrame, FeedTabOp, FrameKind,
};
use quill::kernel::proto::{ClientMsg, TabOp};
use tungstenite::stream::MaybeTlsStream;
use tungstenite::{Message, WebSocket};

fn free_port() -> u16 {
    let l = TcpListener::bind("127.0.0.1:0").expect("bind 临时端口");
    l.local_addr().expect("local_addr").port()
}

/// 会合 kernel 子进程 + 会合 socket 路径。
struct FedKernel {
    child: Child,
    sock: PathBuf,
    dir: PathBuf,
}

impl FedKernel {
    fn spawn(port: u16) -> Self {
        Self::spawn_with_env(port, &[])
    }

    fn spawn_with_env(port: u16, extra_env: &[(&str, &str)]) -> Self {
        let dir = std::env::temp_dir().join(format!("quill-fed-{}-{}", std::process::id(), port));
        std::fs::create_dir_all(&dir).expect("建临时目录");
        let sock = dir.join("rendezvous.sock");
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_quill-kernel"));
        cmd.arg(format!("--rendezvous={}", sock.display()))
            .arg(format!("--ws-bind=127.0.0.1:{port}"))
            .env("RUST_LOG", "quill=warn")
            // 关键:每 feeder 断都会触发"最后 feeder?"检查;若 kernel 启动即零 feeder 会等 grace。
            // 测试里我们很快接入 feeder,不必等 grace;但把 grace 设长防误退。
            .env("QUILL_FED_STARTUP_GRACE_MS", "60000");
        for (k, v) in extra_env {
            cmd.env(k, v);
        }
        let child = cmd.spawn().expect("spawn quill-kernel (Federated)");
        // 等会合 socket 出现(kernel bind 完成)。
        let deadline = Instant::now() + Duration::from_secs(10);
        while !sock.exists() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(30));
        }
        FedKernel { child, sock, dir }
    }

    /// 连一个 feeder(会合 socket 上的全双工 unix 连接)。
    fn connect_feeder(&self) -> UnixStream {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            match UnixStream::connect(&self.sock) {
                Ok(s) => {
                    s.set_read_timeout(Some(Duration::from_millis(200))).ok();
                    return s;
                }
                Err(_) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(30))
                }
                Err(e) => panic!("连会合 socket 失败: {e}"),
            }
        }
    }

    fn cleanup(mut self) {
        // SAFETY: kill 只对已知子进程 pid 发信号。
        unsafe {
            libc::kill(self.child.id() as i32, libc::SIGTERM);
        }
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// 往一个 feeder 写一帧(阻塞小帧)。
fn feed(stream: &mut UnixStream, frame: &FeedFrame) {
    use std::io::Write;
    stream.write_all(&frame.encode()).expect("feeder 写帧");
    stream.flush().ok();
}

/// 一个 feeder 声明自己:FocusChange + TabList(ws / active / tabs)。
fn declare(stream: &mut UnixStream, ws: u64, focus_tab: u64, tabs: &[(u64, &str)]) {
    let active = tabs
        .iter()
        .position(|(id, _)| *id == focus_tab)
        .unwrap_or(0);
    feed(
        stream,
        &FeedFrame {
            kind: FrameKind::FocusChange,
            ws_id: ws,
            tab_id: focus_tab,
            payload: Vec::new(),
        },
    );
    feed(
        stream,
        &FeedFrame {
            kind: FrameKind::TabList,
            ws_id: ws,
            tab_id: focus_tab,
            payload: encode_tab_list(active, tabs),
        },
    );
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

fn ws_recv_binary_until(
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
            }
            Ok(Message::Close(_)) => return false,
            Ok(_) => {}
            Err(_) => {}
        }
    }
    false
}

fn ws_recv_text_until(
    ws: &mut WebSocket<MaybeTlsStream<TcpStream>>,
    needles: &[&str],
    timeout: Duration,
) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        match ws.read() {
            Ok(Message::Text(t)) => {
                if needles.iter().all(|n| t.as_str().contains(n)) {
                    return true;
                }
            }
            Ok(Message::Close(_)) => return false,
            Ok(_) => {}
            Err(_) => {}
        }
    }
    false
}

/// F1b/F2:两 feeder 各一 workspace → WS 客户端连上收到【聚合】Workspaces(含两标题);默认看锚
/// (第一个接入的 feeder)→ 只收锚的 PtyOutput,收不到另一个 feeder 的字节(per-feeder 隔离)。
#[test]
fn federated_two_feeders_aggregate_and_isolate() {
    let port = free_port();
    let k = FedKernel::spawn(port);

    // 锚 feeder A(先接入)= ws 100 "alpha"(tab 1);feeder B = ws 200 "beta"(tab 2)。
    let mut fa = k.connect_feeder();
    declare(&mut fa, 100, 1, &[(1, "alpha")]);
    std::thread::sleep(Duration::from_millis(200)); // 确保 A 先被 accept = 锚。
    let mut fb = k.connect_feeder();
    declare(&mut fb, 200, 2, &[(2, "beta")]);
    std::thread::sleep(Duration::from_millis(300)); // 让 kernel 处理完两 feeder 声明。

    let mut ws = match connect_ws(port, Duration::from_secs(15)) {
        Some(w) => w,
        None => {
            k.cleanup();
            panic!("15s 内未能连上联邦 kernel WS");
        }
    };

    // 连上引导帧:聚合 Workspaces 应含两 workspace 的标题(alpha + beta)。
    let agg = ws_recv_text_until(
        &mut ws,
        &["\"Workspaces\"", "alpha", "beta"],
        Duration::from_secs(10),
    );
    assert!(
        agg,
        "WS 客户端应收到聚合 Workspaces(含两 feeder 的 workspace)"
    );

    // 锚(A)PtyOutput → WS 客户端(默认看锚)应收到。
    feed(
        &mut fa,
        &FeedFrame {
            kind: FrameKind::PtyOutput,
            ws_id: 100,
            tab_id: 1,
            payload: b"ANCHOR_ALPHA_DATA".to_vec(),
        },
    );
    let got_anchor = ws_recv_binary_until(&mut ws, b"ANCHOR_ALPHA_DATA", Duration::from_secs(10));
    assert!(got_anchor, "WS 客户端(看锚)应收到锚 feeder 的 PtyOutput");

    // 非锚(B)PtyOutput → WS 客户端(看锚)不该收到(per-feeder 隔离)。
    feed(
        &mut fb,
        &FeedFrame {
            kind: FrameKind::PtyOutput,
            ws_id: 200,
            tab_id: 2,
            payload: b"OTHER_BETA_DATA".to_vec(),
        },
    );
    let leaked = ws_recv_binary_until(&mut ws, b"OTHER_BETA_DATA", Duration::from_secs(2));
    assert!(
        !leaked,
        "看锚的客户端不该收到另一个 feeder(workspace)的字节"
    );

    let _ = ws.close(None);
    drop(fa);
    drop(fb);
    k.cleanup();
}

/// F2b:断一个非最后 feeder → kernel 存活;断最后一个 feeder → kernel 退出 + 清会合 socket 文件。
#[test]
fn federated_last_feeder_disconnect_exits_and_cleans_socket() {
    let port = free_port();
    let mut k = FedKernel::spawn(port);

    let mut fa = k.connect_feeder();
    declare(&mut fa, 100, 1, &[(1, "alpha")]);
    std::thread::sleep(Duration::from_millis(150));
    let mut fb = k.connect_feeder();
    declare(&mut fb, 200, 2, &[(2, "beta")]);
    std::thread::sleep(Duration::from_millis(300));

    // 断 B(非最后):kernel 应存活 → 仍能 connect 会合 socket + child 未退。
    drop(fb);
    std::thread::sleep(Duration::from_millis(500));
    assert!(
        matches!(k.child.try_wait(), Ok(None)),
        "断非最后 feeder 后 kernel 应存活"
    );
    // 会合 socket 仍在、仍可连(证 kernel loop 在跑)。
    let probe = UnixStream::connect(&k.sock);
    assert!(probe.is_ok(), "非最后 feeder 断后会合 socket 仍应可连");
    drop(probe); // 这条探测连接也是个 feeder(未声明 ws),随即断开。
    std::thread::sleep(Duration::from_millis(300));

    // 此刻剩 feeder A(探测连接已断)。断 A → 最后一个 → kernel 退出。
    drop(fa);

    // 等 kernel 退出 + 清 socket 文件。
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut exited = false;
    while Instant::now() < deadline {
        if let Ok(Some(_)) = k.child.try_wait() {
            exited = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let sock_gone = !k.sock.exists();
    let dir = k.dir.clone();
    // 已 reap(try_wait 收了),直接清目录(cleanup 会再 wait,幂等)。
    if !exited {
        // SAFETY: kill 已知子进程。
        unsafe {
            libc::kill(k.child.id() as i32, libc::SIGKILL);
        }
    }
    let _ = k.child.wait();
    let _ = std::fs::remove_dir_all(&dir);

    assert!(exited, "断最后一个 feeder 后 kernel 应退出(F2b)");
    assert!(sock_gone, "kernel 退出应清理会合 socket 文件(F2b)");
}

/// 数 kernel 进程当前打开的 fd 数(Linux `/proc/<pid>/fd`)。用于 fd 泄漏回归。
fn kernel_fd_count(k: &FedKernel) -> usize {
    let path = format!("/proc/{}/fd", k.child.id());
    std::fs::read_dir(&path).map(|it| it.count()).unwrap_or(0)
}

/// commit A 回归(联邦 fd 双关 bug):`make_feeder` 在 READ 源已注册后若后段失败,必须**事务化回滚**
/// —— 移除已注册 READ 源(关 read_fd)+ 关它 dup 出的写端,且 `attach_feeder` **不再** double-close
/// (旧 bug:READ 源 own 着 raw 的同时 attach 又 `libc::close(raw)` → 双关 + epoll 悬挂已关 fd)。
/// 注入 `QUILL_TEST_FEEDER_FAIL_AFTER_READ`(仅 debug 构建生效)让**每个**会合接入都走"后段失败→回滚"
/// 路径,验:(a) kernel 反复失败接入后仍存活(无双关致崩 / 无 epoll 悬挂源);(b) kernel 进程 fd 数
/// 不随失败接入增长(回滚真关了 read_fd + dup 写端,无泄漏)。
#[test]
fn federated_make_feeder_failure_rolls_back_no_leak_no_crash() {
    let port = free_port();
    let mut k = FedKernel::spawn_with_env(port, &[("QUILL_TEST_FEEDER_FAIL_AFTER_READ", "1")]);

    // 预热:先来一次失败接入,让 kernel 把稳态 fd(WS/会合 listener、signal、timer…)都建好再取基线。
    if let Ok(s) = UnixStream::connect(&k.sock) {
        std::thread::sleep(Duration::from_millis(50));
        drop(s);
    }
    std::thread::sleep(Duration::from_millis(250));
    let base = kernel_fd_count(&k);
    assert!(base > 0, "应能读到 kernel /proc fd 数(Linux)");

    // 反复接入:每次 kernel 侧 accept → attach_feeder → make_feeder 后段失败 → 事务化回滚。
    for _ in 0..25 {
        if let Ok(s) = UnixStream::connect(&k.sock) {
            std::thread::sleep(Duration::from_millis(20)); // 给 kernel 处理 accept+失败+回滚。
            drop(s);
        }
    }
    std::thread::sleep(Duration::from_millis(400));

    // (a) kernel 仍存活(无双关致崩 / 无 epoll 悬挂源致 panic)。
    let alive = matches!(k.child.try_wait(), Ok(None));
    // (b) 无 fd 泄漏:回滚关了 read_fd + dup 的写端;attach 不再 double-close。
    let after = kernel_fd_count(&k);
    let no_leak = after <= base + 3;

    k.cleanup();

    assert!(
        alive,
        "反复 make_feeder 后段失败后 kernel 应存活(无双关致崩 / 无 epoll 悬挂源)"
    );
    assert!(
        no_leak,
        "make_feeder 失败回滚不应泄漏 fd(基线 {base},25 次失败接入后 {after})"
    );
}

/// 在 deadline 内从一个 feeder 的 back-channel(全双工 socket 读向)读一帧;无则 None。
fn recv_feeder_frame(
    stream: &mut UnixStream,
    dec: &mut FeedDecoder,
    timeout: Duration,
) -> Option<FeedFrame> {
    let deadline = Instant::now() + timeout;
    let mut buf = [0u8; 4096];
    loop {
        match dec.next_frame() {
            Ok(Some(f)) => return Some(f),
            Ok(None) => {}
            Err(e) => panic!("feeder back-channel decode 错位: {e}"),
        }
        if Instant::now() >= deadline {
            return None;
        }
        match stream.read(&mut buf) {
            Ok(0) => return None,
            Ok(n) => dec.push(&buf[..n]),
            Err(e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut =>
            {
                continue
            }
            Err(_) => return None,
        }
    }
}

/// F3:手机 `TabOp::New` → 路由到【锚 feeder】(第一个接入的窗口 = home),经其 back-channel 回灌
/// TabOp::New 帧;非锚 feeder 收不到(新 tab 落锚,不是客户端在看的 workspace)。Close 仍走各自 feeder。
#[test]
fn federated_new_tab_routes_to_anchor_feeder() {
    let port = free_port();
    let k = FedKernel::spawn(port);

    // 锚 feeder A(先接入)= ws 100;feeder B = ws 200。
    let mut fa = k.connect_feeder();
    declare(&mut fa, 100, 1, &[(1, "alpha")]);
    std::thread::sleep(Duration::from_millis(200));
    let mut fb = k.connect_feeder();
    declare(&mut fb, 200, 2, &[(2, "beta")]);
    std::thread::sleep(Duration::from_millis(300));

    let mut ws = match connect_ws(port, Duration::from_secs(15)) {
        Some(w) => w,
        None => {
            k.cleanup();
            panic!("15s 内未能连上联邦 kernel WS");
        }
    };
    // 等连上引导帧处理完(客户端 attach 到锚 A)。
    let _ = ws_recv_text_until(&mut ws, &["\"Workspaces\""], Duration::from_secs(10));

    // 手机发 New → 应回灌【锚 A】(不是客户端在看的 workspace,虽此处恰好也是 A)。
    let new = serde_json::to_string(&ClientMsg::TabOp {
        workspace_id: 100,
        op: TabOp::New,
    })
    .expect("ser New");
    ws.send(Message::Text(new.into())).expect("send New");
    let _ = ws.flush();

    // 锚 A 的 back-channel 应收到 TabOp::New 帧。
    let mut dec_a = FeedDecoder::new();
    let got_a = recv_feeder_frame(&mut fa, &mut dec_a, Duration::from_secs(10));
    let got_a = got_a.expect("锚 feeder 应收到 New 的 TabOp 回灌帧");
    assert_eq!(got_a.kind, FrameKind::TabOp, "锚回灌帧 kind 应为 TabOp");
    assert_eq!(
        decode_tab_op(&got_a.payload),
        Some(FeedTabOp::New),
        "锚回灌 payload 应为 New"
    );

    // 非锚 B 的 back-channel 不该收到任何帧(2s 静默)。
    let mut dec_b = FeedDecoder::new();
    let leaked_b = recv_feeder_frame(&mut fb, &mut dec_b, Duration::from_secs(2));
    assert!(
        leaked_b.is_none(),
        "非锚 feeder 不该收到 New 回灌帧(新 tab 落锚,F3)"
    );

    let _ = ws.close(None);
    drop(fa);
    drop(fb);
    k.cleanup();
}
