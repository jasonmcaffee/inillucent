//! `ANALYZE`, the statistics it writes, and the plans they change.
//!
//! Invariant: statistics are a *hint*, and every test here checks both that
//! they change the plan and that the answer is the same either way. A planner
//! that returns different rows with and without `ANALYZE` is not an optimiser,
//! and the difference would be invisible to any test that only looked at the
//! plan.
//!
//! The `sqlite_stat1` inillucent writes is read by the pinned 3.53.4 binary and
//! the reverse holds, which matters more than it looks: statistics decide which
//! route each engine takes to the same rows, and an engine that could not read
//! the other's would diverge in performance rather than in answers - the kind
//! of difference that shows up as a mystery months later.

use std::path::PathBuf;

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

/// Returns a scratch path nothing else in this file uses.
fn scratch(tag: &str) -> PathBuf {
    let directory = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("planner");
    let _ = std::fs::create_dir_all(&directory);
    let path = directory.join(format!("{tag}.db"));
    let _ = std::fs::remove_file(&path);
    path
}

/// Runs a statement through inillucent, returning its rows or its failure.
fn run(
    connection: &inillucent_compat::facade::Connection,
    sql: &str,
) -> Result<Vec<String>, String> {
    let mut statement = match connection.prepare(sql) {
        Ok(statement) => statement,
        Err(reason) => return Err(reason.message().to_string()),
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
            Err(reason) => return Err(reason.message().to_string()),
        }
    }
    Ok(rows)
}

/// Runs a script, asserting every statement succeeds.
fn run_all(connection: &inillucent_compat::facade::Connection, script: &[&str]) {
    for sql in script {
        run(connection, sql).unwrap_or_else(|reason| panic!("{sql}: {reason}"));
    }
}

/// Returns the `EXPLAIN QUERY PLAN` detail lines for a statement.
fn plan(connection: &inillucent_compat::facade::Connection, sql: &str) -> Vec<String> {
    let explained = format!("EXPLAIN QUERY PLAN {sql}");
    run(connection, &explained)
        .unwrap_or_else(|reason| panic!("{explained}: {reason}"))
        .iter()
        .map(|row| {
            row.rsplit_once("text:")
                .map(|(_, detail)| detail.to_string())
                .unwrap_or_else(|| row.clone())
        })
        .collect()
}

/// Builds a skewed two-table schema: one row of `small` for every hundred of
/// `large`, so the two join orders differ by two orders of magnitude.
fn build(connection: &inillucent_compat::facade::Connection) {
    run_all(
        connection,
        &[
            "CREATE TABLE large (id INTEGER PRIMARY KEY, tag TEXT, filler TEXT)",
            "CREATE TABLE small (id INTEGER PRIMARY KEY, tag TEXT)",
            "CREATE INDEX large_tag ON large (tag)",
            "CREATE INDEX small_tag ON small (tag)",
        ],
    );
    run_all(connection, &["BEGIN"]);
    for row in 0..600 {
        let tag = format!("t{}", row % 60);
        let sql = format!("INSERT INTO large VALUES ({row}, '{tag}', 'xxxxxxxxxx')");
        run_all(connection, &[sql.as_str()]);
    }
    for row in 0..6 {
        let sql = format!("INSERT INTO small VALUES ({row}, 't{row}')");
        run_all(connection, &[sql.as_str()]);
    }
    run_all(connection, &["COMMIT"]);
}

