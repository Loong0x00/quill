//! Multi-tab 数据结构 (T-0608, ghostty 风 native multi-tab).
//!
//! 撕 CLAUDE.md 早期"多标签 → tmux"非目标条款 (2026-04-26 user 否决):
//! 单进程多 PTY + 多 [`crate::term::TermState`] + 单标签条 UI, 视觉与
//! ghostty / kitty native multi-tab 同款.
//!
//! ## 模块边界 (INV-010)
//!
//! - [`TabId`] — newtype `u64`, 字段私有 [INV-010]; 拖拽换序时 anchor 不变
//!   依赖 id 而非 idx.
//! - [`TabInstance`] — quill 自有 struct, 字段全私有 (term/pty/title/dirty/id);
//!   下游通过 `&self` / `&mut self` 方法访问.
//! - [`NextTabId`] — 模块私有 atomic counter, 单调递增, 起步 1.
//!
//! ## 不变式
//!
//! - [`TabInstance::pty`] 持的 master fd 必须 O_NONBLOCK (INV-009, 由
//!   [`crate::pty::PtyHandle::spawn_shell`] / `spawn_program` 在构造时 fcntl 一次).
//! - 新建 tab 立即取得**全新** [`TabId`]; close 后不复用 id, 即使 idx
//!   已被后续 tab 占用 (INV-005 多 fd 注册时用 id 索引 calloop registration token).

use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, Result};

use crate::pty::PtyHandle;
use crate::term::TermState;

/// quill 自有的 tab id newtype. **字段私有** (INV-010), 下游通过 [`TabId::new`]
/// (模块私有, [`TabRegistry::next_id`] 唯一调用方) / [`TabId::raw`] 访问.
///
/// **why u64**: 单调递增即使每 ms 开/关一个 tab 也能跑 ~6e8 年不溢出. wayland
/// 协议层无 tab id 概念 (本类型不出 wayland 边界, INV-010).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TabId(u64);

impl TabId {
    /// 模块私有构造. 测试用 [`TabId::for_test`] 构造任意值.
    fn new(raw: u64) -> Self {
        Self(raw)
    }

    /// 取出 raw u64 (供 trace 日志 / 调试用). **不**作为公共 hashable key.
    pub fn raw(self) -> u64 {
        self.0
    }

    /// 测试专用: 构造任意 id 值. 不在生产路径用 (生产走 [`TabRegistry`]).
    #[cfg(test)]
    #[allow(dead_code)]
    pub(crate) fn for_test(raw: u64) -> Self {
        Self(raw)
    }
}

/// 全局 tab id 分配器. 单调递增起 1, 不复用. atomic 是防御性 — 即使后续
/// 加多线程也安全; 当前 INV-005 单线程下 `Ordering::Relaxed` 即可.
struct NextTabId(AtomicU64);

impl NextTabId {
    const fn new() -> Self {
        Self(AtomicU64::new(1))
    }
    fn fetch_add(&self) -> u64 {
        self.0.fetch_add(1, Ordering::Relaxed)
    }
}

static NEXT_TAB_ID: NextTabId = NextTabId::new();

/// 注册器. 模块私有, 仅 [`TabRegistry::next_id`] 是唯一公共入口.
pub struct TabRegistry;

impl TabRegistry {
    /// 取下一个 tab id. 单调递增, 不复用.
    pub fn next_id() -> TabId {
        TabId::new(NEXT_TAB_ID.fetch_add())
    }

    /// 测试专用: 重置全局计数器 (各单测互不串)。`#[cfg(test)]` only.
    /// 真生产路径不会被调用 (atomic counter 跨整 quill 进程生命周期单调).
    #[cfg(test)]
    pub(crate) fn reset_for_test() {
        NEXT_TAB_ID.0.store(1, Ordering::Relaxed);
    }
}

