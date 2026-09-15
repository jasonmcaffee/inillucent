//! The ten-second answer: does a real database file work at all?
//!
//! Invariant: **every assertion here is about a file on disk reached through
//! `inillucent::Database`**, the name an application outside this workspace
//! writes against. Nothing in this file touches an internal crate, and nothing
//! in it uses a harness. If this suite passes, the engine opens a file, keeps
//! what it was told, and hands it back after the process that wrote it has let
//! go - and if it fails, nothing else is worth running.
//!
//! ## Why the public name has a suite of its own
//!
//! Because it is a re-export. Since task-1961's A2, `inillucent::Database` is
//! `inillucent_driver::Database` under another name, and the driver is tested
//! through its own suites and the C ABI's conformance run, so a suite here
//! looked like it would be testing the same code twice.
//!
//! That reasoning has one hole, and it is the hole this file exists for: the
//! thing an application depends on is not the driver, it is **the name**. A
//! re-export that stops compiling, a type that stops being public, a method
//! that moves down a layer - none of those are engine defects and none of them
//! fail a driver test, and every one of them breaks every caller. This name has
//! now been moved between two engines and then re-rooted onto the driver;
//! nothing in the test suite would have noticed if it had landed on neither.
//!
//! Which is also why every path below names `inillucent::` and never
//! `inillucent_driver::`: a suite written against the crate underneath would
//! pass while the re-export was broken, which is the one failure it is here to
//! catch.
//!
//! ## Why the file is real
//!
//! There is no in-memory mode here to fall into by accident, but the reopen is
//! the point rather than a formality: a write that is only in a page pool
//! satisfies every assertion a single-connection test can make. Each test below
//! that claims durability **drops the database and opens the path again**, so
//! what it reads has been through the write-ahead log and recovery.

use std::path::PathBuf;

use inillucent::{Database, Rows, Value};

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
fn one_integer(rows: &Rows) -> i64 {
    assert_eq!(rows.rows.len(), 1, "expected one row: {rows:?}");
    match rows.value(0, 0) {
        Some(Value::Integer(value)) => *value,
        other => panic!("expected one integer, got {other:?}"),
    }
}

/// Returns the one text a single-row, single-column answer holds.
///
/// @param rows - what the query returned
fn one_text(rows: &Rows) -> String {
    assert_eq!(rows.rows.len(), 1, "expected one row: {rows:?}");
    match rows.value(0, 0) {
        Some(Value::Text(value)) => value.clone(),
        other => panic!("expected one text, got {other:?}"),
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
    let connection = database.session();
    connection
        .execute_batch("CREATE TABLE people (id INTEGER PRIMARY KEY, name TEXT NOT NULL)")
        .expect("the table is created");
    connection
        .execute(
            "INSERT INTO people VALUES (?1, ?2)",
            &[Value::Integer(1), Value::Text("Ada".to_string())],
        )
        .expect("the row is written");
    let rows = connection
        .query("SELECT name FROM people WHERE id = 1", &[], 1)
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
        let connection = database.session();
        connection
            .execute_batch("CREATE TABLE notes (id INTEGER PRIMARY KEY, body TEXT)")
            .expect("the table is created");
        connection
            .execute(
                "INSERT INTO notes VALUES (?1, ?2)",
                &[
                    Value::Integer(7),
                    Value::Text("before the reopen".to_string()),
                ],
            )
            .expect("the row is written");
    }
    let database = Database::open(&path).expect("the database reopens");
    let connection = database.session();
    let rows = connection
        .query("SELECT body FROM notes WHERE id = 7", &[], 1)
        .expect("the row is read back");
    assert_eq!(one_text(&rows), "before the reopen");
}

/// A transaction that is dropped without a commit leaves the file as it was.
///
/// **The `Drop` is the assertion, not a `rollback` call (task-1961, A4).** The
/// transaction below is never committed and never rolled back by name: it goes
/// out of scope, and the row it wrote has to be gone. That is the whole reason
/// [`inillucent::Transaction`] is a value rather than a pair of methods, and
/// nothing in this workspace asserted it through the public name before.
#[test]
fn a_dropped_transaction_leaves_nothing_behind() {
    let directory = scratch("rollback");
    let database = Database::open(directory.join("app.rdb")).expect("the database opens");
    let connection = database.session();
    connection
        .execute_batch("CREATE TABLE t (id INTEGER PRIMARY KEY)")
        .expect("the table is created");
    connection
        .execute("INSERT INTO t VALUES (1)", &[])
        .expect("one row");
    {
        let transaction = connection.begin().expect("the transaction opens");
        transaction
            .execute("INSERT INTO t VALUES (2)", &[])
            .expect("a second row");
        let counted = transaction
            .query("SELECT count(*) FROM t", &[], 1)
            .expect("counted");
        assert_eq!(
            one_integer(&counted),
            2,
            "the write is visible inside its own transaction"
        );
    }
    let counted = connection
        .query("SELECT count(*) FROM t", &[], 1)
        .expect("counted");
    assert_eq!(
        one_integer(&counted),
        1,
        "and gone once the transaction is dropped without a commit"
    );
}

