//! `ORDER BY` answered by the walk instead of by a sorter, graded against the
//! pinned oracle.
//!
//! Invariant: skipping the sort is only ever a speed decision, never an answer
//! one. Every statement here is one where the planner may decide the access
//! path already produces the requested order - and every one of them is graded
//! on the rows *in order* against SQLite 3.53.4's own answer, because an
//! ordering optimisation that gets it wrong returns rows in the wrong order and
//! nothing about the result looks wrong.
//!
//! The fixture is built to make that failure visible rather than lucky: NULLs
//! in every indexed column so their placement matters, a `NOCASE` index so a
//! collation mismatch shows, a descending index so a direction mismatch shows,
//! ties on every key so the tie-break shows, and a two-column index so a
//! partial prefix match shows.

use std::path::{Path, PathBuf};

use rustdb::{Database, Value};
use rustdb_compat::oracle::{Driver, Op, TaggedValue};
use rustdb_compat::workspace_root;

/// Returns the pinned oracle binary, when it has been built.
fn oracle_path() -> Option<PathBuf> {
    if let Ok(explicit) = std::env::var("RUSTDB_SQLITE_ORACLE") {
        let path = PathBuf::from(explicit);
        return path.is_file().then_some(path);
    }
    let path = workspace_root()
        .join(".sqlite-ref/3.53.4")
        .join(format!("sqlite-oracle{}", std::env::consts::EXE_SUFFIX));
    path.is_file().then_some(path)
}

/// The schema every statement here is graded against.
const SCHEMA: &[&str] = &[
    "CREATE TABLE t (id INTEGER PRIMARY KEY, k INTEGER, name TEXT, grp INTEGER, note TEXT)",
    "CREATE INDEX t_k ON t (k)",
    "CREATE INDEX t_name_nocase ON t (name COLLATE NOCASE)",
    "CREATE INDEX t_desc ON t (k DESC)",
    "CREATE INDEX t_grp_k ON t (grp, k)",
    "CREATE TABLE side (id INTEGER PRIMARY KEY, owner INTEGER, tag TEXT)",
    "CREATE INDEX side_owner ON side (owner)",
    "INSERT INTO t VALUES (1, 30, 'Ada', 1, 'x')",
    "INSERT INTO t VALUES (2, 10, 'bob', 1, NULL)",
    "INSERT INTO t VALUES (3, NULL, 'CAI', 2, 'y')",
    "INSERT INTO t VALUES (4, 10, NULL, 2, 'z')",
    "INSERT INTO t VALUES (5, 20, 'dee', 1, NULL)",
    "INSERT INTO t VALUES (6, NULL, 'Eve', 3, 'w')",
    "INSERT INTO t VALUES (7, 30, 'fay', 2, 'v')",
    "INSERT INTO t VALUES (9, 40, 'gus', 3, NULL)",
    "INSERT INTO side VALUES (1, 1, 'p')",
    "INSERT INTO side VALUES (2, 2, 'q')",
    "INSERT INTO side VALUES (3, 2, 'r')",
    "INSERT INTO side VALUES (4, 9, 's')",
];