/// 单 tab 实例. 持自己的 [`TermState`] (alacritty grid + cursor) + [`PtyHandle`]
/// (master fd + 子 shell).
///
/// **字段全私有** (INV-010): 下游通过 method 访问, 不直接读 term / pty 内部状态.
///
/// **drop 顺序 (INV-008 衍生)**: term → pty (term 不持 fd, drop 序无关; pty
/// 内部按 INV-008 reader → master → child drop). 字段顺序定义这条:
/// 1. `term` — 先 drop, alacritty grid 释放
/// 2. `pty` — master fd 关闭, slave 端读到 EOF + SIGHUP, 子 shell 退出
/// 3. POD 字段 (title / id / dirty) — 顺序无关
///
/// **所有权契约**: TabInstance 是 [`crate::wl::window::State`] 的字段, 整个 tab
/// 生命周期内由 State 持有; close 时由 [`TabList::remove`] 弹出 + drop.
pub struct TabInstance {
    /// alacritty 终端状态机. PTY 字节经 [`TermState::advance`] 喂进来.
    term: TermState,
    /// 子 shell 进程 + master fd. INV-009 master fd O_NONBLOCK.
    pty: PtyHandle,
    /// 当前显示标题. 默认 "shell" (PTY argv[0] basename) — Phase 后期接 OSC 0/2
    /// 自动更新需重写 [`crate::term::TermState`] 用 `Term<TitleListener>` (派单
    /// In #H 实装偏离: 当前 title 默认 "shell N", N=tab idx; **OSC title** 由
    /// 后续 ticket 接). 派单 In #J 接受 default title 测试覆盖.
    title: String,
    /// per-tab dirty 标记. 当前未单独读 (term.is_dirty() 已覆盖 cell 内容变化),
    /// 留作未来 inactive tab 累积重画跳过决策. 派单 In #B "per-tab dirty: 仅
    /// active tab dirty 触发渲染" 字面要求, 当前实装走 active tab term.is_dirty
    /// 等价路径.
    dirty: bool,
    /// 单调递增 id, 拖拽换序后仍可锁定 active tab.
    id: TabId,
}

impl TabInstance {
    /// 构造一个新 tab — spawn 子 shell + 建 alacritty Term + 取下一个 id.
    ///
    /// `cols` / `rows` 是初始 grid 尺寸 (与 PTY winsize 同步).
    /// `title` 默认走 "shell" (派单 In #H 实装偏离: OSC 0/2 自动更新延后到后续 ticket).
    pub fn spawn(cols: u16, rows: u16) -> Result<Self> {
        let pty = PtyHandle::spawn_shell(cols, rows)
            .context("PtyHandle::spawn_shell 失败 (新 tab 不可建)")?;
        let term = TermState::new(cols, rows);
        Ok(Self {
            term,
            pty,
            title: "shell".to_string(),
            dirty: true,
            id: TabRegistry::next_id(),
        })
    }

    /// 测试专用构造: 不 spawn 真子 shell, 给单测用 (e.g. swap reorder / close 邻近
    /// 选择不需要真 PTY). Production code 走 [`Self::spawn`].
    #[cfg(test)]
    pub(crate) fn for_test(title: &str) -> Self {
        let pty = PtyHandle::spawn_program("true", &[], 80, 24)
            .expect("test PtyHandle spawn 'true' 应成功 (CI 环境必备)");
        Self {
            term: TermState::new(80, 24),
            pty,
            title: title.to_string(),
            dirty: false,
            id: TabRegistry::next_id(),
        }
    }

    pub fn id(&self) -> TabId {
        self.id
    }

    pub fn title(&self) -> &str {
        &self.title
    }

    /// 设置 tab 标题 (例: 用户 rename / OSC 同步路径 — 后续 ticket).
    pub fn set_title(&mut self, title: String) {
        self.title = title;
    }

    pub fn term(&self) -> &TermState {
        &self.term
    }

    pub fn term_mut(&mut self) -> &mut TermState {
        &mut self.term
    }

    pub fn pty(&self) -> &PtyHandle {
        &self.pty
    }

    pub fn pty_mut(&mut self) -> &mut PtyHandle {
        &mut self.pty
    }

    pub fn dirty(&self) -> bool {
        self.dirty
    }

    /// 字段级 split borrow — 同时拿 &mut TermState + &mut PtyHandle.
    /// PTY callback 路径需 advance term + read pty 同时进行, 此方法是必需 (单纯
    /// `tab.term_mut() + tab.pty_mut()` 触发 E0499 双重借用).
    pub fn split_term_pty(&mut self) -> (&mut TermState, &mut PtyHandle) {
        (&mut self.term, &mut self.pty)
    }

