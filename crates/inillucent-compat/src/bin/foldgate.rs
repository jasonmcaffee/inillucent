//! What an ordinary write pays to keep a vector index current.
//!
//! Invariant: **the two arms differ in one thing only - whether a commit folds
//! the delta log into the built generation or builds a new one from every
//! row.** Same corpus, same vectors, same commit boundaries, same generation
//! sizes, same query set, same process image. So a difference in insert latency
//! is the graph work and nothing else.
//!
//! ## What it is for
//!
//! M8 is the finding that an incremental vector insert rebuilt the
//! complete HNSW graph. It did: automatic compaction ran a single pass over
//! every row inside the committing transaction, and `docs/roadmap.md` recorded
//! the cost - 132.6 s over 185,078 passages, about nine and a half minutes over
//! 598,560. This measures the two behaviours side by side so the fix is a
//! number rather than a claim, and so the operating range published in
//! `docs/relational-architecture.md` has something under it.
//!
//! ## The arms
//!
//! - `fold` is the shipped behaviour. The table is declared with the default
//!   compaction threshold and a commit that crosses it folds.
//! - `build` reproduces what the module did before M8. The table is declared
//!   `compact = 0`, so nothing happens automatically, and the harness issues
//!   `INSERT INTO docs(docs) VALUES('compact')` at exactly the commits where
//!   the fold arm folds - the same trigger, the same generation sizes, the same
//!   number of generations written.
//!
//! Each arm runs in its own child process, because peak resident memory is a
//! high water mark for a whole process and two arms sharing one would report
//! the larger of them twice.
//!
//! **Runs on `inillucent-engine`, not the retired `inillucent-session`.** This
//! file used to open the old engine directly, which meant it was measuring
//! M8's fold-versus-rebuild behaviour over the old engine's storage rather
//! than the one that ships. `inillucent_search` is storage-agnostic - it reads
//! its shadow tables through `inillucent_sql::vtab::ShadowStore`, which each
//! engine implements over its own trees - so the module and the M8 fix are
//! unchanged; only which storage answers `docs`'s shadow tables changes here.
//!
//! Usage:
//!   inillucent-foldgate [--documents N] [--queries N] [--dims N]
//!   inillucent-foldgate --arm fold|build [...]   (one arm, machine readable)

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, Instant};

use inillucent_compat::procstat::{mebibytes, ProcessCost};
use inillucent_compat::workspace_root;
use inillucent_engine::connect::{Connection, Database};
use inillucent_tree::datum::OwnedDatum;

/// The lexical queries the reader thread and the recovery check both run.
const QUERIES: [&str; 4] = [
    "SELECT rowid FROM docs WHERE docs MATCH 'eligible account' AND k = 10 ORDER BY rank",
    "SELECT rowid FROM docs WHERE docs MATCH 'appeal window' AND k = 10 ORDER BY rank",
    "SELECT rowid FROM docs WHERE docs MATCH 'discount schedule' AND k = 10 ORDER BY rank",
    "SELECT rowid FROM docs WHERE docs MATCH 'denial notice' AND k = 10 ORDER BY rank",
];

/// How long the concurrent reader waits between passes of the query set.
const READ_INTERVAL: Duration = Duration::from_millis(50);

/// The phrases a document's body is drawn from.
const PHRASES: [&str; 6] = [
    "a member is eligible when the plan covers the service",
    "submit the claim within ninety days of the service date",
    "an appeal must be filed within sixty days of the denial",
    "the discount schedule applies to every eligible account",
    "eligibility is decided by the plan and not by the service",
    "the denial notice states the appeal window and the schedule",
];

/// One arm's measurements, in the order they are reported.
#[derive(Default)]
struct Arm {
    /// Every commit's wall time, in milliseconds.
    commits: Vec<f64>,
    /// How many chunks every generation build inserted into the graph.
    inserted_total: i64,
    /// The most chunks any one generation build inserted.
    inserted_max: i64,
    /// How many generations the arm published.
    generations: i64,
    /// How many chunks the final generation holds, live and tombstoned.
    chunks: i64,
    /// Mean recall at ten of the approximate search against the exhaustive one.
    recall: f64,
    /// The database file, in bytes, before it was closed.
    database_bytes: u64,
    /// The write ahead log, in bytes, before it was closed.
    wal_bytes: u64,
    /// Peak resident memory for the whole arm, in bytes.
    peak_bytes: u64,
    /// How long reopening and answering the query set took, in milliseconds.
    recovery_millis: f64,
    /// Whether the reopened database answered exactly what the arm saw.
    recovered: bool,
    /// Every concurrent read's wall time, in milliseconds.
    reads: Vec<f64>,
    /// How many concurrent reads were refused because a writer held the file.
    refused: usize,
}

