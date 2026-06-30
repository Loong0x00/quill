//! 无头内核 ↔ 渲染客户端线缆协议 (Phase 7 T1, ADR-0015 Phase 1)。
//!
//! 这里的类型是**跨线程 / 跨进程**传输的 owned 纯数据：渲染客户端 (桌面
//! `wl/` 或手机 web) 与无头内核之间只交换序列化字节。设计三铁律：
//!
//! 1. **owned, 无借用 / 无 `Rc` / 无字体句柄** —— ADR-0015 头号约束:
//!    `TermState` / `TabInstance` 含 `Rc<RefCell<String>>` 非 `Send`,快照必须
//!    先 `.clone()` 成纯结构才能过线程边界发给 WS fan-out。
//! 2. **忠实承载渲染所需信息** —— [`CellWire`] 镜像 [`crate::term::CellRef`] 的
//!    **全部**字段 (`pos` / `c` / `fg` / `bg`)。已逐行核实 `wl/render.rs` 两条
//!    draw 路径 (`Renderer::draw_frame` 实装 per-glyph `cell.fg` 上色 +
//!    `cell.bg` 块,`render_headless` 截图路径同 `cell.bg` 块) 消费的就是这四个
//!    字段;`CellRef` 本身不带 bold/italic/underline (INVERSE 在 `cells_iter`
//!    阶段已解析为 fg/bg 互换),所以 `CellWire` 镜像 `CellRef` 即对当前渲染器
//!    无损 (见模块底部 fidelity note)。
//! 3. **先 JSON 后 bincode** —— ADR-0015 决策:Phase 1 用 `serde_json` 调通,
//!    带宽优化 (bincode + dirty-row 增量) 留后续 ticket。
//!
//! 字段命名照 ADR-0015 Phase 1 §2:`Snapshot{tab_id,cols,rows,cells,row_texts,
//! cursor,title}`,直接对应 `render_headless` 入参 (`wl/render.rs:4310`)。

use serde::{Deserialize, Serialize};

use crate::term::{CellRef, Color, CursorShape};

/// 已解析 RGB,镜像 [`crate::term::Color`] (owned,可 serde)。
///
/// 终端 cell 永远不透明,无 alpha —— 与 `term::Color` 同决策。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ColorWire {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

impl From<Color> for ColorWire {
    fn from(c: Color) -> Self {
        Self {
            r: c.r,
            g: c.g,
            b: c.b,
        }
    }
}

impl From<ColorWire> for Color {
    fn from(c: ColorWire) -> Self {
        Color {
            r: c.r,
            g: c.g,
            b: c.b,
        }
    }
}

/// 单 cell 的线缆表示,镜像 [`crate::term::CellRef`] 全字段。
///
/// **为何 `pos` 拍平成 `col` / `line` 而非嵌 `CellPos`**: 线缆类型尽量扁平,
/// JSON 体积小、客户端解析直白。语义与 `CellRef.pos.col` / `.pos.line` 一一对应。
///
/// **为何同时需要 [`Snapshot::row_texts`]**: 渲染器的字形 (glyph) 路径走
/// `text_system.shape_line(row_text)` 整行 shaping (CJK fallback / 宽字符 advance
/// 由 cosmic-text 算),`CellWire.c` 是逐 cell 字符 (含 alacritty `WIDE_CHAR_SPACER`
/// 占位的空格)。两者都进快照:`cells` 给 per-cell fg/bg 块 + per-glyph 上色,
/// `row_texts` 给 shaping 正确的行文本。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CellWire {
    /// viewport 列号 (= `CellRef.pos.col`)。
    pub col: usize,
    /// viewport 行号 (= `CellRef.pos.line`, `0..rows`,scrollback 已映射回视口)。
    pub line: usize,
    /// cell 字符。空 cell / 宽字符 spacer 为 `' '`。
    pub c: char,
    /// 前景色 (已解析 RGB)。`draw_frame` per-glyph 上色用 (`render.rs:2941`)。
    pub fg: ColorWire,
    /// 背景色 (已解析 RGB)。两条 draw 路径都按此画 cell bg 块 (default bg 跳过)。
    pub bg: ColorWire,
}

impl From<CellRef> for CellWire {
    fn from(c: CellRef) -> Self {
        Self {
            col: c.pos.col,
            line: c.pos.line,
            c: c.c,
            fg: c.fg.into(),
            bg: c.bg.into(),
        }
    }
}