/// `ANALYZE` writes a `sqlite_stat1` the pinned binary reads and agrees with.
///
/// **Through the interchange, not through `fopen`.** inillucent stores an
/// `RDB2` file, so handing its path to SQLite tests the file format rather than
/// the statistics - and the file format is deliberately its own, which is what
/// `inillucent-migrate` exists for. The claim this test is named after is about
/// the *content* of `sqlite_stat1`: that the strings inillucent's `ANALYZE`
/// writes mean to SQLite what they mean here. So the schema and the rows are
/// rebuilt in a real SQLite file, inillucent's own `sqlite_stat1` rows are
/// written into it verbatim, and SQLite is asked what it plans - which is the
/// question, asked of the bytes that answer it.
#[test]
fn analyze_writes_statistics_sqlite_reads() {
    let path = scratch("analyze");
    let database = Database::open(&path).expect("the database opens");
    let connection = database.session().expect("the connection opens");
    build(&connection);
    run_all(&connection, &["ANALYZE"]);

    // The row counts are the real ones, not an estimate.
    let stats = run(
        &connection,
        "SELECT tbl, idx, stat FROM sqlite_stat1 ORDER BY tbl, idx",
    )
    .expect("the statistics read back");
    assert!(
        stats
            .iter()
            .any(|row| row.contains("large_tag") && row.contains("600")),
        "{stats:?}"
    );
    assert!(
        stats
            .iter()
            .any(|row| row.contains("small_tag") && row.contains("text:6 1")),
        "{stats:?}"
    );
    // The same rows as SQL literals, ready to be handed to the other engine.
    let carried = run(
        &connection,
        "SELECT quote(tbl) || ',' || quote(idx) || ',' || quote(stat) FROM sqlite_stat1",
    )
    .expect("the statistics quote");
    assert!(!carried.is_empty(), "ANALYZE wrote no statistics");
    drop(connection);
    drop(database);

    // **A skip rather than a panic, so the row and the suite agree
    // (task-1969, 4.9).** This file's row declares `requires = ["oracle"]`,
    // which says the suite reports success when the oracle is absent. It did
    // not: it panicked, so a fresh clone read as a failure of the planner
    // rather than as a machine without the reference. The helper prints the
    // one marker and panics under `INILLUCENT_STRICT`, so a strict run still
    // names the case, and an ordinary run names the prerequisite.
    let Some(program) = oracle_path() else {
        inillucent_compat::differential::skipping(
            "the pinned SQLite oracle is not built; run tools/sqlite-reference.{ps1,sh}",
        );
        return;
    };
    let mirror = scratch("analyze-mirror");
    let mut driver = Driver::start("sqlite", &program).expect("the oracle starts");
    driver.send(&Op::Hello).expect("the oracle answers");
    driver
        .send(&Op::Open(mirror.display().to_string()))
        .expect("the oracle opens the file");
    let mut script = vec![
        "CREATE TABLE large (id INTEGER PRIMARY KEY, tag TEXT, filler TEXT)".to_string(),
        "CREATE TABLE small (id INTEGER PRIMARY KEY, tag TEXT)".to_string(),
        "CREATE INDEX large_tag ON large (tag)".to_string(),
        "CREATE INDEX small_tag ON small (tag)".to_string(),
        "BEGIN".to_string(),
    ];
    for row in 0..600 {
        script.push(format!(
            "INSERT INTO large VALUES ({row}, 't{}', 'xxxxxxxxxx')",
            row % 60
        ));
    }
    for row in 0..6 {
        script.push(format!("INSERT INTO small VALUES ({row}, 't{row}')"));
    }
    script.push("COMMIT".to_string());
    // `sqlite_stat1` is created by SQLite's own ANALYZE and then overwritten:
    // the numbers the planner is asked about have to be inillucent's, not the
    // ones SQLite would have computed for itself.
    script.push("ANALYZE".to_string());
    script.push("DELETE FROM sqlite_stat1".to_string());
    for row in &carried {
        let values = row
            .strip_prefix("text:")
            .expect("quote() returns text")
            .to_string();
        script.push(format!("INSERT INTO sqlite_stat1 VALUES({values})"));
    }
    // Reloading is what makes the planner read what was just written.
    script.push("ANALYZE sqlite_schema".to_string());
    for sql in &script {
        let observation = driver
            .send(&Op::Exec(sql.clone()))
            .expect("the oracle answers");
        assert!(observation.ok, "{sql}: {}", observation.message);
    }

    let integrity = driver
        .send(&Op::Query("PRAGMA integrity_check".to_string()))
        .expect("the oracle answers");
    assert_eq!(
        integrity
            .rows
            .first()
            .and_then(|row| row.first())
            .map(|value| matches!(value, TaggedValue::Text(text) if text == b"ok")),
        Some(true),
        "integrity_check: {:?}",
        integrity.rows
    );
    // SQLite reads the statistics and uses them, which it reports by planning
    // the join the way the row counts say to.
    let planned = driver
        .send(&Op::Query(
            "EXPLAIN QUERY PLAN SELECT count(*) FROM large, small WHERE large.tag = small.tag"
                .to_string(),
        ))
        .expect("the oracle answers");
    assert!(planned.ok, "{}", planned.message);
    let detail: Vec<String> = planned
        .rows
        .iter()
        .filter_map(|row| row.last())
        .filter_map(|value| match value {
            TaggedValue::Text(text) => Some(String::from_utf8_lossy(text).into_owned()),
            _ => None,
        })
        .collect();
    assert!(
        detail.first().is_some_and(|line| line.contains("small")),
        "sqlite did not scan the small table first: {detail:?}"
    );
}

