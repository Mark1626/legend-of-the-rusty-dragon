//! Serverless hosting for Legend of the Pink Dragon.
//!
//! Every route reduces to the same shape: take the game row, call
//! [`dragon_core::step`], write it back. Nothing here knows any game rules.

pub mod auth;
pub mod config;
pub mod error;
pub mod routes;
pub mod store;

use anyhow::Result;
use axum::Router;
use tower_http::cors::{Any, CorsLayer};

pub use config::Config;
pub use store::Store;

#[derive(Clone)]
pub struct AppState {
    pub store: Store,
    /// Accepted signup keys. Empty means the Realm is closed to newcomers.
    pub invite_keys: Vec<String>,
    pub join_attempts_per_window: i32,
    pub join_window_secs: i64,
    pub admin_secret: Option<String>,
    pub cron_secret: Option<String>,
}

impl AppState {
    /// A state with signups closed and no secrets — the safe starting point
    /// for tests, which then opt into whatever they mean to exercise.
    pub fn new(store: Store) -> Self {
        Self {
            store,
            invite_keys: Vec::new(),
            join_attempts_per_window: 10,
            join_window_secs: 3_600,
            admin_secret: None,
            cron_secret: None,
        }
    }
}

/// Build the application. The caller owns connecting and migrating, so tests
/// and the Vercel entry point can share this.
pub fn app(state: AppState) -> Router {
    let api = routes::router().with_state(state);

    // The API is a public read surface with bearer-token writes; there are no
    // cookies to protect, so a permissive CORS policy costs nothing and lets
    // the page be hosted anywhere.
    api.layer(
        CorsLayer::new()
            .allow_origin(Any)
            .allow_methods(Any)
            .allow_headers(Any),
    )
}

/// Connect, migrate, and build the application from the environment.
pub async fn from_env() -> Result<(Router, Config)> {
    let config = Config::from_env()?;
    let store = Store::connect(&config).await?;
    store.migrate().await?;

    if config.admin_secret.is_none() {
        tracing::info!("ADMIN_SECRET is unset; administration routes are disabled");
    }
    if config.invite_keys.is_empty() {
        tracing::warn!("INVITE_KEY is unset; nobody can join this Realm");
    } else {
        tracing::info!(keys = config.invite_keys.len(), "signup is invitation-only");
    }
    if config.cron_secret.is_none() {
        // Harmless rather than dangerous: /api/tick does no work, takes no
        // lock and writes nothing unless the clock actually owes a tick.
        tracing::info!("CRON_SECRET is unset; /api/tick is unauthenticated");
    }

    let state = AppState {
        store,
        invite_keys: config.invite_keys.clone(),
        join_attempts_per_window: config.join_attempts_per_window,
        join_window_secs: config.join_window_secs,
        admin_secret: config.admin_secret.clone(),
        cron_secret: config.cron_secret.clone(),
    };

    let mut router = app(state);
    // Only for local development; on Vercel the platform serves static files.
    if let Some(dir) = &config.static_dir {
        router = router.fallback_service(tower_http::services::ServeDir::new(dir));
    }
    Ok((router, config))
}
