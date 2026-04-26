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
use alacritty_terminal::vte::ansi::{Color as AlacColor, NamedColor, Processor, Rgb as AlacRgb};

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
///
/// **HollowBlock(空心方块,focus 失去时的光标形状)在 Phase 3 色块渲染下
/// 简化为实心 Block(一个色块),Phase 4 字形渲染时再画矩形外框区分焦点状态。
/// (T-0303 审码 P3-2 推荐 fold + 延后)** —— T-0305 落决策但不画 cursor
/// (派单 scope "cursor 渲染本单不强制"),fold 实施留 cursor 渲染 ticket。
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

/// quill 自己的 cell 颜色。**不**re-export `alacritty_terminal::vte::ansi::Color`
/// (那是 `enum { Spec(Rgb), Named(NamedColor), Indexed(u8) }`,语义未解析,
/// 渲染层拿到要再分支)。本结构是**已解析**的 RGB,渲染层直接喂 GPU。
///
/// **不带 alpha**:terminal cell 总是不透明,引入 alpha 只会让下游误用
/// (T-0305 scope 显式)。Phase 4 加 glyph 渲染时,fg 用作 glyph 颜色,
/// bg 用作 cell 全色块,都 opaque。
///
/// **设计理由**(沿袭 T-0302 [`CellPos`] / T-0303 [`CursorShape`] / T-0304
/// [`ScrollbackPos`] 的类型隔离 SOP):
/// - 不 re-export 上游 `Color` enum
/// - 私有 `from_alacritty` inherent fn(非 `From` trait)—— 下游 `use
///   alacritty::Color` 后无法 `c.into()` 反向构造,alacritty 类型彻底锁在
///   `src/term/mod.rs` 内
/// - 256 色调色板 / NamedColor 的解析在本模块一处搞定,渲染层只看 (r,g,b)
///
/// 测试覆盖见 `tests::color_*`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Color {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

impl Color {
    /// quill 默认前景色,light gray(`#d3d3d3`)。alacritty `NamedColor::Foreground`
    /// 解析到这里。bash 默认文字色应当用这个值,在深蓝清屏上视觉对比清晰。
    const DEFAULT_FG: Color = Color {
        r: 0xd3,
        g: 0xd3,
        b: 0xd3,
    };

    /// quill 默认背景色,黑(`#000000`)。alacritty `NamedColor::Background`
    /// 解析到这里。Phase 3 渲染层若画 bg(本 ticket 未启用),空白 cell 是黑;
    /// 当前 [`crate::wl::render::Renderer::draw_cells`] 用 fg 着色,bg 字段
    /// 仅做 Phase 4 准备,不直接体现在屏幕上。
    const DEFAULT_BG: Color = Color {
        r: 0x00,
        g: 0x00,
        b: 0x00,
    };

    /// 把 alacritty 的 `Color` 三 variants 解析为已解析的 RGB。
    ///
    /// **模块私有 inherent fn**,不开 `impl From` / `impl Into` —— 沿袭
    /// `CellPos::from_alacritty` / `CursorShape::from_alacritty` / `ScrollbackPos::to_alacritty`
    /// 的隔离套路:下游既不能 `Color::from(c)` 反向构造,也不能 `c.into()` 偷渡
    /// `alacritty::Color` 出去。
    ///
    /// **exhaustive match 无 `_ =>`**:alacritty `Color` enum 加新 variant 时
    /// 编译期报非穷尽错,届时显式补一行 + 决策映射。同样的策略下推到
    /// `NamedColor` 30 个 variant 的 [`named_color_rgb`] —— 那个 fn 内部 match
    /// 也无 `_ =>`(给未来加 variant 留 catch)。
    ///
    /// 三 variants 处理:
    /// - `Spec(Rgb)` —— 直接取 `(r, g, b)`
    /// - `Named(NamedColor)` —— [`named_color_rgb`] 查 ANSI 16 色 + 特殊色映射
    /// - `Indexed(u8)` —— [`indexed_color_rgb`] 查 256 色调色板
    ///
    /// [`named_color_rgb`]: crate::term::named_color_rgb
    /// [`indexed_color_rgb`]: crate::term::indexed_color_rgb
    fn from_alacritty(c: AlacColor) -> Self {
        match c {
            AlacColor::Spec(AlacRgb { r, g, b }) => Color { r, g, b },
            AlacColor::Named(name) => named_color_rgb(name),
            AlacColor::Indexed(i) => indexed_color_rgb(i),
        }
    }
}

