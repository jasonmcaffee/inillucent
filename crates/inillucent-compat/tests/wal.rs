//! The write-ahead log, through the public API and against the pinned SQLite.
//!
//! Invariant: every claim here is made about a file, not about an internal
//! state. A log is a format two engines share, so the tests write with one
//! engine and read with the other wherever the format contract allows it, and
//! where it does not - the shared-memory index, which SQLite rebuilds rather
//! than trusts across builds - they say so and test the rebuild instead.

use std::path::{Path, PathBuf};
use std::process::Command;

use inillucent::{Database, Value};
use inillucent_compat::workspace_root;

/// Returns the pinned SQLite shell, or `None` when it has not been downloaded.
fn pinned_shell() -> Option<PathBuf> {
    if let Ok(explicit) = std::env::var("INILLUCENT_SQLITE_SHELL") {
        let path = PathBuf::from(explicit);
        if path.is_file() {
            return Some(path);
        }
    }
    let directory = workspace_root().join(".sqlite-ref/3.53.4/shell");
    let names: [&str; 2] = if cfg!(windows) {
        ["sqlite3.exe", "sqlite3"]
    } else {
        ["sqlite3", "sqlite3.exe"]
    };
    for name in names {
        let path = directory.join(name);
        if path.is_file() {
            return Some(path);
        }
    }
    None
}

/// Returns a fresh scratch path, with every companion file removed.
fn scratch(name: &str) -> PathBuf {
    let directory = workspace_root().join("_agent_output/task-1788/wal");
    let _ = std::fs::create_dir_all(&directory);
    for suffix in ["", "-journal", "-wal", "-shm"] {
        let _ = std::fs::remove_file(directory.join(format!("{name}.db{suffix}")));
    }
    directory.join(format!("{name}.db"))
}

