# T-0101 创建 Wayland 窗口

**Phase**: 1
**Assigned**: 写码 A
**Status**: merged
**Budget**: tokenBudget=100k, walltime=3600s, cost=$5
**Dependencies**: 无

## Goal

运行 `cargo run` 后屏幕上出现一个 Wayland 窗口,标题栏显示 "quill",窗口里面目前是空白(没有内容也允许是 compositor 默认的未定义色)。窗口可以被 compositor 移动、最小化。点窗口右上角叉程序必须退出,不留僵尸进程。**本 ticket 只负责把窗口"拉起来",里面画什么交给 T-0102**,所以窗口内显示什么都不算 bug,只要窗口本身存在、可被关闭即可。

## Scope

- In:
  - 新增 `src/wl/mod.rs` 与 `src/wl/window.rs`:封装 `smithay-client-toolkit` 的 `Connection`、`EventQueue`、`CompositorState`、`XdgShell`、`WlSurface`、`XdgToplevel`
  - 修改 `src/main.rs`:启动时建 Wayland 连接、创建窗口、进入阻塞事件循环(本 ticket 允许先用 SCTK 提供的简单 dispatch,T-0105 再迁到 calloop)
  - 修改 `Cargo.toml`:加 `smithay-client-toolkit`、`wayland-client`、`anyhow` 依赖
- Out:
  - wgpu / GPU 渲染(T-0102)
  - resize 事件的 wgpu 重配置(T-0103)
  - 关闭事件的优雅资源释放(T-0104,本 ticket 只要求进程能退出,不要求所有资源按顺序 drop)
  - `calloop::EventLoop` 统一集成(T-0105)
  - 多显示器、fractional scaling、HiDPI 缩放

## Acceptance

- [ ] `cargo build` 通过
- [ ] `cargo test` 通过
- [ ] `cargo clippy -- -D warnings` 通过
- [ ] `cargo fmt --check` 通过
- [ ] 审码 放行
- [ ] `cargo run` 后能在 Wayland 桌面上看见一个标题为 "quill" 的窗口
- [ ] 点窗口右上角叉程序退出,退出码 0,没有 panic
- [ ] 运行后 `pgrep quill` 无残留进程
- [ ] 窗口模块对外只导出创建窗口的函数和必要句柄类型,不漏 `wayland-client` 的原始类型到 main

## Context

- `CLAUDE.md` — "技术栈"一节:Wayland 客户端锁 `smithay-client-toolkit`
- `CLAUDE.md` — "架构"一节:`wl/` 模块切分、所有 IO 走同一循环(本 ticket 先接 SCTK 的 dispatch,T-0105 合并)
- `CLAUDE.md` — "开发准则":禁止 `unwrap` / `expect`,用 `?` + `anyhow`
- `docs/adr/0002-stack-lock.md` — 为啥锁 SCTK,不用 winit
- `smithay-client-toolkit` docs:https://docs.rs/smithay-client-toolkit
- `wayland-client` docs:https://docs.rs/wayland-client
- SCTK 官方示例 `simple_window.rs`:https://github.com/Smithay/client-toolkit/blob/master/examples/simple_window.rs

## Implementation notes

- SCTK 的 `delegate_*!` 宏族是必经之路,照 `simple_window.rs` 抄,不自己卷 Dispatch
- XDG toplevel 的 `configure` 事件第一次到达之前不要 commit surface,否则某些 compositor 不画窗口
- 窗口初始大小先硬编码 `800x600`,不从环境变量读
- 本 ticket 的事件循环可以是 `while !should_exit { event_queue.blocking_dispatch(&mut state)?; }` 这种最简形态,T-0105 会替换掉
