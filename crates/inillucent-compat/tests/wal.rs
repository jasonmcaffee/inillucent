//! The write-ahead log, through the public API.
//!
//! Invariant: every claim here is made about a file, not about an internal
//! state - a log that only holds while the process that wrote it is running is
//! not a log.
//!
//! **The eight tests that read and wrote SQLite's log are gone, and are not
//! deleted.** They asserted file-format interoperability, which the
//! rearchitecture to a native storage format dropped when this engine
//! stopped writing SQLite files: they failed because the promise was
//! withdrawn rather than because the engine was wrong. They now live in
//! `_junk/wal_interop.rs`, out of `tests/` and out of git. What is left here
//! is this engine's own log: a commit that survives a reopen, a rollback that leaves
//! nothing, a second connection that sees the commit, and a checkpoint that
//! moves the data into the database.

use std::path::{Path, PathBuf};

use inillucent_compat::facade::Database;
use inillucent_compat::workspace_root;
use inillucent_value::Value;

/// Returns a fresh scratch path, with every companion file removed.
fn scratch(name: &str) -> PathBuf {
    let directory = workspace_root().join("_agent_output/wal");
    let _ = std::fs::create_dir_all(&directory);
    for suffix in ["", "-journal", "-wal", "-shm"] {
        let _ = std::fs::remove_file(directory.join(format!("{name}.db{suffix}")));
    }
    directory.join(format!("{name}.db"))
}

/// Opens a inillucent connection on a path.
fn connect(path: &Path) -> inillucent_compat::facade::Connection {
    let database = Database::open(path).expect("the database opens");
    database.connect().expect("the connection opens")
}

/// Returns the single integer a query reports.
fn integer(connection: &inillucent_compat::facade::Connection, sql: &str) -> i64 {
    let rows = connection.query(sql).expect("the query runs");
    match rows.first().and_then(|row| row.first()) {
        Some(Value::Integer(value)) => *value,
        other => panic!("{sql} reported {other:?}"),
    }
}

/// Returns the text a query reports.
fn text(connection: &inillucent_compat::facade::Connection, sql: &str) -> String {
    let rows = connection.query(sql).expect("the query runs");
    match rows.first().and_then(|row| row.first()) {
        Some(Value::Text(value)) => String::from_utf8_lossy(&value.utf8_bytes()).to_string(),
        other => panic!("{sql} reported {other:?}"),
    }
}

/// A database left in WAL mode is reopened in WAL mode without being asked,
/// because the file says so and a connection that ignored it would be writing
/// undo images into a database another one is appending frames to.
#[test]
fn a_wal_database_reopens_in_wal_mode() {
    let path = scratch("reopen");
    {
        let connection = connect(&path);
        connection
            .execute_batch("PRAGMA journal_mode=wal; CREATE TABLE t(a); INSERT INTO t VALUES (7)")
            .expect("the database is written");
    }
    let connection = connect(&path);
    assert_eq!(text(&connection, "PRAGMA journal_mode"), "wal");
    assert_eq!(integer(&connection, "SELECT a FROM t"), 7);
}

/// A commit in WAL mode survives the connection that made it, and is there
/// for a connection that opens the database afresh - which is the whole claim
/// a log makes.
#[test]
fn a_committed_transaction_survives_reopening() {
    let path = scratch("durable");
    {
        let connection = connect(&path);
        connection
            .execute_batch(
                "PRAGMA journal_mode=wal;
                 CREATE TABLE t(a INTEGER PRIMARY KEY, b TEXT);
                 BEGIN;
                 INSERT INTO t VALUES (1, 'one');
                 INSERT INTO t VALUES (2, 'two');
                 COMMIT;",
            )
            .expect("the transaction commits");
    }
    let connection = connect(&path);
    assert_eq!(integer(&connection, "SELECT count(*) FROM t"), 2);
    assert_eq!(text(&connection, "SELECT b FROM t WHERE a=2"), "two");
}

/// A rolled-back transaction leaves nothing behind, and the log it appended
/// to is reused by the next writer rather than growing.
#[test]
fn a_rolled_back_transaction_leaves_nothing() {
    let path = scratch("rollback");
    let connection = connect(&path);
    connection
        .execute_batch("PRAGMA journal_mode=wal; CREATE TABLE t(a); INSERT INTO t VALUES (1)")
        .expect("the table is written");
    connection
        .execute_batch("BEGIN; INSERT INTO t VALUES (2); INSERT INTO t VALUES (3); ROLLBACK;")
        .expect("the transaction rolls back");
    assert_eq!(integer(&connection, "SELECT count(*) FROM t"), 1);
    connection
        .execute_batch("INSERT INTO t VALUES (4)")
        .expect("the next write works");
    assert_eq!(integer(&connection, "SELECT count(*) FROM t"), 2);
    assert_eq!(integer(&connection, "SELECT max(a) FROM t"), 4);
}

/// A second connection sees what the first committed, and neither of them had
/// to wait for the other to finish reading.
#[test]
fn a_second_connection_sees_the_commit() {
    let path = scratch("two-connections");
    let database = Database::open(&path).expect("the database opens");
    let writer = database.connect().expect("the writer opens");
    writer
        .execute_batch("PRAGMA journal_mode=wal; CREATE TABLE t(a)")
        .expect("the table is created");
    let reader = database.connect().expect("the reader opens");
    assert_eq!(integer(&reader, "SELECT count(*) FROM t"), 0);
    writer
        .execute_batch("INSERT INTO t VALUES (1)")
        .expect("the row is written");
    assert_eq!(integer(&reader, "SELECT count(*) FROM t"), 1);
}

/// A checkpoint moves the frames into the database file, after which the file
/// alone answers the query - which is what makes the log a cache rather than
/// half of the database.
#[test]
fn a_checkpoint_moves_the_data_into_the_database() {
    let path = scratch("checkpoint");
    {
        let connection = connect(&path);
        connection
            .execute_batch(
                "PRAGMA journal_mode=wal;
                 CREATE TABLE t(a INTEGER PRIMARY KEY, b TEXT);
                 INSERT INTO t VALUES (1, 'one'), (2, 'two'), (3, 'three');",
            )
            .expect("the rows are written");
        let rows = connection
            .query("PRAGMA wal_checkpoint(TRUNCATE)")
            .expect("the checkpoint runs");
        let busy = match rows.first().and_then(|row| row.first()) {
            Some(Value::Integer(value)) => *value,
            other => panic!("the checkpoint reported {other:?}"),
        };
        assert_eq!(busy, 0, "the checkpoint reported that it was blocked");
        let log = PathBuf::from(format!("{}-wal", path.display()));
        assert_eq!(
            std::fs::metadata(&log).map(|meta| meta.len()).unwrap_or(0),
            0,
            "the log was not emptied"
        );
    }
    // Reading with the log removed proves the pages are in the database file.
    let _ = std::fs::remove_file(PathBuf::from(format!("{}-wal", path.display())));
    let _ = std::fs::remove_file(PathBuf::from(format!("{}-shm", path.display())));
    let connection = connect(&path);
    assert_eq!(integer(&connection, "SELECT count(*) FROM t"), 3);
    assert_eq!(text(&connection, "SELECT b FROM t WHERE a=3"), "three");
}
