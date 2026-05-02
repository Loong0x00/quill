# T-0807 Unicode 宽度 / spacer 协议收口 (M1+M2+m1+n1)

**Phase**: 8 (polish++, terminal correctness)
**Assigned**: TBD (writer)
**Status**: open
**Budget**: tokenBudget=100k (text/mod.rs glyph cluster + term/mod.rs scrollback API + window.rs closure + render.rs preedit underline + 测试)
**Dependencies**: 无 (T-0803/T-0804/T-0805/T-0805 hotfix 已 merge)
**Priority**: P1 (M2 是用户高频复制场景, M1 是 T-0803 收口未完, m1 视觉, n1 文档)

## 背景

Codex review 在 e12f276 (T-0805 hotfix) 后对 quill 最近 10 commit 做只读审查, 发现 4 处同源缺陷, 共同主题是 **Unicode 宽度 / spacer 协议各处不一致**。本 ticket 一次性收口。

## Bug 列表

### M1 — `force_cjk_double_advance` 对零宽 / 共享 cluster 错误推 cursor

`src/text/mod.rs:519-549`. 当前实现:

```rust
for g in glyphs {
    let ch = original.get(g.cluster..).and_then(|s| s.chars().next()).unwrap_or(' ');
    let is_wide = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(1) >= 2;
    let forced_advance = if is_wide { double_w } else { cell_w_phys };
    // ...
    cursor_x += forced_advance;
}
```

问题:
1. **零宽字符**: `width(ch) == 0` (combining mark / U+200D ZWJ / variation selector) 走 `unwrap_or(1)` fallback → 仍按 1 cell 推 → 把 base char 后的零宽 mark 当独立字 1 cell 算 → 视觉宽度膨胀
2. **共享 cluster**: 单 cluster 多 glyph (ZWJ emoji 序列 / 复合连字 / 复合字) 当前每 glyph 重复推 forced_advance → cursor 严重前进过头

举例:
- `"a\u{0301}"` (a + combining acute, 视觉 1 cell): 当前推 1+1=2 cell, 错位 1 cell
- `"👨\u{200D}👩\u{200D}👧"` (家庭 emoji, 视觉 2 cell): cosmic-text shape 出 1~5 glyph (视 face), cluster 通常都 = 0, 当前推 N×2 cell, 错位巨大

### M2 — 跨 scrollback 复制 spacer 协议不一致

`src/wl/window.rs:1256-1282` (closure) + `src/term/mod.rs:1003-1019` (`display_text_with_spacers`) + `src/term/mod.rs:1060-1066` (`scrollback_line_text`).

closure 在 `line >= 0` 走 `display_text_with_spacers` (WIDE_CHAR_SPACER → `\0`, 外层 `replace('\0', "")` 去除), 在 `line < 0` 走 `scrollback_line_text` (WIDE_CHAR_SPACER 的 `cell.c` 是 alacritty 默认填的空格 `' '`, 直接进字符串)。

`extract_selection_text` 拼出来的字符串外层只 replace `\0`, 不去空格 → 跨 scrollback 复制 CJK 行 (例 `你好世界`) 会变成 `你 好 世 界 ` (每个 CJK 后多 1 空格) 而 viewport 行不会。复制行为视行所在位置不同, 与 user 实测预期 (与屏显一致) 不符。

### m1 — preedit 下划线宽度按 char 数算, CJK 偏短

`src/wl/render.rs:3629-3657`. `append_preedit_underline_to_cell_bytes` 接 `char_count: usize`, 调用方传 `preedit_text.chars().count()`. CJK preedit (例 `"今天"` 2 char / 4 cell) 下划线只画 2 cell 宽。

注释自己也承认 "Phase 6 接 east-asian width 表精确算", 现在 unicode-width crate 已经在依赖 (T-0803 引入), 顺手补。

### n1 — `extract_selection_text` 文档过时

`src/wl/selection.rs:404-407` 文档:

> `>= 0` 表 viewport 内行, 调用方应返 [`crate::term::TermState::display_text_with_spacers`]

T-0805 hotfix e12f276 实际协议是 callsite 先 `viewport_line = line + display_offset` 再调 `display_text_with_spacers`. 文档没同步, 后续按文档加 callsite 的话会重新引入坐标错位。

## Goal

Unicode 宽度判定 (M1) + scrollback/viewport spacer 协议 (M2) + preedit underline cell 数 (m1) + 文档准确性 (n1) 一次收口, 让"看到的就是复制到的"在所有路径上一致。

## Scope

### In

#### A. M1 — `force_cjk_double_advance` 按 cluster 聚合 + 零宽不推 (`src/text/mod.rs`)

修改语义: cursor_x 推进按 **cluster** (不是按 glyph), 且 cluster 总宽以 cluster 内**所有 char 的 unicode-width 之和**决定:

