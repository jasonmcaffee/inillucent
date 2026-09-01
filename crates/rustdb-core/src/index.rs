//! The public engine: one type holding the store, the vectors, the graph, the
//! quantized codes and the inverted index, with the three query paths the current
//! stack exposes.

use crate::bm25::{Bm25Index, LexicalHit, LexicalParams};
use crate::filter::{CompiledFilter, Filter};
use crate::flat::{self, Neighbour};
use crate::hnsw::{Hnsw, HnswParams};
use crate::quantize::QuantizedSet;
use crate::rank::{
    self, AdaptiveWeights, Fusion, FusedHit, FusionParams, QuerySignals, ScoreBounds, PER_DOC_CAP,
};
use crate::store::{ChunkInput, Store};
use crate::tokenize::Tokenizer;
use crate::vectors::VectorSet;

#[derive(Debug, Clone, Copy)]
pub struct IndexConfig {
    pub dims: usize,
    pub hnsw: HnswParams,
    /// Build int8 codes alongside the f32 vectors and use them for the first
    /// pass, rescoring the survivors with full precision.
    pub quantized: bool,
    /// Candidate multiplier for the quantized pass. 1.0 means no oversampling,
    /// which is the setting most likely to lose accuracy.
    pub oversample: f32,
    /// Candidates drawn from each side before fusion. The baseline uses 50.
    pub candidates: usize,
    pub fusion: Fusion,
    pub per_doc_cap: usize,
    /// Whether a query term also matches the terms it prefixes.
    pub lexical_prefix: bool,
    /// Exponent on the share of the query a lexical hit actually contains. 0
    /// scores any term, which is what the engine shipped with; above 0 a chunk
    /// holding one word of a six word question ranks below one holding five.
    pub lexical_coverage: f32,
    /// How much of a lexical score is scaled by how tightly the matched query terms
    /// sit together. 0 is bag of words, which is what BM25 alone gives; 1 lets
    /// position decide. PostgreSQL gets this for free from `ts_rank_cd`.
    pub lexical_proximity: f32,
    /// Rank lexical hits by how many query terms they hold first, and by score
    /// second. PostgreSQL gets this from joining terms with `&`: a chunk missing a
    /// word is not a worse answer there, it is not returned at all. Tiering keeps
    /// that ordering without losing the partial matches underneath it.
    pub lexical_tier: bool,
    /// How much of a lexical score is scaled by whether the matched query terms
    /// appear in the query's own order. 0 is off, and off is what the engine has
    /// always done.
    pub lexical_phrase: f32,
    /// How far down the BM25 ranking the position-aware rescoring reaches, as a
    /// multiple of the requested `k`.
    pub lexical_rescore_depth: usize,
    /// How the vector weight is chosen per query. Every gain at zero, which is the
    /// default, makes this exactly the fixed weight `fusion` carries.
    pub adaptive: AdaptiveWeights,
    /// Whether the vector weight is chosen per query at all. Separate from the
    /// gains so a caller can turn the whole mechanism off without losing a tuned
    /// rule, and so the cost of computing the signals is not paid when it is off.
    pub adaptive_fusion: bool,
    /// Maximal Marginal Relevance selection: 1.0 selects purely by fused score,
    /// lower values trade score for novelty. 1.0 is what the engine has always
    /// done.
    pub mmr_lambda: f32,
}

