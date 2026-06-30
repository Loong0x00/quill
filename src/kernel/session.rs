//! 无头会话内核的 [`Session`] —— 持有【多个工作区】(T6 砖0 块B),每个工作区
//! 是一份独立的 tab 工作区 (多 PTY + 多 term),不含任何 Wayland / GPU / selection
//! 状态 (ADR-0015:selection 归客户端)。
//!
//! **多 workspace 维度 (T6, ADR-0015 R1 / ADR-0018)**:一个 Session 可持多个
//! [`Workspace`](各含自己的 [`TabList`]);客户端连上默认同步【全部】工作区。每个
//! 工作区有单调递增的 `workspace_id`,与协议层 [`crate::kernel::proto::WorkspaceMeta`]
//! / [`Snapshot::workspace_id`] 对齐。
//!
//! **tab id 全局唯一**(`TabRegistry` 跨整个进程单调):故按 `tab_id` 寻址的方法
//! (`on_pty_output` / `on_input` / `snapshot` / `clear_dirty`) **跨所有工作区线性扫**
//! 定位即可,调用方无需先报 workspace —— daemon / 既有测试的 tab 寻址签名不变。按
//! 工作区操作的方法 (`apply_tab_op` / `resize` / `workspace_info`) 显式带 `workspace_id`。
//!
//! **引用计数生命周期 (anchor / holder / 销毁) 是块C**,本文件块B 只做多 workspace
//! 数据模型 + 协议 `workspace_id` 接入,不含 refcount。
//!
//! **与 `wl/window.rs` 路径的关系**:`on_pty_output` 是 `pty_read_tick_inner`
//! (`window.rs:539`) 去掉 selection rebase + composer OSC133 + Wayland 重绘后的
//! 纯内核子集;`on_input` 是 `Dispatch<WlKeyboard>` 写 PTY 那一步的纯子集。

use std::collections::HashSet;

use anyhow::{bail, Context, Result};

use crate::kernel::proto::{
    CellWire, CursorWire, Snapshot, TabMeta, WorkspaceInfo, WorkspaceList, WorkspaceMeta,
};
use crate::tab::{TabInstance, TabList};

pub use crate::kernel::proto::TabOp;

/// 引用计数生命周期操作 (set_anchor / release) 的结果 (块C, ADR-0015 R1 / ADR-0018)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lifecycle {
    /// 工作区仍在 (refcount > 0,或本次是"断线"非 explicit 不触发销毁)。
    Alive,
    /// refcount (anchor + holders) 归 0 → 工作区已销毁 (移出 Session → drop TabList
    /// → PTY SIGHUP)。
    Destroyed,
    /// 未知 workspace_id,无操作。
    Unknown,
}

/// 一个工作区:单调递增 id + 自己的 [`TabList`] + 引用计数生命周期状态。多个工作区
/// 组成一个 [`Session`]。
///
/// **引用计数 (块C, ADR-0015 R1 / ADR-0018)**:`refcount = anchored as usize +
/// holders.len()`。
/// - **anchor(锚)holder** = spawn 工作区者(standalone daemon = daemon 自己;E′ =
///   父终端进程)。anchor 在 → 工作区**不因 WS 全断而死**(如今天的 daemon)。
/// - **holders** = 当前连着的显示端 (WS 连接 id)。**X 显式关闭 (Release/Close) 释放**;
///   **断线 (网络抖 / 后台 / WS 掉) = 非事件**:移除该连接 holder(防泄漏)但**绝不
///   触发销毁**(靠 anchor 保活)。
/// - refcount 归 0 (仅 explicit 释放 / 清 anchor 时检查) → 销毁工作区。
struct Workspace {
    id: u64,
    tabs: TabList,
    /// anchor holder 是否在 (见类型 doc)。
    anchored: bool,
    /// 连接显示端 holder 集合 (WS 连接 id)。`HashSet` → Hold 幂等、断线移除幂等。
    holders: HashSet<u64>,
}

