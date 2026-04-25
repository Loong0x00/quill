//! T-0603 端到端: KeyboardState repeat 状态 + calloop Timer 的真 reschedule
//! 行为, 不真起 wayland / 不 spawn pty.
//!
//! **why 不接真 pty**: PTY read loop / signal handler 复杂耦合, 集成测试若起
//! 完整 `wl::run_window` 需起真 wayland compositor — 本工程历来仅手测 (CLAUDE.md
//! "wayland / wgpu 不写自动化测试"). 这里**仅验** `KeyboardState` + calloop
//! `Timer` 的最小子系统: schedule → tick fire 累积字节 → cancel 终止累积.
//! 真 PTY 写入路径与 Dispatch<WlKeyboard> 路径由 `tests/keyboard_event_to_pty.rs`
//! (T-0501) + 手测 deliverable 覆盖.
//!
//! 派单 In #D 第 3 条: "mock timerfd, fire 模拟, 验 bytes 累积". 我们用
//! calloop Timer (派单允许 — calloop 内部高精度 timerfd, 测试以 wall-clock
//! sleep 触发 fire, 不 mock timerfd 系统调用本身).

use std::time::{Duration, Instant};

use calloop::timer::{TimeoutAction, Timer};
use calloop::EventLoop;
use quill::wl::{handle_key_event, KeyboardAction, KeyboardState};
use wayland_client::protocol::wl_keyboard::{self, KeyState};
use wayland_client::WEnum;

const KEY_A: u32 = 30;
const KEY_BACKSPACE: u32 = 14;

/// 集成测试 LoopData: 累积 timer fire 时写出的字节.
struct TestData {
    keyboard_state: KeyboardState,
    accumulated: Vec<u8>,
    /// 测试用 rate (keys/sec). 真 wl_keyboard.repeat_info 给的值, 我们模拟
    /// 25 keys/sec → interval 40ms (足够快让 200ms 内有多次 fire).
    rate_per_sec: u32,
}

fn pressed(key: u32) -> wl_keyboard::Event {
    wl_keyboard::Event::Key {
        serial: 0,
        time: 0,
        key,
        state: WEnum::Value(KeyState::Pressed),
    }
}

fn released(key: u32) -> wl_keyboard::Event {
    wl_keyboard::Event::Key {
        serial: 0,
        time: 0,
        key,
        state: WEnum::Value(KeyState::Released),
    }
}

fn modifiers(mask: u32) -> wl_keyboard::Event {
    wl_keyboard::Event::Modifiers {
        serial: 0,
        mods_depressed: mask,
        mods_latched: 0,
        mods_locked: 0,
        group: 0,
    }
}

/// 构造测试用 KeyboardState (载 us layout + 设 RepeatInfo).
fn make_state(rate: i32, delay: i32) -> KeyboardState {
    let mut state = KeyboardState::new().expect("KeyboardState::new");
    state
        .load_default_us_keymap()
        .expect("us keymap (装 xkeyboard-config 包?)");
    let info = wl_keyboard::Event::RepeatInfo { rate, delay };
    let _ = handle_key_event(info, &mut state, 24);
    state
}

/// 测试辅助: 跑 event_loop 直到 wall-clock 到 (calloop dispatch 单 cycle 即返,
/// 我们需要持续调度 timer fire — 循环 dispatch with 短 timeout).
fn run_for(event_loop: &mut EventLoop<'_, TestData>, data: &mut TestData, duration: Duration) {
    let deadline = Instant::now() + duration;
    while Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let slice = remaining.min(Duration::from_millis(20));
        event_loop.dispatch(Some(slice), data).expect("dispatch");
    }
}

