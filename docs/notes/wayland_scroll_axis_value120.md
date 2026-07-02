<!-- migrated from claude memory: wayland_axis_value120_over_smooth_axis_2026-04-26.md on 2026-05-11 -->
---
name: Wayland 滚轮走 AxisValue120 不走 smooth Axis (mutter 协议陷阱)
description: wl_pointer Axis smooth value 跟物理速度变 (mutter 慢 8px / 快 30px), 阈值化致"滚一下随机出 0-2 行". 改走 wl_pointer.AxisValue120 (Wayland 1.21+ 离散 notch 协议, ±120/notch) 直接乘 lines-per-notch (3 主流), 跟速度无关. quill T-0602 → T-0618 真因
type: feedback
originSessionId: 8250e730-19a5-479b-86d8-29f26fec010b
---
**陷阱**: Wayland client 处理鼠标滚轮如果只接 `wl_pointer::Event::Axis`
(smooth pixel value), 体感会**跟物理转速变化**:
- 慢转: value 小 (mutter 给 8 px), 累积器 / 阈值化致**不动**或随机
- 中转: value 中 (15-20 px), 部分跨阈值出 1 line
- 快转: value 大 (30+ px), 1:1 出 line

阈值化设计 (例 24 px / line) 在 mutter / sway / KWin 上都不稳, 因为
**compositor 给 Axis 的 value 跟时间梯度有关**, 同一物理 notch 不同速度
出不同数字. 用户体感"自适应随机".

**正解**: 走 **`wl_pointer::Event::AxisValue120`** (Wayland 1.21+ 加,
mutter ≥ 45 / sway ≥ 1.8 / kwin ≥ 5.27 全支持):
- value120 = ±120 / 物理 wheel notch (协议规约死)
- 直接 lines = (value120 / 120) × WHEEL_LINES_PER_NOTCH (主流 3)
- **跟物理速度无关**, 一格 notch 永远出 3 line, 体感稳

**实证 (quill T-0618 修 T-0602, 2026-04-26)**:

T-0602 写法 (错):
```rust
state.scroll_accum_y += value;  // smooth Axis value
let lines = (state.scroll_accum_y / SCROLL_ACCUM_LINE_PX).trunc() as i32;
// 24 px 阈值出 1 line, mutter 慢转给 8 px 永远不出, 快转 30+ 才出 1.
```

T-0618 写法 (对):
```rust
wl_pointer::Event::AxisValue120 { axis, value120 } => {
    let notches = value120 / 120;  // discrete 计数, 跟速度无关
    let lines = notches * 3;       // 3 lines per notch (alacritty/foot/gnome 默认)
    PointerAction::Scroll(-lines)  // wl + value = 向下手势 → quill 看新内容
}
// smooth Axis 路径 ignore (touchpad 复活时再开)
```

**How to apply**:
- Wayland client 接鼠标滚轮: **优先 AxisValue120, 不接 Axis**
- 触摸板需要支持时, 走 `wl_pointer.Event::AxisSource` 区分: Wheel → AxisValue120 路径, Finger/Continuous → smooth Axis 累积 (按 cell_h px 阈值)
- WHEEL_LINES_PER_NOTCH = 3 (alacritty/foot/gnome 默认), xterm 用 5; 给用户 config

**别的终端怎么干**:
- alacritty 0.13+: 同时接 Axis + AxisValue120, AxisValue120 优先 (在 Frame 内)
- foot 1.16+: 仅 AxisValue120 路径 (触摸板单独 Axis 路径)
- ghostty: 同 alacritty
- xterm: 老 X11 protocol, 不适用

**跨平台对照**: X11 用 Button 4/5 (向上/下) + 6/7 (向左/右) discrete events,
天然离散. macOS NSEvent.scrollingDeltaY 是连续 px (类似 Wayland Axis), 苹果
不暴露 discrete notch. Windows WM_MOUSEWHEEL 用 wParam 高 16-bit = WHEEL_DELTA
× 120 (天生 ±120 / notch, Wayland AxisValue120 设计就是抄它).

**why mutter 给 Axis 跟速度变**: libinput 加 acceleration profile 让 smooth
Axis 数值"更自然", 但破坏 client 端阈值化的可预测性. AxisValue120 是后来
加的"escape hatch" 给 client 拿真离散计数. 设计上是修历史 bug.

跨项目复用: 任何 Wayland client (终端 / 编辑器 / 浏览器替代品 / GUI app)
处理鼠标滚轮, 第一选择是 AxisValue120, 别用 smooth Axis 阈值化. 可以省
"用户报滚轮体感差" 的反复迭代.
