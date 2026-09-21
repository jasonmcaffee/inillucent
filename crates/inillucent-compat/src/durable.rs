//! The fixtures two durability suites both build, in one place.
//!
//! Invariant: **a table these functions write is the same table whichever suite
//! wrote it, so a failure in one is comparable with a failure in the other.**
//!
//! `crates/inillucent/tests/durability.rs` and
//! `crates/inillucent/tests/durability_arms.rs` are one subject split across two
//! files, and the split is not a preference:
//! `crates/inillucent-compat/tests/scenarios.rs` refuses a file that holds both
//! a `scenario!` and a bare `#[test]`, because then "the file grades one story
//! six ways and another once and the run's output cannot tell them apart". The
//! cases that run at every arm therefore live apart from the cases that pick
//! their own geometry, and what they share is here rather than copied into both
//! - which is the case `stories.rs` makes at the top of its own file.

// **This module may panic**, for the reason `stories.rs` beside it may: it is a
// helper every suite here uses, it lives in `src/` so the crate's test-only
// relaxation does not reach it, and what it panics on is a statement that will
// not run, with the statement in the message.
#![allow(clippy::panic)]

use std::path::Path;

use inillucent_engine::connect::{Connection, Database};
use inillucent_tree::datum::OwnedDatum;

use crate::matrix::Arm;
use crate::stories::{open, run};

/// Returns the one integer a single-row, single-column answer holds.
///
/// @param rows - what the query returned
pub fn count(rows: &[Vec<OwnedDatum>]) -> i64 {
    match rows {
        [row] => match row.as_slice() {
            [OwnedDatum::Int(value)] => *value,
            other => panic!("expected one integer, got {other:?}"),
        },
        other => panic!("expected one row, got {}", other.len()),
    }
}

/// Runs a query and hands back its rows, failing with the SQL when it will not
/// run.
///
/// [`crate::stories::ask`] renders the answer as text, which is right for a
/// story comparing printed output and wrong here: these suites compare the rows
/// against a `Vec<Vec<OwnedDatum>>` the test wrote out, so that a table
/// overwritten by another table's tree is caught by its *contents* and not only
/// by its count.
///
/// @param connection - the connection to ask
/// @param sql - the query
pub fn rows(connection: &Connection<'_>, sql: &str) -> Vec<Vec<OwnedDatum>> {
    match connection.query(sql) {
        Ok(rows) => rows,
        Err(why) => panic!("`{sql}` failed: {}", why.message()),
    }
}

/// Checks every tree in a database, failing with what the check said.
///
/// @param database - the database to check
/// @param what - what the caller was doing, for the message
pub fn sound(database: &Database, what: &str) {
    if let Err(why) = database.check() {
        panic!(
            "{what}: the file is not sound: {} ({})",
            why.message(),
            why.detail().unwrap_or_default()
        );
    }
}

/// Creates `p` with three rows, through a handle that is then dropped.
///
/// @param arm - the configuration to build at
/// @param path - the database file
pub fn three_rows(arm: &Arm, path: &Path) {
    let database = open(arm, path);
    let connection = database.session();
    run(
        &connection,
        "CREATE TABLE p (id INTEGER PRIMARY KEY, n INTEGER); \
         INSERT INTO p VALUES (1, 10), (2, 20), (3, 30)",
    );
}

/// Returns the three rows [`three_rows`] writes, as a query returns them.
pub fn the_three_rows() -> Vec<Vec<OwnedDatum>> {
    vec![
        vec![OwnedDatum::Int(1), OwnedDatum::Int(10)],
        vec![OwnedDatum::Int(2), OwnedDatum::Int(20)],
        vec![OwnedDatum::Int(3), OwnedDatum::Int(30)],
    ]
}

/// Returns the rows of `p` through a freshly opened handle, and checks the file.
///
/// Separate from a plain row count because these suites care *which* rows came
/// back, not how many: a table that was overwritten by a different table's tree
/// can have the right count and the wrong contents.
///
/// @param arm - the configuration the file was built at
/// @param path - the database file
pub fn values_after_reopen(arm: &Arm, path: &Path) -> Vec<Vec<OwnedDatum>> {
    let database = sound_after_reopen(arm, path);
    let connection = database.session();
    rows(&connection, "SELECT id, n FROM p ORDER BY id")
}

/// Reopens at the arm and checks the file, without reading any table.
///
/// Hands the handle back, because a caller that reopens usually has something
/// else to ask of the file and opening it twice would be a second recovery.
///
/// @param arm - the configuration the file was built at
/// @param path - the database file
pub fn sound_after_reopen(arm: &Arm, path: &Path) -> Database {
    let database = open(arm, path);
    sound(&database, "after the reopen");
    database
}

/// How many rows [`migrate_and_index`] writes.
///
/// Enough that the table's pages outgrow a 64 frame pool at 4,096 bytes - a
/// quarter of a mebibyte - so the read that function ends with really does
/// evict.
pub const MIGRATED_ROWS: i64 = 800;

/// Writes a populated table, adds a column to it, and indexes the column.
///
/// **The sequence task-2055 is about, and no part of it is there for decoration.**
/// `ALTER TABLE ADD COLUMN` on a populated table rebuilds the table's tree and,
/// since task-2043, frees the old one at the commit - so the `CREATE INDEX`
/// after it is handed that table's own old leaves, because
/// `inillucent_pool::FreeMap::free` rewinds the allocator hint to the lowest
/// page it was given back. The index is written straight into the data file and
/// no log record describes its contents, so it is the one thing in the file
/// nothing can rebuild.
///
/// The read at the end is what empties the pool of dirty frames. Without it the
/// close folds the log in and every one of the three routes that put those
/// pages' previous life back is hidden, which is the difference between the arm
/// that failed and the arm that did not.
///
/// One body in five is large enough to leave the leaf, the proportion
/// [`crate::nikaya::seed`] uses, so the table has extent pages as well as leaves
/// and the build is handed both kinds back.
///
/// @param connection - the connection to write through
pub fn migrate_and_index(connection: &Connection<'_>) {
    run(
        connection,
        "CREATE TABLE doc (id INTEGER PRIMARY KEY, body TEXT)",
    );
    // Written a batch at a time, for the reason `crate::nikaya::seed` gives: a
    // statement per row is a compile per row, and the rows are the same rows.
    let mut batch = String::from("BEGIN;");
    for id in 0..MIGRATED_ROWS {
        let body = "x".repeat(if id % 5 == 0 { 4_000 } else { 300 });
        batch.push_str(&format!(
            "INSERT INTO doc (id, body) VALUES ({id}, '{body}');"
        ));
    }
    batch.push_str("COMMIT");
    run(connection, &batch);
    run(
        connection,
        "ALTER TABLE doc ADD COLUMN seen INTEGER DEFAULT 7",
    );
    run(connection, "CREATE INDEX doc_seen ON doc (seen)");
    let read = rows(connection, "SELECT id, body, seen FROM doc ORDER BY id");
    assert_eq!(
        read.len() as i64,
        MIGRATED_ROWS,
        "the migrated table does not read back whole"
    );
}
