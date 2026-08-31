//! Hierarchical Navigable Small World graph, with traversal that honours a
//! predicate instead of filtering after the fact.
//!
//! The build follows the original algorithm and uses the same parameters as the
//! pgvector (`m = 16`, `ef_construction = 64`) so the comparison is fair.
//!
//! The search is where this differs, and it is the whole point of the project.
//! pgvector filters after the index scan, so a query restricted to a minority
//! source gets whatever survives from `ef_search` globally nearest candidates,
//! which measured as zero rows for slack and jira on the real corpus. Here a node
//! that fails the predicate is still *expanded*, because it is a useful stepping
//! stone through the graph, but it does not *enter the results*. The traversal
//! therefore keeps walking until it has found `ef` chunks that actually pass,
//! which is the behaviour pgvector bolts on afterwards as `hnsw.iterative_scan`.

use std::cmp::Ordering;
use std::collections::BinaryHeap;

use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};

use crate::filter::CompiledFilter;
use crate::flat::{self, Neighbour};
use crate::store::Store;
use crate::vectors::{Scorer, VectorSet};

#[derive(Debug, Clone, Copy)]
pub struct HnswParams {
    /// Maximum connections per node per layer above layer zero. Layer zero uses
    /// `2 * m`, as in the original algorithm.
    pub m: usize,
    pub ef_construction: usize,
    /// Default candidate breadth at query time.
    pub ef_search: usize,
    pub seed: u64,
    /// A hard floor: a filter admitting fewer than this many chunks always scans
    /// exhaustively. Above it the cost model in `prefers_exhaustive` decides.
    /// Setting it to zero leaves the decision entirely to the cost model, which
    /// is what the graph's own tests do so they exercise the traversal.
    pub exhaustive_below: usize,
}

impl Default for HnswParams {
    fn default() -> Self {
        HnswParams {
            m: 16,
            ef_construction: 64,
            ef_search: 64,
            seed: 0x5eed_1234,
            exhaustive_below: 1_000,
        }
    }
}

/// A candidate ordered so that `BinaryHeap` yields the *nearest* first.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Nearest {
    distance: f32,
    node: u32,
}
impl Eq for Nearest {}
impl Ord for Nearest {
    fn cmp(&self, other: &Self) -> Ordering {
        // Reversed on distance so the max-heap behaves as a min-heap.
        other
            .distance
            .partial_cmp(&self.distance)
            .unwrap_or(Ordering::Equal)
            .then(other.node.cmp(&self.node))
    }
}
impl PartialOrd for Nearest {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// A candidate ordered so that `BinaryHeap` yields the *furthest* first, which is
/// what a bounded result set needs in order to evict.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Furthest {
    distance: f32,
    node: u32,
}
impl Eq for Furthest {}
impl Ord for Furthest {
    fn cmp(&self, other: &Self) -> Ordering {
        self.distance
            .partial_cmp(&other.distance)
            .unwrap_or(Ordering::Equal)
            .then(self.node.cmp(&other.node))
    }
}
impl PartialOrd for Furthest {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

pub struct Hnsw {
    params: HnswParams,
    /// Set by tests that need the traversal exercised regardless of what the cost
    /// model would choose. Production code leaves this alone.
    force_graph: bool,
    /// `layers[l][node]` is the neighbour list of `node` at layer `l`. A node
    /// absent from a layer has an empty list.
    layers: Vec<Vec<Vec<u32>>>,
    /// Highest layer each node occupies.
    node_top: Vec<u8>,
    entry: Option<u32>,
    rng: StdRng,
    level_factor: f64,
}

impl Hnsw {
    pub fn new(params: HnswParams) -> Self {
        let level_factor = 1.0 / (params.m as f64).ln();
        Hnsw {
            params,
            force_graph: false,
            layers: vec![Vec::new()],
            node_top: Vec::new(),
            entry: None,
            rng: StdRng::seed_from_u64(params.seed),
            level_factor,
        }
    }

