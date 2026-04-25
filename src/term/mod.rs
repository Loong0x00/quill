//! `alacritty_terminal::Term` 薄封装(T-0301)。
//!
//! 把 PTY 喂进来的字节流交给 `vte::ansi::Processor::advance(&mut term, bytes)`,
//! 驱动 grid / cursor / scrollback 状态机。**本 Phase 不渲染**,屏幕继续深蓝;
//! 本模块只是让字节不再只是被 `tracing::trace!` 吐掉而是真进了终端状态机,
//! 为 Phase 3 后续 ticket(T-0303 cursor / T-0304 scrollback / T-0305 渲染
//! cell)准备数据源。
//!
//! 设计:
//! - `Term<VoidListener>` —— EventListener 是 title / clipboard / bell 等
//!   外部副作用回调,Phase 3 的目标是"字节 → grid",这些还不接,用 `VoidListener`
//!   的 no-op 实现兜住
//! - 自建 `Dimensions` impl(alacritty_terminal 把它的 `TermSize` 放在
//!   `#[cfg(test)]` 的 `term::test` 模块里,下游只能自己实现)
//! - `advance(bytes)` 是入口,单个方法名一致于上游 `Processor::advance`
//!   语义,调用方不用学两套术语
//! - `cursor_pos()` 返回 [`CellPos`](T-0303 替代原 `cursor_point() -> (usize, i32)`,
//!   消 `i32` 类型污染);[`cursor_visible`] / [`cursor_shape`] 见各自 doc
//!
//! [`cursor_visible`]: TermState::cursor_visible
//! [`cursor_shape`]: TermState::cursor_shape

use alacritty_terminal::event::VoidListener;
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::Point as AlacPoint;
use alacritty_terminal::term::{Config, Term, TermMode};
use alacritty_terminal::vte::ansi::Processor;

/// 渲染层 cell 坐标。两字段都是 `usize` —— viewport 不含 scrollback(Phase 3
/// T-0304 再扩),所以 `line` 永不为负。
///
/// 刻意**不**re-export `alacritty_terminal::index::Point`:那个类型的
/// `Line(i32)` / `Column(usize)` 不对称,且未来换 VT 库(或 alacritty 版本
/// 升级改字段)时会 cascade 改到每个渲染调用点。本 struct 是 quill 渲染层
/// ↔ alacritty 的**唯一**适配点。
///
/// 给 T-0305 色块渲染、T-0303 光标追踪用:算像素位置直接
/// `col * cell_w, line * cell_h`,不用跨越 newtype 层。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct CellPos {
    pub col: usize,
    pub line: usize,
}

impl CellPos {
    /// 内部用:`cells_iter` 把 `display_iter` 的 `Indexed<&Cell>.point` 转
    /// 成 `CellPos`。
    ///
    /// **刻意保留为模块私有 inherent fn,不作为 `From<alacritty::Point>`
    /// trait impl 对外暴露** —— trait impl 一旦公开,下游可以
    /// `use alacritty::Point; let cp: CellPos = p.into()` 绕过 wrapper,
    /// 把 alacritty 类型漏出去。私有 fn 让 alacritty 类型彻底锁在本模块
    /// 内部,是"单一绑定点"的真正落实(审码 2026-04-25 T-0302 重审 P0-3
    /// 原话:"比 From trait 建议更严更好")。
    ///
    /// **saturating cast**:`line.0` 是 `i32` 可负(scrollback 历史),但
    /// `cells_iter` 只吐 viewport 内 screen-line(`display_iter` 对
    /// offset=0 时 line 范围是 `0..screen_lines`),不含负值;正常路径
    /// `max(0) as usize` 是 zero-loss。若未来 T-0304 scrollback 漏让负数
    /// 进来,clamp 到 0 比 panic / UB 强,下游看见 line=0 也不会炸。
    /// 届时补 scrollback 专用入口,不走本函数。
    fn from_alacritty(p: AlacPoint) -> Self {
        Self {
            col: p.column.0,
            line: p.line.0.max(0) as usize,
        }
    }
}

/// 滚动 buffer (scrollback) 中某历史行的位置。**与 [`CellPos`] 完全分离**,
/// 不扩 `CellPos` enum:viewport 内的 cell 用 `CellPos { col, line }`(line ∈
/// `0..rows`),滚出去的历史行用 `ScrollbackPos { row }`(row ∈
/// `0..scrollback_size()`)。两条独立通路,渲染层 / 调用方按场景选其一。
///
/// **row 语义**:
/// - `row = 0` → **最旧**的历史行(scrollback 顶端)
/// - `row = scrollback_size() - 1` → **最新**滚出 viewport 那一行(贴着 viewport 顶)
///
/// 这个方向选择是 quill 渲染层友好序:scroll-up UI 时"往上滚 N 行"对应
/// `row` 减少,自然顺序与 alacritty 内部 `Line(-1)` 是最新、`Line(-history)`
/// 是最旧的负值方向相反 —— 私有 [`to_alacritty`] 做反向映射,下游不感知。
///
/// **设计理由**(沿袭 T-0302 [`CellPos`] / T-0303 [`CursorShape`] 类型隔离 SOP):
/// - 不 re-export `alacritty_terminal::index::Line`/`Point`(那是 `i32`,负值 =
///   scrollback,语义对外不友好,且未来换 VT 库时要 cascade 改)
/// - 私有 `to_alacritty` inherent fn(非 `From` trait),让 alacritty scrollback
///   坐标彻底锁在 `src/term/mod.rs` 内,审码 T-0303 P3-2 推荐源头
/// - viewport line 永正,scrollback row 永正 —— 类型层面隔开正/负,
///   渲染调用点不再 mix `i32` / `usize`
///
/// 测试覆盖见 `tests::scrollback_*`。
///
/// [`to_alacritty`]: ScrollbackPos::to_alacritty
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ScrollbackPos {
    pub row: usize,
}

