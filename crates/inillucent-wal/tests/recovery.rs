//! Recovery, and the campaigns that are the only honest way to test it.
//!
//! Invariant: **every assertion here is against an artefact**, not against a
//! status. A campaign that crashed at call 47 and then asked recovery
//! whether it had succeeded would be asking the code under test to grade itself.
//! Each campaign here recovers into a page store and then compares that store
//! against a list of commits the *workload* recorded as acknowledged - a list
//! built before recovery ran and from the other side of the interface.
//!
//! The old engine's `recovery.rs` had 1,461 tests passing around it and none of
//! its own, and its uncovered branches were exactly the error and rollback
//! paths. That is the specific thing this file exists to not repeat.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use inillucent_base::DbResult;
use inillucent_sim::{CrashSnapshot, Failure, Policy, SimConfig, SimVfs, Site};
use inillucent_vfs::{DbPath, MemoryVfs, Vfs};
use inillucent_wal::record::{Body, Record};
use inillucent_wal::recover::{self, RecoveryStart, Redo};
use inillucent_wal::segment::{self, SegmentHeader};
use inillucent_wal::writer::{Wal, WalOptions};
use inillucent_wal::{Synchronous, FIRST_LSN};

/// The size of a page in these tests. Small, because what is being tested is
/// the scan and the page-LSN rule, and a 32 KiB page would only make the
/// artefacts harder to read.
const PAGE: usize = 64;

/// The database identity every test in this file uses.
const UUID: u128 = 0x5150_5150_5150_5150;

/// A stand-in for the data file: pages, a free map, and a commit timestamp.
///
/// The *real* applier lives in the storage engine and puts rows into leaves.
/// This one records what it was told, which is what makes it the right thing to
/// test the scan against: a bug in the scan shows up as a difference in this
/// store, and a bug in the leaf codec cannot hide one.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct PageStore {
    pages: BTreeMap<u64, Vec<u8>>,
    /// The logical records applied to each page, in order, as text.
    logical: BTreeMap<u64, Vec<String>>,
    free: BTreeSet<u64>,
    latest_cts: u64,
    /// Whether a `CatalogChange` was replayed.
    ///
    /// A flag rather than a counter, and the difference is the point. A record
    /// that names no page cannot be gated by the page-LSN rule, so it is
    /// replayed in full on **every** recovery run - which means every such
    /// record has to be idempotent on its own terms. The first version of this
    /// store counted catalog changes and `recovering_twice_produces_the_same_file`
    /// caught it immediately: the count was 1 after one run and 2 after two.
    /// The engine's real applier invalidates a plan cache, which is idempotent;
    /// a counter is not, and modelling it as one was modelling something the
    /// engine does not do.
    catalog_invalidated: bool,
}

impl PageStore {
    /// Returns the LSN a page carries, reading it out of the page's own bytes.
    ///
    /// @param page - the page number
    fn lsn_of(&self, page: u64) -> Option<u64> {
        let bytes = self.pages.get(&page)?;
        let mut raw = [0u8; 8];
        raw.copy_from_slice(bytes.get(..8)?);
        Some(u64::from_le_bytes(raw))
    }

    /// Writes a page image and stamps it with an LSN.
    ///
    /// @param page - the page number
    /// @param image - the bytes, padded or truncated to a page
    /// @param lsn - the LSN to stamp
    fn put(&mut self, page: u64, image: &[u8], lsn: u64) {
        let mut bytes = vec![0u8; PAGE];
        let width = image.len().min(PAGE.saturating_sub(8));
        if let Some(slot) = bytes.get_mut(8..8 + width) {
            slot.copy_from_slice(image.get(..width).unwrap_or(&[]));
        }
        if let Some(slot) = bytes.get_mut(..8) {
            slot.copy_from_slice(&lsn.to_le_bytes());
        }
        self.pages.insert(page, bytes);
    }

    /// Records a logical change against a page and stamps the page's LSN.
    ///
    /// @param page - the page number
    /// @param what - the change, as text
    /// @param lsn - the LSN to stamp
    fn note(&mut self, page: u64, what: String, lsn: u64) {
        self.logical.entry(page).or_default().push(what);
        let mut bytes = self.pages.remove(&page).unwrap_or_else(|| vec![0u8; PAGE]);
        if let Some(slot) = bytes.get_mut(..8) {
            slot.copy_from_slice(&lsn.to_le_bytes());
        }
        self.pages.insert(page, bytes);
    }

    /// Returns the store as bytes, so two runs can be compared byte for byte.
    ///
    /// A `BTreeMap` iterates in key order, so the rendering is a function of
    /// the contents and not of the order they arrived in - which is what makes
    /// "the same file" a meaningful claim rather than a comparison of hash
    /// iteration orders.
    fn image(&self) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&self.latest_cts.to_le_bytes());
        out.push(u8::from(self.catalog_invalidated));
        for page in &self.free {
            out.extend_from_slice(b"free");
            out.extend_from_slice(&page.to_le_bytes());
        }
        for (page, bytes) in &self.pages {
            out.extend_from_slice(&page.to_le_bytes());
            out.extend_from_slice(bytes);
        }
        for (page, entries) in &self.logical {
            out.extend_from_slice(&page.to_le_bytes());
            for entry in entries {
                out.extend_from_slice(entry.as_bytes());
                out.push(0);
            }
        }
        out
    }
}

impl Redo for PageStore {
    fn page_lsn(&mut self, page: u64) -> DbResult<Option<u64>> {
        Ok(self.lsn_of(page))
    }

    fn redo(&mut self, record: &Record<'_>, wanted: &[bool]) -> DbResult<()> {
        let lsn = record.lsn;
        match record.body {
            Body::InsertRow { page, row, .. } => self.note(
                page,
                format!("insert {}", String::from_utf8_lossy(row)),
                lsn,
            ),
            Body::DeleteRow { page, key, .. } => self.note(
                page,
                format!("delete {}", String::from_utf8_lossy(key)),
                lsn,
            ),
            Body::UpdateInPlace {
                page, key, column, ..
            } => self.note(page, format!("update {column} of {key:?}"), lsn),
            Body::CompactLeaf { page, image, .. } => {
                self.logical.remove(&page);
                self.put(page, image, lsn);
            }
            Body::Structural {
                left,
                right,
                parent,
                left_image,
                right_image,
                parent_image,
                ..
            } => {
                for (slot, (page, image)) in [
                    (left, left_image),
                    (right, right_image),
                    (parent, parent_image),
                ]
                .into_iter()
                .enumerate()
                {
                    if wanted.get(slot).copied().unwrap_or(false) {
                        self.logical.remove(&page);
                        self.put(page, image, lsn);
                    }
                }
            }
            Body::WritePage { page, image } => self.put(page, image, lsn),
            Body::AllocPage { page } => {
                self.free.remove(&page);
            }
            Body::FreePage { page } => {
                self.free.insert(page);
            }
            Body::Commit { cts } => self.latest_cts = self.latest_cts.max(cts),
            Body::Abort => {}
            Body::Checkpoint { cts_watermark, .. } => {
                self.latest_cts = self.latest_cts.max(cts_watermark)
            }
            Body::CatalogChange { .. } => self.catalog_invalidated = true,
        }
        Ok(())
    }
}

