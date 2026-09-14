//! The graded scenarios.
//!
//! Every family states what it measures, gets the same query set for every
//! engine, and reports the number it measured rather than the number that would
//! be convenient.

use inillucent_core::embed_onnx::Device;
use std::collections::{BTreeMap, HashSet};
use std::path::PathBuf;
use std::time::Instant;

use anyhow::Result;
use inillucent_core::embed::MATRYOSHKA_WIDTHS;
use inillucent_core::filter::Filter;
use inillucent_core::hnsw::HnswParams;
use inillucent_core::index::{BuildStats, Index, IndexConfig};
use inillucent_core::rank::{AdaptiveWeights, Fusion};
use inillucent_core::store::ChunkInput;

use crate::corpus::Corpus;
use crate::engine::{
    exhaustive_reference, fuse_with, PgMode, PgVectorEngine, InillucentEngine, SearchEngine,
};
use crate::metrics::{
    graded_recall_at_k, ndcg_at_k_attainable, ndcg_graded_at_k, percentile, precision_at_k,
    reciprocal_rank, recall_at_k, success_at_k, Accumulator,
};
use crate::queryset::{self, GradedQuery, Perturbation};
use crate::report::{
    BuildFacts, GateResult, Measure, MetricRow, Role, Scenario, ScoreCard, Series,
};
use crate::runs::{self, HitRecord, QueryRecord, RunManifest, RunWriter};

/// Traversal width inillucent uses on a filtered query, matched to the `hnsw.ef_search`
/// the well configured baseline uses on one. It also moves the cost model's crossover:
/// exhaustive search is chosen below `sqrt(ef_search * 32 * chunks)`, which at 400 on
/// this corpus is 48,672 chunks, so the second largest source is scanned exactly
/// instead of walked approximately.
const FILTERED_EF_SEARCH: usize = 400;

const INILLUCENT: &str = "inillucent";
const SOURCES: &[&str] = &["confluence", "github", "slack", "jira", "figma", "miro"];

/// Build a inillucent index from the cached corpus. Returns the index, the mapping
/// from chunk ordinal back to the source database identifier, the build
/// statistics and the elapsed seconds.
pub fn build_index(
    corpus: &Corpus,
    limit: Option<usize>,
    quantized: bool,
) -> Result<(Index, Vec<String>, BuildStats, f64)> {
    build_index_at_width(corpus, limit, quantized, corpus.dims)
}

/// Build at a chosen Matryoshka width, truncating and renormalizing the stored
/// vectors. Used by the quantization ladder scenario.
pub fn build_index_at_width(
    corpus: &Corpus,
    limit: Option<usize>,
    quantized: bool,
    dims: usize,
) -> Result<(Index, Vec<String>, BuildStats, f64)> {
    let n = limit.unwrap_or(corpus.len()).min(corpus.len());
    build_selected(corpus, (0..n).collect(), quantized, dims)
}

/// Chunk ordinals spread evenly across the whole corpus.
///
/// A prefix of this corpus is not a sample of it: chunks are ordered by
/// identifier, which correlates with source, so the first 40,000 are entirely
/// confluence. Striding keeps all six sources represented.
pub fn strided_sample(n: usize, wanted: usize) -> Vec<usize> {
    if wanted >= n {
        return (0..n).collect();
    }
    let stride = (n as f64 / wanted as f64).max(1.0);
    (0..wanted)
        .map(|i| ((i as f64 * stride) as usize).min(n - 1))
        .collect()
}

/// Build over an explicit set of chunk ordinals.
pub fn build_selected(
    corpus: &Corpus,
    selected: Vec<usize>,
    quantized: bool,
    dims: usize,
) -> Result<(Index, Vec<String>, BuildStats, f64)> {
    let mut index = Index::new(IndexConfig {
        dims,
        quantized,
        hnsw: HnswParams::default(),
        ..Default::default()
    });

    let chunks: Vec<ChunkInput> = selected
        .iter()
        .map(|&i| &corpus.chunks[i])
        .map(|c| ChunkInput {
            source: c.source.clone(),
            external_doc_id: c.external_doc_id.clone(),
            chunk_index: c.chunk_index,
            heading_path: c.heading_path.clone(),
            content: c.content.clone(),
            title: c.title.clone(),
            url: c.url.clone(),
            space_key: c.space_key.clone(),
            author: c.author.clone(),
            author_id: c.author_id.clone(),
            updated_at: c.updated_at,
            external_chunk_id: None,
            labels: c.labels.clone(),
            attributes: Vec::new(),
            flags: Vec::new(),
            deleted: c.deleted,
        })
        .collect();

    let vectors: Vec<Vec<f32>> = selected
        .iter()
        .map(|&i| {
            if dims == corpus.dims {
                corpus.vectors[i].clone()
            } else {
                inillucent_core::distance::truncate_normalized(&corpus.vectors[i], dims)
            }
        })
        .collect();

    let keys: Vec<String> = selected.iter().map(|&i| corpus_key(corpus, i)).collect();

    let start = Instant::now();
    index.add(chunks, &vectors).expect("the chunks are added");
    let stats = index.commit();
    let elapsed = start.elapsed().as_secs_f64();

    Ok((index, keys, stats, elapsed))
}

/// The key both engines agree on: the document identifier and the chunk index.
/// The pgvector side selects `d.id || '#' || c.chunk_index` so the two match
/// exactly; without a shared key every comparison would silently score zero.
fn corpus_key(corpus: &Corpus, ordinal: usize) -> String {
    format!("{}#{}", corpus.chunks[ordinal].external_doc_id, corpus.chunks[ordinal].chunk_index)
}

fn keys_of(hits: &[crate::engine::Hit]) -> Vec<String> {
    hits.iter().map(|h| h.key.clone()).collect()
}

/// Map string keys to dense ordinals so the metric functions, which work on u32,
/// can be shared between the two engines.
pub struct KeySpace {
    ids: std::collections::HashMap<String, u32>,
}

impl KeySpace {
    pub fn new() -> Self {
        KeySpace { ids: std::collections::HashMap::new() }
    }
    pub fn id(&mut self, key: &str) -> u32 {
        let next = self.ids.len() as u32;
        *self.ids.entry(key.to_string()).or_insert(next)
    }
    pub fn ids_of(&mut self, keys: &[String]) -> Vec<u32> {
        keys.iter().map(|k| self.id(k)).collect()
    }
    pub fn set_of(&mut self, keys: &[String]) -> HashSet<u32> {
        keys.iter().map(|k| self.id(k)).collect()
    }
}

/// Everything a graded run is configured with.
///
/// A struct rather than fourteen positional arguments, and the same reason
/// `LexicalParams` exists: the list had grown past the point where a caller could
/// transpose two `f32`s without the compiler noticing, and every setting here also
/// has to be written into the run manifest, which is far easier to keep complete
/// when the settings are one value rather than fourteen.
pub struct GradeOptions {
    pub limit: Option<usize>,
    pub per_source: usize,
    /// The model the queries are embedded with, resolved from its own manifest.
    /// The corpus vectors already exist in the cache; this is the other half of
    /// the pair, and it has to be the same model or every query is asked in a
    /// space the documents were not embedded into.
    pub model: crate::models::ResolvedModel,
    pub database_url: String,
    pub inillucent_only: bool,
    pub device: Device,
    pub fusion: Fusion,
    pub lexical_coverage: f32,
    pub lexical_proximity: f32,
    pub lexical_prefix: bool,
    pub lexical_tier: bool,
    pub lexical_phrase: f32,
    pub lexical_rescore_depth: usize,
    pub adaptive_fusion: bool,
    pub adaptive: AdaptiveWeights,
    pub mmr_lambda: f32,
    /// Where per-query run artifacts are collected.
    pub runs_dir: PathBuf,
    /// Fixes every bootstrap interval and p-value the card reports.
    pub stats_seed: u64,
    /// Where and how hard to run the model, including the llama.cpp endpoint for
    /// a served arm.
    pub arm_options: crate::arm::ArmOptions,
    /// The corpus file, recorded so two cards can be checked to describe the same
    /// corpus before they are compared.
    pub cache_path: PathBuf,
}

impl GradeOptions {
    /// Every ranking setting, as the run manifest and the score card record it.
    ///
    /// A number is not reproducible without this, and the settings are exactly
    /// where two runs most often silently differ.
    fn arm(&self) -> BTreeMap<String, String> {
        let mut arm = BTreeMap::new();
        arm.insert("fusion".into(), format!("{:?}", self.fusion));
        arm.insert("lexical_coverage".into(), self.lexical_coverage.to_string());
        arm.insert("lexical_proximity".into(), self.lexical_proximity.to_string());
        arm.insert("lexical_prefix".into(), self.lexical_prefix.to_string());
        arm.insert("lexical_tier".into(), self.lexical_tier.to_string());
        arm.insert("lexical_phrase".into(), self.lexical_phrase.to_string());
        arm.insert("lexical_rescore_depth".into(), self.lexical_rescore_depth.to_string());
        arm.insert("adaptive_fusion".into(), self.adaptive_fusion.to_string());
        arm.insert("adaptive".into(), format!("{:?}", self.adaptive));
        arm.insert("mmr_lambda".into(), self.mmr_lambda.to_string());
        arm.insert("per_source".into(), self.per_source.to_string());
        arm.insert("filtered_ef_search".into(), FILTERED_EF_SEARCH.to_string());
        arm
    }
}

