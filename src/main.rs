#![forbid(unsafe_code)]

use tracing_subscriber::EnvFilter;

fn main() {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("quill=debug"));
    tracing_subscriber::fmt().with_env_filter(filter).init();

    tracing::info!("quill booting");
    println!("quill v0.1.0 - scaffold, not runnable yet");
}
