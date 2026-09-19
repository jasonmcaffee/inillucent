//! The log writer: appending records, rolling segments, and group commit.
//!
//! Invariant: **nothing is durable before its log record is.** Every caller
//! that is about to make a change durable somewhere else asks this module for
//! [`Wal::durable_end`] first, and a page whose LSN is above that number may
//! not be written to the data file. The rule is stated here because this is the
//! only place that can know the answer, and it is enforced in the buffer pool,
//! which is told the number rather than being made to depend on the log.
//!
//! ## LSNs are positions in a byte stream
//!
//! The log is logically one stream of records; segments are how it is stored,
//! not what it is. A record's LSN is its byte offset in the concatenation of
//! every segment's *record area* - headers excluded - which makes three things
//! fall out rather than be maintained:
//!
//! - "is this durable" is one `u64` comparison against [`Wal::durable_end`];
//! - the file offset of a record is `header + (lsn - segment.first_lsn)`, so
//!   recovery seeks rather than scans to find a checkpoint's LSN;
//! - LSNs increase across a roll with no special case, because the next
//!   segment's `first_lsn` is where the previous one stopped.
//!
//! The stream starts at [`FIRST_LSN`] rather than at zero, so that zero means
//! "no LSN" everywhere - a freshly built page carries LSN 0 and every record
//! therefore applies to it.
//!
//! ## Group commit without a log-writer thread
//!
//! The TDD describes a log writer thread that drains the buffer while
//! committers wait on a gate. This implements the same property - **concurrent
//! committers share one write and one sync** - with a leader-follower drain
//! instead: the first committer to find the buffer undrained becomes the leader,
//! takes the whole buffer including records other threads appended after it, and
//! does the one write and the one sync for all of them; every other committer
//! waits on the gate until its own end position is durable.
//!
//! It is the same number of system calls per commit group and one fewer moving
//! part, and the reason to prefer it here is specific: a background thread makes
//! *when* a sync happens depend on scheduling, and this phase's acceptance
//! includes a crash-at-every-sync campaign that has to enumerate an execution
//! rather than sample one. On a single connection - which is what the scorecard
//! measures - the committer is always the leader, so the path is one write and
//! one sync per commit under `synchronous=FULL`, exactly as SQLite's WAL mode is.
//!
//! ## What happens after a failed write
//!
//! A write or sync that fails **poisons** the log: the error is kept and every
//! later call returns it. A log that reported one commit durable, failed, and
//! then carried on appending would be a log whose durable prefix is not a
//! prefix, and there is no honest way to continue from that. The owning
//! transaction manager turns a poisoned log into a refusal to start new work.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};

use inillucent_base::error::{misuse, DbError};
use inillucent_base::DbResult;
use inillucent_vfs::{DbPath, FileKind, OpenOptions, SyncMode, Vfs, VfsFile};

use crate::record::{Body, Record};
use crate::segment::{self, SegmentHeader};

/// The LSN the first record of a fresh log is written at.
///
/// Eight rather than zero so that LSN zero can mean "no LSN" without a
/// sentinel, and eight rather than one because every record starts on an
/// eight-byte boundary.
pub const FIRST_LSN: u64 = 8;

/// How many bytes `synchronous=NORMAL` lets accumulate before it syncs.
///
/// The TDD's 64 MiB. Under `NORMAL` a commit is acknowledged when its record
/// has been *written*, and the guarantee is that the database is not corrupted
/// by a power loss - not that the last commits survive one.
pub const NORMAL_SYNC_BYTES: u64 = 64 << 20;

/// How much unwritten log may sit in memory before it is handed to the file.
///
/// **A buffer that is only ever drained by a commit is a buffer the size of the
/// transaction.** The consequence was measured: one round of
/// `write.update.indexed` writes 7,927 KiB of log, and because every byte of it
/// was held until the commit, that round raised the process's high-water mark
/// by 7.04 MiB - and the buffer is recycled rather than freed, so the capacity
/// stayed for the rest of the process. `write.insert.batch` and `write.upsert`
/// add another 7.74 MiB between them by the same route. That is a quarter of
/// this engine's whole residency, spent holding bytes whose only destination is
/// a file.
///
/// Handing them over early costs nothing that matters and is already a case the
/// design has a name for. Under `FULL` and `NORMAL` a page may not reach the
/// data file above [`Wal::durable_end`], and a plain write does not move it -
/// only a sync does - so writing early cannot let an uncommitted change out.
/// Recovery decides what to replay in a first pass over the `Commit` records,
/// and [`Body::Abort`] exists precisely so that a transaction *whose records
/// were flushed* can roll back; `Transaction::rollback` already appends one
/// when `written_end` has passed the transaction's first LSN.
///
/// 512 KiB rather than a smaller bound: the drain is one `write_all_at` of
/// whatever has accumulated, so a bound of one page would be an ordinary write
/// path made of syscalls. At 512 KiB the 7,927 KiB round pays fifteen extra
/// writes and no extra syncs, and holds a sixteenth of what it held before.
pub const SPILL_BYTES: usize = 512 << 10;

/// The device sector this log pads every durable write out to.
///
/// **A write that starts in the middle of a sector shares that sector with
/// whatever else ends inside it**, and a device with no `powersafe_overwrite`
/// guarantee - `inillucent_sim`'s `MediaModel::default`, the pessimistic model
/// this crate's own durability campaigns run against - resolves an unsynced
/// sector as a whole: applied, dropped, torn or garbled. So the moment a new
/// write touches any byte of a sector, the *rest* of that sector - including
/// bytes an earlier, independently synced record ended with - goes back into
/// the unsynced pool, and a crash or a failed sync from the *new* write can
/// destroy the *old* record's tail even though nothing about the old record
/// itself failed.
///
/// This was not theoretical: `durability::a_full_disk_at_every_cut_point_is_recoverable`
/// reproduced it directly. A workload's commit, whose own sync then failed on
/// an injected disk-full error, wrote its first bytes at file offset 9440 -
/// inside sector 18 (9216..9728), the same sector the *previous session's*
/// `INSERT` had already written its tail into and durably synced. The crash
/// model resolved sector 18 as `Garbage`, and the reopened database came back
/// with a checksum failure on that `INSERT`'s own record - an acknowledged,
/// already-closed commit lost by a later, unrelated transaction it happened
/// to share a sector with.
///
/// Padding every synced write's tail out to this boundary means the *next*
/// write always starts on a fresh sector, so it can never again share one
/// with a record that was already durable before it began. 512 is what
/// `MediaModel::default` declares and is also the traditional physical sector
/// size, so it is the granularity this crate's own tests can prove; a real
/// 4Kn device gains nothing extra from it, but loses nothing either; it is
/// still every current write's own tail that gets protected, never a
/// stranger's.
const SECTOR_ALIGN: u64 = 512;

