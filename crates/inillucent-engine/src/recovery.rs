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

use crate::{attach_catalog, let_the_pool_ask_the_log, read_catalog};
use crate::{index_shape, keyed_table_shape, schema_layout, table_from_create_sql, table_shape};
use inillucent_base::error::refusal;
use inillucent_catalog::paged::ObjectKind;
use inillucent_pool::PageId;
use inillucent_tree::datum::Datum;
use inillucent_txn::redo::{RowRedo, TreeRows};

/// Returns which segment of a database a file name beside it is.
///
/// **Re-exported rather than reimplemented.** The command surface reads the
/// directory beside a database to find a segment the chain does not reach
/// (task-1979, C9) and cannot ask `inillucent-wal` directly - the dependency
/// contract does not give it that edge, and there is no reason to add one for a
/// string function. What a segment is called is
/// `inillucent_wal::segment::segment_name`, and this is its inverse, so the two
/// stay in one crate rather than in two that agree today.
pub use inillucent_wal::{first_lsn_of, sequence_of_segment_name};

/// What opening one file did to it, for a caller that has to say so.
///
/// **An operator could not tell a clean open from a recovered one (task-1979,
/// C10).** After a `TerminateProcess` on a writer mid transaction the reopen
/// replayed the log, returned the right rows and said nothing, in text and in
/// `--output json`. The numbers here are what the log's own scan already
/// counted; nothing is computed for them.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct RecoveryReport {
    /// Whether the log held anything above the file's own checkpoint.
    pub recovered: bool,
    /// How many records the scan read.
    pub scanned: u64,
    /// How many records the second pass applied.
    pub applied: u64,
    /// How many records the replay **dropped** rather than applied.
    ///
    /// A record naming a tree this pass had no shape for. The argument for
    /// dropping one is that the tree was dropped before the end of the window,
    /// and that argument has been wrong twice here - task-1932 and task-2033,
    /// where three `CREATE TABLE`s and an `INSERT` in one transaction lost the
    /// inserted row and an FTS5 index lost 487 records, with
    /// `PRAGMA integrity_check` answering `ok` on the result.
    ///
    /// **It is reported because it was invisible** (task-2066 §4.1.10). A drop
    /// answers `Ok`, and the replay loop counts an `Ok` as applied, so a
    /// recovery that dropped rows printed the same numbers as one that did not.
    /// Non-zero here does not by itself mean data was lost - a genuinely
    /// dropped table produces one - but it is the only signal there is, and an
    /// operator who cannot see it cannot ask.
    pub dropped: u64,
    /// How many transactions committed in the replayed window.
    pub committed: u64,
    /// How many transactions were open at the end of the log and were
    /// discarded.
    pub losers: u64,
    /// The highest segment the live chain reaches.
    ///
    /// A file beside the database at a sequence above this is one nothing will
    /// replay and nothing will remove (task-1979, C9). Finding one means
    /// reading the directory, which no layer below the command surface does, so
    /// this is the number those layers can supply and the caller does the
    /// looking.
    pub last_sequence: u64,
    /// The stream position the chain ended at.
    ///
    /// Beside `last_sequence` because the sequence on its own cannot tell a
    /// leftover copy from the next segment a live writer rolled to. A copy
    /// holds positions the chain has already passed; a genuine later segment
    /// starts at or above this number.
    pub last_lsn: u64,
}

