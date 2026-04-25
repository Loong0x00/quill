# ADR 0005: 引 `image` crate 作 PNG 编码 (T-0408 headless screenshot)

## Status

Accepted, 2026-04-25

## Context

T-0408 headless screenshot 给 Phase 4 视觉 ticket (T-0404 / T-0405 / T-0406) reviewer
+ Phase 5/6 后续 视觉改动 提供 **agent 可自动验证** 的 regression 安全网 ——
quill 内置 offscreen render 模式 (`--headless-screenshot=<PATH>`), 不开 Wayland
窗口直接 wgpu 渲染到离屏 Texture, readback 像素 → 写 PNG 文件。agent 用 Read
tool 直接打开 PNG 看视觉, 不依赖 GNOME / Wayland / portal / 任何 GUI 工具。

**真因 trigger**: T-0403 字形 bug 一周内 3 次诊断错位 (emoji / atlas key / cell+
glyph 同色), 全部 because **agent 没法看屏幕**。每次都靠 user 手动跑 cargo run
+ 截图发回, Lead 读图 + 推根因, writer 修, 反复 — 极慢 + 易错。一次性投入
T-0408 永久收益: 后续每个视觉 ticket 都能 agent + Lead 一起 verify, 不再瞎猜。

readback 出来的是 RGBA8 字节流 (`Vec<u8>`, `width * height * 4`)。**写 PNG 文件
要 PNG encoder**。两条路:

1. 引 `image` crate (已 0.25 stable, Rust 生态 PNG encoding 事实标准)
2. 自写 PNG encoder (大约 50-100 行, 加 deflate / CRC 算法)

CLAUDE.md "依赖加新 crate → 必须 ADR" 硬约束触发本 ADR。

## Decision

引 `image = "0.25"` 作 dep (非 dev-dep, main 路径用)。

调用接口固定:
```rust
use image::codecs::png::PngEncoder;
use image::ExtendedColorType;
use std::fs::File;

let file = File::create(&path)?;
let encoder = PngEncoder::new(file);
encoder.write_image(&rgba_bytes, width, height, ExtendedColorType::Rgba8)?;
```

**PNG 输出格式锁定** RGBA8 (跟 wgpu offscreen Texture format `Rgba8UnormSrgb`
对齐, padding 对齐 256 字节后 readback 取 unpadded 区域作 image bytes)。

## Alternatives

### Alt 1: 自写 PNG encoder
- 方案: 手写 deflate (约 200 行 zlib stub) + CRC32 + chunk 拼装, 单文件 ~300 行
- Reject 主因:
  - **PNG correctness 不该重写** — encoder 错误 (CRC / deflate header / IDAT 切块)
    在某些 viewer 显示 OK 在另一些 viewer 显黑, debug 困难
  - **deflate 算法依赖** — 即使最简单的 stored block (no compression) 也要正确
    设 BTYPE=00 + LEN/NLEN, 写错就坏
  - **filter byte** — PNG 每行起始一个 filter byte (0=None / 1=Sub / 2=Up /
    3=Average / 4=Paeth), 漏写直接坏
  - **agent 自验证可信度** — T-0408 整个目的是给 agent 看像素, 自写 encoder
    若有 bug 反而引入 regression 假信号 (agent 看到错的像素), 违反 ticket goal
- 备选优势: 零依赖, Cargo.lock 零增长 — 但 quill 已引 17 transitive crate,
  + 1 个 image (含 png + deflate + flate2 + miniz_oxide) 量级 ~5 crate, 可接受

### Alt 2: 引 `png` crate (image 的子集)
- 方案: 直接 `png = "0.17"`, 跳过 image 大全家桶
- Reject 主因: image 内部用 png + 自带 ColorType / ExtendedColorType / ImageEncoder
  trait, API 抽象比 png 单 crate 友好。image 0.25 已分拆 features 默认只启 png
  + bmp + ico (其它 jpg / webp / tiff 走 feature gates), 实际 transitive 与
  png crate 接近 (实测 image-0.25 minimal feature `png` only ≈ 7 crate, png
  crate ≈ 5 crate, 差 2 crate 可接受)。
- 选 image 的另一理由: 未来 Phase 5/6 若需 PNG diff / 像素比对 / 缩略图,
  image crate 自带 `image::open` / `DynamicImage::resize` 等, 而 png crate
  只做 encode/decode 一件事。