/// How much log a checkpoint is triggered by, in bytes.
pub const CHECKPOINT_BYTES: u64 = 256 << 20;

/// How long a checkpoint is triggered by, in microseconds.
pub const CHECKPOINT_MICROS: u64 = 30_000_000;

/// When the log is flushed to the media.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Synchronous {
    /// Never sync. A power loss may lose commits and may corrupt the database.
    Off,
    /// Sync when [`NORMAL_SYNC_BYTES`] have accumulated and before a
    /// checkpoint. A power loss may lose recent commits and may not corrupt the
    /// database, because a checkpoint never advances past the durable LSN.
    Normal,
    /// Sync on every commit. A commit that returned is durable.
    Full,
}

impl Synchronous {
    /// Parses the pragma's spelling of a policy.
    ///
    /// @param text - `off`, `normal` or `full`, in any case
    pub fn parse(text: &str) -> DbResult<Synchronous> {
        match text.trim().to_ascii_lowercase().as_str() {
            "off" | "0" => Ok(Synchronous::Off),
            "normal" | "1" => Ok(Synchronous::Normal),
            "full" | "2" | "extra" | "3" => Ok(Synchronous::Full),
            other => Err(misuse(format!("{other} is not a synchronous policy"))),
        }
    }

    /// Returns the policy's name, as a pragma reports it.
    pub fn name(self) -> &'static str {
        match self {
            Synchronous::Off => "off",
            Synchronous::Normal => "normal",
            Synchronous::Full => "full",
        }
    }
}

/// What the log has done, for tests and for the gate's fairness section.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct WalStats {
    /// Records appended.
    pub records: u64,
    /// Calls to the file's `write_all_at`.
    pub writes: u64,
    /// Calls to the file's `sync`.
    pub syncs: u64,
    /// Commits that waited for a leader rather than becoming one.
    pub followers: u64,
    /// Segments opened since this handle was created.
    pub segments: u64,
    /// Bytes appended.
    pub bytes: u64,
}

/// The open segment.
struct OpenSegment {
    file: Box<dyn VfsFile>,
    path: DbPath,
    sequence: u64,
    first_lsn: u64,
}

/// The log's mutable bookkeeping.
struct Inner {
    /// Records appended and not yet handed to the file.
    buffer: Vec<u8>,
    /// The stream position the buffer's first byte occupies.
    buffer_start: u64,
    /// The stream position the next record will be written at.
    next_lsn: u64,
    /// Everything below this has been handed to `write_all_at`.
    written_end: u64,
    /// Everything below this is durable.
    durable_end: u64,
    /// Bytes written and not yet synced.
    unsynced: u64,
    /// Bytes appended since the last checkpoint record.
    since_checkpoint: u64,
    /// True while one thread is doing the write and the sync for everybody.
    draining: bool,
    /// The first failure, which every later call repeats.
    poisoned: Option<DbError>,
    /// Stats.
    stats: WalStats,
}

/// Everything a handle shares with the other handles on the same log.
struct Shared {
    inner: Mutex<Inner>,
    /// Woken when `durable_end` or `draining` changes.
    gate: Condvar,
    /// The open segment, behind its own lock so a leader can do its I/O
    /// without holding the bookkeeping lock that followers are appending under.
    io: Mutex<OpenSegment>,
    vfs: Arc<dyn Vfs>,
    base: String,
    directory: Option<std::path::PathBuf>,
    uuid: u128,
    segment_bytes: u64,
    /// The policy, atomic so a pragma can change it without taking the lock a
    /// commit is waiting on.
    synchronous: AtomicU64,
    /// The lowest sequence number [`Wal::retire_segments_below`] still has to
    /// look at.
    ///
    /// **Without it that function is quadratic, and it was measured (task-1999).**
    /// It walks `1..current` and opens a file per sequence to read its header,
    /// so every call re-asks about every segment every earlier call already
    /// deleted. Under `locking_mode = normal` a segment is rolled per statement
    /// and a checkpoint is taken per statement, which makes `current` roughly
    /// the statement count and the walk O(statements) *per statement*. Two
    /// thousand autocommit inserts opened **1.5 million segment headers, 1.49
    /// million of them for a file that is not there**, and the call grew from
    /// 4.9 ms at statement 100 to 14.5 ms at statement 2,000 with no bound; a
    /// database reopened with 2,030 segments behind it paid 19.5 ms a
    /// statement, 52% of the whole checkpoint, to delete nothing, because the
    /// sequence number is read back from the meta record and the cost therefore
    /// survives a close.
    ///
    /// It is only ever raised past a sequence this process has watched become
    /// absent - deleted here, or already gone when this handle first looked -
    /// and only while that run is unbroken from the floor, so a segment that is
    /// still there, including one whose deletion failed, keeps the floor
    /// beneath it and is looked at again next time. A fresh handle starts at 1
    /// and so pays the old walk exactly once.
    retired_below: AtomicU64,
}

/// A write-ahead log over a chain of segment files.
pub struct Wal {
    shared: Arc<Shared>,
}

/// How to open a log.
#[derive(Clone, Debug)]
pub struct WalOptions {
    /// The sync policy.
    pub synchronous: Synchronous,
    /// How large a segment grows before the log rolls.
    pub segment_bytes: u64,
}

impl Default for WalOptions {
    fn default() -> WalOptions {
        WalOptions {
            synchronous: Synchronous::Full,
            segment_bytes: segment::SEGMENT_BYTES,
        }
    }
}

impl Wal {
    /// Opens the log for a database, creating its first segment.
    ///
    /// The caller has already recovered, so `first_lsn` is where the recovered
    /// log stopped and `sequence` is the segment to write next. A fresh
    /// database passes [`FIRST_LSN`] and 1.
    ///
    /// @param vfs - the file system the segments live in
    /// @param base - the database file's path, which the segments are named after
    /// @param uuid - the database's identity, stamped into every segment
    /// @param first_lsn - the stream position the next record takes
    /// @param sequence - the sequence number of the segment to open
    /// @param options - the sync policy and segment size
    pub fn open(
        vfs: Arc<dyn Vfs>,
        base: &DbPath,
        uuid: u128,
        first_lsn: u64,
        sequence: u64,
        options: WalOptions,
    ) -> DbResult<Wal> {
        let name = base.as_path().to_string_lossy().to_string();
        let directory = base.as_path().parent().map(std::path::Path::to_path_buf);
        let segment = open_segment(
            vfs.as_ref(),
            &name,
            directory.as_deref(),
            uuid,
            sequence,
            first_lsn,
        )?;
        let shared = Shared {
            inner: Mutex::new(Inner {
                buffer: Vec::with_capacity(64 << 10),
                buffer_start: first_lsn,
                next_lsn: first_lsn,
                written_end: first_lsn,
                durable_end: first_lsn,
                unsynced: 0,
                since_checkpoint: 0,
                draining: false,
                poisoned: None,
                stats: WalStats {
                    segments: 1,
                    ..WalStats::default()
                },
            }),
            gate: Condvar::new(),
            io: Mutex::new(segment),
            vfs,
            base: name,
            directory,
            uuid,
            segment_bytes: options.segment_bytes.max(segment::HEADER_BYTES as u64 * 2),
            synchronous: AtomicU64::new(policy_code(options.synchronous)),
            // A new handle knows nothing about which of the segments below it
            // are still on disk, so it starts at the bottom and learns.
            retired_below: AtomicU64::new(1),
        };
        // **No padding call here.** Every commit pads its own tail to the next
        // sector boundary (see `commit`'s own call), so by induction a resumed
        // position is always already on one: the segment this session opens
        // into either holds nothing yet - nothing durable to share a sector
        // with, so there is nothing to protect - or its last record is an
        // earlier session's own commit, which already closed the boundary
        // behind it. Padding here too was tried and reverted: it inserted an
        // unconditional filler record at the front of every freshly created
        // log, which cost every fresh database a wasted sector and broke
        // `inillucent-wal/tests/log.rs`'s own exact-LSN assertions for no
        // durability this crate does not already have.
        let wal = Wal {
            shared: Arc::new(shared),
        };
        Ok(wal)
    }

