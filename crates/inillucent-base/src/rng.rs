//! A small deterministic pseudo-random generator.
//!
//! Invariant: the same seed always produces the same sequence, on every
//! platform and every build profile.
//!
//! The assurance model records a seed with every failure and expects the run to
//! be reproducible from it. `rand` cannot promise that across versions, and a
//! test generator that silently changes its stream turns a recorded seed into a
//! lie, so inillucent owns this one. It is `xoshiro256**`, chosen because it is
//! small enough to audit in one screen and has no failing BigCrush tests.
//!
//! This is emphatically not a cryptographic generator and never seeds a
//! database's `randomness()`; the VFS gets that from the operating system.

/// A deterministic `xoshiro256**` generator.
#[derive(Clone, Debug)]
pub struct Rng {
    state: [u64; 4],
}

impl Rng {
    /// Creates a generator from a 64-bit seed.
    ///
    /// The seed is expanded through `splitmix64` so that adjacent seeds - 1, 2,
    /// 3 - produce unrelated streams instead of correlated ones.
    pub fn new(seed: u64) -> Rng {
        let mut expander = seed;
        let mut state = [0u64; 4];
        for slot in state.iter_mut() {
            *slot = split_mix_64(&mut expander);
        }
        Rng { state }
    }

    /// Returns the next 64-bit value.
    pub fn next_u64(&mut self) -> u64 {
        let [s0, s1, s2, s3] = self.state;
        let result = s1.wrapping_mul(5).rotate_left(7).wrapping_mul(9);
        let shifted = s1 << 17;
        let n2 = s2 ^ s0;
        let n3 = s3 ^ s1;
        self.state = [s0 ^ n3, s1 ^ n2, n2 ^ shifted, n3.rotate_left(45)];
        result
    }

    /// Returns the next 32-bit value.
    pub fn next_u32(&mut self) -> u32 {
        (self.next_u64() >> 32) as u32
    }

    /// Returns a value in `0..bound`, or zero when `bound` is zero.
    ///
    /// Uses Lemire's multiply-shift reduction, which is unbiased enough for
    /// test-case selection and never divides.
    pub fn below(&mut self, bound: u64) -> u64 {
        if bound == 0 {
            return 0;
        }
        let product = u128::from(self.next_u64()).wrapping_mul(u128::from(bound));
        (product >> 64) as u64
    }

    /// Returns true with probability `numerator / denominator`.
    pub fn chance(&mut self, numerator: u64, denominator: u64) -> bool {
        denominator != 0 && self.below(denominator) < numerator
    }

    /// Fills a buffer with pseudo-random bytes.
    pub fn fill(&mut self, output: &mut [u8]) {
        for chunk in output.chunks_mut(8) {
            let word = self.next_u64().to_le_bytes();
            for (slot, byte) in chunk.iter_mut().zip(word.iter()) {
                *slot = *byte;
            }
        }
    }

    /// Picks one element of a slice, or `None` when the slice is empty.
    pub fn pick<'a, T>(&mut self, items: &'a [T]) -> Option<&'a T> {
        items.get(self.below(items.len() as u64) as usize)
    }
}

/// Advances a `splitmix64` state and returns the mixed output.
fn split_mix_64(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9e37_79b9_7f4a_7c15);
    let mut value = *state;
    value = (value ^ (value >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    value = (value ^ (value >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    value ^ (value >> 31)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A recorded seed is only useful if it replays exactly.
    #[test]
    fn the_same_seed_replays_the_same_stream() {
        let first: Vec<u64> = (0..64)
            .scan(Rng::new(1782), |rng, _| Some(rng.next_u64()))
            .collect();
        let second: Vec<u64> = (0..64)
            .scan(Rng::new(1782), |rng, _| Some(rng.next_u64()))
            .collect();
        assert_eq!(first, second);
    }

    /// Adjacent seeds must not produce correlated streams, or a sweep over
    /// seeds 1..n would be testing nearly the same schedule every time.
    #[test]
    fn adjacent_seeds_produce_unrelated_streams() {
        let a = Rng::new(1).next_u64();
        let b = Rng::new(2).next_u64();
        assert_ne!(a, b);
        assert!(
            a ^ b != 1,
            "streams differ by more than the seed difference"
        );
    }

    /// Bounded selection stays in range, including at the degenerate bounds.
    #[test]
    fn bounded_selection_stays_in_range() {
        let mut rng = Rng::new(7);
        assert_eq!(rng.below(0), 0);
        assert_eq!(rng.below(1), 0);
        for _ in 0..10_000 {
            assert!(rng.below(97) < 97);
        }
    }

    /// The generator must actually cover its range rather than sticking near a
    /// single value, which is the failure mode a broken mixer produces.
    ///
    /// The draw count is load-bearing here in a way it is not in the other
    /// seeded loops: the bounds below are calibrated for a hundred and sixty
    /// thousand draws across sixteen buckets, so sampling it does not check the
    /// property less thoroughly, it checks a different property and fails. It
    /// is therefore skipped under Miri rather than sampled - what it tests is
    /// the mixer's distribution, which an interpreter has nothing to say about.
    #[cfg_attr(miri, ignore)]
    #[test]
    fn output_covers_its_range() {
        let mut rng = Rng::new(0xdead_beef);
        let mut buckets = [0u32; 16];
        for _ in 0..160_000 {
            let index = rng.below(16) as usize;
            buckets[index] += 1;
        }
        for count in buckets {
            assert!((7_000..13_000).contains(&count), "uneven bucket {count}");
        }
    }
}
