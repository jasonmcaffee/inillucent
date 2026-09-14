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
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use inillucent_base::error::{corrupt, misuse, no_mem};
use inillucent_base::rng::Rng;
use inillucent_base::DbResult;
use inillucent_vfs::{FileLock, SyncMode, VfsFile};

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

/// Why a page is being written to the file.
///
/// **No-steal is a rule about the checkpointer, and only the checkpointer can
/// obey it by doing nothing.** A checkpoint that leaves an open transaction's
/// page out of the file has lost nothing: the frame is still resident and
/// still dirty, and the checkpoint after that transaction ends writes it. An
/// eviction has no such option, because the frame it would have written from
/// is about to hold a different page - so the same skip there does not hold
/// the page back, it throws it away.
///
/// That is what page 597 of `new_engine_log_lead` was. A `CREATE INDEX`
/// through a 64-frame pool evicted 129 dirty pages during the build, every one
/// of them skipped by no-steal and then freed, and the file was left with a
/// hole of never-written zeros where the index's own pages should have been -
/// `page 597 checksum 00000000 is not the computed 8d1053d3`, which is the
/// checksum of a page of zeros. The build reported success; the `SELECT` after
/// it did not.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Writing {
    /// A checkpoint's flush, which may leave a page for the next checkpoint.
    Checkpoint,
    /// An eviction, which either writes the page or keeps the frame.
    Eviction,
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
    /// Where recovery has to start to rebuild this page, when it is dirty.
    ///
    /// The LSN the page carried when it first became dirty after its last
    /// writeback, and `u64::MAX` when it is clean. See `Pool::note_dirty_from`
    /// for why a checkpoint cannot do without it.
    rec_lsn: u64,
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
            rec_lsn: u64::MAX,
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

/// How long a lock request waits before it reports the file as busy.
///
/// SQLite's own default is zero - it reports `SQLITE_BUSY` at once and leaves
/// the waiting to a busy handler the application installs. This waits by
/// default because an application that has not thought about concurrency is
/// better served by taking turns than by an error it does not handle, and
/// `PRAGMA busy_timeout` moves it either way.
pub(crate) const DEFAULT_BUSY_MILLIS: u64 = 5_000;

