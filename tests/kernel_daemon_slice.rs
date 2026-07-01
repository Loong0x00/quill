//! Phase 7 T2 daemon 垂直切片端到端验收。
//!
//! 两条互补:
//! 1. **进程内(确定性)**:spawn shell tab → 写已知字节(`echo MARKER`)→ drain
//!    → `Session::snapshot` → 断言 dims + cell 数 + `row_texts` 命中 marker。证
//!    "被驱动的 tab → Snapshot" 这半段,不引入子进程/socket 时序的 flaky。
//! 2. **子进程(真 daemon + socket)**:起 `quill-kernel` bin,连 socket 读一条
//!    Snapshot JSON,断言结构(dims / cell 数 / 行数)+ SIGTERM 后 socket 被清理。
//!    证 "Snapshot → unix socket → 客户端 + 退出清理" 这半段(内容非确定,只断结构)。
//!
//! tests/ 允许 `unwrap`/`expect`(CLAUDE.md 仅约束 src/)。

use std::io::{BufRead, BufReader};
use std::net::TcpListener;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::{Duration, Instant};

use quill::kernel::proto::Snapshot;
use quill::kernel::Session;
use quill::tab::{TabInstance, TabList};

const COLS: u16 = 80;
const ROWS: u16 = 24;
const MARKER: &str = "QUILL_T2_MARKER_42";

/// 非阻塞 drain master fd → on_pty_output(同 daemon 的 drain 语义)。
fn drain(session: &mut Session, id: u64) {
    let mut buf = [0u8; 4096];
    loop {
        let read = session.tabs_mut().active_mut().pty_mut().read(&mut buf);
        match read {
            Ok(0) => break,
            Ok(n) => {
                session.on_pty_output(id, &buf[..n]);
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        }
    }
}

#[test]
fn in_process_driven_tab_snapshot_contains_typed_marker() {
    let tab = TabInstance::spawn(COLS, ROWS).expect("spawn shell tab (CI 需 shell)");
    let mut session = Session::new(TabList::new(tab));
    let id = session.tabs().active().id().raw();

    // 等 prompt + 排掉启动噪声。
    std::thread::sleep(Duration::from_millis(400));
    drain(&mut session, id);

    session
        .on_input(id, format!("echo {MARKER}\n").as_bytes())
        .expect("写 echo 命令到 PTY");

    let deadline = Instant::now() + Duration::from_secs(5);
    let mut found = false;
    while Instant::now() < deadline {
        drain(&mut session, id);
        let snap = session.snapshot(id).expect("snapshot");
        if snap.row_texts.iter().any(|r| r.contains(MARKER)) {
            found = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }

    let snap = session.snapshot(id).expect("snapshot");
    assert_eq!(snap.cols, COLS as usize, "cols 应为 80");
    assert_eq!(snap.rows, ROWS as usize, "rows 应为 24");
    assert_eq!(
        snap.cells.len(),
        snap.cols * snap.rows,
        "cells 应为 cols×rows 全量"
    );
    assert_eq!(snap.row_texts.len(), snap.rows, "row_texts 行数应为 rows");
    assert!(
        found,
        "echo {MARKER} 的输出应出现在某行 row_texts;实际: {:?}",
        snap.row_texts
    );
}

fn connect_retry(path: &Path, timeout: Duration) -> Option<UnixStream> {
    let deadline = Instant::now() + timeout;
    loop {
        match UnixStream::connect(path) {
            Ok(s) => return Some(s),
            Err(_) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(50)),
            Err(_) => return None,
        }
    }
}

fn send_signal(pid: u32, sig: i32) {
    // SAFETY: kill 只对已知子进程 pid 发信号,不涉内存安全。tests crate 无 deny(unsafe_code)。
    unsafe {
        libc::kill(pid as i32, sig);
    }
}

/// 取一个空闲 TCP 端口(bind :0 让内核分配后立刻释放)。用于给 daemon `--ws-bind` 一个**独占**端口,
/// 避免撞默认 `0.0.0.0:7878`(用户 daily-driver `quill --share` 常驻占它)或并行测试互撞。
fn free_port() -> u16 {
    let l = TcpListener::bind("127.0.0.1:0").expect("bind 临时端口");
    l.local_addr().expect("local_addr").port()
}

#[test]
fn daemon_serves_snapshot_over_unix_socket() {
    let dir = std::env::temp_dir().join(format!("quill-kernel-t2-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("建临时目录");
    let sock = dir.join("kernel.sock");

    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_quill-kernel"))
        .arg(format!("--socket={}", sock.display()))
        // 独占空闲端口:不撞默认 7878(live share)/ 并行测试(测的是 unix socket 面,WS 面无关)。
        .arg(format!("--ws-bind=127.0.0.1:{}", free_port()))
        .env("RUST_LOG", "quill=warn")
        .spawn()
        .expect("spawn quill-kernel daemon");

    let stream = match connect_retry(&sock, Duration::from_secs(10)) {
        Some(s) => s,
        None => {
            let _ = child.kill();
            let _ = child.wait();
            let _ = std::fs::remove_dir_all(&dir);
            panic!("10s 内未能连上 daemon socket {}", sock.display());
        }
    };

    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    let n = reader.read_line(&mut line).expect("读 Snapshot JSON 行");
    assert!(n > 0, "应收到非空 Snapshot 行");

    let snap: Snapshot = serde_json::from_str(line.trim_end()).expect("反序列化 Snapshot");
    assert_eq!(snap.cols, COLS as usize);
    assert_eq!(snap.rows, ROWS as usize);
    assert_eq!(snap.cells.len(), snap.cols * snap.rows);
    assert_eq!(snap.row_texts.len(), snap.rows);

    // SIGTERM → daemon 停 loop + 清理 socket(优雅退出路径,非 SIGKILL)。
    send_signal(child.id(), libc::SIGTERM);
    let _ = child.wait();

    let mut gone = false;
    for _ in 0..100 {
        if !sock.exists() {
            gone = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let _ = std::fs::remove_dir_all(&dir);
    assert!(gone, "daemon 退出后应清理 socket 文件 {}", sock.display());
}
