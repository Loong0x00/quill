//! 无头会话内核的 [`Session`] —— 持有整个 tab 工作区 (多 PTY + 多 term),
//! 不含任何 Wayland / GPU / selection 状态 (ADR-0015:selection 归客户端)。
//!
//! Phase 7 T1 骨架:协议 ([`crate::kernel::proto`]) + `Session` 数据流入口
//! (`on_pty_output` / `on_input` / `apply_tab_op` + `snapshot`)。daemon 的
//! calloop 接线 (注册 PTY fd + `UnixListener`) 与 WS fan-out 是后续 ticket
//! (ADR-0015 Phase 1 §4-6),本文件不碰。
//!
//! **与 `wl/window.rs` 路径的关系**:`on_pty_output` 是 `pty_read_tick_inner`
//! (`window.rs:539`) 去掉 selection rebase + composer OSC133 + Wayland 重绘后的
//! 纯内核子集;`on_input` 是 `Dispatch<WlKeyboard>` 写 PTY 那一步的纯子集。

use anyhow::{bail, Result};

use crate::kernel::proto::{CellWire, CursorWire, Snapshot, TabMeta, WorkspaceInfo};
use crate::tab::{TabInstance, TabList};

pub use crate::kernel::proto::TabOp;

/// 无头会话内核。边界 = `State.tabs` (ADR-0015):内核 own [`TabList`],客户端
/// 只持序列化快照镜像。
pub struct Session {
    tabs: TabList,
}

impl Session {
    /// 用已有 [`TabList`] 建会话 (daemon 启动期建首个 tab 后传入)。
    pub fn new(tabs: TabList) -> Self {
        Self { tabs }
    }

    /// 只读访问 tab 工作区 (calloop 注册 fd / 调试用)。
    pub fn tabs(&self) -> &TabList {
        &self.tabs
    }

    /// 可变访问 tab 工作区。
    pub fn tabs_mut(&mut self) -> &mut TabList {
        &mut self.tabs
    }

    /// PTY 吐出的字节喂进对应 tab 的终端状态机。返回喂入后该 tab 的终端是否
    /// 处于 dirty 状态 (= 该重算快照下发);tab 不存在 (close race) 时返 `false`。
    ///
    /// **dirty 真相源 = [`crate::term::TermState`] 的 `dirty`**(经 `is_dirty` /
    /// `clear_dirty` 读写)。`advance` 无条件置脏 (空切片也置),所以对一个**存在
    /// 的** tab 本方法恒返 `true` —— 它是"喂了字节、该重算快照"的粗信号,不是
    /// "内容精确变了"的判定 (term 层不提供后者)。消费方下发快照后用
    /// [`Self::clear_dirty`] 复位,否则 dirty 信号长真、每 tick 重发。
    ///
    /// **T1 审码记的双标志已收口**:不再写 [`crate::tab::TabInstance::mark_dirty`]。
    /// `TabInstance.dirty` 是渲染客户端 (`wl/window.rs`) 的 per-tab 累积位;内核
    /// 快照/下发只认 term 层 dirty,内核路径再写它纯属冗余、且制造"两个真相源"
    /// 的错觉。**内核侧 dirty 只认 `TermState.dirty` 一个源。**
    ///
    /// **Phase 1 范围**:直接 `term.advance`,不做 composer OSC133 扫描
    /// (inline 补全提示是客户端关注点) 也不做 selection scroll-rebase
    /// (selection 归客户端,ADR-0015)。
    pub fn on_pty_output(&mut self, tab_id: u64, bytes: &[u8]) -> bool {
        let Some(idx) = self.idx_by_raw(tab_id) else {
            return false;
        };
        let Some(tab) = self.tabs.get_mut(idx) else {
            return false;
        };
        tab.term_mut().advance(bytes);
        tab.term().is_dirty()
    }

    /// 清掉指定 tab 的 (term 层) dirty 真相源。快照下发后调用,与
    /// [`Self::on_pty_output`] 的"置脏"对称,防止 dirty 信号长真而重复全量下发。
    /// tab 不存在返 `false`。
    pub fn clear_dirty(&mut self, tab_id: u64) -> bool {
        let Some(idx) = self.idx_by_raw(tab_id) else {
            return false;
        };
        let Some(tab) = self.tabs.get_mut(idx) else {
            return false;
        };
        tab.term_mut().clear_dirty();
        true
    }

