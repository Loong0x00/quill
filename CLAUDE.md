# quill

极简 Wayland 终端模拟器。Rust。个人 daily driver。

**名字由来**:quill = 羽毛笔,写字工具。隐喻:终端 = 文字流的载体。

---

## 为什么存在

现有 Linux 终端在 6K + NVIDIA + Wayland 组合下都有硬伤:
- **Ghostty**: GTK4 event starvation hang + PageList 内存泄露 3 年没修
- **Foot**: 作为保底没问题,但没法加用户想要的新功能
- **Alacritty**: 稳定但极简,想加 feature 得 fork
- **Kitty**: 作者哲学激进,上游扯皮多

所以自己写一个:针对单用户(Lead = user + Claude Code 主 session)的工作流,不追求通用。

---

## 目标(Phase 1-6 必做)

- [ ] 跑 Claude Code 长时间无 memory leak(soak 1h RSS 稳定)
- [ ] 2x HiDPI 整数缩放 + CJK 字形正常
- [ ] fcitx5 输入法(Wayland `text-input-v3` 协议)
- [ ] 事件循环零 starvation(单线程 `ppoll` 绑所有 fd)
- [ ] ~30K LOC 左右,架构读得懂
- [ ] inline composer + popup 候选不依赖 readline

## 非目标(暂不做,不纠结)

- 分屏 (Phase 后期再考虑, 不在 daily drive 路径)
- 滚动搜索 (Phase 后期)
- ligature(可以以后加)
- Windows / macOS(Wayland-only)
- 任何"省显存/省 CPU"的优化技巧 —— 先正确,再性能

**注**: 早期写过"多标签交给 tmux"被 user 否决 (2026-04-26). tmux 是
tty 时代单 stream 限制的产物, Wayland 窗口能开无数, 在 GUI 终端再叠虚
拟终端复用器是叠床架屋. 多 tab 走 ghostty 风 native 实现, 见 T-0608。

---

## Phase 7(进行中,2026-06-29):无头内核 + 多端镜像

**目标**:把 quill 拆成「无头会话内核(daemon)+ 渲染客户端」,使**同一工作区(多 PTY/tab)能镜像到手机**,在外面盯/控 Claude Code 会话;**手机端自适应渲染**(流式内容按手机宽 reflow)。

**不是回归 tmux** —— 恰恰要消灭 tmux 的繁琐(prefix/attach 仪式/嵌套 alt-screen 发癫)。把"持久 + 共享"烤进 quill 本体,桌面/手机都是同一内核的渲染端,零仪式。动机:user 已有移动 IPv6 + WireGuard VPN(全局记忆 `project_home_wireguard_vpn_over_v6`),手机能直连家里这台。

**已知架构事实(审计 2026-06-29,完整见 ADR-0015):**
- 拆分难度【中】:`tab/term/pty/composer` 数据类型零 GUI 耦合(term 包 alacritty,本就无头);但无现成 `Session` 抽象,tabs 塞在 `wl::State`(240 字段),整合点是 calloop。
- ⚠️ **`Rc<RefCell<String>>` 单线程设计**(`term/mod.rs:512/570`)→ `TermState`/`TabInstance` **非 `Send`**。不挡单线程 daemon,但**挡 WS fan-out**:快照必先 `.clone()` 成 owned 纯结构才能跨线程发。最硬约束。
- `CellRef` 含借用/字体引用**不能直接 serde** → 需 `CellWire`(owned)。Phase 1 主要新代码量。
- ✅ 便宜:`render_headless`(`wl/render.rs`)+ `run_headless_screenshot`(`main.rs`)已是纯数据无头渲染路径 → 快照协议 ≈ 照其入参抄。
- 边界画在 `State.tabs`;selection 归客户端(现掺在 `pty_read_tick`)。
- ⚠️ **render 侧耦合(`Renderer::draw_frame` wgpu)审计没逐行读** → 改 render 的 ticket,impl/审码 agent **必须自己读 `wl/render.rs` / `wl/window.rs` draw 路径**,别信摘要。

