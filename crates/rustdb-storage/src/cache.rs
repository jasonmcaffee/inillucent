//! The page cache: sharded, pinned, versioned, and bounded.
//!
//! Invariant: a pinned frame is never evicted, and a frame that is evicted is
//! never being read. Both are enforced by construction rather than by
//! discipline: a page is reached only through a `PagePin`, which holds an
//! `Arc` to the frame and a pin count that it releases on drop, and eviction
//! refuses any frame whose pin count is not zero. There is no way to hold a
//! reference to a page's bytes without holding its pin, so "the cache evicted
//! a page someone was reading" cannot be written.
//!
//! Sharding is for contention, not for capacity. The key's page number picks
//! one of `SHARDS` independent maps, so two connections reading different
//! pages do not queue behind one lock; the byte budget is global and each
//! shard evicts its own share of it.
//!
//! Eviction is CLOCK: each frame has a reference bit that a hit sets and the
//! hand clears, and the hand evicts the first unpinned frame whose bit is
//! already clear. It is the policy SQLite's own cache uses for the same
//! reason - it approximates least-recently-used without a list to maintain on
//! every hit, which is the operation that actually happens millions of times.

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use rustdb_base::buffer::PageBuffer;
use rustdb_base::error::no_mem;
use rustdb_base::ids::{DatabaseId, PageId};
use rustdb_base::DbResult;

/// How many independent shards the cache is split into.
pub const SHARDS: usize = 64;

/// Which page of which database a frame holds.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct PageKey {
    /// Which attached database the page belongs to.
    pub database: DatabaseId,
    /// The page number within that database.
    pub page: PageId,
}

impl PageKey {
    /// Builds a key for the main database.
    pub fn main(page: PageId) -> PageKey {
        PageKey {
            database: DatabaseId(0),
            page,
        }
    }

    /// Returns the shard this key belongs to.
    fn shard(self) -> usize {
        // The page number alone would collide across databases on every shard
        // boundary, so both parts mix in.
        let mixed = u64::from(self.page.get()).wrapping_mul(0x9e37_79b9_7f4a_7c15)
            ^ u64::from(self.database.0).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        (mixed >> 32) as usize % SHARDS
    }
}

/// A frame's version, which increases every time its contents are replaced.
///
/// A cursor records the version of every page on its stack. If a page is
/// reloaded underneath it - which a read-only pager does not do, but a writer
/// and a WAL reader will - the recorded version no longer matches and the
/// cursor knows to reseek rather than to trust a slot number that now points
/// at a different cell.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct PageVersion(pub u64);

/// What a frame is doing.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PageState {
    /// The frame's bytes are the file's bytes and nothing has changed them.
    Clean,
    /// The frame has been modified by a write transaction.
    Dirty {
        /// Whether the before image has been journaled.
        before_image_saved: bool,
    },
    /// The frame is being written back to the file.
    Writeback,
    /// The frame's contents are not usable and it must be reloaded.
    Invalid,
}

/// One cached page.
#[derive(Debug)]
pub struct PageFrame {
    /// Which page this frame holds.
    pub key: PageKey,
    /// The page's bytes.
    bytes: PageBuffer,
    /// How many pins are outstanding.
    pins: AtomicU32,
    /// The frame's version.
    version: PageVersion,
    /// What the frame is doing.
    state: PageState,
    /// The CLOCK reference bit.
    referenced: AtomicBool,
}

impl PageFrame {
    /// Returns the page's bytes.
    pub fn bytes(&self) -> &[u8] {
        self.bytes.as_slice()
    }

    /// Returns the frame's version.
    pub fn version(&self) -> PageVersion {
        self.version
    }

    /// Returns what the frame is doing.
    pub fn state(&self) -> PageState {
        self.state
    }

    /// Returns how many pins are outstanding.
    pub fn pin_count(&self) -> u32 {
        self.pins.load(Ordering::Acquire)
    }
}

/// A pinned page.
///
/// While this exists the frame cannot be evicted. Dropping it releases the
/// pin, so a caller cannot forget: the only way to reach the bytes is through
/// the guard, and the only way to stop reaching them is to drop it.
#[derive(Debug)]
pub struct PagePin {
    frame: Arc<PageFrame>,
}

impl PagePin {
    /// Returns the page's bytes.
    pub fn bytes(&self) -> &[u8] {
        self.frame.bytes()
    }

    /// Returns which page this is.
    pub fn key(&self) -> PageKey {
        self.frame.key
    }