fn main() -> ExitCode {
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    let documents = flag(&arguments, "--documents").unwrap_or(20_000);
    let queries = flag(&arguments, "--queries").unwrap_or(32);
    let dims = flag(&arguments, "--dims").unwrap_or(64);
    let fixed = flag(&arguments, "--compact");
    let arm = text_flag(&arguments, "--arm");
    let outcome = match arm.as_deref() {
        Some(name) => run_one(name, documents, queries, dims, fixed).map(|arm| {
            report(&arm);
            true
        }),
        None => compare(documents, queries, dims, fixed),
    };
    match outcome {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::FAILURE,
        Err(reason) => {
            eprintln!("{reason}");
            ExitCode::FAILURE
        }
    }
}

/// Returns the value of a `--name value` flag as a number.
///
/// @param arguments - the command line
/// @param name - the flag
fn flag(arguments: &[String], name: &str) -> Option<usize> {
    let position = arguments.iter().position(|argument| argument == name)?;
    arguments.get(position.saturating_add(1))?.parse().ok()
}

/// Returns the value of a `--name value` flag as text.
///
/// @param arguments - the command line
/// @param name - the flag
fn text_flag(arguments: &[String], name: &str) -> Option<String> {
    let position = arguments.iter().position(|argument| argument == name)?;
    arguments.get(position.saturating_add(1)).cloned()
}

/// Runs both arms as child processes and prints the comparison.
///
/// @param documents - how many rows each arm writes
/// @param queries - how many vector queries the recall estimate averages
/// @param dims - how wide a vector is
/// @param fixed - a delta log length to pin, or the default rule
fn compare(
    documents: usize,
    queries: usize,
    dims: usize,
    fixed: Option<usize>,
) -> Result<bool, String> {
    println!("## configuration");
    println!("  documents   : {documents}");
    println!("  dimensions  : {dims}");
    println!("  queries     : {queries}");
    println!("  commits     : one row, one transaction");
    match fixed {
        Some(length) => println!("  delta log   : pinned at {length} by `compact = {length}`"),
        None => println!(
            "  delta log   : the default rule, max(1024, rows / 8), so it grows with the corpus"
        ),
    }
    println!(
        "  arms        : `fold` is the shipped behaviour; `build` reproduces \
         the single-pass rebuild M8 replaced, at the same trigger points"
    );
    println!();

    let fold = spawn("fold", documents, queries, dims, fixed)?;
    let build = spawn("build", documents, queries, dims, fixed)?;
    print_table(&fold, &build);
    Ok(verdict(&fold, &build))
}

