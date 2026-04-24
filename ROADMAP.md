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
- [ ] `T-0103` resize 动态重建 wgpu swapchain — **未开**,Phase 3+ 渲染文本时才真有用
- [ ] `T-0104` close 事件优雅退出(wgpu resources 有序释放)— **未开**,当前 Ctrl+C 粗退
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

## Phase 2 — PTY 接入 (3-5 天)

**产出**:窗口打开后 spawn shell,PTY 输出能进 ppoll,还不渲染。

关键 ticket:
- `T-0201` `portable-pty` spawn(优先用 `bash -l`)
- `T-0202` PTY master fd 注册进 `calloop`
- `T-0203` 字节流 `tracing::trace!` 打印出来(不渲染)
- `T-0204` SIGWINCH 正确转发(Phase 1 没 resize 所以先 hardcode 80x24)
- `T-0205` 子进程退出 → 窗口关闭
- `T-0206` 集成测试:spawn `echo hello`,检查 stdout 捕获到

**里程碑**:`cargo run` 后能看见 shell 启动日志,但屏幕还是纯色背景。

---

## Phase 3 — VT 解析 + 屏幕状态 (3-5 天)

**产出**:屏幕上看见 ASCII 字符,哪怕丑。

关键 ticket:
- `T-0301` `alacritty_terminal` 集成,`Term` 对象持有 grid
- `T-0302` PTY bytes → `term.process_byte()`
- `T-0303` 光标位置追踪
- `T-0304` 滚动 buffer 基础
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
- (后续每个阶段起止在这追加)
