//! The leaf's delta area: rows a write staged since the page was last packed,
//! read in the order they arrive rather than found by search.
//!
//! Invariant: **a delta row is decoded left to right, once, and never re-read
//! from its first byte to answer a later column.** A delta row has no index of
//! its own - it is a run of tagged values, one per column - so the only way to
//! reach column `k` is to walk past columns `0..k` first. [`LeafRef::delta_value`]
//! pays that walk once per call, which is fine for the one caller who wants a
//! single column; the two callers who want the whole row -
//! [`LeafRef::delta_row_values`] and [`LeafRef::row_key_matches`] - both go
//! through [`LeafRef::delta_column_at`] instead, carrying the cursor forward
//! themselves so the row is walked once rather than once per column asked for.
//! [`LeafRef::row_key_matches`]'s own doc comment carries the measurement
//! this was worth.
//!
//! What lives here is the delta area on its own: its directory
//! ([`LeafRef::delta_count`], [`LeafRef::delta_start`]), one row's bytes
//! ([`LeafRef::delta_row`]), and every reader that starts from a row once it
//! has them - a single value, a whole row, a key comparison, an out-of-line
//! reference, or the walk [`LeafRef::parse`] runs once per page to prove every
//! row decodes. [`LeafRef::locate`], [`LeafRef::live`] and
//! [`LeafRef::live_source`] stay in [`super`] rather than moving here: each
//! reads the sorted region and the delta area together and picking one of the
//! two a home for them would be arbitrary.

use inillucent_base::error::{corrupt, misuse};
use inillucent_base::DbResult;

use inillucent_pool::extent::ExtentRef;

use crate::datum::Datum;
use crate::page;

use super::{unreachable_branch, LeafRef};

impl<'p> LeafRef<'p> {
    /// Returns the number of rows in the delta area.
    pub fn delta_count(&self) -> usize {
        self.delta_count
    }

    /// Returns where the delta area begins.
    ///
    /// Exposed for [`crate::mutate`], which grows the area downwards and needs
    /// to know where it currently starts. A reader has no use for it - every
    /// delta accessor takes an index.
    pub fn delta_start(&self) -> usize {
        self.delta_start
    }

    /// Walks the delta area, proving every row decodes and stops where it says.
    pub(super) fn validate_delta(&self) -> DbResult<()> {
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
                let rest = row.get(cursor..).unwrap_or(&[]);
                let used = Datum::tagged_span(rest).map_err(|_| {
                    corrupt(format!("delta row {index} column {column} is corrupt"))
                })?;
                // An out-of-line delta value is checked the way the sorted
                // region's are: the reference has to decode and name a page.
                if Datum::tag_of(rest)? == crate::datum::tag::EXTENT {
                    ExtentRef::decode(rest.get(1..).unwrap_or(&[]))?;
                }
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

    /// Returns the out-of-line reference a delta value names, if it names one.
    ///
    /// @param index - the row's position in the delta area
    /// @param column - which column
    pub fn delta_extent_at(&self, index: usize, column: usize) -> DbResult<Option<ExtentRef>> {
        let row = self.delta_row(index)?;
        let mut cursor = 0usize;
        for position in 0..=column {
            let rest = row.get(cursor..).unwrap_or(&[]);
            if position == column {
                if Datum::tag_of(rest)? != crate::datum::tag::EXTENT {
                    return Ok(None);
                }
                return ExtentRef::decode(rest.get(1..).unwrap_or(&[])).map(Some);
            }
            cursor = cursor.saturating_add(Datum::tagged_span(rest)?);
        }
        Err(unreachable_branch("an inclusive range ran to its end"))
    }

    /// The same, without believing the flag.
    ///
    /// For [`crate::mutate::LeafMut::remove_delta`], which is deciding what the
    /// flag should say and so cannot start from what it does say.
    pub fn any_delta_extent_unchecked_pub(&self) -> DbResult<bool> {
        self.any_delta_extent_unchecked()
    }

    /// Reports whether one delta row holds an out-of-line value.
    ///
    /// @param index - the row's position in the delta area
    pub fn delta_extents_in(&self, index: usize) -> DbResult<bool> {
        let row = self.delta_row(index)?;
        let mut cursor = 0usize;
        while cursor < row.len() {
            let rest = row.get(cursor..).unwrap_or(&[]);
            if Datum::tag_of(rest)? == crate::datum::tag::EXTENT {
                return Ok(true);
            }
            cursor = cursor.saturating_add(Datum::tagged_span(rest)?);
        }
        Ok(false)
    }

    /// The same, without believing the flag.
    ///
    /// The integrity check asks it, because what it is checking *is* the flag:
    /// a reader that trusted it would agree with itself and find nothing.
    pub(super) fn any_delta_extent_unchecked(&self) -> DbResult<bool> {
        for index in 0..self.delta_count {
            let row = self.delta_row(index)?;
            let mut cursor = 0usize;
            while cursor < row.len() {
                let rest = row.get(cursor..).unwrap_or(&[]);
                if Datum::tag_of(rest)? == crate::datum::tag::EXTENT {
                    return Ok(true);
                }
                cursor = cursor.saturating_add(Datum::tagged_span(rest)?);
            }
        }
        Ok(false)
    }

    /// Returns the bytes of one delta row.
    ///
    /// **Finding row `index` costs a walk from the first row**, because the
    /// area is a run of rows, each with its length in front of it, and it has
    /// no directory of its own. A caller that
    /// wants one row pays that walk once and it is what this is for; a caller
    /// that wants every row in turn must use [`LeafRef::delta_row_from`]
    /// instead, or the area is walked `index` times to visit row `index` and
    /// the loop is quadratic in the delta count - see that function.
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
        let (row, _) = self.delta_row_from(at)?;
        Ok(row)
    }