/// 光标形状,镜像 [`crate::term::CursorShape`] 5 个 variant。
///
/// **为何镜像 term 层 5-variant 而非 render 层 [`crate::wl::render::CursorStyle`]
/// 4-variant**: 快照承载的是**会话状态** (term 的真实光标语义),`Hidden` 是一种
/// 状态而非"不画";客户端渲染时自行把 `Hidden` 折叠到 `visible=false`、把其余 4
/// 个映射成 render 的 `CursorStyle`。协议层不替客户端做渲染决策。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CursorShapeWire {
    Block,
    Underline,
    Beam,
    HollowBlock,
    Hidden,
}

impl From<CursorShape> for CursorShapeWire {
    fn from(s: CursorShape) -> Self {
        match s {
            CursorShape::Block => CursorShapeWire::Block,
            CursorShape::Underline => CursorShapeWire::Underline,
            CursorShape::Beam => CursorShapeWire::Beam,
            CursorShape::HollowBlock => CursorShapeWire::HollowBlock,
            CursorShape::Hidden => CursorShapeWire::Hidden,
        }
    }
}

/// 光标的线缆表示。
///
/// **为何不带颜色**: `wl/render.rs` 的 `CursorInfo.color` 常态走光标位 cell 的
/// `fg` (见 `render.rs:690` doc)。客户端已持 [`Snapshot::cells`],能查 `(col,line)`
/// 处 cell 的 `fg` 自行推导,协议层不冗余携带。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CursorWire {
    pub col: usize,
    pub line: usize,
    /// `TermState::cursor_visible()` —— scrollback 中 / DECRST 25 时为 `false`。
    pub visible: bool,
    pub shape: CursorShapeWire,
}

/// 单 tab 的完整渲染快照。字段照 ADR-0015 Phase 1 §2 + `render_headless` 入参。
///
/// 客户端拿到即可独立渲染一帧:`cells` + `row_texts` + `cols`/`rows` 喂
/// `render_headless` (或桌面 `Renderer::draw_frame` 等价路径),`cursor` 画光标,
/// `title` 画标题栏。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Snapshot {
    /// 所属工作区 id (T6 多 workspace:标识此快照属于哪个工作区,与
    /// [`WorkspaceMeta::id`] / [`WorkspaceInfo::workspace_id`] 对齐)。
    pub workspace_id: u64,
    /// 来源 tab 的 [`crate::tab::TabId::raw`] (协议层用 `u64`,不暴露 newtype)。
    pub tab_id: u64,
    pub cols: usize,
    pub rows: usize,
    /// viewport 全量 cell (`rows × cols` 个)。
    pub cells: Vec<CellWire>,
    /// 每视口行的 shaping 文本 (`TermState::line_text`,已跳宽字符 spacer)。
    pub row_texts: Vec<String>,
    pub cursor: CursorWire,
    /// OSC 标题。空串时客户端 fallback 到默认 "quill" (与 `wl/render` 同决策)。
    pub title: String,
}

/// 工作区某 tab 的元信息 (tab 条 UI 用,不含 grid 内容)。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TabMeta {
    pub tab_id: u64,
    pub title: String,
}

/// 工作区整体结构 (tab 列表 + active + 当前尺寸)。连接时与 tab 增删 / 换序 /
/// 重命名时下发,让客户端画 tab 条。
///
/// **`workspace_id`(T6 多 workspace 维度)**: 一个内核 [`crate::kernel::Session`]
/// 持多个工作区,本结构描述**其中一个**;`workspace_id` 标识是哪一个,与
/// [`WorkspaceMeta::id`] / [`Snapshot::workspace_id`] 对齐。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceInfo {
    /// 所属工作区 id (T6:一个 Session 多工作区,标识是哪个)。
    pub workspace_id: u64,
    pub tabs: Vec<TabMeta>,
    pub active: usize,
    pub cols: usize,
    pub rows: usize,
}

/// 单个工作区在【工作区列表】里的摘要 (不含 tab 明细 / grid 内容)。客户端用它画
/// "工作区切换器" UI;明细 (tab 条) 走 [`WorkspaceInfo`],grid 走 [`Snapshot`]。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceMeta {
    pub id: u64,
    /// 工作区标题摘要 (取其 active tab 标题;空则客户端 fallback)。
    pub title: String,
    pub tab_count: usize,
    /// 是否为 Session 当前 active 工作区。
    pub active: bool,
}

/// 工作区列表 (T6 多 workspace 维度):连接时下发,工作区增删 / 切换 active 时再发。
/// 客户端默认同步桌面【全部】工作区 (ADR-0015 R1),用此画切换器。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceList {
    pub workspaces: Vec<WorkspaceMeta>,
    /// 当前 active 工作区 id (与某 [`WorkspaceMeta::id`] 对应;列表空时无意义)。
    pub active: u64,
}

