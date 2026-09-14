//! Comparing embedding models against each other over one corpus.
//!
//! `grade` holds the embedding constant and compares engines. It says so in its
//! own caveats: the corpus is embedded once, the identical vectors go to both
//! engines, and "the embedding model cancels out of the comparison entirely".
//! That is exactly the right design for grading an index and exactly the wrong
//! one for grading an embedder, so this is the other half.
//!
//! Here the *engine* is held constant and the model varies. Each arm is a cache
//! embedded from scratch by one model, and the same questions are asked of every
//! arm, embedded by that arm's own model with that arm's own prefixes.
//!
//! What makes that honest is the refusal, not the arithmetic. Two caches can look
//! identical and describe different corpora, different chunk counts, or a query
//! seed table that changed between the two embedding runs, and a card built from
//! them would be confident and wrong. So the first thing this does is read every
//! cache's header - and nothing else - and refuse, by name, on any disagreement,
//! before a single byte of vector is loaded. Five instruments on this project have
//! quietly measured something other than what they claimed; this one is built so
//! it cannot.
//!
//! Four lanes, because a model can win one and lose another and the card has to
//! show which:
//!
//! - **dense**: exhaustive cosine, no graph and no fusion, over every relevance
//!   family. This is the embedding on its own, with the index's 0.925 recall and
//!   the ranker's policy both removed as variables.
//! - **hybrid**: the real pipeline with the shipped fusion, because that is what
//!   an agent actually gets, and a model that is better in isolation and worse
//!   once BM25 is fused in has not helped anybody.
//! - **cost**: chunks per second on the processor and on `cuda:0`, bytes on disk,
//!   bytes per vector at each Matryoshka width, and the share of the corpus each
//!   model truncated. A model that is better because it read less is not better.
//! - **Matryoshka**: each model's narrowed ranking against its own full-width
//!   exact ranking, so the storage saving is priced per model rather than assumed.

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::{Context, Result};
use rayon::prelude::*;
use serde::Serialize;

use inillucent_core::distance::{dot, truncate_normalized};
use inillucent_core::embed_onnx::Device;
use inillucent_core::filter::Filter;

use crate::corpus::{self, short, CacheHeader, Corpus};
use crate::engine::{Hit, InillucentEngine, SearchEngine};
use crate::metrics::{
    graded_recall_at_k, ndcg_at_k_attainable, ndcg_graded_at_k, precision_at_k, reciprocal_rank,
    success_at_k,
};
use crate::models::{self, ResolvedModel};
use crate::queryset::{self, GradedQuery, Perturbation};
use crate::report::Role;
use crate::runs::{self, HitRecord, QueryRecord, RunWriter};
use crate::scenarios::{self, KeySpace};
use crate::stats::{self, Paired, Verdict};

/// The smallest difference in a ranking measure this card will call a win.
///
/// The same hundredth of a point `grade` uses, and declared in the same place for
/// the same reason: a threshold chosen after seeing the numbers is not a
/// threshold. It is also the number gates G0 and G1 are written
/// against, so moving it here would move a pre-declared gate.
pub const RANKING_THRESHOLD: f64 = 0.01;

/// The two families the score card leads with, and the ones the composite is
/// declared on before any run. If no off-the-shelf model can be told apart from
/// the baseline on these, the instrument cannot see embedding quality, and that
/// is a result about the suite rather than about the models.
const HEADLINE: &[&str] = &["document identity", "heading"];

/// The families the composite is promoted to when the headline composite cannot
/// separate any model from the baseline. Named here rather than chosen later, so
/// the promotion is a pre-declared fallback and not a search for a family that
/// gives the answer somebody wanted.
const PROMOTED: &[&str] = &[
    "document identity",
    "heading",
    "passage evidence",
    "passage evidence, shorthand",
    "passage evidence, typo",
    "multi-source",
];

const K: usize = 10;

/// Traversal width on a filtered query, matched to the number `grade` uses and to
/// the `hnsw.ef_search` the well configured pgvector baseline is given on one.
/// Applied identically to every arm, so it cannot favour a model.
const FILTERED_EF_SEARCH: usize = 400;

pub struct EmbeddingGradeOptions {
    /// Two or more caches, each embedded from the same corpus by one model.
    pub caches: Vec<PathBuf>,
    pub models_root: PathBuf,
    /// The arm every other arm is compared against. Defaults to the first cache.
    pub baseline: Option<String>,
    pub limit: Option<usize>,
    pub per_source: usize,
    /// The processor query embedding runs on. Not the corpus: the corpus is
    /// already embedded, and re-embedding it here would be measuring twice.
    pub device: Device,
    pub runs_dir: PathBuf,
    /// Where the card goes. Carried here rather than only on the command line so
    /// the options value is the whole description of a run.
    #[allow(dead_code)]
    pub out: PathBuf,
    pub stats_seed: u64,
    pub dense: bool,
    pub hybrid: bool,
    pub cost: bool,
    pub matryoshka: bool,
    /// The lane gate G4 is read from: how often each model answers confidently
    /// when nothing in the corpus answers the question.
    pub abstention: bool,
    /// BM25 with no embedding model at all, so the card says what the lexical
    /// half of the shipped pipeline is worth on its own. Scored inside the
    /// hybrid lane, on the index that lane already builds, so it measures
    /// nothing when `hybrid` is off.
    pub lexical: bool,
    /// Chunks re-embedded to time each model. Distinct chunks, spread across the
    /// corpus: timing a model on repeated text measures a cache rather than a
    /// model, which is a mistake this box has already made once.
    pub cost_samples: usize,
    /// Processors the cost lane times each model on.
    pub cost_devices: Vec<Device>,
    /// How many times each model is timed, each pass over a slice of chunks no
    /// earlier pass has shown it.
    ///
    /// One timing is not a measurement. The same model, the same binary and the
    /// same machine produced 3.1, 11.3 and 8.3 chunks a second on CPU in Phase 0,
    /// and a gate read off any one of those would have been read off noise. The
    /// slices are disjoint because repeating the text would let a served arm's
    /// prompt cache answer the second pass, which is how a benchmark on this box
    /// once reported 300 chunks a second against a real 85.
    ///
    /// A machine setting, like `max_batch_cells`: it is in no manifest and
    /// changing it moves no digest.
    pub cost_repeats: usize,
    /// Chunks the Matryoshka lane ranks over. The full corpus would cost an
    /// exhaustive pass per width per model for a number that a stride sample
    /// answers to three decimals.
    pub matryoshka_chunks: usize,
    /// Where and how hard to run each arm, including the llama.cpp endpoint for
    /// a served arm.
    pub arm_options: crate::arm::ArmOptions,
}

// ---------------------------------------------------------------------------
// The card
// ---------------------------------------------------------------------------

/// What one arm is, so a reader never has to trust that two columns are
/// comparable - they can check.
#[derive(Serialize, Clone)]
pub struct ArmFacts {
    pub model_id: String,
    pub dims: usize,
    pub max_tokens: usize,
    pub mrl_widths: Vec<usize>,
    pub pooling: String,
    pub query_prefix: String,
    pub document_prefix: String,
    pub manifest_sha256: String,
    pub weights_sha256: String,
    pub tokenizer_sha256: String,
    pub manifest_on_disk: bool,
    pub cache_path: String,
    pub cache_bytes: u64,
    pub chunks: usize,
    pub truncated_chunks: usize,
    pub truncation_share: f64,
    pub model_bytes: u64,
    pub source: Option<String>,
    /// Filled by the cost lane: device label to the median chunks per second.
    pub chunks_per_second: BTreeMap<String, f64>,
    /// Every timing behind that median, so the card can print how far apart the
    /// repeats were rather than only their middle.
    pub chunks_per_second_runs: BTreeMap<String, Vec<f64>>,
    /// Filled by the cost lane, on distinct sampled chunks.
    pub tokens_per_chunk: Option<f64>,
    pub sample_truncation_share: Option<f64>,
    /// Width to bytes one vector occupies at that width, at fp32.
    pub bytes_per_vector: BTreeMap<String, usize>,
}

#[derive(Serialize, Clone)]
pub struct EmbeddingRow {
    pub family: String,
    pub metric: String,
    pub higher_is_better: bool,
    pub role: Role,
    /// Model id to the mean this row prints.
    pub values: BTreeMap<String, f64>,
    /// Model id to the per-query scores behind that mean. Empty for a row that is
    /// not a per-query measurement, which can be reported but never judged.
    #[serde(skip_serializing_if = "BTreeMap::is_empty", default)]
    pub series: BTreeMap<String, Vec<f64>>,
}

#[derive(Serialize, Clone)]
pub struct Lane {
    pub name: String,
    pub rationale: String,
    pub rows: Vec<EmbeddingRow>,
}

/// One candidate against the baseline on one row.
#[derive(Serialize, Clone)]
pub struct ArmJudgement {
    pub lane: String,
    pub family: String,
    pub metric: String,
    pub model: String,
    pub baseline: String,
    pub candidate_value: f64,
    pub baseline_value: f64,
    pub verdict: Verdict,
    pub threshold: f64,
    pub paired: Option<Paired>,
}

#[derive(Serialize)]
pub struct EmbeddingCard {
    pub run_id: String,
    pub generated_at_unix: u64,
    pub corpus_sha256: String,
    pub corpus_chunks: usize,
    pub corpus_documents: usize,
    pub query_seed_digest: String,
    pub baseline: String,
    pub arms: Vec<ArmFacts>,
    pub lanes: Vec<Lane>,
    pub judgements: Vec<ArmJudgement>,
    pub query_counts: BTreeMap<String, usize>,
    pub stats_seed: u64,
    pub ranking_threshold: f64,
    pub composite_declared: String,
    pub provenance: BTreeMap<String, String>,
    pub caveats: Vec<String>,
}

// ---------------------------------------------------------------------------
// The refusal
// ---------------------------------------------------------------------------

/// One arm, before its vectors are loaded.
#[derive(Debug)]
struct Arm {
    cache: PathBuf,
    header: CacheHeader,
    model: ResolvedModel,
}

