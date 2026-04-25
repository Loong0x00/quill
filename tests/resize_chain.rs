//! T-0306: term + pty resize 走同一对 (cols, rows), 模拟 Wayland configure
//! 触发的同步链路。
//!
//! 完整 chain (renderer.resize + 算 cols from surface px) 由
//! `crate::wl::window::propagate_resize_if_dirty` 完成 —— 那条路径需要真
//! wayland Connection / wgpu Surface, 没法在 headless 里跑。**派单允许"退而
//! 求其次:直接调 chain fn 验证调用顺序 + 副作用"**。本测试只验 term + pty
//! 两端的 lockstep:
//!
//! 1. 一对 (new_cols, new_rows) 同时推给 term.resize + pty.resize
//! 2. term.dimensions() 真反映新尺寸
//! 3. pty.resize() 不报 ioctl 错 (Ok 即 TIOCSWINSZ syscall 成功;kernel
//!    把 winsize 落到 fd 是 portable-pty 的责任, 已被 tests/pty_resize.rs
//!    端到端验过 cols=120 lines=40 真传到 bash)
//!
//! 配套 tests/pty_resize.rs (T-0204 端到端 PTY winsize) +
//! src/term/mod.rs::tests::resize_* (T-0306 4 单测覆盖 term 行为) +
//! src/wl/window.rs::tests::cells_from_surface_px_* (T-0306 4 单测覆盖换算
//! 纯逻辑)。本文件验"两端真同步在一对 cols/rows"这一条契约。

use quill::pty::PtyHandle;
use quill::term::TermState;

#[test]
fn term_and_pty_resize_in_lockstep() {
    let mut term = TermState::new(80, 24);
    let pty = PtyHandle::spawn_program("true", &[], 80, 24).expect("spawn `true`");

    let new_cols: usize = 100;
    let new_rows: usize = 30;

    // 模拟 propagate_resize_if_dirty 的同步链 (term + pty 部分):
    term.resize(new_cols, new_rows);
    pty.resize(new_cols as u16, new_rows as u16)
        .expect("PTY resize ioctl 应成功");

    // 双重锁:term 真 resize + dirty 置位
    assert_eq!(
        term.dimensions(),
        (new_cols, new_rows),
        "term.dimensions() 应反映 lockstep resize"
    );
    assert!(
        term.is_dirty(),
        "term.resize 应置 dirty (下游 idle callback 触发重画)"
    );

    drop(pty);
}

/// 反向验:resize 到更小尺寸也走同一对 (cols, rows), term 应跟随 + pty
/// ioctl 不报错。补充覆盖"窗口缩小"路径 —— shell 收到 SIGWINCH 缩列重排。
#[test]
fn term_and_pty_resize_to_smaller_in_lockstep() {
    let mut term = TermState::new(80, 24);
    let pty = PtyHandle::spawn_program("true", &[], 80, 24).expect("spawn");

    let cols: usize = 40;
    let rows: usize = 12;

    term.resize(cols, rows);
    pty.resize(cols as u16, rows as u16).expect("resize");

    assert_eq!(term.dimensions(), (cols, rows));

    drop(pty);
}
