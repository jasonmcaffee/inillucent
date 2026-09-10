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
    ///
    /// Setting it to `usize::MAX` makes every search exact. That is not a silly
    /// setting: an exhaustive scan of 598,560 chunks measures 109 ms p50 in this
    /// engine, which is better at p95 than what a cold pgvector query costs, and
    /// it is the only option that is correct by construction.
    pub exhaustive_below: usize,
    /// How many places a layer-0 search starts from.
    ///
    /// One entry point is what the original algorithm specifies and what makes a
    /// near-duplicate hub dangerous: a corpus with 3,653 copies of the same
    /// automated alert collapses them into a node with an in-degree of 3,511
    /// against a median of 28, and a greedy descent that enters it does not come
    /// back out. Measured on the real mailbox, the true nearest chunk was not
    /// returned even when its own vector was the query. Extra seeds spread across
    /// the corpus mean one hub cannot swallow every walk, and they cost one
    /// descent each.
    pub entry_points: usize,
    /// Whether a node whose diversity heuristic could not fill its degree budget
    /// has the remainder topped up by plain distance.
    ///
    /// hnswlib calls this `keepPrunedConnections` and defaults it to false, for
    /// exactly the reason this corpus demonstrates: inside a near-duplicate
    /// cluster the nearest remaining candidates are all cluster-mates, so every
    /// edge that could have led out of the cluster is spent on a clone. Leaving it
    /// on under-uses the budget; turning it off under-connects. Which is better is
    /// a property of the corpus, so it is a setting and the score card measures it.
    pub keep_pruned_connections: bool,
    /// How many threads build the graph.
    ///
    /// 1 is the sequential build, and it is the default because it is
    /// reproducible: the same vectors and the same seed give the same graph, which
    /// is what lets a measurement be attributed to a setting rather than to a
    /// scheduling accident. Above 1 the insert loop runs on a thread pool with a
    /// lock per adjacency list, which is what hnswlib and FAISS do, and the graph
    /// it produces is valid but is not the sequential one.
    ///
    /// The number to weigh it against: 9 minutes 27 seconds on one core of
    /// twenty-four, for a corpus that is rebuilt whenever compaction runs.
    pub build_threads: usize,
}

