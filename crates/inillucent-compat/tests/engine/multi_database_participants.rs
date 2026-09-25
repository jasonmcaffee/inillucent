//! Which schemas a commit is decided over, and which it is not.
//!
//! Invariant: a transaction pays for a cross-file commit exactly when it wrote
//! more than one file. The participant set is collected from the logs that were
//! actually used, and it is emptied by the commit that consumed it - so a schema
//! change to an attached database cannot leave itself in the set for the *next*
//! statement to find, which would make a one-file insert write a super-journal
//! and put a `Commit` record in a log it never touched.
//!
//! This is a test rather than a comment because that is exactly what the first
//! version did: `seal` committed the log directly instead of going through
//! `commit_across`, so `CREATE TABLE aux.t` left `aux` in the set and the next
//! autocommit `INSERT INTO main.t` took the two-phase path.

use std::path::PathBuf;

use inillucent_compat::workspace_root;
use inillucent_engine::connect::Database;

/// Returns a scratch directory for one scenario, emptied first.
fn scratch(name: &str) -> PathBuf {
    let directory = workspace_root()
        .join("_agent_output/participants")
        .join(name);
    let _ = std::fs::remove_dir_all(&directory);
    let _ = std::fs::create_dir_all(&directory);
    directory
}

/// Returns the multi-file commit's files left in a directory.
///
/// @param directory - where the databases are
fn multi_file_leftovers(directory: &std::path::Path) -> Vec<String> {
    std::fs::read_dir(directory)
        .expect("the directory is readable")
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.file_name().to_string_lossy().to_string())
        .filter(|name| name.contains("-mj"))
        .collect()
}

/// A statement that wrote one file does not become a two-file commit because an
/// earlier statement wrote another.
#[test]
fn a_schema_change_on_one_database_does_not_enlist_it_in_the_next_commit() {
    let directory = scratch("no-carry-over");
    let main = directory.join("main.db");
    let aux = directory.join("aux.db");
    let database = Database::open(&main).expect("the database opens");
    let connection = database.session();
    connection
        .execute_batch(&format!(
            "CREATE TABLE t(a);
             ATTACH DATABASE '{}' AS aux;
             CREATE TABLE aux.t(a);",
            aux.display().to_string().replace('\\', "/")
        ))
        .expect("both schemas are written");
    // One file, in autocommit, straight after a schema change to the other one.
    connection
        .execute_batch("INSERT INTO main.t VALUES (1)")
        .expect("the insert commits");
    assert_eq!(
        connection
            .decided_over()
            .expect("nothing is running on this connection"),
        1,
        "a statement that wrote one file was committed as though it had written two"
    );
    assert!(
        multi_file_leftovers(&directory).is_empty(),
        "a one-file statement wrote a multi-file commit's files: {:?}",
        multi_file_leftovers(&directory)
    );
    // And the rows are where they should be, which is the half a file check
    // cannot see.
    let rows = connection
        .query("SELECT (SELECT count(*) FROM main.t), (SELECT count(*) FROM aux.t)")
        .expect("the query runs");
    let counts: Vec<i64> = rows
        .first()
        .map(|row| {
            row.iter()
                .filter_map(|held| match held {
                    inillucent_tree::datum::OwnedDatum::Int(number) => Some(*number),
                    _ => None,
                })
                .collect()
        })
        .unwrap_or_default();
    assert_eq!(counts, vec![1, 0]);
}

/// A transaction that really did write two files cleans up after itself.
#[test]
fn a_two_file_transaction_leaves_nothing_behind() {
    let directory = scratch("two-file");
    let main = directory.join("main.db");
    let aux = directory.join("aux.db");
    let database = Database::open(&main).expect("the database opens");
    let connection = database.session();
    connection
        .execute_batch(&format!(
            "CREATE TABLE t(a);
             ATTACH DATABASE '{}' AS aux;
             CREATE TABLE aux.t(a);
             BEGIN;
             INSERT INTO t VALUES (1);
             INSERT INTO aux.t VALUES (2);
             COMMIT;",
            aux.display().to_string().replace('\\', "/")
        ))
        .expect("the transaction commits");
    assert_eq!(
        connection
            .decided_over()
            .expect("nothing is running on this connection"),
        2,
        "a transaction that wrote two files was not decided over both"
    );
    assert!(
        multi_file_leftovers(&directory).is_empty(),
        "the commit left files behind: {:?}",
        multi_file_leftovers(&directory)
    );
}
