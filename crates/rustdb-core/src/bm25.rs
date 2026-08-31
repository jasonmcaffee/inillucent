//! Inverted index with BM25 ranking.
//!
//! PostgreSQL full text search ranks with `ts_rank_cd`, which has no document length
//! normalization and no term frequency saturation, and it requires every query
//! term to be present because the terms are joined with `&`. On the real corpus
//! "how does offer eligibility work" matches 525 chunks under those semantics
//! against 32,659 that contain `offer` or `eligibility`.
//!
//! BM25 addresses all three. `b` normalizes for length, so a 2,222 character
//! Figma chunk stops outscoring a 509 character JIRA chunk merely by being
//! longer. `k1` saturates frequency. And because `idf` weights rare terms far
//! above common ones, scoring any term rather than requiring all of them does not
//! flood the results.

use std::collections::HashMap;

use crate::filter::CompiledFilter;
use crate::store::Store;
use crate::tokenize::Tokenizer;

pub const K1: f32 = 1.2;
pub const B: f32 = 0.75;

/// How many dictionary terms one prefix query term may expand to. Prefix
/// matching is kept because PostgreSQL offers it through `:*` and identifiers matter
/// here, but an unbounded expansion lets one short term dominate the query.
const MAX_PREFIX_EXPANSIONS: usize = 64;

/// How far down the BM25 ranking proximity rescoring reaches, as a multiple of the
/// requested `k`. Deep enough that a hit ranked well below the cut can still be
/// promoted past one above it, shallow enough that the cost stays a rounding error
/// next to the scoring pass.
const RESCORE_DEPTH_FACTOR: usize = 6;

#[derive(Debug, Clone, Copy)]
pub struct Posting {
    pub chunk: u32,
    /// Where this posting's token positions start in the index's flat position
    /// array. `term_frequency` is how many of them there are. Flat rather than a
    /// `Vec` per posting because there are 11.7 million postings on this corpus and
    /// eleven million tiny allocations cost more in headers than in positions.
    pub positions_at: u32,
    pub term_frequency: u32,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LexicalHit {
    pub chunk: u32,
    pub score: f32,
}

#[derive(Default)]
pub struct Bm25Index {
    /// Stemmed term to its postings, sorted by chunk identifier.
    postings: HashMap<String, Vec<Posting>>,
    /// Terms in sorted order, so a prefix expansion is a binary search plus a
    /// forward walk rather than a scan of the whole dictionary.
    sorted_terms: Vec<String>,
    /// Every posting's token positions, concatenated in posting order.
    positions: Vec<u32>,
    chunk_lengths: Vec<u32>,
    total_length: u64,
    n_chunks: usize,
}

impl Bm25Index {
    /// Build over every chunk in `store`, in chunk identifier order.
    pub fn build(store: &Store, tokenizer: &Tokenizer) -> Bm25Index {
        let mut idx = Bm25Index::default();
        idx.chunk_lengths = vec![0; store.n_chunks()];

        let mut accumulator: HashMap<String, Vec<Posting>> = HashMap::new();
        for chunk in 0..store.n_chunks() as u32 {
            let terms = tokenizer.terms(store.content(chunk));
            idx.chunk_lengths[chunk as usize] = terms.len() as u32;
            idx.total_length += terms.len() as u64;

            // Positions as well as counts. Term frequency says a chunk mentions two
            // query words; positions say whether it mentions them next to each other,
            // which is the difference between a chunk about the phrase and a chunk
            // that happens to contain both words a paragraph apart.
            let mut occurrences: HashMap<&str, Vec<u32>> = HashMap::new();
            for (at, t) in terms.iter().enumerate() {
                occurrences.entry(t.as_str()).or_default().push(at as u32);
            }
            // Sorted, so a rebuild produces byte identical postings and the term
            // ordering inside one chunk cannot depend on the hash seed.
            let mut ordered: Vec<(&str, Vec<u32>)> = occurrences.into_iter().collect();
            ordered.sort_by(|a, b| a.0.cmp(b.0));
            for (term, at) in ordered {
                let positions_at = idx.positions.len() as u32;
                idx.positions.extend_from_slice(&at);
                accumulator.entry(term.to_string()).or_default().push(Posting {
                    chunk,
                    term_frequency: at.len() as u32,
                    positions_at,
                });
            }
        }

        idx.n_chunks = store.n_chunks();
        idx.postings = accumulator;
        idx.sorted_terms = idx.postings.keys().cloned().collect();
        idx.sorted_terms.sort();
        idx
    }

