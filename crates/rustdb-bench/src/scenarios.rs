//! The graded scenarios.
//!
//! Every family states what it measures, gets the same query set for every
//! engine, and reports the number it measured rather than the number that would
//! be convenient.

use rustdb_core::embed_onnx::Device;
use std::collections::HashSet;
use std::time::Instant;

use anyhow::Result;
use rustdb_core::embed::MATRYOSHKA_WIDTHS;
use rustdb_core::filter::Filter;
use rustdb_core::hnsw::HnswParams;
use rustdb_core::index::{BuildStats, Index, IndexConfig};
use rustdb_core::rank::Fusion;
use rustdb_core::store::ChunkInput;

use crate::corpus::Corpus;
use crate::engine::{
    exhaustive_reference, fuse_with, PgMode, PgVectorEngine, RustDbEngine, SearchEngine,
};
use crate::metrics::{
    ndcg_at_k_attainable, percentile, reciprocal_rank, recall_at_k, success_at_k, Accumulator,
};
use crate::queryset::{self, GradedQuery};
use crate::report::{BuildFacts, GateResult, Measure, MetricRow, Scenario, ScoreCard};

/// Traversal width rust-db uses on a filtered query, matched to the `hnsw.ef_search`
/// the well configured baseline uses on one. It also moves the cost model's crossover:
/// exhaustive search is chosen below `sqrt(ef_search * 32 * chunks)`, which at 400 on
/// this corpus is 48,672 chunks, so the second largest source is scanned exactly
/// instead of walked approximately.
const FILTERED_EF_SEARCH: usize = 400;

const RUSTDB: &str = "rust-db";
const SOURCES: &[&str] = &["confluence", "github", "slack", "jira", "figma", "miro"];

/// Build a rust-db index from the cached corpus. Returns the index, the mapping
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
            labels: c.labels.clone(),
            deleted: c.deleted,
        })
        .collect();

    let vectors: Vec<Vec<f32>> = selected
        .iter()
        .map(|&i| {
            if dims == corpus.dims {
                corpus.vectors[i].clone()
            } else {
                rustdb_core::distance::truncate_normalized(&corpus.vectors[i], dims)
            }
        })
        .collect();

    let keys: Vec<String> = selected.iter().map(|&i| corpus_key(corpus, i)).collect();

    let start = Instant::now();
    index.add(chunks, &vectors);
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

