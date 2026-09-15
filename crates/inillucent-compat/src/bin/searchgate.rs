//! The retrieval consumer's absolute cost, on the new engine's storage.
//!
//! Invariant: **one module, one corpus, one query set.** The tokenizer, the
//! BM25, the vector distances and the generation merging are the same code
//! this file has always measured; what changed is what it is measured
//! against.
//!
//! **This used to compare the new engine's PAX-tree storage against the old
//! engine's SQLite-b-tree storage for the same module, and it cannot any
//! more.** `inillucent-session`, the old engine's connection, was deleted along
//! with the rest of the retired engine (`inillucent-legacy`, `inillucent-capi`,
//! `inillucent-vm`) - nothing outside `inillucent-compat`'s own test and
//! profiling binaries named any of the four, so the workspace lost nothing
//! else, but it means there is no second store left to put beside this one. No
//! historical absolute-latency number for the old engine's retrieval consumer
//! was found recorded anywhere in this repository's docs, `compat/*.toml` or
//! `tests/performance-history.tsv` to gate against instead - the file's own
//! numbers were always a live two-arm ratio, never archived as a floor.
//!
//! **`main` still returns `ExitCode::FAILURE` on a real problem**, but a
//! two-store disagreement is no longer one of the things it can detect - that
//! is the actual change this file makes, and it is a change to what this
//! binary claims rather than a tidy-up. Correctness is still asserted rather
//! than assumed: every answer is checked against what the corpus construction
//! guarantees (a row names a document this corpus wrote, the count is within
//! the range the corpus could produce) and against itself (the same query
//! answers the same rows twice in a row). It does not try to predict an exact
//! row count from the corpus, because the tokenizer's own stemming turned out
//! to be a detail this file does not own an independent model of - see
//! [`QUERIES`].
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
//! This measures what that acceptance is *for*: how expensive the consumer is
//! on the store that actually ships. It is not a replacement for the pgvector
//! scorecard and does not claim to be one - that number is about ranking
//! quality against another product, and it is reported where it always was.
//!
//! ## Fairness
//!
//! The corpus is built once, and every round asks the same queries the same
//! number of times, so a number from one round is directly comparable to the
//! next - which is what makes recording this over time (rather than a single
//! run) the way to notice a regression without a second engine to catch it
//! live.
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

use inillucent_engine::connect::{Connection, Database};
use inillucent_tree::datum::OwnedDatum;

/// The queries every round puts to the store.
///
/// `"eligibility"` matching the phrase that says `"eligible"` (below) is why
/// the correctness check does not try to predict an exact row count from the
/// corpus construction: the tokenizer's own stemming is a detail this file
/// does not own an independent model of, and a hand-rolled model of it broke
/// on exactly this query the first time this file was run against the new
/// engine. What is checked instead is real and does not need one: every
/// result names a document this corpus actually wrote, the count is within
/// the range the corpus could produce, and the same query answers the same
/// rows twice in a row.
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

/// The phrase and title each document index cycles through.
const PHRASES: [&str; 6] = [
    "a member is eligible when the plan covers the service",
    "submit the claim within ninety days of the service date",
    "an appeal must be filed within sixty days of the denial",
    "the discount schedule applies to every eligible claim",
    "eligibility is decided by the plan and not by the service",
    "the denial notice states the appeal window and the schedule",
];

/// The title each document index cycles through.
const TITLES: [&str; 6] = [
    "eligibility rules",
    "claim submission",
    "appeal window",
    "discount schedule",
    "eligibility review",
    "denial notice",
];

