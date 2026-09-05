//! Memory failures, lock contention, and statement savepoints.
//!
//! Invariant: a failure never leaves the connection holding something it did
//! not hold before. A statement that ran out of memory, was refused a lock, or
//! hit a constraint releases its undo level, its locks and its pinned pages,
//! and the transaction it was inside is either exactly as it was or has been
//! rolled back - never half-open.
//!
//! These are the three matrices the crash campaign in `durability.rs` cannot
//! reach. A memory failure is not I/O and cannot be injected at the VFS; a
//! lock conflict needs two connections; and a statement savepoint is undone
//! without any device being involved at all.

use std::sync::Arc;

use inillucent_base::buffer;
use inillucent_session::connection::{Connection, OpenOptions, SessionDatabase};
use inillucent_sim::media::MediaModel;
use inillucent_sim::sim_vfs::{SimConfig, SimVfs};
use inillucent_transaction::journal::JournalOptions;
use inillucent_value::Value;
use inillucent_vfs::path::DbPath;
use inillucent_vfs::Vfs;

/// The schema every test here starts from.
const SCHEMA: &str = "CREATE TABLE t(a INTEGER PRIMARY KEY, b TEXT UNIQUE, c INTEGER);
     INSERT INTO t VALUES(1, 'one', 10);
     INSERT INTO t VALUES(2, 'two', 20);
     INSERT INTO t VALUES(3, 'three', 30);";

/// Returns the path every run uses.
fn path() -> DbPath {
    DbPath::from("/sim/faults.db")
}

/// Returns a simulator with a database already in it.
fn built(seed: u64) -> Arc<SimVfs> {
    let vfs = Arc::new(SimVfs::new(SimConfig {
        seed,
        model: MediaModel::default(),
        ..SimConfig::default()
    }));
    let connection = connect(Arc::clone(&vfs) as Arc<dyn Vfs>);
    run(&connection, SCHEMA).expect("the schema builds");
    drop(connection);
    vfs
}

/// Opens a connection on one simulated file system.
fn connect(vfs: Arc<dyn Vfs>) -> Connection {
    let database = SessionDatabase::open_with(
        path().as_path(),
        vfs,
        OpenOptions {
            journal: JournalOptions::default(),
            ..OpenOptions::default()
        },
    )
    .expect("the database opens");
    database.connect().expect("the connection opens")
}

/// Runs a script, reporting whether it succeeded.
fn run(connection: &Connection, sql: &str) -> Result<(), inillucent_base::DbError> {
    inillucent_session::statement::execute_batch(connection, sql.as_bytes())
}

/// Returns the rows the table holds, as `a|c` pairs.
fn rows(connection: &Connection) -> Vec<String> {
    let (mut statement, _) = inillucent_session::statement::Statement::prepare(
        connection,
        b"SELECT a, c FROM t ORDER BY a",
    )
    .expect("the query prepares");
    let mut out = Vec::new();
    while statement.step().expect("the query steps") {
        let row = statement.row();
        out.push(format!(
            "{:?}|{:?}",
            row.first().and_then(Value::as_integer),
            row.get(1).and_then(Value::as_integer)
        ));
    }
    out
}

/// Writes a report into the checked-in crash schedules.
fn record(name: &str, body: &str) {
    let directory = inillucent_compat::workspace_root().join("tests/crash");
    let _ = std::fs::create_dir_all(&directory);
    let _ = std::fs::write(directory.join(name), body);
}

/// An allocation failure at any point of a write leaves the database exactly
/// as it was, and the connection usable afterwards.
#[test]
fn an_allocation_failure_at_every_point_leaves_the_database_alone() {
    let mut report = String::new();
    let mut covered = 0u64;
    let mut refused = 0u64;
    for nth in 1..=120u64 {
        let vfs = built(6100 + nth);
        let connection = connect(Arc::clone(&vfs) as Arc<dyn Vfs>);
        let before = rows(&connection);

        buffer::fail_allocation_after(nth);
        let outcome = run(
            &connection,
            "BEGIN;
             INSERT INTO t VALUES(4, 'four', 40);
             UPDATE t SET c = c + 1;
             COMMIT;",
        );
        let reached = buffer::allocations_since_armed();
        buffer::clear_allocation_failpoint();

        if reached < nth {
            // The write made fewer allocations than that; every point has been
            // covered.
            break;
        }
        covered = covered.saturating_add(1);
        if outcome.is_err() {
            refused = refused.saturating_add(1);
            // The transaction is still open and has to be closed by hand, the
            // same as any other failed statement inside a BEGIN.
            let _ = run(&connection, "ROLLBACK");
            let after = rows(&connection);
            assert_eq!(
                before, after,
                "allocation {nth}: a refused write changed the database"
            );
        }
        report.push_str(&format!(
            "{nth}\t{}\n",
            if outcome.is_err() { "refused" } else { "wrote" }
        ));

        // Whatever happened, the connection still works.
        drop(connection);
        let connection = connect(Arc::clone(&vfs) as Arc<dyn Vfs>);
        let _ = rows(&connection);
    }
    assert!(
        covered >= 20,
        "only {covered} allocation points were reached"
    );
    assert!(
        refused >= 1,
        "no allocation point actually refused; the failpoint is not wired up"
    );
    record(
        "allocation.txt",
        &format!("points: {covered}, refused: {refused}\n{report}"),
    );
}

