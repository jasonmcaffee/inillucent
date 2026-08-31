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
mod synth;

use std::path::PathBuf;
use std::time::Instant;

use anyhow::Result;
use clap::{Parser, Subcommand};

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
        /// Chunks re-embedded and compared, spread across the whole corpus.
        #[arg(long, default_value_t = 200)]
        samples: usize,
        #[arg(long, default_value_t = 16)]
        batch: usize,
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
        /// Skip the two pgvector configurations, for iterating on rust-db alone.
        #[arg(long, default_value_t = false)]
        rustdb_only: bool,
    },
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
        Command::SynthEmbed { corpus, model_dir, model_file, batch, report_every } => {
            let dir = expand_home(&model_dir)?;
            synth::embed(&corpus, &cli.cache, &dir, &model_file, batch, report_every)?;
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
        Command::EmbedCheck { model_dir, model_file, samples, batch } => {
            let dir = expand_home(&model_dir)?;
            let c = corpus::load_cache(&cli.cache)?;
            embedcheck::run(&c, &dir, &model_file, samples, batch)?;
        }
        Command::Grade {
            limit,
            per_source,
            model_dir,
            model_file,
            out,
            rustdb_only,
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