/// 无头会话内核。边界 = `State.tabs` (ADR-0015):内核 own 多个工作区 ([`TabList`]),
/// 客户端只持序列化快照镜像。
///
/// **非空不变式**:`workspaces` 在块B 永远非空 ([`Self::new`] 建一个,`new_workspace`
/// 只增)。块C 的引用计数销毁会移除工作区,届时 active 调整 + 空 Session 处理在块C。
pub struct Session {
    /// 所有工作区 (连上默认同步全部,ADR-0015 R1)。非空。
    workspaces: Vec<Workspace>,
    /// active 工作区下标 (`< workspaces.len()`)。
    active: usize,
    /// 工作区 id 自增源。单线程 (INV-005),无需 atomic。
    next_ws_id: u64,
}

impl Session {
    /// 用已有 [`TabList`] 建会话 (daemon 启动期建首个 tab 后传入):构造唯一的初始
    /// 工作区,分配 id 1。
    pub fn new(tabs: TabList) -> Self {
        let mut s = Self {
            workspaces: Vec::new(),
            active: 0,
            next_ws_id: 1,
        };
        let id = s.alloc_ws_id();
        s.workspaces.push(Workspace {
            id,
            tabs,
            anchored: false,
            holders: HashSet::new(),
        });
        s
    }

    /// 分配下一个工作区 id (单调递增)。
    fn alloc_ws_id(&mut self) -> u64 {
        let id = self.next_ws_id;
        self.next_ws_id = self.next_ws_id.wrapping_add(1);
        id
    }

    /// 只读访问 **active 工作区**的 tab 列表 (calloop 注册 fd / 调试 / dump 用)。
    /// 砖0 daemon 单工作区:即那唯一工作区。
    pub fn tabs(&self) -> &TabList {
        &self.active_ws().tabs
    }

    /// 可变访问 **active 工作区**的 tab 列表。
    pub fn tabs_mut(&mut self) -> &mut TabList {
        &mut self.active_ws_mut().tabs
    }

    /// active 工作区 id。
    pub fn active_workspace_id(&self) -> u64 {
        self.active_ws().id
    }

    /// 全部工作区 id (连上下发列表 / 测试用,按当前顺序)。
    pub fn workspace_ids(&self) -> Vec<u64> {
        self.workspaces.iter().map(|w| w.id).collect()
    }

    /// 工作区数量。
    pub fn workspace_count(&self) -> usize {
        self.workspaces.len()
    }

    /// 切 active 工作区。未知 id 返 `false` (active 不变)。
    pub fn set_active_workspace(&mut self, ws_id: u64) -> bool {
        match self.ws_idx(ws_id) {
            Some(i) => {
                self.active = i;
                true
            }
            None => false,
        }
    }

    /// 新建一个工作区 (spawn 一个 shell tab),返回新工作区 id。不改 active。
    pub fn new_workspace(&mut self, cols: u16, rows: u16) -> Result<u64> {
        let tab = TabInstance::spawn(cols, rows).context("new_workspace: spawn shell tab 失败")?;
        let id = self.alloc_ws_id();
        self.workspaces.push(Workspace {
            id,
            tabs: TabList::new(tab),
            anchored: false,
            holders: HashSet::new(),
        });
        Ok(id)
    }

    /// PTY 吐出的字节喂进对应 tab 的终端状态机 (跨工作区按全局 `tab_id` 定位)。返回
    /// 喂入后该 tab 的终端是否处于 dirty 状态 (= 该重算快照下发);tab 不存在 (close
    /// race) 时返 `false`。
    ///
    /// **dirty 真相源 = [`crate::term::TermState`] 的 `dirty`**(经 `is_dirty` /
    /// `clear_dirty` 读写)。`advance` 无条件置脏 (空切片也置),所以对一个**存在
    /// 的** tab 本方法恒返 `true` —— 它是"喂了字节、该重算快照"的粗信号,不是
    /// "内容精确变了"的判定 (term 层不提供后者)。消费方下发快照后用
    /// [`Self::clear_dirty`] 复位,否则 dirty 信号长真、每 tick 重发。
    ///
    /// **dirty 只认 `TermState.dirty` 一个源** (不写 `TabInstance.dirty`,那是渲染
    /// 客户端 `wl/window.rs` 的 per-tab 累积位,内核路径写它制造"两个真相源"错觉)。
    pub fn on_pty_output(&mut self, tab_id: u64, bytes: &[u8]) -> bool {
        let Some((wi, ti)) = self.locate(tab_id) else {
            return false;
        };
        let Some(tab) = self.workspaces[wi].tabs.get_mut(ti) else {
            return false;
        };
        tab.term_mut().advance(bytes);
        tab.term().is_dirty()
    }