**进展(2026-06-29):**
- ✅ **T1** `441168f`:`src/kernel/{mod,proto,session}.rs` —— `CellWire`/`Snapshot`/`ServerMsg`/`ClientMsg` + `Session{tabs}` 骨架 + roundtrip 测试。CellWire 镜像 CellRef 无损(fg 像素测试盲区由 serde/From 单测兜,等 T-0405 接 per-cell fg 自动覆盖)。
- ✅ **dirty 收口** `6db5945`:`TermState` 为 dirty 唯一真相源。
- ✅ **T2** `14e2a33`:`src/kernel/daemon.rs` + bin `quill-kernel`(daemon)+ bin `quill-dump`(客户端)+ `tests/kernel_daemon_slice.rs`。**单线程 calloop** daemon:spawn 一 shell tab,PTY fd + UnixListener 同一 calloop,客户端连上发当前 Snapshot JSON。spine 通:**driven tab → Snapshot → unix socket → client**。故意单线程绕开 Rc 非 Send。
- ✅ **T3a** `7e84b18`(+ ADR `ef28ff2`):带线程 WS 传输 —— `tungstenite 0.29`(同步,无 tokio/TLS)WS 线程 bind `0.0.0.0:7878`;calloop 线程 `serde_json::to_vec` 序列化 Snapshot → `mpsc::Sender<Vec<u8>>` → WS 线程 → 浏览器(`assets/web/index.html`)。**只有 owned `Vec<u8>` 过线程边界**(Lead 读码核实,Rc 非 Send 守住);手机经 VPN→路由器→`10.0.0.2:7878` 可达,VPN 把门。详见 ADR-0016。
  - ⚠️ 遗留(非阻断):① WS 每连接无上限起线程;② daemon 只 serve WS、不 serve `index.html`(其中"同口 HTTP serve 页面"那点可摘出来留用)。
