//! The log writer, measured rather than described.
//!
//! Invariant: every claim about the log here is asserted against a
//! *counter or an artefact* - how many syncs the file system saw, what bytes are
//! on the media - and never against the log agreeing with itself. A durability
//! layer that is tested by asking it whether it is durable is a durability layer
//! with no test at all, which is what `recovery.rs` in the old engine turned out
//! to be: 1,461 tests passing around it and none of its own.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use inillucent_sim::{Failure, SimConfig, SimVfs, Site};
use inillucent_vfs::{DbPath, MemoryVfs, Vfs};
use inillucent_wal::record::{Body, Record};
use inillucent_wal::segment::{self, SegmentHeader};
use inillucent_wal::writer::{Wal, WalOptions};
use inillucent_wal::{Synchronous, FIRST_LSN};

/// Opens a log over a fresh in-memory file system.
///
/// @param options - the sync policy and segment size
fn fresh(options: WalOptions) -> (Arc<dyn Vfs>, DbPath, Wal) {
    let vfs: Arc<dyn Vfs> = Arc::new(MemoryVfs::new());
    let path = DbPath::new("log-test.rdb");
    let wal = Wal::open(Arc::clone(&vfs), &path, 0xABCD, FIRST_LSN, 1, options).expect("a log");
    (vfs, path, wal)
}

/// Returns the bytes of one segment as the file system holds them.
///
/// @param vfs - the file system
/// @param base - the database path
/// @param sequence - which segment
fn segment_bytes(vfs: &dyn Vfs, base: &DbPath, sequence: u64) -> Vec<u8> {
    let path = inillucent_wal::writer::segment_path(
        &base.as_path().to_string_lossy(),
        base.as_path().parent(),
        sequence,
    );
    let file = vfs
        .open(
            &path,
            inillucent_vfs::OpenOptions::of_kind(inillucent_vfs::FileKind::Wal),
        )
        .expect("the segment opens");
    let size = file.file_size().expect("a size") as usize;
    let mut bytes = vec![0u8; size];
    file.read_exact_at(0, &mut bytes)
        .expect("the segment reads");
    bytes
}

/// The records a segment's bytes hold, decoded in order.
///
/// @param bytes - the whole segment
fn records_of(bytes: &[u8]) -> Vec<(u64, u64, u8)> {
    let mut out = Vec::new();
    let mut at = segment::HEADER_BYTES;
    while let Ok(Some(record)) = Record::decode(bytes.get(at..).unwrap_or(&[])) {
        out.push((record.lsn, record.txn, record.body.kind()));
        at += record.length;
    }
    out
}

/// A record that is appended and flushed is on the media, in order, with the
/// LSN the append returned.
#[test]
fn appended_records_land_where_their_lsn_says() {
    let (vfs, path, wal) = fresh(WalOptions::default());
    let mut lsns = Vec::new();
    for index in 0..8u64 {
        lsns.push(
            wal.append(
                index + 1,
                Body::InsertRow {
                    tree: 1,
                    page: index + 10,
                    row: b"a row of some length",
                },
            )
            .expect("an append"),
        );
    }
    wal.flush().expect("a flush");
    let bytes = segment_bytes(vfs.as_ref(), &path, 1);
    let header = SegmentHeader::decode(&bytes).expect("a header");
    assert_eq!(header.first_lsn, FIRST_LSN);
    assert_eq!(header.sequence, 1);
    assert_eq!(header.uuid, 0xABCD);
    let found: Vec<u64> = records_of(&bytes).iter().map(|entry| entry.0).collect();
    assert_eq!(
        found, lsns,
        "every record sits at the lsn its append returned"
    );
    assert_eq!(wal.written_end(), wal.next_lsn());
}

