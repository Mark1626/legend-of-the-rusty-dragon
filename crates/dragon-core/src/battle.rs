//! The D&D5-flavoured combat resolver.
//!
//! Faithful to the reference with two deliberate corrections, both of which the
//! original could get away with in a long-lived IRC process and this port
//! cannot:
//!
//! * **The loop is bounded.** `user.battle` carries a standing
//!   `# TODO: add counter for avoiding infinite battle?`. Two warriors that can
//!   never damage each other spin forever; under a serverless request that is a
//!   function timeout, a rolled-back transaction and a lost turn.
//! * **Mutual destruction is handled.** The reference ends with
//!   `return warriors[0]` after filtering out the dead, which raises
//!   `IndexError` if the last combatants fall in the same round. Here that is
//!   `None`, and a player who didn't survive simply didn't win.

use serde::{Deserialize, Serialize};

use crate::assets::{self, MonsterId};
use crate::dice::{DiceSum, roll_all};
use crate::numeric::{Cr, ability_modifier};
use crate::rng::GameRng;

/// Hard ceiling on combat rounds. Generously above any real fight: even a
/// level-1 character chipping at a CR-30 monster resolves in well under a
/// hundred. Only genuine stalemates reach this.
pub const MAX_ROUNDS: u32 = 1_000;

/// Index into the warrior roster of a single battle.
pub type WarriorId = usize;

#[derive(Debug, Clone)]
pub struct Attack {
    pub attack_bonus: i32,
    /// Empty for a player with no weapon, which rolls 0 damage.
    pub damage: DiceSum,
    pub damage_bonus: i32,
}

/// How a combatant picks its next target. Lower score wins, and ties go to
/// whoever is encountered first, matching the reference's strict `<`.
///
/// This is the one part of combat that persists: a player's chosen strategy is
/// remembered between fights.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlayerStrategy {
    /// Pick a random target each turn.
    Random,
    /// Weakest first, by current HP.
    Harsh,
    /// Easiest first, by challenge rating.
    Rage,
    /// Most difficult first, by challenge rating.
    Wise,
}

