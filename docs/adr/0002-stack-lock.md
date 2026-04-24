# ADR 0002: 技术栈锁定

## Status

Accepted, 2026-04-24

## Context

Phase 1 起 写码 会并行开工。若每个 ticket 都允许自由选 crate,必然在 Wayland 客户端、
事件循环、GPU 抽象三条轴上发生选择冲突,最终导致架构碎裂。需要在 Phase 0 就把主干依赖
一次锁死,后续通过 ADR 才能更换。

## Decision

锁定以下主干依赖(版本不锁,由 `Cargo.toml` 与 `Cargo.lock` 决定;主版本升级需新 ADR,
小版本与 patch 版本升级不需要):

| crate | 职责 | 一句话理由 |
|---|---|---|
| `smithay-client-toolkit` | Wayland 客户端抽象 | 封装了 registry / globals / seat / output / xdg-shell 的公共样板,避免手糊 |
| `calloop` | 单线程事件循环 | Smithay 同作者,与 `wayland-client` 原生配对,天然单线程 `epoll/ppoll` 语义匹配本项目不变式 |
| `wgpu` | GPU 渲染后端 | 6K 分辨率下 CPU 渲染不现实,且在 NVIDIA/AMD/Intel/Vulkan/GL 上有可切换 backend 当 fallback |
| `alacritty_terminal` | VT 解析 + 屏幕状态 | Alacritty 抽出的独立 crate,生产级 VT100/xterm 行为,重写不现实 |
| `cosmic-text` | 字体 shaping + CJK fallback | 唯一把 shaping + bidi + CJK fallback + 字形缓存一把做好的 Rust crate |
| `portable-pty` | PTY 管理 | fork / openpty / waitpid 的 corner case 已经踩过,比自己 libc 调用风险低 |
| `tracing` + `tracing-subscriber` | 结构化日志 | Rust 生态事实标准,与 `env_logger` 相比支持 span / field / 异步友好 |

## Alternatives

- **`wayland-client` 裸用** — Rejected。粒度过低,registry 绑定 / seat 管理 / buffer pool
  都要手写,smithay-client-toolkit 已经把这层抽象做好。
- **`winit`** — Rejected。过度通用的跨平台窗口库,事件循环由其自己持有,本项目要求
  "所有 IO fd 注册到同一个 calloop"这一不变式与之冲突。
- **`smithay` 全家桶(server 端)** — Rejected。方向反了,smithay 是用来写 compositor
  的,我们是 client。
- **`rusttype` / `ab-glyph`** — Rejected。仅做 glyph 光栅化,不做 shaping,CJK 无法正确
  排版(bidi / 组合字符 / fallback 全缺)。
- **手撸 pty / `nix::pty::openpty`** — Rejected。SIGCHLD 处理、控制终端设置、
  `ioctl(TIOCSCTTY)` 顺序错误会导致 shell 无 job control,portable-pty 内部已处理。
- **`env_logger` / `log`** — Rejected。无 span / 无结构化字段,长跑 session 排查
  starvation / 内存增长这类问题时信息不够。

## Consequences

- 新增任何主干 crate(例如更换 `wgpu` 为 `vulkano`)必须新开 ADR,审码 会挡。
- 小版本号与 patch 号升级由 写码 在 ticket 内完成,不需 ADR,但必须在 commit
  message 说明升级理由。
- Teammate spawn prompt 必须包含本文件路径,防止 写码 自行选型。
- 这些 crate 各自的不兼容变更会成为 Lead 的维护负担,特别是 `cosmic-text` 与
  `wgpu`(较新,breaking change 频繁),升级时要准备回滚路径。
