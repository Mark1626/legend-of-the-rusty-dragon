//! End-to-end tests through the HTTP surface.
//!
//! Same requirement as `store.rs`: set `TEST_DATABASE_URL` or these skip.
//! Requests go through the real router, so routing, extraction, authorisation
//! and status codes are all exercised.

use std::sync::atomic::{AtomicU32, Ordering};

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use dragon_api::{AppState, Store};
use dragon_core::config::Pacing;
use http_body_util::BodyExt;
use serde_json::{Value, json};
use sqlx::postgres::PgPoolOptions;
use sqlx::AssertSqlSafe;
use tower::ServiceExt;

static COUNTER: AtomicU32 = AtomicU32::new(0);

/// The invitation key every test signs up with unless it is testing the gate.
const INVITE: &str = "open-sesame";

struct Harness {
    router: Router,
    pool: sqlx::PgPool,
}

impl Harness {
    async fn new() -> Option<Self> {
        Self::configured(|state| {
            state.invite_keys = vec![INVITE.to_string()];
        })
        .await
    }

    async fn with_secrets(admin: Option<&str>, cron: Option<&str>) -> Option<Self> {
        let (admin, cron) = (admin.map(str::to_string), cron.map(str::to_string));
        Self::configured(move |state| {
            state.invite_keys = vec![INVITE.to_string()];
            state.admin_secret = admin.clone();
            state.cron_secret = cron.clone();
        })
        .await
    }

    async fn configured(adjust: impl FnOnce(&mut AppState)) -> Option<Self> {
        let url = std::env::var("TEST_DATABASE_URL").ok().filter(|v| !v.trim().is_empty())?;
        let schema = format!(
            "dragon_http_{}_{}",
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
                        sqlx::query(AssertSqlSafe(format!("set search_path to {schema}")))
                            .execute(&mut *conn)
                            .await?;
                        Ok(())
                    })
                }
            })
            .connect(&url)
            .await
            .expect("could not connect to TEST_DATABASE_URL");
        // Audited: `schema` is built from the process id and a counter, never
        // from anything a caller supplies.
        sqlx::query(AssertSqlSafe(format!("create schema if not exists {schema}")))
            .execute(&pool)
            .await
            .unwrap();

        let store = Store::from_pool(pool.clone(), Pacing::WEB, Some(31_337));
        store.migrate().await.unwrap();
        let mut state = AppState::new(store);
        adjust(&mut state);
        Some(Self { router: dragon_api::app(state), pool })
    }

    /// For assertions that need to look at the database directly.
    fn pool(&self) -> &sqlx::PgPool {
        &self.pool
    }

    async fn send(&self, request: Request<Body>) -> (StatusCode, Value) {
        let response = self.router.clone().oneshot(request).await.expect("router failed");
        let status = response.status();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let body = if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes).unwrap_or(Value::Null)
        };
        (status, body)
    }

    async fn get(&self, uri: &str) -> (StatusCode, Value) {
        self.send(Request::get(uri).body(Body::empty()).unwrap()).await
    }

    async fn get_auth(&self, uri: &str, token: &str) -> (StatusCode, Value) {
        self.send(
            Request::get(uri)
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
    }

    async fn post(&self, uri: &str, body: Value) -> (StatusCode, Value) {
        self.send(
            Request::post(uri)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
    }

    async fn post_auth(&self, uri: &str, token: &str, body: Value) -> (StatusCode, Value) {
        self.send(
            Request::post(uri)
                .header(header::CONTENT_TYPE, "application/json")
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
    }

    /// Join with the standard invitation, returning the bearer token.
    async fn join(&self, nick: &str) -> String {
        let (status, body) =
            self.post("/api/join", json!({ "nick": nick, "invite": INVITE })).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        body["token"].as_str().expect("no token returned").to_string()
    }
}

macro_rules! harness {
    () => {
        match Harness::new().await {
            Some(h) => h,
            None => {
                eprintln!("skipping: TEST_DATABASE_URL is not set");
                return;
            }
        }
    };
    ($admin:expr, $cron:expr) => {
        match Harness::with_secrets($admin, $cron).await {
            Some(h) => h,
            None => {
                eprintln!("skipping: TEST_DATABASE_URL is not set");
                return;
            }
        }
    };
}

#[tokio::test]
async fn health_reports_the_version() {
    let h = harness!();
    let (status, body) = h.get("/api/health").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["ok"], true);
    assert_eq!(body["version"], dragon_core::version());
}

#[tokio::test]
async fn joining_returns_a_token_once() {
    let h = harness!();
    let (status, body) = h.post("/api/join", json!({ "nick": "Absalom", "invite": INVITE })).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["nick"], "Absalom");
    let token = body["token"].as_str().unwrap();
    assert_eq!(token.len(), 64);
    assert!(
        body["feed"].as_array().unwrap().iter().any(|line| line["text"]
            .as_str()
            .unwrap_or("")
            .contains("has entered the Realm")),
        "{body}"
    );
}

