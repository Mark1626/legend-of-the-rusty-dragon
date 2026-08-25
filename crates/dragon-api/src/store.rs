//! Postgres persistence.
//!
//! # Why a lock rather than a compare-and-swap
//!
//! IRC gave the reference a single thread for free. Serverless does not: two
//! players questing at the same instant run in different processes. The whole
//! world is one row, so a turn takes a row lock for its duration —
//! `SELECT … FOR UPDATE` — and load, `step`, save and feed-append all commit or
//! roll back together. Optimistic retry would work too, but it would let both
//! callers simulate a catch-up and throw one away; on a lazily-ticked game
//! that is exactly the expensive path to duplicate.
//!
//! # Where time comes from
//!
//! `now` is read from Postgres inside the same statement that takes the lock.
//! Function instances have their own clocks, and a virtual tick clock that
//! several skewed instances take turns advancing would drift; the database is
//! the one clock everybody already agrees on.

use std::time::Duration;

use anyhow::{Context, Result};
use dragon_core::config::Pacing;
use dragon_core::out::{Kind, Line, Span};
use dragon_core::state::GameState;
use dragon_core::step::{Input, Turn};
use sqlx::postgres::{PgPool, PgPoolOptions};
use sqlx::{Executor, QueryBuilder, Row};

use crate::config::Config;

/// The single row every turn contends for.
const GAME_ID: i16 = 1;

/// How many feed lines to keep. A busy Realm produces a few thousand a day, so
/// this is roughly a week of history.
const FEED_RETENTION: i64 = 20_000;

/// How long to wait for a free connection before giving up.
const ACQUIRE_TIMEOUT_SECS: u64 = 2;

/// Session limits applied to every connection.
///
/// Without these a turn can hold the global row lock indefinitely. That matters
/// most when an invocation is killed mid-transaction: the backend keeps the
/// transaction — and the lock — open, and every other writer queues behind a
/// client that no longer exists. `lock_timeout` turns that outage into a fast
/// 503; `idle_in_transaction_session_timeout` reaps the abandoned transaction
/// itself.
///
/// Public so tests can apply the same configuration they are asserting about.
pub const SESSION_SETTINGS: &str = "set statement_timeout = '5s';
     set lock_timeout = '2s';
     set idle_in_transaction_session_timeout = '10s';";

/// How many feed lines to write per statement.
///
/// Postgres encodes a statement's bind-parameter count as an `int16`, so 65535
/// is the hard ceiling; at four parameters per line that is 16383 lines. A turn
/// *can* exceed it — the inactivity purge narrates one line per departing
/// player — and the failure mode is vicious: the INSERT fails inside the
/// transaction, the whole turn rolls back, and the next request recomputes the
/// identical catch-up and fails identically. Nothing would ever commit again.
/// Chunking well under the ceiling makes that unreachable.
const FEED_INSERT_CHUNK: usize = 4_000;

/// What a committed turn produced.
#[derive(Debug)]
pub struct Committed {
    pub turn: Turn,
    /// The world as it stands after the turn, so a caller that needs to render
    /// it does not have to read the row back.
    pub state: GameState,
    /// The id of the last feed line written, if any. A client advances its
    /// cursor to this; when nothing was broadcast there is nothing to advance.
    pub cursor: Option<i64>,
    /// The database clock at the moment of the turn.
    pub now: i64,
}

/// One line of history as stored.
#[derive(Debug, Clone)]
pub struct FeedEntry {
    pub id: i64,
    pub line: Line,
}

#[derive(Clone)]
pub struct Store {
    pool: PgPool,
    pacing: Pacing,
    seed: Option<u64>,
}

impl Store {
    pub async fn connect(config: &Config) -> Result<Self> {
        let pool = PgPoolOptions::new()
            .max_connections(config.max_connections)
            // Must sit well under the function budget. sqlx defaults to 30s,
            // which is longer than a serverless invocation lives — connection
            // starvation would surface as an opaque platform timeout instead
            // of an error anyone can act on.
            .acquire_timeout(Duration::from_secs(ACQUIRE_TIMEOUT_SECS))
            .after_connect(|conn, _| {
                Box::pin(async move {
                    conn.execute(SESSION_SETTINGS).await?;
                    Ok(())
                })
            })
            .connect(&config.database_url)
            .await
            .context("could not reach the database")?;
        Ok(Self { pool, pacing: config.pacing.clone(), seed: config.seed })
    }