    /// Always walk the graph, never route to an exhaustive scan. Only the graph's
    /// own accuracy tests use this: measuring traversal accuracy against
    /// exhaustive search is meaningless if the call silently became one.
    pub fn force_graph_traversal(&mut self) {
        self.force_graph = true;
    }

    /// Turn forced traversal on or off after the graph is built.
    pub fn set_force_graph(&mut self, on: bool) {
        self.force_graph = on;
    }

    pub fn params(&self) -> &HnswParams {
        &self.params
    }

    pub fn len(&self) -> usize {
        self.node_top.len()
    }

    pub fn is_empty(&self) -> bool {
        self.node_top.is_empty()
    }

    pub fn n_layers(&self) -> usize {
        self.layers.len()
    }

    /// Total directed edges, used by the score card to report graph footprint.
    pub fn edge_count(&self) -> usize {
        self.layers
            .iter()
            .map(|l| l.iter().map(|n| n.len()).sum::<usize>())
            .sum()
    }

    /// Write the adjacency lists, the per node top layer and the entry point.
    ///
    /// The graph is the one structure worth storing rather than rebuilding: the
    /// lexical index and the int8 codes are deterministic functions of data
    /// already on disk, but rebuilding the graph costs minutes on a corpus this
    /// size.
    pub fn write_graph(&self, w: &mut impl std::io::Write) -> std::io::Result<()> {
        w.write_all(&(self.layers.len() as u32).to_le_bytes())?;
        w.write_all(&(self.node_top.len() as u32).to_le_bytes())?;
        w.write_all(&self.entry.unwrap_or(u32::MAX).to_le_bytes())?;
        w.write_all(&self.node_top)?;
        for layer in &self.layers {
            w.write_all(&(layer.len() as u32).to_le_bytes())?;
            for neighbours in layer {
                w.write_all(&(neighbours.len() as u32).to_le_bytes())?;
                for n in neighbours {
                    w.write_all(&n.to_le_bytes())?;
                }
            }
        }
        Ok(())
    }

    /// Read a graph written by `write_graph`.
    pub fn read_graph(r: &mut impl std::io::Read, params: HnswParams) -> std::io::Result<Hnsw> {
        let mut buf4 = [0u8; 4];
        r.read_exact(&mut buf4)?;
        let n_layers = u32::from_le_bytes(buf4) as usize;
        r.read_exact(&mut buf4)?;
        let n_nodes = u32::from_le_bytes(buf4) as usize;
        r.read_exact(&mut buf4)?;
        let raw_entry = u32::from_le_bytes(buf4);
        let entry = if raw_entry == u32::MAX { None } else { Some(raw_entry) };

        let mut node_top = vec![0u8; n_nodes];
        r.read_exact(&mut node_top)?;

        let mut layers = Vec::with_capacity(n_layers);
        for _ in 0..n_layers {
            r.read_exact(&mut buf4)?;
            let count = u32::from_le_bytes(buf4) as usize;
            let mut layer = Vec::with_capacity(count);
            for _ in 0..count {
                r.read_exact(&mut buf4)?;
                let degree = u32::from_le_bytes(buf4) as usize;
                let mut neighbours = Vec::with_capacity(degree);
                for _ in 0..degree {
                    r.read_exact(&mut buf4)?;
                    neighbours.push(u32::from_le_bytes(buf4));
                }
                layer.push(neighbours);
            }
            layers.push(layer);
        }

        let level_factor = 1.0 / (params.m as f64).ln();
        Ok(Hnsw {
            params,
            force_graph: false,
            layers,
            node_top,
            entry,
            rng: StdRng::seed_from_u64(params.seed),
            level_factor,
        })
    }

