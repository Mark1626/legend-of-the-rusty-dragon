//! The HTTP surface.
//!
//! Reads never take the game lock. `/api/feed` is pure history, and `/api/state`
//! escalates to a write only when the clock actually owes a tick — otherwise a
//! polling client would serialise every other player behind it.

use axum::extract::{Query, State};
use axum::http::HeaderMap;
use axum::{Json, Router, routing};
use dragon_core::assets::{self, ItemKind, ItemStats};
use dragon_core::config::time_display;
use dragon_core::quest::QuestId;
use dragon_core::state::GameState;
use dragon_core::step::{AdminCommand, Input};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::auth;
use crate::error::{ApiError, ApiResult};
use crate::store::Committed;
use crate::{AppState, store::Store};

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/health", routing::get(health))
        .route("/api/join", routing::post(join))
        .route("/api/quest", routing::post(quest))
        .route("/api/buy", routing::post(buy))
        .route("/api/me", routing::get(me))
        .route("/api/state", routing::get(state))
        .route("/api/feed", routing::get(feed))
        .route("/api/tick", routing::post(tick).get(tick))
        .route("/api/admin", routing::post(admin))
        .fallback(unrouted)
}

/// Report what path actually arrived.
///
/// On Vercel a single function serves every route by way of a rewrite, and a
/// rewrite is supposed to leave the original path alone. If that ever stops
/// being true, every route would 404 at once and a bare "Not Found" would say
/// nothing about why — so the reply names the path it was asked for.
async fn unrouted(uri: axum::http::Uri) -> (axum::http::StatusCode, Json<Value>) {
    (
        axum::http::StatusCode::NOT_FOUND,
        Json(json!({
            "error": "No such endpoint.",
            "path": uri.path(),
            "endpoints": [
                "GET  /api/health",
                "POST /api/join",
                "POST /api/quest",
                "POST /api/buy",
                "GET  /api/me",
                "GET  /api/state",
                "GET  /api/feed",
                "POST /api/tick",
                "POST /api/admin",
            ],
        })),
    )
}

