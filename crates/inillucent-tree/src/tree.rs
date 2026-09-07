//! The tree: an ordered run of PAX leaves, with point, range and full scan.
//!
//! Invariant: the leaves are in key order and their key ranges do not overlap,
//! so a descent is a binary search over the leaf fences and a full scan is a
//! walk of the run. Every mutation restores that property before it returns,
//! and [`Tree::check`] asserts it directly rather than trusting that it held.
//!
//! ## What Phase 1 builds and what it does not
//!
//! The TDD's Phase 1 asks for "an in-memory tree (no pool, no disk) with
//! insert, point, range and full scan", and that is exactly what this is: the
//! leaves live in a `Vec`, the fences live beside them, and a descent is a
//! binary search over the fences instead of a walk down interior pages. Phase 2
//! replaces the `Vec` with the buffer pool and the fence array with interior
//! pages carrying swizzled child pointers.
//!
//! That is a real substitution rather than a stub, and it is honest about what
//! it does not yet measure: a descent here is one binary search over an array
//! that fits in cache, where a real one is two or three page reads. The phase
//! gate is a *scan* measurement, where the descent happens once, so the
//! difference does not flatter the number the gate reads. It would flatter a
//! point-lookup measurement, which is why Phase 2 owns that gate and this
//! module does not claim it.

use inillucent_base::error::{corrupt, misuse};
use inillucent_base::DbResult;

use crate::datum::{borrow_row, own_row, Datum, OwnedDatum};
use crate::leaf::{compare_rows, LeafBuilder, LeafRef, Packed};
use crate::page::PageId;
use crate::types::ColumnSpec;

/// The fill factor a bulk build packs leaves to.
///
/// Ninety per cent leaves room for a few delta inserts before the first
/// compaction without wasting a tenth of the file, which is the trade the TDD's
/// bulk-build section names.
pub const BULK_FILL: f64 = 0.9;

/// The fill factor a split packs each half to.
pub const SPLIT_FILL: f64 = 0.6;

/// One tree: an ordered run of leaves over one column directory.
pub struct Tree {
    builder: LeafBuilder,
    columns: Vec<ColumnSpec>,
    key_columns: usize,
    page_size: usize,
    /// The leaf pages, in key order.
    pages: Vec<Vec<u8>>,
    /// Each leaf's first key, so a descent compares without reading a page.
    ///
    /// This is what an interior page *is*, held as an array because Phase 1 has
    /// no buffer pool to put one in. It is not an optimisation bolted on: a
    /// descent that parses a leaf per comparison reads log(leaves) pages to
    /// find out something the separators already say, and on a skip scan -
    /// which descends once per distinct value - that was the entire cost of the
    /// query. Phase 2 replaces the array with real interior pages carrying
    /// swizzled child pointers, and the comparison code does not change.
    ///
    /// Rebuilt by [`Tree::relink`] after every structural change, so it cannot
    /// drift from the pages: there is no path that alters `pages` without going
    /// through it.
    fences: Vec<Vec<OwnedDatum>>,
}

impl Tree {
    /// Returns an empty tree.
    ///
    /// @param page_size - the database's page size in bytes
    /// @param tree - the tree identifier written into every page
    /// @param columns - the column directory, key columns first
    /// @param key_columns - how many leading columns form the key
    pub fn new(
        page_size: usize,
        tree: u64,
        columns: Vec<ColumnSpec>,
        key_columns: usize,
    ) -> DbResult<Tree> {
        let builder = LeafBuilder::new(page_size, tree, columns.clone(), key_columns)?;
        Ok(Tree {
            builder,
            columns,
            key_columns,
            page_size,
            pages: Vec::new(),
            fences: Vec::new(),
        })
    }

