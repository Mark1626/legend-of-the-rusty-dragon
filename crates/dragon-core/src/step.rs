//! The one entry point: `step(&mut state, input, now) -> Turn`.
//!
//! Every request the hosting layer serves reduces to this call, sandwiched
//! between loading a row and writing it back.
//!
//! # The clock
//!
//! There is no timer to run the world, so the world runs when someone looks at
//! it. Each turn first settles whatever the clock owes: [`GameState::last_tick_at`]
//! is a *virtual* clock, and catch-up replays the missed ticks, dating each
//! one's output to the moment it would have happened. A player returning after
//! forty quiet minutes sees a world that was visibly running the whole time,
//! not thirteen events stamped with the same second.
//!
//! Two things keep that honest. Replayed ticks are handed their own virtual
//! `now`, so rest timers and elapsed-time displays are computed against the
//! right moment rather than drifting by the length of the gap. And the replay
//! is capped by [`Pacing::max_catchup_ticks`]: past that the clock is skipped
//! forward with a single note, so the first visitor to a Realm that has been
//! dormant for days does not pay to simulate all of them.

use serde::{Deserialize, Serialize};

use crate::assets::ItemId;
use crate::config::{Pacing, time_display};
use crate::event;
use crate::out::{Line, Out};
use crate::quest::QuestId;
use crate::rng::GameRng;
use crate::state::{GameState, NOT_REGISTERED};

/// What someone is asking the world to do.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "command", rename_all = "snake_case")]
pub enum Input {
    /// Settle the clock and nothing else: the cron and heartbeat path.
    CatchUp,
    /// Enter the Realm without taking a quest yet.
    Join { nick: String },
    /// Accept a quest, optionally changing strategy first.
    Quest {
        nick: String,
        quest: QuestId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        strategy: Option<String>,
    },
    /// Buy an item from Absalom's store.
    Buy { nick: String, item: ItemId },
    /// Ask for one's own statistics.
    AboutMe { nick: String },
    /// Read out the leaderboard.
    Scores,
    Admin(AdminCommand),
}

/// The reference's admin commands, carried over. The hosting layer is
/// responsible for proving the caller is entitled to these.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "admin", rename_all = "snake_case")]
pub enum AdminCommand {
    /// Read the whole Bulletin Board into the feed.
    ShowBoard,
    /// Stop the world.
    Kill,
    /// Discard the Realm and start a fresh one.
    Restart { seed: u64 },
    /// Force an event; a random one if unspecified.
    RunEvent {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        which: Option<i64>,
    },
    /// Post a quest, optionally at a chosen difficulty.
    AddQuest {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        level: Option<i64>,
    },
    /// Retire a specific quest.
    RemoveQuest { quest: QuestId },
}

/// Everything a turn produced.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Turn {
    pub out: Out,
    /// Set when a player completed the game, for relaying to other channels.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ascension: Option<String>,
    /// How many world ticks this turn simulated.
    pub ticks_replayed: u64,
    /// How many were skipped past instead, because the Realm had been dormant
    /// longer than [`Pacing::max_catchup_ticks`] allows replaying.
    pub ticks_skipped: u64,
}

/// Run one turn.
pub fn step(state: &mut GameState, input: Input, now: i64) -> Turn {
    let mut rng = GameRng::seed_from_u64(state.rng_seed);
    let mut turn = Turn::default();

    // The world always settles its debt to the clock first, so a command lands
    // in a present that has already caught up with itself.
    let (replayed, skipped) = catch_up(state, now, &mut rng, &mut turn.out);
    turn.ticks_replayed = replayed;
    turn.ticks_skipped = skipped;

    apply(state, input, now, &mut rng, &mut turn);

    // Anything the command itself produced happened now, not during a replay.
    turn.out.stamp(now);
    state.rng_seed = rng.next_seed();
    turn
}

