//! The engine: a database file, its log, and the transactions over both.
//!
//! Invariant: **a page is never written to the data file before the log record
//! that describes it is durable.** This module is where the two halves are tied
//! together - it is the only thing that holds both a [`Database`] and a [`Wal`],
//! and it is the thing that calls [`inillucent_pool::Pool::set_durable_lsn`]
//! after every sync of the log and never before one. A watermark that ran ahead
//! of the media would turn the pool's check into a formality that always passes,
//! which is worse than no check because it looks like one.
//!
//! ## Opening is recovery
//!
//! There is no "open cleanly" path that skips the log. Every open scans from the
//! meta page's `checkpoint_lsn`, replays the committed prefix, truncates the
//! torn tail and then starts writing where the scan stopped. A database that was
//! closed cleanly has an empty scan and pays one segment-header read for it,
//! which is the right price for not having a second, less-tested way to open a
//! file.
//!
//! ## The commit gate
//!
//! Commit does four things in one order that is not negotiable:
//!
//! 1. take the gate, so commit order is one order for everybody;
//! 2. assign the `cts` and append the `Commit` record **under the gate**, so
//!    commit order equals visibility order equals log order - which is what
//!    lets recovery reconstruct visibility from log order alone;
//! 3. publish the undo buffer to the version log tagged with that `cts`;
//! 4. release the gate and wait for durability under the `synchronous` policy.
//!
//! Step 4 is outside the gate on purpose: that is what lets the next committer
//! append while this one is waiting on a sync, and it is the whole of group
//! commit. Step 2 is inside it because a `cts` assigned outside would let two
//! commits become visible in an order their log records do not agree with.
//!
//! If step 4 fails, step 3 is taken back out with
//! [`crate::version::VersionLog::withdraw`]. A commit that published its
//! before-images and then could not make its record durable would otherwise
//! leave readers seeing a transaction recovery will not replay.

use std::cell::{Cell, RefCell};
use std::sync::{Arc, Mutex};

use inillucent_base::error::misuse;
use inillucent_base::DbResult;
use inillucent_pool::{Database, Options, Pool};
use inillucent_vfs::{DbPath, Vfs};
use inillucent_wal::record::Body;
use inillucent_wal::recover::{self, Recovered, RecoveryStart};
use inillucent_wal::writer::{Wal, WalOptions};
use inillucent_wal::Synchronous;

use crate::redo::{Applier, RowRedo};
use crate::slot::{WriterGuard, WriterSlot};
use crate::undo::{Undo, UndoBuffer};
use crate::version::{Clock, Cts, Snapshot, TxnId, VersionLog, Visible};

/// How a transaction begins.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Begin {
    /// Take the snapshot now and the writer slot only if a write happens.
    Deferred,
    /// Take the writer slot now, so the first write cannot fail with `BUSY`.
    Immediate,
}

/// What the engine has done.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct EngineStats {
    /// Transactions begun.
    pub begun: u64,
    /// Transactions committed.
    pub committed: u64,
    /// Transactions rolled back.
    pub rolled_back: u64,
    /// Checkpoints taken.
    pub checkpoints: u64,
    /// Before-images collected from the version log.
    pub versions_collected: u64,
}

/// Restores a row to its before-image during a rollback.
pub trait UndoSink {
    /// Puts one row back the way it was.
    ///
    /// @param undo - the tree, key and before-image
    fn restore(&mut self, undo: &Undo) -> DbResult<()>;
}

/// An `UndoSink` that refuses, for a caller with no tree to restore into.
#[derive(Debug, Default)]
pub struct RefuseUndo;

impl UndoSink for RefuseUndo {
    fn restore(&mut self, undo: &Undo) -> DbResult<()> {
        Err(misuse(format!(
            "rolling back a change to tree {} needs a tree to restore into",
            undo.tree
        )))
    }
}

