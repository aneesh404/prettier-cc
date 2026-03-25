//! continuumd — the Continuum daemon.
//!
//! Watches Claude Code transcript files and serves parsed timeline data
//! to UI clients over a Unix socket.

use continuum_core::config::Config;
use tracing::info;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let config = Config::load();

    let filter = if config.debug {
        "continuum_daemon=debug"
    } else {
        "continuum_daemon=info"
    };
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| filter.into()),
        )
        .with_target(false)
        .init();

    info!(
        version = env!("CARGO_PKG_VERSION"),
        "continuumd starting"
    );

    // Placeholder — the CLI reads transcripts directly for now.
    // The daemon will be needed for real-time file watching + hook ingestion
    // once the TUI sidebar is built.
    info!("continuumd ready — use `continuum` CLI to view timelines");

    tokio::signal::ctrl_c().await?;
    info!("shutting down");
    Ok(())
}