    pub fn from_pool(pool: PgPool, pacing: Pacing, seed: Option<u64>) -> Self {
        Self { pool, pacing, seed }
    }

    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    /// Create the schema. Safe to run on every cold start.
    pub async fn migrate(&self) -> Result<()> {
        let statements = [
            "create table if not exists game (
                id          smallint primary key,
                version     bigint      not null default 0,
                state       jsonb       not null,
                updated_at  timestamptz not null default now(),
                constraint game_is_a_singleton check (id = 1)
            )",
            "create table if not exists feed (
                id          bigserial primary key,
                occurred_at bigint not null,
                kind        text   not null,
                actors      text[] not null default '{}',
                spans       jsonb  not null
            )",
            "create index if not exists feed_occurred_at_idx on feed (occurred_at)",
            "create index if not exists feed_actors_idx on feed using gin (actors)",
            "create table if not exists player (
                nick        text  primary key,
                token_hash  bytea not null unique,
                created_at  timestamptz not null default now()
            )",
            "create table if not exists join_attempt (
                bucket       text primary key,
                window_start timestamptz not null default now(),
                attempts     integer     not null default 0
            )",
        ];
        for statement in statements {
            sqlx::query(statement)
                .execute(&self.pool)
                .await
                .with_context(|| format!("migration failed: {statement}"))?;
        }
        Ok(())
    }

    /// Run one turn under the row lock, and persist everything it produced.
    pub async fn commit_turn(&self, input: Input) -> Result<Committed> {
        let mut tx = self.pool.begin().await?;
        let committed = self.run_turn(&mut tx, input).await?;
        tx.commit().await.context("could not commit the turn")?;
        Ok(committed)
    }

    /// Claim a name, mint its token, and admit the player — all in one
    /// transaction, so the credential and the character live or die together.
    ///
    /// Returns `None` when the name is already taken, having written nothing.
    pub async fn commit_join(&self, nick: &str) -> Result<Option<(Committed, String)>> {
        let mut tx = self.pool.begin().await?;

        let Some(token) = crate::auth::claim(&mut tx, nick).await.map_err(|e| match e {
            crate::error::ApiError::Internal(inner) => inner,
            other => anyhow::anyhow!("{other:?}"),
        })?
        else {
            // Taken. Roll back rather than leaving a half-made player.
            tx.rollback().await.ok();
            return Ok(None);
        };

        let committed =
            self.run_turn(&mut tx, Input::Join { nick: nick.to_string() }).await?;
        tx.commit().await.context("could not commit the join")?;
        Ok(Some((committed, token)))
    }

    /// The body of a turn, minus the commit, so callers can add work to the
    /// same transaction.
    async fn run_turn(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        input: Input,
    ) -> Result<Committed> {
        // The lock and the clock in a single round trip.
        let existing = sqlx::query(
            "select state, extract(epoch from now())::bigint as now
             from game where id = $1 for update",
        )
        .bind(GAME_ID)
        .fetch_optional(&mut **tx)
        .await
        .context("could not lock the game row")?;

        let opened_the_realm = existing.is_none();
        let (mut state, now, mut out) = match existing {
            Some(row) => {
                let state: GameState = serde_json::from_value(row.try_get("state")?)
                    .context("the stored game state could not be read")?;
                (state, row.try_get::<i64, _>("now")?, dragon_core::Out::new())
            }
            None => {
                // First request ever: open the Realm.
                let now: i64 =
                    sqlx::query_scalar("select extract(epoch from now())::bigint")
                        .fetch_one(&mut **tx)
                        .await?;
                let seed = self.seed.unwrap_or(now as u64);
                let (state, opening) =
                    dragon_core::start(now, seed, self.pacing.clone());
                (state, now, opening.out)
            }
        };

        // Pacing is deployment configuration, not saved game data: whatever the
        // environment says wins, so TICK_SECS can be retuned without starting
        // over. This is only safe because every duration in the core is
        // wall-clock rather than a tick count.
        state.pacing = self.pacing.clone();

        // A heartbeat that finds the clock already settled writes nothing.
        //
        // Callers check this before taking the lock, but that check races: at a
        // tick boundary every concurrent reader sees the same stale row and
        // escalates at once. The first through does the work; without this
        // second check the rest would each acquire the lock in turn and rewrite
        // the row with identical bytes — a thundering herd that fires on
        // schedule, every tick, and gets worse the more players are watching.
        // Re-checking under the lock makes all but the first a no-op.
        if !opened_the_realm
            && matches!(input, Input::CatchUp)
            && state.ticks_due(now) == 0
        {
            return Ok(Committed { turn: Turn::default(), state, cursor: None, now });
        }

        let Turn { out: stepped, ascension, ticks_replayed, ticks_skipped } =
            dragon_core::step(&mut state, input, now);
        let replayed = ticks_replayed > 0;
        // `out` already holds the opening announcements when this request is
        // the one that created the Realm; everything else appends to them.
        out.absorb(stepped);
        let turn = Turn { out, ascension, ticks_replayed, ticks_skipped };

        sqlx::query(
            "insert into game (id, state, version) values ($1, $2, 1)
             on conflict (id) do update
             set state = excluded.state,
                 version = game.version + 1,
                 updated_at = now()",
        )
        .bind(GAME_ID)
        .bind(sqlx::types::Json(&state))
        .execute(&mut **tx)
        .await
        .context("could not save the game state")?;

        let cursor = append_feed(tx, &turn.out.feed).await?;

        // Trim only when the world actually moved, so a poll never pays for it.
        if replayed {
            sqlx::query(
                "delete from feed where id <= (select max(id) - $1 from feed)",
            )
            .bind(FEED_RETENTION)
            .execute(&mut **tx)
            .await?;
        }

        Ok(Committed { turn, state, cursor, now })
    }

    /// Read the world without taking the lock.
    ///
    /// Returns whether the clock owes any ticks, so a caller can decide whether
    /// it is worth escalating to a write.
    pub async fn snapshot(&self) -> Result<Option<(GameState, i64)>> {
        let row = sqlx::query(
            "select state, extract(epoch from now())::bigint as now
             from game where id = $1",
        )
        .bind(GAME_ID)
        .fetch_optional(&self.pool)
        .await?;

        let Some(row) = row else { return Ok(None) };
        let mut state: GameState = serde_json::from_value(row.try_get("state")?)
            .context("the stored game state could not be read")?;
        state.pacing = self.pacing.clone();
        Ok(Some((state, row.try_get::<i64, _>("now")?)))
    }

    /// History after `since`, oldest first. `actor` narrows it to lines naming
    /// one player.
    pub async fn feed(
        &self,
        since: i64,
        limit: i64,
        actor: Option<&str>,
    ) -> Result<Vec<FeedEntry>> {
        let limit = limit.clamp(1, 500);
        let rows = match actor {
            Some(nick) => {
                sqlx::query(
                    "select id, occurred_at, kind, actors, spans from feed
                     where id > $1 and actors @> array[$2]::text[]
                     order by id limit $3",
                )
                .bind(since)
                .bind(nick)
                .bind(limit)
                .fetch_all(&self.pool)
                .await?
            }
            None => {
                sqlx::query(
                    "select id, occurred_at, kind, actors, spans from feed
                     where id > $1 order by id limit $2",
                )
                .bind(since)
                .bind(limit)
                .fetch_all(&self.pool)
                .await?
            }
        };

        rows.into_iter().map(feed_entry).collect()
    }

    /// The newest `limit` lines, oldest first — the backlog a fresh session
    /// paints before it starts polling forward.
    ///
    /// This is what makes history a server-side matter: the feed table outlives
    /// browsers, devices and game restarts, so a client that has lost its
    /// cursor can always reopen on the recent past instead of an empty channel.
    pub async fn feed_tail(
        &self,
        limit: i64,
        actor: Option<&str>,
    ) -> Result<Vec<FeedEntry>> {
        let limit = limit.clamp(1, 500);
        let rows = match actor {
            Some(nick) => {
                sqlx::query(
                    "select id, occurred_at, kind, actors, spans from
                       (select id, occurred_at, kind, actors, spans from feed
                        where actors @> array[$1]::text[]
                        order by id desc limit $2) newest
                     order by id",
                )
                .bind(nick)
                .bind(limit)
                .fetch_all(&self.pool)
                .await?
            }
            None => {
                sqlx::query(
                    "select id, occurred_at, kind, actors, spans from
                       (select id, occurred_at, kind, actors, spans from feed
                        order by id desc limit $1) newest
                     order by id",
                )
                .bind(limit)
                .fetch_all(&self.pool)
                .await?
            }
        };
        rows.into_iter().map(feed_entry).collect()
    }

    /// The newest feed id, for a client that wants to start from "now".
    pub async fn cursor(&self) -> Result<i64> {
        Ok(sqlx::query_scalar("select coalesce(max(id), 0) from feed")
            .fetch_one(&self.pool)
            .await?)
    }

    /// Count one signup attempt against `bucket`'s rolling window.
    ///
    /// Every attempt counts, successful or not — that is what bounds a leaked
    /// invitation key and what stops the taken-name response being used to
    /// enumerate the roster. The whole thing is one statement, so concurrent
    /// callers cannot race past the limit between a read and a write.
    pub async fn record_join_attempt(
        &self,
        bucket: &str,
        window_secs: i64,
    ) -> Result<JoinAttempt> {
        let row = sqlx::query(
            "insert into join_attempt (bucket, window_start, attempts)
             values ($1, now(), 1)
             on conflict (bucket) do update set
               window_start = case
                   when join_attempt.window_start
                        < now() - make_interval(secs => $2::double precision)
                   then now() else join_attempt.window_start end,
               attempts = case
                   when join_attempt.window_start
                        < now() - make_interval(secs => $2::double precision)
                   then 1 else join_attempt.attempts + 1 end
             returning
               attempts,
               greatest(0, ceil(extract(epoch from (
                   window_start + make_interval(secs => $2::double precision) - now()
               ))))::bigint as retry_after",
        )
        .bind(bucket)
        .bind(window_secs as f64)
        .fetch_one(&self.pool)
        .await
        .context("could not record the signup attempt")?;

        Ok(JoinAttempt {
            attempts: row.try_get("attempts")?,
            retry_after: row.try_get("retry_after")?,
        })
    }
}