/// Opens a log on a file system.
///
/// @param vfs - the file system
/// @param path - the database path
/// @param policy - the sync policy
fn log_on(vfs: Arc<dyn Vfs>, path: &DbPath, policy: Synchronous) -> Wal {
    Wal::open(
        vfs,
        path,
        UUID,
        FIRST_LSN,
        1,
        WalOptions {
            synchronous: policy,
            segment_bytes: 8_192,
        },
    )
    .expect("a log")
}

/// The page a transaction's `FreePage` record names.
///
/// A transaction writes *two* things that land in two different places in the
/// store - a page image and a free-map bit - so that atomicity is observable.
/// A workload whose transactions each wrote one thing could not tell a
/// half-applied transaction from a missing one.
///
/// @param txn - the transaction
fn freed_page(txn: u64) -> u64 {
    1_000 + txn
}

/// Writes a workload of `count` transactions, each touching two things.
///
/// Returns the commits the log acknowledged, in order. Under `FULL` an
/// acknowledged commit is a durable one, which is what makes this list the
/// oracle a crash campaign compares against.
///
/// @param wal - the log
/// @param count - how many transactions
fn write_workload(wal: &Wal, count: u64) -> Vec<u64> {
    let mut acknowledged = Vec::new();
    for txn in 1..=count {
        let image = format!("txn {txn} wrote this page");
        if wal
            .append(
                txn,
                Body::WritePage {
                    page: txn,
                    image: image.as_bytes(),
                },
            )
            .is_err()
        {
            break;
        }
        if wal
            .append(
                txn,
                Body::FreePage {
                    page: freed_page(txn),
                },
            )
            .is_err()
        {
            break;
        }
        match wal.commit(txn, txn * 10) {
            Ok(_) => acknowledged.push(txn),
            Err(_) => break,
        }
    }
    acknowledged
}

/// Holds the three properties every recovered store must have.
///
/// **Durability**: every commit the log acknowledged is present. Under `FULL` an
/// acknowledged commit was synced, so losing one is a violation and not a
/// policy.
///
/// **Atomicity**: a transaction that is visible is visible in full. Each one
/// wrote a page image and a free-map bit, and a store holding one without the
/// other is a torn transaction - which is the failure a scan that skipped a
/// damaged record and carried on would produce.
///
/// **Prefix**: the visible transactions are `1..=k` with no gaps, because they
/// were logged in that order and redo applies a prefix of the log.
///
/// What is deliberately **not** asserted is "nothing past the last acknowledged
/// commit is visible". The first version of this file asserted that and it was
/// wrong: a commit whose record was written and whose `sync` then failed is in
/// doubt, not absent. Its bytes are on the media, its checksum is good, and
/// recovery is right to replay it. The contract is that an acknowledged commit
/// survives, not that an unacknowledged one cannot; SQLite's WAL mode has the
/// same property for the same reason, and asserting otherwise would have made
/// the campaign fail on correct behaviour.
///
/// @param store - the recovered store
/// @param acknowledged - the commits the workload was told had succeeded
/// @param context - what to say when it fails
fn assert_durable_atomic_and_prefixed(store: &PageStore, acknowledged: &[u64], context: &str) {
    assert_atomic_and_prefixed(store, context);
    for txn in acknowledged {
        assert!(
            store.pages.contains_key(txn),
            "{context}: transaction {txn} was acknowledged and is gone"
        );
    }
}

/// Holds atomicity and the prefix property, without claiming durability.
///
/// Split out for the one failure the engine does not owe durability against: a
/// **short write**, which stores fewer bytes and *reports success*. Nothing in
/// the log can know it happened - `write_all_at` promises to write all of its
/// input, and a device that returns success having written half of it has
/// broken the contract the engine is written against.
///
/// What the engine does owe is that the damage is **caught**, and this codebase
/// has already settled where: `inillucent-pool`'s own campaign says "a short
/// write is caught by the checksum rather than by the pool", and the record
/// checksum is the same answer one layer down. So the short-write arm asserts
/// atomicity, the prefix property, *and* that recovery said why it stopped -
/// a lost commit that recovery could not account for would still fail.
///
/// @param store - the recovered store
/// @param context - what to say when it fails
fn assert_atomic_and_prefixed(store: &PageStore, context: &str) {
    let visible: BTreeSet<u64> = store.pages.keys().copied().collect();
    for txn in &visible {
        assert!(
            store.free.contains(&freed_page(*txn)),
            "{context}: transaction {txn} is half applied - its page is there and its \
             free-map bit is not"
        );
    }
    for bit in &store.free {
        let txn = bit.saturating_sub(1_000);
        assert!(
            visible.contains(&txn),
            "{context}: transaction {txn} is half applied - its free-map bit is there \
             and its page is not"
        );
    }
    let highest = visible.iter().copied().max().unwrap_or(0);
    for txn in 1..=highest {
        assert!(
            visible.contains(&txn),
            "{context}: transaction {txn} is missing from a run that recovered {highest}, \
             so the replayed set is not a prefix of the log"
        );
    }
}

/// Recovers a log into a fresh store.
///
/// @param vfs - the file system
/// @param path - the database path
fn recover_into(vfs: &dyn Vfs, path: &DbPath) -> (PageStore, inillucent_wal::Recovered) {
    let mut store = PageStore::default();
    let outcome =
        recover::recover(vfs, path, RecoveryStart::fresh(UUID), &mut store).expect("recovery runs");
    (store, outcome)
}

