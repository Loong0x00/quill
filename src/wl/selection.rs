//! T-0607 鼠标拖选 + 复制 / 粘贴的 quill 自有状态机 (无 wayland 协议类型).
//!
//! ## 模块边界 (INV-010 类型隔离)
//!
//! 本模块**完全不引** wayland-client / wayland-protocols / sctk 类型. 输入/输出
//! 全是 quill 自有 struct + enum + 标量 (`SelectionState` / `SelectionMode` /
//! `PasteSource` / `CellPos` / `f64`). 真协议路径
//! (`wp_primary_selection_v1` / `wl_data_device_manager`) 在 `wl/window.rs` 持
//! 协议 handle, 通过本模块的纯 fn 拿"该复制哪段文本"决策, 自己调发协议 request.
//!
//! ## 公开 API
//!
//! - [`SelectionMode`]: Linear (跨行流式) vs Block (Alt+drag 矩形).
//! - [`SelectionState`]: 当前选区状态 (anchor / cursor / mode / active).
//! - [`PasteSource`]: PRIMARY (中键) vs Clipboard (Ctrl+Shift+V).
//! - [`pixel_to_cell`]: 鼠标 logical px + display_offset → SelectionPos
//!   (考虑 titlebar y 偏移 + cell 像素常数 + viewport 当前向上滚行数, T-0804).
//! - [`modifier_to_selection_mode`]: bool (Alt 是否按住) → SelectionMode.
//! - [`selected_cells_linear`] / [`selected_cells_block`]: 给定 anchor / cursor
//!   (SelectionPos) + display_offset, 返当前 viewport 内可见 CellPos 序列.
//!   选区滚出 viewport 的部分 skip 不 emit (T-0804 视觉同步, 跨 history 复制
//!   走 T-0805).
//! - [`bracketed_paste_wrap`]: 给定原文 + 是否 bracketed paste 启用, 返字节
//!   (启用时包 `\x1b[200~ ... \x1b[201~`, 否则原样).
//! - [`extract_selection_text`]: 给定 SelectionState + row_text fn, 返完整选
//!   区文本字符串 (Linear / Block 走不同算法, Block 末尾空格 trim).
//!
//! ## 决策状态机 (conventions §3 抽决策模式)
//!
//! - `SelectionState::start(anchor, mode)` — 鼠标按下: 清旧选区 + 新 anchor.
//! - `SelectionState::update(cursor)` — 拖动: cursor 实时更新, anchor 不变.
//! - `SelectionState::end()` — 松开左键: active=false, anchor/cursor 保留 (用
//!   于后续 Ctrl+Shift+C 复制 / PRIMARY auto-copy 算文本).
//! - `SelectionState::clear()` — 显式清空 (新一次按下前不需要主动 clear, start
//!   自动清).
//!
//! ## 与 alacritty 边界
//!
//! 不复用 alacritty 内部 `Selection` struct (它持 alacritty Point 类型, 违反
//! INV-010). Block 选区拖动方向反向 (cursor 在 anchor 之上 / 之左) 走自己的
//! min/max clamp, 与 alacritty `Selection::Block` 同语义 (kitty / foot 同行为).

use crate::term::CellPos;

/// T-0804: viewport-relative grid position. `line=0` 是 viewport 顶,
/// `line=rows-1` 是底, `line<0` 是 scrollback history (越小越旧, `-display_offset`
/// 时正好落 viewport 第 0 行).
///
/// **why i32 line 而非 [`crate::term::ScrollbackPos`]**: ScrollbackPos.row 是
/// `usize` 仅覆盖 history 区 (0..history-1), 不能表 viewport 内行. 选区可跨
/// history + viewport, 用统一 i32 (negative=history, non-negative=viewport)
/// 是 alacritty / kitty / ghostty 工业共用方案 (详 ADR 0011).
///
/// **why quill 自己的 struct 而非 re-export alacritty `Point<Line=i32>`**:
/// INV-010 类型隔离 — alacritty 0.27/0.28 升级时类型可能动, quill 公共 API
/// 跟着抖. line+col 两基础类型, 升级面在 quill 边界单点.
///
/// 滚屏机制: viewport 顶 = `line=0` origin, viewport 滚动时 origin 不动, 内容
/// 通过 [`crate::term::TermState::display_offset`] 跟 origin 错位. 同一
/// SelectionPos 永远指向同一 cell; 渲染时通过 `viewport_line = selection.line +
/// display_offset` 反查算回当前 viewport 中位置 (落 0..rows 内才可见).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SelectionPos {
    /// viewport-relative 行号. `>=0` = viewport 第 N 行 (0=顶); `<0` = scrollback
    /// 第 |line| 行 (history, 越小越旧).
    pub line: i32,
    /// 列索引 (0=最左). cell-based, 与 [`CellPos::col`] 同语义.
    pub col: usize,
}

/// **选区模式**. Linear (alacritty / foot 默认) vs Block (Alt+drag, vim visual
/// block 风, 矩形).
///
/// **why enum 而非 bool**: 派单 In #B + #D 已规定两模式, 且 Phase 6+ 可能加
/// SemanticWord (双击选词, 派单 Out 但不排除将来) — exhaustive match 让加新
/// variant 编译期 catch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SelectionMode {
    /// 流式跨行选择. 起点行右半 + 中间整行 + 终点行左半 (anchor.line ≤ cursor.line),
    /// 反向同理 (anchor 在下时反过来).
    #[default]
    Linear,
    /// 矩形 (`min_col..=max_col`) × (`min_row..=max_row`), 不跨行流, 只取列范围.
    /// Alt+drag 触发. 复制时各行间 `\n` 连接, 行尾空格 **trim** (派单 In #G
    /// 已知陷阱: alacritty/kitty trim 行为分裂, 本项目选 trim — 用户期望"剪
    /// 表格列"行为).
    Block,
}

/// **粘贴来源**. 中键 → Primary (Linux 中键粘贴标准, X11 PRIMARY 同源);
/// Ctrl+Shift+V → Clipboard (跨应用复制粘贴标准).
///
/// 与 SelectionMode 同套路 (派单 In #C 强约束 quill 自有 enum).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PasteSource {
    /// `wp_primary_selection_v1` — 鼠标松开自动复制, 中键单击粘贴.
    Primary,
    /// `wl_data_device_manager` — Ctrl+Shift+C 复制选区, Ctrl+Shift+V 粘贴.
    Clipboard,
}

/// T-0607 鼠标拖选状态. 字段全私有, 走 `start` / `update` / `end` / `clear`
/// 入口转移. 与 [`crate::wl::pointer::PointerState`] 同模块隔离套路.
///
/// **T-0804: anchor / cursor 走 [`SelectionPos`] (viewport-relative i32 line)**.
/// viewport 滚屏 origin 不动, 同一 SelectionPos 永远指向同一 cell — 滚走的字
/// 落 line<0 (history), 蓝框视觉上消失但选区数据保留, 滚回 viewport 蓝框自动
/// 重现 (依赖 [`crate::term::TermState::display_offset`] 反查). 跨 history 部分
/// 数据保留供 T-0805 复制路径用, 当前 [`selected_cells_linear`] /
/// [`selected_cells_block`] 仅 emit viewport 内 cell.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SelectionState {
    /// 选区起点. 鼠标按下时记录, 拖动 + 滚屏期间不变 (origin 跟 viewport 顶绑死,
    /// 滚屏只动 display_offset 不动 anchor — T-0804 关键).
    anchor: SelectionPos,
    /// 选区终点. 拖动 / autoscroll 时实时更新; 松开后保留供后续复制.
    cursor: SelectionPos,
    /// 当前模式. 鼠标按下时由 modifier 决定 (Alt → Block 否则 Linear), 拖动
    /// 中不变 (alacritty / kitty 同行为, foot 也是).
    mode: SelectionMode,
    /// 鼠标是否仍按住. true = 拖动中 (cursor 跟随 motion); false = 已松开
    /// 但选区保留 (Ctrl+Shift+C / 中键 PRIMARY auto-copy 仍能拿到 anchor/cursor).
    active: bool,
    /// 选区是否非空. `false` = 默认 / `clear` 后 / 从未按下; `true` = `start`
    /// 后未 `clear`. 调用方据此决定渲染是否反色 + 是否走 PRIMARY auto-copy.
    has_selection: bool,
}

