<!-- migrated from claude memory: wayland_proxy_drop_destroys_resource_2026-04-26.md on 2026-05-11 -->
---
name: wayland-client Proxy drop 即触发 destroy request
description: wayland-client 0.31 Proxy::drop 自动发送 destroy request, source/offer 类对象需 store 防 drop. 否则 set_selection / receive 后 source 立刻被 compositor cancel, 数据 lost. T-0607 复制粘贴粘出空格的真因
type: feedback
originSessionId: 8250e730-19a5-479b-86d8-29f26fec010b
---
**陷阱**: wayland-client 0.31 (Rust binding) Proxy::drop 默认会发送
对应协议的 destroy request. wp_primary_selection_source / wl_data_source /
wl_data_offer 等 owner 类对象, 如果创建后不 store 在 state 里, 离开
fn 作用域立刻 drop → destroy 发出去 → compositor cancel selection →
数据丢失.

**Why**: wayland 协议有"destroy"语义的对象 (源对象 / offer), Rust binding
通过 Drop trait 自动调 destroy. 跟 GC 语言 (C/C++ libwayland 手动管理)
不同 — Rust ownership 让"忘记 store"=立刻 destroy. 写代码时容易把
source 当一次性变量 (创建 → set_selection → 完事), 实际上 set_selection
之后 compositor 还要 lazy fetch 数据 (Send event), 此时 source 已 drop
就拿不到了.

**实证 (quill T-0607 hotfix v1, 2026-04-26)**:
```rust
// 错: source 是局部变量, fn 返回时 drop → compositor cancel
let source = manager.create_source(qh, ());
source.offer(mime);
device.set_selection(Some(&source), serial);
// fn 返回 → source drop → destroy → cancel → 别人 paste 拿不到内容

// 对: store 防 drop
let source = manager.create_source(qh, ());
source.offer(mime);
device.set_selection(Some(&source), serial);
state.active_source = Some(source);  // 关键
```

PRIMARY 中键粘贴能用是因为 mutter 收 set_selection 后**立刻**通过 Send
event 拉数据 cache (源端 Send 同步处理), 之后 source drop 无所谓; CLIPBOARD
mutter **lazy fetch** (粘贴方真要时才 send), source drop 后拿不到 → 粘贴方
收空 → 用户看到粘出空格 (bracketed wrap 后是 `\x1b[200~\x1b[201~` readline
处理为 0 字节).

**How to apply**:
- 任何 wayland source / offer 类对象创建后, **必须 store 在 state 字段里**
- 释放路径: cancelled event handler 内 `state.active_source = None;` 显式 take
- 同款字段隔离: PRIMARY / CLIPBOARD 各自 active_primary_source / active_data_source,
  跨 source race 防 (T-0607 hotfix v2 修了)
- 测试: 单元测试无法验 (Proxy drop 行为依赖 wayland-client + compositor),
  必须真 wayland session + wl-paste 验

跨平台: C 代码用 libwayland 手动管理生命周期, 不会撞这个; Rust binding
都有这个陷阱 (wayland-client / smithay-client-toolkit). 同样适用 Smithay
server 侧 (server proxy 也有 destroy 语义).

跨项目复用: 任何 Rust + wayland 项目接入 selection / clipboard / DnD
功能, 第一个 design rule 是 "owner proxy 必 store". 自写 wayland client
都要踩.