/// Runs one arm in a child process and reads its measurements back.
///
/// @param name - the arm to run
/// @param documents - how many rows to write
/// @param queries - how many vector queries to average recall over
/// @param dims - how wide a vector is
fn spawn(
    name: &str,
    documents: usize,
    queries: usize,
    dims: usize,
    fixed: Option<usize>,
) -> Result<Arm, String> {
    let program =
        std::env::current_exe().map_err(|error| format!("this program has no path: {error}"))?;
    let mut command = std::process::Command::new(program);
    command
        .args(["--arm", name])
        .args(["--documents", &documents.to_string()])
        .args(["--queries", &queries.to_string()])
        .args(["--dims", &dims.to_string()]);
    if let Some(length) = fixed {
        command.args(["--compact", &length.to_string()]);
    }
    let output = command
        .output()
        .map_err(|error| format!("the {name} arm did not start: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "the {name} arm failed: {}",
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    parse(name, &String::from_utf8_lossy(&output.stdout))
}

/// Rebuilds an arm from the `key = value` lines it printed.
///
/// @param name - the arm's name
/// @param text - what the child printed
fn parse(name: &str, text: &str) -> Result<Arm, String> {
    let mut arm = Arm::default();
    for line in text.lines() {
        let Some((key, value)) = line.split_once(" = ") else {
            continue;
        };
        let value = value.trim();
        match key.trim() {
            "commits" => arm.commits = numbers(value),
            "reads" => arm.reads = numbers(value),
            "inserted_total" => arm.inserted_total = value.parse().unwrap_or(0),
            "inserted_max" => arm.inserted_max = value.parse().unwrap_or(0),
            "generations" => arm.generations = value.parse().unwrap_or(0),
            "chunks" => arm.chunks = value.parse().unwrap_or(0),
            "recall" => arm.recall = value.parse().unwrap_or(0.0),
            "database_bytes" => arm.database_bytes = value.parse().unwrap_or(0),
            "wal_bytes" => arm.wal_bytes = value.parse().unwrap_or(0),
            "peak_bytes" => arm.peak_bytes = value.parse().unwrap_or(0),
            "recovery_millis" => arm.recovery_millis = value.parse().unwrap_or(0.0),
            "recovered" => arm.recovered = value == "true",
            "refused" => arm.refused = value.parse().unwrap_or(0),
            _ => {}
        }
    }
    if arm.commits.is_empty() {
        return Err(format!("the {name} arm reported no commits"));
    }
    Ok(arm)
}

/// Parses a comma separated list of numbers.
fn numbers(text: &str) -> Vec<f64> {
    text.split(',')
        .filter_map(|part| part.trim().parse::<f64>().ok())
        .collect()
}

/// Prints the arm's measurements as `key = value` lines for the parent.
///
/// @param arm - what the arm measured
fn report(arm: &Arm) {
    let commits: Vec<String> = arm
        .commits
        .iter()
        .map(|value| format!("{value:.4}"))
        .collect();
    let reads: Vec<String> = arm
        .reads
        .iter()
        .map(|value| format!("{value:.4}"))
        .collect();
    println!("commits = {}", commits.join(","));
    println!("reads = {}", reads.join(","));
    println!("inserted_total = {}", arm.inserted_total);
    println!("inserted_max = {}", arm.inserted_max);
    println!("generations = {}", arm.generations);
    println!("chunks = {}", arm.chunks);
    println!("recall = {:.6}", arm.recall);
    println!("database_bytes = {}", arm.database_bytes);
    println!("wal_bytes = {}", arm.wal_bytes);
    println!("peak_bytes = {}", arm.peak_bytes);
    println!("recovery_millis = {:.4}", arm.recovery_millis);
    println!("recovered = {}", arm.recovered);
    println!("refused = {}", arm.refused);
}

/// Returns the value at a percentile of a set of readings.
///
/// @param readings - the readings, in any order
/// @param share - the percentile, from 0 to 1
fn percentile(readings: &[f64], share: f64) -> f64 {
    if readings.is_empty() {
        return 0.0;
    }
    let mut sorted = readings.to_vec();
    sorted.sort_by(|left, right| left.partial_cmp(right).unwrap_or(std::cmp::Ordering::Equal));
    let last = sorted.len().saturating_sub(1);
    let position = ((last as f64) * share).round() as usize;
    sorted.get(position.min(last)).copied().unwrap_or(0.0)
}

/// Prints the two arms side by side.
///
/// @param fold - the shipped behaviour
/// @param build - the behaviour M8 replaced
fn print_table(fold: &Arm, build: &Arm) {
    println!("## insert latency, milliseconds per commit");
    row(
        "p50",
        percentile(&fold.commits, 0.50),
        percentile(&build.commits, 0.50),
    );
    row(
        "p99",
        percentile(&fold.commits, 0.99),
        percentile(&build.commits, 0.99),
    );
    row(
        "max",
        percentile(&fold.commits, 1.00),
        percentile(&build.commits, 1.00),
    );
    println!();
    println!("## graph work");
    row(
        "chunks inserted",
        fold.inserted_total as f64,
        build.inserted_total as f64,
    );
    row(
        "worst one build",
        fold.inserted_max as f64,
        build.inserted_max as f64,
    );
    row(
        "generations",
        fold.generations as f64,
        build.generations as f64,
    );
    row("chunks resident", fold.chunks as f64, build.chunks as f64);
    println!();
    println!("## quality, memory and the file");
    row("recall at 10", fold.recall, build.recall);
    row(
        "peak MiB",
        mebibytes(fold.peak_bytes),
        mebibytes(build.peak_bytes),
    );
    row(
        "database MiB",
        mebibytes(fold.database_bytes),
        mebibytes(build.database_bytes),
    );
    row(
        "log MiB",
        mebibytes(fold.wal_bytes),
        mebibytes(build.wal_bytes),
    );
    println!();
    println!("## recovery and concurrent reads");
    row(
        "reopen and answer ms",
        fold.recovery_millis,
        build.recovery_millis,
    );
    println!(
        "  {:<22} {:>14} {:>14}",
        "answers agree", fold.recovered, build.recovered
    );
    row(
        "reads served",
        fold.reads.len() as f64,
        build.reads.len() as f64,
    );
    row(
        "read p50 ms",
        percentile(&fold.reads, 0.50),
        percentile(&build.reads, 0.50),
    );
    row(
        "read max ms",
        percentile(&fold.reads, 1.00),
        percentile(&build.reads, 1.00),
    );
    row("reads refused", fold.refused as f64, build.refused as f64);
    println!();
}

/// Prints one labelled pair.
///
/// @param label - what the number is
/// @param fold - the shipped behaviour's number
/// @param build - the replaced behaviour's number
fn row(label: &str, fold: f64, build: f64) {
    println!("  {label:<22} {fold:>14.3} {build:>14.3}");
}

/// Says whether the run supports the claim M8 makes.
///
/// Two things have to hold, and neither is a stopwatch threshold. The fold arm
/// must insert strictly fewer chunks than the build arm, which is the bound
/// itself and is a count. And both arms must have recovered, because a faster
/// arm that came back different is not a faster arm.
///
/// @param fold - the shipped behaviour
/// @param build - the behaviour M8 replaced
fn verdict(fold: &Arm, build: &Arm) -> bool {
    let mut ok = true;
    if fold.inserted_total >= build.inserted_total {
        println!(
            "## verdict: FAIL - folding inserted {} chunks against the rebuild's {}",
            fold.inserted_total, build.inserted_total
        );
        ok = false;
    }
    if fold.inserted_max >= build.inserted_max {
        println!(
            "## verdict: FAIL - folding's worst build was {} chunks against the rebuild's {}",
            fold.inserted_max, build.inserted_max
        );
        ok = false;
    }
    if !fold.recovered || !build.recovered {
        println!("## verdict: FAIL - an arm did not answer the same way after reopening");
        ok = false;
    }
    if ok {
        println!(
            "## verdict: folding inserts {} chunks where the rebuild inserts {}, \
             its worst single build is {} against {}, and both answer the same \
             after reopening",
            fold.inserted_total, build.inserted_total, fold.inserted_max, build.inserted_max
        );
    }
    ok
}

/// Runs one arm end to end.
///
/// @param name - `fold` or `build`
/// @param documents - how many rows to write
/// @param queries - how many vector queries to average recall over
/// @param dims - how wide a vector is
/// @param fixed - a delta log length to pin, or the default rule
fn run_one(
    name: &str,
    documents: usize,
    queries: usize,
    dims: usize,
    fixed: Option<usize>,
) -> Result<Arm, String> {
    let folding = match name {
        "fold" => true,
        "build" => false,
        other => return Err(format!("no such arm: {other}")),
    };
    let path = area().join(format!("{name}.db"));
    for suffix in ["", "-journal", "-wal", "-shm"] {
        let _ = std::fs::remove_file(area().join(format!("{name}.db{suffix}")));
    }
    let mut arm = Arm::default();
    let expected = {
        let database = open(&path)?;
        let connection = database.session();
        set_busy_timeout(&connection)?;
        declare(&connection, folding, dims, fixed)?;
        let reader = start_reader(&path);
        write_corpus(&connection, &mut arm, documents, dims, folding, fixed)?;
        let (reads, refused) = reader.stop();
        arm.reads = reads;
        arm.refused = refused;
        arm.recall = measure_recall(&connection, queries, dims, documents)?;
        arm.generations = state(&connection, "generation")?;
        arm.chunks = state(&connection, "chunks")?;
        arm.database_bytes = bytes_of(&path);
        arm.wal_bytes = bytes_of(&path.with_extension("db-wal"));
        answers(&connection)?
    };
    let started = Instant::now();
    let database = open(&path)?;
    let connection = database.session();
    let after = answers(&connection)?;
    arm.recovery_millis = started.elapsed().as_secs_f64() * 1e3;
    arm.recovered = after == expected;
    arm.peak_bytes = ProcessCost::now().peak_working_set;
    Ok(arm)
}

/// Returns where this gate's databases live.
fn area() -> PathBuf {
    let directory = workspace_root().join("_agent_output").join("foldgate");
    let _ = std::fs::create_dir_all(&directory);
    directory
}

/// Opens a database.
///
/// @param path - the file to open
fn open(path: &Path) -> Result<Database, String> {
    Database::open(path).map_err(|error| error.message().to_string())
}

/// Sets a connection's busy timeout long enough for a reader to wait out a
/// writer holding the file.
///
/// @param connection - the connection
fn set_busy_timeout(connection: &Connection<'_>) -> Result<(), String> {
    connection
        .execute_batch("PRAGMA busy_timeout = 30000")
        .map_err(|error| error.message().to_string())
}

/// Creates the arm's search table.
///
/// The two declarations differ in one clause. `compact = 0` turns automatic
/// compaction off entirely, which is how the build arm gets to decide for
/// itself when a generation is published.
///
/// **`mode = 'approximate'` is not optional here.** A search table defaults to
/// exact, which sets `exhaustive_below` to the largest number there is, and an
/// exhaustive scan finds the true neighbours whatever the graph looks like - so
/// a recall figure measured on a default table is 1.000 by construction and
/// says nothing about the graph the fold produced.
///
/// @param connection - the database
/// @param folding - whether this is the fold arm
/// @param dims - how wide a vector is
/// @param fixed - a delta log length to pin, or the default rule
fn declare(
    connection: &Connection<'_>,
    folding: bool,
    dims: usize,
    fixed: Option<usize>,
) -> Result<(), String> {
    let clause = match (folding, fixed) {
        (false, _) => ", compact = 0".to_string(),
        (true, Some(length)) => format!(", compact = {length}"),
        (true, None) => String::new(),
    };
    run(
        connection,
        &format!(
            "CREATE VIRTUAL TABLE docs USING inillucent_search(\
             title, body, dims = {dims}, mode = 'approximate'{clause})"
        ),
    )
}

/// Writes the corpus one row per transaction, timing every commit.
///
/// The build arm compacts wherever the fold arm folds, which is what makes the
/// two comparable: the same generations are written at the same sizes, and the
/// only difference left is whether the graph inside them was folded or built.
///
/// @param connection - the database
/// @param arm - where the commit times are recorded
/// @param documents - how many rows to write
/// @param dims - how wide a vector is
/// @param folding - whether the module publishes generations by itself
/// @param fixed - a delta log length to pin, or the default rule
fn write_corpus(
    connection: &Connection<'_>,
    arm: &mut Arm,
    documents: usize,
    dims: usize,
    folding: bool,
    fixed: Option<usize>,
) -> Result<(), String> {
    let mut pending = 0u64;
    for index in 0..documents {
        let id = index.saturating_add(1);
        let title = format!("document {id}");
        let body = PHRASES
            .get(index % PHRASES.len())
            .copied()
            .unwrap_or("body");
        let vector = vector_for(id, dims);
        let insert = format!(
            "INSERT INTO docs(rowid, title, body, vector) VALUES ({id}, '{title}', '{body} {id}', x'{}')",
            hex(&vector)
        );
        let started = Instant::now();
        run(connection, &insert)?;
        pending = pending.saturating_add(1);
        let published = pending >= threshold(id as u64, fixed);
        if published && !folding {
            run(connection, "INSERT INTO docs(docs) VALUES ('compact')")?;
        }
        arm.commits.push(started.elapsed().as_secs_f64() * 1e3);
        if published {
            pending = 0;
            let inserted = state(connection, "inserted")?;
            arm.inserted_total = arm.inserted_total.saturating_add(inserted);
            arm.inserted_max = arm.inserted_max.max(inserted);
        }
    }
    Ok(())
}

/// Returns the delta count at which the module publishes a generation.
///
/// The same arithmetic `Options::compact_threshold` uses, restated here because
/// the build arm has to reach the trigger points the fold arm reaches without
/// the module's help.
///
/// @param rows - how many rows the table holds
/// @param fixed - a delta log length to pin, or the default rule
fn threshold(rows: u64, fixed: Option<usize>) -> u64 {
    match fixed {
        Some(length) => (length as u64).max(1),
        None => 1024u64.max(rows / 8),
    }
}

/// Returns the vector one document carries.
///
/// A cheap deterministic generator, so both arms index the same corpus and a
/// re-run of the gate measures the same thing. The values are spread over the
/// unit cube rather than clustered, which is the hardest case for a graph and
/// therefore the one worth reporting.
///
/// @param id - the document's rowid
/// @param dims - how wide a vector is
fn vector_for(id: usize, dims: usize) -> Vec<f32> {
    let mut seed = (id as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15).max(1);
    let mut out = Vec::with_capacity(dims);
    for _ in 0..dims {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        out.push(((seed >> 40) as f32 / 16_777_216.0) - 0.5);
    }
    out
}

/// Renders a vector as the hexadecimal an `x'...'` literal takes.
///
/// @param vector - the vector
fn hex(vector: &[f32]) -> String {
    let mut out = String::with_capacity(vector.len() * 8);
    for value in vector {
        for byte in value.to_le_bytes() {
            out.push_str(&format!("{byte:02x}"));
        }
    }
    out
}

/// Returns how well the approximate vector search finds the exact neighbours.
///
/// Both sides come from the same index in the same process: `recall = 1.0` is
/// the module's own way of asking for the exhaustive comparison, so the
/// difference between the two lists is the graph and nothing else.
///
/// @param connection - the database
/// @param queries - how many query vectors to average over
/// @param dims - how wide a vector is
/// @param documents - how many rows the table holds
fn measure_recall(
    connection: &Connection<'_>,
    queries: usize,
    dims: usize,
    documents: usize,
) -> Result<f64, String> {
    if queries == 0 || documents == 0 {
        return Ok(0.0);
    }
    let mut total = 0.0f64;
    for query in 0..queries {
        let seed = documents
            .saturating_mul(7)
            .saturating_add(query.saturating_mul(1_009));
        let literal = hex(&vector_for(seed, dims));
        let approximate = first_column(
            connection,
            &format!("SELECT rowid FROM docs WHERE vector = x'{literal}' AND k = 10"),
        )?;
        let exact = first_column(
            connection,
            &format!(
                "SELECT rowid FROM docs WHERE vector = x'{literal}' AND k = 10 AND recall = 1.0"
            ),
        )?;
        if exact.is_empty() {
            continue;
        }
        let found = approximate.iter().filter(|id| exact.contains(id)).count();
        total += found as f64 / exact.len() as f64;
    }
    Ok(total / queries as f64)
}

/// Returns what the lexical query set answers, as one comparable list.
///
/// @param connection - the database
fn answers(connection: &Connection<'_>) -> Result<Vec<Vec<String>>, String> {
    let mut out = Vec::new();
    for query in QUERIES {
        out.push(first_column(connection, query)?);
    }
    Ok(out)
}

/// Returns the integer a `%_state` row holds.
///
/// @param connection - the database
/// @param key - the state key
fn state(connection: &Connection<'_>, key: &str) -> Result<i64, String> {
    let rows = first_column(
        connection,
        &format!("SELECT v FROM docs_state WHERE k = '{key}'"),
    )?;
    Ok(rows.first().and_then(|text| text.parse().ok()).unwrap_or(0))
}

/// Runs a statement for its effect.
///
/// @param connection - the database
/// @param sql - the statement
fn run(connection: &Connection<'_>, sql: &str) -> Result<(), String> {
    connection.execute_batch(sql).map_err(|error| {
        let head: String = sql.chars().take(96).collect();
        format!("{head}: {}", error.message())
    })
}

/// Returns the first column of every row a query answers, rendered as text.
///
/// @param connection - the database
/// @param sql - the query
fn first_column(connection: &Connection<'_>, sql: &str) -> Result<Vec<String>, String> {
    let mut statement = connection
        .prepare(sql)
        .map_err(|error| format!("{sql}: {}", error.message()))?;
    let mut out = Vec::new();
    while statement
        .step()
        .map_err(|error| format!("{sql}: {}", error.message()))?
    {
        out.push(match statement.row().first() {
            None | Some(OwnedDatum::Null) => "NULL".to_string(),
            Some(OwnedDatum::Int(number)) => number.to_string(),
            Some(OwnedDatum::Real(number)) => format!("{number:.6}"),
            Some(OwnedDatum::Text(text)) => String::from_utf8_lossy(text).into_owned(),
            Some(OwnedDatum::Blob(blob)) => format!("blob:{}", blob.len()),
        });
    }
    Ok(out)
}

/// Returns a file's size, or zero when it is not there.
///
/// @param path - the file
fn bytes_of(path: &Path) -> u64 {
    std::fs::metadata(path).map(|data| data.len()).unwrap_or(0)
}

/// A reader running against the same file while the writer works.
struct Reader {
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    handle: Option<std::thread::JoinHandle<(Vec<f64>, usize)>>,
}

impl Reader {
    /// Stops the reader and returns what it measured.
    fn stop(mut self) -> (Vec<f64>, usize) {
        self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
        match self.handle.take() {
            Some(handle) => handle.join().unwrap_or_default(),
            None => (Vec::new(), 0),
        }
    }
}

/// Starts a reader on its own connection to the same database file.
///
/// It is a second connection rather than a second statement on the writer's,
/// because the question is what a concurrent reader sees while a write is
/// publishing a generation - and a reader sharing the writer's connection is
/// not concurrent with it.
///
/// It pauses between passes on purpose. Every query rebuilds the merged index
/// from the published generation and the delta log, so a reader spinning as
/// fast as it can is a load generator, and what it would then measure is
/// itself. One pass every [`READ_INTERVAL`] leaves the writer the machine and
/// still puts a reader inside every generation publish.
///
/// @param path - the database file
fn start_reader(path: &Path) -> Reader {
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let flag = stop.clone();
    let owned = path.to_path_buf();
    let handle = std::thread::spawn(move || read_until(&owned, &flag));
    Reader {
        stop,
        handle: Some(handle),
    }
}

/// Reads the query set until it is told to stop, timing each query.
///
/// @param path - the database file
/// @param flag - set when the writer has finished
fn read_until(path: &Path, flag: &std::sync::atomic::AtomicBool) -> (Vec<f64>, usize) {
    let mut times = Vec::new();
    let mut refused = 0usize;
    // Held as the `Database` alone, with a fresh `Connection` borrowed from it
    // every query, rather than as a `(Database, Connection)` pair: the new
    // engine's `Connection<'d>` borrows the `Database` it came from, so the two
    // cannot be held together in one local without the struct borrowing from
    // itself. `connect()` is cheap - a session number - so re-deriving it each
    // pass costs nothing this measurement cares about.
    let mut held: Option<Database> = None;
    while !flag.load(std::sync::atomic::Ordering::Relaxed) {
        if held.is_none() {
            held = open(path).ok().and_then(|database| {
                let connection = database.session();
                set_busy_timeout(&connection).ok()?;
                Some(database)
            });
        }
        let Some(database) = held.as_ref() else {
            refused = refused.saturating_add(1);
            std::thread::sleep(READ_INTERVAL);
            continue;
        };
        let connection = database.session();
        for query in QUERIES {
            if flag.load(std::sync::atomic::Ordering::Relaxed) {
                break;
            }
            let started = Instant::now();
            match first_column(&connection, query) {
                Ok(_) => times.push(started.elapsed().as_secs_f64() * 1e3),
                Err(_) => {
                    refused = refused.saturating_add(1);
                    held = None;
                    break;
                }
            }
        }
        std::thread::sleep(READ_INTERVAL);
    }
    (times, refused)
}
