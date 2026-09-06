//! `EXPLAIN QUERY PLAN` through the new engine, and why plain `EXPLAIN` is not.
//!
//! Invariant: **the two forms of `EXPLAIN` ask different questions, and this
//! engine can answer one of them.** SQLite's plain `EXPLAIN` lists the opcodes
//! of the bytecode program it compiled; this engine compiles no bytecode, it
//! builds an operator chain, so there is no opcode listing to print. Printing
//! the operator chain under that name would be answering a different question
//! with the same word, so plain `EXPLAIN` is refused and says why.
//!
//! `EXPLAIN QUERY PLAN` asks what the plan *is*, and that this engine has. The
//! renderer is not new - `PhysicalPlan::describe` has been printing beside
//! SQLite's in the benchmark harness since Phase 1, so a reader could see
//! whether the two engines chose the same structure. What was missing was the
//! statement reaching it: the binder refused `EXPLAIN` outright, because the
//! old engine handled it a level up where a program was available to render.
//!
//! ## The expected strings were checked against SQLite 3.53.4
//!
//! Every `detail` below was produced by running the same schema and the same
//! statement through the pinned shell. They are written out rather than
//! compared at run time so the test still says something on a machine with no
//! oracle built - the same contract `new_engine_slt.rs` holds.

use inillucent_compat::workspace_root;
use inillucent_engine::connect::{Connection, Database};
use inillucent_tree::datum::OwnedDatum;

/// Returns a database holding one indexed table.
///
/// @param name - the test's name, which names its file
fn fixture(name: &str) -> Database {
    let area = workspace_root().join("target/scratch/task-1834/explain");
    let _ = std::fs::create_dir_all(&area);
    let path = area.join(format!("{name}.rdb"));
    let _ = std::fs::remove_file(&path);
    let database = Database::open(&path).expect("a fresh database opens");
    {
        let connection = database.connect();
        connection
            .execute_batch(
                "CREATE TABLE t (id INTEGER PRIMARY KEY, a TEXT, b INTEGER); \
                 CREATE INDEX ix ON t(a); \
                 INSERT INTO t(id, a, b) VALUES (1, 'x', 10)",
            )
            .expect("the fixture loads");
    }
    database
}

/// Returns the `detail` column of every row a query plan produced.
///
/// @param connection - the connection to ask
/// @param sql - the `EXPLAIN QUERY PLAN` statement
fn details(connection: &Connection<'_>, sql: &str) -> Vec<String> {
    connection
        .query(sql)
        .unwrap_or_else(|error| panic!("{sql}: {error:?}"))
        .iter()
        .map(|row| match row.get(3) {
            Some(OwnedDatum::Text(bytes)) => String::from_utf8_lossy(bytes).into_owned(),
            other => panic!("{sql} produced {other:?} in the detail column"),
        })
        .collect()
}

/// The plan lines are the ones SQLite prints for the same schema.
///
/// One divergence is deliberate and recorded here rather than hidden: SQLite
/// writes `(a=?)` for an index seek and this engine writes `(?=?)`, because
/// `AccessPath::describe` is given the table's alias and not its column names.
/// Naming the column would mean threading the table's metadata into a renderer
/// several callers share, which is a change worth making on purpose rather than
/// as a side effect of this test. SQLite documents its own `EXPLAIN QUERY PLAN`
/// text as unstable between releases, so this was never the comparable part.
#[test]
fn a_query_plan_reads_the_way_sqlite_s_reads() {
    let database = fixture("shape");
    let connection = database.connect();

    assert_eq!(
        details(&connection, "EXPLAIN QUERY PLAN SELECT a FROM t"),
        vec!["SCAN t USING COVERING INDEX ix".to_string()],
        "a query answerable from the index should not say it scans the table"
    );
    assert_eq!(
        details(
            &connection,
            "EXPLAIN QUERY PLAN SELECT a FROM t WHERE id = 1"
        ),
        vec!["SEARCH t USING INTEGER PRIMARY KEY (rowid=?)".to_string()]
    );
    assert_eq!(
        details(
            &connection,
            "EXPLAIN QUERY PLAN SELECT count(*) FROM t GROUP BY b"
        ),
        vec![
            "SCAN t".to_string(),
            "USE TEMP B-TREE FOR GROUP BY".to_string(),
        ],
        "the grouping pass is named only when there is one"
    );

    // The recorded divergence, asserted so that closing it fails this test and
    // brings someone here to read the paragraph above.
    assert_eq!(
        details(
            &connection,
            "EXPLAIN QUERY PLAN SELECT a FROM t WHERE a = 'x'"
        ),
        vec!["SEARCH t USING COVERING INDEX ix (?=?)".to_string()],
        "SQLite writes (a=?) here; see this test's documentation"
    );
}

