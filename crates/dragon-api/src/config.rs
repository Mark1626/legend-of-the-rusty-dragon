//! Deployment settings, read from the environment.

use anyhow::{Context, Result};
use dragon_core::config::Pacing;

#[derive(Debug, Clone)]
pub struct Config {
    /// Postgres connection string. On Neon this should be the **pooled**
    /// endpoint: a serverless deployment opens far more connections than a
    /// single long-lived server would.
    pub database_url: String,
    /// How many connections one function instance may hold. Small on purpose.
    pub max_connections: u32,
    /// Signup keys. `POST /api/join` requires one of these, so the Realm is
    /// invitation-only.
    ///
    /// Several may be configured at once, comma-separated, which is what makes
    /// rotation possible without locking anyone out: publish the new key,
    /// carry both for as long as invitations are in flight, then drop the old
    /// one. Joining is refused outright when none is set — an unset key means
    /// nobody can sign up, never that anybody can.
    pub invite_keys: Vec<String>,

    /// How many signup attempts one caller may make per window, successful or
    /// not. Counting failures is what bounds both a leaked invite key and
    /// name enumeration.
    pub join_attempts_per_window: i32,
    pub join_window_secs: i64,

    /// Required by `/api/admin`. Admin routes are refused outright when unset,
    /// rather than left open.
    pub admin_secret: Option<String>,
    /// Required by `/api/tick` when set. Left open when unset, so a local run
    /// or a lazy-tick-only deployment needs no ceremony.
    pub cron_secret: Option<String>,
    /// Seeds a brand-new Realm. Derived from the clock when unset, so a fresh
    /// deployment is not identical to every other fresh deployment.
    pub seed: Option<u64>,
    pub pacing: Pacing,
    /// Directory of static files to serve, for local development. On Vercel the
    /// platform serves these itself.
    pub static_dir: Option<String>,
    pub port: u16,
}

impl Config {
    pub fn from_env() -> Result<Self> {
        let database_url = std::env::var("DATABASE_URL")
            .or_else(|_| std::env::var("POSTGRES_URL"))
            .context("DATABASE_URL (or POSTGRES_URL) must be set")?;
        let database_url = secure_database_url(&database_url);

        let mut pacing = match env("PACING").as_deref() {
            Some("irc") => Pacing::IRC,
            Some("web") | None => Pacing::WEB,
            Some(other) => anyhow::bail!("PACING must be 'web' or 'irc', got {other:?}"),
        };
        // The heartbeat is the knob most likely to be retuned in the field, so
        // it can be moved without a redeploy of the game logic.
        if let Some(raw) = env("TICK_SECS") {
            pacing.tick_secs =
                raw.parse().context("TICK_SECS must be a whole number of seconds")?;
            anyhow::ensure!(pacing.tick_secs > 0, "TICK_SECS must be positive");
        }
        if let Some(raw) = env("MAX_CATCHUP_TICKS") {
            pacing.max_catchup_ticks =
                raw.parse().context("MAX_CATCHUP_TICKS must be a whole number")?;
        }

        Ok(Self {
            database_url,
            max_connections: env("DB_MAX_CONNECTIONS")
                .and_then(|v| v.parse().ok())
                .unwrap_or(3),
            invite_keys: parse_invite_keys(env("INVITE_KEY").as_deref()),
            join_attempts_per_window: env("JOIN_ATTEMPTS_PER_WINDOW")
                .and_then(|v| v.parse().ok())
                .unwrap_or(10),
            join_window_secs: env("JOIN_WINDOW_SECS")
                .and_then(|v| v.parse().ok())
                .unwrap_or(3_600),
            admin_secret: env("ADMIN_SECRET"),
            cron_secret: env("CRON_SECRET"),
            seed: env("GAME_SEED").and_then(|v| v.parse().ok()),
            pacing,
            static_dir: env("STATIC_DIR"),
            port: env("PORT").and_then(|v| v.parse().ok()).unwrap_or(3000),
        })
    }
}

/// Read an environment variable, treating blank as absent — Vercel presents
/// unset project variables as empty strings.
fn env(key: &str) -> Option<String> {
    std::env::var(key).ok().map(|v| v.trim().to_string()).filter(|v| !v.is_empty())
}

