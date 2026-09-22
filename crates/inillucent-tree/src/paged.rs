//! The B+tree on the buffer pool: real interior pages, a paged descent, and
//! scans in both directions.
//!
//! Invariant: the interior separators and the leaf keys are ordered by the same
//! function. That is not a coincidence to be maintained by care - it is
//! [`PagedTree::encode_key`], one method, used by the builder that writes the
//! separators and by every descent that compares against them. A tree whose
//! separators were encoded by one rule and probed by another routes descents to
//! the wrong leaf and returns *no row* rather than an error, which is the
//! failure mode this module is arranged to make impossible.
//!
//! ## What this replaces
//!
//! Phase 1's [`crate::tree::Tree`] held its leaves in a `Vec` and its
//! separators in a parallel array of owned keys, and said in its own
//! documentation that the array "is what an interior page *is*, held as an
//! array because Phase 1 has no buffer pool to put one in". This is the buffer
//! pool version. The comparison code did not change, exactly as that note
//! predicted: a descent is still "the last child whose separator is not above
//! the probe", it just reads the separator out of a page now.
//!
//! Phase 1's tree survives beside this one. It is the model the property test
//! compares against, and keeping a second, obviously-correct implementation of
//! the same ordering is worth more than the file it costs.
//!
//! ## Two key encodings, and why
//!
//! A rowid tree's key is one integer, so its separators are eight bytes of
//! [`crate::key::order_preserving_int`] with no class byte and no tail. Every
//! other tree uses the general [`crate::key::encode`]. The distinction is worth
//! its branch: a rowid descent compares eight bytes where the general encoding
//! would compare eighteen, and `point.rowid` is the workload with the tightest
//! budget in Phase 2's gate.
//!
//! ## The descent, and what "optimistic" buys in Phase 2
//!
//! A descent observes each interior frame's version, reads the swip, follows
//! it, and validates. In Phase 2 nothing writes while a read runs, so the
//! validation never fails - and that is stated rather than glossed. What it
//! buys now is that the machinery is exercised on every descent rather than
//! being written and never run: the pool bumps a frame's version on every load
//! and every eviction, so a descent that raced one really would restart, and
//! `crate::paged::tests` forces exactly that race.

// —— a read, in the three things it does to the tree (task-1962, A8) ————
//
// 3,565 lines, of which 2,124 were one `impl PagedTree` block whose methods
// answered three different questions. Each module reopens the same `impl`, so
// no signature changed and nothing outside this directory can tell.
//
// `occupancy` joined them for the same reason (task-2052): which pages a tree
// holds is a fourth question, asked by `DROP` and by the integrity checker and
// by nothing that reads a row.
mod bulk;
mod cursor;
mod descent;
mod occupancy;
mod skip;

use std::cell::RefCell;

use inillucent_base::error::{corrupt, misuse};
use inillucent_base::DbResult;
use inillucent_pool::extent::{self, ExtentRef};
use inillucent_pool::interior::{InteriorBuilder, InteriorRef};
use inillucent_pool::page::{self, PageKind};
use inillucent_pool::{Database, PageId, Pool, Swip};

use inillucent_value::collation::Collation;

use crate::datum::{Datum, OwnedDatum};
use crate::key;
pub use crate::keyenc::KeyEncoding;
use crate::leaf::{Extents, Hit, LeafBuilder, LeafRef, Spill};
use crate::tree::BULK_FILL;
use crate::types::ColumnSpec;
pub use occupancy::{released, PageShare, Released};

/// How many times a descent retries an optimistic read before giving up.
///
/// The TDD's four, taken from the pool so the two cannot drift.
const RESTARTS: u32 = inillucent_pool::OPTIMISTIC_RETRIES;

/// The lowest and the highest key under a subtree, either absent when the
/// subtree holds no live row.
pub type KeyRange = (Option<Vec<u8>>, Option<Vec<u8>>);

/// Reads one out-of-line value.
///
/// The reference's length is checked against the lengths the pages themselves
/// declare rather than trusted: the two disagreeing is a corruption, and reading
/// the longer of the two would walk off the end of the chain.
///
/// @param pool - the buffer pool the file is open through
/// @param reference - what the leaf holds
pub fn read_extent(pool: &Pool, reference: ExtentRef) -> DbResult<Vec<u8>> {
    // A value packed into a shared page is one slot of one page, so it is one
    // fetch and one copy - there is no chain to follow and no length to
    // reconcile beyond the one the slot itself declares.
    if let Some(slot) = reference.slot {
        let guard = pool.fetch(reference.first)?;
        let held = extent::shared::read(guard.bytes(), slot)?;
        if held.len() as u64 != reference.length {
            return Err(corrupt(format!(
                "a shared extent slot holds {} bytes where its reference says {}",
                held.len(),
                reference.length
            )));
        }
        return Ok(held.to_vec());
    }
    let mut out = Vec::with_capacity(reference.length.min(1 << 20) as usize);
    let mut page = reference.first;
    let mut pages = 0u64;
    while !page.is_none() {
        let guard = pool.fetch(page)?;
        // **The page number, because the message alone does not identify one.**
        // A page whose kind is no longer `BlobExtent` is a page something else
        // has taken over, and naming the page number is what tied a failing
        // read to a `CREATE TABLE`'s root page landing on a page a recovery bug
        // had left mismarked as free while it was still live. Reading it out of
        // the reference costs nothing on the path that succeeds.
        let (body, next) = extent::read_page(guard.bytes()).map_err(|error| {
            corrupt(format!(
                "{} at page {}, which is page {} of an extent starting at {} and {} bytes long",
                error.detail().unwrap_or("an extent page is not readable"),
                page.0,
                pages.saturating_add(1),
                reference.first.0,
                reference.length,
            ))
        })?;
        out.extend_from_slice(body);
        page = next;
        pages = pages.saturating_add(1);
        if out.len() as u64 > reference.length {
            return Err(corrupt(
                "an extent holds more bytes than its reference says",
            ));
        }
        // A chain that loops would otherwise read for ever. The bound is the
        // most pages the declared length could possibly need, plus one for the
        // zero-length case.
        if pages > extent::pages_needed(reference.length, pool.page_size()).saturating_add(1) {
            return Err(corrupt("an extent chain is longer than its length allows"));
        }
    }
    if out.len() as u64 != reference.length {
        return Err(corrupt(format!(
            "an extent holds {} bytes where its reference says {}",
            out.len(),
            reference.length
        )));
    }
    Ok(out)
}

/// Writes one value out of line, logging every page it takes.
///
/// The allocator asks the free map for a **contiguous run** big enough for the
/// whole value, so the common case is one seek and one sequential read rather
/// than SQLite's chain of dependent four-kilobyte reads. That is the whole of
/// the `large.values` argument in the TDD.
///
/// @param database - the file the pages are allocated and installed in
/// @param log - where the records go
/// @param tree_id - the tree the value belongs to
/// @param value - the bytes to store
pub fn write_extent(
    database: &mut Database,
    log: &mut dyn crate::write::TreeLog,
    tree_id: u64,
    value: &[u8],
) -> DbResult<ExtentRef> {
    let page_size = database.page_size();
    let pages = extent::pages_needed(value.len() as u64, page_size).max(1);
    // **A value that fits inside one page shares one.** A run is
    // whole pages, so a value one byte over the spill threshold used to cost a
    // whole page: 4,200 bytes took 32,768, and the band from there to a full
    // page is where extracted text, JSON payloads and rendered vectors live.
    // Nikaya's 601,862 embeddings at 9,513 bytes each were 20.5 GB of a 25.66 GB
    // staged file for exactly this reason.
    //
    // Anything longer than a page keeps its contiguous run. Packing would not
    // help it - it needs every byte of the pages it takes - and the run is the
    // whole of the `large.values` argument.
    //
    // **And it has to fit a shared page, which holds less than a page.** The
    // count above is pages of *payload*; a shared page spends another fifty six
    // bytes on its header and the directory entry, so a value between the two
    // reached `shared::place` on a page made fresh for it and was refused with
    // `a shared extent page has no room for this value` - which the caller read
    // as `bad parameter or other API misuse`. Measured on the default page
    // size: a bound TEXT of 32,000 bytes stored and one of 32,700 did not, and
    // nothing in the tree had a test for a value between a shared page's
    // capacity and a whole one (task-1979, section 10, D2).
    if pages <= 1 && value.len() <= extent::shared::capacity(page_size) {
        return write_packed_extent(database, log, tree_id, value);
    }
    let first = database.allocate(pages)?;
    let images = extent::encode_run(value, first, page_size, tree_id)?;
    for (id, mut image) in images {
        log_allocated_page(&mut Some(log), database, id, &mut image)?;
    }
    Ok(ExtentRef::run(first, value.len() as u64))
}

/// Puts one small value into a shared extent page, allocating one if needed.
///
/// The page the last value went on is tried first - it is a hint on the
/// `Database`, held only in memory - and a fresh page is allocated when it is
/// full or there is none. Either way the whole page image is logged, so redo is
/// a copy and no new record kind is needed for this.
///
/// @param database - the file the page is allocated and installed in
/// @param log - where the records go
/// @param tree_id - the tree the value belongs to
/// @param value - the bytes to store
fn write_packed_extent(
    database: &mut Database,
    log: &mut dyn crate::write::TreeLog,
    tree_id: u64,
    value: &[u8],
) -> DbResult<ExtentRef> {
    if let Some(page) = database.shared_extent() {
        let held = {
            let guard = database.pool().fetch(page)?;
            let bytes = guard.bytes();
            extent::shared::is_shared(bytes) && extent::shared::has_room(bytes, value.len())?
        };
        if held {
            let mut image = {
                let guard = database.pool().fetch(page)?;
                guard.bytes().to_vec()
            };
            let slot = extent::shared::place(&mut image, value)?;
            log_shared_page(log, database, page, &mut image, false)?;
            return Ok(ExtentRef::packed(page, slot, value.len() as u64));
        }
    }
    let page = database.allocate(1)?;
    let mut image = vec![0u8; database.page_size()];
    extent::shared::initialise(&mut image, tree_id)?;
    let slot = extent::shared::place(&mut image, value)?;
    log_shared_page(log, database, page, &mut image, true)?;
    database.set_shared_extent(Some(page));
    Ok(ExtentRef::packed(page, slot, value.len() as u64))
}

/// Logs and installs a shared extent page's whole image.
///
/// **The image, not the change.** A slot placed or cleared is a few bytes, but
/// the record has to be replayable against a page recovery may be holding at any
/// earlier state, and a whole image is the one form that always is - the same
/// argument `WritePage` already rests on. A shared page is written once per
/// value that lands on it, and a value that lands on it is at least four
/// kilobytes, so the log is at most a page per four kilobytes of payload.
///
/// @param log - where the records go
/// @param database - the file
/// @param page - the page's number
/// @param image - the page bytes, stamped with the record's LSN on return
/// @param fresh - whether the page was just allocated
fn log_shared_page(
    log: &mut dyn crate::write::TreeLog,
    database: &mut Database,
    page: PageId,
    image: &mut [u8],
    fresh: bool,
) -> DbResult<()> {
    if fresh {
        log.log(inillucent_wal::record::Body::AllocPage { page: page.0 })?;
    }
    let lsn = log.log(inillucent_wal::record::Body::WritePage {
        page: page.0,
        image,
    })?;
    page::write_u64(image, page::header::LSN, lsn)?;
    database.install(page, image)
}