/// 内核 → 客户端消息。
///
/// **两个平面 (T6 冻结)**: 控制面走本枚举的 JSON (WS **Text** 帧),数据面走 PTY
/// **原始字节** (WS **Binary** 帧)。数据面字节属于哪个 (workspace, tab) 由控制面
/// [`ServerMsg::StreamFocus`] 标记 (连上 + 切换时发) —— 即"字节流帧打 tab/workspace
/// 标签",标签走控制面而非每帧包头,热路径零额外拷贝/解析。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ServerMsg {
    /// 单 tab 全量快照 (连接时关键帧 + dirty 帧;字节流模型下降级为连上引导帧)。
    Snapshot(Snapshot),
    /// 单个工作区结构 (tab 增删 / 换序 / 重命名 / active 切换 → 全量重发该工作区)。
    Workspace(WorkspaceInfo),
    /// 全部工作区列表 (连上即发;工作区增删 / 切 active 时再发)。
    Workspaces(WorkspaceList),
    /// tab 增量事件:某工作区新增一个 tab (连上发全量 [`ServerMsg::Workspace`],之后增量)。
    TabAdded { workspace_id: u64, tab: TabMeta },
    /// tab 增量事件:某工作区移除一个 tab。
    TabRemoved { workspace_id: u64, tab_id: u64 },
    /// **字节流标签**:声明此后 WS **Binary** 帧承载的 PTY 字节属于哪个
    /// (workspace, tab)。连上发一次 (当前 active),active tab / workspace 切换时再发。
    StreamFocus { workspace_id: u64, tab_id: u64 },
}

/// 客户端发起的 tab 工作区操作。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum TabOp {
    /// 新建 tab (内核按当前尺寸 spawn shell)。
    New,
    /// 关闭指定 tab。
    Close { tab_id: u64 },
    /// 切换 active tab (idx 越界则忽略)。
    Select { idx: usize },
    /// 拖拽换序。
    Reorder { origin: usize, target: usize },
    /// 设置标题 (客户端手动重命名;OSC 标题由 PTY 输出自动改)。
    SetTitle { tab_id: u64, title: String },
}

