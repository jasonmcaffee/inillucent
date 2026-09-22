//! Pivoted query synthesis: a row, a predicate that is true of it, and the
//! query that must therefore return it.
//!
//! Invariant: **a query whose `WHERE` is true of a row returns that row.** Pick
//! a row, build a conjunction of atoms each verified true of *that row* in Rust
//! rather than by asking the engine, and the row has to come back. If it does
//! not, the planner's route to it skipped it.
//!
//! ## Why this is here beside TLP and NoREC (task-2066 §4.4.4)
//!
//! `tlp_differential.rs` grades a predicate against its own negation, and
//! `differential.rs` grades this engine against the pinned SQLite. Between them
//! they miss one class, and it is not a small one: **a comparison rule that is
//! wrong the same way in all three partitions.**
//!
//! TLP cannot see it. If `>=` is wrong about how a TEXT value compares with an
//! INTEGER one, then `P`, `NOT P` and `P IS NULL` are all computed with the
//! same wrong rule, the three partitions still add up to the table, and every
//! case passes. What TLP proves is that the partitions are consistent with each
//! other, not that any of them is right.
//!
//! An oracle can see it, and that is what `differential.rs` is for - but an
//! oracle is a second engine that has to be built, and every case it grades is
//! a case somebody wrote. This needs neither: the expected answer is computed
//! in Rust from the row's own values, so the only thing the engine is asked is
//! "give me this row back", and the only way to fail is to not give it back.
//!
//! ## What it varies, and why each one matters
//!
//! - **Before and after `ANALYZE`**, because the statistics are what make the
//!   planner take a different route to the same row.
//! - **With each index dropped in turn**, because the route through an index is
//!   the one that can skip a row, and the route through a scan is the control.
//!   A disagreement between them is the defect; a disagreement with nothing is
//!   not visible.
//! - **Over a corpus that has been churned**, so some leaves hold delta rows
//!   rather than a sorted run written in order.

use std::path::PathBuf;

use inillucent_compat::facade::{Connection, Database};
use inillucent_value::Value;

/// How many rows the table holds.
const ROWS: i64 = 400;

/// How many pivots each arm tries.
const CASES: usize = 200;

/// Returns a scratch path nothing else in this file uses.
///
/// @param tag - what to name it after
fn scratch(tag: &str) -> PathBuf {
    let directory = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("pqs");
    let _ = std::fs::create_dir_all(&directory);
    let path = directory.join(format!("{tag}.db"));
    let _ = std::fs::remove_file(&path);
    path
}

/// Advances the generator and returns its next value.
///
/// A fixed xorshift rather than a crate: the corpus and the pivots have to be
/// the same on every machine for a failure to be reproducible from its seed.
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

/// One row of the corpus, as this file knows it.
///
/// Held in Rust so an atom can be checked against it without asking the engine
/// anything - which is the whole point: an expectation the engine computed is
/// not an expectation.
#[derive(Clone)]
struct Row {
    /// The primary key.
    a: i64,
    /// A text column, `None` for NULL.
    b: Option<String>,
    /// A real column, `None` for NULL.
    c: Option<f64>,
    /// An integer column, `None` for NULL.
    d: Option<i64>,
}

