//! `ANALYZE`, then reopened with the log unfolded.
//!
//! Invariant: **every tree the log names has a shape the replay can be told.**
//! The same one `autoindex_reopen.rs` carries. Recovery refuses a record naming a tree it was not given the shape of, which
//! is the right refusal - replaying into a guessed shape corrupts a file quietly - so the shapes have
//! to be derivable for *every* tree, not for most of them.
//!
//! **`ANALYZE` breaks that invariant on a real database, and these tests do not yet reproduce it.**
//! They are written down as the search so far, not as a reproduction, and they are honest about
//! which of the two they are.
//!
//! What the failure looks like, found on task-1876 against Nikaya's 6.9 GB mail database:
//!
//! ```text
//! > inillucent --db nikaya.rdb analyze
//! ok. sqlite_stat1 is up to date.
//! > inillucent --db nikaya.rdb query "SELECT count(*) FROM document"
//! Error [io]: could not open "nikaya.rdb": bad parameter or other API misuse:
//!            the log names tree 2147483712, which this recovery was not told the shape of
//! ```
//!
//! Isolated by reverting the variable rather than by matching a symptom: on the *same file*, an
//! ordinary `UPDATE` writes and reopens fine, and `ANALYZE` immediately afterwards leaves it
//! unopenable. Deterministic, about seven seconds, one command. `2147483712` is `0x8000_0040` - a
//! **provisional** root, `0x8000_0000` plus a counter, which is how a tree created inside the
//! transaction being replayed is named before it has a page. The catalog the shape derivation reads
//! holds real page numbers, so nothing there can place it.
//!
//! It matters more than a missing statistic: the failure is *silent at write time* - `ANALYZE`
//! prints `ok` - and appears only at the next open, which for a service is the next restart.
//!
//! What these three cover, and it is the class rather than the case: `ANALYZE` with an unfolded log
//! at one object, at sixty-odd objects (so the provisional counter reaches `0x8000_0040`, the exact
//! number the real failure reported), and twice over so the second run writes into a `sqlite_stat1`
//! the catalog already holds. All three pass, and they are kept because they pin what `ANALYZE`
//! itself must keep doing.
//!
//! ## What the 6.9 GB database had that a fresh one does not (task-1880 §20)
//!
//! **A page carrying an LSN from a log stream that no longer exists**, and `ANALYZE` is the
//! messenger rather than the cause. Reproduced against a copy of the parked file at
//! `J:/nikaya-data/wal-parked-task1876/nikaya.rdb.after-checkpoint-recovery`: copy it, run
//! `analyze`, reopen. Nine seconds, deterministic, and the same message.
//!
//! What the file says, read out of its own bytes:
//!
//! ```text
//! meta checkpoint_lsn 21,074,969,552      the position the log resumes at
//! page 3, the catalog leaf, is stamped 21,939,058,496
//! of the first 3,000 pages, 9 carry a stamp above the whole log's end (21,075,008,400)
//! ```
//!
//! Page 3 is stamped 864 million positions **beyond** the end of the log beside it. Recovery's
//! page-LSN rule - apply a record only when the page's stamp is below it - therefore skips every
//! record for that page, because the stamp says the page already has them. It does not: the stamp
//! is a position in an earlier stream that was abandoned when 24 segments were moved aside to
//! recover the file.
//!
//! So `ANALYZE` wrote `sqlite_stat1`'s catalog row into page 3, the row went to the log, the reopen
//! skipped that record, and the row was gone. A trace in `LearningRows` confirms it directly: the
//! applier is built from 62 checkpointed entries knowing trees `0x8000_0000..=0x8000_003D`, and
//! `learn` is **never called** for the catalog row that would have added `0x8000_003E`. The shape
//! derivation then has nothing to register the tree under and refuses - which is the message, and it
//! is the *second* thing that went wrong. The first is that a committed row was discarded silently.
//!
//! It is not specific to `ANALYZE` and not specific to the catalog. Any write to a page stamped
//! above the log's resumed position is discarded at the next open, and `PRAGMA integrity_check`
//! answers `ok` about the result, because the file really is structurally intact - it is missing a
//! row nothing can see was lost.
//!
//! **What is not here is a reduction.** Building and checkpointing, stealing pages with a 64-frame
//! pool, truncating the log and reopening does *not* reproduce it: the reopened write survives,
//! because the page it lands on reaches the file again before the next open and the log is never
//! asked. The distance matters and the reduction has not found what sets it. The recipe above needs
//! only the parked file, which is preserved, so nobody has to guess.
//!
//! The tests abandon the connection the way the crash tests do, so the log is still there to replay.
//! A tidy close checkpoints, which folds the log into the file and never reaches the code this is
//! about.