#[allow(clippy::too_many_arguments)]
pub fn grade(
    corpus: &Corpus,
    limit: Option<usize>,
    per_source: usize,
    model_dir: &str,
    model_file: &str,
    database_url: &str,
    rustdb_only: bool,
    device: Device,
    fusion: Fusion,
    lexical_coverage: f32,
    lexical_proximity: f32,
    lexical_prefix: bool,
    lexical_tier: bool,
) -> Result<ScoreCard> {
    eprintln!("building the rust-db index");
    let (index, keys, stats, build_seconds) = build_index(corpus, limit, true)?;
    eprintln!("  built in {build_seconds:.1}s");

    let mut rustdb = RustDbEngine::new(index, keys.clone(), RUSTDB.to_string(), Some(128));
    // The same asymmetry the well configured baseline is given: a wider traversal on
    // a filtered query, because a filtered walk has to step past everything the
    // predicate rejects. pgvector gets hnsw.ef_search 400 filtered against 100
    // unfiltered; without this rust-db was walking a quarter as wide on the one
    // family that is entirely about filtered search.
    rustdb.filtered_ef_search = Some(FILTERED_EF_SEARCH);
    // Both engines get the same ranking policy. Only the lexical coverage is
    // one-sided, and only because PostgreSQL already has what it buys: to_tsquery
    // joins the terms with `&`, so every row it returns contains all of them.
    // rust-db scores any term, which returns far more rows, and the exponent is
    // what stops the extra rows outranking the complete ones.
    rustdb.index.set_lexical_coverage(lexical_coverage);
    rustdb.index.set_lexical_proximity(lexical_proximity);
    rustdb.index.set_lexical_prefix(lexical_prefix);
    rustdb.index.set_lexical_tier(lexical_tier);
    rustdb.index.set_fusion(fusion);

    // Query sets. Generated from the same slice of the corpus the index holds.
    let n = limit.unwrap_or(corpus.len()).min(corpus.len());
    // Query generation reads the chunks; it does not need a second copy of them.
    // Cloning every chunk with its text cost about a gigabyte resident.
    let sliced = &corpus.chunks[..n];

    eprintln!("generating query sets");
    let identity = queryset::document_identity_queries(sliced, &keys, per_source, 11);
    let headings = queryset::heading_queries(sliced, &keys, per_source * 3, 12);
    let identifiers = queryset::identifier_queries(sliced, &keys, per_source * 3, 13);
    eprintln!(
        "  {} document identity, {} heading, {} identifier queries",
        identity.len(),
        headings.len(),
        identifiers.len()
    );

    eprintln!("embedding queries with the in process model from {model_dir}");
    let identity_texts: Vec<String> = identity.iter().map(|q| q.text.clone()).collect();
    let identity_vectors = queryset::embed_queries(model_dir, model_file, &identity_texts, device)?;
    let heading_texts: Vec<String> = headings.iter().map(|q| q.text.clone()).collect();
    let heading_vectors = queryset::embed_queries(model_dir, model_file, &heading_texts, device)?;
    eprintln!("  embedded {} queries", identity_vectors.len() + heading_vectors.len());

    let mut engines: Vec<String> = vec![RUSTDB.to_string()];
    let mut pg_default = if rustdb_only {
        None
    } else {
        engines.push(PgMode::Default.label().to_string());
        Some(PgVectorEngine::connect(database_url, PgMode::Default)?)
    };
    let mut pg_well_configured = if rustdb_only {
        None
    } else {
        engines.push(PgMode::WellConfigured.label().to_string());
        Some(PgVectorEngine::connect(database_url, PgMode::WellConfigured)?)
    };

    // The baseline is fused the same way, so the hybrid family measures retrieval
    // rather than which engine was handed the better ranking policy.
    for engine in [pg_default.as_mut(), pg_well_configured.as_mut()].into_iter().flatten() {
        engine.set_fusion(fusion);
    }

    let mut scenarios = Vec::new();

    // ---- Family: approximation accuracy against exhaustive cosine ----
    eprintln!("scenario: vector accuracy against exhaustive cosine");
    scenarios.push(vector_accuracy(
        &mut rustdb,
        &identity,
        &identity_vectors,
    ));
    // An unfiltered query is never routed to an exhaustive scan, so the figure
    // above is genuinely the graph's. The filtered family below reports, per
    // source, which path the cost model chose.

    // ---- Family: how ef_search trades accuracy against latency ----
    eprintln!("scenario: ef_search sweep");
    scenarios.push(ef_sweep(&mut rustdb, &identity_vectors)?);

    // ---- Family: filtered vector accuracy and rows actually returned ----
    eprintln!("scenario: filtered vector search");
    scenarios.push(filtered_vector(
        &mut rustdb,
        pg_default.as_mut(),
        pg_well_configured.as_mut(),
        &identity_vectors,
    )?);

    // ---- Family: filter correctness gate ----
    eprintln!("scenario: filter correctness");
    scenarios.push(filter_correctness(&mut rustdb, &identity_vectors, corpus, n));

    // ---- Family: lexical retrieval ----
    eprintln!("scenario: lexical retrieval");
    scenarios.push(lexical(
        &mut rustdb,
        pg_default.as_mut(),
        &headings,
        &identifiers,
    )?);

    // ---- Family: hybrid retrieval end to end ----
    eprintln!("scenario: hybrid retrieval");
    scenarios.push(hybrid(
        &mut rustdb,
        pg_default.as_mut(),
        pg_well_configured.as_mut(),
        &identity,
        &identity_vectors,
        &headings,
        &heading_vectors,
    )?);

    // ---- Family: fusion comparison ----
    eprintln!("scenario: fusion methods");
    scenarios.push(fusion_methods(&rustdb, &identity, &identity_vectors));

    // ---- Family: quantization and the Matryoshka ladder ----
    eprintln!("scenario: quantization ladder");
    scenarios.push(quantization_ladder(corpus, limit, &identity_vectors)?);

    // ---- Family: latency ----
    eprintln!("scenario: latency");
    scenarios.push(latency(
        &mut rustdb,
        pg_default.as_mut(),
        pg_well_configured.as_mut(),
        &identity,
        &identity_vectors,
    )?);

    // ---- Family: invariants ----
    eprintln!("scenario: invariants");
    scenarios.push(invariants(&mut rustdb, &identity_vectors));

    let build = vec![BuildFacts {
        engine: RUSTDB.to_string(),
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

    Ok(ScoreCard {
        corpus_chunks: stats.chunks,
        corpus_documents: stats.documents,
        dimensions: corpus.dims,
        engines,
        scenarios,
        build,
        caveats: caveats(),
        generated_at: timestamp(),
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
        "Both engines are graded on the same vectors: the corpus is embedded once and the identical bytes are written to the cache rust-db reads and to the PostgreSQL column pgvector reads. A score difference therefore cannot come from the embedding model. The `embed-check` command verifies that pairing by re-embedding a sample and comparing against what is stored.".to_string(),
        "Latency is measured inside the calling process. rust-db pays no network cost because it is a library; pgvector pays a loopback round trip. That is a real difference in the deployed system rather than a measurement artefact, but it is not a difference in index quality.".to_string(),
    ]
}

/// How well the graph approximates exhaustive cosine, with no predicate. This is
/// the first thing to check, because a handwritten graph fails quietly, and a
/// defect shows up here as a recall figure below what the algorithm should reach.
fn vector_accuracy(
    rustdb: &mut RustDbEngine,
    queries: &[GradedQuery],
    vectors: &[Vec<f32>],
) -> Scenario {
    let mut rows = Vec::new();
    let filter = Filter::default();

    for k in [10usize, 50] {
        let mut acc = Accumulator::default();
        for (q, v) in queries.iter().zip(vectors) {
            let _ = q;
            let reference = exhaustive_reference(rustdb, v, &filter, k);
            let got = keys_of(&rustdb.vector_search(v, &filter, k).unwrap_or_default());
            let mut space = KeySpace::new();
            let ref_ids = space.ids_of(&reference);
            let got_ids = space.ids_of(&got);
            acc.push(recall_at_k(&got_ids, &ref_ids, k));
        }
        rows.push(MetricRow {
            label: format!("all sources, no predicate ({} queries)", acc.len()),
            metric: format!("recall@{k}"),
            measures: vec![Measure { engine: RUSTDB.to_string(), value: acc.mean() as f64 }],
            higher_is_better: true,
        });
    }

    Scenario {
        name: "Approximation accuracy against exhaustive cosine".to_string(),
        rationale: "Exhaustive cosine over the whole corpus defines the exact answer, so this is the measure of how much the graph gives up for its speed. It is reported for rust-db only: the same question cannot be asked of pgvector without reading its index internals, and pgvector's own accuracy against exact search is a property of the same HNSW algorithm at the same parameters. A figure well below what HNSW should reach at m = 16 and ef_construction = 64 would indicate a graph defect rather than a tuning choice.".to_string(),
        rows,
        gate: None,
    }
}

/// How accuracy and latency move with `ef_search`, the one knob a caller has.
///
/// Reported so the default is a choice with evidence behind it rather than a
/// number someone picked. The graph is forced, because routing a query to an
/// exhaustive scan would make the sweep measure nothing.
fn ef_sweep(engine: &mut RustDbEngine, vectors: &[Vec<f32>]) -> Result<Scenario> {
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
        rationale: "`ef_search` is how wide the traversal keeps its candidate list, and it is the one knob a caller turns. Accuracy is measured against exhaustive cosine over the whole corpus, latency alongside it, so the default rust-db ships is a choice with a table behind it. The columns are settings, not engines.".to_string(),
        rows: vec![
            MetricRow { label: "no predicate".into(), metric: "recall@10".into(), measures: recall, higher_is_better: true },
            MetricRow { label: "no predicate".into(), metric: "vector search p50 ms".into(), measures: p50, higher_is_better: false },
        ],
        gate: None,
    })
}

/// The defect this project exists to fix, measured on the real corpus.
fn filtered_vector(
    rustdb: &mut RustDbEngine,
    pg_default: Option<&mut PgVectorEngine>,
    pg_well_configured: Option<&mut PgVectorEngine>,
    vectors: &[Vec<f32>],
) -> Result<Scenario> {
    let mut rows = Vec::new();
    let sample: Vec<&Vec<f32>> = vectors.iter().take(25).collect();
    let k = 50usize;

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
        let compiled = rustdb.index.compile(&filter);
        let path = rustdb.index.path_for(&compiled, rustdb.ef_search);
        let pass = compiled.pass_count();

        // Rows actually returned, which is the number that measured zero.
        let mut rustdb_rows = Accumulator::default();
        let mut rustdb_recall = Accumulator::default();
        for v in &sample {
            let hits = rustdb.vector_search(v, &filter, k)?;
            rustdb_rows.push(hits.len() as f32);
            let reference = exhaustive_reference(rustdb, v, &filter, 10);
            let mut space = KeySpace::new();
            let ref_ids = space.ids_of(&reference);
            let got_ids = space.ids_of(&keys_of(&hits));
            rustdb_recall.push(recall_at_k(&got_ids, &ref_ids, 10));
        }

        let mut row_measures = vec![Measure {
            engine: RUSTDB.to_string(),
            value: rustdb_rows.mean() as f64,
        }];
        let mut recall_measures = vec![Measure {
            engine: RUSTDB.to_string(),
            value: rustdb_recall.mean() as f64,
        }];

        for engine in pg.iter_mut() {
            let mut returned = Accumulator::default();
            let mut recall = Accumulator::default();
            for v in &sample {
                let hits = engine.vector_search(v, &filter, k)?;
                returned.push(hits.len() as f32);
                // The reference is exhaustive cosine over the same passing set,
                // computed by rust-db because it is exact.
                let reference = exhaustive_reference(rustdb, v, &filter, 10);
                let mut space = KeySpace::new();
                let ref_ids = space.ids_of(&reference);
                let got_ids = space.ids_of(&keys_of(&hits));
                recall.push(recall_at_k(&got_ids, &ref_ids, 10));
            }
            let name = engine.name().to_string();
            row_measures.push(Measure { engine: name.clone(), value: returned.mean() as f64 });
            recall_measures.push(Measure { engine: name, value: recall.mean() as f64 });
        }

        rows.push(MetricRow {
            label: format!("source = {source} ({pass} chunks, rust-db path: {path})"),
            metric: format!("rows returned of {k} requested"),
            measures: row_measures,
            higher_is_better: true,
        });
        rows.push(MetricRow {
            label: format!("source = {source} ({pass} chunks, rust-db path: {path})"),
            metric: "recall@10 within the filter".to_string(),
            measures: recall_measures,
            higher_is_better: true,
        });
    }

    Ok(Scenario {
        name: "Filtered vector search, per source".to_string(),
        rationale: "An application with one search tool per source filters on `source` on every call, so this is the common case rather than a corner case. pgvector applies the filter after the index scan and the initial scan yields only `hnsw.ef_search` candidates, so a query restricted to a minority source can come back empty. rust-db expands nodes that fail the predicate but admits only nodes that pass, so the walk continues until it has found enough passing chunks. Two numbers are reported per source: how many rows came back at all, and whether they were the right ones, measured against exhaustive cosine over the same passing set.".to_string(),
        rows,
        gate: None,
    })
}

