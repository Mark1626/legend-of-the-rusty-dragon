//! The Vercel entry point.
//!
//! One function serves the whole API. `vercel.json` rewrites `/api/(.*)` here,
//! and a rewrite leaves the request's original path intact, so the same axum
//! router that `dragon-server` runs locally does the routing — there is no
//! second copy of the route table to keep in step.
//!
//! Nine separate functions would also work, and would cost nine cold starts,
//! nine copies of the binary, and nine chances for them to drift apart.

use tower::ServiceBuilder;
use vercel_runtime::axum::VercelLayer;

#[tokio::main]
async fn main() -> Result<(), vercel_runtime::Error> {
    // Vercel captures stdout per invocation, so plain formatting is right here;
    // there is no terminal to colour and no file to rotate.
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,dragon_api=info".into()),
        )
        .with_ansi(false)
        .init();

    let (router, config) = dragon_api::from_env().await?;
    tracing::info!(
        tick_secs = config.pacing.tick_secs,
        "the Realm is open on Vercel"
    );

    // `VercelLayer` adapts the router — a `Service<Request<Body>>` — into the
    // `Service<(AppState, Request)>` shape the runtime drives.
    let service = ServiceBuilder::new().layer(VercelLayer::new()).service(router);
    vercel_runtime::run(service).await
}
