//! The grading harness.
//!
//! Subcommands:
//!   synth-build  assemble the graded corpus from the downloaded public material
//!   synth-embed  embed that corpus and write the cache both engines read
//!   synth-load   load the corpus and its vectors into PostgreSQL for the baseline
//!   load         pull a corpus and its vectors out of PostgreSQL into a cache
//!   build        build a inillucent index from the cache and report what it built
//!   grade        run every scenario against both engines and write the score card
//!   grade-embedding  compare two or more embedding models over the same corpus
//!   models       write or reseal a model manifest
//!   embed-residency  what loading the embedding model costs, and what moves it

// The harness connects to live databases and scores the numbers
// `docs/performance.md` and `docs/retrieval-quality.md` publish, so a bad
// `unwrap` in it crashes the box producing a score card rather than writing a
// caught error into the card. `docs/repository.md` used to say this crate had
// "no library to put the attributes in"; a `#![deny(...)]` is a crate root
// inner attribute and `main.rs` is a crate root, which is what
// `crates/inillucent-search/src/bin/write_latency.rs` already relies on.
//
// One attribute per lint, which is the form every governed crate uses and the
// form `policy.rs`'s `every_governed_crate_denies_undocumented_items` looks
// for - so adding this crate to `GOVERNED` is a one-line change rather than a
// reformat. It is not on that list yet: the other tests it carries ask for
// things this ticket did not scope, and the four lints and `missing_docs` are
// what the review asked for.
#![deny(missing_docs)]
#![deny(clippy::indexing_slicing)]
#![deny(clippy::unwrap_used)]
#![deny(clippy::expect_used)]
#![deny(clippy::panic)]
// The four lints are about a path that must return an error instead of
// aborting. A test that has already decided the value is there is asserting,
// and an assertion that cannot fail is not a test - so the relaxation is the
// same one every governed crate carries.
#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::indexing_slicing
    )
)]

mod arm;
mod corpus;
mod embedcheck;
mod engine;
mod gradeembed;
mod http;
mod llamacpp;
mod metrics;
mod models;
mod queryset;
mod report;
mod residency;
mod runs;
mod scenarios;
mod stats;
mod synth;
mod truncation;
mod tune;

use std::path::PathBuf;
use std::time::Instant;

use anyhow::{Context, Result};
use clap::{Args, Parser, Subcommand};
use inillucent_core::embed_onnx::Device;
use inillucent_core::rank::{AdaptiveWeights, Fusion};

/// The synthetic corpus database. `synth-load` creates and fills it, so this
/// default works for anybody who has run the setup steps in the README.
const DEFAULT_DB: &str = "postgres://127.0.0.1:5433/inillucent_synth";
/// Where the embedding model's weights live. The harness runs the model in
/// process, so there is no server to start.
const DEFAULT_MODEL_DIR: &str = "~/.cache/inillucent-models/nomic-embed-text-v1.5";

#[derive(Parser)]
#[command(
    name = "inillucent-bench",
    about = "Grade inillucent against postgres + pgvector"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,

    #[arg(long, default_value = DEFAULT_DB, global = true)]
    database_url: String,

    #[arg(long, default_value = "corpus.cache", global = true)]
    cache: PathBuf,

    /// Where models live, one directory per model id, each holding its weights,
    /// its tokenizer and the `model.json` that says what it is. Defaults to
    /// `INILLUCENT_MODELS` when it is set, and to `~/.cache/inillucent-models`
    /// otherwise.
    #[arg(long, global = true)]
    models_root: Option<PathBuf>,

    /// Where a `llama-server` arm's server listens. Never Nikaya's 8087: pointing
    /// a corpus embedding run at the production mail service is not a benchmark.
    #[arg(long, global = true, default_value_t = default_endpoint())]
    endpoint: String,

    /// Where one named model's `llama-server` listens, as `<model id>=<host:port>`,
    /// repeatable. Falls back to `--endpoint` for any model not named.
    ///
    /// One server holds one model, so comparing several GGUF arms on one card means
    /// several servers on several ports. Gate C1 does exactly that: the student's
    /// q8_0 against `nomic-embed-text-v2-moe`'s f16 and its q8_0, all through the
    /// same llama.cpp build. A machine setting, like `--max-batch-cells`: it is in
    /// no manifest, it moves no digest, and it invalidates no cache.
    #[arg(long = "endpoint-for", global = true, value_name = "MODEL=HOST:PORT")]
    endpoint_for: Vec<String>,

    /// Requests a `llama-server` arm keeps in flight at once.
    ///
    /// One by default, which is what anything being timed wants: a served model's
    /// throughput depends on how many requests are in flight far more than on the
    /// model, and the cost lane's numbers are only comparable between arms if that
    /// number is the same. Raise it for `synth-embed`, where the round trip is waste
    /// rather than measurement - embedding v2-moe's 185,078 chunks one request at a
    /// time ran at 64.6 chunks a second falling to 12, with the card at 6 per cent.
    /// Never raise it for `grade-embedding --cost`.
    #[arg(long, global = true, default_value_t = 1)]
    llama_concurrency: usize,

    /// Ceiling on `texts in a batch x longest sequence in it, squared`, which is
    /// what bounds an attention allocation. A machine setting rather than a model
    /// property: it is in no manifest and changing it moves no digest.
    ///
    /// It counts attention cells, and what a cell costs depends on the model's
    /// head count, which the budget does not know. The default is calibrated on a
    /// 12-head encoder, where it works out at about 2.8 GB a batch; at 16 heads
    /// the same budget asks for 3.7 GB. Lower it for a model with more heads.
    #[arg(long, global = true, default_value_t = 24_000_000)]
    max_batch_cells: usize,
}

/// The default `llama-server` endpoint, spelled out so the port constant has one
/// home.
fn default_endpoint() -> String {
    format!("127.0.0.1:{}", arm::DEFAULT_LLAMA_PORT)
}

/// Turn the repeated `--query-vectors <model>=<path>` arguments into a lookup.
///
/// Same shape and the same refusals as the endpoint overrides: a value with no `=` and
/// a model named twice are both silent in a long command line and both would grade an
/// arm against a file nobody meant.
/// @param pairs - the raw command line values, in the order they were given
fn parse_query_vectors(pairs: &[String]) -> Result<std::collections::BTreeMap<String, PathBuf>> {
    let mut map = std::collections::BTreeMap::new();
    for pair in pairs {
        let (model, path) = pair
            .split_once('=')
            .with_context(|| format!("--query-vectors {pair} is not <model id>=<path>"))?;
        anyhow::ensure!(!model.is_empty(), "--query-vectors {pair} names no model");
        anyhow::ensure!(!path.is_empty(), "--query-vectors {pair} names no file");
        if map.insert(model.to_string(), PathBuf::from(path)).is_some() {
            anyhow::bail!("--query-vectors names {model} twice");
        }
    }
    Ok(map)
}

/// Turn the repeated `--endpoint-for <model>=<host:port>` arguments into a lookup.
///
/// Refuses a value with no `=`, and refuses naming the same model twice, because both
/// are silent in a long command line and both would put an arm on a server the reader
/// did not intend. The `host:port` half is checked where it is used, by the same
/// splitter every endpoint goes through.
/// @param pairs - the raw command line values, in the order they were given
fn parse_endpoint_overrides(
    pairs: &[String],
) -> Result<std::collections::BTreeMap<String, String>> {
    let mut map = std::collections::BTreeMap::new();
    for pair in pairs {
        let (model, endpoint) = pair
            .split_once('=')
            .with_context(|| format!("--endpoint-for {pair} is not <model id>=<host:port>"))?;
        anyhow::ensure!(!model.is_empty(), "--endpoint-for {pair} names no model");
        anyhow::ensure!(
            !endpoint.is_empty(),
            "--endpoint-for {pair} names no endpoint"
        );
        if map
            .insert(model.to_string(), endpoint.to_string())
            .is_some()
        {
            anyhow::bail!("--endpoint-for names {model} twice");
        }
    }
    Ok(map)
}

