// ADR 0001 规定 wgpu FFI / wayland-scanner 产物可能需要 unsafe,通过"显式豁免"放行。
// `forbid` 在 crate 根无法被 inner `#[allow]` 降级,所以本 crate 用 `deny`:默认硬拒,
// 具体 item 加 `#[allow(unsafe_code)]` + `// SAFETY:` 才通过。
#![deny(unsafe_code)]

use std::fs::File;
use std::io::Write as _;
use std::path::PathBuf;

use anyhow::{anyhow, Context, Result};
use tracing_subscriber::EnvFilter;

fn main() -> Result<()> {
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("quill=debug"));
    tracing_subscriber::fmt().with_env_filter(filter).init();

    // why 手写 std::env::args 解析 (派单硬约束: 不引 clap): 当前只 1 个 flag
    // (--headless-screenshot=<PATH>), clap 是过度设计。Phase 5+ 若 flag 数 ≥ 5
    // 再考虑引 clap (届时另写 ADR)。
    let args: Vec<String> = std::env::args().collect();
    let headless_path = parse_headless_screenshot_arg(&args)?;

    if let Some(path) = headless_path {
        tracing::info!(path = %path.display(), "quill 进入 headless screenshot 模式 (T-0408)");
        run_headless_screenshot(&path)?;
        tracing::info!(path = %path.display(), "headless screenshot 写出完成, 退出");
        return Ok(());
    }

    tracing::info!("quill booting");
    quill::wl::run_window()?;
    tracing::info!("quill exited cleanly");
    Ok(())
}

/// 扫 argv 找 `--headless-screenshot=<PATH>` (单一形式, 不接受空格分隔
/// `--headless-screenshot <PATH>` — 派单硬约束 "手写 std::env::args 解析",
/// 单形式简化逻辑 + 防 shell 误传)。
///
/// 返 `Ok(None)` 当未传, `Ok(Some(PathBuf))` 当合法, `Err` 当格式错或路径空。
fn parse_headless_screenshot_arg(args: &[String]) -> Result<Option<PathBuf>> {
    const PREFIX: &str = "--headless-screenshot=";
    for arg in args.iter().skip(1) {
        if let Some(rest) = arg.strip_prefix(PREFIX) {
            if rest.is_empty() {
                return Err(anyhow!(
                    "--headless-screenshot= 后路径不能为空 \
                     (用法: --headless-screenshot=/tmp/x.png)"
                ));
            }
            return Ok(Some(PathBuf::from(rest)));
        }
        if arg == "--headless-screenshot" {
            return Err(anyhow!(
                "--headless-screenshot 必须 = 形式 \
                 (用法: --headless-screenshot=/tmp/x.png), \
                 不接受空格分隔 — 派单硬约束简化解析"
            ));
        }
    }
    Ok(None)
}