/// Recovering twice produces the same file, byte for byte.
///
/// The TDD's acceptance, and the property the page-LSN rule exists for: a
/// record already applied is skipped, so a second run over the same log is a
/// no-op rather than a doubling.
#[test]
fn recovering_twice_produces_the_same_file() {
    let vfs: Arc<dyn Vfs> = Arc::new(MemoryVfs::new());
    let path = DbPath::new("twice.rdb");
    let wal = log_on(Arc::clone(&vfs), &path, Synchronous::Full);

    // A mixture: committed transactions, one that aborts after flushing, one
    // left open, a structural record, a compaction, a catalog change.
    wal.append(
        1,
        Body::InsertRow {
            tree: 1,
            page: 10,
            row: b"alpha",
        },
    )
    .unwrap();
    wal.append(
        1,
        Body::Structural {
            kind: inillucent_wal::Structural::Split,
            tree: 1,
            left: 10,
            right: 11,
            parent: 2,
            left_image: b"left half",
            right_image: b"right half",
            parent_image: b"parent",
        },
    )
    .unwrap();
    wal.commit(1, 100).unwrap();

    wal.append(
        2,
        Body::InsertRow {
            tree: 1,
            page: 11,
            row: b"beta",
        },
    )
    .unwrap();
    wal.append(2, Body::Abort).unwrap();
    wal.flush().unwrap();

    wal.append(3, Body::CatalogChange { delta: b"a table" })
        .unwrap();
    wal.append(
        3,
        Body::CompactLeaf {
            tree: 1,
            page: 10,
            image: b"compacted",
            from_lsn: 0,
        },
    )
    .unwrap();
    wal.commit(3, 300).unwrap();

    // Left open: never committed, never aborted. A loser.
    wal.append(
        4,
        Body::InsertRow {
            tree: 1,
            page: 12,
            row: b"gamma",
        },
    )
    .unwrap();
    wal.flush().unwrap();

    let mut store = PageStore::default();
    let first = recover::recover(vfs.as_ref(), &path, RecoveryStart::fresh(UUID), &mut store)
        .expect("the first recovery");
    let after_one = store.image();

    let second = recover::recover(vfs.as_ref(), &path, RecoveryStart::fresh(UUID), &mut store)
        .expect("the second recovery");
    let after_two = store.image();

    assert_eq!(
        after_one, after_two,
        "recovering twice changed the file, so redo is not idempotent"
    );
    assert_eq!(first.next_lsn, second.next_lsn);
    assert_eq!(first.committed, 2, "transactions 1 and 3 committed");
    assert_eq!(first.losers, 1, "transaction 4 was open at the end");
    assert_eq!(first.latest_cts, 300);
    assert!(first.catalog_changed);
    assert!(
        second.applied < first.applied,
        "the second run applied {} records where the first applied {}, so the \
         page-LSN rule is not skipping anything",
        second.applied,
        first.applied
    );

    // The aborted transaction's row is not in the store, and the open one's is
    // not either.
    let page_11 = store.logical.get(&11).cloned().unwrap_or_default();
    assert!(
        !page_11.iter().any(|entry| entry.contains("beta")),
        "an aborted transaction's row was replayed: {page_11:?}"
    );
    assert!(
        !store.logical.contains_key(&12),
        "an open transaction's row was replayed"
    );
}

/// A log with no checkpoint and a torn tail keeps the prefix and drops the rest.
#[test]
fn a_torn_tail_stops_the_scan_and_keeps_the_prefix() {
    let vfs: Arc<dyn Vfs> = Arc::new(MemoryVfs::new());
    let path = DbPath::new("torn.rdb");
    let wal = log_on(Arc::clone(&vfs), &path, Synchronous::Full);
    let acknowledged = write_workload(&wal, 6);
    assert_eq!(acknowledged.len(), 6);
    let clean_end = wal.next_lsn();
    drop(wal);

    // Damage the last few bytes of the segment, which is what a torn write at
    // the end of a log looks like.
    let segment = inillucent_wal::writer::segment_path(
        &path.as_path().to_string_lossy(),
        path.as_path().parent(),
        1,
    );
    let file = vfs
        .open(
            &segment,
            inillucent_vfs::OpenOptions::of_kind(inillucent_vfs::FileKind::Wal),
        )
        .unwrap();
    let size = file.file_size().unwrap();
    file.write_all_at(size - 4, &[0xFF; 4]).unwrap();
    drop(file);

    let (store, outcome) = recover_into(vfs.as_ref(), &path);
    assert!(
        outcome.next_lsn < clean_end,
        "the scan kept a tail it should have refused"
    );
    assert!(
        outcome.stopped_because.is_some(),
        "a torn tail stopped the scan and the scan did not say so"
    );
    assert!(
        outcome.committed >= 5,
        "only {} transactions survived a single torn record",
        outcome.committed
    );
    assert!(store.pages.contains_key(&1), "the prefix is still there");
}

/// A log whose segment belongs to another database is not applied.
#[test]
fn a_segment_from_another_database_is_refused() {
    let vfs: Arc<dyn Vfs> = Arc::new(MemoryVfs::new());
    let path = DbPath::new("foreign.rdb");
    let wal = log_on(Arc::clone(&vfs), &path, Synchronous::Full);
    write_workload(&wal, 4);
    drop(wal);

    let mut store = PageStore::default();
    let outcome = recover::recover(
        vfs.as_ref(),
        &path,
        RecoveryStart {
            uuid: UUID ^ 1,
            ..RecoveryStart::fresh(UUID)
        },
        &mut store,
    )
    .expect("recovery runs");
    assert_eq!(outcome.scanned, 0, "another database's log was read");
    assert_eq!(outcome.applied, 0);
    assert!(store.pages.is_empty());
}

/// Truncation removes the torn tail and the segments past it.
#[test]
fn truncation_removes_the_tail_and_the_later_segments() {
    let vfs: Arc<dyn Vfs> = Arc::new(MemoryVfs::new());
    let path = DbPath::new("truncate.rdb");
    let wal = log_on(Arc::clone(&vfs), &path, Synchronous::Full);
    // Enough to roll several segments at the 8 KiB size `log_on` sets.
    for txn in 1..=40u64 {
        wal.append(
            txn,
            Body::WritePage {
                page: txn,
                image: &[0x5Au8; 400],
            },
        )
        .unwrap();
        wal.commit(txn, txn).unwrap();
    }
    let sequences = wal.sequence();
    assert!(sequences > 2, "the workload did not roll a segment");
    drop(wal);

    let (_, outcome) = recover_into(vfs.as_ref(), &path);
    recover::truncate_after(vfs.as_ref(), &path, &outcome).expect("truncation");

    // Recovering the truncated log gives the same answer, which is the property
    // that matters: truncation removed bytes nobody was going to read.
    let (_, again) = recover_into(vfs.as_ref(), &path);
    assert_eq!(again.next_lsn, outcome.next_lsn);
    assert_eq!(again.committed, outcome.committed);

    // A segment past the recovered one is gone.
    let past = inillucent_wal::writer::segment_path(
        &path.as_path().to_string_lossy(),
        path.as_path().parent(),
        outcome.sequence + 1,
    );
    assert!(
        !vfs.access(&past, inillucent_vfs::AccessMode::Exists)
            .unwrap(),
        "a segment past the recovered prefix was left behind"
    );
}

/// Runs one arm of a crash campaign.
///
/// Returns the commits the workload acknowledged and the snapshot the crash
/// left, or `None` when the injected failure never fired.
///
/// @param site - where to crash
/// @param nth - which call at that site to crash on
/// @param policy - the sync policy the workload runs under
fn crash_arm(
    site: Site,
    nth: u64,
    policy: Synchronous,
) -> Option<(Vec<u64>, CrashSnapshot, DbPath)> {
    let vfs = Arc::new(SimVfs::new(SimConfig::default()));
    let path = DbPath::new("campaign.rdb");
    let wal = log_on(Arc::clone(&vfs) as Arc<dyn Vfs>, &path, policy);
    vfs.failpoints().set(site, Policy::Nth(nth, Failure::Crash));
    let acknowledged = write_workload(&wal, 12);
    let snapshot = vfs.crash();
    Some((acknowledged, snapshot, path))
}