/// One database file, opened, recovered, and ready to be read.
pub(crate) struct OpenedFile {
    /// The pool, the meta page and the free map.
    pub(crate) database: Database,
    /// The log, positioned where recovery ended.
    pub(crate) wal: std::rc::Rc<Wal>,
    /// The catalog tree, attached from the meta page's root.
    pub(crate) catalog_tree: PagedTree,
    /// What the open did to the file, for the caller to report.
    pub(crate) recovery: RecoveryReport,
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

/// Puts the free map's own changes back, in log order.
///
/// **In log order.** Claiming every allocation and then releasing every free
/// gave the frees the last word, so a page freed and allocated again inside the
/// replayed range came back free while it was live, and the next allocation
/// handed it to a second owner. See `Applier::free_map_changes`.
///
/// It is applied after the scan rather than inside it because the map and every
/// page write are both behind `&mut Database`, and one record cannot hold two
/// mutable borrows of the same object.
///
/// @param database - the file being opened
/// @param changes - what the replay did to the free map
fn apply_free_map_changes(
    database: &mut Database,
    changes: &[inillucent_txn::redo::FreeMapChange],
) -> DbResult<()> {
    for change in changes {
        match change.allocated {
            true => database.claim(change.page)?,
            false => database.release(change.page, 1)?,
        }
    }
    Ok(())
}

/// Returns where this file's recovery starts.
///
/// From the file's own checkpoint, not from the start of the log:
/// `RecoveryStart::fresh` scans from `FIRST_LSN` and would replay everything
/// the last checkpoint already applied.
///
/// `doubtful` is how a cross-file commit reaches this. A transaction that wrote
/// two databases votes in each file's log and is *decided* by a super-journal
/// outside both, so a `Commit` record for one of those transactions is a vote
/// rather than the decision - see `super_journal_doubt`.
///
/// @param database - the file being opened
/// @param doubtful - transactions whose `Commit` record is not the decision
fn where_recovery_starts(
    database: &Database,
    doubtful: &std::collections::BTreeSet<u64>,
) -> inillucent_wal::RecoveryStart {
    let meta = database.meta();
    if meta.checkpoint_lsn == 0 {
        return inillucent_wal::RecoveryStart {
            doubtful: doubtful.clone(),
            ..inillucent_wal::RecoveryStart::fresh(database.uuid())
        };
    }
    inillucent_wal::RecoveryStart {
        uuid: database.uuid(),
        checkpoint_lsn: meta.checkpoint_lsn,
        sequence: meta.wal_sequence,
        cts_watermark: meta.cts_watermark,
        doubtful: doubtful.clone(),
    }
}

/// Returns the catalog redo is seeded with, repairing the file first when the
/// page it lives on is the one a crash tore.
///
/// Answers the catalog and whether the repair pass ran; `replay_with_repair`
/// needs the second, because a catalog read *after* a repair is the catalog at
/// the end of the window rather than at its start.
///
/// The shapes come from the catalog as it stood at the last checkpoint, plus
/// the catalog tree itself, whose own rows are what a `CREATE TABLE` writes.
/// A record naming a tree that is in none of them - a table created *after*
/// the checkpoint, whose rows were then written - makes `TreeRows` refuse,
/// which fails this open with a named error rather than replaying into a
/// tree that is not the one meant.
///
/// **The catalog root can itself be the page a crash tore, and reading it
/// here is what makes that unrepairable.** This read is an ordinary
/// checksummed page fetch, done before redo has run a single record, so a
/// checkpoint interrupted while rewriting the catalog's own page fails
/// exactly the way redo exists to fix - except redo cannot run first
/// either, because its row decoder needs the catalog's shapes to replay a
/// row record. Neither side can go first, which is what makes it a circle
/// rather than an ordering bug.
///
/// It breaks like this: when this first read fails, a **repair pass** runs
/// ahead of the real one, tolerant of a record naming a tree it has not
/// been told the shape of - every other record it applies exactly as
/// normal, including the schema tree's own rows, whose shape
/// (`schema_layout()`) is fixed and needs no catalog at all. That is enough
/// whenever the log holds a record for the torn page, which it does for
/// the one case measured: `ddl.rs`'s `refresh_statistics` rewrites a
/// table's catalog row on every checkpoint whose shape changed, and that
/// rewrite is an ordinary schema-tree row record - the very thing this pass
/// can already replay without the catalog. The catalog is read again after
/// it; if it is still unreadable, the file is refused the way it always
/// was, unchanged - a corruption no record in the log describes is not
/// this pass's to fix, and `crates/inillucent-compat/tests/corruption.rs`
/// is what proves that stays true. Reached only on the error path, so an
/// ordinary open pays nothing extra: one read, one pass, exactly as before.
///
/// @param database - the file being opened
/// @param vfs - the file system the log lives on
/// @param db_path - the database file
/// @param start - where in the log to replay from
fn catalog_before_redo(
    database: &mut Database,
    vfs: &std::sync::Arc<dyn inillucent_vfs::Vfs>,
    db_path: &DbPath,
    start: &inillucent_wal::RecoveryStart,
) -> DbResult<(Vec<SchemaEntry>, bool)> {
    match read_checkpointed_catalog(database) {
        Ok(checkpointed) => Ok((checkpointed, false)),
        Err(_) => {
            {
                // **The whole page images first** (task-2000, design 1a). The
                // pass below is the full applier: it replays logical row
                // records, and replaying one means reading the leaf it names.
                // When the page a crash tore is the catalog's own, that read is
                // the thing that fails - so the pass sent to repair it died on
                // the damage it was sent to repair, and the open was refused
                // with `page 3 checksum ... is not the computed ...`. Measured
                // by `wal_crash`'s checkpoint campaign at cut 57, which the
                // rollback journal used to answer by putting the page back
                // before the log was ever read.
                //
                // An images pass needs no catalog and no row decoder - it copies
                // `WritePage`, `CompactLeaf` and split images into their pages
                // and nothing else - so it can go first, and after it every page
                // the log carries whole is whole. It is the same pass
                // `replay_with_repair` runs for the same reason one layer up.
                let mut images = inillucent_txn::redo::Applier::images(
                    database,
                    LearningRows::new_tolerant(&[]),
                );
                inillucent_wal::recover(vfs.as_ref(), db_path, start.clone(), &mut images)
                    .map_err(|why| {
                        let said = why.detail().unwrap_or_default().to_string();
                        why.with_detail(format!("applying the log's page images: {said}"))
                    })?;
            }
            {
                let mut repair =
                    inillucent_txn::redo::Applier::new(database, LearningRows::new_tolerant(&[]));
                inillucent_wal::recover(vfs.as_ref(), db_path, start.clone(), &mut repair)?;
            }
            Ok((read_checkpointed_catalog(database)?, true))
        }
    }
}

/// Reads the catalog as the file's own pages currently show it, before redo.
///
/// A plain, checksummed page fetch - see `open_file`'s own comment on why a
/// caller of this may need to run a repair pass and ask again rather than
/// treat a failure here as final.
///
/// @param database - the file being opened
fn read_checkpointed_catalog(database: &Database) -> DbResult<Vec<SchemaEntry>> {
    let before = attach_catalog(database.pool(), database.catalog_root()).map_err(|error| {
        let said = error.detail().unwrap_or_default().to_string();
        error.with_detail(format!("attaching the catalog before redo: {said}"))
    })?;
    read_catalog(database.pool(), &before).map_err(|error| {
        let said = error.detail().unwrap_or_default().to_string();
        error.with_detail(format!("reading the catalog before redo: {said}"))
    })
}

/// Replays the log into the file, logically, and reports what it did.
///
/// Its own function because `open_file` runs it twice when the first attempt
/// reports corruption: once as it stands, and once after every whole page image
/// in the window has been applied. The catalog it is seeded with and its
/// tolerance are the same both times, so the only difference between the two is
/// that the pages the log describes are whole.
///
/// @param database - the file being opened
/// @param vfs - the file system the log lives on
/// @param db_path - the database file
/// @param start - where in the log to replay from
/// @param checkpointed - the catalog the row decoder is seeded with
/// @param tolerant - whether a record naming an unknown tree is skipped
fn replay(
    database: &mut Database,
    vfs: &std::sync::Arc<dyn inillucent_vfs::Vfs>,
    db_path: &DbPath,
    start: inillucent_wal::RecoveryStart,
    checkpointed: &[SchemaEntry],
    tolerant: bool,
) -> DbResult<(
    inillucent_wal::recover::Recovered,
    Vec<inillucent_txn::redo::FreeMapChange>,
)> {
    let (outcome, changes) = {
        let mut applier = inillucent_txn::redo::Applier::new(
            database,
            LearningRows::new_with_tolerance(checkpointed, tolerant),
        );
        let outcome = inillucent_wal::recover(vfs.as_ref(), db_path, start, &mut applier).map_err(
            |error| {
                let said = error.detail().unwrap_or_default().to_string();
                error.with_detail(format!("replaying the log: {said}"))
            },
        )?;
        let mut outcome = outcome;
        // **The applier is the only thing that knows** (task-2066 §4.1.10).
        // `inillucent_wal::recover` counts an `Ok` from `redo` as an applied
        // record, and a dropped one answers `Ok` - so the count has to come
        // back from the applier rather than out of the loop.
        outcome.dropped = applier.rows().dropped();
        (outcome, applier.free_map_changes().to_vec())
    };
    // **Inside this function rather than after it**, because the free map's own
    // page is read here and it is a page like any other: a crash can tear it,
    // and the log can hold it whole. Leaving it outside put it beyond the
    // retry, so the one page a checkpoint rewrites on every checkpoint was the
    // one page the repair could not reach (task-1962, roadmap item 6).
    database.load_free_map().map_err(|error| {
        let said = error.detail().unwrap_or_default().to_string();
        error.with_detail(format!("reading the free map after redo: {said}"))
    })?;
    Ok((outcome, changes))
}

/// Replays the log, and repairs the pages it carries whole when the replay
/// reports corruption.
///
/// Its own function rather than a block inside `open_file` because it is one
/// decision with two outcomes, and `open_file` is already the longest function
/// in this file.
///
/// @param database - the file being opened
/// @param vfs - the file system the log lives on
/// @param db_path - the database file
/// @param start - where in the log to replay from
/// @param checkpointed - the catalog the row decoder is seeded with
/// @param tolerant - whether a record naming an unknown tree is skipped
fn replay_with_repair(
    database: &mut Database,
    vfs: &std::sync::Arc<dyn inillucent_vfs::Vfs>,
    db_path: &DbPath,
    start: inillucent_wal::RecoveryStart,
    checkpointed: &[SchemaEntry],
    tolerant: bool,
) -> DbResult<(
    inillucent_wal::recover::Recovered,
    Vec<inillucent_txn::redo::FreeMapChange>,
)> {
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
    match replay(
        database,
        vfs,
        db_path,
        start.clone(),
        checkpointed,
        tolerant,
    ) {
        Ok(replayed) => Ok(replayed),
        // **A page the log holds whole, torn, and read by a record that is
        // not the one that would have fixed it (task-1962, roadmap item
        // 6).** Redo reads the page a row record changes, so a crash that
        // tore a page fails the replay at the first record naming it - even
        // when a later record in the same window carries that page whole.
        // The images need no catalog and no row decoder, so they can all go
        // in first and the pass can be run again against a file the log has
        // already made whole.
        //
        // **On the failure and not before it**, which is what keeps every
        // other outcome the one it was. Running the images unconditionally
        // makes `read_checkpointed_catalog` succeed where it used to fail,
        // which flips `repaired` and seeds this pass with the
        // checkpoint-time catalog rather than the end-of-window one - and
        // `wal_crash`'s commit campaign measured the cost of that: the one
        // cut of twenty-three that reaches the new state stopped reaching
        // it. The repair belongs where the damage is reported.
        //
        // Re-running is sound because redo is idempotent on the page-LSN
        // rule: every record the first attempt applied has stamped its
        // pages with its own LSN, so the second attempt skips it.
        Err(error) if error.code() == inillucent_base::error::PrimaryCode::Corrupt => {
            {
                let mut images = inillucent_txn::redo::Applier::images(
                    database,
                    LearningRows::new_tolerant(&[]),
                );
                inillucent_wal::recover(vfs.as_ref(), db_path, start.clone(), &mut images)
                    .map_err(|why| {
                        let said = why.detail().unwrap_or_default().to_string();
                        why.with_detail(format!("applying the log's page images: {said}"))
                    })?;
            }
            replay(database, vfs, db_path, start, checkpointed, tolerant)
        }
        Err(error) => Err(error),
    }
}

/// Rebuilds a connection's view of a file another process has written, without
/// letting the file go.
///
/// **The whole of task-1979 section 4 in one function.** A connection read the
/// meta record and discovered the log's tail at `open`, under no lock or under
/// a shared one two processes can hold at once, and then trusted both for the
/// rest of its life. Two processes therefore computed the same append position
/// and each wrote over the other's records: 120 acknowledged inserts, 60 rows
/// present, `integrity-check ok`, every process exit 0. Nothing read at `open`
/// before the first lock may be trusted afterwards, so this re-derives all of
/// it - the meta record, the pages, the free map and the log's real tail - from
/// the files, at a moment the caller holds the lock.
///
/// It is deliberately the same sequence `open_file` runs, in the same order and
/// through the same functions, because the question both answer is the same
/// one: what does this file plus the log beside it say right now. The
/// difference is only that the `Database` already exists and keeps its open
/// file - and so keeps the lock, which is the point.
///
/// The caller has already called [`Database::adopt_from_file`], so the cache is
/// empty and the free map is deliberately not loaded: redo may carry the free
/// map's own pages.
///
/// @param database - the connection's file, with its cache already discarded
/// @param vfs - the file system the file and its log live on
/// @param db_path - the database file
/// @param doubtful - transactions whose `Commit` record is not the decision
pub(crate) fn resync_file(
    database: &mut Database,
    vfs: &std::sync::Arc<dyn inillucent_vfs::Vfs>,
    db_path: &DbPath,
    doubtful: &std::collections::BTreeSet<u64>,
) -> DbResult<(std::rc::Rc<Wal>, u64)> {
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
    let mut repaired = false;
    let checkpointed = match read_checkpointed_catalog(database) {
        Ok(checkpointed) => checkpointed,
        Err(_) => {
            repaired = true;
            {
                // **The whole page images first** (task-2000, design 1a). The
                // pass below is the full applier: it replays logical row
                // records, and replaying one means reading the leaf it names.
                // When the page a crash tore is the catalog's own, that read is
                // the thing that fails - so the pass sent to repair it died on
                // the damage it was sent to repair, and the open was refused
                // with `page 3 checksum ... is not the computed ...`. Measured
                // by `wal_crash`'s checkpoint campaign at cut 57, which the
                // rollback journal used to answer by putting the page back
                // before the log was ever read.
                //
                // An images pass needs no catalog and no row decoder - it copies
                // `WritePage`, `CompactLeaf` and split images into their pages
                // and nothing else - so it can go first, and after it every page
                // the log carries whole is whole. It is the same pass
                // `replay_with_repair` runs for the same reason one layer up.
                let mut images = inillucent_txn::redo::Applier::images(
                    database,
                    LearningRows::new_tolerant(&[]),
                );
                inillucent_wal::recover(vfs.as_ref(), db_path, start.clone(), &mut images)
                    .map_err(|why| {
                        let said = why.detail().unwrap_or_default().to_string();
                        why.with_detail(format!("applying the log's page images: {said}"))
                    })?;
            }
            let mut repair =
                inillucent_txn::redo::Applier::new(database, LearningRows::new_tolerant(&[]));
            inillucent_wal::recover(vfs.as_ref(), db_path, start.clone(), &mut repair)?;
            read_checkpointed_catalog(database)?
        }
    };
    let (outcome, free_map) =
        replay_with_repair(database, vfs, db_path, start, &checkpointed, repaired)?;
    for change in &free_map {
        match change.allocated {
            true => database.claim(change.page)?,
            false => database.release(change.page, 1)?,
        }
    }
    inillucent_wal::truncate_after(vfs.as_ref(), db_path, &outcome)?;
    let (next_lsn, sequence) = resume_above_every_stamp(database, &outcome)?;
    let wal = std::rc::Rc::new(Wal::open(
        std::sync::Arc::clone(vfs),
        db_path,
        database.uuid(),
        next_lsn,
        sequence,
        WalOptions::default(),
    )?);
    database.pool().set_durable_lsn(wal.write_ahead_point());
    database
        .pool()
        .set_retained_lsn(database.meta().checkpoint_lsn);
    let_the_pool_ask_the_log(database.pool(), &wal);
    // **The highest transaction number the log holds, so the caller can raise
    // its own counter past it** (task-2000, design 1b). Until the fold became
    // lazy this did not matter: a connection folded on its way out of every
    // statement, so the log beside a file never held more than the statement that
    // had just run, and the numbers in it were this connection's own. Now the log
    // holds every statement since the last fold, from **every** process sharing
    // the file - and two processes each counting their transactions from what
    // they saw at open issue the same numbers.
    //
    // What that costs is not a collision of names. Recovery decides which records
    // to replay by transaction number, so one number used by two processes makes
    // two different transactions into one: a transaction the other process left
    // open at the end of the log is a loser, and discarding it discards this
    // process's committed records that happen to carry the same number. That is a
    // lost write, and it was measured - two processes each inserting five rows
    // into one `ATTACH`ed file acknowledged ten and the file held two.
    //
    // `Recovered::highest_txn` is the field `inillucent-wal` documents for exactly
    // this, and `open` and `ATTACH` already read it. A resynchronisation is the
    // third moment a connection learns what a log holds, and it was the one that
    // did not.
    Ok((wal, outcome.highest_txn))
}

/// Returns a log for a connection that will never write one.
///
/// **On a file system of its own, in memory.** A read only connection still
/// holds a `Wal` because every path through the engine reads one - the durable
/// point the pool is told, the sync policy a pragma reports - and it must not
/// create or extend a segment beside the database. A scratch log on a
/// `MemoryVfs` satisfies the first and cannot do the second: nothing it holds
/// has a name on the disk. A write that reached it is refused earlier, by the
/// pool, and by the file handle under that.
///
/// @param db_path - the database file, so the scratch log is named after it
/// @param uuid - the database's identity
/// @param first_lsn - where the real log ended
/// @param sequence - the segment the real log ended in
fn scratch_log(
    db_path: &DbPath,
    uuid: u128,
    first_lsn: u64,
    sequence: u64,
) -> DbResult<std::rc::Rc<Wal>> {
    let held: std::sync::Arc<dyn inillucent_vfs::Vfs> =
        std::sync::Arc::new(inillucent_vfs::MemoryVfs::new());
    let name = db_path
        .as_path()
        .file_name()
        .map(|part| part.to_string_lossy().into_owned())
        .unwrap_or_else(|| "readonly".to_string());
    Ok(std::rc::Rc::new(Wal::open(
        held,
        &DbPath::new(&name),
        uuid,
        first_lsn,
        sequence,
        WalOptions::default(),
    )?))
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
    open_file_as(vfs, db_path, frames, doubtful, false)
}

/// [`open_file`], with the caller saying whether this connection may write.
///
/// **A read only open writes nothing at all (task-1979, C5 and section 5.2).**
/// It replays the log into its own buffer pool, because that is what makes the
/// rows it reads the committed ones, and it skips every step of an ordinary
/// open that would touch the media: the log is not trimmed to its last valid
/// record, the resume position is not written back, the tail past the header's
/// page count is not given back, and the log it opens is a scratch one in
/// memory rather than a segment beside the database. The file handle itself is
/// read only, so anything that got past all of that is refused by the operating
/// system.
///
/// @param vfs - the file system the file and its log live on
/// @param db_path - the database file
/// @param frames - how many frames the buffer pool holds
/// @param doubtful - transactions whose `Commit` record is not the decision
/// @param read_only - whether this connection may write the file
pub(crate) fn open_file_as(
    vfs: &std::sync::Arc<dyn inillucent_vfs::Vfs>,
    db_path: &DbPath,
    frames: usize,
    doubtful: &std::collections::BTreeSet<u64>,
    read_only: bool,
) -> DbResult<OpenedFile> {
    let database = match read_only {
        true => Database::open_read_only(vfs.as_ref(), db_path, frames.max(64)),
        false => Database::open_before_recovery(vfs.as_ref(), db_path, frames.max(64)),
    }
    .map_err(|error| {
        let said = error.detail().unwrap_or_default().to_string();
        error.with_detail(format!("opening the file before redo: {said}"))
    })?;
    database.refuse_a_page_count_that_cannot_be_addressed()?;

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
    let start = where_recovery_starts(&database, doubtful);
    let mut database = database;
    let (checkpointed, repaired) = catalog_before_redo(&mut database, vfs, db_path, &start)?;
    let (outcome, free_map) =
        replay_with_repair(&mut database, vfs, db_path, start, &checkpointed, repaired)?;
    // The free map is rebuilt after the scan rather than inside it: the map and
    // every page write are both behind `&mut Database`, and one record cannot
    // hold two mutable borrows of the same object.
    //
    // **In log order.** Claiming every allocation and then
    // releasing every free gave the frees the last word, so a page freed and
    // allocated again inside the replayed range came back free while it was
    // live, and the next allocation handed it to a second owner. See
    // `Applier::free_map_changes`.
    apply_free_map_changes(&mut database, &free_map)?;
    if !read_only {
        inillucent_wal::truncate_after(vfs.as_ref(), db_path, &outcome)?;
    }
    // **And the file's own header is made to describe the file.** A
    // transaction that grew the file and then did not become durable leaves a
    // file longer than the meta record claims: the pool grows the file when it
    // writes a page past the end, the meta record is written last on purpose,
    // and the rollback journal restores a page's contents and says nothing
    // about the file's length. Recovery has just settled what the file holds,
    // so this is the moment the two can be made to agree - and they have to,
    // because a header that under-counts is a file whose tail nothing will ever
    // reclaim and nothing can account for.
    //
    // The checkpoint runs only when the count has moved, so an ordinary open of
    // a clean file writes nothing. `durability.rs`'s
    // `a_recovered_database_is_no_longer_than_its_header_says` is what measures
    // it, at every call of a growing transaction (task-1980).
    if !read_only && database.pool().page_count() != database.meta().page_count {
        database.checkpoint()?;
    }
    // **The trim that used to follow waits until the open has succeeded**
    // (task-2070). See `crate::engine::open::header_accounts_for_every_object`,
    // which says what it cost to do it here.

    // **The log resumes where recovery ended, not at the beginning.** Opening it
    // at `FIRST_LSN` with sequence 1 starts a second stream over the same
    // segments: the session writes records the *next* open cannot find, because
    // the meta page's checkpoint points into the first stream. A test caught it
    // as a table created after an open vanishing on the one after that -
    // `no such table: second` from a file that had just been told to make it.
    //
    // **And above every stamp the file carries.** See
    // `resume_above_every_stamp`.
    let (next_lsn, sequence) = match read_only {
        // `resume_above_every_stamp` checkpoints when the file's pages are
        // stamped above where the log ended, which a read only connection
        // cannot do and does not need: nothing it does will write a record.
        true => (outcome.next_lsn.max(FIRST_LSN), outcome.sequence.max(1)),
        false => resume_above_every_stamp(&mut database, &outcome)?,
    };
    let wal = match read_only {
        true => scratch_log(db_path, database.uuid(), next_lsn, sequence)?,
        false => std::rc::Rc::new(Wal::open(
            std::sync::Arc::clone(vfs),
            db_path,
            database.uuid(),
            next_lsn,
            sequence,
            WalOptions::default(),
        )?),
    };
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
    let catalog_tree =
        attach_catalog(database.pool(), database.catalog_root()).map_err(|error| {
            let said = error.detail().unwrap_or_default().to_string();
            error.with_detail(format!("attaching the catalog after redo: {said}"))
        })?;
    // **What this open did, kept rather than dropped (task-1979, C10).** The
    // numbers are the scan's own counters and the stray search is a fixed
    // handful of `access` calls, so an open that recovered nothing pays for a
    // report that says so.
    let recovery = RecoveryReport {
        // **A transaction put back or thrown away, not a record read.** Every
        // open scans at least the checkpoint record that told it where to
        // start, so `applied` is above zero on a file nothing has ever crashed
        // on; what an operator is asking is whether this open had work to
        // restore.
        recovered: outcome.committed > 0 || outcome.losers > 0,
        scanned: outcome.scanned,
        applied: outcome.applied,
        dropped: outcome.dropped,
        committed: outcome.committed,
        losers: outcome.losers,
        last_sequence: sequence.max(outcome.sequence),
        last_lsn: next_lsn,
    };
    Ok(OpenedFile {
        database,
        wal,
        catalog_tree,
        recovery,
        highest_txn: outcome.highest_txn,
    })
}

// **The replay applier lives here rather than in `lib.rs` (task-1932).** It is
// the half of recovery that decides what a log record means, and `open_file`
// above is the half that decides which records to replay; they were seven
// hundred lines apart in a file of nearly eight thousand. Nothing in them
// changed in the move.

/// A [`RowRedo`] that learns a tree's shape from the catalog rows it replays.
///
/// **The problem it solves.** `TreeRows` has to be told every tree's column
/// directory before the replay starts, and the only place to get one is the
/// catalog. But the catalog a reader can read before the replay is the catalog
/// as at the *last checkpoint* - so a table created after it, and then written
/// to, names a tree the applier has never heard of, and the replay refuses.
/// That is not a corner: a database that is created, given a schema and filled
/// without ever being checkpointed is the ordinary shape of a crash, and it is
/// exactly what `ImportedDatabase::create` followed by DDL produces.
///
/// **Why it works.** A `CREATE TABLE` is a row inserted into the catalog tree,
/// and the log is replayed in LSN order - so that row goes past *before* any row
/// of the tree it describes. Watching the catalog tree go by is therefore enough
/// to know every shape by the time it is needed, and it needs no second pass.
///
/// The catalog row is decoded by `inillucent-catalog`'s own decoder rather than
/// here, because a second decoder is a second opinion about which column is
/// which, and the columns are what the format is.
///
/// ## `seen` is a catalog, not a list of the rows that went past
///
/// It used to be the second, and that made a database unopenable after an
/// ordinary migration. `ALTER TABLE chunk ADD COLUMN embedded_at INTEGER`
/// rewrites the table's catalog row - a `DeleteRow` and an `InsertRow` - so a
/// list that only appends held **both** definitions, and `shape_of` reads the
/// owner table with `find`, which answers with the first. Every shape derived
/// after that ALTER therefore came from the definition before it.
///
/// What that costs is not a missing column in a report. `CREATE INDEX
/// chunk_embedded_at_idx ON chunk (embedded_at)` against a table with no
/// `embedded_at` resolves the key to no column at all, and `index_shape` gives
/// an unresolved key column `PhysicalType::Any` where the writer used
/// `PhysicalType::Int64`. An `Any` mini-column is wider, so recovery repacks the
/// index's leaves less densely than the process that wrote them - and the first
/// logical `CompactLeaf` replayed against such a leaf cannot fit rows that
/// demonstrably fitted when they were written. The open fails with
/// `database disk image is malformed`, and the whole database is unreachable
/// while the log is beside it.
///
/// It was measured on Nikaya's real 5.8 GB corpus: `chunk`'s catalog row was
/// rewritten at LSN 21,934,260,592 and the index's row written at
/// 21,936,659,792, both after the last checkpoint at 21,074,969,552; recovery
/// then refused a compaction of index leaf 237505 that held 2,031 live rows.
/// Replaying that page's records out of the parked log with the shape read off
/// the page fits every one of them, and with the first key column forced to
/// `Any` it fails at the same LSN with the same 2,031 rows.
///
/// So a row is *remembered* rather than appended: an entry replaces the one it
/// supersedes, by name and by tree identifier, and every shape is derived again
/// from the catalog as it now stands. Deriving them all again rather than only
/// the one that changed is what makes an ALTER reach the indexes on the table -
/// their own rows may have gone past already, and their shapes come from the
/// table's text rather than from their own.
struct LearningRows {
    /// The applier this delegates to, gaining trees as it goes.
    rows: TreeRows,
    /// The catalog as it now stands: at most one entry per object.
    seen: Vec<SchemaEntry>,
    /// How many records this pass dropped because it had no shape for the
    /// tree they name.
    ///
    /// **A drop used to be invisible** (task-2066 §4.1.10).
    /// `tolerate_unknown_tree` answers `Ok(())`, and `inillucent-wal`'s replay
    /// loop counts an `Ok` as applied - so a dropped record was reported as an
    /// applied one, `Recovered` had no field for it, and nothing printed it.
    /// task-1932 and task-2033 are both defects that lived inside that silence:
    /// the second lost an inserted row and 487 FTS5 records with
    /// `PRAGMA integrity_check` answering `ok` on the result.
    dropped: u64,
    /// The trees a fresh read of the catalog has already been made for.
    ///
    /// See [`LearningRows::refresh_from_the_file`]: the read is a walk of the
    /// schema tree and its answer is the same for every record naming the same
    /// tree, so it is made once each.
    refreshed_for: Vec<u64>,
    /// Whether a record naming a tree this pass has no shape for is skipped
    /// rather than refused.
    ///
    /// Set for the repair pass `open_file` runs when the checkpointed catalog
    /// itself could not be read - see the module-level note above `open_file`
    /// on the catalog/redo circularity - **and for the second pass that
    /// follows one** (task-1932). That pass cannot yet know the shape of a
    /// tree whose `CREATE TABLE` predates the redo window, so a record naming
    /// one is not damage, it is a shape this pass was never going to have;
    /// refusing it would fail an open a second, catalog-aware pass is about to
    /// repair. What this pass exists to fix - the schema tree's own pages -
    /// has no such gap: `TreeRows::new` always knows `schema_layout()`,
    /// tolerant or not.
    ///
    /// The second pass needs it for the opposite reason: the catalog it is
    /// seeded with was read after the repair pass replayed the whole window,
    /// so it describes the END of the log while the records run from the start
    /// of it, and a tree superseded inside the window - every `REINDEX` and
    /// every `CREATE INDEX` allocates a fresh one - is named by no row it will
    /// ever see. An ordinary open, where the checkpointed catalog read, stays
    /// strict.
    tolerant: bool,
}

impl LearningRows {
    /// Returns an applier for the repair pass, told nothing but the schema
    /// tree's own fixed shape and asked to skip what it cannot yet decode.
    ///
    /// @param checkpointed - the catalog as at the last checkpoint, empty when
    ///   even that could not be read
    fn new_tolerant(checkpointed: &[SchemaEntry]) -> LearningRows {
        LearningRows::new_with_tolerance(checkpointed, true)
    }

