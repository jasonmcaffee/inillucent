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

use inillucent_compat::facade::Database;
use inillucent_compat::oracle::{Driver, Op, TaggedValue};
use inillucent_compat::rendering::tagged as render;
use inillucent_compat::workspace_root;

/// Returns the pinned oracle binary, when it has been built.
fn oracle_path() -> Option<PathBuf> {
    if let Ok(explicit) = std::env::var("INILLUCENT_SQLITE_ORACLE") {
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
    // The two rowids an integer overflow can fold onto, so a seek key that
    // wrapped would find a row rather than nothing (task-1932, H7).
    "CREATE TABLE edge (id INTEGER PRIMARY KEY, tag TEXT)",
    "INSERT INTO edge VALUES (-9223372036854775808, 'floor')",
    "INSERT INTO edge VALUES (9223372036854775807, 'ceiling')",
    "INSERT INTO edge VALUES (1, 'one')",
    // One-column tables, for the `IN table-name` form: `IN` over a table is
    // `IN` over its single column, and a table of more than one column is
    // refused (task-1913).
    "CREATE TABLE teams (team TEXT)",
    "INSERT INTO teams VALUES ('blue')",
    "INSERT INTO teams VALUES ('red')",
    "CREATE TABLE scores (score REAL)",
    "INSERT INTO scores VALUES (10.5)",
    "INSERT INTO scores VALUES (99.25)",
    "INSERT INTO scores VALUES (NULL)",
    "CREATE VIEW blue AS SELECT id, name, score FROM a WHERE team = 'blue'",
    "CREATE VIEW ranked (who, place) AS SELECT a.name, b.rank FROM a JOIN b ON a.team = b.team",
];

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

/// Runs a statement through inillucent, returning its rows or its failure.
fn inillucent_rows(
    connection: &inillucent_compat::facade::Connection,
    sql: &str,
) -> Result<Vec<String>, String> {
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
        inillucent_compat::differential::skipping("the pinned SQLite oracle is not built");
        return;
    };
    let handle = Database::import_with_busy_timeout(&database, std::time::Duration::from_secs(5))
        .expect("the fixture opens");
    let connection = handle.session().expect("the connection opens");
    let mut failures = Vec::new();
    for sql in statements {
        let observation = driver
            .send(&Op::Query((*sql).to_string()))
            .expect("the oracle answers");
        let ours = inillucent_rows(&connection, sql);
        if !observation.ok {
            // SQLite refused it, so inillucent must refuse it too. The messages
            // are not compared: they are prose, and this suite grades answers.
            if ours.is_ok() {
                failures.push(format!(
                    "{sql}\n  sqlite refused: {}\n  inillucent accepted it",
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
        // reading a covering index while inillucent scans the table is a
        // difference in speed, not in answer.
        let ordered = sql.to_ascii_uppercase().contains("ORDER BY");
        match ours {
            Err(reason) => failures.push(format!("{sql}\n  inillucent failed: {reason}")),
            Ok(actual) => {
                let (mut left, mut right) = (expected.clone(), actual.clone());
                if !ordered {
                    left.sort();
                    right.sort();
                }
                if left != right {
                    failures.push(format!(
                        "{sql}\n  sqlite:  {left:?}\n  inillucent: {right:?}"
                    ));
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
        "SELECT a.name, b.region FROM a RIGHT JOIN b ON a.team = b.team ORDER BY b.id, a.id",
        "SELECT a.name, b.region FROM a RIGHT OUTER JOIN b ON a.team = b.team ORDER BY b.id, a.id",
        "SELECT count(*) FROM a RIGHT JOIN b ON a.team = b.team",
        "SELECT a.name, b.region FROM a FULL JOIN b ON a.team = b.team ORDER BY a.id, b.id",
        "SELECT a.name, b.region FROM a FULL OUTER JOIN b ON a.team = b.team ORDER BY a.id, b.id",
        "SELECT count(*) FROM a FULL JOIN b ON a.team = b.team",
        "SELECT a.name, b.region FROM a RIGHT JOIN b ON a.team = b.team WHERE a.name IS NULL ORDER BY b.id",
        "SELECT a.name, b.region FROM a FULL JOIN b ON a.team = b.team AND b.rank > 1 ORDER BY a.id, b.id",
        "SELECT a.name, b.region, c.v FROM a RIGHT JOIN b ON a.team = b.team JOIN c ON c.k = b.rank ORDER BY b.id, c.v",
        // **A cross join whose inner side is read through a covering index
        // (task-1913).** `a_team` covers `SELECT count(*) FROM c, a`, so the
        // planner reads `a` as `SCAN a USING COVERING INDEX a_team` - an index
        // seek with no equality and no bound, which is a full scan of the
        // index and therefore exactly the cross product an empty probe key
        // means. The physical pass refused it together with the *bounded*
        // no-equality case, so a plain `FROM c, a` failed outright.
        "SELECT count(*) FROM c, a",
        "SELECT count(*) FROM a, c",
        "SELECT c.v, a.name FROM c, a ORDER BY c.v, a.name",
        "SELECT count(*) FROM c, edge",
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

/// A compound's `ORDER BY` sorts under the collation the term names.
///
/// **The differential corpus cannot catch this and this test can (task-1979,
/// F15).** Its case `cmp-014` is
/// `SELECT 'b' AS a UNION SELECT 'a' ORDER BY a COLLATE NOCASE`, and `'a'`
/// sorts before `'b'` under `BINARY` and under `NOCASE` alike - so the case
/// passes whether the named collation is applied or thrown away, which is the
/// shape rule 4 of the testing standard calls a test that cannot fail. Every
/// statement here mixes the cases, so the two collations disagree about the
/// order and only the right one matches the oracle: under `BINARY` every
/// capital sorts ahead of every lower-case letter, under `NOCASE` they
/// interleave.
///
/// The last two are the control. They name no collation, so they must keep
/// answering what they answered before - the result column's own collation,
/// which for a compound is also the one its duplicate removal uses.
#[test]
fn a_compound_ordered_by_a_named_collation_matches_the_oracle() {
    grade(
        "compound-collation",
        &[
            "SELECT 'B' AS a UNION SELECT 'a' ORDER BY a COLLATE NOCASE",
            "SELECT 'B' AS a UNION SELECT 'a' UNION SELECT 'C' ORDER BY a COLLATE NOCASE",
            "SELECT 'B' AS a UNION SELECT 'a' ORDER BY a COLLATE NOCASE DESC",
            "SELECT 'B' AS a UNION SELECT 'a' ORDER BY 1 COLLATE NOCASE",
            "SELECT 'B' AS a UNION SELECT 'a' UNION SELECT 'C' ORDER BY a COLLATE BINARY",
            "SELECT 'B' AS a UNION SELECT 'a' UNION SELECT 'C' ORDER BY a",
            "SELECT 'B' AS a UNION SELECT 'a' UNION SELECT 'C' ORDER BY 1",
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
        // **`RECURSIVE` is optional in SQLite, and these prove it here
        // (task-1913).** A CTE whose FROM names itself is the recursion
        // whether or not the keyword is written. Reading the keyword as the
        // only evidence made the binder bind the same definition inside
        // itself, and the process ran out of stack - so before the fix this
        // file did not fail, it died.
        "WITH n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 5) SELECT i FROM n",
        "WITH n(i) AS (VALUES(1) UNION ALL SELECT i * 2 FROM n WHERE i < 40) SELECT i FROM n",
        "WITH n AS (SELECT 1 AS i UNION ALL SELECT i + 1 FROM n WHERE i < 3) SELECT sum(i) FROM n",
        // An inner `WITH` binding the same name shadows the outer one, so this
        // is an ordinary query and not a recursion - which is why the test for
        // a self-reference stops at a nested rebinding of the name.
        "WITH n AS (WITH n AS (SELECT 7 AS i) SELECT * FROM n) SELECT i FROM n",
        // A CTE may name one declared after it, which is a forward reference
        // rather than a cycle.
        "WITH x AS (SELECT * FROM y), y AS (SELECT 1 AS i) SELECT i FROM x",
    ]);
}

/// An aggregate's `FILTER (WHERE ...)` under a `GROUP BY` that streams.
///
/// **A wrong answer this ticket found rather than one the review named.** The
/// M6 test below refused to pass until it was fixed, and it has nothing to do
/// with subqueries: `StreamAggregate::push` has two paths, and only one of them
/// applied `FILTER`. The dense path - one bare integer key column over a dense
/// batch - folds each run through `fold_run`, which checks `spec.filter`. The
/// general path, which is what a text or multi-column group key takes, pushed
/// the argument straight into the accumulator and never looked at the filter at
/// all. So `SELECT team, count(*) FILTER (WHERE score > 0) FROM a GROUP BY
/// team` counted every row of every group, while the same statement grouped by
/// an integer column answered correctly.
///
/// That is why it went unnoticed: the two group-key types take different paths
/// and only one of them was wrong. Both are graded here, over the same
/// predicate, against the same oracle.
#[test]
fn an_aggregate_filter_under_a_group_by_matches_the_oracle() {
    grade(
        "aggregate-filter",
        &[
            // A text group key: the general path, which ignored the filter.
            "SELECT team, count(*) FILTER (WHERE score > 0) FROM a GROUP BY team ORDER BY team",
            "SELECT team, sum(id) FILTER (WHERE score > 0) FROM a GROUP BY team ORDER BY team",
            "SELECT team, count(*) FILTER (WHERE 0) FROM a GROUP BY team ORDER BY team",
            "SELECT team, count(*) FILTER (WHERE 1) FROM a GROUP BY team ORDER BY team",
            "SELECT team, count(*) FILTER (WHERE name IS NULL) FROM a GROUP BY team ORDER BY team",
            "SELECT team, group_concat(name, '-') FILTER (WHERE id > 1) FROM a GROUP BY team ORDER BY team",
            "SELECT team, min(id) FILTER (WHERE id > 2), max(id) FILTER (WHERE id > 2) FROM a GROUP BY team ORDER BY team",
            // Two calls, one filtered and one not, so a fix that filtered
            // everything would fail here.
            "SELECT team, count(*), count(*) FILTER (WHERE score > 0) FROM a GROUP BY team ORDER BY team",
            // An integer group key: the dense path, which was already right.
            "SELECT id, count(*) FILTER (WHERE score > 0) FROM a GROUP BY id ORDER BY id",
            "SELECT id, sum(id) FILTER (WHERE id > 2) FROM a GROUP BY id ORDER BY id",
            // Grouped by an expression, which is neither.
            "SELECT id % 2, count(*) FILTER (WHERE score > 0) FROM a GROUP BY id % 2 ORDER BY 1",
            // And an inner ORDER BY, which the general path also skipped.
            "SELECT team, group_concat(name, '-' ORDER BY id DESC) FROM a GROUP BY team ORDER BY team",
        ],
    );
}

/// A correlated subquery inside an aggregate's argument, its `FILTER` and its
/// inner `ORDER BY`.
///
/// **What was wrong.** `correlate::gather_select` walked the columns, the
/// filter, the having, the group by, the order by and the join constraints,
/// and never `select.aggregates` - which is a list of its own, beside
/// `select.columns` rather than inside it. A subquery written as an
/// aggregate's argument was therefore never recognised as a correlated block,
/// its slot was never filled, and `translate` reported the empty slot as
/// `unsupported("a correlated subquery used as a value")` - a true statement
/// about the slot and a false one about the query. The whole shape was refused
/// with exit code 3.
#[test]
fn a_correlated_subquery_in_an_aggregate_argument_matches_the_oracle() {
    grade(
        "aggregate-subquery",
        &[
            "SELECT team, SUM((SELECT count(*) FROM b WHERE b.team = a.team)) FROM a GROUP BY team ORDER BY team",
            "SELECT team, max((SELECT b.rank FROM b WHERE b.team = a.team)) FROM a GROUP BY team ORDER BY team",
            "SELECT count((SELECT b.rank FROM b WHERE b.team = a.team)) FROM a",
            "SELECT team, group_concat((SELECT b.region FROM b WHERE b.team = a.team), '-') FROM a GROUP BY team ORDER BY team",
            "SELECT team, count(*) FILTER (WHERE (SELECT count(*) FROM b WHERE b.team = a.team) > 0) FROM a GROUP BY team ORDER BY team",
            "SELECT team, sum(id + (SELECT count(*) FROM b WHERE b.team = a.team)) FROM a GROUP BY team ORDER BY team",
            // An uncorrelated subquery in the same places, which folds rather
            // than correlating and went through a different path.
            "SELECT team, SUM((SELECT count(*) FROM b)) FROM a GROUP BY team ORDER BY team",
            "SELECT sum(id) FILTER (WHERE id > (SELECT min(id) FROM b)) FROM a",
            // And in a window function's argument, partition and order, which
            // the same walk was missing.
            "SELECT name, sum((SELECT count(*) FROM b WHERE b.team = a.team)) OVER (ORDER BY id) FROM a ORDER BY id",
            "SELECT name, count(*) OVER (PARTITION BY (SELECT count(*) FROM b WHERE b.team = a.team)) FROM a ORDER BY id",
        ],
    );
}

/// An arithmetic overflow in a seek key, a range bound and a projection.
///
/// **What this catches.** `constant::fold` - the folder that turns
/// `WHERE id = <constant expression>` into the key a scan seeks to - had its
/// own arithmetic, built on `wrapping_add` and its siblings, while the row
/// evaluator promotes an integer overflow to a double the way SQLite does.
/// `WHERE id = 9223372036854775807 + 1` therefore folded to `i64::MIN`, and
/// the fixture above has a row there: the query returned `floor` where SQLite
/// returns nothing. An empty table would have hidden it, because an empty
/// result is the right answer by accident. The same three statements graded
/// through a projection instead of a seek key exercise the evaluator's own
/// path, so a later change that fixes one and not the other fails here.
#[test]
fn integer_overflow_in_a_seek_key_matches_the_oracle() {
    grade(
        "overflow",
        &[
            "SELECT tag FROM edge WHERE id = 9223372036854775807 + 1",
            "SELECT tag FROM edge WHERE id = -9223372036854775807 - 2",
            "SELECT tag FROM edge WHERE id = 9223372036854775807 * 2",
            "SELECT tag FROM edge WHERE id > 9223372036854775807 + 1 ORDER BY id",
            "SELECT tag FROM edge WHERE id < -9223372036854775807 - 2 ORDER BY id",
            "SELECT tag FROM edge WHERE id = 1 + 0 ORDER BY id",
            "SELECT 9223372036854775807 + 1, typeof(9223372036854775807 + 1)",
            "SELECT -9223372036854775807 - 2, typeof(-9223372036854775807 - 2)",
            "SELECT 9223372036854775807 * 2, typeof(9223372036854775807 * 2)",
            "SELECT 'abc' + 1, typeof('abc' + 1), '4' + 1, typeof('4' + 1)",
            "SELECT tag FROM edge WHERE id = 'abc' + 1",
            "SELECT tag FROM edge WHERE id = '1' + 0",
            "SELECT tag FROM edge WHERE id = NULL + 1",
        ],
    );
}

/// Window functions: every frame unit, every bound, every `EXCLUDE`, and the
/// eleven functions that only exist in a window.
///
/// **This test was retired and is back (task-1932, H1).** It was removed on
/// the evidence that "the shipping engine's physical pass refuses every
/// `OVER (...)` statement outright", and `sql.select.window` and
/// `functions.window` were moved to `missing` in `compat/sqlite-3.53.4.toml`
/// on that reading. The capability was not missing. `run_windowed` answered
/// all forty-one of these statements the whole time; what refused them was
/// `compiled::try_compile`, which bailed out on `plan.compounds` and not on
/// `plan.select.windows`, so every application entry point - which all go
/// through the cached path - hit `refuse_unhandled` instead of the evaluator.
/// One `Ok(None)` in `try_compile` reconnects the two, and the grading below
/// is what says the evaluator is right rather than merely reachable.
/// Window functions: every frame unit, every bound, every `EXCLUDE`, and the
/// eleven functions that only exist in a window.
#[test]
fn windows_match_the_oracle() {
    grade(
        "windows",
        &[
            "SELECT name, row_number() OVER () FROM a ORDER BY id",
            "SELECT name, row_number() OVER (ORDER BY score) FROM a ORDER BY id",
            "SELECT name, rank() OVER (ORDER BY team) FROM a ORDER BY id",
            "SELECT name, dense_rank() OVER (ORDER BY team) FROM a ORDER BY id",
            "SELECT name, percent_rank() OVER (ORDER BY team) FROM a ORDER BY id",
            "SELECT name, cume_dist() OVER (ORDER BY team) FROM a ORDER BY id",
            "SELECT name, ntile(3) OVER (ORDER BY id) FROM a ORDER BY id",
            "SELECT name, ntile(2) OVER (PARTITION BY team ORDER BY id) FROM a ORDER BY id",
            "SELECT name, lag(name) OVER (ORDER BY id) FROM a ORDER BY id",
            "SELECT name, lag(name, 2) OVER (ORDER BY id) FROM a ORDER BY id",
            "SELECT name, lag(name, 2, 'none') OVER (ORDER BY id) FROM a ORDER BY id",
            "SELECT name, lead(name) OVER (ORDER BY id) FROM a ORDER BY id",
            "SELECT name, lead(name, 3, 'none') OVER (ORDER BY id) FROM a ORDER BY id",
            "SELECT name, first_value(name) OVER (ORDER BY id) FROM a ORDER BY id",
            "SELECT name, last_value(name) OVER (ORDER BY id) FROM a ORDER BY id",
            "SELECT name, nth_value(name, 2) OVER (ORDER BY id) FROM a ORDER BY id",
            "SELECT name, count(*) OVER () FROM a ORDER BY id",
            "SELECT name, count(*) OVER (PARTITION BY team) FROM a ORDER BY id",
            "SELECT name, sum(id) OVER (ORDER BY id) FROM a ORDER BY id",
            "SELECT name, sum(id) OVER (ORDER BY id ROWS BETWEEN 1 PRECEDING AND 1 FOLLOWING) FROM a ORDER BY id",
            "SELECT name, sum(id) OVER (ORDER BY id ROWS BETWEEN UNBOUNDED PRECEDING AND UNBOUNDED FOLLOWING) FROM a ORDER BY id",
            "SELECT name, sum(id) OVER (ORDER BY id ROWS BETWEEN CURRENT ROW AND UNBOUNDED FOLLOWING) FROM a ORDER BY id",
            "SELECT name, sum(id) OVER (ORDER BY id ROWS 2 PRECEDING) FROM a ORDER BY id",
            "SELECT name, avg(score) OVER (PARTITION BY team ORDER BY id) FROM a ORDER BY id",
            "SELECT name, min(id) OVER (ORDER BY team), max(id) OVER (ORDER BY team) FROM a ORDER BY id",
            "SELECT name, group_concat(name, '-') OVER (ORDER BY id) FROM a ORDER BY id",
            "SELECT name, count(*) OVER (ORDER BY team RANGE BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW) FROM a ORDER BY id",
            "SELECT name, count(*) OVER (ORDER BY team GROUPS BETWEEN 1 PRECEDING AND 1 FOLLOWING) FROM a ORDER BY id",
            "SELECT name, count(*) OVER (ORDER BY team RANGE BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW EXCLUDE CURRENT ROW) FROM a ORDER BY id",
            "SELECT name, count(*) OVER (ORDER BY team RANGE BETWEEN UNBOUNDED PRECEDING AND UNBOUNDED FOLLOWING EXCLUDE GROUP) FROM a ORDER BY id",
            "SELECT name, count(*) OVER (ORDER BY team RANGE BETWEEN UNBOUNDED PRECEDING AND UNBOUNDED FOLLOWING EXCLUDE TIES) FROM a ORDER BY id",
            "SELECT name, count(*) FILTER (WHERE score > 0) OVER (PARTITION BY team) FROM a ORDER BY id",
            "SELECT name, sum(id) FILTER (WHERE id > 2) OVER () FROM a ORDER BY id",
            "SELECT name, row_number() OVER w FROM a WINDOW w AS (ORDER BY id) ORDER BY id",
            "SELECT name, row_number() OVER (w ORDER BY id DESC) FROM a WINDOW w AS (PARTITION BY team) ORDER BY id",
            "SELECT name, row_number() OVER (PARTITION BY team ORDER BY id), count(*) OVER (PARTITION BY score) FROM a ORDER BY id",
            "SELECT id + row_number() OVER (ORDER BY id) FROM a ORDER BY id",
            "SELECT name FROM a WHERE id > 1 ORDER BY row_number() OVER (ORDER BY id DESC)",
            "SELECT team, count(*), row_number() OVER (ORDER BY team) FROM a GROUP BY team ORDER BY team",
            "SELECT name, sum(id) OVER (ORDER BY id) FROM a ORDER BY id LIMIT 2 OFFSET 1",
            "SELECT DISTINCT count(*) OVER (PARTITION BY team) FROM a ORDER BY 1",
            // **A `RANGE` offset over a column that holds a NULL (task-1913).**
            // `a.score` is NULL for one row. A NULL ordering value has no
            // distance to anything, so SQLite gives such a row the frame of
            // its own peer group - every NULL row and nothing else - and never
            // lets it inside the frame of a row that has a value. This engine
            // read a NULL as `0.0`, which put the NULL row one unit from zero:
            // `a` holds a score of `0.0` and one of `-2.0`, so the wrong rows
            // were drawn into each other's frames in both directions. The
            // `UNBOUNDED` arms are here too, because those bounds *do* reach a
            // NULL row and the fix must not stop them.
            "SELECT name, sum(score) OVER (ORDER BY score RANGE BETWEEN 1 PRECEDING AND 1 FOLLOWING) FROM a ORDER BY id",
            "SELECT name, count(*) OVER (ORDER BY score RANGE BETWEEN 1 PRECEDING AND 1 FOLLOWING) FROM a ORDER BY id",
            "SELECT name, sum(score) OVER (ORDER BY score RANGE BETWEEN UNBOUNDED PRECEDING AND 1 FOLLOWING) FROM a ORDER BY id",
            "SELECT name, sum(score) OVER (ORDER BY score RANGE BETWEEN 1 PRECEDING AND UNBOUNDED FOLLOWING) FROM a ORDER BY id",
            "SELECT name, sum(score) OVER (ORDER BY score DESC RANGE BETWEEN 1 PRECEDING AND 1 FOLLOWING) FROM a ORDER BY id",
            "SELECT name, sum(id) OVER (ORDER BY rank RANGE BETWEEN 2 PRECEDING AND 2 FOLLOWING) FROM b ORDER BY id",
            // **A frame written entirely off one end of the partition
            // (task-1913).** On the last row `1 FOLLOWING AND 2 FOLLOWING`
            // names rows that are not there, and the answer is NULL rather
            // than the row itself. Clamping both ends into the partition
            // instead left a frame of one row. Both edges and all three units
            // are here: `ROWS` was wrong at both, `GROUPS` and `RANGE` at the
            // start only.
            "SELECT name, sum(id) OVER (ORDER BY id ROWS BETWEEN 1 FOLLOWING AND 2 FOLLOWING) FROM a ORDER BY id",
            "SELECT name, sum(id) OVER (ORDER BY id ROWS BETWEEN 2 PRECEDING AND 1 PRECEDING) FROM a ORDER BY id",
            "SELECT name, sum(id) OVER (ORDER BY id GROUPS BETWEEN 1 FOLLOWING AND 2 FOLLOWING) FROM a ORDER BY id",
            "SELECT name, sum(id) OVER (ORDER BY id GROUPS BETWEEN 2 PRECEDING AND 1 PRECEDING) FROM a ORDER BY id",
            "SELECT name, sum(id) OVER (ORDER BY id RANGE BETWEEN 1 FOLLOWING AND 2 FOLLOWING) FROM a ORDER BY id",
            "SELECT name, sum(id) OVER (ORDER BY id RANGE BETWEEN 2 PRECEDING AND 1 PRECEDING) FROM a ORDER BY id",
            "SELECT name, count(*) OVER (ORDER BY id ROWS BETWEEN 9 FOLLOWING AND 9 FOLLOWING) FROM a ORDER BY id",
            "SELECT name, count(*) OVER (PARTITION BY team ORDER BY id ROWS BETWEEN 1 FOLLOWING AND 2 FOLLOWING) FROM a ORDER BY id",
        ],
    );
}
/// The math built-ins, over a value matrix that includes the awkward cases:
/// a domain error, a non-numeric argument, the integer/real boundary, and the
/// two functions whose meaning changes with their argument count.
#[test]
fn math_functions_match_the_oracle() {
    grade(
        "math",
        &[
            "SELECT pi()",
            "SELECT abs(-3), abs(-3.5), abs('x'), abs(NULL)",
            "SELECT ceil(1.2), ceil(-1.2), ceiling(1.0), floor(1.8), floor(-1.8)",
            "SELECT trunc(1.9), trunc(-1.9), trunc(2)",
            "SELECT sqrt(4), sqrt(2), sqrt(0), sqrt(-1), sqrt('4'), sqrt('four'), sqrt(NULL)",
            "SELECT exp(0), exp(1), ln(1), ln(0), ln(-1)",
            "SELECT log(100), log(10, 1000), log(1, 5), log(0), log10(1000), log2(8)",
            "SELECT sin(0), cos(0), tan(0)",
            "SELECT asin(1), acos(1), atan(1), asin(2), acos(2)",
            "SELECT sinh(0), cosh(0), tanh(0), asinh(0), acosh(1), atanh(0)",
            "SELECT acosh(0), atanh(1), atanh(-1)",
            "SELECT atan2(1, 1), atan2(0, 1)",
            "SELECT degrees(pi()), radians(180)",
            "SELECT mod(7, 3), mod(-7, 3), mod(7, -3), mod(7.5, 2), mod(7, 0)",
            "SELECT pow(2, 10), power(2, 0.5), pow(-8, 2)",
            "SELECT typeof(ceil(1)), typeof(floor(1)), typeof(sqrt(4))",
            "SELECT ln(x'00'), sqrt(x'00')",
            "SELECT round(2.5), round(-2.5), round(2.345, 2), round(1)",
            "SELECT max(1, 2, 3), min(1, 2, 3), max(1, NULL), min(NULL, 1)",
        ],
    );
}

/// The date and time built-ins.
///
/// Nothing here names `'now'`: the two engines read the clock microseconds
/// apart and a test that compared them would fail whenever those microseconds
/// crossed a second. Every value is a fixed timestamp, and the wall clock is
/// covered by the unit test in `datetime.rs` instead.
///
/// **`localtime` and `utc` are graded here, and they were the one deliberate
/// difference in this table until task-1981 implemented them.** They answered
/// NULL and did nothing, because both of SQLite's answers depend on the
/// operating system's time zone database and on the zone the process is running
/// in - so the same query answered differently on two machines and differently
/// again after a daylight saving change, and this engine is used to grade itself
/// against a second process. `inillucent-vfs`'s `zone` module now asks the
/// operating system for the offset one instant at a time, which is what
/// `date.c` does, so the two engines running on one machine agree.
///
/// They are graded rather than asserted for exactly the reason they used to be
/// refused: the answer is the machine's zone and neither engine can be asked
/// for it in advance, but the two of them on one machine have to give the same
/// one. A regression to NULL fails here, because SQLite never answers NULL for
/// either. A winter instant and a summer one, so a zone that observes daylight
/// saving exercises both of its offsets (task-1987).
///
/// They are graded *here*, in the test `compat/sqlite-3.53.4.toml` already
/// cites, rather than in a case of their own. A new test identifier has no
/// recorded result on either required platform, so citing one would drop
/// `functions.date-time` from `pass` to `partial` until a Linux run recorded it
/// - and the alternative, writing a Linux result nobody observed, is not
/// evidence.
#[test]
fn date_and_time_functions_match_the_oracle() {
    grade(
        "datetime",
        &[
            "SELECT date('2026-09-03'), time('2026-09-03 14:30:15'), datetime('2026-09-03 14:30:15')",
            "SELECT date('2026-09-03T14:30:15'), datetime('2026-09-03T14:30:15Z')",
            "SELECT julianday('2026-09-03'), julianday('1970-01-01'), julianday('2000-01-01 12:00:00')",
            "SELECT unixepoch('1970-01-01'), unixepoch('2026-09-03'), unixepoch('2026-09-03 14:30:15')",
            "SELECT date(2451545.0), datetime(2451545.0)",
            "SELECT datetime(0, 'unixepoch'), datetime(1000000000, 'unixepoch')",
            "SELECT datetime('2026-09-03', '+1 day'), datetime('2026-09-03', '-1 day')",
            "SELECT datetime('2026-09-03', '+3 hours'), datetime('2026-09-03', '+90 minutes')",
            "SELECT datetime('2026-09-03 00:00:00', '+30 seconds')",
            "SELECT date('2026-01-31', '+1 month'), date('2026-03-31', '-1 month')",
            "SELECT date('2024-02-29', '+1 year'), date('2026-09-03', '-2 years')",
            "SELECT date('2026-09-03', 'start of month'), date('2026-09-03', 'start of year')",
            "SELECT datetime('2026-09-03 14:30:15', 'start of day')",
            "SELECT date('2026-09-03', 'weekday 0'), date('2026-09-03', 'weekday 3'), date('2026-09-03', 'weekday 6')",
            "SELECT date('2026-09-03', 'start of month', '+1 month', '-1 day')",
            "SELECT strftime('%Y-%m-%d', '2026-09-03')",
            "SELECT strftime('%Y/%m/%d %H:%M:%S', '2026-09-03 14:30:15')",
            "SELECT strftime('%d %m %Y %H %M %S %j %w %W', '2026-09-03 14:30:15')",
            "SELECT strftime('%s', '2026-09-03'), strftime('%J', '2026-09-03')",
            "SELECT strftime('%f', '2026-09-03 14:30:15.250')",
            "SELECT strftime('%%|%Y', '2026-09-03')",
            "SELECT date('not a date'), time('nonsense'), julianday('xyz'), unixepoch('xyz')",
            "SELECT date(NULL), datetime(NULL), strftime('%Y', NULL)",
            "SELECT date('2026-09-03', 'bogus modifier')",
            "SELECT timediff('2026-09-03', '2026-09-01')",
            "SELECT timediff('2026-09-01', '2026-09-03')",
            "SELECT timediff('2026-03-01', '2026-01-31')",
            "SELECT date('1582-10-15'), julianday('1582-10-15'), date('1200-06-06')",
            "SELECT datetime('2026-09-03 14:30:15+02:00'), datetime('2026-09-03 14:30:15-05:30')",
            "SELECT date('2026-09-03 25:00:00'), date('2026-13-01'), date('2026-09-32')",
            "SELECT datetime('2026-09-03 14:30:00', 'localtime')",
            "SELECT datetime('2026-09-03 14:30:00', 'utc')",
            "SELECT datetime('2026-01-15 03:00:00', 'localtime')",
            "SELECT datetime('2026-01-15 03:00:00', 'utc')",
            "SELECT datetime('2026-09-03 14:30:00', 'localtime') IS NULL",
            "SELECT date('2026-09-03 14:30:00', 'localtime'), time('2026-09-03 14:30:00', 'utc')",
        ],
    );
}

/// The core scalar built-ins, with `printf` the substantial one.
///
/// `random()`, `randomblob()` and `sqlite_source_id()` are deliberately absent:
/// two engines cannot agree on a random number, and the source id names the
/// build rather than the behaviour. Their shape is covered by unit tests.
#[test]
fn core_functions_match_the_oracle() {
    grade(
        "core",
        &[
            "SELECT printf('%d', 42), printf('%d', -42), printf('%i', 7), printf('%u', 3)",
            "SELECT printf('%5d|', 42), printf('%-5d|', 42), printf('%05d', 42), printf('%05d', -42)",
            "SELECT printf('%+d %+d % d', 42, -42, 42)",
            "SELECT printf('%.3d', 7), printf('%8.3d|', 7)",
            "SELECT printf('%x %X %o', 255, 255, 8)",
            "SELECT printf('%#x %#X %#o', 255, 255, 8)",
            "SELECT printf('%f', 3.5), printf('%.2f', 3.14159), printf('%.0f', 2.5)",
            "SELECT printf('%10.2f|', 3.14159), printf('%-10.2f|', 3.14159)",
            "SELECT printf('%e', 1234.5), printf('%E', 1234.5), printf('%.2e', 1234.5)",
            "SELECT printf('%g', 1234.5), printf('%g', 0.00001234), printf('%G', 1e20)",
            "SELECT printf('%s|%s', 'ab', 'cd'), printf('%10s|', 'ab'), printf('%-10s|', 'ab')",
            "SELECT printf('%.2s', 'abcdef')",
            // The `,` flag, which was parsed and thrown away before task-1932
            // (M8). Grouping applies to `d`, `i`, `u` and `f` and to nothing
            // else, and for the integer conversions it is applied after the
            // zero padding - so `%0,12d` is fifteen characters wide in a field
            // of twelve, and `%0,14.2f` is fourteen. Both orderings are here
            // because getting them the same way round is the mistake.
            "SELECT printf('%,d', 1234567), printf('%,d', -1234567), printf('%,d', 123)",
            "SELECT printf('%,12d|', 1234567), printf('%-,12d|', 1234567), printf('%+,d', 1234567)",
            "SELECT printf('%0,12d', 1234567), printf('%0,12d', -1234567), printf('%,.8d', 1234)",
            "SELECT printf('%,x', 255), printf('%,o', 8), printf('%,e', 1234567.0), printf('%,g', 1234567.0)",
            "SELECT printf('%,f', 1234567.5), printf('%,14.2f|', 1234.5), printf('%0,14.2f', 1234.5)",
            // The `!` flag, which counts the width and the precision in
            // characters rather than bytes. Every string here is multi-byte on
            // purpose: with ASCII the two spellings agree and the case says
            // nothing.
            "SELECT printf('%10s|', 'café'), printf('%!10s|', 'café'), printf('%!-10s|', 'café')",
            "SELECT printf('%5s|', '日本語'), printf('%!5s|', '日本語')",
            "SELECT printf('%.3s', 'éab'), printf('%!.3s', 'éab'), printf('%!8.2s|', '日本語')",
            "SELECT printf('%c%c', 65, 66)",
            "SELECT printf('%q', 'it''s'), printf('%Q', 'it''s'), printf('%Q', NULL), printf('%q', NULL)",
            "SELECT printf('%w', 'a\"b')",
            "SELECT printf('%%'), printf('a%%b'), printf('100%%')",
            "SELECT printf('%*d|', 5, 42), printf('%-*d|', 5, 42), printf('%.*f', 2, 3.14159)",
            "SELECT printf('%d-%d', 1), printf('%s!')",
            "SELECT printf('%s', 5), printf('%s', 5.5), printf('%d', '42'), printf('%d', 'abc')",
            "SELECT printf(NULL, 1), printf('no conversions')",
            "SELECT format('%d apples', 3)",
            "SELECT octet_length('abc'), octet_length(x'00ff'), octet_length(123), octet_length(NULL)",
            "SELECT length('abc'), length(x'00ff'), length(12345), length(NULL)",
            "SELECT hex('abc'), hex(x'00ff'), quote('it''s'), quote(NULL), quote(1.5)",
            "SELECT substr('abcdef', 2), substr('abcdef', 2, 3), substr('abcdef', -2), substr('abcdef', -2, 1)",
            "SELECT instr('abcdef', 'cd'), instr('abcdef', 'z'), instr(NULL, 'a')",
            "SELECT replace('abcabc', 'b', 'X'), replace('abc', '', 'X'), replace(NULL, 'a', 'b')",
            "SELECT trim('  ab  '), ltrim('xxabxx', 'x'), rtrim('xxabxx', 'x')",
            "SELECT upper('aBc'), lower('aBc'), unicode('A'), char(65, 66)",
            "SELECT iif(1, 'y', 'n'), iif(0, 'y', 'n'), iif(NULL, 'y', 'n')",
            "SELECT coalesce(NULL, NULL, 3), ifnull(NULL, 2), nullif(1, 1), nullif(1, 2)",
            "SELECT typeof(1), typeof(1.5), typeof('x'), typeof(x'00'), typeof(NULL)",
            "SELECT zeroblob(3), hex(zeroblob(3)), unhex('414243')",
            "SELECT concat('a', 1, NULL, 'b'), concat_ws('-', 'a', NULL, 'b')",
            "SELECT sign(-3), sign(0), sign(3.5), sign('x')",
            "SELECT likelihood(1, 0.5), likely(1), unlikely(1)",
            // `soundex` is not in the pinned build - it needs
            // `SQLITE_SOUNDEX` - so parity means refusing it rather than
            // implementing it.
            "SELECT soundex('Robert')",
        ],
    );
}

/// The statements SQLite refuses, refused the same way.
///
/// A negative test is worth as much as a positive one and is easier to get
/// wrong: an engine that accepts what the reference rejects has a *larger*
/// language, and every statement it accepts is one the reference cannot read.
/// The messages are not compared - they are prose - but the refusal is.
#[test]
fn refusals_match_the_oracle() {
    grade(
        "negative",
        &[
            // Access control is not SQLite's; these are not statements at all.
            "GRANT SELECT ON a TO someone",
            "REVOKE SELECT ON a FROM someone",
            "CREATE USER bob",
            // A view is not writable, however it is written to.
            "INSERT INTO blue VALUES (9, 'x', 1.0)",
            "INSERT INTO blue (id) VALUES (9)",
            "UPDATE blue SET name = 'x'",
            "UPDATE blue SET name = 'x' WHERE id = 1",
            "DELETE FROM blue",
            "DELETE FROM blue WHERE id = 1",
            "DROP TABLE blue",
            "DROP VIEW a",
            "CREATE INDEX blue_name ON blue (name)",
            // A compound's arms must agree on their width, and only the last
            // may carry an ORDER BY or a LIMIT.
            "SELECT id FROM a UNION SELECT id, name FROM a",
            "SELECT id FROM a ORDER BY id UNION SELECT id FROM a",
            "SELECT id FROM a LIMIT 1 UNION SELECT id FROM a",
            "SELECT id FROM a UNION SELECT id FROM a ORDER BY nosuchcolumn",
            // A scalar subquery is one column.
            "SELECT (SELECT id, name FROM a)",
            "SELECT * FROM a WHERE id IN (SELECT id, name FROM a)",
            // Names that do not resolve.
            "SELECT nosuchcolumn FROM a",
            "SELECT * FROM nosuchtable",
            "SELECT * FROM a JOIN b USING (nosuchcolumn)",
            "SELECT * FROM a AS x JOIN b AS y ON x.nosuch = y.team",
            "WITH t AS (SELECT 1) SELECT * FROM nosucht",
            "SELECT row_number() OVER nosuchwindow FROM a",
            "SELECT nosuchfunction(1)",
            "SELECT abs(1, 2)",
            "SELECT count(*, 1) FROM a",
            // Aggregates and windows where they do not belong.
            "SELECT * FROM a WHERE count(*) > 1",
            "SELECT sum(sum(id)) FROM a",
            "SELECT * FROM a WHERE row_number() OVER () = 1",
            // A frame offset is a constant.
            "SELECT sum(id) OVER (ORDER BY id ROWS id PRECEDING) FROM a",
            // `RIGHT` and `FULL` are accepted by the pinned build, so this is
            // the one row here that both engines must *not* refuse.
            "SELECT count(*) FROM a RIGHT JOIN b ON a.team = b.team",
            "SELECT count(*) FROM a FULL JOIN b ON a.team = b.team",
            // **A CTE that names itself where the recursion cannot read it
            // (task-1913).** SQLite answers `circular reference`. This engine
            // bound the same definition again and again until the process ran
            // out of stack, so these three did not fail the suite, they ended
            // it. The first has no compound arm to separate a seed from a
            // step; the second names itself from a scalar subquery, which the
            // recursion has no way to feed; the third is a cycle through two
            // names rather than one.
            "WITH q AS (SELECT * FROM q) SELECT * FROM q",
            "WITH q AS (SELECT 1 AS v WHERE (SELECT count(*) FROM q) = 0) SELECT * FROM q",
            "WITH x AS (SELECT * FROM y), y AS (SELECT * FROM x) SELECT * FROM x",
            // **`DISTINCT` takes exactly one argument (task-1913).** There is
            // nothing for a second one to be distinct by, so SQLite refuses
            // the call rather than choosing which duplicate's separator wins.
            // This answered it.
            "SELECT group_concat(DISTINCT name, '-') FROM a",
            "SELECT group_concat(DISTINCT name, id) FROM a",
        ],
    );
}

/// `group_concat` with a separator that is a value of each row.
///
/// **A form the engine refused outright (task-1913).** The separator had to be
/// a literal; anything else answered `group_concat with a computed separator`,
/// which is a documented SQLite form this engine did not have. The rule the
/// reference follows, measured rather than assumed: the separator written
/// before a row is *that row's own*, so the first row contributes none; a NULL
/// separator contributes nothing, which is why `group_concat(s, NULL)` is the
/// values run together rather than NULL; and a row whose value is NULL is
/// skipped entirely, separator and all.
///
/// The ordered arms matter on their own: the rows are sorted first and the
/// rule then applies to the sorted order, so the separator between the first
/// two of them comes from whichever row the sort put second.
#[test]
fn a_computed_group_concat_separator_matches_the_oracle() {
    grade(
        "group-concat-separator",
        &[
            "SELECT group_concat(name, id) FROM a",
            "SELECT group_concat(name, NULL) FROM a",
            "SELECT group_concat(name, 3) FROM a",
            "SELECT group_concat(name, 1.5) FROM a",
            "SELECT group_concat(name, team) FROM a",
            // Ordered, because `a.team` is indexed and the two engines read it
            // through different plans: `group_concat` over an unordered scan
            // has no defined order, and grading one would grade the plan.
            "SELECT group_concat(team, id ORDER BY id) FROM a",
            "SELECT group_concat(DISTINCT team ORDER BY team) FROM a",
            "SELECT group_concat(name, id ORDER BY name DESC) FROM a",
            "SELECT group_concat(name, id ORDER BY id DESC) FROM a",
            "SELECT group_concat(name, id) FROM a WHERE 0",
            "SELECT team, group_concat(name, id) FROM a GROUP BY team ORDER BY team",
            "SELECT group_concat(name, id) FILTER (WHERE id > 2) FROM a",
            // The literal form, beside it, so a change that routed everything
            // through the new path is caught by the old answers.
            "SELECT group_concat(name) FROM a",
            "SELECT group_concat(name, '-') FROM a",
        ],
    );
}

/// A call's own `ORDER BY` does not turn its `DISTINCT` off.
///
/// **Adding an `ORDER BY` to an aggregate used to drop its `DISTINCT`
/// entirely (task-1913).** The de-duplication lived on the path that folds one
/// value per row, and a call with an `ORDER BY` keeps whole rows instead
/// because they cannot be folded until they are in order - so it went straight
/// past. `group_concat(DISTINCT t ORDER BY t)` answered every duplicate. The
/// three forms here are the three that keep rows: an ordered `group_concat`,
/// an ordered JSON array, and an unordered one, which was always right and is
/// here so that a fix which de-duplicated the wrong thing is caught.
#[test]
fn a_distinct_aggregate_with_its_own_order_by_matches_the_oracle() {
    grade(
        "distinct-ordered",
        &[
            "SELECT group_concat(DISTINCT team ORDER BY team) FROM a",
            "SELECT json_group_array(DISTINCT team ORDER BY team) FROM a",
            "SELECT count(DISTINCT team) FROM a",
            "SELECT group_concat(DISTINCT k ORDER BY k) FROM c",
            "SELECT json_group_array(DISTINCT k ORDER BY k) FROM c",
            "SELECT group_concat(k ORDER BY k) FROM c",
        ],
    );
}

/// `DISTINCT` inside a function that is not an aggregate is ignored.
///
/// The engine used to refuse `abs(DISTINCT a)` with exit code 3, and its
/// capability note said SQLite refused it too. The pinned SQLite answers it as
/// `abs(a)`, one row per input row. Each statement here reaches a different
/// branch of the binder: a core scalar, a math function, a date function, a
/// JSON function, and a scalar under `GROUP BY`.
#[test]
fn distinct_in_a_scalar_function_matches_the_oracle() {
    grade(
        "distinct-scalar",
        &[
            "SELECT abs(DISTINCT score) FROM a ORDER BY id",
            "SELECT upper(DISTINCT name), substr(DISTINCT name, 1, 2) FROM a ORDER BY id",
            "SELECT round(DISTINCT score), sqrt(DISTINCT abs(score)) FROM a ORDER BY id",
            "SELECT date(DISTINCT '2020-01-02'), json(DISTINCT '[1]')",
            "SELECT coalesce(DISTINCT NULL, 1)",
            "SELECT team, length(DISTINCT team) FROM a GROUP BY team ORDER BY team",
        ],
    );
}

/// `IN` over a bare table name, which is SQLite's own form.
///
/// **A form the engine simply did not answer (task-1913).** The binder refused
/// it as `IN over a table name`; it is documented SQL in the reference, it is
/// not named as a gap in `docs/sql.md` or in the 416-case probe, and nothing
/// here graded it. It now reads as `x IN (SELECT * FROM t)`, which is what the
/// reference means by it - including the refusal when the table has more than
/// one column, and the unknown-rather-than-false answer when the table holds a
/// NULL.
#[test]
fn in_over_a_table_name_matches_the_oracle() {
    grade(
        "in-table",
        &[
            "SELECT name FROM a WHERE team IN teams ORDER BY id",
            "SELECT name FROM a WHERE team NOT IN teams ORDER BY id",
            "SELECT 'blue' IN teams, 'none' IN teams",
            "SELECT name FROM a WHERE score IN scores ORDER BY id",
            "SELECT 1 IN scores, 99.25 IN scores",
            // A table of more than one column is refused, the way a subquery
            // of more than one column is.
            "SELECT 1 IN c",
            // A table that does not exist is refused rather than read as a
            // value.
            "SELECT 1 IN nosuchtable",
        ],
    );
}