#[derive(Subcommand)]
enum Command {
    /// Assemble the graded corpus from the downloaded public material.
    SynthBuild {
        /// Directory holding articles.jsonl, talk.jsonl, code.jsonl and issues.jsonl.
        #[arg(long)]
        derived: Option<PathBuf>,
        #[arg(long, default_value = "corpus.jsonl")]
        out: PathBuf,
        /// Multiplies every source's document and chunk count, keeping the
        /// proportions between sources. 1.0 reproduces the measured size.
        #[arg(long, default_value_t = 1.0)]
        scale: f64,
    },
    /// Check the corpus supports every graded scenario, before paying to embed it.
    SynthCheck {
        #[arg(long, default_value = "corpus.jsonl")]
        corpus: PathBuf,
        #[arg(long, default_value_t = 30)]
        per_source: usize,
    },
    /// Embed the corpus and write the cache. Resumable: rerun it to continue.
    SynthEmbed {
        #[arg(long, default_value = "corpus.jsonl")]
        corpus: PathBuf,
        #[arg(
            long,
            default_value = "~/.cache/inillucent-models/nomic-embed-text-v1.5"
        )]
        model_dir: String,
        /// The weights file, used only when the model directory has no
        /// `model.json` and the directory is the baseline's.
        #[arg(long, default_value = "model.onnx")]
        model_file: String,
        /// Processors the corpus is embedded on, comma separated: `cpu`, `cuda`,
        /// `cuda:1`, or several. Two cards halve the wall clock; each holds its own
        /// copy of the weights, so they do not contend the way two CPU sessions do.
        #[arg(long, default_value = "cpu")]
        devices: String,
        /// Batches handed to each device between synchronisations. Only matters with
        /// more than one device, where too small a window leaves the faster card idle.
        #[arg(long, default_value_t = 8)]
        window_batches: usize,
        #[arg(long, default_value_t = 16)]
        batch: usize,
        /// Chunks between progress lines.
        #[arg(long, default_value_t = 2000)]
        report_every: usize,
    },
    /// Re-score a card that is already on disk, from the per-query series inside it.
    ///
    /// A judging rule can change after a card is made, and this ticket changed one: the
    /// promoted six family composite was diagnostic, so it had values and a series but
    /// no verdict, and the gates are declared on it. Without this, correcting that
    /// would mean re-embedding every arm over 185,078 chunks to recover a number the
    /// card already contains everything needed to compute.
    ///
    /// The lanes, the values and the series are read and never touched. Only the
    /// verdicts are rewritten, by the same code a fresh run uses, from the card's own
    /// recorded stats seed.
    Rejudge {
        /// The card's JSON. Its markdown is rewritten beside it.
        #[arg(long)]
        card: PathBuf,
        /// Write to a different path instead of over the card.
        #[arg(long)]
        out: Option<PathBuf>,
    },
    /// Write every query `grade-embedding` would generate, in the order an arm's
    /// query vectors have to be in.
    ///
    /// Half of the pair that lets a model the harness cannot open be a graded arm:
    /// this writes the texts, an external embedder writes the vectors, and
    /// `grade-embedding --query-vectors` reads them back under the same refusal
    /// checks a cache header gets.
    QueryTexts {
        /// A cache of the corpus the queries are generated from. Any arm's will do:
        /// the queries come from the chunks, and every arm holds the same chunks.
        #[arg(long)]
        from_cache: PathBuf,
        #[arg(long, default_value_t = 40)]
        per_source: usize,
        /// Generate against only the first N chunks, matching `grade-embedding --limit`.
        #[arg(long)]
        limit: Option<usize>,
        #[arg(long)]
        out: PathBuf,
    },
    /// Assemble a sealed cache from a vector file another program produced, so a
    /// model with no ONNX graph and no GGUF can still be graded by this harness.
    ///
    /// Two models this ticket has to grade have neither. `Qwen3-Embedding-8B` and
    /// `-4B` are the candidate teachers and ship as safetensors only, and a teacher
    /// that is not graded on the suite before it teaches is exactly the gap
    /// task-1818 left. Every student checkpoint is in the same position: the loop
    /// grades five weight interpolations an iteration and exporting each to ONNX
    /// first would cost more than the pilot that produced them.
    ///
    /// The vector file is little-endian f32, `dims` floats a chunk, in corpus order,
    /// which is the same shape `synth-embed` writes and `export-vectors` reads. What
    /// this command adds is the header: the corpus digest, the manifest digest, the
    /// query seed digest and the truncation count all come from the harness's own
    /// code, so a cache assembled here carries the same provenance as one the
    /// harness embedded itself and `grade-embedding`'s refusal checks apply to it
    /// unchanged. A producer that gets the model wrong is caught by those checks
    /// rather than by this command trusting it.
    CacheFromVectors {
        #[arg(long, default_value = "corpus.jsonl")]
        corpus: PathBuf,
        /// The model directory, holding the `model.json` these vectors belong to.
        #[arg(long)]
        model_dir: String,
        /// The weights file, used only when the directory has no `model.json`.
        #[arg(long, default_value = "model.onnx")]
        model_file: String,
        /// The little-endian f32 vector file, `dims` floats a chunk, in corpus order.
        #[arg(long)]
        vectors: PathBuf,
        /// How many chunks the producer had to cut at the model's token bound.
        ///
        /// Not derivable from the vectors, and the truncation share is a gate (C3),
        /// so it is required rather than defaulted to zero. Task-1818 had a path that
        /// silently reassembled a cache with a truncation count of zero and the card
        /// reported it as fact.
        #[arg(long)]
        truncated: usize,
        /// Skip the weights and tokenizer digest check.
        ///
        /// For a model whose weights this harness cannot see, which is the whole
        /// reason the command exists: a `sentence-transformers` checkpoint has no
        /// `model.onnx` to digest. The manifest digest still goes into the header,
        /// so the cache is still bound to the model contract that produced it.
        #[arg(long, default_value_t = false)]
        unverified_weights: bool,
    },
    /// Create the PostgreSQL schema and load the corpus and its vectors into it.
    SynthLoad {
        #[arg(long, default_value = "corpus.jsonl")]
        corpus: PathBuf,
        /// Skip building the HNSW and full text indexes, which is the slow part.
        #[arg(long, default_value_t = false)]
        no_indexes: bool,
    },
    /// Pull the corpus out of PostgreSQL and cache it locally.
    Load {
        /// Load only the first N chunks, for a fast iteration cycle.
        #[arg(long)]
        limit: Option<usize>,
    },
    /// Build a inillucent index from the cache and report build statistics.
    Build {
        #[arg(long)]
        limit: Option<usize>,
        #[arg(long, default_value_t = false)]
        quantized: bool,
    },
    /// Build an index and save it to a directory.
    Save {
        #[arg(long)]
        limit: Option<usize>,
        #[arg(long, default_value_t = true)]
        quantized: bool,
        #[arg(long, default_value = "index.inillucent")]
        dir: PathBuf,
    },
    /// Open a saved index, run a few queries, and report what it cost.
    ///
    /// This is the measurement that describes a serving process: it holds the
    /// index and nothing else, where a build also holds the loader's copy of the
    /// corpus.
    Open {
        #[arg(long, default_value = "index.inillucent")]
        dir: PathBuf,
    },
    /// Check that the cache's vectors were made from the cache's text, by the
    /// model the cache says made them.
    EmbedCheck {
        /// Overrides the model the cache names, and is refused when the two
        /// disagree. Required only for a legacy cache, which names nothing.
        #[arg(long)]
        model_dir: Option<String>,
        #[arg(long, default_value = "model.onnx")]
        model_file: String,
        /// Processor the queries are embedded on: `cpu`, `cuda` or `cuda:N`.
        #[arg(long, default_value = "cpu")]
        device: String,
        /// Chunks re-embedded and compared, spread across the whole corpus.
        #[arg(long, default_value_t = 200)]
        samples: usize,
        #[arg(long, default_value_t = 16)]
        batch: usize,
    },
    /// Sweep the ranking settings over ONE index build.
    ///
    /// `grade` rebuilds the index for every run and takes twenty minutes; nothing
    /// swept here needs a new index, so this asks the same graph a few hundred more
    /// questions instead. Use it to choose a default, then confirm it with `grade`.
    Tune(TuneArgs),
    /// Run the full graded suite and write the score card.
    Grade(GradeArgs),
    /// Compare two or more embedding models over one corpus.
    ///
    /// Each cache is one model's vectors for the same corpus. The run refuses
    /// before it starts unless every cache agrees on the corpus digest, the chunk
    /// count and the query seed table, and unless every model's manifest still
    /// digests to what its cache was embedded against.
    GradeEmbedding(GradeEmbeddingArgs),
    /// Embed a stride sample of the corpus with one model and write the texts
    /// and the vectors out, so another implementation can be compared against
    /// this one.
    ///
    /// This is how gate G10 is measured. Parity between what the harness runs
    /// and what the model's own framework produces cannot be checked from inside
    /// either of them; it needs both, over the same real chunks, and this is the
    /// side of it that speaks ONNX.
    ExportVectors {
        #[arg(long, default_value = "corpus.jsonl")]
        corpus: PathBuf,
        /// The model directory, which must hold a manifest.
        #[arg(long)]
        model_dir: String,
        /// Chunks embedded, spread evenly across the whole corpus.
        #[arg(long, default_value_t = 1000)]
        samples: usize,
        /// Where the texts go, one JSON object per line.
        #[arg(long)]
        texts_out: PathBuf,
        /// Where the vectors go: `samples` x `dims` little endian f32.
        #[arg(long)]
        vectors_out: PathBuf,
        #[arg(long, default_value = "cpu")]
        device: String,
        #[arg(long, default_value_t = 16)]
        batch: usize,
        /// Embed as queries rather than as documents, so the query prefix and the
        /// query side of an asymmetric model are covered too.
        #[arg(long, default_value_t = false)]
        as_queries: bool,
    },
    /// Measure what opening the embedding model costs, and what moves that cost.
    ///
    /// The number this exists for is the one that decides whether an
    /// application can load the model per query and drop it again, or has to
    /// keep it resident. `docs/embeddings.md` prints what it found.
    EmbedResidency {
        /// The model directory, which must hold a manifest.
        #[arg(long, default_value = DEFAULT_MODEL_DIR)]
        model_dir: String,
        /// Where a pre-optimized graph is written and read back from. It is a
        /// second copy of the weights, so it is not written under the model
        /// directory by default.
        #[arg(long)]
        optimized_dir: Option<PathBuf>,
        /// Processors to run every arm on, comma separated.
        #[arg(long, default_value = "cpu")]
        devices: String,
        /// Load and drop cycles per arm. The median of these is reported.
        #[arg(long, default_value_t = 5)]
        repeats: usize,
        /// Embeddings taken through an already-open session, per cycle.
        #[arg(long, default_value_t = 20)]
        steady: usize,
        /// Skip writing and measuring the pre-optimized graph, which costs a
        /// second copy of the weights on disk.
        #[arg(long, default_value_t = false)]
        skip_optimized: bool,
    },
    /// Write or reseal a model's manifest, filling in the weights and tokenizer
    /// digests from the files on disk.
    ///
    /// Sealing is what makes every later refusal possible: a manifest with no
    /// digests cannot notice that its weights were replaced.
    Models {
        /// The model directory. Its name is the model id.
        #[arg(long)]
        dir: PathBuf,
    },
}

