//! Exhaustive search. This is the accuracy reference the whole project is graded
//! against, so it is deliberately the dullest code here: scan every chunk that
//! passes the filter, keep the best k. No index, nothing approximate, nothing to
//! get subtly wrong.
//!
//! It is also a genuine query path, not only a test fixture. When a predicate is
//! selective enough, scanning the passing set is both faster and exact, so the
//! vector index falls back to it rather than walking a graph.
//!
//! Invariant: **this is the answer every approximate path is graded against,
//! so it is exhaustive by construction.** It scans every chunk the filter
//! passes and keeps the best k. Nothing here is skipped, bounded early or
//! approximated; a shortcut in the reference is a shortcut in every recall
//! number measured against it.

use rayon::prelude::*;

#[cfg(test)]
use crate::distance::Metric;
use crate::filter::CompiledFilter;
use crate::store::Store;
use crate::vectors::{Scorer, VectorSet};

/// Below this many candidates the scan runs on one thread. Splitting a few
/// thousand dot products across cores costs more in coordination than it saves.
const PARALLEL_ABOVE: usize = 4_096;

#[derive(Debug, Clone, Copy, PartialEq)]
/// One result of a search: which chunk, and how far from the query it is.
pub struct Neighbour {
    /// The chunk identifier.
    pub chunk: u32,
    /// The distance under the set's own metric. Smaller is nearer.
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
    // **A filed set is scanned in blocks, not one vector at a time.** Scoring
    // through `Scorer::distance` is one positional read per chunk, and a scan of a
    // 601,862 chunk corpus is 601,862 of them - which is the cost of the read
    // syscall, not of the dot product. Reading 2,730 at a time makes it the
    // sequential read it should be. A predicate selective enough for the store to
    // list its own candidates goes the other way, because those ordinals are
    // scattered and there is no block to read.
    if !vectors.is_resident() && filter.candidate_chunks(store).is_none() {
        return streaming_search(vectors, store, filter, query, k);
    }
    search_with(vectors, store, filter, query, k)
}

/// Exhaustive top k over a set whose vectors are read from a file, block by block.
///
/// The blocks are independent, so they are scanned across cores the way the
/// resident scan is, and each thread reads its own block positionally - which is
/// why the file is read rather than seeked: a shared cursor could not be split.
///
/// @param vectors - the filed vector set
/// @param store - the chunk metadata the filter reads
/// @param filter - the compiled predicate
/// @param query - the query vector, in the form this set's metric expects -
///   already normalized, under cosine
/// @param k - how many neighbours to return
fn streaming_search(
    vectors: &VectorSet,
    store: &Store,
    filter: &CompiledFilter,
    query: &[f32],
    k: usize,
) -> Vec<Neighbour> {
    if k == 0 || filter.is_dead() {
        return Vec::new();
    }
    let trivial = filter.is_trivial();
    let n = store.n_chunks().min(vectors.len());
    let dims = vectors.dims();
    let block = vectors.block_len();
    if n == 0 || dims == 0 || block == 0 {
        return Vec::new();
    }
    let blocks = n.div_ceil(block);

    (0..blocks)
        .into_par_iter()
        .fold(
            || (TopK::new(k), vec![0f32; block * dims]),
            |(mut top, mut buffer), nth| {
                let first = nth * block;
                let read = vectors.read_block(first as u32, &mut buffer);
                for at in 0..read {
                    let chunk = (first + at) as u32;
                    if !trivial && !filter.passes(chunk, store) {
                        continue;
                    }
                    let start = at.saturating_mul(dims);
                    let Some(stored) = buffer.get(start..start.saturating_add(dims)) else {
                        continue;
                    };
                    // Was a hardcoded `1.0 - dot(stored, query)`, which is
                    // cosine distance whatever this set's metric actually is.
                    // `distance_of` reads the metric this set was constructed
                    // with, the same one `Scorer::distance` (used by every
                    // other path here) reads.
                    top.push(Neighbour {
                        chunk,
                        distance: vectors.distance_of(stored, query),
                    });
                }
                (top, buffer)
            },
        )
        .map(|(top, _)| top)
        .reduce(|| TopK::new(k), TopK::merged)
        .finish()
}

