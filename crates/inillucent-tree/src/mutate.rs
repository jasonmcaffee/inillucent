//! Mutating a leaf in place: the delta area, tombstones, and slot updates.
//!
//! Invariant: **every mutation leaves a page `LeafRef::parse` accepts and
//! `LeafRef::integrity` passes.** That is not a hope - each operation here
//! computes where the region it is about to write ends and refuses when it would
//! cross the heap or the mini-columns, so a page that would have become
//! unreadable is a `false` return and a compaction rather than a corrupt page
//! nobody notices until the next read.
//!
//! ## Where a delta row goes, and why it goes downwards
//!
//! A freshly built leaf looks like this, and `LeafBuilder::encode` sets
//! `delta_start` equal to `heap_start` so that the whole free gap is available:
//!
//! ```text
//! [ header ][ directory ][ mini-columns ] ... free ... [ delta ][ heap ]
//!                                                       ^        ^
//!                                                       |        heap_start
//!                                                       delta_start
//! ```
//!
//! An insert moves `delta_start` **down** by the row's size and writes the row
//! at the new `delta_start`. Existing delta rows do not move, so an insert is
//! one bounds check and one copy rather than a memmove of the whole area - and
//! the newest row is therefore *first*, which is what makes "the first match
//! wins" the right rule for a key the delta area holds twice.
//!
//! The tombstone bitmap sits immediately below `delta_start`, so it moves when
//! `delta_start` does. That is `row_count / 8` bytes - 223 for the 1,782-row
//! leaves the scorecard's index trees hold - and it keeps the bitmap's position
//! derivable from the header rather than being a fourth offset to keep
//! consistent with the other three.
//!
//! ## What is *not* here
//!
//! Splitting, merging and choosing when to compact are the tree's business,
//! because they allocate pages and rewrite parents. This module knows one page.

use inillucent_base::error::{corrupt, misuse};
use inillucent_base::DbResult;

use crate::datum::Datum;
use crate::leaf::{
    class_bytes, leaf_header, tombstone_bytes, LeafRef, DELTA_LIMIT, LEAF_HAS_DELTA,
    LEAF_HAS_TOMBSTONES,
};
use crate::page::{self, header};
use crate::types::{ColumnSpec, PhysicalType};

/// What a mutation did, when it could not simply be done.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Applied {
    /// The change is in the page.
    Yes,
    /// The page has no room, or the delta area is at its limit. The caller
    /// compacts and tries again, and if it still does not fit, splits.
    NoRoom,
}

/// A delta insert that has been costed and not yet performed.
///
/// **The reason this is a separate step is the write-ahead rule.** A record has
/// to be in the log before the change it describes reaches the page, and a
/// change that could still fail *after* its record was written would leave a log
/// saying something that did not happen. So the arithmetic runs first and
/// returns either a plan or "no room"; the caller compacts on "no room", and
/// [`LeafMut::apply_delta`] cannot fail on a plan this page produced.
///
/// One arithmetic, two entry points. A separate `would_it_fit` predicate would
/// be a second implementation of the same offsets, which is the duplicate this
/// codebase has already paid for more than once.
#[derive(Clone, Debug)]
pub struct DeltaPlan {
    /// The row's tagged bytes.
    encoded: Vec<u8>,
    /// Where the delta area will start afterwards.
    new_delta_start: usize,
    /// Where it starts now.
    old_delta_start: usize,
    /// How many bytes of tombstone bitmap have to move with it.
    bitmap: usize,
    /// How many delta rows the page holds now.
    delta_count: usize,
}

impl DeltaPlan {
    /// Returns the bytes the row will occupy in the page.
    pub fn encoded(&self) -> &[u8] {
        &self.encoded
    }
}

/// A leaf page being changed.
pub struct LeafMut<'p> {
    page: &'p mut [u8],
}

impl<'p> LeafMut<'p> {
    /// Returns a mutable view, after checking the page is a leaf.
    ///
    /// Parsed first rather than trusted: every offset this module computes comes
    /// from the header, so a header that has not been validated is a header that
    /// can send a write anywhere in the page.
    ///
    /// @param page - the page bytes, exactly one page long
    pub fn new(page: &'p mut [u8]) -> DbResult<LeafMut<'p>> {
        LeafRef::parse(page)?;
        Ok(LeafMut { page })
    }

    /// Returns a read view over the page as it currently stands.
    pub fn view(&self) -> DbResult<LeafRef<'_>> {
        LeafRef::parse(self.page)
    }

    /// Returns the page bytes.
    pub fn bytes(&self) -> &[u8] {
        self.page
    }

    /// Stamps the page's LSN, which is what makes redo idempotent.
    ///
    /// @param lsn - the log record that describes this change
    pub fn set_lsn(&mut self, lsn: u64) -> DbResult<()> {
        page::write_u64(self.page, header::LSN, lsn)
    }

    /// Records the newest commit timestamp on the page.
    ///
    /// The TDD's seventh invariant: a leaf's `max_cts` is at or above the `cts`
    /// of every committed change on it. A reader whose snapshot is at or above
    /// it can read the page as it stands and skip the version log entirely,
    /// which is the common case and the reason the field exists.
    ///
    /// @param cts - the commit timestamp to record
    pub fn set_max_cts(&mut self, cts: u64) -> DbResult<()> {
        let current = page::read_u64(self.page, leaf_header::MAX_CTS)?;
        page::write_u64(self.page, leaf_header::MAX_CTS, current.max(cts))
    }

    /// Returns where the mini-columns end, which is the floor for everything
    /// that grows downwards.
    /// Reports whether a row of this size and a tombstone would both fit.
    ///
    /// **One question, one page parse, one walk of the column directory.** The
    /// two halves used to be asked separately and each of them recomputed where
    /// the mini-columns end - a loop over every column reading its spec - so a
    /// write asked the same arithmetic three times before it wrote anything. It
    /// was the most expensive phase of a put, ahead of the descent that found
    /// the page.
    ///
    /// Both halves have to hold: the row needs room in the delta area, and the
    /// write may also have to tombstone whatever was under the key, which needs
    /// the bitmap to exist or to have room to.
    ///
    /// @param encoded_len - how many bytes the row's tagged form occupies
    pub fn room_for(&self, encoded_len: usize) -> DbResult<bool> {
        let leaf = LeafRef::parse(self.page)?;
        let (delta_count, delta_start, row_count, has_tombstones) = (
            leaf.delta_count(),
            leaf.delta_start(),
            leaf.row_count(),
            leaf.has_tombstones(),
        );
        // `LeafRef` is `Copy` and borrows the guard, so this ends the borrow
        // rather than releasing anything.
        let _ = leaf;
        if delta_count >= DELTA_LIMIT || encoded_len > u16::MAX as usize {
            return Ok(false);
        }
        let floor = self.columns_end()?;
        let bitmap = if has_tombstones {
            tombstone_bytes(row_count)
        } else {
            0
        };
        let Some(new_delta_start) = delta_start.checked_sub(encoded_len.saturating_add(2)) else {
            return Ok(false);
        };
        if new_delta_start.saturating_sub(bitmap) < floor {
            return Ok(false);
        }
        if !has_tombstones && delta_start.saturating_sub(tombstone_bytes(row_count)) < floor {
            return Ok(false);
        }
        Ok(true)
    }

    fn columns_end(&self) -> DbResult<usize> {
        columns_end(self.page)
    }

