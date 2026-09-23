//! The leaf's delta area: rows a write staged since the page was last packed,
//! found through a directory kept in the tree's key order.
//!
//! Invariant: **the directory is in key order, and a delta index is a position
//! in the directory.** Entry `i` names the row with the `i`-th smallest key, and
//! where the area holds one key twice the newer row comes first. Every reader
//! that takes a delta index - a [`crate::leaf::Hit::Delta`], a
//! [`crate::write::Located::Delta`], the out-of-line values in
//! [`crate::leaf::Extents`] - means a directory position, so the order of the
//! rows' bytes in the page is nobody's business but this module's.
//!
//! ## The layout (format version 2, task-2074)
//!
//! ```text
//! delta_start                                   heap_start
//! | directory: delta_count u16 | rows ...        |
//! ```
//!
//! A directory entry holds the distance from `heap_start` back to its row's
//! two-byte length. Measured from the heap rather than from the page start
//! because [`crate::mutate::LeafMut`] moves the whole area down when it makes
//! heap room for a longer text, and a distance from the heap does not change
//! when it does. A row is a length followed by one tagged value per column, as
//! it was before the directory existed.
//!
//! ## A leaf format 1 wrote
//!
//! A leaf without [`super::LEAF_DELTA_DIRECTORY`] has no directory: its rows
//! start at `delta_start`, newest first, at most 32 of them, and a delta index
//! there means a position in that run. Every function below answers for both
//! layouts, so a database format 1 wrote reads unchanged. The two that differ
//! in kind rather than in arithmetic are the searches - a format 1 area is not
//! in key order, so it is scanned - and [`LeafRef::delta_in_key_order`], which
//! sorts a format 1 area's positions for the readers that merge.
//!
//! ## Why there is a directory at all
//!
//! **The area used to be a run of rows with no index, capped at 32 rows.** A
//! lookup walked it from the start and compared every row, so the cap was what
//! kept a write's `locate` cheap - and the cap was a count, so an index leaf
//! holding 3,704 packed rows compacted after every 32 writes exactly as a table
//! leaf holding 241 did. task-2066's audit put the two secondary indexes of
//! `write.insert.batch` at 69% of that workload for this reason (C1). With the
//! directory a lookup is a binary search and a compaction merges two sorted
//! runs, so nothing depends on the area being short, and the area is now as
//! large as the free gap lets it be.

use inillucent_base::error::{corrupt, misuse};
use inillucent_base::DbResult;

use inillucent_pool::extent::ExtentRef;

use crate::datum::Datum;
use crate::page;

use super::{LeafRef, DELTA_ENTRY};

#[cfg(test)]
mod tests;

impl<'p> LeafRef<'p> {
    /// Returns the number of rows in the delta area.
    pub fn delta_count(&self) -> usize {
        self.delta_count
    }

    /// Returns where the delta area begins, which is where its directory begins.
    ///
    /// Exposed for [`crate::mutate`], which grows the area downwards and needs
    /// to know where it currently starts. A reader has no use for it - every
    /// delta accessor takes an index.
    pub fn delta_start(&self) -> usize {
        self.delta_start
    }

    /// Returns where the delta area's rows begin, just past its directory.
    ///
    /// A format 1 leaf has no directory, so its rows begin at `delta_start`.
    pub fn delta_rows_start(&self) -> usize {
        match self.has_delta_directory() {
            true => self
                .delta_start
                .saturating_add(self.delta_count.saturating_mul(DELTA_ENTRY)),
            false => self.delta_start,
        }
    }

    /// Returns where one delta row's length prefix sits in the page.
    ///
    /// Checked against the rows region, so a directory entry that has been
    /// damaged is an error here rather than a row read out of the heap or the
    /// directory.
    ///
    /// @param index - the row's position in the directory
    pub fn delta_offset(&self, index: usize) -> DbResult<usize> {
        if index >= self.delta_count {
            return Err(misuse(format!("delta row {index} does not exist")));
        }
        if !self.has_delta_directory() {
            // Format 1: walk past the rows ahead of it. At most 32 of them.
            let mut at = self.delta_start;
            for _ in 0..index {
                let length = page::read_u16(self.page, at)? as usize;
                at = at.saturating_add(2).saturating_add(length);
                if at >= self.heap_start {
                    return Err(corrupt(format!("delta row {index} runs into the heap")));
                }
            }
            return Ok(at);
        }
        let entry = self
            .delta_start
            .saturating_add(index.saturating_mul(DELTA_ENTRY));
        let distance = page::read_u16(self.page, entry)? as usize;
        let Some(at) = self.heap_start.checked_sub(distance) else {
            return Err(corrupt(format!(
                "delta row {index} is {distance} bytes from a heap that starts at {}",
                self.heap_start
            )));
        };
        if at < self.delta_rows_start() || distance < 2 {
            return Err(corrupt(format!(
                "delta row {index} lies outside the delta area"
            )));
        }
        Ok(at)
    }