    /// Returns a second handle on the same log, for another thread.
    pub fn handle(&self) -> Wal {
        Wal {
            shared: Arc::clone(&self.shared),
        }
    }

    /// Returns the sync policy.
    pub fn synchronous(&self) -> Synchronous {
        policy_of(self.shared.synchronous.load(Ordering::Relaxed))
    }

    /// Sets the sync policy.
    ///
    /// @param policy - the new policy
    pub fn set_synchronous(&self, policy: Synchronous) {
        self.shared
            .synchronous
            .store(policy_code(policy), Ordering::Relaxed);
    }

    /// Returns the position past the last durable byte of the log.
    ///
    /// This is the number the write-ahead rule is stated against: a page whose
    /// LSN is at or above it has not been described by a durable record and may
    /// not be written to the data file.
    pub fn durable_end(&self) -> u64 {
        self.with_inner(|inner| inner.durable_end)
    }

    /// Returns the position past the last byte handed to the file.
    pub fn written_end(&self) -> u64 {
        self.with_inner(|inner| inner.written_end)
    }

    /// Returns the LSN the next record will take.
    pub fn next_lsn(&self) -> u64 {
        self.with_inner(|inner| inner.next_lsn)
    }

    /// Returns how many bytes have been appended since the last checkpoint.
    pub fn since_checkpoint(&self) -> u64 {
        self.with_inner(|inner| inner.since_checkpoint)
    }

    /// Returns the counters.
    pub fn stats(&self) -> WalStats {
        self.with_inner(|inner| inner.stats)
    }

    /// Returns the database identity every segment of this log is stamped
    /// with.
    ///
    /// Exists for a caller that wants to scan its own log with
    /// `inillucent_wal::recover` - `RecoveryStart` needs this to reject a
    /// segment belonging to a different database, and nothing before this
    /// exposed it outside the crate.
    pub fn uuid(&self) -> u128 {
        self.shared.uuid
    }

    /// Returns the sequence number of the segment being written.
    pub fn sequence(&self) -> u64 {
        match self.shared.io.lock() {
            Ok(segment) => segment.sequence,
            Err(poisoned) => poisoned.into_inner().sequence,
        }
    }

    /// Returns the segment that holds a given LSN, or refuses if none present
    /// does.
    ///
    /// **A checkpoint's own recovery point is not always in the segment the
    /// checkpoint just rolled to, and pairing it with that segment anyway is a
    /// real bug, not a simplification.** `roll_segment` moves the active
    /// segment forward before a checkpoint computes its recovery point, so the
    /// two used to be assumed to march together; that held as long as recovery
    /// could only ever start at the log's own durable end. Once
    /// `ImportedDatabase::checkpoint` began bounding the recovery point by a
    /// held-back page's earlier, already-checkpointed `rec_lsn` too - the fix
    /// for the checkpoint-during-an-open-transaction defect - the recovery
    /// point can land in an *earlier* segment than the one just rolled to, and
    /// `RecoveryStart::sequence` has to name that one: `read_chain` starts
    /// reading at the segment number it is given and never looks earlier, so
    /// pairing an old LSN with the new segment made recovery start scanning
    /// bytes that do not contain it, silently skipping every record between
    /// the true segment and the new one - among them the very commit the
    /// bound exists to keep. Reproduced by
    /// `a_checkpoint_during_a_later_open_transaction_keeps_the_earlier_commit`
    /// in `inillucent-compat`'s `durability.rs`.
    ///
    /// Walks backward from the segment being written, because that is the one
    /// call already answers with no I/O, and stops at the first segment whose
    /// own `first_lsn` is at or below `lsn` - segments are contiguous and
    /// numbered in order, so that segment is the one that holds it.
    ///
    /// **Refuses rather than guesses the moment a segment cannot be read -
    /// continuing past it is not a fix, because `read_chain` cannot skip a
    /// gap either way.** An earlier version of this function kept walking
    /// lower after an unreadable segment, on the theory that an *earlier*
    /// surviving segment might still legitimately hold `lsn`. The task-1911
    /// review named the flaw directly: even when such an
    /// earlier segment exists and its own `first_lsn` is at or below `lsn`,
    /// `read_chain` reads segments in strict, unbroken sequence from where it
    /// starts - the gap left by the unreadable one still ends the chain
    /// before it ever reaches the records above it, silently discarding them
    /// the same way pairing `recovery_from` with the wrong segment did
    /// originally. There is no number this function could return in that
    /// situation that `read_chain` could actually use, so it has to say so
    /// rather than hand back one anyway.
    ///
    /// This is also why the walk needs no explicit upper-bound check on a
    /// candidate: every rejection of the segment *above* it already proved
    /// `lsn` is below that segment's own `first_lsn`, so a candidate accepted
    /// here is bounded on both sides by segments confirmed present and
    /// confirmed contiguous with it.
    ///
    /// **The segment being appended to is answered without a file open**,
    /// which is the answer almost every call gets: the walk below reads a
    /// header off disk even for the segment this handle already has open, and
    /// under `locking_mode = normal` a checkpoint asks this once a statement.
    /// It was 3.3 ms of a 19 ms checkpoint (task-1999). The open segment's
    /// `first_lsn` is held in memory and is exact, so an `lsn` at or above it
    /// is in that segment and in no other.
    ///
    /// @param lsn - the stream position to locate
    pub fn sequence_containing(&self, lsn: u64) -> DbResult<u64> {
        let mut candidate = self.sequence();
        if lsn >= self.segment_first_lsn() {
            return Ok(candidate);
        }
        loop {
            match self.first_lsn_of(candidate) {
                Some(first) if first <= lsn => return Ok(candidate),
                Some(first) if candidate <= 1 => {
                    return Err(misuse(format!(
                        "no present segment holds lsn {lsn}: segment 1 starts at {first}, \
                         still above it"
                    )));
                }
                Some(_) => candidate -= 1,
                None => {
                    return Err(misuse(format!(
                        "no present segment holds lsn {lsn}: segment {candidate} could not be \
                         read, and the gap it leaves cannot be skipped over"
                    )));
                }
            }
        }
    }