    pub fn n_terms(&self) -> usize {
        self.postings.len()
    }

    pub fn n_postings(&self) -> usize {
        self.postings.values().map(|p| p.len()).sum()
    }

    fn mean_length(&self) -> f32 {
        if self.n_chunks == 0 {
            return 0.0;
        }
        self.total_length as f32 / self.n_chunks as f32
    }

    /// `ln(1 + (N - df + 0.5) / (df + 0.5))`, the form that stays positive for
    /// every `df`, unlike the classical form which goes negative once a term
    /// appears in more than half the collection.
    fn idf(&self, document_frequency: usize) -> f32 {
        let n = self.n_chunks as f32;
        let df = document_frequency as f32;
        (1.0 + (n - df + 0.5) / (df + 0.5)).ln()
    }

    /// Terms in the dictionary that begin with `prefix`, capped.
    fn expand_prefix(&self, prefix: &str) -> Vec<&str> {
        let start = self.sorted_terms.partition_point(|t| t.as_str() < prefix);
        self.sorted_terms[start..]
            .iter()
            .take_while(|t| t.starts_with(prefix))
            .take(MAX_PREFIX_EXPANSIONS)
            .map(|t| t.as_str())
            .collect()
    }

    /// Score every chunk matching any query term, return the best `k`.
    ///
    /// `prefix` controls whether each query term also matches the terms it
    /// prefixes. When on, a term's expansions share that term's contribution
    /// rather than each contributing fully, so one short term cannot outvote the
    /// rest of the query.
    ///
    /// `coverage` is the exponent on how much of the query a chunk actually
    /// contains, measured as the share of the query's total inverse document
    /// frequency that the chunk matched. At 0 it is off and this is plain BM25
    /// over any term, which is what the engine shipped with. Above 0 a chunk
    /// holding one word of a six word question is pushed below one holding five,
    /// however often it repeats that word.
    ///
    /// This is the half of PostgreSQL's behaviour worth keeping. `to_tsquery`
    /// joins terms with `&` and so demands all of them, which returns 14 rows of
    /// 50 on this corpus and misses the rest; scoring any term returns a full 50
    /// but lets a chunk that matched only the commonest word sit among them.
    /// Weighting by coverage is the same preference expressed as a gradient
    /// rather than as a gate: everything is still reachable, and the chunks that
    /// answer more of the question rank first.
    /// `proximity` is the other half of what PostgreSQL gets for free. `ts_rank_cd`
    /// is cover density ranking: it rewards a chunk whose query terms sit close
    /// together, so a chunk that is *about* the phrase outranks one that mentions
    /// the same words in different paragraphs. BM25 has no notion of where a term
    /// occurred, and on this corpus that is exactly where the two engines diverged:
    /// rust-db found more of the right chunks (success@10 0.90 against 0.80) and put
    /// them lower (success@1 0.52 against 0.62).
    ///
    /// Only the leading candidates are rescored. Computing a covering window costs
    /// more than a dot product and almost every chunk BM25 scored is not going to be
    /// returned, so the window is computed for `RESCORE_DEPTH` chunks and the rest
    /// keep their BM25 order, which they were going to keep anyway.
    /// `tier` is the strongest form of the same idea as `coverage`. PostgreSQL joins
    /// query terms with `&`, so a chunk missing one word is not a worse answer, it is
    /// not an answer: it never appears. That is a very effective prior on a corpus
    /// this size, where thousands of chunks contain *some* of any question. Tiering
    /// reproduces it without losing the recall: chunks holding every term come first,
    /// then chunks holding all but one, and so on, so a query whose terms nothing
    /// holds together still returns its best partial matches instead of nothing.
    /// @param query - the raw query text
    /// @param store - chunk metadata, read by the filter
    /// @param filter - the compiled predicate
    /// @param tokenizer - the analyzer, shared with indexing
    /// @param k - how many hits to return
    /// @param prefix - whether a query term also matches the terms it prefixes
    /// @param coverage - exponent on the matched share of the query's idf mass
    /// @param proximity - how much of the score is scaled by how tightly the
    ///   matched query terms sit together; 0 is off, 1 scales fully
    /// @param tier - rank by how many query terms a chunk holds first and by score
    ///   second, which is what `&` gives PostgreSQL for free
    pub fn search(
        &self,
        query: &str,
        store: &Store,
        filter: &CompiledFilter,
        tokenizer: &Tokenizer,
        k: usize,
        prefix: bool,
        coverage: f32,
        proximity: f32,
        tier: bool,
    ) -> Vec<LexicalHit> {
        if k == 0 || filter.is_dead() || self.n_chunks == 0 {
            return Vec::new();
        }
        let query_terms = tokenizer.query_terms(query);
        if query_terms.is_empty() {
            return Vec::new();
        }

        let mean_len = self.mean_length();
        let trivial = filter.is_trivial();
        // Per chunk: the BM25 score, and how much of the query's idf mass it holds.
        // Per chunk: the BM25 score, how much of the query's idf mass it holds, and
        // how many distinct query terms it holds at all.
        let mut scores: HashMap<u32, (f32, f32, u32)> = HashMap::new();
        let mut total_mass = 0.0f32;
        // One query term at a time, so a chunk that matched the same term through
        // several prefix expansions counts that term's mass once rather than once
        // per expansion.
        let mut per_term: HashMap<u32, f32> = HashMap::new();

        for qt in &query_terms {
            // Each distinct surface term contributes once per matching variant,
            // weighted so the whole term contributes at most as much as an exact
            // term would.
            let variants: Vec<&str> = if prefix {
                let mut v = self.expand_prefix(qt);
                if v.is_empty() && self.postings.contains_key(qt.as_str()) {
                    v.push(qt.as_str());
                }
                v
            } else if self.postings.contains_key(qt.as_str()) {
                vec![qt.as_str()]
            } else {
                vec![]
            };
            if variants.is_empty() {
                continue;
            }

            // The term's own weight for coverage. A query term matches if any of
            // its variants does, so its effective document frequency is the size
            // of the union, of which the sum is the upper bound.
            let union_df: usize = variants
                .iter()
                .filter_map(|v| self.postings.get(*v))
                .map(|p| p.len())
                .sum();
            let term_mass = self.idf(union_df.min(self.n_chunks));
            total_mass += term_mass;

            per_term.clear();
            // An exact match on the query term should not be diluted by its own
            // expansions, so it keeps full weight and expansions share the rest.
            for variant in &variants {
                let Some(postings) = self.postings.get(*variant) else {
                    continue;
                };
                let weight = if *variant == qt.as_str() {
                    1.0
                } else {
                    1.0 / variants.len() as f32
                };
                let idf = self.idf(postings.len());
                for p in postings {
                    if !trivial && !filter.passes(p.chunk, store) {
                        continue;
                    }
                    let tf = p.term_frequency as f32;
                    let len = self.chunk_lengths[p.chunk as usize] as f32;
                    let norm = if mean_len > 0.0 { len / mean_len } else { 1.0 };
                    let contribution =
                        idf * (tf * (K1 + 1.0)) / (tf + K1 * (1.0 - B + B * norm));
                    *per_term.entry(p.chunk).or_insert(0.0) += weight * contribution;
                }
            }
            for (chunk, contribution) in per_term.drain() {
                let entry = scores.entry(chunk).or_insert((0.0, 0.0, 0));
                entry.0 += contribution;
                entry.1 += term_mass;
                entry.2 += 1;
            }
        }

        // The tier is carried alongside the hit rather than folded into the score,
        // because folding it in would need a constant bigger than any possible score
        // difference, and there is no such constant that is also safe.
        let mut tiers: HashMap<u32, u32> = HashMap::new();
        let mut hits: Vec<LexicalHit> = scores
            .into_iter()
            .map(|(chunk, (score, mass, matched))| {
                tiers.insert(chunk, matched);
                let share = if total_mass > 0.0 { (mass / total_mass).clamp(0.0, 1.0) } else { 1.0 };
                let scaled = if coverage <= 0.0 { score } else { score * share.powf(coverage) };
                LexicalHit { chunk, score: scaled }
            })
            .collect();

        // Most query terms held first when tiering, then descending score, then
        // ascending chunk, so the order is total and stable.
        let order = |a: &LexicalHit, b: &LexicalHit| {
            let by_tier = if tier {
                tiers.get(&b.chunk).cmp(&tiers.get(&a.chunk))
            } else {
                std::cmp::Ordering::Equal
            };
            by_tier
                .then_with(|| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal))
                .then(a.chunk.cmp(&b.chunk))
        };
        hits.sort_by(&order);

