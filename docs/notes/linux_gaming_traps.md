<!-- migrated from claude memory: linux_gaming_traps_2026-04-23.md on 2026-05-11 -->
---
name: Linux 游戏 + Proton 陷阱集 (2026-04-23)
description: 今天踩的几个 Linux 游戏坑的合辑: Xid 109 真因 (GPU 争抢, 不是 bus_lock) + split_lock_detect=off + GE-Proton 10.30-34 wayland regression + FH5 必须用 official Proton + gamescope 在 NVIDIA Wayland 的 -e flag 坑
type: reference
originSessionId: 8250e730-19a5-479b-86d8-29f26fec010b
---

# Linux 游戏 / GPU 共享踩坑合辑

## 1. ⚠️ Xid 109 CTX SWITCH TIMEOUT 真因 = GPU 争抢, 不是 bus_lock

2026-04-23 上午 debug 时我**先怀疑 bus_lock/split_lock**, 帮用户加了 grub `split_lock_detect=off`. 但**那不是真因**.

**真因**: ML 训练 (99% GPU util, 20GB VRAM, 352W) + 星铁游戏 (同一张 5090) 同时跑 → NVIDIA 内部 context switch 超时 → Xid 109. 这是 GPU **单卡多重工作负载**的必然冲突, 不是 CPU 级别的 bus_lock.

识别信号:
```
NVRM: Xid (PCI:0000:01:00): 109, pid=XXXX, name=StarRail.exe, channel 0x000000X, errorString CTX SWITCH TIMEOUT
```

真正解法 (按代价升序):
1. **训练和游戏物理分开** — 不同 GPU, 或不同时间
2. **把游戏路由到 iGPU** — DXVK_FILTER_DEVICE_NAME, 详见 `9950x3d_igpu_gaming_setup_2026-04-23.md`
3. 牺牲其中一个 (停训练 / 停游戏)

**不要**再建议 `split_lock_detect=off` 作 Xid 109 的 fix — 那改了也没用, bus_lock 跟 GPU context switch 是两回事.

## 2. split_lock_detect=off (还是加了, 但是为另一原因)

Steam CHTTPClient 确实大量 bus lock, kernel 默认 warn 模式下 msleep(20) 每个 excess trap, 累积让线程变慢. 加 `split_lock_detect=off` 确实消除这个性能损失. 只是**不是 Xid 109 的修复**.

Grub cmdline 已加:
```
/etc/default/grub:
GRUB_CMDLINE_LINUX_DEFAULT="... split_lock_detect=off"
```

## 3. GE-Proton 10.30-10.34 Wayland regression 期

**2026-02 到 2026-04** GE-Proton 10 系列在 winewayland 层频繁 breakage:
- 10-30 (02-10): winewayland patches 更新
- 10-31 (02-15): 回退 10-30 systray patch (许多游戏 breakage)
- 10-33 (03-18): 又改 wine-wayland
- 10-34 (03-23, **我们这版**): 加 PROTON_WAYLAND_MONITOR env var

这段时间 **FH5 在 GE-Proton 10-34 直接 warning 屏崩**. 官方 Proton 10 stable 正常.

**结论 / how to apply**:
- GE-Proton 默认全局用可以, 但**遇到游戏启动崩/黑屏先切官方 Proton**, 不要假设 GE 是"更好"
- Steam 库 → 游戏 → 属性 → 兼容性 → 勾 "强制使用 Steam Play 特定版本" → 选 Proton 10 stable
- 这是 per-game override, 其他游戏仍用全局 GE
- FH5 实测 Proton 10 stable OK, GE-Proton 10-34 崩
- 推迟升级 GE 到 11 + 系列前不要 panic, 等新 wine-wayland 稳定

## 4. gamescope 在 NVIDIA Wayland 的坑

2026-04-23 gamescope 3.16.23 测试结果 (NVIDIA Wayland, 5090 + 6K):

| gamescope 参数 | 结果 |
|---|---|
| `gamescope -- sleep` (无 game) | ✓ 能启动退出 |
| `gamescope -- %command%` (包 FH5) | 黑色小窗口, 游戏没渲染出来 |
| `gamescope -W 2560 -H 1440 -f -e -- %command%` (完整) | Steam 秒退 (看起来游戏没启动) |
| 不用 gamescope, 切 Proton stable | ✓ FH5 正常启动 |

**推测**: gamescope 的 **-e flag (Steam 集成)** 在 NVIDIA Wayland 下协议协商失败. 单独 gamescope 能跑但 FH5 在里面渲染黑.

**结论**: NVIDIA Wayland 下 gamescope 不是"免费可用", 不是所有游戏都能无脑包. 如果多显示器导致游戏问题, **优先换 Proton 版本而不是上 gamescope**.

## 5. FH5 6K 实测性能 (5090 + Proton stable)

```
6K native (6144x3456) + 全高画质 + DLSS Quality → 80 fps
```

FH5 是 AAA 里优化天花板级. ForzaTech 引擎 (非 UE5), 不堆 Nanite/Lumen, 每帧精算. 5090 + DLSS Q 6K 80fps 是**合理**结果.

对比: 同机跑黑神话 4K + PT 原生 20-28 fps (必须 DLSS Performance + Frame Gen). UE5 堆料游戏跟 FH5 不是一个时代的优化态度.

## 关联记忆

- `9950x3d_igpu_gaming_setup_2026-04-23.md` — iGPU 路由游戏 (Xid 109 的最佳解)
- `nvidia_gsp_bug.md` — 5090 GPU hang 的独立问题 (非今天这个)
- `nvidia_hdmi_audio_requires_display_2026-04-23.md` — HDMI audio 要 display 才启用
