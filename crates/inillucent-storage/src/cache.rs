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

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, AtomicU8, Ordering};

use crate::pinstate::{PinState, Sweep};
use std::sync::{Arc, Mutex, OnceLock};

use inillucent_base::buffer::PageBuffer;
use inillucent_base::error::no_mem;
use inillucent_base::ids::{DatabaseId, PageId};

use crate::btree::PageLayout;
use inillucent_base::DbResult;

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

impl PageState {
    /// Returns the byte a frame stores this state as.
    ///
    /// The state has to be changeable without replacing the frame - a commit
    /// turns every dirty frame clean, and copying each page again to do that
    /// would double the cost of committing - so it lives in an atomic and this
    /// is its encoding.
    fn to_code(self) -> u8 {
        match self {
            PageState::Clean => 0,
            PageState::Dirty {
                before_image_saved: false,
            } => 1,
            PageState::Dirty {
                before_image_saved: true,
            } => 2,
            PageState::Writeback => 3,
            PageState::Invalid => 4,
        }
    }

    /// Returns the state a byte names, treating anything unknown as invalid.
    fn from_code(code: u8) -> PageState {
        match code {
            0 => PageState::Clean,
            1 => PageState::Dirty {
                before_image_saved: false,
            },
            2 => PageState::Dirty {
                before_image_saved: true,
            },
            3 => PageState::Writeback,
            _ => PageState::Invalid,
        }
    }

    /// Reports whether a frame in this state may be evicted.
    fn is_evictable(self) -> bool {
        matches!(self, PageState::Clean)
    }
}

/// One cached page.
#[derive(Debug)]
pub struct PageFrame {
    /// The page's validated structure, parsed at most once.
    ///
    /// Validating a page is the expensive half of reading one: an interior
    /// table page holds hundreds of cells and every one of them is decoded
    /// before the first is read. A descent parses each page it passes through,
    /// an insert parses the leaf again, and a balance parses it once more - so
    /// the same few hundred decodes were paid four or five times for one row.
    /// Hanging the result off the frame pays them once, and gets the
    /// invalidation for free: a writer publishes a *new* frame rather than
    /// mutating this one, so a layout cannot outlive the bytes it describes.
    layout: OnceLock<Arc<PageLayout>>,
    /// Which page this frame holds.
    pub key: PageKey,
    /// The page's bytes.
    bytes: PageBuffer,
    /// The pin count and CLOCK reference bit this frame is evicted by.
    ///
    /// Its own type because it is the one part of this structure that is
    /// synchronised without the shard's mutex, and therefore the one part Loom
    /// has anything to say about. See [`crate::pinstate`].
    pin: PinState,
    /// The frame's version.
    version: PageVersion,
    /// What the frame is doing, as a [`PageState`] code.
    state: AtomicU8,
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
        PageState::from_code(self.state.load(Ordering::Acquire))
    }

    /// Records what the frame is doing.
    fn set_state(&self, state: PageState) {
        self.state.store(state.to_code(), Ordering::Release);
    }

    /// Returns how many pins are outstanding.
    pub fn pin_count(&self) -> u32 {
        self.pin.pins()
    }

    /// Returns the page's validated layout, parsing it the first time.
    ///
    /// Two callers racing both parse and one result is kept; the two are equal,
    /// so which one wins does not matter, and the alternative is a lock on the
    /// read path to save an occasional duplicate parse.
    pub fn layout(&self, usable: u32) -> DbResult<Arc<PageLayout>> {
        if let Some(layout) = self.layout.get() {
            return Ok(Arc::clone(layout));
        }
        let parsed = Arc::new(PageLayout::parse(self.bytes(), self.key.page, usable)?);
        let _ = self.layout.set(Arc::clone(&parsed));
        Ok(parsed)
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

    /// Returns the page's validated layout, parsed at most once per frame.
    pub fn layout(&self, usable: u32) -> DbResult<Arc<PageLayout>> {
        self.frame.layout(usable)
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
        self.frame.pin.acquire();
        PagePin {
            frame: Arc::clone(&self.frame),
        }
    }
}