/// An engine that returns a row it was told to exclude is wrong, not fast.
fn filter_correctness(
    rustdb: &mut RustDbEngine,
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
        let compiled = rustdb.index.compile(filter);
        for v in vectors.iter().take(5) {
            let hits = rustdb.vector_search(v, filter, 50).unwrap_or_default();
            // Every returned chunk must pass the compiled predicate, and the
            // count must never exceed what the predicate admits.
            if hits.len() > compiled.pass_count() {
                violations += 1;
                detail = format!("{label}: returned {} rows but only {} pass", hits.len(), compiled.pass_count());
            }
            for h in &hits {
                checked += 1;
                let ordinal = rustdb.ordinal(&h.key);
                match ordinal {
                    Some(o) => {
                        if !compiled.passes(o, rustdb.index.store()) {
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
            let hits = rustdb.vector_search(&vectors[0], filter, 50).unwrap_or_default();
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
    rustdb: &mut RustDbEngine,
    pg_default: Option<&mut PgVectorEngine>,
    headings: &[GradedQuery],
    identifiers: &[GradedQuery],
) -> Result<Scenario> {
    let mut rows = Vec::new();
    let filter = Filter::default();

    let mut engines: Vec<&mut dyn SearchEngine> = vec![rustdb];
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
        for engine in engines.iter_mut() {
            let mut s = Accumulator::default();
            let mut r = Accumulator::default();
            let mut n = Accumulator::default();
            for q in queries {
                let hits = engine.lexical_search(&q.text, &filter, 50)?;
                n.push(hits.len() as f32);
                let mut space = KeySpace::new();
                let correct = space.set_of(&q.correct);
                let got = space.ids_of(&keys_of(&hits));
                s.push(success_at_k(&got, &correct, 10));
                r.push(reciprocal_rank(&got, &correct));
            }
            let name = engine.name().to_string();
            success.push(Measure { engine: name.clone(), value: s.mean() as f64 });
            mrr.push(Measure { engine: name.clone(), value: r.mean() as f64 });
            returned.push(Measure { engine: name, value: n.mean() as f64 });
        }
        rows.push(MetricRow { label: label.to_string(), metric: "success@10".into(), measures: success, higher_is_better: true });
        rows.push(MetricRow { label: label.to_string(), metric: "mean reciprocal rank".into(), measures: mrr, higher_is_better: true });
        rows.push(MetricRow { label: label.to_string(), metric: "rows returned of 50".into(), measures: returned, higher_is_better: true });
    }

    Ok(Scenario {
        name: "Lexical retrieval".to_string(),
        rationale: "PostgreSQL full text search joins query terms with `&` through `to_tsquery`, so a chunk must contain every term, and ranks with `ts_rank_cd`, which has no document length normalization and no term frequency saturation. Joining the terms with `|` instead would return more rows and has not been measured. rust-db scores any term with BM25, which returns partial matches and weights rare terms above common ones. The natural language set is drawn from document headings, which read like questions; the identifier set is rare literal tokens, where a lexical index should be at its strongest and where exact matching matters most.".to_string(),
        rows,
        gate: None,
    })
}

#[allow(clippy::too_many_arguments)]
fn hybrid(
    rustdb: &mut RustDbEngine,
    pg_default: Option<&mut PgVectorEngine>,
    pg_well_configured: Option<&mut PgVectorEngine>,
    identity: &[GradedQuery],
    identity_vectors: &[Vec<f32>],
    headings: &[GradedQuery],
    heading_vectors: &[Vec<f32>],
) -> Result<Scenario> {
    let mut rows = Vec::new();
    let filter = Filter::default();

    let per_doc_cap = rustdb.index.config().per_doc_cap;
    let mut engines: Vec<&mut dyn SearchEngine> = vec![rustdb];
    if let Some(e) = pg_default {
        engines.push(e);
    }
    if let Some(e) = pg_well_configured {
        engines.push(e);
    }

    for (label, queries, vectors) in [
        ("document identity, title as query", identity, identity_vectors),
        ("natural language, heading as query", headings, heading_vectors),
    ] {
        let mut ndcg = Vec::new();
        let mut s1 = Vec::new();
        let mut s10 = Vec::new();
        let mut mrr = Vec::new();
        for engine in engines.iter_mut() {
            let mut a_ndcg = Accumulator::default();
            let mut a_s1 = Accumulator::default();
            let mut a_s10 = Accumulator::default();
            let mut a_mrr = Accumulator::default();
            for (q, v) in queries.iter().zip(vectors) {
                let hits = engine.hybrid_search(&q.text, v, &filter, 10)?;
                let mut space = KeySpace::new();
                let correct = space.set_of(&q.correct);
                let got = space.ids_of(&keys_of(&hits));
                // The correct set is every chunk of one document, and both
                // engines cap chunks per document, so the ideal ranking is
                // capped the same way.
                a_ndcg.push(ndcg_at_k_attainable(&got, &correct, 10, per_doc_cap));
                a_s1.push(success_at_k(&got, &correct, 1));
                a_s10.push(success_at_k(&got, &correct, 10));
                a_mrr.push(reciprocal_rank(&got, &correct));
            }
            let name = engine.name().to_string();
            ndcg.push(Measure { engine: name.clone(), value: a_ndcg.mean() as f64 });
            s1.push(Measure { engine: name.clone(), value: a_s1.mean() as f64 });
            s10.push(Measure { engine: name.clone(), value: a_s10.mean() as f64 });
            mrr.push(Measure { engine: name, value: a_mrr.mean() as f64 });
        }
        rows.push(MetricRow { label: label.into(), metric: "nDCG@10".into(), measures: ndcg, higher_is_better: true });
        rows.push(MetricRow { label: label.into(), metric: "success@1".into(), measures: s1, higher_is_better: true });
        rows.push(MetricRow { label: label.into(), metric: "success@10".into(), measures: s10, higher_is_better: true });
        rows.push(MetricRow { label: label.into(), metric: "mean reciprocal rank".into(), measures: mrr, higher_is_better: true });
    }

    Ok(Scenario {
        name: "Hybrid retrieval, whole pipeline".to_string(),
        rationale: "Both sides, fused, capped at two chunks per document, truncated to ten. This is the only family that grades fusion, and it is the closest measure of what a person asking a question actually experiences. success@1 is the strictest: it asks whether the right document is the first thing returned.".to_string(),
        rows,
        gate: None,
    })
}

fn fusion_methods(
    rustdb: &RustDbEngine,
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
            let hits = fuse_with(rustdb, &q.text, v, &filter, 10, *fusion);
            let mut space = KeySpace::new();
            let correct = space.set_of(&q.correct);
            let got = space.ids_of(&keys_of(&hits));
            a_ndcg.push(ndcg_at_k_attainable(
                &got,
                &correct,
                10,
                rustdb.index.config().per_doc_cap,
            ));
            a_s1.push(success_at_k(&got, &correct, 1));
        }
        ndcg.push(Measure { engine: name.clone(), value: a_ndcg.mean() as f64 });
        s1.push(Measure { engine: name.clone(), value: a_s1.mean() as f64 });
    }
    rows.push(MetricRow { label: "document identity".into(), metric: "nDCG@10".into(), measures: ndcg, higher_is_better: true });
    rows.push(MetricRow { label: "document identity".into(), metric: "success@1".into(), measures: s1, higher_is_better: true });

    Scenario {
        name: "Fusion methods compared".to_string(),
        rationale: "Reciprocal Rank Fusion keeps only position and discards score magnitude, which makes it robust across two scoring scales that are not comparable but blind to how good the top hit actually was. Normalized score fusion keeps the magnitude. This family decides which one rust-db should default to on this corpus, rather than inheriting the choice. The columns here are fusion methods, not engines.".to_string(),
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
        RustDbEngine::new(reference_index, reference_keys, "reference".to_string(), Some(256));

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
            let mut engine = RustDbEngine::new(index, keys, label.clone(), Some(128));

            let mut acc = Accumulator::default();
            for v in &sample {
                let narrowed = rustdb_core::distance::truncate_normalized(v, width);
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

    rows.push(MetricRow {
        label: format!("against the 768 dimension f32 exact ranking, {ladder_chunks} chunks"),
        metric: "recall@10".into(),
        measures: recall_measures,
        higher_is_better: true,
    });
    rows.push(MetricRow {
        label: "storage".into(),
        metric: "bytes per vector".into(),
        measures: bytes_measures,
        higher_is_better: false,
    });

    Ok(Scenario {
        name: "Quantization and the Matryoshka ladder".to_string(),
        rationale: "Two independent levers for memory. int8 scalar quantization keeps all 768 dimensions and spends one byte each; Matryoshka truncation keeps full precision on fewer dimensions. Qdrant reports int8 costing under 1% accuracy and binary quantization degrading below roughly 1000 dimensions, which is why binary is not offered here at 768. These are the measured numbers on this corpus, not the vendor's. The columns are configurations of rust-db, not engines. This family builds eleven separate indexes, so it runs on an evenly strided sample of the corpus rather than all of it; the sample size is in the row label.".to_string(),
        rows,
        gate: None,
    })
}

fn latency(
    rustdb: &mut RustDbEngine,
    pg_default: Option<&mut PgVectorEngine>,
    pg_well_configured: Option<&mut PgVectorEngine>,
    queries: &[GradedQuery],
    vectors: &[Vec<f32>],
) -> Result<Scenario> {
    let mut rows = Vec::new();

    let mut engines: Vec<&mut dyn SearchEngine> = vec![rustdb];
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
            samples.sort_by(|a, b| a.partial_cmp(b).unwrap());
            let name = engine.name().to_string();
            p50.push(Measure { engine: name.clone(), value: percentile(&samples, 0.5) });
            p95.push(Measure { engine: name, value: percentile(&samples, 0.95) });
        }
        rows.push(MetricRow { label: label.clone(), metric: "vector search p50 ms".into(), measures: p50, higher_is_better: false });
        rows.push(MetricRow { label, metric: "vector search p95 ms".into(), measures: p95, higher_is_better: false });
    }

    Ok(Scenario {
        name: "Latency".to_string(),
        rationale: "Wall clock per query, measured inside the calling process after a warmup pass, reported as median and 95th percentile because a mean hides the tail people notice. rust-db pays no network cost because it is a library, while pgvector pays a loopback round trip. That is a genuine difference in the deployed system, but it is not a difference in index quality, so read this alongside the accuracy families rather than instead of them.".to_string(),
        rows,
        gate: None,
    })
}

fn invariants(rustdb: &mut RustDbEngine, vectors: &[Vec<f32>]) -> Scenario {
    let filter = Filter::default();
    let mut failures: Vec<String> = Vec::new();

    // Determinism: the same query twice must give the same answer.
    let a = keys_of(&rustdb.vector_search(&vectors[0], &filter, 20).unwrap_or_default());
    let b = keys_of(&rustdb.vector_search(&vectors[0], &filter, 20).unwrap_or_default());
    if a != b {
        failures.push("vector search is not deterministic".into());
    }
    let ha = keys_of(&rustdb.hybrid_search("offer eligibility", &vectors[0], &filter, 10).unwrap_or_default());
    let hb = keys_of(&rustdb.hybrid_search("offer eligibility", &vectors[0], &filter, 10).unwrap_or_default());
    if ha != hb {
        failures.push("hybrid search is not deterministic".into());
    }

    // The per document cap.
    let hits = rustdb.hybrid_search("offer eligibility rules", &vectors[0], &filter, 20).unwrap_or_default();
    let mut per_doc: std::collections::HashMap<u32, usize> = std::collections::HashMap::new();
    for h in &hits {
        if let Some(o) = rustdb.ordinal(&h.key) {
            let doc = rustdb.index.store().chunks[o as usize].doc;
            *per_doc.entry(doc).or_insert(0) += 1;
        }
    }
    if per_doc.values().any(|c| *c > rustdb.index.config().per_doc_cap) {
        failures.push("the per document cap was exceeded".into());
    }

    // Soft deleted documents must never appear.
    let mut deleted_leaked = 0;
    for h in &hits {
        if let Some(o) = rustdb.ordinal(&h.key) {
            let doc = rustdb.index.store().chunks[o as usize].doc;
            if rustdb.index.store().documents[doc as usize].deleted {
                deleted_leaked += 1;
            }
        }
    }
    if deleted_leaked > 0 {
        failures.push(format!("{deleted_leaked} soft deleted chunks were returned"));
    }

    // Pathological queries must return empty rather than panicking.
    for q in ["", "   ", "the of and a", "!!!???", &"x".repeat(5000)] {
        let _ = rustdb.lexical_search(q, &filter, 10);
    }
    if !rustdb.lexical_search("the of and a", &filter, 10).unwrap_or_default().is_empty() {
        failures.push("a query of only stopwords returned rows".into());
    }

    // k = 0 must return nothing.
    if !rustdb.vector_search(&vectors[0], &filter, 0).unwrap_or_default().is_empty() {
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
