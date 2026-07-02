<!-- migrated from claude memory: wayland_dnd_set_actions_required_2026-04-26.md on 2026-05-11 -->
---
name: Wayland DnD set_actions 必调 (mutter 默认拒绝 drop)
description: wl_data_device DnD 协议陷阱 — Enter handler 调了 accept(serial, mime) 还不够, 必须紧跟调 offer.set_actions(Copy, Copy) 否则 mutter 默认 client 拒绝 drop, 用户拖入释放鼠标 cursor 显示禁止图标 + 直接发 Leave event 不发 Drop event. 拖文件功能挂在这里
type: feedback
originSessionId: 8250e730-19a5-479b-86d8-29f26fec010b
---
**陷阱**: wayland-client `wl_data_device::Event::Enter` handler 内只调
`offer.accept(serial, Some(mime))` **不够**. 必须紧跟调
`offer.set_actions(DndAction::Copy, DndAction::Copy)` 显式声明可接受的
drag action (copy/move/ask).

**Why**: wl_data_device v3+ 协议 (mutter 实装) 默认 client 不声明 actions
就当 client 拒绝 drop. cursor 显示"禁止"图标, 用户释放鼠标 → compositor
直接发 Leave event 跳过 Drop event. client 永远收不到 drop 通知, 拖文件
功能完全挂. accept(serial, mime) 只是说"我接受这个 mime", set_actions
是说"我接受这种 drag 行为", 两者缺一不可。

**实证 (quill T-0611 hotfix v1, 2026-04-26)**:
RUST_LOG=quill=debug log 显示
```
DnD enter accepted=Some("text/uri-list")  ← 协议层 accept OK
DnD leave                                   ← 12 微秒后 Leave
```
中间无 Drop event, 用户拖文件释放完全无响应. 加 `set_actions(Copy, Copy)`
后立刻
```
DnD enter ... set_actions=Copy
DnD drop event (pending_drop set)
```

**How to apply**:
- Wayland 拖文件 / 拖文本 client 实装时, Enter handler 模板必带:
  ```rust
  if mime_accepted.is_some() {
      offer.accept(serial, mime_accepted);
      offer.set_actions(DndAction::Copy, DndAction::Copy);
  }
  ```
- 失败症状定位: 用户拖文件入窗口, cursor 显示 ⊘ 禁止图标 (而非 + 复制),
  释放鼠标无任何响应 → 99% 是 set_actions 漏调
- DndAction enum 来自 `wayland_client::protocol::wl_data_device_manager::DndAction`,
  copy/move/ask 三选一. 终端 / 文本编辑 一律选 Copy (拖入是复制不是
  移动文件)
- sctk 0.19.2 / wayland-client 0.31 不自动调 set_actions, 必须 client 显式

跨平台对照: GTK4 / Qt 框架自动调 set_actions, 用框架不会撞这个 bug. 自管
wayland 协议的 client (alacritty / foot / quill / 自写终端) 必须显式调,
ghostty 源码 termio/posix.zig set_actions 也是显式。

跨项目复用: 任何走 sctk + wl_data_device 的 Rust client (终端 / IDE /
浏览器替代品), DnD 实装第一个写的就是 set_actions, 防 1 小时 debug。
