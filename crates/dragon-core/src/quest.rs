//! Quests: a warband of monsters posted on the Bulletin Board.
//!
//! A quest stores nothing but monster indices and when it was posted; its
//! headline and total challenge rating are derived, so the serialised state
//! stays tiny and the two can never drift apart.

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::assets::{self, MonsterId, Pool};
use crate::numeric::Cr;
use crate::rng::GameRng;

/// The four hex digits players type: `quest 0A3F`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct QuestId(pub u16);

impl QuestId {
    /// Case-insensitive, matching `bboard.accept_quest`'s `myid.upper()`.
    pub fn parse(text: &str) -> Option<Self> {
        let text = text.trim();
        if text.is_empty() || text.len() > 4 {
            return None;
        }
        u16::from_str_radix(text, 16).ok().map(QuestId)
    }
}

impl fmt::Display for QuestId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:04X}", self.0)
    }
}

/// A deterministic stand-in for Python's `hash(name)`.
///
/// The reference sorts a quest's monsters by `(challenge_rating, hash(name))`
/// specifically so equal-CR monsters are not listed alphabetically. CPython's
/// hash is salted per process, so that order was never reproducible anyway;
/// FNV-1a keeps the intent and makes it stable.
fn name_order(name: &str) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in name.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// One entry in a quest's headline: `3x Goblin (CR 1/4)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MonsterGroup {
    pub monster: MonsterId,
    pub count: usize,
}

impl MonsterGroup {
    pub fn name(&self) -> &'static str {
        &assets::monster(self.monster).name
    }

    pub fn cr(&self) -> Cr {
        assets::monster(self.monster).cr
    }
}

impl fmt::Display for MonsterGroup {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.count > 1 {
            write!(f, "{}x ", self.count)?;
        }
        write!(f, "{} (CR {})", self.name(), self.cr().label())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Quest {
    /// Kept in display order — sorted by challenge rating, then by name hash —
    /// so duplicates are always adjacent.
    monsters: Vec<MonsterId>,
    /// Unix seconds. The reference stores a tick number; wall-clock survives a
    /// change of tick length.
    pub created_at: i64,
}

impl Quest {
    /// Roll a quest. `force` overrides the difficulty draw; values of 10 or
    /// more select the difficult pool but size the warband as if the low digit
    /// had been rolled, which is how the reference seeds an easy opening board.
    pub fn random(force: Option<i64>, created_at: i64, rng: &mut GameRng) -> Self {
        let raw_difficulty = force.unwrap_or_else(|| rng.range(0, 10));
        let pool = Pool::for_difficulty(raw_difficulty);
        let difficulty = raw_difficulty.rem_euclid(10);

        let mut available: Vec<MonsterId> = pool.members().to_vec();
        let mut chosen: Vec<MonsterId> = Vec::new();

        while !available.is_empty() && chosen.len() < 3 {
            let (picked, compatible) = pickup_random(&available, rng);
            chosen.push(picked);
            available = compatible;
            // Most warbands stop at one or two kinds of monster.
            if chosen.len() == 1 && rng.unit() < 0.45 {
                break;
            }
            if chosen.len() == 2 && rng.unit() < 0.75 {
                break;
            }
        }

        Self::from_roster(pad_roster(chosen, difficulty, rng), created_at)
    }

    /// Build a set-piece encounter by rolling `count` quests, ordering them and
    /// merging from the top until their combined CR clears `threshold`.
    ///
    /// `epic` takes the hardest first, so the warband is a few enormous
    /// monsters; otherwise the weakest come first and it becomes a swarm.
    pub fn assembled(
        count: usize,
        threshold: Cr,
        epic: bool,
        created_at: i64,
        rng: &mut GameRng,
    ) -> Self {
        let mut rolled: Vec<Quest> =
            (0..count).map(|_| Quest::random(None, created_at, rng)).collect();
        rolled.sort_by_key(|q| if epic { -q.total_cr().eighths() } else { q.total_cr().eighths() });

        let mut roster = Vec::new();
        let mut total = Cr::ZERO;
        for quest in rolled {
            total += quest.total_cr();
            roster.extend(quest.monsters);
            if total >= threshold {
                break;
            }
        }
        Self::from_roster(roster, created_at)
    }

    pub fn from_roster(mut roster: Vec<MonsterId>, created_at: i64) -> Self {
        roster.sort_by_key(|&id| {
            let m = assets::monster(id);
            (m.cr, name_order(&m.name))
        });
        Self { monsters: roster, created_at }
    }

