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
    /// How good this hit is in absolute terms, in `[0, 1]`, independent of how
    /// good the rest of the candidate list was.
    ///
    /// Separate from `score` because the two answer different questions and the
    /// same number cannot answer both. `score` orders the list, and the fusion
    /// that orders it best reads its scale out of the list itself: per-list
    /// min-max maps the leader of every list to 1.0, which is exactly what makes
    /// it a good ranker and exactly what makes it useless as a confidence. So
    /// confidence is always computed the theoretical min-max way — each side
    /// divided by a bound the results had no say in — whatever fusion is ordering
    /// the list.
    ///
    /// This is what lets an engine be asked "is there an answer here at all",
    /// which is the question a query nothing in the corpus answers turns on, and
    /// it costs one extra multiply per candidate.
    pub confidence: f32,
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
    /// Score based, scaled by bounds that do not depend on the results.
    ///
    /// The flaw both `NormalizedScore` and `Convex` share is that their scale is
    /// read out of the very list being scaled, so the leader of a list is defined
    /// to be as good as a list can get. A query nothing in the corpus answers
    /// still produces a top hit at 1.0, and a fused score therefore says where a
    /// chunk stood among the candidates rather than how well it matched. Bruch,
    /// Gai and Ingber call the alternative theoretical min-max: replace the
    /// observed minimum and maximum with the range the scoring function itself
    /// can produce. Cosine similarity over normalized vectors lies in `[0, 1]`
    /// once a vector pointing away from the query is floored at zero, and BM25 is
    /// bounded below by zero and above by the query's own idf mass at saturation,
    /// which `Bm25Index::score_ceiling` computes without looking at any result.
    ///
    /// The consequence is that a fused score becomes comparable between queries,
    /// which is what makes an abstention threshold possible at all: under min-max
    /// there is no threshold to set, because every query's best hit is 1.0.
    TheoreticalMinMax { vector_weight: f32 },
}

/// The bounds `Fusion::TheoreticalMinMax` scales by, computed from the query
/// rather than from the results.
///
/// `lexical_ceiling` is the query's own BM25 saturation point. It is zero when no
/// query term is in the dictionary, which is the honest answer: there is no
/// lexical evidence to normalize, and every lexical score for that query is zero.
#[derive(Debug, Clone, Copy, Default)]
pub struct ScoreBounds {
    pub lexical_ceiling: f32,
}

/// Per-query evidence about which of the two retrievers is worth listening to,
/// gathered from the candidate lists that were going to be produced anyway.
///
/// The idea is DAT's: the right weight for a query reflects how well each
/// retriever actually did on that query, not an average over a benchmark. DAT
/// gets that signal by asking a language model to grade each side's top hit,
/// which puts a model in the online retrieval path. These are the same question
/// asked of numbers the search already has, so the whole calculation is a handful
/// of floating point operations on lists that are already in cache.
#[derive(Debug, Clone, Copy, Default)]
pub struct QuerySignals {
    /// Share of query terms that look like identifiers: a ticket key, a path, a
    /// symbol, a version string. Lexical retrieval is the only side that can match
    /// these exactly, and a dense vector of a rare literal is mostly noise.
    pub identifier_share: f32,
    /// Share of query terms the lexical dictionary has never seen. Nothing lexical
    /// can be found through them, so the vector side is all there is.
    pub out_of_vocabulary_share: f32,
    /// How much of the query's idf mass the best lexical hit holds. A chunk
    /// holding the whole question is strong evidence; a chunk holding its
    /// commonest word is not.
    pub lexical_coverage: f32,
    /// How far the best lexical hit stands above the rest of its list, in `[0, 1]`.
    pub lexical_separation: f32,
    /// The same for the vector list.
    pub vector_separation: f32,
}

