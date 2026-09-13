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
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use inillucent_base::error::misuse;
use inillucent_base::DbResult;
use inillucent_pool::page::{self, header};
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

/// How many before-images the version log holds before the first sweep.
///
/// Small enough that an ordinary write workload never accumulates megabytes,
/// large enough that a hundred single-row commits do not each pay for a walk.
const COLLECT_FLOOR: usize = 1024;

/// A database file, its log, and the transaction machinery over both.
pub struct Engine {
    database: RefCell<Database>,
    wal: Wal,
    clock: Clock,
    /// The before-image of every key the **open writer** has changed and not
    /// yet committed, keyed by tree and key.
    ///
    /// The version log holds what *committed* transactions overwrote, and it is
    /// filled at commit - which leaves the window between a write and its commit
    /// uncovered. In that window the changed page is in the buffer pool, so a
    /// reader that consulted only the version log read the uncommitted value:
    /// a **dirty read**, and the model campaign found it on its first seed.
    ///
    /// This is that window. One map rather than one per transaction because
    /// there is one writer slot, so there is at most one transaction whose
    /// changes are uncommitted at any moment; a design with concurrent writers
    /// would key it by transaction and is what this comment is here to warn.
    uncommitted: RefCell<BTreeMap<(u64, Vec<u8>), Option<Vec<u8>>>>,
    /// The pool's no-steal watermark, held directly.
    ///
    /// Through a handle rather than through the file, because it is moved from
    /// inside the tree mutation that holds the file mutably - see
    /// `Pool::uncommitted_handle`.
    uncommitted_lsn: Arc<AtomicU64>,
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
    /// How many before-images the version log may hold before a commit collects.
    ///
    /// **Because nothing was collecting them at all.** `collect_versions` was
    /// written in Phase 3, tested, and then called by nothing but its own
    /// tests, so every before-image every write had ever published stayed for
    /// the life of the connection. The cost was measured on the gate's write
    /// family: 7.5 MiB of a round, held by rows no snapshot could reach.
    ///
    /// It is a threshold rather than a collect-per-commit because collecting
    /// walks the whole log: doing it after every commit would be quadratic in
    /// the images a batch publishes. The threshold doubles past whatever
    /// survived the last sweep, so a long reader that legitimately holds a
    /// thousand images is swept when the log reaches two thousand and not
    /// repeatedly at a thousand - which is the same amortisation a growing
    /// vector uses, for the same reason.
    collect_at: Cell<usize>,
    stats: Cell<EngineStats>,
    path: DbPath,
    vfs: Arc<dyn Vfs>,
    /// What the last recovery found, kept so a caller can report it.
    recovered: Recovered,
}

/// How to open an engine.
#[derive(Clone, Debug, Default)]
pub struct EngineOptions {
    /// The page size and pool size.
    pub database: Options,
    /// The sync policy and segment size.
    pub wal: WalOptions,
    /// How long a would-be writer waits for the writer slot.
    pub busy_timeout_ms: u64,
}

