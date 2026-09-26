//! One logical connection over a connection per call.
//!
//! Invariant: **a session outlives the `Connection` value that opened it**, and
//! [`Database::connect_as`] is how a caller stays in one.
//!
//! `Connection<'d>` borrows its `Database`, so a long-lived object cannot hold
//! both - that is a self-referential struct and Rust will not have it. A
//! consumer in that position, which is every binding in a garbage-collected
//! language and the C ABI too, holds the `Database` and connects per call. Every
//! one of those was a *new session* before [`Database::connect_as`] existed, and
//! a session is what `temp.`, `ATTACH` and the connection pragmas are scoped to - so a
//! `CREATE TEMP TABLE` typed into a query console was gone by the next
//! statement, silently, with the console reporting success.
//!
//! The conformance suite's `a_temp_table_survives_a_connection_per_call` is the
//! same claim as data, run by both runners. This file is the Rust driver's own,
//! and it also covers the two things the suite cannot: that two *different*
//! sessions really are separate, and that the number is stable.

use std::path::PathBuf;

use inillucent_driver::Database;

/// Returns a scratch database nothing else is using.
///
/// @param tag - what to name it after
fn scratch(tag: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!(
        "inillucent-driver-session-{}-{tag}-{:?}.db",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = std::fs::remove_file(&path);
    path
}

/// A temporary table survives a caller that connects per call.
///
/// The shape the ticket describes: the database is held, a connection is made
/// for each statement, and nothing but the session number travels between them.
#[test]
fn a_temp_table_survives_a_connection_per_call() {
    let path = scratch("temp");
    let database = Database::open(&path).expect("the database opens");
    let session = database.session().session();

    database
        .session_as(session)
        .query(
            "CREATE TEMP TABLE scratch (a INTEGER PRIMARY KEY, b TEXT)",
            &[],
            0,
        )
        .expect("the temporary table is made");
    database
        .session_as(session)
        .query("INSERT INTO scratch VALUES (1, 'one')", &[], 0)
        .expect("the row is written");
    let rows = database
        .session_as(session)
        .query("SELECT b FROM temp.scratch", &[], 10)
        .expect("the temporary table is still there");
    assert_eq!(rows.total, 1, "the temporary table lost its row");

    // And a connection that is *not* the same session cannot see it, which is
    // what makes the assertion above about the session rather than about the
    // table having been permanent all along.
    let elsewhere = database
        .session()
        .query("SELECT b FROM scratch", &[], 10)
        .expect_err("another session must not see it");
    assert!(
        format!("{elsewhere}").contains("scratch"),
        "the refusal should name the table: {elsewhere}"
    );

    drop(database);
    let _ = std::fs::remove_file(&path);
}

/// A connection reports a session number, and two connections report two.
///
/// The number is what a caller keeps, so it has to be stable across the
/// connections that share it and different for one that does not.
#[test]
fn a_session_number_identifies_the_connection_that_reported_it() {
    let path = scratch("number");
    let database = Database::open(&path).expect("the database opens");

    let first = database.session().session();
    let second = database.session().session();
    assert_ne!(first, second, "two connections shared a session number");
    assert_eq!(
        database.session_as(first).session(),
        first,
        "a continued connection reported a different session"
    );
    assert_eq!(
        database.session_as(first).session(),
        database.session_as(first).session(),
        "the number is not stable across two continuations of one session"
    );

    drop(database);
    let _ = std::fs::remove_file(&path);
}

/// A connection pragma set once holds for every later call in the session.
///
/// The second thing the ticket names, and the reason Unluminous's session
/// re-applied `PRAGMA foreign_keys = ON` on every call: a pragma is scoped to
/// the connection, so a new session per call meant re-applying it per call, and
/// a caller that forgot had foreign keys quietly off.
#[test]
fn a_connection_pragma_holds_for_the_whole_session() {
    let path = scratch("pragma");
    let database = Database::open(&path).expect("the database opens");
    let session = database.session().session();

    database
        .session_as(session)
        .query("PRAGMA foreign_keys = ON", &[], 10)
        .expect("the pragma is accepted");
    let rows = database
        .session_as(session)
        .query("PRAGMA foreign_keys", &[], 10)
        .expect("the pragma reads back");
    let answer = rows
        .rows
        .first()
        .and_then(|row| row.first())
        .map(|value| format!("{value:?}"))
        .unwrap_or_default();
    assert!(
        answer.contains('1'),
        "the pragma did not survive the connection that set it: {answer}"
    );

    drop(database);
    let _ = std::fs::remove_file(&path);
}

/// Each connection's counters belong to it alone.
///
/// `changes()`, `total_changes()` and `last_insert_rowid()` are a
/// connection's in SQLite. They were one set of numbers for the whole
/// database, so a second connection's one row insert left the first reading
/// `changes()` 1 and `total_changes()` 4 where SQLite answers 3 and 3, and a
/// pool handed every thread whichever numbers the last statement set.
#[test]
fn a_connection_reads_its_own_counters_after_another_connection_writes() {
    let path = scratch("counters");
    let database = Database::open(&path).expect("the database opens");
    let first = database.session();
    let second = database.session();

    first
        .execute("CREATE TABLE t(a)", &[])
        .expect("the table is made");
    first
        .execute("INSERT INTO t VALUES (1), (2), (3)", &[])
        .expect("three rows are written");
    second
        .execute("INSERT INTO t VALUES (4)", &[])
        .expect("the second connection writes one row");

    assert_eq!(first.changes().expect("changes"), 3);
    assert_eq!(first.total_changes().expect("total_changes"), 3);
    assert_eq!(first.last_insert_rowid().expect("last_insert_rowid"), 3);
    assert_eq!(second.changes().expect("changes"), 1);
    assert_eq!(second.total_changes().expect("total_changes"), 1);
    assert_eq!(second.last_insert_rowid().expect("last_insert_rowid"), 4);
    let scalar = first
        .query(
            "SELECT changes(), total_changes(), last_insert_rowid()",
            &[],
            1,
        )
        .expect("the scalars answer");
    assert_eq!(
        format!("{:?}", scalar.rows.first()),
        format!(
            "{:?}",
            second
                .query("SELECT 3, 3, 3", &[], 1)
                .expect("the literal row")
                .rows
                .first()
        ),
        "the SQL scalars read another connection's counters"
    );

    drop(first);
    drop(second);
    drop(database);
    let _ = std::fs::remove_file(&path);
}
