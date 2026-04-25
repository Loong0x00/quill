//! T-0505 集成测试: TIv3 mock event sequence → [`ImeAction`] →
//! [`PtyHandle::write`] 端到端验 commit bytes 真到 PTY。
//!
//! **why 集成测试 (而非 lib unit)**: lib unit (src/ime/mod.rs::tests) 已覆盖
//! event → ImeAction 纯映射; 这里覆盖 "ImeAction::Commit(bytes) →
//! pty.write(&bytes) → cat echo back" 真 PTY 路径, 验 IME 字节真到 shell stdin。
//!
//! **不真起 fcitx5** (CI 不可控, 实测环境差异大): 用 mock event 序列模拟
//! fcitx5 推过来的 PreeditString + CommitString + Done 三件套, ImeState 状态
//! 机内部应正确算出 ImeAction::Commit, 然后调用方写 PTY。
//!
//! **运行**: `cargo test --test ime_e2e`

use quill::ime::{handle_text_input_event, ImeAction, ImeState};
use quill::pty::PtyHandle;
use wayland_protocols::wp::text_input::zv3::client::zwp_text_input_v3;

/// 构造 PreeditString event helper.
fn preedit_event(
    text: Option<&str>,
    cursor_begin: i32,
    cursor_end: i32,
) -> zwp_text_input_v3::Event {
    zwp_text_input_v3::Event::PreeditString {
        text: text.map(String::from),
        cursor_begin,
        cursor_end,
    }
}

fn commit_event(text: Option<&str>) -> zwp_text_input_v3::Event {
    zwp_text_input_v3::Event::CommitString {
        text: text.map(String::from),
    }
}

fn done_event(serial: u32) -> zwp_text_input_v3::Event {
    zwp_text_input_v3::Event::Done { serial }
}

/// 把 [`ImeAction`] 翻译成 PTY 副作用 (与 `src/wl/window.rs::apply_ime_action`
/// 同源, 但 inline 让测试不依赖 wayland 协议对象 (text_input.enable() 等))。
/// 仅 commit / composite 路径写 PTY, 其他 trace。
fn apply_ime_action_to_pty(action: ImeAction, pty: &PtyHandle) {
    match action {
        ImeAction::Commit(bytes) => {
            // INV-009 master fd O_NONBLOCK + INV-005 不重试 WouldBlock; 测试
            // 期 PTY buffer 必有空间 (kernel 默认 4 KiB, 单帧 < 12 字节中文).
            let _ = pty.write(&bytes);
        }
        ImeAction::Composite(actions) => {
            for a in actions {
                apply_ime_action_to_pty(a, pty);
            }
        }
        _ => {}
    }
}

/// **核心 e2e**: 模拟 fcitx5-rime 输 "你" 一字: PreeditString "ni" → Done →
/// CommitString "你" + 清 preedit → Done → bytes "你" 经 PTY 写到 cat echo
/// 子进程 → 读回 stdout 验字节正确。
#[test]
fn fcitx5_commit_bytes_reach_pty_via_cat_echo() {
    // 起 cat 子进程: master 写 → slave 收 → cat 原样吐回 master 读
    let mut pty =
        PtyHandle::spawn_program("cat", &[], 80, 24).expect("PtyHandle::spawn_program(cat) failed");

    // 给 cat 启动留时间 (与 tests/keyboard_event_to_pty.rs 同套路)
    std::thread::sleep(std::time::Duration::from_millis(300));

    let mut state = ImeState::new();

    // 帧 1: 仅显示 preedit "ni" (用户敲拼音, 还没选词)
    let _ = handle_text_input_event(preedit_event(Some("ni"), 2, 2), &mut state);
    let action = handle_text_input_event(done_event(1), &mut state);
    // preedit-only 帧不应写 PTY
    apply_ime_action_to_pty(action, &pty);

    // 帧 2: 用户选 "你": fcitx5 推 commit + 清 preedit + done
    let _ = handle_text_input_event(commit_event(Some("你")), &mut state);
    let _ = handle_text_input_event(preedit_event(Some(""), 0, 0), &mut state);
    let action = handle_text_input_event(done_event(2), &mut state);
    apply_ime_action_to_pty(action, &pty);

    // 加 \n 让 cat 立即吐 (line buffer)
    let _ = pty.write(b"\n");

    // 等 cat echo (master fd O_NONBLOCK, 用 sleep + read loop, 与
    // keyboard_event_to_pty.rs 同套路)
    std::thread::sleep(std::time::Duration::from_millis(200));

    let mut received: Vec<u8> = Vec::new();
    let mut buf = [0u8; 4096];
    for _ in 0..10 {
        match pty.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                received.extend_from_slice(&buf[..n]);
                if received.windows(3).any(|w| w == "你".as_bytes()) {
                    break;
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            Err(_) => break,
        }
    }

    // cat 把 "你\n" 输入完整回显 — received 应含 "你" 三字节 UTF-8
    assert!(
        received.windows(3).any(|w| w == "你".as_bytes()),
        "cat echo 应含 '你' UTF-8 三字节; received: {received:?}"
    );
}

/// 多次 commit 序列 "你好": 验 IME state 机不漏字。
#[test]
fn multi_commit_sequence_ni_hao() {
    let mut pty = PtyHandle::spawn_program("cat", &[], 80, 24).expect("spawn cat failed");
    std::thread::sleep(std::time::Duration::from_millis(300));

    let mut state = ImeState::new();

    // commit "你"
    handle_text_input_event(commit_event(Some("你")), &mut state);
    let action = handle_text_input_event(done_event(1), &mut state);
    apply_ime_action_to_pty(action, &pty);

    // commit "好"
    handle_text_input_event(commit_event(Some("好")), &mut state);
    let action = handle_text_input_event(done_event(2), &mut state);
    apply_ime_action_to_pty(action, &pty);

    let _ = pty.write(b"\n");
    std::thread::sleep(std::time::Duration::from_millis(200));

    let mut received: Vec<u8> = Vec::new();
    let mut buf = [0u8; 4096];
    for _ in 0..10 {
        match pty.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                received.extend_from_slice(&buf[..n]);
                if received.windows(6).any(|w| w == "你好".as_bytes()) {
                    break;
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            Err(_) => break,
        }
    }

    assert!(
        received.windows(6).any(|w| w == "你好".as_bytes()),
        "cat echo 应含 '你好' UTF-8 六字节; received: {received:?}"
    );
}

/// preedit-only 帧 (无 commit) 不应写任何字节给 PTY。
#[test]
fn preedit_only_frame_writes_no_bytes() {
    let mut pty = PtyHandle::spawn_program("cat", &[], 80, 24).expect("spawn cat failed");
    std::thread::sleep(std::time::Duration::from_millis(300));

    let mut state = ImeState::new();
    handle_text_input_event(preedit_event(Some("ni"), 2, 2), &mut state);
    let action = handle_text_input_event(done_event(1), &mut state);
    apply_ime_action_to_pty(action, &pty);

    // cat 不应吐任何字节 (我们没发任何 commit)
    std::thread::sleep(std::time::Duration::from_millis(150));
    let mut buf = [0u8; 4096];
    match pty.read(&mut buf) {
        Ok(n) => assert_eq!(
            n, 0,
            "preedit-only 帧应不写 PTY, cat 应无 echo, got {n} bytes"
        ),
        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
            // 也是合法的"无字节"信号
        }
        Err(e) => panic!("unexpected pty.read error: {e}"),
    }
}