/// Read every cache's header, resolve every model, and refuse on any
/// disagreement - before a byte of vector is read.
///
/// The order matters. Loading eight caches costs minutes and six gigabytes; the
/// mistakes this catches are all visible in a few hundred bytes at the front of
/// each file. A harness that discovers halfway through a run that two of its arms
/// describe different corpora has already spent the run.
/// @param options - the caches and where the models live
fn resolve_arms(options: &EmbeddingGradeOptions) -> Result<Vec<Arm>> {
    anyhow::ensure!(
        options.caches.len() >= 2,
        "grade-embedding compares models, so it needs at least two caches; it was given {}",
        options.caches.len()
    );

    let mut arms: Vec<Arm> = Vec::new();
    for path in &options.caches {
        let header = corpus::read_header(path)
            .with_context(|| format!("reading the header of {}", path.display()))?;
        anyhow::ensure!(
            header.has_provenance(),
            "{} is {}. A head-to-head cannot be graded from a cache that cannot say which corpus \
             or which model it came from; re-embed it with synth-embed, which writes a header",
            path.display(),
            header.describe()
        );
        let model = models::resolve_id(&options.models_root, &header.model_id)?;
        model.verify_files()?;

        // The manifest on disk against the manifest the vectors were made with.
        // A prefix or a pooling that changed since the run is invisible in the
        // weights and changes every vector in the file.
        let digest = model.digest();
        anyhow::ensure!(
            digest == header.manifest_sha256,
            "{} was embedded against {}'s manifest {}, and that manifest now digests to {}. \
             Something in the model's contract has changed since these vectors were made, so \
             they no longer describe the model this card would name",
            path.display(),
            header.model_id,
            short(&header.manifest_sha256),
            short(&digest)
        );
        // A model's width is declared by its manifest and realised by its cache.
        // The two disagreeing means one of them is describing another model.
        anyhow::ensure!(
            header.dims == model.manifest.dims,
            "{} holds {} dimensional vectors and {}'s manifest declares {}",
            path.display(),
            header.dims,
            header.model_id,
            model.manifest.dims
        );
        anyhow::ensure!(
            header.max_tokens == model.manifest.max_tokens,
            "{} was embedded with a {} token bound and {}'s manifest declares {}. The share of \
             the corpus each arm truncated is one of the things this card compares",
            path.display(),
            header.max_tokens,
            header.model_id,
            model.manifest.max_tokens
        );
        arms.push(Arm { cache: path.clone(), header, model });
    }

    // Every arm against the first, so an error names one pair rather than a set.
    let first = &arms[0];
    for arm in &arms[1..] {
        anyhow::ensure!(
            arm.header.corpus_sha256 == first.header.corpus_sha256,
            "{} was embedded from corpus {} and {} from corpus {}. These are two different \
             corpora, and a score from one says nothing about a score from the other",
            first.header.model_id,
            short(&first.header.corpus_sha256),
            arm.header.model_id,
            short(&arm.header.corpus_sha256)
        );
        anyhow::ensure!(
            arm.header.chunk_count == first.header.chunk_count,
            "{} holds {} chunks and {} holds {}. One of them was embedded from a corpus that \
             had been rebuilt, and the ground truth is generated from the chunks",
            first.header.model_id,
            first.header.chunk_count,
            arm.header.model_id,
            arm.header.chunk_count
        );
        anyhow::ensure!(
            arm.header.query_seed_digest == first.header.query_seed_digest,
            "{} was embedded when the query seed table digested to {} and {} when it digested \
             to {}. The seeds decide which questions the suite asks, so these two arms would be \
             graded on two different question sets",
            first.header.model_id,
            short(&first.header.query_seed_digest),
            arm.header.model_id,
            short(&arm.header.query_seed_digest)
        );
    }

    // And every arm against the harness that is about to grade it, which is the
    // case a pairwise check between caches cannot see: all of them written before
    // a seed changed, all agreeing, all describing a suite this binary no longer
    // runs.
    let current = corpus::seed_digest(&scenarios::seeds());
    anyhow::ensure!(
        first.header.query_seed_digest == current,
        "these caches were embedded when the query seed table digested to {} and this harness \
         digests to {}. The seeds decide which questions get asked, so grading these caches now \
         would compare vectors made for one suite against the ground truth of another",
        short(&first.header.query_seed_digest),
        short(&current)
    );

    let mut seen: HashSet<&str> = HashSet::new();
    for arm in &arms {
        anyhow::ensure!(
            seen.insert(arm.header.model_id.as_str()),
            "{} appears twice. Two caches of one model is not a comparison",
            arm.header.model_id
        );
    }

    Ok(arms)
}

// ---------------------------------------------------------------------------
// The lanes
// ---------------------------------------------------------------------------

/// One family's per-query scores for one metric.
type PerMetric = BTreeMap<String, Vec<f64>>;

/// Metric names, shared with `grade` where they mean the same thing.
const M_NDCG: &str = "nDCG@10";
const M_NDCG_GRADED: &str = "graded nDCG@10";
const M_SUCCESS_1: &str = "success@1";
const M_SUCCESS_10: &str = "success@10";
const M_MRR: &str = "mean reciprocal rank";
const M_PRECISION_10: &str = "precision@10";
const M_EVIDENCE_RECALL: &str = "evidence recall@10";
const M_TOP_SCORE: &str = "top result score";
const M_RECALL_AGAINST_FULL: &str = "recall@10 against its own full width";

/// Exhaustive cosine over every vector, which is the embedding with nothing else
/// in the way.
///
/// Not the graph, deliberately. The index reaches 0.925 recall against exhaustive
/// cosine on this corpus, and 7.5% of the answer moving for reasons that have
/// nothing to do with the model is more than the whole effect being measured.
/// @param vectors - the corpus, in corpus order
/// @param query - the query vector, already the model's own
/// @param k - how many to return
fn exhaustive_top_k(vectors: &[Vec<f32>], query: &[f32], k: usize) -> Vec<usize> {
    // Per-chunk scoring in parallel, then one selection pass. A parallel
    // selection would need a lock per comparison and buys nothing: the dot
    // products are the work.
    let mut scored: Vec<(f32, usize)> = vectors
        .par_iter()
        .enumerate()
        .map(|(i, v)| (dot(v, query), i))
        .collect();
    let take = k.min(scored.len());
    // Partial selection: the tail's order is never read, so paying to sort it
    // would be paying for a fact nothing reads, once per query per model.
    let pivot = take.saturating_sub(1).min(scored.len().saturating_sub(1));
    scored.select_nth_unstable_by(pivot, |a, b| {
        b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal)
    });
    scored.truncate(take);
    scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
    scored.into_iter().map(|(_, i)| i).collect()
}

/// Score one family for one model, whatever produced the hits.
///
/// One function for both lanes, because the difference between them is which
/// search ran and nothing else. Two functions computing "the same" metrics is how
/// two rows of one card come to mean slightly different things.
/// @param model - the arm's id, which is the column name
/// @param lane - dense or hybrid, recorded on every per-query row
/// @param queries - the family, with its judgements
/// @param search - what a query returns under this arm
/// @param per_doc_cap - the cap the ideal ranking is computed against
/// @param writer - where per-query records go
#[allow(clippy::too_many_arguments)]
fn score_family(
    model: &str,
    lane: &str,
    queries: &[GradedQuery],
    search: &mut dyn FnMut(&GradedQuery, usize) -> Result<Vec<Hit>>,
    per_doc_cap: usize,
    writer: &mut Option<RunWriter>,
) -> Result<PerMetric> {
    let mut per_metric: PerMetric = BTreeMap::new();
    for (index, q) in queries.iter().enumerate() {
        let start = Instant::now();
        let hits = search(q, index)?;
        let latency = start.elapsed().as_secs_f64() * 1000.0;

        let mut space = KeySpace::new();
        let correct = space.set_of(&q.correct);
        let grades = q.grades(&mut space);
        let got = space.ids_of(&hits.iter().map(|h| h.key.clone()).collect::<Vec<_>>());

        let scores: [(&str, f64); 8] = [
            (M_NDCG_GRADED, ndcg_graded_at_k(&got, &grades, K, per_doc_cap) as f64),
            (M_NDCG, ndcg_at_k_attainable(&got, &correct, K, per_doc_cap) as f64),
            (M_SUCCESS_1, success_at_k(&got, &correct, 1) as f64),
            (M_SUCCESS_10, success_at_k(&got, &correct, K) as f64),
            (M_MRR, reciprocal_rank(&got, &correct) as f64),
            (M_PRECISION_10, precision_at_k(&got, &correct, K) as f64),
            (
                M_EVIDENCE_RECALL,
                graded_recall_at_k(&got, &grades, K, queryset::GRADE_ANSWER) as f64,
            ),
            (M_TOP_SCORE, hits.first().map(|h| h.score as f64).unwrap_or(0.0)),
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
                    grade: q.graded.iter().find(|(k, _)| *k == h.key).map(|(_, g)| *g).unwrap_or(0),
                })
                .collect();
            // The engine column carries `<model> / <lane>`. It is what the run
            // writer calls the thing that answered, and here the thing that
            // answered is a model in a lane rather than an engine.
            w.record(&QueryRecord {
                family: &q.family,
                query_id: &q.id,
                query: &q.text,
                engine: &format!("{model} / {lane}"),
                source: &q.source,
                answerable: q.answerable,
                latency_ms: latency,
                returned: hits.len(),
                hits: hit_records,
                metrics: scores.iter().map(|(m, v)| (m.to_string(), *v)).collect(),
            })?;
        }
    }
    Ok(per_metric)
}

/// Every graded family, generated once from the corpus every arm shares.
struct Families {
    named: Vec<(String, Vec<GradedQuery>)>,
    counts: BTreeMap<String, usize>,
    /// The two families the abstention lane uses, kept out of `named` so they
    /// cannot reach the ranking lanes or the composite. Unanswerable queries
    /// carry no positive grades, and an nDCG over an empty reference set is not
    /// a low score - it is a meaningless one.
    abstention: Vec<(String, Vec<GradedQuery>)>,
}

/// The answerable family the threshold is fitted on.
const CALIBRATION: &str = "abstention calibration (not scored)";
/// The family the confident-answer rate is measured on.
const UNANSWERABLE: &str = "unanswerable";

/// Build the query families. Identical for every arm by construction: they are
/// generated from the chunks, and the refusal above has already established that
/// every arm's chunks are the same chunks.
/// @param corpus - any arm's corpus; the text is shared
/// @param keys - the corpus keys, in corpus order
/// @param per_source - queries per source for the identity family
fn build_families(corpus: &Corpus, keys: &[String], per_source: usize, n: usize) -> Families {
    let sliced = &corpus.chunks[..n];
    let identity =
        queryset::document_identity_queries(sliced, keys, per_source, scenarios::seed("identity"));
    let headings =
        queryset::heading_queries(sliced, keys, per_source * 3, scenarios::seed("heading"));
    let df = queryset::document_frequencies(sliced);
    let passage = queryset::passage_evidence_queries(
        sliced,
        keys,
        &df,
        per_source,
        scenarios::seed("passage"),
    );
    let typo = queryset::perturbed_queries(&passage, Perturbation::Typo, &df);
    let shorthand = queryset::perturbed_queries(&passage, Perturbation::Shorthand, &df);
    let multi_source = queryset::multi_source_queries(
        sliced,
        keys,
        per_source * 2,
        scenarios::seed("multi_source"),
    );

    // Built here rather than in the lane so they are generated once from the
    // shared corpus, like every other family, and are byte-identical per arm.
    let unanswerable = queryset::unanswerable_queries(
        sliced,
        &df,
        per_source * 2,
        scenarios::seed("unanswerable"),
    );
    // Answerable questions the lane never scores: they exist only to place each
    // model's own threshold, from a seed deliberately far from the rest.
    let calibration =
        queryset::heading_queries(sliced, keys, per_source * 2, scenarios::seed("calibration"));

    let named = vec![
        ("document identity".to_string(), identity),
        ("heading".to_string(), headings),
        ("passage evidence".to_string(), passage),
        ("passage evidence, typo".to_string(), typo),
        ("passage evidence, shorthand".to_string(), shorthand),
        ("multi-source".to_string(), multi_source),
    ];
    let abstention = vec![
        (CALIBRATION.to_string(), calibration),
        (UNANSWERABLE.to_string(), unanswerable),
    ];
    // The abstention families are counted too, so the card says how many
    // unanswerable questions its G4 rate is over rather than leaving a reader to
    // infer it from a percentage.
    let counts = named
        .iter()
        .chain(abstention.iter())
        .map(|(name, qs)| (name.clone(), qs.len()))
        .collect();
    Families { named, counts, abstention }
}

/// The per-query composite: every family's scores concatenated in a fixed order.
///
/// A mean of family means would weight a 300-query family the same as a 600-query
/// one and, worse, would not be a per-query series, so it could not be tested
/// pairwise. Concatenating keeps one score per question, which is what the paired
/// bootstrap needs.
/// @param per_family - family name to that family's per-query scores
/// @param families - which families the composite is declared over, in order
fn composite(per_family: &BTreeMap<String, Vec<f64>>, families: &[&str]) -> Vec<f64> {
    let mut out = Vec::new();
    for name in families {
        if let Some(values) = per_family.get(*name) {
            out.extend_from_slice(values);
        }
    }
    out
}

// ---------------------------------------------------------------------------
// The run
// ---------------------------------------------------------------------------

