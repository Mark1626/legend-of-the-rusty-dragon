//! Turns that produce an enormous number of feed lines.
//!
//! These began as reproductions of an unfixed defect found during the security
//! review; they now guard the fix.

use std::sync::atomic::{AtomicU32, Ordering};

use dragon_api::Store;
use dragon_core::config::Pacing;
use dragon_core::step::Input;
use sqlx::AssertSqlSafe;
use sqlx::postgres::PgPoolOptions;

static COUNTER: AtomicU32 = AtomicU32::new(0);

async fn fresh() -> Option<Store> {
    let url = std::env::var("TEST_DATABASE_URL").ok().filter(|v| !v.trim().is_empty())?;
    let schema = format!("wedge_{}_{}", std::process::id(), COUNTER.fetch_add(1, Ordering::Relaxed));
    let pool = PgPoolOptions::new()
        .max_connections(4)
        .after_connect({
            let schema = schema.clone();
            move |conn, _| {
                let schema = schema.clone();
                Box::pin(async move {
                    sqlx::query(AssertSqlSafe(format!("set search_path to {schema}")))
                        .execute(&mut *conn)
                        .await?;
                    Ok(())
                })
            }
        })
        .connect(&url)
        .await
        .unwrap();
    sqlx::query(AssertSqlSafe(format!("create schema if not exists {schema}")))
        .execute(&pool)
        .await
        .unwrap();
    let store = Store::from_pool(pool, Pacing::WEB, Some(1));
    store.migrate().await.unwrap();
    Some(store)
}

/// A turn far past Postgres's bind-parameter ceiling still commits.
///
/// Postgres encodes a statement's parameter count as an int16, so 65535 is the
/// hard limit — 16383 feed lines at four parameters each. Before `append_feed`
/// chunked its inserts, a bigger turn failed *inside* the transaction and
/// rolled the whole turn back; the next request then recomputed the identical
/// catch-up from the identical unchanged row and failed the same way, so
/// nothing ever committed again. It was unrecoverable through the API.
///
/// The inactivity purge is the reachable trigger, narrating one line per
/// departing player with no cap.
#[tokio::test]
async fn a_mass_purge_commits_rather_than_wedging_the_realm() {
    let Some(store) = fresh().await else {
        eprintln!("skipping: TEST_DATABASE_URL is not set");
        return;
    };

    // Open the Realm, then stuff it with players well past the 16383 line cap.
    store.commit_turn(Input::CatchUp).await.unwrap();
    let (mut state, now) = store.snapshot().await.unwrap().unwrap();

    const CROWD: usize = 20_000;
    let stale = now - state.pacing.purge_after_secs - 1;
    for i in 0..CROWD {
        let nick = format!("flood{i:05}");
        let mut user = dragon_core::User::new(&nick, stale);
        user.last_quest_at = stale;
        state.users.insert(nick, user);
    }
    // Purge only runs inside a tick, so make the clock owe one.
    state.last_tick_at = now - state.pacing.tick_secs * 2;

    // Write the crowded state straight in, standing in for the flood of joins
    // that would produce it.
    sqlx::query("update game set state = $1 where id = 1")
        .bind(sqlx::types::Json(&state))
        .execute(store.pool())
        .await
        .unwrap();

    println!("seeded {CROWD} stale players; next turn should purge them all");

    // Any request now triggers the purge, which broadcasts CROWD lines.
    let outcome = store.commit_turn(Input::CatchUp).await;

    match &outcome {
        Ok(committed) => {
            println!(
                "turn COMMITTED with {} feed lines (params = {})",
                committed.turn.out.feed.len(),
                committed.turn.out.feed.len() * 4
            );
        }
        Err(error) => {
            println!("turn FAILED: {error:#}");
        }
    }

    // Whatever happened, check whether the game can still take a turn at all.
    let second = store.commit_turn(Input::CatchUp).await;
    println!("follow-up turn: {}", if second.is_ok() { "ok" } else { "ALSO FAILED" });

    assert!(
        outcome.is_ok() && second.is_ok(),
        "the Realm wedged: first={:?} second={:?}",
        outcome.err().map(|e| format!("{e:#}")),
        second.err().map(|e| format!("{e:#}")),
    );
}