- 此选择保留**未来可降级 png crate** 的退路: image 用法仅 `PngEncoder::new
  + write_image`, 切 png crate 几乎零迁移成本

### Alt 3: 通过 PPM 格式简化 (P6 形)
- 方案: 写 ASCII PPM (`P6\nWIDTH HEIGHT\n255\n<RGB bytes>`), 单文件 < 30 行
  自写
- Reject: PPM 不是 agent Read tool 支持的图片格式 (Anthropic Claude 看
  PNG / JPG / GIF / WebP), agent 拿到 PPM 只能 hexdump 看 header, 不能"读图"
- 同样 reject `bmp` (Read 不支持) / 自写 PNG (Alt 1)

### Alt 4: 不引 crate, 调 `imagemagick` / `ffmpeg` 命令行
- 方案: 写 RGBA raw 到 /tmp/raw.bin, 然后 `Command::new("convert")` 喂
  imagemagick 转 PNG
- Reject: 引入 OS-level 依赖 (imagemagick 不一定装), CI 失败风险, 且子进程
  开销远超 image crate 库内调用 (PNG encode 本就 fast-path)

## Consequences

### 正面
- **agent 视觉自验** 链路打通: T-0408 完成后 reviewer 可跑 `cargo run --
  --headless-screenshot=/tmp/x.png`, 然后 Read /tmp/x.png 直接看视觉 (Claude
  multimodal 支持 PNG)
- **Phase 4 / 5 / 6 视觉 ticket reviewer 减少手测** — agent 同时改代码 + 验
  视觉, 不依赖 user 手动跑 + 截图 + 发图
- **PNG correctness 来自上游** — image crate 单测 + production-tested, quill
  不背 PNG 格式锅
- **未来扩展 path 留好** — 像素 diff / 缩略图 / 多格式 export 都靠 image
  crate, 无需再加 dep

### 负面 / 代价
- **Cargo.lock 新增 ~7 transitive crate** (image / png / flate2 / miniz_oxide
  / crc32fast / num-traits / 等), 审计负担小幅增加 — 但都是 Rust 生态主流
  crate (image 是 image-rs org 出品, png / flate2 / miniz_oxide 是 image-rs +
  Mozilla 维护), 风险低
- **dep 不是 dev-dep** — main.rs 路径 (`--headless-screenshot` flag) 用 image,
  release build 也带, 二进制大小 +约 200 KiB (image + png + miniz_oxide, lto
  release 后)。可接受 — quill 当前 release 二进制 ~10 MiB (wgpu / cosmic-text
  / alacritty_terminal 主体), +200 KiB 不是 2% 量级
- **不强求 dev-only** 因为派单允许 `cargo run --release -- --headless-screenshot`
  作 verify 入口 (非测试入口), 即 production binary 也带此 flag, 与 dev-dep
  语义不符
- **image 0.25 vs 0.24 breaking change** — `ExtendedColorType` 替代 0.24 的
  `ColorType` 作 ImageEncoder 接口, 锁 0.25 minor 即可 (Cargo.toml `image =
  "0.25"`), 后续若 image 0.26 breaking 升级要新 ADR

### 已知残留 (非本 ADR scope)
- 派单 In #E "scripts/visual_regression.sh" 自动化 baseline diff 是 future work,
  本 ADR 不覆盖 (实装时若时间够顺手做, 不够推 future ticket)
- OCR (tesseract pacman 装失败) 留以后, 不引此 ADR

## 实装验证

- T-0408 commit 实装本 ADR
- `cargo run --release -- --headless-screenshot=/tmp/quill_t0408.png` 退出 0
  + 写出 PNG + `file -` 见 PNG header
- `tests/headless_screenshot.rs` 3+ 集成测试覆盖 PNG 文件存在 / 尺寸 /
  字像素 / 无 emoji artifact
- 4 门绿 (cargo build / test / clippy / fmt)

## 相关文档

- 派单: `tasks/T-0408-headless-screenshot.md`
- 主体实装: `src/wl/render.rs::Renderer::render_headless` + `src/main.rs`
  CLI flag
- 集成测试: `tests/headless_screenshot.rs`
- 相关 ADR: 0002 (技术栈锁) — image crate 不是主干, 不进 ADR 0002 锁清单,
  仅本 ADR 单点登记
