//! An already-prepared statement follows a schema change.
//!
//! Invariant: **a plan is compiled against a snapshot of the catalog, so a
//! statement held across a schema change has to be compiled again before it
//! runs.** A plan names trees by identifier; drop the table those identifiers
//! belong to and re-create it, and the held plan is describing pages that
//! belong to something else now.
//!
//! `Connection::step` has checked the schema generation since it existed.
//! `ImportedDatabase::execute_statement` - the entry point the gates, the
//! profiles and every harness in this crate run their statements through - did
//! not, so the two halves of the same public API disagreed about whether an
//! already-prepared statement follows a schema change (task-1932, M11).
//! SQLite's own `sqlite3_step` reprepares, and so does this now.

use std::path::PathBuf;

use inillucent_compat::newengine::ImportedDatabase;
use inillucent_compat::workspace_root;
use inillucent_exec::physical::Params;
use inillucent_tree::datum::OwnedDatum;

/// The page size these databases are built at.
const PAGE_SIZE: usize = 4_096;

/// How many frames the pool holds.
const FRAMES: usize = 64;

/// Returns a scratch file for one scenario.
///
/// @param name - what to name it after
fn scratch(name: &str) -> PathBuf {
    let directory = workspace_root().join("_agent_output/prepared-schema");
    let _ = std::fs::create_dir_all(&directory);
    let path = directory.join(format!("{name}.rdb"));
    let _ = std::fs::remove_file(&path);
    path
}

/// Runs one statement, reporting the failure rather than ending the test.
///
/// @param engine - the database
/// @param sql - the statement
fn run(engine: &mut ImportedDatabase, sql: &str) {
    engine
        .execute_any(sql, &Params::new())
        .unwrap_or_else(|failure| {
            // The detail as well as the message: "bad parameter or other API
            // misuse" names nothing, and a misuse raised deep in the pool names
            // the page and the reason in its detail.
            let said = failure.detail().unwrap_or_default().to_string();
            panic!("{sql} did not run: {failure}: {said}")
        });
}

/// Returns the first value of the first row an outcome holds, as an integer.
///
/// @param rows - the outcome's rows
fn first_integer(rows: &[Vec<OwnedDatum>]) -> Option<i64> {
    match rows.first().and_then(|row| row.first()) {
        Some(OwnedDatum::Int(number)) => Some(*number),
        _ => None,
    }
}

/// A prepared statement answers the table that is there now, not the one that
/// was there when it was compiled.
#[test]
fn a_prepared_statement_follows_a_table_that_was_dropped_and_rebuilt() {
    let path = scratch("dropped");
    let mut engine =
        ImportedDatabase::create(path.clone(), PAGE_SIZE, FRAMES).expect("the database is created");
    run(&mut engine, "CREATE TABLE t(a INTEGER PRIMARY KEY, b TEXT)");
    run(&mut engine, "INSERT INTO t VALUES(1, 'one'), (2, 'two')");

    let statement = engine
        .prepare_statement("SELECT count(*) FROM t")
        .expect("the statement compiles");
    let before = engine
        .execute_statement(&statement, &Params::new())
        .expect("it runs");
    assert_eq!(first_integer(&before.rows), Some(2));

    // The identifiers the held plan names stop describing anything after this.
    run(&mut engine, "DROP TABLE t");
    run(&mut engine, "CREATE TABLE t(a INTEGER PRIMARY KEY, b TEXT)");
    run(&mut engine, "INSERT INTO t VALUES(7, 'seven')");

    let after = engine
        .execute_statement(&statement, &Params::new())
        .expect("the held statement runs after the schema changed");
    assert_eq!(
        first_integer(&after.rows),
        Some(1),
        "the held statement answered the table that used to be there"
    );
}

/// A prepared statement sees a column added after it was compiled.
///
/// `ALTER TABLE ... ADD COLUMN` rewrites the catalog row and moves the
/// generation without moving the tree, so this is the half of the same problem
/// that answers with the wrong *shape* rather than from the wrong tree.
#[test]
fn a_prepared_statement_sees_a_column_added_after_it_was_compiled() {
    let path = scratch("altered");
    let mut engine =
        ImportedDatabase::create(path.clone(), PAGE_SIZE, FRAMES).expect("the database is created");
    run(&mut engine, "CREATE TABLE t(a INTEGER PRIMARY KEY, b TEXT)");
    run(&mut engine, "INSERT INTO t VALUES(1, 'one')");

    let counting = engine
        .prepare_statement("SELECT count(*) FROM t")
        .expect("the statement compiles");
    assert_eq!(
        first_integer(
            &engine
                .execute_statement(&counting, &Params::new())
                .expect("it runs")
                .rows
        ),
        Some(1)
    );

    run(&mut engine, "ALTER TABLE t ADD COLUMN c INTEGER DEFAULT 5");
    run(&mut engine, "INSERT INTO t(a, b) VALUES(2, 'two')");

    let after = engine
        .execute_statement(&counting, &Params::new())
        .expect("the held statement runs after the column was added");
    assert_eq!(
        first_integer(&after.rows),
        Some(2),
        "the held statement did not see the row added after the ALTER"
    );

    // And a statement that reads the new column, prepared before it existed,
    // is the case a stale plan cannot answer at all.
    let reading = engine
        .prepare_statement("SELECT sum(c) FROM t")
        .expect("the statement compiles against the altered table");
    assert_eq!(
        first_integer(
            &engine
                .execute_statement(&reading, &Params::new())
                .expect("it runs")
                .rows
        ),
        Some(10)
    );
}

/// A prepared statement's parameter count is the one a parse of its text reads.
///
/// **`Connection::prepare` parsed the same text twice** (task-2066 §4.3.3):
/// once through `parameter_count`, for the number a bind index is checked
/// against, and once through `prepare_statement`, for the plan. On a plan
/// cache hit the second of those did not parse at all, so the only parse left
/// was for a number the compilation had already read.
///
/// The count is carried on the compiled statement now, and this is what says
/// it is the same number: `parameter_count` still parses the text and answers
/// from the parse, and the two have to agree for every shape - including the
/// ones where they could plausibly differ, a statement with no parameters, one
/// with a gap in its numbering, and one where the same index appears twice.
///
/// **It also has to survive a reprepare.** A statement held across a schema
/// change is compiled again on its next step, and `?1` is `?1` whatever the
/// catalog says - so the count must not move.
#[test]
fn a_prepared_statement_counts_the_parameters_its_text_declares() {
    let path = scratch("parameter-count");
    let database =
        ImportedDatabase::create(path.clone(), PAGE_SIZE, FRAMES).expect("the database is created");

    let shapes = [
        ("SELECT 1", 0u32),
        ("SELECT ?1", 1),
        ("SELECT ?1, ?2", 2),
        // A gap: SQLite counts to the highest index, not the number written.
        ("SELECT ?3", 3),
        // The same index twice is one parameter.
        ("SELECT ?1, ?1", 1),
    ];
    for (sql, expected) in shapes {
        let parsed = database
            .parameter_count(sql)
            .unwrap_or_else(|why| panic!("{sql}: {}", why.message()));
        assert_eq!(
            parsed, expected,
            "`{sql}`: a parse of the text counts {parsed} parameters, and the shape declares {expected}"
        );
        let prepared = database
            .prepare_statement(sql)
            .unwrap_or_else(|why| panic!("{sql}: {}", why.message()));
        assert_eq!(
            prepared.parameter_count(),
            parsed,
            "`{sql}`: the prepared statement carries {} and a parse of the same text reads {parsed}",
            prepared.parameter_count()
        );
    }
}
