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
//! - [`pixel_to_cell`]: 鼠标 logical px → CellPos (考虑 titlebar y 偏移 +
//!   cell 像素常数).
//! - [`modifier_to_selection_mode`]: bool (Alt 是否按住) → SelectionMode.
//! - [`selected_cells_linear`] / [`selected_cells_block`]: 给定 anchor / cursor,
//!   返迭代器吐 CellPos 全选区. 渲染层 (`wl/render.rs`) 走 set lookup, 选区
//!   提取层 (`wl/window.rs::extract_selection_text`) 用同 iter 串字.
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
/// **anchor / cursor 永远在 viewport 内** (`0..cols × 0..rows`), 调用方
/// (`Dispatch<WlPointer>`) 走 [`pixel_to_cell`] + clamp 保证. scrollback
/// 跨边界跟历史滚走是 P2 (派单 Out), 当前: 滚屏期间 cursor 跟 viewport 偏移
/// (autoscroll Timer 路径, 见 `wl/window.rs::resize_followup_tick` 同套路 +
/// pending_autoscroll_timer).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SelectionState {
    /// 选区起点 cell. 鼠标按下时记录, 拖动期间不变.
    anchor: CellPos,
    /// 选区终点 cell. 拖动 / autoscroll 时实时更新; 松开后保留供后续复制.
    cursor: CellPos,
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
            anchor: CellPos { col: 0, line: 0 },
            cursor: CellPos { col: 0, line: 0 },
            mode: SelectionMode::Linear,
            active: false,
            has_selection: false,
        }
    }

    /// 鼠标按下: 清旧选区 + 新 anchor + active=true. cursor 同步 anchor (单
    /// click 是零长度选区, 真长度由 update 拖出).
    pub fn start(&mut self, anchor: CellPos, mode: SelectionMode) {
        self.anchor = anchor;
        self.cursor = anchor;
        self.mode = mode;
        self.active = true;
        self.has_selection = true;
    }

    /// 拖动: 更新 cursor (anchor / mode 不变). 仅 active=true 时有效, 其它
    /// 情况静默忽略 (防协议事件 race — 例 button release 后又来 motion).
    pub fn update(&mut self, cursor: CellPos) {
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

    /// 当前选区 anchor (鼠标按下点).
    pub fn anchor(&self) -> CellPos {
        self.anchor
    }

    /// 当前选区 cursor (拖动到的点).
    pub fn cursor(&self) -> CellPos {
        self.cursor
    }

    /// 当前选区模式.
    pub fn mode(&self) -> SelectionMode {
        self.mode
    }
}

