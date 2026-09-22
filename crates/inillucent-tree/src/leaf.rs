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
//! ..        free space
//! ..        tombstone bitmap, if the has_tombstones flag is set
//! ..        delta area: a directory of delta_count u16 entries in key order,
//!           then the rows it names (see `leaf/delta.rs`)
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

use inillucent_pool::extent::EXTENT_REF_BYTES;

use crate::datum::Datum;
use crate::page::{self, header, PageId, PageKind};
use crate::types::{
    heap_slot_width, int_slot_width, read_heap_slot, read_int_slot, read_slot_offset,
    write_heap_slot, write_int_slot, write_slot_offset, ValueClass, COLUMN_ALL_TYPED, COLUMN_KEY,
};

// —— a leaf page, in the four things it is (task-1962, A8) —————————————
//
// 5,026 lines, the largest file in the workspace, with `impl LeafRef` alone at
// 1,470. The two modules that hold part of that `impl` reopen it, so no
// signature changed.
mod compare;
mod encode;
mod layout;
mod read;

pub use compare::{compare_rows, compare_rows_under, Hit, KeyView};
pub use encode::{
    encode_extent_tagged, Extents, ImageTiming, LeafBuilder, Packed, RowSlice, Rows, Spill,
};
pub use layout::{
    class_bytes, extent_class_for, extent_datum, fits_frame, from_frame, tombstone_bytes,
    write_frame, Layout,
};
pub use read::{LiveOrder, LiveRow, LiveSource, MiniColumn};

mod delta;
mod splice;

pub use splice::{splice_image, splices_of};

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

/// The size of one delta directory entry: a `u16` distance back from the heap.
///
/// **The delta area used to be capped at 32 rows, and it is now capped by the
/// free gap alone** (task-2074). Thirty-two was the TDD's number, measured in
/// Phase 3 against 16 and 64, and it made sense while a lookup scanned the area
/// row by row: the cap was what kept that scan short. It was a count and not a
/// size, so an index leaf of 3,704 packed rows compacted after every 32 writes
/// just as a table leaf of 241 did, and a compaction re-reads every live row -
/// which is why task-2066's audit found the two secondary indexes of
/// `write.insert.batch` were 69% of it. The area now has a directory in key
/// order, so a lookup is a binary search and nothing depends on it being short.
pub const DELTA_ENTRY: usize = 2;

/// Bit 5: the leaf is in format 2's layout, with a delta directory.
///
/// **Every leaf this build writes has it, and a leaf without it was written by
/// format 1** (task-2074). The two differ only in the delta area: format 1 has
/// no directory, holds at most 32 rows in the order they arrived, newest first,
/// and is walked rather than searched. A page says which it is rather than the
/// file saying it for every page, because a file format 1 wrote and this build
/// then writes to holds both kinds until every leaf in it has been rewritten -
/// the same argument [`LEAF_WIDE_DIRECTORY`] makes for being a flag.
///
/// A format 1 leaf is read as it is. It is rewritten in format 2's layout, and
/// the rewrite logged as a whole page image, the first time this build writes
/// to it - see `PagedTree::upgrade_format_one`. Recovery replaying a format 1
/// log onto format 1 pages follows format 1's rules, so it lands on the bytes
/// the build that wrote the log produced.
pub const LEAF_DELTA_DIRECTORY: u8 = 0b0010_0000;

/// Bits 6 and 7 of the common flags byte: how many compactions in a row have
/// spliced this leaf rather than repacking it.
///
/// **A splice keeps the page's slot widths and its heap as they are**, which is
/// what makes it cheap and is also its cost: a width never narrows again, and a
/// heap value a deleted row left behind is never reclaimed. So the count is
/// kept on the page, and a compaction that finds it at [`SPLICE_LIMIT`] repacks
/// the leaf from scratch and sets it back to zero. It is on the page rather than
/// anywhere else because recovery re-runs a compaction from the page it starts
/// from and has to make the same choice. See `leaf/splice.rs`.
pub const LEAF_SPLICES_SHIFT: u8 = 6;

/// The mask of [`LEAF_SPLICES_SHIFT`]'s two bits.
pub const LEAF_SPLICES_MASK: u8 = 0b1100_0000;

/// How many compactions in a row may splice before one has to repack.
///
/// Three, which is what two bits of the flag byte hold once one of the three
/// spare bits went to [`LEAF_DELTA_DIRECTORY`]. So at most three compactions in
/// every four are splices.
pub const SPLICE_LIMIT: u8 = 3;

/// The most rows a format 1 delta area held, which format 1's writer enforced.
///
/// Recovery replaying a format 1 log onto a format 1 page enforces it too, so a
/// replay refuses exactly where the build that wrote the log refused.
pub const FORMAT_ONE_DELTA_LIMIT: usize = 32;

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
/// and an index leaf now holds twice as many - and a delta limit of 64 rather
/// than 32 was measured on both arms and **does not buy it back**. (task-2074
/// removed the count limit, with a directory that makes a large delta area cheap
/// to search; see [`DELTA_ENTRY`].)
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
        // The delta directory has to fit inside the delta area. Each row it
        // names is checked when it is read, not here: see `validate_delta`. A
        // format 1 leaf has no directory and holds at most 32 rows.
        let directory = match flags & LEAF_DELTA_DIRECTORY != 0 {
            true => delta_count.saturating_mul(DELTA_ENTRY),
            false if delta_count > FORMAT_ONE_DELTA_LIMIT => {
                return Err(corrupt(format!(
                    "a format 1 leaf holds {delta_count} delta rows, over the limit of \
                     {FORMAT_ONE_DELTA_LIMIT}"
                )));
            }
            false => 0,
        };
        if delta_start.saturating_add(directory) > heap_start {
            return Err(corrupt(format!(
                "a delta directory of {delta_count} entries does not fit below the heap at {heap_start}"
            )));
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
        // **The delta rows are not walked here any more** (task-2074). They
        // were, while the area held at most 32 rows; it now holds as many as
        // the free gap does, and a parse runs on every level of every descent.
        // Every delta accessor bounds the row it reads, so the walk moved to
        // `integrity`, where a check proportional to the page belongs.
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
    /// 4. The delta area decodes, its rows fill it exactly, and its directory
    ///    is in key order.
    pub fn integrity(&self) -> DbResult<()> {
        self.validate_delta()?;
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
}

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{ColumnSpec, PhysicalType, COLUMN_NULLABLE};

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
