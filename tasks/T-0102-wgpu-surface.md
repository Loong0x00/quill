# T-0102 wgpu 绑定 Wayland 表面并每帧画纯色

**Phase**: 1
**Assigned**: impl-t0102
**Status**: in-review
**Budget**: tokenBudget=100k, walltime=3600s, cost=$5
**Dependencies**: T-0101

## Goal

运行 `cargo run` 后弹出的 Wayland 窗口整块区域被填成一个固定的纯深蓝色(例如 `#0a1030`)。窗口打开后颜色立刻覆盖整个客户区,没有白闪、没有透明黑块、没有花屏。本 ticket 不要求颜色会变动、也不要求响应任何输入,只要求"窗口一出来就是深蓝,看起来就是一块色块"。

## Scope

- In:
  - 新增 `src/wl/render.rs`(或 `src/render/mod.rs`,由 写码 决定,保持跟 CLAUDE.md 架构一致即可):封装 wgpu `Instance`、`Surface`、`Adapter`、`Device`、`Queue`、`SurfaceConfiguration`
  - 从 `WlSurface` 的 `RawWindowHandle` / `RawDisplayHandle`(或 wgpu 0.19+ 的 `SurfaceTargetUnsafe`)构造 wgpu `Surface`
  - 每次收到 XDG `configure` 后以当前窗口尺寸配置 surface,提交一个清屏命令(`LoadOp::Clear(<深蓝>)`)然后 `present()`
  - 修改 `src/main.rs`:在事件循环里触发渲染(可用 `wl_surface.frame()` 回调或收到 configure 后 draw)
  - 修改 `Cargo.toml`:加 `wgpu`、`pollster`(同步初始化 adapter/device),必要的 `raw-window-handle`
- Out:
  - resize 后的 surface 重配置(T-0103,本 ticket 允许 resize 后花屏,只要初次显示正确)
  - 关闭事件下 GPU 资源优雅释放(T-0104)
  - 任何文字、字形、纹理,只有 clear color
  - 帧率统计 / tracing 输出(T-0106)
  - 性能优化(vsync 策略选择除外,选 `PresentMode::Fifo` 即可)

## Acceptance

- [ ] `cargo build` 通过
- [ ] `cargo test` 通过
- [ ] `cargo clippy -- -D warnings` 通过
- [ ] `cargo fmt --check` 通过
- [ ] 审码 放行
- [ ] `cargo run` 后窗口客户区显示为约定的纯深蓝色,整屏无花、无白闪
- [ ] wgpu 初始化失败(例如 GPU 驱动异常)时用 `anyhow::Error` 返回并打印 `tracing::error!`,不 panic
- [ ] 单元测试:有一个测试验证"颜色常量存在且为预期 RGBA"这种可单测的纯函数逻辑(实际 GPU 绘制无法在 CI 验证,不强求)

## Context

- `CLAUDE.md` — "技术栈":渲染锁 `wgpu`
- `CLAUDE.md` — "架构"图:`WinBackend` 到 `draw()` 的调用路径
- `docs/adr/0002-stack-lock.md` — wgpu 可在 Vulkan / GL 间 fallback 的理由
- `ROADMAP.md` Phase 1 "潜在坑":NVIDIA Wayland 可能需要 `WGPU_BACKEND=vulkan`
- `wgpu` docs:https://docs.rs/wgpu
- `wgpu` surface on Wayland 示例:https://github.com/gfx-rs/wgpu/tree/trunk/examples
- `raw-window-handle` docs:https://docs.rs/raw-window-handle

## Implementation notes

- wgpu `Instance::new` 默认把所有后端都开,NVIDIA Wayland 下遇到 GL backend hang 时 写码 本地可设 `WGPU_BACKEND=vulkan` 排查,代码不 hardcode 后端
- `adapter.request_device` 用最保守的 `Features::empty()`、`Limits::downlevel_defaults()`,Phase 1 不用高级特性
- `PresentMode::Fifo` 保证 vsync,不要用 `Mailbox` / `Immediate`
- SCTK 不实现 `raw-window-handle`,需要手工构造 `RawWindowHandle::Wayland` 与 `RawDisplayHandle::Wayland`,注意 wgpu `SurfaceTargetUnsafe::RawHandle` 是 `unsafe`,加 `// SAFETY:` 注释
- adapter / device 初始化是 async,用 `pollster::block_on` 同步化,不要拉 tokio
