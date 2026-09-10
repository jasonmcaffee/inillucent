//! The retrieval consumer on both stores, side by side.
//!
//! Invariant: **one module, one corpus, one query set, two stores.** The
//! tokenizer, the BM25, the vector distances and the generation merging are the
//! same code in both arms; the only difference is whether `inillucent_search`'s
//! five shadow tables are SQLite b-tree pages behind a pager or PAX trees in the
//! new engine. So a difference in what comes back is a bug, and a difference in
//! how long it takes is the storage change - which is the only thing a storage
//! change can be asked to show.
//!
//! ## Why this exists rather than `inillucent-bench grade`
//!
//! The TDD's Phase 5 acceptance names the retrieval scorecard, at "1.50x its
//! configured baseline". That scorecard grades `inillucent-core`'s ranking against
//! pgvector, and `inillucent-bench` depends on neither the search module nor either
//! engine - its manifest is `inillucent-core` plus postgres, pgvector and `ort`. The
//! store is not in its path, so re-running it cannot say anything about a
//! storage change, and `inillucent-core` is a crate the TDD's own triage lists as
//! "the retrieval algorithms are untouched".
//!
//! This measures what that acceptance is *for*: whether the consumer is slower
//! or worse on the new store. It is not a replacement for the pgvector
//! scorecard and does not claim to be one - that number is about ranking
//! quality against another product, and it is reported where it always was.
//!
//! ## Fairness
//!
//! Both arms are built in one process, from the same corpus, in the same order,
//! and are asked the same queries the same number of times, interleaved by
//! round so that a busy machine cannot favour whichever went first. The answers
//! are compared before any timing is reported: a number from an arm that
//! returned different rows is not a measurement of anything.
//!
//! Usage:
//!   inillucent-searchgate [--documents N] [--rounds N]

/// The engine's own allocator, installed for this program.
///
/// **Part of the build, not of a workload.** SQLite ships its own memory
/// subsystem and is compiled as one translation unit; a Rust workspace measured
/// on the platform allocator is being measured on a build configuration rather
/// than on an engine, which is the same reasoning that fixed fat LTO and one
/// codegen unit in the release profile. The Windows C runtime heap was
/// measured at 59% of a trivial compile and this size-classed free list at
/// 17% overall, which is why Phase 3's Part E names it the cheapest first move.
#[global_allocator]
static ALLOCATOR: inillucent_alloc::Pooled = inillucent_alloc::Pooled;

use std::process::ExitCode;
use std::time::Instant;

use inillucent_compat::newengine::ImportedDatabase;
use inillucent_compat::workspace_root;
use inillucent_exec::physical::Params;
use inillucent_session::connection::{Connection, OpenOptions, SessionDatabase};
use inillucent_session::statement::{execute_batch, Statement};
use inillucent_tree::datum::OwnedDatum;

/// The queries every round puts to both stores.
const QUERIES: [&str; 6] = [
    "SELECT title FROM docs WHERE docs MATCH 'eligibility' ORDER BY rank",
    "SELECT title FROM docs WHERE docs MATCH 'claim' ORDER BY rank",
    "SELECT title FROM docs WHERE docs MATCH 'service' ORDER BY rank",
    "SELECT title, body FROM docs WHERE docs MATCH 'denial' ORDER BY rank",
    "SELECT count(*) FROM docs",
    "SELECT title FROM docs WHERE docs MATCH 'schedule' ORDER BY rank",
];

fn main() -> ExitCode {
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    let documents = flag(&arguments, "--documents").unwrap_or(500);
    let rounds = flag(&arguments, "--rounds").unwrap_or(30);
    match run(documents, rounds) {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::FAILURE,
        Err(reason) => {
            eprintln!("{reason}");
            ExitCode::FAILURE
        }
    }
}

/// Returns the value of a `--name value` flag.
///
/// @param arguments - the command line
/// @param name - the flag
fn flag(arguments: &[String], name: &str) -> Option<usize> {
    let position = arguments.iter().position(|argument| argument == name)?;
    arguments.get(position.saturating_add(1))?.parse().ok()
}

