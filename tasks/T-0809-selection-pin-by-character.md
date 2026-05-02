# T-0809 Selection 钉字 (rebase on grid scroll, 修三 bug 同源)

**Phase**: 8 (terminal correctness)
**Assigned**: TBD (writer)
**Status**: open
**Budget**: tokenBudget=120k (selection 模块 + advance_bytes 包装层 + 渲染坐标排查 + 测试; 估计中等复杂)
**Dependencies**: T-0807 已 merge (Unicode 宽度收口); ADR 0011 (SelectionPos 类型) 不动, ADR 0012 本 ticket 引入
**Priority**: P1 (用户日常踩, 三场景同源)

## Bug (用户实测三场景, 全部 selection 钉 cell 不钉字)

### 场景 1: `yes hello | head -100000` 流式输出

- 用户拖选 viewport 中某 5 个 hello
- PTY 持续输出 → alacritty grid 内部 scroll 旋转, 老 hello 进 scrollback
- **观察**: 选框**绝对屏幕位置不动**, 框圈住的内容一直变 (永远是 viewport 当前的字)
- **预期**: 框跟原选的 5 个 hello 一起进 scrollback (字进 scrollback 框也跟)

### 场景 2: `pacman -Syu` 大量更新输出

- 用户拖选 viewport 中某段
- pacman 用 ANSI cursor home + 重绘 (CSI K 清行 + 写新内容), alacritty grid 内部**不真 scroll** (cell 位置不变), 但 cell.c 内容被改写
- **观察**: 选框位置不变, 但圈住的字符内容变了 (pacman 写入的新字符)
- **预期**: 框圈住的应是用户原选的字符, 即使 cell.c 被改写 (从"selection 是字"语义算)

### 场景 3: Claude Code 后台输出 + user 在 history

- 用户滚到 history (display_offset > 0), 拖选 history 中某段
- Claude Code 后台输出 PTY (流式 token) → grid 内部 scroll, history_size += N (alacritty 自动 display_offset += N 维持 user 视角锚定)
- **观察 (实测)**: **每次 CC 输出新 token, 选框跳一格** (跳跃, 不是平滑), 方向跟 PTY 输出方向一致 (往底部蹦)
- **预期**: user 原选的字 (在 history) 物理位置没动, 框应钉死不蹦

## 真因

ADR 0011 把 SelectionPos.line 钉 viewport 第几行 (= cell 几何位置), **跟字本身无关**. 三场景都暴露这个根因:

- 场景 1+3: alacritty grid 内部 scroll 旋转, viewport_line=N 的字换了, selection.line=N 却没动 → 框圈住新字
- 场景 2: ANSI 重绘, viewport_line=N 的 cell 没动但 cell.c 换了, selection.line=N 圈住的字也换了 (这种 case 跟 1+3 同源, 都是"框 cells 不框字")

场景 3 还透露**附加 bug**: "每次 CC 输出框跳一格" 暗示 selection 渲染坐标计算路径每帧累加 history_size 漂移 (具体真因 writer 在 worktree 排查 — 可能 selection.rs 某处用了 `selection.line + history_size_delta` 之类公式).

## Goal

selection 钉字而非钉 cell. 完工后:
- 场景 1: 框跟原 hello 进 scrollback, 一起滚走
- 场景 2: 框圈住原字符 (即使 ANSI 重绘也不变)
- 场景 3: 框钉死不蹦, CC 输出无影响

## Scope

### In

#### A. ADR 0012 实装 — `Term::advance_bytes` 包装 selection rebase

- 找 quill 调 `alacritty Term::advance_bytes` 的位置 (src/wl/window.rs drive_wayland 主循环, 处理 PTY 字节的地方)
- 包装逻辑:
  ```rust
  let history_before = term.scrollback_size();
  term.advance_bytes(&pty_bytes);
  let history_after = term.scrollback_size();
  let scroll_delta = history_after.saturating_sub(history_before) as i32;
  if scroll_delta > 0 {
      selection_state.rebase_for_grid_scroll(scroll_delta);
  }
  ```
- `SelectionState::rebase_for_grid_scroll(delta: i32)` 新方法 (在 src/wl/selection.rs):
  ```rust
  pub fn rebase_for_grid_scroll(&mut self, delta: i32) {
      if let Some(anchor) = self.anchor.as_mut() { anchor.line -= delta; }
      if let Some(cursor) = self.cursor.as_mut() { cursor.line -= delta; }
      // clamp: 若 line < -(history_size_max as i32) 表字滚出 scrollback 顶
      // writer 决定: clamp 到 -(history_size_max) 还是 clear selection
      // 推荐 clamp (UI 上保留选区高亮上限, 比 silent clear 直观)
  }
  ```

#### B. 排查 + 修 Bug 3 (selection 渲染坐标漂移)

scope B 是**调查任务**, 不是已知答案:

