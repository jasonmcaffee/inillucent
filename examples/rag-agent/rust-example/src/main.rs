//! `rag-server`: an MCP server that answers questions from an inillucent database.
//!
//! Four commands:
//!
//! | Command | What it does |
//! |---|---|
//! | `serve` | syncs the corpus into the database, keeps syncing on a timer, and answers MCP requests on stdin and stdout |
//! | `sync` | runs one sync and prints its report |
//! | `search` | runs one search and prints the hits, which is the `search` tool without an agent |
//! | `evaluate` | asks the questions in `questions.json` in every mode and prints how often each found the right article |
//!
//! The modules, in the order a question travels through them:
//!
//! ```text
//! mcp.rs        reads the request from stdin
//! tools.rs      decodes the tool call
//! search.rs     embeds the question and ranks chunks
//! store.rs      runs the SQL against inillucent
//! ```
//!
//! and the order a document travels through them:
//!
//! ```text
//! scheduler.rs  decides when to sync
//! sync.rs       compares the source with the database
//! corpus.rs     reads the source documents
//! chunker.rs    cuts each document into overlapping chunks
//! store.rs      embeds each chunk and writes the document in one transaction
//! ```

mod chunker;
mod clock;
mod config;
mod corpus;
mod evaluate;
mod mcp;
mod scheduler;
mod search;
mod store;
mod sync;
mod tools;

use std::path::PathBuf;
use std::sync::Mutex;

use clap::{Args, Parser, Subcommand};

use crate::config::{parse_interval, ChunkSettings, ContextMode, IndexSettings};
use crate::scheduler::Scheduler;
use crate::search::{search, Mode, SearchRequest};
use crate::store::Store;
use crate::tools::Tools;

/// The command line.
#[derive(Parser)]
#[command(name = "rag-server", version, about = "An MCP server that searches Greek and Roman philosophy in an inillucent database")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

/// The settings every command shares.
#[derive(Args, Clone)]
struct IndexArgs {
    /// The database file. It is created, with its schema, when it does not exist.
    #[arg(long, default_value = "data/greek-philosophy.rdb")]
    db: PathBuf,
    /// The source documents: a JSONL file with title, url and text on each line, or a folder of .md and .txt files.
    #[arg(long, default_value = "../corpus/greek-philosophy.jsonl")]
    corpus: PathBuf,
    /// What is written in front of each chunk before it is embedded.
    #[arg(long, value_enum, default_value = "title")]
    context: ContextMode,
    /// The length a chunk is packed up to, in bytes of whole sentences.
    #[arg(long, default_value_t = 1000)]
    chunk_chars: usize,
    /// How much of each chunk the next one repeats, in bytes. 0 turns overlap off.
    #[arg(long, default_value_t = 200)]
    overlap_chars: usize,
    /// When the embedding model stays in memory: resident, on-demand, idle, or idle:<time> such as idle:10m.
    #[arg(long)]
    residency: Option<String>,
}

/// The four commands.
#[derive(Subcommand)]
enum Command {
    /// Serve MCP on stdin and stdout, syncing the corpus at start and on a timer.
    Serve {
        #[command(flatten)]
        index: IndexArgs,
        /// How often to sync: 90s, 15m, 2h, or off.
        #[arg(long, default_value = "15m")]
        sync_every: String,
        /// The mode `search` uses when the agent names none.
        #[arg(long, value_enum, default_value = "rrf")]
        mode: Mode,
        /// Run one search at start, so the first question does not wait for the model or the file.
        #[arg(long)]
        warm: bool,
    },
    /// Run one sync and print its report.
    Sync {
        #[command(flatten)]
        index: IndexArgs,
    },
    /// Run one search and print the hits as JSON.
    Search {
        /// The question.
        query: String,
        #[command(flatten)]
        index: IndexArgs,
        #[arg(long, value_enum, default_value = "rrf")]
        mode: Mode,
        #[arg(long, default_value_t = 5)]
        k: usize,
        /// Only search the document with this title.
        #[arg(long)]
        title: Option<String>,
    },
    /// Ask every question in a questions file in every mode, and print a table.
    Evaluate {
        #[command(flatten)]
        index: IndexArgs,
        #[arg(long, default_value = "questions.json")]
        questions: PathBuf,
        #[arg(long, default_value_t = 5)]
        k: usize,
        /// Print every question's result as JSON after the table.
        #[arg(long)]
        details: bool,
    },
}

