//! The PAX leaf: rows sorted by key, stored column within page.
//!
//! Invariant: `LeafRef::parse` validates every offset in the header against the
//! page's own length *before* any accessor can be called, so an accessor is a
//! slice of an already-checked range rather than a fresh chance to read out of
//! bounds. A page that fails validation produces a `DbError`; there is no way
//! to obtain a `LeafRef` over a page that did not pass.
//!
//! ## The layout
//!
//! ```text
//! 0                        common header (32 bytes, see page.rs)
//! 32   u16  row_count      rows in the sorted region
//! 34   u16  delta_count    rows in the unsorted delta area
//! 36   u16  column_count
//! 38   u16  key_columns    the first N columns are the key, in order
//! 40   u32  heap_start     the heap runs [heap_start, page_size)
//! 44   u32  delta_start    the delta area starts here
//! 48   u64  max_cts        commit timestamp of the last modification
//! 56   i64  low_fence      smallest key hint; rowid trees only
//! 64        column directory, column_count entries of 8 bytes
//! ..        mini-columns, in column order, each 8-byte aligned
//! ..        tombstone bitmap, if the has_tombstones flag is set
//! ..        delta area
//! ..        free space
//! ..        heap, growing down from the page end
//! ```
//!
//! A mini-column is a **class array** of two bits per row (0 NULL, 1 typed
//! value present, 2 exception, 3 reserved) padded to eight bytes, followed by
//! `row_count` slots of the type's slot width. For `Int64` and `Float64` the
//! slot is the value; for `Text` and `Blob` it is a `(u32 offset, u32 length)`
//! into the page; for `Any` it is a `u32` offset of a tagged value.
//!
//! ## Why the class array is separate from the values
//!
//! The whole speed argument rests on one property: a scan that needs `sum(key)`
//! wants a contiguous run of 8-byte integers and nothing else in the cache
//! lines it touches. Putting the NULL bit inline with the value would halve the
//! useful density of those lines. Putting it in a separate 2-bit array means
//! the common case - no NULLs, no exceptions - is one pass over 1/32nd as many
//! bytes to prove it, and then a run of pure values. That pass is why
//! [`MiniColumn::all_typed`] exists and why the leaf carries a
//! `has_exceptions` flag: the cost of dynamic typing is paid once per leaf, not
//! once per row.

use inillucent_base::error::{corrupt, misuse};
use inillucent_base::DbResult;

use inillucent_pool::extent::{ExtentRef, EXTENT_REF_BYTES};

use crate::datum::Datum;
use crate::page::{self, header, PageId, PageKind};
use crate::types::{
    heap_slot_width, int_slot_width, read_heap_slot, read_int_slot, read_slot_offset,
    write_heap_slot, write_int_slot, write_slot_offset, ColumnSpec, PhysicalType, ValueClass,
    COLUMN_ALL_TYPED, COLUMN_KEY,
};

mod delta;

/// Byte offsets inside the leaf header, after the common header.
pub mod leaf_header {
    /// Rows in the sorted region, 2 bytes.
    pub const ROW_COUNT: usize = 32;
    /// Rows in the delta area, 2 bytes.
    pub const DELTA_COUNT: usize = 34;
    /// Columns in the directory, 2 bytes.
    pub const COLUMN_COUNT: usize = 36;
    /// How many leading columns form the key, 2 bytes.
    pub const KEY_COLUMNS: usize = 38;
    /// Where the heap starts, 4 bytes.
    pub const HEAP_START: usize = 40;
    /// Where the delta area starts, 4 bytes.
    pub const DELTA_START: usize = 44;
    /// The commit timestamp of the last modification, 8 bytes.
    pub const MAX_CTS: usize = 48;
    /// The smallest key hint, 8 bytes.
    pub const LOW_FENCE: usize = 56;
    /// Where the column directory starts.
    pub const DIRECTORY: usize = 64;
}

/// Bit 0 of the common flags byte: some class array holds an exception.
pub const LEAF_HAS_EXCEPTIONS: u8 = 0b0000_0001;
/// Bit 1: the tombstone bitmap is present.
pub const LEAF_HAS_TOMBSTONES: u8 = 0b0000_0010;
/// Bit 2: the delta area holds rows.
pub const LEAF_HAS_DELTA: u8 = 0b0000_0100;
/// Bit 3: some class array holds a value stored out of line in a blob extent.
pub const LEAF_HAS_EXTENTS: u8 = 0b0000_1000;

/// Bit 4: the column directory's entries are sixteen bytes, not eight.
///
/// **The eight extra bytes are a per-column base**, which is what lets an
/// integer column spend one byte a row on values that span a hundred thousand:
/// a leaf of an index holds a contiguous *run* of its key, so the range inside
/// one page is small even when the column's range is not, and a slot holds the
/// distance from the base rather than the value.
///
/// It is a flag rather than a version because it is a property of the page. A
/// leaf written before this existed has the bit clear, its entries are eight
/// bytes and its bases are zero, and it reads exactly as it always did.
pub const LEAF_WIDE_DIRECTORY: u8 = 0b0001_0000;

/// The divisor that decides when a value is stored out of line.
///
/// The TDD's rule, quoted: "A value longer than `page_size / 8` (4 KiB at the
/// default page size) is stored out of line." An eighth is the trade between two
/// costs that pull opposite ways: a large value kept inline empties the leaf, so
/// a scan reads a page per row; a small value pushed out of line costs a second
/// read to get at bytes that would have been free.
pub const EXTENT_DIVISOR: usize = 8;

/// The most rows the delta area may hold before a compaction is forced.
///
/// Thirty-two is the TDD's number and is measured in Phase 3 against 16 and 64.
/// The reasoning: a merge on read costs at most this many comparisons, and a
/// compaction's memmove is amortised across this many inserts.
pub const DELTA_LIMIT: usize = 32;

/// The size of one column directory entry, without a base.
const DIRECTORY_ENTRY: usize = 8;

/// The size of one column directory entry that carries a base.
const DIRECTORY_ENTRY_WIDE: usize = 16;

/// Whether the builder gives integer columns a frame of reference.
///
/// **A width is chosen from a column's *range*, not its magnitude, once there is
/// a base to measure from.** `main_key` is `(key, rowid)` and its leaves hold
/// contiguous runs of `key`, so a leaf spans about three and a half thousand of
/// the hundred thousand distinct values in it: four bytes without a base and two
/// with one. `main_table`'s `id` spans about two hundred and eighty inside a
/// leaf and needs one.
///
/// Requires [`NARROW_INT_SLOTS`]: the base exists to make the width smaller, and
/// with a fixed eight-byte slot there is no width to make smaller.
pub const FRAME_OF_REFERENCE: bool = true;

/// Whether the builder narrows an integer mini-column's slots.
///
/// **An `Int64` slot used to be eight bytes whatever the value was**, so
/// `main_table`'s three integer columns cost twenty-four bytes a row where
/// SQLite's record varints cost about five - and the `.rdb` was 1.41x the
/// `.db` because of it, which the page cache then paid for a second time. The
/// builder now picks the narrowest of 1, 2, 4 and 8 that holds every typed
/// value in the leaf, and writes it in the `slot_width` `u16` the column
/// directory has always carried and no reader ever read.
///
/// **The readers honour the directory unconditionally**, whatever this says.
/// That is what makes the two arms comparable and what makes the change
/// backward compatible in both directions: a file written by a narrow build is
/// read correctly by a wide one and the other way round, because a wide build's
/// pages simply say eight. Flipping this constant and rebuilding is the whole
/// of the A/B, and it is left here so the measurement can be repeated.
///
/// ## What it was measured at (four 30-round gate runs each way)
///
/// | | wide | narrow |
/// |---|---|---|
/// | the imported medium `.rdb` | 22.66 MiB | **17.90 MiB** (1.41x the `.db` -> **1.12x**) |
/// | peak resident set | 51.33 MiB | **46.61 MiB** |
/// | `read.point` | 27.06x | **30.49x** |
/// | `read.join` | 3.98x | **4.44x** |
/// | `read.analytical` | 6.08x | **6.68x** |
/// | `write` | **2.03x** | 1.52x |
/// | weighted headline | **3.85x** | 3.71x |
///
/// **The reads got faster, not slower**, which is the thing the ticket was
/// written to find out: the same values through fewer cache lines. The price is
/// the write families - a compaction is one pass over every live row of a leaf,
/// and an index leaf now holds twice as many - and `DELTA_LIMIT = 64` was
/// measured on both arms and **does not buy it back**.
pub const NARROW_INT_SLOTS: bool = true;

/// The arm a caller cannot reach, kept because removing it would be a lie.
///
/// The coverage gate asks for 100% branch coverage on this codec with the
/// unreachable branches documented, and the way to document one is to make it
/// say so in the code rather than in a spreadsheet. Every call site names why
/// it cannot happen; a test build panics if one ever does, so "unreachable"
/// stays a claim the test suite checks rather than a comment that rots.
///
/// @param why - what the caller has already established
fn unreachable_branch(why: &str) -> inillucent_base::DbError {
    debug_assert!(false, "reached a branch documented as unreachable: {why}");
    corrupt(format!("unreachable: {why}"))
}

/// How many key columns a [`KeyView`] holds without spilling.
///
/// Four covers every key in the scorecard's schema and every index this engine
/// builds by default; a wider key still works and is only slower, because the
/// view falls back to the general accessor past this point.
const KEY_VIEW_INLINE: usize = 4;

/// A leaf's key columns, derived once and reused across a binary search.
#[derive(Clone, Copy, Debug)]
pub struct KeyView<'p> {
    columns: [Option<MiniColumn<'p>>; KEY_VIEW_INLINE],
    width: usize,
}

/// A validated read view over one leaf page.
///
/// Holds no allocation: everything is derived from the page bytes on demand,
/// because a scan visits hundreds of leaves and asks each for two or three of
/// its columns.
/// Where to place a bound search's next probe, by proportion.
///
/// It reads the leading key column's raw bytes, which for an all-typed `Int64`
/// mini-column is a contiguous run of eight-byte little-endian values.
struct IntegerGuide<'p> {
    /// The leading key column's value array.
    values: &'p [u8],
    /// How many bytes one of its slots occupies.
    width: usize,
    /// What those slots are measured from.
    base: i64,
    /// The value being looked for.
    target: i64,
}

impl IntegerGuide<'_> {
    /// Returns a row in `low..high` to probe next.
    ///
    /// Always inside the window, so the loop that calls it terminates whatever
    /// the data looks like: a degenerate guess is a slow search, never a wrong
    /// one or a hanging one.
    ///
    /// @param low - the first row still in the window
    /// @param high - one past the last row still in the window
    fn between(&self, low: usize, high: usize) -> DbResult<usize> {
        let last = high.saturating_sub(1);
        let low_value = self.read(low)?;
        let high_value = self.read(last)?;
        if high_value <= low_value {
            return Ok(low.saturating_add(high.saturating_sub(low) / 2));
        }
        if self.target <= low_value {
            return Ok(low);
        }
        if self.target >= high_value {
            return Ok(last);
        }
        let span = i128::from(high_value).saturating_sub(i128::from(low_value));
        let into = i128::from(self.target).saturating_sub(i128::from(low_value));
        let width = last.saturating_sub(low) as i128;
        let offset = if span == 0 { 0 } else { into * width / span };
        Ok(low.saturating_add(offset.max(0).min(width) as usize))
    }

    /// Reads one row's value from the leading key column.
    ///
    /// @param row - the row to read
    fn read(&self, row: usize) -> DbResult<i64> {
        let at = row.saturating_mul(self.width);
        let slice = self
            .values
            .get(at..at.saturating_add(self.width))
            .ok_or_else(|| corrupt(format!("row {row} is past the key column")))?;
        Ok(from_frame(self.base, self.width, slice))
    }
}

/// A parsed leaf, borrowing the page it describes.
///
/// Holds no allocation: everything is derived from the page bytes on demand,
/// because a scan visits hundreds of leaves and asks each for two or three of
/// its columns.
#[derive(Clone, Copy, Debug)]
pub struct LeafRef<'p> {
    /// How many bytes one column directory entry occupies: eight, or sixteen
    /// when the page carries a base per column. See [`LEAF_WIDE_DIRECTORY`].
    entry_size: usize,
    /// The collation of each key column, supplied by whoever built the tree.
    ///
    /// Empty means BINARY throughout, which is what a bare [`LeafRef::parse`]
    /// gives - the page does not carry collations and must not, because the
    /// catalog is what says a column has one. The tree hands them in, and a
    /// comparison that used the page's answer instead would silently disagree
    /// with the order the tree is stored in.
    collations: &'p [inillucent_value::collation::Collation],
    /// The direction of each key column, in key order.
    ///
    /// Empty means every column ascending, which is every tree but an index
    /// declared `DESC`. It travels with the column directory for the same
    /// reason the collations do: the page does not carry it and must not.
    directions: &'p [bool],
    page: &'p [u8],
    row_count: usize,
    delta_count: usize,
    column_count: usize,
    key_columns: usize,
    heap_start: usize,
    delta_start: usize,
    flags: u8,
    /// The out-of-line values, when a caller has read them.
    extents: Option<&'p Extents>,
}

impl<'p> LeafRef<'p> {
    /// Validates a page and returns a view over it.
    ///
    /// Every header field is checked against the page's own length, and every
    /// mini-column's extent is checked to lie inside the region between the
    /// directory and the delta area. A page that passes cannot make an accessor
    /// read out of bounds.
    ///
    /// @param pageent - the page bytes, exactly one page long
    pub fn parse(pageent: &'p [u8]) -> DbResult<LeafRef<'p>> {
        if pageent.len() < leaf_header::DIRECTORY {
            return Err(corrupt("page is shorter than a leaf header"));
        }
        if page::kind_of(pageent)? != PageKind::Leaf {
            return Err(corrupt("page is not a leaf"));
        }
        let flags = page::flags_of(pageent)?;
        let row_count = page::read_u16(pageent, leaf_header::ROW_COUNT)? as usize;
        let delta_count = page::read_u16(pageent, leaf_header::DELTA_COUNT)? as usize;
        let column_count = page::read_u16(pageent, leaf_header::COLUMN_COUNT)? as usize;
        let key_columns = page::read_u16(pageent, leaf_header::KEY_COLUMNS)? as usize;
        let heap_start = page::read_u32(pageent, leaf_header::HEAP_START)? as usize;
        let delta_start = page::read_u32(pageent, leaf_header::DELTA_START)? as usize;

        if column_count == 0 {
            return Err(corrupt("a leaf with no columns cannot hold a row"));
        }
        if key_columns == 0 || key_columns > column_count {
            return Err(corrupt(format!(
                "key_columns {key_columns} is not within 1..={column_count}"
            )));
        }
        if delta_count > DELTA_LIMIT {
            return Err(corrupt(format!(
                "delta_count {delta_count} exceeds the limit of {DELTA_LIMIT}"
            )));
        }
        if (delta_count > 0) != (flags & LEAF_HAS_DELTA != 0) {
            return Err(corrupt("the delta flag disagrees with delta_count"));
        }
        let entry_size = if flags & LEAF_WIDE_DIRECTORY != 0 {
            DIRECTORY_ENTRY_WIDE
        } else {
            DIRECTORY_ENTRY
        };
        let directory_end = leaf_header::DIRECTORY
            .checked_add(column_count.saturating_mul(entry_size))
            .ok_or_else(|| corrupt("the column directory overflows"))?;
        if directory_end > pageent.len() {
            return Err(corrupt("the column directory runs past the page"));
        }
        if heap_start > pageent.len() {
            return Err(corrupt("heap_start runs past the page"));
        }
        if delta_start > heap_start || delta_start < directory_end {
            return Err(corrupt(
                "delta_start must lie between the directory and the heap",
            ));
        }

        let leaf = LeafRef {
            collations: &[],
            directions: &[],
            page: pageent,
            entry_size,
            row_count,
            delta_count,
            column_count,
            key_columns,
            heap_start,
            delta_start,
            flags,
            extents: None,
        };

        // What `parse` checks is the header: the counts are self-consistent,
        // the directory fits, and the heap and delta offsets are inside the
        // page and in the right order. What it does NOT check is where each
        // mini-column lands, and that is deliberate.
        //
        // Every accessor slices with `get`, so a mini-column whose declared
        // offset runs past the page returns an error rather than reading out of
        // bounds - safety does not depend on this check. What the check buys is
        // *detection*: a page whose columns overlap its heap is corrupt even
        // though every read of it is in bounds. That is an integrity question,
        // and it belongs to `LeafRef::integrity`, the checksums and the
        // corrupt-page campaigns rather than to every reader.
        //
        // It is here rather than there because it was measured: a descent
        // parses a leaf per level, and a skip scan descends once per distinct
        // value. Walking the directory on every parse was most of the cost of
        // a query whose answer is sixty-four rows.
        // Walking the delta once here means every later delta accessor is
        // reading bytes that have already been proved to decode.
        leaf.validate_delta()?;
        Ok(leaf)
    }

    /// Checks the leaf's row-level invariants.
    ///
    /// The O(rows) half of validation, kept out of [`LeafRef::parse`] so that
    /// reading a leaf costs time proportional to its columns. The integrity
    /// checker runs this over every page; a reader does not.
    ///
    /// 1. The `has_exceptions` flag agrees with the class arrays.
    /// 2. Sorted-region keys strictly increase.
    /// 3. No delta key equals a live sorted-region key.
    pub fn integrity(&self) -> DbResult<()> {
        // The mini-column extents: inside the page, inside the region between
        // the directory and the tombstone bitmap, and 8-byte aligned so the
        // value array a vectorised scan reads is aligned.
        let directory_end = leaf_header::DIRECTORY
            .checked_add(self.column_count.saturating_mul(self.entry_size))
            .ok_or_else(|| corrupt("the column directory overflows"))?;
        let columns_end = self.tombstones_start()?;
        for index in 0..self.column_count {
            let column = self.column(index)?;
            let start = self.column_offset(index)?;
            let end = start
                .checked_add(column.class.len())
                .and_then(|at| at.checked_add(column.values.len()))
                .ok_or_else(|| corrupt("a mini-column extent overflows"))?;
            if start < directory_end || end > columns_end {
                return Err(corrupt(format!(
                    "mini-column {index} lies outside [{directory_end}, {columns_end})"
                )));
            }
            if start % 8 != 0 {
                return Err(corrupt(format!(
                    "mini-column {index} is not 8-byte aligned"
                )));
            }
        }
        let mut seen_exception = false;
        let mut seen_extent = false;
        for index in 0..self.column_count {
            let column = self.column(index)?;
            seen_exception = seen_exception || column.any_exception()?;
            seen_extent = seen_extent || column.any_extent()?;
        }
        if seen_exception != self.has_exceptions() {
            return Err(corrupt(
                "the exception flag disagrees with the class arrays",
            ));
        }
        // **The extent flag is what a reader believes.** A leaf whose flag is
        // clear is read as mini-columns, so a leaf holding an out-of-line value
        // without saying so would have its sixteen-byte reference read as
        // though it were the value.
        //
        // Either region can hold one, so the flag is the union: a leaf that has
        // taken a wide row since its last compaction has the reference in the
        // delta area and nothing in a class array says so.
        let seen_extent = seen_extent || self.any_delta_extent_unchecked()?;
        if seen_extent != self.has_extents() {
            return Err(corrupt(
                "the extent flag disagrees with what the leaf holds",
            ));
        }
        for row in 0..self.row_count {
            for column in 0..self.column_count {
                if self.column(column)?.class_at(row)? != ValueClass::Extent {
                    continue;
                }
                // The reference itself has to decode: a page number of zero or
                // sixteen bytes that run off the page are corruption a reader
                // would otherwise meet as a failed fetch much later.
                self.extent_at(row, column)?;
            }
        }
        for row in 1..self.row_count {
            let mut previous = Vec::with_capacity(self.key_columns);
            for column in 0..self.key_columns {
                previous.push(self.value(row.saturating_sub(1), column)?);
            }
            if self.compare_key(row, &previous)? != std::cmp::Ordering::Greater {
                return Err(corrupt(format!(
                    "row {row} does not sort after the row before it"
                )));
            }
        }
        for entry in 0..self.delta_count {
            let mut key = Vec::with_capacity(self.key_columns);
            for column in 0..self.key_columns {
                key.push(self.delta_value(entry, column)?);
            }
            if let Ok(row) = self.search(&key)? {
                if !self.is_tombstoned(row)? {
                    return Err(corrupt(format!(
                        "delta row {entry} duplicates a live sorted-region key"
                    )));
                }
            }
        }
        Ok(())
    }

    /// Returns the same leaf, reading its key columns under these collations.
    ///
    /// @param collations - one per key column; short means BINARY for the rest
    pub fn with_collations(
        mut self,
        collations: &'p [inillucent_value::collation::Collation],
    ) -> LeafRef<'p> {
        self.collations = collations;
        self
    }