use std::path::{Path, PathBuf};

use inillucent_compat::facade::Database;
use inillucent_compat::workspace_root;
use inillucent_value::Value;

/// Returns a scratch directory for one scenario.
///
/// @param name - what to name it after
fn scratch(name: &str) -> PathBuf {
    let directory = workspace_root()
        .join("_agent_output/task-1880/analyze-reopen")
        .join(name);
    let _ = std::fs::remove_dir_all(&directory);
    let _ = std::fs::create_dir_all(&directory);
    directory
}

/// Runs statements and abandons the connection with its log unfolded.
///
/// @param path - the database file
/// @param sql - the statements to run
fn write_and_abandon(path: &Path, sql: &str) {
    let database = Database::open(path).expect("the database opens");
    let connection = database.connect().expect("the connection opens");
    connection
        .execute_batch(&format!("{sql}\nPRAGMA locking_mode = NORMAL;"))
        .expect("the statements run");
    std::mem::forget(connection);
    std::mem::forget(database);
}

/// Returns the rows a query answers over a freshly opened database.
///
/// @param path - the database file
/// @param sql - the query
fn read_back(path: &Path, sql: &str) -> Vec<Vec<Value<'static>>> {
    let database = Database::open(path).expect("the database reopens");
    let connection = database.connect().expect("the connection opens");
    connection.query(sql).expect("the query runs")
}

/// The smallest form: one table, one index, `ANALYZE`, reopen.
#[test]
fn analyze_survives_a_reopen_with_the_log_unfolded() {
    let directory = scratch("one-table");
    let path = directory.join("stats.db");
    write_and_abandon(
        &path,
        "CREATE TABLE t(a TEXT, b INT);\n\
         CREATE INDEX t_a ON t(a);\n\
         INSERT INTO t VALUES ('x', 1), ('y', 2), ('z', 3);\n\
         ANALYZE;",
    );
    let rows = read_back(&path, "SELECT count(*) FROM sqlite_stat1");
    assert!(
        rows.first()
            .and_then(|row| row.first())
            .and_then(Value::as_integer)
            .unwrap_or(0)
            > 0,
        "ANALYZE wrote no statistics that survived the reopen"
    );
}

/// The shape the real database had: enough objects that the provisional root is well past the first.
///
/// The counter that names a created tree starts at `0x8000_0000` and advances per object, so a
/// database with sixty-odd tables and indexes reaches `0x8000_0040` - which is the number the
/// original failure reported, and a test that only ever allocates the first provisional root would
/// pass while the bug stood.
#[test]
fn analyze_survives_a_reopen_on_a_schema_of_many_objects() {
    let directory = scratch("many-objects");
    let path = directory.join("stats.db");
    let mut sql = String::new();
    for table in 0..24 {
        sql.push_str(&format!(
            "CREATE TABLE t{table}(id TEXT PRIMARY KEY, a TEXT, b INT);\n"
        ));
        sql.push_str(&format!("CREATE INDEX t{table}_a ON t{table}(a);\n"));
        sql.push_str(&format!(
            "INSERT INTO t{table} VALUES ('x', 'p', 1), ('y', 'q', 2);\n"
        ));
    }
    sql.push_str("ANALYZE;");
    write_and_abandon(&path, &sql);
    let rows = read_back(&path, "SELECT count(*) FROM sqlite_stat1");
    assert!(
        rows.first()
            .and_then(|row| row.first())
            .and_then(Value::as_integer)
            .unwrap_or(0)
            > 0,
        "ANALYZE wrote no statistics that survived the reopen"
    );
    // And the tables it measured still read, which is what says the replay applied rather than that
    // it merely did not refuse.
    let found = read_back(&path, "SELECT b FROM t23 WHERE id = 'y'");
    assert_eq!(found.len(), 1, "the corpus did not survive the replay");
}

