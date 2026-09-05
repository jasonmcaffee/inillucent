//! The measures the score card reports, each defined once so every scenario
//! computes it the same way.

use std::collections::{HashMap, HashSet};

/// Fraction of the reference answer set that the engine also returned.
///
/// The denominator is the size of the reference, not `k`, so a query whose
/// filter admits fewer than `k` chunks is not penalised for returning what
/// exists.
pub fn recall_at_k(returned: &[u32], reference: &[u32], k: usize) -> f32 {
    if reference.is_empty() {
        return 1.0;
    }
    let want: HashSet<u32> = reference.iter().take(k).copied().collect();
    if want.is_empty() {
        return 1.0;
    }
    let got = returned
        .iter()
        .take(k)
        .filter(|c| want.contains(c))
        .count();
    got as f32 / want.len() as f32
}

/// 1.0 when any correct answer appears in the top k, else 0.0.
pub fn success_at_k(returned: &[u32], correct: &HashSet<u32>, k: usize) -> f32 {
    if returned.iter().take(k).any(|c| correct.contains(c)) {
        1.0
    } else {
        0.0
    }
}

/// Reciprocal of the rank of the first correct answer, zero when none appears.
pub fn reciprocal_rank(returned: &[u32], correct: &HashSet<u32>) -> f32 {
    for (i, c) in returned.iter().enumerate() {
        if correct.contains(c) {
            return 1.0 / (i + 1) as f32;
        }
    }
    0.0
}

/// Normalized discounted cumulative gain at k with binary relevance.
///
/// `attainable` caps how many correct answers the ideal ranking is allowed to
/// contain. It exists because both engines deliberately cap chunks per document,
/// so a query whose correct set is the twelve chunks of one document can never
/// have more than two of them returned. Dividing by a twelve hit ideal would
/// report a score no correctly working engine could reach, and would keep
/// reporting it however good the ranking was.
pub fn ndcg_at_k_attainable(
    returned: &[u32],
    correct: &HashSet<u32>,
    k: usize,
    attainable: usize,
) -> f32 {
    let mut dcg = 0.0f64;
    for (i, c) in returned.iter().take(k).enumerate() {
        if correct.contains(c) {
            dcg += 1.0 / ((i + 2) as f64).log2();
        }
    }
    let ideal_hits = correct.len().min(k).min(attainable.max(1));
    if correct.is_empty() {
        return 1.0;
    }
    let mut ideal = 0.0f64;
    for i in 0..ideal_hits {
        ideal += 1.0 / ((i + 2) as f64).log2();
    }
    if ideal == 0.0 {
        return 0.0;
    }
    ((dcg / ideal) as f32).min(1.0)
}

/// nDCG with no cap on the ideal ranking. Kept because it is the textbook
/// definition and the tests use it to show what the cap changes, but no scenario
/// uses it: every ground truth in this suite is a whole document, and every
/// engine caps chunks per document.
#[cfg_attr(not(test), allow(dead_code))]
pub fn ndcg_at_k(returned: &[u32], correct: &HashSet<u32>, k: usize) -> f32 {
    ndcg_at_k_attainable(returned, correct, k, usize::MAX)
}

/// Share of the first `k` results that is relevant.
///
/// The metric success@k cannot report. A list whose first hit answers the
/// question and whose next nine are topical noise scores a perfect success@10,
/// and for an agent that has to read all ten it is nine tenths waste. Precision
/// is what makes that visible.
pub fn precision_at_k(returned: &[u32], correct: &HashSet<u32>, k: usize) -> f32 {
    if k == 0 {
        return 0.0;
    }
    let considered = returned.len().min(k);
    if considered == 0 {
        return 0.0;
    }
    let hits = returned.iter().take(k).filter(|c| correct.contains(c)).count();
    // The denominator is what was returned rather than `k`, so a query whose
    // filter admits only three chunks is not scored as 70% wrong for returning
    // the three that exist.
    hits as f32 / considered as f32
}