    /// Returns the same leaf, reading its key columns in these directions.
    ///
    /// @param directions - one per key column; short means ascending
    pub fn with_directions(mut self, directions: &'p [bool]) -> LeafRef<'p> {
        self.directions = directions;
        self
    }

    /// Returns whether one key column is stored descending.
    ///
    /// @param index - the key column's position
    pub fn descending_at(&self, index: usize) -> bool {
        self.directions.get(index).copied().unwrap_or(false)
    }

    /// Returns a comparison with the column's direction applied.
    ///
    /// **One place, so a comparison cannot be written that forgets.** Every
    /// order this leaf produces - the binary search, the bound tests, the row
    /// comparison - goes through here, so a descending column is descending for
    /// all of them or for none.
    ///
    /// @param order - the ascending comparison
    /// @param index - which key column it was made on
    fn directed(&self, order: std::cmp::Ordering, index: usize) -> std::cmp::Ordering {
        if self.descending_at(index) {
            order.reverse()
        } else {
            order
        }
    }

    /// Attaches the out-of-line values a caller has read.
    ///
    /// Every accessor then answers for them exactly as it does for an inline
    /// value, which is what keeps the extent out of the readers: a scan does not
    /// branch on where a value is, only on whether the leaf has any.
    ///
    /// @param extents - the resolved values, which must outlive the view
    pub fn with_extents(self, extents: &'p Extents) -> LeafRef<'p> {
        LeafRef {
            extents: Some(extents),
            ..self
        }
    }

    /// Returns the collation of one key column.
    ///
    /// @param index - the key column's position
    pub fn collation_of(&self, index: usize) -> inillucent_value::collation::Collation {
        self.collations
            .get(index)
            .copied()
            .unwrap_or(inillucent_value::collation::Collation::Binary)
    }

    /// Returns the raw page bytes.
    pub fn bytes(&self) -> &'p [u8] {
        self.page
    }

    /// Returns the number of rows in the sorted region.
    ///
    /// This counts tombstoned rows; [`LeafRef::live_rows`] does not.
    pub fn row_count(&self) -> usize {
        self.row_count
    }

    /// Returns the number of columns.
    pub fn column_count(&self) -> usize {
        self.column_count
    }

    /// Returns how many leading columns form the key.
    pub fn key_columns(&self) -> usize {
        self.key_columns
    }

    /// Returns the commit timestamp of the last modification.
    pub fn max_cts(&self) -> u64 {
        page::read_u64(self.page, leaf_header::MAX_CTS).unwrap_or(0)
    }

    /// Returns the right sibling, or [`PageId::NONE`].
    pub fn right_sibling(&self) -> PageId {
        PageId(page::read_u64(self.page, header::RIGHT).unwrap_or(0))
    }

    /// Reports whether any class array holds an exception.
    pub fn has_exceptions(&self) -> bool {
        self.flags & LEAF_HAS_EXCEPTIONS != 0
    }

    /// Reports whether the tombstone bitmap is present.
    pub fn has_tombstones(&self) -> bool {
        self.flags & LEAF_HAS_TOMBSTONES != 0
    }

    /// Reports whether any value in this leaf is stored out of line.
    ///
    /// **A reader that ignores this gets an error, not a wrong answer.** The
    /// leaf's own accessors cannot return an out-of-line value - the bytes are
    /// on pages this view does not hold and it has no pool to fetch them
    /// through - so [`MiniColumn::value`] refuses one by name. A consumer that
    /// sees this flag asks the *tree* for the rows instead, which is the same
    /// shape [`LeafRef::has_writes`] already has: one flag test per leaf that
    /// sends the reader down a materialising path rather than the vectorised
    /// one.
    pub fn has_extents(&self) -> bool {
        self.flags & LEAF_HAS_EXTENTS != 0
    }

    /// Returns the extent reference one out-of-line value names.
    ///
    /// @param row - the row's position in the sorted region
    /// @param column - which column
    pub fn extent_at(&self, row: usize, column: usize) -> DbResult<ExtentRef> {
        self.column(column)?.extent(row)
    }

    /// Returns where the heap begins, which is where the delta area ends.
    pub fn heap_start(&self) -> usize {
        self.heap_start
    }

    /// Reports whether the leaf is on the vectorised fast path.
    ///
    /// A leaf with no exceptions, no tombstones and no delta rows yields column
    /// vectors that borrow the page directly and need no merge, no filter and
    /// no per-row branch. This is the case the whole design optimises for and
    /// the case a freshly built or freshly compacted leaf is in.
    pub fn is_clean(&self) -> bool {
        !self.has_exceptions()
            && !self.has_extents()
            && !self.has_tombstones()
            && self.delta_count == 0
    }

    /// Reports whether this leaf's rows have to be merged rather than read as
    /// mini-columns.
    ///
    /// **One predicate, because there is one decision.** A leaf can fail to be
    /// readable as vectors for two unrelated reasons - a write put rows in its
    /// delta area or tombstoned some of its sorted ones, or a build sent one of
    /// its values out of line - and a consumer that tested only the first would
    /// read a sixteen-byte extent reference through a slot accessor that expects
    /// an offset and a length. The length half of that reference is zero, so the
    /// answer would be an empty string rather than an error.
    ///
    /// An *exception* is deliberately not here: an exception is read through the
    /// general vector path and stays vectorised. Confusing the two cost the SLT
    /// corpus thirty-four refusals once, which is why the two flags are separate.
    pub fn needs_materialising(&self) -> bool {
        self.has_writes() || self.has_extents()
    }

    /// Reports whether the leaf holds anything only a *write* can put there.
    ///
    /// Distinct from [`LeafRef::is_clean`], and the distinction cost the SLT
    /// corpus thirty-four refusals. `is_clean` means "on the vectorised fast
    /// path", and a leaf with *exceptions* is not - but it is perfectly
    /// readable, because an exception is a value of the wrong class and the
    /// scan's own vector builder falls back to the general path for that
    /// column. The corpus's `people` table has an untyped column, so every one
    /// of its leaves has exceptions, and a reader that refused them refused the
    /// table.
    ///
    /// Tombstones and delta rows are the ones that really do not arrive until
    /// Phase 3, because only a write makes one.
    pub fn has_writes(&self) -> bool {
        self.has_tombstones() || self.delta_count > 0
    }

    /// Returns the number of live rows: sorted rows less tombstones, plus delta
    /// rows.
    pub fn live_rows(&self) -> DbResult<usize> {
        let mut live = self.row_count;
        if self.has_tombstones() {
            let bitmap = self.tombstones()?;
            let dead = bitmap
                .iter()
                .map(|byte| byte.count_ones() as usize)
                .sum::<usize>();
            live = live.saturating_sub(dead);
        }
        Ok(live.saturating_add(self.delta_count))
    }

    /// Returns the byte offset of one mini-column's class array.
    ///
    /// @param index - the column's position in the directory
    fn column_offset(&self, index: usize) -> DbResult<usize> {
        let entry = leaf_header::DIRECTORY
            .checked_add(index.saturating_mul(self.entry_size))
            .ok_or_else(|| corrupt("directory index overflows"))?;
        Ok(page::read_u32(self.page, entry.saturating_add(4))? as usize)
    }

    /// Returns how many bytes one of a column's value slots occupies.
    ///
    /// **Read off the page rather than derived from the physical type**, which
    /// is what lets an integer column spend one byte a row where its values fit
    /// in one. The field has been written since the format existed and every
    /// page ever produced carries the right answer in it, so a file written
    /// before [`NARROW_INT_SLOTS`] existed reads here exactly as it did.
    ///
    /// @param index - the column's position in the directory
    pub fn column_width(&self, index: usize) -> DbResult<usize> {
        let entry = leaf_header::DIRECTORY
            .checked_add(index.saturating_mul(self.entry_size))
            .ok_or_else(|| corrupt("directory index overflows"))?;
        let physical = PhysicalType::from_code(
            self.page
                .get(entry)
                .copied()
                .ok_or_else(|| corrupt("directory entry runs past the page"))?,
        )?;
        let width = page::read_u16(self.page, entry.saturating_add(2))? as usize;
        if !physical.admits_width(width) {
            return Err(corrupt(format!(
                "column {index} claims a slot width of {width}"
            )));
        }
        Ok(width)
    }

    /// Returns how many bytes one column directory entry occupies here.
    pub fn directory_entry_size(&self) -> usize {
        self.entry_size
    }

    /// Returns the frame of reference one column's slots are measured from.
    ///
    /// Zero for every column of a page whose directory entries are eight bytes,
    /// which is every page written before [`LEAF_WIDE_DIRECTORY`] existed and
    /// every column that has no use for a base.
    ///
    /// @param index - the column's position in the directory
    pub fn column_base(&self, index: usize) -> DbResult<i64> {
        if self.entry_size < DIRECTORY_ENTRY_WIDE {
            return Ok(0);
        }
        let entry = leaf_header::DIRECTORY
            .checked_add(index.saturating_mul(self.entry_size))
            .ok_or_else(|| corrupt("directory index overflows"))?;
        Ok(page::read_u64(self.page, entry.saturating_add(8))? as i64)
    }

    /// Returns the directory entry for one column.
    ///
    /// @param index - the column's position in the directory
    pub fn spec(&self, index: usize) -> DbResult<ColumnSpec> {
        if index >= self.column_count {
            return Err(misuse(format!("column {index} does not exist")));
        }
        let entry = leaf_header::DIRECTORY
            .checked_add(index.saturating_mul(self.entry_size))
            .ok_or_else(|| corrupt("directory index overflows"))?;
        let type_byte = self
            .page
            .get(entry)
            .copied()
            .ok_or_else(|| corrupt("directory entry runs past the page"))?;
        let flags = self
            .page
            .get(entry.saturating_add(1))
            .copied()
            .ok_or_else(|| corrupt("directory entry runs past the page"))?;
        Ok(ColumnSpec {
            physical: PhysicalType::from_code(type_byte)?,
            flags,
            // The page does not carry a collation and must not: the catalog
            // says what a column's collation is, and a page that carried its
            // own could disagree with it. A caller that needs the collation
            // has the column directory it built the tree from.
            collation: inillucent_value::collation::Collation::Binary,
            // Nor a direction, for the same reason.
            descending: false,
        })
    }

    /// Returns a view over one mini-column.
    ///
    /// @param index - the column's position in the directory
    pub fn column(&self, index: usize) -> DbResult<MiniColumn<'p>> {
        let spec = self.spec(index)?;
        let width = self.column_width(index)?;
        let base = self.column_base(index)?;
        let start = self.column_offset(index)?;
        let class_len = class_bytes(self.row_count);
        let value_len = self.row_count.saturating_mul(width);
        let class = self
            .page
            .get(start..start.saturating_add(class_len))
            .ok_or_else(|| corrupt(format!("class array of column {index} runs past the page")))?;
        let values_at = start.saturating_add(class_len);
        let values = self
            .page
            .get(values_at..values_at.saturating_add(value_len))
            .ok_or_else(|| corrupt(format!("values of column {index} run past the page")))?;
        Ok(MiniColumn {
            physical: spec.physical,
            flags: spec.flags,
            width,
            base,
            class,
            values,
            rows: self.row_count,
            page: self.page,
            extents: self.extents,
            index,
        })
    }

    /// Returns where the tombstone bitmap starts, which is also where the
    /// mini-columns must end.
    fn tombstones_start(&self) -> DbResult<usize> {
        if !self.has_tombstones() {
            return Ok(self.delta_start);
        }
        self.delta_start
            .checked_sub(tombstone_bytes(self.row_count))
            .ok_or_else(|| corrupt("the tombstone bitmap runs below the mini-columns"))
    }