/// The seeds every query family is generated from.
///
/// Named and recorded rather than written inline, because a family generated from
/// a different seed is a different query set, and two cards compared without
/// noticing that are two cards about different questions. `calibration` is
/// deliberately far from the rest: it generates answerable queries the report does
/// not score, used only to choose each engine's abstention threshold, so the
/// threshold is not fitted on the queries it is then judged on.
pub fn seeds() -> BTreeMap<String, u64> {
    [
        ("identity", 11u64),
        ("heading", 12),
        ("identifier", 13),
        ("passage", 14),
        ("unanswerable", 15),
        ("multi_source", 16),
        ("calibration", 1012),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v))
    .collect()
}

pub fn seed(name: &str) -> u64 {
    seeds().get(name).copied().unwrap_or(0)
}

pub fn grade(corpus: &Corpus, options: &GradeOptions) -> Result<ScoreCard> {
    let GradeOptions {
        limit,
        per_source,
        model,
        database_url,
        inillucent_only,
        device,
        fusion,
        lexical_coverage,
        lexical_proximity,
        lexical_prefix,
        lexical_tier,
        ..
    } = options;
    let (limit, per_source, device) = (*limit, *per_source, *device);
    let (fusion, inillucent_only) = (*fusion, *inillucent_only);
    let database_url = database_url.as_str();
    let model_dir = model.dir.display().to_string();
    let model_file = model.manifest.model_file.clone();
    let (lexical_coverage, lexical_proximity) = (*lexical_coverage, *lexical_proximity);
    let (lexical_prefix, lexical_tier) = (*lexical_prefix, *lexical_tier);

    eprintln!("building the inillucent index");
    let (index, keys, stats, build_seconds) = build_index(corpus, limit, true)?;
    eprintln!("  built in {build_seconds:.1}s");

    let mut inillucent = InillucentEngine::new(index, keys.clone(), INILLUCENT.to_string(), Some(128));
    // The same asymmetry the well configured baseline is given: a wider traversal on
    // a filtered query, because a filtered walk has to step past everything the
    // predicate rejects. pgvector gets hnsw.ef_search 400 filtered against 100
    // unfiltered; without this inillucent was walking a quarter as wide on the one
    // family that is entirely about filtered search.
    inillucent.filtered_ef_search = Some(FILTERED_EF_SEARCH);
    // Both engines get the same ranking policy. Only the lexical coverage is
    // one-sided, and only because PostgreSQL already has what it buys: to_tsquery
    // joins the terms with `&`, so every row it returns contains all of them.
    // inillucent scores any term, which returns far more rows, and the exponent is
    // what stops the extra rows outranking the complete ones.
    inillucent.index.set_lexical_coverage(lexical_coverage);
    inillucent.index.set_lexical_proximity(lexical_proximity);
    inillucent.index.set_lexical_prefix(lexical_prefix);
    inillucent.index.set_lexical_tier(lexical_tier);
    inillucent.index.set_lexical_phrase(options.lexical_phrase);
    inillucent.index.set_lexical_rescore_depth(options.lexical_rescore_depth);
    inillucent.index.set_adaptive_fusion(options.adaptive_fusion, options.adaptive);
    inillucent.index.set_mmr_lambda(options.mmr_lambda);
    inillucent.index.set_fusion(fusion);

    // Query sets. Generated from the same slice of the corpus the index holds.
    let n = limit.unwrap_or(corpus.len()).min(corpus.len());
    // Query generation reads the chunks; it does not need a second copy of them.
    // Cloning every chunk with its text cost about a gigabyte resident.
    let sliced = &corpus.chunks[..n];

    eprintln!("generating query sets");
    let identity = queryset::document_identity_queries(sliced, &keys, per_source, seed("identity"));
    let headings = queryset::heading_queries(sliced, &keys, per_source * 3, seed("heading"));
    let identifiers =
        queryset::identifier_queries(sliced, &keys, per_source * 3, seed("identifier"));

    // The hard families all need to know how rare a word is in this corpus, which
    // is one pass over the chunks rather than one per family.
    let df = queryset::document_frequencies(sliced);
    let passage =
        queryset::passage_evidence_queries(sliced, &keys, &df, per_source, seed("passage"));
    let typo = queryset::perturbed_queries(&passage, Perturbation::Typo, &df);
    let shorthand = queryset::perturbed_queries(&passage, Perturbation::Shorthand, &df);
    let unanswerable =
        queryset::unanswerable_queries(sliced, &df, per_source * 2, seed("unanswerable"));
    let multi_source =
        queryset::multi_source_queries(sliced, &keys, per_source * 2, seed("multi_source"));
    // Answerable queries the report never scores, used only to choose each
    // engine's abstention threshold. A threshold fitted on the queries it is then
    // judged against would measure the fit rather than the engine.
    let calibration = queryset::heading_queries(sliced, &keys, per_source * 2, seed("calibration"));

    eprintln!(
        "  {} document identity, {} heading, {} identifier, {} passage evidence, {} typo, {} shorthand, {} unanswerable, {} multi-source, {} calibration",
        identity.len(),
        headings.len(),
        identifiers.len(),
        passage.len(),
        typo.len(),
        shorthand.len(),
        unanswerable.len(),
        multi_source.len(),
        calibration.len()
    );

    eprintln!("embedding queries with {} from {model_dir}", model.manifest.id);
    // The cache's own header, checked against the model about to embed the
    // queries. Documents embedded by one model and queries by another is a
    // comparison of two coordinate systems, and it produces a plausible-looking
    // card rather than an error, which is why this is a refusal.
    if corpus.header.has_provenance() {
        anyhow::ensure!(
            corpus.header.model_id == model.manifest.id,
            "the cache was embedded with {} and the queries would be embedded with {}. A query \
             vector only means anything in the space its documents were embedded into",
            corpus.header.model_id,
            model.manifest.id
        );
        anyhow::ensure!(
            corpus.header.manifest_sha256 == model.digest(),
            "{}'s manifest has changed since this cache was embedded ({} then, {} now)",
            model.manifest.id,
            crate::corpus::short(&corpus.header.manifest_sha256),
            crate::corpus::short(&model.digest())
        );
    }
    model.verify_files()?;
    // One session for all nine families. Opening a CUDA session costs tens of
    // seconds and there is no reason to pay for it nine times.
    let embedder = queryset::open_query_embedder(
        model,
        &crate::arm::ArmOptions { device, ..options.arm_options.clone() },
    )?;
    let embed = |qs: &[GradedQuery]| -> Result<Vec<Vec<f32>>> {
        let texts: Vec<String> = qs.iter().map(|q| q.text.clone()).collect();
        queryset::embed_with(&embedder, &texts)
    };
    let identity_vectors = embed(&identity)?;
    let heading_vectors = embed(&headings)?;
    let passage_vectors = embed(&passage)?;
    let typo_vectors = embed(&typo)?;
    let shorthand_vectors = embed(&shorthand)?;
    let unanswerable_vectors = embed(&unanswerable)?;
    let multi_vectors = embed(&multi_source)?;
    let calibration_vectors = embed(&calibration)?;
    let embedded = identity_vectors.len()
        + heading_vectors.len()
        + passage_vectors.len()
        + typo_vectors.len()
        + shorthand_vectors.len()
        + unanswerable_vectors.len()
        + multi_vectors.len()
        + calibration_vectors.len();
    eprintln!("  embedded {embedded} queries");

    let mut query_counts: BTreeMap<String, usize> = BTreeMap::new();
    for (name, n) in [
        ("document identity", identity.len()),
        ("heading", headings.len()),
        ("identifier", identifiers.len()),
        ("passage evidence", passage.len()),
        ("passage evidence, typo", typo.len()),
        ("passage evidence, shorthand", shorthand.len()),
        ("multi-source", multi_source.len()),
        ("unanswerable", unanswerable.len()),
        ("abstention calibration (not scored)", calibration.len()),
    ] {
        query_counts.insert(name.to_string(), n);
    }

    // The run's own directory, opened before the first query so a run that dies
    // half way still leaves the evidence it had gathered.
    let (git_commit, git_dirty) = runs::git_revision(std::path::Path::new("."));
    let run_id = runs::run_id(&git_commit);
    let mut writer = match RunWriter::create(&options.runs_dir, &run_id) {
        Ok(w) => Some(w),
        Err(e) => {
            eprintln!("  could not open the run directory, continuing without per-query records: {e:#}");
            None
        }
    };

    let mut engines: Vec<String> = vec![INILLUCENT.to_string()];
    let mut pg_default = if inillucent_only {
        None
    } else {
        engines.push(PgMode::Default.label().to_string());
        Some(PgVectorEngine::connect(database_url, PgMode::Default)?)
    };
    let mut pg_well_configured = if inillucent_only {
        None
    } else {
        engines.push(PgMode::WellConfigured.label().to_string());
        Some(PgVectorEngine::connect(database_url, PgMode::WellConfigured)?)
    };

    // The baseline is fused the same way, so the hybrid family measures retrieval
    // rather than which engine was handed the better ranking policy.
    let calibration_texts: Vec<String> = calibration.iter().map(|q| q.text.clone()).collect();
    for engine in [pg_default.as_mut(), pg_well_configured.as_mut()].into_iter().flatten() {
        engine.set_fusion(fusion);
        // Theoretical min-max needs a bound on each engine's lexical scores.
        // inillucent has one analytically; `ts_rank_cd` does not, so the baseline's is
        // measured from its own scores on the calibration queries, which the
        // report does not score.
        if matches!(fusion, Fusion::TheoreticalMinMax { .. }) {
            let ceiling = engine.calibrate_lexical_ceiling(&calibration_texts, 0.99)?;
            eprintln!("  {} lexical ceiling calibrated to {ceiling:.4}", engine.name());
        }
    }

    let mut scenarios = Vec::new();

    // ---- Family: approximation accuracy against exhaustive cosine ----
    eprintln!("scenario: vector accuracy against exhaustive cosine");
    scenarios.push(vector_accuracy(
        &mut inillucent,
        &identity,
        &identity_vectors,
    ));
    // An unfiltered query is never routed to an exhaustive scan, so the figure
    // above is genuinely the graph's. The filtered family below reports, per
    // source, which path the cost model chose.

    // ---- Family: how ef_search trades accuracy against latency ----
    eprintln!("scenario: ef_search sweep");
    scenarios.push(ef_sweep(&mut inillucent, &identity_vectors)?);

    // ---- Family: filtered vector accuracy and rows actually returned ----
    eprintln!("scenario: filtered vector search");
    scenarios.push(filtered_vector(
        &mut inillucent,
        pg_default.as_mut(),
        pg_well_configured.as_mut(),
        &identity_vectors,
    )?);

    // ---- Family: filter correctness gate ----
    eprintln!("scenario: filter correctness");
    scenarios.push(filter_correctness(&mut inillucent, &identity_vectors, corpus, n));

    // ---- Family: lexical retrieval ----
    eprintln!("scenario: lexical retrieval");
    scenarios.push(lexical(
        &mut inillucent,
        pg_default.as_mut(),
        &headings,
        &identifiers,
    )?);

    // ---- Family: hybrid retrieval end to end ----
    eprintln!("scenario: hybrid retrieval");
    {
        let per_doc_cap = inillucent.index.config().per_doc_cap;
        let mut engines: Vec<&mut dyn SearchEngine> = vec![&mut inillucent];
        if let Some(e) = pg_default.as_mut() {
            engines.push(e);
        }
        if let Some(e) = pg_well_configured.as_mut() {
            engines.push(e);
        }
        let filter = Filter::default();

        let identity_scores = run_family(
            &mut engines, &identity, &identity_vectors, &filter, per_doc_cap, 10, &mut writer,
        )?;
        let heading_scores = run_family(
            &mut engines, &headings, &heading_vectors, &filter, per_doc_cap, 10, &mut writer,
        )?;
        scenarios.push(hybrid(&identity_scores, &heading_scores));

        // ---- Family: the hard packs ----
        eprintln!("scenario: passage evidence, perturbations and multi-source");
        let passage_scores = run_family(
            &mut engines, &passage, &passage_vectors, &filter, per_doc_cap, 10, &mut writer,
        )?;
        let typo_scores = run_family(
            &mut engines, &typo, &typo_vectors, &filter, per_doc_cap, 10, &mut writer,
        )?;
        let shorthand_scores = run_family(
            &mut engines, &shorthand, &shorthand_vectors, &filter, per_doc_cap, 10, &mut writer,
        )?;
        let multi_scores = run_family(
            &mut engines, &multi_source, &multi_vectors, &filter, per_doc_cap, 10, &mut writer,
        )?;
        scenarios.push(hard_retrieval(
            &passage_scores,
            &typo_scores,
            &shorthand_scores,
            &multi_scores,
        ));

        // ---- Family: abstention on questions nothing answers ----
        eprintln!("scenario: abstention");
        let calibration_scores = run_family(
            &mut engines, &calibration, &calibration_vectors, &filter, per_doc_cap, 10, &mut writer,
        )?;
        let negative_scores = run_family(
            &mut engines,
            &unanswerable,
            &unanswerable_vectors,
            &filter,
            per_doc_cap,
            10,
            &mut writer,
        )?;
        scenarios.push(abstention(&calibration_scores, &negative_scores));
    }

    // ---- Family: fusion comparison ----
    eprintln!("scenario: fusion methods");
    scenarios.push(fusion_methods(&inillucent, &identity, &identity_vectors));

    // ---- Family: quantization and the Matryoshka ladder ----
    eprintln!("scenario: quantization ladder");
    scenarios.push(quantization_ladder(corpus, limit, &identity_vectors)?);

    // ---- Family: latency ----
    eprintln!("scenario: latency");
    scenarios.push(latency(
        &mut inillucent,
        pg_default.as_mut(),
        pg_well_configured.as_mut(),
        &identity,
        &identity_vectors,
    )?);

    // ---- Family: invariants ----
    eprintln!("scenario: invariants");
    scenarios.push(invariants(&mut inillucent, &identity_vectors));

    let build = vec![BuildFacts {
        engine: INILLUCENT.to_string(),
        chunks: stats.chunks,
        documents: stats.documents,
        build_seconds,
        graph_layers: stats.graph_layers,
        graph_edges: stats.graph_edges,
        lexical_terms: stats.lexical_terms,
        lexical_postings: stats.lexical_postings,
        vector_megabytes: stats.vector_bytes as f64 / 1e6,
        quantized_megabytes: stats.quantized_bytes as f64 / 1e6,
    }];

    // Provenance last, so the manifest records the run that actually completed.
    let cache_meta = std::fs::metadata(&options.cache_path).ok();
    let manifest = RunManifest {
        run_id: run_id.clone(),
        generated_at_unix: runs::now_unix(),
        git_commit: git_commit.clone(),
        git_dirty,
        command: runs::command_line(),
        corpus: runs::CorpusFacts {
            chunks: stats.chunks,
            documents: stats.documents,
            dimensions: corpus.dims,
            cache_path: options.cache_path.display().to_string(),
            cache_bytes: cache_meta.as_ref().map(|m| m.len()).unwrap_or(0),
            cache_modified_unix: cache_meta
                .and_then(|m| m.modified().ok())
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs())
                .unwrap_or(0),
        },
        model_dir: model_dir.clone(),
        model_file: model_file.clone(),
        model_id: model.manifest.id.clone(),
        model_manifest_sha256: model.digest(),
        model_dims: model.manifest.dims,
        model_max_tokens: model.manifest.max_tokens,
        cache_header: corpus.header.clone(),
        device: format!("{device:?}"),
        database: runs::redact(database_url),
        seeds: seeds(),
        arm: options.arm(),
        host: runs::host_facts(),
        query_counts: query_counts.clone(),
        practical_thresholds: [
            ("ranking measures".to_string(), 0.01),
            ("latency, relative".to_string(), 0.05),
        ]
        .into_iter()
        .collect(),
    };

    let mut provenance: BTreeMap<String, String> = BTreeMap::new();
    provenance.insert("run id".into(), run_id.clone());
    provenance.insert(
        "commit".into(),
        format!("{}{}", git_commit, if git_dirty { " (working tree dirty)" } else { "" }),
    );
    provenance.insert("command".into(), runs::command_line());
    runs::note_inputs(
        &mut provenance,
        &options.cache_path,
        &model_dir,
        &model_file,
    );
    // Beside the path, what the path was taken to mean. A directory says where
    // the weights were; only this says which prefixes, pooling, width and token
    // bound produced the vectors, and those are what another run has to match.
    provenance.insert(
        "model manifest".into(),
        format!(
            "{} at {} dims, {} tokens, manifest {}{}",
            model.manifest.id,
            model.manifest.dims,
            model.manifest.max_tokens,
            crate::corpus::short(&model.digest()),
            if model.manifest_on_disk { "" } else { " (assumed from the baseline constants)" }
        ),
    );
    provenance.insert("corpus cache header".into(), corpus.header.describe());
    provenance.insert("device".into(), format!("{device:?}"));
    provenance.insert("baseline database".into(), runs::redact(database_url));
    provenance.insert(
        "host".into(),
        format!(
            "{} {}, {} logical processors",
            std::env::consts::OS,
            std::env::consts::ARCH,
            std::thread::available_parallelism().map(|n| n.get()).unwrap_or(0)
        ),
    );
    provenance.insert(
        "query seeds".into(),
        seeds().iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join(", "),
    );
    provenance.insert("statistics seed".into(), options.stats_seed.to_string());
    provenance.insert(
        "ranking settings".into(),
        options.arm().iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join(", "),
    );

    match writer {
        Some(w) => {
            let records = w.written();
            let dir = w.finish(&manifest)?;
            runs::note_run_files(&mut provenance, records, &dir);
        }
        None => {
            provenance.insert("per-query records".into(), "**not written**".into());
        }
    }

    Ok(ScoreCard {
        corpus_chunks: stats.chunks,
        corpus_documents: stats.documents,
        dimensions: corpus.dims,
        model_id: model.manifest.id.clone(),
        engines,
        scenarios,
        build,
        caveats: caveats(),
        generated_at: timestamp(),
        stats_seed: options.stats_seed,
        provenance,
        query_counts,
    })
}

