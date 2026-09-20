//! Checkpointing `ImportedDatabase`: writing every dirty page out and moving
//! the log's recovery point behind it.
//!
//! Invariant: **the recorded recovery point never sits above the oldest
//! change any currently-dirty, not-yet-written page still needs.**
//! `Pool::writeback`'s no-steal rule (`holds_uncommitted`) keeps an open
//! transaction's dirty pages out of the file; a checkpoint that recorded the
//! log's durable end as the recovery point regardless would tell recovery it
//! can start above records those held-back pages still need.
//!
//! **This used to read only `Pool::uncommitted_lsn`, bounding recovery by the
//! oldest open transaction's first record and nothing else - which is wrong,
//! not merely narrower than `Pool::oldest_dirty_lsn`.** A held-back page does
//! not have to hold the *open* transaction's own change to be held back: an
//! autocommit statement can write page P and commit; a second connection can
//! then open a transaction and write P again, *before either has been
//! checkpointed*; a checkpoint now finds P dirty and above the open
//! transaction's watermark, so `holds_uncommitted` correctly leaves P out of
//! the file - but `uncommitted_lsn` alone names the *open* transaction's
//! first record, which is *above* the autocommit statement's record. Recovery
//! then starts above a record it still needs, P is not in the file (held
//! back) and not replayed (the record is skipped), and the autocommit
//! statement's row is gone after a crash - reproduced directly: schema, three
//! rows, close, reopen; a fourth row inserted and committed; `BEGIN; UPDATE
//! ...` left open; `PRAGMA user_version = 7` (which checkpoints without
//! refusing, unlike `PRAGMA wal_checkpoint`) then a crash: the table is gone
//! on reopen. `Pool::oldest_dirty_lsn`'s own doc comment already says what is
//! needed - "the lowest LSN recovery must start at to rebuild every dirty
//! page" - and that is every dirty page, not only the open transaction's own.
//!
//! **Fixing the bound alone was not enough - `Pool::oldest_dirty_lsn` itself
//! had a second, independent defect, and both had to be fixed before the
//! reproduction above actually passed.** A page's `rec_lsn` (the LSN it
//! carried when it went dirty) is only refreshed when the page is next
//! modified, not on every checkpoint; a leaf page can sit untouched through
//! many checkpoints while `retire_segments_below` keeps deleting segments
//! below each one's own, much higher, recovery point. The page is still
//! correct on disk the whole time - nothing has changed about it - but the
//! stamp it is carrying is now a lie about what the log still holds. The
//! moment such a page dirties again, `Pool::note_dirty_from` used to record
//! that stale stamp as `rec_lsn` verbatim, and this checkpoint would then ask
//! recovery to start from a point already retired out of existence - which
//! this same reproduction hit even after the bound below was fixed, because
//! `oldest_dirty` itself came back stale. The fix lives in
//! `Pool::note_dirty_from`: it floors `rec_lsn` at `Pool::retained_lsn`, the
//! lowest point any checkpoint has actually persisted and retired segments
//! against, which this function and `checkpoint_attached` both update. See
//! that function's own doc comment for why the floor has to be `retained_lsn`
//! and not the more obvious-looking `durable_lsn`.
//!
//! **A third, separate defect in `Pool::oldest_dirty_lsn` broke every ordinary
//! checkpoint with no transaction open at all, and this same fix uncovered
//! it.** The function used to fold in every dirty page's `rec_lsn`, not only
//! the ones this checkpoint's flush is actually going to hold back. With no
//! transaction open, `holds_uncommitted` answers false for every page - the
//! flush below writes all of them - so nothing should bound `recovery_from`
//! below `durable` at all. But a freshly allocated page can be dirty with its
//! header LSN still at its zeroed, never-stamped default, and folding that in
//! pinned `recovery_from` at zero on every such checkpoint forever:
//! `retire_segments_below` then had nothing beneath the pinned point to
//! reclaim, and a build that should shrink its log to a few kilobytes never
//! shed a single segment. Measured directly by
//! `a_checkpoint_reclaims_the_log` in `inillucent-compat`'s
//! `new_engine_log_retire.rs`, which this fix's first pass broke and which
//! `analyze_reopen.rs` and `services.rs` broke alongside it, all for the same
//! reason. The fix is in `oldest_dirty_lsn` itself: with no transaction open
//! it returns `u64::MAX` outright, and otherwise only counts a page whose
//! *current* stamp is at or above `uncommitted_lsn` - the same condition
//! `holds_uncommitted` checks, so a page this checkpoint will not hold back
//! cannot pull the bound down for no reason.
//!
//! **Read before `log_free_map_pages`, and without an extra `flush()`.**
//! `inillucent_txn::engine::Engine::checkpoint` - the engine that does not
//! ship - takes this same minimum, guarded by an explicit `flush()` first;
//! that shape corrupted the rollback-journal campaigns here
//! (`power_loss_at_every_cut_point_of_a_checkpoint` and its
//! `truncate`/`persist` siblings failed with "database disk image is
//! malformed"), because it puts a second `flush()` - and so a second round of
//! `journal_page`/`seal_journal` calls, since `ImportedDatabase` runs those
//! under a real rollback journal and `inillucent-txn`'s `Engine` never does -
//! ahead of the one `checkpoint_after_free_map` already makes. No second
//! `flush()` is needed here: a page `holds_uncommitted` will hold back was
//! already dirty before this function was ever called - by the very
//! transaction whose write made it so - so its `rec_lsn` is already correct
//! to read. Reading it *before* `log_free_map_pages` matters for a different
//! reason: `log_free_map_pages` dirties the free map's own pages, and those
//! carry whatever stale `rec_lsn` they last held from a much earlier write,
//! which would pull the bound down for no reason - they are logged and
//! flushed unconditionally by this same checkpoint regardless of any open
//! transaction, so they were never a page recovery could be missing.
//!
//! Extracted from `lib.rs`, whose recorded size this pushed past.