    pub fn monsters(&self) -> &[MonsterId] {
        &self.monsters
    }

    pub fn is_empty(&self) -> bool {
        self.monsters.is_empty()
    }

    /// Consecutive runs of the same monster, in display order.
    pub fn groups(&self) -> Vec<MonsterGroup> {
        let mut groups: Vec<MonsterGroup> = Vec::new();
        for &id in &self.monsters {
            match groups.last_mut() {
                Some(g) if g.monster == id => g.count += 1,
                _ => groups.push(MonsterGroup { monster: id, count: 1 }),
            }
        }
        groups
    }

    pub fn total_cr(&self) -> Cr {
        self.monsters.iter().map(|&id| assets::monster(id).cr).sum()
    }

    /// `3x Goblin (CR 1/4), Ogre (CR 2)`
    pub fn display(&self) -> String {
        self.groups().iter().map(MonsterGroup::to_string).collect::<Vec<_>>().join(", ")
    }

    /// Absorb another quest's monsters, as the quest-merging event does.
    pub fn absorb(&mut self, other: &Quest, created_at: i64) {
        self.monsters.extend_from_slice(&other.monsters);
        *self = Self::from_roster(std::mem::take(&mut self.monsters), created_at);
    }
}

impl fmt::Display for Quest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.display())
    }
}

/// Pick one monster from `pool`, and return it alongside the monsters that
/// would willingly share a lair with it.
///
/// The candidate list also drops everything sharing the picked monster's name,
/// so a warband never gains a second kind by accident; repeats come from
/// [`pad_roster`] instead.
fn pickup_random(pool: &[MonsterId], rng: &mut GameRng) -> (MonsterId, Vec<MonsterId>) {
    let picked = *rng.choice(pool);
    let chosen = assets::monster(picked);

    // The Cloud Giant is "neutral good (50%) or neutral evil (50%)" and settles
    // the question when it is the one leading the warband. As a mere candidate
    // it is judged on its primary alignment — the reference leaves that case
    // genuinely undefined, reusing whatever `valid` held from the last loop.
    let alignment = match chosen.alignment_alt {
        Some(alt) if rng.below(2) == 1 => alt,
        _ => chosen.alignment,
    };

    let compat = assets::compat();
    let compatible = pool
        .iter()
        .copied()
        .filter(|&id| id != picked)
        .filter(|&id| {
            let other = assets::monster(id);
            other.name != chosen.name
                && compat.alignments_agree(alignment, other.alignment)
                && compat.sizes_agree(chosen.size, other.size)
        })
        .collect();

    (picked, compatible)
}

