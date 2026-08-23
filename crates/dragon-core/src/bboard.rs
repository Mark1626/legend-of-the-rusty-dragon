//! The Bulletin Board: which quests are currently on offer.
//!
//! As with the store, the reference reprinted the whole board on every rotation
//! because IRC gave players no way to ask. A web client reads [`BBoard::posted`]
//! instead, so the feed only carries what changed.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::numeric::Cr;
use crate::out::{Line, Out};
use crate::quest::{Quest, QuestId};
use crate::rng::GameRng;

/// How many quests the board opens with.
const OPENING_QUESTS: usize = 16;

/// The board rotates on the same 32-step cycle as the store.
const CYCLE: u32 = 32;
const DOUBLE_ROTATION_STEP: u32 = 17;

/// Monsters have to leave the board somehow.
const RETIREMENT_NOTICES: [&str; 10] = [
    "Monsters from Quest {} are getting too old for fighting again; they are now retiring.",
    "Monsters from Quest {} feel faint this morning.",
    "Monsters from Quest {} are on vacation for the week.",
    "Monsters from Quest {} are too tired for fighting.",
    "Monsters from Quest {} have left the Realm!",
    "BREAKING NEWS: monsters from Quest {} go on strike!",
    "Monsters from Quest {} have been devoured by monsters from another quest.",
    "Monsters from Quest {} have decided to turn into nice pets for enjoying children of the Realm.",
    "Monsters from Quest {} are getting depressed.",
    "The King has pardoned monsters from Quest {}.",
];

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BBoard {
    quests: BTreeMap<QuestId, Quest>,
    /// Next identifier to hand out; wraps at four hex digits.
    next_id: u16,
    /// Position in the rotation cycle, not a duration.
    tick_cnt: u32,
}

impl BBoard {
    /// Open the board with an easy spread, so a brand-new Realm is playable.
    pub fn new(now: i64, rng: &mut GameRng) -> Self {
        let mut board = BBoard::default();
        for _ in 0..OPENING_QUESTS {
            board.add_quest(Some(rng.range(0, 4)), now, rng);
        }
        board
    }

    pub fn len(&self) -> usize {
        self.quests.len()
    }

    pub fn is_empty(&self) -> bool {
        self.quests.is_empty()
    }

    pub fn get(&self, id: QuestId) -> Option<&Quest> {
        self.quests.get(&id)
    }

    pub fn get_mut(&mut self, id: QuestId) -> Option<&mut Quest> {
        self.quests.get_mut(&id)
    }

    pub fn contains(&self, id: QuestId) -> bool {
        self.quests.contains_key(&id)
    }

    pub fn take(&mut self, id: QuestId) -> Option<Quest> {
        self.quests.remove(&id)
    }

    /// Everything on offer, easiest first — the order the board was read out in.
    pub fn posted(&self) -> Vec<(QuestId, &Quest)> {
        let mut listed: Vec<(QuestId, &Quest)> =
            self.quests.iter().map(|(&id, q)| (id, q)).collect();
        listed.sort_by_key(|(id, q)| (q.total_cr(), *id));
        listed
    }

    /// A quest's board entry: `Quest 0A3F (CR 4): 2x Goblin (CR 1/4), Ogre (CR 2)`.
    pub fn describe(&self, id: QuestId) -> Option<Line> {
        let quest = self.get(id)?;
        Some(
            Line::bboard()
                .text(" Quest ")
                .bold(id)
                .text(" (total CR = ")
                .text(quest.total_cr())
                .text("): ")
                .text(quest.display()),
        )
    }

    /// Post a new quest. `force` pins the difficulty; see [`Quest::random`].
    pub fn add_quest(&mut self, force: Option<i64>, now: i64, rng: &mut GameRng) -> QuestId {
        let id = self.claim_id();
        self.quests.insert(id, Quest::random(force, now, rng));
        id
    }

    /// Find a free identifier. The reference simply wraps and would silently
    /// overwrite a live quest on collision; with at most a couple of dozen
    /// quests against 65,536 slots, skipping ahead always terminates.
    fn claim_id(&mut self) -> QuestId {
        loop {
            let id = QuestId(self.next_id);
            self.next_id = self.next_id.wrapping_add(1);
            if !self.quests.contains_key(&id) {
                return id;
            }
        }
    }