/// Exhaustive top k using an arbitrary scorer, so the exhaustive path is
/// available over int8 codes as well as full precision vectors.
///
/// The candidates are reduced to the best `k` as they are produced rather than
/// collected and sorted. On a source predicate that is the difference between
/// sorting every passing chunk and sorting `k` per thread: the slack predicate
/// passes 17,642 chunks and only 50 are wanted, so a full sort spent most of its
/// time ordering rows nobody would ever read.
/// @param scorer - full precision vectors, or the int8 codes
/// @param store - the chunk metadata the filter reads
/// @param filter - the compiled predicate
/// @param query - the query vector, in the form the scorer's metric expects
/// @param k - how many neighbours to return
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
    let score = |chunk: u32| Neighbour {
        chunk,
        distance: scorer.distance(chunk, query),
    };

    // When the predicate names sources, the store can list their chunks, so the
    // scan visits those instead of testing every chunk in the corpus. The
    // predicate is still applied to each candidate, so a filter that also
    // constrains labels or a timestamp stays correct.
    if let Some(groups) = filter.candidate_chunks(store) {
        let total: usize = groups.iter().map(|g| g.len()).sum();
        if total > PARALLEL_ABOVE {
            // Parallelise over the chunks, not over the groups. A single source
            // predicate produces exactly one group, so splitting the work by
            // group leaves the whole scan on one thread. Measured: doing it by
            // group took 5.08 ms on the slack predicate where doing it by chunk
            // takes 1.67 ms.
            return groups
                .par_iter()
                .flat_map(|g| g.par_iter().copied())
                .filter(|chunk| filter.passes(*chunk, store))
                .map(score)
                .fold(|| TopK::new(k), TopK::pushed)
                .reduce(|| TopK::new(k), TopK::merged)
                .finish();
        }
        let mut top = TopK::new(k);
        for chunk in groups.iter().flat_map(|g| g.iter().copied()) {
            if filter.passes(chunk, store) {
                top.push(score(chunk));
            }
        }
        return top.finish();
    }

    // The scan is the query path for a selective predicate, not only a test
    // reference, so it is worth spreading across cores. Every chunk is
    // independent, and the ordering below restores a single deterministic order,
    // so parallelism changes the speed and not the answer.
    if n > PARALLEL_ABOVE {
        return (0..n as u32)
            .into_par_iter()
            .filter(|chunk| trivial || filter.passes(*chunk, store))
            .map(score)
            .fold(|| TopK::new(k), TopK::pushed)
            .reduce(|| TopK::new(k), TopK::merged)
            .finish();
    }
    let mut top = TopK::new(k);
    for chunk in 0..n as u32 {
        if !trivial && !filter.passes(chunk, store) {
            continue;
        }
        top.push(score(chunk));
    }
    top.finish()
}

/// The best `k` neighbours seen so far, in the order the scan wants them out:
/// ascending distance, ascending chunk identifier on a tie.
///
/// A max heap keyed on that order would be the textbook structure. This keeps a
/// buffer instead and reduces it when it fills, because the cut off after the
/// first reduction rejects almost every later candidate with a single float
/// comparison, and the occasional `select_nth_unstable` is linear. It also gives
/// the same answer whatever order the candidates arrive in, which matters: the
/// parallel scan does not fix the order, and two runs disagreeing on equally
/// distant chunks would read as a recall difference that is really a coincidence.
struct TopK {
    k: usize,
    /// Filled to `2k` before being reduced back to `k`, so the reduction is paid
    /// once per `k` candidates rather than once per candidate.
    buffer: Vec<Neighbour>,
    /// The worst distance currently kept, once the buffer has overflowed at least
    /// once. Everything worse than this is rejected without being stored.
    cutoff: f32,
    full: bool,
}

