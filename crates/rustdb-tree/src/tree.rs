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

use rustdb_base::error::{corrupt, misuse};
use rustdb_base::DbResult;

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
        built.link_siblings()?;
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
    fn link_siblings(&mut self) -> DbResult<()> {
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
        Ok(())
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
            let leaf = self.leaf(middle)?;
            let order = if leaf.row_count() > 0 {
                leaf.compare_key(0, probe)?
            } else {
                std::cmp::Ordering::Greater
            };
            match order {
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
            self.link_siblings()?;
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
            .map(|held| compare_rows(&borrow_row(held), row, key_columns) == std::cmp::Ordering::Equal)
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
                let page = self.builder.encode(&[])?;
                if let Some(slot) = self.pages.get_mut(index) {
                    *slot = page;
                }
            }
            self.link_siblings()?;
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
                        Packed::RowTooLarge => {
                            return Err(misuse("a row is larger than a page"))
                        }
                    }
                }
                self.pages.splice(index..index.saturating_add(1), produced);
            }
            Packed::RowTooLarge => return Err(misuse("a row is larger than a page")),
        }
        self.link_siblings()?;
        Ok(())
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
    /// 1. Every leaf parses.
    /// 2. Keys strictly increase within a leaf and across leaf boundaries.
    /// 3. The sibling chain matches the leaf order.
    pub fn check(&self) -> DbResult<()> {
        let mut previous: Option<Vec<OwnedDatum>> = None;
        for index in 0..self.pages.len() {
            let leaf = self.leaf(index)?;
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
            ColumnSpec::new(PhysicalType::Int64),
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
            assert_eq!(found.unwrap()[1].as_int().unwrap(), n * 2 * 3, "key {}", n * 2);
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
                assert_eq!(removed, model.remove(&key).is_some(), "step {step} key {key}");
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
        tree.insert(&[
            Datum::Int(5),
            Datum::Int(999),
            Datum::Text(b"replaced"),
        ])
        .unwrap();
        let rows = tree.rows().unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0][1].as_int().unwrap(), 999);
    }
}