/// An `UndoSink` that records what it was asked to restore, and does nothing.
///
/// For the tests of the transaction machinery itself, which have no tree. It
/// keeps the entries so a test asserts on *what* would be restored and in what
/// order, rather than on the rollback having been called.
#[derive(Debug, Default)]
pub struct RecordingUndo {
    /// What the rollback asked for, in the order it asked.
    pub restored: Vec<Undo>,
}

impl UndoSink for RecordingUndo {
    fn restore(&mut self, undo: &Undo) -> DbResult<()> {
        self.restored.push(undo.clone());
        Ok(())
    }
}

/// A database file, its log, and the transaction machinery over both.
pub struct Engine {
    database: RefCell<Database>,
    wal: Wal,
    clock: Clock,
    versions: RefCell<VersionLog>,
    slot: Arc<WriterSlot>,
    /// Serialises `cts` assignment and `Commit` appends, so the two orders and
    /// the log's order are one order.
    gate: Mutex<()>,
    next_txn: Cell<u64>,
    /// The first LSN of the oldest transaction that has logged anything and has
    /// not finished, or `u64::MAX` when there is none.
    ///
    /// This is what a checkpoint may not advance recovery past, because
    /// no-steal means that transaction's pages are not in the file.
    oldest_open_lsn: Cell<u64>,
    stats: Cell<EngineStats>,
    path: DbPath,
    vfs: Arc<dyn Vfs>,
    /// What the last recovery found, kept so a caller can report it.
    recovered: Recovered,
}

/// How to open an engine.
#[derive(Clone, Debug)]
pub struct EngineOptions {
    /// The page size and pool size.
    pub database: Options,
    /// The sync policy and segment size.
    pub wal: WalOptions,
    /// How long a would-be writer waits for the writer slot.
    pub busy_timeout_ms: u64,
}

impl Default for EngineOptions {
    fn default() -> EngineOptions {
        EngineOptions {
            database: Options::default(),
            wal: WalOptions::default(),
            busy_timeout_ms: 0,
        }
    }
}

impl Engine {
    /// Creates a fresh database with an empty log.
    ///
    /// @param vfs - the file system
    /// @param path - where to create it
    /// @param options - the page size, pool size, sync policy and timeout
    pub fn create(vfs: Arc<dyn Vfs>, path: &DbPath, options: EngineOptions) -> DbResult<Engine> {
        let database = Database::create(vfs.as_ref(), path, options.database)?;
        Engine::assemble(vfs, path, database, options, Recovered::default())
    }

    /// Opens an existing database, recovering its log.
    ///
    /// The `rows` argument applies the logical row records. A caller with no
    /// tree passes [`crate::redo::RefuseRows`] and finds out that the log needed
    /// one, rather than opening a database that is missing the rows it names.
    ///
    /// @param vfs - the file system
    /// @param path - the database file
    /// @param options - the pool size, sync policy and timeout
    /// @param rows - what to do with the logical row records
    pub fn open<R: RowRedo>(
        vfs: Arc<dyn Vfs>,
        path: &DbPath,
        options: EngineOptions,
        rows: R,
    ) -> DbResult<Engine> {
        let mut database = Database::open(vfs.as_ref(), path, options.database.frames)?;
        let start = RecoveryStart {
            uuid: database.uuid(),
            checkpoint_lsn: database
                .meta()
                .checkpoint_lsn
                .max(inillucent_wal::FIRST_LSN),
            sequence: database.meta().wal_sequence.max(1),
            cts_watermark: database.meta().cts_watermark,
        };
        let (outcome, allocated, freed) = {
            let mut applier = Applier::new(&mut database, rows);
            let outcome = recover::recover(vfs.as_ref(), path, start, &mut applier)?;
            let (allocated, freed) = applier.allocations();
            (outcome, allocated.to_vec(), freed.to_vec())
        };
        // "Rebuild the free map if any AllocPage or FreePage was replayed."
        // Done after the scan rather than inside it, because the free map and
        // every page write are both behind `&mut Database` and one record
        // cannot hold two mutable borrows of the same object.
        for page in &allocated {
            database.claim(*page)?;
        }
        for page in &freed {
            database.release(*page, 1)?;
        }
        recover::truncate_after(vfs.as_ref(), path, &outcome)?;
        Engine::assemble(vfs, path, database, options, outcome)
    }