/// Returns the statements that build and fill the corpus.
///
/// The bodies are drawn from a small phrase set so that terms repeat across
/// documents the way they do in a real corpus - a corpus of unique words gives
/// every query a single hit and measures nothing about ranking.
///
/// @param documents - how many documents to write
fn corpus(documents: usize) -> Vec<String> {
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

/// Checks a query's answer against what the corpus construction guarantees,
/// without needing to model the tokenizer's own stemming.
///
/// @param documents - how many documents the corpus holds
/// @param sql - the query, for the message
/// @param rows - the rows it returned
fn implausible(documents: usize, sql: &str, rows: &[Vec<String>]) -> Option<String> {
    if sql.contains("count(*)") {
        return match rows {
            [row] if row.len() == 1 && row[0] == documents.to_string() => None,
            _ => Some(format!(
                "{rows:?} is not exactly one row reading {documents}"
            )),
        };
    }
    if rows.len() > documents {
        return Some(format!(
            "{} rows is more than the {documents} documents there are",
            rows.len()
        ));
    }
    let valid_titles: Vec<String> = (0..documents)
        .map(|index| {
            format!(
                "{} {index}",
                TITLES.get(index % TITLES.len()).copied().unwrap_or("doc")
            )
        })
        .collect();
    for row in rows {
        let Some(title) = row.first() else {
            return Some("a row with no columns at all".to_string());
        };
        if !valid_titles.contains(title) {
            return Some(format!("`{title}` names no document this corpus wrote"));
        }
    }
    None
}

/// Runs the corpus and prints the report.
///
/// @param documents - how many documents to index
/// @param rounds - how many rounds to time
fn run(documents: usize, rounds: usize) -> Result<bool, String> {
    // **A corpus of nothing is a refusal, not a fast run (task-1961, T1).**
    // Every check below passed on an empty corpus: `count(*)` answered `0`,
    // which is exactly the number of documents there are, and every other
    // query answered no rows, which is not more rows than the corpus holds. So
    // `--documents 0` built an index with nothing in it, timed it, printed
    // "one pass of every query: 0.004 ms" and exited zero. There is no number
    // in that report about this engine.
    if documents == 0 {
        return Err(
            "a corpus of 0 documents measures nothing; pass --documents with a positive number"
                .to_string(),
        );
    }
    if rounds == 0 {
        return Err("0 rounds times nothing; pass --rounds with a positive number".to_string());
    }
    let area = std::env::temp_dir().join("inillucent-searchgate");
    let _ = std::fs::create_dir_all(&area);

    println!("## configuration");
    println!("  documents   : {documents}");
    println!("  rounds      : {rounds}");
    println!("  queries     : {}", QUERIES.len());
    println!(
        "  fairness    : one process, one corpus, the same query set every round; every \
         answer is checked before any clock is read"
    );

    let statements = corpus(documents);

    let path = area.join("docs.rdb");
    for suffix in ["", "-wal.0000000001"] {
        let mut name = path.clone().into_os_string();
        name.push(suffix);
        let _ = std::fs::remove_file(std::path::PathBuf::from(name));
    }
    let _ = std::fs::remove_file(&path);
    let database = Database::open(&path).map_err(|error| format!("open: {error}"))?;
    let connection = database.session();
    let build_started = Instant::now();
    for statement in &statements {
        connection
            .execute_batch(statement)
            .map_err(|error| format!("{statement}: {error}"))?;
    }
    let build_elapsed = build_started.elapsed().as_secs_f64() * 1e3;

    // The answers, checked before any clock is read: every result names a
    // document this corpus actually wrote, the count is within the range the
    // corpus could produce, and repeating the query answers the same rows.
    println!();
    println!("## answers");
    let mut correct = true;
    let mut found_any = false;
    for query in QUERIES {
        let rows = query_rows(&connection, query)?;
        let repeated = query_rows(&connection, query)?;
        if let Some(reason) = implausible(documents, query, &rows) {
            println!("  {:<58} WRONG - {reason}", query);
            correct = false;
        } else if rows != repeated {
            println!(
                "  {:<58} WRONG - answered differently the second time",
                query
            );
            correct = false;
        } else {
            found_any = found_any || !rows.is_empty();
            println!("  {:<58} plausible  {} rows", query, rows.len());
        }
    }
    if !correct {
        println!();
        println!("## verdict: a query's answer was not the query's own to give");
        return Ok(false);
    }
    // **At least one query has to have found something.** A corpus that built
    // but indexed nothing answers every query with no rows, and every one of
    // those answers is plausible by the test above: no rows is not more rows
    // than the corpus holds. The clock below would then time an index that
    // holds nothing, which is the shape `crates/inillucent-compat/tests/
    // gates_fail_closed.rs` exists to catch.
    if !found_any {
        return Err(format!(
            "no query found a single row over {documents} documents, so the index holds nothing and there is nothing to time"
        ));
    }

    // The clock.
    let mut total = 0.0f64;
    for _ in 0..rounds {
        total += time_pass(&connection)?;
    }
    let each = total / rounds.max(1) as f64;

    println!();
    println!("## result   (milliseconds)");
    println!("  {:<28} {:>10}", "", "cost");
    println!("  {:<28} {build_elapsed:>10.2}", "build the index");
    println!("  {:<28} {each:>10.3}", "one pass of every query");
    Ok(true)
}

/// Times one pass of every query.
///
/// @param connection - the connection the corpus was built on
fn time_pass(connection: &Connection<'_>) -> Result<f64, String> {
    let started = Instant::now();
    for query in QUERIES {
        let _ = query_rows(connection, query)?;
    }
    Ok(started.elapsed().as_secs_f64() * 1e3)
}

/// Returns every row of a query, rendered as text.
///
/// @param connection - the connection the corpus was built on
/// @param sql - the query
fn query_rows(connection: &Connection<'_>, sql: &str) -> Result<Vec<Vec<String>>, String> {
    let mut statement = connection
        .prepare(sql)
        .map_err(|error| format!("{sql}: {error}"))?;
    let mut out = Vec::new();
    while statement
        .step()
        .map_err(|error| format!("{sql}: {error}"))?
    {
        out.push(
            statement
                .row()
                .iter()
                .map(|value| match value {
                    OwnedDatum::Null => "NULL".to_string(),
                    OwnedDatum::Int(number) => number.to_string(),
                    OwnedDatum::Real(number) => number.to_string(),
                    OwnedDatum::Text(bytes) => String::from_utf8_lossy(bytes).into_owned(),
                    OwnedDatum::Blob(bytes) => {
                        bytes.iter().map(|byte| format!("{byte:02x}")).collect()
                    }
                })
                .collect(),
        );
    }
    Ok(out)
}
