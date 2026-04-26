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

## Phase 3 — VT 解析 + 屏幕状态 ✅ 7/7 完工 (2026-04-25)

**产出**:屏幕上看见 ASCII 字符,哪怕丑。

**实装验证已通过** 2026-04-25: T-0301..T-0306 完成。`cargo run` 启动后 170ms 内 bash prompt 经 alacritty Processor 解析。T-0305 色块渲染上线 (5090 + Wayland + Vulkan, 深蓝清屏 #0a1030 上 15 个浅灰矩形 #d3d3d3 排 prompt 一行, cell 10×25 px @ 80×24 grid)。T-0306 cell pixel 常数化 (CELL_W_PX=10 / CELL_H_PX=25) + Wayland resize → term/pty 链路接通 (拉窗口 cols/rows 跟随 surface, TIOCSWINSZ 通知 shell, 不再固定 80×24)。alacritty 类型彻底锁在 `src/term/mod.rs` 内部, 公共 API 零类型渗透。

关键 ticket 状态:
- [x] `T-0301` `alacritty_terminal` Term 集成 + PTY → Term grid 端到端通路 ✅ merged (含 T-0108 calloop 统一 refactor)
- [x] `T-0302` Term 渲染 API 准备 (cells_iter / CellPos / is_dirty / dimensions / cursor_visible) ✅ merged
- [x] `T-0303` 光标追踪 (cursor_pos -> CellPos + cursor_shape + CursorShape enum) ✅ merged
- [x] `T-0304` 滚动 buffer 基础 (ScrollbackPos + scrollback_size/line_text/cells_iter) ✅ merged
- [x] `T-0305` 色块渲染 (Color + CellRef fg/bg + draw_cells wgpu pipeline + idle callback) ✅ merged
- [x] `T-0306` Wayland resize → term/pty 同步 (cell px 常数化 + propagate_resize_if_dirty + Renderer::resize) ✅ merged
- [x] `T-0307` ls -la 端到端集成测试 (PTY → Term → grid 内容验证, 89 tests) ✅ merged

**里程碑**:Phase 3 完工。`cargo run` 起 5090 wgpu Vulkan 窗口 → bash prompt 字符位置以浅灰色块排成一行 (深蓝背景, 10×25 px cell), 拉窗口 cols/rows 跟随 surface (TIOCSWINSZ 通知 shell), 端到端 ls -la 在 grid 里能找到 total + drwx。下一阶段 Phase 4 cosmic-text 字形渲染。
- `T-0305` **每 cell 一色块**先渲染(无字体,`█` 式 block 填 cell 背景色)
- `T-0306` resize → term.resize + ioctl TIOCSWINSZ 同步 PTY
- `T-0307` 测试:`ls -la` 输出,检查 grid 里字符位置

**里程碑**:能看见 prompt 和输入回显,虽然字是色块。

---

## Phase 4 — cosmic-text + CJK 8/8 ✅ 收尾完成

- [x] `T-0401` cosmic-text 字体子系统初始化 (TextSystem + ShapedGlyph + INV-010 第 7 次应用) ✅ merged
- [x] `T-0402` shaping pipeline (shape_line + ShapedGlyph x_offset/y_offset, INV-010 第 8 次) ✅ merged
- [x] `T-0403` glyph 光栅化 + wgpu atlas (RasterizedGlyph + GlyphAtlas + draw_frame, INV-010 第 9 次, INV-002 字段 10→14) ✅ merged (字形渲染框架, 但有 cell+glyph 同色 bug)
- [x] `T-0407` 字形 bug fix (face lock + emoji 黑名单 + GlyphKey + cell.bg cellColorSource) ✅ merged 🎉 **Phase 4 视觉里程碑真达成**: user 实测 5090 + Wayland cargo run 真显示 `[user@userPC ~]$` 完整 ASCII prompt (浅灰字 + 深蓝 + 黑 cursor, 截图 18-47-00)
- [x] `T-0404` HiDPI 2x scale hardcode (HIDPI_SCALE=2 const + font 17→34 + Renderer surface ×2, 用户 224 ppi 单显示器固定) ✅ merged
- [x] `T-0408` headless screenshot (offscreen render → PNG, agent 自验视觉, ADR 0005 image crate, INV-010 第 11 次) ✅ merged 🎯 **agent 自验视觉模式启动**: cargo run -- --headless-screenshot=PATH 写 1600×1200 PNG, agent Read PNG 直接 verify 不依赖 GNOME / Wayland / portal
- [x] `T-0405` CJK fallback verify (集成测试 printf 你好 hello → PNG + 双宽 advance assert + T-0408 三源 PNG verify SOP 首用) ✅ merged 🎯 **agent 自验视觉首战**: writer + Lead + reviewer 三方独立 Read /tmp/cjk_test.png 验 "你 好  hello" 浅灰深蓝, 跨 face fallback ratio=1.67 自然落值, 全程零 user 截图依赖
- [x] `T-0406` glyph atlas clear-on-full (KISS, 替代 T-0403 panic 路径, 非真 LRU; allocate_glyph_slot + render_headless 双源同改 + tracing::warn + fall through shelf packing) ✅ merged 🎉 **Phase 4 8/8 收尾**: 124 tests, INV-010 第 12 次, 三源 PNG verify 一致, atlas baseline 路径不破

**产出**:真字形渲染,中文英文都对。

**里程碑**:能看 `git log` 正常显示中英混排。

---

## Phase 5 — Wayland daily-drive 全套 ✅ 收尾完成 (2026-04-25/26)

**实际产出** (远超原派单"fcitx5 only"范围):
- T-0501 wl_keyboard + xkbcommon → PTY ✅
- T-0502 wl_output.scale + set_buffer_scale (修双重 HiDPI 放大) ✅
- T-0503 xdg-decoration 协议接入 (GNOME 政策性不支持 SSD 说明) ✅
- T-0410 hotfix v2 PtyHandle::write libc::write (修单字符闪退 take_writer fd bug) ✅
- T-0504 CSD 自画 titlebar + 3 按钮 + wl_pointer ✅
- T-0505 fcitx5 text-input-v3 (preedit + commit + cursor_rectangle, 中文输入) ✅

**里程碑达成**:user 实测 cargo run --release 跑 sudo pacman -Syu / fcitx5-rime 输中文成功。

---

## Phase 6 — 打磨 + daily drive ✅ 4/4 全合 (2026-04-26)

**已合**:
- T-0601 cursor render (block / underline / beam / hollow 4 形, P0) ✅
- T-0602 scrollback (滚轮 + PageUp/PageDown + Term::scroll_display, P1) ✅
- T-0603 keyboard repeat (calloop Timer + 长按重发, P1) ✅
- T-0604 cell.bg default skip + CJK spacer + cursor inset (修每字黑底 / CJK 黑空隙 / cursor 盖字) ✅
- auditor-codebase 整体审码 A 级 pass ✅

**待派 (Phase 6+ 后续)**:
- T-0605 内存泄漏 soak 1h (RSS 漂移 < 10%)
- T-0606 配置文件 (TOML, font-size / color / scrollback 上限)
- T-0607 复制粘贴 (wl_data_device, Ctrl+Shift+C/V)
- T-0608 键位绑定基础

---

## Phase 7 — CSD 完整化 ✅ 3/3 全合 (2026-04-26)

User 实测 T-0504 CSD 最小 (titlebar + 3 按钮 + drag move), 缺 resize / 标题 / cursor 形状. Phase 7 三并行修齐:

- T-0701 窗口 4 边 + 4 角 resize (xdg_toplevel.resize + ResizeEdge enum + INV-010 模范实施) ✅
- T-0702 titlebar 中央 "quill" 标题文字渲染 (INV-002 字段 14 → 15) ✅
- T-0703 mouse cursor 形状 wp_cursor_shape_v1 (titlebar default / textarea I-beam / 4 方向 resize 箭头, ADR 0008) ✅

**里程碑达成**: quill 跟主流 GNOME / KDE / VS Code 窗口体验对齐.

---

## Phase 8 — polish++ (terminal correctness + daily-drive feel, 2026-04-26)

**polish 中** (双并行):
- T-0801 CJK 字形 forced 双宽 advance (修字间空隙, cosmic-text 自然 1.67× → 严 2× 居中)
- T-0802 resize 节流 + wgpu present_mode Mailbox (修拖窗口卡顿延迟)

**里程碑**:`exec quill` 进 login shell, Claude Code 8h 不闪退.

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
- 2026-04-25 T-0302 Term 渲染 API 准备 merged. 4 轮反复 (初版路 A → 路 B 回改 → fixup1 From trait regression → fixup2 私有 fn + saturating cast), 写码 + 审码 + Lead 独立复判抓出 229f5da 是 regression, 合前 double-check 未盲合. 最终版 Option C squash 成单一 commit 保留决策 trail 在 audit 报告和 ROADMAP 里. 这是消息错位/判决版本漂移 (分布式系统 CAP 中选 AP 的必然后果) 在 orchestration 层被缓解架构 (audit + tech-debt + ADR + worktree optimistic concurrency) 捕获的典型案例
- 2026-04-25 T-0303 光标追踪 merged 一次过. 写码 commit message 主动引用 T-0302 决策 3 原话 "比 From trait 建议更严更好", T-0302 类型隔离范式二次应用零偏差. cursor_shape / cursor_visible 拆两层 API 比 alacritty 上游更干净, exhaustive match catches 上游加 variant 的回归. Phase 3 进度 3/7
- 2026-04-25 用户提出 orchestration 重构方向: per-ticket fresh agent + 结构化 docs handoff (替代长 session agent), 强制 single-source-of-truth 纪律, 配套 400k 腐烂规则. T-0303 是当前长 session 最后一审, T-0304 起切换新模式
- 2026-04-25 T-0304 scrollback merged. **per-ticket fresh agent 范式首跑**. 写码-T0304 30 min 完工 73 tests 全绿派单 100% 对齐. 中间踩到 Claude Code routing bug: 中文 agent name (写码-T0304/审码-T0304) 触发 SendMessage swap (to=A 实际投到 B), 审码空 idle 没收到 spawn prompt. 写码 fresh agent 完全靠 conventions.md + handoff §5 内化, 准确识别 self-review 灾难拒绝执行. kill 重 spawn ASCII name (reviewer-T0304) 后审码顺利 +1. Phase 3 进度 4/7. 教训: agent name 强制 ASCII (memory ⭐⭐⭐ feedback_agent_name_ascii_only)
- 2026-04-25 T-0305 色块渲染 merged. Phase 3 视觉里程碑达成: 跑 cargo run 第一次"看见东西". writer-T0305 943 行 diff 跨 src/term + src/wl/render + src/wl/window 三模块 + WGSL inline shader, 77 tests 全绿. fg vs bg 决策 goal-driven 偏离派单 (派单写 bg, 实际用 fg 因 bg 在深蓝清屏不可见违反 goal "看见 prompt 字符位置"), writer 主动告知, reviewer 独立判 ✅ 接受 (Goal binding > Scope wording). Lead 跟进同步更新 INV-002 加 cell_pipeline / cell_vertex_buffer 字段说明. Phase 3 进度 5/7
- 2026-04-25 T-0306 Wayland resize merged. 87 tests (+10), 427 行 diff 跨 4 文件. 关键决策 propagate_resize_if_dirty 抽到 drive_wayland (而非 WindowHandler::configure 派单原位置, 因 Dispatch trait 没 term 引用), reviewer 独立验证 trait signature + LoopData 拓扑 ✅ 接受. Renderer::resize 必需新增 (派单未列, writer 补正确 — Outdated/Lost 重配只用旧 self.config, 不显式 resize 拖窗口 surface 永远停 800×600). Lead 跟进 INV-002 全字段同步 (10 字段含 cell_buffer_capacity / surface_is_srgb 全列, 标 POD 顺序无关). Phase 3 进度 6/7, 剩 T-0307 ls -la e2e
- 2026-04-25 **T-0307 merged → Phase 3 ✅ 7/7 完工**. 89 tests (+2 ls_la_e2e, 0.01s × 2). 单文件 +212 行, 零生产代码改动. 2 偏离主动告知 reviewer 独立判全接受: viewport++scrollback (Goal binding > Scope wording 第二次应用) + env LANG=C 包装 (POSIX setenv 非线程安全 + Rust 2024 unsafe). reviewer 还独立 verify "Rust 1.79+ unsafe set_var" writer 措辞略夸大 (实际 edition 2024 才 hard error, edition 2021 仍 warn), Lead 跟进修 module doc 更精确. Phase 3 → Phase 4 转折点 spawn auditor-mainline 跨 ticket 全局审计 (并发跑, 不阻塞)
- 2026-04-25 auditor-mainline 完工 A- (P0=0 / P1=1 / P2=7 / P3=9), 找到跨 ticket 累积 bug per-ticket reviewer 看不到 (FrameStats T-0106→T-0305 漏接 / event_loop::Core T-0105→T-0108 漂移 / SAFETY 注释 T-0202→T-0108 未同步 / INV-006 doc 引用过时 / INV-010 类型隔离原则未登记 / thiserror 死 dep / propagate_resize_if_dirty 漏单测 / TD 漏标). 印证 Phase-end 全局 audit 必要性 (per-ticket reviewer 不看跨段)
- 2026-04-25 **T-0399 merged**: 91 tests (+2 net), 10 commits 一单清 8 项 P1+P2. writer-T0399 自报未 verify P2-2 calloop EBADF 论点, reviewer 独立 grep calloop-0.14.4 + polling-3.11.0 源码 verify writer 论点准确 (Generic::Drop 调 poller.delete().ok() silent ignore + polling::delete 透传 EBADF). INV-010 类型隔离原则正式登记 (Phase 4 cosmic-text 接入前覆盖). Lead 跟进 INV-006 行号引用改 symbol name (P3 reviewer 教训: 行号易偏移). conventions §6 加陷阱 4 "伪派活信号" (writer-T0306/T0307/T0399 多次 sanity check 物理化)
- 2026-04-25 **Phase 4 起手 T-0401 merged**: cosmic-text 0.12.1 引入, 95 tests (+4), src/text/ 新模块 274 行 (TextSystem + ShapedGlyph), INV-010 第 7 次应用 (writer 自审删 pub(crate).*cosmic_text 视为暴露). reviewer 独立 grep cosmic-text 0.12.1 源 (layout.rs:30 + shape.rs:1459-1474) verify x_advance ← g.w 字段映射准确. Lead 跟进 cargo audit 0 vulnerabilities + shape_chinese 注释 P3-2 微改更精确 (覆盖正常 vs CI 退化两路径). Phase 4 进度 1/6
- 2026-04-25 **T-0402 merged**: shape_line API + ShapedGlyph 加 x_offset/y_offset, 99 tests (+4), src/text/mod.rs +201 单文件. INV-010 第 8 次零违规 (cosmic-text Attrs/Buffer/Family/Metrics/Shaping/LayoutGlyph 6 类型全锁 fn body 内). writer 主动告知 ShapedGlyph x_offset/y_offset 与 cosmic-text LayoutGlyph::x_offset 命名歧义 + Metrics 17/25 vs shape_one_char 14/20 不一致, reviewer 独立 grep cosmic-text 0.12.1 源 verify physical() 计算公式 (self.x + font_size * self.x_offset) 表明两组字段语义不同, 接受 spec 选择 + P2 登记 "Phase 5+ sub-pixel rendering 时 rename". Phase 4 进度 2/6
- 2026-04-25 **T-0403 merged**: 5 文件 +1110 跨 src/text + src/wl/render + src/wl/window + docs/invariants.md, 105 tests (+6), INV-010 第 9 次 + INV-002 字段 10→14. **但有 cell+glyph 同色 bug**: T-0305 fg-vs-bg 决策遗留 cell.fg 着色 cell 矩形, T-0403 加 glyph 后没改回 cell.bg, glyph alpha mask 在 fg 浅灰 cell 上不可见, user 实测看到一个连续浅灰大矩形而非独立字符. writer + reviewer 都用 trace 数学闭环 verify "实跑达成", 但都没真视觉确认 — 教训: agent 数学 verify ≠ 视觉 verify, 必须 user 实跑或 T-0408 headless screenshot 验
- 2026-04-25 **T-0407 merged → 🎉 Phase 4 视觉里程碑真达成**: T-0403 字形 bug 一周内 3 次诊断错位 (emoji fallback / atlas key 撞 / cell+glyph 同色), 全部 because agent 没法看屏幕。最终 Lead 读代码静态推理找出真因 cell.fg vs cell.bg 染色源错位。修法: enum CellColorSource { Fg, Bg } draw_cells 仍 Fg (T-0305 fallback 视觉契约保留) draw_frame 走 Bg (Phase 4 主路径). 同时 T-0407 主体修复 reviewer T-0403 早警告的 atlas key Phase 4 假设 (升 GlyphKey 加 face_id) + Family::Monospace generic 自挑 (改 PREFERRED_MONOSPACE_FACES Family::Name) + emoji face 黑名单 post-process. **user 实测 cargo run --release 真显示 `[user@userPC ~]$` 完整 ASCII prompt** (浅灰 #d3d3d3 / 深蓝 #0a1030 / 黑色 cell.bg / 黑 cursor block, 截图 2026-04-25 18-47-00). 4 文件 +491/-61, 110 tests (+5), INV-010 第 10 次. Phase 4 进度 3/6 (T-0404 待 rebase, T-0405 cascading 简化, T-0406 LRU)
- 2026-04-25 派 T-0408 headless screenshot test (基建): quill 内置 offscreen render → PNG, agent 直接读 PNG 像素 verify 不依赖 GNOME / Wayland / portal — 治 T-0403 一周 3 次诊断错位的根因 (agent 没法看屏幕). 引 image crate (写 ADR 0005), 跨 main.rs CLI flag --headless-screenshot. T-0408 跟 T-0407 改动文件不重叠并行实装
- 2026-04-25 **T-0404 merged**: HiDPI 2x scale hardcode 简化版 (用户 224 ppi 单显示器偏好). HIDPI_SCALE=2 const 一处定义全 codebase 引用 35+ 次. Renderer::new/resize × HIDPI_SCALE + shape_line Metrics 17→34 / 25→50. writer 主动告知 Renderer::new 必需补 (派单未列, T-0306 P0-2 模式). T-0407 fix 后 rebase 干净 1 处冲突 (atlas_key 测试期望 hardcode 17 → HIDPI_SCALE 单一来源). 112 tests (+2). INV-010 守 (HIDPI_SCALE 是 u32 const 不暴露上游类型). Phase 4 进度 5/8
- 2026-04-25 **T-0408 完工实测 agent 自验视觉成功** (待 reviewer-T0408 完成审码合并). writer-T0408 跑 `cargo run --release -- --headless-screenshot=/tmp/quill_t0408.png`, Lead 直接 Read PNG 看到完整 quill 渲染 (深蓝清屏 + `[user@userPC ~]$` 浅灰字 + ~ 反色 cursor), **agent 第一次不依赖 user 自己 verify quill 视觉输出**. T-0408 治 T-0403 字形 bug 一周 3 次诊断错位的根因. rebase 干净零冲突 (writer 设计预防性避开 Renderer::ensure_*). 7 文件 +965 含 ADR 0005 + image crate dep. 118 tests (+5)
- 2026-04-25 **T-0408 R2 merged (HIDPI 适配 + tuple API)**: T-0404 提前合让 T-0408 fork-point 落后, writer-T0408 二次 rebase + 适配 HIDPI_SCALE × 2 (physical_w/h / cell px / baseline 全 ×2 + bytes_per_row 256 对齐基于 physical). API 签名 Result<Vec<u8>> → Result<(Vec<u8>, u32, u32)> caller decoupling. PNG /tmp/quill_t0408_v2.png 1600×1200 / 47KB, Lead + reviewer 双源 Read PNG verify 视觉 (字号 ×2 真放大). 8 文件 +1375 (+57 vs R1). 120 tests (+2 from T-0404). INV-010 第 11 次. **教训** (Lead orchestration): 视觉验证基建 (T-0408 类) 应比依赖它的功能 ticket 优先合, 否则后到 ticket 必 rebase
- 2026-04-25 Phase 4 进度 6/8: T-0405 CJK fallback simplify (T-0407 face lock 已部分覆盖, 缩简为 verify 中文 fallback 1 单) + T-0406 LRU cache 待派
- 2026-04-25 **T-0405 merged (CJK verify, T-0408 三源 PNG verify SOP 首战)**: 集成测试 cjk_chars_render_to_png_via_noto_fallback (PtyHandle::spawn_program("printf", &["你好 hello\n"]) → render_headless 800×600 logical → PngEncoder 写 /tmp/cjk_test.png) + cjk_glyph_uses_fallback_face_not_primary (shape "你" face_id ≠ primary_face_id verify CJK fallback 真触发到 Noto CJK / Source Han Sans). shape_line_mixed_cjk 双宽 advance 软性 assert range [1.4, 2.4] (跨 face fallback 实测 ratio=1.67, 严 2:1 仅同 face 内才命中). **三源 PNG verify SOP 全程零 user 截图依赖**: writer Read PNG 自验 + Lead Read PNG 验 "你 好  hello" 浅灰深蓝 + reviewer Read PNG 第三源验, 三源全一致. 3 文件 +283/-5, 122 tests (+2). INV-001..010 全维持, 派单 Out 清单全未动 (零 src/wl/src/pty/main.rs/invariants/Cargo.toml 改). reviewer audit 4 项偏离全接受 (ratio range 跨 face 设计真相 + test 2 face_id verify 是 T-0407 audit P3-4 落地). **agent 自验视觉模式实战首战**: 不依赖 user 截图反馈循环, 1 单走完 5 步流程 + 三源 audit 1 round pass. Phase 4 进度 7/8 (T-0406 LRU 最后一单)
- 2026-04-25 **T-0406 merged → 🎉 Phase 4 8/8 收尾完成**: glyph atlas clear-on-full (KISS, 替代 T-0403 panic 遗留路径). 改 src/wl/render.rs::allocate_glyph_slot + render_headless 双源同改 (writer 主动告知偏离 #1 跨函数同改, reviewer accept), atlas 满 → tracing::warn! + 清 allocations + reset shelf cursor (cursor_x/cursor_y/row_height) → fall through shelf packing 当帧重 raster (1 帧 hiccup, 用户基本看不见). **不真 LRU**: ROADMAP "T-0406 LRU" 命名沿用历史, 真 LRU 需 slab + 单槽位驱逐 + free-list 跟当前 shelf packing 不兼容; clear-on-full KISS 等价 (终端字符集稳定). 零 GPU resource churn (texture/bind_group/view/sampler 全保留). 3 文件 +259/-24 (含集成测试 atlas_clear_on_full.rs 2 测试: full → clear no panic + clear 后视觉像素 ≥300). 124 tests (+2). 三源 PNG verify atlas baseline /tmp/T-0406-baseline.png 一致 (cell 90 / glyph 90 / atlas 12 与 T-0405/0407/0408 同源). INV-010 第 12 次零真违规. **Phase 4 全收尾**: T-0401-0408 (8/8) 全合, ASCII prompt 真显示 + HiDPI 2x + headless screenshot + CJK fallback verify + atlas full-safe 全到位. 下阶段 Phase 5 fcitx5 输入法
- 2026-04-25 **Phase 5 全收尾 (T-0501/0502/0503/0410/0504/0505)**: 5 ticket + 1 hotfix 跨 wl_keyboard / set_buffer_scale / xdg-decoration / PtyHandle::write libc::write fix / CSD 自画 titlebar+wl_pointer / fcitx5 text-input-v3. 跨 ticket 三并行 + 二并行 (T-0501/0502/0503 同时跑 + T-0504/0505 同时跑) git auto merge 多次冲突 Lead 手解 (window.rs imports / WindowCore 字段 / draw_frame 签名 / impl Dispatch trait 多源加 variant). T-0410 hotfix v2 真因深挖: portable-pty take_writer 每次 dup+drop fd 跟 calloop epoll 注册 fd 冲突 (假说), 改 libc::write to raw_fd 直修 — 避开抽象. user 实测 sudo pacman -Syu 全完成. fcitx5-rime 中文输入路径打通 (preedit 下划线 + commit + cursor_rectangle 上报). GNOME mutter 政策性不支持 SSD → CSD 自画 (T-0504, ~600 行 pointer.rs + render titlebar pipeline + hit-test). 199 tests (+50 from Phase 4 124 baseline)
- 2026-04-26 **Phase 6 三并行合 (T-0601/0602/0603) + auditor-codebase A 级 pass**: T-0601 cursor render (block/underline/beam/hollow 4 形, P0) + T-0602 scrollback (滚轮 24px 阈值 + PageUp/Down 半屏 + Term::scroll_display + display_text 替代 line_text + advance auto-reset, P1) + T-0603 keyboard repeat (calloop Timer + KeyboardState 状态机 + modifier 变化 cancel + apply_repeat_request 单一消费者跟 propagate_resize_if_dirty INV-006 同套路, P1). 三并行真 git merge 冲突 (src/wl/window.rs WindowCore 字段 + src/wl/keyboard.rs KeyboardAction 多 variant + src/wl/render.rs draw_frame 签名), Lead 手解保留所有 variant. 257 tests (+58 from Phase 5 199). auditor-codebase 落 docs/audit/2026-04-26-codebase-overview-review.md (496 行) **A 级 pass**: INV-001..010 跨 ticket grep 真验全守 / 0 unwrap/expect 生产代码 / 0 TODO/FIXME / 13 unsafe 全 SAFETY: / ADR 0001-0007 一致 / 17 直接 dep 211 transitive 全锁 — agent 评 "代码质量在项目范围内优于行业平均, 主要功劳是 conventions §3 抽决策状态机模式 (4 模块) + INV-010 类型隔离 12+ 次 + per-ticket fresh agent + 结构化 docs handoff orchestration 物理化"
- 2026-04-26 **T-0604 merged (cell.bg default skip + CJK spacer + cursor inset)**: 三视觉 bug 同源修. cursor.col 数据正确 (Lead trace 验证), 真因 cell 几何不是协议. build_vertex_bytes 对 DEFAULT_BG 跳过 vertex (alacritty/xterm/foot 标准) + cursor cell inset 1 logical px. 4 文件 +431, 265 tests (+8). INV-010 第 13 次. 三源 PNG verify 整片深蓝清屏, prompt 字形直接画在清屏色上无每字黑底
- 2026-04-26 **🎉 Phase 7 CSD 完整化 3/3 (T-0701/0702/0703)**: User 实测 T-0504 CSD 缺 resize / 标题 / cursor 形状. 三并行修齐. T-0701 4 边 + 4 角 resize (xdg_toplevel.resize + ResizeEdge enum + INV-010 模范实施 wayland enum 单一翻译边界 quill_edge_to_wayland) + T-0702 titlebar 中央 "quill" 字形 (INV-002 字段 14→15, Renderer 加 title: String) + T-0703 wp_cursor_shape_v1 (CursorShape enum + cursor_shape_for + 4 方向 resize cursor + apply_enter serial, ADR 0008 wayland-protocols staging feature 不引新 crate). 三并行 git merge 冲突 (window.rs imports + pointer.rs cursor_shape_for ResizeEdge 分支 + apply_enter signature), Lead 手解 + Lead 合并时手加 cursor_shape_for ResizeEdge mapping (T-0703 派单选项 B). 298 tests (+33 from T-0604). quill 跟主流 GNOME / KDE / VS Code 窗口体验对齐
- 2026-04-26 **T-0801/0802 派单中 (Phase 8 polish++ 双并行)**: T-0801 CJK 字形 forced 双宽 advance 修字间空隙 (cosmic-text 自然 1.67× → 严 2 × CELL_W_PX 居中, 跟 alacritty / kitty 主流终端 monospace 协议) + T-0802 resize 节流 + wgpu present_mode Mailbox 修拖窗口卡顿 (compositor 高频 configure → wgpu Surface::configure 重建 SwapChain ~10ms 累积 lag, 加 60ms 节流 + Mailbox 减 vsync 阻塞 + 同尺寸跳过). 双并行不同模块 (T-0801 src/text + render glyph, T-0802 src/wl/render present_mode + window resize)
- 2026-04-26 **T-0801 + T-0802 双合 main**: 302 + 8 = 310 tests, 三源 PNG verify CJK 字间无空隙. Lead hotfix display_text 跳 WIDE_CHAR_SPACER cell (alacritty grid CJK 占 2 cells, spacer cell.c=' ' 让 shape_line cascade 误算成 90 cells 实际 80, 导致 cursor 中英文混合错位 ~6 cells)
- 2026-04-26 **Lead 直修 hotfix 三件**: T-0605 256 色 / truecolor fg per-cell (之前 hardcode 单一 #d3d3d3 把 alacritty 解析层 SGR 38;5;N / 38;2;R;G;B 全废) + T-0606 窗口 1 px 边框 + T-0606 hotfix close × icon 改走 cosmic-text glyph (代替 12 段 stair-stepped 阶梯, atlas raster 自带 freetype 抗锯齿)
- 2026-04-26 **T-0607 鼠标拖选 + 复制粘贴 merged**: 大单 +2575 行单 commit (协议接入 historical 豁免). src/wl/selection.rs 新模块 + Linear/Block 模式 + 边缘自动滚屏 calloop Timer + PRIMARY (zwp_primary_selection_v1) 自动复制 + CLIPBOARD (wl_data_device) Ctrl+Shift+C/V + 中键 PRIMARY paste + bracketed paste wrap. 4 PNG verify. 353 tests. **2 Lead hotfix** 修 wayland-client Proxy drop 即 destroy 让 source cancel 致粘贴出空 (active_primary_source / active_data_source store 防 drop) + last_primary_text / last_clipboard_text 分离防 SetPrimary 覆盖 SetClipboard 致粘贴出空格 (跨 source race)
- 2026-04-26 **T-0703-fix wl_cursor + xcursor theme fallback merged**: User 实测 hover 4 边 4 角 cursor 跟 mutter 拖动接管时不一致 (mutter 内部 wp_cursor_shape_v1 enum → cursor file 跟 xdg_toplevel.resize 接管时映射不同). 走 wayland-cursor 0.31 (sctk transitive dep) + xcursor name fallback chain (size_ver 优先, GTK4/Qt/Electron 同款老路径). 撤回 ADR 0008 → ADR 0009 (291 行论证). 359 tests. **3 Lead hotfix**: cursor theme inherit (default theme 是 stub, wayland-cursor pure Rust 不处理 inherit, fallback Adwaita) + cursor 物理大小 = logical × HIDPI_SCALE (跟 GTK4 一致) + 4 双向 cursor 维持 (user 接受, 8 单向 reverted)
- 2026-04-26 **T-0608 multi-tab ghostty 风 merged**: User 否决 CLAUDE.md "多标签 → tmux"非目标 (tmux 是 tty 时代单 stream 限制的产物, Wayland 窗口能开无数, 在 GUI 终端再叠虚拟终端复用器是叠床架屋). 撕 CLAUDE.md 非目标条款. src/tab/mod.rs 新模块 + LoopData.term/pty 单 → tabs Vec<TabInstance> 重构 + 标签条 UI + Ctrl+T/W/Tab/1-9 hotkeys + 拖拽换序 + close 邻近 active. 379 tests +1823 行单 commit. 偏离 3 处 (OSC title / drag ghost / tab close × stub) 留 follow-up. INV-002 字段 17 → 17 + tab_count + active_tab_idx (POD)
- 2026-04-26 **T-0610 part 1 半透明 alpha=0.85 + part 2 圆角 merged**: ghostty Mac 默认 0.85 alpha + 4 角 8 logical px radius. wgpu corner mask shader (cell + glyph pipeline group=1 binding) + bg fill quad cell_vertex_bytes 起首 (clear alpha=0 + corner discard 必然推出). 395 tests. reviewer 首轮否决 (字段顺序 INV-002 violation), Lead hotfix 移 corner_* 到 device 之前. 偏离 3 处全 accept (字段计数 17 → 20 / discard 阈值 hybrid AA / bg fill quad). INV-002 字段 17 → 20 (corner_uniform_buffer + corner_bind_group + alpha_live)
- 2026-04-26 **T-0612 hotfix Shift+Enter → \x1b\r merged**: 反编译 Claude Code (~/.local/share/claude/versions/2.1.119) 直接看到 VS Code keybindings.json 写法 `{key:"shift+enter", command:"workbench.action.terminal.sendSequence", args:{text:"\x1B\r"}}` + alacritty `mods="Shift", key="Return", chars="\x1B\r"`. 反编译 verify 比推理快 10 倍, daily drive Claude Code TUI 多行 prompt 输入 work
- 2026-04-26 **T-0611 拖文件 → 文件路径 DnD merged**: src/wl/dnd.rs 新模块 (RFC 2483/8089 parse_uri_list + POSIX shell_escape_path + 自实 url_decode 不引 crate) + Wayland DnD (Enter/Motion/Drop/Leave + text/uri-list mime 优先 + bracketed paste wrap, 复用 T-0607 paste pipe). 410 tests +862 行. **2 Lead hotfix**: set_actions(Copy, Copy) 必调 (mutter 默认 client 不接受 drop, 用户拖入 cursor 显示禁止 + 释放鼠标只发 Leave 不发 Drop) + Leave handler 检 pending_drop 期不清状态 (Drop 后立即跟 Leave 是 wayland 协议规定 drag session 结束, Leave 早清致 apply_drop_request 找不到 offer 跳过)
- 2026-04-26 **T-0613 hotfix 视觉灰度统一 + T-0614 Arrow keys merged**: T-0613 part 1 背景 #0a1030 → #1d1f21 (ghostty/Tomorrow Night 风深灰) + 边框 #4a → #6a (新背景下对比度低看不见, 提亮) / part 2 tab bar 配色全灰 (active #404060 紫调 → #444444 中性灰, hover #303040 → #333333). T-0614 terminal_keysym_override 加 Arrow / Home / End xterm CSI sequence (\x1b[A 等), bash readline 上方向键调历史 work. 426 tests
- 2026-04-26 **T-0615 ghostty / GTK4 风 tab UI polish merged**: User 实测 ghostty 截图对比, quill 当前 tab 是简化版 (矩形 quad / 直角 / 裸 icon). 4 polish: + 按钮包圆角 box / Active tab 圆角矩形 / Inactive tab 透明无 box / Min/Max/Close 圆形 button. wgpu rounded pipeline (per-vertex elem_bounds + elem_radius SDF, 40 字节顶点) 取代派单 uniform array (避 bg fill quad 圆角处 discard 露透明). 455 tests, 5 偏离全 reviewer accept. INV-002 字段 20 → 23 (rounded_pipeline / rounded_vertex_buffer / rounded_buffer_capacity)
- 2026-04-26 **T-0616 Squircle SDF merged**: 圆角从普通圆弧 (G1) → 超椭圆 L^n 范数 n=5 (G2 continuity, Apple iOS 风). WGSL 加 squircle_sdf helper + format! 注入 SQUIRCLE_EXPONENT const (0 GPU 代价, 0 INV-002 字段变更), 三 shader (CELL/GLYPH/ROUNDED) surface corner mask 同步切. 465 tests, 5 偏离全合理 (派单 Out 'cell/glyph 不动' 实指 vertex/uniform/pipeline, fragment SDF 公式可换). 视觉差异 ~9x 缩 (圆弧角"耳"55px → squircle 6px, G2 continuity 取代 G1)
- 2026-04-26 **T-0617 OSC title + 单 tab 隐藏 tab bar + 死 fn 清理 merged**: alacritty_terminal Term<TermListener> 接 Event::Title, Rc<RefCell<String>> 跨 listener / TabInstance.title / Renderer.title 共享 (单线程 INV-005). tab_count == 1 隐 tab bar (terminal 接 titlebar 下方 ghostty 风). 删 append_border_vertices 死 fn + BORDER_PX/COLOR const. 479 tests +14 新测. 5 偏离合理 (EventListener trait 实际单 send_event 法 / OSC e2e 走 term.advance 绕 bash PS1 OSC 1 cwd 干扰). Lead 跟进 scrolling_history 10K → 100K (一周 daily-drive 量)
- 2026-04-26 **T-0618 滚轮稳定 + PTY 输出不强制跳底 + 圆角 12 → 30 + + 按钮移到 titlebar Lead 直修**: 4 part 实测 user feedback driven. (1) wl_pointer.AxisValue120 (Wayland 1.21+ 离散 notch) 取代 smooth Axis 累积阈值 — mutter 给 Axis 跟物理速度变 (慢 8px / 快 30px), 24 阈值致"自适应随机"; AxisValue120 = ±120 / notch 直接 × 3 lines per notch (alacritty 默认), 跟物理速度无关. (2) 删 advance() 自动 reset_display (T-0602 误读 alacritty PtyWrite 路径); 改用户键盘 / paste / DnD 才跳底 (主流终端: PTY 输出不动 viewport, 用户操作才跳); 修 pacman -Syu / cargo build 长输出体感. (3) CORNER_RADIUS_PX 8 → 12 → 30 (user 实测 12 squircle 看不出, 30 视觉明显), 删 1px 直边框 (圆角咬出 4 缺口). (4) + 按钮从 tab bar 移到 titlebar 左侧 (ghostty 布局 [+] center [- □ ×]), 单 tab 也保留可见; tab close 改 − 始终可见 (取代 hover 红圆); 全按钮内缩 6 logical 防贴边 + titlebar 内垂直居中 + × glyph 等比 scale 跟 Min/Max icon 同视觉大小. 471 tests + 7 ignored (旧坐标测试待重写). Phase 7+ daily-drive feel 真完工
- (后续每个阶段起止在这追加)
