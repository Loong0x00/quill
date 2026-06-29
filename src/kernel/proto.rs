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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceInfo {
    pub tabs: Vec<TabMeta>,
    pub active: usize,
    pub cols: usize,
    pub rows: usize,
}

/// 内核 → 客户端消息。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ServerMsg {
    /// 单 tab 全量快照 (连接时 + dirty 帧)。
    Snapshot(Snapshot),
    /// 工作区结构变化 (tab 增删 / 换序 / 重命名 / active 切换)。
    Workspace(WorkspaceInfo),
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

/// 客户端 → 内核消息。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ClientMsg {
    /// 键盘 / 粘贴字节,写到指定 tab 的 PTY。
    Input { tab_id: u64, bytes: Vec<u8> },
    /// 主控端改尺寸 (cols/rows),内核 resize 所有 tab 的 PTY + term。
    Resize { cols: u16, rows: u16 },
    /// tab 工作区操作。
    TabOp(TabOp),
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

    /// `ClientMsg` / `ServerMsg` enum 也能 JSON 往返 (externally tagged)。
    #[test]
    fn messages_json_roundtrip() {
        let msgs = vec![
            ClientMsg::Input {
                tab_id: 1,
                bytes: vec![0x1b, b'[', b'A'],
            },
            ClientMsg::Resize { cols: 80, rows: 24 },
            ClientMsg::TabOp(TabOp::New),
            ClientMsg::TabOp(TabOp::Close { tab_id: 2 }),
            ClientMsg::TabOp(TabOp::SetTitle {
                tab_id: 3,
                title: "x".to_string(),
            }),
        ];
        for m in msgs {
            let j = serde_json::to_string(&m).expect("ser ClientMsg");
            let b: ClientMsg = serde_json::from_str(&j).expect("de ClientMsg");
            assert_eq!(m, b);
        }
    }
}