    /// Returns the delta row that starts at an offset, and where the next one starts.
    ///
    /// **How a caller visits every delta row without walking the area once per
    /// row** (task-2034). The area has no directory of its own, so
    /// [`LeafRef::delta_row`]
    /// reaches row `index` by reading and skipping the `index` rows ahead of
    /// it; a loop over every row built out of that call reads
    /// `delta_count`squared over two lengths to visit `delta_count` rows. In
    /// [`LeafRef::locate`] that loop is on the write path, and it is run on
    /// every write and every delete, against a delta area holding up to
    /// [`crate::leaf::DELTA_LIMIT`] rows. Measured on `extension.fts.build`, the
    /// delta scan was 0.63 of the 3.3 microseconds a write with no compaction
    /// costs - the largest single stage of that write.
    ///
    /// The first offset is [`LeafRef::delta_start`]. The caller stops after
    /// `delta_count` rows rather than at a sentinel, which is what the count in
    /// the header is for.
    ///
    /// @param at - the offset of the row's length, which is the two bytes in front of it
    /// @returns the row's bytes, and the offset of the next row's prefix
    pub fn delta_row_from(&self, at: usize) -> DbResult<(&'p [u8], usize)> {
        let length = page::read_u16(self.page, at)? as usize;
        let start = at.saturating_add(2);
        let next = start.saturating_add(length);
        let row = self
            .page
            .get(start..next)
            .ok_or_else(|| corrupt("delta row runs past the page"))?;
        Ok((row, next))
    }

    /// Decodes the value that starts at `cursor` inside an already-fetched delta row.
    ///
    /// **The one place a delta value's bytes are turned into a `Datum`, shared
    /// by every reader that walks a delta row** - one column at a time in
    /// [`LeafRef::delta_value`], or left to right in [`LeafRef::delta_row_values`]
    /// and [`LeafRef::row_key_matches`]. What a tagged value at an offset
    /// means does not depend on how the caller reached that offset, and the one
    /// case that is not a plain decode - an out-of-line value, answered from the
    /// resolved extents rather than the seventeen placeholder bytes on the page,
    /// the same rule the sorted region's `MiniColumn::value` follows - only
    /// needs to be written once.
    ///
    /// @param row - the delta row's bytes, from [`LeafRef::delta_row`]
    /// @param cursor - the byte offset at which `column`'s value starts
    /// @param index - the row's position in the delta area, for the extents lookup
    /// @param column - which column this is
    fn delta_column_at(
        &self,
        row: &'p [u8],
        cursor: usize,
        index: usize,
        column: usize,
    ) -> DbResult<(Datum<'p>, usize)> {
        let rest = row.get(cursor..).unwrap_or(&[]);
        if Datum::tag_of(rest)? == crate::datum::tag::EXTENT {
            let bytes = self
                .extents
                .and_then(|held| held.get_delta(index, column))
                .ok_or_else(|| {
                    misuse(concat!(
                        "this value is stored out of line; read the leaf's extents ",
                        "through the tree first"
                    ))
                })?;
            // The reference says what the bytes are when it can, and the column
            // answers when it does not - the same rule the sorted region's
            // `MiniColumn::value` follows, for the same reason (task-1986).
            let reference = ExtentRef::decode(rest.get(1..).unwrap_or(&[]))?;
            let value =
                crate::leaf::extent_datum(reference.class, self.column(column)?.physical, bytes);
            let span = Datum::tagged_span(rest)?;
            return Ok((value, cursor.saturating_add(span)));
        }
        let (value, span) = Datum::decode_tagged(rest)?;
        Ok((value, cursor.saturating_add(span)))
    }

