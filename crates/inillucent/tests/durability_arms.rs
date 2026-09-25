//! The durability cases that run at every arm of the matrix.
//!
//! Invariant: **a storage defect that only appears at one page size or one pool
//! size fails here rather than in a story three files away.**
//!
//! ## Why this is a file of its own
//!
//! `durability.rs` beside it is the same subject and holds the cases that pick
//! their own geometry. Keeping both in one file is refused by
//! `crates/inillucent-compat/tests/tooling/scenarios.rs`: a file that invokes
//! `scenario!` may hold no bare `#[test]`, because then "the file grades one
//! story six ways and another once and the run's output cannot tell them
//! apart". What the two files share is in `inillucent_compat::durable`.
//!
//! ## What every case here does that `durability.rs` also does
//!
//! Asserts through a `Database` that did not write the data it is reading. The
//! writing handle is dropped first, every time, which is the only way a test at
//! this level can tell a durable write from a page sitting in a buffer pool.

use std::path::Path;

use inillucent_compat::durable::{
    count, migrate_and_index, sound_after_reopen, the_three_rows, three_rows, values_after_reopen,
    MIGRATED_ROWS,
};
use inillucent_compat::matrix::Arm;
use inillucent_compat::scenario;
use inillucent_tree::datum::OwnedDatum;

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
//
// **They ran at one configuration until task-2055, and that is how they missed
// what the fix let through.** Every one of them opened with `Database::open`,
// which is 32,768 byte pages and a 4,096 frame pool - so nothing here evicted,
// nothing here wrote a rollback journal, and the file was folded at every close.
// The defect task-2043 let through needed all three of those to be false: at a
// 4,096 byte page with 64 frames, `story_nikaya` reopened a migrated database on
// `a key below separator 0 is in the child above it`. That is why they moved
// into this file and go through `scenario!`, so each one runs at all six arms of
// `inillucent_compat::matrix` and a fix that holds only where nothing is evicted
// fails here rather than in a story three files away.