    pub fn mark_dirty(&mut self) {
        self.dirty = true;
    }

    pub fn clear_dirty(&mut self) {
        self.dirty = false;
    }
}

/// 多 tab 集合 + active 索引. 抽 struct 而非 `Vec<TabInstance> + usize` 散落
/// 在 State 里, 防 active_tab 越界 / swap reorder 时漏更新 active 这类回归.
///
/// **不变式**:
/// - `tabs.is_empty() == false` 在 quill 跑期 (close 最后一个 tab → quit 整 quill).
/// - `active_tab < tabs.len()`.
/// - tab id 单调全局唯一.
pub struct TabList {
    tabs: Vec<TabInstance>,
    active: usize,
}

impl TabList {
    /// 起步: 单 tab 初始 quill 启动期一份 (与原 LoopData.term + state.pty single
    /// 等价). PTY 子 shell 已 spawn, fd 注册 token 由 [`crate::wl::window`] 路径
    /// 在 LoopData 级单独维护.
    pub fn new(initial: TabInstance) -> Self {
        Self {
            tabs: vec![initial],
            active: 0,
        }
    }

    pub fn len(&self) -> usize {
        self.tabs.len()
    }

    pub fn is_empty(&self) -> bool {
        self.tabs.is_empty()
    }

    pub fn active_idx(&self) -> usize {
        self.active
    }

    /// 切换 active tab. `idx >= len` 时不改 (防御性, 调用方应先校验).
    pub fn set_active(&mut self, idx: usize) -> bool {
        if idx >= self.tabs.len() {
            return false;
        }
        self.active = idx;
        true
    }

    /// 当前 active tab 引用. tabs 非空时永远 Some (不变式保证).
    pub fn active(&self) -> &TabInstance {
        &self.tabs[self.active]
    }

    pub fn active_mut(&mut self) -> &mut TabInstance {
        &mut self.tabs[self.active]
    }

    pub fn get(&self, idx: usize) -> Option<&TabInstance> {
        self.tabs.get(idx)
    }

    pub fn get_mut(&mut self, idx: usize) -> Option<&mut TabInstance> {
        self.tabs.get_mut(idx)
    }

    pub fn iter(&self) -> std::slice::Iter<'_, TabInstance> {
        self.tabs.iter()
    }

    pub fn iter_mut(&mut self) -> std::slice::IterMut<'_, TabInstance> {
        self.tabs.iter_mut()
    }

    /// 找 id 对应的 idx (拖拽换序时 anchor 锁定用).
    pub fn idx_of(&self, id: TabId) -> Option<usize> {
        self.tabs.iter().position(|t| t.id == id)
    }

    /// 追加新 tab 到末尾, 返回新 tab 的 idx + id.
    pub fn push(&mut self, tab: TabInstance) -> (usize, TabId) {
        let id = tab.id;
        self.tabs.push(tab);
        (self.tabs.len() - 1, id)
    }

    /// 关闭 idx 处 tab. 返回被移除的 [`TabInstance`] (调用方 drop 触发 PTY
    /// SIGHUP + fd close), 同时按 [`Self::neighbor_active_after_close`] 决策更新
    /// active idx. 若关掉最后一个 tab, 返 Some(removed) 但 tabs.is_empty() 后调
    /// 用方应触发 quill 退出.
    ///
    /// **idx 越界**: 返 None, active 不动.
    pub fn remove(&mut self, idx: usize) -> Option<TabInstance> {
        if idx >= self.tabs.len() {
            return None;
        }
        let removed = self.tabs.remove(idx);
        // active 调整决策 (派单 In #I)
        if self.tabs.is_empty() {
            self.active = 0;
        } else {
            self.active = neighbor_active_after_close(self.active, idx, self.tabs.len() + 1);
        }
        Some(removed)
    }

    /// 拖拽换序: tabs.swap(origin_idx, target_idx), active 跟拖中 tab id 同步
    /// (派单 In #F, 用 TabId 锁定 anchor).
    ///
    /// 越界 / 同 idx 不变 → 返 false. 成功 → 返 true.
    pub fn swap_reorder(&mut self, origin_idx: usize, target_idx: usize) -> bool {
        if origin_idx >= self.tabs.len() || target_idx >= self.tabs.len() {
            return false;
        }
        if origin_idx == target_idx {
            return false;
        }
        // 锁定 active tab id 以便换序后跟随
        let active_id = self.tabs[self.active].id;
        self.tabs.swap(origin_idx, target_idx);
        // 更新 active idx 以指向之前 active 的 tab id (拖中可能就是 active, 也可能不是)
        if let Some(new_active) = self.tabs.iter().position(|t| t.id == active_id) {
            self.active = new_active;
        }
        true
    }
}