    /// Reports whether a failed write has stopped the log.
    pub fn is_poisoned(&self) -> bool {
        self.with_inner(|inner| inner.poisoned.is_some())
    }

    /// Appends one record and returns its LSN.
    ///
    /// The record is in the buffer when this returns, not on the media. The
    /// caller makes it durable with [`Wal::commit`] or [`Wal::sync`].
    ///
    /// @param txn - the transaction the record belongs to, or zero
    /// @param body - what happened
    pub fn append(&self, txn: u64, body: Body<'_>) -> DbResult<u64> {
        self.roll_if_full()?;
        let mut inner = self.lock()?;
        Wal::check_poison(&inner)?;
        let lsn = inner.next_lsn;
        let record = Record {
            lsn,
            txn,
            body,
            length: 0,
        };
        let mut buffer = std::mem::take(&mut inner.buffer);
        let outcome = record.encode(&mut buffer);
        inner.buffer = buffer;
        let length = outcome? as u64;
        inner.next_lsn = lsn.saturating_add(length);
        inner.stats.records = inner.stats.records.saturating_add(1);
        inner.stats.bytes = inner.stats.bytes.saturating_add(length);
        inner.since_checkpoint = inner.since_checkpoint.saturating_add(length);
        // **The bound, taken after the record is in and with the lock let go.**
        // A transaction bigger than [`SPILL_BYTES`] hands its earlier records to
        // the file rather than holding them to the commit; the commit still does
        // the one sync, so nothing about durability moves. See `SPILL_BYTES` for
        // why writing an uncommitted record early is safe here and not merely
        // convenient.
        let over = inner.buffer.len() >= SPILL_BYTES;
        drop(inner);
        if over {
            self.flush()?;
        }
        Ok(lsn)
    }

    /// Appends a `Commit` record and makes it durable under the policy.
    ///
    /// Returns the commit record's LSN. When this returns, the commit is
    /// durable if and only if the policy says a commit is durable - which is
    /// the whole of what `synchronous` means, and is why the policy is read
    /// here rather than by the caller.
    ///
    /// @param txn - the committing transaction
    /// @param cts - the commit timestamp it was assigned
    pub fn commit(&self, txn: u64, cts: u64) -> DbResult<u64> {
        let lsn = self.append(txn, Body::Commit { cts })?;
        // Rides along in this same commit's own write and sync - see
        // `SECTOR_ALIGN`'s own comment for why it must be this write and not
        // a later one.
        self.pad_to_sector_boundary()?;
        let end = self.with_inner(|inner| inner.next_lsn);
        self.await_commit(end)?;
        Ok(lsn)
    }

    /// Appends a `Pad` record reaching the next sector boundary, if the log
    /// is not already sitting on one.
    ///
    /// Appended only - the caller's own sync (`commit`'s or `sync`'s) is what
    /// makes it durable, together with the record it rides along with. See
    /// `SECTOR_ALIGN`'s own comment for why it must ride along with a write
    /// that is already going to succeed rather than be synced on its own
    /// afterward: a pad synced separately would touch the shared sector all
    /// over again with nothing riding along to protect.
    ///
    /// A no-op once the log is already sitting on a boundary - the ordinary
    /// case, since every commit closes its own.
    fn pad_to_sector_boundary(&self) -> DbResult<()> {
        let segment_first_lsn = self.io_lock()?.first_lsn;
        let next_lsn = self.with_inner(|inner| inner.next_lsn);
        let offset = (segment::HEADER_BYTES as u64)
            .saturating_add(next_lsn.saturating_sub(segment_first_lsn));
        let remainder = offset % SECTOR_ALIGN;
        if remainder == 0 {
            return Ok(());
        }
        let mut gap = SECTOR_ALIGN.saturating_sub(remainder);
        // A gap narrower than a record's own header cannot hold a record at
        // all; the next sector is padded to instead, which still leaves this
        // one durable on its own once this record's write lands.
        if gap < crate::record::HEADER_BYTES as u64 {
            gap = gap.saturating_add(SECTOR_ALIGN);
        }
        let len = gap.saturating_sub(crate::record::HEADER_BYTES as u64);
        self.append(0, Body::Pad { len: len as u32 })?;
        Ok(())
    }

    /// Makes everything below `end` durable under the policy.
    ///
    /// The half of [`Wal::commit`] that does not append, for a caller that has
    /// already written its own `Commit` record and only needs the wait. A
    /// transaction manager is such a caller: it appends the record **under the
    /// commit gate**, so that commit order equals visibility order equals log
    /// order, and then waits out here where the next committer can append past
    /// it - which is what group commit is.
    ///
    /// The first version of the transaction manager called [`Wal::commit`] for
    /// the wait and got a second `Commit` record for the same transaction. It
    /// was harmless to recovery, which takes the first one, and it was still
    /// wrong: the log said something that did not happen. What found it was a
    /// reopen test asserting the number of records replayed - eighteen where six
    /// transactions had been committed.
    ///
    /// @param end - the stream position the caller needs to reach
    pub fn await_commit(&self, end: u64) -> DbResult<()> {
        match self.synchronous() {
            Synchronous::Full => self.drive(end, true),
            Synchronous::Normal => {
                let due = self.with_inner(|inner| {
                    inner
                        .unsynced
                        .saturating_add(inner.next_lsn.saturating_sub(inner.written_end))
                }) >= NORMAL_SYNC_BYTES;
                self.drive(end, due)
            }
            Synchronous::Off => self.drive(end, false),
        }
    }

    /// Returns the position a page may be written up to under the write-ahead
    /// rule.
    ///
    /// **This is `durable_end` under `NORMAL` and `FULL`, and `written_end`
    /// under `OFF`** - and the difference is what `OFF` *means*. The rule is
    /// that a page must not reach the file before the record describing it; what
    /// "reach" means is exactly what the sync policy sets. Under `FULL` and
    /// `NORMAL` the record has to be on the media, so a page waits for a sync.
    /// Under `OFF` nothing is ever synced, so a watermark taken from
    /// `durable_end` never moves - and a checkpoint could never write a page at
    /// all. That is not "less safe", it is "does not work": the model campaign's
    /// `OFF` arms failed with *the checkpoint failed*, on every seed.
    ///
    /// Under `OFF` the log and the data file are both in the operating system's
    /// hands and a power failure may lose either, which is the bargain `OFF`
    /// offers. What is still guaranteed is the *order*: the record is handed to
    /// the file system before the page is.
    pub fn write_ahead_point(&self) -> u64 {
        match self.synchronous() {
            Synchronous::Off => self.written_end(),
            Synchronous::Normal | Synchronous::Full => self.durable_end(),
        }
    }