impl PlayerStrategy {
    pub fn parse(name: &str) -> Option<Self> {
        match name.trim().to_ascii_lowercase().as_str() {
            "random" => Some(Self::Random),
            "harsh" => Some(Self::Harsh),
            "rage" => Some(Self::Rage),
            "wise" => Some(Self::Wise),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Random => "random",
            Self::Harsh => "harsh",
            Self::Rage => "rage",
            Self::Wise => "wise",
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub enum Strategy {
    /// A player fighting monsters, or duelling other players: their own choice.
    Solo(PlayerStrategy),
    /// A player in a party: never turn on a fellow adventurer.
    Party(PlayerStrategy),
    /// A monster on a solo quest: fixate on the one adventurer present.
    HuntPlayer(WarriorId),
    /// A monster on a party quest: lunge at a party member picked afresh each
    /// time it looks, so aggro scatters across the group.
    HuntAnyPlayer,
}

#[derive(Debug, Clone)]
pub struct Warrior {
    pub hp: i32,
    pub attacks: Vec<Attack>,
    pub dexterity: i32,
    pub ac: i32,
    /// Challenge rating for monsters; the character's level for players, which
    /// is the same scale the reference compared them on.
    pub threat: Cr,
    pub strategy: Strategy,
    /// The player this warrior belongs to, or `None` for a monster.
    pub owner: Option<String>,
    initiative: i32,
}

impl Warrior {
    pub fn new(
        hp: i32,
        attacks: Vec<Attack>,
        dexterity: i32,
        ac: i32,
        threat: Cr,
        strategy: Strategy,
    ) -> Self {
        Self { hp, attacks, dexterity, ac, threat, strategy, owner: None, initiative: 0 }
    }

    pub fn owned_by(mut self, nick: impl Into<String>) -> Self {
        self.owner = Some(nick.into());
        self
    }

    /// Roll a monster into the fight; its HP comes from its hit dice.
    pub fn monster(id: MonsterId, strategy: Strategy, rng: &mut GameRng) -> Self {
        let m = assets::monster(id);
        let attacks = m
            .attacks
            .iter()
            .map(|a| Attack {
                attack_bonus: a.attack_bonus,
                damage: a.damage.clone(),
                damage_bonus: a.damage_bonus,
            })
            .collect();
        Self::new(m.hit_dice.roll(rng), attacks, m.dex, m.ac, m.cr, strategy)
    }

    pub fn is_player(&self) -> bool {
        self.owner.is_some()
    }

    pub fn dexterity_modifier(&self) -> i32 {
        ability_modifier(self.dexterity)
    }
}

/// Why a battle stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ending {
    /// Exactly one warrior was left standing.
    LastStanding,
    /// Nobody was standing to begin with, so there is no winner.
    ///
    /// Combat itself cannot reach this: a warrior killed earlier in a round
    /// never gets their turn, so the last exchange always leaves its instigator
    /// alive. It guards the degenerate rosters — empty, or everyone already
    /// down — on which the reference's `return warriors[0]` raises `IndexError`.
    MutualDestruction,
    /// [`MAX_ROUNDS`] elapsed, so the healthiest survivor was awarded the win.
    Stalemate,
    /// Nobody left alive could deal damage, so the fight could never end.
    Deadlock,
}

#[derive(Debug, Clone)]
pub struct Outcome {
    pub winner: Option<WarriorId>,
    pub ending: Ending,
    pub rounds: u32,
}

impl Outcome {
    pub fn won_by(&self, id: WarriorId) -> bool {
        self.winner == Some(id)
    }
}

/// A single d20 attack roll. Natural 1 always misses; natural 20 always hits
/// and lands a critical.
enum AttackRoll {
    Fumble,
    Critical,
    Normal(i32),
}

impl AttackRoll {
    fn roll(rng: &mut GameRng) -> Self {
        match rng.die(20) {
            1 => Self::Fumble,
            20 => Self::Critical,
            other => Self::Normal(other),
        }
    }

    fn hits(&self, attack_bonus: i32, target_ac: i32) -> bool {
        match self {
            Self::Fumble => false,
            Self::Critical => true,
            Self::Normal(d) => d + attack_bonus >= target_ac,
        }
    }

    fn is_critical(&self) -> bool {
        matches!(self, Self::Critical)
    }
}

/// Resolve a battle to the death. Returns which warrior survived, as an index
/// into `warriors`, which is left holding each combatant's final state.
pub fn battle(warriors: &mut [Warrior], rng: &mut GameRng) -> Outcome {
    // Captured before the fight and never narrowed, mirroring the reference's
    // `_warriors_`: monsters may lunge at an adventurer who has already fallen,
    // in which case they simply hit nothing that turn.
    let party: Vec<WarriorId> =
        (0..warriors.len()).filter(|&i| warriors[i].is_player()).collect();

    let mut alive: Vec<WarriorId> =
        (0..warriors.len()).filter(|&i| warriors[i].hp > 0).collect();
    let mut rounds = 0;

    while alive.len() > 1 {
        // A fight nobody can win would otherwise burn every remaining round.
        if !alive.iter().any(|&i| !warriors[i].attacks.is_empty()) {
            return finish(warriors, &alive, Ending::Deadlock, rounds);
        }
        if rounds >= MAX_ROUNDS {
            return finish(warriors, &alive, Ending::Stalemate, rounds);
        }
        rounds += 1;

        for &i in &alive {
            warriors[i].initiative = rng.die(20) + warriors[i].dexterity_modifier();
        }
        // Stable, so equal initiative keeps roster order as Python's sort does.
        alive.sort_by(|&a, &b| warriors[b].initiative.cmp(&warriors[a].initiative));

        let order = alive.clone();
        for &attacker in &order {
            if warriors[attacker].hp <= 0 || warriors[attacker].attacks.is_empty() {
                continue;
            }
            let Some(target) = choose_target(attacker, &order, warriors, &party, rng) else {
                continue;
            };

            let roll = AttackRoll::roll(rng);
            let choice = rng.choice_index(warriors[attacker].attacks.len());

            let damage = {
                let attack = &warriors[attacker].attacks[choice];
                if roll.hits(attack.attack_bonus, warriors[target].ac) {
                    let mut dealt = roll_all(&attack.damage, rng) + attack.damage_bonus;
                    if roll.is_critical() {
                        dealt += roll_all(&attack.damage, rng);
                    }
                    dealt
                } else {
                    0
                }
            };
            warriors[target].hp -= damage;
        }

        alive.retain(|&i| warriors[i].hp > 0);
    }

    let ending =
        if alive.is_empty() { Ending::MutualDestruction } else { Ending::LastStanding };
    finish(warriors, &alive, ending, rounds)
}

fn finish(
    warriors: &[Warrior],
    alive: &[WarriorId],
    ending: Ending,
    rounds: u32,
) -> Outcome {
    // On a clean finish there is at most one survivor; when the fight was cut
    // short, the healthiest takes it, ties going to the earliest in the roster.
    let winner = alive
        .iter()
        .copied()
        .max_by_key(|&i| (warriors[i].hp, std::cmp::Reverse(i)));
    Outcome { winner, ending, rounds }
}

fn choose_target(
    attacker: WarriorId,
    order: &[WarriorId],
    warriors: &[Warrior],
    party: &[WarriorId],
    rng: &mut GameRng,
) -> Option<WarriorId> {
    let mut best: Option<(f64, WarriorId)> = None;
    for &candidate in order {
        if candidate == attacker || warriors[candidate].hp <= 0 {
            continue;
        }
        let score = target_score(&warriors[attacker].strategy, candidate, warriors, party, rng);
        // Strictly less, so the first warrior holding the minimum keeps it.
        if best.is_none_or(|(lowest, _)| score < lowest) {
            best = Some((score, candidate));
        }
    }
    best.map(|(_, id)| id)
}

fn target_score(
    strategy: &Strategy,
    candidate: WarriorId,
    warriors: &[Warrior],
    party: &[WarriorId],
    rng: &mut GameRng,
) -> f64 {
    /// What the reference charges for "don't hit this one". Deliberately a
    /// large finite number rather than infinity, so that if a party member is
    /// somehow the only thing left standing they still get attacked instead of
    /// the battle deadlocking.
    const AVOID: f64 = 999_999.0;

    let by_preference = |pref: PlayerStrategy, rng: &mut GameRng| -> f64 {
        match pref {
            PlayerStrategy::Random => rng.unit(),
            PlayerStrategy::Harsh => f64::from(warriors[candidate].hp),
            PlayerStrategy::Rage => f64::from(warriors[candidate].threat.eighths()),
            PlayerStrategy::Wise => f64::from(-warriors[candidate].threat.eighths()),
        }
    };

    match *strategy {
        Strategy::Solo(pref) => by_preference(pref, rng),
        Strategy::Party(pref) => {
            if warriors[candidate].is_player() {
                AVOID
            } else {
                by_preference(pref, rng)
            }
        }
        Strategy::HuntPlayer(target) => f64::from(u8::from(candidate != target)),
        Strategy::HuntAnyPlayer => {
            // Re-drawn for every candidate, exactly as the reference's
            // `int(m != choice(_warriors_))` does.
            if party.is_empty() {
                return 1.0;
            }
            let focus = *rng.choice(party);
            f64::from(u8::from(candidate != focus))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dice::Die;

    fn attack(bonus: i32, dice: Die, damage_bonus: i32) -> Attack {
        Attack { attack_bonus: bonus, damage: vec![dice], damage_bonus }
    }

    fn fighter(hp: i32, ac: i32, attacks: Vec<Attack>) -> Warrior {
        Warrior::new(hp, attacks, 10, ac, Cr::from_whole(1), Strategy::Solo(PlayerStrategy::Random))
    }

    #[test]
    fn a_lone_warrior_wins_without_fighting() {
        let mut rng = GameRng::seed_from_u64(1);
        let mut ws = vec![fighter(10, 10, vec![])];
        let outcome = battle(&mut ws, &mut rng);
        assert_eq!(outcome.winner, Some(0));
        assert_eq!(outcome.rounds, 0);
    }

    #[test]
    fn the_overwhelming_favourite_wins() {
        let mut rng = GameRng::seed_from_u64(2);
        let mut wins = 0;
        for _ in 0..200 {
            let mut ws = vec![
                fighter(200, 20, vec![attack(15, Die::new(6, 12), 20)]),
                fighter(4, 1, vec![attack(-5, Die::new(1, 2), 0)]),
            ];
            if battle(&mut ws, &mut rng).won_by(0) {
                wins += 1;
            }
        }
        assert!(wins > 190, "the strong warrior only won {wins}/200");
    }

    #[test]
    fn two_harmless_warriors_deadlock_instead_of_hanging() {
        let mut rng = GameRng::seed_from_u64(3);
        let mut ws = vec![fighter(10, 10, vec![]), fighter(10, 10, vec![])];
        let outcome = battle(&mut ws, &mut rng);
        assert_eq!(outcome.ending, Ending::Deadlock);
        assert_eq!(outcome.rounds, 0, "a deadlock is detected before any rounds burn");
        assert!(outcome.winner.is_some(), "someone still has to be awarded the field");
    }

    #[test]
    fn an_unwinnable_grind_stops_at_the_round_cap() {
        // Both sides deal zero damage but do have attacks, so the deadlock
        // check cannot see it coming and the round cap has to catch it.
        let mut rng = GameRng::seed_from_u64(4);
        let harmless = || Attack { attack_bonus: 99, damage: vec![], damage_bonus: 0 };
        let mut ws = vec![fighter(10, 1, vec![harmless()]), fighter(10, 1, vec![harmless()])];
        let outcome = battle(&mut ws, &mut rng);
        assert_eq!(outcome.ending, Ending::Stalemate);
        assert_eq!(outcome.rounds, MAX_ROUNDS);
        assert!(outcome.winner.is_some());
    }

    #[test]
    fn striking_first_decides_a_lethal_exchange() {
        // Both sides one-shot the other, so whoever wins initiative survives
        // untouched: the loser is already dead when their turn comes around.
        // This is why combat can never end in mutual destruction.
        let mut rng = GameRng::seed_from_u64(5);
        let lethal = || attack(99, Die::new(20, 20), 500);
        for _ in 0..100 {
            let mut ws = vec![fighter(1, 1, vec![lethal()]), fighter(1, 1, vec![lethal()])];
            let outcome = battle(&mut ws, &mut rng);
            assert_eq!(outcome.ending, Ending::LastStanding);
            assert_eq!(outcome.rounds, 1);
            assert_eq!(ws.iter().filter(|w| w.hp > 0).count(), 1);
        }
    }

    #[test]
    fn a_roster_with_nobody_standing_yields_no_winner() {
        // The reference's `return warriors[0]` raises IndexError here.
        let mut rng = GameRng::seed_from_u64(5);

        let outcome = battle(&mut [], &mut rng);
        assert_eq!(outcome.ending, Ending::MutualDestruction);
        assert_eq!(outcome.winner, None);

        let mut downed = vec![fighter(0, 10, vec![]), fighter(-3, 10, vec![])];
        let outcome = battle(&mut downed, &mut rng);
        assert_eq!(outcome.ending, Ending::MutualDestruction);
        assert_eq!(outcome.winner, None);
        assert!(!outcome.won_by(0) && !outcome.won_by(1));
    }

    #[test]
    fn a_natural_one_always_misses_and_a_natural_twenty_always_hits() {
        assert!(!AttackRoll::Fumble.hits(1000, 0));
        assert!(AttackRoll::Critical.hits(-1000, 100));
        assert!(AttackRoll::Critical.is_critical());
        assert!(!AttackRoll::Normal(19).hits(0, 20));
        assert!(AttackRoll::Normal(19).hits(1, 20));
    }

    #[test]
    fn harsh_targets_the_weakest_and_wise_the_most_dangerous() {
        let mut rng = GameRng::seed_from_u64(6);
        let mut ws = vec![
            fighter(50, 10, vec![attack(5, Die::new(1, 6), 0)]),
            Warrior::new(40, vec![], 10, 10, Cr::from_whole(2), Strategy::Solo(PlayerStrategy::Random)),
            Warrior::new(10, vec![], 10, 10, Cr::from_whole(9), Strategy::Solo(PlayerStrategy::Random)),
        ];
        let order: Vec<_> = (0..ws.len()).collect();
        let party = vec![];

        ws[0].strategy = Strategy::Solo(PlayerStrategy::Harsh);
        assert_eq!(choose_target(0, &order, &ws, &party, &mut rng), Some(2), "harsh picks 10 HP");

        ws[0].strategy = Strategy::Solo(PlayerStrategy::Wise);
        assert_eq!(choose_target(0, &order, &ws, &party, &mut rng), Some(2), "wise picks CR 9");

        ws[0].strategy = Strategy::Solo(PlayerStrategy::Rage);
        assert_eq!(choose_target(0, &order, &ws, &party, &mut rng), Some(1), "rage picks CR 2");
    }

    #[test]
    fn party_members_never_turn_on_each_other() {
        let mut rng = GameRng::seed_from_u64(7);
        let mut ws = vec![
            fighter(50, 10, vec![attack(5, Die::new(1, 6), 0)]).owned_by("Ann"),
            fighter(50, 10, vec![]).owned_by("Bob"),
            Warrior::new(50, vec![], 10, 10, Cr::from_whole(3), Strategy::HuntAnyPlayer),
        ];
        ws[0].strategy = Strategy::Party(PlayerStrategy::Harsh);
        let order: Vec<_> = (0..ws.len()).collect();
        let party = vec![0, 1];
        // Bob is just as wounded, but only the monster is a legal target.
        assert_eq!(choose_target(0, &order, &ws, &party, &mut rng), Some(2));
    }

    #[test]
    fn a_monster_on_a_solo_quest_fixates_on_its_adventurer() {
        let mut rng = GameRng::seed_from_u64(8);
        let ws = vec![
            fighter(50, 10, vec![]).owned_by("Ann"),
            Warrior::new(50, vec![], 10, 10, Cr::from_whole(3), Strategy::HuntPlayer(0)),
            Warrior::new(50, vec![], 10, 10, Cr::from_whole(3), Strategy::HuntPlayer(0)),
        ];
        let order: Vec<_> = (0..ws.len()).collect();
        assert_eq!(choose_target(1, &order, &ws, &[0], &mut rng), Some(0));
    }

    #[test]
    fn party_hunters_spread_their_attention_across_the_group() {
        let mut rng = GameRng::seed_from_u64(9);
        let ws = vec![
            fighter(50, 10, vec![]).owned_by("Ann"),
            fighter(50, 10, vec![]).owned_by("Bob"),
            Warrior::new(50, vec![], 10, 10, Cr::from_whole(3), Strategy::HuntAnyPlayer),
        ];
        let order: Vec<_> = (0..ws.len()).collect();
        let party = vec![0, 1];
        let mut hits = [0; 2];
        for _ in 0..400 {
            let t = choose_target(2, &order, &ws, &party, &mut rng).unwrap();
            hits[t] += 1;
        }
        assert!(hits[0] > 100 && hits[1] > 100, "aggro did not spread: {hits:?}");
    }

    #[test]
    fn the_dead_are_never_targeted() {
        let mut rng = GameRng::seed_from_u64(10);
        let mut ws = vec![
            fighter(50, 10, vec![attack(5, Die::new(1, 6), 0)]),
            fighter(1, 10, vec![]),
            fighter(40, 10, vec![]),
        ];
        ws[0].strategy = Strategy::Solo(PlayerStrategy::Harsh);
        ws[1].hp = 0;
        let order: Vec<_> = (0..ws.len()).collect();
        assert_eq!(choose_target(0, &order, &ws, &[], &mut rng), Some(2));
    }

    #[test]
    fn every_battle_terminates_across_many_seeds() {
        for seed in 0..300 {
            let mut rng = GameRng::seed_from_u64(seed);
            let mut ws: Vec<Warrior> = (0..6)
                .map(|k| {
                    fighter(
                        10 + (seed as i32 % 40),
                        8 + k,
                        vec![attack(k, Die::new(1, 4), 0)],
                    )
                })
                .collect();
            let outcome = battle(&mut ws, &mut rng);
            assert!(outcome.rounds <= MAX_ROUNDS, "seed {seed} ran {} rounds", outcome.rounds);
            let survivors = ws.iter().filter(|w| w.hp > 0).count();
            assert!(survivors <= 1, "seed {seed} left {survivors} standing");
        }
    }

    #[test]
    fn monsters_roll_their_hit_dice_for_hp() {
        let mut rng = GameRng::seed_from_u64(11);
        let aboleth = assets::monsters().iter().position(|m| m.name == "Aboleth").unwrap() as MonsterId;
        for _ in 0..200 {
            let w = Warrior::monster(aboleth, Strategy::HuntAnyPlayer, &mut rng);
            assert!((18..=180).contains(&w.hp), "18d10 gave {}", w.hp);
            assert_eq!(w.ac, 17);
            assert_eq!(w.threat, Cr::from_whole(10));
            assert_eq!(w.attacks.len(), 2, "only damaging actions become attacks");
            assert!(!w.is_player());
        }
    }

    #[test]
    fn strategy_names_round_trip_case_insensitively() {
        for s in [
            PlayerStrategy::Random,
            PlayerStrategy::Harsh,
            PlayerStrategy::Rage,
            PlayerStrategy::Wise,
        ] {
            assert_eq!(PlayerStrategy::parse(s.name()), Some(s));
        }
        assert_eq!(PlayerStrategy::parse("  WISE "), Some(PlayerStrategy::Wise));
        assert_eq!(PlayerStrategy::parse("reckless"), None);
    }
}
