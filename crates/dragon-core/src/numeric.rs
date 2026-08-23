//! The two arithmetic primitives the tuning formulas are built on.

use std::fmt;

use serde::{Deserialize, Serialize};

/// How many bits it takes to write `n`, so `bit_length(0) == 0` and
/// `bit_length(10) == 4`.
///
/// The game leans on this as a cheap logarithm: it sets how many fights it
/// takes to master a weapon, how much a warrior's gear is worth in combat, and
/// how steeply XP and coin payouts are deflated. Subtracting the leading zeros
/// handles zero without a special case, where `ilog2` would panic.
pub fn bit_length(n: u64) -> u32 {
    u64::BITS - n.leading_zeros()
}

/// The D&D ability modifier: `floor((score - 10) / 2)`.
///
/// Flooring is the rule, not an artifact of the port. The published table runs
/// 1 → −5, 2-3 → −4, … 8-9 → −1, 10-11 → 0, and only flooring reproduces it;
/// truncating toward zero would misprice every odd score below 10. That is not
/// hypothetical — 36 of the 325 monsters have an ability score under 10, down
/// to dexterity 1, and they would each gain a point of initiative and of
/// armour class. `div_euclid` is Rust's own floor division.
pub fn ability_modifier(score: i32) -> i32 {
    (score - 10).div_euclid(2)
}

/// A challenge rating, stored in eighths.
///
/// Every CR in the monster table is a multiple of 1/8 (0, 1/8, 1/4, 1/2, then
/// whole numbers), so eighths keep grouping, summing and comparison exact
/// without floating point creeping into game logic.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Cr(pub i32);

impl Cr {
    pub const ZERO: Cr = Cr(0);
    pub const ONE: Cr = Cr(8);

    pub const fn from_whole(cr: i32) -> Cr {
        Cr(cr * 8)
    }

    pub const fn eighths(self) -> i32 {
        self.0
    }

    /// The whole-number part, discarding any fraction. CR is never negative,
    /// so plain division is the same as flooring here.
    pub const fn truncated(self) -> i32 {
        self.0 / 8
    }

    /// How a single monster's CR is labelled: `1/8`, `1/4`, `1/2`, otherwise
    /// the plain integer.
    pub fn label(self) -> String {
        match self.0 {
            1 => "1/8".into(),
            2 => "1/4".into(),
            4 => "1/2".into(),
            _ => self.to_string(),
        }
    }
}

impl std::ops::Add for Cr {
    type Output = Cr;
    fn add(self, rhs: Cr) -> Cr {
        Cr(self.0 + rhs.0)
    }
}

impl std::ops::AddAssign for Cr {
    fn add_assign(&mut self, rhs: Cr) {
        self.0 += rhs.0;
    }
}

impl std::ops::Mul<i32> for Cr {
    type Output = Cr;
    fn mul(self, rhs: i32) -> Cr {
        Cr(self.0 * rhs)
    }
}

impl std::iter::Sum for Cr {
    fn sum<I: Iterator<Item = Cr>>(iter: I) -> Cr {
        Cr(iter.map(|c| c.0).sum())
    }
}

impl fmt::Display for Cr {
    /// Whole numbers render without a decimal point, eighths as an exact short
    /// decimal: `0.125`, `1.5`, `65`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let whole = self.0.div_euclid(8);
        let frac = self.0.rem_euclid(8);
        if frac == 0 {
            return write!(f, "{whole}");
        }
        // n/8 is exactly n * 125 thousandths, so this never rounds.
        let thousandths = format!("{:03}", frac * 125);
        write!(f, "{whole}.{}", thousandths.trim_end_matches('0'))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bit_length_counts_significant_bits() {
        for (n, expected) in [(0, 0), (1, 1), (2, 2), (3, 2), (10, 4), (18, 5), (5000, 13)] {
            assert_eq!(bit_length(n), expected, "bit_length({n})");
        }
    }

    #[test]
    fn bit_length_handles_zero_and_the_extremes() {
        // Zero is reachable through the payout formulas, and would panic under
        // `ilog2`.
        assert_eq!(bit_length(0), 0);
        assert_eq!(bit_length(u64::MAX), 64);
        // Each power of two needs exactly one more bit than the value below it.
        for exponent in 0..63 {
            let n = 1u64 << exponent;
            assert_eq!(bit_length(n), exponent + 1);
            assert_eq!(bit_length(n - 1), exponent);
        }
    }

    #[test]
    fn weapon_and_weight_bit_lengths_land_in_the_tuned_ranges() {
        // The proficiency formula assumes cost in 10..=5000 and weight in 1..=18,
        // giving required_count = (cost.bit_length() * weight.bit_length()) >> 1.
        assert_eq!(bit_length(10), 4);
        assert_eq!(bit_length(5000), 13);
        assert_eq!(bit_length(1), 1);
        assert_eq!(bit_length(18), 5);
        assert_eq!((bit_length(10) * bit_length(1)) >> 1, 2);
        assert_eq!((bit_length(5000) * bit_length(18)) >> 1, 32);
    }

    #[test]
    fn ability_modifier_matches_the_published_table() {
        // The full run of the D&D table across the range the game uses. The
        // scores below 10 are the ones flooring gets right and truncating
        // toward zero would not.
        let table = [
            (1, -5), (2, -4), (3, -4), (4, -3), (5, -3), (6, -2), (7, -2),
            (8, -1), (9, -1), (10, 0), (11, 0), (12, 1), (13, 1), (14, 2),
            (15, 2), (20, 5), (21, 5), (30, 10),
        ];
        for (score, expected) in table {
            assert_eq!(ability_modifier(score), expected, "score {score}");
        }
    }

    #[test]
    fn ability_modifier_never_truncates_toward_zero() {
        // Guards the whole reason this is `div_euclid` and not `/`: every odd
        // score below 10 differs between the two, and 36 monsters sit in that
        // range.
        for score in [1, 3, 5, 7, 9] {
            let floored = ability_modifier(score);
            let truncated = (score - 10) / 2;
            assert_ne!(floored, truncated, "score {score}");
            assert_eq!(floored, truncated - 1);
        }
    }

    #[test]
    fn cr_renders_eighths_as_exact_decimals() {
        assert_eq!(Cr(0).to_string(), "0");
        assert_eq!(Cr(1).to_string(), "0.125");
        assert_eq!(Cr(2).to_string(), "0.25");
        assert_eq!(Cr(3).to_string(), "0.375");
        assert_eq!(Cr(4).to_string(), "0.5");
        assert_eq!(Cr(8).to_string(), "1");
        assert_eq!(Cr(12).to_string(), "1.5");
        assert_eq!(Cr(520).to_string(), "65");
    }

    #[test]
    fn cr_labels_fractional_ratings_as_fractions() {
        assert_eq!(Cr(1).label(), "1/8");
        assert_eq!(Cr(2).label(), "1/4");
        assert_eq!(Cr(4).label(), "1/2");
        assert_eq!(Cr(8).label(), "1");
        assert_eq!(Cr(240).label(), "30");
    }

    #[test]
    fn cr_truncation_discards_the_fraction() {
        assert_eq!(Cr(1).truncated(), 0);
        assert_eq!(Cr(7).truncated(), 0);
        assert_eq!(Cr(8).truncated(), 1);
        assert_eq!(Cr(23).truncated(), 2);
    }
}
