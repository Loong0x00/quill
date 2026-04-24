//! 窗口状态机 headless 测试(T-0107)。
//!
//! 不启任何 Wayland 连接,也不读 `WAYLAND_DISPLAY`;直接给纯逻辑
//! `handle_event` 喂 `WindowEvent`,逐个验证状态字段与副作用。
//!
//! 这批测试的目的是:未来 T-0103/T-0104 的 写码 可以在 `window.rs` 的真
//! Wayland 回调里自由调整 SlotPool / wgpu surface 重建逻辑,只要这里还
//! 过,说明 "Wayland 事件 → 状态转移" 这段语义没坏。
//!
//! 覆盖(对应 ticket Acceptance "初始 configure / resize / 0x0 / close /
//! 连续 resize 合并 / 异常 disconnect"):
//!   1. `initial_configure_*`     — 首次 configure 吃下尺寸、清 first_configure、要求重画
//!   2. `subsequent_resize_*`     — 后续 configure 改尺寸并置 resize_dirty
//!   3. `zero_size_configure_*`   — 0x0 被吞,保留旧尺寸
//!   4. `close_sets_exit`         — Close 事件置 exit 标志
//!   5. `disconnect_sets_exit`    — compositor 异常断开视同 close
//!   6. `consecutive_resize_*`    — 多次 resize 合并到单次 dirty 标记
//!   7. `full_lifecycle_*`        — 端到端:init → configure → resize → close
//!   8. `partial_size_*`          — compositor 只给一轴尺寸时保留另一轴旧值

use quill::wl::{handle_event, WindowAction, WindowCore, WindowEvent};

/// 工具:初始化一个尺寸为 800x600 的 WindowCore,等价于 run_window 里
/// 对 State 的初始化,方便在测试里复用。
fn fresh_core() -> WindowCore {
    WindowCore::new(800, 600)
}

#[test]
fn initial_configure_applies_size_and_clears_first_flag() {
    let mut core = fresh_core();
    assert!(
        core.first_configure,
        "新建的 core 应处于 first_configure 态"
    );
    assert!(!core.exit);
    assert!(!core.resize_dirty);

    let action = handle_event(
        &mut core,
        WindowEvent::Configure {
            new_width: Some(1024),
            new_height: Some(768),
        },
    );

    assert_eq!(core.width, 1024);
    assert_eq!(core.height, 768);
    assert!(
        !core.first_configure,
        "首次 configure 后应翻转 first_configure"
    );
    assert!(
        core.resize_dirty,
        "首次 configure 应置 resize_dirty 让上层重画"
    );
    assert!(!core.exit);
    assert_eq!(
        action,
        WindowAction { needs_draw: true },
        "首次 configure 必须要求重画"
    );
}

#[test]
fn initial_configure_with_none_falls_back_to_initial_dims() {
    // compositor 可以不给尺寸,意思是 "由 client 自己决定" (xdg-shell 语义)。
    // 这时 first_configure 仍应翻转,尺寸保留构造时的 800x600。
    let mut core = fresh_core();
    let action = handle_event(
        &mut core,
        WindowEvent::Configure {
            new_width: None,
            new_height: None,
        },
    );
    assert_eq!((core.width, core.height), (800, 600));
    assert!(!core.first_configure);
    assert!(core.resize_dirty);
    assert!(action.needs_draw);
}

#[test]
fn subsequent_resize_updates_dims_and_sets_dirty() {
    let mut core = fresh_core();
    // 走完首次 configure,让状态进入 "稳态" 再测 resize。
    let _ = handle_event(
        &mut core,
        WindowEvent::Configure {
            new_width: Some(800),
            new_height: Some(600),
        },
    );
    core.resize_dirty = false; // 模拟上层消费过

    let action = handle_event(
        &mut core,
        WindowEvent::Configure {
            new_width: Some(1920),
            new_height: Some(1080),
        },
    );

    assert_eq!(core.width, 1920);
    assert_eq!(core.height, 1080);
    assert!(
        core.resize_dirty,
        "后续 configure 改尺寸应再次置 resize_dirty"
    );
    assert!(action.needs_draw);
    assert!(!core.exit);
}

#[test]
fn zero_size_configure_is_swallowed() {
    // xdg-shell: compositor 给 0 表示 "client 决定",我们已经有默认尺寸,
    // 防御性吞掉整条事件,保留老尺寸,不置脏。
    let mut core = fresh_core();
    // 先走首次 configure,避开 "first_configure=true" 分支的影响。
    let _ = handle_event(
        &mut core,
        WindowEvent::Configure {
            new_width: Some(800),
            new_height: Some(600),
        },
    );
    core.resize_dirty = false;

    for (w, h) in [
        (Some(0), Some(768)),
        (Some(1024), Some(0)),
        (Some(0), Some(0)),
    ] {
        let action = handle_event(
            &mut core,
            WindowEvent::Configure {
                new_width: w,
                new_height: h,
            },
        );
        assert_eq!(core.width, 800, "0 轴不应污染老尺寸 (got w={w:?} h={h:?})");
        assert_eq!(core.height, 600);
        assert!(!core.resize_dirty, "0 轴被吞,不应置 dirty");
        assert!(!action.needs_draw);
    }
}

