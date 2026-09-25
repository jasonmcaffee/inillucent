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
    /// Neither direction moves a lock here. Raising to `exclusive` takes it at
    /// the next statement, because taking it now would make a pragma block on a
    /// lock the caller has not asked to wait for; dropping to `normal` releases
    /// it at the end of this statement, which is `leave`'s job and is the next
    /// thing that happens.
    ///
    /// **It used to release the file here, and that lost writes (task-1979,
    /// section 4, found again in task-1980).** `inillucent --db f batch "PRAGMA
    /// locking_mode = normal; INSERT ..."` runs both statements between one
    /// `enter` and one `leave`. The pragma released the file in the middle of
    /// that window, and the insert after it then ran believing the lock was
    /// still held - so two processes wrote the same file at once. Measured at
    /// six of a hundred and twenty acknowledged inserts missing, intermittently
    /// and only under load, which is what an unsynchronised write looks like
    /// from outside.
    ///
    /// @param exclusive - whether to keep the lock
    pub(crate) fn set_locking_exclusive(&mut self, exclusive: bool) -> DbResult<()> {
        self.pragmas.set_locking_exclusive(exclusive);
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
        // **A failure puts the count back (task-1979, section 4, measured
        // again in task-1980).** `execute_compiled` calls this with `?` and
        // reaches `leave` only on the way out of a statement that ran, so an
        // error here used to leave `running` raised for the life of the
        // connection. What followed was silent: every later statement read
        // `running > 1` as "inside a transaction", so it took no lock, made no
        // resynchronisation and never released - and `leave` returned early
        // because the count never reached zero, so nothing was ever committed
        // or checkpointed. The statements reported success and wrote nothing.
        //
        // Measured on `two_processes_attaching_one_file_lose_nothing`: one
        // writer refused at line 42 of a three hundred statement script by an
        // ordinary busy timeout, and the file then held forty of its rows with
        // no error printed for the other two hundred and fifty eight.
        let entered = self.enter_within(writing);
        if entered.is_err() {
            self.writing
                .set_running(self.writing.running().saturating_sub(1));
            // The same release `leave` performs, so a statement that could not
            // start leaves the file exactly as one that finished does.
            self.release_if_idle()?;
        }
        entered
    }

    /// Takes every lock one statement needs, and re-derives what has moved.
    ///
    /// The body of [`ImportedDatabase::enter`], separated so that the count it
    /// raises is put back on every path out of it.
    ///
    /// @param writing - whether the statement changes the database
    fn enter_within(&mut self, writing: bool) -> DbResult<()> {
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
        // **Two questions, because the log's tail alone does not answer it.**
        // This asked only whether the log had moved, and that is not sound: the
        // other process's own `leave` checkpoints, a checkpoint rolls a new
        // segment and deletes the ones below the new recovery point, so the
        // tail this connection remembers can be the length of a *different*
        // segment and compare equal. Measured: one to three of a hundred and
        // twenty acknowledged inserts missing, intermittently, after the two
        // larger holes in this path were closed.
        //
        // The meta record's generation is what closes it, and it is exactly
        // the case the tail cannot see - a checkpoint bumps the generation, and
        // a checkpoint is the only thing that retires a segment. Committed
        // records nobody has folded yet move the tail and not the generation,
        // which is why both are asked.
        //
        // **Re-deriving unconditionally was tried first and is wrong**: the
        // resynchronisation below builds a new log writer, and a new log writer
        // starts at the default `synchronous`, so `PRAGMA synchronous = NORMAL`
        // followed by `PRAGMA synchronous` read back FULL. It also threw away
        // an attached file's uncommitted pages under a `ROLLBACK` that had
        // already discarded them, which `attach.rs` reads as a row that
        // survived a rollback. Two page reads and a `file_size` per lock are
        // the price of not doing that.
        let moved =
            reloaded || (taking && !inside && (self.the_meta_moved()? || self.the_log_moved()?));
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
            let moved = reloaded
                || (taking
                    && !inside
                    && (self.attached_meta_moved(index)? || self.attached_log_moved(index)?));
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

    /// Reports whether one attached file's meta record names a generation
    /// other than the one this connection's cache describes.
    ///
    /// The attached half of [`ImportedDatabase::the_meta_moved`], and there for
    /// the same reason.
    ///
    /// @param index - which attachment
    fn attached_meta_moved(&mut self, index: usize) -> DbResult<bool> {
        let Some(held) = self.session_state.attached.get_mut(index) else {
            return Ok(false);
        };
        if held.path.is_none() {
            return Ok(false);
        }
        // The cheap check first, for the reason `the_meta_moved` gives.
        if held.database.disk_record_is_as_last_read()? {
            return Ok(false);
        }
        // Against the record last seen on the disk, for the reason
        // `the_meta_moved` gives.
        Ok(match held.database.meta_on_disk()? {
            Some(found) => found != *held.database.seen_on_disk(),
            None => true,
        })
    }

    /// Reports whether one attached file's log ends somewhere other than where
    /// this connection left it.
    ///
    /// The attached half of [`ImportedDatabase::the_log_moved`], and asked off
    /// this file's own open segment for the same reason: the check runs once
    /// per statement per attached file, and going through
    /// `inillucent_wal::tail_on_disk` made it a path lookup and a file open
    /// each time (task-1999).
    ///
    /// @param index - which attachment
    fn attached_log_moved(&self, index: usize) -> DbResult<bool> {
        let Some(held) = self.session_state.attached.get(index) else {
            return Ok(false);
        };
        if held.path.is_none() {
            return Ok(false);
        }
        let tail = held.wal.tail_of_open_segment()?;
        Ok(tail.sequence != held.wal.sequence() || tail.next_lsn != held.wal.written_end())
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
        let (wal, highest_txn) =
            crate::recovery::resync_file(&mut held.database, &vfs, &db_path, &doubtful)?;
        held.wal = wal;
        // Past every number the shared log holds - see `resync_file`.
        self.writing.raise_transactions_past(highest_txn);
        Ok(())
    }

    /// Reports whether the file's meta record names a generation other than the
    /// one this connection's cache describes.
    ///
    /// **The question [`ImportedDatabase::the_log_moved`] cannot answer.** A
    /// checkpoint by another process rolls a new log segment and deletes the
    /// ones below the new recovery point, so the tail this connection remembers
    /// can be the length of a different segment and compare equal - and the
    /// connection then reads its own stale pages with the lock in its hand.
    /// A checkpoint is also the one thing that moves the generation, so this
    /// reports exactly the case the tail misses.
    ///
    /// **The whole record, and unreadable counts as moved.** Comparing the
    /// generation alone left one insert of six hundred missing on the
    /// reviewer's two writer script, and both halves of that are fail-open
    /// answers: a record this connection cannot decode says "nothing changed",
    /// and a field that moved without the generation moving says the same. The
    /// record is `Eq`, so asking about all of it costs nothing over asking
    /// about one field of it, and the page has already been read.
    ///
    /// **Compared against the record this connection last saw on the disk, not
    /// against its own** (task-2000, design 1b). The two were the same thing until
    /// the fold became lazy: a statement folded on its way out, so the connection's
    /// record reached the file before the lock was released. They are not the same
    /// any more - a `CREATE TABLE` bumps `schema_cookie` and a `PRAGMA user_version`
    /// sets `user_version`, and neither reaches the file until a fold is due - so
    /// comparing against `meta` made the very next statement read *its own* pending
    /// edit as another process's write. It then resynchronised, which throws the
    /// pool away and replays the log from the file's checkpoint, and the statement
    /// that had just reported success was gone. `new_engine_ddl`'s
    /// `creating_a_table_writes_the_row_sqlite_writes` is the measurement: `CREATE
    /// TABLE plain (a, b)` ran, said ok, and was not in `sqlite_schema` afterwards.
    /// See `Database::disk_meta`.
    ///
    /// A database with no file has no second process and answers no.
    fn the_meta_moved(&mut self) -> DbResult<bool> {
        if self.storage.path.as_os_str().is_empty() {
            return Ok(false);
        }
        // **The record's own bytes, compared without decoding them, and read
        // once per lock acquisition rather than once per caller**
        // (task-2046). `begin_read` has already asked this question of the
        // same two slots a few instructions ago, under the same SHARED lock,
        // which a writer cannot hold at the same time - so the answer is
        // remembered and this costs nothing at all. What it replaced was a
        // second `meta_on_disk`: a buffer one page long allocated and zeroed
        // for each of the two slots, a whole page read into each, and a crc32
        // pass over each, at a 32 KiB default page size, to compare a record
        // 116 bytes long. Measured through
        // `Connection` on the medium fixture, `SELECT 1` cost 132.9 us outside
        // a transaction and 1.1 us inside one, and 89 us of the difference was
        // the two calls to this and to `begin_read`.
        //
        // The comparison decides nothing on its own: bytes that differ send
        // this straight to the full read and its checksum below, which is
        // unchanged and is still what says whether the record moved.
        if self.storage.database.disk_record_is_as_last_read()? {
            return Ok(false);
        }
        Ok(match self.storage.database.meta_on_disk()? {
            Some(found) => found != *self.storage.database.seen_on_disk(),
            None => true,
        })
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
    /// **It used to cost a path lookup and a file open instead, and that was
    /// 37% of an autocommit statement (task-1999).** This called
    /// `inillucent_wal::tail_on_disk`, which takes a *path* and walks forward
    /// from a sequence, asking `access` at each one and opening the file to
    /// read its header - so the sentence above about one `file_size` described
    /// an intention rather than the code. Under `locking_mode = normal` the
    /// check runs once a statement, and it was measured at 3.2 ms of an 8.8 ms
    /// autocommit statement, as much as the whole checkpoint beside it, while
    /// `the_meta_moved` next to it cost 0.03 ms. `Wal::tail_of_open_segment`
    /// answers the same question off the handle the log already holds open,
    /// and its own doc comment carries why the open segment is the only one
    /// that has to be asked.
    ///
    /// A database with no file - `:memory:` and a temporary one - has no
    /// segment and no second process, so it answers no.
    fn the_log_moved(&self) -> DbResult<bool> {
        if self.storage.path.as_os_str().is_empty() {
            return Ok(false);
        }
        let tail = self.storage.wal.tail_of_open_segment()?;
        Ok(tail.sequence != self.storage.wal.sequence()
            || tail.next_lsn != self.storage.wal.written_end())
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
        // **`synchronous` is the connection's, not the log writer's.**
        // `resync_file` builds a new writer, a new writer starts at the default
        // FULL, and `PRAGMA synchronous = NORMAL` on a connection that later
        // resynchronised read back 2. The pragma is set once and expected to
        // hold for the connection, so it is carried across.
        let synchronous = self.storage.wal.synchronous();
        let (wal, highest_txn) =
            crate::recovery::resync_file(&mut self.storage.database, &vfs, &db_path, &doubtful)?;
        self.storage.wal = wal;
        self.storage.wal.set_synchronous(synchronous);
        // Past every number the shared log holds - see `resync_file`.
        self.writing.raise_transactions_past(highest_txn);
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
        self.release_if_idle()
    }

    /// Releases every file this connection holds, when nothing is using them.
    ///
    /// Shared by [`ImportedDatabase::leave`] and by the failure path of
    /// [`ImportedDatabase::enter`], because a statement that could not start
    /// has to leave the file exactly as one that finished does.
    fn release_if_idle(&mut self) -> DbResult<()> {
        if self.writing.running() > 0
            || self.pragmas.locking_exclusive()
            || self.writing.batch().is_some()
        {
            return Ok(());
        }
        // **Durable before the file is let go, and after task-2000 that is one
        // sync of the log and nothing else.**
        //
        // What a lock release owes the next process is that every statement this
        // one acknowledged can be *found*, not that it is already folded into the
        // data file. The invariant moved with design 1b, and this is where it
        // moved from and to:
        //
        // - before: the file holds every acknowledged statement at release;
        // - now: **the file plus the log hold every acknowledged statement at
        //   release, and a connection that takes the lock replays what it has
        //   not seen before it reads or writes anything.**
        //
        // The second half is `enter_within`'s: `the_meta_moved` and
        // `the_log_moved` are asked on every take under `normal`, and a moved log
        // sends the connection through `resync_from_file`, which discards its
        // pages and replays the log from the file's own checkpoint. That replay
        // is what puts a statement another process left in the log into this
        // connection's pool, and it is deterministic, so two connections that
        // have both caught up hold identical pages.
        //
        // **What it cost to fold here, measured.** A fold per statement is six to
        // eight fsync class calls: the log, the journal's pre images and its
        // seal, the pages, the free map, the meta pages' journal, the meta record,
        // and the journal's unlink with its directory entry. `txn.autocommit` was
        // 8.7 ms a statement against SQLite's 1.17 and `write.insert.autocommit`
        // 8.5 against 4.06, which put `write`, `transaction` and `schema` under
        // `compat/perf/contract.toml`'s floor on four consecutive gate runs.
        //
        // **Removing it was tried once before and put back**, and what put it
        // back was not a lost write - the next process replayed the log and lost
        // nothing - but that eighteen suites saw the log where they expected the
        // file. Those suites now assert the file after a `close` or a `PRAGMA
        // wal_checkpoint` rather than after a release, which is the same property
        // stated at the moment it is true. The two process campaigns are the
        // correctness gate and are not weakened.
        //
        // **Only when this statement wrote something.** A fold is a write: it
        // rewrites the free map's pages, moves the meta record's generation and
        // retires log segments. Running one after a `SELECT` makes reading a
        // database change it, which `lifecycle.rs`'s `a_long_session_changes_no_byte`
        // compares byte for byte, and made four reads delete log segments a
        // handle still had open. A read takes SHARED and a write raises past it,
        // so the lock level is the question already answered; the dirty count is
        // the second half, for a statement that wrote and released before
        // reaching here.
        //
        // **Every file the connection holds, not only `main`.** A statement that
        // wrote an attached database and nothing else leaves `main` clean, so
        // asking `main` alone answered "nothing was written" and released every
        // file with the attachment's pages still dirty - which lost 116 of 599
        // acknowledged inserts through `ATTACH` under load.
        let wrote = self.storage.database.lock_level() > inillucent_vfs::FileLock::Shared
            || self.storage.database.pool().dirty_pages() > 0
            || self.session_state.attached.iter().any(|held| {
                held.path.is_some()
                    && (held.database.lock_level() > inillucent_vfs::FileLock::Shared
                        || held.database.pool().dirty_pages() > 0)
            });
        if wrote && self.storage.database.pool().lock_level() != inillucent_vfs::FileLock::None {
            // **The log, once.** `Wal::commit` already syncs under `synchronous =
            // FULL`, so for an autocommit statement this returns without doing
            // anything: `drive` compares the durable end against the point asked
            // for and stops. It is here for the statements whose commit did not
            // go through that path, and for `synchronous = NORMAL`, where a
            // release is a boundary the next process's replay starts from and a
            // buffered tail it could not read is not one.
            self.storage.wal.sync()?;
            for held in self.session_state.attached.iter() {
                if held.path.is_some() {
                    held.wal.sync()?;
                }
            }
            // **An `ATTACH`ed file still folds at release, and `main` does not.**
            //
            // The lazy fold rests on the lock handoff catching up: a connection
            // that takes the lock and finds the log moved replays what it has not
            // seen before it reads or writes anything. `main` does that through
            // `the_meta_moved`, `the_log_moved` and `resync_from_file`, and the two
            // process campaigns say it holds - a thousand statements alternating
            // between two processes with a fold forced every fifty lose nothing.
            //
            // **The attached path does not hold, measured** (task-2000, design 1b).
            // Two processes, each with its own `main` and both `ATTACH`ing one
            // shared file, sixty inserts each: a hundred and twenty acknowledged
            // and between one and fourteen in the file, with `ANALYZE` afterwards
            // reporting the table empty and every process exit zero. It is not the
            // detection - forcing a resynchronisation on every take made it worse,
            // not better, and took the count to nought - so the loss is inside the
            // resynchronisation of an attached file when another process holds
            // records in the same log: `resync_attached` throws the pool away,
            // replays, and then `truncate_after` cuts the shared log to what its
            // own scan reached. Under the eager fold that window never existed,
            // because the file held every statement at release and the replay had
            // nothing to rebuild.
            //
            // So the attachment keeps the fold it has always had, and `main` gets
            // the lazy one. That is where the measurement is: the performance
            // contract's `write`, `transaction` and `schema` families are all
            // `main`, the gate never attaches a file, and an application that
            // writes through `ATTACH` keeps today's cost rather than a correctness
            // hole. Making the attached handoff sound is its own ticket and its own
            // campaign; it is named in this one's closing comment.
            if self.any_attached_is_dirty() {
                self.checkpoint_attached_only()?;
            }
            // **And `main`'s fold only once its log has grown past the bar** - see
            // `crate::checkpoint::RECLAIM_BYTES` for why four mebibytes and why
            // bytes rather than pages. At the 32 KiB default page size an
            // autocommit statement writes about 34 KiB of log, so this is one fold
            // every hundred and twenty statements, and a hot leaf is imaged once
            // per fold however many statements touched it.
            if self.a_fold_is_due() {
                // `false`: a statement letting the file go is not somebody asking for
                // a checkpoint - see `checkpoint_of` for what the difference costs.
                self.checkpoint_of(false)?;
            }
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

    /// Takes the lock, folds every file, and lets the lock go.
    ///
    /// The body of `Drop for ImportedDatabase`, separated so the `Drop` has one
    /// statement in it and so this can say what it refuses and why.
    ///
    /// **Through `enter` and `leave`, exactly as a statement does.** A fold is a
    /// write - it rewrites the free map's pages, moves the meta record's
    /// generation and retires log segments - and doing one without the file lock
    /// is doing it while another process may be doing the same thing to the same
    /// bytes, which is task-1987's lost commits. Under `locking_mode = normal` the
    /// lock is not held between statements, so it is taken here and released
    /// again; under `exclusive` `enter` compares two integers and `leave` returns.
    ///
    /// A file the lock cannot be taken on - another process is writing it, and
    /// the busy timeout ran out - is left with its log, which the next open
    /// replays.
    pub(crate) fn fold_on_close(&mut self) -> DbResult<()> {
        // A read only connection has nothing to fold and a file handle that would
        // refuse the write; `:memory:` and a temporary database have no file.
        if self.storage.read_only || self.storage.path.as_os_str().is_empty() {
            return Ok(());
        }
        // An open transaction is a rollback, not a fold. See `Drop`.
        if self.writing.batch().is_some() {
            return Ok(());
        }
        // **Nothing to fold means nothing to lock, and that is not a shortcut.**
        // `enter(true)` takes the file EXCLUSIVE, and a connection that only read is
        // a connection with nothing the file does not already have - so folding it
        // would take a write lock on the way out of `inillucent query`. Under the
        // per-statement fold that never happened, because a read released without
        // checkpointing; with the fold at close it happened on every process,
        // including every short-lived one. It showed up as contention rather than as
        // a wrong answer: `cli`, `confinement`, `mcp_wire` and `budgets` each spawn
        // hundreds of processes against one file, and under the test runner's
        // twenty-four way parallelism they began reporting the file busy.
        //
        // Asked of the dirty count rather than of the log, because the question is
        // what *this* connection holds that the file does not. Another process's
        // unfolded records are in the log and are that process's to fold.
        //
        // **And of the rollback journal, which is the other half of that question
        // and was missing (task-2055).** A journal is created by the first page a
        // writeback puts in the file and is disposed of by `Journal::finish`,
        // which only a checkpoint reaches - so the more a small buffer pool
        // evicts, the larger the journal grows and the fewer frames are left
        // dirty, and a connection that evicted everything it had reached here
        // with nothing dirty and left a 686 KB journal beside the file. The next
        // open replayed it, which put the file back past writes that had been
        // acknowledged, and a `CREATE INDEX` built straight into the data file
        // does not survive that: its pages are in no log record, so nothing
        // replays them forward again. `story_nikaya`'s small-pool arm reopened
        // on `a key below separator 0 is in the child above it`.
        //
        // It costs a read-only connection nothing, which is what the paragraph
        // above is protecting: a journal is only ever written by a *dirty*
        // page's writeback, so a session that read and evicted clean frames has
        // none and still releases without taking the file exclusively.
        let holding = self.holds_what_the_file_does_not(&self.storage.database)
            || self.session_state.attached.iter().any(|held| {
                held.path.is_some() && self.holds_what_the_file_does_not(&held.database)
            });
        if !holding {
            return Ok(());
        }
        self.enter(true)?;
        let folded = self.checkpoint();
        let left = self.leave();
        folded.and(left)
    }

    /// Reports whether one file holds something the data file does not.
    ///
    /// Two things count, and a fold on the way out is owed for either: a dirty
    /// frame, whose contents have not reached the file, and a rollback journal
    /// with pre-images in it, which would move the file *backwards* at the next
    /// open. See [`Self::fold_on_close`] for what leaving the second one behind
    /// cost (task-2055).
    ///
    /// @param database - the file to ask about
    fn holds_what_the_file_does_not(&self, database: &inillucent_pool::Database) -> bool {
        database.pool().dirty_pages() > 0 || database.pool().journal_is_hot()
    }

    /// Reports whether any file this connection holds has piled up enough log
    /// to be worth folding.
    ///
    /// **The bar, and the only thing that decides a fold on the release path**
    /// (task-2000, design 1b). A fold runs at exactly four points now: here, when
    /// a file's log passes [`crate::checkpoint::RECLAIM_BYTES`]; when the
    /// connection is dropped, so a closed file is self contained; when a caller
    /// asks, through `PRAGMA wal_checkpoint`, `VACUUM`, a backup, an integrity
    /// check or the driver's `checkpoint`; and before a switch of `journal_mode`
    /// or `locking_mode`.
    ///
    /// Asked of every file, because each has its own log and its own size. One
    /// file over the bar folds all of them, which is what `checkpoint` does
    /// anyway and costs the others nothing - a file with no dirty page writes no
    /// page.
    /// **Every journal mode, because the mode decides what protects a fold and not
    /// when one runs** (task-2000, design 1). The two halves of design 1 come apart:
    /// the **after images** are `wal`'s and stay there, since they are what replaces
    /// a rollback journal and a caller who asks for `journal_mode = delete` is asking
    /// for `app.db-journal` beside the database. Deferring the fold is the other
    /// half, and the rollback journal protects the fold's in place writes whenever
    /// the fold happens, so a deferred one is safe in `delete` by the same argument
    /// that makes an eager one safe.
    ///
    /// **And the measurement is why it has to be every mode.** task-2000 states its
    /// goals for "the shipped defaults, `locking_mode = normal` and
    /// `journal_mode = wal`", and says of design 1 that it "changes `wal` mode, which
    /// is the default". `wal` is not the default and was not when that was written:
    /// `Pragmas::fresh` starts at `JournalMode::Delete` for SQLite parity, and the
    /// gate sets `journal_mode = delete` on both arms. A fold deferred in `wal` alone
    /// is a fold deferred in a mode nothing measures, so the goals could not be
    /// reached by honouring that sentence.
    ///
    /// What deferring does change in a rollback mode is where an acknowledged commit
    /// lives until the fold: in the log, and only there. An eager fold put it in the
    /// log *and* in the data file before `COMMIT` returned, and that second copy is
    /// what let `delete` mode survive a device which acknowledges a write and stores
    /// half of it. The redundancy was the second write and the second sync this
    /// design removes, rather than anything the journal did.
    /// `durability::a_short_write_at_every_cut_point_is_recoverable` is where that is
    /// argued and counted, and `wal_crash.rs` is where the same exemption was already
    /// granted for `wal`.
    fn a_fold_is_due(&self) -> bool {
        self.storage.wal.since_checkpoint() >= crate::checkpoint::RECLAIM_BYTES
    }

    /// Reports whether any attached file holds a change its own file does not.
    ///
    /// The condition the attachment's eager fold rides on - see `release_if_idle`
    /// for why an attached file still folds at release when `main` does not. Asked
    /// off the dirty count rather than off the lock level, because a statement that
    /// wrote `main` alone raises the attachment's lock too and folding it then would
    /// make reading a database change it.
    fn any_attached_is_dirty(&self) -> bool {
        self.session_state.attached.iter().any(|held| {
            held.path.is_some()
                && held.database.lock_level() > inillucent_vfs::FileLock::Shared
                && held.database.pool().dirty_pages() > 0
        })
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
        // **And the fold's protection follows the mode** (task-2000, design 1a).
        // Under `wal` the fold appends an after image of every page it is about
        // to write to the log it already has, so it asks the journal for
        // nothing; the journal stays in place for an eviction, which is undo and
        // needs a pre image. See `Pool::fold_protected_by_log`.
        self.storage
            .database
            .pool()
            .set_fold_protected_by_log(mode == inillucent_pool::journal::JournalMode::Wal);
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
            // The same rule `main` takes two blocks above.
            held.database
                .pool()
                .set_fold_protected_by_log(mode == inillucent_pool::journal::JournalMode::Wal);
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
/// `crates/inillucent-compat/tests/durability/wal_crash.rs`'s checkpoint campaign found
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
/// **In `wal` mode the fold no longer asks it, and an eviction still does**
/// (task-2000, design 1a). The paragraph above is the argument for a journal and it
/// is answered differently now: immediately before the fold writes any page in
/// place, it appends a `Body::WritePage` **after** image of every page it is about
/// to write to the redo log it already has, and syncs behind them. Recovery installs
/// one idempotently by page LSN, and a page whose checksum fails reads as "no LSN",
/// so the torn page the campaign found is repaired from the log rather than put back
/// from a journal. That is cheaper by the whole of the journal's read before write,
/// its two seals and its unlink with the directory sync.
///
/// The journal object is still handed out, because **an eviction is a different
/// question**. Stealing an uncommitted page out to the file is undo, and a redo log
/// cannot do undo, so `Pool::writeback` asks the journal for `Writing::Eviction` and
/// not for a fold - see `Pool::fold_protected_by_log`. The file is created at the
/// first pre image, so a connection that never steals never creates one, and a
/// transaction whose dirty pages outgrow the pool can still steal.
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