impl ScrollbackPos {
    /// 把 quill 的 `ScrollbackPos { row }`(row=0 最旧、row=history-1 最新)
    /// 映射到 alacritty 内部 `Line(i32)`(`-history` 最旧、`-1` 最新)。
    ///
    /// **模块私有 inherent fn**,不开 `impl From` / `impl Into` —— 沿袭
    /// `CellPos::from_alacritty` / `CursorShape::from_alacritty` 的隔离套路:
    /// 下游既不能 `Line::from(pos)` 反向构造,也不能 `pos.into()` 偷渡 alacritty
    /// 类型出去。
    ///
    /// **饱和 clamp**:理想情况下调用方先用 [`TermState::scrollback_size`] 校验
    /// row 在范围内,但万一漏检(row >= history_size 或 history_size == 0),
    /// 这里 clamp 到 `Line(-1)`(最新历史行)而非 panic / UB。下游看见已存在
    /// 历史行的内容(过近 1 行),比 alacritty `grid[Line]` 的越界 panic 友好。
    /// `history_size == 0` 时落到 `Line(0)` 即 viewport 第 0 行(无历史可索引,
    /// 显式分支让意图清楚)。
    fn to_alacritty(self, history_size: usize) -> alacritty_terminal::index::Line {
        use alacritty_terminal::index::Line;
        if history_size == 0 {
            return Line(0);
        }
        let history = history_size as i32;
        let row = (self.row as i32).min(history - 1);
        Line(row - history)
    }
}

/// quill 自己的光标形状枚举,**不**re-export `alacritty_terminal::vte::ansi::CursorShape`。
///
/// 与 alacritty 0.26 的 5 个 variants 一一对应:
/// - `Block` — 实心方块 `▒`(alacritty 默认)
/// - `Underline` — 下划线 `_`
/// - `Beam` — 竖线 `⎸`
/// - `HollowBlock` — 空心框 `☐`(blur 时常见)
/// - `Hidden` — 不画(独立于 SHOW_CURSOR mode 位)
///
/// **设计理由**(沿袭 `CellPos` 同款类型隔离):
/// - 不 re-export 上游 enum,防止下游 `use alacritty::CursorShape` 后
///   `match` 时漏 / 多 variant
/// - 私有 `from_alacritty` inherent fn(非 `From` trait),让 alacritty 类型
///   彻底锁在 `src/term/mod.rs` 内
/// - 未来换 VT 库时,quill 渲染层只需要重写 `from_alacritty` 转换逻辑,
///   不动渲染调用点
///
/// `Hidden` 与 `cursor_visible() == false` 的关系:**正交,两个都得查**。
/// alacritty 内部 `CursorRenderingData` 在 `SHOW_CURSOR` 关时返 Hidden;
/// 我们刻意把"模式位"(SHOW_CURSOR)和"形状配置"(CursorShape::Hidden)
/// 拆开 —— 渲染层 `if visible { draw(shape) }`,语义清晰。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CursorShape {
    Block,
    Underline,
    Beam,
    HollowBlock,
    Hidden,
}

impl CursorShape {
    /// 从 alacritty 的 `CursorShape` 转过来。**模块私有 inherent fn**,不开
    /// `impl From<...>` trait —— 同 `CellPos::from_alacritty` 的隔离套路:
    /// 下游即使 `use alacritty::CursorShape` 也无法构造 quill 的
    /// `CursorShape`,只能拿 `cursor_shape()` 返回的实例。
    ///
    /// 5 个 variant 全 1:1 映射,无折叠 —— alacritty 0.26 的 enum 与本枚举
    /// 一一对应,未来若 alacritty 加新 variant(例 `Bar`),编译期会报
    /// 非穷尽 match 错,届时显式补一行 + 决策映射(可能合并到 `Beam`)。
    fn from_alacritty(s: alacritty_terminal::vte::ansi::CursorShape) -> Self {
        use alacritty_terminal::vte::ansi::CursorShape as Up;
        match s {
            Up::Block => CursorShape::Block,
            Up::Underline => CursorShape::Underline,
            Up::Beam => CursorShape::Beam,
            Up::HollowBlock => CursorShape::HollowBlock,
            Up::Hidden => CursorShape::Hidden,
        }
    }
}

/// 80x24 窗口的最小 Dimensions 实现。`total_lines` 与 `screen_lines` 相等
/// 表示"无 scrollback"—— Phase 3 后续 T-0304 再加。
///
/// `Dimensions` 有多个方法,但大部分有默认实现(基于 total_lines / screen_lines
/// / columns 三个 primitive),我们只填这三个最基础的。
struct TermSize {
    columns: usize,
    screen_lines: usize,
}

impl Dimensions for TermSize {
    fn total_lines(&self) -> usize {
        // 无 scrollback:total_lines == screen_lines
        self.screen_lines
    }

    fn screen_lines(&self) -> usize {
        self.screen_lines
    }