use inillucent_base::DbResult;

use crate::ImportedDatabase;

/// **`CheckpointKind` is gone and the distinction it carried is not** (task-2000,
/// design 1b). The enum had two variants, `Asked` and `Releasing`, and what they
/// told apart is whether the log's housekeeping runs - the catalog's statistics, a
/// rolled segment, a checkpoint record and the retirement of the segments the fold
/// made redundant. That is a yes or no about one call, so it is a `bool` parameter
/// on `ImportedDatabase::checkpoint_of` now rather than a type; nothing about which
/// half of a fold runs when has changed.
///
/// Design 1b's first cut *did* retire the distinction, on the argument that a
/// release no longer folds at all in `wal` mode - so by the time one runs the log
/// has passed [`RECLAIM_BYTES`] and the housekeeping is due anyway. The argument is
/// right about `wal` and wrong about everything else: the lazy fold is `wal`'s,
/// because the after images in the log are what make an interrupted one repairable,
/// and `journal_mode = delete` and its siblings still fold on the way out of every
/// statement. Making every one of those folds do the whole of a checkpoint is
/// exactly the 20x task-1999 measured and removed.
///
/// How much log may pile up before a statement letting the file go reclaims it
/// on its way out.
///
/// **It has to be small, and the first cut of task-1999 got that wrong.** The
/// reclamation was put behind `Wal::checkpoint_due`, whose bar is 256 MiB -
/// which answers "is a whole checkpoint overdue", a different question from "has
/// enough piled up that the statement already folding its pages may as well
/// reclaim while it is here". Nothing in this engine checkpointed when a
/// connection closed: `ImportedDatabase` had no `Drop`, and the command line, the
/// shell and the driver all left it to a caller. Until task-1999's change the
/// per-statement checkpoint hid that, because it reclaimed every statement.
///
/// **It has a `Drop` now** (task-2000, design 1b): with the fold lazy in `wal` mode,
/// a connection that went away with less than this much log behind it would leave a
/// file that needs its log read, and a closed `.rdb` being self contained is what
/// `inillucent backup` and anybody copying one rely on. The bar still decides
/// everything else, and it is still the reason it has to be small.
///
/// The same run measured at three bars - 4,000 autocommit statements through
/// `inillucent-shell` against a 320 KB database, and what was on disk after the
/// process exited:
///
/// | bar | log left behind | the run |
/// |---|---|---|
/// | 256 MiB, the first cut | 129.7 MB in two segments | 72.8 s |
/// | 32 MiB, a thousand 32 KiB pages | 30.1 MB in one | 59.3 s |
/// | **4 MiB, this** | **3.3 MB in one** | **57.8 s** |
///
/// **Why four mebibytes, and why bytes rather than pages.** SQLite makes the
/// same decision at `SQLITE_DEFAULT_WAL_AUTOCHECKPOINT`, 1,000 pages, which at
/// its 4 KiB default page size is four mebibytes. Counting pages here would not
/// mean the same thing: this engine's default page is 32 KiB, so a thousand of
/// them is 32 MiB, which is the middle row above. What has to be bounded is
/// bytes, so bytes is what the bar counts.
///
/// The cost of choosing it this way is small and was measured. A reclamation is
/// about 4 ms - 2.4 ms to create the new segment, 1.2 ms for the checkpoint
/// record, about 0.2 ms to delete - against the 6.5 ms the fold beside it
/// costs. An autocommit statement writes about 34 KiB of log at the 32 KiB
/// default, so four mebibytes is a reclamation every 120 statements and 0.03 ms
/// a statement amortised, half a percent of the fold. At a 4 KiB page it is
/// every 950 statements and nothing at all.
///
/// A clock was considered and rejected. A reclamation that fired on elapsed time
/// would make what a suite sees depend on how long it took to get there.
pub(crate) const RECLAIM_BYTES: u64 = 4 << 20;

