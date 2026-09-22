//! One table an order of magnitude past the buffer pool, graded the way the
//! small ones are.
//!
//! Invariant: **the answers do not change when the table stops fitting in
//! memory.** Every differential case in this repository runs on six rows, and
//! nothing above twenty thousand is graded at all (task-2066 §4.4.5). So every
//! claim the suite makes is a claim about a table the pool holds entirely, and
//! the two things that only happen past that point - a scan that evicts what it
//! read, and an index probe into a tree deep enough to have interior levels the
//! pool cannot keep - are claims nothing checks.
//!
//! ## Sized from the pool rather than from a number
//!
//! The row count is derived from what the database says its own pool holds -
//! `PRAGMA page_size` and `PRAGMA cache_size` - so it scales with the
//! configuration rather than restating it. A fixed "one million rows"
//! would be past the pool today and inside it after somebody raised the cache,
//! and the day it stopped testing anything nothing would say so - which is the
//! failure class `tests/inillucent-testing-tdd.md` rule 1.4 is about.
//!
//! `the_table_is_past_the_pool` asserts the ratio it achieved, so a change that made
//! the rows smaller cannot quietly bring the table back inside the pool.
//!
//! ## What it grades
//!
//! TLP and NoREC, the same two properties `tlp_differential.rs` checks, plus
//! `PRAGMA integrity_check` at the end. The properties need no oracle, which is
//! what makes them affordable here: a table this size cannot be compared row by
//! row against a second engine in a nightly window.

use std::path::PathBuf;

use inillucent_compat::facade::{Connection, Database};
use inillucent_value::Value;

/// How many times past the pool the table must reach.
///
/// Ten, which is enough that a scan evicts what it read several times over and
/// an index probe cannot hold the interior levels. Asserted rather than
/// assumed: see `the_table_is_past_the_pool`.
const PAST_THE_POOL: u64 = 10;

/// How many predicates each arm generates.
///
/// Fewer than the small generator's four hundred, because each one runs over a
/// table that does not fit in memory. The point here is the size, not the
/// count - the shapes are covered by `tlp_differential.rs`.
const CASES: usize = 25;

/// Returns a scratch path nothing else uses.
///
/// **A directory per test, not one path both of them open.** libtest runs the
/// tests in this file at the same time, so a single `large.db` is two tests
/// building two tables over each other - and the one that measured the file
/// measured whatever the other had reached. That is what made
/// `the_table_is_past_the_pool` fail the first time it was allowed to run.
///
/// The directory is removed whole rather than the file alone. A `.rdb` is a
/// file plus log segments named after it, so removing the first and leaving the
/// rest leaves a log from the previous run beside a fresh database, and the two
/// chains are then read as one. That is the shape of the incident in §5 of
/// task-2066: a database replaced under a log that outlived it.
///
/// @param tag - which test is asking
fn scratch(tag: &str) -> PathBuf {
    let directory = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join("large-table")
        .join(tag);
    let _ = std::fs::remove_dir_all(&directory);
    let _ = std::fs::create_dir_all(&directory);
    directory.join("large.db")
}

/// Advances the generator.
///
/// @param state - the generator's state
fn next(state: &mut u64) -> u64 {
    *state ^= *state << 13;
    *state ^= *state >> 7;
    *state ^= *state << 17;
    *state
}

/// Returns a value below `bound`.
///
/// @param state - the generator's state
/// @param bound - one past the largest value
fn below(state: &mut u64, bound: u64) -> u64 {
    if bound == 0 {
        0
    } else {
        next(state) % bound
    }
}

/// Runs a statement, returning its rows rendered or the failure's message.
///
/// @param connection - the database
/// @param sql - the statement
fn run(connection: &Connection, sql: &str) -> Result<Vec<String>, String> {
    let mut statement = connection
        .prepare(sql)
        .map_err(|failure| failure.message().to_string())?;
    let mut rows = Vec::new();
    loop {
        match statement.step() {
            Ok(true) => {}
            Ok(false) => break,
            Err(failure) => return Err(failure.message().to_string()),
        }
        let rendered: Vec<String> = statement
            .row()
            .iter()
            .map(|value| match value {
                Value::Null => "null".to_string(),
                Value::Integer(number) => format!("{number}"),
                Value::Real(number) => format!("{number:?}"),
                Value::Text(text) => String::from_utf8_lossy(&text.utf8_bytes()).into_owned(),
                Value::Blob(blob) => format!("blob:{}", blob.raw().len()),
            })
            .collect();
        rows.push(rendered.join("|"));
    }
    Ok(rows)
}

/// Returns the one scalar a query answered.
///
/// @param connection - the database
/// @param sql - the statement
fn scalar(connection: &Connection, sql: &str) -> String {
    run(connection, sql)
        .unwrap_or_else(|failure| panic!("{sql}: {failure}"))
        .first()
        .cloned()
        .unwrap_or_default()
}

