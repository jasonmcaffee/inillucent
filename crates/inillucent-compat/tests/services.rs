//! Backup, incremental blob access, and serialize/deserialize.
//!
//! Invariant: every one of these is judged by what SQLite makes of its output.
//! A backup only this engine can open is not a backup, and a blob written a
//! range at a time has to leave a row every reader agrees about - so the
//! results are handed to the pinned build and read back through it.

use std::path::{Path, PathBuf};
use std::process::Command;

use inillucent_compat::workspace_root;
use inillucent_legacy::{Database, Value};

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

/// Returns a fresh scratch path.
fn scratch(name: &str) -> PathBuf {
    let directory = workspace_root().join("_agent_output/services");
    let _ = std::fs::create_dir_all(&directory);
    for suffix in ["", "-journal", "-wal", "-shm"] {
        let _ = std::fs::remove_file(directory.join(format!("{name}.db{suffix}")));
    }
    directory.join(format!("{name}.db"))
}

/// Runs a script in the pinned shell.
fn shell(path: &Path, script: &str) -> Option<String> {
    let program = pinned_shell()?;
    let output = Command::new(program).arg(path).arg(script).output().ok()?;
    let mut text = String::from_utf8_lossy(&output.stdout).to_string();
    text.push_str(&String::from_utf8_lossy(&output.stderr));
    Some(text)
}

/// Returns a database's bytes with its version markers blanked.
///
/// Everything a page holds is compared, and the two numbers that describe the
/// *file's* history rather than its contents are not: the change counter and
/// the version it was last valid for. A copy has to differ in those - they are
/// how another connection learns its cache is stale, and a copy whose counter
/// matched the original's would be a file readers believed they had already
/// seen.
fn pages(bytes: &[u8]) -> Vec<u8> {
    let mut copy = bytes.to_vec();
    for range in [24..28usize, 92..96] {
        if let Some(window) = copy.get_mut(range) {
            window.fill(0);
        }
    }
    copy
}

/// Returns the first column of the first row a query reports.
fn one(connection: &inillucent_legacy::Connection, sql: &str) -> Option<Value<'static>> {
    connection
        .query(sql)
        .ok()
        .and_then(|rows| rows.first().and_then(|row| row.first()).cloned())
}

/// A backup reproduces the file, and SQLite reads what comes out.
///
/// Page for page rather than rebuilt: a backup is not a `VACUUM`, so the two
/// files are compared byte for byte rather than row for row.
#[test]
fn a_backup_reproduces_the_database() {
    let source_path = scratch("backup-source");
    let destination_path = scratch("backup-destination");
    {
        let source = Database::open(&source_path).expect("the source opens");
        let connection = source.connect().expect("the connection opens");
        connection
            .execute_batch(
                "CREATE TABLE t(a INTEGER PRIMARY KEY, b TEXT);
                 CREATE INDEX t_b ON t(b);
                 INSERT INTO t VALUES (1,'one'),(2,'two'),(3,'three');",
            )
            .expect("the rows are written");
        let destination = Database::open(&destination_path).expect("the destination opens");
        let target = destination.connect().expect("the connection opens");
        connection.backup_into(&target).expect("the backup runs");
    }
    let source_bytes = std::fs::read(&source_path).expect("the source is readable");
    let copy_bytes = std::fs::read(&destination_path).expect("the copy is readable");
    assert_eq!(
        pages(&source_bytes),
        pages(&copy_bytes),
        "the backup is not a copy of the file"
    );
    let Some(out) = shell(
        &destination_path,
        "PRAGMA integrity_check; SELECT b FROM t ORDER BY a;",
    ) else {
        eprintln!("the pinned shell is not present; skipping the read-back");
        return;
    };
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(lines.first().copied(), Some("ok"));
    assert_eq!(lines.get(1).copied(), Some("one"));
}

