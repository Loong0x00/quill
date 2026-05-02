# T-0805 选区复制支持跨 history (修 T-0804 显式 Out 的复制 fallback 误导)

**Phase**: 8 (polish++, terminal correctness)
**Assigned**: TBD (writer)
**Status**: open
**Budget**: tokenBudget=80k (selection.rs extract 路径 + window.rs row_text closure + 测试)
**Dependencies**: T-0804 (SelectionPos viewport-relative i32 line 已实装)
**Priority**: P1 (user 实测 T-0804 蓝框跟字滚 OK 后, 复制功能跨 history 给"viewport 整段"误导内容, 阻塞 IME bug 调查 — 用户需要复制大块 wayland debug log)

## Bug

T-0804 完工后, user 实测拖拽到 viewport 边缘触发 autoscroll, 蓝框跟字
滚 ✓ (T-0804 核心修了). 但选区释放后**复制内容不正确**:

- 选区 anchor 在 history (line < 0), cursor 在 viewport
- 复制结果 ≈ "当前显示范围内的 viewport 整段从 col 0 起整行"
- 实际期望: 跨 history + viewport 完整复制 user 真选过的 cells

T-0804 显式 Out 段写: "跨 history 部分的复制内容走另开 ticket T-0805".
当前 fallback 实装 (extract_linear/block 把端点 clamp 到 viewport 边界 +
首可见行 col=0) 误导用户以为复制成功但内容不对.

## 真因

`extract_linear` (src/wl/selection.rs:454-467, T-0804 实装) 的 fallback
逻辑:

```rust
// T-0804: 选区端点 sel_line → viewport_line. start/end 整体滚出 viewport
// (start>底 或 end<顶) 时返空; 部分滚出时把端点 clamp 到可见边界.
let visible_start_line = start_v.max(0) as usize;        // history clamp 到 viewport 顶
let visible_end_line = end_v.min(rows_i32 - 1) as usize;
let start_col = if start_v < 0 { 0 } else { start.col }; // history 部分首可见行 col=0
let end_col = if end_v >= rows_i32 { cols.saturating_sub(1) } else { end.col };
```

clamp 后 anchor=(line=0, col=0) cursor=(line=10, col=20) → 复制 viewport
0-10 行整段从 col 0 起 → user 看到的"viewport 整段". 这个 fallback 选择
T-0804 ticket 写明是过渡, T-0805 应该补完真正的跨 history 复制.

## Goal

extract_selection_text 真支持跨 history. row_text closure 签名扩展, 调用方
(src/wl/window.rs:1246) 在 line < 0 时调 `TermState::scrollback_line_text`
拿 history 行内容; line >= 0 时调原 display_text_with_spacers (viewport 内).
extract_linear/block 不再 clamp col, 保留 user 真实 anchor.col / cursor.col.

完工后 user 实测拖拽到 viewport 外触发 autoscroll 选 50 行 (跨 history +
viewport), 释放后复制粘贴拿到完整 50 行内容, 起止 col 跟用户实际拖动起止位
置一致.

## Scope

### In

#### A. extract_selection_text + extract_linear + extract_block 改 row_text 签名
- 当前 `F: FnMut(usize) -> String` (viewport line)
- 改为 `F: FnMut(i32) -> String` (viewport-relative i32 line, 负=history,
  非负=viewport, 跟 SelectionPos.line 同语义)
- 调用方负责 line < 0 → scrollback_line_text 转换 (selection 模块不接触
  ScrollbackPos)

#### B. 删除 clamp 逻辑 (src/wl/selection.rs:454-467)
- 不再 `start_v.max(0)` / `end_v.min(rows_i32 - 1)`
- 不再 `start_col = if start_v < 0 { 0 }` / `end_col = if end_v >= rows_i32 { cols-1 }`
- 直接用 user 真实 (start.line, start.col) / (end.line, end.col) 喂给 row_text
- 边界保护: 如果 row_text 返 "" (调用方判 line 超出 history 边界返空),
  那行 skip / push 空 string (按 alacritty 行为)
- 整体仍尊重 anchor ≤ cursor 顺序排序逻辑不动

#### C. window.rs 调用方 closure 改造 (src/wl/window.rs:1246)
- 当前: `|line| t.display_text_with_spacers(line)`
- 改为:
  ```rust
  |line: i32| -> String {
      if line >= 0 {
          if (line as usize) < rows {
              t.display_text_with_spacers(line as usize)
          } else {
              String::new()  // 滚到 viewport 底外, 无内容
          }
      } else {
          // history 行: line=-1 是 viewport 上方第 1 行 (最新 history),
          // line=-history_size 是最旧. ScrollbackPos.row=0 最旧, row=history-1
          // 最新, 故转换 row = history_size - (-line) = history_size + line.
          let history_size = t.scrollback_size();
          let k = (-line) as usize;
          if k > history_size {
              String::new()  // 选区滚出 history 顶 (压根没那么多 history)
          } else {
              let row = history_size - k;
              t.scrollback_line_text(crate::term::ScrollbackPos { row })
          }
      }
  }
  ```
- 复用既有 quill API (scrollback_size 已有, scrollback_line_text 已有), 无新
  alacritty 类型暴露

#### D. 测试 (src/wl/selection.rs::tests)
- `extract_linear_crosses_history_to_viewport` — anchor.line=-3 col=2,
  cursor.line=2 col=10, display_offset=5, 验返 6 行字符串 (3 history + 3
  viewport, 起 col=2 末 col=10)
- `extract_linear_anchor_in_history_far_past_history_size` — anchor.line=
  -100 (大于 history_size), 验首行返空但后续行正常 (skip 不到的 history)
- `extract_block_crosses_history` — block 模式跨 history, 验各行 col 范围
  正确

#### E. e2e 测试调整 (tests/selection_e2e.rs)
- 既有调用 row_text closure 签名 `usize -> String` 的地方改 `i32 -> String`
  (test 内 closure 已写, 改 |line: usize| 为 |line: i32|, 对 line<0 返空,
  非负如旧)

### Out

- 历史中 cell 颜色 / 装饰 (粗体 / 反色) 在复制时**不**保留 (复制纯文本, alacritty
  /ghostty 同行为)
- 选区跨 history 后 user 滚到不同 viewport 位置, 蓝框是否在 history 部分
  显示 "占位条" 之类视觉反馈 — 不在本 ticket, 后续 polish 视觉

## Acceptance

1. `cargo test --lib selection::` + `cargo test --test selection_e2e` 全过
   (含新增 3 测试)
2. `cargo clippy --all-targets -- -D warnings` 无 warning
3. `cargo fmt --check` 跳过 main 既有漂移文件
4. user 实测: 鼠标拖拽自动滚屏选 50 行 (跨 history 30 行 + viewport 20 行),
   释放复制粘贴, 拿到完整 50 行字符 + 起止 col 跟用户实际拖动一致
5. user 之后能复制大块 wayland debug log 用于 IME bug (T-0806) 调查

## INV-010

- selection 模块不接触 ScrollbackPos (该类型在 src/term/mod.rs); 转换在
  调用方 window.rs 内 inline
- row_text closure 签名改 `FnMut(i32) -> String`, i32 是 Rust 基础类型,
  不是 alacritty Line

## 相关

- T-0804 (SelectionPos i32 line 实装, 留下本次 P2 → P1)
- T-0607 (selection 原始实装)
- src/term/mod.rs:1060 scrollback_line_text (本次复用)
- src/term/mod.rs:101 ScrollbackPos struct