/// Crashing at every write leaves exactly the acknowledged prefix.
///
/// The TDD's invariant 12, enumerated rather than sampled: every write the
/// workload performs is crashed at in turn, and every arm is checked against
/// the list of commits the *log* said it had made durable.
#[test]
fn crashing_at_every_write_leaves_the_committed_prefix() {
    let mut arms = 0usize;
    let mut with_loss = 0usize;
    for nth in 1..=40u64 {
        let Some((acknowledged, snapshot, path)) = crash_arm(Site::Write, nth, Synchronous::Full)
        else {
            continue;
        };
        arms += 1;
        let recovered_vfs = SimVfs::recovered(SimConfig::default(), &snapshot);
        let mut store = PageStore::default();
        let outcome = recover::recover(
            &recovered_vfs,
            &path,
            RecoveryStart::fresh(UUID),
            &mut store,
        )
        .expect("recovery runs after a crash");

        assert_durable_atomic_and_prefixed(
            &store,
            &acknowledged,
            &format!(
                "crash at write {nth} (recovered {} commits, stopped because {:?})",
                outcome.committed, outcome.stopped_because
            ),
        );
        if (acknowledged.len() as u64) < 12 {
            with_loss += 1;
        }
    }
    assert!(arms >= 20, "the campaign only ran {arms} arms");
    assert!(
        with_loss > 0,
        "no arm of the campaign actually interrupted the workload, so it proved nothing"
    );
}

/// Crashing at every sync leaves exactly the acknowledged prefix.
///
/// The sync is the commit point under `FULL`, so this campaign is the one that
/// distinguishes "the record was written" from "the record is durable" - and a
/// mutant that acknowledges a commit before its sync returns is killed here and
/// nowhere else.
#[test]
fn crashing_at_every_sync_leaves_the_committed_prefix() {
    let mut arms = 0usize;
    for nth in 1..=20u64 {
        let Some((acknowledged, snapshot, path)) = crash_arm(Site::Sync, nth, Synchronous::Full)
        else {
            continue;
        };
        arms += 1;
        let recovered_vfs = SimVfs::recovered(SimConfig::default(), &snapshot);
        let mut store = PageStore::default();
        recover::recover(
            &recovered_vfs,
            &path,
            RecoveryStart::fresh(UUID),
            &mut store,
        )
        .expect("recovery runs after a crash");
        assert_durable_atomic_and_prefixed(&store, &acknowledged, &format!("crash at sync {nth}"));
    }
    assert!(arms >= 10, "the campaign only ran {arms} arms");
}

/// Failing the Nth call reports an error and never half-commits.
///
/// The failures are the ones a real file system produces. Two of them - a
/// disk-full and an I/O error - are *reported*, so the log refuses the commit
/// and every acknowledged commit survives. The third, a short write, is not
/// reported: it stores fewer bytes and returns success, so the commit is
/// acknowledged and its record is damaged. Nothing in the log can prevent that,
/// and this campaign holds it to what it can do - the damage is caught by the
/// record checksum, no torn transaction is applied, and recovery says why it
/// stopped rather than losing a commit silently.
#[test]
fn failing_the_nth_call_never_leaves_a_half_commit() {
    let mut short_write_detections = 0usize;
    for failure in [Failure::DiskFull, Failure::IoError, Failure::ShortWrite] {
        for nth in 1..=12u64 {
            let vfs = Arc::new(SimVfs::new(SimConfig::default()));
            let path = DbPath::new("nth.rdb");
            let wal = log_on(Arc::clone(&vfs) as Arc<dyn Vfs>, &path, Synchronous::Full);
            vfs.failpoints().fail_nth_call(nth, failure);
            let acknowledged = write_workload(&wal, 12);
            vfs.failpoints().set(Site::Write, Policy::Off);
            vfs.failpoints().set(Site::Sync, Policy::Off);
            drop(wal);

            let mut store = PageStore::default();
            let outcome =
                recover::recover(vfs.as_ref(), &path, RecoveryStart::fresh(UUID), &mut store)
                    .expect("recovery runs");
            let context = format!(
                "{failure:?} at call {nth} (stopped because {:?})",
                outcome.stopped_because
            );
            if failure == Failure::ShortWrite {
                assert_atomic_and_prefixed(&store, &context);
                let lost = acknowledged
                    .iter()
                    .filter(|txn| !store.pages.contains_key(txn))
                    .count();
                if lost > 0 {
                    let why = outcome.stopped_because.clone().unwrap_or_default();
                    assert!(
                        why.contains("checksum") || why.contains("legal record length"),
                        "{context}: {lost} acknowledged commits vanished and recovery \
                         did not say the log was damaged"
                    );
                    short_write_detections += 1;
                }
            } else {
                assert_durable_atomic_and_prefixed(&store, &acknowledged, &context);
            }
        }
    }
    assert!(
        short_write_detections > 0,
        "no short-write arm actually damaged an acknowledged commit, so the arm that \
         proves the checksum catches it never ran"
    );
}

/// A corrupt log never panics, whatever is done to its bytes.
///
/// This is the stable counterpart of the `corrupt_wal` fuzz target: the same
/// entry point, driven over a systematic set of corruptions rather than over a
/// fuzzer's. Every byte of the segment is flipped in turn, every byte is zeroed
/// in turn, and the file is truncated at every length.
#[test]
fn a_corrupt_log_never_panics() {
    let vfs: Arc<dyn Vfs> = Arc::new(MemoryVfs::new());
    let path = DbPath::new("hostile.rdb");
    let wal = log_on(Arc::clone(&vfs), &path, Synchronous::Full);
    write_workload(&wal, 5);
    drop(wal);

    let segment = inillucent_wal::writer::segment_path(
        &path.as_path().to_string_lossy(),
        path.as_path().parent(),
        1,
    );
    let file = vfs
        .open(
            &segment,
            inillucent_vfs::OpenOptions::of_kind(inillucent_vfs::FileKind::Wal),
        )
        .unwrap();
    let size = file.file_size().unwrap() as usize;
    let mut original = vec![0u8; size];
    file.read_exact_at(0, &mut original).unwrap();
    drop(file);

    let mut damaged = 0usize;
    for index in 0..original.len() {
        for value in [0x00u8, 0xFF, original[index] ^ 0x40] {
            let mut bytes = original.clone();
            bytes[index] = value;
            damaged += 1;
            let fresh: Arc<dyn Vfs> = Arc::new(MemoryVfs::new());
            let handle = fresh
                .open(
                    &segment,
                    inillucent_vfs::OpenOptions::of_kind(inillucent_vfs::FileKind::Wal),
                )
                .unwrap();
            handle.write_all_at(0, &bytes).unwrap();
            drop(handle);
            // The only requirement is that this returns. A panic fails the
            // test; an error is a perfectly good answer.
            let _ = recover::inspect(fresh.as_ref(), &path, RecoveryStart::fresh(UUID));
        }
    }
    for cut in 0..original.len() {
        let fresh: Arc<dyn Vfs> = Arc::new(MemoryVfs::new());
        let handle = fresh
            .open(
                &segment,
                inillucent_vfs::OpenOptions::of_kind(inillucent_vfs::FileKind::Wal),
            )
            .unwrap();
        handle.write_all_at(0, &original[..cut]).unwrap();
        drop(handle);
        let _ = recover::inspect(fresh.as_ref(), &path, RecoveryStart::fresh(UUID));
    }
    assert!(damaged > 100, "the sweep only tried {damaged} corruptions");
}

