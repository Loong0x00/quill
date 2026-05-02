# T-0804 选区坐标改 scrollback-absolute (修拖拽自动滚屏时蓝框不跟字)

**Phase**: 8 (polish++, terminal correctness)
**Assigned**: TBD (writer)
**Status**: open
**Budget**: tokenBudget=120k (selection.rs 重构 + pointer.rs 调用方 + render.rs 反查 + 测试)
**Dependencies**: T-0607 (selection 实装, viewport-relative anchor/cursor 是当时显式留 P2)
**Priority**: P1 (user 实测拖拽到 viewport 外触发 autoscroll 时, 蓝框停留视觉原位但内容已滚走, 视觉与底层选区脱钩)

## Bug

User 实测: 鼠标在 viewport 内按下 anchor + 拖到 viewport 上 / 下边缘触发自动
滚屏 (T-0607 已实装 autoscroll), 文字内容正常向上 / 向下滚动, **但蓝色选区
框不跟着内容走**, 仍停留在 grid 原位置. 用户期望能选住更多内容跨过 viewport
边界, 实际选区是"按屏幕坐标固定", 不是"按字本身定位".

## 真因

`SelectionState` (src/wl/selection.rs:78-90) 当前用 `CellPos` (viewport-relative,
line ∈ 0..rows) 存 anchor / cursor. 文档显式注 (selection.rs:78-80):

> **anchor / cursor 永远在 viewport 内** (`0..cols × 0..rows`), 调用方 ...
> 跨边界跟历史滚走是 P2 (派单 Out), 当前: 滚屏期间 cursor 跟 viewport 偏移

T-0607 实装时 P2 留 Out 段, 现在 user 实测碰到, 升级 P1.

后果: viewport 滚屏 N 行后, anchor.line 仍是按下时的 viewport-relative 值,
render 路径仍按这值画蓝框 → 蓝框不动 + 字滚走 → 蓝框跟实际选中的字脱钩.

## Goal

selection anchor / cursor 改用 **viewport-relative i32 line** (类比 alacritty
`Line(i32)`: 负数 = history, 0..rows-1 = viewport 内). 选区用这种相对坐标存,
viewport 滚屏时 line 自动跟随 (因 viewport 顶就是 origin, content 滚但 origin
不动 → 同一 Line 永远指向同一 cell, 滚走的滚到 Line<0 history 区, 蓝框看不见
但底层选区数据保留, 滚回来蓝框自动重现).

完工后 user 实测: 拖拽自动滚屏时蓝框跟字走 (字滚到 viewport 外蓝框消失,
滚回 viewport 蓝框重现, 选区数据正确覆盖滚动期间所有 cell).

## Scope

### In

#### A. 新建 SelectionPos 类型 (src/term/mod.rs 或 src/wl/selection.rs)

```rust
/// Viewport-relative grid position. line=0 是 viewport 顶, line=rows-1 是底,
/// line<0 是 scrollback history (越小越旧). 跟 alacritty `Line(i32)` 同语义但
/// quill 自己的类型 (INV-010 守, 不暴露 alacritty Line).
///
/// **why i32 line 而非 ScrollbackPos { row: usize }**: ScrollbackPos 只覆盖
/// history (row=0..history-1), 不能表达 viewport 内的行. 选区需跨 viewport +
/// history, 用统一 i32 (negative=history, non-negative=viewport) 是 alacritty
/// / kitty / ghostty 共用方案.
pub struct SelectionPos {
    pub line: i32,       // viewport-relative, negative = history
    pub col: usize,
}
```

#### B. SelectionState 改用 SelectionPos (src/wl/selection.rs)

- `anchor: SelectionPos`, `cursor: SelectionPos` (替换 CellPos)
- `start(anchor, mode)` / `update(cursor)` / `end()` 签名跟随
- 文档段 "anchor / cursor 永远在 viewport 内" 删除, 改为 "anchor / cursor 用
  viewport-relative i32 line, 滚屏自动跟随内容; 选区滚出 viewport 时数据保留
  但视觉不可见, 滚回视觉重现"

#### C. pixel_to_cell 改返 SelectionPos (src/wl/selection.rs)

