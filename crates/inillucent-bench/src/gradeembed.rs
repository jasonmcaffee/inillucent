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
use crate::stats::{Paired, Verdict};

mod card;

// The card is the run's other half and it is re-imported by name rather than
// reached through `card::`, so every call site below reads as it did when the
// two halves were one file.
use card::{
    abstention_lane, caveats, composite_lane, cost_lane, judge, lane_from, lexical_lane,
    matryoshka_lane, LaneScores, LEXICAL_COLUMN,
};
pub use card::{print_summary, rejudge, render};

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
    /// Query vectors an arm's model produced elsewhere, by model id.
    ///
    /// An arm's queries are embedded by that arm's own model, which means the harness
    /// has to be able to open it, and two things this ticket grades it cannot:
    /// `Qwen3-Embedding-8B` and `-4B` are safetensors with no ONNX graph and no GGUF,
    /// and the loop grades five weight interpolations an iteration straight out of
    /// PyTorch. Exporting first would put the exporter inside the loop, and task-1818
    /// measured the exporter's own effect at cosine 0.99999 over 300 chunks, which is
    /// small but is not nothing when the kill test's threshold is 0.01.
    ///
    /// So the vectors can come from the same producer that made the corpus vectors,
    /// through `query-texts` and `cache-from-vectors`. Nothing is taken on trust: the
    /// sidecar records the corpus digest, the seed digest, the per-source count and
    /// the model id it was made for, and every one is checked against what this run
    /// generated before a single vector is read.
    pub query_vectors: BTreeMap<String, PathBuf>,
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
#[derive(Serialize, serde::Deserialize, Clone)]
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

#[derive(Serialize, serde::Deserialize, Clone)]
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

#[derive(Serialize, serde::Deserialize, Clone)]
pub struct Lane {
    pub name: String,
    pub rationale: String,
    pub rows: Vec<EmbeddingRow>,
}

/// One candidate against the baseline on one row.
#[derive(Serialize, serde::Deserialize, Clone)]
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