/// Records the checkpoint in the log and deletes the segments it has made
/// redundant.
///
/// `retire_segments_below` was written, documented as "called after a
/// checkpoint", and covered by six cases in `inillucent-wal`'s recovery tests -
/// and called from exactly one place, `inillucent-txn`'s engine, which is not
/// the engine that ships. The consequence was measured: the same 200,000 rows
/// are 18.4 MB in SQLite and 179.1 MB here, 27.6 MB of data file and 151.5 MB
/// of log segments that survive a checkpoint, a clean close, a reopen and a
/// second checkpoint.
///
/// It is safe to do here rather than only at close because the function deletes
/// a segment only when every record in it is below the checkpoint LSN *and* the
/// next segment starts at or below it, so a segment holding anything recovery
/// would still need is left alone - and a segment it cannot unlink is left
/// alone and reported `Ok`, because failing a checkpoint over a file that would
/// not delete would turn a tidy-up into an outage. `recovery_from` rather than
/// the log's durable end is what makes that true of a segment a held-back page
/// still needs, not only of one recovery has already replayed.
///
/// **The checkpoint record goes in beside the deletion rather than before it.**
/// `note_checkpoint` is what resets the counter [`RECLAIM_BYTES`] is compared
/// against, so writing one per statement would mean the log never registered as
/// having grown and nothing would ever be reclaimed. Recovery does not need it -
/// it starts from the meta record's own `checkpoint_lsn` and `wal_sequence`,
/// which `set_log_position` moves on every checkpoint including one that does
/// not reach here - and reports the last one it saw for a caller that wants to
/// know.
///
/// @param wal - the log to record the checkpoint in and reclaim
/// @param recovery_from - the point the checkpoint persisted
fn reclaim(wal: &inillucent_wal::writer::Wal, recovery_from: u64) -> DbResult<()> {
    wal.note_checkpoint(recovery_from, 0)?;
    wal.retire_segments_below(recovery_from)?;
    Ok(())
}

impl ImportedDatabase {
    /// Writes every dirty page and advances the log's recovery point.
    ///
    /// The log is synced *first*, so that every page about to be written is one
    /// the log has already described durably. The other order is the durability
    /// mutant the Phase 3 gate exists to kill.
    pub fn checkpoint(&mut self) -> DbResult<()> {
        self.checkpoint_of(true)
    }