    fn columns(&self) -> usize {
        self.columns
    }
}

/// PTY 字节 → alacritty `Term` 状态机的入口。
///
/// 结构:
/// - `term`:持 grid / cursor / scrollback / modes 等全部终端状态
/// - `processor`:VT / ANSI escape 解析器,`advance(&mut term, bytes)` 把
///   字节推给 `term` 的 `Handler` impl(alacritty_terminal 内部实现)
///
/// 字段顺序:`processor` 先 drop 再 `term` 也行,反过来也行——两者解耦,
/// processor 只在 advance 时持 &mut term。不登 INV。
pub struct TermState {
    term: Term<VoidListener>,
    processor: Processor,
    /// T-0302 dirty flag:`advance` 调用一次即置 true,`clear_dirty` 清零。
    /// 给 T-0305 渲染层"数据有新变动,该重画一帧"信号。
    ///
    /// 刻意用 plain `bool` 而非 alacritty 的行级 damage —— Phase 3 首轮
    /// 渲染全屏重画(1920 cell @ 60fps,5090 GPU 无压力)。Phase 6 soak
    /// 若发现性能瓶颈,再升级为行级 / 列级 damage(alacritty 已有内部 API,
    /// 换上来是加法不是重构)。
    dirty: bool,
}

impl TermState {
    /// 起一个初始尺寸为 `cols × rows` 的终端。`cols`/`rows` 由上游(T-0202
    /// 写死的 80×24,Phase 3 T-0306 才接 Wayland resize)传进来。
    ///
    /// 初始 `dirty = true`:ctor 后 Term 的 grid 是空白,但第一帧也得 "画"
    /// 一次(哪怕全空格),所以视作脏。调用方 [`clear_dirty`] 即可。
    ///
    /// [`clear_dirty`]: Self::clear_dirty
    pub fn new(cols: u16, rows: u16) -> Self {
        let size = TermSize {
            columns: cols as usize,
            screen_lines: rows as usize,
        };
        let config = Config::default();
        Self {
            term: Term::new(config, &size, VoidListener),
            processor: Processor::new(),
            dirty: true,
        }
    }

    /// 把一批 PTY 字节推进解析器,驱动 grid 更新。
    ///
    /// 上游 `Processor::advance(&mut handler, bytes)` 签名里的 handler 就是
    /// `Term<T>`,`Term` 实现了 `vte::ansi::Handler`。我们作为胶水把两者连起来。
    ///
    /// **副作用**:置 `self.dirty = true`。即使 `bytes` 空切片也置(没改变就
    /// 多画一次,成本小于漏画)。下游 [`is_dirty`] / [`clear_dirty`] 消费。
    ///
    /// [`is_dirty`]: Self::is_dirty
    /// [`clear_dirty`]: Self::clear_dirty
    pub fn advance(&mut self, bytes: &[u8]) {
        self.processor.advance(&mut self.term, bytes);
        self.dirty = true;
    }

    /// 返回当前光标位置(viewport 坐标 [`CellPos`])。
    ///
    /// T-0303 把原 `cursor_point() -> (usize, i32)` 改成本签名 —— 消除 `i32`
    /// line 的类型污染,与 [`cells_iter`] 产出的 `CellRef.pos: CellPos`
    /// 类型一致,渲染层 / 调用方一套类型贯通。
    ///
    /// - `pos.col` 0-based 列号(left = 0)
    /// - `pos.line` 0-based screen-line(不含 scrollback offset);bash prompt
    ///   刚出来时通常是 `(prompt_len, 0)`
    ///
    /// 走 `CellPos::from_alacritty`(模块私有 saturating cast),scrollback
    /// 历史的负 line 在本 API 路径下不会触发(grid().cursor.point 永远在
    /// viewport),但即使触发也 clamp 到 0,不 panic。
    ///
    /// [`cells_iter`]: Self::cells_iter
    pub fn cursor_pos(&self) -> CellPos {
        CellPos::from_alacritty(self.term.grid().cursor.point)
    }

    /// 读取指定行(screen-line `0..screen_lines`)的字符,作为 `String` 返回。
    /// 末尾空白不 trim,调用方自己判断。主要给集成测试 / 调试查 grid 内容。
    ///
    /// T-0302 之前是 grid 内容的唯一公开入口;T-0302 起 [`cells_iter`] 是更
    /// 高效的逐 cell 访问方式,渲染代码应该用 cells_iter;`line_text` 留给
    /// 测试 / 人工调试。
    pub fn line_text(&self, line: usize) -> String {
        use alacritty_terminal::index::{Column, Line};
        let grid = self.term.grid();
        let row = &grid[Line(line as i32)];
        let cols = grid.columns();
        (0..cols).map(|c| row[Column(c)].c).collect()
    }

    // ---------- T-0302 渲染 API ----------
    // 下面这组方法给 T-0305 色块渲染准备好入口。**本 ticket 不做渲染**,只把
    // alacritty_terminal 内部 API 包一层:
    // - `cells_iter`:viewport 内所有 cell 迭代(渲染全量重画用)
    // - `is_dirty` / `clear_dirty`:advance 后置 true,渲染后清
    // - `cursor_visible`:光标画不画
    // - `dimensions`:渲染算 cell 像素位置
    //
    // 这些全部是"读 `self.term` 状态并转换成对下游友好的类型",副作用仅
    // `clear_dirty` 改 `self.dirty`。