/// Returns the corpus this file builds, as Rust values.
///
/// The same rule the `INSERT`s below follow, written once so the two cannot
/// drift: a corpus the test believes in and a corpus the database holds have to
/// be the same corpus.
fn corpus() -> Vec<Row> {
    (1..=ROWS)
        .map(|a| Row {
            a,
            b: (a % 11 != 0).then(|| format!("k{}", a % 40)),
            c: (a % 5 != 0).then(|| (a % 60) as f64 + 0.5),
            d: (a % 7 != 0).then_some(a % 53),
        })
        .collect()
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

/// Builds the table and its indexes, and loads the corpus.
///
/// @param connection - the database
/// @param rows - the corpus, as this file knows it
fn build(connection: &Connection, rows: &[Row]) {
    for statement in [
        "CREATE TABLE t(a INTEGER PRIMARY KEY, b TEXT, c REAL, d INTEGER)",
        "CREATE INDEX t_b ON t(b)",
        "CREATE INDEX t_c ON t(c, a)",
        "CREATE INDEX t_d ON t(d)",
        "BEGIN",
    ] {
        run(connection, statement).unwrap_or_else(|failure| panic!("{statement}: {failure}"));
    }
    for row in rows {
        let b = row
            .b
            .as_ref()
            .map_or_else(|| "NULL".to_string(), |held| format!("'{held}'"));
        let c = row
            .c
            .map_or_else(|| "NULL".to_string(), |held| format!("{held}"));
        let d = row
            .d
            .map_or_else(|| "NULL".to_string(), |held| format!("{held}"));
        let sql = format!("INSERT INTO t VALUES ({}, {b}, {c}, {d})", row.a);
        run(connection, &sql).unwrap_or_else(|failure| panic!("{sql}: {failure}"));
    }
    run(connection, "COMMIT").expect("the corpus commits");
}

/// Returns an atom that is true of this row, verified here rather than asked.
///
/// **Every atom is checked against the row's own values in Rust.** That is what
/// makes the expectation independent of the engine: if the engine's `>=` is
/// wrong, this still says the row must come back, and the engine still has to.
///
/// A NULL column is never compared with `=` or `<`, because those are NULL
/// rather than true, and a conjunction with a NULL in it is not true. `IS NULL`
/// is what a NULL column contributes.
///
/// @param state - the generator's state
/// @param row - the row the predicate must be true of
fn true_atom(state: &mut u64, row: &Row) -> String {
    match below(state, 8) {
        0 => format!("a = {}", row.a),
        1 => format!("a >= {}", row.a.saturating_sub(below(state, 50) as i64)),
        2 => format!("a <= {}", row.a.saturating_add(below(state, 50) as i64)),
        3 => match &row.b {
            Some(held) => format!("b = '{held}'"),
            None => "b IS NULL".to_string(),
        },
        4 => match &row.b {
            // A prefix of the row's own text, so the pattern is true of it.
            Some(held) => {
                let take = 1 + (below(state, held.len().max(1) as u64) as usize);
                format!("b LIKE '{}%'", held.get(..take).unwrap_or(held))
            }
            None => "b IS NULL".to_string(),
        },
        5 => match row.c {
            Some(held) => format!("c >= {}", held - 1.0),
            None => "c IS NULL".to_string(),
        },
        6 => match row.d {
            Some(held) => format!("d IN ({}, {}, {held})", below(state, 53), below(state, 53)),
            None => "d IS NULL".to_string(),
        },
        _ => match row.d {
            Some(held) => format!("d <> {}", held.saturating_add(1)),
            None => "d IS NULL".to_string(),
        },
    }
}

/// Returns a conjunction of atoms, every one of them true of the row.
///
/// @param state - the generator's state
/// @param row - the row it must be true of
fn true_predicate(state: &mut u64, row: &Row) -> String {
    let count = 1 + below(state, 4);
    (0..count)
        .map(|_| true_atom(state, row))
        .collect::<Vec<String>>()
        .join(" AND ")
}

/// Asks for the pivot row through a predicate that is true of it.
///
/// @param connection - the database
/// @param row - the pivot
/// @param condition - a predicate true of it
/// @param context - what to say if the row does not come back
fn must_return_the_row(connection: &Connection, row: &Row, condition: &str, context: &str) {
    let sql = format!("SELECT a FROM t WHERE {condition}");
    let found = run(connection, &sql)
        .unwrap_or_else(|failure| panic!("{context}: `{sql}` was refused: {failure}"));
    assert!(
        found.iter().any(|held| held == &format!("{}", row.a)),
        "{context}: row {} is not in the answer to `{sql}`, and every atom of that predicate is \
         true of it. The planner's route to the row skipped it.",
        row.a
    );
}

/// Every predicate true of a row returns that row, before and after `ANALYZE`.
#[test]
fn a_predicate_true_of_a_row_returns_it() {
    let path = scratch("pivot");
    let database = Database::open(&path).expect("the database opens");
    let connection = database.session().expect("the connection opens");
    let rows = corpus();
    build(&connection, &rows);

    let mut state = 0x2068_5150_u64;
    let mut analysed = false;
    for case in 0..CASES {
        if case == CASES / 2 && !analysed {
            run(&connection, "ANALYZE").expect("ANALYZE runs");
            analysed = true;
        }
        let at = below(&mut state, rows.len() as u64) as usize;
        let Some(row) = rows.get(at) else { continue };
        let condition = true_predicate(&mut state, row);
        must_return_the_row(
            &connection,
            row,
            &condition,
            &format!("case {case} (seed {state:#x}, analysed {analysed})"),
        );
    }
}

/// And with each index dropped in turn, so the route changes under the same row.
///
/// **The index route is the one that can skip a row**, and a scan is the
/// control. Dropping one index at a time is what makes the same predicate take
/// a different route without changing what it means - so a row that comes back
/// one way and not the other is the planner rather than the predicate.
#[test]
fn a_predicate_true_of_a_row_returns_it_by_every_route() {
    let path = scratch("routes");
    let database = Database::open(&path).expect("the database opens");
    let connection = database.session().expect("the connection opens");
    let rows = corpus();
    build(&connection, &rows);
    run(&connection, "ANALYZE").expect("ANALYZE runs");

    // Held so the same pivots and predicates are asked by every route, which is
    // what makes the routes comparable rather than three different runs.
    let mut state = 0x2068_5151_u64;
    let asked: Vec<(Row, String)> = (0..CASES / 4)
        .filter_map(|_| {
            let at = below(&mut state, rows.len() as u64) as usize;
            let row = rows.get(at)?.clone();
            let condition = true_predicate(&mut state, &row);
            Some((row, condition))
        })
        .collect();

    for dropped in ["", "t_b", "t_c", "t_d"] {
        if !dropped.is_empty() {
            let sql = format!("DROP INDEX {dropped}");
            run(&connection, &sql).unwrap_or_else(|failure| panic!("{sql}: {failure}"));
        }
        let route = if dropped.is_empty() {
            "every index".to_string()
        } else {
            format!("without {dropped}")
        };
        for (case, (row, condition)) in asked.iter().enumerate() {
            must_return_the_row(
                &connection,
                row,
                condition,
                &format!("case {case} ({route})"),
            );
        }
    }
}

/// And over a corpus whose pages have moved.
///
/// The same pivots after a range has been deleted and put back, so some leaves
/// hold delta rows rather than a sorted run laid down in order. A probe into
/// one of those has to merge before it can answer, which is the path §4.3.4 is
/// about - and a wrong merge loses the row this asks for.
#[test]
fn a_predicate_true_of_a_row_returns_it_after_the_pages_have_moved() {
    let path = scratch("churned");
    let database = Database::open(&path).expect("the database opens");
    let connection = database.session().expect("the connection opens");
    let rows = corpus();
    build(&connection, &rows);

    for round in 0..6i64 {
        let low = (round * 60 % (ROWS - 60)).max(1);
        let high = low + 60;
        for sql in [
            format!("CREATE TABLE held AS SELECT * FROM t WHERE a BETWEEN {low} AND {high}"),
            format!("DELETE FROM t WHERE a BETWEEN {low} AND {high}"),
            "INSERT INTO t SELECT * FROM held".to_string(),
            "DROP TABLE held".to_string(),
        ] {
            run(&connection, &sql)
                .unwrap_or_else(|failure| panic!("round {round}: {sql}: {failure}"));
        }
    }
    let counted = run(&connection, "SELECT count(*) FROM t").expect("the count reads back");
    assert_eq!(
        counted.first().map(String::as_str),
        Some(format!("{ROWS}").as_str()),
        "the churn did not put every row back, so this arm is asking about a different corpus"
    );

    let mut state = 0x2068_5152_u64;
    for case in 0..CASES {
        let at = below(&mut state, rows.len() as u64) as usize;
        let Some(row) = rows.get(at) else { continue };
        let condition = true_predicate(&mut state, row);
        must_return_the_row(
            &connection,
            row,
            &condition,
            &format!("case {case} (after the churn, seed {state:#x})"),
        );
    }
}
