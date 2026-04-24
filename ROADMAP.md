# quill Roadmap

分阶段。估时是 **AI 协调节奏**(Lead 每天投入 1-2 小时协调 + 2 写码 并行),不是纯手写估时。

---

## Phase 0 — 脚手架 ✅ (Lead 一次性, 半天)

**已完工** 2026-04-24。完整状态见 git history + CLAUDE.md + tasks/README.md。

---

## Phase 1 — Wayland 窗口 + wgpu 纯色 🟡 5/7 (2026-04-24~25)

**实装验证已通过** 2026-04-24 夜:`cargo run --release` 在 NVIDIA 5090 + Wayland 上打开深蓝窗口,Vulkan backend 自动选中,**没 hang**,不需要 `WGPU_BACKEND=vulkan`。

关键 ticket 状态:
- [x] `T-0101` Wayland 窗口(xdg-toplevel + 占位 wl_shm 白 buffer)✅ merged
- [x] `T-0102` wgpu surface 绑 WlSurface + 深蓝 clear pass ✅ merged
- [ ] `T-0103` resize 动态重建 wgpu swapchain — **推迟到 Phase 3+**, 文本渲染接入才有视觉可验
- [x] `T-0104` close 事件优雅退出(SIGINT/SIGTERM/xdg close 统一路径, ADR 0003 signal-hook + rustix)✅ merged
- [x] `T-0105` `calloop::EventLoop` 骨架(`Core<State>`)✅ merged
- [x] `T-0106` frame stats(tracing 每 60 帧一行)✅ merged
- [x] `T-0107` state machine headless 测试(抽出 WindowCore/WindowEvent/handle_event)✅ merged

**里程碑进度**:`cargo run` 开窗口 ✓。关不崩 ⏳ 等 T-0104。变色 ⏳ 等 Phase 3+。

**不变式** 全部登记到 `docs/invariants.md`(INV-001..INV-007)。审码报告在 `docs/audit/`。

### T-0103 / T-0104 剩余决策(2026-04-25)

Lead 决定:**T-0104 + Phase 2 PTY 并行推进**。理由:
- T-0104 只动 `src/wl/`(close handler + renderer drop)
- Phase 2 新建 `src/pty/`,不撞
- T-0103 暂不做,Phase 3 文本渲染接入时才有视觉可验(当前窗口一块深蓝,resize 看不出差异)

---

## Phase 2 — PTY 接入 (3-5 天) 🟢 可开

**产出**:窗口打开后 spawn shell,PTY 输出能进 ppoll,还不渲染。

关键 ticket:
- `T-0201` `portable-pty` spawn(优先用 `bash -l`)

---

## Phase 2 — PTY 接入 ✅ 6/6 (2026-04-25)

**实装验证已通过** 2026-04-25 晚。PtyHandle 五方法全实装, calloop 接入, 端到端字节通路通, 59 tests 绿。

关键 ticket 状态:
- [x] `T-0201` `portable-pty` spawn (bash -l + O_NONBLOCK + INV-008/009) ✅ merged
- [x] `T-0202` PTY master fd 注册进 `calloop::generic::Generic` (drain stopgap) ✅ merged
- [x] `T-0203` 字节流 tracing (PtyHandle::read + Core<PtyHandle> Data 升级 + escape_ascii) ✅ merged
- [x] `T-0204` PtyHandle::resize API (Phase 2 只开 API, Wayland 接入 Phase 3) ✅ merged
- [x] `T-0205` 子进程退出 → 窗口关闭 (方案 3 Arc<AtomicBool> 复用 T-0104 + POLLHUP bug fix) ✅ merged
- [x] `T-0206` 集成测试 spawn echo hello + drop 清理验证 ✅ merged

**里程碑达成**:`cargo run` 启动窗口 → bash 起 → PTY 字节经 calloop 进 trace → 退 shell / SIGINT / SIGTERM / xdg close 任一都 <1s 干净退, 深蓝窗口不渲染。

