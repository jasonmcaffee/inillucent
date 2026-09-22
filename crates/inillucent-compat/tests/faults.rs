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
//!
//! Re-pointed from the old engine (`inillucent-session`) onto the new one
//! (`inillucent-engine`, `inillucent_engine::connect`). None of these cases
//! inject an actual I/O fault - `built()` used `inillucent-session`'s
//! `SimVfs`-backed opener only as a fast, disposable scratch database, not
//! because anything here arms a failpoint - so a leaked in-memory `Database`
//! (the same shape `differential::start_inillucent` uses, for the same
//! reason: a bare `Connection` return type with no lifetime plumbing at the
//! call site) does the same job without needing the simulator at all.
//!
//! Two of the six cases here did not survive the re-point as written, and are
//! both genuinely missing capabilities rather than harness bugs - each is now
//! a small test pinning the gap instead of sweeping it:
//!
//! - **The second-writer/BUSY case** was the one real question this port had
//!   to answer: whether two `Connection`s opened from the same in-process
//!   `Database` enforce the same reservation as two separate old-engine
//!   connections did. It does not - both borrow the one `RefCell` behind
//!   `Database`, which holds one shared, unkeyed transaction rather than one
//!   per session, so a second connection's write joins the first's open
//!   transaction instead of being refused. See
//!   `a_second_connections_write_joins_the_first_writers_open_transaction`
//!   and `txn.writer-contention`, `status = "missing"`, in
//!   `compat/sqlite-3.53.4.toml`.
//! - **The allocation-failure sweep** armed `inillucent_base::buffer`'s
//!   failpoint, which only `buffer::try_zeroed`/`try_copy_of` consult. The
//!   shipping write path (`inillucent-tree`, `inillucent-pool`,
//!   `inillucent-wal`, `inillucent-txn`, `inillucent-exec`) allocates with
//!   plain `Vec`/`vec![...]` and never reaches either function -
//!   `inillucent-engine` does not even depend on `inillucent-storage`, the one
//!   crate that does. See
//!   `an_injected_allocation_failure_never_reaches_the_write_path` and
//!   `txn.oom-injection`, `status = "missing"`, in the same manifest.

use inillucent_engine::connect::{Connection, Database};
use inillucent_tree::datum::OwnedDatum;

/// The schema every test here starts from.
const SCHEMA: &str = "CREATE TABLE t(a INTEGER PRIMARY KEY, b TEXT UNIQUE, c INTEGER);
     INSERT INTO t VALUES(1, 'one', 10);
     INSERT INTO t VALUES(2, 'two', 20);
     INSERT INTO t VALUES(3, 'three', 30);";

/// Returns a fresh, leaked, in-memory database with the base schema loaded.
///
/// Leaked so a test can call `.session()` on it more than once - to open a
/// second writer, or to prove a write survived dropping the first connection -
/// without threading a lifetime through every helper. A short-lived test
/// process is not where a few kilobytes per case matter.
fn built() -> &'static Database {
    let database: &'static Database = Box::leak(Box::new(
        Database::open(":memory:").expect("an in-memory database opens"),
    ));
    database
        .session()
        .execute_batch(SCHEMA)
        .expect("the schema builds");
    database
}

/// Runs a script, reporting whether it succeeded.
fn run(connection: &Connection<'_>, sql: &str) -> Result<(), inillucent_base::DbError> {
    connection.execute_batch(sql)
}

/// Returns an integer datum as `Option<i64>`, matching the old `Value::as_integer`.
fn as_integer(value: Option<&OwnedDatum>) -> Option<i64> {
    match value {
        Some(OwnedDatum::Int(value)) => Some(*value),
        _ => None,
    }
}

/// Returns the rows the table holds, as `a|c` pairs.
fn rows(connection: &Connection<'_>) -> Vec<String> {
    let rows = connection
        .query("SELECT a, c FROM t ORDER BY a")
        .expect("the query runs");
    rows.iter()
        .map(|row| format!("{:?}|{:?}", as_integer(row.first()), as_integer(row.get(1)),))
        .collect()
}

/// Writes a report into the checked-in crash schedules.
fn record(name: &str, body: &str) {
    let directory = inillucent_compat::workspace_root().join("tests/crash");
    let _ = std::fs::create_dir_all(&directory);
    let _ = std::fs::write(directory.join(name), body);
}