pub fn run(options: &EmbeddingGradeOptions) -> Result<EmbeddingCard> {
    let arms = resolve_arms(options)?;
    let baseline_id = match &options.baseline {
        Some(id) => {
            anyhow::ensure!(
                arms.iter().any(|a| &a.header.model_id == id),
                "--baseline names {id}, which is not among the arms: {}",
                arms.iter().map(|a| a.header.model_id.as_str()).collect::<Vec<_>>().join(", ")
            );
            id.clone()
        }
        None => arms[0].header.model_id.clone(),
    };
    eprintln!(
        "{} arms over corpus {} ({} chunks), baseline {baseline_id}",
        arms.len(),
        short(&arms[0].header.corpus_sha256),
        arms[0].header.chunk_count
    );
    for arm in &arms {
        eprintln!("  {}", arm.header.describe());
    }

    let (git_commit, git_dirty) = runs::git_revision(Path::new("."));
    let run_id = runs::run_id(&git_commit);
    let mut writer = match RunWriter::create(&options.runs_dir, &run_id) {
        Ok(w) => Some(w),
        Err(e) => {
            eprintln!("  could not open the run directory, continuing without per-query records: {e:#}");
            None
        }
    };

    // The query set, built once from the corpus every arm shares.
    eprintln!("loading {} to generate the query set", arms[0].cache.display());
    let first = corpus::load_cache(&arms[0].cache)?;
    let n = options.limit.unwrap_or(first.len()).min(first.len());
    let keys: Vec<String> = (0..first.len())
        .map(|i| format!("{}#{}", first.chunks[i].external_doc_id, first.chunks[i].chunk_index))
        .collect();
    let families = build_families(&first, &keys, options.per_source, n);
    let corpus_documents = first
        .chunks
        .iter()
        .take(n)
        .map(|c| c.external_doc_id.as_str())
        .collect::<HashSet<_>>()
        .len();
    eprintln!(
        "  {} queries across {} families",
        families.counts.values().sum::<usize>(),
        families.counts.len()
    );
    // The sample the cost and Matryoshka lanes use, chosen from the shared corpus
    // so every arm is timed and narrowed over the same chunks.
    // Enough for every repeat to get its own slice: a repeat that reused the
    // previous repeat's text would be timing a cache on a served arm.
    let cost_repeats = options.cost_repeats.max(1);
    let cost_texts: Vec<String> =
        scenarios::strided_sample(n, options.cost_samples * cost_repeats)
            .into_iter()
            .map(|i| crate::synth::sanitize_for_model(&first.chunks[i].content))
            .collect();
    let matryoshka_rows = scenarios::strided_sample(n, options.matryoshka_chunks);
    drop(first);

    let mut arm_facts: Vec<ArmFacts> = Vec::new();
    // lane -> family -> metric -> model -> per-query series
    let mut dense: BTreeMap<String, BTreeMap<String, BTreeMap<String, Vec<f64>>>> = BTreeMap::new();
    let mut hybrid: BTreeMap<String, BTreeMap<String, BTreeMap<String, Vec<f64>>>> = BTreeMap::new();
    let mut mrl: BTreeMap<String, BTreeMap<String, f64>> = BTreeMap::new();
    // family -> metric -> per-query scores, for BM25 with no model at all.
    let mut lexical: BTreeMap<String, BTreeMap<String, Vec<f64>>> = BTreeMap::new();
    // model -> family -> the top result's confidence on each of its queries.
    let mut confidences: BTreeMap<String, BTreeMap<String, Vec<f64>>> = BTreeMap::new();

    for arm in &arms {
        let id = arm.header.model_id.clone();
        eprintln!("\n=== {id} ===");
        let started = Instant::now();
        let corpus = corpus::load_cache(&arm.cache)
            .with_context(|| format!("loading {}", arm.cache.display()))?;
        anyhow::ensure!(
            corpus.dims == arm.model.manifest.dims,
            "{} loaded at {} dimensions against a manifest declaring {}",
            id,
            corpus.dims,
            arm.model.manifest.dims
        );
        eprintln!("  loaded {} chunks in {:.1}s", corpus.len(), started.elapsed().as_secs_f64());

        // Queries, embedded by this arm's own model with this arm's own prefixes.
        let embedder = queryset::open_query_embedder(
            &arm.model,
            &crate::arm::ArmOptions { device: options.device, ..options.arm_options.clone() },
        )
        .with_context(|| format!("opening {id} to embed the query set"))?;
        let mut query_vectors: Vec<Vec<Vec<f32>>> = Vec::new();
        for (name, qs) in &families.named {
            let texts: Vec<String> = qs.iter().map(|q| q.text.clone()).collect();
            let vectors = queryset::embed_with(&embedder, &texts)
                .with_context(|| format!("embedding the {name} family with {id}"))?;
            query_vectors.push(vectors);
        }
        eprintln!("  embedded {} queries", query_vectors.iter().map(|v| v.len()).sum::<usize>());

        let mut facts = ArmFacts {
            model_id: id.clone(),
            dims: arm.model.manifest.dims,
            max_tokens: arm.model.manifest.max_tokens,
            mrl_widths: arm.model.manifest.widths(),
            pooling: format!("{:?}", arm.model.manifest.pooling).to_lowercase(),
            query_prefix: arm.model.manifest.prefixes.query.clone(),
            document_prefix: arm.model.manifest.prefixes.document.clone(),
            manifest_sha256: arm.header.manifest_sha256.clone(),
            weights_sha256: arm.model.manifest.weights_sha256.clone(),
            tokenizer_sha256: arm.model.manifest.tokenizer_sha256.clone(),
            manifest_on_disk: arm.model.manifest_on_disk,
            cache_path: arm.cache.display().to_string(),
            cache_bytes: std::fs::metadata(&arm.cache).map(|m| m.len()).unwrap_or(0),
            chunks: arm.header.chunk_count,
            truncated_chunks: arm.header.truncated_chunks,
            truncation_share: if arm.header.chunk_count == 0 {
                0.0
            } else {
                arm.header.truncated_chunks as f64 / arm.header.chunk_count as f64
            },
            model_bytes: weights_bytes(&arm.model.dir, &arm.model.manifest.model_file),
            source: arm.model.manifest.source.clone(),
            chunks_per_second: BTreeMap::new(),
            chunks_per_second_runs: BTreeMap::new(),
            tokens_per_chunk: None,
            sample_truncation_share: None,
            bytes_per_vector: arm
                .model
                .manifest
                .widths()
                .into_iter()
                .map(|w| (w.to_string(), w * 4))
                .collect(),
        };

        // ---- dense lane ----
        if options.dense {
            // The same slice the queries were generated from and the same slice
            // the hybrid lane indexes. Searching the whole cache while grading
            // against ground truth drawn from a prefix of it would let a query
            // be answered by a chunk no judgement covers, and every `--limit` run
            // would quietly score lower for a reason unrelated to any model.
            let vectors = &corpus.vectors[..n];
            eprintln!("  dense lane: exhaustive cosine over {} chunks", vectors.len());
            for ((name, qs), qvs) in families.named.iter().zip(&query_vectors) {
                let started = Instant::now();
                let mut search = |_q: &GradedQuery, index: usize| -> Result<Vec<Hit>> {
                    let ranked = exhaustive_top_k(vectors, &qvs[index], K);
                    Ok(ranked
                        .into_iter()
                        .map(|i| Hit {
                            key: keys[i].clone(),
                            score: dot(&vectors[i], &qvs[index]),
                            confidence: dot(&vectors[i], &qvs[index]).clamp(0.0, 1.0),
                        })
                        .collect())
                };
                // No per-document cap here: the dense lane is the ranking the
                // model produces, with no policy on top. The ideal is computed
                // the same way for every arm, so the absolute value is a little
                // pessimistic and the comparison is exact.
                let scored = score_family(&id, "dense", qs, &mut search, K, &mut writer)?;
                eprintln!(
                    "    {name}: {} queries in {:.1}s, nDCG@10 {:.4}",
                    qs.len(),
                    started.elapsed().as_secs_f64(),
                    mean(scored.get(M_NDCG))
                );
                for (metric, values) in scored {
                    dense
                        .entry(name.clone())
                        .or_default()
                        .entry(metric)
                        .or_default()
                        .insert(id.clone(), values);
                }
            }
        }

        // ---- hybrid lane ----
        if options.hybrid {
            eprintln!("  hybrid lane: building the index");
            let (index, index_keys, stats, seconds) =
                scenarios::build_index(&corpus, options.limit, true)?;
            eprintln!(
                "    built {} chunks / {} documents in {seconds:.1}s",
                stats.chunks, stats.documents
            );
            let per_doc_cap = index.config().per_doc_cap;
            let mut engine = InillucentEngine::new(index, index_keys, id.clone(), Some(128));
            engine.filtered_ef_search = Some(FILTERED_EF_SEARCH);
            // Every ranking setting is left exactly as `build_selected` built it,
            // which is `IndexConfig::default()` - the fusion, the lexical
            // coverage, the phrase weight, the adaptive weighting, all of it.
            //
            // Copying those values into setters here would say the same thing in
            // a second place, and a second place is somewhere the two can drift:
            // the day a default moves, this lane would keep grading against the
            // old policy and the card would still call itself "the shipped
            // pipeline". The one thing set is the traversal width on a filtered
            // query, which is a harness decision rather than an engine default and
            // is the same number `grade` uses.
            let filter = Filter::default();

            // Scored on the first arm only. The lexical ranking reads the
            // corpus text and the query and nothing else, and both are the same
            // for every arm, so the later arms would print the same numbers.
            if options.lexical && lexical.is_empty() {
                for (name, qs) in families.named.iter() {
                    let started = Instant::now();
                    let mut search = |q: &GradedQuery, _index: usize| -> Result<Vec<Hit>> {
                        engine.lexical_search(&q.text, &filter, K)
                    };
                    let scored = score_family(
                        LEXICAL_COLUMN,
                        "lexical",
                        qs,
                        &mut search,
                        per_doc_cap,
                        &mut writer,
                    )?;
                    eprintln!(
                        "    BM25 alone, {name}: {} queries in {:.1}s, nDCG@10 {:.4}",
                        qs.len(),
                        started.elapsed().as_secs_f64(),
                        mean(scored.get(M_NDCG))
                    );
                    for (metric, values) in scored {
                        lexical.entry(name.clone()).or_default().insert(metric, values);
                    }
                }
            }

            for ((name, qs), qvs) in families.named.iter().zip(&query_vectors) {
                let started = Instant::now();
                let mut search = |q: &GradedQuery, index: usize| -> Result<Vec<Hit>> {
                    engine.hybrid_search(&q.text, &qvs[index], &filter, K)
                };
                let scored = score_family(&id, "hybrid", qs, &mut search, per_doc_cap, &mut writer)?;
                eprintln!(
                    "    {name}: {} queries in {:.1}s, nDCG@10 {:.4}",
                    qs.len(),
                    started.elapsed().as_secs_f64(),
                    mean(scored.get(M_NDCG))
                );
                for (metric, values) in scored {
                    hybrid
                        .entry(name.clone())
                        .or_default()
                        .entry(metric)
                        .or_default()
                        .insert(id.clone(), values);
                }
            }
        }

        // ---- Matryoshka lane ----
        if options.matryoshka {
            let widths = arm.model.manifest.widths();
            eprintln!("  Matryoshka lane over {} chunks at {:?}", matryoshka_rows.len(), widths);
            let sample: Vec<&Vec<f32>> = matryoshka_rows.iter().map(|&i| &corpus.vectors[i]).collect();
            // Queries from the identity family, which is the one family whose
            // ground truth is a whole document and therefore the one where a
            // narrowed ranking has the most room to go wrong.
            let probe = &query_vectors[0];
            // The full-width answer, computed once. It is the same reference for
            // every width, and recomputing it inside the width loop was doing a
            // 25,000-vector exhaustive pass three extra times per query for a
            // result that could not change.
            let references: Vec<HashSet<usize>> =
                probe.iter().map(|q| top_k_of(&sample, q, K).into_iter().collect()).collect();
            for width in &widths {
                if *width == corpus.dims {
                    mrl.entry(id.clone()).or_default().insert(width.to_string(), 1.0);
                    continue;
                }
                let narrowed: Vec<Vec<f32>> =
                    sample.iter().map(|v| truncate_normalized(v, *width)).collect();
                let borrowed: Vec<&Vec<f32>> = narrowed.iter().collect();
                let mut recalls = Vec::with_capacity(probe.len());
                for (q, reference) in probe.iter().zip(&references) {
                    let narrow_q = truncate_normalized(q, *width);
                    let got = top_k_of(&borrowed, &narrow_q, K);
                    let hit = got.iter().filter(|i| reference.contains(i)).count();
                    recalls.push(hit as f64 / K.min(reference.len()).max(1) as f64);
                }
                let value = recalls.iter().sum::<f64>() / recalls.len().max(1) as f64;
                eprintln!(
                    "    {width} dims: recall@10 {value:.4} against its own {}",
                    corpus.dims
                );
                mrl.entry(id.clone()).or_default().insert(width.to_string(), value);
            }
        }

        // ---- abstention lane ----
        //
        // Dense cosine rather than the engine's fused score, and deliberately.
        // Fusion normalises out of the candidate list, so the top hit of every
        // query maps to the top of the scale whether the list is good or
        // hopeless and no threshold on it exists. A cosine is computed against a
        // bound the results had no say in, so one query's value means the same
        // as the next one's - which is the whole premise of a threshold.
        if options.abstention {
            for (name, qs) in &families.abstention {
                let texts: Vec<String> = qs.iter().map(|q| q.text.clone()).collect();
                let vectors = queryset::embed_with(&embedder, &texts)
                    .with_context(|| format!("embedding the {name} family with {id}"))?;
                let vectors_slice = &corpus.vectors[..n];
                let tops: Vec<f64> = vectors
                    .iter()
                    .map(|q| {
                        exhaustive_top_k(vectors_slice, q, 1)
                            .first()
                            .map(|i| f64::from(dot(&vectors_slice[*i], q).clamp(0.0, 1.0)))
                            .unwrap_or(0.0)
                    })
                    .collect();
                eprintln!(
                    "  abstention lane, {name}: {} queries, mean top confidence {:.4}",
                    tops.len(),
                    if tops.is_empty() { 0.0 } else { tops.iter().sum::<f64>() / tops.len() as f64 }
                );
                confidences
                    .entry(id.clone())
                    .or_default()
                    .insert(name.clone(), tops);
            }
        }

        // ---- cost lane ----
        if options.cost {
            // A served arm runs wherever its server runs, and `--device` says
            // nothing about that. Timing it once per requested device would put
            // one number in a `cpu` column and the same number in a `cuda:0`
            // column, and a reader would take that to mean this model reaches GPU
            // throughput on a processor. It is timed once, under a label naming
            // the backend and the endpoint, and the card says so.
            let devices: Vec<Device> = match arm.model.manifest.backend {
                inillucent_core::model::Backend::LlamaCpp => vec![options.device],
                inillucent_core::model::Backend::Onnx => options.cost_devices.clone(),
            };
            for device in &devices {
                match time_model(
                    &arm.model,
                    &cost_texts,
                    cost_repeats,
                    *device,
                    &options.arm_options,
                ) {
                    Ok((rates, tokens, share)) => {
                        let label = match arm.model.manifest.backend {
                            inillucent_core::model::Backend::LlamaCpp => {
                                format!("llama.cpp at {}", options.arm_options.endpoint)
                            }
                            inillucent_core::model::Backend::Onnx => device.label(),
                        };
                        let middle = median(&rates);
                        let printed: Vec<String> =
                            rates.iter().map(|r| format!("{r:.1}")).collect();
                        eprintln!(
                            "  cost lane on {label}: {middle:.1} chunks/s median of [{}], \
                             {tokens:.1} tokens per chunk, {:.2}% truncated",
                            printed.join(", "),
                            100.0 * share
                        );
                        facts.chunks_per_second.insert(label.clone(), middle);
                        facts.chunks_per_second_runs.insert(label, rates);
                        facts.tokens_per_chunk = Some(tokens);
                        facts.sample_truncation_share = Some(share);
                    }
                    Err(e) => {
                        eprintln!("  cost lane on {} failed: {e:#}", device.label());
                    }
                }
            }
        }

        arm_facts.push(facts);
    }

    // ---- assemble ----
    let mut lanes = Vec::new();
    if options.dense {
        lanes.push(lane_from(
            "dense only, exhaustive cosine",
            "Every graded family answered by exhaustive cosine over the whole corpus: no graph, \
             no lexical side, no fusion. This is the embedding on its own. The index reaches \
             0.925 recall against exhaustive cosine on this corpus, and letting 7.5% of the \
             answer move for reasons unrelated to the model would be larger than the effect \
             being measured.",
            &dense,
        ));
    }
    if options.hybrid {
        lanes.push(lane_from(
            "hybrid, the shipped pipeline",
            "The same families through the real pipeline with the shipped fusion and ranking \
             settings, applied identically to every arm. A model that wins in isolation and \
             loses once BM25 is fused beside it has not helped an agent, and this is the lane \
             that says so.",
            &hybrid,
        ));
    }
    if options.matryoshka && !mrl.is_empty() {
        lanes.push(matryoshka_lane(&mrl, &arm_facts));
    }
    if options.abstention {
        lanes.push(abstention_lane(&confidences));
    }
    if options.lexical && !lexical.is_empty() {
        lanes.push(lexical_lane(&lexical));
    }
    if options.cost {
        lanes.push(cost_lane(&arm_facts));
    }

    // The composite, on both the declared families and the promoted set, so the
    // promotion is a visible decision rather than a quiet re-aim.
    if options.dense {
        lanes.insert(0, composite_lane(&dense, "dense composite"));
    }
    if options.hybrid {
        lanes.insert(
            if options.dense { 1 } else { 0 },
            composite_lane(&hybrid, "hybrid composite"),
        );
    }

    let judgements = judge(&lanes, &baseline_id, options.stats_seed);

    let mut provenance: BTreeMap<String, String> = BTreeMap::new();
    provenance.insert("run id".into(), run_id.clone());
    provenance.insert(
        "commit".into(),
        format!("{}{}", git_commit, if git_dirty { " (working tree dirty)" } else { "" }),
    );
    provenance.insert("command".into(), runs::command_line());
    provenance.insert("corpus digest".into(), arms[0].header.corpus_sha256.clone());
    provenance.insert("query seed digest".into(), arms[0].header.query_seed_digest.clone());
    provenance.insert(
        "query seeds".into(),
        scenarios::seeds().iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join(", "),
    );
    provenance.insert("statistics seed".into(), options.stats_seed.to_string());
    provenance.insert("query embedding device".into(), format!("{:?}", options.device));
    provenance.insert(
        "host".into(),
        format!(
            "{} {}, {} logical processors",
            std::env::consts::OS,
            std::env::consts::ARCH,
            std::thread::available_parallelism().map(|n| n.get()).unwrap_or(0)
        ),
    );
    for arm in &arms {
        provenance.insert(format!("cache: {}", arm.header.model_id), arm.cache.display().to_string());
    }

    if let Some(w) = writer.take() {
        let records = w.written();
        let manifest = runs::RunManifest {
            run_id: run_id.clone(),
            generated_at_unix: runs::now_unix(),
            git_commit: git_commit.clone(),
            git_dirty,
            command: runs::command_line(),
            corpus: runs::CorpusFacts {
                chunks: arms[0].header.chunk_count,
                documents: corpus_documents,
                dimensions: arms[0].header.dims,
                cache_path: arms[0].cache.display().to_string(),
                cache_bytes: std::fs::metadata(&arms[0].cache).map(|m| m.len()).unwrap_or(0),
                cache_modified_unix: 0,
            },
            model_dir: arms.iter().map(|a| a.model.dir.display().to_string()).collect::<Vec<_>>().join(", "),
            model_file: arms.iter().map(|a| a.model.manifest.model_file.clone()).collect::<Vec<_>>().join(", "),
            model_id: arms.iter().map(|a| a.header.model_id.clone()).collect::<Vec<_>>().join(", "),
            model_manifest_sha256: arms.iter().map(|a| short(&a.header.manifest_sha256)).collect::<Vec<_>>().join(", "),
            model_dims: arms[0].header.dims,
            model_max_tokens: arms[0].header.max_tokens,
            cache_header: arms[0].header.clone(),
            device: format!("{:?}", options.device),
            database: "not used: grade-embedding compares models, not engines".into(),
            seeds: scenarios::seeds(),
            arm: BTreeMap::new(),
            host: runs::host_facts(),
            query_counts: families.counts.clone(),
            practical_thresholds: [("ranking measures".to_string(), RANKING_THRESHOLD)]
                .into_iter()
                .collect(),
        };
        let dir = w.finish(&manifest)?;
        runs::note_run_files(&mut provenance, records, &dir);
    } else {
        provenance.insert("per-query records".into(), "**not written**".into());
    }

    Ok(EmbeddingCard {
        run_id,
        generated_at_unix: runs::now_unix(),
        corpus_sha256: arms[0].header.corpus_sha256.clone(),
        // The slice that was graded, not the size of the cache. With `--limit`
        // set they differ, and a card that prints the cache's count beside a
        // document count taken from the slice is two scopes in one sentence.
        corpus_chunks: n,
        corpus_documents,
        query_seed_digest: arms[0].header.query_seed_digest.clone(),
        baseline: baseline_id,
        arms: arm_facts,
        lanes,
        judgements,
        query_counts: families.counts,
        stats_seed: options.stats_seed,
        ranking_threshold: RANKING_THRESHOLD,
        composite_declared: HEADLINE.join(" + "),
        provenance,
        caveats: caveats(),
    })
}