    /// Writes everything buffered to the segment without syncing.
    pub fn flush(&self) -> DbResult<()> {
        let end = self.with_inner(|inner| inner.next_lsn);
        self.drive(end, false)
    }

    /// Writes everything buffered and syncs it, whatever the policy says.
    ///
    /// The policy decides when a *commit* syncs. A checkpoint syncs regardless,
    /// under `NORMAL` and `FULL`, because a checkpoint that advanced past
    /// unsynced log would be a data file describing changes the log cannot
    /// replay. Under `OFF` nothing syncs, which is what `OFF` means and is why
    /// a checkpoint under it is not a durability boundary.
    pub fn sync(&self) -> DbResult<()> {
        // **No padding call here.** Only `commit` pads its own tail - see
        // `SECTOR_ALIGN`'s comment for why it has to ride along with a write
        // that is already going to succeed, which is true of an acknowledged
        // commit and not generally true of a bare `sync`: `note_checkpoint`
        // calls this right after its own `Checkpoint` record, and callers
        // that assert an exact record count over a sequence ending in one -
        // `inillucent-wal/tests/recovery.rs`'s
        // `records_belonging_to_no_transaction_are_replayed` is one - would
        // see an extra filler record they never asked for and have no reason
        // to expect. A checkpoint's own records already end wherever the
        // schema tree's or the free map's last write happened to land; the
        // next *commit*, whenever it comes, closes that boundary as its own.
        let end = self.with_inner(|inner| inner.next_lsn);
        self.drive(end, self.synchronous() != Synchronous::Off)
    }

    /// Records that a checkpoint completed and resets the trigger counters.
    ///
    /// @param checkpoint_lsn - every page write at or below this is in the file
    /// @param cts_watermark - the commit timestamp watermark at the checkpoint
    pub fn note_checkpoint(&self, checkpoint_lsn: u64, cts_watermark: u64) -> DbResult<u64> {
        let lsn = self.append(
            0,
            Body::Checkpoint {
                checkpoint_lsn,
                cts_watermark,
            },
        )?;
        self.sync()?;
        self.with_inner_mut(|inner| inner.since_checkpoint = 0);
        Ok(lsn)
    }

    /// Reports whether the log has grown enough to want a checkpoint.
    ///
    /// @param elapsed_micros - how long since the last checkpoint
    pub fn checkpoint_due(&self, elapsed_micros: u64) -> bool {
        self.since_checkpoint() >= CHECKPOINT_BYTES || elapsed_micros >= CHECKPOINT_MICROS
    }

    /// Deletes every segment whose records are entirely below `lsn`.
    ///
    /// Called after a checkpoint, when the data file already holds every change
    /// those segments describe. A segment that cannot be deleted is left alone
    /// and reported as `Ok`: a leftover segment is refused on the next open by
    /// its sequence number, so it is untidy rather than dangerous, and failing a
    /// checkpoint because a file could not be unlinked would turn a tidy-up into
    /// an outage.
    ///
    /// **It starts at the lowest sequence that might still be there, not at
    /// 1.** See [`Shared::retired_below`] for what walking from 1 every time
    /// cost and how the floor is allowed to move. The set of files this deletes
    /// is unchanged: everything below the floor is a file this handle has
    /// already watched become absent, and nothing creates a segment below the
    /// one being appended to.
    ///
    /// @param lsn - the checkpoint LSN
    pub fn retire_segments_below(&self, lsn: u64) -> DbResult<usize> {
        let current = self.sequence();
        let mut removed = 0usize;
        // Raised past each sequence that is absent when this call is done with
        // it, and only while that run is unbroken from where the walk began: a
        // segment still on disk - kept deliberately, or one whose deletion the
        // file system refused - has to be looked at again next time.
        let floor = self.shared.retired_below.load(Ordering::Relaxed).max(1);
        let mut next_floor = floor;
        let mut unbroken = true;
        for sequence in floor..current {
            let path = self.segment_path(sequence);
            // No `access` check first: the read-only open below fails for a
            // segment that is not there, so asking twice was one extra call and
            // one extra error arm that no file system this engine runs on can
            // take - which is a branch the coverage gate can only be lied to
            // about. The open is the check.
            let mut header = vec![0u8; segment::HEADER_BYTES];
            let Ok(file) = self
                .shared
                .vfs
                .open(&path, OpenOptions::of_kind(FileKind::Wal).read_only())
            else {
                // Not there at all, so the floor may pass it: the only thing
                // that creates a segment is `roll_now`, and it only ever
                // creates the one above the current.
                next_floor = match unbroken {
                    true => sequence.saturating_add(1),
                    false => next_floor,
                };
                continue;
            };
            if file.read_exact_at(0, &mut header).is_err() {
                unbroken = false;
                continue;
            }
            drop(file);
            let Ok(decoded) = SegmentHeader::decode(&header) else {
                unbroken = false;
                continue;
            };
            let next_first = self.first_lsn_of(sequence.saturating_add(1)).unwrap_or(lsn);
            let gone = decoded.first_lsn < lsn
                && next_first <= lsn
                && self.shared.vfs.delete(&path, false).is_ok();
            if gone {
                removed = removed.saturating_add(1);
            }
            // A segment this call left on disk - because it still holds records
            // recovery needs, or because the deletion was refused - keeps the
            // floor beneath itself, so the next call asks about it again.
            unbroken = unbroken && gone;
            next_floor = match unbroken {
                true => sequence.saturating_add(1),
                false => next_floor,
            };
        }
        self.shared
            .retired_below
            .store(next_floor.max(floor), Ordering::Relaxed);
        Ok(removed)
    }

    /// Returns the path of one segment.
    ///
    /// @param sequence - which segment
    pub fn segment_path(&self, sequence: u64) -> DbPath {
        segment_path(
            &self.shared.base,
            self.shared.directory.as_deref(),
            sequence,
        )
    }

    /// Returns the first LSN of one segment, by reading its header.
    ///
    /// @param sequence - which segment
    fn first_lsn_of(&self, sequence: u64) -> Option<u64> {
        let path = self.segment_path(sequence);
        let file = self
            .shared
            .vfs
            .open(&path, OpenOptions::of_kind(FileKind::Wal).read_only())
            .ok()?;
        let mut header = vec![0u8; segment::HEADER_BYTES];
        file.read_exact_at(0, &mut header).ok()?;
        SegmentHeader::decode(&header).ok().map(|it| it.first_lsn)
    }