    /// Ties a database and a log together.
    ///
    /// @param vfs - the file system
    /// @param path - the database file
    /// @param database - the open file
    /// @param options - the sync policy and timeout
    /// @param recovered - what recovery found
    fn assemble(
        vfs: Arc<dyn Vfs>,
        path: &DbPath,
        database: Database,
        options: EngineOptions,
        recovered: Recovered,
    ) -> DbResult<Engine> {
        let uuid = database.uuid();
        let next_lsn = if recovered.next_lsn == 0 {
            inillucent_wal::FIRST_LSN
        } else {
            recovered.next_lsn
        };
        let sequence = recovered.sequence.max(1);
        let wal = Wal::open(
            Arc::clone(&vfs),
            path,
            uuid,
            next_lsn,
            sequence,
            options.wal.clone(),
        )?;
        // The pool may write pages up to what the log has already made durable,
        // which after a recovery is everything the scan accepted.
        database.pool().set_durable_lsn(wal.durable_end());
        let slot = Arc::new(WriterSlot::new());
        slot.set_busy_timeout_ms(options.busy_timeout_ms);
        Ok(Engine {
            database: RefCell::new(database),
            wal,
            clock: Clock::new(recovered.latest_cts),
            versions: RefCell::new(VersionLog::new()),
            slot,
            gate: Mutex::new(()),
            next_txn: Cell::new(1),
            oldest_open_lsn: Cell::new(u64::MAX),
            stats: Cell::new(EngineStats::default()),
            path: path.clone(),
            vfs,
            recovered,
        })
    }

    /// Returns what the open's recovery found.
    pub fn recovered(&self) -> &Recovered {
        &self.recovered
    }

    /// Returns the log.
    pub fn wal(&self) -> &Wal {
        &self.wal
    }

    /// Returns the writer slot, so a pragma can set `busy_timeout`.
    pub fn slot(&self) -> &Arc<WriterSlot> {
        &self.slot
    }

    /// Returns the clock, for a caller that wants the newest timestamp.
    pub fn clock(&self) -> &Clock {
        &self.clock
    }

    /// Returns the counters.
    pub fn stats(&self) -> EngineStats {
        self.stats.get()
    }

    /// Returns the database file's path.
    pub fn path(&self) -> &DbPath {
        &self.path
    }

    /// Runs `read` against the buffer pool.
    ///
    /// The pool is behind a `RefCell` because the free map and the meta record
    /// beside it need `&mut`, and a caller that only wants to read a page should
    /// not have to know that.
    ///
    /// @param read - what to do with the pool
    pub fn with_pool<R>(&self, read: impl FnOnce(&Pool) -> R) -> R {
        let database = self.database.borrow();
        read(database.pool())
    }

    /// Runs `change` against the database file.
    ///
    /// @param change - what to do with the file
    pub fn with_database<R>(&self, change: impl FnOnce(&mut Database) -> R) -> R {
        let mut database = self.database.borrow_mut();
        change(&mut database)
    }

