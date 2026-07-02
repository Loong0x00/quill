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
use std::os::fd::AsRawFd;
use std::os::unix::fs::PermissionsExt;
use std::time::{Duration, Instant};

use tungstenite::stream::MaybeTlsStream;
use tungstenite::{Bytes, ClientRequestBuilder, Error as WsError, Message, WebSocket};

const MARKER: &str = "QUILL_BYTES_MARKER";

/// P0 CSWSH:daemon 起 WS 后强制握手鉴权。测试用一个已知 token(经 `QUILL_SHARE_TOKEN` env 传给
/// kernel),连接 URL 带 `?t=<TEST_TOKEN>` 才能握手成功。
const TEST_TOKEN: &str = "test0token0deadbeef";

/// 带对 token 的本机 WS URL(正向用例的默认 URL)。
fn ws_url(port: u16) -> String {
    format!("ws://127.0.0.1:{port}/?t={TEST_TOKEN}")
}

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
        .env("QUILL_SHARE_TOKEN", TEST_TOKEN)
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

    let url = ws_url(port);
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

    let url = ws_url(port);
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

    let url = ws_url(port);
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
        .env("QUILL_SHARE_TOKEN", TEST_TOKEN)
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

/// 在已连 socket 上设极小 SO_RCVBUF:节流 TCP 接收窗口(令在飞字节远小于资产大小,使
/// "永不读 → 写源 WouldBlock 挂住"确定性成立),同时忠实复刻 slowloris-read 的小窗口手法。
fn set_rcvbuf(stream: &TcpStream, bytes: i32) {
    let fd = stream.as_raw_fd();
    let val = bytes;
    // SAFETY: setsockopt 只读栈上一个 c_int 的 size_of 字节(只读不写),fd 来自活着的 TcpStream;
    // 返回值忽略(尽力而为,clamp 到内核下限也无妨)。tests crate 无 deny(unsafe_code)。
    unsafe {
        libc::setsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_RCVBUF,
            std::ptr::addr_of!(val).cast::<libc::c_void>(),
            std::mem::size_of::<i32>() as libc::socklen_t,
        );
    }
}

/// 拆 HTTP 响应:返回 (Content-Length 头声明值, 实际收到的 body 字节数)。在**原始字节**上
/// 找 `\r\n\r\n`(body 长度不受 UTF-8 lossy 影响),头部按 ASCII 解析 Content-Length。
fn split_http(resp: &[u8]) -> (usize, usize) {
    let sep = match resp.windows(4).position(|w| w == b"\r\n\r\n") {
        Some(i) => i,
        None => return (0, 0),
    };
    let body_len = resp.len() - (sep + 4);
    let headers = String::from_utf8_lossy(&resp[..sep]);
    let content_length = headers
        .lines()
        .find_map(|l| {
            let l = l.trim();
            if l.to_ascii_lowercase().starts_with("content-length:") {
                l.split(':')
                    .nth(1)
                    .and_then(|v| v.trim().parse::<usize>().ok())
            } else {
                None
            }
        })
        .unwrap_or(0);
    (content_length, body_len)
}