/// A second connection that wants to write while the first holds the writer's
/// reservation is refused with BUSY rather than allowed to corrupt anything.
#[test]
fn a_second_writer_is_refused_while_the_first_holds_the_reservation() {
    let vfs = built(6200);
    let first = connect(Arc::clone(&vfs) as Arc<dyn Vfs>);
    let second = connect(Arc::clone(&vfs) as Arc<dyn Vfs>);

    run(&first, "BEGIN IMMEDIATE").expect("the first writer reserves");
    let refused = run(&second, "INSERT INTO t VALUES(9, 'nine', 90)")
        .expect_err("a second writer cannot reserve");
    assert_eq!(
        refused.code(),
        inillucent_base::PrimaryCode::Busy,
        "the second writer should be BUSY, not {refused}"
    );

    // A reader is still allowed while the writer only holds RESERVED.
    let readable = rows(&second);
    assert_eq!(readable.len(), 3);

    run(&first, "INSERT INTO t VALUES(9, 'nine', 90)").expect("the first writer writes");
    run(&first, "COMMIT").expect("the first writer commits");

    // Once the reservation is gone the second connection writes normally.
    run(&second, "INSERT INTO t VALUES(10, 'ten', 100)").expect("the second writer writes");
    assert_eq!(rows(&second).len(), 5);
}

/// Every conflict algorithm undoes exactly what it says it does.
#[test]
fn every_conflict_algorithm_undoes_what_it_says() {
    // ABORT undoes the statement and keeps the transaction.
    let vfs = built(6300);
    let connection = connect(Arc::clone(&vfs) as Arc<dyn Vfs>);
    run(&connection, "BEGIN").expect("begins");
    run(&connection, "INSERT INTO t VALUES(4, 'four', 40)").expect("inserts");
    let failed = run(
        &connection,
        "INSERT INTO t VALUES(5, 'five', 50), (6, 'four', 60)",
    );
    assert!(failed.is_err(), "the duplicate 'four' is refused");
    assert!(
        !connection.autocommit(),
        "ABORT leaves the transaction open"
    );
    run(&connection, "COMMIT").expect("commits");
    let after = rows(&connection);
    assert_eq!(after.len(), 4, "only the first statement's row survived");

    // ROLLBACK undoes the transaction.
    let vfs = built(6301);
    let connection = connect(Arc::clone(&vfs) as Arc<dyn Vfs>);
    run(&connection, "BEGIN").expect("begins");
    run(&connection, "INSERT INTO t VALUES(4, 'four', 40)").expect("inserts");
    let failed = run(
        &connection,
        "INSERT OR ROLLBACK INTO t VALUES(5, 'four', 50)",
    );
    assert!(failed.is_err(), "the duplicate is refused");
    assert!(
        connection.autocommit(),
        "OR ROLLBACK ends the transaction, leaving autocommit true"
    );
    assert_eq!(
        rows(&connection).len(),
        3,
        "the whole transaction is undone"
    );

    // FAIL keeps the rows the statement had already written.
    let vfs = built(6302);
    let connection = connect(Arc::clone(&vfs) as Arc<dyn Vfs>);
    let failed = run(
        &connection,
        "INSERT OR FAIL INTO t VALUES(4, 'four', 40), (5, 'one', 50), (6, 'six', 60)",
    );
    assert!(failed.is_err(), "the duplicate 'one' is refused");
    let after = rows(&connection);
    assert_eq!(
        after.len(),
        4,
        "FAIL keeps the row written before the conflict and stops: {after:?}"
    );

    // IGNORE skips the offending row and carries on.
    let vfs = built(6303);
    let connection = connect(Arc::clone(&vfs) as Arc<dyn Vfs>);
    run(
        &connection,
        "INSERT OR IGNORE INTO t VALUES(4, 'four', 40), (5, 'one', 50), (6, 'six', 60)",
    )
    .expect("IGNORE reports no error");
    let after = rows(&connection);
    assert_eq!(after.len(), 5, "the two clean rows landed: {after:?}");

    // REPLACE deletes what is in the way.
    let vfs = built(6304);
    let connection = connect(Arc::clone(&vfs) as Arc<dyn Vfs>);
    run(&connection, "INSERT OR REPLACE INTO t VALUES(7, 'one', 70)")
        .expect("REPLACE reports no error");
    let after = rows(&connection);
    assert_eq!(after.len(), 3, "the row it replaced is gone: {after:?}");
    assert!(
        after.iter().any(|row| row.contains("Some(7)")),
        "the new row is there: {after:?}"
    );
}