    /// 清掉指定 tab 的 (term 层) dirty 真相源 (跨工作区定位)。快照下发后调用,与
    /// [`Self::on_pty_output`] 的"置脏"对称,防止 dirty 信号长真而重复全量下发。
    /// tab 不存在返 `false`。
    pub fn clear_dirty(&mut self, tab_id: u64) -> bool {
        let Some((wi, ti)) = self.locate(tab_id) else {
            return false;
        };
        let Some(tab) = self.workspaces[wi].tabs.get_mut(ti) else {
            return false;
        };
        tab.term_mut().clear_dirty();
        true
    }

    /// 客户端输入 (键盘 / 粘贴) 写到指定 tab 的 PTY (跨工作区按全局 `tab_id` 定位)。
    ///
    /// **interim 部分写处理(片2 / bug1)**:[`crate::pty::PtyHandle::write`] 是单次
    /// `libc::write`,非阻塞 PTY 缓冲未空时返 `Ok(n)`(`n < len`,部分写)。这里**循环写
    /// 剩余**:每次推进 `Ok(n)`,真 `WouldBlock`(内核 PTY 缓冲满)才按既有 daily-drive
    /// 背压策略 warn + 丢剩余,`Ok(0)`(罕见,内核不再收)防死循环退出。把"一发就丢尾"
    /// 改成"尽量写完、只在缓冲真满才丢" → keystroke / 中等粘贴不再丢字节。
    ///
    /// **完整解 deferred(app-wide 硬化)**:超 PTY 内核缓冲的超大粘贴,缓冲满后本调用
    /// 仍会丢剩余。daemon 要等价兜底须在 calloop loop 里维护 per-tab pending 缓冲 + PTY
    /// WRITE readiness drain (app-wide 数据流改动),不在本片做。
    ///
    /// `Interrupted`(EINTR)重试本次写;非 `WouldBlock` / `Ok(0)` / `Interrupted` 的
    /// IO 错误上抛。未知 tab_id 报错。
    pub fn on_input(&mut self, tab_id: u64, bytes: &[u8]) -> Result<()> {
        let Some((wi, ti)) = self.locate(tab_id) else {
            bail!("on_input: 未知 tab_id {tab_id}");
        };
        let Some(tab) = self.workspaces[wi].tabs.get(ti) else {
            bail!("on_input: tab_id {tab_id} 定位失效");
        };
        let pty = tab.pty();
        let mut written = 0;
        while written < bytes.len() {
            match pty.write(&bytes[written..]) {
                Ok(0) => {
                    // 内核不再接受字节(非 WouldBlock 的零写,罕见)→ 防死循环退出,
                    // 按背压策略丢剩余。
                    tracing::warn!(
                        tab_id,
                        "PTY write 返回 0, 丢剩余 {} 字节",
                        bytes.len() - written
                    );
                    break;
                }
                Ok(n) => written += n,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    tracing::warn!(
                        tab_id,
                        "PTY write 背压 (WouldBlock), 已写 {written} / 丢剩余 {} 字节",
                        bytes.len() - written
                    );
                    break;
                }
                // EINTR:被信号打断的部分/零写,非错误 → 重试本次写(不推进 written,
                // 重新写剩余切片)。PtyHandle::write 内部通常已吞 EINTR(本层多半见不到),
                // 防御性兜底,别把可重试的中断当致命错上抛。
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(anyhow::Error::new(e).context("on_input: PTY write 失败")),
            }
        }
        Ok(())
    }

    /// 应用一条 tab 工作区操作 (新建 / 关闭 / 切换 / 换序 / 重命名) 到**指定工作区**。
    /// 未知 `workspace_id` 报错。
    pub fn apply_tab_op(&mut self, workspace_id: u64, op: TabOp) -> Result<()> {
        let Some(wi) = self.ws_idx(workspace_id) else {
            bail!("apply_tab_op: 未知 workspace_id {workspace_id}");
        };
        let tabs = &mut self.workspaces[wi].tabs;
        match op {
            TabOp::New => {
                // 新 tab 跟随当前 active tab 的尺寸 (一个 PTY 一个尺寸,ADR-0015
                // "主控端定尺寸")。
                let (cols, rows) = tabs.active().term().dimensions();
                let tab = TabInstance::spawn(clamp_u16(cols), clamp_u16(rows))?;
                tabs.push(tab);
                Ok(())
            }
            TabOp::Close { tab_id } => {
                let Some(idx) = tabs.iter().position(|t| t.id().raw() == tab_id) else {
                    bail!("apply_tab_op Close: workspace {workspace_id} 无 tab_id {tab_id}");
                };
                // remove 返回 Some(TabInstance),drop 触发 PTY SIGHUP + fd close。
                tabs.remove(idx);
                Ok(())
            }
            TabOp::Select { idx } => {
                if !tabs.set_active(idx) {
                    bail!("apply_tab_op Select: idx {idx} 越界 (len={})", tabs.len());
                }
                Ok(())
            }
            TabOp::Reorder { origin, target } => {
                if !tabs.swap_reorder(origin, target) {
                    bail!("apply_tab_op Reorder: ({origin},{target}) 非法");
                }
                Ok(())
            }
            TabOp::SetTitle { tab_id, title } => {
                let Some(idx) = tabs.iter().position(|t| t.id().raw() == tab_id) else {
                    bail!("apply_tab_op SetTitle: workspace {workspace_id} 无 tab_id {tab_id}");
                };
                let Some(tab) = tabs.get_mut(idx) else {
                    bail!("apply_tab_op SetTitle: tab_id {tab_id} 定位失效");
                };
                tab.set_title(title);
                Ok(())
            }
        }
    }

    /// 改尺寸:**指定工作区**所有 tab 的 term + PTY 同步 resize (一个工作区一个尺寸)。
    /// 未知 `workspace_id` 报错。
    pub fn resize(&mut self, workspace_id: u64, cols: u16, rows: u16) -> Result<()> {
        let Some(wi) = self.ws_idx(workspace_id) else {
            bail!("resize: 未知 workspace_id {workspace_id}");
        };
        for tab in self.workspaces[wi].tabs.iter_mut() {
            tab.term_mut().resize(cols as usize, rows as usize);
            tab.pty().resize(cols, rows)?;
        }
        Ok(())
    }

    /// 取指定 tab 的全量渲染快照 (跨工作区定位,快照带其 `workspace_id`)。tab 不存在返 `None`。
    pub fn snapshot(&self, tab_id: u64) -> Option<Snapshot> {
        let (wi, ti) = self.locate(tab_id)?;
        let ws = &self.workspaces[wi];
        let tab = ws.tabs.get(ti)?;
        Some(snapshot_of(ws.id, tab))
    }

    /// 取 **active 工作区的 active tab** 的全量渲染快照 (非空不变式保证总有一个)。
    pub fn snapshot_active(&self) -> Snapshot {
        let ws = self.active_ws();
        snapshot_of(ws.id, ws.tabs.active())
    }

    /// 某工作区结构 (tab 条 UI 用)。尺寸取该工作区 active tab 的 grid 尺寸。未知
    /// `workspace_id` 返 `None`。
    pub fn workspace_info(&self, workspace_id: u64) -> Option<WorkspaceInfo> {
        let ws = self.ws_by_id(workspace_id)?;
        let (cols, rows) = ws.tabs.active().term().dimensions();
        let tabs = ws
            .tabs
            .iter()
            .map(|t| TabMeta {
                tab_id: t.id().raw(),
                title: t.title(),
            })
            .collect();
        Some(WorkspaceInfo {
            workspace_id: ws.id,
            tabs,
            active: ws.tabs.active_idx(),
            cols,
            rows,
        })
    }

    /// 全部工作区摘要列表 (连上下发 / 工作区增删 / 切 active 时再发)。每个工作区
    /// 标题摘要取其 active tab 标题。
    pub fn workspace_list(&self) -> WorkspaceList {
        let active_id = self.active_ws().id;
        let workspaces = self
            .workspaces
            .iter()
            .map(|w| WorkspaceMeta {
                id: w.id,
                title: w.tabs.active().title(),
                tab_count: w.tabs.len(),
                active: w.id == active_id,
            })
            .collect();
        WorkspaceList {
            workspaces,
            active: active_id,
        }
    }

    // ── 引用计数生命周期 (块C, ADR-0015 R1 / ADR-0018) ──────────────────────────

    /// 设/清工作区的 **anchor** holder (见 [`Workspace`] doc)。standalone daemon 启动
    /// 后给自己唯一工作区 `set_anchor(id, true)` → WS 全断不死。**清 anchor**
    /// (`anchored=false`) 是显式生命周期事件:若清后 refcount 归 0 则销毁工作区并返
    /// [`Lifecycle::Destroyed`]。未知 id 返 [`Lifecycle::Unknown`]。
    pub fn set_anchor(&mut self, ws_id: u64, anchored: bool) -> Lifecycle {
        let Some(wi) = self.ws_idx(ws_id) else {
            return Lifecycle::Unknown;
        };
        self.workspaces[wi].anchored = anchored;
        if !anchored && self.refcount_at(wi) == 0 {
            self.destroy_at(wi);
            Lifecycle::Destroyed
        } else {
            Lifecycle::Alive
        }
    }

    /// 登记一个连接显示端 holder (WS 连上 → 成为 holder,引用计数 +1)。`holder` = 连接
    /// id。幂等 (同 id 重复 Hold 不重复计)。未知 `ws_id` 返 `false`。
    pub fn hold(&mut self, ws_id: u64, holder: u64) -> bool {
        let Some(wi) = self.ws_idx(ws_id) else {
            return false;
        };
        self.workspaces[wi].holders.insert(holder);
        true
    }

    /// 释放一个 holder。
    ///
    /// - **`explicit = true`(X 显式关闭:收到 `Release` / WS `Close`)**:移除 holder
    ///   后若 refcount 归 0 → 销毁工作区,返 [`Lifecycle::Destroyed`]。
    /// - **`explicit = false`(断线:网络抖 / 后台 / WS 掉 / 收割 / cap 断开)= 非事件**:
    ///   移除该连接 holder(防泄漏,连接已死、其 id 不会再用,重连是新 id 重新 Hold),
    ///   但**绝不触发销毁**(靠 anchor 保活)。恒返 [`Lifecycle::Alive`]。
    ///
    /// **区分关闭 vs 断线是关键**(ADR-0015 R1):正因断线不销毁、anchor 构造上恒在
    /// (PTY⟺桌面窗口原子耦合),手机后台/锁屏/断网杀不掉会话。未知 `ws_id` 返
    /// [`Lifecycle::Unknown`]。
    pub fn release(&mut self, ws_id: u64, holder: u64, explicit: bool) -> Lifecycle {
        let Some(wi) = self.ws_idx(ws_id) else {
            return Lifecycle::Unknown;
        };
        self.workspaces[wi].holders.remove(&holder);
        if explicit && self.refcount_at(wi) == 0 {
            self.destroy_at(wi);
            Lifecycle::Destroyed
        } else {
            Lifecycle::Alive
        }
    }

    /// 工作区当前 refcount (`anchored + holders`)。未知 `ws_id` 返 `None`。
    pub fn refcount(&self, ws_id: u64) -> Option<usize> {
        self.ws_idx(ws_id).map(|wi| self.refcount_at(wi))
    }

    fn refcount_at(&self, wi: usize) -> usize {
        let w = &self.workspaces[wi];
        w.anchored as usize + w.holders.len()
    }

    /// 销毁下标 `wi` 的工作区:移出 `workspaces`(drop → drop TabList → 各 PTY SIGHUP)
    /// 并调整 `active` 下标。
    ///
    /// **active 调整**:删 `wi` 后,> `wi` 的下标左移 1 → active 在 `wi` 之后则 −1;
    /// active 正是 `wi`(其工作区被销毁)则 clamp 到剩余范围(选邻近)。空 Session 时
    /// active 归 0(daemon 单工作区因 anchor 恒在不会走到这;见类型非空不变式注解)。
    fn destroy_at(&mut self, wi: usize) {
        self.workspaces.remove(wi);
        if self.workspaces.is_empty() {
            self.active = 0;
        } else if wi < self.active {
            self.active -= 1;
        } else if wi == self.active {
            self.active = self.active.min(self.workspaces.len() - 1);
        }
    }

    /// active 工作区引用 (非空不变式)。
    fn active_ws(&self) -> &Workspace {
        &self.workspaces[self.active]
    }

    fn active_ws_mut(&mut self) -> &mut Workspace {
        &mut self.workspaces[self.active]
    }

    /// workspace_id → workspaces 下标。
    fn ws_idx(&self, ws_id: u64) -> Option<usize> {
        self.workspaces.iter().position(|w| w.id == ws_id)
    }

    fn ws_by_id(&self, ws_id: u64) -> Option<&Workspace> {
        self.ws_idx(ws_id).map(|i| &self.workspaces[i])
    }

    /// 全局 `tab_id` → `(工作区下标, tab 下标)`。`TabId` 字段私有 (INV-010) 不能从
    /// `u64` 重建 + tab id 全局唯一,故跨工作区线性扫 `id().raw()` 定位 (工作区×tab
    /// 数 daily-drive 都个位数,O(n) 无所谓)。
    fn locate(&self, tab_id: u64) -> Option<(usize, usize)> {
        for (wi, w) in self.workspaces.iter().enumerate() {
            if let Some(ti) = w.tabs.iter().position(|t| t.id().raw() == tab_id) {
                return Some((wi, ti));
            }
        }
        None
    }
}

