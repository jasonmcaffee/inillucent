//! Exhaustive search. This is the accuracy reference the whole project is graded
//! against, so it is deliberately the dullest code here: scan every chunk that
//! passes the filter, keep the best k. No index, nothing approximate, nothing to
//! get subtly wrong.
//!
//! It is also a genuine query path, not only a test fixture. When a predicate is
//! selective enough, scanning the passing set is both faster and exact, so the
//! vector index falls back to it rather than walking a graph.

use rayon::prelude::*;

use crate::filter::CompiledFilter;
use crate::store::Store;
use crate::vectors::{Scorer, VectorSet};

/// Below this many candidates the scan runs on one thread. Splitting a few
/// thousand dot products across cores costs more in coordination than it saves.
const PARALLEL_ABOVE: usize = 4_096;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Neighbour {
    pub chunk: u32,
    pub distance: f32,
}

/// Exhaustive top k over the chunks passing `filter`.
///
/// Ties are broken by ascending chunk identifier so the result is deterministic;
/// without that, two runs can disagree on equally distant chunks and the harness
/// would report a recall difference that is really an ordering coincidence.
pub fn search(
    vectors: &VectorSet,
    store: &Store,
    filter: &CompiledFilter,
    query: &[f32],
    k: usize,
) -> Vec<Neighbour> {
    search_with(vectors, store, filter, query, k)
}