    /// Returns the tombstone bitmap, one bit per sorted-region row.
    pub fn tombstones(&self) -> DbResult<&'p [u8]> {
        if !self.has_tombstones() {
            return Ok(&[]);
        }
        let start = self.tombstones_start()?;
        self.page
            .get(start..self.delta_start)
            .ok_or_else(|| corrupt("the tombstone bitmap runs past the page"))
    }

    /// Reports whether one sorted-region row has been deleted.
    ///
    /// @param row - the row's position in the sorted region
    pub fn is_tombstoned(&self, row: usize) -> DbResult<bool> {
        if !self.has_tombstones() {
            return Ok(false);
        }
        let bitmap = self.tombstones()?;
        let byte = bitmap
            .get(row / 8)
            .copied()
            .ok_or_else(|| corrupt(format!("row {row} is outside the tombstone bitmap")))?;
        Ok(byte & (1u8 << (row % 8)) != 0)
    }

    /// Returns one value of one sorted-region row.
    ///
    /// @param row - the row's position in the sorted region
    /// @param column - which column to decode
    pub fn value(&self, row: usize, column: usize) -> DbResult<Datum<'p>> {
        self.column(column)?.value(row)
    }

    /// Returns where a key sits in this leaf: sorted region, delta area, or absent.
    ///
    /// **One implementation, two callers.** The write path asks this before it
    /// changes a row, and recovery asks it before it replays one - and the two
    /// have to agree about which row a key names, or a replay would tombstone a
    /// different row than the original delete did. It lives here rather than on
    /// `PagedTree` because recovery has no tree: the trees are attached after
    /// the file opens, and the file does not open until recovery has finished.
    ///
    /// A tombstoned row in the sorted region reads as absent from *there* and
    /// the delta area is searched anyway, because a key can be tombstoned in
    /// the sorted region and live again in the delta area - which is exactly
    /// what an insert over a deleted key leaves behind.
    ///
    /// @param key - the key, one value per key column
    /// @param key_columns - how many leading columns form the key
    pub fn locate(&self, key: &[Datum<'_>], key_columns: usize) -> DbResult<crate::write::Located> {
        for index in 0..self.delta_count() {
            if self.delta_key_matches(index, key, key_columns)? {
                return Ok(crate::write::Located::Delta(index));
            }
        }
        if let Ok(row) = self.search(key)? {
            if !self.is_tombstoned(row)? {
                return Ok(crate::write::Located::Sorted(row));
            }
        }
        Ok(crate::write::Located::Absent)
    }

    /// Returns a reusable view of the leaf's key columns.
    ///
    /// A binary search over a leaf makes about eleven comparisons, and each one
    /// re-derived the key columns' directory entries and slice bounds from the
    /// page. That is a dozen bounds-checked reads per comparison to rediscover
    /// something that does not change, and on a skip scan - which searches a
    /// leaf per seek per distinct value - it was the whole cost of the query.
    /// Deriving it once per search and passing it down makes the comparison
    /// two slice reads.
    pub fn key_view(&self) -> DbResult<KeyView<'p>> {
        let mut columns: [Option<MiniColumn<'p>>; KEY_VIEW_INLINE] = Default::default();
        let width = self.key_columns.min(KEY_VIEW_INLINE);
        for index in 0..width {
            if let Some(slot) = columns.get_mut(index) {
                *slot = Some(self.column(index)?);
            }
        }
        Ok(KeyView {
            columns,
            width: self.key_columns,
        })
    }

    /// Compares one sorted-region row's key against a probe key.
    ///
    /// @param row - the row's position in the sorted region
    /// @param probe - the key to compare against, one value per key column
    pub fn compare_key(&self, row: usize, probe: &[Datum<'_>]) -> DbResult<std::cmp::Ordering> {
        self.compare_key_with(&self.key_view()?, row, probe)
    }

    /// Compares one row's key against a probe, through a prepared key view.
    ///
    /// @param view - the leaf's key columns, from [`LeafRef::key_view`]
    /// @param row - the row's position in the sorted region
    /// @param probe - the key to compare against, one value per key column
    pub fn compare_key_with(
        &self,
        view: &KeyView<'p>,
        row: usize,
        probe: &[Datum<'_>],
    ) -> DbResult<std::cmp::Ordering> {
        for (index, wanted) in probe.iter().enumerate().take(view.width) {
            let held = match view.columns.get(index).and_then(|held| held.as_ref()) {
                Some(column) => column.value(row)?,
                // Past the inline capacity: fall back to the general path,
                // which is correct and only slower.
                None => self.value(row, index)?,
            };
            let order = self.directed(
                crate::types::compare_under(&held, wanted, self.collation_of(index)),
                index,
            );
            if order != std::cmp::Ordering::Equal {
                return Ok(order);
            }
        }
        Ok(std::cmp::Ordering::Equal)
    }

    /// Returns the run of sorted rows whose key begins with a prefix.
    ///
    /// The half-open range `begin..end`, empty when the prefix is not present.
    ///
    /// An index nested loop asks this once per outer row, so the shape matters:
    /// the lower bound is a search, and the *upper* bound is a short forward
    /// walk rather than a second search, because the run a join probe finds is
    /// usually one entry long. Past `scan_cap` matching rows it stops walking
    /// and bisects, so a prefix that matches a whole leaf still costs a search.
    ///
    /// The integer fast path reads the leading key column's raw values, which
    /// is what [`LeafRef::lower_bound`] already does for the search; doing it
    /// for the walk as well is what removes the key view and the two
    /// `Datum` comparisons that a probe finding one entry was paying.
    ///
    /// @param prefix - the prefix to match, one value per compared column
    /// @param scan_cap - how far the run is walked before the end is bisected
    pub fn equal_run(&self, prefix: &[Datum<'_>], scan_cap: usize) -> DbResult<(usize, usize)> {
        // The guide is built once and used for both ends. Asking for it again
        // is not free: `all_typed` walks the column's class array, which on a
        // leaf holding 1,667 index entries is 417 bytes, and a probe that
        // called `lower_bound` and then re-derived the guide walked it twice.
        if let [Datum::Int(target)] = prefix {
            if let Some(guide) = self.integer_guide(prefix)? {
                let begin = self.partition_integer(&guide, *target, false)?;
                if begin >= self.row_count || guide.read(begin)? != *target {
                    return Ok((begin, begin));
                }
                let mut end = begin.saturating_add(1);
                while end < self.row_count
                    && end.saturating_sub(begin) < scan_cap
                    && guide.read(end)? == *target
                {
                    end = end.saturating_add(1);
                }
                if end.saturating_sub(begin) >= scan_cap {
                    end = self
                        .partition_integer(&guide, *target, true)?
                        .min(self.row_count);
                }
                return Ok((begin, end));
            }
        }
        let begin = self.lower_bound(prefix)?;
        if begin >= self.row_count {
            return Ok((begin, begin));
        }
        let view = self.key_view()?;
        if self.compare_key_with(&view, begin, prefix)? != std::cmp::Ordering::Equal {
            return Ok((begin, begin));
        }
        let mut end = begin.saturating_add(1);
        while end < self.row_count
            && end.saturating_sub(begin) < scan_cap
            && self.compare_key_with(&view, end, prefix)? == std::cmp::Ordering::Equal
        {
            end = end.saturating_add(1);
        }
        if end.saturating_sub(begin) >= scan_cap {
            end = self.upper_bound(prefix)?.min(self.row_count);
        }
        Ok((begin, end))
    }

    /// Finds the position of a key in the sorted region.
    ///
    /// Returns `Ok(row)` when the key is present and `Err(insertion point)` when
    /// it is not, which is `slice::binary_search`'s contract and the shape both
    /// the point probe and the insert path want.
    ///
    /// @param probe - the key to look for, one value per key column
    pub fn search(&self, probe: &[Datum<'_>]) -> DbResult<Result<usize, usize>> {
        if self.key_columns == 1 {
            // **The column comes back with the target.** Both halves need the
            // key mini-column - one to decide the probe is an integer one, the
            // other to interpolate over its values - and parsing the directory
            // entry twice to hand the same four fields back twice is a
            // measurable part of a 40 ns leaf search on a probe that does two
            // or three value reads in total.
            if let Some((target, column)) = self.integer_key_probe(probe)? {
                return self.search_integer_key(target, &column);
            }
        }
        let view = self.key_view()?;
        self.search_between(&view, probe, 0, self.row_count)
    }

    /// Binary-searches a window of the sorted region.
    ///
    /// @param view - the leaf's key columns
    /// @param probe - the key to look for
    /// @param from - the first row of the window
    /// @param to - one past the last row of the window
    fn search_between(
        &self,
        view: &KeyView<'p>,
        probe: &[Datum<'_>],
        from: usize,
        to: usize,
    ) -> DbResult<Result<usize, usize>> {
        let mut low = from;
        let mut high = to;
        while low < high {
            let middle = low.saturating_add(high.saturating_sub(low) / 2);
            match self.compare_key_with(view, middle, probe)? {
                std::cmp::Ordering::Less => low = middle.saturating_add(1),
                std::cmp::Ordering::Greater => high = middle,
                std::cmp::Ordering::Equal => return Ok(Ok(middle)),
            }
        }
        Ok(Err(low))
    }

    /// Returns the integer a probe is looking for, when this leaf is one a
    /// rowid tree would have.
    ///
    /// The conditions are narrow on purpose: one key column, physically
    /// `Int64`, every row typed, and an integer probe. Anything else - a
    /// compound key, a NULL, an exception row, a text probe against an integer
    /// column - falls through to the general comparison, which is correct for
    /// all of them.
    ///
    /// @param probe - the key being looked for
    fn integer_key_probe(&self, probe: &[Datum<'_>]) -> DbResult<Option<(i64, MiniColumn<'p>)>> {
        if probe.len() != 1 || self.row_count < 8 {
            return Ok(None);
        }
        let Some(Datum::Int(target)) = probe.first() else {
            return Ok(None);
        };
        let column = self.column(0)?;
        if column.physical != PhysicalType::Int64 || !column.all_typed() {
            return Ok(None);
        }
        Ok(Some((*target, column)))
    }

    /// Finds an integer key by interpolation, falling back to a binary search.
    ///
    /// **This is a measurement, not a preference.** A 32 KiB leaf holds a few
    /// hundred rows, and a binary search over them touches a scattered cache
    /// line per step: measured at **120 ns** against a 100 ns descent and a
    /// 423 ns point probe, so the search was the largest single cost in three
    /// of the four read families - `read.point`, `read.range` through a rowid
    /// lookup, and `read.analytical` through the skip scan's per-seek search.
    ///
    /// Interpolation converges in one or two steps on keys that are anywhere
    /// near uniform, which a rowid is by construction: `INTEGER PRIMARY KEY`
    /// values are handed out in order. It is *not* a promise about arbitrary
    /// data, so the step count is capped and what is left of the window is
    /// binary searched. The worst case is therefore a binary search plus four
    /// probes, and the ordinary case is two.
    ///
    /// @param target - the integer being looked for
    fn search_integer_key(
        &self,
        target: i64,
        column: &MiniColumn<'p>,
    ) -> DbResult<Result<usize, usize>> {
        /// How many interpolation steps before giving up and bisecting.
        ///
        /// Four, because a distribution that has not converged in four steps is
        /// not one interpolation is going to help with, and because bounding it
        /// is what makes the worst case no worse than the search it replaces.
        const STEPS: usize = 4;

        let values = column.inline_bytes();
        let width = column.width;
        let base = column.base;
        let read = |row: usize| -> DbResult<i64> {
            let at = row.saturating_mul(width);
            let slice = values
                .get(at..at.saturating_add(width))
                .ok_or_else(|| corrupt(format!("row {row} is past the key column")))?;
            Ok(from_frame(base, width, slice))
        };

        let mut low = 0usize;
        let mut high = self.row_count.saturating_sub(1);
        let mut low_value = read(low)?;
        let mut high_value = read(high)?;
        if target < low_value {
            return Ok(Err(0));
        }
        if target > high_value {
            return Ok(Err(self.row_count));
        }
        for _ in 0..STEPS {
            if low > high {
                break;
            }
            if low_value == high_value {
                return Ok(if low_value == target {
                    Ok(low)
                } else {
                    Err(low)
                });
            }
            // The guess, in i128 so a span of nearly the whole integer range
            // cannot overflow the multiply.
            let span = i128::from(high_value).saturating_sub(i128::from(low_value));
            let into = i128::from(target).saturating_sub(i128::from(low_value));
            let width = (high.saturating_sub(low)) as i128;
            let offset = if span == 0 { 0 } else { into * width / span };
            let guess = low.saturating_add(offset.max(0).min(width) as usize);
            let seen = read(guess)?;
            match seen.cmp(&target) {
                std::cmp::Ordering::Equal => return Ok(Ok(guess)),
                std::cmp::Ordering::Less => {
                    low = guess.saturating_add(1);
                    if low > high {
                        return Ok(Err(low));
                    }
                    low_value = read(low)?;
                    if target < low_value {
                        return Ok(Err(low));
                    }
                }
                std::cmp::Ordering::Greater => {
                    if guess == 0 {
                        return Ok(Err(0));
                    }
                    high = guess.saturating_sub(1);
                    if low > high {
                        return Ok(Err(low));
                    }
                    high_value = read(high)?;
                    if target > high_value {
                        return Ok(Err(high.saturating_add(1)));
                    }
                }
            }
        }
        // Whatever window is left, bisected. The general comparison is used so
        // that this path and the one above cannot disagree about ordering.
        let view = self.key_view()?;
        self.search_between(&view, &[Datum::Int(target)], low, high.saturating_add(1))
    }

    /// Returns the first row whose key is at or above a probe.
    ///
    /// The bound an index range and an index nested loop each need once per
    /// leaf. It is an ordinary bound search with an **interpolated midpoint**
    /// for its first few steps: when the leading key column is an all-typed
    /// `Int64` - which every index on an integer column is - a guess placed by
    /// proportion lands far closer than the middle, and the loop's invariant is
    /// unchanged by where the midpoint came from.
    ///
    /// That last sentence is the whole correctness argument, and it is why the
    /// interpolation is here rather than in a separate narrowing pass: a pass
    /// that returned a *window* would have to be right about the window, and a
    /// run longer than it would put the answer outside. A midpoint cannot be
    /// wrong; it can only be a poor guess, and after a few of those the loop
    /// falls back to bisection.
    ///
    /// @param probe - the bound, one value per compared column
    pub fn lower_bound(&self, probe: &[Datum<'_>]) -> DbResult<usize> {
        self.bounded(probe, false)
    }

    /// Returns the first row whose key is above a probe.
    ///
    /// @param probe - the bound, one value per compared column
    pub fn upper_bound(&self, probe: &[Datum<'_>]) -> DbResult<usize> {
        self.bounded(probe, true)
    }

    /// The shared bound search.
    ///
    /// @param probe - the bound
    /// @param past_equal - whether a row equal to the probe is below the bound
    fn bounded(&self, probe: &[Datum<'_>], past_equal: bool) -> DbResult<usize> {
        /// How many interpolated midpoints before falling back to bisection.
        const GUESSES: u32 = 4;

        // The integer fast path, and it is a measurement rather than a
        // preference. A bound over one integer column is what a skip scan seeks
        // with and what an index nested loop probes with, and the generic
        // search reads a `Datum` out of the mini-column and compares it under a
        // collation at every step. `inillucent-probeprofile` measured a prefix
        // probe into `side_owner` - two integer key columns, 25,000 entries -
        // at 209 ns for the descent plus this search, of which the descent was
        // 62 ns. `join.range` pays it 201 times per execution.
        //
        // When the probe is one integer and the leading key column is a fully
        // typed `Int64` mini-column, the same partition point is found over a
        // contiguous run of eight-byte values, with the interpolation guide
        // still choosing the midpoints. Comparing only column zero is exactly
        // what the generic path does for a one-column probe, so this is the
        // same answer by a shorter route rather than a different one.
        if let [Datum::Int(target)] = probe {
            if let Some(guide) = self.integer_guide(probe)? {
                return self.partition_integer(&guide, *target, past_equal);
            }
        }

        let view = self.key_view()?;
        let guide = self.integer_guide(probe)?;
        let mut low = 0usize;
        let mut high = self.row_count;
        let mut guesses = 0u32;
        while low < high {
            let middle = match &guide {
                Some(guide) if guesses < GUESSES => {
                    guesses = guesses.saturating_add(1);
                    guide.between(low, high)?
                }
                _ => low.saturating_add(high.saturating_sub(low) / 2),
            };
            let order = self.compare_key_with(&view, middle, probe)?;
            let below = match order {
                std::cmp::Ordering::Less => true,
                std::cmp::Ordering::Equal => past_equal,
                std::cmp::Ordering::Greater => false,
            };
            if below {
                low = middle.saturating_add(1);
            } else {
                high = middle;
            }
        }
        Ok(low)
    }

    /// Returns the partition point of an integer bound over column zero.
    ///
    /// @param guide - the interpolation guide over the leading key column
    /// @param target - the integer being bounded
    /// @param past_equal - whether a row equal to the target is below the bound
    fn partition_integer(
        &self,
        guide: &IntegerGuide<'p>,
        target: i64,
        past_equal: bool,
    ) -> DbResult<usize> {
        /// How many interpolated midpoints before falling back to bisection.
        const GUESSES: u32 = 4;

        let mut low = 0usize;
        let mut high = self.row_count;
        let mut guesses = 0u32;
        while low < high {
            let middle = if guesses < GUESSES {
                guesses = guesses.saturating_add(1);
                guide.between(low, high)?
            } else {
                low.saturating_add(high.saturating_sub(low) / 2)
            };
            let held = guide.read(middle)?;
            let below = if past_equal {
                held <= target
            } else {
                held < target
            };
            if below {
                low = middle.saturating_add(1);
            } else {
                high = middle;
            }
        }
        Ok(low)
    }

    /// Returns what is needed to interpolate on the leading key column.
    ///
    /// `None` whenever interpolation does not apply, which leaves the bound
    /// search an ordinary bisection.
    ///
    /// @param probe - the bound
    fn integer_guide(&self, probe: &[Datum<'_>]) -> DbResult<Option<IntegerGuide<'p>>> {
        if probe.is_empty() || self.row_count < 8 {
            return Ok(None);
        }
        let Some(Datum::Int(target)) = probe.first() else {
            return Ok(None);
        };
        let column = self.column(0)?;
        if column.physical != PhysicalType::Int64 || !column.all_typed() {
            return Ok(None);
        }
        // A collation is an order over text and never changes where an integer
        // sits, so interpolation on an integer column is valid under any of
        // them. The guard is here so that a collation that *did* reorder
        // numbers would turn it off rather than silently mis-guess.
        if self.collation_of(0) != inillucent_value::collation::Collation::Binary {
            return Ok(None);
        }
        // **And off entirely for a descending column.** Interpolation assumes
        // the values rise across the leaf; in a descending tree they fall, and
        // a guess made on the wrong slope is a guess that lands past the row it
        // was looking for.
        if self.descending_at(0) {
            return Ok(None);
        }
        Ok(Some(IntegerGuide {
            values: column.inline_bytes(),
            width: column.width,
            base: column.base,
            target: *target,
        }))
    }

    /// Returns the leaf's live rows in key order, as a source the builder reads
    /// through.
    ///
    /// **The allocation-free half of [`LeafRef::live`], and the one a compaction
    /// wants.** It also does asymptotically less work: the sorted region is
    /// already in key order, so the delta rows, at most [`DELTA_LIMIT`] of
    /// them, are merged into it by **binary search** rather than the whole leaf
    /// being sorted again.
    ///
    /// The shadowing rules are `live`'s, and the two are checked against each
    /// other by `live_order_agrees_with_live`.
    pub fn live_source(&self) -> DbResult<LiveSource<'p>> {
        let mut columns = Vec::with_capacity(self.column_count);
        for index in 0..self.column_count {
            columns.push(self.column(index)?);
        }
        // Each delta row decoded once, through `delta_row_values`, and reused
        // by every comparison below - `compare_live` reads from this `Vec`
        // rather than the page, so a row placed by binary search against `n`
        // entries already here is not `n` more trips through `delta_value`.
        let mut delta: Vec<Vec<Datum<'p>>> = Vec::with_capacity(self.delta_count);
        for index in 0..self.delta_count {
            delta.push(self.delta_row_values(index)?);
        }

        let mut order: Vec<LiveRow> =
            Vec::with_capacity(self.row_count.saturating_add(self.delta_count));
        for row in 0..self.row_count {
            if self.is_tombstoned(row)? {
                continue;
            }
            order.push(LiveRow::Sorted(row as u32));
        }
        // The newest entry for a key wins and "newest" is the lowest delta
        // index, so an entry whose key a lower index already placed is dropped.
        let mut placed: Vec<u32> = Vec::new();
        for index in 0..self.delta_count {
            let entry = LiveRow::Delta(index as u32);
            let mut shadowed = false;
            for earlier in &placed {
                if self.compare_live(&columns, &delta, LiveRow::Delta(*earlier), entry)?
                    == std::cmp::Ordering::Equal
                {
                    shadowed = true;
                    break;
                }
            }
            if shadowed {
                continue;
            }
            placed.push(index as u32);
            // Binary search, not a scan: the delta area holds at most
            // `DELTA_LIMIT` rows and the sorted region holds thousands, and a
            // scan per delta row made a compaction quadratic in the leaf.
            let mut low = 0usize;
            let mut high = order.len();
            let mut found = None;
            while low < high {
                let mid = low.saturating_add(high.saturating_sub(low) / 2);
                let held = order.get(mid).copied().unwrap_or(LiveRow::Sorted(0));
                match self.compare_live(&columns, &delta, held, entry)? {
                    std::cmp::Ordering::Less => low = mid.saturating_add(1),
                    std::cmp::Ordering::Greater => high = mid,
                    std::cmp::Ordering::Equal => {
                        found = Some(mid);
                        break;
                    }
                }
            }
            // A delta row is newer than the sorted region, so it *replaces* the
            // row it shadows rather than joining it.
            match found {
                Some(at) => {
                    if let Some(slot) = order.get_mut(at) {
                        *slot = entry;
                    }
                }
                None => order.insert(low, entry),
            }
        }
        // The order is settled; now one flat pass to materialise it. Reading
        // through the mini-columns here rather than in the builder's two passes
        // means each value is decoded once instead of twice.
        let mut values = Vec::with_capacity(order.len().saturating_mul(self.column_count));
        for at in &order {
            for column in 0..self.column_count {
                values.push(live_value(&columns, &delta, *at, column)?);
            }
        }
        Ok(LiveSource {
            values,
            width: self.column_count,
        })
    }

    /// Compares two live rows by their key columns, reading through the views.
    ///
    /// @param columns - the mini-columns, derived once
    /// @param delta - the delta rows, decoded once
    /// @param left - one row's position
    /// @param right - the other's
    fn compare_live(
        &self,
        columns: &[MiniColumn<'p>],
        delta: &[Vec<Datum<'p>>],
        left: LiveRow,
        right: LiveRow,
    ) -> DbResult<std::cmp::Ordering> {
        for index in 0..self.key_columns {
            let a = live_value(columns, delta, left, index)?;
            let b = live_value(columns, delta, right, index)?;
            let order = self.directed(
                crate::types::compare_under(&a, &b, self.collation_of(index)),
                index,
            );
            if order != std::cmp::Ordering::Equal {
                return Ok(order);
            }
        }
        Ok(std::cmp::Ordering::Equal)
    }

    /// Materialises every live row, sorted region merged with the delta.    /// Materialises every live row, sorted region merged with the delta.
    ///
    /// This is what compaction, the property tests and every read path over a
    /// leaf that has been written to all need, and it is deliberately the one
    /// place the merge is written. A leaf that has *not* been written to never
    /// comes here: [`LeafRef::has_writes`] is false and the caller takes the
    /// vectorised path, which is the whole design.
    ///
    /// **A key the delta area holds twice keeps the newest.** The delta area
    /// grows downwards, so index 0 is the most recent insert; the merge below
    /// walks it in order and the first entry for a key wins. The write path
    /// removes the old entry rather than shadowing it, so this is a belt on top
    /// of braces - and it is the belt that makes `live()` correct on a page
    /// recovery replayed rather than on one this process built.
    pub fn live(&self) -> DbResult<Vec<Vec<Datum<'p>>>> {
        let mut rows: Vec<Vec<Datum<'p>>> =
            Vec::with_capacity(self.row_count.saturating_add(self.delta_count));
        for row in 0..self.row_count {
            if self.is_tombstoned(row)? {
                continue;
            }
            let mut values = Vec::with_capacity(self.column_count);
            for column in 0..self.column_count {
                values.push(self.value(row, column)?);
            }
            rows.push(values);
        }
        let sorted_rows = rows.len();
        for index in 0..self.delta_count {
            // One pass over the row rather than one `delta_value` call per
            // column - see `delta_row_values` for why that used to cost the
            // square of the column count instead of the column count.
            let values = self.delta_row_values(index)?;
            // The newest wins, and "newest" is the *lowest* delta index. A
            // shadowed entry is dropped here rather than sorted and deduped
            // afterwards, because a stable sort would keep whichever the
            // comparison happened to leave first.
            let shadowed = rows
                .get(sorted_rows..)
                .unwrap_or(&[])
                .iter()
                .any(|held| self.compare_keys(held, &values) == std::cmp::Ordering::Equal);
            if shadowed {
                continue;
            }
            // **A delta row is newer than the sorted region, so it replaces the
            // row it shadows rather than joining it.** Comparing only against
            // the other delta entries - which is what this did - emitted both
            // copies of every row that had been written after it was packed,
            // and a table read back twice as many rows as it held. It survived
            // for as long as it did because the two copies only exist together
            // after a compaction has moved rows into the sorted region and a
            // later write has put them back in the delta area, which is a state
            // a freshly written table never reaches and a reopened one does.
            let position = rows
                .get(..sorted_rows)
                .unwrap_or(&[])
                .iter()
                .position(|held| self.compare_keys(held, &values) == std::cmp::Ordering::Equal);
            match position.and_then(|at| rows.get_mut(at)) {
                Some(slot) => *slot = values,
                None => rows.push(values),
            }
        }
        rows.sort_by(|left, right| self.compare_keys(left, right));
        Ok(rows)
    }

    /// Visits every live row, projecting only the columns asked for.
    ///
    /// **The same merge [`LeafRef::live`] performs, without materialising a row
    /// per row and without reading the columns the caller did not ask for.**
    /// The sorted region minus its tombstones, with the delta area's rows
    /// replacing the ones they shadow and, where the delta area holds one key
    /// twice, the newest - the lowest index - winning.
    ///
    /// It exists because `CREATE INDEX` reads **two** columns of a table that
    /// may have six, over every row, and `live` hands it all six in a fresh
    /// `Vec` each. On a leaf that has never been written to that does not
    /// arise - the caller takes the vectorised mini-column path and `live` is
    /// not called at all - but the performance gate builds its index *after*
    /// its write workloads, so nearly every leaf has a delta entry by then and
    /// the whole scan went down the slow path. It was **14.3 ms** of a 38.9 ms
    /// `CREATE INDEX` there against 3.4 ms on a freshly imported table, and
    /// that gap is this.
    ///
    /// Rows are **not sorted**: a caller that needs key order sorts what it
    /// collects, and the index build sorts the whole table's entries once, so a
    /// sort per leaf would be work thrown away.
    ///
    /// @param columns - the columns to project, in the order to project them
    /// @param visit - called once per live row with those columns
    pub fn visit_live(
        &self,
        columns: &[usize],
        visit: &mut dyn FnMut(&[Datum<'p>]) -> DbResult<()>,
    ) -> DbResult<()> {
        // The delta area's keys, read once. Almost every leaf has none, and
        // then the sorted region needs no shadow test at all.
        let mut delta_keys: Vec<Vec<Datum<'p>>> = Vec::with_capacity(self.delta_count);
        for index in 0..self.delta_count {
            let mut key = Vec::with_capacity(self.key_columns);
            for column in 0..self.key_columns {
                key.push(self.delta_value(index, column)?);
            }
            delta_keys.push(key);
        }
        let projected_columns: Vec<MiniColumn<'p>> = columns
            .iter()
            .map(|column| self.column(*column))
            .collect::<DbResult<Vec<MiniColumn<'p>>>>()?;
        let key_columns: Vec<MiniColumn<'p>> = if delta_keys.is_empty() {
            Vec::new()
        } else {
            (0..self.key_columns)
                .map(|column| self.column(column))
                .collect::<DbResult<Vec<MiniColumn<'p>>>>()?
        };
        // The tombstone bitmap, derived once rather than per row.
        let tombstones = if self.has_tombstones() {
            Some(self.tombstones()?)
        } else {
            None
        };
        let mut row_key: Vec<Datum<'p>> = Vec::with_capacity(self.key_columns);
        let mut projected: Vec<Datum<'p>> = Vec::with_capacity(columns.len());
        for row in 0..self.row_count {
            if let Some(bitmap) = tombstones {
                let byte = bitmap
                    .get(row / 8)
                    .copied()
                    .ok_or_else(|| corrupt(format!("row {row} is outside the tombstone bitmap")))?;
                if byte & (1u8 << (row % 8)) != 0 {
                    continue;
                }
            }
            if !delta_keys.is_empty() {
                row_key.clear();
                for column in &key_columns {
                    row_key.push(column.value(row)?);
                }
                if delta_keys
                    .iter()
                    .any(|held| self.compare_keys(held, &row_key) == std::cmp::Ordering::Equal)
                {
                    // A delta entry for this key replaces the sorted row, so
                    // the sorted one is skipped and the delta one emitted below.
                    continue;
                }
            }
            projected.clear();
            for column in &projected_columns {
                projected.push(column.value(row)?);
            }
            visit(&projected)?;
        }
        for index in 0..self.delta_count {
            let Some(key) = delta_keys.get(index) else {
                continue;
            };
            let shadowed = delta_keys
                .get(..index)
                .unwrap_or(&[])
                .iter()
                .any(|held| self.compare_keys(held, key) == std::cmp::Ordering::Equal);
            if shadowed {
                continue;
            }
            projected.clear();
            for column in columns {
                projected.push(self.delta_value(index, *column)?);
            }
            visit(&projected)?;
        }
        Ok(())
    }

    /// Materialises the live rows inside a key range.
    ///
    /// The merged counterpart of the sorted region's `lower_bound`/`upper_bound`
    /// pair, for a leaf that has been written to. The bounds are compared under
    /// the leaf's own collations, which is what keeps a range over a `NOCASE`
    /// column agreeing with the order the tree is stored in.
    ///
    /// @param low - the lower bound, or `None` for the start
    /// @param low_inclusive - whether a key equal to `low` is in the range
    /// @param high - the upper bound, or `None` for the end
    /// @param high_inclusive - whether a key equal to `high` is in the range
    pub fn live_between(
        &self,
        low: Option<&[Datum<'_>]>,
        low_inclusive: bool,
        high: Option<&[Datum<'_>]>,
        high_inclusive: bool,
    ) -> DbResult<Vec<Vec<Datum<'p>>>> {
        let mut rows = self.live()?;
        rows.retain(|row| {
            if let Some(bound) = low {
                let order = self.compare_prefix(row, bound);
                let inside = if low_inclusive {
                    order != std::cmp::Ordering::Less
                } else {
                    order == std::cmp::Ordering::Greater
                };
                if !inside {
                    return false;
                }
            }
            if let Some(bound) = high {
                let order = self.compare_prefix(row, bound);
                let inside = if high_inclusive {
                    order != std::cmp::Ordering::Greater
                } else {
                    order == std::cmp::Ordering::Less
                };
                if !inside {
                    return false;
                }
            }
            true
        });
        Ok(rows)
    }

    /// Compares two materialised rows on their key columns, under the leaf's
    /// collations.
    ///
    /// @param left - one row
    /// @param right - the other row
    fn compare_keys(&self, left: &[Datum<'_>], right: &[Datum<'_>]) -> std::cmp::Ordering {
        for index in 0..self.key_columns {
            let (Some(a), Some(b)) = (left.get(index), right.get(index)) else {
                return std::cmp::Ordering::Equal;
            };
            let order = self.directed(
                crate::types::compare_under(a, b, self.collation_of(index)),
                index,
            );
            if order != std::cmp::Ordering::Equal {
                return order;
            }
        }
        std::cmp::Ordering::Equal
    }

    /// Compares a row against a probe that may be shorter than the key.
    ///
    /// A probe of two columns against a three-column key matches a *run*, so
    /// only the columns the probe names are compared - which is the same rule
    /// `lower_bound` and `upper_bound` follow over the sorted region, and the
    /// reason a prefix bound returns three rows rather than one.
    ///
    /// @param row - the row
    /// @param probe - the bound, one value per column it names
    fn compare_prefix(&self, row: &[Datum<'_>], probe: &[Datum<'_>]) -> std::cmp::Ordering {
        for (index, wanted) in probe.iter().enumerate() {
            let Some(held) = row.get(index) else {
                return std::cmp::Ordering::Less;
            };
            let order = self.directed(
                crate::types::compare_under(held, wanted, self.collation_of(index)),
                index,
            );
            if order != std::cmp::Ordering::Equal {
                return order;
            }
        }
        std::cmp::Ordering::Equal
    }

    /// Returns one value of a row wherever it lives.
    ///
    /// The sorted region and the delta area are read differently - one is a
    /// mini-column, the other a tagged row - and a caller that has been handed a
    /// [`Hit`] should not have to know which. Every probe path goes through
    /// this, so a delta row and a sorted row cannot be read by two rules that
    /// drift apart.
    ///
    /// @param hit - where the row is
    /// @param column - which column to read
    pub fn value_at(&self, hit: Hit, column: usize) -> DbResult<Datum<'p>> {
        match hit {
            Hit::Sorted(row) => self.value(row, column),
            Hit::Delta(index) => self.delta_value(index, column),
        }
    }
}

/// Where a row a probe found actually lives.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Hit {
    /// In the sorted region, at this row.
    Sorted(usize),
    /// In the delta area, at this index.
    Delta(usize),
}

/// Compares two materialised rows on their leading key columns.
///
/// @param left - one row
/// @param right - the other row
/// @param key_columns - how many leading columns form the key
pub fn compare_rows(
    left: &[Datum<'_>],
    right: &[Datum<'_>],
    key_columns: usize,
) -> std::cmp::Ordering {
    compare_rows_under(left, right, key_columns, &[], &[])
}

/// Compares two rows' keys the way the tree they came from is ordered.
///
/// **The same comparison the tree's own searches make, which is the point.** A
/// caller that checks a tree's ordering with `BINARY` while the tree is ordered
/// by `NOCASE` is not checking the tree - it is checking a different tree. The
/// integrity check did exactly that, and `REINDEX` over
/// `CREATE INDEX ix ON t(team COLLATE NOCASE)` holding `'Blue'` and `'BLUE'`
/// reported `a key does not increase across the leaf chain` about a tree whose
/// every read was correct.
///
/// Short slices default the rest: no collation is `BINARY` and no direction is
/// ascending, which is what every rowid tree is.
///
/// @param left - one row
/// @param right - the other
/// @param key_columns - how many leading columns form the key
/// @param collations - the collation of each key column
/// @param directions - whether each key column is stored descending
pub fn compare_rows_under(
    left: &[Datum<'_>],
    right: &[Datum<'_>],
    key_columns: usize,
    collations: &[inillucent_value::collation::Collation],
    directions: &[bool],
) -> std::cmp::Ordering {
    for index in 0..key_columns {
        let (Some(a), Some(b)) = (left.get(index), right.get(index)) else {
            return std::cmp::Ordering::Equal;
        };
        let collation = collations
            .get(index)
            .copied()
            .unwrap_or(inillucent_value::collation::Collation::Binary);
        let order = crate::types::compare_under(a, b, collation);
        let order = if directions.get(index).copied().unwrap_or(false) {
            order.reverse()
        } else {
            order
        };
        if order != std::cmp::Ordering::Equal {
            return order;
        }
    }
    std::cmp::Ordering::Equal
}

/// The bytes a class array of `rows` rows occupies: two bits each, padded to
/// eight bytes so the value array that follows is 8-byte aligned.
///
/// @param rows - how many rows the leaf holds
pub fn class_bytes(rows: usize) -> usize {
    rows.saturating_mul(2)
        .saturating_add(63)
        .checked_div(64)
        .unwrap_or(0)
        .saturating_mul(8)
}

/// The bytes a tombstone bitmap of `rows` rows occupies, padded to eight.
///
/// @param rows - how many rows the leaf holds
pub fn tombstone_bytes(rows: usize) -> usize {
    rows.saturating_add(63)
        .checked_div(64)
        .unwrap_or(0)
        .saturating_mul(8)
}

/// Reads one value of a live row through the derived views.
///
/// @param columns - the mini-columns
/// @param delta - the decoded delta rows
/// @param at - the row's position
/// @param column - which column
fn live_value<'p>(
    columns: &[MiniColumn<'p>],
    delta: &[Vec<Datum<'p>>],
    at: LiveRow,
    column: usize,
) -> DbResult<Datum<'p>> {
    match at {
        LiveRow::Sorted(row) => match columns.get(column) {
            Some(held) => held.value(row as usize),
            None => Ok(Datum::Null),
        },
        LiveRow::Delta(index) => Ok(delta
            .get(index as usize)
            .and_then(|values| values.get(column).copied())
            .unwrap_or(Datum::Null)),
    }
}

/// Where one live row of a leaf sits.
///
/// **The positional half of [`LeafRef::live`].** A compaction wants the leaf's
/// live rows in key order and then reads them column by column; `live` gives it
/// that as a `Vec<Vec<Datum>>`, which is one allocation per row plus one for the
/// outer vector. On an index leaf holding three and a half thousand entries that
/// is three and a half thousand allocations, per compaction, to produce values
/// that are already on the page and stay there.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LiveRow {
    /// Still in the sorted region, at this row number.
    Sorted(u32),
    /// In the delta area, at this index.
    Delta(u32),
}

/// A leaf's live rows in key order, as a [`Rows`] the builder packs from.
///
/// **One allocation, and a direct index per value.** `live` produces a
/// `Vec<Vec<Datum>>`, which is one allocation per row plus one for the outer
/// vector: on an index leaf holding three and a half thousand entries that is
/// three and a half thousand allocations per compaction. Reading straight
/// through the mini-columns instead removes them, but replaces every value
/// access with a class check and a slot decode - and on a leaf of many small
/// rows the decodes cost more than the allocations did: `txn.large` went from
/// 4.1 ms to 5.7 measuring exactly that.
///
/// So the values are materialised **once, flat**: one allocation of
/// `rows * width`, and `value` is an index into it. The builder makes two
/// passes over them - one to size the page and one to write it - and both are
/// a bounds-checked index.
pub struct LiveSource<'p> {
    /// `rows * width` values in key order.
    values: Vec<Datum<'p>>,
    /// How many columns each row has.
    width: usize,
}

impl<'p> LiveSource<'p> {
    /// How many live rows the leaf holds.
    pub fn len(&self) -> usize {
        if self.width == 0 {
            return 0;
        }
        self.values.len() / self.width
    }

    /// Reports whether the leaf holds none.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// How many columns each row has.
    pub fn width(&self) -> usize {
        self.width
    }
}

impl<'p> Rows<'p> for LiveSource<'p> {
    fn len(&self) -> usize {
        LiveSource::len(self)
    }

    fn value(&self, row: usize, column: usize) -> Datum<'p> {
        self.values
            .get(row.saturating_mul(self.width).saturating_add(column))
            .copied()
            .unwrap_or(Datum::Null)
    }
}

/// A view over one column's class array and value slots.
#[derive(Clone, Copy, Debug)]
pub struct MiniColumn<'p> {
    /// The layout of the value slots.
    pub physical: PhysicalType,
    /// The directory entry's flag byte.
    pub flags: u8,
    /// How many bytes one slot occupies, as the column directory says.
    ///
    /// `physical.slot_width()` for every type but `Int64`, which may be 1, 2, 4
    /// or 8 - see [`NARROW_INT_SLOTS`].
    pub width: usize,
    /// What an integer slot's contents are measured from.
    ///
    /// Zero unless the page carries a base per column; see
    /// [`LEAF_WIDE_DIRECTORY`].
    pub base: i64,
    /// Two bits per row.
    pub class: &'p [u8],
    /// `rows * width` bytes.
    pub values: &'p [u8],
    /// How many rows the column holds.
    pub rows: usize,
    /// The whole page, because text and blob slots address it absolutely.
    page: &'p [u8],
    /// The out-of-line values, when a caller has read them.
    extents: Option<&'p Extents>,
    /// Which column this is, so an out-of-line value can be found by position.
    index: usize,
}

impl<'p> MiniColumn<'p> {
    /// Returns the class of one row's value.
    ///
    /// @param row - the row's position in the sorted region
    pub fn class_at(&self, row: usize) -> DbResult<ValueClass> {
        let byte = self
            .class
            .get(row / 4)
            .copied()
            .ok_or_else(|| corrupt(format!("row {row} is outside the class array")))?;
        ValueClass::from_code((byte >> ((row % 4) * 2)) & 3)
    }

    /// Reports whether every row's value is present and of the column's type.
    ///
    /// One pass over the class array, which is a thirty-second of the value
    /// array. A `true` answer licenses the caller to read the value slots as a
    /// contiguous run with no per-row branch, which is the vectorised fast
    /// path; a `false` answer costs a scan of 1/32nd of the data to find out.
    pub fn all_typed(&self) -> bool {
        if self.rows == 0 {
            return true;
        }
        // The directory says so, because the builder walked these values once
        // and wrote down what it found. A page whose bit is clear falls through
        // to the walk below, which is the right answer either way and only
        // slower - so nothing has to have been written by this version of the
        // builder for this to be correct.
        if self.flags & COLUMN_ALL_TYPED != 0 {
            return true;
        }
        let full_words = self.rows / 32;
        let mut words = self.class.chunks_exact(8);
        for _ in 0..full_words {
            match words.next() {
                Some(word) => {
                    let raw = u64::from_le_bytes(word.try_into().unwrap_or([0; 8]));
                    if raw != 0x5555_5555_5555_5555 {
                        return false;
                    }
                }
                // Unreachable: `column` builds `class` as exactly
                // `class_bytes(rows)`, which is `ceil(rows * 2 / 64) * 8` and
                // therefore never shorter than the `rows / 32` words this loop
                // asks for. A page whose class array does not fit fails in
                // `column` before it gets here.
                None => {
                    debug_assert!(false, "a class array shorter than its own row count");
                    return false;
                }
            }
        }
        // The tail: only the rows that exist are checked, because the padding
        // bits of the last word are zero and would read as NULL.
        let mut row = full_words.saturating_mul(32);
        while row < self.rows {
            if !matches!(self.class_at(row), Ok(ValueClass::Typed)) {
                return false;
            }
            row = row.saturating_add(1);
        }
        true
    }

    /// Reports whether any row's value is an exception.
    ///
    /// @return an error only if a class array holds the reserved code
    pub fn any_exception(&self) -> DbResult<bool> {
        for row in 0..self.rows {
            if self.class_at(row)? == ValueClass::Exception {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Returns the raw 8-byte slots, for the vectorised paths.
    ///
    /// Only meaningful for an inline type; a caller that has checked
    /// [`MiniColumn::all_typed`] and the physical type can walk this with
    /// `chunks_exact(8)` and pay nothing per row.
    pub fn inline_bytes(&self) -> &'p [u8] {
        self.values
    }

    /// Returns the whole page the column lives in.
    ///
    /// A variable-width slot is an absolute `(offset, length)` into the page,
    /// so a reader that wants to resolve one without going back through
    /// [`MiniColumn::value`] needs the page as well as the slots.
    pub fn page_bytes(&self) -> &'p [u8] {
        self.page
    }

    /// Returns the integer in one slot, without consulting the class array.
    ///
    /// @param row - the row's position in the sorted region
    pub fn int_unchecked(&self, row: usize) -> DbResult<i64> {
        let at = row.saturating_mul(self.width);
        let slice = self
            .values
            .get(at..at.saturating_add(self.width))
            .ok_or_else(|| corrupt(format!("row {row} is outside the value array")))?;
        Ok(from_frame(self.base, self.width, slice))
    }

    /// Returns one row's value, consulting the class array.
    ///
    /// @param row - the row's position in the sorted region
    pub fn value(&self, row: usize) -> DbResult<Datum<'p>> {
        // **The class array is not read when the directory already answered.**
        // `COLUMN_ALL_TYPED` is set by the builder only when every one of this
        // column's rows classified as `Typed`, and `update_slot` refuses any
        // in-place write that would leave the bit stale - so a set bit is a
        // proof rather than a hint, and consulting the class array after
        // reading it is a second cache line spent on an answer already in hand.
        //
        // It is a cache line rather than an instruction. The class array sits
        // ahead of the value array in a different part of the page, so a probe
        // that read one value touched the class line, the slot line and, for a
        // text, the heap line. Measured on the medium fixture's rowid probe,
        // reading `label` cost 59.9 ns of a 170.7 ns lookup; this removes one
        // of its three lines.
        if self.flags & COLUMN_ALL_TYPED != 0 {
            return self.typed_value(row);
        }
        match self.class_at(row)? {
            ValueClass::Null => Ok(Datum::Null),
            // Answered from the resolved values when the caller read them, and
            // **refused rather than answered with the reference** when it did
            // not. The sixteen bytes in the heap are a page number and a length,
            // not the value, and handing them back as a blob would be a wrong
            // answer that looked like a right one. `PagedTree::read_extents` is
            // what a caller reads them with; it has the pool and this does not.
            ValueClass::Extent => match self.extents.and_then(|held| held.get(row, self.index)) {
                Some(bytes) if self.physical == PhysicalType::Blob => Ok(Datum::Blob(bytes)),
                Some(bytes) => Ok(Datum::Text(bytes)),
                None => Err(misuse(concat!(
                    "this value is stored out of line; read the leaf's extents ",
                    "through the tree first"
                ))),
            },
            ValueClass::Exception => {
                let offset = self.slot_u32(row)?;
                let (value, _) = Datum::decode_tagged(self.page.get(offset..).unwrap_or(&[]))?;
                Ok(value)
            }
            ValueClass::Typed => self.typed_value(row),
        }
    }

    /// Returns one row's value, its class already known to be
    /// [`ValueClass::Typed`].
    ///
    /// The `Typed` arm of [`MiniColumn::value`], factored out so that the fast
    /// path which skipped the class array and the general path which read it
    /// cannot come to decode a slot two different ways.
    ///
    /// @param row - the row's position in the sorted region
    fn typed_value(&self, row: usize) -> DbResult<Datum<'p>> {
        match self.physical {
            PhysicalType::Int64 => Ok(Datum::Int(self.int_unchecked(row)?)),
            PhysicalType::Float64 => {
                let at = row.saturating_mul(self.width);
                let slice = self
                    .values
                    .get(at..at.saturating_add(self.width))
                    .ok_or_else(|| corrupt(format!("row {row} is outside the value array")))?;
                // A base is never applied to a double: the slot is a bit
                // pattern, and adding to one produces a different number.
                Ok(Datum::Real(f64::from_bits(read_int_slot(slice) as u64)))
            }
            PhysicalType::Text | PhysicalType::Blob => {
                let bytes = self.heap_slice(row)?;
                Ok(if self.physical == PhysicalType::Text {
                    Datum::Text(bytes)
                } else {
                    Datum::Blob(bytes)
                })
            }
            PhysicalType::Any => {
                let offset = self.slot_u32(row)?;
                let (value, _) = Datum::decode_tagged(self.page.get(offset..).unwrap_or(&[]))?;
                Ok(value)
            }
        }
    }

    /// Reports whether any of this column's values is stored out of line.
    pub fn any_extent(&self) -> DbResult<bool> {
        for row in 0..self.rows {
            if self.class_at(row)? == ValueClass::Extent {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Returns the extent reference one out-of-line value names.
    ///
    /// @param row - the row's position in the sorted region
    pub fn extent(&self, row: usize) -> DbResult<ExtentRef> {
        if self.class_at(row)? != ValueClass::Extent {
            return Err(misuse("that value is not stored out of line"));
        }
        let offset = self.slot_u32(row)?;
        let raw = self
            .page
            .get(offset..offset.saturating_add(EXTENT_REF_BYTES))
            .ok_or_else(|| corrupt("an extent reference runs past the page"))?;
        ExtentRef::decode(raw)
    }

    /// Reports whether this column's heap references are `(u16, u16)` pairs.
    ///
    /// Four bytes rather than eight, which a page of 64 KiB or less always
    /// admits. It changes how a *single* offset is read as well as a pair - an
    /// exception's slot holds one - so it is asked wherever a slot is decoded.
    fn narrow_pair(&self) -> bool {
        matches!(self.physical, PhysicalType::Text | PhysicalType::Blob) && self.width == 4
    }

    /// Returns the `(offset, length)` heap slice one variable-width slot names.
    ///
    /// @param row - the row's position in the sorted region
    pub fn heap_slice(&self, row: usize) -> DbResult<&'p [u8]> {
        let at = row.saturating_mul(self.width);
        let slice = self
            .values
            .get(at..at.saturating_add(self.width))
            .ok_or_else(|| corrupt(format!("row {row} is outside the value array")))?;
        let (offset, length) = read_heap_slot(slice);
        self.page
            .get(offset..offset.saturating_add(length))
            .ok_or_else(|| corrupt(format!("row {row}'s heap slice runs past the page")))
    }

    /// Returns the heap offset one slot names, for an exception or an extent.
    ///
    /// @param row - the row's position in the sorted region
    fn slot_u32(&self, row: usize) -> DbResult<usize> {
        let narrow = self.narrow_pair();
        let wanted = if narrow { 2 } else { 4 };
        let at = row.saturating_mul(self.width);
        let slice = self
            .values
            .get(at..at.saturating_add(wanted))
            .ok_or_else(|| corrupt(format!("row {row} is outside the value array")))?;
        Ok(read_slot_offset(slice, narrow))
    }
}

/// Builds one leaf page from a set of rows.
///
/// This is the only writer of the layout above: bulk build, compaction and
/// split all go through it, so there is one encoder to prove correct rather
/// than three. It fills the sorted region and leaves the delta area empty, so
/// every leaf it produces is [`LeafRef::is_clean`].
pub struct LeafBuilder {
    page_size: usize,
    tree: u64,
    columns: Vec<ColumnSpec>,
    key_columns: usize,
}

/// The out-of-line values one leaf holds, read into memory.
///
/// **This is what lets a leaf answer for a value that is not in it.** A
/// `LeafRef` hands back `Datum<'p>` borrowed from the page, and an extent's
/// bytes are on other pages - so the only way the ordinary accessor can return
/// one is for the bytes to already be somewhere that outlives the borrow. That
/// somewhere is this: the caller reads the extents once through the pool, hands
/// the result to [`LeafRef::with_extents`], and every accessor then works
/// exactly as it does for an inline value.
///
/// One read per leaf rather than one per access, which matters because a leaf
/// with extents is scanned column by column and a naive resolver would re-read
/// the same value once per column pass.
#[derive(Debug, Default)]
pub struct Extents {
    /// `(row, column, bytes)`, in the order the leaf holds them.
    values: Vec<(usize, usize, Vec<u8>)>,
    /// The same, for rows in the delta area, keyed by delta index.
    ///
    /// **A delta index is not a row number**, so the two cannot share a table:
    /// delta row 0 and sorted row 0 are different rows, and a lookup that
    /// confused them would answer one row's value for another's.
    delta: Vec<(usize, usize, Vec<u8>)>,
}

impl Extents {
    /// Records one resolved value.
    ///
    /// @param row - the row's position in the sorted region
    /// @param column - which column
    /// @param bytes - the value
    pub fn push(&mut self, row: usize, column: usize, bytes: Vec<u8>) {
        self.values.push((row, column, bytes));
    }

    /// Returns one resolved value, when it was read.
    ///
    /// @param row - the row's position in the sorted region
    /// @param column - which column
    pub fn get(&self, row: usize, column: usize) -> Option<&[u8]> {
        self.values
            .iter()
            .find(|(held_row, held_column, _)| *held_row == row && *held_column == column)
            .map(|(_, _, bytes)| bytes.as_slice())
    }

    /// Records one resolved value of a delta row.
    ///
    /// @param index - the row's position in the delta area
    /// @param column - which column
    /// @param bytes - the value
    pub fn push_delta(&mut self, index: usize, column: usize, bytes: Vec<u8>) {
        self.delta.push((index, column, bytes));
    }

    /// Returns one resolved delta value, when it was read.
    ///
    /// @param index - the row's position in the delta area
    /// @param column - which column
    pub fn get_delta(&self, index: usize, column: usize) -> Option<&[u8]> {
        self.delta
            .iter()
            .find(|(held, held_column, _)| *held == index && *held_column == column)
            .map(|(_, _, bytes)| bytes.as_slice())
    }

    /// Reports whether anything was read.
    pub fn is_empty(&self) -> bool {
        self.values.is_empty() && self.delta.is_empty()
    }
}

/// Writes the tagged form a delta row holds for an out-of-line value.
///
/// @param out - the buffer the row is being encoded into
/// @param reference - where the value was written
pub fn encode_extent_tagged(out: &mut Vec<u8>, reference: ExtentRef) {
    out.push(crate::datum::tag::EXTENT);
    out.extend_from_slice(&reference.encode());
}

/// The rows a leaf builder packs, read one value at a time.
///
/// **A value accessor, not a row iterator, and not a slice.** The builder is
/// column-major - it writes every value of one mini-column, then the next - so
/// a source that handed back whole rows would have to rebuild each row once per
/// column. And a slice is what this exists to avoid: `CREATE INDEX` holds its
/// entries in an arena, and it used to materialise a flat
/// `Vec<Datum>` in key order plus a `Vec<&[Datum]>` of slices into it purely so
/// that a `&[R]` could be passed - 6.4 MiB of copies at a hundred thousand rows,
/// of a statement whose whole resident cost was 28.9 MiB.
///
/// A source is indexed in **its own** order, which for an index build is key
/// order and not scan order. Out-of-range indices answer `Datum::Null` rather
/// than panicking, exactly as the slice form did.
pub trait Rows<'d> {
    /// How many rows there are.
    fn len(&self) -> usize;

    /// Reports whether there are none, which clippy asks for beside `len`.
    fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Returns one value.
    ///
    /// @param row - which row, in this source's own order
    /// @param column - which column of it
    fn value(&self, row: usize, column: usize) -> Datum<'d>;
}

/// A slice of already-materialised rows, as a [`Rows`].
///
/// The trivial implementation, so every caller that already holds owned rows -
/// the compaction path, the fixture import, the tests - is unchanged.
pub struct RowSlice<'r, R>(pub &'r [R]);

impl<'d, R: AsRef<[Datum<'d>]>> Rows<'d> for RowSlice<'_, R> {
    fn len(&self) -> usize {
        self.0.len()
    }

    fn value(&self, row: usize, column: usize) -> Datum<'d> {
        self.0
            .get(row)
            .and_then(|values| values.as_ref().get(column).copied())
            .unwrap_or(Datum::Null)
    }
}

/// Where a value too large for a leaf is written.
///
/// The builder decides *that* a value goes out of line - it is the only thing
/// that knows the page size and the layout - and this decides *where*. The two
/// are separate because the builder has no file: it is handed rows and hands
/// back a page image, and allocating a run of pages and describing it in the log
/// is the caller's business.
pub trait Spill {
    /// Writes a value out of line and returns the reference the leaf stores.
    ///
    /// **The position is passed because a repack usually has nothing to write.**
    /// A compaction, a split or a merge repacks rows that are already in the
    /// tree, and a value that was out of line before is out of line in the same
    /// run afterwards - so the spiller answers with the reference it already has
    /// and no bytes move. Without the position it could not tell that case from
    /// a value arriving for the first time, and every repack of a leaf holding
    /// three hundred out-of-line values would read and rewrite all of them to
    /// change one.
    ///
    /// @param row - the row's position among the rows being packed
    /// @param column - which column
    /// @param value - the bytes to store
    fn spill(&mut self, row: usize, column: usize, value: &[u8]) -> DbResult<ExtentRef>;
}

/// What a build produced, or why the rows would not fit.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Packed {
    /// The page, and how many of the offered rows it holds.
    Filled {
        /// The encoded page.
        page: Vec<u8>,
        /// How many of the offered rows it holds.
        rows: usize,
    },
    /// Not even one row fits.
    ///
    /// With a spiller this means a row whose *inline* part alone is larger than
    /// the page - every oversized text and blob has already gone out of line, so
    /// what is left is keys, fixed-width slots and sixteen bytes per reference.
    /// Without one it is the older answer: a value too large to keep in a leaf
    /// and nowhere to put it.
    RowTooLarge,
}

impl LeafBuilder {
    /// Returns a builder for one tree's leaves.
    ///
    /// @param page_size - the database's page size in bytes
    /// @param tree - the tree the leaves belong to
    /// @param columns - the column directory, key columns first
    /// @param key_columns - how many leading columns form the key
    pub fn new(
        page_size: usize,
        tree: u64,
        columns: Vec<ColumnSpec>,
        key_columns: usize,
    ) -> DbResult<LeafBuilder> {
        if columns.is_empty() {
            return Err(misuse("a leaf needs at least one column"));
        }
        if key_columns == 0 || key_columns > columns.len() {
            return Err(misuse("key_columns must name a prefix of the columns"));
        }
        if page_size < leaf_header::DIRECTORY {
            return Err(misuse("page size is smaller than a leaf header"));
        }
        Ok(LeafBuilder {
            page_size,
            tree,
            columns,
            key_columns,
        })
    }

    /// Packs as many of `rows` as fit into one page, up to `fill` of it.
    ///
    /// Rows must already be sorted by the key columns; the builder does not
    /// sort, because every caller either has sorted input (bulk build) or has
    /// just sorted it (compaction), and sorting twice is the kind of cost that
    /// does not show up in a profile as one line.
    ///
    /// @param rows - the rows to pack, sorted by key
    /// @param fill - the fraction of the page to fill, 0.0..=1.0
    pub fn pack<'d, R: AsRef<[Datum<'d>]>>(&self, rows: &[R], fill: f64) -> DbResult<Packed> {
        self.pack_with(rows, fill, None)
    }

    /// Packs **every** row into one page at `fill`, or answers `None`.
    ///
    /// The difference from [`LeafBuilder::pack`] is what happens when they do
    /// not all fit: this encodes nothing. A caller walking a ladder of fills
    /// asks this at each rung, so a rung that fails costs one sizing pass
    /// rather than a whole page image that is then thrown away - which is what
    /// `make_room` was doing on every compaction of a leaf packed above
    /// `COMPACT_FILL`.
    ///
    /// @param rows - the rows to pack, sorted by key
    /// @param fill - the fraction of the page to fill, 0.0..=1.0
    pub fn pack_all<'d, R: AsRef<[Datum<'d>]>>(
        &self,
        rows: &[R],
        fill: f64,
    ) -> DbResult<Option<Vec<u8>>> {
        self.pack_all_rows(&RowSlice(rows), fill)
    }

    /// The row-source form of [`LeafBuilder::pack_all`].
    ///
    /// A compaction reads its rows straight out of the page it is repacking, so
    /// it has a [`Rows`] rather than a slice and never builds one.
    ///
    /// @param rows - the rows to pack, sorted by key
    /// @param fill - the fraction of the page to fill, 0.0..=1.0
    pub fn pack_all_rows<'d>(&self, rows: &dyn Rows<'d>, fill: f64) -> DbResult<Option<Vec<u8>>> {
        let (placed, layout) = self.fit_widths(rows, 0, fill, false);
        if placed != rows.len() {
            return Ok(None);
        }
        Ok(Some(self.encode_rows_with(
            rows,
            0,
            placed,
            None,
            Some(&layout),
        )?))
    }

    /// Returns how many rows from `at` would fit in one page, without encoding.
    ///
    /// **The sizing half of [`LeafBuilder::pack_rows`], on its own.** A bulk
    /// build has to allocate its leaves as one contiguous run, so it has to know
    /// how many leaves there will be before it writes the first one - and the
    /// only way to find out used to be to pack every leaf into a
    /// `Vec<Vec<u8>>` and count them, which is a whole copy of the tree held in
    /// memory for the sake of one integer. A `CREATE INDEX` over a hundred
    /// thousand rows spent 6.2 MiB that way.
    ///
    /// The count is exact rather than an estimate: it is the same forward pass
    /// [`LeafBuilder::pack_rows`] runs, over the same values, with the same
    /// spill threshold. It does not spill - it only *prices* a spill, which is
    /// what makes running it twice safe.
    ///
    /// @param rows - the rows, sorted by key
    /// @param at - the first row to place
    /// @param fill - the fraction of the page to fill, 0.0..=1.0
    /// @param spilling - whether the real pass will have a spiller
    pub fn fit<'d>(&self, rows: &dyn Rows<'d>, at: usize, fill: f64, spilling: bool) -> usize {
        self.fit_widths(rows, at, fill, spilling).0
    }

    /// Returns both halves of the sizing pass: how many rows fit, and the slot
    /// widths those rows force.
    ///
    /// **The widths are handed back rather than recomputed** because
    /// [`LeafBuilder::encode_rows`] would otherwise derive them a second time,
    /// over the same values, by the same rule - a whole extra pass over the
    /// rows of every leaf a compaction or a bulk build writes.
    ///
    /// @param rows - the rows, sorted by key
    /// @param at - the first row to place
    /// @param fill - the fraction of the page to fill, 0.0..=1.0
    /// @param spilling - whether the real pass will have a spiller
    pub fn fit_widths<'d>(
        &self,
        rows: &dyn Rows<'d>,
        at: usize,
        fill: f64,
        spilling: bool,
    ) -> (usize, Layout) {
        let budget = ((self.page_size as f64) * fill.clamp(0.05, 1.0)) as usize;
        let mut heap = 0usize;
        let mut placed = 0usize;
        let total = rows.len();
        let mut row = at;
        // **A column's shape widens as rows are added, and never narrows.** A
        // value outside the span makes the whole column wider, which raises the
        // price of the rows already placed - so the size is recomputed against
        // the widened column rather than accumulated. The loop is still one
        // forward pass and still exact.
        let mut shapes: Vec<Shape> = vec![Shape::new(); self.columns.len()];
        let mut wanted: Vec<Shape> = shapes.clone();
        let mut layout = self.resolve(&shapes);
        while row < total {
            let mut row_heap = 0usize;
            wanted.copy_from_slice(&shapes);
            for (index, column) in self.columns.iter().enumerate() {
                let value = rows.value(row, index);
                // **One classification per value, not two.** The heap cost and
                // the column's shape are both functions of the class, and
                // asking for them separately classified every value of every
                // column twice - on the path a compaction and a bulk build
                // both take.
                let class = classify_at(column.physical, &value, self.threshold(index, spilling));
                row_heap = row_heap.saturating_add(heap_cost_of(column.physical, &value, class));
                if let Some(shape) = wanted.get_mut(index) {
                    shape.observe(column.physical, &value, class, self.page_size);
                }
            }
            let next = placed.saturating_add(1);
            let candidate = self.resolve(&wanted);
            let size = self
                .fixed_size_with(next, &candidate.widths, candidate.has_bases())
                .saturating_add(heap.saturating_add(row_heap));
            if size > budget {
                break;
            }
            shapes.copy_from_slice(&wanted);
            layout = candidate;
            heap = heap.saturating_add(row_heap);
            placed = next;
            row = row.saturating_add(1);
        }
        (placed, layout)
    }

    /// Packs as many rows from `at` as fit, sending oversized values out of line.
    ///
    /// The row-source form of [`LeafBuilder::pack_with`]: identical arithmetic,
    /// reading its values through [`Rows`] instead of out of a slice, so a
    /// caller whose rows are in an arena never has to build the slice.
    ///
    /// @param rows - the rows, sorted by key
    /// @param at - the first row to place
    /// @param fill - the fraction of the page to fill, 0.0..=1.0
    /// @param spill - where an oversized value goes, when there is somewhere
    pub fn pack_rows<'d>(
        &self,
        rows: &dyn Rows<'d>,
        at: usize,
        fill: f64,
        spill: Option<&mut dyn Spill>,
    ) -> DbResult<Packed> {
        let (placed, layout) = self.fit_widths(rows, at, fill, spill.is_some());
        if placed == 0 {
            return Ok(Packed::RowTooLarge);
        }
        let page = self.encode_rows_with(rows, at, placed, spill, Some(&layout))?;
        Ok(Packed::Filled { page, rows: placed })
    }

    /// Packs as many of `rows` as fit, sending oversized values out of line.
    ///
    /// With `None` for the spiller nothing goes out of line and this is
    /// [`LeafBuilder::pack`] exactly - which is what the import wants, because
    /// it builds into a file nothing has read and measures the same bytes twice.
    ///
    /// @param rows - the rows to pack, sorted by key
    /// @param fill - the fraction of the page to fill, 0.0..=1.0
    /// @param spill - where an oversized value goes, when there is somewhere
    ///
    /// **Generic over the row's container, and that is the whole point.** A
    /// `Vec<Datum>` already implements `AsRef<[Datum]>`, so every caller that
    /// holds owned rows (the compaction path in `write.rs`, the fixture
    /// import) compiles unchanged; and a caller that has its rows in an arena passes
    /// `&[&[Datum]]` and copies nothing. `CREATE INDEX` used to materialise a
    /// `Vec<Vec<Datum>>` of the whole input purely to call this, which was
    /// 4.2 ms of a 48 ms statement at a hundred thousand rows.
    pub fn pack_with<'d, R: AsRef<[Datum<'d>]>>(
        &self,
        rows: &[R],
        fill: f64,
        spill: Option<&mut dyn Spill>,
    ) -> DbResult<Packed> {
        // **One forward pass over the rows it places, not a binary search over
        // the rows it does not.** The pass itself is [`LeafBuilder::fit`] now;
        // the reasoning it was written with is kept here because this is the
        // signature everything but the bulk builder still calls.
        //
        // The size of a prefix is a closed form in the count plus a prefix sum
        // over the rows' heap costs, so a running total answers "does the next
        // row still fit" in the cost of that one row. The version this replaces
        // bisected `0..rows.len()` and re-measured a whole prefix per probe -
        // correct, and quadratic in the wrong argument: a bulk build hands the
        // *entire remaining input* to every pack, so filling the first leaf of
        // a hundred thousand rows measured about fifty thousand rows seventeen
        // times to place a hundred and forty-five.
        //
        // It cost 96 ms of a 155 ms `CREATE INDEX` and it was not the first
        // guess. The first guess was the write-ahead log, which a measurement
        // with the log switched off showed costs nothing at all.
        self.pack_rows(&RowSlice(rows), 0, fill, spill)
    }

    /// Returns the longest value one column keeps in the leaf.
    ///
    /// **A key column never spills, whatever its length.** Every comparison the
    /// tree makes - the binary search inside a leaf, the separator an interior
    /// page holds, the order a bulk build relies on - reads key columns out of
    /// the page, and a key whose bytes were on another page would turn each of
    /// those into a page fetch. A long key is a slow tree; a long key out of
    /// line would be a tree that cannot be searched without the pool.
    ///
    /// @param column - which column
    /// @param spilling - whether the caller gave a spiller
    fn threshold(&self, column: usize, spilling: bool) -> usize {
        if !spilling || column < self.key_columns {
            return usize::MAX;
        }
        self.page_size / EXTENT_DIVISOR
    }

    /// Returns the bytes a leaf of `count` rows spends before its heap.
    ///
    /// The header, the directory, and each mini-column's class array and inline
    /// slots. It depends on the row count and not on the values, which is what
    /// makes the size of a prefix a closed form plus a prefix sum - and that is
    /// what lets [`LeafBuilder::pack`] answer "does one more row fit" in the
    /// cost of one row.
    ///
    /// @param count - how many rows
    ///
    /// **Unreachable since the narrow-slot pricing moved onto the width
    /// arrays.** It is kept rather than deleted because removing it is a
    /// separate decision; it is flagged for removal elsewhere so a person
    /// decides.
    #[allow(dead_code)]
    fn fixed_size(&self, count: usize) -> usize {
        let widths: Vec<usize> = self
            .columns
            .iter()
            .map(|column| column.physical.slot_width())
            .collect();
        self.fixed_size_with(count, &widths, false)
    }

    /// Returns the bytes a leaf of `count` rows spends before its heap, at the
    /// slot widths given.
    ///
    /// The widths are a parameter rather than a property of the column
    /// directory because they are a property of the *values*: see
    /// [`NARROW_INT_SLOTS`]. Every caller that prices a page and the one that
    /// writes it derive them from the same rows by the same rule, which is what
    /// keeps `fit` and `encode_rows` in agreement.
    ///
    /// @param count - how many rows
    /// @param widths - the slot width of each column, in directory order
    /// @param wide_directory - whether the entries carry a base each
    fn fixed_size_with(&self, count: usize, widths: &[usize], wide_directory: bool) -> usize {
        let entry = if wide_directory {
            DIRECTORY_ENTRY_WIDE
        } else {
            DIRECTORY_ENTRY
        };
        let mut fixed =
            leaf_header::DIRECTORY.saturating_add(self.columns.len().saturating_mul(entry));
        for (index, column) in self.columns.iter().enumerate() {
            let width = widths
                .get(index)
                .copied()
                .unwrap_or_else(|| column.physical.slot_width());
            fixed = fixed
                .saturating_add(class_bytes(count))
                .saturating_add(count.saturating_mul(width));
            // Every mini-column starts 8-byte aligned; the class array is
            // already a multiple of eight and so is an inline value array, but
            // an `Any` array of an odd row count is not.
            fixed = align8(fixed);
        }
        fixed
    }

    /// Returns the layout a run of rows forces on this builder's columns.
    ///
    /// One pass over the values, widening each column's shape. It is the same
    /// rule [`LeafBuilder::fit_widths`] applies incrementally, run over a row
    /// count that has already been decided - so the page `encode_rows` lays out
    /// is the page `fit` priced.
    ///
    /// @param rows - the rows, sorted by key
    /// @param at - the first row
    /// @param count - how many rows
    /// @param spilling - whether the real pass will have a spiller
    fn layout_over<'d>(
        &self,
        rows: &dyn Rows<'d>,
        at: usize,
        count: usize,
        spilling: bool,
    ) -> Layout {
        let mut shapes: Vec<Shape> = vec![Shape::new(); self.columns.len()];
        for row in 0..count {
            for (index, column) in self.columns.iter().enumerate() {
                let Some(shape) = shapes.get_mut(index) else {
                    continue;
                };
                let value = rows.value(at.saturating_add(row), index);
                let class = classify_at(column.physical, &value, self.threshold(index, spilling));
                shape.observe(column.physical, &value, class, self.page_size);
            }
        }
        self.resolve(&shapes)
    }

    /// Turns a set of column shapes into the layout they resolve to.
    ///
    /// @param shapes - one shape per column
    fn resolve(&self, shapes: &[Shape]) -> Layout {
        let mut widths = Vec::with_capacity(self.columns.len());
        let mut bases = Vec::with_capacity(self.columns.len());
        for (index, column) in self.columns.iter().enumerate() {
            let shape = shapes.get(index).copied().unwrap_or_else(Shape::new);
            let (width, base) = shape.resolve(column.physical);
            widths.push(width);
            bases.push(base);
        }
        Layout { widths, bases }
    }

    /// Encodes the rows into a page.
    ///
    /// @param rows - the rows to encode, sorted by key
    pub fn encode<'d, R: AsRef<[Datum<'d>]>>(&self, rows: &[R]) -> DbResult<Vec<u8>> {
        self.encode_with(rows, None)
    }

    /// Encodes a leaf holding no rows.
    ///
    /// Named rather than written as `encode(&[])`, because a generic `encode`
    /// cannot infer the row container from an empty slice and the turbofish it
    /// would otherwise need at each call site says nothing to a reader.
    pub fn encode_empty(&self) -> DbResult<Vec<u8>> {
        self.encode_with::<&[Datum<'_>]>(&[], None)
    }

    /// Encodes the rows into a page, sending oversized values out of line.
    ///
    /// @param rows - the rows to encode, sorted by key
    /// @param spill - where an oversized value goes, when there is somewhere
    pub fn encode_with<'d, R: AsRef<[Datum<'d>]>>(
        &self,
        rows: &[R],
        spill: Option<&mut dyn Spill>,
    ) -> DbResult<Vec<u8>> {
        self.encode_rows(&RowSlice(rows), 0, rows.len(), spill)
    }

    /// Encodes `count` rows from `at` into one leaf page.
    ///
    /// The row-source form of [`LeafBuilder::encode_with`]. It reads
    /// column-major - every value of one column, then the next - which is why
    /// [`Rows`] is a value accessor rather than a row iterator: a row-at-a-time
    /// source would have to rebuild each row once per column.
    ///
    /// @param rows - the rows, sorted by key
    /// @param at - the first row to encode
    /// @param count - how many to encode
    /// @param spill - where an oversized value goes, when there is somewhere
    pub fn encode_rows<'d>(
        &self,
        rows: &dyn Rows<'d>,
        at: usize,
        count: usize,
        spill: Option<&mut dyn Spill>,
    ) -> DbResult<Vec<u8>> {
        self.encode_rows_with(rows, at, count, spill, None)
    }

    /// Encodes `count` rows from `at`, at slot widths the caller may already
    /// have.
    ///
    /// [`LeafBuilder::fit_widths`] derives the widths as it prices the page, so
    /// a caller that has just called it passes them here rather than paying for
    /// a second pass over the same values.
    ///
    /// @param rows - the rows, sorted by key
    /// @param at - the first row to encode
    /// @param count - how many to encode
    /// @param spill - where an oversized value goes, when there is somewhere
    /// @param layout - the widths and bases, when the caller already derived them
    pub fn encode_rows_with<'d>(
        &self,
        rows: &dyn Rows<'d>,
        at: usize,
        count: usize,
        mut spill: Option<&mut dyn Spill>,
        layout: Option<&Layout>,
    ) -> DbResult<Vec<u8>> {
        if count > u16::MAX as usize {
            return Err(misuse("a leaf cannot hold more than 65535 rows"));
        }
        let mut page = vec![0u8; self.page_size];
        page::write_common(&mut page, PageKind::Leaf, 0, self.tree)?;
        // **One pass for the widths, then the layout** - unless the caller
        // already made that pass. `fit_widths` widened its columns over exactly
        // these rows by exactly this rule, so the page it priced is the page
        // laid out here either way.
        let derived;
        let layout: &Layout = match layout {
            Some(held) => held,
            None => {
                derived = self.layout_over(rows, at, count, spill.is_some());
                &derived
            }
        };
        let wide_directory = layout.has_bases();

        // Lay the mini-columns out first so the directory can name them.
        //
        // `layout` rather than `at`: `at` is this function's first-row argument,
        // and a layout cursor called the same thing shadowed it silently - which
        // would have read every value out of the wrong row.
        let mut offsets = Vec::with_capacity(self.columns.len());
        let entry_size = if wide_directory {
            DIRECTORY_ENTRY_WIDE
        } else {
            DIRECTORY_ENTRY
        };
        let mut cursor = align8(
            leaf_header::DIRECTORY.saturating_add(self.columns.len().saturating_mul(entry_size)),
        );
        for (index, column) in self.columns.iter().enumerate() {
            offsets.push(cursor);
            cursor = cursor
                .saturating_add(class_bytes(count))
                .saturating_add(count.saturating_mul(layout.width(index, column)));
            cursor = align8(cursor);
        }
        if cursor > self.page_size {
            return Err(misuse("the mini-columns do not fit in one page"));
        }

        // The heap grows down from the page end. Every variable-width value and
        // every exception is appended to it as the columns are written.
        let mut heap_end = self.page_size;
        let mut has_exceptions = false;
        let mut has_extents = false;
        // Which columns turned out to hold nothing but present, correctly typed
        // values. The builder is walking every value anyway, so recording the
        // answer costs a branch and saves every later reader the walk.
        let mut all_typed: Vec<bool> = vec![true; self.columns.len()];

        for (index, column) in self.columns.iter().enumerate() {
            let base = offsets.get(index).copied().unwrap_or(0);
            let values_at = base.saturating_add(class_bytes(count));
            let threshold = self.threshold(index, spill.is_some());
            let width = layout.width(index, column);
            let frame = layout.base(index);
            for row in 0..count {
                let value = rows.value(at.saturating_add(row), index);
                let class = classify_at(column.physical, &value, threshold);
                if class == ValueClass::Exception {
                    has_exceptions = true;
                }
                if class == ValueClass::Extent {
                    has_extents = true;
                }
                if class != ValueClass::Typed {
                    if let Some(slot) = all_typed.get_mut(index) {
                        *slot = false;
                    }
                }
                set_class(&mut page, base, row, class)?;
                let slot = values_at.saturating_add(row.saturating_mul(width));
                match class {
                    ValueClass::Null => {}
                    ValueClass::Typed => match column.physical {
                        PhysicalType::Int64 => {
                            let target = page
                                .get_mut(slot..slot.saturating_add(width))
                                .ok_or_else(|| misuse("an integer slot runs past the page"))?;
                            write_frame(frame, target, value.as_int().unwrap_or(0));
                        }
                        PhysicalType::Float64 => page::write_u64(
                            &mut page,
                            slot,
                            match value {
                                Datum::Real(number) => number.to_bits(),
                                // REAL affinity converts, which is why this is
                                // a typed value rather than an exception.
                                Datum::Int(number) => (number as f64).to_bits(),
                                // Unreachable: `classify` returns `Typed` for a
                                // Float64 column only for these two classes.
                                _ => return Err(unreachable_branch("a typed Float64 slot")),
                            },
                        )?,
                        PhysicalType::Text | PhysicalType::Blob => {
                            let bytes = value.as_bytes().unwrap_or(&[]);
                            heap_end = heap_end
                                .checked_sub(bytes.len())
                                .ok_or_else(|| misuse("the heap overflowed the page"))?;
                            let target = page
                                .get_mut(heap_end..heap_end.saturating_add(bytes.len()))
                                .ok_or_else(|| misuse("the heap overflowed the page"))?;
                            target.copy_from_slice(bytes);
                            let pair = page
                                .get_mut(slot..slot.saturating_add(width))
                                .ok_or_else(|| misuse("a heap slot runs past the page"))?;
                            write_heap_slot(pair, heap_end, bytes.len());
                        }
                        PhysicalType::Any => {
                            heap_end = write_tagged(&mut page, heap_end, &value)?;
                            page::write_u32(&mut page, slot, heap_end as u32)?;
                        }
                    },
                    ValueClass::Exception => {
                        heap_end = write_tagged(&mut page, heap_end, &value)?;
                        let narrow = narrow_pair_at(column.physical, width);
                        let target = page
                            .get_mut(slot..slot.saturating_add(width))
                            .ok_or_else(|| misuse("a slot runs past the page"))?;
                        write_slot_offset(target, heap_end, narrow);
                    }
                    ValueClass::Extent => {
                        // Unreachable without a spiller: `classify_at` returns
                        // this class only when `threshold` is finite, and
                        // `threshold` is `usize::MAX` when there is none.
                        let spiller = spill
                            .as_deref_mut()
                            .ok_or_else(|| unreachable_branch("an extent with no spiller"))?;
                        let reference =
                            spiller.spill(row, index, value.as_bytes().unwrap_or(&[]))?;
                        heap_end = heap_end
                            .checked_sub(EXTENT_REF_BYTES)
                            .ok_or_else(|| misuse("the heap overflowed the page"))?;
                        let target = page
                            .get_mut(heap_end..heap_end.saturating_add(EXTENT_REF_BYTES))
                            .ok_or_else(|| misuse("the heap overflowed the page"))?;
                        target.copy_from_slice(&reference.encode());
                        let narrow = narrow_pair_at(column.physical, width);
                        let held = page
                            .get_mut(slot..slot.saturating_add(width))
                            .ok_or_else(|| misuse("a slot runs past the page"))?;
                        write_slot_offset(held, heap_end, narrow);
                    }
                }
            }
        }

        if heap_end < cursor {
            return Err(misuse("the heap collided with the mini-columns"));
        }

        // The delta area is empty and sits where the heap begins, so a later
        // insert has the whole free gap to grow into.
        let delta_start = heap_end;
        page::write_u16(&mut page, leaf_header::ROW_COUNT, count as u16)?;
        page::write_u16(&mut page, leaf_header::DELTA_COUNT, 0)?;
        page::write_u16(
            &mut page,
            leaf_header::COLUMN_COUNT,
            self.columns.len() as u16,
        )?;
        page::write_u16(&mut page, leaf_header::KEY_COLUMNS, self.key_columns as u16)?;
        page::write_u32(&mut page, leaf_header::HEAP_START, heap_end as u32)?;
        page::write_u32(&mut page, leaf_header::DELTA_START, delta_start as u32)?;
        page::write_u64(&mut page, leaf_header::MAX_CTS, 0)?;
        let low_fence = if count == 0 {
            0
        } else {
            rows.value(at, 0).as_int().unwrap_or(0)
        };
        page::write_u64(&mut page, leaf_header::LOW_FENCE, low_fence as u64)?;
        for (index, column) in self.columns.iter().enumerate() {
            let entry = leaf_header::DIRECTORY.saturating_add(index.saturating_mul(entry_size));
            let type_slot = page
                .get_mut(entry)
                .ok_or_else(|| misuse("the directory does not fit"))?;
            *type_slot = column.physical.code();
            let flag_slot = page
                .get_mut(entry.saturating_add(1))
                .ok_or_else(|| misuse("the directory does not fit"))?;
            let key_bit = if index < self.key_columns {
                column.flags | COLUMN_KEY
            } else {
                column.flags & !COLUMN_KEY
            };
            *flag_slot = if all_typed.get(index).copied().unwrap_or(false) {
                key_bit | COLUMN_ALL_TYPED
            } else {
                key_bit & !COLUMN_ALL_TYPED
            };
            page::write_u16(
                &mut page,
                entry.saturating_add(2),
                layout.width(index, column) as u16,
            )?;
            page::write_u32(
                &mut page,
                entry.saturating_add(4),
                offsets.get(index).copied().unwrap_or(0) as u32,
            )?;
            if wide_directory {
                page::write_u64(
                    &mut page,
                    entry.saturating_add(8),
                    layout.base(index) as u64,
                )?;
            }
        }
        if wide_directory {
            let flags = page
                .get_mut(header::FLAGS)
                .ok_or_else(|| misuse("the page has no flag byte"))?;
            *flags |= LEAF_WIDE_DIRECTORY;
        }
        if has_exceptions || has_extents {
            let flags = page
                .get_mut(header::FLAGS)
                .ok_or_else(|| misuse("the page has no flag byte"))?;
            if has_exceptions {
                *flags |= LEAF_HAS_EXCEPTIONS;
            }
            if has_extents {
                *flags |= LEAF_HAS_EXTENTS;
            }
        }
        Ok(page)
    }
}

