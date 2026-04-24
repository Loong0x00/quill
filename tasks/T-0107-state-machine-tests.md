# T-0107 窗口状态机测试

**Phase**: 1
**Assigned**: impl-t0107
**Status**: in-review
**Budget**: tokenBudget=100k, walltime=3600s, cost=$5
**Dependencies**: T-0101, T-0103, T-0104, T-0105

## Goal

不启动真正的 Wayland 窗口,在 `cargo test` 下跑起一个"假装自己是 compositor"的测试工具,给程序的内部状态机按顺序喂进一系列事件(configure 给个初始大小 → resize 给个新大小 → 再 resize 给 0x0 应该忽略 → close 触发退出标志),验证每一步之后状态机里的字段按预期变化。跑 `cargo test` 时这批测试会被自动执行,不需要图形环境,CI / headless 机器上都能过。

测试覆盖的行为层面含义:写码 可以放心改 T-0101~T-0105 的实现细节,只要这批测试还能过,说明状态流转没坏。

## Scope

- In:
  - 新增 `tests/state_machine.rs`(集成测试目录)或在主 crate 内 `#[cfg(test)]` 模块里写,由 写码 决定
  - 把 T-0101~T-0105 里"收到 Wayland 事件 → 改状态"这部分纯逻辑抽成可单测的函数 / 方法,接受事件枚举 + `&mut State`,不依赖真实的 `WlSurface`
  - 写 4~6 个测试用例覆盖:初次 configure / resize / 0x0 被忽略 / close 置位退出标志 / 连续 resize 合并到单次脏标记
  - 为了让测试成立,可能需要在 T-0101~T-0105 的实现里加一层轻薄的抽象(例如事件枚举 `WindowEvent { Configure(u32,u32), Close, ... }`),写码 可以直接改这些文件
- Out:
  - 真正的 Wayland mock compositor(成本高,Phase 6 之后再看)
  - 渲染输出像素级比对(Phase 4 再做)
  - PTY 或子进程相关测试(Phase 2)
  - 性能测试 / benchmark

## Acceptance

- [ ] `cargo build` 通过
- [ ] `cargo test` 通过,包含本 ticket 新增的所有用例
- [ ] `cargo clippy -- -D warnings` 通过(含测试代码,`cargo clippy --all-targets`)
- [ ] `cargo fmt --check` 通过
- [ ] 审码 放行
- [ ] 测试不依赖 `DISPLAY` / `WAYLAND_DISPLAY` 环境变量,在 headless 下能过
- [ ] 覆盖的场景至少包括:初始 configure、后续 resize、0x0 configure 被吞、close 置位退出标志、连续 resize 合并
- [ ] 所有测试运行总时长 < 1 秒

## Context

- `CLAUDE.md` — "开发准则":先写测试再写实现,骨架可以 `todo!()`;本 ticket 是对 T-0101~T-0105 的测试补齐,视为那几张票的"后置保险"
- `CLAUDE.md` — "架构 不变式":单线程事件循环,方便单测
- `ROADMAP.md` Phase 1 `T-0107`
- Rust 集成测试文档:https://doc.rust-lang.org/book/ch11-03-test-organization.html
- `cargo test` 文档:https://doc.rust-lang.org/cargo/commands/cargo-test.html

## Implementation notes

- 测试里不要 `Connection::connect_to_env()`,这会试图连 `WAYLAND_DISPLAY`,headless 机会失败
- 把 Wayland 回调里改状态的核心函数抽成 `fn handle_event(state: &mut WindowState, ev: WindowEvent)`,测试直接调它
- `WindowEvent` 枚举是测试和实现的桥接,作为模块内部类型即可,不对外公开
- 不要用 `#[ignore]` 跳过真 GPU 测试,直接不写真 GPU 测试(本 ticket scope 只在 state machine 逻辑)
- 如果 T-0101~T-0105 实现者没抽好这一层,本 ticket 写码 有权力改动那几个文件,把接口抽出来,属于合理 refactor 不算 scope creep