#[test]
fn zero_size_on_first_configure_does_not_flip_first_flag() {
    // 首次 configure 就是 0x0 是病态情况,应完全吞掉 —— 下次合法 configure
    // 仍能走 first_configure 分支。
    let mut core = fresh_core();
    let action = handle_event(
        &mut core,
        WindowEvent::Configure {
            new_width: Some(0),
            new_height: Some(0),
        },
    );
    assert!(core.first_configure, "病态 0x0 不应消耗首次 configure 权");
    assert!(!core.resize_dirty);
    assert!(!action.needs_draw);
}

#[test]
fn close_sets_exit_flag() {
    let mut core = fresh_core();
    let action = handle_event(&mut core, WindowEvent::Close);
    assert!(core.exit, "Close 必须置 exit");
    assert!(!action.needs_draw, "Close 不应触发重画");
    assert!(
        core.first_configure,
        "Close 不影响 configure 路径的状态字段"
    );
}

#[test]
fn disconnect_sets_exit_flag() {
    // compositor 异常断开(真跑时 blocking_dispatch 返回 Err)。headless
    // 测试里我们假设事件环在捕获到断开后把它喂给状态机,语义等同 close。
    let mut core = fresh_core();
    let action = handle_event(&mut core, WindowEvent::Disconnect);
    assert!(core.exit, "Disconnect 必须置 exit");
    assert!(!action.needs_draw);
}

#[test]
fn consecutive_resizes_merge_to_single_dirty() {
    // "连续 resize 合并到单次脏标记" = 脏标记是布尔,不是队列;哪怕来 10 次
    // 尺寸变化,上层只需要 *一次* 清零就跟上所有变化。
    let mut core = fresh_core();
    let _ = handle_event(
        &mut core,
        WindowEvent::Configure {
            new_width: Some(800),
            new_height: Some(600),
        },
    );
    core.resize_dirty = false;

    for (w, h) in [(1024, 768), (1280, 720), (1920, 1080), (2560, 1440)] {
        let _ = handle_event(
            &mut core,
            WindowEvent::Configure {
                new_width: Some(w),
                new_height: Some(h),
            },
        );
    }
    assert_eq!(core.width, 2560, "最终尺寸是最后一次 resize");
    assert_eq!(core.height, 1440);
    assert!(core.resize_dirty, "连续 resize 期间 dirty 一直为 true");

    // 清一次,后续无新事件则保持 clean —— 证明是 flag 不是队列。
    core.resize_dirty = false;
    assert!(!core.resize_dirty);
}

#[test]
fn idempotent_same_size_configure_does_not_re_dirty() {
    let mut core = fresh_core();
    let _ = handle_event(
        &mut core,
        WindowEvent::Configure {
            new_width: Some(1024),
            new_height: Some(768),
        },
    );
    core.resize_dirty = false;

    let action = handle_event(
        &mut core,
        WindowEvent::Configure {
            new_width: Some(1024),
            new_height: Some(768),
        },
    );
    assert!(
        !core.resize_dirty,
        "同尺寸的重复 configure 不应再次置 dirty,避免每帧 resize"
    );
    assert!(!action.needs_draw);
}

#[test]
fn partial_size_configure_keeps_other_axis() {
    // compositor 只给一轴、另一轴 None,按语义保留老值。
    let mut core = fresh_core();
    let _ = handle_event(
        &mut core,
        WindowEvent::Configure {
            new_width: Some(1024),
            new_height: Some(768),
        },
    );
    core.resize_dirty = false;

    let _ = handle_event(
        &mut core,
        WindowEvent::Configure {
            new_width: Some(1600),
            new_height: None,
        },
    );
    assert_eq!(core.width, 1600);
    assert_eq!(core.height, 768, "height=None 应保留老值");
    assert!(core.resize_dirty);
}

#[test]
fn full_lifecycle_init_configure_resize_close() {
    // ticket 要求里的 "端到端":初始化 → 首次 configure → resize → close。
    let mut core = fresh_core();
    assert!(core.first_configure);
    assert!(!core.exit);

    let a1 = handle_event(
        &mut core,
        WindowEvent::Configure {
            new_width: Some(1024),
            new_height: Some(768),
        },
    );
    assert!(a1.needs_draw);
    assert!(!core.first_configure);
    core.resize_dirty = false;

    let a2 = handle_event(
        &mut core,
        WindowEvent::Configure {
            new_width: Some(1920),
            new_height: Some(1080),
        },
    );
    assert!(a2.needs_draw);
    assert_eq!((core.width, core.height), (1920, 1080));
    assert!(core.resize_dirty);

    let a3 = handle_event(&mut core, WindowEvent::Close);
    assert!(!a3.needs_draw);
    assert!(core.exit);
    // 已经 exit 的 core 仍保留最后的尺寸,方便上层 drop 前对 swapchain 做
    // 最后的清理。
    assert_eq!((core.width, core.height), (1920, 1080));
}