impl Default for IndexConfig {
    fn default() -> Self {
        IndexConfig {
            dims: 768,
            hnsw: HnswParams::default(),
            quantized: false,
            oversample: 3.0,
            candidates: 50,
            // Measured, not inherited. On the 18,685 chunk corpus, min-max fusion at a
            // vector weight of 0.35 beat Reciprocal Rank Fusion on every hybrid metric:
            // nDCG 0.751 against 0.434 on natural language queries, 0.981 against 0.933
            // on document identity. RRF is still available and still what `Fusion`
            // defaults to on its own; this is the engine saying which one it recommends.
            fusion: Fusion::NormalizedScore { vector_weight: 0.35 },
            per_doc_cap: PER_DOC_CAP,
            // Measured off. Prefix matching lets `town` match `township`, which is what
            // PostgreSQL offers through `:*`, and on a 512,000 term dictionary it mostly
            // buys noise: it credits a chunk with holding a query term it does not hold,
            // which is exactly the judgement coverage weighting depends on. Measured on
            // the full corpus, off is better on heading MRR (0.671 against 0.665) and on
            // the whole hybrid family, and the only thing it costs is a thousandth of
            // identifier MRR in a scenario rust-db already wins five to one.
            lexical_prefix: false,
            lexical_coverage: 3.0,
            lexical_proximity: 1.0,
            // Off, because coverage weighting already does its job better. Tiering is the
            // blunt version of the same idea and it is worth keeping for a caller who
            // sets `lexical_coverage` to 0: there it lifts heading success@10 from 0.778
            // to 0.889. At coverage 3 the two orderings agree, and where they disagree,
            // idf mass is the better judge than a count of terms.
            lexical_tier: false,
            // Measured on. Proximity asks how wide the smallest window holding
            // the matched terms is; phrase asks whether they came in the query's
            // own order inside it, which is the part of a question that survives
            // paraphrase least well and the part window width cannot see. On the
            // 185,078 chunk corpus, at 0.75 with everything else held fixed, it
            // moved graded nDCG on the passage evidence family from 0.7183 to
            // 0.7211 and multi-source evidence recall from 0.5904 to 0.5949, and
            // regressed nothing. On its own that is inside the practical
            // threshold; it is on because it is free and it compounds with the
            // adaptive weighting below.
            lexical_phrase: 0.75,
            lexical_rescore_depth: 6,
            // Measured on, at a tenth. The vector weight is chosen per query from
            // four signals the search already computed: how many query terms look
            // like identifiers, how many the dictionary has never seen, how much
            // of the query the best lexical hit holds, and how far each side's
            // leader stands above its own list. DAT (arXiv 2503.23013) established
            // that the right balance is a property of the query rather than of a
            // benchmark, and got the signal by putting a language model in the
            // retrieval path; this gets the same signal for four floating point
            // operations.
            //
            // On the 185,078 chunk corpus over 614 passage evidence queries, with
            // the phrase weight above: passage evidence 0.7211 to 0.7253,
            // multi-source evidence recall 0.5949 to 0.6382, heading nDCG 0.7621
            // to 0.7653, identifier reciprocal rank 0.5617 unchanged, and the rate
            // at which the engine returns a confident top result for a question
            // with no answer in the corpus 0.1367 to 0.0067. The one negative
            // movement anywhere is document identity, 0.9834 to 0.9814, which is a
            // fifth of the practical threshold and the family least like a
            // question an agent asks.
            //
            // A tenth rather than the 0.15 the sweep also liked: the two cannot be
            // separated on the primary family, and where a sweep cannot separate
            // two arms the smaller intervention is the one to ship.
            adaptive: AdaptiveWeights {
                base: 0.35,
                out_of_vocabulary_gain: 0.10,
                identifier_gain: 0.10,
                separation_gain: 0.10,
                coverage_gain: 0.10,
                floor: 0.05,
                ceiling: 0.95,
            },
            adaptive_fusion: true,
            // Measured off. Diversity selection is implemented and available, and
            // on this corpus the per document cap already removes the redundancy
            // it would remove: every lambda below 1 measured equal to or below 1.0
            // on the passage and multi-source families. It is worth revisiting on a
            // corpus with genuine near-duplicate documents, which this one does not
            // have because each source draws from a disjoint pool.
            mmr_lambda: 1.0,
        }
    }
}

pub struct Index {
    config: IndexConfig,
    store: Store,
    vectors: VectorSet,
    quantized: Option<QuantizedSet>,
    graph: Option<Hnsw>,
    lexical: Option<Bm25Index>,
    tokenizer: Tokenizer,
    force_graph: bool,
}

pub struct BuildStats {
    pub chunks: usize,
    pub documents: usize,
    pub graph_edges: usize,
    pub graph_layers: usize,
    pub lexical_terms: usize,
    pub lexical_postings: usize,
    pub vector_bytes: usize,
    pub quantized_bytes: usize,
}

impl Index {
    pub fn new(config: IndexConfig) -> Self {
        Index {
            store: Store::default(),
            vectors: VectorSet::new(config.dims),
            quantized: None,
            graph: None,
            lexical: None,
            tokenizer: Tokenizer::default(),
            force_graph: false,
            config,
        }
    }