    /// Inserts a row into the delta area.
    ///
    /// Returns [`Applied::NoRoom`] when the delta area is at [`DELTA_LIMIT`] or
    /// the row would collide with the mini-columns or the tombstone bitmap. The
    /// caller compacts; nothing is written in that case, so a refused insert
    /// leaves the page exactly as it was.
    ///
    /// @param columns - the column directory, so the row's width is known
    /// @param row - the row's values, one per column
    pub fn insert_delta(&mut self, columns: &[ColumnSpec], row: &[Datum<'_>]) -> DbResult<Applied> {
        match self.plan_delta(columns, row)? {
            Some(plan) => {
                self.apply_delta(&plan)?;
                Ok(Applied::Yes)
            }
            None => Ok(Applied::NoRoom),
        }
    }

    /// Costs a delta insert without performing it.
    ///
    /// Returns `None` when the delta area is at [`DELTA_LIMIT`] or the row would
    /// collide with the mini-columns or the tombstone bitmap. Nothing is
    /// written either way.
    ///
    /// @param columns - the column directory, so the row's width is known
    /// @param row - the row's values, one per column
    pub fn plan_delta(
        &self,
        columns: &[ColumnSpec],
        row: &[Datum<'_>],
    ) -> DbResult<Option<DeltaPlan>> {
        if row.len() != columns.len() {
            return Err(misuse(format!(
                "a row of {} values does not fit {} columns",
                row.len(),
                columns.len()
            )));
        }
        let mut encoded = Vec::new();
        for value in row {
            value.encode_tagged(&mut encoded);
        }
        self.plan_encoded(encoded)
    }

    /// Writes one already-encoded row into the delta area, as it was logged.
    ///
    /// **Recovery replays the bytes, not the values.** A delta row can hold an
    /// out-of-line reference, which is not a value a `Datum` can carry, so a
    /// replay that decoded and re-encoded would have to invent one. Writing
    /// what the record holds is also the more faithful replay: the page ends up
    /// byte-identical to the one the write path produced.
    ///
    /// @param encoded - the row's tagged bytes, as the log record carries them
    pub fn insert_delta_encoded(&mut self, encoded: &[u8]) -> DbResult<Applied> {
        let Some(plan) = self.plan_encoded(encoded.to_vec())? else {
            return Ok(Applied::NoRoom);
        };
        self.apply_delta(&plan)?;
        Ok(Applied::Yes)
    }

    /// Costs a delta insert whose row is already in its tagged form.
    ///
    /// **The encoding is the expensive half and it does not depend on the
    /// page.** A write costs the insert once to find out whether it fits, logs
    /// the row, and then costs it again after displacing whatever was under the
    /// key - and the first version encoded the row inside each of those *and*
    /// once more for the log record. Three copies of every row, and on the
    /// gate's `wide` table a row is four kilobytes.
    ///
    /// The plan cannot simply be carried across the log call instead: it holds
    /// the page's delta offsets, and displacing the old row moves them. So the
    /// *encoding* is what is carried, and the offsets are recomputed - which is
    /// arithmetic over four fields.
    ///
    /// @param encoded - the row's tagged bytes
    pub fn plan_encoded(&self, encoded: Vec<u8>) -> DbResult<Option<DeltaPlan>> {
        let Some(offsets) = self.delta_offsets(encoded.len())? else {
            return Ok(None);
        };
        Ok(Some(DeltaPlan {
            encoded,
            new_delta_start: offsets.0,
            old_delta_start: offsets.1,
            bitmap: offsets.2,
            delta_count: offsets.3,
        }))
    }

    /// Returns where a row of this size would land, or `None` if it would not.
    ///
    /// @param encoded_len - how many bytes the row's tagged form occupies
    fn delta_offsets(&self, encoded_len: usize) -> DbResult<Option<(usize, usize, usize, usize)>> {
        // Every field this needs is copied out and the view is dropped, because
        // the writes below take a mutable borrow of the same bytes. Keeping the
        // view alive and reaching around it is what the borrow checker is for.
        let (delta_count, delta_start, row_count, has_tombstones) = {
            let leaf = LeafRef::parse(self.page)?;
            (
                leaf.delta_count(),
                leaf.delta_start(),
                leaf.row_count(),
                leaf.has_tombstones(),
            )
        };
        if delta_count >= DELTA_LIMIT {
            return Ok(None);
        }
        if encoded_len > u16::MAX as usize {
            return Ok(None);
        }
        let bitmap = if has_tombstones {
            tombstone_bytes(row_count)
        } else {
            0
        };
        let entry = encoded_len.saturating_add(2);
        let Some(new_delta_start) = delta_start.checked_sub(entry) else {
            return Ok(None);
        };
        let floor = self.columns_end()?;
        if new_delta_start.saturating_sub(bitmap) < floor {
            return Ok(None);
        }
        Ok(Some((new_delta_start, delta_start, bitmap, delta_count)))
    }

    /// Performs a delta insert that [`LeafMut::plan_delta`] costed.
    ///
    /// Cannot run out of room: the plan already proved the offsets. It can still
    /// return an error, because every slice it takes is bounds checked - but the
    /// only way one of those fires is a plan from a different page, which is a
    /// caller bug rather than a full page.
    ///
    /// @param plan - what [`LeafMut::plan_delta`] returned
    pub fn apply_delta(&mut self, plan: &DeltaPlan) -> DbResult<()> {
        // The bitmap moves down with the delta area. Copied *before* the row is
        // written, because the row's bytes land where part of the old bitmap
        // may still be.
        if plan.bitmap > 0 {
            let from = plan.old_delta_start.saturating_sub(plan.bitmap);
            let to = plan.new_delta_start.saturating_sub(plan.bitmap);
            self.page
                .copy_within(from..from.saturating_add(plan.bitmap), to);
        }
        let at = plan.new_delta_start;
        page::write_u16(self.page, at, plan.encoded.len() as u16)?;
        let body = self
            .page
            .get_mut(at.saturating_add(2)..at.saturating_add(2).saturating_add(plan.encoded.len()))
            .ok_or_else(|| corrupt("the delta row ran past the page"))?;
        body.copy_from_slice(&plan.encoded);
        page::write_u32(self.page, leaf_header::DELTA_START, at as u32)?;
        page::write_u16(
            self.page,
            leaf_header::DELTA_COUNT,
            plan.delta_count.saturating_add(1) as u16,
        )?;
        self.set_flag(LEAF_HAS_DELTA, true)?;
        Ok(())
    }

    /// Reports whether a tombstone can be set without compacting first.
    ///
    /// Only a leaf that has no bitmap yet can refuse, and only because the
    /// bitmap has to be created. Once it exists, setting a bit always fits.
    pub fn has_room_for_a_tombstone(&self) -> DbResult<bool> {
        let (row_count, delta_start, has_tombstones) = {
            let leaf = LeafRef::parse(self.page)?;
            (leaf.row_count(), leaf.delta_start(), leaf.has_tombstones())
        };
        if has_tombstones {
            return Ok(true);
        }
        let floor = self.columns_end()?;
        Ok(delta_start.saturating_sub(tombstone_bytes(row_count)) >= floor)
    }

    /// Removes one row from the delta area, rebuilding the area without it.
    ///
    /// A delta row cannot be marked dead in place - its encoding has no room for
    /// a flag and inventing one would make every reader check it - so the area
    /// is rebuilt. That is a copy of at most [`DELTA_LIMIT`] short rows, which
    /// is what the limit is for.
    ///
    /// @param index - the row's position in the delta area
    pub fn remove_delta(&mut self, index: usize) -> DbResult<()> {
        let leaf = LeafRef::parse(self.page)?;
        if index >= leaf.delta_count() {
            return Err(misuse(format!("delta row {index} does not exist")));
        }
        let kept: Vec<Vec<u8>> = (0..leaf.delta_count())
            .filter(|position| *position != index)
            .map(|position| leaf.delta_row(position).map(<[u8]>::to_vec))
            .collect::<DbResult<Vec<Vec<u8>>>>()?;
        // **The extent flag is the union of both regions, so removing the row
        // that held the only reference has to clear it.** A flag left set over
        // a leaf holding nothing out of line is what the integrity check calls
        // a disagreement, and it would send every later reader looking for
        // extents that are not there. Only asked when the removed row actually
        // held one, which is rare: the scan costs nothing on an ordinary write.
        let dropped = leaf.delta_extents_in(index)?;
        self.rewrite_delta(&kept)?;
        if dropped {
            let union = {
                let leaf = LeafRef::parse(self.page)?;
                let mut seen = leaf.any_delta_extent_unchecked_pub()?;
                for column in 0..leaf.column_count() {
                    seen = seen || leaf.column(column)?.any_extent()?;
                }
                seen
            };
            self.set_flag(crate::leaf::LEAF_HAS_EXTENTS, union)?;
        }
        Ok(())
    }

    /// Replaces the whole delta area with the given rows, newest first.
    ///
    /// @param rows - the encoded rows, in the order they should be read
    fn rewrite_delta(&mut self, rows: &[Vec<u8>]) -> DbResult<()> {
        let (heap_start, row_count, has_tombstones, old_delta_start) = {
            let leaf = LeafRef::parse(self.page)?;
            (
                leaf.heap_start(),
                leaf.row_count(),
                leaf.has_tombstones(),
                leaf.delta_start(),
            )
        };
        let bitmap = if has_tombstones {
            tombstone_bytes(row_count)
        } else {
            0
        };
        let total: usize = rows
            .iter()
            .map(|row| row.len().saturating_add(2))
            .fold(0usize, usize::saturating_add);
        let new_delta_start = heap_start
            .checked_sub(total)
            .ok_or_else(|| corrupt("the delta area ran below the page"))?;
        let floor = self.columns_end()?;
        if new_delta_start.saturating_sub(bitmap) < floor {
            return Err(corrupt("a rewritten delta area does not fit its own page"));
        }
        // The bitmap first, and out through a copy rather than in place: the
        // area is moving up, so a `copy_within` would overwrite the source when
        // the regions overlap.
        let saved: Vec<u8> = if bitmap > 0 {
            let from = old_delta_start.saturating_sub(bitmap);
            self.page
                .get(from..from.saturating_add(bitmap))
                .ok_or_else(|| corrupt("the tombstone bitmap ran past the page"))?
                .to_vec()
        } else {
            Vec::new()
        };
        let mut at = new_delta_start;
        for row in rows {
            page::write_u16(self.page, at, row.len() as u16)?;
            let body = self
                .page
                .get_mut(at.saturating_add(2)..at.saturating_add(2).saturating_add(row.len()))
                .ok_or_else(|| corrupt("a delta row ran past the page"))?;
            body.copy_from_slice(row);
            at = at.saturating_add(2).saturating_add(row.len());
        }
        if bitmap > 0 {
            let to = new_delta_start.saturating_sub(bitmap);
            let target = self
                .page
                .get_mut(to..to.saturating_add(bitmap))
                .ok_or_else(|| corrupt("the tombstone bitmap ran past the page"))?;
            target.copy_from_slice(&saved);
        }
        page::write_u32(self.page, leaf_header::DELTA_START, new_delta_start as u32)?;
        page::write_u16(self.page, leaf_header::DELTA_COUNT, rows.len() as u16)?;
        self.set_flag(LEAF_HAS_DELTA, !rows.is_empty())?;
        Ok(())
    }

    /// Marks one sorted-region row as deleted.
    ///
    /// Returns [`Applied::NoRoom`] when the bitmap has to be created and there
    /// is nowhere to put it, which is the same answer an insert gives and takes
    /// the same route out: compact, then try again.
    ///
    /// @param row - the row's position in the sorted region
    pub fn set_tombstone(&mut self, row: usize) -> DbResult<Applied> {
        let (row_count, delta_start, has_tombstones) = {
            let leaf = LeafRef::parse(self.page)?;
            (leaf.row_count(), leaf.delta_start(), leaf.has_tombstones())
        };
        if row >= row_count {
            return Err(misuse(format!("row {row} is not in the sorted region")));
        }
        let bitmap = tombstone_bytes(row_count);
        if !has_tombstones {
            let floor = self.columns_end()?;
            if delta_start.saturating_sub(bitmap) < floor {
                return Ok(Applied::NoRoom);
            }
            let start = delta_start.saturating_sub(bitmap);
            let target = self
                .page
                .get_mut(start..delta_start)
                .ok_or_else(|| corrupt("the tombstone bitmap ran past the page"))?;
            target.fill(0);
            self.set_flag(LEAF_HAS_TOMBSTONES, true)?;
        }
        let start = delta_start.saturating_sub(bitmap);
        let byte = self
            .page
            .get_mut(start.saturating_add(row / 8))
            .ok_or_else(|| corrupt("the tombstone bitmap ran past the page"))?;
        *byte |= 1u8 << (row % 8);
        Ok(Applied::Yes)
    }

    /// Clears one sorted-region row's tombstone.
    ///
    /// Used when a key that was deleted is inserted again and the sorted region
    /// still holds its slot: resurrecting the row in place is cheaper than a
    /// delta row, and it keeps the leaf on the vectorised path when nothing else
    /// has touched it.
    ///
    /// @param row - the row's position in the sorted region
    pub fn clear_tombstone(&mut self, row: usize) -> DbResult<()> {
        let (row_count, delta_start, has_tombstones) = {
            let leaf = LeafRef::parse(self.page)?;
            (leaf.row_count(), leaf.delta_start(), leaf.has_tombstones())
        };
        if !has_tombstones {
            return Ok(());
        }
        if row >= row_count {
            return Err(misuse(format!("row {row} is not in the sorted region")));
        }
        let bitmap = tombstone_bytes(row_count);
        let start = delta_start.saturating_sub(bitmap);
        let byte = self
            .page
            .get_mut(start.saturating_add(row / 8))
            .ok_or_else(|| corrupt("the tombstone bitmap ran past the page"))?;
        *byte &= !(1u8 << (row % 8));
        Ok(())
    }

    /// Reports whether [`LeafMut::update_slot`] would write, without writing.
    ///
    /// The method form of [`would_update_slot`], for a caller that already holds
    /// a `LeafMut`.
    ///
    /// @param column - which column would be written
    /// @param row - the row's position in the sorted region
    /// @param value - the new value
    pub fn would_update_slot(
        &self,
        column: usize,
        row: usize,
        value: &Datum<'_>,
    ) -> DbResult<bool> {
        would_update_slot(self.page, column, row, value)
    }

    /// Overwrites one slot of one sorted-region row.
    ///
    /// The only update that does not go through the delta area, and the only one
    /// that keeps a leaf on the vectorised fast path. It applies to `Int64` and
    /// `Float64` when the new value is of that class, and to `Text` and `Blob`
    /// whatever length the new value is - the same length and shorter are
    /// written where the value lies, and longer is written at the bottom of the
    /// heap with the slot repointed at it. See
    /// `LeafMut::overwrite_heap_slot`, which is where the argument for that
    /// is.
    ///
    /// The text case is not a curiosity: it is what an `UPDATE` of a string
    /// column does, which is the gate's `txn.large`, two thousand of them in one
    /// transaction. The length-changing branches were added later; before that
    /// the writer refused any change of length and forced a delete plus insert.
    ///
    /// Returns [`Applied::NoRoom`] when it does not apply, which the caller
    /// turns into a delete plus a delta insert.
    ///
    /// @param column - which column to write
    /// @param row - the row's position in the sorted region
    /// @param value - the new value
    pub fn update_slot(
        &mut self,
        column: usize,
        row: usize,
        value: &Datum<'_>,
    ) -> DbResult<Applied> {
        let (row_count, spec, width, frame, stride, class) = {
            let leaf = LeafRef::parse(self.page)?;
            if row >= leaf.row_count() {
                return Err(misuse(format!("row {row} is not in the sorted region")));
            }
            let spec = leaf.spec(column)?;
            (
                leaf.row_count(),
                spec,
                leaf.column_width(column)?,
                leaf.column_base(column)?,
                leaf.directory_entry_size(),
                leaf.column(column)?.class_at(row)?,
            )
        };
        // A row whose current value is a NULL or an exception has its class bit
        // set to something other than Typed, and changing that is a write into
        // the class array as well - which is fine, except that the directory's
        // `all_typed` bit would then be stale. Refusing is one branch; keeping
        // the bit correct through every path is several. It also excludes an
        // `Extent`, whose slot holds a page reference rather than a value.
        if class != crate::types::ValueClass::Typed {
            return Ok(Applied::NoRoom);
        }
        let entry = leaf_header::DIRECTORY.saturating_add(column.saturating_mul(stride));
        let base = page::read_u32(self.page, entry.saturating_add(4))? as usize;
        let values_at = base.saturating_add(class_bytes(row_count));
        let at = values_at.saturating_add(row.saturating_mul(width));
        let slot = match (spec.physical, value) {
            (PhysicalType::Int64, Datum::Int(number)) => {
                // **A narrow slot is a promise about the values in it, and an
                // in-place write is the one place that promise can be broken.**
                // A column laid out at one byte holds -128..=127; writing 300
                // over one of them would truncate, so the write is refused and
                // the caller's own route out - delete the row, insert it into
                // the delta area, compact later at a width chosen from the new
                // values - takes it instead.
                // **A narrow slot is a promise about the values in it**, and
                // with a frame of reference the promise is about the distance
                // from the base rather than the magnitude. Either way an
                // in-place write that would not fit is refused.
                if !crate::leaf::fits_frame(frame, width, *number) {
                    return Ok(Applied::NoRoom);
                }
                let target = self
                    .page
                    .get_mut(at..at.saturating_add(width))
                    .ok_or_else(|| corrupt("an integer slot runs past the page"))?;
                crate::leaf::write_frame(frame, target, *number);
                return Ok(Applied::Yes);
            }
            (PhysicalType::Float64, Datum::Real(number)) => number.to_bits(),
            (PhysicalType::Float64, Datum::Int(number)) => (*number as f64).to_bits(),
            (PhysicalType::Text, Datum::Text(bytes)) | (PhysicalType::Blob, Datum::Blob(bytes)) => {
                return self.overwrite_heap_slot(at, width, bytes);
            }
            // Every other combination needs the heap to move or the class to
            // change, and both are a rewrite of the mini-column rather than a
            // slot write.
            _ => return Ok(Applied::NoRoom),
        };
        page::write_u64(self.page, at, slot)?;
        Ok(Applied::Yes)
    }

    /// Writes new bytes over a heap value, whatever length they are.
    ///
    /// Three cases, and only the third moves anything on the page:
    ///
    /// - **The same length.** The bytes go where the old ones were and the slot
    ///   does not change at all.
    /// - **Shorter.** The bytes go where the old ones were and the slot's length
    ///   half is lowered. The tail stays where it lies as bytes nothing points
    ///   at, which the next compaction reclaims.
    /// - **Longer.** The value is written at a new place in the same leaf - the
    ///   bottom of the heap, after the tombstone bitmap and the delta area have
    ///   been moved down to make room - and the slot is repointed at it. The old
    ///   bytes stop being pointed at in the same way.
    ///
    /// **The third case is what put the `transaction` family under its floor.**
    /// Refusing a length change is one branch, and the caller's route out of it
    /// is a tombstone plus a delta insert plus, every [`DELTA_LIMIT`] writes, a
    /// compaction over every live row of the leaf. The gate's `txn.large` is two
    /// thousand `UPDATE side_table SET note = ?2` in one transaction, replacing
    /// an eight-byte `note 1234` with a forty-two byte `row 1234 lorem ipsum
    /// ...`: not one of them reached this path. `inillucent-writegate` counted
    /// `inplace` at 0.00 per statement, and the family measured 0.09x to 0.11x
    /// against a floor of 1.00x on four consecutive gate runs.
    ///
    /// **Moving the delta area is bounded work and repacking the leaf is not.**
    /// What moves is the tombstone bitmap and the delta area, which is
    /// `row_count / 8` bytes and at most [`DELTA_LIMIT`] rows - about two
    /// kilobytes on the leaves this fixture holds. A compaction rewrites every
    /// live row, and a `side_table` leaf holds about fifteen hundred of them.
    ///
    /// **Bytes nothing points at are not a leak.** They are what SQLite calls
    /// fragments: the space is inside the leaf, the leaf's own room check counts
    /// it as used, and the next compaction packs the live rows and gets it back.
    /// No reader walks the heap from one end to the other, so a hole in it is
    /// not visible to any of them.
    ///
    /// **A value that belongs out of line is refused rather than moved.** The
    /// format's rule is that a value longer than `page_size / EXTENT_DIVISOR`
    /// lives in an extent, and this is the one write that could put a large
    /// value inline behind the packer's back. Refusing sends it to `put`, which
    /// spills it the way every other write does.
    ///
    /// @param slot_at - where the row's slot sits in the page
    /// @param width - how wide that slot is: eight bytes of `u32`s or four of `u16`s
    /// @param bytes - the new value
    fn overwrite_heap_slot(
        &mut self,
        slot_at: usize,
        width: usize,
        bytes: &[u8],
    ) -> DbResult<Applied> {
        let slot = self
            .page
            .get(slot_at..slot_at.saturating_add(width))
            .ok_or_else(|| corrupt("a heap slot runs past the page"))?;
        let (offset, length) = crate::types::read_heap_slot(slot);
        if bytes.len() <= length {
            let Some(room) = self
                .page
                .get_mut(offset..offset.saturating_add(bytes.len()))
            else {
                return Err(corrupt("a heap slice runs past the page"));
            };
            room.copy_from_slice(bytes);
            if bytes.len() != length {
                let Some(slot) = self.page.get_mut(slot_at..slot_at.saturating_add(width)) else {
                    return Err(corrupt("a heap slot runs past the page"));
                };
                crate::types::write_heap_slot(slot, offset, bytes.len());
            }
            return Ok(Applied::Yes);
        }
        let Some(at) = self.carve_heap(bytes.len())? else {
            return Ok(Applied::NoRoom);
        };
        let Some(room) = self.page.get_mut(at..at.saturating_add(bytes.len())) else {
            return Err(corrupt("a carved heap slice runs past the page"));
        };
        room.copy_from_slice(bytes);
        let Some(slot) = self.page.get_mut(slot_at..slot_at.saturating_add(width)) else {
            return Err(corrupt("a heap slot runs past the page"));
        };
        crate::types::write_heap_slot(slot, at, bytes.len());
        Ok(Applied::Yes)
    }

    /// Makes `wanted` bytes of heap at the bottom of the heap, and says where.
    ///
    /// The tombstone bitmap and the delta area sit between the free gap and the
    /// heap, so the room is made by moving both of them down by `wanted` and
    /// lowering `heap_start` and `delta_start` by the same amount. Every delta
    /// row keeps its position relative to `delta_start`, which is the only thing
    /// any reader of the area knows about it, so no row has to be re-encoded.
    ///
    /// `None` when the free gap is not that wide, which is the caller's signal
    /// to take the ordinary write path and let the leaf compact.
    ///
    /// **Deterministic, which is what makes the logical redo record correct.**
    /// `Body::UpdateInPlace` carries the key, the column and the value, and
    /// recovery replays it by running [`LeafMut::update_slot`] again over a page
    /// that LSN ordering has already put back into the state the original write
    /// saw. The offset this returns is a function of that page and `wanted`
    /// alone, so the replay lands on the same bytes.
    ///
    /// @param wanted - how many bytes of heap the caller needs
    fn carve_room(&self, wanted: usize) -> DbResult<Option<usize>> {
        carve_room(self.page, wanted)
    }

    /// Performs the move [`carve_room`] costed.
    ///
    /// @param wanted - how many bytes of heap the caller needs
    fn carve_heap(&mut self, wanted: usize) -> DbResult<Option<usize>> {
        if self.carve_room(wanted)?.is_none() {
            return Ok(None);
        }
        let (delta_start, heap_start, row_count, has_tombstones) = {
            let leaf = LeafRef::parse(self.page)?;
            (
                leaf.delta_start(),
                leaf.heap_start(),
                leaf.row_count(),
                leaf.has_tombstones(),
            )
        };
        let bitmap = if has_tombstones {
            tombstone_bytes(row_count)
        } else {
            0
        };
        let new_delta_start = delta_start.saturating_sub(wanted);
        let new_heap_start = heap_start.saturating_sub(wanted);
        // The bitmap and the delta area, as one block, moved down together. A
        // delta row is read from `delta_start` upwards, so moving the block and
        // the header field by the same amount leaves every row exactly where its
        // reader looks for it.
        let from = delta_start.saturating_sub(bitmap);
        if from < heap_start {
            self.page
                .copy_within(from..heap_start, from.saturating_sub(wanted));
        }
        page::write_u32(self.page, leaf_header::HEAP_START, new_heap_start as u32)?;
        page::write_u32(self.page, leaf_header::DELTA_START, new_delta_start as u32)?;
        Ok(Some(new_heap_start))
    }

    /// Records that the leaf holds at least one out-of-line value.
    ///
    /// The flag is what a reader checks before it goes looking for extents, so
    /// a delta row that carries a reference has to set it. It is never cleared
    /// here: a compaction rebuilds the page from scratch and sets it from what
    /// it actually packed.
    pub fn mark_extents(&mut self) -> DbResult<()> {
        self.set_flag(crate::leaf::LEAF_HAS_EXTENTS, true)
    }

    /// Sets or clears one of the leaf's flag bits.
    ///
    /// @param bit - the bit to change
    /// @param on - whether to set it
    fn set_flag(&mut self, bit: u8, on: bool) -> DbResult<()> {
        let flags = self
            .page
            .get_mut(header::FLAGS)
            .ok_or_else(|| corrupt("the page has no flag byte"))?;
        if on {
            *flags |= bit;
        } else {
            *flags &= !bit;
        }
        Ok(())
    }

    /// Sets the right-sibling pointer, which a split moves.
    ///
    /// @param right - the new right sibling
    pub fn set_right(&mut self, right: crate::page::PageId) -> DbResult<()> {
        page::set_right(self.page, right)
    }
}

/// Rounds an offset up to the next multiple of eight.
///
/// @param at - the offset to align
fn align8(at: usize) -> usize {
    at.saturating_add(7) & !7
}

/// Returns where the mini-columns end, which is the floor for everything that
/// grows downwards.
///
/// A free function over the page bytes rather than a method, because the write
/// path asks this question about a page it is only allowed to read - see
/// [`would_update_slot`].
///
/// @param page - the page bytes, exactly one page long
fn columns_end(page: &[u8]) -> DbResult<usize> {
    let leaf = LeafRef::parse(page)?;
    let stride = leaf.directory_entry_size();
    let mut end = leaf_header::DIRECTORY.saturating_add(leaf.column_count().saturating_mul(stride));
    for index in 0..leaf.column_count() {
        let entry = leaf_header::DIRECTORY.saturating_add(index.saturating_mul(stride));
        let start = page::read_u32(page, entry.saturating_add(4))? as usize;
        // The column directory's own width, not the physical type's: an integer
        // column's slots may be one, two or four bytes wide, and a page whose
        // end was computed at eight would leave the delta area and the tombstone
        // bitmap floating above the free space they are supposed to be able to
        // grow into.
        let width = class_bytes(leaf.row_count())
            .saturating_add(leaf.row_count().saturating_mul(leaf.column_width(index)?));
        end = end.max(align8(start.saturating_add(width)));
    }
    Ok(end)
}

/// Returns where `wanted` bytes of heap would be carved, or `None`.
///
/// Reads only, so the write path can cost a relocation before its record is in
/// the log. [`LeafMut::carve_heap`] performs what this costs, and the two must
/// agree: the offset is `heap_start - wanted` in both.
///
/// @param page - the page bytes, exactly one page long
/// @param wanted - how many bytes of heap the caller needs
fn carve_room(page: &[u8], wanted: usize) -> DbResult<Option<usize>> {
    // The format's own rule about where a large value lives. See
    // `LeafMut::overwrite_heap_slot`.
    if wanted > page.len() / crate::leaf::EXTENT_DIVISOR {
        return Ok(None);
    }
    let (delta_start, heap_start, row_count, has_tombstones) = {
        let leaf = LeafRef::parse(page)?;
        (
            leaf.delta_start(),
            leaf.heap_start(),
            leaf.row_count(),
            leaf.has_tombstones(),
        )
    };
    let bitmap = if has_tombstones {
        tombstone_bytes(row_count)
    } else {
        0
    };
    let floor = columns_end(page)?;
    let Some(new_delta_start) = delta_start.checked_sub(wanted) else {
        return Ok(None);
    };
    if new_delta_start.saturating_sub(bitmap) < floor {
        return Ok(None);
    }
    Ok(Some(heap_start.saturating_sub(wanted)))
}

/// Returns where one row's slot for one column sits in the page.
///
/// @param page - the page bytes, exactly one page long
/// @param column - which column
/// @param row - the row's position in the sorted region
fn slot_at(page: &[u8], column: usize, row: usize) -> DbResult<usize> {
    let (row_count, width, stride) = {
        let leaf = LeafRef::parse(page)?;
        (
            leaf.row_count(),
            leaf.column_width(column)?,
            leaf.directory_entry_size(),
        )
    };
    let entry = leaf_header::DIRECTORY.saturating_add(column.saturating_mul(stride));
    let base = page::read_u32(page, entry.saturating_add(4))? as usize;
    Ok(base
        .saturating_add(class_bytes(row_count))
        .saturating_add(row.saturating_mul(width)))
}

/// Reports whether [`LeafMut::update_slot`] would write, without writing.
///
/// **Because the alternative was a copy of the whole page.** A write may not
/// change a page before its record is in the log, and the way `update_in_place`
/// used to find out whether the slot write applied was to run it against
/// `guard.bytes().to_vec()` - a 32 KiB allocation and a 32 KiB copy for every
/// in-place update, and the gate's `txn.large` is two thousand of them in one
/// transaction. Asking the same questions without writing costs a page parse and
/// some arithmetic.
///
/// **Every refusal here is one `update_slot` also makes, in the same order**,
/// and it has to stay that way: the caller logs the record on the strength of a
/// `true` from this and then applies it for real, so a disagreement would be a
/// record in the log describing a write that did not happen. `redo` now fails
/// rather than continuing when the replayed `update_slot` answers `NoRoom`,
/// which is what would catch such a disagreement.
///
/// @param page - the page bytes, exactly one page long
/// @param column - which column would be written
/// @param row - the row's position in the sorted region
/// @param value - the new value
pub fn would_update_slot(
    page: &[u8],
    column: usize,
    row: usize,
    value: &Datum<'_>,
) -> DbResult<bool> {
    let (spec, width, frame, class) = {
        let leaf = LeafRef::parse(page)?;
        if row >= leaf.row_count() {
            return Err(misuse(format!("row {row} is not in the sorted region")));
        }
        (
            leaf.spec(column)?,
            leaf.column_width(column)?,
            leaf.column_base(column)?,
            leaf.column(column)?.class_at(row)?,
        )
    };
    if class != crate::types::ValueClass::Typed {
        return Ok(false);
    }
    match (spec.physical, value) {
        (PhysicalType::Int64, Datum::Int(number)) => {
            Ok(crate::leaf::fits_frame(frame, width, *number))
        }
        (PhysicalType::Float64, Datum::Real(_) | Datum::Int(_)) => Ok(true),
        (PhysicalType::Text, Datum::Text(bytes)) | (PhysicalType::Blob, Datum::Blob(bytes)) => {
            let at = slot_at(page, column, row)?;
            let slot = page
                .get(at..at.saturating_add(width))
                .ok_or_else(|| corrupt("a heap slot runs past the page"))?;
            let (_, length) = crate::types::read_heap_slot(slot);
            if bytes.len() <= length {
                return Ok(true);
            }
            Ok(carve_room(page, bytes.len())?.is_some())
        }
        _ => Ok(false),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::leaf::LeafBuilder;

    /// The three-column shape the tests share: an integer key, a text and an
    /// integer.
    fn columns() -> Vec<ColumnSpec> {
        vec![
            ColumnSpec::key(PhysicalType::Int64),
            ColumnSpec::new(PhysicalType::Text),
            ColumnSpec::new(PhysicalType::Int64),
        ]
    }

    /// Builds a leaf holding `count` rows.
    ///
    /// @param page_size - the page size
    /// @param count - how many rows
    fn leaf_of(page_size: usize, count: i64) -> Vec<u8> {
        let labels: Vec<String> = (0..count).map(|key| format!("row-{key}")).collect();
        let rows: Vec<Vec<Datum<'_>>> = (0..count)
            .map(|key| {
                vec![
                    Datum::Int(key),
                    Datum::Text(
                        labels
                            .get(key as usize)
                            .map(String::as_bytes)
                            .unwrap_or(b""),
                    ),
                    Datum::Int(key * 10),
                ]
            })
            .collect();
        LeafBuilder::new(page_size, 1, columns(), 1)
            .expect("a builder")
            .encode(&rows)
            .expect("a leaf")
    }

    /// A delta insert lands, parses, and reads back.
    #[test]
    fn a_delta_row_lands_and_reads_back() {
        let mut page = leaf_of(1_024, 8);
        let mut leaf = LeafMut::new(&mut page).expect("a leaf");
        assert_eq!(
            leaf.insert_delta(
                &columns(),
                &[Datum::Int(99), Datum::Text(b"inserted"), Datum::Int(990)]
            )
            .expect("an insert"),
            Applied::Yes
        );
        let view = leaf.view().expect("the page still parses");
        assert_eq!(view.delta_count(), 1);
        assert!(view.has_writes());
        assert!(!view.is_clean());
        assert_eq!(view.delta_value(0, 0).expect("a value").as_int(), Some(99));
        assert_eq!(
            view.delta_value(0, 1).expect("a value").as_bytes(),
            Some(&b"inserted"[..])
        );
        view.integrity().expect("the page is sound");
    }

    /// The newest delta row is first, so the first match wins.
    #[test]
    fn the_newest_delta_row_is_first() {
        let mut page = leaf_of(1_024, 4);
        let mut leaf = LeafMut::new(&mut page).expect("a leaf");
        for round in 0..3i64 {
            leaf.insert_delta(
                &columns(),
                &[Datum::Int(50), Datum::Text(b"x"), Datum::Int(round)],
            )
            .expect("an insert");
        }
        let view = leaf.view().expect("the page parses");
        assert_eq!(view.delta_count(), 3);
        assert_eq!(view.delta_value(0, 2).expect("a value").as_int(), Some(2));
        assert_eq!(view.delta_value(2, 2).expect("a value").as_int(), Some(0));
    }

    /// A leaf fills up and says so rather than corrupting itself.
    #[test]
    fn a_full_delta_area_says_no_room() {
        let mut page = leaf_of(512, 6);
        let mut leaf = LeafMut::new(&mut page).expect("a leaf");
        let mut inserted = 0usize;
        loop {
            let outcome = leaf
                .insert_delta(
                    &columns(),
                    &[
                        Datum::Int(1_000 + inserted as i64),
                        Datum::Text(b"padding padding padding padding"),
                        Datum::Int(7),
                    ],
                )
                .expect("an insert");
            if outcome == Applied::NoRoom {
                break;
            }
            inserted = inserted.saturating_add(1);
            assert!(inserted < 1_000, "the page never filled up");
        }
        assert!(inserted > 0, "not even one row fitted");
        let view = leaf.view().expect("the page still parses after a refusal");
        assert_eq!(view.delta_count(), inserted);
        view.integrity().expect("the page is sound");
    }

    /// The delta limit is enforced whatever the page size.
    #[test]
    fn the_delta_limit_is_enforced() {
        let mut page = leaf_of(65_536, 4);
        let mut leaf = LeafMut::new(&mut page).expect("a leaf");
        for round in 0..DELTA_LIMIT {
            assert_eq!(
                leaf.insert_delta(
                    &columns(),
                    &[Datum::Int(1_000 + round as i64), Datum::Null, Datum::Int(1)]
                )
                .expect("an insert"),
                Applied::Yes,
                "row {round} of the limit did not fit a 64 KiB page"
            );
        }
        assert_eq!(
            leaf.insert_delta(&columns(), &[Datum::Int(9_999), Datum::Null, Datum::Int(1)])
                .expect("an insert"),
            Applied::NoRoom,
            "the delta limit was not enforced"
        );
    }

    /// A tombstone hides a row and survives a later delta insert.
    ///
    /// The bitmap moves when the delta area grows, and this is the test that
    /// says it moved with its contents intact rather than being reallocated
    /// empty - which would silently un-delete every row.
    #[test]
    fn a_tombstone_survives_the_delta_area_growing_under_it() {
        let mut page = leaf_of(2_048, 40);
        let mut leaf = LeafMut::new(&mut page).expect("a leaf");
        for row in [0usize, 7, 39] {
            assert_eq!(leaf.set_tombstone(row).expect("a tombstone"), Applied::Yes);
        }
        for round in 0..4i64 {
            assert_eq!(
                leaf.insert_delta(
                    &columns(),
                    &[Datum::Int(500 + round), Datum::Text(b"new"), Datum::Int(1)]
                )
                .expect("an insert"),
                Applied::Yes
            );
        }
        let view = leaf.view().expect("the page parses");
        for row in 0..40usize {
            let expected = matches!(row, 0 | 7 | 39);
            assert_eq!(
                view.is_tombstoned(row).expect("a bit"),
                expected,
                "row {row}'s tombstone did not survive the delta area moving"
            );
        }
        assert_eq!(view.live_rows().expect("a count"), 40 - 3 + 4);
        view.integrity().expect("the page is sound");
    }

    /// Clearing a tombstone brings the row back.
    #[test]
    fn clearing_a_tombstone_brings_the_row_back() {
        let mut page = leaf_of(1_024, 16);
        let mut leaf = LeafMut::new(&mut page).expect("a leaf");
        leaf.set_tombstone(3).expect("a tombstone");
        assert!(leaf.view().unwrap().is_tombstoned(3).unwrap());
        leaf.clear_tombstone(3).expect("cleared");
        assert!(!leaf.view().unwrap().is_tombstoned(3).unwrap());
        // Clearing on a leaf with no bitmap at all is a no-op, not an error.
        let mut fresh = leaf_of(1_024, 4);
        LeafMut::new(&mut fresh)
            .expect("a leaf")
            .clear_tombstone(1)
            .expect("a no-op");
    }

    /// Removing a delta row leaves the others readable and the bitmap intact.
    #[test]
    fn removing_a_delta_row_keeps_the_rest() {
        let mut page = leaf_of(2_048, 24);
        let mut leaf = LeafMut::new(&mut page).expect("a leaf");
        leaf.set_tombstone(5).expect("a tombstone");
        for round in 0..5i64 {
            leaf.insert_delta(
                &columns(),
                &[
                    Datum::Int(200 + round),
                    Datum::Text(b"d"),
                    Datum::Int(round),
                ],
            )
            .expect("an insert");
        }
        // Newest first, so index 0 holds round 4.
        leaf.remove_delta(2).expect("a removal");
        let view = leaf.view().expect("the page parses");
        assert_eq!(view.delta_count(), 4);
        let remaining: Vec<i64> = (0..4)
            .map(|index| view.delta_value(index, 2).unwrap().as_int().unwrap_or(-1))
            .collect();
        assert_eq!(remaining, vec![4, 3, 1, 0], "the wrong row was removed");
        assert!(view.is_tombstoned(5).unwrap(), "the bitmap was lost");
        view.integrity().expect("the page is sound");
        assert!(leaf.remove_delta(9).is_err());
    }

    /// Emptying the delta area clears its flag.
    #[test]
    fn emptying_the_delta_area_clears_its_flag() {
        let mut page = leaf_of(1_024, 8);
        let mut leaf = LeafMut::new(&mut page).expect("a leaf");
        leaf.insert_delta(&columns(), &[Datum::Int(60), Datum::Null, Datum::Int(1)])
            .expect("an insert");
        assert!(leaf.view().unwrap().has_writes());
        leaf.remove_delta(0).expect("a removal");
        let view = leaf.view().expect("the page parses");
        assert_eq!(view.delta_count(), 0);
        assert!(
            !view.has_writes(),
            "an emptied delta area still says it has writes"
        );
        view.integrity().expect("the page is sound");
    }

    /// A fixed-width slot update lands, and anything else says so.
    #[test]
    fn a_slot_update_lands_and_the_rest_refuses() {
        let mut page = leaf_of(1_024, 8);
        // The counter column holds 0, 10, ..., 70, so a leaf that narrows its
        // slots gives it one byte and a four-figure value does not fit. What
        // this test is about is that a slot update lands, so it writes a value
        // the leaf's own width admits - and the refusal of one that does not is
        // `a_value_too_wide_for_the_slot_is_refused`.
        let replacement = if crate::leaf::NARROW_INT_SLOTS {
            42
        } else {
            4_242
        };
        let mut leaf = LeafMut::new(&mut page).expect("a leaf");
        assert_eq!(
            leaf.update_slot(2, 3, &Datum::Int(replacement))
                .expect("an update"),
            Applied::Yes
        );
        let view = leaf.view().expect("the page parses");
        assert_eq!(
            view.value(3, 2).expect("a value").as_int(),
            Some(replacement)
        );
        assert!(
            view.is_clean(),
            "a slot update took the leaf off the fast path"
        );

        // A longer text is written at the bottom of the heap and the slot
        // follows it. Refusing that case and falling back to delete plus insert
        // is what had taken the `transaction` benchmark family off its floor.
        // The leaf stays on the fast path: nothing went into the delta area and
        // nothing was tombstoned.
        assert_eq!(
            leaf.update_slot(1, 3, &Datum::Text(b"longer than before"))
                .expect("an update"),
            Applied::Yes
        );
        let view = leaf.view().expect("the page parses");
        match view.value(3, 1).expect("a value") {
            Datum::Text(bytes) => assert_eq!(bytes, b"longer than before"),
            other => panic!("row 3 column 1 is {other:?}, not text"),
        }
        assert!(
            view.is_clean(),
            "a relocating slot update took the leaf off the fast path"
        );
        // A NULL is a class change, so it refuses.
        assert_eq!(
            leaf.update_slot(2, 3, &Datum::Null).expect("an update"),
            Applied::NoRoom
        );
        assert!(leaf.update_slot(2, 99, &Datum::Int(1)).is_err());
    }

    /// An integer written into a `Float64` column is converted, not refused.
    /// A value too wide for the leaf's own integer slot is refused rather than
    /// truncated, and the page is left exactly as it was.
    ///
    /// The caller's route out is the one it already has for a slot it cannot
    /// overwrite: delete the row, put the new one in the delta area, and let
    /// the next compaction pick a width from the values it then holds.
    #[test]
    fn a_value_too_wide_for_the_slot_is_refused() {
        if !crate::leaf::NARROW_INT_SLOTS {
            inillucent_base::testing::skipping("narrow integer slots are compiled out");
            return;
        }
        let mut page = leaf_of(4096, 8);
        let before = page.clone();
        let width = LeafRef::parse(&page).unwrap().column_width(2).unwrap();
        assert_eq!(width, 1, "a column of small counters should be one byte");
        let applied = LeafMut::new(&mut page)
            .unwrap()
            .update_slot(2, 0, &Datum::Int(1_000_000))
            .unwrap();
        assert_eq!(applied, Applied::NoRoom, "a wide value was written anyway");
        assert_eq!(page, before, "a refused update still changed the page");
        // A value that does fit still lands.
        let applied = LeafMut::new(&mut page)
            .unwrap()
            .update_slot(2, 0, &Datum::Int(-42))
            .unwrap();
        assert_eq!(applied, Applied::Yes);
        assert_eq!(
            LeafRef::parse(&page)
                .unwrap()
                .value(0, 2)
                .unwrap()
                .as_int()
                .unwrap(),
            -42
        );
    }

    #[test]
    fn an_integer_into_a_real_column_is_converted() {
        let specs = vec![
            ColumnSpec::key(PhysicalType::Int64),
            ColumnSpec::new(PhysicalType::Float64),
        ];
        let rows: Vec<Vec<Datum<'_>>> = (0..4i64)
            .map(|key| vec![Datum::Int(key), Datum::Real(key as f64)])
            .collect();
        let mut page = LeafBuilder::new(1_024, 1, specs, 1)
            .expect("a builder")
            .encode(&rows)
            .expect("a leaf");
        let mut leaf = LeafMut::new(&mut page).expect("a leaf");
        assert_eq!(
            leaf.update_slot(1, 2, &Datum::Int(7)).expect("an update"),
            Applied::Yes
        );
        assert_eq!(
            leaf.view().unwrap().value(2, 1).expect("a value").as_f64(),
            Some(7.0)
        );
    }

    /// `max_cts` only ever moves forward.
    #[test]
    fn max_cts_only_moves_forward() {
        let mut page = leaf_of(1_024, 4);
        let mut leaf = LeafMut::new(&mut page).expect("a leaf");
        leaf.set_max_cts(10).expect("a timestamp");
        assert_eq!(leaf.view().unwrap().max_cts(), 10);
        leaf.set_max_cts(4).expect("a timestamp");
        assert_eq!(
            leaf.view().unwrap().max_cts(),
            10,
            "max_cts went backwards, which would make a reader miss a change"
        );
        leaf.set_max_cts(11).expect("a timestamp");
        assert_eq!(leaf.view().unwrap().max_cts(), 11);
    }

    /// A row of the wrong width is refused rather than half written.
    #[test]
    fn a_row_of_the_wrong_width_is_refused() {
        let mut page = leaf_of(1_024, 4);
        let before = page.clone();
        let mut leaf = LeafMut::new(&mut page).expect("a leaf");
        assert!(leaf.insert_delta(&columns(), &[Datum::Int(1)]).is_err());
        assert_eq!(page, before, "a refused insert changed the page");
    }

    /// A page that is not a leaf is refused before anything is written.
    #[test]
    fn a_page_that_is_not_a_leaf_is_refused() {
        let mut page = vec![0u8; 512];
        page::write_common(&mut page, crate::page::PageKind::Interior, 1, 1).expect("a header");
        assert!(LeafMut::new(&mut page).is_err());
    }

    /// Reads one row's text out of a leaf, so a test can assert on the value
    /// rather than on the fact that a call returned.
    ///
    /// @param page - the page bytes
    /// @param row - the row's position in the sorted region
    fn text_at(page: &[u8], row: usize) -> Vec<u8> {
        let leaf = LeafRef::parse(page).expect("a leaf");
        match leaf.value(row, 1).expect("a value") {
            Datum::Text(bytes) => bytes.to_vec(),
            other => panic!("row {row} column 1 is {other:?}, not text"),
        }
    }

    /// A shorter value is written where the old one lay and the slot shortens.
    ///
    /// The rows either side are read back too: a length written into the slot
    /// without the bytes going with it would leave this row right and its
    /// neighbours reading into the middle of a value.
    #[test]
    fn a_shorter_text_is_written_in_place() {
        let mut page = leaf_of(4_096, 40);
        let before_heap = LeafRef::parse(&page).expect("a leaf").heap_start();
        {
            let mut leaf = LeafMut::new(&mut page).expect("a leaf");
            assert_eq!(
                leaf.update_slot(1, 7, &Datum::Text(b"ab"))
                    .expect("applies"),
                Applied::Yes
            );
        }
        assert_eq!(text_at(&page, 7), b"ab");
        assert_eq!(text_at(&page, 6), b"row-6");
        assert_eq!(text_at(&page, 8), b"row-8");
        assert_eq!(
            LeafRef::parse(&page).expect("a leaf").heap_start(),
            before_heap,
            "a shorter value should not move the heap"
        );
    }

    /// A longer value is written at the bottom of the heap and the slot follows.
    #[test]
    fn a_longer_text_is_relocated_within_the_leaf() {
        let mut page = leaf_of(4_096, 40);
        let before_heap = LeafRef::parse(&page).expect("a leaf").heap_start();
        let longer = b"a much longer label than the one it replaces";
        {
            let mut leaf = LeafMut::new(&mut page).expect("a leaf");
            assert_eq!(
                leaf.update_slot(1, 7, &Datum::Text(longer))
                    .expect("applies"),
                Applied::Yes
            );
        }
        assert_eq!(text_at(&page, 7), longer.to_vec());
        assert_eq!(text_at(&page, 6), b"row-6");
        assert_eq!(text_at(&page, 8), b"row-8");
        assert_eq!(text_at(&page, 39), b"row-39");
        let after = LeafRef::parse(&page).expect("a leaf");
        assert_eq!(
            after.heap_start(),
            before_heap.saturating_sub(longer.len()),
            "the heap should have grown down by exactly the new value"
        );
    }

    /// A relocation moves the delta area with it and every delta row survives.
    ///
    /// **The case the header field alone would not catch.** `carve_heap` lowers
    /// `delta_start` and moves the bytes under it by the same amount; getting
    /// one of those without the other leaves a delta area that parses and reads
    /// somebody else's bytes.
    #[test]
    fn a_relocation_carries_the_delta_area_with_it() {
        let mut page = leaf_of(4_096, 40);
        {
            let mut leaf = LeafMut::new(&mut page).expect("a leaf");
            for key in 100..104i64 {
                assert_eq!(
                    leaf.insert_delta(
                        &columns(),
                        &[Datum::Int(key), Datum::Text(b"delta"), Datum::Int(key)],
                    )
                    .expect("inserts"),
                    Applied::Yes
                );
            }
            assert_eq!(
                leaf.update_slot(1, 7, &Datum::Text(b"a much longer label indeed"))
                    .expect("applies"),
                Applied::Yes
            );
        }
        let leaf = LeafRef::parse(&page).expect("a leaf");
        assert_eq!(
            leaf.delta_count(),
            4,
            "the delta rows should still be there"
        );
        for index in 0..4 {
            match leaf.delta_value(index, 1).expect("a delta value") {
                Datum::Text(bytes) => assert_eq!(bytes, b"delta"),
                other => panic!("delta row {index} column 1 is {other:?}"),
            }
        }
        let _ = leaf;
        assert_eq!(text_at(&page, 7), b"a much longer label indeed");
    }

    /// A value with nowhere to go is refused and the page is left alone.
    #[test]
    fn a_relocation_with_no_room_is_refused() {
        let mut page = leaf_of(1_024, 40);
        let before = page.clone();
        let enormous = vec![b'x'; 900];
        {
            let mut leaf = LeafMut::new(&mut page).expect("a leaf");
            assert_eq!(
                leaf.update_slot(1, 7, &Datum::Text(&enormous))
                    .expect("answers"),
                Applied::NoRoom
            );
        }
        assert_eq!(page, before, "a refused update changed the page");
    }

    /// A value that belongs in an extent is refused rather than moved inline.
    ///
    /// The format's rule is that a value longer than `page_size /
    /// EXTENT_DIVISOR` lives out of line, and a relocation is the one write that
    /// could put a large value inline behind the packer's back.
    #[test]
    fn a_value_over_the_extent_threshold_is_refused() {
        let mut page = leaf_of(8_192, 4);
        let before = page.clone();
        let over = vec![b'x'; 8_192 / crate::leaf::EXTENT_DIVISOR + 1];
        {
            let mut leaf = LeafMut::new(&mut page).expect("a leaf");
            assert_eq!(
                leaf.update_slot(1, 1, &Datum::Text(&over))
                    .expect("answers"),
                Applied::NoRoom
            );
        }
        assert_eq!(page, before, "a refused update changed the page");
    }

    /// The read-only costing agrees with the write, which is what the log rests
    /// on: the record is written on the strength of `would_update_slot` and then
    /// applied by `update_slot`.
    #[test]
    fn the_costing_agrees_with_the_write() {
        for (page_size, length) in [
            (4_096usize, 2usize),
            (4_096, 44),
            (1_024, 900),
            (8_192, 1_100),
        ] {
            let mut page = leaf_of(page_size, 40);
            let value = vec![b'x'; length];
            let costed = would_update_slot(&page, 1, 7, &Datum::Text(&value)).expect("costs");
            let applied = LeafMut::new(&mut page)
                .expect("a leaf")
                .update_slot(1, 7, &Datum::Text(&value))
                .expect("answers");
            assert_eq!(
                costed,
                applied == Applied::Yes,
                "page {page_size}, value {length}: costed {costed} but applied {applied:?}"
            );
        }
    }
}
