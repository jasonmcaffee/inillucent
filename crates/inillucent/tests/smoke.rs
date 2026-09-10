//! The ten-second answer: does a real database file work at all?
//!
//! Invariant: **every assertion here is about a file on disk reached through
//! `inillucent::Database`**, the name an application outside this workspace
//! writes against. Nothing in this file touches an internal crate, and nothing
//! in it uses a harness. If this suite passes, the engine opens a file, keeps
//! what it was told, and hands it back after the process that wrote it has let
//! go - and if it fails, nothing else is worth running.
//!
//! ## Why the public facade had no tests until now
//!
//! Because it is a re-export. `inillucent::Database` is
//! `inillucent_engine::connect::Database` under another name, and the engine
//! crate is tested thoroughly through `inillucent-compat`, so a suite here
//! looked like it would be testing the same code twice.
//!
//! That reasoning has one hole, and it is the hole this file exists for: the
//! thing an application depends on is not the engine, it is **the name**. A
//! re-export that stops compiling, a type that stops being public, a method
//! that moves down a layer - none of those are engine defects and none of them
//! fail an engine test, and every one of them breaks every caller. This
//! facade was moved from one engine to another; nothing in the test suite
//! would have noticed if it had moved to neither.
//!
//! ## Why the file is real
//!
//! There is no in-memory mode here to fall into by accident, but the reopen is
//! the point rather than a formality: a write that is only in a page pool
//! satisfies every assertion a single-connection test can make. Each test below
//! that claims durability **drops the database and opens the path again**, so
//! what it reads has been through the write-ahead log and recovery.

use std::path::PathBuf;

use inillucent::{Database, OwnedDatum};