/// The flags `tune` takes.
///
/// **A struct rather than fields on the variant, so the function that runs
/// this subcommand takes one parameter.** Three of `main`'s arms turn twenty
/// or more flags into one options struct, and a helper taking those flags
/// positionally would be the nine-argument call `policy.rs`'s parameter check
/// refuses - twenty times over.
#[derive(Args)]
struct TuneArgs {
    #[arg(long)]
    limit: Option<usize>,
    #[arg(long, default_value_t = 30)]
    per_source: usize,
    #[arg(long, default_value = DEFAULT_MODEL_DIR)]
    model_dir: String,
    #[arg(long, default_value = "model.onnx")]
    model_file: String,
    /// Processor the queries are embedded on: `cpu`, `cuda` or `cuda:N`.
    #[arg(long, default_value = "cpu")]
    device: String,
    /// Lexical coverage exponents to try, comma separated.
    #[arg(long, default_value = "0,0.5,1,1.5,2")]
    coverages: String,
    /// Vector weights to try for the two score based fusions, comma separated.
    #[arg(long, default_value = "0.3,0.5,0.7,0.8,0.9")]
    weights: String,
    /// Lexical proximity weights to try, comma separated.
    #[arg(long, default_value = "0,0.25,0.5,0.75,1")]
    proximities: String,
    /// Whether a query term also matches the terms it prefixes: `true`, `false`
    /// or both, comma separated.
    #[arg(long, default_value = "true,false")]
    prefixes: String,
    /// Whether the count of matched query terms outranks the score: `true`,
    /// `false` or both, comma separated.
    #[arg(long, default_value = "true,false")]
    tiers: String,
    /// Ordered-phrase weights to try, comma separated.
    #[arg(long, default_value = "0")]
    phrases: String,
    /// Fusion methods to try, comma separated: `rrf`, `minmax`, `convex`,
    /// `tmm`.
    #[arg(long, default_value = "minmax")]
    fusions: String,
    /// Diversity lambdas to try, comma separated. 1 selects purely by score.
    #[arg(long, default_value = "1")]
    mmrs: String,
    /// Adaptive weighting rules to try, as `oov:identifier:separation:coverage`
    /// gain quadruples, several separated by commas. The arm with no adaptive
    /// rule at all is always included alongside them.
    #[arg(long, default_value = "")]
    adaptive: String,
    /// Added to the query set seeds, so a setting can be chosen on queries the
    /// graded run will not use. 0 uses the same queries `grade` does.
    #[arg(long, default_value_t = 100)]
    seed_offset: u64,
    /// Fixes every interval and p-value the sweep prints.
    #[arg(long, default_value_t = 20260901)]
    stats_seed: u64,
}

/// The flags `grade` takes.
///
/// **A struct rather than fields on the variant, so the function that runs
/// this subcommand takes one parameter.** Three of `main`'s arms turn twenty
/// or more flags into one options struct, and a helper taking those flags
/// positionally would be the nine-argument call `policy.rs`'s parameter check
/// refuses - twenty times over.
#[derive(Args)]
struct GradeArgs {
    #[arg(long)]
    limit: Option<usize>,
    /// Queries per source for the document identity scenarios.
    #[arg(long, default_value_t = 40)]
    per_source: usize,
    #[arg(long, default_value = DEFAULT_MODEL_DIR)]
    model_dir: String,
    #[arg(long, default_value = "model.onnx")]
    model_file: String,
    /// Relative to the working directory, so running from the repository root
    /// writes the card into the repository. The previous default was
    /// `../inillucent-scorecard.md`, which from the repository root wrote it to the
    /// parent directory instead.
    #[arg(long, default_value = "inillucent-scorecard.md")]
    out: PathBuf,
    /// Processor the queries are embedded on: `cpu`, `cuda` or `cuda:N`.
    #[arg(long, default_value = "cpu")]
    device: String,
    /// How the vector and lexical lists are combined: `rrf`, `minmax`,
    /// `convex` or `tmm`. Applied to BOTH engines, so the hybrid family stays
    /// a measurement of retrieval rather than of ranking policy.
    #[arg(long, default_value = "minmax")]
    fusion: String,
    /// Weight on the vector side for the two score based fusions.
    #[arg(long, default_value_t = 0.35)]
    vector_weight: f32,
    /// Exponent on the share of the query a lexical hit contains. inillucent only:
    /// PostgreSQL already requires every term, so it has nothing to weight.
    #[arg(long, default_value_t = 3.0)]
    lexical_coverage: f32,
    /// How much of a lexical score is scaled by how tightly the matched query
    /// terms sit together. inillucent only: `ts_rank_cd` already does this.
    #[arg(long, default_value_t = 1.0)]
    lexical_proximity: f32,
    /// Whether a query term also matches the terms it prefixes.
    #[arg(long, default_value_t = false, action = clap::ArgAction::Set)]
    lexical_prefix: bool,
    /// Whether the count of matched query terms outranks the score.
    #[arg(long, default_value_t = false, action = clap::ArgAction::Set)]
    lexical_tier: bool,
    /// How much of a lexical score is scaled by whether the matched query
    /// terms appear in the query's own order, on top of how close together
    /// they sit. inillucent only.
    #[arg(long, default_value_t = 0.75)]
    lexical_phrase: f32,
    /// How far down the BM25 ranking the position aware rescoring reaches, as
    /// a multiple of the requested k.
    #[arg(long, default_value_t = 6)]
    lexical_rescore_depth: usize,
    /// Choose the vector weight per query from the query's own shape and from
    /// how well each side separated its leader, rather than using one weight
    /// for every question.
    #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
    adaptive_fusion: bool,
    /// Adaptive weighting: added in proportion to the share of query terms the
    /// dictionary has never seen.
    #[arg(long, default_value_t = 0.10)]
    adaptive_oov_gain: f32,
    /// Adaptive weighting: subtracted in proportion to the share of query terms
    /// that look like identifiers.
    #[arg(long, default_value_t = 0.10)]
    adaptive_identifier_gain: f32,
    /// Adaptive weighting: added in proportion to how much better the vector
    /// list separates its leader than the lexical list separates its own.
    #[arg(long, default_value_t = 0.10)]
    adaptive_separation_gain: f32,
    /// Adaptive weighting: subtracted in proportion to how much of the query
    /// the best lexical hit holds.
    #[arg(long, default_value_t = 0.10)]
    adaptive_coverage_gain: f32,
    /// Maximal Marginal Relevance: 1.0 selects purely by fused score, lower
    /// values trade score for novelty against what is already selected.
    #[arg(long, default_value_t = 1.0)]
    mmr_lambda: f32,
    /// Where per-query run artifacts are collected, one directory per run.
    #[arg(long, default_value = "runs")]
    runs_dir: PathBuf,
    /// Fixes every bootstrap interval and p-value the card reports, so two
    /// readings of one run reach the same verdict.
    #[arg(long, default_value_t = 20260901)]
    stats_seed: u64,
    /// Skip the two pgvector configurations, for iterating on inillucent alone.
    #[arg(long, default_value_t = false)]
    inillucent_only: bool,
}