/// Default the connection to a verified TLS session, and complain when the
/// operator has explicitly chosen weaker.
///
/// This matters more than it looks. sqlx defaults to `sslmode=prefer`, and for
/// every mode below `verify-ca` it installs a certificate verifier that accepts
/// *anything*. Neon's copy-paste connection string says `sslmode=require`,
/// which reads like "secure" but only means "encrypted" — it will happily
/// complete a handshake with an impostor, handing over the database password
/// and every row that follows.
///
/// An explicit `sslmode` is left alone: local development against a container
/// with no TLS at all is legitimate, and silently overriding a deliberate
/// choice would be worse than warning about it.
fn secure_database_url(url: &str) -> String {
    if let Some(mode) = existing_sslmode(url) {
        if !matches!(mode.as_str(), "verify-full" | "verify-ca") {
            tracing::warn!(
                sslmode = %mode,
                "DATABASE_URL sets sslmode={mode}, which does not verify the \
                 server's certificate; use sslmode=verify-full for anything \
                 that is not a local database"
            );
        }
        return url.to_string();
    }

    let separator = if url.contains('?') { '&' } else { '?' };
    format!("{url}{separator}sslmode=verify-full")
}

fn existing_sslmode(url: &str) -> Option<String> {
    let (_, query) = url.split_once('?')?;
    query
        .split('&')
        .filter_map(|pair| pair.split_once('='))
        .find(|(key, _)| key.eq_ignore_ascii_case("sslmode"))
        .map(|(_, value)| value.to_ascii_lowercase())
}

