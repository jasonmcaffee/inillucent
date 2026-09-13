//! Checkpointing `ImportedDatabase`: writing every dirty page out and moving
//! the log's recovery point behind it.
//!
//! Invariant: **the recorded recovery point never sits above an open
//! transaction's own first record.** `Pool::writeback`'s no-steal rule
//! (`holds_uncommitted`) keeps an open transaction's dirty pages out of the
//! file; a checkpoint that recorded `checkpoint_lsn` as the log's durable end
//! regardless would tell recovery it can start above records those held-back
//! pages still need, and a crash before the transaction committed would leave
//! its rows in the file with nothing left to undo them.
//!
//! **This reads only `Pool::uncommitted_lsn`, not `Pool::oldest_dirty_lsn`
//! too.** `inillucent_txn::engine::Engine::checkpoint` - the engine that does
//! not ship - takes the minimum of both, guarded by an explicit `flush()`
//! before either is read; measured here, that same shape corrupted the
//! rollback-journal campaigns (`power_loss_at_every_cut_point_of_a_checkpoint`
//! and its `truncate`/`persist` siblings failed at specific cut points with
//! "database disk image is malformed"), because it puts a second `flush()`
//! - and so a second round of `journal_page`/`seal_journal` calls, since
//! `ImportedDatabase` runs those under a real rollback journal and
//! `inillucent-txn`'s `Engine` never does - ahead of the one
//! `checkpoint_after_free_map` already makes. `oldest_dirty_lsn` alone
//! protects a narrower case than `uncommitted_lsn` does - a page still dirty
//! from a transaction that committed at an *earlier* checkpoint and was never
//! rewritten since - which neither of this ticket's two required campaigns
//! reaches; `uncommitted_lsn` alone is what they need, and it is what stays
//! armed for exactly as long as the open transaction that must not reach the
//! file does.
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
        let sequence = self.wal.sequence();
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
        // **Never past an open transaction's own first record.** Read off
        // this schema's own pool, because a connection with an `ATTACH`ed
        // file checkpoints each file's recovery point against that file's own
        // transaction, not another file's.
        let recovery_from = durable.min(self.database.pool().uncommitted_lsn());
        self.database.set_log_position(recovery_from, 0, sequence);
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
            let sequence = held.wal.sequence();
            let durable = held.wal.write_ahead_point();
            held.database.pool().set_durable_lsn(durable);
            let recovery_from = durable.min(held.database.pool().uncommitted_lsn());
            held.database.set_log_position(recovery_from, 0, sequence);
            held.database.checkpoint()?;
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