/// Rehydrate one stored feed row.
fn feed_entry(row: sqlx::postgres::PgRow) -> Result<FeedEntry> {
    let spans: Vec<Span> = serde_json::from_value(row.try_get("spans")?)?;
    Ok(FeedEntry {
        id: row.try_get("id")?,
        line: Line {
            kind: parse_kind(&row.try_get::<String, _>("kind")?),
            actors: row.try_get("actors")?,
            spans,
            at: Some(row.try_get("occurred_at")?),
        },
    })
}

/// One caller's standing in the signup rate limit.
#[derive(Debug, Clone, Copy)]
pub struct JoinAttempt {
    /// Attempts made in the current window, including this one.
    pub attempts: i32,
    /// Seconds until the window rolls over.
    pub retry_after: i64,
}

/// Write a turn's broadcasts, returning the last id.
///
/// Batched at [`FEED_INSERT_CHUNK`] lines per statement so no single statement
/// can approach Postgres's bind-parameter ceiling — see that constant for why
/// exceeding it is unrecoverable rather than merely slow.
async fn append_feed(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    lines: &[Line],
) -> Result<Option<i64>> {
    let mut last = None;
    for batch in lines.chunks(FEED_INSERT_CHUNK) {
        let mut builder =
            QueryBuilder::new("insert into feed (occurred_at, kind, actors, spans) ");
        builder.push_values(batch, |mut row, line| {
            row.push_bind(line.at.unwrap_or_default())
                .push_bind(kind_name(line.kind))
                .push_bind(line.actors.clone())
                .push_bind(sqlx::types::Json(line.spans.clone()));
        });
        builder.push(" returning id");

        let ids = builder
            .build()
            .fetch_all(&mut **tx)
            .await
            .context("could not append to the feed")?;
        for row in ids {
            last = last.max(Some(row.try_get::<i64, _>("id")?));
        }
    }
    Ok(last)
}

