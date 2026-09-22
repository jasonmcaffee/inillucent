//! Recovery derives a tree's shape from the catalog **as it now stands**.
//!
//! Invariant: **a crash after a migration reopens.** A database whose last
//! checkpoint predates an `ALTER TABLE ... ADD COLUMN` and a `CREATE INDEX` on
//! the added column has to be recoverable from its log, and the shape recovery
//! uses to replay that index's leaves has to be the shape the process that
//! wrote them used.
//!
//! ## What this is a regression test for
//!
//! It was not, and the failure was total: the database could not be opened at
//! all while its log was beside it.
//!
//! `ALTER TABLE ... ADD COLUMN` rewrites the table's catalog row - a delete and
//! an insert - and recovery's applier kept every row it replayed in a list it
//! only ever appended to, seeded with the catalog as at the last checkpoint. The
//! superseded definition was therefore still in that list, first, and the owner
//! table was looked up with `find`. So `CREATE INDEX chunk_embedded_at_idx ON
//! chunk (embedded_at)` was resolved against a `chunk` that had no
//! `embedded_at`: the key column resolved to no table column, and an unresolved
//! index key column is given `PhysicalType::Any` where the writer had used
//! `PhysicalType::Int64`.
//!
//! An `Any` mini-column is wider. Recovery therefore repacks the index's leaves
//! less densely than the process that wrote them, and a `CompactLeaf` record -
//! which carries no page image, because a compaction is meant to be
//! deterministic given the page it starts from - cannot fit rows that
//! demonstrably fitted when they were written. The open fails with
//! `database disk image is malformed`.
//!
//! Measured on Nikaya's real 5.8 GB corpus before it was fixed: index leaf
//! 237505, 2,031 live rows, refused.
//!
//! ## Why there are two arms
//!
//! The second one puts the same column in the original `CREATE TABLE` and does
//! the same work, the same crash and the same reopen. It passed before the fix
//! and passes after it, which is what makes the first arm a diagnosis rather
//! than a guess: the variable is the catalog row that was **superseded after the
//! last checkpoint**, not the column, the index, the crash or the workload.

use std::path::PathBuf;
use std::sync::Arc;

use inillucent_compat::newengine::ImportedDatabase;
use inillucent_exec::physical::Params;
use inillucent_sim::sim_vfs::{SimConfig, SimVfs};
use inillucent_tree::datum::OwnedDatum;
use inillucent_vfs::Vfs;

/// The page size these tests build at.
///
/// Small on purpose: a leaf holds fewer index entries, so the leaf that a
/// compaction cannot repack is reached in thousands of rows rather than
/// hundreds of thousands.
const PAGE_SIZE: usize = 4_096;

/// How many frames the pool holds.
///
/// Large enough that nothing is evicted and no checkpoint happens on its own,
/// which is what leaves the whole workload in the log.
const FRAMES: usize = 8_192;

/// How many rows the fixture seeds.
const ROWS: i64 = 12_000;

/// The table, without the column the first arm adds later.
const BASE_TABLE: &str = "CREATE TABLE chunk (
       id           TEXT PRIMARY KEY,
       document_id  TEXT NOT NULL,
       ordinal      INTEGER NOT NULL,
       content      TEXT NOT NULL,
       UNIQUE (document_id, ordinal)
     )";

/// The same table with the column declared up front.
const TABLE_WITH_COLUMN: &str = "CREATE TABLE chunk (
       id           TEXT PRIMARY KEY,
       document_id  TEXT NOT NULL,
       ordinal      INTEGER NOT NULL,
       content      TEXT NOT NULL,
       embedded_at  INTEGER,
       UNIQUE (document_id, ordinal)
     )";

/// Runs one statement, failing the test with what the engine said.
///
/// @param engine - the database
/// @param sql - the statement
fn run(engine: &mut ImportedDatabase, sql: &str) {
    engine
        .execute_any(sql, &Params::new())
        .unwrap_or_else(|error| {
            panic!(
                "{sql}: {} ({:?})",
                error.message(),
                error.detail().unwrap_or_default()
            )
        });
}

/// Returns a scattered row number, so index entries do not arrive in key order.
///
/// A stride coprime with the row count walks every row exactly once, and
/// entries landing in the middle of a leaf are what make a leaf compact rather
/// than append.
///
/// @param nth - how far through the walk
/// @param rows - how many rows there are
fn scatter(nth: i64, rows: i64) -> i64 {
    (nth.saturating_mul(4_861) % rows).saturating_add(1)
}