/// A blob is read a range at a time and gives the bytes the row holds.
#[test]
fn a_blob_reads_ranges() {
    let path = scratch("blob-read");
    let database = Database::open(&path).expect("the database opens");
    let connection = database.connect().expect("the connection opens");
    connection
        .execute_batch("CREATE TABLE t(a INTEGER PRIMARY KEY, b BLOB)")
        .expect("the table is created");
    let payload: Vec<u8> = (0..20_000u32).map(|index| (index % 251) as u8).collect();
    let mut statement = connection
        .prepare("INSERT INTO t VALUES (1, ?)")
        .expect("the insert prepares");
    statement.bind_blob(1, &payload).expect("the blob binds");
    while statement.step().expect("the insert steps") {}
    drop(statement);

    let blob = connection
        .blob_open("main", "t", "b", 1, false)
        .expect("the blob opens");
    assert_eq!(blob.len() as usize, payload.len());
    for (offset, len) in [(0usize, 10usize), (4000, 4096), (19_990, 10), (8191, 2)] {
        let mut window = vec![0u8; len];
        blob.read_at(offset as u32, &mut window)
            .expect("the range reads");
        assert_eq!(
            window,
            payload
                .get(offset..offset.saturating_add(len))
                .unwrap_or_default(),
            "bytes at {offset} came back wrong"
        );
    }
    let mut past = [0u8; 4];
    assert!(
        blob.read_at(blob.len().saturating_sub(2), &mut past)
            .is_err(),
        "a read past the end was allowed"
    );
}

/// A blob write changes those bytes and nothing else, and every reader agrees.
#[test]
fn a_blob_writes_ranges_in_place() {
    let path = scratch("blob-write");
    let mut expected: Vec<u8> = (0..30_000u32).map(|index| (index % 241) as u8).collect();
    {
        let database = Database::open(&path).expect("the database opens");
        let connection = database.connect().expect("the connection opens");
        connection
            .execute_batch("CREATE TABLE t(a INTEGER PRIMARY KEY, b BLOB)")
            .expect("the table is created");
        let mut statement = connection
            .prepare("INSERT INTO t VALUES (1, ?)")
            .expect("the insert prepares");
        statement.bind_blob(1, &expected).expect("the blob binds");
        while statement.step().expect("the insert steps") {}
        drop(statement);

        let blob = connection
            .blob_open("main", "t", "b", 1, true)
            .expect("the blob opens");
        for (offset, fill) in [(0usize, 0xa1u8), (12_345, 0xb2), (29_990, 0xc3)] {
            let patch = vec![fill; 10];
            blob.write_at(offset as u32, &patch)
                .expect("the range writes");
            if let Some(window) = expected.get_mut(offset..offset.saturating_add(patch.len())) {
                window.copy_from_slice(&patch);
            }
        }
        let mut readback = vec![0u8; expected.len()];
        blob.read_at(0, &mut readback).expect("the value reads");
        assert_eq!(readback, expected, "the writes did not land");
    }
    let database = Database::open(&path).expect("the database reopens");
    let connection = database.connect().expect("the connection opens");
    let stored = one(&connection, "SELECT b FROM t WHERE a = 1")
        .and_then(|value| match value {
            Value::Blob(bytes) => Some(bytes.raw().to_vec()),
            _ => None,
        })
        .expect("the row reads back");
    assert_eq!(stored, expected, "the query and the blob handle disagree");
    drop(connection);
    drop(database);

    let Some(out) = shell(
        &path,
        "PRAGMA integrity_check; SELECT length(b) || '|' || hex(substr(b,1,4)) FROM t;",
    ) else {
        eprintln!("the pinned shell is not present; skipping the read-back");
        return;
    };
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(lines.first().copied(), Some("ok"));
    assert_eq!(lines.get(1).copied(), Some("30000|A1A1A1A1"));
}

/// A read-only handle refuses to write.
#[test]
fn a_read_only_blob_refuses_a_write() {
    let path = scratch("blob-readonly");
    let database = Database::open(&path).expect("the database opens");
    let connection = database.connect().expect("the connection opens");
    connection
        .execute_batch(
            "CREATE TABLE t(a INTEGER PRIMARY KEY, b BLOB);
             INSERT INTO t VALUES (1, x'0011223344');",
        )
        .expect("the row is written");
    let blob = connection
        .blob_open("main", "t", "b", 1, false)
        .expect("the blob opens");
    assert!(blob.write_at(0, &[0xff]).is_err());
}