/// The flags `grade-embedding` takes.
///
/// **A struct rather than fields on the variant, so the function that runs
/// this subcommand takes one parameter.** Three of `main`'s arms turn twenty
/// or more flags into one options struct, and a helper taking those flags
/// positionally would be the nine-argument call `policy.rs`'s parameter check
/// refuses - twenty times over.
#[derive(Args)]
struct GradeEmbeddingArgs {
    /// The caches, one per model. Two or more.
    #[arg(long = "cache-set", num_args = 1.., required = true)]
    cache_set: Vec<PathBuf>,
    /// The model every other model is compared against. Defaults to the
    /// first cache's model.
    #[arg(long)]
    baseline: Option<String>,
    #[arg(long)]
    limit: Option<usize>,
    /// Queries per source for the document identity family.
    #[arg(long, default_value_t = 40)]
    per_source: usize,
    /// Query vectors an arm's own model produced elsewhere, as
    /// `<model id>=<path.f32>`, repeatable. Use it for a model this harness cannot
    /// open - a safetensors checkpoint with no export - after writing the texts
    /// with `query-texts`. The `.meta.json` beside the file has to name the same
    /// corpus, seed table, `--per-source`, model and width, or the run refuses.
    #[arg(long = "query-vectors", value_name = "MODEL=PATH")]
    query_vectors: Vec<String>,
    /// Processor the queries are embedded on: `cpu`, `cuda` or `cuda:N`.
    #[arg(long, default_value = "cpu")]
    device: String,
    #[arg(long, default_value = "embedding-scorecard.md")]
    out: PathBuf,
    #[arg(long, default_value = "runs")]
    runs_dir: PathBuf,
    #[arg(long, default_value_t = 20260901)]
    stats_seed: u64,
    /// Exhaustive cosine over every family: the embedding on its own.
    #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
    dense: bool,
    /// The real pipeline with the shipped fusion, which is what an agent gets.
    #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
    hybrid: bool,
    /// Throughput, weights on disk, tokens per chunk and truncation share.
    /// Re-embeds a sample per model per device, so it costs minutes.
    #[arg(long, default_value_t = false, action = clap::ArgAction::Set)]
    cost: bool,
    /// Each model's narrowed ranking against its own full-width ranking.
    #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
    matryoshka: bool,
    /// Gate G4: how often each model answers confidently when nothing in
    /// the corpus answers the question, on its own calibrated threshold.
    #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
    abstention: bool,
    /// BM25 with no embedding model at all, so the card says what the
    /// lexical half of the shipped pipeline is worth on its own. Scored
    /// inside the hybrid lane, so it measures nothing when hybrid is off.
    #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
    lexical: bool,
    /// Distinct chunks re-embedded to time each model.
    #[arg(long, default_value_t = 2000)]
    cost_samples: usize,
    /// Processors the cost lane times each model on, comma separated.
    #[arg(long, default_value = "cpu,cuda:0")]
    cost_devices: String,
    /// Timed passes per model per device, each over a disjoint slice of
    /// chunks. One timing is not a measurement: the same model on this
    /// machine has varied 3.7x between runs.
    #[arg(long, default_value_t = 3)]
    cost_repeats: usize,
    /// Chunks the Matryoshka lane ranks over.
    #[arg(long, default_value_t = 25000)]
    matryoshka_chunks: usize,
}

/// Parses a comma separated device list into the devices the embedder opens a
/// session on, rejecting an empty list rather than silently embedding on nothing.
/// @param text - the value of --devices, e.g. "cuda:0,cuda:1"
fn parse_devices(text: &str) -> Result<Vec<Device>> {
    let devices: Vec<Device> = text
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(Device::parse)
        .collect::<Result<_>>()?;
    anyhow::ensure!(!devices.is_empty(), "--devices named no device");
    Ok(devices)
}

/// Parses a fusion name into the method the engines are given.
/// @param name - `rrf`, `minmax` or `convex`
/// @param vector_weight - the weight on the vector side, ignored by `rrf`
fn parse_fusion(name: &str, vector_weight: f32) -> Result<Fusion> {
    Ok(match name.trim().to_ascii_lowercase().as_str() {
        "rrf" => Fusion::ReciprocalRank {
            k: inillucent_core::rank::RRF_K,
        },
        "minmax" => Fusion::NormalizedScore { vector_weight },
        "convex" => Fusion::Convex { vector_weight },
        // Theoretical min-max. Scales each side by bounds the results had no say
        // in, so a weak list stays weak and a fused score means the same thing
        // from one query to the next.
        "tmm" => Fusion::TheoreticalMinMax { vector_weight },
        other => {
            anyhow::bail!("unknown fusion {other}, expected rrf, minmax, convex or tmm")
        }
    })
}

/// Parses a comma separated list of booleans.
/// @param text - e.g. "true,false"
fn parse_bools(text: &str) -> Result<Vec<bool>> {
    let values: Vec<bool> = text
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| {
            s.parse::<bool>()
                .map_err(|e| anyhow::anyhow!("{s} is not true or false: {e}"))
        })
        .collect::<Result<_>>()?;
    anyhow::ensure!(!values.is_empty(), "the list named no values");
    Ok(values)
}

/// Parses a comma separated list of numbers, rejecting an empty list rather than
/// silently sweeping nothing.
/// @param text - e.g. "0,0.5,1"
fn parse_floats(text: &str) -> Result<Vec<f32>> {
    let values: Vec<f32> = text
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| {
            s.parse::<f32>()
                .map_err(|e| anyhow::anyhow!("{s} is not a number: {e}"))
        })
        .collect::<Result<_>>()?;
    anyhow::ensure!(!values.is_empty(), "the list named no values");
    Ok(values)
}

/// Parses adaptive weighting rules: `oov:identifier:separation:coverage` gain
/// quadruples, several separated by commas.
///
/// A quadruple rather than four separate lists, because the gains interact and
/// sweeping their cross product produces hundreds of arms nobody asked for. The
/// base weight is filled in from the weight being swept, so a rule with every gain
/// at zero is exactly the fixed arm beside it.
/// @param text - e.g. "0.3:0.3:0:0,0:0.4:0.2:0"
fn parse_adaptive(text: &str) -> Result<Vec<AdaptiveWeights>> {
    let mut out = Vec::new();
    for rule in text.split(',').map(str::trim).filter(|s| !s.is_empty()) {
        let parts: Vec<f32> = rule
            .split(':')
            .map(|p| {
                p.trim()
                    .parse::<f32>()
                    .map_err(|e| anyhow::anyhow!("{p} is not a number: {e}"))
            })
            .collect::<Result<_>>()?;
        let [out_of_vocabulary_gain, identifier_gain, separation_gain, coverage_gain] =
            parts.as_slice()
        else {
            anyhow::bail!(
                "an adaptive rule needs four gains, oov:identifier:separation:coverage, got {rule}"
            );
        };
        out.push(AdaptiveWeights {
            out_of_vocabulary_gain: *out_of_vocabulary_gain,
            identifier_gain: *identifier_gain,
            separation_gain: *separation_gain,
            coverage_gain: *coverage_gain,
            ..Default::default()
        });
    }
    Ok(out)
}