/// Seeds the table and returns the database, checkpointed.
///
/// @param engine - the database
fn seed(engine: &mut ImportedDatabase) {
    let content = "x".repeat(200);
    run(engine, "BEGIN");
    for nth in 1..=ROWS {
        let sql = format!(
            "INSERT INTO chunk (id, document_id, ordinal, content) \
             VALUES ('chunk-{nth:09}', 'doc-{:07}', {}, '{content}')",
            nth / 40,
            nth % 40
        );
        run(engine, &sql);
        if nth % 3_000 == 0 {
            run(engine, "COMMIT");
            run(engine, "BEGIN");
        }
    }
    run(engine, "COMMIT");
}

/// Moves index entries off the NULL end, in transactions, checkpointing nothing.
///
/// @param engine - the database
fn move_the_entries(engine: &mut ImportedDatabase) {
    let mut written = 0i64;
    for batch in 0..6i64 {
        run(engine, "BEGIN");
        for nth in 0..2_000i64 {
            let id = scatter(written + nth, ROWS);
            let sql = format!(
                "UPDATE chunk SET embedded_at = {} WHERE id = 'chunk-{id:09}'",
                1_700_000_000 + batch / 2
            );
            run(engine, &sql);
        }
        run(engine, "COMMIT");
        written = written.saturating_add(2_000);
    }
}

/// Returns how many rows a reopened database holds, and how many carry the
/// column, read through the index and through a scan.
///
/// @param engine - the reopened database
fn counts(engine: &mut ImportedDatabase) -> (i64, i64) {
    let outcome = engine
        .execute_any(
            "SELECT count(*), count(embedded_at) FROM chunk",
            &Params::new(),
        )
        .unwrap_or_else(|error| {
            panic!(
                "the reopened database could not be read: {}",
                error.message()
            )
        });
    let row = outcome.rows.first().expect("one row").clone();
    let number = |slot: usize| match row.get(slot) {
        Some(OwnedDatum::Int(value)) => *value,
        other => panic!("expected an integer, got {other:?}"),
    };
    (number(0), number(1))
}

/// Builds a database, crashes it mid-log, and returns it reopened.
///
/// @param table - the `CREATE TABLE` text
/// @param alter - whether to add the column after the checkpoint
fn crash_and_reopen(
    table: &str,
    alter: bool,
) -> Result<ImportedDatabase, inillucent_base::DbError> {
    let vfs = Arc::new(SimVfs::new(SimConfig::default()));
    let path = PathBuf::from("recovery-shapes.rdb");
    {
        let mut engine = ImportedDatabase::create_on(
            Arc::clone(&vfs) as Arc<dyn Vfs>,
            path.clone(),
            PAGE_SIZE,
            FRAMES,
        )
        .expect("the database is created");
        run(&mut engine, table);
        seed(&mut engine);
        // **The last checkpoint.** Everything after this reaches a reopen
        // through the log, which is the whole point of the arm.
        engine.checkpoint().expect("the fixture checkpoints");
        if alter {
            run(
                &mut engine,
                "ALTER TABLE chunk ADD COLUMN embedded_at INTEGER",
            );
        }
        run(
            &mut engine,
            "CREATE INDEX chunk_embedded_at_idx ON chunk (embedded_at)",
        );
        move_the_entries(&mut engine);
        // Forgotten rather than dropped: dropping the handle would give it the
        // chance to write a checkpoint, and a checkpointed database is not the
        // one this test is about.
        std::mem::forget(engine);
    }
    let snapshot = vfs.crash();
    let recovered: Arc<dyn Vfs> = Arc::new(SimVfs::recovered(SimConfig::default(), &snapshot));
    ImportedDatabase::open_on(recovered, path, PAGE_SIZE, FRAMES)
}

/// A crash that recovers every row drops no log record, and says so.
///
/// **The counter is the fix** (task-2066 §4.1.10). `tolerate_unknown_tree`
/// turns "the log names tree N, which this recovery was not told the shape of"
/// into `Ok(())` and forgets the record, and `inillucent-wal`'s replay loop
/// counts an `Ok` as an applied record - so a recovery that dropped rows
/// reported exactly the numbers of one that did not. task-1932 and task-2033
/// are both defects that lived inside that silence; the second lost an inserted
/// row and 487 FTS5 records with `PRAGMA integrity_check` answering `ok`.
///
/// This is the arm that says the ordinary case is clean. The one below is the
/// one that says the number can move, and without it this would be a counter
/// nobody has seen count.
#[test]
fn a_recovery_that_restores_every_row_drops_nothing() {
    let mut engine = crash_and_reopen(BASE_TABLE, true).unwrap_or_else(|error| {
        panic!(
            "the database did not reopen: {} ({})",
            error.message(),
            error.detail().unwrap_or_default()
        )
    });
    let (rows, carried) = counts(&mut engine);
    assert_eq!(rows, ROWS, "every seeded row is back");
    assert_eq!(carried, 12_000, "every committed update is back");
    let report = engine.recovery_report();
    assert!(
        report.applied > 0,
        "this arm replayed nothing, so it cannot say anything about drops"
    );
    assert_eq!(
        report.dropped, 0,
        "the recovery dropped {} log record(s) while restoring every row, which means the \
         counter is counting something it should not",
        report.dropped
    );
}

