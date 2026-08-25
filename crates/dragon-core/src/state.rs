//! The whole world, in one serialisable value.
//!
//! Everything a turn can read or change lives here, so the hosting layer's job
//! reduces to: load this, call a method, write it back.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::assets::ItemId;
use crate::bboard::BBoard;
use crate::config::{Pacing, time_display};
use crate::event::EventState;
use crate::numeric::Cr;
use crate::out::{Line, Out};
use crate::quest::QuestId;
use crate::rng::GameRng;
use crate::shop::Shop;
use crate::user::User;
use crate::{event, version};

/// Bumped when the shape of a persisted game changes incompatibly.
pub const SCHEMA_VERSION: u32 = 1;

/// The four-beat world cycle. Events land on two of the four beats, so they
/// happen twice as often as a board or store rotation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Beat {
    BulletinBoard,
    Event,
    Store,
}

impl Beat {
    fn of(tick_turn: u64) -> Beat {
        match tick_turn % 4 {
            1 => Beat::BulletinBoard,
            3 => Beat::Store,
            _ => Beat::Event,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GameState {
    #[serde(default = "default_schema")]
    pub schema: u32,
    #[serde(default)]
    pub pacing: Pacing,
    /// The random stream's position. Persisting it is what makes a turn
    /// reproducible from its inputs.
    pub rng_seed: u64,
    pub tick_turn: u64,
    /// The virtual clock. Catch-up replays advance this in whole ticks, so a
    /// quiet stretch is filled in at the times things would have happened.
    pub last_tick_at: i64,
    pub started_at: i64,
    #[serde(default)]
    pub killed: bool,
    pub users: BTreeMap<String, User>,
    pub bboard: BBoard,
    pub shop: Shop,
    #[serde(default)]
    pub events: EventState,
}

fn default_schema() -> u32 {
    SCHEMA_VERSION
}

impl GameState {
    /// Start a new Realm.
    pub fn new(now: i64, seed: u64, pacing: Pacing) -> (Self, Out) {
        let mut rng = GameRng::seed_from_u64(seed);
        let mut out = Out::new();

        out.broadcast(Line::system().accent(
            crate::out::Color::Red,
            "** The Legend of the Rusty Dragon **",
        ));
        out.broadcast(
            Line::system()
                .text("Version ")
                .text(version())
                .text(" of the game"),
        );
        out.announce(format!("New game is starting (version {}).", version()));

        let state = Self {
            schema: SCHEMA_VERSION,
            pacing,
            tick_turn: 0,
            last_tick_at: now,
            started_at: now,
            killed: false,
            users: BTreeMap::new(),
            bboard: BBoard::new(now, &mut rng),
            shop: Shop::new(&mut rng),
            events: EventState::default(),
            rng_seed: rng.next_seed(),
        };
        (state, out)
    }

    pub fn uptime(&self, now: i64) -> i64 {
        (now - self.started_at).max(0)
    }

    /// How many whole ticks are owed, given the virtual clock.
    pub fn ticks_due(&self, now: i64) -> u64 {
        if self.killed || self.pacing.tick_secs <= 0 {
            return 0;
        }
        let elapsed = now - self.last_tick_at;
        if elapsed <= 0 {
            0
        } else {
            (elapsed / self.pacing.tick_secs) as u64
        }
    }

    /// Run one beat of the world at virtual time `now`.
    pub fn tick(&mut self, now: i64, rng: &mut GameRng, out: &mut Out) {
        if self.killed {
            return;
        }
        self.tick_turn += 1;
        self.last_tick_at += self.pacing.tick_secs;

        match Beat::of(self.tick_turn) {
            Beat::BulletinBoard => self.bboard.tick(now, rng, out),
            Beat::Store => self.shop.tick(rng, out),
            Beat::Event => event::run(self, None, now, rng, out),
        }
        self.purge(now, out);
    }

    /// Skip the clock forward without simulating, used when a Realm has been
    /// quiet for longer than it is worth replaying.
    pub fn skip_to(&mut self, target_tick_at: i64) {
        let skipped = (target_tick_at - self.last_tick_at) / self.pacing.tick_secs.max(1);
        if skipped > 0 {
            self.tick_turn += skipped as u64;
            self.last_tick_at += skipped * self.pacing.tick_secs;
        }
    }

    /// Show out anyone who has not taken a quest in a very long time.
    pub fn purge(&mut self, now: i64, out: &mut Out) {
        let gone: Vec<String> = self
            .users
            .values()
            .filter(|u| now - u.last_quest_at > self.pacing.purge_after_secs)
            .map(|u| u.nick.clone())
            .collect();
        for nick in gone {
            self.users.remove(&nick);
            out.broadcast(
                Line::system()
                    .nick(&nick)
                    .text(" has left the Realm with no glory."),
            );
        }
    }

    /// Stop the world. Existing state is kept so it can still be read.
    pub fn kill(&mut self, out: &mut Out) {
        self.killed = true;
        out.broadcast(
            Line::event()
                .text(" ")
                .colored(crate::out::Color::Red, "The current game is now stopped."),
        );
    }

    /// Register a newcomer, announcing their arrival.
    pub fn admit(&mut self, nick: &str, now: i64, out: &mut Out) -> bool {
        if self.users.contains_key(nick) {
            return false;
        }
        self.users.insert(nick.to_string(), User::new(nick, now));
        out.broadcast(Line::system().nick(nick).text(" has entered the Realm!"));
        true
    }

    /// Buy an item from the store.
    pub fn buy(&mut self, nick: &str, item: ItemId, rng: &mut GameRng, out: &mut Out) {
        let Some(mut user) = self.users.remove(nick) else {
            out.reply(Line::store().text(NOT_REGISTERED));
            return;
        };
        self.shop.sell(&mut user, item, rng, out);
        self.users.insert(user.nick.clone(), user);
    }

    /// Accept a quest from the Bulletin Board.
    ///
    /// Returns the ascension announcement if this completed the game for the
    /// player, which the caller relays to any external channel.
    pub fn accept_quest(
        &mut self,
        nick: &str,
        id: QuestId,
        strategy: Option<&str>,
        now: i64,
        rng: &mut GameRng,
        out: &mut Out,
    ) -> Option<String> {
        self.admit(nick, now, out);
        let mut user = self.users.remove(nick)?;
        user.last_quest_at = now;

        if let Some(name) = strategy
            && let Some(accepted) = user.set_strategy(name)
        {
            out.reply(
                Line::system()
                    .text("Your strategy is now \"")
                    .text(accepted)
                    .text("\"."),
            );
        }

        let known = self.bboard.contains(id);
        if !known {
            out.reply(
                Line::quest()
                    .text("The quest ")
                    .text(id)
                    .text(" is currently unavailable!"),
            );
        }

        let mut ascension = None;
        if known && !user.is_resting(now) {
            let quest = self.bboard.get(id).expect("just checked").clone();
            out.broadcast(
                Line::quest()
                    .text(" ")
                    .nick(&user.nick)
                    .text(" has accepted quest ")
                    .bold(id)
                    .text(" (total CR = ")
                    .text(quest.total_cr())
                    .text("): ")
                    .text(quest.display())
                    .text("."),
            );

            let won = user.fight(&quest, &self.pacing, now, rng, out);
            out.broadcast(if won {
                Line::quest()
                    .text(" ")
                    .nick(&user.nick)
                    .text(" has won! Quest ")
                    .bold(id)
                    .text(" has been removed from the Bulletin Board.")
            } else {
                Line::quest()
                    .text(" ")
                    .nick(&user.nick)
                    .text(" has lost! Quest ")
                    .bold(id)
                    .text(" is still available.")
            });

            if won {
                if quest.total_cr() >= self.pacing.ascension_cr {
                    ascension = Some(self.ascend(&mut user, now, out));
                    // An ascended adventurer leaves the Realm entirely; the
                    // quest stays posted for whoever comes next.
                    return ascension;
                }
                self.bboard.take(id);
                self.bboard.replenish(now, rng, out);
            }
        }

        // Always report the rest timer, even when the quest was unavailable.
        let resting = user.rest_remaining(now);
        if resting > 0 {
            out.reply(
                Line::quest()
                    .text("You must rest for ")
                    .text(time_display(resting))
                    .text("!"),
            );
        }

        self.users.insert(user.nick.clone(), user);
        ascension
    }

    /// The player has completed the game: celebrate, then show them out.
    fn ascend(&mut self, user: &mut User, now: i64, out: &mut Out) -> String {
        let played = time_display(now - user.created_at);
        out.broadcast(Line::system().bold("** New Ascension **"));
        out.broadcast(Line::system().bold("** ").nick(&user.nick).bold(format!(
            " (level {}) has completed the game in {played}! **",
            user.level
        )));
        out.broadcast(
            Line::system()
                .nick(&user.nick)
                .text(" has now left the Realm with ")
                .text(user.xp)
                .text(" XP."),
        );
        out.broadcast(
            Line::system()
                .nick(&user.nick)
                .text(" is allowed to reincarnate and start again!"),
        );

        user.idle_until = 0;
        self.users.remove(&user.nick);

        let announcement = format!(
            "{} has ascended after having played for {played} (level {} with {} XP)!",
            user.nick, user.level, user.xp
        );
        out.announce(announcement.clone());
        announcement
    }

    /// Players ordered for the leaderboard: highest level, then highest XP.
    pub fn ranking(&self) -> Vec<&User> {
        let mut ranked: Vec<&User> = self.users.values().collect();
        ranked.sort_by_key(|u| (std::cmp::Reverse(u.level), std::cmp::Reverse(u.xp), &u.nick));
        ranked
    }

    /// Read out the leaderboard.
    pub fn display_scores(&self, out: &mut Out) {
        if self.users.is_empty() {
            return;
        }
        out.broadcast(Line::system().bold("Best adventurers:"));
        for user in self.ranking() {
            out.broadcast(
                Line::system()
                    .text("| ")
                    .nick(&user.nick)
                    .text(format!(" (level = {}, XP = {})", user.level, user.xp)),
            );
        }
    }

    /// The total challenge rating currently posted.
    pub fn posted_cr(&self) -> Cr {
        self.bboard.total_cr()
    }
}

pub const NOT_REGISTERED: &str =
    "You are not in the database; start playing by using the 'quest' command!";

#[cfg(test)]
mod tests {
    use super::*;
    use crate::assets;

    fn new_game() -> GameState {
        GameState::new(1_000_000, 42, Pacing::WEB).0
    }

    #[test]
    fn a_new_realm_opens_stocked_and_announced() {
        let (state, out) = GameState::new(1_000, 7, Pacing::WEB);
        assert_eq!(state.tick_turn, 0);
        assert_eq!(state.last_tick_at, 1_000);
        assert!(state.users.is_empty());
        assert_eq!(state.bboard.len(), 16);
        assert_eq!(state.shop.total_items(), 12);
        assert!(!state.killed);

        let said = out.transcript().join(" ");
        assert!(said.contains("The Legend of the Rusty Dragon"), "{said}");
        assert!(said.contains(version()), "{said}");
        assert_eq!(out.announce.len(), 1);
    }

    #[test]
    fn the_world_cycle_alternates_board_event_store_event() {
        assert_eq!(Beat::of(1), Beat::BulletinBoard);
        assert_eq!(Beat::of(2), Beat::Event);
        assert_eq!(Beat::of(3), Beat::Store);
        assert_eq!(Beat::of(4), Beat::Event);
        assert_eq!(Beat::of(5), Beat::BulletinBoard);
        // Events get half the beats.
        let events = (1..=100).filter(|&t| Beat::of(t) == Beat::Event).count();
        assert_eq!(events, 50);
    }

    #[test]
    fn ticks_due_counts_whole_ticks_only() {
        let mut state = new_game();
        let tick = state.pacing.tick_secs;
        assert_eq!(state.ticks_due(state.last_tick_at), 0);
        assert_eq!(state.ticks_due(state.last_tick_at + tick - 1), 0);
        assert_eq!(state.ticks_due(state.last_tick_at + tick), 1);
        assert_eq!(state.ticks_due(state.last_tick_at + tick * 7 + 5), 7);
        // Clocks that run backwards must not owe negative ticks.
        assert_eq!(state.ticks_due(state.last_tick_at - 10_000), 0);

        state.killed = true;
        assert_eq!(state.ticks_due(state.last_tick_at + tick * 5), 0);
    }

    #[test]
    fn ticking_advances_the_virtual_clock_by_exactly_one_tick() {
        let mut state = new_game();
        let mut rng = GameRng::seed_from_u64(1);
        let mut out = Out::new();
        let tick = state.pacing.tick_secs;
        let started = state.last_tick_at;

        for n in 1..=20 {
            state.tick(started + n * tick, &mut rng, &mut out);
            assert_eq!(state.tick_turn, n as u64);
            assert_eq!(state.last_tick_at, started + n * tick);
        }
    }

    #[test]
    fn a_killed_realm_stops_ticking() {
        let mut state = new_game();
        let mut rng = GameRng::seed_from_u64(2);
        let mut out = Out::new();
        state.kill(&mut out);
        let before = (state.tick_turn, state.last_tick_at);
        state.tick(state.last_tick_at + 100_000, &mut rng, &mut out);
        assert_eq!((state.tick_turn, state.last_tick_at), before);
        assert!(out.transcript().join(" ").contains("now stopped"));
    }

    #[test]
    fn skipping_ahead_jumps_whole_ticks() {
        let mut state = new_game();
        let tick = state.pacing.tick_secs;
        let target = state.last_tick_at + tick * 500 + 17;
        state.skip_to(target);
        assert_eq!(state.tick_turn, 500);
        assert_eq!(state.last_tick_at, state.last_tick_at);
        assert!(state.ticks_due(target) == 0);
    }

    #[test]
    fn skipping_backwards_does_nothing() {
        let mut state = new_game();
        let before = (state.tick_turn, state.last_tick_at);
        state.skip_to(state.last_tick_at - 50_000);
        assert_eq!((state.tick_turn, state.last_tick_at), before);
    }

    #[test]
    fn admitting_a_newcomer_announces_them_once() {
        let mut state = new_game();
        let mut out = Out::new();
        assert!(state.admit("Absalom", 1_000, &mut out));
        assert!(state.users.contains_key("Absalom"));
        assert!(out.transcript()[0].contains("has entered the Realm!"));

        let mut out = Out::new();
        assert!(!state.admit("Absalom", 2_000, &mut out));
        assert!(
            out.feed.is_empty(),
            "a returning player is not re-announced"
        );
    }

    #[test]
    fn the_inactivity_purge_only_takes_the_long_absent() {
        let mut state = new_game();
        let mut out = Out::new();
        let now = 1_000_000;
        state.admit("Recent", now, &mut out);
        state.admit("Ancient", now, &mut out);
        state.users.get_mut("Ancient").unwrap().last_quest_at =
            now - state.pacing.purge_after_secs - 1;

        let mut out = Out::new();
        state.purge(now, &mut out);
        assert!(state.users.contains_key("Recent"));
        assert!(!state.users.contains_key("Ancient"));
        assert!(out.transcript()[0].contains("has left the Realm with no glory."));
    }

    #[test]
    fn buying_without_registering_is_refused() {
        let mut state = new_game();
        let mut rng = GameRng::seed_from_u64(3);
        let mut out = Out::new();
        state.buy("Nobody", 0, &mut rng, &mut out);
        assert!(out.reply[0].plain().contains("not in the database"));
    }

    #[test]
    fn buying_goes_through_the_store_and_keeps_the_player() {
        let mut state = new_game();
        let mut rng = GameRng::seed_from_u64(4);
        let mut out = Out::new();
        state.admit("Absalom", 1_000, &mut out);
        state.users.get_mut("Absalom").unwrap().money = 1_000_000;

        let stocked = state.shop.stock()[0].0;
        let mut out = Out::new();
        state.buy("Absalom", stocked, &mut rng, &mut out);

        let user = &state.users["Absalom"];
        assert!(user.weapon == Some(stocked) || user.armor == Some(stocked));
        assert!(user.money < 1_000_000);
    }

    #[test]
    fn accepting_a_quest_admits_a_newcomer_and_resolves_a_fight() {
        let mut state = new_game();
        let mut rng = GameRng::seed_from_u64(5);
        let mut out = Out::new();
        let now = 1_000_000;
        let id = state.bboard.posted()[0].0;

        assert!(
            state
                .accept_quest("Absalom", id, None, now, &mut rng, &mut out)
                .is_none()
        );
        assert!(state.users.contains_key("Absalom"));

        let said = out.transcript().join(" ");
        assert!(said.contains("has entered the Realm!"), "{said}");
        assert!(said.contains("has accepted quest"), "{said}");
        assert!(
            said.contains("has won!") || said.contains("has lost!"),
            "{said}"
        );
        assert_eq!(state.users["Absalom"].last_quest_at, now);
    }

    #[test]
    fn an_unknown_quest_is_reported_and_costs_nothing() {
        let mut state = new_game();
        let mut rng = GameRng::seed_from_u64(6);
        let mut out = Out::new();
        let now = 1_000_000;
        state.accept_quest("Absalom", QuestId(0xDEAD), None, now, &mut rng, &mut out);

        assert!(
            out.reply
                .iter()
                .any(|l| l.plain().contains("currently unavailable"))
        );
        let user = &state.users["Absalom"];
        assert_eq!((user.quest_win, user.quest_lost), (0, 0));
        assert!(!user.is_resting(now));
    }

    #[test]
    fn a_resting_adventurer_cannot_start_another_quest() {
        let mut state = new_game();
        let mut rng = GameRng::seed_from_u64(7);
        let mut out = Out::new();
        let now = 1_000_000;
        state.admit("Absalom", now, &mut out);
        state.users.get_mut("Absalom").unwrap().idle_until = now + 500;

        let id = state.bboard.posted()[0].0;
        let mut out = Out::new();
        state.accept_quest("Absalom", id, None, now, &mut rng, &mut out);

        assert!(out.feed.is_empty(), "no fight is broadcast");
        assert!(
            out.reply
                .iter()
                .any(|l| l.plain().contains("You must rest for"))
        );
        assert!(state.bboard.contains(id), "the quest stays posted");
    }

    #[test]
    fn setting_a_strategy_is_confirmed_and_remembered() {
        let mut state = new_game();
        let mut rng = GameRng::seed_from_u64(8);
        let mut out = Out::new();
        let id = state.bboard.posted()[0].0;
        state.accept_quest("Absalom", id, Some("wise"), 1_000_000, &mut rng, &mut out);

        assert!(
            out.reply
                .iter()
                .any(|l| l.plain().contains("strategy is now \"wise\""))
        );
        assert_eq!(state.users["Absalom"].strategy, crate::PlayerStrategy::Wise);
    }

    #[test]
    fn an_unknown_strategy_is_ignored_silently() {
        let mut state = new_game();
        let mut rng = GameRng::seed_from_u64(9);
        let mut out = Out::new();
        let id = state.bboard.posted()[0].0;
        state.accept_quest("A", id, Some("reckless"), 1_000_000, &mut rng, &mut out);
        assert!(
            !out.reply
                .iter()
                .any(|l| l.plain().contains("strategy is now"))
        );
        assert_eq!(state.users["A"].strategy, crate::PlayerStrategy::Random);
    }

    #[test]
    fn completing_a_legendary_quest_ascends_the_player() {
        let mut state = new_game();
        let mut rng = GameRng::seed_from_u64(10);
        let mut out = Out::new();
        let now = 2_000_000;

        // An unbeatable hero against a quest that clears the ascension bar.
        state.admit("Hero", now - 100_000, &mut out);
        {
            let hero = state.users.get_mut("Hero").unwrap();
            hero.level = 20;
            hero.hp = 100_000;
            hero.strength = 200;
            hero.money = 100_000_000;
            hero.buy(
                assets::items()
                    .iter()
                    .find(|i| i.name == "Greatsword")
                    .unwrap()
                    .id,
            );
        }
        let frog = assets::monsters()
            .iter()
            .position(|m| m.name == "Frog")
            .unwrap() as u16;
        let legendary = crate::Quest::from_roster(vec![frog; 1], now);
        let id = state.bboard.add_quest(Some(0), now, &mut rng);
        *state.bboard.get_mut(id).unwrap() = legendary;
        // Force the quest over the ascension threshold.
        state.pacing.ascension_cr = Cr::ZERO;

        let mut out = Out::new();
        let announcement = state
            .accept_quest("Hero", id, None, now, &mut rng, &mut out)
            .unwrap();

        assert!(announcement.contains("has ascended"), "{announcement}");
        assert!(
            !state.users.contains_key("Hero"),
            "an ascended player leaves"
        );
        let said = out.transcript().join(" ");
        assert!(said.contains("** New Ascension **"), "{said}");
        assert!(said.contains("reincarnate"), "{said}");
        assert_eq!(out.announce.len(), 1);
    }

    #[test]
    fn the_leaderboard_ranks_by_level_then_experience() {
        let mut state = new_game();
        let mut out = Out::new();
        for (nick, level, xp) in [("Low", 1, 10), ("High", 9, 5), ("Mid", 9, 900)] {
            state.admit(nick, 0, &mut out);
            let u = state.users.get_mut(nick).unwrap();
            u.level = level;
            u.xp = xp;
        }
        let ranked: Vec<&str> = state.ranking().iter().map(|u| u.nick.as_str()).collect();
        assert_eq!(ranked, vec!["Mid", "High", "Low"]);

        let mut out = Out::new();
        state.display_scores(&mut out);
        assert!(out.transcript()[0].contains("Best adventurers:"));
        assert_eq!(out.feed.len(), 4);
    }

    #[test]
    fn an_empty_realm_has_no_leaderboard() {
        let mut out = Out::new();
        new_game().display_scores(&mut out);
        assert!(out.feed.is_empty());
    }

    #[test]
    fn game_state_survives_a_serde_round_trip() {
        let mut state = new_game();
        let mut rng = GameRng::seed_from_u64(11);
        let mut out = Out::new();
        state.admit("Absalom", 1_000, &mut out);
        for n in 1..=10 {
            state.tick(state.last_tick_at + n, &mut rng, &mut out);
        }

        let json = serde_json::to_string(&state).unwrap();
        let back: GameState = serde_json::from_str(&json).unwrap();
        assert_eq!(back, state);
    }

    #[test]
    fn a_persisted_game_survives_new_optional_fields() {
        // Old rows lack `pacing`, `killed`, `events` and `schema`; they must
        // still load rather than stranding an in-flight game.
        let state = new_game();
        let mut value = serde_json::to_value(&state).unwrap();
        let object = value.as_object_mut().unwrap();
        for dropped in ["pacing", "killed", "events", "schema"] {
            object.remove(dropped);
        }
        let back: GameState = serde_json::from_value(value).unwrap();
        assert_eq!(back.pacing, Pacing::default());
        assert_eq!(back.schema, SCHEMA_VERSION);
        assert!(!back.killed);
    }
}