/// `synchronous` changes what happens, not just what is stored.
///
/// The acceptance says each policy has a test that shows the behaviour changing.
/// The observable is the sync count the file system saw, and the three policies
/// answer differently on the same ten commits.
#[test]
fn each_synchronous_policy_changes_the_number_of_syncs() {
    let mut counts = Vec::new();
    for policy in [Synchronous::Full, Synchronous::Normal, Synchronous::Off] {
        let (_, _, wal) = fresh(WalOptions {
            synchronous: policy,
            ..WalOptions::default()
        });
        for txn in 1..=10u64 {
            wal.append(
                txn,
                Body::InsertRow {
                    tree: 1,
                    page: txn,
                    row: b"row",
                },
            )
            .expect("an append");
            wal.commit(txn, txn).expect("a commit");
        }
        let stats = wal.stats();
        counts.push((policy, stats.syncs, wal.durable_end(), wal.written_end()));
    }

    let full = counts.first().copied().expect("full");
    assert_eq!(full.1, 10, "FULL syncs once per commit");
    assert_eq!(full.2, full.3, "FULL leaves nothing written but unsynced");

    let normal = counts.get(1).copied().expect("normal");
    assert_eq!(normal.1, 0, "NORMAL does not sync under 64 MiB of log");
    assert!(
        normal.3 > FIRST_LSN,
        "NORMAL still writes: it is the sync it skips, not the write"
    );
    assert_eq!(
        normal.2, FIRST_LSN,
        "NORMAL has nothing durable, which is what it promises"
    );

    let off = counts.get(2).copied().expect("off");
    assert_eq!(off.1, 0, "OFF never syncs");
    assert!(off.3 > FIRST_LSN, "OFF still writes");
}

/// A checkpoint syncs under NORMAL and does not under OFF.
///
/// This is the second half of the policy contract and the half that is easy to
/// get wrong, because both policies look identical while commits are happening.
#[test]
fn a_checkpoint_syncs_under_normal_and_not_under_off() {
    for (policy, expected) in [(Synchronous::Normal, 1u64), (Synchronous::Off, 0)] {
        let (_, _, wal) = fresh(WalOptions {
            synchronous: policy,
            ..WalOptions::default()
        });
        wal.append(
            1,
            Body::WritePage {
                page: 3,
                image: b"an image",
            },
        )
        .expect("an append");
        wal.commit(1, 1).expect("a commit");
        assert_eq!(wal.stats().syncs, 0, "{policy:?} synced during the commit");
        wal.note_checkpoint(wal.written_end(), 1)
            .expect("a checkpoint");
        assert_eq!(
            wal.stats().syncs,
            expected,
            "{policy:?} syncs {expected} times at a checkpoint"
        );
    }
}

/// The policy can be changed while the log is open, and the change takes.
#[test]
fn the_policy_can_be_changed_and_is_reported() {
    let (_, _, wal) = fresh(WalOptions {
        synchronous: Synchronous::Off,
        ..WalOptions::default()
    });
    assert_eq!(wal.synchronous(), Synchronous::Off);
    wal.commit(1, 1).expect("a commit");
    assert_eq!(wal.stats().syncs, 0);
    wal.set_synchronous(Synchronous::Full);
    assert_eq!(wal.synchronous().name(), "full");
    wal.commit(2, 2).expect("a commit");
    assert_eq!(wal.stats().syncs, 1, "the new policy took effect");
    assert_eq!(Synchronous::parse("NORMAL").unwrap(), Synchronous::Normal);
    assert_eq!(Synchronous::parse("2").unwrap(), Synchronous::Full);
    assert!(Synchronous::parse("sometimes").is_err());
}

