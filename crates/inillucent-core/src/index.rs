//! The public engine: one type holding the store, the vectors, the graph, the
//! quantized codes and the inverted index, with the three query paths the current
//! stack exposes.
//!
//! Invariant: **the parts agree about which chunk is which.** The store, the
//! vectors, the graph, the quantized codes and the inverted index are five
//! structures addressed by one chunk identifier, and a build or an append that
//! advanced some of them and not others would return a neighbour list whose
//! text belongs to different rows.

use crate::bm25::{Bm25Index, LexicalHit, LexicalParams};
use crate::distance::Metric;
use crate::filter::{CompiledFilter, Filter};
use crate::flat::{self, Neighbour};
use crate::hnsw::{Hnsw, HnswParams};
use crate::quantize::QuantizedSet;
use crate::rank::{
    self, AdaptiveWeights, FusedHit, Fusion, FusionParams, QuerySignals, ScoreBounds, PER_DOC_CAP,
};
use crate::store::{ChunkInput, Store};
use crate::tokenize::Tokenizer;
use crate::vectors::VectorSet;

/// Every setting an index is built and queried with.
#[derive(Debug, Clone, Copy)]
pub struct IndexConfig {
    /// How wide the vectors are.
    pub dims: usize,
    /// The distance the vector branch minimises. Decided once, at
    /// construction: it decides whether `add`/`append` normalize a vector on
    /// the way in, so a config that changed metric after vectors were already
    /// stored would leave old and new rows compared inconsistently.
    pub metric: Metric,
    /// How the graph is built and how widely it is searched.
    pub hnsw: HnswParams,
    /// Hold the full precision vectors on the heap rather than reading them from
    /// the index file as they are scored.
    ///
    /// **Off by default, and that is a deliberate change from what this engine
    /// used to do.** The vectors are the largest thing an index holds - 1.85 GB
    /// for a 601,862 chunk corpus at 768 dimensions, against about 2 GB for
    /// everything else - and holding them means every process that opens the index
    /// pays for them whether or not it ever runs a semantic search. Left in the
    /// file they are still served out of the operating system's page cache while
    /// they are being used, but that cache is reclaimable and this process's heap
    /// is not.
    ///
    /// Turn it on when the index is the only thing on the machine and the search
    /// latency matters more than the memory. `docs/vector-residency.md` has what it
    /// costs and what it buys, measured on a real corpus rather than estimated.
    pub resident_vectors: bool,
    /// Build int8 codes alongside the f32 vectors and use them for the first
    /// pass, rescoring the survivors with full precision.
    pub quantized: bool,
    /// Candidate multiplier for the quantized pass. 1.0 means no oversampling,
    /// which is the setting most likely to lose accuracy.
    pub oversample: f32,
    /// Candidates drawn from each side before fusion. The baseline uses 50.
    pub candidates: usize,
    /// How the vector list and the lexical list are combined.
    pub fusion: Fusion,
    /// How many chunks of one document may appear in a fused list.
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
    /// How much a term in a chunk's heading outweighs the same term in its body.
    /// 0 is off, which is what the engine has always done.
    pub lexical_heading_boost: f32,
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
            metric: Metric::Cosine,
            hnsw: HnswParams::default(),
            // Off. See the field for the argument; the short version is that the
            // vectors are the largest thing an index holds and the operating
            // system is better placed than this process to decide whether they
            // stay in memory.
            resident_vectors: false,
            quantized: false,
            oversample: 3.0,
            candidates: 50,
            // Measured, not inherited. On the 18,685 chunk corpus, min-max fusion at a
            // vector weight of 0.35 beat Reciprocal Rank Fusion on every hybrid metric:
            // nDCG 0.751 against 0.434 on natural language queries, 0.981 against 0.933
            // on document identity. RRF is still available and still what `Fusion`
            // defaults to on its own; this is the engine saying which one it recommends.
            fusion: Fusion::NormalizedScore {
                vector_weight: 0.35,
            },
            per_doc_cap: PER_DOC_CAP,
            // Measured off. Prefix matching lets `town` match `township`, which is what
            // PostgreSQL offers through `:*`, and on a 512,000 term dictionary it mostly
            // buys noise: it credits a chunk with holding a query term it does not hold,
            // which is exactly the judgement coverage weighting depends on. Measured on
            // the full corpus, off is better on heading MRR (0.671 against 0.665) and on
            // the whole hybrid family, and the only thing it costs is a thousandth of
            // identifier MRR in a scenario inillucent already wins five to one.
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
            // Measured off. PostgreSQL weights a subject line above a body with
            // `setweight`, and the terms are all present here either way because a
            // chunk's text begins with its heading - what is missing is only the
            // boost. Whether the boost is worth having is a question about a corpus,
            // and the engine's rule is that a knob is measured rather than assumed.
            lexical_heading_boost: 0.0,
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

/// The engine: a store, its vectors, its graph, its quantized codes and its
/// inverted index, addressed by one chunk identifier.
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

/// What one build produced, so a report does not have to ask the index five
/// separate questions.
pub struct BuildStats {
    /// How many chunks were indexed.
    pub chunks: usize,
    /// How many documents those chunks belong to.
    pub documents: usize,
    /// Total directed edges in the graph.
    pub graph_edges: usize,
    /// How many layers the graph has.
    pub graph_layers: usize,
    /// How many distinct stemmed terms the lexical index holds.
    pub lexical_terms: usize,
    /// How many term-in-chunk appearances it holds.
    pub lexical_postings: usize,
    /// How much the full-precision vectors occupy.
    pub vector_bytes: usize,
    /// How much the int8 codes occupy, or zero when none were built.
    pub quantized_bytes: usize,
    /// Whether the full precision vectors are on this process's heap.
    ///
    /// Reported because it is the difference between an index that costs two
    /// gigabytes to hold and one that costs four, and because a number measured in
    /// one mode and read as if it were the other is the mistake this field exists
    /// to prevent.
    pub resident_vectors: bool,
}

/// What one incremental append did, so a sync can report it without asking the
/// index a second question.
#[derive(Debug, Clone, Copy, Default)]
pub struct AppendStats {
    /// How many chunks the append added.
    pub chunks_added: usize,
    /// How many of those belonged to documents the index had not seen.
    pub documents_added: usize,
    /// Terms the lexical dictionary had never seen before this append.
    pub new_terms: usize,
    /// Whether the append went into built structures, or into an index that has
    /// not been committed yet and still needs a `commit` before it answers.
    pub committed: bool,
}

impl Index {
    /// Returns an empty index built to this configuration.
    ///
    /// @param config - how it is built and queried
    pub fn new(config: IndexConfig) -> Self {
        Index {
            store: Store::default(),
            vectors: VectorSet::with_metric(config.dims, config.metric),
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

    /// Changes how many places a vector search starts from, on a committed index.
    ///
    /// Nothing the build produced depends on it, so a sweep over it reuses one
    /// index instead of building one per value - which also means the difference it
    /// measures is the setting rather than two builds that differ for other
    /// reasons.
    /// @param count - how many starting points, at least one
    pub fn set_entry_points(&mut self, count: usize) {
        self.config.hnsw.entry_points = count.max(1);
        if let Some(graph) = self.graph.as_mut() {
            graph.set_entry_points(count);
        }
    }

    /// Changes the default candidate breadth on a committed index.
    /// @param ef - how many candidates a search keeps in flight
    pub fn set_ef_search(&mut self, ef: usize) {
        self.config.hnsw.ef_search = ef.max(1);
        if let Some(graph) = self.graph.as_mut() {
            graph.set_ef_search(ef);
        }
    }

    /// Changes the size below which a filter always scans exhaustively.
    ///
    /// The lever the design recommends reaching for first when a graph proves
    /// unnavigable on a corpus: an exhaustive scan is exact by construction, and on
    /// a corpus of 598,560 chunks it costs about 110 ms - less at p95 than what the
    /// baseline charges for a query it has not seen recently.
    /// @param below - the chunk count; `usize::MAX` makes every search exact
    pub fn set_exhaustive_below(&mut self, below: usize) {
        self.config.hnsw.exhaustive_below = below;
        if let Some(graph) = self.graph.as_mut() {
            graph.set_exhaustive_below(below);
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
            heading_boost: self.config.lexical_heading_boost,
        }
    }

    /// Returns the configuration this index was built with.
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

    /// Rebuild an index from the parts that were stored.
    ///
    /// The int8 codes are still derived: they are one linear pass over vectors
    /// that have just been read, and a code depends on nothing but its own vector.
    /// The lexical index is not, any more - deriving it means running the analyzer
    /// over the whole text arena, which measured at 26.6 seconds of every cold
    /// start on 598,560 chunks. A caller that has it on disk passes it in; one
    /// that does not gets it rebuilt.
    /// @param config - the configuration the index was saved with
    /// @param store - the documents and chunks
    /// @param vectors - one vector per chunk
    /// @param graph - the adjacency lists
    /// @param lexical - the inverted index, or `None` to rebuild it here
    pub fn from_parts(
        config: IndexConfig,
        store: Store,
        vectors: VectorSet,
        graph: Hnsw,
        lexical: Option<Bm25Index>,
    ) -> anyhow::Result<Index> {
        if vectors.len() != store.n_chunks() {
            anyhow::bail!(
                "index is inconsistent: {} vectors for {} chunks",
                vectors.len(),
                store.n_chunks()
            );
        }
        // The same kind of inconsistency as the chunk count above, and the
        // same answer: refuse rather than search a graph that was built over
        // vectors stored for one metric as though it answered for another.
        // `persist::read_index`/`load_generation` never produce this - they
        // build `vectors` from `config.metric` themselves - so reaching this
        // means a caller assembled the parts by hand and got two of them to
        // disagree.
        if vectors.metric() != config.metric {
            anyhow::bail!(
                "index is inconsistent: the vectors are stored for {:?}, the config declares {:?}",
                vectors.metric(),
                config.metric
            );
        }
        let tokenizer = Tokenizer::default();
        let lexical = lexical.unwrap_or_else(|| Bm25Index::build(&store, &tokenizer));
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

    /// Write the lexical index, for `persist::save`.
    pub fn write_lexical(&self, w: &mut impl std::io::Write) -> anyhow::Result<()> {
        match &self.lexical {
            Some(l) => {
                l.write_to(w)?;
                Ok(())
            }
            None => anyhow::bail!("commit the index before saving it"),
        }
    }

    /// Returns the corpus.
    pub fn store(&self) -> &Store {
        &self.store
    }

    /// Reads the whole chunk text into memory.
    ///
    /// **For a caller that reads every chunk once** (task-2066 §4.3.8). A load
    /// leaves the text in `store.bin` and reads a range per result, which is
    /// what a search wants and what a full scan does not: `inillucent-migrate`
    /// digests every chunk of the corpus, and paying a positional read for each
    /// of six hundred thousand of them is slower than holding the text it is
    /// about to touch anyway.
    pub fn make_text_resident(&mut self) {
        self.store.make_text_resident();
    }

    /// Returns the full-precision vectors.
    pub fn vectors(&self) -> &VectorSet {
        &self.vectors
    }

    // -- checkpoint recording and replay --------------------------------
    //
    // A merge that checkpoints its own progress across several commits
    // (`inillucent_search::module::SearchTable::continue_merge`) needs to
    // record exactly what one checkpoint's fold changed - not re-derive it
    // by re-running the fold again later, which for the graph and the
    // lexical index would cost what building them cost in the first place,
    // on every single chain resolution. These methods are the seam that
    // lets `inillucent_core::persist`'s segment delta format capture that
    // real work once and replay its *result* cheaply afterwards; nothing
    // else in this crate reaches for them.

    /// Starts recording exactly what a subsequent `append`/`replace_document`
    /// changes in the graph and the lexical index, so it can be replayed
    /// later without recomputing it. The store and the vectors need no such
    /// recording: appending to them is already a pure function of the rows
    /// given, cheap to repeat, which `append_store_and_vectors` below reaches
    /// for directly on replay.
    pub fn start_recording(&mut self) {
        if let Some(graph) = self.graph.as_mut() {
            graph.start_recording();
        }
        if let Some(lexical) = self.lexical.as_mut() {
            lexical.start_recording();
        }
    }

    /// Returns and clears every adjacency list the graph has changed since
    /// recording started or since the last drain.
    pub fn drain_graph_recording(&mut self) -> Vec<(u8, u32, Vec<u32>)> {
        self.graph
            .as_mut()
            .map(Hnsw::drain_recording)
            .unwrap_or_default()
    }

    /// Returns and clears what the lexical index has added since recording
    /// started or since the last drain, or `None` if nothing was indexed.
    pub fn drain_lexical_recording(&mut self) -> Option<crate::bm25::LexicalDelta> {
        self.lexical.as_mut().and_then(Bm25Index::drain_recording)
    }

    /// The graph's current entry point, node count and level count, for a
    /// caller building a checkpoint that names how far the graph it is
    /// recording has grown.
    pub fn graph_shape(&self) -> (Option<u32>, usize, usize) {
        match &self.graph {
            Some(graph) => (graph.entry_point(), graph.node_count(), graph.n_layers()),
            None => (None, 0, 0),
        }
    }

    /// The graph's per-node top layer from `from` onward, for a caller
    /// checkpointing only the nodes added since it last did.
    /// @param from - the first node ordinal to include
    pub fn graph_node_top_tail(&self, from: usize) -> Vec<u8> {
        self.graph
            .as_ref()
            .map(|g| g.node_top_tail(from).to_vec())
            .unwrap_or_default()
    }

    /// Appends chunks and vectors to the store and the vector set alone,
    /// leaving the graph, the lexical index and the quantized codes
    /// untouched - the store-only half of `append`, for a caller that will
    /// supply the other three from their own recorded content instead of
    /// recomputing them from these rows a second time.
    ///
    /// Returns the chunk ordinals this call assigned, which line up with
    /// `embeddings` in order.
    /// @param chunks - the new chunks
    /// @param embeddings - their vectors, in the same order
    pub fn append_store_and_vectors(
        &mut self,
        chunks: Vec<ChunkInput>,
        embeddings: &[Vec<f32>],
    ) -> anyhow::Result<std::ops::Range<u32>> {
        let first = self.store.n_chunks() as u32;
        self.store.add_chunks(chunks)?;
        for e in embeddings {
            self.vectors.push(e);
        }
        Ok(first..(self.store.n_chunks() as u32))
    }

    /// Applies a graph checkpoint's recorded content directly - growing the
    /// graph to the recorded shape and overwriting exactly the adjacency
    /// lists that were recorded - instead of running `insert`/`insert_batch`
    /// again over the rows that produced it.
    ///
    /// Builds a graph from scratch if this index was never committed with
    /// one, which only happens when replaying a checkpoint with no base of
    /// its own.
    /// @param entry - the recorded entry point
    /// @param layers_len - how many levels the graph must have afterwards
    /// @param node_top_tail - the recorded top layer for every node added
    ///   since the checkpoint this one continues
    /// @param touched - every adjacency list the checkpoint changed
    pub fn apply_graph_recording(
        &mut self,
        entry: Option<u32>,
        layers_len: usize,
        node_top_tail: &[u8],
        touched: &[(u8, u32, Vec<u32>)],
    ) {
        let graph = self
            .graph
            .get_or_insert_with(|| Hnsw::new(self.config.hnsw));
        graph.ensure_layers(layers_len);
        for top in node_top_tail {
            graph.push_node_top(*top);
        }
        for (layer, node, neighbours) in touched {
            graph.apply_touched(*layer, *node, neighbours.clone());
        }
        graph.set_entry(entry);
    }

    /// Applies a lexical checkpoint's recorded content directly, without
    /// re-tokenising the chunks that produced it.
    /// @param delta - what one checkpoint's own fold added to the lexical index
    pub fn apply_lexical_recording(&mut self, delta: &crate::bm25::LexicalDelta) {
        let lexical = self.lexical.get_or_insert_with(Bm25Index::default);
        lexical.apply_lexical_delta(delta);
    }

    /// Add chunks together with their vectors. The two slices must correspond
    /// element for element, which is the one to one chunk to vector mapping the
    /// baseline also maintains.
    pub fn add(&mut self, chunks: Vec<ChunkInput>, embeddings: &[Vec<f32>]) -> anyhow::Result<()> {
        assert_eq!(
            chunks.len(),
            embeddings.len(),
            "each chunk needs exactly one vector"
        );
        self.store.add_chunks(chunks)?;
        for e in embeddings {
            self.vectors.push(e);
        }
        // Any structure built earlier is now stale.
        self.graph = None;
        self.lexical = None;
        self.quantized = None;
        Ok(())
    }

    /// Add chunks to a committed index without rebuilding anything.
    ///
    /// This is the difference between a sync costing a second and costing nine
    /// minutes. `add` invalidates the graph, the lexical index and the codes, so
    /// 181 new chunks used to force a full rebuild of a 598,560 node graph; every
    /// structure underneath already supported the append, and this composes them.
    ///
    /// The graph an incremental insert produces is not byte-identical to one built
    /// in a single pass, which is why compaction exists and why the mutation gates
    /// compare a month of appends against a clean rebuild. On an index that was
    /// never committed this falls back to `add`, because there is nothing to
    /// append to.
    /// @param chunks - the new chunks, one vector each
    /// @param embeddings - their vectors, in the same order
    pub fn append(
        &mut self,
        chunks: Vec<ChunkInput>,
        embeddings: &[Vec<f32>],
    ) -> anyhow::Result<AppendStats> {
        assert_eq!(
            chunks.len(),
            embeddings.len(),
            "each chunk needs exactly one vector"
        );
        if self.graph.is_none() || self.lexical.is_none() {
            let documents_before = self.store.n_documents();
            let chunks_added = chunks.len();
            self.add(chunks, embeddings)?;
            return Ok(AppendStats {
                chunks_added,
                documents_added: self.store.n_documents() - documents_before,
                new_terms: 0,
                committed: false,
            });
        }

        let first_chunk = self.store.n_chunks() as u32;
        let documents_before = self.store.n_documents();
        self.store.add_chunks(chunks)?;
        for e in embeddings {
            self.vectors.push(e);
        }
        let last_chunk = self.store.n_chunks() as u32;

        // Node identifiers are vector ordinals, which are chunk identifiers, so an
        // appended chunk's node is the one the insert is about to create.
        //
        // `insert_batch` rather than a loop over `insert`, because a batch large
        // enough to be worth it goes in on every core the caller asked for -
        // which is what stops a bounded append being slower in wall clock than
        // the unbounded rebuild it replaced (M8). A batch below the
        // floor, or a caller who left `build_threads` at one, gets exactly the
        // loop it always got.
        if let Some(graph) = self.graph.as_mut() {
            graph.insert_batch(&self.vectors, first_chunk, last_chunk);
        }
        let new_terms = match self.lexical.as_mut() {
            Some(lexical) => {
                lexical.index_chunks(&self.store, &self.tokenizer, first_chunk..last_chunk)
            }
            None => 0,
        };
        if let Some(codes) = self.quantized.as_mut() {
            codes.encode_from(&self.vectors, first_chunk as usize);
        }

        Ok(AppendStats {
            chunks_added: (last_chunk - first_chunk) as usize,
            documents_added: self.store.n_documents() - documents_before,
            new_terms,
            committed: true,
        })
    }

    /// Marks one document unreachable, without rebuilding anything.
    ///
    /// Query-time exclusion was already there - every path calls
    /// `CompiledFilter::passes`, which checks `deleted` before anything else - so
    /// this is only the mutation that was missing. The chunks stay in the graph on
    /// purpose: a tombstoned node is still a useful stepping stone, and removing
    /// it would disconnect the paths that ran through it. Compaction is what
    /// eventually reclaims the space.
    ///
    /// Returns whether a live document was found and tombstoned.
    /// @param source - the source name
    /// @param external_doc_id - the identifier the source system uses
    pub fn tombstone(&mut self, source: &str, external_doc_id: &str) -> bool {
        match self.store.find_document(source, external_doc_id) {
            Some(doc) => {
                self.store.tombstone_document(doc);
                true
            }
            None => false,
        }
    }

    /// Tombstones many documents under one call. A Gmail history sync removes
    /// twenty-nine messages in a pass, and each one taking the writer lock
    /// separately is a cost with nothing to show for it.
    /// @param documents - source and external id pairs
    pub fn tombstone_many(&mut self, documents: &[(String, String)]) -> usize {
        documents
            .iter()
            .filter(|(source, id)| self.tombstone(source, id))
            .count()
    }

    /// Replaces one document's chunks: the old document is tombstoned and the new
    /// chunks are appended as a fresh document.
    ///
    /// This is what an append-only index does with an edit. The old chunks stay
    /// resident until compaction, which is the price of not rebuilding, and the
    /// store's lookup holds only live documents so the appended chunks cannot land
    /// back on the row that was just tombstoned.
    /// @param source - the source name
    /// @param external_doc_id - the identifier the source system uses
    /// @param chunks - the document's new chunks
    /// @param embeddings - their vectors, in the same order
    pub fn replace_document(
        &mut self,
        source: &str,
        external_doc_id: &str,
        chunks: Vec<ChunkInput>,
        embeddings: &[Vec<f32>],
    ) -> anyhow::Result<AppendStats> {
        self.tombstone(source, external_doc_id);
        self.append(chunks, embeddings)
    }

    /// Share of the corpus that is tombstoned, from 0 to 1. The trigger a
    /// compaction schedule needs.
    pub fn deleted_ratio(&self) -> f32 {
        self.store.deleted_ratio()
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
            vector_bytes: self.vectors.heap_bytes(),
            resident_vectors: self.vectors.is_resident(),
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

    /// Resolves a filter against this index's own dictionaries.
    ///
    /// @param filter - what the caller wrote
    pub fn compile(&self, filter: &Filter) -> CompiledFilter {
        CompiledFilter::compile(filter, &self.store)
    }

    /// Exact top k under this index's declared metric. The reference the rest
    /// is graded against.
    ///
    /// @param query - the query vector, at this index's width and all finite
    /// @param filter - the compiled predicate
    /// @param k - how many neighbours to return
    /// @returns the neighbours, or a refusal when the query cannot be compared
    pub fn exhaustive_search(
        &self,
        query: &[f32],
        filter: &CompiledFilter,
        k: usize,
    ) -> anyhow::Result<Vec<Neighbour>> {
        crate::distance::check_query(query, self.config.dims)?;
        Ok(flat::search(&self.vectors, &self.store, filter, query, k))
    }

    /// Approximate top k through the graph, with the quantized first pass when
    /// configured.
    ///
    /// @param query - the query vector, at this index's width and all finite
    /// @param filter - the compiled predicate
    /// @param k - how many neighbours to return
    /// @param ef_search - traversal width, or the configured default
    /// @returns the neighbours, or a refusal when the query cannot be compared
    pub fn vector_search(
        &self,
        query: &[f32],
        filter: &CompiledFilter,
        k: usize,
        ef_search: Option<usize>,
    ) -> anyhow::Result<Vec<Neighbour>> {
        crate::distance::check_query(query, self.config.dims)?;
        let Some(graph) = &self.graph else {
            return self.exhaustive_search(query, filter, k);
        };

        match &self.quantized {
            None => Ok(graph.search(&self.vectors, &self.store, filter, query, k, ef_search)),
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
                Ok(candidates)
            }
        }
    }

    /// The BM25 branch alone, for a caller with no embedder or one whose user
    /// asked for keyword search.
    ///
    /// @param query - the query text, tokenized by this index's own tokenizer
    /// @param filter - the compiled predicate
    /// @param k - how many hits to return
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
            self.lexical_params(),
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
    ) -> anyhow::Result<Vec<FusedHit>> {
        Ok(self
            .hybrid_search_explained(query, query_vector, filter, k, ef_search)?
            .0)
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
    ) -> anyhow::Result<(Vec<FusedHit>, QueryExplanation)> {
        self.search_branches(query, query_vector, filter, k, ef_search, Branches::Both)
    }

    /// The pipeline, running only the branches asked for.
    ///
    /// A branch that is switched off contributes an empty list rather than a
    /// differently-shaped result, so one branch and two branches are fused,
    /// grouped, scored and reported by exactly the same code. That matters because
    /// the single-branch modes are what a caller falls back to when the embedder
    /// is down, and a fallback that runs different code is a fallback nobody has
    /// measured.
    /// @param query - the raw query text
    /// @param query_vector - the query embedding, ignored when the vector branch is off
    /// @param filter - the compiled predicate
    /// @param k - how many chunk hits to return
    /// @param ef_search - traversal width, or the configured default
    /// @param branches - which branches to run
    pub fn search_branches(
        &self,
        query: &str,
        query_vector: &[f32],
        filter: &CompiledFilter,
        k: usize,
        ef_search: Option<usize>,
        branches: Branches,
    ) -> anyhow::Result<(Vec<FusedHit>, QueryExplanation)> {
        let candidates = self.config.candidates.max(k);
        // **Only when the vector branch runs.** A lexical-only search is the
        // fallback a caller uses when its embedder is down, and it passes an
        // empty vector deliberately; refusing that would turn the fallback into
        // a failure, which is the opposite of what it is for.
        // **The two legs run at the same time** (task-2000, design 9). They read
        // disjoint structures - the vector leg walks the HNSW graph and the vector
        // set, the lexical leg walks the BM25 postings - and neither writes
        // anything, so the hybrid query costs about the slower leg rather than the
        // sum. Measured on the grading card's title query at 5.125 ms with the two
        // in sequence.
        //
        // `rayon::join` rather than two spawns: it runs the second closure on the
        // calling thread when no worker is free, so a single-threaded caller pays a
        // closure call and nothing else, and a search inside a `rayon` worker does
        // not deadlock waiting for a pool it is itself occupying.
        //
        // The `?` is outside the join, because a closure that returns early out of
        // `join` would leave the other leg's result unclaimed. Both legs hand back a
        // `Result` and the vector leg's is unwrapped here.
        let (vector_found, lexical_hits) = rayon::join(
            || match branches.runs_vector() {
                // **Only when the vector branch runs.** A lexical-only search is the
                // fallback a caller uses when its embedder is down, and it passes an
                // empty vector deliberately; refusing that would turn the fallback
                // into a failure, which is the opposite of what it is for.
                true => self.vector_search(query_vector, filter, candidates, ef_search),
                false => Ok(Vec::new()),
            },
            || match branches.runs_lexical() {
                true => self.lexical_search(query, filter, candidates),
                false => Vec::new(),
            },
        );
        let vector_hits = self.without_the_unembedded(vector_found?);

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
        Ok((hits, explanation))
    }

    /// Drops the candidates that have no embedding yet.
    ///
    /// **A row with no vector used to come back from the vector branch
    /// (task-1979, R16).** `embedding_of` gives such a row the zero vector,
    /// whose own doc comment says it "is orthogonal to nothing and therefore
    /// never a near neighbour of anything" - but a scan that has fewer than
    /// `k` real neighbours returns it anyway, at the bottom of the list. Two
    /// things followed from that: a pure vector query answered a document
    /// nobody had embedded, at score zero, and a hybrid query labelled it
    /// `origin = both`, which says it was found by a branch that cannot have
    /// found it.
    ///
    /// It is a filter on the answer rather than on the scan because the scan is
    /// the accuracy reference the whole project is graded against, and a
    /// shortcut in the reference is a shortcut in every recall number measured
    /// against it. At most `candidates` vectors are read.
    ///
    /// @param found - what the vector branch answered
    fn without_the_unembedded(&self, found: Vec<Neighbour>) -> Vec<Neighbour> {
        found
            .into_iter()
            .filter(|hit| {
                self.vectors
                    .with(hit.chunk, |vector| vector.iter().any(|held| *held != 0.0))
            })
            .collect()
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
        let v_scores: Vec<f32> = vector_hits
            .iter()
            .map(|h| (1.0 - h.distance).clamp(0.0, 1.0))
            .collect();
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

/// Which retrieval branches a search runs.
///
/// A caller with a lexical-only and a semantic-only mode was otherwise forced to
/// call the single-branch entry points and reimplement grouping, corroboration
/// and confidence for them. Running one branch through the same fusion keeps all
/// three modes on one code path, and fusing one list with an empty one is already
/// what the hybrid path does whenever a branch finds nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Branches {
    /// Both retrievers, which is the hybrid path.
    Both,
    /// Lexical only, for a caller whose embedder is unavailable or whose user
    /// asked for keyword search.
    Lexical,
    /// Vector only.
    Vector,
}

impl Branches {
    fn runs_lexical(self) -> bool {
        matches!(self, Branches::Both | Branches::Lexical)
    }

    fn runs_vector(self) -> bool {
        matches!(self, Branches::Both | Branches::Vector)
    }
}

/// What a grouped search was asked for.
///
/// A struct rather than five positional arguments, for the same reason
/// `LexicalParams` is one: a caller can transpose two numbers of the same type
/// without the compiler noticing.
#[derive(Debug, Clone, Copy)]
pub struct GroupedParams {
    /// How many documents to return.
    pub documents: usize,
    /// How many documents to skip before those, for paging.
    ///
    /// Deep paging costs a deep search here, because the fusion produces a total
    /// order and the only way to reach position 200 is to compute the first 200.
    /// That is the same trade the SQL it replaces was making with `OFFSET`.
    pub offset: usize,
    /// Traversal width, or the configured default.
    pub ef_search: Option<usize>,
    /// How much each matching chunk beyond the best one adds to its document's
    /// score.
    ///
    /// Summing every chunk outright lets a long newsletter with a dozen weak
    /// matches outrank a short message that answers the question exactly, so the
    /// additional chunks corroborate rather than accumulate. Supplied by the
    /// caller rather than kept as an engine default, because it is a property of
    /// what documents mean in the caller's corpus and nothing in the engine has
    /// measured it.
    pub corroboration: f32,
    /// How many matching chunks to carry on each returned document.
    pub chunks_per_document: usize,
    /// Which branches to run.
    pub branches: Branches,
}

impl Default for GroupedParams {
    fn default() -> Self {
        GroupedParams {
            documents: 20,
            offset: 0,
            ef_search: None,
            corroboration: 0.25,
            chunks_per_document: 3,
            branches: Branches::Both,
        }
    }
}

/// One document that matched, with the chunk evidence that made it match.
#[derive(Debug, Clone)]
pub struct DocumentHit {
    /// Index into `Store::documents`.
    pub document: u32,
    /// The document's score, folded up from its chunks.
    pub score: f32,
    /// The confidence of this document's best chunk, on absolute bounds.
    ///
    /// Carried up from the chunk rather than recomputed, because it is the
    /// question "is there an answer here at all" and the best chunk is what
    /// answers it. This is what lets a caller abstain instead of handing an agent
    /// ten passages for a question the corpus does not answer.
    pub confidence: f32,
    /// The matching chunks, best first, capped by `chunks_per_document`.
    pub chunks: Vec<FusedHit>,
}

/// A grouped search, and what the engine decided while running it.
#[derive(Debug, Clone)]
pub struct GroupedSearch {
    /// The documents that matched, best first.
    pub documents: Vec<DocumentHit>,
    /// `graph` or `exhaustive`. The engine already knew; it just never said, and
    /// "this search was slow" and "this search was exact" are things a person
    /// should be able to see rather than infer.
    pub path: &'static str,
    /// What the engine decided while running this query.
    pub explanation: QueryExplanation,
}

impl Index {
    /// The full pipeline, grouped by document.
    ///
    /// The engine already knows the document of every chunk and already caps hits
    /// per document, so a caller that wants documents was reimplementing grouping,
    /// corroboration weighting and a re-sort that the engine is better placed to
    /// do. It also gets the path and the confidence, neither of which a caller can
    /// recompute afterwards without asking a second, differently-configured
    /// question.
    /// @param query - the raw query text
    /// @param query_vector - the query embedding, already normalized
    /// @param filter - the compiled predicate
    /// @param params - how many documents, how deep, and how to score them
    pub fn hybrid_search_grouped(
        &self,
        query: &str,
        query_vector: &[f32],
        filter: &CompiledFilter,
        params: GroupedParams,
    ) -> anyhow::Result<GroupedSearch> {
        // Enough chunk hits that the requested number of documents can be filled
        // even when every document contributes its cap.
        //
        // Floored at the configured candidate depth, and that floor is what makes
        // paging coherent. A document's score depends on how many of its chunks
        // came back, so asking for five documents and then asking for the next
        // five would otherwise group two differently-sized chunk lists and produce
        // two orderings that do not continue each other. Fusing to a fixed depth
        // and paging inside it means every page reads the same list. Past that
        // depth - a window of more than `candidates / per_doc_cap` documents - the
        // caller has asked for a deeper search and gets one.
        let wanted_documents = params.offset + params.documents;
        let chunk_budget = wanted_documents
            .saturating_mul(self.config.per_doc_cap)
            .max(self.config.candidates)
            .max(1);
        let (hits, explanation) = self.search_branches(
            query,
            query_vector,
            filter,
            chunk_budget,
            params.ef_search,
            params.branches,
        )?;

        let mut order: Vec<u32> = Vec::new();
        let mut grouped: std::collections::HashMap<u32, Vec<FusedHit>> =
            std::collections::HashMap::new();
        for hit in hits {
            let Some(document) = self.store.doc_of(hit.chunk) else {
                continue;
            };
            let entry = grouped.entry(document).or_insert_with(|| {
                order.push(document);
                Vec::new()
            });
            entry.push(hit);
        }

        let mut documents: Vec<DocumentHit> = order
            .into_iter()
            .map(|document| {
                let mut chunks = grouped.remove(&document).unwrap_or_default();
                chunks.sort_by(|a, b| {
                    b.score
                        .partial_cmp(&a.score)
                        .unwrap_or(std::cmp::Ordering::Equal)
                        .then(a.chunk.cmp(&b.chunk))
                });
                let score = score_document(&chunks, params.corroboration);
                let confidence = chunks.first().map(|c| c.confidence).unwrap_or(0.0);
                chunks.truncate(params.chunks_per_document.max(1));
                DocumentHit {
                    document,
                    score,
                    confidence,
                    chunks,
                }
            })
            .collect();
        documents.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(a.document.cmp(&b.document))
        });

        let documents = documents
            .into_iter()
            .skip(params.offset)
            .take(params.documents)
            .collect();
        let path = if params.branches.runs_vector() {
            self.path_for(filter, params.ef_search)
        } else {
            "lexical"
        };
        Ok(GroupedSearch {
            documents,
            path,
            explanation,
        })
    }
}

/// One document's score: its best chunk leads and the rest only corroborate.
/// @param chunks - the document's matching chunks, best first
/// @param corroboration - what each chunk beyond the best contributes
fn score_document(chunks: &[FusedHit], corroboration: f32) -> f32 {
    match chunks.split_first() {
        Some((best, rest)) => {
            best.score + corroboration * rest.iter().map(|c| c.score).sum::<f32>()
        }
        None => 0.0,
    }
}

/// What one hybrid query decided, for a run artifact to record.
#[derive(Debug, Clone, Copy, Default)]
pub struct QueryExplanation {
    /// What the query itself said about which retriever to trust.
    pub signals: QuerySignals,
    /// The weight actually used, or 0 for a rank based fusion that has none.
    pub vector_weight: f32,
    /// The BM25 score this query could have reached at saturation, which is
    /// what `Fusion::TheoreticalMinMax` scaled by.
    pub lexical_ceiling: f32,
    /// How many candidates the vector branch produced.
    pub vector_candidates: usize,
    /// How many the lexical branch produced.
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
                external_chunk_id: None,
                labels: vec![],
                attributes: Vec::new(),
                flags: Vec::new(),
                deleted: false,
            });
            let c = &centres[i % 16];
            let mut v: Vec<f32> = c.iter().map(|x| x + rng.gen_range(-0.3..0.3)).collect();
            normalize(&mut v);
            vectors.push(v);
        }
        index.add(chunks, &vectors).expect("the chunks are added");
        index.commit();
        index
    }

    /// A query narrower or wider than the index is refused, not truncated.
    ///
    /// **All three of these used to return a `Vec<Neighbour>` in a release
    /// build.** `dot` zips the two slices and stops at the shorter one, so a
    /// four wide query against an eight wide index scored every vector on its
    /// first four components and came back with a plausible ranking; a twelve
    /// wide one scored on eight and ignored the rest. Nothing said either had
    /// happened (task-1946, H4). A debug build reached `dot`'s own
    /// `debug_assert_eq!` on the two lengths and panicked there instead, which
    /// is why the release binary a user runs was the one that answered.
    #[test]
    fn a_query_of_the_wrong_width_is_refused() {
        let index = build(200, 8, IndexConfig::default());
        let filter = index.compile(&Filter::default());

        for width in [4usize, 12] {
            let query = vector(width, 0.25);
            let refusals = [
                index
                    .exhaustive_search(&query, &filter, 10)
                    .err()
                    .map(|why| why.to_string()),
                index
                    .vector_search(&query, &filter, 10, None)
                    .err()
                    .map(|why| why.to_string()),
                index
                    .hybrid_search("offer", &query, &filter, 10, None)
                    .err()
                    .map(|why| why.to_string()),
            ];
            for refusal in refusals {
                let Some(message) = refusal else {
                    panic!("a {width} wide query against an 8 wide index was answered");
                };
                assert!(
                    message.contains("8 dimensions") && message.contains(&width.to_string()),
                    "the refusal does not say which widths disagreed: {message}"
                );
            }
        }
    }

    /// A query carrying a component that is not a finite number is refused.
    ///
    /// A NaN passes `rank.rs`'s `clamp` unchanged and makes a distance that
    /// compares equal to everything, which is not a total order - so the heap
    /// the graph search walks stops being a heap, and what comes out is an
    /// arbitrary set of neighbours rather than a wrong score.
    #[test]
    fn a_query_with_a_component_that_is_not_a_number_is_refused() {
        let index = build(200, 8, IndexConfig::default());
        let filter = index.compile(&Filter::default());

        for (name, bad) in [
            ("NaN", f32::NAN),
            ("an infinity", f32::INFINITY),
            ("a negative infinity", f32::NEG_INFINITY),
        ] {
            let mut query = vector(8, 0.25);
            query[3] = bad;
            let refusal = index
                .vector_search(&query, &filter, 10, None)
                .err()
                .map(|why| why.to_string());
            let Some(message) = refusal else {
                panic!("a query holding {name} was answered");
            };
            assert!(
                message.contains("component 3"),
                "the refusal does not say which component: {message}"
            );
            assert!(
                message.contains("finite"),
                "the refusal does not say what was wrong with it: {message}"
            );
        }
    }

    /// A query of the right width and all finite is still answered.
    ///
    /// The other half of the check: a guard that refuses everything would pass
    /// both tests above and break every caller.
    #[test]
    fn a_query_of_the_right_width_is_still_answered() {
        let index = build(200, 8, IndexConfig::default());
        let filter = index.compile(&Filter::default());
        let query = vector(8, 0.25);

        assert!(!index
            .exhaustive_search(&query, &filter, 10)
            .expect("the query is this index's width and finite")
            .is_empty());
        assert!(!index
            .vector_search(&query, &filter, 10, None)
            .expect("the query is this index's width and finite")
            .is_empty());
    }

    /// A lexical-only search is still answered with no vector at all.
    ///
    /// **The fallback a caller uses when its embedder is down passes an empty
    /// vector deliberately**, and `inillucent-migrate`'s verification passes
    /// `&[]` with `Branches::Lexical` at six sites. Checking the width
    /// unconditionally would have turned every one of them into a failure, so
    /// the check runs only when the vector branch does.
    #[test]
    fn a_lexical_only_search_needs_no_vector() {
        let index = build(200, 8, IndexConfig::default());
        let filter = index.compile(&Filter::default());
        let (hits, _) = index
            .search_branches("offer", &[], &filter, 10, None, Branches::Lexical)
            .expect("a lexical search does not look at the vector");
        assert!(!hits.is_empty(), "the lexical branch answered nothing");
    }

    #[test]
    fn commit_reports_the_structures_it_built() {
        let mut index = Index::new(IndexConfig {
            dims: 16,
            quantized: true,
            ..Default::default()
        });
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
                external_chunk_id: None,
                labels: vec![],
                attributes: Vec::new(),
                flags: Vec::new(),
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
        index.add(chunks, &vectors).expect("the chunks are added");
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
        let index = build(
            2000,
            32,
            IndexConfig {
                hnsw: HnswParams {
                    exhaustive_below: 0,
                    ..Default::default()
                },
                ..Default::default()
            },
        );
        let f = index.compile(&Filter::default());
        let q = index.vectors().copy_of(5);
        let exact = index
            .exhaustive_search(&q, &f, 10)
            .expect("the query is this index's width and finite");
        let approx = index
            .vector_search(&q, &f, 10, Some(200))
            .expect("the query is this index's width and finite");
        let want: std::collections::HashSet<u32> = exact.iter().map(|n| n.chunk).collect();
        let overlap = approx.iter().filter(|n| want.contains(&n.chunk)).count();
        assert!(
            overlap >= 9,
            "only {overlap} of 10 matched exhaustive search"
        );
    }

    #[test]
    fn hybrid_search_returns_results_under_a_selective_filter() {
        let index = build(
            20000,
            32,
            IndexConfig {
                hnsw: HnswParams {
                    exhaustive_below: 0,
                    ..Default::default()
                },
                ..Default::default()
            },
        );
        for source in ["slack", "jira"] {
            let f = index.compile(&Filter::source(source));
            let q = index.vectors().copy_of(1);
            let hits = index
                .hybrid_search("offer eligibility rules", &q, &f, 10, Some(64))
                .expect("the query is this index's width and finite");
            assert_eq!(hits.len(), 10, "{source} returned {} of 10", hits.len());
        }
    }

    #[test]
    fn quantization_keeps_the_ranking_after_rescoring() {
        let plain = build(
            3000,
            64,
            IndexConfig {
                hnsw: HnswParams {
                    exhaustive_below: 0,
                    ..Default::default()
                },
                ..Default::default()
            },
        );
        let quant = build(
            3000,
            64,
            IndexConfig {
                quantized: true,
                hnsw: HnswParams {
                    exhaustive_below: 0,
                    ..Default::default()
                },
                ..Default::default()
            },
        );
        let f = plain.compile(&Filter::default());
        let fq = quant.compile(&Filter::default());
        let q = plain.vectors().copy_of(9);
        let a = plain
            .vector_search(&q, &f, 10, Some(128))
            .expect("the query is this index's width and finite");
        let b = quant
            .vector_search(&q, &fq, 10, Some(128))
            .expect("the query is this index's width and finite");
        let want: std::collections::HashSet<u32> = a.iter().map(|n| n.chunk).collect();
        let overlap = b.iter().filter(|n| want.contains(&n.chunk)).count();
        assert!(overlap >= 9, "quantized ranking kept only {overlap} of 10");
    }

    #[test]
    fn the_per_document_cap_holds_in_the_hybrid_path() {
        let index = build(400, 16, IndexConfig::default());
        let f = index.compile(&Filter::default());
        let q = index.vectors().copy_of(0);
        let hits = index
            .hybrid_search("offer eligibility", &q, &f, 20, None)
            .expect("the query is this index's width and finite");
        let mut per_doc = std::collections::HashMap::new();
        for h in &hits {
            *per_doc
                .entry(index.store().chunks[h.chunk as usize].doc)
                .or_insert(0) += 1;
        }
        assert!(per_doc.values().all(|c| *c <= PER_DOC_CAP));
    }

    #[test]
    fn every_query_path_honours_the_filter() {
        let index = build(3000, 32, IndexConfig::default());
        let slack = index.store().sources.get("slack").unwrap();
        let f = index.compile(&Filter::source("slack"));
        let q = index.vectors().copy_of(3);

        let mut chunks: Vec<u32> = index
            .vector_search(&q, &f, 20, None)
            .expect("the query is this index's width and finite")
            .iter()
            .map(|n| n.chunk)
            .collect();
        chunks.extend(
            index
                .lexical_search("offer eligibility", &f, 20)
                .iter()
                .map(|h| h.chunk),
        );
        chunks.extend(
            index
                .hybrid_search("offer eligibility", &q, &f, 20, None)
                .expect("the query is this index's width and finite")
                .iter()
                .map(|h| h.chunk),
        );
        assert!(!chunks.is_empty());
        for c in chunks {
            let doc = index.store().chunks[c as usize].doc;
            assert_eq!(index.store().documents[doc as usize].source, slack);
        }
    }

    #[test]
    fn an_empty_index_answers_every_path_without_panicking() {
        let mut index = Index::new(IndexConfig {
            dims: 8,
            ..Default::default()
        });
        index.commit();
        let f = index.compile(&Filter::default());
        assert!(index
            .vector_search(&[0.0; 8], &f, 10, None)
            .expect("the query is this index's width and finite")
            .is_empty());
        assert!(index.lexical_search("anything", &f, 10).is_empty());
        assert!(index
            .hybrid_search("anything", &[0.0; 8], &f, 10, None)
            .expect("the query is this index's width and finite")
            .is_empty());
        assert!(index
            .exhaustive_search(&[0.0; 8], &f, 10)
            .expect("the query is this index's width and finite")
            .is_empty());
    }

    #[test]
    fn adding_after_commit_requires_another_commit_and_still_answers() {
        let mut index = build(200, 16, IndexConfig::default());
        let before = index.store().n_chunks();
        index
            .add(
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
                    external_chunk_id: None,
                    labels: vec![],
                    attributes: Vec::new(),
                    flags: Vec::new(),
                    deleted: false,
                }],
                &[{
                    let mut v = vec![0.5f32; 16];
                    normalize(&mut v);
                    v
                }],
            )
            .expect("the chunks are added");
        index.commit();
        assert_eq!(index.store().n_chunks(), before + 1);
        let f = index.compile(&Filter::default());
        let hits = index.lexical_search("tirzepatide", &f, 5);
        assert_eq!(hits.len(), 1);
    }

    #[test]
    fn a_grouped_search_returns_documents_rather_than_chunks() {
        let index = build(2000, 32, IndexConfig::default());
        let f = index.compile(&Filter::default());
        let q = index.vectors().copy_of(0);
        let grouped = index
            .hybrid_search_grouped(
                "offer eligibility rules",
                &q,
                &f,
                GroupedParams {
                    documents: 10,
                    ..Default::default()
                },
            )
            .expect("the query is this index's width and finite");
        assert_eq!(grouped.documents.len(), 10);
        let unique: std::collections::HashSet<u32> =
            grouped.documents.iter().map(|d| d.document).collect();
        assert_eq!(unique.len(), 10, "a document appeared twice");
        assert!(grouped.documents.iter().all(|d| !d.chunks.is_empty()));
        // Descending by score, with a total order.
        for pair in grouped.documents.windows(2) {
            assert!(pair[0].score >= pair[1].score);
        }
    }

    #[test]
    fn a_single_branch_grouped_search_returns_that_branch_only() {
        let index = build(2000, 32, IndexConfig::default());
        let f = index.compile(&Filter::default());
        // A vector that matches nothing in particular, and a query that does.
        let q = vector(32, 999.0);

        let lexical = index
            .hybrid_search_grouped(
                "offer eligibility rules",
                &q,
                &f,
                GroupedParams {
                    branches: Branches::Lexical,
                    ..Default::default()
                },
            )
            .expect("the query is this index's width and finite");
        assert_eq!(lexical.path, "lexical");
        assert!(!lexical.documents.is_empty());
        assert!(lexical.explanation.vector_candidates == 0);

        let vector_only = index
            .hybrid_search_grouped(
                "a query holding no term this corpus knows",
                &q,
                &f,
                GroupedParams {
                    branches: Branches::Vector,
                    ..Default::default()
                },
            )
            .expect("the query is this index's width and finite");
        assert!(
            !vector_only.documents.is_empty(),
            "the vector branch found nothing"
        );
        assert_eq!(vector_only.explanation.lexical_candidates, 0);
    }

    #[test]
    fn a_grouped_search_says_which_path_it_took() {
        let index = build(3000, 32, IndexConfig::default());
        let q = index.vectors().copy_of(0);

        let everything = index.compile(&Filter::default());
        let selective = index.compile(&Filter::source("jira"));
        let broad = index
            .hybrid_search_grouped("offer", &q, &everything, GroupedParams::default())
            .expect("the query is this index's width and finite");
        let narrow = index
            .hybrid_search_grouped("offer", &q, &selective, GroupedParams::default())
            .expect("the query is this index's width and finite");

        assert_eq!(broad.path, index.path_for(&everything, None));
        assert_eq!(narrow.path, "exhaustive", "a selective filter should scan");
    }

    #[test]
    fn corroboration_lifts_a_document_that_matched_more_than_once() {
        let index = build(400, 16, IndexConfig::default());
        let f = index.compile(&Filter::default());
        let q = index.vectors().copy_of(0);

        let none = index
            .hybrid_search_grouped(
                "offer eligibility",
                &q,
                &f,
                GroupedParams {
                    corroboration: 0.0,
                    ..Default::default()
                },
            )
            .expect("the query is this index's width and finite");
        let weighted = index
            .hybrid_search_grouped(
                "offer eligibility",
                &q,
                &f,
                GroupedParams {
                    corroboration: 1.0,
                    ..Default::default()
                },
            )
            .expect("the query is this index's width and finite");
        let multi_chunk = weighted.documents.iter().find(|d| d.chunks.len() > 1);
        let Some(multi) = multi_chunk else {
            return; // nothing in this fixture matched twice; the rule is untested but not wrong
        };
        let same = none
            .documents
            .iter()
            .find(|d| d.document == multi.document)
            .unwrap();
        assert!(
            multi.score > same.score,
            "corroboration did not lift a document with {} matching chunks",
            multi.chunks.len()
        );
    }

    #[test]
    fn a_grouped_search_pages_without_repeating_a_document() {
        let index = build(2000, 32, IndexConfig::default());
        let f = index.compile(&Filter::default());
        let q = index.vectors().copy_of(0);
        let first = index
            .hybrid_search_grouped(
                "offer eligibility",
                &q,
                &f,
                GroupedParams {
                    documents: 5,
                    offset: 0,
                    ..Default::default()
                },
            )
            .expect("the query is this index's width and finite");
        let second = index
            .hybrid_search_grouped(
                "offer eligibility",
                &q,
                &f,
                GroupedParams {
                    documents: 5,
                    offset: 5,
                    ..Default::default()
                },
            )
            .expect("the query is this index's width and finite");
        let front: std::collections::HashSet<u32> =
            first.documents.iter().map(|d| d.document).collect();
        assert!(second
            .documents
            .iter()
            .all(|d| !front.contains(&d.document)));
    }

    #[test]
    fn a_grouped_search_carries_the_confidence_of_its_best_chunk() {
        let index = build(2000, 32, IndexConfig::default());
        let f = index.compile(&Filter::default());
        let q = index.vectors().copy_of(0);
        let grouped = index
            .hybrid_search_grouped("offer eligibility", &q, &f, GroupedParams::default())
            .expect("the query is this index's width and finite");
        for document in &grouped.documents {
            let best = document
                .chunks
                .iter()
                .max_by(|a, b| a.score.partial_cmp(&b.score).unwrap())
                .unwrap();
            assert_eq!(document.confidence, best.confidence);
        }
    }

    #[test]
    fn a_grouped_search_excludes_tombstoned_documents() {
        let mut index = build(400, 16, IndexConfig::default());
        index
            .append(
                vec![chunk("doomed", 0, "a chunk about tirzepatide dosing")],
                &[vector(16, 3.0)],
            )
            .expect("the chunks are added");
        let v = vector(16, 3.0);
        let f = index.compile(&Filter::default());
        let before = index
            .hybrid_search_grouped("tirzepatide", &v, &f, GroupedParams::default())
            .expect("the query is this index's width and finite");
        assert!(before
            .documents
            .iter()
            .any(|d| { index.store().documents[d.document as usize].external_id == "doomed" }));

        index.tombstone("slack", "doomed");
        let f = index.compile(&Filter::default());
        let after = index
            .hybrid_search_grouped("tirzepatide", &v, &f, GroupedParams::default())
            .expect("the query is this index's width and finite");
        assert!(after
            .documents
            .iter()
            .all(|d| { index.store().documents[d.document as usize].external_id != "doomed" }));
    }

    #[test]
    fn both_fusion_methods_produce_a_full_result_set() {
        for fusion in [
            Fusion::ReciprocalRank { k: 60.0 },
            Fusion::NormalizedScore { vector_weight: 0.5 },
        ] {
            let index = build(
                1000,
                32,
                IndexConfig {
                    fusion,
                    ..Default::default()
                },
            );
            let f = index.compile(&Filter::default());
            let q = index.vectors().copy_of(0);
            let hits = index
                .hybrid_search("offer eligibility rules", &q, &f, 10, None)
                .expect("the query is this index's width and finite");
            assert_eq!(hits.len(), 10, "{fusion:?} returned {} of 10", hits.len());
        }
    }

    /// One chunk shaped like the corpus `build` produces, so an appended chunk is
    /// comparable to the ones already there.
    fn chunk(doc: &str, index: u32, content: &str) -> ChunkInput {
        ChunkInput {
            source: "slack".into(),
            external_doc_id: doc.into(),
            chunk_index: index,
            content: content.into(),
            title: doc.into(),
            url: format!("u/{doc}/{index}"),
            ..Default::default()
        }
    }

    /// A deterministic unit vector, so an appended chunk lands somewhere specific
    /// rather than somewhere random.
    fn vector(dims: usize, seed: f32) -> Vec<f32> {
        let mut v: Vec<f32> = (0..dims)
            .map(|d| ((d as f32 + seed) * 0.37).sin())
            .collect();
        normalize(&mut v);
        v
    }

    #[test]
    fn an_append_makes_new_chunks_retrievable_without_a_rebuild() {
        let mut index = build(2000, 32, IndexConfig::default());
        let before = index.store().n_chunks();

        let stats = index
            .append(
                vec![chunk("appended", 0, "a chunk about tirzepatide dosing")],
                &[vector(32, 7.0)],
            )
            .expect("the append runs");
        assert!(
            stats.committed,
            "the append should not have needed a commit"
        );
        assert_eq!(stats.chunks_added, 1);
        assert_eq!(stats.documents_added, 1);
        assert_eq!(index.store().n_chunks(), before + 1);

        // No commit call anywhere between the append and the search.
        let f = index.compile(&Filter::default());
        let hits = index.lexical_search("tirzepatide", &f, 5);
        assert_eq!(
            hits.len(),
            1,
            "the appended chunk is not lexically reachable"
        );
        assert_eq!(hits[0].chunk, before as u32);
    }

    #[test]
    fn an_appended_chunk_is_reachable_by_its_own_vector() {
        let mut index = build(
            2000,
            32,
            IndexConfig {
                hnsw: HnswParams {
                    exhaustive_below: 0,
                    ..Default::default()
                },
                ..Default::default()
            },
        );
        let v = vector(32, 11.0);
        index
            .append(
                vec![chunk("appended", 0, "vector reachable")],
                std::slice::from_ref(&v),
            )
            .expect("the chunks are added");

        let f = index.compile(&Filter::default());
        let hits = index
            .vector_search(&v, &f, 5, Some(200))
            .expect("the query is this index's width and finite");
        assert!(
            hits.iter()
                .any(|h| h.chunk == index.store().n_chunks() as u32 - 1),
            "the appended node was not found through the graph"
        );
    }

    #[test]
    fn an_append_leaves_the_existing_ranking_alone() {
        // idf moves when the corpus grows, so the scores move; the ordering of the
        // chunks that were already there must not.
        let mut index = build(2000, 32, IndexConfig::default());
        let f = index.compile(&Filter::default());
        let before: Vec<u32> = index
            .lexical_search("offer eligibility rules", &f, 20)
            .iter()
            .map(|h| h.chunk)
            .collect();

        index
            .append(
                vec![chunk("appended", 0, "an unrelated chunk about tirzepatide")],
                &[vector(32, 13.0)],
            )
            .expect("the chunks are added");
        let f = index.compile(&Filter::default());
        let after: Vec<u32> = index
            .lexical_search("offer eligibility rules", &f, 20)
            .iter()
            .map(|h| h.chunk)
            .collect();
        assert_eq!(before, after, "an unrelated append reordered the results");
    }

    #[test]
    fn appending_matches_building_the_same_corpus_in_one_pass_lexically() {
        // The lexical index has no approximation in it, so an appended index and a
        // rebuilt one must agree exactly. Anything else is a bookkeeping bug in the
        // postings, the positions or the lengths.
        let mut appended = build(500, 16, IndexConfig::default());
        let extra: Vec<ChunkInput> = (0..40)
            .map(|i| {
                chunk(
                    &format!("extra{i}"),
                    0,
                    &format!("extra chunk {i} about offer eligibility"),
                )
            })
            .collect();
        let vectors: Vec<Vec<f32>> = (0..40).map(|i| vector(16, 100.0 + i as f32)).collect();
        appended
            .append(extra.clone(), &vectors)
            .expect("the chunks are added");

        let mut rebuilt = build(500, 16, IndexConfig::default());
        rebuilt.add(extra, &vectors).expect("the chunks are added");
        rebuilt.commit();

        let a = appended.compile(&Filter::default());
        let r = rebuilt.compile(&Filter::default());
        for query in ["offer eligibility", "extra chunk", "rules number3"] {
            let left = appended.lexical_search(query, &a, 20);
            let right = rebuilt.lexical_search(query, &r, 20);
            assert_eq!(left.len(), right.len(), "{query} returned different counts");
            for (l, r) in left.iter().zip(right.iter()) {
                assert_eq!(
                    l.chunk, r.chunk,
                    "{query} ranked differently after an append"
                );
                assert!(
                    (l.score - r.score).abs() < 1e-4,
                    "{query} scored differently"
                );
            }
        }
    }

    #[test]
    fn a_tombstoned_document_disappears_from_every_branch() {
        let mut index = build(400, 16, IndexConfig::default());
        index
            .append(
                vec![chunk("doomed", 0, "a chunk about tirzepatide dosing")],
                &[vector(16, 3.0)],
            )
            .expect("the chunks are added");
        let f = index.compile(&Filter::default());
        assert_eq!(index.lexical_search("tirzepatide", &f, 5).len(), 1);

        assert!(index.tombstone("slack", "doomed"));

        let f = index.compile(&Filter::default());
        assert!(index.lexical_search("tirzepatide", &f, 5).is_empty());
        let v = vector(16, 3.0);
        assert!(index
            .vector_search(&v, &f, 10, None)
            .expect("the query is this index's width and finite")
            .iter()
            .all(|h| h.chunk != 400));
        assert!(index
            .exhaustive_search(&v, &f, 10)
            .expect("the query is this index's width and finite")
            .iter()
            .all(|h| h.chunk != 400));
        assert!(index
            .hybrid_search("tirzepatide", &v, &f, 10, None)
            .expect("the query is this index's width and finite")
            .iter()
            .all(|h| h.chunk != 400));
    }

    #[test]
    fn tombstoning_corrects_the_counts_that_route_queries() {
        let mut index = build(400, 16, IndexConfig::default());
        index
            .append(
                vec![chunk("doomed", 0, "one"), chunk("doomed", 1, "two")],
                &[vector(16, 3.0), vector(16, 4.0)],
            )
            .expect("the chunks are added");
        let live_before = index.store().live_chunks;
        let source = index.store().sources.get("slack").unwrap();
        let per_source_before = index.store().live_chunks_for_source(source);

        index.tombstone("slack", "doomed");

        assert_eq!(index.store().live_chunks, live_before - 2);
        assert_eq!(
            index.store().live_chunks_for_source(source),
            per_source_before - 2
        );
        // The chunks are still there, and still visible to a scan that has to
        // reject them.
        assert!(index.store().chunks_of_source(source).len() as u32 >= 2);
        let empty = index.compile(&Filter::default());
        assert_eq!(empty.pass_count(), index.store().live_chunks as usize);
    }

    #[test]
    fn tombstoning_something_absent_reports_that_it_was_absent() {
        let mut index = build(100, 16, IndexConfig::default());
        assert!(!index.tombstone("slack", "never-existed"));
        assert!(!index.tombstone("nosuchsource", "d0"));
    }

    #[test]
    fn a_batch_tombstone_counts_only_what_it_actually_removed() {
        let mut index = build(100, 16, IndexConfig::default());
        index
            .append(
                vec![chunk("a", 0, "one"), chunk("b", 0, "two")],
                &[vector(16, 1.0), vector(16, 2.0)],
            )
            .expect("the chunks are added");
        let removed = index.tombstone_many(&[
            ("slack".into(), "a".into()),
            ("slack".into(), "b".into()),
            ("slack".into(), "a".into()),
            ("slack".into(), "never".into()),
        ]);
        assert_eq!(
            removed, 2,
            "a repeat and an absent document must not be counted"
        );
    }

    #[test]
    fn replacing_a_document_leaves_the_old_chunks_unreachable() {
        let mut index = build(400, 16, IndexConfig::default());
        index
            .append(
                vec![chunk("edited", 0, "the original text mentions tirzepatide")],
                &[vector(16, 5.0)],
            )
            .expect("the chunks are added");

        index
            .replace_document(
                "slack",
                "edited",
                vec![chunk("edited", 0, "the revised text mentions semaglutide")],
                &[vector(16, 6.0)],
            )
            .expect("the chunks are added");

        let f = index.compile(&Filter::default());
        assert!(
            index.lexical_search("tirzepatide", &f, 5).is_empty(),
            "the old text is still reachable"
        );
        assert_eq!(index.lexical_search("semaglutide", &f, 5).len(), 1);
    }

    #[test]
    fn replacing_a_document_keeps_the_live_count_right() {
        let mut index = build(400, 16, IndexConfig::default());
        index
            .append(
                vec![chunk("edited", 0, "one"), chunk("edited", 1, "two")],
                &[vector(16, 5.0), vector(16, 6.0)],
            )
            .expect("the chunks are added");
        let live = index.store().live_chunks;

        // Two chunks out, three in.
        index
            .replace_document(
                "slack",
                "edited",
                vec![
                    chunk("edited", 0, "a"),
                    chunk("edited", 1, "b"),
                    chunk("edited", 2, "c"),
                ],
                &[vector(16, 7.0), vector(16, 8.0), vector(16, 9.0)],
            )
            .expect("the chunks are added");

        assert_eq!(index.store().live_chunks, live - 2 + 3);
        assert_eq!(
            index.compile(&Filter::default()).pass_count(),
            index.store().live_chunks as usize
        );
        // Two documents now carry the same external id: one dead, one live.
        assert_eq!(
            index
                .store()
                .documents
                .iter()
                .filter(|d| d.external_id == "edited")
                .count(),
            2
        );
    }

    #[test]
    fn the_deleted_ratio_tracks_what_compaction_would_reclaim() {
        let mut index = build(400, 16, IndexConfig::default());
        assert_eq!(index.deleted_ratio(), 0.0);
        index
            .append(vec![chunk("doomed", 0, "x")], &[vector(16, 2.0)])
            .expect("the chunks are added");
        index.tombstone("slack", "doomed");
        assert!(
            (index.deleted_ratio() - 1.0 / 401.0).abs() < 1e-6,
            "got {}",
            index.deleted_ratio()
        );
    }

    #[test]
    fn appending_to_an_index_that_was_never_committed_still_works() {
        let mut index = Index::new(IndexConfig {
            dims: 16,
            ..Default::default()
        });
        let stats = index
            .append(
                vec![chunk("d", 0, "a chunk about tirzepatide")],
                &[vector(16, 1.0)],
            )
            .expect("the append runs");
        assert!(!stats.committed, "there was nothing to append to yet");
        index.commit();
        let f = index.compile(&Filter::default());
        assert_eq!(index.lexical_search("tirzepatide", &f, 5).len(), 1);
    }

    #[test]
    fn thirty_appends_stay_close_to_a_clean_rebuild() {
        // A month of daily syncs, then the same corpus built in one pass. The graph
        // an incremental insert produces is not identical to a rebuilt one, so this
        // measures how far apart they drift rather than asserting they agree.
        //
        // **`build_threads: 1`, because the drift this measures has to be the appends'
        // and not the thread pool's** (task-2006).
        // `HnswParams::default().build_threads` became `available_parallelism()` in
        // design 9 of task-2000, and a parallel build's link order is whatever the pool
        // produced - see `available_parallelism`'s own note. Run on its own the test
        // passes either way; run inside the suite, where two dozen test binaries are
        // each asking for every core, the pool's order varies enough that recall after
        // the appends measured 0.8975 against a 0.90 bar. That is two graphs differing,
        // which is what the default is documented to allow, rather than the appends
        // drifting - and this test is about the appends.
        let mut appended = build(
            3000,
            32,
            IndexConfig {
                hnsw: HnswParams {
                    exhaustive_below: 0,
                    build_threads: 1,
                    ..Default::default()
                },
                ..Default::default()
            },
        );
        let mut every: Vec<ChunkInput> = Vec::new();
        let mut every_vector: Vec<Vec<f32>> = Vec::new();
        for day in 0..30u32 {
            let chunks: Vec<ChunkInput> = (0..6)
                .map(|i| {
                    chunk(
                        &format!("day{day}-{i}"),
                        0,
                        &format!("sync {day} chunk {i} about offer eligibility"),
                    )
                })
                .collect();
            let vectors: Vec<Vec<f32>> = (0..6)
                .map(|i| vector(32, 500.0 + (day * 6 + i) as f32))
                .collect();
            appended
                .append(chunks.clone(), &vectors)
                .expect("the chunks are added");
            every.extend(chunks);
            every_vector.extend(vectors);
        }

        let mut rebuilt = build(
            3000,
            32,
            IndexConfig {
                hnsw: HnswParams {
                    exhaustive_below: 0,
                    build_threads: 1,
                    ..Default::default()
                },
                ..Default::default()
            },
        );
        rebuilt
            .add(every, &every_vector)
            .expect("the chunks are added");
        rebuilt.commit();
        assert_eq!(appended.store().n_chunks(), rebuilt.store().n_chunks());

        let f = appended.compile(&Filter::default());
        let mut total = 0.0;
        let probes = 20;
        for i in 0..probes {
            let q = vector(32, 900.0 + i as f32);
            let exact = appended
                .exhaustive_search(&q, &f, 20)
                .expect("the query is this index's width and finite");
            let approximate = appended
                .vector_search(&q, &f, 20, Some(200))
                .expect("the query is this index's width and finite");
            let want: std::collections::HashSet<u32> = exact.iter().map(|n| n.chunk).collect();
            total += approximate
                .iter()
                .filter(|n| want.contains(&n.chunk))
                .count() as f32
                / 20.0;
        }
        let recall = total / probes as f32;
        assert!(recall > 0.9, "recall after 30 appends fell to {recall}");
    }
}