fn timestamp() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!("at unix time {now}")
}

fn caveats() -> Vec<String> {
    vec![
        "Every number is measured on the synthetic corpus this repository builds, with one embedding model. The suite is reusable against another corpus, but these numbers describe this one. The corpus reproduces the size, the per source split, the chunk length distribution and the ingestion ordering of the private corpus this engine was first graded on; retrieval difficulty is not identical, because encyclopedia articles are not the same material as one company's pages.".to_string(),
        "Document identity ground truth uses a document's own title as the query. Titles share vocabulary with their own bodies, which flatters lexical retrieval. It flatters both engines equally, so the comparison holds even though the absolute figure is optimistic.".to_string(),
        "Both engines are graded on the same vectors: the corpus is embedded once and the identical bytes are written to the cache inillucent reads and to the PostgreSQL column pgvector reads. A score difference therefore cannot come from the embedding model. The `embed-check` command verifies that pairing by re-embedding a sample and comparing against what is stored.".to_string(),
        "Latency is measured inside the calling process. inillucent pays no network cost because it is a library; pgvector pays a loopback round trip. That is a real difference in the deployed system rather than a measurement artefact, but it is not a difference in index quality.".to_string(),
    ]
}

/// How well the graph approximates exhaustive cosine, with no predicate. This is
/// the first thing to check, because a handwritten graph fails quietly, and a
/// defect shows up here as a recall figure below what the algorithm should reach.
fn vector_accuracy(
    inillucent: &mut InillucentEngine,
    queries: &[GradedQuery],
    vectors: &[Vec<f32>],
) -> Scenario {
    let mut rows = Vec::new();
    let filter = Filter::default();

    for k in [10usize, 50] {
        let mut acc = Accumulator::default();
        for (q, v) in queries.iter().zip(vectors) {
            let _ = q;
            let reference = exhaustive_reference(inillucent, v, &filter, k);
            let got = keys_of(&inillucent.vector_search(v, &filter, k).unwrap_or_default());
            let mut space = KeySpace::new();
            let ref_ids = space.ids_of(&reference);
            let got_ids = space.ids_of(&got);
            acc.push(recall_at_k(&got_ids, &ref_ids, k));
        }
        rows.push(MetricRow::diagnostic(
            format!("all sources, no predicate ({} queries)", acc.len()),
            &format!("recall@{k}"),
            vec![Measure { engine: INILLUCENT.to_string(), value: acc.mean() as f64 }],
            true,
        ));
    }

    Scenario {
        name: "Approximation accuracy against exhaustive cosine".to_string(),
        rationale: "Exhaustive cosine over the whole corpus defines the exact answer, so this is the measure of how much the graph gives up for its speed. It is reported for inillucent only: the same question cannot be asked of pgvector without reading its index internals, and pgvector's own accuracy against exact search is a property of the same HNSW algorithm at the same parameters. A figure well below what HNSW should reach at m = 16 and ef_construction = 64 would indicate a graph defect rather than a tuning choice.".to_string(),
        rows,
        gate: None,
    }
}

