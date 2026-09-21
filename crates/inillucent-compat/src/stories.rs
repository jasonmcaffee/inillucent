//! What every application-shaped story does the same way.
//!
//! Invariant: **a story reads its answers back as text a person can compare, it
//! reopens before it claims anything persisted, and it asks the same question
//! two ways when an index is involved.** Those three are rules 1.1, 1.4 and 1.6
//! of `tests/inillucent-testing-tdd.md`, and writing them once means a new
//! story gets them by using this module rather than by remembering them.
//!
//! ## Why the helpers are here and not in each test file
//!
//! `crates/inillucent/tests/application.rs` had its own `ask`, `line`, `table`
//! and `reopen_and_check`, written before there was a second story file. There
//! are eight story files now, and eight copies of a rendering function is eight
//! chances for two stories to disagree about how a NULL prints - at which point
//! two failures that are the same failure read as different ones.

// **This module may panic, and the crate-level deny does not reach it.** The
// same case `cliproc.rs` and `differential.rs` make: a helper every `tests/*.rs`
// target uses lives in `src/`, which is compiled without `cfg(test)`, so the
// crate's test-only relaxation does not apply.
//
// What it panics on is the story having failed - a statement that will not run,
// a file that will not reopen - and the message carries the SQL and the arm. A
// helper that returned a `Result` here would put every story back to writing
// `.expect("...")` at each call site, which is where the messages that name
// neither the statement nor the configuration came from.
#![allow(clippy::panic)]

use std::path::Path;

use inillucent_engine::connect::{Connection, Database};
use inillucent_tree::datum::OwnedDatum;

use crate::matrix::Arm;

/// The engine's parameter set, re-exported.
///
/// A story in `crates/inillucent/tests/` binds parameters and that crate
/// does not depend on `inillucent-exec`, which is where `Params` lives. Adding
/// the edge would be a second row in the layering contract for a type that is
/// already reachable through this module, so it comes through here.
pub use inillucent_exec::physical::Params;

/// Renders one value the way every story prints it.
///
/// A blob prints as its length rather than its bytes, because a story that
/// compares a megabyte of blob against an expected string is a story nobody can
/// read the failure of; the length is what an assertion about a blob is about.
///
/// @param cell - the value
pub fn cell(cell: &OwnedDatum) -> String {
    match cell {
        OwnedDatum::Null => "NULL".to_string(),
        OwnedDatum::Int(value) => value.to_string(),
        OwnedDatum::Real(value) => format!("{value}"),
        OwnedDatum::Text(bytes) => String::from_utf8_lossy(bytes).into_owned(),
        OwnedDatum::Blob(bytes) => format!("x'{}'", bytes.len()),
    }
}

/// Renders one row as a comma separated line.
///
/// @param row - the row
pub fn line(row: &[OwnedDatum]) -> String {
    row.iter().map(cell).collect::<Vec<_>>().join(",")
}