    /// viewport 内所有可见 cell 的迭代器,带位置。给 T-0305 全量重画用。
    ///
    /// 一次调用产生 `rows × cols` 个 [`CellRef`](典型 80×24 = 1920)。
    /// 不走 scrollback,不包含历史行 —— 一帧重画 viewport 全部 cell。
    ///
    /// `CellRef` 只暴露 `pos + c`,暂不带 fg/bg color —— 本 ticket scope
    /// 是"API 搭好",T-0305 色块渲染先用 `c == ' '` 判空 / 非空画块,颜色
    /// 跟 style 等 Phase 3 后期补(加字段不破坏下游,因为 CellRef 是 struct
    /// 不是 tuple)。
    pub fn cells_iter(&self) -> CellsIter<'_> {
        CellsIter {
            inner: self.term.grid().display_iter(),
        }
    }

    /// 自上次 [`clear_dirty`] 后 grid 是否有任何变动。
    /// T-0305 render loop:
    /// ```text
    /// loop tick:
    ///   if term.is_dirty() {
    ///     render_frame(term.cells_iter(), term.cursor_*);
    ///     term.clear_dirty();
    ///   }
    /// ```
    ///
    /// 语义:`advance` 调一次就置 true(哪怕空切片,多画一次比漏画强)。
    /// [`new`] 返回的新 TermState 也是 `dirty = true`(首帧要画)。
    ///
    /// **不精确到行 / 列**:Phase 3 首轮用 plain bool 即够(5090 全屏重画
    /// 无压力);Phase 6 soak 若发现浪费,再升级为 alacritty 行级 damage
    /// (其内部 API 已具备)。
    ///
    /// [`clear_dirty`]: Self::clear_dirty
    /// [`new`]: Self::new
    pub fn is_dirty(&self) -> bool {
        self.dirty
    }

    /// 清 dirty 标记。调用时机:每帧渲染结束后。
    ///
    /// 忘了调 → 每帧都 `is_dirty == true` → 每帧全屏重画 → GPU 持续耗电但
    /// 屏幕内容不变。不出错但退化。
    pub fn clear_dirty(&mut self) {
        self.dirty = false;
    }

    /// 光标是否可见(`TermMode::SHOW_CURSOR` bit)。bash 启动后默认可见;
    /// 某些全屏程序(vim / less)会切 DECRST 25(`ESC[?25l`)隐藏,
    /// DECSET 25(`ESC[?25h`)恢复。T-0305 渲染判断"要不要画光标"。
    pub fn cursor_visible(&self) -> bool {
        self.term.mode().contains(TermMode::SHOW_CURSOR)
    }

    /// 光标形状,见 [`CursorShape`]。**与 [`cursor_visible`] 正交,两个都得查**:
    /// 渲染层伪代码 `if t.cursor_visible() { draw_cursor(t.cursor_shape()) }`。
    ///
    /// 改光标形状的 ANSI 序列是 `DECSCUSR` (`CSI Ps SP q`):
    /// - `0` / `1` = blinking block(默认)→ Block
    /// - `2` = steady block → Block
    /// - `3` = blinking underline → Underline
    /// - `4` = steady underline → Underline
    /// - `5` = blinking beam → Beam
    /// - `6` = steady beam → Beam
    ///
    /// **不暴露 blinking 信息**:`alacritty::CursorStyle.blinking` 字段我们
    /// 暂不读。Phase 3 渲染先不实现 blink 动画,T-0303 scope 也不含;若未来
    /// 加 blink 渲染,新增 `cursor_blinking() -> bool` 方法,不破坏本 API。
    ///
    /// [`cursor_visible`]: Self::cursor_visible
    pub fn cursor_shape(&self) -> CursorShape {
        CursorShape::from_alacritty(self.term.cursor_style().shape)
    }

    /// viewport 尺寸,返 `(cols, rows)`。渲染层算 cell 像素位置用:
    /// - 窗口宽 = cols × cell_width
    /// - 窗口高 = rows × cell_height
    ///
    /// Phase 2/3 早期写死 80×24,Phase 3 T-0306 接 Wayland resize 后会随
    /// 运行时变化;本方法读 alacritty 内部 `Dimensions::columns/screen_lines`,
    /// 永远返当前值。
    pub fn dimensions(&self) -> (usize, usize) {
        (self.term.columns(), self.term.screen_lines())
    }

    // ---------- T-0304 scrollback API ----------
    // alacritty `Grid` 已经实装 ring-buffer 形式的 scrollback storage(`Term::new`
    // 时按 `Config::scrolling_history` 默认 10000 行预留 max_scroll_limit),viewport
    // 满后多余的行往负 `Line(i32)` 索引扩张。本组方法是给 quill 公共 API 暴露
    // **只读**入口:
    // - `scrollback_size`:当前历史行数(动态 0..max_scroll_limit)
    // - `scrollback_line_text`:某历史行文本(测试 / 调试 / 未来 search-up UI)
    // - `scrollback_cells_iter`:某历史行 cell 迭代(给 T-0305 渲染层 scroll-up 用)
    //
    // 位置类型用独立 [`ScrollbackPos`](row=0 最旧),不混入 `CellPos`(它的 line
    // 永远在 `0..rows` viewport 内)。
    //
    // 不在 scope 内:scroll-up UI / 选择文本 / 历史行写入 / 改 history_size,
    // 这些是后续 ticket 的事。

    /// 当前 scrollback 中的历史行数。`0` 表示还没行被滚出 viewport。
    ///
    /// 上限是 `Config::scrolling_history`(默认 10000),实际值随 PTY 输出动态
    /// 增长 —— 每次 viewport 满后再来一行,最旧 viewport 行进 scrollback,
    /// `scrollback_size()` 加 1,直到撞到上限后旧行被丢弃。
    pub fn scrollback_size(&self) -> usize {
        self.term.grid().history_size()
    }

    /// 读取某历史行的文本。row 语义见 [`ScrollbackPos`]:`row = 0` 最旧、
    /// `row = scrollback_size() - 1` 最新滚出。
    ///
    /// 末尾空白不 trim(与 [`line_text`] 一致),调用方自己判断。主要给集成
    /// 测试 / 调试用;渲染层走 [`scrollback_cells_iter`] 更高效(避免 String 分配)。
    ///
    /// 越界(row >= scrollback_size 或 scrollback_size == 0)走
    /// [`ScrollbackPos::to_alacritty`] 的 clamp 路径,不 panic;返回的内容是
    /// clamp 落点行(最新一行 / viewport 第 0 行),调用方应先用
    /// [`scrollback_size`] 校验。
    ///
    /// [`line_text`]: Self::line_text
    /// [`scrollback_cells_iter`]: Self::scrollback_cells_iter
    /// [`scrollback_size`]: Self::scrollback_size
    pub fn scrollback_line_text(&self, pos: ScrollbackPos) -> String {
        use alacritty_terminal::index::Column;
        let grid = self.term.grid();
        let line = pos.to_alacritty(grid.history_size());
        let row = &grid[line];
        let cols = grid.columns();
        (0..cols).map(|c| row[Column(c)].c).collect()
    }

    /// 历史行的 cell 迭代器,给 T-0305 渲染层 scroll-up 用。
    ///
    /// 产出 `cols` 个 [`CellRef`](80×24 默认 80 个),与 [`cells_iter`] 同类型
    /// —— 渲染调用点能复用同一套绘制逻辑(`draw_cell_at_pos`)。
    ///
    /// **位置语义注意**:每个 `CellRef.pos.line` 字段固定填 `0`(占位),
    /// **真实位置由调用方传入的 [`ScrollbackPos`] 单独承载**。理由:scrollback
    /// 行没有 viewport line 概念,硬塞会让 `CellPos` 语义混乱(viewport line ∈
    /// `0..rows`)。`pos.col` 字段仍然有效(0..cols)。
    ///
    /// 越界 row 走 clamp(见 [`ScrollbackPos::to_alacritty`])。
    ///
    /// [`cells_iter`]: Self::cells_iter
    pub fn scrollback_cells_iter(&self, pos: ScrollbackPos) -> impl Iterator<Item = CellRef> + '_ {
        use alacritty_terminal::index::Column;
        let grid = self.term.grid();
        let line = pos.to_alacritty(grid.history_size());
        let cols = grid.columns();
        (0..cols).map(move |c| CellRef {
            // line=0 占位:scrollback 行没有 viewport line 概念,真实位置走
            // 调用方传入的 ScrollbackPos。详见 fn docstring。
            pos: CellPos { col: c, line: 0 },
            c: grid[line][Column(c)].c,
        })
    }
}