/// The log rolls to a new segment and the stream is continuous across the roll.
#[test]
fn the_log_rolls_and_the_stream_is_continuous() {
    let (vfs, path, wal) = fresh(WalOptions {
        synchronous: Synchronous::Full,
        segment_bytes: 4_096,
    });
    let mut lsns = Vec::new();
    for index in 0..64u64 {
        lsns.push(
            wal.append(
                1,
                Body::WritePage {
                    page: index,
                    image: &[0xABu8; 200],
                },
            )
            .expect("an append"),
        );
    }
    wal.flush().expect("a flush");
    assert!(wal.sequence() > 1, "the log rolled");

    // Every record in every segment, in order, with the segment headers saying
    // where each one starts. The check that matters is that concatenating the
    // record areas reproduces the LSNs the appends returned, with no gap and no
    // overlap - which is what "LSNs are positions in a byte stream" means.
    let mut found = Vec::new();
    for sequence in 1..=wal.sequence() {
        let bytes = segment_bytes(vfs.as_ref(), &path, sequence);
        let header = SegmentHeader::decode(&bytes).expect("a header");
        assert_eq!(header.sequence, sequence);
        let first_in_segment = records_of(&bytes).first().map(|entry| entry.0);
        if let Some(first) = first_in_segment {
            assert_eq!(
                first, header.first_lsn,
                "segment {sequence} says it starts at {} and its first record is at {first}",
                header.first_lsn
            );
        }
        found.extend(records_of(&bytes).iter().map(|entry| entry.0));
    }
    assert_eq!(found, lsns);
}

/// A file system that counts syncs, so group commit can be measured rather than
/// asserted.
#[derive(Debug)]
struct CountingVfs {
    inner: MemoryVfs,
    syncs: Arc<AtomicU64>,
    writes: Arc<AtomicU64>,
}

/// A file that counts its syncs and writes.
#[derive(Debug)]
struct CountingFile {
    inner: Box<dyn inillucent_vfs::VfsFile>,
    syncs: Arc<AtomicU64>,
    writes: Arc<AtomicU64>,
}

impl inillucent_vfs::VfsFile for CountingFile {
    fn read_exact_at(&self, offset: u64, output: &mut [u8]) -> inillucent_vfs::VfsResult<()> {
        self.inner.read_exact_at(offset, output)
    }
    fn write_all_at(&self, offset: u64, input: &[u8]) -> inillucent_vfs::VfsResult<()> {
        self.writes.fetch_add(1, Ordering::SeqCst);
        self.inner.write_all_at(offset, input)
    }
    fn file_size(&self) -> inillucent_vfs::VfsResult<u64> {
        self.inner.file_size()
    }
    fn truncate(&self, size: u64) -> inillucent_vfs::VfsResult<()> {
        self.inner.truncate(size)
    }
    fn sync(&self, mode: inillucent_vfs::SyncMode) -> inillucent_vfs::VfsResult<()> {
        self.syncs.fetch_add(1, Ordering::SeqCst);
        self.inner.sync(mode)
    }
    fn lock(&self, level: inillucent_vfs::FileLock) -> inillucent_vfs::VfsResult<()> {
        self.inner.lock(level)
    }
    fn unlock(&self, level: inillucent_vfs::FileLock) -> inillucent_vfs::VfsResult<()> {
        self.inner.unlock(level)
    }
    fn lock_level(&self) -> inillucent_vfs::FileLock {
        self.inner.lock_level()
    }
    fn check_reserved_lock(&self) -> inillucent_vfs::VfsResult<bool> {
        self.inner.check_reserved_lock()
    }
    fn device_characteristics(&self) -> inillucent_vfs::DeviceCharacteristics {
        self.inner.device_characteristics()
    }
    fn shared_memory(
        &self,
    ) -> inillucent_vfs::VfsResult<Option<Arc<dyn inillucent_vfs::SharedMemory>>> {
        self.inner.shared_memory()
    }
    fn file_identity(&self) -> inillucent_vfs::VfsResult<inillucent_vfs::FileIdentity> {
        self.inner.file_identity()
    }
}