fn mean(values: Option<&Vec<f64>>) -> f64 {
    match values {
        Some(v) if !v.is_empty() => v.iter().sum::<f64>() / v.len() as f64,
        _ => 0.0,
    }
}

/// What the model weighs on disk, including the weights the graph does not hold.
///
/// An ONNX file over 2 GB cannot carry its own initialisers - protobuf's limit -
/// so exporters write them beside it, by convention as `<model>.onnx_data`. Sizing
/// a model by its graph file alone therefore reports the largest models as the
/// smallest: `qwen3-embedding-0.6b` is a 307 MB graph and a 2,093 MB weight file,
/// and counting only the graph put it *inside* gate G8's footprint budget when it
/// is the heaviest arm on the board. That is a reporting error that changes a
/// conclusion rather than a number.
/// @param dir - the model directory
/// @param model_file - the graph file named by the manifest
fn weights_bytes(dir: &Path, model_file: &str) -> u64 {
    let mut total = std::fs::metadata(dir.join(model_file)).map(|m| m.len()).unwrap_or(0);
    let Ok(entries) = std::fs::read_dir(dir) else { return total };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        // Anything named after the graph but not the graph itself: the external
        // data convention. A graph may in principle name arbitrary files, which
        // would need the proto read to discover; every exporter used here follows
        // the convention.
        if name != model_file && name.starts_with(model_file) {
            total += entry.metadata().map(|m| m.len()).unwrap_or(0);
        }
    }
    total
}

/// Top k by cosine over a borrowed sample, returning positions within it.
fn top_k_of(vectors: &[&Vec<f32>], query: &[f32], k: usize) -> Vec<usize> {
    let mut scored: Vec<(f32, usize)> =
        vectors.iter().enumerate().map(|(i, v)| (dot(v, query), i)).collect();
    scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
    scored.truncate(k.min(vectors.len()));
    scored.into_iter().map(|(_, i)| i).collect()
}

/// The middle value of a set of timings.
///
/// The median rather than the mean: a single stalled pass - a driver waking up,
/// another process taking the card - moves a mean of three and does not move
/// their middle.
/// @param values - the timings, in any order
fn median(values: &[f64]) -> f64 {
    let mut sorted = values.to_vec();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    match sorted.len() {
        0 => 0.0,
        n if n % 2 == 1 => sorted[n / 2],
        n => 0.5 * (sorted[n / 2 - 1] + sorted[n / 2]),
    }
}