/// Returns where the log resumes, raising it above every stamp the file carries.
///
/// **A page's LSN has to be a position in the stream currently beside the file,
/// and after a recovery whose chain was short of what the pages reflect it is
/// not.** Recovery applies a record to a page only when the page's
/// stamp is below the record's, so a page stamped by a stream that no longer
/// exists silently swallows every later write to it: the record is skipped and
/// nothing anywhere says a committed row was lost.
///
/// The log therefore resumes at `max(recovered.next_lsn, high_water + 1)`. In a
/// healthy file the first term already wins - the write-ahead rule puts every
/// stamp below the log's durable end, and the durable end is at or below where
/// recovery stopped - so this fires only on a file whose log is short of what
/// its pages carry.
///
/// A jump takes the **next** sequence, because a record's write offset is
/// `header + (lsn - segment.first_lsn)` and resuming far into the current
/// segment would ask for a file that size; and it checkpoints the meta page
/// first, because the jump leaves a gap that `read_chain` would otherwise stop
/// the next recovery at.
///
/// @param database - the recovered file, whose pool carries the high water
/// @param outcome - what recovery found
fn resume_above_every_stamp(database: &mut Database, outcome: &Recovered) -> DbResult<(u64, u64)> {
    let next_lsn = outcome.next_lsn.max(inillucent_wal::FIRST_LSN);
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

/// Logs and installs the free map's own pages, ahead of a checkpoint that
/// would otherwise rewrite them with nothing behind the write.
///
/// **The defect this closes:** `Database::checkpoint` rewrites every free-map
/// page on every checkpoint, because `FreeMap` keeps no per-page dirty bit -
/// see `Database::free_map_pages`. Done through `Pool::install` alone, that
/// rewrite carries no LSN stamp and no log record, so once an earlier
/// checkpoint's own record for a page's last real change has retired, a crash
/// partway through a later, otherwise-redundant rewrite leaves a torn page
/// with nothing for redo to repair it from - `load_free_map` then fails its
/// checksum on the next open, unconditionally, rather than reading either the
/// state before this checkpoint or the state after it.
///
/// **The fix reuses the convention `AllocPage` and `FreePage` already use.**
/// Both are logged under transaction `0` because a free-map bit "belongs to
/// no transaction and is always part of the prefix" - `should_replay` says so
/// in so many words - and this does the same for the page's own bytes: one
/// `Body::WritePage` record per free-map page, txn `0`, so recovery replays
/// it unconditionally rather than needing a matching `Commit`. The image is
/// then stamped with that record's own LSN before it is installed, exactly as
/// `Applier::put_image` stamps one on replay, so the same two guards that
/// protect every other page protect this one too: `refuse_if_ahead_of_the_log`
/// will not let the page reach disk before its record does, and a torn copy
/// of it is repaired by the very same `put_image` on the next recovery.
///
/// **The invariant this buys back:** after a crash at any point during a
/// checkpoint, a reopen's free map is either exactly what it held before this
/// checkpoint began, or exactly what it holds once this checkpoint's own
/// `WritePage` records are all replayed - never a torn mixture of the two,
/// because every byte that changes is behind a record before it is written
/// and the page-LSN rule makes a record's replay a no-op once its bytes are
/// already durable.
///
/// **The caller still owns the durability order.** This only appends records
/// and installs the stamped pages; it does not sync the log. A caller that
/// goes on to flush these pages - `Engine::checkpoint` and
/// `ImportedDatabase::checkpoint` both do - syncs the log once more first, so
/// every record this function wrote is durable before `refuse_if_ahead_of_the_log`
/// is asked to let the page it stamped go to disk.
///
/// **A page byte-identical to what is already durable is left alone.**
/// `Database::free_map_pages`'s own comment already names the defect as a
/// *redundant* rewrite - one that changes nothing and still costs a record
/// and a physical write, on every single checkpoint, forever. Comparing
/// against the pool's own resident copy before touching either the log or
/// the file is not a cached flag that can go stale: it reads the same
/// current state `Pool::install` would otherwise overwrite, every time, so a
/// page this skips is a page nothing below here has any way to disagree
/// about. `recovering_checkpointing_and_recovering_again_is_the_same_database`
/// is the test that measures it - a checkpoint that changes nothing leaves
/// nothing new for the next reopen to scan.
///
/// @param database - the file whose free map is about to be checkpointed
/// @param wal - the log to record each page's rewrite in
pub fn log_free_map_pages(database: &mut Database, wal: &Wal) -> DbResult<()> {
    for (page_id, mut image) in database.free_map_pages() {
        let unchanged = database
            .pool()
            .fetch(page_id)
            .is_ok_and(|guard| guard.bytes() == image.as_slice());
        if unchanged {
            continue;
        }
        let lsn = wal.append(
            0,
            Body::WritePage {
                page: page_id.0,
                image: &image,
            },
        )?;
        page::write_u64(&mut image, header::LSN, lsn)?;
        database.install(page_id, &image)?;
    }
    Ok(())
}

impl Engine {
    /// Creates a fresh database with an empty log.
    ///
    /// @param vfs - the file system
    /// @param path - where to create it
    /// @param options - the page size, pool size, sync policy and timeout
    pub fn create(vfs: Arc<dyn Vfs>, path: &DbPath, options: EngineOptions) -> DbResult<Engine> {
        let database = Database::create(vfs.as_ref(), path, options.database)?;
        // A fresh file has no stamp to resume above, so the log starts where it
        // always did.
        Engine::assemble(
            vfs,
            path,
            database,
            options,
            Recovered::default(),
            (inillucent_wal::FIRST_LSN, 1),
        )
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
            // This engine opens one file at a time, so no commit it recovers
            // was ever waiting on a decision in another one.
            doubtful: Default::default(),
        };
        let (outcome, free_map) = {
            let mut applier = Applier::new(&mut database, rows);
            let outcome = recover::recover(vfs.as_ref(), path, start, &mut applier)?;
            (outcome, applier.free_map_changes().to_vec())
        };
        // "Rebuild the free map if any AllocPage or FreePage was replayed."
        // Done after the scan rather than inside it, because the free map and
        // every page write are both behind `&mut Database` and one record
        // cannot hold two mutable borrows of the same object.
        //
        // **In log order.** Two passes, claims then releases, made
        // a page that was freed and allocated again inside the replayed range
        // come back free while it was live. See `Applier::free_map_changes`.
        for change in &free_map {
            match change.allocated {
                true => database.claim(change.page)?,
                false => database.release(change.page, 1)?,
            }
        }
        recover::truncate_after(vfs.as_ref(), path, &outcome)?;
        // **The log resumes above every stamp the file carries.**
        // The one rule, in both open paths: a page's LSN has to be a position
        // in the stream beside the file, or the page-LSN rule discards every
        // later write to that page without saying so. See
        // `resume_above_every_stamp`.
        let resumed = resume_above_every_stamp(&mut database, &outcome)?;
        Engine::assemble(vfs, path, database, options, outcome, resumed)
    }

    /// Ties a database and a log together.
    ///
    /// @param vfs - the file system
    /// @param path - the database file
    /// @param database - the open file
    /// @param options - the sync policy and timeout
    /// @param recovered - what recovery found
    /// @param resumed - where the log resumes, and in which segment
    fn assemble(
        vfs: Arc<dyn Vfs>,
        path: &DbPath,
        database: Database,
        options: EngineOptions,
        recovered: Recovered,
        resumed: (u64, u64),
    ) -> DbResult<Engine> {
        let uuid = database.uuid();
        let (next_lsn, sequence) = resumed;
        let next_lsn = if next_lsn == 0 {
            inillucent_wal::FIRST_LSN
        } else {
            next_lsn
        };
        let sequence = sequence.max(1);
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
        database.pool().set_durable_lsn(wal.write_ahead_point());
        let uncommitted_lsn = database.pool().uncommitted_handle();
        let slot = Arc::new(WriterSlot::new());
        slot.set_busy_timeout_ms(options.busy_timeout_ms);
        Ok(Engine {
            database: RefCell::new(database),
            wal,
            clock: Clock::new(recovered.latest_cts),
            uncommitted: RefCell::new(BTreeMap::new()),
            uncommitted_lsn,
            versions: RefCell::new(VersionLog::new()),
            slot,
            gate: Mutex::new(()),
            // **Above every transaction number the log still holds.** Recovery
            // decides what to replay by transaction number, so reusing one that
            // is still in the log merges two different transactions: a loser
            // from a crashed run is resurrected the moment a new transaction of
            // the same number commits, and no later crash can undo it. The
            // model campaign found exactly that - a row from an uncommitted
            // transaction reappearing three crashes after it should have died.
            // Above every number the log still holds, as a belt on top of the
            // braces in `begin`: a log whose records were written by an older
            // build cannot collide with this run either.
            next_txn: Cell::new(recovered.highest_txn.saturating_add(1).max(1)),
            oldest_open_lsn: Cell::new(u64::MAX),
            collect_at: Cell::new(COLLECT_FLOOR),
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
        self.visible_to(tree, key, snapshot, None, read)
    }

    /// Says what a reader at `snapshot` should do with one key.
    ///
    /// **The open writer's uncommitted changes are hidden first, then the
    /// version log's committed ones.** The two cover different windows and both
    /// are needed: the version log says what a *committed* transaction
    /// overwrote since the snapshot, and [`Engine::uncommitted`] says what the
    /// transaction that is writing *right now* has overwritten and not yet
    /// committed. A reader that consulted only the second saw a dirty read, and
    /// one that consulted only the first saw a stale read.
    ///
    /// `reader` names the asking transaction so that the writer is not hidden
    /// from its own writes - a transaction reads what it has written, and its
    /// writes are exactly what this map is hiding from everybody else.
    ///
    /// @param tree - the tree the key is in
    /// @param key - the key's encoded bytes
    /// @param snapshot - the reader's snapshot
    /// @param reader - the asking transaction, when it is inside one
    /// @param read - what to do with the answer, while the log is borrowed
    pub fn visible_to<R>(
        &self,
        tree: u64,
        key: &[u8],
        snapshot: &Snapshot,
        reader: Option<TxnId>,
        read: impl FnOnce(Visible<'_>) -> R,
    ) -> R {
        if self.slot.holder() != reader {
            let uncommitted = self.uncommitted.borrow();
            if let Some(before) = uncommitted.get(&(tree, key.to_vec())) {
                return match before {
                    Some(bytes) => read(Visible::Instead(bytes)),
                    None => read(Visible::Absent),
                };
            }
        }
        let versions = self.versions.borrow();
        read(versions.visible(tree, key, snapshot.cts()))
    }

    /// Records that the open writer has changed a key, keeping its first image.
    ///
    /// First image wins: what a reader outside the transaction must see is what
    /// the row held before the *transaction* started, not before its most recent
    /// write to the same key.
    ///
    /// @param tree - the tree the key is in
    /// @param key - the key's encoded bytes
    /// @param before - what the row held, or `None` if it did not exist
    fn note_uncommitted(&self, tree: u64, key: Vec<u8>, before: Option<Vec<u8>>) {
        self.uncommitted
            .borrow_mut()
            .entry((tree, key))
            .or_insert(before);
    }

    /// Forgets the open writer's uncommitted changes.
    ///
    /// Called on both endings. On a commit the images have just been published
    /// to the version log, where a reader older than the commit finds them; on a
    /// rollback they describe changes that no longer exist. Either way what is
    /// in the trees is now what a reader should see, and leaving an entry here
    /// would hide a row that is committed.
    fn forget_uncommitted(&self) {
        self.uncommitted.borrow_mut().clear();
        // Nothing is uncommitted any more, so every page the pool has been
        // holding back is free to go to the file at the next checkpoint.
        self.uncommitted_lsn.store(u64::MAX, Ordering::SeqCst);
    }

    /// Returns how many before-images the version log holds.
    pub fn versions_held(&self) -> usize {
        self.versions.borrow().len()
    }

    /// Begins a transaction.
    ///
    /// @param how - deferred or immediate
    pub fn begin(&self, how: Begin) -> DbResult<Transaction<'_>> {
        // **The number comes from the log's own position, not from a counter
        // that restarts.** Recovery decides what to replay by transaction
        // number, so a number that appears twice in one log turns two different
        // transactions into one - and the log outlives the run. Seeding a
        // counter at open is not enough, because a later checkpoint can move the
        // recovery point *backwards* (a page held out of the file by no-steal
        // keeps it there), so the next recovery may scan records this open never
        // saw.
        //
        // Taking the next LSN makes it airtight rather than probable: a
        // transaction that writes anything puts its records at or above its own
        // number, so the log's end afterwards is above it - and every later run
        // starts above that end. A transaction that writes nothing never appears
        // in the log at all, so its number cannot collide with anything.
        //
        // The `max` with the counter keeps two transactions that begin with no
        // record between them apart, which matters to the writer slot and to the
        // statistics even though the log never sees the second one.
        let id = TxnId(self.wal.next_lsn().max(self.next_txn.get()));
        self.next_txn.set(id.0.saturating_add(1));
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
        let durable = self.wal.write_ahead_point();
        self.with_pool(|pool| pool.set_durable_lsn(durable));
        let watermark = self.clock.latest();
        let sequence = self.wal.sequence();
        self.with_database(|database| -> DbResult<()> {
            // The pages first, then the meta record that says where recovery
            // starts - because the answer depends on which pages went out. A
            // page held back by no-steal keeps recovery below its first
            // unwritten change, and that change may belong to a transaction
            // that committed long ago: `oldest_open_lsn` alone would step over
            // it, and the model campaign reported the row it lost.
            database.pool().flush()?;
            // Measured here, before the free map's own pages are touched
            // below: that page is about to be dirtied and is flushed inside
            // `checkpoint_after_free_map` regardless, never held back by
            // no-steal, so it must not pull `recovery_from` down as though it
            // could be.
            let dirty = database.pool().oldest_dirty_lsn();
            // The free map's own pages, logged and stamped before they are
            // rewritten - see `log_free_map_pages` - and done *before*
            // `set_log_position` below reads the durable point. Read
            // earlier, `durable` would sit under this checkpoint's own new
            // free-map record, so the meta record would claim recovery need
            // not start below a point that is, in fact, below an unreplayed
            // record - the next reopen would scan it again every time, which
            // is what `recovering_checkpointing_and_recovering_again_is_the_same_database`
            // measures.
            log_free_map_pages(database, &self.wal)?;
            self.wal.sync()?;
            let durable = self.wal.write_ahead_point();
            database.pool().set_durable_lsn(durable);
            let recovery_from = durable.min(self.oldest_open_lsn.get()).min(dirty);
            database.set_log_position(recovery_from, watermark, sequence);
            database.checkpoint_after_free_map()
        })?;
        // Refreshed rather than reused: `log_free_map_pages` appended records
        // of its own, so the log's durable end has moved past the `durable`
        // read at the top of this function.
        let durable = self.wal.write_ahead_point();
        let recovery_from = durable
            .min(self.oldest_open_lsn.get())
            .min(self.with_pool(|pool| pool.oldest_dirty_lsn()));
        self.wal.note_checkpoint(recovery_from, watermark)?;
        let durable = self.wal.write_ahead_point();
        self.with_pool(|pool| pool.set_durable_lsn(durable));
        self.wal.retire_segments_below(recovery_from)?;
        let mut stats = self.stats.get();
        stats.checkpoints = stats.checkpoints.saturating_add(1);
        self.stats.set(stats);
        Ok(recovery_from)
    }

    /// Collects the version log when it has grown past its threshold.
    ///
    /// Called on every commit. It does nothing until the log passes the
    /// threshold, and the threshold is then set to twice what survived - so the
    /// work is amortised against the images that are actually accumulating,
    /// and a reader holding a large but stable set is not re-swept on every
    /// commit for nothing.
    fn collect_if_grown(&self) {
        let held = self.versions.borrow().len();
        if held < self.collect_at.get() {
            return;
        }
        self.collect_versions();
        let remaining = self.versions.borrow().len();
        self.collect_at
            .set(remaining.saturating_mul(2).max(COLLECT_FLOOR));
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
        // No-steal: from here until this transaction ends, the pool must not
        // write a page stamped at or above this record. The pool enforces it;
        // this is the only place that knows the number.
        self.uncommitted_lsn.store(lsn, Ordering::SeqCst);
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
    ///
    /// **Only the transaction that logged something calls this**, which the
    /// caller enforces. A reader ending is not a writer ending, and when this
    /// was called unconditionally a read-only `BEGIN ... COMMIT` between a write
    /// and a checkpoint let the checkpoint advance the recovery point past the
    /// open writer's records - whose pages were being held out of the file by
    /// no-steal, so a crash lost them with nothing left to replay. The model
    /// campaign reported it as a committed row "is missing".
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
        self.engine
            .note_uncommitted(tree, key.clone(), before.clone());
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
        let durable = self.engine.wal.write_ahead_point();
        self.engine.with_pool(|pool| pool.set_durable_lsn(durable));
        self.finish(true);
        // **After `finish`, so this transaction's own snapshot is already
        // closed.** Collecting while it was still registered would keep every
        // image the transaction had just published, which is the one shape of
        // sweep that costs the walk and frees nothing.
        self.engine.collect_if_grown();
        Ok(cts)
    }

    /// Releases the writer slot and updates the counters.
    ///
    /// @param committed - whether the transaction committed
    fn finish(&mut self, committed: bool) {
        self.finished = true;
        // **Only a transaction that wrote clears the uncommitted state**, and
        // the difference is a correctness one rather than a saving. There is one
        // writer slot but any number of readers, and a reader ending is an
        // ordinary event: when a read-only `BEGIN ... COMMIT` cleared this, it
        // took the open *writer's* before-images out of the version map and its
        // first record out of the pool's no-steal watermark - so the next
        // checkpoint wrote that writer's uncommitted pages to the file, and a
        // crash could not take them back out. The model campaign found it as
        // "(1, 6) is there and should not be", on a trace where a transaction
        // that wrote nothing committed between a write and a checkpoint.
        if self.has_written() {
            self.engine.forget_uncommitted();
            self.engine.note_finished();
        }
        self.writer = None;
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
