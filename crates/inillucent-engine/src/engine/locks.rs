//! Taking the file lock a statement needs, and choosing the journal.
//!
//! Invariant: **the lock is taken on the way into the outermost statement and
//! released on the way out of it.** A statement runs statements - a trigger
//! body, a foreign-key sweep, a `CHECK` - so what counts them is a counter and
//! not a flag: an inner release would drop the file while the outer statement
//! was still reading it.
//!
//! A transaction holds its lock from its first write to its commit, so `leave`
//! releases nothing while one is open. A `BEGIN` that let the file go between
//! its statements would be a transaction another process could write through
//! the middle of.

use crate::*;

impl ImportedDatabase {
    /// Chooses whether the file lock is kept between transactions.
    ///
    /// Dropping to `normal` releases the lock immediately, which is the moment
    /// a second process may open the file; raising to `exclusive` takes it at
    /// the next statement rather than here, because taking it now would make a
    /// pragma block on a lock the caller has not asked to wait for.
    ///
    /// @param exclusive - whether to keep the lock
    pub(crate) fn set_locking_exclusive(&mut self, exclusive: bool) -> DbResult<()> {
        self.pragmas.locking_exclusive.set(exclusive);
        if !exclusive && self.writing.batch.get().is_none() {
            self.storage.database.end_access()?;
        }
        Ok(())
    }

    /// Takes the lock a statement needs and reloads if the file has moved.
    ///
    /// **Called before every statement**, so a connection that has been idle
    /// while another process wrote sees the new database rather than its own
    /// cache of the old one. Under `exclusive` the lock is already held and
    /// this is a comparison of two integers.
    ///
    /// @param writing - whether the statement changes the database
    pub(crate) fn enter(&mut self, writing: bool) -> DbResult<()> {
        self.writing
            .running
            .set(self.writing.running.get().saturating_add(1));
        // **A transaction holds its lock from the first write to the commit.**
        // Once inside one, the retry loop's release would open a window another
        // process could write through - see `Database::begin_write_within`.
        let inside = self.writing.batch.get().is_some() || self.writing.running.get() > 1;
        let reloaded = if writing {
            self.storage.database.begin_write_within(!inside)?
        } else {
            self.storage.database.begin_read()?
        };
        // **The pages are not the whole cache.** `begin_read` throws away the
        // pool when another process has committed; the *schema* this connection
        // read at open is just as stale, and a connection that kept it would
        // write its own catalog tree over the one the other process just built -
        // which is a lost table rather than a stale read. `reload_catalog` is
        // the same reread `ATTACH` does.
        if reloaded && !inside {
            self.reload_catalog()?;
            // **And the modules hear that somebody else committed
            // (task-1932, M2).** A module's own state is derived from its
            // shadow tables, which are ordinary trees another connection can
            // have written; this is the one moment the engine knows that
            // happened.
            self.committed_elsewhere_modules();
        }
        Ok(())
    }

    /// Releases the lock when nothing holds the connection to the file.
    ///
    /// A transaction is open, so nothing is released: a `BEGIN` that let the
    /// file go between its statements would be a transaction another process
    /// could write through the middle of.
    pub(crate) fn leave(&mut self) -> DbResult<()> {
        self.writing
            .running
            .set(self.writing.running.get().saturating_sub(1));
        if self.writing.running.get() > 0
            || self.pragmas.locking_exclusive.get()
            || self.writing.batch.get().is_some()
        {
            return Ok(());
        }
        // **Durable before the file is let go, and this is the whole cost of
        // `locking_mode = normal`.** A connection that released the lock with
        // dirty pages would leave the file describing a database without the
        // statement that just succeeded - and the next process to write would
        // build on that file and overwrite the statement for good. It is not a
        // stale read; it is a lost write, and it is what the first version of
        // this did eight times in ten under two concurrent writers.
        //
        // Under `exclusive`, which is the default, the lock is never let go and
        // none of this runs: the checkpoint happens when the connection closes,
        // as it always did.
        if self.storage.database.pool().lock_level() != inillucent_vfs::FileLock::None {
            self.checkpoint()?;
        }
        self.storage.database.end_access()
    }