/// Statistics change the join order, and do not change the answer.
#[test]
fn statistics_reorder_the_join() {
    let path = scratch("join-order");
    let database = Database::open(&path).expect("the database opens");
    let connection = database.session().expect("the connection opens");
    build(&connection);

    let query = "SELECT count(*) FROM large, small WHERE large.tag = small.tag";
    let before = run(&connection, query).expect("the query answers");
    let plan_before = plan(&connection, query);

    run_all(&connection, &["ANALYZE"]);
    let after = run(&connection, query).expect("the query answers");
    let plan_after = plan(&connection, query);

    // The answer is the same, which is the part that is not negotiable.
    assert_eq!(before, after, "the statistics changed the answer");
    assert_eq!(before, vec!["int:60".to_string()]);

    // And the plan visits the small table first, because six rows times a
    // search of six hundred beats six hundred times a search of six.
    assert!(
        plan_after
            .first()
            .is_some_and(|line| line.contains("small")),
        "before: {plan_before:?}\nafter: {plan_after:?}"
    );
    // Through `large_tag`, whether or not the index turns out to carry every
    // column the query wants: the join order is what this test is about, and
    // asserting on the exact wording made a *better* plan - a covering search
    // rather than a plain one - fail a test about something else.
    assert!(
        plan_after
            .iter()
            .any(|line| line.starts_with("SEARCH large USING") && line.contains("large_tag")),
        "{plan_after:?}"
    );
}

/// A written order that is already the cheapest is left alone, and `CROSS JOIN`
/// pins the order whatever the statistics say.
#[test]
fn cross_join_pins_the_order() {
    let path = scratch("cross-join");
    let database = Database::open(&path).expect("the database opens");
    let connection = database.session().expect("the connection opens");
    build(&connection);
    run_all(&connection, &["ANALYZE"]);

    let reorderable = plan(
        &connection,
        "SELECT count(*) FROM large, small WHERE large.tag = small.tag",
    );
    assert!(
        reorderable
            .first()
            .is_some_and(|line| line.contains("small")),
        "{reorderable:?}"
    );

    // `CROSS JOIN` is SQLite's documented instruction not to reorder, and the
    // point of honouring it is that a person who has measured their query can
    // overrule the planner.
    let pinned = plan(
        &connection,
        "SELECT count(*) FROM large CROSS JOIN small ON large.tag = small.tag",
    );
    assert!(
        pinned.first().is_some_and(|line| line.contains("large")),
        "{pinned:?}"
    );
    assert_eq!(
        run(
            &connection,
            "SELECT count(*) FROM large CROSS JOIN small ON large.tag = small.tag"
        ),
        Ok(vec!["int:60".to_string()])
    );
}

/// A costed choice between two usable indexes takes the more selective one.
#[test]
fn the_more_selective_index_wins() {
    let path = scratch("selectivity");
    let database = Database::open(&path).expect("the database opens");
    let connection = database.session().expect("the connection opens");
    run_all(
        &connection,
        &[
            "CREATE TABLE t (id INTEGER PRIMARY KEY, common TEXT, rare TEXT)",
            "CREATE INDEX t_common ON t (common)",
            "CREATE INDEX t_rare ON t (rare)",
            "BEGIN",
        ],
    );
    for row in 0..400 {
        // `common` takes two values and `rare` takes four hundred, so a query
        // constrained on both should search `rare`.
        let sql = format!("INSERT INTO t VALUES ({row}, 'c{}', 'r{row}')", row % 2);
        run_all(&connection, &[sql.as_str()]);
    }
    run_all(&connection, &["COMMIT", "ANALYZE"]);

    let chosen = plan(
        &connection,
        "SELECT id FROM t WHERE common = 'c0' AND rare = 'r10'",
    );
    assert!(
        chosen
            .iter()
            .any(|line| line.contains("USING INDEX t_rare")),
        "{chosen:?}"
    );
    assert_eq!(
        run(
            &connection,
            "SELECT id FROM t WHERE common = 'c0' AND rare = 'r10'"
        ),
        Ok(vec!["int:10".to_string()])
    );
}

