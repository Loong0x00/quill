//! 无头会话内核 (Phase 7,ADR-0015)。
//!
//! 把 quill 拆成「无头会话内核 (daemon) + 渲染客户端」的内核侧:持有所有
//! PTY + VT + 屏幕模型 + tab 工作区,渲染端 (桌面 `wl/` / 手机 web) 都是它的
//! 客户端,交换序列化快照。
//!
//! - [`proto`] —— 跨线程 / 跨进程线缆协议 (owned 可 serde 的 [`proto::Snapshot`]
//!   / [`proto::CellWire`] / [`proto::ClientMsg`] / [`proto::ServerMsg`])。
//! - [`feed`] —— 父 ↔ 子 (E′, ADR-0018) 共享子进程的轻量二进制喂料帧 codec
//!   ([`feed::FeedFrame`] / [`feed::encode_into`] / 增量 [`feed::FeedDecoder`])。
//! - [`session`] —— [`session::Session`]:tab 工作区 + 数据流入口
//!   (`on_pty_output` / `on_input` / `apply_tab_op` / `snapshot`)。
//! - [`daemon`] —— Phase 7 T2 单线程 calloop daemon 切片:注册 PTY fd +
//!   `UnixListener`,客户端连上发当前快照。
//!
//! **Phase 7 T1 范围**:协议 + `Session` 骨架。
//! **Phase 7 T2 范围**([`daemon`]):单线程 calloop 接线 (PTY fd + `UnixListener`)。
//! WS fan-out / dirty 增量广播 / 客户端 [`proto::ClientMsg`] 回灌仍是后续 ticket
//! (ADR-0015 Phase 1 §5-6)。

pub mod daemon;
pub mod feed;
pub mod proto;
pub mod session;

pub use feed::{FeedDecoder, FeedFrame, FrameKind};
pub use proto::{
    CellWire, ClientMsg, ColorWire, CursorShapeWire, CursorWire, ServerMsg, Snapshot, TabMeta,
    TabOp, WorkspaceInfo, WorkspaceList, WorkspaceMeta,
};
pub use session::{Lifecycle, Session};