/// How accuracy and latency move with `ef_search`, the one knob a caller has.
///
/// Reported so the default is a choice with evidence behind it rather than a
/// number someone picked. The graph is forced, because routing a query to an
/// exhaustive scan would make the sweep measure nothing.
fn ef_sweep(engine: &mut InillucentEngine, vectors: &[Vec<f32>]) -> Result<Scenario> {
    // Borrow the index that already exists rather than building a second one.
    // A second full build costs another three minutes and, more importantly,
    // another 1.5 GB resident, which is what killed an earlier run on this
    // machine.
    let restore_ef = engine.ef_search;
    engine.index.set_force_graph(true);

    let filter = Filter::default();
    let sample: Vec<&Vec<f32>> = vectors.iter().take(40).collect();

    let mut recall = Vec::new();
    let mut p50 = Vec::new();
    // The reference answer does not depend on ef, so compute it once per query
    // rather than once per query per setting.
    let references: Vec<Vec<String>> = sample
        .iter()
        .map(|v| exhaustive_reference(engine, v, &filter, 10))
        .collect();

    for ef in [64usize, 128, 256, 512] {
        engine.ef_search = Some(ef);
        let mut acc = Accumulator::default();
        let mut samples = Vec::new();
        for (v, reference) in sample.iter().zip(&references) {
            let start = Instant::now();
            let got = keys_of(&engine.vector_search(v, &filter, 10)?);
            samples.push(start.elapsed().as_secs_f64() * 1000.0);
            let mut space = KeySpace::new();
            let ref_ids = space.ids_of(reference);
            let got_ids = space.ids_of(&got);
            acc.push(recall_at_k(&got_ids, &ref_ids, 10));
        }
        samples.sort_by(|a, b| a.partial_cmp(b).unwrap());
        recall.push(Measure { engine: format!("ef_search = {ef}"), value: acc.mean() as f64 });
        p50.push(Measure { engine: format!("ef_search = {ef}"), value: percentile(&samples, 0.5) });
    }

    // Leave the index exactly as it was found, or every later scenario silently
    // measures forced traversal instead of the routing the engine really uses.
    engine.index.set_force_graph(false);
    engine.ef_search = restore_ef;

    Ok(Scenario {
        name: "The ef_search tradeoff".to_string(),
        rationale: "`ef_search` is how wide the traversal keeps its candidate list, and it is the one knob a caller turns. Accuracy is measured against exhaustive cosine over the whole corpus, latency alongside it, so the default inillucent ships is a choice with a table behind it. The columns are settings, not engines.".to_string(),
        rows: vec![
            MetricRow::diagnostic("no predicate".into(), "recall@10", recall, true),
            MetricRow::diagnostic("no predicate".into(), "vector search p50 ms", p50, false),
        ],
        gate: None,
    })
}

/// The defect this project exists to fix, measured on the real corpus.
fn filtered_vector(
    inillucent: &mut InillucentEngine,
    pg_default: Option<&mut PgVectorEngine>,
    pg_well_configured: Option<&mut PgVectorEngine>,
    vectors: &[Vec<f32>],
) -> Result<Scenario> {
    let mut rows = Vec::new();
    let sample: Vec<&Vec<f32>> = vectors.iter().take(25).collect();
    let k = 50usize;
    // Every short result, so the completeness question can be a gate instead of a
    // scored row. Returning fifty irrelevant chunks is not better than returning
    // ten useful ones, so a count must never be a relevance win; coming back with
    // thirty rows when fifty exist is still a defect, and a gate is what that is.
    //
    // The gate is on inillucent. The baseline's short results are the finding this
    // whole family exists to report, not a failure of the card, and a gate that
    // fails because the engine being compared against is incomplete would put "A
    // GATE FAILED" at the top of a card where nothing inillucent did was wrong. They
    // are listed in the detail instead, which is where the evidence belongs.
    let mut short_results: Vec<String> = Vec::new();
    let mut baseline_short: Vec<String> = Vec::new();

    // Keep the two optional engines as a list so each source loop touches them
    // uniformly rather than through duplicated branches.
    let mut pg: Vec<&mut PgVectorEngine> = Vec::new();
    if let Some(e) = pg_default {
        pg.push(e);
    }
    if let Some(e) = pg_well_configured {
        pg.push(e);
    }

    for source in SOURCES {
        let filter = Filter::source(source);
        let compiled = inillucent.index.compile(&filter);
        let path = inillucent.index.path_for(&compiled, inillucent.ef_search);
        let pass = compiled.pass_count();

        // Rows actually returned, which is the number that measured zero.
        let mut inillucent_rows = Accumulator::default();
        let mut inillucent_recall = Accumulator::default();
        let mut recall_series: Vec<Series> = Vec::new();
        let mut inillucent_recall_values: Vec<f64> = Vec::new();
        let mut short = 0usize;
        for v in &sample {
            let hits = inillucent.vector_search(v, &filter, k)?;
            inillucent_rows.push(hits.len() as f32);
            if hits.len() < k.min(pass) {
                short += 1;
            }
            let reference = exhaustive_reference(inillucent, v, &filter, 10);
            let mut space = KeySpace::new();
            let ref_ids = space.ids_of(&reference);
            let got_ids = space.ids_of(&keys_of(&hits));
            let r = recall_at_k(&got_ids, &ref_ids, 10);
            inillucent_recall.push(r);
            inillucent_recall_values.push(r as f64);
        }
        if short > 0 {
            short_results.push(format!(
                "inillucent, source = {source}: {short} of {} queries returned fewer than the {} rows the predicate admits",
                sample.len(),
                k.min(pass)
            ));
        }
        recall_series.push(Series { engine: INILLUCENT.to_string(), values: inillucent_recall_values });

        let mut row_measures = vec![Measure {
            engine: INILLUCENT.to_string(),
            value: inillucent_rows.mean() as f64,
        }];
        let mut recall_measures = vec![Measure {
            engine: INILLUCENT.to_string(),
            value: inillucent_recall.mean() as f64,
        }];

        for engine in pg.iter_mut() {
            let mut returned = Accumulator::default();
            let mut recall = Accumulator::default();
            let mut values: Vec<f64> = Vec::new();
            let mut short = 0usize;
            for v in &sample {
                let hits = engine.vector_search(v, &filter, k)?;
                returned.push(hits.len() as f32);
                if hits.len() < k.min(pass) {
                    short += 1;
                }
                // The reference is exhaustive cosine over the same passing set,
                // computed by inillucent because it is exact.
                let reference = exhaustive_reference(inillucent, v, &filter, 10);
                let mut space = KeySpace::new();
                let ref_ids = space.ids_of(&reference);
                let got_ids = space.ids_of(&keys_of(&hits));
                let r = recall_at_k(&got_ids, &ref_ids, 10);
                recall.push(r);
                values.push(r as f64);
            }
            let name = engine.name().to_string();
            if short > 0 {
                baseline_short.push(format!(
                    "{name} on {source} returned fewer than the {} rows the predicate admits, on {} of {} queries",
                    k.min(pass),
                    short,
                    sample.len()
                ));
            }
            row_measures.push(Measure { engine: name.clone(), value: returned.mean() as f64 });
            recall_measures.push(Measure { engine: name.clone(), value: recall.mean() as f64 });
            recall_series.push(Series { engine: name, values });
        }

        rows.push(MetricRow::diagnostic(
            format!("source = {source} ({pass} chunks, inillucent path: {path})"),
            &format!("rows returned of {k} requested"),
            row_measures,
            true,
        ));
        rows.push(MetricRow::primary(
            format!("source = {source} ({pass} chunks, inillucent path: {path})"),
            "recall@10 within the filter",
            recall_measures,
            true,
            recall_series,
        ));
    }

    let completeness = GateResult {
        passed: short_results.is_empty(),
        detail: if short_results.is_empty() {
            let mut detail =
                "inillucent returned every row the predicate admits, on every source".to_string();
            if !baseline_short.is_empty() {
                detail.push_str(". The baseline did not: ");
                detail.push_str(&baseline_short.join("; "));
            }
            detail
        } else {
            short_results.join("; ")
        },
    };

    Ok(Scenario {
        name: "Filtered vector search, per source".to_string(),
        rationale: "An application with one search tool per source filters on `source` on every call, so this is the common case rather than a corner case. pgvector applies the filter after the index scan and the initial scan yields only `hnsw.ef_search` candidates, so a query restricted to a minority source can come back empty. inillucent expands nodes that fail the predicate but admits only nodes that pass, so the walk continues until it has found enough passing chunks. Recall within the filter is the primary measurement, against exhaustive cosine over the same passing set. Rows returned is a diagnostic and a gate rather than a score: fifty irrelevant chunks are not better than ten useful ones, so a count must never be a relevance win, but coming back with thirty rows when fifty exist is still a defect and that is what the completeness gate is for.".to_string(),
        rows,
        gate: Some(completeness),
    })
}

