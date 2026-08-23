//! An adventurer.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::assets::{self, ItemId, ItemStats, MonsterId};
use crate::battle::{Attack, PlayerStrategy, Strategy, Warrior, battle};
use crate::config::{Pacing, time_display};
use crate::numeric::{Cr, ability_modifier, bit_length};
use crate::out::{Line, Out};
use crate::quest::Quest;
use crate::rng::GameRng;

/// XP thresholds for levels 1-20, straight from the D&D5 table.
const LEVEL_THRESHOLDS: [i64; 20] = [
    0, 300, 900, 2_700, 6_500, 14_000, 23_000, 34_000, 48_000, 64_000, 85_000, 100_000,
    120_000, 140_000, 165_000, 195_000, 225_000, 265_000, 305_000, 355_000,
];

/// XP awarded for defeating a monster of CR 1 through 30.
const XP_BY_CR: [i64; 30] = [
    200, 450, 700, 1_100, 1_800, 2_300, 2_900, 3_900, 5_000, 5_900, 7_200, 8_400, 10_000,
    11_500, 13_000, 15_000, 18_000, 20_000, 22_000, 25_000, 33_000, 41_000, 50_000, 62_000,
    75_000, 90_000, 105_000, 120_000, 135_000, 155_000,
];

/// What a monster is worth.
///
/// The reference deflates the D&D table hard — XP would otherwise outrun the
/// level curve within an evening — and pays a flat 10 for the CR 0 monsters
/// that can actually fight back.
pub fn monster_xp(id: MonsterId) -> i64 {
    let m = assets::monster(id);
    if m.cr == Cr::ZERO && !m.attacks.is_empty() {
        return 10;
    }
    let base = if m.cr < Cr::ONE {
        // int(cr * 200), exact because CR below 1 is always eighths.
        i64::from(m.cr.eighths()) * 25
    } else {
        XP_BY_CR[m.cr.truncated() as usize - 1]
    };
    base / (2 * i64::from(bit_length(m.cr.truncated() as u64 + 1)))
}

pub fn quest_xp(quest: &Quest) -> i64 {
    quest.monsters().iter().copied().map(monster_xp).sum()
}

/// Per-player event cooldowns. The reference counts these in event ticks; here
/// they are timestamps, so retuning the heartbeat cannot rescale them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventClass {
    /// Stumbling on coins, or being the one who picks a pocket.
    Chance,
    /// Finding a potion, blessed or cursed.
    Potion,
    /// Being called to the Path of the Glory.
    Glory,
    /// Being drafted into an epic or creepy quest.
    PartyQuest,
    /// Having one's purse cut.
    Victim,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct User {
    pub nick: String,
    pub created_at: i64,
    pub money: i64,
    pub weapon: Option<ItemId>,
    /// Fights fought with the current weapon, which is how mastery is earned.
    pub weapon_count: u32,
    pub proficiency_bonus: i32,
    pub armor: Option<ItemId>,
    /// Unix seconds until which this player must rest.
    pub idle_until: i64,
    pub level: i32,
    pub xp: i64,
    pub strength: i32,
    pub dexterity: i32,
    pub hp: i32,
    pub ac: i32,
    pub quest_win: u32,
    pub quest_lost: u32,
    pub strategy: PlayerStrategy,
    /// When this player last accepted a quest; drives the inactivity purge.
    pub last_quest_at: i64,
    #[serde(default)]
    pub cooldowns: BTreeMap<EventClass, i64>,
}

impl User {
    /// A fresh level-1 character. The starting stats are the D&D standard
    /// array: strength 15, dexterity 14, constitution 13.
    pub fn new(nick: impl Into<String>, now: i64) -> Self {
        Self {
            nick: nick.into(),
            created_at: now,
            money: 0,
            weapon: None,
            weapon_count: 0,
            proficiency_bonus: 0,
            armor: None,
            idle_until: 0,
            level: 1,
            xp: 0,
            strength: 15,
            dexterity: 14,
            hp: 10,
            ac: 12, // 10 + modifier(dexterity 14)
            quest_win: 0,
            quest_lost: 0,
            strategy: PlayerStrategy::Random,
            last_quest_at: now,
            cooldowns: BTreeMap::new(),
        }
    }

