//! What happens when several callers want the one game row at once.
//!
//! The whole world is a single Postgres row and every turn locks it, so these
//! are the tests that matter for availability. Needs `TEST_DATABASE_URL`.

use std::sync::atomic::{AtomicU32, Ordering};

use dragon_api::Store;
use dragon_core::config::Pacing;
use dragon_core::step::Input;
use sqlx::postgres::PgPoolOptions;
use sqlx::{Executor, PgPool, Row};

static COUNTER: AtomicU32 = AtomicU32::new(0);

/// A pool in its own schema, carrying the session limits the deployment uses.
///
/// `lock_timeout` is overridden to something short so a contention test does
/// not spend two seconds proving the point.
async fn fresh(lock_timeout: &str) -> Option<(Store, PgPool)> {
    let url = std::env::var("TEST_DATABASE_URL").ok().filter(|v| !v.trim().is_empty())?;
    let schema =
        format!("contend_{}_{}", std::process::id(), COUNTER.fetch_add(1, Ordering::Relaxed));

    let settings = format!(
        "set search_path to {schema}; {} set lock_timeout = '{lock_timeout}';",
        dragon_api::store::SESSION_SETTINGS
    );
    let pool = PgPoolOptions::new()
        .max_connections(12)
        .after_connect(move |conn, _| {
            let settings = settings.clone();
            Box::pin(async move {
                conn.execute(settings.as_str()).await?;
                Ok(())
            })
        })
        .connect(&url)
        .await
        .expect("could not connect to TEST_DATABASE_URL");

    sqlx::query(&format!("create schema if not exists {schema}"))
        .execute(&pool)
        .await
        .unwrap();

    let store = Store::from_pool(pool.clone(), Pacing::WEB, Some(7));
    store.migrate().await.unwrap();
    Some((store, pool))
}

async fn game_version(pool: &PgPool) -> i64 {
    sqlx::query("select version from game where id = 1")
        .fetch_one(pool)
        .await
        .unwrap()
        .get("version")
}

/// Wind the clock back so a catch-up genuinely owes ticks.
async fn owe_ticks(pool: &PgPool, seconds: i64) {
    sqlx::query(
        "update game set state = jsonb_set(state, '{last_tick_at}',
         to_jsonb((state->>'last_tick_at')::bigint - $1)) where id = 1",
    )
    .bind(seconds)
    .execute(pool)
    .await
    .unwrap();
}

macro_rules! store {
    ($timeout:expr) => {
        match fresh($timeout).await {
            Some(pair) => pair,
            None => {
                eprintln!("skipping: TEST_DATABASE_URL is not set");
                return;
            }
        }
    };
}

#[tokio::test]
async fn a_stranded_lock_produces_a_retryable_error_not_an_indefinite_wait() {
    // Stands in for an invocation killed mid-transaction: a client that holds
    // the game row and never comes back. Without `lock_timeout` every later
    // writer would block forever behind it.
    let (store, pool) = store!("300ms");
    store.commit_turn(Input::CatchUp).await.unwrap();

    let mut squatter = pool.begin().await.unwrap();
    sqlx::query("select state from game where id = 1 for update")
        .fetch_one(&mut *squatter)
        .await
        .unwrap();

    let started = std::time::Instant::now();
    let blocked = store.commit_turn(Input::Join { nick: "Absalom".into() }).await;
    let waited = started.elapsed();

    let error = blocked.expect_err("the lock was held; this should not have succeeded");
    assert!(
        waited < std::time::Duration::from_secs(3),
        "waited {waited:?} instead of giving up quickly"
    );

    // And it is classified as retryable rather than as a fault.
    let api: dragon_api::error::ApiError = error.into();
    assert!(
        matches!(api, dragon_api::error::ApiError::Busy(_)),
        "a lock timeout should be Busy (503), got {api:?}"
    );

    // Once the squatter leaves, the Realm carries on.
    squatter.rollback().await.unwrap();
    store.commit_turn(Input::Join { nick: "Absalom".into() }).await.unwrap();
}

#[tokio::test]
async fn concurrent_catch_ups_at_a_tick_boundary_write_exactly_once() {
    // The thundering herd. Every caller reads the same stale row, sees ticks
    // owed and escalates to a write; re-checking under the lock is what stops
    // all but the first from rewriting the row with identical bytes.
    let (store, pool) = store!("5s");
    store.commit_turn(Input::CatchUp).await.unwrap();
    owe_ticks(&pool, 100_000).await;

    let before = game_version(&pool).await;

    let herd = (0..10).map(|_| {
        let store = store.clone();
        tokio::spawn(async move { store.commit_turn(Input::CatchUp).await })
    });
    let mut replayed_by = 0;
    for handle in herd {
        let committed = handle.await.unwrap().expect("a catch-up failed");
        if committed.turn.ticks_replayed > 0 {
            replayed_by += 1;
        }
    }

    assert_eq!(replayed_by, 1, "more than one caller replayed the same ticks");
    assert_eq!(
        game_version(&pool).await,
        before + 1,
        "the row was rewritten by callers that had nothing to do"
    );
}

#[tokio::test]
async fn a_settled_clock_makes_a_catch_up_free() {
    let (store, pool) = store!("5s");
    store.commit_turn(Input::CatchUp).await.unwrap();

    let before = game_version(&pool).await;
    for _ in 0..30 {
        let committed = store.commit_turn(Input::CatchUp).await.unwrap();
        assert_eq!(committed.turn.ticks_replayed, 0);
        assert!(committed.cursor.is_none());
    }
    assert_eq!(game_version(&pool).await, before, "a no-op catch-up rewrote the row");
}

#[tokio::test]
async fn real_commands_still_write_when_the_clock_is_settled() {
    // The guard must apply only to catch-ups; a player's turn always commits.
    let (store, pool) = store!("5s");
    store.commit_turn(Input::CatchUp).await.unwrap();
    let before = game_version(&pool).await;

    store.commit_join("Absalom").await.unwrap().expect("name is free");
    assert_eq!(game_version(&pool).await, before + 1);
}

#[tokio::test]
async fn the_realm_still_opens_itself_when_the_first_request_is_a_catch_up() {
    // The skip must not fire before the row exists, or the game would never
    // be created.
    let (store, pool) = store!("5s");
    let committed = store.commit_turn(Input::CatchUp).await.unwrap();
    assert_eq!(committed.state.bboard.len(), 16);
    assert_eq!(game_version(&pool).await, 1);
    assert!(store.snapshot().await.unwrap().is_some());
}