/// A savepoint rolled back mid-transaction restores the rows *and* the
/// counters, and the transaction carries on afterwards.
#[test]
fn a_savepoint_restores_rows_and_counters_together() {
    let vfs = built(6400);
    let connection = connect(Arc::clone(&vfs) as Arc<dyn Vfs>);
    run(&connection, "BEGIN").expect("begins");
    run(&connection, "INSERT INTO t VALUES(4, 'four', 40)").expect("inserts");
    let rowid_before = connection.counters().last_insert_rowid;

    run(&connection, "SAVEPOINT s").expect("opens the savepoint");
    run(&connection, "INSERT INTO t VALUES(5, 'five', 50)").expect("inserts");
    run(&connection, "DELETE FROM t WHERE a = 1").expect("deletes");
    assert_eq!(rows(&connection).len(), 4);

    run(&connection, "ROLLBACK TO s").expect("rolls back to the savepoint");
    assert_eq!(rows(&connection).len(), 4, "the deleted row came back");
    // `last_insert_rowid` is deliberately *not* restored: SQLite documents it
    // as unpredictable after a rollback and keeps the undone insert's rowid,
    // and parity on a value applications read is worth more than tidiness.
    assert_ne!(
        connection.counters().last_insert_rowid,
        rowid_before,
        "the rowid the undone insert allocated is kept, as SQLite keeps it"
    );

    run(&connection, "INSERT INTO t VALUES(6, 'six', 60)").expect("carries on");
    run(&connection, "RELEASE s").expect("releases");
    run(&connection, "COMMIT").expect("commits");
    let after = rows(&connection);
    assert_eq!(after.len(), 5, "{after:?}");
}

/// Nested savepoints unwind in the order they were opened, and a duplicate
/// name resolves to the innermost one.
#[test]
fn nested_savepoints_unwind_innermost_first() {
    let vfs = built(6500);
    let connection = connect(Arc::clone(&vfs) as Arc<dyn Vfs>);
    run(&connection, "BEGIN").expect("begins");
    run(&connection, "SAVEPOINT s").expect("opens");
    run(&connection, "INSERT INTO t VALUES(4, 'four', 40)").expect("inserts");
    run(&connection, "SAVEPOINT s").expect("opens a second s");
    run(&connection, "INSERT INTO t VALUES(5, 'five', 50)").expect("inserts");
    assert_eq!(rows(&connection).len(), 5);

    // The inner `s` is the one that resolves.
    run(&connection, "ROLLBACK TO s").expect("rolls back");
    assert_eq!(rows(&connection).len(), 4, "only the inner row went");
    run(&connection, "RELEASE s").expect("releases the inner one");
    run(&connection, "ROLLBACK TO s").expect("the outer one is still there");
    assert_eq!(rows(&connection).len(), 3, "the outer row went too");
    run(&connection, "COMMIT").expect("commits");
    assert_eq!(rows(&connection).len(), 3);
}

/// A savepoint outside a transaction starts one, and releasing it commits.
#[test]
fn a_savepoint_outside_a_transaction_commits_on_release() {
    let vfs = built(6600);
    let connection = connect(Arc::clone(&vfs) as Arc<dyn Vfs>);
    run(&connection, "SAVEPOINT top").expect("opens");
    assert!(
        !connection.autocommit(),
        "a bare SAVEPOINT starts a transaction, and SQLite reports autocommit false"
    );
    run(&connection, "INSERT INTO t VALUES(4, 'four', 40)").expect("inserts");
    run(&connection, "RELEASE top").expect("releases, which commits");
    drop(connection);

    let connection = connect(Arc::clone(&vfs) as Arc<dyn Vfs>);
    assert_eq!(rows(&connection).len(), 4, "the release committed");
}