    /// Writes every dirty page and advances the log's recovery point, doing as much
    /// of the log's housekeeping as the reason for it calls for.
    ///
    /// **`asked` is the distinction `CheckpointKind` used to carry, and it is back
    /// because `journal_mode = delete` needs it** (task-2000, design 1b). A fold has
    /// two halves: the flush and the meta record, which are what makes the file hold
    /// the statement that just committed, and the log's housekeeping - the catalog's
    /// statistics, a rolled segment, a checkpoint record and the retirement of the
    /// segments the fold made redundant - which is what makes the log shrink.
    /// task-1999 measured the second half at two thirds of a 19 ms fold and put it
    /// behind [`RECLAIM_BYTES`], because under `locking_mode = normal` a fold was a
    /// per-statement event and an autocommit insert went from 1.26 ms to 27.4.
    ///
    /// In `wal` mode that no longer matters, because a fold is not a per-statement
    /// event any more: `release_if_idle` only folds once the log has passed the bar,
    /// so by the time one runs the housekeeping is due as well. In every other
    /// journal mode the fold is still per statement - the after images in the log are
    /// what make a lazy one repairable, and those modes protect the fold with pre
    /// images instead - so the distinction is exactly as load bearing there as
    /// task-1999 found it.
    ///
    /// @param asked - whether somebody asked for a whole checkpoint, rather than a
    ///   statement letting the file go
    pub(crate) fn checkpoint_of(&mut self, asked: bool) -> DbResult<()> {
        // **A read only connection has nothing to fold down.** Its pool holds
        // no change the file does not, its log is a scratch one in memory, and
        // its file handle would refuse the write. Answering `Ok` rather than a
        // refusal is what lets `leave` and `settle_journal` run the same code
        // on both kinds of connection.
        if self.storage.read_only {
            return Ok(());
        }
        // **What a caller reads about its own last statement is put back
        // afterwards (task-1980).** `refresh_statistics` below rewrites the
        // catalog rows whose shape has moved, and that is a write: it opens a
        // transaction, commits it, and on the way through sets
        // `last_insert_rowid`, `changes`, `total_changes` and the participant
        // count of the last commit. Under `locking_mode = exclusive` a
        // connection checkpointed at close and nothing read those afterwards.
        // Under `normal`, which is the default now, every statement that wrote
        // checkpoints on its way out - so a two-file `COMMIT` answered
        // `decided_over` 1 instead of 2, because the checkpoint's own one-file
        // commit was the last one to set it.
        //
        // A checkpoint is the engine's bookkeeping and not a statement the
        // caller ran, so none of those four is its to move. SQLite makes the
        // same distinction: `PRAGMA wal_checkpoint` does not change
        // `sqlite3_changes`.
        let held = (
            self.writing.decided_over(),
            self.counters.last_rowid.get(),
            self.counters.last_changes.get(),
            self.counters.changed_ever.get(),
        );
        let outcome = self.checkpoint_within(asked);
        self.writing.set_decided_over(held.0);
        self.counters.last_rowid.set(held.1);
        self.counters.last_changes.set(held.2);
        self.counters.changed_ever.set(held.3);
        outcome
    }

    /// Folds every attached file and leaves `main` alone.
    ///
    /// **What a lock release does for an attachment** - see
    /// `ImportedDatabase::release_if_idle` for why an attached file still folds at
    /// release and `main` does not. The caller's own counters are put back the way
    /// [`ImportedDatabase::checkpoint`] puts them back, and for the same reason: a
    /// fold is the engine's bookkeeping and not a statement the caller ran, so
    /// `last_insert_rowid`, `changes`, `total_changes` and the participant count of
    /// the last commit are none of its business to move.
    pub(crate) fn checkpoint_attached_only(&mut self) -> DbResult<()> {
        if self.storage.read_only {
            return Ok(());
        }
        let held = (
            self.writing.decided_over(),
            self.counters.last_rowid.get(),
            self.counters.last_changes.get(),
            self.counters.changed_ever.get(),
        );
        // `false`: a release is not somebody asking for a checkpoint, so each
        // attached file's own log decides whether this one reclaims.
        let outcome = self.checkpoint_attached(false);
        self.writing.set_decided_over(held.0);
        self.counters.last_rowid.set(held.1);
        self.counters.last_changes.set(held.2);
        self.counters.changed_ever.set(held.3);
        outcome
    }

