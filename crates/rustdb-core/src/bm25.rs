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

#[derive(Debug, Clone, Copy)]
pub struct Posting {
    pub chunk: u32,
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

            let mut frequencies: HashMap<&str, u32> = HashMap::new();
            for t in &terms {
                *frequencies.entry(t.as_str()).or_insert(0) += 1;
            }
            for (term, tf) in frequencies {
                accumulator
                    .entry(term.to_string())
                    .or_default()
                    .push(Posting { chunk, term_frequency: tf });
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
    pub fn search(
        &self,
        query: &str,
        store: &Store,
        filter: &CompiledFilter,
        tokenizer: &Tokenizer,
        k: usize,
        prefix: bool,
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
        let mut scores: HashMap<u32, f32> = HashMap::new();

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
                    *scores.entry(p.chunk).or_insert(0.0) += weight * contribution;
                }
            }
        }

        let mut hits: Vec<LexicalHit> = scores
            .into_iter()
            .map(|(chunk, score)| LexicalHit { chunk, score })
            .collect();
        // Descending score, ascending chunk on a tie, so the order is stable.
        hits.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(a.chunk.cmp(&b.chunk))
        });
        hits.truncate(k);
        hits
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
        idx.search(query, store, &f, &tok, k, false)
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
        let hits = idx.search("offer", &s, &f, &tok, 2, false);
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
        let hits = idx.search("offer eligibility", &s, &f, &tok, 5, false);
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
        let hits = idx.search("eli", &s, &f, &tok, 5, true);
        assert_eq!(hits.len(), 1);
        assert!(idx.search("eli", &s, &f, &tok, 5, false).is_empty());
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
        let hits = idx.search("offer eligibility", &s, &f, &tok, 3, false);
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
}