/// An engine that returns a row it was told to exclude is wrong, not fast.
fn filter_correctness(
    inillucent: &mut InillucentEngine,
    vectors: &[Vec<f32>],
    corpus: &Corpus,
    n: usize,
) -> Scenario {
    let mut violations = 0usize;
    let mut checked = 0usize;
    let mut detail = String::new();

    // A source that exists, a label that exists, a timestamp in the middle of the
    // range, and combinations of them.
    let mut label_pool: Vec<String> = Vec::new();
    for c in corpus.chunks[..n].iter() {
        for l in &c.labels {
            if !label_pool.contains(l) {
                label_pool.push(l.clone());
            }
            if label_pool.len() >= 4 {
                break;
            }
        }
        if label_pool.len() >= 4 {
            break;
        }
    }
    let mut timestamps: Vec<i64> = corpus.chunks[..n].iter().filter_map(|c| c.updated_at).collect();
    timestamps.sort_unstable();
    let midpoint = timestamps.get(timestamps.len() / 2).copied();

    let mut filters: Vec<(String, Filter)> = Vec::new();
    for source in SOURCES {
        filters.push((format!("source = {source}"), Filter::source(source)));
    }
    if let Some(after) = midpoint {
        filters.push((
            "updated_after = corpus midpoint".to_string(),
            Filter { updated_after: Some(after), ..Default::default() },
        ));
        filters.push((
            "source = confluence and updated_after".to_string(),
            Filter {
                source: Some("confluence".into()),
                updated_after: Some(after),
                ..Default::default()
            },
        ));
    }
    if !label_pool.is_empty() {
        filters.push((
            format!("labels overlap {:?}", label_pool),
            Filter { labels: Some(label_pool.clone()), ..Default::default() },
        ));
    }
    filters.push((
        "sources = slack or jira".to_string(),
        Filter { sources: Some(vec!["slack".into(), "jira".into()]), ..Default::default() },
    ));
    filters.push((
        "a source the corpus does not contain".to_string(),
        Filter::source("sharepoint"),
    ));

    for (label, filter) in &filters {
        let compiled = inillucent.index.compile(filter);
        for v in vectors.iter().take(5) {
            let hits = inillucent.vector_search(v, filter, 50).unwrap_or_default();
            // Every returned chunk must pass the compiled predicate, and the
            // count must never exceed what the predicate admits.
            if hits.len() > compiled.pass_count() {
                violations += 1;
                detail = format!("{label}: returned {} rows but only {} pass", hits.len(), compiled.pass_count());
            }
            for h in &hits {
                checked += 1;
                let ordinal = inillucent.ordinal(&h.key);
                match ordinal {
                    Some(o) => {
                        if !compiled.passes(o, inillucent.index.store()) {
                            violations += 1;
                            detail = format!("{label}: chunk {} does not satisfy the predicate", h.key);
                        }
                    }
                    None => {
                        violations += 1;
                        detail = format!("{label}: returned an unknown key {}", h.key);
                    }
                }
            }
        }
        // A filter naming a value the corpus lacks has to return nothing.
        if label.contains("does not contain") {
            let hits = inillucent.vector_search(&vectors[0], filter, 50).unwrap_or_default();
            if !hits.is_empty() {
                violations += 1;
                detail = "a filter naming an absent value returned rows".to_string();
            }
        }
    }

    if violations == 0 {
        detail = format!(
            "{} returned rows across {} filter shapes, every one satisfying its predicate",
            checked,
            filters.len()
        );
    }

    Scenario {
        name: "Filter correctness".to_string(),
        rationale: "Checks that every row an engine returns actually satisfies the predicate it was given, across single source, several sources, timestamp, label overlap, a combination, and a value the corpus does not contain. This gates rather than scores.".to_string(),
        rows: Vec::new(),
        gate: Some(GateResult { passed: violations == 0, detail }),
    }
}

fn lexical(
    inillucent: &mut InillucentEngine,
    pg_default: Option<&mut PgVectorEngine>,
    headings: &[GradedQuery],
    identifiers: &[GradedQuery],
) -> Result<Scenario> {
    let mut rows = Vec::new();
    let filter = Filter::default();

    let mut engines: Vec<&mut dyn SearchEngine> = vec![inillucent];
    if let Some(e) = pg_default {
        engines.push(e);
    }

    for (label, queries) in [
        ("natural language, from headings", headings),
        ("identifiers, rare literal tokens", identifiers),
    ] {
        let mut success = Vec::new();
        let mut mrr = Vec::new();
        let mut returned = Vec::new();
        let mut mrr_series = Vec::new();
        let mut success_series = Vec::new();
        for engine in engines.iter_mut() {
            let mut s = Accumulator::default();
            let mut r = Accumulator::default();
            let mut n = Accumulator::default();
            let mut mrr_values: Vec<f64> = Vec::new();
            let mut success_values: Vec<f64> = Vec::new();
            for q in queries {
                let hits = engine.lexical_search(&q.text, &filter, 50)?;
                n.push(hits.len() as f32);
                let mut space = KeySpace::new();
                let correct = space.set_of(&q.correct);
                let got = space.ids_of(&keys_of(&hits));
                let hit = success_at_k(&got, &correct, 10);
                let rr = reciprocal_rank(&got, &correct);
                s.push(hit);
                r.push(rr);
                success_values.push(hit as f64);
                mrr_values.push(rr as f64);
            }
            let name = engine.name().to_string();
            success.push(Measure { engine: name.clone(), value: s.mean() as f64 });
            mrr.push(Measure { engine: name.clone(), value: r.mean() as f64 });
            returned.push(Measure { engine: name.clone(), value: n.mean() as f64 });
            mrr_series.push(Series { engine: name.clone(), values: mrr_values });
            success_series.push(Series { engine: name, values: success_values });
        }
        // Reciprocal rank is the primary measurement here rather than success@10:
        // it is the same evidence at finer resolution, so it separates two engines
        // on far fewer queries, and where a lexical index is weak it is being weak
        // about position rather than about presence.
        rows.push(MetricRow::primary(
            label.to_string(),
            "mean reciprocal rank",
            mrr,
            true,
            mrr_series,
        ));
        let mut hit_row =
            MetricRow::diagnostic(label.to_string(), "success@10", success, true);
        hit_row.series = success_series;
        rows.push(hit_row);
        rows.push(MetricRow::diagnostic(
            label.to_string(),
            "rows returned of 50",
            returned,
            true,
        ));
    }

    Ok(Scenario {
        name: "Lexical retrieval".to_string(),
        rationale: "PostgreSQL full text search joins query terms with `&` through `to_tsquery`, so a chunk must contain every term, and ranks with `ts_rank_cd`, which has no document length normalization and no term frequency saturation. Joining the terms with `|` instead would return more rows and has not been measured. inillucent scores any term with BM25, which returns partial matches and weights rare terms above common ones. The natural language set is drawn from document headings, which read like questions; the identifier set is rare literal tokens, where a lexical index should be at its strongest and where exact matching matters most.".to_string(),
        rows,
        gate: None,
    })
}