/// Expand a leading `~/` so a default path can name the home directory.
/// Unwraps a search over a vector this program took out of the index itself.
///
/// The entry points refuse a query whose width is not the index's (task-1946,
/// H4); a probe vector copied out of the same index cannot be one of those, so
/// a refusal here is a defect in this program rather than a condition to handle.
///
/// @param answered - what the search returned
fn probe<T>(answered: anyhow::Result<T>) -> anyhow::Result<T> {
    answered.context("the probe vector was copied out of this index, so a refused width here is a defect in this program")
}

fn expand_home(path: &str) -> Result<String> {
    Ok(match path.strip_prefix("~/") {
        Some(rest) => format!("{}/{}", std::env::var("HOME")?, rest),
        None => path.to_string(),
    })
}

/// Write a scorecard as its markdown and its JSON, and say where both went.
///
/// `grade-embedding` and `rejudge` both end this way, and the pair has to stay a pair: the
/// markdown is what a person reads and the JSON is what `rejudge` and the report scripts parse,
/// so a card written as one without the other is a card that cannot be re-scored later.
///
/// @param card - the scorecard to write
/// @param markdown - where the markdown goes; the JSON goes beside it under the same stem
fn write_card(card: &gradeembed::EmbeddingCard, markdown: &std::path::Path) -> Result<()> {
    std::fs::write(markdown, gradeembed::render(card))?;
    let json_path = markdown.with_extension("json");
    std::fs::write(&json_path, serde_json::to_string_pretty(card)?)?;
    eprintln!(
        "card written to {}, measurements to {}",
        markdown.display(),
        json_path.display()
    );
    Ok(())
}

/// Assemble a corpus cache from vectors an embedder outside this harness produced.
///
/// Half of the pair that lets a model this harness cannot open still be a graded arm. The
/// vectors arrive as a file; everything that makes a cache trustworthy - the corpus they
/// describe, the model manifest they are labelled with, the seeds, the truncation count - is
/// attached here, and the cache is loaded straight back so the run fails now rather than on the
/// first arm if any of it disagrees.
///
/// @param corpus - the chunk file the vectors were produced from
/// @param model_dir - the model directory, resolved for its manifest
/// @param model_file - the weights file inside that directory
/// @param vectors - the vectors an external embedder wrote
/// @param cache - where the assembled cache goes
/// @param truncated - how many chunks the embedder reported truncating
/// @param unverified_weights - skip the weights digest check, for a model held elsewhere
fn cache_from_vectors(
    corpus: &std::path::Path,
    model_dir: &str,
    model_file: &str,
    vectors: &std::path::Path,
    cache: &std::path::Path,
    truncated: usize,
    unverified_weights: bool,
) -> Result<()> {
    let dir = expand_home(model_dir)?;
    let model = models::resolve_dir(std::path::Path::new(&dir), model_file)?;
    if !unverified_weights {
        model.verify_files()?;
    }
    let chunks = synth::read_corpus(corpus)?;
    anyhow::ensure!(
        truncated <= chunks.len(),
        "{truncated} truncated chunks were reported for a corpus of {}",
        chunks.len()
    );
    synth::assemble_cache(
        &chunks,
        vectors,
        cache,
        &model.manifest,
        &scenarios::seeds(),
        truncated,
    )?;
    let assembled = corpus::load_cache(cache)?;
    eprintln!(
        "{} chunks of {} at {} dims, {truncated} truncated, corpus {}",
        assembled.len(),
        assembled.header.model_id,
        assembled.dims,
        corpus::short(&assembled.header.corpus_sha256)
    );
    Ok(())
}

/// The arm options every subcommand builds the same way, from the same global flags.
///
/// Built in one place because it was built in four, and each flag added to the set had to be
/// added to all four by hand. `--endpoint-for` and `--llama-concurrency` were added that way on
/// this ticket: a served arm that reached three of the four call sites would have run against
/// the wrong endpoint under a label naming the right one, which is the failure this harness
/// exists to make impossible.
///
/// A caller that needs a different device or batch size still says so at the call site, with
/// `..base.clone()`, so the override stays visible where it is made. It carries no device of its
/// own: `--device` belongs to a subcommand, not to the command line as a whole.
///
/// @param cli - the parsed command line, for the endpoint flags and the batch ceiling
fn arm_options_for(cli: &Cli) -> Result<arm::ArmOptions> {
    Ok(arm::ArmOptions {
        endpoint: cli.endpoint.clone(),
        endpoint_overrides: parse_endpoint_overrides(&cli.endpoint_for)?,
        concurrency: cli.llama_concurrency,
        max_batch_cells: cli.max_batch_cells,
        ..Default::default()
    })
}

/// What one `synth-embed` run is asked to do.
///
/// **A struct rather than nine positional arguments, which is what task-1961's
/// A8 did for the ten functions that used to take more than eight.** This one
/// kept its nine under an `#[allow(clippy::too_many_arguments)]` - the only
/// such allow in `crates/` or `drivers/` - because nothing measured parameter
/// counts until task-1970 added `no_function_takes_more_than_eight_parameters`
/// to `policy.rs`. Four of the nine are `&str` or `usize` in a row, so a
/// transposed pair at the call site was a compiling change of meaning.
struct SynthEmbedRequest<'a> {
    /// The chunk file to embed.
    corpus: &'a std::path::Path,
    /// Where the cache goes.
    cache: &'a std::path::Path,
    /// The model directory, with `~` not yet expanded.
    model_dir: &'a str,
    /// The weights file inside the model directory.
    model_file: &'a str,
    /// The processors to spread the work over, comma separated as given.
    devices: &'a str,
    /// Texts per request.
    batch: usize,
    /// How often to print progress, in chunks.
    report_every: usize,
    /// Batches per progress window.
    window_batches: usize,
    /// The arm options built from the global flags.
    base: &'a arm::ArmOptions,
}

/// Embed a corpus into a cache with one model, on one or more processors.
///
/// The weights are verified before a chunk is read. A cache is trusted for the rest of the
/// ticket - every lane reads it and nothing re-embeds - so a model that is not the model its
/// manifest describes has to be refused here or not at all.
///
/// @param request - what to embed, with which model, on which processors
fn synth_embed(request: &SynthEmbedRequest<'_>) -> Result<()> {
    let dir = expand_home(request.model_dir)?;
    let devices = parse_devices(request.devices)?;
    // `parse_devices` answers at least one device or an error, so this names
    // the first of the list the caller gave rather than defaulting to one.
    let first = devices
        .first()
        .copied()
        .context("no device to embed on: the device list parsed to nothing")?;
    let model = models::resolve_dir(std::path::Path::new(&dir), request.model_file)?;
    model.verify_files()?;
    synth::embed(
        request.corpus,
        request.cache,
        &model,
        &scenarios::seeds(),
        &arm::ArmOptions {
            batch_size: request.batch,
            device: first,
            ..request.base.clone()
        },
        request.report_every,
        &devices,
        request.window_batches,
    )
}

/// The flags that belong to no one subcommand.
///
/// **Cloned out of `Cli` before the `match`, because matching on
/// `cli.command` moves it and a partially moved `Cli` cannot be borrowed.**
/// Two clones once per process, against every subcommand function having to
/// take the two fields separately.
struct Global {
    /// The corpus cache every subcommand reads or writes.
    cache: PathBuf,
    /// The PostgreSQL baseline the corpus is loaded from and graded against.
    database_url: String,
}

