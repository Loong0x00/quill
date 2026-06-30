//! Phase 7 WS 传输垂直切片端到端验收(片1 字节流直播 + 片2 输入回灌,
//! ADR-0015 R1 + ADR-0016 calloop 单线程化)。
//!
//! 起真 `quill-kernel` 子进程(同口 HTTP + WS 端点,全 calloop 无线程),验四条:
//! 1. **同口 HTTP**:裸 TCP `GET /` 拿到 200 + 含 xterm.js 的页面,证「普通 GET 出
//!    网页、Upgrade 走 WS」同口分流。
//! 2. **WS 字节流 + 连接保持**:tab(SHELL 覆写成循环 printf 已知 MARKER 的脚本)持续
//!    产出已知字节;client 连上收**二进制帧**(连上重放 + 之后 live),歇一会儿仍能收到
//!    新 MARKER —— 证连接保持的持续直播。
//! 3. **输入往返(片2)**:SHELL 覆写成 read-echo 脚本;client 经 WS 发 `RUNDONE\n`,
//!    daemon `on_input` 写 active tab PTY,脚本读到后 `printf GOT[...]`,client 收回 —
//!    证浏览器 → daemon → PTY 的输入回灌闭环(无 channel、无线程)。
//! 4. **慢客户端背压回收**:SHELL 覆写成高速 spew 脚本;client 连上后**完全不读**,
//!    daemon 该连接出站积压超 cap → 断开回收;client 随后读到连接关闭(非无限流)—
//!    单线程下 drop 同步发生在 fan-out,不依赖线程时序。
//!
//! 用 SHELL 覆写脚本注入确定性输入/输出。tests/ 允许 `unwrap`/`expect`(CLAUDE.md
//! 仅约束 src/)。

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::unix::fs::PermissionsExt;
use std::time::{Duration, Instant};

use tungstenite::stream::MaybeTlsStream;
use tungstenite::{Bytes, Error as WsError, Message, WebSocket};

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

fn write_exec_script(path: &std::path::Path, body: &str) {
    std::fs::write(path, body).expect("写 shell 脚本");
    let mut perm = std::fs::metadata(path).expect("stat 脚本").permissions();
    perm.set_mode(0o755);
    std::fs::set_permissions(path, perm).expect("chmod +x 脚本");
}

/// 循环 printf 已知 MARKER(并保持 PTY 打开,daemon 不退)。确定性产出字节流。
fn write_marker_shell(path: &std::path::Path) {
    write_exec_script(
        path,
        &format!("#!/bin/sh\nwhile :; do printf '{MARKER}\\n'; sleep 0.2; done\n"),
    );
}

/// read-echo:逐行读 stdin(PTY),回 `GOT[<line>]`。验输入回灌往返。
fn write_read_echo_shell(path: &std::path::Path) {
    write_exec_script(
        path,
        "#!/bin/sh\nwhile IFS= read -r line; do printf 'GOT[%s]\\n' \"$line\"; done\n",
    );
}

/// 高速 spew:`yes` 大行,尽快灌满出站。验慢客户端背压回收。
fn write_spew_shell(path: &std::path::Path) {
    let line = "A".repeat(200);
    write_exec_script(path, &format!("#!/bin/sh\nexec yes '{line}'\n"));
}

/// 静默 shell:`exec cat` 读 PTY(无输入→阻塞,零输出→daemon 空闲基线 CPU≈0);daemon 关
/// PTY 时 cat 收 EOF 自退(测试自清,不留孤儿)。用于忙等 / 收割测试(需要 daemon 真空闲)。
fn write_quiet_shell(path: &std::path::Path) {
    write_exec_script(path, "#!/bin/sh\nexec cat\n");
}

/// 子进程累计 CPU 时间(utime+stime,单位 jiffies)。读 `/proc/<pid>/stat` 第 14/15 字段;
/// comm(第 2 字段)可能含空格/括号,故按【最后一个 ')'】切分后再数字段(其后第一个
/// token 是字段 3=state,故 utime=tokens[11]、stime=tokens[12])。
fn proc_cpu_jiffies(pid: u32) -> u64 {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap_or_default();
    let rparen = match stat.rfind(')') {
        Some(i) => i,
        None => return 0,
    };
    let toks: Vec<&str> = stat[rparen + 1..].split_whitespace().collect();
    let utime = toks
        .get(11)
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(0);
    let stime = toks
        .get(12)
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(0);
    utime + stime
}

/// `sysconf(_SC_CLK_TCK)`(jiffies/秒,通常 100),用于把 jiffies 折算成毫秒。
fn clk_tck() -> u64 {
    // SAFETY: sysconf 只读系统常量,无内存安全问题。tests crate 无 deny(unsafe_code)。
    let v = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
    if v <= 0 {
        100
    } else {
        v as u64
    }
}

