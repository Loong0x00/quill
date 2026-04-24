//! T-0204 验收测试:`PtyHandle::resize` 确实把新尺寸推进 kernel winsize。
//!
//! 端到端思路:spawn `bash -c "echo cols=$(tput cols) lines=$(tput lines); exit"`,
//! bash 在打印 prompt 前会调 `TIOCGWINSZ` 读取 winsize,`tput cols` / `tput lines`
//! 反映出我们 spawn 时(或 resize 后)设给 PTY 的值。两条 case:
//! 1. 初始 80x24 不动,bash 应 echo `cols=80 lines=24`
//! 2. spawn 完立刻 `resize(120, 40)` —— 必须在 bash 运行 `tput` 之前 ticket
//!    Implementation notes 强调过 —— 再跑同样命令,断言 `cols=120 lines=40`
//!
//! 不走 calloop,不走 wayland;纯 `PtyHandle::read` 循环把字节收进 Vec<u8>,
//! UTF-8 解码后做子串断言。最多等 3 秒,bash 启动慢也能吃得下。

use std::io;
use std::time::{Duration, Instant};

use quill::pty::PtyHandle;

const SCRIPT: &str = "echo cols=$(tput cols) lines=$(tput lines); exit";

/// 循环非阻塞读 master,直到见到 "cols=" 子串或超时。PTY 会把 `\n` 转 `\r\n`,
/// 所以整个捕获按 byte 累积,最后用 UTF-8 解码。
fn read_until_cols_or_timeout(handle: &mut PtyHandle, deadline: Duration) -> Vec<u8> {
    let mut out = Vec::new();
    let mut buf = [0u8; 256];
    let start = Instant::now();
    loop {
        if start.elapsed() > deadline {
            break;
        }
        match handle.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                out.extend_from_slice(&buf[..n]);
                // 看到 "cols=" 并且后面跟着一个 '\n' 就可以停了(不确保完整,但足够给断言匹配)。
                if out.windows(5).any(|w| w == b"cols=") && out.contains(&b'\n') {
                    break;
                }
            }
            Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(10));
                continue;
            }
            Err(ref e) if e.raw_os_error() == Some(libc::EIO) => break,
            Err(e) => panic!("read 异常: {e} (kind={:?})", e.kind()),
        }
    }
    out
}

#[test]
fn spawned_size_reflects_in_tput() {
    let mut handle = PtyHandle::spawn_program("bash", &["-c", SCRIPT], 80, 24).expect("spawn bash");
    let out = read_until_cols_or_timeout(&mut handle, Duration::from_secs(3));
    let s = String::from_utf8_lossy(&out);
    assert!(s.contains("cols=80"), "应见 'cols=80', 实际输出:\n{s}");
    assert!(s.contains("lines=24"), "应见 'lines=24', 实际输出:\n{s}");
}

#[test]
fn resize_before_tput_changes_reported_size() {
    // spawn 时先走默认 80x24,然后 **立刻** resize(120, 40) —— 在 bash 调
    // `tput` 之前把 winsize 改掉。ticket Implementation notes:bash 本身不主动
    // 广播列数变化,readline / `tput` 是读 `TIOCGWINSZ`,resize 必须在 tput
    // 运行前生效;`bash -c "..."` 启动足够慢(fork + exec + 解析),resize
    // 拿到同步 ioctl 成功后,tput 再跑时一定看到新值。
    let mut handle = PtyHandle::spawn_program("bash", &["-c", SCRIPT], 80, 24).expect("spawn bash");
    handle.resize(120, 40).expect("resize(120, 40) 应成功");
    let out = read_until_cols_or_timeout(&mut handle, Duration::from_secs(3));
    let s = String::from_utf8_lossy(&out);
    assert!(
        s.contains("cols=120"),
        "resize 后应见 'cols=120', 实际输出:\n{s}"
    );
    assert!(
        s.contains("lines=40"),
        "resize 后应见 'lines=40', 实际输出:\n{s}"
    );
}

/// 防回归:`PtyHandle::resize` 参数顺序是 `(cols, rows)`,不要和 `PtySize`
/// 字段顺序 `rows, cols` 搞反。把一个明显不对称的尺寸传进去,从 tput 字节里
/// 看出来若对错位。
#[test]
fn resize_arg_order_cols_rows_not_swapped() {
    // 150 cols × 50 rows —— 如果实现里不慎把参数传反,tput 会报 cols=50 lines=150。
    let mut handle = PtyHandle::spawn_program("bash", &["-c", SCRIPT], 80, 24).expect("spawn");
    handle.resize(150, 50).expect("resize");
    let out = read_until_cols_or_timeout(&mut handle, Duration::from_secs(3));
    let s = String::from_utf8_lossy(&out);
    assert!(s.contains("cols=150"), "args 顺序错, 实际:\n{s}");
    assert!(s.contains("lines=50"), "args 顺序错, 实际:\n{s}");
    // 强断言:错的反向组合不应出现
    assert!(!s.contains("cols=50"), "参数反了, 实际:\n{s}");
    assert!(!s.contains("lines=150"), "参数反了, 实际:\n{s}");
}