    /// Returns the point this checkpoint may tell recovery to start from.
    ///
    /// **Never past an open transaction's own first record, nor past any other
    /// held-back page's.** Read off this schema's own pool, because a
    /// connection with an `ATTACH`ed file checkpoints each file's recovery
    /// point against that file's own transaction, not another file's.
    /// `uncommitted_lsn` alone missed the case where the held-back page's
    /// change belongs to an *earlier*, already-committed statement that simply
    /// had not been checkpointed yet - see this module's own doc comment for
    /// the reproduction.
    ///
    /// **Never below the first position the stream has.** A page that has never
    /// been described by a record carries stamp zero, and `note_dirty_from`
    /// floors a fresh database's `rec_lsn` at its `checkpoint_lsn`, which is
    /// also zero - so a checkpoint taken while a statement holds such a page
    /// computed a recovery point of 0, and `sequence_containing(0)` refused
    /// with "no present segment holds lsn 0: segment 1 starts at 8". Zero is
    /// not a position in the stream; it is the absence of one, and the recovery
    /// point that covers everything is the stream's own start.
    ///
    /// Nothing reached it before task-1980 because the default was
    /// `locking_mode = exclusive`, under which `leave` never checkpoints: the
    /// first checkpoint of a fresh database happened at close, with no
    /// statement holding a page. With `normal` as the default every statement
    /// checkpoints, and a `CREATE VIRTUAL TABLE` that fails on a database
    /// nothing has checkpointed yet reaches it on its way out.
    ///
    /// **And never past a vote nobody has counted.** See `Storage::in_doubt`:
    /// while a super-journal beside this file names a transaction as undecided,
    /// the records of that transaction have to stay where recovery can find
    /// them, whichever way the decision goes.
    ///
    /// @param durable - how far this file's log is durable
    /// @param oldest_dirty - the lowest stamp any page this flush will hold
    ///   back still needs
    fn recovery_point(&self, durable: u64, oldest_dirty: u64) -> u64 {
        let recovery_from = durable
            .min(self.storage.database.pool().uncommitted_lsn())
            .min(oldest_dirty)
            .max(inillucent_wal::FIRST_LSN);
        match self.storage.in_doubt {
            true => recovery_from.min(
                self.storage
                    .database
                    .meta()
                    .checkpoint_lsn
                    .max(inillucent_wal::FIRST_LSN),
            ),
            false => recovery_from,
        }
    }