/// 关闭 tab 后的 active 邻近选择 (派单 In #I).
///
/// 输入:
/// - `prev_active`: close 前的 active idx
/// - `closed_idx`: 被关的 tab idx
/// - `prev_len`: close 前的 tabs.len() (close 后 = `prev_len - 1`)
///
/// 决策:
/// - 关的不是 active → active 不变 (但若 closed_idx < prev_active 需 -1 平移)
/// - 关的就是 active → 选邻近: prev_active > 0 ? prev_active - 1 : 0
///   - 但 prev_active = 0 关掉后 active 应落到新的 0 (原 idx=1 上移)
///
/// 返回 close 后的新 active idx, **保证 < prev_len - 1** (剩余 tabs 范围内).
///
/// **why 抽纯 fn**: 单测覆盖 ≥3 case (close active idx>0 / close active idx==0 /
/// close non-active 影响 active 平移), 与 conventions §3 抽决策状态机套路一致.
pub fn neighbor_active_after_close(
    prev_active: usize,
    closed_idx: usize,
    prev_len: usize,
) -> usize {
    debug_assert!(prev_len >= 1);
    let new_len = prev_len.saturating_sub(1);
    if new_len == 0 {
        return 0; // tabs 全空 (调用方应 quit 整 quill, 派单 In #I)
    }
    if closed_idx == prev_active {
        // 关 active → 选邻近
        if prev_active > 0 {
            (prev_active - 1).min(new_len - 1)
        } else {
            0
        }
    } else if closed_idx < prev_active {
        // 关左侧 → active idx -1 (平移)
        prev_active.saturating_sub(1).min(new_len - 1)
    } else {
        // 关右侧 → active 不变 (但仍 clamp 防御)
        prev_active.min(new_len - 1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// neighbor_active_after_close: 关左侧 tab → active 平移 -1.
    #[test]
    fn neighbor_close_left_shifts_active_down() {
        // tabs [0,1,2,3], active=2. 关 idx=0 → tabs [1,2,3], active=1.
        assert_eq!(neighbor_active_after_close(2, 0, 4), 1);
    }

    /// 关右侧 tab → active 不变.
    #[test]
    fn neighbor_close_right_keeps_active() {
        // tabs [0,1,2,3], active=1. 关 idx=3 → tabs [0,1,2], active=1.
        assert_eq!(neighbor_active_after_close(1, 3, 4), 1);
    }

    /// 关 active idx>0 → active 邻近左 (idx-1).
    #[test]
    fn neighbor_close_active_idx_pos_picks_left() {
        // tabs [0,1,2,3], active=2. 关 idx=2 → tabs [0,1,3], active=1.
        assert_eq!(neighbor_active_after_close(2, 2, 4), 1);
    }

    /// 关 active idx==0 → 新 active=0 (原 1 上移到 0).
    #[test]
    fn neighbor_close_active_idx_zero_stays_zero() {
        // tabs [0,1,2], active=0. 关 idx=0 → tabs [1,2], active=0.
        assert_eq!(neighbor_active_after_close(0, 0, 3), 0);
    }

    /// 关最后一个 tab → 兜底 0 (调用方 quit 整 quill).
    #[test]
    fn neighbor_close_last_tab_returns_zero() {
        assert_eq!(neighbor_active_after_close(0, 0, 1), 0);
    }

    /// TabRegistry 单调递增 (重置后从 1 起).
    #[test]
    fn tab_registry_monotonic() {
        TabRegistry::reset_for_test();
        let a = TabRegistry::next_id();
        let b = TabRegistry::next_id();
        let c = TabRegistry::next_id();
        assert_eq!(a.raw(), 1);
        assert_eq!(b.raw(), 2);
        assert_eq!(c.raw(), 3);
        assert!(a < b && b < c);
    }

    /// TabInstance::for_test 给的 id 单调全局唯一 (跨实例).
    #[test]
    fn tab_instance_for_test_distinct_ids() {
        let a = TabInstance::for_test("a");
        let b = TabInstance::for_test("b");
        assert_ne!(a.id(), b.id(), "tab id 应全局唯一");
        // 收尸防止 zombie
        let _ = a;
        let _ = b;
    }

    /// TabList::push 正确返 idx + id.
    #[test]
    fn tab_list_push_returns_idx_and_id() {
        let initial = TabInstance::for_test("0");
        let initial_id = initial.id();
        let mut list = TabList::new(initial);
        let new_tab = TabInstance::for_test("1");
        let new_id = new_tab.id();
        let (idx, ret_id) = list.push(new_tab);
        assert_eq!(idx, 1);
        assert_eq!(ret_id, new_id);
        assert_eq!(list.len(), 2);
        assert_eq!(list.idx_of(initial_id), Some(0));
        assert_eq!(list.idx_of(new_id), Some(1));
    }

    /// TabList::set_active 越界返 false.
    #[test]
    fn tab_list_set_active_out_of_bounds_returns_false() {
        let mut list = TabList::new(TabInstance::for_test("0"));
        assert!(!list.set_active(5));
        assert_eq!(list.active_idx(), 0);
    }

    /// TabList::swap_reorder: active 跟随 tab id (派单 In #F, anchor=id).
    #[test]
    fn tab_list_swap_reorder_active_follows_id() {
        let mut list = TabList::new(TabInstance::for_test("0"));
        list.push(TabInstance::for_test("1"));
        list.push(TabInstance::for_test("2"));
        // active=2 (idx=2)
        assert!(list.set_active(2));
        let active_id = list.active().id();
        // swap idx=2 ↔ idx=0 → 原 active "2" 现在在 idx=0
        assert!(list.swap_reorder(2, 0));
        assert_eq!(list.active_idx(), 0);
        assert_eq!(list.active().id(), active_id);
    }

    /// TabList::remove: 关 active idx>0 → 邻近 idx-1.
    #[test]
    fn tab_list_remove_active_picks_neighbor() {
        let mut list = TabList::new(TabInstance::for_test("0"));
        list.push(TabInstance::for_test("1"));
        list.push(TabInstance::for_test("2"));
        list.set_active(2);
        let removed = list.remove(2);
        assert!(removed.is_some());
        assert_eq!(list.active_idx(), 1);
        assert_eq!(list.len(), 2);
    }

    /// TabList::remove: 关最后一个 → tabs 空, active=0 (调用方 quit).
    #[test]
    fn tab_list_remove_last_tab_empties_list() {
        let mut list = TabList::new(TabInstance::for_test("0"));
        let removed = list.remove(0);
        assert!(removed.is_some());
        assert!(list.is_empty());
    }

    /// TabList::remove 越界 → None, 不破坏 list.
    #[test]
    fn tab_list_remove_out_of_bounds_returns_none() {
        let mut list = TabList::new(TabInstance::for_test("0"));
        assert!(list.remove(99).is_none());
        assert_eq!(list.len(), 1);
    }

    /// TabList::set_active 后 active() 返回正确 tab.
    #[test]
    fn tab_list_active_returns_correct_tab() {
        let mut list = TabList::new(TabInstance::for_test("first"));
        list.push(TabInstance::for_test("second"));
        list.set_active(1);
        assert_eq!(list.active().title(), "second");
    }

    /// TabInstance::set_title 更新存值.
    #[test]
    fn tab_instance_set_title_updates() {
        let mut tab = TabInstance::for_test("orig");
        tab.set_title("new".to_string());
        assert_eq!(tab.title(), "new");
    }

    /// TabInstance::dirty 默认 false (test ctor), mark_dirty 后 true, clear 后 false.
    #[test]
    fn tab_instance_dirty_lifecycle() {
        let mut tab = TabInstance::for_test("a");
        assert!(!tab.dirty());
        tab.mark_dirty();
        assert!(tab.dirty());
        tab.clear_dirty();
        assert!(!tab.dirty());
    }
}
