//! The buffer pool: frames, the page table, the cooling FIFO, eviction, and
//! writeback with swip translation.
//!
//! Invariant: a page a caller is reading is a page the pool cannot move. Every
//! other rule here is a consequence of that one, and the two the TDD numbers
//! are the two ways it can be broken.
//!
//! Invariant (TDD 5): a swizzled swip is only ever stored while the child frame
//! is resident, and a frame with a live pin is never evicted. The two halves are
//! enforced in different places and both are checked: the pin count gates
//! [`Pool::cool`] and [`Pool::evict_one`], and the parent back-reference
//! recorded when a swip is swizzled is what lets eviction put the page id back
//! before the frame is reused.
//!
//! Invariant (TDD 6): a page written to disk contains no frame references.
//! [`Pool::writeback`] copies the frame, translates every swizzled swip in the
//! copy, checksums the copy, and writes that. The in-memory frame keeps its
//! swizzled swips, because unswizzling a page just because it was written would
//! throw away the work for no reason.
//!
//! ## What a frame is, and why swizzling is by index
//!
//! The TDD asks for one virtual reservation so that frame *addresses* are
//! stable and a swip can be a pointer. This implementation allocates every
//! frame buffer once, at construction, and never resizes the frame array - so
//! frame identity is stable in exactly the way swizzling needs - but a swip
//! holds the frame's **index**. [`crate::swip`] argues that trade in full: an
//! index is a bounds-checked add where a pointer is a dereference, both skip
//! the page-table hash that swizzling exists to skip, and only one of them
//! needs `unsafe`.
//!
//! ## Borrowing, pinning and the push model
//!
//! [`Pool::fetch`] hands back a [`PageGuard`], which is a shared borrow of the
//! frame plus a pin. The borrow's lifetime is the guard's, so a caller reads a
//! page inside the scope that holds it. That is not a limitation the executor
//! has to work around - it is why the executor is push-based. A scan fetches a
//! leaf, builds a batch of vectors that borrow it, pushes the batch downstream,
//! and drops the guard; nothing needs the bytes after the push returns.
//!
//! A frame that is borrowed cannot be loaded over, and the pin count says so
//! before the borrow is attempted, so an eviction never has to discover a live
//! borrow by panicking on it.

use std::cell::{Cell, Ref, RefCell};
use std::collections::{HashMap, VecDeque};
use std::ops::Deref;

use inillucent_base::error::{corrupt, misuse, no_mem};
use inillucent_base::rng::Rng;
use inillucent_base::DbResult;
use inillucent_vfs::{SyncMode, VfsFile};

use crate::latch::{Observed, VersionLatch};
use crate::meta::{Meta, FIRST_DATA_PAGE, META_PAGE, SHADOW_PAGE};
use crate::page::{self, PageKind};
use crate::swip::Swip;
use crate::PageId;

/// The fraction of the pool that must be free before the clock runs.
///
/// The TDD's 2%: below it the cooling sweep starts, so an allocation almost
/// never has to wait for one.
const FREE_WATERMARK: f64 = 0.02;

/// The fraction of the pool one cooling sweep moves into the FIFO.
const COOL_FRACTION: f64 = 0.10;

/// How many random frames the clock samples per frame it cools.
///
/// Sampling rather than scanning is what keeps the clock O(cooled) instead of
/// O(pool); four candidates per pick is enough that a pinned or already-cooling
/// frame does not stall the sweep.
const SAMPLE_FACTOR: usize = 4;

/// What a frame is doing.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FrameState {
    /// Holds no page.
    Free,
    /// Holds a page and is reachable through a swizzled swip or the page table.
    Hot,
    /// Holds a page, its parent's swip has been put back to a page id, and it
    /// is queued for eviction.
    Cooling,
}

/// What the pool has been asked to do, for the report.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct PoolStats {
    /// Fetches answered from a resident frame.
    pub hits: u64,
    /// Fetches that had to read the file.
    pub misses: u64,
    /// Fetches answered by a frame in the cooling FIFO, which cost no I/O.
    pub rewarms: u64,
    /// Frames moved into the cooling FIFO.
    pub cooled: u64,
    /// Frames evicted.
    pub evicted: u64,
    /// Pages read from the file.
    pub reads: u64,
    /// Pages written to the file.
    pub writes: u64,
    /// Swips translated back to page ids on writeback.
    pub translated: u64,
}

/// One frame's bookkeeping, held apart from its bytes.
#[derive(Clone, Copy, Debug)]
struct FrameMeta {
    /// The page the frame holds, or [`PageId::NONE`] when free.
    page: PageId,
    /// What the frame is doing.
    state: FrameState,
    /// Whether the frame differs from the file.
    dirty: bool,
    /// The parent frame, the *page* that frame held, and the byte offset of
    /// the swip that points here.
    ///
    /// `None` for a root, for a page reached through the page table rather
    /// than through a swip, and for a cooling frame whose swip has already been
    /// put back.
    ///
    /// The page is carried alongside the frame because the frame number alone
    /// goes stale: a parent that is itself evicted leaves its children holding
    /// a reference to a frame that now holds somebody else's page, and putting
    /// a page id back into *that* is a silent corruption of an unrelated page.
    /// A descent through an eight-frame pool found it by losing a key.
    parent: Option<(u32, PageId, usize)>,
}

impl FrameMeta {
    /// Returns the bookkeeping of a frame holding nothing.
    fn empty() -> FrameMeta {
        FrameMeta {
            page: PageId::NONE,
            state: FrameState::Free,
            dirty: false,
            parent: None,
        }
    }
}

/// The page table's hasher: one multiply, no keying.
///
/// **This is a measurement.** The page table is keyed by a `u64` page number
/// and was hashed with the standard library's default, which is SipHash-1-3
/// with a per-process key. That is the right default for a map whose keys come
/// from outside the process and the wrong one for this map: every fetch that is
/// not already holding a frame number pays it, and `inillucent-probeprofile`
/// measured a resident fetch at 19.0 ns against a descent of 97.6 ns - a fifth
/// of a descent spent hashing eight bytes nobody is attacking.
///
/// Page numbers are dense and sequential, so multiplying by an odd constant and
/// keeping the high bits is enough to spread them across the buckets. A tree's
/// pages are allocated consecutively, which is the case a weak hash would fail
/// on, and the multiply moves the entropy into the top bits precisely so that
/// consecutive keys land in different buckets rather than adjacent ones.
#[derive(Default)]
struct PageHasher {
    /// The hash so far.
    state: u64,
}

impl std::hash::Hasher for PageHasher {
    fn finish(&self) -> u64 {
        self.state
    }

    /// Hashes bytes, which a `PageId` never produces.
    ///
    /// `PageId` derives `Hash` over its single `u64`, so this is only reached
    /// if the key type changes. Mixing them one at a time is slow and correct,
    /// which is the right trade for a path nothing takes.
    ///
    /// @param bytes - the bytes to fold in
    fn write(&mut self, bytes: &[u8]) {
        for byte in bytes {
            self.state = self
                .state
                .rotate_left(8)
                .wrapping_add(u64::from(*byte))
                .wrapping_mul(0x9E37_79B9_7F4A_7C15);
        }
    }

