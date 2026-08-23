//! Dice notation, pre-parsed at asset-generation time.
//!
//! The reference parses `"18d10"` and `"4d6 + 5d8"` at import time into
//! closures; here the split has already happened in `tools/gen-assets`, so a
//! roll is just a walk over `(count, faces)` pairs.

use serde::{Deserialize, Serialize};

use crate::rng::GameRng;

/// `count`d`faces`, e.g. `2d6`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(from = "[u32; 2]", into = "[u32; 2]")]
pub struct Die {
    pub count: u32,
    pub faces: u32,
}

impl Die {
    pub const fn new(count: u32, faces: u32) -> Self {
        Self { count, faces }
    }

    pub fn roll(self, rng: &mut GameRng) -> i32 {
        (0..self.count).map(|_| rng.die(self.faces)).sum()
    }

    /// Highest possible result, used to bound worst-case battle damage.
    pub const fn max(self) -> i32 {
        (self.count * self.faces) as i32
    }
}

impl From<[u32; 2]> for Die {
    fn from([count, faces]: [u32; 2]) -> Self {
        Self { count, faces }
    }
}

impl From<Die> for [u32; 2] {
    fn from(d: Die) -> Self {
        [d.count, d.faces]
    }
}

impl std::fmt::Display for Die {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}d{}", self.count, self.faces)
    }
}

/// A sum of dice terms: `4d6 + 5d8` is two terms.
///
/// Weapons contribute exactly one term, monster attacks up to two, and a
/// weaponless player contributes none (which rolls 0, as in the reference).
pub type DiceSum = Vec<Die>;

pub fn roll_all(dice: &[Die], rng: &mut GameRng) -> i32 {
    dice.iter().map(|d| d.roll(rng)).sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_roll_stays_within_its_bounds() {
        let mut rng = GameRng::seed_from_u64(1);
        let d = Die::new(3, 6);
        for _ in 0..5_000 {
            let v = d.roll(&mut rng);
            assert!((3..=18).contains(&v), "3d6 rolled {v}");
        }
    }

    #[test]
    fn multi_term_damage_sums_every_term() {
        let mut rng = GameRng::seed_from_u64(2);
        let dice = vec![Die::new(4, 6), Die::new(5, 8)];
        for _ in 0..5_000 {
            let v = roll_all(&dice, &mut rng);
            assert!((9..=64).contains(&v), "4d6 + 5d8 rolled {v}");
        }
    }

    #[test]
    fn no_dice_rolls_zero() {
        // A player with no weapon has an empty damage expression.
        let mut rng = GameRng::seed_from_u64(3);
        assert_eq!(roll_all(&[], &mut rng), 0);
    }

    #[test]
    fn averages_land_near_the_expected_value() {
        let mut rng = GameRng::seed_from_u64(4);
        let d = Die::new(1, 20);
        let total: i32 = (0..20_000).map(|_| d.roll(&mut rng)).sum();
        let mean = f64::from(total) / 20_000.0;
        assert!((10.0..11.0).contains(&mean), "d20 mean {mean}");
    }

    #[test]
    fn serde_round_trips_through_the_asset_representation() {
        let d: Die = serde_json::from_str("[18,10]").unwrap();
        assert_eq!(d, Die::new(18, 10));
        assert_eq!(serde_json::to_string(&d).unwrap(), "[18,10]");
    }
}
