# ADR 0012 — Selection 在 grid 内部 scroll 时 rebase (钉字而非钉 cell)

**Status**: Proposed
**Date**: 2026-05-02
**Phase**: 8 (terminal correctness)
**Related**: T-0607 (selection 实装), T-0804 + ADR 0011 (SelectionPos viewport-relative 索引), T-0805 (cross-history copy), T-0809 (本次落地)

## Context

ADR 0011 把 `SelectionPos { line: i32, col: usize }` 定义为 **viewport-relative**:
- `line >= 0` viewport 内 (0=顶)
- `line < 0` history (-1=viewport 上方第 1 行)
- 静态 viewport 时同一 SelectionPos 永远指向同一 cell ✓

ADR 0011 第 31-34 行明确假设:
> "viewport 滚动时 origin 不动 (内容通过 display_offset 跟 origin 错位). 所以同一 SelectionPos 永远指向同一 cell, viewport 滚动不需要更新 anchor / cursor"

这个假设**只对 user 主动滚屏**成立 (display_offset 变, alacritty grid 内部状态不变). 对 **PTY 流式输出导致的 alacritty grid 内部 ring-buffer 旋转**不成立:

- PTY 输出 newline + viewport 满 → alacritty `Term::scroll_up` → 内部 ring-buffer 旋转, 顶 viewport line 内容下移到 scrollback (history_size += 1), 底 viewport line 空出给新内容
- viewport_line=0 的"字"换了 (从 user 视角看老字进了 history, 新字在底部)
- SelectionPos.line=0 仍指 viewport 顶 cell, 但**那个 cell 现在是新字**

T-0809 user 实测确认三 bug 同源:

1. **`yes hello` 流式输出**: 字滚出 viewport, 框留屏幕原位 — 框圈住的内容一直变 (PTY 新字)
2. **pacman ANSI cursor + 重绘**: 字位置不变但 cell 内容被 ANSI 重写, 框圈住新写入的字符 (从"框字"语义算 bug)
3. **Claude Code 后台输出 + user 看 history**: 每次 CC 输出新 token, 选框跳一格 (具体真因待 writer 在 worktree 排查, 假设是 selection 渲染坐标依赖 history_size / display_offset 漂移)

## Decision

**SelectionPos 的字段不变** (兼容 ADR 0011 类型), 但**语义升级**: 必须钉**字**而非 cell. 实装走 **rebase-on-grid-scroll**:

> 每次 alacritty grid 因 PTY 输出 (或其他内部原因) 发生 scroll 旋转 N 行时, quill 在调用 `Term::advance_bytes` 后**主动调整** SelectionState:
>
> - `selection.anchor.line -= N`
> - `selection.cursor.line -= N`
>
> 这样 selection.line 永远指向**原字** (字进 scrollback 时 line 变负数, 渲染翻译自然落到 history 区).

字滚出 scrollback 顶 (history 满 + 旋转) 时, selection.line < `-history_size_max`, **selection 整体丢弃** (clear) 或 **clamp 到 history 顶** (writer 二选一, 倾向 clamp 让 user 仍能看到部分高亮).

## Alternatives 考察

### A. 单调递增 character ID (foot / kitty 风格)

每个字给 unique u64 ID, selection 持 (anchor_id, cursor_id, anchor_col, cursor_col), 字进 scrollback 也带 ID. 渲染时反查 ID → 当前 grid 位置.

- ✓ 鲁棒性最高 (id 永不漂移, 任何 grid 操作不影响)
- ✗ 改动大: 新增 quill 层 ID counter + 反查 map (每行一个 ID 还是每 cell 一个? 通常每行一个) + SelectionPos 字段重定义 + 所有 callsite 改
- ✗ 跨 quill 重启 / alt screen 切换 ID 失效边界
- ✗ 跟 ADR 0011 类型不兼容, 需要 supersede 整个 ADR

### B. Rebase on grid scroll (本 ADR 选择)

- ✓ ADR 0011 SelectionPos 类型完全保留, 改动局部 (advance_bytes 包装层)
- ✓ alacritty / iTerm2 / wezterm 都用此方案, 工业对齐
- ✓ 边界 case (字滚出 scrollback 顶) 自然 clamp / clear, 不需要 ID 失效追踪
- 需要每次 PTY drain 后比 history_size 差值, 计算 scroll delta — O(1) 开销, 无所谓
- 关键: T-0809 还要修 Bug 3 (selection 渲染坐标漂移), 这部分跟 rebase 独立但同 ticket 一起处理

### C. 改 selection 渲染坐标计算, 不动 selection 状态

只改"selection.line + display_offset = 屏幕 line"渲染公式, 让公式随 history_size 动态调整, selection.line 字段语义保持 ADR 0011 viewport-relative.

- ✓ 改动最小 (只动渲染层公式)
- ✗ 治标不治本: PTY scroll 后 selection.line=5 仍指 viewport 第 5 行, 字已不在那, 渲染公式无论怎么算都还是 cell 而非字
- ✗ 解决不了 Bug 1+2

## Consequences

### 正向
- 三 bug 同源根除: 流式输出 / ANSI 重绘 / history view + PTY 后台输出, selection 都钉字
- 跟 alacritty selection 行为 1:1 对齐, 后续若引入 alacritty 上游算法可直接套
- ADR 0011 类型不动, 不影响其他模块 (selection 模块改动局限)

### 负向 / 待跟进
- `Term::advance_bytes` 包装层必须**每次** drain 后调用 selection rebase, 漏一处就漂. 集中在 src/wl/window.rs (drive_wayland 主循环) 单点 wrapping
- 字滚出 scrollback 顶时 selection 行为 (clamp vs clear) 需 writer 实测后定. 默认 **clamp 到 history_size 上限** (`selection.line = -(history_size as i32)`), 让 user 仍能看到选区上限标记, 比 silent clear 更直观
- resize 时 selection rebase 逻辑独立 (resize 改 viewport rows, 跟 grid scroll 不同). 本 ticket Out, 留后续 ticket
- alt screen 切换时 selection 应清空 (alt screen 没 history, selection 状态无效). 本 ticket Out

## 落地

- 在 `Term::advance_bytes` 调用点 (drive_wayland step 处理 PTY) 包装: 取 history_size before/after, delta = after - before
- 调 `selection_state.rebase(-delta)` 把 anchor / cursor line 都减 delta (= 字进 scrollback)
- rebase 内 clamp: line < -(history_size_max) 时 clamp 或 clear (writer 选)
- 修 Bug 3 (渲染坐标漂移) 作为同 ticket sub-task, 真因 writer 排查 (可能在 selection.rs 渲染计算依赖 history_size 错算)
- 详 T-0809 ticket