impl Vfs for CountingVfs {
    fn name(&self) -> &str {
        "counting"
    }
    fn open(
        &self,
        path: &DbPath,
        options: inillucent_vfs::OpenOptions,
    ) -> inillucent_vfs::VfsResult<Box<dyn inillucent_vfs::VfsFile>> {
        Ok(Box::new(CountingFile {
            inner: self.inner.open(path, options)?,
            syncs: Arc::clone(&self.syncs),
            writes: Arc::clone(&self.writes),
        }))
    }
    fn delete(&self, path: &DbPath, sync_dir: bool) -> inillucent_vfs::VfsResult<()> {
        self.inner.delete(path, sync_dir)
    }
    fn access(
        &self,
        path: &DbPath,
        mode: inillucent_vfs::AccessMode,
    ) -> inillucent_vfs::VfsResult<bool> {
        self.inner.access(path, mode)
    }
    fn full_pathname(&self, path: &DbPath) -> inillucent_vfs::VfsResult<DbPath> {
        self.inner.full_pathname(path)
    }
    fn randomness(&self, output: &mut [u8]) -> inillucent_vfs::VfsResult<()> {
        self.inner.randomness(output)
    }
    fn current_time(&self) -> inillucent_vfs::VfsResult<std::time::SystemTime> {
        self.inner.current_time()
    }
    fn temp_path(&self, prefix: &str) -> inillucent_vfs::VfsResult<DbPath> {
        self.inner.temp_path(prefix)
    }
    fn sleep(&self, micros: u64) -> inillucent_vfs::VfsResult<()> {
        self.inner.sleep(micros)
    }
}

/// Concurrent committers share one write and one sync.
///
/// This is the property the TDD asks a log-writer thread for, and it is the
/// reason the leader-follower drain is an implementation of that design rather
/// than a substitute for it. The assertion is on the *file system's* counters,
/// not on the log's, so a log that counted a group commit it did not perform
/// would fail.
#[test]
fn concurrent_committers_share_one_write_and_one_sync() {
    let syncs = Arc::new(AtomicU64::new(0));
    let writes = Arc::new(AtomicU64::new(0));
    let vfs: Arc<dyn Vfs> = Arc::new(CountingVfs {
        inner: MemoryVfs::new(),
        syncs: Arc::clone(&syncs),
        writes: Arc::clone(&writes),
    });
    let path = DbPath::new("group.rdb");
    let wal = Wal::open(
        Arc::clone(&vfs),
        &path,
        7,
        FIRST_LSN,
        1,
        WalOptions::default(),
    )
    .expect("a log");
    // Opening the segment writes and syncs its header; the group commit is
    // measured from there.
    let base_syncs = syncs.load(Ordering::SeqCst);
    let base_writes = writes.load(Ordering::SeqCst);

    // **The barrier goes between appending and waiting, not between appending
    // the row and appending the commit.**
    //
    // The first version put it in the middle of `commit`, which appends the
    // `Commit` record and then drives - so a thread that got all the way
    // through before the next one appended drained a buffer holding only its
    // own record, and each committer took its own sync. That is a *timing*
    // assertion dressed as a property one: it passed on a quiet machine and
    // failed the first time the whole suite ran in parallel.
    //
    // Split into its two halves, the property is exact rather than likely:
    // after the barrier every record is in the buffer, so whichever thread
    // drains first drains all of them, and the rest find their own end already
    // durable. One write and one sync, deterministically.
    let threads = 8usize;
    let barrier = Arc::new(std::sync::Barrier::new(threads));
    let mut handles = Vec::new();
    for index in 0..threads {
        let handle = wal.handle();
        let barrier = Arc::clone(&barrier);
        handles.push(std::thread::spawn(move || {
            let txn = index as u64 + 1;
            handle
                .append(
                    txn,
                    Body::InsertRow {
                        tree: 1,
                        page: txn,
                        row: b"a row",
                    },
                )
                .expect("an append");
            handle
                .append(txn, Body::Commit { cts: txn })
                .expect("a commit record");
            let end = handle.next_lsn();
            barrier.wait();
            handle
                .await_commit(end)
                .expect("the commit becomes durable");
        }));
    }
    for handle in handles {
        handle.join().expect("a thread");
    }

    let sync_count = syncs.load(Ordering::SeqCst) - base_syncs;
    let write_count = writes.load(Ordering::SeqCst) - base_writes;
    assert_eq!(
        sync_count, 1,
        "{threads} committers with everything already buffered took {sync_count} syncs,          which is not a group commit"
    );
    assert_eq!(
        write_count, 1,
        "{threads} committers with everything already buffered took {write_count} writes,          which is not a group commit"
    );
    // Everything every thread committed is durable when the last one returns.
    assert_eq!(wal.durable_end(), wal.next_lsn());
}

