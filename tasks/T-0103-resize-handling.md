# T-0103 窗口缩放时表面自动跟随

**Phase**: 1
**Assigned**:
**Status**: open
**Budget**: tokenBudget=100k, walltime=3600s, cost=$5
**Dependencies**: T-0101, T-0102

## Goal

用户用鼠标拖拽窗口边缘把窗口放大或缩小时,窗口里的深蓝色区域始终铺满整个客户区,没有黑边、没有被拉伸的马赛克、没有一瞬间的空白帧。把窗口拉很大再拉很小来回几次,程序不卡死不崩溃,窗口内容持续正常。

## Scope

- In:
  - 修改 `src/wl/window.rs` 或渲染模块:监听 XDG toplevel 的 `configure` 事件,拿到新尺寸后通知渲染层
  - 修改渲染模块:收到新尺寸时调用 `surface.configure(&device, &new_config)` 并在下一帧前完成
  - 忽略 0 宽或 0 高的 configure(某些 compositor 最小化时会发)
  - 更新 main.rs 把尺寸变化路由进渲染模块
- Out:
  - 多显示器上 `wl_output.scale` 变化处理(Phase 4 HiDPI)
  - 分数缩放(fractional scale)
  - 帧缓冲之外任何与尺寸相关的数据结构(文字网格、字体 atlas,这些都还不存在)
  - 最小化 / 最大化时的特殊处理,只跟着 compositor 给的尺寸走

## Acceptance

- [ ] `cargo build` 通过
- [ ] `cargo test` 通过
- [ ] `cargo clippy -- -D warnings` 通过
- [ ] `cargo fmt --check` 通过
- [ ] 审码 放行
- [ ] 手测:窗口拉伸过程中深蓝色持续铺满,不出现黑边
- [ ] 手测:快速连续拉伸 10 秒程序不 panic,没有 wgpu validation error(用 `WGPU_VALIDATION=1` 或 `RUST_LOG=wgpu_core=warn` 确认)
- [ ] 0 宽或 0 高的 configure 事件被忽略,不触发 `surface.configure` 崩溃
- [ ] 单元测试:resize 处理的纯逻辑函数(例如 "收到 (w,h) 后决定是否调用 reconfigure")可单测

## Context

- `CLAUDE.md` — "架构 不变式":渲染线程不做 >1ms 计算,resize 要在下一帧前完成但不阻塞事件循环
- `ROADMAP.md` Phase 1 `T-0103`
- `wgpu` surface resize 文档:https://docs.rs/wgpu/latest/wgpu/struct.Surface.html#method.configure
- SCTK `XdgShell` configure 事件:https://docs.rs/smithay-client-toolkit/latest/smithay_client_toolkit/shell/xdg/window/index.html

## Implementation notes

- wgpu 的 surface 在 resize 之后一定要先 `surface.configure`,否则下一次 `get_current_texture` 会报 `Outdated`
- `get_current_texture` 返回 `SurfaceError::Outdated` 或 `Lost` 时要自愈:重新 configure 后跳过本帧,不 panic
- XDG configure 可能发 `(0, 0)` 意为"由 client 自己决定",此时用上一次已知尺寸或初始 800x600,不要传 0 给 wgpu
- configure 事件来得比 frame callback 频繁,不要每个 configure 都立刻 reconfigure,用脏标记,渲染前 check 一次
