//! Who is asking.
//!
//! IRC authenticated a nick for free — the server vouched for it. Over HTTP
//! nothing does, so joining mints a bearer token and the database keeps only
//! its SHA-256 hash. That means a leaked backup cannot be played with, and it
//! means we can never mail anyone their token back.
//!
//! Credentials live in their own table rather than inside the game state, so
//! they are not re-serialised on every tick — and so a player who is purged or
//! ascends keeps their token and can walk back in with a fresh character.

use anyhow::{Context, Result};
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use subtle::ConstantTimeEq;

use crate::error::{ApiError, ApiResult};

/// 256 bits, so the token is not worth guessing.
const TOKEN_BYTES: usize = 32;

/// What the `player` table stores in place of a token.
fn digest(token: &str) -> [u8; 32] {
    Sha256::digest(token.as_bytes()).into()
}

pub const MAX_NICK_LEN: usize = 24;

/// Check a requested name.
///
/// Deliberately strict: names are shown to other players, used as map keys, and
/// embedded in feed lines. Restricting to ASCII letters, digits, `_` and `-`
/// shuts the whole Unicode confusable surface — no normalisation differences,
/// no bidirectional overrides, no zero-width characters, no cross-script
/// lookalikes — and refuses rather than silently rewriting what was typed.
///
/// Two things this deliberately does *not* do. Names differing only by case
/// (`Absalom` / `absalom`) are distinct, as are ASCII lookalikes such as
/// `Absa1om`. Both are consistently distinct everywhere — credential table,
/// game state, feed filter — so neither can act as the other; they are a
/// display-impersonation nuisance, not an authorization gap.
pub fn validate_nick(nick: &str) -> ApiResult<String> {
    let nick = nick.trim();
    if nick.is_empty() {
        return Err(ApiError::BadRequest("Your name cannot be empty.".into()));
    }
    if nick.chars().count() > MAX_NICK_LEN {
        return Err(ApiError::BadRequest(format!(
            "Your name can be at most {MAX_NICK_LEN} characters."
        )));
    }
    if !nick.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-') {
        return Err(ApiError::BadRequest(
            "Your name may only contain letters, digits, '_' and '-'.".into(),
        ));
    }
    if !nick.starts_with(|c: char| c.is_ascii_alphanumeric()) {
        return Err(ApiError::BadRequest("Your name must start with a letter or digit.".into()));
    }
    Ok(nick.to_string())
}

/// Claim a name and mint its token, inside a caller-supplied transaction.
///
/// Returns `None` when the name is already taken, and the token exactly once
/// otherwise — only its digest is stored, so it can never be recovered.
///
/// Taking the transaction rather than the pool is the point: the credential and
/// the character have to be created together. Committing the credential
/// separately means a later failure leaves a name claimed by a token nobody
/// holds — permanently unusable *and* permanently unclaimable.
pub async fn claim(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    nick: &str,
) -> ApiResult<Option<String>> {
    let token = mint_token()?;
    let hash = digest(&token);

    let inserted = sqlx::query(
        "insert into player (nick, token_hash) values ($1, $2)
         on conflict (nick) do nothing",
    )
    .bind(nick)
    .bind(hash.as_slice())
    .execute(&mut **tx)
    .await
    .context("could not register the player")?;

    Ok((inserted.rows_affected() == 1).then_some(token))
}

/// Whether a supplied signup key matches any configured one.
///
/// Every key is compared even after a match, so the time taken does not reveal
/// which key was the right one — that would leak the position of the live key
/// during a rotation, when both old and new are accepted.
pub fn invite_accepted(configured: &[String], provided: &str) -> bool {
    configured
        .iter()
        .fold(false, |matched, key| secret_matches(key, provided) | matched)
}

/// Resolve a bearer token to the name it was issued for.
pub async fn authenticate(pool: &PgPool, token: &str) -> ApiResult<String> {
    let token = token.trim();
    // Reject anything that is not a plausible token before touching the
    // database, so a flood of junk costs nothing.
    if token.len() != TOKEN_BYTES * 2 || hex::decode(token).is_err() {
        return Err(ApiError::Unauthorized("That is not a valid token.".into()));
    }

    let hash = digest(token);
    let nick: Option<String> =
        sqlx::query_scalar("select nick from player where token_hash = $1")
            .bind(hash.as_slice())
            .fetch_optional(pool)
            .await
            .context("could not look up the token")?;

    nick.ok_or_else(|| {
        ApiError::Unauthorized("Nobody in the Realm answers to that token.".into())
    })
}