#[tokio::test]
async fn a_taken_name_is_a_conflict() {
    let h = harness!();
    h.join("Absalom").await;
    let (status, body) = h.post("/api/join", json!({ "nick": "Absalom", "invite": INVITE })).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert!(body["error"].as_str().unwrap().contains("already taken"), "{body}");
}

#[tokio::test]
async fn a_bad_name_is_rejected_with_a_reason() {
    let h = harness!();
    for bad in ["", "  ", "has spaces", "-dash", "emoji🐉", &"x".repeat(30)] {
        let (status, body) = h.post("/api/join", json!({ "nick": bad, "invite": INVITE })).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "accepted {bad:?}");
        assert!(body["error"].is_string(), "{body}");
    }
}

#[tokio::test]
async fn state_lists_the_board_the_store_and_the_standings() {
    let h = harness!();
    h.join("Absalom").await;
    let (status, body) = h.get("/api/state").await;
    assert_eq!(status, StatusCode::OK, "{body}");

    assert_eq!(body["quests"].as_array().unwrap().len(), 16);
    assert!(!body["shop"].as_array().unwrap().is_empty());
    assert_eq!(body["players"], 1);
    assert_eq!(body["scores"][0]["nick"], "Absalom");
    assert!(body["cursor"].as_i64().unwrap() > 0);
    assert_eq!(body["tick_secs"], Pacing::WEB.tick_secs);

    // Quests are readable without having to parse the prose.
    let quest = &body["quests"][0];
    assert_eq!(quest["id"].as_str().unwrap().len(), 4);
    assert!(quest["monsters"].as_array().unwrap().iter().all(|m| {
        m["name"].is_string() && m["count"].is_number() && m["cr"].is_string()
    }));

    // Store entries carry what a shopper needs.
    let item = &body["shop"][0];
    assert!(item["cost"].is_number());
    assert!(matches!(item["kind"].as_str(), Some("weapon" | "armor")));
    assert!(item["in_stock"].as_i64().unwrap() > 0);
}

#[tokio::test]
async fn the_feed_pages_forward_from_a_cursor() {
    let h = harness!();
    h.join("Absalom").await;

    let (_, all) = h.get("/api/feed?since=0&limit=500").await;
    let lines = all["lines"].as_array().unwrap();
    assert!(!lines.is_empty());
    assert!(lines.iter().all(|l| l["id"].is_number() && l["text"].is_string()));

    // Reading from the tip returns nothing until something new happens.
    let cursor = all["cursor"].as_i64().unwrap();
    let (_, empty) = h.get(&format!("/api/feed?since={cursor}")).await;
    assert!(empty["lines"].as_array().unwrap().is_empty());
    assert_eq!(empty["cursor"], cursor, "an empty page keeps the cursor");

    h.join("Barnabas").await;
    let (_, next) = h.get(&format!("/api/feed?since={cursor}")).await;
    assert!(!next["lines"].as_array().unwrap().is_empty());
    assert!(next["cursor"].as_i64().unwrap() > cursor);
}

#[tokio::test]
async fn the_feed_can_be_filtered_to_one_player() {
    let h = harness!();
    h.join("Absalom").await;
    h.join("Barnabas").await;

    let (status, body) = h.get("/api/feed?since=0&actor=Absalom").await;
    assert_eq!(status, StatusCode::OK);
    let lines = body["lines"].as_array().unwrap();
    assert!(!lines.is_empty());
    assert!(lines.iter().all(|line| {
        line["actors"].as_array().is_some_and(|actors| {
            actors.iter().any(|a| a == "Absalom")
        })
    }), "{body}");
}