/// Ascending distance, ascending chunk identifier on a tie.
fn nearer(a: &Neighbour, b: &Neighbour) -> std::cmp::Ordering {
    a.distance
        .partial_cmp(&b.distance)
        .unwrap_or(std::cmp::Ordering::Equal)
        .then(a.chunk.cmp(&b.chunk))
}

impl TopK {
    fn new(k: usize) -> TopK {
        TopK {
            k,
            buffer: Vec::with_capacity(2 * k.max(1)),
            cutoff: f32::INFINITY,
            full: false,
        }
    }

    fn push(&mut self, n: Neighbour) {
        // Equal to the cutoff is still kept: the tie break is on the chunk
        // identifier, and a lower identifier at the same distance wins.
        if self.full && n.distance > self.cutoff {
            return;
        }
        self.buffer.push(n);
        if self.buffer.len() >= 2 * self.k.max(1) {
            self.reduce();
        }
    }

    /// `push`, by value, for `Iterator::fold`.
    fn pushed(mut self, n: Neighbour) -> TopK {
        self.push(n);
        self
    }

    fn reduce(&mut self) {
        if self.buffer.len() <= self.k {
            return;
        }
        self.buffer.select_nth_unstable_by(self.k - 1, nearer);
        self.buffer.truncate(self.k);
        self.cutoff = self
            .buffer
            .iter()
            .map(|n| n.distance)
            .fold(f32::NEG_INFINITY, f32::max);
        self.full = true;
    }

    fn merged(mut self, other: TopK) -> TopK {
        for n in other.buffer {
            self.push(n);
        }
        self
    }