/// ANSI 16 标准色 + alacritty `NamedColor` 30 个 variant 的 RGB 解析。
///
/// **exhaustive match 无 `_ =>`**:NamedColor 加新 variant 时编译期 catch。
/// 30 行密集映射看起来啰嗦,但**所有调色板决策固化在一处**,Phase 4 字形
/// 渲染 / 未来 theming 都从这一个 fn 改起。
///
/// 调色板取舍:
/// - 0..15(Black..BrightWhite)用 xterm-classic 16 色 RGB(广泛认可的 ANSI 标准)
/// - Foreground/Background/Cursor 用 quill 默认 fg/bg(见 [`Color::DEFAULT_FG`]
///   / [`Color::DEFAULT_BG`])。Cursor 用白色(`#ffffff`)便于和 fg 区分
/// - DimX(Dim 系列)Phase 3 暂用同色名 X(SGR Dim 暗化属性 Phase 3 不渲染)
/// - BrightForeground / DimForeground 退到 DEFAULT_FG
fn named_color_rgb(name: NamedColor) -> Color {
    use NamedColor as N;
    match name {
        // ANSI 16 标准色 (xterm-classic palette)
        N::Black => Color { r: 0, g: 0, b: 0 },
        N::Red => Color { r: 170, g: 0, b: 0 },
        N::Green => Color { r: 0, g: 170, b: 0 },
        N::Yellow => Color {
            r: 170,
            g: 85,
            b: 0,
        },
        N::Blue => Color { r: 0, g: 0, b: 170 },
        N::Magenta => Color {
            r: 170,
            g: 0,
            b: 170,
        },
        N::Cyan => Color {
            r: 0,
            g: 170,
            b: 170,
        },
        N::White => Color {
            r: 170,
            g: 170,
            b: 170,
        },
        N::BrightBlack => Color {
            r: 85,
            g: 85,
            b: 85,
        },
        N::BrightRed => Color {
            r: 255,
            g: 85,
            b: 85,
        },
        N::BrightGreen => Color {
            r: 85,
            g: 255,
            b: 85,
        },
        N::BrightYellow => Color {
            r: 255,
            g: 255,
            b: 85,
        },
        N::BrightBlue => Color {
            r: 85,
            g: 85,
            b: 255,
        },
        N::BrightMagenta => Color {
            r: 255,
            g: 85,
            b: 255,
        },
        N::BrightCyan => Color {
            r: 85,
            g: 255,
            b: 255,
        },
        N::BrightWhite => Color {
            r: 255,
            g: 255,
            b: 255,
        },

        // 特殊角色色:渲染层语义,quill 自己挑默认值
        N::Foreground => Color::DEFAULT_FG,
        N::Background => Color::DEFAULT_BG,
        N::Cursor => Color {
            r: 0xff,
            g: 0xff,
            b: 0xff,
        },

        // Dim 系列:Phase 3 不渲染 SGR Dim 暗化属性,直接用同色名(non-Dim)
        // 等价。Phase 4 加 alpha blending / luminance scaling 时再细化。
        N::DimBlack => Color { r: 0, g: 0, b: 0 },
        N::DimRed => Color { r: 170, g: 0, b: 0 },
        N::DimGreen => Color { r: 0, g: 170, b: 0 },
        N::DimYellow => Color {
            r: 170,
            g: 85,
            b: 0,
        },
        N::DimBlue => Color { r: 0, g: 0, b: 170 },
        N::DimMagenta => Color {
            r: 170,
            g: 0,
            b: 170,
        },
        N::DimCyan => Color {
            r: 0,
            g: 170,
            b: 170,
        },
        N::DimWhite => Color {
            r: 170,
            g: 170,
            b: 170,
        },

        // BrightForeground / DimForeground 退到 DEFAULT_FG(无独立 theme 时
        // 与 Foreground 等价)
        N::BrightForeground | N::DimForeground => Color::DEFAULT_FG,
    }
}