/// Pull a bearer token out of an `Authorization` header.
pub fn bearer(headers: &axum::http::HeaderMap) -> ApiResult<&str> {
    let header = headers
        .get(axum::http::header::AUTHORIZATION)
        .ok_or_else(|| ApiError::Unauthorized("Sign in to do that.".into()))?
        .to_str()
        .map_err(|_| ApiError::Unauthorized("Malformed Authorization header.".into()))?;

    header
        .strip_prefix("Bearer ")
        .or_else(|| header.strip_prefix("bearer "))
        .map(str::trim)
        .ok_or_else(|| {
            ApiError::Unauthorized("Authorization must be a Bearer token.".into())
        })
}

/// Compare a request's secret against the configured one.
///
/// Constant-time, because unlike a token lookup this is a direct string
/// comparison an attacker could otherwise time. Length is compared first and
/// in the clear, which leaks only the secret's length.
pub fn secret_matches(expected: &str, provided: &str) -> bool {
    let (expected, provided) = (expected.as_bytes(), provided.as_bytes());
    expected.len() == provided.len() && expected.ct_eq(provided).into()
}

fn mint_token() -> Result<String> {
    let mut bytes = [0u8; TOKEN_BYTES];
    getrandom::fill(&mut bytes).context("no system randomness available")?;
    Ok(hex::encode(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::{HeaderMap, HeaderValue, header::AUTHORIZATION};

    #[test]
    fn ordinary_names_are_accepted() {
        for nick in ["Absalom", "bot00", "a", "some-one", "under_score", "X9"] {
            assert_eq!(validate_nick(nick).unwrap(), nick, "rejected {nick}");
        }
    }

    #[test]
    fn surrounding_whitespace_is_trimmed() {
        assert_eq!(validate_nick("  Absalom \n").unwrap(), "Absalom");
    }

    #[test]
    fn names_that_could_impersonate_are_refused() {
        // Empty, overlong, punctuation, spaces inside, and anything that does
        // not begin with an alphanumeric.
        for nick in [
            "",
            "   ",
            "a name with spaces",
            "-leading-dash",
            "_leading_underscore",
            "emoji🐉",
            "semi;colon",
            "quote\"mark",
            "new\nline",
            "zero\u{200b}width",
        ] {
            assert!(validate_nick(nick).is_err(), "accepted {nick:?}");
        }
        assert!(validate_nick(&"a".repeat(MAX_NICK_LEN + 1)).is_err());
        assert!(validate_nick(&"a".repeat(MAX_NICK_LEN)).is_ok());
    }

    #[test]
    fn a_minted_token_is_long_random_hex() {
        let a = mint_token().unwrap();
        let b = mint_token().unwrap();
        assert_eq!(a.len(), TOKEN_BYTES * 2);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, b, "tokens must not repeat");
        assert!(hex::decode(&a).is_ok());
    }

    #[test]
    fn tokens_are_stored_only_as_a_digest() {
        let token = mint_token().unwrap();
        let stored = digest(&token);
        assert_ne!(hex::encode(stored), token, "the stored value must not be the token");
        // And the digest is stable, or every account would be locked out.
        assert_eq!(digest(&token), stored);
        assert_ne!(digest(&token), digest(&mint_token().unwrap()));
    }

    #[test]
    fn the_digest_is_the_pinned_sha256() {
        // A published vector, so a dependency bump that changed the algorithm
        // would fail here rather than silently invalidating every account.
        assert_eq!(
            hex::encode(digest("abc")),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn bearer_tokens_are_extracted_case_insensitively() {
        let mut headers = HeaderMap::new();
        headers.insert(AUTHORIZATION, HeaderValue::from_static("Bearer abc123"));
        assert_eq!(bearer(&headers).unwrap(), "abc123");

        headers.insert(AUTHORIZATION, HeaderValue::from_static("bearer  spaced  "));
        assert_eq!(bearer(&headers).unwrap(), "spaced");
    }

    #[test]
    fn a_missing_or_wrong_scheme_is_unauthorized() {
        assert!(matches!(bearer(&HeaderMap::new()), Err(ApiError::Unauthorized(_))));

        let mut headers = HeaderMap::new();
        headers.insert(AUTHORIZATION, HeaderValue::from_static("Basic abc123"));
        assert!(matches!(bearer(&headers), Err(ApiError::Unauthorized(_))));
    }

    #[test]
    fn secret_comparison_is_length_safe_and_correct() {
        assert!(secret_matches("hunter2", "hunter2"));
        assert!(!secret_matches("hunter2", "hunter3"));
        assert!(!secret_matches("hunter2", "hunter"));
        assert!(!secret_matches("hunter2", "hunter22"));
        assert!(!secret_matches("hunter2", ""));
        assert!(secret_matches("", ""));
    }
}
