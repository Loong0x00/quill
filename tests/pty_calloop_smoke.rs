//! T-0202 验收测试:PTY master fd 进 calloop Generic source,数据写进去能被 poll 到。
//!
//! 不依赖真实 Wayland / wgpu。路径:
//! 1. `PtyHandle::spawn_program("echo", ["hello"], 80, 24)` 起一次性子进程,子进程
//!    把 "hello\n"(或带 tty 翻译后的 CRLF)写到 slave stdout。
//! 2. `calloop::EventLoop<bool>` + `Generic::new(master_fd, READ, Level)`,回调把
//!    `*data = true`。
//! 3. `dispatch(Some(500ms), ...)` 阻塞等 fd ready。回调触发后 `*data == true`。
//! 4. drop(handle) 自动关 master → slave EOF + SIGHUP,子进程很快退出;zombie
//!    由 init 在测试进程退出后收养。不显式 wait(集成测试拿不到 `pub(crate)`
//!    helper)。
//!
//! 这条是对 INV-005 "所有 IO fd 必须在同一 calloop::EventLoop" 的局部证实:至少
//! PTY 分支走通了。

use std::os::fd::BorrowedFd;
use std::time::Duration;

use calloop::generic::Generic;
use calloop::{EventLoop, Interest, Mode, PostAction};
use quill::pty::PtyHandle;

#[test]
fn master_fd_becomes_readable_after_child_writes() {
    let handle = PtyHandle::spawn_program("echo", &["hello"], 80, 24).expect("spawn `echo hello`");
    let fd = handle.raw_fd();

    let mut event_loop: EventLoop<'_, bool> = EventLoop::try_new().expect("build EventLoop");
    let loop_handle = event_loop.handle();

    // SAFETY: fd 由 handle.master 拥有,handle 还活着;borrow_raw 的 'static 只是
    // 语法上的擦除,实际生命周期 ≤ handle 的生命周期,本测试全程 handle 在栈上。
    #[allow(unsafe_code)]
    let borrowed: BorrowedFd<'static> = unsafe { BorrowedFd::borrow_raw(fd) };

    loop_handle
        .insert_source(
            Generic::new(borrowed, Interest::READ, Mode::Level),
            |_readiness, _fd, data: &mut bool| -> std::io::Result<PostAction> {
                *data = true;
                // 读干净字节,避免 Level 触发无限轮;测试本身只关心"收到 readable"这一
                // 事件,字节内容由 T-0203 / T-0206 测试断言。
                // SAFETY 提醒:此处只 std::io::read(BorrowedFd) 是不行的(没 Read trait
                // for BorrowedFd);改用 std::fs::File::from_raw_fd 再 forget,或直接
                // rustix::io::read。最简单:用 libc::read loop,但测试不需要。留给
                // dispatch 下一次再触发是 OK 的;Level 模式下同一事件会一直 ready,
                // 但我们退出 loop 马上就 return 不再 dispatch。
                Ok(PostAction::Continue)
            },
        )
        .expect("insert_source(pty master)");

    let mut got_ready = false;
    // 500ms 给子进程充足时间 fork+exec+写字节。本测试内部单次 dispatch 已足够。
    event_loop
        .dispatch(Some(Duration::from_millis(500)), &mut got_ready)
        .expect("dispatch");

    assert!(
        got_ready,
        "500ms 内 echo 子进程应已写 stdout,master fd 应 readable"
    );

    // 不显式 wait:`wait_child_for_test` 是 `pub(crate)`,集成测试拿不到;但 echo
    // 已退出,drop(handle) 后 master 关闭,子进程变 zombie,待本测试进程退出后
    // 被 init 收养回收。cargo test 的 harness 行为里不残留可观察进程。
    drop(handle);
}

/// 对称回归:若 **没人** 写字节,dispatch 在超时窗口内不应误报 readable。
/// 防"callback 在注册瞬间就触发"的实现错。
#[test]
fn master_fd_stays_unreadable_when_nothing_written() {
    // 起 /bin/sleep 0.5,子进程 500ms 内不写 stdout 任何数据。我们用 100ms dispatch
    // 超时,应超时返回 got_ready = false。
    let handle: PtyHandle =
        PtyHandle::spawn_program("sleep", &["0.5"], 80, 24).expect("spawn sleep");
    let fd = handle.raw_fd();

    let mut event_loop: EventLoop<'_, bool> = EventLoop::try_new().expect("build EventLoop");
    let loop_handle = event_loop.handle();

    // SAFETY: 同上一 case。
    #[allow(unsafe_code)]
    let borrowed: BorrowedFd<'static> = unsafe { BorrowedFd::borrow_raw(fd) };

    loop_handle
        .insert_source(
            Generic::new(borrowed, Interest::READ, Mode::Level),
            |_, _, data: &mut bool| -> std::io::Result<PostAction> {
                *data = true;
                Ok(PostAction::Continue)
            },
        )
        .expect("insert_source");

    let mut got_ready = false;
    event_loop
        .dispatch(Some(Duration::from_millis(100)), &mut got_ready)
        .expect("dispatch");

    assert!(
        !got_ready,
        "sleep 期间 master 不应 readable;若触发说明误报 / 注册路径有毛病"
    );

    // sleep 还在跑。drop(handle) 触发 master close → slave EOF + SIGHUP 给 sleep,
    // sleep 几乎立刻退出;zombie 由 init 在本测试进程退出后收养回收。
    drop(handle);
}
