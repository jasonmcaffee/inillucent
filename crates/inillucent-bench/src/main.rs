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

mod arm;
mod corpus;
mod embedcheck;
mod http;
mod llamacpp;
mod engine;
mod gradeembed;
mod metrics;
mod models;
mod queryset;
mod report;
mod residency;
mod scenarios;
mod runs;
mod stats;
mod tune;
mod synth;

use std::path::PathBuf;
use std::time::Instant;

use anyhow::Result;
use clap::{Parser, Subcommand};
use inillucent_core::embed_onnx::Device;
use inillucent_core::rank::{AdaptiveWeights, Fusion};

/// The synthetic corpus database. `synth-load` creates and fills it, so this
/// default works for anybody who has run the setup steps in the README.
const DEFAULT_DB: &str = "postgres://127.0.0.1:5433/inillucent_synth";
/// Where the embedding model's weights live. The harness runs the model in
/// process, so there is no server to start.
const DEFAULT_MODEL_DIR: &str = "~/.cache/inillucent-models/nomic-embed-text-v1.5";

#[derive(Parser)]
#[command(name = "inillucent-bench", about = "Grade inillucent against postgres + pgvector")]
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
        #[arg(long, default_value = "~/.cache/inillucent-models/nomic-embed-text-v1.5")]
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
    Tune {
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
    },
    /// Run the full graded suite and write the score card.
    Grade {
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
    },
    /// Compare two or more embedding models over one corpus.
    ///
    /// Each cache is one model's vectors for the same corpus. The run refuses
    /// before it starts unless every cache agrees on the corpus digest, the chunk
    /// count and the query seed table, and unless every model's manifest still
    /// digests to what its cache was embedded against.
    GradeEmbedding {
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
    },
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
        "rrf" => Fusion::ReciprocalRank { k: inillucent_core::rank::RRF_K },
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
        .map(|s| s.parse::<bool>().map_err(|e| anyhow::anyhow!("{s} is not true or false: {e}")))
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
        .map(|s| s.parse::<f32>().map_err(|e| anyhow::anyhow!("{s} is not a number: {e}")))
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
            .map(|p| p.trim().parse::<f32>().map_err(|e| anyhow::anyhow!("{p} is not a number: {e}")))
            .collect::<Result<_>>()?;
        anyhow::ensure!(
            parts.len() == 4,
            "an adaptive rule needs four gains, oov:identifier:separation:coverage, got {rule}"
        );
        out.push(AdaptiveWeights {
            out_of_vocabulary_gain: parts[0],
            identifier_gain: parts[1],
            separation_gain: parts[2],
            coverage_gain: parts[3],
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
fn probe<T>(answered: anyhow::Result<T>) -> T {
    answered.expect("the probe vector comes from this index")
}

fn expand_home(path: &str) -> Result<String> {
    Ok(match path.strip_prefix("~/") {
        Some(rest) => format!("{}/{}", std::env::var("HOME")?, rest),
        None => path.to_string(),
    })
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let models_root = cli.models_root.clone().unwrap_or_else(models::default_models_root);
    match cli.command {
        Command::SynthBuild { derived, out, scale } => {
            let derived = derived.unwrap_or_else(synth::default_derived_dir);
            let report = synth::build(&derived, &out, scale)?;
            eprintln!(
                "\nwrote {} chunks across {} documents to {}",
                report.chunks,
                report.documents,
                out.display()
            );
            eprintln!("{:<12} {:>7} {:>8} {:>7}", "source", "docs", "chunks", "mean");
            for (source, docs, chunks, mean) in &report.per_source {
                eprintln!("{source:<12} {docs:>7} {chunks:>8} {mean:>7}");
            }
            eprintln!(
                "titles unique to one document: {}, of which usable as identity queries: {}",
                report.unique_titles, report.identity_usable
            );
        }
        Command::SynthCheck { corpus, per_source } => {
            synth::check(&corpus, per_source)?;
        }
        Command::SynthEmbed { corpus, model_dir, model_file, batch, report_every, devices, window_batches } => {
            let dir = expand_home(&model_dir)?;
            let devices = parse_devices(&devices)?;
            let model = models::resolve_dir(std::path::Path::new(&dir), &model_file)?;
            model.verify_files()?;
            synth::embed(
                &corpus,
                &cli.cache,
                &model,
                &scenarios::seeds(),
                &arm::ArmOptions {
                    batch_size: batch,
                    device: devices[0],
                    endpoint: cli.endpoint.clone(),
                    max_batch_cells: cli.max_batch_cells,
                    ..Default::default()
                },
                report_every,
                &devices,
                window_batches,
            )?;
        }
        Command::SynthLoad { corpus, no_indexes } => {
            let chunks = synth::read_corpus(&corpus)?;
            let c = corpus::load_cache(&cli.cache)?;
            synth::load_postgres(&cli.database_url, &chunks, &c, !no_indexes)?;
        }
        Command::Load { limit } => {
            eprintln!("loading corpus from {}", cli.database_url);
            let start = Instant::now();
            let c = corpus::load_from_postgres(&cli.database_url, limit)?;
            eprintln!(
                "loaded {} chunks at {} dimensions in {:.1}s",
                c.len(),
                c.dims,
                start.elapsed().as_secs_f64()
            );
            corpus::save_cache(&c, &cli.cache)?;
            let bytes = std::fs::metadata(&cli.cache)?.len();
            eprintln!("cached {} ({:.1} MB)", cli.cache.display(), bytes as f64 / 1e6);
        }
        Command::Build { limit, quantized } => {
            let c = corpus::load_cache(&cli.cache)?;
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
        }
        Command::Save { limit, quantized, dir } => {
            let c = corpus::load_cache(&cli.cache)?;
            let (index, _keys, stats, seconds) = scenarios::build_index(&c, limit, quantized)?;
            eprintln!("built {} chunks in {:.1}s", stats.chunks, seconds);
            let start = Instant::now();
            inillucent_core::persist::save(&index, &dir)?;
            eprintln!("saved to {} in {:.1}s", dir.display(), start.elapsed().as_secs_f64());
            let mut total = 0u64;
            for entry in std::fs::read_dir(&dir)? {
                let entry = entry?;
                let size = entry.metadata()?.len();
                eprintln!("  {:>12} {}", size, entry.file_name().to_string_lossy());
                total += size;
            }
            eprintln!("  {:>12} total ({:.1} MB)", total, total as f64 / 1e6);
        }
        Command::Open { dir } => {
            let start = Instant::now();
            let index = inillucent_core::persist::load(&dir)?;
            let load_seconds = start.elapsed().as_secs_f64();
            eprintln!(
                "loaded {} chunks / {} documents in {:.1}s",
                index.store().n_chunks(),
                index.store().n_documents(),
                load_seconds
            );

            // Exercise every query path so the reported memory reflects a process
            // that has actually served traffic, not one that only opened files.
            let query = index.vectors().copy_of(7);
            for (label, filter) in [
                ("no predicate", inillucent_core::filter::Filter::default()),
                ("source = slack", inillucent_core::filter::Filter::source("slack")),
            ] {
                let compiled = index.compile(&filter);
                let start = Instant::now();
                let hits = probe(index.vector_search(&query, &compiled, 10, None));
                let vector_ms = start.elapsed().as_secs_f64() * 1000.0;
                let start = Instant::now();
                let lexical = index.lexical_search("offer eligibility rules", &compiled, 10);
                let lexical_ms = start.elapsed().as_secs_f64() * 1000.0;
                let start = Instant::now();
                let hybrid = probe(index.hybrid_search("offer eligibility rules", &query, &compiled, 10, None));
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
        }
        Command::EmbedCheck { model_dir, model_file, samples, batch, device } => {
            let dir = model_dir.map(|d| expand_home(&d)).transpose()?;
            let c = corpus::load_cache(&cli.cache)?;
            embedcheck::run(
                &c,
                &models_root,
                dir.as_deref(),
                &model_file,
                samples,
                &arm::ArmOptions {
                    batch_size: batch,
                    device: Device::parse(&device)?,
                    endpoint: cli.endpoint.clone(),
                    max_batch_cells: cli.max_batch_cells,
                    ..Default::default()
                },
            )?;
        }
        Command::Tune {
            limit,
            per_source,
            model_dir,
            model_file,
            device,
            coverages,
            weights,
            proximities,
            prefixes,
            tiers,
            phrases,
            fusions,
            mmrs,
            adaptive,
            seed_offset,
            stats_seed,
        } => {
            let c = corpus::load_cache(&cli.cache)?;
            let dir = expand_home(&model_dir)?;
            // The arm the engine currently ships, spelled out so every other arm
            // in the sweep is compared against it rather than against whichever of
            // themselves happened to come first.
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
            let names: Vec<String> = fusions
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect();
            anyhow::ensure!(!names.is_empty(), "--fusions named no method");
            let settings = tune::build_settings(baseline, &tune::Sweep { coverages: &parse_floats(&coverages)?, weights: &parse_floats(&weights)?, proximities: &parse_floats(&proximities)?, prefixes: &parse_bools(&prefixes)?, tiers: &parse_bools(&tiers)?, phrases: &parse_floats(&phrases)?, fusions: &names, mmrs: &parse_floats(&mmrs)?, adaptive: &parse_adaptive(&adaptive)? });
            let model = models::resolve_dir(std::path::Path::new(&dir), &model_file)?;
            tune::run(
                &c,
                limit,
                per_source,
                &model,
                Device::parse(&device)?,
                &settings,
                seed_offset,
                stats_seed,
            )?;
        }
        Command::Grade {
            limit,
            per_source,
            model_dir,
            model_file,
            out,
            inillucent_only,
            fusion,
            vector_weight,
            lexical_coverage,
            lexical_proximity,
            lexical_prefix,
            lexical_tier,
            lexical_phrase,
            lexical_rescore_depth,
            adaptive_fusion,
            adaptive_oov_gain,
            adaptive_identifier_gain,
            adaptive_separation_gain,
            adaptive_coverage_gain,
            mmr_lambda,
            runs_dir,
            stats_seed,
            device,
        } => {
            let c = corpus::load_cache(&cli.cache)?;
            let dir = expand_home(&model_dir)?;
            let options = scenarios::GradeOptions {
                limit,
                per_source,
                arm_options: arm::ArmOptions {
                    device: Device::parse(&device)?,
                    endpoint: cli.endpoint.clone(),
                    max_batch_cells: cli.max_batch_cells,
                    ..Default::default()
                },
                model: models::resolve_dir(std::path::Path::new(&dir), &model_file)?,
                database_url: cli.database_url.clone(),
                inillucent_only,
                device: Device::parse(&device)?,
                fusion: parse_fusion(&fusion, vector_weight)?,
                lexical_coverage,
                lexical_proximity,
                lexical_prefix,
                lexical_tier,
                lexical_phrase,
                lexical_rescore_depth,
                adaptive_fusion,
                adaptive: AdaptiveWeights {
                    // The base is the same weight a fixed fusion would use, so
                    // every gain at zero reproduces that fusion exactly.
                    base: vector_weight,
                    out_of_vocabulary_gain: adaptive_oov_gain,
                    identifier_gain: adaptive_identifier_gain,
                    separation_gain: adaptive_separation_gain,
                    coverage_gain: adaptive_coverage_gain,
                    ..Default::default()
                },
                mmr_lambda,
                runs_dir,
                stats_seed,
                cache_path: cli.cache.clone(),
            };
            let card = scenarios::grade(&c, &options)?;
            let markdown = report::render(&card);
            std::fs::write(&out, markdown)?;
            // The measurements are also saved as JSON, so the card can be
            // re-rendered or re-judged without repaying the twenty minutes the
            // run costs.
            let json_path = out.with_extension("json");
            std::fs::write(&json_path, serde_json::to_string_pretty(&card)?)?;
            eprintln!(
                "score card written to {}, measurements to {}",
                out.display(),
                json_path.display()
            );
            report::print_summary(&card);
        }
        Command::GradeEmbedding {
            cache_set,
            baseline,
            limit,
            per_source,
            device,
            out,
            runs_dir,
            stats_seed,
            dense,
            hybrid,
            cost,
            matryoshka,
            abstention,
            lexical,
            cost_samples,
            cost_devices,
            cost_repeats,
            matryoshka_chunks,
        } => {
            let options = gradeembed::EmbeddingGradeOptions {
                caches: cache_set,
                models_root: models_root.clone(),
                baseline,
                limit,
                per_source,
                device: Device::parse(&device)?,
                runs_dir,
                out: out.clone(),
                stats_seed,
                dense,
                hybrid,
                cost,
                matryoshka,
                abstention,
                lexical,
                cost_samples,
                cost_devices: if cost { parse_devices(&cost_devices)? } else { Vec::new() },
                cost_repeats,
                matryoshka_chunks,
                arm_options: arm::ArmOptions {
                    device: Device::parse(&device)?,
                    endpoint: cli.endpoint.clone(),
                    max_batch_cells: cli.max_batch_cells,
                    ..Default::default()
                },
            };
            let card = gradeembed::run(&options)?;
            std::fs::write(&out, gradeembed::render(&card))?;
            let json_path = out.with_extension("json");
            std::fs::write(&json_path, serde_json::to_string_pretty(&card)?)?;
            eprintln!(
                "card written to {}, measurements to {}",
                out.display(),
                json_path.display()
            );
            gradeembed::print_summary(&card);
        }
        Command::ExportVectors {
            corpus,
            model_dir,
            samples,
            texts_out,
            vectors_out,
            device,
            batch,
            as_queries,
        } => {
            use inillucent_core::embed::Embedder;
            let dir = expand_home(&model_dir)?;
            let model = models::resolve_dir(std::path::Path::new(&dir), "model.onnx")?;
            model.verify_files()?;
            let chunks = synth::read_corpus(&corpus)?;
            let chosen = scenarios::strided_sample(chunks.len(), samples);
            let texts: Vec<String> = chosen
                .iter()
                .map(|&i| synth::sanitize_for_model(&chunks[i].content))
                .collect();
            eprintln!(
                "embedding {} chunks with {} as {}",
                texts.len(),
                model.manifest.id,
                if as_queries { "queries" } else { "documents" }
            );
            let embedder = inillucent_core::embed_onnx::OnnxEmbedder::open_manifest(
                &model.dir,
                &model.manifest,
                batch,
                Device::parse(&device)?,
            )?;
            let start = Instant::now();
            let vectors = if as_queries {
                let prefix = &model.manifest.prefixes.query;
                let prefixed: Vec<String> =
                    texts.iter().map(|t| format!("{prefix}{t}")).collect();
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

            use std::io::Write as _;
            let mut text_file = std::io::BufWriter::new(std::fs::File::create(&texts_out)?);
            for (row, (&i, text)) in chosen.iter().zip(&texts).enumerate() {
                let record = serde_json::json!({
                    "row": row,
                    "chunk": i,
                    "source": chunks[i].source,
                    "text": text,
                });
                writeln!(text_file, "{record}")?;
            }
            text_file.flush()?;

            let mut vector_file = std::io::BufWriter::new(std::fs::File::create(&vectors_out)?);
            for v in &vectors {
                for x in v {
                    vector_file.write_all(&x.to_le_bytes())?;
                }
            }
            vector_file.flush()?;
            eprintln!("wrote {} and {}", texts_out.display(), vectors_out.display());
        }
        Command::EmbedResidency {
            model_dir,
            optimized_dir,
            devices,
            repeats,
            steady,
            skip_optimized,
        } => {
            let dir = expand_home(&model_dir)?;
            let dir = std::path::Path::new(&dir);
            let model = models::resolve_dir(dir, "model.onnx")?;
            let optimized_dir = optimized_dir
                .unwrap_or_else(|| std::env::temp_dir().join("inillucent-optimized-graph"));
            if !skip_optimized {
                match residency::prepare_optimized(dir, &model.manifest, &optimized_dir) {
                    Ok(path) => eprintln!("optimized graph at {}", path.display()),
                    Err(err) => eprintln!("no pre-optimized arm: {err:#}"),
                }
            }
            let devices = parse_devices(&devices)?;
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
        }
        Command::Models { dir } => {
            let mut model = models::resolve_dir(&dir, "model.onnx")?;
            let before = model.digest();
            let path = model.seal()?;
            eprintln!(
                "sealed {} ({} -> {})",
                path.display(),
                corpus::short(&before),
                corpus::short(&model.digest())
            );
            eprintln!("  weights   {} {}", model.manifest.model_file, model.manifest.weights_sha256);
            eprintln!("  tokenizer tokenizer.json {}", model.manifest.tokenizer_sha256);
            eprintln!("  manifest  {}", model.digest());
        }
    }
    Ok(())
}
