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
        self.pragmas.set_locking_exclusive(exclusive);
        if !exclusive && self.writing.batch().is_none() {
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
            .set_running(self.writing.running().saturating_add(1));
        // **A transaction holds its lock from the first write to the commit.**
        // Once inside one, the retry loop's release would open a window another
        // process could write through - see `Database::begin_write_within`.
        let inside = self.writing.batch().is_some() || self.writing.running() > 1;
        // **A read only connection takes SHARED for every statement.**
        // `writes_of` answers true for every directive, a `PRAGMA` included, so
        // reading `PRAGMA user_version` asked for the write lock - which a read
        // only connection cannot take and does not need. Anything that would
        // actually write is refused before it gets here, by
        // `inillucent_engine::readonly` on the command surface and by the pool
        // underneath (task-1979, section 5.2).
        let writing = writing && !self.storage.read_only;
        // **Whether the lock is being taken now, asked before it is taken.**
        // Everything this connection read at `open` - the meta record, the
        // catalog, the log's tail - was read without this lock or under a
        // shared one two processes hold at once, so none of it may be trusted
        // once the lock is in hand. A connection that already holds the file
        // has nothing to re-derive, which is what keeps the check off the path
        // `locking_mode = exclusive` takes (task-1979, section 4.4 item 1).
        let taking = !self.storage.database.trusted();
        let reloaded = if writing {
            self.storage.database.begin_write_within(!inside)?
        } else {
            self.storage.database.begin_read()?
        };
        // **The log is asked the same question the meta record is.** A commit
        // that is durable in the log and not yet checkpointed does not move the
        // generation, so the meta record alone reports "nothing has changed"
        // about a file another process has just written - which is how 120
        // acknowledged inserts became 60 rows (task-1979, section 4.2). The
        // segment beside the file is where that commit is, and its length is
        // the answer.
        let moved = reloaded || (taking && !inside && self.the_log_moved()?);
        if moved && !inside {
            self.resync_from_file()?;
        }
        if taking && !inside {
            self.storage.database.mark_trusted();
        }
        // **Every attached file takes its own lock, for the same reason `main`
        // does (task-1979, C2).** An attachment was the one file this engine
        // wrote with no lock at all, so two processes with different `main`
        // databases attaching one shared `.rdb` lost one side entirely, with
        // nothing excluding them. A file is a file; which name a statement
        // qualifies it with does not change what another process can do to it.
        let moved = self.enter_attached(writing, inside)? || moved;
        // **The pages are not the whole cache.** The resynchronisation above
        // throws away the pool when another process has committed; the *schema*
        // this connection read at open is just as stale, and a connection that
        // kept it would write its own catalog tree over the one the other
        // process just built - which is a lost table rather than a stale read.
        // `reload_catalog` is the same reread `ATTACH` does.
        if moved && !inside {
            self.reload_catalog()?;
            self.reattach_every_tree()?;
            // **And the modules hear that somebody else committed
            // (task-1932, M2).** A module's own state is derived from its
            // shadow tables, which are ordinary trees another connection can
            // have written; this is the one moment the engine knows that
            // happened.
            self.committed_elsewhere_modules();
        }
        Ok(())
    }

    /// Takes the lock every attached file needs and re-derives the ones that
    /// moved.
    ///
    /// Returns whether any of them had.
    ///
    /// A `temp` database and a `:memory:` one have no path, so no other process
    /// can reach them and there is nothing to take.
    ///
    /// @param writing - whether the statement changes the database
    /// @param inside - whether a transaction is already holding these files
    fn enter_attached(&mut self, writing: bool, inside: bool) -> DbResult<bool> {
        let mut any = false;
        for index in 0..self.session_state.attached.len() {
            let Some(held) = self.session_state.attached.get(index) else {
                continue;
            };
            let Some(path) = held.path.clone() else {
                continue;
            };
            let taking = !held.database.trusted();
            let reloaded = {
                let Some(held) = self.session_state.attached.get_mut(index) else {
                    continue;
                };
                if writing {
                    held.database.begin_write_within(!inside)?
                } else {
                    held.database.begin_read()?
                }
            };
            let moved = reloaded || (taking && !inside && self.attached_log_moved(index)?);
            if moved && !inside {
                self.resync_attached(index, &path)?;
                any = true;
            }
            if taking && !inside {
                if let Some(held) = self.session_state.attached.get_mut(index) {
                    held.database.mark_trusted();
                }
            }
        }
        Ok(any)
    }

    /// Reports whether one attached file's log ends somewhere other than where
    /// this connection left it.
    ///
    /// @param index - which attachment
    fn attached_log_moved(&self, index: usize) -> DbResult<bool> {
        let Some(held) = self.session_state.attached.get(index) else {
            return Ok(false);
        };
        let Some(path) = held.path.as_ref() else {
            return Ok(false);
        };
        let db_path = DbPath::new(path.to_string_lossy().as_ref());
        let tail = inillucent_wal::tail_on_disk(
            held.vfs.as_ref(),
            &db_path,
            held.database.uuid(),
            held.wal.sequence(),
        )?;
        Ok(match tail {
            Some(tail) => {
                tail.sequence != held.wal.sequence() || tail.next_lsn != held.wal.written_end()
            }
            None => false,
        })
    }

    /// Rebuilds one attached file's pages, free map and log position from the
    /// files, with its lock held.
    ///
    /// @param index - which attachment
    /// @param path - that attachment's file
    fn resync_attached(&mut self, index: usize, path: &std::path::Path) -> DbResult<()> {
        let db_path = DbPath::new(path.to_string_lossy().as_ref());
        let doubtful = crate::multi::doubtful_transactions(path)?;
        let Some(held) = self.session_state.attached.get_mut(index) else {
            return Ok(());
        };
        let vfs = std::sync::Arc::clone(&held.vfs);
        if let Some(found) = held.database.meta_on_disk()? {
            held.database.adopt_from_file(found)?;
        }
        held.wal = crate::recovery::resync_file(&mut held.database, &vfs, &db_path, &doubtful)?;
        Ok(())
    }

    /// Reports whether the log beside the file ends somewhere other than where
    /// this connection last left it.
    ///
    /// **Read from the segment rather than from a counter this engine writes.**
    /// The alternative the design offered was moving the meta record's
    /// generation on every commit, which would put one more page in every
    /// commit's write set; the segment's own length answers the same question
    /// for one `file_size` per lock acquisition and costs the write path
    /// nothing (task-1979, section 4.4 item 2, the second option).
    ///
    /// A database with no file - `:memory:` and a temporary one - has no
    /// segment and no second process, so it answers no.
    fn the_log_moved(&self) -> DbResult<bool> {
        if self.storage.path.as_os_str().is_empty() {
            return Ok(false);
        }
        let db_path = DbPath::new(self.storage.path.to_string_lossy().as_ref());
        let tail = inillucent_wal::tail_on_disk(
            self.storage.vfs.as_ref(),
            &db_path,
            self.storage.database.uuid(),
            self.storage.wal.sequence(),
        )?;
        Ok(match tail {
            Some(tail) => {
                tail.sequence != self.storage.wal.sequence()
                    || tail.next_lsn != self.storage.wal.written_end()
            }
            // No segment where this connection believes its own log is. The
            // only way that happens is a checkpoint by another process that
            // retired it, which moved the generation, so the meta record has
            // already reported it.
            None => false,
        })
    }

    /// Rebuilds this connection's pages, free map and log position from the
    /// files, with the lock held.
    ///
    /// See `crate::recovery::resync_file` for why every part of it is
    /// re-derived rather than patched.
    fn resync_from_file(&mut self) -> DbResult<()> {
        if self.storage.path.as_os_str().is_empty() {
            return Ok(());
        }
        let db_path = DbPath::new(self.storage.path.to_string_lossy().as_ref());
        let doubtful = crate::multi::doubtful_transactions(&self.storage.path)?;
        if let Some(found) = self.storage.database.meta_on_disk()? {
            self.storage.database.adopt_from_file(found)?;
        }
        let vfs = std::sync::Arc::clone(&self.storage.vfs);
        self.storage.wal =
            crate::recovery::resync_file(&mut self.storage.database, &vfs, &db_path, &doubtful)?;
        Ok(())
    }

    /// Releases the lock when nothing holds the connection to the file.
    ///
    /// A transaction is open, so nothing is released: a `BEGIN` that let the
    /// file go between its statements would be a transaction another process
    /// could write through the middle of.
    pub(crate) fn leave(&mut self) -> DbResult<()> {
        self.writing
            .set_running(self.writing.running().saturating_sub(1));
        if self.writing.running() > 0
            || self.pragmas.locking_exclusive()
            || self.writing.batch().is_some()
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
        // **What it costs, measured (task-1979, section 18, decision 1).** 300
        // autocommit inserts through `inillucent-shell` take 1.91 s with this
        // checkpoint removed and 10.70 s with it, against 0.46 s under
        // `locking_mode = exclusive`, where this function returns above and
        // never reaches here. The 32 ms per statement is a checkpoint's own
        // work: it rewrites the catalog's statistics, syncs the log twice,
        // rolls a new log segment, rewrites the free map's pages, writes the
        // meta record and deletes every segment below the new recovery point.
        //
        // **Removing it was tried and put back.** The next process does replay
        // the log, so the *committed* statement is not lost either way - but
        // leaving every statement's records unfolded meant every later open
        // replayed them, which changed what eighteen suites saw: a recovery
        // report on the front of ordinary command output, and the crash
        // campaigns grading a file with a log nothing had folded. The cost is
        // the price of a default that two processes can share; an application
        // that never opens a second connection sets
        // `PRAGMA locking_mode = exclusive` and pays none of it.
        //
        // **Only when this statement wrote something.** A checkpoint is a
        // write: it rolls a log segment, rewrites the free map's pages, moves
        // the meta record's generation and deletes the segments below the new
        // recovery point. Running one after a `SELECT` makes reading a database
        // change it, which `lifecycle.rs`'s `a_long_session_changes_no_byte`
        // compares byte for byte, and made four reads delete log segments a
        // handle still had open. A read takes SHARED and a write raises past
        // it, so the lock level is the question already answered; the dirty
        // count is the second half, for a statement that wrote and released
        // before reaching here.
        //
        // **Every file the connection holds, not only `main`.** A statement
        // that wrote an attached database and nothing else leaves `main` clean,
        // so asking `main` alone answered "nothing was written" and released
        // every file with the attachment's pages still dirty - which lost 116
        // of 599 acknowledged inserts through `ATTACH` under load.
        let wrote = self.storage.database.lock_level() > inillucent_vfs::FileLock::Shared
            || self.storage.database.pool().dirty_pages() > 0
            || self.session_state.attached.iter().any(|held| {
                held.path.is_some()
                    && (held.database.lock_level() > inillucent_vfs::FileLock::Shared
                        || held.database.pool().dirty_pages() > 0)
            });
        if wrote && self.storage.database.pool().lock_level() != inillucent_vfs::FileLock::None {
            self.checkpoint()?;
        }
        // Every attached file is let go on the same terms `main` is: the
        // checkpoint above wrote all of them - `checkpoint_attached` is part of
        // it - so each one's file is current before its lock is released.
        for held in self.session_state.attached.iter_mut() {
            if held.path.is_some() {
                held.database.end_access()?;
            }
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
        if mode == self.pragmas.journal_mode() {
            return Ok(());
        }
        if self.writing.batch().is_some() {
            return Err(refusal(
                "cannot change PRAGMA journal_mode from within a transaction",
            ));
        }
        self.checkpoint()?;
        self.pragmas.set_journal_mode(mode);
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