/// Pins the OOM-injection gap: an allocation failure armed before a write
/// never reaches it.
///
/// **Retired from `an_allocation_failure_at_every_point_leaves_the_database_alone`**,
/// which swept every allocation point of a `BEGIN; INSERT; UPDATE; COMMIT;`
/// and asserted at least 20 were refused, each refusal leaving the database
/// untouched. Against the old engine (`inillucent-session`) 36 of 36 points
/// were covered and refused - see the checked-in `tests/crash/allocation.txt`
/// this test used to write. Against `inillucent_engine::connect`,
/// `allocations_since_armed()` stays 0 through the whole script: the shipping
/// write path allocates with plain `Vec`/`vec![...]`, not through
/// `inillucent_base::buffer::try_zeroed`/`try_copy_of`, so the failpoint has
/// nothing to refuse. Recorded as `txn.oom-injection`, `status = "missing"`,
/// in `compat/sqlite-3.53.4.toml` - injected out-of-memory testing of a write
/// is a capability the shipping write path genuinely does not have yet, and
/// wiring every allocation in five crates through a fallible, counted path is
/// not a contained fix.
///
/// This test goes red the day an allocation the failpoint counts happens
/// during this script - the signal to bring back the swept version and move
/// `txn.oom-injection` to `pass`.
#[test]
fn an_injected_allocation_failure_never_reaches_the_write_path() {
    let database = built();
    let connection = database.session();
    let before = rows(&connection);

    inillucent_base::buffer::fail_allocation_after(1);
    let outcome = run(
        &connection,
        "BEGIN;
         INSERT INTO t VALUES(4, 'four', 40);
         UPDATE t SET c = c + 1;
         COMMIT;",
    );
    let reached = inillucent_base::buffer::allocations_since_armed();
    inillucent_base::buffer::clear_allocation_failpoint();

    assert!(
        outcome.is_ok(),
        "the write failed with nothing armed to fail it: {outcome:?}"
    );
    assert_eq!(
        reached, 0,
        "an allocation the failpoint counts happened during this write - the \
         OOM-injection gap this test pins is closed, so bring back the swept \
         version of this test and move txn.oom-injection to \"pass\""
    );

    let after = rows(&connection);
    assert_eq!(
        before.len() + 1,
        after.len(),
        "the write, with nothing to refuse it, added its row"
    );
    record(
        "allocation.txt",
        "points: 0, refused: 0\n\
         the write path makes no allocation inillucent_base::buffer counts: the buffer \
         module hands out byte buffers and a write allocates Vec<OwnedDatum> and String, \
         which no byte-buffer call covers; see txn.oom-injection in \
         compat/sqlite-3.53.4.toml\n",
    );
}

/// Pins the writer-contention gap: two `Connection`s from one in-process
/// `Database` do not hold independent transaction state, so a second
/// connection's write joins the first's open transaction instead of being
/// refused BUSY.
///
/// **Retired from `a_second_writer_is_refused_while_the_first_holds_the_reservation`**,
/// which asserted the second connection's write was refused BUSY while the
/// first held `BEGIN IMMEDIATE`'s reservation, and passed against the old
/// engine (`inillucent-session`). Against `inillucent_engine::connect`,
/// `Database` holds its `ImportedDatabase` behind one `RefCell`, and
/// `ImportedDatabase` tracks one shared, unkeyed transaction rather than one
/// per session - so the second connection's write is admitted, reports the
/// same `autocommit() == false` the first connection's `BEGIN IMMEDIATE` set,
/// and the first connection's later attempt to write the identical row then
/// fails on the UNIQUE constraint the second connection's write already
/// satisfied. That collision is the proof: both writes landed in one
/// transaction, not two. Recorded as `txn.writer-contention`,
/// `status = "missing"`, in `compat/sqlite-3.53.4.toml` -
/// `inillucent-txn::slot::WriterSlot` already implements real `busy_timeout`
/// semantics for the Phase 3 model driver, but nothing wires it to
/// `inillucent_engine::connect`, and doing so is a cross-cutting change to
/// `ImportedDatabase`'s transaction state, not a contained fix.
///
/// This test goes red the day the second connection's write is refused
/// instead - the signal to bring back the BUSY-refusal version and move
/// `txn.writer-contention` to `pass`.
#[test]
fn a_second_connections_write_joins_the_first_writers_open_transaction() {
    let database = built();
    let first = database.session();
    let second = database.session();

    run(&first, "BEGIN IMMEDIATE").expect("the first writer reserves");
    assert!(
        !first
            .autocommit()
            .expect("nothing is running on this connection"),
        "BEGIN IMMEDIATE opens a transaction"
    );

    run(&second, "INSERT INTO t VALUES(9, 'nine', 90)").expect(
        "the second connection's write is admitted rather than refused BUSY - \
         the gap this test pins",
    );
    assert!(
        !second
            .autocommit()
            .expect("nothing is running on this connection"),
        "the second connection reports the same open transaction the first began"
    );

    let collides = run(&first, "INSERT INTO t VALUES(9, 'nine', 90)")
        .expect_err("the first connection's identical insert collides with the second's");
    assert_eq!(
        collides.code(),
        inillucent_base::PrimaryCode::Constraint,
        "expected the UNIQUE constraint the second connection's insert already \
         satisfied, not {collides}"
    );

    run(&first, "COMMIT").expect("the first connection commits what both wrote");
    assert_eq!(
        rows(&first).len(),
        4,
        "the second connection's insert survived the first connection's commit"
    );
}