/// A comparison on the column proves `IS NOT NULL`, so a partial index
/// declared that way is searched.
///
/// **`CREATE UNIQUE INDEX ... WHERE col IS NOT NULL` is how SQLite spells
/// "unique among the rows that have one", and this engine never used it
/// (task-1913).** The rule for choosing a partial index was that its predicate
/// appears unchanged as a conjunct of the `WHERE`, so `WHERE n = 1` did not
/// match `WHERE n IS NOT NULL` and the query scanned the table. The pinned
/// 3.53.4 answers the same statement with `SEARCH t USING COVERING INDEX t_n
/// (n=?)`.
///
/// A comparison is three valued: `n = 1` is *true* only when `n` is not NULL,
/// and the same holds for the other five comparison operators. `IS NULL` is
/// the case that must not match, and it is asserted here rather than left to
/// the reading: an index holding no NULL row cannot answer a query asking for
/// exactly the NULL rows, and choosing it would lose them silently.
///
/// Every arm checks the rows as well as the plan, because a plan test on its
/// own cannot tell a better route from a wrong one.
#[test]
fn a_comparison_proves_a_partial_index_predicate_of_is_not_null() {
    let path = scratch("partial-not-null");
    let database = Database::open(&path).expect("the database opens");
    let connection = database.session().expect("the connection opens");
    run_all(
        &connection,
        &[
            "CREATE TABLE t (id INTEGER PRIMARY KEY, n INTEGER, s TEXT)",
            "INSERT INTO t (id, n, s) VALUES (1, 10, 'a'), (2, 20, 'b'), (3, 30, 'c')",
            "INSERT INTO t (id, n, s) VALUES (4, NULL, 'd'), (5, NULL, 'e')",
            "CREATE UNIQUE INDEX t_n ON t (n) WHERE n IS NOT NULL",
        ],
    );

    // Each of the six comparison operators proves the predicate, so each makes
    // the index a candidate. The projection is `n`, which the index covers:
    // whether a *non-covering* range then beats a scan is a costing question
    // this engine answers differently from the reference for an ordinary index
    // too, so it is not what this case is about.
    for statement in [
        "SELECT n FROM t WHERE n = 10",
        "SELECT n FROM t WHERE n > 25",
        "SELECT n FROM t WHERE n >= 30",
        "SELECT n FROM t WHERE n < 15",
        "SELECT n FROM t WHERE n <= 10",
        "SELECT n FROM t WHERE n <> 10 AND n < 25",
    ] {
        let chosen = plan(&connection, statement);
        assert!(
            chosen.iter().any(|line| line.contains("INDEX t_n")),
            "{statement} did not use the partial index: {chosen:?}"
        );
    }

    // The lookup form, which is the one the defect was reported on: an
    // equality reaching a column the index does not carry.
    let looked_up = plan(&connection, "SELECT s FROM t WHERE n = 10");
    assert!(
        looked_up.iter().any(|line| line.contains("INDEX t_n")),
        "{looked_up:?}"
    );

    assert_eq!(
        run(&connection, "SELECT s FROM t WHERE n = 10"),
        Ok(vec!["text:a".to_string()])
    );
    assert_eq!(
        run(&connection, "SELECT n FROM t WHERE n > 25 ORDER BY n"),
        Ok(vec!["int:30".to_string()])
    );
    assert_eq!(
        run(&connection, "SELECT n FROM t WHERE n < 25 ORDER BY n"),
        Ok(vec!["int:10".to_string(), "int:20".to_string()])
    );

    // **The rows the index does not hold.** `IS NULL` asks for exactly the
    // rows the predicate excludes, so the index must not be chosen and the
    // answer must be both of them.
    let refused = plan(&connection, "SELECT s FROM t WHERE n IS NULL");
    assert!(
        !refused.iter().any(|line| line.contains("INDEX t_n")),
        "a query for the NULL rows used an index that holds none of them: {refused:?}"
    );
    assert_eq!(
        run(&connection, "SELECT s FROM t WHERE n IS NULL ORDER BY id"),
        Ok(vec!["text:d".to_string(), "text:e".to_string()])
    );

    // `IS` is not a comparison: it is true when both sides are NULL, so it
    // proves nothing about the operand being non-NULL.
    let by_is = plan(&connection, "SELECT s FROM t WHERE n IS NULL AND s = 'd'");
    assert!(
        !by_is.iter().any(|line| line.contains("INDEX t_n")),
        "{by_is:?}"
    );

    // A whole-table query still sees every row, including the ones the index
    // does not hold.
    assert_eq!(
        run(&connection, "SELECT count(*) FROM t"),
        Ok(vec!["int:5".to_string()])
    );
}