    /// Returns an applier that already knows a catalog, tolerant or not.
    ///
    /// @param checkpointed - the catalog the pass starts from
    /// @param tolerant - whether an unknown tree is skipped rather than refused
    pub(crate) fn new_with_tolerance(checkpointed: &[SchemaEntry], tolerant: bool) -> LearningRows {
        let mut learning = LearningRows {
            dropped: 0,
            rows: TreeRows::new().with_tree(
                inillucent_catalog::paged::SCHEMA_TREE_ID,
                schema_layout(),
                1,
            ),
            seen: checkpointed.to_vec(),
            refreshed_for: Vec::new(),
            tolerant,
        };
        learning.derive_every_shape();
        learning
    }

    /// Learns a tree's shape from a catalog row the replay is about to apply.
    ///
    /// Silent about a row it cannot make a shape of - a view, a trigger, an
    /// index whose table has not gone past yet - because the applier refuses by
    /// name if a record then needs it, and refusing there says which tree.
    ///
    /// @param row - the catalog row's encoded values
    fn learn(&mut self, row: &[u8]) {
        let Ok(values) = decode_row(row) else {
            return;
        };
        let Ok(entry) = inillucent_catalog::paged::entry_from_row(&values) else {
            return;
        };
        self.remember(entry);
        self.derive_every_shape();
    }

