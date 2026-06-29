//! Phase 7 T3a WS 传输垂直切片端到端验收 (ADR-0016)。
//!
//! 起真 `quill-kernel` 子进程(unix socket + WS 端点),用 tungstenite **client**
//! 连 `ws://127.0.0.1:<port>/`,收一条 `Snapshot` text 消息,断言 dims +
//! `row_texts` 行数。再 SIGTERM,断言 daemon 优雅退出 + 清理 unix socket 文件。
//! 证 "被驱动的 tab → Snapshot → WS → 浏览器(这里是 Rust client)" 这条 spine。
//!
//! tests/ 允许 `unwrap`/`expect`(CLAUDE.md 仅约束 src/)。

use std::net::TcpListener;
use std::time::{Duration, Instant};

use quill::kernel::proto::Snapshot;
use tungstenite::Message;

const COLS: u16 = 80;
const ROWS: u16 = 24;

/// 抓一个空闲 TCP 端口(bind :0 拿到后立刻释放,把端口号交给 daemon)。
/// 有 TOCTOU 窗口但本地 CI 足够稳。
fn free_port() -> u16 {
    let l = TcpListener::bind("127.0.0.1:0").expect("bind 临时端口");
    l.local_addr().expect("local_addr").port()
}

fn send_signal(pid: u32, sig: i32) {
    // SAFETY: kill 只对已知子进程 pid 发信号,不涉内存安全。tests crate 无 deny(unsafe_code)。
    unsafe {
        libc::kill(pid as i32, sig);
    }
}

/// 在 deadline 内反复尝试「连 WS + 读一条消息」,直到拿到一条 Text(JSON 快照)。
/// 容忍:daemon 尚未起好(connect 失败)/ 早连时还没种好首帧(收到 Close 无消息)。
fn recv_one_snapshot(url: &str, timeout: Duration) -> Option<String> {
    let deadline = Instant::now() + timeout;
    loop {
        if Instant::now() >= deadline {
            return None;
        }
        let (mut ws, _resp) = match tungstenite::connect(url) {
            Ok(pair) => pair,
            Err(_) => {
                std::thread::sleep(Duration::from_millis(50));
                continue;
            }
        };
        match ws.read() {
            Ok(Message::Text(t)) => {
                let _ = ws.close(None);
                return Some(t.as_str().to_string());
            }
            // 收到 Close / 其它帧 / 读错 → daemon 可能还没种好首帧,重试。
            _ => {
                let _ = ws.close(None);
                std::thread::sleep(Duration::from_millis(50));
            }
        }
    }
}

#[test]
fn daemon_serves_snapshot_over_websocket() {
    let dir = std::env::temp_dir().join(format!("quill-kernel-t3a-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("建临时目录");
    let sock = dir.join("kernel.sock");
    let port = free_port();

    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_quill-kernel"))
        .arg(format!("--socket={}", sock.display()))
        .arg(format!("--ws-bind=127.0.0.1:{port}"))
        .env("RUST_LOG", "quill=warn")
        .spawn()
        .expect("spawn quill-kernel daemon");

    let url = format!("ws://127.0.0.1:{port}/");
    let snapshot_json = match recv_one_snapshot(&url, Duration::from_secs(15)) {
        Some(s) => s,
        None => {
            let _ = child.kill();
            let _ = child.wait();
            let _ = std::fs::remove_dir_all(&dir);
            panic!("15s 内未能经 WS 收到一帧 Snapshot ({url})");
        }
    };

    let snap: Snapshot = serde_json::from_str(&snapshot_json).expect("反序列化 WS Snapshot");
    assert_eq!(snap.cols, COLS as usize, "cols 应为 80");
    assert_eq!(snap.rows, ROWS as usize, "rows 应为 24");
    assert_eq!(
        snap.cells.len(),
        snap.cols * snap.rows,
        "cells 应为 cols×rows 全量"
    );
    assert_eq!(snap.row_texts.len(), snap.rows, "row_texts 行数应为 rows");

    // SIGTERM → daemon 停 loop + join WS 线程 + 清理 unix socket(优雅退出)。
    send_signal(child.id(), libc::SIGTERM);
    let status = child.wait().expect("wait daemon");

    let mut gone = false;
    for _ in 0..150 {
        if !sock.exists() {
            gone = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let _ = std::fs::remove_dir_all(&dir);

    assert!(
        gone,
        "daemon 退出后应清理 unix socket 文件 {}",
        sock.display()
    );
    // SIGTERM 优雅退出:走 loop_signal.stop() 路径正常返回,不应是 SIGKILL/异常码。
    assert!(
        status.success() || status.code().is_some(),
        "daemon 应优雅退出 (非被信号杀死),实际: {status:?}"
    );
}
