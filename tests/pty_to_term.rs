//! T-0301 端到端集成:spawn `echo hello` → `PtyHandle::read` → `TermState::advance`
//! → `term.grid()[Line(0)]` 应出现 `"hello"` 字面字符。
//!
//! 这个测试把 Phase 2 PTY 通路 + Phase 3 alacritty_terminal 接入两段连起来,
//! 不依赖 Wayland / wgpu / calloop。证明**整条 data plane** 从子进程 stdout
//! 到 `Term::grid()` 没断:PTY 字节被读到、被 `Processor::advance` 解析、写入
//! grid 的第一行。

use std::io;
use std::thread;
use std::time::{Duration, Instant};

use quill::pty::PtyHandle;
use quill::term::TermState;

#[test]
fn echo_hello_reaches_term_grid_first_line() {
    let mut pty = PtyHandle::spawn_program("echo", &["hello"], 80, 24).expect("spawn echo");
    let mut term = TermState::new(80, 24);

    // 循环 read 喂 term 到见 "hello" 或超时。典型耗时 <50ms (fork + exec + echo + pipe),
    // 2 秒超时是保险。
    let mut buf = [0u8; 256];
    let deadline = Instant::now() + Duration::from_secs(2);

    loop {
        if Instant::now() > deadline {
            panic!(
                "2 秒内未看到 'hello' 在 grid 第一行。当前 line 0 内容: {:?}",
                term.line_text(0)
            );
        }
        match pty.read(&mut buf) {
            Ok(0) => break, // EOF
            Ok(n) => {
                term.advance(&buf[..n]);
                // 检查第一行是否已含 "hello"。line_text 末尾会带空白(剩余 cols),
                // contains 足够。
                if term.line_text(0).contains("hello") {
                    break;
                }
            }
            Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(10));
                continue;
            }
            Err(ref e) if e.raw_os_error() == Some(libc::EIO) => break,
            Err(e) => panic!("read 非预期错误: {e} (kind={:?})", e.kind()),
        }
    }

    let line0 = term.line_text(0);
    assert!(
        line0.contains("hello"),
        "line 0 应含 'hello';实际: {line0:?}"
    );
    // 额外:光标应已前进过(不在 0,0),精确位置取决于 PTY 的 onlcr + echo 换行
    // 行为,不强断言具体坐标,只断"不是起点"即可证 advance 起作用。
    assert_ne!(term.cursor_point(), (0, 0), "喂过字节后光标不应还在 (0,0)");
}

/// 防回归:PTY 喂空切片 /  写的 ANSI 序列不该让 term panic 或卡死。
/// 用 `echo -e` 写一个 CR 序列,证 Term 处理 CR 能正确归 col 0。
#[test]
fn echo_cr_resets_cursor_column() {
    let mut pty = PtyHandle::spawn_program("printf", &["abc\\rxy"], 80, 24).expect("spawn printf");
    let mut term = TermState::new(80, 24);

    let mut buf = [0u8; 128];
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        if Instant::now() > deadline {
            break;
        }
        match pty.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                term.advance(&buf[..n]);
                if term.line_text(0).starts_with("xyc") {
                    break; // CR 后 "xy" 覆写前两个字符
                }
            }
            Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(10));
                continue;
            }
            Err(ref e) if e.raw_os_error() == Some(libc::EIO) => break,
            Err(e) => panic!("read err: {e}"),
        }
    }

    // `abc\rxy` → Term 先写 a b c, CR 回列 0, 然后 x y 覆写列 0 1 →
    // line 0 应以 "xyc" 开头(原 "abc" 的 c 没被覆写留下)。
    let line0 = term.line_text(0);
    assert!(
        line0.starts_with("xyc"),
        "CR 后覆写应产生 'xyc...', 实际: {line0:?}"
    );
}