#[tokio::test]
async fn writes_require_a_token() {
    let h = harness!();
    for (uri, body) in [
        ("/api/quest", json!({ "quest": "0000" })),
        ("/api/buy", json!({ "item": 0 })),
    ] {
        let (status, _) = h.post(uri, body).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{uri} accepted an anonymous write");
    }
    let (status, _) = h.get("/api/me").await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn a_forged_token_is_rejected() {
    let h = harness!();
    h.join("Absalom").await;
    let forged = "a".repeat(64);
    let (status, _) = h.get_auth("/api/me", &forged).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let (status, _) = h.get_auth("/api/me", "not-even-hex").await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn one_players_token_cannot_act_as_another() {
    let h = harness!();
    let absalom = h.join("Absalom").await;
    h.join("Barnabas").await;

    let (status, body) = h.get_auth("/api/me", &absalom).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["nick"], "Absalom", "a token resolved to the wrong player");
}

#[tokio::test]
async fn me_reports_a_full_character_sheet() {
    let h = harness!();
    let token = h.join("Absalom").await;
    let (status, body) = h.get_auth("/api/me", &token).await;
    assert_eq!(status, StatusCode::OK, "{body}");

    assert_eq!(body["in_realm"], true);
    assert_eq!(body["level"], 1);
    assert_eq!(body["xp"], 0);
    assert_eq!(body["money"], 0);
    assert_eq!(body["hp"], 10);
    assert_eq!(body["strategy"], "random");
    assert!(body["weapon"].is_null(), "a newcomer carries nothing");
    assert!(body["summary"].as_str().unwrap().starts_with("Your stats: level 1"));
}

#[tokio::test]
async fn questing_resolves_a_fight_and_answers_privately() {
    let h = harness!();
    let token = h.join("Absalom").await;
    let (_, state) = h.get("/api/state").await;
    let quest = state["quests"][0]["id"].as_str().unwrap().to_string();

    let (status, body) = h
        .post_auth("/api/quest", &token, json!({ "quest": quest, "strategy": "wise" }))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let feed = body["feed"].as_array().unwrap();
    let narration: String =
        feed.iter().filter_map(|l| l["text"].as_str()).collect::<Vec<_>>().join(" ");
    assert!(narration.contains("has accepted quest"), "{narration}");
    assert!(
        narration.contains("has won!") || narration.contains("has lost!"),
        "{narration}"
    );

    let (_, me) = h.get_auth("/api/me", &token).await;
    assert_eq!(me["strategy"], "wise");
    assert_eq!(
        me["quests_won"].as_i64().unwrap() + me["quests_lost"].as_i64().unwrap(),
        1
    );
}

#[tokio::test]
async fn a_malformed_quest_number_is_a_bad_request() {
    let h = harness!();
    let token = h.join("Absalom").await;
    let (status, body) = h.post_auth("/api/quest", &token, json!({ "quest": "zzzz" })).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body["error"].as_str().unwrap().contains("0A3F"), "{body}");
}

#[tokio::test]
async fn buying_without_money_is_refused_privately() {
    let h = harness!();
    let token = h.join("Absalom").await;
    let (_, state) = h.get("/api/state").await;
    let item = state["shop"][0]["id"].as_i64().unwrap();

    let (status, body) = h.post_auth("/api/buy", &token, json!({ "item": item })).await;
    assert_eq!(status, StatusCode::OK, "a refusal is not an error");
    let reply: String = body["reply"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|l| l["text"].as_str())
        .collect::<Vec<_>>()
        .join(" ");
    assert!(reply.contains("enough money"), "{reply}");
    assert!(body["feed"].as_array().unwrap().is_empty(), "nothing was announced");
}

#[tokio::test]
async fn admin_is_bolted_shut_when_no_secret_is_configured() {
    let h = harness!();
    let (status, body) = h.post("/api/admin", json!({ "admin": "kill" })).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert!(body["error"].as_str().unwrap().contains("not enabled"), "{body}");
}