impl Default for HnswParams {
    fn default() -> Self {
        HnswParams {
            m: 16,
            ef_construction: 64,
            ef_search: 64,
            seed: 0x5eed_1234,
            exhaustive_below: 1_000,
            entry_points: 1,
            keep_pruned_connections: true,
            build_threads: 1,
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

    /// Changes how many places a layer-0 search starts from, on a built graph.
    ///
    /// A query-time setting, not a build one: the adjacency lists are the same
    /// whatever this is. That distinction is what makes it measurable honestly -
    /// sweeping it needs one index rather than one per value, so the comparison
    /// cannot be contaminated by two builds differing for other reasons.
    /// @param count - how many starting points, at least one
    pub fn set_entry_points(&mut self, count: usize) {
        self.params.entry_points = count.max(1);
    }

    /// Changes the default candidate breadth on a built graph.
    /// @param ef - how many candidates a search keeps in flight
    pub fn set_ef_search(&mut self, ef: usize) {
        self.params.ef_search = ef.max(1);
    }

    /// Changes the size below which a filter always scans exhaustively.
    /// @param below - the chunk count; `usize::MAX` makes every search exact
    pub fn set_exhaustive_below(&mut self, below: usize) {
        self.params.exhaustive_below = below;
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

    /// Insert every vector in `vectors`. Node identifiers are the vector ordinals,
    /// which the index keeps aligned with chunk identifiers.
    ///
    /// Sequential and in ordinal order unless `build_threads` says otherwise; see
    /// `build_parallel` for what changes when it does.
    pub fn build(&mut self, vectors: &VectorSet) {
        if self.params.build_threads > 1 {
            self.build_parallel(vectors, self.params.build_threads);
            return;
        }
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

        let query = vectors.copy_of(node);

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
                self.search_layer_unfiltered(vectors, &query, &[current], layer, self.params.ef_construction);
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
        let from_vec = vectors.copy_of(from);
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
            let cv = vectors.copy_of(c.node);
            let closer_to_query_than_to_kept = kept
                .iter()
                .all(|k| c.distance < vectors.distance(*k, &cv));
            if closer_to_query_than_to_kept {
                kept.push(c.node);
            }
        }
        // If the heuristic was too strict to fill the budget, top up by distance
        // so the node is not left underconnected. Off, and the node stays
        // under-connected rather than spending its remaining edges on clones.
        if self.params.keep_pruned_connections && kept.len() < cap {
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
    ///
    /// Seeded from every node in `entries`, which is one node during construction
    /// and however many entry points are configured at query time.
    fn search_layer_unfiltered<S: Scorer + Sync>(
        &self,
        vectors: &S,
        query: &[f32],
        entries: &[u32],
        layer: usize,
        ef: usize,
    ) -> Vec<Nearest> {
        let mut visited = vec![false; self.layers[layer].len()];
        let mut frontier: BinaryHeap<Nearest> = BinaryHeap::new();
        let mut results: BinaryHeap<Furthest> = BinaryHeap::new();

        for entry in entries {
            if visited[*entry as usize] {
                continue;
            }
            let d = vectors.distance(*entry, query);
            visited[*entry as usize] = true;
            frontier.push(Nearest { distance: d, node: *entry });
            results.push(Furthest { distance: d, node: *entry });
        }
        while results.len() > ef {
            results.pop();
        }

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
        entries: &[u32],
        ef: usize,
        max_visits: usize,
        exhausted: &mut bool,
    ) -> Vec<Nearest> {
        let layer = 0;
        let mut visited = vec![false; self.layers[layer].len()];
        let mut frontier: BinaryHeap<Nearest> = BinaryHeap::new();
        let mut results: BinaryHeap<Furthest> = BinaryHeap::new();
        let mut visits = 0usize;

        for entry in entries {
            if visited[*entry as usize] {
                continue;
            }
            let d = vectors.distance(*entry, query);
            visited[*entry as usize] = true;
            frontier.push(Nearest { distance: d, node: *entry });
            if filter.passes(*entry, store) {
                results.push(Furthest { distance: d, node: *entry });
            }
        }
        while results.len() > ef {
            results.pop();
        }

        while let Some(candidate) = frontier.pop() {
            if visits >= max_visits {
                // The walk ran out of budget before it had `ef` passing nodes, so
                // what it is about to return is whatever it happened to reach.
                // The caller decides whether to trust that or scan.
                *exhausted = results.len() < ef;
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
        if self.entry.is_none() {
            return Vec::new();
        }

        let starts = self.entry_points(scorer, query);

        // A visit budget keeps a pathologically selective predicate from walking
        // the whole graph. It is generous relative to `ef` because the cost of
        // stopping early is a wrong answer, and the exhaustive path above
        // already covers the genuinely small cases.
        let max_visits = (ef * 64).max(4096);

        let mut exhausted = false;
        let found = if filter.is_trivial() {
            self.search_layer_unfiltered(scorer, query, &starts, 0, ef)
        } else {
            self.search_layer_filtered(
                scorer, store, filter, query, &starts, ef, max_visits, &mut exhausted,
            )
        };

        // A walk that spent its whole budget and still could not fill `ef` did not
        // answer the question; it reported where it happened to stop. Scanning the
        // passing set is exact and, on this corpus, costs about what the walk just
        // spent. This is sound rather than heuristic: it fires only when the
        // traversal has already told us it failed.
        if exhausted && found.len() < k {
            return flat::search_with(scorer, store, filter, query, k);
        }

        found
            .into_iter()
            .take(k)
            .map(|n| Neighbour { chunk: n.node, distance: n.distance })
            .collect()
    }

    /// Where a layer-0 search starts from.
    ///
    /// The original algorithm descends greedily from one entry point, keeping one
    /// node per layer, and hands layer 0 a single starting node. That is what makes
    /// a near-duplicate hub dangerous: this mailbox holds 3,653 copies of one
    /// automated alert, they collapse into a node with an in-degree of 3,511
    /// against a median of 28, and a walk that enters does not come back out -
    /// because once the result set is full of clones, the frontier only admits
    /// candidates nearer than the clones, so every path out of the cluster is
    /// closed.
    ///
    /// Two things widen the start, and they fail differently, which is why both:
    ///
    ///   * **The last upper layer is searched rather than descended.** Layer 1
    ///     holds roughly a sixteenth of the nodes and its edges are the long-range
    ///     ones, so its best few results are query-relevant *and* spread across
    ///     basins. This is nearly free: a search over 37,000 nodes at `ef` equal to
    ///     the seed count.
    ///   * **Stratified ordinals.** Query-independent, so they cannot be captured
    ///     by whatever captured the descent. Node identifiers are chunk
    ///     identifiers and a corpus arrives in some meaningful order - for a
    ///     mailbox, chronological - so even strides are even in whatever the corpus
    ///     is organised by.
    ///
    /// Deterministic either way, so the same query on the same index answers the
    /// same twice.
    /// @param scorer - what compares a stored vector to the query
    /// @param query - the query vector
    fn entry_points<S: Scorer + Sync>(&self, scorer: &S, query: &[f32]) -> Vec<u32> {
        let Some(entry) = self.entry else {
            return Vec::new();
        };
        let wanted = self.params.entry_points.max(1);
        let mut starts: Vec<u32> = Vec::with_capacity(wanted * 2);

        // Descend to layer 1, then search it instead of taking its single best.
        let mut current = entry;
        let top = self.node_top[entry as usize] as usize;
        for layer in (2..=top).rev() {
            current = self.greedy_descend(scorer, query, current, layer);
        }
        if top >= 1 && wanted > 1 {
            for candidate in
                self.search_layer_unfiltered(scorer, query, &[current], 1, wanted)
            {
                starts.push(candidate.node);
            }
        } else if top >= 1 {
            starts.push(self.greedy_descend(scorer, query, current, 1));
        } else {
            starts.push(current);
        }

        for seed in self.stratified_seeds(wanted.saturating_sub(1)) {
            if !starts.contains(&seed) {
                starts.push(seed);
            }
        }
        starts
    }

    /// Nodes at even ordinal strides across the corpus, as query-independent
    /// starting points.
    /// @param count - how many to return
    fn stratified_seeds(&self, count: usize) -> Vec<u32> {
        let n = self.node_top.len();
        if count == 0 || n == 0 {
            return Vec::new();
        }
        let stride = n / (count + 1);
        if stride == 0 {
            return Vec::new();
        }
        (1..=count)
            .map(|i| (i * stride) as u32)
            .filter(|node| (*node as usize) < n)
            .collect()
    }
}


/// One layer's adjacency lists during a parallel build, each behind its own lock.
///
/// A lock per node rather than one over the graph: the whole point is that two
/// threads inserting into different regions never contend, and on this corpus
/// they almost never do. hnswlib and FAISS both build this way.
type LockedLayer = Vec<std::sync::RwLock<Vec<u32>>>;

/// The smallest batch [`Hnsw::insert_batch`] will insert in parallel.
///
/// Moving the layers into locks and back costs one move per node per layer over
/// the whole graph, however few nodes are being added, so a batch of ten into a
/// graph of six hundred thousand would spend far more on the representation than
/// on the inserts. Above this the inserts dominate.
const PARALLEL_INSERT_FLOOR: u32 = 256;

impl Hnsw {
    /// Insert every vector using `build_threads` threads.
    ///
    /// The sequential build is `for id in 0..n { insert(id) }` on one core, and it
    /// measured at 9 minutes 27 seconds for 598,560 vectors on a 24-core machine.
    /// Every distance computation inside an insert is independent; the graph is
    /// the only shared mutable state, and it is shared one adjacency list at a
    /// time.
    ///
    /// **This does not produce the same graph as the sequential build.** Levels are
    /// drawn from the same seeded generator so they are identical, but the order in
    /// which nodes link to each other is whatever the thread pool produced, and
    /// neighbour selection depends on who was already there. Both graphs are valid
    /// and measure the same on recall; neither is byte-comparable to the other. That
    /// is why this is a setting rather than the default: a caller that wants a
    /// reproducible index leaves `build_threads` at 1, and a caller rebuilding a
    /// 598,560-node index nightly does not care.
    /// @param vectors - every vector, indexed by node
    fn build_parallel(&mut self, vectors: &VectorSet, threads: usize) {
        let n = vectors.len();
        if n == 0 {
            return;
        }
        // Levels first, on one thread, so the level distribution is exactly the
        // one the seed describes however many threads then insert.
        let levels: Vec<usize> = (0..n).map(|_| self.random_level()).collect();
        let max_level = levels.iter().copied().max().unwrap_or(0);

        // The entry point is the highest node, chosen up front. The sequential
        // build discovers it as it goes, which it cannot do when insertion order
        // is not a total order.
        let entry = levels
            .iter()
            .enumerate()
            .max_by_key(|(i, l)| (**l, std::cmp::Reverse(*i)))
            .map(|(i, _)| i as u32)
            .unwrap_or(0);
        let entry_level = levels[entry as usize];

        let layers: Vec<LockedLayer> = (0..=max_level)
            .map(|_| (0..n).map(|_| std::sync::RwLock::new(Vec::new())).collect())
            .collect();

        let insert_one = |node: usize| {
            if node as u32 == entry {
                return;
            }
            let query = vectors.copy_of(node as u32);
            let level = levels[node];

            let mut current = entry;
            for layer in (level + 1..=entry_level).rev() {
                current = greedy_descend_locked(&layers[layer], vectors, &query, current);
            }
            for layer in (0..=level.min(entry_level)).rev() {
                let candidates = search_layer_locked(
                    &layers[layer],
                    vectors,
                    &query,
                    current,
                    self.params.ef_construction,
                );
                let selected = self.select_neighbours(vectors, &candidates, self.max_degree(layer));
                // Merged into whatever is already there, never assigned over it.
                // Another thread inserting concurrently may already have linked
                // back to this node, and clearing the list erases that edge - the
                // other endpoint keeps its half, so the graph quietly loses a link
                // in one direction and gains a race nobody can reproduce.
                self.publish_neighbours(&layers[layer], vectors, node as u32, &selected, layer);
                for neighbour in &selected {
                    self.link_locked(&layers[layer], vectors, *neighbour, node as u32, layer);
                }
                if let Some(best) = selected.first() {
                    current = *best;
                }
            }
        };

        if threads <= 1 {
            (0..n).for_each(insert_one);
        } else {
            let pool = rayon::ThreadPoolBuilder::new()
                .num_threads(threads)
                .build()
                .expect("building the index thread pool");
            pool.install(|| {
                use rayon::prelude::*;
                (0..n).into_par_iter().for_each(insert_one);
            });
        }

        self.layers = layers
            .into_iter()
            .map(|layer| {
                layer
                    .into_iter()
                    .map(|cell| cell.into_inner().unwrap_or_default())
                    .collect()
            })
            .collect();
        self.node_top = levels.iter().map(|l| *l as u8).collect();
        self.entry = Some(entry);
    }

    /// Insert a contiguous range of new nodes, using `build_threads` threads.
    ///
    /// **What an append pays when the graph is already large (M8).**
    /// `insert` is one node on one core. That is the right shape for a handful
    /// of rows and the wrong shape for the batch a folded generation carries:
    /// the single-pass build this replaced ran on every core, so a fold of
    /// 1,337 nodes measured *slower* in wall clock than a rebuild of 10,699
    /// that used twenty-four of them. The work was eight times smaller and the
    /// wait was longer, which is not a fix.
    ///
    /// So a batch goes through the same locked layers `build_parallel` uses: a
    /// lock per adjacency list, neighbours merged rather than assigned so a
    /// concurrent insert's back edge is never erased. The existing graph is what
    /// the batch is inserted into; nothing already in it is rebuilt.
    ///
    /// **The graph this produces is not the graph the sequential loop produces.**
    /// Levels come from the same seeded generator, so the level distribution is
    /// identical, but which node links to which depends on what the pool got to
    /// first. Both are valid approximate graphs - the same property the parallel
    /// build has, and the reason it is a setting rather than the default.
    ///
    /// One thread, or a batch too small to be worth the locked representation,
    /// takes the sequential loop instead. The locked layers cost one move per
    /// node per layer over the *whole* graph, not over the batch, which is why
    /// there is a floor at all.
    /// @param vectors - every vector, indexed by node, including the new ones
    /// @param first - the first new node
    /// @param last - one past the last new node
    pub fn insert_batch(&mut self, vectors: &VectorSet, first: u32, last: u32) {
        let batch = last.saturating_sub(first);
        if self.params.build_threads <= 1
            || batch < PARALLEL_INSERT_FLOOR
            || self.entry.is_none()
        {
            for node in first..last {
                self.insert(vectors, node);
            }
            return;
        }
        let levels: Vec<usize> = (first..last).map(|_| self.random_level()).collect();
        self.make_room(last, levels.iter().copied().max().unwrap_or(0));
        for (offset, level) in levels.iter().enumerate() {
            let node = first as usize + offset;
            self.node_top[node] = *level as u8;
        }
        let entry = self.entry.unwrap_or(first);
        let entry_level = self.node_top[entry as usize] as usize;
        self.insert_range_locked(vectors, first, last, &levels, entry, entry_level);
        // The entry is promoted after the batch rather than during it, so every
        // node in the batch descends from the same place. A node promoted this
        // way has empty lists above the old entry's level, which is exactly what
        // `insert` leaves behind when it promotes one.
        if let Some((offset, _)) = levels
            .iter()
            .enumerate()
            .filter(|(_, level)| **level > entry_level)
            .max_by_key(|(offset, level)| (**level, std::cmp::Reverse(*offset)))
        {
            self.entry = Some(first + offset as u32);
        }
    }

    /// Grows the layers and the level table to hold nodes up to `last`.
    ///
    /// @param last - one past the highest node identifier the graph will hold
    /// @param level - the highest layer any new node occupies
    fn make_room(&mut self, last: u32, level: usize) {
        while self.layers.len() <= level {
            self.layers.push(Vec::new());
        }
        for layer in self.layers.iter_mut() {
            while layer.len() < last as usize {
                layer.push(Vec::new());
            }
        }
        while self.node_top.len() < last as usize {
            self.node_top.push(0);
        }
    }

    /// Inserts `first..last` into the locked representation of the layers.
    ///
    /// Split out of [`Hnsw::insert_batch`] so that the locking, the insertion
    /// and the unwrapping read as one thing each. The layers are moved into
    /// locks and moved back out, so the graph is never held twice.
    /// @param vectors - every vector, indexed by node
    /// @param first - the first new node
    /// @param last - one past the last new node
    /// @param levels - the level drawn for each new node, in order
    /// @param entry - the node every descent starts from
    /// @param entry_level - the layer that node occupies
    fn insert_range_locked(
        &mut self,
        vectors: &VectorSet,
        first: u32,
        last: u32,
        levels: &[usize],
        entry: u32,
        entry_level: usize,
    ) {
        let layers: Vec<LockedLayer> = std::mem::take(&mut self.layers)
            .into_iter()
            .map(|layer| layer.into_iter().map(std::sync::RwLock::new).collect())
            .collect();
        let insert_one = |node: u32| {
            if node == entry {
                return;
            }
            let query = vectors.copy_of(node);
            let level = levels.get((node - first) as usize).copied().unwrap_or(0);
            let mut current = entry;
            for layer in (level + 1..=entry_level).rev() {
                current = greedy_descend_locked(&layers[layer], vectors, &query, current);
            }
            for layer in (0..=level.min(entry_level)).rev() {
                let candidates = search_layer_locked(
                    &layers[layer],
                    vectors,
                    &query,
                    current,
                    self.params.ef_construction,
                );
                let selected = self.select_neighbours(vectors, &candidates, self.max_degree(layer));
                self.publish_neighbours(&layers[layer], vectors, node, &selected, layer);
                for neighbour in &selected {
                    self.link_locked(&layers[layer], vectors, *neighbour, node, layer);
                }
                if let Some(best) = selected.first() {
                    current = *best;
                }
            }
        };
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(self.params.build_threads)
            .build()
            .expect("building the index thread pool");
        pool.install(|| {
            use rayon::prelude::*;
            (first..last).into_par_iter().for_each(insert_one);
        });
        self.layers = layers
            .into_iter()
            .map(|layer| {
                layer
                    .into_iter()
                    .map(|cell| cell.into_inner().unwrap_or_default())
                    .collect()
            })
            .collect();
    }

    /// `link`, against the locked representation used during a parallel build.
    ///
    /// The same rule as `link`: add the edge, and if that overflows the degree cap
    /// prune with the diversity heuristic rather than by plain distance, because
    /// pruning by distance is what collapses a graph into clusters.
    ///
    /// Everything happens under one write lock. An earlier version dropped the lock
    /// to compute distances and took it again to write the result back, which is a
    /// read-modify-write across a gap: an edge another thread added in between was
    /// overwritten, and the "keep what arrived while we were pruning" loop could
    /// never keep anything because the pruned list was already at the cap. Holding
    /// the lock costs `cap` dot products - tens of microseconds - and the lock is
    /// per node, so two threads only ever contend on a node they both link to.
    fn link_locked(
        &self,
        layer_lists: &LockedLayer,
        vectors: &VectorSet,
        from: u32,
        to: u32,
        layer: usize,
    ) {
        let cap = self.max_degree(layer);
        let Ok(mut list) = layer_lists[from as usize].write() else {
            return;
        };
        if list.contains(&to) {
            return;
        }
        list.push(to);
        if list.len() <= cap {
            return;
        }
        *list = self.prune_to_cap(vectors, from, &list, cap);
    }

    /// Publishes a node's own neighbour list, merging rather than replacing.
    ///
    /// A node being inserted starts with an empty list, but by the time its search
    /// finishes another thread may already have linked back to it. Those reciprocal
    /// edges are real - the other endpoint is keeping its half - so they are merged
    /// with the selection and the union is pruned to the cap by the same heuristic
    /// that chose the selection.
    /// @param layer_lists - the layer being written
    /// @param vectors - the vectors, for the pruning heuristic
    /// @param node - the node whose list this is
    /// @param selected - what this node's own search chose
    /// @param layer - which layer, which sets the degree cap
    fn publish_neighbours(
        &self,
        layer_lists: &LockedLayer,
        vectors: &VectorSet,
        node: u32,
        selected: &[u32],
        layer: usize,
    ) {
        let cap = self.max_degree(layer);
        let Ok(mut list) = layer_lists[node as usize].write() else {
            return;
        };
        for candidate in selected {
            if !list.contains(candidate) {
                list.push(*candidate);
            }
        }
        if list.len() > cap {
            *list = self.prune_to_cap(vectors, node, &list, cap);
        }
    }

    /// Reduces one neighbour list to the degree cap with the diversity heuristic.
    /// @param vectors - the vectors, for the distances the heuristic reads
    /// @param node - the node the list belongs to
    /// @param list - the current neighbours, which may exceed the cap
    /// @param cap - the degree cap for this layer
    fn prune_to_cap(
        &self,
        vectors: &VectorSet,
        node: u32,
        list: &[u32],
        cap: usize,
    ) -> Vec<u32> {
        let node_vector = vectors.copy_of(node);
        let mut candidates: Vec<Nearest> = list
            .iter()
            .map(|n| Nearest { distance: vectors.distance(*n, &node_vector), node: *n })
            .collect();
        candidates.sort_by(|a, b| {
            a.distance
                .partial_cmp(&b.distance)
                .unwrap_or(Ordering::Equal)
                .then(a.node.cmp(&b.node))
        });
        self.select_neighbours(vectors, &candidates, cap)
    }
}

/// `greedy_descend`, against the locked representation.
fn greedy_descend_locked(
    layer_lists: &LockedLayer,
    vectors: &VectorSet,
    query: &[f32],
    start: u32,
) -> u32 {
    let mut current = start;
    let mut current_distance = vectors.distance(current, query);
    loop {
        let neighbours: Vec<u32> = match layer_lists[current as usize].read() {
            Ok(list) => list.clone(),
            Err(_) => return current,
        };
        let mut improved = false;
        for n in neighbours {
            let d = vectors.distance(n, query);
            if d < current_distance {
                current_distance = d;
                current = n;
                improved = true;
            }
        }
        if !improved {
            return current;
        }
    }
}

/// `search_layer_unfiltered`, against the locked representation.
fn search_layer_locked(
    layer_lists: &LockedLayer,
    vectors: &VectorSet,
    query: &[f32],
    entry: u32,
    ef: usize,
) -> Vec<Nearest> {
    // A set rather than a bitmap over every node: one construction search touches
    // a few hundred nodes, and a 598,560-entry allocation per insert would cost
    // more than the search.
    let mut visited: std::collections::HashSet<u32> = std::collections::HashSet::new();
    let mut frontier: BinaryHeap<Nearest> = BinaryHeap::new();
    let mut results: BinaryHeap<Furthest> = BinaryHeap::new();

    let d = vectors.distance(entry, query);
    visited.insert(entry);
    frontier.push(Nearest { distance: d, node: entry });
    results.push(Furthest { distance: d, node: entry });

    while let Some(candidate) = frontier.pop() {
        let worst = results.peek().map(|f| f.distance).unwrap_or(f32::MAX);
        if candidate.distance > worst && results.len() >= ef {
            break;
        }
        let neighbours: Vec<u32> = match layer_lists[candidate.node as usize].read() {
            Ok(list) => list.clone(),
            Err(_) => continue,
        };
        for n in neighbours {
            if !visited.insert(n) {
                continue;
            }
            let nd = vectors.distance(n, query);
            let worst = results.peek().map(|f| f.distance).unwrap_or(f32::MAX);
            if results.len() < ef || nd < worst {
                frontier.push(Nearest { distance: nd, node: n });
                results.push(Furthest { distance: nd, node: n });
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
                external_chunk_id: None,
                labels: vec![],
                attributes: Vec::new(),
                flags: Vec::new(),
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
            let q = vs.copy_of((t * 97 % 4000) as u32);
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
            let q = vs.copy_of(id);
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
            let q = vs.copy_of(123);
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
                let q = vs.copy_of((t * 313 % 20000) as u32);
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
        let hits = g.search(&vs, &store, &f, &vs.copy_of(7), 40, Some(128));
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
        let q = vs.copy_of(11);
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
        let q = vs.copy_of(42);
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
        assert!(g.search(&vs, &store, &f, &vs.copy_of(0), 10, None).is_empty());
    }

    #[test]
    fn the_graph_has_more_than_one_layer_on_a_realistic_size() {
        let (vs, _store) = fixture(5000, 16);
        let mut g = Hnsw::new(HnswParams::default());
        g.build(&vs);
        assert!(g.n_layers() > 1, "expected a hierarchy, got {} layer(s)", g.n_layers());
        assert_eq!(g.len(), 5000);
    }

    /// The whole point of a parallel build is that it produces a graph as good as
    /// the sequential one, in less wall clock. "As good" is measured against the
    /// exhaustive scan, because the two graphs are not the same graph.
    #[test]
    fn a_parallel_build_is_as_accurate_as_the_sequential_one() {
        let (vectors, store) = fixture(6000, 32);
        let base = HnswParams { exhaustive_below: 0, ..Default::default() };

        let mut sequential = Hnsw::new(base);
        sequential.force_graph_traversal();
        sequential.build(&vectors);

        let mut parallel = Hnsw::new(HnswParams { build_threads: 4, ..base });
        parallel.force_graph_traversal();
        parallel.build(&vectors);

        assert_eq!(parallel.len(), sequential.len());
        assert!(parallel.edge_count() > 0);

        let filter = CompiledFilter::compile(&Filter::default(), &store);
        let mut sequential_recall = 0.0;
        let mut parallel_recall = 0.0;
        for probe in [7u32, 300, 1500, 2900, 4400, 5100] {
            let q = vectors.copy_of(probe);
            let exact = flat::search(&vectors, &store, &filter, &q, 10);
            sequential_recall +=
                recall(&sequential.search(&vectors, &store, &filter, &q, 10, Some(128)), &exact);
            parallel_recall +=
                recall(&parallel.search(&vectors, &store, &filter, &q, 10, Some(128)), &exact);
        }
        sequential_recall /= 6.0;
        parallel_recall /= 6.0;
        assert!(
            parallel_recall >= sequential_recall - 0.05,
            "parallel recall {parallel_recall} against sequential {sequential_recall}"
        );
    }

    /// Every node must end up with a neighbour list and a level, whichever thread
    /// wrote it. A lost write shows up here as an isolated node.
    #[test]
    fn a_parallel_build_leaves_no_node_unlinked() {
        let (vectors, _) = fixture(4000, 16);
        let mut graph = Hnsw::new(HnswParams { build_threads: 8, exhaustive_below: 0, ..Default::default() });
        graph.build(&vectors);
        let isolated = (0..graph.len())
            .filter(|n| graph.layers[0][*n].is_empty())
            .count();
        assert_eq!(isolated, 0, "{isolated} nodes have no layer-0 neighbours");
    }

    /// A parallel build must not lose edges to a race.
    ///
    /// The failure this pins down is a read-modify-write across a dropped lock: a
    /// thread publishing its own neighbour list, or pruning another node's, would
    /// overwrite an edge a second thread had added in between. The other endpoint
    /// keeps its half, so the graph loses a link in one direction and nothing
    /// reports it. Edge count is the observable: a build that loses edges builds a
    /// measurably smaller graph, and repeating it under contention is what makes a
    /// rare race show up at all.
    #[test]
    fn a_parallel_build_does_not_lose_edges_to_a_race() {
        let (vectors, _) = fixture(8_000, 16);
        let base = HnswParams { exhaustive_below: 0, ..Default::default() };
        let mut sequential = Hnsw::new(base);
        sequential.build(&vectors);
        let expected = sequential.edge_count();

        for attempt in 0..3 {
            let mut parallel = Hnsw::new(HnswParams { build_threads: 16, ..base });
            parallel.build(&vectors);
            let edges = parallel.edge_count();
            assert!(
                edges as f64 >= expected as f64 * 0.97,
                "attempt {attempt} built {edges} edges against the sequential build's {expected}"
            );
        }
    }

    /// A reciprocal edge added by another thread has to survive the node's own
    /// publish, which is the specific write that used to clear the list.
    #[test]
    fn publishing_a_nodes_own_neighbours_keeps_edges_another_thread_added() {
        let (vectors, _) = fixture(200, 8);
        let graph = Hnsw::new(HnswParams::default());
        let layer: LockedLayer = (0..200).map(|_| std::sync::RwLock::new(Vec::new())).collect();

        // Another thread got there first and linked back to node 5.
        layer[5].write().unwrap().push(42);
        graph.publish_neighbours(&layer, &vectors, 5, &[7, 9, 11], 0);

        let list = layer[5].read().unwrap().clone();
        assert!(list.contains(&42), "the reciprocal edge was erased: {list:?}");
        for chosen in [7u32, 9, 11] {
            assert!(list.contains(&chosen), "the node's own choice {chosen} is missing: {list:?}");
        }
    }

    /// No adjacency list may exceed its degree cap, however many threads pruned it.
    #[test]
    fn a_parallel_build_respects_the_degree_cap() {
        let (vectors, _) = fixture(4000, 16);
        let params = HnswParams { build_threads: 8, exhaustive_below: 0, ..Default::default() };
        let mut graph = Hnsw::new(params);
        graph.build(&vectors);
        for (layer, lists) in graph.layers.iter().enumerate() {
            let cap = graph.max_degree(layer);
            for (node, list) in lists.iter().enumerate() {
                assert!(list.len() <= cap, "node {node} at layer {layer} has {} edges", list.len());
            }
        }
    }

    /// Extra entry points are the mitigation for a near-duplicate hub, so the
    /// first thing to check is that they really are several distinct places, and
    /// that they are not all the same basin.
    #[test]
    fn extra_entry_points_are_several_distinct_places_to_start() {
        let (vectors, _) = fixture(2000, 16);
        let mut graph = Hnsw::new(HnswParams { entry_points: 8, ..Default::default() });
        graph.build(&vectors);
        let query = vectors.copy_of(11);
        let starts = graph.entry_points(&vectors, &query);
        assert!(starts.len() >= 8, "only {} starting points: {starts:?}", starts.len());
        let unique: std::collections::HashSet<u32> = starts.iter().copied().collect();
        assert_eq!(unique.len(), starts.len(), "starting points repeat: {starts:?}");
    }

    #[test]
    fn one_entry_point_starts_the_walk_in_exactly_one_place() {
        let (vectors, _) = fixture(500, 16);
        let mut graph = Hnsw::new(HnswParams::default());
        graph.build(&vectors);
        let query = vectors.copy_of(3);
        assert_eq!(graph.entry_points(&vectors, &query).len(), 1);
    }

    /// The query-relevant half of the seed set has to actually be query-relevant:
    /// searching layer 1 rather than descending it is the difference between eight
    /// starting points near the answer and eight arbitrary ones.
    #[test]
    fn the_seed_set_includes_points_near_the_query() {
        let (vectors, store) = fixture(6000, 32);
        let mut graph = Hnsw::new(HnswParams { entry_points: 8, exhaustive_below: 0, ..Default::default() });
        graph.force_graph_traversal();
        graph.build(&vectors);

        let filter = CompiledFilter::compile(&Filter::default(), &store);
        let query = vectors.copy_of(2_345);
        let exact = flat::search(&vectors, &store, &filter, &query, 200);
        let cutoff = exact.last().map(|n| n.distance).unwrap_or(f32::MAX);

        let starts = graph.entry_points(&vectors, &query);
        let near = starts.iter().filter(|n| vectors.distance(**n, &query) <= cutoff).count();
        assert!(near > 0, "no starting point was anywhere near the query: {starts:?}");
    }

    /// More entry points must never make the answer worse; the walk starts from a
    /// superset of where it started before.
    #[test]
    fn extra_entry_points_do_not_lower_recall() {
        let (vectors, store) = fixture(6000, 32);
        let base = HnswParams { exhaustive_below: 0, ..Default::default() };
        let mut one = Hnsw::new(base);
        one.force_graph_traversal();
        one.build(&vectors);

        // Same seed, same insertion order, so the two graphs are identical and only
        // the number of starting points differs.
        let mut many = Hnsw::new(HnswParams { entry_points: 8, ..base });
        many.force_graph_traversal();
        many.build(&vectors);

        let filter = CompiledFilter::compile(&Filter::default(), &store);
        let mut single = 0.0;
        let mut multiple = 0.0;
        for probe in [11u32, 640, 1900, 3300, 4800, 5500] {
            let q = vectors.copy_of(probe);
            let exact = flat::search(&vectors, &store, &filter, &q, 10);
            single += recall(&one.search(&vectors, &store, &filter, &q, 10, Some(32)), &exact);
            multiple += recall(&many.search(&vectors, &store, &filter, &q, 10, Some(32)), &exact);
        }
        assert!(
            multiple >= single,
            "eight entry points scored {multiple} against one entry point's {single}"
        );
    }

    /// Turning the top-up off is meant to change the graph, not break it.
    #[test]
    fn a_graph_built_without_pruned_connections_still_answers() {
        let (vectors, store) = fixture(4000, 32);
        let mut graph = Hnsw::new(HnswParams {
            keep_pruned_connections: false,
            exhaustive_below: 0,
            ..Default::default()
        });
        graph.force_graph_traversal();
        graph.build(&vectors);

        let filter = CompiledFilter::compile(&Filter::default(), &store);
        let mut total = 0.0;
        for probe in [3u32, 900, 2100, 3600] {
            let q = vectors.copy_of(probe);
            let exact = flat::search(&vectors, &store, &filter, &q, 10);
            total += recall(&graph.search(&vectors, &store, &filter, &q, 10, Some(128)), &exact);
        }
        assert!(total / 4.0 > 0.8, "recall fell to {}", total / 4.0);
    }

    /// Appending a batch on several threads must be as accurate as appending it
    /// one node at a time.
    ///
    /// The first half is inserted the same way in both graphs, so what is being
    /// compared is the batch and only the batch. "As accurate" is measured
    /// against the exhaustive scan, because the two graphs are not the same
    /// graph and were never going to be.
    #[test]
    fn a_parallel_batch_insert_is_as_accurate_as_the_sequential_one() {
        let (vectors, store) = fixture(6000, 32);
        let base = HnswParams { exhaustive_below: 0, ..Default::default() };

        let mut sequential = Hnsw::new(base);
        sequential.force_graph_traversal();
        for node in 0..3000u32 {
            sequential.insert(&vectors, node);
        }
        for node in 3000..6000u32 {
            sequential.insert(&vectors, node);
        }

        let mut parallel = Hnsw::new(HnswParams { build_threads: 4, ..base });
        parallel.force_graph_traversal();
        for node in 0..3000u32 {
            parallel.insert(&vectors, node);
        }
        parallel.insert_batch(&vectors, 3000, 6000);

        assert_eq!(parallel.len(), sequential.len());
        let filter = CompiledFilter::compile(&Filter::default(), &store);
        let mut sequential_recall = 0.0;
        let mut parallel_recall = 0.0;
        for probe in [7u32, 300, 1500, 2900, 4400, 5100] {
            let q = vectors.copy_of(probe);
            let exact = flat::search(&vectors, &store, &filter, &q, 10);
            sequential_recall +=
                recall(&sequential.search(&vectors, &store, &filter, &q, 10, Some(128)), &exact);
            parallel_recall +=
                recall(&parallel.search(&vectors, &store, &filter, &q, 10, Some(128)), &exact);
        }
        sequential_recall /= 6.0;
        parallel_recall /= 6.0;
        assert!(
            parallel_recall >= sequential_recall - 0.05,
            "batch recall {parallel_recall} against sequential {sequential_recall}"
        );
    }

    /// Every appended node gets a neighbour list, whichever thread wrote it.
    #[test]
    fn a_parallel_batch_insert_leaves_no_appended_node_unlinked() {
        let (vectors, _) = fixture(4000, 16);
        let mut graph = Hnsw::new(HnswParams {
            build_threads: 8,
            exhaustive_below: 0,
            ..Default::default()
        });
        for node in 0..2000u32 {
            graph.insert(&vectors, node);
        }
        graph.insert_batch(&vectors, 2000, 4000);
        let isolated = (2000..graph.len())
            .filter(|node| graph.layers[0][*node].is_empty())
            .count();
        assert_eq!(isolated, 0, "{isolated} appended nodes have no layer-0 neighbours");
    }

    /// A batch below the floor is the sequential loop, exactly.
    ///
    /// The floor exists because the locked representation costs a move per node
    /// per layer over the whole graph however small the batch is. A caller under
    /// it must get the graph it would have got from `insert`, node for node -
    /// same seed, same levels, same lists - or the floor is a behaviour change
    /// wearing an optimisation's clothes.
    #[test]
    fn a_batch_below_the_floor_is_the_sequential_loop() {
        let (vectors, _) = fixture(1200, 16);
        let base = HnswParams {
            build_threads: 8,
            exhaustive_below: 0,
            ..Default::default()
        };

        let mut looped = Hnsw::new(base);
        for node in 0..1200u32 {
            looped.insert(&vectors, node);
        }

        let mut batched = Hnsw::new(base);
        for node in 0..1000u32 {
            batched.insert(&vectors, node);
        }
        batched.insert_batch(&vectors, 1000, 1100);
        batched.insert_batch(&vectors, 1100, 1200);

        assert_eq!(batched.layers, looped.layers);
        assert_eq!(batched.node_top, looped.node_top);
        assert_eq!(batched.entry, looped.entry);
    }

}
