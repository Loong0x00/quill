# T-0202 PTY master fd 注册进 calloop

**Phase**: 2
**Assigned**: 写码-close
**Status**: merged
**Budget**: tokenBudget=100k, walltime=3600s, cost=$5
**Dependencies**: T-0201(需要 `PtyHandle::raw_fd()` 可用)

## Goal

`cargo run` 启动窗口时,主事件循环除了已有的 Wayland 源之外,还会挂着 PTY master fd 作为一个 `calloop::generic::Generic` 源;该 source 在 PTY 可读时触发回调(回调本身先留空或仅打个 `tracing::trace!("pty readable")`,真正读字节是 T-0203)。用户手测:运行程序后日志应出现至少一次 "pty readable"(bash 启动打 prompt 就会走这条路径),但屏幕仍是 Phase 1 的纯深蓝——渲染未动。

验证 INV-005(calloop 为唯一 IO 调度器)落地到 PTY 这条路径。

## Scope

- In:
  - 修改 `src/pty/mod.rs`:实装 `PtyHandle::raw_fd(&self) -> RawFd`,返回 master 端的 `as_raw_fd()`
  - 修改 `src/wl/window.rs`(或 `src/main.rs` / 下文决议的 `src/app.rs`):让 `State` 新增 `pub pty: Option<PtyHandle>` 字段, **位置放在 `conn` 之后 (State 最后一位)**。理由: pty 持有 Linux fd, 与 wl 指针 / wgpu 资源生命周期**正交** —— 放最后 drop (1) 不与 INV-001 的 renderer→window→conn 链条耦合, 不需要新增 INV; (2) 保证 wl 侧资源先释放干净, 避免 pty drop 触发 SIGHUP 时 wl 回调还在飞。**此决策由 Lead + 审码 2026-04-25 拍板, 写码不需重新论证, 若你的实现路径不符必须先 SendMessage Lead 讨论**
  - 在 `run_window`(或事件循环入口)处构造 `PtyHandle::spawn_shell(80, 24)`,失败用 `?` 向上传
  - 用 `calloop::generic::Generic::new(fd, Interest::READ, Mode::Level)` 包装 raw fd,通过 `Core::handle().insert_source(...)` 注册;回调里 **暂时只** 打 `tracing::trace!(target: "quill::pty", "pty readable")` 占位,**不读字节**
  - 修改 `Cargo.toml`:若 `calloop::generic` feature 未启用则添加
- Out:
  - 真读字节(T-0203)
  - SIGWINCH / 初始尺寸超过 80x24(T-0204,本 ticket 写死 80x24)
  - 子进程退出检测(T-0205)

## Acceptance

- [ ] `cargo build` 通过
- [ ] `cargo test` 通过
- [ ] `cargo clippy --all-targets -- -D warnings` 通过
- [ ] `cargo fmt --check` 通过
- [ ] 审码 放行
- [ ] 手测:`RUST_LOG=quill::pty=trace cargo run` 启动后,日志在 1 秒内至少出现 1 行 "pty readable"
- [ ] 程序里无 `std::thread::spawn` / `tokio::spawn` —— 纯单线程 calloop(INV-005)
- [ ] 新增 `tests/pty_calloop_smoke.rs`(或归到 `event_loop_smoke.rs`):构造一个临时 pty + `calloop::EventLoop`,写码侧对 master 端发几个字节(直接 write syscall),证明 calloop 能拿到 readable 事件;不依赖真实 Wayland

## Context

- `CLAUDE.md` —— 架构不变式 1:所有 IO fd 必须进同一 `calloop::EventLoop`
- `docs/invariants.md` —— INV-005(calloop 唯一调度器)+ INV-001(State 字段顺序)
- `ROADMAP.md` Phase 2 T-0202
- `src/event_loop.rs::Core<State>` —— 提供 `handle()` 拿 `LoopHandle`,直接 `insert_source`
- `calloop::generic::Generic` docs:https://docs.rs/calloop/latest/calloop/generic/struct.Generic.html

## Implementation notes

- `Generic::new(fd, ...)` 吃 `RawFd` 后 **不会** 在 drop 时关 fd(PtyHandle 继续持有所有权)。这个边界搞清楚,不要 double-close
- Interest 只开 `READ`,`WRITE` 放到 Phase 3 键盘输入接入时再加
- `Mode::Level` 比 `Mode::Edge` 安全:即使回调一次没读完,下一轮 dispatch 会再触发。Edge 要求回调必须循环读到 `EAGAIN`,出错概率高
- 回调签名需要处理 `calloop::PostAction`,占位返回 `Ok(PostAction::Continue)` 即可
- PTY 刚起来时 bash 会打 prompt → master 变可读 → 你的回调触发一次。若 1s 内未触发,说明注册路径有问题
- 本 ticket 不动 `src/wl/render.rs`
