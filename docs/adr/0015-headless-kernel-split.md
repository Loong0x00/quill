# ADR-0015: 无头内核 + 渲染客户端拆分(daemon/client)

状态: 提议中(Phase 7,2026-06-29)
关联: 全局记忆 `project_home_wireguard_vpn_over_v6`(手机经 WireGuard IPv6 直连家里这台)

## Context

需求:在手机上盯/控正在跑的工作区(多 PTY/tab,主力是 Claude Code 会话),且要"开箱即用"。
tmux 能做但繁琐(prefix 键 / attach 仪式 / 嵌套 alt-screen 重绘发癫),且 CC 跑 inline(`tui: default`)
时输出是**可 reflow 的流**。目标:把"持久 + 共享"烤进 quill —— 无头会话内核(daemon)持有所有
PTY + VT + 屏幕模型 + tab 工作区,渲染端(桌面 `wl/` / 手机 web)都是它的客户端。

**不是回归 tmux**(那是 tty 单 stream 时代的产物,见 CLAUDE.md 非目标),是 quill 原生的会话内核 ——
桌面 + 手机镜像同一工作区,零仪式。

## 审计结论(2026-06-29,5-reader workflow + 读码核实)

**拆分难度【中】**:
- `tab/ term/ pty/ composer/` 数据类型**零 GUI 耦合**(term 包 `alacritty_terminal`,本就无头)。
- 但**无现成 `Session` 抽象**:tabs 塞在 `wl::State`(240 字段),整合点是 **calloop**(PTY fd 与
  Wayland fd 注册到同一个 `calloop::EventLoop`)。是手术,不是搬模块。

**关键约束**:
- ⚠️ **`Rc<RefCell<String>>` 单线程共享**(`TabInstance.title` / `TermState.title` / `TermListener`,
  `term/mod.rs:512/570`)→ `TermState`/`TabInstance` **非 `Send`**。不挡单线程 daemon,但**挡跨线程
  WS fan-out**:快照必须先序列化成 owned 纯结构才能过线程边界。**最硬约束。**
- `CellRef` 含借用/字体引用**不能直接 serde** → 需新写 `CellWire`(owned)。Phase 1 主要新代码量。
- `pty_read_tick`(`wl/window.rs:488`)掺了 `selection_state` 的 scroll-rebase → selection 要拆出去。
- ✅ **现成便宜**:`render_headless`(`wl/render.rs:4310`)+ `run_headless_screenshot`(`main.rs:125`)
  已是纯数据无头渲染路径 → 快照协议 ≈ 照其入参抄。

## Decision

- **无头核(daemon)** = `tab/ + term/ + pty/ + composer/` 原样 + 新 `Session{tabs}` + 自建 calloop
  (只注册 PTY fd + socket,无 Wayland)。
- **渲染客户端** = `wl/`(Wayland dispatch / `Renderer` / 键鼠 / IME / clipboard / DnD)+ selection 归客户端。
- **边界 = `State.tabs`**:`State` 不再 own `TabList`,改持 socket + 本地镜像缓存。
- **fan-out 架构**:calloop 线程算快照 → `mpsc` → tokio + tungstenite IO 线程广播 owned 序列化字节
  (绕开 `Rc` 非 Send)。

## Phase 1(daemon 多-PTY 工作区 + 持久化 + WS fan-out + 连接发快照)

1. 新 bin `quill-kernel`(`src/bin/kernel.rs` + `src/kernel/`),依赖 lib 的 tab/term/pty/composer。
2. `kernel/proto.rs`:`Snapshot{tab_id,cols,rows,cells:Vec<CellWire>,row_texts,cursor,title}`(字段照
   `render_headless`);`CellWire`(owned);`ClientMsg::{Input,Resize,TabOp}`。先 JSON 后 bincode。
3. `kernel/session.rs`:`Session{tabs:TabList}` + `on_pty_output/on_input/apply_tab_op`(从 `window.rs`
   搬,去掉 selection + Wayland)。
4. daemon calloop:注册各 PTY master fd + `UnixListener`。
5. fan-out:calloop → `mpsc` → IO 线程(tungstenite)广播。
6. 连接发全量快照(`cells_iter().collect()` + `line_text()`),dirty 帧 `term.is_dirty()` 触发。
7. 持久化只存**工作区意图**(tab 数/标题/cwd/active → `~/.local/state/quill/workspace.json`);
   PTY 子进程不能跨重启迁移,真 scrollback 持久化 Phase 1 不做。
8. 客户端改造后置(先用 main.rs 式 dump 客户端验)。

## 风险

- `Rc` 非 Send(头号):跨线程只传序列化字节。
- `CellRef`→`CellWire` 是真新代码,非"零改动"。
- PTY 不能跨 daemon 重启 → 只持久化意图。
- 快照带宽:全量 cell JSON 大 → 尽早 dirty-row 增量 + bincode。
- 多客户端尺寸冲突:一个 PTY 一个尺寸 → "主控端定尺寸,其余缩放"。
- calloop 单线程吞吐 → IO 拆线程缓解。

## ⚠️ 待 impl 时核实(审计未逐行读 —— 凡改 render 的 ticket 必做)

- `Renderer::draw_frame` 对 wgpu Surface 的耦合深度只读了签名/调用点,未逐行。
- 240-字段 `State` 的 drop 序契约(INV-001/008)在拆 `tabs` 出去后是否成立,需实际编译验证。
- **凡改 render 的 ticket,impl / 审码 agent 必须自己读 `wl/render.rs` 与 `wl/window.rs` 的 draw
  路径核实,别信本 ADR 摘要。**