    /// Always walk the graph rather than routing a selective filter to an
    /// exhaustive scan. Used only where traversal accuracy is what is being
    /// measured.
    pub fn force_graph_traversal(&mut self) {
        self.set_force_graph(true);
    }

    /// Turn forced traversal on or off on a committed index. Lets a caller that
    /// wants to measure traversal accuracy borrow an index built for other
    /// purposes rather than paying for a second one.
    pub fn set_force_graph(&mut self, on: bool) {
        self.force_graph = on;
        if let Some(g) = self.graph.as_mut() {
            g.set_force_graph(on);
        }
    }

    /// Changes the lexical coverage exponent on a committed index.
    ///
    /// Ranking settings are not baked into anything the build produced: the
    /// postings, the graph and the codes are the same whatever this is. So a
    /// sweep over it can reuse one index instead of paying a two minute build per
    /// point, which is the difference between choosing this default in a minute
    /// and choosing it in an hour.
    /// @param coverage - the new exponent; 0 scores any term, as before
    pub fn set_lexical_coverage(&mut self, coverage: f32) {
        self.config.lexical_coverage = coverage;
    }

    /// Changes the fusion method on a committed index, for the same reason as
    /// `set_lexical_coverage`: nothing the build produced depends on it.
    /// @param fusion - how the vector and lexical lists are combined
    pub fn set_fusion(&mut self, fusion: Fusion) {
        self.config.fusion = fusion;
    }

    /// Changes the lexical proximity weight on a committed index. The positions it
    /// reads were recorded at build time, so only the weight is a setting.
    /// @param proximity - 0 for bag of words, 1 to let position decide
    pub fn set_lexical_proximity(&mut self, proximity: f32) {
        self.config.lexical_proximity = proximity;
    }

    /// Turns prefix matching on or off on a committed index. The dictionary is
    /// already built either way; this only decides whether a query term is also
    /// allowed to match the terms it prefixes.
    /// @param on - whether a query term matches the terms it prefixes
    pub fn set_lexical_prefix(&mut self, on: bool) {
        self.config.lexical_prefix = on;
    }

    /// Turns tiered lexical ranking on or off on a committed index.
    /// @param on - whether the count of matched query terms outranks the score
    pub fn set_lexical_tier(&mut self, on: bool) {
        self.config.lexical_tier = on;
    }

    /// Changes the ordered-phrase weight on a committed index. Like proximity it
    /// reads positions recorded at build time, so only the weight is a setting.
    /// @param phrase - 0 ignores order, 1 lets order decide
    pub fn set_lexical_phrase(&mut self, phrase: f32) {
        self.config.lexical_phrase = phrase;
    }

    /// Changes how far the position-aware rescoring reaches, as a multiple of `k`.
    /// @param depth - the multiple; 1 rescores only the hits already in reach
    pub fn set_lexical_rescore_depth(&mut self, depth: usize) {
        self.config.lexical_rescore_depth = depth.max(1);
    }

    /// Turns per-query vector weighting on or off, and sets the rule it uses.
    /// @param on - whether the weight is chosen per query
    /// @param weights - the rule; every gain at zero reproduces the fixed weight
    pub fn set_adaptive_fusion(&mut self, on: bool, weights: AdaptiveWeights) {
        self.config.adaptive_fusion = on;
        self.config.adaptive = weights;
    }

    /// Changes the diversity trade-off on a committed index.
    /// @param lambda - 1 selects purely by fused score, lower prefers novelty
    pub fn set_mmr_lambda(&mut self, lambda: f32) {
        self.config.mmr_lambda = lambda;
    }

    /// The ranking dials the lexical index is currently being asked to apply.
    fn lexical_params(&self) -> LexicalParams {
        LexicalParams {
            prefix: self.config.lexical_prefix,
            coverage: self.config.lexical_coverage,
            proximity: self.config.lexical_proximity,
            tier: self.config.lexical_tier,
            phrase: self.config.lexical_phrase,
            rescore_depth_factor: self.config.lexical_rescore_depth,
        }
    }

    pub fn config(&self) -> &IndexConfig {
        &self.config
    }