**Phase 2 产出**:
- `PtyHandle` 公共 API 完备 (spawn_shell / spawn_program / raw_fd / read / resize / try_wait)
- calloop Generic source 注册 + pump_once 三源 poll (wayland + signal + pty)
- 统一退出机制 `Arc<AtomicBool> should_exit` 从 T-0104 复用, 不新建退出变量
- POLLHUP 检测修复 (Linux pty master slave 关闭 + 缓冲空时只发 POLLHUP 无 POLLIN)
- INV-008 (PtyHandle drop 序) + INV-009 (O_NONBLOCK) 登记到 `docs/invariants.md`
- ADR 0003 (signal-hook + rustix, T-0104 时加)
- 7 份 audit 报告 (phase2-planning / T-0104 / T-0201-T-0206) 归档 `docs/audit/`
- 13 条 tech-debt 登记 `docs/tech-debt.md` (TD-001..TD-013, 主要是 T-0105 refactor 的累积前置)

**Phase 2 里程碑打勾, 进 Phase 3**。

---

## Phase 3 — VT 解析 + 屏幕状态 1/7 (2026-04-25)

**产出**:屏幕上看见 ASCII 字符,哪怕丑。

**实装验证已通过** 2026-04-25: T-0301 完成, `cargo run` 启动后 170ms 内 bash prompt 经 alacritty Processor 解析, cursor 精准停在 `col=17 line=0` (匹配 `[user@userPC ~]$ ` 长度), 证 ANSI/OSC/DECSET 转义正确吞入。

关键 ticket 状态:
- [x] `T-0301` `alacritty_terminal` Term 集成 + PTY → Term grid 端到端通路 ✅ merged (含 T-0108 calloop 统一 refactor)
- [ ] `T-0302` PTY bytes → `term.advance()` 深化 (cursor 行为 / DECSET 模式 / 多字节处理)
- [ ] `T-0303` 光标位置追踪 (cursor shape / blink / position API)
- [ ] `T-0304` 滚动 buffer 基础
- `T-0305` **每 cell 一色块**先渲染(无字体,`█` 式 block 填 cell 背景色)
- `T-0306` resize → term.resize + ioctl TIOCSWINSZ 同步 PTY
- `T-0307` 测试:`ls -la` 输出,检查 grid 里字符位置

**里程碑**:能看见 prompt 和输入回显,虽然字是色块。

---

## Phase 4 — cosmic-text + CJK (3-5 天)

**产出**:真字形渲染,中文英文都对。

关键 ticket:
- `T-0401` `cosmic-text` 初始化 + 字体加载(Noto Sans CJK + 思源黑)
- `T-0402` shaping pipeline: grid cell → glyph run
- `T-0403` glyph 光栅化 → wgpu texture atlas
- `T-0404` 2x HiDPI 整数缩放(`wl_output.scale` 接入)
- `T-0405` CJK fallback 正确(ASCII 用 mono,中文 fallback CJK)
- `T-0406` glyph cache + LRU 驱逐
- `T-0407` 测试:`echo 你好世界 hello` 像素级比对

**里程碑**:能看 `git log` 正常显示中英混排。

---

## Phase 5 — fcitx5 输入法 (5-7 天, 最坑)

**产出**:中文输入能敲,候选框显示正确。

关键 ticket:
- `T-0501` `wayland-scanner` 生成 `text-input-unstable-v3` 绑定
- `T-0502` 绑定 `ZwpTextInputV3` 到主 surface
- `T-0503` preedit string 渲染(下划线风格)
- `T-0504` commit → 转发到 PTY
- `T-0505` `cursor_rectangle` 正确上报(让 fcitx5 候选框定位)
- `T-0506` 焦点切换不丢 IME 状态
- `T-0507` 手测:输中文段落,所有 preedit / 候选 / commit 路径正常

**里程碑**:能在 quill 里用 fcitx5-rime 输中文,Claude Code session 里打字流畅。

