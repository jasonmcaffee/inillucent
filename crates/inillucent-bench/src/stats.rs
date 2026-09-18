//! Deciding whether a difference between two engines is evidence or noise.
//!
//! The score card used to call any difference above `1e-4` a win. That number was
//! asserted to sit below the sampling noise, but nothing computed the noise. On a
//! set of ninety queries one query changing its mind moves a mean by about
//! `0.011`, a hundred times the tolerance, so a run could report a win that a
//! different random seed would have reported as a loss.
//!
//! Both tests here are paired: each query contributes its score under both
//! engines, and only the per-query difference is used. That is the right shape
//! for this comparison because the two engines answer the *same* queries, and it
//! removes the variance that comes from some queries simply being harder than
//! others. Smucker, Allan and Carterette compared the available tests on TREC
//! data and found the paired bootstrap and the randomization test agree with each
//! other and are the appropriate choices for retrieval evaluation; both are
//! computed here rather than one, because they answer different questions.
//!
//! - The **bootstrap interval** answers "how large is the difference, and how
//!   precisely do we know it". That is what a practical threshold needs.
//! - The **randomization test** answers "could a difference this large have come
//!   from noise alone". That is what a claim of a difference needs.
//!
//! Neither is a substitute for the point estimate, which is always reported
//! alongside them.

/// A deterministic uniform generator.
///
/// Written here rather than taken from `rand` so a p-value is reproducible from
/// the seed recorded in the run manifest regardless of which version of the
/// dependency is compiled in. `xorshift64*` is more than adequate for resampling.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        // Zero is a fixed point of xorshift, so it is never the state.
        Rng(seed | 1)
    }

    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    /// A uniform integer in `[0, n)`.
    fn below(&mut self, n: usize) -> usize {
        (self.next_u64() % n as u64) as usize
    }

    fn coin(&mut self) -> bool {
        self.next_u64() & 1 == 0
    }
}

/// How a comparison between two engines came out, with the uncertainty attached.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct Paired {
    /// Mean of `candidate - baseline` over the queries. Positive means the
    /// candidate scored higher, whatever the metric's direction; the caller
    /// orients the inputs.
    pub delta: f64,
    /// The 95% bootstrap interval on `delta`.
    pub low: f64,
    pub high: f64,
    /// Two-sided p-value from the paired randomization test.
    pub p_value: f64,
    /// Queries behind the comparison.
    pub queries: usize,
    /// Queries where the two engines scored differently at all. A comparison
    /// where this is small is worth reading with suspicion however small the
    /// p-value: the whole result rests on a handful of queries.
    pub disagreements: usize,
}

/// The verdict a comparison earns, once its uncertainty and a practical
/// threshold have both been taken into account.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Verdict {
    /// The interval clears both zero and the practical threshold.
    Better,
    /// The interval is entirely inside the practical threshold in both
    /// directions: the two are the same for any purpose that matters.
    Equivalent,
    /// The interval crosses zero, or clears zero without clearing the threshold.
    /// Not a win and not a tie; the run simply cannot tell.
    Inconclusive,
    /// The interval clears zero and the threshold in the wrong direction.
    Worse,
}

impl Verdict {
    /// What the score card prints for this verdict.
    ///
    /// `Worse` is the one that is emphasised, because a reader skimming a
    /// table of verdicts is looking for the row that says the change lost.
    pub fn label(&self) -> &'static str {
        match self {
            Verdict::Better => "better",
            Verdict::Equivalent => "equivalent",
            Verdict::Inconclusive => "inconclusive",
            Verdict::Worse => "**worse**",
        }
    }
}

/// Bootstrap iterations. A thousand is enough for a 95% interval to be stable to
/// about the third decimal, which is finer than any threshold this card uses, and
/// it keeps a whole card's worth of comparisons under a second.
pub const ITERATIONS: usize = 2000;