impl Default for SelectionState {
    fn default() -> Self {
        Self::new()
    }
}

impl SelectionState {
    /// 启动期建空状态 (无选区).
    pub const fn new() -> Self {
        Self {
            anchor: SelectionPos { col: 0, line: 0 },
            cursor: SelectionPos { col: 0, line: 0 },
            mode: SelectionMode::Linear,
            active: false,
            has_selection: false,
        }
    }

    /// 鼠标按下: 清旧选区 + 新 anchor + active=true. cursor 同步 anchor (单
    /// click 是零长度选区, 真长度由 update 拖出).
    pub fn start(&mut self, anchor: SelectionPos, mode: SelectionMode) {
        self.anchor = anchor;
        self.cursor = anchor;
        self.mode = mode;
        self.active = true;
        self.has_selection = true;
    }

    /// 拖动: 更新 cursor (anchor / mode 不变). 仅 active=true 时有效, 其它
    /// 情况静默忽略 (防协议事件 race — 例 button release 后又来 motion).
    pub fn update(&mut self, cursor: SelectionPos) {
        if !self.active {
            return;
        }
        self.cursor = cursor;
    }

    /// 松开左键: active=false. 选区保留供后续复制 (派单 In #A "anchor/cursor
    /// 保留 用于 Ctrl+Shift+C 后续复制").
    pub fn end(&mut self) {
        self.active = false;
    }

    /// 显式清空选区. 新一次 `start` 自动清, 但 PRIMARY auto-copy 失败 / 用户
    /// 切窗口需要主动清时调.
    pub fn clear(&mut self) {
        self.active = false;
        self.has_selection = false;
    }

    /// **T-0809 / ADR 0012**: PTY 输出致 alacritty grid 内部 ring-buffer 旋转
    /// `delta` 行 (顶 viewport 行进 scrollback) 后, **调整 anchor / cursor 让
    /// SelectionPos 钉字而非钉 cell**.
    ///
    /// 具体: `anchor.line -= delta`, `cursor.line -= delta`. 字进 scrollback,
    /// SelectionPos.line 跟着变负数, 渲染翻译 `viewport_line = sel_line +
    /// display_offset` 自然落到 history 区 (滚出 viewport 时该 cell skip 不绘,
    /// 数据保留, user 滚回时蓝框重现).
    ///
    /// **why 单点 mutation 而非 selected_cells_* 渲染补偿**: ADR 0012 §C 拒绝
    /// "改渲染公式" 路径 — selection.line 仍指 viewport 第 N 行, 字已不在那,
    /// 渲染层无论怎么算都还是 cell 而非字 (修不了 bug 1+2). 必须主动 mutate
    /// SelectionState 让 .line 字段重新对准 "原字现在在第几行".
    ///
    /// **bug 3 同源 (CC 后台输出 + user 看 history)**: 用户 display_offset=D 时
    /// 选了 history 一段, anchor.line = -k. CC 输出 N 行 → alacritty 内部
    /// `display_offset += N` 维持 user 视角锚定 (alacritty grid::scroll_up 路径).
    /// 不 rebase 时渲染 viewport_line = -k + (D+N) → 比原位置往下漂 N. rebase
    /// 后 anchor.line = -(k+N), viewport_line = -(k+N) + (D+N) = -k+D 不变 →
    /// 蓝框钉死. 三 bug 同根.
    ///
    /// **clamp 边界**: `delta > 0` 推 anchor / cursor 向负方向 (history). 字滚
    /// 出 scrollback 顶 (line < `-(history_size_max as i32)`) 时 clamp 到上限
    /// (= `-(history_size_max as i32)`), 让 user 仍能看到选区高亮上限边界标记
    /// — 比 silent clear 直观. `history_size_max` 由调用方传入, 当前 quill 走
    /// `TermState::new` 写死 `scrolling_history = 100_000` (term/mod.rs:596).
    ///
    /// **选区为空时 no-op**: `has_selection == false` 直接 return. 防止 PTY
    /// 输出空跑时偶发干扰 anchor / cursor 默认零值.
    ///
    /// **why `i32` delta**: 上游 `term.scrollback_size()` 返 `usize`, 调用方
    /// `as i32` 转 (cast 在调用方, 本 fn 接 i32 让 sign 显式). 调用方应保证
    /// `delta > 0` (`history_after - history_before`); `delta == 0` no-op,
    /// `delta < 0` 走防御 saturating_sub (history shrink 路径当前不存在 — 仅
    /// alacritty 内部 reset / clear 才减 history, 本 fn 不该被调).
    pub fn rebase_for_grid_scroll(&mut self, delta: i32, history_size_max: usize) {
        if !self.has_selection {
            return;
        }
        if delta <= 0 {
            return;
        }
        let min_line = -(history_size_max as i32);
        self.anchor.line = (self.anchor.line - delta).max(min_line);
        self.cursor.line = (self.cursor.line - delta).max(min_line);
    }

    /// 当前是否有选区可渲染 / 复制. `false` = 默认或 clear 后, `true` = start
    /// 之后 (即使 anchor==cursor 单 click).
    pub fn has_selection(&self) -> bool {
        self.has_selection
    }

    /// 当前是否在拖动中 (按住未松开). 调用方走 autoscroll 决策时读此判断 (仅
    /// active 才走边缘自动滚屏).
    pub fn is_active(&self) -> bool {
        self.active
    }

    /// 当前选区 anchor (鼠标按下点, viewport-relative i32 line).
    pub fn anchor(&self) -> SelectionPos {
        self.anchor
    }

    /// 当前选区 cursor (拖动到的点, viewport-relative i32 line).
    pub fn cursor(&self) -> SelectionPos {
        self.cursor
    }

    /// 当前选区模式.
    pub fn mode(&self) -> SelectionMode {
        self.mode
    }
}