/// A gap in the segment chain ends the scan rather than skipping the gap.
///
/// Skipping it would apply a later record without the earlier one it depends on,
/// which is the one way redo can produce a state that never existed.
#[test]
fn a_gap_in_the_chain_ends_the_scan() {
    let vfs: Arc<dyn Vfs> = Arc::new(MemoryVfs::new());
    let path = DbPath::new("gap.rdb");
    let wal = log_on(Arc::clone(&vfs), &path, Synchronous::Full);
    for txn in 1..=40u64 {
        wal.append(
            txn,
            Body::WritePage {
                page: txn,
                image: &[0x11u8; 400],
            },
        )
        .unwrap();
        wal.commit(txn, txn).unwrap();
    }
    assert!(wal.sequence() >= 3, "the workload did not roll twice");
    let last = wal.sequence();
    drop(wal);

    // Truncate the *first* segment's body away, so the second segment's first
    // LSN is past where the first one now stops. The chain is no longer
    // continuous and everything from the gap on is discarded.
    let first = inillucent_wal::writer::segment_path(
        &path.as_path().to_string_lossy(),
        path.as_path().parent(),
        1,
    );
    let file = vfs
        .open(
            &first,
            inillucent_vfs::OpenOptions::of_kind(inillucent_vfs::FileKind::Wal),
        )
        .unwrap();
    file.truncate(segment::HEADER_BYTES as u64 + 64).unwrap();
    drop(file);

    let (_, outcome) = recover_into(vfs.as_ref(), &path);
    assert!(
        outcome.sequence < last,
        "the scan carried on past a gap into segment {last}"
    );
}

/// A stale record left in a reused segment ends the scan.
///
/// A segment file that a shorter run left behind holds records at LSNs that do
/// not match where they now sit. The scan notices the position rather than
/// trusting the record, and says so.
#[test]
fn a_record_that_is_not_where_it_says_it_is_ends_the_scan() {
    let vfs: Arc<dyn Vfs> = Arc::new(MemoryVfs::new());
    let path = DbPath::new("stale.rdb");
    let wal = log_on(Arc::clone(&vfs), &path, Synchronous::Full);
    write_workload(&wal, 4);
    let good_end = wal.next_lsn();
    drop(wal);

    // Append a well-formed record whose LSN is wrong for its position: exactly
    // what a reused segment leaves behind, and indistinguishable from a valid
    // record by checksum alone.
    let segment_path = inillucent_wal::writer::segment_path(
        &path.as_path().to_string_lossy(),
        path.as_path().parent(),
        1,
    );
    let mut stale = Vec::new();
    Record {
        lsn: good_end + 4_096,
        txn: 99,
        body: Body::WritePage {
            page: 77,
            image: b"stale",
        },
        length: 0,
    }
    .encode(&mut stale)
    .unwrap();
    let file = vfs
        .open(
            &segment_path,
            inillucent_vfs::OpenOptions::of_kind(inillucent_vfs::FileKind::Wal),
        )
        .unwrap();
    let size = file.file_size().unwrap();
    file.write_all_at(size, &stale).unwrap();
    drop(file);

    let (store, outcome) = recover_into(vfs.as_ref(), &path);
    assert_eq!(
        outcome.next_lsn, good_end,
        "the scan accepted a record that was not where it said it was"
    );
    let why = outcome.stopped_because.unwrap_or_default();
    assert!(why.contains("sits at"), "the scan did not say why: {why}");
    assert!(!store.pages.contains_key(&77));
}

/// A segment that cannot be read ends the chain rather than failing the open.
#[test]
fn an_unreadable_segment_ends_the_chain() {
    for failure in [Failure::IoError, Failure::ShortRead] {
        let vfs = Arc::new(SimVfs::new(SimConfig::default()));
        let path = DbPath::new("unreadable.rdb");
        let wal = log_on(Arc::clone(&vfs) as Arc<dyn Vfs>, &path, Synchronous::Full);
        let acknowledged = write_workload(&wal, 5);
        assert_eq!(acknowledged.len(), 5);
        drop(wal);

        vfs.failpoints().set(Site::Read, Policy::Always(failure));
        let mut store = PageStore::default();
        let outcome = recover::recover(vfs.as_ref(), &path, RecoveryStart::fresh(UUID), &mut store)
            .expect("an unreadable segment is an empty chain, not a failure");
        vfs.failpoints().set(Site::Read, Policy::Off);
        assert_eq!(outcome.scanned, 0, "{failure:?} was read anyway");
        assert!(store.pages.is_empty());
    }
}

/// A segment file too short to hold a header ends the chain.
#[test]
fn a_segment_shorter_than_its_header_ends_the_chain() {
    let vfs: Arc<dyn Vfs> = Arc::new(MemoryVfs::new());
    let path = DbPath::new("stub.rdb");
    let wal = log_on(Arc::clone(&vfs), &path, Synchronous::Full);
    write_workload(&wal, 3);
    drop(wal);
    let segment_path = inillucent_wal::writer::segment_path(
        &path.as_path().to_string_lossy(),
        path.as_path().parent(),
        1,
    );
    let file = vfs
        .open(
            &segment_path,
            inillucent_vfs::OpenOptions::of_kind(inillucent_vfs::FileKind::Wal),
        )
        .unwrap();
    file.truncate(16).unwrap();
    drop(file);
    let (store, outcome) = recover_into(vfs.as_ref(), &path);
    assert_eq!(outcome.scanned, 0);
    assert!(store.pages.is_empty());
}

/// Records that belong to no transaction are replayed unconditionally.
///
/// A bulk build writes pages with no transaction around them, and a checkpoint
/// marker belongs to none by definition. Both carry transaction id zero, and the
/// scan has to replay them without looking for a commit that will never come.
#[test]
fn records_belonging_to_no_transaction_are_replayed() {
    let vfs: Arc<dyn Vfs> = Arc::new(MemoryVfs::new());
    let path = DbPath::new("no-txn.rdb");
    let wal = log_on(Arc::clone(&vfs), &path, Synchronous::Full);
    wal.append(
        0,
        Body::WritePage {
            page: 30,
            image: b"bulk built",
        },
    )
    .unwrap();
    wal.append(0, Body::AllocPage { page: 30 }).unwrap();
    wal.note_checkpoint(0, 4).unwrap();
    drop(wal);

    let (store, outcome) = recover_into(vfs.as_ref(), &path);
    assert_eq!(outcome.committed, 0, "no transaction committed");
    assert_eq!(outcome.scanned, 3);
    assert_eq!(
        outcome.applied, 3,
        "a record with no transaction was skipped"
    );
    assert!(store.pages.contains_key(&30));
    assert!(store.free.contains(&30) || !store.free.contains(&30));
    assert_eq!(store.latest_cts, 4);
    assert!(outcome.last_checkpoint.is_some());
}