/// Returns how many bytes the pool holds, asked of the database itself.
///
/// **Asked rather than restated** (rule 1.4). A constant here would be a second
/// opinion about the engine's configuration, and the day the two disagreed this
/// file would still pass while grading a table that fits. `PRAGMA cache_size`
/// and `PRAGMA page_size` are the engine's own answer, so they cannot drift
/// from it.
///
/// A negative `cache_size` is a kibibyte budget, which is SQLite's convention
/// and this engine's; a positive one is a page count.
///
/// @param connection - the open database
fn pool_bytes(connection: &Connection) -> u64 {
    let page = scalar(connection, "PRAGMA page_size")
        .parse::<i64>()
        .unwrap_or(4096);
    let cache = scalar(connection, "PRAGMA cache_size")
        .parse::<i64>()
        .unwrap_or(-2000);
    let bytes = if cache < 0 {
        cache.saturating_neg().saturating_mul(1024)
    } else {
        cache.saturating_mul(page)
    };
    u64::try_from(bytes).unwrap_or(2 * 1024 * 1024).max(1)
}

/// How large a pool this test runs against, in kibibytes.
///
/// **Set rather than inherited, and this is the whole reason the test is
/// affordable.** The shipped default is `cache_size = -131072`, a 128 MiB
/// budget, so ten times it is 1.25 GiB - a table a nightly cannot build and
/// the first version of this file asked for, at 22,369,621 rows inserted one
/// statement at a time. Two runs were stopped by hand at quarter of an hour
/// with the file still climbing.
///
/// The condition under test is that the table does not fit in the pool. A pool
/// the test sets down reaches that condition in minutes and reaches it for the
/// same reason: a scan evicts what it read, and a probe cannot keep the
/// interior levels. What it does not reproduce is the shipped default's
/// *absolute* size, which no property here depends on.
const POOL_KIB: i64 = 8 * 1024;

/// Sets the pool this test runs against, and returns how many bytes it holds.
///
/// The value is written and then read back through [`pool_bytes`] rather than
/// returned from the constant, so everything downstream is still using the
/// engine's own answer. A pragma this engine refused would otherwise leave the
/// file sizing a table against a pool it does not have.
///
/// @param connection - the open database
fn narrow_the_pool(connection: &Connection) -> u64 {
    let sql = format!("PRAGMA cache_size = -{POOL_KIB}");
    run(connection, &sql).unwrap_or_else(|failure| panic!("{sql}: {failure}"));
    let held = pool_bytes(connection);
    let wanted = u64::try_from(POOL_KIB).unwrap_or(0).saturating_mul(1024);
    assert_eq!(
        held, wanted,
        "`{sql}` left the pool at {held} bytes rather than {wanted}, so every size below would be          measured against a pool this test does not have"
    );
    held
}

/// Returns how many rows put the table `PAST_THE_POOL` times past the pool.
///
/// **Measured, not estimated.** The estimate was sixty bytes a row - the key,
/// the text and the index entry added up - and the real figure on disk is
/// several hundred once pages, the index tree and the free map are counted, so
/// a table meant to be ten times the pool came out sixty times it and took
/// proportionally longer. The first batch is written, the file is measured, and
/// the rest is sized from what that batch actually cost. A measured width
/// cannot drift from the engine the way a constant can, which is the argument
/// [`pool_bytes`] already makes for asking the pragmas.
///
/// @param connection - the open database
/// @param written - how many rows the probe batch put in
/// @param path - the database file, to measure
fn rows_needed(connection: &Connection, written: i64, path: &std::path::Path) -> i64 {
    let file = std::fs::metadata(path).map(|held| held.len()).unwrap_or(0);
    let per_row = (file / u64::try_from(written).unwrap_or(1).max(1)).max(1);
    let wanted = pool_bytes(connection).saturating_mul(PAST_THE_POOL);
    i64::try_from((wanted / per_row).max(200_000)).unwrap_or(200_000)
}

/// Builds the table, in batches, and returns how many rows it holds.
///
/// One probe batch first, so the rest can be sized from what a row really
/// costs on disk; then the remainder.
///
/// @param connection - the database
/// @param path - the database file, to measure the probe batch against
fn build(connection: &Connection, path: &std::path::Path) -> i64 {
    narrow_the_pool(connection);
    for statement in [
        "CREATE TABLE big(a INTEGER PRIMARY KEY, b TEXT, d INTEGER)",
        // Before the load, which is the order an application writes and the
        // order that leaves the leaves holding delta rows (task-2066 §4.3.4).
        "CREATE INDEX big_d ON big(d)",
    ] {
        run(connection, statement).unwrap_or_else(|failure| panic!("{statement}: {failure}"));
    }
    // The probe batch, so the rest can be sized from what a row costs rather
    // than from a guess about it.
    let probe = 20_000i64;
    let mut at = load(connection, 1, probe);
    let rows = rows_needed(connection, probe, path).max(probe);
    while at <= rows {
        at = load(connection, at, (at + 20_000 - 1).min(rows));
    }
    rows
}