/// **T-0408 主路径**: 不开 Wayland 窗口, 跑 PtyHandle::spawn_shell + 等 prompt
/// 出现 + Term advance 进 grid + render_headless 离屏渲染 + image::PngEncoder
/// 写盘。
///
/// **why 800 × 600 hardcode**: 派单 In #B "调 render_headless(text_system,
/// cells, cols, rows, row_texts, 800, 600)" — Phase 4 视觉 acceptance 与
/// `INITIAL_WIDTH × INITIAL_HEIGHT` (`src/wl/window.rs`) 同尺寸, 给 PNG 后续
/// 比对路径锁定基线。Phase 5+ 若需多尺寸 baseline 加 `--headless-size=WxH`
/// flag, 现在不做。
///
/// **why 80 × 24 grid**: 与 `src/wl/window.rs::run_window` 启动期 `PtyHandle::
/// spawn_shell(80, 24)` 一致, 让 prompt 输出与窗口路径同 grid 形状, 视觉
/// regression 比对可对齐。
///
/// **prompt 等待 500 ms**: bash 启动到 prompt 输出走 ~50-300 ms (实测),
/// 500 ms 给安全余量。`std::thread::sleep` 在 headless 路径允许 (派单已写
/// "headless 路径允许阻塞", 与 INV-005 calloop 单线程禁阻塞不冲突 —— 本 fn
/// 不挂 EventLoop)。
fn run_headless_screenshot(path: &std::path::Path) -> Result<()> {
    use image::codecs::png::PngEncoder;
    use image::ExtendedColorType;
    use image::ImageEncoder;

    use quill::pty::PtyHandle;
    use quill::term::TermState;
    use quill::text::TextSystem;
    use quill::wl::render_headless;

    const WIDTH: u32 = 800;
    const HEIGHT: u32 = 600;
    const COLS: u16 = 80;
    const ROWS: u16 = 24;
    const PROMPT_WAIT_MS: u64 = 500;

    let mut text_system = TextSystem::new()
        .context("TextSystem::new 失败 — check `fc-list :spacing=mono` (需 monospace face)")?;

    let mut term = TermState::new(COLS, ROWS);
    let mut pty =
        PtyHandle::spawn_shell(COLS, ROWS).context("PtyHandle::spawn_shell(80, 24) 失败")?;

    // bash prompt 输出延迟 — 等 PROMPT_WAIT_MS 让 stdout 飞到 master fd
    std::thread::sleep(std::time::Duration::from_millis(PROMPT_WAIT_MS));

    // 非阻塞 read 把 PTY 字节全吸进 term grid。fd 已 O_NONBLOCK (INV-009),
    // WouldBlock 当退出条件; 其他 IO 错误警告但不 fail (允许部分 grid)。
    let mut read_total: usize = 0;
    let mut buf = [0u8; 4096];
    loop {
        match pty.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                term.advance(&buf[..n]);
                read_total += n;
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => {
                tracing::warn!(?e, "headless PTY read 非致命错, 跳出 read 循环");
                break;
            }
        }
    }
    tracing::info!(read_bytes = read_total, "headless PTY drain 完成");

    let cells: Vec<_> = term.cells_iter().collect();
    let (cols_actual, rows_actual) = term.dimensions();
    let row_texts: Vec<String> = (0..rows_actual).map(|r| term.line_text(r)).collect();

    // T-0404: render_headless 返 (rgba, physical_w, physical_h) — physical
    // 尺寸由内部 width/height × HIDPI_SCALE 算 (我们传 logical 800×600,
    // 拿回 physical 1600×1200 用作 PNG header 尺寸)。
    let (rgba, physical_w, physical_h) = render_headless(
        &mut text_system,
        &cells,
        cols_actual,
        rows_actual,
        &row_texts,
        WIDTH,
        HEIGHT,
        None, // T-0505: --headless-screenshot CLI 路径无 IME 上下文, 不画 preedit
        None, // T-0601: CLI 路径不强制画光标 (静态截图 daily-drive 视觉验证
              // 不依赖光标; 集成测试 tests/cursor_render_e2e.rs 走 Some(_)
              // 验证 cursor quad 渲染. 派单 Out 段同决策).
    )
    .context("render_headless 失败")?;

    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("创建父目录 {} 失败", parent.display()))?;
        }
    }
    let mut file =
        File::create(path).with_context(|| format!("创建 PNG 文件 {} 失败", path.display()))?;
    let encoder = PngEncoder::new(&mut file);
    encoder
        .write_image(&rgba, physical_w, physical_h, ExtendedColorType::Rgba8)
        .with_context(|| format!("PngEncoder write_image 写 {} 失败", path.display()))?;
    file.flush()
        .with_context(|| format!("flush PNG 文件 {} 失败", path.display()))?;

    tracing::info!(
        path = %path.display(),
        logical_w = WIDTH,
        logical_h = HEIGHT,
        physical_w,
        physical_h,
        bytes = rgba.len(),
        "headless screenshot 写出 PNG 完成"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_no_arg_returns_none() {
        let args = vec!["quill".to_string()];
        let r = parse_headless_screenshot_arg(&args).expect("no arg should be Ok(None)");
        assert!(r.is_none());
    }

    #[test]
    fn parse_screenshot_path_found() {
        let args = vec![
            "quill".to_string(),
            "--headless-screenshot=/tmp/foo.png".to_string(),
        ];
        let r = parse_headless_screenshot_arg(&args)
            .expect("with valid flag should parse")
            .expect("should be Some");
        assert_eq!(r, PathBuf::from("/tmp/foo.png"));
    }

    #[test]
    fn parse_screenshot_empty_path_errs() {
        let args = vec!["quill".to_string(), "--headless-screenshot=".to_string()];
        assert!(parse_headless_screenshot_arg(&args).is_err());
    }

    #[test]
    fn parse_screenshot_space_separator_errs() {
        let args = vec![
            "quill".to_string(),
            "--headless-screenshot".to_string(),
            "/tmp/foo.png".to_string(),
        ];
        assert!(parse_headless_screenshot_arg(&args).is_err());
    }

    #[test]
    fn parse_unrelated_flags_ignored() {
        let args = vec![
            "quill".to_string(),
            "--unrelated".to_string(),
            "--other=value".to_string(),
        ];
        let r = parse_headless_screenshot_arg(&args).expect("unrelated flags should not error");
        assert!(r.is_none());
    }
}