/// 等 daemon 端口可连(起进程有延迟),返一条已连 TcpStream。
fn connect_tcp(port: u16, timeout: Duration) -> Option<TcpStream> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        match TcpStream::connect(("127.0.0.1", port)) {
            Ok(s) => return Some(s),
            Err(_) => std::thread::sleep(Duration::from_millis(50)),
        }
    }
    None
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
    std::thread::sleep(Duration::from_millis(700));
    let phase2 = ws_recv_until(&mut ws, MARKER.as_bytes(), Duration::from_secs(10));

    let _ = ws.close(None);

    // SIGTERM → daemon 停 loop + 清理 unix socket(优雅退出,无线程可 join)。
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

/// 片2 输入回灌:WS → daemon `on_input` → PTY → read-echo 脚本 → WS 回程。
#[test]
fn daemon_round_trips_input_over_ws() {
    let dir = std::env::temp_dir().join(format!("quill-input-ws-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("建临时目录");
    let sock = dir.join("kernel.sock");
    let shell = dir.join("shell.sh");
    write_read_echo_shell(&shell);
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

    // 经 WS 发一行输入(裸字节二进制帧,与浏览器 onData→ws.send 同形)。
    if ws
        .send(Message::Binary(Bytes::from_static(b"RUNDONE\n")))
        .is_err()
    {
        cleanup(&mut child, &dir);
        panic!("WS send 输入失败");
    }
    let _ = ws.flush();

    // 脚本读到该行后回 `GOT[RUNDONE]`(经 PTY 回到 WS)。收到即证输入真写进了 PTY。
    let got = ws_recv_until(&mut ws, b"GOT[RUNDONE]", Duration::from_secs(15));

    let _ = ws.close(None);
    cleanup(&mut child, &dir);

    assert!(
        got,
        "经 WS 发的输入应写进 PTY,read-echo 脚本应回 GOT[RUNDONE](输入回灌往返)"
    );
}

/// 慢客户端背压:连上后完全不读,daemon 出站积压超 cap → 断开回收;client 随后应
/// 读到连接关闭(非无限流)。单线程下 drop 同步发生在 fan-out,确定性优于线程版。
#[test]
fn daemon_drops_slow_client_over_cap() {
    let dir = std::env::temp_dir().join(format!("quill-slow-ws-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("建临时目录");
    let sock = dir.join("kernel.sock");
    let shell = dir.join("shell.sh");
    write_spew_shell(&shell);
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

    // 故意完全不读:daemon 高速 spew → 该连接出站积压撑过内核缓冲 + cap → 被断开。
    // 给足时间让积压超 1 MiB(yes 经 PTY 数 MB/s,数秒足够)。
    std::thread::sleep(Duration::from_secs(6));

    // 现在开始读:先排空 client 端内核缓冲里已缓存的帧,随后应撞上 daemon 关闭的连接
    //(FIN/RST → Err 或 Close 帧)。若 daemon **没**断开(回归),则会无限流到 deadline。
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut closed = false;
    while Instant::now() < deadline {
        match ws.read() {
            Ok(Message::Close(_)) => {
                closed = true;
                break;
            }
            Ok(_) => continue, // 缓存的数据帧,排空它
            Err(WsError::Io(e))
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut =>
            {
                // read timeout:还有缓存或刚好没数据,继续等。
                continue;
            }
            Err(_) => {
                // ConnectionClosed / reset 等 → daemon 已断开回收本连接。
                closed = true;
                break;
            }
        }
    }

    cleanup(&mut child, &dir);
    assert!(
        closed,
        "完全不读的慢客户端应在出站积压超 cap 后被 daemon 断开回收(读到连接关闭)"
    );
}

/// 忙等回归(must-fix):发**半截 HTTP 头**后停住,daemon **不应忙等**。
///
/// 旧码用 `TcpStream::peek`(MSG_PEEK 不消费内核缓冲)+ `Mode::Level`:半截头令 fd 恒可读 →
/// calloop 每轮 ~0 超时重派 `ws_peek` → 烧满一个核(实测 99.3%)。新码消费式读把 fd 排空 →
/// Level 不再触发 → 静默等下次新字节。
///
/// **确定性判据**(非瞬时 CPU%):测 daemon 在固定 wall 窗口内**累计消耗的 CPU 时间**
/// (`/proc/<pid>/stat` utime+stime jiffies)。忙等会在 2s 窗里吃满 ~2000ms CPU,健康
/// daemon <~100ms;门设 500ms,两侧各 4x 余量,稳不 flaky。
#[test]
fn daemon_does_not_busy_spin_on_partial_header() {
    let dir = std::env::temp_dir().join(format!("quill-spin-ws-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("建临时目录");
    let sock = dir.join("kernel.sock");
    let shell = dir.join("shell.sh");
    write_quiet_shell(&shell); // 不产 PTY 输出 → daemon 空闲基线 CPU≈0
    let port = free_port();
    let mut child = spawn_daemon(&sock, &shell, port);
    let pid = child.id();

    let cleanup = |child: &mut std::process::Child, dir: &std::path::Path| {
        send_signal(child.id(), libc::SIGTERM);
        let _ = child.wait();
        let _ = std::fs::remove_dir_all(dir);
    };

    let mut stream = match connect_tcp(port, Duration::from_secs(15)) {
        Some(s) => s,
        None => {
            cleanup(&mut child, &dir);
            panic!("15s 内无法连上 daemon");
        }
    };

    // 发半截头(**无**结尾空行 \r\n\r\n)后停住不再发。
    if stream.write_all(b"GET / HTTP/1.1\r\nHost: x\r\n").is_err() {
        cleanup(&mut child, &dir);
        panic!("写半截头失败");
    }
    let _ = stream.flush();

    // 让 daemon 处理半截头(消费→WouldBlock→Continue)进入稳定态,再开始测 CPU。
    std::thread::sleep(Duration::from_millis(500));
    let before = proc_cpu_jiffies(pid);
    std::thread::sleep(Duration::from_secs(2));
    let after = proc_cpu_jiffies(pid);

    // stream 保活到测量结束(别提前 drop 触发回收改变行为)。
    drop(stream);
    cleanup(&mut child, &dir);

    let cpu_ms = after.saturating_sub(before) * 1000 / clk_tck();
    assert!(
        cpu_ms < 500,
        "半截 HTTP 头后 daemon 不应忙等:2s 窗内消耗 {cpu_ms}ms CPU(忙等会 ~2000ms)"
    );
}

/// 收割回归(顺带修):卡握手连接超 `handshake_deadline` 被回收。
///
/// 发半截头后停住,该连接永远卡在 Peeking 阶段;收割 Timer 在 deadline 后 `loop_handle.remove`
/// 其 READ 源(关 original fd)→ 客户端读到 EOF/RST。env 注入短超时(600ms deadline /
/// 200ms 扫描)让测试快且确定:回收→client `read` 速返 EOF;若没回收(回归)→ client `read`
/// 阻塞到 5s 读超时 = 失败。
#[test]
fn daemon_reaps_stuck_handshake_connection() {
    let dir = std::env::temp_dir().join(format!("quill-reap-ws-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("建临时目录");
    let sock = dir.join("kernel.sock");
    let shell = dir.join("shell.sh");
    write_quiet_shell(&shell);
    let port = free_port();
    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_quill-kernel"))
        .arg(format!("--socket={}", sock.display()))
        .arg(format!("--ws-bind=127.0.0.1:{port}"))
        .env("SHELL", &shell)
        .env("RUST_LOG", "quill=warn")
        .env("QUILL_WS_REAP_MS", "200")
        .env("QUILL_WS_HANDSHAKE_DEADLINE_MS", "600")
        .spawn()
        .expect("spawn quill-kernel daemon");

    let cleanup = |child: &mut std::process::Child, dir: &std::path::Path| {
        send_signal(child.id(), libc::SIGTERM);
        let _ = child.wait();
        let _ = std::fs::remove_dir_all(dir);
    };

    let mut stream = match connect_tcp(port, Duration::from_secs(15)) {
        Some(s) => s,
        None => {
            cleanup(&mut child, &dir);
            panic!("15s 内无法连上 daemon");
        }
    };
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));

    // 发半截头后停住(不发结尾 \r\n\r\n)。
    if stream.write_all(b"GET / HTTP/1.1\r\nHost: x\r\n").is_err() {
        cleanup(&mut child, &dir);
        panic!("写半截头失败");
    }
    let _ = stream.flush();

    // deadline 600ms + 扫描 200ms → 应在 ~1s 内被回收。read 阻塞直到对端关(Ok(0))/
    // reset(Err 非超时)/ 5s 读超时(= 没回收 = 失败)。
    let mut buf = [0u8; 64];
    let r = stream.read(&mut buf);
    cleanup(&mut child, &dir);

    let reclaimed = match r {
        Ok(0) => true,  // FIN:被回收
        Ok(_) => false, // 半截头不该有响应
        Err(ref e) => !matches!(
            e.kind(),
            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
        ), // reset 等也算回收;唯独读超时 = 没回收
    };
    assert!(
        reclaimed,
        "卡握手连接应在 handshake_deadline 后被收割(读到 EOF/RST),实际 read={r:?}"
    );
}