    fn finish(mut self) -> Vec<Neighbour> {
        self.buffer.sort_by(nearer);
        self.buffer.truncate(self.k);
        self.buffer
    }
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
                external_chunk_id: None,
                labels: vec![],
                attributes: Vec::new(),
                flags: Vec::new(),
                deleted: false,
            });
        }
        store.add_chunks(inputs).expect("the chunks are added");
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
        let query = vs.copy_of(7);
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
        let query = vs.copy_of(7);
        let hits = search(&vs, &store, &f, &query, 10);
        assert!(!hits.is_empty());
        for h in &hits {
            assert_eq!(
                store.documents[store.chunks[h.chunk as usize].doc as usize].source,
                store.sources.get("slack").unwrap()
            );
        }
    }

    #[test]
    fn k_larger_than_the_passing_set_returns_the_whole_set() {
        let (vs, store) = fixture(10);
        let f = CompiledFilter::compile(&Filter::source("slack"), &store);
        let hits = search(&vs, &store, &f, &vs.copy_of(0), 1000);
        assert_eq!(hits.len(), f.pass_count());
    }

    #[test]
    fn a_dead_filter_returns_nothing() {
        let (vs, store) = fixture(10);
        let f = CompiledFilter::compile(&Filter::source("nowhere"), &store);
        assert!(search(&vs, &store, &f, &vs.copy_of(0), 10).is_empty());
    }

    #[test]
    fn k_zero_returns_nothing() {
        let (vs, store) = fixture(10);
        let f = CompiledFilter::compile(&Filter::default(), &store);
        assert!(search(&vs, &store, &f, &vs.copy_of(0), 0).is_empty());
    }

    /// The parallel path and the single threaded path must return the identical
    /// list, or the exhaustive reference stops being a reference.
    #[test]
    fn the_parallel_and_sequential_paths_agree() {
        // Larger than PARALLEL_ABOVE, so the parallel branch is taken.
        let (vs, store) = fixture(6_000);
        let f = CompiledFilter::compile(&Filter::default(), &store);
        let query = vs.copy_of(11);
        let parallel = search(&vs, &store, &f, &query, 50);
        assert_eq!(parallel.len(), 50);
        // Recompute the same answer the slow, obvious way.
        let mut expected: Vec<Neighbour> = (0..vs.len() as u32)
            .map(|chunk| Neighbour {
                chunk,
                distance: vs.distance(chunk, &query),
            })
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
        let q = vs.copy_of(3);
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
            assert!(
                f.candidate_chunks(&store).is_some(),
                "expected a narrowed scan"
            );
            let q = vs.copy_of(29);
            let narrowed = search(&vs, &store, &f, &q, 30);

            // The same answer computed without the narrowing.
            let mut expected: Vec<Neighbour> = (0..vs.len() as u32)
                .filter(|c| f.passes(*c, &store))
                .map(|chunk| Neighbour {
                    chunk,
                    distance: vs.distance(chunk, &q),
                })
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
            &Filter {
                source: Some("slack".into()),
                updated_after: Some(3_000),
                ..Default::default()
            },
            &store,
        );
        let hits = search(&vs, &store, &f, &vs.copy_of(1), 100);
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
                external_chunk_id: None,
                labels: vec![],
                attributes: Vec::new(),
                flags: Vec::new(),
                deleted: false,
            });
        }
        store.add_chunks(inputs).expect("the chunks are added");
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

    /// The reduce-as-you-go top k has to agree with sorting everything, including
    /// on ties, or a speed change would read as a recall change on the score card.
    #[test]
    fn top_k_agrees_with_sorting_every_candidate() {
        use rand::rngs::StdRng;
        use rand::{Rng, SeedableRng};
        let mut rng = StdRng::seed_from_u64(7);
        for k in [1usize, 3, 17, 50] {
            for n in [0usize, 1, 5, 300, 5000] {
                // Distances drawn from a small set, so ties are common rather than rare.
                let candidates: Vec<Neighbour> = (0..n as u32)
                    .map(|chunk| Neighbour {
                        chunk,
                        distance: rng.gen_range(0..7) as f32 * 0.25,
                    })
                    .collect();

                let mut sorted = candidates.clone();
                sorted.sort_by(nearer);
                sorted.truncate(k);

                let mut top = TopK::new(k);
                for c in &candidates {
                    top.push(*c);
                }
                assert_eq!(top.finish(), sorted, "k={k} n={n}");
            }
        }
    }

    /// Merging two partial results is the parallel scan's reduction step, and it has
    /// to give the same answer as one thread having seen everything.
    #[test]
    fn merging_two_partial_results_matches_one_pass() {
        let candidates: Vec<Neighbour> = (0..200u32)
            .map(|chunk| Neighbour {
                chunk,
                distance: ((chunk * 37) % 100) as f32 / 100.0,
            })
            .collect();
        let k = 10;
        let mut whole = TopK::new(k);
        for c in &candidates {
            whole.push(*c);
        }
        let mut left = TopK::new(k);
        for c in &candidates[..90] {
            left.push(*c);
        }
        let mut right = TopK::new(k);
        for c in &candidates[90..] {
            right.push(*c);
        }
        assert_eq!(left.merged(right).finish(), whole.finish());
    }

    /// A filed set answers exactly what the resident one answers, on every filter shape.
    ///
    /// **This is the claim the whole option rests on.** Leaving the vectors in the file is only
    /// acceptable if it changes what a search costs and nothing about what it returns, and the two
    /// paths through `search` are different code: one scores through `Scorer::distance` and the other
    /// reads blocks. So the test is equality of the whole result, chunk for chunk and distance for
    /// distance, rather than a recall figure that would hide a reordering.
    ///
    /// The corpus is deliberately larger than one block, so the streaming path actually reads more
    /// than one, and larger than `PARALLEL_ABOVE`, so the resident path is the parallel one.
    #[test]
    fn a_filed_set_returns_what_the_resident_set_returns() {
        let (resident, store) = fixture(6_000);
        let directory = std::env::temp_dir().join(format!(
            "inillucent-flat-filed-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::create_dir_all(&directory);
        let path = directory.join("vectors.raw");
        std::fs::write(
            &path,
            bytemuck::cast_slice(resident.raw().expect("the fixture is resident")),
        )
        .expect("the vectors are written");
        let filed = VectorSet::from_file(
            resident.dims(),
            resident.metric(),
            resident.len(),
            std::fs::File::open(&path).expect("it opens"),
            0,
        );
        assert!(!filed.is_resident());
        assert!(
            filed.len() > filed.block_len(),
            "the scan has to cross a block boundary"
        );

        let query: Vec<f32> = (0..8).map(|d| ((d as f32) * 0.7).cos()).collect();
        for (name, filter) in [
            (
                "no predicate",
                CompiledFilter::compile(&Filter::default(), &store),
            ),
            (
                "a source predicate",
                CompiledFilter::compile(&Filter::source("slack"), &store),
            ),
        ] {
            for k in [1usize, 10, 50] {
                let left = search(&resident, &store, &filter, &query, k);
                let right = search(&filed, &store, &filter, &query, k);
                assert_eq!(
                    left.len(),
                    right.len(),
                    "{name}, k={k}: the same number of neighbours"
                );
                assert_eq!(
                    left, right,
                    "{name}, k={k}: the same neighbours in the same order"
                );
            }
        }
        let _ = std::fs::remove_file(&path);
    }

    /// Pins the defect the streaming path had: it computed `1.0 - dot(stored,
    /// query)` unconditionally, which is cosine distance whatever metric the
    /// set was actually built under. A filed, unfiltered, L2 set is exactly
    /// the combination that reaches `streaming_search` rather than
    /// `search_with`, and the two vectors here are picked so cosine and L2
    /// disagree about which is nearer - the case that proves the metric was
    /// actually read rather than merely not crashing.
    #[test]
    fn a_filed_l2_set_streams_through_the_l2_metric_not_cosine() {
        let dims = 4usize;
        let mut resident = VectorSet::with_metric(dims, Metric::L2);
        let mut store = Store::default();
        store
            .add_chunks(vec![
                ChunkInput {
                    source: "s".into(),
                    external_doc_id: "0".into(),
                    content: "0".into(),
                    ..Default::default()
                },
                ChunkInput {
                    source: "s".into(),
                    external_doc_id: "1".into(),
                    content: "1".into(),
                    ..Default::default()
                },
            ])
            .expect("the chunks are added");
        // Aligned with the query but twice as far; close to the query but
        // slightly off axis. Cosine prefers chunk 0, L2 prefers chunk 1.
        resident.push(&[2.0, 0.0, 0.0, 0.0]);
        resident.push(&[0.9, 0.1, 0.0, 0.0]);
        let query = [1.0f32, 0.0, 0.0, 0.0];

        let directory = std::env::temp_dir().join(format!(
            "inillucent-flat-l2-streaming-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::create_dir_all(&directory);
        let path = directory.join("vectors.raw");
        std::fs::write(
            &path,
            bytemuck::cast_slice(resident.raw().expect("resident")),
        )
        .expect("written");
        // No candidate list for this filter, and not resident: exactly the
        // condition `search` routes to `streaming_search` rather than the
        // `Scorer`-driven `search_with`.
        let filed = VectorSet::from_file(
            dims,
            Metric::L2,
            resident.len(),
            std::fs::File::open(&path).expect("opens"),
            0,
        );
        let filter = CompiledFilter::compile(&Filter::default(), &store);

        let hits = search(&filed, &store, &filter, &query, 1);
        assert_eq!(
            hits.first().map(|n| n.chunk),
            Some(1),
            "the streaming path answered by cosine, not by the L2 metric this set was built under"
        );
        let _ = std::fs::remove_file(&path);
    }
}
