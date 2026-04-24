#![forbid(unsafe_code)]

use anyhow::Result;
use tracing_subscriber::EnvFilter;

mod wl;

fn main() -> Result<()> {
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("quill=debug"));
    tracing_subscriber::fmt().with_env_filter(filter).init();

    tracing::info!("quill booting");
    wl::run_window()?;
    tracing::info!("quill exited cleanly");
    Ok(())
}
