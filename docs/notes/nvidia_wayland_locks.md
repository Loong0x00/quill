<!-- migrated from claude memory: nvidia_wayland_4_locks_2026-04-23.md on 2026-05-11 -->
---
name: NVIDIA Wayland 显示被锁的四层配置陷阱
description: 2026-04-23 修复. 用户为"GPU 挂了 GUI 不崩"设过 4 道锁把 NVIDIA 完全排除出显示 (2 udev rules + 2 env vars + 物理线接 iGPU). 想用 5090 驱 6K 必须全拆. NVIDIA Wayland 595.58 本身没 bug, 是 mesa libEGL 被强制接管了 NVIDIA card
type: reference
originSessionId: 8250e730-19a5-479b-86d8-29f26fec010b
---

# 为什么 NVIDIA Wayland 不工作 — 其实是你自己上的 4 道锁

## 起因

用户历史上遇到过 NVIDIA GSP firmware bug (GPU hang 后无法 reset, 只能 SBR 整个 PCIe 域, 详见 `nvidia_gsp_bug.md`). 为保证 GPU 崩了也能用 GUI, 把 display 彻底锁到 iGPU HDMI, NVIDIA 只做 CUDA compute. 用了 4 层独立机制防止 mutter 误接 NVIDIA.

2026-04-23 接 LG 32U990A 6K 显示器, 需要 5090 DP-1 才能驱 6K60, 于是发现**四层锁一起拦**. 每层单独看都看不出问题, 必须全拆.

## 四道锁位置

| 层 | 文件 / 位置 | 作用 |
|---|---|---|
| L1 | `/etc/environment.d/10-igpu-desktop.conf` | `__EGL_VENDOR_LIBRARY_FILENAMES=.../50_mesa.json` — 屏蔽 NVIDIA EGL ICD (**核心锁**, 导致 "driver (null)" 报错) |
| L1 | 同上文件 | `DRI_PRIME=0` — 锁定 default render device |
| L2 | `/etc/udev/rules.d/99-nvidia-mutter-ignore.rules` | `SUBSYSTEM=="drm", DRIVERS=="nvidia", TAG+="mutter-device-ignore"` — 告诉 mutter 跳过 NVIDIA DRM |
| L3 | `/etc/udev/rules.d/61-gpu-default.rules` | `ENV{DEVNAME}=="/dev/dri/card0", TAG+="mutter-device-preferred-primary"` — 强制 card0 为 primary |
| L4 | 物理连接 | HDMI 线接主板 iGPU 而非 5090 |

## 诊断症状 (未来识别)

mutter / gnome-shell 日志里出现:
```
libEGL warning: pci id for fd N: 10de:XXXX, driver (null)
libEGL warning: egl: failed to create dri2 screen
Failed to setup: The GPU /dev/dri/card1 chosen as primary is not supported by EGL.
```
→ **不是 driver bug**, 是 GLVND 被 env 强制只看 mesa ICD, 接到 NVIDIA card 找不到 driver.

快速验证: `env | grep __EGL_VENDOR_LIBRARY_FILENAMES` — 如果只有 `50_mesa.json`, NVIDIA 被锁死.

## 修复步骤 (canonical)

```bash
# 1. 禁用 4 道锁 (rename 成 .disabled 便于 rollback)
sudo mv /etc/environment.d/10-igpu-desktop.conf{,.disabled}
sudo mv /etc/udev/rules.d/99-nvidia-mutter-ignore.rules{,.disabled}
sudo mv /etc/udev/rules.d/61-gpu-default.rules{,.disabled}
sudo udevadm control --reload-rules
sudo udevadm trigger --subsystem-match=drm

# 2. 物理拔 HDMI 线 (iGPU), 换成 DP 线接 5090 DP-1

# 3. reboot (env 变更需要 fresh systemd session, logout 不够)

# 4. 在主 @ subvol 启动, NOT snapshot
```

### 预留 rescue 脚本 `/root/rescue-igpu.sh`

万一重启后 NVIDIA Wayland 挂了, TTY2 跑这个秒回 iGPU 模式 (把 .disabled 都 rename 回去 + reload + reboot).

## 重要 trap: snapshot 和 live @ 是独立 subvol

用户如果启动时不小心进了 btrfs snapshot (timeshift), `/etc/` 是快照的内容, **今天改的 live @ udev/environment 都看不见**. 要看 live state 需要:
```bash
sudo mount -o subvol=@ /dev/nvme0n1p2 /mnt/live_root
# 看 /mnt/live_root/etc/... 才是真配置
```

## 2026-04-23 验证成功状态

```
Session:    Wayland
主显示:     DP-1 (5090) → LG 32U990A 6K@60 原生
iGPU:       闲置, HDMI 拔了, 但 amdgpu 驱动仍在, 留作 fallback
NVIDIA:     mutter + CUDA + HDMI 音频同时用, 2-3GB VRAM 给 display 合成
```

## trade-off: 失去的回退能力

原 4 锁架构的目的是"GPU 挂了 GUI 不崩". 现在全拆, NVIDIA driver 挂了 → **GUI 直接黑**, 只能 TTY + rescue script. 代价在: 为 6K 显示放弃 iGPU fallback.

如果 GSP bug 再犯, tty2 跑 `/root/rescue-igpu.sh` 快速回原架构.

## 关联记忆

- `nvidia_gsp_bug.md` — 起因 (GPU hang 无 reset)
- `9950x3d_igpu_gaming_setup_2026-04-23.md` — DXVK 路由游戏到 iGPU (独立于显示)
- `nvidia_hdmi_audio_requires_display_2026-04-23.md` — NVIDIA HDMI audio 要求同口有 video 的怪癖
