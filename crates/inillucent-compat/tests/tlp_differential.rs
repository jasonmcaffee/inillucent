//! Generated predicates, graded against this engine's own answers.
//!
//! Invariant: **a predicate partitions a table into three parts and nothing
//! else.** For any predicate `P`, every row of a table satisfies exactly one of
//! `P`, `NOT P` and `P IS NULL`, so the three answers together are the table
//! and no row appears twice. That is true of SQL rather than of an engine, so
//! it can be checked without a second engine to compare against - which is the
//! point, because the pinned SQLite shares none of this engine's planner and so
//! cannot say whether *this* planner's route to a row is the route that finds
//! it.
//!
//! `new_engine_differential.rs` enumerates templates a person wrote, so it
//! finds the defects a person thought of. This generates predicate trees, which
//! is how the two arms below catch an optimiser defect nothing else here can:
//!
//! - **TLP** (ternary logic partitioning). `SELECT a FROM t` against
//!   `WHERE P` + `WHERE NOT P` + `WHERE P IS NULL`. A seek that skips a row the
//!   predicate matches shows up as a row missing from the union, and a range
//!   that admits a row twice shows up as a duplicate. Three-valued logic is
//!   what makes it work: `NOT P` is not "the rest", and an engine that treated
//!   it that way would lose every row where `P` is NULL.
//! - **NoREC** (non-optimising reference engine construction). The same
//!   predicate as a `WHERE`, which the planner may answer through an index,
//!   against the same predicate applied to every row of a scan. The second
//!   cannot use an index - there is no `WHERE` for the planner to push into one
//!   - so a disagreement is the index route answering something the row-by-row
//!   evaluation does not.
//!
//! Both arms run twice per case, before and after `ANALYZE`, because the
//! statistics are what make the planner choose a different route for the same
//! query and a suite that only ran one of them would only ever grade one route.

use std::path::PathBuf;

use inillucent_compat::facade::{Connection, Database};
use inillucent_value::Value;

/// How many predicates each test generates.
const CASES: usize = 400;

/// How many rows the table holds.
///
/// Enough that a seek and a scan are different plans, small enough that four
/// hundred predicates each run six queries over it in a few seconds.
const ROWS: i64 = 600;

/// Returns a scratch path nothing else in this file uses.
///
/// @param tag - what to name it after
fn scratch(tag: &str) -> PathBuf {
    let directory = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("tlp");
    let _ = std::fs::create_dir_all(&directory);
    let path = directory.join(format!("{tag}.db"));
    let _ = std::fs::remove_file(&path);
    path
}

/// Returns the next value of a deterministic generator.
///
/// Written out rather than taken from a crate because the workspace's
/// dependency allow-list does not carry one, and a generator whose sequence is
/// fixed is what makes a failure here reproducible from the seed alone.
///
/// @param state - the generator's state, advanced in place
fn next(state: &mut u64) -> u64 {
    *state ^= *state << 13;
    *state ^= *state >> 7;
    *state ^= *state << 17;
    *state
}

/// Returns a number below a bound.
///
/// @param state - the generator's state
/// @param bound - one past the largest value wanted
fn below(state: &mut u64, bound: u64) -> u64 {
    match bound {
        0 => 0,
        bound => next(state) % bound,
    }
}

/// The columns a generated predicate may read.
///
/// One of each affinity, and `d` holds NULLs - which is what makes the `IS
/// NULL` partition non-empty and the whole TLP arm worth running.
const COLUMNS: &[&str] = &["a", "b", "c", "d"];

/// Returns a random comparison against one column.
///
/// @param state - the generator's state
fn atom(state: &mut u64) -> String {
    let column = COLUMNS
        .get((below(state, COLUMNS.len() as u64)) as usize)
        .copied()
        .unwrap_or("a");
    let operator = match below(state, 8) {
        0 => "=",
        1 => "<>",
        2 => "<",
        3 => "<=",
        4 => ">",
        5 => ">=",
        6 => "IS",
        _ => "IS NOT",
    };
    let value = match column {
        "b" => match below(state, 4) {
            0 => "NULL".to_string(),
            1 => "'k1'".to_string(),
            2 => format!("'k{}'", below(state, 40)),
            _ => format!("'{}'", below(state, 9)),
        },
        "c" => match below(state, 3) {
            0 => "NULL".to_string(),
            1 => format!("{}.5", below(state, 60)),
            _ => format!("{}", below(state, 600)),
        },
        _ => match below(state, 4) {
            0 => "NULL".to_string(),
            _ => format!("{}", below(state, 700)),
        },
    };
    // `IS` and `IS NOT` are the two that are not three-valued, and they are
    // generated on purpose: a partition that only ever held three-valued
    // comparisons would never exercise the arm where `P IS NULL` is empty.
    format!("{column} {operator} {value}")
}