/// A failed write poisons the log rather than letting it carry on.
///
/// The failure is injected at the VFS, which is where a real one comes from,
/// and the assertion is that the *next* call fails too - a log that reported one
/// commit durable, failed, and kept appending would have a durable prefix that
/// is not a prefix.
#[test]
fn a_failed_write_poisons_the_log() {
    let vfs = Arc::new(SimVfs::new(SimConfig::default()));
    let path = DbPath::new("poison.rdb");
    let wal = Wal::open(
        Arc::clone(&vfs) as Arc<dyn Vfs>,
        &path,
        3,
        FIRST_LSN,
        1,
        WalOptions::default(),
    )
    .expect("a log");
    wal.append(
        1,
        Body::WritePage {
            page: 1,
            image: b"before",
        },
    )
    .expect("an append");
    vfs.failpoints().set(
        Site::Write,
        inillucent_sim::Policy::Always(Failure::IoError),
    );
    let failed = wal.commit(1, 1).expect_err("the commit fails");
    assert!(failed.detail().is_some());
    assert!(wal.is_poisoned());
    vfs.failpoints()
        .set(Site::Write, inillucent_sim::Policy::Off);
    assert!(
        wal.append(2, Body::Abort).is_err(),
        "a poisoned log refuses new work even once the media is back"
    );
    assert!(wal.commit(2, 2).is_err());
}

/// A sync that fails is reported, and the log does not claim what failed.
#[test]
fn a_failed_sync_does_not_advance_the_durable_end() {
    let vfs = Arc::new(SimVfs::new(SimConfig::default()));
    let path = DbPath::new("sync-fail.rdb");
    let wal = Wal::open(
        Arc::clone(&vfs) as Arc<dyn Vfs>,
        &path,
        3,
        FIRST_LSN,
        1,
        WalOptions::default(),
    )
    .expect("a log");
    let before = wal.durable_end();
    vfs.failpoints()
        .set(Site::Sync, inillucent_sim::Policy::Always(Failure::IoError));
    assert!(wal.commit(1, 1).is_err());
    assert_eq!(
        wal.durable_end(),
        before,
        "a sync that failed did not make anything durable"
    );
}

/// The checkpoint trigger fires on bytes and on time, and resets.
#[test]
fn the_checkpoint_trigger_fires_on_bytes_and_on_time() {
    let (_, _, wal) = fresh(WalOptions::default());
    assert!(!wal.checkpoint_due(0));
    assert!(
        wal.checkpoint_due(inillucent_wal::writer::CHECKPOINT_MICROS),
        "thirty seconds is a trigger on its own"
    );
    let image = vec![0u8; 1 << 20];
    while wal.since_checkpoint() < inillucent_wal::writer::CHECKPOINT_BYTES {
        wal.append(
            1,
            Body::WritePage {
                page: 1,
                image: &image,
            },
        )
        .expect("an append");
    }
    assert!(wal.checkpoint_due(0), "256 MiB is a trigger on its own");
    wal.note_checkpoint(wal.next_lsn(), 5)
        .expect("a checkpoint");
    assert_eq!(wal.since_checkpoint(), 0, "the trigger resets");
    assert!(!wal.checkpoint_due(0));
}