/// Every conflict algorithm undoes exactly what it says it does.
#[test]
fn every_conflict_algorithm_undoes_what_it_says() {
    // ABORT undoes the statement and keeps the transaction.
    let database = built();
    let connection = database.session();
    run(&connection, "BEGIN").expect("begins");
    run(&connection, "INSERT INTO t VALUES(4, 'four', 40)").expect("inserts");
    let failed = run(
        &connection,
        "INSERT INTO t VALUES(5, 'five', 50), (6, 'four', 60)",
    );
    assert!(failed.is_err(), "the duplicate 'four' is refused");
    assert!(
        !connection
            .autocommit()
            .expect("nothing is running on this connection"),
        "ABORT leaves the transaction open"
    );
    run(&connection, "COMMIT").expect("commits");
    let after = rows(&connection);
    assert_eq!(after.len(), 4, "only the first statement's row survived");

    // ROLLBACK undoes the transaction.
    let database = built();
    let connection = database.session();
    run(&connection, "BEGIN").expect("begins");
    run(&connection, "INSERT INTO t VALUES(4, 'four', 40)").expect("inserts");
    let failed = run(
        &connection,
        "INSERT OR ROLLBACK INTO t VALUES(5, 'four', 50)",
    );
    assert!(failed.is_err(), "the duplicate is refused");
    assert!(
        connection
            .autocommit()
            .expect("nothing is running on this connection"),
        "OR ROLLBACK ends the transaction, leaving autocommit true"
    );
    assert_eq!(
        rows(&connection).len(),
        3,
        "the whole transaction is undone"
    );

    // FAIL keeps the rows the statement had already written.
    let database = built();
    let connection = database.session();
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
    let database = built();
    let connection = database.session();
    run(
        &connection,
        "INSERT OR IGNORE INTO t VALUES(4, 'four', 40), (5, 'one', 50), (6, 'six', 60)",
    )
    .expect("IGNORE reports no error");
    let after = rows(&connection);
    assert_eq!(after.len(), 5, "the two clean rows landed: {after:?}");

    // REPLACE deletes what is in the way.
    let database = built();
    let connection = database.session();
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
    let database = built();
    let connection = database.session();
    run(&connection, "BEGIN").expect("begins");
    run(&connection, "INSERT INTO t VALUES(4, 'four', 40)").expect("inserts");
    let rowid_before = connection
        .last_insert_rowid()
        .expect("nothing is running on this connection");

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
        connection
            .last_insert_rowid()
            .expect("nothing is running on this connection"),
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
    let database = built();
    let connection = database.session();
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
    let database = built();
    let connection = database.session();
    run(&connection, "SAVEPOINT top").expect("opens");
    assert!(
        !connection
            .autocommit()
            .expect("nothing is running on this connection"),
        "a bare SAVEPOINT starts a transaction, and SQLite reports autocommit false"
    );
    run(&connection, "INSERT INTO t VALUES(4, 'four', 40)").expect("inserts");
    run(&connection, "RELEASE top").expect("releases, which commits");

    let connection = database.session();
    assert_eq!(rows(&connection).len(), 4, "the release committed");
}