/// Gives one out-of-line value's pages back to the free map.
///
/// @param database - the file
/// @param log - where the records go
/// @param reference - what the leaf held
pub fn free_extent(
    database: &mut Database,
    log: &mut dyn crate::write::TreeLog,
    reference: ExtentRef,
) -> DbResult<()> {
    // **A packed value gives back a slot, and the page only when it is the
    // last.** The bytes stay where they are: compacting the page
    // would move a value whose reference is in some other leaf, and the space a
    // dead slot holds is bounded by the page it is on.
    if let Some(slot) = reference.slot {
        let mut image = {
            let guard = database.pool().fetch(reference.first)?;
            guard.bytes().to_vec()
        };
        let remaining = extent::shared::clear(&mut image, slot)?;
        if remaining > 0 {
            return log_shared_page(log, database, reference.first, &mut image, false);
        }
        // Nothing on it is live, so it goes back to the free map - and the hint
        // has to let go of it first, or the next small value would be placed on
        // a page that is no longer this tree's.
        if database.shared_extent() == Some(reference.first) {
            database.set_shared_extent(None);
        }
        log.log(inillucent_wal::record::Body::FreePage {
            page: reference.first.0,
        })?;
        return database.release(reference.first, 1);
    }
    let pages = extent::pages_needed(reference.length, database.page_size()).max(1);
    for offset in 0..pages {
        let page = PageId(reference.first.0.saturating_add(offset));
        log.log(inillucent_wal::record::Body::FreePage { page: page.0 })?;
        database.release(page, 1)?;
    }
    Ok(())
}

/// A [`Spill`] that allocates a run and describes it in the log.
///
/// Held apart from the tree so that the builder, which knows nothing about
/// files, can ask for one without the tree lending it anything else.
pub struct Extender<'a> {
    /// The file the run is allocated in.
    pub database: &'a mut Database,
    /// Where the records go.
    pub log: &'a mut dyn crate::write::TreeLog,
    /// The tree the values belong to.
    pub tree_id: u64,
    /// Every run this spiller has written, so a failure can be traced.
    pub written: Vec<ExtentRef>,
}

impl Spill for Extender<'_> {
    fn spill(&mut self, _row: usize, _column: usize, value: &[u8]) -> DbResult<ExtentRef> {
        let reference = write_extent(self.database, self.log, self.tree_id, value)?;
        self.written.push(reference);
        Ok(reference)
    }
}

/// A [`Spill`] that hands back a reference a repack already had.
///
/// Wraps an [`Extender`] and consults a per-row table first: a value that was
/// already out of line keeps its run, and only a value arriving for the first
/// time is written. That is what makes a write to one row of a leaf full of
/// large values cost one run rather than all of them.
///
/// `used` records which carried references the repack kept, so the caller frees
/// exactly the ones it did not.
pub struct Carrying<'a> {
    /// Where a genuinely new value goes.
    pub inner: Extender<'a>,
    /// `carried[row][column]`, aligned with the rows being packed.
    pub carried: Vec<Vec<Option<ExtentRef>>>,
    /// The carried references this pack kept.
    pub used: Vec<ExtentRef>,
}

impl Spill for Carrying<'_> {
    fn spill(&mut self, row: usize, column: usize, value: &[u8]) -> DbResult<ExtentRef> {
        if let Some(held) = self
            .carried
            .get(row)
            .and_then(|columns| columns.get(column))
            .copied()
            .flatten()
        {
            self.used.push(held);
            return Ok(held);
        }
        self.inner.spill(row, column, value)
    }
}

/// Describes one freshly allocated page in the log, then installs it.
///
/// Two records rather than one: the allocation and the contents are separate
/// facts and recovery needs both. `AllocPage` is what stops a later allocation
/// handing the same page out twice after a crash; `WritePage` is what puts the
/// bytes back. The image is stamped with the write's LSN before it is
/// installed, so the page-LSN rule holds for it exactly as it does for a page a
/// split wrote.
///
/// With no log the image is installed unstamped, which is the byte-for-byte
/// behaviour the unlogged builder has always had.
///
/// **This is the out-of-line value run's path, and it is not the bulk builder's**
/// (task-2000, design 2). A bulk build can write its pages past the log because
/// it syncs the data file before the statement that names its root commits; an
/// overflow run written in the middle of an ordinary statement has no such sync
/// to hide behind, so its bytes are in the log like any other page's. See
/// [`write_built_page`].
///
/// @param log - where the records go, when there is one
/// @param database - the file the page is installed in
/// @param id - the page, already allocated
/// @param image - the page bytes, stamped in place with the LSN
fn log_allocated_page(
    log: &mut Option<&mut dyn crate::write::TreeLog>,
    database: &mut Database,
    id: PageId,
    image: &mut [u8],
) -> DbResult<()> {
    if let Some(log) = log.as_mut() {
        log.log(inillucent_wal::record::Body::AllocPage { page: id.0 })?;
        let lsn = log.log(inillucent_wal::record::Body::WritePage { page: id.0, image })?;
        page::write_u64(image, page::header::LSN, lsn)?;
    }
    database.install(id, image)
}

/// Returns the collation of each key column.
///
/// @param columns - the column directory
/// @param key_columns - how many leading columns form the key
fn collations_of(columns: &[ColumnSpec], key_columns: usize) -> Vec<Collation> {
    columns
        .iter()
        .take(key_columns)
        .map(|column| column.collation)
        .collect()
}

/// Returns the direction of each key column, in key order.
///
/// The same derivation as the collations and for the same reason: a tree is
/// *stored* in its key columns' directions, so every comparison it makes has to
/// use them, and the column directory is what says a column has one.
///
/// @param columns - the column directory
/// @param key_columns - how many leading columns form the key
fn directions_of(columns: &[ColumnSpec], key_columns: usize) -> Vec<bool> {
    columns
        .iter()
        .take(key_columns)
        .map(|column| column.descending)
        .collect()
}

/// An encoded key, on the stack when it fits.
///
/// A rowid key is eight bytes and a descent needs one per probe, so returning a
/// `Vec` put a heap allocation on the path `point.rowid` and every rowid lookup
/// take. Twenty-four bytes inline covers a rowid, a one-column general key and
/// most two-column ones; anything longer spills, which is correct and only
/// slower for the keys that were going to allocate anyway.
#[derive(Clone, Debug)]
pub enum KeyBytes {
    /// The key fits inline; the second field is its length.
    Inline([u8; KeyBytes::INLINE], usize),
    /// The key did not fit and lives on the heap.
    Heap(Vec<u8>),
}

impl KeyBytes {
    /// How many bytes fit without allocating.
    pub const INLINE: usize = 24;

    /// Returns the encoded bytes.
    pub fn as_slice(&self) -> &[u8] {
        match self {
            KeyBytes::Inline(bytes, length) => bytes.get(..*length).unwrap_or(&[]),
            KeyBytes::Heap(bytes) => bytes.as_slice(),
        }
    }

    /// Returns a key over a run of bytes, inline when it fits.
    ///
    /// @param bytes - the encoded key
    pub fn from_slice(bytes: &[u8]) -> KeyBytes {
        if bytes.len() <= KeyBytes::INLINE {
            let mut inline = [0u8; KeyBytes::INLINE];
            if let Some(slot) = inline.get_mut(..bytes.len()) {
                slot.copy_from_slice(bytes);
            }
            KeyBytes::Inline(inline, bytes.len())
        } else {
            KeyBytes::Heap(bytes.to_vec())
        }
    }
}

/// What one descent found, and the path it took.
///
/// The path is kept so a reverse walk can step left without descending from the
/// root again: moving to the previous leaf is "back up until a child index can
/// be decremented, then down the rightmost spine".
#[derive(Clone, Debug, Default)]
pub struct Descent {
    /// The interior pages entered, with the child index taken at each.
    pub steps: Vec<(PageId, usize)>,
    /// The leaf the descent landed on.
    pub leaf: PageId,
}