/// A `DROP` and a `CREATE` of the same name, rolled back, keep every row.
///
/// The ticket's own reproduction. Before the fix this answered zero rows, here
/// and after the reopen, and `PRAGMA integrity_check` said `ok` about it -
/// because the tree the catalog pointed at was a perfectly valid empty tree.
///
/// @param arm - the configuration this run is at
/// @param area - a scratch directory of this arm's own
fn a_dropped_and_recreated_table_rolls_back(arm: &Arm, area: &Path) {
    let path = area.join("d.rdb");
    three_rows(arm, &path);
    {
        let database = arm.open(&path).expect("the database opens");
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
    assert_eq!(values_after_reopen(arm, &path), the_three_rows());
}

scenario!(
    a_dropped_and_recreated_table_rolls_back,
    a_dropped_and_recreated_table_rolls_back
);

/// The name never mattered: a `CREATE` of a different table lost them too.
///
/// This is what says the defect is the page being handed back and not the
/// catalog row being repointed, which is what the ticket first supposed. It
/// fails for the same reason the test above does and reads as a different bug,
/// so both are held.
///
/// @param arm - the configuration this run is at
/// @param area - a scratch directory of this arm's own
fn a_drop_then_an_unrelated_create_rolls_back(arm: &Arm, area: &Path) {
    let path = area.join("d.rdb");
    three_rows(arm, &path);
    {
        let database = arm.open(&path).expect("the database opens");
        let connection = database.session();
        connection
            .execute_batch("BEGIN; DROP TABLE p; CREATE TABLE q (a TEXT); ROLLBACK")
            .expect("the transaction is abandoned");
    }
    assert_eq!(values_after_reopen(arm, &path), the_three_rows());
}

scenario!(
    a_drop_then_an_unrelated_create_rolls_back,
    a_drop_then_an_unrelated_create_rolls_back
);

/// A `DROP` alone, rolled back, leaves the free map alone.
///
/// **The case the ticket recorded as passing.** It did pass, in the sense that
/// the count immediately after the `ROLLBACK` was three - but the page was
/// still marked free while `p` still pointed at it, so the next statement to
/// allocate anything overwrote `p`'s rows. The `CREATE TABLE` and `INSERT`
/// after the rollback are the whole test; without them it passes against the
/// bug it is here to catch.
///
/// @param arm - the configuration this run is at
/// @param area - a scratch directory of this arm's own
fn a_dropped_table_rolled_back_keeps_its_pages(arm: &Arm, area: &Path) {
    let path = area.join("d.rdb");
    three_rows(arm, &path);
    {
        let database = arm.open(&path).expect("the database opens");
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
    assert_eq!(values_after_reopen(arm, &path), the_three_rows());
}

scenario!(
    a_dropped_table_rolled_back_keeps_its_pages,
    a_dropped_table_rolled_back_keeps_its_pages
);

/// The same for an index: its tree is released by the same call.
///
/// Read through the index rather than through the table, because a table scan
/// would answer correctly from the table's own tree and say nothing about the
/// index's. Before the fix this returned no rows and `PRAGMA integrity_check`
/// reported `row 1 missing from index ix` - the one shape of this defect the
/// check did catch, because it cross-checks an index against its table.
///
/// @param arm - the configuration this run is at
/// @param area - a scratch directory of this arm's own
fn a_dropped_index_rolled_back_keeps_its_pages(arm: &Arm, area: &Path) {
    let path = area.join("d.rdb");
    three_rows(arm, &path);
    {
        let database = arm.open(&path).expect("the database opens");
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
    sound_after_reopen(arm, &path);
}

scenario!(
    a_dropped_index_rolled_back_keeps_its_pages,
    a_dropped_index_rolled_back_keeps_its_pages
);

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
///
/// @param arm - the configuration this run is at
/// @param area - a scratch directory of this arm's own
fn rolling_back_to_a_savepoint_keeps_the_drops_before_it(arm: &Arm, area: &Path) {
    let path = area.join("d.rdb");
    {
        let database = arm.open(&path).expect("the database opens");
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
    let database = sound_after_reopen(arm, &path);
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

scenario!(
    rolling_back_to_a_savepoint_keeps_the_drops_before_it,
    rolling_back_to_a_savepoint_keeps_the_drops_before_it
);

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
///
/// @param arm - the configuration this run is at
/// @param area - a scratch directory of this arm's own
fn a_rolled_back_virtual_table_leaves_no_module_behind(arm: &Arm, area: &Path) {
    let path = area.join("d.rdb");
    three_rows(arm, &path);
    {
        let database = arm.open(&path).expect("the database opens");
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
    assert_eq!(values_after_reopen(arm, &path), the_three_rows());
}

scenario!(
    a_rolled_back_virtual_table_leaves_no_module_behind,
    a_rolled_back_virtual_table_leaves_no_module_behind
);

// --- A tree built straight into the data file, on pages a DROP gave back
//
// task-2055. A bulk build writes its pages into the data file itself and logs
// no image of them, which is design 2 of task-2000 and is what makes
// `CREATE INDEX` write its index once rather than three times. The pages it is
// handed come from the free map, and after task-2043 an `ALTER TABLE` on a
// populated table frees the table's old tree at its commit - so the index below
// is built onto pages that were leaves of `doc` until the statement before it.
//
// Three separate things then had to be true for those pages to survive a
// reopen, and none of them was:
//
//  1. The built page carried an LSN of zero, so redo replayed every record the
//     page's previous life had written over the index that replaced it.
//  2. `Applier::page_lsn` asked the *pool* whether the file held a page, and
//     the pool's count is the last checkpoint's - so for every page allocated
//     since that checkpoint it answered "not there" and the page-LSN rule was
//     switched off for the whole tail of the file.
//  3. A rollback journal holds a pre-image of every page an eviction wrote
//     since the last checkpoint, and only a checkpoint disposes of one. The
//     more a small pool evicts, the fewer frames are left dirty - so the close,
//     which folds only when something is dirty, did nothing and left the
//     journal hot. The next open replayed it and put the pages back to their
//     previous life, with nothing in the log able to rebuild the index.
//
// The read before the close is required. Without it the pool still holds dirty
// frames, the close folds, and every one of the three is hidden - which is the
// difference between the arm that failed and the arm that did not.

/// How many rows the migrated table holds.
///
/// An index built on pages an `ALTER TABLE` freed survives the reopen.
///
/// Asserts the count through the index against the count through the table,
/// because a file whose index tree has been replaced by the table's old leaves
/// answers the second correctly and the first with whatever those leaves say.
/// The reopen's own `check` is what named the failure - `a key below separator
/// 0 is in the child above it` - and it is kept as well, since it is the thing
/// an application would hit first.
///
/// @param arm - the configuration this run is at
/// @param area - a scratch directory of this arm's own
fn an_index_built_on_freed_pages_survives_a_reopen(arm: &Arm, area: &Path) {
    let path = area.join("d.rdb");
    {
        let database = arm.open(&path).expect("the database opens");
        migrate_and_index(&database.session());
    }
    let database = sound_after_reopen(arm, &path);
    let connection = database.session();
    let through_the_table = count(
        &connection
            .query("SELECT count(*) FROM doc NOT INDEXED WHERE seen = 7")
            .expect("read through the table"),
    );
    let through_the_index = count(
        &connection
            .query("SELECT count(*) FROM doc INDEXED BY doc_seen WHERE seen = 7")
            .expect("read through the index"),
    );
    assert_eq!(through_the_table, MIGRATED_ROWS);
    assert_eq!(
        through_the_index, through_the_table,
        "the index built on the freed pages does not agree with the table"
    );
}

scenario!(
    an_index_built_on_freed_pages_survives_a_reopen,
    an_index_built_on_freed_pages_survives_a_reopen
);
