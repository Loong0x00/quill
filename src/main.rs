// ADR 0001 规定 wgpu FFI / wayland-scanner 产物可能需要 unsafe,通过"显式豁免"放行。
// `forbid` 在 crate 根无法被 inner `#[allow]` 降级,所以本 crate 用 `deny`:默认硬拒,
// 具体 item 加 `#[allow(unsafe_code)]` + `// SAFETY:` 才通过。
#![deny(unsafe_code)]

use anyhow::Result;
use tracing_subscriber::EnvFilter;

fn main() -> Result<()> {
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("quill=debug"));
    tracing_subscriber::fmt().with_env_filter(filter).init();

    tracing::info!("quill booting");
    quill::wl::run_window()?;
    tracing::info!("quill exited cleanly");
    Ok(())
}