```rust
fn force_cjk_double_advance(glyphs: Vec<ShapedGlyph>, original: &str) -> Vec<ShapedGlyph> {
    if glyphs.is_empty() { return glyphs; }
    let cell_w_phys = crate::wl::CELL_W_PX * (crate::wl::HIDPI_SCALE as f32);

    let mut out: Vec<ShapedGlyph> = Vec::with_capacity(glyphs.len());
    let mut cursor_x: f32 = 0.0;
    let mut last_cluster: Option<usize> = None;
    let mut cluster_advance: f32 = 0.0;

    for g in glyphs {
        // 进入新 cluster: 累加上一 cluster 的 advance
        if Some(g.cluster) != last_cluster {
            if last_cluster.is_some() {
                cursor_x += cluster_advance;
            }
            // 新 cluster: 计算其 width (sum 所有 char 的 unicode-width, 取下一
            // cluster 起点之前的所有 char). 简化: 找 cluster 起点到原文末/下一
            // cluster 起点的 substring, sum width.
            let cluster_str = cluster_substring(original, g.cluster, /* glyphs 提供的下一 cluster 起点 */);
            let cluster_cells = unicode_width::UnicodeWidthStr::width(cluster_str).max(1);
            cluster_advance = (cluster_cells as f32) * cell_w_phys;
            last_cluster = Some(g.cluster);
        }
        // 同 cluster 多 glyph: 共享同 cursor_x 起点, 不再推
        let is_wide = cluster_advance >= 2.0 * cell_w_phys - 0.01;
        let center_pad = if is_wide { (cluster_advance - g.x_advance).max(0.0) / 2.0 } else { 0.0 };
        out.push(ShapedGlyph {
            x_advance: cluster_advance,  // 仅 cluster 第一 glyph 推, 其余 0?
            x_offset: cursor_x + center_pad,
            ..g
        });
    }
    if last_cluster.is_some() {
        cursor_x += cluster_advance;  // 最后一 cluster
    }
    out
}
```

实现细节由 writer 决定 (上面是示意, 不是规范代码), 但必须满足:
- **零宽 char (width=0)**: 不单独推 cursor_x, 计入所属 cluster 的 sum (sum 通常仍是 base char 的 1 或 2)
- **共享 cluster 多 glyph**: cursor_x 只推一次 cluster_advance, 不重复
- **下一 cluster 边界**: glyphs 是按 cluster 单调递增 (cosmic-text/HarfBuzz 协议), 下一 cluster 起点 = 下一 glyph 的 g.cluster (或 original.len() 结尾)
- **空 cluster_str (cluster 越界)**: width = 0 时 fallback 1 cell (防退化为 0 宽影响 cursor)

`x_advance` 字段语义微调 (元数据用, 渲染层只看 `x_offset`): 第一 glyph 存 cluster_advance, 后续 glyph 存 0, 让 sum 仍等于 cluster_advance — 或者每 glyph 都存 cluster_advance/N (writer 选 KISS 方案, 测试验 sum == 总宽)。

测试 (新增, 在现有 `force_cjk_double_advance` 测试旁):
- `combining_mark_no_extra_advance` — `"a\u{0301}"` 推 1 cell 不是 2 cell
- `zwj_emoji_family_single_cluster_advance` — `"👨\u{200D}👩\u{200D}👧"` 总 advance == 2 cell (单 cluster wide emoji 视觉 1 emoji 宽 = 2 cell). 若 cosmic-text 给的 cluster 不是单一, 测试改为验"cluster 内所有 glyph 共享 x_offset 起点"
- `variation_selector_no_extra_advance` — `"⚠\u{FE0F}"` (warning sign + emoji presentation selector) 推 2 cell (emoji presentation), 不是 3 cell

#### B. M2 — `scrollback_line_text_with_spacers` 镜像 viewport 协议

`src/term/mod.rs` 在 `scrollback_line_text` 后加新 fn:

```rust
/// 同 [`Self::scrollback_line_text`] 但 WIDE_CHAR_SPACER cell 用 `'\0'` 占位,
/// 镜像 [`Self::display_text_with_spacers`] viewport 路径协议. 给 selection
/// 跨 scrollback 复制用, 调用方事后 `replace('\0', "")` 跟 viewport 路径一致.
pub fn scrollback_line_text_with_spacers(&self, pos: ScrollbackPos) -> String {
    use alacritty_terminal::index::Column;
    use alacritty_terminal::term::cell::Flags;
    let grid = self.term.grid();
    let line = pos.to_alacritty(grid.history_size());
    let row = &grid[line];
    let cols = grid.columns();
    (0..cols).map(|c| {
        let cell = &row[Column(c)];
        if cell.flags.contains(Flags::WIDE_CHAR_SPACER) { '\0' } else { cell.c }
    }).collect()
}
```

`src/wl/window.rs:1277` 改:
```rust
t.scrollback_line_text(crate::term::ScrollbackPos { row })
// → 
t.scrollback_line_text_with_spacers(crate::term::ScrollbackPos { row })
```

