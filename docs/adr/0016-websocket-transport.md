# ADR 0016: 无头内核 WebSocket 传输 —— 引 `tungstenite`(同步,无 tokio)

## Status

Accepted, 2026-06-29

## Context

Phase 7 T2(`14e2a33`)证通了 spine:被驱动的 tab → `Snapshot` → **unix domain
socket**(line-delimited JSON)→ 客户端(`quill-dump`)。但 unix socket 只能本机
进程相连,**浏览器连不上**:

- 浏览器只能 `new WebSocket("ws://host:port")` —— 走 TCP,不是文件路径 socket。
- WebSocket 需要 HTTP `Upgrade` 握手(`Sec-WebSocket-Accept = base64(SHA1(key +
  GUID))`)+ 帧封装(opcode / mask / len),不是裸 JSON 行。

T3a 的目标是把这条 spine 延到浏览器:**手机经 WireGuard VPN → 路由器 →
`10.0.0.2:<port>`** 连上 quill-kernel,收到一帧 `Snapshot` 并显示。需要在 daemon
侧加一个 TCP + WebSocket 端点。

**头号约束(ADR-0015)**:`TermState` / `TabInstance` 含 `Rc<RefCell<String>>`
(`term/mod.rs:512/570`)→ **非 `Send`**。`Session` 不能跨线程 move,WS IO 线程
永远拿不到 `&Session`。跨线程边界**只能过 owned 序列化字节**(`Snapshot` 是纯
derive、Send,但按 ADR-0015 §5 在 calloop 线程序列化成 `Vec<u8>` 后再发)。

CLAUDE.md「依赖加新 crate → 必须 ADR」硬约束触发本 ADR。WebSocket 协议
(握手 + 帧 mask/解码)正确性不该手搓,需要一个库。

## Decision

引 `tungstenite = { version = "0.29", default-features = false, features =
["handshake"] }` 作 dep(非 dev-dep,daemon 主路径用)。

**关键取舍:同步 `tungstenite` + `std::thread`,不上 tokio。**

拓扑(daemon 侧,加在现有单线程 calloop 之上):

```
calloop 线程 (own Session, !Send)          WS 子系统 (owned 字节, Send)
  pty_readable: drain → on_pty_output
    drain 完若 dirty:
      snapshot_active() → serde_json::to_vec ──┐ std::sync::mpsc::Sender<Vec<u8>>
      clear_dirty                              │
                                               ▼
                              [updater 线程] rx.recv() → 存 Arc<Mutex<最新字节>>
                              [acceptor 线程] TcpListener.accept()
                                  → 每连一个短命线程: tungstenite::accept(握手)
                                    → ws.send(最新字节) → close
```

- **跨线程载荷 = `Vec<u8>`**(JSON 序列化在 calloop 线程做完),绝不让 `Rc` /
  `Session` / `Snapshot` 引用过线程边界。
- bind 地址可配(`--ws-bind=<addr:port>`,默认 `0.0.0.0:7878`),LAN 可达;
  安全靠 WireGuard VPN 把门(走 `ws://`,无 TLS)。
- 本切片**只下发一次性最新快照**(连上即发),直播(dirty 增量)留 T3b,
  输入回灌(`ClientMsg` → `Session::on_input`,需 `calloop::channel` 反向唤醒)
  留 T3c。

线程必须在 `calloop::Signals::new` **之后** spawn(继承已 block 的 SIGINT/SIGTERM
mask,见 daemon.rs 信号顺序注释);否则 SIGTERM 落到未 block 的 WS 线程 → 默认
terminate → 跳过 socket 清理。

## Alternatives

### Alt 1: `tokio-tungstenite` + tokio runtime(ADR-0015 §5 草图)
- 方案: 引整个 tokio runtime,async/await 重写 IO 线程,单 reactor 管所有连接。
- Reject 主因:
  - **重**:tokio 是 ~20+ transitive crate 的大依赖,本切片单用户 + 客户端个
    位数,每连一个阻塞线程的成本可忽略,用不上 reactor 的并发收益。
  - **`!Send` 推理更绕**:tokio task 默认要求 `Send`,而 `Session` 偏偏 `!Send`。
    虽然 Session 留在 calloop 线程不进 tokio task,但混入 async 让"谁在哪个线程"
    的边界更难一眼看清。同步 + 显式 `std::thread` 边界最直白。
  - ADR-0015 §5 写的「tokio + tungstenite」是 Phase-1 草图,本 ADR 是对该草图的
    **有意偏离**:最小切片用同步版。**退路**:若后续多客户端高并发 / 需要单
    reactor,再升级 tokio(届时另开 ADR 或在本 ADR 增补)。tungstenite 是
    tokio-tungstenite 的底层同步实现,迁移面小。

### Alt 2: 手搓 WebSocket 握手 + 帧(零新依赖)
- 方案: 自写 HTTP upgrade 解析 + `Sec-WebSocket-Accept` SHA1/base64 + 帧
  mask/解码,~150 行。
- Reject 主因:
  - **正确性不该重写**:帧 mask、分片(continuation frame)、控制帧(ping/pong/
    close)、`Sec-WebSocket-Accept` 的 magic GUID —— 写错某些浏览器能连某些不能,
    debug 困难(与 ADR-0005 自写 PNG encoder 同理由)。
  - **SHA1 + base64 仍要依赖**(或自写),省不下多少。
  - tungstenite 是 Rust WebSocket 事实标准、production-tested,正确性来自上游。