/// Both sides, fused, on the two families whose ground truth is a document.
///
/// The primary measurement is nDCG@10 and everything else in the family is a
/// diagnostic. That is a change of substance rather than of presentation: nDCG,
/// success@1, success@10 and reciprocal rank are four views of one ranking, they
/// move together, and counting each of them separately turned one result into
/// four votes.
///
/// The scope of what these two families prove is also stated more plainly than it
/// was. A title or a heading query is answered by *any* chunk of the document,
/// because the corpus builder writes the title and the heading into the front of
/// every chunk, so what is being graded is finding the right page. That is a real
/// question and it is not the question an agent asks, which is why the passage
/// evidence family below exists.
/// @param identity - scores for the title-as-query family
/// @param headings - scores for the heading-as-query family
fn hybrid(identity: &FamilyScores, headings: &FamilyScores) -> Scenario {
    let mut rows = Vec::new();
    for (label, scores) in [
        ("document identity, title as query", identity),
        ("natural language, heading as query", headings),
    ] {
        rows.push(scores.row(label, M_NDCG, true, Role::Primary));
        for metric in [M_SUCCESS_1, M_SUCCESS_10, M_MRR, M_PRECISION_10] {
            rows.push(scores.row(label, metric, true, Role::Diagnostic));
        }
        rows.push(scores.row(label, M_LATENCY, false, Role::Diagnostic));
    }

    Scenario {
        name: "Hybrid retrieval, whole pipeline".to_string(),
        rationale: "Both sides, fused, capped at two chunks per document, truncated to ten. nDCG@10 is the primary measurement and the rest are diagnostics, because success@1, success@10 and reciprocal rank are the same ranking looked at from three more angles and giving each of them a vote turns one behaviour into four wins. The ground truth here is a whole document: a title or heading query is answered by any chunk of the document that carries it, since the corpus writes both into the front of every chunk. That grades finding the right page, which is worth grading and is not what an agent needs; the passage family below grades finding the right paragraph.".to_string(),
        rows,
        gate: None,
    }
}

/// The families a document-level ground truth cannot express.
///
/// Four packs, each aimed at one thing the original three could not measure:
///
/// - **passage evidence** grades the paragraph rather than the page, with graded
///   judgements, so a chunk of the right document that does not answer scores
///   below the chunk that does instead of scoring identically.
/// - **typo** is the same queries with one adjacent-character transposition. The
///   absolute score matters much less than the gap: it is exactly what the
///   perturbation cost, on ground truth that did not move.
/// - **shorthand** is the same queries reduced to their three rarest content
///   words, which is what people type when they are searching rather than writing.
/// - **multi-source** needs evidence from two documents in two sources, and is
///   scored on whether *both* arrived. Success@10 calls half an answer a success;
///   evidence recall does not.
///
/// @param passage - the passage evidence pack
/// @param typo - the same pack with a transposition
/// @param shorthand - the same pack reduced to keywords
/// @param multi - the two-source pack
fn hard_retrieval(
    passage: &FamilyScores,
    typo: &FamilyScores,
    shorthand: &FamilyScores,
    multi: &FamilyScores,
) -> Scenario {
    let mut rows = Vec::new();

    for (label, scores) in [
        ("passage evidence", passage),
        ("passage evidence, one transposed character", typo),
        ("passage evidence, three keywords", shorthand),
    ] {
        rows.push(scores.row(label, M_NDCG_GRADED, true, Role::Primary));
        for metric in [M_SUCCESS_1, M_SUCCESS_10, M_MRR, M_PRECISION_10] {
            rows.push(scores.row(label, metric, true, Role::Diagnostic));
        }
    }

    // The multi-source pack is decided by whether all the required evidence
    // arrived, not by whether some of it did.
    rows.push(multi.row("multi-source, evidence in two sources", M_EVIDENCE_RECALL, true, Role::Primary));
    for metric in [M_NDCG_GRADED, M_SUCCESS_10, M_MRR] {
        rows.push(multi.row("multi-source, evidence in two sources", metric, true, Role::Diagnostic));
    }

    Scenario {
        name: "Passage evidence, perturbation and multi-source".to_string(),
        rationale: "The families the document-level ground truth cannot express. A passage query is built from one body sentence with the chunk's own breadcrumb words removed, so it cannot be answered by the title text every chunk of that document shares, and with the two rarest remaining words removed, so it is a description of the passage rather than a quotation of it. Judgements are graded: the passage that answers is grade 3, the rest of its document is grade 2 supporting context, and graded nDCG is what separates them. The transposed-character and three-keyword packs are those same queries made harder, on ground truth that did not move, so the gap between the clean score and the perturbed one is exactly what the perturbation cost. The multi-source pack needs evidence from two documents in two sources and is scored on whether both arrived: success@10 counts half an answer as a success, and evidence recall does not.".to_string(),
        rows,
        gate: None,
    }
}

/// What each engine does with a question nothing in the corpus answers.
///
/// This is the failure that does not look like one. Both engines return ten
/// confident-looking passages for a question with no answer, an agent writes a
/// paragraph out of them, and nothing anywhere reports an error. Measuring it
/// needs an absolute notion of confidence, which per-list min-max normalization
/// destroys by construction: it maps the best hit of every list to 1.0, so every
/// query's top result looks equally good and there is no threshold to set.
///
/// The protocol is the same for both engines and does not require their scores to
/// be on the same scale. Each engine's threshold is the 5th percentile of its own
/// top-1 scores over a set of answerable calibration queries the report never
/// scores — the score below which it would start abstaining on questions it can
/// answer. The measurement is then how often it produces a result above its own
/// threshold for a question with no answer.
/// @param calibration - answerable queries, used only to choose the threshold
/// @param negative - the queries nothing answers
fn abstention(calibration: &FamilyScores, negative: &FamilyScores) -> Scenario {
    let mut thresholds = Vec::new();
    let mut false_positive = Vec::new();
    let mut margin = Vec::new();
    let mut fp_series = Vec::new();

    for engine in negative.by_engine.keys() {
        let empty: Vec<f64> = Vec::new();
        // Confidence, not the fused score. The fused score is produced by a
        // normalization that reads its scale out of the candidate list, so the top
        // result of every query looks equally good and there is no threshold to
        // set; confidence is computed on bounds the results had no say in and is
        // therefore comparable between one query and the next.
        let calibration_scores = calibration
            .by_engine
            .get(engine)
            .and_then(|m| m.get(M_TOP_CONFIDENCE))
            .unwrap_or(&empty);
        let negative_scores = negative
            .by_engine
            .get(engine)
            .and_then(|m| m.get(M_TOP_CONFIDENCE))
            .unwrap_or(&empty);

        let mut sorted = calibration_scores.clone();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let threshold = percentile(&sorted, 0.05);

        // One value per query so the comparison can be paired like every other.
        let flags: Vec<f64> = negative_scores
            .iter()
            .map(|s| if *s >= threshold { 1.0 } else { 0.0 })
            .collect();
        let rate = if flags.is_empty() {
            0.0
        } else {
            flags.iter().sum::<f64>() / flags.len() as f64
        };
        let mean = |v: &[f64]| if v.is_empty() { 0.0 } else { v.iter().sum::<f64>() / v.len() as f64 };

        thresholds.push(Measure { engine: engine.clone(), value: threshold });
        false_positive.push(Measure { engine: engine.clone(), value: rate });
        margin.push(Measure {
            engine: engine.clone(),
            value: mean(calibration_scores) - mean(negative_scores),
        });
        fp_series.push(Series { engine: engine.clone(), values: flags });
    }

    let rows = vec![
        MetricRow::primary(
            "questions with no answer in the corpus".to_string(),
            "confident answer rate at the calibrated threshold",
            false_positive,
            false,
            fp_series,
        ),
        MetricRow::diagnostic(
            "calibration queries, 5th percentile of the top result confidence".to_string(),
            "abstention threshold",
            thresholds,
            true,
        ),
        MetricRow::diagnostic(
            "answerable minus unanswerable".to_string(),
            "mean top result confidence gap",
            margin,
            true,
        ),
    ];

    Scenario {
        name: "Abstention on questions nothing answers".to_string(),
        rationale: "Queries built by mixing the distinctive words of two documents from two sources, which the corpus builder draws from disjoint pools, so no chunk holds material from both and the question sounds entirely plausible with no answer. This is the failure mode that does not announce itself: ten confident looking passages about nothing, and an agent writes an answer out of them. Each engine is calibrated on its own scale rather than on a shared one, so the comparison needs no assumption that a inillucent score and a `ts_rank_cd` score mean the same thing: the threshold is the fifth percentile of that engine's own top result score over answerable calibration queries the report does not score, and the measurement is how often it exceeds its own threshold on a question with no answer. Lower is better. The number the threshold is set on is deliberately not the fused score: per-list min-max normalization, which is the fusion that ranks best here, maps the best hit of every list to 1.0 whether the list is good or hopeless, so no threshold on it exists. Every hit therefore also carries a confidence computed the theoretical min-max way — each side divided by a bound the results had no say in — whatever fusion ordered the list. Ranking and confidence are two questions and one number could not answer both.".to_string(),
        rows,
        gate: None,
    }
}

// ---------------------------------------------------------------------------
// Running a graded family against every engine, and keeping the per-query
// evidence rather than only its mean.
// ---------------------------------------------------------------------------

/// Metric names, spelled once so a row label and a per-query record cannot drift
/// apart.
pub const M_NDCG_GRADED: &str = "graded nDCG@10";
pub const M_NDCG: &str = "nDCG@10";
pub const M_SUCCESS_1: &str = "success@1";
pub const M_SUCCESS_10: &str = "success@10";
pub const M_MRR: &str = "mean reciprocal rank";
pub const M_PRECISION_10: &str = "precision@10";
pub const M_EVIDENCE_RECALL: &str = "evidence recall@10";
pub const M_LATENCY: &str = "hybrid search ms";
pub const M_TOP_SCORE: &str = "top result score";
/// The top result's absolute confidence, which is a different question from its
/// fused score and the only one an abstention threshold can be set on.
pub const M_TOP_CONFIDENCE: &str = "top result confidence";

/// One engine's per-query scores for a family, metric by metric.
type PerMetric = BTreeMap<String, Vec<f64>>;