    /// Puts one catalog entry in place of the one it supersedes.
    ///
    /// Matched on the folded name **and** on the tree identifier: an
    /// `ALTER TABLE ... ADD COLUMN` rewrites the row under the same name, and an
    /// `ALTER TABLE ... RENAME TO` rewrites it under a new name with the same
    /// identifier. Both leave one entry behind, which is what the rest of this
    /// type assumes.
    ///
    /// @param entry - the entry the replay just read
    fn remember(&mut self, entry: SchemaEntry) {
        let name = entry.name.to_ascii_lowercase();
        let kind = entry.kind;
        let identifier = entry.tree_id;
        self.seen.retain(|held| {
            let same_name = held.kind == kind && held.name.to_ascii_lowercase() == name;
            let same_tree = identifier != 0 && held.tree_id == identifier;
            !same_name && !same_tree
        });
        self.seen.push(entry);
    }

    /// Derives every tree's shape again from the catalog as it now stands.
    ///
    /// Every one of them, not only the entry that changed: an index's columns
    /// come from its *table's* declaration, so a table whose row was just
    /// rewritten changes the shape of indexes whose own rows went past earlier.
    fn derive_every_shape(&mut self) {
        let mut rows = std::mem::take(&mut self.rows);
        for entry in &self.seen {
            let Ok(identifier) = identifier_of(entry) else {
                continue;
            };
            let Some((columns, key_columns)) = shape_of(entry, &self.seen, identifier) else {
                continue;
            };
            rows = rows.with_tree(u64::from(identifier), columns, key_columns);
        }
        self.rows = rows;
    }
}

/// Decodes a run of tagged values, which is how a row record carries a row.
///
/// @param row - the record's row bytes
fn decode_row(row: &[u8]) -> DbResult<Vec<Datum<'_>>> {
    let mut values = Vec::new();
    let mut at = 0usize;
    while at < row.len() {
        let (value, width) = Datum::decode_tagged(row.get(at..).unwrap_or(&[]))?;
        values.push(value);
        at = at.saturating_add(width);
    }
    Ok(values)
}