    /// Returns the page number.
    pub fn page(&self) -> PageId {
        self.frame.key.page
    }

    /// Returns the frame's version at the time it was pinned.
    pub fn version(&self) -> PageVersion {
        self.frame.version()
    }

    /// Returns the frame itself, for callers that need its state.
    pub fn frame(&self) -> &Arc<PageFrame> {
        &self.frame
    }
}

impl Clone for PagePin {
    /// Taking a second pin on the same frame increments the count, so both
    /// have to be dropped before the frame can be evicted.
    fn clone(&self) -> PagePin {
        self.frame.pins.fetch_add(1, Ordering::AcqRel);
        PagePin {
            frame: Arc::clone(&self.frame),
        }
    }
}

impl Drop for PagePin {
    /// Releases the pin.
    fn drop(&mut self) {
        self.frame.pins.fetch_sub(1, Ordering::AcqRel);
    }
}

/// What the cache has been doing, for diagnosis and for the perf baselines.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct CacheCounters {
    /// Lookups that found the page already resident.
    pub hits: u64,
    /// Lookups that had to read the page.
    pub misses: u64,
    /// Frames evicted to stay inside the budget.
    pub evictions: u64,
    /// Times eviction found nothing it was allowed to evict.
    pub eviction_failures: u64,
    /// Frames currently resident.
    pub resident: u64,
    /// Bytes currently resident.
    pub resident_bytes: u64,
}

/// One shard of the cache.
#[derive(Debug, Default)]
struct Shard {
    /// The frames this shard holds, in insertion order for the clock hand.
    frames: Vec<Arc<PageFrame>>,
    /// Where the clock hand is.
    hand: usize,
}

impl Shard {
    /// Finds a resident frame.
    fn find(&self, key: PageKey) -> Option<Arc<PageFrame>> {
        self.frames
            .iter()
            .find(|frame| frame.key == key)
            .map(Arc::clone)
    }

    /// Evicts one frame, returning its byte size, or `None` when every frame
    /// is pinned.
    ///
    /// The hand makes at most two passes: the first clears reference bits, the
    /// second finds a frame whose bit was already clear. A third pass would
    /// mean every frame is pinned, which is a caller problem rather than a
    /// policy one and is reported as such.
    fn evict_one(&mut self) -> Option<usize> {
        if self.frames.is_empty() {
            return None;
        }
        let limit = self.frames.len().saturating_mul(2);
        for _ in 0..limit {
            let index = self.hand % self.frames.len().max(1);
            self.hand = index.saturating_add(1);
            let Some(frame) = self.frames.get(index) else {
                continue;
            };
            if frame.pin_count() != 0 {
                continue;
            }
            if matches!(frame.state, PageState::Dirty { .. } | PageState::Writeback) {
                // A dirty frame cannot leave until the durability protocol has
                // written it. A read-only pager never makes one; the check is
                // here so that phase 4 cannot lose a page by adding one.
                continue;
            }
            if frame.referenced.swap(false, Ordering::AcqRel) {
                continue;
            }
            let evicted = self.frames.remove(index);
            self.hand = index;
            return Some(evicted.bytes.as_slice().len());
        }
        None
    }
}

/// A bounded, sharded page cache.
#[derive(Debug)]
pub struct PageCache {
    shards: Vec<Mutex<Shard>>,
    budget_bytes: u64,
    resident_bytes: AtomicU64,
    resident: AtomicU64,
    hits: AtomicU64,
    misses: AtomicU64,
    evictions: AtomicU64,
    eviction_failures: AtomicU64,
    next_version: AtomicU64,
}

impl PageCache {
    /// Builds a cache with a global byte budget.
    ///
    /// The budget is a target rather than a hard bound: a caller that pins
    /// more pages than fit gets them, because refusing a pin mid-traversal
    /// would fail a query that a slower cache would have answered. What the
    /// budget does guarantee is that unpinned pages are dropped to make room.
    pub fn new(budget_bytes: u64) -> PageCache {
        let mut shards = Vec::with_capacity(SHARDS);
        for _ in 0..SHARDS {
            shards.push(Mutex::new(Shard::default()));
        }
        PageCache {
            shards,
            budget_bytes: budget_bytes.max(1),
            resident_bytes: AtomicU64::new(0),
            resident: AtomicU64::new(0),
            hits: AtomicU64::new(0),
            misses: AtomicU64::new(0),
            evictions: AtomicU64::new(0),
            eviction_failures: AtomicU64::new(0),
            next_version: AtomicU64::new(1),
        }
    }