    /// Says what a reader at `snapshot` should do with one key.
    ///
    /// @param tree - the tree the key is in
    /// @param key - the key's encoded bytes
    /// @param snapshot - the reader's snapshot
    /// @param read - what to do with the answer, while the log is borrowed
    pub fn visible<R>(
        &self,
        tree: u64,
        key: &[u8],
        snapshot: &Snapshot,
        read: impl FnOnce(Visible<'_>) -> R,
    ) -> R {
        let versions = self.versions.borrow();
        read(versions.visible(tree, key, snapshot.cts()))
    }

    /// Returns how many before-images the version log holds.
    pub fn versions_held(&self) -> usize {
        self.versions.borrow().len()
    }

    /// Begins a transaction.
    ///
    /// @param how - deferred or immediate
    pub fn begin(&self, how: Begin) -> DbResult<Transaction<'_>> {
        let id = TxnId(self.next_txn.get());
        self.next_txn.set(self.next_txn.get().saturating_add(1));
        let writer = match how {
            Begin::Immediate => Some(WriterSlot::acquire(&self.slot, id)?),
            Begin::Deferred => None,
        };
        let mut stats = self.stats.get();
        stats.begun = stats.begun.saturating_add(1);
        self.stats.set(stats);
        Ok(Transaction {
            engine: self,
            id,
            snapshot: self.clock.snapshot(),
            writer,
            undo: UndoBuffer::new(),
            first_lsn: u64::MAX,
            finished: false,
        })
    }

    /// Takes a checkpoint: every dirty page out, then the meta record.
    ///
    /// Refuses to advance recovery past the oldest open transaction, because
    /// no-steal means that transaction's pages are not in the file and starting
    /// recovery above its first record would lose them.
    pub fn checkpoint(&self) -> DbResult<u64> {
        // The log is synced first, so that every page about to be written is
        // one the log has already described durably. Doing it the other way
        // round is the durability-order mutant this phase exists to kill.
        self.wal.sync()?;
        let durable = self.wal.durable_end();
        self.with_pool(|pool| pool.set_durable_lsn(durable));
        let recovery_from = durable.min(self.oldest_open_lsn.get());
        let watermark = self.clock.latest();
        let sequence = self.wal.sequence();
        self.with_database(|database| -> DbResult<()> {
            database.set_log_position(recovery_from, watermark, sequence);
            database.checkpoint()
        })?;
        self.wal.note_checkpoint(recovery_from, watermark)?;
        let durable = self.wal.durable_end();
        self.with_pool(|pool| pool.set_durable_lsn(durable));
        self.wal.retire_segments_below(recovery_from)?;
        let mut stats = self.stats.get();
        stats.checkpoints = stats.checkpoints.saturating_add(1);
        self.stats.set(stats);
        Ok(recovery_from)
    }

    /// Discards every before-image no active snapshot can reach.
    pub fn collect_versions(&self) -> usize {
        let active = self.clock.active_timestamps();
        let dropped = self.versions.borrow_mut().collect(&active);
        let mut stats = self.stats.get();
        stats.versions_collected = stats.versions_collected.saturating_add(dropped as u64);
        self.stats.set(stats);
        dropped
    }

    /// Sets the sync policy.
    ///
    /// @param policy - the new policy
    pub fn set_synchronous(&self, policy: Synchronous) {
        self.wal.set_synchronous(policy);
    }

    /// Returns the file system the engine was opened on.
    pub fn vfs(&self) -> &Arc<dyn Vfs> {
        &self.vfs
    }

    /// Records that a transaction has logged its first record.
    ///
    /// Set unconditionally, and the writer slot is what makes that correct: the
    /// caller only reaches here on its *own* first record, and there is at most
    /// one transaction holding the slot, so there is never a second open
    /// transaction whose earlier position this would overwrite. A guard against
    /// that would be a branch no input can take, which is the shape the coverage
    /// gate can only be lied to about - and the assumption it would be guarding
    /// is stated on [`Engine::note_finished`], where a later phase with
    /// concurrent writers has to change both.
    ///
    /// @param lsn - the record's LSN
    fn note_first_record(&self, lsn: u64) {
        debug_assert_eq!(
            self.oldest_open_lsn.get(),
            u64::MAX,
            "a second transaction logged while one was already open"
        );
        self.oldest_open_lsn.set(lsn);
    }