/// Parses the command line and runs the one subcommand it names.
///
/// **A dispatcher, with one function per subcommand.** It used to be a single
/// `match` of 576 lines: eighteen arms, three of which turned twenty or more
/// flags into one options struct, so a reader looking for what `grade` does had
/// to find it among seventeen other commands. The three widest arms take an
/// `Args` struct now, which is what lets each of their functions take one
/// parameter instead of twenty.
fn main() -> Result<()> {
    let cli = Cli::parse();
    let models_root = cli
        .models_root
        .clone()
        .unwrap_or_else(models::default_models_root);
    let base = arm_options_for(&cli)?;
    // Taken before the `match`, which moves the command out of `cli` and so
    // leaves `cli` itself unborrowable.
    let global = Global {
        cache: cli.cache.clone(),
        database_url: cli.database_url.clone(),
    };
    match cli.command {
        Command::SynthBuild {
            derived,
            out,
            scale,
        } => synth_build(derived, &out, scale),
        Command::SynthCheck { corpus, per_source } => synth::check(&corpus, per_source),
        Command::SynthEmbed {
            corpus,
            model_dir,
            model_file,
            batch,
            report_every,
            devices,
            window_batches,
        } => synth_embed(&SynthEmbedRequest {
            corpus: &corpus,
            cache: &cli.cache,
            model_dir: &model_dir,
            model_file: &model_file,
            devices: &devices,
            batch,
            report_every,
            window_batches,
            base: &base,
        }),
        Command::Rejudge { card, out } => rejudge(&card, out),
        Command::QueryTexts {
            from_cache,
            per_source,
            limit,
            out,
        } => query_texts(&from_cache, per_source, limit, &out),
        Command::CacheFromVectors {
            corpus,
            model_dir,
            model_file,
            vectors,
            truncated,
            unverified_weights,
        } => cache_from_vectors(
            &corpus,
            &model_dir,
            &model_file,
            &vectors,
            &cli.cache,
            truncated,
            unverified_weights,
        ),
        Command::SynthLoad { corpus, no_indexes } => synth_load(&global, &corpus, no_indexes),
        Command::Load { limit } => load(&global, limit),
        Command::Build { limit, quantized } => build(&global, limit, quantized),
        Command::Save {
            limit,
            quantized,
            dir,
        } => save(&global, limit, quantized, &dir),
        Command::Open { dir } => open_saved(&dir),
        Command::EmbedCheck {
            model_dir,
            model_file,
            samples,
            batch,
            device,
        } => embed_check(EmbedCheckRequest {
            global: &global,
            base: &base,
            models_root: &models_root,
            model_dir,
            model_file: &model_file,
            samples,
            batch,
            device: &device,
        }),
        Command::Tune(args) => tune_command(&global, &args),
        Command::Grade(args) => grade_command(&global, &base, &args),
        Command::GradeEmbedding(args) => grade_embedding(&base, &models_root, args),
        Command::ExportVectors {
            corpus,
            model_dir,
            samples,
            texts_out,
            vectors_out,
            device,
            batch,
            as_queries,
        } => export_vectors(ExportVectorsRequest {
            corpus: &corpus,
            model_dir: &model_dir,
            samples,
            texts_out: &texts_out,
            vectors_out: &vectors_out,
            device: &device,
            batch,
            as_queries,
        }),
        Command::EmbedResidency {
            model_dir,
            optimized_dir,
            devices,
            repeats,
            steady,
            skip_optimized,
        } => embed_residency(
            &model_dir,
            optimized_dir,
            &devices,
            repeats,
            steady,
            skip_optimized,
        ),
        Command::Models { dir } => seal_model(&dir),
    }
}

/// Assembles the graded corpus from the downloaded public material.
///
/// @param derived - the directory the material was extracted to, or the default
/// @param out - where the JSONL corpus goes
/// @param scale - a multiplier on every source's document and chunk targets
fn synth_build(derived: Option<PathBuf>, out: &std::path::Path, scale: f64) -> Result<()> {
    let derived = derived.unwrap_or_else(synth::default_derived_dir);
    let report = synth::build(&derived, out, scale)?;
    eprintln!(
        "\nwrote {} chunks across {} documents to {}",
        report.chunks,
        report.documents,
        out.display()
    );
    eprintln!(
        "{:<12} {:>7} {:>8} {:>7}",
        "source", "docs", "chunks", "mean"
    );
    for (source, docs, chunks, mean) in &report.per_source {
        eprintln!("{source:<12} {docs:>7} {chunks:>8} {mean:>7}");
    }
    eprintln!(
        "titles unique to one document: {}, of which usable as identity queries: {}",
        report.unique_titles, report.identity_usable
    );
    Ok(())
}

/// Re-judges a saved card and writes it out again.
///
/// The judgements are recomputed from the measurements the card already holds,
/// so nothing is re-run: a change to how a verdict is decided can be applied to
/// a run that cost twenty minutes without paying for it twice.
///
/// @param card - the saved card's JSON
/// @param out - where to write it, or over the card it came from
fn rejudge(card: &std::path::Path, out: Option<PathBuf>) -> Result<()> {
    let (rescored, before, after) = gradeembed::rejudge(card)?;
    eprintln!(
        "rejudged {}: {before} judgements -> {after}, stats seed {}, baseline {}",
        card.display(),
        rescored.stats_seed,
        rescored.baseline
    );
    let json_path = out.unwrap_or_else(|| card.to_path_buf());
    write_card(&rescored, &json_path.with_extension("md"))
}

/// Writes the query set a graded run would generate, as text.
///
/// @param from_cache - the cache the queries are generated from
/// @param per_source - queries per source for the identity family
/// @param limit - grade only the first N chunks of the cache
/// @param out - where the queries go
fn query_texts(
    from_cache: &std::path::Path,
    per_source: usize,
    limit: Option<usize>,
    out: &std::path::Path,
) -> Result<()> {
    let total = gradeembed::write_query_texts(from_cache, per_source, limit, out)?;
    eprintln!("wrote {total} queries to {}", out.display());
    Ok(())
}

/// Loads the generated corpus and its vectors into PostgreSQL for the baseline.
///
/// @param global - the global flags, for the cache and the database
/// @param corpus - the JSONL corpus produced by `synth-build`
/// @param no_indexes - load the rows and leave the indexes unbuilt
fn synth_load(global: &Global, corpus: &std::path::Path, no_indexes: bool) -> Result<()> {
    let chunks = synth::read_corpus(corpus)?;
    let c = corpus::load_cache(&global.cache)?;
    synth::load_postgres(&global.database_url, &chunks, &c, !no_indexes)
}

/// Pulls a corpus and its vectors out of PostgreSQL into a cache.
///
/// @param global - the global flags, for the cache and the database
/// @param limit - read only the first N chunks, for a fast iteration cycle
fn load(global: &Global, limit: Option<usize>) -> Result<()> {
    eprintln!("loading corpus from {}", global.database_url);
    let start = Instant::now();
    let c = corpus::load_from_postgres(&global.database_url, limit)?;
    eprintln!(
        "loaded {} chunks at {} dimensions in {:.1}s",
        c.len(),
        c.dims,
        start.elapsed().as_secs_f64()
    );
    corpus::save_cache(&c, &global.cache)?;
    let bytes = std::fs::metadata(&global.cache)?.len();
    eprintln!(
        "cached {} ({:.1} MB)",
        global.cache.display(),
        bytes as f64 / 1e6
    );
    Ok(())
}

/// Builds an index from the cache and reports what it built.
///
/// @param global - the global flags, for the cache
/// @param limit - index only the first N chunks
/// @param quantized - also build the int8 codes
fn build(global: &Global, limit: Option<usize>, quantized: bool) -> Result<()> {
    let c = corpus::load_cache(&global.cache)?;
    eprintln!("cache holds {} chunks", c.len());
    let (index, _keys, stats, elapsed) = scenarios::build_index(&c, limit, quantized)?;
    eprintln!(
        "built {} chunks / {} documents in {:.1}s",
        stats.chunks, stats.documents, elapsed
    );
    eprintln!(
        "  graph: {} layers, {} edges\n  lexical: {} terms, {} postings\n  vectors: {:.1} MB, int8 codes: {:.1} MB",
        stats.graph_layers,
        stats.graph_edges,
        stats.lexical_terms,
        stats.lexical_postings,
        stats.vector_bytes as f64 / 1e6,
        stats.quantized_bytes as f64 / 1e6,
    );
    drop(index);
    Ok(())
}