/// Runs the command and exits with 1 and a message when it fails.
fn main() {
    let cli = Cli::parse();
    if let Err(error) = run(cli.command) {
        eprintln!("rag-server: {error}");
        std::process::exit(1);
    }
}

/// Runs one command.
///
/// @param command - the parsed command line
fn run(command: Command) -> Result<(), String> {
    match command {
        Command::Serve { index, sync_every, mode, warm } => serve(&index, &sync_every, mode, warm),
        Command::Sync { index } => {
            let store = open(&index)?;
            let report = sync::run_sync(&store, &index.corpus, &settings(&index), &Mutex::default())?;
            print_json(&report)
        }
        Command::Search { query, index, mode, k, title } => {
            let store = open(&index)?;
            print_json(&search(&store, &SearchRequest { query, mode, k, title })?)
        }
        Command::Evaluate { index, questions, k, details } => {
            let store = open(&index)?;
            let questions = evaluate::read_questions(&questions)?;
            let summaries = evaluate::evaluate(&store, &questions, &Mode::ALL, k)?;
            println!("{}", evaluate::markdown_table(&summaries));
            if details {
                print_json(&summaries)?;
            }
            Ok(())
        }
    }
}

/// Starts the sync thread and answers MCP requests until stdin closes.
///
/// @param index - the shared settings
/// @param sync_every - the sync interval as written on the command line
/// @param mode - the default search mode
/// @param warm - whether to run one search before the first request
fn serve(index: &IndexArgs, sync_every: &str, mode: Mode, warm: bool) -> Result<(), String> {
    let interval = parse_interval(sync_every)?;
    let store = open(index)?;
    if warm {
        warm_up(&store)?;
    }
    let scheduler = Scheduler::start(store.clone(), index.corpus.clone(), settings(index), interval);
    eprintln!("rag-server: serving {} from {}", index.db.display(), index.corpus.display());
    let tools = Tools { store, scheduler, default_mode: mode };
    mcp::serve(&tools).map_err(|error| format!("stdin or stdout failed: {error}"))
}

/// Runs one search in every mode so the first real question is fast.
///
/// The first search in a process pays twice: about 750 ms to load the model,
/// and about 950 ms to read the stored vectors and the keyword index from the
/// file for the first time. Loading the model alone saves only the first. A
/// search in each mode touches everything a later search reads.
///
/// @param store - the database
fn warm_up(store: &Store) -> Result<(), String> {
    for mode in Mode::ALL {
        search(store, &SearchRequest { query: "warm up".to_string(), mode, k: 1, title: None })?;
    }
    Ok(())
}

/// Opens the database, after setting the model's residency if one was given.
///
/// `INILLUCENT_EMBED_RESIDENCY` is read when the model is first loaded, so it
/// is set here, before any embedding and before any other thread starts.
///
/// @param index - the shared settings
fn open(index: &IndexArgs) -> Result<Store, String> {
    if let Some(residency) = &index.residency {
        std::env::set_var("INILLUCENT_EMBED_RESIDENCY", residency);
    }
    Store::open(&index.db)
}

/// Builds the index settings from the command line.
///
/// @param index - the shared settings
fn settings(index: &IndexArgs) -> IndexSettings {
    let chunking = ChunkSettings {
        target_chars: index.chunk_chars,
        overlap_chars: index.overlap_chars,
        max_chars: index.chunk_chars * 2,
        min_chars: index.chunk_chars / 4,
    };
    IndexSettings { chunking, context: index.context }
}

/// Prints a value as indented JSON.
///
/// @param value - anything serde can write
fn print_json<T: serde::Serialize>(value: &T) -> Result<(), String> {
    println!("{}", serde_json::to_string_pretty(value).map_err(|error| error.to_string())?);
    Ok(())
}