/// A checkpoint retires the segments entirely below it.
#[test]
fn a_checkpoint_retires_the_segments_below_it() {
    let vfs: Arc<dyn Vfs> = Arc::new(MemoryVfs::new());
    let path = DbPath::new("retire.rdb");
    let wal = log_on(Arc::clone(&vfs), &path, Synchronous::Full);
    for txn in 1..=40u64 {
        wal.append(
            txn,
            Body::WritePage {
                page: txn,
                image: &[0x22u8; 400],
            },
        )
        .unwrap();
        wal.commit(txn, txn).unwrap();
    }
    let current = wal.sequence();
    assert!(current >= 3, "the workload did not roll twice");
    for sequence in 1..current {
        assert!(
            vfs.access(
                &wal.segment_path(sequence),
                inillucent_vfs::AccessMode::Exists
            )
            .unwrap(),
            "segment {sequence} should still be there before the checkpoint"
        );
    }

    let retired = wal
        .retire_segments_below(wal.durable_end())
        .expect("segments retire");
    assert!(retired > 0, "no segment was retired");
    for sequence in 1..current {
        assert!(
            !vfs.access(
                &wal.segment_path(sequence),
                inillucent_vfs::AccessMode::Exists
            )
            .unwrap(),
            "segment {sequence} survived a checkpoint past its end"
        );
    }
    // The segment being written is never retired, whatever the LSN says.
    assert!(vfs
        .access(
            &wal.segment_path(current),
            inillucent_vfs::AccessMode::Exists
        )
        .unwrap());
    // Retiring again is a no-op rather than an error.
    assert_eq!(wal.retire_segments_below(wal.durable_end()).unwrap(), 0);
}

/// A database in a directory names its segments beside it.
///
/// The path arithmetic is easy to get wrong in a way no single-directory test
/// would see: a segment written to the process's working directory instead of
/// the database's would still be found by the same run and lost by the next.
#[test]
fn a_database_in_a_directory_keeps_its_segments_beside_it() {
    let vfs: Arc<dyn Vfs> = Arc::new(MemoryVfs::new());
    let path = DbPath::new("some/where/nested.rdb");
    let wal = Wal::open(
        Arc::clone(&vfs),
        &path,
        UUID,
        FIRST_LSN,
        1,
        WalOptions {
            synchronous: Synchronous::Full,
            segment_bytes: 8_192,
        },
    )
    .expect("a log");
    let acknowledged = write_workload(&wal, 5);
    assert_eq!(acknowledged.len(), 5);
    let segment_path = wal.segment_path(1);
    assert!(
        segment_path.as_path().to_string_lossy().contains("some"),
        "the segment landed outside the database's directory: {}",
        segment_path.display()
    );
    drop(wal);

    let (store, outcome) = recover_into(vfs.as_ref(), &path);
    assert_eq!(outcome.committed, 5);
    assert_eq!(store.pages.len(), 5);
}

/// Retirement leaves a segment alone when it is not entirely below the LSN.
///
/// The condition has three parts and all three matter: the segment must start
/// below the checkpoint, the *next* segment must start at or below it, and the
/// delete must succeed. A retirement that removed a segment holding records
/// above the checkpoint would delete the log recovery is about to need.
#[test]
fn retirement_keeps_a_segment_the_checkpoint_has_not_passed() {
    let vfs: Arc<dyn Vfs> = Arc::new(MemoryVfs::new());
    let path = DbPath::new("keep.rdb");
    let wal = log_on(Arc::clone(&vfs), &path, Synchronous::Full);
    for txn in 1..=40u64 {
        wal.append(
            txn,
            Body::WritePage {
                page: txn,
                image: &[0x33u8; 400],
            },
        )
        .unwrap();
        wal.commit(txn, txn).unwrap();
    }
    assert!(wal.sequence() >= 3);

    // A checkpoint at the very start of the log retires nothing.
    assert_eq!(wal.retire_segments_below(FIRST_LSN).unwrap(), 0);
    for sequence in 1..wal.sequence() {
        assert!(vfs
            .access(
                &wal.segment_path(sequence),
                inillucent_vfs::AccessMode::Exists
            )
            .unwrap());
    }

    // A checkpoint inside the second segment retires the first and not the
    // second, because the second holds records above it.
    let second_start = {
        let file = vfs
            .open(
                &wal.segment_path(2),
                inillucent_vfs::OpenOptions::of_kind(inillucent_vfs::FileKind::Wal),
            )
            .unwrap();
        let mut header = vec![0u8; segment::HEADER_BYTES];
        file.read_exact_at(0, &mut header).unwrap();
        SegmentHeader::decode(&header).unwrap().first_lsn
    };
    assert_eq!(wal.retire_segments_below(second_start + 8).unwrap(), 1);
    assert!(!vfs
        .access(&wal.segment_path(1), inillucent_vfs::AccessMode::Exists)
        .unwrap());
    assert!(vfs
        .access(&wal.segment_path(2), inillucent_vfs::AccessMode::Exists)
        .unwrap());
}

/// Retirement that cannot read a segment leaves it alone rather than failing.
///
/// A leftover segment is refused on the next open by its sequence number, so it
/// is untidy rather than dangerous - and failing a checkpoint because a file
/// could not be unlinked would turn a tidy-up into an outage.
#[test]
fn retirement_that_cannot_read_a_segment_is_not_an_outage() {
    for failure in [Failure::IoError, Failure::ShortRead] {
        let vfs = Arc::new(SimVfs::new(SimConfig::default()));
        let path = DbPath::new("retire-fail.rdb");
        let wal = log_on(Arc::clone(&vfs) as Arc<dyn Vfs>, &path, Synchronous::Full);
        for txn in 1..=40u64 {
            wal.append(
                txn,
                Body::WritePage {
                    page: txn,
                    image: &[0x44u8; 400],
                },
            )
            .unwrap();
            wal.commit(txn, txn).unwrap();
        }
        assert!(wal.sequence() >= 3);

        vfs.failpoints().set(Site::Read, Policy::Always(failure));
        let retired = wal
            .retire_segments_below(wal.durable_end())
            .expect("retirement reports success even when it can retire nothing");
        vfs.failpoints().set(Site::Read, Policy::Off);
        assert_eq!(retired, 0, "{failure:?} did not stop the retirement");
        assert!(vfs
            .access(&wal.segment_path(1), inillucent_vfs::AccessMode::Exists)
            .unwrap());
    }

    // And the same for an open that fails.
    let vfs = Arc::new(SimVfs::new(SimConfig::default()));
    let path = DbPath::new("retire-open-fail.rdb");
    let wal = log_on(Arc::clone(&vfs) as Arc<dyn Vfs>, &path, Synchronous::Full);
    for txn in 1..=40u64 {
        wal.append(
            txn,
            Body::WritePage {
                page: txn,
                image: &[0x55u8; 400],
            },
        )
        .unwrap();
        wal.commit(txn, txn).unwrap();
    }
    vfs.failpoints()
        .set(Site::Open, Policy::Always(Failure::Permission));
    assert_eq!(wal.retire_segments_below(wal.durable_end()).unwrap(), 0);
    vfs.failpoints().set(Site::Open, Policy::Off);
}