    /// The largest lexical score this query could produce, which is the bound
    /// `Fusion::TheoreticalMinMax` scales by. Zero for a query holding no term the
    /// dictionary knows.
    /// @param query - the raw query text
    pub fn lexical_score_ceiling(&self, query: &str) -> f32 {
        self.lexical
            .as_ref()
            .map(|l| l.score_ceiling(query, &self.tokenizer, self.config.lexical_prefix))
            .unwrap_or(0.0)
    }

    /// Rebuild an index from the parts that were stored, deriving the lexical
    /// index and the int8 codes rather than reading them: both are deterministic
    /// functions of the store and the vectors, and recomputing them costs less
    /// than the disk they would take.
    pub fn from_parts(
        config: IndexConfig,
        store: Store,
        vectors: VectorSet,
        graph: Hnsw,
    ) -> anyhow::Result<Index> {
        if vectors.len() != store.n_chunks() {
            anyhow::bail!(
                "index is inconsistent: {} vectors for {} chunks",
                vectors.len(),
                store.n_chunks()
            );
        }
        let tokenizer = Tokenizer::default();
        let lexical = Bm25Index::build(&store, &tokenizer);
        let quantized = if config.quantized {
            Some(QuantizedSet::from_vectors(&vectors))
        } else {
            None
        };
        Ok(Index {
            config,
            store,
            vectors,
            quantized,
            graph: Some(graph),
            lexical: Some(lexical),
            tokenizer,
            force_graph: false,
        })
    }

    /// Write the graph, for `persist::save`.
    pub fn write_graph(&self, w: &mut impl std::io::Write) -> anyhow::Result<()> {
        match &self.graph {
            Some(g) => {
                g.write_graph(w)?;
                Ok(())
            }
            None => anyhow::bail!("commit the index before saving it"),
        }
    }

    pub fn store(&self) -> &Store {
        &self.store
    }

    pub fn vectors(&self) -> &VectorSet {
        &self.vectors
    }

    /// Add chunks together with their vectors. The two slices must correspond
    /// element for element, which is the one to one chunk to vector mapping the
    /// baseline also maintains.
    pub fn add(&mut self, chunks: Vec<ChunkInput>, embeddings: &[Vec<f32>]) {
        assert_eq!(
            chunks.len(),
            embeddings.len(),
            "each chunk needs exactly one vector"
        );
        self.store.add_chunks(chunks);
        for e in embeddings {
            self.vectors.push(e);
        }
        // Any structure built earlier is now stale.
        self.graph = None;
        self.lexical = None;
        self.quantized = None;
    }

    /// Build every queryable structure. Separated from `add` so a bulk load pays
    /// for graph construction once rather than per insert.
    pub fn commit(&mut self) -> BuildStats {
        let mut graph = Hnsw::new(self.config.hnsw);
        if self.force_graph {
            graph.force_graph_traversal();
        }
        graph.build(&self.vectors);
        let lexical = Bm25Index::build(&self.store, &self.tokenizer);
        let quantized = if self.config.quantized {
            Some(QuantizedSet::from_vectors(&self.vectors))
        } else {
            None
        };

        let stats = BuildStats {
            chunks: self.store.n_chunks(),
            documents: self.store.n_documents(),
            graph_edges: graph.edge_count(),
            graph_layers: graph.n_layers(),
            lexical_terms: lexical.n_terms(),
            lexical_postings: lexical.n_postings(),
            vector_bytes: self.vectors.raw().len() * 4,
            quantized_bytes: quantized.as_ref().map(|q| q.bytes()).unwrap_or(0),
        };

        self.graph = Some(graph);
        self.lexical = Some(lexical);
        self.quantized = quantized;
        stats
    }