/// xterm 256 色调色板:0..16 走 [`named_color_rgb`] 的 ANSI 16 色,16..232 是
/// 6×6×6 RGB 立方体,232..256 是 24 阶灰度。
///
/// **levels 数组取自 xterm 官方 256colres.pl 输出**(`0, 95, 135, 175, 215, 255`)
/// —— 与 alacritty / kitty / iTerm2 默认调色板一致。换数组就破坏与上游兼容,
/// 用户输入 `\x1b[48;5;25m` 时该看见的 (0, 95, 135) 蓝灰会变形。
fn indexed_color_rgb(i: u8) -> Color {
    if i < 16 {
        // 0..16 复用 NamedColor 的标准 16 色映射(单一 source-of-truth)
        let name = match i {
            0 => NamedColor::Black,
            1 => NamedColor::Red,
            2 => NamedColor::Green,
            3 => NamedColor::Yellow,
            4 => NamedColor::Blue,
            5 => NamedColor::Magenta,
            6 => NamedColor::Cyan,
            7 => NamedColor::White,
            8 => NamedColor::BrightBlack,
            9 => NamedColor::BrightRed,
            10 => NamedColor::BrightGreen,
            11 => NamedColor::BrightYellow,
            12 => NamedColor::BrightBlue,
            13 => NamedColor::BrightMagenta,
            14 => NamedColor::BrightCyan,
            15 => NamedColor::BrightWhite,
            // i < 16 时 0..=15 全覆盖,unreachable 是模式穷尽守卫;若未来 i >= 16
            // 路径变化导致该分支被打到,渲染会得 Black 而非 panic(release 安全)
            _ => NamedColor::Black,
        };
        named_color_rgb(name)
    } else if i < 232 {
        // 6x6x6 cube: idx = 16 + 36*r + 6*g + b, 每分量 0..6
        let levels: [u8; 6] = [0, 95, 135, 175, 215, 255];
        let v = i - 16;
        let r = (v / 36) as usize;
        let g = ((v / 6) % 6) as usize;
        let b = (v % 6) as usize;
        Color {
            r: levels[r],
            g: levels[g],
            b: levels[b],
        }
    } else {
        // 232..=255 灰阶 24 级:xterm 公式 v = 8 + 10 * (i - 232),范围 8..=238
        let v = 8u8.saturating_add(10u8.saturating_mul(i - 232));
        Color { r: v, g: v, b: v }
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

impl TermSize {
    /// 构造 `TermSize`。`new(cols, rows)` 比字段字面量易读,也防止两个 `usize`
    /// 顺序写反(`columns` / `screen_lines` 同类型不报错)。
    /// T-0306 [`TermState::resize`] 内复用,与 [`TermState::new`] 同一构造路径。
    fn new(cols: usize, rows: usize) -> Self {
        Self {
            columns: cols,
            screen_lines: rows,
        }
    }
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
        let size = TermSize::new(cols as usize, rows as usize);
        let config = Config::default();
        Self {
            term: Term::new(config, &size, VoidListener),
            processor: Processor::new(),
            dirty: true,
        }
    }

    /// 改 grid 尺寸为 `cols × rows`。Wayland configure 事件后由
    /// [`crate::wl::window`] 调用,把"surface 像素 → cell 计数"换算结果推给
    /// 终端状态机;同时 [`crate::pty::PtyHandle::resize`] 把同一对 cols/rows
    /// 推给 PTY master(TIOCSWINSZ → SIGWINCH 通知 shell)。两条同步是 T-0306
    /// 的契约。
    ///
    /// **`max(1)` 防 0**:窗口被拖到极小时 surface 像素 / cell px 可能算出 0;
    /// alacritty `Term::resize` 在 cols=0 / rows=0 时内部走除零或越界路径会
    /// panic。1×1 是 grid 不可见但状态机仍合法的最小尺寸,优于 panic 让 quill
    /// 跟着崩。
    ///
    /// **副作用**:置 `self.dirty = true`(沿袭 [`Self::advance`] 模式)。resize
    /// 后即使没新字节,viewport 内容也变了(cells 重新布局,cursor 可能被 alacritty
    /// 内部 clamp),下游 idle callback 看到 dirty 就 [`Self::cells_iter`] +
    /// [`crate::wl::render::Renderer::draw_cells`] 重画一帧。
    ///
    /// **cursor / scrollback 自动跟随**:alacritty `Term::resize` 内部对 grid /
    /// inactive_grid / vi_mode_cursor / selection / scroll_region / damage 都做
    /// 一致更新(参 alacritty_terminal 0.26 `term/mod.rs::resize`),光标缩小时
    /// 自动 clamp 到 `(target - 1)`,scrollback 历史保留。本 fn 不再二次干预。
    ///
    /// 测试覆盖见 `tests::resize_*`(4 单测:dimensions / cursor clamp / zero
    /// clamp / dirty)。
    pub fn resize(&mut self, cols: usize, rows: usize) {
        let size = TermSize::new(cols.max(1), rows.max(1));
        self.term.resize(size);
        self.dirty = true;
    }

    /// 把一批 PTY 字节推进解析器,驱动 grid 更新。
    ///
    /// 上游 `Processor::advance(&mut handler, bytes)` 签名里的 handler 就是
    /// `Term<T>`,`Term` 实现了 `vte::ansi::Handler`。我们作为胶水把两者连起来。
    ///
    /// **副作用**:置 `self.dirty = true`。即使 `bytes` 空切片也置(没改变就
    /// 多画一次,成本小于漏画)。下游 [`is_dirty`] / [`clear_dirty`] 消费。
    ///
    /// **T-0602: 非空字节自动 reset_display 跳到底部**. 用户滚 scrollback 看
    /// 历史时, 子进程吐新输出 (例 cron 日志 / shell 自动 prompt 重画) 应该
    /// 把视图跳回最新, 与 alacritty / xterm / kitty / foot 行为一致 — 否则
    /// 用户看不到新内容只能手动 PgDn. 空 advance (no-op) 不重置, 让 term
    /// dirty / wakeup 路径不影响滚动状态.
    ///
    /// [`is_dirty`]: Self::is_dirty
    /// [`clear_dirty`]: Self::clear_dirty
    pub fn advance(&mut self, bytes: &[u8]) {
        self.processor.advance(&mut self.term, bytes);
        self.dirty = true;
        // T-0602: 新输入到达 → 跳到底 (语义同 alacritty `Event::PtyWrite` 路径).
        if !bytes.is_empty() {
            self.reset_display();
        }
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
        use alacritty_terminal::term::cell::Flags;
        let grid = self.term.grid();
        let row = &grid[Line(line as i32)];
        let cols = grid.columns();
        // 跳过 WIDE_CHAR_SPACER cell: alacritty 协议下 CJK 字符占 2 cells (实字
        // cell + spacer cell, spacer 的 .c 是空格 ' '), 渲染层走 shape_line
        // cascade 时若把 spacer 当独立字符, CJK 视觉宽度变 3 cells (T-0801 force
        // 2× advance + spacer 1 cell), cursor 按 grid col 算反而显得错位。
        // 跳过 spacer 让 row_text 长度 == grid 实际"语义字符数", shape_line 严
        // 按 grid cell 宽度 cascade。
        (0..cols)
            .filter_map(|c| {
                let cell = &row[Column(c)];
                if cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
                    None
                } else {
                    Some(cell.c)
                }
            })
            .collect()
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
    /// 一帧重画 viewport 全部 cell。
    ///
    /// **T-0602 scrollback 接入**: 走 alacritty `display_iter` (它内部已用
    /// `display_offset()` 做 viewport 起点偏移), `display_offset > 0` 时迭代
    /// 起点是 `Line(-display_offset)`, 产出的 `point.line` 为负. CellsIter
    /// `next()` 加上 `display_offset` 偏移把负 line 映射回 `0..rows` viewport
    /// 坐标 — 渲染调用方 (window.rs idle / draw_frame) 拿到的 `pos.line`
    /// 永远在 `0..rows`, 不感知 scrollback. cursor 渲染 (T-0601) 同源走
    /// `cursor_pos()` (alacritty 内部 cursor 坐标永远在 active grid, 不随
    /// scrollback 漂); scrollback 时 `cursor_visible` 返 false (避免 cursor
    /// 画在历史行上, 与 alacritty/foot/kitty 一致).
    ///
    /// `CellRef` 暴露 `pos + c + fg + bg`, T-0305+ 色块渲染 / T-0403+ 字形
    /// 渲染共用一个数据通道。
    pub fn cells_iter(&self) -> CellsIter<'_> {
        CellsIter {
            inner: self.term.grid().display_iter(),
            // T-0602: 把 display_iter 产出的负 line (scrollback) 偏回 viewport.
            // display_offset == 0 时本字段为 0, 行为与 T-0304/T-0305 完全等价.
            display_offset: self.term.grid().display_offset(),
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

    /// 显式置位 dirty (T-0504). 用于 cell 内容未变但 presentation 状态
    /// (CSD titlebar 三按钮 hover 高亮) 需要触发重画的场景 — `wl/window.rs`
    /// `Dispatch<WlPointer>` 在 `PointerAction::HoverChange` 时调.
    ///
    /// **why 不开新 "presentation_dirty" 标志**: term.dirty 唯一作用是触发
    /// idle callback 重画, hover 变化也走重画路径, 语义等价 — 复用既有标志
    /// 不破 INV-006 / INV-007 (handle_event 仍纯逻辑, 本 fn 是对外副作用入口
    /// 与 advance 同性质).
    pub fn mark_dirty(&mut self) {
        self.dirty = true;
    }

    /// 光标是否可见(`TermMode::SHOW_CURSOR` bit)。bash 启动后默认可见;
    /// 某些全屏程序(vim / less)会切 DECRST 25(`ESC[?25l`)隐藏,
    /// DECSET 25(`ESC[?25h`)恢复。T-0305 渲染判断"要不要画光标"。
    ///
    /// **T-0602: scrollback 时强制返 false**. `display_offset > 0` (用户滚到
    /// 历史) 时 alacritty 内部 cursor 仍指向 active grid 的位置, 但视觉上
    /// 应不画 — 否则 cursor 会出现在与当前历史无关的行上. alacritty / foot /
    /// kitty 等终端均如此. 用户输入 (任何键) 触发 PTY 字节回响, `advance` 路径
    /// 自动 `reset_display` 跳到底, cursor 重新可见.
    pub fn cursor_visible(&self) -> bool {
        if self.display_offset() > 0 {
            return false;
        }
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

    // ---------- T-0602 scrollback 滚动 API ----------
    //
    // `cells_iter` / `display_text` / `cursor_visible` 已自动跟随 `display_offset`
    // (cells_iter 内 remap line, display_text 取负 line 偏移, cursor_visible
    // 在 offset > 0 时返 false). 调用方仅需 `scroll_display(±N)` 推 / 拉
    // viewport, 无需理解 alacritty 内部 `Scroll` enum 或负 `Line(i32)` 路径.
    //
    // 类型隔离 (INV-010): 入参出参全 i32 / usize quill 自有类型, 不暴露
    // alacritty `Scroll` / `Line` enum.

    /// 滚动 scrollback viewport. **正值 = 向上滚 (看更老历史), 负值 = 向下滚
    /// (回最新)** — 与 [`crate::wl::keyboard::KeyboardAction::Scroll`] /
    /// [`crate::wl::pointer::PointerAction::Scroll`] 同方向语义.
    ///
    /// 走 alacritty `Term::scroll_display(Scroll::Delta(delta))`, 内部:
    /// - 增 / 减 `grid.display_offset` (clamp 到 `[0, history_size]`)
    /// - 重置 vi-mode cursor 到 viewport 内
    /// - mark_fully_damaged (我们不读 alacritty damage, 自走 dirty 标志)
    ///
    /// **副作用**: 置 `self.dirty = true` — viewport 内容已变 (新行从 history
    /// 进入 viewport), 渲染层下次 idle 看到 dirty 重画.
    ///
    /// **delta=0 是 no-op**: 不滚, 但 mark dirty (与 [`Self::advance`] 空切片
    /// 同保守策略 — 多画一帧 << 漏画). 调用方上层 (Dispatch) 已过滤
    /// 0 line 累积不发, 但本 fn 容忍.
    ///
    /// **clamp 边界**: 正过头 → 截到 `history_size` (最旧历史顶); 负过头 →
    /// 截到 0 (回 viewport active). 不 panic / 不 wrap.
    ///
    /// 测试覆盖见 `tests::scroll_display_*`.
    pub fn scroll_display(&mut self, delta: i32) {
        use alacritty_terminal::grid::Scroll;
        self.term.scroll_display(Scroll::Delta(delta));
        self.dirty = true;
    }

    /// 当前 scrollback 偏移. `0` = 在底 (active grid, 无历史滚出), `>0` = 滚到
    /// 历史区 (值 = 离底多少行); 上限是 [`Self::scrollback_size`].
    ///
    /// 渲染层 / IME cursor_rectangle 路径用此值判断"是否在 scrollback 模式"
    /// (滚动时 IME cursor 跟 cell 不再对齐, 调用方按需做 fallback). T-0602 内部
    /// `cursor_visible` 也读此值在 > 0 时返 false.
    pub fn display_offset(&self) -> usize {
        self.term.grid().display_offset()
    }

    /// 跳到底 (active grid). 等同 `scroll_display(-(display_offset as i32))`,
    /// 但走 alacritty `Scroll::Bottom` 语义更显式 (不依赖 i32 算术), 且 O(1)
    /// 内部直接置 `display_offset = 0`.
    ///
    /// 调用时机: `advance` 收到非空字节 (T-0602 hook, 见 [`Self::advance`] 文档)
    /// — 子进程吐新输出自动跳回最新, 与 alacritty / foot / kitty 一致.
    /// 也可手动调用 (Phase 6+ 加 "End 键跳底" / "Esc 退 scrollback" 时).
    ///
    /// **副作用**: 置 `self.dirty = true` (viewport 内容会从历史回到 active).
    /// `display_offset` 已是 0 时 alacritty 内部 no-op, 但仍置 dirty (语义
    /// 一致, 调用方不必区分"真重置 vs 假重置").
    pub fn reset_display(&mut self) {
        use alacritty_terminal::grid::Scroll;
        self.term.scroll_display(Scroll::Bottom);
        self.dirty = true;
    }

    /// 读取 viewport 内某行 (`0..rows`) 的字符, **跟 [`Self::display_offset`]
    /// 自动同步**. `display_offset == 0` 时等价 [`Self::line_text`]; 滚到历史时
    /// 返 history 行的内容.
    ///
    /// 渲染层 (`window.rs::idle` collect row_texts → `draw_frame`) 应走本方法
    /// 而非 [`Self::line_text`] — 后者直接读 active grid 第 N 行, 不感知 scrollback,
    /// 用户滚到顶却看到 active grid 的内容会"看不到历史" (派单 In #D 真因).
    ///
    /// alacritty `grid[Line(line - display_offset)]` 路径: viewport 行 `row` 在
    /// 滚动时映射到 alacritty `Line(row as i32 - display_offset as i32)`. 当
    /// display_offset=10, row=0 时映射 Line(-10) (10 行前的历史行).
    ///
    /// 越界 row (>= rows) 走 alacritty 内部 grid index, 上面会 panic; 本 API
    /// 不做 bounds check (与 `line_text` 一致, 调用方应用 dimensions 校验).
    pub fn display_text(&self, line: usize) -> String {
        use alacritty_terminal::index::{Column, Line};
        use alacritty_terminal::term::cell::Flags;
        let grid = self.term.grid();
        let alac_line = Line(line as i32 - grid.display_offset() as i32);
        let row = &grid[alac_line];
        let cols = grid.columns();
        // 见 line_text 同款 spacer 跳过逻辑 (修 CJK cursor 错位 bug, T-0801 fix-up)。
        (0..cols)
            .filter_map(|c| {
                let cell = &row[Column(c)];
                if cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
                    None
                } else {
                    Some(cell.c)
                }
            })
            .collect()
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
        (0..cols).map(move |c| {
            let cell = &grid[line][Column(c)];
            CellRef {
                // line=0 占位:scrollback 行没有 viewport line 概念,真实位置走
                // 调用方传入的 ScrollbackPos。详见 fn docstring。
                pos: CellPos { col: c, line: 0 },
                c: cell.c,
                fg: Color::from_alacritty(cell.fg),
                bg: Color::from_alacritty(cell.bg),
            }
        })
    }
}

/// [`TermState::cells_iter`] 的 iterator。把 alacritty 的
/// `GridIterator<Cell>` 的 `Indexed<&Cell>` 重映射成我们自己的 [`CellRef`]
/// —— 隔离上游类型,T-0305 / T-0303 只对本模块 API 编码,不直接抓
/// `alacritty_terminal::term::cell::Cell` / `alacritty::index::Point`。
///
/// **T-0602 scrollback 偏移** (`display_offset` 字段): alacritty `display_iter()`
/// 在 `display_offset > 0` 时迭代起点是 `Line(-display_offset)`, 产出 `Indexed.point.line`
/// 为负 (`-display_offset..-display_offset+screen_lines`). `CellPos::from_alacritty`
/// 的 `.max(0) as usize` 会把所有负 line 全 clamp 到 0, 导致 scrollback 行
/// 全部塞到 viewport 第 0 行. **本字段在 next() 给负 line 加上 display_offset
/// 偏量, 把 scrollback 区域映射回 0..screen_lines viewport 坐标** —— 渲染调用
/// 方 (window.rs idle) 拿到的 pos.line 永远在 viewport 范围, 不感知 scrollback.
pub struct CellsIter<'a> {
    inner: alacritty_terminal::grid::GridIterator<'a, alacritty_terminal::term::cell::Cell>,
    /// 当前 `display_offset` 快照. 0 时本 iterator 行为与 T-0304 等价
    /// (CellPos::from_alacritty 直读). > 0 时 next() 把负 line 加偏量映射回
    /// 0..screen_lines viewport.
    display_offset: usize,
}

impl<'a> Iterator for CellsIter<'a> {
    type Item = CellRef;

    fn next(&mut self) -> Option<Self::Item> {
        self.inner.next().map(|indexed| {
            // T-0602: scrollback 偏移补正 — display_iter 在 display_offset > 0
            // 时产出负 line, 此处加 display_offset 让 line 落回 [0, screen_lines).
            // display_offset == 0 时本加法 no-op (保持 T-0304 路径行为).
            //
            // 仍走模块私有 `CellPos::from_alacritty` / `Color::from_alacritty`,
            // 不经 `From` trait —— 防止 alacritty 类型漏到公共 API
            // (见 `CellPos::from_alacritty` / `Color::from_alacritty` 文档)。
            // T-0305:fg/bg 解析在迭代时一起做,渲染层拿到的就是已解析 RGB,
            // 不再分支 Spec/Named/Indexed。
            let mut pos_alac = indexed.point;
            pos_alac.line += self.display_offset;
            CellRef {
                pos: CellPos::from_alacritty(pos_alac),
                c: indexed.cell.c,
                fg: Color::from_alacritty(indexed.cell.fg),
                bg: Color::from_alacritty(indexed.cell.bg),
            }
        })
    }
}

/// 渲染用 cell 引用。T-0305 加 `fg` / `bg` 字段(quill [`Color`],已解析 RGB),
/// 给色块渲染 + Phase 4 字形渲染共用一个数据通道。
///
/// **fg vs bg 渲染语义**(见 `crate::wl::render::Renderer::draw_cells`):
/// - Phase 3 色块渲染用 **fg** 着色非空 cell —— 视觉等价于"字符位置以 fg 色块
///   占位"(没有真字形,先以 fg 色矩形代表"这里有内容");bash prompt 在
///   深蓝清屏上显示为 light gray 离散块,符合 T-0305 acceptance"看见 prompt
///   字符位置以色块画出"
/// - Phase 4 字形渲染:bg 画 cell 全色块(覆盖该 cell 区域),fg 画 glyph
///   纹理。bg 字段在本 ticket 内已存好,Phase 4 不破 API
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CellRef {
    /// cell 在 viewport 里的位置。见 [`CellPos`]。
    pub pos: CellPos,
    /// cell 里的字符。空 cell 是 `' '`(空格),下游按需判空。
    pub c: char,
    /// 前景色(已解析 RGB)。Phase 3 色块渲染时 cell 矩形按 fg 着色。
    /// 默认 [`Color::DEFAULT_FG`](`#d3d3d3` light gray),vim/git 等程序
    /// 用 SGR 38 改写。
    pub fg: Color,
    /// 背景色(已解析 RGB)。Phase 3 字段已存但 [`crate::wl::render::Renderer::draw_cells`]
    /// 暂不画;Phase 4 字形渲染时用作 cell 全色块。默认 [`Color::DEFAULT_BG`]
    /// (`#000000` 黑),vim status line / less 反色等用 SGR 48 改写。
    pub bg: Color,
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

    // ---------- T-0305 Color 类型单测 ----------

    /// `Color::from_alacritty(Spec)` 直接透传 RGB,不做任何调色板查表。
    #[test]
    fn color_from_alacritty_spec_passes_rgb() {
        let c = Color::from_alacritty(AlacColor::Spec(AlacRgb {
            r: 0x12,
            g: 0x34,
            b: 0x56,
        }));
        assert_eq!(
            c,
            Color {
                r: 0x12,
                g: 0x34,
                b: 0x56
            }
        );
    }

    /// `Color::from_alacritty(Named(Red))` 解析到 ANSI 标准红 (170, 0, 0)。
    /// 防回归:换调色板等于改用户文字色,不能"顺手"调亮 / 调暗。
    /// 同时验另一个高频色 BrightGreen → (85, 255, 85)(`ls --color` 目录色)。
    #[test]
    fn color_from_alacritty_named_resolves_to_palette() {
        assert_eq!(
            Color::from_alacritty(AlacColor::Named(NamedColor::Red)),
            Color { r: 170, g: 0, b: 0 },
            "ANSI Named::Red → xterm-classic (170, 0, 0)"
        );
        assert_eq!(
            Color::from_alacritty(AlacColor::Named(NamedColor::BrightGreen)),
            Color {
                r: 85,
                g: 255,
                b: 85
            },
            "ANSI Named::BrightGreen → xterm-classic (85, 255, 85)"
        );
        // Foreground / Background 走 quill 自定 default,不走标准色;锁住默认值。
        assert_eq!(
            Color::from_alacritty(AlacColor::Named(NamedColor::Foreground)),
            Color::DEFAULT_FG,
            "Named::Foreground 应解析到 quill DEFAULT_FG"
        );
        assert_eq!(
            Color::from_alacritty(AlacColor::Named(NamedColor::Background)),
            Color::DEFAULT_BG,
            "Named::Background 应解析到 quill DEFAULT_BG"
        );
    }

    /// `Color::from_alacritty(Indexed)` 三档(0..16 / 16..232 cube / 232..256 灰阶)
    /// 各自验一个代表点,锁住调色板。xterm 256colres.pl 的标准值,任何"美化"
    /// 改动会破坏与上游兼容。
    #[test]
    fn color_from_alacritty_indexed_lookup() {
        // 0..16 复用 NamedColor 路径
        assert_eq!(
            Color::from_alacritty(AlacColor::Indexed(1)),
            Color { r: 170, g: 0, b: 0 },
            "Indexed(1) == Red"
        );
        assert_eq!(
            Color::from_alacritty(AlacColor::Indexed(15)),
            Color {
                r: 255,
                g: 255,
                b: 255
            },
            "Indexed(15) == BrightWhite"
        );

        // 6x6x6 cube: idx 16 应是 (0, 0, 0)(全零);idx 231 应是 (255, 255, 255)
        assert_eq!(
            Color::from_alacritty(AlacColor::Indexed(16)),
            Color { r: 0, g: 0, b: 0 },
            "Indexed(16) cube 起点应 (0,0,0)"
        );
        assert_eq!(
            Color::from_alacritty(AlacColor::Indexed(231)),
            Color {
                r: 255,
                g: 255,
                b: 255
            },
            "Indexed(231) cube 末端应 (255,255,255)"
        );
        // idx 25 公式: v=9, r=0, g=1, b=3 → (0, 95, 175)
        assert_eq!(
            Color::from_alacritty(AlacColor::Indexed(25)),
            Color {
                r: 0,
                g: 95,
                b: 175
            },
            "Indexed(25) cube 应 (0, 95, 175)"
        );

        // 灰阶: idx 232 应 (8, 8, 8); idx 255 应 (8 + 10*23, ...) = (238, 238, 238)
        assert_eq!(
            Color::from_alacritty(AlacColor::Indexed(232)),
            Color { r: 8, g: 8, b: 8 },
            "Indexed(232) 灰阶起点"
        );
        assert_eq!(
            Color::from_alacritty(AlacColor::Indexed(255)),
            Color {
                r: 238,
                g: 238,
                b: 238
            },
            "Indexed(255) 灰阶末端"
        );
    }

    /// `cells_iter` 产出的 `CellRef` 应携 fg/bg 字段。新构造 TermState 时所有
    /// cell 都是 alacritty default(`fg=Named(Foreground)` / `bg=Named(Background)`),
    /// 走 `Color::from_alacritty` 后应是 `DEFAULT_FG` / `DEFAULT_BG`。
    ///
    /// 这个测试同时锁住 cells_iter 真填充而非偷塞默认 (T-0305 acceptance:
    /// "cells_iter 真填充")。
    #[test]
    fn cellref_carries_fg_and_bg() {
        let t = TermState::new(80, 24);
        let cell = t
            .cells_iter()
            .next()
            .expect("80x24 viewport 至少应产 1 个 cell");
        assert_eq!(
            cell.fg,
            Color::DEFAULT_FG,
            "默认 cell.fg 应解析到 DEFAULT_FG (Named::Foreground)"
        );
        assert_eq!(
            cell.bg,
            Color::DEFAULT_BG,
            "默认 cell.bg 应解析到 DEFAULT_BG (Named::Background)"
        );

        // scrollback_cells_iter 同样应填 fg/bg(T-0304 路径也走过 Color::from_alacritty)。
        // 推 50 行触发 scrollback,验 row=0 的 cell 也带 fg/bg(应仍是 default,
        // bash 没跑就没颜色 escape)。
        let mut t2 = TermState::new(80, 24);
        for i in 0..50 {
            let line = format!("line_{:02}\r\n", i);
            t2.advance(line.as_bytes());
        }
        let scroll_cell = t2
            .scrollback_cells_iter(ScrollbackPos { row: 0 })
            .next()
            .expect("scrollback row=0 应至少 1 个 cell");
        assert_eq!(
            scroll_cell.fg,
            Color::DEFAULT_FG,
            "scrollback cell.fg 应解析"
        );
        assert_eq!(
            scroll_cell.bg,
            Color::DEFAULT_BG,
            "scrollback cell.bg 应解析"
        );
    }

    // ---------- T-0306 resize 单测 ----------

    /// `resize(cols, rows)` 后 `dimensions()` 应反映新尺寸。是 T-0306 与
    /// `crate::wl::window` configure callback 之间最小契约 ——
    /// "调 resize 后 cells_iter 数量 = 新 cols × 新 rows"。
    #[test]
    fn resize_changes_dimensions() {
        let mut t = TermState::new(80, 24);
        assert_eq!(t.dimensions(), (80, 24), "前置: ctor 80x24");

        t.resize(100, 30);
        assert_eq!(t.dimensions(), (100, 30), "resize 后应反映新尺寸");
        // 二次复验:cells_iter 数量也应跟随
        assert_eq!(
            t.cells_iter().count(),
            100 * 30,
            "cells_iter 应产 100 × 30 = {} cells",
            100 * 30
        );

        // 反向(变小)
        t.resize(40, 12);
        assert_eq!(t.dimensions(), (40, 12));
        assert_eq!(t.cells_iter().count(), 40 * 12);
    }

    /// 缩小到光标当前位置之外时,alacritty `Term::resize` 内部 `grid.shrink_*`
    /// 自动 clamp 光标到 `(target_cols - 1, target_rows - 1)`。本测试锁住
    /// "resize 后 cursor 不会留在越界位置"—— 渲染层假设 `cursor_pos.col <
    /// cols && cursor_pos.line < rows`,违反会画到屏幕外。
    ///
    /// 推 11 行字 (line 0..10 各写若干字符),cursor 落在 (col≈3, line=10)
    /// 附近;resize 到 30×5 后,line ≤ 4 + col ≤ 29 必须成立。
    #[test]
    fn resize_to_smaller_clamps_cursor() {
        let mut t = TermState::new(80, 24);
        // 11 行后 cursor 落在 line=10。每行写 'X' 一次再换行,行号定值。
        for _ in 0..11 {
            t.advance(b"X\r\n");
        }
        let pre = t.cursor_pos();
        assert!(
            pre.line >= 10,
            "前置: 11 行后 cursor.line 应 >= 10, 实际 {}",
            pre.line
        );

        t.resize(30, 5);
        let post = t.cursor_pos();
        assert!(
            post.col < 30,
            "resize(30, 5) 后 cursor.col 必须 < 30, 实际 {}",
            post.col
        );
        assert!(
            post.line < 5,
            "resize(30, 5) 后 cursor.line 必须 < 5, 实际 {}",
            post.line
        );
    }

    /// `resize(0, 0)` 不该 panic —— compositor 极端情况下拍来 0×0 surface
    /// (窗口被拖到极小 / 隐藏前一瞬)。`max(1)` 防 0 兜底,落到 1×1 而非
    /// 让 alacritty `Term::resize` 内部除零 / 越界 panic。
    ///
    /// 同时验 (0, 5) / (5, 0) 单维度为 0 的边界(派单原文"cols/rows 至少 1
    /// (max(1) 防 0 panic)")。
    #[test]
    fn resize_zero_clamped_to_one() {
        let mut t = TermState::new(80, 24);
        t.resize(0, 0);
        assert_eq!(t.dimensions(), (1, 1), "(0,0) 应 clamp 到 (1,1)");

        let mut t2 = TermState::new(80, 24);
        t2.resize(0, 5);
        assert_eq!(t2.dimensions(), (1, 5), "(0,5) 应 clamp 到 (1,5)");

        let mut t3 = TermState::new(80, 24);
        t3.resize(5, 0);
        assert_eq!(t3.dimensions(), (5, 1), "(5,0) 应 clamp 到 (5,1)");
    }

    // ---------- T-0602 scrollback 滚动 API 单测 ----------

    /// 初始 display_offset 为 0 (在底, viewport 显示 active grid).
    #[test]
    fn display_offset_zero_initially() {
        let t = TermState::new(80, 24);
        assert_eq!(t.display_offset(), 0);
    }

    /// 写超 viewport 后 scroll_display(+N) 增 display_offset, 走 alacritty
    /// `Scroll::Delta(+)` 路径; clamp 到 history_size 上限.
    #[test]
    fn scroll_display_positive_increments_offset() {
        let mut t = TermState::new(80, 24);
        for i in 0..50 {
            t.advance(format!("line_{:02}\r\n", i).as_bytes());
        }
        let history = t.scrollback_size();
        assert!(history >= 25, "前置: scrollback >= 25");

        // 滚 5 line
        t.scroll_display(5);
        assert_eq!(
            t.display_offset(),
            5,
            "scroll_display(+5) 应让 display_offset = 5"
        );
        // 再滚 5
        t.scroll_display(5);
        assert_eq!(t.display_offset(), 10);

        // clamp: 滚到顶
        t.scroll_display(99999);
        assert_eq!(
            t.display_offset(),
            history,
            "正向过头应 clamp 到 history_size = {}",
            history
        );
    }

    /// 负 scroll_display 减 display_offset, clamp 到 0.
    #[test]
    fn scroll_display_negative_decrements_offset_and_clamps_to_zero() {
        let mut t = TermState::new(80, 24);
        for i in 0..50 {
            t.advance(format!("line_{:02}\r\n", i).as_bytes());
        }
        t.scroll_display(20);
        assert!(t.display_offset() > 0, "前置: 应已滚到历史");

        t.scroll_display(-5);
        assert_eq!(t.display_offset(), 15);

        // 负过头 clamp 到 0
        t.scroll_display(-9999);
        assert_eq!(t.display_offset(), 0, "负向过头应 clamp 到 0");
    }

    /// scroll_display 应置 dirty (重画路径需要).
    #[test]
    fn scroll_display_sets_dirty() {
        let mut t = TermState::new(80, 24);
        for i in 0..50 {
            t.advance(format!("line_{:02}\r\n", i).as_bytes());
        }
        t.clear_dirty();
        assert!(!t.is_dirty(), "前置 clean");
        t.scroll_display(5);
        assert!(t.is_dirty(), "scroll_display 后应 dirty");
    }

    /// reset_display 把 display_offset 直接置 0 (跳到底).
    #[test]
    fn reset_display_jumps_to_bottom() {
        let mut t = TermState::new(80, 24);
        for i in 0..50 {
            t.advance(format!("line_{:02}\r\n", i).as_bytes());
        }
        t.scroll_display(10);
        assert_eq!(t.display_offset(), 10, "前置: 滚到 10");
        t.reset_display();
        assert_eq!(t.display_offset(), 0, "reset 后应跳到底");
    }

    /// **T-0602 关键**: advance 收非空字节自动 reset_display, 子进程吐新输出
    /// 时用户视图自动跳回最新 — 与 alacritty / xterm 一致.
    #[test]
    fn advance_with_nonempty_bytes_auto_resets_display() {
        let mut t = TermState::new(80, 24);
        for i in 0..50 {
            t.advance(format!("line_{:02}\r\n", i).as_bytes());
        }
        t.scroll_display(10);
        assert_eq!(t.display_offset(), 10, "前置: 滚到 10");

        // 喂新字节
        t.advance(b"NEW");
        assert_eq!(
            t.display_offset(),
            0,
            "新输入到达应自动 reset_display 跳到底"
        );
    }

    /// 空 advance 不重置 display (no-op 不该影响滚动状态).
    #[test]
    fn advance_with_empty_bytes_does_not_reset_display() {
        let mut t = TermState::new(80, 24);
        for i in 0..50 {
            t.advance(format!("line_{:02}\r\n", i).as_bytes());
        }
        t.scroll_display(10);
        assert_eq!(t.display_offset(), 10);
        t.advance(b"");
        assert_eq!(t.display_offset(), 10, "空 advance 不该改 display_offset");
    }

    /// **T-0602 cells_iter 关键**: 滚动后 cells_iter 产出的 pos.line 仍在
    /// `0..rows`, 内容来自历史行 (派单 In #D 真因 — `CellPos::from_alacritty`
    /// 的 `.max(0) as usize` 不再让所有 scrollback row 全塌到 line 0).
    #[test]
    fn cells_iter_after_scroll_keeps_viewport_lines_and_shows_history() {
        let mut t = TermState::new(80, 24);
        for i in 0..50 {
            t.advance(format!("L{:02}\r\n", i).as_bytes());
        }
        // 滚 10 line 看历史
        t.scroll_display(10);
        let cells: Vec<_> = t.cells_iter().collect();
        // 数量仍 80 × 24 (viewport 不变)
        assert_eq!(cells.len(), 80 * 24, "cells_iter 数量永远 = cols × rows");
        // 每个 pos.line 必须在 [0, rows)
        for cr in &cells {
            assert!(
                cr.pos.line < 24,
                "scrollback 后 pos.line 应仍 < rows, 实际 {}",
                cr.pos.line
            );
        }
        // 第 0 行字符应来自历史 (不是 active 最末行); 取第 0 行 80 个 cell 拼字符串
        let row0: String = cells
            .iter()
            .filter(|cr| cr.pos.line == 0)
            .map(|cr| cr.c)
            .collect();
        assert!(
            row0.starts_with("L"),
            "scroll_display(10) 后 viewport 第 0 行应是某历史行 'L..', 实际: {:?}",
            &row0[..10.min(row0.len())]
        );
    }

    /// display_text 跟 display_offset 同步.
    #[test]
    fn display_text_follows_display_offset() {
        let mut t = TermState::new(80, 24);
        for i in 0..50 {
            t.advance(format!("L{:02}\r\n", i).as_bytes());
        }
        let active_row0 = t.display_text(0);
        // active row 0 应是最末 24 行的开头
        assert!(
            active_row0.starts_with("L"),
            "active 第 0 行应是某 'L..', 实际: {:?}",
            &active_row0[..10.min(active_row0.len())]
        );

        t.scroll_display(10);
        let scrolled_row0 = t.display_text(0);
        // 滚后 row 0 应是更早的历史行, 与 active 不同
        assert_ne!(
            active_row0, scrolled_row0,
            "scroll_display 后 display_text(0) 应换内容"
        );
        assert!(scrolled_row0.starts_with("L"));
    }

    /// cursor_visible 在 scrollback 时应返 false (与 alacritty / foot / kitty 一致).
    #[test]
    fn cursor_visible_false_when_scrolled() {
        let mut t = TermState::new(80, 24);
        for i in 0..50 {
            t.advance(format!("L{:02}\r\n", i).as_bytes());
        }
        assert!(t.cursor_visible(), "前置: 在底时 cursor 可见");
        t.scroll_display(5);
        assert!(
            !t.cursor_visible(),
            "scrollback (display_offset > 0) 时 cursor 应隐藏"
        );
        t.reset_display();
        assert!(t.cursor_visible(), "回底后 cursor 应重新可见");
    }

    /// `resize` 后必须置 `dirty=true`(沿袭 [`advance`] 模式)。clear → resize
    /// → is_dirty == true。下游 idle callback 看到 dirty 才会 [`draw_cells`]
    /// 重画一帧;若忘了置,resize 后屏幕保持旧布局直到下次字节进 term 才更新,
    /// 视觉上看着像"resize 没反应"。
    ///
    /// [`advance`]: TermState::advance
    /// [`draw_cells`]: crate::wl::render::Renderer::draw_cells
    #[test]
    fn resize_sets_dirty() {
        let mut t = TermState::new(80, 24);
        t.clear_dirty();
        assert!(!t.is_dirty(), "前置: clear 后应 clean");

        t.resize(100, 30);
        assert!(t.is_dirty(), "resize 后应 dirty");

        // 即使尺寸不变也置 dirty(保守, 与 advance 空切片同思路 —— 多画一帧
        // 比漏画强)。alacritty Term::resize 在尺寸不变时内部 early return
        // (debug log "Term::resize dimensions unchanged"), 但我们仍置 dirty
        // 让调用方不必区分 "真 resize / 假 resize"。
        t.clear_dirty();
        t.resize(100, 30);
        assert!(
            t.is_dirty(),
            "resize 到相同尺寸也应置 dirty (调用方一致语义)"
        );
    }
}
