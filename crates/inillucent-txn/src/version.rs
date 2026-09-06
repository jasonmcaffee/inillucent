//! Snapshots, the version log, and the garbage collection that bounds it.
//!
//! Invariant: **a version-log entry is never discarded while any snapshot older
//! than its `cts` is active.** That is the TDD's eighth. That is the property that
//! makes a reader's answer stable, and the reason it is stated as an invariant
//! rather than as a policy is that violating it produces no error - it produces
//! a reader that silently sees a row change under it, which no assertion about
//! the log's *size* would ever notice.
//!
//! ## What the log holds
//!
//! Pages hold the newest version of every row. The version log holds
//! **before-images**: for each key that has changed, the bytes it held before
//! each change, tagged with the `cts` of the transaction that made the change.
//! A reader with snapshot `s` that lands on a leaf whose `max_cts` is above `s`
//! consults the log for that key, takes the *earliest* entry with `cts > s`, and
//! reads that entry's before-image instead of the page's bytes.
//!
//! "Earliest above `s`" is the whole of the visibility rule and it is worth
//! being precise about: the entry tagged `cts` records what the row held
//! *before* the transaction that committed at `cts`. A reader at `s` wants what
//! the row held as of `s`, which is what it held before the first change that
//! happened after `s`.
//!
//! ## Why the cost is bounded by what a reader holds open, not by how long
//!
//! The TDD's phrasing is that "a long reader costs memory bounded by what it is
//! holding open rather than by how long it has been open". Both halves are
//! delivered here: the log is keyed by `(tree, key)` so an untouched row costs
//! nothing however long a reader runs, and [`VersionLog::collect`] drops every
//! entry whose `cts` is at or below the oldest active snapshot, so a reader that
//! finishes releases everything only it was keeping.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use inillucent_base::error::misuse;
use inillucent_base::DbResult;

/// A commit timestamp: the order commits become visible in.
pub type Cts = u64;

/// The commit timestamp a database has before anything has committed.
pub const FIRST_CTS: Cts = 0;

/// One transaction's identity.
///
/// Distinct from a `Cts`: a transaction has an id from the moment it starts and
/// a `cts` only when it commits, and the two orders are not the same - a
/// transaction that started first may commit second.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct TxnId(pub u64);

/// A read snapshot: everything committed at or below `cts` is visible.
#[derive(Clone, Debug)]
pub struct Snapshot {
    cts: Cts,
    id: u64,
    registry: Arc<Registry>,
}

impl Snapshot {
    /// Returns the commit timestamp this snapshot reads at.
    pub fn cts(&self) -> Cts {
        self.cts
    }

    /// Returns the snapshot's registry identity, for a report.
    pub fn id(&self) -> u64 {
        self.id
    }

    /// Reports whether a change committed at `cts` is visible here.
    ///
    /// @param cts - the change's commit timestamp
    pub fn sees(&self, cts: Cts) -> bool {
        cts <= self.cts
    }
}

impl Drop for Snapshot {
    /// Releases the snapshot, which is what lets the version log collect.
    ///
    /// Dropping is the release, rather than an explicit `close`, because a
    /// snapshot that is released by a call is a snapshot that stops being
    /// released the first time a caller returns early. The whole reason the
    /// version log's size is bounded is that this cannot be forgotten.
    fn drop(&mut self) {
        self.registry.release(self.id);
    }
}

/// The active snapshots, so the oldest one can be found.
#[derive(Debug, Default)]
struct Registry {
    active: Mutex<BTreeMap<u64, Cts>>,
    next: AtomicU64,
}

