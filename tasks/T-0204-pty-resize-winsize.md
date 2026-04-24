# T-0204 PTY resize (SIGWINCH 转发)

**Phase**: 2
**Assigned**: 写码-close
**Status**: in-review
**Budget**: tokenBudget=80k, walltime=2400s, cost=$4
**Dependencies**: T-0201(需要 `PtyHandle` 已有 master)

## Goal

`PtyHandle::resize(cols, rows)` 调 `MasterPty::resize(PtySize { .. })`,底层走 `ioctl(TIOCSWINSZ)` + 给前台进程组发 SIGWINCH。Phase 2 **不** 对接窗口 resize 事件(那是 Phase 3 T-0306 的事,因为现在还没有文本网格,像素尺寸 → cell 尺寸的换算不存在);本 ticket 只把 API 通出来并加一个单测证明调用不报错、且 shell 内部 `tput cols` / `tput lines` 拿到的值与我们传入的一致。

**硬编码 80x24 的行为** 由 T-0202 的 `spawn_shell(80, 24)` 保持不变。本 ticket 只是让 `resize()` 这个出口可用,将来 Phase 3 T-0306 接入时不用动 `PtyHandle` 内部。

## Scope

- In:
  - 实装 `PtyHandle::resize(&self, cols: u16, rows: u16) -> anyhow::Result<()>`:
    - 包装 `master.resize(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 })`
    - 错误用 `anyhow::Context::context("PTY resize ioctl 失败")` 包装
  - `pixel_width` / `pixel_height` 传 0(语义:未知 / 不关心;Phase 4 接入 HiDPI 时再填)
  - 单元 / 集成测试:
    - spawn `bash -c "echo cols=$(tput cols) lines=$(tput lines); exit"`,初始 80x24,读到字节中应包含 `cols=80` 和 `lines=24`
    - 再 spawn 一个,先 `resize(120, 40)` 再跑同样命令,读到字节包含 `cols=120` 和 `lines=40`
- Out:
  - 把 Wayland 的 `configure` 事件 → px → cell 的换算接到 resize(Phase 3 T-0306)
  - 字体 / cell 尺寸的确定(Phase 4)
  - 对 `&mut self` 还是 `&self` 的讨论(`portable-pty` 的 `MasterPty::resize` 签名是 `&self`,直接照抄)

## Acceptance

- [ ] `cargo build` 通过
- [ ] `cargo test` 通过
- [ ] `cargo clippy --all-targets -- -D warnings` 通过
- [ ] `cargo fmt --check` 通过
- [ ] 审码 放行
- [ ] 上述 `tput cols / lines` 断言的集成测试通过(可放 `tests/pty_resize.rs`)
- [ ] `PtyHandle::resize` 的 docstring 说明了"Phase 2 阶段无调用方,接口预留给 Phase 3 T-0306"

## Context

- `CLAUDE.md` —— 禁 `unwrap`;"先写测试再写实现"
- `docs/invariants.md` —— INV-005(这里不新增 invariant,resize 是一次性 ioctl,不涉 fd 生命周期)
- `ROADMAP.md` Phase 2 T-0204;后续 T-0306
- `portable-pty::MasterPty::resize`:https://docs.rs/portable-pty/latest/portable_pty/trait.MasterPty.html#tymethod.resize
- TIOCSWINSZ / SIGWINCH 背景:`man 4 tty_ioctl`

## Implementation notes

- `portable-pty::PtySize` 字段名是 `rows`(行)和 `cols`(列),别把 (cols, rows) 传反 —— 写一个 named-args 辅助函数 或严格按 docstring 顺序命名,避免两个 u16 交换
- bash 自己不主动广播列数变化;是 readline / shell builtin 在每次 prompt 前调 `tcgetattr` + `TIOCGWINSZ` 拿。`tput cols` 会 stat 控制终端,所以 resize 一定要在 spawn **之前** 或在 spawn 后 prompt 之前完成 —— 这不是本 ticket 的约束,写测试时注意
- 本 ticket 完全不动 `src/wl/`,也不动 `src/main.rs`。纯 `src/pty/mod.rs` 内部实装 + test 文件
