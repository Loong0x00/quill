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
//! - `cursor_point()` 给 trace / 未来渲染用,返回 `(column, line)`
//!   —— 注意 line 可以是负数(scrollback 历史)但 Phase 3 暂不触发

use alacritty_terminal::event::VoidListener;
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::term::{Config, Term};
use alacritty_terminal::vte::ansi::Processor;

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
}

impl TermState {
    /// 起一个初始尺寸为 `cols × rows` 的终端。`cols`/`rows` 由上游(T-0202
    /// 写死的 80×24,Phase 3 T-0306 才接 Wayland resize)传进来。
    pub fn new(cols: u16, rows: u16) -> Self {
        let size = TermSize {
            columns: cols as usize,
            screen_lines: rows as usize,
        };
        let config = Config::default();
        Self {
            term: Term::new(config, &size, VoidListener),
            processor: Processor::new(),
        }
    }

    /// 把一批 PTY 字节推进解析器,驱动 grid 更新。
    ///
    /// 上游 `Processor::advance(&mut handler, bytes)` 签名里的 handler 就是
    /// `Term<T>`,`Term` 实现了 `vte::ansi::Handler`。我们作为胶水把两者连起来。
    pub fn advance(&mut self, bytes: &[u8]) {
        self.processor.advance(&mut self.term, bytes);
    }

    /// 返回当前光标位置 `(column, line_offset_from_top_of_screen)`。
    ///
    /// - `column` 是 0-based 列号(left = 0)
    /// - `line` 是 0-based 行号,相对当前 viewport(不含 scrollback offset);
    ///   typical bash prompt 刚出来时是 `(prompt_len, 0)`
    ///
    /// 返回 `i32` 而不是 `usize`:alacritty 的 `Line` 内部是 i32,-n 表示
    /// scrollback 历史。当前 Phase 3 暂不触发负数;保留原始类型少一次 lossy
    /// cast。
    pub fn cursor_point(&self) -> (usize, i32) {
        let point = self.term.grid().cursor.point;
        (point.column.0, point.line.0)
    }

    /// 读取指定行(screen-line `0..screen_lines`)的字符,作为 `String` 返回。
    /// 末尾空白不 trim,调用方自己判断。主要给集成测试 / 调试查 grid 内容。
    ///
    /// 给 T-0302 写字节 → grid 断言时用;Phase 3 T-0305 真渲染时会走
    /// 更直接的 cell-level API,不调本方法。
    pub fn line_text(&self, line: usize) -> String {
        use alacritty_terminal::index::{Column, Line};
        let grid = self.term.grid();
        let row = &grid[Line(line as i32)];
        let cols = grid.columns();
        (0..cols).map(|c| row[Column(c)].c).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// T-0301 冒烟:构造一个 80×24 的 Term,喂 `"hi\n"`,光标应从 (0,0) 前进到
    /// 下一行列首(`\n` 在 Term 里走 Index + CarriageReturn 语义 —— 实际就是
    /// 新行列 0)。这一条最小验证 `advance` 通路接通,不依赖 PTY / wayland。
    #[test]
    fn advance_hi_newline_moves_cursor() {
        let mut t = TermState::new(80, 24);
        assert_eq!(t.cursor_point(), (0, 0), "初始光标应在 (0, 0)");

        t.advance(b"hi");
        let (col, line) = t.cursor_point();
        assert_eq!(line, 0, "'hi' 没换行, 光标应仍在第 0 行");
        assert_eq!(col, 2, "'hi' 写两个字,列号到 2");

        // \r\n: CR 回列首, LF index 到下一行。alacritty 对 LF 默认 linefeed
        // 不附带 CR, 实际终端 onlcr 由 tty 层翻译;本测试走 \r\n 模拟 PTY
        // 真正吐给我们的字节。
        t.advance(b"\r\n");
        let (col, line) = t.cursor_point();
        assert_eq!(col, 0, "CRLF 后列应回 0");
        assert_eq!(line, 1, "CRLF 后行应 +1");
    }

    /// 防回归:构造后立即 advance 空切片不 panic,不改动光标。
    #[test]
    fn advance_empty_is_noop() {
        let mut t = TermState::new(80, 24);
        t.advance(b"");
        assert_eq!(t.cursor_point(), (0, 0));
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
        assert_ne!(t.cursor_point(), (0, 0), "pre: 'xyz' 后不应仍在 0,0");

        t.advance(b"\x1b[H\x1b[2J"); // ESC[H 回家 + ESC[2J 清屏
        assert_eq!(t.cursor_point(), (0, 0), "ESC[H 应把光标回 (0, 0)");
    }
}
