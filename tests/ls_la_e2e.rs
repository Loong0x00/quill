//! T-0307 Phase 3 收尾: 端到端 `ls -la /` → PTY → TermState.advance →
//! grid (含 scrollback) 内容真有 ls 特征字符 (`total`, `drwx`)。
//!
//! 链路:
//! ```text
//! PtyHandle::spawn_program("env", ["LANG=C", "LC_ALL=C", "ls", "-la", "/"])
//!   → loop read (O_NONBLOCK + WouldBlock + try_wait)
//!   → TermState::advance(bytes)
//!   → all_grid_lines(viewport ++ scrollback) 含 'total' / 'drwx'
//! ```
//!
//! 与 `tests/pty_to_term.rs` (T-0301 `echo hello`) 一脉相承, 但喂的是真 GNU
//! coreutils 程序的 ANSI 输出 (含 `--color=auto` 在 PTY 上默认开启的 SGR 序列),
//! 验证 alacritty VT 解析在真实 terminal 程序输出下不出错且字符正确进 grid。
//!
//! Phase 3 收尾产出: 证明 PTY → Term 整链路可以承载真 shell 输出, 不只是
//! 一行 echo (T-0301 测试覆盖 5 字节; 本测试覆盖几 KB ANSI 流 + 多行
//! scrollback)。
//!
//! ## viewport vs scrollback (派单 cols=80 rows=24 与现实 ls 行数的 trade-off)
//!
//! 派单写 `TermState::new(80, 24)`。但 `ls -la /` 在典型 Linux 根目录输出
//! ~25 行 (total + . + .. + ~22-25 顶层条目), 24 行 viewport 一定不够装,
//! 多余行被 alacritty 滚入 **scrollback** (T-0304 已暴露 `scrollback_size`
//! + `scrollback_line_text`)。
//!
//! ticket Goal 原话: "grid 内容里能找到 ls 特征字符" — quill 的 "grid" 在
//! T-0304 已分两部分: viewport (`line_text`) + scrollback (`scrollback_line_text`),
//! 任一处找到即满足 "整链路真把字节解析进 grid"。本测试断言遍历 viewport ++
//! scrollback, 不强求 `total` 必在 viewport (实际位置取决于具体 ls 行数, 与
//! 平台 / 根目录条目数有关, 非测试核心)。
//!
//! ## locale 兜底 (env LANG=C LC_ALL=C)
//!
//! GNU coreutils `ls` 第一行 `total N` 在中文 locale 下被翻译为 `总计 N`。
//! 测试断 `'total'` 字面, 必须强制英文 locale。直接 `std::env::set_var("LANG", "C")`
//! 会与 cargo test 默认并行执行的其它测试互相干扰 (POSIX `setenv` 非线程安全;
//! Rust 1.79+ 在 edition 2024 把 `set_var` 标 hard `unsafe`, edition 2021 现仍
//! 是 warn 但底层 race 同样存在), 改用 `/usr/bin/env LANG=C LC_ALL=C`
//! 包装只影响当前子进程 env, 无全局副作用 — 这是配套 `tests/pty_echo.rs`
//! (echo 输出与 locale 无关) 不需要 locale 兜底的延伸做法。
//!
//! ## 派单已知陷阱对照 (来自 tasks/T-0307-ls-la-e2e.md "已知陷阱")
//!
//! - O_NONBLOCK read loop 处理 WouldBlock + sleep ~10ms ✅
//! - try_wait 检查子进程退出 (Ok(Some(_)) → drain residual + 停止 read) ✅
//! - PtyHandle drop 自动 SIGHUP + waitpid (我们不手动 reaping) ✅
//! - timeout 5 秒 (CI 慢机器留余量, ls 正常 <100ms) ✅
//! - GNU coreutils 格式 (派单允许只覆盖 Linux) ✅

use std::io;
use std::thread;
use std::time::{Duration, Instant};