    /// Records that the writer finished, so a checkpoint may advance again.
    ///
    /// With one writer slot there is at most one transaction that has logged
    /// anything and not finished, so "the oldest open" is "the one that was
    /// open". A design with concurrent writers would keep a set here; the slot
    /// is what makes a single cell correct, and that is stated because it is the
    /// assumption a later phase would break.
    fn note_finished(&self) {
        self.oldest_open_lsn.set(u64::MAX);
    }
}

/// One transaction.
pub struct Transaction<'e> {
    engine: &'e Engine,
    id: TxnId,
    snapshot: Snapshot,
    writer: Option<WriterGuard>,
    undo: UndoBuffer,
    /// The LSN of this transaction's first record, or `u64::MAX`.
    first_lsn: u64,
    finished: bool,
}

impl Transaction<'_> {
    /// Returns the transaction's id.
    pub fn id(&self) -> TxnId {
        self.id
    }

    /// Returns the snapshot the transaction reads at.
    pub fn snapshot(&self) -> &Snapshot {
        &self.snapshot
    }

    /// Reports whether the transaction holds the writer slot.
    pub fn is_writer(&self) -> bool {
        self.writer.is_some()
    }

    /// Reports whether the transaction has changed anything.
    pub fn has_written(&self) -> bool {
        !self.undo.is_empty() || self.first_lsn != u64::MAX
    }

    /// Returns the open savepoints, outermost first.
    pub fn savepoints(&self) -> Vec<&str> {
        self.undo.savepoints()
    }

    /// Takes the writer slot, if it is not held already.
    ///
    /// A deferred transaction calls this on its first write, which is where
    /// `BUSY` can happen; an immediate one took the slot at `BEGIN` and this is
    /// a no-op. That difference is the whole of `BEGIN IMMEDIATE`.
    pub fn become_writer(&mut self) -> DbResult<()> {
        if self.writer.is_none() {
            self.writer = Some(WriterSlot::acquire(&self.engine.slot, self.id)?);
        }
        Ok(())
    }

    /// Appends a record to the log on this transaction's behalf.
    ///
    /// Takes the writer slot first, because a transaction that logged without
    /// holding it would be a second writer.
    ///
    /// @param body - what happened
    pub fn log(&mut self, body: Body<'_>) -> DbResult<u64> {
        self.become_writer()?;
        let lsn = self.engine.wal.append(self.id.0, body)?;
        if self.first_lsn == u64::MAX {
            self.first_lsn = lsn;
            self.engine.note_first_record(lsn);
        }
        Ok(lsn)
    }

    /// Records what a row held before this transaction changed it.
    ///
    /// Called **before** the change. A caller that recorded afterwards would
    /// record the new value as the old one, and every test of a single change
    /// would pass.
    ///
    /// @param tree - the tree the row is in
    /// @param key - the row's key, encoded
    /// @param before - the row's bytes, or `None` when it did not exist
    pub fn record_undo(&mut self, tree: u64, key: Vec<u8>, before: Option<Vec<u8>>) {
        self.undo.record(tree, key, before);
    }

    /// Opens a savepoint.
    ///
    /// @param name - the savepoint's name
    pub fn savepoint(&mut self, name: &str) {
        self.undo.savepoint(name);
    }

    /// Rolls back to a savepoint.
    ///
    /// @param name - the savepoint to roll back to
    /// @param sink - what puts the rows back
    pub fn rollback_to(&mut self, name: &str, sink: &mut dyn UndoSink) -> DbResult<usize> {
        let undone = self.undo.rollback_to(name)?;
        for entry in &undone {
            sink.restore(entry)?;
        }
        Ok(undone.len())
    }

    /// Closes a savepoint, keeping its work.
    ///
    /// @param name - the savepoint to release
    pub fn release(&mut self, name: &str) -> DbResult<()> {
        self.undo.release(name)
    }

    /// Rolls the whole transaction back.
    ///
    /// No `Abort` record is written when nothing was flushed, which is the
    /// common case: the records are still in the log's buffer and a transaction
    /// that never reached the media does not need a record saying it did not.
    /// When something *was* flushed - a large transaction, or a `NORMAL` policy
    /// that wrote without syncing - an `Abort` is appended so recovery knows not
    /// to replay it.
    ///
    /// @param sink - what puts the rows back
    pub fn rollback(&mut self, sink: &mut dyn UndoSink) -> DbResult<()> {
        let undone = self.undo.take_all();
        for entry in &undone {
            sink.restore(entry)?;
        }
        if self.first_lsn != u64::MAX && self.engine.wal.written_end() > self.first_lsn {
            self.engine.wal.append(self.id.0, Body::Abort)?;
        }
        self.finish(false);
        Ok(())
    }

    /// Commits.
    ///
    /// Returns the commit timestamp. A transaction that changed nothing gets the
    /// current timestamp and writes nothing, which is what makes a read-only
    /// `BEGIN ... COMMIT` free.
    pub fn commit(&mut self) -> DbResult<Cts> {
        if !self.has_written() {
            let cts = self.engine.clock.latest();
            self.finish(true);
            return Ok(cts);
        }
        let (cts, end) = {
            // The gate. Inside it: assign the timestamp and append the commit
            // record, so commit order equals visibility order equals log order.
            let _gate = self
                .engine
                .gate
                .lock()
                .map_err(|_| misuse("the commit gate was poisoned by a panicking writer"))?;
            let cts = self.engine.clock.commit();
            self.engine.wal.append(self.id.0, Body::Commit { cts })?;
            let end = self.engine.wal.next_lsn();
            let images = self.undo.take_for_publication();
            self.engine.versions.borrow_mut().publish(cts, images);
            (cts, end)
        };
        // Outside the gate: the wait for durability, which is what lets the next
        // committer append while this one is waiting on a sync. `await_commit`
        // rather than `commit`, because the record has already been written -
        // calling `commit` here appended a *second* one for every transaction.
        match self.engine.wal.await_commit(end) {
            Ok(()) => {}
            Err(error) => {
                // The before-images were published and the record is not
                // durable, so readers would be seeing a transaction recovery
                // will not replay. Take them back out.
                let _ = self.engine.versions.borrow_mut().withdraw(cts);
                self.finish(false);
                return Err(error);
            }
        }
        let durable = self.engine.wal.durable_end();
        self.engine.with_pool(|pool| pool.set_durable_lsn(durable));
        self.finish(true);
        Ok(cts)
    }

    /// Releases the writer slot and updates the counters.
    ///
    /// @param committed - whether the transaction committed
    fn finish(&mut self, committed: bool) {
        self.finished = true;
        self.writer = None;
        self.engine.note_finished();
        let mut stats = self.engine.stats.get();
        if committed {
            stats.committed = stats.committed.saturating_add(1);
        } else {
            stats.rolled_back = stats.rolled_back.saturating_add(1);
        }
        self.engine.stats.set(stats);
    }
}

impl Drop for Transaction<'_> {
    /// A transaction that goes out of scope without committing is rolled back.
    ///
    /// It cannot *undo* here - restoring a row needs a tree and a `Drop` has
    /// nowhere to get one - so what it does is release the writer slot and count
    /// the rollback. The pages it dirtied are still dirty, and the thing that
    /// makes that safe is no-steal: they were never written to the file, so
    /// discarding them is the whole of undoing them. A caller that wants its
    /// in-memory pages restored calls `rollback` with a sink.
    fn drop(&mut self) {
        if !self.finished {
            self.finish(false);
        }
    }
}

impl std::fmt::Debug for Transaction<'_> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Transaction")
            .field("id", &self.id.0)
            .field("snapshot", &self.snapshot.cts())
            .field("writer", &self.writer.is_some())
            .field("undo", &self.undo.len())
            .finish()
    }
}