    /// Retire a quest, with a bit of theatre.
    pub fn remove_quest(&mut self, id: QuestId, rng: &mut GameRng, out: &mut Out) {
        if self.quests.remove(&id).is_none() {
            return;
        }
        let notice = rng.choice(&RETIREMENT_NOTICES).replace("{}", &id.to_string());
        out.broadcast(
            Line::bboard()
                .text(" ")
                .text(notice)
                .text(" Quest is removed from the Bulletin Board."),
        );
    }

    /// Choose a quest to retire, weighted heavily toward the oldest.
    pub fn select_to_remove(&self, rng: &mut GameRng) -> Option<QuestId> {
        if self.quests.is_empty() {
            return None;
        }
        let mut by_age: Vec<(QuestId, i64)> =
            self.quests.iter().map(|(&id, q)| (id, q.created_at)).collect();
        by_age.sort_by_key(|&(id, created)| (created, id));

        // Squaring the draw biases the pick toward the front of the list.
        let r = rng.unit();
        let slot = (r * r * (by_age.len() as f64 - 0.5)) as usize;
        Some(by_age[slot.min(by_age.len() - 1)].0)
    }

    /// Retire `count` quests and post the same number of replacements.
    pub fn replace_quests(
        &mut self,
        count: usize,
        announce: bool,
        now: i64,
        rng: &mut GameRng,
        out: &mut Out,
    ) {
        for _ in 0..count {
            if let Some(id) = self.select_to_remove(rng) {
                self.remove_quest(id, rng, out);
            }
        }
        for _ in 0..count {
            let id = self.add_quest(None, now, rng);
            if announce
                && let Some(line) = self.describe(id)
            {
                out.broadcast(line);
            }
        }
    }

    /// Top the board back up after a quest was completed.
    pub fn replenish(&mut self, now: i64, rng: &mut GameRng, out: &mut Out) {
        let posted = self.len();
        let low = if posted < 8 { 1 } else { 0 };
        let high = if posted > 16 {
            1
        } else if posted < 10 {
            3
        } else {
            2
        };
        for _ in 0..rng.range(low, high + 1) {
            let id = self.add_quest(None, now, rng);
            if let Some(line) = self.describe(id) {
                out.broadcast(line);
            }
        }
    }

    /// Advance the rotation cycle.
    pub fn tick(&mut self, now: i64, rng: &mut GameRng, out: &mut Out) {
        self.tick_cnt = (self.tick_cnt + 1) % CYCLE;
        if self.tick_cnt % 2 == 1 {
            let count = 1 + usize::from(self.tick_cnt == DOUBLE_ROTATION_STEP);
            self.replace_quests(count, true, now, rng, out);
        }
    }

    /// Merge `from` into `into`, as the quest-merging event does. Returns
    /// whether both quests existed.
    pub fn merge(&mut self, from: QuestId, into: QuestId, now: i64) -> bool {
        let Some(source) = self.quests.remove(&from) else {
            return false;
        };
        match self.quests.get_mut(&into) {
            Some(target) => {
                target.absorb(&source, now);
                true
            }
            None => {
                // Put it back rather than losing a quest to a bad call.
                self.quests.insert(from, source);
                false
            }
        }
    }