/// The four columns are the four SQLite answers with, by name.
#[test]
fn a_query_plan_has_the_columns_a_caller_expects() {
    let database = fixture("columns");
    let connection = database.connect();
    let rows = connection
        .query("EXPLAIN QUERY PLAN SELECT a FROM t")
        .expect("the plan is produced");
    let first = rows.first().expect("a plan has at least one line");
    assert_eq!(first.len(), 4, "id, parent, notused, detail");
    assert!(
        matches!(first.first(), Some(OwnedDatum::Int(0))),
        "the first line is numbered 0"
    );
    assert!(
        matches!(first.get(3), Some(OwnedDatum::Text(_))),
        "the detail is text"
    );
}

/// A write's plan is the search that finds the rows it changes.
///
/// "Did my `DELETE` use the index" is the same question as "did the search use
/// it", and it is the question a reader asks. Answering "a delete" would be
/// answering that it is a delete, which the reader wrote.
#[test]
fn a_write_is_explained_by_the_query_that_finds_its_rows() {
    let database = fixture("write");
    let connection = database.connect();

    assert_eq!(
        details(&connection, "EXPLAIN QUERY PLAN DELETE FROM t WHERE id = 1"),
        vec!["SEARCH t USING INTEGER PRIMARY KEY (rowid=?)".to_string()],
        "SQLite prints exactly this line for the same statement"
    );
    assert_eq!(
        details(
            &connection,
            "EXPLAIN QUERY PLAN UPDATE t SET b = 1 WHERE id = 1"
        ),
        vec!["SEARCH t USING INTEGER PRIMARY KEY (rowid=?)".to_string()]
    );
}

/// The description follows the schema, because it is cached like the query.
///
/// A plan description held against the statement text has to be thrown away
/// when the schema it described changes. This drops the index the first plan
/// used and asks again.
#[test]
fn a_query_plan_is_re_rendered_after_the_schema_changes() {
    let database = fixture("stale");
    let connection = database.connect();

    let query = "EXPLAIN QUERY PLAN SELECT a FROM t";
    assert_eq!(
        details(&connection, query),
        vec!["SCAN t USING COVERING INDEX ix".to_string()]
    );

    connection
        .execute_batch("DROP INDEX ix")
        .expect("the index is dropped");

    assert_eq!(
        details(&connection, query),
        vec!["SCAN t".to_string()],
        "the plan description outlived the index it named"
    );
}

/// Plain `EXPLAIN` is refused, and the refusal says why rather than "not yet".
#[test]
fn plain_explain_is_refused_because_there_is_no_bytecode() {
    let database = fixture("plain");
    let connection = database.connect();

    let error = connection
        .query("EXPLAIN SELECT a FROM t")
        .expect_err("plain EXPLAIN is refused");
    let text = format!("{error:?}");
    assert!(
        text.contains("compiles no bytecode"),
        "the refusal did not say why it cannot be answered: {text}"
    );
    assert!(
        text.contains("EXPLAIN QUERY PLAN"),
        "the refusal did not name the form that is answered: {text}"
    );
}