/// T-0607 + T-0804: 鼠标 logical px → viewport-relative [`SelectionPos`].
///
/// **T-0804 新增 `display_offset` 参数**: scrollback 当前向上滚行数 (来自
/// [`crate::term::TermState::display_offset`]). 鼠标点 viewport 第 N 行 (0..rows)
/// 在底层选区坐标里其实是 `N - display_offset` 行 — display_offset=0 (无滚动)
/// 时跟旧 viewport-relative 等价, display_offset>0 时返负 line (history). 选区
/// 数据存负 line 让滚屏期间 anchor/cursor 永远指向同一 cell, 视觉同步靠
/// [`selected_cells_linear`] / [`selected_cells_block`] 反查 display_offset.
///
/// **why top_reserved 偏移**: cells 区从 `y >= top_reserved` 起绘. 单 tab 时
/// `top_reserved = TITLEBAR_H_LOGICAL_PX` (T-0504), 多 tab 时
/// `top_reserved = TITLEBAR_H + TAB_BAR_H` (T-0608/T-0617). 鼠标在 titlebar /
/// tab bar 区不该映射到 cell (派单"鼠标在 viewport 之外不算选"). 走
/// `Option<SelectionPos>` 让调用方判 None 直接早返.
///
/// **why 改名 titlebar_h_logical → top_reserved_h_logical** (T-0608 hotfix
/// 2026-04-29): 多 tab 时 cells 区起点是 titlebar + tab_bar, 调用方应传整个
/// `top_reserved` 给本函数. 老签名只传 titlebar_h 漏 tab_bar_h, 多 tab 时鼠标
/// 视觉位置与算出的 cell row 差 1 行 (复现: 开 2 tab 后 hover/click 错位, 关
/// 回 1 tab 后无错位). render 路径用 `titlebar_y_offset_px = (titlebar_h +
/// tab_bar_h) * HIDPI`, hit_test 路径必须同 origin 才能视觉与逻辑同步 (与
/// `cells_from_surface_px` / `tab_bar_h_logical_for` 同 invariants 套路).
///
/// **why 接 logical px (而非 physical)**: wl_pointer 协议给的坐标本就是 logical
/// (与 surface 尺寸 logical 同源, 见 [`crate::wl::pointer::PointerState`] 字段
/// 注释), HIDPI scale 仅影响 wgpu surface backing buffer.
///
/// 越界 (`col >= cols` / `row >= rows`) 走 saturating clamp 到 `cols-1 / rows-1`
/// (用户拖到右下角时 cursor 应在最后 cell 而非 None — 派单"滑到哪选到哪").
/// 不接 (x, y) 在 surface 外 (负坐标 / 大于 surface_w) 的 case (调用方
/// `apply_motion` 已记 pos, 真负坐标 compositor 不会发).
#[allow(clippy::too_many_arguments)] // 8 个参数皆为 hit-test 必需 (px / 网格几何 / scrollback offset), 拆 struct 反增 callsite 噪声.
pub fn pixel_to_cell(
    x: f64,
    y: f64,
    cols: usize,
    rows: usize,
    cell_w_logical: f64,
    cell_h_logical: f64,
    top_reserved_h_logical: f64,
    display_offset: usize,
) -> Option<SelectionPos> {
    if y < top_reserved_h_logical {
        return None;
    }
    if x < 0.0 {
        return None;
    }
    if cols == 0 || rows == 0 {
        return None;
    }
    let usable_y = y - top_reserved_h_logical;
    let col_f = (x / cell_w_logical).floor() as i64;
    let row_f = (usable_y / cell_h_logical).floor() as i64;
    let col = col_f.max(0) as usize;
    let viewport_row = (row_f.max(0) as usize).min(rows - 1);
    // T-0804: viewport 第 N 行 → 选区坐标 N - display_offset. display_offset=0
    // 时跟旧 viewport-relative 行号等价; >0 时落负 line 进 history 区.
    let selection_line = viewport_row as i32 - display_offset as i32;
    Some(SelectionPos {
        col: col.min(cols - 1),
        line: selection_line,
    })
}

/// T-0607: Alt 是否按住 → [`SelectionMode`]. Alt+drag → Block (vim visual block
/// 风, kitty `mouse_map button1+alt rectangular_select` 同语义); 否则 Linear
/// (alacritty / foot 默认).
pub fn modifier_to_selection_mode(alt_active: bool) -> SelectionMode {
    if alt_active {
        SelectionMode::Block
    } else {
        SelectionMode::Linear
    }
}

/// T-0607 + T-0804: 给定 SelectionState + cols/rows + display_offset, 返当前
/// viewport 内可见 [`CellPos`] 序列 (Linear 模式).
///
/// **行内分段** (派单 In #D):
/// - anchor.line < cursor.line: 起点行 `[anchor.col..cols)` + 中间整行
///   `[0..cols)` + 终点行 `[0..=cursor.col]`
/// - anchor.line == cursor.line: 单行段 `[min_col..=max_col]`
/// - anchor.line > cursor.line: 反向 (cursor 在 anchor 之上) — 把 (anchor, cursor)
///   交换走正向算法
///
/// **T-0804 viewport 反查**: SelectionPos.line 是 viewport-relative i32 (negative
/// = history). emit 时算 `viewport_line = sel_line + display_offset`, 仅
/// `0 <= viewport_line < rows` 的 cell 才 push (其它 line 滚出 viewport, 数据
/// 仍在 SelectionState 但不可见). CellPos.line 是 `0..rows` viewport 内行号
/// 供渲染层直接索引. 跨 history 部分的复制走另开 ticket T-0805.
///
/// **why `Vec<CellPos>` 而非 `impl Iterator`**: 选区典型 < 1000 cell (终端 80×24
/// = 1920 满屏, 实际拖选远少); Vec 分配开销 << 渲染或文本提取开销, KISS 胜过
/// 聪明 (与 `cells_iter().collect::<Vec<_>>` 派单 In #D 同决策, 单测易写).
pub fn selected_cells_linear(
    state: &SelectionState,
    cols: usize,
    rows: usize,
    display_offset: usize,
) -> Vec<CellPos> {
    if !state.has_selection || cols == 0 || rows == 0 {
        return Vec::new();
    }
    // anchor 在 cursor 之上时正向, 之下时交换走同算法.
    let (start, end) =
        if (state.anchor.line, state.anchor.col) <= (state.cursor.line, state.cursor.col) {
            (state.anchor, state.cursor)
        } else {
            (state.cursor, state.anchor)
        };

    let mut out = Vec::new();
    let push_if_visible = |out: &mut Vec<CellPos>, sel_line: i32, col: usize| {
        if let Some(viewport_line) = viewport_line_of(sel_line, rows, display_offset) {
            out.push(CellPos {
                col,
                line: viewport_line,
            });
        }
    };
    if start.line == end.line {
        for c in start.col..=end.col.min(cols.saturating_sub(1)) {
            push_if_visible(&mut out, start.line, c);
        }
        return out;
    }
    // 起点行: start.col..cols
    for c in start.col..cols {
        push_if_visible(&mut out, start.line, c);
    }
    // 中间整行: (start.line+1..end.line) × (0..cols)
    for line in (start.line + 1)..end.line {
        for c in 0..cols {
            push_if_visible(&mut out, line, c);
        }
    }
    // 终点行: 0..=end.col
    for c in 0..=end.col.min(cols.saturating_sub(1)) {
        push_if_visible(&mut out, end.line, c);
    }
    out
}

/// T-0607 + T-0804: Block 模式选区 cell 集合. (`min_row..=max_row`) ×
/// (`min_col..=max_col`) 矩形, 不跨行流. 跟 linear 同, 仅 emit viewport 内
/// 可见 cell, 滚出 viewport 部分 skip (data 保留在 SelectionState).
pub fn selected_cells_block(
    state: &SelectionState,
    cols: usize,
    rows: usize,
    display_offset: usize,
) -> Vec<CellPos> {
    if !state.has_selection || cols == 0 || rows == 0 {
        return Vec::new();
    }
    let min_col = state.anchor.col.min(state.cursor.col);
    let max_col = state
        .anchor
        .col
        .max(state.cursor.col)
        .min(cols.saturating_sub(1));
    let min_row = state.anchor.line.min(state.cursor.line);
    let max_row = state.anchor.line.max(state.cursor.line);
    let mut out = Vec::new();
    for line in min_row..=max_row {
        let Some(viewport_line) = viewport_line_of(line, rows, display_offset) else {
            continue;
        };
        for c in min_col..=max_col {
            out.push(CellPos {
                col: c,
                line: viewport_line,
            });
        }
    }
    out
}