fn kind_name(kind: Kind) -> &'static str {
    match kind {
        Kind::System => "system",
        Kind::BBoard => "b_board",
        Kind::Store => "store",
        Kind::Event => "event",
        Kind::Quest => "quest",
    }
}

fn parse_kind(name: &str) -> Kind {
    match name {
        "b_board" => Kind::BBoard,
        "store" => Kind::Store,
        "event" => Kind::Event,
        "quest" => Kind::Quest,
        _ => Kind::System,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_kind_survives_a_round_trip_through_the_column() {
        for kind in [Kind::System, Kind::BBoard, Kind::Store, Kind::Event, Kind::Quest] {
            assert_eq!(parse_kind(kind_name(kind)), kind);
        }
    }

    #[test]
    fn an_unrecognised_kind_degrades_to_system() {
        // An older row, or one written by a future version, must still render.
        assert_eq!(parse_kind("prophecy"), Kind::System);
        assert_eq!(parse_kind(""), Kind::System);
    }

    #[test]
    fn kind_names_match_the_wire_format_the_core_serialises() {
        // The client sees the core's own snake_case names in the JSON body, so
        // the column has to agree with them.
        for kind in [Kind::System, Kind::BBoard, Kind::Store, Kind::Event, Kind::Quest] {
            let json = serde_json::to_string(&kind).unwrap();
            assert_eq!(json, format!("\"{}\"", kind_name(kind)));
        }
    }
}