// ---------------------------------------------------------------------------
// request and response bodies
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct JoinRequest {
    pub nick: String,
    /// The signup key. The Realm is invitation-only.
    #[serde(default)]
    pub invite: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct QuestRequest {
    /// Four hex digits, as shown on the Bulletin Board.
    pub quest: String,
    #[serde(default)]
    pub strategy: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct BuyRequest {
    pub item: u16,
}

#[derive(Debug, Deserialize)]
pub struct FeedQuery {
    /// Return lines after this id. Start from `/api/state`'s cursor.
    #[serde(default)]
    pub since: i64,
    #[serde(default = "default_limit")]
    pub limit: i64,
    /// Narrow to lines naming one player.
    #[serde(default)]
    pub actor: Option<String>,
}

fn default_limit() -> i64 {
    200
}

#[derive(Debug, Serialize)]
pub struct TurnResponse {
    /// What the caller is told privately.
    reply: Vec<Value>,
    /// What this turn broadcast, so a client can render before its next poll.
    feed: Vec<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    ascension: Option<String>,
    /// Advance the client's feed cursor to this, when present.
    #[serde(skip_serializing_if = "Option::is_none")]
    cursor: Option<i64>,
    now: i64,
    ticks_replayed: u64,
    ticks_skipped: u64,
}

impl TurnResponse {
    /// Nothing happened, and nothing needed to.
    fn idle(now: i64) -> Self {
        Self {
            reply: Vec::new(),
            feed: Vec::new(),
            ascension: None,
            cursor: None,
            now,
            ticks_replayed: 0,
            ticks_skipped: 0,
        }
    }
}

impl From<Committed> for TurnResponse {
    fn from(committed: Committed) -> Self {
        let Committed { turn, cursor, now, .. } = committed;
        Self {
            reply: turn.out.reply.iter().map(as_json).collect(),
            feed: turn.out.feed.iter().map(as_json).collect(),
            ascension: turn.ascension,
            cursor,
            now,
            ticks_replayed: turn.ticks_replayed,
            ticks_skipped: turn.ticks_skipped,
        }
    }
}

fn as_json(line: &dragon_core::out::Line) -> Value {
    let mut value = serde_json::to_value(line).unwrap_or(Value::Null);
    // The plain rendering saves every client reimplementing span flattening.
    if let Some(object) = value.as_object_mut() {
        object.insert("text".into(), Value::String(line.plain()));
    }
    value
}

// ---------------------------------------------------------------------------
// handlers
// ---------------------------------------------------------------------------

async fn health() -> Json<Value> {
    Json(json!({ "ok": true, "version": dragon_core::version() }))
}

/// Enter the Realm and receive a bearer token.
///
/// Invitation-only: the caller must present a configured signup key. The token
/// is shown exactly once; only its digest is kept.
async fn join(
    State(app): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<JoinRequest>,
) -> ApiResult<Json<Value>> {
    // An unset key means nobody may sign up, never that anybody may.
    if app.invite_keys.is_empty() {
        return Err(ApiError::Forbidden(
            "This Realm is not accepting new adventurers.".into(),
        ));
    }

    // Counted before anything else, so a wrong key, a taken name and a
    // malformed name all cost an attempt. That is what bounds a leaked key and
    // what stops the taken-name reply being used to enumerate the roster.
    let attempt = app
        .store
        .record_join_attempt(&client_bucket(&headers), app.join_window_secs)
        .await?;
    if attempt.attempts > app.join_attempts_per_window {
        return Err(ApiError::TooManyRequests(format!(
            "Too many signup attempts. Try again in {}.",
            time_display(attempt.retry_after)
        )));
    }

    if !auth::invite_accepted(&app.invite_keys, request.invite.as_deref().unwrap_or("")) {
        return Err(ApiError::Forbidden(
            "That is not a valid invitation to this Realm.".into(),
        ));
    }

    let nick = auth::validate_nick(&request.nick)?;
    let Some((committed, token)) = app.store.commit_join(&nick).await? else {
        return Err(ApiError::Conflict(format!(
            "The name {nick} is already taken in this Realm."
        )));
    };

    let mut body = serde_json::to_value(TurnResponse::from(committed))?;
    if let Some(object) = body.as_object_mut() {
        object.insert("nick".into(), Value::String(nick));
        object.insert("token".into(), Value::String(token));
    }
    Ok(Json(body))
}

/// Which rate-limit bucket a request belongs to.
///
/// Vercel sets `x-forwarded-for` itself and appends the true client address as
/// the first hop, so on the platform this is trustworthy. Off-platform the
/// header is absent and every caller shares one bucket, which fails safe: it
/// limits more than intended rather than less.
fn client_bucket(headers: &HeaderMap) -> String {
    headers
        .get("x-forwarded-for")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(',').next())
        .map(str::trim)
        .filter(|address| !address.is_empty())
        .map_or_else(|| "ip:unattributed".to_string(), |address| format!("ip:{address}"))
}

async fn quest(
    State(app): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<QuestRequest>,
) -> ApiResult<Json<TurnResponse>> {
    let nick = current_player(&app, &headers).await?;
    let quest = QuestId::parse(&request.quest).ok_or_else(|| {
        ApiError::BadRequest(format!(
            "{:?} is not a quest number; they look like 0A3F.",
            request.quest
        ))
    })?;
    let committed = app
        .store
        .commit_turn(Input::Quest { nick, quest, strategy: request.strategy })
        .await?;
    Ok(Json(committed.into()))
}

async fn buy(
    State(app): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<BuyRequest>,
) -> ApiResult<Json<TurnResponse>> {
    let nick = current_player(&app, &headers).await?;
    let committed = app.store.commit_turn(Input::Buy { nick, item: request.item }).await?;
    Ok(Json(committed.into()))
}

async fn me(
    State(app): State<AppState>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    let nick = current_player(&app, &headers).await?;
    let (state, now) = settled(&app.store).await?;

    let Some(user) = state.users.get(&nick) else {
        // Purged for inactivity, or ascended. The token still works; the
        // character does not exist until they quest again.
        return Ok(Json(json!({
            "nick": nick,
            "in_realm": false,
            "message": dragon_core::state::NOT_REGISTERED,
            "now": now,
        })));
    };

    let gear = |slot: Option<u16>| {
        slot.and_then(assets::item).map(|item| {
            json!({ "id": item.id, "name": item.name, "cost": item.cost })
        })
    };
    Ok(Json(json!({
        "nick": user.nick,
        "in_realm": true,
        "level": user.level,
        "xp": user.xp,
        "money": user.money,
        "hp": user.hp,
        "ac": user.ac,
        "strength": user.strength,
        "dexterity": user.dexterity,
        "strategy": user.strategy,
        "weapon": gear(user.weapon),
        "armor": gear(user.armor),
        "proficiency_bonus": user.proficiency_bonus,
        "quests_won": user.quest_win,
        "quests_lost": user.quest_lost,
        "resting_until": user.idle_until,
        "resting_for": user.rest_remaining(now),
        "resting_display": time_display(user.rest_remaining(now)),
        "joined_at": user.created_at,
        "summary": user.about(now).plain(),
        "now": now,
    })))
}

/// The board, the store and the standings.
///
/// This is the endpoint that replaces the reference's periodic channel dumps:
/// IRC had to broadcast the whole board because there was no way to ask.
async fn state(State(app): State<AppState>) -> ApiResult<Json<Value>> {
    let (state, now) = settled(&app.store).await?;
    let cursor = app.store.cursor().await?;

    let quests: Vec<Value> = state
        .bboard
        .posted()
        .into_iter()
        .map(|(id, quest)| {
            json!({
                "id": id.to_string(),
                "total_cr": quest.total_cr().to_string(),
                "display": quest.display(),
                "monsters": quest.groups().iter().map(|group| json!({
                    "name": group.name(),
                    "count": group.count,
                    "cr": group.cr().label(),
                })).collect::<Vec<_>>(),
                "posted_at": quest.created_at,
            })
        })
        .collect();

    let shop: Vec<Value> = state
        .shop
        .stock()
        .into_iter()
        .filter_map(|(id, count)| {
            let item = assets::item(id)?;
            let detail = match item.stats {
                ItemStats::Weapon { damage } => json!({ "damage": damage.to_string() }),
                ItemStats::Armor { base, dex_bonus, max_bonus } => json!({
                    "armor_class": base,
                    "dex_bonus": dex_bonus,
                    "max_dex_bonus": max_bonus,
                }),
            };
            Some(json!({
                "id": item.id,
                "tag": item.tag(),
                "name": item.name,
                "cost": item.cost,
                "weight": item.weight,
                "kind": match item.kind() {
                    ItemKind::Weapon => "weapon",
                    ItemKind::Armor => "armor",
                },
                "in_stock": count,
                "detail": detail,
            }))
        })
        .collect();

    let scores: Vec<Value> = state
        .ranking()
        .iter()
        .map(|user| {
            json!({
                "nick": user.nick,
                "level": user.level,
                "xp": user.xp,
                "resting": user.is_resting(now),
            })
        })
        .collect();

    Ok(Json(json!({
        "now": now,
        "cursor": cursor,
        "tick": state.tick_turn,
        "next_tick_at": state.last_tick_at + state.pacing.tick_secs,
        "tick_secs": state.pacing.tick_secs,
        "uptime": state.uptime(now),
        "uptime_display": time_display(state.uptime(now)),
        "stopped": state.killed,
        "ascension_cr": state.pacing.ascension_cr.to_string(),
        "players": state.users.len(),
        "quests": quests,
        "shop": shop,
        "scores": scores,
        "version": dragon_core::version(),
    })))
}

/// History. Never ticks: a client polling this every few seconds must not
/// contend for the game lock.
async fn feed(
    State(app): State<AppState>,
    Query(query): Query<FeedQuery>,
) -> ApiResult<Json<Value>> {
    let entries = app.store.feed(query.since, query.limit, query.actor.as_deref()).await?;
    let cursor = entries.last().map(|entry| entry.id).unwrap_or(query.since);
    Ok(Json(json!({
        "cursor": cursor,
        "lines": entries
            .iter()
            .map(|entry| {
                let mut value = as_json(&entry.line);
                if let Some(object) = value.as_object_mut() {
                    object.insert("id".into(), json!(entry.id));
                }
                value
            })
            .collect::<Vec<_>>(),
    })))
}

/// Advance the world. Optional — the game ticks lazily on any request — but
/// useful as a heartbeat so a Realm nobody is looking at still lives.
///
/// Calling this is *asking* the world to catch up, not telling it to move. When
/// the clock owes nothing the request settles on an unlocked read and returns:
/// no row lock, no write, no version bump. Within one tick window every call
/// after the first is therefore free, which makes the route safe to leave
/// exposed and safe to hammer — a flood costs a single indexed read each.
///
/// `GET` is routed alongside `POST` because Vercel Cron invokes with `GET`;
/// making the operation idempotent is what makes that acceptable.
async fn tick(
    State(app): State<AppState>,
    headers: HeaderMap,
) -> ApiResult<Json<TurnResponse>> {
    if let Some(expected) = &app.cron_secret {
        let provided = auth::bearer(&headers).unwrap_or("");
        if !auth::secret_matches(expected, provided) {
            return Err(ApiError::Forbidden("Not a valid heartbeat.".into()));
        }
    }

    if let Some((state, now)) = app.store.snapshot().await?
        && state.ticks_due(now) == 0
    {
        return Ok(Json(TurnResponse::idle(now)));
    }

    let committed = app.store.commit_turn(Input::CatchUp).await?;
    Ok(Json(committed.into()))
}

async fn admin(
    State(app): State<AppState>,
    headers: HeaderMap,
    Json(command): Json<AdminCommand>,
) -> ApiResult<Json<TurnResponse>> {
    // Absent configuration means the door is bolted, not open.
    let Some(expected) = &app.admin_secret else {
        return Err(ApiError::Forbidden(
            "Administration is not enabled on this Realm.".into(),
        ));
    };
    let provided = auth::bearer(&headers)?;
    if !auth::secret_matches(expected, provided) {
        return Err(ApiError::Forbidden("That is not the King's seal.".into()));
    }
    let committed = app.store.commit_turn(Input::Admin(command)).await?;
    Ok(Json(committed.into()))
}

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

async fn current_player(app: &AppState, headers: &HeaderMap) -> ApiResult<String> {
    let token = auth::bearer(headers)?;
    auth::authenticate(app.store.pool(), token).await
}

/// Read the world, first letting it catch up if the clock owes anything.
///
/// The unlocked read comes first so the common case — a client polling a world
/// that is already current — never touches the game lock at all.
async fn settled(store: &Store) -> ApiResult<(GameState, i64)> {
    match store.snapshot().await? {
        Some((state, now)) if state.ticks_due(now) == 0 => Ok((state, now)),
        // Either the clock is behind, or the Realm has never been opened.
        _ => {
            let committed = store.commit_turn(Input::CatchUp).await?;
            Ok((committed.state, committed.now))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dragon_core::out::{Kind, Line};

    #[test]
    fn a_line_is_rendered_with_both_spans_and_flat_text() {
        let line = Line::event().text(" ").nick("Absalom").text(" has gained 42 XP!");
        let value = as_json(&line);
        assert_eq!(value["text"], " Absalom has gained 42 XP!");
        assert_eq!(value["kind"], "event");
        assert_eq!(value["actors"][0], "Absalom");
        assert!(value["spans"].is_array());
    }

    #[test]
    fn a_plain_line_omits_empty_metadata() {
        let value = as_json(&Line::new(Kind::System).text("hello"));
        assert_eq!(value["text"], "hello");
        assert!(value.get("actors").is_none(), "no actors key when nobody is named");
    }

    #[test]
    fn quest_ids_are_parsed_the_way_players_type_them() {
        assert_eq!(QuestId::parse("0a3f"), Some(QuestId(0x0A3F)));
        assert_eq!(QuestId::parse("0A3F"), Some(QuestId(0x0A3F)));
        assert!(QuestId::parse("not-hex").is_none());
    }

    #[test]
    fn feed_queries_default_to_the_beginning_with_a_sane_limit() {
        let query: FeedQuery = serde_json::from_str("{}").unwrap();
        assert_eq!(query.since, 0);
        assert_eq!(query.limit, 200);
        assert!(query.actor.is_none());
    }

    #[test]
    fn request_bodies_accept_the_shapes_a_client_would_send() {
        let quest: QuestRequest =
            serde_json::from_str(r#"{"quest":"0A3F","strategy":"wise"}"#).unwrap();
        assert_eq!(quest.quest, "0A3F");
        assert_eq!(quest.strategy.as_deref(), Some("wise"));

        // Strategy is optional.
        let quest: QuestRequest = serde_json::from_str(r#"{"quest":"1"}"#).unwrap();
        assert!(quest.strategy.is_none());

        let buy: BuyRequest = serde_json::from_str(r#"{"item":12}"#).unwrap();
        assert_eq!(buy.item, 12);

        let join: JoinRequest = serde_json::from_str(r#"{"nick":"Absalom"}"#).unwrap();
        assert_eq!(join.nick, "Absalom");
    }

    #[test]
    fn admin_commands_deserialise_from_their_tagged_form() {
        let command: AdminCommand =
            serde_json::from_str(r#"{"admin":"run_event","which":16}"#).unwrap();
        assert_eq!(command, AdminCommand::RunEvent { which: Some(16) });

        let command: AdminCommand = serde_json::from_str(r#"{"admin":"kill"}"#).unwrap();
        assert_eq!(command, AdminCommand::Kill);

        let command: AdminCommand =
            serde_json::from_str(r#"{"admin":"remove_quest","quest":4660}"#).unwrap();
        assert_eq!(command, AdminCommand::RemoveQuest { quest: QuestId(0x1234) });
    }

    #[test]
    fn a_committed_turn_becomes_a_response_without_losing_anything() {
        use dragon_core::step::Turn;
        let mut out = dragon_core::Out::new();
        out.broadcast(Line::quest().text("public news"));
        out.reply(Line::system().text("just for you"));
        out.stamp(1_700_000_000);

        let (blank, _) = dragon_core::start(1_700_000_000, 1, Default::default());
        let response = TurnResponse::from(Committed {
            turn: Turn { out, ascension: Some("someone ascended".into()), ticks_replayed: 3, ticks_skipped: 1 },
            state: blank,
            cursor: Some(99),
            now: 1_700_000_000,
        });

        assert_eq!(response.feed.len(), 1);
        assert_eq!(response.reply.len(), 1);
        assert_eq!(response.feed[0]["text"], "public news");
        assert_eq!(response.reply[0]["text"], "just for you");
        assert_eq!(response.cursor, Some(99));
        assert_eq!(response.ticks_replayed, 3);
        assert_eq!(response.ascension.as_deref(), Some("someone ascended"));
    }
}