    /// Hashes one page number.
    ///
    /// @param value - the page number
    fn write_u64(&mut self, value: u64) {
        self.state = value
            .wrapping_mul(0x9E37_79B9_7F4A_7C15)
            .rotate_left(31)
            .wrapping_mul(0x9E37_79B9_7F4A_7C15);
    }
}

/// The page table's hasher factory.
type PageHashing = std::hash::BuildHasherDefault<PageHasher>;

/// Everything about the pool that a fetch may have to change.
struct State {
    /// One entry per frame.
    frames: Vec<FrameMeta>,
    /// Which frame holds which page.
    table: HashMap<PageId, u32, PageHashing>,
    /// Frames holding nothing.
    free: Vec<u32>,
    /// Frames queued for eviction, coldest first.
    cooling: VecDeque<u32>,
    /// The clock's position, so successive sweeps do not resample the same
    /// frames.
    clock: Rng,
}

/// A pinned, borrowed page.
pub struct PageGuard<'p> {
    pool: &'p Pool,
    frame: u32,
    bytes: Ref<'p, Vec<u8>>,
}

impl std::fmt::Debug for PageGuard<'_> {
    fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        out.debug_struct("PageGuard")
            .field("frame", &self.frame)
            .field("bytes", &self.bytes.len())
            .finish()
    }
}

impl PageGuard<'_> {
    /// Returns the frame holding this page.
    pub fn frame(&self) -> u32 {
        self.frame
    }

    /// Returns the page's bytes.
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }
}

impl Deref for PageGuard<'_> {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        &self.bytes
    }
}

impl Drop for PageGuard<'_> {
    fn drop(&mut self) {
        self.pool.unpin(self.frame);
    }
}

/// The buffer pool over one data file.
pub struct Pool {
    /// The page size every frame holds.
    page_size: usize,
    /// The frame buffers, allocated once and never resized.
    buffers: Vec<RefCell<Vec<u8>>>,
    /// One version latch per frame, so a descent can read optimistically.
    ///
    /// Phase 2 reads single-threaded and never fails a validation, which is
    /// stated rather than hidden: the latch is here because it is the piece a
    /// concurrent checkpointer needs unchanged and because a latch is easy to
    /// get right while writing it and impossible to retrofit. Every load and
    /// every eviction takes it exclusively, so the version genuinely moves when
    /// a frame's contents change - which is what makes the descent's validation
    /// a real check rather than a formality that would pass regardless.
    latches: Vec<VersionLatch>,
    /// How many guards are outstanding on each frame.
    ///
    /// **Held apart from [`State`], which is a measurement.** A pin and an
    /// unpin happen on every fetch, and both went through the one `RefCell`
    /// that also holds the page table, the free list and the cooling queue - so
    /// a two-level descent took six borrows of it to move four counters.
    /// `inillucent-probeprofile` measures a descent of a fifteen-leaf index at
    /// 74 ns, where the search inside it is four comparisons; most of the rest
    /// was this. A `Cell` per frame is the same single-threaded discipline with
    /// none of the sharing.
    pins: Vec<Cell<u32>>,
    /// The bookkeeping.
    state: RefCell<State>,
    /// Where pages come from and go.
    file: Box<dyn VfsFile>,
    /// How many pages the file holds.
    page_count: Cell<u64>,
    /// What has happened, for the report.
    ///
    /// One `Cell` per counter rather than one `Cell<PoolStats>`, because
    /// reading and writing the whole struct to increment `hits` copied
    /// seventy-two bytes in each direction on every fetch.
    counters: Counters,
    /// The write-ahead watermark: no page whose LSN is at or above this may be
    /// written to the data file.
    ///
    /// **This is the whole of the pool's relationship with the log.** The rule
    /// is "nothing is durable before its log record is", and the number that
    /// decides it can only be known by the log - so the pool is *told* it rather
    /// than made to depend on the crate that owns it. `inillucent-wal` sits at
    /// the same layer as this crate and neither depends on the other; a `u64`
    /// crosses the gap where a dependency edge would have pointed the wrong way,
    /// because the log is written before any page is.
    ///
    /// `u64::MAX` means "no log", which is what a Phase 2 read-only open and
    /// every bulk build is: there is no log to be ahead of, so nothing is
    /// refused. A caller that has a log calls [`Pool::set_durable_lsn`] after
    /// every sync, and [`Pool::writeback`] refuses a page the log has not
    /// caught up with.
    durable_lsn: Cell<u64>,
}

/// The pool's counters, one cell each.
#[derive(Default)]
struct Counters {
    /// Fetches answered from a resident frame.
    hits: Cell<u64>,
    /// Fetches that had to read the file.
    misses: Cell<u64>,
    /// Fetches answered by a frame in the cooling FIFO.
    rewarms: Cell<u64>,
    /// Frames moved into the cooling FIFO.
    cooled: Cell<u64>,
    /// Frames evicted.
    evicted: Cell<u64>,
    /// Pages read from the file.
    reads: Cell<u64>,
    /// Pages written to the file.
    writes: Cell<u64>,
    /// Swips translated back to page ids on writeback.
    translated: Cell<u64>,
}

impl Counters {
    /// Adds one to a counter.
    ///
    /// @param counter - the cell to bump
    /// @param by - how much to add
    fn add(counter: &Cell<u64>, by: u64) {
        counter.set(counter.get().saturating_add(by));
    }
}

impl Pool {
    /// Returns a pool over an open file.
    ///
    /// Every frame buffer is allocated here, so a fetch never allocates and the
    /// pool's memory footprint is what the configuration says it is rather than
    /// whatever the workload grew it to.
    ///
    /// @param file - the data file
    /// @param page_size - the page size in bytes
    /// @param frames - how many frames the pool holds
    /// @param page_count - how many pages the file currently holds
    pub fn new(
        file: Box<dyn VfsFile>,
        page_size: usize,
        frames: usize,
        page_count: u64,
    ) -> DbResult<Pool> {
        if frames == 0 {
            return Err(misuse("a buffer pool needs at least one frame"));
        }
        if page_size == 0 {
            return Err(misuse("a buffer pool needs a page size"));
        }
        let mut buffers = Vec::new();
        buffers
            .try_reserve(frames)
            .map_err(|_| no_mem(format!("{frames} frames of {page_size} bytes")))?;
        let mut latches = Vec::new();
        latches
            .try_reserve(frames)
            .map_err(|_| no_mem(format!("{frames} frame latches")))?;
        let mut pins = Vec::new();
        pins.try_reserve(frames)
            .map_err(|_| no_mem(format!("{frames} pin counters")))?;
        for _ in 0..frames {
            buffers.push(RefCell::new(vec![0u8; page_size]));
            latches.push(VersionLatch::new());
            pins.push(Cell::new(0));
        }
        Ok(Pool {
            page_size,
            buffers,
            latches,
            pins,
            state: RefCell::new(State {
                frames: vec![FrameMeta::empty(); frames],
                table: HashMap::with_capacity_and_hasher(frames, PageHashing::default()),
                free: (0..frames as u32).rev().collect(),
                cooling: VecDeque::new(),
                clock: Rng::new(0x5EED_0B0F_C0FF_EE01),
            }),
            file,
            page_count: Cell::new(page_count),
            counters: Counters::default(),
            // No log until a caller says otherwise, so nothing is refused. A
            // bulk build and a read-only open both run this way, and both are
            // correct to: a page cannot be ahead of a log that does not exist.
            durable_lsn: Cell::new(u64::MAX),
        })
    }

