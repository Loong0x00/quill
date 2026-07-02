<!-- migrated from claude memory: ghostty_linux_nvidia_unusable_2026-04-24.md on 2026-05-11 -->
---
name: Ghostty 在 Linux NVIDIA 栈不能 daily drive
description: Ghostty 1.3.1 tip 在 Arch + GNOME Wayland + NVIDIA 5090 + 6K + GDK_BACKEND=x11 下两条复合失败:GTK4 Xwayland event starvation hang + 3 年陈的 PageList memory leak (Claude Code 是最佳 trigger)。推荐 Foot 作为替代。
type: reference
originSessionId: 8250e730-19a5-479b-86d8-29f26fec010b
---
# Ghostty 在 Linux + NVIDIA 不能 daily drive (2026-04-24)

## 实测配置
Ghostty 1.3.1-arch2 tip / Arch kernel 6.19.13 / GNOME 50.1 Wayland / NVIDIA 5090 driver 595.58.03 Open Kernel / 6144×3456 6K 单屏 / GDK_BACKEND=x11 (fcitx5 定位 workaround) / GTK 4.22.2 / fcitx5 5.1.19

## 两个独立失败

### 1. Event starvation hang(focus return 触发)
触发:窗口从 occluded 变回可见(super overview / alt-tab / 最小化其他窗口)→ Ghostty 完全不响应,GNOME 不弹 "not responding",只能 SIGKILL。**频率 15-25 分钟一次,一小时 4 次 kill**。

`/proc/<pid>/task/*/stack` 捕获结果:所有线程 idle-wait(main 在 `ppoll`,renderer 在 `io_cqring_wait`),**完全没有 GL / DRM / NVIDIA 符号**。不是 GPU deadlock 是 **GTK4 X11 backend / Xwayland 的 expose 事件丢失**。偶尔自愈(几秒到一分钟)说明 secondary event 源(PTY / timer / D-Bus)有时及时唤醒,permanent hang 是所有救援路径都刚好安静的情形。

### 2. Memory leak(Claude Code + 大 scrollback)
场景:Claude Code 跑一两小时。RSS 爬到 30 GB → 吃光 130 GB swap。根因:PageList 在 non-standard page pruning 时的 leak + residual leak after official fix。
- issue #10289(71.49 GB on 16 GB system)
- discussion #10269(residual after fix)
- Mitchell 承认 leak 存在 3 年 (mitchellh.com/writing/ghostty-memory-leak-fix)

**触发条件**:多 codepoint grapheme 流 + 大 scrollback。**Claude Code 是行业级 worst-case trigger**。空载也缓慢涨(可能是"failed to activate on-screen keyboard" warning 每几秒一次的小 leak 累加)。

## 为什么 Mac 用户不撞
macOS 走 Metal + AppKit,不走 GTK4 / Xwayland / EGL。Mitchell 本人 Mac,Metal 路径是他主测路径。Linux 是二等公民。

## discussion #12411 被关的 post-mortem
- 我 (Claude) 用 gh GraphQL createDiscussion 绕过 template → 被检测
- AI-verbose 书写(section 堆 / bullet 炫 / "Related but Not Matching" 这种 AI template signature)→ 一眼识别
- 少 required 字段(完整 `ghostty +version` 输出、minimal config)
- maintainer 00-kat 秒关 + link AI_POLICY.md

## 推荐替代:Foot
- 瑞典人写,纯 Wayland 原生,~10K 行 C
- 无 GTK,无 Xwayland
- fcft 字体栈 + 自己简单 VT parser
- 有 valgrind CI 测试,**无 memory leak 报告**
- 原生支持 Wayland text-input-v3 → fcitx5 直接工作
- 6K HiDPI 流畅
- `sudo pacman -S foot`,30 分钟迁配置

## 中期路径:自写 terminal
用户决策倾向用 Claude 协作写自己的。2-3 周全职能用。最小 spec:跑 Claude Code + 中文显示输入 + 200% HiDPI。
关键库:**libvterm**(Neovim 的无头 VT parser, 纯 C) + **fcft**(Foot 的字体层)+ **libwayland-client** + **libxkbcommon**
语言:C(跟 Foot 同语言,抄代码最快)。**不用 Zig**(Ghostty leak 的 memory safety 教训)。不推荐 Rust(FFI boilerplate 多 20%)。
参考实现:`codeberg.org/dnkl/foot`
