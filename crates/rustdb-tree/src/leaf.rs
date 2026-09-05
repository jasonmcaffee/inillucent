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

use rustdb_base::error::{corrupt, misuse};
use rustdb_base::DbResult;

use crate::datum::Datum;
use crate::page::{self, header, PageId, PageKind};
use crate::types::{ColumnSpec, PhysicalType, ValueClass, COLUMN_KEY};

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

/// The most rows the delta area may hold before a compaction is forced.
///
/// Thirty-two is the TDD's number and is measured in Phase 3 against 16 and 64.
/// The reasoning: a merge on read costs at most this many comparisons, and a
/// compaction's memmove is amortised across this many inserts.
pub const DELTA_LIMIT: usize = 32;

/// The size of one column directory entry.
const DIRECTORY_ENTRY: usize = 8;

/// The arm a caller cannot reach, kept because removing it would be a lie.
///
/// The coverage gate asks for 100% branch coverage on this codec with the
/// unreachable branches documented, and the way to document one is to make it
/// say so in the code rather than in a spreadsheet. Every call site names why
/// it cannot happen; a test build panics if one ever does, so "unreachable"
/// stays a claim the test suite checks rather than a comment that rots.
///
/// @param why - what the caller has already established
fn unreachable_branch(why: &str) -> rustdb_base::DbError {
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
        let at = row.saturating_mul(8);
        let slice = self
            .values
            .get(at..at.saturating_add(8))
            .ok_or_else(|| corrupt(format!("row {row} is past the key column")))?;
        let mut raw = [0u8; 8];
        raw.copy_from_slice(slice);
        Ok(i64::from_le_bytes(raw))
    }
}

