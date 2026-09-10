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
//! [`crate::paged::tests`] forces exactly that race.

use std::cell::RefCell;

use inillucent_base::error::{corrupt, misuse};
use inillucent_base::DbResult;
use inillucent_pool::extent::{self, ExtentRef};
use inillucent_pool::interior::{InteriorBuilder, InteriorRef};
use inillucent_pool::page::{self, PageKind};
use inillucent_pool::{Database, PageGuard, PageId, Pool, Swip};

use inillucent_value::collation::Collation;

use crate::datum::{Datum, OwnedDatum};
use crate::key;
use crate::leaf::{Extents, Hit, LeafBuilder, LeafRef, Spill};
use crate::tree::BULK_FILL;
use crate::types::{ColumnSpec, PhysicalType};

/// How many times a descent retries an optimistic read before giving up.
///
/// The TDD's four, taken from the pool so the two cannot drift.
const RESTARTS: u32 = inillucent_pool::OPTIMISTIC_RETRIES;

/// How a tree's keys become comparable bytes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum KeyEncoding {
    /// One `Int64` column, encoded as eight exact bytes.
    Rowid,
    /// Anything else, through the general memcmp encoding.
    General,
}

/// The lowest and the highest key under a subtree, either absent when the
/// subtree holds no live row.
pub type KeyRange = (Option<Vec<u8>>, Option<Vec<u8>>);

impl KeyEncoding {
    /// Chooses the encoding a column directory calls for.
    ///
    /// The rowid form is taken only for a single non-nullable `Int64` key
    /// column, because it has no way to represent a NULL, a text or a real -
    /// and a rowid, by definition, is never any of those.
    ///
    /// @param columns - the column directory, key columns first
    /// @param key_columns - how many leading columns form the key
    pub fn choose(columns: &[ColumnSpec], key_columns: usize) -> KeyEncoding {
        if key_columns != 1 {
            return KeyEncoding::General;
        }
        match columns.first() {
            Some(spec) if spec.physical == PhysicalType::Int64 && !spec.nullable() => {
                KeyEncoding::Rowid
            }
            _ => KeyEncoding::General,
        }
    }