    /// The total challenge rating on offer, for the board summary.
    pub fn total_cr(&self) -> Cr {
        self.quests.values().map(|q| q.total_cr()).sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_new_board_opens_with_sixteen_gentle_quests() {
        let mut rng = GameRng::seed_from_u64(1);
        let board = BBoard::new(1_000, &mut rng);
        assert_eq!(board.len(), OPENING_QUESTS);
        // Opening quests are forced to difficulty 0-3, so nothing brutal.
        let hardest = board.posted().last().unwrap().1.total_cr();
        assert!(hardest < Cr::from_whole(40), "opening board peaked at CR {hardest}");
        assert!(board.posted().iter().all(|(_, q)| !q.is_empty()));
    }

    #[test]
    fn quests_are_listed_easiest_first() {
        let mut rng = GameRng::seed_from_u64(2);
        let board = BBoard::new(0, &mut rng);
        let crs: Vec<Cr> = board.posted().iter().map(|(_, q)| q.total_cr()).collect();
        assert!(crs.windows(2).all(|w| w[0] <= w[1]), "{crs:?}");
    }

    #[test]
    fn identifiers_are_unique_and_never_collide() {
        let mut rng = GameRng::seed_from_u64(3);
        let mut board = BBoard::default();
        let mut seen = std::collections::HashSet::new();
        for _ in 0..500 {
            let id = board.add_quest(Some(0), 0, &mut rng);
            assert!(seen.insert(id), "identifier {id} handed out twice");
        }
        assert_eq!(board.len(), 500);
    }

    #[test]
    fn identifiers_survive_wrapping_past_the_hex_ceiling() {
        let mut rng = GameRng::seed_from_u64(4);
        let mut board = BBoard { next_id: u16::MAX, ..Default::default() };
        let first = board.add_quest(Some(0), 0, &mut rng);
        let second = board.add_quest(Some(0), 0, &mut rng);
        assert_eq!(first, QuestId(0xFFFF));
        assert_eq!(second, QuestId(0));
        assert_ne!(first, second);
    }

    #[test]
    fn a_board_entry_names_its_id_and_total_challenge() {
        let mut rng = GameRng::seed_from_u64(5);
        let mut board = BBoard::default();
        let id = board.add_quest(Some(0), 0, &mut rng);
        let described = board.describe(id).unwrap().plain();
        assert!(described.contains(&id.to_string()), "{described}");
        assert!(described.contains("total CR = "), "{described}");
        assert!(described.contains(&board.get(id).unwrap().display()), "{described}");
    }

    #[test]
    fn describing_a_quest_that_is_gone_returns_nothing() {
        let board = BBoard::default();
        assert!(board.describe(QuestId(0x1234)).is_none());
    }

    #[test]
    fn removing_a_quest_announces_it_and_takes_it_off_the_board() {
        let mut rng = GameRng::seed_from_u64(6);
        let mut board = BBoard::new(0, &mut rng);
        let id = board.posted()[0].0;
        let mut out = Out::new();
        board.remove_quest(id, &mut rng, &mut out);

        assert!(!board.contains(id));
        assert_eq!(board.len(), OPENING_QUESTS - 1);
        let said = out.transcript()[0].clone();
        assert!(said.contains(&id.to_string()), "{said}");
        assert!(said.contains("removed from the Bulletin Board"), "{said}");
    }

    #[test]
    fn removing_a_quest_that_is_already_gone_says_nothing() {
        let mut rng = GameRng::seed_from_u64(7);
        let mut board = BBoard::default();
        let mut out = Out::new();
        board.remove_quest(QuestId(0xBEEF), &mut rng, &mut out);
        assert!(out.feed.is_empty());
    }

    #[test]
    fn retirement_picks_older_quests_far_more_often() {
        let mut rng = GameRng::seed_from_u64(8);
        let mut board = BBoard::default();
        let mut ids = Vec::new();
        for age in 0..10 {
            ids.push(board.add_quest(Some(0), age, &mut rng));
        }
        let mut picks = vec![0usize; 10];
        for _ in 0..4_000 {
            let chosen = board.select_to_remove(&mut rng).unwrap();
            picks[ids.iter().position(|&i| i == chosen).unwrap()] += 1;
        }
        assert!(
            picks[0] > picks[9] * 3,
            "the oldest quest should go first: {picks:?}"
        );
        assert!(picks.iter().all(|&p| p > 0), "every quest stays reachable: {picks:?}");
    }

    #[test]
    fn retirement_on_an_empty_board_finds_nothing() {
        let mut rng = GameRng::seed_from_u64(9);
        assert!(BBoard::default().select_to_remove(&mut rng).is_none());
    }

    #[test]
    fn replacing_quests_keeps_the_board_the_same_size() {
        let mut rng = GameRng::seed_from_u64(10);
        let mut board = BBoard::new(0, &mut rng);
        let mut out = Out::new();
        for round in 0..50 {
            board.replace_quests(2, false, round, &mut rng, &mut out);
            assert_eq!(board.len(), OPENING_QUESTS);
        }
    }

    #[test]
    fn replenishing_a_thin_board_always_posts_something() {
        let mut rng = GameRng::seed_from_u64(11);
        for _ in 0..50 {
            let mut board = BBoard::default();
            for _ in 0..5 {
                board.add_quest(Some(0), 0, &mut rng);
            }
            let mut out = Out::new();
            board.replenish(0, &mut rng, &mut out);
            assert!(board.len() > 5, "a board under eight quests must grow");
            assert!(!out.feed.is_empty(), "new quests are announced");
        }
    }

    #[test]
    fn replenishing_a_crowded_board_adds_at_most_one() {
        let mut rng = GameRng::seed_from_u64(12);
        for _ in 0..50 {
            let mut board = BBoard::default();
            for _ in 0..17 {
                board.add_quest(Some(0), 0, &mut rng);
            }
            let mut out = Out::new();
            board.replenish(0, &mut rng, &mut out);
            assert!(board.len() <= 18, "the board caps out at eighteen quests");
        }
    }

    #[test]
    fn the_board_stays_within_its_bounds_over_a_long_run() {
        let mut rng = GameRng::seed_from_u64(13);
        let mut board = BBoard::new(0, &mut rng);
        let mut out = Out::new();
        for step in 0..2_000i64 {
            board.tick(step, &mut rng, &mut out);
            if step % 7 == 0 {
                // Simulate a player completing something.
                if let Some((id, _)) = board.posted().first() {
                    let id = *id;
                    board.take(id);
                    board.replenish(step, &mut rng, &mut out);
                }
            }
            assert!(
                (4..=20).contains(&board.len()),
                "board drifted to {} quests at step {step}", board.len()
            );
        }
    }

    #[test]
    fn rotation_only_happens_on_odd_cycle_steps() {
        let mut rng = GameRng::seed_from_u64(14);
        let mut board = BBoard::new(0, &mut rng);
        let mut rotations = 0;
        for _ in 0..CYCLE {
            let mut out = Out::new();
            board.tick(0, &mut rng, &mut out);
            if !out.feed.is_empty() {
                rotations += 1;
            }
        }
        assert_eq!(rotations, 16, "half of a 32-step cycle");
    }

    #[test]
    fn merging_moves_every_monster_and_refreshes_the_posting() {
        let mut rng = GameRng::seed_from_u64(15);
        let mut board = BBoard::default();
        let a = board.add_quest(Some(0), 100, &mut rng);
        let b = board.add_quest(Some(0), 200, &mut rng);
        let (cr_a, cr_b) = (board.get(a).unwrap().total_cr(), board.get(b).unwrap().total_cr());
        let monsters = board.get(a).unwrap().monsters().len() + board.get(b).unwrap().monsters().len();

        assert!(board.merge(b, a, 300));
        assert!(!board.contains(b));
        let merged = board.get(a).unwrap();
        assert_eq!(merged.total_cr(), cr_a + cr_b);
        assert_eq!(merged.monsters().len(), monsters);
        assert_eq!(merged.created_at, 300);
    }

    #[test]
    fn a_failed_merge_loses_nothing() {
        let mut rng = GameRng::seed_from_u64(16);
        let mut board = BBoard::default();
        let a = board.add_quest(Some(0), 0, &mut rng);

        assert!(!board.merge(QuestId(0xDEAD), a, 0), "source does not exist");
        assert_eq!(board.len(), 1);

        assert!(!board.merge(a, QuestId(0xDEAD), 0), "target does not exist");
        assert_eq!(board.len(), 1, "the source quest is put back");
        assert!(board.contains(a));
    }

    #[test]
    fn a_board_survives_a_serde_round_trip() {
        let mut rng = GameRng::seed_from_u64(17);
        let board = BBoard::new(4_242, &mut rng);
        let json = serde_json::to_string(&board).unwrap();
        let back: BBoard = serde_json::from_str(&json).unwrap();
        assert_eq!(back, board);
        assert_eq!(back.posted().len(), board.posted().len());
    }
}