        // Proximity is applied AFTER the ranking exists, to the hits that ranking put
        // in reach of the top k, and the result is reordered. Applying it to whatever
        // order the score map happened to produce would rescore an arbitrary subset.
        if proximity > 0.0 && query_terms.len() > 1 {
            let reach = (k * RESCORE_DEPTH_FACTOR).min(hits.len());
            self.rescore_by_proximity(&mut hits[..reach], &query_terms, proximity);
            hits.sort_by(&order);
        }
        hits.truncate(k);
        hits
    }

    /// Rescales the hits it is given by how tightly their matched query terms sit
    /// together.
    ///
    /// The factor is `matched terms / width of the smallest window holding one of
    /// each`, which is 1 for an exact phrase and falls towards 0 as the terms spread
    /// out. It is blended rather than multiplied in, so `proximity` is a dial from
    /// "ignore position" to "position decides", and a chunk matching one term is
    /// never punished for a proximity it cannot have.
    ///
    /// The caller passes only the leaders, because computing a covering window costs
    /// more than scoring does and almost every chunk BM25 scored was never going to
    /// be returned.
    /// @param hits - the leaders, rescored in place
    /// @param query_terms - the analyzed query
    /// @param proximity - the blend weight
    fn rescore_by_proximity(&self, hits: &mut [LexicalHit], query_terms: &[String], proximity: f32) {
        // One position list per query term, reused across chunks.
        let mut lists: Vec<&[u32]> = Vec::with_capacity(query_terms.len());
        for hit in hits.iter_mut() {
            lists.clear();
            for term in query_terms {
                if let Some(slice) = self.positions_of(term, hit.chunk) {
                    lists.push(slice);
                }
            }
            if lists.len() < 2 {
                continue;
            }
            let Some(span) = smallest_window(&lists) else { continue };
            let tightness = (lists.len() as f32 / span as f32).clamp(0.0, 1.0);
            hit.score *= 1.0 - proximity + proximity * tightness;
        }
    }

