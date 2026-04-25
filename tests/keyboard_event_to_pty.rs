//! T-0501 集成测试: 验 [`PtyHandle::write`] 与 keyboard event 解码端到端通路。
//!
//! ## 测试策略
//!
//! src/wl/keyboard.rs 内 `#[cfg(test)] mod tests` 已覆盖派单 In #E 列举的 4
//! acceptance ('a' / Ctrl+C / Enter / Backspace) + 6 个补充 (Shift+a / Tab /
//! Shift only / release no-write / key before keymap / RepeatInfo) — 共 10
//! 单测走真 us xkbcommon keymap, 不需 mock。
//!
//! 本集成测试覆盖 **PtyHandle::write 真写真读** 端到端:
//! - spawn `cat` (PTY echo 模式: 写啥到 master 输入, slave 立即从 stdin 读到,
//!   再写回 stdout, master 读端拿到回声)
//! - `pty.write(b"hello\r")` 后 read 回路应见 `"hello"` 字面或 PTY echo 的
//!   `"hello\r\n"` (取决于 termios echo / icrnl)
//! - 验 INV-005 兼容: write 在子进程不消费时仍非阻塞 (master fd O_NONBLOCK,
//!   缓冲未满时 Ok(n), 满时 WouldBlock — 后者在小数据量罕见)
//!
//! 不依赖 Wayland / wgpu / calloop, 仅 PTY + libc。

use std::io;
use std::thread;
use std::time::{Duration, Instant};

use quill::pty::PtyHandle;

/// **核心端到端**: 写 ASCII 串 → 子进程 cat 回声 → master read 拿到回声。
///
/// PTY 默认 echo 开启, 所以 `pty.write(b"hello\r")` 在 cat 还没读到前就会
/// 因为 termios `ECHO` 见到回声; cat 进程从 stdin 读到 `"hello\n"` (icrnl
/// 把 \r 转 \n) 再写到 stdout 又一遍。最终 master read 大概率拿到至少
/// `"hello"` 字面 (echo 第一遍即可命中)。
#[test]
fn write_then_read_loops_back_via_cat_echo() {
    let mut handle = PtyHandle::spawn_program("cat", &[], 80, 24).expect("spawn cat");

    // 等 cat ready (fork/exec ~50ms)
    thread::sleep(Duration::from_millis(100));

    let written = handle.write(b"hello\r").expect("write 应成功");
    assert!(written > 0, "写 'hello\\r' 至少应写 1 字节");

    // 累 read 直到见到 "hello" 字面或超时
    let mut buffer = Vec::<u8>::with_capacity(128);
    let mut buf = [0u8; 256];
    let deadline = Instant::now() + Duration::from_secs(2);

    loop {
        if Instant::now() > deadline {
            panic!(
                "2 秒内未见 'hello' 回声。累计 buffer = {:?}",
                String::from_utf8_lossy(&buffer)
            );
        }
        match handle.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                buffer.extend_from_slice(&buf[..n]);
                if buffer.windows(5).any(|w| w == b"hello") {
                    return; // 见到回声, pass
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

    panic!(
        "read 完毕但未见 'hello' 回声。累计: {:?}",
        String::from_utf8_lossy(&buffer)
    );
}

/// 空 buffer write 应直接返 Ok(0), 不触发 syscall — 防御性, 调用方
/// (`Dispatch<WlKeyboard>` 的 KeyboardAction::Nothing 路径) 不应到此。
#[test]
fn write_empty_returns_zero() {
    // 用 sleep 占住 PTY (其实写空也走不到子进程, 任何 spawn 都行)
    let handle = PtyHandle::spawn_program("sleep", &["10"], 80, 24).expect("spawn sleep");
    let n = handle.write(&[]).expect("write 空 buffer 应 Ok(0)");
    assert_eq!(n, 0);
    drop(handle);
}

/// `pty.write` 单字节 ASCII (例如 wl_keyboard 'a' 按一次的典型 size) 应立即
/// 成功 — 验 INV-009 O_NONBLOCK + 小 size 不触发 WouldBlock 回归。
#[test]
fn write_single_ascii_byte_succeeds() {
    let handle = PtyHandle::spawn_program("cat", &[], 80, 24).expect("spawn cat");
    thread::sleep(Duration::from_millis(100));

    let n = handle.write(b"a").expect("写 'a' 应成功");
    assert_eq!(n, 1, "单字节 ASCII 应一次写入");

    // 不验回声 (write_then_read_loops_back_via_cat_echo 已覆盖); 仅验 write
    // 自身 syscall 不阻塞 / 不 WouldBlock。
    drop(handle);
}

/// `pty.write` 串 (派单允许多字节, 例如 ESC sequence "ESC[3~" Delete 键)
/// 应一次写入 — 4 字节远小于 PTY 内核缓冲 (>= 4096), 不触发部分写入。
#[test]
fn write_multibyte_escape_sequence_succeeds() {
    let handle = PtyHandle::spawn_program("cat", &[], 80, 24).expect("spawn cat");
    thread::sleep(Duration::from_millis(100));

    let payload = b"\x1b[3~"; // ESC [ 3 ~ (Delete key)
    let n = handle.write(payload).expect("写 ESC seq 应成功");
    assert_eq!(n, payload.len(), "4 字节 ESC seq 应一次写入");

    drop(handle);
}