    /// Tells the pool how far the log is durable.
    ///
    /// After this, [`Pool::writeback`] refuses any page whose LSN is above
    /// `lsn`. The caller sets it after every sync of the log and never before
    /// one: a watermark that ran ahead of the media would turn the check into a
    /// formality that always passes, which is worse than no check at all
    /// because it looks like one.
    ///
    /// @param lsn - the position past the last durable byte of the log
    pub fn set_durable_lsn(&self, lsn: u64) {
        self.durable_lsn.set(lsn);
    }

    /// Returns the write-ahead watermark, or `u64::MAX` when there is no log.
    pub fn durable_lsn(&self) -> u64 {
        self.durable_lsn.get()
    }

    /// Returns the page size in bytes.
    pub fn page_size(&self) -> usize {
        self.page_size
    }

    /// Returns how many frames the pool holds.
    pub fn frames(&self) -> usize {
        self.buffers.len()
    }

    /// Returns how many bytes the pool occupies.
    pub fn byte_size(&self) -> usize {
        self.buffers.len().saturating_mul(self.page_size)
    }

    /// Returns how many pages the file holds.
    pub fn page_count(&self) -> u64 {
        self.page_count.get()
    }

    /// Returns what the pool has done.
    pub fn stats(&self) -> PoolStats {
        PoolStats {
            hits: self.counters.hits.get(),
            misses: self.counters.misses.get(),
            rewarms: self.counters.rewarms.get(),
            cooled: self.counters.cooled.get(),
            evicted: self.counters.evicted.get(),
            reads: self.counters.reads.get(),
            writes: self.counters.writes.get(),
            translated: self.counters.translated.get(),
        }
    }

    /// Returns how many guards are outstanding on a frame.
    ///
    /// @param frame - the frame's index
    fn pins_of(&self, frame: u32) -> u32 {
        self.pins.get(frame as usize).map(Cell::get).unwrap_or(0)
    }

    /// Forgets the counters, so a measurement can start from zero.
    pub fn reset_stats(&self) {
        self.counters.hits.set(0);
        self.counters.misses.set(0);
        self.counters.rewarms.set(0);
        self.counters.cooled.set(0);
        self.counters.evicted.set(0);
        self.counters.reads.set(0);
        self.counters.writes.set(0);
        self.counters.translated.set(0);
    }

    /// Returns how many frames hold a page right now.
    pub fn resident(&self) -> usize {
        self.state.borrow().table.len()
    }

    /// Returns what one frame is doing, for tests and for the campaign.
    ///
    /// @param frame - the frame's index
    pub fn frame_state(&self, frame: u32) -> Option<FrameState> {
        self.state
            .borrow()
            .frames
            .get(frame as usize)
            .map(|meta| meta.state)
    }

    /// Reports whether a page is resident, without fetching it.
    ///
    /// @param page - the page to look for
    pub fn is_resident(&self, page: PageId) -> bool {
        self.state.borrow().table.contains_key(&page)
    }