/// 派单 In #D 第 1 条: 走 KeyboardAction → 走 calloop Timer → tick fire 累积
/// 字节 → 多次 reschedule. 用短 delay (10ms) + rate=50 (interval 20ms) 让
/// 100ms 测试窗口能见 4-5 次 fire.
#[test]
fn calloop_timer_repeat_accumulates_bytes() {
    let mut event_loop: EventLoop<'_, TestData> = EventLoop::try_new().expect("EventLoop");
    let loop_handle = event_loop.handle();

    let mut data = TestData {
        keyboard_state: make_state(50, 10), // rate=50/sec → 20ms interval, delay=10ms
        accumulated: Vec::new(),
        rate_per_sec: 50,
    };

    // press 'a' → 拿 StartRepeat 字节 (跟 window.rs 真路径一样 push 到累积器
    // 模拟"立即写一次"). 注: 真路径中 Dispatch<WlKeyboard> 调
    // `write_keyboard_bytes` 写 PTY, 这里直接 append 到 accumulated.
    let action = handle_key_event(pressed(KEY_A), &mut data.keyboard_state, 24);
    match action {
        KeyboardAction::StartRepeat { bytes } => {
            data.accumulated.extend_from_slice(&bytes);
        }
        other => panic!("期望 StartRepeat, 得 {other:?}"),
    }

    // schedule 一个真 calloop Timer (跟 apply_repeat_request 路径一样).
    let timer = Timer::from_duration(Duration::from_millis(10));
    let token = loop_handle
        .insert_source(timer, |_deadline, _meta, data: &mut TestData| {
            // tick: 拿当前 repeat 字节累积 + reschedule.
            match data.keyboard_state.tick_repeat() {
                Some(bytes) => {
                    data.accumulated.extend_from_slice(&bytes);
                    let interval_ms = (1000 / data.rate_per_sec.max(1)) as u64;
                    TimeoutAction::ToDuration(Duration::from_millis(interval_ms))
                }
                None => TimeoutAction::Drop,
            }
        })
        .expect("insert_source(Timer)");

    // 跑 event_loop 100ms, 让 timer fire 多次.
    run_for(&mut event_loop, &mut data, Duration::from_millis(150));

    // 100ms 内: 立即写 1 + delay=10ms 后首次 fire (1) + 之后 20ms 间隔
    // (~4 次 fire) ≈ 6 次. 实际由 OS scheduler 决定下界, 至少 3 次确认
    // reschedule 工作.
    assert!(
        data.accumulated.len() >= 3,
        "期望 ≥3 字节累积 (immediate + ≥2 fire), 实得 {}",
        data.accumulated.len()
    );
    assert!(
        data.accumulated.iter().all(|&b| b == b'a'),
        "全部累积字节应为 'a', 实得 {:?}",
        data.accumulated
    );

    // cleanup
    loop_handle.remove(token);
}

/// 派单 In #D 第 1 条 (接续): release 后 tick_repeat 返 None → timer Drop
/// 终止 → 后续 wall-clock 不再累积.
#[test]
fn release_stops_accumulation() {
    let mut event_loop: EventLoop<'_, TestData> = EventLoop::try_new().expect("EventLoop");
    let loop_handle = event_loop.handle();

    let mut data = TestData {
        keyboard_state: make_state(100, 5), // 极快 rate=100, delay=5ms
        accumulated: Vec::new(),
        rate_per_sec: 100,
    };

    let _ = handle_key_event(pressed(KEY_BACKSPACE), &mut data.keyboard_state, 24);
    let timer = Timer::from_duration(Duration::from_millis(5));
    let _token = loop_handle
        .insert_source(timer, |_d, _m, data: &mut TestData| {
            match data.keyboard_state.tick_repeat() {
                Some(bytes) => {
                    data.accumulated.extend_from_slice(&bytes);
                    TimeoutAction::ToDuration(Duration::from_millis(10))
                }
                None => TimeoutAction::Drop,
            }
        })
        .expect("insert_source");

    run_for(&mut event_loop, &mut data, Duration::from_millis(80));

    let count_before_release = data.accumulated.len();
    assert!(
        count_before_release >= 2,
        "release 前应累积 ≥2 字节, 实得 {}",
        count_before_release
    );

    // release: tick_repeat 此后返 None
    let action = handle_key_event(released(KEY_BACKSPACE), &mut data.keyboard_state, 24);
    assert_eq!(action, KeyboardAction::StopRepeat);

    // 再跑 80ms — timer 下次 fire 看 None 走 Drop, 之后无新累积.
    run_for(&mut event_loop, &mut data, Duration::from_millis(80));

    // release 后 timer 至多再 fire 1-2 次然后 Drop, 累积量应远小于"持续累积"
    // 假设 (release 后 50ms 若仍 active rate=100 会再 +5).
    let count_after_release = data.accumulated.len();
    assert!(
        count_after_release - count_before_release <= 2,
        "release 后累积应停止 (差值 ≤2), 实得 before={count_before_release} after={count_after_release}"
    );
    assert!(
        data.accumulated.iter().all(|&b| b == 0x7f),
        "全为 BackSpace=DEL 0x7f, 实得 {:?}",
        data.accumulated
    );
}

