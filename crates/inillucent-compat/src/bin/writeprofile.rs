//! Where a write actually spends its time, in counters rather than guesses.
//!
//! Invariant: this reports what the pool did, not what anybody thinks it did.
//! The scorecard can report a structural difference between this engine and
//! the reference on a single-row `UPDATE` inside an open transaction, and a
//! large ratio is a structural problem rather than a constant factor - so the
//! first question is not "which line is slow" but "how much work is being
//! done". Pages read, cache hits and pages written answer that without a
//! profiler, and they are the numbers an optimisation has to move.
//!
//! **"Page images copied" and "pages allocated" are gone from this report.**
//! They read `inillucent_legacy::Connection::pager_counters()`, which does not
//! exist on the new engine: `Database::cache_stats()` reports `hits`, `misses`,
//! `rewarms`, `cooled`, `evicted`, `reads` and `writes` - a cache's-eye view of
//! the pool rather than the old pager's own image-copy and allocation
//! counters - and nothing in `inillucent-pool` counts a before-image copy or a
//! fresh page allocation the way the old one did. `reads`, `hits` and `writes`
//! below are the three that carried over.
//!
//! Usage: `cargo run --release -p inillucent-compat --bin inillucent-writeprofile [-- --sweep | --spread <rows> <pool MiB>]`

use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Instant;

use inillucent_compat::workspace_root;
use inillucent_engine::connect::Database;
use inillucent_tree::datum::OwnedDatum;

/// How many rows the probe table holds.
const ROWS: i64 = 5_000;

/// How many statements each case runs.
const OPERATIONS: i64 = 500;

/// The secondary index counts the index count sweep runs at.
///
/// **Two is one point on a curve, and the gate's fixture only has that one.**
/// `main_table` carries two secondary indexes, so a change to how an index leaf
/// absorbs writes measured on it says nothing about whether it gets better or
/// worse with the index count - and whether the gain grows with the indexes is
/// what decides if a change to the page format is worth one (task-2066 section
/// 4.3.9, task-2074).
const SWEEP_INDEXES: [usize; 4] = [0, 2, 5, 10];

/// How many rows the sweep's table holds before the timed inserts.
///
/// A hundred thousand, which is `main_table` at the gate's medium scale, so an
/// index leaf holds the three to four thousand entries a real one does.
const SWEEP_SEEDED: i64 = 100_000;

/// How many rows the sweep inserts in its one timed transaction.
const SWEEP_INSERTS: i64 = 5_000;

/// How many times each arm of the sweep is run.
///
/// The arms are interleaved - every index count once, then every index count
/// again - so a machine that got busy halfway through slows all four.
const SWEEP_ROUNDS: usize = 5;

/// The multipliers that scatter each indexed column's values.
///
/// Distinct primes, so the ten columns are ten different orders over the same
/// rows and an insert lands in a different leaf of each index.
const SWEEP_PRIMES: [i64; 10] = [
    7_919, 104_729, 1_299_709, 15_485_863, 3, 31, 541, 7_907, 65_537, 999_983,
];

/// Runs every case and prints what each one cost.
///
/// `--sweep` runs the index count sweep alone, which is what a before and after
/// of a leaf format change wants: the other cases take a minute and measure
/// something else. `--spread <rows> <pool MiB>` runs the every other row
/// delete of `story_large_table_nightly` alone (task-2077), with the pool
/// defaulting to that story's 8 MiB.
fn main() -> ExitCode {
    let mut arguments: Vec<String> = std::env::args().skip(1).collect();
    // **Pinned before anything is timed, and the mask printed (task-2085).**
    // Its index count sweep feeds `docs/performance.md`, and an unpinned run
    // on a hybrid processor measures whichever core class the scheduler chose.
    if let Err(reason) = inillucent_compat::affinity::pin_from_arguments(&mut arguments) {
        eprintln!("{reason}");
        return ExitCode::FAILURE;
    }
    let sweep_only = arguments.iter().any(|argument| argument == "--sweep");
    let numbers: Vec<usize> = arguments
        .iter()
        .skip_while(|argument| *argument != "--spread")
        .skip(1)
        .filter_map(|number| number.parse().ok())
        .collect();
    let spread = numbers
        .first()
        .map(|rows| (*rows as i64, numbers.get(1).copied().unwrap_or(8)));
    let outcome = match (spread, sweep_only) {
        (Some((rows, pool_mib)), _) => spread_sweep(rows, pool_mib),
        (None, true) => index_sweep(),
        (None, false) => run(),
    };
    match outcome {
        Ok(()) => ExitCode::SUCCESS,
        Err(reason) => {
            eprintln!("{reason}");
            ExitCode::FAILURE
        }
    }
}