/// Compare two per-query series that are already oriented so higher is better.
///
/// Returns `None` when there is nothing to compare: no queries, or series of
/// different lengths, which would mean the two engines were not asked the same
/// questions and no paired statistic is meaningful.
/// @param candidate - per-query scores for the engine being judged
/// @param baseline - per-query scores for the engine it is judged against
/// @param seed - fixes the resampling, so a p-value is reproducible
pub fn compare(candidate: &[f64], baseline: &[f64], seed: u64) -> Option<Paired> {
    if candidate.is_empty() || candidate.len() != baseline.len() {
        return None;
    }
    let diffs: Vec<f64> = candidate.iter().zip(baseline).map(|(a, b)| a - b).collect();
    let n = diffs.len();
    let delta = diffs.iter().sum::<f64>() / n as f64;
    let disagreements = diffs.iter().filter(|d| d.abs() > 1e-12).count();
    let (low, high) = bootstrap_interval(&diffs, seed);
    let p_value = randomization_p(&diffs, seed ^ 0x9E37_79B9_7F4A_7C15);
    Some(Paired {
        delta,
        low,
        high,
        p_value,
        queries: n,
        disagreements,
    })
}

/// The 95% percentile bootstrap interval on the mean of `diffs`.
///
/// Resamples queries with replacement, which is what makes the interval describe
/// uncertainty about the *query sample* rather than about the engines. A run on a
/// different ninety queries is the thing this is trying to anticipate.
fn bootstrap_interval(diffs: &[f64], seed: u64) -> (f64, f64) {
    let n = diffs.len();
    if n < 2 {
        let only = diffs.first().copied().unwrap_or(0.0);
        return (only, only);
    }
    let mut rng = Rng::new(seed);
    let mut means = Vec::with_capacity(ITERATIONS);
    for _ in 0..ITERATIONS {
        let mut total = 0.0;
        for _ in 0..n {
            total += diffs.get(rng.below(n)).copied().unwrap_or(0.0);
        }
        means.push(total / n as f64);
    }
    means.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    // Nearest rank on each tail, so the bounds are values the resampling actually
    // produced rather than an interpolation between two of them.
    let last = ITERATIONS.saturating_sub(1);
    let lo = means
        .get(((0.025 * ITERATIONS as f64) as usize).min(last))
        .copied()
        .unwrap_or(0.0);
    let hi = means
        .get(((0.975 * ITERATIONS as f64) as usize).min(last))
        .copied()
        .unwrap_or(0.0);
    (lo, hi)
}

/// Two-sided p-value from the paired randomization test.
///
/// Under the hypothesis that the two engines are the same, which of the pair a
/// query's two scores came from is arbitrary, so flipping the sign of a
/// difference produces an equally likely outcome. The p-value is how often a
/// random assignment of signs produces a mean at least as extreme as the observed
/// one. It assumes nothing about the shape of the distribution, which matters
/// because per-query retrieval scores are neither normal nor continuous:
/// success@1 takes two values and nDCG takes a few dozen.
fn randomization_p(diffs: &[f64], seed: u64) -> f64 {
    let n = diffs.len();
    if n == 0 {
        return 1.0;
    }
    let observed = (diffs.iter().sum::<f64>() / n as f64).abs();
    // Nothing moved at all: the two engines produced identical scores on every
    // query, which is certainty of no difference rather than evidence of one.
    if observed == 0.0 && diffs.iter().all(|d| *d == 0.0) {
        return 1.0;
    }
    let mut rng = Rng::new(seed);
    let mut extreme = 0usize;
    for _ in 0..ITERATIONS {
        let mut total = 0.0;
        for d in diffs {
            total += if rng.coin() { *d } else { -*d };
        }
        if (total / n as f64).abs() >= observed - 1e-12 {
            extreme += 1;
        }
    }
    // The observed assignment is itself one of the possible ones, so it is
    // counted in both numerator and denominator. Without that a p-value could be
    // reported as exactly zero, which no finite resampling can establish.
    (extreme + 1) as f64 / (ITERATIONS + 1) as f64
}

