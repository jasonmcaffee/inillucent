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
    let area = workspace_root().join("target/scratch/explain");
    let _ = std::fs::create_dir_all(&area);
    let path = area.join(format!("{name}.rdb"));
    inillucent_base::testing::remove_database(&path);
    let database = Database::open(&path).expect("a fresh database opens");
    {
        let connection = database.session();
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
/// **The one divergence this test used to record is closed.** It said SQLite
/// writes `(a=?)` for an index seek where this engine wrote `(?=?)`, because
/// the renderer was given the table's alias and not its columns - and named
/// threading the declaration into it as "a change worth making on purpose".
/// That thread now runs: `AccessPath::describe_over` takes the declaration,
/// the key column is named, and the line is the reference's line.
#[test]
fn a_query_plan_reads_the_way_sqlite_s_reads() {
    let database = fixture("shape");
    let connection = database.session();

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

    // The key column is named, which is the part a reader uses to tell one
    // index on a table from another.
    assert_eq!(
        details(
            &connection,
            "EXPLAIN QUERY PLAN SELECT a FROM t WHERE a = 'x'"
        ),
        vec!["SEARCH t USING COVERING INDEX ix (a=?)".to_string()],
    );
}

/// The four columns are the four SQLite answers with, by name.
#[test]
fn a_query_plan_has_the_columns_a_caller_expects() {
    let database = fixture("columns");
    let connection = database.session();
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
    let connection = database.session();

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
    let connection = database.session();

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

/// Plain `EXPLAIN` lists the steps the statement runs, in SQLite's columns.
///
/// **It used to be refused, and the refusal was true**: SQLite's `EXPLAIN`
/// lists the opcodes of a bytecode program and this engine compiles none. What
/// it answers now is the operator chain the statement actually runs, in the
/// eight columns SQLite answers with, framed by the `Init` and `Halt` that
/// begin and end an execution here as they do there. A reader comparing two
/// engines' listings is comparing two machines and will see that; a reader
/// asking what *this* statement does gets an answer.
#[test]
fn plain_explain_lists_the_chain_in_the_references_columns() {
    let database = fixture("plain");
    let connection = database.session();

    let rows = connection
        .query("EXPLAIN SELECT a FROM t")
        .expect("plain EXPLAIN is answered");
    assert_eq!(rows.len(), 3, "an Init, one step and a Halt: {rows:?}");
    let opcode = |at: usize| match rows.get(at).and_then(|row| row.get(1)) {
        Some(OwnedDatum::Text(bytes)) => String::from_utf8_lossy(bytes).into_owned(),
        other => panic!("row {at} has no opcode: {other:?}"),
    };
    assert_eq!(opcode(0), "Init");
    assert_eq!(opcode(1), "Scan");
    assert_eq!(opcode(2), "Halt");
    // The comment is the same sentence `EXPLAIN QUERY PLAN` prints, which is
    // what makes the two forms readable against each other.
    let comment = match rows.get(1).and_then(|row| row.get(7)) {
        Some(OwnedDatum::Text(bytes)) => String::from_utf8_lossy(bytes).into_owned(),
        other => panic!("the step has no comment: {other:?}"),
    };
    assert!(comment.starts_with("SCAN t"), "the comment was {comment:?}");
}