/// HTTP 响应写源超时收割(must-fix):客户端发**完整** GET 请求(大资产 `/vendor/xterm.js`
/// ~283KB,远超内核收发缓冲)却**通告极小 TCP 窗口 + 永不读响应**(slowloris-read)。daemon
/// 在 dup fd 上建了非阻塞写源发响应,写满内核 send 缓冲后恒 `WouldBlock` 挂着 —— 该写源在
/// 分流时已被摘出 `clients`(不受 `MAX_WS_CONNS` 限、握手收割也扫不到),且 `WouldBlock` 时
/// 回调不再被派发(fd 不可写 → Level 不触发),只能靠收割 Timer 兜底。
///
/// **确定性判据**:env 注入短 deadline(600ms)+ 短扫描(200ms);客户端设极小 SO_RCVBUF
/// 节流(令在飞字节 << 283KB)且在 deadline 窗口内**完全不读**;窗口过后再排空整条连接到
/// 关闭,断言收到的 body **短于 Content-Length**(被收割截断)。若没收割(回归):客户端排空
/// 时 daemon 写源恢复 → 发完整 283KB → body == Content-Length(断言失败,逮住回归)。
#[test]
fn daemon_reaps_unread_http_response_writer() {
    let dir = std::env::temp_dir().join(format!("quill-http-reap-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("建临时目录");
    let sock = dir.join("kernel.sock");
    let shell = dir.join("shell.sh");
    write_quiet_shell(&shell); // daemon 空闲,不干扰
    let port = free_port();
    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_quill-kernel"))
        .arg(format!("--socket={}", sock.display()))
        .arg(format!("--ws-bind=127.0.0.1:{port}"))
        .env("SHELL", &shell)
        .env("RUST_LOG", "quill=warn")
        .env("QUILL_SHARE_TOKEN", TEST_TOKEN)
        .env("QUILL_WS_REAP_MS", "200")
        .env("QUILL_WS_HTTP_WRITE_DEADLINE_MS", "600")
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
    // 极小接收窗口:节流传输 → 在飞字节 << 283KB(确定性 partial)+ 复刻 slowloris-read 小窗口。
    set_rcvbuf(&stream, 4096);

    // 发完整请求(含结尾 \r\n\r\n),请求大资产 xterm.js(283KB > 内核 send 缓冲 → 必 WouldBlock)。
    if stream
        .write_all(b"GET /vendor/xterm.js HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n")
        .is_err()
    {
        cleanup(&mut child, &dir);
        panic!("写 GET 请求失败");
    }
    let _ = stream.flush();

    // deadline 窗口内完全不读:daemon 写满内核缓冲后写源恒 WouldBlock 挂着。
    // deadline 600ms + 扫描 200ms → ~1s 内被收割;睡 1.5s 留足余量。
    std::thread::sleep(Duration::from_millis(1500));

    // 现在排空整条连接到关闭(短读超时反复读直到 EOF/RST 或总 deadline)。
    let _ = stream.set_read_timeout(Some(Duration::from_millis(250)));
    let mut resp: Vec<u8> = Vec::new();
    let mut closed = false;
    let overall = Instant::now() + Duration::from_secs(15);
    let mut buf = [0u8; 8192];
    while Instant::now() < overall {
        match stream.read(&mut buf) {
            Ok(0) => {
                closed = true; // FIN:被收割(或回归路径发完整后正常关)
                break;
            }
            Ok(n) => resp.extend_from_slice(&buf[..n]),
            Err(ref e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut =>
            {
                continue; // 暂无新数据:继续等到关闭 / deadline
            }
            Err(_) => {
                closed = true; // RST 等也算关闭
                break;
            }
        }
    }

    cleanup(&mut child, &dir);

    let (content_length, body_len) = split_http(&resp);
    assert!(
        content_length > 0,
        "应收到含 Content-Length 的响应头;实际收到 {} 字节",
        resp.len()
    );
    assert!(
        closed,
        "永不读的 HTTP 客户端最终应被 daemon 关闭(收割写源 + 关 dup fd)"
    );
    assert!(
        body_len < content_length,
        "HTTP 写源应在 http_write_deadline 后被收割 → 响应被截断 \
         (收到 body {body_len} < Content-Length {content_length});\
         若 body == Content-Length 说明没收割(写源恢复发完整响应 = slowloris-read 回归)"
    );
}

/// T6 砖0 块C/D:连上应收到**控制面 Text 帧**(工作区列表 + 字节流 (workspace,tab) 标签),
/// 与数据面 Binary 字节分流。证 `ServerMsg::Workspaces/Workspace/StreamFocus` 真接线下发。
#[test]
fn daemon_sends_control_text_frames_on_connect() {
    let dir = std::env::temp_dir().join(format!("quill-ctrl-ws-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("建临时目录");
    let sock = dir.join("kernel.sock");
    let shell = dir.join("shell.sh");
    write_marker_shell(&shell);
    let port = free_port();
    let mut child = spawn_daemon(&sock, &shell, port);

    let url = ws_url(port);
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

    // 读若干帧,收集**控制面 Text 帧**(数据面是 Binary,被忽略)。应见工作区列表 / 字节流标签。
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut saw_workspaces = false;
    let mut saw_stream_focus = false;
    while Instant::now() < deadline && !(saw_workspaces && saw_stream_focus) {
        match ws.read() {
            Ok(Message::Text(t)) => {
                let s = t.as_str();
                if s.contains("\"Workspaces\"") {
                    saw_workspaces = true;
                }
                if s.contains("\"StreamFocus\"") && s.contains("workspace_id") {
                    saw_stream_focus = true;
                }
            }
            Ok(Message::Close(_)) => break,
            Ok(_) => {}  // Binary 数据帧:忽略
            Err(_) => {} // read timeout:继续读到 deadline
        }
    }

    let _ = ws.close(None);
    cleanup(&mut child, &dir);

    assert!(
        saw_workspaces,
        "连上应收到控制面 ServerMsg::Workspaces(工作区列表)Text 帧"
    );
    assert!(
        saw_stream_focus,
        "连上应收到控制面 ServerMsg::StreamFocus(字节流 workspace/tab 标签)Text 帧"
    );
}

/// T6 砖0 块C/D:**死客户端不泄漏 + anchor 在 → WS 全断不死**。远超 MAX_WS_CONNS(16)的
/// 顺序「连上→收 MARKER→断线(drop,不发 Close)」循环:若死连接不回收(`clients` 泄漏),
/// 第 17+ 次连接会被 daemon 满额拒绝 → connect 失败。全部成功 = 连接被回收(无泄漏)+ 工作区
/// 因 anchor(daemon 自锚)在每次断线后都还在直播(断线 = 非事件,不销毁)。
#[test]
fn daemon_reaps_disconnected_clients_and_anchor_keeps_alive() {
    let dir = std::env::temp_dir().join(format!("quill-churn-ws-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("建临时目录");
    let sock = dir.join("kernel.sock");
    let shell = dir.join("shell.sh");
    write_marker_shell(&shell);
    let port = free_port();
    let mut child = spawn_daemon(&sock, &shell, port);

    let url = ws_url(port);
    let cleanup = |child: &mut std::process::Child, dir: &std::path::Path| {
        send_signal(child.id(), libc::SIGTERM);
        let _ = child.wait();
        let _ = std::fs::remove_dir_all(dir);
    };

    // 20 > MAX_WS_CONNS(16):每次连断都回收才可能 20 连全成功。
    const CYCLES: usize = 20;
    let mut ok = 0usize;
    for _ in 0..CYCLES {
        match connect_ws(&url, Duration::from_secs(8)) {
            Some(mut ws) => {
                if ws_recv_until(&mut ws, MARKER.as_bytes(), Duration::from_secs(8)) {
                    ok += 1;
                }
                drop(ws); // 断线:drop TcpStream = FIN,无 WS Close 帧 → daemon 按断线(非事件)回收
                std::thread::sleep(Duration::from_millis(30)); // 让 daemon 事件循环处理 FIN 回收
            }
            None => break, // 连不上(疑似满额 = 泄漏)→ 留给断言抓
        }
    }

    cleanup(&mut child, &dir);
    assert_eq!(
        ok, CYCLES,
        "20(>MAX_WS_CONNS 16)次连断循环每次都应连上并收到 MARKER:\
         死连接被回收(无 clients/holder 泄漏)+ anchor 在工作区不死(断线非事件)"
    );
}

/// T6 砖0 块C/D:**显式 X 关闭(Release)回收该连接,但 anchor 保活、工作区不销毁**。
/// 发 `ClientMsg::Release` → daemon explicit 释放 holder(anchor 在 → 不归 0 → 不销毁)+ 回收
/// 本连接 → client 读到连接关闭;随后新连接仍能收 MARKER(只关了那个 view)。
#[test]
fn daemon_release_closes_connection_but_keeps_workspace() {
    let dir = std::env::temp_dir().join(format!("quill-release-ws-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("建临时目录");
    let sock = dir.join("kernel.sock");
    let shell = dir.join("shell.sh");
    write_marker_shell(&shell);
    let port = free_port();
    let mut child = spawn_daemon(&sock, &shell, port);

    let url = ws_url(port);
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

    // 显式 X 关闭:发 Release 控制帧(Text)。daemon 用连接 held_ws,消息里的 id 仅占位
    //(daemon 首个工作区 id=1);anchor 在 → 不销毁,只回收本连接。
    if ws
        .send(Message::Text("{\"Release\":{\"workspace_id\":1}}".into()))
        .is_err()
    {
        cleanup(&mut child, &dir);
        panic!("发 Release 控制帧失败");
    }
    let _ = ws.flush();

    // 该连接应被 daemon 回收 → client 读到关闭(Close / EOF / RST),排空缓冲 MARKER 后命中。
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut closed = false;
    while Instant::now() < deadline {
        match ws.read() {
            Ok(Message::Close(_)) => {
                closed = true;
                break;
            }
            Ok(_) => continue, // 缓冲的 MARKER 数据帧 / 控制帧:排空
            Err(WsError::ConnectionClosed) | Err(WsError::AlreadyClosed) => {
                closed = true;
                break;
            }
            Err(WsError::Io(e))
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut =>
            {
                continue
            }
            Err(_) => {
                closed = true; // RST 等也算回收
                break;
            }
        }
    }

    // anchor 保活:新连接仍能收 MARKER(Release 只关了那个 view,工作区没死)。
    let alive = match connect_ws(&url, Duration::from_secs(10)) {
        Some(mut ws2) => ws_recv_until(&mut ws2, MARKER.as_bytes(), Duration::from_secs(8)),
        None => false,
    };

    cleanup(&mut child, &dir);
    assert!(
        closed,
        "收到 Release(显式 X 关闭)后 daemon 应回收该连接(client 读到关闭)"
    );
    assert!(
        alive,
        "Release 只关那个 view;anchor 保活 → 工作区不销毁,新连接仍能收 MARKER"
    );
}

// ── P0 CSWSH 握手鉴权(token + Origin + Host)端到端验收 ────────────────────────────

/// 尝试一次 WS 握手,返回是否被 daemon **以 403 拒绝**(CSWSH 防线命中)。daemon 须已在跑
///(调用前先用带对 token 的连接确认 up),故单次尝试即确定:成功=非拒绝;`Http(403)`=正确拒绝。
fn ws_rejected_403<R: tungstenite::client::IntoClientRequest>(req: R) -> bool {
    match tungstenite::connect(req) {
        Ok((mut ws, _)) => {
            let _ = ws.close(None);
            false
        }
        Err(WsError::Http(resp)) => resp.status() == tungstenite::http::StatusCode::FORBIDDEN,
        Err(_) => false,
    }
}

/// 正向:带**对** token 的握手成功 + 收到控制面帧(证鉴权放行合法客户端)。
#[test]
fn daemon_accepts_ws_with_valid_token() {
    let dir = std::env::temp_dir().join(format!("quill-auth-ok-{}", std::process::id()));
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

    let ws = connect_ws(&ws_url(port), Duration::from_secs(15));
    let ok = match ws {
        Some(mut w) => {
            let got = ws_recv_until(&mut w, MARKER.as_bytes(), Duration::from_secs(10));
            let _ = w.close(None);
            got
        }
        None => false,
    };
    cleanup(&mut child, &dir);
    assert!(ok, "带对 token 的 WS 握手应成功并收到直播字节");
}

/// 负向:**缺 token**(裸 `ws://host/`,恶意网页 `new WebSocket` 的形态)→ 403 拒绝(CSWSH 核心)。
#[test]
fn daemon_rejects_ws_without_token() {
    let dir = std::env::temp_dir().join(format!("quill-auth-notok-{}", std::process::id()));
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

    // 先用对 token 确认 daemon up(排除"连不上"混淆)。
    let up = connect_ws(&ws_url(port), Duration::from_secs(15)).is_some();
    let rejected = ws_rejected_403(format!("ws://127.0.0.1:{port}/"));
    cleanup(&mut child, &dir);
    assert!(up, "前置:带对 token 应能连上(daemon up)");
    assert!(
        rejected,
        "缺 token 的 WS 握手应被 403 拒绝(CSWSH:恶意网页拿不到 token)"
    );
}

/// 负向:**错 token** → 403 拒绝。
#[test]
fn daemon_rejects_ws_with_wrong_token() {
    let dir = std::env::temp_dir().join(format!("quill-auth-badtok-{}", std::process::id()));
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

    let up = connect_ws(&ws_url(port), Duration::from_secs(15)).is_some();
    let rejected = ws_rejected_403(format!("ws://127.0.0.1:{port}/?t=wrongtokenvalue"));
    cleanup(&mut child, &dir);
    assert!(up, "前置:带对 token 应能连上(daemon up)");
    assert!(rejected, "错 token 的 WS 握手应被 403 拒绝");
}

/// 负向:token 对但 **Origin 跨源**(`http://evil.com`,恶意网页真实形态:浏览器自动带其页面
/// Origin)→ 403 拒绝(CSWSH 纵深防线)。
#[test]
fn daemon_rejects_ws_cross_origin() {
    let dir = std::env::temp_dir().join(format!("quill-auth-origin-{}", std::process::id()));
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

    let up = connect_ws(&ws_url(port), Duration::from_secs(15)).is_some();
    // 带对 token + 合法 Host(IP)但跨源 Origin → 应被拒。
    let uri: tungstenite::http::Uri = ws_url(port).parse().expect("parse ws uri");
    let evil = ClientRequestBuilder::new(uri).with_header("Origin", "http://evil.com");
    let rejected = ws_rejected_403(evil);
    cleanup(&mut child, &dir);
    assert!(up, "前置:带对 token 应能连上(daemon up)");
    assert!(
        rejected,
        "token 对但 Origin=evil.com 的握手应被 403 拒绝(CSWSH 纵深:跨源浏览器请求)"
    );
}
