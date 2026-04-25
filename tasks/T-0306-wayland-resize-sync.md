# T-0306 Wayland resize → Term + PTY 同步

**Phase**: 3
**Assigned**: writer-T0306
**Status**: in-review
**Budget**: tokenBudget=60k (lead 派单)
**Dependencies**: T-0204 (PtyHandle::resize 已就绪) / T-0302 (TermState::dimensions) / T-0305 (draw_cells 用 cols/rows 参数)

## Goal

窗口拉大/缩小时, **terminal 的 80×24 grid 不固定**, 而是按 surface 像素 / cell pixel size 动态计算新 cols/rows。Wayland configure 事件来时, 同步:
1. wgpu surface 重 configure (新 width/height) — 已部分实装 in render.rs
2. **term.resize(new_cols, new_rows)** ← 本单加
3. **pty.resize(new_cols, new_rows)** ← 已 API 就绪 (T-0204), 接通即可

完工后: 拉窗口 bash 真的能多显示行 / 列 (TIOCSWINSZ ioctl 让 shell 知道新 winsize, 不再固定 80×24)。

## Scope

### In

#### A. `src/term/mod.rs` — 加 `pub fn resize`
- `pub fn resize(&mut self, cols: usize, rows: usize)`
  - 走 alacritty `Term::resize(TermSize::new(cols, rows))` (TermSize 已实装在 mod 内, 复用)
  - cols / rows 都至少 1 (`max(1)` 防 0 panic)
  - resize 后 grid 内部状态 (cells / cursor / scrollback) 自动跟随 alacritty 处理
  - **置 dirty=true** (resize 后必须重画, 沿袭 advance 模式)
- 测试 (放 `#[cfg(test)] mod tests`):
  - `resize_changes_dimensions`: ctor 80×24 → resize 100×30 → dimensions() 返 (100, 30)
  - `resize_to_smaller_clamps_cursor`: 字光标推到 (50, 10) → resize 30×5 → cursor_pos.col ≤ 29 + line ≤ 4
  - `resize_zero_clamped_to_one`: resize(0, 0) 不 panic, dimensions() 返 (1, 1)
  - `resize_sets_dirty`: clear_dirty() 后 resize → is_dirty() == true

#### B. `src/wl/window.rs` — 接 Wayland configure → term + pty 同步
- 现有 wayland configure callback (`xdg_toplevel.configure` event) 已处理 surface 重 configure (render.rs side)
- T-0306 加: 在同一 callback 拿到新 width/height (px) 后:
  1. 算 `new_cols = max(1, new_width / cell_w)`, `new_rows = max(1, new_height / cell_h)` — cell pixel size 用 T-0305 的 `surface_w / cols` **倒过来** (现在固定 80x24, 后续 Phase 4 字形测量后会改)
  2. **决策点**: cell pixel size 当前是 dynamic (cell_w = surface_w / cols), T-0306 要么:
     - (a) 保持 cell dynamic, cols/rows 不变 → 拉窗口 cell 变大但不能多显示行/列 (Phase 3 现状)
     - (b) hardcode cell pixel size 为常数 (e.g., 10×25 px), cols/rows 跟随 surface → 拉窗口能多显示
  - **推 (b)**: hardcode `CELL_W_PX = 10`, `CELL_H_PX = 25` 作为模块常数 (Phase 4 字形渲染时用 cosmic-text 字符宽高替换), 现在写在 src/wl/render.rs 顶部
  - 改 draw_cells 用常数计算 cell rect (替代当前 `surface_w / cols` 公式)
  - configure callback 算 cols/rows 调用 `term.resize(cols, rows)` + `state.pty.as_ref().map(|p| p.resize(cols, rows))` 链式调用
- LoopData split borrow: `let LoopData { state, term, .. } = &mut *data;` 拿 `&mut state.pty` (Option) + `&mut term`, callback 内调 term.resize + pty.resize 后置 dirty (term.resize 自动置)