/// Turn a comparison into a verdict against a threshold declared before the run.
///
/// The threshold is the smallest difference worth acting on. Without one, a large
/// enough query set eventually makes every difference statistically detectable,
/// including differences far too small to change what anyone experiences.
/// @param paired - the comparison
/// @param threshold - the smallest difference that would matter, in metric units
pub fn verdict(paired: &Paired, threshold: f64) -> Verdict {
    let t = threshold.abs();
    if paired.low > t {
        Verdict::Better
    } else if paired.high < -t {
        Verdict::Worse
    } else if paired.low > -t && paired.high < t {
        Verdict::Equivalent
    } else {
        Verdict::Inconclusive
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identical_series_are_equivalent_with_an_interval_at_zero() {
        let a = vec![0.5; 40];
        let p = compare(&a, &a, 7).unwrap();
        assert_eq!(p.delta, 0.0);
        assert_eq!(p.low, 0.0);
        assert_eq!(p.high, 0.0);
        assert_eq!(p.disagreements, 0);
        assert_eq!(p.p_value, 1.0);
        assert_eq!(verdict(&p, 0.01), Verdict::Equivalent);
    }

    #[test]
    fn a_clearly_better_series_clears_zero_and_the_threshold() {
        // Every query improves by 0.1, which is far above any threshold used here.
        let baseline: Vec<f64> = (0..60).map(|i| (i % 5) as f64 / 10.0).collect();
        let candidate: Vec<f64> = baseline.iter().map(|v| v + 0.1).collect();
        let p = compare(&candidate, &baseline, 11).unwrap();
        assert!((p.delta - 0.1).abs() < 1e-9);
        assert!(p.low > 0.0, "interval {:?}", (p.low, p.high));
        assert!(p.p_value < 0.01, "p = {}", p.p_value);
        assert_eq!(verdict(&p, 0.01), Verdict::Better);
    }

    #[test]
    fn the_direction_is_reported_rather_than_hidden() {
        let baseline: Vec<f64> = (0..60).map(|i| (i % 5) as f64 / 10.0).collect();
        let candidate: Vec<f64> = baseline.iter().map(|v| v - 0.1).collect();
        let p = compare(&candidate, &baseline, 11).unwrap();
        assert_eq!(verdict(&p, 0.01), Verdict::Worse);
    }

    /// The behaviour the fixed tolerance got wrong. One query out of ninety
    /// changing its mind moves the mean by about 0.011, a hundred times the old
    /// `1e-4` tie tolerance, and used to be counted as a win.
    #[test]
    fn a_single_query_difference_in_ninety_is_not_a_win() {
        let baseline = vec![0.5; 90];
        let mut candidate = baseline.clone();
        candidate[0] = 1.5;
        let p = compare(&candidate, &baseline, 3).unwrap();
        assert!(
            p.delta > 1e-4,
            "the old tolerance would have called this a win"
        );
        assert_eq!(p.disagreements, 1);
        assert_ne!(verdict(&p, 0.01), Verdict::Better);
    }

    /// A difference that is real but tiny is statistically detectable on a large
    /// enough sample and still must not be called better.
    #[test]
    fn a_real_but_trivial_difference_is_equivalent_not_better() {
        let baseline = vec![0.5; 500];
        let candidate = vec![0.5005; 500];
        let p = compare(&candidate, &baseline, 5).unwrap();
        assert!(
            p.low > 0.0,
            "the difference is detectable: {:?}",
            (p.low, p.high)
        );
        assert_eq!(verdict(&p, 0.01), Verdict::Equivalent);
    }

    #[test]
    fn a_noisy_wash_is_inconclusive_rather_than_a_win() {
        // Half the queries improve by 0.4, half get worse by 0.4.
        let baseline = vec![0.5; 80];
        let candidate: Vec<f64> = (0..80)
            .map(|i| if i % 2 == 0 { 0.9 } else { 0.1 })
            .collect();
        let p = compare(&candidate, &baseline, 9).unwrap();
        assert!(
            p.low < 0.0 && p.high > 0.0,
            "interval {:?}",
            (p.low, p.high)
        );
        assert_eq!(verdict(&p, 0.01), Verdict::Inconclusive);
    }

    #[test]
    fn a_p_value_is_reproducible_from_its_seed() {
        let baseline: Vec<f64> = (0..50).map(|i| (i % 7) as f64 / 10.0).collect();
        let candidate: Vec<f64> = baseline.iter().map(|v| v + 0.02).collect();
        let a = compare(&candidate, &baseline, 42).unwrap();
        let b = compare(&candidate, &baseline, 42).unwrap();
        assert_eq!(a.p_value, b.p_value);
        assert_eq!((a.low, a.high), (b.low, b.high));
    }

    #[test]
    fn a_p_value_is_never_exactly_zero() {
        let baseline = vec![0.0; 200];
        let candidate = vec![1.0; 200];
        let p = compare(&candidate, &baseline, 1).unwrap();
        assert!(p.p_value > 0.0);
        assert!(p.p_value <= 2.0 / (ITERATIONS + 1) as f64);
    }

    #[test]
    fn mismatched_or_empty_series_produce_no_comparison() {
        assert!(compare(&[], &[], 1).is_none());
        assert!(compare(&[1.0, 2.0], &[1.0], 1).is_none());
    }
}