/// 把一个 tab 的当前终端状态拍成 [`Snapshot`](带其所属 `workspace_id`)。`cells` /
/// `row_texts` / `cursor` 直接对应 `render_headless` 入参 (`wl/render.rs:4310`),
/// 客户端拿到即可独立渲染。
fn snapshot_of(workspace_id: u64, tab: &TabInstance) -> Snapshot {
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
        workspace_id,
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

    /// 建一个含单个 for_test tab 的单工作区 Session (不 spawn 真 shell,纯内核逻辑)。
    fn session_one_tab() -> Session {
        TabRegistry::reset_for_test();
        let tab = TabInstance::for_test("shell");
        Session::new(TabList::new(tab))
    }

    /// `on_pty_output` 把字节喂进 term,快照能读回内容,且返回 dirty。快照带 workspace_id。
    #[test]
    fn on_pty_output_feeds_term_and_snapshot_reflects() {
        let mut s = session_one_tab();
        let ws_id = s.active_workspace_id();
        let id = s.tabs().active().id().raw();
        let dirty = s.on_pty_output(id, b"hi");
        assert!(dirty, "advance 后 tab 应 dirty");

        let snap = s.snapshot(id).expect("snapshot of known tab");
        assert_eq!(snap.tab_id, id);
        assert_eq!(snap.workspace_id, ws_id, "快照应带所属工作区 id");
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

    /// apply_tab_op:New 增 tab,Select 切 active,Reorder 换序,Close 减 tab (寻址到工作区)。
    #[test]
    fn apply_tab_op_new_select_reorder_close() {
        let mut s = session_one_tab();
        let ws = s.active_workspace_id();
        let first = s.tabs().active().id().raw();
        assert_eq!(s.tabs().len(), 1);

        // New (spawn 真 shell — CI 环境必备,同既有 tab 测试)
        s.apply_tab_op(ws, TabOp::New).expect("New");
        assert_eq!(s.tabs().len(), 2);
        let second = s.tabs().iter().nth(1).expect("2nd tab").id().raw();
        assert_ne!(first, second);

        // Select idx 1
        s.apply_tab_op(ws, TabOp::Select { idx: 1 })
            .expect("Select");
        assert_eq!(s.tabs().active_idx(), 1);
        // 越界 Select 报错
        assert!(s.apply_tab_op(ws, TabOp::Select { idx: 99 }).is_err());

        // Reorder 0<->1
        s.apply_tab_op(
            ws,
            TabOp::Reorder {
                origin: 0,
                target: 1,
            },
        )
        .expect("Reorder");
        // active 跟随 id 锁定 (换序后仍指向原 active tab)
        assert_eq!(s.tabs().active().id().raw(), second);

        // Close first
        s.apply_tab_op(ws, TabOp::Close { tab_id: first })
            .expect("Close");
        assert_eq!(s.tabs().len(), 1);
        assert!(s.apply_tab_op(ws, TabOp::Close { tab_id: 4242 }).is_err());

        // 未知 workspace 报错
        assert!(s.apply_tab_op(999_999, TabOp::New).is_err());
    }

    /// SetTitle 改标题,workspace_info 反映;workspace_info 带 workspace_id。
    #[test]
    fn set_title_and_workspace_info() {
        let mut s = session_one_tab();
        let ws_id = s.active_workspace_id();
        let id = s.tabs().active().id().raw();
        s.apply_tab_op(
            ws_id,
            TabOp::SetTitle {
                tab_id: id,
                title: "renamed".to_string(),
            },
        )
        .expect("SetTitle");
        let ws = s.workspace_info(ws_id).expect("workspace_info");
        assert_eq!(ws.workspace_id, ws_id);
        assert_eq!(ws.tabs.len(), 1);
        assert_eq!(ws.tabs[0].title, "renamed");
        assert_eq!(ws.active, 0);
        // 未知工作区返 None
        assert!(s.workspace_info(424_242).is_none());
    }

    /// dirty 真相源单一性:on_pty_output 置脏 → clear_dirty 复位 → 再 advance 又置脏。
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

    /// 多 workspace:new_workspace 增工作区、各持独立 tab;按全局 tab_id 跨工作区
    /// 定位正确 (快照带各自 workspace_id);workspace_list 列出全部 + 标 active。
    #[test]
    fn multi_workspace_isolation_and_list() {
        let mut s = session_one_tab();
        let ws1 = s.active_workspace_id();
        let tab1 = s.tabs().active().id().raw();
        assert_eq!(s.workspace_count(), 1);

        // 新建第二个工作区 (spawn 真 shell — CI 环境必备)。
        let ws2 = s.new_workspace(80, 24).expect("new_workspace");
        assert_ne!(ws1, ws2, "工作区 id 应唯一");
        assert_eq!(s.workspace_count(), 2);
        // new_workspace 不改 active
        assert_eq!(s.active_workspace_id(), ws1);

        // 第二个工作区的 tab id 与第一个不同;切过去取它的 tab。
        assert!(s.set_active_workspace(ws2));
        let tab2 = s.tabs().active().id().raw();
        assert_ne!(tab1, tab2);

        // 喂字节到各工作区的 tab,跨工作区按全局 tab_id 定位,快照带正确 workspace_id。
        assert!(s.on_pty_output(tab1, b"in-ws1"));
        assert!(s.on_pty_output(tab2, b"in-ws2"));
        let snap1 = s.snapshot(tab1).expect("snap tab1");
        let snap2 = s.snapshot(tab2).expect("snap tab2");
        assert_eq!(snap1.workspace_id, ws1);
        assert_eq!(snap2.workspace_id, ws2);
        assert!(snap1.row_texts[0].starts_with("in-ws1"));
        assert!(snap2.row_texts[0].starts_with("in-ws2"));

        // workspace_list:两个工作区,active 标在当前 active (ws2)。
        let list = s.workspace_list();
        assert_eq!(list.workspaces.len(), 2);
        assert_eq!(list.active, ws2);
        let m2 = list
            .workspaces
            .iter()
            .find(|m| m.id == ws2)
            .expect("ws2 in list");
        assert!(m2.active, "active 工作区应标 active");
        let m1 = list
            .workspaces
            .iter()
            .find(|m| m.id == ws1)
            .expect("ws1 in list");
        assert!(!m1.active);
        assert_eq!(s.workspace_ids().len(), 2);

        // 未知工作区切换返 false。
        assert!(!s.set_active_workspace(7_777_777));
    }

    /// 引用计数:anchor 在 → WS holder 全(断线)掉不销毁;显式关闭 + 清 anchor → 归 0
    /// 销毁。确定性、纯逻辑。
    #[test]
    fn refcount_anchor_keeps_alive_then_destroys_at_zero() {
        let mut s = session_one_tab();
        let ws1 = s.active_workspace_id();
        // 第二个工作区用来销毁,避免清空 Session (active 单工作区因 anchor 不会被销毁)。
        let ws2 = s.new_workspace(80, 24).expect("new ws");

        // ws2:anchor(模拟 standalone daemon)+ 两个 WS holder。
        assert_eq!(s.set_anchor(ws2, true), Lifecycle::Alive);
        assert!(s.hold(ws2, 100));
        assert!(s.hold(ws2, 101));
        assert_eq!(s.refcount(ws2), Some(3), "anchor + 2 holders");
        // Hold 幂等。
        assert!(s.hold(ws2, 100));
        assert_eq!(s.refcount(ws2), Some(3));

        // 断线 (非 explicit) 两个 holder 全掉 → 不销毁 (anchor 保活)。
        assert_eq!(s.release(ws2, 100, false), Lifecycle::Alive);
        assert_eq!(s.release(ws2, 101, false), Lifecycle::Alive);
        assert_eq!(s.refcount(ws2), Some(1), "只剩 anchor");
        assert!(
            s.workspace_ids().contains(&ws2),
            "anchor 在 → WS 全断工作区不死 (如今天)"
        );

        // 加 holder 再显式 X 关闭:anchor 仍在 → refcount 不为 0 → 不销毁。
        assert!(s.hold(ws2, 102));
        assert_eq!(
            s.release(ws2, 102, true),
            Lifecycle::Alive,
            "anchor 在 → 显式关闭一个 holder 只关那个 view"
        );
        assert_eq!(s.refcount(ws2), Some(1));

        // 清 anchor(显式生命周期事件)→ refcount 归 0 → 销毁。
        assert_eq!(s.set_anchor(ws2, false), Lifecycle::Destroyed);
        assert!(!s.workspace_ids().contains(&ws2), "归 0 应销毁工作区");
        assert_eq!(s.workspace_count(), 1);
        // ws1 仍在,Session 非空,snapshot_active 不 panic。
        assert!(s.workspace_ids().contains(&ws1));
        let _ = s.snapshot_active();

        // 未知 workspace。
        assert_eq!(s.refcount(999), None);
        assert_eq!(s.set_anchor(999, false), Lifecycle::Unknown);
        assert_eq!(s.release(999, 1, true), Lifecycle::Unknown);
        assert!(!s.hold(999, 1));
    }

    /// 无 anchor 时:显式关闭最后一个 holder → 归 0 销毁;而**断线**(非 explicit)即便
    /// 归 0 也**不**销毁(断线非事件)。
    #[test]
    fn refcount_explicit_destroys_disconnect_does_not() {
        let mut s = session_one_tab();
        let ws2 = s.new_workspace(80, 24).expect("new ws");

        // 断线归 0 不销毁 (非事件)。
        assert!(s.hold(ws2, 300));
        assert_eq!(s.release(ws2, 300, false), Lifecycle::Alive);
        assert_eq!(s.refcount(ws2), Some(0));
        assert!(
            s.workspace_ids().contains(&ws2),
            "断线即便归 0 也不销毁 (非事件)"
        );

        // 重连 (新 holder id) → 显式 X 关闭 → 归 0 → 销毁。
        assert!(s.hold(ws2, 301));
        assert_eq!(s.release(ws2, 301, true), Lifecycle::Destroyed);
        assert!(!s.workspace_ids().contains(&ws2));
        assert_eq!(s.workspace_count(), 1);
    }

    /// 销毁工作区时 active 下标正确调整 (删 active 之前 → 左移;删 active 自身 → 选邻近)。
    #[test]
    fn destroy_adjusts_active_index() {
        let mut s = session_one_tab();
        let ws1 = s.active_workspace_id();
        let ws2 = s.new_workspace(80, 24).expect("ws2");
        let ws3 = s.new_workspace(80, 24).expect("ws3");
        // workspaces = [ws1, ws2, ws3], active = ws1 (idx0)。删 ws2 (idx1) → active 仍 ws1。
        assert!(s.hold(ws2, 1));
        assert_eq!(s.release(ws2, 1, true), Lifecycle::Destroyed);
        assert_eq!(s.active_workspace_id(), ws1);

        // workspaces = [ws1, ws3];切 active 到 ws3 (idx1),删 ws1 (idx0) → active 左移到 ws3。
        assert!(s.set_active_workspace(ws3));
        assert!(s.hold(ws1, 1));
        assert_eq!(s.release(ws1, 1, true), Lifecycle::Destroyed);
        assert_eq!(
            s.active_workspace_id(),
            ws3,
            "删 active 之前的工作区 → active 左移仍指原 ws"
        );
        assert_eq!(s.workspace_count(), 1);
    }
}
