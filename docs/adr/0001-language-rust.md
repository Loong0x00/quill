# ADR 0001: 语言选择 — Rust

## Status

Accepted, 2026-04-24

## Context

quill 是单用户 daily driver 终端,主要承载 Lead 的 Claude Code 长跑 session(连续运行 8h+)。
核心可用性要求:内存安全(无 UAF / double-free / 堆损坏导致的 session 崩溃)、长时间运行下 RSS 稳定。

项目开发模式是 AI orchestration:Lead 不手写代码,由 写码 teammate 产出 diff,
审码 审查。这一模式对"编译期能把类别的错误堵死"有很强依赖 —— 错误越是运行期才
暴露,反馈回路越长,AI 越容易在 token 预算内迷路。

同时,Wayland / PTY / GPU 渲染这条链路上必须存在成熟的可复用基础设施,否则项目会在
Phase 1-2 就卡在基础设施造轮子上。

## Decision

采用 **Rust**(stable 通道,由 `rust-toolchain.toml` 锁定)作为全栈语言。

理由:

- **borrow checker 是 AI 的编译期反馈回路**:类型与生命周期错误在 `cargo check` 阶段
  返回定位清晰的错误,写码 一两轮即可修正,不消耗长对话预算去追 run-time 崩溃。
- **Wayland crate 生态成熟**:`smithay-client-toolkit` / `wayland-client` / `calloop`
  由 Smithay 项目持续维护,和 Mutter / KWin 一样属于 Wayland 事实生态的一部分。
- **alacritty_terminal 可直接复用**:VT 解析 + grid + scrollback 这部分 Alacritty 已经
  拆出独立 crate,省下 Phase 3 的大块工作。
- **cosmic-text 解决 CJK**:COSMIC 桌面的字体子系统,shaping + fallback + bidi 已覆盖,
  不用自己接 HarfBuzz + FreeType。

## Alternatives

- **Zig** — Rejected。宣传"内存安全替代 C",但 Ghostty(Zig 写)的 PageList 内存泄露
  issue 已 3 年未修,说明 Zig 的安全承诺在 GC-free 环境下仍然要靠人脑不变式维护,
  与 Rust 的静态保证不同。同时 Zig 1.0 未至,语言和标准库处于经常变动期。
- **C** — Rejected。UAF / double-free / 堆溢出的排查负担完全压到 AI 与 Lead,属于不合理
  的工具选择,违背"编译期堵死"原则。
- **C++** — Rejected。现代 feature(C++17/20/23)分裂严重,不同编译器支持度不一,CMake
  / Meson / Bazel 构建系统碎片化,对 AI 生成代码不友好。智能指针 + RAII 仍不能静态防
  止悬垂引用。

## Consequences

- 工具链锁 Rust,团队与 teammate spawn prompt 必须写明 Rust 成语(`Result` / `?` /
  ownership / lifetime)与项目禁忌(`unwrap` / `expect` / `unsafe` 三限制)。
- 部分底层接口(Wayland scanner 生成的 C 结构、wgpu FFI)仍可能要求 `unsafe`,通过
  `// SAFETY:` 注释与局部 `#[allow(unsafe_code)]` 放行,main.rs 顶层 `#![forbid]` 保证
  非明确豁免的位置一律拒绝。
- 构建产物单一静态二进制,打包与分发无运行时依赖,方便 soak 与打磨阶段替换 daily driver。
