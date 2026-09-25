//! What survives the handle that wrote it, and what must not.
//!
//! Invariant: **every assertion here is made by a `Database` that did not write
//! the data it is reading.** The writing handle is dropped first, every time.
//! That is the only way a test at this level can tell a durable write from a
//! page sitting in a buffer pool, and it is the difference between testing the
//! engine and testing a cache.
//!
//! ## What this file is not
//!
//! It is not the crash suite. `inillucent-compat` has campaigns that fail the
//! *n*th write for every *n*, cut the log short, fill the disk and crash inside
//! recovery, all under a deterministic simulator, and none of that is
//! reachable - or should be - from a crate that has no dependency on the
//! simulator. Repeating a weaker version of it here would be a test that
//! passes for a reason the strong one already covers.
//!
//! What this file covers is the half the campaigns cannot: that the ordinary,
//! uninjured path an application takes - commit, roll back, save a point,
//! unwind to it, check a point, close, open - agrees with itself **through the
//! public name**, across a reopen, with no harness in the way.
//!
//! ## The other half of this file
//!
//! `durability_arms.rs` beside it holds the cases that run at every arm of
//! `inillucent_compat::matrix`, and the split is not a preference:
//! `crates/inillucent-compat/tests/tooling/scenarios.rs` refuses a file holding both a
//! `scenario!` and a bare `#[test]`, because a file that grades one story six
//! ways and another once produces output nobody can read. What both files build
//! is in `inillucent_compat::durable`.
//!
//! ## Why some of these look like they are testing the same thing twice
//!
//! `a_commit_survives` and `a_commit_survives_without_a_checkpoint` differ by
//! one call, and the difference is the whole point: the first can be satisfied
//! by a checkpoint having folded the log into the file, and the second cannot.
//! A recovery path that only ever ran after a clean checkpoint would pass the
//! first and lose data on the second, which is precisely the failure an
//! application sees after a power cut and never sees in testing.

use std::path::PathBuf;

use inillucent_compat::durable::{
    count, migrate_and_index, the_three_rows, three_rows, values_after_reopen, MIGRATED_ROWS,
};
use inillucent_compat::matrix::Arm;
use inillucent_engine::connect::Database;
use inillucent_tree::datum::OwnedDatum;