    /// Returns one value of one delta row.
    ///
    /// Skips to `column` by measuring the tagged span of every column before
    /// it, so a caller after one column pays for the columns ahead of it and
    /// nothing else. A caller after several - `locate` used to be one, calling
    /// this once per key column - pays for that skip again on every call; see
    /// `LeafRef::row_key_matches` and `LeafRef::delta_row_values` for the
    /// left-to-right walk that avoids it.
    ///
    /// @param index - the row's position in the delta area
    /// @param column - which column to decode
    pub fn delta_value(&self, index: usize, column: usize) -> DbResult<Datum<'p>> {
        let row = self.delta_row(index)?;
        let mut cursor = 0usize;
        for _ in 0..column {
            let rest = row.get(cursor..).unwrap_or(&[]);
            cursor = cursor.saturating_add(Datum::tagged_span(rest)?);
        }
        let (value, _) = self.delta_column_at(row, cursor, index, column)?;
        Ok(value)
    }

    /// Decodes every column of one delta row, left to right, in a single pass.
    ///
    /// The reader for a leaf's live rows - compaction, and every plain scan
    /// over a leaf that has been written to - used to build this same `Vec`
    /// with one [`LeafRef::delta_value`] call per column, which re-measured
    /// column zero's span for every later column: decoding a row of `w`
    /// columns cost the sum `1 + 2 + ... + w`, not `w`. This walks the row's
    /// cursor forward once, so each column's span is measured exactly once.
    ///
    /// @param index - the row's position in the delta area
    pub(super) fn delta_row_values(&self, index: usize) -> DbResult<Vec<Datum<'p>>> {
        let row = self.delta_row(index)?;
        let mut cursor = 0usize;
        let mut values = Vec::with_capacity(self.column_count);
        for column in 0..self.column_count {
            let (value, next) = self.delta_column_at(row, cursor, index, column)?;
            values.push(value);
            cursor = next;
        }
        Ok(values)
    }

    /// Returns whether a delta row's leading columns equal a probe key.
    ///
    /// **Measured as the cost of `write.insert.batch`, 0.50x against SQLite.**
    /// `locate` used to ask [`LeafRef::delta_value`] once per key column, and
    /// that function re-decodes a delta row from its first byte on every call
    /// - so comparing a two-column key cost 1 + 2 tagged decodes instead of 2,
    /// and the cost grows with the *square* of the key's width, not its width.
    /// `main_table` in the write gate carries two secondary indexes, so a
    /// batch insert pays this once for the primary key and once per index,
    /// three times a row. This walks the row's cursor forward once instead,
    /// column by column, exactly as [`LeafRef::key_view`] does for the sorted
    /// region's comparisons - and stops at the first column that differs, so a
    /// row that fails on its first column, the common case in an unsorted
    /// delta area, never pays to decode the rest of its key at all.
    ///
    /// The row's bytes are the parameter rather than its index, because the one
    /// caller - [`LeafRef::delta_index_of`] - is walking the area in one pass
    /// and already has them. Taking the index instead would make that caller
    /// walk the area again for every row, which is the defect that function
    /// exists to remove.
    ///
    /// @param row - the delta row's bytes, from [`LeafRef::delta_row_from`]
    /// @param index - the row's position in the delta area, for the extents lookup
    /// @param key - the probe key, one value per key column
    /// @param key_columns - how many leading columns form the key
    pub(super) fn row_key_matches(
        &self,
        row: &'p [u8],
        index: usize,
        key: &[Datum<'_>],
        key_columns: usize,
    ) -> DbResult<bool> {
        let mut cursor = 0usize;
        for column in 0..key_columns {
            let (held, next) = self.delta_column_at(row, cursor, index, column)?;
            cursor = next;
            let wanted = key.get(column).copied().unwrap_or(Datum::Null);
            if crate::types::compare_under(&held, &wanted, self.collation_of(column))
                != core::cmp::Ordering::Equal
            {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// Returns where a key sits in the delta area, walking the area once.
    ///
    /// **One pass, not one per row** (task-2034). The same search written as
    /// `for index in 0..delta_count { .. delta_row(index) .. }` reaches
    /// row `index` by walking past the `index` rows ahead of it, so finding
    /// nothing in a full delta area reads about five hundred lengths to compare
    /// 32 keys. This carries the offset forward instead, and the
    /// comparison it does per row is the same one.
    ///
    /// The newest entry for a key is the one with the lowest index, and this
    /// returns the first match, so the answer is unchanged.
    ///
    /// @param key - the probe key, one value per key column
    /// @param key_columns - how many leading columns form the key
    pub(super) fn delta_index_of(
        &self,
        key: &[Datum<'_>],
        key_columns: usize,
    ) -> DbResult<Option<usize>> {
        let mut at = self.delta_start;
        for index in 0..self.delta_count {
            let (row, next) = self.delta_row_from(at)?;
            if self.row_key_matches(row, index, key, key_columns)? {
                return Ok(Some(index));
            }
            at = next;
        }
        Ok(None)
    }
}