/// Rounds an offset up to the next multiple of eight.
///
/// @param at - the offset to align
fn align8(at: usize) -> usize {
    at.saturating_add(7) & !7
}

/// How one leaf's columns are laid out: a slot width and a base each.
///
/// Derived from the values that leaf actually holds, by
/// [`LeafBuilder::fit_widths`] as it prices the page and by
/// [`LeafBuilder::layout_over`] when nothing has priced it yet. The two follow
/// the same rule over the same rows, which is what makes the page the sizing
/// pass measured the page the encoder writes.
#[derive(Clone, Debug, Default)]
pub struct Layout {
    /// How many bytes one slot of each column occupies.
    widths: Vec<usize>,
    /// What each column's integer slots are measured from.
    bases: Vec<i64>,
}

impl Layout {
    /// Returns one column's slot width, or the type's when it is out of range.
    ///
    /// @param index - the column's position
    /// @param column - the column's directory entry
    fn width(&self, index: usize, column: &ColumnSpec) -> usize {
        self.widths
            .get(index)
            .copied()
            .unwrap_or_else(|| column.physical.slot_width())
    }

    /// Returns one column's base.
    ///
    /// @param index - the column's position
    fn base(&self, index: usize) -> i64 {
        self.bases.get(index).copied().unwrap_or(0)
    }

