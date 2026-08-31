//! The grading harness.
//!
//! Subcommands:
//!   synth-build  assemble the graded corpus from the downloaded public material
//!   synth-embed  embed that corpus and write the cache both engines read
//!   synth-load   load the corpus and its vectors into PostgreSQL for the baseline
//!   load         pull a corpus and its vectors out of PostgreSQL into a cache
//!   build        build a rust-db index from the cache and report what it built
//!   grade        run every scenario against both engines and write the score card

mod corpus;
mod embedcheck;
mod engine;
mod metrics;
mod queryset;
mod report;
mod scenarios;
mod tune;
mod synth;

use std::path::PathBuf;
use std::time::Instant;

use anyhow::Result;
use clap::{Parser, Subcommand};
use rustdb_core::embed_onnx::Device;
use rustdb_core::rank::Fusion;

/// The synthetic corpus database. `synth-load` creates and fills it, so this
/// default works for anybody who has run the setup steps in the README.
const DEFAULT_DB: &str = "postgres://127.0.0.1:5433/rustdb_synth";
/// Where the embedding model's weights live. The harness runs the model in
/// process, so there is no server to start.
const DEFAULT_MODEL_DIR: &str = "~/.cache/rust-db-models/nomic-embed-text-v1.5";

#[derive(Parser)]
#[command(name = "rustdb-bench", about = "Grade rust-db against postgres + pgvector")]
struct Cli {
    #[command(subcommand)]
    command: Command,

    #[arg(long, default_value = DEFAULT_DB, global = true)]
    database_url: String,