use quill::pty::PtyHandle;
use quill::term::{CellPos, ScrollbackPos, TermState};

/// 5 秒兜底。`ls -la /` 在 Linux 上通常 <100ms, 5 秒给 CI 慢机器 / fork+exec
/// 调度延迟留余量。配套 `tests/pty_echo.rs` 的 2 秒 / `tests/pty_to_term.rs`
/// 的 2 秒, 这里更宽是因为 ls 输出量大几十倍 + 子进程退出后还要 drain 残余。
const READ_TIMEOUT: Duration = Duration::from_secs(5);

#[test]
fn ls_la_root_grid_contains_total_and_drwx() {
    let mut pty =
        PtyHandle::spawn_program("env", &["LANG=C", "LC_ALL=C", "ls", "-la", "/"], 80, 24)
            .expect("spawn env LANG=C LC_ALL=C ls -la /");
    let mut term = TermState::new(80, 24);

    feed_until_child_exits(&mut pty, &mut term);

    let all_lines = collect_all_grid_lines(&term);

    assert!(
        all_lines.iter().any(|l| l.contains("total")),
        "ls -la 第一行 'total N' 应进入 grid (viewport 或 scrollback, LANG=C 强制英文); \
         共 {} 行 (scrollback {} + viewport 24):\n{}",
        all_lines.len(),
        term.scrollback_size(),
        all_lines.join("\n"),
    );
    assert!(
        all_lines.iter().any(|l| l.contains("drwx")),
        "根目录至少一项目录 mode 位 'drwx' 应进入 grid; 共 {} 行:\n{}",
        all_lines.len(),
        all_lines.join("\n"),
    );
    assert!(
        term.is_dirty(),
        "advance 至少调过一次, dirty 应置位 (T-0301 advance 模式)",
    );
    assert_ne!(
        term.cursor_pos(),
        CellPos { col: 0, line: 0 },
        "ls 输出多行 + 末尾换行, 光标不应仍停在 (0,0)",
    );
}

/// 缩 grid 到 40 cols × 24 rows, 验证:
/// 1. ls 输出文件名行 (典型 50+ 字符) 在 alacritty 内部按 40 cols 自动 wrap,
///    所有 viewport 行字符数恰好 40 (line_text 末尾 pad 空格)
/// 2. 内容不丢: `total` / `drwx` 仍可在 grid (viewport ++ scrollback) 找到
///
/// 配套 T-0306 `term.resize` 在 `tests/resize_chain.rs` 验过 lockstep, 本测试
/// 进一步验证 "小 cols 上跑真程序" 这一组合下 grid 内容仍正确。
#[test]
fn ls_la_smaller_grid_truncates_lines() {
    let mut pty =
        PtyHandle::spawn_program("env", &["LANG=C", "LC_ALL=C", "ls", "-la", "/"], 40, 24)
            .expect("spawn env LANG=C LC_ALL=C ls -la /");
    let mut term = TermState::new(40, 24);

    feed_until_child_exits(&mut pty, &mut term);

    let viewport: Vec<String> = (0..24).map(|n| term.line_text(n)).collect();
    let all_lines = collect_all_grid_lines(&term);

    assert!(
        viewport.iter().all(|l| l.chars().count() == 40),
        "viewport 每行字符数应严格 == 40 cols, 实际:\n{}",
        viewport
            .iter()
            .enumerate()
            .map(|(i, l)| format!("  line {i}: len={} {l:?}", l.chars().count()))
            .collect::<Vec<_>>()
            .join("\n"),
    );

    assert!(
        all_lines.iter().any(|l| l.contains("total")),
        "40 cols grid 内仍应有 'total' (alacritty wrap 不丢字符); 共 {} 行:\n{}",
        all_lines.len(),
        all_lines.join("\n"),
    );
    assert!(
        all_lines.iter().any(|l| l.contains("drwx")),
        "40 cols grid 内仍应有 'drwx'; 共 {} 行:\n{}",
        all_lines.len(),
        all_lines.join("\n"),
    );
}