**预警**:这阶段文档少,兼容性 bug 多,留 buffer 时间,必要时把 ticket 再拆一遍。

---

## Phase 6 — 打磨 + daily drive (持续 1-2 周)

**产出**:从 Alacritty / Foot 迁到 quill 做主力终端。

关键 ticket:
- `T-0601` soak test 框架:跑满 1h 监控 RSS 不增 >10%
- `T-0602` 内存泄漏排查(heaptrack / valgrind 若适用)
- `T-0603` 键位绑定基础(Ctrl+C / Ctrl+V / 复制粘贴)
- `T-0604` 选择文本 + 鼠标滚动
- `T-0605` 首批 daily drive bug 修复(留半周)
- `T-0606` 配置文件格式(TOML,先支持 font-size / font-family / color)

**里程碑**:`exec quill` 进 login shell,跑 Claude Code 8 小时不闪退。

---

## 总估时

**2-4 周到 daily drive**(理想),**6-8 周**(踩坑保守)。

若 Phase 5 fcitx5 卡住 >10 天,触发应急方案:先放 IME 上线只做英文输入,把 quill 用起来再逐步补。

---

## 决策日志(谁 / 啥时候 / 啥决定)

- 2026-04-24 Lead 启项目,锁 Rust + smithay + wgpu + alacritty_terminal + cosmic-text 栈
- 2026-04-24 Lead 定 Phase 0-6 切分,多 agent 协议写入 CLAUDE.md
- 2026-04-24 Phase 0 完工,git init + 9 文件脚手架
- 2026-04-24 Phase 1 team quill-phase1 首次并行: impl-t0102/0106/0107 + audit
- 2026-04-24 Phase 1 5/7 合 main (T-0101/0102/0105/0106/0107), 首次实装验证窗口打开
- 2026-04-24 docs/invariants.md 建立, INV-001..007 登记硬约束
- 2026-04-25 Lead 决定 T-0104 + Phase 2 并行(src/wl/ 和 src/pty/ 无交集)
- 2026-04-25 T-0103 推迟到 Phase 3+(当前一块深蓝,resize 无视觉可验)
- 2026-04-25 T-0104 完工合并, ADR 0003 signal-hook + rustix, 手写 poll 绕 wayland-client 0.31 的 EINTR 吞咽 bug
- 2026-04-25 Phase 2 team quill-phase2 起, 规划-phase2 一次性交付 6 ticket + pty 骨架后退
- 2026-04-25 Phase 2 全程写码-close + 审码-opus 搭档, 审码中途 Haiku → Opus 1M context 换将
- 2026-04-25 T-0202 写码发现 Level + 不 drain = busy loop, 选 drain stopgap 伏笔 T-0203 替换
- 2026-04-25 T-0203 Core Data 从 `()` 升到 `PtyHandle` (A 方案, TD-001 登记)
- 2026-04-25 T-0205 顺手修 POLLHUP 检测 bug (T-0202/T-0203 残留, EOF 场景漏侦)
- 2026-04-25 Phase 2 6/6 闭环, PtyHandle 五方法全实装, 端到端字节通路通
- 2026-04-25 docs/tech-debt.md 建立, TD-001..TD-013 登记已识别未修风险点
- 2026-04-25 Phase 3 起手: T-0108 + T-0301 合并 ticket (B 方案), 一次清掉 TD-001/005/006 三条技术债
- 2026-04-25 T-0108 refactor: wayland/signal/pty 三源统一进 calloop::EventLoop, LoopData 聚合 + LoopSignal::stop 统一出口, signal-hook + rustix direct dep 删除
- 2026-04-25 T-0301: alacritty_terminal 0.26 + TermState 薄封装 (Term<VoidListener> + Processor), PTY → Term grid 端到端通路通, bash prompt col=17 精准匹配
- 2026-04-25 ADR 0004 建立 (calloop 统一), ADR 0003 Superseded by 0004
- 2026-04-25 TD-001/005/006 三条 ✅ RESOLVED
- (后续每个阶段起止在这追加)