/// Normalized discounted cumulative gain at `k` over graded relevance.
///
/// The binary version above asks whether a result is right. This one asks how
/// right, which is the distinction the whole graded-qrel exercise exists to make:
/// a passage that directly answers the question and a passage from the same page
/// that merely shares its topic are both "correct" under a binary judgement, and
/// only one of them is evidence an agent can answer from.
///
/// Gain is `2^grade - 1`, the standard exponential form, so the distance between
/// grade 2 and grade 3 is larger than between 0 and 1. Discount is `1/log2(rank +
/// 1)`, so a grade 3 at rank one is worth much more than the same passage at rank
/// ten.
///
/// `attainable` caps how many graded items the ideal ranking may contain, for the
/// same reason the binary version caps it: both engines limit chunks per
/// document, so an ideal built from twelve chunks of one document would describe
/// a ranking no correctly working engine is allowed to produce.
/// @param returned - the ranking, best first
/// @param grades - relevance grade per chunk; anything absent is grade 0
/// @param k - the cutoff
/// @param attainable - most graded items the ideal ranking may use
pub fn ndcg_graded_at_k(
    returned: &[u32],
    grades: &HashMap<u32, u8>,
    k: usize,
    attainable: usize,
) -> f32 {
    let gain = |g: u8| ((1u32 << g.min(16)) - 1) as f64;
    let discount = |i: usize| 1.0 / ((i + 2) as f64).log2();

    let mut dcg = 0.0f64;
    for (i, c) in returned.iter().take(k).enumerate() {
        let g = grades.get(c).copied().unwrap_or(0);
        if g > 0 {
            dcg += gain(g) * discount(i);
        }
    }

    let mut ideal_grades: Vec<u8> = grades.values().copied().filter(|g| *g > 0).collect();
    if ideal_grades.is_empty() {
        // Nothing in the corpus is relevant, so there is no ranking to be graded
        // against. Negative queries are measured by their own family, not here.
        return 1.0;
    }
    ideal_grades.sort_unstable_by(|a, b| b.cmp(a));
    ideal_grades.truncate(k.min(attainable.max(1)));
    let ideal: f64 = ideal_grades.iter().enumerate().map(|(i, g)| gain(*g) * discount(i)).sum();
    if ideal == 0.0 {
        return 0.0;
    }
    ((dcg / ideal) as f32).min(1.0)
}

/// Share of the graded items at or above `floor` that appear in the first `k`.
///
/// Used for evidence recall, where the question is not "did anything relevant
/// appear" but "did everything the answer needs appear". A query whose answer
/// needs two passages from two different sources is only served when both are
/// there, and success@k reports it as fully served when one is.
/// @param returned - the ranking, best first
/// @param grades - relevance grade per chunk
/// @param k - the cutoff
/// @param floor - the lowest grade that counts as required evidence
pub fn graded_recall_at_k(
    returned: &[u32],
    grades: &HashMap<u32, u8>,
    k: usize,
    floor: u8,
) -> f32 {
    let required: HashSet<u32> =
        grades.iter().filter(|(_, g)| **g >= floor).map(|(c, _)| *c).collect();
    if required.is_empty() {
        return 1.0;
    }
    let found = returned.iter().take(k).filter(|c| required.contains(c)).count();
    found as f32 / required.len() as f32
}

/// Percentile of a set of durations in milliseconds, by the nearest rank
/// definition: the smallest observed value at or above which `p` of the samples
/// fall. `p` is in `[0, 1]`, and the input must already be sorted ascending.
///
/// Nearest rank rather than interpolation, because an interpolated latency is a
/// number no request actually took.
pub fn percentile(sorted_ms: &[f64], p: f64) -> f64 {
    if sorted_ms.is_empty() {
        return 0.0;
    }
    let rank = (p * sorted_ms.len() as f64).ceil() as usize;
    let idx = rank.saturating_sub(1).min(sorted_ms.len() - 1);
    sorted_ms[idx]
}

#[derive(Default, Clone)]
pub struct Accumulator {
    values: Vec<f32>,
}

impl Accumulator {
    pub fn push(&mut self, v: f32) {
        self.values.push(v);
    }

    pub fn mean(&self) -> f32 {
        if self.values.is_empty() {
            return 0.0;
        }
        self.values.iter().sum::<f32>() / self.values.len() as f32
    }