/// The half-open range of `texts` a timed pass reads.
///
/// Disjoint by construction: pass `p` reads `[p*slice, (p+1)*slice)` and every
/// pass takes the same count, so no chunk is timed twice and a served arm's prompt
/// cache never gets to answer a pass. The leftover when the sample does not divide
/// evenly is dropped rather than given to the last pass, because a pass over more
/// chunks is not comparable with the passes beside it.
/// @param texts - how many sampled chunks there are
/// @param repeats - how many timed passes to run
/// @param pass - which pass, from zero
fn timing_range(texts: usize, repeats: usize, pass: usize) -> Result<std::ops::Range<usize>> {
    let repeats = repeats.max(1);
    let slice = texts / repeats;
    if slice == 0 {
        anyhow::bail!("{texts} chunks cannot be split into {repeats} disjoint timing slices");
    }
    Ok(pass * slice..(pass + 1) * slice)
}

/// Time one model over distinct chunks, several times over disjoint slices.
///
/// Distinct, and that word is doing work. A benchmark on repeated or
/// shared-prefix text on this box measures a prefix cache: the same measurement
/// reported 300 chunks a second against a real 85-134. These are stride-sampled
/// from across the corpus, so no two share a document, let alone a prefix.
/// The model is opened once and every repeat runs through that one session, so
/// the spread describes the machine rather than the cost of loading a graph.
/// @param model - the arm being timed
/// @param texts - distinct chunk bodies, already sanitized, `repeats` slices worth
/// @param repeats - how many timed passes to run
/// @param device - the processor to time on
fn time_model(
    model: &ResolvedModel,
    texts: &[String],
    repeats: usize,
    device: Device,
    options: &crate::arm::ArmOptions,
) -> Result<(Vec<f64>, f64, f64)> {
    let embedder = crate::arm::Arm::open(
        model,
        &crate::arm::ArmOptions { batch_size: 16, device, ..options.clone() },
    )?;
    let repeats = repeats.max(1);
    // Checked before the warm-up, so a sample too small to split fails without
    // having spent a forward pass on it.
    timing_range(texts.len(), repeats, 0)?;
    // One warm-up batch, outside every timing, so the numbers describe
    // steady-state throughput rather than the first allocation of the arena.
    embedder.embed_documents(&texts[..texts.len().min(16)])?;
    let before = embedder.truncation();

    let mut rates = Vec::with_capacity(repeats);
    for pass in 0..repeats {
        // Disjoint: this pass gets chunks no earlier pass has shown the model.
        let batch = &texts[timing_range(texts.len(), repeats, pass)?];
        let started = Instant::now();
        embedder.embed_documents(batch)?;
        rates.push(batch.len() as f64 / started.elapsed().as_secs_f64().max(1e-9));
    }

    let after = embedder.truncation();
    let counted = after.texts - before.texts;
    let truncated = after.truncated - before.truncated;
    let tokens = after.tokens - before.tokens;
    Ok((
        rates,
        tokens as f64 / counted.max(1) as f64,
        truncated as f64 / counted.max(1) as f64,
    ))
}

// ---------------------------------------------------------------------------
// Rendering and judging
// ---------------------------------------------------------------------------

type LaneScores = BTreeMap<String, BTreeMap<String, BTreeMap<String, Vec<f64>>>>;

/// Turn one lane's accumulated scores into rows.
///
/// nDCG@10 is the primary row for every family and everything else is a
/// diagnostic, for the same reason `grade` makes that distinction: six correlated
/// measures of one behaviour are one result, and counting each of them separately
/// turns a single win into six.
fn lane_from(name: &str, rationale: &str, scores: &LaneScores) -> Lane {
    let mut rows = Vec::new();
    for (family, metrics) in scores {
        for metric in [
            M_NDCG,
            M_NDCG_GRADED,
            M_SUCCESS_1,
            M_SUCCESS_10,
            M_MRR,
            M_PRECISION_10,
            M_EVIDENCE_RECALL,
            M_TOP_SCORE,
        ] {
            let Some(by_model) = metrics.get(metric) else { continue };
            rows.push(EmbeddingRow {
                family: family.clone(),
                metric: metric.to_string(),
                higher_is_better: true,
                role: if metric == M_NDCG { Role::Primary } else { Role::Diagnostic },
                values: by_model
                    .iter()
                    .map(|(m, v)| (m.clone(), v.iter().sum::<f64>() / v.len().max(1) as f64))
                    .collect(),
                series: by_model.clone(),
            });
        }
    }
    Lane { name: name.to_string(), rationale: rationale.to_string(), rows }
}

/// The composite lane: the declared families, and the promoted set beside it.
fn composite_lane(scores: &LaneScores, name: &str) -> Lane {
    let mut rows = Vec::new();
    for (label, set, role) in [
        (format!("composite, declared ({})", HEADLINE.join(" + ")), HEADLINE, Role::Primary),
        (
            format!("composite, promoted ({} families)", PROMOTED.len()),
            PROMOTED,
            Role::Diagnostic,
        ),
    ] {
        // family -> model -> series, transposed to model -> concatenated series.
        let mut per_model: BTreeMap<String, BTreeMap<String, Vec<f64>>> = BTreeMap::new();
        for family in set {
            let Some(metrics) = scores.get(*family) else { continue };
            let Some(by_model) = metrics.get(M_NDCG) else { continue };
            for (model, values) in by_model {
                per_model
                    .entry(model.clone())
                    .or_default()
                    .insert((*family).to_string(), values.clone());
            }
        }
        let series: BTreeMap<String, Vec<f64>> =
            per_model.iter().map(|(m, fams)| (m.clone(), composite(fams, set))).collect();
        // Only models that answered every family in the set, or the concatenated
        // series would be different lengths and no paired test would be valid.
        let expected = series.values().map(|v| v.len()).max().unwrap_or(0);
        let series: BTreeMap<String, Vec<f64>> =
            series.into_iter().filter(|(_, v)| v.len() == expected).collect();
        rows.push(EmbeddingRow {
            family: label,
            metric: M_NDCG.to_string(),
            higher_is_better: true,
            role,
            values: series
                .iter()
                .map(|(m, v)| (m.clone(), v.iter().sum::<f64>() / v.len().max(1) as f64))
                .collect(),
            series,
        });
    }
    Lane {
        name: name.to_string(),
        rationale: format!(
            "One score per question, every family concatenated, so the composite is a paired \
             series rather than a mean of means. The declared composite is {} and is what gate \
             G0 and gate G1 are decided on; the promoted composite is reported beside it so \
             that a promotion, if the declared one turns out to be blind, is a visible decision \
             taken against a number that was already on the card.",
            HEADLINE.join(" and ")
        ),
        rows,
    }
}

fn matryoshka_lane(mrl: &BTreeMap<String, BTreeMap<String, f64>>, arms: &[ArmFacts]) -> Lane {
    let mut widths: Vec<usize> = arms.iter().flat_map(|a| a.mrl_widths.clone()).collect();
    widths.sort_unstable();
    widths.dedup();
    let mut rows = Vec::new();
    for width in widths {
        let key = width.to_string();
        let values: BTreeMap<String, f64> = mrl
            .iter()
            .filter_map(|(model, by_width)| by_width.get(&key).map(|v| (model.clone(), *v)))
            .collect();
        if values.is_empty() {
            continue;
        }
        rows.push(EmbeddingRow {
            family: format!("{width} dims"),
            metric: M_RECALL_AGAINST_FULL.to_string(),
            higher_is_better: true,
            role: Role::Diagnostic,
            values,
            series: BTreeMap::new(),
        });
    }
    Lane {
        name: "Matryoshka".to_string(),
        rationale:
            "Each model's narrowed ranking against its own full-width exact ranking, so the \
             storage saving is priced per model instead of assumed from a model card. A model \
             with no Matryoshka training appears only at its full width, which is the honest \
             way to show that it has none."
                .to_string(),
        rows,
    }
}

/// The value at a percentile of an already sorted series.
///
/// Nearest-rank rather than interpolated, so the threshold is always a score
/// some calibration query actually produced.
/// @param sorted - the series, ascending
/// @param p - the percentile, from zero to one
fn percentile(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let rank = ((sorted.len() - 1) as f64 * p).round() as usize;
    sorted[rank.min(sorted.len() - 1)]
}

/// Gate G4: how often each model answers confidently when nothing answers.
///
/// Each model is judged against its own threshold, because a cosine from one
/// model and a cosine from another are not the same number - a model whose
/// vectors sit in a narrower cone scores every pair higher, and a shared
/// threshold would grade that geometry rather than the behaviour. The threshold
/// is the fifth percentile of the model's own top-result confidence over
/// answerable calibration queries this lane never scores.
/// @param confidences - model to family to per-query top confidence
fn abstention_lane(confidences: &BTreeMap<String, BTreeMap<String, Vec<f64>>>) -> Lane {
    let mut rates = BTreeMap::new();
    let mut series = BTreeMap::new();
    let mut thresholds = BTreeMap::new();
    let mut gaps = BTreeMap::new();

    for (model, by_family) in confidences {
        let (Some(calibration), Some(negative)) =
            (by_family.get(CALIBRATION), by_family.get(UNANSWERABLE))
        else {
            continue;
        };
        if calibration.is_empty() || negative.is_empty() {
            continue;
        }
        let mut sorted = calibration.clone();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let threshold = percentile(&sorted, 0.05);
        // One value per query, so this row can be tested pairwise like every
        // other primary row rather than compared as two summary numbers.
        let flags: Vec<f64> =
            negative.iter().map(|s| if *s >= threshold { 1.0 } else { 0.0 }).collect();
        let mean = |v: &[f64]| v.iter().sum::<f64>() / v.len().max(1) as f64;
        rates.insert(model.clone(), mean(&flags));
        series.insert(model.clone(), flags);
        thresholds.insert(model.clone(), threshold);
        gaps.insert(model.clone(), mean(calibration) - mean(negative));
    }

    let rows = vec![
        EmbeddingRow {
            family: "questions with no answer in the corpus".to_string(),
            metric: "confident answer rate at the model's own threshold".to_string(),
            higher_is_better: false,
            role: Role::Primary,
            values: rates,
            series,
        },
        EmbeddingRow {
            family: "calibration queries, 5th percentile of the top result".to_string(),
            metric: "the model's own abstention threshold".to_string(),
            higher_is_better: true,
            role: Role::Diagnostic,
            values: thresholds,
            series: BTreeMap::new(),
        },
        EmbeddingRow {
            family: "answerable minus unanswerable".to_string(),
            metric: "mean top result confidence gap".to_string(),
            higher_is_better: true,
            role: Role::Diagnostic,
            values: gaps,
            series: BTreeMap::new(),
        },
    ];

    Lane {
        name: "abstention".to_string(),
        rationale:
            "Gate G4. Queries built by mixing the distinctive words of two documents \
             from two sources the corpus builder draws from disjoint pools, so no chunk \
             holds material from both and the question sounds entirely plausible with no \
             answer. This is the failure that does not announce itself: ten confident \
             looking passages about nothing. Each model is calibrated on its own scale - \
             the threshold is the fifth percentile of its own top result confidence over \
             answerable queries this lane never scores - so the comparison needs no \
             assumption that two models' cosines mean the same thing. Lower is better."
                .to_string(),
        rows,
    }
}

/// The column name the lexical lane prints under. Not a model id, because
/// there is no model: the ranking is BM25 over the corpus text.
const LEXICAL_COLUMN: &str = "BM25 alone, no model";

