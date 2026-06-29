//! Phase 7 片1 WS **字节流** 传输垂直切片端到端验收 (ADR-0015 R1)。
//!
//! 起真 `quill-kernel` 子进程(同口 HTTP + WS 端点),验两条:
//! 1. **同口 HTTP**:裸 TCP `GET /` 拿到 200 + 含 xterm.js 的页面(`/vendor/xterm.js`
//!    引用 + WebSocket 接线),证「普通 GET 出网页、Upgrade 走 WS」同口分流。
//! 2. **WS 字节流 + 连接保持**:让 daemon 的 tab(SHELL 覆写成循环 printf 已知
//!    MARKER 的脚本)持续产出**已知字节**;tungstenite client 连上收**二进制帧**,
//!    断言收到 MARKER(含连上重放 + 之后 live),且 **sleep 一段后仍能继续收到**
//!    新 MARKER —— 证连接保持的持续直播(非 T3a「发一帧即关」)。
//!
//! 用 SHELL 覆写脚本注入确定性输出:不依赖输入回灌(T5),也不靠真 shell 的 rc /
//! prompt(非确定)。tests/ 允许 `unwrap`/`expect`(CLAUDE.md 仅约束 src/)。

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::unix::fs::PermissionsExt;
use std::time::{Duration, Instant};

use tungstenite::stream::MaybeTlsStream;
use tungstenite::{Message, WebSocket};

const MARKER: &str = "QUILL_BYTES_MARKER";

/// 抓一个空闲 TCP 端口(bind :0 拿到后立刻释放,把端口号交给 daemon)。
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

/// 写一个可执行 shell 脚本:循环 printf 已知 MARKER(并保持 PTY 打开,daemon 不退)。
/// daemon 经 `$SHELL -li` spawn 它 → 确定性产出字节流,无需输入回灌。
fn write_marker_shell(path: &std::path::Path) {
    let script = format!("#!/bin/sh\nwhile :; do printf '{MARKER}\\n'; sleep 0.2; done\n");
    std::fs::write(path, script).expect("写 marker shell 脚本");
    let mut perm = std::fs::metadata(path).expect("stat 脚本").permissions();
    perm.set_mode(0o755);
    std::fs::set_permissions(path, perm).expect("chmod +x 脚本");
}

fn spawn_daemon(sock: &std::path::Path, shell: &std::path::Path, port: u16) -> std::process::Child {
    std::process::Command::new(env!("CARGO_BIN_EXE_quill-kernel"))
        .arg(format!("--socket={}", sock.display()))
        .arg(format!("--ws-bind=127.0.0.1:{port}"))
        .env("SHELL", shell)
        .env("RUST_LOG", "quill=warn")
        .spawn()
        .expect("spawn quill-kernel daemon")
}