/// A handle whose row has gone reports it rather than reading whatever is
/// there now.
#[test]
fn a_blob_on_a_deleted_row_is_refused() {
    let path = scratch("blob-deleted");
    let database = Database::open(&path).expect("the database opens");
    let connection = database.connect().expect("the connection opens");
    connection
        .execute_batch(
            "CREATE TABLE t(a INTEGER PRIMARY KEY, b BLOB);
             INSERT INTO t VALUES (1, x'0011223344');",
        )
        .expect("the row is written");
    let blob = connection
        .blob_open("main", "t", "b", 1, false)
        .expect("the blob opens");
    connection
        .execute_batch("DELETE FROM t WHERE a = 1")
        .expect("the row is deleted");
    let mut window = [0u8; 4];
    assert!(blob.read_at(0, &mut window).is_err());
}

/// Serialising produces the file, and SQLite opens it.
#[test]
fn a_database_serialises_to_bytes_sqlite_can_read() {
    let path = scratch("serialize");
    let bytes = {
        let database = Database::open(&path).expect("the database opens");
        let connection = database.connect().expect("the connection opens");
        connection
            .execute_batch(
                "CREATE TABLE t(a INTEGER PRIMARY KEY, b TEXT);
                 INSERT INTO t VALUES (1,'one'),(2,'two');",
            )
            .expect("the rows are written");
        connection.serialize().expect("the database serialises")
    };
    assert_eq!(bytes.len() % 4096, 0);
    let written = scratch("serialize-written");
    std::fs::write(&written, &bytes).expect("the bytes are writable");
    let Some(out) = shell(
        &written,
        "PRAGMA integrity_check; SELECT b FROM t ORDER BY a;",
    ) else {
        eprintln!("the pinned shell is not present; skipping the read-back");
        return;
    };
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(lines.first().copied(), Some("ok"));
    assert_eq!(lines.get(1).copied(), Some("one"));
    assert_eq!(lines.get(2).copied(), Some("two"));
}

/// Bytes SQLite wrote deserialise into a database this engine queries.
#[test]
fn bytes_sqlite_wrote_deserialise() {
    let path = scratch("deserialize");
    let Some(_) = shell(
        &path,
        "CREATE TABLE t(a INTEGER PRIMARY KEY, b TEXT); INSERT INTO t VALUES (1,'from sqlite');",
    ) else {
        eprintln!("the pinned shell is not present; skipping");
        return;
    };
    let bytes = std::fs::read(&path).expect("the database is readable");
    let opened = Database::deserialize(&bytes).expect("the bytes open");
    let connection = opened.connect().expect("the connection opens");
    let value = one(&connection, "SELECT b FROM t WHERE a = 1");
    assert_eq!(
        value.and_then(|value| match value {
            Value::Text(text) => Some(String::from_utf8_lossy(&text.utf8_bytes()).to_string()),
            _ => None,
        }),
        Some("from sqlite".to_string())
    );
}

/// A deserialised database is writable, and serialising it again round-trips.
#[test]
fn a_deserialised_database_can_be_written_and_serialised_again() {
    let path = scratch("deserialize-write");
    let bytes = {
        let database = Database::open(&path).expect("the database opens");
        let connection = database.connect().expect("the connection opens");
        connection
            .execute_batch("CREATE TABLE t(a INTEGER PRIMARY KEY); INSERT INTO t VALUES (1)")
            .expect("the row is written");
        connection.serialize().expect("it serialises")
    };
    let opened = Database::deserialize(&bytes).expect("the bytes open");
    let connection = opened.connect().expect("the connection opens");
    connection
        .execute_batch("INSERT INTO t VALUES (2)")
        .expect("the copy is writable");
    let again = connection.serialize().expect("it serialises again");
    assert_ne!(bytes, again, "a write did not change the bytes");

    let written = scratch("deserialize-written");
    std::fs::write(&written, &again).expect("the bytes are writable");
    let Some(out) = shell(&written, "SELECT count(*) FROM t;") else {
        eprintln!("the pinned shell is not present; skipping the read-back");
        return;
    };
    assert_eq!(out.lines().next(), Some("2"));
}