- ⏸️ **T3b**(搁置,分支 `feat/kernel-ws-live` 不合并):做了"网格直播 + 同口 HTTP",对抗审查逮到背压 **drop-newest 阻断 bug**(慢客户端卡陈帧);修复时 agent 遇 API 529 中断成半截。**更关键:架构 R1 修订后"每帧广播网格"整体作废**(改传字节流本地渲染),故 T3b 搁置。
- ✅ **片1(T3c'+T4)** `e04f909`+`2ea1a68`:WS 载荷从网格 **pivot 成 PTY 字节流** + 浏览器 **xterm.js 本地渲染 + 本地 reflow** + **同口 HTTP**(`http://host:7878/` 一个 URL;xterm.js/css/fit vendor 进 `assets/web/vendor/`,零 CDN)。daemon 每 tab 有界字节环缓冲:连上重放重建屏 + 之后 live 字节(`Message::Binary`);背压 = 慢客户端 cap→断开重连(**不丢字节**,非网格的 drop-newest)。Lead 收口两 bug:① shell 退出 flush 尾字节;② web 重连先 `term.reset()` 防屏幕重复。**只读**(输入=片2)。
  - ⚠️ 遗留(非阻断,硬化跟进):① **死客户端 tx 泄漏** —— 空闲 PTY 时 fan_out 不跑→不剪除,弱网手机每 ~10s 重连堆积 `clients` Vec(同 T3b 清理缺口,干净修法有 shutdown 死锁张力,需小心);② **"关键帧"用近期字节尾**(非真网格快照)→ 环缓冲若从转义序列中段起,重建屏可能不完整(代码注释"最近字节完整=可见屏恒完整"是 overstatement;真关键帧应发当前屏的 ANSI/Snapshot)。
- ✅ **C(WS 去线程 + 片2 输入)** `7cb3567`+`4a5589c`+`859ed7f`(+ ADR-0017):**把 WS 从独立线程重构成注册进单一 calloop EventLoop 的 fd 源 = 彻底单线程**。`daemon.rs` 零 `std::thread`/`mpsc`/`channel`/`Arc`/`Mutex`/`Atomic`(grep+审查实证)→ 跨线程 / `Rc` 非 Send / 手搓双向轮询(忙等 / 无界缓冲 / 卡死收割那【一整类 bug】)**从架构上消失**。顺带把**片2 输入**(xterm.js `onData`→WS 源可读→直接 `on_input` 写 PTY,**无 channel 桥**)+ **软键栏**(ESC / ESC ESC 撤回改 / Ctrl-C / Tab / 方向键)落到这干净地基。reaper Timer 收卡握手 / 半开 / slowloris-read 写源 + SO_KEEPALIVE 探死;ws_peek 改消费式读(PrefixStream 退还完整握手)灭 level+MSG_PEEK 忙等。**线程版片1/片2 已被 C 取代,旧分支 `feat/kernel-input` / `feat/kernel-ws-live` 已删。**

**✅ 决策已定(2026-06-30):sync WS vs tokio → 选了第三条「C:去线程进 calloop」。** 既不继续手搓 sync 线程(A 的反复脆弱),也不引 tokio 大依赖(B);用项目本来的单线程 calloop 模型(回归 INV-001「绝不起 thread pool 做 IO」)把整类 bug 从根上消掉,**零新依赖**。这条是用户"为什么不塞回主线程"的直觉逼出来的,完整见 **ADR-0017**。

**⭐ 模型修订 R1(2026-06-29,与用户对齐;完整见 ADR-0015「修订 R1」)—— 取代下面旧链与部分 Decision:**
- **真终端 ≠ ssh**:会话是常驻一等实体、设备是窗口;但也 ≠ tty(不是关机才死)。
- **载荷传字节流、客户端本地渲染(不传网格)**:daemon = PTY 多路复用;客户端各自完整模拟器(Linux 原生 quill / 浏览器 xterm.js),**本地按自己宽度 reflow**。网格 `Snapshot` 降级为**连上时的关键帧**,不再逐帧广播网格。理由:网格/原始字节都按某宽折死,多端不同宽各自 reflow 须传"没焊死宽度的原料"(逻辑行)。
- **生命周期 = 连接显示端引用计数,无租约**:holder = 连着的显示端;**X = 释放该 holder,断线(后台/锁屏/网络)= 非事件,holder 归 0 才销毁**(0 holder = 没人看 = 回收,反孤儿;想脱机持久就**显式跑 tmux**,其 server 独立于 quill 的 PTY,quill 杀 PTY 后 server 照活可重 attach)。手机 = 镜子、不拥有任何东西 → **无需租约/grace/TTL**;连上默认**同步桌面全部工作区**。**不落盘持久化**(取代旧 `workspace.json`,主机重启全死=合理)。
- **⭐ 会话是反馈环,PTY ⟺ 桌面窗口【原子耦合】(关键,且治"反复找脆弱态"的病)**:一次 spawn 里 `PTY(源)→ {quill 桌面窗口, 手机}` 一起出生 —— PTY 是电脑进程、由 quill 管,**它一存在桌面就有窗口**。所以**"手机持有、桌面看不到"这个态根本构造不出来**(不是靠"事后自动补窗救援",是耦合让它不存在;"自动补窗"不是独立兜底步骤,是 spawn 的固有部分)。⟹ **桌面窗口靠构造永远是锚,手机永不会是唯一 holder** → 这才是"无租约"能成立、且手机后台/锁屏杀不掉会话的**真正原因**(不是因为补窗救了它,是脆弱态不存在)。
- **客户端矩阵**:Linux 原生 quill;手机/Mac/其它 = web(xterm.js,通用)。**不为跨平台移植原生 quill**(Mac 本地用 ghostty,要连家里走 web)。"任意位置"+ 安全 = WireGuard VPN 把门(以后可加 token 当登录)。

**⭐ 客户端渲染 / 多宽响应式策略(2026-06-30 定;用户"完整版"+ 表格 widget):桌面定宽 + 手机客户端两段式自适配**
- **冲突的根** = "多设备**不同宽**同时看";单设备 / 同宽都没问题(直接广播)。**解法:桌面为主、手机适配流** —— PTY 宽度 = 桌面显示器宽(primary),程序只按一个宽跑 → **"一个 PTY 两个宽"冲突根本不触发**;手机**不要求 PTY 变窄,客户端自己适配那份桌面宽的流**。
- **一套 JS/TS 客户端(手机/电脑/iPad 通用),保底 + 渐进增强两段**:
  1. **忠实渲染(保底,永远可得)** = 按桌面终端原样画网格(现 main 上 xterm.js 已是这段);手机大不了缩放/横滑。零损失、保证正确、**随时可切回核对 → 控制面安全**。
  2. **渐进响应式(纯增益,永不比保底差)**:
     - **文本(逻辑行)→ CSS 重排**(`<span>` 颜色/粗体 + `white-space:pre-wrap`),浏览器原生按设备宽重折 = 字面意义的响应式网页 → **主内容(CC prose/代码)全对**。
     - **表格 → 渲染成【可横滑/拖动的表格 widget】,绝不 cram-reflow**:带框(`│─┼`)确定性解析、无框对齐列启发式探测 → 当真表格画,**列对齐保留、表格内横向滚动**。⚠️ **手机版 GPT/Claude app 遇表格难看 = 它们去 cram(硬塞重排毁列对齐);正解是当可滚表格 widget。**
     - 探不出结构的无框 2D → 停在忠实视图(横滑),不强排。
- **真·响应式上限** = 程序**吐语义结构**(DomTerm / CC 用其内部 markdown)→ 拿源头结构,CSS 完全响应(表格窄屏改卡片堆叠等)。CC 是自己的工具可配合;`ls` 等遗留走上面"探测 + 保底"。
- **唯一真做不到**:任意遗留无框 2D 的【全自动可靠】重排(= 从有损渲染反推任意意图,AI-hard + 控制面不可靠)→ 有忠实保底兜着、降级成"滑一下",**非阻塞**。曾考虑训语义模型反推 → 否(层错了:结构在上游已被扔,且控制面不可靠;只在"源头不可改的遗留 TUI"才轮得到)。
- **关键洞察(用户逼出)**:① 桌面定宽 + 手机客户端适配 = 把多宽冲突绕没;② 两段式(忠实保底 + 渐进增强、保底一键可回)= 让"反推不可靠"不再危险,变成纯上限增益。
- **落地(已实现 2026-06-30,见 T6 分砖 A′)**:实际走 **client-side**(非原计划"daemon 发逻辑行")—— 客户端从 **xterm.js buffer** 还原逻辑行 + 属性 → CSS `pre-wrap` 重排 + 表格 widget;daemon 只加 dims 传播。**实测结论:reflow 对流式文本有效,对 CC 这种 \r 重绘 TUI 反推不可靠(2D-TUI 墙,CC 走忠实视图)** —— 详见 T6 分砖 A′ 条。

**T6 架构 = E′(2026-06-30 锁,见 ADR-0018)**:一个程序;**父 = 终端 own PTY 直接渲染(热路径零 IPC)**;**共享 = 懒启动的隔离子进程**(ADR-0017 的 WS-kernel,字节从父 tee 来;没手机=没子进程=零成本);子进程崩溃 OS 隔绝、终端无感。传输 pipe 现用 / shmem ring+eventfd 零拷贝升级备。

**T6 分砖:**
- ✅ **砖0(kernel-only)** `8fd2a41`(2026-06-30):冻 T6 协议(多 workspace 列表 + `Hold/Release` 生命周期 + 字节流 (workspace,tab) 标签 + 用起 `ServerMsg::Workspace(s)/WorkspaceInfo`)+ `Session` 多 workspace(`Vec<Workspace>` 各含 TabList + holders)+ **引用计数生命周期**(anchor holder=spawner;**显式 Release/Close vs 断线**靠 explicit 标志区分;refcount=anchor+holders 归 0 → `destroy_at` 移出 session→drop TabList→PTY SIGHUP)+ 死客户端收口(单线程 calloop 每断开路径 remove 全部源 + release_holder)+ web 真 X 发 Release(Text 帧)。**没碰 render/wl、零真线程原语**(Lead 独立核实);快 CI 全绿(lib 519 + kernel_ws_slice 10)。impl 末尾 API stall 漏提交块D测试 + Hold 无条件覆盖 held_ws 缺陷,均由 Lead 补/修(`2657edd`/`8fd2a41`)。
  - **砖1 carry-over**(审查记录,砖0 单工作区+恒 anchored 触发不到):① `held_ws: Option<u64>` 单槽 → 真多工作区持有须升 `HashSet<u64>` 且 Release 用消息 workspace_id;② `destroy_at` 销毁带【已注册 calloop fd 源】的工作区时须同步 remove 该源(否则悬挂源),砖0 唯一工作区恒 anchored 不销毁故安全;③ `StreamFocus` 焦点 tab vs 二进制泵硬编码 `data.tab_id`(接 `TabOp::Select` 时对齐);④ `ServerMsg::Snapshot` 在 WS 路径是死 variant(真关键帧=硬化项);⑤ `server_messages_json_roundtrip` 漏 Snapshot variant(trivial 补测)。
- ✅ **砖1a(kernel-only)** `b2df410`(2026-06-30):父↔子喂料地基(定 1b 要对接的契约)。**父↔子二进制帧 codec**(`src/kernel/feed.rs`:`[kind:u8][ws_id:u64][tab_id:u64][len:u32][payload]` 小端手写零依赖;`FrameKind` PtyOutput/Input/FocusChange/WorkspaceAdd/Remove;增量 decoder 处理半包/粘包+压实有界,先校 kind 再信 len、超 16MiB 拒)+ **daemon Fed 字节来源拓扑**(`SourceConfig` Local(自 spawn shell,standalone 保留)| Fed{read_fd,write_fd}:从父 pipe 解帧→PtyOutput 喂 fan_out、FocusChange→StreamFocus 广播,不开真 PTY;父 pipe EOF→停子)+ **输入回灌**(Fed 下 WS 输入分帧成 ≤64KiB Input 帧写父 back-channel、不写本地 PTY;Local 仍写 PTY)+ 合成喂料器集成测(5 测,含半包 + **背压整帧往返**)。**没碰 wl/render/main 桌面、零线程**(Lead 核);快 CI 全绿(lib 528 + feed 5 + ws 10)。impl 末尾 stall,3 块靠增量提交保住;审查逮 3 处 back-channel bug(write_frame 丢半帧→错位、大输入未分帧、Input 用焦点 ws 而非消息 ws)+ impl 漏改一调用点致编译错,均 Lead 收口(`b2df410`)。
  - **Fed 不复用 Session holders**(Lead 祝福的偏离):Session 焊死在持 PtyHandle 的 TabInstance,Fed 无 PTY 构造不出 → Fed 子用 clients 表 fan-out、held_ws=None;workspace holders/refcount 归父(砖1b,正合 E′ 父=holder#1)。
  - **砖1b carry-over**:① 父监管子的 workspace holders/refcount 信号;② WorkspaceAdd/Remove 帧接控制面元数据(现仅 debug 日志留接口);③ Fed Resize 回灌(帧集未含 Resize);④ FrameKind::Input 反向(父→子)目前忽略。
- ✅ **砖1b(碰 wl/window.rs)** `dc52bd4`(2026-06-30):**E′ 桌面接入端到端打通** —— `quill --share`(opt-in,默认关=今天的 quill)→ `ShareChild::spawn` 起隔离 `quill-kernel` 子(Fed 模式,socketpair/pipe + CLOEXEC)→ **tee 焦点 tab 的 PTY 输出喂子**(tee 点 window.rs **ContinueReading 分支**,转原始 `buf[..n]` 非 term_bytes、保 OSC-133,只焦点 tab)→ 子 fan-out 给手机(xterm.js);**子→父 back-channel 读源**(单 calloop Generic READ)解 Input 帧 → `queue_or_write_pty` 写 active tab(复用唯一写汇,与 paste/键盘同背压);`on_active_tab_changed` 发 FocusChange + `set_focus`。`share: Option<ShareChild>` 挂 LoopData 尾(非 State,不破 INV-008 drop 序)、像 `selection_state` 那样 Option 穿 `pty_read_tick(_inner)`。背压:非阻塞 + 队满丢【整帧】(子永不见半帧)。**render.rs 一行没改、键鼠/selection/pointer 零触、零真线程(只 Command::spawn 子进程)、默认路径字节等价**(Lead 独立核 + 4 路审 APPROVE);CI:lib 531(+3 share 单测)+ kernel/feed/ws + 代表性 wl e2e(single/multi_tab/cursor/kbd)全绿。impl 末尾 stall,2 commit + 1b-iii 重构靠增量提交保住、Lead 收口(`dc52bd4`)。
  - **🎉 至此 E′ MVP 通**:`quill --share` 能把你**正在用的桌面焦点 tab** 共享给手机(只读看 + 回打字),默认不开则完全是今天的 quill。
  - **砖1c carry-over**:① 父侧 workspace holders/refcount 信号(现 Fed 子 held_ws=None,生命周期归父未接);② WorkspaceAdd/Remove 帧接元数据(现 debug 日志);③ Fed Resize 回灌(帧集未含 Resize → 手机改不了 PTY 尺寸);④ 多 tab 寻址回灌(现仅焦点 tab);⑤ FocusChange 走"必达"通道(现与 PtyOutput 同背压路径,极端积压可丢 → 手机焦点标签短暂陈旧、自愈);⑥ **真机端到端实测**(`quill --share` + 手机经 VPN 连,迄今只单测/集成测,没真跑过)。
- ✅ **A′ 响应式渲染(client-side)** `5f5f6d2`..`e01d673`(2026-06-30):**① dims 传播**(桌面 PTY cols/rows → `FrameKind::Dims`→`ServerMsg::Dims`→客户端 `term.resize` 桌面宽 → 忠实视图与桌面像素对齐、宽度折行 artifact 消失;daemon 端 gate 在 `--share`,默认零回归)+ **② 逻辑行 CSS 重排**(从 xterm.js buffer 用 `isWrapped` 接回逻辑行 + 每格 fg/bg/bold 等 → `<span>`+`white-space:pre-wrap`,浏览器按设备宽重折)+ **③ 表格 widget**(带框 `│` / markdown `---|---` / 对齐列 → 可横滑 `<table>`;**裸 ASCII `|` 须配盒绘 `│` 或分隔行才算表** → git graph/管道不误判)+ **④ 忠实↔重排一键切换**(顶栏按钮,忠实=保底)+ **⑤ 滚动修复**(`.xterm-viewport{pointer-events:none}` 让外层 `#term` 独占平滑 2D、X/Y 不互锁;往返不退化:`#term` 不缩 1px、改 `#stage` 包层 + `#reflow` 叠盖)。**只动 `assets/web/index.html` + kernel dims(proto/feed/daemon/window.rs);`render.rs` 一行没改、键鼠/selection 零触、默认路径零回归**;vm-DOM 58 测 + 代表性 wl e2e 绿。
  - ⚠️ **CC 反推重排的天花板(实测确证 2026-06-30,别再试反推)**:reflow 对**流式文本(shell/日志/prose/代码)有效**;但 **Claude Code 这种 `\r` 重绘 TUI** —— 输入框 = **两条满宽 `─` + 中间 `❯`**(无 alt-screen、无 2D 定位,但靠 \r 重绘)+ 状态栏 `[Opus..] │ user`(带 `│`)—— **反推不可靠**:实测(无头 Chrome 渲真 CC + DOM dump)状态栏的 `│` 被 `borderedTableEnd` **误判成表格 widget**(确证),宽终端+窄浏览器下整体糊成"一堆线"(宽浏览器正常)。**这是 CLAUDE.md 客户端渲染策略早标的"任意 2D TUI 可靠重排=AI-hard"那堵墙。CC 用【忠实视图】本就干净(实测"只有一条线")** → 定位:**重排=给非 TUI 流式内容;CC 走忠实**。真·非妥协解 = **CC 专属结构渲染**(CC 是自家工具 → 读其 transcript 渲对话 + 手机原生输入框替代它画的输入框;源码没开但可写插件/hook + transcript 已是结构化产物),归 T7+。
  - **A′ carry-over**:① **状态栏 `│`→表格误判**(单个 `│` 的状态行不该成表,可修,DOM dump 可验)② **service-worker 缓存静态资产**(弱网首屏快;现 daemon 已发 `Cache-Control:no-store`,手机非缓存问题)③ "弱网一次刷一行"=字节涓流(网络锅、非 client bug,xterm 已 RAF 批渲)④ 忠实视图手机端只平移当前屏、翻历史用重排(scroll 修复的取舍)。
  - **⚠️ 工具限制(本会话踩的)**:主 session + 子 agent **都没 puppeteer**;无头 Chrome harness 能跑 DOM dump(可信)但**视觉截图布局失真(#stage 无高度→空白,不可信)**。要真看手机渲染只能靠用户截图。**复现条件 = 宽终端(307列)+ 窄浏览器(手机宽);宽浏览器正常。**
- **部署(实测路径)**:web 资产 `include_str!` 烤进 **`quill-kernel`** → 改 `index.html` 必须**重建 + 重装 quill-kernel**(`cargo build --release --bin quill --bin quill-kernel`,`install -m 755` 原子免 ETXTBSY)。**✅ 两套安装位是【有意的稳定/测试分流】(2026-06-30 用户定,非待收口)**:
  - **`/usr/local/bin/quill`(root,系统级)= 稳定日用** —— 桌面图标 Exec 指它;**只在某版【证实稳了】才 `sudo install` 提升上来**(图标传不了 `--share`,日用就是普通终端)。
  - **`~/.local/bin/quill`(用户级,shell `which quill`)= 测试** —— AI 每次重编装这里(**免 sudo**),shell 里 `quill` / `quill --share` 跑最新、不碰日用稳定版。
  - 测共享:shell 跑 `quill --share`(起隔离 `quill-kernel` 子绑 `0.0.0.0:7878`)→ 手机经 VPN `http://10.0.0.2:7878/`(本机 LAN=10.0.0.2,VPN 把门)。⚠️ `quill --share` 找 `quill-kernel` 是【同目录 sibling】→ 装 quill 必同时装 quill-kernel 到同一 bin 目录。
- 🚧 **共享开关(运行期 toggle,在飞 `feat/share-toggle`)**:标题栏按钮 + `Ctrl+Shift+S` 运行期开/关共享(抽 ShareChild spawn/teardown 成 `toggle_share`),替代"第二个图标"那个烂招(合后删 `~/.local/share/applications/quill-share.desktop`)。碰 render.rs(标题栏)。
- **砖2(待做):手机端 tab/工作区管理** —— 现手机只镜像+控焦点 tab,没法新建/关闭/切换。协议早备(砖0 的 `ClientMsg::TabOp` + `ServerMsg::Workspaces/WorkspaceInfo/TabMeta`)但端到端没接:① web 客户端没标签栏 UI(也没渲 Workspaces/TabMeta)② 父↔子 back-channel 只跑 Input 帧、缺 TabOp 帧 ③ 子 `handle_client_msg` TabOp 显式没接线 + **E′ 里 tab 归桌面父** → 链路须 `手机→WS→子→back-channel(新增 TabOp 帧)→父执行 TabOp→广播新 Workspaces→手机刷新`。= R1"连上同步全部工作区"那半。与共享开关同动 window.rs/web → **串行做**。
- **T7** web"X 关闭"语义 / PWA 壳(+ 登录 token)→ **硬化**(真网格关键帧替字节尾、bincode、带宽)。
**⚠️ 凡碰 render/wl 的 ticket(砖1 起),impl/审码 agent 必须自己逐行读 `wl/render.rs`、`wl/window.rs` draw 路径,别信摘要。**
**⏱️ CI:`ci.sh --fast`(fmt+clippy+build)≈17s 增量;全门 ≈12–25min(33 测里 28 个是 GPU/wl e2e、串行,有一个 e2e binary 单独 ~12min)。kernel 砖改动【不影响】那 28 个 → 可只跑快门 + kernel/pty/ws 测子集(<1min);加 `ci.sh --kernel` 模式是省时选项(暂未做)。**

**协作环(本阶段在用)**:每砖一个 worktree(`git worktree add ../quill-impl-<x> -b feat/<x>`)→ 写码 **Opus xhigh**(改 render 必自己读 `wl/render.rs`,别信摘要)→ 自跑 `scripts/ci.sh` 调全绿 → **多视角对抗审 Opus xhigh** → Lead(user + 主 session)裁决合并(纯加法的 cohesive 单模块 commit 可超 300 行软线)。⚠️ workflow 每个 agent 显式写 `model:'opus' effort:'xhigh'`,别靠默认(`agentType` 自带便宜模型如 Explore=Haiku)。

## CI(本地,不走远端)

```bash
./scripts/ci.sh          # 全门:fmt + clippy + build + test(AI 合并前 / main 把门)
./scripts/ci.sh --fast   # 快门:fmt + clippy + build(pre-push 用)
git config core.hooksPath scripts/hooks   # 启用 pre-push 钩子(一次性)
```

**为什么本地不走 GitHub Actions**:quill 的渲染/Wayland e2e 测试要本机 GPU + Wayland 会话,GitHub runner 跑不了;本机更快更全。**AI 协作环**:写码 agent 写 → 自跑 `ci.sh` 调全绿 → 审码 agent 审 → 合并 main → main 上全门把门。

---

## 技术栈(锁死,非 ADR 不改)

| 层 | crate | 为啥 |
|---|---|---|
| Wayland 客户端 | `smithay-client-toolkit` | Wayland 生态主流 |
| 事件循环 | `calloop` | Smithay 配套,单线程 epoll |
| 渲染 | `wgpu` | 6K 分辨率需要 GPU |
| 终端状态机 | `alacritty_terminal` | Alacritty 拆出来的核心,成熟 |
| 字体 + CJK shaping | `cosmic-text` | COSMIC 桌面用,CJK / fallback 都做好 |
| PTY | `portable-pty` | fork + openpty 封装 |
| 输入法 | 手实现 `text-input-v3` | 没现成 crate,wayland-scanner 生成绑定 |
| 日志 | `tracing` + `tracing-subscriber` | Rust 主流 |

---

## 架构(50-ft 鸟瞰)

```
┌────────────────┐          ┌──────────────────┐
│ Wayland        │          │ 子进程 (shell)   │
│ compositor     │          │                  │
└────────┬───────┘          └────────┬─────────┘
         │ wl events                 │ bytes
         │                           │
         ▼                           ▼
    WinBackend ───► main ppoll ◄── PtyHandle
         ▲         (calloop loop)    ▲
         │                           │
       draw() ◄── StateMachine ◄── VtParser
                  (grid, cursor,    (alacritty_terminal)
                   scrollback)
```

**不变式**:
1. 所有 IO fd(wayland / pty / timerfd / xkb / d-bus)全部注册到同一个 `calloop::EventLoop`,绝不起 thread pool 做 IO
2. 渲染线程不做 >1ms 的任何计算,长任务分帧
3. PTY 写入必须非阻塞,背压时丢帧而非 stall

**模块切分**(实测 2026-06-29):
- `wl/` — Wayland client + wgpu 渲染 + 事件循环(calloop)+ 240-字段 `State` 巨对象
- `pty/` — 子进程 + fd 读写(`PtyHandle`)
- `term/` — VT 解析 + 屏幕/滚回状态(包 `alacritty_terminal`)
- `tab/` — 多 PTY/标签工作区(`TabList`/`TabInstance`/`TabId`)
- `composer/` — inline 补全引擎
- `completion/` — 补全触发/探测(bwrap 沙箱)
- `text/` — cosmic-text 字体 cache + shaping
- `ime/` — fcitx5 text-input-v3
- `main.rs` / `lib.rs` — 入口 + 接线;`run_headless_screenshot` = 无头渲染范式

---

## 开发准则(强制,审码 会挡)

- **一次 commit 做一件事**,diff < 300 行(大改拆成多个)
- 禁止"顺手优化" —— 性能改动必须单独 commit,附 bench 证据
- 非 `main`/`tests` 代码**禁用 `unwrap()` / `expect()`**,用 `?` 或 `anyhow`
- 任何 `unsafe` 必须有 `// SAFETY:` 注释说明不变式
- 依赖加新 crate → 必须 ADR(architecture decision record,存 `docs/adr/`)
- 注释只写 **why**,不写 what;自描述代码不加注释
- **先写测试再写实现**,骨架可以先挖坑(`#[test] fn name() { todo!() }`)
- 单 feature 打开 1 周没合 → 关闭 feature flag 重新切分

---

## Multi-agent 协议

**Lead = user + Claude Code 主 session**。不写码,做 规划 / 合并 / 最终裁决。

**强制 Agent Team 模式**(与用户全局 CLAUDE.md 一致,本项目硬约束):

核心价值(按重要性排):
1. **可审计** — 每个 agent 有 name + 共享任务板,Lead 随时能看每个 agent 进度到哪、输入输出啥
2. **并发可控** — team 级预算 / 状态一致性,agent 不私自疯跑

通信(`SendMessage`)是**副作用**,不是目的。Lead 介入优先,agent 之间握手能不做就不做。

硬约束:
- 所有并行 / 复杂任务必须 `TeamCreate` + `team_name` + `name` 寻址
- **禁止无 `team_name` 的一次性 fan-out** —— 只有单行无状态查询(例: "列出当前 git 分支")可例外
- team 名格式 `quill-phase<N>`,阶段结束 `TeamDelete` 清理

**Teammate 角色**(spawn 时明确,一个 teammate 只担一个角色):

| 角色 | 职责 | 实例数 |
|---|---|---|
| **规划** | 一次性:写 ADR / 定模块切分 / 定接口形状 | 阶段起始 1 个,然后退 |
| **写码** | 从 `tasks/` 抢未认领 ticket,在自己 worktree 干 | 2 个并行 |
| **审码** | pre-commit 审查 diff 是否违规 | 1 个常驻 |
| **跑测** | main 每次更新后跑 full test + soak + bench,发现回归开 issue | 1 个常驻 |

**Spawn prompt 必须自包含**(teammate 不继承 lead 对话):
- 任务目的(why)+ 边界(don'ts)+ 交付物(具体文件路径 / 测试)
- 相关背景文件路径(CLAUDE.md / 相关源码 / ADR)
- 接受标准(`cargo test && cargo clippy -- -D warnings && 审码 放行`)
- 硬预算:`tokenBudget=100k / wallClockTimeout=3600s / costBudget=$5 / recursionDepth=10`

**Worktree 约定**:每个 写码 用独立 worktree:
```bash
git worktree add ../quill-impl-<name> -b feat/<ticket-id>
```

**合并流程**:
1. 写码 本地 commit → 通知 审码
2. 审码 review diff:通过 → 写码 merge 到 main;不过 → 改了再来
3. 跑测 监听 main 新 commit → 自动跑 full test + 1h soak
4. 回归 → 开 issue 扔回 task queue

**禁止按组件分 agent**(反模式):不要"UI agent / DB agent / Docs agent",组件间天天卡接口。按**任务类型**分。

---

## 常用命令

```bash
# 构建 + 四门验收
cargo build
cargo test                            # Phase 1 完有 29 tests(含 state_machine 11 / frame_stats 3)
cargo clippy --all-targets -- -D warnings
cargo fmt --check

# 启动窗口(Phase 1 起跑得动)
cargo run --release                   # NVIDIA 5090 Wayland 自动选 Vulkan, 无需 env

# 以后
# cargo test --test soak_1h           # 1h 稳定性(Phase 6 T-0601)
# cargo bench --bench render_frame    # 渲染性能
```

## 已验证信号 (Phase 1 实装, 2026-04-24)

- NVIDIA RTX 5090 + Wayland + wgpu 0.29 → Vulkan backend **自动选中不 hang**
- xdg-toplevel 首次 configure 发 `new_size=None`, 需 fallback 到初始尺寸
- compositor 会周期性重发 configure(focus / 窗口管理器事件), 用 `resize_dirty` idempotent 过滤

---

## 禁止清单

- `println!` 调试(用 `tracing::debug!`)
- 非受控 `unwrap` / `expect`
- 架构级改动(模块切、数据流变)跳过 ADR
- IO 塞进渲染线程
- "以后再说" 的 TODO,要么现在解决,要么开 issue

---

## 相关文档

- `ROADMAP.md` — 分阶段任务
- `docs/adr/` — 所有架构决策(Phase 0 末建立)
- `tasks/` — 任务 queue(Phase 1 建立)
