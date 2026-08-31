//! Fusing the vector list and the lexical list into one ranking.
//!
//! Reciprocal Rank Fusion with `k = 60` and a cap of two chunks per document is
//! the default, identical to the baseline, so the first comparison isolates
//! retrieval accuracy from ranking policy.
//!
//! Reciprocal Rank Fusion discards score magnitude and keeps only position, which
//! makes it robust to two scoring scales that are not comparable, but it cannot
//! tell a hit that is nearly identical to the query from one that merely came
//! first in a weak list. Normalized score fusion keeps the magnitude. Which one
//! wins is an empirical question per scenario, so both are implemented and the
//! score card reports both.

use std::collections::HashMap;

use crate::bm25::LexicalHit;
use crate::flat::Neighbour;
use crate::store::Store;

pub const RRF_K: f32 = 60.0;
pub const PER_DOC_CAP: usize = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HitOrigin {
    Vector,
    Lexical,
    Both,
}

#[derive(Debug, Clone, Copy)]
pub struct FusedHit {
    pub chunk: u32,
    pub score: f32,
    pub origin: HitOrigin,
}

#[derive(Debug, Clone, Copy)]
pub enum Fusion {
    /// Rank based. `k` damps the influence of the very top positions.
    ReciprocalRank { k: f32 },
    /// Score based. Each list is scaled onto `[0, 1]` by its own minimum and
    /// maximum, then combined as `weight * vector + (1 - weight) * lexical`.
    NormalizedScore { vector_weight: f32 },
    /// Score based, scaled by each list's maximum rather than by its range.
    ///
    /// The difference from `NormalizedScore` is what happens to a weak list.
    /// Min-max maps every list onto the full `[0, 1]`, so the best hit of a
    /// hopeless lexical list scores exactly as high as the best hit of a perfect
    /// one. Dividing by the maximum keeps zero meaning zero, so a lexical list
    /// whose scores are all small stays small next to the vector list and
    /// contributes proportionally to how good it actually was. Bruch et al.
    /// (TOIS 2023) measure this family, convex combination, above Reciprocal
    /// Rank Fusion in and out of domain, with one parameter to tune.
    Convex { vector_weight: f32 },
}

impl Default for Fusion {
    fn default() -> Self {
        Fusion::ReciprocalRank { k: RRF_K }
    }
}

/// Scale to `[0, 1]` by the maximum, keeping zero at zero.
///
/// Unlike `min_max` this is not invariant to how good the list is: a list whose
/// best score is a tenth of another run's best still maps its best to 1.0, but
/// the *spacing* below it is preserved, so a hit half as good as the leader
/// scores 0.5 rather than being stretched to fill the range. Negative values are
/// floored at zero: a cosine similarity below zero means the vector points away
/// from the query, which is not a partial match.
fn max_scale(values: &[f32]) -> Vec<f32> {
    let max = values.iter().cloned().fold(f32::MIN, f32::max);
    if !(max > f32::EPSILON) {
        return vec![0.0; values.len()];
    }
    values.iter().map(|v| (v / max).clamp(0.0, 1.0)).collect()
}

/// Scale to `[0, 1]` by minimum and maximum. A list whose scores are all equal
/// maps to 1.0 throughout rather than dividing by zero: every entry really is
/// equally good, so flattening them is the honest answer.
fn min_max(values: &[f32]) -> Vec<f32> {
    if values.is_empty() {
        return Vec::new();
    }
    let min = values.iter().cloned().fold(f32::MAX, f32::min);
    let max = values.iter().cloned().fold(f32::MIN, f32::max);
    let span = max - min;
    if span <= f32::EPSILON {
        return vec![1.0; values.len()];
    }
    values.iter().map(|v| (v - min) / span).collect()
}