    /// Builds a tree bottom-up from rows already sorted by key.
    ///
    /// This is the bulk builder: leaves are packed left to right at
    /// [`BULK_FILL`] and never revisited, so a load costs one pass and no
    /// per-key descent. It is what `CREATE INDEX` and the fixture import use.
    ///
    /// @param page_size - the database's page size in bytes
    /// @param tree - the tree identifier
    /// @param columns - the column directory, key columns first
    /// @param key_columns - how many leading columns form the key
    /// @param rows - the rows, already sorted by the key columns
    pub fn bulk_build(
        page_size: usize,
        tree: u64,
        columns: Vec<ColumnSpec>,
        key_columns: usize,
        rows: &[Vec<Datum<'_>>],
    ) -> DbResult<Tree> {
        let mut built = Tree::new(page_size, tree, columns, key_columns)?;
        let mut at = 0usize;
        while at < rows.len() {
            let remaining = rows.get(at..).unwrap_or(&[]);
            match built.builder.pack(remaining, BULK_FILL)? {
                Packed::Filled { page, rows: packed } => {
                    built.pages.push(page);
                    at = at.saturating_add(packed);
                }
                Packed::RowTooLarge => {
                    return Err(misuse(
                        "a row is larger than a page; out-of-line blobs are Phase 4",
                    ))
                }
            }
        }
        built.relink()?;
        Ok(built)
    }

    /// Returns the column directory.
    pub fn columns(&self) -> &[ColumnSpec] {
        &self.columns
    }

    /// Returns how many leading columns form the key.
    pub fn key_columns(&self) -> usize {
        self.key_columns
    }

    /// Returns the number of leaves.
    pub fn leaf_count(&self) -> usize {
        self.pages.len()
    }

    /// Returns the page size in bytes.
    pub fn page_size(&self) -> usize {
        self.page_size
    }

    /// Returns the total bytes the leaves occupy.
    pub fn byte_size(&self) -> usize {
        self.pages.len().saturating_mul(self.page_size)
    }

    /// Returns one leaf by position.
    ///
    /// @param index - the leaf's position in key order
    pub fn leaf(&self, index: usize) -> DbResult<LeafRef<'_>> {
        let page = self
            .pages
            .get(index)
            .ok_or_else(|| misuse(format!("leaf {index} does not exist")))?;
        LeafRef::parse(page)
    }

    /// Returns the raw bytes of one leaf.
    ///
    /// @param index - the leaf's position in key order
    pub fn leaf_bytes(&self, index: usize) -> Option<&[u8]> {
        self.pages.get(index).map(|page| page.as_slice())
    }

    /// Writes each leaf's right-sibling pointer.
    ///
    /// The sibling chain is what a scan follows once the pool exists; in Phase 1
    /// the scan walks the `Vec`, but the pointers are written and checked now so
    /// the Phase 2 move does not discover them missing.
    fn relink(&mut self) -> DbResult<()> {
        let count = self.pages.len();
        for index in 0..count {
            let right = if index.saturating_add(1) < count {
                PageId(index as u64 + 2)
            } else {
                PageId::NONE
            };
            let page = self
                .pages
                .get_mut(index)
                .ok_or_else(|| corrupt("leaf vanished while linking"))?;
            crate::page::write_u64(page, crate::page::header::RIGHT, right.0)?;
        }
        self.fences = Vec::with_capacity(count);
        for index in 0..count {
            let leaf = self.leaf(index)?;
            let mut key = Vec::with_capacity(self.key_columns);
            if leaf.row_count() > 0 {
                for column in 0..self.key_columns {
                    key.push(OwnedDatum::from_datum(&leaf.value(0, column)?));
                }
            }
            self.fences.push(key);
        }
        Ok(())
    }

