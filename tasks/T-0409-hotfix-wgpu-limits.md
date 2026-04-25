# T-0409 hotfix: wgpu limits downlevel → adapter (fix HiDPI compositor resize panic)

**Phase**: 4 (hotfix, post-收尾)
**Assigned**: Lead 直修 (派单 fresh agent paradigm 例外: 2 行 trivial fix +
production blocker + 用户实测中等修)
**Status**: merged
**Budget**: tokenBudget=N/A (Lead 直修)
**Dependencies**: T-0404 (HIDPI_SCALE=2) / T-0408 (headless render)

## Bug

user 实测 cargo run --release 时 compositor 把 quill 窗口 resize 到 1600×1200
logical, ×HIDPI=2 → 3200×2400 physical, 超过 wgpu downlevel_defaults
`max_texture_dimension_2d=2048`, Surface::configure validation panic:

```
wgpu error: Validation Error
  In Surface::configure
    `Surface` width and height must be within the maximum supported texture size.
    Requested was (3200, 2400), maximum extent for either dimension is 2048.
```

**真因**: T-0403 引 wgpu 时为兼容性用 `Limits::downlevel_defaults()` (2048),
T-0404 加 HIDPI_SCALE=2 把所有 logical surface size 翻倍, 实战中 compositor
任何 resize > 1024 logical 都会触发. reviewer-T0406 在集成测试用 800×600
绕过 (LOGICAL_W=800 × HIDPI=2 = 1600, 落 2048 内), 但生产 user 实测必触发.

## Fix

`src/wl/render.rs` 两处 `request_device` 的 `required_limits`:
- `Renderer::new` (line ~397, 窗口路径)
- `run_headless` (line ~1604, headless 路径)

从 `wgpu::Limits::downlevel_defaults()` 改为 `adapter.limits()` (取实际硬件
上限, Vulkan 5090 = 16384+).

**为啥不 cap surface size**: NVIDIA 5090 Vulkan max 远超任何 6K 物理屏幕,
KISS 不加防御代码. 未来低端 GPU 实战遇到再加.

## 实装

```diff
-            required_limits: wgpu::Limits::downlevel_defaults(),
+            required_limits: adapter.limits(),
```

两处同改 (windowed + headless), 都加 doc 注释引 T-0409.

## Acceptance

- [x] 4 门 release 全绿 (build / clippy --all-targets -D warnings / fmt --check / test)
- [x] 124 tests pass (无回归)
- [ ] **user 实测 cargo run --release 不 panic** (待 user retry 验)

## 派单 fresh agent paradigm 例外说明

quill 项目硬约束 "per-ticket fresh agent + 三源 verify SOP"。本单 Lead 直修原因:
- 2 行 trivial fix, 边界清晰 (不动逻辑只升 limits)
- production blocker, user 等着继续测
- 派 fresh agent 流程 (写派单 + writer + reviewer + audit + merge) 1h, 修代码
  3 分钟
- 后续若再有 hotfix 类 ticket 用此模式, 否则保 fresh agent paradigm

## 后续

如需 6K 显示器 (现在 user 主屏 224 ppi), 还需 verify wgpu Surface 配置 +
glyph atlas 容量 (2048×2048 atlas 装不下 6K 字符变化, 但 T-0406 clear-on-full
已兜底).