/// Statistics the pinned binary gathered are read by inillucent.
#[test]
fn statistics_sqlite_wrote_are_read_back() {
    let path = scratch("stats-from-sqlite");
    // **A skip rather than a panic, so the row and the suite agree
    // (task-1969, 4.9).** This file's row declares `requires = ["oracle"]`,
    // which says the suite reports success when the oracle is absent. It did
    // not: it panicked, so a fresh clone read as a failure of the planner
    // rather than as a machine without the reference. The helper prints the
    // one marker and panics under `INILLUCENT_STRICT`, so a strict run still
    // names the case, and an ordinary run names the prerequisite.
    let Some(program) = oracle_path() else {
        inillucent_compat::differential::skipping(
            "the pinned SQLite oracle is not built; run tools/sqlite-reference.{ps1,sh}",
        );
        return;
    };
    let mut driver = Driver::start("sqlite", &program).expect("the oracle starts");
    driver.send(&Op::Hello).expect("the oracle answers");
    driver
        .send(&Op::Open(path.display().to_string()))
        .expect("the oracle opens the file");
    let mut script = vec![
        "CREATE TABLE large (id INTEGER PRIMARY KEY, tag TEXT)".to_string(),
        "CREATE TABLE small (id INTEGER PRIMARY KEY, tag TEXT)".to_string(),
        "CREATE INDEX large_tag ON large (tag)".to_string(),
        "CREATE INDEX small_tag ON small (tag)".to_string(),
        "BEGIN".to_string(),
    ];
    for row in 0..600 {
        script.push(format!("INSERT INTO large VALUES ({row}, 't{}')", row % 60));
    }
    for row in 0..6 {
        script.push(format!("INSERT INTO small VALUES ({row}, 't{row}')"));
    }
    script.push("COMMIT".to_string());
    script.push("ANALYZE".to_string());
    for sql in &script {
        let observation = driver
            .send(&Op::Exec(sql.clone()))
            .expect("the oracle answers");
        assert!(observation.ok, "{sql}: {}", observation.message);
    }
    drop(driver);

    // The file SQLite wrote is a SQLite file, so it is *imported* rather than
    // opened: `Database::open` reads inillucent's own format and would refuse
    // this one at its meta page. The statistics come across with everything
    // else, which is the half of the round trip this test is about.
    let database = Database::import(&path).expect("the database imports");
    let connection = database.session().expect("the connection opens");
    let query = "SELECT count(*) FROM large, small WHERE large.tag = small.tag";
    let planned = plan(&connection, query);
    let carried = run(
        &connection,
        "SELECT tbl, idx, stat FROM sqlite_stat1 ORDER BY tbl",
    );
    assert!(
        planned.first().is_some_and(|line| line.contains("small")),
        "inillucent did not use SQLite's statistics: {planned:?}
  sqlite_stat1: {carried:?}"
    );
    assert_eq!(run(&connection, query), Ok(vec!["int:60".to_string()]));
}

/// How many rows the seek-union cases build.
///
/// Large enough that `ANALYZE` says the one-column seek is expensive, which is
/// what makes the planner choose the union at all - and the union is what these
/// cases are about.
const UNION_ROWS: usize = 20_000;

/// Fills a table whose `a` has ten values and whose `b` has 997.
///
/// @param connection - the database to write to
/// @param columns - the index to create over `t`
fn build_union_corpus(connection: &inillucent_compat::facade::Connection, columns: &str) {
    run_all(
        connection,
        &[
            "CREATE TABLE t (a INTEGER, b INTEGER, c TEXT)",
            &format!("CREATE INDEX i ON t ({columns})"),
            "BEGIN",
        ],
    );
    for row in 0..UNION_ROWS {
        let sql = format!(
            "INSERT INTO t VALUES ({}, {}, 'r{row}')",
            row % 10,
            row % 997
        );
        run_all(connection, &[sql.as_str()]);
    }
    run_all(connection, &["COMMIT", "ANALYZE"]);
}

