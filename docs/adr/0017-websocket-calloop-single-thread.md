# ADR 0017: WebSocket 传输去线程 —— 全部注册进单一 calloop EventLoop(单线程)

## Status

Accepted, 2026-06-30(取代 ADR-0016 的「同步 tungstenite + `std::thread`」线程模型;
tungstenite 仍是 WS 协议库,只换驱动方式)

## Context

ADR-0016 的 WS 子系统跑 3 类 `std::thread`(broadcaster / acceptor / per-conn writer)+
`std::sync::mpsc`(PTY 字节 `Vec<u8>` 过线程边界)+ `Arc<Mutex<Shared>>`(环缓冲 + 客户端
注册表),因为 [`Session`] 含 `Rc<RefCell<…>>` 非 `Send`(ADR-0015 头号约束),只能让
owned 字节过线程。

这套**同步手搓双向 WS 生命周期**反复出微妙 bug(CLAUDE.md 记 4 轮):忙等、慢客户端
无界缓冲、永久不读连接的槽不回收(DoS)、死客户端 tx 泄漏(空闲 PTY 时 broadcaster 不
跑 → 不剪除 → 弱网手机每 ~10s 重连堆积 `clients`),且核心 DoS 修复**难写出真覆盖的
回归测试**(需 fiddly 操控 `SO_SNDBUF` 让 flush 恒 Pending)。根因是「跨线程 + 手搓双向
非阻塞轮询」这一整类复杂度。

ADR-0016 的 Alt 3(「calloop `Generic` 单线程裸注册 `TcpListener`,不起线程」)当初被否,
两条理由:① 慢客户端握手/帧解码 stall 单线程 loop;② 单线程驱动 tungstenite 非阻塞
状态机复杂度高。本 ADR **有意翻案**:正面啃下这两点,把 WS 全搬进 calloop。

## Decision

**WS 子系统去掉所有线程,全部注册成同一个 `calloop::EventLoop` 的 fd 源**(回到 INV-001
「所有 IO fd 进同一 calloop,绝不起线程池做 IO」)。`Session` / PTY / WS 全在 calloop
线程 → `Rc` 非 `Send` 不再是问题 → **删掉 `Arc` / `Mutex` / `mpsc` / channel 桥**。

实现要点(`src/kernel/daemon.rs`):

- **listener**:`TcpListener` 注册成 owned `Generic` READ 源;可读→`accept` 排空到
  WouldBlock→每个新连接设非阻塞 + 注册成它自己的 READ 源(owns 该 `TcpStream`,当可读
  信号 + `peek` 句柄)。
- **同口 HTTP / WS 分流**:连接 READ 源可读时把请求头**消费**(`read`,非 `peek`)进 per-conn
  累积缓冲(`WsStage::Peeking(Vec<u8>)`);头没收全(`WouldBlock`)就 `Continue` 等下次新字节
  (**不 `thread::sleep`**)。带 `Upgrade: websocket` 走 WS(已消费的请求字节经 `PrefixStream`
  退还 tungstenite 从头读完整握手),否则当 HTTP(响应排进一个非阻塞写源写完即 `Remove`)。
  - **⚠️ 忙等修复(对抗审出的 must-fix)**:最初用 `peek`(MSG_PEEK 不消费内核缓冲)+
    `Mode::Level`,半截头时 fd **恒可读** → calloop 每轮 ~0 超时重派分流回调 → **紧凑自旋
    烧满一个核**(实测 99.3%),直到客户端把头发全。经典 level-triggered + MSG_PEEK 陷阱。
    改成**消费式**读把 fd 排空,Level 不再恒触发,半截头静默等下次可读。WS 分支因此手里
    握着完整握手请求字节,用 `PrefixStream`(`Cursor<Vec<u8>>` 前缀 + socket dup)当流喂
    tungstenite:`read` 先吐前缀、耗尽后回落到 socket,握手完成后前缀已耗尽 ≈ 裸 dup,故可
    沿用为 Live 阶段流类型。
- **非阻塞握手**:`tungstenite::handshake::server::ServerHandshake::start` 拿 `MidHandshake`,
  存进连接状态;fd 再可读时 `MidHandshake::handshake()`(按值消费,`Interrupted`/WouldBlock
  时把推进后的状态存回)续做,直到 `Ok(WebSocket)`。**握手不阻塞 loop**(否决理由②化解)。
- **输出(PTY→浏览器)**:`pty_readable` 同线程把字节 append 进环缓冲 + fan-out 进各 live
  连接的有界出站队列,打开其 WRITE 兴趣。连接可写→排空队列(非阻塞 `ws.write`/`flush`)→
  **排空后关 WRITE 兴趣退回只 READ(`PostAction::Disable`,不忙等)**;有新出站再 `enable`。
- **输入(浏览器→PTY)**:连接 READ 源可读→`ws.read()` 排空→数据帧字节直接
  `Session::on_input` 写 active tab PTY(**无 channel、无线程**)。
- **背压**:字节流非幂等(丢任一字节 = VT 状态机错位),**绝不丢/合帧**;某连接出站积压
  超 `WS_CLIENT_OUT_CAP`(1 MiB)→ 断开它(`loop_handle.remove` 两个源 + 关 fd 回收,它重连
  从环缓冲重放恢复)。
- **死客户端回收(顺带修 ADR-0016 遗留泄漏)**:对端关闭 → 该连接 READ 源拿到 EOF/Close →
  `PostAction::Remove` 立即回收。**结构性消灭**了线程版「空闲 PTY 不剪除死 tx」的泄漏 ——
  不再依赖「有 PTY 输出时才 fan-out 才剪除」。