/// What one family's run produced: per engine, per metric, one value per query.
///
/// Kept whole rather than reduced to means, because every honest comparison in
/// this card is paired: it needs each query's score under both engines, not two
/// averages that happen to be over the same query count.
pub struct FamilyScores {
    pub by_engine: BTreeMap<String, PerMetric>,
    #[allow(dead_code)]
    pub queries: usize,
}

impl FamilyScores {
    /// The mean of one metric for one engine, which is the number a row prints.
    fn mean(&self, engine: &str, metric: &str) -> f64 {
        let Some(values) = self.by_engine.get(engine).and_then(|m| m.get(metric)) else {
            return 0.0;
        };
        if values.is_empty() {
            return 0.0;
        }
        values.iter().sum::<f64>() / values.len() as f64
    }

    /// The engines that answered this family, in the order they were given.
    fn engines(&self) -> Vec<String> {
        self.by_engine.keys().cloned().collect()
    }

    /// One row: the mean per engine, and the per-query series behind it.
    /// @param label - what was measured
    /// @param metric - which metric to read
    /// @param higher_is_better - the direction
    /// @param role - whether this row is judged or only reported
    fn row(&self, label: &str, metric: &str, higher_is_better: bool, role: Role) -> MetricRow {
        let measures: Vec<Measure> = self
            .engines()
            .into_iter()
            .map(|e| Measure { value: self.mean(&e, metric), engine: e })
            .collect();
        let series: Vec<Series> = self
            .engines()
            .into_iter()
            .filter_map(|e| {
                self.by_engine
                    .get(&e)
                    .and_then(|m| m.get(metric))
                    .map(|values| Series { engine: e, values: values.clone() })
            })
            .collect();
        match role {
            Role::Primary => {
                MetricRow::primary(label.to_string(), metric, measures, higher_is_better, series)
            }
            Role::Diagnostic => {
                let mut row =
                    MetricRow::diagnostic(label.to_string(), metric, measures, higher_is_better);
                row.series = series;
                row
            }
        }
    }
}

/// Run one graded query family through every engine, scoring each query with every
/// metric and writing the per-query evidence.
///
/// One function rather than one loop per family, because the families now differ
/// only in their queries and their judgements, and having each of them compute its
/// own metrics is how two rows of the same card end up meaning slightly different
/// things.
/// @param engines - the engines under test, all asked the same questions
/// @param queries - the family, with its graded judgements
/// @param vectors - the query embeddings, one per query, shared by every engine
/// @param filter - the predicate every query in this family carries
/// @param per_doc_cap - the cap both engines apply, needed by the nDCG ideal
/// @param k - the cutoff
/// @param writer - where per-query records go, when a run is being recorded
fn run_family(
    engines: &mut [&mut dyn SearchEngine],
    queries: &[GradedQuery],
    vectors: &[Vec<f32>],
    filter: &Filter,
    per_doc_cap: usize,
    k: usize,
    writer: &mut Option<RunWriter>,
) -> Result<FamilyScores> {
    let mut by_engine: BTreeMap<String, PerMetric> = BTreeMap::new();

    for engine in engines.iter_mut() {
        let name = engine.name().to_string();
        let mut per_metric: PerMetric = BTreeMap::new();

        for (q, v) in queries.iter().zip(vectors) {
            let start = Instant::now();
            let hits = engine.hybrid_search(&q.text, v, filter, k)?;
            let latency = start.elapsed().as_secs_f64() * 1000.0;

            // One key space per query, so the ordinals a metric sees are dense and
            // local. The alternative, one space for the whole run, grows to the
            // size of the corpus and buys nothing.
            let mut space = KeySpace::new();
            let correct = space.set_of(&q.correct);
            let grades = q.grades(&mut space);
            let got = space.ids_of(&keys_of(&hits));

            let scores: [(&str, f64); 10] = [
                (M_NDCG_GRADED, ndcg_graded_at_k(&got, &grades, k, per_doc_cap) as f64),
                (M_NDCG, ndcg_at_k_attainable(&got, &correct, k, per_doc_cap) as f64),
                (M_SUCCESS_1, success_at_k(&got, &correct, 1) as f64),
                (M_SUCCESS_10, success_at_k(&got, &correct, k) as f64),
                (M_MRR, reciprocal_rank(&got, &correct) as f64),
                (M_PRECISION_10, precision_at_k(&got, &correct, k) as f64),
                (
                    M_EVIDENCE_RECALL,
                    graded_recall_at_k(&got, &grades, k, queryset::GRADE_ANSWER) as f64,
                ),
                (M_LATENCY, latency),
                (M_TOP_SCORE, hits.first().map(|h| h.score as f64).unwrap_or(0.0)),
                (M_TOP_CONFIDENCE, hits.first().map(|h| h.confidence as f64).unwrap_or(0.0)),
            ];
            for (metric, value) in scores {
                per_metric.entry(metric.to_string()).or_default().push(value);
            }

            if let Some(w) = writer.as_mut() {
                let hit_records: Vec<HitRecord> = hits
                    .iter()
                    .enumerate()
                    .map(|(i, h)| HitRecord {
                        rank: i + 1,
                        key: h.key.clone(),
                        score: h.score as f64,
                        grade: q
                            .graded
                            .iter()
                            .find(|(k, _)| *k == h.key)
                            .map(|(_, g)| *g)
                            .unwrap_or(0),
                    })
                    .collect();
                w.record(&QueryRecord {
                    family: &q.family,
                    query_id: &q.id,
                    query: &q.text,
                    engine: &name,
                    source: &q.source,
                    answerable: q.answerable,
                    latency_ms: latency,
                    returned: hits.len(),
                    hits: hit_records,
                    metrics: scores.iter().map(|(m, v)| (m.to_string(), *v)).collect(),
                })?;
            }
        }
        by_engine.insert(name, per_metric);
    }

    Ok(FamilyScores { by_engine, queries: queries.len() })
}

fn fusion_methods(
    inillucent: &InillucentEngine,
    queries: &[GradedQuery],
    vectors: &[Vec<f32>],
) -> Scenario {
    let filter = Filter::default();
    let mut rows = Vec::new();

    // The methods that are actually a choice for this engine, including the one it now
    // defaults to, so the family says why the default is the default rather than
    // comparing two settings nobody ships.
    let candidates: Vec<(String, Fusion)> = vec![
        ("Reciprocal Rank Fusion, k = 60".to_string(), Fusion::ReciprocalRank { k: 60.0 }),
        ("min-max, vector weight 0.35 (default)".to_string(), Fusion::NormalizedScore { vector_weight: 0.35 }),
        ("min-max, vector weight 0.5".to_string(), Fusion::NormalizedScore { vector_weight: 0.5 }),
        ("min-max, vector weight 0.7".to_string(), Fusion::NormalizedScore { vector_weight: 0.7 }),
        ("convex, vector weight 0.35".to_string(), Fusion::Convex { vector_weight: 0.35 }),
    ];

    let mut ndcg = Vec::new();
    let mut s1 = Vec::new();
    for (name, fusion) in &candidates {
        let mut a_ndcg = Accumulator::default();
        let mut a_s1 = Accumulator::default();
        for (q, v) in queries.iter().zip(vectors) {
            let hits = fuse_with(inillucent, &q.text, v, &filter, 10, *fusion);
            let mut space = KeySpace::new();
            let correct = space.set_of(&q.correct);
            let got = space.ids_of(&keys_of(&hits));
            a_ndcg.push(ndcg_at_k_attainable(
                &got,
                &correct,
                10,
                inillucent.index.config().per_doc_cap,
            ));
            a_s1.push(success_at_k(&got, &correct, 1));
        }
        ndcg.push(Measure { engine: name.clone(), value: a_ndcg.mean() as f64 });
        s1.push(Measure { engine: name.clone(), value: a_s1.mean() as f64 });
    }
    rows.push(MetricRow::diagnostic("document identity".into(), "nDCG@10", ndcg, true));
    rows.push(MetricRow::diagnostic("document identity".into(), "success@1", s1, true));

    Scenario {
        name: "Fusion methods compared".to_string(),
        rationale: "Reciprocal Rank Fusion keeps only position and discards score magnitude, which makes it robust across two scoring scales that are not comparable but blind to how good the top hit actually was. Normalized score fusion keeps the magnitude. This family decides which one inillucent should default to on this corpus, rather than inheriting the choice. The columns here are fusion methods, not engines.".to_string(),
        rows,
        gate: None,
    }
}

/// Chunks the ladder builds on. Eleven index builds at the full corpus would cost
/// most of an hour, and the question the ladder answers, how much accuracy
/// narrowing costs, does not need every chunk. The sample is strided so all six
/// sources are represented, and the size is reported in the score card.
const LADDER_SAMPLE: usize = 25_000;