    /// Encodes a key tuple into comparable bytes.
    ///
    /// A rowid tree's probe that is not an integer falls back to the general
    /// encoding for that one comparison, which cannot match any separator and
    /// therefore lands the descent at the leftmost or rightmost leaf rather
    /// than somewhere arbitrary. The leaf's own search then answers correctly.
    ///
    /// @param self - the tree's encoding
    /// @param values - the key tuple
    pub fn encode(self, values: &[Datum<'_>]) -> Vec<u8> {
        self.encode_under(values, &[])
    }

    /// Encodes a key tuple whose columns have collations.
    ///
    /// @param values - the key tuple
    /// @param collations - one per column; short means BINARY for the rest
    pub fn encode_under(self, values: &[Datum<'_>], collations: &[Collation]) -> Vec<u8> {
        self.encode_ordered(values, collations, &[])
    }

    /// Encodes a key tuple whose columns have collations and directions.
    ///
    /// @param values - the key tuple
    /// @param collations - one per column; short means BINARY for the rest
    /// @param descending - one per column; short means ascending for the rest
    pub fn encode_ordered(
        self,
        values: &[Datum<'_>],
        collations: &[Collation],
        descending: &[bool],
    ) -> Vec<u8> {
        let mut out = Vec::new();
        self.encode_into(values, collations, descending, &mut out);
        out
    }

    /// Appends a key tuple's comparable bytes to a buffer.
    ///
    /// **The appending form is what lets a bulk build keep every key in one
    /// arena.** `CREATE INDEX` needs a key per row to sort by, and returning a
    /// `Vec` per row is one heap allocation per row - the exact cost that made
    /// the previous attempt at pre-encoded keys slower than the comparison sort
    /// it replaced. With a single buffer and a run of offsets there is no
    /// per-row allocation at all, and the bytes are the tree's own encoding, so
    /// sorting by `memcmp` over them is sorting by the order the tree is read
    /// in rather than by a second opinion about it.
    ///
    /// @param values - the key tuple
    /// @param collations - one per column; short means BINARY for the rest
    /// @param out - the buffer to append to
    pub fn encode_into(
        self,
        values: &[Datum<'_>],
        collations: &[Collation],
        descending: &[bool],
        out: &mut Vec<u8>,
    ) {
        match (self, values.first()) {
            (KeyEncoding::Rowid, Some(Datum::Int(number))) if values.len() == 1 => {
                out.extend_from_slice(&key::order_preserving_int(*number));
            }
            (KeyEncoding::Rowid, Some(Datum::Real(number))) if values.len() == 1 => {
                // A real probe against a rowid tree: clamp to the integer it
                // sits between, so the descent lands on the leaf that could
                // hold it rather than at an end of the tree.
                let clamped = if *number <= i64::MIN as f64 {
                    i64::MIN
                } else if *number >= i64::MAX as f64 {
                    i64::MAX
                } else {
                    number.floor() as i64
                };
                out.extend_from_slice(&key::order_preserving_int(clamped));
            }
            (KeyEncoding::Rowid, Some(Datum::Null)) => {
                out.extend_from_slice(&key::order_preserving_int(i64::MIN));
            }
            (KeyEncoding::Rowid, None) => {}
            (KeyEncoding::Rowid, Some(_)) => out.extend_from_slice(&[0xFF; 8]),
            (KeyEncoding::General, _) => {
                for (index, value) in values.iter().enumerate() {
                    let collation = collations.get(index).copied().unwrap_or(Collation::Binary);
                    let start = out.len();
                    key::encode_into_with(value, collation, out);
                    // **A descending column is its own bytes, inverted.** The
                    // encoding is order preserving, so complementing every byte
                    // of one column's span reverses that column's order and
                    // leaves every other column's alone - which is what a
                    // `DESC` key column means, and is why the comparison below
                    // and the sort the bulk build does over these bytes both
                    // come out right with no further change.
                    if descending.get(index).copied().unwrap_or(false) {
                        if let Some(span) = out.get_mut(start..) {
                            for byte in span {
                                *byte = !*byte;
                            }
                        }
                    }
                }
            }
        }
    }
}

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
    if pages <= 1 {
        return write_packed_extent(database, log, tree_id, value);
    }
    let first = database.allocate(pages)?;
    let images = extent::encode_run(value, first, page_size, tree_id)?;
    for (id, mut image) in images {
        log_built_page(&mut Some(log), database, id, &mut image)?;
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

/// Describes one bulk-built page in the log, then installs it.
///
/// Two records rather than one: the allocation and the contents are separate
/// facts and recovery needs both. `AllocPage` is what stops a later allocation
/// handing the same page out twice after a crash; `WritePage` is what puts the
/// bytes back. The image is stamped with the write's LSN before it is
/// installed, so the page-LSN rule holds for a bulk-built page exactly as it
/// does for one a split wrote.
///
/// With no log the image is installed unstamped, which is the byte-for-byte
/// behaviour the unlogged builder has always had.
///
/// @param log - where the records go, when there is one
/// @param database - the file the page is installed in
/// @param id - the page, already allocated
/// @param image - the page bytes, stamped in place with the LSN
fn log_built_page(
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
    /// Builds a tree bottom-up from rows already sorted by key.
    ///
    /// Leaves are packed left to right at [`BULK_FILL`] and never revisited,
    /// then each interior level is built from the level below it, then the root
    /// is whatever the last level left. One pass, no per-key descent - the
    /// TDD's bulk builder, and what the fixture import uses.
    ///
    /// @param database - the file the pages are allocated and installed in
    /// @param tree_id - the identifier stamped into every page
    /// @param columns - the column directory, key columns first
    /// @param key_columns - how many leading columns form the key
    /// @param rows - the rows, already sorted by the key columns
    pub fn bulk_build<'d, R: AsRef<[Datum<'d>]>>(
        database: &mut Database,
        tree_id: u64,
        columns: Vec<ColumnSpec>,
        key_columns: usize,
        rows: &[R],
    ) -> DbResult<PagedTree> {
        PagedTree::bulk_build_logged(database, None, tree_id, columns, key_columns, rows)
    }

    /// Builds a tree bottom-up, describing every page in the log first.
    ///
    /// The same builder as [`PagedTree::bulk_build`], with the write-ahead rule
    /// applied to it: every page it allocates is an `AllocPage` record and every
    /// page it packs is a `WritePage` record carrying the whole image, so redo
    /// is a copy and a `CREATE INDEX` that crashed half-way is either wholly
    /// there after recovery or wholly absent.
    ///
    /// A whole-image record per page is the right record here and a small one
    /// would be wrong. The logical-redo argument that made a compaction
    /// twenty-four bytes in Phase 3 relies on the page already being in the
    /// state the operation started from; a bulk build's pages did not exist
    /// before it, so there is no such state and the image *is* the instruction.
    ///
    /// `None` for the log is the import's case: it builds into a file nothing
    /// has read, checkpoints it, and reopens it, so there is no window in which
    /// a log would be consulted. Passing `None` writes the same bytes the
    /// unlogged builder always wrote, LSN field included, which is what keeps
    /// the import's byte-for-byte round-trip check meaningful.
    ///
    /// @param database - the file the pages are allocated and installed in
    /// @param log - where the records go, when the build is inside a transaction
    /// @param tree_id - the identifier stamped into every page
    /// @param columns - the column directory, key columns first
    /// @param key_columns - how many leading columns form the key
    /// @param rows - the rows, already sorted by the key columns
    pub fn bulk_build_logged<'d, R: AsRef<[Datum<'d>]>>(
        database: &mut Database,
        log: Option<&mut dyn crate::write::TreeLog>,
        tree_id: u64,
        columns: Vec<ColumnSpec>,
        key_columns: usize,
        rows: &[R],
    ) -> DbResult<PagedTree> {
        PagedTree::bulk_build_rows(
            database,
            log,
            tree_id,
            columns,
            key_columns,
            &crate::leaf::RowSlice(rows),
        )
    }

    /// Builds a tree bottom-up from a row source, holding one leaf at a time.
    ///
    /// **Two passes over the sizing arithmetic, and one leaf image in memory.**
    /// The builder allocates its leaves as one contiguous run, so it has to know
    /// the leaf count before it writes the first one. It used to find
    /// that out by packing every leaf into a `Vec<Vec<u8>>` and taking its
    /// length, which is a whole second copy of the tree held live for the sake
    /// of one integer: 6.2 MiB of a `CREATE INDEX` whose entire resident cost
    /// was 28.9 MiB.
    ///
    /// [`LeafBuilder::fit`] answers the same question without encoding
    /// anything, so the count is taken first, the run is allocated, and each
    /// leaf is then packed into a buffer that is logged and dropped before the
    /// next one is made. The two passes see the same values and price the same
    /// spill threshold, so they agree by construction rather than by luck - and
    /// the counting pass writes nothing, which is what makes running it twice
    /// safe.
    ///
    /// One thing does move: an oversized value's extent pages are now allocated
    /// **after** the leaf run rather than interleaved with it, because the run
    /// is claimed before any packing happens. The leaves are therefore *more*
    /// contiguous than they were, which is the property the run exists for.
    ///
    /// @param database - the file the pages are allocated and installed in
    /// @param log - where the records go, when the build is inside a transaction
    /// @param tree_id - the identifier stamped into every page
    /// @param columns - the column directory, key columns first
    /// @param key_columns - how many leading columns form the key
    /// @param rows - the rows, already sorted by the key columns
    pub fn bulk_build_rows<'d>(
        database: &mut Database,
        mut log: Option<&mut dyn crate::write::TreeLog>,
        tree_id: u64,
        columns: Vec<ColumnSpec>,
        key_columns: usize,
        rows: &dyn crate::leaf::Rows<'d>,
    ) -> DbResult<PagedTree> {
        let page_size = database.page_size();
        let encoding = KeyEncoding::choose(&columns, key_columns);
        let collations = collations_of(&columns, key_columns);
        let directions = directions_of(&columns, key_columns);
        let builder = LeafBuilder::new(page_size, tree_id, columns.clone(), key_columns)?;

        // **The bulk builder always spills, logged or not.** The import builds
        // unlogged - it writes into a file nothing has read and checkpoints it -
        // and it has to produce the same tree the DDL path produces from the
        // same rows, because `ImportedDatabase::import_with` reads the catalog
        // back and refuses if it differs from what it wrote. A builder that
        // spilled only when it had a log would make the two disagree about where
        // a four-kilobyte value lives.
        let mut nowhere = crate::write::NoLog::default();

        // Pass one: how many rows each leaf takes, and each leaf's first key as
        // a separator. Nothing is encoded, allocated or written.
        let mut boundaries: Vec<usize> = Vec::new();
        let mut separators: Vec<Vec<u8>> = Vec::new();
        let mut head: Vec<Datum<'d>> = Vec::with_capacity(key_columns);
        let mut at = 0usize;
        let mut row_count = 0u64;
        let total = rows.len();
        while at < total {
            let placed = builder.fit(rows, at, BULK_FILL, true);
            if placed == 0 {
                // Every oversized text and blob would have gone out of line, so
                // what is left is keys, fixed-width slots and sixteen bytes per
                // reference. A row that still does not fit is one whose *key* is
                // most of a page.
                return Err(misuse(
                    "a row's keys and fixed-width columns alone are larger than a page",
                ));
            }
            head.clear();
            for column in 0..key_columns {
                head.push(rows.value(at, column));
            }
            let mut separator = Vec::new();
            encoding.encode_into(&head, &collations, &directions, &mut separator);
            separators.push(separator);
            boundaries.push(placed);
            at = at.saturating_add(placed);
            row_count = row_count.saturating_add(placed as u64);
        }
        // An empty tree is still a tree: one empty leaf, so every reader has a
        // page to land on and nothing has to special-case a root that does not
        // exist.
        let empty = boundaries.is_empty();
        if empty {
            boundaries.push(0);
            separators.push(Vec::new());
        }

        // Pass two: the leaves themselves. They are allocated as one run so the
        // sibling chain is also the file's page order, which is what makes a
        // full scan sequential - and one image is live at a time.
        let leaf_pages = boundaries.len();
        let first_leaf = database.allocate(leaf_pages as u64)?;
        let mut leaves: Vec<PageId> = Vec::with_capacity(leaf_pages);
        let mut placed_at = 0usize;
        for (index, placed) in boundaries.iter().copied().enumerate() {
            let mut image = if empty {
                builder.encode_empty()?
            } else {
                let mut spiller = Extender {
                    database,
                    log: match log.as_deref_mut() {
                        Some(log) => log,
                        None => &mut nowhere,
                    },
                    tree_id,
                    written: Vec::new(),
                };
                builder.encode_rows(rows, placed_at, placed, Some(&mut spiller))?
            };
            placed_at = placed_at.saturating_add(placed);
            let id = PageId(first_leaf.0.saturating_add(index as u64));
            let right = if index.saturating_add(1) < leaf_pages {
                PageId(id.0.saturating_add(1))
            } else {
                PageId::NONE
            };
            page::set_right(&mut image, right)?;
            log_built_page(&mut log, database, id, &mut image)?;
            leaves.push(id);
        }

        // Pass two: the interior levels, bottom up.
        let mut level = 1u16;
        let mut children = leaves.clone();
        let mut child_keys = separators;
        let mut root = *leaves.first().unwrap_or(&PageId::NONE);
        let mut height = 0u16;
        while children.len() > 1 {
            let interior = InteriorBuilder::new(page_size, tree_id, level)?;
            let mut parents: Vec<PageId> = Vec::new();
            let mut parent_keys: Vec<Vec<u8>> = Vec::new();
            let mut cursor = 0usize;
            while cursor < children.len() {
                let remaining_keys: Vec<&[u8]> = child_keys
                    .get(cursor.saturating_add(1)..)
                    .unwrap_or(&[])
                    .iter()
                    .map(|key| key.as_slice())
                    .collect();
                let fit = interior
                    .capacity(&remaining_keys)
                    .min(children.len().saturating_sub(cursor))
                    .max(1);
                let group: Vec<Swip> = children
                    .get(cursor..cursor.saturating_add(fit))
                    .unwrap_or(&[])
                    .iter()
                    .map(|page| Swip::unswizzled(*page))
                    .collect();
                let group_separators: Vec<&[u8]> = child_keys
                    .get(cursor.saturating_add(1)..cursor.saturating_add(fit))
                    .unwrap_or(&[])
                    .iter()
                    .map(|key| key.as_slice())
                    .collect();
                let mut image = interior.build(&group_separators, &group)?;
                let id = database.allocate(1)?;
                log_built_page(&mut log, database, id, &mut image)?;
                parents.push(id);
                parent_keys.push(child_keys.get(cursor).cloned().unwrap_or_default());
                cursor = cursor.saturating_add(fit);
            }
            children = parents;
            child_keys = parent_keys;
            height = level;
            level = level.saturating_add(1);
            root = *children.first().unwrap_or(&PageId::NONE);
            if level > 32 {
                return Err(corrupt(
                    "the bulk builder made a tree deeper than 32 levels",
                ));
            }
        }
        let collations = collations_of(&columns, key_columns);
        let directions = directions_of(&columns, key_columns);
        Ok(PagedTree {
            tree_id,
            root,
            height,
            columns,
            key_columns,
            page_size,
            encoding,
            collations,
            directions,
            first_leaf: *leaves.first().unwrap_or(&PageId::NONE),
            leaf_count: leaves.len() as u64,
            row_count,
            scratch: RefCell::new(Vec::new()),
            stats: std::cell::Cell::new(crate::write::WriteStats::default()),
        })
    }

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
    #[allow(clippy::too_many_arguments)]
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
    }

    /// Adjusts the row count by a signed amount.
    ///
    /// @param delta - how many rows were gained or lost
    pub(crate) fn note_rows(&mut self, delta: i64) {
        self.row_count = self.row_count.saturating_add_signed(delta);
    }

    /// Returns the leftmost leaf, where a full scan starts.
    pub fn first_leaf(&self) -> PageId {
        self.first_leaf
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

    /// Descends to the leaf whose range holds a key, returning the pinned leaf.
    ///
    /// The guard comes back rather than the page id alone because every caller
    /// wants to read the leaf next, and re-fetching it would be a second page
    /// lookup for a page the descent is already holding. On `point.rowid` -
    /// the workload with the tightest budget in this phase's gate - that one
    /// lookup is a measurable share of the whole probe.
    ///
    /// @param pool - the buffer pool
    /// @param key - the encoded key being looked for
    pub fn descend_guard<'p>(
        &self,
        pool: &'p Pool,
        key: &[u8],
    ) -> DbResult<(PageGuard<'p>, PageId)> {
        for attempt in 0..=RESTARTS {
            match self.try_descend(pool, key, attempt == RESTARTS)? {
                Some(found) => return Ok(found),
                None => continue,
            }
        }
        Err(corrupt(
            "a descent restarted more times than the tree is deep",
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

    /// Returns the leftmost leaf of a tree, taking the first child at each level.
    ///
    /// Reads the child pointer directly rather than going through
    /// [`PagedTree::take_child`], because that swizzles the parent's slot as a
    /// side effect and this walk happens once, at open, before any descent has
    /// a reason to want the pointer warm.
    ///
    /// @param pool - the buffer pool
    /// @param root - the root page
    fn leftmost_leaf(pool: &Pool, root: PageId) -> DbResult<PageId> {
        let mut page = root;
        for _ in 0..=64 {
            let next = {
                let guard = pool.fetch(page)?;
                match page::kind_of(&guard)? {
                    PageKind::Leaf => return Ok(page),
                    PageKind::Interior => {
                        let interior = InteriorRef::parse(&guard)?;
                        pool.page_of_swip(interior.swip(0)?)?
                    }
                    other => return Err(corrupt(format!("a tree cannot contain {other:?}"))),
                }
            };
            page = next;
        }
        Err(corrupt("a tree is deeper than 64 levels"))
    }

    /// Descends to the leaf whose range holds a key, recording the path taken.
    ///
    /// The path costs one `Vec` push per level and is only wanted by a reverse
    /// walk, which is why it is a separate entry point from
    /// [`PagedTree::descend_guard`] rather than something every point probe
    /// pays for.
    ///
    /// @param pool - the buffer pool
    /// @param key - the encoded key being looked for
    pub fn descend(&self, pool: &Pool, key: &[u8]) -> DbResult<Descent> {
        let mut steps = Vec::with_capacity(self.height as usize);
        let mut page = self.root;
        loop {
            let child = {
                let guard = pool.fetch(page)?;
                if page::kind_of(&guard)? == PageKind::Leaf {
                    return Ok(Descent { steps, leaf: page });
                }
                InteriorRef::parse(&guard)?.child_for(key)?.0
            };
            steps.push((page, child));
            page = self.take_child(pool, page, child)?;
            if steps.len() > 64 {
                return Err(corrupt("a descent walked more than 64 levels"));
            }
        }
    }

    /// One attempt at a descent.
    ///
    /// Returns `None` when an optimistic read failed validation, which means
    /// the caller should start again. On the last attempt the reader takes
    /// shared latches instead, so a writer that keeps invalidating cannot
    /// starve it - the TDD's "after 4 restarts they descend with shared
    /// latches".
    ///
    /// The guard on the level above is held until the level below is pinned, so
    /// a pool too small to hold the path cannot evict a page this descent is
    /// still walking through. It is then dropped *before* the swizzle, because
    /// reading a frame and writing it are exclusive - and the swizzle re-checks
    /// that the frame still holds the page it was about to annotate.
    ///
    /// @param pool - the buffer pool
    /// @param key - the encoded key
    /// @param pessimistic - whether to take shared latches rather than observe
    fn try_descend<'p>(
        &self,
        pool: &'p Pool,
        key: &[u8],
        pessimistic: bool,
    ) -> DbResult<Option<(PageGuard<'p>, PageId)>> {
        let mut page = self.root;
        let mut guard = pool.fetch(page)?;
        loop {
            let frame = guard.frame();
            let observed = if pessimistic {
                None
            } else {
                pool.observe(frame)
            };
            if !pessimistic && observed.is_none() {
                return Ok(None);
            }
            if page::kind_of(&guard)? == PageKind::Leaf {
                if let Some(seen) = observed {
                    if !pool.validate(frame, seen) {
                        return Ok(None);
                    }
                }
                return Ok(Some((guard, page)));
            }
            let (swip, at) = {
                let interior = InteriorRef::parse(&guard)?;
                let (_, swip, at) = interior.child_for(key)?;
                (swip, at)
            };
            if let Some(seen) = observed {
                if !pool.validate(frame, seen) {
                    return Ok(None);
                }
            }
            // A swizzled swip names the frame directly, which is the whole
            // point of swizzling: no page-table lookup, no page id to resolve.
            //
            // `already` is why the write below is conditional. A slot that
            // already names this frame is a slot the swizzle would rewrite with
            // the bytes it already holds, and paying for that on every descent
            // is not free: it is a page-table lookup to re-check the parent, an
            // exclusive borrow of the parent's buffer, and a store into an
            // interior page that every later descent then has to re-read. The
            // first descent through a slot does the work; the millionth does
            // not need to repeat it.
            let (child_guard, target, already) = match swip.frame() {
                Some(child_frame) => {
                    let child_guard = pool.fetch_frame(child_frame)?;
                    let target = pool
                        .page_in_frame(child_frame)
                        .filter(|page| !page.is_none())
                        .ok_or_else(|| corrupt("a swizzled swip names an empty frame"))?;
                    (child_guard, target, true)
                }
                None => {
                    let target = pool.page_of_swip(swip)?;
                    let child_guard = pool.fetch(target)?;
                    pool.note_parent(child_guard.frame(), frame, page, at);
                    (child_guard, target, false)
                }
            };
            let child_frame = child_guard.frame();
            let parent_page = page;
            drop(guard);
            if !already {
                pool.swizzle_into(frame, parent_page, at, Swip::swizzled(child_frame))?;
            }
            guard = child_guard;
            page = target;
        }
    }

    /// Follows one child swip, swizzling the parent's slot.
    ///
    /// The shared half of [`PagedTree::try_descend`], used by the walks that
    /// take a fixed child rather than searching for one: the rightmost spine
    /// and the step-left of a reverse scan.
    ///
    /// @param pool - the buffer pool
    /// @param parent - the parent page
    /// @param child - which child to take
    fn take_child(&self, pool: &Pool, parent: PageId, child: usize) -> DbResult<PageId> {
        let (frame, at, target, child_frame) = {
            let guard = pool.fetch(parent)?;
            let frame = guard.frame();
            let interior = InteriorRef::parse(&guard)?;
            let swip = interior.swip(child)?;
            let at = interior.swip_offset(child)?;
            let target = pool.page_of_swip(swip)?;
            let child_guard = pool.fetch(target)?;
            let child_frame = child_guard.frame();
            pool.note_parent(child_frame, frame, parent, at);
            (frame, at, target, child_frame)
        };
        pool.swizzle_into(frame, parent, at, Swip::swizzled(child_frame))?;
        Ok(target)
    }

    /// Visits every leaf of the tree in key order.
    ///
    /// The visitor returns `false` to stop, which is how `LIMIT` gets out of a
    /// scan without reading the rest of the tree.
    ///
    /// @param pool - the buffer pool
    /// @param visit - what to do with each leaf
    pub fn visit_leaves(
        &self,
        pool: &Pool,
        visit: &mut dyn FnMut(&LeafRef<'_>) -> DbResult<bool>,
    ) -> DbResult<()> {
        self.visit_from(pool, self.first_leaf, visit)
    }

    /// Visits leaves from a starting page rightwards.
    ///
    /// @param pool - the buffer pool
    /// @param from - the first leaf to visit
    /// @param visit - what to do with each leaf
    pub fn visit_from(
        &self,
        pool: &Pool,
        from: PageId,
        visit: &mut dyn FnMut(&LeafRef<'_>) -> DbResult<bool>,
    ) -> DbResult<()> {
        let mut page = from;
        let mut seen = 0u64;
        while !page.is_none() {
            let guard = pool.fetch(page)?;
            let leaf = LeafRef::parse(&guard)?
                .with_collations(&self.collations)
                .with_directions(&self.directions);
            // **The out-of-line values are read here, once per leaf, for every
            // walk in the crate.** A consumer that had to know about extents
            // would be five consumers that each had to; attaching them where the
            // leaf is opened means a scan branches on whether a leaf has any and
            // never on where a value lives. A leaf with none reads nothing and
            // allocates nothing.
            let held = self.read_extents(pool, &leaf)?;
            let leaf = leaf.with_extents(&held);
            let next = leaf.right_sibling();
            if !visit(&leaf)? {
                return Ok(());
            }
            drop(guard);
            page = next;
            seen = seen.saturating_add(1);
            // **Bounded by the file, not by the recorded leaf count.** The
            // guard is here to stop a cyclic chain from looping forever, and a
            // chain cannot be longer than the file has pages - which is true
            // whatever the catalog last wrote down. It used to be bounded by
            // `self.leaf_count`, a statistic written at the last checkpoint, so
            // a database closed without one refused to read a tree that had
            // simply grown since: fifty rows, two leaves, one recorded.
            if seen > pool.page_count().max(1) {
                return Err(corrupt("a leaf chain is longer than the file has pages"));
            }
        }
        Ok(())
    }

    /// Visits every leaf whose range can hold a key at or above `low`.
    ///
    /// @param pool - the buffer pool
    /// @param low - the encoded lower bound
    /// @param visit - what to do with each leaf
    pub fn visit_range(
        &self,
        pool: &Pool,
        low: &[u8],
        visit: &mut dyn FnMut(&LeafRef<'_>) -> DbResult<bool>,
    ) -> DbResult<()> {
        let start = {
            let (guard, page) = self.descend_guard(pool, low)?;
            drop(guard);
            page
        };
        self.visit_from(pool, start, visit)
    }

    /// Visits leaves right to left, starting from the one holding a key.
    ///
    /// A reverse walk cannot follow a sibling pointer, because the common
    /// header carries only a right sibling. It walks the descent path instead:
    /// back up until a child index can be decremented, then down the rightmost
    /// spine of the sibling before it. That is `O(height)` per leaf rather than
    /// `O(1)`, which is the right trade for a workload that is always bounded
    /// by a `LIMIT`; adding a left-sibling field would cost a write on every
    /// split for the benefit of an unbounded reverse scan nobody runs.
    ///
    /// @param pool - the buffer pool
    /// @param from - the encoded key to start at, or `None` for the last leaf
    /// @param visit - what to do with each leaf
    pub fn visit_reverse(
        &self,
        pool: &Pool,
        from: Option<&[u8]>,
        visit: &mut dyn FnMut(&LeafRef<'_>) -> DbResult<bool>,
    ) -> DbResult<()> {
        let mut descent = match from {
            Some(key) => self.descend(pool, key)?,
            None => self.descend_rightmost(pool)?,
        };
        let mut seen = 0u64;
        loop {
            {
                let guard = pool.fetch(descent.leaf)?;
                let leaf = LeafRef::parse(&guard)?
                    .with_collations(&self.collations)
                    .with_directions(&self.directions);
                let held = self.read_extents(pool, &leaf)?;
                let leaf = leaf.with_extents(&held);
                if !visit(&leaf)? {
                    return Ok(());
                }
            }
            seen = seen.saturating_add(1);
            if seen > self.leaf_count.saturating_add(1) {
                return Err(corrupt("a reverse walk visited more leaves than exist"));
            }
            match self.step_left(pool, &mut descent)? {
                true => {}
                false => return Ok(()),
            }
        }
    }

    /// Descends the rightmost spine.
    ///
    /// @param pool - the buffer pool
    fn descend_rightmost(&self, pool: &Pool) -> DbResult<Descent> {
        let mut steps = Vec::new();
        let mut page = self.root;
        loop {
            let child = {
                let guard = pool.fetch(page)?;
                if page::kind_of(&guard)? == PageKind::Leaf {
                    return Ok(Descent { steps, leaf: page });
                }
                InteriorRef::parse(&guard)?.count()
            };
            steps.push((page, child));
            page = self.take_child(pool, page, child)?;
            if steps.len() > 64 {
                return Err(corrupt("a rightmost descent walked more than 64 levels"));
            }
        }
    }

    /// Moves a descent to the leaf before the one it is on.
    ///
    /// Returns false when there is none, which is the left edge of the tree.
    ///
    /// @param pool - the buffer pool
    /// @param descent - the path, updated in place
    fn step_left(&self, pool: &Pool, descent: &mut Descent) -> DbResult<bool> {
        while let Some((page, index)) = descent.steps.pop() {
            if index == 0 {
                continue;
            }
            let child = index.saturating_sub(1);
            descent.steps.push((page, child));
            let mut next = self.take_child(pool, page, child)?;
            // Down the rightmost spine of that subtree.
            loop {
                let count = {
                    let guard = pool.fetch(next)?;
                    if page::kind_of(&guard)? == PageKind::Leaf {
                        descent.leaf = next;
                        break;
                    }
                    InteriorRef::parse(&guard)?.count()
                };
                descent.steps.push((next, count));
                next = self.take_child(pool, next, count)?;
                if descent.steps.len() > 64 {
                    return Err(corrupt("a left step walked more than 64 levels"));
                }
            }
            return Ok(true);
        }
        Ok(false)
    }

    /// Reports whether two rows share their leading `prefix` key columns.
    ///
    /// @param left - one row
    /// @param right - the other row
    /// @param prefix - how many leading columns to compare
    fn same_prefix(&self, left: &[Datum<'_>], right: &[Datum<'_>], prefix: usize) -> bool {
        for index in 0..prefix {
            let (Some(a), Some(b)) = (left.get(index), right.get(index)) else {
                return false;
            };
            let collation = self
                .collations
                .get(index)
                .copied()
                .unwrap_or(Collation::Binary);
            if crate::types::compare_under(a, b, collation) != std::cmp::Ordering::Equal {
                return false;
            }
        }
        true
    }

    /// Returns the key that sorts immediately after every key sharing a prefix.
    ///
    /// Appending `0xFF` works because no encoded value can begin with it: a
    /// number's class byte is `0x01`, text's is `0x02`, a blob's `0x03` and a
    /// NULL is `0x00`. So `enc(v) ++ [0xFF]` sorts above every key that starts
    /// with `enc(v)` and below `enc(w)` for every `w > v` - the two facts a
    /// skip scan's seek needs, and the reason it does not have to know what the
    /// next distinct value *is* before it looks for it.
    ///
    /// @param prefix - the key prefix to step past
    pub fn after_prefix(&self, prefix: &[Datum<'_>]) -> KeyBytes {
        match self.encode_key_small(prefix) {
            KeyBytes::Inline(mut bytes, length) if length < KeyBytes::INLINE => {
                if let Some(slot) = bytes.get_mut(length) {
                    *slot = 0xFF;
                }
                KeyBytes::Inline(bytes, length.saturating_add(1))
            }
            other => {
                let mut heap = other.as_slice().to_vec();
                heap.push(0xFF);
                KeyBytes::Heap(heap)
            }
        }
    }

    /// Visits the rows of a range, a contiguous span of one leaf at a time.
    ///
    /// The span rather than the row is what keeps a range scan vectorised: the
    /// caller builds one batch per leaf over `start..end` instead of one batch
    /// per row. A range of 200 rows inside a leaf of 400 is one batch, one
    /// selection vector and no copying of values at all.
    ///
    /// The visitor returns `false` to stop.
    ///
    /// @param pool - the buffer pool
    /// @param low - the lower bound, or `None` for the start
    /// @param low_inclusive - whether a key equal to `low` is in the range
    /// @param high - the upper bound, or `None` for the end
    /// @param high_inclusive - whether a key equal to `high` is in the range
    /// @param visit - what to do with each leaf and its live span
    pub fn visit_span(
        &self,
        pool: &Pool,
        low: Option<&[Datum<'_>]>,
        low_inclusive: bool,
        high: Option<&[Datum<'_>]>,
        high_inclusive: bool,
        visit: &mut dyn FnMut(&LeafRef<'_>, usize, usize) -> DbResult<bool>,
    ) -> DbResult<()> {
        let start_page = match low {
            Some(values) => {
                let key = self.encode_key_small(values);
                let (guard, page) = self.descend_guard(pool, key.as_slice())?;
                drop(guard);
                page
            }
            None => self.first_leaf,
        };
        self.visit_from(pool, start_page, &mut |leaf| {
            let rows = leaf.row_count();
            // The lower bound is applied to *every* leaf, not only the first.
            //
            // A descent lands on the last child whose separator is not above
            // the probe, which is the leaf that *could* hold the key - and when
            // the key is above every key in that leaf, the run starts in the
            // next one. Applying the bound only to the first leaf and then
            // treating an empty result as "past the end" made such a range
            // return nothing at all.
            //
            // It was invisible at the default 32 KiB page size, where an index
            // leaf holds enough entries that a 200-row range almost always fits
            // in the leaf the descent lands on, and it was digest-equal to
            // SQLite on every workload. The page-size sweep is what found it:
            // at 8 KiB, `point.index` returned 3,991 rows where SQLite returned
            // 4,000. Five workloads were wrong and one measurement showed it.
            //
            // The cost of applying it everywhere is one binary search per leaf,
            // and after the first matching leaf it returns 0 immediately.
            // An *exclusive* lower bound starts past the run equal to it, so
            // it is an upper bound on the same key. `WHERE id > 495` returned
            // `id >= 495` until the physical pass's tests asked it directly.
            let begin = match low {
                Some(values) if low_inclusive => lower_bound(leaf, values)?,
                Some(values) => upper_bound(leaf, values)?,
                None => 0,
            };
            // The bound is a *bound*, not a search: a probe shorter than the
            // key matches a run of rows, and `search` lands somewhere inside
            // that run rather than at either end of it. Using it here returned
            // one row of a three-row match on the first index nested loop test
            // that exercised a prefix bound, which is how this is written as an
            // explicit partition point instead.
            let end = match high {
                Some(values) => {
                    if high_inclusive {
                        upper_bound(leaf, values)?
                    } else {
                        lower_bound(leaf, values)?
                    }
                }
                None => rows,
            };
            let end = end.min(rows);
            if begin >= end {
                // **An empty span over a written leaf says nothing.** `begin`
                // and `end` are partition points of the *sorted region*, and a
                // leaf that has been written to holds live rows that are not in
                // it - so a key inserted after the leaf was packed sorts past
                // every packed key, `lower_bound` returns `rows`, and the leaf
                // is skipped with the row still in it. `WHERE score = 99` came
                // back empty after `UPDATE ... SET score = 99` while a scan of
                // the same index listed the row.
                //
                // The visitor already knows what to do: every consumer of this
                // walk re-derives its rows from `live_between` when the leaf has
                // writes. It just has to be *called*.
                if !leaf.has_writes() {
                    // Nothing in this leaf. Two different reasons, and they have
                    // opposite answers: the lower bound skipped the whole leaf,
                    // so the run is further right; or the upper bound cut it
                    // short, so there is nothing further right.
                    return Ok(begin >= rows && end >= rows);
                }
                if !visit(leaf, begin, begin)? {
                    return Ok(false);
                }
                return Ok(end >= rows);
            }
            if !visit(leaf, begin, end)? {
                return Ok(false);
            }
            // A leaf that did not reach its own end reached the upper bound.
            Ok(end == rows)
        })
    }

    /// Visits every row whose key begins with an exact prefix.
    ///
    /// The equality case of [`PagedTree::visit_span`], and it is separate
    /// because the general version pays for generality it does not need here:
    /// two binary searches per leaf, one for each bound, where the bounds are
    /// the same key. This does one, then walks forward while the key still
    /// matches. An index nested loop probes once per outer row, so the second
    /// binary search was being paid two hundred times per `join.range`
    /// execution to find a run that is usually one entry long.
    ///
    /// The forward walk is capped: past [`RUN_SCAN`] matching rows it stops
    /// guessing and bisects, so a prefix that matches a whole leaf costs a
    /// binary search rather than a linear one.
    ///
    /// @param pool - the buffer pool
    /// @param key - the exact prefix, one value per compared column
    /// @param visit - what to do with each leaf and its matching span
    pub fn visit_equal(
        &self,
        pool: &Pool,
        key: &[Datum<'_>],
        visit: &mut dyn FnMut(&LeafRef<'_>, usize, usize) -> DbResult<bool>,
    ) -> DbResult<()> {
        /// How far the run is walked before the upper bound is bisected.
        const RUN_SCAN: usize = 8;

        // The landed leaf is read through the guard the descent already holds.
        // Dropping it and calling `visit_from` re-fetched and re-parsed the
        // page a probe had just finished descending to, once per outer row.
        let encoded = self.encode_key_small(key);
        let (guard, page) = self.descend_guard(pool, encoded.as_slice())?;
        let mut next = {
            let leaf = LeafRef::parse(&guard)?
                .with_collations(&self.collations)
                .with_directions(&self.directions);
            let held = self.read_extents(pool, &leaf)?;
            let leaf = leaf.with_extents(&held);
            match Self::equal_span(&leaf, key, RUN_SCAN, visit)? {
                Some(right) => right,
                None => return Ok(()),
            }
        };
        drop(guard);
        let _ = page;
        let mut seen = 0u64;
        while !next.is_none() {
            let guard = pool.fetch(next)?;
            let leaf = LeafRef::parse(&guard)?
                .with_collations(&self.collations)
                .with_directions(&self.directions);
            let held = self.read_extents(pool, &leaf)?;
            let leaf = leaf.with_extents(&held);
            match Self::equal_span(&leaf, key, RUN_SCAN, visit)? {
                Some(right) => next = right,
                None => return Ok(()),
            }
            seen = seen.saturating_add(1);
            if seen > self.leaf_count.saturating_add(1) {
                return Err(corrupt(
                    "an equality walk visited more leaves than the tree holds",
                ));
            }
        }
        Ok(())
    }

    /// Visits one leaf's share of an equality run.
    ///
    /// Returns the next leaf to look in, or `None` when the walk is over -
    /// either because the run ended inside this leaf or because the visitor
    /// asked to stop.
    ///
    /// @param leaf - the leaf to read
    /// @param key - the exact prefix
    /// @param scan_cap - how far a run is walked before its end is bisected
    /// @param visit - what to do with the matching span
    fn equal_span(
        leaf: &LeafRef<'_>,
        key: &[Datum<'_>],
        scan_cap: usize,
        visit: &mut dyn FnMut(&LeafRef<'_>, usize, usize) -> DbResult<bool>,
    ) -> DbResult<Option<PageId>> {
        let rows = leaf.row_count();
        let (begin, end) = leaf.equal_run(key, scan_cap)?;
        if begin >= end {
            // A written leaf is visited even with an empty *sorted* run, for
            // the reason `visit_span` gives: the run is computed over the packed
            // region and the live rows are not that set. The consumer merges.
            if leaf.has_writes() && !visit(leaf, begin, begin)? {
                return Ok(None);
            }
            // Either the key sorts after everything here - so the run, if there
            // is one, starts in the next leaf - or it is simply not present and
            // everything after it is greater.
            return Ok(if begin >= rows {
                Some(leaf.right_sibling())
            } else {
                None
            });
        }
        if !visit(leaf, begin, end)? {
            return Ok(None);
        }
        // Only a run that reached the end of its leaf can continue.
        Ok(if end == rows {
            Some(leaf.right_sibling())
        } else {
            None
        })
    }

    /// Visits the rows of a range right to left, a span of one leaf at a time.
    ///
    /// The caller walks `start..end` backwards inside each span. Used by
    /// `ORDER BY ... DESC LIMIT n`, which is why it takes an upper bound and no
    /// lower one: the walk is always stopped by the limit rather than by the
    /// range.
    ///
    /// @param pool - the buffer pool
    /// @param high - the inclusive upper bound, or `None` for the last row
    /// @param visit - what to do with each leaf and its live span
    pub fn visit_span_reverse(
        &self,
        pool: &Pool,
        low: Option<&[Datum<'_>]>,
        low_inclusive: bool,
        high: Option<&[Datum<'_>]>,
        high_inclusive: bool,
        visit: &mut dyn FnMut(&LeafRef<'_>, usize, usize) -> DbResult<bool>,
    ) -> DbResult<()> {
        let from = high.map(|values| self.encode_key(values));
        let mut first = true;
        self.visit_reverse(pool, from.as_deref(), &mut |leaf| {
            let rows = leaf.row_count();
            // **Both bounds, on every leaf, exactly as the forward walk applies
            // them.** This used to take an upper bound alone, apply it to the
            // first leaf only, and walk to the start of the tree - so
            // `WHERE id >= 3 ORDER BY id DESC` returned rows 2 and 1 as well,
            // `WHERE id < 5 ORDER BY id DESC` returned 5, and
            // `WHERE grp = 2 ORDER BY k DESC` returned the other groups too.
            // Every one of those is a *wrong answer* rather than a refusal, and
            // it was reachable only through the descending-order path, which is
            // why a forward walk of the same range was right all along.
            //
            // The upper bound is a partition point of the sorted region like
            // the lower one is: inclusive means past the run equal to it,
            // exclusive means before it - which is `upper_bound` and
            // `lower_bound` respectively, the same pair the forward walk uses
            // and in the same roles.
            let end = if first {
                match high {
                    Some(values) if high_inclusive => upper_bound(leaf, values)?,
                    Some(values) => lower_bound(leaf, values)?,
                    None => rows,
                }
            } else {
                rows
            };
            first = false;
            let end = end.min(rows);
            // An exclusive lower bound starts past the run equal to it, so it
            // is an upper bound on the same key.
            let begin = match low {
                Some(values) if low_inclusive => lower_bound(leaf, values)?,
                Some(values) => upper_bound(leaf, values)?,
                None => 0,
            };
            if begin >= end {
                // A leaf that has been written to holds live rows that are not
                // in the sorted region, so an empty span over one says nothing
                // and the visitor re-derives its rows from `live_between`. The
                // walk stops when the lower bound cut the leaf short, because
                // everything further left is below it.
                if !leaf.has_writes() {
                    return Ok(begin == 0 && end == 0);
                }
                if !visit(leaf, begin, begin)? {
                    return Ok(false);
                }
                return Ok(begin == 0);
            }
            if !visit(leaf, begin, end)? {
                return Ok(false);
            }
            // A leaf that did not reach its own start reached the lower bound.
            Ok(begin == 0)
        })
    }

    /// Visits one row per distinct value of a key prefix, in order.
    ///
    /// The index skip scan. `SELECT DISTINCT category` over an index led by
    /// `category` does not need to read the rows - it needs to visit one row
    /// per distinct value. Over 100,000 rows with 64 distinct categories that
    /// is 64 seeks instead of 100,000 row reads, which is the algorithm SQLite
    /// uses for the same query.
    ///
    /// **One parse per leaf, and a seek that looks before it jumps.** Each turn
    /// of the loop parses the leaf it holds exactly once and does everything it
    /// needs from that one parse: the visit, the copy of the prefix, and the
    /// partition point that says where the run of equal prefixes ends. That
    /// partition point is what a descent has to compute *after* it arrives, so
    /// when the run ends in the leaf already open the descent never happens.
    /// When it does not, the walk steps right up to [`WALK_BUDGET`] leaves
    /// before giving up and descending - and the first time it gives up it
    /// switches itself off, because a tree's runs are all about the same length.
    ///
    /// Every route answers the same question - the first row whose key is above
    /// the prefix just visited - so the choice between them is a choice of cost,
    /// never of answer. `scan.distinct` is 65 seeks whatever the table's size,
    /// which is why the workload gets *better* with scale and why the per-seek
    /// constant is the whole of it.
    ///
    /// @param pool - the buffer pool
    /// @param prefix - how many leading key columns form the distinct value
    /// @param visit - what to do with each representative row
    pub fn skip_scan(
        &self,
        pool: &Pool,
        prefix: usize,
        visit: &mut dyn FnMut(&[Datum<'_>]) -> DbResult<bool>,
    ) -> DbResult<()> {
        if prefix == 0 || prefix > self.key_columns {
            return Err(misuse(format!(
                "a skip scan over {prefix} of {} key columns",
                self.key_columns
            )));
        }
        let mut page = self.first_leaf;
        let mut row = 0usize;
        // The leaf the position refers to, still pinned when the last step left
        // it open.
        let mut carried: Option<PageGuard<'_>> = None;
        // Reused across seeks, so a scan of 64 distinct values makes one
        // allocation rather than 64.
        let mut held: Vec<OwnedDatum> = Vec::with_capacity(prefix);
        let mut visited = 0u64;
        // Whether to look for the end of a run by walking before descending for
        // it. Switched off the first time a walk runs out of budget.
        let mut walking = true;
        // The last prefix handed to the visitor, kept only while a leaf that
        // had to be merged might have already emitted the value the next clean
        // leaf starts with. A tree nobody has written to never sets it, so the
        // resynchronising `upper_bound` below is not on the clean path at all.
        let mut resync: Option<Vec<OwnedDatum>> = None;

        loop {
            if page.is_none() {
                return Ok(());
            }
            let guard = match carried.take() {
                Some(open) => open,
                None => pool.fetch(page)?,
            };
            let step = 'step: {
                let leaf = LeafRef::parse(&guard)?
                    .with_collations(&self.collations)
                    .with_directions(&self.directions);
                let resolved = self.read_extents(pool, &leaf)?;
                let leaf = leaf.with_extents(&resolved);
                if leaf.has_writes() {
                    // A leaf that has been written to is walked rather than
                    // seeked over: its distinct values can live in the delta
                    // area, where there is no partition point to jump to. The
                    // seek machinery below is for the clean leaves, which is
                    // every leaf until something writes to one and every leaf
                    // again after the next compaction.
                    let merged = leaf.live()?;
                    let mut last: Option<Vec<Datum<'_>>> = resync
                        .as_ref()
                        .map(|held| held.iter().map(OwnedDatum::borrow).collect());
                    for values in merged.iter().skip(row) {
                        let head = values.get(..prefix).unwrap_or(&[]);
                        let repeated = last
                            .as_ref()
                            .is_some_and(|previous| self.same_prefix(previous, head, prefix));
                        if repeated {
                            continue;
                        }
                        if !visit(head)? {
                            return Ok(());
                        }
                        visited = visited.saturating_add(1);
                        last = Some(head.to_vec());
                    }
                    resync = last.map(|held| held.iter().map(OwnedDatum::from_datum).collect());
                    Step::Right(leaf.right_sibling())
                } else if row >= leaf.row_count() {
                    // An empty leaf, or a position past the end of this one. A
                    // bulk-built tree has neither, but one that has been deleted
                    // from can, and a scan that stopped here would silently
                    // return short.
                    Step::Right(leaf.right_sibling())
                } else {
                    // A merged leaf just before this one may have emitted the
                    // value this one starts with, so the position is advanced
                    // past it. `resync` is `None` on a tree nobody has written
                    // to, which is what keeps this off the measured path.
                    let mut row = row;
                    let mut past_the_end = false;
                    if let Some(previous) = &resync {
                        let borrowed: Vec<Datum<'_>> =
                            previous.iter().map(OwnedDatum::borrow).collect();
                        row = row.max(leaf.upper_bound(&borrowed)?);
                        resync = None;
                        past_the_end = row >= leaf.row_count();
                    }
                    if past_the_end {
                        break 'step Step::Right(leaf.right_sibling());
                    }
                    held.clear();
                    for column in 0..prefix {
                        held.push(OwnedDatum::from_datum(&leaf.value(row, column)?));
                    }
                    {
                        let borrowed: Vec<Datum<'_>> =
                            held.iter().map(OwnedDatum::borrow).collect();
                        if !visit(&borrowed)? {
                            return Ok(());
                        }
                    }
                    visited = visited.saturating_add(1);
                    if visited > self.row_count.saturating_add(1) {
                        return Err(corrupt("a skip scan visited more rows than the tree holds"));
                    }
                    // The borrows of the prefix live on the stack when the
                    // prefix is narrow, which every index in the fixture and in
                    // the dialect's own corpus is. Collecting them was one
                    // allocation per distinct value on a path whose whole
                    // argument is that it makes as few reads as there are
                    // distinct values.
                    let mut inline: [Datum<'_>; SKIP_PREFIX_INLINE] =
                        [Datum::Null; SKIP_PREFIX_INLINE];
                    let spilled: Vec<Datum<'_>>;
                    let borrowed: &[Datum<'_>] = if prefix <= SKIP_PREFIX_INLINE {
                        for (index, value) in held.iter().enumerate().take(prefix) {
                            if let Some(slot) = inline.get_mut(index) {
                                *slot = value.borrow();
                            }
                        }
                        inline.get(..prefix).unwrap_or(&[])
                    } else {
                        spilled = held.iter().map(OwnedDatum::borrow).collect();
                        spilled.as_slice()
                    };
                    let low = leaf.upper_bound(borrowed)?;
                    if low < leaf.row_count() {
                        Step::Here(low)
                    } else if leaf.right_sibling().is_none() {
                        // The run reaches the end of the tree, so there is no
                        // key above the prefix and the scan is over.
                        return Ok(());
                    } else {
                        Step::Seek(leaf.right_sibling())
                    }
                }
            };
            match step {
                Step::Right(right) => {
                    page = right;
                    row = 0;
                }
                Step::Here(low) => {
                    row = low;
                    carried = Some(guard);
                }
                Step::Seek(right) => {
                    drop(guard);
                    // The borrows of the prefix live on the stack when the
                    // prefix is narrow, which every index in the fixture and in
                    // the dialect's own corpus is. Collecting them was one
                    // allocation per distinct value on a path whose whole
                    // argument is that it makes as few reads as there are
                    // distinct values.
                    let mut inline: [Datum<'_>; SKIP_PREFIX_INLINE] =
                        [Datum::Null; SKIP_PREFIX_INLINE];
                    let spilled: Vec<Datum<'_>>;
                    let borrowed: &[Datum<'_>] = if prefix <= SKIP_PREFIX_INLINE {
                        for (index, value) in held.iter().enumerate().take(prefix) {
                            if let Some(slot) = inline.get_mut(index) {
                                *slot = value.borrow();
                            }
                        }
                        inline.get(..prefix).unwrap_or(&[])
                    } else {
                        spilled = held.iter().map(OwnedDatum::borrow).collect();
                        spilled.as_slice()
                    };
                    let mut found = None;
                    if walking {
                        let mut here = right;
                        let mut examined = 0usize;
                        while !here.is_none() {
                            let stepped = pool.fetch(here)?;
                            let (low, rows, next) = {
                                let leaf = LeafRef::parse(&stepped)?
                                    .with_collations(&self.collations)
                                    .with_directions(&self.directions);
                                (
                                    leaf.upper_bound(borrowed)?,
                                    leaf.row_count(),
                                    leaf.right_sibling(),
                                )
                            };
                            if low < rows {
                                found = Some((here, low, stepped));
                                break;
                            }
                            if next.is_none() {
                                return Ok(());
                            }
                            examined = examined.saturating_add(1);
                            if examined >= WALK_BUDGET {
                                break;
                            }
                            here = next;
                        }
                        if found.is_none() {
                            // The run is longer than the walk will follow, so
                            // this tree's runs span more leaves than a step is
                            // worth and every later seek descends. One failed
                            // walk per scan is what the measurement costs.
                            walking = false;
                        }
                    }
                    match found {
                        Some((landed, low, stepped)) => {
                            page = landed;
                            row = low;
                            carried = Some(stepped);
                        }
                        None => {
                            let seek = self.after_prefix(borrowed);
                            let (landed_guard, landed) =
                                self.descend_guard(pool, seek.as_slice())?;
                            let next = {
                                let leaf = LeafRef::parse(&landed_guard)?
                                    .with_collations(&self.collations)
                                    .with_directions(&self.directions);
                                let low = leaf.upper_bound(borrowed)?;
                                if low < leaf.row_count() {
                                    Some((landed, low, true))
                                } else {
                                    let right = leaf.right_sibling();
                                    if right.is_none() {
                                        None
                                    } else {
                                        Some((right, 0, false))
                                    }
                                }
                            };
                            match next {
                                Some((next_page, next_row, open)) => {
                                    page = next_page;
                                    row = next_row;
                                    carried = if open { Some(landed_guard) } else { None };
                                }
                                None => return Ok(()),
                            }
                        }
                    }
                }
            }
        }
    }

    /// Runs a function over the row a key names, if the tree holds it.
    ///
    /// The row is handed to the caller *inside* the leaf's guard, so a point
    /// probe projects straight out of the page and copies only what the query
    /// asked for. Returning the row instead would mean copying every column of
    /// it first, which on `point.rowid` is the difference between the target
    /// and twice the target.
    ///
    /// @param pool - the buffer pool
    /// @param probe - the key, one value per key column
    /// @param read - what to do with the leaf and the row index
    pub fn probe<R>(
        &self,
        pool: &Pool,
        probe: &[Datum<'_>],
        read: impl FnOnce(&LeafRef<'_>, Hit) -> DbResult<R>,
    ) -> DbResult<Option<R>> {
        let key = self.encode_key_small(probe);
        let (guard, _) = self.descend_guard(pool, key.as_slice())?;
        let leaf = LeafRef::parse(&guard)?
            .with_collations(&self.collations)
            .with_directions(&self.directions);
        // **One row's out-of-line values, not the leaf's.** A leaf whose values
        // are out of line holds a great many rows - it is sixteen bytes per
        // value rather than four kilobytes - so resolving the whole leaf to
        // answer one probe read three hundred extents to return one. It measured
        // 31 us per point read against SQLite's 13.
        //
        // The row is found first, which is safe because finding it reads only
        // key columns and a key is never out of line.
        if !leaf.has_extents() {
            return self.probe_leaf(&leaf, probe, read);
        }
        let Some(hit) = self.hit_in(&leaf, probe)? else {
            return Ok(None);
        };
        let held = match hit {
            Hit::Sorted(row) => self.read_extents_row(pool, &leaf, row)?,
            // A delta row can hold one too, as a tagged reference: the write
            // path spills the value and puts the reference in the delta rather
            // than repacking the leaf around it.
            Hit::Delta(index) => self.read_extents_delta(pool, &leaf, index)?,
        };
        let leaf = leaf.with_extents(&held);
        Ok(Some(read(&leaf, hit)?))
    }

    /// Finds a key inside a leaf the caller has already descended to.
    ///
    /// The half of [`PagedTree::probe`] below the descent, factored out so the
    /// pipelined form cannot drift from the single form - including the delta
    /// check, which is the part that would be quietly dropped.
    ///
    /// @param leaf - the leaf the descent landed on
    /// @param probe - the key, one value per key column
    /// @param read - what to do with the leaf and the row index
    fn probe_leaf<R>(
        &self,
        leaf: &LeafRef<'_>,
        probe: &[Datum<'_>],
        read: impl FnOnce(&LeafRef<'_>, Hit) -> DbResult<R>,
    ) -> DbResult<Option<R>> {
        if let Ok(row) = leaf.search(probe)? {
            if !leaf.is_tombstoned(row)? {
                return Ok(Some(read(leaf, Hit::Sorted(row))?));
            }
        }
        // The delta area, which Phase 2 refused and Phase 3 reads. The loop is
        // guarded by `delta_count`, which is zero on every leaf that has not
        // been written to - so a clean tree pays one comparison against zero
        // per probe and nothing else, which is what keeps `point.rowid` at the
        // 300 ns the read gate measured.
        // **As many columns as the probe supplied, not as many as the key has.**
        // A probe may be a *prefix*: an equality on the leading column of a
        // two-column index is one value against a key of `(score, rowid)`, and
        // the sorted search above compares exactly the columns it was given.
        // Comparing `key_columns` here instead filled the missing positions with
        // NULL and compared the delta row's rowid against it - so a row that had
        // been written since the leaf was packed was never found, and
        // `WHERE score = 99` came back empty after `UPDATE ... SET score = 99`
        // while a scan of the same index showed the row.
        let compared = probe.len().min(self.key_columns);
        for entry in 0..leaf.delta_count() {
            let mut matches = true;
            for column in 0..compared {
                let held = leaf.delta_value(entry, column)?;
                let wanted = probe.get(column).copied().unwrap_or(Datum::Null);
                if crate::types::compare_under(&held, &wanted, leaf.collation_of(column))
                    != std::cmp::Ordering::Equal
                {
                    matches = false;
                    break;
                }
            }
            if matches {
                return Ok(Some(read(leaf, Hit::Delta(entry))?));
            }
        }
        Ok(None)
    }

    /// Returns one row by key, copying it out.
    ///
    /// The allocating path, for tests and for the model comparison. The
    /// executor uses [`PagedTree::probe`].
    ///
    /// @param pool - the buffer pool
    /// @param probe - the key, one value per key column
    /// Reports whether a key is in the tree, without copying its row.
    ///
    /// The uniqueness check a write does before it writes, which asks only
    /// whether something is there. `point` answers the same question and copies
    /// every column of the row to do it, allocating per text and per blob - on
    /// `main_table` that is five columns, a text and a blob, per insert, thrown
    /// away.
    ///
    /// @param pool - the buffer pool
    /// @param probe - the key, one value per key column
    pub fn contains(&self, pool: &Pool, probe: &[Datum<'_>]) -> DbResult<bool> {
        Ok(self.probe(pool, probe, |_, _| Ok(()))?.is_some())
    }

    /// Returns one row by key, copying it out.
    ///
    /// The allocating path, for tests and for the model comparison. The
    /// executor uses [`PagedTree::probe`], and a caller that only wants to know
    /// whether the key is there uses [`PagedTree::contains`].
    ///
    /// @param pool - the buffer pool
    /// @param probe - the key, one value per key column
    pub fn point(&self, pool: &Pool, probe: &[Datum<'_>]) -> DbResult<Option<Vec<OwnedDatum>>> {
        self.probe(pool, probe, |leaf, hit| {
            let mut values = Vec::with_capacity(leaf.column_count());
            for column in 0..leaf.column_count() {
                values.push(OwnedDatum::from_datum(&leaf.value_at(hit, column)?));
            }
            Ok(values)
        })
    }

    /// Returns every row in key order, copying them out.
    ///
    /// The slow path the property test and the integrity checker use.
    ///
    /// @param pool - the buffer pool
    pub fn rows(&self, pool: &Pool) -> DbResult<Vec<Vec<OwnedDatum>>> {
        let mut out = Vec::new();
        self.visit_leaves(pool, &mut |leaf| {
            for row in leaf.live()? {
                out.push(row.iter().map(OwnedDatum::from_datum).collect());
            }
            Ok(true)
        })?;
        Ok(out)
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
        let compared = probe.len().min(self.key_columns);
        for entry in 0..leaf.delta_count() {
            let mut matches = true;
            for column in 0..compared {
                let held = leaf.delta_value(entry, column)?;
                let wanted = probe.get(column).copied().unwrap_or(Datum::Null);
                if crate::types::compare_under(&held, &wanted, leaf.collation_of(column))
                    != std::cmp::Ordering::Equal
                {
                    matches = false;
                    break;
                }
            }
            if matches {
                return Ok(Some(Hit::Delta(entry)));
            }
        }
        Ok(None)
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

    /// Returns every page the tree occupies, interior pages and leaves.
    ///
    /// For `DROP`, which gives them back to the free map. It walks the interior
    /// levels rather than following the sibling chain, because the chain only
    /// reaches the leaves and a dropped tree that left its interior pages behind
    /// would leak a page per fanout for the life of the file.
    ///
    /// The walk is level-order from the root, and a page that appears twice -
    /// which a corrupt file could produce - is returned once, because handing
    /// the same page to the free map twice is worse than leaking it.
    ///
    /// @param pool - the buffer pool the file is open through
    pub fn pages(&self, pool: &Pool) -> DbResult<Vec<PageId>> {
        let mut seen: Vec<PageId> = Vec::new();
        let mut frontier: Vec<PageId> = vec![self.root];
        while let Some(page) = frontier.pop() {
            if page.is_none() || seen.contains(&page) {
                continue;
            }
            seen.push(page);
            let image = {
                let guard = pool.fetch(page)?;
                guard.bytes().to_vec()
            };
            if page::kind_of(&image)? == PageKind::Leaf {
                continue;
            }
            let interior = InteriorRef::parse(&image)?;
            for child in 0..interior.children() {
                let swip = interior.swip(child)?;
                frontier.push(pool.page_of_swip(swip)?);
            }
        }
        Ok(seen)
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