/// Turn the one to three distinct monsters into an actual warband by repeating
/// them, more freely on easy quests than on hard ones.
fn pad_roster(chosen: Vec<MonsterId>, difficulty: i64, rng: &mut GameRng) -> Vec<MonsterId> {
    match chosen.len() {
        0 => chosen,
        1 => {
            // 5 - isqrt(difficulty): up to four of a weak monster, but only one
            // of something drawn from the difficult pool.
            let limit = 5 - difficulty.isqrt();
            let copies = rng.range(1, limit.max(2)) as usize;
            chosen.repeat(copies)
        }
        2 => {
            let r = rng.unit();
            if difficulty < 7 && r < 0.55 {
                if r < 0.3 {
                    chosen.repeat(2)
                } else {
                    let mut roster = chosen.repeat(2);
                    roster.push(chosen[0]);
                    roster
                }
            } else if difficulty < 9 && r > 0.7 {
                let mut roster = chosen;
                roster.push(roster[0]);
                roster
            } else {
                chosen
            }
        }
        _ => {
            let r = rng.unit();
            let mut roster = chosen;
            if difficulty < 7 && r < 0.35 {
                roster.push(roster[0]);
            } else if difficulty < 5 && r < 0.375 {
                roster.push(roster[0]);
                roster.push(roster[1]);
            }
            roster
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn monster_named(name: &str) -> MonsterId {
        assets::monsters().iter().position(|m| m.name == name).unwrap() as MonsterId
    }

    #[test]
    fn quest_ids_are_four_hex_digits() {
        assert_eq!(QuestId(0).to_string(), "0000");
        assert_eq!(QuestId(0x0A3F).to_string(), "0A3F");
        assert_eq!(QuestId(0xFFFF).to_string(), "FFFF");
    }

    #[test]
    fn quest_ids_parse_case_insensitively() {
        assert_eq!(QuestId::parse("0a3f"), Some(QuestId(0x0A3F)));
        assert_eq!(QuestId::parse("0A3F"), Some(QuestId(0x0A3F)));
        assert_eq!(QuestId::parse(" ff "), Some(QuestId(0xFF)));
        assert_eq!(QuestId::parse("zzzz"), None);
        assert_eq!(QuestId::parse("12345"), None);
        assert_eq!(QuestId::parse(""), None);
    }

    #[test]
    fn quest_ids_round_trip() {
        for raw in [0u16, 1, 0x1234, 0xABCD, 0xFFFF] {
            let id = QuestId(raw);
            assert_eq!(QuestId::parse(&id.to_string()), Some(id));
        }
    }

    #[test]
    fn a_headline_counts_duplicates_and_labels_fractional_cr() {
        let goblin = monster_named("Goblin");
        let quest = Quest::from_roster(vec![goblin, goblin, goblin], 0);
        assert_eq!(quest.display(), "3x Goblin (CR 1/4)");
        assert_eq!(quest.total_cr(), Cr(6), "three monsters at CR 1/4");
        assert_eq!(quest.total_cr().to_string(), "0.75");
    }

    #[test]
    fn a_single_monster_gets_no_count_prefix() {
        let quest = Quest::from_roster(vec![monster_named("Aboleth")], 0);
        assert_eq!(quest.display(), "Aboleth (CR 10)");
        assert_eq!(quest.total_cr(), Cr::from_whole(10));
    }

    #[test]
    fn monsters_are_ordered_by_challenge_rating() {
        let quest = Quest::from_roster(
            vec![monster_named("Aboleth"), monster_named("Goblin"), monster_named("Ogre")],
            0,
        );
        let crs: Vec<Cr> = quest.groups().iter().map(|g| g.cr()).collect();
        assert!(crs.windows(2).all(|w| w[0] <= w[1]), "{crs:?}");
        assert!(quest.display().starts_with("Goblin"));
    }

    #[test]
    fn duplicates_are_grouped_however_they_arrive() {
        let (goblin, ogre) = (monster_named("Goblin"), monster_named("Ogre"));
        let quest = Quest::from_roster(vec![goblin, ogre, goblin, ogre, goblin], 0);
        let groups = quest.groups();
        assert_eq!(groups.len(), 2);
        assert_eq!(groups.iter().map(|g| g.count).sum::<usize>(), 5);
        assert_eq!(quest.display(), "3x Goblin (CR 1/4), 2x Ogre (CR 2)");
    }

    #[test]
    fn equal_cr_monsters_are_not_listed_alphabetically() {
        // The whole point of sorting by a name hash rather than the name.
        let same_cr: Vec<MonsterId> = assets::monsters()
            .iter()
            .enumerate()
            .filter(|(_, m)| m.cr == Cr::from_whole(2))
            .map(|(i, _)| i as MonsterId)
            .take(8)
            .collect();
        let quest = Quest::from_roster(same_cr, 0);
        let names: Vec<&str> = quest.groups().iter().map(|g| g.name()).collect();
        let mut alphabetical = names.clone();
        alphabetical.sort_unstable();
        assert_ne!(names, alphabetical, "ordering collapsed to alphabetical");
    }

    #[test]
    fn name_ordering_is_stable_across_calls() {
        assert_eq!(name_order("Goblin"), name_order("Goblin"));
        assert_ne!(name_order("Goblin"), name_order("Ogre"));
    }

    #[test]
    fn random_quests_are_never_empty_and_stay_small() {
        let mut rng = GameRng::seed_from_u64(1);
        for _ in 0..2_000 {
            let quest = Quest::random(None, 0, &mut rng);
            assert!(!quest.is_empty());
            assert!(quest.monsters().len() <= 6, "{}", quest.display());
            assert!(quest.groups().len() <= 3, "at most three kinds of monster");
        }
    }

    #[test]
    fn forcing_a_difficulty_selects_the_matching_pool() {
        let mut rng = GameRng::seed_from_u64(2);
        for _ in 0..300 {
            let easy = Quest::random(Some(0), 0, &mut rng);
            assert!(
                easy.groups().iter().all(|g| g.cr() <= Cr::ONE),
                "easy quest drew {}", easy.display()
            );
            let hard = Quest::random(Some(9), 0, &mut rng);
            assert!(
                hard.groups().iter().all(|g| g.cr() > Cr::from_whole(13)),
                "difficult quest drew {}", hard.display()
            );
        }
    }

    #[test]
    fn easy_quests_are_much_weaker_than_difficult_ones() {
        let mut rng = GameRng::seed_from_u64(3);
        let mean = |force: i64, rng: &mut GameRng| {
            let total: i32 = (0..400)
                .map(|_| Quest::random(Some(force), 0, rng).total_cr().eighths())
                .sum();
            f64::from(total) / 400.0 / 8.0
        };
        let easy = mean(0, &mut rng);
        let hard = mean(9, &mut rng);
        assert!(easy < 4.0, "easy quests averaged CR {easy}");
        assert!(hard > 30.0, "difficult quests averaged CR {hard}");
    }

    #[test]
    fn forcing_ten_draws_hard_monsters_but_sizes_like_an_easy_warband() {
        // The reference uses force >= 10 to open the board; the low digit
        // drives the head count while the pool stays difficult.
        let mut rng = GameRng::seed_from_u64(4);
        let mut saw_a_crowd = false;
        for _ in 0..400 {
            let quest = Quest::random(Some(10), 0, &mut rng);
            assert!(quest.groups().iter().all(|g| g.cr() > Cr::from_whole(13)));
            if quest.monsters().len() > 2 {
                saw_a_crowd = true;
            }
        }
        assert!(saw_a_crowd, "force=10 should still allow multiple monsters");
    }

    #[test]
    fn a_warband_only_contains_monsters_that_tolerate_each_other() {
        let mut rng = GameRng::seed_from_u64(5);
        let compat = assets::compat();
        for _ in 0..2_000 {
            let quest = Quest::random(None, 0, &mut rng);
            let kinds: Vec<MonsterId> = quest.groups().iter().map(|g| g.monster).collect();
            for (i, &a) in kinds.iter().enumerate() {
                for &b in &kinds[i + 1..] {
                    let (ma, mb) = (assets::monster(a), assets::monster(b));
                    assert!(
                        compat.sizes_agree(ma.size, mb.size),
                        "{} and {} are incompatible sizes", ma.name, mb.name
                    );
                }
            }
        }
    }

    #[test]
    fn assembled_quests_clear_their_threshold() {
        let mut rng = GameRng::seed_from_u64(6);
        let threshold = Cr::from_whole(70);
        for _ in 0..40 {
            let quest = Quest::assembled(32, threshold, true, 0, &mut rng);
            assert!(
                quest.total_cr() >= threshold,
                "epic quest only reached CR {}", quest.total_cr()
            );
        }
    }

    #[test]
    fn epic_quests_are_few_big_monsters_and_creepy_ones_are_swarms() {
        let mut rng = GameRng::seed_from_u64(7);
        let count = |epic: bool, rng: &mut GameRng| {
            let total: usize = (0..30)
                .map(|_| Quest::assembled(32, Cr::from_whole(45), epic, 0, rng).monsters().len())
                .sum();
            total as f64 / 30.0
        };
        let epic = count(true, &mut rng);
        let creepy = count(false, &mut rng);
        assert!(creepy > epic, "creepy quests ({creepy}) should outnumber epic ({epic})");
    }

    #[test]
    fn absorbing_another_quest_merges_and_regroups() {
        let (goblin, ogre) = (monster_named("Goblin"), monster_named("Ogre"));
        let mut a = Quest::from_roster(vec![goblin, ogre], 100);
        let b = Quest::from_roster(vec![goblin, goblin], 200);
        let before = a.total_cr();
        a.absorb(&b, 300);
        assert_eq!(a.total_cr(), before + b.total_cr());
        assert_eq!(a.created_at, 300, "merging refreshes the posting time");
        assert_eq!(a.display(), "3x Goblin (CR 1/4), Ogre (CR 2)");
    }

    #[test]
    fn a_quest_survives_a_serde_round_trip() {
        let mut rng = GameRng::seed_from_u64(8);
        let quest = Quest::random(None, 12_345, &mut rng);
        let json = serde_json::to_string(&quest).unwrap();
        let back: Quest = serde_json::from_str(&json).unwrap();
        assert_eq!(back, quest);
        assert_eq!(back.display(), quest.display());
        assert_eq!(back.total_cr(), quest.total_cr());
    }
}