/// Split a comma-separated key list, discarding blanks.
///
/// Blanks matter: a trailing comma or a half-cleared variable would otherwise
/// leave an empty string in the list, and an empty key would accept a request
/// that supplied no key at all.
fn parse_invite_keys(raw: Option<&str>) -> Vec<String> {
    raw.map(|value| {
        value
            .split(',')
            .map(str::trim)
            .filter(|key| !key.is_empty())
            .map(str::to_string)
            .collect()
    })
    .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Environment access is process-global, so these run under one lock and
    /// put back what they found.
    fn with_env<T>(vars: &[(&str, Option<&str>)], body: impl FnOnce() -> T) -> T {
        use std::sync::{Mutex, OnceLock};
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        let _guard = LOCK.get_or_init(|| Mutex::new(())).lock().unwrap_or_else(|e| e.into_inner());

        let saved: Vec<(String, Option<String>)> = vars
            .iter()
            .map(|(key, _)| ((*key).to_string(), std::env::var(key).ok()))
            .collect();
        for (key, value) in vars {
            match value {
                Some(v) => unsafe { std::env::set_var(key, v) },
                None => unsafe { std::env::remove_var(key) },
            }
        }
        let result = body();
        for (key, value) in saved {
            match value {
                Some(v) => unsafe { std::env::set_var(&key, v) },
                None => unsafe { std::env::remove_var(&key) },
            }
        }
        result
    }

    const CLEAR: &[(&str, Option<&str>)] = &[
        ("DATABASE_URL", None),
        ("POSTGRES_URL", None),
        ("PACING", None),
        ("TICK_SECS", None),
        ("MAX_CATCHUP_TICKS", None),
        ("ADMIN_SECRET", None),
        ("CRON_SECRET", None),
        ("GAME_SEED", None),
        ("DB_MAX_CONNECTIONS", None),
        ("STATIC_DIR", None),
        ("PORT", None),
    ];

    fn base(extra: &[(&str, Option<&str>)]) -> Vec<(&'static str, Option<&'static str>)> {
        let mut vars: Vec<(&'static str, Option<&'static str>)> = CLEAR.to_vec();
        vars.push(("DATABASE_URL", Some("postgres://localhost/dragon")));
        // Leaked so the borrow outlives the call; only ever a handful in tests.
        for (key, value) in extra {
            let key: &'static str = Box::leak(key.to_string().into_boxed_str());
            let value = value.map(|v| &*Box::leak(v.to_string().into_boxed_str()));
            vars.push((key, value));
        }
        vars
    }

    #[test]
    fn an_unspecified_sslmode_defaults_to_a_verified_session() {
        // sqlx defaults to `prefer`, which installs a verifier that accepts any
        // certificate. Silence must not mean "encrypted but unauthenticated".
        assert_eq!(
            secure_database_url("postgres://u:p@db.neon.tech/dragon"),
            "postgres://u:p@db.neon.tech/dragon?sslmode=verify-full"
        );
        // An existing query string is appended to, not clobbered.
        assert_eq!(
            secure_database_url("postgres://u:p@db.neon.tech/dragon?application_name=x"),
            "postgres://u:p@db.neon.tech/dragon?application_name=x&sslmode=verify-full"
        );
    }

    #[test]
    fn an_explicit_sslmode_is_always_respected() {
        // Local development against a container with no TLS is legitimate, and
        // silently overriding a deliberate choice would be worse than warning.
        for mode in ["disable", "require", "prefer", "allow", "verify-ca", "verify-full"] {
            let url = format!("postgres://u:p@localhost/dragon?sslmode={mode}");
            assert_eq!(secure_database_url(&url), url, "rewrote sslmode={mode}");
        }
    }

    #[test]
    fn sslmode_is_read_case_insensitively_and_from_any_position() {
        assert_eq!(
            existing_sslmode("postgres://h/d?SSLMode=Verify-Full").as_deref(),
            Some("verify-full")
        );
        assert_eq!(
            existing_sslmode("postgres://h/d?a=1&sslmode=disable&b=2").as_deref(),
            Some("disable")
        );
        assert_eq!(existing_sslmode("postgres://h/d"), None);
        // A parameter that merely ends in "sslmode" is not the one we mean.
        assert_eq!(existing_sslmode("postgres://h/d?xsslmode=disable"), None);
    }

    #[test]
    fn a_database_url_is_required() {
        with_env(CLEAR, || {
            assert!(Config::from_env().is_err());
        });
    }

    #[test]
    fn postgres_url_is_accepted_as_an_alias() {
        let vars = [CLEAR, &[("POSTGRES_URL", Some("postgres://neon/db"))][..]].concat();
        with_env(&vars, || {
            let config = Config::from_env().unwrap();
            // Picked up from the alias, and hardened on the way through.
            assert_eq!(config.database_url, "postgres://neon/db?sslmode=verify-full");
        });
    }

    #[test]
    fn defaults_are_the_web_retune_and_a_small_pool() {
        with_env(&base(&[]), || {
            let config = Config::from_env().unwrap();
            assert_eq!(config.pacing, Pacing::WEB);
            assert_eq!(config.max_connections, 3);
            assert_eq!(config.port, 3000);
            assert!(config.admin_secret.is_none());
            assert!(config.cron_secret.is_none());
        });
    }

    #[test]
    fn pacing_can_be_switched_to_the_original() {
        with_env(&base(&[("PACING", Some("irc"))]), || {
            assert_eq!(Config::from_env().unwrap().pacing, Pacing::IRC);
        });
    }

    #[test]
    fn an_unknown_pacing_is_rejected_rather_than_ignored() {
        with_env(&base(&[("PACING", Some("fast"))]), || {
            assert!(Config::from_env().is_err());
        });
    }

    #[test]
    fn the_heartbeat_can_be_retuned_without_a_redeploy() {
        with_env(&base(&[("TICK_SECS", Some("60")), ("MAX_CATCHUP_TICKS", Some("90"))]), || {
            let config = Config::from_env().unwrap();
            assert_eq!(config.pacing.tick_secs, 60);
            assert_eq!(config.pacing.max_catchup_ticks, 90);
            // Everything else keeps the retune's values.
            assert_eq!(config.pacing.idle_max_secs, Pacing::WEB.idle_max_secs);
        });
    }

    #[test]
    fn a_nonsensical_heartbeat_is_refused() {
        with_env(&base(&[("TICK_SECS", Some("0"))]), || {
            assert!(Config::from_env().is_err());
        });
        with_env(&base(&[("TICK_SECS", Some("soon"))]), || {
            assert!(Config::from_env().is_err());
        });
    }

    #[test]
    fn blank_variables_count_as_unset() {
        // Vercel hands unset project variables through as empty strings.
        with_env(&base(&[("ADMIN_SECRET", Some("   ")), ("PACING", Some(""))]), || {
            let config = Config::from_env().unwrap();
            assert!(config.admin_secret.is_none());
            assert_eq!(config.pacing, Pacing::WEB);
        });
    }

    #[test]
    fn secrets_and_seed_are_read_when_present() {
        with_env(
            &base(&[
                ("ADMIN_SECRET", Some("hunter2")),
                ("CRON_SECRET", Some("beat")),
                ("GAME_SEED", Some("4242")),
            ]),
            || {
                let config = Config::from_env().unwrap();
                assert_eq!(config.admin_secret.as_deref(), Some("hunter2"));
                assert_eq!(config.cron_secret.as_deref(), Some("beat"));
                assert_eq!(config.seed, Some(4242));
            },
        );
    }
}