/// The same query families through BM25 with no embedding model.
///
/// One column, because the ranking depends only on the corpus text and the
/// query. Every row is diagnostic: there is no baseline model in this lane to
/// compare against, so nothing here is judged. What it answers is a question
/// the other lanes cannot: how much of what the shipped pipeline finds would
/// be found with no embedding model at all.
/// @param lexical - family to metric to the per-query scores BM25 produced
fn lexical_lane(lexical: &BTreeMap<String, BTreeMap<String, Vec<f64>>>) -> Lane {
    let mut rows = Vec::new();
    let row = |family: String, values: &[f64]| EmbeddingRow {
        family,
        metric: M_NDCG.to_string(),
        higher_is_better: true,
        role: Role::Diagnostic,
        values: [(
            LEXICAL_COLUMN.to_string(),
            values.iter().sum::<f64>() / values.len().max(1) as f64,
        )]
        .into_iter()
        .collect(),
        series: BTreeMap::new(),
    };
    let per_family: BTreeMap<String, Vec<f64>> = lexical
        .iter()
        .filter_map(|(f, m)| m.get(M_NDCG).map(|v| (f.clone(), v.clone())))
        .collect();
    // Both composites the other lanes print, so a reader comparing this lane
    // against the hybrid lane is comparing the same quantity.
    for (label, families) in [
        (format!("composite, declared ({})", HEADLINE.join(" + ")), HEADLINE),
        (format!("composite, promoted ({} families)", PROMOTED.len()), PROMOTED),
    ] {
        let values = composite(&per_family, families);
        if !values.is_empty() {
            rows.push(row(label, &values));
        }
    }
    for (family, values) in &per_family {
        rows.push(row(family.clone(), values));
    }
    Lane {
        name: "lexical only".to_string(),
        rationale:
            "The same families through BM25 with no embedding model, over the index the \
             hybrid lane builds and with every ranking setting left as the shipped \
             defaults. One column, because the ranking reads the corpus text and the \
             query and nothing else, so each arm would produce the same numbers. Every \
             row is diagnostic: there is no model in this lane, so there is no baseline \
             to judge against. Read it against the hybrid lane. The difference between \
             the two is what the embedding model adds to the pipeline Inillucent ships, \
             and it is the only place on this card that quantity appears."
                .to_string(),
        rows,
    }
}

fn cost_lane(arms: &[ArmFacts]) -> Lane {
    let mut rows = Vec::new();
    let mut devices: Vec<String> =
        arms.iter().flat_map(|a| a.chunks_per_second.keys().cloned()).collect();
    devices.sort();
    devices.dedup();
    for device in devices {
        rows.push(EmbeddingRow {
            family: format!("throughput on {device}"),
            metric: "chunks per second, median".to_string(),
            higher_is_better: true,
            role: Role::Diagnostic,
            values: arms
                .iter()
                .filter_map(|a| a.chunks_per_second.get(&device).map(|v| (a.model_id.clone(), *v)))
                .collect(),
            series: BTreeMap::new(),
        });
        // The spread beside the median, because a reader cannot tell whether a
        // gap between two arms is real without knowing how far apart two runs of
        // one arm land. Printed as a share of the median so arms of very
        // different speeds can be compared on it.
        let spreads: BTreeMap<String, f64> = arms
            .iter()
            .filter_map(|a| {
                let runs = a.chunks_per_second_runs.get(&device)?;
                if runs.len() < 2 {
                    return None;
                }
                let low = runs.iter().cloned().fold(f64::INFINITY, f64::min);
                let high = runs.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
                let middle = median(runs);
                Some((a.model_id.clone(), 100.0 * (high - low) / middle.max(1e-9)))
            })
            .collect();
        if !spreads.is_empty() {
            rows.push(EmbeddingRow {
                family: format!("throughput on {device}"),
                metric: "spread across repeats, % of median".to_string(),
                higher_is_better: false,
                role: Role::Diagnostic,
                values: spreads,
                series: BTreeMap::new(),
            });
        }
    }
    for (label, metric, higher, pick) in [
        (
            "weights on disk",
            "megabytes",
            false,
            Box::new(|a: &ArmFacts| a.model_bytes as f64 / 1e6) as Box<dyn Fn(&ArmFacts) -> f64>,
        ),
        (
            "tokens per chunk",
            "tokens",
            false,
            Box::new(|a: &ArmFacts| a.tokens_per_chunk.unwrap_or(0.0)),
        ),
        (
            "corpus truncated at the model's bound",
            "share of chunks",
            false,
            Box::new(|a: &ArmFacts| a.truncation_share),
        ),
        (
            "bytes per vector at full width",
            "bytes",
            false,
            Box::new(|a: &ArmFacts| (a.dims * 4) as f64),
        ),
    ] {
        rows.push(EmbeddingRow {
            family: label.to_string(),
            metric: metric.to_string(),
            higher_is_better: higher,
            role: Role::Diagnostic,
            values: arms.iter().map(|a| (a.model_id.clone(), pick(a))).collect(),
            series: BTreeMap::new(),
        });
    }
    Lane {
        name: "cost".to_string(),
        rationale:
            "Throughput is timed on distinct stride-sampled chunks, never on repeated text: a \
             repeated-input benchmark on this machine reported 300 chunks a second against a \
             real 85 to 134, because it was measuring a prefix cache. Truncation share is \
             printed beside throughput because a model that is fast for having read less of \
             each chunk is not fast."
                .to_string(),
        rows,
    }
}

/// Compare every candidate against the baseline on every primary row.
fn judge(lanes: &[Lane], baseline: &str, seed: u64) -> Vec<ArmJudgement> {
    let mut out = Vec::new();
    for lane in lanes {
        for row in &lane.rows {
            if row.role != Role::Primary {
                continue;
            }
            let Some(base_series) = row.series.get(baseline) else { continue };
            let base_value = row.values.get(baseline).copied().unwrap_or(0.0);
            // Oriented so that a positive delta always means the candidate is
            // better. `stats::verdict` reads the interval's sign and has no idea
            // which way a metric runs; the abstention lane is the first primary
            // row where lower is better, and without this its verdicts would come
            // out exactly backwards. `candidate_value` and `baseline_value` stay
            // in the row's own units.
            let orient = |v: &Vec<f64>| -> Vec<f64> {
                if row.higher_is_better {
                    v.clone()
                } else {
                    v.iter().map(|x| -x).collect()
                }
            };
            let base_oriented = orient(base_series);
            for (model, series) in &row.series {
                if model == baseline {
                    continue;
                }
                let paired = stats::compare(&orient(series), &base_oriented, seed);
                let verdict = paired
                    .as_ref()
                    .map(|p| stats::verdict(p, RANKING_THRESHOLD))
                    .unwrap_or(Verdict::Inconclusive);
                out.push(ArmJudgement {
                    lane: lane.name.clone(),
                    family: row.family.clone(),
                    metric: row.metric.clone(),
                    model: model.clone(),
                    baseline: baseline.to_string(),
                    candidate_value: row.values.get(model).copied().unwrap_or(0.0),
                    baseline_value: base_value,
                    verdict,
                    threshold: RANKING_THRESHOLD,
                    paired,
                });
            }
        }
    }
    out
}

fn caveats() -> Vec<String> {
    vec![
        "Every arm embedded the same corpus from scratch with its own model, its own prefixes, \
         its own pooling and its own token bound. No cache is reused between arms, and the \
         header of each one names the corpus digest, the model and the manifest digest it was \
         made with; a run refuses before it starts if any two of those disagree."
            .to_string(),
        "The queries are generated once from the shared corpus and are byte-identical across \
         arms. Each arm embeds them with its own model, which is the only honest way to ask two \
         models the same question."
            .to_string(),
        "The dense lane applies no per-document cap and computes its ideal ranking the same way \
         for every arm, so its absolute values sit below the hybrid lane's and its comparisons \
         are exact."
            .to_string(),
        "Throughput is measured on distinct chunks. On this machine a repeated-input embedding \
         benchmark reports roughly three times the real rate, because shared prefixes collapse \
         in the cache."
            .to_string(),
        "A model that wins on this suite and loses on a public retrieval benchmark has \
         overfitted to this corpus. This card cannot see that; it is the reason gate G5 exists \
         outside it."
            .to_string(),
    ]
}

pub fn render(card: &EmbeddingCard) -> String {
    let mut s = String::new();
    s.push_str("# Embedding model head-to-head\n\n");
    s.push_str(&format!(
        "Run `{}`, corpus `{}` ({} chunks across {} documents), baseline `{}`. Every comparison \
         is a paired bootstrap 95% interval plus a paired randomization test over per-query \
         scores, against a practical threshold of {} declared before the run.\n\n",
        card.run_id,
        short(&card.corpus_sha256),
        card.corpus_chunks,
        card.corpus_documents,
        card.baseline,
        card.ranking_threshold
    ));

    s.push_str("## The arms\n\n");
    s.push_str("| model | dims | max tokens | pooling | query prefix | truncated | weights MB | manifest |\n");
    s.push_str("|---|---:|---:|---|---|---:|---:|---|\n");
    for a in &card.arms {
        s.push_str(&format!(
            "| `{}` | {} | {} | {} | `{}` | {:.2}% | {:.0} | `{}`{} |\n",
            a.model_id,
            a.dims,
            a.max_tokens,
            a.pooling,
            a.query_prefix.replace('|', "\\|"),
            100.0 * a.truncation_share,
            a.model_bytes as f64 / 1e6,
            short(&a.manifest_sha256),
            if a.manifest_on_disk { "" } else { " (assumed)" }
        ));
    }
    s.push('\n');

    s.push_str("## Verdicts on the primary rows\n\n");
    if card.judgements.is_empty() {
        s.push_str("No primary row carried per-query scores for both a candidate and the baseline.\n\n");
    } else {
        s.push_str("| lane | family | model | value | baseline | delta | 95% interval | p | verdict |\n");
        s.push_str("|---|---|---|---:|---:|---:|---|---:|---|\n");
        for j in &card.judgements {
            let (delta, interval, p) = match &j.paired {
                Some(p) => (
                    format!("{:+.4}", p.delta),
                    format!("[{:+.4}, {:+.4}]", p.low, p.high),
                    format!("{:.4}", p.p_value),
                ),
                None => ("n/a".into(), "n/a".into(), "n/a".into()),
            };
            s.push_str(&format!(
                "| {} | {} | `{}` | {:.4} | {:.4} | {delta} | {interval} | {p} | {} |\n",
                j.lane,
                j.family,
                j.model,
                j.candidate_value,
                j.baseline_value,
                j.verdict.label()
            ));
        }
        s.push('\n');
    }

    for lane in &card.lanes {
        s.push_str(&format!("## {}\n\n{}\n\n", lane.name, lane.rationale));
        let mut models: Vec<&String> = lane.rows.iter().flat_map(|r| r.values.keys()).collect();
        models.sort();
        models.dedup();
        s.push_str("| family | metric | ");
        s.push_str(&models.iter().map(|m| format!("`{m}`")).collect::<Vec<_>>().join(" | "));
        s.push_str(" |\n|---|---|");
        s.push_str(&"---:|".repeat(models.len()));
        s.push('\n');
        for row in &lane.rows {
            s.push_str(&format!(
                "| {}{} | {} |",
                row.family,
                if row.role == Role::Primary { " **(primary)**" } else { "" },
                row.metric
            ));
            for m in &models {
                match row.values.get(*m) {
                    Some(v) => s.push_str(&format!(" {} |", fmt(*v))),
                    None => s.push_str(" - |"),
                }
            }
            s.push('\n');
        }
        s.push('\n');
    }

    s.push_str("## Query families\n\n| family | queries |\n|---|---:|\n");
    for (name, n) in &card.query_counts {
        s.push_str(&format!("| {name} | {n} |\n"));
    }
    s.push('\n');

    s.push_str("## Caveats\n\n");
    for c in &card.caveats {
        s.push_str(&format!("- {c}\n"));
    }
    s.push('\n');

    s.push_str("## Provenance\n\n| | |\n|---|---|\n");
    for (k, v) in &card.provenance {
        s.push_str(&format!("| {k} | {v} |\n"));
    }
    s.push('\n');
    s
}

fn fmt(v: f64) -> String {
    if v >= 1000.0 {
        format!("{v:.0}")
    } else if v >= 1.0 {
        format!("{v:.2}")
    } else {
        format!("{v:.4}")
    }
}