/// Bring [`GameState::last_tick_at`] up to `now`. Returns how many ticks were
/// replayed and how many were skipped past.
fn catch_up(
    state: &mut GameState,
    now: i64,
    rng: &mut GameRng,
    out: &mut Out,
) -> (u64, u64) {
    let due = state.ticks_due(now);
    if due == 0 {
        return (0, 0);
    }

    let limit = state.pacing.max_catchup_ticks;
    let skipped = due.saturating_sub(limit);
    if skipped > 0 {
        let dormant_for = skipped as i64 * state.pacing.tick_secs;
        state.skip_to(state.last_tick_at + dormant_for);
        out.broadcast(
            Line::system()
                .text("The Realm lay quiet for ")
                .text(time_display(dormant_for))
                .text("."),
        );
        out.stamp(state.last_tick_at);
    }

    let replayed = due.min(limit);
    for _ in 0..replayed {
        // Each tick is handed the moment it would have happened, so rest
        // timers and elapsed-time displays are computed against the right
        // clock rather than against whenever someone finally looked.
        let tick_at = state.last_tick_at + state.pacing.tick_secs;
        state.tick(tick_at, rng, out);
        out.stamp(tick_at);
    }
    (replayed, skipped)
}

fn apply(
    state: &mut GameState,
    input: Input,
    now: i64,
    rng: &mut GameRng,
    turn: &mut Turn,
) {
    let out = &mut turn.out;
    match input {
        Input::CatchUp => {}
        Input::Join { nick } => {
            if state.admit(&nick, now, out) {
                out.reply(
                    Line::system()
                        .text("Welcome to the Realm, ")
                        .nick(&nick)
                        .text("! Accept a quest from the Bulletin Board to begin."),
                );
            } else {
                out.reply(Line::system().text("You are already in the Realm."));
            }
        }
        Input::Quest { nick, quest, strategy } => {
            turn.ascension =
                state.accept_quest(&nick, quest, strategy.as_deref(), now, rng, out);
        }
        Input::Buy { nick, item } => state.buy(&nick, item, rng, out),
        Input::AboutMe { nick } => match state.users.get(&nick) {
            Some(user) => out.reply(user.about(now)),
            None => out.reply(Line::system().text(NOT_REGISTERED)),
        },
        Input::Scores => {
            let mut scores = Out::new();
            state.display_scores(&mut scores);
            out.reply.extend(scores.feed);
        }
        Input::Admin(command) => apply_admin(state, command, now, rng, out),
    }
}

fn apply_admin(
    state: &mut GameState,
    command: AdminCommand,
    now: i64,
    rng: &mut GameRng,
    out: &mut Out,
) {
    match command {
        AdminCommand::ShowBoard => {
            out.reply(Line::bboard().text(" Currently available quests:"));
            let ids: Vec<QuestId> =
                state.bboard.posted().into_iter().map(|(id, _)| id).collect();
            for id in ids {
                if let Some(line) = state.bboard.describe(id) {
                    out.reply(line);
                }
            }
        }
        AdminCommand::Kill => state.kill(out),
        AdminCommand::Restart { seed } => {
            let pacing = state.pacing.clone();
            let (fresh, opening) = GameState::new(now, seed, pacing);
            *state = fresh;
            out.absorb(opening);
        }
        AdminCommand::RunEvent { which } => event::run(state, which, now, rng, out),
        AdminCommand::AddQuest { level } => {
            let id = state.bboard.add_quest(level, now, rng);
            if let Some(line) = state.bboard.describe(id) {
                out.broadcast(line);
            }
        }
        AdminCommand::RemoveQuest { quest } => {
            if state.bboard.contains(quest) {
                state.bboard.remove_quest(quest, rng, out);
            } else {
                out.reply(Line::bboard().text("Wrong quest ID: ").text(quest));
            }
        }
    }
}

