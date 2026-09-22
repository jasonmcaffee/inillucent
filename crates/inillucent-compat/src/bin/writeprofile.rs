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
//! Usage: `cargo run --release -p inillucent-compat --bin inillucent-writeprofile`

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

/// Runs every case and prints what each one cost.
fn main() -> ExitCode {
    match run() {
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
    let _ = std::fs::remove_file(&path);
    for suffix in ["-journal", "-wal", "-shm"] {
        let _ = std::fs::remove_file(directory.join(format!("{name}.db{suffix}")));
    }
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
    let database = Database::open_at(path, page_size, inillucent_engine::DEFAULT_FRAMES as usize)
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