    /// Changes how the pre-commit state is protected.
    ///
    /// **The database is checkpointed on the way through**, which is not a
    /// tidy-up: the two schemes protect different things, and a switch made
    /// with uncommitted state in either of them would leave a file neither of
    /// them could recover. SQLite refuses the switch inside a transaction for
    /// the same reason.
    ///
    /// @param mode - the mode to switch to
    pub(crate) fn set_journal_mode(
        &mut self,
        mode: inillucent_pool::journal::JournalMode,
    ) -> DbResult<()> {
        if mode == self.pragmas.journal_mode.get() {
            return Ok(());
        }
        if self.writing.batch.get().is_some() {
            return Err(refusal(
                "cannot change PRAGMA journal_mode from within a transaction",
            ));
        }
        self.checkpoint()?;
        self.pragmas.journal_mode.set(mode);
        // **WAL is the one mode the file remembers.** SQLite writes a
        // read/write version of 2 into its header for a WAL database and 1 for
        // everything else, so a reopen comes back in WAL and comes back at the
        // connection's default for any of the rollback modes. Recording it here
        // is what makes `PRAGMA journal_mode = wal` outlive the connection that
        // asked - without it, a reopen of a WAL database answered `delete` and
        // would have started writing pre-images beside a log.
        self.storage
            .database
            .set_wal_mode(mode == inillucent_pool::journal::JournalMode::Wal);
        // And checkpointed again, because the meta record reaches the file at a
        // checkpoint and the one above ran before the flag was set. Without
        // this second one the flag is written only if something else forces a
        // checkpoint later, so `PRAGMA journal_mode = wal` followed by a clean
        // close reopened as `delete`.
        self.checkpoint()?;
        // A VFS of its own rather than the schema's, because the journal opens
        // one file by name and `OsVfs` is stateless - the same reasoning that
        // lets `create` and `open` each make their own.
        let held: std::sync::Arc<dyn inillucent_vfs::Vfs> =
            std::sync::Arc::clone(&self.storage.vfs);
        let journal = journal_for(mode).map(|protection| {
            inillucent_pool::journal::Journal::new(
                held,
                &DbPath::new(self.storage.path.to_string_lossy().as_ref()),
                protection,
                self.storage.page_size,
            )
        });
        self.storage.database.pool().set_journal(journal);
        // **`PRAGMA journal_mode` names the connection, not one file of it.**
        // `checkpoint_attached` writes an attached file's pages in place
        // exactly as `main`'s checkpoint does, so an attachment left on its
        // old journal here would keep the interrupted-checkpoint defect open
        // for every database this connection holds but the one the pragma
        // named. Each attachment gets its own `Journal`, over its own path and
        // its own file's page size, because an attached file can have been
        // created at a page size that differs from this connection's.
        for held in self.session_state.attached.iter_mut() {
            let Some(path) = held.path.as_ref() else {
                // `:memory:` has no file to checkpoint into, so nothing here
                // needs protecting.
                continue;
            };
            let attached_path = DbPath::new(path.to_string_lossy().as_ref());
            let attached_vfs: std::sync::Arc<dyn inillucent_vfs::Vfs> =
                std::sync::Arc::clone(&held.vfs);
            let page_size = held.database.page_size();
            let attached_journal = journal_for(mode).map(|protection| {
                inillucent_pool::journal::Journal::new(
                    attached_vfs,
                    &attached_path,
                    protection,
                    page_size,
                )
            });
            held.database.pool().set_journal(attached_journal);
        }
        Ok(())
    }
}

/// Returns the journal a connection in `mode` needs to protect a checkpoint.
///
/// **A write-ahead log does not remove the need for a rollback journal here,
/// and that is a consequence of the log being logical.** A checkpoint writes
/// pages into the data file in place. Once a page's content is below the
/// recorded checkpoint point, the log no longer describes it: the records that
/// built it have been made redundant and their segments retired. So a page the
/// checkpoint half wrote before a power loss is content nothing can rebuild -
/// not the log, which has moved past it, and not the page itself, which is
/// torn. SQLite is not exposed to this because its log holds whole page images
/// and a checkpoint is a copy, so an interrupted one is simply redone.
///
/// `crates/inillucent-compat/tests/wal_crash.rs`'s checkpoint campaign found
/// it: a crash inside `PRAGMA wal_checkpoint` left pages 2 and 3 written and
/// unsynced, the meta record correctly still naming the *previous* checkpoint,
/// and recovery unable to read a page it could not rebuild either -
/// `page 3 checksum fe9063aa is not the computed f53956bb`.
///
/// So a connection in `wal` takes a `delete` journal, which holds the
/// pre-images for the duration of a checkpoint and removes the file when the
/// checkpoint's meta record is durable. The cost is the one the default mode
/// already pays; what it buys is that an interrupted checkpoint is undoable in
/// every mode rather than in three of the five.
///
/// `off` is the one mode that gets nothing, because that is what it asks for.
///
/// @param mode - what `PRAGMA journal_mode` reports
pub(crate) fn journal_for(
    mode: inillucent_pool::journal::JournalMode,
) -> Option<inillucent_pool::journal::JournalMode> {
    match mode {
        inillucent_pool::journal::JournalMode::Off => None,
        inillucent_pool::journal::JournalMode::Wal => {
            Some(inillucent_pool::journal::JournalMode::Delete)
        }
        other => Some(other),
    }
}