    pub fn is_resting(&self, now: i64) -> bool {
        now < self.idle_until
    }

    pub fn rest_remaining(&self, now: i64) -> i64 {
        (self.idle_until - now).max(0)
    }

    /// Whether an event of this class may pick this player again yet.
    pub fn event_ready(&self, class: EventClass, now: i64, cooldown: i64) -> bool {
        match self.cooldowns.get(&class) {
            Some(&last) => now - last >= cooldown,
            None => true,
        }
    }

    pub fn mark_event(&mut self, class: EventClass, now: i64) {
        self.cooldowns.insert(class, now);
    }

    /// The level this player's XP entitles them to.
    pub fn level_for_xp(&self) -> i32 {
        if self.xp >= LEVEL_THRESHOLDS[19] {
            return 20;
        }
        LEVEL_THRESHOLDS
            .iter()
            .position(|&threshold| threshold > self.xp)
            .unwrap_or(20) as i32
    }

    /// Apply any levels earned, rolling hit points for each. Returns how many.
    pub fn level_up(&mut self, rng: &mut GameRng, out: &mut Out) -> i32 {
        let new_level = self.level_for_xp();
        let gained = new_level - self.level;
        if gained <= 0 {
            self.level = new_level;
            return 0;
        }
        self.level = new_level;

        for _ in 0..gained {
            // Take the fixed 6 two times in three, otherwise roll a d10.
            let base = if rng.range(0, 3) < 2 { 6 } else { rng.die(10) };
            // Plus 2: one for racial constitution, one for an average
            // constitution of 13.
            self.hp += base + 2;
            self.strength += 1;
            self.dexterity += 1;
            if self.armor.is_none() {
                self.ac = 10 + ability_modifier(self.dexterity);
            }
        }

        out.broadcast(
            Line::system()
                .nick(&self.nick)
                .text(" has gained ")
                .text(gained)
                .text(if gained > 1 { " levels!" } else { " level!" })
                .text(" New level is ")
                .text(self.level)
                .text(". New HP is ")
                .text(self.hp)
                .text("."),
        );
        gained
    }

    pub fn give_xp(&mut self, xp: i64, rng: &mut GameRng, out: &mut Out) -> i32 {
        self.xp += xp;
        out.broadcast(
            Line::system()
                .nick(&self.nick)
                .text(" has gained ")
                .text(xp)
                .text(" XP! (Total XP = ")
                .text(self.xp)
                .text(", current money = ")
                .text(self.money)
                .text(")."),
        );
        self.level_up(rng, out)
    }

    /// Record another fight with the current weapon, and grant mastery once
    /// enough have been fought.
    ///
    /// Expensive and heavy weapons take longer to master: the threshold is
    /// `(cost.bit_length() * weight.bit_length()) >> 1`, which spans 2 to 32
    /// fights across the catalogue.
    pub fn compute_proficiency(&mut self, out: &mut Out) {
        let Some(weapon) = self.weapon.and_then(assets::item) else {
            self.proficiency_bonus = 0;
            return;
        };
        self.weapon_count += 1;
        if self.proficiency_bonus != 0 {
            return;
        }

        let required =
            (bit_length(weapon.cost as u64) * bit_length(weapon.weight as u64)) >> 1;
        if self.weapon_count >= required {
            self.proficiency_bonus = (self.level + 7) >> 2;
        }
        if self.weapon_count == required {
            out.broadcast(
                Line::system()
                    .nick(&self.nick)
                    .text(" has now a proficiency bonus (+")
                    .text(self.proficiency_bonus)
                    .text(") when using a ")
                    .text(&weapon.name)
                    .text("!"),
            );
        }
    }