/// Exhaustive top k using an arbitrary scorer, so the exhaustive path is
/// available over int8 codes as well as full precision vectors.
pub fn search_with<S: Scorer + Sync>(
    scorer: &S,
    store: &Store,
    filter: &CompiledFilter,
    query: &[f32],
    k: usize,
) -> Vec<Neighbour> {
    if k == 0 || filter.is_dead() {
        return Vec::new();
    }
    let trivial = filter.is_trivial();
    let n = store.n_chunks();

    // When the predicate names sources, the store can list their chunks, so the
    // scan visits those instead of testing every chunk in the corpus. The
    // predicate is still applied to each candidate, so a filter that also
    // constrains labels or a timestamp stays correct.
    if let Some(groups) = filter.candidate_chunks(store) {
        let total: usize = groups.iter().map(|g| g.len()).sum();
        let mut all: Vec<Neighbour> = if total > PARALLEL_ABOVE {
            // Parallelise over the chunks, not over the groups. A single source
            // predicate produces exactly one group, so splitting the work by
            // group leaves the whole scan on one thread. Measured: doing it by
            // group took 5.08 ms on the slack predicate where doing it by chunk
            // takes 1.67 ms.
            groups
                .par_iter()
                .flat_map(|g| g.par_iter().copied())
                .filter(|chunk| filter.passes(*chunk, store))
                .map(|chunk| Neighbour {
                    chunk,
                    distance: scorer.distance(chunk, query),
                })
                .collect()
        } else {
            let mut v = Vec::with_capacity(total);
            for chunk in groups.iter().flat_map(|g| g.iter().copied()) {
                if filter.passes(chunk, store) {
                    v.push(Neighbour {
                        chunk,
                        distance: scorer.distance(chunk, query),
                    });
                }
            }
            v
        };
        all.sort_by(|a, b| {
            a.distance
                .partial_cmp(&b.distance)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(a.chunk.cmp(&b.chunk))
        });
        all.truncate(k);
        return all;
    }

    // The scan is the query path for a selective predicate, not only a test
    // reference, so it is worth spreading across cores. Every chunk is
    // independent, and the sort below restores a single deterministic order, so
    // parallelism changes the speed and not the answer.
    let mut all: Vec<Neighbour> = if n > PARALLEL_ABOVE {
        (0..n as u32)
            .into_par_iter()
            .filter(|chunk| trivial || filter.passes(*chunk, store))
            .map(|chunk| Neighbour {
                chunk,
                distance: scorer.distance(chunk, query),
            })
            .collect()
    } else {
        let mut v: Vec<Neighbour> = Vec::with_capacity(filter.pass_count());
        for chunk in 0..n as u32 {
            if !trivial && !filter.passes(chunk, store) {
                continue;
            }
            v.push(Neighbour {
                chunk,
                distance: scorer.distance(chunk, query),
            });
        }
        v
    };

    all.sort_by(|a, b| {
        a.distance
            .partial_cmp(&b.distance)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.chunk.cmp(&b.chunk))
    });
    all.truncate(k);
    all
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::filter::Filter;
    use crate::store::ChunkInput;

    fn fixture(n: usize) -> (VectorSet, Store) {
        let mut vs = VectorSet::new(8);
        let mut store = Store::default();
        let mut inputs = Vec::new();
        for i in 0..n {
            inputs.push(ChunkInput {
                source: if i % 3 == 0 { "slack" } else { "confluence" }.to_string(),
                external_doc_id: format!("d{i}"),
                chunk_index: 0,
                heading_path: vec![],
                content: format!("chunk {i}"),
                title: format!("t{i}"),
                url: format!("u{i}"),
                space_key: None,
                author: None,
                author_id: None,
                updated_at: Some(i as i64),
                labels: vec![],
                deleted: false,
            });
        }
        store.add_chunks(inputs);
        for i in 0..n {
            let v: Vec<f32> = (0..8).map(|d| ((i * 8 + d) as f32 * 0.31).sin()).collect();
            vs.push(&v);
        }
        (vs, store)
    }

    #[test]
    fn returns_the_nearest_first() {
        let (vs, store) = fixture(50);
        let f = CompiledFilter::compile(&Filter::default(), &store);
        let query = vs.get(7).to_vec();
        let hits = search(&vs, &store, &f, &query, 5);
        assert_eq!(hits[0].chunk, 7, "a vector is its own nearest neighbour");
        assert!(hits[0].distance.abs() < 1e-5);
        for w in hits.windows(2) {
            assert!(w[0].distance <= w[1].distance, "distances must ascend");
        }
    }

    #[test]
    fn honours_the_filter() {
        let (vs, store) = fixture(50);
        let f = CompiledFilter::compile(&Filter::source("slack"), &store);
        let query = vs.get(7).to_vec();
        let hits = search(&vs, &store, &f, &query, 10);
        assert!(!hits.is_empty());
        for h in &hits {
            assert_eq!(store.documents[store.chunks[h.chunk as usize].doc as usize].source,
                       store.sources.get("slack").unwrap());
        }
    }

    #[test]
    fn k_larger_than_the_passing_set_returns_the_whole_set() {
        let (vs, store) = fixture(10);
        let f = CompiledFilter::compile(&Filter::source("slack"), &store);
        let hits = search(&vs, &store, &f, &vs.get(0).to_vec(), 1000);
        assert_eq!(hits.len(), f.pass_count());
    }

    #[test]
    fn a_dead_filter_returns_nothing() {
        let (vs, store) = fixture(10);
        let f = CompiledFilter::compile(&Filter::source("nowhere"), &store);
        assert!(search(&vs, &store, &f, &vs.get(0).to_vec(), 10).is_empty());
    }

    #[test]
    fn k_zero_returns_nothing() {
        let (vs, store) = fixture(10);
        let f = CompiledFilter::compile(&Filter::default(), &store);
        assert!(search(&vs, &store, &f, &vs.get(0).to_vec(), 0).is_empty());
    }

    /// The parallel path and the single threaded path must return the identical
    /// list, or the exhaustive reference stops being a reference.
    #[test]
    fn the_parallel_and_sequential_paths_agree() {
        // Larger than PARALLEL_ABOVE, so the parallel branch is taken.
        let (vs, store) = fixture(6_000);
        let f = CompiledFilter::compile(&Filter::default(), &store);
        let query = vs.get(11).to_vec();
        let parallel = search(&vs, &store, &f, &query, 50);
        assert_eq!(parallel.len(), 50);
        // Recompute the same answer the slow, obvious way.
        let mut expected: Vec<Neighbour> = (0..vs.len() as u32)
            .map(|chunk| Neighbour { chunk, distance: vs.distance(chunk, &query) })
            .collect();
        expected.sort_by(|a, b| {
            a.distance
                .partial_cmp(&b.distance)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(a.chunk.cmp(&b.chunk))
        });
        expected.truncate(50);
        assert_eq!(
            parallel.iter().map(|n| n.chunk).collect::<Vec<_>>(),
            expected.iter().map(|n| n.chunk).collect::<Vec<_>>()
        );
    }

    #[test]
    fn repeated_parallel_scans_return_the_same_order() {
        let (vs, store) = fixture(6_000);
        let f = CompiledFilter::compile(&Filter::default(), &store);
        let q = vs.get(3).to_vec();
        let a = search(&vs, &store, &f, &q, 40);
        let b = search(&vs, &store, &f, &q, 40);
        assert_eq!(
            a.iter().map(|n| n.chunk).collect::<Vec<_>>(),
            b.iter().map(|n| n.chunk).collect::<Vec<_>>()
        );
    }

    /// The narrowed scan must return exactly what the full scan returns, or the
    /// exhaustive reference stops being a reference.
    #[test]
    fn the_source_narrowed_scan_agrees_with_a_full_scan() {
        let (vs, store) = fixture(6_000);
        for source in ["slack", "confluence"] {
            let f = CompiledFilter::compile(&Filter::source(source), &store);
            assert!(f.candidate_chunks(&store).is_some(), "expected a narrowed scan");
            let q = vs.get(29).to_vec();
            let narrowed = search(&vs, &store, &f, &q, 30);

            // The same answer computed without the narrowing.
            let mut expected: Vec<Neighbour> = (0..vs.len() as u32)
                .filter(|c| f.passes(*c, &store))
                .map(|chunk| Neighbour { chunk, distance: vs.distance(chunk, &q) })
                .collect();
            expected.sort_by(|a, b| {
                a.distance
                    .partial_cmp(&b.distance)
                    .unwrap_or(std::cmp::Ordering::Equal)
                    .then(a.chunk.cmp(&b.chunk))
            });
            expected.truncate(30);
            assert_eq!(
                narrowed.iter().map(|n| n.chunk).collect::<Vec<_>>(),
                expected.iter().map(|n| n.chunk).collect::<Vec<_>>(),
                "disagreement for {source}"
            );
        }
    }

    /// Narrowing by source must not skip the other predicates.
    #[test]
    fn a_narrowed_scan_still_applies_the_rest_of_the_predicate() {
        let (vs, store) = fixture(6_000);
        let f = CompiledFilter::compile(
            &Filter { source: Some("slack".into()), updated_after: Some(3_000), ..Default::default() },
            &store,
        );
        let hits = search(&vs, &store, &f, &vs.get(1).to_vec(), 100);
        assert!(!hits.is_empty());
        for h in &hits {
            assert!(f.passes(h.chunk, &store));
            let doc = store.chunks[h.chunk as usize].doc;
            assert!(store.documents[doc as usize].updated_at >= 3_000);
        }
    }

    #[test]
    fn ties_break_deterministically_by_chunk_id() {
        // Every vector identical, so every distance is equal.
        let mut vs = VectorSet::new(4);
        let mut store = Store::default();
        let mut inputs = Vec::new();
        for i in 0..10 {
            inputs.push(ChunkInput {
                source: "confluence".into(),
                external_doc_id: format!("d{i}"),
                chunk_index: 0,
                heading_path: vec![],
                content: "same".into(),
                title: "t".into(),
                url: "u".into(),
                space_key: None,
                author: None,
                author_id: None,
                updated_at: None,
                labels: vec![],
                deleted: false,
            });
        }
        store.add_chunks(inputs);
        for _ in 0..10 {
            vs.push(&[1.0, 0.0, 0.0, 0.0]);
        }
        let f = CompiledFilter::compile(&Filter::default(), &store);
        let hits = search(&vs, &store, &f, &[1.0, 0.0, 0.0, 0.0], 3);
        assert_eq!(
            hits.iter().map(|h| h.chunk).collect::<Vec<_>>(),
            vec![0, 1, 2]
        );
    }
}
