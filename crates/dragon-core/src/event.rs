//! Random world events.
//!
//! One of twenty-six things happens on each event beat. Most need a quorum of
//! players and quietly do nothing without one, which is why a young Realm feels
//! sleepy and a busy one does not.
//!
//! Two departures from the reference, both noted at their sites: event 24
//! crashes there, and the two hard-coded idle penalties are expressed as
//! fractions of the configured maximum rest so they scale with [`Pacing`].

use serde::{Deserialize, Serialize};

use crate::battle::{Strategy, Warrior, battle};
use crate::config::{Pacing, time_display};
use crate::numeric::{Cr, ability_modifier, bit_length};
use crate::out::{Line, Out};
use crate::quest::Quest;
use crate::rng::GameRng;
use crate::state::GameState;
use crate::user::{EventClass, User, monster_xp, quest_xp};

/// How many distinct events there are.
pub const EVENT_COUNT: i64 = 26;

/// Cap on tie-break rounds in an ability check. Re-rolling settles a tie almost
/// immediately, but a request must never depend on "almost".
const MAX_TIEBREAKS: u32 = 100;

const BATTLE_NAMES: [&str; 9] = [
    "Battle of the Desperate Souls",
    "Battle for the Forgotten Realm",
    "Battle for the Forbidden Glory",
    "Battle for the Salvation of the Rusty Crown",
    "Battle of the Ultimate Hope",
    "Battle of the Fallen Honor",
    "Battle of the Immortal Horror",
    "Battle of the Infinite Despair",
    "Battle of the Legendary Fields",
];

const DRAGON_SIGHTINGS: [&str; 6] = [
    "at the north of the Realm.",
    "at the south of the Realm.",
    "in the eastern Mountains.",
    "in the western Forests.",
    "devouring cattle in the Fields.",
    "hovering above the Castle.",
];

const TIER_NAMES: [&str; 4] = [
    "first tier (levels 1-4)",
    "second tier (levels 5-10)",
    "third tier (levels 11-16)",
    "fourth tier (levels 17-20)",
];

/// World-wide event cooldowns. Per-player ones live on [`User`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct EventState {
    dragon_seen_at: Option<i64>,
    scoreboard_at: Option<i64>,
}

impl EventState {
    fn ready(last: Option<i64>, now: i64, cooldown: i64) -> bool {
        last.is_none_or(|at| now - at >= cooldown)
    }
}

/// Which ability an inter-player check tests.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Ability {
    Dexterity,
    Strength,
}

impl Ability {
    fn label(self) -> &'static str {
        match self {
            Self::Dexterity => "Dexterity",
            Self::Strength => "Strength",
        }
    }

    /// What the winner of the top tier is crowned.
    fn top_tier_title(self) -> &'static str {
        match self {
            Self::Dexterity => "Dude",
            Self::Strength => "Boss",
        }
    }

    fn score(self, user: &User) -> i32 {
        match self {
            Self::Dexterity => user.dexterity,
            Self::Strength => user.strength,
        }
    }
}

/// Run one event. `force` selects a specific one, for testing and admin use.
pub fn run(
    state: &mut GameState,
    force: Option<i64>,
    now: i64,
    rng: &mut GameRng,
    out: &mut Out,
) {
    let roll = force.unwrap_or_else(|| rng.range(0, EVENT_COUNT));
    match roll {
        0 => state.bboard.replace_quests(1, true, now, rng, out),
        1 => {
            out.broadcast(Line::bboard().text(
                " BREAKING NEWS: More and more creeps are prowling each day in the Realm!",
            ));
            let id = state.bboard.add_quest(None, now, rng);
            if let Some(line) = state.bboard.describe(id) {
                out.broadcast(line);
            }
        }
        2 => state.bboard.replenish(now, rng, out),
        3 => {
            if state.bboard.len() > 8
                && let Some(id) = state.bboard.select_to_remove(rng)
            {
                state.bboard.remove_quest(id, rng, out);
            }
        }
        4 => player_battle(state, 2, rng, out),
        5 => player_battle(state, 3, rng, out),
        6 => player_battle(state, 4, rng, out),
        7 => player_battle(state, 5, rng, out),
        8 => {
            let count = rng.range(6, 13) as usize;
            player_battle(state, count, rng, out);
        }
        9 => found_coins(state, now, rng, out),
        10 => dragon_sighting(state, now, rng, out),
        11 | 12 => top_adventurers(state, now, out),
        13 => potion_of_experience(state, now, rng, out),
        14 => potion_of_speed(state, now, rng, out),
        15 => potion_of_ability(state, now, rng, out),
        16 => path_of_glory(state, now, rng, out),
        17..=19 => party_quest(state, 3, Cr::from_whole(70), true, "Epic", now, rng, out),
        20 | 21 => party_quest(state, 4, Cr::from_whole(45), false, "Creepy", now, rng, out),
        22 => ability_check(state, Ability::Dexterity, rng, out),
        23 => ability_check(state, Ability::Strength, rng, out),
        24 => steal_gold(state, now, rng, out),
        25 => merge_quests(state, now, rng, out),
        _ => {}
    }
}

// ---------------------------------------------------------------------------
// player selection
// ---------------------------------------------------------------------------

/// Split the Realm into the four level brackets the reference uses.
fn tiers(state: &GameState) -> [Vec<String>; 4] {
    let mut tiers: [Vec<String>; 4] = Default::default();
    for user in state.users.values() {
        let level = user.level;
        let index = (level - 1) / 5 + i32::from(level == 5) - i32::from(level == 16);
        tiers[index.clamp(0, 3) as usize].push(user.nick.clone());
    }
    tiers
}