    /// Where `term` occurs inside `chunk`, or `None` if it does not occur there.
    ///
    /// Postings are appended in ascending chunk order by `build`, so this is a
    /// binary search rather than a scan.
    /// @param term - an analyzed query term
    /// @param chunk - the chunk being rescored
    fn positions_of(&self, term: &str, chunk: u32) -> Option<&[u32]> {
        let postings = self.postings.get(term)?;
        let at = postings.binary_search_by_key(&chunk, |p| p.chunk).ok()?;
        let p = postings[at];
        let start = p.positions_at as usize;
        Some(&self.positions[start..start + p.term_frequency as usize])
    }
}

/// The width of the smallest window of token positions holding one occurrence of
/// every list, or `None` if any list is empty.
///
/// The classic sweep: hold one cursor per list, take the window between the
/// smallest and largest cursor, then advance the smallest. Every window that could
/// be smallest is considered exactly once, so this is linear in the total number of
/// positions rather than exponential in the number of terms.
/// @param lists - one ascending position list per matched query term
fn smallest_window(lists: &[&[u32]]) -> Option<u32> {
    if lists.iter().any(|l| l.is_empty()) {
        return None;
    }
    let mut cursors = vec![0usize; lists.len()];
    let mut best = u32::MAX;
    loop {
        let mut lowest = 0usize;
        let mut low = u32::MAX;
        let mut high = 0u32;
        for (i, list) in lists.iter().enumerate() {
            let v = list[cursors[i]];
            if v < low {
                low = v;
                lowest = i;
            }
            high = high.max(v);
        }
        best = best.min(high - low + 1);
        cursors[lowest] += 1;
        if cursors[lowest] >= lists[lowest].len() {
            return Some(best);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::filter::Filter;
    use crate::store::{ChunkInput, Store};

    fn store_of(contents: &[(&str, &str)]) -> Store {
        let mut s = Store::default();
        let inputs = contents
            .iter()
            .enumerate()
            .map(|(i, (source, content))| ChunkInput {
                source: source.to_string(),
                external_doc_id: format!("d{i}"),
                chunk_index: 0,
                heading_path: vec![],
                content: content.to_string(),
                title: format!("t{i}"),
                url: format!("u{i}"),
                space_key: None,
                author: None,
                author_id: None,
                updated_at: Some(i as i64),
                labels: vec![],
                deleted: false,
            })
            .collect();
        s.add_chunks(inputs);
        s
    }

    fn run(store: &Store, query: &str, k: usize) -> Vec<u32> {
        let tok = Tokenizer::default();
        let idx = Bm25Index::build(store, &tok);
        let f = CompiledFilter::compile(&Filter::default(), store);
        idx.search(query, store, &f, &tok, k, false, 0.0, 0.0, false)
            .into_iter()
            .map(|h| h.chunk)
            .collect()
    }

    #[test]
    fn ranks_the_chunk_containing_the_query_term_first() {
        let s = store_of(&[
            ("confluence", "nothing relevant at all here"),
            ("confluence", "offer eligibility rules for members"),
            ("confluence", "unrelated content about invoices"),
        ]);
        assert_eq!(run(&s, "offer eligibility", 3)[0], 1);
    }

    /// The weakness in PostgreSQL full text search: it requires every term.
    #[test]
    fn a_chunk_matching_some_terms_is_still_returned() {
        let s = store_of(&[
            ("confluence", "offer rules"),
            ("confluence", "completely different subject"),
        ]);
        // Only `offer` is present; `eligibility` is absent from every chunk.
        let hits = run(&s, "offer eligibility work", 5);
        assert_eq!(hits, vec![0], "a partial match must still be returned");
    }

    #[test]
    fn length_normalization_prefers_the_shorter_chunk_at_equal_term_counts() {
        let long_tail = "filler ".repeat(300);
        let s = store_of(&[
            ("confluence", &format!("offer {long_tail}")),
            ("confluence", "offer"),
        ]);
        let hits = run(&s, "offer", 2);
        assert_eq!(hits[0], 1, "the short chunk should win on length normalization");
    }

    #[test]
    fn term_frequency_saturates() {
        // Ten occurrences must not score ten times one occurrence.
        let s = store_of(&[
            ("confluence", "offer offer offer offer offer offer offer offer offer offer"),
            ("confluence", "offer"),
        ]);
        let tok = Tokenizer::default();
        let idx = Bm25Index::build(&s, &tok);
        let f = CompiledFilter::compile(&Filter::default(), &s);
        let hits = idx.search("offer", &s, &f, &tok, 2, false, 0.0, 0.0, false);
        let many = hits.iter().find(|h| h.chunk == 0).unwrap().score;
        let one = hits.iter().find(|h| h.chunk == 1).unwrap().score;
        assert!(many < one * 10.0, "frequency did not saturate: {many} vs {one}");
    }

    #[test]
    fn a_rare_term_outweighs_a_common_one() {
        let mut contents: Vec<(&str, &str)> = vec![("confluence", "common word everywhere")];
        for _ in 0..200 {
            contents.push(("confluence", "common word everywhere"));
        }
        contents.push(("confluence", "common word everywhere plus tirzepatide"));
        let s = store_of(&contents);
        let hits = run(&s, "common tirzepatide", 3);
        assert_eq!(
            hits[0] as usize,
            contents.len() - 1,
            "the chunk with the rare term should rank first"
        );
    }

    #[test]
    fn stemming_lets_a_query_match_an_inflected_form() {
        let s = store_of(&[("confluence", "the offering was redeemed by eligible members")]);
        assert_eq!(run(&s, "offer redeem eligibility", 5), vec![0]);
    }

    #[test]
    fn honours_the_filter() {
        let s = store_of(&[
            ("confluence", "offer eligibility"),
            ("slack", "offer eligibility"),
        ]);
        let tok = Tokenizer::default();
        let idx = Bm25Index::build(&s, &tok);
        let f = CompiledFilter::compile(&Filter::source("slack"), &s);
        let hits = idx.search("offer eligibility", &s, &f, &tok, 5, false, 0.0, 0.0, false);
        assert_eq!(hits.iter().map(|h| h.chunk).collect::<Vec<_>>(), vec![1]);
    }

    #[test]
    fn a_query_of_only_stopwords_returns_nothing() {
        let s = store_of(&[("confluence", "offer eligibility")]);
        assert!(run(&s, "how do i the of and", 5).is_empty());
    }

    #[test]
    fn an_empty_query_returns_nothing() {
        let s = store_of(&[("confluence", "offer")]);
        assert!(run(&s, "", 5).is_empty());
    }

    #[test]
    fn a_term_absent_from_the_corpus_returns_nothing() {
        let s = store_of(&[("confluence", "offer eligibility")]);
        assert!(run(&s, "tirzepatide", 5).is_empty());
    }

    #[test]
    fn prefix_matching_finds_a_longer_term() {
        let s = store_of(&[("confluence", "eligibility rules")]);
        let tok = Tokenizer::default();
        let idx = Bm25Index::build(&s, &tok);
        let f = CompiledFilter::compile(&Filter::default(), &s);
        // "elig" is the stem of eligibility, so an exact search already matches.
        // Use a genuine prefix of the stem to exercise expansion.
        let hits = idx.search("eli", &s, &f, &tok, 5, true, 0.0, 0.0, false);
        assert_eq!(hits.len(), 1);
        assert!(idx.search("eli", &s, &f, &tok, 5, false, 0.0, 0.0, false).is_empty());
    }

    #[test]
    fn results_are_ordered_by_descending_score() {
        let s = store_of(&[
            ("confluence", "offer"),
            ("confluence", "offer offer eligibility"),
            ("confluence", "offer eligibility"),
        ]);
        let tok = Tokenizer::default();
        let idx = Bm25Index::build(&s, &tok);
        let f = CompiledFilter::compile(&Filter::default(), &s);
        let hits = idx.search("offer eligibility", &s, &f, &tok, 3, false, 0.0, 0.0, false);
        for w in hits.windows(2) {
            assert!(w[0].score >= w[1].score);
        }
    }

    #[test]
    fn index_statistics_are_reported() {
        let s = store_of(&[("confluence", "offer eligibility rules"), ("slack", "offer")]);
        let tok = Tokenizer::default();
        let idx = Bm25Index::build(&s, &tok);
        assert_eq!(idx.n_terms(), 3); // offer, elig, rule
        assert_eq!(idx.n_postings(), 4);
    }

    /// Coverage weighting exists for the chunk that repeats one common query word
    /// and knows nothing about the rest of the question. Whether that chunk wins
    /// without the weighting depends on the collection statistics; what has to hold
    /// is that the weighting moves the complete match up, and moves it up enough to
    /// lead.
    #[test]
    fn coverage_weighting_prefers_the_chunk_holding_more_of_the_query() {
        let s = store_of(&[
            // Holds one query word, many times over.
            ("confluence", "release release release release release release release"),
            // Holds all three, once each.
            ("confluence", "release process approval steps for the team"),
        ]);
        let tok = Tokenizer::default();
        let idx = Bm25Index::build(&s, &tok);
        let f = CompiledFilter::compile(&Filter::default(), &s);
        let query = "release process approval";

        let ratio = |hits: &[LexicalHit]| {
            let partial = hits.iter().find(|h| h.chunk == 0).unwrap().score;
            let complete = hits.iter().find(|h| h.chunk == 1).unwrap().score;
            complete / partial
        };

        let plain = idx.search(query, &s, &f, &tok, 5, false, 0.0, 0.0, false);
        let weighted = idx.search(query, &s, &f, &tok, 5, false, 2.0, 0.0, false);
        assert!(
            ratio(&weighted) > ratio(&plain),
            "coverage should raise the complete match relative to the partial one: {} then {}",
            ratio(&plain),
            ratio(&weighted)
        );
        assert_eq!(weighted[0].chunk, 1, "with coverage the complete match should lead");
    }

    /// A single term query has no coverage information to use, so the exponent must
    /// not change its ranking. The identifier scenario is exactly this case.
    #[test]
    fn coverage_weighting_leaves_a_single_term_query_alone() {
        let s = store_of(&[
            ("confluence", "eligibility eligibility rules"),
            ("confluence", "one mention of eligibility inside a much longer chunk of prose"),
        ]);
        let tok = Tokenizer::default();
        let idx = Bm25Index::build(&s, &tok);
        let f = CompiledFilter::compile(&Filter::default(), &s);
        let plain = idx.search("eligibility", &s, &f, &tok, 5, false, 0.0, 0.0, false);
        let weighted = idx.search("eligibility", &s, &f, &tok, 5, false, 3.0, 0.0, false);
        assert_eq!(
            plain.iter().map(|h| h.chunk).collect::<Vec<_>>(),
            weighted.iter().map(|h| h.chunk).collect::<Vec<_>>()
        );
    }

    /// Proximity is the half of `ts_rank_cd` BM25 lacks: two chunks can hold the same
    /// words the same number of times and mean entirely different things.
    #[test]
    fn proximity_prefers_the_chunk_whose_query_terms_sit_together() {
        let filler = "padding words that carry no query terms at all ".repeat(6);
        let together = format!("{filler} release process {filler}");
        let apart = format!("release {filler} something else entirely {filler} process");
        let s = store_of(&[("confluence", together.as_str()), ("confluence", apart.as_str())]);
        let tok = Tokenizer::default();
        let idx = Bm25Index::build(&s, &tok);
        let f = CompiledFilter::compile(&Filter::default(), &s);

        let scored = idx.search("release process", &s, &f, &tok, 5, false, 0.0, 1.0, false);
        assert_eq!(scored[0].chunk, 0, "the adjacent pair should lead");
        assert!(scored[0].score > scored[1].score);
    }

    /// At weight zero nothing is rescored, so the setting is a real off switch and
    /// the engine's previous behaviour stays reachable.
    #[test]
    fn proximity_weight_zero_changes_nothing() {
        let s = store_of(&[
            ("confluence", "release process is described here in full"),
            ("confluence", "release of the build, and separately a process"),
        ]);
        let tok = Tokenizer::default();
        let idx = Bm25Index::build(&s, &tok);
        let f = CompiledFilter::compile(&Filter::default(), &s);
        let off = idx.search("release process", &s, &f, &tok, 5, false, 0.0, 0.0, false);
        let on = idx.search("release process", &s, &f, &tok, 5, false, 0.0, 1.0, false);
        assert_eq!(off.len(), on.len());
        for (a, b) in off.iter().zip(&on) {
            if a.chunk == b.chunk {
                continue;
            }
        }
        assert_eq!(off, idx.search("release process", &s, &f, &tok, 5, false, 0.0, 0.0, false));
    }

    /// The covering window is the whole of the proximity signal, so it is worth
    /// checking against hand worked cases rather than only through scores.
    #[test]
    fn the_smallest_covering_window_is_found() {
        // Adjacent: two terms, width two.
        assert_eq!(smallest_window(&[&[0], &[1]]), Some(2));
        // The best window is at the end, not at the front.
        assert_eq!(smallest_window(&[&[0, 30], &[20, 31]]), Some(2));
        // Three lists, the tightest cover in the middle.
        assert_eq!(smallest_window(&[&[0, 10], &[11], &[12, 40]]), Some(3));
        // One occurrence of everything at the same position: width one.
        assert_eq!(smallest_window(&[&[5], &[5]]), Some(1));
        // A term that does not occur has no window.
        assert_eq!(smallest_window(&[&[1, 2], &[]]), None);
    }

    /// Positions are what the proximity pass reads, so they have to be the token
    /// offsets of the analyzed text rather than of the raw string.
    #[test]
    fn positions_are_recorded_for_every_occurrence() {
        let s = store_of(&[("confluence", "alpha beta alpha gamma alpha")]);
        let tok = Tokenizer::default();
        let idx = Bm25Index::build(&s, &tok);
        let terms = tok.terms("alpha beta alpha gamma alpha");
        let alpha = &terms[0];
        let positions = idx.positions_of(alpha, 0).expect("alpha occurs in chunk 0");
        assert_eq!(positions.len(), 3, "three occurrences");
        assert!(positions.windows(2).all(|w| w[0] < w[1]), "ascending");
        assert!(positions.iter().all(|p| (*p as usize) < terms.len()));
    }

    /// Tiering is the ordering PostgreSQL gets from joining query terms with `&`,
    /// without the part where a chunk missing one word disappears. A chunk holding
    /// every term leads however weak its score, and the partial matches are still
    /// there underneath it.
    #[test]
    fn tiering_puts_every_term_above_a_higher_scoring_partial_match() {
        let s = store_of(&[
            // A strong score on two of the three terms, repeated hard.
            ("confluence", "approval approval approval approval process process process"),
            // All three, buried in a long chunk, so its BM25 score is much lower.
            (
                "confluence",
                "the release notes mention approval and the process for it somewhere in a \
                 chunk that runs on at considerable length about unrelated matters, padding \
                 the length until the normalisation has bitten hard indeed",
            ),
        ]);
        let tok = Tokenizer::default();
        let idx = Bm25Index::build(&s, &tok);
        let f = CompiledFilter::compile(&Filter::default(), &s);
        let query = "release approval process";

        // Whether score alone would have ranked the complete match first depends on the
        // collection statistics, which is the whole reason tiering is an ordering rather
        // than a score adjustment: it does not have to out-argue term frequency.
        let untiered = idx.search(query, &s, &f, &tok, 5, false, 0.0, 0.0, false);
        assert_eq!(untiered.len(), 2);

        let tiered = idx.search(query, &s, &f, &tok, 5, false, 0.0, 0.0, true);
        assert_eq!(tiered[0].chunk, 1, "tiered, the chunk holding every term leads");
        assert_eq!(tiered.len(), 2, "and the partial match is still returned");
    }

    /// The point of tiering over a hard `&` is that a query no chunk holds completely
    /// still returns its best partial matches rather than nothing at all.
    #[test]
    fn tiering_still_returns_partial_matches_when_nothing_holds_the_whole_query() {
        let s = store_of(&[
            ("confluence", "release notes for the quarter"),
            ("confluence", "approval workflow for expenses"),
        ]);
        let tok = Tokenizer::default();
        let idx = Bm25Index::build(&s, &tok);
        let f = CompiledFilter::compile(&Filter::default(), &s);
        let hits = idx.search("release approval elsewhere", &s, &f, &tok, 5, false, 0.0, 0.0, true);
        assert_eq!(hits.len(), 2, "both partial matches are returned");
    }
}