impl RowRedo for LearningRows {
    fn insert_row(
        &mut self,
        database: &mut Database,
        tree: u64,
        page: PageId,
        row: &[u8],
        lsn: u64,
    ) -> DbResult<()> {
        if tree == inillucent_catalog::paged::SCHEMA_TREE_ID {
            self.learn(row);
        }
        let result = self.rows.insert_row(database, tree, page, row, lsn);
        self.retry_after_reading_the_catalog(database, tree, result, |rows, database| {
            rows.insert_row(database, tree, page, row, lsn)
        })
    }

    fn delete_row(
        &mut self,
        database: &mut Database,
        tree: u64,
        page: PageId,
        key: &[u8],
        lsn: u64,
    ) -> DbResult<()> {
        let result = self.rows.delete_row(database, tree, page, key, lsn);
        self.retry_after_reading_the_catalog(database, tree, result, |rows, database| {
            rows.delete_row(database, tree, page, key, lsn)
        })
    }

    fn update_in_place(
        &mut self,
        database: &mut Database,
        tree: u64,
        page: PageId,
        key: &[u8],
        column: u32,
        value: &[u8],
        lsn: u64,
    ) -> DbResult<()> {
        let result = self
            .rows
            .update_in_place(database, tree, page, key, column, value, lsn);
        self.retry_after_reading_the_catalog(database, tree, result, |rows, database| {
            rows.update_in_place(database, tree, page, key, column, value, lsn)
        })
    }