/// Draw `count` distinct players that pass `eligible`, or nothing if there
/// aren't that many.
fn sample_players(
    state: &GameState,
    count: usize,
    eligible: impl Fn(&User) -> bool,
    rng: &mut GameRng,
) -> Option<Vec<String>> {
    let pool: Vec<&User> = state.users.values().filter(|u| eligible(u)).collect();
    if pool.len() < count || count == 0 {
        return None;
    }
    Some(
        rng.sample_indices(pool.len(), count)
            .into_iter()
            .map(|i| pool[i].nick.clone())
            .collect(),
    )
}

/// Draw players whose cooldown for `class` has expired, and start it again.
fn players_for_event(
    state: &mut GameState,
    count: usize,
    class: EventClass,
    cooldown: i64,
    now: i64,
    rng: &mut GameRng,
) -> Option<Vec<String>> {
    let picked =
        sample_players(state, count, |u| u.event_ready(class, now, cooldown), rng)?;
    for nick in &picked {
        if let Some(user) = state.users.get_mut(nick) {
            user.mark_event(class, now);
        }
    }
    Some(picked)
}

// ---------------------------------------------------------------------------
// events 4-8: adventurers fighting each other
// ---------------------------------------------------------------------------

fn player_battle(state: &mut GameState, count: usize, rng: &mut GameRng, out: &mut Out) {
    // Only within a tier, so a veteran never farms newcomers.
    let eligible: Vec<Vec<String>> =
        tiers(state).into_iter().filter(|t| t.len() >= count).collect();
    if eligible.is_empty() {
        return;
    }
    let tier = &eligible[rng.choice_index(eligible.len())];
    let Some(fighters) = (if tier.len() == count {
        Some(tier.clone())
    } else {
        Some(rng.sample_indices(tier.len(), count).into_iter().map(|i| tier[i].clone()).collect())
    }) else {
        return;
    };

    let mut line = Line::event().text(" ").text(rng.choice(&BATTLE_NAMES)).text(": ");
    let mut warriors = Vec::with_capacity(fighters.len());
    for nick in &fighters {
        let Some(user) = state.users.get(nick) else { continue };
        // Duels use the character's real numbers, not the quest tuning.
        warriors.push(user.build_warrior(Strategy::Solo(user.strategy), true));
        line = line.nick(nick).text(format!(" (level {})", user.level)).text(", ");
    }
    if warriors.len() < 2 {
        return;
    }

    let outcome = battle(&mut warriors, rng);
    let Some(winner) = outcome.winner.and_then(|w| warriors[w].owner.clone()) else {
        return;
    };

    // Trim the trailing separator the loop leaves behind.
    out.broadcast(trim_trailing(line, ", ").text("."));
    out.broadcast(
        Line::event()
            .text(" Winner of the current battle is ")
            .nick(&winner)
            .text("!"),
    );

    let total_xp: i64 = fighters.iter().filter_map(|n| state.users.get(n)).map(|u| u.xp).sum();
    let winner_xp = state.users.get(&winner).map_or(0, |u| u.xp);
    let spoils = deflate(total_xp - winner_xp);

    let idle_reset = rng.range(0, 3) == 0;
    if let Some(user) = state.users.get_mut(&winner) {
        user.give_xp(spoils, rng, out);
        if idle_reset {
            user.idle_until = 0;
        }
    }
    if idle_reset {
        out.broadcast(
            Line::event()
                .text(" As a special reward, ")
                .nick(&winner)
                .text("'s idle time is set to 0."),
        );
    }
}

/// The reference's recurring `x // x.bit_length()` payout, with its `or 5`
/// floor for players who have yet to earn anything.
fn deflate(amount: i64) -> i64 {
    if amount <= 0 {
        return 5;
    }
    amount / i64::from(bit_length(amount as u64))
}

fn trim_trailing(line: Line, suffix: &str) -> Line {
    let text = line.plain();
    let trimmed = text.strip_suffix(suffix).unwrap_or(&text).to_string();
    // Rebuild preserving who was named, but flattened; the separator is the
    // only thing that needed removing.
    let mut rebuilt = Line::new(line.kind).text(trimmed);
    rebuilt.actors = line.actors;
    rebuilt
}

// ---------------------------------------------------------------------------
// events 9-15: things that happen to one adventurer
// ---------------------------------------------------------------------------

fn found_coins(state: &mut GameState, now: i64, rng: &mut GameRng, out: &mut Out) {
    let cooldown = state.pacing.chance_cooldown_secs;
    let Some(picked) = players_for_event(state, 1, EventClass::Chance, cooldown, now, rng)
    else {
        return;
    };
    let bonus = rng.range(0, 3);
    let Some(user) = state.users.get_mut(&picked[0]) else { return };

    let found = i64::from(user.level).saturating_add(bonus).pow(2) * 15;
    user.money += found;
    out.broadcast(
        Line::event()
            .text(" ")
            .nick(&user.nick)
            .text(" was rummaging in the bush and just found ")
            .text(found)
            .text(" coins! (Current money: ")
            .text(user.money)
            .text(")"),
    );
}

fn dragon_sighting(state: &mut GameState, now: i64, rng: &mut GameRng, out: &mut Out) {
    let cooldown = state.pacing.dragon_sighting_cooldown_secs;
    if !EventState::ready(state.events.dragon_seen_at, now, cooldown) {
        return;
    }
    state.events.dragon_seen_at = Some(now);
    out.broadcast(
        Line::event()
            .text(" BREAKING NEWS: The Pink Dragon was seen ")
            .text(rng.choice(&DRAGON_SIGHTINGS))
            .text(" (Game running for ")
            .text(time_display(state.uptime(now)))
            .text(", with ")
            .text(state.users.len())
            .text(" adventurers now in the Realm)"),
    );
}

