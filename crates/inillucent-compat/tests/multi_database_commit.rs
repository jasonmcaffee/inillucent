//! What decides a commit that spans two databases.
//!
//! Invariant: a transaction that wrote two files is committed by the deletion of
//! one file, and by nothing else. Each participant's log carries a `Commit`
//! record and neither of those records is the decision - so while the
//! super-journal is still there, *neither* database replays the transaction, and
//! once it is gone, *both* do. There is no third outcome, because there is
//! nothing to observe between "the file is there" and "the file is gone".
//!
//! ## Why this is the shape of the test
//!
//! `inillucent-compat`'s older `multi_database_crash.rs` drives the *old* engine
//! through `inillucent-sim`'s failpoints, cutting the commit at every injectable
//! VFS call. The new engine opens its files through `OsVfs` directly and takes
//! no VFS from its caller, so that campaign cannot be pointed at it without
//! threading a file system through `connect::Database::open` - which is a change
//! to the engine's surface rather than to its tests, and is not this ticket's.
//!
//! What is testable without that is the thing the protocol actually claims: the
//! decision rule. Both databases are written by one transaction, the on-disk
//! state a crash *before* the commit point would have left is reconstructed with
//! the engine's own `SuperJournal`, and both files are reopened. Then the
//! super-journal is removed - which is exactly what the commit point does - and
//! they are reopened again. The two answers are the two halves of the invariant.

use std::path::{Path, PathBuf};

use inillucent_compat::facade::Database;
use inillucent_compat::workspace_root;
use inillucent_engine::multi::{marker_path, SuperJournal};
use inillucent_value::Value;

/// How many transaction numbers a marker is written for.
///
/// The engine numbers a connection's transactions from one and does not report
/// which number a `COMMIT` took, so the marker names every number the script
/// below could plausibly have reached. That is what a super-journal written by
/// the engine itself holds for one transaction; naming a few more suppresses a
/// few transactions that never existed, which changes nothing about what is
/// being asserted and keeps the test from depending on a counter it cannot see.
const NUMBERED: u64 = 64;

/// Returns a scratch directory for one scenario, emptied first.
fn scratch(name: &str) -> PathBuf {
    let directory = workspace_root()
        .join("_agent_output/multi-commit")
        .join(name);
    let _ = std::fs::remove_dir_all(&directory);
    let _ = std::fs::create_dir_all(&directory);
    directory
}

/// Builds the two tables and folds them into the two files.
///
/// **Checkpointed and closed tidily, so that the schema is in the files rather
/// than in the logs.** What the test suppresses afterwards is everything the
/// logs still hold, and a `CREATE TABLE` left there would be suppressed with the
/// insert - which would make the assertion "the tables are gone" rather than
/// "the transaction did not happen".
///
/// @param main - the database the connection is opened on
/// @param aux - the database it attaches
fn prepare(main: &Path, aux: &Path) {
    let database = Database::open(main).expect("the database opens");
    let connection = database.connect().expect("the connection opens");
    connection
        .execute_batch(&format!(
            "CREATE TABLE t(a);
             ATTACH DATABASE '{}' AS aux;
             CREATE TABLE aux.t(a);",
            aux.display().to_string().replace('\\', "/")
        ))
        .expect("both schemas are written");
    database.checkpoint().expect("both files are folded down");
}

/// Commits one transaction across the two databases and abandons the process.
///
/// @param main - the database the connection is opened on
/// @param aux - the database it attaches
fn write_across(main: &Path, aux: &Path) {
    let database = Database::open(main).expect("the database reopens");
    let connection = database.connect().expect("the connection opens");
    connection
        .execute_batch(&format!(
            "ATTACH DATABASE '{}' AS aux;
             BEGIN;
             INSERT INTO t VALUES (1);
             INSERT INTO aux.t VALUES (2);
             COMMIT;
             PRAGMA locking_mode = NORMAL;",
            aux.display().to_string().replace('\\', "/")
        ))
        .expect("the transaction commits");
    // **Abandoned rather than closed, which is what a crash is.** Dropping the
    // handle folds the pools' dirty pages into the two files, and a page already
    // in the file is a page recovery has no say over - so a test that closed
    // tidily would be asserting about a decision something else had taken.
    //
    // The `locking_mode = NORMAL` above is the other half of the same
    // simulation, and it is there because the engine now takes real file locks.
    // A crashing process has its locks released by the operating system, which
    // is what that pragma does here - it lets the file go without checkpointing,
    // so the pages stay dirty and unwritten. Forgetting the handle instead would
    // hold the default `exclusive` lock for the rest of the process, and the
    // reopen below would be told the file was busy rather than being allowed to
    // recover it.
    std::mem::forget(connection);
    std::mem::forget(database);
}