fn quantization_ladder(
    corpus: &Corpus,
    limit: Option<usize>,
    vectors: &[Vec<f32>],
) -> Result<Scenario> {
    let mut rows = Vec::new();
    let filter = Filter::default();
    let sample: Vec<&Vec<f32>> = vectors.iter().take(20).collect();

    let n = limit.unwrap_or(corpus.len()).min(corpus.len());
    let selected = strided_sample(n, LADDER_SAMPLE.min(n));
    let ladder_chunks = selected.len();

    // The full width, full precision index over the same sample defines the
    // reference ranking, so every rung is measured against the same answer.
    let (reference_index, reference_keys, _stats, _s) =
        build_selected(corpus, selected.clone(), false, corpus.dims)?;
    let reference_engine =
        InillucentEngine::new(reference_index, reference_keys, "reference".to_string(), Some(256));

    let mut recall_measures = Vec::new();
    let mut bytes_measures = Vec::new();

    for &width in MATRYOSHKA_WIDTHS {
        for quantized in [false, true] {
            let label = if quantized {
                format!("{width} dims, int8")
            } else {
                format!("{width} dims, f32")
            };
            let (index, keys, stats, _s) =
                build_selected(corpus, selected.clone(), quantized, width)?;
            let mut engine = InillucentEngine::new(index, keys, label.clone(), Some(128));

            let mut acc = Accumulator::default();
            for v in &sample {
                let narrowed = inillucent_core::distance::truncate_normalized(v, width);
                let reference =
                    exhaustive_reference(&reference_engine, v, &filter, 10);
                let got = keys_of(&engine.vector_search(&narrowed, &filter, 10)?);
                let mut space = KeySpace::new();
                let ref_ids = space.ids_of(&reference);
                let got_ids = space.ids_of(&got);
                acc.push(recall_at_k(&got_ids, &ref_ids, 10));
            }
            let per_vector = if quantized {
                stats.quantized_bytes as f64 / stats.chunks.max(1) as f64
            } else {
                stats.vector_bytes as f64 / stats.chunks.max(1) as f64
            };
            recall_measures.push(Measure { engine: label.clone(), value: acc.mean() as f64 });
            bytes_measures.push(Measure { engine: label, value: per_vector });
            // Release the rung before the next one is built, so the ladder holds
            // one index at a time rather than eleven.
            drop(engine);
        }
    }

    rows.push(MetricRow::diagnostic(
        format!("against the 768 dimension f32 exact ranking, {ladder_chunks} chunks"),
        "recall@10",
        recall_measures,
        true,
    ));
    rows.push(MetricRow::diagnostic(
        "storage".into(),
        "bytes per vector",
        bytes_measures,
        false,
    ));

    Ok(Scenario {
        name: "Quantization and the Matryoshka ladder".to_string(),
        rationale: "Two independent levers for memory. int8 scalar quantization keeps all 768 dimensions and spends one byte each; Matryoshka truncation keeps full precision on fewer dimensions. Qdrant reports int8 costing under 1% accuracy and binary quantization degrading below roughly 1000 dimensions, which is why binary is not offered here at 768. These are the measured numbers on this corpus, not the vendor's. The columns are configurations of inillucent, not engines. This family builds eleven separate indexes, so it runs on an evenly strided sample of the corpus rather than all of it; the sample size is in the row label.".to_string(),
        rows,
        gate: None,
    })
}

fn latency(
    inillucent: &mut InillucentEngine,
    pg_default: Option<&mut PgVectorEngine>,
    pg_well_configured: Option<&mut PgVectorEngine>,
    queries: &[GradedQuery],
    vectors: &[Vec<f32>],
) -> Result<Scenario> {
    let mut rows = Vec::new();

    let mut engines: Vec<&mut dyn SearchEngine> = vec![inillucent];
    if let Some(e) = pg_default {
        engines.push(e);
    }
    if let Some(e) = pg_well_configured {
        engines.push(e);
    }

    for (label, filter) in [
        ("no predicate".to_string(), Filter::default()),
        ("source = slack".to_string(), Filter::source("slack")),
    ] {
        let mut p50 = Vec::new();
        let mut p95 = Vec::new();
        let mut per_query = Vec::new();
        let mut series = Vec::new();
        for engine in engines.iter_mut() {
            // A warmup pass, so the first query's cold caches do not land in the
            // reported percentiles.
            for v in vectors.iter().take(3) {
                let _ = engine.vector_search(v, &filter, 50);
            }
            let mut samples = Vec::new();
            for (q, v) in queries.iter().zip(vectors) {
                let _ = q;
                let start = Instant::now();
                let _ = engine.vector_search(v, &filter, 50)?;
                samples.push(start.elapsed().as_secs_f64() * 1000.0);
            }
            let name = engine.name().to_string();
            let mean = if samples.is_empty() {
                0.0
            } else {
                samples.iter().sum::<f64>() / samples.len() as f64
            };
            // The unsorted samples, in query order, so this row can be compared
            // as a paired series like every other primary row: the same query
            // timed under both engines.
            series.push(Series { engine: name.clone(), values: samples.clone() });
            per_query.push(Measure { engine: name.clone(), value: mean });
            samples.sort_by(|a, b| a.partial_cmp(b).unwrap());
            p50.push(Measure { engine: name.clone(), value: percentile(&samples, 0.5) });
            p95.push(Measure { engine: name, value: percentile(&samples, 0.95) });
        }
        // The median is what this family is judged on, and it is the one row here
        // that carries no per-query series.
        //
        // Every other primary row on the card is decided by a paired test over
        // per-query scores, which is strictly better evidence — except for
        // latency, where it is worse. A mean is not robust: a handful of scheduler
        // stalls from something else on the machine moves it by more than the
        // difference being measured, and a run that happened to catch one reported
        // inillucent's filtered mean at 2.013 ms against a median of 0.838 ms. The
        // paired machinery faithfully reported that as inconclusive, which was the
        // correct answer to the wrong question.
        //
        // So latency is judged on the median against the same relative threshold,
        // and the mean and the 95th percentile are diagnostics beside it. The
        // per-query timings are still written into the run's per-query file and
        // into the card's JSON, so anyone who wants to reanalyse them can.
        rows.push(MetricRow::primary(
            label.clone(),
            "vector search p50 ms",
            p50,
            false,
            Vec::new(),
        ));
        let mut mean_row =
            MetricRow::diagnostic(label.clone(), "vector search mean ms", per_query, false);
        mean_row.series = series;
        rows.push(mean_row);
        rows.push(MetricRow::diagnostic(label, "vector search p95 ms", p95, false));
    }

    Ok(Scenario {
        name: "Latency".to_string(),
        rationale: "Wall clock per query, measured inside the calling process after a warmup pass. The median is the primary measurement and the mean is a diagnostic beside it, which is the one place on this card where a paired test over per-query values is *not* the better evidence: a mean latency is not robust, a few scheduler stalls from something else on the machine move it by more than the difference being measured, and a run that catches one reports a filtered mean of 2.0 ms against a median of 0.8 ms. The 95th percentile is reported because a mean also hides the tail people notice. inillucent pays no network cost because it is a library, while pgvector pays a loopback round trip. That is a genuine difference in the deployed system, but it is not a difference in index quality, so read this alongside the accuracy families rather than instead of them.".to_string(),
        rows,
        gate: None,
    })
}

fn invariants(inillucent: &mut InillucentEngine, vectors: &[Vec<f32>]) -> Scenario {
    let filter = Filter::default();
    let mut failures: Vec<String> = Vec::new();

    // Determinism: the same query twice must give the same answer.
    let a = keys_of(&inillucent.vector_search(&vectors[0], &filter, 20).unwrap_or_default());
    let b = keys_of(&inillucent.vector_search(&vectors[0], &filter, 20).unwrap_or_default());
    if a != b {
        failures.push("vector search is not deterministic".into());
    }
    let ha = keys_of(&inillucent.hybrid_search("offer eligibility", &vectors[0], &filter, 10).unwrap_or_default());
    let hb = keys_of(&inillucent.hybrid_search("offer eligibility", &vectors[0], &filter, 10).unwrap_or_default());
    if ha != hb {
        failures.push("hybrid search is not deterministic".into());
    }

    // The per document cap.
    let hits = inillucent.hybrid_search("offer eligibility rules", &vectors[0], &filter, 20).unwrap_or_default();
    let mut per_doc: std::collections::HashMap<u32, usize> = std::collections::HashMap::new();
    for h in &hits {
        if let Some(o) = inillucent.ordinal(&h.key) {
            let doc = inillucent.index.store().chunks[o as usize].doc;
            *per_doc.entry(doc).or_insert(0) += 1;
        }
    }
    if per_doc.values().any(|c| *c > inillucent.index.config().per_doc_cap) {
        failures.push("the per document cap was exceeded".into());
    }

    // Soft deleted documents must never appear.
    let mut deleted_leaked = 0;
    for h in &hits {
        if let Some(o) = inillucent.ordinal(&h.key) {
            let doc = inillucent.index.store().chunks[o as usize].doc;
            if inillucent.index.store().documents[doc as usize].deleted {
                deleted_leaked += 1;
            }
        }
    }
    if deleted_leaked > 0 {
        failures.push(format!("{deleted_leaked} soft deleted chunks were returned"));
    }

    // Pathological queries must return empty rather than panicking.
    for q in ["", "   ", "the of and a", "!!!???", &"x".repeat(5000)] {
        let _ = inillucent.lexical_search(q, &filter, 10);
    }
    if !inillucent.lexical_search("the of and a", &filter, 10).unwrap_or_default().is_empty() {
        failures.push("a query of only stopwords returned rows".into());
    }

    // k = 0 must return nothing.
    if !inillucent.vector_search(&vectors[0], &filter, 0).unwrap_or_default().is_empty() {
        failures.push("k = 0 returned rows".into());
    }

    let passed = failures.is_empty();
    let detail = if passed {
        "determinism, the per document cap, soft delete exclusion, stopword only and empty and oversized queries, and k = 0 all behave as specified".to_string()
    } else {
        failures.join("; ")
    };

    Scenario {
        name: "Invariants".to_string(),
        rationale: "Properties that have to hold regardless of accuracy: the same query gives the same answer, no document contributes more chunks than the cap allows, soft deleted content never surfaces, and a degenerate query returns nothing rather than failing.".to_string(),
        rows: Vec::new(),
        gate: Some(GateResult { passed, detail }),
    }
}