/// 裸 TCP 发一个最小 HTTP GET,读全响应(到 EOF / 超时)。
fn http_get(port: u16, path: &str, timeout: Duration) -> Option<String> {
    let deadline = Instant::now() + timeout;
    loop {
        if Instant::now() >= deadline {
            return None;
        }
        let mut stream = match TcpStream::connect(("127.0.0.1", port)) {
            Ok(s) => s,
            Err(_) => {
                std::thread::sleep(Duration::from_millis(50));
                continue;
            }
        };
        let req = format!("GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n");
        if stream.write_all(req.as_bytes()).is_err() {
            std::thread::sleep(Duration::from_millis(50));
            continue;
        }
        let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
        let mut resp = Vec::new();
        let _ = stream.read_to_end(&mut resp);
        if !resp.is_empty() {
            return Some(String::from_utf8_lossy(&resp).into_owned());
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn connect_ws(url: &str, timeout: Duration) -> Option<WebSocket<MaybeTlsStream<TcpStream>>> {
    let deadline = Instant::now() + timeout;
    loop {
        if Instant::now() >= deadline {
            return None;
        }
        match tungstenite::connect(url) {
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
                // 防累积无界:只保留尾部足够匹配 needle 的窗口。
                if acc.len() > 64 * 1024 {
                    let keep = needle.len().max(1);
                    acc.drain(..acc.len() - keep);
                }
            }
            Ok(Message::Close(_)) => return false,
            Ok(_) => {}
            Err(_) => {} // read timeout:循环再读直到 deadline
        }
    }
    false
}

#[test]
fn daemon_serves_xterm_html_on_same_port() {
    let dir = std::env::temp_dir().join(format!("quill-bytes-html-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("建临时目录");
    let sock = dir.join("kernel.sock");
    let shell = dir.join("shell.sh");
    write_marker_shell(&shell);
    let port = free_port();
    let mut child = spawn_daemon(&sock, &shell, port);

    let cleanup = |child: &mut std::process::Child, dir: &std::path::Path| {
        send_signal(child.id(), libc::SIGTERM);
        let _ = child.wait();
        let _ = std::fs::remove_dir_all(dir);
    };

    let page = match http_get(port, "/", Duration::from_secs(15)) {
        Some(p) => p,
        None => {
            cleanup(&mut child, &dir);
            panic!("15s 内 GET / 无响应");
        }
    };

    let ok = page.starts_with("HTTP/1.1 200")
        && page.contains("/vendor/xterm.js")
        && page.contains("WebSocket")
        && page.contains("term.write");

    // 顺带验 vendored 资产可取(xterm.js 真在同口 serve)。
    let xtermjs = http_get(port, "/vendor/xterm.js", Duration::from_secs(5)).unwrap_or_default();
    let xterm_ok = xtermjs.starts_with("HTTP/1.1 200") && xtermjs.contains("Terminal");

    cleanup(&mut child, &dir);
    assert!(
        ok,
        "GET / 应返回含 xterm.js 接线的 200 页面;实际头部:\n{}",
        &page[..page.len().min(400)]
    );
    assert!(xterm_ok, "GET /vendor/xterm.js 应返回 200 + xterm.js 源");
}

#[test]
fn daemon_streams_known_pty_bytes_over_ws_and_keeps_alive() {
    let dir = std::env::temp_dir().join(format!("quill-bytes-ws-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("建临时目录");
    let sock = dir.join("kernel.sock");
    let shell = dir.join("shell.sh");
    write_marker_shell(&shell);
    let port = free_port();
    let mut child = spawn_daemon(&sock, &shell, port);

    let url = format!("ws://127.0.0.1:{port}/");
    let cleanup = |child: &mut std::process::Child, dir: &std::path::Path| {
        send_signal(child.id(), libc::SIGTERM);
        let _ = child.wait();
        let _ = std::fs::remove_dir_all(dir);
    };

    let mut ws = match connect_ws(&url, Duration::from_secs(15)) {
        Some(w) => w,
        None => {
            cleanup(&mut child, &dir);
            panic!("15s 内未能连上 WS {url}");
        }
    };

    // 阶段 1:连上后(重放 + 早期 live)应收到已知 MARKER 字节(二进制帧)。
    let phase1 = ws_recv_until(&mut ws, MARKER.as_bytes(), Duration::from_secs(10));

    // 阶段 2:连接保持。歇一会儿让脚本继续产出新 MARKER,再读 —— 仍应收到。
    // 这区分「持续直播(连接保持)」与 T3a「发一帧即关」(后者此处会收到 Close)。
    std::thread::sleep(Duration::from_millis(700));
    let phase2 = ws_recv_until(&mut ws, MARKER.as_bytes(), Duration::from_secs(10));

    let _ = ws.close(None);

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

    assert!(phase1, "连上后应经 WS 二进制帧收到已知 MARKER 字节");
    assert!(
        phase2,
        "连接应保持:歇一会儿后仍能收到新 MARKER(持续直播,非发一帧即关)"
    );
    assert!(
        gone,
        "daemon 退出后应清理 unix socket 文件 {}",
        sock.display()
    );
    assert!(
        status.success() || status.code().is_some(),
        "daemon 应优雅退出 (非被信号杀死),实际: {status:?}"
    );
}