/// Runs statements in the pinned shell and returns its stdout.
fn shell(path: &Path, statements: &[&str]) -> Option<String> {
    let program = pinned_shell()?;
    let mut script = String::new();
    for statement in statements {
        script.push_str(statement);
        script.push('\n');
    }
    let output = Command::new(program).arg(path).arg(&script).output().ok()?;
    assert!(
        output.status.success(),
        "the shell failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    Some(String::from_utf8_lossy(&output.stdout).to_string())
}

/// Opens a inillucent connection on a path.
fn connect(path: &Path) -> inillucent::Connection {
    let database = Database::open(path).expect("the database opens");
    database.connect().expect("the connection opens")
}

/// Returns the single integer a query reports.
fn integer(connection: &inillucent::Connection, sql: &str) -> i64 {
    let rows = connection.query(sql).expect("the query runs");
    match rows.first().and_then(|row| row.first()) {
        Some(Value::Integer(value)) => *value,
        other => panic!("{sql} reported {other:?}"),
    }
}

/// Returns the text a query reports.
fn text(connection: &inillucent::Connection, sql: &str) -> String {
    let rows = connection.query(sql).expect("the query runs");
    match rows.first().and_then(|row| row.first()) {
        Some(Value::Text(value)) => String::from_utf8_lossy(&value.utf8_bytes()).to_string(),
        other => panic!("{sql} reported {other:?}"),
    }
}

/// Switching to WAL mode reports the new mode, stamps the file format
/// versions that announce it, and creates the log beside the database.
#[test]
fn switching_to_wal_stamps_the_file_and_creates_the_log() {
    let path = scratch("switch");
    {
        let connection = connect(&path);
        connection
            .execute_batch("CREATE TABLE t(a INTEGER PRIMARY KEY, b TEXT)")
            .expect("the table is created");
        assert_eq!(text(&connection, "PRAGMA journal_mode=wal"), "wal");
        connection
            .execute_batch("INSERT INTO t VALUES (1, 'one'), (2, 'two')")
            .expect("the rows are written");
        assert_eq!(integer(&connection, "SELECT count(*) FROM t"), 2);
    }
    let mut header = [0u8; 20];
    let bytes = std::fs::read(&path).expect("the database is readable");
    header.copy_from_slice(&bytes[..20]);
    assert_eq!(header[18], 2, "the write version does not say WAL");
    assert_eq!(header[19], 2, "the read version does not say WAL");
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

/// Switching back out of WAL mode empties the log, removes it, and puts the
/// format versions back, so the file is one an engine with no WAL support
/// could read.
#[test]
fn switching_out_of_wal_mode_leaves_a_rollback_database() {
    let path = scratch("switch-back");
    {
        let connection = connect(&path);
        connection
            .execute_batch(
                "PRAGMA journal_mode=wal;
                 CREATE TABLE t(a);
                 INSERT INTO t VALUES (1), (2);",
            )
            .expect("the rows are written");
        assert_eq!(text(&connection, "PRAGMA journal_mode=delete"), "delete");
        connection
            .execute_batch("INSERT INTO t VALUES (3)")
            .expect("the next write works in rollback mode");
    }
    let bytes = std::fs::read(&path).expect("the database is readable");
    assert_eq!(bytes[18], 1, "the write version still says WAL");
    assert_eq!(bytes[19], 1, "the read version still says WAL");
    assert!(
        !PathBuf::from(format!("{}-wal", path.display())).exists(),
        "the log was left behind"
    );
    let connection = connect(&path);
    assert_eq!(integer(&connection, "SELECT count(*) FROM t"), 3);
}

/// SQLite reads a WAL database inillucent wrote, with the transactions still only
/// in the log. This is the interop claim that matters: the log's format is
/// the contract, and the shared-memory index is rebuilt from it.
#[test]
fn sqlite_reads_a_log_inillucent_wrote() {
    let Some(_) = pinned_shell() else {
        eprintln!("the pinned shell is not present; skipping");
        return;
    };
    let path = scratch("sqlite-reads");
    {
        let connection = connect(&path);
        connection
            .execute_batch(
                "PRAGMA journal_mode=wal;
                 CREATE TABLE t(a INTEGER PRIMARY KEY, b TEXT);
                 INSERT INTO t VALUES (1, 'one'), (2, 'two'), (3, 'three');",
            )
            .expect("the rows are written");
        // The log is deliberately left un-checkpointed: SQLite has to read the
        // frames, not the database file.
        let log = PathBuf::from(format!("{}-wal", path.display()));
        assert!(log.exists(), "there is no log for SQLite to read");
        assert!(
            std::fs::metadata(&log).expect("the log is readable").len() > 32,
            "the log holds no frames"
        );
    }
    let out = shell(
        &path,
        &[
            "PRAGMA journal_mode;",
            "SELECT count(*) FROM t;",
            "SELECT b FROM t WHERE a=3;",
            "PRAGMA integrity_check;",
        ],
    )
    .expect("the shell runs");
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(lines.first().copied(), Some("wal"));
    assert_eq!(lines.get(1).copied(), Some("3"));
    assert_eq!(lines.get(2).copied(), Some("three"));
    assert_eq!(lines.get(3).copied(), Some("ok"));
}

/// inillucent reads and extends a WAL database SQLite wrote and closed.
///
/// A shell that exits cleanly checkpoints the log into the database and
/// removes it, so what is being tested here is the hand-off: the file is still
/// marked WAL, inillucent opens it in WAL mode without being told to, and what it
/// writes afterwards is readable by SQLite again.
#[test]
fn inillucent_extends_a_wal_database_sqlite_wrote() {
    let Some(_) = pinned_shell() else {
        eprintln!("the pinned shell is not present; skipping");
        return;
    };
    let path = scratch("sqlite-writes");
    shell(
        &path,
        &[
            "PRAGMA journal_mode=wal;",
            "CREATE TABLE t(a INTEGER PRIMARY KEY, b TEXT);",
            "INSERT INTO t VALUES (1, 'one'), (2, 'two');",
        ],
    )
    .expect("the shell runs");
    let bytes = std::fs::read(&path).expect("the database is readable");
    assert_eq!(
        bytes.get(18).copied(),
        Some(2),
        "SQLite did not leave WAL mode on"
    );

    {
        let connection = connect(&path);
        assert_eq!(text(&connection, "PRAGMA journal_mode"), "wal");
        assert_eq!(integer(&connection, "SELECT count(*) FROM t"), 2);
        assert_eq!(text(&connection, "SELECT b FROM t WHERE a=2"), "two");
        connection
            .execute_batch("INSERT INTO t VALUES (3, 'three')")
            .expect("inillucent writes to SQLite's database");
    }

    let out = shell(
        &path,
        &["SELECT count(*) FROM t;", "PRAGMA integrity_check;"],
    )
    .expect("the shell runs");
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(lines.first().copied(), Some("3"));
    assert_eq!(lines.get(1).copied(), Some("ok"));
}

/// inillucent recovers a log a SQLite writer died holding.
///
/// The shell is killed rather than closed, so it never gets to checkpoint: the
/// frames are still in the log and the shared-memory index is whatever SQLite
/// last published into it. Opening that is the strongest interop claim in this
/// file, because it is the case where the two engines have to agree about a
/// structure neither of them wrote for the other.
#[test]
fn inillucent_recovers_a_log_a_killed_sqlite_left() {
    let Some(program) = pinned_shell() else {
        eprintln!("the pinned shell is not present; skipping");
        return;
    };
    let path = scratch("sqlite-killed");
    let mut child = Command::new(program)
        .arg(&path)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .expect("the shell starts");
    {
        use std::io::Write;
        let stdin = child.stdin.as_mut().expect("the shell takes input");
        for statement in [
            "PRAGMA journal_mode=wal;",
            "PRAGMA wal_autocheckpoint=0;",
            "CREATE TABLE t(a INTEGER PRIMARY KEY, b TEXT);",
            "INSERT INTO t VALUES (1, 'one'), (2, 'two');",
            "SELECT 'ready-' || count(*) FROM t;",
        ] {
            writeln!(stdin, "{statement}").expect("the shell accepts input");
        }
        stdin.flush().expect("the input reaches the shell");
    }
    // Waiting for the answer is what makes the kill deterministic: the shell
    // cannot have answered without having written the rows first.
    {
        use std::io::BufRead;
        let stdout = child.stdout.take().expect("the shell gives output");
        let mut reader = std::io::BufReader::new(stdout);
        let mut line = String::new();
        loop {
            line.clear();
            let read = reader.read_line(&mut line).expect("the shell answers");
            assert!(read > 0, "the shell closed before answering");
            if line.starts_with("ready-") {
                assert_eq!(line.trim_end(), "ready-2");
                break;
            }
        }
    }
    child.kill().expect("the shell can be killed");
    let _ = child.wait();

    let log = PathBuf::from(format!("{}-wal", path.display()));
    assert!(log.exists(), "the killed shell left no log");
    assert!(
        std::fs::metadata(&log).expect("the log is readable").len() > 32,
        "the log holds no frames"
    );

    let connection = connect(&path);
    assert_eq!(text(&connection, "PRAGMA journal_mode"), "wal");
    assert_eq!(
        integer(&connection, "SELECT count(*) FROM t"),
        2,
        "the frames SQLite left were not recovered"
    );
    assert_eq!(text(&connection, "SELECT b FROM t WHERE a=2"), "two");
    connection
        .execute_batch("INSERT INTO t VALUES (3, 'three')")
        .expect("inillucent writes to the log SQLite left");
    drop(connection);

    let out = shell(
        &path,
        &["SELECT count(*) FROM t;", "PRAGMA integrity_check;"],
    )
    .expect("the shell runs");
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(lines.first().copied(), Some("3"));
    assert_eq!(lines.get(1).copied(), Some("ok"));
}

/// Each checkpoint mode does the thing its name claims and no more.
///
/// The four modes differ in two axes and it is easy to implement three of them
/// as one: how hard they try to copy the log back, and what they leave the log
/// file looking like afterwards. `PASSIVE` copies what it can and leaves the
/// log where it is. `FULL` and `RESTART` copy everything; `RESTART` also makes
/// the next writer begin again at frame one rather than appending, which is
/// what stops a busy database's log growing without bound. `TRUNCATE` does
/// that and takes the file back to nothing, which is the only one visible in
/// the file system - so the file's size is what this test watches.
#[test]
fn every_checkpoint_mode_does_what_it_says() {
    let path = scratch("modes");
    let connection = connect(&path);
    connection
        .execute_batch("PRAGMA journal_mode=wal")
        .expect("the mode changes");
    connection
        .execute_batch("CREATE TABLE t(a INTEGER PRIMARY KEY, b TEXT)")
        .expect("the table is made");
    let log = std::path::PathBuf::from(format!("{}-wal", path.display()));

    // PASSIVE copies the log back and leaves the file where it is.
    for key in 1..=4 {
        connection
            .execute_batch(&format!("INSERT INTO t VALUES({key}, 'passive')"))
            .expect("a row");
    }
    let grown = log.metadata().expect("the log exists").len();
    assert!(grown > 0, "the log is empty before a checkpoint");
    let (busy, frames, copied) = checkpoint(&connection, "PASSIVE");
    assert_eq!(
        busy, 0,
        "a passive checkpoint with nobody in the way was busy"
    );
    assert_eq!(copied, frames, "a passive checkpoint left frames behind");
    assert_eq!(
        log.metadata().expect("the log exists").len(),
        grown,
        "a passive checkpoint changed the size of the log"
    );

    // FULL copies everything and, like PASSIVE, leaves the file alone.
    connection
        .execute_batch("INSERT INTO t VALUES(5, 'full')")
        .expect("a row");
    let grown = log.metadata().expect("the log exists").len();
    let (busy, frames, copied) = checkpoint(&connection, "FULL");
    assert_eq!(busy, 0, "a full checkpoint with nobody in the way was busy");
    assert_eq!(copied, frames, "a full checkpoint left frames behind");
    assert_eq!(
        log.metadata().expect("the log exists").len(),
        grown,
        "a full checkpoint changed the size of the log"
    );

    // RESTART sends the next writer back to the start of the log, so the file
    // stops growing however many transactions follow.
    connection
        .execute_batch("INSERT INTO t VALUES(6, 'restart')")
        .expect("a row");
    let grown = log.metadata().expect("the log exists").len();
    let (busy, frames, copied) = checkpoint(&connection, "RESTART");
    assert_eq!(
        busy, 0,
        "a restart checkpoint with nobody in the way was busy"
    );
    assert_eq!(copied, frames, "a restart checkpoint left frames behind");
    for key in 7..=10 {
        connection
            .execute_batch(&format!("INSERT INTO t VALUES({key}, 'after')"))
            .expect("a row");
    }
    assert!(
        log.metadata().expect("the log exists").len() <= grown,
        "the log grew past its restart point"
    );

    // TRUNCATE is the one a file listing can see.
    let (busy, frames, copied) = checkpoint(&connection, "TRUNCATE");
    assert_eq!(busy, 0, "a truncating checkpoint was busy");
    assert_eq!(copied, frames, "a truncating checkpoint left frames behind");
    assert_eq!(
        log.metadata().expect("the log exists").len(),
        0,
        "TRUNCATE left bytes in the log"
    );

    // Every row is in the database after all of that, and the pinned build
    // agrees - which is the point of copying frames back at all.
    assert_eq!(integer(&connection, "SELECT count(*) FROM t"), 10);
    drop(connection);
    if let Some(reported) = shell(
        &path,
        &["SELECT count(*) FROM t;", "PRAGMA integrity_check;"],
    ) {
        let reported: Vec<&str> = reported.lines().map(str::trim).collect();
        assert_eq!(reported, ["10", "ok"], "SQLite disagreed");
    }
}

/// A checkpoint that cannot finish says so rather than pretending.
///
/// `FULL` and `RESTART` have to wait for every reader to leave before they can
/// promise the log is fully copied. A reader that is not going anywhere is
/// therefore reported as busy, and the frames it protects stay in the log.
#[test]
fn a_full_checkpoint_reports_a_reader_it_cannot_wait_out() {
    let path = scratch("modes-busy");
    let writer = connect(&path);
    writer
        .execute_batch("PRAGMA journal_mode=wal")
        .expect("the mode changes");
    writer
        .execute_batch("CREATE TABLE t(a INTEGER PRIMARY KEY)")
        .expect("the table is made");
    writer
        .execute_batch("INSERT INTO t VALUES(1)")
        .expect("a row");

    let reader = connect(&path);
    reader.execute_batch("BEGIN").expect("the read opens");
    assert_eq!(integer(&reader, "SELECT count(*) FROM t"), 1);
    writer
        .execute_batch("INSERT INTO t VALUES(2)")
        .expect("a row");

    let (busy, frames, copied) = checkpoint(&writer, "FULL");
    assert_eq!(busy, 1, "a full checkpoint ignored a reader in its way");
    assert!(
        copied < frames,
        "a full checkpoint copied {copied} of {frames} frames past a live reader"
    );
    reader.execute_batch("COMMIT").expect("the read closes");

    let (busy, frames, copied) = checkpoint(&writer, "RESTART");
    assert_eq!(
        busy, 0,
        "the reader has gone and the checkpoint was still busy"
    );
    assert_eq!(copied, frames, "the log was not fully copied back");
}

/// Runs a checkpoint and returns what it reported.
fn checkpoint(connection: &inillucent::Connection, mode: &str) -> (i64, i64, i64) {
    let rows = connection
        .query(&format!("PRAGMA wal_checkpoint({mode})"))
        .expect("the checkpoint runs");
    let row = rows.first().expect("the checkpoint reports a row");
    let field = |index: usize| row.get(index).and_then(Value::as_integer).unwrap_or(-1);
    (field(0), field(1), field(2))
}

/// SQLite and inillucent share one log *at the same time*, through the same
/// shared-memory index.
///
/// Every other interop test here hands the files over: one engine closes, the
/// other opens. This one does not. A inillucent connection stays open, holding the
/// database, the log and the wal-index, while the pinned SQLite build opens the
/// same three files and writes through them - and then each engine reads what
/// the other did.
///
/// It is the test the shared-memory index exists for, and it is the one that
/// would fail if the index were merely a file this engine read and wrote rather
/// than memory both engines map. It would also fail if the dead-man switch were
/// taken on the wrong byte: SQLite holds a shared lock on byte 128 for as long
/// as it has the file mapped, and an engine that thought it was alone would
/// throw away the index SQLite was using.
#[test]
fn sqlite_and_inillucent_share_one_log_at_the_same_time() {
    let Some(_) = pinned_shell() else {
        eprintln!("skipping: the pinned SQLite shell is not present");
        return;
    };
    let path = scratch("shared-log");
    let connection = connect(&path);
    connection
        .execute_batch("PRAGMA journal_mode=wal")
        .expect("the mode changes");
    connection
        .execute_batch("CREATE TABLE t(a INTEGER PRIMARY KEY, who TEXT)")
        .expect("the table is made");
    connection
        .execute_batch("INSERT INTO t VALUES(1, 'inillucent')")
        .expect("a row");

    // The connection is still open, and deliberately still holding a read, so
    // the log and the index are live rather than tidied away.
    assert_eq!(integer(&connection, "SELECT count(*) FROM t"), 1);

    // SQLite opens the same three files and appends to the same log.
    let reported = shell(
        &path,
        &[
            "PRAGMA journal_mode;",
            "INSERT INTO t VALUES(2, 'sqlite');",
            "SELECT group_concat(who) FROM t ORDER BY a;",
        ],
    )
    .expect("the shell runs");
    let lines: Vec<&str> = reported.lines().map(str::trim).collect();
    assert_eq!(
        lines.first().copied(),
        Some("wal"),
        "SQLite did not open the database in WAL mode: {reported:?}"
    );
    assert!(
        lines.contains(&"inillucent,sqlite"),
        "SQLite could not see the row inillucent committed: {reported:?}"
    );

    // And back: this connection, which never closed, sees SQLite's row.
    assert_eq!(
        integer(&connection, "SELECT count(*) FROM t"),
        2,
        "inillucent could not see the row SQLite committed through the shared log"
    );
    assert_eq!(text(&connection, "SELECT who FROM t WHERE a = 2"), "sqlite");

    // A checkpoint from this side copies both engines' frames back, and SQLite
    // reads the result.
    connection
        .execute_batch("INSERT INTO t VALUES(3, 'inillucent again')")
        .expect("a row");
    let rows = connection
        .query("PRAGMA wal_checkpoint(TRUNCATE)")
        .expect("the checkpoint runs");
    let row = rows.first().expect("the checkpoint reports a row");
    assert_eq!(
        row.first().and_then(Value::as_integer),
        Some(0),
        "the checkpoint was busy with nobody else holding the log"
    );
    drop(connection);
    let reported = shell(
        &path,
        &["SELECT count(*) FROM t;", "PRAGMA integrity_check;"],
    )
    .expect("the shell runs");
    let lines: Vec<&str> = reported.lines().map(str::trim).collect();
    assert_eq!(lines, ["3", "ok"], "SQLite read {reported:?}");
}
