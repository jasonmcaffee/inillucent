//! Recovery rebuilds the free map in the order the log recorded it.
//!
//! Invariant: **a page a recovered log left allocated is never handed out
//! again.** A page has exactly one owner, and recovery is the one place that
//! decides which pages the free map calls free without any page saying so
//! itself - a free-map bit carries no LSN, so nothing below recovery can catch
//! it if the answer is wrong.
//!
//! ## What this is a regression test for
//!
//! Recovery collected the `AllocPage` records it replayed into one list and the
//! `FreePage` records into another, and the caller then claimed every page in
//! the first list and released every page in the second. The frees therefore
//! had the last word whatever order the log put them in, so a page that was
//! **freed and then allocated again** inside the replayed range came back from
//! the recovery marked free while it was live.
//!
//! The next allocation was then handed a page something else already owned. It
//! is silent at write time: the statement that takes the page reports success,
//! and nothing is wrong until something reads a row whose value lived there.
//!
//! Measured on Nikaya's 6.9 GB corpus. Segment 318 of its log holds, in order,
//! `AllocPage 211519` at lsn 21,194,643,632, `FreePage 211519` at
//! 21,197,118,024 and `AllocPage 211519` again at 21,197,121,920. After the
//! reopen, one `CREATE TABLE` took pages 211519 and 211520 for its two roots
//! and wrote over a `document` row's 6,040-byte out-of-line value; `count(*)`
//! and a key-only projection still answered and a projection that decodes every
//! column returned `database disk image is malformed`. `ANALYZE` reached the
//! same ending through `sqlite_stat1`, which is a `CREATE TABLE` wearing a
//! different hat.
//!
//! ## Why there are two arms
//!
//! The second one does the same work with nothing freed after the checkpoint,
//! so its two lists cannot disagree. It passed before the fix and passes after
//! it, which is what makes the first arm a diagnosis rather than a guess: the
//! variable is the page freed and allocated again inside the replayed range,
//! not the checkpoint, the reopen, the wide value or the `CREATE TABLE`.

use std::path::PathBuf;
use std::sync::Arc;

use inillucent_compat::newengine::ImportedDatabase;
use inillucent_exec::physical::Params;
use inillucent_sim::sim_vfs::{SimConfig, SimVfs};
use inillucent_tree::datum::OwnedDatum;
use inillucent_vfs::Vfs;

/// The page size these tests build at.
///
/// Small on purpose: a value of a few tens of kilobytes then needs a run of ten
/// pages rather than one, so the run that is freed and taken again is large
/// enough that a later allocation lands inside it rather than beside it.
const PAGE_SIZE: usize = 4_096;

/// How many frames the pool holds.
///
/// Large enough that nothing is evicted and no checkpoint happens on its own,
/// which is what leaves the whole workload after the checkpoint in the log.
const FRAMES: usize = 8_192;

/// How long each row's out-of-line value is.
const BODY: usize = 40_000;

/// The table the wide values live in.
const TABLE: &str = "CREATE TABLE document (
       id    TEXT PRIMARY KEY,
       kind  TEXT NOT NULL,
       body  TEXT NOT NULL
     )";

/// The statement that takes a page after the reopen.
///
/// Nikaya's migration `005_document_index_queue`, word for word. Any statement
/// that allocates would do; this is the one that reached the live corpus.
const QUEUE: &str = "CREATE TABLE document_index_queue (
       document_id  TEXT PRIMARY KEY REFERENCES document(id) ON DELETE CASCADE,
       queued_at    INTEGER NOT NULL
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
                "{sql}: {} ({})",
                error.message(),
                error.detail().unwrap_or_default()
            )
        });
}