/// Builds an index and saves it to a directory, with the size of every file.
///
/// @param global - the global flags, for the cache
/// @param limit - index only the first N chunks
/// @param quantized - also build the int8 codes
/// @param dir - where the index goes
fn save(
    global: &Global,
    limit: Option<usize>,
    quantized: bool,
    dir: &std::path::Path,
) -> Result<()> {
    let c = corpus::load_cache(&global.cache)?;
    let (index, _keys, stats, seconds) = scenarios::build_index(&c, limit, quantized)?;
    eprintln!("built {} chunks in {:.1}s", stats.chunks, seconds);
    let start = Instant::now();
    inillucent_core::persist::save(&index, dir)?;
    eprintln!(
        "saved to {} in {:.1}s",
        dir.display(),
        start.elapsed().as_secs_f64()
    );
    let mut total = 0u64;
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let size = entry.metadata()?.len();
        eprintln!("  {:>12} {}", size, entry.file_name().to_string_lossy());
        total += size;
    }
    eprintln!("  {:>12} total ({:.1} MB)", total, total as f64 / 1e6);
    Ok(())
}

/// Loads a saved index and exercises every query path against it.
///
/// **Every path, not just the open.** The resident memory this reports is
/// meant to describe a process that has actually served traffic; one that only
/// opened files has not paged in the graph, the postings or the codes.
///
/// @param dir - the saved index directory
fn open_saved(dir: &std::path::Path) -> Result<()> {
    let start = Instant::now();
    let index = inillucent_core::persist::load(dir)?;
    let load_seconds = start.elapsed().as_secs_f64();
    eprintln!(
        "loaded {} chunks / {} documents in {:.1}s",
        index.store().n_chunks(),
        index.store().n_documents(),
        load_seconds
    );

    let query = index.vectors().copy_of(7);
    for (label, filter) in [
        ("no predicate", inillucent_core::filter::Filter::default()),
        (
            "source = slack",
            inillucent_core::filter::Filter::source("slack"),
        ),
    ] {
        let compiled = index.compile(&filter);
        let start = Instant::now();
        let hits = probe(index.vector_search(&query, &compiled, 10, None))?;
        let vector_ms = start.elapsed().as_secs_f64() * 1000.0;
        let start = Instant::now();
        let lexical = index.lexical_search("offer eligibility rules", &compiled, 10);
        let lexical_ms = start.elapsed().as_secs_f64() * 1000.0;
        let start = Instant::now();
        let hybrid =
            probe(index.hybrid_search("offer eligibility rules", &query, &compiled, 10, None))?;
        let hybrid_ms = start.elapsed().as_secs_f64() * 1000.0;
        eprintln!(
            "  {label}: {} passing chunks, path {} | vector {} hits {:.2}ms | lexical {} hits {:.2}ms | hybrid {} hits {:.2}ms",
            compiled.pass_count(),
            index.path_for(&compiled, None),
            hits.len(),
            vector_ms,
            lexical.len(),
            lexical_ms,
            hybrid.len(),
            hybrid_ms
        );
    }
    Ok(())
}

/// What one `embed-check` run is asked to do.
struct EmbedCheckRequest<'a> {
    /// The global flags, for the cache.
    global: &'a Global,
    /// The arm options built from the global flags.
    base: &'a arm::ArmOptions,
    /// Where the models live, for a cache that names its own model.
    models_root: &'a std::path::Path,
    /// The model directory, when the caller named one.
    model_dir: Option<String>,
    /// The weights file inside it.
    model_file: &'a str,
    /// Chunks re-embedded, spread evenly across the whole corpus.
    samples: usize,
    /// Texts per request.
    batch: usize,
    /// The processor to re-embed on.
    device: &'a str,
}

/// Re-embeds a sample of the cache and checks it against the stored vectors.
///
/// @param request - the cache to check, and the model to check it with
fn embed_check(request: EmbedCheckRequest<'_>) -> Result<()> {
    let dir = request.model_dir.map(|d| expand_home(&d)).transpose()?;
    let c = corpus::load_cache(&request.global.cache)?;
    embedcheck::run(
        &c,
        request.models_root,
        dir.as_deref(),
        request.model_file,
        request.samples,
        &arm::ArmOptions {
            batch_size: request.batch,
            device: Device::parse(request.device)?,
            ..request.base.clone()
        },
    )
}

/// Sweeps the ranking settings over one index build.
///
/// The baseline arm is the configuration the engine currently ships, spelled
/// out so every other arm in the sweep is compared against it rather than
/// against whichever of themselves happened to come first.
///
/// @param global - the global flags, for the cache
/// @param args - the sweep's own flags
fn tune_command(global: &Global, args: &TuneArgs) -> Result<()> {
    let c = corpus::load_cache(&global.cache)?;
    let dir = expand_home(&args.model_dir)?;
    let defaults = inillucent_core::index::IndexConfig::default();
    let baseline = tune::Setting {
        label: "baseline (shipped defaults)".to_string(),
        coverage: defaults.lexical_coverage,
        proximity: defaults.lexical_proximity,
        prefix: defaults.lexical_prefix,
        tier: defaults.lexical_tier,
        phrase: defaults.lexical_phrase,
        fusion: defaults.fusion,
        adaptive: None,
        mmr_lambda: defaults.mmr_lambda,
    };
    let names: Vec<String> = args
        .fusions
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect();
    anyhow::ensure!(!names.is_empty(), "--fusions named no method");
    let settings = tune::build_settings(
        baseline,
        &tune::Sweep {
            coverages: &parse_floats(&args.coverages)?,
            weights: &parse_floats(&args.weights)?,
            proximities: &parse_floats(&args.proximities)?,
            prefixes: &parse_bools(&args.prefixes)?,
            tiers: &parse_bools(&args.tiers)?,
            phrases: &parse_floats(&args.phrases)?,
            fusions: &names,
            mmrs: &parse_floats(&args.mmrs)?,
            adaptive: &parse_adaptive(&args.adaptive)?,
        },
    );
    let model = models::resolve_dir(std::path::Path::new(&dir), &args.model_file)?;
    tune::run(
        &c,
        args.limit,
        args.per_source,
        &model,
        Device::parse(&args.device)?,
        &settings,
        args.seed_offset,
        args.stats_seed,
    )
}

/// Runs the full graded suite and writes the score card.
///
/// The measurements are saved as JSON beside the markdown, so the card can be
/// re-rendered or re-judged without repaying the twenty minutes the run costs.
///
/// @param global - the global flags, for the cache and the database
/// @param base - the arm options built from the global flags
/// @param args - the run's own flags
fn grade_command(global: &Global, base: &arm::ArmOptions, args: &GradeArgs) -> Result<()> {
    let c = corpus::load_cache(&global.cache)?;
    let dir = expand_home(&args.model_dir)?;
    let options = scenarios::GradeOptions {
        limit: args.limit,
        per_source: args.per_source,
        arm_options: arm::ArmOptions {
            device: Device::parse(&args.device)?,
            ..base.clone()
        },
        model: models::resolve_dir(std::path::Path::new(&dir), &args.model_file)?,
        database_url: global.database_url.clone(),
        inillucent_only: args.inillucent_only,
        device: Device::parse(&args.device)?,
        fusion: parse_fusion(&args.fusion, args.vector_weight)?,
        lexical_coverage: args.lexical_coverage,
        lexical_proximity: args.lexical_proximity,
        lexical_prefix: args.lexical_prefix,
        lexical_tier: args.lexical_tier,
        lexical_phrase: args.lexical_phrase,
        lexical_rescore_depth: args.lexical_rescore_depth,
        adaptive_fusion: args.adaptive_fusion,
        adaptive: AdaptiveWeights {
            // The base is the same weight a fixed fusion would use, so
            // every gain at zero reproduces that fusion exactly.
            base: args.vector_weight,
            out_of_vocabulary_gain: args.adaptive_oov_gain,
            identifier_gain: args.adaptive_identifier_gain,
            separation_gain: args.adaptive_separation_gain,
            coverage_gain: args.adaptive_coverage_gain,
            ..Default::default()
        },
        mmr_lambda: args.mmr_lambda,
        runs_dir: args.runs_dir.clone(),
        stats_seed: args.stats_seed,
        cache_path: global.cache.clone(),
    };
    let card = scenarios::grade(&c, &options)?;
    let markdown = report::render(&card);
    std::fs::write(&args.out, markdown)?;
    let json_path = args.out.with_extension("json");
    std::fs::write(&json_path, serde_json::to_string_pretty(&card)?)?;
    eprintln!(
        "score card written to {}, measurements to {}",
        args.out.display(),
        json_path.display()
    );
    report::print_summary(&card);
    Ok(())
}

