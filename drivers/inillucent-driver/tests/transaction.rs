//! Driving a transaction statement by statement.
//!
//! Invariant: **an uncommitted transaction leaves the database as it was, and
//! the only way to keep the work is to say so.** The rollback is in `Drop`, so
//! an early return, a `?`, or a panic all end the same way; `commit()` is the
//! one thing that does not.
//!
//! ## The defect this suite was written for (task-1932, M1)
//!
//! `Connection::transaction` takes every statement up front, which is the right
//! shape for a row editor and the wrong one for anything that has to look at
//! what it just wrote before deciding the next statement. The only way to do
//! that was `execute_batch("BEGIN")`, which:
//!
//! - the nesting guard cannot see, so two callers doing it on one connection
//!   got one transaction and no warning;
//! - has no `Drop`, so a caller that returned early left the write lock held
//!   for the life of the process.
//!
//! The driver's README called it "the one surface", and this was the shape an
//! application most often needed and could not have.

use std::path::PathBuf;

use inillucent_driver::{Database, Status, Value};

/// Where this suite's scratch databases live.
///
/// One file per case: these run in parallel threads, and a shared file gives
/// whichever case loses the race a "database is locked" that looks like a
/// defect rather than the collision it is.
fn scratch(name: &str) -> PathBuf {
    let mut directory = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    directory.pop();
    directory.pop();
    let directory = directory.join("_agent_output/driver-transaction");
    let _ = std::fs::create_dir_all(&directory);
    let path = directory.join(format!("{name}.rdb"));
    for suffix in ["", "-wal", "-journal", "-shm"] {
        let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
    }
    path
}

/// Opens a database with one table in it.
///
/// @param name - the file's name
fn opened(name: &str) -> Database {
    let database = Database::open(scratch(name)).expect("the database opens");
    let connection = database.connect();
    connection
        .execute_batch("CREATE TABLE note (id INTEGER PRIMARY KEY, body TEXT)")
        .expect("the table is created");
    database
}

/// Returns how many rows the table holds, through a fresh connection.
///
/// @param database - the open database
fn count(database: &Database) -> i64 {
    let connection = database.connect();
    let rows = connection
        .query("SELECT count(*) FROM note", &[], 1)
        .expect("the count runs");
    match rows.rows.first().and_then(|row| row.first()) {
        Some(Value::Integer(count)) => *count,
        other => panic!("the count answered {other:?}"),
    }
}

/// A transaction that is dropped without committing keeps nothing.
///
/// **The whole reason `Transaction` is a value.** The write happens, the
/// statement reports one changed row, and the row is not there afterwards.
#[test]
fn a_dropped_transaction_keeps_nothing() {
    let database = opened("dropped");
    let connection = database.connect();
    {
        let transaction = connection.begin().expect("a transaction opens");
        let changed = transaction
            .execute(
                "INSERT INTO note VALUES (?1, ?2)",
                &[Value::Integer(1), Value::Text("first".to_string())],
            )
            .expect("the insert runs");
        assert_eq!(changed, 1, "the insert reported {changed} changed rows");
        // And the row is visible inside the transaction that wrote it.
        let inside = transaction
            .query("SELECT count(*) FROM note", &[], 1)
            .expect("the count runs");
        assert_eq!(
            inside.rows.first().and_then(|row| row.first()),
            Some(&Value::Integer(1)),
            "the row was not visible to the transaction that wrote it"
        );
    }
    assert_eq!(
        count(&database),
        0,
        "a transaction that was dropped without committing left its row behind"
    );
}

/// A committed transaction keeps everything.
///
/// The other half of the pair. A `Drop` that rolled back unconditionally would
/// satisfy the case above and nothing else.
#[test]
fn a_committed_transaction_keeps_everything() {
    let database = opened("committed");
    let connection = database.connect();
    let transaction = connection.begin().expect("a transaction opens");
    transaction
        .execute(
            "INSERT INTO note VALUES (?1, ?2)",
            &[Value::Integer(1), Value::Text("first".to_string())],
        )
        .expect("the first insert runs");
    transaction
        .execute(
            "INSERT INTO note VALUES (?1, ?2)",
            &[Value::Integer(2), Value::Text("second".to_string())],
        )
        .expect("the second insert runs");
    transaction.commit().expect("the commit runs");
    assert_eq!(count(&database), 2, "a committed transaction lost a row");
}

/// The work survives a reopen, which is what "committed" has to mean.
///
/// **A count through a second connection is not enough on its own.** The
/// engine holds one buffer pool per file, so a row that was never made durable
/// still answers a query in the same process. Closing the database and opening
/// it again is what asks the file.
#[test]
fn a_committed_transaction_survives_a_reopen() {
    let path = scratch("reopened");
    {
        let database = Database::open(&path).expect("the database opens");
        let connection = database.connect();
        connection
            .execute_batch("CREATE TABLE note (id INTEGER PRIMARY KEY, body TEXT)")
            .expect("the table is created");
        let transaction = connection.begin().expect("a transaction opens");
        transaction
            .execute(
                "INSERT INTO note VALUES (?1, ?2)",
                &[Value::Integer(1), Value::Text("kept".to_string())],
            )
            .expect("the insert runs");
        transaction.commit().expect("the commit runs");
    }
    let database = Database::open(&path).expect("the database reopens");
    assert_eq!(
        count(&database),
        1,
        "the committed row did not survive a reopen"
    );
}