    /// Returns the cache's byte budget.
    pub fn budget_bytes(&self) -> u64 {
        self.budget_bytes
    }

    /// Returns a pin on a resident page, or `None` when it is not resident.
    pub fn get(&self, key: PageKey) -> Option<PagePin> {
        let shard = self.lock(key.shard())?;
        let frame = shard.find(key)?;
        drop(shard);
        frame.referenced.store(true, Ordering::Release);
        frame.pins.fetch_add(1, Ordering::AcqRel);
        self.hits.fetch_add(1, Ordering::Relaxed);
        Some(PagePin { frame })
    }

    /// Records a miss, for a caller that is about to read the page.
    pub fn record_miss(&self) {
        self.misses.fetch_add(1, Ordering::Relaxed);
    }

    /// Inserts a freshly read page and returns a pin on it.
    ///
    /// A concurrent reader may have inserted the same page first; in that case
    /// the resident frame wins and this one is dropped, so two readers never
    /// see two frames for one page.
    pub fn insert(&self, key: PageKey, bytes: PageBuffer) -> DbResult<PagePin> {
        let size = bytes.as_slice().len();
        let version = PageVersion(self.next_version.fetch_add(1, Ordering::Relaxed));
        let frame = Arc::new(PageFrame {
            key,
            bytes,
            pins: AtomicU32::new(1),
            version,
            state: PageState::Clean,
            referenced: AtomicBool::new(true),
        });
        {
            let Some(mut shard) = self.lock(key.shard()) else {
                return Err(no_mem("the page cache shard is poisoned"));
            };
            if let Some(existing) = shard.find(key) {
                drop(shard);
                existing.referenced.store(true, Ordering::Release);
                existing.pins.fetch_add(1, Ordering::AcqRel);
                return Ok(PagePin { frame: existing });
            }
            shard.frames.push(Arc::clone(&frame));
        }
        self.resident.fetch_add(1, Ordering::Relaxed);
        self.resident_bytes
            .fetch_add(size as u64, Ordering::Relaxed);
        self.enforce_budget();
        Ok(PagePin { frame })
    }

    /// Drops every unpinned frame, whatever the budget says.
    ///
    /// This is what a cursor reset or a `PRAGMA shrink_memory` does, and it is
    /// what the tests use to prove that a traversal left no pin behind: after
    /// it, `resident` counts exactly the pages someone is still holding.
    pub fn release_unpinned(&self) {
        for index in 0..self.shards.len() {
            let Some(mut shard) = self.lock(index) else {
                continue;
            };
            let mut freed_bytes = 0u64;
            let mut freed = 0u64;
            shard.frames.retain(|frame| {
                let keep = frame.pin_count() != 0
                    || matches!(frame.state, PageState::Dirty { .. } | PageState::Writeback);
                if !keep {
                    freed_bytes = freed_bytes.saturating_add(frame.bytes.as_slice().len() as u64);
                    freed = freed.saturating_add(1);
                }
                keep
            });
            shard.hand = 0;
            drop(shard);
            self.resident.fetch_sub(freed, Ordering::Relaxed);
            self.resident_bytes
                .fetch_sub(freed_bytes, Ordering::Relaxed);
            self.evictions.fetch_add(freed, Ordering::Relaxed);
        }
    }

    /// Returns what the cache has been doing.
    pub fn counters(&self) -> CacheCounters {
        CacheCounters {
            hits: self.hits.load(Ordering::Relaxed),
            misses: self.misses.load(Ordering::Relaxed),
            evictions: self.evictions.load(Ordering::Relaxed),
            eviction_failures: self.eviction_failures.load(Ordering::Relaxed),
            resident: self.resident.load(Ordering::Relaxed),
            resident_bytes: self.resident_bytes.load(Ordering::Relaxed),
        }
    }

    /// Returns how many frames are pinned right now.
    pub fn pinned_frames(&self) -> u64 {
        let mut count = 0u64;
        for index in 0..self.shards.len() {
            let Some(shard) = self.lock(index) else {
                continue;
            };
            for frame in &shard.frames {
                if frame.pin_count() != 0 {
                    count = count.saturating_add(1);
                }
            }
        }
        count
    }

