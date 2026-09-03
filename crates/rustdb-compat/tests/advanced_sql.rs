//! Joins, compounds, subqueries and CTEs, graded against the pinned oracle.
//!
//! Invariant: every expectation here is SQLite 3.53.4's own answer, taken from
//! the oracle at run time rather than written out by hand. A hand-written
//! expectation is a second opinion about what SQL means, and the whole point of
//! this suite is that there is only one.
//!
//! When the oracle has not been built the tests skip rather than fail: the
//! conformance suite is where checked-in expectations live, and duplicating
//! them here would mean two places to update when the corpus grows.

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

/// The schema every statement in this file is graded against.
///
/// It is deliberately awkward: nullable columns on both sides of every join,
/// a row in each table that matches nothing in the other, duplicate keys so a
/// set operation has something to de-duplicate, and a `NULL` in the column an
/// `IN` subquery reads so the three-valued case is exercised rather than
/// assumed.
const SCHEMA: &[&str] = &[
    "CREATE TABLE a (id INTEGER PRIMARY KEY, name TEXT, team TEXT, score REAL)",
    "CREATE TABLE b (id INTEGER PRIMARY KEY, team TEXT, region TEXT, rank INTEGER)",
    "CREATE TABLE c (k INTEGER, v TEXT)",
    "CREATE INDEX a_team ON a (team)",
    "INSERT INTO a VALUES (1, 'ada', 'blue', 10.5)",
    "INSERT INTO a VALUES (2, 'bob', 'red', -2.0)",
    "INSERT INTO a VALUES (3, 'cai', 'blue', 0.0)",
    "INSERT INTO a VALUES (4, 'dee', NULL, 99.25)",
    "INSERT INTO a VALUES (5, 'eve', 'gone', NULL)",
    "INSERT INTO b VALUES (10, 'blue', 'north', 1)",
    "INSERT INTO b VALUES (11, 'red', 'south', 2)",
    "INSERT INTO b VALUES (12, 'green', 'east', 3)",
    "INSERT INTO b VALUES (13, NULL, 'west', 4)",
    "INSERT INTO c VALUES (1, 'one')",
    "INSERT INTO c VALUES (1, 'one')",
    "INSERT INTO c VALUES (2, 'two')",
    "INSERT INTO c VALUES (NULL, 'null')",
    "INSERT INTO c VALUES (3, NULL)",
    "CREATE VIEW blue AS SELECT id, name, score FROM a WHERE team = 'blue'",
    "CREATE VIEW ranked (who, place) AS SELECT a.name, b.rank FROM a JOIN b ON a.team = b.team",
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
    // One file per test: these run in parallel threads, and a shared file gives
    // whichever test loses the race a "database is locked" that looks like a
    // divergence rather than the collision it is.
    let database = directory.join(format!("advanced-{tag}.db"));
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

/// Grades every statement in a list against the oracle.
///
/// Both engines answer the same statement against the same file, and the
/// comparison is on the rendered rows in order - so a row that came back as an
/// integer where SQLite returned a real is a failure, not a rounding detail.
fn grade(tag: &str, statements: &[&str]) {
    // `CARGO_TARGET_TMPDIR` is set at compile time, not at run time. Reading it
    // with `var_os` returns nothing, and the whole suite then skips silently -
    // which is how these five tests first "passed" in 0.00 seconds.
    let directory = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("advanced-sql");
    let _ = std::fs::create_dir_all(&directory);
    let Some((mut driver, database)) = build(&directory, tag) else {
        eprintln!("the pinned SQLite oracle is not built; skipping");
        return;
    };
    let handle = Database::open_with_busy_timeout(&database, std::time::Duration::from_secs(5))
        .expect("the fixture opens");
    let connection = handle.connect().expect("the connection opens");
    let mut failures = Vec::new();
    for sql in statements {
        let observation = driver
            .send(&Op::Query((*sql).to_string()))
            .expect("the oracle answers");
        let ours = rustdb_rows(&connection, sql);
        if !observation.ok {
            // SQLite refused it, so rust-db must refuse it too. The messages
            // are not compared: they are prose, and this suite grades answers.
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
        // A statement with no `ORDER BY` has no defined row order, so the two
        // answers are compared as multisets. Comparing them in order would
        // grade the access path each engine happened to choose - and SQLite
        // reading a covering index while rust-db scans the table is a
        // difference in speed, not in answer.
        let ordered = sql.to_ascii_uppercase().contains("ORDER BY");
        match ours {
            Err(reason) => failures.push(format!("{sql}\n  rust-db failed: {reason}")),
            Ok(actual) => {
                let (mut left, mut right) = (expected.clone(), actual.clone());
                if !ordered {
                    left.sort();
                    right.sort();
                }
                if left != right {
                    failures.push(format!("{sql}\n  sqlite:  {left:?}\n  rust-db: {right:?}"));
                }
            }
        }
    }
    assert!(
        failures.is_empty(),
        "{} of {} statements diverged:\n{}",
        failures.len(),
        statements.len(),
        failures.join("\n")
    );
}

/// Every join form returns exactly what SQLite returns, including the rows an
/// outer join keeps and the NULLs it fills them with.
#[test]
fn joins_match_the_oracle() {
    grade("joins", &[
        "SELECT a.name, b.region FROM a, b WHERE a.team = b.team ORDER BY a.id",
        "SELECT a.name, b.region FROM a JOIN b ON a.team = b.team ORDER BY a.id",
        "SELECT a.name, b.region FROM a CROSS JOIN b ORDER BY a.id, b.id",
        "SELECT a.name, b.region FROM a LEFT JOIN b ON a.team = b.team ORDER BY a.id",
        "SELECT a.name, b.region FROM a LEFT OUTER JOIN b ON a.team = b.team ORDER BY a.id",
        "SELECT count(*) FROM a LEFT JOIN b ON a.team = b.team",
        "SELECT a.name, b.region FROM a LEFT JOIN b ON a.team = b.team WHERE b.region IS NULL ORDER BY a.id",
        "SELECT a.name, b.region FROM a LEFT JOIN b ON a.team = b.team AND b.rank > 1 ORDER BY a.id",
        "SELECT * FROM a JOIN b USING (team) ORDER BY a.id",
        "SELECT * FROM a NATURAL JOIN b ORDER BY a.id",
        "SELECT a.name, b.region, c.v FROM a LEFT JOIN b ON a.team = b.team LEFT JOIN c ON c.k = b.rank ORDER BY a.id, c.v",
        "SELECT a.name FROM a LEFT JOIN b ON a.team = b.team WHERE b.rank IS NULL ORDER BY a.id",
    ]);
}

/// Compound selects, including the de-duplication each operator applies to
/// everything to its left.
#[test]
fn compounds_match_the_oracle() {
    grade(
        "compounds",
        &[
            "SELECT team FROM a UNION SELECT team FROM b",
            "SELECT team FROM a UNION ALL SELECT team FROM b",
            "SELECT team FROM a INTERSECT SELECT team FROM b",
            "SELECT team FROM a EXCEPT SELECT team FROM b",
            "SELECT k FROM c UNION SELECT id FROM b ORDER BY 1",
            "SELECT k FROM c UNION ALL SELECT k FROM c ORDER BY 1",
            "SELECT k FROM c UNION ALL SELECT k FROM c UNION SELECT 99 ORDER BY 1",
            "SELECT team FROM a UNION SELECT team FROM b ORDER BY team DESC",
            "SELECT team FROM a UNION SELECT team FROM b ORDER BY 1 LIMIT 2",
            "SELECT team FROM a UNION SELECT team FROM b ORDER BY 1 LIMIT 2 OFFSET 1",
            "SELECT 1, 'x' UNION SELECT 2, 'y' ORDER BY 1",
            "SELECT id, name FROM a UNION ALL SELECT id, region FROM b ORDER BY 1, 2",
            "VALUES (1, 'a'), (2, 'b') UNION SELECT 3, 'c'",
        ],
    );
}

/// Subqueries in every position, correlated and not.
#[test]
fn subqueries_match_the_oracle() {
    grade("subqueries", &[
        "SELECT * FROM (SELECT id, name FROM a WHERE id > 2) ORDER BY id",
        "SELECT t.name FROM (SELECT id, name FROM a) AS t WHERE t.id = 3",
        "SELECT n FROM (SELECT name AS n, score FROM a ORDER BY score DESC LIMIT 2) ORDER BY n",
        "SELECT a.name FROM a WHERE a.team IN (SELECT team FROM b) ORDER BY a.id",
        "SELECT a.name FROM a WHERE a.team NOT IN (SELECT team FROM b) ORDER BY a.id",
        "SELECT a.name FROM a WHERE a.team NOT IN (SELECT region FROM b) ORDER BY a.id",
        "SELECT a.name FROM a WHERE EXISTS (SELECT 1 FROM b WHERE b.team = a.team) ORDER BY a.id",
        "SELECT a.name FROM a WHERE NOT EXISTS (SELECT 1 FROM b WHERE b.team = a.team) ORDER BY a.id",
        "SELECT a.name, (SELECT b.region FROM b WHERE b.team = a.team) FROM a ORDER BY a.id",
        "SELECT a.name, (SELECT count(*) FROM b) FROM a ORDER BY a.id",
        "SELECT (SELECT max(score) FROM a), (SELECT min(score) FROM a)",
        "SELECT a.name FROM a WHERE a.score > (SELECT avg(score) FROM a) ORDER BY a.id",
        "SELECT x.name, y.region FROM (SELECT * FROM a) AS x LEFT JOIN (SELECT * FROM b) AS y ON x.team = y.team ORDER BY x.id",
        "SELECT k FROM c WHERE k IN (SELECT k FROM c WHERE v IS NULL)",
    ]);
}

/// Views behave as the query they stand for, and refuse to be written.
#[test]
fn views_match_the_oracle() {
    grade("views", &[
        "SELECT * FROM blue ORDER BY id",
        "SELECT name FROM blue WHERE score > 1 ORDER BY id",
        "SELECT * FROM ranked ORDER BY who",
        "SELECT count(*) FROM blue",
        "SELECT b.region FROM blue JOIN a ON a.id = blue.id JOIN b ON b.team = a.team ORDER BY blue.id",
        "INSERT INTO blue VALUES (9, 'x', 1.0)",
        "UPDATE blue SET name = 'x'",
        "DELETE FROM blue",
    ]);
}

/// Common table expressions, ordinary and recursive.
#[test]
fn ctes_match_the_oracle() {
    grade("ctes", &[
        "WITH t AS (SELECT id, name FROM a WHERE id > 2) SELECT * FROM t ORDER BY id",
        "WITH t(x, y) AS (SELECT id, name FROM a) SELECT y FROM t WHERE x = 1",
        "WITH t AS (SELECT team FROM a) SELECT count(*) FROM t",
        "WITH t AS (SELECT id FROM a), u AS (SELECT id FROM b) SELECT * FROM t UNION ALL SELECT * FROM u ORDER BY 1",
        "WITH t AS (SELECT id FROM a) SELECT * FROM t AS x JOIN t AS y ON x.id = y.id ORDER BY x.id",
        "WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 5) SELECT i FROM n",
        "WITH RECURSIVE n(i) AS (VALUES(1) UNION ALL SELECT i * 2 FROM n WHERE i < 40) SELECT i FROM n",
        "WITH RECURSIVE n(i) AS (SELECT 1 UNION SELECT 1 FROM n) SELECT count(*) FROM n",
    ]);
}