1. 复现: 在 worktree 跑 quill, 滚到 history 看, 在另一终端 `for i in {1..30}; do echo "tick $i"; sleep 1; done > >(tee >(cat >> /tmp/quill-pty-tee))` 或类似, 观察 selection 框跳的具体频率 / 方向 / 间距
2. 推测真因 (排序优先级):
   - **D-1**: selection 渲染走 `viewport_line = selection.line + display_offset`, 但 display_offset 在 PTY 输出时被 alacritty auto-adjust (维持 user 视角), selection.line 没动 → 渲染时 viewport_line 错算 → 框漂. 修法: 渲染公式改为不依赖 display_offset (selection.line 直接是 viewport 行号 + grid 内部坐标)
   - **D-2**: selection 渲染层有重复执行的 callback 每帧把 line += 1
   - **D-3**: T-0805 hotfix 的 closure 在某个渲染路径被错误调用
3. 修复 + 测试覆盖 — bug 3 的修法跟 A 段 rebase 是独立改动 (rebase 修字滚框留, B 段修框自漂), 都在 ticket scope 内

#### C. 测试 (覆盖三场景的回归锁)

测试要避开 mock 协议接口盲区 (memory `feedback_mock_closure_protocol_blindspot_2026-05-02.md`), 倾向用真实 alacritty Term + selection 状态机:

- `selection_anchor_follows_pty_scroll` — mock TermState 输出 N 行让 history_size += N, 调 rebase, 验 selection.anchor.line 减 N
- `selection_cursor_follows_pty_scroll` — 同上 cursor 端
- `selection_clamped_when_scrolled_past_history_top` — 字滚出 scrollback 顶, selection 走 clamp / clear 路径 (writer 选哪个就测哪个)
- `selection_render_does_not_drift_with_history_growth` — 关键测 Bug 3 真因. mock display_offset 增长 + selection 不动, 验渲染层算出的屏幕位置 stable
- 跨 history-viewport 边界 selection 测试 (anchor 在 scrollback, cursor 在 viewport) rebase 后两者都正确减 delta

#### D. ADR 0011 注释更新 (轻量)

ADR 0011 第 31-34 行的"viewport 滚动不需要更新 anchor / cursor"假设错误, 加 follow-up note 指向 ADR 0012 的修正 — 不动 ADR 0011 主体 (历史决策保留), 文末加:

```
## Update 2026-05-02 (T-0809 / ADR 0012)

本 ADR 的"viewport 滚动 origin 不动"假设只覆盖 user 主动滚屏 (display_offset
变), 不覆盖 PTY 输出导致的 alacritty grid 内部 ring-buffer 旋转. 后者需要
selection 主动 rebase, 详见 ADR 0012.
```

### Out

- **resize selection rebase** (resize 改 viewport rows, 跟 grid scroll 不同 — 后续 ticket)
- **alt screen 切换 selection** (alt screen 没 history, selection 应清空 — 后续 ticket)
- **selection 字符实际内容追踪** (即"框记 anchor 字本身, 不靠 line 索引") — 这是 ADR 0012 Alt A 单调 ID 方案, 本 ticket 选 rebase 路径
- **改 ADR 0011 主体决策** — 0011 类型保留, 0012 是补充
- **真 e2e 集成测试** (启 alacritty + PTY + 模拟用户拖选 + 观察 selection 框) — 仅作 acceptance user 实测, 不强制写自动化

## Acceptance

1. `cargo test --lib` 全过 (含新增测试 ≥ 5 个, 三场景各覆盖 + clamp 边界 + render 不漂)
2. `cargo clippy --all-targets -- -D warnings` 无 warning
3. `cargo fmt --check` 跳过 main 既有漂移 (本 ticket 改动文件干净)
4. **user 实测三 bug 全消失**:
   - **场景 1**: 开 quill, `yes hello | head -1000`, 拖选 5 个 hello, 等输出滚 viewport → 框跟原 hello 一起进 scrollback (滚回 history 看仍在), viewport 顶不留空框
   - **场景 2**: 开 quill 跑 `pacman -Syu`, 拖选某行字符, 等 pacman 重绘 → 框圈住的字符不变 (跟原选一致)
   - **场景 3**: 开 quill 跑 Claude Code, 等 CC 后台输出, 用户滚到 history 选某段, CC 继续输出 → 选框**钉死不蹦**, history 内容也不动

## INV-010

- selection rebase 调用栈不接触 alacritty 类型, delta 是普通 i32 (来自 quill `term.scrollback_size()` u32 cast), `SelectionState::rebase_for_grid_scroll` 不引新类型
- ADR 0012 是 quill 内部决策, 不暴露上游

## 相关

- ADR 0011 (SelectionPos 类型, 本 ticket 不改, 文末加 update note 指向 0012)
- ADR 0012 (本 ticket 引入)
- T-0607 (selection 实装基础)
- T-0804 (SelectionPos 类型实装)
- T-0805 + e12f276 (cross-history copy + display_offset 协议; 注意本 ticket 修的"渲染漂移" Bug 3 可能跟 hotfix 引入的某条路径有关)
- alacritty 上游 selection rebase 实装 (alacritty/alacritty_terminal/src/term/mod.rs::scroll_up — selection.update_for_scroll)
- memory `feedback_mock_closure_protocol_blindspot_2026-05-02.md` (Bug 3 排查避免再踩协议接口盲区)
