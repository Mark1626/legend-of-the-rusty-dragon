//! Integration tests against a real Postgres.
//!
//! Set `TEST_DATABASE_URL` to run these; without it they skip, so `cargo test`
//! stays useful on a machine with no database. A throwaway server is enough:
//!
//! ```text
//! docker run -d --name dragon-pg -e POSTGRES_PASSWORD=dragon \
//!     -e POSTGRES_USER=dragon -e POSTGRES_DB=dragon -p 55432:5432 postgres:16-alpine
//! export TEST_DATABASE_URL=postgres://dragon:dragon@localhost:55432/dragon
//! ```
//!
//! Every test gets its own schema, so they can run concurrently without
//! fighting over the single game row.

use std::sync::atomic::{AtomicU32, Ordering};

use dragon_api::Store;
use dragon_api::auth;
use dragon_core::config::Pacing;
use dragon_core::quest::QuestId;
use dragon_core::step::{AdminCommand, Input};
use sqlx::Executor;
use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;

static COUNTER: AtomicU32 = AtomicU32::new(0);

fn database_url() -> Option<String> {
    std::env::var("TEST_DATABASE_URL")
        .ok()
        .filter(|v| !v.trim().is_empty())
}

/// A pool bound to a fresh, empty schema.
async fn fresh_pool() -> Option<(PgPool, String)> {
    let url = database_url()?;
    let schema = format!(
        "dragon_test_{}_{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    );

    let pool = PgPoolOptions::new()
        .max_connections(6)
        .after_connect({
            let schema = schema.clone();
            move |conn, _| {
                let schema = schema.clone();
                Box::pin(async move {
                    conn.execute(format!("set search_path to {schema}").as_str()).await?;
                    Ok(())
                })
            }
        })
        .connect(&url)
        .await
        .expect("could not connect to TEST_DATABASE_URL");

    // Audited: `schema` is built from the process id and a counter, never from
    // anything a caller supplies. An identifier cannot be a bind parameter.
    sqlx::query(&format!("create schema if not exists {schema}"))
        .execute(&pool)
        .await
        .expect("could not create the test schema");
    Some((pool, schema))
}

async fn fresh_store(pacing: Pacing) -> Option<(Store, PgPool)> {
    let (pool, _) = fresh_pool().await?;
    let store = Store::from_pool(pool.clone(), pacing, Some(4242));
    store.migrate().await.expect("migration failed");
    Some((store, pool))
}

/// Skip the test body when no database is configured.
macro_rules! store {
    ($pacing:expr) => {
        match fresh_store($pacing).await {
            Some(pair) => pair,
            None => {
                eprintln!("skipping: TEST_DATABASE_URL is not set");
                return;
            }
        }
    };
    () => {
        store!(Pacing::WEB)
    };
}

#[tokio::test]
async fn migration_is_idempotent() {
    let (store, _pool) = store!();
    // Cold starts run it every time.
    for _ in 0..3 {
        store
            .migrate()
            .await
            .expect("re-running the migration failed");
    }
}

#[tokio::test]
async fn the_first_request_opens_the_realm() {
    let (store, _pool) = store!();
    assert!(
        store.snapshot().await.unwrap().is_none(),
        "nothing exists yet"
    );

    let committed = store.commit_turn(Input::CatchUp).await.unwrap();
    assert_eq!(committed.state.bboard.len(), 16);
    assert_eq!(committed.state.shop.total_items(), 12);
    assert!(
        committed.now > 1_700_000_000,
        "the clock came from the database"
    );

    let said = committed.turn.out.transcript().join(" ");
    assert!(said.contains("The Legend of the Rusty Dragon"), "{said}");

    // And the opening announcements reached the feed.
    let history = store.feed(0, 100, None).await.unwrap();
    assert!(!history.is_empty());
    assert!(history.iter().all(|entry| entry.line.at.is_some()));
    assert_eq!(committed.cursor, history.last().map(|e| e.id));
}

#[tokio::test]
async fn state_survives_a_round_trip_through_the_database() {
    let (store, _pool) = store!();
    let first = store.commit_turn(Input::CatchUp).await.unwrap();
    let (loaded, _) = store.snapshot().await.unwrap().expect("the realm exists");

    assert_eq!(loaded.tick_turn, first.state.tick_turn);
    assert_eq!(loaded.rng_seed, first.state.rng_seed);
    assert_eq!(loaded.bboard, first.state.bboard);
    assert_eq!(loaded.shop, first.state.shop);
    assert_eq!(loaded.started_at, first.state.started_at);
}

#[tokio::test]
async fn a_player_can_join_quest_and_buy() {
    let (store, pool) = store!();

    let (joined, token) = store
        .commit_join("Absalom")
        .await
        .unwrap()
        .expect("name is free");
    assert_eq!(auth::authenticate(&pool, &token).await.unwrap(), "Absalom");
    assert!(joined.state.users.contains_key("Absalom"));

    // Give them something to spend, then buy the first thing in stock.
    let item = joined.state.shop.stock()[0].0;
    let mut state = joined.state.clone();
    state.users.get_mut("Absalom").unwrap().money = 1_000_000;
    // Persist the doctored purse through an ordinary turn path.
    let quest = state.bboard.posted()[0].0;
    drop(state);

    let bought = store
        .commit_turn(Input::Buy {
            nick: "Absalom".into(),
            item,
        })
        .await
        .unwrap();
    // With no money the purchase is refused, privately, and nothing is lost.
    assert!(bought.turn.out.reply.iter().any(|line| {
        let text = line.plain();
        text.contains("enough money") || text.contains("no ") || text.contains("bought")
    }));

    let quested = store
        .commit_turn(Input::Quest {
            nick: "Absalom".into(),
            quest,
            strategy: Some("wise".into()),
        })
        .await
        .unwrap();
    let user = &quested.state.users["Absalom"];
    assert_eq!(user.strategy, dragon_core::PlayerStrategy::Wise);
    assert_eq!(
        user.quest_win + user.quest_lost,
        1,
        "the fight was resolved"
    );
}

#[tokio::test]
async fn a_name_can_only_be_claimed_once() {
    let (store, pool) = store!();
    let (_, first) = store
        .commit_join("Absalom")
        .await
        .unwrap()
        .expect("name is free");
    assert!(
        store.commit_join("Absalom").await.unwrap().is_none(),
        "the name is taken"
    );
    // The original token still works.
    assert_eq!(auth::authenticate(&pool, &first).await.unwrap(), "Absalom");
}

#[tokio::test]
async fn an_unknown_token_is_rejected() {
    let (store, pool) = store!();
    store.commit_join("Absalom").await.unwrap();
    // Well-formed but never issued.
    let impostor = "f".repeat(64);
    assert!(auth::authenticate(&pool, &impostor).await.is_err());
    assert!(auth::authenticate(&pool, "short").await.is_err());
    assert!(auth::authenticate(&pool, "").await.is_err());
}

#[tokio::test]
async fn the_feed_is_ordered_and_pages_by_cursor() {
    let (store, _pool) = store!();
    store
        .commit_turn(Input::Join {
            nick: "Absalom".into(),
        })
        .await
        .unwrap();
    for _ in 0..6 {
        store
            .commit_turn(Input::Admin(AdminCommand::AddQuest { level: Some(2) }))
            .await
            .unwrap();
    }

    let all = store.feed(0, 500, None).await.unwrap();
    assert!(all.len() > 5);
    assert!(all.windows(2).all(|w| w[0].id < w[1].id), "ids must ascend");
    assert!(all.iter().all(|entry| !entry.line.plain().is_empty()));

    // Paging picks up exactly where it left off, with no gap and no repeat.
    let first = store.feed(0, 3, None).await.unwrap();
    assert_eq!(first.len(), 3);
    let rest = store
        .feed(first.last().unwrap().id, 500, None)
        .await
        .unwrap();
    let stitched: Vec<i64> = first
        .iter()
        .chain(rest.iter())
        .map(|entry| entry.id)
        .collect();
    assert_eq!(stitched, all.iter().map(|e| e.id).collect::<Vec<_>>());
}

#[tokio::test]
async fn the_feed_tail_is_the_newest_lines_in_reading_order() {
    let (store, _pool) = store!();
    store
        .commit_turn(Input::Join {
            nick: "Absalom".into(),
        })
        .await
        .unwrap();
    for _ in 0..6 {
        store
            .commit_turn(Input::Admin(AdminCommand::AddQuest { level: Some(2) }))
            .await
            .unwrap();
    }

    let all = store.feed(0, 500, None).await.unwrap();
    assert!(all.len() > 3);

    // The tail is exactly the end of the full history, still oldest-first.
    let tail = store.feed_tail(3, None).await.unwrap();
    assert_eq!(
        tail.iter().map(|e| e.id).collect::<Vec<_>>(),
        all[all.len() - 3..]
            .iter()
            .map(|e| e.id)
            .collect::<Vec<_>>()
    );

    // Asking for more than exists returns everything, not an error.
    let generous = store.feed_tail(500, None).await.unwrap();
    assert_eq!(generous.len(), all.len());

    // The actor filter applies before the tail is cut, so a quiet player's
    // few lines are found even under a flood of other news.
    let mine = store.feed_tail(500, Some("Absalom")).await.unwrap();
    assert!(!mine.is_empty());
    assert!(
        mine.iter()
            .all(|entry| entry.line.actors.iter().any(|a| a == "Absalom"))
    );
}

#[tokio::test]
async fn the_feed_can_be_narrowed_to_one_player() {
    let (store, _pool) = store!();
    store
        .commit_turn(Input::Join {
            nick: "Absalom".into(),
        })
        .await
        .unwrap();
    store
        .commit_turn(Input::Join {
            nick: "Barnabas".into(),
        })
        .await
        .unwrap();

    let mine = store.feed(0, 500, Some("Absalom")).await.unwrap();
    assert!(!mine.is_empty());
    assert!(
        mine.iter()
            .all(|entry| entry.line.actors.iter().any(|a| a == "Absalom")),
        "the filter returned lines that do not name the player"
    );
    assert!(
        mine.iter()
            .all(|entry| !entry.line.plain().contains("Barnabas")),
        "the filter leaked another player's news"
    );
}

#[tokio::test]
async fn line_styling_and_timestamps_survive_storage() {
    let (store, _pool) = store!();
    store
        .commit_turn(Input::Join {
            nick: "Absalom".into(),
        })
        .await
        .unwrap();

    let entry = store
        .feed(0, 500, Some("Absalom"))
        .await
        .unwrap()
        .into_iter()
        .find(|e| e.line.plain().contains("has entered the Realm"))
        .expect("the arrival should have been broadcast");

    assert_eq!(entry.line.kind, dragon_core::out::Kind::System);
    assert_eq!(entry.line.actors, vec!["Absalom"]);
    assert!(entry.line.at.is_some(), "the stored line kept its time");
    assert!(
        entry
            .line
            .spans
            .iter()
            .any(|span| span.bold && span.text == "Absalom"),
        "bold styling was lost in storage"
    );
}

#[tokio::test]
async fn concurrent_turns_are_serialised_rather_than_lost() {
    // The reason the row lock exists: parallel writers must not clobber each
    // other's version of the world.
    let (store, _pool) = store!();
    store.commit_turn(Input::CatchUp).await.unwrap();

    let names: Vec<String> = (0..8).map(|i| format!("rider{i:02}")).collect();
    let joins = names.iter().map(|nick| {
        let store = store.clone();
        let nick = nick.clone();
        tokio::spawn(async move { store.commit_turn(Input::Join { nick }).await })
    });
    for handle in joins {
        handle.await.expect("task panicked").expect("turn failed");
    }

    let (state, _) = store.snapshot().await.unwrap().unwrap();
    for nick in &names {
        assert!(state.users.contains_key(nick), "{nick} was lost to a race");
    }
    assert_eq!(state.users.len(), 8);
}

#[tokio::test]
async fn concurrent_turns_produce_a_gapless_feed() {
    let (store, _pool) = store!();
    store.commit_turn(Input::CatchUp).await.unwrap();

    let writers = (0..8).map(|_| {
        let store = store.clone();
        tokio::spawn(async move {
            store
                .commit_turn(Input::Admin(AdminCommand::AddQuest { level: Some(1) }))
                .await
        })
    });
    let mut cursors = Vec::new();
    for handle in writers {
        cursors.push(handle.await.unwrap().unwrap().cursor);
    }

    // Every writer got a distinct cursor, and the history is intact.
    let reported: Vec<i64> = cursors.into_iter().flatten().collect();
    let mut unique = reported.clone();
    unique.sort_unstable();
    unique.dedup();
    assert_eq!(
        reported.len(),
        unique.len(),
        "two turns claimed the same cursor"
    );

    let history = store.feed(0, 500, None).await.unwrap();
    assert!(history.windows(2).all(|w| w[0].id < w[1].id));
}

#[tokio::test]
async fn the_clock_catches_up_across_separate_requests() {
    // A tick length short enough that a real pause owes ticks.
    let mut pacing = Pacing::WEB;
    pacing.tick_secs = 1;
    let (store, _pool) = store!(pacing);

    let opened = store.commit_turn(Input::CatchUp).await.unwrap();
    let started = opened.state.tick_turn;

    tokio::time::sleep(std::time::Duration::from_millis(2_500)).await;

    let later = store.commit_turn(Input::CatchUp).await.unwrap();
    assert!(
        later.state.tick_turn >= started + 2,
        "expected at least two ticks, went from {started} to {}",
        later.state.tick_turn
    );
    assert!(later.turn.ticks_replayed >= 2);

    // The replayed lines are dated across the gap, not stamped all at once.
    let stamps: Vec<i64> = later
        .turn
        .out
        .feed
        .iter()
        .filter_map(|line| line.at)
        .collect();
    if stamps.len() > 1 {
        assert!(stamps.windows(2).all(|w| w[0] <= w[1]), "{stamps:?}");
    }
}

#[tokio::test]
async fn pacing_from_configuration_overrides_what_was_stored() {
    let (pool, _schema) = match fresh_pool().await {
        Some(pair) => pair,
        None => {
            eprintln!("skipping: TEST_DATABASE_URL is not set");
            return;
        }
    };
    let irc = Store::from_pool(pool.clone(), Pacing::IRC, Some(1));
    irc.migrate().await.unwrap();
    let opened = irc.commit_turn(Input::CatchUp).await.unwrap();
    assert_eq!(opened.state.pacing.tick_secs, Pacing::IRC.tick_secs);

    // Redeploy with a different heartbeat; the saved game must follow it.
    let web = Store::from_pool(pool, Pacing::WEB, Some(1));
    let (state, _) = web.snapshot().await.unwrap().unwrap();
    assert_eq!(state.pacing.tick_secs, Pacing::WEB.tick_secs);
    assert_eq!(
        state.started_at, opened.state.started_at,
        "same Realm, new pacing"
    );
}

#[tokio::test]
async fn an_admin_restart_replaces_the_realm_in_place() {
    let (store, _pool) = store!();
    store
        .commit_turn(Input::Join {
            nick: "Absalom".into(),
        })
        .await
        .unwrap();
    let before = store.snapshot().await.unwrap().unwrap().0;
    assert_eq!(before.users.len(), 1);

    let restarted = store
        .commit_turn(Input::Admin(AdminCommand::Restart { seed: 9 }))
        .await
        .unwrap();
    assert!(restarted.state.users.is_empty());
    assert_eq!(restarted.state.bboard.len(), 16);

    let (after, _) = store.snapshot().await.unwrap().unwrap();
    assert!(after.users.is_empty(), "the restart was persisted");
}

#[tokio::test]
async fn a_quest_that_does_not_exist_is_reported_without_side_effects() {
    let (store, _pool) = store!();
    store
        .commit_turn(Input::Join {
            nick: "Absalom".into(),
        })
        .await
        .unwrap();

    let committed = store
        .commit_turn(Input::Quest {
            nick: "Absalom".into(),
            quest: QuestId(0xDEAD),
            strategy: None,
        })
        .await
        .unwrap();

    assert!(
        committed
            .turn
            .out
            .reply
            .iter()
            .any(|l| l.plain().contains("unavailable")),
        "the player should be told"
    );
    let user = &committed.state.users["Absalom"];
    assert_eq!((user.quest_win, user.quest_lost), (0, 0));
}

#[tokio::test]
async fn the_cursor_tracks_the_newest_line() {
    let (store, _pool) = store!();
    assert_eq!(
        store.cursor().await.unwrap(),
        0,
        "an empty feed starts at zero"
    );

    let committed = store
        .commit_turn(Input::Join {
            nick: "Absalom".into(),
        })
        .await
        .unwrap();
    let cursor = store.cursor().await.unwrap();
    assert_eq!(Some(cursor), committed.cursor);
    assert!(
        store.feed(cursor, 100, None).await.unwrap().is_empty(),
        "nothing after it"
    );
}