/// A B+tree whose pages live in a buffer pool.
#[derive(Clone, Debug)]
pub struct PagedTree {
    /// The identifier written into every page of this tree.
    tree_id: u64,
    /// The root page; a one-leaf tree's root is that leaf.
    root: PageId,
    /// How many interior levels sit above the leaves.
    height: u16,
    /// The column directory.
    columns: Vec<ColumnSpec>,
    /// How many leading columns form the key.
    key_columns: usize,
    /// The database's page size in bytes.
    page_size: usize,
    /// How keys become bytes.
    encoding: KeyEncoding,
    /// The collation of each key column.
    ///
    /// A tree is *stored* in its collations' order, so every comparison the
    /// tree makes - a descent's separator test, a leaf's binary search, a
    /// bound - has to use them. They come from the column directory rather
    /// than from the page, because the catalog is what says a column has one.
    collations: Vec<Collation>,
    /// The direction of each key column.
    ///
    /// Ascending for every column of nearly every tree; a `CREATE INDEX ...
    /// (k DESC)` is what puts a `true` here, and the tree is then genuinely
    /// stored in that order rather than stored ascending and reversed on the
    /// way out. See `ColumnSpec::descending`.
    directions: Vec<bool>,
    /// The leftmost leaf, so a full scan needs no descent.
    first_leaf: PageId,
    /// How many leaves the tree holds.
    leaf_count: u64,
    /// How many rows the tree holds.
    row_count: u64,
    /// The buffer the general key encoding writes into.
    ///
    /// A tree whose key is not a bare rowid encodes through
    /// `key::encode_with`, which builds a `Vec`. That allocation is per
    /// *probe*, and an index nested loop probes once per outer row:
    /// `inillucent-probeprofile` measured `encode_key_small` on `side_owner` at
    /// 61.4 ns against a 62.1 ns descent of the same tree, so half of a probe's
    /// pre-descent cost was a malloc and a free of twenty bytes.
    ///
    /// The buffer is per tree and reused, which is sound because the encoding
    /// borrows nothing: the bytes are copied into a [`KeyBytes`] before the
    /// borrow ends, and no encode can re-enter another. It is a `RefCell`
    /// rather than a `&mut` parameter because every caller of
    /// [`PagedTree::encode_key_small`] holds the tree by shared reference, as
    /// the buffer pool's own state does.
    scratch: RefCell<Vec<u8>>,
    /// What the write path has done to this tree.
    ///
    /// A `Cell` rather than a field the write methods set directly, so that a
    /// counter can be bumped from a `&self` method - the read side reports them
    /// and the write side is the only thing that moves them.
    pub(crate) stats: std::cell::Cell<crate::write::WriteStats>,
    /// The page this tree's rightmost leaf was on, the last time a write looked.
    ///
    /// **The leaf hint** (task-2000, designs 6 and 7). A write descends from the
    /// root for every row: `PagedTree::leaf_for` walks the interior levels and
    /// allocates a `Vec<PageId>` for the path it took. An append at the right edge
    /// - which is what a rowid insert into `main_table` is, and what FTS5's
    /// dictionary flush is once its terms are in order - lands in the same leaf
    /// every time, so the descent answers a question it has already answered.
    ///
    /// **A miss has to cost a comparison and nothing else, which is why the key is
    /// kept here beside the page.** The first version held only the `PageId` and
    /// answered by fetching the page, parsing the leaf and comparing the probe
    /// against its first row - so every key that was *not* an append paid a page
    /// fetch, a leaf parse and a column by column tuple comparison *before* the
    /// descent it then had to do anyway. `write.insert.batch` inserts into a rowid
    /// table carrying two secondary indexes, and the two index keys arrive in no
    /// order at all: one hint in three hit, and the other two paid twice.
    ///
    /// The bytes are the lowest probe this connection has seen descend into the
    /// hinted leaf, in [`PagedTree::key_encoding`]'s comparable form. A probe that
    /// descended into a leaf is at or above that leaf's low fence by construction,
    /// and a rightmost leaf's fence range runs from its low fence to positive
    /// infinity, so **any key at or above those bytes belongs in the hinted leaf** -
    /// for as long as the fence has not moved. `memcmp` order is the descent's order
    /// because that is what the encoding is for.
    ///
    /// Two things keep the fence still, and the hint needs both:
    ///
    /// - [`PagedTree::note_leaves`] drops the hint, and every split and every merge
    ///   calls it. Those are the only operations that move a leaf's fence.
    /// - `leaf_for_hinted` still reads the hinted page's header on a **hit** and
    ///   takes it only when it is a leaf, of *this* tree, with no right sibling. That
    ///   is what covers a page freed and handed to another tree, and a fence moved by
    ///   something other than this write path. It costs one fetch of a page the
    ///   insert is about to fetch anyway, and it is on the hit path only, so a miss
    ///   never pays it.
    ///
    /// **Any leaf, not only the rightmost one** (task-2006). The first version hinted
    /// the rightmost leaf, which is every descent of a tree being appended to and none
    /// of a tree being written in sorted order *through the middle*. FTS5's dictionary
    /// flush is the second shape: the pending terms are a `BTreeMap`, so they arrive in
    /// key order, but a term new to the index lands between terms already in it rather
    /// than past all of them.
    ///
    /// **What the hint is measured to do, and what it is not.** The A/B is one box, two
    /// adjacent gate runs, the hint the only difference. Read the two halves of it in
    /// the right order, because they disagree and one is weaker: the stage line the gate
    /// prints is a **single round**, while a workload's ratio is the median of thirty, so
    /// the stage numbers below are one sample each and the ratio is not.
    ///
    /// The stage line says it cuts the stages it touches by about a third: FTS5's `dict write` from 2.4 ms to 1.6, its `content` row
    /// writes from 2.0 to 1.4 and its `docsize` rows from 1.6 to 1.1, over 500
    /// documents. It does **not** move `extension.fts.build`'s ratio, which reads 0.68x
    /// without it and 0.69x with it, nor `write.insert.batch`, nor the weighted
    /// headline. Where the saved time goes is not accounted for: the named stages sum to
    /// about 5.5 ms of that workload's 8.1 and the rest is unattributed, so the saving
    /// lands somewhere the stage timers do not name.
    ///
    /// It is kept because it demonstrably does less work - a descent skipped is a
    /// descent skipped - and removing a change that does less work because a noisy
    /// total did not move would be reading the noise. It is not kept on a claim about
    /// any ratio, and an earlier comment here that credited it with
    /// `extension.fts.build` at 1.24x was reading a run that had four gate processes on
    /// one disk.
    ///
    /// So the hint holds a **window**: the lowest and highest probes this connection
    /// has seen descend into the hinted leaf. A leaf's key range is contiguous, so a
    /// key between two keys known to be in it is in it too - which is the whole of the
    /// argument for a middle leaf, and it needs both ends.
    ///
    /// `rightmost` is why the window is not enough on its own. A rightmost leaf's range
    /// runs to positive infinity, so an append is above every probe seen so far and a
    /// window would reject it; for that leaf the test is the low end alone. Keeping the
    /// flag is what lets one hint serve both shapes.
    ///
    /// A `RefCell` rather than a `Cell` because the keys are `Vec`s; the write path
    /// holds the tree by shared reference, the same reason [`PagedTree::stats`] is a
    /// `Cell`.
    ///
    /// **Several windows and not one, because one window only ever fitted the primary
    /// key** (task-2006). A counter on each side of it, over the 6,000 writes
    /// `inillucent-writelogattrib` makes into a table carrying two secondary indexes,
    /// said the single slot answered 3,773 of them and sent 2,227 down the tree from its
    /// root. The 2,000 writes to `main_table` are a rowid append and 1,999 of them hit;
    /// the 4,000 writes to `main_key` and `main_category` arrive in the primary key's
    /// order and not in their own, so consecutive rows land in different leaves and each
    /// one evicted the window the row before it had just proved.
    ///
    /// The windows of different leaves cannot overlap, which is what makes a set of them
    /// no weaker than one. A window is the lowest and highest probes seen to descend
    /// into a leaf, so it lies inside that leaf's fence range, and fence ranges are
    /// disjoint - so at most one entry claims any key, and the first that claims it is
    /// the only one that could. Every entry is then proved the way the single entry was:
    /// the page is still a leaf, still this tree's, and still has the right sibling it
    /// had when the window was recorded. A split or a merge clears the whole set through
    /// [`PagedTree::note_leaves`], and one done by another process is caught by that
    /// sibling, per entry.
    ///
    /// **Eight entries take 2,227 of those descents down to 153, and the transaction's
    /// wall time does not clearly move.** The A/B is the same binary with `LEAF_HINTS` at
    /// 1 and at 8, nine runs each, medians 32.99 ms and 30.51 ms - but the two spreads
    /// are 28.37..36.26 and 28.09..77.38, so 2.5 ms is inside them and this instrument is
    /// one transaction where the gate's workload is the median of thirty rounds. What the
    /// counters say is not in doubt: 2,074 descents of a root and its interior levels are
    /// not made. What that is worth is a question for the gate.
    pub(crate) leaf_hints: std::cell::RefCell<Vec<LeafHint>>,
    /// Which entry of `leaf_hints` the next unknown leaf overwrites, round robin.
    ///
    /// Round robin rather than least recently used: the set is small enough that finding
    /// the coldest entry costs more than replacing an entry that is merely old, and a
    /// secondary index walks its leaves in a cycle, which is the case both policies get
    /// right. It is an index into a full set and means nothing until the set is full.
    pub(crate) hint_victim: std::cell::Cell<usize>,
}

/// How many leaves one connection remembers per tree for its next write.
///
/// Eight, because the miss path is what a larger set costs: every entry is two
/// comparisons against bytes the caller already has, and they are paid in full by a key
/// that is in none of the windows. Eight covers a secondary index whose rows arrive in
/// another index's order while leaving that miss at a handful of `memcmp`s.
pub(crate) const LEAF_HINTS: usize = 8;

/// The leaf a write is likely to want next, and the keys that prove it.
///
/// See [`PagedTree::leaf_hint`] for the argument. Held rather than derived because
/// the proof is two comparisons and deriving it is a descent.
#[derive(Clone, Debug)]
pub(crate) struct LeafHint {
    /// The leaf.
    pub(crate) page: PageId,
    /// The lowest probe seen to descend into it, in comparable bytes.
    pub(crate) low: Vec<u8>,
    /// The highest probe seen to descend into it.
    pub(crate) high: Vec<u8>,
    /// The right sibling it had when it was recorded.
    ///
    /// Two jobs. `PageId::NONE` means the leaf was the rightmost one, whose range runs
    /// to positive infinity, so the high end of the window is unnecessary for it - an
    /// append is above every probe seen so far and a window would reject it.
    ///
    /// And it is **how a split by anybody is detected**. A split of this leaf points it
    /// at the new page, and a merge changes it too, so a sibling that still matches is
    /// a leaf whose fence range has not moved since the window was proved. That covers
    /// the one case the window's own argument cannot: another process splitting this
    /// leaf between two of our statements. It costs nothing, because the hit path
    /// already reads this page's header to check the page is still a leaf of this tree.
    pub(crate) right: PageId,
}

/// How many key-prefix columns a skip scan borrows on the stack.
///
/// Four covers every index in the scorecard fixture and in the dialect's own
/// corpus; a wider prefix spills to the heap, which costs what every prefix
/// used to cost.
const SKIP_PREFIX_INLINE: usize = 4;

/// What a skip scan does after reading the leaf it holds.
enum Step {
    /// The position is past this leaf's rows; try the next leaf.
    Right(PageId),
    /// The next distinct value is at this row of the leaf already open.
    Here(usize),
    /// The run continues past this leaf; look for its end from here.
    Seek(PageId),
}

/// How many leaves past the one in hand a skip scan will step before it
/// descends instead.
///
/// **A step and a descent answer the same question and the cheaper one depends
/// on the data.** A skip scan seeks past the run of rows sharing the prefix it
/// just visited; the partition point it wants is what a descent has to compute
/// *after* it arrives, so taking it in the leaf already open costs nothing when
/// the run ends there. When it does not, a step right is one fetch, one parse
/// and one partition point, against a descent's fetch and search per level and
/// then the same partition point - so a run that spans two or three leaves is
/// cheaper to walk and a run that spans seven is cheaper to jump.
///
/// `scan.distinct` at medium is 65 categories over 67 leaves of
/// `main_category`, and a category's run is about one and a fifth leaves - long
/// enough that the leaf in hand almost never holds the end, short enough that
/// the next one does. At large the same 65 categories span seven leaves each
/// and the walk is switched off after its first failure.
const WALK_BUDGET: usize = 2;