    /// Returns the bytes of one delta row.
    ///
    /// One directory read and one length read, whatever the index. Before the
    /// directory this walked every row ahead of `index`, and a loop over the
    /// area built out of it was quadratic in the delta count (task-2034).
    ///
    /// @param index - the row's position in the directory
    pub fn delta_row(&self, index: usize) -> DbResult<&'p [u8]> {
        self.delta_row_at(self.delta_offset(index)?)
    }

    /// Returns the delta row whose length prefix sits at an offset.
    ///
    /// @param at - the offset of the row's two-byte length
    fn delta_row_at(&self, at: usize) -> DbResult<&'p [u8]> {
        let length = page::read_u16(self.page, at)? as usize;
        let start = at.saturating_add(2);
        let end = start.saturating_add(length);
        if end > self.heap_start {
            return Err(corrupt("a delta row runs into the heap"));
        }
        self.page
            .get(start..end)
            .ok_or_else(|| corrupt("delta row runs past the page"))
    }

    /// Walks the delta area, proving every row decodes and the directory is sound.
    ///
    /// **Part of [`LeafRef::integrity`], not of [`LeafRef::parse`].** It ran on
    /// every parse while the area held at most 32 rows. Now the area is sized
    /// by the free gap and holds hundreds, and a parse happens on every level
    /// of every descent - so the walk moved to the check that is allowed to
    /// cost time in proportion to the page. A reader stays safe without it:
    /// [`LeafRef::delta_offset`] and [`LeafRef::delta_row_at`] bound every row
    /// they hand out, and every decode returns an error rather than reading past
    /// what it was given.
    ///
    /// Three things are checked. Every row decodes into exactly one value per
    /// column and ends where its length says. The rows are the whole of the
    /// rows region: they do not overlap, and their lengths add up to its size,
    /// so nothing in it is unaccounted for. And the directory is in key order
    /// under the leaf's collations and directions, which is what every binary
    /// search over it assumes.
    pub(super) fn validate_delta(&self) -> DbResult<()> {
        let mut covered = 0usize;
        let mut starts: Vec<usize> = Vec::with_capacity(self.delta_count);
        for index in 0..self.delta_count {
            let at = self.delta_offset(index)?;
            let row = self.delta_row_at(at)?;
            starts.push(at);
            covered = covered.saturating_add(row.len()).saturating_add(2);
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
            if cursor != row.len() {
                return Err(corrupt(format!(
                    "delta row {index} declares {} bytes and decodes {cursor}",
                    row.len()
                )));
            }
        }
        let region = self.heap_start.saturating_sub(self.delta_rows_start());
        starts.sort_unstable();
        starts.dedup();
        if covered != region || starts.len() != self.delta_count {
            return Err(corrupt(format!(
                "the delta rows cover {covered} bytes of a {region}-byte area"
            )));
        }
        // A format 1 area is in arrival order, so there is no order to check.
        if !self.has_delta_directory() {
            return Ok(());
        }
        for index in 1..self.delta_count {
            let key = self.delta_key(index)?;
            if self.compare_delta_key(index.saturating_sub(1), &key)? == std::cmp::Ordering::Greater
            {
                return Err(corrupt(format!(
                    "delta row {index} sorts before the row ahead of it in the directory"
                )));
            }
        }
        Ok(())
    }

    /// Returns one delta row's key columns.
    ///
    /// A key column is never stored out of line, so this needs no extents.
    ///
    /// @param index - the row's position in the directory
    pub(super) fn delta_key(&self, index: usize) -> DbResult<Vec<Datum<'p>>> {
        let row = self.delta_row(index)?;
        let mut cursor = 0usize;
        let mut key = Vec::with_capacity(self.key_columns);
        for column in 0..self.key_columns {
            let (value, next) = self.delta_column_at(row, cursor, index, column)?;
            key.push(value);
            cursor = next;
        }
        Ok(key)
    }

    /// Returns the out-of-line reference a delta value names, if it names one.
    ///
    /// @param index - the row's position in the directory
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
        Err(super::unreachable_branch(
            "an inclusive range ran to its end",
        ))
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
    /// @param index - the row's position in the directory
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
            if self.delta_extents_in(index)? {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Decodes the value that starts at `cursor` inside an already-fetched delta row.
    ///
    /// **The one place a delta value's bytes are turned into a `Datum`, shared
    /// by every reader that walks a delta row** - one column at a time in
    /// [`LeafRef::delta_value`], or left to right in [`LeafRef::delta_row_values`]
    /// and the key comparisons. What a tagged value at an offset means does not
    /// depend on how the caller reached that offset, and the one case that is
    /// not a plain decode - an out-of-line value, answered from the resolved
    /// extents rather than the seventeen placeholder bytes on the page, the same
    /// rule the sorted region's `MiniColumn::value` follows - only needs to be
    /// written once.
    ///
    /// @param row - the delta row's bytes, from [`LeafRef::delta_row`]
    /// @param cursor - the byte offset at which `column`'s value starts
    /// @param index - the row's position in the directory, for the extents lookup
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
    /// nothing else. A caller after several should use
    /// [`LeafRef::delta_row_values`], which walks the row once.
    ///
    /// @param index - the row's position in the directory
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
    /// @param index - the row's position in the directory
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

    /// Compares one delta row's key against a probe, in the tree's order.
    ///
    /// Only the columns the probe names are compared, so a probe shorter than
    /// the key compares as a prefix - the rule [`LeafRef::lower_bound`] follows
    /// over the sorted region. The row's cursor is carried forward column by
    /// column and the walk stops at the first column that differs, so a row that
    /// fails on its first column never pays to decode the rest of its key.
    ///
    /// @param index - the row's position in the directory
    /// @param probe - the key, one value per column it names
    pub(crate) fn compare_delta_key(
        &self,
        index: usize,
        probe: &[Datum<'_>],
    ) -> DbResult<std::cmp::Ordering> {
        let row = self.delta_row(index)?;
        let mut cursor = 0usize;
        for (column, wanted) in probe.iter().enumerate().take(self.key_columns) {
            let (held, next) = self.delta_column_at(row, cursor, index, column)?;
            cursor = next;
            let order = self.directed(
                crate::types::compare_under(&held, wanted, self.collation_of(column)),
                column,
            );
            if order != std::cmp::Ordering::Equal {
                return Ok(order);
            }
        }
        Ok(std::cmp::Ordering::Equal)
    }

    /// Binary-searches the directory for the first row whose key is not below a probe.
    ///
    /// `Ok(index)` when that row's key equals the probe on the columns the probe
    /// names, and `Err(index)` when it does not - `slice::binary_search`'s
    /// contract, with the difference that `Ok` is always the *first* equal row.
    /// That is the newest row for a key the area holds twice, and the start of
    /// the run for a prefix probe.
    ///
    /// @param probe - the key, one value per column it names
    pub fn delta_search(&self, probe: &[Datum<'_>]) -> DbResult<Result<usize, usize>> {
        if !self.has_delta_directory() {
            // Format 1: no order to search, so the first equal row, which is
            // the newest. `Err` carries no position, because nothing inserts
            // into a format 1 area by position.
            for index in 0..self.delta_count {
                if self.compare_delta_key(index, probe)? == std::cmp::Ordering::Equal {
                    return Ok(Ok(index));
                }
            }
            return Ok(Err(self.delta_count));
        }
        // **A probe past the last entry is answered by one comparison**
        // (task-2082). Appending at the right edge of a table probes a key
        // above every key the leaf holds, twice a row - the uniqueness check
        // and the write's own `locate` - and so does a lookup of a rowid past
        // the end. The area is as large as the free gap since task-2074, so
        // the rows an append leaves stay in it until the gap fills, where they
        // used to be packed every 32, and a binary search costs one decoded
        // key per halving. The gate's `txn.large` is the case that showed it:
        // `write.insert.autocommit` appends 100 rows to `side_table` earlier
        // in the round, and `Bind::Scatter` picks rowids up to `main_table`'s
        // row count while `side_table` holds a quarter of that, so three in
        // four of its updates match no row and probe past the end of
        // `side_table`'s last leaf. The last entry holds the largest key, so a
        // probe above it is past the whole area.
        let Some(last) = self.delta_count.checked_sub(1) else {
            return Ok(Err(0));
        };
        if self.compare_delta_key(last, probe)? == std::cmp::Ordering::Less {
            return Ok(Err(self.delta_count));
        }
        let mut low = 0usize;
        let mut high = last;
        while low < high {
            let middle = low.saturating_add(high.saturating_sub(low) / 2);
            match self.compare_delta_key(middle, probe)? {
                std::cmp::Ordering::Less => low = middle.saturating_add(1),
                _ => high = middle,
            }
        }
        if low < self.delta_count
            && self.compare_delta_key(low, probe)? == std::cmp::Ordering::Equal
        {
            return Ok(Ok(low));
        }
        Ok(Err(low))
    }

    /// Returns the delta rows whose key begins with a probe, in key order.
    ///
    /// For a leaf with a directory that is a run of it, found by two binary
    /// searches where the area used to be scanned in full once per probe. A
    /// format 1 area is scanned, and what matched is put in key order.
    ///
    /// @param probe - the key, one value per column it names
    pub fn delta_matching(&self, probe: &[Datum<'_>]) -> DbResult<Vec<usize>> {
        if !self.has_delta_directory() {
            let mut matched = Vec::new();
            for index in 0..self.delta_count {
                if self.compare_delta_key(index, probe)? == std::cmp::Ordering::Equal {
                    matched.push(index);
                }
            }
            return self.sorted_by_key(matched);
        }
        let begin = match self.delta_search(probe)? {
            Ok(at) | Err(at) => at,
        };
        let mut low = begin;
        let mut high = self.delta_count;
        while low < high {
            let middle = low.saturating_add(high.saturating_sub(low) / 2);
            match self.compare_delta_key(middle, probe)? {
                std::cmp::Ordering::Greater => high = middle,
                _ => low = middle.saturating_add(1),
            }
        }
        Ok((begin..low).collect())
    }

    /// Returns the first directory position whose key is not below a bound.
    ///
    /// With `past_equal` set, the first whose key is above it instead. The
    /// delta counterpart of `lower_bound` and `upper_bound` over the sorted
    /// region, comparing only the columns the bound names, which is what lets
    /// [`LeafRef::live_between`] take a run of the directory rather than
    /// merging the whole area. Only meaningful for a leaf with a directory: a
    /// format 1 area is not in key order and the caller has to scan it.
    ///
    /// @param probe - the bound, one value per column it names
    /// @param past_equal - whether a row equal to the bound is below it
    pub(crate) fn delta_bound(&self, probe: &[Datum<'_>], past_equal: bool) -> DbResult<usize> {
        let mut low = 0usize;
        let mut high = self.delta_count;
        while low < high {
            let middle = low.saturating_add(high.saturating_sub(low) / 2);
            let below = match self.compare_delta_key(middle, probe)? {
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

    /// Returns every delta row's position, in key order.
    ///
    /// The directory's own order for a leaf that has one. A format 1 area is
    /// sorted, stably, so where it holds one key twice the newer row - the
    /// lower position - still comes first, which is the rule every reader
    /// follows for that case.
    pub fn delta_in_key_order(&self) -> DbResult<Vec<usize>> {
        let all: Vec<usize> = (0..self.delta_count).collect();
        match self.has_delta_directory() {
            true => Ok(all),
            false => self.sorted_by_key(all),
        }
    }

    /// Sorts delta positions by their rows' keys, keeping equal keys in order.
    ///
    /// @param positions - the positions to sort
    fn sorted_by_key(&self, positions: Vec<usize>) -> DbResult<Vec<usize>> {
        let mut keyed: Vec<(Vec<Datum<'p>>, usize)> = Vec::with_capacity(positions.len());
        for index in positions {
            keyed.push((self.delta_key(index)?, index));
        }
        keyed.sort_by(|(left, _), (right, _)| self.compare_keys(left, right));
        Ok(keyed.into_iter().map(|(_, index)| index).collect())
    }

    /// Returns where a key sits in the delta area.
    ///
    /// The newest entry for a key is the first equal one in the directory, and
    /// that is the one this returns. Every key column is compared, and a key
    /// shorter than the key compares its missing columns as `NULL`, which is
    /// what the scan this replaced did.
    ///
    /// @param key - the probe key, one value per key column
    /// @param key_columns - how many leading columns form the key
    pub(super) fn delta_index_of(
        &self,
        key: &[Datum<'_>],
        key_columns: usize,
    ) -> DbResult<Result<usize, usize>> {
        if key.len() >= key_columns {
            return self.delta_search(key.get(..key_columns).unwrap_or(key));
        }
        let mut padded: Vec<Datum<'_>> = key.to_vec();
        padded.resize(key_columns, Datum::Null);
        self.delta_search(&padded)
    }
}
