//! Phase 7 T1 端到端往返:真实 tab → 无头渲染 → [`Snapshot`] → JSON 序列化 +
//! 反序列化 → 从往返后的快照重建渲染输入 → 再渲染 → 断言**像素逐字节一致**。
//!
//! **证明什么**:`CellWire` (+ `row_texts` + `cursor`) 承载了渲染器消费的**全部**
//! 信息 —— 若 `CellWire` 丢了任一渲染字段 (fg/bg/char/pos),第二次渲染的 RGBA
//! 必与第一次不同,测试即红。这是 ADR-0015 "快照不丢渲染信息" 的硬验收。
//!
//! **why 走真 wgpu offscreen**: 与 `tests/headless_screenshot.rs` 同决策 —— GPU
//! (NVIDIA Vulkan 5090) 是渲染真相,无法 mock。`tests/` 允许 `expect`/`unwrap`
//! (CLAUDE.md "禁 unwrap" 仅约束 src/)。

use quill::kernel::proto::{CellWire, Snapshot};
use quill::kernel::Session;
use quill::tab::{TabInstance, TabList};
use quill::term::{CellPos, CellRef, Color};
use quill::text::TextSystem;
use quill::wl::{render_headless, HIDPI_SCALE};

const LOGICAL_W: u32 = 800;
const LOGICAL_H: u32 = 600;
const COLS: u16 = 80;
const ROWS: u16 = 24;
const PROMPT_WAIT_MS: u64 = 500;

/// 把 `CellWire` 重建回渲染器入参 `CellRef` (客户端渲染时要做的事)。
/// `CellRef` / `CellPos` / `Color` 字段全 pub,owned 协议类型无损还原。
fn cellwire_to_cellref(w: &CellWire) -> CellRef {
    CellRef {
        pos: CellPos {
            col: w.col,
            line: w.line,
        },
        c: w.c,
        fg: Color::from(w.fg),
        bg: Color::from(w.bg),
    }
}

/// 用真实 shell tab 建 [`Session`],drain PTY 输出喂进 term。
fn session_with_drained_shell() -> Session {
    let tab = TabInstance::spawn(COLS, ROWS).expect("spawn shell tab (CI 环境需 shell)");
    let mut session = Session::new(TabList::new(tab));
    let id = session.tabs().active().id().raw();

    std::thread::sleep(std::time::Duration::from_millis(PROMPT_WAIT_MS));

    // 非阻塞 drain master fd → on_pty_output。fd 已 O_NONBLOCK (INV-009)。
    let mut buf = [0u8; 4096];
    loop {
        let read = session.tabs_mut().active_mut().pty_mut().read(&mut buf);
        match read {
            Ok(0) => break,
            Ok(n) => {
                session.on_pty_output(id, &buf[..n]);
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        }
    }
    session
}

/// 渲染一份快照 (重建 `CellRef` 入参 → `render_headless`),返 `(rgba, w, h)`。
/// `cursor` / `preedit` / `selection` 全 None,让 RGBA 只由 cells + row_texts 决定。
fn render_snapshot(ts: &mut TextSystem, snap: &Snapshot) -> (Vec<u8>, u32, u32) {
    let cells: Vec<CellRef> = snap.cells.iter().map(cellwire_to_cellref).collect();
    render_headless(
        ts,
        &cells,
        snap.cols,
        snap.rows,
        &snap.row_texts,
        LOGICAL_W,
        LOGICAL_H,
        None,
        None,
        None,
    )
    .expect("render_headless from snapshot")
}

#[test]
fn snapshot_roundtrip_preserves_dimensions_cells_cursor() {
    let session = session_with_drained_shell();
    let id = session.tabs().active().id().raw();

    let snap = session.snapshot(id).expect("snapshot of active tab");

    // 基本不变式:cells 数 = cols × rows,row_texts 行数 = rows。
    assert_eq!(snap.cols, COLS as usize);
    assert_eq!(snap.rows, ROWS as usize);
    assert_eq!(
        snap.cells.len(),
        snap.cols * snap.rows,
        "viewport cell 数应为 cols×rows"
    );
    assert_eq!(snap.row_texts.len(), snap.rows);

    // JSON 序列化 + 反序列化。
    let json = serde_json::to_string(&snap).expect("serialize Snapshot to JSON");
    let snap2: Snapshot = serde_json::from_str(&json).expect("deserialize Snapshot from JSON");

    // 逐字段相等 (Snapshot: PartialEq 覆盖 tab_id/cols/rows/cells/row_texts/cursor/title)。
    assert_eq!(snap, snap2, "Snapshot JSON 往返必须逐字段相等");
    // 显式再点名几个渲染关键字段 (回归可读性)。
    assert_eq!(snap.cursor, snap2.cursor, "cursor 往返一致");
    assert_eq!(snap.cells, snap2.cells, "cells 往返一致");
    assert_eq!(snap.row_texts, snap2.row_texts, "row_texts 往返一致");
}

#[test]
fn roundtripped_snapshot_renders_byte_identical() {
    let session = session_with_drained_shell();
    let id = session.tabs().active().id().raw();
    let snap = session.snapshot(id).expect("snapshot of active tab");

    let json = serde_json::to_string(&snap).expect("serialize");
    let snap2: Snapshot = serde_json::from_str(&json).expect("deserialize");

    let mut ts = TextSystem::new().expect("TextSystem::new (需 monospace font)");
    // 同一进程内串行 (render_headless 内部 OnceLock<Mutex>),两次渲染输入若等价
    // 则 RGBA 必逐字节相等 —— 证明往返没丢任何渲染字段。
    let (rgba_pre, w_pre, h_pre) = render_snapshot(&mut ts, &snap);
    let (rgba_post, w_post, h_post) = render_snapshot(&mut ts, &snap2);

    assert_eq!((w_pre, h_pre), (w_post, h_post), "渲染尺寸应一致");
    assert_eq!(
        (w_pre, h_pre),
        (LOGICAL_W * HIDPI_SCALE, LOGICAL_H * HIDPI_SCALE),
        "physical 尺寸 = logical × HIDPI_SCALE"
    );
    assert_eq!(
        rgba_pre.len(),
        rgba_post.len(),
        "RGBA 字节数应一致 (= w×h×4)"
    );
    assert!(
        rgba_pre == rgba_post,
        "往返前后渲染的 RGBA 必须逐字节相等 —— 不等说明 CellWire 丢了渲染字段"
    );
}