- **卡握手 / 半开连接收割(对抗审出的顺带修)**:一个周期 `calloop::Timer` 源
  (`reap_stale_clients`,默认每 5s)扫描所有连接,回收卡在 Peeking/Handshaking 阶段超
  `handshake_deadline`(默认 10s,自 accept 起的**绝对**期限 → robust against slowloris
  逐字节拖延)的连接(`loop_handle.remove` 其源 + 关 fd)。**live 连接不在收割列**(健康但
  安静的终端无收发也正常,按空闲超时会误杀);其网络静默掉线(无 FIN/RST)的半开态靠 accept
  时设的 **SO_KEEPALIVE**(idle 60s / intvl 10s / cnt 3,经 `libc::setsockopt`,无新依赖)
  让内核探测对端死活 → 死则 fd 报错 → epoll 唤醒 READ 源走既有回收路径。收割周期 / 握手
  期限可经 env(`QUILL_WS_REAP_MS` / `QUILL_WS_HANDSHAKE_DEADLINE_MS`)覆盖(调参 / 测试用)。

**fd 所有权拆分(否决理由②的具体啃法)**:一条 live WS 连接用 3 个 dup 的 fd(都
`try_clone`,不同 fd 号,故同一 socket 在 epoll 里不会重复注册 EEXIST):READ 源 own
`original`(可读信号)、`WebSocket` own 一份做真 IO、WRITE 源 own 一份(可写信号)。各源
`PostAction::Remove`/`drop` 各自 `EPOLL_CTL_DEL` + 关自己那份 fd,drop 序干净(无 EBADF
噪声)。慢客户端(否决理由①)不再 stall loop:握手是非阻塞状态机,慢/不读客户端只是积压
进它自己的有界出站队列,超 cap 即被同步回收,**不拖累 loop 也不拖累别的客户端**。

零新依赖(tungstenite 0.29 仍只当协议库;calloop 已有)。

## Consequences

### 正面
- **一整类 bug 消失**:忙等(WRITE 兴趣按需开关)、无界缓冲(per-client 有界 + cap 断开)、
  槽不回收 / 死客户端泄漏(READ 源 EOF 即回收,结构性)、跨线程同步(没有线程了)。
- **可测性变好**:慢客户端回收在单线程下**同步**发生在 fan-out(cap 判定即 `remove`),
  不依赖线程时序 —— 回归测试直接「喂超 cap 字节 → 断言连接被关」即可确定性覆盖(对照
  ADR-0016 卡点:核心 DoS 修复的回归测试假绿)。
- **`Rc` 非 `Send` 不再是约束**:`Session` / 环缓冲 / 客户端注册表都是 calloop 线程的普通
  字段,无 `Arc`/`Mutex`/`mpsc`;输入直接 `on_input`,输出直接 fan-out,数据流一眼看清。
- **守 INV-001**:所有 IO fd 回到同一 calloop;ADR-0016 的「WS 走独立线程」INV-1 偏离作废。

### 负面 / 代价
- **每条 live WS 连接 3 个 fd**(原始 + IO dup + 写信号 dup)。daily-drive 个位数客户端,
  fd 预算可忽略(`MAX_WS_CONNS` 仍封顶 16)。
- **单线程驱动 tungstenite 非阻塞状态机**(握手 `MidHandshake` 续做 + 出站 WouldBlock 重试
  + WRITE 兴趣开关)有内在复杂度 —— 但它是**确定性的事件驱动**,比手搓跨线程双向轮询
  好推理、好测;且 calloop 的 `PostAction`(`Remove`/`Disable`/`Continue`)在回调返回后才
  应用(借用已释放),自删/自关 WRITE 兴趣都干净。

### 已知残留(非本切片 scope)
- **多 tab**:输入恒投 active tab;多 tab 动态增删 + 输入按 tab 寻址 + resize 协商留 T6。
- ~~**半截连接超时**~~:已修 —— 收割 Timer 按 `handshake_deadline` 回收卡握手连接(见 Decision)。
- ~~**探活**~~:已修 —— accept 时设 SO_KEEPALIVE(idle 60s)→ 内核探死半开 live 连接 →
  既有 READ 源回收路径(见 Decision)。

## 实装验证

- `src/kernel/daemon.rs` 全 calloop 化(无 `std::thread` / `mpsc` / `calloop::channel` /
  `Arc` / `Mutex` / `Atomic`)。
- `tests/kernel_ws_slice.rs` 6 条端到端(真子进程):同口 HTTP 出页、WS 字节流重放+live
  保活、**输入回灌往返**(`RUNDONE\n` → `GOT[RUNDONE]`)、**慢客户端超 cap 被断开回收**、
  **半截头不忙等**(2s 窗内 daemon CPU jiffies < 阈值,确定性)、**卡握手连接超 deadline
  被收割**(client 读到 EOF 而非读超时)。
- daemon 单测覆盖环缓冲 / PTY 分类 / 同口分流 / HTTP 路由等纯函数。
- 4 门绿(`scripts/ci.sh`:fmt + clippy + build + test)。

## 相关文档

- 前序: `docs/adr/0016-websocket-transport.md`(线程版 + Alt 3 当初否决理由,本 ADR 翻案)
- 主 ADR: `docs/adr/0015-headless-kernel-split.md`(§5 fan-out + `Rc` 非 `Send` 头号约束)
- 实装: `src/kernel/daemon.rs` + `src/kernel/session.rs`(`on_input` 部分写 + EINTR)+
  `assets/web/index.html`(onData 输入 + 软键栏)+ `tests/kernel_ws_slice.rs`
