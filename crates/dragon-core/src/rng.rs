//! The game's random stream.
//!
//! This is deliberately hand-rolled rather than pulled from `rand`. The seed is
//! persisted inside [`crate::GameState`], so an in-flight game must keep
//! producing the same stream across dependency upgrades; owning the algorithm
//! is the only way to guarantee that.
//!
//! xoshiro256++ (Blackman & Vigna, public domain), seeded through SplitMix64.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GameRng {
    s: [u64; 4],
}

impl GameRng {
    pub fn seed_from_u64(seed: u64) -> Self {
        // SplitMix64 is the reference way to expand a single u64 into
        // xoshiro's 256-bit state without leaving it sparse.
        let mut z = seed;
        let mut next = || {
            z = z.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut x = z;
            x = (x ^ (x >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            x = (x ^ (x >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            x ^ (x >> 31)
        };
        Self { s: [next(), next(), next(), next()] }
    }

    pub fn next_u64(&mut self) -> u64 {
        let result = self.s[0]
            .wrapping_add(self.s[3])
            .rotate_left(23)
            .wrapping_add(self.s[0]);
        let t = self.s[1] << 17;
        self.s[2] ^= self.s[0];
        self.s[3] ^= self.s[1];
        self.s[1] ^= self.s[2];
        self.s[0] ^= self.s[3];
        self.s[2] ^= t;
        self.s[3] = self.s[3].rotate_left(45);
        result
    }

    /// Uniform in `0..n`. Panics if `n == 0`.
    ///
    /// Lemire's multiply-shift with rejection: unbiased, and normally exits
    /// without a second draw.
    pub fn below(&mut self, n: u64) -> u64 {
        assert!(n > 0, "below(0) has no valid output");
        let mut m = (self.next_u64() as u128).wrapping_mul(n as u128);
        let mut low = m as u64;
        if low < n {
            let threshold = n.wrapping_neg() % n;
            while low < threshold {
                m = (self.next_u64() as u128).wrapping_mul(n as u128);
                low = m as u64;
            }
        }
        (m >> 64) as u64
    }

    /// Python's `randrange(lo, hi)`: uniform over `lo..hi`, upper bound excluded.
    pub fn range(&mut self, lo: i64, hi: i64) -> i64 {
        assert!(hi > lo, "empty range {lo}..{hi}");
        lo + self.below((hi - lo) as u64) as i64
    }

    /// Roll one `faces`-sided die: `1..=faces`, i.e. Python's `randrange(1, faces + 1)`.
    pub fn die(&mut self, faces: u32) -> i32 {
        1 + self.below(faces as u64) as i32
    }

    /// Uniform in `[0, 1)`, matching Python's `random()` resolution (53 bits).
    pub fn unit(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 * (1.0 / (1u64 << 53) as f64)
    }

    pub fn choice<'a, T>(&mut self, items: &'a [T]) -> &'a T {
        &items[self.below(items.len() as u64) as usize]
    }

    pub fn choice_index(&mut self, len: usize) -> usize {
        self.below(len as u64) as usize
    }

    /// Python's `random.sample`: `k` distinct elements, in random order.
    /// Returns indices so callers can keep borrowing the source collection.
    pub fn sample_indices(&mut self, len: usize, k: usize) -> Vec<usize> {
        assert!(k <= len, "cannot sample {k} of {len}");
        let mut pool: Vec<usize> = (0..len).collect();
        for i in 0..k {
            let j = i + self.choice_index(len - i);
            pool.swap(i, j);
        }
        pool.truncate(k);
        pool
    }

    pub fn shuffle<T>(&mut self, items: &mut [T]) {
        for i in (1..items.len()).rev() {
            items.swap(i, self.choice_index(i + 1));
        }
    }

    /// Fork a fresh seed, e.g. to re-seed persisted state after a tick.
    pub fn next_seed(&mut self) -> u64 {
        self.next_u64()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_reproducible_from_a_seed() {
        let a: Vec<u64> = (0..8).map(|_| GameRng::seed_from_u64(42).next_u64()).collect();
        assert!(a.iter().all(|&x| x == a[0]), "same seed must replay identically");

        let mut r1 = GameRng::seed_from_u64(7);
        let mut r2 = GameRng::seed_from_u64(7);
        for _ in 0..1000 {
            assert_eq!(r1.next_u64(), r2.next_u64());
        }
    }

    #[test]
    fn distinct_seeds_diverge() {
        let mut a = GameRng::seed_from_u64(1);
        let mut b = GameRng::seed_from_u64(2);
        assert_ne!(a.next_u64(), b.next_u64());
    }

    #[test]
    fn below_stays_in_bounds_and_covers_the_range() {
        let mut rng = GameRng::seed_from_u64(99);
        let mut seen = [0usize; 6];
        for _ in 0..6000 {
            let v = rng.below(6) as usize;
            seen[v] += 1;
        }
        assert!(seen.iter().all(|&c| c > 800), "d6 badly skewed: {seen:?}");
    }

    #[test]
    fn die_never_rolls_zero() {
        let mut rng = GameRng::seed_from_u64(5);
        for _ in 0..10_000 {
            let v = rng.die(20);
            assert!((1..=20).contains(&v), "d20 rolled {v}");
        }
    }

    #[test]
    fn range_matches_python_randrange_bounds() {
        let mut rng = GameRng::seed_from_u64(3);
        for _ in 0..10_000 {
            let v = rng.range(1, 21);
            assert!((1..21).contains(&v));
        }
        // Negative lower bounds are used for nothing today but must not panic.
        assert!((-5..5).contains(&rng.range(-5, 5)));
    }

    #[test]
    fn unit_is_within_zero_and_one() {
        let mut rng = GameRng::seed_from_u64(11);
        let mut sum = 0.0;
        for _ in 0..10_000 {
            let v = rng.unit();
            assert!((0.0..1.0).contains(&v));
            sum += v;
        }
        let mean = sum / 10_000.0;
        assert!((0.45..0.55).contains(&mean), "mean {mean} looks wrong");
    }

    #[test]
    fn sample_returns_distinct_indices() {
        let mut rng = GameRng::seed_from_u64(13);
        for _ in 0..200 {
            let picked = rng.sample_indices(10, 4);
            assert_eq!(picked.len(), 4);
            let mut sorted = picked.clone();
            sorted.sort_unstable();
            sorted.dedup();
            assert_eq!(sorted.len(), 4, "sample repeated an index: {picked:?}");
            assert!(picked.iter().all(|&i| i < 10));
        }
    }

    #[test]
    fn sample_of_everything_is_a_permutation() {
        let mut rng = GameRng::seed_from_u64(17);
        let mut picked = rng.sample_indices(6, 6);
        picked.sort_unstable();
        assert_eq!(picked, vec![0, 1, 2, 3, 4, 5]);
    }
}
