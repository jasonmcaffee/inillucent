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
        };
        Ok(Wal {
            shared: Arc::new(shared),
        })
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

    /// Returns the sequence number of the segment being written.
    pub fn sequence(&self) -> u64 {
        match self.shared.io.lock() {
            Ok(segment) => segment.sequence,
            Err(poisoned) => poisoned.into_inner().sequence,
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
        let end = self.with_inner(|inner| inner.next_lsn);
        self.await_commit(end)?;
        Ok(lsn)
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
    /// @param lsn - the checkpoint LSN
    pub fn retire_segments_below(&self, lsn: u64) -> DbResult<usize> {
        let current = self.sequence();
        let mut removed = 0usize;
        for sequence in 1..current {
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
                continue;
            };
            if file.read_exact_at(0, &mut header).is_err() {
                continue;
            }
            drop(file);
            let Ok(decoded) = SegmentHeader::decode(&header) else {
                continue;
            };
            let next_first = self.first_lsn_of(sequence.saturating_add(1)).unwrap_or(lsn);
            if decoded.first_lsn < lsn
                && next_first <= lsn
                && self.shared.vfs.delete(&path, false).is_ok()
            {
                removed = removed.saturating_add(1);
            }
        }
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
        if file.read_exact_at(0, &mut head).is_ok() {
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