    /// Everything [`ImportedDatabase::checkpoint`] does, without putting the
    /// caller's own counters back.
    fn checkpoint_within(&mut self, asked: bool) -> DbResult<()> {
        // **The log is reclaimed once it has grown past [`RECLAIM_BYTES`], not
        // once a statement.** `roll_segment`, the checkpoint record and
        // `retire_segments_below` below exist so the log can shrink, and none
        // of them is what makes the file hold the statement that just
        // committed.
        //
        // Measured on 2,000 autocommit inserts (task-1999): rolling a segment
        // was 2.4 ms a statement, locating the recovery point's segment 3.3 ms,
        // the checkpoint record 1.2 ms, and deleting the segments 4.9 ms rising
        // to 14.5 ms as the run went on - together about two thirds of a 19 ms
        // checkpoint.
        // See [`ImportedDatabase::checkpoint_of`]: a fold somebody asked for does
        // the whole of it, and one a statement took on its way out does the log's
        // housekeeping only once the log has grown past [`RECLAIM_BYTES`].
        let reclaiming = asked || self.storage.wal.since_checkpoint() >= RECLAIM_BYTES;
        // **The catalog's statistics are made honest first, and inside the
        // transaction the checkpoint is about to make durable - but only on a
        // checkpoint that is doing the rest of the housekeeping too.** A tree's
        // shape changes on every split and every insert, and rewriting a catalog
        // row that often would put a catalog write on the write path. That is
        // exactly what happened: the sentence this comment used to carry - "a
        // checkpoint is the moment it is cheap, the file is being flushed
        // anyway" - was written when a connection checkpointed at close under
        // `locking_mode = exclusive`. It stopped being true when `normal` became
        // the default and a checkpoint became a per-statement event, and an
        // insert moves its tree's shape almost every time, so the guard inside
        // `refresh_statistics` that skips an unchanged tree never fired.
        // Measured at 1.2 ms a statement, plus a second commit inside a
        // checkpoint (task-1999).
        //
        // Leaving them behind between those checkpoints is safe because they are
        // an estimate and nothing treats them as an invariant: `PagedTree::attach`
        // derives `first_leaf` from the file rather than trusting the recorded
        // copy, and `PagedTree::check` was changed for the same reason - "a
        // cached count is not an invariant; the agreement between the two ways
        // of reaching a leaf is". What is left is `leaf_count` and `row_count`,
        // which seed a planner estimate and a walk's cycle guard.
        //
        // **It rides on `reclaiming` rather than on `Asked` alone**, which the
        // first cut of this change had it do. Nothing checkpoints when a
        // connection closes (see [`RECLAIM_BYTES`]), so a database built
        // entirely out of autocommit inserts would have gone to disk with the
        // row and leaf counts its `CREATE TABLE` wrote and never another.
        if reclaiming {
            self.refresh_statistics()?;
        }
        self.storage.wal.sync()?;
        // **The segment boundary is moved to the checkpoint point first.**
        // A segment is only retirable once every record in it is below the
        // checkpoint LSN, and the segment being appended to never is - the
        // checkpoint record itself lands in it. Rolling here is what turns
        // "everything except the current segment" into "everything", and it is
        // the difference between a log that shrinks and one that keeps one
        // segment's worth of a finished build for ever.
        //
        // Only when this checkpoint is reclaiming: rolling a segment is
        // creating a file, and a segment rolled per statement is also what made
        // `retire_segments_below` walk a sequence range the length of the run
        // (see `Shared::retired_below`).
        if reclaiming {
            self.storage.wal.roll_segment()?;
        }
        // **Read before `log_free_map_pages` touches anything.** Every page
        // `holds_uncommitted` will hold back in the flush below is already
        // dirty right now - it was dirtied by the write that made it
        // uncommitted, before this function was ever called - so its
        // `rec_lsn` is already correct to read; no extra `flush()` is needed
        // to make it accurate, and adding one is exactly the shape that broke
        // the rollback-journal campaigns (see this module's own doc comment).
        // Reading it after `log_free_map_pages` would fold in the free map's
        // own pages' stale `rec_lsn` values for no reason: they are logged and
        // flushed unconditionally by this same checkpoint regardless of any
        // open transaction, so they were never a page recovery could miss.
        let oldest_dirty = self.storage.database.pool().oldest_dirty_lsn();
        // **An after image of every page this fold is about to write in place**
        // (task-2000, design 1a), so a torn in place write is repairable from the
        // log rather than from a rollback journal of pre images. See
        // `inillucent_txn::engine::log_dirty_page_images` for why this and not
        // the journal, and `Pool::fold_protected_by_log` for the two callers -
        // this one and an eviction - that the journal is now split between.
        //
        // **Before `log_free_map_pages` rather than after it.** That function
        // dirties the free map's own pages and appends a `WritePage` for each one
        // itself, so a pass run afterwards would image them a second time for
        // nothing. A free map page that was already dirty before this checkpoint
        // is imaged twice, which is a record the replay applies twice to the same
        // bytes and costs one page of log at the 4 MiB bar.
        //
        // Only under `journal_mode = wal`, which is what
        // `fold_is_protected_by_log` reports: `delete` and its siblings keep the
        // rollback journal and the path they were measured on.
        if self.storage.database.pool().fold_is_protected_by_log() {
            inillucent_txn::engine::log_dirty_page_images(
                &mut self.storage.database,
                &self.storage.wal,
            )?;
        }
        // The free map's own pages, logged and stamped before they are
        // rewritten - see `inillucent_txn::engine::log_free_map_pages` - and
        // done *before* `set_log_position` reads the durable point below.
        // Read earlier, `durable` would sit under this checkpoint's own new
        // free-map record, so the meta record would claim recovery need not
        // start below a point that is, in fact, below an unreplayed record -
        // the next reopen would scan it again every time. That is exactly
        // what `inillucent-txn`'s own copy of this checkpoint measures:
        // `recovering_checkpointing_and_recovering_again_is_the_same_database`.
        inillucent_txn::engine::log_free_map_pages(&mut self.storage.database, &self.storage.wal)?;
        self.storage.wal.sync()?;
        let recovery_from = self.record_recovery_point(oldest_dirty)?;
        self.storage.database.checkpoint_after_free_map()?;
        if reclaiming {
            reclaim(&self.storage.wal, recovery_from)?;
        }
        self.storage
            .database
            .pool()
            .set_durable_lsn(self.storage.wal.write_ahead_point());
        // Every attached database too, because a log is per file and a
        // connection closed after a checkpoint should leave databases rather
        // than databases and logs nobody will open again.
        self.checkpoint_attached(asked)?;
        Ok(())
    }