- 当前 `pixel_to_cell(...) -> Option<CellPos>` 返 viewport-relative
- 改 `pixel_to_cell(...) -> Option<SelectionPos>`, line 算法:
  ```rust
  let viewport_line = (usable_y / cell_h_logical).floor() as i64;  // 0..rows
  let display_offset = state.term.display_offset();  // 当前向上滚的行数, 0=底部
  let selection_line = viewport_line as i32 - display_offset as i32;
  ```
  当 display_offset=0 (无滚动), selection_line == viewport_line, 跟旧行为一致.
  当 display_offset>0 (向上滚 N 行), viewport 第 0 行其实是 scrollback 第 -N 行
  (history), selection_line = 0 - N = -N (负数, 进 history).
- 调用方 (pointer.rs apply_button / apply_motion) 透传新类型, 仍传 `top_reserved`
  顶部偏移, 仍 panic-free.

#### D. selected_cells_linear / selected_cells_block 改用 SelectionPos

- 输入 anchor / cursor: SelectionPos
- 输出仍是 viewport-relative CellPos 序列 (供渲染), 但仅吐当前 viewport 内的 cell
  (line < 0 或 line >= rows 的 cell skip, 因为不可见)
- viewport 内可见的 line 用 `display_offset` 反查: `viewport_line = selection_line + display_offset`,
  若 viewport_line ∈ 0..rows 则 emit, 否则 skip
- block 模式同理

#### E. 滚屏 callback 不动 SelectionState (src/wl/window.rs / src/term/mod.rs)

- 当前 autoscroll / 滚轮 / IME 滚屏路径如果有"重置 selection" 之类调用, 删除
- selection.anchor / cursor 在滚屏期间**不变**, viewport 偏移自然让它们指向新 cell
- 仅 selection cleared (用户主动取消) / Resize / Reset (清屏) 时才动 selection

#### F. 测试

- `selection_persists_across_scrollback_when_dragging` — 模拟拖拽到 viewport 外
  + scrollback 上滚 5 行, 验 anchor/cursor 保持 + 重 emit 时 viewport CellPos 序列
  随 display_offset 正确变化
- `pixel_to_cell_with_display_offset` — display_offset=3 时 y=titlebar (viewport
  第 0 行) → SelectionPos.line == -3
- `selected_cells_linear_skips_off_viewport_lines` — 选区跨 history + viewport,
  验仅 emit viewport 内 cell
- T-0607 既有 selection 测试改用 SelectionPos 接口, 行为不变

### Out

- 复制选区跨 history 的内容 (T-0804 只修视觉同步 + 选区数据). 复制路径要拿
  scrollback 区 cell 内容, 走另开 ticket T-0805.
- IME backspace 路由 bug — 另开 ticket T-0806 (用户已说 IME 因自改环境复杂,
  延后调查).
- Selection 序列化保存 (跨 quill 重启) — 不在 scope.

## Acceptance

1. `cargo test --lib selection::` + `cargo test --lib text::` 全过 (含新增 3 测试)
2. `cargo clippy --all-targets -- -D warnings` 无 warning
3. `cargo fmt --check` 过 (跳过既有 fmt 漂移文件 render.rs / window.rs / pointer.rs
   仅看 selection.rs / term/mod.rs 是否漂)
4. user 实测: 鼠标拖拽到 viewport 边缘触发 autoscroll, 蓝框跟字滚动, 字滚出
   viewport 蓝框消失, 滚回 viewport 蓝框重现, 复制能拿到滚动期间选中的所有字
   (复制 path 走 selected_cells_linear 当前 emit, 跨 history 部分若 skip 则
   是 T-0805 范围, 本 ticket 仅验视觉同步)

## INV-010

- SelectionPos 是 quill 自定义 struct (line: i32 + col: usize 两个基础类型)
- 不暴露 alacritty_terminal `Point` / `Line` 类型给 SelectionState 公共 API
- display_offset 拿数走 `TermState::display_offset()` (已存在的 quill 方法), 不
  re-export alacritty offset

## 相关

- T-0607 (selection 实装, 留下本次 P2 → P1)
- T-0608 hotfix (pixel_to_cell top_reserved 修, 跟本次正交)
- ADR 0011 (新写, SelectionPos 设计决定)
- 业界对照: alacritty/src/selection.rs (Point<Line=i32>), kitty/kitty/screen.c
  (selection 用 absolute line idx)