/// Raises a lock on a file, waiting up to a budget for the holder to let go.
///
/// **Waiting is the whole of what a busy timeout is.** A lock another process
/// holds is not an error - it is a lock that will be released - and reporting
/// failure immediately would make every concurrent pair of writers fail rather
/// than take turns. The sleep grows so that a long wait is not a spin, and the
/// last attempt reports what it found.
///
/// A free function rather than a `Pool` method so that
/// [`crate::journal::replay_hot_journal`] can escalate a lock on the file it
/// opens for replay with the same retry `Pool::lock_within` uses, instead of
/// a second, independently invented backoff.
///
/// @param file - the file to lock
/// @param level - the level to raise to
/// @param budget_millis - how long to keep trying
pub(crate) fn lock_with_wait(
    file: &dyn VfsFile,
    level: FileLock,
    budget_millis: u64,
) -> DbResult<()> {
    if file.lock_level() >= level {
        return Ok(());
    }
    let mut waited = 0u64;
    let mut pause = 1u64;
    loop {
        match file.lock(level) {
            Ok(()) => return Ok(()),
            Err(error) if waited >= budget_millis => {
                return Err(error.into_db_error());
            }
            Err(_) => {}
        }
        std::thread::sleep(std::time::Duration::from_millis(pause));
        waited = waited.saturating_add(pause);
        pause = pause.saturating_mul(2).min(50);
    }
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
    /// The frame buffers, each allocated the first time its frame is claimed
    /// and never resized after that.
    ///
    /// **Empty until claimed, which is what keeps a pool a budget rather than a
    /// reservation.** Allocating and zeroing every frame at open made the pool's
    /// configured size the process's resident set from the first statement:
    /// a 128 MiB pool was measured as 128 MiB of resident memory against
    /// SQLite's 37 MiB at the same `cache_size`, on a fixture that only ever
    /// touched a quarter of it. SQLite grows into its cache and so does this
    /// now. A frame is sized once, in `claim_frame`, and an evicted frame keeps
    /// its buffer for the next page, so the steady state costs nothing.
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
    /// The rollback journal, when `PRAGMA journal_mode` selected one.
    ///
    /// `None` is the write-ahead log, which is the default and costs a branch
    /// per page write. See [`crate::journal`].
    journal: RefCell<Option<crate::journal::Journal>>,
    /// How many pages the file holds.
    page_count: Cell<u64>,
    /// The most pages the pool holds at once, which `PRAGMA cache_size` sets.
    ///
    /// The whole pool by default, so the setting costs nothing until it is
    /// used. See [`Pool::set_budget`].
    budget: Cell<usize>,
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
    /// The lowest LSN a checkpoint has ever persisted as this file's recovery
    /// point, which is the lowest LSN `retire_segments_below` has ever been
    /// asked to keep.
    ///
    /// **Not `durable_lsn`, and the difference is the whole fix.** `durable_lsn`
    /// advances on every sync of the log - including the sync a commit does for
    /// its own record, which happens before that commit's own change reaches
    /// this pool's `modify`. Using it as `note_dirty_from`'s floor would clamp
    /// a page's `rec_lsn` up past the very record that just dirtied it, so a
    /// later checkpoint that has to hold the page back (because a *different*,
    /// still-open transaction touched it too) would believe that record no
    /// longer needs replay - the record is not in the file (correctly held
    /// back) and now not replayed either, which is the data loss this field
    /// exists to prevent rather than the one `checkpoint.rs`'s module comment
    /// describes.
    ///
    /// This field only moves when a checkpoint actually persists a new
    /// recovery point and retires segments against it, so it names exactly
    /// what is physically still guaranteed to be on disk - never more.
    /// `u64::MAX` means no checkpoint has run yet in this pool's lifetime
    /// (a fresh build, or before `recovery::open_file` seeds it from the
    /// file's own last checkpoint), which disables the clamp rather than
    /// asserting a floor nothing has earned.
    retained_lsn: Cell<u64>,
    /// The highest LSN this pool has written into the data file.
    ///
    /// **A page's stamp has to be a position in the stream beside the file, and
    /// this is the number that keeps it one.** Recovery applies a
    /// record to a page only when the page's stamp is below the record's, so a
    /// page carrying a stamp from a stream that no longer exists silently
    /// swallows every later write to it. The checkpoint records this in the
    /// meta page and the next open resumes the log above it.
    ///
    /// It is collected here rather than beside the writeback's caller because
    /// [`Pool::writeback`] is the only route a page takes to the file - the
    /// checkpointer's flush, an eviction and a manual flush all funnel through
    /// it, which is exactly the argument that makes the write-ahead rule
    /// enforceable in one place.
    high_water_lsn: Cell<u64>,
    /// What to call when a page the pool must write is ahead of the log.
    ///
    /// `None` means there is nothing to ask, which is a read-only open and a
    /// bulk build with no log.
    #[allow(clippy::type_complexity)]
    advance_log: RefCell<Option<std::rc::Rc<dyn Fn() -> DbResult<u64>>>>,
    /// The LSN at or above which a page's change belongs to a transaction that
    /// has not committed.
    ///
    /// No-steal: such a page does not go to the file, because recovery is
    /// redo-only and a page written before its commit can never be taken back
    /// out. `u64::MAX` means no transaction is open, which is the state between
    /// transactions and the state of a database with no log at all.
    ///
    /// Shared rather than owned, so that the transaction manager can move the
    /// watermark **while the file is mutably borrowed** - which is exactly when
    /// it needs to, because a transaction's first log record is written from
    /// inside the tree mutation that is holding the file. A `Cell` here meant
    /// reaching the pool through the file, and reaching the file was the one
    /// thing that could not be done at that moment.
    uncommitted_lsn: Arc<AtomicU64>,
    /// Whether an eviction has written a page an open transaction had changed
    /// since the current rollback journal was started.
    ///
    /// **What makes such a write undoable is the journal, so the journal has to
    /// outlive the transaction that needed it.** A checkpoint disposes of the
    /// journal once its own meta record is durable, which is right when every
    /// page in the file belongs to a committed transaction. It is wrong while a
    /// writer is still open and an eviction has already put one of that
    /// writer's pages in the file: deleting the pre-images there would leave a
    /// crash with an uncommitted page it could neither replay away - recovery
    /// is redo-only and discards a loser rather than undoing it - nor put back.
    /// So [`Pool::checkpoint`] keeps the journal while this is set and the
    /// writer is still open, and [`Pool::finish_journal`] clears it.
    stolen: Cell<bool>,
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
    /// Writebacks skipped because an open transaction had changed the page.
    held_back: Cell<u64>,
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
            // Empty: `claim_frame` gives a frame its page-sized buffer the
            // first time the frame is used. See the field's own note.
            buffers.push(RefCell::new(Vec::new()));
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
            journal: RefCell::new(None),
            page_count: Cell::new(page_count),
            // The whole pool until `PRAGMA cache_size` says otherwise.
            budget: Cell::new(frames.max(1)),
            counters: Counters::default(),
            // No log until a caller says otherwise, so nothing is refused. A
            // bulk build and a read-only open both run this way, and both are
            // correct to: a page cannot be ahead of a log that does not exist.
            durable_lsn: Cell::new(u64::MAX),
            // No checkpoint has run yet in this pool's lifetime, so nothing is
            // floored - see the field's own doc comment.
            retained_lsn: Cell::new(u64::MAX),
            // Nothing has been written yet, and zero is what the meta page
            // means by "no high water recorded".
            high_water_lsn: Cell::new(0),
            advance_log: RefCell::new(None),
            // Nothing is uncommitted until a transaction says so.
            uncommitted_lsn: Arc::new(AtomicU64::new(u64::MAX)),
            stolen: Cell::new(false),
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

    /// Records the lowest LSN a checkpoint has persisted as this file's
    /// recovery point - the floor [`Pool::note_dirty_from`] clamps a newly
    /// dirtied page's `rec_lsn` against.
    ///
    /// A caller with a log calls this from the same place it calls
    /// [`Pool::set_durable_lsn`] during a checkpoint - `recovery_from`, not
    /// `durable`, because `durable` can already include a record this exact
    /// call is about to dirty a page with. It also has to be called once at
    /// open, from the file's own `meta.checkpoint_lsn`, or a page whose stale
    /// stamp predates a checkpoint from a *previous* session would repeat the
    /// bug across a reopen instead of within one.
    ///
    /// @param lsn - the checkpoint's own recovery point
    pub fn set_retained_lsn(&self, lsn: u64) {
        self.retained_lsn.set(lsn);
    }

    /// Registers what the pool may call when a page it must write is ahead of
    /// the log.
    ///
    /// **The pool cannot flush the log and must not learn how.** It does not
    /// know `inillucent-wal` exists - the layering invariant says so, and the log
    /// is written before any page is, so an edge from the pool to the log would
    /// point the wrong way. But refusing the write is not a correct answer
    /// either: a statement that dirties more pages than the pool holds has to
    /// evict, and every candidate it has carries an LSN the log has not reached
    /// yet, so the statement fails through no fault of its own.
    ///
    /// A closure is the seam. The pool asks "make the log durable and tell me
    /// how far it got"; whoever registered it knows what a log is. The guard
    /// below still refuses if the answer is not far enough, so the write-ahead
    /// rule is enforced by the same check it always was - the caller simply now
    /// gets a chance to satisfy it.
    ///
    /// @param advance - makes the log durable and returns its new durable point
    ///
    /// An `Rc` rather than a `Box` so that [`Pool::refuse_if_ahead_of_the_log`]
    /// can take a handle to it and drop its borrow before calling it. A `Box`
    /// would have to be moved out of the cell and put back, which loses the
    /// closure on the error path the call is most likely to take.
    #[allow(clippy::type_complexity)]
    pub fn on_log_behind(&self, advance: std::rc::Rc<dyn Fn() -> DbResult<u64>>) {
        *self.advance_log.borrow_mut() = Some(advance);
    }

    /// Returns the write-ahead watermark, or `u64::MAX` when there is no log.
    pub fn durable_lsn(&self) -> u64 {
        self.durable_lsn.get()
    }

    /// Returns the highest LSN this pool has written into the data file.
    ///
    /// Zero when it has written no stamped page, which is what the meta record
    /// means by "unset" - see [`crate::meta::Meta::high_water_lsn`].
    pub fn high_water_lsn(&self) -> u64 {
        self.high_water_lsn.get()
    }

    /// Raises the high water to at least `lsn`.
    ///
    /// Used by a caller that has read a stamp the pool did not write - an open
    /// that folds the meta page's recorded high water back in, so a run which
    /// writes nothing does not report a lower number than the run before it.
    ///
    /// @param lsn - a stamp the file is known to carry
    pub fn note_high_water_lsn(&self, lsn: u64) {
        self.high_water_lsn.set(self.high_water_lsn.get().max(lsn));
    }

    /// Returns the page size in bytes.
    pub fn page_size(&self) -> usize {
        self.page_size
    }

    /// Returns how many frames the pool holds.
    pub fn frames(&self) -> usize {
        self.buffers.len()
    }

    /// Returns how many pages the pool will hold at once.
    ///
    /// The whole pool unless `PRAGMA cache_size` asked for less.
    pub fn budget(&self) -> usize {
        self.budget.get()
    }

    /// Caps how many pages the pool holds at once.
    ///
    /// Clamped into `1..=frames()`: a budget of nothing cannot read a page, and
    /// one larger than the pool is the pool. Growing past the frames the pool
    /// was opened with is not possible - the buffers are allocated once - so a
    /// caller asking for more is told the truth by reading the value back.
    ///
    /// @param pages - how many pages to hold at once
    pub fn set_budget(&self, pages: usize) {
        self.budget.set(pages.clamp(1, self.buffers.len().max(1)));
    }

    /// Adds frames, so a pool can hold more pages than it was opened with.
    ///
    /// **A frame costs a latch, a pin counter and an empty `Vec` until it is
    /// claimed**, which is what makes this cheap enough to be a pragma rather
    /// than a reopen: `claim_frame` gives a frame its page-sized buffer the
    /// first time the frame is used, so growing to sixty-five thousand frames
    /// allocates bookkeeping rather than two gigabytes.
    ///
    /// Nothing that exists moves. The new frames go on the end, every frame
    /// index already handed out still names the same buffer, and the free list
    /// gains the new indices - so a page that is resident stays resident and a
    /// descent holding a frame number is unaffected.
    ///
    /// Never shrinks: a smaller cache is what [`Pool::set_budget`] is for, and
    /// removing a frame would mean evicting whatever is in it while a caller
    /// may be holding it.
    ///
    /// @param frames - how many frames the pool should hold in total
    pub fn grow_frames(&mut self, frames: usize) -> DbResult<()> {
        let held = self.buffers.len();
        if frames <= held {
            return Ok(());
        }
        let more = frames.saturating_sub(held);
        self.buffers
            .try_reserve(more)
            .map_err(|_| no_mem(format!("{more} more frames of {} bytes", self.page_size)))?;
        self.latches
            .try_reserve(more)
            .map_err(|_| no_mem(format!("{more} more frame latches")))?;
        self.pins
            .try_reserve(more)
            .map_err(|_| no_mem(format!("{more} more pin counters")))?;
        for _ in 0..more {
            self.buffers.push(RefCell::new(Vec::new()));
            self.latches.push(VersionLatch::new());
            self.pins.push(Cell::new(0));
        }
        {
            let mut state = self.state.borrow_mut();
            state.frames.resize(frames, FrameMeta::empty());
            // Pushed in reverse, the way `new` builds the list, so the next
            // claim takes the lowest new index.
            for index in (held..frames).rev() {
                state.free.push(index as u32);
            }
        }
        Ok(())
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
    ///
    /// The one place a frame is handed out for a page it does not yet hold, and
    /// therefore the one place its buffer has to exist by. Every caller - the
    /// read path through `fill_frame`, and `install` for a page built in memory
    /// - comes through here.
    fn claim_frame(&self) -> DbResult<u32> {
        let frame = self.take_frame()?;
        self.give_the_frame_a_buffer(frame)?;
        Ok(frame)
    }

    /// Returns a free frame's index, cooling and evicting to get one.
    ///
    /// **The budget is consulted before the free list.** `PRAGMA cache_size` is
    /// a ceiling on how many database pages are held in memory - that is
    /// SQLite's own definition of it - so a pool asked for a smaller cache
    /// stops taking fresh frames once that many are resident and re-uses one
    /// instead. The allocated buffers stay allocated, which is what SQLite's
    /// page cache does too; what the setting bounds is the pages, and that is
    /// what this bounds.
    ///
    /// With no `cache_size` set the budget is the whole pool, so the first
    /// branch is the only one taken until the pool is full - the same path, and
    /// the same cost, as before the budget existed.
    fn take_frame(&self) -> DbResult<u32> {
        let budget = self.budget.get();
        {
            let mut state = self.state.borrow_mut();
            let resident = self.buffers.len().saturating_sub(state.free.len());
            if resident < budget {
                if let Some(frame) = state.free.pop() {
                    return Ok(frame);
                }
            }
        }
        self.cool()?;
        if let Some(frame) = self.evict_one()? {
            return Ok(frame);
        }
        // Over budget with nothing evictable - every resident page is pinned.
        // The budget is a ceiling on caching, not a wall the statement runs
        // into, so a frame the pool owns and is not using is better than a
        // refusal.
        if let Some(frame) = self.state.borrow_mut().free.pop() {
            return Ok(frame);
        }
        // Nothing is evictable, and the two reasons for that are different
        // enough to the caller that they are told apart. A pool whose frames
        // are all pinned is a caller holding too many guards at once. A pool
        // whose frames all hold pages an open transaction has changed, with no
        // rollback journal to undo an eviction from, is the documented limit of
        // a no-steal policy: such a page may not reach the file before its
        // commit, and it may not be dropped either, so the transaction cannot
        // dirty more pages than the pool holds. Selecting a journal mode that
        // keeps pre-images, or a larger `PRAGMA cache_size`, is what lifts it.
        let held = self.frames_no_steal_is_holding();
        if held > 0 {
            return Err(no_mem(format!(
                "the open transaction has changed {held} of the buffer pool's {} pages, and no \
                 rollback journal is in force to undo an eviction from, so none of them may \
                 be written before it commits: a transaction cannot dirty more pages than \
                 the pool holds",
                self.buffers.len()
            )));
        }
        Err(no_mem(
            "every frame in the buffer pool is pinned; nothing can be evicted",
        ))
    }

    /// Returns how many resident frames hold a page no-steal will not let go.
    ///
    /// Only asked when the pool has nothing to give, so that the refusal names
    /// the reason it is refusing rather than the first reason anybody wrote a
    /// message for.
    fn frames_no_steal_is_holding(&self) -> usize {
        if self.can_undo_a_steal() {
            return 0;
        }
        let uncommitted = self.uncommitted_lsn.load(Ordering::SeqCst);
        if uncommitted == u64::MAX {
            return 0;
        }
        let dirty: Vec<u32> = {
            let state = self.state.borrow();
            state
                .frames
                .iter()
                .enumerate()
                .filter(|(_, meta)| meta.dirty && meta.state != FrameState::Free)
                .map(|(index, _)| index as u32)
                .collect()
        };
        dirty
            .into_iter()
            .filter(|frame| self.lsn_of(*frame).is_ok_and(|lsn| lsn >= uncommitted))
            .count()
    }

    /// Makes sure a claimed frame has a page-sized buffer.
    ///
    /// Costs a length check on every claim and an allocation on the first one
    /// per frame; a pool that has been full once never allocates again, because
    /// an evicted frame keeps its buffer.
    ///
    /// @param frame - the frame just claimed
    fn give_the_frame_a_buffer(&self, frame: u32) -> DbResult<()> {
        let cell = self
            .buffers
            .get(frame as usize)
            .ok_or_else(|| misuse("frame index out of range"))?;
        let mut bytes = cell
            .try_borrow_mut()
            .map_err(|_| misuse("a frame chosen for a new page was still borrowed"))?;
        if bytes.len() == self.page_size {
            return Ok(());
        }
        // `try_reserve` rather than `resize`, so a pool that cannot grow says
        // so as an error instead of aborting the process.
        let wanted = self.page_size.saturating_sub(bytes.len());
        bytes
            .try_reserve_exact(wanted)
            .map_err(|_| no_mem(format!("a frame of {} bytes", self.page_size)))?;
        bytes.resize(self.page_size, 0);
        Ok(())
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

    /// Forgets every back-reference that points into a page being rewritten.
    ///
    /// A child records *where in its parent* its swip lives, so that eviction
    /// can put a page id back there. That offset is only meaningful for the
    /// layout the parent had when the child was swizzled - and a split rewrites
    /// its parent with one more separator and one more child, which moves every
    /// slot after the insertion point.
    ///
    /// The existing guard checks that the parent's *frame* still holds the
    /// parent's *page*, which is true throughout: it is the same page, rewritten
    /// in place. So without this, evicting a child after a split writes eight
    /// bytes of page id into whatever the new layout put at the old offset. The
    /// symptom was an interior page whose seventh key claimed to start at byte
    /// 23, which is inside the header.
    ///
    /// Called only for interior pages, because only an interior page is ever a
    /// parent - so the bulk builder's leaf installs do not pay for the sweep.
    ///
    /// @param page - the page whose layout is about to change
    fn forget_children_of(&self, page: PageId) {
        let mut state = self.state.borrow_mut();
        for meta in state.frames.iter_mut() {
            if let Some((_, parent_page, _)) = meta.parent {
                if parent_page == page {
                    meta.parent = None;
                }
            }
        }
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
        //
        // The page check is necessary and, on its own, **not sufficient**: it
        // catches a frame reused for a different page and misses the same page
        // rewritten with a different layout, where `at` now points at some other
        // field. A split does exactly that to a parent, and the symptom was a
        // page id appearing where an interior page's key offset should be -
        // `interior key 7 starts at 23, before the heap`. What closes it is
        // [`Pool::forget_children_of`], called from `install`, which is the one
        // operation that replaces a whole page image.
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
            if dirty && !self.writeback(frame, page, Writing::Eviction)? {
                // No-steal held the page back and there is no durable rollback
                // journal to undo a steal with, so this frame cannot be freed:
                // the frame holds the only copy of the page, and emptying it
                // would lose the change rather than defer it. Back to hot, and
                // on to the next candidate. A pool with nothing else to give
                // then fails the statement in `take_frame`, which is the
                // documented limit of a no-steal policy - a transaction can
                // dirty at most the pool - and is an error rather than a file
                // with a hole in it.
                let mut state = self.state.borrow_mut();
                if let Some(meta) = state.frames.get_mut(frame as usize) {
                    meta.state = FrameState::Hot;
                }
                continue;
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
    /// Returns whether the page reached the file. `false` is no-steal holding
    /// it back, which is a decision only a [`Writing::Checkpoint`] may act on -
    /// see [`Writing`] for why an eviction that acted on it lost the page.
    ///
    /// @param frame - the frame to write
    /// @param page - the page it holds
    /// @param why - a checkpoint's flush, or an eviction
    fn writeback(&self, frame: u32, page: PageId, why: Writing) -> DbResult<bool> {
        // The write-ahead rule, and the only place in the engine it is
        // enforced. Phase 2 said this seam was here and that Phase 3 would add
        // "a condition rather than a caller"; this is that condition. Every
        // page write in the engine funnels through this function - the
        // checkpointer's flush, an eviction, a manual flush - so a page cannot
        // reach the data file by a route that skips it.
        self.refuse_if_ahead_of_the_log(frame, page)?;
        // No-steal: a page an open transaction has changed does not go to the
        // file. It stays dirty, so a later checkpoint - after the transaction
        // ends either way - writes it then.
        //
        // **That sentence is true of a checkpoint and false of an eviction**,
        // which is what `why` is here to tell apart. An evicted frame does not
        // stay dirty, because it does not stay: the page is dropped and the
        // frame is handed to the next caller. So an eviction writes the page
        // instead, and what makes that safe is the rollback journal - the
        // pre-image is saved and synced below, before the new image goes to the
        // file, so a crash before the transaction commits puts the page back.
        // `crate::journal`'s own header already names this case: "a transaction
        // whose dirty pages outgrow the buffer pool evicts, which creates the
        // journal".
        //
        // **A journal whose pre-images do not reach the disk cannot undo the
        // write**, so `memory` and `off` do not get to steal; there the frame
        // is simply not evictable and `evict_one` moves on. Under those two
        // modes the engine has already been told it may lose a half-written
        // checkpoint, so refusing here is the stricter of the two answers, not
        // a new hole.
        if self.holds_uncommitted(frame, page)? {
            if why == Writing::Checkpoint || !self.can_undo_a_steal() {
                Counters::add(&self.counters.held_back, 1);
                return Ok(false);
            }
            self.stolen.set(true);
        }
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
        // **Read before the checksum is recomputed, off the image that is about
        // to reach the file.** The stamp is what a later recovery compares a
        // record's LSN against, so the number recorded here has to be the one
        // the file will carry rather than anything a caller remembers - the
        // same argument `refuse_if_ahead_of_the_log` makes for reading the
        // header rather than the bookkeeping beside it.
        let stamp = page::read_u64(&image, page::header::LSN)?;
        self.high_water_lsn
            .set(self.high_water_lsn.get().max(stamp));
        page::checksum_page(&mut image)?;
        // **The old image goes to the journal before the new one goes to the
        // file**, and this is the one place either happens - so a page cannot
        // reach the file by a route that skipped its pre-image, exactly as it
        // cannot skip the write-ahead rule two lines above. In WAL mode the
        // journal is not a rollback journal and this costs a branch.
        //
        // **And the pre-image is synced here, not once at the head of the
        // checkpoint.** A writeback also reaches this line from the evictor,
        // one page at a time, with no checkpoint around it; and the checkpoint
        // itself used to seal before it had saved anything, so every pre-image
        // it wrote was still in the file's buffers when the page it belonged to
        // was overwritten. `flush` now saves the whole batch before the first
        // page moves, which leaves this call with nothing outstanding on all
        // but the first page - see `Journal::seal`.
        if self.journal_page(page)? {
            self.seal_journal()?;
        }
        self.file
            .write_all_at(page.0.saturating_mul(self.page_size as u64), &image)
            .map_err(|error| error.into_db_error())?;
        let mut state = self.state.borrow_mut();
        if let Some(meta) = state.frames.get_mut(frame as usize) {
            meta.dirty = false;
            meta.rec_lsn = u64::MAX;
        }
        drop(state);
        Counters::add(&self.counters.writes, 1);
        Counters::add(&self.counters.translated, translated as u64);
        Ok(true)
    }

    /// Reports whether a page written before its transaction committed could be
    /// put back after a crash.
    ///
    /// Which is to say: whether a rollback journal whose pre-images reach the
    /// disk is in force. `wal` takes a `delete` journal rather than none, so
    /// this is true of every mode the engine ships with except `memory` and
    /// `off` - see `journal_for` in `crates/inillucent-engine/src/lib.rs`.
    fn can_undo_a_steal(&self) -> bool {
        self.journal
            .borrow()
            .as_ref()
            .is_some_and(|journal| journal.mode().is_durable())
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
    /// Reports whether a page holds a change no transaction has committed.
    ///
    /// **This is no-steal, and it is a condition rather than a convention.** The
    /// checkpointer's whole correctness argument is that an open transaction's
    /// pages are not in the file: recovery is redo-only, so a page written
    /// before its transaction committed can never be taken back out - replaying
    /// from an earlier point does not *undo* anything, it only re-applies.
    ///
    /// Until this existed the argument was written down and nothing enforced
    /// it. A checkpoint taken while a transaction was open wrote that
    /// transaction's dirty pages, the crash that followed rolled it back
    /// everywhere except the data file, and the row was still there afterwards.
    /// The model campaign found it on its second seed: "(0, 12) is there and
    /// should not be".
    ///
    /// A page above the watermark is **skipped**, not refused. A checkpoint
    /// with a writer open is an ordinary thing to do and has to succeed; what
    /// it must not do is advance the recovery point past the pages it skipped,
    /// which is why the caller sets `recovery_from` no higher than the oldest
    /// open transaction's first record.
    ///
    /// @param frame - the frame about to be written
    /// @param page - the page it holds
    fn holds_uncommitted(&self, frame: u32, page: PageId) -> DbResult<bool> {
        let uncommitted = self.uncommitted_lsn.load(Ordering::SeqCst);
        if uncommitted == u64::MAX {
            return Ok(false);
        }
        let _ = page;
        let lsn = self.lsn_of(frame)?;
        Ok(lsn >= uncommitted)
    }

    /// Returns the LSN stamped on a frame's page.
    ///
    /// @param frame - the frame
    fn lsn_of(&self, frame: u32) -> DbResult<u64> {
        let bytes = self
            .buffers
            .get(frame as usize)
            .ok_or_else(|| misuse("frame index out of range"))?
            .try_borrow()
            .map_err(|_| misuse("a frame chosen for writeback was mutably borrowed"))?;
        page::read_u64(&bytes, page::header::LSN)
    }

    /// Sets the LSN at or above which a page's change is uncommitted.
    ///
    /// `u64::MAX` means nothing is uncommitted, which is the state between
    /// transactions and the state of a database with no log at all.
    ///
    /// @param lsn - the open writer's first record, or `u64::MAX` for none
    pub fn set_uncommitted_lsn(&self, lsn: u64) {
        self.uncommitted_lsn.store(lsn, Ordering::SeqCst);
    }

    /// Returns how many writebacks no-steal has held back.
    pub fn held_back(&self) -> u64 {
        self.counters.held_back.get()
    }

    /// Returns the LSN at or above which a page's change is uncommitted.
    pub fn uncommitted_lsn(&self) -> u64 {
        self.uncommitted_lsn.load(Ordering::SeqCst)
    }

    /// Returns a handle to the watermark, for a caller that has to move it
    /// while the file is borrowed.
    ///
    /// The transaction manager takes one at assembly and keeps it. A
    /// transaction's first log record is written from inside the tree mutation
    /// that holds the file mutably, so reaching the pool through the file at
    /// that moment is not possible - and setting the watermark afterwards would
    /// leave a window in which an eviction could steal the page.
    pub fn uncommitted_handle(&self) -> Arc<AtomicU64> {
        Arc::clone(&self.uncommitted_lsn)
    }

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
        if lsn <= durable {
            return Ok(());
        }
        // The log is behind. Ask it to catch up before refusing: a statement
        // that dirties more pages than the pool holds has to evict, and every
        // candidate it has carries an LSN the log has not reached, so refusing
        // outright fails a statement that has done nothing wrong.
        //
        // The handle is cloned out and the borrow dropped before the call,
        // so an `advance` that reaches back into the pool - to register a
        // different one, or to write a page of its own - does not find this
        // cell already borrowed. The comment here used to say that while the
        // borrow was held straight through the call (task-1932, M9).
        let advance = self.advance_log.borrow().as_ref().map(std::rc::Rc::clone);
        let Some(advance) = advance else {
            return Err(misuse(format!(
                "page {} carries lsn {lsn} and the log is durable to {durable}: writing it would put the data file ahead of the log",
                page.0
            )));
        };
        let reached = advance()?;
        self.durable_lsn.set(reached);
        if lsn <= reached {
            return Ok(());
        }
        Err(misuse(format!(
            "page {} carries lsn {lsn}, the log was asked to catch up and reached {reached}: writing it would put the data file ahead of the log",
            page.0
        )))
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
        // An interior page being replaced invalidates every child's record of
        // where its swip sits inside it. See `forget_children_of`.
        if page::kind_of(image).ok() == Some(PageKind::Interior) {
            self.forget_children_of(page);
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
        // Before the bytes change, so the recovery point is the LSN the file's
        // copy of this page still carries.
        self.note_dirty_from(frame);
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
    /// Records where recovery has to start for a page that is about to change.
    ///
    /// **The LSN the page carried *before* the change, floored at the log's
    /// retained point.** This is ARIES's `recLSN`, and the floor is not an
    /// extra precaution - without it this function is wrong, not merely
    /// conservative. A page's header LSN only changes when the page itself is
    /// modified, not when it is merely checkpointed: a leaf page can go a dozen
    /// checkpoints without being touched, carrying the same old stamp the whole
    /// time while `retire_segments_below` keeps deleting the segments below
    /// each checkpoint's own, much higher, recovery point. The page is still
    /// perfectly correct on disk - nothing has changed about it since that old
    /// stamp, so every checkpoint in between was right to consider it fully
    /// caught up - but the stamp itself is now a lie about what the log still
    /// holds. The moment this page dirties again, using that stale stamp as
    /// `rec_lsn` asks the *next* checkpoint to point recovery at a segment that
    /// is already gone, which is worse than the bug this field exists to fix:
    /// reproduced directly by a page that shares a table with an unrelated
    /// later commit (`a_checkpoint_during_a_later_open_transaction_keeps_the_earlier_commit`
    /// in `inillucent-compat`'s `durability.rs`), where `rec_lsn` came back
    /// below a bound an earlier checkpoint had already retired past and the
    /// row that commit inserted did not survive a crash.
    ///
    /// **`retained_lsn`, not `durable_lsn`, is the floor - `durable_lsn` is
    /// already wrong by the time this runs.** `durable_lsn` advances on every
    /// sync of the log, including the sync a commit does for its own record -
    /// which happens *before* that same commit's change reaches this call. A
    /// first attempt at this fix clamped to `durable_lsn` and it stayed broken:
    /// the very insert this comment's reproduction depends on dirtied its page
    /// with `durable_lsn` already past its own record's LSN, so the clamp
    /// swallowed the one record a later checkpoint would need to replay for a
    /// page it has to hold back - the same symptom, for a new reason, on a
    /// clean build with the naive fix applied. `retained_lsn` only moves when
    /// a checkpoint actually persists a new recovery point and retires
    /// segments against it, so it never names a point later than what is
    /// truly, physically retained, and never later than the record that is
    /// dirtying this page right now.
    ///
    /// @param frame - the frame about to change
    fn note_dirty_from(&self, frame: u32) {
        let already = {
            let state = self.state.borrow();
            state
                .frames
                .get(frame as usize)
                .is_some_and(|meta| meta.dirty)
        };
        if already {
            return;
        }
        let lsn = self.lsn_of(frame).unwrap_or(0);
        let floor = self.retained_lsn.get();
        let lsn = if floor == u64::MAX {
            lsn
        } else {
            lsn.max(floor)
        };
        let mut state = self.state.borrow_mut();
        if let Some(meta) = state.frames.get_mut(frame as usize) {
            meta.rec_lsn = lsn;
        }
    }

    /// Returns the lowest LSN recovery must start at to rebuild every page a
    /// checkpoint's flush is actually going to hold back.
    ///
    /// `u64::MAX` when nothing is dirty, which is what lets a checkpoint that
    /// wrote everything advance the recovery point to the log's durable end.
    ///
    /// **Only a page `holds_uncommitted` will actually hold back, not every
    /// dirty page - counting the rest is a real bug, not extra caution.** A
    /// freshly allocated page can sit dirty with its header LSN still at its
    /// zeroed, never-stamped default; with no transaction open,
    /// `holds_uncommitted` answers false for every page regardless of that
    /// stamp, so a checkpoint's flush writes all of them and holds nothing
    /// back. Folding that page's `rec_lsn` into this minimum anyway pinned
    /// `recovery_from` at its stale-or-zero stamp on every such checkpoint,
    /// forever - `retire_segments_below` then has nothing below the pinned
    /// point to reclaim, and a build that should shrink its log to a few
    /// kilobytes never sheds a single segment. Reproduced directly:
    /// `a_checkpoint_reclaims_the_log` in `inillucent-compat`'s
    /// `new_engine_log_retire.rs`, which measures exactly this - the log
    /// shrinking after a checkpoint with nothing open at all.
    ///
    /// So a page only contributes when `uncommitted_lsn` names an open
    /// transaction *and* this page's current stamp is at or above it - the
    /// same test `holds_uncommitted` itself makes, checked here rather than
    /// shared with it because `writeback` needs the answer for one frame at a
    /// time and this needs the minimum over all of them before any of them
    /// move.
    pub fn oldest_dirty_lsn(&self) -> u64 {
        let uncommitted = self.uncommitted_lsn.load(Ordering::SeqCst);
        if uncommitted == u64::MAX {
            // No transaction is open, so no page is held back this
            // checkpoint - every dirty page's stamp is about to be written,
            // and none of them bound what recovery still needs.
            return u64::MAX;
        }
        let state = self.state.borrow();
        state
            .frames
            .iter()
            .enumerate()
            .filter(|(_, meta)| meta.dirty && meta.state != FrameState::Free)
            .filter(|(frame, _)| {
                self.lsn_of(*frame as u32)
                    .is_ok_and(|stamp| stamp >= uncommitted)
            })
            .map(|(_, meta)| meta.rec_lsn)
            .min()
            .unwrap_or(u64::MAX)
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
        self.note_dirty_from(frame);
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
    ///
    /// Under a rollback journal it takes two passes over the same list: every
    /// pre-image first, then one sync, then the pages. The pre-images have to
    /// be on the media before the first page is overwritten, and doing it in
    /// two passes is what lets a batch of a thousand pages pay for one sync
    /// instead of a thousand. The writeback loop still asks for the sync per
    /// page, because the evictor reaches it without a flush around it; after
    /// this pass there is nothing left for it to sync.
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
        if self.journal.borrow().is_some() {
            for (page, _) in &dirty {
                self.journal_page(*page)?;
            }
            self.seal_journal()?;
        }
        for (page, frame) in &dirty {
            self.writeback(*frame, *page, Writing::Checkpoint)?;
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
    /// The record is taken mutably because one of its fields is only knowable
    /// **after** the flush: the high water is the highest stamp any page in the
    /// file carries, and the pages this checkpoint is about to write are part
    /// of the file it describes. Setting it before the flush would leave the
    /// meta page one checkpoint behind the stamps it is meant to bound, which
    /// is the state the next open resumes the log above.
    ///
    /// @param meta - the record to write, with its generation already bumped
    pub fn checkpoint(&self, meta: &mut Meta) -> DbResult<()> {
        // **The journal is sealed before the first page moves**, and `flush`
        // is where that happens: it saves every pre-image the batch needs and
        // syncs once before it writes anything. This call used to be the only
        // one, and it ran here - before `flush` had saved a single pre-image -
        // so it synced an empty file and the ordering a rollback journal exists
        // to forbid held anyway. It is kept because anything a caller saved
        // before reaching a checkpoint is still owed a sync, and it costs
        // nothing when there is none.
        self.seal_journal()?;
        self.flush()?;
        self.file
            .sync(SyncMode::Normal)
            .map_err(|error| error.into_db_error())?;
        // Every page this checkpoint wrote has now raised the high water, so
        // the number recorded here bounds the stamps the file actually holds
        // rather than the ones it held a checkpoint ago. It never goes
        // backwards: a run that writes no stamped page keeps what it read.
        meta.high_water_lsn = meta.high_water_lsn.max(self.high_water_lsn.get());
        let mut image = vec![0u8; self.page_size];
        meta.encode(&mut image)?;
        // **The meta pages are journaled too, and they were the last pages that
        // were not.** A rollback journal has to hold a pre-image of every page
        // the checkpoint overwrites, and these two are pages the checkpoint
        // overwrites. Leaving them out left a crash here able to produce a file
        // whose data pages the journal put back to before the checkpoint and
        // whose meta record says the checkpoint finished: the recorded
        // `checkpoint_lsn` then tells redo that everything up to it is already
        // in the file, so the records that would have re-applied the pages the
        // journal just undid are skipped, and the database comes back as
        // neither its old self nor its new one. It came back with no tables at
        // all, because the catalog's own page is one of the pages the journal
        // put back.
        //
        // The shadow page does not cover this. Both slots take the same image
        // in the loop below, so the second one is not an older copy to fall
        // back on - it is a second chance for the *new* record to survive, and
        // `Meta::choose` believing either of them is the failure. What makes
        // the checkpoint undoable is the previous record being on the disk in
        // the journal, which is the same thing that makes every other page
        // undoable.
        self.journal_page(META_PAGE)?;
        self.journal_page(SHADOW_PAGE)?;
        self.seal_journal()?;
        for slot in [META_PAGE, SHADOW_PAGE] {
            self.file
                .write_all_at(slot.0.saturating_mul(self.page_size as u64), &image)
                .map_err(|error| error.into_db_error())?;
        }
        self.file
            .sync(SyncMode::Normal)
            .map_err(|error| error.into_db_error())?;
        Counters::add(&self.counters.writes, 2);
        // **And disposed of after the meta record is durable**, which is the
        // moment the commit exists. A journal removed a line earlier would
        // leave a crash with a file it could neither trust nor repair.
        //
        // **Unless an eviction has already put an open transaction's page in
        // the file**, in which case the pre-images in this journal are the only
        // way back from that page and the journal outlives the checkpoint. The
        // journal restores the meta record too, so keeping it does not leave a
        // half-undone file: a crash puts the data pages, the meta page and its
        // shadow all back to what they were before this checkpoint, and the log
        // replays forward from the recovery point that older meta record names.
        // The next checkpoint with no writer open disposes of it.
        if self.stolen.get() && self.uncommitted_lsn.load(Ordering::SeqCst) != u64::MAX {
            return Ok(());
        }
        self.finish_journal()?;
        Ok(())
    }

    /// Saves one page's current contents to the rollback journal, reporting
    /// whether it saved anything.
    ///
    /// The answer is what tells `writeback` whether it owes a sync: a page
    /// already saved by this checkpoint's first pass needs neither the read
    /// below nor a second sync, and in write-ahead-log mode there is no
    /// journal and the answer is always no.
    ///
    /// Reads the page **from the file**, not from the pool: the pre-image the
    /// journal needs is what is durably there, and the frame holds the new
    /// version. A page beyond the end of the file has no pre-image, which is
    /// the right answer - restoring it would mean writing zeros over a page the
    /// transaction created.
    ///
    /// @param page - the page about to be overwritten
    fn journal_page(&self, page: PageId) -> DbResult<bool> {
        let mut journal = self.journal.borrow_mut();
        let Some(journal) = journal.as_mut() else {
            return Ok(false);
        };
        if page.0 >= self.page_count.get() || !journal.wants(page) {
            return Ok(false);
        }
        // **A read that fails is a refusal, not an absence** - unless the page
        // is genuinely not in the file yet. This used to answer "no pre-image
        // needed" for *any* read error, and both callers then carried on and
        // overwrote the page, so a transient read error followed by a crash
        // left a modified page with nothing to put back. That is the one thing
        // the invariant at the top of `crate::journal` forbids.
        //
        // The page count above is not the test for "not in the file yet", and
        // using it as one is what made the first attempt at this refuse every
        // growing transaction: `page_count` is the pool's logical count, and it
        // runs ahead of the file whenever pages have been allocated but not yet
        // written. The file's own length is the answer. A page at or past it
        // has no pre-image because it has no image, and restoring it would mean
        // writing zeros over a page the transaction created.
        let offset = page.0.saturating_mul(self.page_size as u64);
        let length = self
            .file
            .file_size()
            .map_err(|error| error.into_db_error())?;
        if offset.saturating_add(self.page_size as u64) > length {
            return Ok(false);
        }
        let mut before = vec![0u8; self.page_size];
        self.file
            .read_exact_at(offset, &mut before)
            .map_err(|error| error.into_db_error())?;
        journal.save(page, &before)?;
        Ok(true)
    }

    /// Puts a rollback journal in force, or takes it out of force.
    ///
    /// @param journal - the journal, or nothing for the write-ahead log
    pub fn set_journal(&self, journal: Option<crate::journal::Journal>) {
        *self.journal.borrow_mut() = journal;
    }

    /// Syncs the journal, which must happen before the first page is written.
    pub fn seal_journal(&self) -> DbResult<()> {
        match self.journal.borrow().as_ref() {
            Some(journal) => journal.seal(),
            None => Ok(()),
        }
    }

    /// Disposes of the journal once the commit is durable.
    ///
    /// Clears the record of what evictions have stolen along with it: the
    /// pre-images are gone, so the next steal is the first one this journal
    /// has to outlive.
    pub fn finish_journal(&self) -> DbResult<()> {
        let outcome = match self.journal.borrow_mut().as_mut() {
            Some(journal) => journal.finish(),
            None => Ok(()),
        };
        if outcome.is_ok() {
            self.stolen.set(false);
        }
        outcome
    }

    /// Raises the lock on the database file.
    ///
    /// **The pool owns the file, so the pool owns the lock.** The protocol
    /// itself is `inillucent-vfs`'s - it has been implemented and conformance
    /// tested since Phase 2 and nothing used it, because the engine assumed it
    /// was the only process on the file. Using it is what makes
    /// `PRAGMA locking_mode = normal` a description rather than a claim.
    ///
    /// @param level - the level to raise to
    pub fn lock(&self, level: FileLock) -> DbResult<()> {
        self.lock_within(level, DEFAULT_BUSY_MILLIS)
    }

    /// Raises the lock, waiting up to a budget for the holder to let go.
    ///
    /// **Waiting is the whole of what a busy timeout is.** A lock another
    /// process holds is not an error - it is a lock that will be released - and
    /// an engine that reported failure immediately would make every concurrent
    /// pair of writers fail rather than take turns. The sleep grows so that a
    /// long wait is not a spin, and the last attempt reports what it found.
    ///
    /// @param level - the level to raise to
    /// @param budget_millis - how long to keep trying
    pub fn lock_within(&self, level: FileLock, budget_millis: u64) -> DbResult<()> {
        lock_with_wait(self.file.as_ref(), level, budget_millis)
    }

    /// Lowers the lock on the database file.
    ///
    /// @param level - the level to drop to, `None` to release entirely
    pub fn unlock(&self, level: FileLock) -> DbResult<()> {
        if self.file.lock_level() <= level {
            return Ok(());
        }
        self.file
            .unlock(level)
            .map_err(|error| error.into_db_error())
    }

    /// Returns the level currently held.
    pub fn lock_level(&self) -> FileLock {
        self.file.lock_level()
    }

    /// Reads the two meta slots straight from the file.
    ///
    /// **Past the pool, deliberately.** A connection asking whether another
    /// process has committed cannot ask its own cache: the whole question is
    /// whether the cache is stale.
    ///
    /// @param page_size - how big a page is
    pub fn read_meta_slots(&self, page_size: usize) -> DbResult<(Vec<u8>, Vec<u8>)> {
        let mut primary = vec![0u8; page_size];
        let mut shadow = vec![0u8; page_size];
        self.file
            .read_exact_at(0, &mut primary)
            .map_err(|error| error.into_db_error())?;
        self.file
            .read_exact_at(page_size as u64, &mut shadow)
            .map_err(|error| error.into_db_error())?;
        Ok((primary, shadow))
    }

    /// Drops every cached page, so the next read comes from the file.
    ///
    /// **What a connection does when another process has committed.** Every
    /// frame is written back if it is dirty and then released, and every
    /// swizzled pointer into it is put back to a page id on the way - which is
    /// `evict_one`'s job and the reason this is written in terms of it rather
    /// than by clearing the tables. Clearing them directly would leave a parent
    /// page holding a pointer to a frame that now holds something else, which
    /// is the single worst thing this engine can do.
    ///
    /// Returns how many frames went.
    pub fn discard_all(&self) -> DbResult<usize> {
        let mut gone = 0usize;
        // Bounded by the frame count: a pinned frame cannot be evicted, and a
        // caller that still holds a guard gets fewer frames dropped rather than
        // an endless sweep.
        for _ in 0..self.frames().saturating_mul(2) {
            if self.resident() == 0 {
                break;
            }
            self.cool()?;
            match self.evict_one()? {
                Some(_) => gone = gone.saturating_add(1),
                None => break,
            }
        }
        Ok(gone)
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

    /// Fills a memory file with `pages` zeroed, checksummed pages.
    ///
    /// Split out of [`pool_over`] so a test that needs the VFS afterwards - to
    /// put a rollback journal beside the database - can keep it.
    ///
    /// @param vfs - where the file lives
    /// @param path - the database's name
    /// @param page_size - how big a page is
    /// @param frames - how many frames the pool holds
    /// @param pages - how many pages to write
    fn pool_beside(
        vfs: &Arc<MemoryVfs>,
        path: &DbPath,
        page_size: usize,
        frames: usize,
        pages: u64,
    ) -> Pool {
        let file = vfs.open(path, OpenOptions::main_db()).unwrap();
        for page in 0..pages {
            let mut image = vec![0u8; page_size];
            page::write_common(&mut image, PageKind::Leaf, 0, 1).unwrap();
            page::write_u64(&mut image, 32, page).unwrap();
            page::checksum_page(&mut image).unwrap();
            file.write_all_at(page * page_size as u64, &image).unwrap();
        }
        Pool::new(file, page_size, frames, pages).unwrap()
    }

    /// Stamps a page with an LSN and a marker, and opens a transaction under it.
    ///
    /// The stamp is what `holds_uncommitted` reads, so a page stamped above the
    /// watermark is one no-steal holds back.
    ///
    /// @param pool - the pool
    /// @param page - the page to dirty
    fn dirty_under_an_open_transaction(pool: &Pool, page: PageId) {
        pool.modify(page, |bytes| {
            page::write_u64(bytes, page::header::LSN, 900)?;
            page::write_u64(bytes, 40, 0xABCD)
        })
        .unwrap();
        pool.set_uncommitted_lsn(800);
    }

    /// A page an open transaction changed is written, not dropped, when its
    /// frame is evicted.
    ///
    /// No-steal lets a **checkpoint** leave such a page out of the file, because
    /// the frame keeps it and the next checkpoint writes it. An eviction does
    /// not keep it, so the same skip there discards the change - which is what
    /// a `CREATE INDEX` through a 64-frame pool did to 129 pages, and what
    /// `inillucent-compat`'s `new_engine_log_lead` reported as
    /// `page 597 checksum 00000000 is not the computed 8d1053d3`: the checksum
    /// of a page of zeros, on a page nothing had ever written.
    ///
    /// The pre-image goes to the rollback journal before the new image goes to
    /// the file, so a crash before the transaction commits can still put the
    /// page back. See [`Writing`].
    #[test]
    fn an_uncommitted_page_is_written_rather_than_dropped_when_its_frame_goes() {
        let vfs = Arc::new(MemoryVfs::new());
        let path = DbPath::new("steal-test.rdb");
        let pool = pool_beside(&vfs, &path, 512, 2, 12);
        pool.set_journal(Some(crate::journal::Journal::new(
            Arc::clone(&vfs) as Arc<dyn Vfs>,
            &path,
            crate::journal::JournalMode::Delete,
            512,
        )));
        dirty_under_an_open_transaction(&pool, PageId(5));
        for page in 6..12u64 {
            let _ = pool.fetch(PageId(page)).unwrap();
        }
        assert!(
            !pool.is_resident(PageId(5)),
            "the frame was never evicted, so this proves nothing"
        );
        let guard = pool.fetch(PageId(5)).unwrap();
        assert_eq!(
            page::read_u64(&guard, 40).unwrap(),
            0xABCD,
            "the change was thrown away with the frame"
        );
    }

    /// With no journal to undo a steal with, the frame is kept instead.
    ///
    /// `memory` and `off` hold no pre-images on disk, so an eviction there has
    /// no way to put an uncommitted page back after a crash and must not write
    /// it. What it must also not do is free the frame: the page is still only
    /// in memory, and emptying the frame would lose it. So the page stays
    /// resident and the evictor takes another frame.
    #[test]
    fn an_uncommitted_page_keeps_its_frame_when_nothing_can_undo_a_steal() {
        let pool = pool_over(512, 2, 12);
        dirty_under_an_open_transaction(&pool, PageId(5));
        for page in 6..12u64 {
            let _ = pool.fetch(PageId(page));
        }
        assert!(
            pool.is_resident(PageId(5)),
            "the page with nowhere to go was evicted anyway"
        );
        let guard = pool.fetch(PageId(5)).unwrap();
        assert_eq!(
            page::read_u64(&guard, 40).unwrap(),
            0xABCD,
            "the change was thrown away with the frame"
        );
    }

    /// A pool with nothing left to give says which rule is refusing.
    ///
    /// The documented limit of a no-steal policy is that a transaction cannot
    /// dirty more pages than the pool holds, and a caller who has hit it needs
    /// to be told that rather than told its frames are pinned - they are not,
    /// and no number of released guards would help.
    #[test]
    fn a_pool_full_of_uncommitted_pages_says_so_rather_than_blaming_pins() {
        let pool = pool_over(512, 2, 12);
        for page in [PageId(5), PageId(6)] {
            pool.modify(page, |bytes| {
                page::write_u64(bytes, page::header::LSN, 900)?;
                page::write_u64(bytes, 40, 0xABCD)
            })
            .unwrap();
        }
        pool.set_uncommitted_lsn(800);
        let refusal = pool
            .fetch(PageId(7))
            .expect_err("a full pool has to refuse");
        let detail = refusal.detail().unwrap_or_default();
        assert!(
            detail.contains("the open transaction has changed 2"),
            "the refusal did not name no-steal: {detail}"
        );
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
        pool.checkpoint(&mut meta).unwrap();
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
