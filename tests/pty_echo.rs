//! T-0206 集成测试:Phase 2 端到端 "PTY 字节通路" 冒烟。
//!
//! 不依赖 Wayland / calloop / wgpu,只验 `PtyHandle`:
//! 1. `captures_echo_hello_output`:spawn `/bin/echo hello`,循环 read 拿 EOF/EIO,
//!    断言聚合 buffer 包含 `b"hello"`(PTY 默认 onlcr 会把 `\n` 转 `\r\n`,本测
//!    只查字面 "hello",回车符不敏感)
//! 2. `drop_cleans_up_long_running_child`:spawn `sleep 600.5`(用非常规 float 秒数
//!    避免和系统里其它 sleep 撞字面),drop handle 后 500ms 内 `pgrep -f` 不再匹配,
//!    证明 `PtyHandle::Drop` 通过 master-close → SIGHUP 把 sleep 干掉了
//!
//! 跑法:`cargo test --test pty_echo` 本地 Arch Linux pass;headless CI 也应该过
//! (依赖 `/bin/echo`、`/bin/sleep`、`/usr/bin/pgrep` 三个 coreutils + procps 标配)。

use std::io;
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant};

use quill::pty::PtyHandle;

/// 用于 drop-cleanup 测试的 sleep 秒数。非常规小数 `600.5` 避免和系统里
/// 其它正在跑的 sleep(典型整数秒)撞字面 `pgrep` 匹配。
const LONG_SLEEP_ARG: &str = "600.5";

#[test]
fn captures_echo_hello_output() {
    let mut handle =
        PtyHandle::spawn_program("echo", &["hello"], 80, 24).expect("spawn echo hello");

    // 累加 read。超时 2 秒(echo 正常 <10ms 退出 + fork/exec 延迟);超时视作失败。
    let mut buffer = Vec::<u8>::with_capacity(64);
    let mut buf = [0u8; 256];
    let deadline = Instant::now() + Duration::from_secs(2);

    loop {
        if Instant::now() > deadline {
            panic!(
                "2 秒内未读到 echo 输出 EOF。累计 buffer = {:?}",
                String::from_utf8_lossy(&buffer)
            );
        }
        match handle.read(&mut buf) {
            // EOF:slave 关(少见,Linux 通常给 EIO)
            Ok(0) => break,
            Ok(n) => {
                buffer.extend_from_slice(&buf[..n]);
                // 看到 "hello" 就够了,不一定要等到 EOF/EIO
                if buffer.windows(5).any(|w| w == b"hello") {
                    break;
                }
            }
            Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                // 子进程还没写完 stdout。10ms 是 ticket Implementation notes 给的
                // "内核调度问题" 兜底间隔,对 cargo test 数量级影响可忽略。
                thread::sleep(Duration::from_millis(10));
                continue;
            }
            // Linux 典型:slave 在 echo 退出后关闭 → master 读回 EIO,等价 EOF。
            Err(ref e) if e.raw_os_error() == Some(libc::EIO) => break,
            Err(e) => panic!("read 非预期错误: {e} (kind={:?})", e.kind()),
        }
    }

    assert!(
        buffer.windows(5).any(|w| w == b"hello"),
        "echo 输出应包含 'hello', 实际: {:?}",
        String::from_utf8_lossy(&buffer)
    );
}

#[test]
fn drop_cleans_up_long_running_child() {
    // spawn sleep,让它真的跑起来。不在 drop 之前 wait —— 我们要验证 drop 自己
    // 能让 sleep 死。
    let handle = PtyHandle::spawn_program("sleep", &[LONG_SLEEP_ARG], 80, 24).expect("spawn sleep");

    // 给 kernel 100ms 完成 fork/exec。否则 pgrep 可能在 sleep 还没来得及跑起来之前
    // 就查了,先手一次伪 positive(读到空)但不具备 "sleep 活着" 的前置事实。
    thread::sleep(Duration::from_millis(100));

    // 前置:sleep 应已跑起来(pgrep 命中非空)。如果 pre 查不到,说明 spawn 语义
    // 有问题,早报错比 drop 后假 pass 强。
    let pre = Command::new("pgrep")
        .args(["-f", &format!("sleep {LONG_SLEEP_ARG}")])
        .output()
        .expect("pgrep 命令应可用");
    assert!(
        !pre.stdout.is_empty(),
        "pre-drop: sleep {LONG_SLEEP_ARG} 应已启动 (pgrep 输出空表示 spawn 失败)"
    );

    // drop: PtyHandle Drop 按 INV-008 序 reader → master → child。master 关 →
    // slave 端 SIGHUP → sleep 默认退出(SIGHUP's default = Term)。child 仅 drop
    // std::process::Child 句柄(不 wait),zombie 由 init 收养 —— 不影响 pgrep
    // 命中字符串 cmdline,zombie 的 cmdline 在 /proc 里是空的。
    drop(handle);

    // 500ms 窗口循环 pgrep。大多数情况 SIGHUP 跑完 + kernel cleanup 在 <100ms,
    // 500ms 给 CI 慢机留裕量。ticket 说 "100ms 内 pgrep 不再命中",我放宽到 500ms
    // 加反复 poll,更 robust。
    let deadline = Instant::now() + Duration::from_millis(500);
    while Instant::now() < deadline {
        let post = Command::new("pgrep")
            .args(["-f", &format!("sleep {LONG_SLEEP_ARG}")])
            .output()
            .expect("pgrep");
        if post.stdout.is_empty() {
            return; // 干净了,pass
        }
        thread::sleep(Duration::from_millis(20));
    }

    // 500ms 后仍在:失败。把 pgrep 原文抛出去帮排障。
    let post = Command::new("pgrep")
        .args(["-f", &format!("sleep {LONG_SLEEP_ARG}")])
        .output()
        .expect("pgrep");
    panic!(
        "drop 后 500ms 内 sleep {LONG_SLEEP_ARG} 未被回收。pgrep stdout:\n{}",
        String::from_utf8_lossy(&post.stdout)
    );
}