    fn compact_leaf(
        &mut self,
        database: &mut Database,
        tree: u64,
        page: PageId,
        lsn: u64,
        from_lsn: u64,
    ) -> DbResult<()> {
        let result = self.rows.compact_leaf(database, tree, page, lsn, from_lsn);
        self.retry_after_reading_the_catalog(database, tree, result, |rows, database| {
            rows.compact_leaf(database, tree, page, lsn, from_lsn)
        })
    }
}

impl LearningRows {
    /// Turns "this pass was never told the shape of that tree" into success,
    /// for a tree the catalog does not name and on the repair pass.
    ///
    /// **A tree no catalog row names has been dropped, and its records are for
    /// pages that belong to something else now (task-1932).** Every rebuild
    /// allocates a fresh tree and moves the catalog row onto it - `REINDEX`
    /// does, and so does `CREATE INDEX` - so a replay window that spans one
    /// holds records naming a tree the catalog at the end of it has no row for.
    /// Refusing those failed the open outright, which meant a database that had
    /// survived a crash during a `REINDEX` **could not be opened at all**:
    ///
    /// ```text
    /// bad parameter or other API misuse: the log names tree 2147483649,
    /// which this recovery was not told the shape of
    /// ```
    ///
    /// `reindex_crash.rs` found it, deterministically, at the fifty-fifth cut
    /// of both journal modes.
    ///
    /// **What makes skipping right rather than merely convenient** is that the
    /// log names a tree before it describes any of its pages: `create_index`
    /// and `rebuild_index` both write the catalog row naming the new tree
    /// before they fill it, in the same transaction and superseded by the row
    /// at the end of the statement. So a record for a tree with no row is never
    /// one whose row has not gone past yet - it is a tree that has been
    /// dropped, and writing its pages back would overwrite whatever owns them
    /// now.
    ///
    /// **Never for the schema tree.** Its shape is fixed
    /// (`with_tree(SCHEMA_TREE_ID, schema_layout(), 1)` in every constructor),
    /// so a refusal naming it is never this gap - it is a genuinely damaged
    /// catalog row, and has to be refused the way it always was. And never for
    /// any other failure a row's own redo can raise - a bad key, a page that is
    /// not a leaf - which name a real defect this pass must not hide. A tree
    /// the catalog *does* name and whose shape could not be derived is still
    /// refused, which is the gap `autoindex_reopen.rs` and `analyze_reopen.rs`
    /// were written for.
    ///
    /// @param tree - the tree the record named
    /// @param result - what the delegated redo answered
    fn tolerate_unknown_tree(&mut self, tree: u64, result: DbResult<()>) -> DbResult<()> {
        let named = self.seen.iter().any(|entry| entry.tree_id == tree);
        if (!self.tolerant && named) || tree == inillucent_catalog::paged::SCHEMA_TREE_ID {
            return result;
        }
        match result {
            Err(error) if is_an_unknown_tree(&error) => {
                // Counted here rather than at the caller, because this is the
                // one place a record stops being applied and starts being
                // forgotten. See `LearningRows::dropped`.
                self.dropped = self.dropped.saturating_add(1);
                Ok(())
            }
            other => other,
        }
    }