    /// Evicts until the resident bytes are inside the budget, or until nothing
    /// can be evicted.
    fn enforce_budget(&self) {
        let mut guard = 0usize;
        while self.resident_bytes.load(Ordering::Relaxed) > self.budget_bytes {
            guard = guard.saturating_add(1);
            if guard > SHARDS.saturating_mul(4) {
                self.eviction_failures.fetch_add(1, Ordering::Relaxed);
                return;
            }
            let mut freed = false;
            for index in 0..self.shards.len() {
                let Some(mut shard) = self.lock(index) else {
                    continue;
                };
                if let Some(size) = shard.evict_one() {
                    drop(shard);
                    self.resident.fetch_sub(1, Ordering::Relaxed);
                    self.resident_bytes
                        .fetch_sub(size as u64, Ordering::Relaxed);
                    self.evictions.fetch_add(1, Ordering::Relaxed);
                    freed = true;
                    if self.resident_bytes.load(Ordering::Relaxed) <= self.budget_bytes {
                        return;
                    }
                }
            }
            if !freed {
                self.eviction_failures.fetch_add(1, Ordering::Relaxed);
                return;
            }
        }
    }

    /// Locks one shard, treating a poisoned lock as an empty shard rather than
    /// panicking; a panic here would take down a reader that did nothing wrong.
    fn lock(&self, index: usize) -> Option<std::sync::MutexGuard<'_, Shard>> {
        let shard = self.shards.get(index)?;
        match shard.lock() {
            Ok(guard) => Some(guard),
            Err(poisoned) => Some(poisoned.into_inner()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustdb_base::page::PageSize;

    /// Builds a page buffer whose first byte identifies it.
    fn page_of(size: PageSize, marker: u8) -> PageBuffer {
        let mut buffer = PageBuffer::zeroed(size).unwrap();
        buffer.as_mut_slice()[0] = marker;
        buffer
    }

    /// A hit returns the same bytes and counts as a hit; a lookup for a page
    /// that is not resident returns nothing rather than a zeroed frame.
    #[test]
    fn a_resident_page_is_returned_and_a_missing_one_is_not() {
        let size = PageSize::new(512).unwrap();
        let cache = PageCache::new(1 << 20);
        let key = PageKey::main(PageId::new(1).unwrap());
        assert!(cache.get(key).is_none());
        let pin = cache.insert(key, page_of(size, 0xab)).unwrap();
        assert_eq!(pin.bytes()[0], 0xab);
        let again = cache.get(key).unwrap();
        assert_eq!(again.bytes()[0], 0xab);
        assert_eq!(cache.counters().hits, 1);
        assert_eq!(cache.counters().resident, 1);
    }

    /// A pin keeps a frame alive; dropping every pin lets it go.
    #[test]
    fn pins_are_counted_and_released_on_drop() {
        let size = PageSize::new(512).unwrap();
        let cache = PageCache::new(1 << 20);
        let key = PageKey::main(PageId::new(1).unwrap());
        let pin = cache.insert(key, page_of(size, 1)).unwrap();
        assert_eq!(pin.frame().pin_count(), 1);
        let second = pin.clone();
        assert_eq!(pin.frame().pin_count(), 2);
        drop(second);
        assert_eq!(pin.frame().pin_count(), 1);
        assert_eq!(cache.pinned_frames(), 1);
        drop(pin);
        assert_eq!(cache.pinned_frames(), 0);
    }

    /// The budget must actually evict. A cache asked to hold ten pages of
    /// room and given a hundred pages has to end up near ten, not a hundred.
    #[test]
    fn the_budget_evicts_unpinned_pages() {
        let size = PageSize::new(512).unwrap();
        let cache = PageCache::new(512 * 10);
        for number in 1..=100u32 {
            let key = PageKey::main(PageId::new(number).unwrap());
            let pin = cache.insert(key, page_of(size, number as u8)).unwrap();
            drop(pin);
        }
        let counters = cache.counters();
        assert!(counters.evictions > 0, "{counters:?}");
        assert!(
            counters.resident_bytes <= 512 * 10 + 512,
            "{counters:?} exceeded the budget"
        );
    }

    /// A pinned page must survive pressure that evicts everything else. This
    /// is the invariant a cursor's correctness rests on.
    #[test]
    fn a_pinned_page_survives_cache_pressure() {
        let size = PageSize::new(512).unwrap();
        let cache = PageCache::new(512 * 4);
        let held_key = PageKey::main(PageId::new(1).unwrap());
        let held = cache.insert(held_key, page_of(size, 0x5a)).unwrap();
        for number in 2..=200u32 {
            let key = PageKey::main(PageId::new(number).unwrap());
            drop(cache.insert(key, page_of(size, number as u8)).unwrap());
        }
        assert_eq!(held.bytes()[0], 0x5a);
        assert!(cache.get(held_key).is_some(), "the pinned page was evicted");
        drop(held);
    }

    /// Releasing unpinned frames must leave exactly the pinned ones, which is
    /// how a test proves a traversal returned every pin it took.
    #[test]
    fn releasing_unpinned_frames_leaves_only_the_pinned_ones() {
        let size = PageSize::new(512).unwrap();
        let cache = PageCache::new(1 << 20);
        let held = cache
            .insert(PageKey::main(PageId::new(1).unwrap()), page_of(size, 1))
            .unwrap();
        for number in 2..=50u32 {
            drop(
                cache
                    .insert(
                        PageKey::main(PageId::new(number).unwrap()),
                        page_of(size, 2),
                    )
                    .unwrap(),
            );
        }
        assert_eq!(cache.counters().resident, 50);
        cache.release_unpinned();
        assert_eq!(cache.counters().resident, 1);
        assert_eq!(cache.pinned_frames(), 1);
        drop(held);
        cache.release_unpinned();
        assert_eq!(cache.counters().resident, 0);
        assert_eq!(cache.counters().resident_bytes, 0);
    }

    /// Inserting the same page twice must not produce two frames, or two
    /// readers would see two versions of one page.
    #[test]
    fn inserting_a_page_twice_keeps_one_frame() {
        let size = PageSize::new(512).unwrap();
        let cache = PageCache::new(1 << 20);
        let key = PageKey::main(PageId::new(9).unwrap());
        let first = cache.insert(key, page_of(size, 0x11)).unwrap();
        let second = cache.insert(key, page_of(size, 0x22)).unwrap();
        assert_eq!(cache.counters().resident, 1);
        // The resident frame wins; the loser's bytes are discarded.
        assert_eq!(second.bytes()[0], 0x11);
        assert_eq!(first.version(), second.version());
    }

    /// Versions must be distinct per frame, so a cursor can tell that the page
    /// under it was replaced rather than merely re-read.
    #[test]
    fn every_frame_gets_its_own_version() {
        let size = PageSize::new(512).unwrap();
        let cache = PageCache::new(1 << 20);
        let first = cache
            .insert(PageKey::main(PageId::new(1).unwrap()), page_of(size, 1))
            .unwrap();
        let second = cache
            .insert(PageKey::main(PageId::new(2).unwrap()), page_of(size, 2))
            .unwrap();
        assert_ne!(first.version(), second.version());
        let version = first.version();
        drop(first);
        cache.release_unpinned();
        let reloaded = cache
            .insert(PageKey::main(PageId::new(1).unwrap()), page_of(size, 1))
            .unwrap();
        assert_ne!(reloaded.version(), version);
    }

    /// Keys must spread across shards, or the sharding buys nothing.
    #[test]
    fn keys_spread_across_shards() {
        let mut seen = std::collections::BTreeSet::new();
        for number in 1..=1000u32 {
            seen.insert(PageKey::main(PageId::new(number).unwrap()).shard());
        }
        assert_eq!(seen.len(), SHARDS, "pages landed on {} shards", seen.len());
        // Two databases must not map the same page number to the same shard
        // every time, or an attached database would contend with the main one.
        let main = PageKey::main(PageId::new(7).unwrap()).shard();
        let attached = PageKey {
            database: DatabaseId(1),
            page: PageId::new(7).unwrap(),
        }
        .shard();
        assert_ne!(main, attached);
    }

    /// A cache in which every frame is pinned cannot evict, and must report
    /// that rather than spinning or dropping a pinned page.
    #[test]
    fn a_fully_pinned_cache_reports_that_it_cannot_evict() {
        let size = PageSize::new(512).unwrap();
        let cache = PageCache::new(512);
        let mut held = Vec::new();
        for number in 1..=20u32 {
            held.push(
                cache
                    .insert(
                        PageKey::main(PageId::new(number).unwrap()),
                        page_of(size, 1),
                    )
                    .unwrap(),
            );
        }
        let counters = cache.counters();
        assert_eq!(counters.resident, 20);
        assert!(counters.eviction_failures > 0, "{counters:?}");
        assert_eq!(cache.pinned_frames(), 20);
        drop(held);
    }
}