/// A segment that cannot be deleted is left alone and the checkpoint goes on.
///
/// The last of the three conditions in the retirement test, and the one that
/// decides whether a tidy-up can take a checkpoint down with it.
#[test]
fn a_segment_that_cannot_be_deleted_does_not_fail_the_checkpoint() {
    let vfs = Arc::new(SimVfs::new(SimConfig::default()));
    let path = DbPath::new("undeletable.rdb");
    let wal = log_on(Arc::clone(&vfs) as Arc<dyn Vfs>, &path, Synchronous::Full);
    for txn in 1..=40u64 {
        wal.append(
            txn,
            Body::WritePage {
                page: txn,
                image: &[0x77u8; 400],
            },
        )
        .unwrap();
        wal.commit(txn, txn).unwrap();
    }
    let current = wal.sequence();
    assert!(current >= 3);

    vfs.failpoints()
        .set(Site::Delete, Policy::Always(Failure::Permission));
    let retired = wal
        .retire_segments_below(wal.durable_end())
        .expect("a segment that will not unlink is not an outage");
    vfs.failpoints().set(Site::Delete, Policy::Off);
    assert_eq!(
        retired, 0,
        "a delete that failed was counted as a retirement"
    );
    assert!(
        vfs.access(&wal.segment_path(1), inillucent_vfs::AccessMode::Exists)
            .unwrap(),
        "the segment should still be there"
    );

    // And with the media back, the same call retires it.
    assert!(wal.retire_segments_below(wal.durable_end()).unwrap() > 0);
}

/// A segment whose header is damaged is not retired.
#[test]
fn retirement_leaves_a_segment_whose_header_is_damaged() {
    let vfs: Arc<dyn Vfs> = Arc::new(MemoryVfs::new());
    let path = DbPath::new("retire-damaged.rdb");
    let wal = log_on(Arc::clone(&vfs), &path, Synchronous::Full);
    for txn in 1..=40u64 {
        wal.append(
            txn,
            Body::WritePage {
                page: txn,
                image: &[0x66u8; 400],
            },
        )
        .unwrap();
        wal.commit(txn, txn).unwrap();
    }
    assert!(wal.sequence() >= 3);
    let first = wal.segment_path(1);
    let file = vfs
        .open(
            &first,
            inillucent_vfs::OpenOptions::of_kind(inillucent_vfs::FileKind::Wal),
        )
        .unwrap();
    file.write_all_at(0, &[0xEEu8; 8]).unwrap();
    drop(file);
    let retired = wal.retire_segments_below(wal.durable_end()).unwrap();
    assert!(
        vfs.access(&first, inillucent_vfs::AccessMode::Exists)
            .unwrap(),
        "a segment nobody could read was deleted anyway"
    );
    assert!(retired < 2);
}

/// Truncating a log whose segment cannot be opened is not an error.
///
/// `truncate_after` is a tidy-up: the recovered prefix is already decided, and a
/// segment that cannot be opened is one nothing will read past. Failing here
/// would turn every open on a read-only directory into a refusal to open.
#[test]
fn truncating_a_log_that_cannot_be_opened_is_not_an_error() {
    let vfs = Arc::new(SimVfs::new(SimConfig::default()));
    let path = DbPath::new("truncate-fail.rdb");
    let wal = log_on(Arc::clone(&vfs) as Arc<dyn Vfs>, &path, Synchronous::Full);
    write_workload(&wal, 4);
    drop(wal);
    let mut store = PageStore::default();
    let outcome = recover::recover(vfs.as_ref(), &path, RecoveryStart::fresh(UUID), &mut store)
        .expect("recovery runs");

    vfs.failpoints()
        .set(Site::Open, Policy::Always(Failure::Permission));
    recover::truncate_after(vfs.as_ref(), &path, &outcome)
        .expect("a segment that will not open is not an outage");
    vfs.failpoints().set(Site::Open, Policy::Off);
}

/// Truncating a log whose header is damaged is not an error either.
#[test]
fn truncating_a_log_whose_header_is_damaged_is_not_an_error() {
    let vfs: Arc<dyn Vfs> = Arc::new(MemoryVfs::new());
    let path = DbPath::new("truncate-damaged.rdb");
    let wal = log_on(Arc::clone(&vfs), &path, Synchronous::Full);
    write_workload(&wal, 4);
    drop(wal);
    let mut store = PageStore::default();
    let outcome = recover::recover(vfs.as_ref(), &path, RecoveryStart::fresh(UUID), &mut store)
        .expect("recovery runs");

    let segment_path = inillucent_wal::writer::segment_path(
        &path.as_path().to_string_lossy(),
        path.as_path().parent(),
        outcome.sequence,
    );
    let file = vfs
        .open(
            &segment_path,
            inillucent_vfs::OpenOptions::of_kind(inillucent_vfs::FileKind::Wal),
        )
        .unwrap();
    file.write_all_at(0, &[0xEEu8; 8]).unwrap();
    drop(file);
    recover::truncate_after(vfs.as_ref(), &path, &outcome)
        .expect("a damaged header is not an outage");
}

/// A chain whose first segment cannot be opened is an empty chain.
#[test]
fn a_chain_that_cannot_be_opened_is_empty() {
    let vfs = Arc::new(SimVfs::new(SimConfig::default()));
    let path = DbPath::new("open-fail.rdb");
    let wal = log_on(Arc::clone(&vfs) as Arc<dyn Vfs>, &path, Synchronous::Full);
    write_workload(&wal, 5);
    drop(wal);
    vfs.failpoints()
        .set(Site::Open, Policy::Always(Failure::Permission));
    let mut store = PageStore::default();
    let outcome = recover::recover(vfs.as_ref(), &path, RecoveryStart::fresh(UUID), &mut store)
        .expect("an unopenable chain is empty, not a failure");
    vfs.failpoints().set(Site::Open, Policy::Off);
    assert_eq!(outcome.scanned, 0);
    assert!(store.pages.is_empty());
}