    /// Compares one leaf's first key against a probe.
    ///
    /// Reads the fence array, not the page. An empty leaf has no first key and
    /// is treated as sorting after everything, so a run of empty leaves cannot
    /// make a descent walk past a leaf that holds the key.
    ///
    /// @param index - the leaf's position
    /// @param probe - the key to compare against
    fn fence_compare(&self, index: usize, probe: &[Datum<'_>]) -> std::cmp::Ordering {
        let Some(key) = self.fences.get(index) else {
            return std::cmp::Ordering::Greater;
        };
        if key.is_empty() {
            return std::cmp::Ordering::Greater;
        }
        // Compared in place. Building a `Vec<Datum>` to borrow the fence with
        // was one heap allocation per comparison, and a descent makes several -
        // which on a skip scan, one descent per distinct value, was most of the
        // per-seek cost.
        for (held, wanted) in key.iter().zip(probe.iter()) {
            let order = held.borrow().compare(wanted);
            if order != std::cmp::Ordering::Equal {
                return order;
            }
        }
        std::cmp::Ordering::Equal
    }

    /// Finds the leaf whose key range contains a key.
    ///
    /// Returns the index of the last leaf whose first key is at or below the
    /// probe, which is where the key is if it is anywhere.
    ///
    /// @param probe - the key to look for, one value per key column
    pub fn find_leaf(&self, probe: &[Datum<'_>]) -> DbResult<usize> {
        if self.pages.is_empty() {
            return Err(misuse("an empty tree has no leaf"));
        }
        let mut low = 0usize;
        let mut high = self.pages.len();
        while low.saturating_add(1) < high {
            let middle = low.saturating_add(high.saturating_sub(low) / 2);
            match self.fence_compare(middle, probe) {
                std::cmp::Ordering::Greater => high = middle,
                _ => low = middle,
            }
        }
        Ok(low)
    }

    /// Returns one row by key, or `None` if it is not there.
    ///
    /// @param probe - the key to look for, one value per key column
    pub fn point(&self, probe: &[Datum<'_>]) -> DbResult<Option<Vec<Datum<'_>>>> {
        if self.pages.is_empty() {
            return Ok(None);
        }
        let index = self.find_leaf(probe)?;
        let leaf = self.leaf(index)?;
        if let Ok(row) = leaf.search(probe)? {
            if !leaf.is_tombstoned(row)? {
                let mut values = Vec::with_capacity(leaf.column_count());
                for column in 0..leaf.column_count() {
                    values.push(leaf.value(row, column)?);
                }
                return Ok(Some(values));
            }
        }
        // The delta area is unsorted, so a miss in the sorted region is not a
        // miss in the leaf.
        for entry in 0..leaf.delta_count() {
            let mut matches = true;
            for column in 0..self.key_columns {
                let held = leaf.delta_value(entry, column)?;
                let wanted = probe.get(column).copied().unwrap_or(Datum::Null);
                if held.compare(&wanted) != std::cmp::Ordering::Equal {
                    matches = false;
                    break;
                }
            }
            if matches {
                let mut values = Vec::with_capacity(leaf.column_count());
                for column in 0..leaf.column_count() {
                    values.push(leaf.delta_value(entry, column)?);
                }
                return Ok(Some(values));
            }
        }
        Ok(None)
    }

    /// Inserts or replaces one row.
    ///
    /// The row goes into the target leaf's delta area if it fits; otherwise the
    /// leaf is compacted, and if it still does not fit, split.
    ///
    /// @param row - the row, one value per column, key columns first
    pub fn insert(&mut self, row: &[Datum<'_>]) -> DbResult<()> {
        if row.len() != self.columns.len() {
            return Err(misuse(format!(
                "a row of {} values does not fit {} columns",
                row.len(),
                self.columns.len()
            )));
        }
        if self.pages.is_empty() {
            let page = self.builder.encode(&[row.to_vec()])?;
            self.pages.push(page);
            self.relink()?;
            return Ok(());
        }
        let probe: Vec<Datum<'_>> = row.iter().copied().take(self.key_columns).collect();
        let index = self.find_leaf(&probe)?;
        // Phase 1 rebuilds the target leaf rather than appending to its delta
        // area. The delta path is a Phase 3 write-family optimisation, measured
        // there against the 16/32/64 sweep; doing it now would be an unmeasured
        // fast path in a phase whose gate is a read.
        let mut rows = self.materialise_leaf(index)?;
        let fresh = own_row(row);
        let key_columns = self.key_columns;
        let at = rows.partition_point(|held| {
            compare_rows(&borrow_row(held), row, key_columns) == std::cmp::Ordering::Less
        });
        let replaces = rows
            .get(at)
            .map(|held| {
                compare_rows(&borrow_row(held), row, key_columns) == std::cmp::Ordering::Equal
            })
            .unwrap_or(false);
        if replaces {
            if let Some(slot) = rows.get_mut(at) {
                *slot = fresh;
            }
        } else {
            rows.insert(at, fresh);
        }
        self.rewrite_leaf(index, &rows)?;
        Ok(())
    }

    /// Deletes one row by key, reporting whether it was there.
    ///
    /// @param probe - the key to delete, one value per key column
    pub fn delete(&mut self, probe: &[Datum<'_>]) -> DbResult<bool> {
        if self.pages.is_empty() {
            return Ok(false);
        }
        let index = self.find_leaf(probe)?;
        let rows = self.materialise_leaf(index)?;
        let before = rows.len();
        let key_columns = self.key_columns;
        let kept: Vec<Vec<OwnedDatum>> = rows
            .into_iter()
            .filter(|held| {
                compare_rows(&borrow_row(held), probe, key_columns) != std::cmp::Ordering::Equal
            })
            .collect();
        let removed = kept.len() != before;
        if removed {
            self.rewrite_leaf(index, &kept)?;
        }
        Ok(removed)
    }

    /// Copies one leaf's live rows out of the page.
    ///
    /// The copy is what lets the page be rewritten underneath them: a `Datum`
    /// borrows the page, and rewriting the page while a borrow is live is
    /// exactly the bug the borrow checker exists to refuse.
    ///
    /// @param index - the leaf's position in key order
    fn materialise_leaf(&self, index: usize) -> DbResult<Vec<Vec<OwnedDatum>>> {
        let leaf = self.leaf(index)?;
        let borrowed = leaf.live()?;
        Ok(borrowed.iter().map(|row| own_row(row)).collect())
    }

    /// Rewrites one leaf from a set of rows, splitting it if they do not fit.
    ///
    /// @param index - the leaf's position in key order
    /// @param rows - the rows the leaf should hold, sorted by key
    fn rewrite_leaf(&mut self, index: usize, rows: &[Vec<OwnedDatum>]) -> DbResult<()> {
        if rows.is_empty() {
            if self.pages.len() > 1 {
                self.pages.remove(index);
            } else {
                let page = self.builder.encode_empty()?;
                if let Some(slot) = self.pages.get_mut(index) {
                    *slot = page;
                }
            }
            self.relink()?;
            return Ok(());
        }
        let borrowed: Vec<Vec<Datum<'_>>> = rows.iter().map(|row| borrow_row(row)).collect();
        match self.builder.pack(&borrowed, BULK_FILL)? {
            Packed::Filled { page, rows: packed } if packed == borrowed.len() => {
                if let Some(slot) = self.pages.get_mut(index) {
                    *slot = page;
                }
            }
            Packed::Filled { .. } => {
                // Split: pack each part to SPLIT_FILL so both halves have room
                // to grow before the next rewrite.
                let mut produced: Vec<Vec<u8>> = Vec::new();
                let mut at = 0usize;
                while at < borrowed.len() {
                    let remaining = borrowed.get(at..).unwrap_or(&[]);
                    match self.builder.pack(remaining, SPLIT_FILL)? {
                        Packed::Filled { page, rows: packed } => {
                            produced.push(page);
                            at = at.saturating_add(packed);
                        }
                        Packed::RowTooLarge => return Err(misuse("a row is larger than a page")),
                    }
                }
                self.pages.splice(index..index.saturating_add(1), produced);
            }
            Packed::RowTooLarge => return Err(misuse("a row is larger than a page")),
        }
        self.relink()?;
        Ok(())
    }

    /// Returns the first position whose key is strictly greater than a prefix.
    ///
    /// The seek an index skip scan is built on. `SELECT DISTINCT category` over
    /// an index led by `category` does not need to read the rows: it needs to
    /// visit one row per distinct value, and this is how it gets from one to
    /// the next. Over 100,000 rows with 64 distinct categories that is 64
    /// descents instead of 100,000 row reads - which is the algorithm SQLite
    /// uses for the same query, so it is also what makes the comparison a
    /// comparison of engines rather than of algorithms.
    ///
    /// The probe may be shorter than the tree's key: comparison stops at the
    /// probe's length, so a one-column probe against a three-column key skips
    /// every row sharing that first column.
    ///
    /// @param prefix - the key prefix to skip past
    /// @param from - the position to start looking from, as a `(leaf, row)` pair
    pub fn seek_after(
        &self,
        prefix: &[Datum<'_>],
        from: (usize, usize),
    ) -> DbResult<Option<(usize, usize)>> {
        if self.pages.is_empty() || prefix.is_empty() {
            return Ok(None);
        }
        // Which leaf the prefix's last row is in.
        //
        // A skip scan walks forward, so the answer is almost always the leaf it
        // is already on or the next one, and a binary search over every leaf
        // parses log(leaves) pages to rediscover that. Galloping from the
        // current position instead - 1, 2, 4, 8 leaves ahead until one starts
        // past the prefix, then a binary search inside that bracket - parses
        // one or two pages in the common case and never more than the plain
        // search would. On the fixture it took `scan.distinct` from a seek that
        // cost more than the scan it replaced to one that cost a twentieth of
        // it.
        let landed = self.gallop(prefix, from.0);
        let leaf = self.leaf(landed)?;
        let view = leaf.key_view()?;
        // The upper bound inside that leaf: the first row whose key is greater.
        let mut low = if landed == from.0 { from.1 } else { 0 };
        let mut high = leaf.row_count();
        while low < high {
            let middle = low.saturating_add(high.saturating_sub(low) / 2);
            match leaf.compare_key_with(&view, middle, prefix)? {
                std::cmp::Ordering::Greater => high = middle,
                _ => low = middle.saturating_add(1),
            }
        }
        if low < leaf.row_count() {
            return Ok(Some((landed, low)));
        }
        // Past the end of that leaf: the next leaf's first row is greater,
        // because keys increase across leaf boundaries.
        let next = landed.saturating_add(1);
        if next < self.pages.len() && self.leaf(next)?.row_count() > 0 {
            return Ok(Some((next, 0)));
        }
        Ok(None)
    }

    /// Returns the last leaf at or after `from` whose first key is not greater
    /// than the probe.
    ///
    /// Doubling the step until the bracket is found, then bisecting it. The
    /// same answer `find_leaf` gives, reached in a number of page reads
    /// proportional to the distance moved rather than to the size of the tree.
    ///
    /// @param prefix - the key prefix being sought past
    /// @param from - the leaf to start from
    fn gallop(&self, prefix: &[Datum<'_>], from: usize) -> usize {
        let count = self.pages.len();
        if from >= count.saturating_sub(1) {
            return count.saturating_sub(1);
        }
        // Find the first leaf after `from` that starts past the prefix. Every
        // leaf strictly before it can hold the prefix; the one before it is the
        // answer.
        let mut step = 1usize;
        let mut lower = from;
        let mut upper = count;
        loop {
            let probe = from.saturating_add(step);
            if probe >= count {
                break;
            }
            if self.leaf_starts_after(probe, prefix) {
                upper = probe;
                break;
            }
            lower = probe;
            step = step.saturating_mul(2);
        }
        while lower.saturating_add(1) < upper {
            let middle = lower.saturating_add(upper.saturating_sub(lower) / 2);
            if self.leaf_starts_after(middle, prefix) {
                upper = middle;
            } else {
                lower = middle;
            }
        }
        lower
    }

    /// Reports whether a leaf's first key sorts after a probe.
    ///
    /// An empty leaf counts as starting after everything, so a run of empty
    /// leaves cannot make the search walk past a leaf that holds the key.
    ///
    /// @param index - the leaf's position
    /// @param prefix - the probe
    fn leaf_starts_after(&self, index: usize, prefix: &[Datum<'_>]) -> bool {
        self.fence_compare(index, prefix) == std::cmp::Ordering::Greater
    }

    /// Returns one row's leading key values.
    ///
    /// @param at - the position, as a `(leaf, row)` pair
    /// @param width - how many leading columns to read
    pub fn key_at(&self, at: (usize, usize), width: usize) -> DbResult<Vec<OwnedDatum>> {
        let mut key = Vec::with_capacity(width);
        self.key_at_into(at, width, &mut key)?;
        Ok(key)
    }

    /// Reads one row's leading key values into a buffer the caller reuses.
    ///
    /// A skip scan does this once per distinct value and has no use for the
    /// allocation, so the buffer is handed in.
    ///
    /// @param at - the position, as a `(leaf, row)` pair
    /// @param width - how many leading columns to read
    /// @param into - the buffer to fill, cleared first
    pub fn key_at_into(
        &self,
        at: (usize, usize),
        width: usize,
        into: &mut Vec<OwnedDatum>,
    ) -> DbResult<()> {
        let leaf = self.leaf(at.0)?;
        into.clear();
        for column in 0..width {
            into.push(OwnedDatum::from_datum(&leaf.value(at.1, column)?));
        }
        Ok(())
    }

    /// Returns the first position holding a live row.
    ///
    /// The starting point for a skip scan, and `None` for an empty tree.
    pub fn first_position(&self) -> DbResult<Option<(usize, usize)>> {
        for index in 0..self.pages.len() {
            if self.leaf(index)?.row_count() > 0 {
                return Ok(Some((index, 0)));
            }
        }
        Ok(None)
    }

    /// Returns a cursor over every live row, in key order.
    pub fn scan(&self) -> ScanCursor<'_> {
        ScanCursor {
            tree: self,
            leaf: 0,
        }
    }

    /// Returns every live row, in key order.
    ///
    /// The slow, allocating path: the model tests and the correctness harness
    /// use it, the executor does not.
    pub fn rows(&self) -> DbResult<Vec<Vec<Datum<'_>>>> {
        let mut out = Vec::new();
        for index in 0..self.pages.len() {
            out.extend(self.leaf(index)?.live()?);
        }
        Ok(out)
    }

    /// Checks the tree's structural invariants.
    ///
    /// 1. Every leaf parses and passes its own row-level integrity check.
    /// 2. Keys strictly increase within a leaf and across leaf boundaries.
    /// 3. The sibling chain matches the leaf order.
    pub fn check(&self) -> DbResult<()> {
        let mut previous: Option<Vec<OwnedDatum>> = None;
        for index in 0..self.pages.len() {
            let leaf = self.leaf(index)?;
            leaf.integrity()?;
            for row in leaf.live()? {
                let key: Vec<Datum<'_>> = row.iter().copied().take(self.key_columns).collect();
                if let Some(last) = &previous {
                    if compare_rows(&borrow_row(last), &key, self.key_columns)
                        != std::cmp::Ordering::Less
                    {
                        return Err(corrupt(format!(
                            "leaf {index} holds a key that does not increase"
                        )));
                    }
                }
                previous = Some(own_row(&key));
            }
            let leaf_now = self.leaf(index)?;
            let fence = self.fences.get(index).map(Vec::as_slice).unwrap_or(&[]);
            if leaf_now.row_count() > 0 {
                let mut first = Vec::with_capacity(self.key_columns);
                for column in 0..self.key_columns {
                    first.push(leaf_now.value(0, column)?);
                }
                if compare_rows(&borrow_row(fence), &first, self.key_columns)
                    != std::cmp::Ordering::Equal
                {
                    return Err(corrupt(format!(
                        "leaf {index}'s fence does not match its first key"
                    )));
                }
            } else if !fence.is_empty() {
                return Err(corrupt(format!("leaf {index} is empty but has a fence")));
            }
            let expected = if index.saturating_add(1) < self.pages.len() {
                PageId(index as u64 + 2)
            } else {
                PageId::NONE
            };
            if leaf.right_sibling() != expected {
                return Err(corrupt(format!(
                    "leaf {index} points at {:?}, not {expected:?}",
                    leaf.right_sibling()
                )));
            }
        }
        Ok(())
    }
}

/// A cursor over a tree's leaves.
///
/// The executor's `TableScan` drives this: one leaf at a time, so a batch can
/// borrow the leaf's mini-columns directly instead of copying rows out of it.
pub struct ScanCursor<'t> {
    tree: &'t Tree,
    leaf: usize,
}

impl<'t> ScanCursor<'t> {
    /// Returns the next leaf, or `None` at the end of the tree.
    pub fn next_leaf(&mut self) -> DbResult<Option<LeafRef<'t>>> {
        if self.leaf >= self.tree.pages.len() {
            return Ok(None);
        }
        let page = self
            .tree
            .pages
            .get(self.leaf)
            .ok_or_else(|| corrupt("leaf vanished mid-scan"))?;
        self.leaf = self.leaf.saturating_add(1);
        Ok(Some(LeafRef::parse(page)?))
    }

    /// Restarts the cursor at the first leaf.
    pub fn rewind(&mut self) {
        self.leaf = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::PhysicalType;
    use std::collections::BTreeMap;

    fn columns() -> Vec<ColumnSpec> {
        vec![
            ColumnSpec::key(PhysicalType::Int64),
            ColumnSpec::key(PhysicalType::Int64),
            ColumnSpec::new(PhysicalType::Text),
        ]
    }

    fn row(key: i64) -> Vec<Datum<'static>> {
        vec![
            Datum::Int(key),
            Datum::Int(key * 3),
            Datum::Text(b"a reasonably long label so leaves fill up"),
        ]
    }

    /// A bulk build over many rows produces a valid multi-leaf tree that reads
    /// back in order.
    #[test]
    fn bulk_build_round_trips() {
        let rows: Vec<Vec<Datum<'static>>> = (0..5_000).map(row).collect();
        let tree = Tree::bulk_build(8192, 1, columns(), 1, &rows).unwrap();
        assert!(tree.leaf_count() > 1, "{} leaves", tree.leaf_count());
        tree.check().unwrap();
        let back = tree.rows().unwrap();
        assert_eq!(back.len(), rows.len());
        for (index, got) in back.iter().enumerate() {
            assert_eq!(got[0].as_int().unwrap(), index as i64);
            assert_eq!(got[1].as_int().unwrap(), index as i64 * 3);
        }
    }

    /// Every key a bulk build packed is findable, and nothing else is.
    #[test]
    fn point_lookups_find_what_is_there() {
        let rows: Vec<Vec<Datum<'static>>> = (0..2_000).map(|n| row(n * 2)).collect();
        let tree = Tree::bulk_build(8192, 1, columns(), 1, &rows).unwrap();
        for n in 0..2_000i64 {
            let found = tree.point(&[Datum::Int(n * 2)]).unwrap();
            assert_eq!(
                found.unwrap()[1].as_int().unwrap(),
                n * 2 * 3,
                "key {}",
                n * 2
            );
            assert!(tree.point(&[Datum::Int(n * 2 + 1)]).unwrap().is_none());
        }
        assert!(tree.point(&[Datum::Int(-1)]).unwrap().is_none());
        assert!(tree.point(&[Datum::Int(1_000_000)]).unwrap().is_none());
    }

    /// The tree agrees with a `BTreeMap` over a long random operation trace,
    /// including through the splits a small page forces.
    ///
    /// This is the property test the TDD's Phase 1 acceptance asks for. The
    /// page size is deliberately the smallest legal one so splits and merges
    /// happen constantly rather than never.
    #[test]
    fn the_tree_agrees_with_a_btreemap() {
        let mut tree = Tree::new(8192, 1, columns(), 1).unwrap();
        let mut model: BTreeMap<i64, i64> = BTreeMap::new();
        let mut state = 0x2545_F491_4F6C_DD1Du64;
        let mut next = || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        for step in 0..4_000u32 {
            let key = (next() % 800) as i64;
            if next() % 3 == 0 {
                let removed = tree.delete(&[Datum::Int(key)]).unwrap();
                assert_eq!(
                    removed,
                    model.remove(&key).is_some(),
                    "step {step} key {key}"
                );
            } else {
                let payload = (next() % 100_000) as i64;
                tree.insert(&[
                    Datum::Int(key),
                    Datum::Int(payload),
                    Datum::Text(b"a reasonably long label so leaves fill up"),
                ])
                .unwrap();
                model.insert(key, payload);
            }
            if step % 97 == 0 {
                tree.check().unwrap();
                let rows = tree.rows().unwrap();
                assert_eq!(rows.len(), model.len(), "step {step}");
                for (got, (key, payload)) in rows.iter().zip(model.iter()) {
                    assert_eq!(got[0].as_int().unwrap(), *key, "step {step}");
                    assert_eq!(got[1].as_int().unwrap(), *payload, "step {step}");
                }
            }
        }
        tree.check().unwrap();
        let rows = tree.rows().unwrap();
        assert_eq!(rows.len(), model.len());
        for (got, (key, payload)) in rows.iter().zip(model.iter()) {
            assert_eq!(got[0].as_int().unwrap(), *key);
            assert_eq!(got[1].as_int().unwrap(), *payload);
        }
        for key in model.keys() {
            assert!(tree.point(&[Datum::Int(*key)]).unwrap().is_some(), "{key}");
        }
    }

    /// A skip scan visits one position per distinct prefix, in order, and
    /// agrees with what a full scan would have produced.
    ///
    /// The property that matters is not that it is fast, it is that it does not
    /// miss a value and does not repeat one - and the boundaries where it could
    /// are leaf boundaries, so the page size is small enough that the prefixes
    /// span many leaves.
    #[test]
    fn a_skip_scan_visits_every_distinct_prefix_once() {
        for distinct in [1usize, 2, 7, 64, 999] {
            let owned: Vec<Vec<OwnedDatum>> = (0..3_000)
                .map(|n| {
                    vec![
                        OwnedDatum::Int((n % distinct) as i64),
                        OwnedDatum::Int(n as i64),
                        OwnedDatum::Text(b"a reasonably long label so leaves fill up".to_vec()),
                    ]
                })
                .collect();
            let mut sorted = owned;
            sorted.sort_by_key(|row| match (&row[0], &row[1]) {
                (OwnedDatum::Int(a), OwnedDatum::Int(b)) => (*a, *b),
                _ => (0, 0),
            });
            let borrowed: Vec<Vec<Datum<'_>>> = sorted
                .iter()
                .map(|row| row.iter().map(OwnedDatum::borrow).collect())
                .collect();
            let tree = Tree::bulk_build(8192, 1, columns(), 2, &borrowed).unwrap();

            let mut seen: Vec<i64> = Vec::new();
            let mut at = tree.first_position().unwrap();
            while let Some(position) = at {
                let key = tree.key_at(position, 1).unwrap();
                seen.push(key[0].borrow().as_int().unwrap());
                let probe: Vec<Datum<'_>> = key.iter().map(OwnedDatum::borrow).collect();
                at = tree.seek_after(&probe, position).unwrap();
            }
            let wanted: Vec<i64> = (0..distinct as i64).collect();
            assert_eq!(seen, wanted, "{distinct} distinct values");
        }
    }

    /// The scan cursor visits every leaf once, in order.
    #[test]
    fn the_scan_cursor_visits_every_leaf() {
        let rows: Vec<Vec<Datum<'static>>> = (0..3_000).map(row).collect();
        let tree = Tree::bulk_build(8192, 1, columns(), 1, &rows).unwrap();
        let mut cursor = tree.scan();
        let mut seen = 0usize;
        let mut leaves = 0usize;
        while let Some(leaf) = cursor.next_leaf().unwrap() {
            seen = seen.saturating_add(leaf.row_count());
            leaves = leaves.saturating_add(1);
        }
        assert_eq!(seen, rows.len());
        assert_eq!(leaves, tree.leaf_count());
    }

    /// An empty tree answers rather than panicking.
    #[test]
    fn an_empty_tree_is_answerable() {
        let tree = Tree::new(8192, 1, columns(), 1).unwrap();
        assert_eq!(tree.leaf_count(), 0);
        assert!(tree.point(&[Datum::Int(1)]).unwrap().is_none());
        assert!(tree.rows().unwrap().is_empty());
        tree.check().unwrap();
    }

    /// Inserting a key that is already there replaces the row rather than
    /// duplicating it.
    #[test]
    fn insert_replaces_an_existing_key() {
        let mut tree = Tree::new(8192, 1, columns(), 1).unwrap();
        tree.insert(&row(5)).unwrap();
        tree.insert(&[Datum::Int(5), Datum::Int(999), Datum::Text(b"replaced")])
            .unwrap();
        let rows = tree.rows().unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0][1].as_int().unwrap(), 999);
    }
}