    /// Whether scanning the passing set beats walking the graph.
    ///
    /// Exhaustive search costs one distance computation per passing chunk. A
    /// filtered walk has to visit roughly `ef / selectivity` nodes before it finds
    /// `ef` that pass, and each visit expands about `2m` neighbours, so it costs
    /// roughly `ef * 2m / selectivity` distance computations. Scanning wins when
    ///
    /// ```text
    /// pass_count  <  ef * 2m * n_chunks / pass_count
    /// ```
    ///
    /// which rearranges to `pass_count < sqrt(ef * 2m * n_chunks)`.
    ///
    /// This replaces a fixed threshold. A constant tuned for one corpus is wrong
    /// for a corpus ten times the size, and wrong again when the caller raises
    /// `ef`; both appear in the formula, so the crossover moves with them. On this
    /// corpus at `ef = 128` it lands near 27,700 chunks, which puts slack, jira,
    /// figma and miro on the exact path and leaves confluence and github on the
    /// graph.
    pub fn prefers_exhaustive(&self, pass_count: usize, n_chunks: usize, ef: usize) -> bool {
        if self.force_graph {
            return false;
        }
        if pass_count <= self.params.exhaustive_below {
            return true;
        }
        let budget = (ef as f64) * (self.max_degree(0) as f64) * (n_chunks as f64);
        (pass_count as f64) < budget.sqrt()
    }

    fn max_degree(&self, layer: usize) -> usize {
        if layer == 0 {
            self.params.m * 2
        } else {
            self.params.m
        }
    }

    fn random_level(&mut self) -> usize {
        let r: f64 = self.rng.gen_range(f64::MIN_POSITIVE..1.0);
        (-r.ln() * self.level_factor).floor() as usize
    }

    /// Insert every vector in `vectors`, in ordinal order. Node identifiers are
    /// the vector ordinals, which the index keeps aligned with chunk identifiers.
    pub fn build(&mut self, vectors: &VectorSet) {
        for id in 0..vectors.len() as u32 {
            self.insert(vectors, id);
        }
    }

    pub fn insert(&mut self, vectors: &VectorSet, node: u32) {
        let level = self.random_level();

        while self.layers.len() <= level {
            self.layers.push(Vec::new());
        }
        for layer in self.layers.iter_mut() {
            while layer.len() <= node as usize {
                layer.push(Vec::new());
            }
        }
        while self.node_top.len() <= node as usize {
            self.node_top.push(0);
        }
        self.node_top[node as usize] = level as u8;

        let query = vectors.get(node).to_vec();

        let Some(entry) = self.entry else {
            self.entry = Some(node);
            return;
        };

        // Greedy descent through the layers above the new node's own level.
        let mut current = entry;
        let top = self.node_top[entry as usize] as usize;
        for layer in (level + 1..=top).rev() {
            current = self.greedy_descend(vectors, &query, current, layer);
        }

        // At each layer the node occupies, find neighbours and link both ways.
        let start_layer = level.min(top);
        for layer in (0..=start_layer).rev() {
            let candidates =
                self.search_layer_unfiltered(vectors, &query, current, layer, self.params.ef_construction);
            let selected = self.select_neighbours(vectors, &candidates, self.max_degree(layer));

            self.layers[layer][node as usize] = selected.clone();
            for neighbour in selected {
                self.link(vectors, neighbour, node, layer);
            }
            if let Some(best) = self.layers[layer][node as usize].first() {
                current = *best;
            }
        }

        if level > self.node_top[entry as usize] as usize {
            self.entry = Some(node);
        }
    }