/// A table created, filled and dropped inside the window drops no record.
///
/// **Not a drop, and that is the finding** (task-2066 §4.1.10). This is the
/// shape that looks most likely to produce one: the transient table's rows are
/// in the log and the catalog the recovery reads no longer names it. It
/// replays cleanly, because `LearningRows` learns a tree's shape from the
/// `InsertRow` records naming the schema tree, and the `CREATE TABLE` is one of
/// those.
///
/// So the tolerated drop is narrower than the code around it suggests: it needs
/// a catalog row that reached the log as a *page image* or through a bulk build
/// rather than as a row, which is the case task-2033 found and repaired. That
/// the ordinary shape cannot produce one is worth a test, because the next
/// person to read `tolerate_unknown_tree` will assume it fires often.
///
/// The counter itself is proven in `inillucent-engine`'s own unit tests, where
/// the drop can be handed to the function directly.
#[test]
fn a_table_created_and_dropped_inside_the_window_replays_without_a_drop() {
    let vfs = Arc::new(SimVfs::new(SimConfig::default()));
    let path = PathBuf::from("dropped-counter.rdb");
    {
        let mut engine = ImportedDatabase::create_on(
            Arc::clone(&vfs) as Arc<dyn Vfs>,
            path.clone(),
            PAGE_SIZE,
            FRAMES,
        )
        .expect("the database is created");
        run(&mut engine, BASE_TABLE);
        seed(&mut engine);
        engine.checkpoint().expect("the fixture checkpoints");
        run(&mut engine, "CREATE TABLE transient (a INTEGER, b TEXT)");
        for at in 0..200i64 {
            run(
                &mut engine,
                &format!("INSERT INTO transient VALUES ({at}, 'row {at}')"),
            );
        }
        run(&mut engine, "DROP TABLE transient");
        std::mem::forget(engine);
    }
    let snapshot = vfs.crash();
    let recovered: Arc<dyn Vfs> = Arc::new(SimVfs::recovered(SimConfig::default(), &snapshot));
    let mut engine =
        ImportedDatabase::open_on(recovered, path, PAGE_SIZE, FRAMES).unwrap_or_else(|error| {
            panic!(
                "the database did not reopen: {} ({})",
                error.message(),
                error.detail().unwrap_or_default()
            )
        });

    let outcome = engine
        .execute_any("SELECT count(*) FROM chunk", &Params::new())
        .unwrap_or_else(|error| {
            panic!(
                "the reopened database could not be read: {}",
                error.message()
            )
        });
    let rows = match outcome.rows.first().and_then(|row| row.first()) {
        Some(OwnedDatum::Int(value)) => *value,
        other => panic!("expected an integer, got {other:?}"),
    };
    assert_eq!(rows, ROWS, "the surviving table lost rows");

    let report = engine.recovery_report();
    assert!(
        report.applied > 0,
        "this arm replayed nothing, so it says nothing about drops"
    );
    assert_eq!(
        report.dropped, 0,
        "a table created and dropped inside the window produced {} dropped record(s). That is          not wrong on its own, but it is a change in when the engine tolerates a record, and          this arm exists to notice it.",
        report.dropped
    );
}

/// A column added after the last checkpoint, and indexed, reopens after a crash.
#[test]
fn a_column_added_after_the_last_checkpoint_reopens_after_a_crash() {
    let mut engine = crash_and_reopen(BASE_TABLE, true).unwrap_or_else(|error| {
        panic!(
            "the database did not reopen: {} ({})",
            error.message(),
            error.detail().unwrap_or_default()
        )
    });
    let (rows, carried) = counts(&mut engine);
    assert_eq!(rows, ROWS, "every seeded row is back");
    assert_eq!(
        carried, 12_000,
        "every committed update to the added column is back"
    );
}

/// The same crash with the column declared up front reopens too.
///
/// The control. It passed before the fix as well, which is what says the
/// variable is the catalog row superseded after the last checkpoint rather than
/// the column, the index, the workload or the crash.
#[test]
fn the_same_crash_with_the_column_declared_up_front_reopens() {
    let mut engine = crash_and_reopen(TABLE_WITH_COLUMN, false).unwrap_or_else(|error| {
        panic!(
            "the control did not reopen: {} ({})",
            error.message(),
            error.detail().unwrap_or_default()
        )
    });
    let (rows, carried) = counts(&mut engine);
    assert_eq!(rows, ROWS, "every seeded row is back");
    assert_eq!(carried, 12_000, "every committed update is back");
}