    /// Number of samples behind `mean`, so a scenario can report how much
    /// evidence a figure rests on.
    pub fn len(&self) -> usize {
        self.values.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(xs: &[u32]) -> HashSet<u32> {
        xs.iter().copied().collect()
    }

    #[test]
    fn perfect_recall_when_the_lists_agree() {
        assert_eq!(recall_at_k(&[1, 2, 3], &[1, 2, 3], 3), 1.0);
    }

    #[test]
    fn recall_counts_only_the_overlap_and_ignores_order() {
        assert_eq!(recall_at_k(&[3, 2, 9], &[1, 2, 3], 3), 2.0 / 3.0);
    }

    #[test]
    fn recall_of_an_empty_reference_is_perfect_not_zero() {
        // A filter that admits nothing must not be scored as a failure.
        assert_eq!(recall_at_k(&[], &[], 10), 1.0);
    }

    #[test]
    fn recall_denominator_is_the_reference_size_not_k() {
        // Only two chunks exist to be found, and both were found.
        assert_eq!(recall_at_k(&[1, 2], &[1, 2], 10), 1.0);
    }

    #[test]
    fn success_is_binary_and_respects_the_cutoff() {
        let correct = set(&[7]);
        assert_eq!(success_at_k(&[1, 2, 7], &correct, 3), 1.0);
        assert_eq!(success_at_k(&[1, 2, 7], &correct, 2), 0.0);
    }

    #[test]
    fn reciprocal_rank_rewards_an_earlier_answer() {
        let correct = set(&[5]);
        assert_eq!(reciprocal_rank(&[5, 1, 2], &correct), 1.0);
        assert_eq!(reciprocal_rank(&[1, 5, 2], &correct), 0.5);
        assert_eq!(reciprocal_rank(&[1, 2, 3], &correct), 0.0);
    }

    #[test]
    fn ndcg_is_one_for_the_ideal_ranking() {
        let correct = set(&[1, 2]);
        assert!((ndcg_at_k(&[1, 2, 3, 4], &correct, 4) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn ndcg_falls_when_correct_answers_rank_lower() {
        let correct = set(&[1, 2]);
        let good = ndcg_at_k(&[1, 2, 3, 4], &correct, 4);
        let worse = ndcg_at_k(&[3, 4, 1, 2], &correct, 4);
        assert!(worse < good);
        assert!(worse > 0.0);
    }

    #[test]
    fn a_capped_ideal_lets_a_correct_engine_reach_one() {
        // Twelve chunks are correct, but the per document cap allows only two.
        let correct = set(&[1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12]);
        let returned = vec![1, 2, 99, 98];
        let uncapped = ndcg_at_k(&returned, &correct, 10);
        let capped = ndcg_at_k_attainable(&returned, &correct, 10, 2);
        assert!(uncapped < 0.5, "uncapped nDCG should look bad: {uncapped}");
        assert!((capped - 1.0).abs() < 1e-6, "capped nDCG should be perfect: {capped}");
    }

    #[test]
    fn a_capped_ideal_still_punishes_a_bad_ranking() {
        let correct = set(&[1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12]);
        let good = ndcg_at_k_attainable(&[1, 2, 99, 98], &correct, 10, 2);
        let worse = ndcg_at_k_attainable(&[99, 98, 1, 2], &correct, 10, 2);
        assert!(worse < good);
        assert!(worse > 0.0);
    }

    #[test]
    fn ndcg_is_zero_when_nothing_correct_is_returned() {
        assert_eq!(ndcg_at_k(&[9, 8], &set(&[1]), 2), 0.0);
    }

    #[test]
    fn percentiles_pick_the_expected_element() {
        let v: Vec<f64> = (1..=100).map(|x| x as f64).collect();
        assert_eq!(percentile(&v, 0.5), 50.0);
        assert_eq!(percentile(&v, 0.95), 95.0);
        assert_eq!(percentile(&v, 0.0), 1.0);
        assert_eq!(percentile(&v, 1.0), 100.0);
    }

    #[test]
    fn an_empty_percentile_is_zero_rather_than_a_panic() {
        assert_eq!(percentile(&[], 0.5), 0.0);
    }

    #[test]
    fn accumulator_means_what_it_was_given() {
        let mut a = Accumulator::default();
        assert_eq!(a.mean(), 0.0);
        a.push(1.0);
        a.push(3.0);
        assert_eq!(a.mean(), 2.0);
        assert_eq!(a.len(), 2);
    }

    fn graded(pairs: &[(u32, u8)]) -> HashMap<u32, u8> {
        pairs.iter().copied().collect()
    }

    #[test]
    fn precision_counts_the_share_of_the_list_that_is_relevant() {
        let correct = set(&[1, 2]);
        assert_eq!(precision_at_k(&[1, 2, 9, 8], &correct, 4), 0.5);
        assert_eq!(precision_at_k(&[1, 2], &correct, 2), 1.0);
    }

    /// A short list is scored on what it returned rather than on k, or a query
    /// whose filter admits three chunks would be marked seventy per cent wrong
    /// for returning the three that exist.
    #[test]
    fn precision_does_not_punish_a_list_shorter_than_k() {
        assert_eq!(precision_at_k(&[1, 2], &set(&[1, 2]), 10), 1.0);
        assert_eq!(precision_at_k(&[], &set(&[1]), 10), 0.0);
    }

    /// The distinction the graded qrels exist to make. Two rankings that a binary
    /// judgement scores identically, because both put a "correct" chunk first,
    /// are separated once the grades say which chunk actually answers.
    #[test]
    fn graded_ndcg_separates_an_answer_from_a_merely_related_passage() {
        let g = graded(&[(1, 3), (2, 2), (3, 1)]);
        let answer_first = ndcg_graded_at_k(&[1, 2, 9], &g, 10, 2);
        let related_first = ndcg_graded_at_k(&[3, 1, 9], &g, 10, 2);
        assert!(answer_first > related_first, "{answer_first} vs {related_first}");

        // The binary metric cannot tell them apart: both put a correct chunk in
        // the list and both put one at rank one.
        let correct = set(&[1, 2, 3]);
        assert_eq!(success_at_k(&[1, 2, 9], &correct, 1), success_at_k(&[3, 1, 9], &correct, 1));
    }

    #[test]
    fn graded_ndcg_is_one_for_the_ideal_ranking_under_the_cap() {
        let g = graded(&[(1, 3), (2, 2), (3, 2), (4, 2)]);
        // The cap allows two, so the best reachable ranking is the 3 then a 2.
        assert!((ndcg_graded_at_k(&[1, 2, 9, 8], &g, 10, 2) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn graded_ndcg_falls_when_the_answer_ranks_lower() {
        let g = graded(&[(1, 3), (2, 2)]);
        let high = ndcg_graded_at_k(&[1, 2], &g, 10, 2);
        let low = ndcg_graded_at_k(&[9, 8, 7, 1, 2], &g, 10, 2);
        assert!(low < high);
        assert!(low > 0.0);
    }

    #[test]
    fn graded_ndcg_is_zero_when_nothing_graded_is_returned() {
        assert_eq!(ndcg_graded_at_k(&[9, 8], &graded(&[(1, 3)]), 10, 2), 0.0);
    }

    /// Evidence recall is the metric a multi-source question needs: finding one
    /// of the two required passages is half an answer, and success@k calls it a
    /// whole one.
    #[test]
    fn graded_recall_requires_every_piece_of_required_evidence() {
        let g = graded(&[(1, 3), (2, 3), (5, 1)]);
        assert_eq!(graded_recall_at_k(&[1, 9, 8], &g, 10, 3), 0.5);
        assert_eq!(graded_recall_at_k(&[1, 2, 9], &g, 10, 3), 1.0);
        assert_eq!(success_at_k(&[1, 9, 8], &set(&[1, 2]), 10), 1.0, "success calls half an answer complete");
    }

    #[test]
    fn graded_recall_ignores_grades_below_the_floor() {
        let g = graded(&[(1, 3), (5, 1)]);
        assert_eq!(graded_recall_at_k(&[1], &g, 10, 3), 1.0);
    }

}
