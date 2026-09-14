//! Opening one database file and replaying its log into it.
//!
//! Invariant: **a file is recovered before anything reads a page out of it.**
//! Every path that opens a database of this engine - the file a connection is
//! opened on, and every file it `ATTACH`es - comes through `open_file`, so
//! there is one set of rules about what a torn tail means rather than one per
//! caller.
//!
//! Extracted from `lib.rs`, whose recorded size this pushed past. Nothing here
//! changed in the move; the recovery order and the reasoning for it are in the
//! comments on `open_file` itself.

use inillucent_base::DbResult;
use inillucent_catalog::paged::SchemaEntry;
use inillucent_pool::Database;
use inillucent_tree::PagedTree;
use inillucent_vfs::DbPath;
use inillucent_wal::{Wal, WalOptions, FIRST_LSN};

use crate::{attach_catalog, let_the_pool_ask_the_log, read_catalog, LearningRows};

/// One database file, opened, recovered, and ready to be read.
pub(crate) struct OpenedFile {
    /// The pool, the meta page and the free map.
    pub(crate) database: Database,
    /// The log, positioned where recovery ended.
    pub(crate) wal: std::rc::Rc<Wal>,
    /// The catalog tree, attached from the meta page's root.
    pub(crate) catalog_tree: PagedTree,
    /// The highest transaction number any record recovery scanned carried.
    ///
    /// **A reopened database must not reuse a number the log still holds**, and
    /// this is what the engine's counter is started above. Recovery decides
    /// which records to replay by transaction number, so a number used twice in
    /// one log makes two different transactions into one - a run that wrote as
    /// transaction 3 and crashed leaves records the *next* run resurrects the
    /// moment its own transaction 3 commits, permanently.
    ///
    /// `inillucent-wal` has reported this since it was written and nothing read
    /// it; the cross-file commit is what made it load-bearing, because a marker
    /// naming transaction 7 in a file whose next run also calls something
    /// transaction 7 would suppress a commit that had nothing to do with it.
    pub(crate) highest_txn: u64,
}

/// Returns where the log resumes, raising it above every stamp the file carries.
///
/// **A page's LSN has to be a position in the stream currently beside the file,
/// and after a recovery whose chain was short of what the pages reflect it is
/// not.** Recovery applies a record to a page only when the page's
/// stamp is below the record's, so a page stamped by a stream that no longer
/// exists silently swallows every later write to it - the record is skipped,
/// the file stays structurally intact, and nothing anywhere says a committed row
/// was lost. It is how Nikaya's mail database ended up with page 3 stamped
/// 21,939,058,496 beside a log ending at 21,075,008,440, after 24 segments were
/// moved aside to recover it.
///
/// So the log resumes at `max(recovered.next_lsn, high_water + 1)`. In every
/// healthy file the first term already wins and this changes nothing: the
/// write-ahead rule puts every stamp below the log's durable end, and the
/// durable end is at or below where recovery stopped. It fires only on a file
/// whose log is short of what its pages carry.
///
/// Two things follow from an LSN being a **byte offset inside a segment**:
///
/// 1. The jump takes the *next* sequence. The write offset of a record is
///    `header + (lsn - segment.first_lsn)`, so resuming 864 million positions
///    into the segment recovery stopped in would ask for an 864 MB file.
/// 2. The meta page is checkpointed before a record is written at the new
///    position. That leaves a gap between the old segment's last byte and the
///    new one's first, and `read_chain` stops a chain at a gap - correctly,
///    since a gap is otherwise a lost segment - so the next recovery has to
///    start *inside* the new segment rather than walk up to it. The claim the
///    checkpoint makes is true at that moment: the replay's pages have just been
///    flushed, and there are no records between the chain's end and the new
///    position.
///
/// @param database - the recovered file, whose pool carries the high water
/// @param outcome - what recovery found
fn resume_above_every_stamp(
    database: &mut Database,
    outcome: &inillucent_wal::Recovered,
) -> DbResult<(u64, u64)> {
    let next_lsn = outcome.next_lsn.max(FIRST_LSN);
    let sequence = outcome.sequence.max(1);
    // Read off the pool rather than off the meta record. `Database::open` seeds
    // it with what the meta page carried and `Pool::writeback` has raised it for
    // every page this recovery has already evicted, so it is the higher of the
    // two and never the lower.
    let high_water = database.pool().high_water_lsn();
    if high_water < next_lsn {
        return Ok((next_lsn, sequence));
    }
    let resumed = high_water.saturating_add(1);
    let rolled = sequence.saturating_add(1);
    database.set_log_position(resumed, outcome.latest_cts, rolled);
    database.checkpoint()?;
    Ok((resumed, rolled))
}