/// Returns the statements that build and fill the corpus.
///
/// The bodies are drawn from a small phrase set so that terms repeat across
/// documents the way they do in a real corpus - a corpus of unique words gives
/// every query a single hit and measures nothing about ranking.
///
/// @param documents - how many documents to write
fn corpus(documents: usize) -> Vec<String> {
    const PHRASES: [&str; 6] = [
        "a member is eligible when the plan covers the service",
        "submit the claim within ninety days of the service date",
        "an appeal must be filed within sixty days of the denial",
        "the discount schedule applies to every eligible claim",
        "eligibility is decided by the plan and not by the service",
        "the denial notice states the appeal window and the schedule",
    ];
    const TITLES: [&str; 6] = [
        "eligibility rules",
        "claim submission",
        "appeal window",
        "discount schedule",
        "eligibility review",
        "denial notice",
    ];
    let mut out =
        vec!["CREATE VIRTUAL TABLE docs USING inillucent_search(title, body)".to_string()];
    for index in 0..documents {
        let title = TITLES.get(index % TITLES.len()).copied().unwrap_or("doc");
        let body = PHRASES
            .get(index % PHRASES.len())
            .copied()
            .unwrap_or("body");
        out.push(format!(
            "INSERT INTO docs(rowid, title, body) VALUES ({}, '{title} {index}', '{body}')",
            index.saturating_add(1)
        ));
    }
    out
}

/// Runs both arms and prints the comparison.
///
/// @param documents - how many documents each arm indexes
/// @param rounds - how many interleaved rounds to time
fn run(documents: usize, rounds: usize) -> Result<bool, String> {
    let area = workspace_root().join("_agent_output").join("searchgate");
    let _ = std::fs::create_dir_all(&area);

    println!("## configuration");
    println!("  documents   : {documents}");
    println!("  rounds      : {rounds}");
    println!("  queries     : {}", QUERIES.len());
    println!(
        "  fairness    : one process, one corpus, one query set, interleaved by round; \
         answers compared before any time is reported"
    );

    let statements = corpus(documents);

    // The old engine's arm.
    let old_path = area.join("old.db");
    for suffix in ["", "-journal", "-wal", "-shm"] {
        let _ = std::fs::remove_file(area.join(format!("old.db{suffix}")));
    }
    let old_database = SessionDatabase::open_with_options(
        &old_path,
        OpenOptions {
            busy_timeout: std::time::Duration::from_secs(5),
            ..OpenOptions::default()
        },
    )
    .map_err(|error| format!("the old engine: {}", error.message()))?;
    let old = old_database
        .connect()
        .map_err(|error| format!("the old engine: {}", error.message()))?;
    let old_build = Instant::now();
    for statement in &statements {
        execute_batch(&old, statement.as_bytes())
            .map_err(|error| format!("{statement}: {}", error.message()))?;
    }
    let old_build = old_build.elapsed().as_secs_f64() * 1e3;

    // The new engine's arm, over an empty SQLite file because import is the way
    // in until the connection is re-rooted.
    let source = workspace_root().join("compat/fixtures/empty-p4096-utf8.db");
    let new_path = area.join("new.db");
    let _ = std::fs::remove_file(&new_path);
    std::fs::copy(&source, &new_path)
        .map_err(|error| format!("the empty fixture could not be copied: {error}"))?;
    let mut new = ImportedDatabase::import(new_path, 32_768)
        .map_err(|error| format!("the new engine: {}", error.message()))?;
    let new_build = Instant::now();
    for statement in &statements {
        new.execute_any(statement, &Params::new())
            .map_err(|error| format!("{statement}: {:?}", error.detail()))?;
    }
    let new_build = new_build.elapsed().as_secs_f64() * 1e3;

    // The answers, before any clock is read.
    println!();
    println!("## answers");
    let mut agreed = true;
    for query in QUERIES {
        let theirs = old_rows(&old, query)?;
        let ours = new_rows(&mut new, query)?;
        if ours == theirs {
            println!("  {:<58} agree  {} rows", query, ours.len());
        } else {
            println!("  {:<58} DIFFER", query);
            agreed = false;
        }
    }
    if !agreed {
        println!();
        println!("## verdict: the two stores disagree, so no timing is reported");
        return Ok(false);
    }

    // The clock, interleaved.
    let mut old_total = 0.0f64;
    let mut new_total = 0.0f64;
    for round in 0..rounds {
        if round % 2 == 0 {
            old_total += time_old(&old)?;
            new_total += time_new(&mut new)?;
        } else {
            new_total += time_new(&mut new)?;
            old_total += time_old(&old)?;
        }
    }
    let old_each = old_total / rounds as f64;
    let new_each = new_total / rounds as f64;

    println!();
    println!("## result   (milliseconds)");
    println!("  {:<28} {:>10} {:>10} {:>9}", "", "old", "new", "ratio");
    println!(
        "  {:<28} {old_build:>10.2} {new_build:>10.2} {:>8.2}x",
        "build the index",
        old_build / new_build.max(f64::MIN_POSITIVE)
    );
    println!(
        "  {:<28} {old_each:>10.3} {new_each:>10.3} {:>8.2}x",
        "one pass of every query",
        old_each / new_each.max(f64::MIN_POSITIVE)
    );
    println!();
    println!("  ratio is old over new; above 1.00x means the new store is faster");
    Ok(true)
}