/// A parsed leaf, borrowing the page it describes.
///
/// Holds no allocation: everything is derived from the page bytes on demand,
/// because a scan visits hundreds of leaves and asks each for two or three of
/// its columns.
#[derive(Clone, Copy, Debug)]
pub struct LeafRef<'p> {
    /// The collation of each key column, supplied by whoever built the tree.
    ///
    /// Empty means BINARY throughout, which is what a bare [`LeafRef::parse`]
    /// gives - the page does not carry collations and must not, because the
    /// catalog is what says a column has one. The tree hands them in, and a
    /// comparison that used the page's answer instead would silently disagree
    /// with the order the tree is stored in.
    collations: &'p [rustdb_value::collation::Collation],
    page: &'p [u8],
    row_count: usize,
    delta_count: usize,
    column_count: usize,
    key_columns: usize,
    heap_start: usize,
    delta_start: usize,
    flags: u8,
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
        let directory_end = leaf_header::DIRECTORY
            .checked_add(column_count.saturating_mul(DIRECTORY_ENTRY))
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
            page: pageent,
            row_count,
            delta_count,
            column_count,
            key_columns,
            heap_start,
            delta_start,
            flags,
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
            .checked_add(self.column_count.saturating_mul(DIRECTORY_ENTRY))
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
        for index in 0..self.column_count {
            seen_exception = seen_exception || self.column(index)?.any_exception()?;
        }
        if seen_exception != self.has_exceptions() {
            return Err(corrupt(
                "the exception flag disagrees with the class arrays",
            ));
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
        collations: &'p [rustdb_value::collation::Collation],
    ) -> LeafRef<'p> {
        self.collations = collations;
        self
    }

    /// Returns the collation of one key column.
    ///
    /// @param index - the key column's position
    pub fn collation_of(&self, index: usize) -> rustdb_value::collation::Collation {
        self.collations
            .get(index)
            .copied()
            .unwrap_or(rustdb_value::collation::Collation::Binary)
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

    /// Returns the number of rows in the delta area.
    pub fn delta_count(&self) -> usize {
        self.delta_count
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

    /// Reports whether the leaf is on the vectorised fast path.
    ///
    /// A leaf with no exceptions, no tombstones and no delta rows yields column
    /// vectors that borrow the page directly and need no merge, no filter and
    /// no per-row branch. This is the case the whole design optimises for and
    /// the case a freshly built or freshly compacted leaf is in.
    pub fn is_clean(&self) -> bool {
        !self.has_exceptions() && !self.has_tombstones() && self.delta_count == 0
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
            .checked_add(index.saturating_mul(DIRECTORY_ENTRY))
            .ok_or_else(|| corrupt("directory index overflows"))?;
        Ok(page::read_u32(self.page, entry.saturating_add(4))? as usize)
    }

    /// Returns the directory entry for one column.
    ///
    /// @param index - the column's position in the directory
    pub fn spec(&self, index: usize) -> DbResult<ColumnSpec> {
        if index >= self.column_count {
            return Err(misuse(format!("column {index} does not exist")));
        }
        let entry = leaf_header::DIRECTORY
            .checked_add(index.saturating_mul(DIRECTORY_ENTRY))
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
            collation: rustdb_value::collation::Collation::Binary,
        })
    }

    /// Returns a view over one mini-column.
    ///
    /// @param index - the column's position in the directory
    pub fn column(&self, index: usize) -> DbResult<MiniColumn<'p>> {
        let spec = self.spec(index)?;
        let start = self.column_offset(index)?;
        let class_len = class_bytes(self.row_count);
        let value_len = self.row_count.saturating_mul(spec.physical.slot_width());
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
            class,
            values,
            rows: self.row_count,
            page: self.page,
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

    /// Walks the delta area, proving every row decodes and stops where it says.
    fn validate_delta(&self) -> DbResult<()> {
        let mut at = self.delta_start;
        for index in 0..self.delta_count {
            let length = page::read_u16(self.page, at)? as usize;
            let start = at.saturating_add(2);
            let end = start.saturating_add(length);
            if end > self.heap_start {
                return Err(corrupt(format!("delta row {index} runs into the heap")));
            }
            let row = self
                .page
                .get(start..end)
                .ok_or_else(|| corrupt("delta row runs past the page"))?;
            let mut cursor = 0usize;
            for column in 0..self.column_count {
                let (_, used) =
                    Datum::decode_tagged(row.get(cursor..).unwrap_or(&[])).map_err(|_| {
                        corrupt(format!("delta row {index} column {column} is corrupt"))
                    })?;
                cursor = cursor.saturating_add(used);
            }
            if cursor != length {
                return Err(corrupt(format!(
                    "delta row {index} declares {length} bytes and decodes {cursor}"
                )));
            }
            at = end;
        }
        Ok(())
    }

    /// Returns the bytes of one delta row.
    ///
    /// @param index - the row's position in the delta area
    pub fn delta_row(&self, index: usize) -> DbResult<&'p [u8]> {
        if index >= self.delta_count {
            return Err(misuse(format!("delta row {index} does not exist")));
        }
        let mut at = self.delta_start;
        for _ in 0..index {
            let length = page::read_u16(self.page, at)? as usize;
            at = at.saturating_add(2).saturating_add(length);
        }
        let length = page::read_u16(self.page, at)? as usize;
        let start = at.saturating_add(2);
        self.page
            .get(start..start.saturating_add(length))
            .ok_or_else(|| corrupt("delta row runs past the page"))
    }

    /// Returns one value of one delta row.
    ///
    /// @param index - the row's position in the delta area
    /// @param column - which column to decode
    pub fn delta_value(&self, index: usize, column: usize) -> DbResult<Datum<'p>> {
        let row = self.delta_row(index)?;
        let mut cursor = 0usize;
        for position in 0..=column {
            let (value, used) = Datum::decode_tagged(row.get(cursor..).unwrap_or(&[]))?;
            if position == column {
                return Ok(value);
            }
            cursor = cursor.saturating_add(used);
        }
        // Unreachable: the loop runs `0..=column` and returns when `position`
        // reaches `column`, so it can only fall out of the bottom if the range
        // were empty, which an inclusive range never is. A column past the end
        // of the row fails earlier, in `decode_tagged`.
        Err(unreachable_branch("an inclusive range ran to its end"))
    }

    /// Returns one value of one sorted-region row.
    ///
    /// @param row - the row's position in the sorted region
    /// @param column - which column to decode
    pub fn value(&self, row: usize, column: usize) -> DbResult<Datum<'p>> {
        self.column(column)?.value(row)
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
            let order = crate::types::compare_under(&held, wanted, self.collation_of(index));
            if order != std::cmp::Ordering::Equal {
                return Ok(order);
            }
        }
        Ok(std::cmp::Ordering::Equal)
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
            if let Some(target) = self.integer_key_probe(probe)? {
                return self.search_integer_key(target);
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
    fn integer_key_probe(&self, probe: &[Datum<'_>]) -> DbResult<Option<i64>> {
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
        Ok(Some(*target))
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
    fn search_integer_key(&self, target: i64) -> DbResult<Result<usize, usize>> {
        /// How many interpolation steps before giving up and bisecting.
        ///
        /// Four, because a distribution that has not converged in four steps is
        /// not one interpolation is going to help with, and because bounding it
        /// is what makes the worst case no worse than the search it replaces.
        const STEPS: usize = 4;

        let column = self.column(0)?;
        let values = column.inline_bytes();
        let read = |row: usize| -> DbResult<i64> {
            let at = row.saturating_mul(8);
            let slice = values
                .get(at..at.saturating_add(8))
                .ok_or_else(|| corrupt(format!("row {row} is past the key column")))?;
            let mut raw = [0u8; 8];
            raw.copy_from_slice(slice);
            Ok(i64::from_le_bytes(raw))
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
        if self.collation_of(0) != rustdb_value::collation::Collation::Binary {
            return Ok(None);
        }
        Ok(Some(IntegerGuide {
            values: column.inline_bytes(),
            target: *target,
        }))
    }

    /// Materialises every live row, sorted region merged with the delta.
    ///
    /// This is what compaction, the property tests and the slow scan path all
    /// need, and it is deliberately the one place the merge is written.
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
        for index in 0..self.delta_count {
            let mut values = Vec::with_capacity(self.column_count);
            for column in 0..self.column_count {
                values.push(self.delta_value(index, column)?);
            }
            rows.push(values);
        }
        let key_columns = self.key_columns;
        rows.sort_by(|left, right| compare_rows(left, right, key_columns));
        Ok(rows)
    }
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
    for index in 0..key_columns {
        let (Some(a), Some(b)) = (left.get(index), right.get(index)) else {
            return std::cmp::Ordering::Equal;
        };
        let order = a.compare(b);
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

/// A view over one column's class array and value slots.
#[derive(Clone, Copy, Debug)]
pub struct MiniColumn<'p> {
    /// The layout of the value slots.
    pub physical: PhysicalType,
    /// The directory entry's flag byte.
    pub flags: u8,
    /// Two bits per row.
    pub class: &'p [u8],
    /// `rows * physical.slot_width()` bytes.
    pub values: &'p [u8],
    /// How many rows the column holds.
    pub rows: usize,
    /// The whole page, because text and blob slots address it absolutely.
    page: &'p [u8],
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

    /// Returns the integer in one slot, without consulting the class array.
    ///
    /// @param row - the row's position in the sorted region
    pub fn int_unchecked(&self, row: usize) -> DbResult<i64> {
        let at = row.saturating_mul(8);
        let slice = self
            .values
            .get(at..at.saturating_add(8))
            .ok_or_else(|| corrupt(format!("row {row} is outside the value array")))?;
        Ok(i64::from_le_bytes(slice.try_into().unwrap_or([0; 8])))
    }

    /// Returns one row's value, consulting the class array.
    ///
    /// @param row - the row's position in the sorted region
    pub fn value(&self, row: usize) -> DbResult<Datum<'p>> {
        match self.class_at(row)? {
            ValueClass::Null => Ok(Datum::Null),
            ValueClass::Exception => {
                let offset = self.slot_u32(row)? as usize;
                let (value, _) = Datum::decode_tagged(self.page.get(offset..).unwrap_or(&[]))?;
                Ok(value)
            }
            ValueClass::Typed => match self.physical {
                PhysicalType::Int64 => Ok(Datum::Int(self.int_unchecked(row)?)),
                PhysicalType::Float64 => {
                    Ok(Datum::Real(f64::from_bits(self.int_unchecked(row)? as u64)))
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
                    let offset = self.slot_u32(row)? as usize;
                    let (value, _) = Datum::decode_tagged(self.page.get(offset..).unwrap_or(&[]))?;
                    Ok(value)
                }
            },
        }
    }

    /// Returns the `(offset, length)` heap slice one variable-width slot names.
    ///
    /// @param row - the row's position in the sorted region
    pub fn heap_slice(&self, row: usize) -> DbResult<&'p [u8]> {
        let at = row.saturating_mul(8);
        let slice = self
            .values
            .get(at..at.saturating_add(8))
            .ok_or_else(|| corrupt(format!("row {row} is outside the value array")))?;
        let mut offset_raw = [0u8; 4];
        let mut length_raw = [0u8; 4];
        offset_raw.copy_from_slice(slice.get(..4).unwrap_or(&[0; 4]));
        length_raw.copy_from_slice(slice.get(4..).unwrap_or(&[0; 4]));
        let offset = u32::from_le_bytes(offset_raw) as usize;
        let length = u32::from_le_bytes(length_raw) as usize;
        self.page
            .get(offset..offset.saturating_add(length))
            .ok_or_else(|| corrupt(format!("row {row}'s heap slice runs past the page")))
    }

    /// Returns the `u32` in the low half of one slot.
    ///
    /// @param row - the row's position in the sorted region
    fn slot_u32(&self, row: usize) -> DbResult<u32> {
        let width = self.physical.slot_width();
        let at = row.saturating_mul(width);
        let slice = self
            .values
            .get(at..at.saturating_add(4))
            .ok_or_else(|| corrupt(format!("row {row} is outside the value array")))?;
        let mut raw = [0u8; 4];
        raw.copy_from_slice(slice);
        Ok(u32::from_le_bytes(raw))
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
    /// Not even one row fits, which means a row is larger than a page and
    /// belongs in a blob extent (Phase 4).
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
    pub fn pack(&self, rows: &[Vec<Datum<'_>>], fill: f64) -> DbResult<Packed> {
        let budget = ((self.page_size as f64) * fill.clamp(0.05, 1.0)) as usize;
        // Binary search for the largest prefix that fits: the encoded size is
        // monotone in the row count, so this is log(n) encodes rather than n.
        let mut low = 0usize;
        let mut high = rows.len();
        while low < high {
            let middle = low.saturating_add(high.saturating_sub(low).saturating_add(1) / 2);
            match self.encoded_size(rows.get(..middle).unwrap_or(&[]))? {
                Some(size) if size <= budget => low = middle,
                _ => high = middle.saturating_sub(1),
            }
        }
        if low == 0 {
            return Ok(Packed::RowTooLarge);
        }
        let page = self.encode(rows.get(..low).unwrap_or(&[]))?;
        Ok(Packed::Filled { page, rows: low })
    }

    /// Returns the bytes the given rows would occupy, or `None` if they cannot
    /// be laid out at all.
    ///
    /// @param rows - the rows to measure
    fn encoded_size(&self, rows: &[Vec<Datum<'_>>]) -> DbResult<Option<usize>> {
        let count = rows.len();
        let mut fixed = leaf_header::DIRECTORY
            .saturating_add(self.columns.len().saturating_mul(DIRECTORY_ENTRY));
        for column in &self.columns {
            fixed = fixed
                .saturating_add(class_bytes(count))
                .saturating_add(count.saturating_mul(column.physical.slot_width()));
            // Every mini-column starts 8-byte aligned; the class array is
            // already a multiple of eight and so is an inline value array, but
            // an `Any` array of an odd row count is not.
            fixed = align8(fixed);
        }
        let mut heap = 0usize;
        for row in rows {
            for (index, column) in self.columns.iter().enumerate() {
                let value = row.get(index).copied().unwrap_or(Datum::Null);
                heap = heap.saturating_add(heap_cost(column.physical, &value));
            }
        }
        Ok(Some(fixed.saturating_add(heap)))
    }

    /// Encodes the rows into a page.
    ///
    /// @param rows - the rows to encode, sorted by key
    pub fn encode(&self, rows: &[Vec<Datum<'_>>]) -> DbResult<Vec<u8>> {
        let count = rows.len();
        if count > u16::MAX as usize {
            return Err(misuse("a leaf cannot hold more than 65535 rows"));
        }
        let mut page = vec![0u8; self.page_size];
        page::write_common(&mut page, PageKind::Leaf, 0, self.tree)?;

        // Lay the mini-columns out first so the directory can name them.
        let mut offsets = Vec::with_capacity(self.columns.len());
        let mut at = align8(
            leaf_header::DIRECTORY
                .saturating_add(self.columns.len().saturating_mul(DIRECTORY_ENTRY)),
        );
        for column in &self.columns {
            offsets.push(at);
            at = at
                .saturating_add(class_bytes(count))
                .saturating_add(count.saturating_mul(column.physical.slot_width()));
            at = align8(at);
        }
        if at > self.page_size {
            return Err(misuse("the mini-columns do not fit in one page"));
        }

        // The heap grows down from the page end. Every variable-width value and
        // every exception is appended to it as the columns are written.
        let mut heap_end = self.page_size;
        let mut has_exceptions = false;

        for (index, column) in self.columns.iter().enumerate() {
            let base = offsets.get(index).copied().unwrap_or(0);
            let values_at = base.saturating_add(class_bytes(count));
            for (row, values) in rows.iter().enumerate() {
                let value = values.get(index).copied().unwrap_or(Datum::Null);
                let class = classify(column.physical, &value);
                if class == ValueClass::Exception {
                    has_exceptions = true;
                }
                set_class(&mut page, base, row, class)?;
                let slot =
                    values_at.saturating_add(row.saturating_mul(column.physical.slot_width()));
                match class {
                    ValueClass::Null => {}
                    ValueClass::Typed => match column.physical {
                        PhysicalType::Int64 => {
                            page::write_u64(&mut page, slot, value.as_int().unwrap_or(0) as u64)?
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
                            page::write_u32(&mut page, slot, heap_end as u32)?;
                            page::write_u32(&mut page, slot.saturating_add(4), bytes.len() as u32)?;
                        }
                        PhysicalType::Any => {
                            heap_end = write_tagged(&mut page, heap_end, &value)?;
                            page::write_u32(&mut page, slot, heap_end as u32)?;
                        }
                    },
                    ValueClass::Exception => {
                        heap_end = write_tagged(&mut page, heap_end, &value)?;
                        page::write_u32(&mut page, slot, heap_end as u32)?;
                    }
                }
            }
        }

        if heap_end < at {
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
        let low_fence = rows
            .first()
            .and_then(|row| row.first().copied())
            .and_then(|value| value.as_int())
            .unwrap_or(0);
        page::write_u64(&mut page, leaf_header::LOW_FENCE, low_fence as u64)?;
        for (index, column) in self.columns.iter().enumerate() {
            let entry =
                leaf_header::DIRECTORY.saturating_add(index.saturating_mul(DIRECTORY_ENTRY));
            let type_slot = page
                .get_mut(entry)
                .ok_or_else(|| misuse("the directory does not fit"))?;
            *type_slot = column.physical.code();
            let flag_slot = page
                .get_mut(entry.saturating_add(1))
                .ok_or_else(|| misuse("the directory does not fit"))?;
            *flag_slot = if index < self.key_columns {
                column.flags | COLUMN_KEY
            } else {
                column.flags & !COLUMN_KEY
            };
            page::write_u16(
                &mut page,
                entry.saturating_add(2),
                column.physical.slot_width() as u16,
            )?;
            page::write_u32(
                &mut page,
                entry.saturating_add(4),
                offsets.get(index).copied().unwrap_or(0) as u32,
            )?;
        }
        if has_exceptions {
            let flags = page
                .get_mut(header::FLAGS)
                .ok_or_else(|| misuse("the page has no flag byte"))?;
            *flags |= LEAF_HAS_EXCEPTIONS;
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

/// Returns the class a value takes in a column of the given physical type.
///
/// @param physical - the column's layout
/// @param value - the value to classify
fn classify(physical: PhysicalType, value: &Datum<'_>) -> ValueClass {
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
fn heap_cost(physical: PhysicalType, value: &Datum<'_>) -> usize {
    match classify(physical, value) {
        ValueClass::Null => 0,
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
            for column in 0..5 {
                let got = leaf.value(index, column).unwrap();
                assert_eq!(
                    got.compare(&row[column]),
                    std::cmp::Ordering::Equal,
                    "row {index} column {column}: {got:?} vs {:?}",
                    row[column]
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
                // Adding one more row must not have fit inside the budget.
                let bigger = builder.encoded_size(&rows[..packed + 1]).unwrap().unwrap();
                assert!(bigger > (8192.0 * 0.9) as usize, "{bigger}");
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
        let mut end = leaf_header::DIRECTORY + columns * DIRECTORY_ENTRY;
        for index in 0..columns {
            let spec = leaf.spec(index).unwrap();
            end = align8(end);
            end += class_bytes(count) + count * spec.physical.slot_width();
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

        // Enough rows that the mini-columns alone overflow the page.
        let narrow =
            LeafBuilder::new(8_192, 1, vec![ColumnSpec::key(PhysicalType::Int64)], 1).unwrap();
        let wide: Vec<Vec<Datum<'static>>> =
            (0..2_000).map(|n| vec![Datum::Int(n as i64)]).collect();
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
        assert_eq!(leaf.column(1).unwrap().inline_bytes().len(), 5 * 8);
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

        let empty = builder.encode(&[]).unwrap();
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
        page::write_u16(&mut lying_count, leaf_header::ROW_COUNT, 4_000).unwrap();
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
        // Sixteen rows of five hundred bytes: the mini-columns are small, so
        // the heap runs down into them rather than off the end of the page.
        let long = vec![b'z'; 500];
        let rows: Vec<Vec<Datum<'_>>> = (0..16)
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
                collation: rustdb_value::collation::Collation::Binary,
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
