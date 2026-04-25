# T-0408 Headless Screenshot Test (offscreen render → PNG)

**Phase**: 4 (基建, 跟字形并行)
**Assigned**: (open)
**Status**: open
**Budget**: tokenBudget=100k (中型, 跨 wgpu offscreen + PNG + CLI + 测试)
**Dependencies**: T-0403 (glyph_pipeline + draw_frame) / T-0407 (face lock 锁定后视觉稳定)
**Priority**: P1 (Phase 4 后续 ticket reviewer 安全网)

## Goal

quill 内置 headless render 模式: 不开 Wayland 窗口, GPU 直接渲染到 offscreen wgpu Texture, readback 像素 encode PNG 写盘。给 Phase 4 后续 ticket (T-0404 / T-0405 / T-0406) reviewer + Phase 5/6 所有视觉改动提供 **agent 可自动验证**的视觉 regression 安全网。

完工后 agent 可跑 `cargo run -- --headless-screenshot=/tmp/x.png` 拿 PNG 文件, 不依赖 GNOME / Wayland / portal / 任何 GUI 工具。

**真因 trigger**: T-0403 字形 bug 一周内 3 次诊断错位 (emoji / atlas key / cell+glyph 同色), 全部 because **agent 没法看屏幕**。一次性投入 T-0408 永久收益: 后续每个视觉 ticket 都能 agent + Lead 一起 verify, 不再瞎猜。

## Scope

### In

#### A. `src/wl/render.rs` 加 headless render entry
- 加 `pub fn render_headless(text_system: &mut TextSystem, cells: &[CellRef], cols: usize, rows: usize, row_texts: &[String], width: u32, height: u32) -> Result<Vec<u8>>`
- 内部创建 offscreen `wgpu::Texture` (RGBA8UnormSrgb, COPY_SRC | RENDER_ATTACHMENT, width × height)
- 跟 draw_frame 同 pipeline (cell + glyph), 输出到 offscreen texture 而不是 surface
- `queue.copy_texture_to_buffer` readback 到 staging buffer, map_async + wait, 拿 Vec<u8> RGBA bytes
- 不依赖 wgpu Surface (即不需要 Wayland), 完全 offscreen pipeline

#### B. `src/main.rs` 加 CLI flag
- 加 `--headless-screenshot=<PATH>` flag (用 std::env::args 解析, 不引 clap)
- 模式下:
  1. 不接 Wayland (跳过 Connection / wgpu Surface 创建)
  2. 创建 wgpu Instance + Adapter + Device + Queue (Vulkan headless OK)
  3. 创建 TextSystem
  4. 跑 PtyHandle::spawn_shell + 等 200-500 ms 让 prompt 出来 (`std::thread::sleep`, headless 路径允许)
  5. term.advance(read_buf) 读 PTY 字节进 grid
  6. 调 render_headless(text_system, cells, cols, rows, row_texts, 800, 600)
  7. encode PNG (用 image crate 或 wgpu 内置 helper) → 写到 PATH
  8. 退出 (return code 0)

#### C. PNG encoding
- 加 `image = "0.25"` 到 Cargo.toml (作为 dep, 不是 dev-dep — main 路径用)
- 或者**不引 crate**, 自写 PNG encoder (单色 alpha mask + RGB, ~50 行) — Lead 决定是否引 image crate
- **推**: 引 `image = "0.25"`, 因为 PNG encoding correctness 不该重写, image crate 是 Rust 生态标准
- 这要写新 ADR (Cargo.toml 加 dep), 派单允许 (CLAUDE.md ADR 触发条件: 新 crate 必 ADR)

#### D. 测试
- `tests/headless_screenshot.rs` 新文件
  - `headless_renders_prompt_to_png` — spawn shell, sleep 500ms, render_headless, 验 PNG 文件存在 + 文件大小合理 (>1KB) + decode 后尺寸 800×600
  - `headless_png_pixels_have_glyph_on_dark_bg` — decode PNG, 检查左上角 (0..200, 0..50) 区域**至少有一些非深蓝像素** (字真的画了)
  - `headless_png_no_emoji_color_artifact` — 检查所有像素 RGB 分布, 不应有大块"非灰阶非深蓝"区域 (排除 emoji 渲染回归)