    /// The dice a player's weapon contributes. A bare-handed adventurer rolls
    /// nothing at all, exactly as in the reference.
    fn weapon_damage(&self) -> Vec<crate::dice::Die> {
        match self.weapon.and_then(assets::item).map(|i| &i.stats) {
            Some(ItemStats::Weapon { damage }) => vec![*damage],
            _ => Vec::new(),
        }
    }

    /// Turn this player into a combatant.
    ///
    /// With `strict`, the character's own numbers are used, which is what
    /// player-versus-player events want. Otherwise they are secretly inflated:
    /// D&D5 balances a party against one monster, and here one adventurer faces
    /// a whole warband, so without this the player loses nearly every fight.
    pub fn build_warrior(&self, strategy: Strategy, strict: bool) -> Warrior {
        let strength_mod = ability_modifier(self.strength);
        let mut warrior = Warrior::new(
            self.hp,
            vec![Attack {
                attack_bonus: strength_mod + self.proficiency_bonus,
                damage: self.weapon_damage(),
                damage_bonus: strength_mod,
            }],
            self.dexterity,
            self.ac,
            Cr::from_whole(self.level),
            strategy,
        )
        .owned_by(&self.nick);

        if strict {
            return warrior;
        }

        let armor_grade = match self.armor.and_then(assets::item) {
            Some(item) => bit_length(item.cost as u64) as i32,
            None => 6,
        };
        warrior.ac += armor_grade + (self.level >> 2) + 2;
        warrior.hp += 2 * self.level + (armor_grade >> 1) + 2;

        let weapon_grade = match self.weapon.and_then(assets::item) {
            Some(item) => bit_length(item.cost as u64) as i32,
            None => 1,
        };
        warrior.attacks[0].attack_bonus += weapon_grade;
        warrior.attacks[0].damage_bonus += weapon_grade;
        warrior
    }

    /// Take on a quest. Returns whether the adventurer prevailed.
    ///
    /// Cost and stock have already been settled by the caller; this resolves
    /// the fight, pays out, and sets the rest timer.
    pub fn fight(
        &mut self,
        quest: &Quest,
        pacing: &Pacing,
        now: i64,
        rng: &mut GameRng,
        out: &mut Out,
    ) -> bool {
        let mut warriors: Vec<Warrior> = Vec::with_capacity(quest.monsters().len() + 1);
        // The adventurer goes in first so the monsters know who to hunt.
        warriors.push(self.build_warrior(Strategy::Solo(self.strategy), false));
        let me = 0;
        for &id in quest.monsters() {
            warriors.push(Warrior::monster(id, Strategy::HuntPlayer(me), rng));
        }

        let victory = battle(&mut warriors, rng).won_by(me);

        if victory {
            self.quest_win += 1;
            let reward = quest_xp(quest);
            self.money += reward;
            self.give_xp(reward, rng, out);
        } else {
            self.quest_lost += 1;
        }

        self.compute_proficiency(out);
        self.idle_until =
            now + pacing.idle_after_fight(i64::from(self.level), quest.total_cr(), victory);
        victory
    }

    /// Equip a purchase. Returns false if the player already had this item, in
    /// which case nothing is spent.
    pub fn buy(&mut self, id: ItemId) -> bool {
        let Some(item) = assets::item(id) else {
            return false;
        };
        match item.stats {
            ItemStats::Weapon { .. } => {
                if self.weapon == Some(id) {
                    return false;
                }
                // A new weapon has to be mastered from scratch.
                self.weapon = Some(id);
                self.weapon_count = 0;
                self.proficiency_bonus = 0;
            }
            ItemStats::Armor { base, dex_bonus, max_bonus } => {
                if self.armor == Some(id) {
                    return false;
                }
                self.armor = Some(id);
                let mut bonus =
                    if dex_bonus { ability_modifier(self.dexterity) } else { 0 };
                if let Some(cap) = max_bonus {
                    bonus = bonus.min(cap);
                }
                self.ac = base + bonus;
            }
        }
        self.money -= item.cost;
        true
    }