/// A `COLLATE NOCASE` index survives a crash with its entries intact.
///
/// **The second way the replay could disagree with the write path, and it is a
/// different one.** `redo::insert_row`, `delete_row` and
/// `update_in_place` parsed the leaf they were locating in with
/// `LeafMut::new(bytes)` and then `view()`, which is a `LeafRef` carrying no
/// collations and no directions - so `locate` compared under BINARY and searched
/// as though every key column ascended. On a `NOCASE` index that finds nothing:
/// the delete leaves its entry behind and the insert adds a second under the
/// same key, and the index then holds entries for rows that no longer have them.
///
/// It is checked with `PRAGMA integrity_check` rather than by waiting for a
/// compaction to run out of room, because the damage is a disagreement between
/// an index and its table and that is what the checker is for: it reports
/// `row <rowid> missing from index` and `wrong # of entries in index` in
/// SQLite's own wording.
///
/// The labels are chosen so that BINARY order and NOCASE order genuinely differ:
/// they alternate case, so under NOCASE they interleave by number and under
/// BINARY every capital sorts before every lowercase one.
#[test]
fn a_nocase_index_keeps_its_entries_across_a_crash() {
    let vfs = Arc::new(SimVfs::new(SimConfig::default()));
    let path = PathBuf::from("recovery-nocase.rdb");
    let rows = 4_000i64;
    {
        let mut engine = ImportedDatabase::create_on(
            Arc::clone(&vfs) as Arc<dyn Vfs>,
            path.clone(),
            PAGE_SIZE,
            FRAMES,
        )
        .expect("the database is created");
        run(
            &mut engine,
            "CREATE TABLE note (id INTEGER PRIMARY KEY, label TEXT)",
        );
        run(&mut engine, "BEGIN");
        for nth in 1..=rows {
            let sql = format!(
                "INSERT INTO note (id, label) VALUES ({nth}, '{}')",
                label(nth, false)
            );
            run(&mut engine, &sql);
        }
        run(&mut engine, "COMMIT");
        engine.checkpoint().expect("the fixture checkpoints");
        run(
            &mut engine,
            "CREATE INDEX note_label_idx ON note (label COLLATE NOCASE)",
        );
        // Every label changes case and number, so every entry moves - which is a
        // delete and an insert against a leaf ordered under NOCASE.
        for batch in 0..4i64 {
            run(&mut engine, "BEGIN");
            for nth in 0..1_000i64 {
                let id = (batch.saturating_mul(1_000) + nth) % rows + 1;
                let sql = format!(
                    "UPDATE note SET label = '{}' WHERE id = {id}",
                    label(id, true)
                );
                run(&mut engine, &sql);
            }
            run(&mut engine, "COMMIT");
        }
        std::mem::forget(engine);
    }
    let snapshot = vfs.crash();
    let recovered: Arc<dyn Vfs> = Arc::new(SimVfs::recovered(SimConfig::default(), &snapshot));
    let mut engine =
        ImportedDatabase::open_on(recovered, path, PAGE_SIZE, FRAMES).unwrap_or_else(|error| {
            panic!(
                "the database did not reopen: {} ({})",
                error.message(),
                error.detail().unwrap_or_default()
            )
        });
    let outcome = engine
        .execute_any("PRAGMA integrity_check", &Params::new())
        .expect("the check runs");
    let said = match outcome.rows.first().and_then(|row| row.first()) {
        Some(OwnedDatum::Text(bytes)) => String::from_utf8_lossy(bytes).to_string(),
        other => panic!("expected text, got {other:?}"),
    };
    assert_eq!(
        said, "ok",
        "the index and its table disagree after recovery"
    );

    let outcome = engine
        .execute_any("SELECT count(*) FROM note", &Params::new())
        .expect("the count runs");
    assert_eq!(
        outcome.rows.first().and_then(|row| row.first()),
        Some(&OwnedDatum::Int(rows)),
        "every row is back"
    );
}

/// Returns a label whose BINARY order is not its NOCASE order.
///
/// @param nth - which row
/// @param moved - whether this is the value the update writes
fn label(nth: i64, moved: bool) -> String {
    let number = if moved {
        nth.saturating_add(rows_offset())
    } else {
        nth
    };
    if number % 2 == 0 {
        format!("Alpha-{number:07}")
    } else {
        format!("alpha-{number:07}")
    }
}

/// How far an update moves a label's number.
///
/// Enough that the entry lands somewhere else in the tree rather than back on
/// the leaf it left.
fn rows_offset() -> i64 {
    1_777
}