/// Compares two or more embedding models over one corpus.
///
/// @param base - the arm options built from the global flags
/// @param models_root - where the models live
/// @param args - the run's own flags
fn grade_embedding(
    base: &arm::ArmOptions,
    models_root: &std::path::Path,
    args: GradeEmbeddingArgs,
) -> Result<()> {
    let options = gradeembed::EmbeddingGradeOptions {
        caches: args.cache_set,
        models_root: models_root.to_path_buf(),
        baseline: args.baseline,
        limit: args.limit,
        per_source: args.per_source,
        device: Device::parse(&args.device)?,
        runs_dir: args.runs_dir,
        out: args.out.clone(),
        stats_seed: args.stats_seed,
        dense: args.dense,
        hybrid: args.hybrid,
        cost: args.cost,
        matryoshka: args.matryoshka,
        abstention: args.abstention,
        lexical: args.lexical,
        cost_samples: args.cost_samples,
        cost_devices: if args.cost {
            parse_devices(&args.cost_devices)?
        } else {
            Vec::new()
        },
        cost_repeats: args.cost_repeats,
        matryoshka_chunks: args.matryoshka_chunks,
        query_vectors: parse_query_vectors(&args.query_vectors)?,
        arm_options: arm::ArmOptions {
            device: Device::parse(&args.device)?,
            ..base.clone()
        },
    };
    let card = gradeembed::run(&options)?;
    write_card(&card, &args.out)?;
    gradeembed::print_summary(&card);
    Ok(())
}

/// What one `export-vectors` run is asked to do.
struct ExportVectorsRequest<'a> {
    /// The JSONL corpus to sample from.
    corpus: &'a std::path::Path,
    /// The model directory, which must hold a manifest.
    model_dir: &'a str,
    /// Chunks embedded, spread evenly across the whole corpus.
    samples: usize,
    /// Where the sampled texts go, one JSON object per line.
    texts_out: &'a std::path::Path,
    /// Where the vectors go, as raw little-endian `f32`.
    vectors_out: &'a std::path::Path,
    /// The processor to embed on.
    device: &'a str,
    /// Texts per request.
    batch: usize,
    /// Embed with the query prefix rather than the document prefix.
    as_queries: bool,
}

/// Embeds a sample of the corpus and writes the texts and the vectors out.
///
/// **This is how gate G10 is measured.** Parity between what the harness runs
/// and what the model's own framework produces cannot be checked from inside
/// either of them; it needs both, over the same real chunks, and this is the
/// side of it that speaks ONNX.
///
/// @param request - what to embed, with which model, and where to put it
fn export_vectors(request: ExportVectorsRequest<'_>) -> Result<()> {
    use inillucent_core::embed::Embedder;
    let dir = expand_home(request.model_dir)?;
    let model = models::resolve_dir(std::path::Path::new(&dir), "model.onnx")?;
    model.verify_files()?;
    let chunks = synth::read_corpus(request.corpus)?;
    let chosen = scenarios::strided_sample(chunks.len(), request.samples);
    let texts: Vec<String> = chosen
        .iter()
        .map(|&i| {
            let chunk = chunks
                .get(i)
                .with_context(|| format!("chunk {i} of {}", chunks.len()))?;
            Ok(synth::sanitize_for_model(&chunk.content))
        })
        .collect::<Result<Vec<String>>>()?;
    eprintln!(
        "embedding {} chunks with {} as {}",
        texts.len(),
        model.manifest.id,
        if request.as_queries {
            "queries"
        } else {
            "documents"
        }
    );
    let embedder = inillucent_core::embed_onnx::OnnxEmbedder::open_manifest(
        &model.dir,
        &model.manifest,
        request.batch,
        Device::parse(request.device)?,
    )?;
    let start = Instant::now();
    let vectors = if request.as_queries {
        let prefix = &model.manifest.prefixes.query;
        let prefixed: Vec<String> = texts.iter().map(|t| format!("{prefix}{t}")).collect();
        embedder.embed_prefixed(&prefixed)?
    } else {
        embedder.embed_documents(&texts)?
    };
    eprintln!(
        "  {} vectors of {} dimensions in {:.1}s",
        vectors.len(),
        vectors.first().map(|v| v.len()).unwrap_or(0),
        start.elapsed().as_secs_f64()
    );
    let facts = embedder.truncation();
    eprintln!(
        "  {:.1} tokens per chunk, {} of {} truncated at {}",
        facts.tokens_per_text(),
        facts.truncated,
        facts.texts,
        model.manifest.max_tokens
    );
    write_exported_vectors(&request, &chunks, &chosen, &texts, &vectors)
}

/// Writes the sampled texts and their vectors, in the order they were embedded.
///
/// Row N of the text file is vector N of the vector file, which is what makes
/// the two comparable against another implementation's output.
///
/// @param request - where the two files go
/// @param chunks - the corpus the sample was drawn from
/// @param chosen - the sampled chunk indices, in the order they were embedded
/// @param texts - the sanitized texts, one per chosen index
/// @param vectors - the vectors, one per text
fn write_exported_vectors(
    request: &ExportVectorsRequest<'_>,
    chunks: &[synth::SynthChunk],
    chosen: &[usize],
    texts: &[String],
    vectors: &[Vec<f32>],
) -> Result<()> {
    use std::io::Write as _;
    let mut text_file = std::io::BufWriter::new(std::fs::File::create(request.texts_out)?);
    for (row, (&i, text)) in chosen.iter().zip(texts).enumerate() {
        let chunk = chunks
            .get(i)
            .with_context(|| format!("chunk {i} of {}", chunks.len()))?;
        let record = serde_json::json!({
            "row": row,
            "chunk": i,
            "source": chunk.source,
            "text": text,
        });
        writeln!(text_file, "{record}")?;
    }
    text_file.flush()?;

    let mut vector_file = std::io::BufWriter::new(std::fs::File::create(request.vectors_out)?);
    for v in vectors {
        for x in v {
            vector_file.write_all(&x.to_le_bytes())?;
        }
    }
    vector_file.flush()?;
    eprintln!(
        "wrote {} and {}",
        request.texts_out.display(),
        request.vectors_out.display()
    );
    Ok(())
}

/// Measures what loading the embedding model costs, and what moves it.
///
/// @param model_dir - the model directory, `~` not yet expanded
/// @param optimized_dir - where to write the pre-optimized graph
/// @param devices - the processors to measure, comma separated as given
/// @param repeats - how many times to load and drop the model
/// @param steady - queries per arm after the load, for the steady-state figure
/// @param skip_optimized - measure only the arms that need no graph pass
fn embed_residency(
    model_dir: &str,
    optimized_dir: Option<PathBuf>,
    devices: &str,
    repeats: usize,
    steady: usize,
    skip_optimized: bool,
) -> Result<()> {
    let dir = expand_home(model_dir)?;
    let dir = std::path::Path::new(&dir);
    let model = models::resolve_dir(dir, "model.onnx")?;
    let optimized_dir =
        optimized_dir.unwrap_or_else(|| std::env::temp_dir().join("inillucent-optimized-graph"));
    if !skip_optimized {
        match residency::prepare_optimized(dir, &model.manifest, &optimized_dir) {
            Ok(path) => eprintln!("optimized graph at {}", path.display()),
            Err(err) => eprintln!("no pre-optimized arm: {err:#}"),
        }
    }
    let devices = parse_devices(devices)?;
    let arms = residency::arms(dir, &optimized_dir, &devices);
    let mut measurements = Vec::new();
    for arm in &arms {
        eprintln!("measuring {}", arm.name);
        match residency::measure(arm, &model.manifest, repeats, steady) {
            Ok(m) => measurements.push(m),
            Err(err) => eprintln!("  skipped: {err:#}"),
        }
    }
    residency::report(&measurements);
    Ok(())
}

/// Writes or reseals a model manifest, and prints what it sealed.
///
/// @param dir - the model directory
fn seal_model(dir: &std::path::Path) -> Result<()> {
    let mut model = models::resolve_dir(dir, "model.onnx")?;
    let before = model.digest();
    let path = model.seal()?;
    eprintln!(
        "sealed {} ({} -> {})",
        path.display(),
        corpus::short(&before),
        corpus::short(&model.digest())
    );
    eprintln!(
        "  weights   {} {}",
        model.manifest.model_file, model.manifest.weights_sha256
    );
    eprintln!(
        "  tokenizer tokenizer.json {}",
        model.manifest.tokenizer_sha256
    );
    eprintln!("  manifest  {}", model.digest());
    Ok(())
}
