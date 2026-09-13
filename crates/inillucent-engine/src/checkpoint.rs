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

impl ImportedDatabase {
    /// Writes every dirty page and advances the log's recovery point.
    ///
    /// The log is synced *first*, so that every page about to be written is one
    /// the log has already described durably. The other order is the durability
    /// mutant the Phase 3 gate exists to kill.
    pub fn checkpoint(&mut self) -> DbResult<()> {
        // **The catalog's statistics are made honest first, and inside the
        // transaction the checkpoint is about to make durable.** A tree's shape
        // changes on every split and every insert, and rewriting a catalog row
        // that often would put a catalog write on the write path. A checkpoint
        // is the moment it is cheap: the file is being flushed anyway, and what
        // the next open reads is the shape as of the last checkpoint - which is
        // exactly what the next open needs, because everything after it is in
        // the log for recovery to replay.
        self.refresh_statistics()?;
        self.wal.sync()?;
        // **The segment boundary is moved to the checkpoint point first.**
        // A segment is only retirable once every record in it is below the
        // checkpoint LSN, and the segment being appended to never is - the
        // checkpoint record itself lands in it. Rolling here is what turns
        // "everything except the current segment" into "everything", and it is
        // the difference between a log that shrinks and one that keeps one
        // segment's worth of a finished build for ever.
        self.wal.roll_segment()?;
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
        let oldest_dirty = self.database.pool().oldest_dirty_lsn();
        // The free map's own pages, logged and stamped before they are
        // rewritten - see `inillucent_txn::engine::log_free_map_pages` - and
        // done *before* `set_log_position` reads the durable point below.
        // Read earlier, `durable` would sit under this checkpoint's own new
        // free-map record, so the meta record would claim recovery need not
        // start below a point that is, in fact, below an unreplayed record -
        // the next reopen would scan it again every time. That is exactly
        // what `inillucent-txn`'s own copy of this checkpoint measures:
        // `recovering_checkpointing_and_recovering_again_is_the_same_database`.
        inillucent_txn::engine::log_free_map_pages(&mut self.database, &self.wal)?;
        self.wal.sync()?;
        let durable = self.wal.write_ahead_point();
        self.database.pool().set_durable_lsn(durable);
        // **Never past an open transaction's own first record, nor past any
        // other held-back page's.** Read off this schema's own pool, because
        // a connection with an `ATTACH`ed file checkpoints each file's
        // recovery point against that file's own transaction, not another
        // file's. `uncommitted_lsn` alone missed the case where the held-back
        // page's change belongs to an *earlier*, already-committed statement
        // that simply had not been checkpointed yet - see this module's own
        // doc comment for the reproduction.
        let recovery_from = durable
            .min(self.database.pool().uncommitted_lsn())
            .min(oldest_dirty);
        // **The segment `recovery_from` actually lives in, not the one
        // `roll_segment` just opened.** The freshly rolled segment is only
        // where `recovery_from` lives when nothing bounded it below
        // `durable`. The moment a held-back page's own `rec_lsn` pulls it
        // earlier - the whole reason this function reads `oldest_dirty` at
        // all - `recovery_from` can sit in an earlier segment than the new
        // one, and pairing it with the new segment's number anyway told the
        // next open's `read_chain` to start reading a segment that does not
        // contain it, silently skipping every record between the true
        // segment and the new one. See `Wal::sequence_containing`'s own doc
        // comment for the reproduction this was caught by, and for why it
        // refuses outright rather than guess when no present segment holds
        // `recovery_from` - the `?` here is that refusal reaching this
        // checkpoint: better a failed checkpoint than one that persists a
        // recovery point `read_chain` cannot actually honor.
        let recovery_sequence = self.wal.sequence_containing(recovery_from)?;
        self.database
            .set_log_position(recovery_from, 0, recovery_sequence);
        // **Before the segments below it are retired.** From this instant, any
        // page that dirties for the first time floors its `rec_lsn` at
        // `recovery_from` rather than at whatever stale stamp it happens to be
        // carrying - see `Pool::note_dirty_from`. Set after `set_log_position`
        // only because the two do not interact; what matters is that it is set
        // before `retire_segments_below` below actually deletes anything.
        self.database.pool().set_retained_lsn(recovery_from);
        self.database.checkpoint_after_free_map()?;
        self.wal.note_checkpoint(recovery_from, 0)?;
        // **And then the segments the checkpoint has made redundant go.**
        //
        // `retire_segments_below` was written, documented as "called after a
        // checkpoint", and covered by six cases in `inillucent-wal`'s recovery
        // tests - and called from exactly one place, `inillucent-txn`'s engine,
        // which is not the engine that ships. The consequence was measured: the
        // same 200,000 rows are 18.4 MB in SQLite and 179.1 MB here, 27.6 MB of
        // data file and 151.5 MB of log segments that survive a checkpoint, a
        // clean close, a reopen and a second checkpoint.
        //
        // It is safe to do here rather than only at close because the function
        // deletes a segment only when every record in it is below the
        // checkpoint LSN *and* the next segment starts at or below it, so a
        // segment holding anything recovery would still need is left alone -
        // and a segment it cannot unlink is left alone and reported `Ok`,
        // because failing a checkpoint over a file that would not delete would
        // turn a tidy-up into an outage. `recovery_from` rather than `durable`
        // is what makes that true of a segment a held-back page still needs,
        // not only of one recovery has already replayed.
        self.wal.retire_segments_below(recovery_from)?;
        self.database
            .pool()
            .set_durable_lsn(self.wal.write_ahead_point());
        // Every attached database too, because a log is per file and a
        // connection closed after a checkpoint should leave databases rather
        // than databases and logs nobody will open again.
        self.checkpoint_attached()?;
        Ok(())
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
    fn checkpoint_attached(&mut self) -> DbResult<()> {
        for nth in 0..self.attached.len() {
            let Some(held) = self.attached.get_mut(nth) else {
                continue;
            };
            if held.path.is_none() {
                // A database with no file has nothing to fold a log into.
                continue;
            }
            held.wal.sync()?;
            held.wal.roll_segment()?;
            // See `checkpoint`'s own comment: read before this file's free-map
            // pages are touched below, and without an extra `flush()`.
            let oldest_dirty = held.database.pool().oldest_dirty_lsn();
            inillucent_txn::engine::log_free_map_pages(&mut held.database, &held.wal)?;
            held.wal.sync()?;
            let durable = held.wal.write_ahead_point();
            held.database.pool().set_durable_lsn(durable);
            let recovery_from = durable
                .min(held.database.pool().uncommitted_lsn())
                .min(oldest_dirty);
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
            held.wal.note_checkpoint(recovery_from, 0)?;
            // The segments below the checkpoint describe changes the file now
            // holds, so keeping them is keeping a second copy of the database
            // for ever. See `Database::checkpoint` for the measurement.
            held.wal.retire_segments_below(recovery_from)?;
            held.database
                .pool()
                .set_durable_lsn(held.wal.write_ahead_point());
        }
        Ok(())
    }
}