/// Times one pass of every query on the old engine.
///
/// @param connection - the old engine's connection
fn time_old(connection: &Connection) -> Result<f64, String> {
    let started = Instant::now();
    for query in QUERIES {
        let _ = old_rows(connection, query)?;
    }
    Ok(started.elapsed().as_secs_f64() * 1e3)
}

/// Times one pass of every query on the new engine.
///
/// @param engine - the new engine's database
fn time_new(engine: &mut ImportedDatabase) -> Result<f64, String> {
    let started = Instant::now();
    for query in QUERIES {
        let _ = new_rows(engine, query)?;
    }
    Ok(started.elapsed().as_secs_f64() * 1e3)
}

/// Returns every row of a query on the old engine, rendered as text.
///
/// @param connection - the old engine's connection
/// @param sql - the query
fn old_rows(connection: &Connection, sql: &str) -> Result<Vec<Vec<String>>, String> {
    let mut statement = Statement::prepare(connection, sql.as_bytes())
        .map_err(|error| format!("{sql}: {}", error.message()))?
        .0;
    let mut out = Vec::new();
    while statement
        .step()
        .map_err(|error| format!("{sql}: {}", error.message()))?
    {
        let width = statement.column_count();
        let mut row = Vec::with_capacity(width);
        for index in 0..width {
            row.push(match statement.value(index) {
                inillucent_value::Value::Null => "NULL".to_string(),
                inillucent_value::Value::Integer(number) => number.to_string(),
                inillucent_value::Value::Real(number) => number.to_string(),
                inillucent_value::Value::Text(text) => {
                    String::from_utf8_lossy(text.raw()).into_owned()
                }
                inillucent_value::Value::Blob(blob) => blob
                    .raw()
                    .iter()
                    .map(|byte| format!("{byte:02x}"))
                    .collect(),
            });
        }
        out.push(row);
    }
    Ok(out)
}

/// Returns every row of a query on the new engine, rendered the same way.
///
/// @param engine - the new engine's database
/// @param sql - the query
fn new_rows(engine: &mut ImportedDatabase, sql: &str) -> Result<Vec<Vec<String>>, String> {
    let answered = engine
        .execute_any(sql, &Params::new())
        .map_err(|error| format!("{sql}: {:?}", error.detail()))?;
    Ok(answered
        .rows
        .iter()
        .map(|row| {
            row.iter()
                .map(|value| match value {
                    OwnedDatum::Null => "NULL".to_string(),
                    OwnedDatum::Int(number) => number.to_string(),
                    OwnedDatum::Real(number) => number.to_string(),
                    OwnedDatum::Text(bytes) => String::from_utf8_lossy(bytes).into_owned(),
                    OwnedDatum::Blob(bytes) => {
                        bytes.iter().map(|byte| format!("{byte:02x}")).collect()
                    }
                })
                .collect()
        })
        .collect())
}