/// 收集当前 term 的 **全部** grid 内容: scrollback (oldest → newest) ++ viewport
/// (line 0 → line rows-1)。返回 `Vec<String>` 便于 iter().any() 字面查找。
///
/// scrollback 用 T-0304 暴露的 `scrollback_size()` + `scrollback_line_text(ScrollbackPos)`
/// API; viewport 用 `line_text(n)`。两段不重叠 (T-0304 ScrollbackPos doc 明示
/// row=size-1 = "贴着 viewport 顶")。
fn collect_all_grid_lines(term: &TermState) -> Vec<String> {
    let scrollback_n = term.scrollback_size();
    let (_, rows) = term.dimensions();
    let mut lines = Vec::with_capacity(scrollback_n + rows);
    for row in 0..scrollback_n {
        lines.push(term.scrollback_line_text(ScrollbackPos { row }));
    }
    for n in 0..rows {
        lines.push(term.line_text(n));
    }
    lines
}

/// 共享 read 循环: 从 PTY 非阻塞读, 把字节喂给 term。直到下列任一退出条件:
/// - `read` 返 `Ok(0)` (EOF, Linux PTY 上少见但合规)
/// - `read` 返 `Err(EIO)` (Linux 上 slave 关闭后 master 的典型信号, 等价 EOF)
/// - `try_wait` 返 `Ok(Some(_))` (子进程已退出, drain 残余字节后退出)
/// - 5 秒超时 (panic, 排障留 grid 内容现场)
///
/// 与 `tests/pty_echo.rs::captures_echo_hello_output` / `tests/pty_to_term.rs::echo_hello_reaches_term_grid_first_line`
/// 风格一致 (WouldBlock + 10ms sleep + EIO 视作 EOF)。新增 `try_wait` 兜底,
/// 防 ls 输出量大 + 子进程退出后 master 缓冲未 drain 完, 派单 "已知陷阱" 明示。
fn feed_until_child_exits(pty: &mut PtyHandle, term: &mut TermState) {
    let mut buf = [0u8; 4096];
    let deadline = Instant::now() + READ_TIMEOUT;

    loop {
        if Instant::now() > deadline {
            panic!(
                "{}s 内未见 PTY EOF / EIO / try_wait Some, ls 阻塞或 grid 未达终态",
                READ_TIMEOUT.as_secs(),
            );
        }
        match pty.read(&mut buf) {
            Ok(0) => break, // EOF (Linux PTY 少见; 绝大多数走下面 EIO 分支)
            Ok(n) => term.advance(&buf[..n]),
            Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                // WouldBlock = 暂无数据。先看子进程是否已退出: 退出 → drain
                // 残余 (kernel master 缓冲可能还有未读字节, ls 输出在子进程
                // 退出前几乎全发出, 但 read 与 wait 之间可能存在毫秒级窗口),
                // 然后退出。未退出 → sleep 10ms 等下次。
                if matches!(pty.try_wait(), Ok(Some(_))) {
                    thread::sleep(Duration::from_millis(20));
                    drain_residual(pty, term, &mut buf);
                    break;
                }
                thread::sleep(Duration::from_millis(10));
            }
            Err(ref e) if e.raw_os_error() == Some(libc::EIO) => break,
            Err(e) => panic!("read 非预期错误: {e} (kind={:?})", e.kind()),
        }
    }
}

/// 子进程退出后, 把 master 端 kernel 缓冲里残留字节读完。
/// 任何 WouldBlock / EIO / Ok(0) 都视作 "已读完", 直接返回。
fn drain_residual(pty: &mut PtyHandle, term: &mut TermState, buf: &mut [u8]) {
    loop {
        match pty.read(buf) {
            Ok(0) => return,
            Ok(n) => term.advance(&buf[..n]),
            Err(_) => return,
        }
    }
}