/// Fuse the two lists, cap chunks per document, and truncate to `top_k`.
///
/// The per document cap stops one long page from filling the result list. It is
/// applied after fusion and before truncation, exactly as the baseline does,
/// so a document's third best chunk is dropped rather than displacing another
/// document's best.
pub fn fuse(
    vector_hits: &[Neighbour],
    lexical_hits: &[LexicalHit],
    store: &Store,
    fusion: Fusion,
    top_k: usize,
    per_doc_cap: usize,
) -> Vec<FusedHit> {
    let mut scores: HashMap<u32, (f32, HitOrigin)> = HashMap::new();

    match fusion {
        Fusion::ReciprocalRank { k } => {
            for (rank, h) in vector_hits.iter().enumerate() {
                let c = 1.0 / (k + rank as f32 + 1.0);
                scores.insert(h.chunk, (c, HitOrigin::Vector));
            }
            for (rank, h) in lexical_hits.iter().enumerate() {
                let c = 1.0 / (k + rank as f32 + 1.0);
                scores
                    .entry(h.chunk)
                    .and_modify(|e| {
                        e.0 += c;
                        e.1 = HitOrigin::Both;
                    })
                    .or_insert((c, HitOrigin::Lexical));
            }
        }
        Fusion::NormalizedScore { vector_weight } => {
            // Vector hits carry distance, where smaller is better, so similarity
            // is the value to scale.
            let v_sim: Vec<f32> = vector_hits.iter().map(|h| 1.0 - h.distance).collect();
            let v_scaled = min_max(&v_sim);
            let l_raw: Vec<f32> = lexical_hits.iter().map(|h| h.score).collect();
            let l_scaled = min_max(&l_raw);

            for (h, s) in vector_hits.iter().zip(&v_scaled) {
                scores.insert(h.chunk, (vector_weight * s, HitOrigin::Vector));
            }
            for (h, s) in lexical_hits.iter().zip(&l_scaled) {
                let c = (1.0 - vector_weight) * s;
                scores
                    .entry(h.chunk)
                    .and_modify(|e| {
                        e.0 += c;
                        e.1 = HitOrigin::Both;
                    })
                    .or_insert((c, HitOrigin::Lexical));
            }
        }
        Fusion::Convex { vector_weight } => {
            let v_sim: Vec<f32> = vector_hits.iter().map(|h| 1.0 - h.distance).collect();
            let v_scaled = max_scale(&v_sim);
            let l_raw: Vec<f32> = lexical_hits.iter().map(|h| h.score).collect();
            let l_scaled = max_scale(&l_raw);

            for (h, s) in vector_hits.iter().zip(&v_scaled) {
                scores.insert(h.chunk, (vector_weight * s, HitOrigin::Vector));
            }
            for (h, s) in lexical_hits.iter().zip(&l_scaled) {
                let c = (1.0 - vector_weight) * s;
                scores
                    .entry(h.chunk)
                    .and_modify(|e| {
                        e.0 += c;
                        e.1 = HitOrigin::Both;
                    })
                    .or_insert((c, HitOrigin::Lexical));
            }
        }
    }

    let mut all: Vec<FusedHit> = scores
        .into_iter()
        .map(|(chunk, (score, origin))| FusedHit { chunk, score, origin })
        .collect();
    all.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.chunk.cmp(&b.chunk))
    });

    let mut per_doc: HashMap<u32, usize> = HashMap::new();
    let mut out = Vec::with_capacity(top_k);
    for hit in all {
        let doc = store.chunks[hit.chunk as usize].doc;
        let used = per_doc.entry(doc).or_insert(0);
        if *used >= per_doc_cap {
            continue;
        }
        *used += 1;
        out.push(hit);
        if out.len() >= top_k {
            break;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::ChunkInput;

    /// `chunks_per_doc` chunks for each of `n_docs` documents.
    fn store_of(n_docs: usize, chunks_per_doc: usize) -> Store {
        let mut s = Store::default();
        let mut inputs = Vec::new();
        for d in 0..n_docs {
            for c in 0..chunks_per_doc {
                inputs.push(ChunkInput {
                    source: "confluence".into(),
                    external_doc_id: format!("d{d}"),
                    chunk_index: c as u32,
                    heading_path: vec![],
                    content: format!("doc {d} chunk {c}"),
                    title: format!("t{d}"),
                    url: format!("u{d}"),
                    space_key: None,
                    author: None,
                    author_id: None,
                    updated_at: None,
                    labels: vec![],
                    deleted: false,
                });
            }
        }
        s.add_chunks(inputs);
        s
    }

    fn v(chunks: &[u32]) -> Vec<Neighbour> {
        chunks
            .iter()
            .enumerate()
            .map(|(i, c)| Neighbour { chunk: *c, distance: i as f32 * 0.01 })
            .collect()
    }

    fn l(chunks: &[u32]) -> Vec<LexicalHit> {
        chunks
            .iter()
            .enumerate()
            .map(|(i, c)| LexicalHit { chunk: *c, score: 10.0 - i as f32 })
            .collect()
    }

    #[test]
    fn a_chunk_in_both_lists_outranks_one_in_a_single_list() {
        let s = store_of(10, 1);
        let fused = fuse(&v(&[0, 1, 2]), &l(&[2, 3, 4]), &s, Fusion::default(), 10, 99);
        assert_eq!(fused[0].chunk, 2, "the chunk both lists agree on should lead");
        assert_eq!(fused[0].origin, HitOrigin::Both);
    }

    #[test]
    fn reciprocal_rank_fusion_matches_the_baseline_formula() {
        let s = store_of(10, 1);
        let fused = fuse(&v(&[0]), &l(&[1]), &s, Fusion::ReciprocalRank { k: 60.0 }, 10, 99);
        let expected = 1.0 / 61.0;
        for h in &fused {
            assert!((h.score - expected).abs() < 1e-6, "got {}", h.score);
        }
    }

    #[test]
    fn the_per_document_cap_limits_chunks_from_one_document() {
        // 3 documents, 5 chunks each. Every chunk of document 0 ranks top.
        let s = store_of(3, 5);
        let all: Vec<u32> = (0..15).collect();
        let fused = fuse(&v(&all), &[], &s, Fusion::default(), 10, PER_DOC_CAP);
        let doc0 = fused
            .iter()
            .filter(|h| s.chunks[h.chunk as usize].doc == 0)
            .count();
        assert_eq!(doc0, PER_DOC_CAP);
    }

    #[test]
    fn truncates_to_top_k() {
        let s = store_of(20, 1);
        let all: Vec<u32> = (0..20).collect();
        assert_eq!(fuse(&v(&all), &[], &s, Fusion::default(), 5, 99).len(), 5);
    }

    #[test]
    fn one_empty_list_still_produces_a_ranking() {
        let s = store_of(10, 1);
        assert_eq!(fuse(&v(&[0, 1]), &[], &s, Fusion::default(), 10, 99).len(), 2);
        assert_eq!(fuse(&[], &l(&[3, 4]), &s, Fusion::default(), 10, 99).len(), 2);
    }

    #[test]
    fn two_empty_lists_produce_nothing() {
        let s = store_of(10, 1);
        assert!(fuse(&[], &[], &s, Fusion::default(), 10, 99).is_empty());
    }

    #[test]
    fn normalized_score_fusion_respects_the_weight() {
        let s = store_of(10, 1);
        // Vector likes chunk 0, lexical likes chunk 5. Weighting the vector side
        // fully should put chunk 0 first, and vice versa.
        let all_vector = fuse(
            &v(&[0, 1, 2]),
            &l(&[5, 6, 7]),
            &s,
            Fusion::NormalizedScore { vector_weight: 1.0 },
            10,
            99,
        );
        assert_eq!(all_vector[0].chunk, 0);
        let all_lexical = fuse(
            &v(&[0, 1, 2]),
            &l(&[5, 6, 7]),
            &s,
            Fusion::NormalizedScore { vector_weight: 0.0 },
            10,
            99,
        );
        assert_eq!(all_lexical[0].chunk, 5);
    }

    #[test]
    fn normalized_score_fusion_survives_a_flat_list() {
        let s = store_of(10, 1);
        // Every distance identical, so the span is zero.
        let flat: Vec<Neighbour> = (0..3).map(|c| Neighbour { chunk: c, distance: 0.5 }).collect();
        let fused = fuse(&flat, &[], &s, Fusion::NormalizedScore { vector_weight: 1.0 }, 10, 99);
        assert_eq!(fused.len(), 3);
        assert!(fused.iter().all(|h| h.score.is_finite()));
    }

    #[test]
    fn fusion_is_deterministic() {
        let s = store_of(10, 2);
        let a = fuse(&v(&[0, 1, 2, 3]), &l(&[3, 2, 1, 0]), &s, Fusion::default(), 10, 2);
        let b = fuse(&v(&[0, 1, 2, 3]), &l(&[3, 2, 1, 0]), &s, Fusion::default(), 10, 2);
        assert_eq!(
            a.iter().map(|h| h.chunk).collect::<Vec<_>>(),
            b.iter().map(|h| h.chunk).collect::<Vec<_>>()
        );
    }

    #[test]
    fn origin_is_reported_per_hit() {
        let s = store_of(10, 1);
        let fused = fuse(&v(&[0]), &l(&[1]), &s, Fusion::default(), 10, 99);
        let origins: Vec<HitOrigin> = fused.iter().map(|h| h.origin).collect();
        assert!(origins.contains(&HitOrigin::Vector));
        assert!(origins.contains(&HitOrigin::Lexical));
    }

    /// The two score based fusions differ in what they preserve. Min-max stretches
    /// each list onto the whole of `[0, 1]`, so three cosine similarities a hundredth
    /// apart come out as 1.0, 0.5 and 0.0 — a rounding error becomes the whole range.
    /// Scaling by the maximum keeps the spacing the scores actually had.
    #[test]
    fn convex_fusion_preserves_score_spacing_where_min_max_stretches_it() {
        let s = store_of(10, 1);
        // Similarities 1.00, 0.99, 0.98: three nearly equally good hits.
        let vector = v(&[0, 1, 2]);
        let convex = fuse(&vector, &[], &s, Fusion::Convex { vector_weight: 1.0 }, 10, 99);
        let min_max = fuse(&vector, &[], &s, Fusion::NormalizedScore { vector_weight: 1.0 }, 10, 99);
        let score_of = |hits: &[FusedHit], chunk: u32| hits.iter().find(|h| h.chunk == chunk).unwrap().score;

        assert!((score_of(&convex, 1) - 0.99).abs() < 1e-3, "convex keeps the middle hit near its own value");
        assert!((score_of(&min_max, 1) - 0.5).abs() < 1e-3, "min-max pushes it to the middle of the range");
        // Both agree on the order; only the distances between them change.
        assert_eq!(
            convex.iter().map(|h| h.chunk).collect::<Vec<_>>(),
            min_max.iter().map(|h| h.chunk).collect::<Vec<_>>()
        );
    }

    /// Every fusion has to survive a list that is entirely zero, which is what a
    /// lexical search matching nothing above the noise floor produces.
    #[test]
    fn convex_fusion_survives_an_all_zero_list() {
        let s = store_of(10, 1);
        let zeros = vec![LexicalHit { chunk: 3, score: 0.0 }, LexicalHit { chunk: 4, score: 0.0 }];
        let fused = fuse(&v(&[0, 1]), &zeros, &s, Fusion::Convex { vector_weight: 0.5 }, 10, 99);
        assert_eq!(fused.len(), 4);
        assert!(fused.iter().all(|h| h.score.is_finite()));
    }
}
