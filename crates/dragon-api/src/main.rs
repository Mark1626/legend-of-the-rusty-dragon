//! Local development server.
//!
//! On Vercel the platform owns the process and `api/index.rs` is the entry
//! point; this is the same router behind an ordinary listener, so the game can
//! be played and tuned without deploying anything.

use anyhow::Result;
use tokio::net::TcpListener;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,dragon_api=debug".into()),
        )
        .init();

    let (router, config) = dragon_api::from_env().await?;
    let listener = TcpListener::bind(("0.0.0.0", config.port)).await?;

    tracing::info!(
        port = config.port,
        tick_secs = config.pacing.tick_secs,
        "the Realm is open"
    );
    axum::serve(listener, router).await?;
    Ok(())
}