    /// Returns how many records this pass dropped for want of a tree's shape.
    fn dropped(&self) -> u64 {
        self.dropped
    }

    /// Reads the catalog off the file's own pages and learns whatever it says.
    ///
    /// **Because a catalog row does not always go past as a row (task-2033).**
    /// Everything this type knows, it learned from the `InsertRow` records
    /// naming the schema tree, and that is only every catalog row while every
    /// `CREATE TABLE`'s row reaches the log as one. It does not: the write path
    /// can place a row by rebuilding the leaf around it, which reaches the log
    /// as a page image, and a bulk build writes its pages to the file before
    /// the record that names them, which puts nothing in the log at all. A row
    /// that arrived either way left this type not knowing the table existed, so
    /// the rows written into it afterwards named a tree with no shape and
    /// `tolerate_unknown_tree` dropped them - silently, with
    /// `PRAGMA integrity_check` answering `ok` on the result. At a 512-byte
    /// page, three `CREATE TABLE`s and an `INSERT` in one transaction lost the
    /// inserted row, and an FTS5 index lost 487 of its records.
    ///
    /// Reading the file is what covers all of those at once, because the
    /// question is what the catalog *says*, not how each row got there. It is
    /// asked only when a record names a tree with no shape - never on an
    /// ordinary write - and a catalog that will not read yet is not an error
    /// here: the pages the log carries whole may not have been put back, and
    /// the caller's existing answer for an unknown tree still applies.
    ///
    /// **Asked at most once per tree**, because the answer does not change
    /// between two records naming the same one and reading the catalog is a
    /// walk of the schema tree. A recovery that spans a `REINDEX` holds many
    /// records for the tree the rebuild dropped, and re-reading the catalog
    /// for each of them would turn a scan into a scan per record.
    ///
    /// @param database - the file being replayed into
    /// @param tree - the tree the record named
    /// @returns whether the catalog now names a tree it did not name before
    fn refresh_from_the_file(&mut self, database: &Database, tree: u64) -> bool {
        if self.refreshed_for.contains(&tree) {
            return false;
        }
        // **Marked as asked only once the catalog was actually read**
        // (task-2066 §4.1.10). This used to push the tree before trying, so a
        // read that failed still counted as an attempt - and a catalog that
        // cannot be read yet is the ordinary case for the record this exists
        // for: a record naming a new tree can precede the page image its
        // catalog row lives on. The tree was then marked refreshed for the rest
        // of the recovery, and every later record naming it was dropped without
        // another attempt.
        //
        // That is the task-2033 defect one level up. The repair is made too
        // early and the bookkeeping guarantees there is no second one.
        //
        // The cost argument the comment above makes still holds: this is asked
        // once per tree *per successful catalog read*, and a read that fails
        // fails at `attach_catalog`, which is a page fetch rather than a walk.
        let Ok(schema) =
            inillucent_catalog::paged::attach_catalog(database.pool(), database.catalog_root())
        else {
            return false;
        };
        let Ok(entries) = inillucent_catalog::paged::read_catalog(database.pool(), &schema) else {
            return false;
        };
        self.refreshed_for.push(tree);
        let mut fresh = false;
        for entry in entries {
            fresh = fresh || !self.seen.iter().any(|held| held.tree_id == entry.tree_id);
            self.remember(entry);
        }
        if fresh {
            self.derive_every_shape();
        }
        fresh
    }

    /// Runs a record's redo again once the file's own catalog has been read.
    ///
    /// The first attempt is the cheap one: almost every record names a tree
    /// whose `CREATE TABLE` went past as a row, and those never reach this.
    ///
    /// @param database - the file being replayed into
    /// @param tree - the tree the record named
    /// @param first - what the first attempt answered
    /// @param again - runs the record's redo a second time
    fn retry_after_reading_the_catalog(
        &mut self,
        database: &mut Database,
        tree: u64,
        first: DbResult<()>,
        again: impl FnOnce(&mut TreeRows, &mut Database) -> DbResult<()>,
    ) -> DbResult<()> {
        match first {
            Err(error) if is_an_unknown_tree(&error) => {
                if self.refresh_from_the_file(database, tree) {
                    let mut rows = std::mem::take(&mut self.rows);
                    let second = again(&mut rows, database);
                    self.rows = rows;
                    return self.tolerate_unknown_tree(tree, second);
                }
                self.tolerate_unknown_tree(tree, Err(error))
            }
            other => self.tolerate_unknown_tree(tree, other),
        }
    }
}

/// Reports whether a redo failed only because this pass has no shape for the
/// tree the record named.
///
/// Matched on the sentence `TreeRows` raises, which is the one failure a fresh
/// read of the catalog can answer. Every other failure - a bad key, a page that
/// is not a leaf - names a real defect and is never retried or tolerated.
///
/// @param error - what the redo answered
fn is_an_unknown_tree(error: &inillucent_base::error::DbError) -> bool {
    error
        .detail()
        .is_some_and(|detail| detail.ends_with("which this recovery was not told the shape of"))
}