impl PagedTree {
    /// Rebuilds the handle for a tree already in a file.
    ///
    /// The shape - root, height, first leaf, counts - is recorded in the
    /// catalog rather than rediscovered, except that the height is verified
    /// against the root page so a catalog that disagrees with the file is an
    /// error rather than a wrong descent.
    ///
    /// @param pool - the buffer pool the pages live in
    /// @param tree_id - the identifier stamped into every page
    /// @param root - the root page
    /// @param columns - the column directory
    /// @param key_columns - how many leading columns form the key
    /// @param first_leaf - the leftmost leaf
    /// @param leaf_count - how many leaves
    /// @param row_count - how many rows
    pub fn attach(
        pool: &Pool,
        tree_id: u64,
        root: PageId,
        columns: Vec<ColumnSpec>,
        key_columns: usize,
        leaf_count: u64,
        row_count: u64,
    ) -> DbResult<PagedTree> {
        let guard = pool.fetch(root)?;
        let height = match page::kind_of(&guard)? {
            PageKind::Leaf => 0,
            PageKind::Interior => page::level_of(&guard)?,
            other => return Err(corrupt(format!("a tree root cannot be {other:?}"))),
        };
        drop(guard);
        // **The leftmost leaf is read out of the file, never taken on trust.**
        // The height above already is, and the first leaf is the same kind of
        // fact: the file knows it and a recorded copy can be stale.
        //
        // It *was* taken from the catalog, whose statistics are written at a
        // checkpoint. `checkpoint`'s own comment explains why - a tree's shape
        // changes on every split, and rewriting a catalog row that often would
        // put a catalog write on the write path - and reasons that "everything
        // after it is in the log for recovery to replay". That is true of the
        // *pages* and false of the *shape*. The root page id never changes, by
        // design; the first leaf does, because a root split moves the root's
        // contents into a new page. So a database closed without a checkpoint
        // reopened with `first_leaf` naming the root, which is now an interior,
        // and every read of it failed with "page is not a leaf". Fifty rows was
        // enough. Deriving it costs one descent per tree per open.
        let first_leaf = PagedTree::leftmost_leaf(pool, root)?;
        let encoding = KeyEncoding::choose(&columns, key_columns);
        let collations = collations_of(&columns, key_columns);
        let directions = directions_of(&columns, key_columns);
        Ok(PagedTree {
            tree_id,
            root,
            height,
            columns,
            key_columns,
            page_size: pool.page_size(),
            encoding,
            collations,
            directions,
            first_leaf,
            leaf_count,
            row_count,
            scratch: RefCell::new(Vec::new()),
            leaf_hints: std::cell::RefCell::new(Vec::new()),
            hint_victim: std::cell::Cell::new(0),
            stats: std::cell::Cell::new(crate::write::WriteStats::default()),
        })
    }

    /// Returns the tree identifier every page carries.
    pub fn tree_id(&self) -> u64 {
        self.tree_id
    }

    /// Returns the root page.
    pub fn root(&self) -> PageId {
        self.root
    }

    /// Returns how many interior levels sit above the leaves.
    pub fn height(&self) -> u16 {
        self.height
    }

    /// Returns the column directory.
    pub fn columns(&self) -> &[ColumnSpec] {
        &self.columns
    }

    /// Returns how many leading columns form the key.
    pub fn key_columns(&self) -> usize {
        self.key_columns
    }

    /// Returns how many leaves the tree holds.
    pub fn leaf_count(&self) -> u64 {
        self.leaf_count
    }

    /// Returns how many rows the tree holds.
    pub fn row_count(&self) -> u64 {
        self.row_count
    }

    /// Returns the bytes the tree's leaves occupy.
    pub fn byte_size(&self) -> usize {
        (self.leaf_count as usize).saturating_mul(self.page_size)
    }

    /// Returns the leftmost leaf.
    /// Returns the database's page size in bytes.
    pub fn page_size(&self) -> usize {
        self.page_size
    }

    /// Records that the leftmost leaf moved, which a root split does.
    ///
    /// @param leaf - the new leftmost leaf
    pub(crate) fn note_first_leaf(&mut self, leaf: PageId) {
        self.first_leaf = leaf;
    }

    /// Records that the tree got taller.
    ///
    /// @param height - the new height
    pub(crate) fn note_height(&mut self, height: u16) {
        self.height = height;
    }

    /// Adjusts the leaf count by a signed amount.
    ///
    /// @param delta - how many leaves were gained or lost
    pub(crate) fn note_leaves(&mut self, delta: i64) {
        self.leaf_count = self.leaf_count.saturating_add_signed(delta);
        // **A split and a merge are the only things that move a leaf's low fence, and
        // both come through here**, so this is where the leaf hint is dropped. See
        // [`PagedTree::leaf_hint`]: the hint says "a key at or above these bytes
        // belongs in that page", and that sentence is about a fence.
        if let Ok(mut hints) = self.leaf_hints.try_borrow_mut() {
            hints.clear();
        }
        self.hint_victim.set(0);
    }

    /// Adjusts the row count by a signed amount.
    ///
    /// @param delta - how many rows were gained or lost
    pub(crate) fn note_rows(&mut self, delta: i64) {
        self.row_count = self.row_count.saturating_add_signed(delta);
    }

    /// Returns how a key tuple becomes comparable bytes.
    pub fn key_encoding(&self) -> KeyEncoding {
        self.encoding
    }