    /// Settles where the next recovery starts, and writes it into the file.
    ///
    /// Answers `recovery_from`, which the caller needs to retire the segments below
    /// it.
    ///
    /// **The segment `recovery_from` actually lives in, not the one `roll_segment`
    /// just opened.** The freshly rolled segment is only where `recovery_from` lives
    /// when nothing bounded it below the durable point. The moment a held-back page's
    /// own `rec_lsn` pulls it earlier - which is the whole reason the caller reads
    /// `oldest_dirty` at all - `recovery_from` can sit in an earlier segment than the
    /// new one, and pairing it with the new segment's number anyway told the next
    /// open's `read_chain` to start reading a segment that does not contain it,
    /// silently skipping every record between the true segment and the new one. See
    /// `Wal::sequence_containing`'s own doc comment for the reproduction this was
    /// caught by, and for why it refuses outright rather than guess when no present
    /// segment holds `recovery_from` - the `?` here is that refusal reaching the
    /// checkpoint, and a failed checkpoint is better than one that persists a
    /// recovery point `read_chain` cannot honour.
    ///
    /// **The retained point is set before anything is deleted.** From that instant
    /// any page dirtying for the first time floors its `rec_lsn` at `recovery_from`
    /// rather than at whatever stale stamp it is carrying - see
    /// `Pool::note_dirty_from`. It is set after `set_log_position` only because the
    /// two do not interact; what matters is that it is set before
    /// `retire_segments_below` deletes a segment.
    ///
    /// Split out of `checkpoint_within` in task-2006, which design 1a's after image
    /// pass took past the 150 line bar `policy.rs` holds a new function to.
    ///
    /// @param oldest_dirty - the earliest log position any dirty page still needs
    fn record_recovery_point(&mut self, oldest_dirty: u64) -> DbResult<u64> {
        let durable = self.storage.wal.write_ahead_point();
        self.storage.database.pool().set_durable_lsn(durable);
        let recovery_from = self.recovery_point(durable, oldest_dirty);
        let recovery_sequence = self.storage.wal.sequence_containing(recovery_from)?;
        self.storage
            .database
            .set_log_position(recovery_from, 0, recovery_sequence);
        self.storage.database.pool().set_retained_lsn(recovery_from);
        Ok(recovery_from)
    }