/// Returns a fresh database path.
fn scratch(name: &str) -> PathBuf {
    let directory = workspace_root().join("_agent_output/writeprofile");
    let _ = std::fs::create_dir_all(&directory);
    let path = directory.join(format!("{name}.db"));
    inillucent_base::testing::remove_database(&path);
    path
}

/// Builds the probe table.
fn build(path: &std::path::Path, indexes: bool) -> Result<Database, String> {
    let database = Database::open(path).map_err(|error| error.message().to_string())?;
    let connection = database.session();
    let mut script = String::from(
        "PRAGMA page_size=4096; PRAGMA journal_mode=delete; PRAGMA synchronous=full;
         CREATE TABLE digits(n INTEGER PRIMARY KEY);
         INSERT INTO digits(n) VALUES (0),(1),(2),(3),(4),(5),(6),(7),(8),(9);
         CREATE TABLE t(id INTEGER PRIMARY KEY, k INTEGER, c INTEGER, label TEXT);",
    );
    if indexes {
        script.push_str("CREATE INDEX t_k ON t(k); CREATE INDEX t_c ON t(c, k);");
    }
    script.push_str(&format!(
        "INSERT INTO t(id,k,c,label) SELECT seq, (seq*7)%{ROWS}, seq%64, 'row ' || seq \
         FROM (SELECT ((a.n*10+b.n)*10+c.n)*10+d.n+1 AS seq \
         FROM digits a, digits b, digits c, digits d) WHERE seq <= {ROWS};"
    ));
    connection
        .execute_batch(&script)
        .map_err(|error| error.message().to_string())?;
    Ok(database)
}

/// Builds a table of `rows` rows with one text column and nothing else.
///
/// The shape `tests/inillucent-testing-tdd.md` §8 measured: no indexes, one
/// text column, keys 1..=rows.
///
/// @param path - where the database goes
/// @param rows - how many rows to write
fn build_plain(path: &std::path::Path, rows: u32, page_size: usize) -> Result<Database, String> {
    let database = Database::open_at(path, page_size, inillucent_engine::DEFAULT_FRAMES)
        .map_err(|error| error.message().to_string())?;
    let connection = database.session();
    connection
        .execute_batch(
            "PRAGMA journal_mode=delete; PRAGMA synchronous=full;
             CREATE TABLE digits(n INTEGER PRIMARY KEY);
             INSERT INTO digits(n) VALUES (0),(1),(2),(3),(4),(5),(6),(7),(8),(9);
             CREATE TABLE t(id INTEGER PRIMARY KEY, label TEXT);",
        )
        .map_err(|error| error.message().to_string())?;
    connection
        .execute_batch(&format!(
            "INSERT INTO t(id,label) SELECT seq, 'label ' || seq              FROM (SELECT ((a.n*10+b.n)*10+c.n)*10+d.n+1 AS seq              FROM digits a, digits b, digits c, digits d) WHERE seq <= {rows};"
        ))
        .map_err(|error| error.message().to_string())?;
    Ok(database)
}

