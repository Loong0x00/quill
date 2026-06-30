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

**重排下一步(ticket 表见 ADR R1;片1+片2 已经 C 落地在单线程 daemon 上)**:**下一砖 = T6 引用计数生命周期**(holder = 连着的显示端;X 释放 / 断线非事件 / 归 0 销毁;桌面 quill 当 daemon 客户端 + 自动补窗当锚 + 同步全部工作区 —— 这才是"共享你真在用的工作区",目前 daemon 还是自己 spawn 一个独立 shell、没接桌面 quill)→ **T7** web"X 关闭"语义 / PWA 壳(+ 可选登录 token)→ **硬化**(真网格关键帧替代字节尾、连接数/带宽、bincode)。
**⚠️ T6 起改的是连接生命周期 / 桌面 quill 接入(`wl/` 那 240 字段 State)—— 凡碰 render/wl 的 ticket,impl/审码 agent 必须自己读 `wl/render.rs`、`wl/window.rs`,别信摘要。**

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
