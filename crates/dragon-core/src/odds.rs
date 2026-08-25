//! How a posted quest is likely to go for a given adventurer.
//!
//! The Bulletin Board can say what a quest pays without asking anyone; what it
//! is likely to cost depends on who is reading. There is no closed form for
//! that: combat is initiative order, target selection, criticals, and a warband
//! whose incoming damage falls away as its members drop. So the answer is
//! *sampled* — the quest is fought a number of times and the wins counted —
//! rather than modelled a second way. Any expression of the odds that was not
//! the battle resolver itself would drift from it the first time combat was
//! touched, and would drift silently, because nothing checks an estimate.
//!
//! This is not part of a turn. It reads a [`User`] and a [`Quest`], rolls its
//! own dice, and leaves both alone.

use crate::battle::battle;
use crate::quest::Quest;
use crate::rng::GameRng;
use crate::user::{PLAYER, User};

/// Fights sampled per estimate.
///
/// At even odds the standard error of 96 draws is about five points, which is
/// finer than the bands the figure is read in and cheap enough to run for a
/// whole board on every character-sheet read.
pub const TRIALS: u32 = 96;

/// The chance `user` walks away from `quest`, in `0.0..=1.0`.
pub fn win_chance(user: &User, quest: &Quest) -> f64 {
    sampled_win_chance(user, quest, TRIALS)
}

/// [`win_chance`] with the sample size exposed, for tests and tuning.
pub fn sampled_win_chance(user: &User, quest: &Quest, trials: u32) -> f64 {
    if trials == 0 {
        return 0.0;
    }
    // Seeded from the quest, so a posting is judged on the same draws every
    // time it is asked about and the figure does not twitch between polls.
    // Its own stream, too: sampling must never advance `GameState::rng_seed`,
    // which an in-flight game depends on reproducing exactly.
    let mut rng = GameRng::seed_from_u64(quest_seed(quest));
    let mut wins = 0;
    for _ in 0..trials {
        let mut roster = user.muster(quest, &mut rng);
        if battle(&mut roster, &mut rng).won_by(PLAYER) {
            wins += 1;
        }
    }
    f64::from(wins) / f64::from(trials)
}