/// Inserts one row whose `body` is long enough to be held out of line.
///
/// The body is one repeated character per row, so a page taken over by another
/// tree is caught by the bytes as well as by the length: a value reassembled
/// out of the wrong pages is the wrong characters.
///
/// @param engine - the database
/// @param id - the row's key
/// @param fill - the character the body repeats
fn insert(engine: &mut ImportedDatabase, id: &str, fill: char) {
    let body = fill.to_string().repeat(BODY);
    run(
        engine,
        &format!("INSERT INTO document (id, kind, body) VALUES ('{id}', 'email', '{body}')"),
    );
}

/// Reads one row's body back and returns it.
///
/// @param engine - the reopened database
/// @param id - the row's key
fn body_of(engine: &mut ImportedDatabase, id: &str) -> String {
    let outcome = engine
        .execute_any(
            &format!("SELECT body FROM document WHERE id = '{id}'"),
            &Params::new(),
        )
        .unwrap_or_else(|error| {
            panic!(
                "reading {id}'s body: {} ({})",
                error.message(),
                error.detail().unwrap_or_default()
            )
        });
    let row = outcome
        .rows
        .first()
        .unwrap_or_else(|| panic!("{id} is missing from the reopened database"));
    match row.first() {
        Some(OwnedDatum::Text(bytes)) => String::from_utf8_lossy(bytes).to_string(),
        other => panic!("{id}'s body came back as {other:?}"),
    }
}

/// Builds the fixture, reopens it, and creates a table that has to allocate.
///
/// @param free_and_take_again - whether a page run is freed and taken again
///   after the last checkpoint, which is the variable under test
fn reopen_and_allocate(free_and_take_again: bool) -> ImportedDatabase {
    let vfs = Arc::new(SimVfs::new(SimConfig::default()));
    let path = PathBuf::from("free-map-recovery.rdb");
    {
        let mut engine = ImportedDatabase::create_on(
            Arc::clone(&vfs) as Arc<dyn Vfs>,
            path.clone(),
            PAGE_SIZE,
            FRAMES,
        )
        .expect("the database is created");
        run(&mut engine, TABLE);
        insert(&mut engine, "keep", 'k');
        // **The last checkpoint.** Every allocation and every free after this
        // reaches the reopen through the log, which is where the two lists were
        // built from.
        engine.checkpoint().expect("the fixture checkpoints");
        if free_and_take_again {
            insert(&mut engine, "temp", 't');
            run(&mut engine, "DELETE FROM document WHERE id = 'temp'");
        }
        insert(&mut engine, "again", 'a');
        // Forgotten rather than dropped: dropping the handle would give it the
        // chance to write a checkpoint, and a checkpointed database has no log
        // left for recovery to get wrong.
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
    run(&mut engine, QUEUE);
    engine
}

/// A page freed and taken again before the reopen is not handed out a third time.
///
/// The discriminator. With the two lists restored - claim every allocation,
/// then release every free - the `CREATE TABLE` takes the run that holds
/// `again`'s body and the read below fails with `page is not a blob extent`.
#[test]
fn a_page_freed_and_taken_again_is_not_handed_out_after_the_reopen() {
    let mut engine = reopen_and_allocate(true);
    assert_eq!(
        body_of(&mut engine, "again"),
        "a".repeat(BODY),
        "the row written into the reused pages reads back whole"
    );
    assert_eq!(
        body_of(&mut engine, "keep"),
        "k".repeat(BODY),
        "the row written before the checkpoint reads back whole"
    );
}

/// The same reopen with nothing freed after the checkpoint reads back too.
///
/// The control. It passed before the fix as well, which is what says the
/// variable is the free that the recovery applied out of order rather than the
/// checkpoint, the reopen, the wide value or the `CREATE TABLE`.
#[test]
fn the_same_reopen_with_nothing_freed_reads_back() {
    let mut engine = reopen_and_allocate(false);
    assert_eq!(
        body_of(&mut engine, "again"),
        "a".repeat(BODY),
        "the row written after the checkpoint reads back whole"
    );
    assert_eq!(
        body_of(&mut engine, "keep"),
        "k".repeat(BODY),
        "the row written before the checkpoint reads back whole"
    );
}