### Alt 3: calloop `Generic` 单线程裸注册 `TcpListener`(不起线程)
- 方案: 把 `TcpListener` 当 owned fd 交 calloop `Generic`(它实现 AsFd,与现有
  `UnixListener` 注册同法),在单线程 loop 内做握手 + 写。
- Reject 主因:
  - WS 握手 + 帧解码在单线程 loop 内做,**慢客户端(手机弱网)会 stall 整个
    loop**,违反不变式「loop 不被慢 IO 拖住」。
  - 仍要手搓握手(Alt 2 问题)或在单线程里驱动 tungstenite 的非阻塞状态机,
    复杂度高于直接开线程。
  - ADR-0015 §5 明确 fan-out 走独立 IO 线程,本切片遵循。

### Alt 4: 复用现有 unix socket,浏览器经 websocketd / 代理桥接
- 方案: 不动 daemon,外面挂个 `websocketd` 或 nginx 把 unix socket 桥成 ws。
- Reject: 引入 OS 级外部依赖 + 部署复杂度,与「开箱即用、零仪式」目标相悖;
  且后续 T3c 输入回灌仍需 daemon 原生理解 WS,迟早要做。

## Consequences

### 正面
- **浏览器/手机能连**:T3a spine 打通,手机经 VPN 看 quill 会话第一帧。
- **Send 边界清晰**:只有 `Vec<u8>` 过 `std::sync::mpsc`,`Session` 全程钉在
  calloop 线程,`Rc` 非 Send 约束被结构性遵守(编译器保证)。
- **依赖面小**:同步 tungstenite 不拉 tokio/mio;无 TLS(WireGuard 已加密)。
- **与现有线程模型同构**:completion 子系统早已用 `std::thread` + `mpsc`
  (`completion/bootstrap.rs`),不引新并发范式。
- **unix socket 路径不动**:`quill-dump` + `tests/kernel_daemon_slice.rs` 仍走
  裸 `Snapshot` JSON 行,向后兼容。

### 负面 / 代价
- **Cargo.lock 新增 20 个 transitive crate**(实测:tungstenite / http / httparse /
  sha1 / data-encoding / digest / block-buffer / crypto-common / generic-array /
  typenum / cpufeatures / bytes / rand 链 [rand / rand_chacha / rand_core /
  ppv-lite86 / getrandom / r-efi / wasip2 / wit-bindgen])。**无 tokio / mio** 这种
  大件;均为同步小 crate,审计负担可接受。release 二进制增量小(无 async runtime)。
- **bind `0.0.0.0` 默认**:暴露在所有接口上,安全完全依赖 WireGuard + 主机防火
  墙。daily-drive 单用户可接受;若主机接公网,部署须用 `--ws-bind=10.0.0.2:<port>`
  绑死单接口 + 防火墙只放行 wg 接口(纵深防御,见下「已知残留」)。
- **WS 镜像一个活 shell**:T3c 加输入回灌后等价远程命令执行,安全模型必须靠
  VPN;本切片只下发(只读镜像),回灌前风险有限。

### 已知残留(非本切片 scope)
- **直播 / dirty 增量广播**(T3b):当前只「连上发一次最新快照」,不持续推帧。
- **输入回灌**(T3c):`ClientMsg` 入站需 `calloop::channel` 反向唤醒 calloop 线程。
- **全量 JSON 带宽**:每帧全 cell JSON(ADR-0015 风险条),增量 + bincode 后续。
- **IPv4 vs IPv6**:默认 `0.0.0.0` 只覆盖 IPv4(手机经路由器 v4 端口转发够)。
  若手机走 ADR-0015 抬头的 WireGuard **IPv6 直连**,需 `--ws-bind=[::]:<port>`。
- **bind 默认安全性**:生产部署建议绑具体 VPN 接口地址而非 `0.0.0.0`,本切片
  默认值偏便利,文档/ticket 提示按需收紧。
- **多客户端尺寸冲突**:一个 PTY 一个尺寸,主控端定尺寸策略留后续。

### 不变式说明(INV-1 偏离)
CLAUDE.md 不变式1「所有 IO fd 注册到同一 calloop,绝不起 thread pool 做 IO」约束
的是 **calloop/渲染主循环**。WS 走独立线程做 socket IO 是 ADR-0015 §5 **预先批准**
的例外(IO 线程广播 owned 序列化字节),且 completion 子系统早有 worker 线程。
PTY / unix socket / signal 仍全部留在 calloop 线程,WS 线程只碰 TCP + mpsc。

## 实装验证

- `cargo run --bin quill-kernel -- --ws-bind=0.0.0.0:7878` 起 daemon。
- `assets/web/index.html` 浏览器连 `ws://<host>:7878/`,显示首帧 `row_texts`。
- `tests/kernel_ws_slice.rs`:tungstenite client 连 daemon、收一条 `Snapshot`
  Text 消息、断言 dims + `row_texts` 长度 + SIGTERM 后干净退出。
- 4 门绿(`scripts/ci.sh`:fmt + clippy + build + test)。

## 相关文档

- 主 ADR: `docs/adr/0015-headless-kernel-split.md`(§5 fan-out + Rc 非 Send 头号约束)
- 单 crate 引入 ADR 范本: `docs/adr/0005-image-crate-png-encoding.md`
- 实装: `src/kernel/daemon.rs`(WS 子系统)+ `assets/web/index.html` + `tests/kernel_ws_slice.rs`
- 相关 ADR: 0002(技术栈锁)— tungstenite 不是主干渲染栈,不进 0002 锁清单,仅本 ADR 单点登记
</content>
</invoke>