/// The statements. Every one of them names an order, so every one is compared
/// in order.
const STATEMENTS: &[&str] = &[
    // The rowid, both directions and both spellings, with and without a range.
    "SELECT id FROM t ORDER BY id",
    "SELECT id FROM t ORDER BY id DESC",
    "SELECT id FROM t ORDER BY rowid DESC",
    "SELECT id FROM t WHERE id <= 5 ORDER BY id DESC",
    "SELECT id FROM t WHERE id < 5 ORDER BY id DESC",
    "SELECT id FROM t WHERE id >= 3 ORDER BY id DESC",
    "SELECT id FROM t WHERE id BETWEEN 2 AND 6 ORDER BY id DESC",
    "SELECT id FROM t WHERE id BETWEEN 2 AND 6 ORDER BY id",
    "SELECT id FROM t WHERE id <= 5 ORDER BY id DESC LIMIT 2",
    "SELECT id FROM t WHERE id <= 5 ORDER BY id DESC LIMIT 2 OFFSET 1",
    "SELECT id FROM t ORDER BY id DESC LIMIT 3",
    "SELECT id FROM t WHERE id = 4 ORDER BY id DESC",
    // An indexed column, where the NULLs and the ties decide the answer.
    "SELECT k, id FROM t ORDER BY k",
    "SELECT k, id FROM t ORDER BY k DESC",
    "SELECT k, id FROM t ORDER BY k, id",
    "SELECT k, id FROM t ORDER BY k DESC, id DESC",
    "SELECT k, id FROM t WHERE k >= 20 ORDER BY k DESC",
    "SELECT k, id FROM t WHERE k BETWEEN 10 AND 30 ORDER BY k DESC",
    "SELECT k, id FROM t WHERE k IS NOT NULL ORDER BY k",
    "SELECT k, id FROM t ORDER BY k NULLS FIRST, id",
    "SELECT k, id FROM t ORDER BY k NULLS LAST, id",
    "SELECT k, id FROM t ORDER BY k DESC NULLS FIRST, id",
    "SELECT k, id FROM t ORDER BY k DESC NULLS LAST, id",
    // A descending index: the same column, held the other way round.
    "SELECT k, id FROM t WHERE k > 5 ORDER BY k DESC",
    "SELECT k, id FROM t WHERE k > 5 ORDER BY k",
    // A collation the index does not hold the column in.
    "SELECT name, id FROM t ORDER BY name",
    "SELECT name, id FROM t ORDER BY name COLLATE NOCASE",
    "SELECT name, id FROM t ORDER BY name COLLATE NOCASE DESC",
    "SELECT name, id FROM t ORDER BY name COLLATE BINARY",
    // A two-column index: a prefix, the whole key, and a mixed direction that
    // no single walk can produce.
    "SELECT grp, k, id FROM t ORDER BY grp, k",
    "SELECT grp, k, id FROM t ORDER BY grp DESC, k DESC",
    "SELECT grp, k, id FROM t ORDER BY grp, k DESC",
    "SELECT grp, k, id FROM t ORDER BY grp",
    "SELECT grp, k, id FROM t WHERE grp = 2 ORDER BY k",
    "SELECT grp, k, id FROM t WHERE grp = 2 ORDER BY k DESC",
    "SELECT grp, k, id FROM t WHERE grp = 2 ORDER BY grp, k DESC",
    "SELECT grp, k, id FROM t WHERE grp = 2 ORDER BY k DESC LIMIT 1",
    // Shapes where the order is not the walk's, and the sort has to stay.
    "SELECT k, id FROM t ORDER BY note",
    "SELECT k, id FROM t ORDER BY k + 1",
    "SELECT k, count(*) FROM t GROUP BY k ORDER BY k DESC",
    "SELECT DISTINCT k FROM t ORDER BY k DESC",
    "SELECT k FROM t UNION ALL SELECT k FROM t ORDER BY k DESC",
    "SELECT t.id, side.tag FROM t JOIN side ON side.owner = t.id ORDER BY t.id DESC",
    "SELECT t.id, side.tag FROM t LEFT JOIN side ON side.owner = t.id ORDER BY t.id DESC",
    "SELECT id, row_number() OVER (ORDER BY k, id) FROM t ORDER BY id",
    "SELECT id FROM (SELECT id FROM t ORDER BY id DESC) ORDER BY id",
    // An empty range, and one whose bounds cross.
    "SELECT id FROM t WHERE id BETWEEN 6 AND 2 ORDER BY id DESC",
    "SELECT k FROM t WHERE k > 1000 ORDER BY k DESC",
    "SELECT k FROM t WHERE k < -1000 ORDER BY k",
];

/// Renders one value as a tagged string, so a storage class difference shows.
fn render(value: &Value<'static>) -> String {
    match value {
        Value::Null => "null".to_string(),
        Value::Integer(integer) => format!("int:{integer}"),
        Value::Real(real) => format!("real:{real:?}"),
        Value::Text(text) => format!("text:{}", String::from_utf8_lossy(&text.utf8_bytes())),
        Value::Blob(blob) => format!(
            "blob:{}",
            blob.raw()
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>()
        ),
    }
}

/// Renders one of the oracle's tagged values the same way.
fn render_tagged(value: &TaggedValue) -> String {
    match value {
        TaggedValue::Null => "null".to_string(),
        TaggedValue::Integer(integer) => format!("int:{integer}"),
        TaggedValue::Real(real) => format!("real:{real:?}"),
        TaggedValue::Text(bytes) => format!("text:{}", String::from_utf8_lossy(bytes)),
        TaggedValue::Blob(bytes) => format!(
            "blob:{}",
            bytes
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>()
        ),
    }
}

/// Runs a statement through rust-db, returning its rows or its failure.
fn rustdb_rows(connection: &rustdb::Connection, sql: &str) -> Result<Vec<String>, String> {
    let mut statement = match connection.prepare(sql) {
        Ok(statement) => statement,
        Err(reason) => return Err(format!("{reason:?}")),
    };
    let mut rows = Vec::new();
    loop {
        match statement.step() {
            Ok(true) => rows.push(
                statement
                    .row()
                    .iter()
                    .map(render)
                    .collect::<Vec<String>>()
                    .join("|"),
            ),
            Ok(false) => break,
            Err(reason) => return Err(format!("{reason:?}")),
        }
    }
    Ok(rows)
}