    /// Returns the strategy's name if it was understood.
    pub fn set_strategy(&mut self, name: &str) -> Option<&'static str> {
        let strategy = PlayerStrategy::parse(name)?;
        self.strategy = strategy;
        Some(strategy.name())
    }

    pub fn about(&self, now: i64) -> Line {
        let mut line = Line::system()
            .text("Your stats: level ")
            .text(self.level)
            .text(", ")
            .text(self.xp)
            .text(" XP, money = ")
            .text(self.money)
            .text(", current HP is ")
            .text(self.hp)
            .text(".");

        if let Some(weapon) = self.weapon.and_then(assets::item) {
            line = line
                .text(" You wield a ")
                .text(&weapon.name)
                .text(" (cost: ")
                .text(weapon.cost)
                .text(").");
        }
        if let Some(armor) = self.armor.and_then(assets::item) {
            line = line
                .text(" You wear a ")
                .text(&armor.name)
                .text(" (cost: ")
                .text(armor.cost)
                .text(").");
        }
        let resting = self.rest_remaining(now);
        if resting > 0 {
            line = line.text(" You must wait ").text(time_display(resting)).text(".");
        }
        line
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::assets::ItemKind;

    fn item_named(name: &str) -> ItemId {
        assets::items().iter().find(|i| i.name == name).unwrap().id
    }

    fn monster_named(name: &str) -> MonsterId {
        assets::monsters().iter().position(|m| m.name == name).unwrap() as MonsterId
    }

    #[test]
    fn a_new_adventurer_starts_on_the_standard_array() {
        let u = User::new("Absalom", 1_000);
        assert_eq!((u.level, u.xp, u.money), (1, 0, 0));
        assert_eq!((u.strength, u.dexterity, u.hp, u.ac), (15, 14, 10, 12));
        assert!(u.weapon.is_none() && u.armor.is_none());
        assert_eq!(u.strategy, PlayerStrategy::Random);
        assert!(!u.is_resting(1_000));
    }

    #[test]
    fn the_level_curve_matches_the_dnd_thresholds() {
        let mut u = User::new("A", 0);
        for (xp, expected) in [
            (0, 1),
            (299, 1),
            (300, 2),
            (899, 2),
            (900, 3),
            (2_699, 3),
            (2_700, 4),
            (304_999, 18),
            (305_000, 19),
            (354_999, 19),
            (355_000, 20),
            (10_000_000, 20),
        ] {
            u.xp = xp;
            assert_eq!(u.level_for_xp(), expected, "at {xp} XP");
        }
    }

    #[test]
    fn level_twenty_needs_the_full_threshold() {
        // The table's last entry doubles as level 19's ceiling, so 20 is only
        // reached through the `xp >= 355_000` short-circuit, not the scan.
        let mut u = User::new("A", 0);
        u.xp = LEVEL_THRESHOLDS[19] - 1;
        assert_eq!(u.level_for_xp(), 19);
        u.xp = LEVEL_THRESHOLDS[19];
        assert_eq!(u.level_for_xp(), 20);
    }

    #[test]
    fn levelling_up_raises_stats_and_announces_once() {
        let mut rng = GameRng::seed_from_u64(1);
        let mut out = Out::new();
        let mut u = User::new("Absalom", 0);
        u.xp = 900; // straight to level 3

        let gained = u.level_up(&mut rng, &mut out);
        assert_eq!(gained, 2);
        assert_eq!(u.level, 3);
        assert_eq!((u.strength, u.dexterity), (17, 16));
        assert!(u.hp > 10, "hit points should have been rolled");
        assert_eq!(out.feed.len(), 1, "one announcement for the whole jump");
        assert!(out.transcript()[0].contains("has gained 2 levels!"));
    }

    #[test]
    fn levelling_up_with_no_xp_says_nothing() {
        let mut rng = GameRng::seed_from_u64(2);
        let mut out = Out::new();
        let mut u = User::new("A", 0);
        assert_eq!(u.level_up(&mut rng, &mut out), 0);
        assert!(out.feed.is_empty());
    }

    #[test]
    fn armour_class_tracks_dexterity_only_while_unarmoured() {
        let mut rng = GameRng::seed_from_u64(3);
        let mut out = Out::new();
        let mut u = User::new("A", 0);
        u.xp = 6_500; // level 5, so +4 dexterity
        u.level_up(&mut rng, &mut out);
        assert_eq!(u.dexterity, 18);
        assert_eq!(u.ac, 10 + ability_modifier(18));

        let mut armoured = User::new("B", 0);
        armoured.money = 1_000_000;
        armoured.buy(item_named("Plate"));
        let plate_ac = armoured.ac;
        armoured.xp = 6_500;
        armoured.level_up(&mut rng, &mut out);
        assert_eq!(armoured.ac, plate_ac, "armour fixes AC regardless of dexterity");
    }

    #[test]
    fn giving_xp_reports_the_running_total() {
        let mut rng = GameRng::seed_from_u64(4);
        let mut out = Out::new();
        let mut u = User::new("Absalom", 0);
        u.money = 55;
        u.give_xp(120, &mut rng, &mut out);
        assert_eq!(u.xp, 120);
        let said = out.transcript().join(" ");
        assert!(said.contains("has gained 120 XP!"), "{said}");
        assert!(said.contains("Total XP = 120"), "{said}");
        assert!(said.contains("current money = 55"), "{said}");
    }

    #[test]
    fn monster_xp_matches_the_reference_formula() {
        // CR 0 with attacks pays a flat 10, bypassing the deflation.
        let kobold = monster_named("Kobold"); // CR 1/8
        assert_eq!(monster_xp(kobold), 12, "int(0.125*200) // 2");
        let goblin = monster_named("Goblin"); // CR 1/4
        assert_eq!(monster_xp(goblin), 25);
        let ogre = monster_named("Ogre"); // CR 2
        assert_eq!(monster_xp(ogre), 450 / (2 * 2));
        let aboleth = monster_named("Aboleth"); // CR 10
        assert_eq!(monster_xp(aboleth), 5_900 / (2 * 4));
    }

    #[test]
    fn harmless_cr_zero_monsters_are_worth_nothing() {
        let frog = monster_named("Frog");
        assert!(assets::monster(frog).attacks.is_empty());
        assert_eq!(monster_xp(frog), 0);
    }

    #[test]
    fn every_monster_has_a_sane_xp_value() {
        for id in 0..assets::monsters().len() as MonsterId {
            let xp = monster_xp(id);
            assert!(xp >= 0, "{} pays {xp}", assets::monster(id).name);
            assert!(xp < 20_000, "{} pays {xp}", assets::monster(id).name);
        }
    }

    #[test]
    fn buying_a_weapon_resets_mastery_and_charges_the_cost() {
        let mut u = User::new("A", 0);
        u.money = 10_000;
        let club = item_named("Club");
        assert!(u.buy(club));
        assert_eq!(u.weapon, Some(club));
        assert_eq!(u.money, 10_000 - 10);

        let mut out = Out::new();
        for _ in 0..5 {
            u.compute_proficiency(&mut out);
        }
        assert!(u.proficiency_bonus > 0, "a Club is quickly mastered");

        assert!(u.buy(item_named("Greatsword")));
        assert_eq!(u.weapon_count, 0);
        assert_eq!(u.proficiency_bonus, 0, "a new weapon starts unmastered");
    }

    #[test]
    fn rebuying_the_same_item_is_refused_and_costs_nothing() {
        let mut u = User::new("A", 0);
        u.money = 10_000;
        let club = item_named("Club");
        assert!(u.buy(club));
        let after = u.money;
        assert!(!u.buy(club));
        assert_eq!(u.money, after);
    }

    #[test]
    fn armour_applies_its_dexterity_rules() {
        let mut u = User::new("A", 0);
        u.money = 10_000_000;

        // Padded: base 11, unlimited dexterity bonus.
        u.buy(item_named("Padded"));
        assert_eq!(u.ac, 11 + ability_modifier(u.dexterity));

        // Plate: base 18, no dexterity bonus at all.
        u.buy(item_named("Plate"));
        assert_eq!(u.ac, 18);

        // Half Plate caps the bonus at 2.
        let half = assets::items().iter().find(|i| i.name == "Half Plate").unwrap();
        let ItemStats::Armor { base, max_bonus, .. } = half.stats else {
            panic!("Half Plate should be armour")
        };
        u.dexterity = 20; // modifier +5, above the cap
        u.buy(half.id);
        assert_eq!(u.ac, base + max_bonus.unwrap());
    }

    #[test]
    fn proficiency_takes_longer_with_expensive_heavy_weapons() {
        let threshold = |name: &str| {
            let w = assets::items().iter().find(|i| i.name == name).unwrap();
            (bit_length(w.cost as u64) * bit_length(w.weight as u64)) >> 1
        };
        assert!(
            threshold("Greatsword") > threshold("Club"),
            "a Greatsword should take longer to master than a Club"
        );
    }

    #[test]
    fn mastery_is_announced_exactly_once() {
        let mut u = User::new("Absalom", 0);
        u.money = 100;
        u.buy(item_named("Club"));
        let mut out = Out::new();
        for _ in 0..40 {
            u.compute_proficiency(&mut out);
        }
        let announcements = out
            .transcript()
            .iter()
            .filter(|l| l.contains("proficiency bonus"))
            .count();
        assert_eq!(announcements, 1);
        assert_eq!(u.weapon_count, 40);
    }

    #[test]
    fn a_bare_handed_adventurer_never_gains_proficiency() {
        let mut u = User::new("A", 0);
        let mut out = Out::new();
        u.compute_proficiency(&mut out);
        assert_eq!(u.proficiency_bonus, 0);
        assert_eq!(u.weapon_count, 0);
        assert!(out.feed.is_empty());
    }

    #[test]
    fn the_combat_tuning_only_applies_to_quests_not_duels() {
        let mut u = User::new("A", 0);
        u.money = 10_000_000;
        u.buy(item_named("Greatsword"));
        u.buy(item_named("Plate"));

        let strict = u.build_warrior(Strategy::Solo(u.strategy), true);
        let tuned = u.build_warrior(Strategy::Solo(u.strategy), false);

        assert_eq!((strict.hp, strict.ac), (u.hp, u.ac));
        assert!(tuned.hp > strict.hp, "quest warriors get extra hit points");
        assert!(tuned.ac > strict.ac, "quest warriors get extra armour");
        assert!(tuned.attacks[0].attack_bonus > strict.attacks[0].attack_bonus);
        assert_eq!(tuned.owner.as_deref(), Some("A"));
    }

    #[test]
    fn a_bare_handed_warrior_rolls_no_damage_dice() {
        let u = User::new("A", 0);
        let w = u.build_warrior(Strategy::Solo(u.strategy), true);
        assert!(w.attacks[0].damage.is_empty());
        let mut rng = GameRng::seed_from_u64(5);
        assert_eq!(crate::dice::roll_all(&w.attacks[0].damage, &mut rng), 0);
    }

    #[test]
    fn winning_a_quest_pays_xp_and_coin_and_sets_a_rest() {
        let mut rng = GameRng::seed_from_u64(6);
        let mut out = Out::new();
        let pacing = Pacing::WEB;

        // A well-equipped level-20 hero against a single frog cannot lose.
        let mut u = User::new("Hero", 0);
        u.money = 10_000_000;
        u.buy(item_named("Greatsword"));
        u.buy(item_named("Plate"));
        u.level = 20;
        u.hp = 300;
        let purse = u.money;

        let quest = Quest::from_roster(vec![monster_named("Goblin")], 0);
        let expected = quest_xp(&quest);
        assert!(u.fight(&quest, &pacing, 1_000, &mut rng, &mut out));

        assert_eq!(u.quest_win, 1);
        assert_eq!(u.quest_lost, 0);
        assert_eq!(u.xp, expected);
        assert_eq!(u.money, purse + expected);
        assert!(u.is_resting(1_000), "a fight should tire the adventurer");
        assert!(u.idle_until <= 1_000 + pacing.idle_max_secs);
        assert_eq!(u.weapon_count, 1, "the fight counts toward mastery");
    }

    #[test]
    fn losing_a_quest_pays_nothing_but_still_tires_and_trains() {
        let mut rng = GameRng::seed_from_u64(7);
        let mut out = Out::new();
        let pacing = Pacing::WEB;

        let mut u = User::new("Doomed", 0);
        u.money = 100;
        u.buy(item_named("Club"));
        let purse = u.money;

        // A level-1 character against an ancient dragon.
        let quest = Quest::from_roster(vec![monster_named("Adult Red Dragon")], 0);
        assert!(!u.fight(&quest, &pacing, 1_000, &mut rng, &mut out));

        assert_eq!(u.quest_lost, 1);
        assert_eq!((u.xp, u.money), (0, purse));
        assert!(u.is_resting(1_000));
        assert_eq!(u.weapon_count, 1, "a defeat still teaches the weapon");
    }

    #[test]
    fn strategies_are_named_and_validated() {
        let mut u = User::new("A", 0);
        assert_eq!(u.set_strategy("WISE"), Some("wise"));
        assert_eq!(u.strategy, PlayerStrategy::Wise);
        assert_eq!(u.set_strategy("nonsense"), None);
        assert_eq!(u.strategy, PlayerStrategy::Wise, "a bad name changes nothing");
    }

    #[test]
    fn the_stat_line_mentions_gear_and_rest_only_when_they_apply() {
        let mut u = User::new("Absalom", 0);
        let bare = u.about(0).plain();
        assert!(bare.starts_with("Your stats: level 1, 0 XP, money = 0, current HP is 10."));
        assert!(!bare.contains("You wield"));
        assert!(!bare.contains("You wear"));
        assert!(!bare.contains("You must wait"));

        u.money = 10_000_000;
        u.buy(item_named("Greatsword"));
        u.buy(item_named("Plate"));
        u.idle_until = 3_600;
        let full = u.about(0).plain();
        assert!(full.contains("You wield a Greatsword (cost: 5000)."), "{full}");
        assert!(full.contains("You wear a Plate (cost: 150000)."), "{full}");
        assert!(full.contains("You must wait 01h00m00s."), "{full}");
    }

    #[test]
    fn event_cooldowns_start_ready_and_then_hold() {
        let mut u = User::new("A", 0);
        let cooldown = 3_600;
        assert!(u.event_ready(EventClass::Potion, 0, cooldown), "never fired yet");

        u.mark_event(EventClass::Potion, 1_000);
        assert!(!u.event_ready(EventClass::Potion, 1_500, cooldown));
        assert!(u.event_ready(EventClass::Potion, 4_600, cooldown));
        // Classes are tracked independently.
        assert!(u.event_ready(EventClass::Chance, 1_500, cooldown));
    }

    #[test]
    fn a_user_survives_a_serde_round_trip() {
        let mut u = User::new("Absalom", 42);
        u.money = 999;
        u.buy(item_named("Club"));
        u.mark_event(EventClass::Glory, 500);
        let json = serde_json::to_string(&u).unwrap();
        assert_eq!(serde_json::from_str::<User>(&json).unwrap(), u);
    }

    #[test]
    fn items_are_split_cleanly_between_the_two_slots() {
        let mut u = User::new("A", 0);
        u.money = 100_000_000;
        for item in assets::items() {
            u.buy(item.id);
            match item.kind() {
                ItemKind::Weapon => assert_eq!(u.weapon, Some(item.id)),
                ItemKind::Armor => assert_eq!(u.armor, Some(item.id)),
            }
        }
        assert!(u.weapon.is_some() && u.armor.is_some());
    }
}