/// An `IN` list over a non-unique index answers every row with each key.
///
/// **A wrong answer in the shipping engine, found while implementing M7
/// (task-1932).** `WHERE b IN (1, 2)` on a non-unique index over `(b)` answered
/// two rows where `WHERE b = 1` alone answers twenty-one. Every branch of an
/// equality union ran as a point probe, which finds the first entry with a key
/// and stops - correct for a rowid and for a unique index, and wrong for every
/// other one, where an equality is a run of entries.
///
/// **Why nothing caught it.** The union has to be *chosen* first, and the cost
/// model only prefers it over a plain seek once `ANALYZE` has run and its
/// statistics have reached a fresh connection. Every other suite's tables are
/// small, unanalysed, or both - so this case is deliberately twenty thousand
/// rows and deliberately re-opened.
#[test]
fn an_in_list_over_a_non_unique_index_answers_every_matching_row() {
    let path = scratch("in-union");
    {
        let database = Database::open(&path).expect("the database opens");
        let connection = database.session().expect("a connection opens");
        build_union_corpus(&connection, "b");
    }

    let database = Database::open(&path).expect("the database reopens");
    let connection = database.session().expect("a connection opens");
    let chosen = plan(&connection, "SELECT c FROM t WHERE b IN (1, 2)").join(" ");
    assert!(
        chosen.contains("SEARCH") && chosen.contains("INDEX i"),
        "the planner did not choose the index, so this case is not about the union: {chosen}"
    );

    let expected = (0..UNION_ROWS)
        .filter(|row| matches!(row % 997, 1 | 2))
        .count();
    assert!(
        expected >= 20,
        "the corpus holds {expected} matching rows, too few to tell a probe from a scan"
    );
    assert_eq!(
        run(&connection, "SELECT count(*) FROM t WHERE b IN (1, 2)"),
        Ok(vec![format!("int:{expected}")]),
        "an IN list over a non-unique index lost rows"
    );

    // The two equalities separately, so a failure says whether the union lost
    // rows or the corpus is not what this case thinks it is.
    let ones = (0..UNION_ROWS).filter(|row| row % 997 == 1).count();
    assert_eq!(
        run(&connection, "SELECT count(*) FROM t WHERE b = 1"),
        Ok(vec![format!("int:{ones}")])
    );
}

/// An `IN` list behind an equality prefix seeks on both columns.
///
/// **M7's second bullet.** `WHERE a = 5 AND b IN (1, 2, 3)` on an index over
/// `(a, b)` is three seeks to `(5, 1)`, `(5, 2)` and `(5, 3)`. The planner
/// looked at the leading column only, so the `IN` became a residual and the
/// query read every row with `a = 5` - two thousand rows here, to answer six.
#[test]
fn an_in_list_behind_an_equality_prefix_seeks_on_both_columns() {
    let path = scratch("in-prefix");
    {
        let database = Database::open(&path).expect("the database opens");
        let connection = database.session().expect("a connection opens");
        build_union_corpus(&connection, "a, b");
    }

    let database = Database::open(&path).expect("the database reopens");
    let connection = database.session().expect("a connection opens");
    let chosen = plan(
        &connection,
        "SELECT c FROM t WHERE a = 5 AND b IN (1, 2, 3)",
    )
    .join(" ");
    assert!(
        chosen.contains("a=? AND b=?"),
        "the plan pins only the leading column, so the IN list is still a residual: {chosen}"
    );

    let expected = (0..UNION_ROWS)
        .filter(|row| row % 10 == 5 && matches!(row % 997, 1..=3))
        .count();
    assert!(expected > 0, "the corpus holds no matching row");
    assert_eq!(
        run(
            &connection,
            "SELECT count(*) FROM t WHERE a = 5 AND b IN (1, 2, 3)"
        ),
        Ok(vec![format!("int:{expected}")]),
        "the two-column seek lost rows"
    );
}

/// An anchored `GLOB` on an indexed column seeks rather than scanning.
///
/// **M7's first bullet.** `k GLOB 'abc*'` selects exactly the keys from `abc`
/// up to but not including `abd`, and the planner matched `BoundExpr::Compare`
/// only - a pattern binds to `BoundExpr::Pattern`, so every prefix query on an
/// indexed column read the whole table.
///
/// `GLOB` on a `BINARY` column and `LIKE` on a `NOCASE` one are the two
/// pairings where the pattern's case sensitivity matches the index's, and they
/// are exactly the two the pinned 3.53.4 seeks for. The other two it scans, and
/// so does this: a case-sensitive range over a case-insensitive index excludes
/// rows the pattern matches.
#[test]
fn an_anchored_pattern_on_an_indexed_column_seeks() {
    let path = scratch("prefix-seek");
    let database = Database::open(&path).expect("the database opens");
    let connection = database.session().expect("a connection opens");
    run_all(
        &connection,
        &[
            "CREATE TABLE t (k TEXT, v INTEGER)",
            "CREATE INDEX i ON t (k)",
            "INSERT INTO t VALUES ('ab', 1), ('abc', 2), ('abcd', 3), ('abd', 4), ('ABC', 5)",
            "CREATE TABLE n (k TEXT COLLATE NOCASE)",
            "CREATE INDEX j ON n (k)",
        ],
    );

    let seeking = plan(&connection, "SELECT v FROM t WHERE k GLOB 'abc*'").join(" ");
    assert!(
        seeking.contains("SEARCH") && seeking.contains("INDEX i"),
        "an anchored GLOB on an indexed column did not seek: {seeking}"
    );
    assert_eq!(
        run(
            &connection,
            "SELECT k FROM t WHERE k GLOB 'abc*' ORDER BY k"
        ),
        Ok(vec!["text:abc".to_string(), "text:abcd".to_string()]),
        "the prefix seek lost the exact prefix or admitted the row past it"
    );

    let folded = plan(&connection, "SELECT k FROM n WHERE k LIKE 'abc%'").join(" ");
    assert!(
        folded.contains("SEARCH"),
        "a LIKE prefix on a NOCASE index did not seek: {folded}"
    );
    let mismatched = plan(&connection, "SELECT k FROM n WHERE k GLOB 'abc*'").join(" ");
    assert!(
        mismatched.contains("SCAN"),
        "a case-sensitive GLOB seeks over a case-insensitive index, which excludes rows it \
         matches: {mismatched}"
    );
    let unanchored = plan(&connection, "SELECT v FROM t WHERE k GLOB '*abc'").join(" ");
    assert!(
        unanchored.contains("SCAN"),
        "a pattern with a leading wildcard selects no range: {unanchored}"
    );
}