/// T-0804: SelectionPos.line (viewport-relative i32) → 当前 viewport 行号
/// (`0..rows`), 落 viewport 外返 None. `viewport_line = sel_line + display_offset`,
/// display_offset=0 时跟 sel_line 等价 (只 sel_line>=0 的 viewport 区).
fn viewport_line_of(sel_line: i32, rows: usize, display_offset: usize) -> Option<usize> {
    let viewport_line = sel_line.checked_add(display_offset as i32)?;
    if viewport_line < 0 {
        return None;
    }
    let viewport_line = viewport_line as usize;
    if viewport_line >= rows {
        return None;
    }
    Some(viewport_line)
}

/// T-0607 + T-0804 + T-0805: 给定 SelectionState + row_text 取行函数, 拼出
/// **完整选区文本** (跨 history + viewport 同源).
///
/// `row_text(sel_line)` 接 [`SelectionPos`] 同款 viewport-relative `i32` 行号:
/// - `>= 0` 表 viewport-absolute 行 (origin = display_offset=0 时 viewport 顶),
///   调用方应:
///   1. 算 `viewport_line = line + display_offset` (T-0805 hotfix e12f276 必需,
///      否则 user 滚屏时复制拿到的是当前显示内容而非 selection 端点真实内容)
///   2. 返 [`crate::term::TermState::display_text_with_spacers`]`(viewport_line)`
/// - `< 0` 表 history 行 (越小越旧, `-1` 是 viewport 上方第 1 行), 调用方应转
///   `ScrollbackPos` 后返 [`crate::term::TermState::scrollback_line_text_with_spacers`]
///   (T-0807: 镜像 viewport 路径的 `\0` spacer 协议, 让外层 `replace('\0', "")`
///   行为对 viewport 与 history 路径一致, 复制 CJK 行不再夹空格)
/// - 越界 (滚出 history 顶 / viewport 底) 返 `String::new()`, 本模块按空行处理
///
/// **why 不接 ScrollbackPos**: INV-010 类型隔离 — selection 模块永不接触
/// alacritty 坐标 / quill term 类型. 转换在调用方 (`wl/window.rs`) inline.
///
/// `display_offset` 仅用于 history 部分整体滚出底外 (`end.line + off < 0`)
/// 的早退优化 — 顶外 (`start.line + off >= rows`) 不可能 (用户没法选 viewport
/// 下方还没渲染的内容). 删 clamp 路径后, 这里基本是空运行, 留参数防 caller
/// 接口抖动.
///
/// **Linear**: 首行 `[start.col..]` + 中间整行 + 末行 `[..=end.col]`. anchor /
/// cursor 真实 col 完整保留 (T-0805 修 T-0804 fallback "首可见行 col=0" 误导).
///
/// **Block** (派单 In #G "末尾空格 trim — 用户期望剪表格列"):
/// - 每行 `[min_col..=max_col]` substr, **trim_end** (空格/tab 都剪) + `\n` join.
///
/// `row_text` 调用次数 ≤ 选区行数 (典型 < 50 行); String 分配开销 << 复制/粘贴
/// I/O. 派单 KISS 接受.
///
/// **char vs byte index**: 走 `chars()` 截断 — 终端 grid 一格 = 一 char (CJK
/// 占 2 cell 但 spacer 已被 display_text 跳过, char count 与 cell count 对齐).
/// 多字节 UTF-8 (中文 3-byte / emoji 4-byte) 走 byte index 会切坏字符.
pub fn extract_selection_text<F>(
    state: &SelectionState,
    cols: usize,
    rows: usize,
    display_offset: usize,
    mut row_text: F,
) -> String
where
    F: FnMut(i32) -> String,
{
    if !state.has_selection || cols == 0 || rows == 0 {
        return String::new();
    }
    match state.mode {
        SelectionMode::Linear => extract_linear(state, cols, rows, display_offset, &mut row_text),
        SelectionMode::Block => extract_block(state, cols, rows, display_offset, &mut row_text),
    }
}

fn extract_linear<F>(
    state: &SelectionState,
    cols: usize,
    rows: usize,
    display_offset: usize,
    row_text: &mut F,
) -> String
where
    F: FnMut(i32) -> String,
{
    let _ = (cols, rows, display_offset);
    let (start, end) =
        if (state.anchor.line, state.anchor.col) <= (state.cursor.line, state.cursor.col) {
            (state.anchor, state.cursor)
        } else {
            (state.cursor, state.anchor)
        };

    let mut out = String::new();
    if start.line == end.line {
        let txt = row_text(start.line);
        out.push_str(&substr_chars(&txt, start.col, end.col + 1));
        return out;
    }
    // 首行: [start.col..]
    let first = row_text(start.line);
    out.push_str(&substr_chars(&first, start.col, usize::MAX));
    out.push('\n');
    // 中间整行
    let mut line = start.line + 1;
    while line < end.line {
        out.push_str(&row_text(line));
        out.push('\n');
        line += 1;
    }
    // 末行: [..=end.col]
    let last = row_text(end.line);
    out.push_str(&substr_chars(&last, 0, end.col + 1));
    out
}

fn extract_block<F>(
    state: &SelectionState,
    cols: usize,
    rows: usize,
    display_offset: usize,
    row_text: &mut F,
) -> String
where
    F: FnMut(i32) -> String,
{
    let _ = (cols, rows, display_offset);
    let min_col = state.anchor.col.min(state.cursor.col);
    let max_col = state.anchor.col.max(state.cursor.col);
    let min_row = state.anchor.line.min(state.cursor.line);
    let max_row = state.anchor.line.max(state.cursor.line);
    let mut lines: Vec<String> = Vec::new();
    let mut line = min_row;
    while line <= max_row {
        let txt = row_text(line);
        let seg = substr_chars(&txt, min_col, max_col + 1);
        // 派单 In #G "Block 末尾空格 trim — 用户期望剪表格列".
        let trimmed = seg.trim_end_matches([' ', '\t']).to_string();
        lines.push(trimmed);
        line += 1;
    }
    lines.join("\n")
}

/// T-0607: 走 `chars()` 截断字符串到指定 char 范围 (`[start_char..end_char)`).
///
/// **why char 不是 byte**: UTF-8 多字节字符 (中文 3 byte / emoji 4 byte) 走
/// byte slice 会切坏. 终端 grid 一格 = 一 char 语义 (CJK 占 2 cell 但
/// `display_text` 跳过 spacer, 实字 1 char), 与 `chars()` count 对齐.
fn substr_chars(s: &str, start_char: usize, end_char: usize) -> String {
    s.chars()
        .skip(start_char)
        .take(end_char.saturating_sub(start_char))
        .collect()
}