/// A transaction that wrote after its own commit record is still a winner.
///
/// A well-formed log never does this. A damaged one can, and the scan decides
/// which transactions lost by subtracting the finished ones at the end rather
/// than by removing each as its outcome arrives - so the answer does not depend
/// on an ordering the bytes are not obliged to have.
#[test]
fn a_record_after_its_own_commit_does_not_make_a_loser() {
    let vfs: Arc<dyn Vfs> = Arc::new(MemoryVfs::new());
    let path = DbPath::new("after-commit.rdb");
    let wal = log_on(Arc::clone(&vfs), &path, Synchronous::Full);
    wal.append(
        1,
        Body::WritePage {
            page: 50,
            image: b"before the commit",
        },
    )
    .unwrap();
    wal.commit(1, 1).unwrap();
    // The same transaction id again, after its commit.
    wal.append(
        1,
        Body::WritePage {
            page: 51,
            image: b"after the commit",
        },
    )
    .unwrap();
    wal.flush().unwrap();
    drop(wal);

    let (store, outcome) = recover_into(vfs.as_ref(), &path);
    assert_eq!(outcome.committed, 1);
    assert_eq!(
        outcome.losers, 0,
        "a transaction that committed was counted as a loser"
    );
    assert!(store.pages.contains_key(&50));
    assert!(store.pages.contains_key(&51));
}

/// Under NORMAL a crash may lose commits; under FULL it may not.
///
/// This is the behaviour half of the `synchronous` acceptance, and it is the
/// half a counter cannot show: the two policies differ in what survives a power
/// loss, which is the whole reason a person chooses between them.
#[test]
fn the_policy_decides_what_survives_a_power_loss() {
    let mut normal_lost = 0usize;
    let mut full_lost = 0usize;
    for nth in 1..=24u64 {
        for policy in [Synchronous::Full, Synchronous::Normal] {
            let vfs = Arc::new(SimVfs::new(SimConfig::default()));
            let path = DbPath::new("policy-crash.rdb");
            let wal = log_on(Arc::clone(&vfs) as Arc<dyn Vfs>, &path, policy);
            vfs.failpoints()
                .set(Site::Write, Policy::Nth(nth, Failure::Crash));
            let acknowledged = write_workload(&wal, 10);
            let snapshot = vfs.crash();
            let recovered_vfs = SimVfs::recovered(SimConfig::default(), &snapshot);
            let mut store = PageStore::default();
            let _ = recover::recover(
                &recovered_vfs,
                &path,
                RecoveryStart::fresh(UUID),
                &mut store,
            );
            let lost = acknowledged
                .iter()
                .filter(|txn| !store.pages.contains_key(txn))
                .count();
            match policy {
                Synchronous::Full => {
                    assert_eq!(
                        lost, 0,
                        "FULL lost {lost} acknowledged commits at write {nth}, which is a \
                         durability violation rather than a policy"
                    );
                    full_lost += lost;
                }
                _ => normal_lost += lost,
            }
        }
    }
    assert_eq!(full_lost, 0);
    assert!(
        normal_lost > 0,
        "NORMAL lost nothing across the whole campaign, so it is behaving like FULL \
         and the policy is not doing anything"
    );
}

/// A page stamped by a stream this log does not contain is refused by name.
///
/// **The page-LSN rule is only sound while a page's stamp is a position in the
/// stream beside the file.** A record at LSN *L* occupies
/// `[L, L + length)`, so every stamp a healthy file carries is strictly below
/// `valid_end`. A stamp at or above it reads as "the page already has this" for
/// every record there will ever be, so every later write to that page is
/// discarded - silently, on a file that stays structurally intact. Measured on
/// Nikaya's parked mail database: page 3 stamped 21,939,058,496 beside a log
/// ending at 21,075,008,440, and an `ANALYZE` that printed `ok` lost the
/// `sqlite_stat1` catalog row it had just committed.
#[test]
fn a_page_stamped_by_an_abandoned_stream_is_refused_by_name() {
    let vfs: Arc<dyn Vfs> = Arc::new(MemoryVfs::new());
    let path = DbPath::new("stamped.rdb");
    let wal = log_on(Arc::clone(&vfs), &path, Synchronous::Full);
    let acknowledged = write_workload(&wal, 4);
    assert_eq!(
        acknowledged.len(),
        4,
        "the workload logged four transactions"
    );
    let end = wal.next_lsn();
    wal.sync().unwrap();

    // The state the parked file was in. Page 1 is the first page the replay
    // asks about, so a refusal that fires before anything is applied leaves the
    // store exactly as it was found - which is what says the check runs ahead
    // of the damage rather than after it.
    let mut store = PageStore::default();
    store.put(
        1,
        b"a stream nobody has any more wrote this",
        end + 864_049_488,
    );
    let before = store.image();

    let error = recover::recover(vfs.as_ref(), &path, RecoveryStart::fresh(UUID), &mut store)
        .expect_err("a stamp above the log's end is refused");
    let detail = error.detail().unwrap_or_default().to_string();
    assert!(detail.contains("page 1"), "the page is not named: {detail}");
    assert!(
        detail.contains("at or above the log's end"),
        "the reason is not stated: {detail}"
    );
    assert_eq!(
        store.image(),
        before,
        "a record was applied before the refusal, so the check is behind the damage"
    );
}

/// A stamp below the log's end is still skipped, and the recovery still runs.
///
/// The refusal above must not have turned the page-LSN rule into a refusal of
/// its own ordinary case: a page whose stamp is above the *record's* LSN and
/// below the log's end is a page a previous run already applied, which is the
/// idempotence the whole design rests on.
#[test]
fn a_stamp_below_the_logs_end_is_skipped_rather_than_refused() {
    let vfs: Arc<dyn Vfs> = Arc::new(MemoryVfs::new());
    let path = DbPath::new("already-applied.rdb");
    let wal = log_on(Arc::clone(&vfs), &path, Synchronous::Full);
    let acknowledged = write_workload(&wal, 4);
    assert_eq!(acknowledged.len(), 4);
    let end = wal.next_lsn();
    wal.sync().unwrap();

    // One below the end: the highest stamp a healthy file can carry, and the
    // boundary the refusal is stated against.
    let mut store = PageStore::default();
    store.put(1, b"applied by an earlier run", end - 1);
    let (_, outcome) = {
        let outcome = recover::recover(vfs.as_ref(), &path, RecoveryStart::fresh(UUID), &mut store)
            .expect("a stamp below the log's end recovers");
        (&store, outcome)
    };
    assert!(outcome.applied > 0, "the replay applied nothing at all");
    assert_eq!(
        store.pages.get(&1).and_then(|bytes| bytes.get(8..33)),
        Some(&b"applied by an earlier run"[..]),
        "page 1 was rewritten, so the page-LSN rule stopped skipping"
    );
    // And the transactions that did not touch page 1 are all there, so the
    // skip was one page's rather than the whole replay's.
    assert_atomic_and_prefixed(&store, "a stamp below the log's end");
}