    /// Reports whether any column carries a base, and so needs a wide entry.
    fn has_bases(&self) -> bool {
        self.bases.iter().any(|base| *base != 0)
    }
}

/// What the values seen so far force on one column's layout.
///
/// Kept as a **span** rather than a width because that is what a frame of
/// reference makes of it: with a base, the width comes from `high - low` and not
/// from how large the numbers are.
#[derive(Clone, Copy, Debug)]
struct Shape {
    /// The smallest and largest typed integer, when one has been seen.
    span: Option<(i64, i64)>,
    /// The widest slot a single value forces whatever the span is: four for an
    /// integer column's exception, whose slot holds a heap offset, and the heap
    /// pair's width for a text or a blob.
    floor: usize,
}

impl Shape {
    /// Returns the shape of a column nothing has been seen for.
    fn new() -> Shape {
        Shape {
            span: None,
            floor: 0,
        }
    }

    /// Widens the shape by one value.
    ///
    /// @param physical - the column's layout
    /// @param value - the value being placed
    /// @param class - the class it took
    /// @param page_size - the database's page size
    fn observe(
        &mut self,
        physical: PhysicalType,
        value: &Datum<'_>,
        class: ValueClass,
        page_size: usize,
    ) {
        match physical {
            PhysicalType::Int64 => match class {
                ValueClass::Typed => {
                    let number = value.as_int().unwrap_or(0);
                    self.span = Some(match self.span {
                        Some((low, high)) => (low.min(number), high.max(number)),
                        None => (number, number),
                    });
                }
                ValueClass::Null => {}
                // The slot holds a `u32` heap offset rather than a value.
                _ => self.floor = self.floor.max(4),
            },
            PhysicalType::Text | PhysicalType::Blob => {
                let longest = match class {
                    ValueClass::Typed => value.as_bytes().map(<[u8]>::len).unwrap_or(0),
                    _ => 0,
                };
                self.floor = self.floor.max(heap_slot_width(page_size, longest));
            }
            other => self.floor = self.floor.max(other.slot_width()),
        }
    }

    /// Returns the width and base this shape resolves to.
    ///
    /// **A base of zero means plain signed truncation**, which is what every
    /// page written before frames existed holds - so a column whose smallest
    /// value is zero takes the signed width rather than the unsigned one, and
    /// the reader needs no flag beyond the base itself to know which it is
    /// looking at.
    ///
    /// @param physical - the column's layout
    fn resolve(&self, physical: PhysicalType) -> (usize, i64) {
        if !NARROW_INT_SLOTS {
            return (physical.slot_width(), 0);
        }
        if physical != PhysicalType::Int64 {
            return (self.floor.max(narrow_floor(physical, 1)), 0);
        }
        let (width, base) = match self.span {
            None => (1, 0),
            Some((low, high)) if FRAME_OF_REFERENCE && low != 0 => (frame_width(low, high), low),
            Some((low, high)) => (int_slot_width(low).max(int_slot_width(high)), 0),
        };
        (width.max(self.floor).max(1), base)
    }
}

/// Reads one integer out of a slot measured from a base.
///
/// With no base the slot is the value, sign-extended - which is what every page
/// written before [`LEAF_WIDE_DIRECTORY`] holds. With one it is the **unsigned
/// distance** from the base, so a column whose values run 900,000..900,200 is
/// one byte a row where the values themselves need four.
///
/// @param base - the column's frame of reference
/// @param width - how many bytes the slot occupies
/// @param slot - exactly that many bytes
#[inline]
pub fn from_frame(base: i64, width: usize, slot: &[u8]) -> i64 {
    if base == 0 {
        return read_int_slot(slot);
    }
    // **A load, a zero-extend and an add, chosen by width.** The first version
    // of this went through a `[u8; 8]` and `i128`, and `read.analytical` fell
    // from 6.57x to 2.77x: a scan that adds a constant to a byte should not be
    // slower than one that does not, and it is not once the width is a
    // constant to the compiler.
    base.wrapping_add(match width {
        1 => i64::from(slot.first().copied().unwrap_or(0)),
        2 => {
            let mut raw = [0u8; 2];
            raw.copy_from_slice(slot.get(..2).unwrap_or(&[0; 2]));
            i64::from(u16::from_le_bytes(raw))
        }
        4 => {
            let mut raw = [0u8; 4];
            raw.copy_from_slice(slot.get(..4).unwrap_or(&[0; 4]));
            i64::from(u32::from_le_bytes(raw))
        }
        _ => {
            let mut raw = [0u8; 8];
            raw.copy_from_slice(slot.get(..8).unwrap_or(&[0; 8]));
            u64::from_le_bytes(raw) as i64
        }
    })
}

/// Reports whether one integer fits a slot of this width, from this base.
///
/// @param base - the column's frame of reference
/// @param width - how many bytes the slot occupies
/// @param value - the integer to store
pub fn fits_frame(base: i64, width: usize, value: i64) -> bool {
    if base == 0 {
        return int_slot_width(value) <= width;
    }
    let delta = (value as i128).saturating_sub(base as i128);
    if delta < 0 {
        return false;
    }
    match width {
        1 => delta <= i128::from(u8::MAX),
        2 => delta <= i128::from(u16::MAX),
        4 => delta <= i128::from(u32::MAX),
        _ => true,
    }
}

/// Writes one integer into a slot measured from a base.
///
/// @param base - the column's frame of reference
/// @param slot - exactly the column's slot width
/// @param value - the integer to store
pub fn write_frame(base: i64, slot: &mut [u8], value: i64) {
    if base == 0 {
        write_int_slot(slot, value);
        return;
    }
    let delta = ((value as i128) - (base as i128)) as u64;
    let raw = delta.to_le_bytes();
    let width = slot.len().min(8);
    if let (Some(target), Some(source)) = (slot.get_mut(..width), raw.get(..width)) {
        target.copy_from_slice(source);
    }
}

/// Returns the narrowest slot the values `low..=high` fit in, from a base.
///
/// The range decides it, not the magnitude: `low` becomes the base and every
/// slot holds `value - low` as an unsigned integer.
///
/// @param low - the smallest typed value in the column
/// @param high - the largest
fn frame_width(low: i64, high: i64) -> usize {
    let span = (high as i128).saturating_sub(low as i128);
    if span < 0 {
        return 8;
    }
    let span = span as u128;
    if span <= u128::from(u8::MAX) {
        1
    } else if span <= u128::from(u16::MAX) {
        2
    } else if span <= u128::from(u32::MAX) {
        4
    } else {
        8
    }
}

/// Reports whether a column laid out at this width holds `(u16, u16)` pairs.
///
/// @param physical - the column's layout
/// @param width - the width it was laid out at
fn narrow_pair_at(physical: PhysicalType, width: usize) -> bool {
    matches!(physical, PhysicalType::Text | PhysicalType::Blob) && width == 4
}

/// Returns the narrowest slot a column of this type may start out at.
///
/// Eight for every type that cannot narrow, and one for an integer column - so
/// a leaf of small integers, or of none at all, spends a byte a row.
///
/// @param physical - the column's layout
fn narrow_floor(physical: PhysicalType, page_size: usize) -> usize {
    if !NARROW_INT_SLOTS {
        return physical.slot_width();
    }
    match physical {
        PhysicalType::Int64 => 1,
        // A pair is four bytes or eight; whether four is admissible is a
        // property of the page size, so the floor already knows the answer for
        // every leaf this builder writes.
        PhysicalType::Text | PhysicalType::Blob => heap_slot_width(page_size, 0),
        other => other.slot_width(),
    }
}