/// Returns how many rows each database holds, reopening both.
///
/// @param main - the database to open
/// @param aux - the database to attach to it
fn counts(main: &Path, aux: &Path) -> Vec<i64> {
    let database = Database::open(main).expect("the database reopens");
    let connection = database.connect().expect("the connection opens");
    connection
        .execute_batch(&format!(
            "ATTACH DATABASE '{}' AS aux",
            aux.display().to_string().replace('\\', "/")
        ))
        .expect("the second database attaches");
    connection
        .query("SELECT (SELECT count(*) FROM main.t), (SELECT count(*) FROM aux.t)")
        .expect("the query runs")
        .first()
        .map(|row| row.iter().filter_map(Value::as_integer).collect())
        .unwrap_or_default()
}

/// Reconstructs the on-disk state a crash before the commit point would leave.
///
/// Written with the engine's own `SuperJournal`, so the file names, the marker
/// format and the order are the ones the commit path produces rather than a
/// second description of them that could drift.
///
/// @param main - the coordinator, which the super-journal sits beside
/// @param aux - the other participant
fn leave_it_undecided(main: &Path, aux: &Path) -> PathBuf {
    let journal = SuperJournal::create(main, 1, &[main.to_path_buf(), aux.to_path_buf()])
        .expect("the super-journal is written");
    // Deliberately neither committed nor abandoned: the file staying where it is
    // *is* the state being tested, and `SuperJournal` has no destructor so
    // dropping the handle leaves it exactly there.
    drop(journal);
    let mut name = main.as_os_str().to_os_string();
    name.push(format!("-mj{:08x}", 1u64));
    let path = PathBuf::from(name);
    // **The markers are written here rather than through `SuperJournal::mark`,
    // and only because of what this test cannot see.** `mark` writes the one
    // transaction a commit is about; the engine does not report which number a
    // `COMMIT` took, so the marker is written with every number the script above
    // could have reached. The line format is `mark`'s own, one `txn`, a tab, and
    // the super-journal's path.
    let mut lines = String::new();
    for txn in 1..=NUMBERED {
        lines.push_str(&format!(
            "{txn}	{}
",
            path.display()
        ));
    }
    for participant in [main, aux] {
        std::fs::write(marker_path(participant), lines.as_bytes()).expect("the marker is written");
    }
    path
}

/// While the super-journal is there, neither database carries the transaction.
#[test]
fn an_undecided_commit_is_replayed_by_neither_database() {
    let directory = scratch("undecided");
    let main = directory.join("main.db");
    let aux = directory.join("aux.db");
    prepare(&main, &aux);
    write_across(&main, &aux);
    let journal = leave_it_undecided(&main, &aux);
    assert!(
        journal.is_file(),
        "the super-journal is where the commit left it"
    );
    assert_eq!(
        counts(&main, &aux),
        vec![0, 0],
        "a commit whose super-journal is still there is a commit that never happened"
    );
}

/// Once the super-journal is gone, both databases carry it.
#[test]
fn the_deletion_of_the_super_journal_commits_both() {
    let directory = scratch("decided");
    let main = directory.join("main.db");
    let aux = directory.join("aux.db");
    prepare(&main, &aux);
    write_across(&main, &aux);
    let journal = leave_it_undecided(&main, &aux);
    assert_eq!(counts(&main, &aux), vec![0, 0], "undecided to begin with");
    // The commit point, and the whole of it.
    std::fs::remove_file(&journal).expect("the super-journal is removed");
    assert_eq!(
        counts(&main, &aux),
        vec![1, 1],
        "one deletion decides every participant"
    );
}

/// A marker whose question has been answered is cleared by the open that
/// answered it.
#[test]
fn a_settled_marker_does_not_outlive_the_open_that_read_it() {
    let directory = scratch("settled");
    let main = directory.join("main.db");
    let aux = directory.join("aux.db");
    prepare(&main, &aux);
    write_across(&main, &aux);
    let journal = leave_it_undecided(&main, &aux);
    std::fs::remove_file(&journal).expect("the super-journal is removed");
    assert_eq!(counts(&main, &aux), vec![1, 1]);
    for participant in [&main, &aux] {
        assert!(
            !marker_path(participant).exists(),
            "{} still has a marker asking a question that has an answer",
            participant.display()
        );
    }
}

/// An ordinary one-file commit writes no super-journal and no marker at all.
///
/// The performance claim, stated as a test rather than as an argument: a
/// transaction that wrote one database pays nothing for a protocol that decides
/// nothing.
#[test]
fn a_single_database_commit_writes_no_super_journal() {
    let directory = scratch("single");
    let main = directory.join("main.db");
    {
        let database = Database::open(&main).expect("the database opens");
        let connection = database.connect().expect("the connection opens");
        connection
            .execute_batch("CREATE TABLE t(a); BEGIN; INSERT INTO t VALUES (1); COMMIT;")
            .expect("the transaction commits");
    }
    let leftovers: Vec<String> = std::fs::read_dir(&directory)
        .expect("the directory is readable")
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.file_name().to_string_lossy().to_string())
        .filter(|name| name.contains("-mj"))
        .collect();
    assert!(
        leftovers.is_empty(),
        "a one-file commit wrote a multi-file commit's files: {leftovers:?}"
    );
}
