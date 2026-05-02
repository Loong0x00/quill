# ADR 0011 — SelectionPos 用 viewport-relative i32 line (跨 history + viewport)

**Status**: Proposed
**Date**: 2026-05-02
**Phase**: 8 (terminal correctness)
**Related**: T-0607 (selection 实装), T-0804 (本次落地)

## Context

T-0607 实装选区时 SelectionState 的 anchor / cursor 用 `CellPos` (viewport-
relative, line ∈ 0..rows). 文档显式注 P2 留 Out: "跨边界跟历史滚走是 P2 (派单
Out), 当前: 滚屏期间 cursor 跟 viewport 偏移".

T-0804 user 实测自动滚屏时蓝框不跟字 → P2 升 P1 → 必须改坐标系.

quill 已有 `ScrollbackPos { row: usize }` (src/term/mod.rs:101) 但**只覆盖
history 区** (row=0..history-1), 不能表达 viewport 内的行. 选区可能跨 history
+ viewport, 需要新坐标类型.

## Decision

新加 `SelectionPos { line: i32, col: usize }`:

- `line >= 0`: viewport 内 (0=顶, rows-1=底)
- `line < 0`: scrollback history (-1=viewport 上方第 1 行, -history=最旧)
- `col`: 列索引 (0=最左)

跟 alacritty `Point<Line=i32>` 同语义, 但 quill 自己的类型 (INV-010 不暴露
alacritty Line).

滚屏机制: viewport 顶就是 line=0 origin, viewport 滚动时 origin 不动 (内容
通过 display_offset 跟 origin 错位). 所以同一 SelectionPos 永远指向同一 cell,
viewport 滚动**不需要更新 anchor / cursor**, 渲染时通过 `viewport_line =
selection.line + display_offset` 算回当前 viewport 中的位置.

## Alternatives 考察

### A. 沿用 CellPos viewport-relative (现状)

- ✓ 无新类型
- ✗ 滚屏时蓝框跟 viewport 不跟字 (本 bug)
- ✗ 不能表达跨 history + viewport 选区

### B. 扩展 ScrollbackPos 加 col

- ✓ 复用现有类型
- ✗ ScrollbackPos.row: usize 仅覆盖 history, 不能表 viewport
- ✗ 改 row 类型语义影响其他 ScrollbackPos 用户 (scrollback_line_text /
  scrollback_cells_iter), 副作用大

### C. 直接 re-export alacritty Point/Line

- ✓ 零 quill 抽象成本
- ✗ 违反 INV-010 类型隔离
- ✗ alacritty 0.27/0.28 升级时类型可能变, quill 公共 API 跟着抖

### D. SelectionPos 用 viewport-relative i32 line (本 ADR 选择)

- ✓ 跟 alacritty / kitty / ghostty 同语义, 工业对齐
- ✓ 滚屏自动跟随 (origin 不变)
- ✓ INV-010 守 (quill 自定义 struct, 不 re-export alacritty)
- ✓ 跨 history + viewport 自然表达 (i32 涵盖正负)

## Consequences

### 正向

- 滚屏期间选区视觉跟字同步 (核心目标)
- 选区可跨 history + viewport (跨边界拖拽自然 work)
- 跟 alacritty / 工业终端对齐, 后续若引入 alacritty selection 算法可直接
  对应

### 负向 / 待跟进

- 选区数据现在可能引用 history 区 cell, 复制路径需要拿 scrollback 区
  cell 内容. 当前 `selected_cells_linear` 仅 emit viewport 内 cell, 跨
  history 部分**跳过不复制** (T-0804 显式 Out 段). 复制完整跨边界选区走
  另开 ticket T-0805.
- pixel_to_cell 现在依赖 `display_offset`, 测试要 mock 这个值. 增加测试
  setup 复杂度但仍 panic-free.

## 落地

- 加 SelectionPos struct (term/mod.rs 或 selection.rs, 选 selection.rs 让
  selection 域类型集中)
- SelectionState anchor/cursor 改 SelectionPos
- pixel_to_cell 改返 SelectionPos, 加 display_offset 参数
- selected_cells_linear / block 改用 SelectionPos, viewport 反查用
  display_offset
- 滚屏 callback 不动 selection
- 详 T-0804 ticket
