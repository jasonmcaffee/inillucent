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
    /// Reports whether the leaf is in format 2's layout, with a delta directory.
    ///
    /// See [`LEAF_DELTA_DIRECTORY`]. `false` is a leaf format 1 wrote.
    pub fn has_delta_directory(&self) -> bool {
        self.flags & LEAF_DELTA_DIRECTORY != 0
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
    /// Returns one column's directory entry, eight or sixteen bytes.
    ///
    /// @param index - the column's position in the directory
    fn directory_entry(&self, index: usize) -> DbResult<&'p [u8]> {
        if index >= self.column_count {
            return Err(misuse(format!("column {index} does not exist")));
        }
        let entry = leaf_header::DIRECTORY
            .checked_add(index.saturating_mul(self.entry_size))
            .ok_or_else(|| corrupt("directory index overflows"))?;
        self.page
            .get(entry..entry.saturating_add(self.entry_size))
            .ok_or_else(|| corrupt("directory entry runs past the page"))
    }
    /// Returns a view over one mini-column.
    ///
    /// **The directory entry is read once, as one slice.** `column` runs on
    /// every probe - once for each key column the search compares and once for
    /// each inner column an index nested loop projects - and it used to ask
    /// `spec`, `column_width`, `column_base` and `column_offset` in turn, each
    /// of which located the same entry again, bounds checked it again, and
    /// between them parsed the type byte twice. `column_base`, the fourth
    /// lookup, is what task-1870's follow-ups added here. task-2091 measured
    /// reading the entry once, pinned, on the read gate: `join.range` 28.36 ms
    /// to 26.11, `range.lookaside` 28.3 to 24.5, and PointProbe 345 ns to 307,
    /// with every workload's digest still agreeing with SQLite's. Every answer
    /// is the same as the four calls give, including which error a bad entry
    /// produces; `a_column_reads_its_entry_the_way_the_accessors_do` checks it.
    ///
    /// @param index - the column's position in the directory
    pub fn column(&self, index: usize) -> DbResult<MiniColumn<'p>> {
        let entry = self.directory_entry(index)?;
        let head: [u8; 8] = entry
            .get(..8)
            .and_then(|head| head.try_into().ok())
            .ok_or_else(|| corrupt("directory entry runs past the page"))?;
        let [type_byte, flags, width_low, width_high, offset_0, offset_1, offset_2, offset_3] =
            head;
        let physical = PhysicalType::from_code(type_byte)?;
        let width = u16::from_le_bytes([width_low, width_high]) as usize;
        if !physical.admits_width(width) {
            return Err(corrupt(format!(
                "column {index} claims a slot width of {width}"
            )));
        }
        let base = match entry.get(8..16) {
            Some(tail) => {
                let raw: [u8; 8] = tail.try_into().unwrap_or([0; 8]);
                u64::from_le_bytes(raw) as i64
            }
            None => 0,
        };
        let start = u32::from_le_bytes([offset_0, offset_1, offset_2, offset_3]) as usize;
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
            physical,
            flags,
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
        Ok(self.locate_slot(key, key_columns)?.0)
    }

    /// Returns where a key sits, and the delta directory position a row for it takes.
    ///
    /// **The position is the one the directory will have after the write has
    /// displaced whatever the key named**, which is what the write path and
    /// recovery both hand to [`crate::mutate::LeafMut::plan_encoded`]. A key in
    /// the delta area at `i` is removed first and its replacement goes back in
    /// at `i`; a key anywhere else goes where the binary search stopped. Both
    /// come out of the one search, so a write pays for the delta area once.
    ///
    /// @param key - the key, one value per key column
    /// @param key_columns - how many leading columns form the key
    pub fn locate_slot(
        &self,
        key: &[Datum<'_>],
        key_columns: usize,
    ) -> DbResult<(crate::write::Located, usize)> {
        // **The two halves are timed apart because they answer to different
        // changes** (task-2034). The delta search and the sorted search are
        // both binary searches now, over a few hundred rows and a few thousand,
        // and a write path that costs a microsecond or two here cannot be aimed
        // at until it is known which of the two is paying. Both clocks are
        // `None` unless a harness asked - see `crate::stages`.
        let scanning = crate::stages::clock();
        let slot = match self.delta_index_of(key, key_columns)? {
            Ok(index) => {
                crate::stages::add_locate(crate::stages::elapsed(scanning), 0);
                return Ok((crate::write::Located::Delta(index), index));
            }
            Err(slot) => slot,
        };
        let deltas = crate::stages::elapsed(scanning);
        let searching = crate::stages::clock();
        // The tombstone is read inside the timed region because it is part of
        // deciding where the key sits: a tombstoned row reads as absent from
        // the sorted region, and the delta area has already been searched.
        let in_sorted_region = match self.search(key)? {
            Ok(row) => Some((row, self.is_tombstoned(row)?)),
            Err(_) => None,
        };
        crate::stages::add_locate(deltas, crate::stages::elapsed(searching));
        if let Some((row, false)) = in_sorted_region {
            return Ok((crate::write::Located::Sorted(row), slot));
        }
        Ok((crate::write::Located::Absent, slot))
    }
    /// Returns the leaf's live rows in key order, as a source the builder reads
    /// through.
    ///
    /// **The allocation-free half of [`LeafRef::live`], and the one a compaction
    /// wants.** It also does asymptotically less work: the sorted region and the
    /// delta directory are both in key order, so the two are **merged** rather
    /// than the whole leaf being sorted again.
    ///
    /// The shadowing rules are `live`'s, and the two are checked against each
    /// other by `live_order_agrees_with_live`.
    pub fn live_source(&self) -> DbResult<LiveSource<'p>> {
        self.live_order()?.materialise()
    }

    /// Returns which rows are live, in key order, without reading their values.
    ///
    /// **The half of [`LeafRef::live_source`] that does not depend on what the
    /// caller intends to do with the rows.** Deciding which rows are live is a
    /// tombstone scan, a decode of the delta rows and a merge; reading every
    /// value of every live row afterwards is a separate pass, and a caller that
    /// copies slots rather than values - the compaction splice - does not need
    /// it.
    ///
    /// **A merge of two sorted runs** (task-2074). The delta directory is in key
    /// order, so each delta row is placed by a binary search over the part of
    /// the sorted region not yet emitted, and the sorted rows below it are
    /// copied across in one step. That is `d log n` key comparisons and one pass
    /// over the positions. The version this replaced inserted each delta row
    /// into a vector of every sorted row, which moved the whole vector once per
    /// delta row - harmless at 32 delta rows and quadratic at the hundreds the
    /// area now holds - and it compared every delta row with every earlier one
    /// to find a key held twice, which the directory's order makes a comparison
    /// with the row before it (task-2066's audit, C6).
    ///
    /// The shadowing rules are the ones every reader follows: a tombstoned
    /// sorted row is not live; where the delta area holds a key twice the first
    /// in the directory, the newer, wins; and a delta row replaces the sorted
    /// row it shadows rather than joining it. `live_order_agrees_with_the_reference`
    /// checks them against a direct implementation of those three sentences.
    pub fn live_order(&self) -> DbResult<LiveOrder<'p>> {
        let mut columns = Vec::with_capacity(self.column_count);
        for index in 0..self.column_count {
            columns.push(self.column(index)?);
        }
        // Each delta row decoded once, through `delta_row_values`, and reused
        // by every comparison below.
        let mut delta: Vec<Vec<Datum<'p>>> = Vec::with_capacity(self.delta_count);
        for index in 0..self.delta_count {
            delta.push(self.delta_row_values(index)?);
        }
        let order = self.merge_order(&delta)?;
        Ok(LiveOrder {
            columns,
            delta,
            order,
            width: self.column_count,
        })
    }

    /// Merges the delta rows into the sorted region, by position.
    ///
    /// Reads only the rows' keys - the first `key_columns` values of each entry
    /// of `delta`, which may hold more - so a caller that has decoded nothing but
    /// the keys can call it, and a delta row holding an out-of-line value it has
    /// no use for is never asked for that value.
    ///
    /// @param delta - each delta row's values, at least its keys, by delta index
    fn merge_order(&self, delta: &[Vec<Datum<'p>>]) -> DbResult<Vec<LiveRow>> {
        let tombstones = match self.has_tombstones() {
            true => Some(self.tombstones()?),
            false => None,
        };
        let view = self.key_view()?;
        let mut order: Vec<LiveRow> =
            Vec::with_capacity(self.row_count.saturating_add(self.delta_count));
        let mut next_sorted = 0usize;
        let mut previous: Option<usize> = None;
        // The directory's own order, or a format 1 area sorted into it.
        for index in self.delta_in_key_order()? {
            let Some(values) = delta.get(index) else {
                continue;
            };
            let key = values.get(..self.key_columns).unwrap_or(values);
            // A key the directory holds twice keeps its first entry, which is
            // the newer; the second sorts straight after it.
            if let Some(earlier) = previous.and_then(|at| delta.get(at)) {
                if self.compare_keys(earlier, values) == std::cmp::Ordering::Equal {
                    continue;
                }
            }
            previous = Some(index);
            let (at, shadows) =
                match self.search_between(&view, key, next_sorted, self.row_count)? {
                    Ok(row) => (row, true),
                    Err(row) => (row, false),
                };
            push_live_sorted(&mut order, tombstones, next_sorted, at)?;
            order.push(LiveRow::Delta(index as u32));
            // The sorted row a delta row shadows is skipped whether or not it
            // was tombstoned: the delta row is the newer copy of that key.
            next_sorted = if shadows { at.saturating_add(1) } else { at };
        }
        push_live_sorted(&mut order, tombstones, next_sorted, self.row_count)?;
        Ok(order)
    }
    /// Materialises every live row, sorted region merged with the delta.
    ///
    /// This is what the property tests and every read path over a leaf that has
    /// been written to need. A leaf that has *not* been written to never comes
    /// here: [`LeafRef::has_writes`] is false and the caller takes the
    /// vectorised path, which is the whole design.
    ///
    /// **It is [`LeafRef::live_order`] with the values read out, and that is
    /// deliberate** (task-2074). It used to be its own merge: every delta row
    /// was compared with every sorted row to find the one it shadowed, and the
    /// result was sorted again at the end - quadratic in a delta area that is
    /// now sized by the free gap rather than capped at 32 rows. One merge is
    /// one set of shadowing rules, and two were two chances for them to drift.
    pub fn live(&self) -> DbResult<Vec<Vec<Datum<'p>>>> {
        let order = self.live_order()?;
        let mut rows: Vec<Vec<Datum<'p>>> = Vec::with_capacity(order.len());
        for at in order.order() {
            if let LiveRow::Delta(index) = at {
                if let Some(values) = order.delta().get(*index as usize) {
                    rows.push(values.clone());
                    continue;
                }
            }
            let mut values = Vec::with_capacity(order.width());
            for column in 0..order.width() {
                values.push(live_value(order.columns(), order.delta(), *at, column)?);
            }
            rows.push(values);
        }
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
        let projected_columns: Vec<MiniColumn<'p>> = columns
            .iter()
            .map(|column| self.column(*column))
            .collect::<DbResult<Vec<MiniColumn<'p>>>>()?;
        let mut projected: Vec<Datum<'p>> = Vec::with_capacity(columns.len());
        // **A leaf with no delta rows needs no merge**, which is almost every
        // leaf: the sorted region minus its tombstones, straight off the
        // mini-columns.
        if self.delta_count == 0 {
            let tombstones = match self.has_tombstones() {
                true => Some(self.tombstones()?),
                false => None,
            };
            for row in 0..self.row_count {
                if is_set(tombstones, row)? {
                    continue;
                }
                projected.clear();
                for column in &projected_columns {
                    projected.push(column.value(row)?);
                }
                visit(&projected)?;
            }
            return Ok(());
        }
        // Otherwise the positions come from the one merge every reader shares,
        // fed the delta rows' **keys** only. A delta row can hold a value out of
        // line, and decoding it needs the leaf's extents; a caller projecting
        // other columns has not read them and must not be asked to. So only the
        // key columns and the projected ones are decoded, as before the merge
        // was shared. The rows are visited in key order, which the callers do
        // not need and which costs nothing more than visiting them out of it.
        let mut delta_keys: Vec<Vec<Datum<'p>>> = Vec::with_capacity(self.delta_count);
        for index in 0..self.delta_count {
            delta_keys.push(self.delta_key(index)?);
        }
        for at in self.merge_order(&delta_keys)? {
            projected.clear();
            match at {
                LiveRow::Sorted(row) => {
                    for column in &projected_columns {
                        projected.push(column.value(row as usize)?);
                    }
                }
                LiveRow::Delta(index) => {
                    for column in columns {
                        projected.push(self.delta_value(index as usize, *column)?);
                    }
                }
            }
            visit(&projected)?;
        }
        Ok(())
    }
    /// Materialises the live rows whose key matches a probe.
    ///
    /// **The merge an index probe needs, over the span it asked for**
    /// (task-2066 §4.3.4). A probe into a written leaf used to call
    /// [`LeafRef::live_between`] with the probe as both bounds, which calls
    /// [`LeafRef::live`] - every row of the leaf materialised, a shadow scan
    /// per delta entry over the delta entries already merged, and a sort of
    /// the result - and then threw away everything outside the probe. A leaf
    /// `CREATE INDEX` built before the load holds its whole contents in the
    /// delta area, so that ran once per probe over the whole leaf: a join over
    /// such an index took 2,634 ms against 2.91 ms for the same index built
    /// after the load.
    ///
    /// The rules are [`LeafRef::live`]'s, unchanged: a tombstoned sorted row
    /// is not live; the newest delta entry for a key wins and newest is the
    /// lowest index; a delta entry replaces the sorted row it shadows rather
    /// than joining it. What changes is that the probe filters the delta area
    /// *before* the shadow scans instead of after the merge, so the quadratic
    /// part applies to the rows that match rather than to the leaf.
    ///
    /// @param probe - the key, one value per column it names
    /// @param scan_cap - how far a run is walked before its end is bisected
    pub fn live_matching(
        &self,
        probe: &[Datum<'_>],
        scan_cap: usize,
    ) -> DbResult<Vec<Vec<Datum<'p>>>> {
        let (begin, end) = self.equal_run(probe, scan_cap)?;
        let mut sorted: Vec<Vec<Datum<'p>>> = Vec::new();
        for row in begin..end {
            if self.is_tombstoned(row)? {
                continue;
            }
            let mut values = Vec::with_capacity(self.column_count);
            for column in 0..self.column_count {
                values.push(self.value(row, column)?);
            }
            sorted.push(values);
        }
        // The delta rows that match are a run of the directory, found by two
        // binary searches, and a key held twice keeps its first entry.
        let mut delta: Vec<Vec<Datum<'p>>> = Vec::new();
        for index in self.delta_matching(probe)? {
            let values = self.delta_row_values(index)?;
            if let Some(earlier) = delta.last() {
                if self.compare_keys(earlier, &values) == std::cmp::Ordering::Equal {
                    continue;
                }
            }
            delta.push(values);
        }
        Ok(self.merge_runs(sorted, delta))
    }

    /// Merges two runs of rows that are each in key order, the second newer.
    ///
    /// A row of the newer run replaces the row of the older run with the same
    /// key rather than joining it, which is the rule a delta row follows over
    /// the sorted row it shadows.
    ///
    /// @param older - the sorted region's rows, in key order
    /// @param newer - the delta area's rows, in key order and one per key
    fn merge_runs(
        &self,
        older: Vec<Vec<Datum<'p>>>,
        newer: Vec<Vec<Datum<'p>>>,
    ) -> Vec<Vec<Datum<'p>>> {
        let mut merged = Vec::with_capacity(older.len().saturating_add(newer.len()));
        let mut older = older.into_iter().peekable();
        for row in newer {
            while let Some(held) = older.peek() {
                match self.compare_keys(held, &row) {
                    std::cmp::Ordering::Less => merged.extend(older.next()),
                    std::cmp::Ordering::Equal => {
                        older.next();
                        break;
                    }
                    std::cmp::Ordering::Greater => break,
                }
            }
            merged.push(row);
        }
        merged.extend(older);
        merged
    }

    /// How many bytes of the page this leaf occupies.
    ///
    /// The page minus its free gap. The slots and the delta area grow up from
    /// the directory and the heap grows *down* from the end of the page - see
    /// `LeafBuilder`, where `heap_end` starts at the page size and decreases -
    /// so everything outside `delta_start..heap_start` is in use.
    ///
    /// Two integers already parsed, which is the point: it lets a caller ask
    /// how full a leaf is without materialising it (task-2066 §4.3.7).
    ///
    /// @param page_size - the page size the leaf was built at
    pub fn used_bytes(&self, page_size: usize) -> usize {
        let gap = self.heap_start.saturating_sub(self.delta_start);
        page_size.saturating_sub(gap)
    }

    /// Whether any live row of this leaf sorts after a probe.
    ///
    /// **The question an equality walk has to ask before it follows the right
    /// sibling** (task-2066 §4.3.4). The leaves of a tree are ordered, so a run
    /// of rows equal to a probe can continue into the next leaf only if nothing
    /// in this one already sorts past it. The sorted region answers that with
    /// `begin >= rows`; the delta area has to be asked directly, and a leaf
    /// `CREATE INDEX` built before the load has its whole contents there.
    ///
    /// The delta directory is in key order, so the last entry holds the
    /// largest key and one comparison answers for the whole area. A format 1
    /// area is in arrival order and is scanned.
    ///
    /// A tombstoned sorted row is not live and does not count; a delta row
    /// shadowing a sorted one is the same key either way, so shadowing does not
    /// change the answer and is not resolved here.
    ///
    /// @param probe - the key, one value per column it names
    pub(crate) fn holds_a_key_past(&self, probe: &[Datum<'_>]) -> DbResult<bool> {
        if !self.has_delta_directory() {
            for index in 0..self.delta_count {
                if self.compare_delta_key(index, probe)? == std::cmp::Ordering::Greater {
                    return Ok(true);
                }
            }
            return Ok(false);
        }
        let Some(last) = self.delta_count.checked_sub(1) else {
            return Ok(false);
        };
        Ok(self.compare_delta_key(last, probe)? == std::cmp::Ordering::Greater)
    }

    /// Materialises the live rows inside a key range.
    ///
    /// The merged counterpart of the sorted region's `lower_bound`/`upper_bound`
    /// pair, for a leaf that has been written to. The bounds are compared under
    /// the leaf's own collations, which is what keeps a range over a `NOCASE`
    /// column agreeing with the order the tree is stored in.
    ///
    /// **Only the rows inside the bounds are read** (task-2082). This used to
    /// call [`LeafRef::live`] and filter, so a range probe into a leaf holding
    /// 1,700 index entries decoded all of them and every delta row as well to
    /// return the handful it wanted. Since task-2074 sized the delta area by
    /// the free gap, a leaf that took writes keeps its delta rows, and so stays
    /// on this path, for much longer than it did at 32 rows. The sorted region
    /// is now bounded by `lower_bound`/`upper_bound` and the delta area by the
    /// same searches over its directory, and the two runs are merged with the
    /// rule [`LeafRef::live_matching`] uses.
    ///
    /// The gate's `join.range` does not reach this: its round reads before it
    /// writes, so every leaf it probes is clean. task-2082 measured its time
    /// unchanged by this and by task-2074 alike.
    ///
    /// A format 1 area is not in key order, and a bound naming more columns
    /// than the key compares columns no search here can, so both still take
    /// the whole leaf and filter it.
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
        let too_wide =
            |bound: Option<&[Datum<'_>]>| bound.is_some_and(|b| b.len() > self.key_columns);
        if !self.has_delta_directory() || too_wide(low) || too_wide(high) {
            return self.live_between_scanned(low, low_inclusive, high, high_inclusive);
        }
        let (sorted_begin, sorted_end) =
            self.sorted_span(low, low_inclusive, high, high_inclusive)?;
        let mut sorted: Vec<Vec<Datum<'p>>> = Vec::new();
        for row in sorted_begin..sorted_end {
            if self.is_tombstoned(row)? {
                continue;
            }
            let mut values = Vec::with_capacity(self.column_count);
            for column in 0..self.column_count {
                values.push(self.value(row, column)?);
            }
            sorted.push(values);
        }
        let delta_begin = match low {
            Some(bound) => self.delta_bound(bound, !low_inclusive)?,
            None => 0,
        };
        let delta_end = match high {
            Some(bound) => self.delta_bound(bound, high_inclusive)?,
            None => self.delta_count,
        };
        // A key held twice keeps its first entry, which is the newer.
        let mut delta: Vec<Vec<Datum<'p>>> = Vec::new();
        for index in delta_begin..delta_end.max(delta_begin) {
            let values = self.delta_row_values(index)?;
            if let Some(earlier) = delta.last() {
                if self.compare_keys(earlier, &values) == std::cmp::Ordering::Equal {
                    continue;
                }
            }
            delta.push(values);
        }
        Ok(self.merge_runs(sorted, delta))
    }

    /// Returns the sorted rows inside a key range, as a half-open span.
    ///
    /// @param low - the lower bound, or `None` for the start
    /// @param low_inclusive - whether a key equal to `low` is in the range
    /// @param high - the upper bound, or `None` for the end
    /// @param high_inclusive - whether a key equal to `high` is in the range
    fn sorted_span(
        &self,
        low: Option<&[Datum<'_>]>,
        low_inclusive: bool,
        high: Option<&[Datum<'_>]>,
        high_inclusive: bool,
    ) -> DbResult<(usize, usize)> {
        let begin = match low {
            Some(bound) if low_inclusive => self.lower_bound(bound)?,
            Some(bound) => self.upper_bound(bound)?,
            None => 0,
        };
        let end = match high {
            Some(bound) if high_inclusive => self.upper_bound(bound)?,
            Some(bound) => self.lower_bound(bound)?,
            None => self.row_count,
        };
        Ok((begin, end.clamp(begin, self.row_count.max(begin))))
    }

    /// Materialises every live row and keeps the ones inside a key range.
    ///
    /// What [`LeafRef::live_between`] did for every leaf before task-2082, and
    /// still does for the two cases it cannot bound by searching.
    ///
    /// @param low - the lower bound, or `None` for the start
    /// @param low_inclusive - whether a key equal to `low` is in the range
    /// @param high - the upper bound, or `None` for the end
    /// @param high_inclusive - whether a key equal to `high` is in the range
    fn live_between_scanned(
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

/// Reports whether one row's bit is set in a tombstone bitmap.
///
/// @param bitmap - the bitmap, or `None` for a leaf that has none
/// @param row - the row's position in the sorted region
fn is_set(bitmap: Option<&[u8]>, row: usize) -> DbResult<bool> {
    let Some(bitmap) = bitmap else {
        return Ok(false);
    };
    let byte = bitmap
        .get(row / 8)
        .copied()
        .ok_or_else(|| corrupt(format!("row {row} is outside the tombstone bitmap")))?;
    Ok(byte & (1u8 << (row % 8)) != 0)
}

/// Appends the live sorted rows of a range to a merge's output.
///
/// @param order - the merge's output
/// @param bitmap - the tombstone bitmap, or `None` for a leaf that has none
/// @param from - the first sorted row of the range
/// @param to - one past the last
fn push_live_sorted(
    order: &mut Vec<LiveRow>,
    bitmap: Option<&[u8]>,
    from: usize,
    to: usize,
) -> DbResult<()> {
    for row in from..to {
        if !is_set(bitmap, row)? {
            order.push(LiveRow::Sorted(row as u32));
        }
    }
    Ok(())
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
/// Which rows of a leaf are live, in key order, before any of them is read.
///
/// **The merge on its own, so the merge and the read can be priced apart.**
/// A compaction pays for both and a splice pays only for this one: it needs to
/// know which rows it is keeping and in what order, and then it moves their
/// slots rather than their values. The views it carries - the mini-columns and
/// the decoded delta rows - are what [`LiveOrder::materialise`] reads through,
/// and what a slot-level caller would read through instead.
pub struct LiveOrder<'p> {
    /// One view per column of the sorted region.
    columns: Vec<MiniColumn<'p>>,
    /// The delta rows, each decoded once.
    delta: Vec<Vec<Datum<'p>>>,
    /// Which row is live, in key order.
    order: Vec<LiveRow>,
    /// How many columns each row has.
    width: usize,
}

impl<'p> LiveOrder<'p> {
    /// How many live rows the leaf holds.
    pub fn len(&self) -> usize {
        self.order.len()
    }

    /// Reports whether the leaf holds none.
    pub fn is_empty(&self) -> bool {
        self.order.is_empty()
    }

    /// How many columns each row has.
    pub fn width(&self) -> usize {
        self.width
    }

    /// Where each live row sits, in key order.
    pub fn order(&self) -> &[LiveRow] {
        &self.order
    }

    /// The sorted region's columns, one view each.
    pub fn columns(&self) -> &[MiniColumn<'p>] {
        &self.columns
    }

    /// The delta rows, decoded.
    pub fn delta(&self) -> &[Vec<Datum<'p>>] {
        &self.delta
    }

    /// Returns one value of one live row, by its place in key order.
    ///
    /// @param row - the row's position among the live rows
    /// @param column - which column
    pub fn value(&self, row: usize, column: usize) -> DbResult<Datum<'p>> {
        let at = self
            .order
            .get(row)
            .copied()
            .ok_or_else(|| corrupt(format!("live row {row} does not exist")))?;
        live_value(&self.columns, &self.delta, at, column)
    }

    /// Reads every value of every live row into one flat vector.
    ///
    /// One allocation of `rows * width`, and each value decoded once rather
    /// than once per pass the builder makes over it.
    pub fn materialise(self) -> DbResult<LiveSource<'p>> {
        let mut values = Vec::with_capacity(self.order.len().saturating_mul(self.width));
        for at in &self.order {
            for column in 0..self.width {
                values.push(live_value(&self.columns, &self.delta, *at, column)?);
            }
        }
        Ok(LiveSource {
            values,
            width: self.width,
        })
    }
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
            //
            // **What the bytes are called comes from the reference first and
            // the column second** (task-1986). The column is the only thing
            // that can answer for a reference written before a reference could
            // say, and it is the wrong thing to ask for a text in a column
            // declared `BLOB` - which is what a column declared nothing at all
            // is. `extent_datum` is the whole rule, written beside the writer
            // that states it.
            ValueClass::Extent => match self.extents.and_then(|held| held.get(row, self.index)) {
                Some(bytes) => Ok(crate::leaf::extent_datum(
                    self.extent(row)?.class,
                    self.physical,
                    bytes,
                )),
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

#[cfg(test)]
mod tests;