/// T-0607: bracketed paste 包装. `enabled=true` 时返 `\x1b[200~ <text> \x1b[201~`,
/// 否则原文 bytes.
///
/// shell (bash readline / zsh) 启动期发 DECSET 2004 后启 bracketed paste; 调用
/// 方走 [`crate::term::TermState::is_bracketed_paste`] 判 enabled, 然后调本 fn
/// 包字节再 `pty.write`. shell 读到 `\x1b[200~` 知道接下来是粘贴而非真键入,
/// 多行粘贴时不会每行 Enter 立即执行 (避免误执行剪贴板 bash 命令).
///
/// **dirty pty 字节裸保留**: 粘贴文本若已含 `\x1b[201~` (恶意 / 凑巧) 会过早
/// 终止 paste, shell 把后续部分当真键入. 派单接受此 risk (alacritty / foot
/// 同等不过滤; 真防御走 shell `bind 'set enable-bracketed-paste off'` 关).
pub fn bracketed_paste_wrap(text: &str, enabled: bool) -> Vec<u8> {
    if !enabled {
        return text.as_bytes().to_vec();
    }
    let mut out = Vec::with_capacity(text.len() + 12);
    out.extend_from_slice(b"\x1b[200~");
    out.extend_from_slice(text.as_bytes());
    out.extend_from_slice(b"\x1b[201~");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// SelectionPos helper. T-0804: viewport-relative i32 line, 测试用 usize
    /// line 隐含 `display_offset=0` 场景 (line=N 即 viewport 第 N 行).
    fn sp(col: usize, line: i32) -> SelectionPos {
        SelectionPos { col, line }
    }

    /// CellPos helper. selected_cells_* 输出 viewport-relative CellPos, 比对用.
    fn cp(col: usize, line: usize) -> CellPos {
        CellPos { col, line }
    }

    // ---- SelectionState 转移 ----

    #[test]
    fn fresh_state_has_no_selection_and_inactive() {
        let s = SelectionState::new();
        assert!(!s.has_selection());
        assert!(!s.is_active());
        assert_eq!(s.mode(), SelectionMode::Linear);
    }

    #[test]
    fn start_records_anchor_and_activates() {
        let mut s = SelectionState::new();
        s.start(sp(5, 3), SelectionMode::Linear);
        assert!(s.has_selection());
        assert!(s.is_active());
        assert_eq!(s.anchor(), sp(5, 3));
        assert_eq!(s.cursor(), sp(5, 3), "cursor 起步 = anchor (零长度选区)");
        assert_eq!(s.mode(), SelectionMode::Linear);
    }

    #[test]
    fn update_advances_cursor_keeping_anchor() {
        let mut s = SelectionState::new();
        s.start(sp(5, 3), SelectionMode::Linear);
        s.update(sp(20, 8));
        assert_eq!(s.anchor(), sp(5, 3));
        assert_eq!(s.cursor(), sp(20, 8));
        assert!(s.is_active());
    }

    #[test]
    fn update_when_inactive_is_ignored() {
        let mut s = SelectionState::new();
        // 没 start 直接 update — race 防御
        s.update(sp(20, 8));
        assert_eq!(s.cursor(), sp(0, 0), "未 start 时 update 应静默忽略");
    }

    #[test]
    fn end_keeps_selection_for_later_copy() {
        let mut s = SelectionState::new();
        s.start(sp(5, 3), SelectionMode::Linear);
        s.update(sp(20, 8));
        s.end();
        assert!(!s.is_active(), "松开后 active=false");
        assert!(s.has_selection(), "选区保留给 Ctrl+Shift+C");
        assert_eq!(s.anchor(), sp(5, 3));
        assert_eq!(s.cursor(), sp(20, 8));
    }

    #[test]
    fn clear_removes_selection() {
        let mut s = SelectionState::new();
        s.start(sp(5, 3), SelectionMode::Linear);
        s.clear();
        assert!(!s.has_selection());
        assert!(!s.is_active());
    }

    #[test]
    fn new_start_clears_old_selection() {
        let mut s = SelectionState::new();
        s.start(sp(5, 3), SelectionMode::Linear);
        s.update(sp(20, 8));
        s.start(sp(40, 1), SelectionMode::Block);
        assert_eq!(s.anchor(), sp(40, 1));
        assert_eq!(s.cursor(), sp(40, 1), "新 start 重置 cursor");
        assert_eq!(s.mode(), SelectionMode::Block);
    }

    // ---- selected_cells_linear (display_offset=0 / 旧 viewport 等价) ----

    #[test]
    fn linear_single_line_forward() {
        let mut s = SelectionState::new();
        s.start(sp(5, 3), SelectionMode::Linear);
        s.update(sp(10, 3));
        let cells = selected_cells_linear(&s, 80, 24, 0);
        assert_eq!(cells.len(), 6); // [5, 6, 7, 8, 9, 10]
        assert_eq!(cells.first(), Some(&cp(5, 3)));
        assert_eq!(cells.last(), Some(&cp(10, 3)));
    }

    #[test]
    fn linear_two_lines_forward() {
        let mut s = SelectionState::new();
        s.start(sp(70, 1), SelectionMode::Linear);
        s.update(sp(10, 2));
        let cells = selected_cells_linear(&s, 80, 24, 0);
        // 行 1: [70..80) = 10 cells
        // 行 2: [0..=10] = 11 cells
        assert_eq!(cells.len(), 21);
        assert_eq!(cells[0], cp(70, 1));
        assert_eq!(cells[9], cp(79, 1));
        assert_eq!(cells[10], cp(0, 2));
        assert_eq!(cells[20], cp(10, 2));
    }

    #[test]
    fn linear_three_lines_with_full_middle() {
        let mut s = SelectionState::new();
        s.start(sp(75, 0), SelectionMode::Linear);
        s.update(sp(5, 2));
        let cells = selected_cells_linear(&s, 80, 24, 0);
        // 行 0: [75..80) = 5
        // 行 1: [0..80) = 80
        // 行 2: [0..=5] = 6
        assert_eq!(cells.len(), 5 + 80 + 6);
    }

    #[test]
    fn linear_anchor_below_cursor_is_swapped() {
        let mut s = SelectionState::new();
        s.start(sp(10, 5), SelectionMode::Linear);
        s.update(sp(70, 1)); // cursor 在 anchor 之上
        let cells = selected_cells_linear(&s, 80, 24, 0);
        // 等价 anchor=(70,1) cursor=(10,5):
        // 行 1: [70..80) = 10, 行 2/3/4: 80×3 = 240, 行 5: [0..=10] = 11
        assert_eq!(cells.len(), 10 + 80 * 3 + 11);
        assert_eq!(cells[0], cp(70, 1));
    }

    #[test]
    fn linear_no_selection_returns_empty() {
        let s = SelectionState::new();
        assert!(selected_cells_linear(&s, 80, 24, 0).is_empty());
    }

    // ---- selected_cells_block (display_offset=0) ----

    #[test]
    fn block_one_by_one() {
        let mut s = SelectionState::new();
        s.start(sp(5, 3), SelectionMode::Block);
        let cells = selected_cells_block(&s, 80, 24, 0);
        assert_eq!(cells.len(), 1);
        assert_eq!(cells[0], cp(5, 3));
    }

    #[test]
    fn block_n_by_m() {
        let mut s = SelectionState::new();
        s.start(sp(5, 2), SelectionMode::Block);
        s.update(sp(8, 4));
        let cells = selected_cells_block(&s, 80, 24, 0);
        // 列 5..=8 (4 cols) × 行 2..=4 (3 rows) = 12 cells
        assert_eq!(cells.len(), 12);
        assert!(cells.contains(&cp(5, 2)));
        assert!(cells.contains(&cp(8, 4)));
    }

    #[test]
    fn block_negative_direction_swaps() {
        let mut s = SelectionState::new();
        s.start(sp(8, 4), SelectionMode::Block);
        s.update(sp(5, 2)); // cursor 在 anchor 之上 + 之左
        let cells = selected_cells_block(&s, 80, 24, 0);
        // min_col=5, max_col=8, min_row=2, max_row=4 — 同正向
        assert_eq!(cells.len(), 12);
        assert!(cells.contains(&cp(5, 2)));
        assert!(cells.contains(&cp(8, 4)));
    }

    // ---- pixel_to_cell (display_offset=0 / 旧 viewport 等价) ----

    #[test]
    fn pixel_to_cell_in_text_area() {
        // 800×600 surface, cell 10×25 logical, titlebar 28
        // x=100 y=53 (53 - 28 = 25 → row 1) → col 10 row 1
        assert_eq!(
            pixel_to_cell(100.0, 53.0, 80, 22, 10.0, 25.0, 28.0, 0),
            Some(sp(10, 1))
        );
    }

    #[test]
    fn pixel_to_cell_at_titlebar_boundary() {
        // y=27.9 < titlebar (28) → None
        assert_eq!(
            pixel_to_cell(100.0, 27.9, 80, 22, 10.0, 25.0, 28.0, 0),
            None
        );
        // y=28 (== titlebar) → row 0
        assert_eq!(
            pixel_to_cell(100.0, 28.0, 80, 22, 10.0, 25.0, 28.0, 0),
            Some(sp(10, 0))
        );
    }

    #[test]
    fn pixel_to_cell_clamps_to_last_cell() {
        // 大于 cols × cell_w_logical (80×10=800) → col=79
        assert_eq!(
            pixel_to_cell(900.0, 100.0, 80, 22, 10.0, 25.0, 28.0, 0),
            Some(sp(79, ((100.0 - 28.0) / 25.0) as i32))
        );
    }

    #[test]
    fn pixel_to_cell_negative_x_returns_none() {
        // 协议防御
        assert_eq!(
            pixel_to_cell(-1.0, 100.0, 80, 22, 10.0, 25.0, 28.0, 0),
            None
        );
    }

    #[test]
    fn pixel_to_cell_zero_dimensions_returns_none() {
        assert_eq!(
            pixel_to_cell(100.0, 100.0, 0, 22, 10.0, 25.0, 28.0, 0),
            None
        );
        assert_eq!(
            pixel_to_cell(100.0, 100.0, 80, 0, 10.0, 25.0, 28.0, 0),
            None
        );
    }

    /// T-0608 hotfix 2026-04-29: 多 tab 时 top_reserved = titlebar + tab_bar.
    /// 复现 bug: 用旧值 28 (仅 titlebar) 时 y=53 算出 row 1, 但 render 路径
    /// 已让 cells 起点偏到 56 (28+28), 视觉上 y=53 还在 tab bar 区不该映射 cell.
    /// 修后传 56 (top_reserved): y=53 < 56 → None (鼠标在 tab bar 区不算选).
    #[test]
    fn pixel_to_cell_with_tab_bar_offset() {
        // 多 tab 场景: titlebar 28 + tab_bar 28 = top_reserved 56.
        // y=55.9 落 tab bar 区 → None.
        assert_eq!(
            pixel_to_cell(100.0, 55.9, 80, 22, 10.0, 25.0, 56.0, 0),
            None
        );
        // y=56 (== top_reserved) → row 0.
        assert_eq!(
            pixel_to_cell(100.0, 56.0, 80, 22, 10.0, 25.0, 56.0, 0),
            Some(sp(10, 0))
        );
        // y=81 (= 56 + 25) → row 1, 与单 tab y=53 (= 28 + 25) row 1 同 row idx.
        // 验证: render origin 与 hit_test origin 同步, row idx 对相同 cell 一致.
        assert_eq!(
            pixel_to_cell(100.0, 81.0, 80, 22, 10.0, 25.0, 56.0, 0),
            Some(sp(10, 1))
        );
    }

    /// T-0804: display_offset>0 时 viewport 第 0 行其实是 history 第 -display_offset 行.
    /// y=titlebar 命中 viewport row 0, display_offset=3 → SelectionPos.line == -3.
    #[test]
    fn pixel_to_cell_with_display_offset() {
        // viewport 第 0 行 (y=28 == titlebar), display_offset=3 → line=-3.
        assert_eq!(
            pixel_to_cell(100.0, 28.0, 80, 22, 10.0, 25.0, 28.0, 3),
            Some(sp(10, -3))
        );
        // viewport 第 5 行 (y=28+5*25=153), display_offset=3 → line=2.
        assert_eq!(
            pixel_to_cell(100.0, 153.0, 80, 22, 10.0, 25.0, 28.0, 3),
            Some(sp(10, 2))
        );
        // 大滚屏: display_offset=100, viewport 第 0 行 → line=-100.
        assert_eq!(
            pixel_to_cell(100.0, 28.0, 80, 22, 10.0, 25.0, 28.0, 100),
            Some(sp(10, -100))
        );
    }

    // ---- modifier_to_selection_mode ----

    #[test]
    fn modifier_alt_active_gives_block() {
        assert_eq!(modifier_to_selection_mode(true), SelectionMode::Block);
    }

    #[test]
    fn modifier_no_alt_gives_linear() {
        assert_eq!(modifier_to_selection_mode(false), SelectionMode::Linear);
    }

    // ---- bracketed_paste_wrap ----

    #[test]
    fn bracketed_paste_wraps_when_enabled() {
        let out = bracketed_paste_wrap("hello", true);
        assert_eq!(out, b"\x1b[200~hello\x1b[201~");
    }

    #[test]
    fn bracketed_paste_passes_through_when_disabled() {
        let out = bracketed_paste_wrap("hello", false);
        assert_eq!(out, b"hello");
    }

    #[test]
    fn bracketed_paste_handles_empty_text() {
        assert_eq!(bracketed_paste_wrap("", true), b"\x1b[200~\x1b[201~");
        assert_eq!(bracketed_paste_wrap("", false), b"");
    }

    #[test]
    fn bracketed_paste_handles_multiline_text() {
        let out = bracketed_paste_wrap("a\nb", true);
        // 包含换行不破坏包装语义 (shell 接收 \x1b[200~..\x1b[201~ 作整体粘贴)
        assert_eq!(out, b"\x1b[200~a\nb\x1b[201~");
    }

    // ---- extract_selection_text (display_offset=0 / 旧 viewport 等价) ----

    #[test]
    fn extract_linear_single_line() {
        let mut s = SelectionState::new();
        s.start(sp(0, 0), SelectionMode::Linear);
        s.update(sp(4, 0));
        let row_text = |line: i32| -> String {
            match line {
                0 => "hello world".into(),
                _ => String::new(),
            }
        };
        assert_eq!(extract_selection_text(&s, 80, 24, 0, row_text), "hello");
    }

    #[test]
    fn extract_linear_multi_line_concat_with_newlines() {
        let mut s = SelectionState::new();
        s.start(sp(6, 0), SelectionMode::Linear);
        s.update(sp(4, 2));
        let row_text = |line: i32| -> String {
            match line {
                0 => "hello world".into(),
                1 => "second line".into(),
                2 => "third line".into(),
                _ => String::new(),
            }
        };
        // 行 0: "world" (chars 6..)
        // 行 1: "second line" (整行)
        // 行 2: "third" (chars 0..=4)
        let out = extract_selection_text(&s, 80, 24, 0, row_text);
        assert_eq!(out, "world\nsecond line\nthird");
    }

    #[test]
    fn extract_block_trims_trailing_spaces() {
        let mut s = SelectionState::new();
        s.start(sp(0, 0), SelectionMode::Block);
        s.update(sp(9, 1));
        let row_text = |line: i32| -> String {
            match line {
                0 => "abc       ".into(), // 行 0: "abc" + 7 空格
                1 => "xyz123    ".into(), // 行 1: "xyz123" + 4 空格
                _ => String::new(),
            }
        };
        // Block 0..=9 × 0..=1, 末尾空格 trim
        assert_eq!(
            extract_selection_text(&s, 80, 24, 0, row_text),
            "abc\nxyz123"
        );
    }

    #[test]
    fn extract_handles_utf8_multibyte() {
        // CJK 字符走 chars().count() 而非 byte index — substr 不切坏中文
        let mut s = SelectionState::new();
        s.start(sp(0, 0), SelectionMode::Linear);
        s.update(sp(2, 0));
        let row_text = |line: i32| -> String {
            match line {
                0 => "中文测试".into(),
                _ => String::new(),
            }
        };
        // chars 0..=2 = "中文测"
        assert_eq!(extract_selection_text(&s, 80, 24, 0, row_text), "中文测");
    }

    #[test]
    fn extract_no_selection_returns_empty() {
        let s = SelectionState::new();
        let row_text = |_: i32| String::new();
        assert!(extract_selection_text(&s, 80, 24, 0, row_text).is_empty());
    }

    // ---- T-0804 新增: 选区跨 scrollback ----

    /// T-0804 核心场景: 用户在 viewport 内按下 anchor 后, 拖拽触发自动滚屏 5 行.
    /// SelectionState anchor/cursor 不变 (origin 跟 viewport 顶绑死), 仅
    /// display_offset 从 0 变 5. 验 selected_cells_linear 反查后 viewport 内
    /// 可见 cell 序列正确从原 anchor 行号下移.
    #[test]
    fn selection_persists_across_scrollback_when_dragging() {
        let mut s = SelectionState::new();
        // viewport 顶 (line=0) 按下 → 拖到 viewport 第 3 行.
        s.start(sp(0, 0), SelectionMode::Linear);
        s.update(sp(5, 3));

        // 滚屏前 (display_offset=0): cells 落 viewport 行 0..=3.
        let cells_before = selected_cells_linear(&s, 80, 24, 0);
        assert!(!cells_before.is_empty(), "滚屏前应 emit cells");
        assert_eq!(cells_before[0], cp(0, 0), "首 cell viewport row 0");
        assert!(
            cells_before.iter().any(|c| c.line == 3),
            "应含 viewport row 3"
        );
        let count_before = cells_before.len();

        // 模拟 autoscroll 5 行 — SelectionState 不动, 仅 display_offset=5.
        // anchor/cursor 仍是 sp(0,0)/sp(5,3); viewport 反查 0+5=5, 3+5=8 → 落
        // viewport row 5..=8 (滚屏后 anchor 字滚到 viewport 第 5 行可见).
        let cells_after = selected_cells_linear(&s, 80, 24, 5);
        assert_eq!(
            cells_after.len(),
            count_before,
            "滚屏后 viewport 内可见 cell 数应等于滚屏前 (整选区都在 viewport 内)"
        );
        assert_eq!(cells_after[0], cp(0, 5), "首 cell viewport row 0+5=5");
        assert!(
            cells_after.iter().any(|c| c.line == 8),
            "应含 viewport row 3+5=8"
        );

        // anchor/cursor 数据本身不变 — origin 跟 viewport 绑死.
        assert_eq!(s.anchor(), sp(0, 0));
        assert_eq!(s.cursor(), sp(5, 3));
    }

    /// T-0804: 选区跨 history + viewport 边界, 仅 emit 当前 viewport 内可见 cell,
    /// 滚出顶 (line<-display_offset) 或滚出底 (line>rows-display_offset) 的部分
    /// 整行 skip. 数据保留在 SelectionState 不丢.
    #[test]
    fn selected_cells_linear_skips_off_viewport_lines() {
        // 选区: anchor line=-2 (history 2 行前), cursor line=10 (viewport 第 10 行).
        // 当前 display_offset=0, rows=5. viewport_line = sel_line + 0 ∈ [0,5).
        // line=-2 → viewport=-2 (skip), line=-1 → -1 (skip), line=0..=4 → 0..=4 (emit),
        // line=5..=10 → 5..=10 (skip).
        let mut s = SelectionState::new();
        s.start(sp(0, -2), SelectionMode::Linear);
        s.update(sp(7, 10));

        let cells = selected_cells_linear(&s, 80, 5, 0);
        // 仅 viewport line 0..=4 应 emit.
        assert!(!cells.is_empty(), "viewport 内部分应 emit");
        for c in &cells {
            assert!(
                c.line < 5,
                "emit 的 cell viewport line 应 < rows=5, 实得 {}",
                c.line
            );
        }
        // line=-2 / line=10 都不应出现 (滚出 viewport).
        assert!(!cells.iter().any(|c| c.line >= 5), "viewport 外行不应 emit");

        // 验数据保留 — anchor/cursor 仍是原值 (跨 history 复制走 T-0805).
        assert_eq!(s.anchor(), sp(0, -2));
        assert_eq!(s.cursor(), sp(7, 10));
    }

    // ---- T-0805 新增: extract_selection_text 跨 history + viewport ----

    /// T-0805 核心场景: 选区跨 history + viewport, 复制结果应是
    /// "anchor.col 起 history 部分末行 → 中间整行 → viewport 末行 end.col 止"
    /// 完整文本, 不再被 T-0804 fallback clamp 成 "viewport 整段从 col 0".
    ///
    /// scenario: anchor.line=-3 col=2, cursor.line=2 col=10, display_offset=5.
    /// 6 行: -3, -2, -1, 0, 1, 2. 首行 "h-3 line"[2..]="3 line", 中间 4 行整行,
    /// 末行 "v2 line"[..=10]="v2 line" (短于 11 char, 全取).
    #[test]
    fn extract_linear_crosses_history_to_viewport() {
        let mut s = SelectionState::new();
        s.start(sp(2, -3), SelectionMode::Linear);
        s.update(sp(10, 2));

        let row_text = |line: i32| -> String {
            match line {
                -3 => "h-3 line".into(),
                -2 => "h-2 line".into(),
                -1 => "h-1 line".into(),
                0 => "v0 line".into(),
                1 => "v1 line".into(),
                2 => "v2 line".into(),
                _ => String::new(),
            }
        };
        let out = extract_selection_text(&s, 80, 24, 5, row_text);
        // 首行 [2..] = "3 line", 中间 4 行整行, 末行 [..=10] (短行全取) = "v2 line"
        assert_eq!(out, "3 line\nh-2 line\nh-1 line\nv0 line\nv1 line\nv2 line");
    }

    /// T-0805 边界: anchor 远超 history_size, 调用方 closure 该行返空 (压根没那
    /// 么多 history). extract_linear 不该 panic 也不该 skip 后续行 — 首行空但
    /// 后续行内容正常拼回去.
    #[test]
    fn extract_linear_anchor_in_history_far_past_history_size() {
        let mut s = SelectionState::new();
        s.start(sp(0, -100), SelectionMode::Linear);
        s.update(sp(5, 0));

        let row_text = |line: i32| -> String {
            // 假设 history_size=2: line=-1, -2 有内容; line<=-3 返空 (越界).
            match line {
                -2 => "h-2 line".into(),
                -1 => "h-1 line".into(),
                0 => "v0 line".into(),
                _ => String::new(),
            }
        };
        let out = extract_selection_text(&s, 80, 24, 5, row_text);
        // 行 -100..=-3 全空, 行 -2/-1 有内容, 行 0 末行截 [..=5] = "v0 lin"
        // 首行 (line=-100, [0..]) = "", 之后每行 push '\n', 末行 [..=5]
        // → 99 个换行 + 中间 -2/-1 行 + 末行截. 直接验关键不变式:
        // 1) 不 panic; 2) 含 "h-2 line"; 3) 末尾是 "v0 lin"
        assert!(!out.is_empty(), "应有输出 (起码换行)");
        assert!(out.contains("h-2 line\n"), "history 中段应有内容");
        assert!(out.contains("h-1 line\n"), "history 中段应有内容");
        assert!(out.ends_with("v0 lin"), "末行应截到 [..=5]");
    }

    /// T-0805: Block 模式跨 history + viewport, 各行 [min_col..=max_col] 截.
    /// scenario: anchor (3, -2), cursor (7, 1) Block. 4 行 (-2,-1,0,1) 各取
    /// col 3..=7 (5 char). 末尾空格 trim.
    #[test]
    fn extract_block_crosses_history() {
        let mut s = SelectionState::new();
        s.start(sp(3, -2), SelectionMode::Block);
        s.update(sp(7, 1));

        let row_text = |line: i32| -> String {
            match line {
                -2 => "ABCDEFGHIJ".into(), // [3..=7] = "DEFGH"
                -1 => "abcdef    ".into(), // [3..=7] = "def  " → trim → "def"
                0 => "0123456789".into(),  // [3..=7] = "34567"
                1 => "xy        ".into(),  // [3..=7] = "     " → trim → ""
                _ => String::new(),
            }
        };
        let out = extract_selection_text(&s, 80, 24, 0, row_text);
        assert_eq!(out, "DEFGH\ndef\n34567\n");
    }

    // ---- T-0809 / ADR 0012: rebase_for_grid_scroll ----

    /// 场景 1 同源回归锁: viewport 内拖选 5 行后 PTY 输出 N 行致 grid 顶 N 行
    /// 进 scrollback. 字本身位置物理不变 (滚到 history 区), anchor.line 必须
    /// 同步减 N — 否则 .line 还指 viewport 顶, 渲染框圈住新输出的字 (bug 1).
    #[test]
    fn selection_anchor_follows_pty_scroll() {
        let mut s = SelectionState::new();
        s.start(sp(2, 0), SelectionMode::Linear); // viewport 第 0 行
        s.update(sp(7, 4)); // viewport 第 4 行
        s.rebase_for_grid_scroll(3, 100_000);
        assert_eq!(
            s.anchor(),
            sp(2, -3),
            "anchor 字进 scrollback 3 行, line=-3"
        );
    }

    /// 场景 1 cursor 端: 同 anchor 同步减 delta. 单独锁防回归 (cursor 路径漏调
    /// rebase 时 anchor 跟字 cursor 不跟会撕开选区).
    #[test]
    fn selection_cursor_follows_pty_scroll() {
        let mut s = SelectionState::new();
        s.start(sp(2, 0), SelectionMode::Linear);
        s.update(sp(7, 4));
        s.rebase_for_grid_scroll(3, 100_000);
        assert_eq!(s.cursor(), sp(7, 1), "cursor line 4 - 3 = 1");
    }

    /// 字滚出 scrollback 顶 (history_size_max 满 + 仍输出): rebase 后 .line
    /// 应 clamp 到 `-(history_size_max as i32)`, 不允许溢出表达"比最旧 history
    /// 还旧"的不存在位置. 选区数据保留 (`has_selection==true`), 渲染走
    /// `viewport_line = -hist + display_offset` 落 viewport 外 skip — 框消失
    /// 但 user 滚回 history 顶 viewport 仍能看到上限边缘.
    #[test]
    fn selection_clamped_when_scrolled_past_history_top() {
        let mut s = SelectionState::new();
        s.start(sp(0, -5), SelectionMode::Linear);
        s.update(sp(0, -5));
        // history_size_max = 10, anchor 已在 -5, rebase delta=20 → 名义 -25,
        // 应 clamp 到 -10.
        s.rebase_for_grid_scroll(20, 10);
        assert_eq!(s.anchor().line, -10, "clamp 到 -history_size_max");
        assert_eq!(s.cursor().line, -10, "cursor 同步 clamp");
        assert!(s.has_selection(), "选区数据仍保留");
    }

    /// 跨 history-viewport 边界: anchor 在 history (line=-2), cursor 在 viewport
    /// (line=3). PTY 输出 5 行, 两端都应同步 -5. 单独锁防"只 rebase viewport
    /// 一端" 漏 history 端 (反之亦然).
    #[test]
    fn selection_cross_boundary_rebase_both_ends() {
        let mut s = SelectionState::new();
        s.start(sp(2, -2), SelectionMode::Linear);
        s.update(sp(10, 3));
        s.rebase_for_grid_scroll(5, 100_000);
        assert_eq!(s.anchor(), sp(2, -7), "history 端 -2 - 5 = -7");
        assert_eq!(
            s.cursor(),
            sp(10, -2),
            "viewport 端 3 - 5 = -2 (进 history)"
        );
    }

    /// **bug 3 真因回归锁**: user 在 history (display_offset=D) 选了一段, CC
    /// 后台输出 N 行 → alacritty 内部 `display_offset += N` (维持 user 视角
    /// 锚定, 见 alacritty grid::scroll_up 路径). 不 rebase 时渲染
    /// `viewport_line = sel_line + (D+N)` → 蓝框相对屏幕往下漂 N. rebase 后
    /// `selection.line -= N` → `viewport_line = (sel_line - N) + (D+N) =
    /// sel_line + D` 不变 → 蓝框钉死.
    ///
    /// 本测试不直接调 alacritty Term, 走纯 selection 模块语义验证: rebase 前后
    /// 同时改 display_offset, 算出的 viewport_line 应 stable.
    #[test]
    fn selection_render_does_not_drift_with_history_growth() {
        let mut s = SelectionState::new();
        // 模拟 user display_offset=10 时选 history 一段.
        s.start(sp(2, -3), SelectionMode::Linear);
        s.update(sp(2, -3));
        let display_offset_before: usize = 10;
        let viewport_line_before = s.anchor().line + display_offset_before as i32;
        assert_eq!(viewport_line_before, 7, "anchor 渲染落 viewport row 7");

        // CC 输出 5 行: alacritty `display_offset += 5`, quill `selection.line -= 5`.
        s.rebase_for_grid_scroll(5, 100_000);
        let display_offset_after: usize = display_offset_before + 5;
        let viewport_line_after = s.anchor().line + display_offset_after as i32;
        assert_eq!(
            viewport_line_after, viewport_line_before,
            "rebase + display_offset auto-pin 抵消, 蓝框屏幕位置不漂"
        );
    }

    /// 边界: 选区为空 (默认 / clear 后) 时 rebase 必须 no-op, 不能因为 PTY
    /// 输出偶发改了 anchor / cursor 默认零值 (虽然不影响渲染 — has_selection
    /// 守门 — 但污染默认状态会让 has_selection() 重启时单测失败).
    #[test]
    fn rebase_when_no_selection_is_noop() {
        let mut s = SelectionState::new();
        s.rebase_for_grid_scroll(5, 100_000);
        assert_eq!(s.anchor(), sp(0, 0));
        assert_eq!(s.cursor(), sp(0, 0));
        assert!(!s.has_selection());
    }

    /// 边界: delta=0 (advance_bytes 没触发 grid scroll) no-op. delta<0
    /// (history shrink — 当前 alacritty 路径不会到此, 防御兜底) 也 no-op.
    #[test]
    fn rebase_zero_or_negative_delta_is_noop() {
        let mut s = SelectionState::new();
        s.start(sp(3, 5), SelectionMode::Linear);
        s.update(sp(3, 5));
        s.rebase_for_grid_scroll(0, 100_000);
        assert_eq!(s.anchor(), sp(3, 5));
        s.rebase_for_grid_scroll(-3, 100_000);
        assert_eq!(s.anchor(), sp(3, 5), "负 delta 不该 wrap 反向加");
    }
}