/// FNV-1a over the warband and its posting time. `created_at` is in the mix so
/// that two quests which happen to draw the same monsters are still sampled
/// independently.
fn quest_seed(quest: &Quest) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for &id in quest.monsters() {
        for byte in id.to_le_bytes() {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }
    for byte in quest.created_at.to_le_bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::assets::{self, MonsterId};
    use crate::numeric::Cr;

    fn monster_named(name: &str) -> MonsterId {
        assets::monsters().iter().position(|m| m.name == name).unwrap() as MonsterId
    }

    /// A level-20 character in the best gear the store sells.
    fn veteran() -> User {
        let mut user = User::new("Veteran", 0);
        user.level = 20;
        user.hp = 200;
        user.strength = 20;
        user.dexterity = 20;
        let dearest = |kind| {
            assets::items()
                .iter()
                .filter(|item| item.kind() == kind)
                .max_by_key(|item| item.cost)
                .unwrap()
                .id
        };
        user.buy(dearest(assets::ItemKind::Weapon));
        user.buy(dearest(assets::ItemKind::Armor));
        user.proficiency_bonus = 4;
        user
    }

    #[test]
    fn a_novice_is_hopeless_against_a_tarrasque_and_safe_against_a_rat() {
        let novice = User::new("Novice", 0);
        let doom = Quest::from_roster(vec![monster_named("Tarrasque")], 0);
        let vermin = Quest::from_roster(vec![monster_named("Rat")], 0);

        assert!(win_chance(&novice, &doom) < 0.05, "{}", win_chance(&novice, &doom));
        assert!(win_chance(&novice, &vermin) > 0.9, "{}", win_chance(&novice, &vermin));
    }

    #[test]
    fn the_same_quest_looks_better_to_a_stronger_character() {
        let quest = Quest::from_roster(vec![monster_named("Ogre"), monster_named("Ogre")], 0);
        let novice = win_chance(&User::new("Novice", 0), &quest);
        let veteran = win_chance(&veteran(), &quest);
        assert!(veteran > novice, "veteran {veteran} did not beat novice {novice}");
    }

    #[test]
    fn a_harder_board_entry_reads_worse_than_an_easier_one() {
        let mut rng = GameRng::seed_from_u64(1);
        let user = veteran();
        let easy = Quest::random(Some(0), 0, &mut rng);
        let hard = Quest::random(Some(9), 0, &mut rng);
        assert!(easy.total_cr() < hard.total_cr(), "the fixture is backwards");
        assert!(win_chance(&user, &easy) >= win_chance(&user, &hard));
    }

    #[test]
    fn an_estimate_is_stable_across_calls() {
        // The client polls; a figure that moved every few seconds without the
        // player or the quest changing would read as noise, not information.
        let mut rng = GameRng::seed_from_u64(2);
        let user = veteran();
        let quest = Quest::random(None, 7_000, &mut rng);
        let first = win_chance(&user, &quest);
        for _ in 0..5 {
            assert_eq!(win_chance(&user, &quest), first);
        }
    }

    #[test]
    fn estimating_never_touches_the_callers_random_stream() {
        let mut rng = GameRng::seed_from_u64(3);
        let quest = Quest::random(None, 0, &mut rng);
        let user = veteran();

        let mut stream = GameRng::seed_from_u64(99);
        let before: Vec<u64> = (0..8).map(|_| stream.next_u64()).collect();
        win_chance(&user, &quest);
        let mut same = GameRng::seed_from_u64(99);
        let after: Vec<u64> = (0..8).map(|_| same.next_u64()).collect();
        assert_eq!(before, after);
    }

    #[test]
    fn two_quests_with_identical_warbands_are_sampled_separately() {
        let roster = vec![monster_named("Ogre"), monster_named("Goblin")];
        let a = Quest::from_roster(roster.clone(), 100);
        let b = Quest::from_roster(roster, 200);
        assert_ne!(quest_seed(&a), quest_seed(&b));
    }

    #[test]
    fn asking_for_no_samples_answers_no_chance_rather_than_dividing_by_zero() {
        let mut rng = GameRng::seed_from_u64(4);
        let quest = Quest::random(None, 0, &mut rng);
        assert_eq!(sampled_win_chance(&veteran(), &quest, 0), 0.0);
    }

    #[test]
    fn every_estimate_is_a_probability() {
        let mut rng = GameRng::seed_from_u64(5);
        let user = veteran();
        for _ in 0..40 {
            let quest = Quest::random(None, 0, &mut rng);
            let chance = win_chance(&user, &quest);
            assert!((0.0..=1.0).contains(&chance), "{chance} for {}", quest.display());
        }
    }

    #[test]
    fn gear_is_worth_more_to_the_estimate_than_the_character_sheet_shows() {
        // `build_warrior` secretly inflates a questing character by their armor
        // and weapon grade, so the odds have to be read off the fight rather
        // than off the stat line: an ascension-grade warband is a coin toss for
        // a kitted veteran and certain death for the same character unarmed.
        let mut rng = GameRng::seed_from_u64(6);
        let quest = Quest::assembled(32, Cr::from_whole(65), true, 0, &mut rng);

        let kitted = veteran();
        let mut bare = veteran();
        bare.weapon = None;
        bare.armor = None;
        bare.proficiency_bonus = 0;

        assert!(win_chance(&bare, &quest) < win_chance(&kitted, &quest));
    }

    #[test]
    fn a_whole_board_can_be_judged_inside_a_read() {
        // The estimate runs once per posted quest on every `/api/me`. If a full
        // board ever stopped being cheap, the endpoint would be the place it
        // showed up, so the budget is asserted here instead.
        let mut rng = GameRng::seed_from_u64(7);
        let board = crate::bboard::BBoard::new(0, &mut rng);
        let user = veteran();

        let started = std::time::Instant::now();
        let judged: Vec<f64> =
            board.posted().iter().map(|(_, quest)| win_chance(&user, quest)).collect();
        let elapsed = started.elapsed();

        assert_eq!(judged.len(), board.len());
        assert!(elapsed.as_millis() < 500, "judging a board took {elapsed:?}");
    }
}