    /// Rolls to the next segment when the current one is full.
    ///
    /// The decision is made on the position of the *next* record rather than on
    /// its length, so a segment overshoots by at most one record and no caller
    /// has to know how long a record will be before it writes one. Computing
    /// that length in advance would mean a second implementation of the
    /// encoder, which is the duplicate this codebase has already paid for once.
    fn roll_if_full(&self) -> DbResult<()> {
        let (needed, next) = {
            let inner = self.lock()?;
            let first = self.segment_first_lsn();
            (
                inner.next_lsn.saturating_sub(first) >= self.shared.segment_bytes,
                inner.next_lsn,
            )
        };
        if !needed {
            return Ok(());
        }
        self.roll_now(next)
    }

    /// Closes the current segment and opens the next one, whatever its size.
    ///
    /// **What makes a checkpoint able to reclaim the log.**
    /// `retire_segments_below` deletes a segment only when every record in it is
    /// below the checkpoint LSN, which the *current* segment never is: it is the
    /// one being appended to, so the checkpoint record itself lands in it and it
    /// is kept for ever. Rolling first moves the boundary to exactly the
    /// checkpoint point, so the segments behind it become entirely redundant and
    /// go, and the new one starts with the checkpoint record and nothing else.
    ///
    /// Nothing is deleted here, so a crash between this and the checkpoint
    /// leaves the same records in one more file - which recovery reads the same
    /// way it reads any segment boundary.
    ///
    /// Returns whether a roll happened: an idle log whose current segment holds
    /// no records is left alone, so a checkpoint on an unchanged database does
    /// not leave a segment behind per call.
    pub fn roll_segment(&self) -> DbResult<bool> {
        let next = {
            let inner = self.lock()?;
            if inner.next_lsn <= self.segment_first_lsn() {
                return Ok(false);
            }
            inner.next_lsn
        };
        self.roll_now(next)?;
        Ok(true)
    }

    /// Writes out the segment being closed and opens the next one.
    ///
    /// @param next - the stream position the new segment starts at
    fn roll_now(&self, next: u64) -> DbResult<()> {
        // Everything buffered belongs to the segment being closed, so it goes
        // out before the new one is opened. It is synced too: a segment nobody
        // will write to again whose tail is only in the page cache is a segment
        // recovery would find truncated for no reason.
        self.drive(next, self.synchronous() != Synchronous::Off)?;
        let mut segment = self.io_lock()?;
        let sequence = segment.sequence.saturating_add(1);
        let opened = open_segment(
            self.shared.vfs.as_ref(),
            &self.shared.base,
            self.shared.directory.as_deref(),
            self.shared.uuid,
            sequence,
            next,
        )?;
        *segment = opened;
        drop(segment);
        self.with_inner_mut(|inner| {
            inner.buffer_start = next;
            inner.stats.segments = inner.stats.segments.saturating_add(1);
        });
        Ok(())
    }

    /// Returns where the segment this handle is appending to currently ends.
    ///
    /// **The same answer [`tail_on_disk`] gives, off the handle this log
    /// already holds open.** That function takes a path and walks forward from
    /// a sequence, calling `access` at each one and opening the file to read
    /// its header - so asking it costs a path lookup and a file open every
    /// time. `ImportedDatabase::enter` asks it once per statement under
    /// `locking_mode = normal` to find out whether another process has appended
    /// to the log, and it was measured at **3.2 ms a statement, 37% of a 8.8 ms
    /// autocommit statement and as much as the whole checkpoint beside it**
    /// (task-1999). Its own doc comment says the check costs "one `file_size`
    /// per lock acquisition", and this is the function that makes that true.
    ///
    /// The size is read from the file rather than from this handle's own
    /// counters, which is the whole point: the question is what somebody else
    /// wrote, and a connection asking whether another process has committed
    /// cannot ask its own cache.
    ///
    /// **It answers for the open segment only, which is enough because a new
    /// segment cannot appear without the meta record moving first.** Only
    /// `roll_now` creates a segment, only a checkpoint rolls one, and a
    /// checkpoint bumps the meta record's generation before it releases the
    /// file lock. A caller therefore sees the generation move first, and
    /// `ImportedDatabase::the_meta_moved` is asked before this is.
    pub fn tail_of_open_segment(&self) -> DbResult<LogTail> {
        let segment = self.io_lock()?;
        let size = segment
            .file
            .file_size()
            .map_err(inillucent_vfs::VfsError::into_db_error)?;
        Ok(LogTail {
            sequence: segment.sequence,
            next_lsn: segment
                .first_lsn
                .saturating_add(size.saturating_sub(segment::HEADER_BYTES as u64)),
        })
    }

    /// Returns the open segment's first LSN.
    fn segment_first_lsn(&self) -> u64 {
        match self.shared.io.lock() {
            Ok(segment) => segment.first_lsn,
            Err(poisoned) => poisoned.into_inner().first_lsn,
        }
    }

    /// Makes everything below `end` written, and durable when `sync` is set.
    ///
    /// This is the commit gate. The first caller to find nobody draining takes
    /// the whole buffer - including records appended by threads that will wait
    /// behind it - and does the one write and the one sync for all of them.
    ///
    /// @param end - the stream position the caller needs to reach
    /// @param sync - whether the caller needs it durable rather than written
    fn drive(&self, end: u64, sync: bool) -> DbResult<()> {
        loop {
            let mut inner = self.lock()?;
            Wal::check_poison(&inner)?;
            let reached = if sync {
                inner.durable_end >= end
            } else {
                inner.written_end >= end
            };
            if reached {
                return Ok(());
            }
            if inner.draining {
                inner.stats.followers = inner.stats.followers.saturating_add(1);
                let (guard, _) = self
                    .shared
                    .gate
                    .wait_timeout(inner, std::time::Duration::from_millis(50))
                    .map_err(|_| misuse("the log's gate was poisoned by a panicking writer"))?;
                drop(guard);
                continue;
            }
            inner.draining = true;
            let payload = std::mem::take(&mut inner.buffer);
            let start = inner.buffer_start;
            let stop = start.saturating_add(payload.len() as u64);
            let unsynced_before = inner.unsynced;
            drop(inner);

            let outcome = self.write_and_sync(start, &payload, sync);

            let mut inner = self.lock()?;
            match outcome {
                Ok((writes, syncs)) => {
                    inner.buffer_start = stop;
                    inner.written_end = inner.written_end.max(stop);
                    inner.stats.writes = inner.stats.writes.saturating_add(writes);
                    inner.stats.syncs = inner.stats.syncs.saturating_add(syncs);
                    if syncs > 0 {
                        inner.durable_end = inner.written_end;
                        inner.unsynced = 0;
                    } else {
                        inner.unsynced = unsynced_before.saturating_add(payload.len() as u64);
                    }
                    // The buffer is reused rather than reallocated: a commit
                    // path that allocated a fresh buffer per group would be an
                    // allocation per commit on the one path the write family is
                    // measured on.
                    let mut recycled = payload;
                    recycled.clear();
                    // **Recycled, but not at any size.** One record may be
                    // larger than the bound - a `Structural` carries three
                    // whole pages - so a drain can hand back a buffer far above
                    // it, and keeping that capacity is exactly the retention
                    // the bound exists to stop. Anything past the bound is
                    // given back to the allocator here; anything under it is
                    // kept, so the ordinary commit path still allocates
                    // nothing.
                    if recycled.capacity() > SPILL_BYTES {
                        recycled.shrink_to(SPILL_BYTES);
                    }
                    if inner.buffer.is_empty() {
                        recycled.append(&mut inner.buffer);
                        inner.buffer = recycled;
                    }
                }
                Err(error) => {
                    // The records are put back so that nothing is silently
                    // lost, and the log is poisoned so that nothing is silently
                    // continued. Both matter: a caller that retried would
                    // otherwise write the same records twice.
                    let mut restored = payload;
                    restored.append(&mut inner.buffer);
                    inner.buffer = restored;
                    inner.poisoned = Some(error.clone());
                    inner.draining = false;
                    self.shared.gate.notify_all();
                    return Err(error);
                }
            }
            inner.draining = false;
            self.shared.gate.notify_all();
        }
    }