/// Builds the graded database with the oracle and returns a driver on it.
fn build(directory: &Path, tag: &str) -> Option<(Driver, PathBuf)> {
    let program = oracle_path()?;
    let database = directory.join(format!("ordering-{tag}.db"));
    let _ = std::fs::remove_file(&database);
    let mut driver = Driver::start("sqlite", &program).ok()?;
    driver.send(&Op::Hello).ok()?;
    driver
        .send(&Op::Open(database.display().to_string()))
        .ok()?;
    for statement in SCHEMA {
        let observation = driver.send(&Op::Exec((*statement).to_string())).ok()?;
        assert!(
            observation.ok,
            "the oracle refused the fixture schema: {statement}: {}",
            observation.message
        );
    }
    Some((driver, database))
}

/// Every ordered statement returns the reference's rows, in the reference's
/// order.
#[test]
fn ordered_statements_match_the_oracle() {
    let directory = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("ordering");
    let _ = std::fs::create_dir_all(&directory);
    let Some((mut driver, database)) = build(&directory, "rows") else {
        eprintln!("the pinned SQLite oracle is not built; skipping");
        return;
    };
    let handle = Database::open_with_busy_timeout(&database, std::time::Duration::from_secs(5))
        .expect("the fixture opens");
    let connection = handle.connect().expect("the connection opens");
    let mut failures = Vec::new();
    for sql in STATEMENTS {
        let observation = driver
            .send(&Op::Query((*sql).to_string()))
            .expect("the oracle answers");
        let ours = rustdb_rows(&connection, sql);
        if !observation.ok {
            if ours.is_ok() {
                failures.push(format!(
                    "{sql}\n  sqlite refused: {}\n  rust-db accepted it",
                    observation.message
                ));
            }
            continue;
        }
        let expected: Vec<String> = observation
            .rows
            .iter()
            .map(|row| {
                row.iter()
                    .map(render_tagged)
                    .collect::<Vec<String>>()
                    .join("|")
            })
            .collect();
        match ours {
            Err(reason) => failures.push(format!("{sql}\n  rust-db failed: {reason}")),
            Ok(actual) if actual != expected => {
                failures.push(format!(
                    "{sql}\n  sqlite:  {expected:?}\n  rust-db: {actual:?}"
                ));
            }
            Ok(_) => {}
        }
    }
    assert!(
        failures.is_empty(),
        "{} of {} statements diverged:\n{}",
        failures.len(),
        STATEMENTS.len(),
        failures.join("\n")
    );
}

/// The optimisation actually happens, and only where it should.
///
/// The rows being right is necessary and not sufficient: a planner that quietly
/// sorted everything would pass the test above and be exactly the thing this
/// work was meant to remove. So the plans are read too - `USE TEMP B-TREE FOR
/// ORDER BY` is what a sort looks like in `EXPLAIN QUERY PLAN` - and both
/// halves are asserted: gone where the walk can answer the order, still there
/// where it cannot.
#[test]
fn the_sort_is_skipped_exactly_where_the_walk_answers_the_order() {
    let directory = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("ordering");
    let _ = std::fs::create_dir_all(&directory);
    let Some((_driver, database)) = build(&directory, "plans") else {
        eprintln!("the pinned SQLite oracle is not built; skipping");
        return;
    };
    let handle = Database::open_with_busy_timeout(&database, std::time::Duration::from_secs(5))
        .expect("the fixture opens");
    let connection = handle.connect().expect("the connection opens");

    let walked = [
        "SELECT id FROM t ORDER BY id",
        "SELECT id FROM t ORDER BY id DESC",
        "SELECT id FROM t WHERE id <= 5 ORDER BY id DESC",
        "SELECT k, id FROM t ORDER BY k",
        "SELECT k, id FROM t ORDER BY k DESC",
        "SELECT grp, k, id FROM t WHERE grp = 2 ORDER BY k DESC",
        "SELECT name, id FROM t ORDER BY name COLLATE NOCASE",
    ];
    let sorted = [
        // The order is over an expression, not a column.
        "SELECT k, id FROM t ORDER BY k + 1",
        // A column no index holds.
        "SELECT k, id FROM t ORDER BY note",
        // Two columns of one index, in opposite directions: one walk cannot
        // produce both.
        "SELECT grp, k, id FROM t ORDER BY grp, k DESC",
        // Grouping, distinct, and a window each reorder the rows after the walk.
        "SELECT k, count(*) FROM t GROUP BY k ORDER BY k DESC",
        "SELECT DISTINCT k FROM t ORDER BY k DESC",
        "SELECT id, row_number() OVER (ORDER BY k, id) FROM t ORDER BY id",
        // A collation the index does not hold the column in.
        "SELECT name, id FROM t ORDER BY name COLLATE BINARY",
    ];

    let sorts = |sql: &str| -> bool {
        let plan = rustdb_rows(&connection, &format!("EXPLAIN QUERY PLAN {sql}"))
            .expect("the plan renders");
        plan.iter().any(|row| row.contains("ORDER BY"))
    };
    for sql in walked {
        assert!(!sorts(sql), "this should be answered by the walk: {sql}");
    }
    for sql in sorted {
        assert!(sorts(sql), "this should still sort: {sql}");
    }
}
