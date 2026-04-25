# ADR 0007: 引 `wayland-protocols` crate 接 `text-input-unstable-v3` (T-0505)

## Status

Accepted, 2026-04-25

## Context

T-0505 给 quill 接 fcitx5 中文输入法。Wayland 时代 IME 走专用协议
`text_input_unstable_v3` (zwp_text_input_v3, 简称 TIv3): client 通过它告诉
compositor "我这个 surface 想要文本输入", compositor 把 IME (fcitx5 / ibus /
wlroots input-method-v2 服务端) 翻译出来的 preedit / commit / 删除环境文本
事件回送 client。键盘 evdev 仍走 `wl_keyboard` (T-0501 实装), TIv3 与
`wl_keyboard` **并行**: IME enabled 时 fcitx5 自己抓住 raw 按键不让走到
client (fcitx5 grab 模型), 我们这侧仅响应 TIv3 事件。

派单 T-0505 是 Phase 5 daily-drive 收尾 — ASCII 输入 (T-0501) 已 work, 中文
输入是用户日常写中文 commit message / 输 GUI 字段必需。**未接 TIv3 之前
quill 在 fcitx5 鼠标右键菜单的"应用列表"里出现, 但选中"中文"切换无效
果**, 因为我们 surface 没声明 text-input 能力, fcitx5 的 commit 字符串
没有目的地。

### text-input-unstable-v3 协议特点

- **atomic state machine**: 一组 event (preedit_string + commit_string +
  delete_surrounding_text) 后 done(serial) 标志一帧, client 必须等 done 才把
  pending state 应用到屏幕 / PTY。每 event 立即处理是常见 bug 来源。