/// Reads the catalog as the file's own pages currently show it, before redo.
///
/// A plain, checksummed page fetch - see `open_file`'s own comment on why a
/// caller of this may need to run a repair pass and ask again rather than
/// treat a failure here as final.
///
/// @param database - the file being opened
fn read_checkpointed_catalog(database: &Database) -> DbResult<Vec<SchemaEntry>> {
    let before = attach_catalog(database.pool(), database.catalog_root())?;
    read_catalog(database.pool(), &before)
}

/// Opens one database file, replays its log into it, and opens that log.
///
/// **The one recovery path, for the file a connection is opened on and for
/// every file it attaches.** An `ATTACH`ed database is an ordinary database of
/// this engine - it may have been written by a process that crashed, and a
/// second recovery path would be a second set of rules about what a torn tail
/// means. There is one, and both callers take it.
///
/// @param vfs - the file system the file and its log live on
/// @param db_path - the database file
/// @param frames - how many frames the buffer pool holds
/// @param doubtful - transactions whose `Commit` record is not the decision
pub(crate) fn open_file(
    vfs: &std::sync::Arc<dyn inillucent_vfs::Vfs>,
    db_path: &DbPath,
    frames: usize,
    doubtful: &std::collections::BTreeSet<u64>,
) -> DbResult<OpenedFile> {
    let database = Database::open_before_recovery(vfs.as_ref(), db_path, frames.max(64))?;

    // **Recovery.** The log is replayed into the file before anything is read
    // out of it, which is what makes this an open rather than a reader of
    // whatever the last checkpoint happened to leave behind.
    //
    // It could not be done before a tree's identifier was stored in the
    // catalog. `TreeRows` is keyed by that identifier, every logical row record
    // carries it, and until then the writer's numbering and a reader's were
    // different - so a replay would have put rows into the wrong tree, which is
    // a wrong answer rather than a refusal.
    //
    // From the file's own checkpoint, not from the start of the log:
    // `RecoveryStart::fresh` scans from `FIRST_LSN` and would replay everything
    // the last checkpoint already applied.
    //
    // `doubtful` is how a cross-file commit reaches this. A transaction that
    // wrote two databases votes in each file's log and is *decided* by a
    // super-journal outside both, so a `Commit` record for one of those
    // transactions is a vote rather than the decision - see
    // `super_journal_doubt`.
    let meta = database.meta();
    let start = if meta.checkpoint_lsn == 0 {
        inillucent_wal::RecoveryStart {
            doubtful: doubtful.clone(),
            ..inillucent_wal::RecoveryStart::fresh(database.uuid())
        }
    } else {
        inillucent_wal::RecoveryStart {
            uuid: database.uuid(),
            checkpoint_lsn: meta.checkpoint_lsn,
            sequence: meta.wal_sequence,
            cts_watermark: meta.cts_watermark,
            doubtful: doubtful.clone(),
        }
    };
    let mut database = database;
    // The shapes come from the catalog as it stood at the last checkpoint, plus
    // the catalog tree itself, whose own rows are what a `CREATE TABLE` writes.
    // A record naming a tree that is in none of them - a table created *after*
    // the checkpoint, whose rows were then written - makes `TreeRows` refuse,
    // which fails this open with a named error rather than replaying into a
    // tree that is not the one meant.
    //
    // **The catalog root can itself be the page a crash tore, and reading it
    // here is what makes that unrepairable.** This read is an ordinary
    // checksummed page fetch, done before redo has run a single record, so a
    // checkpoint interrupted while rewriting the catalog's own page fails
    // exactly the way redo exists to fix - except redo cannot run first
    // either, because its row decoder needs the catalog's shapes to replay a
    // row record. Neither side can go first, which is what makes it a circle
    // rather than an ordering bug.
    //
    // It breaks like this: when this first read fails, a **repair pass** runs
    // ahead of the real one, tolerant of a record naming a tree it has not
    // been told the shape of - every other record it applies exactly as
    // normal, including the schema tree's own rows, whose shape
    // (`schema_layout()`) is fixed and needs no catalog at all. That is enough
    // whenever the log holds a record for the torn page, which it does for
    // the one case measured: `ddl.rs`'s `refresh_statistics` rewrites a
    // table's catalog row on every checkpoint whose shape changed, and that
    // rewrite is an ordinary schema-tree row record - the very thing this pass
    // can already replay without the catalog. The catalog is read again after
    // it; if it is still unreadable, the file is refused the way it always
    // was, unchanged - a corruption no record in the log describes is not
    // this pass's to fix, and `crates/inillucent-compat/tests/corruption.rs`
    // is what proves that stays true. Reached only on the error path, so an
    // ordinary open pays nothing extra: one read, one pass, exactly as before.
    let mut repaired = false;
    let checkpointed = match read_checkpointed_catalog(&database) {
        Ok(checkpointed) => checkpointed,
        Err(_) => {
            repaired = true;
            let mut repair =
                inillucent_txn::redo::Applier::new(&mut database, LearningRows::new_tolerant(&[]));
            inillucent_wal::recover(vfs.as_ref(), db_path, start.clone(), &mut repair)?;
            read_checkpointed_catalog(&database)?
        }
    };
    let (outcome, free_map) = {
        // **The second pass is tolerant exactly when the first one had to run
        // (task-1932, found by `reindex_crash.rs`).** The catalog it is seeded
        // with was read *after* the repair pass replayed the whole window, so
        // it is the catalog as at the END of the log, while the records it is
        // about to replay run from the start of it. A tree that was superseded
        // inside that window - which is what `REINDEX` and `CREATE INDEX` do,
        // every rebuild allocating a fresh tree - is therefore named by no row
        // this pass will ever see, and refusing its records failed the open
        // outright:
        //
        // ```text
        // bad parameter or other API misuse: the log names tree 2147483649,
        // which this recovery was not told the shape of
        // ```
        //
        // A database that had survived a crash during a `REINDEX` would not
        // open at all. Skipping those records is right rather than merely
        // convenient: the tree they name has been dropped by the end of the
        // window, so replaying them would write pages nothing will ever read.
        // When the checkpointed catalog was readable this stays strict, which
        // is every ordinary open.
        let mut applier = inillucent_txn::redo::Applier::new(
            &mut database,
            LearningRows::new_with_tolerance(&checkpointed, repaired),
        );
        let outcome = inillucent_wal::recover(vfs.as_ref(), db_path, start, &mut applier)?;
        (outcome, applier.free_map_changes().to_vec())
    };
    database.load_free_map()?;
    // The free map is rebuilt after the scan rather than inside it: the map and
    // every page write are both behind `&mut Database`, and one record cannot
    // hold two mutable borrows of the same object.
    //
    // **In log order.** Claiming every allocation and then
    // releasing every free gave the frees the last word, so a page freed and
    // allocated again inside the replayed range came back free while it was
    // live, and the next allocation handed it to a second owner. See
    // `Applier::free_map_changes`.
    for change in &free_map {
        match change.allocated {
            true => database.claim(change.page)?,
            false => database.release(change.page, 1)?,
        }
    }
    inillucent_wal::truncate_after(vfs.as_ref(), db_path, &outcome)?;

    // **The log resumes where recovery ended, not at the beginning.** Opening it
    // at `FIRST_LSN` with sequence 1 starts a second stream over the same
    // segments: the session writes records the *next* open cannot find, because
    // the meta page's checkpoint points into the first stream. A test caught it
    // as a table created after an open vanishing on the one after that -
    // `no such table: second` from a file that had just been told to make it.
    //
    // **And above every stamp the file carries.** See
    // `resume_above_every_stamp`.
    let (next_lsn, sequence) = resume_above_every_stamp(&mut database, &outcome)?;
    let wal = std::rc::Rc::new(Wal::open(
        std::sync::Arc::clone(vfs),
        db_path,
        database.uuid(),
        next_lsn,
        sequence,
        WalOptions::default(),
    )?);
    database.pool().set_durable_lsn(wal.write_ahead_point());
    // **Seeds `Pool::note_dirty_from`'s floor from this file's own last
    // checkpoint**, so a page whose stamp predates it cannot repeat, across a
    // reopen, the bug `Pool::retained_lsn`'s doc comment describes within one
    // session. `database.meta()` reflects `resume_above_every_stamp`'s own
    // corrective checkpoint above when it ran, so this always reads the
    // recovery point actually in force for this file right now, not the one
    // this open started from.
    database
        .pool()
        .set_retained_lsn(database.meta().checkpoint_lsn);
    let_the_pool_ask_the_log(database.pool(), &wal);

    // The catalog is read again, because recovery may have changed it: a
    // `CREATE TABLE` after the checkpoint is a row in this very tree.
    let catalog_tree = attach_catalog(database.pool(), database.catalog_root())?;
    Ok(OpenedFile {
        database,
        wal,
        catalog_tree,
        highest_txn: outcome.highest_txn,
    })
}