/// **`NOT INDEXED` takes every index away and leaves the rowid.**
///
/// The clause was parsed, the name it did not carry was validated, and then
/// nothing passed it to the planner: `BoundSource` did not hold it, so
/// `choose_path` chose as though it had not been written (task-2066 section
/// 4.4.14). Measured against the pinned 3.53.4 shell on this fixture:
/// `SELECT count(*) FROM h NOT INDEXED WHERE a = 3 AND b = 100` planned as
/// `SCAN h` there and as `SEARCH h USING INDEX h_b (b=?)` here.
///
/// **What made it more than a plan difference.**
/// `crates/inillucent-cli/src/diagnose.rs` reads every table
/// `SELECT * FROM "t" NOT INDEXED` to build its integrity digest, and says in
/// its own comment that the clause is what makes the digest a fact about the
/// rows. While the hint was dropped, a table whose index disagreed with it
/// could be digested through the index - so the check that exists to find that
/// disagreement was reading the wrong copy.
///
/// Both halves are asserted, because the fix is a subtraction and a subtraction
/// that goes too far is silent: SQLite leaves the INTEGER PRIMARY KEY usable
/// under `NOT INDEXED`, and a version of this that removed the rowid path too
/// would turn every keyed lookup in a digest into a full scan.
#[test]
fn not_indexed_removes_the_indexes_and_keeps_the_rowid() {
    let path = scratch("not-indexed");
    let database = Database::open(&path).expect("the database opens");
    let connection = database.session().expect("the connection opens");
    run_all(
        &connection,
        &[
            "CREATE TABLE h (id INTEGER PRIMARY KEY, a INTEGER, b INTEGER)",
            "CREATE INDEX h_a ON h (a)",
            "CREATE INDEX h_b ON h (b)",
        ],
    );
    run_all(&connection, &["BEGIN"]);
    for row in 1..=600 {
        let sql = format!("INSERT INTO h VALUES ({row}, {}, {row})", row % 7);
        run_all(&connection, &[sql.as_str()]);
    }
    run_all(&connection, &["COMMIT", "ANALYZE"]);

    // Without the clause, an index is chosen. This is the control: if the
    // planner would have scanned anyway, the arm below proves nothing.
    let chosen = plan(
        &connection,
        "SELECT count(*) FROM h WHERE a = 3 AND b = 100",
    );
    assert!(
        chosen.iter().any(|line| line.contains("USING INDEX")),
        "the control query did not use an index, so this fixture cannot show that \
         `NOT INDEXED` takes one away:\n{chosen:?}"
    );

    // With it, no index at all.
    let refused = plan(
        &connection,
        "SELECT count(*) FROM h NOT INDEXED WHERE a = 3 AND b = 100",
    );
    assert!(
        refused.iter().all(|line| !line.contains("USING INDEX")),
        "`NOT INDEXED` was written and an index was used anyway:\n{refused:?}"
    );
    assert!(
        refused.iter().any(|line| line.contains("SCAN h")),
        "`NOT INDEXED` did not fall back to a scan:\n{refused:?}"
    );

    // And the INTEGER PRIMARY KEY is still a seek, which is SQLite's rule.
    let keyed = plan(&connection, "SELECT * FROM h NOT INDEXED WHERE id = 5");
    assert!(
        keyed
            .iter()
            .any(|line| line.contains("INTEGER PRIMARY KEY")),
        "`NOT INDEXED` took the rowid away as well, which SQLite does not:\n{keyed:?}"
    );

    // The rows are the same either way, which is the thing a plan change must
    // never move.
    let with_hint = run(
        &connection,
        "SELECT count(*) FROM h NOT INDEXED WHERE a = 3 AND b = 100",
    )
    .expect("the hinted query runs");
    let without = run(
        &connection,
        "SELECT count(*) FROM h WHERE a = 3 AND b = 100",
    )
    .expect("the plain query runs");
    assert_eq!(
        with_hint, without,
        "`NOT INDEXED` changed the answer rather than the route to it"
    );
}