/// A transaction that is committed keeps what it wrote, past a reopen.
#[test]
fn a_committed_transaction_keeps_what_it_wrote() {
    let directory = scratch("commit");
    let path = directory.join("app.rdb");
    {
        let database = Database::open(&path).expect("the database opens");
        let connection = database.session();
        connection
            .execute_batch("CREATE TABLE t (id INTEGER PRIMARY KEY)")
            .expect("the table is created");
        let transaction = connection.begin().expect("the transaction opens");
        transaction
            .execute("INSERT INTO t VALUES (9)", &[])
            .expect("a row");
        transaction.commit().expect("the transaction commits");
    }
    let database = Database::open(&path).expect("the database reopens");
    let counted = database
        .session()
        .query("SELECT count(*) FROM t", &[], 1)
        .expect("counted");
    assert_eq!(one_integer(&counted), 1);
}

/// A prepared statement binds and runs more than once.
#[test]
fn a_prepared_statement_binds_and_runs() {
    let directory = scratch("prepare");
    let database = Database::open(directory.join("app.rdb")).expect("the database opens");
    let connection = database.session();
    connection
        .execute_batch("CREATE TABLE k (id INTEGER PRIMARY KEY, label TEXT)")
        .expect("the table is created");
    let mut insert = connection
        .prepare("INSERT INTO k VALUES (?1, ?2)")
        .expect("the insert prepares");
    for (id, label) in [(1i64, "one"), (2, "two"), (3, "three")] {
        insert
            .query(&[Value::Integer(id), Value::Text(label.to_string())], 0)
            .expect("the insert runs");
    }
    drop(insert);
    let rows = connection
        .query("SELECT count(*) FROM k", &[], 1)
        .expect("the count is read");
    assert_eq!(one_integer(&rows), 3);
}

/// An index is built, and the planner is willing to say it used it.
#[test]
fn an_index_is_built_and_used() {
    let directory = scratch("index");
    let database = Database::open(directory.join("app.rdb")).expect("the database opens");
    let connection = database.session();
    connection
        .execute_batch(
            "CREATE TABLE m (id INTEGER PRIMARY KEY, email TEXT NOT NULL); \
             CREATE UNIQUE INDEX m_email ON m (email);",
        )
        .expect("the schema is created");
    for number in 0..64i64 {
        connection
            .execute(
                "INSERT INTO m VALUES (?1, ?2)",
                &[
                    Value::Integer(number),
                    Value::Text(format!("a{number}@example.com")),
                ],
            )
            .expect("a row is written");
    }
    let wanted = "a17@example.com";
    let plan = connection
        .explain("SELECT id FROM m WHERE email = ?1")
        .expect("the plan is explained");
    let text = plan.join("\n");
    assert!(
        text.contains("m_email"),
        "the unique index should be the way in, but the plan was: {text}"
    );
    let rows = connection
        .query(
            "SELECT id FROM m WHERE email = ?1",
            &[Value::Text(wanted.to_string())],
            1,
        )
        .expect("the row is found");
    assert_eq!(one_integer(&rows), 17);
}

/// A refused statement is an error the caller can read, not a panic and not a
/// silent nothing.
#[test]
fn a_bad_statement_is_a_readable_error() {
    let directory = scratch("error");
    let database = Database::open(directory.join("app.rdb")).expect("the database opens");
    let connection = database.session();
    let failure = connection
        .execute("SELECT * FROM a_table_that_was_never_created", &[])
        .expect_err("a missing table is an error");
    assert!(
        !failure.message.is_empty(),
        "an error with no message is one nobody can act on"
    );
    assert!(
        connection.query("SELECT 1", &[], 1).is_ok(),
        "and the connection is still usable afterwards"
    );
}

/// The engine's own integrity check passes on a file it built.
#[test]
fn a_fresh_database_passes_its_own_integrity_check() {
    let directory = scratch("integrity");
    let database = Database::open(directory.join("app.rdb")).expect("the database opens");
    let connection = database.session();
    connection
        .execute_batch(
            "CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT); \
             CREATE INDEX t_v ON t (v);",
        )
        .expect("the schema is created");
    for number in 0..128i64 {
        connection
            .execute(
                "INSERT INTO t VALUES (?1, ?2)",
                &[
                    Value::Integer(number),
                    Value::Text(format!("value {number}")),
                ],
            )
            .expect("a row is written");
    }
    database.integrity_check().expect("the file is sound");
    let rows = connection
        .query("PRAGMA integrity_check", &[], 1)
        .expect("the pragma answers");
    assert_eq!(one_text(&rows), "ok");
}

/// The capability table is reachable through the public name.
///
/// The one thing `drivers/README.md` tells an application to ask before it
/// composes a statement, so a re-export that dropped it would be a silent loss
/// of the answer rather than a compile failure in this workspace.
#[test]
fn the_capability_table_is_reachable() {
    assert!(
        !inillucent::CAPABILITIES.is_empty(),
        "an empty capability table answers every question with a shrug"
    );
    assert!(inillucent::capability("foreign_keys").is_some());
}