/// `ANALYZE` twice, so the second run writes into a `sqlite_stat1` the catalog already holds.
#[test]
fn a_second_analyze_survives_a_reopen() {
    let directory = scratch("twice");
    let path = directory.join("stats.db");
    write_and_abandon(
        &path,
        "CREATE TABLE t(a TEXT, b INT);\n\
         CREATE INDEX t_a ON t(a);\n\
         INSERT INTO t VALUES ('x', 1), ('y', 2);\n\
         ANALYZE;",
    );
    let _ = read_back(&path, "SELECT count(*) FROM sqlite_stat1");
    write_and_abandon(&path, "INSERT INTO t VALUES ('z', 3);\nANALYZE;");
    let rows = read_back(&path, "SELECT count(*) FROM t");
    assert_eq!(
        rows.first()
            .and_then(|row| row.first())
            .and_then(Value::as_integer),
        Some(3)
    );
}

/// The real case's shape: the schema already checkpointed, and `ANALYZE` alone in the log.
///
/// This is what Nikaya's database was. Every table and index had real page roots written long
/// before, so the only tree `ANALYZE` creates is `sqlite_stat1`, and it is the only object in the
/// log whose root is provisional. The earlier tests all create the schema and analyse it in the
/// same breath, which means the catalog rows the shape derivation needs are in the same log - a
/// different situation, and the reason they pass.
#[test]
fn analyze_alone_in_the_log_survives_a_reopen() {
    let directory = scratch("checkpointed-schema");
    let path = directory.join("stats.db");
    // Built and closed tidily, so this half is folded into the file and out of the log.
    {
        let database = Database::open(&path).expect("the database opens");
        let connection = database.connect().expect("the connection opens");
        let mut sql = String::new();
        for table in 0..24 {
            sql.push_str(&format!(
                "CREATE TABLE t{table}(id TEXT PRIMARY KEY, a TEXT, b INT);
"
            ));
            sql.push_str(&format!(
                "CREATE INDEX t{table}_a ON t{table}(a);
"
            ));
            sql.push_str(&format!(
                "INSERT INTO t{table} VALUES ('x', 'p', 1), ('y', 'q', 2);
"
            ));
        }
        connection.execute_batch(&sql).expect("the schema is built");
    }
    write_and_abandon(&path, "ANALYZE;");
    let rows = read_back(&path, "SELECT count(*) FROM sqlite_stat1");
    assert!(
        rows.first()
            .and_then(|row| row.first())
            .and_then(Value::as_integer)
            .unwrap_or(0)
            > 0,
        "ANALYZE wrote no statistics that survived the reopen"
    );
    let found = read_back(&path, "SELECT b FROM t23 WHERE id = 'y'");
    assert_eq!(found.len(), 1, "the corpus did not survive the replay");
}

/// The same, with an `AUTOINCREMENT` table so `sqlite_sequence` exists.
///
/// The real database has one - `sqlite_sequence`, root 23 - and none of the tests above did. An
/// internal table is exactly the kind of catalog entry a shape derivation is likely to skip, and
/// `ANALYZE` measures every table including that one.
#[test]
fn analyze_survives_a_reopen_with_an_autoincrement_table() {
    let directory = scratch("autoincrement");
    let path = directory.join("stats.db");
    {
        let database = Database::open(&path).expect("the database opens");
        let connection = database.connect().expect("the connection opens");
        let mut sql = String::from(
            "CREATE TABLE job(id INTEGER PRIMARY KEY AUTOINCREMENT, kind TEXT);
             INSERT INTO job(kind) VALUES ('one'), ('two');
",
        );
        for table in 0..24 {
            sql.push_str(&format!(
                "CREATE TABLE t{table}(id TEXT PRIMARY KEY, a TEXT, b INT);
"
            ));
            sql.push_str(&format!(
                "CREATE INDEX t{table}_a ON t{table}(a);
"
            ));
            sql.push_str(&format!(
                "INSERT INTO t{table} VALUES ('x', 'p', 1), ('y', 'q', 2);
"
            ));
        }
        connection.execute_batch(&sql).expect("the schema is built");
    }
    write_and_abandon(&path, "ANALYZE;");
    let rows = read_back(&path, "SELECT count(*) FROM sqlite_stat1");
    assert!(
        rows.first()
            .and_then(|row| row.first())
            .and_then(Value::as_integer)
            .unwrap_or(0)
            > 0,
        "ANALYZE wrote no statistics that survived the reopen"
    );
    let found = read_back(&path, "SELECT kind FROM job WHERE id = 2");
    assert_eq!(found.len(), 1, "the corpus did not survive the replay");
}
