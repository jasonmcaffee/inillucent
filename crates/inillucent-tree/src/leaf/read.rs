//! Taking a value back out of a leaf page.
//!
//! Invariant: **a read of a page that has been written to goes through the
//! merge.** A leaf with a delta area or a tombstone cannot be read as
//! mini-columns, and `needs_materialising` is the one question every reader
//! asks before choosing which path to take.

use inillucent_base::error::corrupt;
use inillucent_base::DbResult;
use inillucent_pool::extent::ExtentRef;

use crate::datum::Datum;
use crate::types::{ColumnSpec, PhysicalType, ValueClass};

use super::compare::*;
use super::encode::*;
use super::layout::*;
use super::*;

impl<'p> LeafRef<'p> {
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
    pub(crate) fn column_offset(&self, index: usize) -> DbResult<usize> {
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
    pub(crate) fn tombstones_start(&self) -> DbResult<usize> {
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

/// Reads one value of a live row through the derived views.
///
/// @param columns - the mini-columns
/// @param delta - the decoded delta rows
/// @param at - the row's position
/// @param column - which column
pub(crate) fn live_value<'p>(
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