    /// Does the I/O half of a drain, with no lock on the bookkeeping held.
    ///
    /// @param start - the stream position the payload begins at
    /// @param payload - the bytes to write
    /// @param sync - whether to sync afterwards
    fn write_and_sync(&self, start: u64, payload: &[u8], sync: bool) -> DbResult<(u64, u64)> {
        let segment = self.io_lock()?;
        let mut writes = 0u64;
        if !payload.is_empty() {
            let offset = segment::HEADER_BYTES as u64
                + start
                    .checked_sub(segment.first_lsn)
                    .ok_or_else(|| misuse("a log drain would write before the segment it is in"))?;
            segment
                .file
                .write_all_at(offset, payload)
                .map_err(inillucent_vfs::VfsError::into_db_error)?;
            writes = 1;
        }
        let mut syncs = 0u64;
        if sync {
            segment
                .file
                .sync(SyncMode::Normal)
                .map_err(inillucent_vfs::VfsError::into_db_error)?;
            syncs = 1;
        }
        Ok((writes, syncs))
    }

    /// Locks the bookkeeping.
    fn lock(&self) -> DbResult<MutexGuard<'_, Inner>> {
        self.shared
            .inner
            .lock()
            .map_err(|_| misuse("the log's state was poisoned by a panicking writer"))
    }

    /// Locks the open segment.
    fn io_lock(&self) -> DbResult<MutexGuard<'_, OpenSegment>> {
        self.shared
            .io
            .lock()
            .map_err(|_| misuse("the log's segment was poisoned by a panicking writer"))
    }

    /// Returns the poisoning error, if there is one.
    ///
    /// @param inner - the locked bookkeeping
    fn check_poison(inner: &Inner) -> DbResult<()> {
        match &inner.poisoned {
            Some(error) => Err(error.clone()),
            None => Ok(()),
        }
    }

    /// Reads one field of the bookkeeping.
    ///
    /// @param read - what to read
    fn with_inner<R>(&self, read: impl FnOnce(&Inner) -> R) -> R {
        match self.shared.inner.lock() {
            Ok(inner) => read(&inner),
            Err(poisoned) => read(&poisoned.into_inner()),
        }
    }

    /// Changes one field of the bookkeeping.
    ///
    /// @param write - what to change
    fn with_inner_mut<R>(&self, write: impl FnOnce(&mut Inner) -> R) -> R {
        match self.shared.inner.lock() {
            Ok(mut inner) => write(&mut inner),
            Err(poisoned) => write(&mut poisoned.into_inner()),
        }
    }
}

/// Returns the path of one segment.
///
/// @param base - the database file's path as a string
/// @param directory - the directory the database is in, if it has one
/// @param sequence - which segment
pub fn segment_path(base: &str, directory: Option<&std::path::Path>, sequence: u64) -> DbPath {
    let stem = std::path::Path::new(base)
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_else(|| base.to_string());
    let name = segment::segment_name(&stem, sequence);
    match directory {
        Some(directory) if !directory.as_os_str().is_empty() => DbPath::new(directory.join(name)),
        _ => DbPath::new(&name),
    }
}

/// Where the log beside a database currently ends, as the files say.
///
/// **Read from the directory rather than from a connection's memory**, because
/// the position a connection remembers was discovered when it opened the file
/// and another process may have appended since. Two processes that each
/// computed the append position at `open` computed the same one and each wrote
/// over the other's records - 120 acknowledged inserts, 60 rows present
/// (task-1979, section 4).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LogTail {
    /// The highest segment beside the database that belongs to it.
    pub sequence: u64,
    /// The stream position past that segment's last byte.
    pub next_lsn: u64,
}

/// Returns where the log beside `base` ends, or nothing when no segment of it
/// is there.
///
/// The walk starts at `from` and stops at the first sequence with no file,
/// because segments are contiguous: `read_chain` stops a chain at a gap for the
/// same reason, and a numbered file past a gap is a leftover rather than part
/// of this log.
///
/// **Only a caller holding the file lock may act on the answer.** Without the
/// lock another process can append between the read and the use, which is the
/// defect this exists to close rather than one to repeat one layer up.
///
/// **This is the form for a caller with no log open**, and since task-1999 it
/// has no caller inside the workspace. It takes a path, so every call is a
/// path lookup and a file open, and the two callers it had -
/// `ImportedDatabase::the_log_moved` and its attached twin - ask the question
/// once per statement per file under `locking_mode = normal`. They now ask
/// [`Wal::tail_of_open_segment`] instead, off the handle they already hold, and
/// that was worth 3.2 ms of an 8.8 ms autocommit statement. It is kept because
/// it is published API of this crate and it answers for a log this process has
/// not opened, which the method cannot.
///
/// @param vfs - the file system the segments live on
/// @param base - the database file the segments are named after
/// @param uuid - the database's identity; a segment that disagrees is ignored
/// @param from - the lowest sequence to look at
pub fn tail_on_disk(
    vfs: &dyn Vfs,
    base: &DbPath,
    uuid: u128,
    from: u64,
) -> DbResult<Option<LogTail>> {
    let name = base.as_path().to_string_lossy().to_string();
    let directory = base.as_path().parent().map(std::path::Path::to_path_buf);
    let mut found: Option<LogTail> = None;
    let mut sequence = from.max(1);
    loop {
        let path = segment_path(&name, directory.as_deref(), sequence);
        let there = vfs
            .access(&path, inillucent_vfs::AccessMode::Exists)
            .map_err(inillucent_vfs::VfsError::into_db_error)?;
        if !there {
            return Ok(found);
        }
        match read_tail(vfs, &path, uuid)? {
            Some(tail) => found = Some(tail),
            // A file at this sequence that is not a segment of this database
            // ends the walk: the chain cannot continue through it, and
            // reporting a later one as this log's tail would name a position
            // no record of this log occupies.
            None => return Ok(found),
        }
        sequence = sequence.saturating_add(1);
    }
}