    /// Add `to` to `from`'s neighbour list, pruning back to the degree cap with
    /// the same heuristic used when selecting neighbours. Pruning by the
    /// heuristic rather than by plain distance is what keeps the graph navigable
    /// instead of collapsing into clusters.
    fn link(&mut self, vectors: &VectorSet, from: u32, to: u32, layer: usize) {
        let cap = self.max_degree(layer);
        let list = &mut self.layers[layer][from as usize];
        if list.contains(&to) {
            return;
        }
        list.push(to);
        if list.len() <= cap {
            return;
        }
        let from_vec = vectors.get(from).to_vec();
        let mut candidates: Vec<Nearest> = self.layers[layer][from as usize]
            .iter()
            .map(|n| Nearest {
                distance: vectors.distance(*n, &from_vec),
                node: *n,
            })
            .collect();
        candidates.sort_by(|a, b| {
            a.distance
                .partial_cmp(&b.distance)
                .unwrap_or(Ordering::Equal)
                .then(a.node.cmp(&b.node))
        });
        let pruned = self.select_neighbours(vectors, &candidates, cap);
        self.layers[layer][from as usize] = pruned;
    }

    /// Neighbour selection heuristic: keep a candidate only if it is closer to
    /// the query than to any candidate already kept. This spreads edges across
    /// directions instead of piling them onto one dense cluster, which is what
    /// makes long range hops possible.
    fn select_neighbours(
        &self,
        vectors: &VectorSet,
        candidates: &[Nearest],
        cap: usize,
    ) -> Vec<u32> {
        let mut sorted: Vec<Nearest> = candidates.to_vec();
        sorted.sort_by(|a, b| {
            a.distance
                .partial_cmp(&b.distance)
                .unwrap_or(Ordering::Equal)
                .then(a.node.cmp(&b.node))
        });

        let mut kept: Vec<u32> = Vec::with_capacity(cap);
        for c in sorted {
            if kept.len() >= cap {
                break;
            }
            let cv = vectors.get(c.node);
            let closer_to_query_than_to_kept = kept
                .iter()
                .all(|k| c.distance < vectors.distance(*k, cv));
            if closer_to_query_than_to_kept {
                kept.push(c.node);
            }
        }
        // If the heuristic was too strict to fill the budget, top up by distance
        // so the node is not left underconnected.
        if kept.len() < cap {
            for c in candidates {
                if kept.len() >= cap {
                    break;
                }
                if !kept.contains(&c.node) {
                    kept.push(c.node);
                }
            }
        }
        kept
    }

    fn greedy_descend<S: Scorer + Sync>(
        &self,
        vectors: &S,
        query: &[f32],
        start: u32,
        layer: usize,
    ) -> u32 {
        let mut current = start;
        let mut current_distance = vectors.distance(current, query);
        loop {
            let mut improved = false;
            for n in &self.layers[layer][current as usize] {
                let d = vectors.distance(*n, query);
                if d < current_distance {
                    current_distance = d;
                    current = *n;
                    improved = true;
                }
            }
            if !improved {
                return current;
            }
        }
    }

    /// Layer search with no predicate, used during construction.
    fn search_layer_unfiltered<S: Scorer + Sync>(
        &self,
        vectors: &S,
        query: &[f32],
        entry: u32,
        layer: usize,
        ef: usize,
    ) -> Vec<Nearest> {
        let mut visited = vec![false; self.layers[layer].len()];
        let mut frontier: BinaryHeap<Nearest> = BinaryHeap::new();
        let mut results: BinaryHeap<Furthest> = BinaryHeap::new();

        let d = vectors.distance(entry, query);
        visited[entry as usize] = true;
        frontier.push(Nearest { distance: d, node: entry });
        results.push(Furthest { distance: d, node: entry });

        while let Some(candidate) = frontier.pop() {
            let worst = results.peek().map(|f| f.distance).unwrap_or(f32::MAX);
            if candidate.distance > worst && results.len() >= ef {
                break;
            }
            for n in &self.layers[layer][candidate.node as usize] {
                if visited[*n as usize] {
                    continue;
                }
                visited[*n as usize] = true;
                let nd = vectors.distance(*n, query);
                let worst = results.peek().map(|f| f.distance).unwrap_or(f32::MAX);
                if results.len() < ef || nd < worst {
                    frontier.push(Nearest { distance: nd, node: *n });
                    results.push(Furthest { distance: nd, node: *n });
                    if results.len() > ef {
                        results.pop();
                    }
                }
            }
        }

        let mut out: Vec<Nearest> = results
            .into_iter()
            .map(|f| Nearest { distance: f.distance, node: f.node })
            .collect();
        out.sort_by(|a, b| {
            a.distance
                .partial_cmp(&b.distance)
                .unwrap_or(Ordering::Equal)
                .then(a.node.cmp(&b.node))
        });
        out
    }