    /// Which path a query with this filter will take, so a report can say so
    /// rather than leaving the reader to infer it.
    pub fn path_for(&self, filter: &CompiledFilter, ef_search: Option<usize>) -> &'static str {
        match &self.graph {
            None => "exhaustive",
            Some(g) => {
                let ef = ef_search.unwrap_or(self.config.hnsw.ef_search);
                if g.prefers_exhaustive(filter.pass_count(), self.store.n_chunks(), ef) {
                    "exhaustive"
                } else {
                    "graph"
                }
            }
        }
    }

    pub fn compile(&self, filter: &Filter) -> CompiledFilter {
        CompiledFilter::compile(filter, &self.store)
    }

    /// Exact top k by cosine distance. The reference the rest is graded against.
    pub fn exhaustive_search(
        &self,
        query: &[f32],
        filter: &CompiledFilter,
        k: usize,
    ) -> Vec<Neighbour> {
        flat::search(&self.vectors, &self.store, filter, query, k)
    }

    /// Approximate top k through the graph, with the quantized first pass when
    /// configured.
    pub fn vector_search(
        &self,
        query: &[f32],
        filter: &CompiledFilter,
        k: usize,
        ef_search: Option<usize>,
    ) -> Vec<Neighbour> {
        let Some(graph) = &self.graph else {
            return self.exhaustive_search(query, filter, k);
        };

        match &self.quantized {
            None => graph.search(&self.vectors, &self.store, filter, query, k, ef_search),
            Some(codes) => {
                // Walk the graph on the int8 codes, drawing more candidates than
                // needed, then rescore the survivors with the full precision
                // vectors. Oversampling is what buys back the accuracy the
                // compressed comparison costs.
                let wide = ((k as f32 * self.config.oversample).ceil() as usize).max(k);
                let mut candidates =
                    graph.search_with(codes, &self.store, filter, query, wide, ef_search);
                for c in candidates.iter_mut() {
                    c.distance = self.vectors.distance(c.chunk, query);
                }
                candidates.sort_by(|a, b| {
                    a.distance
                        .partial_cmp(&b.distance)
                        .unwrap_or(std::cmp::Ordering::Equal)
                        .then(a.chunk.cmp(&b.chunk))
                });
                candidates.truncate(k);
                candidates
            }
        }
    }

    pub fn lexical_search(
        &self,
        query: &str,
        filter: &CompiledFilter,
        k: usize,
    ) -> Vec<LexicalHit> {
        let Some(lexical) = &self.lexical else {
            return Vec::new();
        };
        lexical.search(query, &self.store, filter, &self.tokenizer, k, self.lexical_params())
    }

    /// The full pipeline: both sides, fused, capped per document, truncated.
    pub fn hybrid_search(
        &self,
        query: &str,
        query_vector: &[f32],
        filter: &CompiledFilter,
        k: usize,
        ef_search: Option<usize>,
    ) -> Vec<FusedHit> {
        self.hybrid_search_explained(query, query_vector, filter, k, ef_search).0
    }

    /// The full pipeline, and what it decided on the way.
    ///
    /// Retrieval returns a ranking; an evaluation needs to know why that ranking
    /// came out the way it did, and recomputing the reason afterwards would ask a
    /// second, differently-configured question. So the signals the fusion read and
    /// the weight it settled on are returned alongside the hits, and the score
    /// card writes them into its per-query run file.
    /// @param query - the raw query text
    /// @param query_vector - the query embedding, already normalized
    /// @param filter - the compiled predicate
    /// @param k - how many hits to return
    /// @param ef_search - traversal width, or the configured default
    pub fn hybrid_search_explained(
        &self,
        query: &str,
        query_vector: &[f32],
        filter: &CompiledFilter,
        k: usize,
        ef_search: Option<usize>,
    ) -> (Vec<FusedHit>, QueryExplanation) {
        let candidates = self.config.candidates.max(k);
        let vector_hits = self.vector_search(query_vector, filter, candidates, ef_search);
        let lexical_hits = self.lexical_search(query, filter, candidates);

        let bounds = ScoreBounds {
            lexical_ceiling: self
                .lexical
                .as_ref()
                .map(|l| l.score_ceiling(query, &self.tokenizer, self.config.lexical_prefix))
                .unwrap_or(0.0),
        };
        let signals = self.signals_for(query, &vector_hits, &lexical_hits);
        let fusion = self.weighted_fusion(&signals);

        let hits = rank::fuse(
            &vector_hits,
            &lexical_hits,
            &self.store,
            Some(&self.vectors),
            FusionParams {
                fusion,
                top_k: k,
                per_doc_cap: self.config.per_doc_cap,
                bounds,
                mmr_lambda: self.config.mmr_lambda,
            },
        );
        let explanation = QueryExplanation {
            signals,
            vector_weight: weight_of(fusion),
            lexical_ceiling: bounds.lexical_ceiling,
            vector_candidates: vector_hits.len(),
            lexical_candidates: lexical_hits.len(),
        };
        (hits, explanation)
    }

    /// The fusion this query should use: the configured one, or the same method
    /// carrying a weight chosen from the query's own signals.
    ///
    /// Rank fusion has no weight to adapt, so it is returned untouched rather than
    /// silently converted into a score fusion.
    /// @param signals - what the two candidate lists said about this query
    fn weighted_fusion(&self, signals: &QuerySignals) -> Fusion {
        if !self.config.adaptive_fusion || self.config.adaptive.is_fixed() {
            return self.config.fusion;
        }
        let w = self.config.adaptive.weight_for(signals);
        match self.config.fusion {
            Fusion::ReciprocalRank { k } => Fusion::ReciprocalRank { k },
            Fusion::NormalizedScore { .. } => Fusion::NormalizedScore { vector_weight: w },
            Fusion::Convex { .. } => Fusion::Convex { vector_weight: w },
            Fusion::TheoreticalMinMax { .. } => Fusion::TheoreticalMinMax { vector_weight: w },
        }
    }

    /// Read the per-query signals out of the two candidate lists.
    ///
    /// Skipped entirely when adaptive fusion is off, because the score card writes
    /// the signals for every query and computing them for 185,000 chunks' worth of
    /// candidates is not free when nothing reads them.
    fn signals_for(
        &self,
        query: &str,
        vector_hits: &[Neighbour],
        lexical_hits: &[LexicalHit],
    ) -> QuerySignals {
        let terms = self.tokenizer.query_terms(query);
        if terms.is_empty() {
            return QuerySignals::default();
        }
        let n = terms.len() as f32;
        let identifiers = terms.iter().filter(|t| looks_like_identifier(t)).count() as f32;
        let unknown = match &self.lexical {
            Some(l) => terms.iter().filter(|t| !l.contains_term(t)).count() as f32,
            None => n,
        };
        let v_scores: Vec<f32> =
            vector_hits.iter().map(|h| (1.0 - h.distance).clamp(0.0, 1.0)).collect();
        let l_scores: Vec<f32> = lexical_hits.iter().map(|h| h.score).collect();
        QuerySignals {
            identifier_share: identifiers / n,
            out_of_vocabulary_share: unknown / n,
            lexical_coverage: lexical_hits.first().map(|h| h.coverage).unwrap_or(0.0),
            lexical_separation: rank::separation(&l_scores),
            vector_separation: rank::separation(&v_scores),
        }
    }
}