/// How `QuerySignals` are turned into a vector weight.
///
/// Every gain is a setting so the whole rule can be swept, and every gain at zero
/// reproduces a fixed weight exactly. That property matters: the adaptive path has
/// to be able to collapse onto the shipped behaviour, or turning it on could not
/// be judged against turning it off.
#[derive(Debug, Clone, Copy)]
pub struct AdaptiveWeights {
    /// The weight a query with no distinguishing signal gets.
    pub base: f32,
    /// Added in proportion to the share of query terms nothing in the dictionary
    /// holds.
    pub out_of_vocabulary_gain: f32,
    /// Subtracted in proportion to the share of query terms that look like
    /// identifiers.
    pub identifier_gain: f32,
    /// Added in proportion to how much better the vector list separates its leader
    /// than the lexical list separates its own.
    pub separation_gain: f32,
    /// Subtracted in proportion to how much of the query the best lexical hit
    /// holds. A chunk containing the whole question needs no help from the vector
    /// side.
    pub coverage_gain: f32,
    /// The weight is clamped into this range, so no combination of signals can
    /// silence either retriever completely.
    pub floor: f32,
    pub ceiling: f32,
}

impl Default for AdaptiveWeights {
    fn default() -> Self {
        AdaptiveWeights {
            base: 0.35,
            out_of_vocabulary_gain: 0.0,
            identifier_gain: 0.0,
            separation_gain: 0.0,
            coverage_gain: 0.0,
            floor: 0.05,
            ceiling: 0.95,
        }
    }
}

impl AdaptiveWeights {
    /// The vector weight for one query.
    ///
    /// Linear and monotone in every signal on purpose. A learned function would
    /// fit this benchmark better and would be impossible to explain in a score
    /// card, and the point of the exercise is a default someone can defend.
    /// @param signals - what the two candidate lists said about this query
    pub fn weight_for(&self, signals: &QuerySignals) -> f32 {
        let w = self.base + self.out_of_vocabulary_gain * signals.out_of_vocabulary_share
            - self.identifier_gain * signals.identifier_share
            + self.separation_gain * (signals.vector_separation - signals.lexical_separation)
            - self.coverage_gain * signals.lexical_coverage;
        let lo = self.floor.min(self.ceiling);
        let hi = self.ceiling.max(self.floor);
        w.clamp(lo, hi)
    }

    /// Whether this rule is the identity, in which case the adaptive path can be
    /// skipped and the configured fixed weight used directly.
    pub fn is_fixed(&self) -> bool {
        self.out_of_vocabulary_gain == 0.0
            && self.identifier_gain == 0.0
            && self.separation_gain == 0.0
            && self.coverage_gain == 0.0
    }
}