fn top_adventurers(state: &mut GameState, now: i64, out: &mut Out) {
    if state.users.is_empty() {
        return;
    }
    let cooldown = state.pacing.scoreboard_cooldown_secs;
    if !EventState::ready(state.events.scoreboard_at, now, cooldown) {
        return;
    }
    state.events.scoreboard_at = Some(now);

    let mut ranked: Vec<(i64, i32, String)> =
        state.users.values().map(|u| (u.xp, u.level, u.nick.clone())).collect();
    ranked.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.2.cmp(&b.2)));

    for (rank, (xp, level, nick)) in ranked.into_iter().take(8).enumerate() {
        out.broadcast(
            Line::event()
                .text("[Top Adventurers] ")
                .nick(&nick)
                .text(format!(", level {level}, has {xp} XP (rank {})", rank + 1)),
        );
    }
}

fn potion_of_experience(state: &mut GameState, now: i64, rng: &mut GameRng, out: &mut Out) {
    let cooldown = state.pacing.potion_cooldown_secs;
    let Some(picked) = players_for_event(state, 1, EventClass::Potion, cooldown, now, rng)
    else {
        return;
    };
    let blessed = rng.below(2) == 1;
    let Some(user) = state.users.get_mut(&picked[0]) else { return };
    let nick = user.nick.clone();
    let base = (user.xp / 10).max(10);

    if blessed {
        out.broadcast(
            Line::event()
                .text(" ")
                .nick(&nick)
                .text(" has found a blessed potion of gain experience!"),
        );
        user.give_xp(base, rng, out);
    } else {
        let lost = base >> 1;
        user.xp = (user.xp - lost).max(0);
        out.broadcast(
            Line::event()
                .text(" ")
                .nick(&nick)
                .text(" has found a cursed potion of gain experience! ")
                .text(&nick)
                .text(" has lost ")
                .text(lost)
                .text(" XP!"),
        );
    }
}

fn potion_of_speed(state: &mut GameState, now: i64, rng: &mut GameRng, out: &mut Out) {
    let cooldown = state.pacing.potion_cooldown_secs;
    let pacing = state.pacing.clone();
    let Some(picked) = players_for_event(state, 1, EventClass::Potion, cooldown, now, rng)
    else {
        return;
    };
    let blessed = rng.below(2) == 1;
    let Some(user) = state.users.get_mut(&picked[0]) else { return };
    let nick = user.nick.clone();

    if blessed {
        user.idle_until = 0;
        out.broadcast(
            Line::event()
                .text(" ")
                .nick(&nick)
                .text(" has found a blessed potion of speed! ")
                .text(&nick)
                .text("'s idle time is set to 0."),
        );
    } else {
        let penalty = paralysis_penalty(&pacing, user.level);
        user.idle_until = (user.idle_until + penalty).max(now + penalty);
        out.broadcast(
            Line::event()
                .text(" ")
                .nick(&nick)
                .text(" has found a cursed potion of paralysis! ")
                .text(penalty)
                .text(" seconds have been added to ")
                .text(&nick)
                .text("'s idle time!"),
        );
    }
}

/// The reference hard-codes `60 * (5 + level)` seconds, which is tuned against
/// its 45-minute ceiling on rest. Expressed as a fraction of that ceiling it
/// reproduces the original exactly and follows the configured pacing.
fn paralysis_penalty(pacing: &Pacing, level: i32) -> i64 {
    pacing.idle_max_secs * i64::from(5 + level) / 45
}

fn potion_of_ability(state: &mut GameState, now: i64, rng: &mut GameRng, out: &mut Out) {
    let cooldown = state.pacing.potion_cooldown_secs;
    let Some(picked) = players_for_event(state, 1, EventClass::Potion, cooldown, now, rng)
    else {
        return;
    };
    let which = rng.range(0, 3);
    let Some(user) = state.users.get_mut(&picked[0]) else { return };

    let boon = match which {
        0 => {
            user.strength += 1;
            "strength! (+1 STR)"
        }
        1 => {
            user.dexterity += 1;
            "dexterity! (+1 DEX)"
        }
        _ => {
            user.hp += 1;
            "endurance! (+1 HP)"
        }
    };
    out.broadcast(
        Line::event()
            .text(" ")
            .nick(&user.nick)
            .text(" has found a blessed potion of ")
            .text(boon),
    );
}

// ---------------------------------------------------------------------------
// event 16: the Path of the Glory
// ---------------------------------------------------------------------------

fn path_of_glory(state: &mut GameState, now: i64, rng: &mut GameRng, out: &mut Out) {
    let cooldown = state.pacing.glory_cooldown_secs;
    let pacing = state.pacing.clone();
    let Some(picked) = players_for_event(state, 1, EventClass::Glory, cooldown, now, rng)
    else {
        return;
    };
    let nick = picked[0].clone();

    // Twenty candidate quests, the eight easiest discarded.
    let mut gauntlet: Vec<Quest> =
        (0..20).map(|_| Quest::random(None, now, rng)).collect();
    gauntlet.sort_by_key(|q| q.total_cr());
    let gauntlet: Vec<Quest> = gauntlet.into_iter().skip(8).collect();
    let stages = gauntlet.len();

    let Some(user) = state.users.get(&nick) else { return };
    // Failure costs nothing beyond the glory, so the rest timer is restored.
    let idle_before = user.idle_until;
    let mut completed = true;

    for (stage, quest) in gauntlet.iter().enumerate() {
        let Some(user) = state.users.get(&nick) else { return };
        out.broadcast(
            Line::event()
                .text(" ")
                .nick(&nick)
                .text(format!(" (level {})", user.level))
                .text(" follows the Path of the Glory. Stage ")
                .text(stage + 1)
                .text(": ")
                .text(quest.display()),
        );

        let Some(user) = state.users.get_mut(&nick) else { return };
        if !user.fight(quest, &pacing, now, rng, out) {
            user.idle_until = idle_before;
            out.broadcast(
                Line::event()
                    .text(" ")
                    .nick(&nick)
                    .text(" was defeated during stage ")
                    .text(stage + 1)
                    .text(" of the Path of Glory!"),
            );
            completed = false;
            break;
        }
    }

    if !completed {
        return;
    }
    out.broadcast(
        Line::event()
            .text(" ")
            .nick(&nick)
            .text(" has completed all ")
            .text(stages)
            .text(" quests on the Path of Glory!"),
    );
    out.broadcast(
        Line::event().text(" The King gives 15,000 coins to ").nick(&nick).text("."),
    );
    out.broadcast(
        Line::event()
            .text(" As a special reward, ")
            .nick(&nick)
            .text("'s idle time is set to 0."),
    );
    if let Some(user) = state.users.get_mut(&nick) {
        user.money += 15_000;
        user.idle_until = 0;
    }
}