    /// Encodes a key tuple the way this tree's separators are encoded.
    ///
    /// @param values - the key tuple, in key-column order
    pub fn encode_key(&self, values: &[Datum<'_>]) -> Vec<u8> {
        self.encoding
            .encode_ordered(values, &self.collations, &self.directions)
    }

    /// Returns the collation of each key column.
    pub fn collations(&self) -> &[Collation] {
        &self.collations
    }

    /// Returns whether each key column is stored descending.
    ///
    /// A leaf parsed without these compares ascending, which for a descending
    /// tree is not a different order but a *scrambled* one: the packed rows are
    /// sorted the other way, so a bound search bisects a sequence its invariant
    /// does not hold for and lands anywhere. `WHERE c >= 10` over
    /// `CREATE INDEX ic ON t(c DESC)` returned no rows at all.
    pub fn directions(&self) -> &[bool] {
        &self.directions
    }

    /// Encodes a key tuple without allocating when it fits inline.
    ///
    /// The hot form. [`PagedTree::encode_key`] stays for the callers that want
    /// an owned `Vec` - the bulk builder's separators, which are kept - and
    /// this one is what every descent uses.
    ///
    /// @param values - the key tuple, in key-column order
    pub fn encode_key_small(&self, values: &[Datum<'_>]) -> KeyBytes {
        if let (KeyEncoding::Rowid, [Datum::Int(number)]) = (self.encoding, values) {
            let mut inline = [0u8; KeyBytes::INLINE];
            if let Some(slot) = inline.get_mut(..8) {
                slot.copy_from_slice(&key::order_preserving_int(*number));
            }
            return KeyBytes::Inline(inline, 8);
        }
        if self.encoding == KeyEncoding::General {
            let mut scratch = self.scratch.borrow_mut();
            scratch.clear();
            for (index, value) in values.iter().enumerate() {
                let collation = self.collations.get(index).copied().unwrap_or_default();
                key::encode_into_with(value, collation, &mut scratch);
            }
            return KeyBytes::from_slice(&scratch);
        }
        KeyBytes::from_slice(&self.encoding.encode_ordered(
            values,
            &self.collations,
            &self.directions,
        ))
    }

    /// Attaches to a tree given only its root, discovering the rest by walking.
    ///
    /// [`PagedTree::attach`] is told the leftmost leaf, the leaf count and the
    /// row count, because the caller that built the tree already knew them.
    /// A reader that opens a file it did not write knows only where the root
    /// is, so it has to find them - and `leaf_count` in particular is not a
    /// statistic here, it is the bound that stops a corrupt right-link chain
    /// from being followed forever.
    ///
    /// That makes the count both the thing being measured and the guard on
    /// measuring it, so this walk is bounded by the file instead: a chain
    /// cannot be longer than the file has pages, and a tree whose chain is
    /// says so rather than looping.
    ///
    /// **The walk reads every leaf.** That is free for a small tree - the
    /// catalog is one page - and it is emphatically not free for a large one,
    /// so this is the right way to open a catalog and the wrong way to open a
    /// table. A table's counts belong in the catalog beside its root page.
    ///
    /// @param pool - the buffer pool
    /// @param tree_id - the tree's identifier
    /// @param root - the root page
    /// @param columns - the column directory
    /// @param key_columns - how many leading columns form the key
    pub fn attach_scanned(
        pool: &Pool,
        tree_id: u64,
        root: PageId,
        columns: Vec<ColumnSpec>,
        key_columns: usize,
    ) -> DbResult<PagedTree> {
        let first_leaf = Self::leftmost_leaf(pool, root)?;
        let limit = pool.page_count().max(1);
        let mut page = first_leaf;
        let mut leaf_count = 0u64;
        let mut row_count = 0u64;
        while !page.is_none() {
            let guard = pool.fetch(page)?;
            let leaf = LeafRef::parse(&guard)?;
            row_count = row_count.saturating_add(leaf.row_count() as u64);
            page = leaf.right_sibling();
            leaf_count = leaf_count.saturating_add(1);
            if leaf_count > limit {
                return Err(corrupt("a leaf chain is longer than the file has pages"));
            }
        }
        PagedTree::attach(
            pool,
            tree_id,
            root,
            columns,
            key_columns,
            leaf_count,
            row_count,
        )
    }

    /// Checks the tree's structural invariants against the pages themselves.
    ///
    /// 1. Every leaf parses and passes its own row-level integrity check.
    /// 2. Keys strictly increase within a leaf and across leaf boundaries.
    /// 3. Every interior separator is the first key of the child it precedes,
    ///    so a descent for that key lands on that child and not its neighbour.
    /// 4. The sibling chain visits exactly the leaves the interior levels do.
    ///
    /// @param pool - the buffer pool
    pub fn check(&self, pool: &Pool) -> DbResult<()> {
        let mut previous: Option<Vec<OwnedDatum>> = None;
        let mut chain = 0u64;
        self.visit_leaves(pool, &mut |leaf| {
            leaf.integrity()?;
            chain = chain.saturating_add(1);
            for row in leaf.live()? {
                let head: Vec<Datum<'_>> = row.iter().copied().take(self.key_columns).collect();
                if let Some(last) = &previous {
                    let borrowed: Vec<Datum<'_>> = last.iter().map(OwnedDatum::borrow).collect();
                    if crate::leaf::compare_rows_under(
                        &borrowed,
                        &head,
                        self.key_columns,
                        &self.collations,
                        &self.directions,
                    ) != std::cmp::Ordering::Less
                    {
                        return Err(corrupt("a key does not increase across the leaf chain"));
                    }
                }
                previous = Some(head.iter().map(OwnedDatum::from_datum).collect());
            }
            Ok(true)
        })?;
        // **Against the interior levels, which is what invariant 4 says.** It
        // used to be compared against `self.leaf_count`, a statistic the
        // catalog writes at a checkpoint - so a database closed without one
        // failed its own integrity check for having grown, while every row in
        // it read back correctly. A cached count is not an invariant; the
        // agreement between the two ways of reaching a leaf is.
        let reachable = self.count_leaves(pool, self.root)?;
        if chain != reachable {
            return Err(corrupt(format!(
                "the sibling chain visits {chain} leaves and the interior levels reach {reachable}"
            )));
        }
        self.check_subtree(pool, self.root, self.height)?;
        Ok(())
    }

    /// Reads the out-of-line values of one row.
    ///
    /// @param pool - the buffer pool the file is open through
    /// @param leaf - the leaf the row is in
    /// @param row - the row's position in the sorted region
    pub fn read_extents_row(
        &self,
        pool: &Pool,
        leaf: &LeafRef<'_>,
        row: usize,
    ) -> DbResult<Extents> {
        let mut held = Extents::default();
        if !leaf.has_extents() {
            return Ok(held);
        }
        for column in 0..leaf.column_count() {
            if leaf.column(column)?.class_at(row)? != crate::types::ValueClass::Extent {
                continue;
            }
            held.push(
                row,
                column,
                read_extent(pool, leaf.extent_at(row, column)?)?,
            );
        }
        Ok(held)
    }

    /// Resolves the out-of-line values of one delta row, and only that row.
    ///
    /// @param pool - the buffer pool the extent pages come from
    /// @param leaf - the leaf the row is in
    /// @param index - the row's position in the delta area
    pub fn read_extents_delta(
        &self,
        pool: &Pool,
        leaf: &LeafRef<'_>,
        index: usize,
    ) -> DbResult<Extents> {
        let mut held = Extents::default();
        if !leaf.has_extents() {
            return Ok(held);
        }
        for column in 0..leaf.column_count() {
            let Some(reference) = leaf.delta_extent_at(index, column)? else {
                continue;
            };
            held.push_delta(index, column, read_extent(pool, reference)?);
        }
        Ok(held)
    }

    /// Returns where a probe's key sits in a leaf, without reading its values.
    ///
    /// The locating half of [`PagedTree::probe_leaf`], split out so a probe into
    /// a leaf with out-of-line values can find the row before deciding which
    /// values to read.
    ///
    /// @param leaf - the leaf the descent landed on
    /// @param probe - the key, one value per key column
    fn hit_in(&self, leaf: &LeafRef<'_>, probe: &[Datum<'_>]) -> DbResult<Option<Hit>> {
        if let Ok(row) = leaf.search(probe)? {
            if !leaf.is_tombstoned(row)? {
                return Ok(Some(Hit::Sorted(row)));
            }
        }
        // The delta directory is in key order, so this is a binary search over
        // the columns the probe names - the same rule as `probe_leaf`'s.
        if leaf.delta_count() == 0 {
            return Ok(None);
        }
        let compared = probe.len().min(self.key_columns);
        match leaf.delta_search(probe.get(..compared).unwrap_or(probe))? {
            Ok(entry) => Ok(Some(Hit::Delta(entry))),
            Err(_) => Ok(None),
        }
    }

    /// Reads every out-of-line value one leaf holds.
    ///
    /// One pass per leaf rather than one per access: a leaf with extents is
    /// scanned column by column, and a resolver called per access would re-read
    /// the same value once per pass.
    ///
    /// A leaf whose flag is clear reads nothing and allocates nothing, which is
    /// what keeps this off every other leaf's path.
    ///
    /// @param pool - the buffer pool the file is open through
    /// @param leaf - the leaf to read
    pub fn read_extents(&self, pool: &Pool, leaf: &LeafRef<'_>) -> DbResult<Extents> {
        let mut held = Extents::default();
        if !leaf.has_extents() {
            return Ok(held);
        }
        for row in 0..leaf.row_count() {
            for column in 0..leaf.column_count() {
                if leaf.column(column)?.class_at(row)? != crate::types::ValueClass::Extent {
                    continue;
                }
                held.push(
                    row,
                    column,
                    read_extent(pool, leaf.extent_at(row, column)?)?,
                );
            }
        }
        // The delta area holds its own out-of-line values, tagged rather than
        // classed. A leaf that has taken a wide row since its last compaction
        // has them here and nowhere else.
        for index in 0..leaf.delta_count() {
            for column in 0..leaf.column_count() {
                let Some(reference) = leaf.delta_extent_at(index, column)? else {
                    continue;
                };
                held.push_delta(index, column, read_extent(pool, reference)?);
            }
        }
        Ok(held)
    }

    /// Returns the references every out-of-line value in a leaf names.
    ///
    /// For the write path, which has to give a replaced value's pages back to
    /// the free map. Reading the references is cheap - they are in the leaf -
    /// where reading the values is not.
    ///
    /// @param leaf - the leaf to read
    pub fn extent_refs(leaf: &LeafRef<'_>) -> DbResult<Vec<ExtentRef>> {
        let mut refs = Vec::new();
        if !leaf.has_extents() {
            return Ok(refs);
        }
        for row in 0..leaf.row_count() {
            for column in 0..leaf.column_count() {
                if leaf.column(column)?.class_at(row)? != crate::types::ValueClass::Extent {
                    continue;
                }
                refs.push(leaf.extent_at(row, column)?);
            }
        }
        for index in 0..leaf.delta_count() {
            for column in 0..leaf.column_count() {
                if let Some(reference) = leaf.delta_extent_at(index, column)? {
                    refs.push(reference);
                }
            }
        }
        Ok(refs)
    }

    /// Checks one subtree's separators against its children's first keys.
    ///
    /// @param pool - the buffer pool
    /// @param page - the subtree's root
    /// @param level - the level the page should declare
    /// Returns how many leaves the interior levels reach from a page.
    ///
    /// @param pool - the buffer pool
    /// @param page - the subtree's root
    fn count_leaves(&self, pool: &Pool, page: PageId) -> DbResult<u64> {
        let children = {
            let guard = pool.fetch(page)?;
            if page::kind_of(&guard)? == PageKind::Leaf {
                return Ok(1);
            }
            let interior = InteriorRef::parse(&guard)?;
            (0..interior.children())
                .map(|child| interior.swip(child))
                .collect::<DbResult<Vec<Swip>>>()?
        };
        let mut leaves = 0u64;
        for swip in children {
            let target = pool.page_of_swip(swip)?;
            leaves = leaves.saturating_add(self.count_leaves(pool, target)?);
        }
        Ok(leaves)
    }

    fn check_subtree(&self, pool: &Pool, page: PageId, level: u16) -> DbResult<()> {
        let image = {
            let guard = pool.fetch(page)?;
            guard.bytes().to_vec()
        };
        if page::kind_of(&image)? == PageKind::Leaf {
            if level != 0 {
                return Err(corrupt(format!("a leaf sits at level {level}")));
            }
            return Ok(());
        }
        let interior = InteriorRef::parse(&image)?;
        // The slot array's ordering is checked here rather than on every parse:
        // a descent parses a page to read one slot, and validating the whole
        // array there cost `PointProbe` more than three times its budget. The
        // integrity checker is where an O(pages x slots) pass belongs.
        interior.validate()?;
        if interior.level() != level {
            return Err(corrupt(format!(
                "an interior page declares level {} where the tree says {level}",
                interior.level()
            )));
        }
        for child in 0..interior.children() {
            let target = pool.page_of_swip(interior.swip(child)?)?;
            // **The separator bounds its children; it is not equal to one.**
            //
            // A bulk-built tree makes every separator exactly its child's first
            // key, and the first version of this check asserted that - which
            // was an accident of the builder rather than the tree's invariant.
            // The moment a row is deleted, the child's first *live* key is
            // above the separator and the check fired on a correct tree.
            //
            // What a B+tree actually promises is that child `i` holds every key
            // `k` with `K[i-1] <= k < K[i]`, and that is what is checked here:
            // both ends, on every child, which is strictly *more* than the old
            // rule caught. The old one never looked at the upper bound at all.
            let (lowest, highest) = self.key_range_under(pool, target)?;
            if let Some(lowest) = &lowest {
                if child > 0 {
                    let separator = interior.key(child.saturating_sub(1))?;
                    if lowest.as_slice() < separator {
                        return Err(corrupt(format!(
                            "a key below separator {} is in the child above it",
                            child.saturating_sub(1)
                        )));
                    }
                }
            }
            if let Some(highest) = &highest {
                if child < interior.count() {
                    let separator = interior.key(child)?;
                    if highest.as_slice() >= separator {
                        return Err(corrupt(format!(
                            "a key at or above separator {child} is in the child below it"
                        )));
                    }
                }
            }
            self.check_subtree(pool, target, level.saturating_sub(1))?;
        }
        Ok(())
    }

    /// Returns the lowest and highest **live** encoded keys under a page.
    ///
    /// `None` for a subtree holding no live rows at all, which a tree that has
    /// been deleted from can perfectly well contain: an empty leaf is still
    /// routed to, and refusing one would be refusing a legal shape.
    ///
    /// Every leaf under the page is read, which is what makes this an integrity
    /// check rather than something a reader could afford.
    ///
    /// @param pool - the buffer pool
    /// @param page - the subtree's root
    fn key_range_under(&self, pool: &Pool, page: PageId) -> DbResult<KeyRange> {
        let mut lowest: Option<Vec<u8>> = None;
        let mut highest: Option<Vec<u8>> = None;
        self.visit_subtree(pool, page, 0, &mut |leaf| {
            for row in leaf.live()? {
                let head: Vec<Datum<'_>> = row.iter().copied().take(self.key_columns).collect();
                let encoded = self.encode_key(&head);
                if lowest.as_ref().is_none_or(|held| encoded < *held) {
                    lowest = Some(encoded.clone());
                }
                if highest.as_ref().is_none_or(|held| encoded > *held) {
                    highest = Some(encoded);
                }
            }
            Ok(())
        })?;
        Ok((lowest, highest))
    }

    /// Runs `visit` over every leaf under a page.
    ///
    /// @param pool - the buffer pool
    /// @param page - the subtree's root
    /// @param depth - how deep the walk already is
    /// @param visit - what to do with each leaf
    fn visit_subtree(
        &self,
        pool: &Pool,
        page: PageId,
        depth: u16,
        visit: &mut dyn FnMut(&LeafRef<'_>) -> DbResult<()>,
    ) -> DbResult<()> {
        if depth > 64 {
            return Err(corrupt("a subtree is deeper than 64 levels"));
        }
        let children = {
            let guard = pool.fetch(page)?;
            if page::kind_of(&guard)? == PageKind::Leaf {
                let leaf = LeafRef::parse(&guard)?
                    .with_collations(&self.collations)
                    .with_directions(&self.directions);
                let held = self.read_extents(pool, &leaf)?;
                let leaf = leaf.with_extents(&held);
                return visit(&leaf);
            }
            let interior = InteriorRef::parse(&guard)?;
            let mut children = Vec::with_capacity(interior.children());
            for child in 0..interior.children() {
                children.push(pool.page_of_swip(interior.swip(child)?)?);
            }
            children
        };
        for child in children {
            self.visit_subtree(pool, child, depth.saturating_add(1), visit)?;
        }
        Ok(())
    }
}

/// Returns the first row of a leaf whose key is at or above a probe.
///
/// A thin name over [`LeafRef::lower_bound`], kept because the span walks read
/// better with it and because the probe may be shorter than the leaf's key -
/// a one-column probe against a two-column key finds the start of the run
/// sharing that column.
///
/// @param leaf - the leaf to search
/// @param probe - the bound, one value per compared column
fn lower_bound(leaf: &LeafRef<'_>, probe: &[Datum<'_>]) -> DbResult<usize> {
    leaf.lower_bound(probe)
}

/// Returns the first row of a leaf whose key is above a probe.
///
/// @param leaf - the leaf to search
/// @param probe - the bound, one value per compared column
fn upper_bound(leaf: &LeafRef<'_>, probe: &[Datum<'_>]) -> DbResult<usize> {
    leaf.upper_bound(probe)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::PhysicalType;
    use inillucent_pool::Options;
    use inillucent_vfs::{DbPath, MemoryVfs};
    use std::collections::BTreeMap;

    /// A two-column rowid tree: the key and a label.
    fn rowid_columns() -> Vec<ColumnSpec> {
        vec![
            ColumnSpec::key(PhysicalType::Int64),
            ColumnSpec::new(PhysicalType::Text),
        ]
    }

    /// Builds a rowid tree of `rows` rows over a small page, so the tree really
    /// has interior levels rather than one leaf pretending to be a root.
    fn build(rows: i64, page_size: usize) -> (Database, PagedTree, Vec<Vec<OwnedDatum>>) {
        let vfs = MemoryVfs::new();
        let path = DbPath::new("paged.rdb");
        let mut database = Database::create(
            &vfs,
            &path,
            Options::default()
                .with_page_size(page_size)
                .with_frames(512),
        )
        .unwrap();
        let labels: Vec<String> = (0..rows).map(|n| format!("label-{n:06}")).collect();
        let owned: Vec<Vec<OwnedDatum>> = (0..rows)
            .map(|n| {
                vec![
                    OwnedDatum::Int(n),
                    OwnedDatum::Text(labels[n as usize].clone().into_bytes()),
                ]
            })
            .collect();
        let borrowed: Vec<Vec<Datum<'_>>> = owned
            .iter()
            .map(|row| row.iter().map(OwnedDatum::borrow).collect())
            .collect();
        let tree = PagedTree::bulk_build(&mut database, 7, rowid_columns(), 1, &borrowed).unwrap();
        (database, tree, owned)
    }

    /// A tree built over enough rows to need interior levels has them, passes
    /// its own integrity check, and scans back every row in order.
    #[test]
    fn a_multi_level_tree_scans_in_order() {
        let (database, tree, rows) = build(4_000, 512);
        assert!(tree.height() >= 2, "height is {}", tree.height());
        assert!(tree.leaf_count() > 100);
        tree.check(database.pool()).unwrap();
        let scanned = tree.rows(database.pool()).unwrap();
        assert_eq!(scanned.len(), rows.len());
        for (index, row) in scanned.iter().enumerate() {
            assert_eq!(row[0], OwnedDatum::Int(index as i64));
        }
        assert_eq!(tree.row_count(), rows.len() as u64);
    }

    /// Every key is found by descent, and a key that is not there is not found.
    #[test]
    fn every_key_is_found_by_descent() {
        let (database, tree, rows) = build(2_000, 512);
        let pool = database.pool();
        for index in (0..rows.len()).step_by(7) {
            let probe = [Datum::Int(index as i64)];
            let found = tree.point(pool, &probe).unwrap().expect("row is there");
            assert_eq!(found[0], OwnedDatum::Int(index as i64));
            assert_eq!(found[1], rows[index][1]);
        }
        for missing in [-1i64, 2_000, 100_000] {
            assert!(tree.point(pool, &[Datum::Int(missing)]).unwrap().is_none());
        }
    }

    /// A reverse walk visits exactly the leaves a forward walk does, in the
    /// opposite order.
    #[test]
    fn a_reverse_walk_is_the_forward_walk_backwards() {
        let (database, tree, _) = build(3_000, 512);
        let pool = database.pool();
        let mut forward: Vec<i64> = Vec::new();
        tree.visit_leaves(pool, &mut |leaf| {
            if leaf.row_count() > 0 {
                forward.push(leaf.value(0, 0)?.as_int().unwrap_or(-1));
            }
            Ok(true)
        })
        .unwrap();
        let mut backward: Vec<i64> = Vec::new();
        tree.visit_reverse(pool, None, &mut |leaf| {
            if leaf.row_count() > 0 {
                backward.push(leaf.value(0, 0)?.as_int().unwrap_or(-1));
            }
            Ok(true)
        })
        .unwrap();
        backward.reverse();
        assert_eq!(forward, backward);
        assert!(forward.len() > 50);
    }

    /// A reverse walk from a key starts on the leaf holding it and stops when
    /// the visitor says so, which is how `ORDER BY ... DESC LIMIT` works.
    #[test]
    fn a_bounded_reverse_walk_stops_early() {
        let (database, tree, _) = build(3_000, 512);
        let pool = database.pool();
        let key = tree.encode_key(&[Datum::Int(1_500)]);
        let mut visited = 0usize;
        let mut highest = None;
        tree.visit_reverse(pool, Some(&key), &mut |leaf| {
            visited = visited.saturating_add(1);
            if highest.is_none() {
                highest = leaf.value(0, 0)?.as_int();
            }
            Ok(visited < 3)
        })
        .unwrap();
        assert_eq!(visited, 3, "the walk stopped when it was told to");
        assert!(highest.unwrap_or(i64::MAX) <= 1_500);
    }

    /// A range walk starts at the leaf holding the low bound rather than at the
    /// beginning of the tree.
    #[test]
    fn a_range_walk_skips_what_is_below_it() {
        let (database, tree, _) = build(3_000, 512);
        let pool = database.pool();
        let low = tree.encode_key(&[Datum::Int(2_000)]);
        let mut first = None;
        tree.visit_range(pool, &low, &mut |leaf| {
            if first.is_none() && leaf.row_count() > 0 {
                first = leaf.value(0, 0)?.as_int();
            }
            Ok(false)
        })
        .unwrap();
        let started = first.expect("the walk visited a leaf");
        assert!(started <= 2_000, "started at {started}");
        assert!(started > 1_800, "started at {started}, too far back");
    }

    /// The tree agrees with a `BTreeMap` on every key, which is the property
    /// test the TDD asks for, now over real interior pages.
    #[test]
    fn the_paged_tree_agrees_with_a_btreemap() {
        let (database, tree, rows) = build(1_500, 512);
        let pool = database.pool();
        let mut model: BTreeMap<i64, Vec<u8>> = BTreeMap::new();
        for row in &rows {
            if let (OwnedDatum::Int(key), OwnedDatum::Text(label)) = (&row[0], &row[1]) {
                model.insert(*key, label.clone());
            }
        }
        for (key, label) in &model {
            let found = tree
                .point(pool, &[Datum::Int(*key)])
                .unwrap()
                .expect("model key is in the tree");
            assert_eq!(found[1], OwnedDatum::Text(label.clone()));
        }
        let scanned = tree.rows(pool).unwrap();
        assert_eq!(scanned.len(), model.len());
    }

    /// A tree with a compound key uses the general encoding and still descends
    /// correctly, including where two rows share a leading column.
    #[test]
    fn a_compound_key_tree_descends() {
        let vfs = MemoryVfs::new();
        let path = DbPath::new("compound.rdb");
        let mut database = Database::create(
            &vfs,
            &path,
            Options::default().with_page_size(512).with_frames(256),
        )
        .unwrap();
        let columns = vec![
            ColumnSpec::key(PhysicalType::Int64),
            ColumnSpec::key(PhysicalType::Text),
        ];
        let labels: Vec<String> = (0..900).map(|n| format!("t{:04}", n)).collect();
        let owned: Vec<Vec<OwnedDatum>> = (0..900)
            .map(|n| {
                vec![
                    OwnedDatum::Int(n / 10),
                    OwnedDatum::Text(labels[n as usize].clone().into_bytes()),
                ]
            })
            .collect();
        let borrowed: Vec<Vec<Datum<'_>>> = owned
            .iter()
            .map(|row| row.iter().map(OwnedDatum::borrow).collect())
            .collect();
        let tree = PagedTree::bulk_build(&mut database, 11, columns, 2, &borrowed).unwrap();
        assert_eq!(tree.key_encoding(), KeyEncoding::General);
        assert!(tree.height() >= 1);
        tree.check(database.pool()).unwrap();
        for index in (0..owned.len()).step_by(11) {
            let probe: Vec<Datum<'_>> = owned[index].iter().map(OwnedDatum::borrow).collect();
            assert!(
                tree.point(database.pool(), &probe[..2]).unwrap().is_some(),
                "row {index} was not found"
            );
        }
    }

    /// A range span visits exactly the rows between the bounds, and no leaf
    /// beyond the upper one.
    #[test]
    fn a_range_span_is_exactly_the_rows_between_the_bounds() {
        let (database, tree, _) = build(3_000, 512);
        let pool = database.pool();
        for (low, high, inclusive) in [
            (100i64, 400i64, true),
            (100, 400, false),
            (0, 0, true),
            (2_999, 2_999, true),
            (1_500, 1_500, false),
        ] {
            let mut seen: Vec<i64> = Vec::new();
            let mut leaves = 0usize;
            tree.visit_span(
                pool,
                Some(&[Datum::Int(low)]),
                true,
                Some(&[Datum::Int(high)]),
                inclusive,
                &mut |leaf, start, end| {
                    leaves = leaves.saturating_add(1);
                    for row in start..end {
                        seen.push(leaf.value(row, 0)?.as_int().unwrap_or(-1));
                    }
                    Ok(true)
                },
            )
            .unwrap();
            let wanted: Vec<i64> = if inclusive {
                (low..=high).collect()
            } else {
                (low..high).collect()
            };
            assert_eq!(seen, wanted, "{low}..{high} inclusive={inclusive}");
        }
        // An unbounded range is the whole tree.
        let mut count = 0usize;
        tree.visit_span(pool, None, true, None, true, &mut |_, start, end| {
            count = count.saturating_add(end - start);
            Ok(true)
        })
        .unwrap();
        assert_eq!(count, 3_000);
    }

    /// A bound shorter than the key selects the whole run sharing it, which is
    /// what an index nested loop over a key prefix needs.
    #[test]
    fn a_prefix_bound_selects_the_whole_run() {
        let vfs = MemoryVfs::new();
        let path = DbPath::new("prefix.rdb");
        let mut database = Database::create(
            &vfs,
            &path,
            Options::default().with_page_size(512).with_frames(256),
        )
        .unwrap();
        let columns = vec![
            ColumnSpec::key(PhysicalType::Int64),
            ColumnSpec::key(PhysicalType::Int64),
        ];
        let owned: Vec<Vec<OwnedDatum>> = (0..600i64)
            .map(|n| vec![OwnedDatum::Int(n / 3), OwnedDatum::Int(n)])
            .collect();
        let borrowed: Vec<Vec<Datum<'_>>> = owned
            .iter()
            .map(|row| row.iter().map(OwnedDatum::borrow).collect())
            .collect();
        let tree = PagedTree::bulk_build(&mut database, 8, columns, 2, &borrowed).unwrap();
        for owner in [0i64, 5, 99, 199] {
            let mut seen: Vec<i64> = Vec::new();
            tree.visit_span(
                database.pool(),
                Some(&[Datum::Int(owner)]),
                true,
                Some(&[Datum::Int(owner)]),
                true,
                &mut |leaf, start, end| {
                    for row in start..end {
                        seen.push(leaf.value(row, 1)?.as_int().unwrap_or(-1));
                    }
                    Ok(true)
                },
            )
            .unwrap();
            assert_eq!(
                seen,
                vec![owner * 3, owner * 3 + 1, owner * 3 + 2],
                "owner {owner}"
            );
        }
        // An exclusive upper bound on a prefix excludes the whole run.
        let mut count = 0usize;
        tree.visit_span(
            database.pool(),
            Some(&[Datum::Int(5)]),
            true,
            Some(&[Datum::Int(5)]),
            false,
            &mut |_, start, end| {
                count = count.saturating_add(end - start);
                Ok(true)
            },
        )
        .unwrap();
        assert_eq!(count, 0);
    }

    /// A range whose lower bound falls past the end of the leaf the descent
    /// lands on still finds its rows.
    ///
    /// The regression test for the bug the page-size sweep found. The descent
    /// goes to the last child whose separator is not above the probe, so a
    /// bound that sits between two leaves lands on the earlier one - and the
    /// first version stopped there and returned nothing. Every boundary in the
    /// tree is tried, because the bug only shows at one.
    #[test]
    fn a_bound_between_two_leaves_finds_the_rows_after_it() {
        let (database, tree, _) = build(4_000, 512);
        let pool = database.pool();
        // The first key of every leaf, so a bound of "one less" sits exactly on
        // a boundary.
        let mut boundaries: Vec<i64> = Vec::new();
        tree.visit_leaves(pool, &mut |leaf| {
            if leaf.row_count() > 0 {
                boundaries.push(leaf.value(0, 0)?.as_int().unwrap_or(-1));
            }
            Ok(true)
        })
        .unwrap();
        assert!(boundaries.len() > 20, "the tree has too few leaves to test");
        for first in boundaries {
            for low in [first.saturating_sub(1), first, first.saturating_add(1)] {
                let high = low.saturating_add(3);
                let mut seen: Vec<i64> = Vec::new();
                tree.visit_span(
                    pool,
                    Some(&[Datum::Int(low)]),
                    true,
                    Some(&[Datum::Int(high)]),
                    true,
                    &mut |leaf, start, end| {
                        for row in start..end {
                            seen.push(leaf.value(row, 0)?.as_int().unwrap_or(-1));
                        }
                        Ok(true)
                    },
                )
                .unwrap();
                let wanted: Vec<i64> = (low.max(0)..=high.min(3_999)).collect();
                assert_eq!(seen, wanted, "range {low}..={high} at boundary {first}");
            }
        }
    }

    /// An exclusive lower bound excludes the key it names.
    #[test]
    fn an_exclusive_lower_bound_excludes_its_key() {
        let (database, tree, _) = build(2_000, 512);
        let pool = database.pool();
        for low in [0i64, 1, 999, 1_999] {
            for inclusive in [true, false] {
                let mut seen: Vec<i64> = Vec::new();
                tree.visit_span(
                    pool,
                    Some(&[Datum::Int(low)]),
                    inclusive,
                    Some(&[Datum::Int(low.saturating_add(3))]),
                    true,
                    &mut |leaf, start, end| {
                        for row in start..end {
                            seen.push(leaf.value(row, 0)?.as_int().unwrap_or(-1));
                        }
                        Ok(true)
                    },
                )
                .unwrap();
                let first = if inclusive {
                    low
                } else {
                    low.saturating_add(1)
                };
                let wanted: Vec<i64> = (first..=low.saturating_add(3))
                    .filter(|key| (0..2_000).contains(key))
                    .collect();
                assert_eq!(seen, wanted, "low {low} inclusive {inclusive}");
            }
        }
    }

    /// A range whose bounds fall outside the tree returns nothing rather than
    /// everything.
    #[test]
    fn a_range_outside_the_tree_is_empty() {
        let (database, tree, _) = build(1_000, 512);
        let pool = database.pool();
        let mut count = 0usize;
        tree.visit_span(
            pool,
            Some(&[Datum::Int(5_000)]),
            true,
            Some(&[Datum::Int(6_000)]),
            true,
            &mut |_, start, end| {
                count = count.saturating_add(end - start);
                Ok(true)
            },
        )
        .unwrap();
        assert_eq!(count, 0);
        let mut count = 0usize;
        tree.visit_span(
            pool,
            Some(&[Datum::Int(-100)]),
            true,
            Some(&[Datum::Int(-1)]),
            true,
            &mut |_, start, end| {
                count = count.saturating_add(end - start);
                Ok(true)
            },
        )
        .unwrap();
        assert_eq!(count, 0);
    }

    /// The equality walk finds the same rows the general span does, at every
    /// run length including one that crosses a leaf boundary.
    #[test]
    fn the_equality_walk_matches_the_general_span() {
        let vfs = MemoryVfs::new();
        let path = DbPath::new("equal.rdb");
        let mut database = Database::create(
            &vfs,
            &path,
            Options::default().with_page_size(512).with_frames(256),
        )
        .unwrap();
        let columns = vec![
            ColumnSpec::key(PhysicalType::Int64),
            ColumnSpec::key(PhysicalType::Int64),
        ];
        // Run lengths of 1, 3 and 40, so the linear walk, its cap and the
        // bisected fallback are all exercised, and 40 is long enough to cross a
        // leaf at this page size.
        let mut owned: Vec<Vec<OwnedDatum>> = Vec::new();
        let mut rowid = 0i64;
        for owner in 0..200i64 {
            let run = match owner % 3 {
                0 => 1,
                1 => 3,
                _ => 40,
            };
            for _ in 0..run {
                owned.push(vec![OwnedDatum::Int(owner), OwnedDatum::Int(rowid)]);
                rowid += 1;
            }
        }
        let borrowed: Vec<Vec<Datum<'_>>> = owned
            .iter()
            .map(|row| row.iter().map(OwnedDatum::borrow).collect())
            .collect();
        let tree = PagedTree::bulk_build(&mut database, 12, columns, 2, &borrowed).unwrap();
        tree.check(database.pool()).unwrap();
        for owner in [-1i64, 0, 1, 2, 3, 100, 199, 200] {
            let mut by_span: Vec<i64> = Vec::new();
            tree.visit_span(
                database.pool(),
                Some(&[Datum::Int(owner)]),
                true,
                Some(&[Datum::Int(owner)]),
                true,
                &mut |leaf, start, end| {
                    for row in start..end {
                        by_span.push(leaf.value(row, 1)?.as_int().unwrap_or(-1));
                    }
                    Ok(true)
                },
            )
            .unwrap();
            let mut by_equal: Vec<i64> = Vec::new();
            tree.visit_equal(
                database.pool(),
                &[Datum::Int(owner)],
                &mut |leaf, start, end| {
                    for row in start..end {
                        by_equal.push(leaf.value(row, 1)?.as_int().unwrap_or(-1));
                    }
                    Ok(true)
                },
            )
            .unwrap();
            assert_eq!(by_equal, by_span, "owner {owner}");
        }
    }

    /// A reverse span walks down from the bound and stops when told to, which
    /// is `ORDER BY ... DESC LIMIT n`.
    #[test]
    fn a_reverse_span_walks_down_from_the_bound() {
        let (database, tree, _) = build(3_000, 512);
        let pool = database.pool();
        let mut seen: Vec<i64> = Vec::new();
        tree.visit_span_reverse(
            pool,
            None,
            true,
            Some(&[Datum::Int(2_000)]),
            true,
            &mut |leaf: &LeafRef<'_>, start, end| {
                for row in (start..end).rev() {
                    seen.push(leaf.value(row, 0)?.as_int().unwrap_or(-1));
                    if seen.len() >= 50 {
                        return Ok(false);
                    }
                }
                Ok(true)
            },
        )
        .unwrap();
        let wanted: Vec<i64> = (1_951..=2_000).rev().collect();
        assert_eq!(seen, wanted);

        // From the end of the tree.
        let mut seen: Vec<i64> = Vec::new();
        tree.visit_span_reverse(
            pool,
            None,
            true,
            None,
            true,
            &mut |leaf: &LeafRef<'_>, start, end| {
                for row in (start..end).rev() {
                    seen.push(leaf.value(row, 0)?.as_int().unwrap_or(-1));
                    if seen.len() >= 3 {
                        return Ok(false);
                    }
                }
                Ok(true)
            },
        )
        .unwrap();
        assert_eq!(seen, vec![2_999, 2_998, 2_997]);
    }

    /// A skip scan visits one row per distinct prefix, in order, and does far
    /// fewer page fetches than a scan of every row would.
    #[test]
    fn a_skip_scan_visits_each_distinct_prefix_once() {
        let vfs = MemoryVfs::new();
        let path = DbPath::new("skip.rdb");
        let mut database = Database::create(
            &vfs,
            &path,
            Options::default().with_page_size(512).with_frames(512),
        )
        .unwrap();
        let columns = vec![
            ColumnSpec::key(PhysicalType::Int64),
            ColumnSpec::key(PhysicalType::Int64),
        ];
        // 64 distinct leading values over 6,400 rows.
        let owned: Vec<Vec<OwnedDatum>> = (0..6_400i64)
            .map(|n| vec![OwnedDatum::Int(n / 100), OwnedDatum::Int(n)])
            .collect();
        let borrowed: Vec<Vec<Datum<'_>>> = owned
            .iter()
            .map(|row| row.iter().map(OwnedDatum::borrow).collect())
            .collect();
        let tree = PagedTree::bulk_build(&mut database, 5, columns, 2, &borrowed).unwrap();
        tree.check(database.pool()).unwrap();
        database.pool().reset_stats();
        let mut distinct: Vec<i64> = Vec::new();
        tree.skip_scan(database.pool(), 1, &mut |values| {
            distinct.push(values.first().and_then(Datum::as_int).unwrap_or(-1));
            Ok(true)
        })
        .unwrap();
        assert_eq!(distinct, (0..64).collect::<Vec<i64>>());
        // What a skip scan saves is *row reads*, not page fetches, and the
        // distinction is worth pinning down because the first version of this
        // test asserted on fetches and failed while the scan was working
        // correctly. Seeking to 64 values in a 291-leaf tree costs one descent
        // each - a few page fetches per descent, all of them hits - where
        // walking costs 291 leaf fetches *and* 6,400 row reads. The fetch
        // counts are the same order of magnitude; the row reads are two orders
        // apart, and that is the whole of the algorithm's advantage.
        assert_eq!(distinct.len(), 64, "one row read per distinct value");
        let fetches = database.pool().stats().hits + database.pool().stats().misses;
        assert!(
            fetches < 64 * u64::from(tree.height() + 3),
            "a skip scan made {fetches} fetches for 64 seeks down {} levels",
            tree.height()
        );
        // Stopping early stops.
        let mut count = 0usize;
        tree.skip_scan(database.pool(), 1, &mut |_| {
            count = count.saturating_add(1);
            Ok(count < 5)
        })
        .unwrap();
        assert_eq!(count, 5);
        assert!(tree
            .skip_scan(database.pool(), 0, &mut |_| Ok(true))
            .is_err());
        assert!(tree
            .skip_scan(database.pool(), 9, &mut |_| Ok(true))
            .is_err());
    }

    /// A skip scan over a tree whose prefix is unique per row degenerates to a
    /// full walk and still answers correctly, which is the case the physical
    /// pass has to avoid choosing rather than the case this has to refuse.
    #[test]
    fn a_skip_scan_over_unique_keys_still_answers() {
        let (database, tree, _) = build(500, 512);
        let mut seen: Vec<i64> = Vec::new();
        tree.skip_scan(database.pool(), 1, &mut |values| {
            seen.push(values.first().and_then(Datum::as_int).unwrap_or(-1));
            Ok(true)
        })
        .unwrap();
        assert_eq!(seen, (0..500).collect::<Vec<i64>>());
    }

    /// A tree of one leaf has that leaf as its root and still answers.
    #[test]
    fn a_single_leaf_tree_is_its_own_root() {
        let (database, tree, _) = build(3, 8_192);
        assert_eq!(tree.height(), 0);
        assert_eq!(tree.leaf_count(), 1);
        assert_eq!(tree.root(), tree.first_leaf());
        tree.check(database.pool()).unwrap();
        assert!(tree
            .point(database.pool(), &[Datum::Int(1)])
            .unwrap()
            .is_some());
    }

    /// An empty tree is one empty leaf, so nothing has to special-case a root
    /// that does not exist.
    #[test]
    fn an_empty_tree_is_one_empty_leaf() {
        let vfs = MemoryVfs::new();
        let path = DbPath::new("empty.rdb");
        let mut database =
            Database::create(&vfs, &path, Options::default().with_page_size(512)).unwrap();
        let tree =
            PagedTree::bulk_build::<Vec<Datum<'_>>>(&mut database, 3, rowid_columns(), 1, &[])
                .unwrap();
        assert_eq!(tree.leaf_count(), 1);
        assert_eq!(tree.row_count(), 0);
        assert!(tree.rows(database.pool()).unwrap().is_empty());
        assert!(tree
            .point(database.pool(), &[Datum::Int(1)])
            .unwrap()
            .is_none());
        tree.check(database.pool()).unwrap();
    }

    /// A tree survives a checkpoint and a reopen with a pool far too small to
    /// hold it, which is the eviction path under a descent.
    #[test]
    fn a_tree_reads_correctly_through_a_tiny_pool() {
        let vfs = MemoryVfs::new();
        let path = DbPath::new("tiny.rdb");
        let (root, leaves, count) = {
            let mut database = Database::create(
                &vfs,
                &path,
                Options::default().with_page_size(512).with_frames(512),
            )
            .unwrap();
            let labels: Vec<String> = (0..4_000).map(|n| format!("label-{n:06}")).collect();
            let owned: Vec<Vec<OwnedDatum>> = (0..4_000i64)
                .map(|n| {
                    vec![
                        OwnedDatum::Int(n),
                        OwnedDatum::Text(labels[n as usize].clone().into_bytes()),
                    ]
                })
                .collect();
            let borrowed: Vec<Vec<Datum<'_>>> = owned
                .iter()
                .map(|row| row.iter().map(OwnedDatum::borrow).collect())
                .collect();
            let tree =
                PagedTree::bulk_build(&mut database, 7, rowid_columns(), 1, &borrowed).unwrap();
            database.set_catalog_root(tree.root());
            database.checkpoint().unwrap();
            (tree.root(), tree.leaf_count(), tree.row_count())
        };
        let database = Database::open(&vfs, &path, 64).unwrap();
        let tree =
            PagedTree::attach(database.pool(), 7, root, rowid_columns(), 1, leaves, count).unwrap();
        assert_eq!(tree.root(), root);
        tree.check(database.pool()).unwrap();
        for key in (0..4_000i64).step_by(37) {
            let found = tree
                .point(database.pool(), &[Datum::Int(key)])
                .unwrap()
                .expect("row survived eviction");
            assert_eq!(found[0], OwnedDatum::Int(key));
        }
        assert!(
            database.pool().stats().evicted > 0,
            "a 64-frame pool over a 4,000-row tree must have evicted"
        );
        assert_eq!(tree.rows(database.pool()).unwrap().len(), 4_000);
    }

    /// A descent that races a frame reload restarts rather than reading a page
    /// that is no longer there. The race is forced: the frame's version is
    /// bumped between the observation and the validation.
    #[test]
    fn a_descent_validates_what_it_read() {
        let (database, tree, _) = build(2_000, 512);
        let pool = database.pool();
        let guard = pool.fetch(tree.root()).unwrap();
        let frame = guard.frame();
        let observed = pool.observe(frame).expect("the root admits a reader");
        assert!(pool.validate(frame, observed));
        drop(guard);
        // A writer taking and releasing the latch is exactly what an eviction
        // or a load does, and it must invalidate the observation.
        let latch = pool.latch(frame).expect("the frame has a latch");
        assert!(latch.try_exclusive());
        latch.release_exclusive();
        assert!(!pool.validate(frame, observed));
        // And the descent itself still answers, because it restarts.
        assert!(tree.point(pool, &[Datum::Int(1_000)]).unwrap().is_some());
    }

    /// Swizzling happens: after a descent, the root's slot for the child taken
    /// holds a frame rather than a page id, and the tree still answers.
    #[test]
    fn a_descent_swizzles_the_slot_it_took() {
        let (database, tree, _) = build(2_000, 512);
        let pool = database.pool();
        assert!(tree.point(pool, &[Datum::Int(500)]).unwrap().is_some());
        let guard = pool.fetch(tree.root()).unwrap();
        let interior = InteriorRef::parse(&guard).unwrap();
        let swizzled = (0..interior.children())
            .filter_map(|child| interior.swip(child).ok())
            .filter(|swip| !swip.is_unswizzled())
            .count();
        assert!(swizzled > 0, "no slot of the root was swizzled");
        drop(guard);
        assert!(tree.point(pool, &[Datum::Int(500)]).unwrap().is_some());
        assert!(tree.point(pool, &[Datum::Int(1_900)]).unwrap().is_some());
    }

    /// The rowid encoding is chosen only for a single non-nullable integer key,
    /// and it is exact where the general one would be eighteen bytes.
    #[test]
    fn the_rowid_encoding_is_chosen_only_when_it_applies() {
        let rowid = vec![ColumnSpec::key(PhysicalType::Int64)];
        assert_eq!(KeyEncoding::choose(&rowid, 1), KeyEncoding::Rowid);
        let nullable = vec![ColumnSpec::new(PhysicalType::Int64)];
        assert_eq!(KeyEncoding::choose(&nullable, 1), KeyEncoding::General);
        let text = vec![ColumnSpec::key(PhysicalType::Text)];
        assert_eq!(KeyEncoding::choose(&text, 1), KeyEncoding::General);
        let two = vec![
            ColumnSpec::key(PhysicalType::Int64),
            ColumnSpec::key(PhysicalType::Int64),
        ];
        assert_eq!(KeyEncoding::choose(&two, 2), KeyEncoding::General);
        assert_eq!(KeyEncoding::choose(&[], 1), KeyEncoding::General);

        assert_eq!(KeyEncoding::Rowid.encode(&[Datum::Int(1)]).len(), 8);
        assert_eq!(KeyEncoding::General.encode(&[Datum::Int(1)]).len(), 18);
        // The rowid encoding is order preserving over the whole integer range.
        let samples = [i64::MIN, -1, 0, 1, 1 << 60, i64::MAX];
        for left in samples {
            for right in samples {
                assert_eq!(
                    KeyEncoding::Rowid
                        .encode(&[Datum::Int(left)])
                        .cmp(&KeyEncoding::Rowid.encode(&[Datum::Int(right)])),
                    left.cmp(&right),
                    "{left} vs {right}"
                );
            }
        }
        // The odd probes are total rather than correct-by-luck.
        assert_eq!(KeyEncoding::Rowid.encode(&[]).len(), 0);
        assert_eq!(KeyEncoding::Rowid.encode(&[Datum::Null]).len(), 8);
        assert_eq!(
            KeyEncoding::Rowid.encode(&[Datum::Text(b"x")]),
            vec![0xFF; 8]
        );
        assert_eq!(KeyEncoding::Rowid.encode(&[Datum::Real(2.5)]).len(), 8);
        assert_eq!(
            KeyEncoding::Rowid.encode(&[Datum::Real(f64::MAX)]),
            KeyEncoding::Rowid.encode(&[Datum::Int(i64::MAX)])
        );
        assert_eq!(
            KeyEncoding::Rowid.encode(&[Datum::Real(f64::MIN)]),
            KeyEncoding::Rowid.encode(&[Datum::Int(i64::MIN)])
        );
    }

    /// A tree whose separators were corrupted is caught by the checker rather
    /// than by a wrong answer.
    #[test]
    fn a_corrupt_separator_is_caught_by_the_checker() {
        let (database, tree, _) = build(2_000, 512);
        let pool = database.pool();
        tree.check(pool).unwrap();
        // Rewrite one separator to a key that is not its child's first.
        let root = tree.root();
        let (offset, length) = {
            let guard = pool.fetch(root).unwrap();
            let interior = InteriorRef::parse(&guard).unwrap();
            assert!(interior.count() > 0);
            let key = interior.key(0).unwrap();
            let base = guard.bytes().as_ptr() as usize;
            let _ = base;
            (
                key.as_ptr() as usize - guard.bytes().as_ptr() as usize,
                key.len(),
            )
        };
        pool.modify(root, |bytes| {
            if let Some(slot) = bytes.get_mut(offset..offset + length) {
                for byte in slot.iter_mut() {
                    *byte = 0x01;
                }
            }
            Ok(())
        })
        .unwrap();
        assert!(tree.check(pool).is_err(), "the checker missed it");
    }

    /// The attach path refuses a root that is not a tree page.
    #[test]
    fn attaching_to_a_page_that_is_not_a_root_is_refused() {
        let vfs = MemoryVfs::new();
        let path = DbPath::new("notaroot.rdb");
        let mut database =
            Database::create(&vfs, &path, Options::default().with_page_size(512)).unwrap();
        let page = database.allocate(1).unwrap();
        let mut image = vec![0u8; 512];
        page::write_common(&mut image, PageKind::BlobExtent, 0, 1).unwrap();
        database.install(page, &image).unwrap();
        assert!(PagedTree::attach(database.pool(), 1, page, rowid_columns(), 1, 0, 0).is_err());
    }
}