/// [`TermState::cells_iter`] 的 iterator。把 alacritty 的
/// `GridIterator<Cell>` 的 `Indexed<&Cell>` 重映射成我们自己的 [`CellRef`]
/// —— 隔离上游类型,T-0305 / T-0303 只对本模块 API 编码,不直接抓
/// `alacritty_terminal::term::cell::Cell` / `alacritty::index::Point`。
pub struct CellsIter<'a> {
    inner: alacritty_terminal::grid::GridIterator<'a, alacritty_terminal::term::cell::Cell>,
}

impl<'a> Iterator for CellsIter<'a> {
    type Item = CellRef;

    fn next(&mut self) -> Option<Self::Item> {
        self.inner.next().map(|indexed| CellRef {
            // 走模块私有 `CellPos::from_alacritty`,不经 `From` trait —— 防止
            // alacritty 类型漏到公共 API(见 `CellPos::from_alacritty` 文档)。
            pos: CellPos::from_alacritty(indexed.point),
            c: indexed.cell.c,
        })
    }
}

/// 渲染用 cell 引用。给 T-0305 看:(位置, 字符)就够画色块;style / color
/// 暂不暴露,Phase 3 后期按需再扩字段。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CellRef {
    /// cell 在 viewport 里的位置。见 [`CellPos`]。
    pub pos: CellPos,
    /// cell 里的字符。空 cell 是 `' '`(空格),下游按需判空。
    pub c: char,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// T-0301 冒烟:构造一个 80×24 的 Term,喂 `"hi\r\n"`,光标移动符合
    /// VT100 / xterm 语义。T-0303 起断言走 `cursor_pos() -> CellPos`。
    #[test]
    fn advance_hi_newline_moves_cursor() {
        let mut t = TermState::new(80, 24);
        assert_eq!(
            t.cursor_pos(),
            CellPos { col: 0, line: 0 },
            "初始光标应在 (0, 0)"
        );

        t.advance(b"hi");
        let cp = t.cursor_pos();
        assert_eq!(cp.line, 0, "'hi' 没换行, 光标应仍在第 0 行");
        assert_eq!(cp.col, 2, "'hi' 写两个字,列号到 2");

        // \r\n: CR 回列首, LF index 到下一行。alacritty 对 LF 默认 linefeed
        // 不附带 CR, 实际终端 onlcr 由 tty 层翻译;本测试走 \r\n 模拟 PTY
        // 真正吐给我们的字节。
        t.advance(b"\r\n");
        let cp = t.cursor_pos();
        assert_eq!(cp.col, 0, "CRLF 后列应回 0");
        assert_eq!(cp.line, 1, "CRLF 后行应 +1");
    }

    /// 防回归:构造后立即 advance 空切片不 panic,不改动光标。
    #[test]
    fn advance_empty_is_noop() {
        let mut t = TermState::new(80, 24);
        t.advance(b"");
        assert_eq!(t.cursor_pos(), CellPos { col: 0, line: 0 });
    }

    /// 尺寸参数如实落到 Term。80x24 是项目基准,改了要明白影响面。
    #[test]
    fn dimensions_reflect_ctor_args() {
        let t = TermState::new(120, 40);
        assert_eq!(t.term.columns(), 120);
        assert_eq!(t.term.screen_lines(), 40);
    }

    /// ANSI 转义序列不崩:bash 启动吐的典型 "cursor home + erase" 序列
    /// `\x1b[H\x1b[2J`,光标应回到 (0, 0)。证 Processor 真跑起来,而不是
    /// 把字符原样输出。
    #[test]
    fn ansi_home_and_clear_resets_cursor() {
        let mut t = TermState::new(80, 24);
        t.advance(b"xyz");
        assert_ne!(
            t.cursor_pos(),
            CellPos { col: 0, line: 0 },
            "pre: 'xyz' 后不应仍在 0,0"
        );

        t.advance(b"\x1b[H\x1b[2J"); // ESC[H 回家 + ESC[2J 清屏
        assert_eq!(
            t.cursor_pos(),
            CellPos { col: 0, line: 0 },
            "ESC[H 应把光标回 (0, 0)"
        );
    }

    // ---------- T-0302 渲染 API 单测 ----------

    /// cells_iter 数量 = rows × cols。viewport 没 scrollback,不多不少。
    /// 80×24 = 1920 个 cell。
    #[test]
    fn cells_iter_yields_rows_times_cols() {
        let t = TermState::new(80, 24);
        let count = t.cells_iter().count();
        assert_eq!(count, 80 * 24, "viewport 应恰好 {} 个 cell", 80 * 24);
    }

    /// cells_iter 能准确读出写入的字符位置 + 内容。写 'A' 到 (0,0),
    /// 应能在 iter 中找到对应 CellRef。
    #[test]
    fn cells_iter_reflects_advance_bytes() {
        let mut t = TermState::new(80, 24);
        t.advance(b"A");

        // 找 (line=0, col=0) 那个 cell —— CellPos 用 usize,不是 alacritty 的 Line/Column
        let cell = t
            .cells_iter()
            .find(|cr| cr.pos.line == 0 && cr.pos.col == 0)
            .expect("(0,0) 应存在");
        assert_eq!(cell.c, 'A', "(0,0) 应为 'A'");

        // (line=0, col=1) 应仍是空格(光标停在 col=1 表示这里没写)
        let cell_01 = t
            .cells_iter()
            .find(|cr| cr.pos.line == 0 && cr.pos.col == 1)
            .expect("(0,1) 应存在");
        assert_eq!(cell_01.c, ' ', "未写入的 cell 应是空格");
    }

    /// ctor 后 is_dirty 应为 true(首帧要画)。clear 后 false。advance 后再 true。
    /// advance 空切片也置 dirty(保守:多画一帧 << 漏画)。
    #[test]
    fn is_dirty_tracks_advance_and_clear() {
        let mut t = TermState::new(80, 24);
        assert!(t.is_dirty(), "ctor 后应 dirty,首帧待画");

        t.clear_dirty();
        assert!(!t.is_dirty(), "clear 后应 clean");

        t.advance(b"hi");
        assert!(t.is_dirty(), "advance 非空后应 dirty");

        t.clear_dirty();
        assert!(!t.is_dirty());

        // 空切片也置 dirty(语义:保守 over-draw,不过 advance 本身是 no-op 也置)
        t.advance(b"");
        assert!(t.is_dirty(), "advance 空切片也应置 dirty");
    }

    /// 初始 cursor_visible 应为 true(TermMode::SHOW_CURSOR 默认开)。
    /// DECSET 25(`ESC[?25h`)开启,DECRST 25(`ESC[?25l`)关闭。
    #[test]
    fn cursor_visible_reacts_to_decrset_25() {
        let mut t = TermState::new(80, 24);
        assert!(t.cursor_visible(), "初始光标应可见");

        t.advance(b"\x1b[?25l"); // 关
        assert!(!t.cursor_visible(), "DECRST 25 后光标应隐藏");

        t.advance(b"\x1b[?25h"); // 开
        assert!(t.cursor_visible(), "DECSET 25 后光标应重现");
    }

    /// dimensions 应原样返回 ctor 参数(cols, rows)。
    /// Phase 3 T-0306 接 Wayland resize 后会动态变化,届时本测试仍应过
    /// (读的是 alacritty 当前状态,不是 ctor 传参)。
    #[test]
    fn dimensions_matches_ctor() {
        let t = TermState::new(80, 24);
        assert_eq!(t.dimensions(), (80, 24));

        let t2 = TermState::new(120, 40);
        assert_eq!(t2.dimensions(), (120, 40));
    }

    /// `CellPos::from_alacritty` 防回归:
    /// 1. 正常 viewport 坐标(line >= 0)字段对应
    /// 2. 负 line 走 `.max(0) as usize` 路径 clamp 到 0,不 panic
    ///
    /// **用 inherent fn 而不是 From trait**:审码 2026-04-25 重审 P0-3 明确
    /// "私有 inherent 比 From trait 更严 —— 下游无法 `use alacritty::Point;
    /// p.into()` 绕过 wrapper"。测试调用走 `CellPos::from_alacritty(p)` 验证
    /// 同文件 + tests mod 能访问该私有 fn。
    #[test]
    fn cellpos_from_alacritty_viewport_and_negative_line() {
        use alacritty_terminal::index::{Column, Line};

        // 正常 viewport 坐标
        let p = AlacPoint::new(Line(5), Column(10));
        let cp = CellPos::from_alacritty(p);
        assert_eq!(cp.col, 10);
        assert_eq!(cp.line, 5);

        // 负 line(理论上 scrollback 历史,cells_iter 不会产生,但防回归)。
        // saturating cast release 也生效(不依赖 debug_assert)。
        let p_neg = AlacPoint::new(Line(-3), Column(7));
        let cp_neg = CellPos::from_alacritty(p_neg);
        assert_eq!(cp_neg.col, 7);
        assert_eq!(cp_neg.line, 0, "负 line 应 clamp 到 0");
    }

    // ---------- T-0303 cursor 追踪 API 单测 ----------

    /// `cursor_pos()` 替代旧 `cursor_point() -> (usize, i32)`,返回 [`CellPos`]
    /// 与 `cells_iter` 产出的 `CellRef.pos` 类型一致。本测试锁住:
    /// 1. 初始 (0, 0)
    /// 2. 写字节后 col 前进
    /// 3. 类型显式是 `CellPos`(非 tuple)—— 编译期保障
    #[test]
    fn cursor_pos_returns_cellpos() {
        let mut t = TermState::new(80, 24);
        let cp: CellPos = t.cursor_pos();
        assert_eq!(cp, CellPos { col: 0, line: 0 });

        t.advance(b"abc");
        assert_eq!(t.cursor_pos(), CellPos { col: 3, line: 0 });
    }

    /// `cursor_shape` 默认 `Block`(alacritty 0.26 `CursorStyle::default().shape`
    /// 即 `Block`)。新构造的 TermState 不应已经被 DECSCUSR 改过状态。
    #[test]
    fn cursor_shape_default_is_block() {
        let t = TermState::new(80, 24);
        assert_eq!(t.cursor_shape(), CursorShape::Block);
    }

    /// `DECSCUSR` (`CSI Ps SP q`) 切光标形状:奇数闪烁,偶数 steady,**形状**
    /// 在我们的 enum 里 fold 掉闪烁信息(不暴露 blinking)。
    /// - `1`/`2` block, `3`/`4` underline, `5`/`6` beam
    /// - `0` 是 "reset to default"(回 block)
    #[test]
    fn cursor_shape_reacts_to_decscusr() {
        let mut t = TermState::new(80, 24);
        // 初始 Block(本测试不依赖 default test 的覆盖,自测一次)
        assert_eq!(t.cursor_shape(), CursorShape::Block);

        t.advance(b"\x1b[3 q"); // blinking underline
        assert_eq!(t.cursor_shape(), CursorShape::Underline);

        t.advance(b"\x1b[6 q"); // steady beam
        assert_eq!(t.cursor_shape(), CursorShape::Beam);

        t.advance(b"\x1b[2 q"); // steady block
        assert_eq!(t.cursor_shape(), CursorShape::Block);
    }

    // ---------- T-0304 scrollback API 单测 ----------

    /// ctor 后 grid 还没溢出过 viewport,scrollback 应为 0。
    #[test]
    fn scrollback_size_zero_initially() {
        let t = TermState::new(80, 24);
        assert_eq!(
            t.scrollback_size(),
            0,
            "新构造的 TermState 还没行被滚出 viewport, scrollback 应为 0"
        );
    }

    /// 写超过 viewport 的行数,scrollback 应增长。24-行 viewport 推 50 行,
    /// 期望 scrollback >= 25(50 - 24 = 26 行进 history,允许 1 行误差给最后一行
    /// 的尾随换行边界)。
    ///
    /// 用 `\r\n` 而非 `\n`(详见 `advance_hi_newline_moves_cursor` 测试 PTY
    /// 字节语义说明)。
    #[test]
    fn scrollback_size_grows_after_overflow() {
        let mut t = TermState::new(80, 24);
        // 50 行,够把 24-行 viewport 顶满 + 多 26 行进 scrollback
        for i in 0..50 {
            let line = format!("line_{:02}\r\n", i);
            t.advance(line.as_bytes());
        }
        assert!(
            t.scrollback_size() >= 25,
            "推 50 行后 scrollback 应 >= 25, 实际 = {}",
            t.scrollback_size()
        );
    }

    /// `ScrollbackPos { row: 0 }` 应取最旧的历史行(scrollback 顶端)。
    ///
    /// 写 `line_00`...`line_49`,`row=0` 应返 `line_00...`(末尾空格不 trim,
    /// 验证 `starts_with`)。锁住"row 方向":row=0 是最旧,row=history-1 是
    /// 最新滚出 — 渲染层 / scroll-up UI 友好序。
    #[test]
    fn scrollback_line_text_returns_oldest_first() {
        let mut t = TermState::new(80, 24);
        for i in 0..50 {
            let line = format!("line_{:02}\r\n", i);
            t.advance(line.as_bytes());
        }
        let history = t.scrollback_size();
        assert!(history >= 25, "前置: scrollback 应 >= 25");

        let oldest = t.scrollback_line_text(ScrollbackPos { row: 0 });
        assert!(
            oldest.starts_with("line_00"),
            "row=0 应是最旧行 'line_00...', 实际: {:?}",
            oldest
        );

        // 反向锁: row=history-1 应是最新滚出去那一行
        // 50 行推完,viewport 显示最末 24 行(line_26..line_49),
        // 所以最新进 scrollback 的是 line_25 (推 line_26 时 line_25 顶出 viewport
        // 进 scrollback,以此类推)。但 alacritty 语义略有边界差异,只锁前缀
        // 是 line_xx 形式即可,不 hard-code 编号。
        let newest_history = t.scrollback_line_text(ScrollbackPos { row: history - 1 });
        assert!(
            newest_history.starts_with("line_"),
            "row=history-1 应也是某行 'line_NN...', 实际: {:?}",
            newest_history
        );
    }

    /// `scrollback_cells_iter` 产出的字符序列应与 `scrollback_line_text` 一致
    /// (同行另一种访问方式)。也锁住每个 `CellRef.pos.line == 0` 占位、
    /// `pos.col` 0..cols 顺序。
    #[test]
    fn scrollback_cells_iter_yields_chars() {
        let mut t = TermState::new(80, 24);
        for i in 0..50 {
            let line = format!("ABC{:02}\r\n", i);
            t.advance(line.as_bytes());
        }
        let history = t.scrollback_size();
        assert!(history >= 25);

        let pos = ScrollbackPos { row: 0 };
        let cells: Vec<CellRef> = t.scrollback_cells_iter(pos).collect();
        assert_eq!(cells.len(), 80, "每行应产 80 个 cell (cols)");

        // pos.line 占位为 0, pos.col 0..80 严格递增
        for (i, cr) in cells.iter().enumerate() {
            assert_eq!(
                cr.pos.line, 0,
                "scrollback CellRef.pos.line 应固定为 0 占位"
            );
            assert_eq!(cr.pos.col, i, "pos.col 应严格 0..80 递增");
        }

        // 字符序列 = scrollback_line_text 一致
        let chars_from_iter: String = cells.iter().map(|c| c.c).collect();
        let chars_from_text = t.scrollback_line_text(pos);
        assert_eq!(
            chars_from_iter, chars_from_text,
            "cells_iter 与 line_text 应给出同一行同一字符序列"
        );
        assert!(
            chars_from_iter.starts_with("ABC00"),
            "row=0 (oldest) 应是 'ABC00...', 实际: {:?}",
            chars_from_iter
        );
    }

    /// `ScrollbackPos::to_alacritty` 私有 inherent fn 的边界测试:
    /// 1. 正常映射 (row=0 → Line(-history), row=history-1 → Line(-1))
    /// 2. row 越界 → clamp 到 Line(-1) (不 panic)
    /// 3. history_size == 0 → 落到 Line(0) (兜底分支)
    ///
    /// 与 T-0302 `cellpos_from_alacritty_viewport_and_negative_line` 同款思路:
    /// 私有 fn 的覆盖也走同文件 tests mod。
    #[test]
    fn scrollbackpos_to_alacritty_boundaries() {
        use alacritty_terminal::index::Line;

        // 正常: history=10, row=0 → Line(-10) (最旧)
        assert_eq!(ScrollbackPos { row: 0 }.to_alacritty(10), Line(-10));
        // 正常: history=10, row=9 → Line(-1) (最新滚出)
        assert_eq!(ScrollbackPos { row: 9 }.to_alacritty(10), Line(-1));
        // 越界: history=10, row=999 → clamp Line(-1)
        assert_eq!(ScrollbackPos { row: 999 }.to_alacritty(10), Line(-1));
        // 边界 history=0: 兜底落 Line(0) (无历史可索引)
        assert_eq!(ScrollbackPos { row: 0 }.to_alacritty(0), Line(0));
        assert_eq!(ScrollbackPos { row: 5 }.to_alacritty(0), Line(0));
    }

    /// `CursorShape::from_alacritty` 5 个 variants 全 1:1 映射,无折叠 / 无丢失。
    /// 同文件 tests mod 可访问私有 inherent fn。
    #[test]
    fn cursor_shape_from_alacritty_all_variants() {
        use alacritty_terminal::vte::ansi::CursorShape as Up;
        assert_eq!(CursorShape::from_alacritty(Up::Block), CursorShape::Block);
        assert_eq!(
            CursorShape::from_alacritty(Up::Underline),
            CursorShape::Underline
        );
        assert_eq!(CursorShape::from_alacritty(Up::Beam), CursorShape::Beam);
        assert_eq!(
            CursorShape::from_alacritty(Up::HollowBlock),
            CursorShape::HollowBlock
        );
        assert_eq!(CursorShape::from_alacritty(Up::Hidden), CursorShape::Hidden);
    }
}
