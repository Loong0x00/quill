//! T-0617 #A + #D OSC title 端到端: OSC 0/1/2 字节流 → `TermState::advance`
//! → `TermListener::send_event(Event::Title)` 路径跑通后, [`TabInstance`] 的
//! title `Rc<RefCell<String>>` 应同步.
//!
//! 验证点 (派单 In #D + Acceptance):
//! - alacritty_terminal `EventListener` 接 OSC 0/1/2 后写共享 title
//! - `TabInstance::title()` 看到的快照 == 期望
//! - 多次 OSC 后覆盖 (不累加 / 不漏)
//!
//! **why 直接走 `term.advance(bytes)` 而非 PTY round trip**: 真 shell (bash /
//! zsh) 启动后通常 PS1 内会有自己的 OSC 1/2 (set_title cwd / user@host) 不停
//! 重写 title — 我们的测试 OSC 进 PTY 反被 shell prompt 覆盖, 时序 race. 直接
//! 走 advance 字节路径锁住 "alacritty Term + TermListener + Rc<RefCell<>> 共享
//! 态" 三方接线, 与 lib unit test `term_state_advance_osc_0_updates_title_handle`
//! 同思路但走真实 [`TabInstance::spawn`] 路径 (含 PTY + shell 子进程, 验证
//! TabInstance 持的 title handle 跟 TermState listener 持的 handle 共享同一根
//! Rc).

use quill::tab::TabInstance;

/// `TabInstance::spawn` 后, 直接把 OSC 0 字节串喂进 term.advance, listener 写
/// 共享 title, tab.title() 立即可见.
#[test]
fn osc_0_set_title_propagates_to_tab_title_handle() {
    let mut tab = TabInstance::spawn(80, 24).expect("spawn shell tab");
    // 注意: 真 shell 启动后 PS1 可能立即写 OSC 1 (cwd), 故 spawn 后不一定空 — 我们
    // 不依赖前置空, 只验"我们注入的 OSC 起作用".
    tab.term_mut().advance(b"\x1b]0;test_title\x07");
    assert_eq!(
        tab.title(),
        "test_title",
        "OSC 0 后 tab.title() 应 = 'test_title'; got {:?}",
        tab.title()
    );
}

/// OSC 2 (window title only, xterm 区分 OSC 1=icon name / 2=window title) 与
/// OSC 0 同走 alacritty `Event::Title` 路径 — 我们都视作 set window title.
#[test]
fn osc_2_set_title_propagates_to_tab_title_handle() {
    let mut tab = TabInstance::spawn(80, 24).expect("spawn shell tab");
    tab.term_mut().advance(b"\x1b]2;hello_world\x07");
    assert_eq!(
        tab.title(),
        "hello_world",
        "OSC 2 后 tab.title() 应 = 'hello_world'; got {:?}",
        tab.title()
    );
}

/// 多次 OSC title 应覆盖 (不累加 / 不漏): set "a" → set "b" → final == "b".
#[test]
fn consecutive_osc_titles_overwrite() {
    let mut tab = TabInstance::spawn(80, 24).expect("spawn shell tab");
    tab.term_mut()
        .advance(b"\x1b]0;a\x07\x1b]0;b\x07\x1b]0;final\x07");
    assert_eq!(
        tab.title(),
        "final",
        "最后 OSC 应覆盖前面的; got {:?}",
        tab.title()
    );
}

/// `TabInstance::title_handle()` 与内部 listener 持的 Rc 是同一根 — listener
/// 写, handle clone 立即可见 (派单 In #A + 红线 "TabInstance.title 字段持
/// Rc<RefCell> 持 Cell 内部数据").
#[test]
fn tab_title_handle_shares_with_term_listener() {
    let mut tab = TabInstance::spawn(80, 24).expect("spawn shell tab");
    let handle1 = tab.title_handle();
    let handle2 = tab.title_handle();
    // 两份 handle 应指向同一 Rc inner — 写一份, 另一份立即可见.
    *handle1.borrow_mut() = "from_handle1".to_string();
    assert_eq!(
        &*handle2.borrow(),
        "from_handle1",
        "title_handle() 多次取应共享同一 Rc<RefCell<String>>"
    );
    // 进一步: term.advance(OSC) 后 listener 写 → handle1 / handle2 / tab.title()
    // 三方都看到新值.
    tab.term_mut().advance(b"\x1b]0;via_listener\x07");
    assert_eq!(&*handle1.borrow(), "via_listener");
    assert_eq!(&*handle2.borrow(), "via_listener");
    assert_eq!(tab.title(), "via_listener");
}