// ---------------------------------------------------------------------------
// events 17-21: parties against a set-piece warband
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
fn party_quest(
    state: &mut GameState,
    count: usize,
    threshold: Cr,
    epic: bool,
    label: &str,
    now: i64,
    rng: &mut GameRng,
    out: &mut Out,
) {
    let cooldown = state.pacing.party_quest_cooldown_secs;
    let Some(party) =
        players_for_event(state, count, EventClass::PartyQuest, cooldown, now, rng)
    else {
        return;
    };
    let quest = Quest::assembled(32, threshold, epic, now, rng);

    let mut line = Line::event().text(format!(" {label} Quest: "));
    let mut warriors = Vec::new();
    for nick in &party {
        let Some(user) = state.users.get(nick) else { continue };
        // Party members use the quest tuning and never turn on each other.
        warriors.push(user.build_warrior(Strategy::Party(user.strategy), false));
        line = line.nick(nick).text(format!(" (level {})", user.level)).text(", ");
    }
    if warriors.is_empty() {
        return;
    }
    out.broadcast(
        trim_trailing(line, ", ")
            .text(" are fighting against: ")
            .text(quest.display()),
    );

    for &id in quest.monsters() {
        warriors.push(Warrior::monster(id, Strategy::HuntAnyPlayer, rng));
    }
    let outcome = battle(&mut warriors, rng);
    let survivors_are_players =
        outcome.winner.is_some_and(|w| warriors[w].owner.is_some());

    if survivors_are_players {
        // The spoils are split three ways regardless of party size, as in the
        // reference.
        let share = quest_xp(&quest) / 3;
        out.broadcast(
            Line::event().text(format!(
                " {label} Quest: Adventurers defeated all {}!",
                if epic { "monsters" } else { "creeps" }
            )),
        );
        for nick in &party {
            if let Some(user) = state.users.get_mut(nick) {
                user.give_xp(share, rng, out);
            }
        }
    } else {
        out.broadcast(
            Line::event().text(format!(" {label} Quest: Adventurers are defeated!")),
        );
    }
}

// ---------------------------------------------------------------------------
// events 22-23: ability checks, run per tier
// ---------------------------------------------------------------------------

fn ability_check(
    state: &mut GameState,
    ability: Ability,
    rng: &mut GameRng,
    out: &mut Out,
) {
    for (index, tier) in tiers(state).into_iter().enumerate() {
        if tier.is_empty() {
            continue;
        }
        let mut narration = format!("{} check: {}:", ability.label(), TIER_NAMES[index]);
        let mut remaining = tier;
        let mut round = 0;

        if remaining.len() == 1 {
            narration.push_str(" Single player.");
        }
        while remaining.len() > 1 && round < MAX_TIEBREAKS {
            round += 1;
            narration.push_str(&format!(" round {round} ({} players),", remaining.len()));

            let rolls: Vec<(i32, String)> = remaining
                .iter()
                .map(|nick| {
                    let score = state.users.get(nick).map_or(10, |u| ability.score(u));
                    // The modifier is capped at +5 so the internal stat inflation
                    // does not make high levels a foregone conclusion.
                    (rng.die(20) + ability_modifier(score).min(5), nick.clone())
                })
                .collect();
            let best = rolls.iter().map(|(roll, _)| *roll).max().unwrap_or(0);
            remaining =
                rolls.into_iter().filter(|(r, _)| *r == best).map(|(_, n)| n).collect();
        }

        let Some(winner) = remaining.first().cloned() else { continue };
        let Some(user) = state.users.get(&winner) else { continue };
        let (level, xp) = (user.level, user.xp);
        let spoils = deflate(xp);
        let coins = (spoils / 100 * 100).max(5);

        let narration = narration.trim_end_matches(',').to_string();
        out.broadcast(
            Line::event()
                .text(format!(" {narration}. Winner is "))
                .nick(&winner)
                .text(format!(
                    " (level {level}). {winner} gets {spoils} XP and {coins} coins."
                )),
        );

        if let Some(user) = state.users.get_mut(&winner) {
            user.money += coins;
            user.give_xp(spoils, rng, out);
        }
        if index == 3 {
            out.broadcast(
                Line::event()
                    .text(" ")
                    .nick(&winner)
                    .text(format!(" is the {}!", ability.top_tier_title())),
            );
        }
    }
}

// ---------------------------------------------------------------------------
// event 24: a cut purse
// ---------------------------------------------------------------------------

