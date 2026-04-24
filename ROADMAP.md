# quill Roadmap

分阶段。估时是 **AI orchestration 节奏**(Lead 每天投入 1-2 小时协调 + 2 implementer 并行),不是纯手写估时。

---

## Phase 0 — 脚手架 (Lead 一次性, 半天)

**产出**:一个能 `cargo build` 通过的空骨架,后续 teammate 能无阻碍开工。

- [x] `CLAUDE.md`(项目宪法 + 多 agent 协议)
- [x] `ROADMAP.md`(本文件)
- [ ] `Cargo.toml`(crate 元数据 + 空依赖列表)
- [ ] `src/main.rs`(stub,`fn main() { tracing init; 打印 hello }`)
- [ ] `.gitignore`(target/ + IDE 垃圾 + .DS_Store 防 Mac)
- [ ] `rust-toolchain.toml`(锁 stable 版本)
- [ ] `docs/adr/0001-language-rust.md`(第一个 ADR:为啥 Rust 不 Zig/C)
- [ ] `docs/adr/0002-stack-lock.md`(锁死 smithay + wgpu + alacritty_terminal + cosmic-text)
- [ ] `tasks/README.md`(任务队列目录说明 + ticket 模板)
- [ ] `git init` + 初始 commit

**不做**:依赖实装(等 Phase 1 Implementer 决定 crate 版本)、CI(等有代码再说)。

---

## Phase 1 — Wayland 窗口 + wgpu 纯色 (3-5 天, 2 implementer 并行)

**产出**:能打开一个 Wayland 窗口,wgpu 渲染纯色背景,resize / close 正确。

关键 ticket:
- `T-0101` smithay-client-toolkit 接入,`WlSurface` 创建
- `T-0102` wgpu surface 绑 `WlSurface`,clear color 每帧
- `T-0103` resize 事件 → 调整 wgpu 配置,不丢帧
- `T-0104` close 事件优雅退出,资源正确 drop
- `T-0105` `calloop::EventLoop` 骨架,所有事件走这一个 loop
- `T-0106` `tracing` 输出 frame stats(每 60 帧一行)
- `T-0107` 单元测试:headless 模拟 Wayland event,验证 state machine

**里程碑**:`cargo run` 开一个会变色的窗口,关不崩。

**潜在坑**:wgpu 初始化在 NVIDIA Wayland 有时 hang,准备好 `WGPU_BACKEND=vulkan` / `gl` 两条退路。

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
- (后续每个阶段起止在这追加)