#### C. 集成测试 (新文件 `tests/resize_chain.rs` 或加到现有 tests/pty_to_term.rs)
- `term_and_pty_resize_in_lockstep`: 模拟 configure → call term.resize(100, 30) + pty.resize(100, 30) → 验证 term.dimensions() = (100, 30) + pty 端 SIGWINCH 收到 (这个用 PtyHandle 暴露的 size readback API 验, 如果没有 readback 则只验 term)
- 可能没法 100% 模拟 Wayland configure (没 mock), 退而求其次: 直接调 chain fn 验证调用顺序 + 副作用

### Out

- **不做**: 字形测量 (Phase 4 cosmic-text 字宽高自适应) / fontmetrics
- **不做**: 高频 resize debounce (Phase 6 soak 再说, 现在 raw configure 就 resize)
- **不动**: src/pty (resize API 已就绪) / docs/invariants.md / Cargo.toml
- **不引新 crate / 不写 ADR**

## Acceptance

- [ ] 4 门全绿 (`cargo build` / `cargo test` / `cargo clippy --all-targets -- -D warnings` / `cargo fmt --check`)
- [ ] term.resize 实装 + 4 个单元测试覆盖 (dimensions / cursor clamp / zero clamp / dirty)
- [ ] cell pixel size 常数化 (CELL_W_PX / CELL_H_PX), draw_cells 用常数算 rect
- [ ] Wayland configure → term.resize + pty.resize 链路接通, LoopData split borrow OK
- [ ] 1 个集成测试 (term + pty resize lockstep)
- [ ] **手测**: cargo run 起窗口 → 鼠标拖角拉大窗口 → bash prompt **不再被截在 80 cols**, 实际能显示更多 cells (描述实测行数即可, 不强制截图)
- [ ] 审码放行 (P0/P1/P2 全过)

## 必读 baseline (fresh agent 启动顺序)

1. `/home/user/quill/CLAUDE.md`
2. `/home/user/quill/docs/conventions.md` (写码 idiom, 必读)
3. `/home/user/quill/docs/invariants.md` (INV-001..009, 注意 INV-002 cell_pipeline 已加)
4. `/home/user/quill/docs/audit/2026-04-25-T-0202-T-0303-handoff.md` (5 主题, 类型隔离 §1 + calloop §3 必读)
5. `/home/user/quill/docs/audit/2026-04-25-T-0305-review.md` (上一单 audit, fg vs bg 决策模式 + 看 Lead INV-002 follow-up 怎么做)
6. `/home/user/quill/src/term/mod.rs` (加 resize 主战场)
7. `/home/user/quill/src/wl/render.rs` (cell pixel 改常数化, draw_cells 调整)
8. `/home/user/quill/src/wl/window.rs` (configure callback 接 term.resize + pty.resize)
9. `/home/user/quill/src/pty/mod.rs` (PtyHandle::resize API, 已就绪不用改, 看签名)

## 已知陷阱

- alacritty Term::resize 接 TermSize, 不接 (cols, rows) tuple — 走 mod 内已有的 TermSize 包装
- LoopData split borrow: configure callback 同时拿 &mut state.pty + &mut term, Rust 2021 NLL 可以但顺序写对
- pty.resize 是 `&self` (内部用 RawFd + ioctl), 不需要 &mut, 跟 term.resize (&mut self) 不冲突
- cell 常数化后 draw_cells 公式改, 注意原 surface_w / cols 公式删干净别留死代码
- resize 到极小 (cols=1 rows=1) 不该 panic, alacritty Term::resize 内部可能有 assert, 写码自己跑一下 1×1 测试验证
- handoff §5 教训: 实装期间 follow-up Lead 5-10 min, 主动告知任何 scope 偏离

## fg vs bg follow-up 期望

T-0305 的 fg-driven 渲染是 Phase 3 临时决策, T-0306 不动这个。Phase 4 字形渲染时会回到 fg=glyph + bg=background 的标准模式 (T-0305 已留 bg 字段在 vertex 通路, WGSL 一行可切)。
