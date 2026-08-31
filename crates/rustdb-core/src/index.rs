//! The public engine: one type holding the store, the vectors, the graph, the
//! quantized codes and the inverted index, with the three query paths the current
//! stack exposes.

use crate::bm25::{Bm25Index, LexicalHit};
use crate::filter::{CompiledFilter, Filter};
use crate::flat::{self, Neighbour};
use crate::hnsw::{Hnsw, HnswParams};
use crate::quantize::QuantizedSet;
use crate::rank::{self, Fusion, FusedHit, PER_DOC_CAP};
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

    pub fn config(&self) -> &IndexConfig {
        &self.config
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
        lexical.search(
            query,
            &self.store,
            filter,
            &self.tokenizer,
            k,
            self.config.lexical_prefix,
            self.config.lexical_coverage,
            self.config.lexical_proximity,
            self.config.lexical_tier,
        )
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
        let candidates = self.config.candidates.max(k);
        let vector_hits = self.vector_search(query_vector, filter, candidates, ef_search);
        let lexical_hits = self.lexical_search(query, filter, candidates);
        rank::fuse(
            &vector_hits,
            &lexical_hits,
            &self.store,
            self.config.fusion,
            k,
            self.config.per_doc_cap,
        )
    }
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