    /// Pins and borrows a page, reading it from the file if it is not resident.
    ///
    /// @param page - the page to read
    pub fn fetch(&self, page: PageId) -> DbResult<PageGuard<'_>> {
        let frame = self.resolve(page)?;
        self.borrow_frame(frame)
    }

    /// Returns the page a swip names, whichever form it is in.
    ///
    /// A swizzled swip names a frame, and the frame knows its page, so this is
    /// the one place the two forms are reconciled. It is separate from
    /// [`Pool::fetch`] because a descent needs the page id before it fetches -
    /// it records the child in its path, and a path of frame numbers would stop
    /// meaning anything the moment one was evicted.
    ///
    /// @param swip - the child reference read from an interior page
    pub fn page_of_swip(&self, swip: Swip) -> DbResult<PageId> {
        if let Some(page) = swip.page() {
            if page.is_none() {
                return Err(corrupt("an interior slot names no child"));
            }
            return Ok(page);
        }
        let frame = swip
            .frame()
            .ok_or_else(|| corrupt("a swip is neither a page nor a frame"))?;
        let page = self
            .page_in_frame(frame)
            .ok_or_else(|| corrupt(format!("a swip names frame {frame}, which holds no page")))?;
        if page.is_none() {
            return Err(corrupt(format!("frame {frame} holds no page")));
        }
        Ok(page)
    }

    /// Returns the page a frame currently holds.
    ///
    /// @param frame - the frame's index
    pub fn page_in_frame(&self, frame: u32) -> Option<PageId> {
        self.state
            .borrow()
            .frames
            .get(frame as usize)
            .map(|meta| meta.page)
    }

    /// Records which slot of which frame points at a child.
    ///
    /// This is the back-reference eviction needs: to reuse a frame it must
    /// first put a page id back into whatever swizzled swip names it, and the
    /// child is the only thing that knows where that is.
    ///
    /// @param child - the child's frame
    /// @param parent - the parent's frame
    /// @param parent_page - the page that frame holds
    /// @param at - the swip's byte offset inside the parent page
    pub fn note_parent(&self, child: u32, parent: u32, parent_page: PageId, at: usize) {
        let mut state = self.state.borrow_mut();
        if let Some(meta) = state.frames.get_mut(child as usize) {
            meta.parent = Some((parent, parent_page, at));
        }
    }

    /// Writes a swizzled swip into a parent page, if the parent is still there.
    ///
    /// The page check is what makes this safe to call after the parent's guard
    /// has been dropped: a frame that was reused in between holds somebody
    /// else's page, and writing eight bytes of frame number into the middle of
    /// it would be a corruption with no error attached. A skipped swizzle costs
    /// one page-table lookup on the next descent and nothing else.
    ///
    /// The write does not dirty the frame. A swizzled swip and an unswizzled
    /// one name the same child; the page's *content* is unchanged, and
    /// [`Pool::writeback`] translates the form back on the way out.
    ///
    /// @param parent - the parent's frame
    /// @param expected - the page the parent frame should still hold
    /// @param at - the swip's byte offset inside the parent page
    /// @param swip - the reference to store
    pub fn swizzle_into(
        &self,
        parent: u32,
        expected: PageId,
        at: usize,
        swip: Swip,
    ) -> DbResult<bool> {
        if self.page_in_frame(parent) != Some(expected) {
            return Ok(false);
        }
        let Some(cell) = self.buffers.get(parent as usize) else {
            return Ok(false);
        };
        let Ok(mut bytes) = cell.try_borrow_mut() else {
            return Ok(false);
        };
        page::write_u64(&mut bytes, at, swip.raw())?;
        Ok(true)
    }

    /// Pins and borrows a frame by index.
    ///
    /// @param frame - the frame's index
    pub fn fetch_frame(&self, frame: u32) -> DbResult<PageGuard<'_>> {
        self.borrow_frame(frame)
    }

    /// Returns the frame holding a page, loading it if necessary.
    ///
    /// @param page - the page wanted
    fn resolve(&self, page: PageId) -> DbResult<u32> {
        if let Some(frame) = self.lookup(page) {
            return Ok(frame);
        }
        self.load(page)
    }

    /// Returns the frame a resident page is in, rewarming it if it was cooling.
    ///
    /// @param page - the page wanted
    fn lookup(&self, page: PageId) -> Option<u32> {
        let mut state = self.state.borrow_mut();
        let frame = *state.table.get(&page)?;
        let cooling = state
            .frames
            .get(frame as usize)
            .map(|meta| meta.state == FrameState::Cooling)
            .unwrap_or(false);
        if cooling {
            // A descent that reaches a cooling page takes it back out of the
            // FIFO. That is the whole point of the FIFO: the page is still
            // resident, so a second chance costs a queue removal rather than a
            // read.
            state.cooling.retain(|held| *held != frame);
            if let Some(meta) = state.frames.get_mut(frame as usize) {
                meta.state = FrameState::Hot;
            }
            Counters::add(&self.counters.rewarms, 1);
        }
        Counters::add(&self.counters.hits, 1);
        Some(frame)
    }

    /// Reads a page into a frame and records it in the page table.
    ///
    /// @param page - the page to read
    fn load(&self, page: PageId) -> DbResult<u32> {
        let frame = self.claim_frame()?;
        // The frame's contents are about to change, so its version moves. A
        // descent holding an observation of this frame is reading a page that
        // is no longer there, and the bump is what tells it so.
        let held = self.latch(frame).map(|latch| latch.try_exclusive());
        let filled = self.fill_frame(frame, page);
        if let Err(error) = filled {
            // **The frame goes back.** A read can fail three ways - the buffer
            // is borrowed, the file is short or unreadable, the checksum does
            // not match - and every one of them used to return the error while
            // keeping the frame, so a pool that saw sixteen failed reads had
            // sixteen fewer frames and then reported "every frame is pinned",
            // which is not even the right diagnosis.
            //
            // Found by a Phase 3 recovery campaign fetching pages a truncated
            // file does not hold, which is the ordinary shape of a fault
            // campaign and of any reader probing for a page. The exclusive
            // latch leaked with it.
            if held == Some(true) {
                if let Some(latch) = self.latch(frame) {
                    latch.release_exclusive();
                }
            }
            self.state.borrow_mut().free.push(frame);
            return Err(error);
        }
        let mut state = self.state.borrow_mut();
        if let Some(meta) = state.frames.get_mut(frame as usize) {
            meta.page = page;
            meta.state = FrameState::Hot;
            meta.dirty = false;
            meta.parent = None;
        }
        if let Some(slot) = self.pins.get(frame as usize) {
            slot.set(0);
        }
        state.table.insert(page, frame);
        drop(state);
        if held == Some(true) {
            if let Some(latch) = self.latch(frame) {
                latch.release_exclusive();
            }
        }
        Counters::add(&self.counters.misses, 1);
        Counters::add(&self.counters.reads, 1);
        Ok(frame)
    }

    /// Reads a page into a claimed frame and checks it.
    ///
    /// Separate from [`Pool::load`] so that every way it can fail returns
    /// through one place, which is what lets the frame be given back on all of
    /// them rather than on the ones somebody remembered.
    ///
    /// @param frame - the claimed frame
    /// @param page - the page to read into it
    fn fill_frame(&self, frame: u32, page: PageId) -> DbResult<()> {
        let mut bytes = self
            .buffers
            .get(frame as usize)
            .ok_or_else(|| misuse("frame index out of range"))?
            .try_borrow_mut()
            .map_err(|_| misuse("a frame chosen for loading was still borrowed"))?;
        self.file
            .read_exact_at(
                page.0.saturating_mul(self.page_size as u64),
                bytes.as_mut_slice(),
            )
            .map_err(|error| error.into_db_error())?;
        page::verify_checksum(&bytes, page)
    }

    /// Returns a frame holding nothing, cooling and evicting to get one.
    fn claim_frame(&self) -> DbResult<u32> {
        if let Some(frame) = self.state.borrow_mut().free.pop() {
            return Ok(frame);
        }
        self.cool()?;
        if let Some(frame) = self.evict_one()? {
            return Ok(frame);
        }
        Err(no_mem(
            "every frame in the buffer pool is pinned; nothing can be evicted",
        ))
    }

    /// Moves a share of the pool into the cooling FIFO.
    ///
    /// Sampling random hot frames rather than scanning is LeanStore's clock:
    /// the sweep costs what it cools rather than what the pool holds, and a
    /// frame that keeps being used keeps being rewarmed out of the queue before
    /// it reaches the front.
    ///
    /// Returns how many frames it moved.
    pub fn cool(&self) -> DbResult<usize> {
        let total = self.buffers.len();
        let want = ((total as f64) * COOL_FRACTION).ceil() as usize;
        let want = want.max(1);
        let mut moved = 0usize;
        let mut attempts = 0usize;
        let budget = want.saturating_mul(SAMPLE_FACTOR).max(total.min(64));
        while moved < want && attempts < budget {
            attempts = attempts.saturating_add(1);
            let candidate = {
                let mut state = self.state.borrow_mut();
                if state.cooling.len() >= want {
                    break;
                }
                let pick = (state.clock.next_u64() as usize) % total.max(1);
                match state.frames.get(pick) {
                    Some(meta)
                        if meta.state == FrameState::Hot
                            && self.pins_of(pick as u32) == 0
                            && !self.parent_is_pinned(meta.parent) =>
                    {
                        Some(pick as u32)
                    }
                    _ => None,
                }
            };
            let Some(frame) = candidate else {
                continue;
            };
            if self.unswizzle_from_parent(frame)? {
                let mut state = self.state.borrow_mut();
                if let Some(meta) = state.frames.get_mut(frame as usize) {
                    meta.state = FrameState::Cooling;
                    meta.parent = None;
                }
                state.cooling.push_back(frame);
                moved = moved.saturating_add(1);
            }
        }
        if moved == 0 {
            // Sampling is a policy, not a guarantee. In a pool of a few frames
            // a random walk can miss the one evictable frame there is, and the
            // caller's next step is to report that the pool is full - which is
            // wrong when it is not. So a sweep that found nothing falls back to
            // a scan, and the pool reports "everything is pinned" only when
            // everything really is.
            //
            // The eight-frame campaign is what found this: a descent pinned two
            // frames, six were coolable, and the clock sampled its budget away
            // without touching one of them.
            for frame in 0..total {
                let candidate = {
                    let state = self.state.borrow();
                    match state.frames.get(frame) {
                        Some(meta)
                            if meta.state == FrameState::Hot
                                && self.pins_of(frame as u32) == 0
                                && !self.parent_is_pinned(meta.parent) =>
                        {
                            Some(frame as u32)
                        }
                        _ => None,
                    }
                };
                let Some(frame) = candidate else {
                    continue;
                };
                if self.unswizzle_from_parent(frame)? {
                    let mut state = self.state.borrow_mut();
                    if let Some(meta) = state.frames.get_mut(frame as usize) {
                        meta.state = FrameState::Cooling;
                        meta.parent = None;
                    }
                    state.cooling.push_back(frame);
                    moved = moved.saturating_add(1);
                    break;
                }
            }
        }
        Counters::add(&self.counters.cooled, moved as u64);
        Ok(moved)
    }

    /// Puts a page id back into the parent's swip, so the child can be reused.
    ///
    /// Returns false when the parent could not be written, which leaves the
    /// child hot rather than making it unreachable.
    ///
    /// @param frame - the child frame
    fn unswizzle_from_parent(&self, frame: u32) -> DbResult<bool> {
        let (parent, parent_page, at, page) = {
            let state = self.state.borrow();
            let Some(meta) = state.frames.get(frame as usize) else {
                return Ok(false);
            };
            match meta.parent {
                Some((parent, parent_page, at)) => (parent, parent_page, at, meta.page),
                None => return Ok(true),
            }
        };
        // The parent may itself have been evicted since it swizzled this child.
        // If its frame now holds a different page then nothing points at this
        // one any more - the parent's own writeback translated the swip on its
        // way out - so there is nothing to put back, and writing into that
        // frame would corrupt whatever page it holds now.
        if self.page_in_frame(parent) != Some(parent_page) {
            return Ok(true);
        }
        let Some(cell) = self.buffers.get(parent as usize) else {
            return Ok(false);
        };
        let Ok(mut bytes) = cell.try_borrow_mut() else {
            return Ok(false);
        };
        page::write_u64(&mut bytes, at, Swip::unswizzled(page).raw())?;
        Ok(true)
    }

    /// Evicts the coldest frame in the FIFO, writing it back if it is dirty.
    ///
    /// Returns the freed frame, or `None` when nothing in the queue could go.
    pub fn evict_one(&self) -> DbResult<Option<u32>> {
        loop {
            let frame = {
                let mut state = self.state.borrow_mut();
                match state.cooling.pop_front() {
                    Some(frame) => frame,
                    None => return Ok(None),
                }
            };
            let (page, dirty, pins) = {
                let state = self.state.borrow();
                match state.frames.get(frame as usize) {
                    Some(meta) => (meta.page, meta.dirty, self.pins_of(frame)),
                    None => continue,
                }
            };
            if pins > 0 {
                // Somebody pinned it after it was queued. Put it back to hot
                // rather than evicting under a live reader.
                let mut state = self.state.borrow_mut();
                if let Some(meta) = state.frames.get_mut(frame as usize) {
                    meta.state = FrameState::Hot;
                }
                continue;
            }
            if dirty {
                self.writeback(frame, page)?;
            }
            // TDD invariant 5: a frame is never reused while a swizzled swip
            // still names it. The only thing that can name it is the parent
            // recorded on the way in, and cooling put a page id back there.
            debug_assert!(
                self.state
                    .borrow()
                    .frames
                    .get(frame as usize)
                    .map(|meta| meta.parent.is_none())
                    .unwrap_or(true),
                "a frame was evicted with a parent still pointing at it"
            );
            let mut state = self.state.borrow_mut();
            state.table.remove(&page);
            if let Some(meta) = state.frames.get_mut(frame as usize) {
                *meta = FrameMeta::empty();
            }
            drop(state);
            // The frame no longer holds the page a descent may have observed.
            if let Some(latch) = self.latch(frame) {
                if latch.try_exclusive() {
                    latch.release_exclusive();
                }
            }
            Counters::add(&self.counters.evicted, 1);
            return Ok(Some(frame));
        }
    }

    /// Writes one frame to the file with every swizzled swip translated back.
    ///
    /// The translation is done on a copy so the resident frame keeps its
    /// swizzled swips; unswizzling a page because it was written would throw
    /// away the descent work for nothing.
    ///
    /// @param frame - the frame to write
    /// @param page - the page it holds
    fn writeback(&self, frame: u32, page: PageId) -> DbResult<()> {
        // The write-ahead rule, and the only place in the engine it is
        // enforced. Phase 2 said this seam was here and that Phase 3 would add
        // "a condition rather than a caller"; this is that condition. Every
        // page write in the engine funnels through this function - the
        // checkpointer's flush, an eviction, a manual flush - so a page cannot
        // reach the data file by a route that skips it.
        self.refuse_if_ahead_of_the_log(frame, page)?;
        let mut image = {
            let bytes = self
                .buffers
                .get(frame as usize)
                .ok_or_else(|| misuse("frame index out of range"))?
                .try_borrow()
                .map_err(|_| misuse("a frame chosen for writeback was mutably borrowed"))?;
            bytes.clone()
        };
        let translated = self.translate_swips(&mut image)?;
        page::checksum_page(&mut image)?;
        self.file
            .write_all_at(page.0.saturating_mul(self.page_size as u64), &image)
            .map_err(|error| error.into_db_error())?;
        let mut state = self.state.borrow_mut();
        if let Some(meta) = state.frames.get_mut(frame as usize) {
            meta.dirty = false;
        }
        drop(state);
        Counters::add(&self.counters.writes, 1);
        Counters::add(&self.counters.translated, translated as u64);
        Ok(())
    }

    /// Refuses a writeback the log has not caught up with.
    ///
    /// Reads the LSN out of the page's own header rather than out of any
    /// bookkeeping beside it, because the header is what the file will hold and
    /// bookkeeping is what can drift from it. A page whose LSN is at or above
    /// the durable watermark describes a change whose log record is not on the
    /// media, and writing it would mean a crash could leave the data file ahead
    /// of the log with no way back.
    ///
    /// @param frame - the frame about to be written
    /// @param page - the page it holds, for the message
    fn refuse_if_ahead_of_the_log(&self, frame: u32, page: PageId) -> DbResult<()> {
        let durable = self.durable_lsn.get();
        if durable == u64::MAX {
            return Ok(());
        }
        let lsn = {
            let bytes = self
                .buffers
                .get(frame as usize)
                .ok_or_else(|| misuse("frame index out of range"))?
                .try_borrow()
                .map_err(|_| misuse("a frame chosen for writeback was mutably borrowed"))?;
            page::read_u64(&bytes, page::header::LSN)?
        };
        if lsn > durable {
            return Err(misuse(format!(
                "page {} carries lsn {lsn} and the log is durable to {durable}: \
                 writing it would put the data file ahead of the log",
                page.0
            )));
        }
        Ok(())
    }

    /// Pins a frame and borrows its bytes.
    ///
    /// @param frame - the frame to borrow
    fn borrow_frame(&self, frame: u32) -> DbResult<PageGuard<'_>> {
        let cell = self
            .buffers
            .get(frame as usize)
            .ok_or_else(|| misuse(format!("frame {frame} does not exist")))?;
        let bytes = cell
            .try_borrow()
            .map_err(|_| misuse(format!("frame {frame} is being written")))?;
        // The pin is a `Cell`, so the ordinary fetch - a resident, hot frame -
        // never borrows the pool's bookkeeping at all. Only a frame that was
        // cooling has to, to take itself out of the queue.
        if let Some(slot) = self.pins.get(frame as usize) {
            slot.set(slot.get().saturating_add(1));
        }
        if self.frame_is_cooling(frame) {
            let mut state = self.state.borrow_mut();
            if let Some(meta) = state.frames.get_mut(frame as usize) {
                meta.state = FrameState::Hot;
            }
            state.cooling.retain(|held| *held != frame);
        }
        Ok(PageGuard {
            pool: self,
            frame,
            bytes,
        })
    }

    /// Drops one pin.
    ///
    /// @param frame - the frame the guard held
    fn unpin(&self, frame: u32) {
        if let Some(slot) = self.pins.get(frame as usize) {
            slot.set(slot.get().saturating_sub(1));
        }
    }

    /// Reports whether a frame is queued for eviction.
    ///
    /// @param frame - the frame's index
    fn frame_is_cooling(&self, frame: u32) -> bool {
        self.state
            .borrow()
            .frames
            .get(frame as usize)
            .map(|meta| meta.state == FrameState::Cooling)
            .unwrap_or(false)
    }

    /// Reports whether a frame's parent is pinned, so its swip cannot be
    /// rewritten.
    ///
    /// @param parent - the parent reference, if any
    fn parent_is_pinned(&self, parent: Option<(u32, PageId, usize)>) -> bool {
        match parent {
            Some((frame, _, _)) => self.pins_of(frame) > 0,
            None => false,
        }
    }

    /// Puts a page image into the pool, marked dirty.
    ///
    /// This is how the bulk loader publishes a page it built: the pool owns it
    /// from that moment, so the loader never writes the file itself and the
    /// write-ahead rule has one place to live.
    ///
    /// @param page - the page id
    /// @param image - the page bytes, exactly one page long
    pub fn install(&self, page: PageId, image: &[u8]) -> DbResult<()> {
        if image.len() != self.page_size {
            return Err(misuse(format!(
                "a page image is {} bytes, not {}",
                image.len(),
                self.page_size
            )));
        }
        let frame = match self.lookup(page) {
            Some(frame) => frame,
            None => {
                let frame = self.claim_frame()?;
                let mut state = self.state.borrow_mut();
                if let Some(meta) = state.frames.get_mut(frame as usize) {
                    meta.page = page;
                    meta.state = FrameState::Hot;
                    meta.parent = None;
                }
                if let Some(slot) = self.pins.get(frame as usize) {
                    slot.set(0);
                }
                state.table.insert(page, frame);
                frame
            }
        };
        {
            let mut bytes = self
                .buffers
                .get(frame as usize)
                .ok_or_else(|| misuse("frame index out of range"))?
                .try_borrow_mut()
                .map_err(|_| misuse("a frame being installed into is borrowed"))?;
            bytes.copy_from_slice(image);
        }
        let mut state = self.state.borrow_mut();
        if let Some(meta) = state.frames.get_mut(frame as usize) {
            meta.dirty = true;
        }
        drop(state);
        if page.0 >= self.page_count.get() {
            self.page_count.set(page.0.saturating_add(1));
        }
        Ok(())
    }

    /// Applies a change to a resident page and marks the frame dirty.
    ///
    /// @param page - the page to change
    /// @param change - what to do to its bytes
    pub fn modify<R>(
        &self,
        page: PageId,
        change: impl FnOnce(&mut [u8]) -> DbResult<R>,
    ) -> DbResult<R> {
        let frame = self.resolve(page)?;
        let outcome = {
            let mut bytes = self
                .buffers
                .get(frame as usize)
                .ok_or_else(|| misuse("frame index out of range"))?
                .try_borrow_mut()
                .map_err(|_| misuse(format!("page {} is borrowed for reading", page.0)))?;
            change(bytes.as_mut_slice())?
        };
        let mut state = self.state.borrow_mut();
        if let Some(meta) = state.frames.get_mut(frame as usize) {
            meta.dirty = true;
        }
        Ok(outcome)
    }

    /// Writes every dirty frame to the file.
    ///
    /// Pages go out in page-id order so the write pattern is sequential, which
    /// is the checkpointer's rule and costs nothing to honour here.
    pub fn flush(&self) -> DbResult<usize> {
        let mut dirty: Vec<(PageId, u32)> = {
            let state = self.state.borrow();
            state
                .frames
                .iter()
                .enumerate()
                .filter(|(_, meta)| meta.dirty && meta.state != FrameState::Free)
                .map(|(index, meta)| (meta.page, index as u32))
                .collect()
        };
        dirty.sort_unstable();
        for (page, frame) in &dirty {
            self.writeback(*frame, *page)?;
        }
        Ok(dirty.len())
    }

    /// Writes every dirty frame, then the meta page and its shadow, then syncs.
    ///
    /// The order is the durability order: data first, then the record that says
    /// the data is there. A crash between them leaves the previous meta page
    /// describing a file whose pages are a superset of what it claims, which is
    /// exactly what a checkpoint is allowed to leave behind.
    ///
    /// @param meta - the record to write, with its generation already bumped
    pub fn checkpoint(&self, meta: &Meta) -> DbResult<()> {
        self.flush()?;
        self.file
            .sync(SyncMode::Normal)
            .map_err(|error| error.into_db_error())?;
        let mut image = vec![0u8; self.page_size];
        meta.encode(&mut image)?;
        for slot in [META_PAGE, SHADOW_PAGE] {
            self.file
                .write_all_at(slot.0.saturating_mul(self.page_size as u64), &image)
                .map_err(|error| error.into_db_error())?;
        }
        self.file
            .sync(SyncMode::Normal)
            .map_err(|error| error.into_db_error())?;
        Counters::add(&self.counters.writes, 2);
        Ok(())
    }

    /// Grows the file by one page and returns its id.
    ///
    /// Allocation through the free map is [`crate::file::Database`]'s job; this
    /// is the bottom of it, and it is separate so that a caller that has
    /// already decided a page is free does not consult the map twice.
    pub fn grow(&self) -> PageId {
        let page = PageId(self.page_count.get().max(FIRST_DATA_PAGE.0));
        self.page_count.set(page.0.saturating_add(1));
        page
    }

    /// Tells the pool how many pages the file holds.
    ///
    /// @param pages - the new page count
    pub fn set_page_count(&self, pages: u64) {
        self.page_count.set(pages);
    }

    /// Reads a page straight from the file, bypassing the pool.
    ///
    /// Used for the two meta pages, which are not pool pages: they carry no
    /// common header, they are written twice, and caching them would put the
    /// pool in the way of the one read that has to happen before the pool's own
    /// configuration is known.
    ///
    /// @param page - the page to read
    /// @param output - the buffer to fill
    pub fn read_raw(&self, page: PageId, output: &mut [u8]) -> DbResult<()> {
        self.file
            .read_exact_at(page.0.saturating_mul(self.page_size as u64), output)
            .map_err(|error| error.into_db_error())
    }

    /// Rewrites every swizzled swip in a page image as the page id it names.
    ///
    /// This is TDD invariant 6, and it is the one place it can be enforced: the
    /// frame-to-page mapping lives in the pool, so nothing above it could do
    /// the translation even if it wanted to. Only interior pages carry swips,
    /// which is checked rather than assumed - a leaf whose bytes happened to
    /// resemble a slot array would otherwise be silently rewritten.
    ///
    /// @param image - the page image about to be written
    fn translate_swips(&self, image: &mut [u8]) -> DbResult<usize> {
        if page::kind_of(image)? != PageKind::Interior {
            return Ok(0);
        }
        let offsets = crate::interior::swip_offsets_of(image)?;
        let state = self.state.borrow();
        let mut translated = 0usize;
        for offset in offsets {
            let swip = Swip::from_raw(page::read_u64(image, offset)?);
            let Some(frame) = swip.frame() else {
                continue;
            };
            let page = state
                .frames
                .get(frame as usize)
                .map(|meta| meta.page)
                .ok_or_else(|| corrupt(format!("a swip names frame {frame}, which is not one")))?;
            if page.is_none() {
                return Err(corrupt(format!(
                    "a swip names frame {frame}, which holds no page"
                )));
            }
            page::write_u64(image, offset, Swip::unswizzled(page).raw())?;
            translated = translated.saturating_add(1);
        }
        Ok(translated)
    }

    /// Writes one of the two meta pages straight to the file.
    ///
    /// The meta pages are not pool pages: they carry no common header, they are
    /// written twice, and one of them has to be readable before the pool's own
    /// page size is known. Keeping them off the page table is what stops an
    /// eviction from ever choosing one.
    ///
    /// @param page - [`META_PAGE`] or [`SHADOW_PAGE`]
    /// @param image - the encoded meta record, one page long
    pub fn write_meta_slot(&self, page: PageId, image: &[u8]) -> DbResult<()> {
        if page != META_PAGE && page != SHADOW_PAGE {
            return Err(misuse(format!("page {} is not a meta page", page.0)));
        }
        self.file
            .write_all_at(page.0.saturating_mul(self.page_size as u64), image)
            .map_err(|error| error.into_db_error())?;
        Counters::add(&self.counters.writes, 1);
        Ok(())
    }

    /// Returns one frame's version latch.
    ///
    /// @param frame - the frame's index
    pub fn latch(&self, frame: u32) -> Option<&VersionLatch> {
        self.latches.get(frame as usize)
    }

    /// Begins an optimistic read of a frame.
    ///
    /// @param frame - the frame's index
    pub fn observe(&self, frame: u32) -> Option<Observed> {
        self.latches
            .get(frame as usize)
            .and_then(VersionLatch::optimistic)
    }

    /// Reports whether an optimistic read of a frame is still valid.
    ///
    /// @param frame - the frame's index
    /// @param observed - what [`Pool::observe`] returned
    pub fn validate(&self, frame: u32, observed: Observed) -> bool {
        self.latches
            .get(frame as usize)
            .map(|latch| latch.validate(observed))
            .unwrap_or(false)
    }

    /// Reports whether the free watermark says the clock should run.
    pub fn under_watermark(&self) -> bool {
        let state = self.state.borrow();
        let free = state.free.len().saturating_add(state.cooling.len());
        (free as f64) < (self.buffers.len() as f64) * FREE_WATERMARK
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use inillucent_vfs::{DbPath, MemoryVfs, OpenOptions, Vfs};

    /// Returns a pool over a memory file of `pages` zeroed, checksummed pages.
    fn pool_over(page_size: usize, frames: usize, pages: u64) -> Pool {
        let vfs = MemoryVfs::new();
        let path = DbPath::new("pool-test.rdb");
        let file = vfs.open(&path, OpenOptions::main_db()).unwrap();
        for page in 0..pages {
            let mut image = vec![0u8; page_size];
            page::write_common(&mut image, PageKind::Leaf, 0, 1).unwrap();
            page::write_u64(&mut image, 32, page).unwrap();
            page::checksum_page(&mut image).unwrap();
            file.write_all_at(page * page_size as u64, &image).unwrap();
        }
        Pool::new(file, page_size, frames, pages).unwrap()
    }

    /// A fetch of an absent page reads it; a second fetch does not.
    #[test]
    fn a_second_fetch_is_a_hit() {
        let pool = pool_over(512, 8, 10);
        {
            let guard = pool.fetch(PageId(3)).unwrap();
            assert_eq!(page::read_u64(&guard, 32).unwrap(), 3);
        }
        assert_eq!(pool.stats().misses, 1);
        assert_eq!(pool.stats().reads, 1);
        {
            let _guard = pool.fetch(PageId(3)).unwrap();
        }
        assert_eq!(pool.stats().misses, 1, "the second fetch read nothing");
        assert_eq!(pool.stats().hits, 1);
    }

    /// Two guards on one page coexist, and the pin count returns to zero.
    #[test]
    fn two_guards_on_one_page_coexist() {
        let pool = pool_over(512, 8, 10);
        let first = pool.fetch(PageId(4)).unwrap();
        let second = pool.fetch(PageId(4)).unwrap();
        assert_eq!(first.frame(), second.frame());
        drop(first);
        drop(second);
        let frame = pool.state.borrow().table[&PageId(4)];
        assert_eq!(pool.pins_of(frame), 0);
    }

    /// A pool smaller than the working set evicts, and every page still reads
    /// correctly afterwards. This is the eviction campaign in miniature; the
    /// 64-frame version lives in `tests/`.
    #[test]
    fn a_small_pool_evicts_and_stays_correct() {
        let pool = pool_over(512, 4, 40);
        for round in 0..3 {
            for page in 2..40u64 {
                let guard = pool.fetch(PageId(page)).unwrap();
                assert_eq!(
                    page::read_u64(&guard, 32).unwrap(),
                    page,
                    "round {round} page {page}"
                );
            }
        }
        assert!(pool.stats().evicted > 0, "nothing was evicted");
        assert!(pool.resident() <= 4);
    }

    /// A frame every caller has pinned cannot be evicted, and the pool says so
    /// rather than corrupting one.
    #[test]
    fn a_fully_pinned_pool_refuses_to_evict() {
        let pool = pool_over(512, 2, 10);
        let _a = pool.fetch(PageId(2)).unwrap();
        let _b = pool.fetch(PageId(3)).unwrap();
        let error = pool.fetch(PageId(4)).unwrap_err();
        assert!(error.detail().unwrap_or("").contains("pinned"), "{error:?}");
    }

    /// A dirty page survives eviction: it is written back and read again.
    #[test]
    fn a_dirty_page_is_written_back_before_it_is_evicted() {
        let pool = pool_over(512, 2, 12);
        pool.modify(PageId(5), |bytes| page::write_u64(bytes, 40, 0xABCD))
            .unwrap();
        for page in 6..12u64 {
            let _ = pool.fetch(PageId(page)).unwrap();
        }
        assert!(pool.stats().writes > 0, "nothing was written back");
        let guard = pool.fetch(PageId(5)).unwrap();
        assert_eq!(page::read_u64(&guard, 40).unwrap(), 0xABCD);
    }

    /// A page installed by the loader is dirty, readable, and flushed.
    #[test]
    fn an_installed_page_is_dirty_and_flushes() {
        let pool = pool_over(512, 8, 4);
        let mut image = vec![0u8; 512];
        page::write_common(&mut image, PageKind::Leaf, 0, 9).unwrap();
        page::write_u64(&mut image, 32, 777).unwrap();
        pool.install(PageId(6), &image).unwrap();
        assert_eq!(pool.page_count(), 7);
        assert_eq!(pool.flush().unwrap(), 1);
        // Reading it back through a fresh pool proves the checksum was written.
        let guard = pool.fetch(PageId(6)).unwrap();
        assert_eq!(page::read_u64(&guard, 32).unwrap(), 777);
        assert!(pool.install(PageId(7), &[0u8; 8]).is_err());
    }

    /// The cooling FIFO takes frames, and a fetch of a cooling page rewarms it
    /// without any I/O.
    #[test]
    fn a_cooling_page_rewarms_without_a_read() {
        let pool = pool_over(512, 16, 20);
        for page in 2..10u64 {
            let _ = pool.fetch(PageId(page)).unwrap();
        }
        let reads_before = pool.stats().reads;
        let cooled = pool.cool().unwrap();
        assert!(cooled > 0, "nothing cooled");
        let cooling: Vec<u32> = pool.state.borrow().cooling.iter().copied().collect();
        let frame = cooling[0];
        let page = pool.state.borrow().frames[frame as usize].page;
        assert_eq!(pool.frame_state(frame), Some(FrameState::Cooling));
        let _guard = pool.fetch(page).unwrap();
        assert_eq!(pool.frame_state(frame), Some(FrameState::Hot));
        assert_eq!(pool.stats().reads, reads_before, "a rewarm read nothing");
        assert!(pool.stats().rewarms > 0);
    }

    /// A corrupt page is refused rather than returned.
    #[test]
    fn a_corrupt_page_is_refused() {
        let vfs = MemoryVfs::new();
        let path = DbPath::new("corrupt.rdb");
        let file = vfs.open(&path, OpenOptions::main_db()).unwrap();
        let mut image = vec![0u8; 512];
        page::write_common(&mut image, PageKind::Leaf, 0, 1).unwrap();
        page::checksum_page(&mut image).unwrap();
        image[100] ^= 0xFF;
        file.write_all_at(2 * 512, &image).unwrap();
        let pool = Pool::new(file, 512, 4, 3).unwrap();
        let error = pool.fetch(PageId(2)).unwrap_err();
        assert!(
            error.detail().unwrap_or("").contains("checksum"),
            "{error:?}"
        );
    }

    /// An observation of a frame is invalidated when the frame is loaded over,
    /// which is the whole reason a descent validates rather than trusting what
    /// it read.
    #[test]
    fn loading_over_a_frame_invalidates_an_observation() {
        let pool = pool_over(512, 1, 8);
        let frame = {
            let guard = pool.fetch(PageId(2)).unwrap();
            guard.frame()
        };
        let observed = pool.observe(frame).expect("a free frame admits a reader");
        assert!(pool.validate(frame, observed));
        // One frame, so fetching a different page must reuse this one.
        let _ = pool.fetch(PageId(3)).unwrap();
        assert!(
            !pool.validate(frame, observed),
            "the frame holds a different page and said so"
        );
        assert!(pool.observe(99).is_none());
        assert!(pool.latch(99).is_none());
    }

    /// A pool with no frames or no page size is a misuse, not a panic.
    #[test]
    fn a_degenerate_pool_is_refused() {
        let vfs = MemoryVfs::new();
        let path = DbPath::new("degenerate.rdb");
        let file = vfs.open(&path, OpenOptions::main_db()).unwrap();
        assert!(Pool::new(file, 512, 0, 0).is_err());
        let file = vfs.open(&path, OpenOptions::main_db()).unwrap();
        assert!(Pool::new(file, 0, 4, 0).is_err());
    }

    /// The reported size is frames times the page size, and the counters reset.
    #[test]
    fn the_pool_reports_its_own_size() {
        let pool = pool_over(512, 6, 8);
        assert_eq!(pool.frames(), 6);
        assert_eq!(pool.page_size(), 512);
        assert_eq!(pool.byte_size(), 6 * 512);
        let _ = pool.fetch(PageId(2)).unwrap();
        assert!(pool.stats().reads > 0);
        pool.reset_stats();
        assert_eq!(pool.stats(), PoolStats::default());
        assert!(pool.is_resident(PageId(2)));
        assert!(!pool.is_resident(PageId(7)));
        assert_eq!(pool.frame_state(99), None);
    }

    /// Growing hands out the page after the last one and moves the count.
    #[test]
    fn growing_hands_out_the_next_page() {
        let pool = pool_over(512, 4, 5);
        assert_eq!(pool.grow(), PageId(5));
        assert_eq!(pool.page_count(), 6);
        pool.set_page_count(2);
        assert_eq!(pool.grow(), PageId(2));
    }

    /// A checkpoint writes the meta page and its shadow, and both decode.
    #[test]
    fn a_checkpoint_writes_both_meta_pages() {
        let pool = pool_over(512, 4, 6);
        let mut meta = Meta::fresh(512, 1234);
        meta.page_count = 6;
        meta.generation = 2;
        pool.checkpoint(&meta).unwrap();
        let mut primary = vec![0u8; 512];
        let mut shadow = vec![0u8; 512];
        pool.read_raw(META_PAGE, &mut primary).unwrap();
        pool.read_raw(SHADOW_PAGE, &mut shadow).unwrap();
        assert_eq!(Meta::choose(&primary, &shadow).unwrap(), meta);
    }

    /// The watermark reports low only when the free and cooling frames really
    /// are below the fraction.
    #[test]
    fn the_watermark_reports_a_full_pool() {
        let pool = pool_over(512, 4, 8);
        assert!(!pool.under_watermark(), "a fresh pool is all free");
        for page in 2..6u64 {
            let _ = pool.fetch(PageId(page)).unwrap();
        }
        assert!(pool.under_watermark());
    }
}