    #[arg(long, default_value = "corpus.cache", global = true)]
    cache: PathBuf,
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
        #[arg(long, default_value = "~/.cache/rust-db-models/nomic-embed-text-v1.5")]
        model_dir: String,
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
    /// Build a rust-db index from the cache and report build statistics.
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
        #[arg(long, default_value = "index.rustdb")]
        dir: PathBuf,
    },
    /// Open a saved index, run a few queries, and report what it cost.
    ///
    /// This is the measurement that describes a serving process: it holds the
    /// index and nothing else, where a build also holds the loader's copy of the
    /// corpus.
    Open {
        #[arg(long, default_value = "index.rustdb")]
        dir: PathBuf,
    },
    /// Check that the cache's vectors were made from the cache's text.
    EmbedCheck {
        #[arg(long, default_value = DEFAULT_MODEL_DIR)]
        model_dir: String,
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
        /// Added to the query set seeds, so a setting can be chosen on queries the
        /// graded run will not use. 0 uses the same queries `grade` does.
        #[arg(long, default_value_t = 100)]
        seed_offset: u64,
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
        /// `../rust-db-scorecard.md`, which from the repository root wrote it to the
        /// parent directory instead.
        #[arg(long, default_value = "rust-db-scorecard.md")]
        out: PathBuf,
        /// Processor the queries are embedded on: `cpu`, `cuda` or `cuda:N`.
        #[arg(long, default_value = "cpu")]
        device: String,
        /// How the vector and lexical lists are combined: `rrf`, `minmax` or
        /// `convex`. Applied to BOTH engines, so the hybrid family stays a
        /// measurement of retrieval rather than of ranking policy.
        #[arg(long, default_value = "minmax")]
        fusion: String,
        /// Weight on the vector side for the two score based fusions.
        #[arg(long, default_value_t = 0.35)]
        vector_weight: f32,
        /// Exponent on the share of the query a lexical hit contains. rust-db only:
        /// PostgreSQL already requires every term, so it has nothing to weight.
        #[arg(long, default_value_t = 3.0)]
        lexical_coverage: f32,
        /// How much of a lexical score is scaled by how tightly the matched query
        /// terms sit together. rust-db only: `ts_rank_cd` already does this.
        #[arg(long, default_value_t = 1.0)]
        lexical_proximity: f32,
        /// Whether a query term also matches the terms it prefixes.
        #[arg(long, default_value_t = false, action = clap::ArgAction::Set)]
        lexical_prefix: bool,
        /// Whether the count of matched query terms outranks the score.
        #[arg(long, default_value_t = false, action = clap::ArgAction::Set)]
        lexical_tier: bool,
        /// Skip the two pgvector configurations, for iterating on rust-db alone.
        #[arg(long, default_value_t = false)]
        rustdb_only: bool,
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
        "rrf" => Fusion::ReciprocalRank { k: rustdb_core::rank::RRF_K },
        "minmax" => Fusion::NormalizedScore { vector_weight },
        "convex" => Fusion::Convex { vector_weight },
        other => anyhow::bail!("unknown fusion {other}, expected rrf, minmax or convex"),
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

/// Expand a leading `~/` so a default path can name the home directory.
fn expand_home(path: &str) -> Result<String> {
    Ok(match path.strip_prefix("~/") {
        Some(rest) => format!("{}/{}", std::env::var("HOME")?, rest),
        None => path.to_string(),
    })
}

fn main() -> Result<()> {
    let cli = Cli::parse();
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
            synth::embed(&corpus, &cli.cache, &dir, &model_file, batch, report_every, &devices, window_batches)?;
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
            rustdb_core::persist::save(&index, &dir)?;
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
            let index = rustdb_core::persist::load(&dir)?;
            let load_seconds = start.elapsed().as_secs_f64();
            eprintln!(
                "loaded {} chunks / {} documents in {:.1}s",
                index.store().n_chunks(),
                index.store().n_documents(),
                load_seconds
            );

            // Exercise every query path so the reported memory reflects a process
            // that has actually served traffic, not one that only opened files.
            let query = index.vectors().get(7).to_vec();
            for (label, filter) in [
                ("no predicate", rustdb_core::filter::Filter::default()),
                ("source = slack", rustdb_core::filter::Filter::source("slack")),
            ] {
                let compiled = index.compile(&filter);
                let start = Instant::now();
                let hits = index.vector_search(&query, &compiled, 10, None);
                let vector_ms = start.elapsed().as_secs_f64() * 1000.0;
                let start = Instant::now();
                let lexical = index.lexical_search("offer eligibility rules", &compiled, 10);
                let lexical_ms = start.elapsed().as_secs_f64() * 1000.0;
                let start = Instant::now();
                let hybrid = index.hybrid_search("offer eligibility rules", &query, &compiled, 10, None);
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
            let dir = expand_home(&model_dir)?;
            let c = corpus::load_cache(&cli.cache)?;
            embedcheck::run(&c, &dir, &model_file, samples, batch, Device::parse(&device)?)?;
        }
        Command::Tune { limit, per_source, model_dir, model_file, device, coverages, weights, proximities, prefixes, tiers, seed_offset } => {
            let c = corpus::load_cache(&cli.cache)?;
            let dir = expand_home(&model_dir)?;
            tune::run(
                &c,
                limit,
                per_source,
                &dir,
                &model_file,
                Device::parse(&device)?,
                &parse_floats(&coverages)?,
                &parse_floats(&weights)?,
                &parse_floats(&proximities)?,
                &parse_bools(&prefixes)?,
                &parse_bools(&tiers)?,
                seed_offset,
            )?;
        }
        Command::Grade {
            limit,
            per_source,
            model_dir,
            model_file,
            out,
            rustdb_only,
            fusion,
            vector_weight,
            lexical_coverage,
            lexical_proximity,
            lexical_prefix,
            lexical_tier,
            device,
        } => {
            let c = corpus::load_cache(&cli.cache)?;
            let dir = expand_home(&model_dir)?;
            let card = scenarios::grade(
                &c,
                limit,
                per_source,
                &dir,
                &model_file,
                &cli.database_url,
                rustdb_only,
                Device::parse(&device)?,
                parse_fusion(&fusion, vector_weight)?,
                lexical_coverage,
                lexical_proximity,
                lexical_prefix,
                lexical_tier,
            )?;
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
    }
    Ok(())
}