impl Drop for PagePin {
    /// Releases the pin.
    fn drop(&mut self) {
        self.frame.pin.release();
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
///
/// The frames are held in a map rather than a list, and that is a measured
/// choice rather than a stylistic one. With a list, finding a page meant
/// comparing keys down the shard - sixty-five pointer chases per lookup at a
/// four-thousand-page cache - and an insert makes about ten lookups, so the
/// scan was most of what a write cost. The clock hand still needs an order, so
/// it keeps one: a ring of keys that may name pages the map no longer holds,
/// which the hand skips and a compaction removes.
#[derive(Debug, Default)]
struct Shard {
    /// The frames this shard holds, by key.
    frames: HashMap<PageKey, Arc<PageFrame>>,
    /// The order the clock hand sweeps, which may name evicted pages.
    ring: Vec<PageKey>,
    /// Where the clock hand is.
    hand: usize,
}

impl Shard {
    /// Finds a resident frame.
    fn find(&self, key: PageKey) -> Option<Arc<PageFrame>> {
        self.frames.get(&key).map(Arc::clone)
    }

    /// Adds a frame, returning the byte size of one it replaced.
    fn put(&mut self, key: PageKey, frame: Arc<PageFrame>) -> Option<usize> {
        let replaced = self.frames.insert(key, frame);
        match replaced {
            Some(old) => Some(old.bytes.as_slice().len()),
            None => {
                self.ring.push(key);
                self.compact();
                None
            }
        }
    }

    /// Removes a frame, returning its byte size when one was resident.
    fn remove(&mut self, key: PageKey) -> Option<usize> {
        let frame = self.frames.remove(&key)?;
        Some(frame.bytes.as_slice().len())
    }

    /// Drops ring entries that name pages the map no longer holds.
    ///
    /// The ring is allowed to carry stale keys because removing one from the
    /// middle would be the linear scan this design exists to avoid. It is only
    /// worth compacting when the staleness is most of it.
    fn compact(&mut self) {
        if self.ring.len() < self.frames.len().saturating_mul(4).max(64) {
            return;
        }
        let live = &self.frames;
        self.ring.retain(|key| live.contains_key(key));
        self.hand = 0;
    }

    /// Evicts one frame, returning its byte size, or `None` when every frame
    /// is pinned.
    ///
    /// The hand makes at most two passes: the first clears reference bits, the
    /// second finds a frame whose bit was already clear. A third pass would
    /// mean every frame is pinned, which is a caller problem rather than a
    /// policy one and is reported as such.
    fn evict_one(&mut self) -> Option<usize> {
        if self.frames.is_empty() || self.ring.is_empty() {
            return None;
        }
        let limit = self.ring.len().saturating_mul(2);
        for _ in 0..limit {
            let index = self.hand % self.ring.len().max(1);
            self.hand = index.saturating_add(1);
            let Some(key) = self.ring.get(index).copied() else {
                continue;
            };
            let Some(frame) = self.frames.get(&key) else {
                continue;
            };
            // A pinned frame is in use; a frame that is not evictable holds a
            // change only this cache has, and evicting one would lose it, so
            // the budget is exceeded instead - which is the failure a caller
            // can survive; and a referenced frame spends its bit and gets one
            // more sweep. The three refusals and their orderings are
            // `PinState::sweep`, which Loom drives directly.
            if frame.pin.sweep(frame.state().is_evictable()) != Sweep::Evict {
                continue;
            }
            let size = frame.bytes.as_slice().len();
            self.frames.remove(&key);
            return Some(size);
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
        frame.pin.acquire();
        self.hits.fetch_add(1, Ordering::Relaxed);
        Some(PagePin { frame })
    }

    /// Returns the version of the resident frame for a page, if there is one.
    ///
    /// This takes no pin and counts no hit, because it answers a question about
    /// the cache rather than asking it for a page: a cursor uses it to find out
    /// whether the page it walked through has been replaced underneath it, and
    /// a lookup that counted as a hit would make the counters describe cursor
    /// bookkeeping instead of reads.
    pub fn version_of(&self, key: PageKey) -> Option<PageVersion> {
        let shard = self.lock(key.shard())?;
        let frame = shard.find(key)?;
        Some(frame.version())
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
            pin: PinState::held(),
            version,
            state: AtomicU8::new(PageState::Clean.to_code()),
            layout: OnceLock::new(),
        });
        {
            let Some(mut shard) = self.lock(key.shard()) else {
                return Err(no_mem("the page cache shard is poisoned"));
            };
            if let Some(existing) = shard.find(key) {
                drop(shard);
                existing.pin.acquire();
                return Ok(PagePin { frame: existing });
            }
            shard.put(key, Arc::clone(&frame));
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
            shard.frames.retain(|_, frame| {
                let keep = frame.pin_count() != 0 || !frame.state().is_evictable();
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
            for frame in shard.frames.values() {
                if frame.pin_count() != 0 {
                    count = count.saturating_add(1);
                }
            }
        }
        count
    }

    /// Publishes new bytes for a page, replacing whatever frame was resident.
    ///
    /// The old frame is removed from the shard rather than mutated, because a
    /// frame's bytes are immutable behind its `Arc`: a cursor that pinned the
    /// page keeps reading the bytes it validated, and its recorded version no
    /// longer matches the resident one, which is exactly the signal
    /// `path_is_current` exists to give. Mutating in place would change a
    /// page under a cursor that had already parsed it.
    pub fn publish(&self, key: PageKey, bytes: PageBuffer, state: PageState) -> DbResult<PagePin> {
        self.publish_with(key, bytes, state, None)
    }

    /// Publishes a page, seeding the layout the writer already computed.
    ///
    /// A writer publishes a new frame rather than mutating the resident one, so
    /// the layout cached on the old frame cannot describe the new bytes and is
    /// deliberately not carried across. What *can* be carried across is a
    /// layout the writer parsed from the bytes it is publishing - and since it
    /// has just laid those bytes out, it knows them. Without this the next
    /// reader parses the page again, which on a full index leaf is six
    /// microseconds paid once per cell inserted or removed.
    /// @param key - which page
    /// @param bytes - the page's new contents
    /// @param state - what the frame is doing
    /// @param layout - the layout of `bytes`, when the caller already has it
    pub fn publish_with(
        &self,
        key: PageKey,
        bytes: PageBuffer,
        state: PageState,
        layout: Option<Arc<PageLayout>>,
    ) -> DbResult<PagePin> {
        let size = bytes.as_slice().len();
        let version = PageVersion(self.next_version.fetch_add(1, Ordering::Relaxed));
        let seeded = OnceLock::new();
        if let Some(layout) = layout {
            let _ = seeded.set(layout);
        }
        let frame = Arc::new(PageFrame {
            key,
            bytes,
            pin: PinState::held(),
            version,
            state: AtomicU8::new(state.to_code()),
            layout: seeded,
        });
        let removed = {
            let Some(mut shard) = self.lock(key.shard()) else {
                return Err(no_mem("the page cache shard is poisoned"));
            };
            shard.put(key, Arc::clone(&frame))
        };
        match removed {
            Some(old) => {
                let old_size = old as u64;
                let new_size = size as u64;
                if new_size >= old_size {
                    self.resident_bytes
                        .fetch_add(new_size.saturating_sub(old_size), Ordering::Relaxed);
                } else {
                    self.resident_bytes
                        .fetch_sub(old_size.saturating_sub(new_size), Ordering::Relaxed);
                }
            }
            None => {
                self.resident.fetch_add(1, Ordering::Relaxed);
                self.resident_bytes
                    .fetch_add(size as u64, Ordering::Relaxed);
            }
        }
        self.enforce_budget();
        Ok(PagePin { frame })
    }

    /// Removes a page's frame whatever state it is in.
    ///
    /// This is what a rollback of a page that did not exist before the
    /// transaction does: there is no earlier image to put back, and leaving
    /// the frame resident would serve a page the file does not have.
    pub fn discard(&self, key: PageKey) {
        let Some(mut shard) = self.lock(key.shard()) else {
            return;
        };
        let removed = shard.remove(key);
        drop(shard);
        if let Some(size) = removed {
            self.resident.fetch_sub(1, Ordering::Relaxed);
            self.resident_bytes
                .fetch_sub(size as u64, Ordering::Relaxed);
        }
    }

    /// Removes every frame for one database above a page number.
    ///
    /// Truncation is the one operation that makes a resident frame describe a
    /// page that no longer exists, and a later read of that page number - after
    /// the file has grown again for a different reason - would otherwise be
    /// served the old contents out of the cache.
    pub fn discard_above(&self, database: DatabaseId, page_count: u32) {
        for index in 0..self.shards.len() {
            let Some(mut shard) = self.lock(index) else {
                continue;
            };
            let mut freed_bytes = 0u64;
            let mut freed = 0u64;
            shard.frames.retain(|key, frame| {
                let keep = key.database != database || key.page.get() <= page_count;
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
        }
    }

    /// Marks a resident frame clean, which is what a commit does once the
    /// page has reached the file.
    ///
    /// The frame keeps its bytes and its version: the contents are now the
    /// file's contents, so a cursor that pinned the page is still looking at
    /// the right thing, and only its eligibility for eviction has changed.
    pub fn mark_clean(&self, key: PageKey) {
        let Some(shard) = self.lock(key.shard()) else {
            return;
        };
        if let Some(frame) = shard.find(key) {
            drop(shard);
            frame.set_state(PageState::Clean);
        }
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
    use inillucent_base::page::PageSize;

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
