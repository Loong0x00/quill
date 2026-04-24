# T-0201 portable-pty spawn bash -l

**Phase**: 2
**Assigned**:
**Status**: open
**Budget**: tokenBudget=100k, walltime=3600s, cost=$5
**Dependencies**: 无(Phase 2 起点;可与 T-0104 并行,两者无文件重叠)

## Goal

`PtyHandle::spawn_shell(cols, rows)` 能在 Linux 上真起一个 `bash -l` 子进程,返回一个持有 master 端的句柄,句柄 `Drop` 时 master fd 关闭、子进程收到 SIGHUP 退出,不留僵尸。集成(被 `main.rs` 调一次,把返回值塞进 `State`)可以先用 `.expect` 失败直接 panic,也可以先不接入(本 ticket 不要求 `cargo run` 看到 shell,留给 T-0202/T-0203)。

单测 / 文档测试中可直接构造句柄并 drop 验证无残留子进程。

## Scope

- In:
  - `Cargo.toml` 添加 `portable-pty = "0.9"`(或当时最新稳定版,记录版本于 commit message;版本选择不需要 ADR —— portable-pty 已由 ADR 0002 锁定为 PTY crate)
  - `src/pty/mod.rs`:把 `PtyHandle` 占位 struct 改为真实结构,内部持有
    - `Box<dyn MasterPty + Send>`(master 端)
    - `Box<dyn Child + Send + Sync>`(子进程句柄)
    - 以及 T-0203 读字节必需的 `Box<dyn Read + Send>`(从 `master.try_clone_reader()` 拿)
  - 实装 `PtyHandle::spawn_shell(cols: u16, rows: u16) -> anyhow::Result<Self>`:
    1. `native_pty_system().openpty(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 })`
    2. 构造 `CommandBuilder::new("bash")`,`arg("-l")`,继承当前环境的 `TERM`(若未设置就 `TERM=xterm-256color`)
    3. `pair.slave.spawn_command(cmd)` 拿 `Child`
    4. **立即 drop `pair.slave`**(否则 master 读不到 EOF,子进程退出后也不会通知)
    5. 从 `pair.master` 拿 reader,组装 `PtyHandle`
  - 实装 `spawn_program(program: &str, args: &[&str], cols: u16, rows: u16)`:与 `spawn_shell` 共用 openpty + slave drop 逻辑,只是 `CommandBuilder` 不同。`spawn_shell` 内部可以直接调 `spawn_program("bash", &["-l"], cols, rows)`
  - 字段声明顺序要让 `Drop` 按"reader → master → child"顺序跑(reader 依赖 master fd,master drop 会触发 SIGHUP 给 child)—— 或显式 `impl Drop` 控制
  - **设置 master fd `O_NONBLOCK`**:在 `PtyHandle` 返回前调 `libc::fcntl(raw_fd, libc::F_SETFL, libc::O_NONBLOCK)`。用 `#[allow(unsafe_code)]` + `// SAFETY:` 注释块(参照 `src/wl/render.rs` 风格)。**不引入 nix crate**(ADR 0002 未锁 nix,加 crate 要 ADR,不值这一个调用)。理由:fd flag 必须随 fd 创建立即设,否则 T-0203 读字节时阻塞整个 event loop (Ghostty starve 坑的反面)
  - 单元 / doc 测试:构造 `PtyHandle::spawn_program("true", &[], 80, 24)`,`drop`,断言不 panic(不断字节;那是 T-0206 的事)
- Out:
  - master fd 注册 calloop(T-0202)
  - 读字节 tracing(T-0203)
  - resize ioctl(T-0204)
  - 子进程退出 → 窗口关(T-0205)
  - 修改 `src/main.rs` / `src/wl/` 接入渲染侧(留到 T-0202 一并改,本 ticket 只写模块)

## Acceptance

- [ ] `cargo build` 通过
- [ ] `cargo test` 通过(含新增 `pty::` 模块下的单测)
- [ ] `cargo clippy --all-targets -- -D warnings` 通过
- [ ] `cargo fmt --check` 通过
- [ ] 审码 放行
- [ ] 单测:调 `PtyHandle::spawn_program("true", &[], 80, 24)` 成功返回,立即 drop 后 `pgrep -f "true$"` 无残留(测试内可走 `child.wait()` 等回收)
- [ ] `spawn_shell` 的错误路径走 `anyhow::Context`,非 `unwrap/expect`(遵守 CLAUDE.md 开发准则)
- [ ] drop 顺序在注释里明写,并在 `docs/invariants.md` 追加 INV-008 "PtyHandle 内部字段 drop 顺序 reader → master → child"
- [ ] `docs/invariants.md` 另追加 INV-009 "PTY master fd 必须 O_NONBLOCK"(或等号,按当时最大 INV 编号 +1)
- [ ] 单测验证 O_NONBLOCK:spawn 后 `fcntl(F_GETFL)` 返回值与 O_NONBLOCK 按位与非零

## Context

- `CLAUDE.md` —— 技术栈锁 `portable-pty`,**禁止** `unwrap`/`expect` 在非测试代码
- `docs/adr/0002-stack-lock.md` —— 为啥锁 portable-pty 而非手撸 nix::pty::openpty
- `docs/invariants.md` —— INV-005 IO 唯一入口(calloop);本 ticket 不注册 fd 但要为 T-0202 留好 `raw_fd()` 出口
- `ROADMAP.md` Phase 2 T-0201
- `portable-pty` docs:https://docs.rs/portable-pty/latest/portable_pty/
- 关联 ticket:T-0202(会把本 ticket 产出的 `raw_fd()` 接进 calloop)

## Implementation notes

- `portable-pty` 的 `PtyPair { master, slave }` 必须先 `slave.spawn_command()` 再 `drop(pair.slave)`;**调换顺序**会导致 spawn 失败或 master 读不到 EOF。这是踩过的坑,写死在注释里
- `CommandBuilder::new("bash")` 默认 `cwd` 为当前进程的 cwd。若后续想让 quill 打开时落到 `$HOME`,留到 Phase 6 配置系统
- `try_clone_reader()` 返回的是独立 fd(dup),**不是**同一个 fd——对 fd 生命周期有影响,T-0202 注册 calloop 时要用 `master.as_raw_fd()` 拿到 **master** 的 fd,不是 reader 的 dup
- 不要在 `PtyHandle` 里存 `slave`;slave 只在 `spawn_command` 瞬间需要
- 子进程退出状态的获取放到 T-0205,本 ticket 不实装 `try_wait`
- 约 50-80 行代码(含注释与单测)
