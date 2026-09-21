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
//! ## Why some of these look like they are testing the same thing twice
//!
//! `a_commit_survives` and `a_commit_survives_without_a_checkpoint` differ by
//! one call, and the difference is the whole point: the first can be satisfied
//! by a checkpoint having folded the log into the file, and the second cannot.
//! A recovery path that only ever ran after a clean checkpoint would pass the
//! first and lose data on the second, which is precisely the failure an
//! application sees after a power cut and never sees in testing.

use std::path::PathBuf;

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

/// Returns the one integer a single-row, single-column answer holds.
///
/// @param rows - what the query returned
fn count(rows: &[Vec<OwnedDatum>]) -> i64 {
    match rows {
        [row] => match row.as_slice() {
            [OwnedDatum::Int(value)] => *value,
            other => panic!("expected one integer, got {other:?}"),
        },
        other => panic!("expected one row, got {}", other.len()),
    }
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

// --- A page a transaction dropped, and what happens when it is not committed
//
// All six of these are task-2043. The engine freed a dropped tree's pages as
// the `DROP` statement ran rather than at the commit, and the free map hands
// the lowest free page to the next allocation - so a `CREATE` in the same
// transaction was given the dropped table's own root page and wrote an empty
// tree over it. The rollback restored the dropped table's catalog row, which
// still named that page, and the table came back empty. Durably: the rows were
// really gone from the file.
//
// The undo buffer could not have repaired it. It holds row before-images, not
// page images, so nothing in it says what the page used to contain. What these
// tests hold in place is the fix's actual claim - **an abandoned transaction
// frees nothing** - which is why four of them look at a table the transaction
// never mentioned, or at a statement that runs after the rollback.

/// Returns the rows of `p` through a freshly opened handle, and checks the file.
///
/// Separate from [`rows_after_reopen`] because these tests care *which* rows
/// came back, not how many: a table that was overwritten by a different table's
/// tree can have the right count and the wrong contents.
///
/// @param path - the database file
fn values_after_reopen(path: &PathBuf) -> Vec<Vec<OwnedDatum>> {
    let database = Database::open(path).expect("the database reopens");
    database.check().expect("the file is sound");
    let connection = database.session();
    connection
        .query("SELECT id, n FROM p ORDER BY id")
        .expect("read after the reopen")
}

/// Creates `p` with three rows, through a handle that is then dropped.
///
/// @param path - the database file
fn three_rows(path: &PathBuf) {
    let database = Database::open(path).expect("the database opens");
    let connection = database.session();
    connection
        .execute_batch(
            "CREATE TABLE p (id INTEGER PRIMARY KEY, n INTEGER); \
             INSERT INTO p VALUES (1, 10), (2, 20), (3, 30)",
        )
        .expect("the table is created with three rows");
}

/// Returns the three rows `p` is made with, as a query returns them.
fn the_three_rows() -> Vec<Vec<OwnedDatum>> {
    vec![
        vec![OwnedDatum::Int(1), OwnedDatum::Int(10)],
        vec![OwnedDatum::Int(2), OwnedDatum::Int(20)],
        vec![OwnedDatum::Int(3), OwnedDatum::Int(30)],
    ]
}

/// A `DROP` and a `CREATE` of the same name, rolled back, keep every row.
///
/// The ticket's own reproduction. Before the fix this answered zero rows, here
/// and after the reopen, and `PRAGMA integrity_check` said `ok` about it -
/// because the tree the catalog pointed at was a perfectly valid empty tree.
#[test]
fn a_dropped_and_recreated_table_rolls_back() {
    let directory = scratch("drop-recreate");
    let path = directory.join("d.rdb");
    three_rows(&path);
    {
        let database = Database::open(&path).expect("the database opens");
        let connection = database.session();
        connection
            .execute_batch(
                "BEGIN; \
                 DROP TABLE p; \
                 CREATE TABLE p (id INTEGER PRIMARY KEY, n INTEGER); \
                 ROLLBACK",
            )
            .expect("the transaction is abandoned");
        assert_eq!(
            connection
                .query("SELECT id, n FROM p ORDER BY id")
                .expect("read in the same session"),
            the_three_rows(),
            "the rows were lost to the handle that abandoned the transaction"
        );
    }
    assert_eq!(values_after_reopen(&path), the_three_rows());
}

/// The name never mattered: a `CREATE` of a different table lost them too.
///
/// This is what says the defect is the page being handed back and not the
/// catalog row being repointed, which is what the ticket first supposed. It
/// fails for the same reason the test above does and reads as a different bug,
/// so both are held.
#[test]
fn a_drop_then_an_unrelated_create_rolls_back() {
    let directory = scratch("drop-other-create");
    let path = directory.join("d.rdb");
    three_rows(&path);
    {
        let database = Database::open(&path).expect("the database opens");
        let connection = database.session();
        connection
            .execute_batch("BEGIN; DROP TABLE p; CREATE TABLE q (a TEXT); ROLLBACK")
            .expect("the transaction is abandoned");
    }
    assert_eq!(values_after_reopen(&path), the_three_rows());
}

/// A `DROP` alone, rolled back, leaves the free map alone.
///
/// **The case the ticket recorded as passing.** It did pass, in the sense that
/// the count immediately after the `ROLLBACK` was three - but the page was
/// still marked free while `p` still pointed at it, so the next statement to
/// allocate anything overwrote `p`'s rows. The `CREATE TABLE` and `INSERT`
/// after the rollback are the whole test; without them it passes against the
/// bug it is here to catch.
#[test]
fn a_dropped_table_rolled_back_keeps_its_pages() {
    let directory = scratch("drop-alone");
    let path = directory.join("d.rdb");
    three_rows(&path);
    {
        let database = Database::open(&path).expect("the database opens");
        let connection = database.session();
        connection
            .execute_batch("BEGIN; DROP TABLE p; ROLLBACK")
            .expect("the transaction is abandoned");
        connection
            .execute_batch(
                "CREATE TABLE later (a TEXT); \
                 INSERT INTO later VALUES ('one'), ('two')",
            )
            .expect("a later statement allocates");
        assert_eq!(
            connection
                .query("SELECT id, n FROM p ORDER BY id")
                .expect("read in the same session"),
            the_three_rows(),
            "a later allocation was given a page p still points at"
        );
    }
    assert_eq!(values_after_reopen(&path), the_three_rows());
}

/// The same for an index: its tree is released by the same call.
///
/// Read through the index rather than through the table, because a table scan
/// would answer correctly from the table's own tree and say nothing about the
/// index's. Before the fix this returned no rows and `PRAGMA integrity_check`
/// reported `row 1 missing from index ix` - the one shape of this defect the
/// check did catch, because it cross-checks an index against its table.
#[test]
fn a_dropped_index_rolled_back_keeps_its_pages() {
    let directory = scratch("drop-index");
    let path = directory.join("d.rdb");
    three_rows(&path);
    {
        let database = Database::open(&path).expect("the database opens");
        let connection = database.session();
        connection
            .execute_batch("CREATE INDEX ix ON p(n)")
            .expect("the index is created");
        connection
            .execute_batch("BEGIN; DROP INDEX ix; ROLLBACK")
            .expect("the transaction is abandoned");
        connection
            .execute_batch("CREATE TABLE later (a TEXT); INSERT INTO later VALUES ('one')")
            .expect("a later statement allocates");
        assert_eq!(
            count(
                &connection
                    .query("SELECT count(*) FROM p WHERE n > 5")
                    .expect("read through the index")
            ),
            3,
            "a later allocation was given a page the index still points at"
        );
    }
    let database = Database::open(&path).expect("the database reopens");
    database.check().expect("the file is sound");
}

/// A `ROLLBACK TO` undoes only the drops taken after the savepoint.
///
/// Both directions in one test, because the two are easy to get wrong in
/// opposite ways and a fix for either alone passes half of it. `p` is dropped
/// *before* the savepoint, so its pages must still be freed by the `COMMIT`;
/// `q` is dropped after it, so `q` must come back whole - including after a
/// later statement has allocated.
///
/// **`PRAGMA freelist_count` is what holds the `p` direction up, and without it
/// this test is satisfied by a fix that leaks.** A savepoint records how long
/// the pending-free list was when it was taken; a first version of the fix
/// derived that from the undo buffer's length instead, and `DROP TABLE p;
/// SAVEPOINT here` leaves both at the same length - so nothing in that number
/// said which came first, and `ROLLBACK TO here` discarded `p`'s pages along
/// with `q`'s. Every visible answer below is the same either way: `p` really is
/// gone from the catalog and `q` really does come back. The only thing that
/// differs is whether `p`'s pages ever return to the free map.
#[test]
fn rolling_back_to_a_savepoint_keeps_the_drops_before_it() {
    let directory = scratch("savepoint-drop");
    let path = directory.join("d.rdb");
    {
        let database = Database::open(&path).expect("the database opens");
        let connection = database.session();
        connection
            .execute_batch(
                "CREATE TABLE p (id INTEGER PRIMARY KEY, n INTEGER); \
                 INSERT INTO p VALUES (1, 10), (2, 20), (3, 30); \
                 CREATE TABLE q (id INTEGER PRIMARY KEY, n INTEGER); \
                 INSERT INTO q VALUES (7, 70), (8, 80)",
            )
            .expect("two tables");
        connection
            .execute_batch(
                "BEGIN; \
                 DROP TABLE p; \
                 SAVEPOINT here; \
                 DROP TABLE q; \
                 ROLLBACK TO here; \
                 COMMIT",
            )
            .expect("the savepoint unwinds and the rest commits");
        // Read before anything else allocates, which would spend them again.
        assert!(
            count(
                &connection
                    .query("PRAGMA freelist_count")
                    .expect("read the free map")
            ) > 0,
            "the commit did not give p's pages back: they are leaked"
        );
        connection
            .execute_batch("CREATE TABLE later (a TEXT); INSERT INTO later VALUES ('one')")
            .expect("a later statement allocates");
        assert_eq!(
            count(
                &connection
                    .query("SELECT count(*) FROM sqlite_master WHERE name = 'p'")
                    .expect("read the catalog")
            ),
            0,
            "the drop taken before the savepoint did not stick"
        );
        assert_eq!(
            connection
                .query("SELECT id, n FROM q ORDER BY id")
                .expect("read the table whose drop was unwound"),
            vec![
                vec![OwnedDatum::Int(7), OwnedDatum::Int(70)],
                vec![OwnedDatum::Int(8), OwnedDatum::Int(80)],
            ],
            "the drop taken after the savepoint was not unwound"
        );
    }
    let database = Database::open(&path).expect("the database reopens");
    database.check().expect("the file is sound");
    let connection = database.session();
    assert_eq!(
        count(
            &connection
                .query("SELECT count(*) FROM q")
                .expect("counted after the reopen")
        ),
        2,
    );
}

/// A rolled-back `CREATE VIRTUAL TABLE` leaves no module behind.
///
/// The fourth shape the ticket asked about, and it needed a second fix.
/// `rebuild_tables` turned any table whose name was in this connection's module
/// map into a virtual table, whatever the catalog said - so after the rollback
/// had correctly restored `p`'s `CREATE TABLE` row and its rows,
/// `PRAGMA table_info(p)` still answered with the fts5 declaration and
/// `SELECT * FROM p` was planned as a scan of a module whose shadow tables no
/// longer existed. It returned nothing, while a reopen of the same file
/// returned all three rows - which is the tell that the schema, not the
/// storage, was what was wrong.
///
/// Asserted in the session that did it, because a reopen has an empty module
/// map and cannot see this.
#[test]
fn a_rolled_back_virtual_table_leaves_no_module_behind() {
    let directory = scratch("drop-virtual");
    let path = directory.join("d.rdb");
    three_rows(&path);
    {
        let database = Database::open(&path).expect("the database opens");
        let connection = database.session();
        connection
            .execute_batch(
                "BEGIN; \
                 DROP TABLE p; \
                 CREATE VIRTUAL TABLE p USING fts5(body); \
                 ROLLBACK",
            )
            .expect("the transaction is abandoned");
        assert_eq!(
            connection
                .query("SELECT id, n FROM p ORDER BY id")
                .expect("p is an ordinary table again"),
            the_three_rows(),
            "the connection still reads p through the module it rolled back"
        );
    }
    assert_eq!(values_after_reopen(&path), the_three_rows());
}