/// Start a Realm. The opening announcements come back as a [`Turn`] so the
/// hosting layer can write them to the feed like any other output.
pub fn start(now: i64, seed: u64, pacing: Pacing) -> (GameState, Turn) {
    let (state, mut out) = GameState::new(now, seed, pacing);
    out.stamp(now);
    (state, Turn { out, ..Default::default() })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Cr;

    fn realm() -> (GameState, i64) {
        let now = 1_700_000_000;
        let (state, _) = start(now, 4242, Pacing::WEB);
        (state, now)
    }

    #[test]
    fn a_turn_with_nothing_owed_changes_no_clock() {
        let (mut state, now) = realm();
        let before = (state.tick_turn, state.last_tick_at);
        let turn = step(&mut state, Input::CatchUp, now);
        assert_eq!((state.tick_turn, state.last_tick_at), before);
        assert_eq!((turn.ticks_replayed, turn.ticks_skipped), (0, 0));
    }

    #[test]
    fn catching_up_replays_every_owed_tick() {
        let (mut state, now) = realm();
        let tick = state.pacing.tick_secs;
        let turn = step(&mut state, Input::CatchUp, now + tick * 5);
        assert_eq!(turn.ticks_replayed, 5);
        assert_eq!(turn.ticks_skipped, 0);
        assert_eq!(state.tick_turn, 5);
        assert_eq!(state.last_tick_at, now + tick * 5);
    }

    #[test]
    fn a_replay_dates_each_tick_to_when_it_would_have_happened() {
        let (mut state, now) = realm();
        let tick = state.pacing.tick_secs;
        // Enough players that the events have something to talk about.
        let mut warmup = Out::new();
        for i in 0..8 {
            state.admit(&format!("p{i}"), now, &mut warmup);
        }

        let turn = step(&mut state, Input::CatchUp, now + tick * 10);
        assert_eq!(turn.ticks_replayed, 10);

        let stamps: Vec<i64> = turn.out.feed.iter().filter_map(|l| l.at).collect();
        assert_eq!(stamps.len(), turn.out.feed.len(), "every line is dated");
        assert!(stamps.windows(2).all(|w| w[0] <= w[1]), "dates run forward");
        assert!(
            stamps.iter().all(|&at| at > now && at <= now + tick * 10),
            "dates land inside the gap, not all at the end: {stamps:?}"
        );
        assert!(
            stamps.first() != stamps.last(),
            "a ten-tick replay should spread across the window"
        );
    }

    #[test]
    fn a_long_dormancy_is_skipped_rather_than_replayed() {
        let (mut state, now) = realm();
        let tick = state.pacing.tick_secs;
        let limit = state.pacing.max_catchup_ticks;
        let dormant = limit * 40;

        let turn = step(&mut state, Input::CatchUp, now + tick * dormant as i64);
        assert_eq!(turn.ticks_replayed, limit);
        assert_eq!(turn.ticks_skipped, dormant - limit);
        // The clock still ends up exactly where it should.
        assert_eq!(state.tick_turn, dormant);
        assert_eq!(state.last_tick_at, now + tick * dormant as i64);
        assert!(
            turn.out.transcript().iter().any(|l| l.contains("lay quiet for")),
            "a dormancy should be acknowledged"
        );
    }

    #[test]
    fn a_dormant_realm_costs_a_bounded_amount_of_work() {
        let (mut state, now) = realm();
        let mut out = Out::new();
        for i in 0..10 {
            state.admit(&format!("p{i}"), now, &mut out);
        }
        // A year of silence must not be simulated.
        let turn = step(&mut state, Input::CatchUp, now + 365 * 24 * 3_600);
        assert_eq!(turn.ticks_replayed, state.pacing.max_catchup_ticks);
        assert!(turn.ticks_skipped > 100_000);
    }

    #[test]
    fn the_clock_lands_on_the_same_place_however_it_is_driven() {
        let tick = Pacing::WEB.tick_secs;
        let (mut steady, now) = realm();
        let (mut bursty, _) = realm();

        for n in 1..=40 {
            step(&mut steady, Input::CatchUp, now + tick * n);
        }
        step(&mut bursty, Input::CatchUp, now + tick * 40);

        assert_eq!(steady.tick_turn, bursty.tick_turn);
        assert_eq!(steady.last_tick_at, bursty.last_tick_at);
    }

    #[test]
    fn a_command_settles_the_clock_before_acting() {
        let (mut state, now) = realm();
        let tick = state.pacing.tick_secs;
        let turn = step(&mut state, Input::Join { nick: "Absalom".into() }, now + tick * 3);
        assert_eq!(turn.ticks_replayed, 3);
        assert!(state.users.contains_key("Absalom"));
    }

    #[test]
    fn joining_is_announced_once_and_then_refused() {
        let (mut state, now) = realm();
        let turn = step(&mut state, Input::Join { nick: "Absalom".into() }, now);
        assert!(turn.out.transcript()[0].contains("has entered the Realm!"));
        assert!(turn.out.reply[0].plain().contains("Welcome to the Realm"));

        let turn = step(&mut state, Input::Join { nick: "Absalom".into() }, now);
        assert!(turn.out.feed.is_empty());
        assert!(turn.out.reply[0].plain().contains("already in the Realm"));
    }

    #[test]
    fn a_quest_command_resolves_a_fight() {
        let (mut state, now) = realm();
        let quest = state.bboard.posted()[0].0;
        let turn = step(
            &mut state,
            Input::Quest { nick: "Absalom".into(), quest, strategy: Some("wise".into()) },
            now,
        );
        let said = turn.out.transcript().join(" ");
        assert!(said.contains("has accepted quest"), "{said}");
        assert_eq!(state.users["Absalom"].strategy, crate::PlayerStrategy::Wise);
    }

    #[test]
    fn buying_and_asking_about_yourself_answer_privately() {
        let (mut state, now) = realm();
        step(&mut state, Input::Join { nick: "Absalom".into() }, now);
        state.users.get_mut("Absalom").unwrap().money = 1_000_000;

        let stocked = state.shop.stock()[0].0;
        let turn = step(&mut state, Input::Buy { nick: "Absalom".into(), item: stocked }, now);
        assert!(turn.out.reply.iter().any(|l| l.plain().contains("You have bought")));

        let turn = step(&mut state, Input::AboutMe { nick: "Absalom".into() }, now);
        assert_eq!(turn.out.reply.len(), 1);
        assert!(turn.out.reply[0].plain().starts_with("Your stats: level 1"));
    }

    #[test]
    fn a_stranger_asking_about_themselves_is_pointed_at_the_quest_command() {
        let (mut state, now) = realm();
        let turn = step(&mut state, Input::AboutMe { nick: "Nobody".into() }, now);
        assert!(turn.out.reply[0].plain().contains("not in the database"));
        assert!(turn.out.feed.is_empty());
    }

    #[test]
    fn scores_are_private_to_the_asker() {
        let (mut state, now) = realm();
        step(&mut state, Input::Join { nick: "Absalom".into() }, now);
        let turn = step(&mut state, Input::Scores, now);
        assert!(turn.out.reply[0].plain().contains("Best adventurers:"));
        assert!(
            turn.out.feed.iter().all(|l| !l.plain().contains("Best adventurers")),
            "the leaderboard should not spam the channel"
        );
    }

    #[test]
    fn every_line_a_turn_produces_is_dated() {
        let (mut state, now) = realm();
        let tick = state.pacing.tick_secs;
        for i in 0..6 {
            step(&mut state, Input::Join { nick: format!("p{i}") }, now);
        }
        let quest = state.bboard.posted()[0].0;
        let turn = step(
            &mut state,
            Input::Quest { nick: "p0".into(), quest, strategy: None },
            now + tick * 4,
        );
        assert!(turn.out.feed.iter().all(|l| l.at.is_some()));
        assert!(turn.out.reply.iter().all(|l| l.at.is_some()));
    }

    #[test]
    fn the_random_stream_advances_every_turn() {
        let (mut state, now) = realm();
        let mut seeds = std::collections::HashSet::new();
        for i in 0..50 {
            step(&mut state, Input::Join { nick: format!("p{i}") }, now);
            seeds.insert(state.rng_seed);
        }
        assert_eq!(seeds.len(), 50, "the seed must move on every turn");
    }

    #[test]
    fn a_turn_is_reproducible_from_the_persisted_state() {
        let (mut a, now) = realm();
        let tick = a.pacing.tick_secs;
        let mut out = Out::new();
        for i in 0..8 {
            a.admit(&format!("p{i}"), now, &mut out);
        }
        // Round-tripping through storage must not change what happens next.
        let stored = serde_json::to_string(&a).unwrap();
        let mut b: GameState = serde_json::from_str(&stored).unwrap();

        let ta = step(&mut a, Input::CatchUp, now + tick * 12);
        let tb = step(&mut b, Input::CatchUp, now + tick * 12);
        assert_eq!(ta.out.transcript(), tb.out.transcript());
        assert_eq!(a, b);
    }

    #[test]
    fn admin_can_read_the_board_privately() {
        let (mut state, now) = realm();
        let turn = step(&mut state, Input::Admin(AdminCommand::ShowBoard), now);
        assert_eq!(turn.out.reply.len(), state.bboard.len() + 1);
        assert!(turn.out.reply[0].plain().contains("Currently available quests"));
        assert!(turn.out.feed.is_empty());
    }

    #[test]
    fn admin_can_stop_and_restart_the_realm() {
        let (mut state, now) = realm();
        let quest = state.bboard.posted()[0].0;
        step(&mut state, Input::Quest { nick: "Absalom".into(), quest, strategy: None }, now);

        step(&mut state, Input::Admin(AdminCommand::Kill), now);
        assert!(state.killed);
        // A stopped Realm no longer advances.
        let tick = state.pacing.tick_secs;
        let turn = step(&mut state, Input::CatchUp, now + tick * 20);
        assert_eq!(turn.ticks_replayed, 0);

        let turn = step(&mut state, Input::Admin(AdminCommand::Restart { seed: 7 }), now);
        assert!(!state.killed);
        assert!(state.users.is_empty(), "a restart clears the Realm");
        assert_eq!(state.bboard.len(), 16);
        assert!(turn.out.transcript().iter().any(|l| l.contains("Pink Dragon")));
    }

    #[test]
    fn admin_can_post_and_retire_quests() {
        let (mut state, now) = realm();
        let before = state.bboard.len();

        let turn = step(&mut state, Input::Admin(AdminCommand::AddQuest { level: Some(9) }), now);
        assert_eq!(state.bboard.len(), before + 1);
        assert!(!turn.out.feed.is_empty());
        let posted = state.bboard.posted().last().unwrap().0;
        assert!(state.bboard.get(posted).unwrap().total_cr() > Cr::from_whole(13));

        step(&mut state, Input::Admin(AdminCommand::RemoveQuest { quest: posted }), now);
        assert_eq!(state.bboard.len(), before);

        let turn = step(
            &mut state,
            Input::Admin(AdminCommand::RemoveQuest { quest: QuestId(0xDEAD) }),
            now,
        );
        assert!(turn.out.reply[0].plain().contains("Wrong quest ID"));
    }

    #[test]
    fn admin_can_force_any_event() {
        let (mut state, now) = realm();
        for i in 0..8 {
            step(&mut state, Input::Join { nick: format!("p{i}") }, now);
        }
        for which in 0..event::EVENT_COUNT {
            step(
                &mut state,
                Input::Admin(AdminCommand::RunEvent { which: Some(which) }),
                now,
            );
        }
        assert!(state.users.values().all(|u| u.money >= 0 && u.xp >= 0));
    }

    #[test]
    fn inputs_survive_a_json_round_trip() {
        let cases = vec![
            Input::CatchUp,
            Input::Join { nick: "Absalom".into() },
            Input::Quest {
                nick: "Absalom".into(),
                quest: QuestId(0x1A2B),
                strategy: Some("harsh".into()),
            },
            Input::Buy { nick: "Absalom".into(), item: 3 },
            Input::AboutMe { nick: "Absalom".into() },
            Input::Scores,
            Input::Admin(AdminCommand::RunEvent { which: Some(16) }),
            Input::Admin(AdminCommand::Restart { seed: 99 }),
        ];
        for input in cases {
            let json = serde_json::to_string(&input).unwrap();
            assert_eq!(serde_json::from_str::<Input>(&json).unwrap(), input, "{json}");
        }
    }

    #[test]
    fn a_realm_survives_a_long_unattended_life() {
        // The whole point of the port: nobody is watching, and it still has to
        // hold together when someone finally looks.
        let (mut state, now) = realm();
        let tick = state.pacing.tick_secs;
        let mut clock = now;

        for i in 0..12 {
            step(&mut state, Input::Join { nick: format!("p{i:02}") }, clock);
        }
        for round in 0..300usize {
            clock += tick * (1 + (round % 7) as i64);
            let nick = format!("p{:02}", round % 12);
            let quest = state.bboard.posted()[round % state.bboard.len()].0;
            let turn = step(
                &mut state,
                Input::Quest { nick: nick.clone(), quest, strategy: None },
                clock,
            );

            assert!(!state.bboard.is_empty(), "the board emptied at round {round}");
            for user in state.users.values() {
                assert!(user.money >= 0, "{} went into debt", user.nick);
                assert!(user.xp >= 0, "{} lost more XP than they had", user.nick);
                assert!((1..=20).contains(&user.level), "{} hit level {}", user.nick, user.level);
            }
            assert!(turn.out.feed.iter().all(|l| l.at.is_some()));
        }
        assert!(state.tick_turn > 300, "the world should have moved on");
    }
}