/// 客户端 → 内核消息 (控制面,WS **Text** 帧)。
///
/// **数据面对照**: 热路径键盘字节走 WS **Binary** 帧 (daemon 直接写 active tab PTY,
/// 见 [`ServerMsg::StreamFocus`]),不经此枚举。[`ClientMsg::Input`] 用于**寻址到非
/// 焦点 tab** 的输入 (带 `workspace_id` + `tab_id`),与热路径 Binary 并存。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ClientMsg {
    /// 键盘 / 粘贴字节,写到指定 (workspace, tab) 的 PTY。
    Input {
        workspace_id: u64,
        tab_id: u64,
        bytes: Vec<u8>,
    },
    /// 改尺寸 (cols/rows):内核 resize 指定工作区所有 tab 的 PTY + term。
    Resize {
        workspace_id: u64,
        cols: u16,
        rows: u16,
    },
    /// tab 工作区操作 (寻址到指定工作区)。
    TabOp { workspace_id: u64, op: TabOp },
    /// **显式持有该工作区** —— 客户端连上即发,成为一个 holder (引用计数 +1)。
    /// 区别于"断线后重连":重连再发一次 Hold 重新登记。
    Hold { workspace_id: u64 },
    /// **显式关闭 (X) = 释放该 holder** (引用计数 −1)。与**断线**严格区分:断线
    /// (后台 / 锁屏 / 网络抖 / WS 掉) 是**非事件**、不发本消息、不释放;只有用户真
    /// 点 X / 关闭视图才发。anchor + holders 归 0 → 内核销毁工作区 (ADR-0015 R1 /
    /// ADR-0018)。
    Release { workspace_id: u64 },
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 手搓一个 [`Snapshot`] (不依赖 PTY / GPU),JSON 往返后逐字段相等。
    /// 这是 [`CellWire`] 无损性的最快确定性证明;真实 tab + 渲染的端到端往返
    /// 在 `tests/kernel_snapshot_roundtrip.rs`。
    #[test]
    fn snapshot_json_roundtrip_is_lossless() {
        let snap = Snapshot {
            workspace_id: 1,
            tab_id: 7,
            cols: 3,
            rows: 2,
            cells: vec![
                CellWire {
                    col: 0,
                    line: 0,
                    c: 'A',
                    fg: ColorWire {
                        r: 0xd3,
                        g: 0xd3,
                        b: 0xd3,
                    },
                    bg: ColorWire { r: 0, g: 0, b: 0 },
                },
                CellWire {
                    col: 1,
                    line: 0,
                    c: '中',
                    fg: ColorWire {
                        r: 10,
                        g: 20,
                        b: 30,
                    },
                    bg: ColorWire {
                        r: 40,
                        g: 50,
                        b: 60,
                    },
                },
            ],
            row_texts: vec!["A中".to_string(), String::new()],
            cursor: CursorWire {
                col: 2,
                line: 1,
                visible: true,
                shape: CursorShapeWire::Beam,
            },
            title: "demo".to_string(),
        };

        let json = serde_json::to_string(&snap).expect("serialize Snapshot");
        let back: Snapshot = serde_json::from_str(&json).expect("deserialize Snapshot");
        assert_eq!(snap, back, "Snapshot JSON 往返必须逐字段相等");
    }

    /// `CellRef -> CellWire -> Color/CellRef` 字段不丢:fg/bg/char/pos 全保。
    #[test]
    fn cellwire_mirrors_cellref_fields() {
        let cr = CellRef {
            pos: crate::term::CellPos { col: 5, line: 9 },
            c: 'Z',
            fg: Color { r: 1, g: 2, b: 3 },
            bg: Color { r: 4, g: 5, b: 6 },
        };
        let w: CellWire = cr.into();
        assert_eq!(w.col, 5);
        assert_eq!(w.line, 9);
        assert_eq!(w.c, 'Z');
        assert_eq!(w.fg, ColorWire { r: 1, g: 2, b: 3 });
        assert_eq!(w.bg, ColorWire { r: 4, g: 5, b: 6 });
        // ColorWire -> Color 反向也无损 (客户端重建渲染输入要用)
        assert_eq!(Color::from(w.fg), cr.fg);
        assert_eq!(Color::from(w.bg), cr.bg);
    }

    /// `ClientMsg` JSON 往返 (含 T6 多 workspace 寻址 + Hold/Release 生命周期)。
    #[test]
    fn client_messages_json_roundtrip() {
        let msgs = vec![
            ClientMsg::Input {
                workspace_id: 1,
                tab_id: 1,
                bytes: vec![0x1b, b'[', b'A'],
            },
            ClientMsg::Resize {
                workspace_id: 1,
                cols: 80,
                rows: 24,
            },
            ClientMsg::TabOp {
                workspace_id: 1,
                op: TabOp::New,
            },
            ClientMsg::TabOp {
                workspace_id: 2,
                op: TabOp::Close { tab_id: 2 },
            },
            ClientMsg::TabOp {
                workspace_id: 2,
                op: TabOp::SetTitle {
                    tab_id: 3,
                    title: "x".to_string(),
                },
            },
            ClientMsg::Hold { workspace_id: 7 },
            ClientMsg::Release { workspace_id: 7 },
        ];
        for m in msgs {
            let j = serde_json::to_string(&m).expect("ser ClientMsg");
            let b: ClientMsg = serde_json::from_str(&j).expect("de ClientMsg");
            assert_eq!(m, b);
        }
    }

    /// `ServerMsg` 控制面消息 JSON 往返 (T6 多 workspace 列表 + tab 增删 + 字节流标签)。
    #[test]
    fn server_messages_json_roundtrip() {
        let msgs = vec![
            ServerMsg::Workspace(WorkspaceInfo {
                workspace_id: 3,
                tabs: vec![TabMeta {
                    tab_id: 11,
                    title: "sh".to_string(),
                }],
                active: 0,
                cols: 80,
                rows: 24,
            }),
            ServerMsg::Workspaces(WorkspaceList {
                workspaces: vec![
                    WorkspaceMeta {
                        id: 3,
                        title: "sh".to_string(),
                        tab_count: 1,
                        active: true,
                    },
                    WorkspaceMeta {
                        id: 4,
                        title: String::new(),
                        tab_count: 2,
                        active: false,
                    },
                ],
                active: 3,
            }),
            ServerMsg::TabAdded {
                workspace_id: 4,
                tab: TabMeta {
                    tab_id: 21,
                    title: "new".to_string(),
                },
            },
            ServerMsg::TabRemoved {
                workspace_id: 4,
                tab_id: 21,
            },
            ServerMsg::StreamFocus {
                workspace_id: 3,
                tab_id: 11,
            },
        ];
        for m in msgs {
            let j = serde_json::to_string(&m).expect("ser ServerMsg");
            let b: ServerMsg = serde_json::from_str(&j).expect("de ServerMsg");
            assert_eq!(m, b);
        }
    }
}
