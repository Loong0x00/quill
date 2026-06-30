# ADR-0018: 共享 = 终端父进程监管的隔离子进程(E′),终端绝对优先

状态: Accepted(Phase 7 T6,2026-06-30)
关联: ADR-0015(R1 共享会话模型)、ADR-0017(WS 去线程进单 calloop)、CLAUDE.md「客户端渲染策略」

## Context

T6 要把桌面 quill 的工作区共享给手机,但有两条**硬约束**(用户反复强调):
1. **它首先是一台终端**(daily driver)—— 共享是加分项,**绝对不能因共享出错/卡/慢**;
2. **是一个程序**(用户跑这一个 `quill`),不是"两个要我伺候的程序",也不是 fork。

三个候选拓扑:
- **C(桌面当独立 daemon 的客户端)**:独立 `quill-kernel` own PTY,桌面 quill 当 socket 客户端。→ **否决**:终端的渲染/键盘**热路径过 IPC**(每键每帧穿进程边界)= 把日用终端置于险地;桌面"锚"退化成策略;两套热路径要维护。
- **E(同进程内嵌)**:桌面 quill 进程内嵌 kernel,共享代码与终端同进程。→ **否决**:同进程**无硬隔离** —— 共享代码 panic/abort/UB/OOM 能拖垮终端(`catch_unwind` 接得住 panic,**接不住 abort/UB/OOM**)。
- **E′(终端父进程 + 监管的隔离共享子进程)** → **采纳**:同时拿到"一个程序 + 终端硬隔离 + 热路径零 IPC"。

## Decision: E′

- **`quill` = 一个程序**(用户只跑这一个)。**父进程 = 终端**(Wayland + wgpu 渲染),**own PTY、直接读它渲染** —— 终端热路径**零 IPC、≈ 今天的 quill,零回归**。本地键盘 → 父直接写 PTY。
- **共享 = opt-in、受父监管的隔离【子进程】**(2026-06-30 定:懒 = **opt-in 默认关**,非"首连才 spawn"的真懒):
  - **默认不开 → 根本不 spawn 子进程**(终端就是今天的 quill,零开销);`--share`(或运行期 toggle)打开才 spawn,quill 退出收子。
  - **`--share` 开但没手机 = 空闲子进程**:阻在 epoll/read,无 CPU、RSS 极小 ≈ 零成本 → 就让它挂着。**不做"首个手机连入才 spawn"的真懒**(那要父预先 listen + 移交 fd 给子 = 把 listener 塞回父进程、违背"父不碰共享";listener 留在隔离子里更干净)。
  - 子进程 = ADR-0017 那套**单线程 calloop WS-kernel**(WS 监听 + 同口 HTTP + 客户端 + reaper),但**字节从父的 tee 来、不自 spawn shell**;子的 WS 输入**回灌父**(父 own PTY),子自己不写 PTY。
  - **隔离 = OS 进程边界 → 不用 `catch_unwind`**(它只接 panic、接不住 abort/UB/OOM,正是否决 E 的理由)。子崩溃靠**父读子 pipe 读到 EOF** 察觉(SIGCHLD 选配且须**限定到已知 kernel-child pid**,否则抢 portable-pty 对 shell 子的 reap);察觉后**拆掉所有子相关源/fd、降级回纯终端**,下个手机连入再 spawn 干净子(从 ring 重放重建)。
- **父 ↔ 子(IPC,只在共享支线、在终端热路径之外)**:
  - 父→子:**tee 一份 PTY 输出**(转发父为渲染本就读进 buffer 的那批字节,多一次 `write`,不重读/不解析);只 tee 手机在看的 active tab。
  - 子→父:手机输入字节 → 父写进 PTY(人手速,可忽略)。
  - 子进程慢/死 → 父丢弃 tee / 重启子,**终端不受影响**。
- **传输**:**现用 pipe / unix socketpair**(KB/s 终端流,那次内核 buffer 拷贝微秒级、非瓶颈,省事)。
  - **父↔子帧 = 轻量二进制 `[kind][ws_id][tab_id][len][payload]`**(**非 proto JSON**:父→子是每-chunk 热 tee 路径,跑 `serde_json` 违背"近免费";(workspace,tab) 标签落二进制头,借砖0 `StreamFocus` 概念)。tee 写**非阻塞**,满了丢/拆子(共享支线丢得起,**绝不阻塞终端**)。
  - **零拷贝升级路径(备)= shmem ring + `eventfd`**:父 `read(PTY)` 直接进共享环 → 子原地读 → tee 零额外拷贝。同步 = head/tail 原子索引 + **acquire/release 内存序**(多核可见性,非"撕裂字节":字节流无半字节问题,真问题是"索引更新先于字节写入对另一核可见")+ 环满背压,**无锁**。仅当吞吐真需要才上(终端文本不需要,属优雅非性能)。

## 终端绝对优先 —— 怎么保证(E′ 天然满足)

1. **独立 quill 永远能跑**:共享是 opt-in / 懒启动,默认就是今天的终端;
2. **渲染 + 输入热路径零 IPC、零回归**(父 own PTY 直接画/写);
3. **共享子进程任何崩溃 = OS 隔绝**,终端无感(比 E 的 `catch_unwind` 硬,扛 abort/UB/OOM);
4. **没开 `--share` = 没子进程 = 零成本**(=今天的终端);开了但没手机 = 空闲子进程阻在 epoll ≈ 零成本。

## 生命周期(引用计数,接 ADR-0015 R1)

- holder = 连着的显示端;**父进程构造上即 holder #1**(终端在,会话就在;PTY⟺窗口原子耦合);手机 = 子进程里的 WS holder。
- **X 显式关闭** = 释放该 holder;**断线(后台/锁屏/网络)= 非事件**;refcount→0 才销毁工作区(drop TabList → PTY SIGHUP)。无租约/grace/TTL。
- 多 workspace:子进程 kernel(`Session`)持多 workspace;父维护本地镜像(给 wgpu 渲染的 alacritty `TermState`,**每个手机自己 xterm.js = 自己那份**,服务端不强求第二份 —— 仅"服务端做 reflow"时子进程才需自己的 grid,默认 reflow 放客户端/CSS,见 CLAUDE.md 渲染策略)。

## 分阶段

- **砖0(kernel 侧,transport/E′-agnostic,不碰 render/wl)**:冻 T6 协议(proto:多 workspace + Hold/Release/X-close + tab 标签流 + 用起已定义未接线的 `ServerMsg::Workspace/WorkspaceInfo/TabMeta`)+ `Session` 多 workspace 数据模型 + 引用计数生命周期(holder / 关闭 vs 断线 / 归 0 销毁 / 顺带收口死客户端泄漏)+ web 端真 X 发 close。
- **砖1+(父监管子 + tee PTY + 桌面接入)**:单作者动 `window.rs`(240 字段 State;tab/pty 触点 127 处全在此一文件,`render.rs`/键鼠/selection 零触点 → **render.rs 不改**),藏在运行期 `--share`/lazy 路径后,护 daily-driver。impl/审码**必须逐行读** `wl/render.rs`、`wl/window.rs` draw 路径。

## 风险

- 父↔子协议 + 子进程监管/重启的管道(可控);
- `window.rs` 240 字段 State 的 drop 序(INV-001/008)在 tabs 改"本地镜像 + 子进程连接"后须 **cargo 编译实证**;
- daily-driver 回归:运行期 flag + 独立 quill 路径不变 + soak 验。