/// Times `DELETE FROM t` over a whole table, and says what the pool did.
///
/// **The shape section 4.3.7 of task-2066 is about, which nothing measured.**
/// The cases above delete one row at a time and cost 2.0 to 8.4 microseconds
/// each. A whole table deleted in key order costs 5.8 ms at 2,000 rows, 366 at
/// 4,000 and 1,156 at 8,000 - the row count quadruples and the time grows
/// nearly two hundred fold. The last attempt at it moved 2,075 ms to 1,695 and
/// was reverted, because an 18% improvement where a decisive one was predicted
/// means the model was wrong, and the model had been reasoned rather than
/// measured.
///
/// The counters are what makes the growth attributable: a cost that is linear
/// in the rows and a cost that is quadratic in the rows per leaf look the same
/// on a stopwatch and different here.
///
/// @param rows - how many rows the table holds
fn whole_table_delete(rows: u32, page_size: usize) -> Result<(), String> {
    let path = scratch(&format!("delete-whole-{page_size}-{rows}"));
    let database = build_plain(&path, rows, page_size)?;
    let connection = database.session();
    let before = database.cache_stats();
    let started = std::time::Instant::now();
    let changed = connection
        .execute("DELETE FROM t")
        .map_err(|error| error.message().to_string())?;
    let elapsed = started.elapsed();
    let after = database.cache_stats();
    let pages = match connection
        .query("PRAGMA page_count")
        .ok()
        .and_then(|rows| rows.first().and_then(|row| row.first().cloned()))
    {
        Some(inillucent_tree::datum::OwnedDatum::Int(count)) => count,
        _ => 0,
    };
    println!(
        "delete.whole page {page_size:>6} rows {rows:>6}  {:>9.1} ms  deleted {changed:>6}  pages {pages:>4}           reads {:>7}  hits {:>9}  writes {:>7}",
        elapsed.as_secs_f64() * 1e3,
        after.reads.saturating_sub(before.reads),
        after.hits.saturating_sub(before.hits),
        after.writes.saturating_sub(before.writes),
    );
    Ok(())
}

/// Returns a fresh database path in a directory of its own, for one spread arm.
///
/// A directory removed whole rather than a file: a database is a file plus log
/// segments named after it, and a segment left by an earlier run of the same
/// arm beside a fresh file would be read as part of it. `scratch` above removes
/// the file and three suffixes, which does not cover the segments.
///
/// @param tag - the arm, so two arms never share a file
fn spread_scratch(tag: &str) -> PathBuf {
    let directory = workspace_root()
        .join("_agent_output/writeprofile")
        .join(tag);
    let _ = std::fs::remove_dir_all(&directory);
    let _ = std::fs::create_dir_all(&directory);
    directory.join("spread.db")
}

/// Builds the table `story_large_table_nightly` deletes half of, at one page
/// size and one pool.
///
/// The same columns, the same values and the same index created before the
/// load, written five hundred rows a statement in one transaction. The pool is
/// opened at its default frame count and narrowed with `PRAGMA cache_size`,
/// which is how the story narrows it.
///
/// @param path - where the database goes
/// @param rows - how many rows to write
/// @param page_size - the page size the file is built at
/// @param pool_bytes - how many bytes `PRAGMA cache_size` lets the pool keep
/// @param indexed - whether `big_d ON big(d)` exists
fn build_big(
    path: &std::path::Path,
    rows: i64,
    page_size: usize,
    pool_bytes: usize,
    indexed: bool,
) -> Result<Database, String> {
    let database = Database::open_at(path, page_size, inillucent_engine::DEFAULT_FRAMES)
        .map_err(|error| error.message().to_string())?;
    let connection = database.session();
    let mut script = format!(
        "PRAGMA cache_size = -{}; CREATE TABLE big(a INTEGER PRIMARY KEY, b TEXT, d INTEGER);",
        pool_bytes / 1024
    );
    if indexed {
        script.push_str("CREATE INDEX big_d ON big(d);");
    }
    script.push_str("BEGIN;");
    connection
        .execute_batch(&script)
        .map_err(|error| error.message().to_string())?;
    let mut at = 1i64;
    while at <= rows {
        let end = (at + 499).min(rows);
        let values: Vec<String> = (at..=end)
            .map(|key| {
                let d = match key % 7 {
                    0 => "NULL".to_string(),
                    _ => format!("{}", key % 1013),
                };
                format!("({key}, 'row-{key}-padding-padding', {d})")
            })
            .collect();
        connection
            .execute_batch(&format!("INSERT INTO big VALUES {}", values.join(", ")))
            .map_err(|error| error.message().to_string())?;
        at = end + 1;
    }
    connection
        .execute_batch("COMMIT")
        .map_err(|error| error.message().to_string())?;
    Ok(database)
}