    /// 客户端输入 (键盘 / 粘贴) 写到指定 tab 的 PTY。
    ///
    /// 背压 (`WouldBlock`) 按 [`crate::pty::PtyHandle::write`] doc 的 daily-drive
    /// 策略丢字节 + warn;其余 IO 错误上抛。未知 tab_id 报错。
    pub fn on_input(&mut self, tab_id: u64, bytes: &[u8]) -> Result<()> {
        let Some(idx) = self.idx_by_raw(tab_id) else {
            bail!("on_input: 未知 tab_id {tab_id}");
        };
        let Some(tab) = self.tabs.get(idx) else {
            bail!("on_input: tab_id {tab_id} idx {idx} 失效");
        };
        match tab.pty().write(bytes) {
            Ok(_) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                tracing::warn!(
                    tab_id,
                    "PTY write 背压 (WouldBlock), 丢 {} 字节",
                    bytes.len()
                );
                Ok(())
            }
            Err(e) => Err(anyhow::Error::new(e).context("on_input: PTY write 失败")),
        }
    }

    /// 应用一条 tab 工作区操作 (新建 / 关闭 / 切换 / 换序 / 重命名)。
    pub fn apply_tab_op(&mut self, op: TabOp) -> Result<()> {
        match op {
            TabOp::New => {
                // 新 tab 跟随当前 active tab 的尺寸 (一个 PTY 一个尺寸,ADR-0015
                // "主控端定尺寸")。
                let (cols, rows) = self.tabs.active().term().dimensions();
                let tab = TabInstance::spawn(clamp_u16(cols), clamp_u16(rows))?;
                self.tabs.push(tab);
                Ok(())
            }
            TabOp::Close { tab_id } => {
                let Some(idx) = self.idx_by_raw(tab_id) else {
                    bail!("apply_tab_op Close: 未知 tab_id {tab_id}");
                };
                // remove 返回 Some(TabInstance),drop 触发 PTY SIGHUP + fd close。
                self.tabs.remove(idx);
                Ok(())
            }
            TabOp::Select { idx } => {
                if !self.tabs.set_active(idx) {
                    bail!(
                        "apply_tab_op Select: idx {idx} 越界 (len={})",
                        self.tabs.len()
                    );
                }
                Ok(())
            }
            TabOp::Reorder { origin, target } => {
                if !self.tabs.swap_reorder(origin, target) {
                    bail!("apply_tab_op Reorder: ({origin},{target}) 非法");
                }
                Ok(())
            }
            TabOp::SetTitle { tab_id, title } => {
                let Some(idx) = self.idx_by_raw(tab_id) else {
                    bail!("apply_tab_op SetTitle: 未知 tab_id {tab_id}");
                };
                let Some(tab) = self.tabs.get_mut(idx) else {
                    bail!("apply_tab_op SetTitle: tab_id {tab_id} idx {idx} 失效");
                };
                tab.set_title(title);
                Ok(())
            }
        }
    }

    /// 主控端改尺寸:所有 tab 的 term + PTY 同步 resize (一个工作区一个尺寸)。
    pub fn resize(&mut self, cols: u16, rows: u16) -> Result<()> {
        for tab in self.tabs.iter_mut() {
            tab.term_mut().resize(cols as usize, rows as usize);
            tab.pty().resize(cols, rows)?;
        }
        Ok(())
    }

    /// 取指定 tab 的全量渲染快照。tab 不存在返 `None`。
    pub fn snapshot(&self, tab_id: u64) -> Option<Snapshot> {
        let idx = self.idx_by_raw(tab_id)?;
        let tab = self.tabs.get(idx)?;
        Some(snapshot_of(tab))
    }

    /// 取当前 active tab 的全量渲染快照 (tabs 非空不变式保证总有一个)。
    pub fn snapshot_active(&self) -> Snapshot {
        snapshot_of(self.tabs.active())
    }

    /// 工作区结构 (tab 条 UI 用)。尺寸取 active tab 的 grid 尺寸。
    pub fn workspace_info(&self) -> WorkspaceInfo {
        let (cols, rows) = self.tabs.active().term().dimensions();
        let tabs = self
            .tabs
            .iter()
            .map(|t| TabMeta {
                tab_id: t.id().raw(),
                title: t.title(),
            })
            .collect();
        WorkspaceInfo {
            tabs,
            active: self.tabs.active_idx(),
            cols,
            rows,
        }
    }

    /// raw `u64` → tab idx。`TabId` 字段私有 (INV-010) 不能从 `u64` 重建,
    /// 故协议层 `u64` 入参靠线性扫 `id().raw()` 定位 (tab 数 daily-drive 个位数,
    /// O(n) 无所谓)。
    fn idx_by_raw(&self, tab_id: u64) -> Option<usize> {
        self.tabs.iter().position(|t| t.id().raw() == tab_id)
    }
}

/// 把一个 tab 的当前终端状态拍成 [`Snapshot`]。`cells` / `row_texts` / `cursor`
/// 直接对应 `render_headless` 入参 (`wl/render.rs:4310`),客户端拿到即可独立渲染。
fn snapshot_of(tab: &TabInstance) -> Snapshot {
    let term = tab.term();
    let (cols, rows) = term.dimensions();
    let cells: Vec<CellWire> = term.cells_iter().map(CellWire::from).collect();
    let row_texts: Vec<String> = (0..rows).map(|r| term.line_text(r)).collect();
    let cp = term.cursor_pos();
    let cursor = CursorWire {
        col: cp.col,
        line: cp.line,
        visible: term.cursor_visible(),
        shape: term.cursor_shape().into(),
    };
    Snapshot {
        tab_id: tab.id().raw(),
        cols,
        rows,
        cells,
        row_texts,
        cursor,
        title: tab.title(),
    }
}