#[tokio::test]
async fn admin_requires_the_right_secret() {
    let h = harness!(Some("hunter2"), None);

    let (status, _) = h.post("/api/admin", json!({ "admin": "kill" })).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "no credentials at all");

    let (status, _) = h.post_auth("/api/admin", "wrong", json!({ "admin": "kill" })).await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    let (status, body) = h
        .post_auth("/api/admin", "hunter2", json!({ "admin": "add_quest", "level": 9 }))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (_, state) = h.get("/api/state").await;
    assert_eq!(state["quests"].as_array().unwrap().len(), 17);
}

#[tokio::test]
async fn the_heartbeat_is_open_when_unsecured_and_guarded_when_not() {
    let open = harness!();
    let (status, _) = open.post("/api/tick", json!({})).await;
    assert_eq!(status, StatusCode::OK);

    let guarded = harness!(None, Some("beat"));
    let (status, _) = guarded.post("/api/tick", json!({})).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "an unsigned heartbeat got through");

    let (status, _) = guarded.post_auth("/api/tick", "beat", json!({})).await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn an_unknown_route_is_a_not_found() {
    let h = harness!();
    let (status, _) = h.get("/api/nonsense").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn the_realm_opens_itself_on_the_very_first_read() {
    // No join, no tick: just look at it.
    let h = harness!();
    let (status, body) = h.get("/api/state").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["quests"].as_array().unwrap().len(), 16);
    assert_eq!(body["players"], 0);
}

#[tokio::test]
async fn concurrent_players_all_get_through() {
    let h = harness!();
    let router = h.router.clone();
    let joins = (0..8).map(|i| {
        let router = router.clone();
        tokio::spawn(async move {
            let request = Request::post("/api/join")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(json!({ "nick": format!("rider{i:02}"), "invite": INVITE }).to_string()))
                .unwrap();
            router.oneshot(request).await.unwrap().status()
        })
    });
    for handle in joins {
        assert_eq!(handle.await.unwrap(), StatusCode::OK);
    }

    let (_, state) = h.get("/api/state").await;
    assert_eq!(state["players"], 8, "a concurrent join was lost");
}

// ---------------------------------------------------------------------------
// Signup is invitation-only  (review pass 1, findings 1 and 4)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn signup_requires_a_valid_invitation() {
    let h = harness!();

    let (status, body) = h.post("/api/join", json!({ "nick": "Absalom" })).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "a missing key got through: {body}");

    let (status, _) =
        h.post("/api/join", json!({ "nick": "Absalom", "invite": "guessed" })).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "a wrong key got through");

    // Neither attempt created anything.
    let (_, state) = h.get("/api/state").await;
    assert_eq!(state["players"], 0);

    let (status, _) =
        h.post("/api/join", json!({ "nick": "Absalom", "invite": INVITE })).await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn a_realm_with_no_invitation_key_is_closed_to_everyone() {
    // Fail-closed: an unset key means nobody may join, never that anybody may.
    let Some(h) = Harness::configured(|state| state.invite_keys.clear()).await else {
        eprintln!("skipping: TEST_DATABASE_URL is not set");
        return;
    };
    for body in [json!({ "nick": "Absalom" }), json!({ "nick": "Absalom", "invite": "" })] {
        let (status, _) = h.post("/api/join", body).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
    }
}

#[tokio::test]
async fn several_invitation_keys_are_accepted_so_rotation_locks_nobody_out() {
    let Some(h) = Harness::configured(|state| {
        state.invite_keys = vec!["retiring".into(), "current".into()];
    })
    .await
    else {
        eprintln!("skipping: TEST_DATABASE_URL is not set");
        return;
    };

    for (nick, key) in [("Early", "retiring"), ("Late", "current")] {
        let (status, body) = h.post("/api/join", json!({ "nick": nick, "invite": key })).await;
        assert_eq!(status, StatusCode::OK, "{key} was refused: {body}");
    }
    let (status, _) =
        h.post("/api/join", json!({ "nick": "Third", "invite": "withdrawn" })).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "a withdrawn key still worked");
}