/// T-0607: 鼠标 logical px → viewport CellPos.
///
/// **why top_reserved 偏移**: cells 区从 `y >= top_reserved` 起绘. 单 tab 时
/// `top_reserved = TITLEBAR_H_LOGICAL_PX` (T-0504), 多 tab 时
/// `top_reserved = TITLEBAR_H + TAB_BAR_H` (T-0608/T-0617). 鼠标在 titlebar /
/// tab bar 区不该映射到 cell (派单"鼠标在 viewport 之外不算选"). 走
/// `Option<CellPos>` 让调用方判 None 直接早返.
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
pub fn pixel_to_cell(
    x: f64,
    y: f64,
    cols: usize,
    rows: usize,
    cell_w_logical: f64,
    cell_h_logical: f64,
    top_reserved_h_logical: f64,
) -> Option<CellPos> {
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
    let row = row_f.max(0) as usize;
    Some(CellPos {
        col: col.min(cols - 1),
        line: row.min(rows - 1),
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

/// T-0607: 给定 SelectionState + cols, 返迭代器吐选区内全 [`CellPos`] (Linear
/// 模式).
///
/// **行内分段** (派单 In #D):
/// - anchor.line < cursor.line: 起点行 `[anchor.col..cols)` + 中间整行
///   `[0..cols)` + 终点行 `[0..=cursor.col]`
/// - anchor.line == cursor.line: 单行段 `[min_col..=max_col]`
/// - anchor.line > cursor.line: 反向 (cursor 在 anchor 之上) — 把 (anchor, cursor)
///   交换走正向算法
///
/// **why `Vec<CellPos>` 而非 `impl Iterator`**: 选区典型 < 1000 cell (终端 80×24
/// = 1920 满屏, 实际拖选远少); Vec 分配开销 << 渲染或文本提取开销, KISS 胜过
/// 聪明 (与 `cells_iter().collect::<Vec<_>>` 派单 In #D 同决策, 单测易写).
pub fn selected_cells_linear(state: &SelectionState, cols: usize) -> Vec<CellPos> {
    if !state.has_selection || cols == 0 {
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
    if start.line == end.line {
        for c in start.col..=end.col.min(cols.saturating_sub(1)) {
            out.push(CellPos {
                col: c,
                line: start.line,
            });
        }
        return out;
    }
    // 起点行: start.col..cols
    for c in start.col..cols {
        out.push(CellPos {
            col: c,
            line: start.line,
        });
    }
    // 中间整行: (start.line+1..end.line) × (0..cols)
    for line in (start.line + 1)..end.line {
        for c in 0..cols {
            out.push(CellPos { col: c, line });
        }
    }
    // 终点行: 0..=end.col
    for c in 0..=end.col.min(cols.saturating_sub(1)) {
        out.push(CellPos {
            col: c,
            line: end.line,
        });
    }
    out
}

/// T-0607: Block 模式选区 cell 集合. (`min_row..=max_row`) × (`min_col..=max_col`)
/// 矩形, 不跨行流.
pub fn selected_cells_block(state: &SelectionState, cols: usize, rows: usize) -> Vec<CellPos> {
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
    let max_row = state
        .anchor
        .line
        .max(state.cursor.line)
        .min(rows.saturating_sub(1));
    let mut out = Vec::new();
    for line in min_row..=max_row {
        for c in min_col..=max_col {
            out.push(CellPos { col: c, line });
        }
    }
    out
}

/// T-0607: 给定 SelectionState + row_text 取行函数, 拼出整段选区文本.
///
/// `row_text(line)` 应返 `String` (与 [`crate::term::TermState::display_text`]
/// 同款 spacer 跳过 — CJK WIDE_CHAR_SPACER cell 不计 char count, 否则 substr
/// 跨字会错位).
///
/// **Linear**:
/// - 单行: `row_text[min_col..=max_col]` 走 char-index slice.
/// - 多行: 起点行 `[anchor.col..]` + 中间整行 + 终点行 `[..=cursor.col]`, 行间
///   `\n`.
///
/// **Block** (派单 In #G "末尾空格 trim — 用户期望剪表格列"):
/// - 每行 `[min_col..=max_col]` substr, **trim_end** (空格/tab 都剪) + `\n`
///   join. 末行后**不加** `\n` (与 alacritty 同).
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
    mut row_text: F,
) -> String
where
    F: FnMut(usize) -> String,
{
    if !state.has_selection || cols == 0 || rows == 0 {
        return String::new();
    }
    match state.mode {
        SelectionMode::Linear => extract_linear(state, cols, rows, &mut row_text),
        SelectionMode::Block => extract_block(state, cols, rows, &mut row_text),
    }
}

fn extract_linear<F>(state: &SelectionState, cols: usize, rows: usize, row_text: &mut F) -> String
where
    F: FnMut(usize) -> String,
{
    let _ = cols; // cols 在行内 substr 时不需要 (chars().count() 是真实字符数 ≤ cols)
    let (start, end) =
        if (state.anchor.line, state.anchor.col) <= (state.cursor.line, state.cursor.col) {
            (state.anchor, state.cursor)
        } else {
            (state.cursor, state.anchor)
        };
    let max_line = rows.saturating_sub(1);
    if start.line > max_line {
        return String::new();
    }
    let end_line = end.line.min(max_line);

    let mut out = String::new();
    if start.line == end_line {
        let txt = row_text(start.line);
        out.push_str(&substr_chars(&txt, start.col, end.col + 1));
        return out;
    }
    // 起点行: [anchor.col..]
    let first = row_text(start.line);
    out.push_str(&substr_chars(&first, start.col, usize::MAX));
    out.push('\n');
    // 中间整行
    for line in (start.line + 1)..end_line {
        out.push_str(&row_text(line));
        out.push('\n');
    }
    // 终点行: [..=cursor.col]
    let last = row_text(end_line);
    out.push_str(&substr_chars(&last, 0, end.col + 1));
    out
}

fn extract_block<F>(state: &SelectionState, cols: usize, rows: usize, row_text: &mut F) -> String
where
    F: FnMut(usize) -> String,
{
    let _ = cols;
    let min_col = state.anchor.col.min(state.cursor.col);
    let max_col = state.anchor.col.max(state.cursor.col);
    let min_row = state.anchor.line.min(state.cursor.line);
    let max_row = state
        .anchor
        .line
        .max(state.cursor.line)
        .min(rows.saturating_sub(1));
    let mut lines: Vec<String> = Vec::new();
    for line in min_row..=max_row {
        let txt = row_text(line);
        let seg = substr_chars(&txt, min_col, max_col + 1);
        // 派单 In #G "Block 末尾空格 trim — 用户期望剪表格列".
        let trimmed = seg.trim_end_matches([' ', '\t']).to_string();
        lines.push(trimmed);
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
        s.start(cp(5, 3), SelectionMode::Linear);
        assert!(s.has_selection());
        assert!(s.is_active());
        assert_eq!(s.anchor(), cp(5, 3));
        assert_eq!(s.cursor(), cp(5, 3), "cursor 起步 = anchor (零长度选区)");
        assert_eq!(s.mode(), SelectionMode::Linear);
    }

    #[test]
    fn update_advances_cursor_keeping_anchor() {
        let mut s = SelectionState::new();
        s.start(cp(5, 3), SelectionMode::Linear);
        s.update(cp(20, 8));
        assert_eq!(s.anchor(), cp(5, 3));
        assert_eq!(s.cursor(), cp(20, 8));
        assert!(s.is_active());
    }

    #[test]
    fn update_when_inactive_is_ignored() {
        let mut s = SelectionState::new();
        // 没 start 直接 update — race 防御
        s.update(cp(20, 8));
        assert_eq!(s.cursor(), cp(0, 0), "未 start 时 update 应静默忽略");
    }

    #[test]
    fn end_keeps_selection_for_later_copy() {
        let mut s = SelectionState::new();
        s.start(cp(5, 3), SelectionMode::Linear);
        s.update(cp(20, 8));
        s.end();
        assert!(!s.is_active(), "松开后 active=false");
        assert!(s.has_selection(), "选区保留给 Ctrl+Shift+C");
        assert_eq!(s.anchor(), cp(5, 3));
        assert_eq!(s.cursor(), cp(20, 8));
    }

    #[test]
    fn clear_removes_selection() {
        let mut s = SelectionState::new();
        s.start(cp(5, 3), SelectionMode::Linear);
        s.clear();
        assert!(!s.has_selection());
        assert!(!s.is_active());
    }

    #[test]
    fn new_start_clears_old_selection() {
        let mut s = SelectionState::new();
        s.start(cp(5, 3), SelectionMode::Linear);
        s.update(cp(20, 8));
        s.start(cp(40, 1), SelectionMode::Block);
        assert_eq!(s.anchor(), cp(40, 1));
        assert_eq!(s.cursor(), cp(40, 1), "新 start 重置 cursor");
        assert_eq!(s.mode(), SelectionMode::Block);
    }

    // ---- selected_cells_linear ----

    #[test]
    fn linear_single_line_forward() {
        let mut s = SelectionState::new();
        s.start(cp(5, 3), SelectionMode::Linear);
        s.update(cp(10, 3));
        let cells = selected_cells_linear(&s, 80);
        assert_eq!(cells.len(), 6); // [5, 6, 7, 8, 9, 10]
        assert_eq!(cells.first(), Some(&cp(5, 3)));
        assert_eq!(cells.last(), Some(&cp(10, 3)));
    }

    #[test]
    fn linear_two_lines_forward() {
        let mut s = SelectionState::new();
        s.start(cp(70, 1), SelectionMode::Linear);
        s.update(cp(10, 2));
        let cells = selected_cells_linear(&s, 80);
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
        s.start(cp(75, 0), SelectionMode::Linear);
        s.update(cp(5, 2));
        let cells = selected_cells_linear(&s, 80);
        // 行 0: [75..80) = 5
        // 行 1: [0..80) = 80
        // 行 2: [0..=5] = 6
        assert_eq!(cells.len(), 5 + 80 + 6);
    }

    #[test]
    fn linear_anchor_below_cursor_is_swapped() {
        let mut s = SelectionState::new();
        s.start(cp(10, 5), SelectionMode::Linear);
        s.update(cp(70, 1)); // cursor 在 anchor 之上
        let cells = selected_cells_linear(&s, 80);
        // 等价 anchor=(70,1) cursor=(10,5):
        // 行 1: [70..80) = 10, 行 2/3/4: 80×3 = 240, 行 5: [0..=10] = 11
        assert_eq!(cells.len(), 10 + 80 * 3 + 11);
        assert_eq!(cells[0], cp(70, 1));
    }

    #[test]
    fn linear_no_selection_returns_empty() {
        let s = SelectionState::new();
        assert!(selected_cells_linear(&s, 80).is_empty());
    }

    // ---- selected_cells_block ----

    #[test]
    fn block_one_by_one() {
        let mut s = SelectionState::new();
        s.start(cp(5, 3), SelectionMode::Block);
        let cells = selected_cells_block(&s, 80, 24);
        assert_eq!(cells.len(), 1);
        assert_eq!(cells[0], cp(5, 3));
    }

    #[test]
    fn block_n_by_m() {
        let mut s = SelectionState::new();
        s.start(cp(5, 2), SelectionMode::Block);
        s.update(cp(8, 4));
        let cells = selected_cells_block(&s, 80, 24);
        // 列 5..=8 (4 cols) × 行 2..=4 (3 rows) = 12 cells
        assert_eq!(cells.len(), 12);
        assert!(cells.contains(&cp(5, 2)));
        assert!(cells.contains(&cp(8, 4)));
    }

    #[test]
    fn block_negative_direction_swaps() {
        let mut s = SelectionState::new();
        s.start(cp(8, 4), SelectionMode::Block);
        s.update(cp(5, 2)); // cursor 在 anchor 之上 + 之左
        let cells = selected_cells_block(&s, 80, 24);
        // min_col=5, max_col=8, min_row=2, max_row=4 — 同正向
        assert_eq!(cells.len(), 12);
        assert!(cells.contains(&cp(5, 2)));
        assert!(cells.contains(&cp(8, 4)));
    }

    // ---- pixel_to_cell ----

    #[test]
    fn pixel_to_cell_in_text_area() {
        // 800×600 surface, cell 10×25 logical, titlebar 28
        // x=100 y=53 (53 - 28 = 25 → row 1) → col 10 row 1
        assert_eq!(
            pixel_to_cell(100.0, 53.0, 80, 22, 10.0, 25.0, 28.0),
            Some(cp(10, 1))
        );
    }

    #[test]
    fn pixel_to_cell_at_titlebar_boundary() {
        // y=27.9 < titlebar (28) → None
        assert_eq!(pixel_to_cell(100.0, 27.9, 80, 22, 10.0, 25.0, 28.0), None);
        // y=28 (== titlebar) → row 0
        assert_eq!(
            pixel_to_cell(100.0, 28.0, 80, 22, 10.0, 25.0, 28.0),
            Some(cp(10, 0))
        );
    }

    #[test]
    fn pixel_to_cell_clamps_to_last_cell() {
        // 大于 cols × cell_w_logical (80×10=800) → col=79
        assert_eq!(
            pixel_to_cell(900.0, 100.0, 80, 22, 10.0, 25.0, 28.0),
            Some(cp(79, ((100.0 - 28.0) / 25.0) as usize))
        );
    }

    #[test]
    fn pixel_to_cell_negative_x_returns_none() {
        // 协议防御
        assert_eq!(pixel_to_cell(-1.0, 100.0, 80, 22, 10.0, 25.0, 28.0), None);
    }

    #[test]
    fn pixel_to_cell_zero_dimensions_returns_none() {
        assert_eq!(pixel_to_cell(100.0, 100.0, 0, 22, 10.0, 25.0, 28.0), None);
        assert_eq!(pixel_to_cell(100.0, 100.0, 80, 0, 10.0, 25.0, 28.0), None);
    }

    /// T-0608 hotfix 2026-04-29: 多 tab 时 top_reserved = titlebar + tab_bar.
    /// 复现 bug: 用旧值 28 (仅 titlebar) 时 y=53 算出 row 1, 但 render 路径
    /// 已让 cells 起点偏到 56 (28+28), 视觉上 y=53 还在 tab bar 区不该映射 cell.
    /// 修后传 56 (top_reserved): y=53 < 56 → None (鼠标在 tab bar 区不算选).
    #[test]
    fn pixel_to_cell_with_tab_bar_offset() {
        // 多 tab 场景: titlebar 28 + tab_bar 28 = top_reserved 56.
        // y=55.9 落 tab bar 区 → None.
        assert_eq!(pixel_to_cell(100.0, 55.9, 80, 22, 10.0, 25.0, 56.0), None);
        // y=56 (== top_reserved) → row 0.
        assert_eq!(
            pixel_to_cell(100.0, 56.0, 80, 22, 10.0, 25.0, 56.0),
            Some(cp(10, 0))
        );
        // y=81 (= 56 + 25) → row 1, 与单 tab y=53 (= 28 + 25) row 1 同 row idx.
        // 验证: render origin 与 hit_test origin 同步, row idx 对相同 cell 一致.
        assert_eq!(
            pixel_to_cell(100.0, 81.0, 80, 22, 10.0, 25.0, 56.0),
            Some(cp(10, 1))
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

    // ---- extract_selection_text ----

    #[test]
    fn extract_linear_single_line() {
        let mut s = SelectionState::new();
        s.start(cp(0, 0), SelectionMode::Linear);
        s.update(cp(4, 0));
        let row_text = |line: usize| -> String {
            match line {
                0 => "hello world".into(),
                _ => String::new(),
            }
        };
        assert_eq!(extract_selection_text(&s, 80, 24, row_text), "hello");
    }

    #[test]
    fn extract_linear_multi_line_concat_with_newlines() {
        let mut s = SelectionState::new();
        s.start(cp(6, 0), SelectionMode::Linear);
        s.update(cp(4, 2));
        let row_text = |line: usize| -> String {
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
        let out = extract_selection_text(&s, 80, 24, row_text);
        assert_eq!(out, "world\nsecond line\nthird");
    }

    #[test]
    fn extract_block_trims_trailing_spaces() {
        let mut s = SelectionState::new();
        s.start(cp(0, 0), SelectionMode::Block);
        s.update(cp(9, 1));
        let row_text = |line: usize| -> String {
            match line {
                0 => "abc       ".into(), // 行 0: "abc" + 7 空格
                1 => "xyz123    ".into(), // 行 1: "xyz123" + 4 空格
                _ => String::new(),
            }
        };
        // Block 0..=9 × 0..=1, 末尾空格 trim
        assert_eq!(extract_selection_text(&s, 80, 24, row_text), "abc\nxyz123");
    }

    #[test]
    fn extract_handles_utf8_multibyte() {
        // CJK 字符走 chars().count() 而非 byte index — substr 不切坏中文
        let mut s = SelectionState::new();
        s.start(cp(0, 0), SelectionMode::Linear);
        s.update(cp(2, 0));
        let row_text = |line: usize| -> String {
            match line {
                0 => "中文测试".into(),
                _ => String::new(),
            }
        };
        // chars 0..=2 = "中文测"
        assert_eq!(extract_selection_text(&s, 80, 24, row_text), "中文测");
    }

    #[test]
    fn extract_no_selection_returns_empty() {
        let s = SelectionState::new();
        let row_text = |_: usize| String::new();
        assert!(extract_selection_text(&s, 80, 24, row_text).is_empty());
    }
}