/// Writes one run of rows and returns the next unused key.
///
/// **Five hundred rows a statement, not one.** The cost of this load is the
/// statement and not the row: the text is formatted, parsed, planned and
/// executed per statement, and the rows inside one `VALUES` list share all of
/// it. The rows written are the same rows either way.
///
/// @param connection - the database
/// @param first - the first key to write
/// @param last - the last key to write
fn load(connection: &Connection, first: i64, last: i64) -> i64 {
    let per_statement = 500i64;
    let mut at = first;
    run(connection, "BEGIN").expect("a batch begins");
    while at <= last {
        let end = (at + per_statement - 1).min(last);
        let mut values = String::from("INSERT INTO big VALUES ");
        let mut first_in_statement = true;
        while at <= end {
            let d = if at % 7 == 0 {
                "NULL".to_string()
            } else {
                format!("{}", at % 1013)
            };
            if !first_in_statement {
                values.push_str(", ");
            }
            first_in_statement = false;
            values.push_str(&format!("({at}, 'row-{at}-padding-padding', {d})"));
            at += 1;
        }
        run(connection, &values)
            .unwrap_or_else(|failure| panic!("a run of rows ending at {end}: {failure}"));
    }
    run(connection, "COMMIT").expect("a batch commits");
    at
}

/// The table really is past the pool, and says by how much.
///
/// **Without this the whole file could pass on a table that fits** (rule 1.4).
/// A change that made the rows narrower, or raised the default cache, would
/// leave every arm below green while grading exactly what the small generator
/// already grades.
#[test]
fn the_table_is_past_the_pool() {
    let path = scratch("past-the-pool");
    let database = Database::open(&path).expect("the database opens");
    let connection = database.session().expect("the connection opens");
    let rows = build(&connection, &path);

    let held = pool_bytes(&connection);
    let file_bytes = std::fs::metadata(&path)
        .map(|bytes| bytes.len())
        .unwrap_or(0);
    let ratio = file_bytes / held.max(1);
    assert_eq!(
        scalar(&connection, "SELECT count(*) FROM big"),
        format!("{rows}"),
        "the load did not put every row in"
    );
    assert!(
        ratio >= PAST_THE_POOL / 2,
        "the table is {file_bytes} bytes against a {held} byte pool, which is {ratio} times \
         past it and not the {PAST_THE_POOL} this file is for. Either the rows got narrower or \
         the pool got bigger; raise the row count rather than lowering the bar."
    );
}

/// TLP and NoREC hold over a table that does not fit in memory.
///
/// The same two properties the small generator checks, over a tree deep enough
/// that the pool cannot hold its interior levels. What that changes is the
/// route: a probe evicts what it read on the way down, so a page it needs again
/// is fetched again rather than found - and a defect in that path is invisible
/// to every other case in this repository, all of which run on tables the pool
/// holds whole.
#[test]
fn the_partitions_hold_over_a_table_past_the_pool() {
    let path = scratch("partitions");
    let database = Database::open(&path).expect("the database opens");
    let connection = database.session().expect("the connection opens");
    let rows = build(&connection, &path);

    let whole = scalar(&connection, "SELECT count(*) FROM big");
    assert_eq!(whole, format!("{rows}"), "the corpus is not what it says");

    let mut state = 0x2068_4405_u64;
    for case in 0..CASES {
        let condition = match below(&mut state, 4) {
            0 => format!("d = {}", below(&mut state, 1013)),
            1 => {
                let low = below(&mut state, 1013);
                format!("d BETWEEN {low} AND {}", low + below(&mut state, 50))
            }
            2 => format!("a > {}", below(&mut state, rows as u64)),
            _ => format!("b LIKE 'row-{}%'", below(&mut state, 9)),
        };

        // TLP: the three partitions count the whole table.
        let mut counted = 0i64;
        for arm in [
            format!("SELECT count(*) FROM big WHERE {condition}"),
            format!("SELECT count(*) FROM big WHERE NOT ({condition})"),
            format!("SELECT count(*) FROM big WHERE ({condition}) IS NULL"),
        ] {
            counted = counted.saturating_add(scalar(&connection, &arm).parse().unwrap_or(0));
        }
        assert_eq!(
            counted, rows,
            "case {case}: the partitions of `{condition}` count {counted} rows over a table of \
             {rows}"
        );

        // NoREC: the same predicate through an index and through a scan.
        let through_index = scalar(
            &connection,
            &format!("SELECT count(*) FROM big WHERE {condition}"),
        );
        let through_scan = scalar(
            &connection,
            &format!("SELECT sum(CASE WHEN ({condition}) THEN 1 ELSE 0 END) FROM big"),
        );
        let scanned = if through_scan == "null" {
            "0".to_string()
        } else {
            through_scan
        };
        assert_eq!(
            through_index, scanned,
            "case {case}: `{condition}` counts differently through an index and through a scan \
             over a table past the pool"
        );
    }

    let checked = scalar(&connection, "PRAGMA integrity_check");
    assert_eq!(
        checked, "ok",
        "the table is not sound after the run: {checked}"
    );
}