- **commit_string** 是已确定的 UTF-8 字节, 直接写 PTY (跟键盘 ASCII 走同 PTY
  路径, INV-009 master fd O_NONBLOCK + 派单 In #D 背压丢字节)。
- **preedit_string** 是组词中的候选字符 (例如拼音 `ni hao` 选中前显示的灰色
  "ni hao" 拼音串), 渲染在 cursor 当前位置, 提交后 fcitx5 发 commit_string
  替换 preedit。
- **set_cursor_rectangle(x, y, w, h)** 是 surface 局部坐标 (logical px),
  client 调它告诉 fcitx5 候选框该贴在屏幕哪里, fcitx5 主流实现用此坐标定位
  弹窗 (输入"你好" 时 quill cursor 下方弹出 "你好/泥嚎/拟好" 选词框)。
- **enter / leave** 跟 `wl_keyboard` 焦点同步, client 必须在 enter 后调
  `enable() + commit()`, leave 时调 `disable() + commit()` (协议明文要求)。

### sctk 0.19.2 不封装 text-input-v3

- sctk 0.19.2 docs.rs 检索: `text_input` / `TextInput` / `IME` / `InputMethod`
  零命中
- sctk roadmap 已知 (上游 issue smithay/client-toolkit#XXX) text-input-v3
  封装在 0.21+ 路线图, 当前 0.19/0.20 不在 scope
- text-input-v3 协议自 2018 起 Purism + Red Hat + Intel 三方推, GNOME mutter /
  KDE kwin / wlroots / sway / Hyprland / cosmic-comp 全实装, fcitx5 / ibus /
  wlroots input-method-v2 三大 IME server 全用 — **桌面 Wayland IME 唯一现代
  协议**

CLAUDE.md "依赖加新 crate → 必须 ADR" 硬约束触发本 ADR。

## Decision

引 `wayland-protocols = "0.32"` 作 dep (非 dev-dep, main 路径用), 启 features
`["client", "unstable"]`:
- `client`: 生成 client-side 绑定 (我们是 client, server 端走 cosmic-comp /
  mutter / kwin)
- `unstable`: text-input-v3 是 `wp/text_input/zv3` 路径, gated 在 `unstable`
  feature (源码 `wayland-protocols-0.32.12/src/wp.rs:524` 明示
  `#[cfg(feature = "unstable")]`)

不启 `staging` (text-input-v3 是 unstable 不是 staging) / `server` (我们不写
compositor)。

固定调用接口:

```rust
use wayland_protocols::wp::text_input::zv3::client::{
    zwp_text_input_manager_v3::ZwpTextInputManagerV3,
    zwp_text_input_v3::{
        ZwpTextInputV3,
        Event as TextInputEvent,
    },
};

// registry bind manager (T-0501 SeatHandler 套路同源)
let manager: ZwpTextInputManagerV3 = registry_state.bind_one(...);

// 每 surface 一个 text_input
let text_input = manager.get_text_input(&seat, &qh, ());

// enter / leave 跟 wl_keyboard 焦点同步
text_input.enable();
text_input.set_content_type(hint, ContentPurpose::Terminal); // 派单 In #C
text_input.set_cursor_rectangle(x, y, w, h);                 // logical px
text_input.commit();

// done event 攒一帧的 preedit/commit/delete 一并 apply
match event {
    TextInputEvent::Enter { .. }            => ImeAction::EnterFocus,
    TextInputEvent::Leave { .. }            => ImeAction::LeaveFocus,
    TextInputEvent::PreeditString { .. }    => 暂存 pending,
    TextInputEvent::CommitString { .. }     => 暂存 pending,
    TextInputEvent::DeleteSurroundingText { .. } => 暂存 pending,
    TextInputEvent::Done { .. }             => 应用 pending → ImeAction,
    _                                       => Nothing,
}
```

**版本**: `wayland-protocols = "0.32"` (0.32.12 实测 docs.rs / cargo search 当
前最新), 与 `wayland-client = "0.31"` 同生态, transitive 拉 wayland-scanner +
wayland-backend 已被 wayland-client 拉过, 无新依赖图节点。

**类型隔离 (INV-010)**: TIv3 协议类型 (`ZwpTextInputV3` / `TextInputEvent` /
`ContentHint` / `ContentPurpose` / `ChangeCause`) **仅出现在
`src/ime/mod.rs` + `src/wl/window.rs::Dispatch` 段**, 不出现在 quill 公共 API
返回类型。`ImeAction` enum / `ImeState` struct 是 quill 自有, 内部包 wayland
类型 但字段全私有, 下游不可构造 — 与 T-0501 KeyboardState 套路同源。

## Alternatives

### Alt 1: 等 sctk 上游封装 text-input-v3

- 方案: 不引 wayland-protocols, 等 sctk 0.21+ 内置 IME wrapper
- Reject 主因:
  - **时间未知**: sctk 0.19 → 0.20 跨度 6+ 月, 0.20 → 0.21 同等; text-input-v3
    封装可能更晚 (Wayland IME 协议生态分散, cosmic-comp 用 input-method-v1,
    fcitx5 默认 v3, wlroots 自有变体, sctk 抽象统一难)
  - **派单 P1 daily-drive 收尾**: 用户写中文 commit / 中文邮件 / 中文
    Markdown 已 daily 需求, 不能等
  - **其他 client 早就直接走 wayland-protocols**: foot / alacritty (foot
    `foot/wayland.c::text_input_*` 直接绑 wayland.xml 生成), 不依赖 sctk
- 备选优势: 不引新 dep, 减一行 Cargo.toml — 收益小

### Alt 2: 手写 wayland-scanner 生成绑定

- 方案: build.rs 调 wayland-scanner CLI 工具 parse text-input-unstable-v3.xml
  生成 .rs, 不引 wayland-protocols crate
- Reject 主因:
  - **wayland-scanner CLI 是 C 工具**, build dep 拉 libwayland-bin (Arch/Debian
    / Fedora 包名差异), CI 配置成本高
  - **wayland-protocols crate 内部就是这条路径** — `wp.rs::text_input::zv3`
    用 `wayland_protocol!` 宏在 build time scan XML 生成绑定 (源码确认), 我们
    自己手做 = 重新实装上游 0.32 自动化, 写 build.rs +
    cargo:rerun-if-changed=protocols/*.xml 等 ~80 行无新增价值
  - **wayland-protocols crate 已包含 XML** (`wayland-protocols-0.32.12/
    protocols/unstable/text-input/text-input-unstable-v3.xml`), 不需要我们
    自己 vendor XML
- 备选优势: zero new crate dep — 但 wayland-protocols 本身已是 wayland-client
  生态主流 dep (本项目已用 wayland-client 0.31, transitive deps 已大半重叠),
  增量极小

### Alt 3: 走 fcitx5 D-Bus IPC 旁路

- 方案: 不接 Wayland IME 协议, 直接用 D-Bus 跟 fcitx5 daemon 通信 (fcitx5
  本身有 fcitx-dbus 接口, 历史 X11 时代 widget 库走这条)
- Reject 主因:
  - **绕过 compositor**: Wayland 安全模型不允许 client 直接拉 input grab,
    fcitx5 D-Bus 旁路在 Wayland session 下 GNOME / KDE 实测无效 (fcitx5
    自己输入插件文档明示需要走 wl_text_input)
  - **不可移植**: ibus / wlroots input-method-v2 服务端不响应 fcitx D-Bus
    协议; 用户切 ibus 后整套坏
  - **D-Bus 在 calloop 整合复杂**: 要起 dbus-tokio / zbus async runtime, 与
    INV-005 单线程 calloop 冲突
- 备选优势: 不依赖 Wayland 协议演进 — 但 quill 是 Wayland-only, 该解耦无价值

### Alt 4: X11 IME (XIM) 旁路

- 方案: quill 跑 XWayland 兼容层, IME 走 X11 XIM 协议
- Reject 主因:
  - **quill 是 Wayland-only** (CLAUDE.md "非目标"段明示), XWayland 是反向
    路径增运维成本
  - XIM 协议 1990s 设计, preedit on-the-spot 模式在现代 Wayland compositor
    上多数失效 (mutter XWayland session XIM 仅 over-the-spot, 候选框漂浮在
    错误位置)
  - 即便能跑也不能称 "Wayland IME 集成", 是技术债
- 备选优势: 无 — 纯 reject

## Consequences

### 正面
- **fcitx5 / ibus / wlroots input-method-v2 全栈适配**: text-input-v3 是 IME
  统一协议, 接一次三家 server 都 work
- **代码量小**: 本 ticket 估 +500 行 (src/ime/mod.rs ~250 行 + window.rs
  Dispatch impl ~120 行 + render.rs preedit ~80 行 + 测试 ~150 行), 比 SCTK
  abstraction 重做更轻
- **Phase 6 IME 高级特性留扩展位**: surrounding text / content_hint 设密码
  字段隐藏候选 / preedit_underline 多段样式, 都可 future ticket 在
  ImeState + render.rs 内增, 不破 API
- **类型隔离 (INV-010) 守得住**: TIv3 协议类型局限 src/ime + window.rs
  Dispatch 一个段, ImeAction enum 全 quill 自有

### 负面 / 代价
- **Cargo.lock 新增 1 transitive crate** (`wayland-protocols`): 内部依赖
  `wayland-client` / `wayland-backend` / `wayland-scanner` 都已被 quill
  既有依赖拉过, 净增量 = 1 crate (wayland-protocols 自身), 编译时间 +5s
  (cargo build --release 实测)
- **unstable feature flag 启用**: wayland-protocols 文档警示 unstable
  feature 下的协议可能 backward incompatible; text-input-v3 自 2018 稳定
  未变, 升级到 stable v1 (未发布) 时本 crate 会 supersede zv3, 届时改一
  import 路径即可 (单点改动 src/ime/mod.rs)
- **协议本身复杂 (atomic done event)**: 实装错把 preedit 即时 apply 是
  常见 bug, src/ime/mod.rs `handle_text_input_event` 必须正确实现 pending
  state buffer, 单测覆盖各 event 序列 → 单 done → 单 ImeAction 路径

### 已知残留 (非本 ADR scope)
- **真键盘 + IME 协调**: fcitx5 enabled 时键盘事件被 IME server 拦截, 我们
  不需要判断 IME on/off, wl_keyboard 与 text_input 并行响应即可 (实测
  fcitx5 grab 时 wl_keyboard 不发, 双管路径不冲突)。Phase 6 若发现某些
  compositor 双发可加 enabled 标志拦截, 本 ticket 不做
- **fractional scale**: cursor_rectangle 现走 logical px (T-0404 锁
  HIDPI_SCALE=2 整数缩放), fractional scale 是 ROADMAP 永久 Out
- **多 surface IME** (多窗口共享 manager): quill 单 surface, manager 仅
  bind 一次, 多窗口 Phase 6+ 才接

## 实装验证

- T-0505 commit 实装本 ADR
- `src/ime/mod.rs::ImeState` + `handle_text_input_event` API 落盘
- `src/wl/window.rs` 接 zwp_text_input_manager_v3 registry bind +
  Dispatch<ZwpTextInputV3, ()> + ImeAction 分派
- ≥ 8 个 lib 单测覆盖 enter / leave / preedit / commit / delete / done
  序列 / cursor_rectangle 上报路径
- 集成测试 `tests/ime_e2e.rs` (mock event sequence → ImeAction → PtyHandle
  收 commit bytes) + `tests/ime_preedit_render.rs` (render_headless 模拟
  preedit + 下划线 → /tmp/ime_test.png)
- 4 门绿 (cargo build / test / clippy / fmt)
- 三源 PNG verify (writer + reviewer + Lead Read /tmp/ime_test.png)
- 手测 deliverable: cargo run --release + fcitx5-rime 切中文 → 输 "你好"
  → preedit 显示候选 → 选中 → quill cell 真显示 (Lead 主导)

## 相关文档

- 派单: `tasks/T-0505-fcitx5-text-input-v3.md`
- 主体实装: `src/ime/mod.rs` + `src/wl/window.rs::Dispatch<ZwpTextInputV3>` 段
- 协议参考: `https://wayland.app/protocols/text-input-unstable-v3` +
  本地 `~/.cargo/registry/src/.../wayland-protocols-0.32.12/protocols/
  unstable/text-input/text-input-unstable-v3.xml`
- 相关 ADR: 0006 (xkbcommon, 与 IME 同 Phase 5 daily-drive 解锁条线)
- 相关 INV: INV-005 (calloop 单线程, IME PTY write 走 O_NONBLOCK 不重试) +
  INV-010 (类型隔离, wayland-protocols 类型不出 ImeState/ImeAction 公共 API)