    /// Search layer zero honouring a predicate.
    ///
    /// Two rules, and the difference between them is the entire fix:
    ///   * every visited node is expanded, whether or not it passes;
    ///   * only passing nodes are admitted to the results.
    ///
    /// The stopping condition counts admitted results, so a selective predicate
    /// forces the walk to continue rather than returning a short list.
    #[allow(clippy::too_many_arguments)]
    fn search_layer_filtered<S: Scorer + Sync>(
        &self,
        vectors: &S,
        store: &Store,
        filter: &CompiledFilter,
        query: &[f32],
        entry: u32,
        ef: usize,
        max_visits: usize,
    ) -> Vec<Nearest> {
        let layer = 0;
        let mut visited = vec![false; self.layers[layer].len()];
        let mut frontier: BinaryHeap<Nearest> = BinaryHeap::new();
        let mut results: BinaryHeap<Furthest> = BinaryHeap::new();
        let mut visits = 0usize;

        let d = vectors.distance(entry, query);
        visited[entry as usize] = true;
        frontier.push(Nearest { distance: d, node: entry });
        if filter.passes(entry, store) {
            results.push(Furthest { distance: d, node: entry });
        }

        while let Some(candidate) = frontier.pop() {
            if visits >= max_visits {
                break;
            }
            let worst = results.peek().map(|f| f.distance).unwrap_or(f32::MAX);
            // Only stop early once enough *passing* nodes have been admitted.
            if results.len() >= ef && candidate.distance > worst {
                break;
            }
            visits += 1;

            for n in &self.layers[layer][candidate.node as usize] {
                if visited[*n as usize] {
                    continue;
                }
                visited[*n as usize] = true;
                let nd = vectors.distance(*n, query);
                let worst = results.peek().map(|f| f.distance).unwrap_or(f32::MAX);
                let passes = filter.passes(*n, store);

                // Expand regardless of the predicate: a failing node is a
                // stepping stone. Refusing to walk through it is what
                // disconnects the reachable set and collapses accuracy.
                if results.len() < ef || nd < worst || !passes {
                    frontier.push(Nearest { distance: nd, node: *n });
                }
                if passes && (results.len() < ef || nd < worst) {
                    results.push(Furthest { distance: nd, node: *n });
                    if results.len() > ef {
                        results.pop();
                    }
                }
            }
        }

        let mut out: Vec<Nearest> = results
            .into_iter()
            .map(|f| Nearest { distance: f.distance, node: f.node })
            .collect();
        out.sort_by(|a, b| {
            a.distance
                .partial_cmp(&b.distance)
                .unwrap_or(Ordering::Equal)
                .then(a.node.cmp(&b.node))
        });
        out
    }

    /// Top k nearest chunks passing `filter`.
    ///
    /// Routes to an exhaustive scan when the predicate is selective, because at
    /// that size scanning is exact and cheaper than walking.
    pub fn search(
        &self,
        vectors: &VectorSet,
        store: &Store,
        filter: &CompiledFilter,
        query: &[f32],
        k: usize,
        ef_search: Option<usize>,
    ) -> Vec<Neighbour> {
        self.search_with(vectors, store, filter, query, k, ef_search)
    }

