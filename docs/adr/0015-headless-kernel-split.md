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

---

## 修订 R1(2026-06-29,与用户对齐后)—— 取代上面 Decision/Phase1 的部分内容

经讨论,原 Decision 里"每帧广播序列化网格"被证明是过度设计。修订如下(T1/T2/T3a 的 **daemon + calloop + WS 传输 + Send 安全** 那条 spine 全部留用,变的只是**载荷**与**生命周期模型**)。

### 定位:真终端 ≠ ssh
会话本身是个**常驻一等实体**(活在 daemon),设备只是它的窗口;不是"远程登录一台机器"(ssh = 连接级、断即亡、要 tmux 才持久、尺寸焊死)。但**也不是 tty**(tty 只在关机/登出销毁)——见下面的引用计数生命周期。

### 载荷:传字节流,客户端本地渲染(不传网格)
- **daemon = PTY 多路复用器**:转发各 PTY 的**原始字节流**给客户端;**客户端各自是完整终端模拟器**(Linux 原生 quill / 浏览器 xterm.js),**本地渲染 + 本地按自己宽度 reflow**。
- **为何不传网格**:网格/原始字节都已按某宽度折死;要多端不同宽各自 reflow,必须传**没焊死宽度的原料**(逻辑行)。原始字节流(对非自折行的流式程序)本就含逻辑行(硬 `\n`),客户端留着重折即可。
- **网格快照(T1 `Snapshot`)降级为"连上时的关键帧"**:新客户端连上先收一帧全量当前屏做引导,之后跟字节流(I 帧 + 后续流)。**不再每 dirty 帧序列化整张网格**(T3b 的"网格直播 + latest-wins 背压"因此**作废/搁置**,见 feat/kernel-ws-live 分支不合并)。
- 用户基本不用全屏 TUI(有问题问 AI),故"一个 PTY 一个尺寸、全屏 TUI 跨宽伺候不了"这条限制基本不触发;真碰到(自折行程序)时接受其在非主宽度下不完美。

### 生命周期:连接的显示端引用计数,**无租约**
- **共享单元 = 整个 quill 工作区(所有 tab/PTY)**,不是单个窗口。手机连上**默认同步桌面 quill 当前持有的【全部】工作区**。
- **holder = 当前连着的显示端**(桌面 quill 窗口 / 连着的手机)。**桌面 quill = 稳定的锚**(用户从不 `exit`,只 X 关 → 窗口一直持有到主动 X)。
- **X(桌面或手机)= 显式释放该 holder;holder 归 0 才销毁工作区**(连同其 PTY/子进程)。`>1` 时 X 一个只关那个 view,会话给其余 holder 留着。
- **断线(后台/锁屏/网络抖/WS 断)= 非事件**:不释放、不销毁——因为桌面锚还在。手机**不拥有任何东西、只是镜子**,故**不需要租约/grace/TTL**;重连只做一件事=再同步一次全量。
- **⭐ PTY ⟺ 桌面窗口【原子耦合】(治"反复找脆弱态")**:会话是反馈环,一次 spawn 里 `PTY(源)→ {quill 桌面窗口, 手机}` 一起出生(PTY 是电脑进程、由 quill 管,一存在桌面就有窗口)。故**"手机持有、桌面看不到"构造不出来** —— 不是事后"自动补窗救援",是耦合让脆弱态不存在("补窗"是 spawn 的固有部分,非独立兜底)。⟹ **桌面窗口靠构造永远是锚、手机永不会是唯一 holder** = "无租约"成立 + 手机后台杀不掉会话的真正原因。
- **不落盘持久化**(取代 Phase1 §7):工作区是临时的、内存里、引用计数;主机重启全死 = 等于"关机销毁",合理。`workspace.json` 不做。

### 客户端矩阵
- **Linux** = 原生 quill 显示端;**手机 / Mac / 借来的机器** = **web 客户端(浏览器,通用)**。**不为跨平台移植原生 quill**(Mac 上本地用 ghostty,要连家里走 web)。
- "任意位置" + 安全 = 已有的 WireGuard VPN 把门(以后可加 token 当登录)。

### 重排后的 ticket(取代 Phase1 §5-8)
1. **(已完成)** T1 协议 `441168f` / T2 单线程 daemon+unix socket `14e2a33` / T3a 带线程 WS 传输 + 连上发关键帧 `7e84b18`。**这些 = spine,留用。**
2. **T3c'(下一砖)**:daemon 转发 PTY **字节流** + 客户端本地模拟器渲染(取代 T3b 网格直播)。连上=关键帧(当前屏)+ 之后字节流。
3. **T4**:web 客户端用 xterm.js 本地渲染 + 本地 reflow;同口 serve 页面(T3b 的"同口 HTTP"那点可摘出来留用)。
4. **T5**:输入回灌(键盘 → daemon → PTY,`calloop::channel` 反向唤醒)。
5. **T6**:引用计数生命周期(holder=连接显示端,X 释放,断线非事件,归 0 销毁)+ 桌面自动补窗当锚 + 手机同步全部工作区。
6. **T7**:web 端"X 关闭"语义、连接状态/重连、PWA 壳缓存;(可选)登录 token。