/// How far the leader of a scored list stands above the rest of it, in `[0, 1]`.
///
/// `(first - mean of the rest) / first`. Zero when every candidate scores the
/// same, which is a list that has told the ranker nothing, and approaching one
/// when the leader is in a class of its own. Scale free, so the lexical list and
/// the vector list produce comparable numbers despite one being a BM25 sum and the
/// other a cosine.
/// @param scores - the list's scores in descending order, higher is better
pub fn separation(scores: &[f32]) -> f32 {
    if scores.len() < 2 {
        return 0.0;
    }
    let first = scores[0];
    if !first.is_finite() || first <= f32::EPSILON {
        return 0.0;
    }
    let rest: f32 = scores[1..].iter().sum::<f32>() / (scores.len() - 1) as f32;
    ((first - rest) / first).clamp(0.0, 1.0)
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
    if !max.is_finite() || max <= f32::EPSILON {
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

/// Everything about combining the two lists that is a setting.
#[derive(Debug, Clone, Copy)]
pub struct FusionParams {
    pub fusion: Fusion,
    pub top_k: usize,
    pub per_doc_cap: usize,
    /// Bounds for `Fusion::TheoreticalMinMax`, ignored by the other methods.
    pub bounds: ScoreBounds,
    /// Maximal Marginal Relevance: `1.0` selects purely by score, lower values
    /// trade score for novelty against what has already been selected. Below 1 a
    /// vector set has to be supplied, because novelty is measured as cosine
    /// distance between the candidates themselves.
    pub mmr_lambda: f32,
}

impl Default for FusionParams {
    fn default() -> Self {
        FusionParams {
            fusion: Fusion::default(),
            top_k: 10,
            per_doc_cap: PER_DOC_CAP,
            bounds: ScoreBounds::default(),
            mmr_lambda: 1.0,
        }
    }
}

/// Fuse the two lists, cap chunks per document, and truncate to `top_k`.
///
/// The per document cap stops one long page from filling the result list. It is
/// applied after fusion and before truncation, exactly as the baseline does,
/// so a document's third best chunk is dropped rather than displacing another
/// document's best.
///
/// `vectors` is needed only when `mmr_lambda` is below 1, where selection has to
/// know how similar a candidate is to what has already been chosen. Passing
/// `None` with a lambda below 1 falls back to selecting by score, because a
/// diversity policy that cannot measure redundancy is not a diversity policy.
/// @param vector_hits - the vector side, ascending distance
/// @param lexical_hits - the lexical side, descending score
/// @param store - chunk metadata, read for the per document cap
/// @param vectors - the stored vectors, for the novelty term
/// @param params - fusion method, bounds, cap, diversity and cutoff
pub fn fuse(
    vector_hits: &[Neighbour],
    lexical_hits: &[LexicalHit],
    store: &Store,
    vectors: Option<&crate::vectors::VectorSet>,
    params: FusionParams,
) -> Vec<FusedHit> {
    let FusionParams { fusion, top_k, per_doc_cap, bounds, mmr_lambda } = params;
    let mut scores: HashMap<u32, (f32, HitOrigin)> = HashMap::new();
    // Confidence, on absolute bounds, whatever fusion is about to order the list.
    let confidence = absolute_confidence(vector_hits, lexical_hits, weight_of(fusion), bounds);

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
        Fusion::TheoreticalMinMax { vector_weight } => {
            // Both sides are divided by a bound the results had no say in, so a
            // list of poor candidates stays poor instead of being stretched onto
            // the whole range. The vector bound is 1: cosine over normalized
            // vectors cannot exceed it, and a negative similarity is floored at
            // zero because a vector pointing away from the query is not a partial
            // match. The lexical bound is the query's own idf mass at saturation.
            for h in vector_hits {
                let s = (1.0 - h.distance).clamp(0.0, 1.0);
                scores.insert(h.chunk, (vector_weight * s, HitOrigin::Vector));
            }
            let ceiling = bounds.lexical_ceiling;
            for h in lexical_hits {
                let s = if ceiling > f32::EPSILON {
                    (h.score / ceiling).clamp(0.0, 1.0)
                } else {
                    0.0
                };
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
        .map(|(chunk, (score, origin))| FusedHit {
            chunk,
            score,
            origin,
            confidence: confidence.get(&chunk).copied().unwrap_or(0.0),
        })
        .collect();
    all.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.chunk.cmp(&b.chunk))
    });

    match vectors {
        Some(v) if mmr_lambda < 1.0 => select_diverse(&all, store, v, top_k, per_doc_cap, mmr_lambda),
        _ => select_by_score(&all, store, top_k, per_doc_cap),
    }
}

/// The weight a fusion puts on the vector side, or the midpoint for a rank based
/// fusion, which has no weight but still needs confidence computed.
fn weight_of(fusion: Fusion) -> f32 {
    match fusion {
        Fusion::ReciprocalRank { .. } => 0.5,
        Fusion::NormalizedScore { vector_weight }
        | Fusion::Convex { vector_weight }
        | Fusion::TheoreticalMinMax { vector_weight } => vector_weight,
    }
}

/// Every candidate's absolute score, on bounds the candidate list had no say in.
///
/// The vector side is a cosine over normalized vectors, bounded by one, with a
/// negative similarity floored at zero because a vector pointing away from the
/// query is not a partial match. The lexical side is divided by the query's own
/// BM25 saturation point, which `Bm25Index::score_ceiling` computes from the
/// query alone. A query holding no term the dictionary knows has a ceiling of
/// zero and therefore no lexical confidence, which is the honest answer.
/// @param vector_hits - the vector side, ascending distance
/// @param lexical_hits - the lexical side, descending score
/// @param vector_weight - how the two are balanced
/// @param bounds - the query's own ceiling
fn absolute_confidence(
    vector_hits: &[Neighbour],
    lexical_hits: &[LexicalHit],
    vector_weight: f32,
    bounds: ScoreBounds,
) -> HashMap<u32, f32> {
    let mut out: HashMap<u32, f32> = HashMap::with_capacity(vector_hits.len() + lexical_hits.len());
    for h in vector_hits {
        let s = (1.0 - h.distance).clamp(0.0, 1.0);
        *out.entry(h.chunk).or_insert(0.0) += vector_weight * s;
    }
    let ceiling = bounds.lexical_ceiling;
    for h in lexical_hits {
        let s = if ceiling > f32::EPSILON { (h.score / ceiling).clamp(0.0, 1.0) } else { 0.0 };
        *out.entry(h.chunk).or_insert(0.0) += (1.0 - vector_weight) * s;
    }
    out
}

/// Take the highest scoring candidates in order, skipping any document that has
/// already contributed `per_doc_cap` chunks.
fn select_by_score(
    all: &[FusedHit],
    store: &Store,
    top_k: usize,
    per_doc_cap: usize,
) -> Vec<FusedHit> {
    let mut per_doc: HashMap<u32, usize> = HashMap::new();
    let mut out = Vec::with_capacity(top_k);
    for hit in all {
        let doc = store.chunks[hit.chunk as usize].doc;
        let used = per_doc.entry(doc).or_insert(0);
        if *used >= per_doc_cap {
            continue;
        }
        *used += 1;
        out.push(*hit);
        if out.len() >= top_k {
            break;
        }
    }
    out
}

/// Maximal Marginal Relevance selection: at each step take the candidate
/// maximising `lambda * score - (1 - lambda) * similarity to what is already
/// selected`.
///
/// The per document cap already removes the crudest redundancy, one page filling
/// the list, but it cannot see that two different pages are saying the same
/// thing. An agent given ten near-identical passages has ten slots' worth of
/// context and one passage's worth of evidence, and Carbonell and Goldstein's
/// formulation is the standard way to spend those slots on complementary
/// material instead.
///
/// The novelty term is cosine similarity between stored vectors, which is already
/// what the index compares, so no new representation is needed.
/// @param all - the fused candidates, descending score
/// @param store - chunk metadata, read for the per document cap
/// @param vectors - the stored vectors
/// @param top_k - how many to select
/// @param per_doc_cap - most chunks kept from any one document
/// @param lambda - 1 is pure score, 0 is pure novelty
fn select_diverse(
    all: &[FusedHit],
    store: &Store,
    vectors: &crate::vectors::VectorSet,
    top_k: usize,
    per_doc_cap: usize,
    lambda: f32,
) -> Vec<FusedHit> {
    let lambda = lambda.clamp(0.0, 1.0);
    let mut per_doc: HashMap<u32, usize> = HashMap::new();
    let mut out: Vec<FusedHit> = Vec::with_capacity(top_k);
    let mut taken = vec![false; all.len()];
    // Highest similarity between each candidate and anything already selected,
    // carried forward so each step compares against one new selection rather than
    // against the whole output.
    let mut redundancy = vec![0.0f32; all.len()];

    while out.len() < top_k {
        let mut best: Option<(usize, f32)> = None;
        for (i, hit) in all.iter().enumerate() {
            if taken[i] {
                continue;
            }
            let doc = store.chunks[hit.chunk as usize].doc;
            if per_doc.get(&doc).copied().unwrap_or(0) >= per_doc_cap {
                continue;
            }
            let value = lambda * hit.score - (1.0 - lambda) * redundancy[i];
            // Ties break on the candidate's own order, which is descending score
            // then ascending chunk, so selection stays deterministic.
            if best.map(|(_, v)| value > v).unwrap_or(true) {
                best = Some((i, value));
            }
        }
        let Some((chosen, _)) = best else { break };
        taken[chosen] = true;
        let hit = all[chosen];
        *per_doc.entry(store.chunks[hit.chunk as usize].doc).or_insert(0) += 1;
        out.push(hit);
        for (i, other) in all.iter().enumerate() {
            if taken[i] {
                continue;
            }
            // Cosine similarity from the cosine distance the vector set computes.
            let sim = 1.0 - vectors.distance_between(hit.chunk, other.chunk);
            redundancy[i] = redundancy[i].max(sim.clamp(0.0, 1.0));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The shape the fusion tests were written against, before diversity and
    /// theoretical bounds gave `fuse` two more things to be told. Keeps each test
    /// about the one property it is checking.
    fn fuse6(
        vector_hits: &[Neighbour],
        lexical_hits: &[LexicalHit],
        store: &Store,
        fusion: Fusion,
        top_k: usize,
        per_doc_cap: usize,
    ) -> Vec<FusedHit> {
        fuse(
            vector_hits,
            lexical_hits,
            store,
            None,
            FusionParams { fusion, top_k, per_doc_cap, ..Default::default() },
        )
    }

    /// A lexical hit with the coverage fields the ranking tests do not exercise.
    fn lex(chunk: u32, score: f32) -> LexicalHit {
        LexicalHit { chunk, score, coverage: 1.0, matched_terms: 1 }
    }
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
                    external_chunk_id: None,
                    labels: vec![],
                    attributes: Vec::new(),
                    flags: Vec::new(),
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
            .map(|(i, c)| lex(*c, 10.0 - i as f32))
            .collect()
    }

    #[test]
    fn a_chunk_in_both_lists_outranks_one_in_a_single_list() {
        let s = store_of(10, 1);
        let fused = fuse6(&v(&[0, 1, 2]), &l(&[2, 3, 4]), &s, Fusion::default(), 10, 99);
        assert_eq!(fused[0].chunk, 2, "the chunk both lists agree on should lead");
        assert_eq!(fused[0].origin, HitOrigin::Both);
    }

    #[test]
    fn reciprocal_rank_fusion_matches_the_baseline_formula() {
        let s = store_of(10, 1);
        let fused = fuse6(&v(&[0]), &l(&[1]), &s, Fusion::ReciprocalRank { k: 60.0 }, 10, 99);
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
        let fused = fuse6(&v(&all), &[], &s, Fusion::default(), 10, PER_DOC_CAP);
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
        assert_eq!(fuse6(&v(&all), &[], &s, Fusion::default(), 5, 99).len(), 5);
    }

    #[test]
    fn one_empty_list_still_produces_a_ranking() {
        let s = store_of(10, 1);
        assert_eq!(fuse6(&v(&[0, 1]), &[], &s, Fusion::default(), 10, 99).len(), 2);
        assert_eq!(fuse6(&[], &l(&[3, 4]), &s, Fusion::default(), 10, 99).len(), 2);
    }

    #[test]
    fn two_empty_lists_produce_nothing() {
        let s = store_of(10, 1);
        assert!(fuse6(&[], &[], &s, Fusion::default(), 10, 99).is_empty());
    }

    #[test]
    fn normalized_score_fusion_respects_the_weight() {
        let s = store_of(10, 1);
        // Vector likes chunk 0, lexical likes chunk 5. Weighting the vector side
        // fully should put chunk 0 first, and vice versa.
        let all_vector = fuse6(
            &v(&[0, 1, 2]),
            &l(&[5, 6, 7]),
            &s,
            Fusion::NormalizedScore { vector_weight: 1.0 },
            10,
            99,
        );
        assert_eq!(all_vector[0].chunk, 0);
        let all_lexical = fuse6(
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
        let fused = fuse6(&flat, &[], &s, Fusion::NormalizedScore { vector_weight: 1.0 }, 10, 99);
        assert_eq!(fused.len(), 3);
        assert!(fused.iter().all(|h| h.score.is_finite()));
    }

    #[test]
    fn fusion_is_deterministic() {
        let s = store_of(10, 2);
        let a = fuse6(&v(&[0, 1, 2, 3]), &l(&[3, 2, 1, 0]), &s, Fusion::default(), 10, 2);
        let b = fuse6(&v(&[0, 1, 2, 3]), &l(&[3, 2, 1, 0]), &s, Fusion::default(), 10, 2);
        assert_eq!(
            a.iter().map(|h| h.chunk).collect::<Vec<_>>(),
            b.iter().map(|h| h.chunk).collect::<Vec<_>>()
        );
    }

    #[test]
    fn origin_is_reported_per_hit() {
        let s = store_of(10, 1);
        let fused = fuse6(&v(&[0]), &l(&[1]), &s, Fusion::default(), 10, 99);
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
        let convex = fuse6(&vector, &[], &s, Fusion::Convex { vector_weight: 1.0 }, 10, 99);
        let min_max = fuse6(&vector, &[], &s, Fusion::NormalizedScore { vector_weight: 1.0 }, 10, 99);
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
        let zeros = vec![lex(3, 0.0), lex(4, 0.0)];
        let fused = fuse6(&v(&[0, 1]), &zeros, &s, Fusion::Convex { vector_weight: 0.5 }, 10, 99);
        assert_eq!(fused.len(), 4);
        assert!(fused.iter().all(|h| h.score.is_finite()));
    }

    /// The property theoretical bounds exist for. Min-max reads its scale out of
    /// the list, so a lexical list of hopeless matches presents its leader at 1.0
    /// and fusion cannot tell it from a list of perfect ones. Dividing by the
    /// query's own ceiling keeps a weak list weak.
    #[test]
    fn theoretical_bounds_keep_a_weak_lexical_list_weak() {
        let s = store_of(10, 1);
        // A ceiling of 20 with a best hit of 1.0: the query could have scored
        // twenty times better than anything the corpus offered.
        let weak = vec![lex(5, 1.0), lex(6, 0.5)];
        let bounds = ScoreBounds { lexical_ceiling: 20.0 };
        let tmm = fuse(
            &[],
            &weak,
            &s,
            None,
            FusionParams {
                fusion: Fusion::TheoreticalMinMax { vector_weight: 0.0 },
                top_k: 10,
                per_doc_cap: 99,
                bounds,
                mmr_lambda: 1.0,
            },
        );
        let min_max = fuse6(&[], &weak, &s, Fusion::NormalizedScore { vector_weight: 0.0 }, 10, 99);
        assert!((min_max[0].score - 1.0).abs() < 1e-6, "min-max always crowns its leader");
        assert!(tmm[0].score < 0.1, "a hit at a twentieth of the ceiling should stay small: {}", tmm[0].score);
        assert_eq!(tmm[0].chunk, 5, "the order is unchanged, only the magnitude");
    }

    /// A query holding no dictionary term has a ceiling of zero. Dividing by it
    /// would produce infinity, so the lexical side has to contribute nothing.
    #[test]
    fn a_zero_ceiling_contributes_nothing_rather_than_infinity() {
        let s = store_of(10, 1);
        let fused = fuse(
            &v(&[0]),
            &[lex(5, 3.0)],
            &s,
            None,
            FusionParams {
                fusion: Fusion::TheoreticalMinMax { vector_weight: 0.5 },
                top_k: 10,
                per_doc_cap: 99,
                bounds: ScoreBounds { lexical_ceiling: 0.0 },
                mmr_lambda: 1.0,
            },
        );
        assert!(fused.iter().all(|h| h.score.is_finite()));
        assert_eq!(fused.iter().find(|h| h.chunk == 5).unwrap().score, 0.0);
    }

    /// Every gain at zero has to reproduce the fixed weight exactly, or turning
    /// adaptive fusion on could never be judged against leaving it off.
    #[test]
    fn adaptive_weighting_with_no_gains_is_the_base_weight() {
        let rule = AdaptiveWeights { base: 0.35, ..Default::default() };
        assert!(rule.is_fixed());
        let signals = QuerySignals {
            identifier_share: 1.0,
            out_of_vocabulary_share: 1.0,
            lexical_coverage: 1.0,
            lexical_separation: 1.0,
            vector_separation: 0.0,
        };
        assert!((rule.weight_for(&signals) - 0.35).abs() < 1e-6);
    }

    #[test]
    fn adaptive_weighting_leans_lexical_for_identifiers_and_dense_for_unknown_words() {
        let rule = AdaptiveWeights {
            base: 0.5,
            identifier_gain: 0.4,
            out_of_vocabulary_gain: 0.4,
            ..Default::default()
        };
        assert!(!rule.is_fixed());
        let identifier = QuerySignals { identifier_share: 1.0, ..Default::default() };
        let unknown = QuerySignals { out_of_vocabulary_share: 1.0, ..Default::default() };
        assert!(rule.weight_for(&identifier) < 0.5, "an identifier query should lean lexical");
        assert!(rule.weight_for(&unknown) > 0.5, "a query of unknown words has only the vector side");
    }

    #[test]
    fn adaptive_weighting_never_silences_a_retriever() {
        let rule = AdaptiveWeights {
            base: 0.5,
            identifier_gain: 10.0,
            out_of_vocabulary_gain: 10.0,
            floor: 0.05,
            ceiling: 0.95,
            ..Default::default()
        };
        let all_identifier = QuerySignals { identifier_share: 1.0, ..Default::default() };
        let all_unknown = QuerySignals { out_of_vocabulary_share: 1.0, ..Default::default() };
        assert!((rule.weight_for(&all_identifier) - 0.05).abs() < 1e-6);
        assert!((rule.weight_for(&all_unknown) - 0.95).abs() < 1e-6);
    }

    #[test]
    fn separation_is_zero_for_a_flat_list_and_high_for_a_clear_leader() {
        assert_eq!(separation(&[1.0, 1.0, 1.0]), 0.0);
        assert!(separation(&[1.0, 0.01, 0.01]) > 0.9);
        assert_eq!(separation(&[5.0]), 0.0, "one candidate says nothing about separation");
        assert_eq!(separation(&[0.0, 0.0]), 0.0, "a list of zeros must not divide by zero");
    }

    /// Diversity selection has to prefer a slightly worse hit that says something
    /// new over a marginally better one that repeats what is already selected.
    #[test]
    fn diversity_selection_prefers_a_novel_hit_over_a_near_duplicate() {
        let s = store_of(3, 1);
        let mut vectors = crate::vectors::VectorSet::new(2);
        vectors.push(&[1.0, 0.0]);
        vectors.push(&[1.0, 0.001]);
        vectors.push(&[0.0, 1.0]);
        let all = vec![
            FusedHit { chunk: 0, score: 1.00, origin: HitOrigin::Vector, confidence: 0.0 },
            FusedHit { chunk: 1, score: 0.99, origin: HitOrigin::Vector, confidence: 0.0 },
            FusedHit { chunk: 2, score: 0.90, origin: HitOrigin::Vector, confidence: 0.0 },
        ];
        let by_score = select_by_score(&all, &s, 2, 99);
        assert_eq!(by_score.iter().map(|h| h.chunk).collect::<Vec<_>>(), vec![0, 1]);
        let diverse = select_diverse(&all, &s, &vectors, 2, 99, 0.5);
        assert_eq!(diverse.iter().map(|h| h.chunk).collect::<Vec<_>>(), vec![0, 2]);
    }

    /// Lambda 1 is pure score, so it must be exactly the selection that ignores
    /// diversity. Otherwise the setting could not be turned off.
    #[test]
    fn diversity_at_lambda_one_is_the_score_ordering() {
        let s = store_of(6, 1);
        let mut vectors = crate::vectors::VectorSet::new(2);
        for i in 0..6 {
            vectors.push(&[1.0, i as f32 * 0.01]);
        }
        let all: Vec<FusedHit> = (0..6u32)
            .map(|c| FusedHit { chunk: c, score: 1.0 - c as f32 * 0.1, origin: HitOrigin::Vector, confidence: 0.0 })
            .collect();
        assert_eq!(
            select_diverse(&all, &s, &vectors, 4, 99, 1.0).iter().map(|h| h.chunk).collect::<Vec<_>>(),
            select_by_score(&all, &s, 4, 99).iter().map(|h| h.chunk).collect::<Vec<_>>()
        );
    }

    #[test]
    fn diversity_selection_still_honours_the_per_document_cap() {
        let s = store_of(2, 5);
        let mut vectors = crate::vectors::VectorSet::new(2);
        for i in 0..10 {
            vectors.push(&[1.0, i as f32 * 0.001]);
        }
        let all: Vec<FusedHit> = (0..10u32)
            .map(|c| FusedHit { chunk: c, score: 1.0 - c as f32 * 0.01, origin: HitOrigin::Vector, confidence: 0.0 })
            .collect();
        let picked = select_diverse(&all, &s, &vectors, 10, PER_DOC_CAP, 0.5);
        for doc in 0..2u32 {
            let n = picked.iter().filter(|h| s.chunks[h.chunk as usize].doc == doc).count();
            assert!(n <= PER_DOC_CAP, "document {doc} contributed {n}");
        }
    }

}