impl Registry {
    /// Locks the table, taking it back from a panicking thread if it has to.
    ///
    /// A poisoned lock means some thread panicked while holding it. The table is
    /// a `BTreeMap` of snapshot ids, so there is no invariant a panic could have
    /// left half-established and nothing to be gained by refusing - and dropping
    /// the entry instead, which is what an `if let Ok(..)` here would do, would
    /// silently *leak a snapshot* and stop the version log from ever collecting.
    ///
    /// It is written as a recovery rather than as a condition for the coverage
    /// reason as well: an `if let Ok` whose `Err` arm no test can take is a
    /// branch the gate can only be lied to about, and this is the same shape
    /// `WriterSlot` and `Wal` already use.
    fn table(&self) -> std::sync::MutexGuard<'_, BTreeMap<u64, Cts>> {
        match self.active.lock() {
            Ok(active) => active,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    /// Registers a snapshot and returns its id.
    ///
    /// @param cts - the timestamp it reads at
    fn take(&self, cts: Cts) -> u64 {
        let id = self.next.fetch_add(1, Ordering::SeqCst);
        self.table().insert(id, cts);
        id
    }

    /// Removes a snapshot.
    ///
    /// @param id - the snapshot's id
    fn release(&self, id: u64) {
        self.table().remove(&id);
    }

    /// Returns the oldest active snapshot's timestamp.
    ///
    /// @param latest - the newest committed timestamp, used when nothing is
    ///   active
    fn oldest(&self, latest: Cts) -> Cts {
        self.table().values().copied().min().unwrap_or(latest)
    }

    /// Returns how many snapshots are active.
    fn count(&self) -> usize {
        self.table().len()
    }

    /// Returns every active snapshot's timestamp.
    fn timestamps(&self) -> Vec<Cts> {
        self.table().values().copied().collect()
    }
}

/// The clock that hands out commit timestamps, and the snapshot registry.
#[derive(Debug)]
pub struct Clock {
    latest: AtomicU64,
    registry: Arc<Registry>,
}

impl Clock {
    /// Returns a clock starting at `latest`.
    ///
    /// @param latest - the newest committed timestamp, from recovery
    pub fn new(latest: Cts) -> Clock {
        Clock {
            latest: AtomicU64::new(latest),
            registry: Arc::new(Registry::default()),
        }
    }

    /// Returns the newest committed timestamp.
    pub fn latest(&self) -> Cts {
        self.latest.load(Ordering::SeqCst)
    }

    /// Takes a snapshot at the newest committed timestamp.
    pub fn snapshot(&self) -> Snapshot {
        let cts = self.latest();
        let id = self.registry.take(cts);
        Snapshot {
            cts,
            id,
            registry: Arc::clone(&self.registry),
        }
    }

    /// Assigns the next commit timestamp and publishes it.
    ///
    /// Called under the commit gate, so commit order equals visibility order
    /// equals log order. Assigning it anywhere else would let two commits
    /// become visible in an order their log records do not agree with, and
    /// recovery replays log order.
    pub fn commit(&self) -> Cts {
        self.latest.fetch_add(1, Ordering::SeqCst).saturating_add(1)
    }

    /// Returns the timestamp below which no reader can see anything.
    pub fn oldest_snapshot(&self) -> Cts {
        self.registry.oldest(self.latest())
    }

    /// Returns every active snapshot's timestamp.
    ///
    /// The version log collects against all of them rather than against the
    /// oldest, because an image is reachable only when some snapshot's search
    /// lands on it - see [`VersionLog::collect`].
    pub fn active_timestamps(&self) -> Vec<Cts> {
        self.registry.timestamps()
    }

    /// Returns how many snapshots are open.
    pub fn active_snapshots(&self) -> usize {
        self.registry.count()
    }
}

/// One before-image: what a key held before a change.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BeforeImage {
    /// The commit timestamp of the change this is the image from *before*.
    pub cts: Cts,
    /// The row's bytes, or `None` when the row did not exist.
    pub bytes: Option<Vec<u8>>,
}

/// What a reader should do with a key, given its snapshot.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Visible<'v> {
    /// Read the page: no committed change since the snapshot touched this key.
    Page,
    /// Read these bytes instead of the page's.
    Instead(&'v [u8]),
    /// The row did not exist at the snapshot; hide it.
    Absent,
}

/// The before-images of every key changed since the oldest active snapshot.
#[derive(Debug, Default)]
pub struct VersionLog {
    /// Keyed by tree and key; the values are ordered by `cts` ascending.
    entries: BTreeMap<(u64, Vec<u8>), Vec<BeforeImage>>,
    /// How many images are held, so the size is reportable without a walk.
    held: usize,
    /// How many images have been discarded, for the report.
    collected: u64,
}

impl VersionLog {
    /// Returns an empty log.
    pub fn new() -> VersionLog {
        VersionLog::default()
    }

    /// Returns how many before-images are held.
    pub fn len(&self) -> usize {
        self.held
    }

    /// Reports whether the log is empty.
    pub fn is_empty(&self) -> bool {
        self.held == 0
    }

    /// Returns how many images have been collected since the log was created.
    pub fn collected(&self) -> u64 {
        self.collected
    }

    /// Returns how many distinct keys the log holds an image for.
    pub fn keys(&self) -> usize {
        self.entries.len()
    }

