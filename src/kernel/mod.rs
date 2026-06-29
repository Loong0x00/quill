//! 无头会话内核 (Phase 7,ADR-0015)。
//!
//! 把 quill 拆成「无头会话内核 (daemon) + 渲染客户端」的内核侧:持有所有
//! PTY + VT + 屏幕模型 + tab 工作区,渲染端 (桌面 `wl/` / 手机 web) 都是它的
//! 客户端,交换序列化快照。
//!
//! - [`proto`] —— 跨线程 / 跨进程线缆协议 (owned 可 serde 的 [`proto::Snapshot`]
//!   / [`proto::CellWire`] / [`proto::ClientMsg`] / [`proto::ServerMsg`])。
//! - [`session`] —— [`session::Session`]:tab 工作区 + 数据流入口
//!   (`on_pty_output` / `on_input` / `apply_tab_op` / `snapshot`)。
//!
//! **Phase 7 T1 范围**:协议 + `Session` 骨架。daemon calloop 接线 (PTY fd +
//! `UnixListener`) 与 WS fan-out 是后续 ticket (ADR-0015 Phase 1 §4-6)。

pub mod proto;
pub mod session;

pub use proto::{
    CellWire, ClientMsg, ColorWire, CursorShapeWire, CursorWire, ServerMsg, Snapshot, TabMeta,
    TabOp, WorkspaceInfo,
};
pub use session::Session;
