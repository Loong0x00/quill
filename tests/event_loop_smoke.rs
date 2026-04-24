//! 事件循环骨架烟雾测试。
//!
//! 证明 `Core` 能同时承载 timer / channel / signal 三类源,且 `LoopSignal::stop`
//! 能使 `Core::run` 干净退出 —— 这是 ticket T-0105 的核心验收点。

use std::time::{Duration, Instant};

use calloop::channel::{channel, Event as ChannelEvent};
use calloop::signals::{Signal, Signals};
use calloop::timer::{TimeoutAction, Timer};
use quill::event_loop::Core;

/// 业务状态 mock:统计三类源各触发了几次,以及是否收到退出请求。
#[derive(Default)]
struct State {
    timer_fires: u32,
    channel_msgs: u32,
    signal_fires: u32,
    should_stop: bool,
}

/// 验证 timer + channel + signal 三类源都能进入同一个 `Core`,
/// `LoopSignal::stop()` 能让 `run()` 返回。
///
/// 这里不真 raise SIGUSR1(测试互相打扰 + CI 环境不稳),只注册源证明 API 形状
/// 接得上;timer/channel 走真路径,`Core::run` 的退出由 channel 消息驱动。
#[test]
fn core_runs_three_source_kinds_and_exits_cleanly() {
    let mut core: Core<'_, State> = Core::new().expect("构造 Core 应成功");
    let handle = core.handle();
    let signal = core.signal();

    // --- timer 源 ---
    handle
        .insert_source(
            Timer::from_duration(Duration::from_millis(10)),
            |_deadline, _meta, state: &mut State| {
                state.timer_fires += 1;
                TimeoutAction::Drop
            },
        )
        .expect("注册 timer 应成功");

    // --- channel 源 ---
    let (tx, rx) = channel::<&'static str>();
    handle
        .insert_source(rx, move |event, _meta, state: &mut State| {
            if let ChannelEvent::Msg(_msg) = event {
                state.channel_msgs += 1;
            }
        })
        .expect("注册 channel 应成功");

    // --- signal 源(只证注册路径通,不 raise) ---
    let signals = Signals::new(&[Signal::SIGUSR1]).expect("创建 Signals 应成功");
    handle
        .insert_source(signals, |_event, _meta, state: &mut State| {
            state.signal_fires += 1;
        })
        .expect("注册 signals 应成功");

    // 后台发一个消息 + 兜底 watchdog:避免测试卡死。
    tx.send("ping").expect("channel 发送应成功");

    let mut state = State::default();
    let deadline = Instant::now() + Duration::from_secs(2);

    core.run(&mut state, move |s| {
        // 两类真实驱动的源(timer + channel)都 tick 过一次就退出。
        if s.timer_fires >= 1 && s.channel_msgs >= 1 {
            s.should_stop = true;
        }
        if s.should_stop || Instant::now() > deadline {
            signal.stop();
        }
    })
    .expect("事件循环应干净返回");

    assert_eq!(state.channel_msgs, 1, "channel 消息应被处理一次");
    assert!(state.timer_fires >= 1, "timer 应至少触发一次");
    assert!(state.should_stop, "循环应因业务条件达成而退出,而非超时");
    // signal 未 raise,不断言 fires;注册本身成功即达标(API 形状验证)。
}

/// 退出路径的极简回归:对应 ticket Acceptance "事件循环在 should_exit 置位后退出"
/// 的 mock state 版本。用一个最短 timer 把 dispatch 从无限阻塞里踢醒一轮,
/// 在随后的 idle 回调里直接调 `stop()`,验证 `run()` 会在下一轮 while 检查时返回。
///
/// 注:`EventLoop::run` 会在入口把 `stop` 原子清零(calloop 0.14 源码 `loop_logic.rs`
/// L667),所以必须从 dispatch 回来之后才能调 stop。
#[test]
fn core_exits_when_signaled_from_idle_callback() {
    let mut core: Core<'_, ()> = Core::new().expect("构造 Core 应成功");
    let handle = core.handle();
    let signal = core.signal();

    handle
        .insert_source(Timer::from_duration(Duration::from_millis(1)), |_, _, _| {
            TimeoutAction::Drop
        })
        .expect("注册 timer 应成功");

    let started = Instant::now();
    core.run(&mut (), move |_| {
        // 第一轮 dispatch 回来(timer 到期)就请求停机。
        signal.stop();
    })
    .expect("run 应干净返回");

    assert!(
        started.elapsed() < Duration::from_secs(2),
        "run 应很快返回(<2s),实际 {:?}",
        started.elapsed()
    );
}