/// Returns a fresh, empty directory for one test's files.
///
/// A directory per test rather than a file per test: the engine writes a log
/// beside the database, so a test that removed only the `.rdb` would reopen
/// onto another test's log. Naming it after the process and the thread keeps
/// two test binaries running at once out of each other's way, which the
/// parallel runner makes the normal case rather than the exception.
///
/// @param tag - what to name the directory after
fn scratch(tag: &str) -> PathBuf {
    let directory = std::env::temp_dir().join(format!(
        "inillucent-smoke-{}-{tag}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = std::fs::remove_dir_all(&directory);
    std::fs::create_dir_all(&directory).expect("a scratch directory");
    directory
}

/// Returns the one integer a single-row, single-column answer holds.
///
/// @param rows - what the query returned
fn one_integer(rows: &[Vec<OwnedDatum>]) -> i64 {
    match rows {
        [row] => match row.as_slice() {
            [OwnedDatum::Int(value)] => *value,
            other => panic!("expected one integer, got {other:?}"),
        },
        other => panic!("expected one row, got {} of them: {other:?}", other.len()),
    }
}

/// Returns the one text a single-row, single-column answer holds.
///
/// @param rows - what the query returned
fn one_text(rows: &[Vec<OwnedDatum>]) -> String {
    match rows {
        [row] => match row.as_slice() {
            [OwnedDatum::Text(bytes)] => String::from_utf8_lossy(bytes).into_owned(),
            other => panic!("expected one text, got {other:?}"),
        },
        other => panic!("expected one row, got {} of them: {other:?}", other.len()),
    }
}

/// A file appears where it was asked for, and reports the path it was opened at.
#[test]
fn opening_a_path_creates_a_database_there() {
    let directory = scratch("create");
    let path = directory.join("app.rdb");
    assert!(!path.exists(), "the scratch directory starts empty");
    let database = Database::open(&path).expect("the database is created");
    assert_eq!(database.path(), path.as_path());
    drop(database);
    assert!(path.is_file(), "the file survives the handle that made it");
}

/// The shortest useful story: a schema, a row, and the row read back.
#[test]
fn a_row_written_is_a_row_read() {
    let directory = scratch("roundtrip");
    let database = Database::open(directory.join("app.rdb")).expect("the database opens");
    let connection = database.connect();
    connection
        .execute_batch("CREATE TABLE people (id INTEGER PRIMARY KEY, name TEXT NOT NULL)")
        .expect("the table is created");
    connection
        .execute("INSERT INTO people VALUES (1, 'Ada')")
        .expect("the row is written");
    let rows = connection
        .query("SELECT name FROM people WHERE id = 1")
        .expect("the row is read");
    assert_eq!(one_text(&rows), "Ada");
}

/// And the row is still there for a handle that was not the one that wrote it.
///
/// This is the assertion the whole file is built around. Everything before it
/// could pass against a page pool that never reached the disk.
#[test]
fn a_row_survives_the_handle_that_wrote_it() {
    let directory = scratch("reopen");
    let path = directory.join("app.rdb");
    {
        let database = Database::open(&path).expect("the database opens");
        let connection = database.connect();
        connection
            .execute_batch("CREATE TABLE notes (id INTEGER PRIMARY KEY, body TEXT)")
            .expect("the table is created");
        connection
            .execute("INSERT INTO notes VALUES (7, 'written before the reopen')")
            .expect("the row is written");
    }
    let database = Database::open(&path).expect("the database reopens");
    let connection = database.connect();
    let rows = connection
        .query("SELECT body FROM notes WHERE id = 7")
        .expect("the row is read back");
    assert_eq!(one_text(&rows), "written before the reopen");
}

/// A transaction that is rolled back leaves the file as it found it.
#[test]
fn a_rollback_leaves_nothing_behind() {
    let directory = scratch("rollback");
    let database = Database::open(directory.join("app.rdb")).expect("the database opens");
    let connection = database.connect();
    connection
        .execute_batch("CREATE TABLE t (id INTEGER PRIMARY KEY)")
        .expect("the table is created");
    connection
        .execute("INSERT INTO t VALUES (1)")
        .expect("one row");
    connection
        .execute_batch("BEGIN")
        .expect("the transaction opens");
    connection
        .execute("INSERT INTO t VALUES (2)")
        .expect("a second row");
    assert_eq!(
        one_integer(&connection.query("SELECT count(*) FROM t").expect("counted")),
        2,
        "the write is visible inside its own transaction"
    );
    connection
        .execute_batch("ROLLBACK")
        .expect("the transaction is abandoned");
    assert_eq!(
        one_integer(&connection.query("SELECT count(*) FROM t").expect("counted")),
        1,
        "and gone once it is abandoned"
    );
}

/// A prepared statement binds, steps and resets.
#[test]
fn a_prepared_statement_binds_and_steps() {
    let directory = scratch("prepare");
    let database = Database::open(directory.join("app.rdb")).expect("the database opens");
    let connection = database.connect();
    connection
        .execute_batch("CREATE TABLE k (id INTEGER PRIMARY KEY, label TEXT)")
        .expect("the table is created");
    let mut insert = connection
        .prepare("INSERT INTO k VALUES (?1, ?2)")
        .expect("the insert prepares");
    for (id, label) in [(1i64, "one"), (2, "two"), (3, "three")] {
        insert.reset();
        insert.bind_integer(1, id).expect("the id binds");
        insert.bind_text(2, label).expect("the label binds");
        while insert.step().expect("the insert runs") {}
    }
    drop(insert);
    let rows = connection
        .query("SELECT count(*) FROM k")
        .expect("the count is read");
    assert_eq!(one_integer(&rows), 3);
}

/// An index is built, and the planner is willing to say it used it.
#[test]
fn an_index_is_built_and_used() {
    let directory = scratch("index");
    let database = Database::open(directory.join("app.rdb")).expect("the database opens");
    let connection = database.connect();
    connection
        .execute_batch(
            "CREATE TABLE m (id INTEGER PRIMARY KEY, email TEXT NOT NULL);\
             CREATE UNIQUE INDEX m_email ON m (email);",
        )
        .expect("the schema is created");
    for number in 0..64i64 {
        connection
            .execute(&format!(
                "INSERT INTO m VALUES ({number}, 'a{number}@example.com')"
            ))
            .expect("a row is written");
    }
    let plan = connection
        .explain("SELECT id FROM m WHERE email = 'a17@example.com'")
        .expect("the plan is explained");
    let text = plan.join("\n");
    assert!(
        text.contains("m_email"),
        "the unique index should be the way in, but the plan was:\n{text}"
    );
    let rows = connection
        .query("SELECT id FROM m WHERE email = 'a17@example.com'")
        .expect("the row is found");
    assert_eq!(one_integer(&rows), 17);
}

/// A refused statement is an error the caller can read, not a panic and not a
/// silent nothing.
#[test]
fn a_bad_statement_is_a_readable_error() {
    let directory = scratch("error");
    let database = Database::open(directory.join("app.rdb")).expect("the database opens");
    let connection = database.connect();
    let failure = connection
        .execute("SELECT * FROM a_table_that_was_never_created")
        .expect_err("a missing table is an error");
    assert!(
        !failure.message().is_empty(),
        "an error with no message is one nobody can act on"
    );
    assert!(
        connection.query("SELECT 1").is_ok(),
        "and the connection is still usable afterwards"
    );
}

/// The engine's own integrity check passes on a file it built.
#[test]
fn a_fresh_database_passes_its_own_integrity_check() {
    let directory = scratch("integrity");
    let database = Database::open(directory.join("app.rdb")).expect("the database opens");
    let connection = database.connect();
    connection
        .execute_batch(
            "CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT);\
             CREATE INDEX t_v ON t (v);",
        )
        .expect("the schema is created");
    for number in 0..128i64 {
        connection
            .execute(&format!(
                "INSERT INTO t VALUES ({number}, 'value {number}')"
            ))
            .expect("a row is written");
    }
    database.check().expect("the file is sound");
    let rows = connection
        .query("PRAGMA integrity_check")
        .expect("the pragma answers");
    assert_eq!(one_text(&rows), "ok");
}
