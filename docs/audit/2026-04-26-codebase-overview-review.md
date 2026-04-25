# Codebase 整体审计报告

日期: 2026-04-26
auditor: auditor-codebase (Opus)
范围: Phase 1-5 全合 + 4 hotfix, 13588 LOC (src+tests), 13 ticket-level audit 累积
main HEAD: `7046851` (T-0601/0602/0603 派单, 写码 worktree 跑中, 主干静止)

## 总览

- **生产代码 LOC**: 10684 (src/) — 8 个模块文件
- **测试 LOC**: 2904 (tests/) — 19 个集成测试文件
- **总 LOC**: 13588
- **测试**: 199 tests pass (release), `cargo clippy --all-targets --release -- -D warnings` 全绿
- **依赖**: 17 直接 dep (Cargo.toml), 211 transitive (cargo audit 0 CVE — 上次扫描 T-0399)
- **INV-001..010**: ✅ 全部维持 (跨 ticket 真验, 非自报)
- **ADR 0001-0007**: ✅ 一致, 0003 已被 0004 Supersede
- **生产代码 unwrap/expect**: 0 (CLAUDE.md 硬约束严守, 仅 #[cfg(test)] 块和 main.rs 的 `unwrap_or_else` fallback)
- **TODO/FIXME/XXX/HACK**: 0 (CLAUDE.md "禁止以后再说 TODO" 严守)
- **类型隔离**: src/lib.rs / src/main.rs / src/frame_stats.rs **零** 上游类型外漏 (grep verify)

模块拓扑:
```
src/lib.rs          — pub mod 入口 (frame_stats / ime / pty / term / text / wl)
src/main.rs         — CLI 入口 + headless screenshot 路径 (225 LOC)
src/frame_stats.rs  — Phase 6 soak 采集点 (214 LOC)
src/wl/             — Wayland 客户端 (mod.rs/window.rs/render.rs/keyboard.rs/pointer.rs)
  └ mod.rs (32) / window.rs (2152) / render.rs (2957) / keyboard.rs (614) / pointer.rs (590)
src/pty/mod.rs      — PTY + 子进程 (462 LOC)
src/term/mod.rs     — alacritty_terminal 封装 (1458 LOC)
src/text/mod.rs     — cosmic-text 封装 (1240 LOC)
src/ime/mod.rs      — fcitx5 IME (text-input-v3) (731 LOC)
```

---

## A. INV-001..010 跨 ticket verify

### INV-001: State 字段顺序 (renderer→window→conn) ✅

`src/wl/window.rs::State` (1021-1080) 字段顺序:
```
registry_state / output_state / renderer / window / conn / core / seat_state /
keyboard_state / keyboard / pointer_state / pointer / pointer_seat / is_maximized /
presentation_dirty / text_input_manager / text_input / ime_state / pty
```

renderer (line 1030) → window (1031) → conn (1032) ✅ INV-001 链条严守。
T-0501 加 keyboard / T-0504 加 pointer / T-0505 加 IME 字段, 都在 conn 之后, **不破链条**。
注释 1024-1029 重述 INV-001 理由, 24/26 字符 doc 引用 docs/invariants.md, 后续维护者读 struct 即看见约束。

### INV-002: Renderer 字段 (14 字段) drop 顺序 ✅

`src/wl/render.rs::Renderer` (62-106) 字段顺序与 docs/invariants.md INV-002 完全一致:
surface → cell_pipeline → cell_vertex_buffer → cell_buffer_capacity → glyph_atlas → glyph_pipeline → glyph_vertex_buffer → glyph_buffer_capacity → device → queue → config → clear → surface_is_srgb → instance.

GlyphAtlas 内部字段 (134-162): bind_group → bind_group_layout → view → sampler → texture → allocations/cursor/row_height. ✅ 与 INV-002 GlyphAtlas 段一致。

`#[allow(dead_code)]` 标在 view / sampler 字段是**显式持资源 + drop 顺序明确**意图, 不是死代码 (注释 143-149 解释)。

### INV-003: `unsafe fn Renderer::new` 调用方合同 ✅

`src/wl/window.rs:1086-1117::init_renderer_and_draw` 是唯一调用点, null check + INV-001 字段顺序保证生命周期。SAFETY 注释 (1103-1107) 引 `docs/invariants.md`。

### INV-004: `#![deny(unsafe_code)]` ✅

`src/lib.rs:2` + `src/main.rs:4` 都用 `#![deny(unsafe_code)]` (非 forbid)。
`grep '^#!\[forbid\(unsafe_code\)\]' src/` 零命中 ✅

### INV-005: calloop 单线程禁阻塞 ✅

生产路径 `grep -rn "thread::spawn\|std::thread" src/` 唯一命中:
- `src/pty/mod.rs:406` — **#[cfg(test)] 块** 内 (`read_captures_echo_hi_output` 测试 sleep)
- `src/main.rs:107` — `run_headless_screenshot` 路径 (注释 80-82 显式说"headless 路径允许阻塞, 不挂 EventLoop")

主路径 `run_window` + `drive_wayland` + `pty_read_tick` 全 calloop, 无 thread::spawn / tokio。INV-005 严守 ✅

### INV-006: resize_dirty 单一消费者 ✅

唯一消费者 `src/wl/window.rs::propagate_resize_if_dirty` (534-577), 由 `drive_wayland` 在 dispatch_pending 之后调一次 (line 632)。`resize_dirty = false` 在 line 569 显式清零。

T-0306 起 init 路径不再清零 (注释 1334-1342 说明), 消费者单一性达成。INV-006 doc 已用 symbol name `propagate_resize_if_dirty` 引用 (T-0399 P3 修)。

`presentation_dirty` (T-0504) 是**独立**布尔脏标记, 不是 INV-006 的别名 — CSD hover 重画用, 与 cell content 解耦, 不破 INV-006。

### INV-007: WindowCore 纯逻辑 ✅

`handle_event(state: &mut WindowCore, ev: WindowEvent) -> WindowAction` (146-176) 只改 WindowCore 字段, 无 IO / Wayland request / GPU。`tests/state_machine.rs` 11 单测覆盖 ✅

### INV-008: PtyHandle drop 顺序 ✅

`src/pty/mod.rs::PtyHandle` (44-55) 字段: reader → master → child → master_fd (RawFd POD 无 Drop)。
模块 doc (line 8-12) + struct doc 重述 INV-008 理由。

### INV-009: master fd O_NONBLOCK ✅

`set_nonblocking` (line 260-283) 在 `spawn_program` (line 109) 调用, **返回 PtyHandle 之前**。
`pty_read_tick` (window.rs:313-323) debug_assertions block 跑 fcntl(F_GETFL) sanity check (TD-008 已识别 release build 无验证, 接受)。
单测 `master_fd_is_nonblocking_after_spawn` (pty/mod.rs:332-350) 锁此契约 ✅

### INV-010: 类型隔离 ✅

跨模块 grep verify:
- `grep -E "alacritty_terminal::|wgpu::|cosmic_text::|wayland_protocols::|xkbcommon::" src/lib.rs src/main.rs src/frame_stats.rs` — **零命中**
- `pub use` 仅 src/wl/mod.rs 5 行 — 全是 quill 自有类型 (run_window / handle_event / WindowCore / WindowEvent / WindowAction / HIDPI_SCALE / render_headless / PreeditOverlay / PREEDIT_UNDERLINE_PX), 无上游类型 re-export ✅
- `impl From<上游类型> for ...` 跨 src/ — 零命中, 全部走模块私有 `from_<crate>` inherent fn ✅

集成测试唯一边界例外:
- `tests/ime_e2e.rs` 用 `wayland_protocols::wp::text_input::zv3::client::zwp_text_input_v3` 直接构造协议 event 测 `handle_text_input_event` — Dispatch trait 必须传协议类型, 这是合理边界 (handle_text_input_event 公共 API 入参就是协议类型, 是 quill 公共 API 的一部分)
- `tests/atlas_clear_on_full.rs` 仅在**注释里**提 wgpu 类型 (说明 INV-010 禁外露), 实际未 import

12 次 INV-010 应用 (T-0302..T-0408) 零真违规, 严守。

---

## B. ADR 0001-0007 实装一致性

### ADR 0001 (Rust language) ✅

`rust-toolchain.toml` 锁 stable, `cargo build --release` 全绿。

### ADR 0002 (技术栈锁定) ✅

Cargo.toml 17 直接 dep 完全对齐 ADR 0002 决策表:
- smithay-client-toolkit 0.19 ✅
- calloop 0.14 + features=["signals"] ✅ (signals feature 是 ADR 0004 接 signalfd 必需)
- wgpu 29 ✅
- alacritty_terminal 0.26 ✅
- cosmic-text 0.12 ✅
- portable-pty 0.9 ✅
- tracing 0.1 + tracing-subscriber 0.3 ✅

ADR 0002 未列的新增 dep 都有 ADR (0003 signal-hook 已 superseded / 0005 image / 0006 xkbcommon / 0007 wayland-protocols)。

### ADR 0003 (signal-hook + rustix) — Superseded ✅

`grep -rn "signal_hook\|rustix" src/ Cargo.toml` 零命中 ✅
ADR 文档 Status 标 `Superseded by ADR 0004`。dep 已删 (T-0108 refactor)。

### ADR 0004 (calloop 三源统一) ✅

`run_window` (window.rs:657-1019) 把 wayland fd / Signals (SIGINT+SIGTERM) / pty fd 三源全注册进同一 EventLoop。LoopData 聚合 event_queue / state / term / loop_signal / frame_stats / text_system 6 字段 (ADR 0004 写 4 字段是当时设计, T-0399/0403 后续加 frame_stats / text_system 是合理增长, 不破 ADR 决策)。

### ADR 0005 (image crate PNG) ✅

`Cargo.toml:15` `image = "0.25"`, `default-features = false, features = ["png"]` 与 ADR 一致。
`src/main.rs:84-87` 用 `image::codecs::png::PngEncoder` + `image::ExtendedColorType` + `image::ImageEncoder` 走 ADR 接口 ✅

### ADR 0006 (xkbcommon) ✅

`Cargo.toml:37` `xkbcommon = "0.8"` 与 ADR 一致。`src/wl/keyboard.rs:48` `use xkbcommon::xkb;` 锁本模块, 不外漏 (INV-010 同时验)。

### ADR 0007 (wayland-protocols text-input-v3) ✅

`Cargo.toml:31` `wayland-protocols = "0.32"` features=["client", "unstable"] 与 ADR 一致。
src/ime/mod.rs + src/wl/window.rs 实装 TIv3 atomic state machine + enable/disable 协议要求, 与 ADR 设计完全一致。

---

## C. 模块边界 (mod 私有 vs pub re-export)

`src/lib.rs:1-9` 仅声明 6 个 `pub mod`, 无 `pub use`. 干净。

`src/wl/mod.rs:1-32` 5 处 `pub use` 全 quill 自有类型 (无上游漏出, INV-010 验过)。

src/wl/ 内部模块 (keyboard / pointer / render / window) 全 `mod` (私有), 仅通过 `super::xxx` 跨模块引用。`render` 内部 `GlyphAtlas` / `AtlasSlot` 也是模块私有 struct, 不漏出 src/wl/ 边界 ✅

src/term/ src/text/ src/pty/ src/ime/ src/frame_stats/ 都是单文件 `mod.rs`, 公共 API 直接 `pub`, 无内部子模块。模块边界清晰。

**未发现过度 re-export 或细节漏出**。

---

## D. unsafe 块 SAFETY: 注释完整性

跨 src/ 13 个 unsafe 块 (4 块在 src/pty/mod.rs / 4 在 src/wl/window.rs / 3 在 src/wl/render.rs / 2 在 src/wl/keyboard.rs):

每块前面都有 `// SAFETY:` 注释 + `#[allow(unsafe_code)]` 放行 ✅

抽样精读:
- **`src/pty/mod.rs::set_nonblocking`** (line 260-283): 4 点风格 (fd 来源 / fd 活性 / syscall 副作用 / 返回值检查) — conventions.md §2 标杆
- **`src/wl/render.rs::Renderer::new`** (676-721): 1 行 # Safety doc + SAFETY block 注释引 INV-001 字段顺序
- **`src/wl/keyboard.rs::load_keymap_from_fd`** (382-428): 5 点 (fd 活性 / mmap flags / size 信任 / munmap defer / 借用不外泄)
- **`src/wl/window.rs:798-801` (wayland_fd) + `:839` (pty_borrowed)**: T-0399 housekeeping 已修文字描述 drop 序 (TD-017 RESOLVED), 引 calloop 0.14 EBADF 容忍, 严格安全 ✅

无 SAFETY 注释缺失 / 文字与代码不匹配的回归 ✅

---

## E. 错误处理一致 (anyhow Context vs unwrap/expect)

**生产代码 unwrap/expect 数: 0**

`grep -n "\.unwrap()\|\.expect(" src/` 全部命中:
- `src/main.rs:15` `unwrap_or_else` (合法 fallback, 不是 unwrap)
- `src/main.rs:183/194/195/222` — **#[cfg(test)] mod tests** 内
- `src/frame_stats.rs:142/157/193/202` — `#[cfg(test)] mod tests` 内
- `src/pty/mod.rs:312..461` — `#[cfg(test)] mod tests` 内
- `src/wl/keyboard.rs:471..606` — `#[cfg(test)] mod tests` 内
- `src/term/mod.rs:907/914/1313/1336` — `#[cfg(test)] mod tests` 内
- `src/text/mod.rs:700..1221` — `#[cfg(test)] mod tests` 内

**CLAUDE.md "禁用 unwrap/expect" 严守** ✅

`grep "anyhow::Context\|\.context(" src/` 普遍使用, with_context 在 IO 路径完整 (e.g. PTY spawn_program / mmap keymap / file create / encoder write 等)。

---

## F. dead code 累积

`grep "#\[allow(dead_code)\]" src/` 命中 6 处:

1. **`src/wl/keyboard.rs:131` `repeat_info()` fn** — T-0501 阶段不消费, Phase 6 timerfd 接 calloop 时移除。注释引派单 In #B 作 traceability ✅ 合理 forward-compat hook
2. **`src/wl/pointer.rs:151` `last_button_serial` 字段** — T-0504 PointerAction::StartMove 直接携带 serial 不读此字段, Phase 6+ show_window_menu / resize 路径需读。注释明示与 KeyboardState::repeat_info 同 forward-compat 决策 ✅ 合理
3. **`src/text/mod.rs:81` `swash_cache` 字段** — Phase 4 后续 ticket (T-0403) 接入光栅化用, T-0401 起手已持有避免后续 ticket 改 struct ✅ 合理预先持有
4. **`src/wl/render.rs:104` `instance` 字段** — INV-002 必需的"持 Vulkan 实例避免提前 drop", 不是真死代码, 只是没 method 访问 ✅
5. **`src/wl/render.rs:146` `view` 字段** — GlyphAtlas 内, 注释 143-149 显式说"不是 placeholder, 是显式持资源不依赖 bind_group 内部 Arc 计数"+ 未来 atlas 重建路径 (T-0406 LRU) 可直接换 view/sampler 不重建 bind_group ✅ 合理
6. **`src/wl/render.rs:148` `sampler` 字段** — 同 5 ✅

**6 处 dead_code 全部有 traceability 注释 + 合理理由**, 无真死代码堆积 ✅

---

## G. 跨 ticket 命名漂移

### dirty 标志命名 ⚠️ 1 处轻微漂移

- `WindowCore::resize_dirty` (T-0107) — INV-006
- `State::presentation_dirty` (T-0504) — CSD hover 重画
- `TermState::dirty` (T-0302) — VT cell 内容变化
- `TermState::mark_dirty()` (T-0504) — 显式置位入口

**注释漂移**: `src/ime/mod.rs:215` doc 说 "调用方置 `preedit_dirty`", 但实装走另一路 (window.rs:1529 / 1616 注释明示**未采用** preedit_dirty 标志, 复用既有 idle 节奏)。
- 严重性: P3 (注释误导, 不影响行为)
- 修复: ime/mod.rs:215 改 doc 为 "preedit 变化由 idle callback 自然在下一帧反映" 与 window.rs 实装注释对齐

### handle_X_event 一致 ✅

- `handle_event` (window.rs) — WindowEvent
- `handle_key_event` (keyboard.rs) — wl_keyboard::Event
- `handle_pointer_event` (pointer.rs) — wl_pointer::Event
- `handle_text_input_event` (ime.rs) — zwp_text_input_v3::Event

命名一致, 都返 quill 自有 Action enum, 都纯逻辑 (副作用解耦), 都有单测。conventions.md §3 抽决策套路全 4 模块严守 ✅

### Action enum 命名 ✅

WindowAction / PtyAction / KeyboardAction / PointerAction / ImeAction 5 个 enum, 命名风格一致 (`<域>Action`), 都 `#[derive(Default)]` + `Nothing` 兜底 variant ✅

### Color / CellPos / Color SOP ✅

T-0302 引入"模块私有 inherent fn `from_<crate>`" SOP 后, T-0303 (CursorShape) / T-0304 (ScrollbackPos) / T-0305 (Color) / T-0401 (ShapedGlyph) / T-0403 (RasterizedGlyph / GlyphKey) / T-0501 (KeyboardAction) / T-0504 (HoverRegion) / T-0505 (CursorRectangle / ImeAction) 全部沿袭, 12 次零真违规。

---

## H. 测试覆盖洞

199 tests pass (release), 6 suite (lib unit + 19 integration files):

**集成测试覆盖路径**:
- `pty_*` (echo / calloop_smoke / resize / to_term) — PTY 端到端
- `state_machine.rs` (11 tests) — WindowCore 状态机全分支
- `xdg_decoration.rs` (3 tests) — T-0503 装饰协商
- `frame_stats_smoke.rs` + `frame_stats_integration.rs` — Phase 6 采集点
- `glyph_atlas.rs` + `atlas_clear_on_full.rs` — T-0403/T-0406 atlas
- `headless_screenshot.rs` — T-0408 离屏渲染基建 (三源 PNG verify SOP)
- `cjk_fallback_e2e.rs` — T-0405 CJK 字形 fallback verify
- `csd_e2e.rs` — T-0504 CSD CSD titlebar
- `ime_e2e.rs` + `ime_preedit_render.rs` — T-0505 IME 协议 + preedit 渲染
- `keyboard_event_to_pty.rs` — T-0501 键盘 → PTY
- `ls_la_e2e.rs` — T-0307 grid 内容
- `resize_chain.rs` — T-0306 三方 resize
- `pty_resize.rs` — T-0204 ioctl
- `pty_to_term.rs` — T-0301 字节 → grid
- `buffer_scale.rs` — T-0502 HiDPI scale

**未测路径** (P3 级, Phase 6 跟进):
1. **Wayland 真 compositor session 退出 (xdg close)** — TD-009 已记 (GNOME mutter 不暴露 foreign-toplevel close IPC, sway / Hyprland 才能自动化)
2. **`drive_wayland` + `pty_read_tick` 真 EventLoop 集成** — 单元测试只覆盖纯逻辑函数 (`pty_readable_action` / `should_propagate_resize` 等), 整 EventLoop 路径靠手测 + 集成测试间接覆盖
3. **`OutputHandler` `new_output` / `update_output` 真 wl_output event** — `verdict_for_scale` 纯逻辑单测覆盖, 但真 wl_output Dispatch 路径无集成测试 (派单 Out 段允许)
4. **`Dispatch<wl_pointer>` 真鼠标事件链** — pointer.rs 单测覆盖 hit_test + handle_pointer_event 纯逻辑, 真 wl_pointer event → button click → xdg_toplevel.move 链路靠手测

**测试评价**: 单元 + 集成混合策略合理, conventions.md §3 抽状态机模式让纯逻辑都能 headless 测; wayland/wgpu 不写自动化测试是项目硬决策 (CLAUDE.md 风格)。

---

## I. 注释 vs 实装矛盾

### I-1: ime/mod.rs:215 提 `preedit_dirty` 但实装未采用 ⚠️ P3

`src/ime/mod.rs:215`:
```rust
/// preedit 变化 → 触发渲染 (调用方置 `preedit_dirty`, idle callback
/// draw_frame 时画下划线 + 字)。空 text = 清 preedit。
UpdatePreedit { ... }
```

但 `src/wl/window.rs:1525-1535` 注释明示**没**新加 `preedit_dirty` 标志:
```rust
// **真简化**: ImeState 本身已存 current_preedit, idle callback 每次 draw 都
// 读它 — preedit 变化自然在下一帧反映, 不需要额外 dirty 标志
```

修复: ime/mod.rs:215 doc 改为 "preedit 变化由 idle callback 在下一次 draw 时反映 (window.rs apply_ime_action)"。

### I-2: TermState mark_dirty doc 与 T-0504 实装一致 ✅

(T-0504 audit 早期发的 src/term mark_dirty 注释跟实装矛盾问题已被修, src/term/mod.rs:636-646 注释引 T-0504 + presentation_dirty 路径解释完全 align 当前实装)

### 其他 doc-vs-code: 无 P0/P1 级矛盾 ✅

抽样查 INV-001 / INV-002 / INV-006 / INV-008 / INV-009 / INV-010 doc 与实装全 align.

---

## J. 跨模块 API 一致性

### PtyHandle (7 pub fn)
- `spawn_shell` / `spawn_program` (构造, Result)
- `raw_fd` / `read` / `resize` / `write` / `try_wait` (操作)

命名: snake_case, 动词为主, `try_wait` 用 try_ 前缀表非阻塞 ✅

### TermState (15 pub fn)
- ctor: `new` / `resize` / `advance` / `clear_dirty` / `mark_dirty`
- 读: `cursor_pos` / `line_text` / `cells_iter` / `is_dirty` / `cursor_visible` / `cursor_shape` / `dimensions` / `scrollback_size` / `scrollback_line_text` / `scrollback_cells_iter`

命名: snake_case 一致, scrollback_* 前缀清晰 ✅

### TextSystem (5 pub fn)
- `new` / `primary_face_id` / `shape_one_char` / `rasterize` / `shape_line`

`shape_one_char` (T-0401 探测 API) 与 `shape_line` (T-0402+ 主路径) 共存, 注释 (text/mod.rs:269-272) 明示 shape_one_char 是 "Phase 4 临时探测 API, T-0402 后续 ticket 替代", 但**未删** — 接受 (Phase 4 已合, 删除留 Phase 6 housekeeping)。

### KeyboardState / PointerState / ImeState 全私有字段 + handle_X_event + Action enum 三件套 ✅

跨 4 模块抽象一致, 调用方 (window.rs Dispatch) 套路相同。

### Dispatch impl 7 个 (CompositorHandler / OutputHandler / WindowHandler / SeatHandler + Dispatch<wl_keyboard / wl_pointer / TextInputManager / TextInputV3>) ✅

派发清晰, 各 Dispatch impl 短 + 调 quill 自有 fn 处理副作用, 无 wayland 类型外漏。

---

## K. 文档完整性

### docs/ROADMAP.md ✅

Phase 1 (5/7 实合, T-0103 推迟) / Phase 2 (6/6) / Phase 3 (7/7) / Phase 4 (8/8) / Phase 5 (3/3 + 2 hotfix) 全合。决策日志 (160-203) 144 行覆盖每个阶段拐点。Phase 6 起 T-0601/0602/0603 已 ROADMAP 列出 (137-145), 待写码 worktree 完成后回归 ROADMAP 更新。

### docs/conventions.md ✅

跟实际 5 步流程 (claim / impl / 四门 / in-review / merge) 一致 (line 130-211)。陷阱 4 条 (POLLHUP / git add -A / 伪派活信号 / fixup1 regression) 全是已踩坑实录, traceability 强。

### docs/invariants.md ✅

INV-001..010 与 src/ 实际一致 (本 audit A 段逐条 verify 过)。条目编号规则 (line 262-267) 明示删除留 tombstone, 未来扩展友好。

### docs/audit/ — 35 个 audit 文档 ✅

per-ticket audit 完整 (T-0102 / T-0104 / T-0106 / T-0107 / T-0108 / T-0201..T-0206 / T-0301..T-0307 / T-0399 / T-0401..T-0408 / T-0501..T-0505) + 2 跨 ticket audit (mainline-audit-phase3-end / 本报告) + 2 handoff (T-0202-T-0303 / T-0303 等)。

### docs/adr/ — 7 个 ADR ✅

0001..0007 全编号连续, 无 tombstone (0003 用 Status 标 Superseded 而非删, 与 invariants.md 编号规则一致)。

### docs/tech-debt.md ✅

19 条 TD 登记 (TD-001..TD-019), 9 条 RESOLVED + 2 OBSOLETE + 8 待。状态字段清晰, 触发修理条件 + 解决路径每条都填。

---

## L. 累积技术债

### TODO/FIXME/XXX/HACK: 0 ✅

CLAUDE.md "禁止以后再说 TODO" 严守, 所有"延后"决策都进 docs/tech-debt.md 登记 (有 trigger condition + 解决路径)。

### 性能瓶颈 hint (Phase 6 soak 前)

1. **每帧全屏重画 (cells_iter collect)** — `window.rs:943` `cells: Vec<CellRef> = t.cells_iter().collect()` 60 KiB / frame, 5090 GPU 无压力, 但 Phase 6 soak 若发现 alloc 抖动可改 reuse Vec
2. **每帧 row_text collect** — `window.rs:971` `(0..rows).map(|row| t.line_text(row)).collect()` 7 KiB Vec<String>, Phase 6 同上
3. **Glyph atlas clear-on-full 不是真 LRU** — T-0406 KISS 决策, 注释明示终端字符集稳定不会 thrash, Phase 6 soak 1h 若 atlas 满频繁可升级
4. **HashMap with DefaultHasher** — Phase 4 atlas key 用 std DefaultHasher, 不引 ahash (派单硬约束); Phase 6 若发现 hot path 可换

### 内存泄漏潜在点

1. **alacritty Term 长跑 scrollback** — 默认 10000 行, 持续 PTY 输出会触上限, ring buffer 自动丢旧, 不真 leak ✅
2. **GlyphAtlas allocations HashMap** — clear-on-full 重置, atlas 满即清, 不增长 ✅
3. **FrameStats intervals Vec** — capacity FRAME_WINDOW=60, 满即 clear, 不增长 ✅
4. **PtyHandle reader Box** — 单次构造, drop 时正确释放 dup fd (INV-008 顺序保证) ✅

无明显 leak risk, Phase 6 T-0601 soak (1h RSS 监控) 可正式验证。

---

## P 列表

### P0 (阻塞 Phase 6 daily-drive)
**0 项**。Phase 5 全合 + 4 hotfix daily-drive 全套到位 (键盘 + CJK + IME + CSD + HiDPI), Phase 6 polish 三并行 (T-0601/0602/0603) 跑中, 主干 healthy。

### P1 (主合后跟进)
**0 项**。

### P2 (低优, Phase 6+ 处理)

#### P2-1: shape_one_char 替代决策已实但未删 ⚠️
- 位置: `src/text/mod.rs:290`
- 现状: `shape_one_char` (T-0401 探测 API) 注释明示"被 T-0402 shape_line 替代", 但仍存在
- 影响: 6 个测试用 (text/mod.rs `tests::shape_*`), 删除会丢测试, 不删占 ~25 行 + 增加 API surface
- 建议: Phase 6 housekeeping 单 ticket — 把测试改用 `shape_line(&str)` (取首 glyph), 删 `shape_one_char`

#### P2-2: TextSystem::primary_face_id() 仅测试用 (公共 API 但生产路径无消费)
- 位置: `src/text/mod.rs:241`
- 用途: tests `face_lock_uses_preferred_monospace` / `shape_ascii_uses_primary_face` 用
- 评估: 公共 API 但生产路径 0 调用, 严格意义上类似 `repeat_info` 等 forward-compat hook
- 建议: 加 `#[allow(dead_code)]` + 注释 traceability, 或 `#[cfg(test)]` 限定 (后者更严但可能限制 Phase 6 调试用途), 倾向前者

### P3 (建议)

#### P3-1: ime/mod.rs:215 doc 提 `preedit_dirty` 但实装未采用 ⚠️
- 位置: `src/ime/mod.rs:215` (UpdatePreedit variant doc)
- 修复: doc 改为 "preedit 变化由 idle callback 在下一次 draw 时反映 (window.rs:1525-1535 apply_ime_action 注释明示真简化)"
- 1 行改动

#### P3-2: drive_wayland step 3.5 注释中 "T-0306" 引用可加 propagate_resize_if_dirty link
- 位置: `src/wl/window.rs:628-632`
- 现状: 注释引 T-0306 但未用 `[propagate_resize_if_dirty]` 文档链接
- 建议: 加 markdown link 让 IDE 跳转

#### P3-3: Renderer::resize 文档 + INV-002 文字仍用 "T-0306 文字回溯校正" 措辞
- 位置: `docs/invariants.md:78` INV-002 演进追溯段
- 现状: T-0305 加 4 字段然后 T-0306 文字回溯校正写法 (历史正确), Phase 6 reader 可能困惑
- 建议: Phase 6 housekeeping 把"演进追溯"段简化为最终字段表 + 一行 changelog, 不影响 INV 本身正确性

---

## 结论

**overall health: A (优, 主干健康可发布)**

Quill Phase 1-5 全合 + 4 hotfix 后整体代码质量在 quill 项目 5000+ LOC daily-drive 终端范围内**优于行业平均**:

1. **类型隔离 INV-010 12 次零违规** — alacritty / wgpu / cosmic-text / wayland-protocols / xkbcommon / portable-pty 6 个上游 crate 全锁本模块, 公共 API 全 quill 自有, 给未来换 VT 库 / wgpu 主版本升级留单点改动
2. **生产代码 0 unwrap/expect / 0 TODO/FIXME** — CLAUDE.md 硬约束严守
3. **conventions §3 抽决策状态机模式 4 模块覆盖** (window/keyboard/pointer/ime), 决策与副作用分离, headless 单测覆盖纯逻辑
4. **199 tests pass + clippy 全绿 + cargo audit 0 CVE** — 自动化基建到位
5. **跨 ticket 命名漂移仅 1 处 P3** (ime preedit_dirty 注释), 12 个 invariant 全部维持

剩余技术债集中在 P2/P3 housekeeping 级 (shape_one_char 替代未删 / face_id() 公共未消费 / 注释微漂移), 不阻塞 Phase 6 daily-drive。

### 推荐下一步

**1. Phase 6 polish 三并行继续 (T-0601/0602/0603 不动)**, 主干已就绪。

**2. Phase 6 housekeeping ticket** (1 ticket / <1h), 一次清 P3-1 (ime doc 漂移) + P2-1 (shape_one_char 删除) + P2-2 (face_id allow_dead_code) + P3-3 (INV-002 演进段简化)。

**3. Phase 6 T-0601 soak 1h 跑起来后**, 验证:
- RSS 漂移 < 10% (Phase 6 目标)
- alacritty Term scrollback 上限触发后 ring buffer 行为正常
- GlyphAtlas clear-on-full 在 1h ASCII + 偶发 CJK 输入下未触发 (终端字符集稳定假设验证)

**4. Phase 6 daily drive 切到 quill 后**, 留意:
- 键盘 repeat 工作 (T-0603 实装后)
- 滚动 (T-0602 实装后)  
- 鼠标光标形状切换 (T-0601 实装后)
- IME 长 session 候选框定位 (Phase 5 已 work, soak 验证)

**Phase 5 → Phase 6 过渡批准** ✅

---

## 附录: 关键 grep 命令 (复现性)

```bash
# INV-004 verify
grep -E '#!\[forbid\(unsafe_code\)\]' src/    # 应零命中

# INV-005 verify
grep -rn "thread::spawn\|tokio::spawn" src/  # 应只命中 main.rs headless / cfg(test)

# INV-010 verify
grep -E "alacritty_terminal::|wgpu::|cosmic_text::|wayland_protocols::|xkbcommon::" \
  src/lib.rs src/main.rs src/frame_stats.rs   # 应零命中

# unwrap/expect 生产代码 verify
grep -n "\.unwrap()\|\.expect(" src/main.rs src/frame_stats.rs src/wl/window.rs \
  src/wl/render.rs src/wl/pointer.rs src/wl/keyboard.rs src/pty/mod.rs \
  src/term/mod.rs src/text/mod.rs src/ime/mod.rs   # 命中应只在 #[cfg(test)] 块

# TODO/FIXME verify
grep -rn "TODO\|FIXME\|XXX\|HACK" src/ tests/  # 应零命中

# SAFETY 注释完整 verify
grep -c "unsafe " src/wl/render.rs src/wl/window.rs src/wl/keyboard.rs src/pty/mod.rs
grep -c "// SAFETY:" src/wl/render.rs src/wl/window.rs src/wl/keyboard.rs src/pty/mod.rs
```