/// Returns a fresh, empty directory for one test's files.
///
/// @param tag - what to name the directory after
fn scratch(tag: &str) -> PathBuf {
    let directory = std::env::temp_dir().join(format!(
        "inillucent-durability-{}-{tag}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = std::fs::remove_dir_all(&directory);
    std::fs::create_dir_all(&directory).expect("a scratch directory");
    directory
}

/// Counts the rows in `t` through a freshly opened handle, and checks the file.
///
/// @param path - the database file
fn rows_after_reopen(path: &PathBuf) -> i64 {
    let database = Database::open(path).expect("the database reopens");
    database.check().expect("the file is sound");
    let connection = database.session();
    count(&connection.query("SELECT count(*) FROM t").expect("counted"))
}

/// A committed transaction is there for the next handle to open the file.
#[test]
fn a_commit_survives() {
    let directory = scratch("commit");
    let path = directory.join("d.rdb");
    {
        let database = Database::open(&path).expect("the database opens");
        let connection = database.session();
        connection
            .execute_batch("CREATE TABLE t (id INTEGER PRIMARY KEY)")
            .expect("the table is created");
        connection
            .execute_batch("BEGIN")
            .expect("the transaction opens");
        for id in 1..=100 {
            connection
                .execute(&format!("INSERT INTO t VALUES ({id})"))
                .expect("a row");
        }
        connection
            .execute_batch("COMMIT")
            .expect("the transaction commits");
        database.checkpoint().expect("the log is folded in");
    }
    assert_eq!(rows_after_reopen(&path), 100);
}

/// And it is there without a checkpoint, which is the case recovery exists for.
#[test]
fn a_commit_survives_without_a_checkpoint() {
    let directory = scratch("no-checkpoint");
    let path = directory.join("d.rdb");
    {
        let database = Database::open(&path).expect("the database opens");
        let connection = database.session();
        connection
            .execute_batch("CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)")
            .expect("the table is created");
        connection
            .execute_batch("BEGIN")
            .expect("the transaction opens");
        for id in 1..=250 {
            connection
                .execute(&format!("INSERT INTO t VALUES ({id}, 'row {id}')"))
                .expect("a row");
        }
        connection
            .execute_batch("COMMIT")
            .expect("the transaction commits");
        // Deliberately no checkpoint: the next open has to read the log.
    }
    assert_eq!(rows_after_reopen(&path), 250);
}

/// An abandoned transaction leaves nothing, for this handle or the next.
#[test]
fn an_abandoned_transaction_leaves_nothing() {
    let directory = scratch("abandon");
    let path = directory.join("d.rdb");
    {
        let database = Database::open(&path).expect("the database opens");
        let connection = database.session();
        connection
            .execute_batch("CREATE TABLE t (id INTEGER PRIMARY KEY); INSERT INTO t VALUES (1)")
            .expect("the table is created with one row");
        connection
            .execute_batch("BEGIN")
            .expect("the transaction opens");
        for id in 2..=200 {
            connection
                .execute(&format!("INSERT INTO t VALUES ({id})"))
                .expect("a row");
        }
        connection
            .execute_batch("ROLLBACK")
            .expect("the transaction is abandoned");
    }
    assert_eq!(rows_after_reopen(&path), 1);
}

/// A savepoint unwinds to exactly where it was taken, and the rest commits.
#[test]
fn a_savepoint_unwinds_to_where_it_was_taken() {
    let directory = scratch("savepoint");
    let path = directory.join("d.rdb");
    {
        let database = Database::open(&path).expect("the database opens");
        let connection = database.session();
        connection
            .execute_batch("CREATE TABLE t (id INTEGER PRIMARY KEY)")
            .expect("the table is created");
        connection
            .execute_batch("BEGIN")
            .expect("the transaction opens");
        connection
            .execute("INSERT INTO t VALUES (1)")
            .expect("kept");
        connection
            .execute_batch("SAVEPOINT half")
            .expect("a point is saved");
        connection
            .execute("INSERT INTO t VALUES (2)")
            .expect("discarded");
        connection
            .execute("INSERT INTO t VALUES (3)")
            .expect("discarded");
        connection
            .execute_batch("ROLLBACK TO half")
            .expect("the point is returned to");
        connection
            .execute("INSERT INTO t VALUES (4)")
            .expect("kept");
        connection
            .execute_batch("COMMIT")
            .expect("the transaction commits");
    }
    let database = Database::open(&path).expect("the database reopens");
    database.check().expect("the file is sound");
    let connection = database.session();
    let rows = connection
        .query("SELECT id FROM t ORDER BY id")
        .expect("the rows are read");
    let ids: Vec<i64> = rows
        .iter()
        .filter_map(|row| match row.as_slice() {
            [OwnedDatum::Int(value)] => Some(*value),
            _ => None,
        })
        .collect();
    assert_eq!(ids, vec![1, 4]);
}

/// A schema change rolls back with everything else.
///
/// A catalogue that kept the table while the data rolled back would leave the
/// next statement reading a tree that is not there.
#[test]
fn a_rolled_back_schema_change_leaves_no_table() {
    let directory = scratch("schema-rollback");
    let path = directory.join("d.rdb");
    {
        let database = Database::open(&path).expect("the database opens");
        let connection = database.session();
        connection
            .execute_batch("CREATE TABLE t (id INTEGER PRIMARY KEY)")
            .expect("the first table");
        connection
            .execute_batch("BEGIN")
            .expect("the transaction opens");
        connection
            .execute_batch("CREATE TABLE gone (id INTEGER PRIMARY KEY)")
            .expect("a second table inside the transaction");
        connection
            .execute("INSERT INTO gone VALUES (1)")
            .expect("a row in it");
        connection
            .execute_batch("ROLLBACK")
            .expect("the transaction is abandoned");
        assert!(
            connection.query("SELECT count(*) FROM gone").is_err(),
            "the rolled-back table is still there"
        );
    }
    let database = Database::open(&path).expect("the database reopens");
    database.check().expect("the file is sound");
    let connection = database.session();
    assert!(
        connection.query("SELECT count(*) FROM gone").is_err(),
        "the rolled-back table came back after a reopen"
    );
    assert_eq!(
        count(
            &connection
                .query("SELECT count(*) FROM t")
                .expect("the first table survived")
        ),
        0
    );
}

/// A transaction that dirties more pages than the pool holds still commits, and
/// still reads back.
///
/// The pool is 4,096 frames by default. This writes past that inside one
/// transaction, so the log leads the file and the pool has to evict pages that
/// the transaction has not committed yet.
#[test]
fn a_transaction_larger_than_the_pool_commits() {
    let directory = scratch("large");
    let path = directory.join("d.rdb");
    {
        let database = Database::open(&path).expect("the database opens");
        let connection = database.session();
        connection
            .execute_batch("CREATE TABLE t (id INTEGER PRIMARY KEY, payload TEXT)")
            .expect("the table is created");
        connection
            .execute_batch("BEGIN")
            .expect("the transaction opens");
        let mut insert = connection
            .prepare("INSERT INTO t VALUES (?1, ?2)")
            .expect("the insert prepares");
        // 20,000 rows of about 200 bytes is comfortably more than the pool.
        let payload = "x".repeat(200);
        for id in 1..=20_000i64 {
            insert.reset();
            insert.bind_integer(1, id).expect("the id binds");
            insert.bind_text(2, &payload).expect("the payload binds");
            while insert.step().expect("the insert runs") {}
        }
        drop(insert);
        connection
            .execute_batch("COMMIT")
            .expect("the transaction commits");
    }
    assert_eq!(rows_after_reopen(&path), 20_000);
}

/// Churn - insert, update, delete, repeat - leaves a sound file and an index
/// that still agrees with its table.
#[test]
fn churn_leaves_the_index_agreeing_with_the_table() {
    let directory = scratch("churn");
    let path = directory.join("d.rdb");
    {
        let database = Database::open(&path).expect("the database opens");
        let connection = database.session();
        connection
            .execute_batch(
                "CREATE TABLE t (id INTEGER PRIMARY KEY, bucket INTEGER NOT NULL, tag TEXT);\
                 CREATE INDEX t_bucket ON t (bucket);\
                 CREATE UNIQUE INDEX t_tag ON t (tag);",
            )
            .expect("the schema is created");
        for round in 0..8i64 {
            connection.execute_batch("BEGIN").expect("a round opens");
            for step in 0..250i64 {
                let id = round * 250 + step;
                connection
                    .execute(&format!(
                        "INSERT INTO t VALUES ({id}, {}, 'tag-{id}')",
                        id % 17
                    ))
                    .expect("a row");
            }
            // Move half of them across buckets, and delete a third.
            connection
                .execute(&format!(
                    "UPDATE t SET bucket = bucket + 1 WHERE id % 2 = 0 AND id >= {}",
                    round * 250
                ))
                .expect("an update");
            connection
                .execute(&format!(
                    "DELETE FROM t WHERE id % 3 = 0 AND id >= {}",
                    round * 250
                ))
                .expect("a delete");
            connection.execute_batch("COMMIT").expect("a round commits");
        }
    }
    let database = Database::open(&path).expect("the database reopens");
    database.check().expect("the file is sound after churn");
    let connection = database.session();

    // The same question asked two ways: once the planner answers from the
    // table, once from the index. A difference is an index that has drifted,
    // and it is invisible to any query that only ever goes one way.
    let by_table = count(
        &connection
            .query("SELECT count(*) FROM t WHERE +bucket = 5")
            .expect("counted through the table"),
    );
    let by_index = count(
        &connection
            .query("SELECT count(*) FROM t WHERE bucket = 5")
            .expect("counted through the index"),
    );
    assert_eq!(
        by_table, by_index,
        "the index and the table disagree about how many rows are in bucket 5"
    );
    assert_eq!(
        count(
            &connection
                .query("PRAGMA integrity_check")
                .map(|rows| {
                    // `integrity_check` answers text; turn "ok" into a number so the
                    // helper can be reused, and anything else into a failure.
                    rows.iter()
                        .map(|row| match row.as_slice() {
                            [OwnedDatum::Text(bytes)] if bytes == b"ok" => vec![OwnedDatum::Int(1)],
                            other => panic!("integrity_check said {other:?}"),
                        })
                        .collect::<Vec<_>>()
                })
                .expect("the check runs")
        ),
        1
    );
}

/// Reopening twice in a row is the same as reopening once.
///
/// Recovery has to be idempotent: the second open reads a log the first open
/// already applied, and applying it again must not double anything.
#[test]
fn recovery_is_idempotent() {
    let directory = scratch("idempotent");
    let path = directory.join("d.rdb");
    {
        let database = Database::open(&path).expect("the database opens");
        let connection = database.session();
        connection
            .execute_batch("CREATE TABLE t (id INTEGER PRIMARY KEY, v INTEGER)")
            .expect("the table is created");
        connection
            .execute_batch("BEGIN")
            .expect("the transaction opens");
        for id in 1..=500 {
            connection
                .execute(&format!("INSERT INTO t VALUES ({id}, {})", id * 2))
                .expect("a row");
        }
        connection
            .execute_batch("COMMIT")
            .expect("the transaction commits");
    }
    let first = rows_after_reopen(&path);
    let second = rows_after_reopen(&path);
    let third = rows_after_reopen(&path);
    assert_eq!((first, second, third), (500, 500, 500));
}

/// A checkpoint does not change what the file says.
#[test]
fn a_checkpoint_changes_nothing_a_reader_can_see() {
    let directory = scratch("checkpoint");
    let path = directory.join("d.rdb");
    let before;
    {
        let database = Database::open(&path).expect("the database opens");
        let connection = database.session();
        connection
            .execute_batch("CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)")
            .expect("the table is created");
        for id in 1..=400 {
            connection
                .execute(&format!("INSERT INTO t VALUES ({id}, 'value {id}')"))
                .expect("a row");
        }
        before = connection
            .query("SELECT id, v FROM t ORDER BY id")
            .expect("read before the checkpoint");
        database.checkpoint().expect("the log is folded in");
        let after = connection
            .query("SELECT id, v FROM t ORDER BY id")
            .expect("read after the checkpoint");
        assert_eq!(before, after, "the checkpoint changed what a reader sees");
    }
    let database = Database::open(&path).expect("the database reopens");
    database.check().expect("the file is sound");
    let connection = database.session();
    let after_reopen = connection
        .query("SELECT id, v FROM t ORDER BY id")
        .expect("read after the reopen");
    assert_eq!(before, after_reopen);
}

/// The same index, in write-ahead-log mode, where there is no journal to route
/// around.
///
/// **The case the page stamp is for, and the matrix cannot reach it** - none of
/// its six arms selects `journal_mode = wal` (task-2055). In that mode there is
/// no rollback journal, so `Journal::holds` is false for every page and a bulk
/// build always writes straight into the data file. What can then put the
/// pages' previous life back is the log, and the stamp on the page is what
/// stops it: leave the built page at an LSN of zero, or read a page's LSN off
/// the pool's page count rather than off the file, and redo applies the records
/// that filled those pages when they were leaves of `doc`.
///
/// The close folds nothing, because the read in `migrate_and_index` evicted
/// every dirty frame, so the reopen really does replay the window.
#[test]
fn an_index_built_in_wal_mode_survives_a_reopen() {
    let directory = scratch("wal-built-index");
    let path = directory.join("d.rdb");
    {
        let database = Database::open_at(&path, 4_096, 64).expect("the database opens");
        let connection = database.session();
        connection
            .execute("PRAGMA journal_mode = wal")
            .expect("write-ahead-log mode");
        migrate_and_index(&connection);
    }
    let database = Database::open_at(&path, 4_096, 64).expect("the database reopens");
    database.check().expect("the file is sound");
    let connection = database.session();
    assert_eq!(
        count(
            &connection
                .query("SELECT count(*) FROM doc INDEXED BY doc_seen WHERE seen = 7")
                .expect("read through the index")
        ),
        MIGRATED_ROWS,
        "the index built in write-ahead-log mode does not agree with the table"
    );
}

/// A closed database file is a database on its own, with no log beside it.
///
/// **The property `inillucent backup` and anybody copying an `.rdb` rely on**,
/// stated where it can fail. `ImportedDatabase::drop` folds the log into the
/// file so that a closed file is self contained, and it decided whether it owed
/// a fold by asking whether any frame was still dirty - which is the wrong half
/// of the question at a small buffer pool, because eviction is what clears
/// dirty flags. A session that read its corpus back through 64 frames left
/// nothing dirty, folded nothing, and left a 686 KB rollback journal and an
/// unfolded log beside a file whose meta record still said the database was
/// four pages long (task-2055).
///
/// The copy is what makes that visible: reopening in place finds the log and
/// the journal and recovers, so the file alone is the only thing that can say
/// whether the close finished its work.
#[test]
fn a_closed_file_is_a_database_by_itself() {
    let directory = scratch("self-contained");
    let path = directory.join("d.rdb");
    {
        let database = Database::open_at(&path, 4_096, 64).expect("the database opens");
        let connection = database.session();
        migrate_and_index(&connection);
    }
    let alone = directory.join("copied.rdb");
    std::fs::copy(&path, &alone).expect("the database file is copied");
    let database = Database::open_at(&alone, 4_096, 64).expect("the copy opens");
    database.check().expect("the copy is sound");
    let connection = database.session();
    assert_eq!(
        count(
            &connection
                .query("SELECT count(*) FROM doc NOT INDEXED WHERE seen = 7")
                .expect("read the copy")
        ),
        MIGRATED_ROWS,
        "the closed file needed the log beside it to hold its rows"
    );
}

/// The geometry a case that is not about the configuration runs at.
///
/// The seven cases in `durability_arms.rs` go through `scenario!`, because what
/// they hold in place is only visible where the pool evicts (task-2055). The
/// four task-2051 cases below are about what one session can see after its own
/// rollback, which no page size or pool size changes, so they run once - at the
/// geometry `Database::open` picks, which is what they were written against and
/// what they still open with.
fn the_default_arm() -> Arm {
    inillucent_compat::matrix::default_arm()
}

/// Returns the three rows plus the fourth these tests write after a rollback.
fn the_four_rows() -> Vec<Vec<OwnedDatum>> {
    let mut rows = the_three_rows();
    rows.push(vec![OwnedDatum::Int(4), OwnedDatum::Int(40)]);
    rows
}

/// A rolled-back `DROP COLUMN` leaves the column readable again.
///
/// The ticket's own reproduction. Before the fix `SELECT n FROM p` answered
/// `the tree read for FROM term 0 does not carry column 1` for the rest of the
/// connection's life, while a reopen of the same file read both columns and all
/// three rows.
///
/// `ALTER TABLE ... DROP COLUMN` rebuilds: it releases the table's tree and
/// registers a new one, holding only the surviving columns, under the same
/// handle. The rollback restores the catalog row naming the original root page,
/// and `reattach_entries` skipped the table because the handle was occupied -
/// so the schema described two columns and the tree attached to it had one.
///
/// Asserted in the session that rolled back, because that is the only thing
/// that is wrong: a reopen builds its trees from the catalog and cannot see it.
#[test]
fn a_rolled_back_dropped_column_is_readable_again() {
    let directory = scratch("alter-drop-column");
    let path = directory.join("d.rdb");
    three_rows(&the_default_arm(), &path);
    {
        let database = Database::open(&path).expect("the database opens");
        let connection = database.session();
        connection
            .execute_batch("BEGIN; ALTER TABLE p DROP COLUMN n; ROLLBACK")
            .expect("the transaction is abandoned");
        assert_eq!(
            connection
                .query("SELECT id, n FROM p ORDER BY id")
                .expect("the column the rollback put back is readable"),
            the_three_rows(),
            "the connection still reads p through the tree the ALTER built"
        );
    }
    assert_eq!(
        values_after_reopen(&the_default_arm(), &path),
        the_three_rows()
    );
}

/// A row written after a rolled-back `DROP COLUMN` reaches the file.
///
/// **The read error above is the visible half of this defect and this is the
/// expensive half.** The tree the `ALTER` built is an orphan once the rollback
/// has restored the catalog row: no row names its root page, so nothing will
/// ever read it again. A connection still attached to it accepted the `INSERT`,
/// reported success, read the row back from its own buffer, and the reopened
/// file did not have it.
///
/// The `INSERT` names only `id`, because before the fix this session could not
/// write `n` at all - so a test that inserted both columns would fail on the
/// write and never reach the question it is asking.
#[test]
fn a_rolled_back_dropped_column_does_not_strand_a_later_write() {
    let directory = scratch("alter-drop-column-write");
    let path = directory.join("d.rdb");
    three_rows(&the_default_arm(), &path);
    {
        let database = Database::open(&path).expect("the database opens");
        let connection = database.session();
        connection
            .execute_batch("BEGIN; ALTER TABLE p DROP COLUMN n; ROLLBACK")
            .expect("the transaction is abandoned");
        connection
            .execute_batch("INSERT INTO p (id, n) VALUES (4, 40)")
            .expect("a write after the rollback");
    }
    assert_eq!(
        values_after_reopen(&the_default_arm(), &path),
        the_four_rows(),
        "the write after the rollback went into a tree the catalog does not name"
    );
}

/// The same for `ADD COLUMN`, which rebuilds too and shows nothing at all.
///
/// **This case has no symptom a reader can see, which is why it is held
/// separately.** The ticket recorded `ADD COLUMN` as fine, and every read in the
/// session that rolled one back does answer correctly - because the tree the
/// `ALTER` built carries a *superset* of the catalog's columns, so every column
/// the restored catalog names still finds a slot in it. `SELECT c FROM p` is a
/// parse error against the restored catalog, which looks like the rollback
/// having worked.
///
/// The tree is stranded exactly as above, and the only thing that says so is the
/// reopen: the `INSERT` below reports success, reads back in the same session,
/// and is not in the file. A version of this test without the reopen passes
/// against the bug.
#[test]
fn a_rolled_back_added_column_does_not_strand_a_later_write() {
    let directory = scratch("alter-add-column-write");
    let path = directory.join("d.rdb");
    three_rows(&the_default_arm(), &path);
    {
        let database = Database::open(&path).expect("the database opens");
        let connection = database.session();
        connection
            .execute_batch("BEGIN; ALTER TABLE p ADD COLUMN c INTEGER DEFAULT 9; ROLLBACK")
            .expect("the transaction is abandoned");
        connection
            .execute_batch("INSERT INTO p (id, n) VALUES (4, 40)")
            .expect("a write after the rollback");
        assert_eq!(
            connection
                .query("SELECT id, n FROM p ORDER BY id")
                .expect("read in the same session"),
            the_four_rows(),
            "the session cannot see its own write",
        );
    }
    assert_eq!(
        values_after_reopen(&the_default_arm(), &path),
        the_four_rows(),
        "the write after the rollback went into a tree the catalog does not name"
    );
}

/// `ROLLBACK TO` a savepoint re-attaches, and the `COMMIT` after it is sound.
///
/// The third shape the ticket asked about. It fails the same way and it fails
/// *past the commit*: the transaction goes on to commit successfully, writing
/// the orphan tree's pages into the file where nothing will read them, while the
/// connection goes on answering out of the tree the `ALTER` built.
///
/// The `INSERT` sits between the `ROLLBACK TO` and the `COMMIT` on purpose, so
/// the row it writes is part of the committed transaction rather than a
/// statement after it - which is what says the re-attach happened at the
/// savepoint and not merely by the end of the transaction.
#[test]
fn an_alter_rolled_back_to_a_savepoint_reattaches() {
    let directory = scratch("alter-savepoint");
    let path = directory.join("d.rdb");
    three_rows(&the_default_arm(), &path);
    {
        let database = Database::open(&path).expect("the database opens");
        let connection = database.session();
        connection
            .execute_batch(
                "BEGIN; \
                 SAVEPOINT here; \
                 ALTER TABLE p DROP COLUMN n; \
                 ROLLBACK TO here; \
                 INSERT INTO p (id, n) VALUES (4, 40); \
                 COMMIT",
            )
            .expect("the savepoint unwinds and the rest commits");
        assert_eq!(
            connection
                .query("SELECT id, n FROM p ORDER BY id")
                .expect("read in the same session"),
            the_four_rows(),
            "the connection still reads p through the tree the ALTER built"
        );
    }
    assert_eq!(
        values_after_reopen(&the_default_arm(), &path),
        the_four_rows()
    );
}