/// 派单 In #D 第 2 条: modifier 变化 cancel repeat — 业务侧 (Dispatch
/// 路径) 收到 StopRepeat 后 remove timer; 同时 KeyboardState 内部
/// current_repeat 也清, 即使 timer 仍 fire 1 次也走 Drop.
#[test]
fn modifier_change_stops_accumulation_via_state() {
    let mut event_loop: EventLoop<'_, TestData> = EventLoop::try_new().expect("EventLoop");
    let loop_handle = event_loop.handle();

    let mut data = TestData {
        keyboard_state: make_state(50, 10),
        accumulated: Vec::new(),
        rate_per_sec: 50,
    };

    // 起步 modifier=0
    let _ = handle_key_event(modifiers(0), &mut data.keyboard_state, 24);
    let _ = handle_key_event(pressed(KEY_A), &mut data.keyboard_state, 24);
    let timer = Timer::from_duration(Duration::from_millis(10));
    let _token = loop_handle
        .insert_source(timer, |_d, _m, data: &mut TestData| {
            match data.keyboard_state.tick_repeat() {
                Some(bytes) => {
                    data.accumulated.extend_from_slice(&bytes);
                    TimeoutAction::ToDuration(Duration::from_millis(20))
                }
                None => TimeoutAction::Drop,
            }
        })
        .expect("insert_source");

    run_for(&mut event_loop, &mut data, Duration::from_millis(100));

    let before = data.accumulated.len();
    assert!(before >= 2, "modifier 变化前应累积 ≥2 字节, 实得 {before}");

    // 按 Shift (mask 变化) → StopRepeat
    let action = handle_key_event(modifiers(1 << 0), &mut data.keyboard_state, 24);
    assert_eq!(action, KeyboardAction::StopRepeat);

    run_for(&mut event_loop, &mut data, Duration::from_millis(100));

    let after = data.accumulated.len();
    assert!(
        after - before <= 2,
        "modifier 变化后累积应停, before={before} after={after}"
    );
}

/// 派单 In #D 第 3 条: tick_repeat 返 None 时 callback 应返 Drop, calloop
/// 自然清 source. 用空 KeyboardState (无 press) 起步, timer fire 一次后即
/// Drop, 不再 reschedule.
#[test]
fn tick_repeat_none_drops_timer() {
    let mut event_loop: EventLoop<'_, TestData> = EventLoop::try_new().expect("EventLoop");
    let loop_handle = event_loop.handle();

    let mut data = TestData {
        keyboard_state: make_state(50, 5),
        accumulated: Vec::new(),
        rate_per_sec: 50,
    };

    // 不调 press → tick_repeat 一直返 None
    let timer = Timer::from_duration(Duration::from_millis(5));
    let _token = loop_handle
        .insert_source(timer, |_d, _m, data: &mut TestData| {
            match data.keyboard_state.tick_repeat() {
                Some(bytes) => {
                    data.accumulated.extend_from_slice(&bytes);
                    TimeoutAction::ToDuration(Duration::from_millis(10))
                }
                None => TimeoutAction::Drop,
            }
        })
        .expect("insert_source");

    run_for(&mut event_loop, &mut data, Duration::from_millis(60));

    assert!(
        data.accumulated.is_empty(),
        "无 press 时 timer 不应累积任何字节, 实得 {:?}",
        data.accumulated
    );
}