    /// Publishes one transaction's undo buffer at its commit timestamp.
    ///
    /// The images arrive in the order they were made, and the log wants them in
    /// `cts` order per key. Within one transaction every image carries the same
    /// `cts`, and a key touched twice by one transaction has only its *first*
    /// before-image published - which is the image a reader outside the
    /// transaction needs, because the intermediate value was never visible to
    /// anyone.
    ///
    /// @param cts - the transaction's commit timestamp
    /// @param images - the undo buffer, oldest first
    pub fn publish(
        &mut self,
        cts: Cts,
        images: impl IntoIterator<Item = (u64, Vec<u8>, Option<Vec<u8>>)>,
    ) {
        for (tree, key, bytes) in images {
            let slot = self.entries.entry((tree, key)).or_default();
            if slot.last().is_some_and(|last| last.cts == cts) {
                // The same transaction touched this key again. The image
                // already recorded is the one from before the transaction
                // started, which is what a reader outside it must see.
                continue;
            }
            slot.push(BeforeImage { cts, bytes });
            self.held = self.held.saturating_add(1);
        }
    }

    /// Says what a reader at `snapshot` should do with one key.
    ///
    /// @param tree - the tree the key is in
    /// @param key - the key's encoded bytes
    /// @param snapshot - the reader's timestamp
    pub fn visible(&self, tree: u64, key: &[u8], snapshot: Cts) -> Visible<'_> {
        let Some(images) = self.entries.get(&(tree, key.to_vec())) else {
            return Visible::Page;
        };
        // The earliest image above the snapshot: what the row held before the
        // first change the reader must not see.
        let Some(image) = images.iter().find(|image| image.cts > snapshot) else {
            return Visible::Page;
        };
        match &image.bytes {
            Some(bytes) => Visible::Instead(bytes),
            None => Visible::Absent,
        }
    }

    /// Discards every image no active snapshot can reach.
    ///
    /// **Against every active snapshot, not just the oldest one**, and the
    /// difference is the acceptance rather than a refinement. A snapshot at `s`
    /// reads, for each key, exactly the *earliest* image above `s` - so an image
    /// is reachable only when some active snapshot's search lands on it. Every
    /// other image is dead however new it is.
    ///
    /// Collecting against the oldest snapshot alone is correct and keeps far too
    /// much: a reader open across two hundred commits to twenty rows would hold
    /// two hundred images where it can reach twenty, because only the first
    /// image of each key is the one its search finds. That is memory growing
    /// with *how long the reader has been open*, which is the exact thing the
    /// TDD says must not happen - "a long reader costs memory bounded by what it
    /// is holding open rather than by how long it has been open". The
    /// conservative rule was written first and `a_long_reader_costs_what_it_holds_open`
    /// failed on it at 200 images against 20.
    ///
    /// A snapshot taken *later* never needs an image at all: it reads at the
    /// newest timestamp, so no image is above it and [`VersionLog::visible`]
    /// sends it to the page. That is why the live set is a function of the
    /// snapshots open right now and of nothing else.
    ///
    /// Returns how many images were discarded.
    ///
    /// @param snapshots - every active snapshot's timestamp, in any order
    pub fn collect(&mut self, snapshots: &[Cts]) -> usize {
        let mut sorted: Vec<Cts> = snapshots.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        let mut dropped = 0usize;
        let mut reachable: Vec<bool> = Vec::new();
        self.entries.retain(|_, images| {
            let before = images.len();
            reachable.clear();
            reachable.resize(before, false);
            for snapshot in &sorted {
                // The image this snapshot's search lands on: the first one
                // above it. `partition_point` is the same comparison
                // `visible` makes, which is why the two cannot disagree about
                // what is reachable.
                let at = images.partition_point(|image| image.cts <= *snapshot);
                if let Some(slot) = reachable.get_mut(at) {
                    *slot = true;
                }
            }
            let mut index = 0usize;
            images.retain(|_| {
                let keep = reachable.get(index).copied().unwrap_or(false);
                index = index.saturating_add(1);
                keep
            });
            dropped = dropped.saturating_add(before.saturating_sub(images.len()));
            !images.is_empty()
        });
        self.held = self.held.saturating_sub(dropped);
        self.collected = self.collected.saturating_add(dropped as u64);
        dropped
    }

    /// Removes every image a rolled-back transaction published.
    ///
    /// A rollback happens before a `cts` is assigned, so ordinarily nothing is
    /// published at all. This exists for the one case where it is: a commit that
    /// published its images and then failed to make its log record durable, and
    /// therefore has to be taken back out.
    ///
    /// @param cts - the timestamp to withdraw
    pub fn withdraw(&mut self, cts: Cts) -> DbResult<usize> {
        if cts == FIRST_CTS {
            return Err(misuse("nothing was ever published at the first timestamp"));
        }
        let mut dropped = 0usize;
        self.entries.retain(|_, images| {
            let before = images.len();
            images.retain(|image| image.cts != cts);
            dropped = dropped.saturating_add(before.saturating_sub(images.len()));
            !images.is_empty()
        });
        self.held = self.held.saturating_sub(dropped);
        Ok(dropped)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A snapshot reads at the newest committed timestamp and holds it.
    #[test]
    fn a_snapshot_holds_the_timestamp_it_was_taken_at() {
        let clock = Clock::new(FIRST_CTS);
        assert_eq!(clock.latest(), 0);
        let first = clock.snapshot();
        assert_eq!(first.cts(), 0);
        assert_eq!(clock.commit(), 1);
        assert_eq!(clock.commit(), 2);
        let second = clock.snapshot();
        assert_eq!(second.cts(), 2);
        // The first snapshot did not move when the clock did, which is the
        // whole of "a reader never sees a row change under it".
        assert_eq!(first.cts(), 0);
        assert!(!first.sees(1));
        assert!(second.sees(1));
        assert!(second.sees(2));
        assert!(!second.sees(3));
        assert_eq!(clock.active_snapshots(), 2);
    }

    /// Dropping a snapshot releases it, and the oldest moves.
    #[test]
    fn the_oldest_snapshot_moves_when_one_is_dropped() {
        let clock = Clock::new(FIRST_CTS);
        let early = clock.snapshot();
        clock.commit();
        clock.commit();
        let late = clock.snapshot();
        assert_eq!(clock.oldest_snapshot(), 0);
        drop(early);
        assert_eq!(clock.oldest_snapshot(), 2);
        assert_eq!(late.cts(), 2);
        drop(late);
        // With nothing active the oldest is the newest commit, so everything is
        // collectable.
        assert_eq!(clock.oldest_snapshot(), 2);
        assert_eq!(clock.active_snapshots(), 0);
    }

    /// A reader sees the before-image of the earliest change above its snapshot.
    #[test]
    fn a_reader_sees_the_image_from_before_the_first_change_it_must_not_see() {
        let mut log = VersionLog::new();
        // The key held "one" before the change at cts 5, and "two" before the
        // change at cts 9. The page now holds "three".
        log.publish(5, [(1u64, b"k".to_vec(), Some(b"one".to_vec()))]);
        log.publish(9, [(1u64, b"k".to_vec(), Some(b"two".to_vec()))]);

        assert_eq!(log.visible(1, b"k", 0), Visible::Instead(b"one"));
        assert_eq!(log.visible(1, b"k", 4), Visible::Instead(b"one"));
        assert_eq!(log.visible(1, b"k", 5), Visible::Instead(b"two"));
        assert_eq!(log.visible(1, b"k", 8), Visible::Instead(b"two"));
        // At 9 and above there is no change the reader must not see, so the
        // page is right.
        assert_eq!(log.visible(1, b"k", 9), Visible::Page);
        assert_eq!(log.visible(1, b"k", 100), Visible::Page);
        // A key nobody touched costs nothing and reads the page.
        assert_eq!(log.visible(1, b"other", 0), Visible::Page);
        // A key in another tree is a different key.
        assert_eq!(log.visible(2, b"k", 0), Visible::Page);
    }

    /// A row inserted after a snapshot is hidden from it.
    #[test]
    fn a_row_inserted_after_a_snapshot_is_hidden() {
        let mut log = VersionLog::new();
        log.publish(7, [(1u64, b"new".to_vec(), None)]);
        assert_eq!(log.visible(1, b"new", 6), Visible::Absent);
        assert_eq!(log.visible(1, b"new", 7), Visible::Page);
    }

    /// A key touched twice by one transaction keeps the image from before it.
    #[test]
    fn one_transaction_publishes_one_image_per_key() {
        let mut log = VersionLog::new();
        log.publish(
            4,
            [
                (1u64, b"k".to_vec(), Some(b"original".to_vec())),
                (1u64, b"k".to_vec(), Some(b"intermediate".to_vec())),
            ],
        );
        assert_eq!(log.len(), 1, "the intermediate value was never visible");
        assert_eq!(log.visible(1, b"k", 3), Visible::Instead(b"original"));
    }

    /// Collection drops what no snapshot can need and keeps what one can.
    #[test]
    fn collection_keeps_exactly_what_a_reader_can_still_need() {
        let mut log = VersionLog::new();
        for cts in 1..=10u64 {
            log.publish(
                cts,
                [(1u64, format!("k{cts}").into_bytes(), Some(vec![cts as u8]))],
            );
        }
        assert_eq!(log.len(), 10);

        // A reader at 4 reaches, for each key, the first image above 4. Every
        // key here has exactly one image, so it reaches k5 through k10.
        assert_eq!(log.collect(&[4]), 4);
        assert_eq!(log.len(), 6);
        assert_eq!(log.visible(1, b"k5", 4), Visible::Instead(&[5]));
        assert_eq!(log.visible(1, b"k4", 4), Visible::Page);

        // With no reader at all, everything goes.
        assert_eq!(log.collect(&[]), 6);
        assert!(log.is_empty());
        assert_eq!(log.keys(), 0, "an emptied key is removed, not left behind");
        assert_eq!(log.collected(), 10);
    }

    /// A withdrawn commit leaves nothing behind.
    #[test]
    fn a_withdrawn_commit_leaves_nothing_behind() {
        let mut log = VersionLog::new();
        log.publish(3, [(1u64, b"a".to_vec(), Some(b"x".to_vec()))]);
        log.publish(
            4,
            [
                (1u64, b"a".to_vec(), Some(b"y".to_vec())),
                (1u64, b"b".to_vec(), None),
            ],
        );
        assert_eq!(log.len(), 3);
        assert_eq!(log.withdraw(4).unwrap(), 2);
        assert_eq!(log.len(), 1);
        assert_eq!(log.visible(1, b"a", 2), Visible::Instead(b"x"));
        assert_eq!(log.visible(1, b"b", 2), Visible::Page);
        assert!(log.withdraw(FIRST_CTS).is_err());
    }

    /// A long reader's cost is what it holds open, not how long it is open.
    ///
    /// The TDD's phrasing, asserted: a reader that stays open while a million
    /// commits touch keys it is not looking at costs nothing extra, because the
    /// log is keyed by row and collection is driven by the oldest snapshot
    /// rather than by a clock.
    #[test]
    fn a_long_reader_costs_what_it_holds_open() {
        let clock = Clock::new(FIRST_CTS);
        let mut log = VersionLog::new();
        let reader = clock.snapshot();

        // Two hundred commits, all of them to the same twenty keys.
        for round in 0..200u64 {
            let cts = clock.commit();
            let key = format!("k{}", round % 20).into_bytes();
            log.publish(cts, [(1u64, key, Some(vec![round as u8]))]);
        }
        log.collect(&clock.active_timestamps());
        // The reader holds twenty keys open, one image each: the image from
        // before the first change it must not see. The other 180 are gone.
        assert_eq!(
            log.keys(),
            20,
            "the log grew with the number of commits rather than with the rows held open"
        );
        assert_eq!(
            log.len(),
            20,
            "the log holds one image per row held open, not one per commit"
        );
        assert_eq!(log.visible(1, b"k0", reader.cts()), Visible::Instead(&[0]));

        drop(reader);
        log.collect(&clock.active_timestamps());
        assert!(log.is_empty(), "a finished reader releases everything");
    }

    /// Two readers at different timestamps each keep the image they reach.
    ///
    /// The collection rule is per snapshot, so a key changed at 3, 6 and 9 with
    /// readers at 1 and 7 keeps the image at 3 (the first above 1) and the one
    /// at 9 (the first above 7), and drops the one at 6 that neither reaches.
    /// Collecting against the oldest reader alone would keep all three.
    #[test]
    fn every_reader_keeps_the_image_it_reaches_and_nothing_keeps_the_rest() {
        let mut log = VersionLog::new();
        log.publish(3, [(1u64, b"k".to_vec(), Some(b"at-3".to_vec()))]);
        log.publish(6, [(1u64, b"k".to_vec(), Some(b"at-6".to_vec()))]);
        log.publish(9, [(1u64, b"k".to_vec(), Some(b"at-9".to_vec()))]);
        assert_eq!(log.len(), 3);

        assert_eq!(
            log.collect(&[7, 1]),
            1,
            "the image at 6 is reached by nobody"
        );
        assert_eq!(log.len(), 2);
        assert_eq!(log.visible(1, b"k", 1), Visible::Instead(b"at-3"));
        assert_eq!(log.visible(1, b"k", 7), Visible::Instead(b"at-9"));

        // And the answers the two readers get are unchanged by the collection,
        // which is the property that makes it safe rather than merely small.
        assert_eq!(log.collect(&[7, 1]), 0, "collection is idempotent");
    }
}