#[derive(Serialize, serde::Deserialize)]
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
        arms.push(Arm {
            cache: path.clone(),
            header,
            model,
        });
    }

    // Every arm against the first, so an error names one pair rather than a set.
    let Some((first, rest)) = arms.split_first() else {
        return Ok(arms);
    };
    for arm in rest {
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
            (
                M_NDCG_GRADED,
                ndcg_graded_at_k(&got, &grades, K, per_doc_cap) as f64,
            ),
            (
                M_NDCG,
                ndcg_at_k_attainable(&got, &correct, K, per_doc_cap) as f64,
            ),
            (M_SUCCESS_1, success_at_k(&got, &correct, 1) as f64),
            (M_SUCCESS_10, success_at_k(&got, &correct, K) as f64),
            (M_MRR, reciprocal_rank(&got, &correct) as f64),
            (M_PRECISION_10, precision_at_k(&got, &correct, K) as f64),
            (
                M_EVIDENCE_RECALL,
                graded_recall_at_k(&got, &grades, K, queryset::GRADE_ANSWER) as f64,
            ),
            (
                M_TOP_SCORE,
                hits.first().map(|h| h.score as f64).unwrap_or(0.0),
            ),
        ];
        for (metric, value) in scores {
            per_metric
                .entry(metric.to_string())
                .or_default()
                .push(value);
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

/// One family's query vector, by the query's position in the family.
///
/// **Fallible, because a family and its embedded vectors are two lists that
/// have to be the same length.** If they ever are not, the query at that
/// position would be scored against another query's vector, which is a score
/// that looks ordinary and is wrong.
///
/// @param vectors - the family's query vectors, in the family's order
/// @param index - the query's position in the family
fn query_at(vectors: &[Vec<f32>], index: usize) -> Result<&Vec<f32>> {
    vectors.get(index).with_context(|| {
        format!(
            "query {index} of this family has no vector: {} were embedded",
            vectors.len()
        )
    })
}

/// The corpus key of every chunk, in corpus order.
///
/// **One function rather than the same `format!` in two places.** Both the
/// grading run and the sidecar writer generate their query set from these
/// keys, and a different spelling in either would produce a different query
/// set - so the sidecar would hold vectors of text the run never asked about,
/// and nothing would say so.
///
/// @param corpus - the loaded cache
fn corpus_keys(corpus: &Corpus) -> Vec<String> {
    corpus
        .chunks
        .iter()
        .map(|c| format!("{}#{}", c.external_doc_id, c.chunk_index))
        .collect()
}

/// Build the query families. Identical for every arm by construction: they are
/// generated from the chunks, and the refusal above has already established that
/// every arm's chunks are the same chunks.
/// @param corpus - any arm's corpus; the text is shared
/// @param keys - the corpus keys, in corpus order
/// @param per_source - queries per source for the identity family
fn build_families(corpus: &Corpus, keys: &[String], per_source: usize, n: usize) -> Families {
    let sliced = corpus.chunks.get(..n).unwrap_or(&corpus.chunks);
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
    Families {
        named,
        counts,
        abstention,
    }
}

/// Write every generated query, in the order an arm's vectors have to be in.
///
/// The order is the contract between this harness and whatever embeds the queries: the
/// six named families in the order the composite concatenates them, then the two
/// abstention families. It is written out rather than described, and the header line
/// carries the corpus digest, the seed digest and the `--per-source` the queries were
/// generated at, so the sidecar built from this file can be refused when any of them
/// stops matching.
/// @param cache - a cache of the corpus the queries are generated from
/// @param per_source - queries per source for the identity family
/// @param limit - grade only the first N chunks, as `grade-embedding`'s own `--limit` does
/// @param out - where the JSONL goes
pub fn write_query_texts(
    cache: &Path,
    per_source: usize,
    limit: Option<usize>,
    out: &Path,
) -> Result<usize> {
    use std::io::Write;

    let corpus = corpus::load_cache(cache)?;
    let n = limit.unwrap_or(corpus.len()).min(corpus.len());
    // The same keys `run` builds, because both call the same function. A
    // different key spelling would generate a different query set and the
    // sidecar would be vectors of the wrong text.
    let keys = corpus_keys(&corpus);
    let families = build_families(&corpus, &keys, per_source, n);
    let total: usize = families
        .named
        .iter()
        .chain(families.abstention.iter())
        .map(|(_, qs)| qs.len())
        .sum();
    let mut file = std::io::BufWriter::new(std::fs::File::create(out)?);
    writeln!(
        file,
        "{}",
        serde_json::json!({
            "corpus_sha256": corpus.header.corpus_sha256,
            "query_seed_digest": corpus.header.query_seed_digest,
            "per_source": per_source,
            "queries": total,
            "chunks": n,
        })
    )?;
    for (name, qs) in families.named.iter().chain(families.abstention.iter()) {
        for q in qs {
            writeln!(
                file,
                "{}",
                serde_json::json!({"family": name, "text": q.text})
            )?;
        }
    }
    file.flush()?;
    Ok(total)
}

/// What a query-vector sidecar claims about itself, written beside the `.f32` file.
///
/// Every field is here to be checked rather than to be read. A sidecar made against a
/// different corpus, a different seed table, a different `--per-source` or a different
/// model is a file that would otherwise load, produce numbers, and be wrong quietly.
#[derive(serde::Deserialize)]
struct QueryVectorSidecar {
    corpus_sha256: String,
    query_seed_digest: String,
    per_source: usize,
    queries: usize,
    model_id: String,
    dims: usize,
}

/// Read query vectors an external embedder produced, refusing anything that does not
/// describe this run.
///
/// The file is little-endian f32, `dims` floats a query, in exactly the order
/// `query-texts` wrote them: every named family in order, then the two abstention
/// families. The sidecar JSON beside it says which corpus, which seed table, which
/// `--per-source` and which model it was made for, and all four have to match or this
/// refuses by name. That is the same standard the cache header is held to, and for the
/// same reason: a number from the wrong file looks exactly like a number from the right
/// one.
/// @param path - the `.f32` vector file
/// @param model_id - the arm this is supposed to belong to
/// @param dims - the arm's declared width
/// @param header - the cache header this run is grading against
/// @param per_source - the queries-per-source this run generated with
/// @param expected - how many queries were generated, named and abstention together
fn read_query_vectors(
    path: &Path,
    model_id: &str,
    dims: usize,
    header: &CacheHeader,
    per_source: usize,
    expected: usize,
) -> Result<Vec<Vec<f32>>> {
    use std::io::Read;

    let meta_path = path.with_extension("meta.json");
    let meta: QueryVectorSidecar = serde_json::from_str(
        &std::fs::read_to_string(&meta_path)
            .with_context(|| format!("reading {}", meta_path.display()))?,
    )
    .with_context(|| format!("parsing {}", meta_path.display()))?;
    anyhow::ensure!(
        meta.model_id == model_id,
        "{} was made for {} and is being read for {model_id}",
        meta_path.display(),
        meta.model_id
    );
    anyhow::ensure!(
        meta.corpus_sha256 == header.corpus_sha256,
        "{} was made against corpus {} and this run grades corpus {}",
        meta_path.display(),
        short(&meta.corpus_sha256),
        short(&header.corpus_sha256)
    );
    anyhow::ensure!(
        meta.query_seed_digest == header.query_seed_digest,
        "{} was made against seed table {} and this run's is {}",
        meta_path.display(),
        short(&meta.query_seed_digest),
        short(&header.query_seed_digest)
    );
    anyhow::ensure!(
        meta.per_source == per_source,
        "{} was made at --per-source {} and this run is {per_source}",
        meta_path.display(),
        meta.per_source
    );
    anyhow::ensure!(
        meta.dims == dims,
        "{} says {} dims and {model_id}'s manifest says {dims}",
        meta_path.display(),
        meta.dims
    );
    anyhow::ensure!(
        meta.queries == expected,
        "{} holds {} queries and this run generated {expected}",
        meta_path.display(),
        meta.queries
    );
    let size = std::fs::metadata(path)
        .with_context(|| format!("reading {}", path.display()))?
        .len() as usize;
    anyhow::ensure!(
        size == expected * dims * 4,
        "{} is {size} bytes and {expected} queries at {dims} dims is {} bytes",
        path.display(),
        expected * dims * 4
    );
    let mut reader = std::io::BufReader::new(std::fs::File::open(path)?);
    let mut buf = vec![0u8; dims * 4];
    let mut out = Vec::with_capacity(expected);
    for _ in 0..expected {
        reader.read_exact(&mut buf)?;
        // `chunks_exact(4)` yields slices of exactly four bytes, so the array
        // conversion is that fact stated where it can be checked.
        out.push(
            buf.chunks_exact(4)
                .filter_map(|b| <[u8; 4]>::try_from(b).ok())
                .map(f32::from_le_bytes)
                .collect(),
        );
    }
    Ok(out)
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

/// Decide where one arm's query vectors come from: its own model, or a sidecar.
///
/// Every arm's queries are embedded by that arm's own model with that arm's own prefixes,
/// because a query vector produced by a different model is not a measurement of this one. The
/// sidecar is the exception and it exists for one case: a model this harness cannot open at
/// all, whose owner produced the vectors elsewhere. `read_query_vectors` is what makes that
/// safe - it refuses a sidecar that does not describe this run - so the choice here is only
/// which of the two paths to take.
///
/// @param arm - the arm being graded, for its model, its cache header and its dimensions
/// @param families - the graded families, to count how many vectors a sidecar must carry
/// @param options - the run's options, holding the sidecar map and the arm options
fn open_query_source(
    arm: &Arm,
    families: &Families,
    options: &EmbeddingGradeOptions,
) -> Result<(
    Option<crate::arm::Arm>,
    Option<std::collections::VecDeque<Vec<f32>>>,
)> {
    let id = arm.header.model_id.clone();
    let Some(path) = options.query_vectors.get(&id).cloned() else {
        let embedder = queryset::open_query_embedder(
            &arm.model,
            &crate::arm::ArmOptions {
                device: options.device,
                ..options.arm_options.clone()
            },
        )
        .with_context(|| format!("opening {id} to embed the query set"))?;
        return Ok((Some(embedder), None));
    };
    let expected: usize = families
        .named
        .iter()
        .chain(families.abstention.iter())
        .map(|(_, qs)| qs.len())
        .sum();
    let rows = read_query_vectors(
        &path,
        &id,
        arm.model.manifest.dims,
        &arm.header,
        options.per_source,
        expected,
    )
    .with_context(|| format!("reading {id}'s query vectors from {}", path.display()))?;
    eprintln!("  {expected} query vectors read from {}", path.display());
    Ok((None, Some(rows.into())))
}

/// Time one arm on every processor the cost lane was asked for, and record what it measured.
///
/// A served arm runs wherever its server runs, and `--device` says nothing about that. Timing it
/// once per requested device would put one number in a `cpu` column and the same number in a
/// `cuda:0` column, and a reader would take that to mean this model reaches GPU throughput on a
/// processor. It is timed once, under a label naming the backend and the endpoint, and the card
/// says so.
///
/// A device that fails is reported and skipped rather than ending the run: the cost lane is one
/// lane of several, and a card missing one throughput number is worth more than no card.
///
/// @param arm - the arm to time
/// @param cost_texts - the shared sample, long enough that each repeat gets its own slice
/// @param cost_repeats - timed passes per device
/// @param options - the run's options, for the requested devices and the arm options
/// @param facts - the arm's row on the card, written in place
fn time_arm(
    arm: &Arm,
    cost_texts: &[String],
    cost_repeats: usize,
    options: &EmbeddingGradeOptions,
    facts: &mut ArmFacts,
) {
    let devices: Vec<Device> = match arm.model.manifest.backend {
        inillucent_core::model::Backend::LlamaCpp => vec![options.device],
        inillucent_core::model::Backend::Onnx => options.cost_devices.clone(),
    };
    for device in &devices {
        match time_model(
            &arm.model,
            cost_texts,
            cost_repeats,
            *device,
            &options.arm_options,
        ) {
            Ok((rates, tokens, share)) => {
                let label = match arm.model.manifest.backend {
                    inillucent_core::model::Backend::LlamaCpp => format!(
                        "llama.cpp at {}",
                        options.arm_options.endpoint_for(&arm.model.manifest.id)
                    ),
                    inillucent_core::model::Backend::Onnx => device.label(),
                };
                let middle = median(&rates);
                let printed: Vec<String> = rates.iter().map(|r| format!("{r:.1}")).collect();
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
            Err(e) => eprintln!("  cost lane on {} failed: {e:#}", device.label()),
        }
    }
}

/// What every arm is graded over: one corpus slice, one query set, one sample.
///
/// **Built once from the first arm's cache, because `resolve_arms` has already
/// refused the run unless every arm agrees on the corpus digest, the chunk
/// count and the query seed table.** Generating the families per arm would ask
/// each model a different question and the card would not say so.
struct Shared {
    /// How many chunks of each cache are graded.
    graded: usize,
    /// The corpus key of every chunk, in corpus order.
    keys: Vec<String>,
    /// The query families every arm answers.
    families: Families,
    /// How many distinct documents the graded slice covers.
    documents: usize,
    /// The texts the cost lane times, long enough for every repeat to get its
    /// own slice: a repeat that reused the previous repeat's text would be
    /// timing a prompt cache on a served arm.
    cost_texts: Vec<String>,
    /// How many timed passes the cost lane makes.
    cost_repeats: usize,
    /// The chunk ordinals the Matryoshka lane narrows.
    matryoshka_rows: Vec<usize>,
}

/// Every lane's per-query scores, keyed the way the card reads them.
///
/// One structure rather than six locals, because every lane writes into one of
/// these and the assembly at the end reads all six.
struct Tables {
    /// family -> metric -> model -> per-query series, exhaustive cosine only.
    dense: LaneScores,
    /// The same, through the shipped pipeline.
    hybrid: LaneScores,
    /// model -> width -> recall against its own full width.
    mrl: BTreeMap<String, BTreeMap<String, f64>>,
    /// family -> metric -> per-query scores, for BM25 with no model at all.
    lexical: BTreeMap<String, BTreeMap<String, Vec<f64>>>,
    /// model -> family -> the top result's confidence on each of its queries.
    confidences: BTreeMap<String, BTreeMap<String, Vec<f64>>>,
}

/// Where one arm's query vectors come from.
///
/// **Two ways, and only one of them is used per arm.** An arm whose model this
/// harness cannot open at all - a different runtime, a format ONNX does not
/// read - supplies its vectors from a sidecar the model produced elsewhere, and
/// `open_query_source` has already checked that sidecar describes this run.
struct QuerySource {
    /// The arm's own model, when the harness could open it.
    embedder: Option<crate::arm::Arm>,
    /// Vectors read from a sidecar, in the order the families are asked for.
    supplied: Option<std::collections::VecDeque<Vec<f32>>>,
}

/// Grades two or more embedding models over the same corpus.
///
/// **Every arm is graded over one corpus and one query set, and
/// `resolve_arms` refuses the run otherwise.** A model that was embedded from
/// a different corpus would score differently for a reason that has nothing to
/// do with the model, and a card that printed both numbers side by side would
/// look exactly like a card comparing two models.
///
/// **One function per stage.** The arms are resolved, the shared corpus facts
/// are read from the first of them, each arm is scored into the tables, the
/// lanes are assembled from those tables, and the manifest is written last - so
/// a manifest on disk means the run finished.
///
/// @param options - the arms, the lanes to run and how much of the corpus to
///   read
pub fn run(options: &EmbeddingGradeOptions) -> Result<EmbeddingCard> {
    let arms = resolve_arms(options)?;
    // The first arm's own facts, taken once. It is the default baseline, the
    // corpus every other arm was just checked against, and the cache the shared
    // query set is generated from - and every one of those is read after `arms`
    // has been iterated, so they are copied rather than borrowed.
    let (lead_header, lead_cache) = {
        let lead = arms
            .first()
            .context("grade-embedding needs at least one arm")?;
        (lead.header.clone(), lead.cache.clone())
    };
    let baseline_id = baseline_named_by(options, &arms, &lead_header)?;
    eprintln!(
        "{} arms over corpus {} ({} chunks), baseline {baseline_id}",
        arms.len(),
        short(&lead_header.corpus_sha256),
        lead_header.chunk_count
    );
    for arm in &arms {
        eprintln!("  {}", arm.header.describe());
    }

    let (git_commit, git_dirty) = runs::git_revision(Path::new("."));
    let run_id = runs::run_id(&git_commit);
    let mut writer = match RunWriter::create(&options.runs_dir, &run_id) {
        Ok(w) => Some(w),
        Err(e) => {
            eprintln!(
                "  could not open the run directory, continuing without per-query records: {e:#}"
            );
            None
        }
    };

    let shared = read_the_shared_corpus(options, &lead_cache)?;
    let mut tables = Tables {
        dense: BTreeMap::new(),
        hybrid: BTreeMap::new(),
        mrl: BTreeMap::new(),
        lexical: BTreeMap::new(),
        confidences: BTreeMap::new(),
    };
    let mut arm_facts: Vec<ArmFacts> = Vec::new();
    for arm in &arms {
        arm_facts.push(score_one_arm(
            arm,
            &shared,
            options,
            &mut tables,
            &mut writer,
        )?);
    }

    let lanes = assemble_the_lanes(options, &tables, &arm_facts);
    let judgements = judge(&lanes, &baseline_id, options.stats_seed);

    let facts = RunFacts {
        run_id: run_id.clone(),
        git_commit,
        git_dirty,
    };
    let mut provenance = provenance_of(&facts, options, &arms, &lead_header);
    if let Some(w) = writer.take() {
        let records = w.written();
        let manifest = manifest_of(&facts, options, &arms, &shared, &lead_header, &lead_cache);
        let dir = w.finish(&manifest)?;
        runs::note_run_files(&mut provenance, records, &dir);
    } else {
        provenance.insert("per-query records".into(), "**not written**".into());
    }

    Ok(EmbeddingCard {
        run_id,
        generated_at_unix: runs::now_unix(),
        corpus_sha256: lead_header.corpus_sha256.clone(),
        // The slice that was graded, not the size of the cache. With `--limit`
        // set they differ, and a card that prints the cache's count beside a
        // document count taken from the slice is two scopes in one sentence.
        corpus_chunks: shared.graded,
        corpus_documents: shared.documents,
        query_seed_digest: lead_header.query_seed_digest.clone(),
        baseline: baseline_id,
        arms: arm_facts,
        lanes,
        judgements,
        query_counts: shared.families.counts,
        stats_seed: options.stats_seed,
        ranking_threshold: RANKING_THRESHOLD,
        composite_declared: HEADLINE.join(" + "),
        provenance,
        caveats: caveats(),
    })
}

/// What a run records about itself, for the manifest and the provenance table.
struct RunFacts {
    /// The run directory's name.
    run_id: String,
    /// The commit the harness was built from.
    git_commit: String,
    /// Whether that commit had uncommitted changes beside it.
    git_dirty: bool,
}

/// The model every other model is compared against.
///
/// The first arm by default, and `--baseline` has to name an arm that is in the
/// run: a baseline that is not among the arms would leave every comparison
/// without a left-hand side.
///
/// @param options - the run's own flags
/// @param arms - every arm in the run
/// @param lead_header - the first arm's cache header
fn baseline_named_by(
    options: &EmbeddingGradeOptions,
    arms: &[Arm],
    lead_header: &CacheHeader,
) -> Result<String> {
    match &options.baseline {
        Some(id) => {
            anyhow::ensure!(
                arms.iter().any(|a| &a.header.model_id == id),
                "--baseline names {id}, which is not among the arms: {}",
                arms.iter()
                    .map(|a| a.header.model_id.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            );
            Ok(id.clone())
        }
        None => Ok(lead_header.model_id.clone()),
    }
}

/// The query set and the samples every arm shares, read from the first cache.
///
/// @param options - the run's own flags
/// @param lead_cache - the first arm's cache
fn read_the_shared_corpus(options: &EmbeddingGradeOptions, lead_cache: &Path) -> Result<Shared> {
    eprintln!("loading {} to generate the query set", lead_cache.display());
    let first = corpus::load_cache(lead_cache)?;
    let graded = options.limit.unwrap_or(first.len()).min(first.len());
    let keys = corpus_keys(&first);
    let families = build_families(&first, &keys, options.per_source, graded);
    let documents = first
        .chunks
        .iter()
        .take(graded)
        .map(|c| c.external_doc_id.as_str())
        .collect::<HashSet<_>>()
        .len();
    eprintln!(
        "  {} queries across {} families",
        families.counts.values().sum::<usize>(),
        families.counts.len()
    );
    let cost_repeats = options.cost_repeats.max(1);
    let cost_texts: Vec<String> =
        scenarios::strided_sample(graded, options.cost_samples * cost_repeats)
            .into_iter()
            .filter_map(|i| first.chunks.get(i))
            .map(|c| crate::synth::sanitize_for_model(&c.content))
            .collect();
    let matryoshka_rows = scenarios::strided_sample(graded, options.matryoshka_chunks);
    Ok(Shared {
        graded,
        keys,
        families,
        documents,
        cost_texts,
        cost_repeats,
        matryoshka_rows,
    })
}

/// Grades one arm through every lane the run asked for.
///
/// @param arm - the arm being graded
/// @param shared - the corpus slice, query set and samples every arm shares
/// @param options - the run's own flags
/// @param tables - where each lane's scores are collected
/// @param writer - the run directory, when one could be opened
fn score_one_arm(
    arm: &Arm,
    shared: &Shared,
    options: &EmbeddingGradeOptions,
    tables: &mut Tables,
    writer: &mut Option<RunWriter>,
) -> Result<ArmFacts> {
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
    eprintln!(
        "  loaded {} chunks in {:.1}s",
        corpus.len(),
        started.elapsed().as_secs_f64()
    );

    // Queries, embedded by this arm's own model with this arm's own prefixes - or read
    // from a sidecar that model produced, when the harness cannot open it at all.
    let (embedder, supplied) = open_query_source(arm, &shared.families, options)?;
    let mut source = QuerySource { embedder, supplied };
    let mut query_vectors: Vec<Vec<Vec<f32>>> = Vec::new();
    for (name, qs) in &shared.families.named {
        query_vectors.push(embed_one_family(&mut source, qs, name, &id)?);
    }
    eprintln!(
        "  {} queries for {id}",
        query_vectors.iter().map(|v| v.len()).sum::<usize>()
    );

    let mut facts = arm_facts_of(arm);
    if options.dense {
        run_dense_lane(&id, &corpus, shared, &query_vectors, tables, writer)?;
    }
    if options.hybrid {
        run_hybrid_lane(
            &id,
            &corpus,
            shared,
            &query_vectors,
            options,
            tables,
            writer,
        )?;
    }
    if options.matryoshka {
        run_matryoshka_lane(&id, arm, &corpus, shared, &query_vectors, tables)?;
    }
    if options.abstention {
        run_abstention_lane(&id, &corpus, shared, &mut source, tables)?;
    }
    if options.cost {
        time_arm(
            arm,
            &shared.cost_texts,
            shared.cost_repeats,
            options,
            &mut facts,
        );
    }
    Ok(facts)
}

/// One family's query vectors, from the arm's own model or from its sidecar.
///
/// @param source - where this arm's vectors come from
/// @param queries - the family to embed
/// @param name - the family's name, for the error if embedding fails
/// @param id - the model's identifier, for the same error
fn embed_one_family(
    source: &mut QuerySource,
    queries: &[GradedQuery],
    name: &str,
    id: &str,
) -> Result<Vec<Vec<f32>>> {
    if let Some(rows) = source.supplied.as_mut() {
        return Ok(rows.drain(..queries.len()).collect());
    }
    let texts: Vec<String> = queries.iter().map(|q| q.text.clone()).collect();
    queryset::embed_with(
        source
            .embedder
            .as_ref()
            .context("an arm with no sidecar must have opened its model to embed with")?,
        &texts,
    )
    .with_context(|| format!("embedding the {name} family with {id}"))
}

/// What the card records about one arm, before any lane has run.
///
/// @param arm - the arm being graded
fn arm_facts_of(arm: &Arm) -> ArmFacts {
    ArmFacts {
        model_id: arm.header.model_id.clone(),
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
    }
}

/// Every graded family answered by exhaustive cosine: the embedding on its own.
///
/// **The same slice the queries were generated from**, which is also the slice
/// the hybrid lane indexes. Searching the whole cache while grading against
/// ground truth drawn from a prefix of it would let a query be answered by a
/// chunk no judgement covers, and every `--limit` run would quietly score lower
/// for a reason unrelated to any model.
///
/// No per-document cap: this lane is the ranking the model produces, with no
/// policy on top. The ideal is computed the same way for every arm, so the
/// absolute value is a little pessimistic and the comparison is exact.
///
/// @param id - the model's identifier, which is its column on the card
/// @param corpus - this arm's loaded cache
/// @param shared - the corpus slice, query set and samples every arm shares
/// @param query_vectors - this arm's own query vectors, one list per family
/// @param tables - where this lane's scores are collected
/// @param writer - the run directory, when one could be opened
fn run_dense_lane(
    id: &str,
    corpus: &Corpus,
    shared: &Shared,
    query_vectors: &[Vec<Vec<f32>>],
    tables: &mut Tables,
    writer: &mut Option<RunWriter>,
) -> Result<()> {
    let vectors = corpus.vectors.get(..shared.graded).with_context(|| {
        format!(
            "the cache holds {} vectors, not {}",
            corpus.vectors.len(),
            shared.graded
        )
    })?;
    eprintln!(
        "  dense lane: exhaustive cosine over {} chunks",
        vectors.len()
    );
    let keys = &shared.keys;
    for ((name, qs), qvs) in shared.families.named.iter().zip(query_vectors) {
        let started = Instant::now();
        let mut search = |_q: &GradedQuery, index: usize| -> Result<Vec<Hit>> {
            let query = query_at(qvs, index)?;
            exhaustive_top_k(vectors, query, K)
                .into_iter()
                .map(|i| {
                    let vector = vectors
                        .get(i)
                        .context("the ranking named a chunk the slice does not hold")?;
                    let score = dot(vector, query);
                    Ok(Hit {
                        key: keys
                            .get(i)
                            .context("the ranking named a chunk with no key")?
                            .clone(),
                        score,
                        confidence: score.clamp(0.0, 1.0),
                    })
                })
                .collect()
        };
        let scored = score_family(id, "dense", qs, &mut search, K, writer)?;
        eprintln!(
            "    {name}: {} queries in {:.1}s, nDCG@10 {:.4}",
            qs.len(),
            started.elapsed().as_secs_f64(),
            mean(scored.get(M_NDCG))
        );
        for (metric, values) in scored {
            tables
                .dense
                .entry(name.clone())
                .or_default()
                .entry(metric)
                .or_default()
                .insert(id.to_string(), values);
        }
    }
    Ok(())
}

/// The same families through the real pipeline, with the shipped settings.
///
/// **Every ranking setting is left exactly as `build_index` built it**, which
/// is `IndexConfig::default()` - the fusion, the lexical coverage, the phrase
/// weight, the adaptive weighting, all of it. Copying those values into setters
/// here would say the same thing in a second place, and a second place is
/// somewhere the two can drift: the day a default moves, this lane would keep
/// grading against the old policy and the card would still call itself "the
/// shipped pipeline". The one thing set is the traversal width on a filtered
/// query, which is a harness decision rather than an engine default and is the
/// same number `grade` uses.
///
/// @param id - the model's identifier, which is its column on the card
/// @param corpus - this arm's loaded cache
/// @param shared - the corpus slice, query set and samples every arm shares
/// @param query_vectors - this arm's own query vectors, one list per family
/// @param options - the run's own flags
/// @param tables - where this lane's scores are collected
/// @param writer - the run directory, when one could be opened
fn run_hybrid_lane(
    id: &str,
    corpus: &Corpus,
    shared: &Shared,
    query_vectors: &[Vec<Vec<f32>>],
    options: &EmbeddingGradeOptions,
    tables: &mut Tables,
    writer: &mut Option<RunWriter>,
) -> Result<()> {
    eprintln!("  hybrid lane: building the index");
    let (index, index_keys, stats, seconds) = scenarios::build_index(corpus, options.limit, true)?;
    eprintln!(
        "    built {} chunks / {} documents in {seconds:.1}s",
        stats.chunks, stats.documents
    );
    let per_doc_cap = index.config().per_doc_cap;
    let mut engine = InillucentEngine::new(index, index_keys, id.to_string(), Some(128));
    engine.filtered_ef_search = Some(FILTERED_EF_SEARCH);
    let filter = Filter::default();

    if options.lexical && tables.lexical.is_empty() {
        score_bm25_alone(&mut engine, shared, per_doc_cap, tables, writer)?;
    }

    for ((name, qs), qvs) in shared.families.named.iter().zip(query_vectors) {
        let started = Instant::now();
        let mut search = |q: &GradedQuery, index: usize| -> Result<Vec<Hit>> {
            engine.hybrid_search(&q.text, query_at(qvs, index)?, &filter, K)
        };
        let scored = score_family(id, "hybrid", qs, &mut search, per_doc_cap, writer)?;
        eprintln!(
            "    {name}: {} queries in {:.1}s, nDCG@10 {:.4}",
            qs.len(),
            started.elapsed().as_secs_f64(),
            mean(scored.get(M_NDCG))
        );
        for (metric, values) in scored {
            tables
                .hybrid
                .entry(name.clone())
                .or_default()
                .entry(metric)
                .or_default()
                .insert(id.to_string(), values);
        }
    }
    Ok(())
}

/// BM25 with no model at all, scored on the first arm only.
///
/// The lexical ranking reads the corpus text and the query and nothing else,
/// and both are the same for every arm, so the later arms would print the same
/// numbers.
///
/// @param engine - the index the first arm built
/// @param shared - the corpus slice, query set and samples every arm shares
/// @param per_doc_cap - the per-document result cap the index was built with
/// @param tables - where this lane's scores are collected
/// @param writer - the run directory, when one could be opened
fn score_bm25_alone(
    engine: &mut InillucentEngine,
    shared: &Shared,
    per_doc_cap: usize,
    tables: &mut Tables,
    writer: &mut Option<RunWriter>,
) -> Result<()> {
    for (name, qs) in shared.families.named.iter() {
        let started = Instant::now();
        let mut search = |q: &GradedQuery, _index: usize| -> Result<Vec<Hit>> {
            engine.lexical_search(&q.text, &Filter::default(), K)
        };
        let scored = score_family(
            LEXICAL_COLUMN,
            "lexical",
            qs,
            &mut search,
            per_doc_cap,
            writer,
        )?;
        eprintln!(
            "    BM25 alone, {name}: {} queries in {:.1}s, nDCG@10 {:.4}",
            qs.len(),
            started.elapsed().as_secs_f64(),
            mean(scored.get(M_NDCG))
        );
        for (metric, values) in scored {
            tables
                .lexical
                .entry(name.clone())
                .or_default()
                .insert(metric, values);
        }
    }
    Ok(())
}

/// How much of the full-width ranking survives at each narrower width.
///
/// **Against its own full width rather than against another model's**, because
/// the question is what narrowing costs this model. The full-width answer is
/// computed once: it is the same reference for every width, and recomputing it
/// inside the width loop was doing a 25,000-vector exhaustive pass three extra
/// times per query for a result that could not change.
///
/// The queries are the identity family, which is the one family whose ground
/// truth is a whole document and therefore the one where a narrowed ranking has
/// the most room to go wrong.
///
/// @param id - the model's identifier, which is its column on the card
/// @param arm - the arm being graded, for the widths its manifest declares
/// @param corpus - this arm's loaded cache
/// @param shared - the corpus slice, query set and samples every arm shares
/// @param query_vectors - this arm's own query vectors, one list per family
/// @param tables - where this lane's scores are collected
fn run_matryoshka_lane(
    id: &str,
    arm: &Arm,
    corpus: &Corpus,
    shared: &Shared,
    query_vectors: &[Vec<Vec<f32>>],
    tables: &mut Tables,
) -> Result<()> {
    let widths = arm.model.manifest.widths();
    eprintln!(
        "  Matryoshka lane over {} chunks at {:?}",
        shared.matryoshka_rows.len(),
        widths
    );
    let sample: Vec<&Vec<f32>> = shared
        .matryoshka_rows
        .iter()
        .filter_map(|&i| corpus.vectors.get(i))
        .collect();
    let probe = query_vectors
        .first()
        .context("no family was embedded, so there is nothing to narrow")?;
    let references: Vec<HashSet<usize>> = probe
        .iter()
        .map(|q| top_k_of(&sample, q, K).into_iter().collect())
        .collect();
    for width in &widths {
        if *width == corpus.dims {
            tables
                .mrl
                .entry(id.to_string())
                .or_default()
                .insert(width.to_string(), 1.0);
            continue;
        }
        let narrowed: Vec<Vec<f32>> = sample
            .iter()
            .map(|v| truncate_normalized(v, *width))
            .collect();
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
        tables
            .mrl
            .entry(id.to_string())
            .or_default()
            .insert(width.to_string(), value);
    }
    Ok(())
}

/// How confident each model is on questions nothing in the corpus answers.
///
/// **Dense cosine rather than the engine's fused score, and deliberately.**
/// Fusion normalises out of the candidate list, so the top hit of every query
/// maps to the top of the scale whether the list is good or hopeless and no
/// threshold on it exists. A cosine is computed against a bound the results had
/// no say in, so one query's value means the same as the next one's - which is
/// the whole premise of a threshold.
///
/// @param id - the model's identifier, which is its column on the card
/// @param corpus - this arm's loaded cache
/// @param shared - the corpus slice, query set and samples every arm shares
/// @param source - where this arm's query vectors come from
/// @param tables - where this lane's scores are collected
fn run_abstention_lane(
    id: &str,
    corpus: &Corpus,
    shared: &Shared,
    source: &mut QuerySource,
    tables: &mut Tables,
) -> Result<()> {
    for (name, qs) in &shared.families.abstention {
        let vectors = embed_one_family(source, qs, name, id)?;
        let vectors_slice = corpus.vectors.get(..shared.graded).with_context(|| {
            format!(
                "the cache holds {} vectors, not {}",
                corpus.vectors.len(),
                shared.graded
            )
        })?;
        let tops: Vec<f64> = vectors
            .iter()
            .map(|q| {
                exhaustive_top_k(vectors_slice, q, 1)
                    .first()
                    .and_then(|i| vectors_slice.get(*i))
                    .map(|v| f64::from(dot(v, q).clamp(0.0, 1.0)))
                    .unwrap_or(0.0)
            })
            .collect();
        eprintln!(
            "  abstention lane, {name}: {} queries, mean top confidence {:.4}",
            tops.len(),
            if tops.is_empty() {
                0.0
            } else {
                tops.iter().sum::<f64>() / tops.len() as f64
            }
        );
        tables
            .confidences
            .entry(id.to_string())
            .or_default()
            .insert(name.clone(), tops);
    }
    Ok(())
}

/// Every lane the run asked for, in the order the card prints them.
///
/// The two composites go first, on both the declared families and the promoted
/// set, so the promotion is a visible decision rather than a quiet re-aim.
///
/// @param options - the run's own flags
/// @param tables - every lane's collected scores
/// @param arm_facts - what the card records about each arm
fn assemble_the_lanes(
    options: &EmbeddingGradeOptions,
    tables: &Tables,
    arm_facts: &[ArmFacts],
) -> Vec<Lane> {
    let mut lanes = Vec::new();
    if options.dense {
        lanes.push(lane_from(
            "dense only, exhaustive cosine",
            "Every graded family answered by exhaustive cosine over the whole corpus: no graph, \
             no lexical side, no fusion. This is the embedding on its own. The index reaches \
             0.925 recall against exhaustive cosine on this corpus, and letting 7.5% of the \
             answer move for reasons unrelated to the model would be larger than the effect \
             being measured.",
            &tables.dense,
        ));
    }
    if options.hybrid {
        lanes.push(lane_from(
            "hybrid, the shipped pipeline",
            "The same families through the real pipeline with the shipped fusion and ranking \
             settings, applied identically to every arm. A model that wins in isolation and \
             loses once BM25 is fused beside it has not helped an agent, and this is the lane \
             that says so.",
            &tables.hybrid,
        ));
    }
    if options.matryoshka && !tables.mrl.is_empty() {
        lanes.push(matryoshka_lane(&tables.mrl, arm_facts));
    }
    if options.abstention {
        lanes.push(abstention_lane(&tables.confidences));
    }
    if options.lexical && !tables.lexical.is_empty() {
        lanes.push(lexical_lane(&tables.lexical));
    }
    if options.cost {
        lanes.push(cost_lane(arm_facts));
    }

    if options.dense {
        lanes.insert(0, composite_lane(&tables.dense, "dense composite"));
    }
    if options.hybrid {
        lanes.insert(
            if options.dense { 1 } else { 0 },
            composite_lane(&tables.hybrid, "hybrid composite"),
        );
    }
    lanes
}

/// The provenance table the card prints.
///
/// @param facts - the run identifier and the commit it was built from
/// @param options - the run's own flags
/// @param arms - every arm in the run, for the cache each one read
/// @param lead_header - the first arm's cache header
fn provenance_of(
    facts: &RunFacts,
    options: &EmbeddingGradeOptions,
    arms: &[Arm],
    lead_header: &CacheHeader,
) -> BTreeMap<String, String> {
    let mut provenance: BTreeMap<String, String> = BTreeMap::new();
    provenance.insert("run id".into(), facts.run_id.clone());
    provenance.insert(
        "commit".into(),
        format!(
            "{}{}",
            facts.git_commit,
            if facts.git_dirty {
                " (working tree dirty)"
            } else {
                ""
            }
        ),
    );
    provenance.insert("command".into(), runs::command_line());
    provenance.insert("corpus digest".into(), lead_header.corpus_sha256.clone());
    provenance.insert(
        "query seed digest".into(),
        lead_header.query_seed_digest.clone(),
    );
    provenance.insert(
        "query seeds".into(),
        scenarios::seeds()
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join(", "),
    );
    provenance.insert("statistics seed".into(), options.stats_seed.to_string());
    provenance.insert(
        "query embedding device".into(),
        format!("{:?}", options.device),
    );
    provenance.insert(
        "host".into(),
        format!(
            "{} {}, {} logical processors",
            std::env::consts::OS,
            std::env::consts::ARCH,
            std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(0)
        ),
    );
    for arm in arms {
        provenance.insert(
            format!("cache: {}", arm.header.model_id),
            arm.cache.display().to_string(),
        );
    }
    provenance
}

/// The run manifest: what this run was, in the form another run can be compared
/// against.
///
/// @param facts - the run identifier and the commit it was built from
/// @param options - the run's own flags
/// @param arms - every arm in the run
/// @param shared - the corpus slice and query counts every arm shared
/// @param lead_header - the first arm's cache header
/// @param lead_cache - the first arm's cache
fn manifest_of(
    facts: &RunFacts,
    options: &EmbeddingGradeOptions,
    arms: &[Arm],
    shared: &Shared,
    lead_header: &CacheHeader,
    lead_cache: &Path,
) -> runs::RunManifest {
    runs::RunManifest {
        run_id: facts.run_id.clone(),
        generated_at_unix: runs::now_unix(),
        git_commit: facts.git_commit.clone(),
        git_dirty: facts.git_dirty,
        command: runs::command_line(),
        corpus: runs::CorpusFacts {
            chunks: lead_header.chunk_count,
            documents: shared.documents,
            dimensions: lead_header.dims,
            cache_path: lead_cache.display().to_string(),
            cache_bytes: std::fs::metadata(lead_cache).map(|m| m.len()).unwrap_or(0),
            cache_modified_unix: 0,
        },
        model_dir: arms
            .iter()
            .map(|a| a.model.dir.display().to_string())
            .collect::<Vec<_>>()
            .join(", "),
        model_file: arms
            .iter()
            .map(|a| a.model.manifest.model_file.clone())
            .collect::<Vec<_>>()
            .join(", "),
        model_id: arms
            .iter()
            .map(|a| a.header.model_id.clone())
            .collect::<Vec<_>>()
            .join(", "),
        model_manifest_sha256: arms
            .iter()
            .map(|a| short(&a.header.manifest_sha256))
            .collect::<Vec<_>>()
            .join(", "),
        model_dims: lead_header.dims,
        model_max_tokens: lead_header.max_tokens,
        cache_header: lead_header.clone(),
        device: format!("{:?}", options.device),
        database: "not used: grade-embedding compares models, not engines".into(),
        seeds: scenarios::seeds(),
        arm: BTreeMap::new(),
        host: runs::host_facts(),
        query_counts: shared.families.counts.clone(),
        practical_thresholds: [("ranking measures".to_string(), RANKING_THRESHOLD)]
            .into_iter()
            .collect(),
    }
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
/// A safetensors checkpoint has the same problem in a different shape: its weights are
/// several numbered shards and the file the manifest names is a JSON index listing them.
/// `Qwen3-Embedding-8B` is 30 KB of index in front of 15.1 GB of shards, so the same
/// mistake would report the heaviest arm on a board as the lightest by six orders of
/// magnitude. When the named file is an index, the shards it names are summed.
/// @param dir - the model directory
/// @param model_file - the graph file named by the manifest
fn weights_bytes(dir: &Path, model_file: &str) -> u64 {
    if model_file.ends_with(".index.json") {
        if let Some(total) = sharded_weights_bytes(&dir.join(model_file)) {
            return total;
        }
    }
    let mut total = std::fs::metadata(dir.join(model_file))
        .map(|m| m.len())
        .unwrap_or(0);
    let Ok(entries) = std::fs::read_dir(dir) else {
        return total;
    };
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

/// Sum the distinct shards a safetensors index names, or nothing if it cannot be read.
///
/// Distinct, because the index maps every tensor name to its shard and a checkpoint has
/// far more tensors than shards; counting per entry would multiply the real size by the
/// tensor count.
/// @param index - the `model.safetensors.index.json` path
fn sharded_weights_bytes(index: &Path) -> Option<u64> {
    let text = std::fs::read_to_string(index).ok()?;
    let parsed: serde_json::Value = serde_json::from_str(&text).ok()?;
    let map = parsed.get("weight_map")?.as_object()?;
    let dir = index.parent()?;
    let shards: std::collections::BTreeSet<&str> =
        map.values().filter_map(|v| v.as_str()).collect();
    let mut total = std::fs::metadata(index).map(|m| m.len()).unwrap_or(0);
    for shard in shards {
        total += std::fs::metadata(dir.join(shard))
            .map(|m| m.len())
            .unwrap_or(0);
    }
    Some(total)
}

/// Top k by cosine over a borrowed sample, returning positions within it.
fn top_k_of(vectors: &[&Vec<f32>], query: &[f32], k: usize) -> Vec<usize> {
    let mut scored: Vec<(f32, usize)> = vectors
        .iter()
        .enumerate()
        .map(|(i, v)| (dot(v, query), i))
        .collect();
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
    let middle = sorted.get(sorted.len() / 2).copied().unwrap_or(0.0);
    match sorted.len() {
        0 => 0.0,
        n if n % 2 == 1 => middle,
        n => {
            let below = sorted
                .get((n / 2).saturating_sub(1))
                .copied()
                .unwrap_or(middle);
            0.5 * (below + middle)
        }
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
        &crate::arm::ArmOptions {
            batch_size: 16,
            device,
            ..options.clone()
        },
    )?;
    let repeats = repeats.max(1);
    // Checked before the warm-up, so a sample too small to split fails without
    // having spent a forward pass on it.
    timing_range(texts.len(), repeats, 0)?;
    // One warm-up batch, outside every timing, so the numbers describe
    // steady-state throughput rather than the first allocation of the arena.
    let warm = texts.get(..texts.len().min(16)).unwrap_or(texts);
    embedder.embed_documents(warm)?;
    let before = embedder.truncation();

    let mut rates = Vec::with_capacity(repeats);
    for pass in 0..repeats {
        // Disjoint: this pass gets chunks no earlier pass has shown the model.
        let range = timing_range(texts.len(), repeats, pass)?;
        let batch = texts
            .get(range.clone())
            .with_context(|| format!("pass {pass} reads {range:?} of {}", texts.len()))?;
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

#[cfg(test)]
mod tests {
    use super::*;
    // `percentile` is tested here and called only from inside `card`, so the
    // parent does not import it and this is the one place that names it.
    use super::card::percentile;
    use inillucent_core::model::{ModelManifest, Pooling, Prefixes};

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "inillucent-gradeembed-{}-{name}",
            std::process::id()
        ));
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
        corpus::save_cache(
            &Corpus {
                chunks: inputs,
                vectors,
                dims,
                header,
            },
            &path,
        )
        .unwrap();
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
            query_vectors: BTreeMap::new(),
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
            lexical
                .entry(family.to_string())
                .or_default()
                .insert(M_NDCG.to_string(), scores);
        }
        let lane = lexical_lane(&lexical);

        let mut columns: Vec<&String> = lane.rows.iter().flat_map(|r| r.values.keys()).collect();
        columns.sort();
        columns.dedup();
        assert_eq!(
            columns.len(),
            1,
            "one column, because no model changes this ranking"
        );
        assert_eq!(columns[0], LEXICAL_COLUMN);

        assert!(
            lane.rows
                .iter()
                .all(|r| r.role == Role::Diagnostic && r.series.is_empty()),
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
        let err = resolve_arms(&options(&root, vec![a, b]))
            .unwrap_err()
            .to_string();
        assert!(err.contains("two different"), "{err}");
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn two_caches_of_different_chunk_counts_are_refused_by_name() {
        let root = scratch("counts");
        let a = arm_at(&root, "model-a", 8, 40, "corpus-one", &seeds_now());
        let b = arm_at(&root, "model-b", 8, 36, "corpus-one", &seeds_now());
        let err = resolve_arms(&options(&root, vec![a, b]))
            .unwrap_err()
            .to_string();
        assert!(err.contains("chunks and"), "{err}");
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn two_caches_written_under_different_query_seeds_are_refused_by_name() {
        let root = scratch("seeds");
        let a = arm_at(&root, "model-a", 8, 40, "corpus-one", &seeds_now());
        let b = arm_at(
            &root,
            "model-b",
            8,
            40,
            "corpus-one",
            "a-different-seed-table",
        );
        let err = resolve_arms(&options(&root, vec![a, b]))
            .unwrap_err()
            .to_string();
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
        let err = resolve_arms(&options(&root, vec![a, b]))
            .unwrap_err()
            .to_string();
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
        let err = resolve_arms(&options(&root, vec![a, b]))
            .unwrap_err()
            .to_string();
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
        let err = resolve_arms(&options(&root, vec![a, b]))
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("has changed since these vectors were made"),
            "{err}"
        );
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
        corpus::save_cache(
            &Corpus {
                chunks: inputs,
                vectors,
                dims: 8,
                header,
            },
            &b_path,
        )
        .unwrap();
        let err = resolve_arms(&options(&root, vec![a, b_path]))
            .unwrap_err()
            .to_string();
        assert!(err.contains("cannot say which corpus"), "{err}");
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn one_cache_is_not_a_comparison() {
        let root = scratch("single");
        let a = arm_at(&root, "model-a", 8, 40, "corpus-one", &seeds_now());
        let err = resolve_arms(&options(&root, vec![a]))
            .unwrap_err()
            .to_string();
        assert!(err.contains("at least two caches"), "{err}");
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn the_same_model_twice_is_not_a_comparison() {
        let root = scratch("dup");
        let a = arm_at(&root, "model-a", 8, 40, "corpus-one", &seeds_now());
        let copy = root.join("model-a-again.cache");
        std::fs::copy(&a, &copy).unwrap();
        let err = resolve_arms(&options(&root, vec![a, copy]))
            .unwrap_err()
            .to_string();
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

    /// `rejudge` has to re-take the role decision, not only re-run the statistics.
    ///
    /// Written after running it on the Phase 0 card, which was produced by a binary that
    /// marked the promoted composite diagnostic: `rejudge` reported 60 judgements before and
    /// 60 after, because it judged the roles stored in the file. A `rejudge` that cannot
    /// change a role cannot repair a card, and the card it exists for costs nine hours of
    /// embedding to earn again.
    #[test]
    fn rejudge_promotes_a_row_an_older_binary_left_diagnostic() {
        let mut scores: LaneScores = BTreeMap::new();
        for family in PROMOTED {
            let mut per_metric: BTreeMap<String, BTreeMap<String, Vec<f64>>> = BTreeMap::new();
            let mut per_model: BTreeMap<String, Vec<f64>> = BTreeMap::new();
            per_model.insert(
                "base".to_string(),
                (0..40).map(|i| 0.5 + i as f64 * 0.001).collect(),
            );
            per_model.insert(
                "candidate".to_string(),
                (0..40).map(|i| 0.6 + i as f64 * 0.001).collect(),
            );
            per_metric.insert(M_NDCG.to_string(), per_model);
            scores.insert((*family).to_string(), per_metric);
        }
        let mut lane = composite_lane(&scores, "dense composite");
        // The card an older binary wrote: the row is there, measured, and unjudgeable.
        for row in &mut lane.rows {
            if row.family.contains("promoted") {
                row.role = Role::Diagnostic;
            }
        }
        let mut card = EmbeddingCard {
            run_id: "rejudge-role".to_string(),
            generated_at_unix: 0,
            corpus_sha256: String::new(),
            corpus_chunks: 0,
            corpus_documents: 0,
            query_seed_digest: String::new(),
            baseline: "base".to_string(),
            arms: Vec::new(),
            lanes: vec![lane],
            judgements: Vec::new(),
            query_counts: BTreeMap::new(),
            stats_seed: 20260901,
            ranking_threshold: RANKING_THRESHOLD,
            composite_declared: HEADLINE.join(" + "),
            provenance: BTreeMap::new(),
            caveats: Vec::new(),
        };
        card.judgements = judge(&card.lanes, &card.baseline, 20260901);
        let stale = card.judgements.len();

        let dir = std::env::temp_dir().join("inillucent-rejudge-role");
        std::fs::create_dir_all(&dir).expect("a temp directory");
        let path = dir.join("card.json");
        std::fs::write(
            &path,
            serde_json::to_string(&card).expect("a card serialises"),
        )
        .expect("writing the card");

        let (rescored, before, after) = rejudge(&path).expect("rejudging the card");
        assert_eq!(
            before, stale,
            "the stored judgement count is what `before` reports"
        );
        assert!(
            after > before,
            "re-taking the role decision must produce judgements the stored card had none of: \
             {before} -> {after}"
        );
        let promoted = rescored
            .lanes
            .iter()
            .flat_map(|l| l.rows.iter())
            .find(|r| r.family.contains("promoted"))
            .expect("the promoted composite is on the card");
        assert_eq!(
            promoted.role,
            Role::Primary,
            "the promoted row must come back primary"
        );
        assert!(
            rescored
                .judgements
                .iter()
                .any(|j| j.family.contains("promoted")),
            "the promoted composite must have a verdict after a rejudge"
        );
    }

    /// The promoted six family composite has to carry a verdict, because the gates that
    /// matter are declared on it. While it was diagnostic it had values and a per-query
    /// series and no verdict at all - and a row nobody can get a verdict for is a row
    /// that gets read as the row beside it, which is how three of task-1818's runs
    /// earned "better" on the declared two family composite while passage evidence
    /// regressed by up to 0.064 underneath.
    #[test]
    fn both_composites_are_judged() {
        let mut scores: LaneScores = BTreeMap::new();
        for family in PROMOTED {
            let mut per_metric: BTreeMap<String, BTreeMap<String, Vec<f64>>> = BTreeMap::new();
            let mut per_model: BTreeMap<String, Vec<f64>> = BTreeMap::new();
            per_model.insert("base".to_string(), vec![0.5; 40]);
            per_model.insert("candidate".to_string(), vec![0.6; 40]);
            per_metric.insert(M_NDCG.to_string(), per_model);
            scores.insert((*family).to_string(), per_metric);
        }
        let lane = composite_lane(&scores, "dense composite");
        let promoted = lane
            .rows
            .iter()
            .find(|r| r.family.contains("promoted"))
            .expect("the promoted composite is on the card");
        assert_eq!(
            promoted.role,
            Role::Primary,
            "the promoted composite must be judged"
        );
        assert!(
            !promoted.series.is_empty(),
            "a judged row needs its per-query series"
        );

        let judgements = judge(&[lane], "base", 20260901);
        let families: Vec<&str> = judgements.iter().map(|j| j.family.as_str()).collect();
        assert!(
            families.iter().any(|f| f.contains("promoted")),
            "the promoted composite produced no judgement: {families:?}"
        );
        assert!(
            families.iter().any(|f| f.contains("declared")),
            "judging the promoted one must not have cost the declared one its verdict: {families:?}"
        );
    }

    /// A safetensors checkpoint names its weights from a JSON index, and the index is
    /// tiny. `Qwen3-Embedding-8B` is 30 KB of index in front of four shards totalling
    /// 15.1 GB, so reading the named file alone would put the heaviest arm on the board
    /// under every footprint budget there is - the same reporting error that put
    /// `qwen3-embedding-0.6b` inside task-1818's gate G8 at 307 MB against a real 2,401.
    #[test]
    fn a_sharded_checkpoint_is_weighed_by_its_shards_and_not_its_index() {
        let dir = sidecar_dir("shards");
        std::fs::write(
            dir.join("model-00001-of-00002.safetensors"),
            vec![0u8; 4096],
        )
        .unwrap();
        std::fs::write(
            dir.join("model-00002-of-00002.safetensors"),
            vec![0u8; 2048],
        )
        .unwrap();
        // Many tensors, two shards: the sum is over the shards, not over the entries.
        let index = serde_json::json!({"weight_map": {
            "a": "model-00001-of-00002.safetensors",
            "b": "model-00001-of-00002.safetensors",
            "c": "model-00002-of-00002.safetensors",
        }});
        let index_path = dir.join("model.safetensors.index.json");
        std::fs::write(&index_path, index.to_string()).unwrap();
        let index_bytes = std::fs::metadata(&index_path).unwrap().len();
        assert_eq!(
            weights_bytes(&dir, "model.safetensors.index.json"),
            4096 + 2048 + index_bytes
        );
    }

    /// A scratch directory for the sidecar tests, named by process so two runs cannot
    /// read each other's files.
    fn sidecar_dir(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("inillucent-sidecar-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Write a sidecar pair: the vectors and the JSON that claims what they are.
    fn write_sidecar(dir: &Path, meta: serde_json::Value, rows: usize, dims: usize) -> PathBuf {
        let vectors = dir.join("q.f32");
        let mut bytes = Vec::with_capacity(rows * dims * 4);
        for row in 0..rows {
            for d in 0..dims {
                bytes.extend_from_slice(&((row * dims + d) as f32).to_le_bytes());
            }
        }
        std::fs::write(&vectors, bytes).unwrap();
        std::fs::write(vectors.with_extension("meta.json"), meta.to_string()).unwrap();
        vectors
    }

    fn sidecar_header() -> CacheHeader {
        CacheHeader {
            version: 4,
            corpus_sha256: "corpus-a".into(),
            model_id: "teacher".into(),
            manifest_sha256: "d".into(),
            dims: 4,
            max_tokens: 512,
            chunk_count: 10,
            truncated_chunks: 0,
            query_seed_digest: "seeds-a".into(),
        }
    }

    fn sidecar_meta() -> serde_json::Value {
        serde_json::json!({
            "corpus_sha256": "corpus-a",
            "query_seed_digest": "seeds-a",
            "per_source": 40,
            "queries": 3,
            "model_id": "teacher",
            "dims": 4,
        })
    }

    /// The whole point of the sidecar is that vectors can come from a model this
    /// harness cannot open. A matching one loads and keeps its row order, because the
    /// families are split off the front of it in the order `query-texts` wrote them.
    #[test]
    fn a_matching_query_vector_sidecar_loads_in_order() {
        let dir = sidecar_dir("match");
        let path = write_sidecar(&dir, sidecar_meta(), 3, 4);
        let rows = read_query_vectors(&path, "teacher", 4, &sidecar_header(), 40, 3).unwrap();
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0], vec![0.0, 1.0, 2.0, 3.0]);
        assert_eq!(rows[2], vec![8.0, 9.0, 10.0, 11.0]);
    }

    /// Each of these produces a file that opens, parses and yields numbers. That is
    /// exactly why every one has to be refused by name: a sidecar built against last
    /// week's corpus, or at a different `--per-source`, or for a different model, is
    /// not a bad file, it is a plausible one, and it would put a wrong number on a
    /// gate card with nothing in the output to say so.
    #[test]
    fn a_sidecar_that_does_not_describe_this_run_is_refused_by_name() {
        let header = sidecar_header();
        let cases: Vec<(&str, serde_json::Value, &str, usize, usize, usize, &str)> = vec![
            (
                "corpus",
                serde_json::json!({"corpus_sha256":"corpus-b","query_seed_digest":"seeds-a","per_source":40,"queries":3,"model_id":"teacher","dims":4}),
                "teacher",
                4,
                40,
                3,
                "corpus",
            ),
            (
                "seeds",
                serde_json::json!({"corpus_sha256":"corpus-a","query_seed_digest":"seeds-b","per_source":40,"queries":3,"model_id":"teacher","dims":4}),
                "teacher",
                4,
                40,
                3,
                "seed table",
            ),
            (
                "per-source",
                serde_json::json!({"corpus_sha256":"corpus-a","query_seed_digest":"seeds-a","per_source":20,"queries":3,"model_id":"teacher","dims":4}),
                "teacher",
                4,
                40,
                3,
                "--per-source",
            ),
            (
                "model",
                serde_json::json!({"corpus_sha256":"corpus-a","query_seed_digest":"seeds-a","per_source":40,"queries":3,"model_id":"someone-else","dims":4}),
                "teacher",
                4,
                40,
                3,
                "made for",
            ),
            (
                "dims",
                serde_json::json!({"corpus_sha256":"corpus-a","query_seed_digest":"seeds-a","per_source":40,"queries":3,"model_id":"teacher","dims":8}),
                "teacher",
                4,
                40,
                3,
                "dims",
            ),
            (
                "count",
                serde_json::json!({"corpus_sha256":"corpus-a","query_seed_digest":"seeds-a","per_source":40,"queries":5,"model_id":"teacher","dims":4}),
                "teacher",
                4,
                40,
                3,
                "queries",
            ),
        ];
        for (name, meta, model, dims, per_source, expected, wanted) in cases {
            let dir = sidecar_dir(name);
            let path = write_sidecar(&dir, meta, 3, 4);
            let error = read_query_vectors(&path, model, dims, &header, per_source, expected)
                .expect_err("the mismatch must be refused");
            let text = format!("{error:#}");
            assert!(
                text.contains(wanted),
                "the {name} refusal should name {wanted}: {text}"
            );
        }
    }

    /// A sidecar whose JSON is right and whose bytes are short is the one failure the
    /// metadata cannot catch, and reading past the end would give a partly-zero query
    /// vector that still ranks documents.
    #[test]
    fn a_sidecar_shorter_than_its_own_metadata_is_refused() {
        let dir = sidecar_dir("short");
        let path = write_sidecar(&dir, sidecar_meta(), 2, 4);
        let error = read_query_vectors(&path, "teacher", 4, &sidecar_header(), 40, 3)
            .expect_err("a short file must be refused");
        assert!(format!("{error:#}").contains("bytes"), "{error:#}");
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
                format!(
                    "{}#{}",
                    corpus.chunks[i].external_doc_id, corpus.chunks[i].chunk_index
                )
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
            let lane_ranking: Vec<String> = exhaustive_top_k(&vectors, query, K)
                .into_iter()
                .map(|i| keys[i].clone())
                .collect();
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
            assert_eq!(
                range.len(),
                666,
                "pass {pass} is a different size from the others"
            );
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
        many.chunks_per_second_runs
            .insert("cpu".into(), vec![3.1, 11.3, 8.3]);
        let lane = cost_lane(&[many]);
        let row = lane
            .rows
            .iter()
            .find(|r| r.metric.contains("spread"))
            .expect("a lane with three timings reports their spread");
        // (11.3 - 3.1) / 8.3 = 98.8% of the median, which is the number that says
        // this column cannot decide a gate.
        assert!(
            (row.values["many"] - 98.795).abs() < 0.01,
            "{:?}",
            row.values
        );
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
            [
                (CALIBRATION.to_string(), calibration.clone()),
                (UNANSWERABLE.to_string(), negative.clone()),
            ]
            .into_iter()
            .collect(),
        );
        confidences.insert(
            "narrow".into(),
            [
                (
                    CALIBRATION.to_string(),
                    calibration.iter().map(|v| v + 0.2).collect(),
                ),
                (
                    UNANSWERABLE.to_string(),
                    negative.iter().map(|v| v + 0.2).collect(),
                ),
            ]
            .into_iter()
            .collect(),
        );

        let lane = abstention_lane(&confidences);
        let rate = &lane.rows[0];
        assert!(
            rate.role == Role::Primary,
            "the confident-answer rate is the row G4 is read from"
        );
        assert!(
            !rate.higher_is_better,
            "a confident answer to an unanswerable question is bad"
        );
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
        let candidate: Vec<f64> = (0..40)
            .map(|i| if i % 10 == 0 { 1.0 } else { 0.0 })
            .collect();
        let baseline: Vec<f64> = (0..40)
            .map(|i| if i % 10 == 0 { 0.0 } else { 1.0 })
            .collect();
        let row = EmbeddingRow {
            family: "questions with no answer in the corpus".into(),
            metric: "confident answer rate at the model's own threshold".into(),
            higher_is_better: false,
            role: Role::Primary,
            values: [("new".to_string(), 0.1), ("v1.5".to_string(), 0.9)]
                .into_iter()
                .collect(),
            series: [
                ("new".to_string(), candidate),
                ("v1.5".to_string(), baseline),
            ]
            .into_iter()
            .collect(),
        };
        let lane = Lane {
            name: "abstention".into(),
            rationale: String::new(),
            rows: vec![row],
        };
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