fn steal_gold(state: &mut GameState, now: i64, rng: &mut GameRng, out: &mut Out) {
    let pacing = state.pacing.clone();
    let Some(victims) =
        players_for_event(state, 1, EventClass::Victim, pacing.victim_cooldown_secs, now, rng)
    else {
        return;
    };
    let victim = victims[0].clone();
    let purse = state.users.get(&victim).map_or(0, |u| u.money);
    if purse < 10 {
        return;
    }

    // The reference lets the thief and the victim be the same player; picking
    // your own pocket makes for a nonsense announcement, so exclude them.
    let Some(thief) = sample_players(
        state,
        1,
        |u| {
            u.nick != victim
                && u.event_ready(EventClass::Chance, now, pacing.chance_cooldown_secs)
        },
        rng,
    )
    .map(|picked| picked[0].clone()) else {
        return;
    };
    if let Some(user) = state.users.get_mut(&thief) {
        user.mark_event(EventClass::Chance, now);
    }

    let stake = deflate(purse);
    // A dexterity check scales the haul: a 1 takes almost nothing, a 20 nearly
    // all of it.
    //
    // The reference reads `u.dexterity` here, but `u` is never bound on this
    // branch, so event 24 raises UnboundLocalError every time it is drawn and
    // has never actually run. The thief is plainly who was meant.
    let dexterity = state.users.get(&thief).map_or(10, |u| u.dexterity);
    let check = rng.die(20) + ability_modifier(dexterity).min(5);
    let stolen = i64::from(check + 4) * stake / 29;

    if stolen > 0 {
        if let Some(user) = state.users.get_mut(&victim) {
            user.money -= stolen;
        }
        if let Some(user) = state.users.get_mut(&thief) {
            user.money += stolen;
        }
        out.broadcast(
            Line::event()
                .text(" ")
                .nick(&thief)
                .text(" has stolen ")
                .text(stolen)
                .text(" coins from ")
                .nick(&victim)
                .text("!"),
        );
    } else {
        // Same reasoning as the paralysis penalty: a third of maximum rest.
        let penalty = pacing.idle_max_secs / 3;
        if let Some(user) = state.users.get_mut(&thief) {
            user.idle_until = (user.idle_until + penalty).max(now + penalty);
        }
        out.broadcast(
            Line::event()
                .text(" ")
                .nick(&thief)
                .text(" tried stealing money from ")
                .nick(&victim)
                .text(" but failed! ")
                .text(&thief)
                .text(" will have to wait ")
                .text(time_display(penalty))
                .text(" more before next turn!"),
        );
    }
}

// ---------------------------------------------------------------------------
// event 25: two warbands throw in together
// ---------------------------------------------------------------------------

fn merge_quests(state: &mut GameState, now: i64, rng: &mut GameRng, out: &mut Out) {
    if state.bboard.len() <= 9 {
        return;
    }
    let posted: Vec<crate::quest::QuestId> =
        state.bboard.posted().into_iter().map(|(id, _)| id).collect();
    let slot = rng.choice_index(posted.len() - 1);
    // Neighbours by challenge rating, so the merge stays plausible; either can
    // be the one that moves.
    let (from, into) = if rng.below(2) == 1 {
        (posted[slot], posted[slot + 1])
    } else {
        (posted[slot + 1], posted[slot])
    };

    if !state.bboard.merge(from, into, now) {
        return;
    }
    out.broadcast(
        Line::bboard()
            .text(" Monsters from quest ")
            .bold(from)
            .text(" have decided to move into quest ")
            .bold(into)
            .text("; both quests have been merged. Quest ")
            .bold(from)
            .text(" is removed from the Bulletin Board."),
    );
    if let Some(line) = state.bboard.describe(into) {
        out.broadcast(line);
    }
}