/// **`INDEXED BY` is checked for a name that exists and is then ignored.**
///
/// A known difference, recorded as §1.3 of the testing standard requires: a
/// difference a document knows about and no test asserts can be closed, or
/// widened, with nothing going red.
///
/// Measured against the pinned 3.53.4 shell on the same fixture as the case
/// above:
///
/// | statement | sqlite | inillucent |
/// |---|---|---|
/// | `... FROM h WHERE a = 3 AND b = 100` | `SEARCH h USING INDEX h_b (b=?)` | the same |
/// | `... FROM h INDEXED BY h_a WHERE a = 3 AND b = 100` | `SEARCH h USING INDEX h_a (a=?)` | `SEARCH h USING INDEX h_b (b=?)` |
///
/// **Why it is recorded rather than fixed here.** Forcing an index means
/// refusing the statement when the named one cannot answer it - SQLite answers
/// `no query solution` - and `choose_path` returns an `AccessPath` rather than
/// a `Result`, so the refusal has nowhere to go without changing the signature
/// of the path chooser and everything that calls it. `NOT INDEXED` needed
/// neither, because taking a candidate away always leaves the scan.
///
/// The answer is the same either way, so this is a performance difference and
/// not a wrong result. **When it is fixed, this test fails**, and the row in
/// `docs/feature-comparison.md` moves with it.
#[test]
fn indexed_by_names_an_index_and_does_not_yet_force_it() {
    let path = scratch("indexed-by");
    let database = Database::open(&path).expect("the database opens");
    let connection = database.session().expect("the connection opens");
    run_all(
        &connection,
        &[
            "CREATE TABLE h (id INTEGER PRIMARY KEY, a INTEGER, b INTEGER)",
            "CREATE INDEX h_a ON h (a)",
            "CREATE INDEX h_b ON h (b)",
        ],
    );
    run_all(&connection, &["BEGIN"]);
    for row in 1..=600 {
        let sql = format!("INSERT INTO h VALUES ({row}, {}, {row})", row % 7);
        run_all(&connection, &[sql.as_str()]);
    }
    run_all(&connection, &["COMMIT", "ANALYZE"]);

    // The name is checked, which is the half that works and is worth keeping:
    // a misspelled hint is a plan that quietly does something else.
    let misspelled = run(
        &connection,
        "SELECT count(*) FROM h INDEXED BY h_nowhere WHERE a = 3",
    )
    .expect_err("an index that does not exist is refused");
    assert!(
        misspelled.contains("h_nowhere"),
        "the refusal did not name the index that does not exist: {misspelled}"
    );

    let unhinted = plan(
        &connection,
        "SELECT count(*) FROM h WHERE a = 3 AND b = 100",
    );
    let hinted = plan(
        &connection,
        "SELECT count(*) FROM h INDEXED BY h_a WHERE a = 3 AND b = 100",
    );
    assert_eq!(
        hinted, unhinted,
        "`INDEXED BY h_a` changed the plan, so the hint now reaches the planner - \
         which is the fix this case is waiting for. Assert the forced index instead, \
         and move the `INDEXED BY` row in docs/feature-comparison.md."
    );

    // And the rows are right whichever index is walked, which is why this is a
    // performance difference rather than a wrong answer.
    // `b` is the row number and `a` is that number modulo seven, so `b = 101`
    // is the one row whose `a` is 3. The plan queries above use `b = 100`
    // because that is the pair the measurement in this comment was taken with,
    // and a plan does not depend on whether a row matches.
    let answered = run(
        &connection,
        "SELECT count(*) FROM h INDEXED BY h_a WHERE a = 3 AND b = 101",
    )
    .expect("the hinted query runs");
    assert_eq!(
        answered,
        vec!["int:1".to_string()],
        "the hinted query answered something other than the one matching row"
    );
}