    /// Search using `scorer` for every comparison during traversal. Passing the
    /// int8 codes here is what makes the quantized pass real: the walk itself
    /// runs on the compressed representation, and the caller rescores the
    /// survivors with full precision.
    pub fn search_with<S: Scorer + Sync>(
        &self,
        scorer: &S,
        store: &Store,
        filter: &CompiledFilter,
        query: &[f32],
        k: usize,
        ef_search: Option<usize>,
    ) -> Vec<Neighbour> {
        if k == 0 || filter.is_dead() || self.is_empty() {
            return Vec::new();
        }
        let ef = ef_search.unwrap_or(self.params.ef_search).max(k);
        if self.prefers_exhaustive(filter.pass_count(), store.n_chunks(), ef) {
            return flat::search_with(scorer, store, filter, query, k);
        }
        let Some(entry) = self.entry else {
            return Vec::new();
        };

        let mut current = entry;
        let top = self.node_top[entry as usize] as usize;
        for layer in (1..=top).rev() {
            current = self.greedy_descend(scorer, query, current, layer);
        }

        // A visit budget keeps a pathologically selective predicate from walking
        // the whole graph. It is generous relative to `ef` because the cost of
        // stopping early is a wrong answer, and the exhaustive path above
        // already covers the genuinely small cases.
        let max_visits = (ef * 64).max(4096);

        let found = if filter.is_trivial() {
            self.search_layer_unfiltered(scorer, query, current, 0, ef)
        } else {
            self.search_layer_filtered(scorer, store, filter, query, current, ef, max_visits)
        };

        found
            .into_iter()
            .take(k)
            .map(|n| Neighbour { chunk: n.node, distance: n.distance })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::filter::Filter;
    use crate::store::ChunkInput;

    /// Clustered vectors, so nearest neighbours are meaningful rather than
    /// uniform noise, and a source label that cuts across the clusters.
    fn fixture(n: usize, dims: usize) -> (VectorSet, Store) {
        let mut rng = StdRng::seed_from_u64(42);
        let mut vs = VectorSet::new(dims);
        let mut store = Store::default();
        let mut inputs = Vec::new();
        let sources = ["confluence", "github", "slack", "jira", "figma", "miro"];
        for i in 0..n {
            // Weight the distribution the way the real corpus is weighted, so
            // the minority sources are genuinely rare.
            let source = match i % 20 {
                0..=9 => sources[0],
                10..=14 => sources[1],
                15..=16 => sources[2],
                17 => sources[3],
                18 => sources[4],
                _ => sources[5],
            };
            inputs.push(ChunkInput {
                source: source.to_string(),
                external_doc_id: format!("d{}", i / 3),
                chunk_index: (i % 3) as u32,
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

        let n_clusters = 24;
        let centres: Vec<Vec<f32>> = (0..n_clusters)
            .map(|_| (0..dims).map(|_| rng.gen_range(-1.0..1.0)).collect())
            .collect();
        for i in 0..n {
            let c = &centres[i % n_clusters];
            let v: Vec<f32> = c
                .iter()
                .map(|x| x + rng.gen_range(-0.35..0.35))
                .collect();
            vs.push(&v);
        }
        (vs, store)
    }

    fn recall(approx: &[Neighbour], exact: &[Neighbour]) -> f32 {
        if exact.is_empty() {
            return 1.0;
        }
        let want: std::collections::HashSet<u32> = exact.iter().map(|n| n.chunk).collect();
        let hit = approx.iter().filter(|n| want.contains(&n.chunk)).count();
        hit as f32 / exact.len() as f32
    }

    #[test]
    fn unfiltered_recall_is_high_against_exhaustive_search() {
        let (vs, store) = fixture(4000, 32);
        let mut g = Hnsw::new(HnswParams { exhaustive_below: 0, ..Default::default() });
        g.force_graph_traversal();
        g.build(&vs);
        let f = CompiledFilter::compile(&Filter::default(), &store);

        let mut total = 0.0;
        let trials = 40;
        for t in 0..trials {
            let q = vs.get((t * 97 % 4000) as u32).to_vec();
            let exact = flat::search(&vs, &store, &f, &q, 10);
            let approx = g.search(&vs, &store, &f, &q, 10, Some(128));
            total += recall(&approx, &exact);
        }
        let mean = total / trials as f32;
        assert!(mean > 0.95, "unfiltered recall@10 was {mean}, expected > 0.95");
    }

    #[test]
    fn a_vector_finds_itself() {
        let (vs, store) = fixture(2000, 32);
        let mut g = Hnsw::new(HnswParams { exhaustive_below: 0, ..Default::default() });
        g.force_graph_traversal();
        g.build(&vs);
        let f = CompiledFilter::compile(&Filter::default(), &store);
        for id in [0u32, 17, 512, 1999] {
            let q = vs.get(id).to_vec();
            let hits = g.search(&vs, &store, &f, &q, 5, Some(128));
            assert_eq!(hits[0].chunk, id, "node {id} did not find itself");
        }
    }

    /// The defect this project exists to fix. A minority source must still
    /// return a full result set, not the empty list pgvector produces.
    #[test]
    fn a_selective_source_filter_still_fills_the_result_set() {
        let (vs, store) = fixture(20000, 32);
        let mut g = Hnsw::new(HnswParams { exhaustive_below: 0, ..Default::default() });
        g.force_graph_traversal();
        g.build(&vs);

        for source in ["slack", "jira", "figma", "miro"] {
            let f = CompiledFilter::compile(&Filter::source(source), &store);
            assert!(f.pass_count() >= 50, "fixture too small for {source}");
            let q = vs.get(123).to_vec();
            let hits = g.search(&vs, &store, &f, &q, 50, Some(64));
            assert_eq!(
                hits.len(),
                50,
                "{source} returned {} of 50 requested",
                hits.len()
            );
        }
    }

    #[test]
    fn filtered_recall_is_high_against_exhaustive_search_within_the_filter() {
        let (vs, store) = fixture(20000, 32);
        let mut g = Hnsw::new(HnswParams { exhaustive_below: 0, ..Default::default() });
        g.force_graph_traversal();
        g.build(&vs);

        for source in ["confluence", "github", "slack", "jira"] {
            let f = CompiledFilter::compile(&Filter::source(source), &store);
            let mut total = 0.0;
            let trials = 20;
            for t in 0..trials {
                let q = vs.get((t * 313 % 20000) as u32).to_vec();
                let exact = flat::search(&vs, &store, &f, &q, 10);
                let approx = g.search(&vs, &store, &f, &q, 10, Some(256));
                total += recall(&approx, &exact);
            }
            let mean = total / trials as f32;
            assert!(
                mean > 0.90,
                "{source} filtered recall@10 was {mean}, expected > 0.90"
            );
        }
    }

    #[test]
    fn every_returned_chunk_satisfies_the_predicate() {
        let (vs, store) = fixture(5000, 32);
        let mut g = Hnsw::new(HnswParams { exhaustive_below: 0, ..Default::default() });
        g.force_graph_traversal();
        g.build(&vs);
        let f = CompiledFilter::compile(&Filter::source("slack"), &store);
        let slack = store.sources.get("slack").unwrap();
        let hits = g.search(&vs, &store, &f, &vs.get(7).to_vec(), 40, Some(128));
        assert!(!hits.is_empty());
        for h in hits {
            let doc = store.chunks[h.chunk as usize].doc;
            assert_eq!(store.documents[doc as usize].source, slack);
        }
    }

    #[test]
    fn a_selective_filter_routes_to_exhaustive_search_and_is_exact() {
        let (vs, store) = fixture(5000, 32);
        let mut g = Hnsw::new(HnswParams::default()); // exhaustive_below = 8000
        g.build(&vs);
        let f = CompiledFilter::compile(&Filter::source("jira"), &store);
        let q = vs.get(11).to_vec();
        let exact = flat::search(&vs, &store, &f, &q, 10);
        let got = g.search(&vs, &store, &f, &q, 10, None);
        assert_eq!(
            got.iter().map(|n| n.chunk).collect::<Vec<_>>(),
            exact.iter().map(|n| n.chunk).collect::<Vec<_>>()
        );
    }

    #[test]
    fn search_is_deterministic_across_repeated_calls() {
        let (vs, store) = fixture(3000, 32);
        let mut g = Hnsw::new(HnswParams { exhaustive_below: 0, ..Default::default() });
        g.force_graph_traversal();
        g.build(&vs);
        let f = CompiledFilter::compile(&Filter::default(), &store);
        let q = vs.get(42).to_vec();
        let a = g.search(&vs, &store, &f, &q, 20, Some(64));
        let b = g.search(&vs, &store, &f, &q, 20, Some(64));
        assert_eq!(
            a.iter().map(|n| n.chunk).collect::<Vec<_>>(),
            b.iter().map(|n| n.chunk).collect::<Vec<_>>()
        );
    }

    /// The routing rule has to move with corpus size and with `ef`, which is the
    /// reason it is a formula and not a constant.
    #[test]
    fn the_cost_model_routes_selective_filters_to_exhaustive_search() {
        let g = Hnsw::new(HnswParams { exhaustive_below: 1_000, ..Default::default() });
        let n = 186_829;
        // sqrt(128 * 32 * 186781) is about 27,662.
        assert!(g.prefers_exhaustive(17_675, n, 128), "slack sized filter should scan");
        assert!(g.prefers_exhaustive(11_160, n, 128), "jira sized filter should scan");
        assert!(!g.prefers_exhaustive(47_525, n, 128), "github sized filter should walk");
        assert!(!g.prefers_exhaustive(93_617, n, 128), "confluence sized filter should walk");
    }

    #[test]
    fn the_crossover_moves_with_ef_and_with_corpus_size() {
        let g = Hnsw::new(HnswParams { exhaustive_below: 1_000, ..Default::default() });
        let n = 186_829;
        // A larger ef makes the walk more expensive, so scanning wins more often.
        assert!(!g.prefers_exhaustive(40_000, n, 128));
        assert!(g.prefers_exhaustive(40_000, n, 512));
        // A larger corpus makes the walk relatively cheaper per passing chunk.
        assert!(!g.prefers_exhaustive(30_000, n, 128));
        assert!(g.prefers_exhaustive(30_000, n * 10, 128));
    }

    #[test]
    fn the_hard_floor_always_scans() {
        let g = Hnsw::new(HnswParams { exhaustive_below: 5_000, ..Default::default() });
        assert!(g.prefers_exhaustive(4_999, 10_000_000, 16));
    }

    #[test]
    fn an_empty_graph_returns_nothing() {
        let vs = VectorSet::new(8);
        let store = Store::default();
        let g = Hnsw::new(HnswParams::default());
        let f = CompiledFilter::compile(&Filter::default(), &store);
        assert!(g.search(&vs, &store, &f, &[0.0; 8], 10, None).is_empty());
    }

    #[test]
    fn a_dead_filter_returns_nothing() {
        let (vs, store) = fixture(500, 16);
        let mut g = Hnsw::new(HnswParams { exhaustive_below: 0, ..Default::default() });
        g.force_graph_traversal();
        g.build(&vs);
        let f = CompiledFilter::compile(&Filter::source("sharepoint"), &store);
        assert!(g.search(&vs, &store, &f, &vs.get(0).to_vec(), 10, None).is_empty());
    }

    #[test]
    fn the_graph_has_more_than_one_layer_on_a_realistic_size() {
        let (vs, _store) = fixture(5000, 16);
        let mut g = Hnsw::new(HnswParams::default());
        g.build(&vs);
        assert!(g.n_layers() > 1, "expected a hierarchy, got {} layer(s)", g.n_layers());
        assert_eq!(g.len(), 5000);
    }
}