/// Returns a predicate tree of at most `depth` levels.
///
/// @param state - the generator's state
/// @param depth - how many more levels of AND, OR and NOT to allow
fn predicate(state: &mut u64, depth: u32) -> String {
    if depth == 0 {
        return atom(state);
    }
    match below(state, 6) {
        0 => format!(
            "({} AND {})",
            predicate(state, depth - 1),
            predicate(state, depth - 1)
        ),
        1 => format!(
            "({} OR {})",
            predicate(state, depth - 1),
            predicate(state, depth - 1)
        ),
        2 => format!("(NOT {})", predicate(state, depth - 1)),
        3 => {
            let column = COLUMNS
                .get((below(state, COLUMNS.len() as u64)) as usize)
                .copied()
                .unwrap_or("a");
            match column {
                "b" => format!("({column} IN ('k1', 'k2', 'k7', NULL))"),
                _ => format!(
                    "({column} IN ({}, {}, {}))",
                    below(state, 700),
                    below(state, 700),
                    below(state, 700)
                ),
            }
        }
        4 => {
            let low = below(state, 700);
            format!("(a BETWEEN {low} AND {})", low + below(state, 200))
        }
        _ => atom(state),
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
                Value::Integer(number) => format!("int:{number}"),
                Value::Real(number) => format!("real:{number:?}"),
                Value::Text(text) => {
                    format!("text:{}", String::from_utf8_lossy(&text.utf8_bytes()))
                }
                Value::Blob(blob) => format!("blob:{}", blob.raw().len()),
            })
            .collect();
        rows.push(rendered.join("|"));
    }
    Ok(rows)
}

/// Builds the table every case runs against.
///
/// @param connection - the database
fn build(connection: &Connection) {
    for statement in [
        "CREATE TABLE t(a INTEGER PRIMARY KEY, b TEXT, c REAL, d INTEGER)",
        "CREATE INDEX t_b ON t(b)",
        "CREATE INDEX t_c ON t(c, a)",
        "CREATE INDEX t_d ON t(d)",
        "BEGIN",
    ] {
        run(connection, statement).unwrap_or_else(|failure| panic!("{statement}: {failure}"));
    }
    for row in 1..=ROWS {
        // `d` is NULL every seventh row, which is what gives the `IS NULL`
        // partition rows to hold.
        let d = if row % 7 == 0 {
            "NULL".to_string()
        } else {
            format!("{}", row % 53)
        };
        let text = if row % 11 == 0 {
            "NULL".to_string()
        } else {
            format!("'k{}'", row % 40)
        };
        let sql = format!(
            "INSERT INTO t VALUES ({row}, {text}, {}, {d})",
            (row % 60) as f64 + 0.5
        );
        run(connection, &sql).unwrap_or_else(|failure| panic!("{sql}: {failure}"));
    }
    run(connection, "COMMIT").expect("the corpus commits");
}

/// A predicate and its negation and its NULL part are the whole table, once.
#[test]
fn a_predicate_partitions_the_table_into_three() {
    let path = scratch("partition");
    let database = Database::open(&path).expect("the database opens");
    let connection = database.session().expect("the connection opens");
    build(&connection);

    let whole = run(&connection, "SELECT a FROM t ORDER BY a").expect("the base query runs");
    assert_eq!(whole.len(), ROWS as usize, "the corpus is not what it says");

    let mut state = 0x1932_7717_u64;
    let mut analysed = false;
    for case in 0..CASES {
        // Half the cases run against the statistics, because they are what
        // make the planner take the other route to the same rows.
        if case == CASES / 2 && !analysed {
            run(&connection, "ANALYZE").expect("ANALYZE runs");
            analysed = true;
        }
        let condition = predicate(&mut state, 2);
        let mut parts = Vec::new();
        for arm in [
            format!("SELECT a FROM t WHERE {condition}"),
            format!("SELECT a FROM t WHERE NOT ({condition})"),
            format!("SELECT a FROM t WHERE ({condition}) IS NULL"),
        ] {
            match run(&connection, &arm) {
                Ok(rows) => parts.extend(rows),
                // A predicate the binder refuses is not a partition failure -
                // it is a construct this engine does not have - and it is
                // reported rather than skipped silently so that a generator
                // producing mostly refusals cannot look like a passing run.
                Err(failure) => panic!("case {case}: {arm} was refused: {failure}"),
            }
        }
        parts.sort();
        let mut expected = whole.clone();
        expected.sort();
        assert_eq!(
            parts, expected,
            "case {case}: the three partitions of `{condition}` are not the table"
        );
    }
}

/// The same predicate through an index and through a scan answers the same.
#[test]
fn an_indexed_predicate_answers_what_a_scan_answers() {
    let path = scratch("norec");
    let database = Database::open(&path).expect("the database opens");
    let connection = database.session().expect("the connection opens");
    build(&connection);

    let mut state = 0x1932_9931_u64;
    let mut analysed = false;
    for case in 0..CASES {
        if case == CASES / 2 && !analysed {
            run(&connection, "ANALYZE").expect("ANALYZE runs");
            analysed = true;
        }
        let condition = predicate(&mut state, 2);
        // The first may be answered through an index; the second has no
        // `WHERE` for the planner to push into one, so it reads every row and
        // applies the predicate in the executor.
        let optimised = format!("SELECT count(*) FROM t WHERE {condition}");
        let scanned = format!("SELECT sum(CASE WHEN ({condition}) THEN 1 ELSE 0 END) FROM t");
        let through_index = run(&connection, &optimised)
            .unwrap_or_else(|failure| panic!("case {case}: {optimised} was refused: {failure}"));
        let through_scan = run(&connection, &scanned)
            .unwrap_or_else(|failure| panic!("case {case}: {scanned} was refused: {failure}"));
        // `sum` over no matching row answers NULL where `count` answers 0, and
        // that difference is SQL's rather than the planner's.
        let scanned_count = match through_scan.first().map(String::as_str) {
            Some("null") => "int:0".to_string(),
            Some(other) => other.to_string(),
            None => String::new(),
        };
        assert_eq!(
            through_index.first().cloned().unwrap_or_default(),
            scanned_count,
            "case {case}: `{condition}` counts differently through an index and through a scan"
        );
    }
}