外层 `replace('\0', "")` (window.rs:1282) 不变, 现在两条路径输出格式一致。

测试 (term/mod.rs::tests):
- `scrollback_line_text_with_spacers_cjk_uses_nul_marker` — 写 `"你好"` 到 PTY → 滚出 viewport → `scrollback_line_text_with_spacers(...)` 返 `"你\0好\0..."` (跟 `display_text_with_spacers` 同)
- 现有 `scrollback_line_text_*` 测试不动 (它仍是"调试 / 人工查"用)

测试 (window.rs::tests 或 selection.rs::tests, e2e 风):
- `selection_copy_across_scrollback_cjk_no_extra_spaces` — mock TermState 让 viewport top 与 scrollback bottom 都有 CJK 行, 选区跨边界, 验 extract 结果不含连续空格 (具体断言 writer 决定, 关键是与"屏幕显示"一致)

#### C. m1 — preedit 下划线宽度按 cell 数

`src/wl/render.rs`:
- `append_preedit_underline_to_cell_bytes` 参数 `char_count: usize` 改名 `cell_count: usize`, 改注释删掉 "char count 偏小" tradeoff 段
- 找所有调用点 (grep `append_preedit_underline_to_cell_bytes`), 把传值从 `preedit_text.chars().count()` 改 `unicode_width::UnicodeWidthStr::width(preedit_text)`
- 同步 `render_headless` 路径如果有 inline 实现 (注释里说 "render_headless preedit 走自己的 inline 实现")

测试:
- 现有 preedit 测试加一个 CJK case, 验下划线 cell 数 == width 而非 chars().count()。如果 preedit underline 没单测 (注释说 inline), 至少加一个对 `unicode_width::UnicodeWidthStr::width("今天")` == 4 的 sanity 断言

#### D. n1 — `extract_selection_text` 文档同步

`src/wl/selection.rs:404-407` 文档块:

```
- `>= 0` 表 viewport 内行, 调用方应返 [`crate::term::TermState::display_text_with_spacers`]
```

改为:

```
- `>= 0` 表 viewport-absolute 行 (origin = display_offset=0 时 viewport 顶), 调用方应:
  1. 算 `viewport_line = line + display_offset` (T-0805 hotfix e12f276 必需)
  2. 返 [`crate::term::TermState::display_text_with_spacers`]`(viewport_line)`
```

history 分支 `< 0` 文档同步指 `scrollback_line_text_with_spacers` (本 ticket B 段)。

### Out

- east-asian width 表自带版本 (本 ticket 直接用 unicode-width crate 已有数据)
- preedit 字体 metrics 替换 (Phase 4+ 字体真实 metrics ticket)
- alacritty Term 内部 CJK 处理改动 (本 ticket 只在 quill 边界做协议层修)
- M3 (preedit + key-repeat 边界) — 见 T-0808 独立 ticket

## Acceptance

1. `cargo test --lib` 全过 (含新增测试 ≥ 5 个: combining_mark / zwj_emoji / variation_selector / scrollback_with_spacers_cjk / cross_scrollback_copy_no_extra_spaces 或近似命名)
2. `cargo clippy --all-targets -- -D warnings` 无 warning
3. `cargo fmt --check` 跳过 main 既有漂移 (本 ticket 改动文件 fmt 必须干净)
4. **user 实测** (e2e):
   - **A**: 终端输 `printf 'á\n'` → 视觉 1 cell, cursor 跟 grid 对齐
   - **B**: PTY 输出大量内容滚到 scrollback, 再 PTY 输 `你好世界` (在 viewport), 选区从 scrollback 中某 CJK 行拖到 viewport 中 `你好世界`, 复制 → 粘贴到任何编辑器, 内容与屏幕显示一字不差 (无夹空格)
   - **C**: fcitx5 输 `今天` (CJK preedit), 下划线宽度覆盖 4 cell (不是 2 cell)

## INV-010

- 不暴露 cosmic-text / unicode-width / alacritty 类型给公共 API
- `scrollback_line_text_with_spacers` 沿袭现有 `scrollback_line_text` 类型签名 (接 ScrollbackPos, 返 String)
- closure 协议 (`row_text(line: i32) -> String`) 不动, 内部实现切换函数

## 相关

- ADR 0010 (unicode-width crate, 本 ticket 收口零宽 / 共享 cluster 边界)
- T-0803 (grid 宽度判定切 unicode-width — 没收完 zero-width / cluster, 本 ticket M1 收尾)
- T-0805 + e12f276 hotfix (cross-scrollback copy 协议, M2 是其 spacer 协议遗漏)
- memory `feedback_mock_closure_protocol_blindspot_2026-05-02.md` (M2 直接验证: viewport ↔ scrollback 协议接口盲区, mock test 无法覆盖, 必须 e2e)
- Codex review 报告 (本 session, 2026-05-02)