/// Renders a whole answer, one row per line.
///
/// @param rows - what the query returned
pub fn table(rows: &[Vec<OwnedDatum>]) -> String {
    rows.iter()
        .map(|row| line(row))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Runs a query and renders it, failing with the SQL when it will not run.
///
/// @param connection - the connection to ask
/// @param sql - the query
pub fn ask(connection: &Connection<'_>, sql: &str) -> String {
    match connection.query(sql) {
        Ok(rows) => table(&rows),
        Err(why) => panic!("`{sql}` failed: {} ({:?})", why.message(), why.code()),
    }
}

/// Runs a statement, failing with the SQL when it will not run.
///
/// @param connection - the connection to write through
/// @param sql - the statement
pub fn run(connection: &Connection<'_>, sql: &str) {
    if let Err(why) = connection.execute_batch(sql) {
        panic!("`{sql}` failed: {} ({:?})", why.message(), why.code());
    }
}

/// Opens a story's database at the arm's geometry.
///
/// @param arm - the configuration this run is at
/// @param path - the database file
pub fn open(arm: &Arm, path: &Path) -> Database {
    match arm.open(path) {
        Ok(database) => database,
        Err(why) => panic!(
            "the database at {} does not open at the {} arm: {}",
            path.display(),
            arm.name,
            why.message()
        ),
    }
}

/// Reopens a database at the arm's geometry and checks it.
///
/// **Every story ends here, and that is rule 1.4.** A write that is only in a
/// page pool satisfies every assertion one connection can make: the
/// virtual-table rollback defect was invisible to every test that did not
/// reopen, because the *file* was correct throughout and only the live
/// connection was wrong.
///
/// @param arm - the configuration this run is at
/// @param path - the database file
pub fn reopen_and_check(arm: &Arm, path: &Path) -> Database {
    let database = open(arm, path);
    if let Err(why) = database.check() {
        panic!(
            "{} is not sound after a reopen at the {} arm: {}",
            path.display(),
            arm.name,
            why.message()
        );
    }
    let connection = database.session();
    let said = ask(&connection, "PRAGMA integrity_check");
    assert_eq!(
        said, "ok",
        "the engine's own check disagrees with `Database::check` at the {} arm",
        arm.name
    );
    // Ends the borrow of `database`, which is what lets it be returned.
    let _ = connection;
    database
}

/// Asks one question two ways and fails when the answers differ.
///
/// **Rule 1.6, written once.** An index the insert path maintains and the
/// delete path forgets answers a covering query wrongly while every other
/// question about the same row is right, so a write story asks each question in
/// a shape the planner answers from the table and in a shape it answers from an
/// index, and reports the difference as an index that has drifted.
///
/// @param connection - the connection to ask
/// @param through_the_table - the shape the planner answers from the table
/// @param through_the_index - the shape it answers from an index
/// @param about - what the question is, for the failure message
pub fn the_same_two_ways(
    connection: &Connection<'_>,
    through_the_table: &str,
    through_the_index: &str,
    about: &str,
) -> String {
    let from_table = ask(connection, through_the_table);
    let from_index = ask(connection, through_the_index);
    assert_eq!(
        from_table, from_index,
        "{about}: the table and the index disagree, which is an index that has drifted rather \
         than a difference between two queries\n  table: {through_the_table}\n  index: \
         {through_the_index}"
    );
    from_table
}

/// A deterministic body of text of about the wanted size.
///
/// **Stories need values that cross the extent threshold**, because a value
/// that fits on a leaf and a value that does not take different write paths,
/// and at 4,096 byte pages the threshold is crossed by text a real message
/// carries. Seeded off the row number so two runs write the same bytes and a
/// failure is reproducible.
///
/// @param seed - the row this text belongs to
/// @param bytes - roughly how long to make it
pub fn body(seed: usize, bytes: usize) -> String {
    const WORDS: [&str; 16] = [
        "pages", "trees", "buffer", "pool", "commit", "segment", "leaf", "index", "catalog",
        "journal", "vector", "chunk", "message", "digest", "extent", "recovery",
    ];
    let mut text = String::with_capacity(bytes + 16);
    let mut step = seed;
    while text.len() < bytes {
        step = step.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
        let pick = (step >> 33) % WORDS.len();
        text.push_str(WORDS.get(pick).copied().unwrap_or("pages"));
        text.push(' ');
    }
    text.truncate(bytes);
    text
}

/// A deterministic JSON vector of the wanted width.
///
/// The spelling `docs/vector-search.md` names first: a JSON array of numbers,
/// which is pgvector's own, and which the distance functions also read.
///
/// @param seed - the row this vector belongs to
/// @param width - how many components
pub fn vector(seed: usize, width: usize) -> String {
    let mut out = String::from("[");
    let mut step = seed.wrapping_add(1);
    for index in 0..width {
        step = step.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
        if index > 0 {
            out.push(',');
        }
        let whole = (step >> 40) % 1_000;
        out.push_str(&format!("0.{whole:03}"));
    }
    out.push(']');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The body is the length asked for, and the same length twice.
    #[test]
    fn a_body_is_the_size_it_was_asked_for() {
        assert_eq!(body(7, 40_000).len(), 40_000);
        assert_eq!(body(7, 1).len(), 1);
        assert_eq!(body(7, 4_096), body(7, 4_096));
    }

    /// Two seeds give two bodies, or the generator is not seeded at all.
    #[test]
    fn two_seeds_give_two_bodies() {
        assert_ne!(body(1, 512), body(2, 512));
    }

    /// A vector is a JSON array of the width asked for.
    #[test]
    fn a_vector_is_a_json_array_of_the_width_asked_for() {
        let made = vector(3, 8);
        assert!(made.starts_with('['), "{made}");
        assert!(made.ends_with(']'), "{made}");
        assert_eq!(made.matches(',').count(), 7, "{made}");
        assert_eq!(made, vector(3, 8));
        assert_ne!(made, vector(4, 8));
    }

    /// A blob renders as its length, because a story cannot read a megabyte.
    #[test]
    fn a_blob_renders_as_its_length() {
        assert_eq!(cell(&OwnedDatum::Blob(vec![0u8; 1_048_576])), "x'1048576'");
        assert_eq!(cell(&OwnedDatum::Null), "NULL");
        assert_eq!(cell(&OwnedDatum::Int(-1)), "-1");
    }
}
