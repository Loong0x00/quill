# T-0802 resize 节流 + present_mode 优化 (修拖动延迟)

**Phase**: 8 (polish++, daily-drive feel)
**Assigned**: writer-T0802
**Status**: merged
**Budget**: tokenBudget=100k (跨 wgpu present_mode + Surface::configure 节流 + window resize event 处理)
**Dependencies**: T-0102 (Renderer init) / INV-006 (resize_dirty 单消费者)
**Priority**: P1 (user 实测拖窗口边卡顿明显, "巨大延迟和滑动, 手感不干脆")

## Bug

User 实测拖窗口 4 边 / 4 角 resize 时:
- 视觉延迟明显 (拖动 → 实际 resize 滞后 ~100ms)
- 拖动停后窗口仍"滑动"几帧
- 手感不干脆 (跟 alacritty / foot 紧跟拖动对比)

## 真因

quill 当前 resize 路径:
1. compositor 拖动时高频发 xdg_surface.configure (60Hz+ event)
2. 每个 configure → core.resize_dirty = true (INV-006 单 bool, 已 idempotent)
3. idle callback `propagate_resize_if_dirty` 消费 dirty:
   - wgpu Surface::configure (重建 SwapChain Texture, ~10ms cost)
   - PtyHandle::resize (TIOCSWINSZ ioctl, ~1ms)
   - TermState::resize (alacritty Term 重排 grid, ~5ms)
4. render → present (waitUntilFinished 阻塞 vsync)

单帧 resize cost ~15-20ms, 高频 configure event 让累积 lag 视觉感受明显.

## Goal

调 wgpu present_mode + 加 resize 节流 + 释放期高分辨率 fast path, 让拖动
跟 alacritty / foot 一样紧跟. 完工后 user 实测拖动无延迟无滑动.

## Scope

### In

#### A. wgpu present_mode 调成 Mailbox (推) 或 Fifo
- 当前 Renderer::new 用默认 PresentMode (可能是 Fifo = vsync 阻塞)
- 改 Mailbox: GPU 渲染好下一帧立即替换, 不阻塞 vsync, 减 stutter
- 兼容性: NVIDIA Vulkan 5090 支持 Mailbox, fallback Fifo if 不支持
- 改动: src/wl/render.rs::Renderer::new 选 PresentMode 时偏好 Mailbox

#### B. resize 节流: 高频 configure 只处理最后一次
- INV-006 resize_dirty 已是 single bool, configure 频繁来仅置 true 一次
- 但 propagate_resize_if_dirty 在 idle callback 里调, **每帧** drive_wayland 都跑一次, 高频 configure 对应高频 propagate
- 加节流: propagate 后记最后处理时间, 60ms 内不再 propagate (减到 ~16Hz)
- 或: 只在 stop dragging 后 propagate (但 wayland 协议无 "stop dragging" event, 启发式靠 timeout)

**简化方案** (推 KISS): 加 60ms 节流 (16Hz), 拖动期间只更新 cell layout 不
configure surface, idle 60ms 后真 configure.

#### C. (可选) Surface::configure 跳过 同尺寸重 configure
- 当前 propagate_resize_if_dirty 收到 configure 不查尺寸是否真变, 直接重 configure
- 加判断: 新 size == 当前 config size → 跳过 surface.configure (省 ~10ms cost)
- INV-006 既定 dirty 不一定意味"尺寸真变" (compositor focus 切等也发 configure), 这个跳过是对的

#### D. 测试
- src/wl/render.rs lib 单测: present_mode 选 Mailbox if 支持 (mock adapter caps)
- src/wl/window.rs lib 单测: resize 节流逻辑 (60ms 内多次 dirty 只处理 1 次)
- 集成测试不易 (wayland resize 是 compositor 侧驱动)

#### E. 手测 deliverable
- cargo run --release 在 GNOME 拖窗口 4 边 + 4 角, 视觉延迟 < 30ms (跟 alacritty / foot 接近)

### Out

- **不做**: 拖动期间低分辨率 fast path (Phase 8+ 复杂度高)
- **不做**: 改 Vulkan triple buffering 配置 (wgpu 抽象层)
- **不动**: src/text / src/pty / src/wl/pointer.rs / src/wl/keyboard.rs / docs/invariants.md / Cargo.toml

### 跟 T-0801 协调
- T-0801 改 src/text/mod.rs shape_line + render glyph 渲染段
- 你改 render present_mode + window resize 节流, 不冲突
- 顶部 imports 可能小冲突, Lead 合并时手解

## Acceptance

- [ ] 4 门 release 全绿
- [ ] present_mode Mailbox 偏好 (cap 不支持 fallback Fifo + tracing log)
- [ ] resize 节流 60ms (单测验)
- [ ] 总测试 298 + ≥2 ≈ 300+ pass
- [ ] **手测**: cargo run --release 拖窗口 4 边 / 4 角 — 视觉延迟 < 30ms 跟 alacritty 接近
- [ ] 审码放行

## 必读

1. /home/user/quill-impl-resize-throttle/CLAUDE.md
2. /home/user/quill-impl-resize-throttle/docs/conventions.md
3. /home/user/quill-impl-resize-throttle/docs/invariants.md (INV-006 resize_dirty)
4. /home/user/quill-impl-resize-throttle/tasks/T-0802-resize-throttle-present-mode.md (派单)
5. /home/user/quill-impl-resize-throttle/docs/audit/2026-04-26-T-0701-review.md (resize 协议接入)
6. /home/user/quill-impl-resize-throttle/src/wl/render.rs (Renderer::new + resize)
7. /home/user/quill-impl-resize-throttle/src/wl/window.rs (propagate_resize_if_dirty)
8. WebFetch https://docs.rs/wgpu/0.29/wgpu/enum.PresentMode.html

## 已知陷阱

- Mailbox 在 NVIDIA Vulkan 大概率支持, AMD / Intel / wlroots 也广泛支持. 保险走 cap 检查 + fallback
- 节流 60ms 太长会让小 resize 卡顿 (用户拖一下 60ms 内不变), 30ms 平衡好
- INV-006 不破: dirty 仍是 single bool, 节流是 propagate 调用频率限制不是 dirty 状态修改
- 不要 git add -A
- HEREDOC commit
- ASCII name writer-T0802

## 路由

writer name = "writer-T0802"。

## 预算

token=100k, wallclock=2h.