/// What one hybrid query decided, for a run artifact to record.
#[derive(Debug, Clone, Copy, Default)]
pub struct QueryExplanation {
    pub signals: QuerySignals,
    /// The weight actually used, or 0 for a rank based fusion that has none.
    pub vector_weight: f32,
    pub lexical_ceiling: f32,
    pub vector_candidates: usize,
    pub lexical_candidates: usize,
}

/// The vector weight a fusion carries, or 0 for a rank based one that has none.
fn weight_of(fusion: Fusion) -> f32 {
    match fusion {
        Fusion::ReciprocalRank { .. } => 0.0,
        Fusion::NormalizedScore { vector_weight }
        | Fusion::Convex { vector_weight }
        | Fusion::TheoreticalMinMax { vector_weight } => vector_weight,
    }
}

/// Whether an analyzed term looks like a ticket key, a symbol, a path or a
/// version string rather than an English word.
///
/// The same test the harness uses to mine identifier queries out of the corpus,
/// so what the ranker calls an identifier and what the benchmark calls one are
/// the same thing: letters mixed with digits, or an explicit separator.
fn looks_like_identifier(term: &str) -> bool {
    let has_digit = term.chars().any(|c| c.is_ascii_digit());
    let has_alpha = term.chars().any(|c| c.is_alphabetic());
    let has_separator = term.contains('-') || term.contains('_') || term.contains('.');
    has_alpha && (has_digit || has_separator)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::distance::normalize;
    use rand::rngs::StdRng;
    use rand::{Rng, SeedableRng};

    fn build(n: usize, dims: usize, config: IndexConfig) -> Index {
        let mut rng = StdRng::seed_from_u64(3);
        let mut index = Index::new(IndexConfig { dims, ..config });
        if config.hnsw.exhaustive_below == 0 {
            index.force_graph_traversal();
        }
        let sources = ["confluence", "github", "slack", "jira"];

        let mut chunks = Vec::new();
        let mut vectors = Vec::new();
        let centres: Vec<Vec<f32>> = (0..16)
            .map(|_| (0..dims).map(|_| rng.gen_range(-1.0..1.0)).collect())
            .collect();

        for i in 0..n {
            let source = match i % 10 {
                0..=5 => sources[0],
                6..=7 => sources[1],
                8 => sources[2],
                _ => sources[3],
            };
            chunks.push(ChunkInput {
                source: source.to_string(),
                external_doc_id: format!("d{}", i / 4),
                chunk_index: (i % 4) as u32,
                heading_path: vec![],
                content: format!("chunk {i} about offer eligibility and rules number{i}"),
                title: format!("title {i}"),
                url: format!("https://x/{i}"),
                space_key: Some("ENG".into()),
                author: None,
                author_id: None,
                updated_at: Some(1000 + i as i64),
                labels: vec![],
                deleted: false,
            });
            let c = &centres[i % 16];
            let mut v: Vec<f32> = c.iter().map(|x| x + rng.gen_range(-0.3..0.3)).collect();
            normalize(&mut v);
            vectors.push(v);
        }
        index.add(chunks, &vectors);
        index.commit();
        index
    }

    #[test]
    fn commit_reports_the_structures_it_built() {
        let mut index = Index::new(IndexConfig { dims: 16, quantized: true, ..Default::default() });
        let chunks: Vec<ChunkInput> = (0..100)
            .map(|i| ChunkInput {
                source: "confluence".into(),
                external_doc_id: format!("d{i}"),
                chunk_index: 0,
                heading_path: vec![],
                content: format!("content number {i} offer"),
                title: format!("t{i}"),
                url: format!("u{i}"),
                space_key: None,
                author: None,
                author_id: None,
                updated_at: None,
                labels: vec![],
                deleted: false,
            })
            .collect();
        let vectors: Vec<Vec<f32>> = (0..100)
            .map(|i| {
                let mut v: Vec<f32> = (0..16).map(|d| ((i * 16 + d) as f32 * 0.1).sin()).collect();
                normalize(&mut v);
                v
            })
            .collect();
        index.add(chunks, &vectors);
        let stats = index.commit();
        assert_eq!(stats.chunks, 100);
        assert_eq!(stats.documents, 100);
        assert!(stats.graph_edges > 0);
        assert!(stats.lexical_terms > 0);
        assert_eq!(stats.vector_bytes, 100 * 16 * 4);
        assert!(stats.quantized_bytes > 0);
        assert!(stats.quantized_bytes < stats.vector_bytes);
    }

    #[test]
    fn vector_search_agrees_with_exhaustive_search_on_a_small_index() {
        let index = build(2000, 32, IndexConfig { hnsw: HnswParams { exhaustive_below: 0, ..Default::default() }, ..Default::default() });
        let f = index.compile(&Filter::default());
        let q = index.vectors().get(5).to_vec();
        let exact = index.exhaustive_search(&q, &f, 10);
        let approx = index.vector_search(&q, &f, 10, Some(200));
        let want: std::collections::HashSet<u32> = exact.iter().map(|n| n.chunk).collect();
        let overlap = approx.iter().filter(|n| want.contains(&n.chunk)).count();
        assert!(overlap >= 9, "only {overlap} of 10 matched exhaustive search");
    }

    #[test]
    fn hybrid_search_returns_results_under_a_selective_filter() {
        let index = build(20000, 32, IndexConfig { hnsw: HnswParams { exhaustive_below: 0, ..Default::default() }, ..Default::default() });
        for source in ["slack", "jira"] {
            let f = index.compile(&Filter::source(source));
            let q = index.vectors().get(1).to_vec();
            let hits = index.hybrid_search("offer eligibility rules", &q, &f, 10, Some(64));
            assert_eq!(hits.len(), 10, "{source} returned {} of 10", hits.len());
        }
    }

    #[test]
    fn quantization_keeps_the_ranking_after_rescoring() {
        let plain = build(3000, 64, IndexConfig { hnsw: HnswParams { exhaustive_below: 0, ..Default::default() }, ..Default::default() });
        let quant = build(3000, 64, IndexConfig { quantized: true, hnsw: HnswParams { exhaustive_below: 0, ..Default::default() }, ..Default::default() });
        let f = plain.compile(&Filter::default());
        let fq = quant.compile(&Filter::default());
        let q = plain.vectors().get(9).to_vec();
        let a = plain.vector_search(&q, &f, 10, Some(128));
        let b = quant.vector_search(&q, &fq, 10, Some(128));
        let want: std::collections::HashSet<u32> = a.iter().map(|n| n.chunk).collect();
        let overlap = b.iter().filter(|n| want.contains(&n.chunk)).count();
        assert!(overlap >= 9, "quantized ranking kept only {overlap} of 10");
    }

    #[test]
    fn the_per_document_cap_holds_in_the_hybrid_path() {
        let index = build(400, 16, IndexConfig::default());
        let f = index.compile(&Filter::default());
        let q = index.vectors().get(0).to_vec();
        let hits = index.hybrid_search("offer eligibility", &q, &f, 20, None);
        let mut per_doc = std::collections::HashMap::new();
        for h in &hits {
            *per_doc.entry(index.store().chunks[h.chunk as usize].doc).or_insert(0) += 1;
        }
        assert!(per_doc.values().all(|c| *c <= PER_DOC_CAP));
    }

    #[test]
    fn every_query_path_honours_the_filter() {
        let index = build(3000, 32, IndexConfig::default());
        let slack = index.store().sources.get("slack").unwrap();
        let f = index.compile(&Filter::source("slack"));
        let q = index.vectors().get(3).to_vec();

        let mut chunks: Vec<u32> = index.vector_search(&q, &f, 20, None).iter().map(|n| n.chunk).collect();
        chunks.extend(index.lexical_search("offer eligibility", &f, 20).iter().map(|h| h.chunk));
        chunks.extend(index.hybrid_search("offer eligibility", &q, &f, 20, None).iter().map(|h| h.chunk));
        assert!(!chunks.is_empty());
        for c in chunks {
            let doc = index.store().chunks[c as usize].doc;
            assert_eq!(index.store().documents[doc as usize].source, slack);
        }
    }

    #[test]
    fn an_empty_index_answers_every_path_without_panicking() {
        let mut index = Index::new(IndexConfig { dims: 8, ..Default::default() });
        index.commit();
        let f = index.compile(&Filter::default());
        assert!(index.vector_search(&[0.0; 8], &f, 10, None).is_empty());
        assert!(index.lexical_search("anything", &f, 10).is_empty());
        assert!(index.hybrid_search("anything", &[0.0; 8], &f, 10, None).is_empty());
        assert!(index.exhaustive_search(&[0.0; 8], &f, 10).is_empty());
    }

    #[test]
    fn adding_after_commit_requires_another_commit_and_still_answers() {
        let mut index = build(200, 16, IndexConfig::default());
        let before = index.store().n_chunks();
        index.add(
            vec![ChunkInput {
                source: "slack".into(),
                external_doc_id: "extra".into(),
                chunk_index: 0,
                heading_path: vec![],
                content: "a brand new chunk about tirzepatide".into(),
                title: "extra".into(),
                url: "u".into(),
                space_key: None,
                author: None,
                author_id: None,
                updated_at: None,
                labels: vec![],
                deleted: false,
            }],
            &[{
                let mut v = vec![0.5f32; 16];
                normalize(&mut v);
                v
            }],
        );
        index.commit();
        assert_eq!(index.store().n_chunks(), before + 1);
        let f = index.compile(&Filter::default());
        let hits = index.lexical_search("tirzepatide", &f, 5);
        assert_eq!(hits.len(), 1);
    }

    #[test]
    fn both_fusion_methods_produce_a_full_result_set() {
        for fusion in [
            Fusion::ReciprocalRank { k: 60.0 },
            Fusion::NormalizedScore { vector_weight: 0.5 },
        ] {
            let index = build(1000, 32, IndexConfig { fusion, ..Default::default() });
            let f = index.compile(&Filter::default());
            let q = index.vectors().get(0).to_vec();
            let hits = index.hybrid_search("offer eligibility rules", &q, &f, 10, None);
            assert_eq!(hits.len(), 10, "{fusion:?} returned {} of 10", hits.len());
        }
    }
}