/// Returns a catalog entry's tree shape, for the recovery applier.
///
/// The same derivations the open uses below, in the one form the applier wants:
/// the column directory and how many leading columns form the key. An entry
/// whose declaration will not parse - or an index whose table is not in the
/// catalog - answers `None`, and the applier then refuses any record naming it
/// rather than replaying into a shape it guessed.
///
/// @param entry - the catalog row
/// @param catalog - every row, so an index can find its table
/// @param identifier - the tree's identifier
fn shape_of(
    entry: &SchemaEntry,
    catalog: &[SchemaEntry],
    identifier: u32,
) -> Option<(Vec<crate::ColumnSpec>, usize)> {
    match entry.kind {
        ObjectKind::Table => {
            let mut info = table_from_create_sql(&entry.sql, 0, identifier).ok()?;
            info.root = identifier;
            if info.without_rowid {
                let (columns, key_columns, _) = keyed_table_shape(&info).ok()?;
                Some((columns, key_columns))
            } else {
                let (columns, _) = table_shape(&info);
                Some((columns, 1))
            }
        }
        ObjectKind::Index => {
            let folded = entry.table.to_ascii_lowercase();
            // **The newest matching entry, not the first.** `LearningRows`
            // keeps one entry per object, so there is only ever one - but a
            // caller that hands this a catalog holding a superseded definition
            // as well should get the definition that superseded it, because the
            // shape derived from the older one is what made a database
            // unopenable after an `ALTER TABLE`.
            let owner = catalog.iter().rev().find(|held| {
                held.kind == ObjectKind::Table && held.name.to_ascii_lowercase() == folded
            })?;
            let mut table = table_from_create_sql(&owner.sql, 0, owner.tree_id as u32).ok()?;
            table.root = owner.tree_id as u32;
            // **An automatic index is declared by the *table's* text.** The
            // catalog stores an empty `sql` for one - which is what SQLite
            // writes for `sqlite_autoindex_t_1` - so parsing that empty text as
            // a `CREATE INDEX` answers nothing, the applier is told no shape,
            // and every log record naming the index is refused. That is a
            // database with a `TEXT PRIMARY KEY` that cannot be reopened after
            // a write, and it is what this branch exists to prevent; the same
            // rule is applied by `ImportedDatabase::shape_of` when the schema
            // is loaded.
            let index = if entry.sql.is_empty() {
                let folded = entry.name.to_ascii_lowercase();
                table
                    .indexes
                    .iter()
                    .find(|index| index.folded == folded)?
                    .clone()
            } else {
                inillucent_catalog::load::index_from_create_sql(&entry.sql, &table, identifier)
                    .ok()?
            };
            let (columns, _) = index_shape(&table, &index, identifier);
            let key_columns = columns.len();
            Some((columns, key_columns))
        }
        _ => None,
    }
}

/// Returns the identifier a catalog row registers its tree under.
///
/// **Refused rather than defaulted.** A zero here is a row written before the
/// identifier was persisted, and guessing one would put the tree back in the
/// state this change exists to leave: a number the writer did not use, which
/// recovery would follow to the wrong tree. A file that does not say is a file
/// this engine will not open.
///
/// @param entry - the catalog row
pub(crate) fn identifier_of(entry: &SchemaEntry) -> DbResult<u32> {
    if entry.tree_id == 0 {
        return Err(refusal(format!(
            "the catalog row for {} carries no tree identifier; the database was created before catalog rows carried one and has to be rebuilt",
            String::from_utf8_lossy(&entry.name)
        )));
    }
    u32::try_from(entry.tree_id).map_err(|_| {
        refusal(format!(
            "the catalog row for {} carries a tree identifier that does not fit",
            String::from_utf8_lossy(&entry.name)
        ))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds an error of the shape `tolerate_unknown_tree` recognises.
    ///
    /// Assembled from the same sentence `is_an_unknown_tree` matches on, so the
    /// two cannot drift apart without this failing.
    ///
    /// @param tree - the tree the record named
    fn an_unknown_tree(tree: u64) -> inillucent_base::DbError {
        inillucent_base::error::corrupt(format!(
            "the log names tree {tree}, which this recovery was not told the shape of"
        ))
    }

    /// A tolerated drop moves the counter.
    ///
    /// **A counter only ever asserted to be zero is not a counter** (task-2066
    /// §4.4.12), and the whole of §4.1.10 is a number that could not move: the
    /// drop answers `Ok(())`, `inillucent-wal` counts an `Ok` as an applied
    /// record, and `Recovered` had no field for the difference. So the drop is
    /// handed to the function directly here.
    ///
    /// It is a unit test rather than a recovery, because the recovery shapes
    /// that can be built from SQL do not produce one: a `CREATE TABLE` reaches
    /// the log as a row naming the schema tree and `LearningRows` learns the
    /// shape from it. What produces a drop is a catalog row that arrived as a
    /// page image or through a bulk build, which is the case task-2033 found -
    /// and `a_table_created_and_dropped_inside_the_window_replays_without_a_drop`
    /// records that the ordinary shape does not.
    #[test]
    fn a_tolerated_drop_is_counted() {
        let mut rows = LearningRows::new_tolerant(&[]);
        assert_eq!(rows.dropped(), 0, "a fresh applier has dropped nothing");

        let tolerated = rows.tolerate_unknown_tree(7, Err(an_unknown_tree(7)));
        assert!(
            tolerated.is_ok(),
            "the record was refused rather than tolerated, so this is testing the wrong branch"
        );
        assert_eq!(rows.dropped(), 1, "the tolerated drop was not counted");

        let again = rows.tolerate_unknown_tree(9, Err(an_unknown_tree(9)));
        assert!(again.is_ok());
        assert_eq!(rows.dropped(), 2, "the second drop was not counted");
    }

    /// A record that succeeds, and one refused for another reason, count nothing.
    ///
    /// The falsifier. Without it the case above passes against a counter that
    /// increments on every record, which would make the number useless in the
    /// other direction - every recovery would look like it had dropped rows.
    #[test]
    fn nothing_but_a_tolerated_drop_is_counted() {
        let mut rows = LearningRows::new_tolerant(&[]);
        assert!(rows.tolerate_unknown_tree(7, Ok(())).is_ok());
        assert_eq!(rows.dropped(), 0, "an applied record was counted as a drop");

        let other = rows.tolerate_unknown_tree(
            7,
            Err(inillucent_base::error::corrupt("a page that is not a leaf")),
        );
        assert!(
            other.is_err(),
            "a failure that is not an unknown tree was tolerated, which would hide a real defect"
        );
        assert_eq!(
            rows.dropped(),
            0,
            "a refusal that was passed through was counted as a drop"
        );
    }

    /// And the schema tree is never tolerated, so it is never counted.
    ///
    /// Its shape is fixed in every constructor, so a refusal naming it is a
    /// damaged catalog row rather than this gap - which is a rule the counter
    /// must not quietly weaken.
    #[test]
    fn the_schema_tree_is_never_a_tolerated_drop() {
        let mut rows = LearningRows::new_tolerant(&[]);
        let schema = inillucent_catalog::paged::SCHEMA_TREE_ID;
        let refused = rows.tolerate_unknown_tree(schema, Err(an_unknown_tree(schema)));
        assert!(
            refused.is_err(),
            "a record naming the schema tree was tolerated"
        );
        assert_eq!(rows.dropped(), 0, "the schema tree's refusal was counted");
    }
}