/// Returns the slot width one value forces on its column.
///
/// A NULL forces nothing - its slot is never read. An **exception** forces
/// four: its slot holds a `u32` offset into the heap rather than the value, so
/// a column with one in it cannot be narrower than that however small its
/// integers are.
///
/// @param physical - the column's layout
/// @param value - the value being placed
/// @param threshold - the longest value kept in the leaf
///
/// **Unreachable since the narrow-slot pricing moved onto the width
/// arrays.** It is kept rather than deleted because removing it is a
/// separate decision; it is flagged for removal elsewhere so a person
/// decides.
#[allow(dead_code)]
fn slot_need(
    physical: PhysicalType,
    value: &Datum<'_>,
    threshold: usize,
    page_size: usize,
) -> usize {
    if !NARROW_INT_SLOTS || !physical.narrows() {
        return physical.slot_width();
    }
    slot_need_of(
        physical,
        value,
        classify_at(physical, value, threshold),
        page_size,
    )
}

/// Returns the slot width one value forces, its class already known.
///
/// @param physical - the column's layout
/// @param value - the value being placed
/// @param class - the class the value classified as
///
/// **Unreachable since the narrow-slot pricing moved onto the width
/// arrays.** It is kept rather than deleted because removing it is a
/// separate decision; it is flagged for removal elsewhere so a person
/// decides.
#[allow(dead_code)]
fn slot_need_of(
    physical: PhysicalType,
    value: &Datum<'_>,
    class: ValueClass,
    page_size: usize,
) -> usize {
    if !NARROW_INT_SLOTS || !physical.narrows() {
        return physical.slot_width();
    }
    match physical {
        PhysicalType::Int64 => match class {
            ValueClass::Null => 1,
            ValueClass::Typed => int_slot_width(value.as_int().unwrap_or(0)),
            // An exception's slot holds a heap offset rather than a value, and
            // an integer column's offset is a `u32`.
            _ => 4,
        },
        // The length is what decides it: a value longer than a `u16` needs the
        // wide pair, and so does a page larger than 64 KiB. NULLs, exceptions
        // and extents all fit the narrow pair - an exception stores one offset
        // and an extent one too, and both are page offsets.
        _ => heap_slot_width(
            page_size,
            match class {
                ValueClass::Typed => value.as_bytes().map(<[u8]>::len).unwrap_or(0),
                _ => 0,
            },
        ),
    }
}

/// Returns the heap bytes and the slot width one value costs its column.
///
/// Both are functions of the same classification, and the sizing pass wants
/// both - so it classifies once.
///
/// @param physical - the column's layout
/// @param value - the value being placed
/// @param threshold - the longest value kept in the leaf
///
/// **Unreachable since the narrow-slot pricing moved onto the width
/// arrays.** It is kept rather than deleted because removing it is a
/// separate decision; it is flagged for removal elsewhere so a person
/// decides.
#[allow(dead_code)]
fn costs_at(
    physical: PhysicalType,
    value: &Datum<'_>,
    threshold: usize,
    page_size: usize,
) -> (usize, usize) {
    let class = classify_at(physical, value, threshold);
    (
        heap_cost_of(physical, value, class),
        slot_need_of(physical, value, class, page_size),
    )
}

/// Returns the class a value takes in a column of the given physical type.
///
/// @param physical - the column's layout
/// @param value - the value to classify
/// Returns the class of one value in a column, spilling past a threshold.
///
/// A `Text` or `Blob` value longer than `threshold` is stored out of line. Only
/// those two: a `PhysicalType::Any` column's slot is a tagged value whose class
/// is in the bytes, and an extent reference carries only a page and a length -
/// so a reader would have nothing to say whether it had found text or a blob.
/// An oversized value in an `Any` column therefore stays inline, and if the row
/// then does not fit its page the builder says `RowTooLarge` as it always has.
///
/// @param physical - the column's layout
/// @param value - the value being placed
/// @param threshold - the longest value kept in the leaf
fn classify_at(physical: PhysicalType, value: &Datum<'_>, threshold: usize) -> ValueClass {
    if matches!(physical, PhysicalType::Text | PhysicalType::Blob) {
        let spillable = match (physical, value) {
            (PhysicalType::Text, Datum::Text(bytes)) => Some(bytes.len()),
            (PhysicalType::Blob, Datum::Blob(bytes)) => Some(bytes.len()),
            _ => None,
        };
        if spillable.is_some_and(|length| length > threshold) {
            return ValueClass::Extent;
        }
    }
    match (physical, value) {
        (_, Datum::Null) => ValueClass::Null,
        (PhysicalType::Any, _) => ValueClass::Typed,
        (PhysicalType::Int64, Datum::Int(_)) => ValueClass::Typed,
        (PhysicalType::Float64, Datum::Real(_)) => ValueClass::Typed,
        // An integer in a column whose affinity is REAL is *converted*, not
        // excepted. That is what REAL affinity means in the dialect - SQLite
        // stores 7 in a REAL column as 7.0 - and it is also what keeps such a
        // column on the vectorised path instead of turning every whole-numbered
        // row into a tagged value in the heap. The encoder's Float64 arm has
        // always converted; this is the classification agreeing with it, which
        // it did not before and which left that arm unreachable.
        (PhysicalType::Float64, Datum::Int(_)) => ValueClass::Typed,
        (PhysicalType::Text, Datum::Text(_)) => ValueClass::Typed,
        (PhysicalType::Blob, Datum::Blob(_)) => ValueClass::Typed,
        _ => ValueClass::Exception,
    }
}

/// Returns the heap bytes one value costs in a column of the given type.
///
/// @param physical - the column's layout
/// @param value - the value to measure
/// Returns the heap bytes one value costs, given the spill threshold.
///
/// A value that goes out of line costs the leaf sixteen bytes whatever its
/// length, which is the whole point of the extent and is why the threshold has
/// to reach the size calculation and not only the encoder.
///
/// @param physical - the column's layout
/// @param value - the value to measure
/// @param threshold - the longest value kept in the leaf
///
/// **Unreachable since the narrow-slot pricing moved onto the width
/// arrays.** It is kept rather than deleted because removing it is a
/// separate decision; it is flagged for removal elsewhere so a person
/// decides.
#[allow(dead_code)]
fn heap_cost_at(physical: PhysicalType, value: &Datum<'_>, threshold: usize) -> usize {
    heap_cost_of(physical, value, classify_at(physical, value, threshold))
}

/// Returns the heap bytes one value costs, its class already known.
///
/// @param physical - the column's layout
/// @param value - the value to measure
/// @param class - the class the value classified as
fn heap_cost_of(physical: PhysicalType, value: &Datum<'_>, class: ValueClass) -> usize {
    match class {
        ValueClass::Null => 0,
        ValueClass::Extent => EXTENT_REF_BYTES,
        ValueClass::Exception => value.tagged_len(),
        ValueClass::Typed => match physical {
            PhysicalType::Int64 | PhysicalType::Float64 => 0,
            PhysicalType::Text | PhysicalType::Blob => value.as_bytes().unwrap_or(&[]).len(),
            PhysicalType::Any => value.tagged_len(),
        },
    }
}

/// Writes one class into a class array.
///
/// @param page - the page being built
/// @param base - where the class array starts
/// @param row - the row's position
/// @param class - what to record
fn set_class(page: &mut [u8], base: usize, row: usize, class: ValueClass) -> DbResult<()> {
    let at = base.saturating_add(row / 4);
    let byte = page
        .get_mut(at)
        .ok_or_else(|| misuse("the class array does not fit"))?;
    let shift = (row % 4).saturating_mul(2);
    *byte = (*byte & !(3u8 << shift)) | (class.code() << shift);
    Ok(())
}