/// And a dropped one leaves nothing behind after a reopen either.
///
/// The case the TDD names. A rollback that only held in memory would pass the
/// first test in this file and fail here.
#[test]
fn a_dropped_transaction_leaves_nothing_after_a_reopen() {
    let path = scratch("dropped-reopened");
    {
        let database = Database::open(&path).expect("the database opens");
        let connection = database.connect();
        connection
            .execute_batch("CREATE TABLE note (id INTEGER PRIMARY KEY, body TEXT)")
            .expect("the table is created");
        let transaction = connection.begin().expect("a transaction opens");
        transaction
            .execute(
                "INSERT INTO note VALUES (?1, ?2)",
                &[Value::Integer(1), Value::Text("discarded".to_string())],
            )
            .expect("the insert runs");
        // No commit. The drop at the end of this block is the rollback.
    }
    let database = Database::open(&path).expect("the database reopens");
    assert_eq!(
        count(&database),
        0,
        "a transaction that was never committed left its row in the file"
    );
}

/// Rolling back explicitly does the same thing and can be reported.
#[test]
fn an_explicit_rollback_keeps_nothing() {
    let database = opened("rolled-back");
    let connection = database.connect();
    let transaction = connection.begin().expect("a transaction opens");
    transaction
        .execute(
            "INSERT INTO note VALUES (?1, ?2)",
            &[Value::Integer(1), Value::Text("first".to_string())],
        )
        .expect("the insert runs");
    transaction.rollback().expect("the rollback runs");
    assert_eq!(count(&database), 0, "an explicit rollback kept a row");
}

/// A second transaction on the same connection is refused by name.
///
/// **The guard `execute_batch("BEGIN")` could not enforce.** Nesting is refused
/// because committing the inner one would commit the outer one's work, and a
/// caller who did not know that would have written the two as though they were
/// independent.
#[test]
fn a_second_transaction_on_one_connection_is_refused() {
    let database = opened("nested");
    let connection = database.connect();
    let _outer = connection.begin().expect("the first transaction opens");
    let refused = connection.begin().expect_err("the second is refused");
    assert_eq!(refused.status, Status::InvalidState);
    assert!(
        refused.message.contains("already open"),
        "the refusal does not say what is wrong: {}",
        refused.message
    );
}

/// A connection can open a transaction again after the first one ends.
///
/// The other side of the guard: a depth counter that was not put back would
/// make one transaction the last one a connection could ever have.
#[test]
fn a_connection_can_open_a_transaction_again_after_one_ends() {
    let database = opened("reopened-transaction");
    let connection = database.connect();

    let first = connection.begin().expect("the first transaction opens");
    first.commit().expect("the first commits");

    let second = connection.begin().expect("the second transaction opens");
    second.rollback().expect("the second rolls back");

    {
        let _third = connection.begin().expect("the third transaction opens");
        // Dropped.
    }

    let fourth = connection.begin().expect("the fourth transaction opens");
    fourth
        .execute(
            "INSERT INTO note VALUES (?1, ?2)",
            &[Value::Integer(9), Value::Text("kept".to_string())],
        )
        .expect("the insert runs");
    fourth.commit().expect("the fourth commits");
    assert_eq!(count(&database), 1);
}

/// A read-only connection refuses to open one at all.
#[test]
fn a_read_only_connection_refuses_a_transaction() {
    let path = scratch("read-only");
    {
        let database = Database::open(&path).expect("the database opens");
        let connection = database.connect();
        connection
            .execute_batch("CREATE TABLE note (id INTEGER PRIMARY KEY, body TEXT)")
            .expect("the table is created");
    }
    let options = inillucent_driver::OpenOptions {
        read_only: true,
        ..inillucent_driver::OpenOptions::default()
    };
    let database = Database::open_with(&path, options).expect("the database reopens read only");
    let connection = database.connect();
    let refused = connection.begin().expect_err("a transaction is refused");
    assert_eq!(refused.status, Status::ReadOnly);
}

/// The batch form still works, and still rolls back on a refused postcondition.
///
/// `transaction()` is now `begin()` with the statements run inside it, so this
/// is the case that says the rewrite did not change what it does.
#[test]
fn the_batch_form_still_rolls_back_on_a_refused_check() {
    let database = opened("batch");
    let connection = database.connect();
    let work = vec![
        (
            "INSERT INTO note VALUES (?1, ?2)".to_string(),
            vec![Value::Integer(1), Value::Text("first".to_string())],
        ),
        (
            "INSERT INTO note VALUES (?1, ?2)".to_string(),
            vec![Value::Integer(2), Value::Text("second".to_string())],
        ),
    ];
    let refused = connection
        .transaction(&work, |affected| {
            assert_eq!(affected, &[1, 1], "the check saw {affected:?}");
            Err(inillucent_driver::Error::said(
                Status::InvalidState,
                "the caller changed its mind.",
            ))
        })
        .expect_err("the refused check fails the transaction");
    assert!(refused.message.contains("changed its mind"));
    assert_eq!(count(&database), 0, "a refused check left rows behind");

    // And the same work with a check that passes is kept.
    let affected = connection
        .transaction(&work, |_| Ok(()))
        .expect("the transaction runs");
    assert_eq!(affected, vec![1, 1]);
    assert_eq!(count(&database), 2);
}