    /// Folds every attached database's log into its own file.
    ///
    /// A checkpoint is per file, because a log is per file. `checkpoint` does
    /// `main`; this does the rest, so that a connection closed after one is not
    /// leaving an attached database's committed rows in a log the next open of
    /// *that file alone* would still have to replay.
    ///
    /// Each file's recovery point is bounded by that file's own dirty pages and
    /// that file's own open transaction, for the same reason `checkpoint` bounds
    /// `main`'s: an attachment is its own no-steal domain, with its own pool and
    /// its own log, and nothing above ever mixes one file's watermark into
    /// another's.
    ///
    /// **Logs and stamps its own free-map pages, the same as `checkpoint` does
    /// for `main`.** This used to call `Database::checkpoint()` directly - the
    /// unlogged path, meant only for a caller with no log to protect the
    /// rewrite with - which left an attached file exposed to exactly the
    /// defect `checkpoint` was fixed against for `main`: every free-map page
    /// rewritten on every checkpoint, with no `WritePage` record and no LSN
    /// stamp behind it, so a crash mid-write could leave it unrecoverable.
    ///
    fn checkpoint_attached(&mut self, asked: bool) -> DbResult<()> {
        for nth in 0..self.session_state.attached.len() {
            let Some(held) = self.session_state.attached.get_mut(nth) else {
                continue;
            };
            if held.path.is_none() {
                // A database with no file has nothing to fold a log into.
                continue;
            }
            // **A file this connection is not holding is not one it may write
            // (task-1987).** A checkpoint is a write - it rewrites the free
            // map's pages, moves the meta record's generation and retires log
            // segments - so doing it without the lock is doing it while another
            // process is doing the same thing to the same bytes.
            //
            // It was reachable, and it lost acknowledged commits. `enter`'s own
            // failure path calls `release_if_idle`, and `release_if_idle` asks
            // whether *`main`* still holds a lock before it checkpoints, then
            // checkpoints every attachment. So a statement refused by a busy
            // timeout on this attachment - `begin_write_retrying` gives up by
            // unlocking to `None` - left the attachment holding nothing while
            // `main` held everything, and this loop then wrote the attachment's
            // meta record from a cache that was by then seconds out of date.
            //
            // Measured, two processes each attaching one shared file and
            // running three hundred autocommit inserts into it: one wrote
            // generation 49 and read it back, the other wrote **generation 10**
            // over it 17 ms later with `lock=None` after an 8.4 second gap in
            // which it had entered nothing, and the first then resynchronised
            // *backwards* onto that record and lost the 38 rows it had
            // committed in between. Between 36 and 129 acknowledged inserts of
            // about 595 went that way, in three trials of every twelve.
            //
            // Skipping is the whole fix and it loses nothing: a page can only
            // be dirtied under EXCLUSIVE, so an attachment that is not held has
            // nothing outstanding to fold, and the next statement that does
            // enter it resynchronises it from the file first.
            if held.database.lock_level() <= inillucent_vfs::FileLock::Shared {
                continue;
            }
            // Each file's log is reclaimed on its own terms, because each file has
            // its own log and its own size. See `checkpoint_within`.
            let reclaiming = asked || held.wal.since_checkpoint() >= RECLAIM_BYTES;
            held.wal.sync()?;
            if reclaiming {
                held.wal.roll_segment()?;
            }
            // See `checkpoint`'s own comment: read before this file's free-map
            // pages are touched below, and without an extra `flush()`.
            let oldest_dirty = held.database.pool().oldest_dirty_lsn();
            // The same after images `main`'s fold appends, for the same reason:
            // an attached file's fold writes its pages in place too, and it has
            // its own log to carry them. See `checkpoint_within`.
            if held.database.pool().fold_is_protected_by_log() {
                inillucent_txn::engine::log_dirty_page_images(&mut held.database, &held.wal)?;
            }
            inillucent_txn::engine::log_free_map_pages(&mut held.database, &held.wal)?;
            held.wal.sync()?;
            let durable = held.wal.write_ahead_point();
            held.database.pool().set_durable_lsn(durable);
            // See `checkpoint`'s own comment: zero is the absence of a stream
            // position rather than one, and the point that covers everything is
            // the stream's start.
            let recovery_from = durable
                .min(held.database.pool().uncommitted_lsn())
                .min(oldest_dirty)
                .max(inillucent_wal::FIRST_LSN);
            // And the same doubt floor `main` has, read off this file's own
            // markers rather than off `main`'s: a transaction may be undecided
            // over the attachment and settled over the database the connection
            // was opened on, or the other way round.
            let recovery_from = match held.in_doubt {
                true => recovery_from.min(
                    held.database
                        .meta()
                        .checkpoint_lsn
                        .max(inillucent_wal::FIRST_LSN),
                ),
                false => recovery_from,
            };
            // See `checkpoint`'s own comment: paired with the segment that
            // actually holds `recovery_from`, not whichever one `roll_segment`
            // just opened, and refused rather than guessed if none does.
            let recovery_sequence = held.wal.sequence_containing(recovery_from)?;
            held.database
                .set_log_position(recovery_from, 0, recovery_sequence);
            // Same reason as `checkpoint`'s own call: floors this file's own
            // pool against this file's own recovery point, before segments
            // below it are retired.
            held.database.pool().set_retained_lsn(recovery_from);
            held.database.checkpoint_after_free_map()?;
            if reclaiming {
                reclaim(&held.wal, recovery_from)?;
            }
            held.database
                .pool()
                .set_durable_lsn(held.wal.write_ahead_point());
        }
        Ok(())
    }
}