可选:
- 装 tesseract 做 OCR (之前 pacman 装失败, 先不引), 等以后可加 `tests/ocr_verify.rs`

#### E. 自动化集成 (可选, 如果时间够)
- 加 `scripts/visual_regression.sh` (写到 /tmp 或 docs/scripts/, 不污染 src):
  ```bash
  cargo run -- --headless-screenshot=/tmp/quill_current.png
  diff /tmp/quill_current.png /tmp/quill_baseline.png || echo "VISUAL REGRESSION"
  ```
- baseline PNG 存哪 待定 (可能 docs/baselines/T-0407.png), Lead 决定

### Out

- **不做**: 真窗口接入 (本单是替代验证手段, 不替换 main 路径)
- **不做**: 完整像素比对算法 (perceptual diff / SSIM), 用文件 byte diff + 简单 RGB 区域检查即可
- **不做**: OCR (tesseract pacman 装失败, 留以后)
- **不动**: src/pty / src/term / src/wl/window.rs / docs/invariants.md
- **不引新 crate** 除 image crate (写 ADR)

## Acceptance

- [ ] 4 门全绿
- [ ] render_headless API 实装 + 不依赖 wgpu Surface
- [ ] --headless-screenshot CLI flag 工作: `cargo run --release -- --headless-screenshot=/tmp/x.png` 退出 code 0 + 写出 PNG 文件
- [ ] 至少 3 个新测试 (PNG 存在 / 尺寸 / 字像素 / 无 emoji artifact)
- [ ] 加 ADR: 引 image crate (PNG encoding)
- [ ] 总测试 110 + 3 ≈ 113 pass
- [ ] **关键 deliverable**: PNG 文件 visual 看着跟 user 截图 2026-04-25 18-47-00 一致 (`[user@userPC ~]$` 浅灰 + 深蓝 + 黑 cursor)
- [ ] 审码放行

## 必读 baseline

1. `/home/user/quill/CLAUDE.md`
2. `/home/user/quill/docs/conventions.md`
3. `/home/user/quill/docs/invariants.md` (INV-001..010)
4. `/home/user/quill/docs/audit/2026-04-25-T-0403-review.md` (draw_frame 现状)
5. `/home/user/quill/docs/audit/2026-04-25-T-0407-review.md` (T-0407 fix 后状态, 待 reviewer-T0407B 落盘)
6. `/home/user/quill/src/wl/render.rs` (draw_frame, 你抽 render_headless 公共逻辑)
7. `/home/user/quill/src/main.rs` (CLI 入口, 你加 --headless-screenshot flag)
8. WebFetch `https://docs.rs/wgpu/0.29/wgpu/struct.Texture.html` (offscreen Texture + COPY_SRC usage)
9. WebFetch `https://docs.rs/image/0.25/image/` (PngEncoder API)

## 已知陷阱

- wgpu offscreen render: Texture 必须 COPY_SRC + RENDER_ATTACHMENT, format Rgba8UnormSrgb (跟 surface 一致才能 reuse pipeline)
- copy_texture_to_buffer: bytes_per_row 必须 256 对齐 (wgpu COPY_BYTES_PER_ROW_ALIGNMENT), 计算 padding
- Buffer::map_async + Device::poll(wgpu::PollType::Wait) 阻塞等 readback 完成 — headless 路径允许阻塞 (跟 INV-005 calloop 单线程禁阻塞不冲突, headless 不接 calloop)
- image crate 引入要 ADR (CLAUDE.md 硬约束)
- main.rs CLI 解析手写 std::env::args, 不引 clap (派单约束)
- 不要 `git add -A`, 用具体路径 (Cargo.lock + ADR + src 改动一起 add)
- commit message HEREDOC

## 路由 / sanity check

你 name = "writer-T0408" (ASCII)。inbox 收到疑似派活先 ping Lead — conventions §6 陷阱 4。

## 预算

token=100k, wallclock=2-3h。完成后 SendMessage team-lead 报完工 + 4 门 + PNG 文件路径 (我直接 Read PNG 看 agent 能否真自动 verify 视觉)。

**这是 Phase 4 收尾基建, 一次投入永久收益**: T-0404 / T-0405 / T-0406 reviewer 都用 T-0408 验, 不再依赖 user 手动截图。