#[tokio::test]
async fn signup_attempts_are_rate_limited_however_they_fail() {
    let Some(h) = Harness::configured(|state| {
        state.invite_keys = vec![INVITE.to_string()];
        state.join_attempts_per_window = 3;
        state.join_window_secs = 3_600;
    })
    .await
    else {
        eprintln!("skipping: TEST_DATABASE_URL is not set");
        return;
    };

    // Wrong keys burn attempts too — that is what bounds a leaked key and
    // closes the name-enumeration oracle.
    for _ in 0..3 {
        let (status, _) =
            h.post("/api/join", json!({ "nick": "Absalom", "invite": "wrong" })).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
    }

    let (status, body) =
        h.post("/api/join", json!({ "nick": "Absalom", "invite": INVITE })).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "the limit did not apply: {body}");
    assert!(body["error"].as_str().unwrap().contains("Try again in"), "{body}");

    let (_, state) = h.get("/api/state").await;
    assert_eq!(state["players"], 0, "a rate-limited attempt still created a player");
}

// ---------------------------------------------------------------------------
// The heartbeat is idempotent  (review pass 1, finding 2)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn repeated_heartbeats_within_one_tick_window_do_no_work() {
    let h = harness!();
    h.get("/api/state").await; // open the Realm and settle the clock

    let (_, before) = h.get("/api/state").await;

    for _ in 0..25 {
        let (status, body) = h.post("/api/tick", json!({})).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["ticks_replayed"], 0, "a spare heartbeat moved the world");
        assert!(body["feed"].as_array().unwrap().is_empty());
        assert!(body["cursor"].is_null(), "a no-op heartbeat advanced the cursor");
    }

    let (_, after) = h.get("/api/state").await;
    assert_eq!(after["tick"], before["tick"], "the clock moved");
    assert_eq!(after["cursor"], before["cursor"], "the feed grew");
}

#[tokio::test]
async fn a_no_op_heartbeat_does_not_bump_the_row_version() {
    // The point of the guard: no lock, no write, nothing for other players to
    // queue behind.
    let h = harness!();
    h.get("/api/state").await;

    let version = |pool: sqlx::PgPool| async move {
        sqlx::query_scalar::<_, i64>("select version from game where id = 1")
            .fetch_one(&pool)
            .await
            .unwrap()
    };
    let before = version(h.pool().clone()).await;
    for _ in 0..20 {
        h.get("/api/tick").await;
    }
    assert_eq!(version(h.pool().clone()).await, before, "the row was rewritten");
}

#[tokio::test]
async fn a_heartbeat_still_advances_the_world_when_a_tick_is_owed() {
    let h = harness!();
    h.get("/api/state").await;

    // Wind the stored clock back so ticks are genuinely due.
    sqlx::query(
        "update game set state = jsonb_set(state, '{last_tick_at}',
         to_jsonb((state->>'last_tick_at')::bigint - 100000)) where id = 1",
    )
    .execute(h.pool())
    .await
    .unwrap();

    let (status, body) = h.post("/api/tick", json!({})).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        body["ticks_replayed"].as_u64().unwrap() > 0,
        "an owed tick was not replayed: {body}"
    );
}

// ---------------------------------------------------------------------------
// Joining is atomic  (review pass 1, finding 3)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_name_is_never_claimed_without_its_token_reaching_the_caller() {
    let h = harness!();
    let token = h.join("Absalom").await;

    let (status, me) = h.get_auth("/api/me", &token).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(me["nick"], "Absalom");
    assert_eq!(me["in_realm"], true);

    let (status, _) =
        h.post("/api/join", json!({ "nick": "Absalom", "invite": INVITE })).await;
    assert_eq!(status, StatusCode::CONFLICT);

    let players: i64 = sqlx::query_scalar("select count(*) from player")
        .fetch_one(h.pool())
        .await
        .unwrap();
    assert_eq!(players, 1, "the refused join left a stranded credential row");
}

#[tokio::test]
async fn a_taken_name_writes_nothing_at_all() {
    let h = harness!();
    h.join("Absalom").await;
    let (_, before) = h.get("/api/state").await;

    for _ in 0..5 {
        let (status, _) =
            h.post("/api/join", json!({ "nick": "Absalom", "invite": INVITE })).await;
        assert_eq!(status, StatusCode::CONFLICT);
    }

    let (_, after) = h.get("/api/state").await;
    assert_eq!(after["players"], before["players"]);
    assert_eq!(after["cursor"], before["cursor"], "a refused join touched the feed");
}