pub fn print_summary(card: &EmbeddingCard) {
    eprintln!("\nbaseline: {}", card.baseline);
    for j in &card.judgements {
        if j.lane.contains("composite") {
            eprintln!(
                "  {} / {}: {} {:.4} vs {:.4} ({:+.4}) -> {}",
                j.lane,
                j.family,
                j.model,
                j.candidate_value,
                j.baseline_value,
                j.candidate_value - j.baseline_value,
                j.verdict.label()
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use inillucent_core::model::{ModelManifest, Pooling, Prefixes};

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir()
            .join(format!("inillucent-gradeembed-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn arm_facts_for(id: &str) -> ArmFacts {
        ArmFacts {
            model_id: id.to_string(),
            dims: 768,
            max_tokens: 512,
            mrl_widths: vec![768],
            pooling: "mean".into(),
            query_prefix: String::new(),
            document_prefix: String::new(),
            manifest_sha256: "m".into(),
            weights_sha256: "w".into(),
            tokenizer_sha256: "t".into(),
            manifest_on_disk: true,
            cache_path: "c".into(),
            cache_bytes: 0,
            chunks: 0,
            truncated_chunks: 0,
            truncation_share: 0.0,
            model_bytes: 0,
            source: None,
            chunks_per_second: BTreeMap::new(),
            chunks_per_second_runs: BTreeMap::new(),
            tokens_per_chunk: None,
            sample_truncation_share: None,
            bytes_per_vector: BTreeMap::new(),
        }
    }

    fn chunk(i: usize) -> inillucent_core::store::ChunkInput {
        inillucent_core::store::ChunkInput {
            source: "confluence".into(),
            external_doc_id: format!("{}", i / 4),
            chunk_index: (i % 4) as u32,
            heading_path: vec!["Section".into()],
            content: format!("chunk {i} about offer eligibility and redemption rules"),
            title: format!("Document {}", i / 4),
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
        }
    }

    /// Write a cache with a chosen header, plus a model directory that agrees
    /// with it, so a refusal test can move exactly one field.
    fn arm_at(
        root: &Path,
        id: &str,
        dims: usize,
        chunks: usize,
        corpus_sha: &str,
        seed_digest: &str,
    ) -> PathBuf {
        let dir = root.join("models").join(id);
        std::fs::create_dir_all(&dir).unwrap();
        let manifest = ModelManifest {
            id: id.to_string(),
            dims,
            mrl_widths: vec![dims],
            prefixes: Prefixes::none(),
            pooling: Pooling::Mean,
            max_tokens: 512,
            layer_norm: false,
            model_file: "model.onnx".into(),
            token_type_ids: false,
            backend: inillucent_core::model::Backend::Onnx,
            output: inillucent_core::model::Output::TokenEmbeddings,
            output_name: String::new(),
            tokenizer_sha256: String::new(),
            weights_sha256: String::new(),
            recipe_git_sha: None,
            source: None,
        };
        std::fs::write(
            dir.join(models::MANIFEST_FILE),
            serde_json::to_string_pretty(&manifest).unwrap(),
        )
        .unwrap();

        let inputs: Vec<inillucent_core::store::ChunkInput> = (0..chunks).map(chunk).collect();
        let vectors: Vec<Vec<f32>> = (0..chunks)
            .map(|i| {
                let mut v: Vec<f32> = (0..dims).map(|d| ((i * dims + d) as f32).sin()).collect();
                inillucent_core::distance::normalize(&mut v);
                v
            })
            .collect();
        let header = CacheHeader {
            version: 4,
            corpus_sha256: corpus_sha.to_string(),
            model_id: id.to_string(),
            manifest_sha256: corpus::manifest_digest(&manifest),
            dims,
            max_tokens: 512,
            chunk_count: chunks,
            truncated_chunks: 0,
            query_seed_digest: seed_digest.to_string(),
        };
        let path = root.join(format!("{id}.cache"));
        corpus::save_cache(&Corpus { chunks: inputs, vectors, dims, header }, &path).unwrap();
        path
    }

    fn options(root: &Path, caches: Vec<PathBuf>) -> EmbeddingGradeOptions {
        EmbeddingGradeOptions {
            caches,
            models_root: root.join("models"),
            baseline: None,
            limit: None,
            per_source: 2,
            device: Device::Cpu,
            runs_dir: root.join("runs"),
            out: root.join("card.md"),
            stats_seed: 1,
            dense: true,
            hybrid: false,
            cost: false,
            matryoshka: false,
            abstention: false,
            lexical: false,
            cost_samples: 8,
            cost_devices: vec![],
            cost_repeats: 3,
            matryoshka_chunks: 8,
            arm_options: crate::arm::ArmOptions::default(),
        }
    }


    /// The lexical lane has one column and no judgement, because there is no
    /// model in it. A second column would be the same ranking printed twice, and
    /// a judgement would need a baseline model this lane does not have.
    #[test]
    fn the_lexical_lane_prints_one_column_and_is_never_judged() {
        let mut lexical: BTreeMap<String, BTreeMap<String, Vec<f64>>> = BTreeMap::new();
        for (family, scores) in [
            ("document identity", vec![1.0, 0.5]),
            ("heading", vec![0.0, 0.5]),
            ("passage evidence", vec![0.25]),
        ] {
            lexical.entry(family.to_string()).or_default().insert(M_NDCG.to_string(), scores);
        }
        let lane = lexical_lane(&lexical);

        let mut columns: Vec<&String> = lane.rows.iter().flat_map(|r| r.values.keys()).collect();
        columns.sort();
        columns.dedup();
        assert_eq!(columns.len(), 1, "one column, because no model changes this ranking");
        assert_eq!(columns[0], LEXICAL_COLUMN);

        assert!(
            lane.rows.iter().all(|r| r.role == Role::Diagnostic && r.series.is_empty()),
            "nothing in this lane is judged, so no row is primary and none carries a series"
        );
        assert!(
            judge(std::slice::from_ref(&lane), LEXICAL_COLUMN, 1).is_empty(),
            "a lane with no baseline model produces no judgements"
        );

        // Each composite concatenates only the families it is declared over, so
        // the declared one leaves passage evidence out and reads 0.5 where the
        // promoted one includes it and reads 0.45.
        let value = |prefix: &str| {
            lane.rows
                .iter()
                .find(|r| r.family.starts_with(prefix))
                .unwrap_or_else(|| panic!("{prefix} is on the card"))
                .values[LEXICAL_COLUMN]
        };
        assert_eq!(value("composite, declared"), 0.5);
        assert_eq!(value("composite, promoted"), 0.45);
    }

    fn seeds_now() -> String {
        corpus::seed_digest(&scenarios::seeds())
    }

    #[test]
    fn two_caches_of_different_corpora_are_refused_by_name() {
        let root = scratch("corpora");
        let a = arm_at(&root, "model-a", 8, 40, "corpus-one", &seeds_now());
        let b = arm_at(&root, "model-b", 8, 40, "corpus-two", &seeds_now());
        let err = resolve_arms(&options(&root, vec![a, b])).unwrap_err().to_string();
        assert!(err.contains("two different"), "{err}");
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn two_caches_of_different_chunk_counts_are_refused_by_name() {
        let root = scratch("counts");
        let a = arm_at(&root, "model-a", 8, 40, "corpus-one", &seeds_now());
        let b = arm_at(&root, "model-b", 8, 36, "corpus-one", &seeds_now());
        let err = resolve_arms(&options(&root, vec![a, b])).unwrap_err().to_string();
        assert!(err.contains("chunks and"), "{err}");
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn two_caches_written_under_different_query_seeds_are_refused_by_name() {
        let root = scratch("seeds");
        let a = arm_at(&root, "model-a", 8, 40, "corpus-one", &seeds_now());
        let b = arm_at(&root, "model-b", 8, 40, "corpus-one", "a-different-seed-table");
        let err = resolve_arms(&options(&root, vec![a, b])).unwrap_err().to_string();
        assert!(err.contains("query seed table"), "{err}");
        std::fs::remove_dir_all(&root).ok();
    }

    /// The case a pairwise check cannot see: every cache agrees with every other
    /// and all of them predate a change to the seeds this binary now uses.
    #[test]
    fn caches_that_agree_with_each_other_and_not_with_the_harness_are_refused() {
        let root = scratch("stale");
        let a = arm_at(&root, "model-a", 8, 40, "corpus-one", "an-older-seed-table");
        let b = arm_at(&root, "model-b", 8, 40, "corpus-one", "an-older-seed-table");
        let err = resolve_arms(&options(&root, vec![a, b])).unwrap_err().to_string();
        assert!(err.contains("this harness digests to"), "{err}");
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn a_cache_whose_width_contradicts_its_manifest_is_refused_by_name() {
        let root = scratch("width");
        let a = arm_at(&root, "model-a", 8, 40, "corpus-one", &seeds_now());
        let b = arm_at(&root, "model-b", 8, 40, "corpus-one", &seeds_now());
        // Rewrite model-b's manifest to declare a width its cache does not hold.
        let dir = root.join("models").join("model-b");
        let text = std::fs::read_to_string(dir.join(models::MANIFEST_FILE)).unwrap();
        let mut manifest: ModelManifest = serde_json::from_str(&text).unwrap();
        manifest.dims = 16;
        manifest.mrl_widths = vec![16];
        std::fs::write(
            dir.join(models::MANIFEST_FILE),
            serde_json::to_string_pretty(&manifest).unwrap(),
        )
        .unwrap();
        let err = resolve_arms(&options(&root, vec![a, b])).unwrap_err().to_string();
        // The manifest digest moves with the width, so whichever check fires
        // first, the run refuses and says which model and which field.
        assert!(err.contains("model-b"), "{err}");
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn a_manifest_edited_after_the_embedding_run_is_refused_by_name() {
        let root = scratch("manifest-moved");
        let a = arm_at(&root, "model-a", 8, 40, "corpus-one", &seeds_now());
        let b = arm_at(&root, "model-b", 8, 40, "corpus-one", &seeds_now());
        let dir = root.join("models").join("model-b");
        let text = std::fs::read_to_string(dir.join(models::MANIFEST_FILE)).unwrap();
        let mut manifest: ModelManifest = serde_json::from_str(&text).unwrap();
        // A prefix change: invisible in the weights, and it moves every vector.
        manifest.prefixes = Prefixes::query_only("query: ");
        std::fs::write(
            dir.join(models::MANIFEST_FILE),
            serde_json::to_string_pretty(&manifest).unwrap(),
        )
        .unwrap();
        let err = resolve_arms(&options(&root, vec![a, b])).unwrap_err().to_string();
        assert!(err.contains("has changed since these vectors were made"), "{err}");
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn a_legacy_cache_with_no_provenance_is_refused_rather_than_assumed() {
        let root = scratch("legacy");
        let a = arm_at(&root, "model-a", 8, 40, "corpus-one", &seeds_now());
        // A header with an empty corpus digest is what a version 3 cache reads as.
        let b_path = root.join("legacy.cache");
        let inputs: Vec<inillucent_core::store::ChunkInput> = (0..40).map(chunk).collect();
        let vectors: Vec<Vec<f32>> = (0..40)
            .map(|_| {
                let mut v = vec![1.0f32; 8];
                inillucent_core::distance::normalize(&mut v);
                v
            })
            .collect();
        let header = CacheHeader {
            version: 4,
            corpus_sha256: String::new(),
            model_id: String::new(),
            manifest_sha256: String::new(),
            dims: 8,
            max_tokens: 0,
            chunk_count: 40,
            truncated_chunks: 0,
            query_seed_digest: String::new(),
        };
        corpus::save_cache(&Corpus { chunks: inputs, vectors, dims: 8, header }, &b_path).unwrap();
        let err = resolve_arms(&options(&root, vec![a, b_path])).unwrap_err().to_string();
        assert!(err.contains("cannot say which corpus"), "{err}");
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn one_cache_is_not_a_comparison() {
        let root = scratch("single");
        let a = arm_at(&root, "model-a", 8, 40, "corpus-one", &seeds_now());
        let err = resolve_arms(&options(&root, vec![a])).unwrap_err().to_string();
        assert!(err.contains("at least two caches"), "{err}");
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn the_same_model_twice_is_not_a_comparison() {
        let root = scratch("dup");
        let a = arm_at(&root, "model-a", 8, 40, "corpus-one", &seeds_now());
        let copy = root.join("model-a-again.cache");
        std::fs::copy(&a, &copy).unwrap();
        let err = resolve_arms(&options(&root, vec![a, copy])).unwrap_err().to_string();
        assert!(err.contains("appears twice"), "{err}");
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn matching_caches_resolve_and_agree_on_what_they_are() {
        let root = scratch("agree");
        let a = arm_at(&root, "model-a", 8, 40, "corpus-one", &seeds_now());
        let b = arm_at(&root, "model-b", 8, 40, "corpus-one", &seeds_now());
        let arms = resolve_arms(&options(&root, vec![a, b])).unwrap();
        assert_eq!(arms.len(), 2);
        assert_eq!(arms[0].header.corpus_sha256, arms[1].header.corpus_sha256);
        assert!(arms.iter().all(|a| a.header.has_provenance()));
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn exhaustive_cosine_returns_the_nearest_vectors_in_order() {
        let vectors: Vec<Vec<f32>> = (0..50)
            .map(|i| {
                let mut v = vec![0.0f32; 4];
                v[0] = 1.0;
                v[1] = i as f32 * 0.01;
                inillucent_core::distance::normalize(&mut v);
                v
            })
            .collect();
        let query = vectors[0].clone();
        let got = exhaustive_top_k(&vectors, &query, 5);
        assert_eq!(got[0], 0);
        // Scores must descend.
        let scores: Vec<f32> = got.iter().map(|&i| dot(&vectors[i], &query)).collect();
        for pair in scores.windows(2) {
            assert!(pair[0] >= pair[1] - 1e-6, "{scores:?}");
        }
    }

    /// The dense lane must agree with the exhaustive search the score card
    /// already uses as its reference ranking.
    ///
    /// Two implementations of "the exact answer" that disagree would mean the
    /// dense lane and the accuracy scenario were measuring against two different
    /// truths, and the card would carry both without saying so. This is cheap and
    /// it is the one place that can be checked directly.
    #[test]
    fn the_dense_lane_agrees_with_the_exhaustive_search_the_card_uses() {
        let dims = 16;
        let n = 300;
        let chunks: Vec<inillucent_core::store::ChunkInput> = (0..n).map(chunk).collect();
        // A non-periodic generator, deliberately. A modular one produced two
        // chunks with byte-identical vectors, and the two implementations then
        // broke that exact tie differently - which is a fact about tie-breaking
        // and not about either search, and it would have made this test read as a
        // disagreement about the ranking.
        let mut state = 0x2545_F491_4F6C_DD1Du64;
        let mut next = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            (state >> 11) as f32 / (1u64 << 53) as f32 - 0.5
        };
        let vectors: Vec<Vec<f32>> = (0..n)
            .map(|_| {
                let mut v: Vec<f32> = (0..dims).map(|_| next()).collect();
                inillucent_core::distance::normalize(&mut v);
                v
            })
            .collect();
        let corpus = Corpus {
            chunks,
            vectors: vectors.clone(),
            dims,
            header: CacheHeader {
                version: 4,
                corpus_sha256: "c".into(),
                model_id: "m".into(),
                manifest_sha256: "d".into(),
                dims,
                max_tokens: 512,
                chunk_count: n,
                truncated_chunks: 0,
                query_seed_digest: "s".into(),
            },
        };
        let (index, index_keys, _stats, _seconds) =
            scenarios::build_index(&corpus, None, false).unwrap();
        let keys: Vec<String> = (0..n)
            .map(|i| {
                format!("{}#{}", corpus.chunks[i].external_doc_id, corpus.chunks[i].chunk_index)
            })
            .collect();
        assert_eq!(keys, index_keys);

        let compiled = index.compile(&Filter::default());
        for probe in [0usize, 41, 199] {
            let query = &vectors[probe];
            let card_ranking: Vec<String> = index
                .exhaustive_search(query, &compiled, K)
                .expect("the harness embeds at the index's width")
                .into_iter()
                .map(|n| index_keys[n.chunk as usize].clone())
                .collect();
            let lane_ranking: Vec<String> =
                exhaustive_top_k(&vectors, query, K).into_iter().map(|i| keys[i].clone()).collect();
            assert_eq!(card_ranking, lane_ranking, "probe {probe}");
        }
    }

    #[test]
    fn every_timing_pass_reads_chunks_no_earlier_pass_read() {
        let texts = 2000;
        let repeats = 3;
        let mut seen: Vec<usize> = Vec::new();
        for pass in 0..repeats {
            let range = timing_range(texts, repeats, pass).unwrap();
            assert_eq!(range.len(), 666, "pass {pass} is a different size from the others");
            for i in range {
                assert!(!seen.contains(&i), "chunk {i} was timed twice");
                seen.push(i);
            }
        }
        // The leftover two chunks are dropped rather than lengthening the last
        // pass: a pass over more text is not comparable with the ones beside it.
        assert_eq!(seen.len(), 1998);
    }

    #[test]
    fn a_sample_too_small_to_split_is_refused_rather_than_timed_twice() {
        // Two chunks and three passes cannot be disjoint, and the honest failure
        // is to say so. Silently reusing text here is exactly the mistake that
        // made a benchmark on this box report 300 chunks a second against a real
        // 85: the second pass was answered from a cache.
        let err = timing_range(2, 3, 0).unwrap_err().to_string();
        assert!(err.contains("disjoint"), "{err}");
        // One pass over two chunks is fine.
        assert_eq!(timing_range(2, 1, 0).unwrap(), 0..2);
    }

    #[test]
    fn the_median_reports_the_middle_rather_than_the_average() {
        // Phase 0's three CPU timings of one model, in the order they happened.
        // Their mean is 7.6 and their middle is 8.3; the mean is dragged by the
        // one run where something else had the machine.
        assert!((median(&[3.1, 11.3, 8.3]) - 8.3).abs() < 1e-9);
        assert!((median(&[3.1, 11.3]) - 7.2).abs() < 1e-9);
        assert_eq!(median(&[]), 0.0);
        assert!((median(&[4.0]) - 4.0).abs() < 1e-9);
    }

    #[test]
    fn the_spread_row_appears_only_when_there_is_more_than_one_timing() {
        let mut one = arm_facts_for("one");
        one.chunks_per_second.insert("cpu".into(), 10.0);
        one.chunks_per_second_runs.insert("cpu".into(), vec![10.0]);
        let lane = cost_lane(&[one.clone()]);
        assert!(
            !lane.rows.iter().any(|r| r.metric.contains("spread")),
            "a single timing has no spread to report"
        );

        let mut many = arm_facts_for("many");
        many.chunks_per_second.insert("cpu".into(), 8.3);
        many.chunks_per_second_runs.insert("cpu".into(), vec![3.1, 11.3, 8.3]);
        let lane = cost_lane(&[many]);
        let row = lane
            .rows
            .iter()
            .find(|r| r.metric.contains("spread"))
            .expect("a lane with three timings reports their spread");
        // (11.3 - 3.1) / 8.3 = 98.8% of the median, which is the number that says
        // this column cannot decide a gate.
        assert!((row.values["many"] - 98.795).abs() < 0.01, "{:?}", row.values);
    }

    #[test]
    fn each_model_is_calibrated_on_its_own_scale() {
        // Two models that behave identically and differ only in geometry: the
        // second one's vectors sit in a narrower cone, so every cosine it produces
        // is 0.2 higher. Under one shared threshold it would look far worse at
        // abstaining while doing exactly the same thing, which is why G4 says
        // "each model on its own calibrated threshold".
        let mut confidences: BTreeMap<String, BTreeMap<String, Vec<f64>>> = BTreeMap::new();
        let calibration: Vec<f64> = (0..20).map(|i| 0.50 + i as f64 * 0.01).collect();
        let negative: Vec<f64> = (0..20).map(|i| 0.40 + i as f64 * 0.01).collect();
        confidences.insert(
            "wide".into(),
            [(CALIBRATION.to_string(), calibration.clone()), (UNANSWERABLE.to_string(), negative.clone())]
                .into_iter()
                .collect(),
        );
        confidences.insert(
            "narrow".into(),
            [
                (CALIBRATION.to_string(), calibration.iter().map(|v| v + 0.2).collect()),
                (UNANSWERABLE.to_string(), negative.iter().map(|v| v + 0.2).collect()),
            ]
            .into_iter()
            .collect(),
        );

        let lane = abstention_lane(&confidences);
        let rate = &lane.rows[0];
        assert!(rate.role == Role::Primary, "the confident-answer rate is the row G4 is read from");
        assert!(!rate.higher_is_better, "a confident answer to an unanswerable question is bad");
        assert!(
            (rate.values["wide"] - rate.values["narrow"]).abs() < 1e-12,
            "two models with the same behaviour and different scales scored differently: {:?}",
            rate.values
        );
        // And the thresholds themselves show the geometry the rates hide.
        let threshold = &lane.rows[1];
        assert!((threshold.values["narrow"] - threshold.values["wide"] - 0.2).abs() < 1e-9);
        // One value per query, so the row can be tested pairwise like any other.
        assert_eq!(rate.series["wide"].len(), 20);
    }

    #[test]
    fn the_threshold_is_a_score_some_calibration_query_actually_produced() {
        let sorted: Vec<f64> = (0..20).map(|i| i as f64).collect();
        // Nearest rank, not interpolated: 5% of 19 is 0.95, which rounds to 1.
        assert_eq!(percentile(&sorted, 0.05), 1.0);
        assert_eq!(percentile(&sorted, 0.0), 0.0);
        assert_eq!(percentile(&sorted, 1.0), 19.0);
        assert_eq!(percentile(&[], 0.05), 0.0);
    }

    #[test]
    fn a_primary_row_where_lower_is_better_is_judged_in_its_own_direction() {
        // The candidate answers confidently on a tenth of the unanswerable
        // questions and the baseline on nine tenths. That is a large improvement,
        // and before the orientation fix it was reported as `worse`.
        let candidate: Vec<f64> = (0..40).map(|i| if i % 10 == 0 { 1.0 } else { 0.0 }).collect();
        let baseline: Vec<f64> = (0..40).map(|i| if i % 10 == 0 { 0.0 } else { 1.0 }).collect();
        let row = EmbeddingRow {
            family: "questions with no answer in the corpus".into(),
            metric: "confident answer rate at the model's own threshold".into(),
            higher_is_better: false,
            role: Role::Primary,
            values: [("new".to_string(), 0.1), ("v1.5".to_string(), 0.9)].into_iter().collect(),
            series: [("new".to_string(), candidate), ("v1.5".to_string(), baseline)]
                .into_iter()
                .collect(),
        };
        let lane = Lane { name: "abstention".into(), rationale: String::new(), rows: vec![row] };
        let judged = judge(&[lane], "v1.5", 7);
        assert_eq!(judged.len(), 1);
        assert_eq!(judged[0].verdict, Verdict::Better, "{:?}", judged[0].paired);
        // The printed values stay in the metric's own units - the orientation is
        // only for the interval, not for the numbers a reader sees.
        assert!((judged[0].candidate_value - 0.1).abs() < 1e-12);
        assert!((judged[0].baseline_value - 0.9).abs() < 1e-12);
    }

    #[test]
    fn the_composite_concatenates_families_in_a_fixed_order() {
        let mut per_family: BTreeMap<String, Vec<f64>> = BTreeMap::new();
        per_family.insert("heading".into(), vec![0.5, 0.6]);
        per_family.insert("document identity".into(), vec![0.1, 0.2, 0.3]);
        let got = composite(&per_family, HEADLINE);
        assert_eq!(got, vec![0.1, 0.2, 0.3, 0.5, 0.6]);
        // A family the model did not answer is skipped rather than zero-filled,
        // and the length check in the lane is what stops that being compared.
        let got = composite(&per_family, &["document identity", "missing"]);
        assert_eq!(got, vec![0.1, 0.2, 0.3]);
    }
}