/// Returns the plan the engine chose for a statement, one step per line joined.
///
/// @param connection - the database
/// @param sql - the statement
fn plan_of(connection: &inillucent_engine::connect::Connection<'_>, sql: &str) -> String {
    let rows = connection
        .query(&format!("EXPLAIN QUERY PLAN {sql}"))
        .unwrap_or_default();
    rows.iter()
        .filter_map(|row| match row.last() {
            Some(OwnedDatum::Text(text)) => Some(String::from_utf8_lossy(text).to_string()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join(" / ")
}

/// Times `DELETE FROM big WHERE a % 2 = 0`, the delete task-2077 is about, and
/// prints the cost per deleted row beside what the pool and the tree did.
///
/// **Two pools, because they separate two costs.** A pool that holds the whole
/// file takes page traffic out of the comparison, so any difference left
/// between the page sizes is work done inside a leaf. A pool the file is past
/// adds the traffic back. task-2077 found the first equal at both sizes (2.4
/// and 2.1 microseconds a row, 5.5 and 5.6 with the index) and the second
/// 176,503 reads at 32 KiB against 9,875 at 4 KiB, which is what named the
/// cause: the delete visited the table in the order the covering index
/// returned the keys.
///
/// @param rows - how many rows the table holds
/// @param page_size - the page size
/// @param pool_bytes - how many bytes the pool keeps
/// @param indexed - whether the table carries `big_d`
fn spread_delete(
    rows: i64,
    page_size: usize,
    pool_bytes: usize,
    indexed: bool,
) -> Result<(), String> {
    let tag = format!(
        "spread-{page_size}-{rows}-{}-{}",
        pool_bytes >> 20,
        u8::from(indexed)
    );
    let path = spread_scratch(&tag);
    let database = build_big(&path, rows, page_size, pool_bytes, indexed)?;
    let connection = database.session();
    let sql = "DELETE FROM big WHERE a % 2 = 0";
    let plan = plan_of(&connection, sql);
    let before = database.cache_stats();
    let stats_before = database.write_stats();
    let started = Instant::now();
    // Prepared and stepped, which is how `story_large_table_nightly` and an
    // application run it; `execute` reaches the delete by another path.
    let mut statement = connection
        .prepare(sql)
        .map_err(|error| error.message().to_string())?;
    while statement
        .step()
        .map_err(|error| error.message().to_string())?
    {}
    drop(statement);
    let changed = connection
        .changes()
        .map_err(|error| error.message().to_string())?;
    let elapsed = started.elapsed();
    let after = database.cache_stats();
    let stats = subtract_stats(database.write_stats(), stats_before);
    let per_row = elapsed.as_secs_f64() * 1e6 / (changed.max(1) as f64);
    println!(
        "spread page {page_size:>6} rows {rows:>7} pool {:>4} MiB index {}  delete {:>8.1} ms  {per_row:>7.2} us/row  \
         reads {:>7}  hits {:>9}  writes {:>7}  cooled {:>7}  evicted {:>7}  rewarms {:>7}  merges {:>4}  [{plan}]",
        pool_bytes >> 20,
        u8::from(indexed),
        elapsed.as_secs_f64() * 1e3,
        after.reads.saturating_sub(before.reads),
        after.hits.saturating_sub(before.hits),
        after.writes.saturating_sub(before.writes),
        after.cooled.saturating_sub(before.cooled),
        after.evicted.saturating_sub(before.evicted),
        after.rewarms.saturating_sub(before.rewarms),
        stats.merges,
    );
    Ok(())
}

/// Runs the spread delete at both page sizes, with and without the index, in a
/// pool that holds the file and in one the file is past.
///
/// @param rows - how many rows each table holds
/// @param past_mib - the pool the table is past, in mebibytes
fn spread_sweep(rows: i64, past_mib: usize) -> Result<(), String> {
    for pool_mib in [512usize, past_mib] {
        for indexed in [false, true] {
            for page_size in [4_096usize, 32_768] {
                spread_delete(rows, page_size, pool_mib << 20, indexed)?;
            }
        }
    }
    Ok(())
}

/// Runs one case and prints its counters.
fn case(name: &str, indexes: bool, sql: &str, binds: usize) -> Result<(), String> {
    let path = scratch(name);
    let database = build(&path, indexes)?;
    let connection = database.session();
    let before = database.cache_stats();
    let mut statement = connection
        .prepare(sql)
        .map_err(|error| format!("{sql}: {}", error.message()))?;
    connection
        .execute_batch("BEGIN")
        .map_err(|error| error.message().to_string())?;
    let started = Instant::now();
    for index in 0..OPERATIONS {
        let row = 1 + (index * 7) % ROWS;
        statement
            .bind_integer(1, row)
            .map_err(|error| error.message().to_string())?;
        if binds > 1 {
            statement
                .bind_text(2, "replacement text for the row")
                .map_err(|error| error.message().to_string())?;
        }
        while statement
            .step()
            .map_err(|error| format!("{sql}: {}", error.message()))?
        {}
        statement.reset();
    }
    let elapsed = started.elapsed();
    drop(statement);
    connection
        .execute_batch("COMMIT")
        .map_err(|error| error.message().to_string())?;
    let after = database.cache_stats();
    let per = elapsed.as_secs_f64() * 1e6 / OPERATIONS as f64;
    println!(
        "{name:<28} {per:>9.1} us/op  reads {:>7}  hits {:>8}  writes {:>6}",
        after.reads - before.reads,
        after.hits - before.hits,
        after.writes - before.writes,
    );
    Ok(())
}

/// Runs a read case, for the contrast.
fn read_case(name: &str, indexes: bool, sql: &str) -> Result<(), String> {
    let path = scratch(name);
    let database = build(&path, indexes)?;
    let connection = database.session();
    let before = database.cache_stats();
    let mut statement = connection
        .prepare(sql)
        .map_err(|error| format!("{sql}: {}", error.message()))?;
    let started = Instant::now();
    let mut rows = 0u64;
    for index in 0..OPERATIONS {
        let row = 1 + (index * 7) % ROWS;
        statement
            .bind_integer(1, row)
            .map_err(|error| error.message().to_string())?;
        while statement
            .step()
            .map_err(|error| format!("{sql}: {}", error.message()))?
        {
            rows += 1;
            let _ = statement.row().first().map(|value| match value {
                OwnedDatum::Int(number) => *number,
                _ => 0,
            });
        }
        statement.reset();
    }
    let elapsed = started.elapsed();
    let after = database.cache_stats();
    let per = elapsed.as_secs_f64() * 1e6 / OPERATIONS as f64;
    println!(
        "{name:<28} {per:>9.1} us/op  reads {:>7}  hits {:>8}  rows {:>6}",
        after.reads - before.reads,
        after.hits - before.hits,
        rows
    );
    Ok(())
}

/// Returns the value the sweep writes into indexed column `column` of row `seq`.
///
/// @param seq - the row's id
/// @param column - which of the ten columns
fn sweep_value(seq: i64, column: usize) -> i64 {
    let prime = SWEEP_PRIMES.get(column).copied().unwrap_or(1);
    seq.wrapping_mul(prime).rem_euclid(SWEEP_SEEDED)
}

/// Builds the sweep's table: ten integer columns, `indexes` of them indexed.
///
/// The index is created **after** the rows are loaded, so each one is bulk
/// built the way an imported fixture's is, and the timed inserts meet leaves at
/// the fill a bulk build leaves rather than the fill a run of inserts does.
///
/// @param path - where the database goes
/// @param indexes - how many secondary indexes to create
fn build_sweep(path: &std::path::Path, indexes: usize) -> Result<Database, String> {
    let database = Database::open(path).map_err(|error| error.message().to_string())?;
    let connection = database.session();
    let columns: Vec<String> = (0..SWEEP_PRIMES.len())
        .map(|column| format!("c{column} INTEGER"))
        .collect();
    let values: Vec<String> = (0..SWEEP_PRIMES.len())
        .map(|column| {
            let prime = SWEEP_PRIMES.get(column).copied().unwrap_or(1);
            format!("(seq * {prime}) % {SWEEP_SEEDED}")
        })
        .collect();
    let mut script = format!(
        "PRAGMA journal_mode=delete; PRAGMA synchronous=full;
         CREATE TABLE digits(n INTEGER PRIMARY KEY);
         INSERT INTO digits(n) VALUES (0),(1),(2),(3),(4),(5),(6),(7),(8),(9);
         CREATE TABLE t(id INTEGER PRIMARY KEY, {});
         INSERT INTO t SELECT seq, {} FROM (SELECT (((a.n*10+b.n)*10+c.n)*10+d.n)*10+e.n+1 AS seq \
         FROM digits a, digits b, digits c, digits d, digits e) WHERE seq <= {SWEEP_SEEDED};",
        columns.join(", "),
        values.join(", "),
    );
    for column in 0..indexes {
        script.push_str(&format!("CREATE INDEX t_c{column} ON t(c{column});"));
    }
    connection
        .execute_batch(&script)
        .map_err(|error| error.message().to_string())?;
    Ok(database)
}

/// What one arm of the sweep cost.
struct SweepArm {
    /// Microseconds per inserted row, one entry per round.
    per_row: Vec<f64>,
    /// What the write path did, from the last round. Counters do not move
    /// between rounds, because every round starts from the same fresh table.
    stats: inillucent_tree::write::WriteStats,
    /// Log bytes the transaction wrote, from the last round.
    log_bytes: u64,
    /// Pages the file holds after the commit, from the last round.
    pages: i64,
}

/// Runs one round of one arm: a fresh table, then the timed inserts.
///
/// @param indexes - how many secondary indexes the table carries
/// @param round - which round, for the scratch file's name
/// @returns microseconds per row, the write counters, log bytes and page count
fn sweep_round(
    indexes: usize,
    round: usize,
) -> Result<(f64, inillucent_tree::write::WriteStats, u64, i64), String> {
    let path = scratch(&format!("sweep-{indexes}-{round}"));
    let database = build_sweep(&path, indexes)?;
    let connection = database.session();
    let placeholders: Vec<String> = (1..=SWEEP_PRIMES.len() + 1)
        .map(|at| format!("?{at}"))
        .collect();
    let sql = format!("INSERT INTO t VALUES ({})", placeholders.join(", "));
    let mut statement = connection
        .prepare(&sql)
        .map_err(|error| format!("{sql}: {}", error.message()))?;
    let stats_before = database.write_stats();
    let log_before = database.log_stats();
    connection
        .execute_batch("BEGIN")
        .map_err(|error| error.message().to_string())?;
    let started = Instant::now();
    for index in 0..SWEEP_INSERTS {
        // Ids past the seeded range, so the table's own tree appends at its
        // right edge the way `write.insert.batch`'s rowid insert does, while
        // every index takes the row somewhere in the middle.
        let seq = SWEEP_SEEDED + 1 + index;
        statement
            .bind_integer(1, seq)
            .map_err(|error| error.message().to_string())?;
        for column in 0..SWEEP_PRIMES.len() {
            statement
                .bind_integer(column as u32 + 2, sweep_value(seq, column))
                .map_err(|error| error.message().to_string())?;
        }
        while statement
            .step()
            .map_err(|error| format!("{sql}: {}", error.message()))?
        {}
        statement.reset();
    }
    drop(statement);
    connection
        .execute_batch("COMMIT")
        .map_err(|error| error.message().to_string())?;
    let elapsed = started.elapsed();
    let stats = subtract_stats(database.write_stats(), stats_before);
    let log_bytes = database.log_stats().bytes.saturating_sub(log_before.bytes);
    let pages = match connection
        .query("PRAGMA page_count")
        .ok()
        .and_then(|rows| rows.first().and_then(|row| row.first().cloned()))
    {
        Some(OwnedDatum::Int(count)) => count,
        _ => 0,
    };
    let per_row = elapsed.as_secs_f64() * 1e6 / SWEEP_INSERTS as f64;
    Ok((per_row, stats, log_bytes, pages))
}

/// Returns the counters one transaction added.
///
/// @param after - the counters after it
/// @param before - the counters before it
fn subtract_stats(
    after: inillucent_tree::write::WriteStats,
    before: inillucent_tree::write::WriteStats,
) -> inillucent_tree::write::WriteStats {
    inillucent_tree::write::WriteStats {
        inserted: after.inserted.saturating_sub(before.inserted),
        deleted: after.deleted.saturating_sub(before.deleted),
        updated_in_place: after
            .updated_in_place
            .saturating_sub(before.updated_in_place),
        compactions: after.compactions.saturating_sub(before.compactions),
        splits: after.splits.saturating_sub(before.splits),
        merges: after.merges.saturating_sub(before.merges),
        compaction_nanos: after
            .compaction_nanos
            .saturating_sub(before.compaction_nanos),
        split_nanos: after.split_nanos.saturating_sub(before.split_nanos),
        source_nanos: after.source_nanos.saturating_sub(before.source_nanos),
        image_nanos: after.image_nanos.saturating_sub(before.image_nanos),
        merge_nanos: after.merge_nanos.saturating_sub(before.merge_nanos),
        sizing_nanos: after.sizing_nanos.saturating_sub(before.sizing_nanos),
        encode_nanos: after.encode_nanos.saturating_sub(before.encode_nanos),
        choose_nanos: after.choose_nanos.saturating_sub(before.choose_nanos),
        splices: after.splices.saturating_sub(before.splices),
        splice_nanos: after.splice_nanos.saturating_sub(before.splice_nanos),
        hinted: after.hinted.saturating_sub(before.hinted),
        descended: after.descended.saturating_sub(before.descended),
        room_nanos: after.room_nanos.saturating_sub(before.room_nanos),
    }
}

/// Runs the index count sweep and prints one line per arm.
///
/// **Graded per index, not per statement.** The column that answers whether a
/// leaf format change pays is the cost each index adds to a row: the arm's time
/// less the no-index arm's, divided by the index count. A change that helps the
/// two-index fixture and is flat at ten, or the other way round, shows up there
/// and nowhere else.
///
/// The fastest round is reported beside the median. Load only ever adds time to
/// a round, so on a machine other agents are using the minimum is the reading
/// closest to what the code costs; the median says how far the rounds spread.
fn index_sweep() -> Result<(), String> {
    println!(
        "index count sweep: {SWEEP_INSERTS} inserts in one transaction into a {SWEEP_SEEDED} row \
         table, {SWEEP_ROUNDS} interleaved rounds, default page size\n"
    );
    let mut arms: Vec<SweepArm> = SWEEP_INDEXES
        .iter()
        .map(|_| SweepArm {
            per_row: Vec::new(),
            stats: inillucent_tree::write::WriteStats::default(),
            log_bytes: 0,
            pages: 0,
        })
        .collect();
    for round in 0..SWEEP_ROUNDS {
        for (arm, indexes) in arms.iter_mut().zip(SWEEP_INDEXES) {
            let (per_row, stats, log_bytes, pages) = sweep_round(indexes, round)?;
            arm.per_row.push(per_row);
            arm.stats = stats;
            arm.log_bytes = log_bytes;
            arm.pages = pages;
        }
    }
    let base = arms.first().map(|arm| fastest(&arm.per_row)).unwrap_or(0.0);
    for (arm, indexes) in arms.iter().zip(SWEEP_INDEXES) {
        let quickest = fastest(&arm.per_row);
        let per_index = match indexes {
            0 => 0.0,
            count => (quickest - base) / count as f64,
        };
        let stats = arm.stats;
        println!(
            "indexes {indexes:>2}  fastest {quickest:>7.2} us/row  median {:>7.2}  per index {per_index:>6.2}  \
             compactions {:>5} (spliced {:>5})  splits {:>4}  room {:>7.2} ms  merge {:>6.2}  splice {:>6.2}  \
             sizing {:>6.2}  encode {:>6.2}  log {:>7} KiB  pages {:>5}",
            median(&arm.per_row),
            stats.compactions,
            stats.splices,
            stats.splits,
            stats.room_nanos as f64 / 1e6,
            stats.merge_nanos as f64 / 1e6,
            stats.splice_nanos as f64 / 1e6,
            stats.sizing_nanos as f64 / 1e6,
            stats.encode_nanos as f64 / 1e6,
            arm.log_bytes / 1024,
            arm.pages,
        );
    }
    Ok(())
}

/// Returns the smallest of some readings.
///
/// @param readings - the readings
fn fastest(readings: &[f64]) -> f64 {
    readings.iter().copied().fold(f64::INFINITY, f64::min)
}

/// Returns the median of some readings.
///
/// @param readings - the readings
fn median(readings: &[f64]) -> f64 {
    let mut sorted = readings.to_vec();
    sorted.sort_by(|left, right| left.total_cmp(right));
    sorted.get(sorted.len() / 2).copied().unwrap_or(0.0)
}

/// Runs every case.
fn run() -> Result<(), String> {
    println!("{OPERATIONS} operations each, on a {ROWS} row table\n");
    read_case("read.point", true, "SELECT label FROM t WHERE id = ?1")?;
    case(
        "update.indexed",
        true,
        "UPDATE t SET k = k + 1 WHERE id = ?1",
        1,
    )?;
    case(
        "update.unindexed-column",
        true,
        "UPDATE t SET label = ?2 WHERE id = ?1",
        2,
    )?;
    case(
        "update.no-indexes",
        false,
        "UPDATE t SET k = k + 1 WHERE id = ?1",
        1,
    )?;
    case("delete.indexed", true, "DELETE FROM t WHERE id = ?1", 1)?;
    case("delete.no-indexes", false, "DELETE FROM t WHERE id = ?1", 1)?;
    println!();
    for page_size in [4_096usize, 32_768] {
        for rows in [1_000u32, 2_000, 4_000, 8_000] {
            whole_table_delete(rows, page_size)?;
        }
    }
    println!();
    case(
        "insert.indexed",
        true,
        "INSERT INTO t(id, k, c, label) VALUES (?1 + 1000000, 1, 1, 'x')",
        1,
    )?;
    case(
        "insert.no-indexes",
        false,
        "INSERT INTO t(id, k, c, label) VALUES (?1 + 1000000, 1, 1, 'x')",
        1,
    )?;
    Ok(())
}