/// Appends a tagged value to the heap, returning the new heap start.
///
/// @param page - the page being built
/// @param heap_end - where the heap currently starts
/// @param value - the value to write
fn write_tagged(page: &mut [u8], heap_end: usize, value: &Datum<'_>) -> DbResult<usize> {
    let mut encoded = Vec::with_capacity(value.tagged_len());
    value.encode_tagged(&mut encoded);
    let start = heap_end
        .checked_sub(encoded.len())
        .ok_or_else(|| misuse("the heap overflowed the page"))?;
    let target = page
        .get_mut(start..heap_end)
        .ok_or_else(|| misuse("the heap overflowed the page"))?;
    target.copy_from_slice(&encoded);
    Ok(start)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::COLUMN_NULLABLE;

    /// `equal_run` finds the same span by every route it has.
    ///
    /// It is the hot path of every index probe - an index nested loop asks for
    /// one run per outer row - and it has three routes to the same answer: an
    /// integer fast path over the leading key column, a forward walk capped at
    /// `scan_cap`, and a bisection when the run is longer than the cap. A
    /// generic prefix that is not a single integer takes a fourth. This checks
    /// all four against a count of the rows that actually match.
    #[test]
    fn an_equal_run_is_the_rows_that_match_by_every_route() {
        /// Counts the rows whose leading column equals a value.
        ///
        /// @param rows - the rows the leaf was built from
        /// @param wanted - the value to match
        fn matching(rows: &[Vec<Datum<'_>>], wanted: &Datum<'_>) -> usize {
            rows.iter()
                .filter(|row| {
                    row.first()
                        .map(|value| value.compare(wanted) == std::cmp::Ordering::Equal)
                        .unwrap_or(false)
                })
                .count()
        }

        // An integer leading key column: runs of one, of three, and of
        // twenty - the last well past any sensible scan cap.
        let columns = vec![
            ColumnSpec::key(PhysicalType::Int64),
            ColumnSpec::key(PhysicalType::Int64),
        ];
        let mut rows: Vec<Vec<Datum<'_>>> = Vec::new();
        for (group, run) in [(10i64, 1usize), (20, 3), (30, 20), (40, 1)] {
            for nth in 0..run {
                rows.push(vec![Datum::Int(group), Datum::Int(nth as i64)]);
            }
        }
        let page = LeafBuilder::new(4096, 1, columns, 2)
            .unwrap()
            .encode(&rows)
            .unwrap();
        let leaf = LeafRef::parse(&page).unwrap();
        for (wanted, cap) in [
            (10i64, 8usize),
            (20, 8),
            (30, 8),
            (30, 64),
            (40, 8),
            (25, 8),
            (99, 8),
        ] {
            let probe = [Datum::Int(wanted)];
            let (begin, end) = leaf.equal_run(&probe, cap).unwrap();
            assert_eq!(
                end.saturating_sub(begin),
                matching(&rows, &probe[0]),
                "integer run for {wanted} at cap {cap}"
            );
            assert_eq!(
                begin,
                leaf.lower_bound(&probe).unwrap(),
                "the run starts at the lower bound for {wanted}"
            );
        }

        // A text leading key column takes the generic route, including the
        // bisection when the run is longer than the cap.
        let columns = vec![
            ColumnSpec::key(PhysicalType::Text),
            ColumnSpec::key(PhysicalType::Int64),
        ];
        let mut rows: Vec<Vec<Datum<'_>>> = Vec::new();
        for (group, run) in [(&b"aa"[..], 1usize), (b"bb", 3), (b"cc", 20)] {
            for nth in 0..run {
                rows.push(vec![Datum::Text(group), Datum::Int(nth as i64)]);
            }
        }
        let page = LeafBuilder::new(4096, 1, columns, 2)
            .unwrap()
            .encode(&rows)
            .unwrap();
        let leaf = LeafRef::parse(&page).unwrap();
        for (wanted, cap) in [
            (&b"aa"[..], 8usize),
            (b"bb", 8),
            (b"cc", 8),
            (b"cc", 64),
            (b"zz", 8),
        ] {
            let probe = [Datum::Text(wanted)];
            let (begin, end) = leaf.equal_run(&probe, cap).unwrap();
            assert_eq!(
                end.saturating_sub(begin),
                matching(&rows, &probe[0]),
                "text run for {wanted:?} at cap {cap}"
            );
        }
    }

    /// A collation that could reorder numbers turns the interpolation guide
    /// off rather than letting it guess under an order it does not know.
    ///
    /// The guard is unreachable for the collations the dialect actually has -
    /// none of them reorders an integer - so it exists to make a future one
    /// safe. A bound taken under it still has to be the right bound.
    #[test]
    fn a_non_binary_collation_declines_the_integer_guide() {
        let columns = vec![
            ColumnSpec::key(PhysicalType::Int64),
            ColumnSpec::new(PhysicalType::Int64),
        ];
        let rows: Vec<Vec<Datum<'_>>> = (0..64i64)
            .map(|n| vec![Datum::Int(n * 2), Datum::Int(n)])
            .collect();
        let page = LeafBuilder::new(4096, 1, columns, 1)
            .unwrap()
            .encode(&rows)
            .unwrap();
        let collations = [inillucent_value::collation::Collation::NoCase];
        let guided = LeafRef::parse(&page).unwrap();
        let unguided = LeafRef::parse(&page).unwrap().with_collations(&collations);
        for wanted in [-1i64, 0, 1, 62, 63, 126, 127, 1_000] {
            let probe = [Datum::Int(wanted)];
            assert_eq!(
                unguided.lower_bound(&probe).unwrap(),
                guided.lower_bound(&probe).unwrap(),
                "the lower bound for {wanted} moved with the collation"
            );
            assert_eq!(
                unguided.upper_bound(&probe).unwrap(),
                guided.upper_bound(&probe).unwrap(),
                "the upper bound for {wanted} moved with the collation"
            );
        }
    }

    /// The directory's all-typed bit says exactly what a walk of the class
    /// array says, on a column of each shape.
    ///
    /// The bit is a cache of that walk, and a cache that can disagree with what
    /// it caches is the worst kind of fast path - it is a different answer, not
    /// a faster one. So this asserts the two agree, and it asserts it on the
    /// three shapes that decide it: every value present and typed, a NULL, and
    /// a value of the wrong class for its column.
    #[test]
    fn the_all_typed_bit_agrees_with_the_class_array() {
        /// Walks the class array the way `all_typed` did before the bit
        /// existed, so the two answers can be compared.
        ///
        /// @param column - the mini-column to inspect
        fn walked(column: &MiniColumn<'_>) -> bool {
            (0..column.rows).all(|row| matches!(column.class_at(row), Ok(ValueClass::Typed)))
        }

        let columns = vec![
            ColumnSpec::key(PhysicalType::Int64),
            ColumnSpec::new(PhysicalType::Int64),
            ColumnSpec::new(PhysicalType::Text),
        ];
        let shapes: [(&str, Datum<'_>); 3] = [
            ("typed", Datum::Int(7)),
            ("null", Datum::Null),
            ("exception", Datum::Text(b"not an integer")),
        ];
        for (name, odd) in shapes {
            let mut rows: Vec<Vec<Datum<'_>>> = (0..40i64)
                .map(|n| vec![Datum::Int(n), Datum::Int(n * 3), Datum::Text(b"label")])
                .collect();
            if let Some(row) = rows.get_mut(17) {
                if let Some(slot) = row.get_mut(1) {
                    *slot = odd;
                }
            }
            let page = LeafBuilder::new(4096, 1, columns.clone(), 1)
                .unwrap()
                .encode(&rows)
                .unwrap();
            let leaf = LeafRef::parse(&page).unwrap();
            for index in 0..3 {
                let column = leaf.column(index).unwrap();
                let by_walk = walked(&column);
                assert_eq!(
                    column.all_typed(),
                    by_walk,
                    "{name}: column {index} disagrees with its class array"
                );
                assert_eq!(
                    column.flags & COLUMN_ALL_TYPED != 0,
                    by_walk,
                    "{name}: column {index}'s directory bit disagrees"
                );
            }
            // The odd value is in column 1 and only column 1.
            let typed_column = matches!(odd, Datum::Int(_));
            assert_eq!(
                leaf.column(1).unwrap().all_typed(),
                typed_column,
                "{name}: the column holding the odd value"
            );
            assert!(
                leaf.column(0).unwrap().all_typed(),
                "{name}: the key column"
            );
        }
    }

    /// The scorecard fixture's shape: rowid key, two integers, text, blob.
    fn fixture_columns() -> Vec<ColumnSpec> {
        vec![
            ColumnSpec::key(PhysicalType::Int64),
            ColumnSpec::new(PhysicalType::Int64),
            ColumnSpec::new(PhysicalType::Int64),
            ColumnSpec::new(PhysicalType::Text),
            ColumnSpec::new(PhysicalType::Blob),
        ]
    }

    fn fixture_rows(count: usize) -> Vec<Vec<Datum<'static>>> {
        (0..count)
            .map(|n| {
                vec![
                    Datum::Int(n as i64),
                    Datum::Int(((n as i64) * 7) % 1000),
                    Datum::Int((n as i64) % 64),
                    Datum::Text(b"row lorem ipsum dolor sit amet"),
                    Datum::Blob(&[0u8; 16]),
                ]
            })
            .collect()
    }

    /// A packed leaf reads back exactly what was packed, in order.
    #[test]
    fn a_packed_leaf_round_trips() {
        let builder = LeafBuilder::new(8192, 1, fixture_columns(), 1).unwrap();
        let rows = fixture_rows(40);
        let page = builder.encode(&rows).unwrap();
        let leaf = LeafRef::parse(&page).unwrap();
        assert_eq!(leaf.row_count(), 40);
        assert_eq!(leaf.column_count(), 5);
        assert_eq!(leaf.key_columns(), 1);
        assert!(leaf.is_clean());
        for (index, row) in rows.iter().enumerate() {
            for (column, wanted) in row.iter().enumerate().take(5) {
                let got = leaf.value(index, column).unwrap();
                assert_eq!(
                    got.compare(wanted),
                    std::cmp::Ordering::Equal,
                    "row {index} column {column}: {got:?} vs {wanted:?}"
                );
            }
        }
    }

    /// NULLs are recorded in the class array and the value slot is not read.
    #[test]
    fn nulls_round_trip_and_leave_the_fast_path() {
        let columns = vec![
            ColumnSpec::key(PhysicalType::Int64),
            ColumnSpec::new(PhysicalType::Int64),
        ];
        let builder = LeafBuilder::new(8192, 1, columns, 1).unwrap();
        let rows = vec![
            vec![Datum::Int(1), Datum::Int(10)],
            vec![Datum::Int(2), Datum::Null],
            vec![Datum::Int(3), Datum::Int(30)],
        ];
        let page = builder.encode(&rows).unwrap();
        let leaf = LeafRef::parse(&page).unwrap();
        assert!(leaf.value(1, 1).unwrap().is_null());
        assert!(!leaf.column(1).unwrap().all_typed());
        assert!(leaf.column(0).unwrap().all_typed());
        // A NULL is not an exception: the leaf stays on the clean path.
        assert!(!leaf.has_exceptions());
        assert!(leaf.is_clean());
    }

    /// A value of the wrong type becomes an exception, the flag is set, and the
    /// value still reads back exactly.
    #[test]
    fn exceptions_round_trip_and_set_the_flag() {
        let columns = vec![
            ColumnSpec::key(PhysicalType::Int64),
            ColumnSpec::new(PhysicalType::Int64),
        ];
        let builder = LeafBuilder::new(8192, 1, columns, 1).unwrap();
        let rows = vec![
            vec![Datum::Int(1), Datum::Int(10)],
            vec![Datum::Int(2), Datum::Text(b"not an integer")],
            vec![Datum::Int(3), Datum::Real(1.5)],
        ];
        let page = builder.encode(&rows).unwrap();
        let leaf = LeafRef::parse(&page).unwrap();
        assert!(leaf.has_exceptions());
        assert!(!leaf.is_clean());
        assert_eq!(
            leaf.value(1, 1).unwrap().as_bytes().unwrap(),
            b"not an integer"
        );
        assert_eq!(leaf.value(2, 1).unwrap().as_f64().unwrap(), 1.5);
        assert_eq!(leaf.value(0, 1).unwrap().as_int().unwrap(), 10);
    }

    /// `all_typed` agrees with the per-row class at every row count around a
    /// word boundary, which is where a tail bug would hide.
    #[test]
    fn all_typed_agrees_with_the_per_row_class() {
        let columns = vec![
            ColumnSpec::key(PhysicalType::Int64),
            ColumnSpec::new(PhysicalType::Int64),
        ];
        let builder = LeafBuilder::new(8192, 1, columns, 1).unwrap();
        for count in 1..=70usize {
            for null_at in 0..count {
                let rows: Vec<Vec<Datum<'static>>> = (0..count)
                    .map(|n| {
                        vec![
                            Datum::Int(n as i64),
                            if n == null_at {
                                Datum::Null
                            } else {
                                Datum::Int(1)
                            },
                        ]
                    })
                    .collect();
                let page = builder.encode(&rows).unwrap();
                let leaf = LeafRef::parse(&page).unwrap();
                let column = leaf.column(1).unwrap();
                assert!(
                    !column.all_typed(),
                    "count {count} with a NULL at {null_at} claimed to be all typed"
                );
                let by_row =
                    (0..count).all(|row| matches!(column.class_at(row), Ok(ValueClass::Typed)));
                assert_eq!(
                    column.all_typed(),
                    by_row,
                    "count {count} null at {null_at}"
                );
            }
            let rows: Vec<Vec<Datum<'static>>> = (0..count)
                .map(|n| vec![Datum::Int(n as i64), Datum::Int(1)])
                .collect();
            let page = builder.encode(&rows).unwrap();
            let leaf = LeafRef::parse(&page).unwrap();
            assert!(leaf.column(1).unwrap().all_typed(), "count {count}");
        }
    }

    /// Every integer round-trips at whatever width the leaf chose for it,
    /// including the values that sit exactly on a width's boundary.
    ///
    /// The widths are a *storage* choice, so the test is the same whether
    /// [`NARROW_INT_SLOTS`] is on or off: what comes out is what went in, and
    /// the width the directory reports is one the type admits.
    #[test]
    fn every_integer_round_trips_at_its_leafs_width() {
        let columns = vec![
            ColumnSpec::key(PhysicalType::Int64),
            ColumnSpec::new(PhysicalType::Int64),
        ];
        let builder = LeafBuilder::new(8192, 1, columns, 1).unwrap();
        // Each run is a set of values whose widest member decides the column's
        // width, and each includes the boundaries of the width below it.
        let runs: Vec<Vec<i64>> = vec![
            vec![0, 1, -1, 127, -128],
            vec![0, 128, -129, 32_767, -32_768],
            vec![0, 32_768, -32_769, 2_147_483_647, -2_147_483_648],
            vec![0, 2_147_483_648, -2_147_483_649, i64::MAX, i64::MIN],
        ];
        for run in &runs {
            let rows: Vec<Vec<Datum<'static>>> = run
                .iter()
                .enumerate()
                .map(|(n, value)| vec![Datum::Int(n as i64), Datum::Int(*value)])
                .collect();
            let page = builder.encode(&rows).unwrap();
            let leaf = LeafRef::parse(&page).unwrap();
            let width = leaf.column_width(1).unwrap();
            assert!(
                PhysicalType::Int64.admits_width(width),
                "the directory claims a width of {width}"
            );
            if NARROW_INT_SLOTS {
                let widest = run
                    .iter()
                    .map(|value| crate::types::int_slot_width(*value))
                    .max()
                    .unwrap_or(8);
                assert_eq!(width, widest, "run {run:?} was laid out at {width}");
            } else {
                assert_eq!(width, 8, "the switch is off and the width is not eight");
            }
            let column = leaf.column(1).unwrap();
            for (row, value) in run.iter().enumerate() {
                assert_eq!(
                    leaf.value(row, 1).unwrap().as_int().unwrap(),
                    *value,
                    "run {run:?} row {row}"
                );
                assert_eq!(column.int_unchecked(row).unwrap(), *value);
            }
            leaf.integrity().unwrap();
        }
    }

    /// A column whose values sit in a narrow band far from zero costs the band,
    /// not the magnitude - and every value still reads back exactly.
    ///
    /// This is what a frame of reference is for: an index leaf holds a
    /// contiguous run of its key, so the span inside one page is small even
    /// when the column's span is not.
    #[test]
    fn a_frame_of_reference_costs_the_span_not_the_magnitude() {
        let columns = vec![
            ColumnSpec::key(PhysicalType::Int64),
            ColumnSpec::new(PhysicalType::Int64),
        ];
        let builder = LeafBuilder::new(8192, 1, columns, 1).unwrap();
        // Two hundred keys starting at nine hundred thousand: four bytes each
        // without a base, one with.
        let rows: Vec<Vec<Datum<'static>>> = (0..200i64)
            .map(|n| vec![Datum::Int(900_000 + n), Datum::Int(-500_000 - n)])
            .collect();
        let page = builder.encode(&rows).unwrap();
        let leaf = LeafRef::parse(&page).unwrap();
        if NARROW_INT_SLOTS && FRAME_OF_REFERENCE {
            assert_eq!(leaf.column_width(0).unwrap(), 1, "the key did not narrow");
            assert_eq!(leaf.column_base(0).unwrap(), 900_000);
            assert_eq!(leaf.column_width(1).unwrap(), 1, "the negative column");
            assert_eq!(leaf.column_base(1).unwrap(), -500_199);
        }
        for row in 0..200usize {
            assert_eq!(
                leaf.value(row, 0).unwrap().as_int().unwrap(),
                900_000 + row as i64,
                "row {row} key"
            );
            assert_eq!(
                leaf.value(row, 1).unwrap().as_int().unwrap(),
                -500_000 - row as i64,
                "row {row} value"
            );
        }
        leaf.integrity().unwrap();
        // And the search finds every one of them, which is the path that reads
        // the slots raw rather than through `value`.
        for row in 0..200i64 {
            assert_eq!(
                leaf.search(&[Datum::Int(900_000 + row)]).unwrap(),
                Ok(row as usize),
                "searching for {}",
                900_000 + row
            );
        }
        assert_eq!(leaf.search(&[Datum::Int(899_999)]).unwrap(), Err(0));
        assert_eq!(leaf.search(&[Datum::Int(900_200)]).unwrap(), Err(200));
    }

    /// `fit` and `encode_rows` agree about how many rows a page holds, over a
    /// run whose width widens part way through.
    ///
    /// They derive the widths from the same rows by the same rule; if they ever
    /// stopped doing so, `fit` would price a page the encoder could not lay out
    /// and the encoder would overflow the heap into the mini-columns.
    #[test]
    fn fit_prices_the_page_the_encoder_writes() {
        let columns = vec![
            ColumnSpec::key(PhysicalType::Int64),
            ColumnSpec::new(PhysicalType::Int64),
            ColumnSpec::new(PhysicalType::Text),
        ];
        let builder = LeafBuilder::new(4096, 1, columns, 1).unwrap();
        // The second column is one byte wide for the first hundred rows and
        // eight from there, so a page that spans the boundary is priced at one
        // width and laid out at another unless the two agree.
        let rows: Vec<Vec<Datum<'static>>> = (0..4_000i64)
            .map(|n| {
                vec![
                    Datum::Int(n),
                    Datum::Int(if n < 100 { n % 100 } else { n * 1_000_000_000 }),
                    Datum::Text(b"label"),
                ]
            })
            .collect();
        for fill in [0.5, 0.75, 0.9, 1.0] {
            let mut at = 0usize;
            while at < rows.len() {
                let placed = builder.fit(&RowSlice(&rows), at, fill, false);
                assert!(placed > 0, "nothing fitted at {at}");
                let page = builder
                    .encode_rows(&RowSlice(&rows), at, placed, None)
                    .unwrap();
                let leaf = LeafRef::parse(&page).unwrap();
                assert_eq!(leaf.row_count(), placed);
                leaf.integrity().unwrap();
                for row in 0..placed {
                    assert_eq!(
                        leaf.value(row, 1).unwrap().as_int().unwrap(),
                        rows[at + row][1].as_int().unwrap(),
                        "fill {fill} at {at} row {row}"
                    );
                }
                at += placed;
            }
        }
    }

    /// An exception in an integer column keeps the slot wide enough to hold the
    /// heap offset it stores instead of a value.
    #[test]
    fn an_exception_keeps_an_integer_slot_four_bytes_wide() {
        let columns = vec![
            ColumnSpec::key(PhysicalType::Int64),
            ColumnSpec::new(PhysicalType::Int64),
        ];
        let builder = LeafBuilder::new(8192, 1, columns, 1).unwrap();
        let rows: Vec<Vec<Datum<'static>>> = (0..20i64)
            .map(|n| {
                vec![
                    Datum::Int(n),
                    if n == 7 {
                        Datum::Text(b"not an integer")
                    } else {
                        Datum::Int(n)
                    },
                ]
            })
            .collect();
        let page = builder.encode(&rows).unwrap();
        let leaf = LeafRef::parse(&page).unwrap();
        assert!(leaf.column_width(1).unwrap() >= 4);
        assert_eq!(
            leaf.value(7, 1).unwrap().as_bytes(),
            Some(b"not an integer".as_slice()),
            "the exception did not read back"
        );
        for row in (0..20usize).filter(|row| *row != 7) {
            assert_eq!(leaf.value(row, 1).unwrap().as_int().unwrap(), row as i64);
        }
        leaf.integrity().unwrap();
    }

    /// `live_order` names the same rows, in the same order, that `live`
    /// materialises - over a leaf with tombstones, delta rows and a delta row
    /// that shadows a sorted one.
    ///
    /// A compaction reads through `live_order` now and `live` is what every
    /// other caller and every property test uses, so the two disagreeing would
    /// be a compaction that silently changed the leaf's contents.
    #[test]
    fn live_order_agrees_with_live() {
        let columns = vec![
            ColumnSpec::key(PhysicalType::Int64),
            ColumnSpec::new(PhysicalType::Text),
            ColumnSpec::new(PhysicalType::Int64),
        ];
        let builder = LeafBuilder::new(4096, 1, columns.clone(), 1).unwrap();
        let labels: Vec<String> = (0..24).map(|n| format!("row-{n:04}")).collect();
        let rows: Vec<Vec<Datum<'_>>> = (0..24i64)
            .map(|n| {
                vec![
                    Datum::Int(n * 2),
                    Datum::Text(labels[n as usize].as_bytes()),
                    Datum::Int(n * 5),
                ]
            })
            .collect();
        let page = builder.encode(&rows).unwrap();
        // A delta row for a key that is not there, one that shadows a sorted
        // row, and a tombstone over a third.
        let fresh = vec![Datum::Int(7), Datum::Text(b"inserted"), Datum::Int(70)];
        let shadow = vec![Datum::Int(10), Datum::Text(b"replaced"), Datum::Int(99)];
        let mut page = with_delta(&page, &[fresh.clone(), shadow.clone()]);
        crate::mutate::LeafMut::new(&mut page)
            .unwrap()
            .set_tombstone(3)
            .unwrap();
        let leaf = LeafRef::parse(&page).unwrap();
        let materialised = leaf.live().unwrap();
        let source = leaf.live_source().unwrap();
        assert_eq!(
            source.len(),
            materialised.len(),
            "live_source named {} rows where live materialised {}",
            source.len(),
            materialised.len()
        );
        for (row, expected) in materialised.iter().enumerate() {
            for (column, want) in expected.iter().enumerate() {
                let got = source.value(row, column);
                assert_eq!(
                    format!("{got:?}"),
                    format!("{want:?}"),
                    "row {row} column {column}"
                );
            }
        }
    }

    /// Binary search finds every present key and reports the right insertion
    /// point for every absent one.
    #[test]
    fn search_finds_present_keys_and_places_absent_ones() {
        let columns = vec![ColumnSpec::key(PhysicalType::Int64)];
        let builder = LeafBuilder::new(8192, 1, columns, 1).unwrap();
        let rows: Vec<Vec<Datum<'static>>> =
            (0..50).map(|n| vec![Datum::Int(n as i64 * 2)]).collect();
        let page = builder.encode(&rows).unwrap();
        let leaf = LeafRef::parse(&page).unwrap();
        for n in 0..50i64 {
            assert_eq!(leaf.search(&[Datum::Int(n * 2)]).unwrap(), Ok(n as usize));
        }
        for n in 0..50i64 {
            assert_eq!(
                leaf.search(&[Datum::Int(n * 2 + 1)]).unwrap(),
                Err(n as usize + 1)
            );
        }
        assert_eq!(leaf.search(&[Datum::Int(-1)]).unwrap(), Err(0));
        assert_eq!(leaf.search(&[Datum::Int(1000)]).unwrap(), Err(50));
    }

    /// `pack` fills to the requested fraction and never overruns the page.
    #[test]
    fn pack_respects_the_fill_factor() {
        let builder = LeafBuilder::new(8192, 1, fixture_columns(), 1).unwrap();
        let rows = fixture_rows(500);
        match builder.pack(&rows, 0.9).unwrap() {
            Packed::Filled { page, rows: packed } => {
                assert_eq!(page.len(), 8192);
                assert!(packed > 0 && packed < 500, "packed {packed}");
                let leaf = LeafRef::parse(&page).unwrap();
                assert_eq!(leaf.row_count(), packed);
                // Adding one more row must not have fit inside the budget:
                // packing the prefix that includes it stops at the same row.
                match builder.pack(&rows[..packed + 1], 0.9).unwrap() {
                    Packed::Filled { rows: again, .. } => {
                        assert_eq!(again, packed, "one more row fit after all");
                    }
                    Packed::RowTooLarge => panic!("these rows fit"),
                }
            }
            Packed::RowTooLarge => panic!("these rows fit"),
        }
    }

    /// A row larger than a page is reported rather than silently truncated.
    #[test]
    fn an_oversized_row_is_reported() {
        let columns = vec![
            ColumnSpec::key(PhysicalType::Int64),
            ColumnSpec::new(PhysicalType::Blob),
        ];
        let builder = LeafBuilder::new(8192, 1, columns, 1).unwrap();
        let big = vec![0u8; 9000];
        let rows = vec![vec![Datum::Int(1), Datum::Blob(&big)]];
        assert_eq!(builder.pack(&rows, 0.9).unwrap(), Packed::RowTooLarge);
    }

    /// Writes a delta area into an already-built page.
    ///
    /// Nothing in Phase 1 *writes* a delta area - the leaf builder always
    /// leaves it empty and the tree rewrites a leaf rather than appending to
    /// one, because the delta path is a Phase 3 write-family item measured
    /// against the 16/32/64 sweep. The reader exists now, though, and a reader
    /// of bytes that come off a disk is exactly the code that has to be
    /// exercised before those bytes are hostile. So the tests build the area by
    /// hand, byte for byte as the layout describes it.
    ///
    /// @param page - a page from `LeafBuilder::encode`
    /// @param rows - the delta rows, each a list of values in column order
    fn with_delta(page: &[u8], rows: &[Vec<Datum<'_>>]) -> Vec<u8> {
        let mut out = page.to_vec();
        let leaf = LeafRef::parse(&out).unwrap();
        let count = leaf.row_count();
        let columns = leaf.column_count();
        // The delta area goes immediately after the last mini-column, which is
        // where the free space between the columns and the heap begins.
        let mut end = leaf_header::DIRECTORY + columns * leaf.directory_entry_size();
        for index in 0..columns {
            end = align8(end);
            end += class_bytes(count) + count * leaf.column_width(index).unwrap();
        }
        // Room for a tombstone bitmap between the mini-columns and the delta
        // area, because that is where the layout puts one and a later
        // `with_tombstones` has to have somewhere to write it.
        let delta_start = align8(end + tombstone_bytes(count));
        let mut bytes = Vec::new();
        for row in rows {
            let mut encoded = Vec::new();
            for value in row {
                value.encode_tagged(&mut encoded);
            }
            bytes.extend_from_slice(&(encoded.len() as u16).to_le_bytes());
            bytes.extend_from_slice(&encoded);
        }
        let heap_start = leaf.heap_start;
        assert!(
            delta_start + bytes.len() <= heap_start,
            "the delta does not fit: {delta_start} + {} > {heap_start}",
            bytes.len()
        );
        out[delta_start..delta_start + bytes.len()].copy_from_slice(&bytes);
        page::write_u32(&mut out, leaf_header::DELTA_START, delta_start as u32).unwrap();
        page::write_u16(&mut out, leaf_header::DELTA_COUNT, rows.len() as u16).unwrap();
        out[header::FLAGS] |= LEAF_HAS_DELTA;
        out
    }

    /// Sets a tombstone bit, moving the delta area up to make room for the
    /// bitmap the way a real delete would.
    ///
    /// @param page - a page from `LeafBuilder::encode`
    /// @param rows - which sorted-region rows to mark deleted
    fn with_tombstones(page: &[u8], rows: &[usize]) -> Vec<u8> {
        let mut out = page.to_vec();
        let leaf = LeafRef::parse(&out).unwrap();
        let count = leaf.row_count();
        let delta_start = leaf.delta_start;
        let bitmap = delta_start - tombstone_bytes(count);
        for row in rows {
            out[bitmap + row / 8] |= 1u8 << (row % 8);
        }
        out[header::FLAGS] |= LEAF_HAS_TOMBSTONES;
        out
    }

    /// A delta area reads back row by row and value by value, and merges into
    /// the live set in key order.
    #[test]
    fn a_delta_area_reads_back_and_merges() {
        let columns = vec![
            ColumnSpec::key(PhysicalType::Int64),
            ColumnSpec::new(PhysicalType::Int64),
            ColumnSpec::new(PhysicalType::Text),
        ];
        let builder = LeafBuilder::new(8192, 1, columns, 1).unwrap();
        let sorted: Vec<Vec<Datum<'static>>> = [10i64, 20, 30]
            .iter()
            .map(|key| {
                vec![
                    Datum::Int(*key),
                    Datum::Int(key * 2),
                    Datum::Text(b"sorted"),
                ]
            })
            .collect();
        let page = builder.encode(&sorted).unwrap();
        let delta = vec![
            vec![Datum::Int(25), Datum::Int(50), Datum::Text(b"delta-a")],
            vec![Datum::Int(5), Datum::Null, Datum::Text(b"delta-b")],
        ];
        let page = with_delta(&page, &delta);
        let leaf = LeafRef::parse(&page).unwrap();

        assert_eq!(leaf.delta_count(), 2);
        assert!(!leaf.is_clean(), "a delta area leaves the fast path");
        assert_eq!(leaf.live_rows().unwrap(), 5);
        assert_eq!(leaf.delta_value(0, 0).unwrap().as_int(), Some(25));
        assert_eq!(
            leaf.delta_value(0, 2).unwrap().as_bytes(),
            Some(b"delta-a".as_slice())
        );
        assert_eq!(leaf.delta_value(1, 0).unwrap().as_int(), Some(5));
        assert!(leaf.delta_value(1, 1).unwrap().is_null());
        assert!(!leaf.delta_row(1).unwrap().is_empty());

        // The merge: sorted region and delta together, in key order.
        let live = leaf.live().unwrap();
        let keys: Vec<i64> = live
            .iter()
            .map(|row| row[0].as_int().unwrap_or(-1))
            .collect();
        assert_eq!(keys, vec![5, 10, 20, 25, 30]);
        leaf.integrity().unwrap();
    }

    /// `locate` decodes each delta row once, left to right, and stops at the
    /// first column that differs - it does not re-measure a column it has
    /// already read.
    ///
    /// Five delta rows share their first two key columns and differ only on
    /// the third, so a probe that agrees with all five on those first two
    /// columns forces every row's comparison to walk out to the third before
    /// it can be ruled out - the shape that made `locate`'s old per-column
    /// `delta_value` calls cost the square of the key's width: comparing
    /// column two re-measured column zero's and column one's spans from
    /// scratch, on every one of the five rows.
    ///
    /// `Datum::tagged_span` is the call that measured a span it was not about
    /// to read - a skip past a column the caller wants no value from - so it
    /// is what a re-walk shows up as, and it is a test-only counter
    /// (`datum::probe`) rather than a clock, because a call count reads the
    /// same on an idle box and a loaded one where a duration would not.
    /// Reverting the fix and running only this test - with the counter kept -
    /// reads exactly 15: `1 + 2` re-measured spans on each of the five rows.
    #[test]
    fn locate_stops_reading_a_delta_row_at_the_first_mismatched_column() {
        let columns = vec![
            ColumnSpec::key(PhysicalType::Int64),
            ColumnSpec::key(PhysicalType::Int64),
            ColumnSpec::key(PhysicalType::Text),
        ];
        let builder = LeafBuilder::new(8192, 1, columns, 3).unwrap();
        // Sorted so it never collides with the delta rows' key: `999` sorts
        // after every probe or delta key this test uses.
        let sorted = vec![vec![Datum::Int(999), Datum::Int(0), Datum::Text(b"sorted")]];
        let page = builder.encode(&sorted).unwrap();
        let delta = vec![
            vec![Datum::Int(0), Datum::Int(0), Datum::Text(b"row-0")],
            vec![Datum::Int(0), Datum::Int(0), Datum::Text(b"row-1")],
            vec![Datum::Int(0), Datum::Int(0), Datum::Text(b"row-2")],
            vec![Datum::Int(0), Datum::Int(0), Datum::Text(b"row-3")],
            vec![Datum::Int(0), Datum::Int(0), Datum::Text(b"row-4")],
        ];
        let page = with_delta(&page, &delta);
        let leaf = LeafRef::parse(&page).unwrap();

        // A hit is still found correctly - the walk is reordered, not the answer.
        crate::datum::probe::reset_tagged_span_calls();
        let found = leaf
            .locate(&[Datum::Int(0), Datum::Int(0), Datum::Text(b"row-2")], 3)
            .unwrap();
        assert_eq!(found, crate::write::Located::Delta(2));

        // A miss that agrees with every row on the first two columns is the
        // case that used to pay for the re-walk five times over.
        crate::datum::probe::reset_tagged_span_calls();
        let missing = leaf
            .locate(&[Datum::Int(0), Datum::Int(0), Datum::Text(b"nomatch")], 3)
            .unwrap();
        assert_eq!(missing, crate::write::Located::Absent);
        assert_eq!(
            crate::datum::probe::tagged_span_calls(),
            0,
            "locate should decode each of the 5 delta rows' 3 columns once, left \
             to right, through decode_tagged - a re-walk that skips a column \
             it is about to decode anyway would show up here as tagged_span \
             calls greater than zero"
        );
    }

    /// Every way a delta area can be malformed is refused, and none of them
    /// panics.
    #[test]
    fn a_malformed_delta_area_is_refused() {
        let columns = vec![
            ColumnSpec::key(PhysicalType::Int64),
            ColumnSpec::new(PhysicalType::Int64),
        ];
        let builder = LeafBuilder::new(8192, 1, columns, 1).unwrap();
        let sorted = vec![
            vec![Datum::Int(1), Datum::Int(1)],
            vec![Datum::Int(2), Datum::Int(2)],
        ];
        let base = builder.encode(&sorted).unwrap();
        let good = with_delta(&base, &[vec![Datum::Int(7), Datum::Int(7)]]);
        LeafRef::parse(&good).unwrap();

        // A length that says the row is longer than it is: the values stop
        // decoding before the declared end.
        let leaf = LeafRef::parse(&good).unwrap();
        let at = leaf.delta_start;
        let mut lying_length = good.clone();
        page::write_u16(&mut lying_length, at, 40).unwrap();
        assert!(LeafRef::parse(&lying_length).is_err());

        // A length that reaches past the heap.
        let mut past_the_heap = good.clone();
        page::write_u16(&mut past_the_heap, at, 60_000).unwrap();
        assert!(LeafRef::parse(&past_the_heap).is_err());

        // A tag byte that is not a value.
        let mut bad_tag = good.clone();
        bad_tag[at + 2] = 200;
        assert!(LeafRef::parse(&bad_tag).is_err());

        // A row that decodes to fewer bytes than it declared.
        let mut short_row = good.clone();
        page::write_u16(&mut short_row, at, 19).unwrap();
        assert!(LeafRef::parse(&short_row).is_err());

        // Asking for a delta row and a delta column that do not exist.
        let leaf = LeafRef::parse(&good).unwrap();
        assert!(leaf.delta_row(1).is_err());
        assert!(leaf.delta_value(0, 9).is_err());
    }

    /// A delta row whose key is already live in the sorted region is an
    /// integrity failure, because a reader would then see the key twice.
    #[test]
    fn a_delta_row_may_not_duplicate_a_live_key() {
        let columns = vec![
            ColumnSpec::key(PhysicalType::Int64),
            ColumnSpec::new(PhysicalType::Int64),
        ];
        let builder = LeafBuilder::new(8192, 1, columns, 1).unwrap();
        let sorted = vec![
            vec![Datum::Int(1), Datum::Int(10)],
            vec![Datum::Int(2), Datum::Int(20)],
        ];
        let base = builder.encode(&sorted).unwrap();
        let clashing = with_delta(&base, &[vec![Datum::Int(2), Datum::Int(99)]]);
        let leaf = LeafRef::parse(&clashing).unwrap();
        assert!(leaf.integrity().is_err());

        // Unless the sorted-region row is tombstoned, in which case the delta
        // row is the live one and there is no duplicate.
        let tombstoned = with_tombstones(&clashing, &[1]);
        let leaf = LeafRef::parse(&tombstoned).unwrap();
        leaf.integrity().unwrap();
        assert!(leaf.is_tombstoned(1).unwrap());
        assert!(!leaf.is_tombstoned(0).unwrap());
        assert_eq!(leaf.live_rows().unwrap(), 2);
        let live = leaf.live().unwrap();
        assert_eq!(live.len(), 2);
        assert_eq!(live[1][1].as_int(), Some(99));
    }

    /// The tombstone bitmap is read only when the flag says it is there, and a
    /// row outside it is refused rather than indexed into.
    #[test]
    fn tombstones_are_read_only_when_they_exist() {
        let columns = vec![ColumnSpec::key(PhysicalType::Int64)];
        let builder = LeafBuilder::new(8192, 1, columns, 1).unwrap();
        let rows: Vec<Vec<Datum<'static>>> = (0..20).map(|n| vec![Datum::Int(n as i64)]).collect();
        let page = builder.encode(&rows).unwrap();
        let leaf = LeafRef::parse(&page).unwrap();
        assert!(!leaf.has_tombstones());
        assert!(leaf.tombstones().unwrap().is_empty());
        assert!(!leaf.is_tombstoned(0).unwrap());
        assert!(!leaf.is_tombstoned(9999).unwrap(), "no bitmap, no lookup");

        let marked = with_tombstones(&page, &[0, 3, 19]);
        let leaf = LeafRef::parse(&marked).unwrap();
        assert!(leaf.has_tombstones());
        assert!(!leaf.is_clean());
        assert!(!leaf.tombstones().unwrap().is_empty());
        assert!(leaf.is_tombstoned(0).unwrap());
        assert!(!leaf.is_tombstoned(1).unwrap());
        assert!(leaf.is_tombstoned(19).unwrap());
        assert!(leaf.is_tombstoned(20_000).is_err(), "past the bitmap");
        assert_eq!(leaf.live_rows().unwrap(), 17);
        assert_eq!(leaf.live().unwrap().len(), 17);
    }

    /// Every way a builder can be asked for an impossible leaf is refused.
    #[test]
    fn the_builder_refuses_impossible_leaves() {
        assert!(LeafBuilder::new(8192, 1, Vec::new(), 1).is_err());
        assert!(LeafBuilder::new(8192, 1, vec![ColumnSpec::key(PhysicalType::Int64)], 0).is_err());
        assert!(LeafBuilder::new(8192, 1, vec![ColumnSpec::key(PhysicalType::Int64)], 2).is_err());
        assert!(LeafBuilder::new(32, 1, vec![ColumnSpec::key(PhysicalType::Int64)], 1).is_err());

        // More rows than the row count field can hold.
        let builder =
            LeafBuilder::new(65_536, 1, vec![ColumnSpec::key(PhysicalType::Int64)], 1).unwrap();
        let too_many: Vec<Vec<Datum<'static>>> =
            (0..70_000).map(|n| vec![Datum::Int(n as i64)]).collect();
        assert!(builder.encode(&too_many).is_err());

        // Enough rows that the mini-columns alone overflow the page. The
        // values are past a 32-bit range so the column is eight bytes wide
        // whatever `NARROW_INT_SLOTS` says: two thousand one-byte slots fit an
        // eight-kilobyte page comfortably, and this is a test about the page
        // overflowing rather than about the width.
        let narrow =
            LeafBuilder::new(8_192, 1, vec![ColumnSpec::key(PhysicalType::Int64)], 1).unwrap();
        let wide: Vec<Vec<Datum<'static>>> = (0..2_000)
            .map(|n| vec![Datum::Int(n as i64 * 1_000_000_000_000)])
            .collect();
        assert!(narrow.encode(&wide).is_err());

        // The heap runs into the mini-columns.
        let heavy = LeafBuilder::new(
            8_192,
            1,
            vec![
                ColumnSpec::key(PhysicalType::Int64),
                ColumnSpec::new(PhysicalType::Text),
            ],
            1,
        )
        .unwrap();
        let long = vec![b'x'; 900];
        let rows: Vec<Vec<Datum<'_>>> = (0..20)
            .map(|n| vec![Datum::Int(n as i64), Datum::Text(&long)])
            .collect();
        assert!(heavy.encode(&rows).is_err());
    }

    /// A `Float64` column given an integer stores it as a double, and an `Any`
    /// column stores whatever it is given as a tagged value.
    #[test]
    fn a_real_column_takes_an_integer_and_an_any_column_takes_anything() {
        let columns = vec![
            ColumnSpec::key(PhysicalType::Int64),
            ColumnSpec::new(PhysicalType::Float64),
            ColumnSpec::new(PhysicalType::Any),
        ];
        let builder = LeafBuilder::new(8192, 1, columns, 1).unwrap();
        let rows = vec![
            vec![Datum::Int(1), Datum::Int(7), Datum::Int(-3)],
            vec![Datum::Int(2), Datum::Real(2.5), Datum::Text(b"anything")],
            vec![Datum::Int(3), Datum::Null, Datum::Null],
        ];
        let page = builder.encode(&rows).unwrap();
        let leaf = LeafRef::parse(&page).unwrap();
        // An integer in a REAL-affinity column is stored as the double, not as
        // an exception: that is what affinity means.
        assert_eq!(leaf.value(0, 1).unwrap().as_f64(), Some(7.0));
        assert!(matches!(leaf.value(0, 1).unwrap(), Datum::Real(_)));
        assert_eq!(leaf.value(1, 1).unwrap().as_f64(), Some(2.5));
        assert!(leaf.value(2, 1).unwrap().is_null());
        assert_eq!(leaf.value(0, 2).unwrap().as_int(), Some(-3));
        assert_eq!(
            leaf.value(1, 2).unwrap().as_bytes(),
            Some(b"anything".as_slice())
        );
        assert!(leaf.value(2, 2).unwrap().is_null());
        assert!(
            !leaf.has_exceptions(),
            "affinity conversion is not an exception"
        );
        leaf.integrity().unwrap();
    }

    /// A key wider than the inline key view still compares correctly, through
    /// the fallback the view documents.
    #[test]
    fn a_key_wider_than_the_inline_view_still_compares() {
        let columns: Vec<ColumnSpec> = (0..6)
            .map(|_| ColumnSpec::key(PhysicalType::Int64))
            .collect();
        let builder = LeafBuilder::new(8192, 1, columns, 6).unwrap();
        let rows: Vec<Vec<Datum<'static>>> = (0..40)
            .map(|n| {
                (0..6)
                    .map(|column| Datum::Int(if column == 5 { n as i64 } else { 1 }))
                    .collect()
            })
            .collect();
        let page = builder.encode(&rows).unwrap();
        let leaf = LeafRef::parse(&page).unwrap();
        for n in 0..40i64 {
            let probe: Vec<Datum<'_>> = (0..6)
                .map(|column| Datum::Int(if column == 5 { n } else { 1 }))
                .collect();
            assert_eq!(leaf.search(&probe).unwrap(), Ok(n as usize), "key {n}");
        }
        let missing: Vec<Datum<'_>> = (0..6)
            .map(|column| Datum::Int(if column == 5 { 40 } else { 1 }))
            .collect();
        assert_eq!(leaf.search(&missing).unwrap(), Err(40));
    }

    /// The small accessors answer, including the ones nothing else reaches.
    #[test]
    fn the_small_accessors_answer() {
        let builder = LeafBuilder::new(8192, 7, fixture_columns(), 1).unwrap();
        let page = builder.encode(&fixture_rows(5)).unwrap();
        let leaf = LeafRef::parse(&page).unwrap();
        assert_eq!(leaf.bytes().len(), 8192);
        assert_eq!(leaf.max_cts(), 0);
        assert_eq!(leaf.right_sibling(), PageId::NONE);
        assert_eq!(
            leaf.column(1).unwrap().inline_bytes().len(),
            5 * leaf.column_width(1).unwrap()
        );
        assert!(leaf.spec(4).is_ok());
        assert!(leaf.spec(5).is_err(), "a column past the directory");
        assert!(leaf.column(5).is_err());
        assert_eq!(
            compare_rows(&[Datum::Int(1)], &[Datum::Int(1), Datum::Int(2)], 2),
            std::cmp::Ordering::Equal,
            "a row shorter than the key compares equal rather than panicking"
        );
    }

    /// The extent checks `parse` gave up live in `integrity` and still fire.
    #[test]
    fn integrity_catches_a_misplaced_mini_column() {
        let builder = LeafBuilder::new(8192, 1, fixture_columns(), 1).unwrap();
        let good = builder.encode(&fixture_rows(10)).unwrap();
        LeafRef::parse(&good).unwrap().integrity().unwrap();

        // Overlapping the directory.
        let mut into_the_directory = good.clone();
        page::write_u32(&mut into_the_directory, leaf_header::DIRECTORY + 4, 8).unwrap();
        assert!(LeafRef::parse(&into_the_directory)
            .unwrap()
            .integrity()
            .is_err());

        // Not eight-byte aligned.
        let leaf = LeafRef::parse(&good).unwrap();
        let where_it_is = page::read_u32(&good, leaf_header::DIRECTORY + 4).unwrap();
        let _ = leaf;
        let mut misaligned = good.clone();
        page::write_u32(
            &mut misaligned,
            leaf_header::DIRECTORY + 4,
            where_it_is.saturating_add(4),
        )
        .unwrap();
        assert!(LeafRef::parse(&misaligned).unwrap().integrity().is_err());

        // Past the delta area.
        let mut past_the_columns = good.clone();
        page::write_u32(&mut past_the_columns, leaf_header::DIRECTORY + 4, 8_000).unwrap();
        assert!(LeafRef::parse(&past_the_columns)
            .unwrap()
            .integrity()
            .is_err());
    }

    /// A sorted region whose keys do not increase is an integrity failure.
    #[test]
    fn integrity_catches_keys_that_do_not_increase() {
        let columns = vec![ColumnSpec::key(PhysicalType::Int64)];
        let builder = LeafBuilder::new(8192, 1, columns, 1).unwrap();
        // The builder does not sort, so handing it unsorted rows produces a
        // page that parses and fails its integrity check - which is exactly the
        // contract `pack` documents.
        let rows = vec![
            vec![Datum::Int(3)],
            vec![Datum::Int(1)],
            vec![Datum::Int(2)],
        ];
        let page = builder.encode(&rows).unwrap();
        let leaf = LeafRef::parse(&page).unwrap();
        assert!(leaf.integrity().is_err());

        let duplicated = vec![vec![Datum::Int(1)], vec![Datum::Int(1)]];
        let page = builder.encode(&duplicated).unwrap();
        assert!(LeafRef::parse(&page).unwrap().integrity().is_err());
    }

    /// The paths a corrupt or empty leaf takes through the class array.
    ///
    /// `all_typed` has two answers nothing else asked for: an empty column is
    /// vacuously all-typed, and a class array shorter than the row count claims
    /// is not - which is a corrupt page rather than a leaf full of NULLs, and
    /// the fast path must refuse it rather than read past the array.
    #[test]
    fn the_class_array_edges_answer() {
        let columns = vec![
            ColumnSpec::key(PhysicalType::Int64),
            ColumnSpec::new(PhysicalType::Int64),
        ];
        let builder = LeafBuilder::new(8192, 1, columns, 1).unwrap();

        let empty = builder.encode_empty().unwrap();
        let leaf = LeafRef::parse(&empty).unwrap();
        assert_eq!(leaf.row_count(), 0);
        assert!(
            leaf.column(0).unwrap().all_typed(),
            "no rows, nothing untyped"
        );
        assert!(!leaf.column(0).unwrap().any_exception().unwrap());
        assert_eq!(leaf.live_rows().unwrap(), 0);
        assert!(leaf.live().unwrap().is_empty());

        // A row count larger than the page can hold: the column itself is
        // refused, before anything reads a class bit. That is why `all_typed`'s
        // short-array arm is documented unreachable rather than tested - a
        // caller cannot obtain the column it would need.
        let rows: Vec<Vec<Datum<'static>>> = (0..40)
            .map(|n| vec![Datum::Int(n as i64), Datum::Int(1)])
            .collect();
        let good = builder.encode(&rows).unwrap();
        let mut lying_count = good.clone();
        // Forty thousand rather than four: the columns hold small integers, so
        // a leaf that narrows its slots to a byte would still fit four thousand
        // of them in an eight-kilobyte page and the column would parse.
        page::write_u16(&mut lying_count, leaf_header::ROW_COUNT, 40_000).unwrap();
        let leaf = LeafRef::parse(&lying_count).unwrap();
        assert!(leaf.column(1).is_err());
        assert!(leaf.integrity().is_err());
        assert!(leaf.live().is_err());
    }

    /// `any_exception` finds one, which is what the integrity check asks it.
    #[test]
    fn any_exception_finds_an_exception() {
        let columns = vec![
            ColumnSpec::key(PhysicalType::Int64),
            ColumnSpec::new(PhysicalType::Int64),
        ];
        let builder = LeafBuilder::new(8192, 1, columns, 1).unwrap();
        let rows = vec![
            vec![Datum::Int(1), Datum::Int(1)],
            vec![Datum::Int(2), Datum::Text(b"not an integer")],
        ];
        let page = builder.encode(&rows).unwrap();
        let leaf = LeafRef::parse(&page).unwrap();
        assert!(leaf.column(1).unwrap().any_exception().unwrap());
        assert!(!leaf.column(0).unwrap().any_exception().unwrap());
        leaf.integrity().unwrap();
    }

    /// `pack` measures the heap correctly for NULLs, exceptions and `Any`.
    ///
    /// The sizing path has an arm per class and `pack` binary-searches on it,
    /// so an arm that measured wrongly would produce a page that does not fit
    /// rather than a wrong answer - which is a failure at build time and easy
    /// to miss until a leaf happens to be full.
    #[test]
    fn pack_measures_every_value_class() {
        let columns = vec![
            ColumnSpec::key(PhysicalType::Int64),
            ColumnSpec::new(PhysicalType::Int64),
            ColumnSpec::new(PhysicalType::Any),
            ColumnSpec::new(PhysicalType::Text),
        ];
        let builder = LeafBuilder::new(8192, 1, columns, 1).unwrap();
        let long = vec![b'y'; 40];
        let rows: Vec<Vec<Datum<'_>>> = (0..400)
            .map(|n| {
                vec![
                    Datum::Int(n as i64),
                    // Every third row is NULL, every third an exception.
                    match n % 3 {
                        0 => Datum::Int(n as i64),
                        1 => Datum::Null,
                        _ => Datum::Text(b"an exception in an integer column"),
                    },
                    Datum::Blob(&long),
                    Datum::Text(&long),
                ]
            })
            .collect();
        match builder.pack(&rows, 0.9).unwrap() {
            Packed::Filled { page, rows: packed } => {
                assert!(packed > 0 && packed < 400, "packed {packed}");
                let leaf = LeafRef::parse(&page).unwrap();
                assert_eq!(leaf.row_count(), packed);
                assert!(leaf.has_exceptions());
                leaf.integrity().unwrap();
                for row in 0..packed {
                    let value = leaf.value(row, 1).unwrap();
                    match row % 3 {
                        0 => assert_eq!(value.as_int(), Some(row as i64)),
                        1 => assert!(value.is_null()),
                        _ => assert_eq!(
                            value.as_bytes(),
                            Some(b"an exception in an integer column".as_slice())
                        ),
                    }
                    assert_eq!(
                        leaf.value(row, 2).unwrap().as_bytes(),
                        Some(long.as_slice())
                    );
                }
            }
            Packed::RowTooLarge => panic!("these rows fit"),
        }
    }

    /// A heap large enough to reach the mini-columns is refused by name.
    #[test]
    fn a_heap_that_reaches_the_columns_is_refused() {
        let columns = vec![
            ColumnSpec::key(PhysicalType::Int64),
            ColumnSpec::new(PhysicalType::Text),
        ];
        let builder = LeafBuilder::new(8_192, 1, columns, 1).unwrap();
        // Seventeen rows of five hundred bytes: the mini-columns are small, so
        // the heap runs down into them rather than off the end of the page.
        // Sixteen was enough while a text slot was eight bytes and is not now
        // that it is four - the mini-columns got smaller, so the heap has
        // further to fall before it reaches them, which is the change working.
        let long = vec![b'z'; 500];
        let rows: Vec<Vec<Datum<'_>>> = (0..17)
            .map(|n| vec![Datum::Int(n as i64), Datum::Text(&long)])
            .collect();
        let error = builder.encode(&rows).unwrap_err();
        let detail = error.detail().unwrap_or("").to_string();
        assert!(
            detail.contains("heap") || detail.contains("collided"),
            "{detail}"
        );
    }

    /// Corrupting any single byte of the leaf header is refused or produces a
    /// view whose accessors all still return a result rather than panicking.
    ///
    /// This is the TDD's "corrupt every field" test. It does not claim every
    /// mutation is detected - a page-size field that stays in range is a legal
    /// page describing different rows - it claims that no mutation reaches an
    /// out-of-bounds read or a panic, which is the property that matters when
    /// the bytes come off a disk somebody else wrote.
    #[test]
    fn no_single_byte_corruption_panics() {
        let builder = LeafBuilder::new(8192, 1, fixture_columns(), 1).unwrap();
        let rows = fixture_rows(30);
        let original = builder.encode(&rows).unwrap();
        // The header, the directory, and the first mini-column: the fields a
        // reader trusts before it has checked anything.
        for at in 0..200usize {
            for bit in 0..8u32 {
                let mut page = original.clone();
                if let Some(byte) = page.get_mut(at) {
                    *byte ^= 1u8 << bit;
                } else {
                    continue;
                }
                let Ok(leaf) = LeafRef::parse(&page) else {
                    continue;
                };
                // Every accessor over every row and column must return, not panic.
                for row in 0..leaf.row_count().min(64) {
                    let _ = leaf.is_tombstoned(row);
                    for column in 0..leaf.column_count().min(16) {
                        let _ = leaf.value(row, column);
                    }
                }
                for index in 0..leaf.delta_count() {
                    let _ = leaf.delta_row(index);
                }
                let _ = leaf.live();
                let _ = leaf.search(&[Datum::Int(5)]);
                let _ = leaf.live_rows();
            }
        }
    }

    /// A page that is not a leaf, is too short, or declares impossible counts
    /// is refused by `parse` rather than trusted.
    #[test]
    fn structurally_impossible_pages_are_refused() {
        let builder = LeafBuilder::new(8192, 1, fixture_columns(), 1).unwrap();
        let good = builder.encode(&fixture_rows(10)).unwrap();

        assert!(LeafRef::parse(&[]).is_err());
        assert!(LeafRef::parse(&good[..40]).is_err());

        let mut wrong_kind = good.clone();
        wrong_kind[header::KIND] = PageKind::Interior.code();
        assert!(LeafRef::parse(&wrong_kind).is_err());

        let mut no_columns = good.clone();
        page::write_u16(&mut no_columns, leaf_header::COLUMN_COUNT, 0).unwrap();
        assert!(LeafRef::parse(&no_columns).is_err());

        let mut too_many_keys = good.clone();
        page::write_u16(&mut too_many_keys, leaf_header::KEY_COLUMNS, 9).unwrap();
        assert!(LeafRef::parse(&too_many_keys).is_err());

        let mut zero_keys = good.clone();
        page::write_u16(&mut zero_keys, leaf_header::KEY_COLUMNS, 0).unwrap();
        assert!(LeafRef::parse(&zero_keys).is_err());

        let mut heap_past_end = good.clone();
        page::write_u32(&mut heap_past_end, leaf_header::HEAP_START, 9000).unwrap();
        assert!(LeafRef::parse(&heap_past_end).is_err());

        // A mini-column offset that runs past the page is caught by the
        // accessor and by `integrity`, not by `parse`: checking it there costs
        // a directory walk on every read, and no read is unsafe without it.
        let mut column_past_end = good.clone();
        page::write_u32(&mut column_past_end, leaf_header::DIRECTORY + 4, 8_180).unwrap();
        let leaf = LeafRef::parse(&column_past_end).unwrap();
        assert!(leaf.integrity().is_err());
        assert!(leaf.value(0, 0).is_err());
        assert!(leaf.live().is_err());

        // An offset that still fits reads the wrong bytes rather than failing,
        // and that is the honest limit of what a reader can detect on its own:
        // the values are in bounds and mean nothing. The page checksum and
        // `integrity` are what catch it, which is why both exist.
        let mut column_moved = good.clone();
        page::write_u32(&mut column_moved, leaf_header::DIRECTORY + 4, 4_000).unwrap();
        let leaf = LeafRef::parse(&column_moved).unwrap();
        assert!(leaf.integrity().is_err());
        assert!(leaf.value(0, 0).is_ok());

        let mut delta_above_heap = good.clone();
        page::write_u32(&mut delta_above_heap, leaf_header::DELTA_START, 8100).unwrap();
        assert!(LeafRef::parse(&delta_above_heap).is_err());

        let mut delta_in_the_directory = good.clone();
        page::write_u32(&mut delta_in_the_directory, leaf_header::DELTA_START, 10).unwrap();
        assert!(LeafRef::parse(&delta_in_the_directory).is_err());

        let mut lying_delta_count = good.clone();
        page::write_u16(&mut lying_delta_count, leaf_header::DELTA_COUNT, 5).unwrap();
        assert!(LeafRef::parse(&lying_delta_count).is_err());

        let mut over_the_delta_limit = good.clone();
        page::write_u16(&mut over_the_delta_limit, leaf_header::DELTA_COUNT, 33).unwrap();
        assert!(LeafRef::parse(&over_the_delta_limit).is_err());

        // A lying exception flag is not a parse failure any more: checking it
        // costs a pass over every class array, which `parse` deliberately does
        // not do. It is an integrity failure, and a reader is unaffected either
        // way because `all_typed` re-derives the answer from the array.
        let mut lying_exception_flag = good.clone();
        lying_exception_flag[header::FLAGS] |= LEAF_HAS_EXCEPTIONS;
        let leaf = LeafRef::parse(&lying_exception_flag).unwrap();
        assert!(leaf.integrity().is_err());
        assert!(!leaf.column(1).unwrap().any_exception().unwrap());
        assert_eq!(leaf.value(0, 1).unwrap().as_int(), Some(0));
    }

    /// The class-array and tombstone size functions pad to eight bytes at every
    /// row count, so the value array that follows is always aligned.
    #[test]
    fn size_helpers_pad_to_eight() {
        for rows in 0..300usize {
            assert_eq!(class_bytes(rows) % 8, 0, "{rows}");
            assert!(class_bytes(rows) * 4 >= rows, "{rows}");
            assert_eq!(tombstone_bytes(rows) % 8, 0, "{rows}");
            assert!(tombstone_bytes(rows) * 8 >= rows, "{rows}");
        }
        assert_eq!(class_bytes(0), 0);
        assert_eq!(class_bytes(1), 8);
        assert_eq!(class_bytes(32), 8);
        assert_eq!(class_bytes(33), 16);
    }

    /// A column declared not-nullable still reads a NULL back as NULL: the
    /// class array is the truth, and the flag is a statement about the schema
    /// rather than a licence to skip the check.
    #[test]
    fn the_class_array_outranks_the_nullable_flag() {
        let columns = vec![
            ColumnSpec::key(PhysicalType::Int64),
            ColumnSpec {
                physical: PhysicalType::Int64,
                flags: 0,
                collation: inillucent_value::collation::Collation::Binary,
                descending: false,
            },
        ];
        let builder = LeafBuilder::new(8192, 1, columns, 1).unwrap();
        let rows = vec![vec![Datum::Int(1), Datum::Null]];
        let page = builder.encode(&rows).unwrap();
        let leaf = LeafRef::parse(&page).unwrap();
        assert!(leaf.value(0, 1).unwrap().is_null());
        assert_eq!(leaf.spec(1).unwrap().flags & COLUMN_NULLABLE, 0);
    }

    /// Every physical type packs and reads back, including `Any`.
    #[test]
    fn every_physical_type_round_trips() {
        for physical in PhysicalType::all() {
            let columns = vec![
                ColumnSpec::key(PhysicalType::Int64),
                ColumnSpec::new(physical),
            ];
            let builder = LeafBuilder::new(8192, 1, columns, 1).unwrap();
            let sample = match physical {
                PhysicalType::Int64 => Datum::Int(-7),
                PhysicalType::Float64 => Datum::Real(2.5),
                PhysicalType::Text => Datum::Text(b"sample"),
                PhysicalType::Blob => Datum::Blob(&[1, 2, 3]),
                PhysicalType::Any => Datum::Text(b"anything"),
            };
            let rows = vec![
                vec![Datum::Int(1), sample],
                vec![Datum::Int(2), Datum::Null],
            ];
            let page = builder.encode(&rows).unwrap();
            let leaf = LeafRef::parse(&page).unwrap();
            assert_eq!(
                leaf.value(0, 1).unwrap().compare(&sample),
                std::cmp::Ordering::Equal,
                "{physical:?}"
            );
            assert!(leaf.value(1, 1).unwrap().is_null(), "{physical:?}");
        }
    }

    /// The interpolation search agrees with a binary search on every key, on
    /// dense keys, sparse keys, clustered keys and a single repeated key.
    ///
    /// The point of the sweep is that interpolation is *data-adaptive*: it is
    /// fast when the keys are near-uniform and must merely be correct when they
    /// are not. Every distribution here is one it could get wrong.
    #[test]
    fn the_integer_search_agrees_with_a_binary_search() {
        let columns = vec![
            ColumnSpec::key(PhysicalType::Int64),
            ColumnSpec::new(PhysicalType::Int64),
        ];
        let builder = LeafBuilder::new(8192, 1, columns, 1).unwrap();
        let distributions: Vec<(&str, Vec<i64>)> = vec![
            ("dense", (0..200i64).collect()),
            ("sparse", (0..200i64).map(|n| n * 1_000).collect()),
            (
                "clustered",
                (0..200i64)
                    .map(|n| if n < 190 { n } else { n * 100_000 })
                    .collect(),
            ),
            (
                "exponential",
                (0..60i64)
                    .map(|n| 1i64 << (n / 2))
                    .scan(0i64, |last, v| {
                        *last = (*last + 1).max(v);
                        Some(*last)
                    })
                    .collect(),
            ),
            ("negative", (-100..100i64).collect()),
            ("extremes", {
                let mut keys: Vec<i64> = (0..100i64).collect();
                keys.push(i64::MAX);
                keys.insert(0, i64::MIN);
                keys
            }),
        ];
        for (name, keys) in distributions {
            let rows: Vec<Vec<Datum<'_>>> = keys
                .iter()
                .map(|key| vec![Datum::Int(*key), Datum::Int(key.saturating_mul(2))])
                .collect();
            let page = builder.encode(&rows).unwrap();
            let leaf = LeafRef::parse(&page).unwrap();
            // Every key that is there, and the gaps either side of each.
            for (index, key) in keys.iter().enumerate() {
                let found = leaf.search(&[Datum::Int(*key)]).unwrap();
                assert_eq!(found, Ok(index), "{name}: key {key}");
                for probe in [key.saturating_sub(1), key.saturating_add(1)] {
                    if keys.contains(&probe) {
                        continue;
                    }
                    let interpolated = leaf.search(&[Datum::Int(probe)]).unwrap();
                    let bisected = keys.binary_search(&probe);
                    assert_eq!(
                        interpolated, bisected,
                        "{name}: probe {probe} disagreed with a binary search"
                    );
                }
            }
            // Outside both ends.
            assert_eq!(
                leaf.search(&[Datum::Int(i64::MIN)]).unwrap().is_ok(),
                keys.contains(&i64::MIN),
                "{name}: i64::MIN"
            );
            assert_eq!(
                leaf.search(&[Datum::Int(i64::MAX)]).unwrap().is_ok(),
                keys.contains(&i64::MAX),
                "{name}: i64::MAX"
            );
        }
    }

    /// A leaf the interpolation path must not take: a NULL in the key column
    /// makes it not all-typed, and a text probe is not an integer.
    #[test]
    fn the_integer_search_declines_what_it_cannot_answer() {
        let columns = vec![
            ColumnSpec::new(PhysicalType::Int64),
            ColumnSpec::new(PhysicalType::Text),
        ];
        let builder = LeafBuilder::new(8192, 1, columns, 1).unwrap();
        let mut rows: Vec<Vec<Datum<'_>>> = (1..40i64)
            .map(|n| vec![Datum::Int(n), Datum::Text(b"x")])
            .collect();
        rows.insert(0, vec![Datum::Null, Datum::Text(b"x")]);
        let page = builder.encode(&rows).unwrap();
        let leaf = LeafRef::parse(&page).unwrap();
        // The NULL makes the column not all-typed, so the general path answers.
        assert_eq!(leaf.search(&[Datum::Int(20)]).unwrap(), Ok(20));
        assert_eq!(leaf.search(&[Datum::Null]).unwrap(), Ok(0));
        assert!(leaf.search(&[Datum::Text(b"zz")]).unwrap().is_err());
    }

    /// A leaf too small for interpolation to be worth a branch uses the binary
    /// search, and still answers.
    #[test]
    fn a_short_leaf_uses_the_binary_search() {
        let columns = vec![ColumnSpec::key(PhysicalType::Int64)];
        let builder = LeafBuilder::new(8192, 1, columns, 1).unwrap();
        let rows: Vec<Vec<Datum<'_>>> = (0..4i64).map(|n| vec![Datum::Int(n * 5)]).collect();
        let page = builder.encode(&rows).unwrap();
        let leaf = LeafRef::parse(&page).unwrap();
        for (index, key) in [0i64, 5, 10, 15].iter().enumerate() {
            assert_eq!(leaf.search(&[Datum::Int(*key)]).unwrap(), Ok(index));
        }
        assert_eq!(leaf.search(&[Datum::Int(7)]).unwrap(), Err(2));
        assert_eq!(leaf.search(&[Datum::Int(-1)]).unwrap(), Err(0));
        assert_eq!(leaf.search(&[Datum::Int(99)]).unwrap(), Err(4));
    }
}