/// Total XP a warband is worth, used by tests and the party payout.
pub fn warband_xp(quest: &Quest) -> i64 {
    quest.monsters().iter().copied().map(monster_xp).sum()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::assets;
    use crate::state::GameState;

    fn realm(players: usize) -> (GameState, GameRng) {
        let (mut state, _) = GameState::new(1_000_000, 99, Pacing::WEB);
        let rng = GameRng::seed_from_u64(1234);
        let mut out = Out::new();
        for i in 0..players {
            let nick = format!("player{i:02}");
            state.admit(&nick, 1_000_000, &mut out);
            let user = state.users.get_mut(&nick).unwrap();
            user.money = 5_000;
            user.xp = 500 * (i as i64 + 1);
            user.level = user.level_for_xp();
        }
        (state, rng.clone())
    }

    fn run_event(state: &mut GameState, which: i64, rng: &mut GameRng) -> Out {
        let mut out = Out::new();
        run(state, Some(which), 2_000_000, rng, &mut out);
        out
    }

    #[test]
    fn every_event_runs_on_an_empty_realm_without_panicking() {
        for which in 0..EVENT_COUNT {
            let (mut state, mut rng) = realm(0);
            let _ = run_event(&mut state, which, &mut rng);
            assert!(state.users.is_empty());
        }
    }

    #[test]
    fn every_event_runs_on_a_busy_realm_without_panicking() {
        for which in 0..EVENT_COUNT {
            let (mut state, mut rng) = realm(14);
            let out = run_event(&mut state, which, &mut rng);
            // Money and XP must never go negative, whatever happened.
            for user in state.users.values() {
                assert!(user.money >= 0, "event {which} left {} in debt", user.nick);
                assert!(user.xp >= 0, "event {which} left {} with negative XP", user.nick);
            }
            assert!(out.feed.iter().all(|l| !l.plain().is_empty()));
        }
    }

    #[test]
    fn a_random_event_is_always_drawn_from_the_full_range() {
        let (mut state, mut rng) = realm(6);
        let mut out = Out::new();
        for _ in 0..2_000 {
            run(&mut state, None, 2_000_000, &mut rng, &mut out);
        }
        assert!(!out.feed.is_empty(), "2000 events produced no news at all");
    }

    #[test]
    fn tiers_split_on_the_documented_level_brackets() {
        let (mut state, _) = realm(0);
        let mut out = Out::new();
        for level in 1..=20 {
            let nick = format!("lvl{level:02}");
            state.admit(&nick, 0, &mut out);
            state.users.get_mut(&nick).unwrap().level = level;
        }
        let tiers = tiers(&state);
        assert_eq!(tiers[0].len(), 4, "levels 1-4");
        assert_eq!(tiers[1].len(), 6, "levels 5-10");
        assert_eq!(tiers[2].len(), 6, "levels 11-16");
        assert_eq!(tiers[3].len(), 4, "levels 17-20");
    }

    #[test]
    fn duels_need_a_quorum_within_one_tier() {
        // Two players in different tiers cannot be matched against each other.
        let (mut state, mut rng) = realm(2);
        state.users.get_mut("player00").unwrap().level = 1;
        state.users.get_mut("player01").unwrap().level = 20;
        let out = run_event(&mut state, 4, &mut rng);
        assert!(out.feed.is_empty(), "cross-tier duel should not happen");

        // Same tier, and it goes ahead.
        state.users.get_mut("player01").unwrap().level = 1;
        let out = run_event(&mut state, 4, &mut rng);
        let said = out.transcript().join(" ");
        assert!(said.contains("Winner of the current battle is"), "{said}");
    }

    #[test]
    fn a_duel_names_its_combatants_without_a_trailing_separator() {
        let (mut state, mut rng) = realm(4);
        for i in 0..4 {
            state.users.get_mut(&format!("player{i:02}")).unwrap().level = 1;
        }
        let out = run_event(&mut state, 5, &mut rng);
        let headline = out.transcript()[0].clone();
        assert!(headline.ends_with('.'), "{headline}");
        assert!(!headline.contains(", ."), "{headline}");
        assert!(headline.contains("(level 1)"), "{headline}");
    }

    #[test]
    fn finding_coins_enriches_exactly_one_player() {
        let (mut state, mut rng) = realm(5);
        let before: i64 = state.users.values().map(|u| u.money).sum();
        let out = run_event(&mut state, 9, &mut rng);
        let after: i64 = state.users.values().map(|u| u.money).sum();

        assert!(after > before, "someone should be richer");
        assert!(out.transcript()[0].contains("rummaging in the bush"));
        let enriched = state.users.values().filter(|u| u.money != 5_000).count();
        assert_eq!(enriched, 1);
    }

    #[test]
    fn a_per_player_cooldown_stops_the_same_event_twice_over() {
        let (mut state, mut rng) = realm(1);
        let mut out = Out::new();
        run(&mut state, Some(9), 2_000_000, &mut rng, &mut out);
        assert!(!out.feed.is_empty(), "the first draw should land");

        let mut out = Out::new();
        run(&mut state, Some(9), 2_000_001, &mut rng, &mut out);
        assert!(out.feed.is_empty(), "the cooldown should block an immediate repeat");

        let mut out = Out::new();
        let later = 2_000_000 + state.pacing.chance_cooldown_secs;
        run(&mut state, Some(9), later, &mut rng, &mut out);
        assert!(!out.feed.is_empty(), "the cooldown should expire");
    }

    #[test]
    fn the_dragon_is_only_sighted_once_per_cooldown() {
        let (mut state, mut rng) = realm(3);
        let mut out = Out::new();
        run(&mut state, Some(10), 2_000_000, &mut rng, &mut out);
        assert!(out.transcript()[0].contains("The Pink Dragon was seen"));
        assert_eq!(state.events.dragon_seen_at, Some(2_000_000));

        let mut out = Out::new();
        run(&mut state, Some(10), 2_000_100, &mut rng, &mut out);
        assert!(out.feed.is_empty());
    }

    #[test]
    fn the_scoreboard_lists_at_most_eight_in_descending_order() {
        let (mut state, mut rng) = realm(12);
        let out = run_event(&mut state, 11, &mut rng);
        assert_eq!(out.feed.len(), 8, "top eight only");

        let ranks: Vec<usize> = (1..=8).collect();
        for (line, rank) in out.transcript().iter().zip(ranks) {
            assert!(line.contains(&format!("(rank {rank})")), "{line}");
        }
        // The highest XP player leads.
        assert!(out.transcript()[0].contains("player11"));
    }

    #[test]
    fn a_blessed_experience_potion_grants_and_a_cursed_one_takes() {
        let mut granted = false;
        let mut drained = false;
        for seed in 0..40 {
            let (mut state, _) = realm(1);
            let mut rng = GameRng::seed_from_u64(seed);
            let before = state.users["player00"].xp;
            let out = run_event(&mut state, 13, &mut rng);
            let after = state.users["player00"].xp;
            let said = out.transcript().join(" ");
            if said.contains("blessed potion of gain experience") {
                assert!(after > before);
                granted = true;
            }
            if said.contains("cursed potion of gain experience") {
                assert!(after < before);
                drained = true;
            }
        }
        assert!(granted && drained, "both outcomes should occur");
    }

    #[test]
    fn a_cursed_experience_potion_cannot_push_xp_below_zero() {
        for seed in 0..60 {
            let (mut state, _) = realm(1);
            let mut rng = GameRng::seed_from_u64(seed);
            state.users.get_mut("player00").unwrap().xp = 0;
            run_event(&mut state, 13, &mut rng);
            assert!(state.users["player00"].xp >= 0);
        }
    }

    #[test]
    fn a_paralysis_potion_only_ever_adds_rest() {
        for seed in 0..60 {
            let (mut state, _) = realm(1);
            let mut rng = GameRng::seed_from_u64(seed);
            let now = 2_000_000;
            state.users.get_mut("player00").unwrap().idle_until = now + 100;
            run(&mut state, Some(14), now, &mut rng, &mut Out::new());
            let idle = state.users["player00"].idle_until;
            assert!(idle == 0 || idle >= now, "idle went backwards to {idle}");
        }
    }

    #[test]
    fn the_paralysis_penalty_reproduces_the_reference_under_irc_pacing() {
        // The reference hard-codes 60 * (5 + level) seconds.
        for level in 1..=20 {
            assert_eq!(
                paralysis_penalty(&Pacing::IRC, level),
                60 * i64::from(5 + level)
            );
        }
        // And scales down with the web retune.
        assert!(paralysis_penalty(&Pacing::WEB, 20) < paralysis_penalty(&Pacing::IRC, 20));
    }

    #[test]
    fn an_ability_potion_raises_exactly_one_stat() {
        for seed in 0..40 {
            let (mut state, _) = realm(1);
            let mut rng = GameRng::seed_from_u64(seed);
            let before = {
                let u = &state.users["player00"];
                (u.strength, u.dexterity, u.hp)
            };
            run_event(&mut state, 15, &mut rng);
            let after = {
                let u = &state.users["player00"];
                (u.strength, u.dexterity, u.hp)
            };
            let changed = u8::from(after.0 != before.0)
                + u8::from(after.1 != before.1)
                + u8::from(after.2 != before.2);
            assert_eq!(changed, 1, "{before:?} -> {after:?}");
        }
    }

    #[test]
    fn the_path_of_glory_either_pays_out_or_restores_the_rest_timer() {
        let (mut state, mut rng) = realm(1);
        let now = 2_000_000;
        state.users.get_mut("player00").unwrap().idle_until = now + 250;
        let money_before = state.users["player00"].money;

        let mut out = Out::new();
        run(&mut state, Some(16), now, &mut rng, &mut out);
        let said = out.transcript().join(" ");
        assert!(said.contains("Path of the Glory"), "{said}");

        let user = &state.users["player00"];
        if said.contains("has completed all") {
            assert_eq!(user.money, money_before + 15_000 + (user.money - money_before - 15_000));
            assert_eq!(user.idle_until, 0);
        } else {
            assert!(said.contains("was defeated during stage"), "{said}");
            assert_eq!(user.idle_until, now + 250, "a failed Path costs no extra rest");
        }
    }

    #[test]
    fn the_path_of_glory_runs_twelve_stages() {
        let (mut state, mut rng) = realm(1);
        // An unkillable hero so the gauntlet runs to the end.
        {
            let u = state.users.get_mut("player00").unwrap();
            u.level = 20;
            u.hp = 1_000_000;
            u.strength = 400;
            u.money = 100_000_000;
            u.buy(assets::items().iter().find(|i| i.name == "Greatsword").unwrap().id);
        }
        let out = run_event(&mut state, 16, &mut rng);
        let stages =
            out.transcript().iter().filter(|l| l.contains("Path of the Glory. Stage")).count();
        assert_eq!(stages, 12, "twenty quests less the eight easiest");
        assert!(out.transcript().iter().any(|l| l.contains("has completed all 12 quests")));
    }

    #[test]
    fn party_quests_need_a_full_party() {
        let (mut state, mut rng) = realm(2);
        let out = run_event(&mut state, 17, &mut rng);
        assert!(out.feed.is_empty(), "an epic quest needs three adventurers");

        let (mut state, mut rng) = realm(3);
        let out = run_event(&mut state, 17, &mut rng);
        assert!(out.transcript()[0].contains("Epic Quest:"));
    }

    #[test]
    fn creepy_quests_draft_four_adventurers() {
        let (mut state, mut rng) = realm(3);
        let out = run_event(&mut state, 20, &mut rng);
        assert!(out.feed.is_empty(), "a creepy quest needs four adventurers");

        let (mut state, mut rng) = realm(4);
        let out = run_event(&mut state, 20, &mut rng);
        assert!(out.transcript()[0].contains("Creepy Quest:"));
    }

    #[test]
    fn a_party_quest_resolves_one_way_or_the_other() {
        for seed in 0..25 {
            let (mut state, _) = realm(6);
            let mut rng = GameRng::seed_from_u64(seed);
            let out = run_event(&mut state, 17, &mut rng);
            let said = out.transcript().join(" ");
            assert!(
                said.contains("defeated all monsters!") || said.contains("Adventurers are defeated!"),
                "{said}"
            );
        }
    }

    #[test]
    fn ability_checks_crown_one_winner_per_populated_tier() {
        let (mut state, mut rng) = realm(0);
        let mut out = Out::new();
        // Two tiers populated, two empty.
        for (nick, level) in [("a", 1), ("b", 2), ("c", 18), ("d", 19)] {
            state.admit(nick, 0, &mut out);
            state.users.get_mut(nick).unwrap().level = level;
        }
        let out = run_event(&mut state, 22, &mut rng);
        let winners =
            out.transcript().iter().filter(|l| l.contains("Winner is")).count();
        assert_eq!(winners, 2);
        assert!(out.transcript().iter().any(|l| l.contains("first tier (levels 1-4)")));
        assert!(out.transcript().iter().any(|l| l.contains("fourth tier (levels 17-20)")));
        assert!(out.transcript().iter().any(|l| l.contains("is the Dude!")));
    }

    #[test]
    fn a_strength_check_crowns_the_boss() {
        let (mut state, mut rng) = realm(0);
        let mut out = Out::new();
        state.admit("solo", 0, &mut out);
        state.users.get_mut("solo").unwrap().level = 20;
        let out = run_event(&mut state, 23, &mut rng);
        let said = out.transcript().join(" ");
        assert!(said.contains("Strength check"), "{said}");
        assert!(said.contains("Single player."), "{said}");
        assert!(said.contains("is the Boss!"), "{said}");
        assert!(!said.contains(",."), "trailing separator not trimmed: {said}");
    }

    #[test]
    fn an_ability_check_always_terminates() {
        // Identical players tie constantly; the cap has to break it.
        let (mut state, mut rng) = realm(0);
        let mut out = Out::new();
        for i in 0..12 {
            let nick = format!("clone{i:02}");
            state.admit(&nick, 0, &mut out);
        }
        let out = run_event(&mut state, 22, &mut rng);
        assert!(out.transcript().iter().any(|l| l.contains("Winner is")));
    }

    #[test]
    fn stealing_moves_money_between_two_different_players() {
        let mut stole = false;
        for seed in 0..80 {
            let (mut state, _) = realm(6);
            let mut rng = GameRng::seed_from_u64(seed);
            let total_before: i64 = state.users.values().map(|u| u.money).sum();
            let out = run_event(&mut state, 24, &mut rng);
            let total_after: i64 = state.users.values().map(|u| u.money).sum();

            if out.transcript().join(" ").contains("has stolen") {
                stole = true;
                assert_eq!(total_before, total_after, "theft must conserve coin");
                // Two distinct players are named.
                let actors = &out.feed[0].actors;
                assert_eq!(actors.len(), 2, "{actors:?}");
                assert_ne!(actors[0], actors[1]);
            }
            assert!(state.users.values().all(|u| u.money >= 0));
        }
        assert!(stole, "a successful theft never occurred");
    }

    #[test]
    fn a_bungled_theft_costs_the_thief_time_and_takes_nothing() {
        // The haul is `(check + 4) * stake / 29`, so it only rounds down to
        // nothing when the victim is barely worth robbing in the first place.
        let mut failed = false;
        for seed in 0..120 {
            let (mut state, _) = realm(6);
            let mut rng = GameRng::seed_from_u64(seed);
            for user in state.users.values_mut() {
                user.money = 10;
            }
            let now = 2_000_000;
            let mut out = Out::new();
            run(&mut state, Some(24), now, &mut rng, &mut out);

            if out.transcript().join(" ").contains("tried stealing") {
                failed = true;
                assert!(
                    state.users.values().all(|u| u.money == 10),
                    "a failed theft must move no coin"
                );
                let thief = &out.feed[0].actors[0];
                assert!(
                    state.users[thief].is_resting(now),
                    "the thief should be delayed"
                );
            }
        }
        assert!(failed, "a failed theft never occurred");
    }

    #[test]
    fn stealing_needs_a_victim_worth_robbing() {
        let (mut state, mut rng) = realm(4);
        for user in state.users.values_mut() {
            user.money = 3; // under the threshold of 10
        }
        let out = run_event(&mut state, 24, &mut rng);
        assert!(out.feed.is_empty());
    }

    #[test]
    fn stealing_does_nothing_with_only_one_player() {
        let (mut state, mut rng) = realm(1);
        state.users.get_mut("player00").unwrap().money = 100_000;
        let out = run_event(&mut state, 24, &mut rng);
        assert!(out.feed.is_empty(), "nobody to rob but yourself");
        assert_eq!(state.users["player00"].money, 100_000);
    }

    #[test]
    fn merging_quests_needs_a_crowded_board() {
        let (mut state, mut rng) = realm(2);
        while state.bboard.len() > 9 {
            let id = state.bboard.posted()[0].0;
            state.bboard.take(id);
        }
        let out = run_event(&mut state, 25, &mut rng);
        assert!(out.feed.is_empty(), "a thin board should not merge");
    }

    #[test]
    fn merging_combines_two_quests_into_one() {
        let (mut state, mut rng) = realm(2);
        let before = state.bboard.len();
        let cr_before = state.bboard.total_cr();
        assert!(before > 9);

        let out = run_event(&mut state, 25, &mut rng);
        assert_eq!(state.bboard.len(), before - 1);
        assert_eq!(state.bboard.total_cr(), cr_before, "no challenge is lost");
        let said = out.transcript().join(" ");
        assert!(said.contains("both quests have been merged"), "{said}");
    }

    #[test]
    fn the_board_settles_under_the_real_event_mix() {
        // Events 1 and 2 only ever post quests, while 3 and 25 only retire
        // them; it is the uniform draw across all twenty-six that holds the
        // board steady. Forcing a diet of the posting events alone would grow
        // it without bound in the reference too, so this exercises the mix
        // players actually see.
        let (mut state, mut rng) = realm(4);
        let mut out = Out::new();
        let mut high = 0;
        for step in 0..6_000 {
            run(&mut state, None, 2_000_000 + step, &mut rng, &mut out);
            high = high.max(state.bboard.len());
            assert!(
                (1..=40).contains(&state.bboard.len()),
                "board reached {} quests at step {step}", state.bboard.len()
            );
        }
        assert!(high >= 10, "the board never filled up at all (peaked at {high})");
    }

    #[test]
    fn a_crowded_board_stops_posting_and_starts_retiring() {
        // The self-correcting pressure: replenishing a full board adds at most
        // one, while both retirement events stay active above nine quests.
        let (mut state, mut rng) = realm(2);
        for _ in 0..20 {
            state.bboard.add_quest(Some(0), 0, &mut rng);
        }
        let crowded = state.bboard.len();
        let mut out = Out::new();

        run(&mut state, Some(2), 2_000_000, &mut rng, &mut out);
        assert!(state.bboard.len() <= crowded + 1, "a full board barely grows");

        let before = state.bboard.len();
        run(&mut state, Some(3), 2_000_000, &mut rng, &mut out);
        assert_eq!(state.bboard.len(), before - 1, "retirement still fires");
    }

    #[test]
    fn the_deflation_formula_matches_the_reference() {
        // x // x.bit_length(), with 5 as the floor for anyone with nothing.
        assert_eq!(deflate(0), 5);
        assert_eq!(deflate(-100), 5);
        assert_eq!(deflate(1), 1);
        assert_eq!(deflate(1_000), 1_000 / 10);
        assert_eq!(deflate(100_000), 100_000 / 17);
    }

    #[test]
    fn warband_xp_sums_its_monsters() {
        let goblin =
            assets::monsters().iter().position(|m| m.name == "Goblin").unwrap() as u16;
        let quest = Quest::from_roster(vec![goblin; 4], 0);
        assert_eq!(warband_xp(&quest), 4 * monster_xp(goblin));
    }

    #[test]
    fn event_state_survives_a_serde_round_trip() {
        let state = EventState { dragon_seen_at: Some(5), scoreboard_at: None };
        let json = serde_json::to_string(&state).unwrap();
        assert_eq!(serde_json::from_str::<EventState>(&json).unwrap(), state);
        assert_eq!(
            serde_json::from_str::<EventState>("{}").unwrap(),
            EventState::default()
        );
    }
}