/// Returns where one segment file ends, or nothing when it is not a segment of
/// this database.
///
/// @param vfs - the file system
/// @param path - the segment file
/// @param uuid - the database's identity
fn read_tail(vfs: &dyn Vfs, path: &DbPath, uuid: u128) -> DbResult<Option<LogTail>> {
    let file = vfs
        .open(path, OpenOptions::of_kind(FileKind::Wal))
        .map_err(inillucent_vfs::VfsError::into_db_error)?;
    let size = file
        .file_size()
        .map_err(inillucent_vfs::VfsError::into_db_error)?;
    if size < segment::HEADER_BYTES as u64 {
        return Ok(None);
    }
    let mut head = vec![0u8; segment::HEADER_BYTES];
    if file.read_exact_at(0, &mut head).is_err() {
        return Ok(None);
    }
    let Ok(header) = SegmentHeader::decode(&head) else {
        return Ok(None);
    };
    if header.uuid != uuid {
        return Ok(None);
    }
    Ok(Some(LogTail {
        sequence: header.sequence,
        next_lsn: header
            .first_lsn
            .saturating_add(size.saturating_sub(segment::HEADER_BYTES as u64)),
    }))
}

/// Opens or creates one segment and writes its header.
///
/// @param vfs - the file system
/// @param base - the database file's path as a string
/// @param directory - the directory the database is in
/// @param uuid - the database's identity
/// @param sequence - which segment
/// @param first_lsn - the stream position of the segment's first record
fn open_segment(
    vfs: &dyn Vfs,
    base: &str,
    directory: Option<&std::path::Path>,
    uuid: u128,
    sequence: u64,
    first_lsn: u64,
) -> DbResult<OpenSegment> {
    let path = segment_path(base, directory, sequence);
    let file = vfs
        .open(&path, OpenOptions::of_kind(FileKind::Wal))
        .map_err(inillucent_vfs::VfsError::into_db_error)?;
    let existing = file
        .file_size()
        .map_err(inillucent_vfs::VfsError::into_db_error)?;
    // **A segment this database wrote is kept, not rewritten.**
    //
    // The first version truncated it, on the argument that "recovery has
    // already read everything below `first_lsn` out of it". That argument is
    // false for a redo-only engine, and the falseness is the whole point of
    // this phase: recovery reads records into the **buffer pool**, not into the
    // data file. Until a checkpoint writes those pages, the log is the only
    // place the changes exist - so truncating it at open threw away every
    // commit since the last checkpoint, and the next crash lost them all.
    //
    // Nothing caught it because a single crash cannot: the run that truncates
    // the log is the run that has the rows in memory, and it answers every
    // question correctly. It takes a **second** crash, before any checkpoint,
    // for the loss to become visible - which is what the model campaign's
    // traces do and what no test in the suite did.
    //
    // The stale tail is not a worry: `recover::truncate_after` has already cut
    // the file back to the last record that decoded, and `first_lsn` is where
    // that record ended.
    if existing >= segment::HEADER_BYTES as u64 {
        let mut head = vec![0u8; segment::HEADER_BYTES];
        // **A failed read of this header is not evidence about what the
        // segment holds.** `file_size` just said the file is long enough, so
        // a failure reading its first bytes is the VFS reporting an
        // operational problem - an I/O error, a full disk, a denied
        // permission - the same distinction `recover::read_chain` already
        // makes about the same kind of read. The first version of this
        // matched on `.is_ok()` alone, so any such failure fell through to
        // the "foreign or unreadable segment" arm below and **truncated a
        // segment that might hold every commit since the last checkpoint** -
        // this crate's own module comment on the very next lines explains why
        // that loss is invisible until a second crash. Only a genuine
        // short-read - the media model's torn-tail signal - is read the same
        // way a truly foreign or damaged header is; every other failure
        // propagates instead of being swallowed.
        match file.read_exact_at(0, &mut head) {
            Ok(()) => {
                if let Ok(held) = SegmentHeader::decode(&head) {
                    if held.belongs_to(uuid, sequence).is_ok() && held.first_lsn <= first_lsn {
                        return Ok(OpenSegment {
                            file,
                            path,
                            sequence,
                            // The segment's *own* first LSN, which is what every
                            // write offset is measured from. Using the resume
                            // position here would put the next record at the wrong
                            // place in a segment that already holds records.
                            first_lsn: held.first_lsn,
                        });
                    }
                }
            }
            Err(error) => {
                if error.extended() != inillucent_base::error::ExtendedCode::IO_ERR_SHORT_READ {
                    return Err(error.into_db_error());
                }
            }
        }
        // A file at this sequence that this database did not write, or one
        // whose records start above where we mean to resume, is not a log we
        // can append to. It is replaced.
        file.truncate(0)
            .map_err(inillucent_vfs::VfsError::into_db_error)?;
    }
    let header = SegmentHeader {
        sequence,
        first_lsn,
        uuid,
    };
    let mut image = vec![0u8; segment::HEADER_BYTES];
    header.encode(&mut image)?;
    file.write_all_at(0, &image)
        .map_err(inillucent_vfs::VfsError::into_db_error)?;
    file.sync(SyncMode::Normal)
        .map_err(inillucent_vfs::VfsError::into_db_error)?;
    Ok(OpenSegment {
        file,
        path,
        sequence,
        first_lsn,
    })
}

/// Encodes a policy as the integer the atomic holds.
///
/// @param policy - the policy
fn policy_code(policy: Synchronous) -> u64 {
    match policy {
        Synchronous::Off => 0,
        Synchronous::Normal => 1,
        Synchronous::Full => 2,
    }
}

/// Decodes the integer the atomic holds.
///
/// Anything but the two lower codes is `Full`, so a value this module did not
/// write can only ever be *safer* than intended rather than less safe.
///
/// @param code - what the atomic held
fn policy_of(code: u64) -> Synchronous {
    match code {
        0 => Synchronous::Off,
        1 => Synchronous::Normal,
        _ => Synchronous::Full,
    }
}

impl std::fmt::Debug for Wal {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let stats = self.stats();
        formatter
            .debug_struct("Wal")
            .field("durable_end", &self.durable_end())
            .field("next_lsn", &self.next_lsn())
            .field("synchronous", &self.synchronous().name())
            .field("stats", &stats)
            .finish()
    }
}

impl OpenSegment {
    /// Returns the segment's path, for a caller that wants to report it.
    #[allow(dead_code)]
    fn path(&self) -> &DbPath {
        &self.path
    }
}