/// grid 尺寸 `usize` → PTY winsize `u16`,clamp 防溢出 (6K 终端宽远低于 u16 上限,
/// 防御性写法,与 `render_headless` 的 `saturating_mul` 同精神)。至少 1 (0 列/行
/// 无意义,`TermState::resize` 也 clamp 到 1)。
fn clamp_u16(v: usize) -> u16 {
    v.clamp(1, u16::MAX as usize) as u16
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tab::TabRegistry;

    /// 建一个含单个 for_test tab 的 Session (不 spawn 真 shell,纯内核逻辑)。
    fn session_one_tab() -> Session {
        TabRegistry::reset_for_test();
        let tab = TabInstance::for_test("shell");
        Session::new(TabList::new(tab))
    }

    /// `on_pty_output` 把字节喂进 term,快照能读回内容,且返回 dirty。
    #[test]
    fn on_pty_output_feeds_term_and_snapshot_reflects() {
        let mut s = session_one_tab();
        let id = s.tabs().active().id().raw();
        let dirty = s.on_pty_output(id, b"hi");
        assert!(dirty, "advance 后 tab 应 dirty");

        let snap = s.snapshot(id).expect("snapshot of known tab");
        assert_eq!(snap.tab_id, id);
        // (0,0) / (0,1) cell 应是 'h' / 'i'
        let c00 = snap
            .cells
            .iter()
            .find(|c| c.line == 0 && c.col == 0)
            .expect("(0,0)");
        let c01 = snap
            .cells
            .iter()
            .find(|c| c.line == 0 && c.col == 1)
            .expect("(0,1)");
        assert_eq!(c00.c, 'h');
        assert_eq!(c01.c, 'i');
        // row_texts[0] 以 "hi" 开头
        assert!(
            snap.row_texts[0].starts_with("hi"),
            "row 0 文本应以 hi 开头"
        );
    }

    /// 未知 tab_id:on_pty_output 返 false,snapshot 返 None,on_input 报错。
    #[test]
    fn unknown_tab_id_is_handled() {
        let mut s = session_one_tab();
        assert!(!s.on_pty_output(9999, b"x"));
        assert!(s.snapshot(9999).is_none());
        assert!(s.on_input(9999, b"x").is_err());
    }

    /// apply_tab_op:New 增 tab,Select 切 active,Reorder 换序,Close 减 tab。
    #[test]
    fn apply_tab_op_new_select_reorder_close() {
        let mut s = session_one_tab();
        let first = s.tabs().active().id().raw();
        assert_eq!(s.tabs().len(), 1);

        // New (spawn 真 shell — CI 环境必备,同既有 tab 测试)
        s.apply_tab_op(TabOp::New).expect("New");
        assert_eq!(s.tabs().len(), 2);
        let second = s.tabs().iter().nth(1).expect("2nd tab").id().raw();
        assert_ne!(first, second);

        // Select idx 1
        s.apply_tab_op(TabOp::Select { idx: 1 }).expect("Select");
        assert_eq!(s.tabs().active_idx(), 1);
        // 越界 Select 报错
        assert!(s.apply_tab_op(TabOp::Select { idx: 99 }).is_err());

        // Reorder 0<->1
        s.apply_tab_op(TabOp::Reorder {
            origin: 0,
            target: 1,
        })
        .expect("Reorder");
        // active 跟随 id 锁定 (换序后仍指向原 active tab)
        assert_eq!(s.tabs().active().id().raw(), second);

        // Close first
        s.apply_tab_op(TabOp::Close { tab_id: first })
            .expect("Close");
        assert_eq!(s.tabs().len(), 1);
        assert!(s.apply_tab_op(TabOp::Close { tab_id: 4242 }).is_err());
    }

    /// SetTitle 改标题,workspace_info 反映。
    #[test]
    fn set_title_and_workspace_info() {
        let mut s = session_one_tab();
        let id = s.tabs().active().id().raw();
        s.apply_tab_op(TabOp::SetTitle {
            tab_id: id,
            title: "renamed".to_string(),
        })
        .expect("SetTitle");
        let ws = s.workspace_info();
        assert_eq!(ws.tabs.len(), 1);
        assert_eq!(ws.tabs[0].title, "renamed");
        assert_eq!(ws.active, 0);
    }

    /// dirty 真相源单一性 (T2 收口双标志):on_pty_output 置脏 → clear_dirty
    /// 复位 → 再 advance 又置脏。内核侧 dirty 只有 term 层这一个源被读写。
    #[test]
    fn dirty_truth_source_set_and_clear() {
        let mut s = session_one_tab();
        let id = s.tabs().active().id().raw();

        assert!(s.on_pty_output(id, b"x"), "advance 后应 dirty");
        assert!(s.clear_dirty(id), "已知 tab clear 应成功");
        assert!(
            !s.tabs().active().term().is_dirty(),
            "clear 后 term (唯一真相源) 不应 dirty"
        );

        // 再喂字节又置脏 (置脏/复位对称)。
        assert!(s.on_pty_output(id, b"y"));
        assert!(s.tabs().active().term().is_dirty());

        // 未知 tab:clear 返 false。
        assert!(!s.clear_dirty(9999));
    }
}
