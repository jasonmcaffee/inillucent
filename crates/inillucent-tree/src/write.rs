//! The write path over the tree: insert, delete, compaction, split and merge.
//!
//! Invariant: **the log record is written before the page is, and the mutation
//! that follows it cannot fail.** Every operation here has the same shape -
//! decide what the page will become, log it, then perform a change that has
//! already been proved to fit. A change that could still fail after its record
//! was written would leave a log saying something that did not happen, which is
//! worse than either half on its own: recovery would replay it.
//!
//! ## The root page id never changes
//!
//! When the root splits, its *contents* move into two new pages and the root is
//! rewritten in place as an interior. That costs one extra allocation on the one
//! occasion a tree gets taller, and it buys three things: the catalog never has
//! to be updated by a split, recovery never has to learn a new root, and a
//! concurrent reader holding the old root id is holding a page that is still the
//! root. The alternative - a new root page and a catalog write - makes every
//! split a two-object transaction.
//!
//! ## What a `NoRoom` means
//!
//! A leaf refuses an insert when its delta area is at [`crate::leaf::DELTA_LIMIT`]
//! or the row would collide with the mini-columns. The caller **compacts**:
//! every live row is read, sorted, and re-packed into a fresh page with the
//! delta area empty again. If they do not all fit, the leaf **splits**. So an
//! insert can do three things and the third is bounded: a page that has just
//! been split is at most half full, and half a page always has room for one row
//! that was small enough to be in the tree at all.
//!
//! ## Merging
//!
//! A delete that empties a leaf enough for it and its right sibling to fit in
//! one page merges them, rewriting the parent without the separator and freeing
//! the right page. Only siblings **under the same parent** are merged, because
//! merging across a parent boundary means rewriting two interior pages and the
//! separator between them, and the case is rare enough that refusing it costs a
//! half-empty leaf rather than a rebalance.

use inillucent_base::error::{corrupt, misuse};
use inillucent_base::DbResult;
use inillucent_pool::interior::{InteriorBuilder, InteriorRef};
use inillucent_pool::page;
use inillucent_pool::{Database, PageId, Pool, Swip};
use inillucent_wal::record::{Body, Structural};

use crate::datum::{Datum, OwnedDatum};
use crate::leaf::{LeafBuilder, LeafRef, Packed};
use crate::mutate::LeafMut;
use crate::paged::PagedTree;

/// How full a compaction packs a page it is not splitting.
///
/// Ninety percent rather than a hundred, so that a leaf which has just been
/// compacted has room for a few more delta rows before it has to be compacted
/// again. A hundred percent would make every insert after a compaction into
/// another compaction.
const COMPACT_FILL: f64 = 0.90;

/// How full each half of a split is packed.
///
/// A half-full page is what makes the "compact, then split, then insert" path
/// terminate: the row that would not fit is one row, and half a page has room
/// for any row small enough to have been in the tree at all.
const SPLIT_FILL: f64 = 0.50;

/// Where a mutation writes its log records.
///
/// A trait rather than a direct dependency on the transaction manager, because
/// the tree sits below it: a tree knows about pages, keys and bytes, and what a
/// transaction is is somebody else's question. `inillucent-txn`'s `Transaction`
/// implements this.
pub trait TreeLog {
    /// Appends one record and returns its LSN.
    ///
    /// @param body - what is about to happen
    fn log(&mut self, body: Body<'_>) -> DbResult<u64>;
}

/// A `TreeLog` that logs nothing and hands out increasing LSNs.
///
/// For the bulk builder, which writes a tree nobody has read yet, and for tests
/// of the tree's own shape. **Not** for anything that has to survive a crash: a
/// page stamped by this carries an LSN no log record explains.
#[derive(Debug, Default)]
pub struct NoLog {
    next: u64,
}

impl TreeLog for NoLog {
    fn log(&mut self, _body: Body<'_>) -> DbResult<u64> {
        self.next = self.next.saturating_add(8);
        Ok(self.next)
    }
}

/// Where a key was found in a leaf.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Located {
    /// In the sorted region, at this row, and live.
    Sorted(usize),
    /// In the delta area, at this index.
    Delta(usize),
    /// Not in this leaf.
    Absent,
}

/// What a write did, for the report and for the tests.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct WriteStats {
    /// Rows inserted, including the insert half of an update.
    pub inserted: u64,
    /// Rows deleted.
    pub deleted: u64,
    /// Slots overwritten in place.
    pub updated_in_place: u64,
    /// Leaves compacted.
    pub compactions: u64,
    /// Leaves split.
    pub splits: u64,
    /// Leaves merged away.
    pub merges: u64,
}

impl PagedTree {
    /// Returns what the write path has done to this tree.
    pub fn write_stats(&self) -> WriteStats {
        self.stats.get()
    }

    /// Inserts or replaces one row.
    ///
    /// Returns the row that was there before, when there was one. A key already
    /// present is *replaced*, which is what makes this the one entry point an
    /// `INSERT OR REPLACE` and an `UPDATE` both go through - a second code path
    /// for "the key is already there" is the kind of duplicate that agrees with
    /// the first one until the day it does not.
    ///
    /// @param database - the file, for allocating pages a split needs
    /// @param log - where the record goes
    /// @param row - the row, one value per column, key columns first
    pub fn insert(
        &mut self,
        database: &mut Database,
        log: &mut dyn TreeLog,
        row: &[Datum<'_>],
    ) -> DbResult<Option<Vec<OwnedDatum>>> {
        if row.len() != self.columns().len() {
            return Err(misuse(format!(
                "a row of {} values does not fit a tree of {} columns",
                row.len(),
                self.columns().len()
            )));
        }
        let key: Vec<Datum<'_>> = row.iter().copied().take(self.key_columns()).collect();
        let encoded_key = self.encode_key(&key);

        // Two attempts at most: the first may find the leaf full, and the
        // compaction or split that follows leaves a page that has room for one
        // row by construction. A third attempt would mean the second did not,
        // which is a bug rather than a case to loop on.
        for attempt in 0..2 {
            let (page, path) = self.leaf_for(database.pool(), &encoded_key)?;
            let previous = self.row_at(database.pool(), page, &key)?;
            let planned = {
                let pool = database.pool();
                pool.modify(page, |bytes| {
                    let leaf = LeafMut::new(bytes)?;
                    let ready = leaf.plan_delta(self.columns(), row)?.is_some();
                    let room = leaf.has_room_for_a_tombstone()?;
                    Ok(ready && room)
                })?
            };
            if !planned {
                if attempt == 1 {
                    return Err(corrupt(
                        "a leaf had no room for one row after being compacted and split",
                    ));
                }
                self.make_room(database, log, page, &path)?;
                continue;
            }

            let mut encoded_row = Vec::new();
            for value in row {
                value.encode_tagged(&mut encoded_row);
            }
            let lsn = log.log(Body::InsertRow {
                tree: self.tree_id(),
                page: page.0,
                row: &encoded_row,
            })?;
            let located = self.locate(database.pool(), page, &key)?;
            database.pool().modify(page, |bytes| {
                let mut leaf = LeafMut::new(bytes)?;
                match located {
                    Located::Sorted(row_index) => {
                        leaf.set_tombstone(row_index)?;
                    }
                    Located::Delta(index) => leaf.remove_delta(index)?,
                    Located::Absent => {}
                }
                let plan = leaf
                    .plan_delta(self.columns(), row)?
                    .ok_or_else(|| corrupt("a leaf that had room lost it before the write"))?;
                leaf.apply_delta(&plan)?;
                leaf.set_lsn(lsn)
            })?;
            let mut stats = self.stats.get();
            stats.inserted = stats.inserted.saturating_add(1);
            self.stats.set(stats);
            if previous.is_none() {
                self.note_rows(1);
            }
            return Ok(previous);
        }
        Err(corrupt("an insert did not converge"))
    }

    /// Deletes one row by key, returning what was there.
    ///
    /// @param database - the file, for freeing a page a merge empties
    /// @param log - where the record goes
    /// @param key - the key, one value per key column
    pub fn delete(
        &mut self,
        database: &mut Database,
        log: &mut dyn TreeLog,
        key: &[Datum<'_>],
    ) -> DbResult<Option<Vec<OwnedDatum>>> {
        let encoded_key = self.encode_key(key);
        let (page, path) = self.leaf_for(database.pool(), &encoded_key)?;
        let previous = self.row_at(database.pool(), page, key)?;
        if previous.is_none() {
            return Ok(None);
        }
        let located = self.locate(database.pool(), page, key)?;
        if let Located::Sorted(_) = located {
            let room = database.pool().modify(page, |bytes| {
                LeafMut::new(bytes)?.has_room_for_a_tombstone()
            })?;
            if !room {
                self.make_room(database, log, page, &path)?;
                return self.delete(database, log, key);
            }
        }
        // The key goes into the record as **tagged values**, not as the
        // comparison encoding the descent uses. The comparison encoding is one
        // way: it orders correctly and it cannot be read back, so a recovery
        // holding one could not find the row it names. The row records already
        // carried tagged values for the same reason, and having the two records
        // disagree about how a key is written was the difference between a
        // database that recovers and one that only recovers if a checkpoint
        // happened to have written the leaf.
        let mut tagged_key = Vec::new();
        for value in key.iter().take(self.key_columns()) {
            value.encode_tagged(&mut tagged_key);
        }
        let lsn = log.log(Body::DeleteRow {
            tree: self.tree_id(),
            page: page.0,
            key: &tagged_key,
        })?;
        let located = self.locate(database.pool(), page, key)?;
        database.pool().modify(page, |bytes| {
            let mut leaf = LeafMut::new(bytes)?;
            match located {
                Located::Sorted(row_index) => {
                    leaf.set_tombstone(row_index)?;
                }
                Located::Delta(index) => leaf.remove_delta(index)?,
                // Unreachable: `previous` was `Some`, which is exactly the
                // condition under which `locate` finds the key. Reported rather
                // than ignored, because the two disagreeing would mean a row
                // vanished between two reads of the same page.
                Located::Absent => {
                    return Err(corrupt("a row that was read could not be found to delete"))
                }
            }
            leaf.set_lsn(lsn)
        })?;
        let mut stats = self.stats.get();
        stats.deleted = stats.deleted.saturating_add(1);
        self.stats.set(stats);
        self.note_rows(-1);
        self.merge_if_small(database, log, page, &path)?;
        Ok(previous)
    }

    /// Overwrites one fixed-width column of one row, in place.
    ///
    /// The fast path an `UPDATE` of a numeric column takes, and the only write
    /// that leaves a leaf on the vectorised fast path: no delta row, no
    /// tombstone, no compaction. Returns false when the change does not fit that
    /// shape - the column is not fixed-width, the value is not of its class, or
    /// the row's current value is a NULL - and the caller falls back to
    /// [`PagedTree::insert`].
    ///
    /// @param database - the file
    /// @param log - where the record goes
    /// @param key - the row's key
    /// @param column - which column to write
    /// @param value - the new value
    pub fn update_in_place(
        &mut self,
        database: &mut Database,
        log: &mut dyn TreeLog,
        key: &[Datum<'_>],
        column: usize,
        value: &Datum<'_>,
    ) -> DbResult<bool> {
        if column < self.key_columns() {
            // Changing a key in place would move the row, which is an insert
            // and a delete rather than an update.
            return Ok(false);
        }
        let encoded_key = self.encode_key(key);
        let (page, _) = self.leaf_for(database.pool(), &encoded_key)?;
        let Located::Sorted(row_index) = self.locate(database.pool(), page, key)? else {
            return Ok(false);
        };
        // Costed on a scratch copy: the page must not change before its record
        // is in the log, and a slot update has no separate plan step because
        // there is nothing to allocate - so the trial runs on a copy.
        let mut scratch = {
            let guard = database.pool().fetch(page)?;
            guard.bytes().to_vec()
        };
        if LeafMut::new(&mut scratch)?.update_slot(column, row_index, value)?
            != crate::mutate::Applied::Yes
        {
            return Ok(false);
        }
        let mut slot = Vec::new();
        value.encode_tagged(&mut slot);
        let mut tagged_key = Vec::new();
        for value in key.iter().take(self.key_columns()) {
            value.encode_tagged(&mut tagged_key);
        }
        let lsn = log.log(Body::UpdateInPlace {
            tree: self.tree_id(),
            page: page.0,
            key: &tagged_key,
            column: column as u32,
            value: &slot,
        })?;
        database.pool().modify(page, |bytes| {
            let mut leaf = LeafMut::new(bytes)?;
            leaf.update_slot(column, row_index, value)?;
            leaf.set_lsn(lsn)
        })?;
        let mut stats = self.stats.get();
        stats.updated_in_place = stats.updated_in_place.saturating_add(1);
        self.stats.set(stats);
        Ok(true)
    }

    /// Records a commit timestamp on every leaf a transaction touched.
    ///
    /// @param pool - the buffer pool
    /// @param pages - the leaves
    /// @param cts - the commit timestamp
    pub fn stamp_commit(&self, pool: &Pool, pages: &[PageId], cts: u64) -> DbResult<()> {
        for page in pages {
            pool.modify(*page, |bytes| LeafMut::new(bytes)?.set_max_cts(cts))?;
        }
        Ok(())
    }

    /// Compacts a leaf, or splits it when its live rows no longer fit one page.
    ///
    /// @param database - the file
    /// @param log - where the record goes
    /// @param page - the leaf
    pub fn make_room(
        &mut self,
        database: &mut Database,
        log: &mut dyn TreeLog,
        page: PageId,
        path: &[PageId],
    ) -> DbResult<()> {
        let rows = self.live_rows_of(database.pool(), page)?;
        let builder = LeafBuilder::new(
            self.page_size(),
            self.tree_id(),
            self.columns().to_vec(),
            self.key_columns(),
        )?;
        let borrowed: Vec<Vec<Datum<'_>>> = rows
            .iter()
            .map(|row| row.iter().map(OwnedDatum::borrow).collect())
            .collect();
        match builder.pack(&borrowed, COMPACT_FILL)? {
            Packed::Filled {
                page: image,
                rows: packed,
            } if packed == borrowed.len() => self.compact_into(database, log, page, image),
            _ => self.split(database, log, page, path, &borrowed),
        }
    }

    /// Replaces a leaf with a freshly packed image of the same rows.
    ///
    /// @param database - the file
    /// @param log - where the record goes
    /// @param page - the leaf
    /// @param image - the packed page, without its sibling pointer
    fn compact_into(
        &mut self,
        database: &mut Database,
        log: &mut dyn TreeLog,
        page: PageId,
        mut image: Vec<u8>,
    ) -> DbResult<()> {
        let (right, max_cts) = {
            let guard = database.pool().fetch(page)?;
            let leaf = LeafRef::parse(&guard)?;
            (leaf.right_sibling(), leaf.max_cts())
        };
        page::set_right(&mut image, right)?;
        crate::page::write_u64(&mut image, crate::leaf::leaf_header::MAX_CTS, max_cts)?;
        let lsn = log.log(Body::CompactLeaf {
            tree: self.tree_id(),
            page: page.0,
            image: &image,
        })?;
        page::write_u64(&mut image, page::header::LSN, lsn)?;
        database.install(page, &image)?;
        let mut stats = self.stats.get();
        stats.compactions = stats.compactions.saturating_add(1);
        self.stats.set(stats);
        Ok(())
    }

    /// Splits a leaf in two, giving the right half a new page.
    ///
    /// @param database - the file
    /// @param log - where the record goes
    /// @param page - the leaf being split
    /// @param path - the interior pages above it, root first
    /// @param rows - its live rows, sorted
    fn split(
        &mut self,
        database: &mut Database,
        log: &mut dyn TreeLog,
        page: PageId,
        path: &[PageId],
        rows: &[Vec<Datum<'_>>],
    ) -> DbResult<()> {
        if rows.len() < 2 {
            return Err(misuse(
                "a leaf holding fewer than two rows cannot be split; the row is larger \
                 than a page and out-of-line blobs are Phase 4",
            ));
        }
        let builder = LeafBuilder::new(
            self.page_size(),
            self.tree_id(),
            self.columns().to_vec(),
            self.key_columns(),
        )?;
        let taken = match builder.pack(rows, SPLIT_FILL)? {
            Packed::Filled { rows: packed, .. } => packed.max(1).min(rows.len().saturating_sub(1)),
            Packed::RowTooLarge => {
                return Err(misuse(
                    "a row is larger than half a page; out-of-line blobs are Phase 4",
                ))
            }
        };
        let left_rows = rows.get(..taken).unwrap_or(&[]);
        let right_rows = rows.get(taken..).unwrap_or(&[]);
        let mut left_image = builder.encode(left_rows)?;
        let mut right_image = builder.encode(right_rows)?;
        let separator = {
            let head: Vec<Datum<'_>> = right_rows
                .first()
                .map(|row| row.iter().copied().take(self.key_columns()).collect())
                .unwrap_or_default();
            self.encode_key(&head)
        };
        let (old_right, max_cts) = {
            let guard = database.pool().fetch(page)?;
            let leaf = LeafRef::parse(&guard)?;
            (leaf.right_sibling(), leaf.max_cts())
        };
        crate::page::write_u64(&mut left_image, crate::leaf::leaf_header::MAX_CTS, max_cts)?;
        crate::page::write_u64(&mut right_image, crate::leaf::leaf_header::MAX_CTS, max_cts)?;

        let right_page = database.allocate(1)?;
        log.log(Body::AllocPage { page: right_page.0 })?;
        page::set_right(&mut right_image, old_right)?;

        // Splitting the root is the one case where the left half moves. The
        // root's page id has to stay what it is - the catalog names it - so the
        // root becomes an interior page and both halves get fresh pages.
        let left_page = if path.is_empty() {
            let moved = database.allocate(1)?;
            log.log(Body::AllocPage { page: moved.0 })?;
            moved
        } else {
            page
        };
        page::set_right(&mut left_image, right_page)?;

        let parent = if path.is_empty() {
            self.build_root(database, log, left_page, &separator, right_page)?
        } else {
            self.insert_separator(database, log, page, path, &separator, right_page)?
        };
        let parent_image = {
            let guard = database.pool().fetch(parent)?;
            guard.bytes().to_vec()
        };
        let lsn = log.log(Body::Structural {
            kind: Structural::Split,
            tree: self.tree_id(),
            left: left_page.0,
            right: right_page.0,
            parent: parent.0,
            left_image: &left_image,
            right_image: &right_image,
            parent_image: &parent_image,
        })?;
        page::write_u64(&mut left_image, page::header::LSN, lsn)?;
        page::write_u64(&mut right_image, page::header::LSN, lsn)?;
        database.install(left_page, &left_image)?;
        database.install(right_page, &right_image)?;
        database.pool().modify(parent, |bytes| {
            page::write_u64(bytes, page::header::LSN, lsn)
        })?;

        if page == self.first_leaf() {
            self.note_first_leaf(left_page);
        }
        self.note_leaves(1);
        let mut stats = self.stats.get();
        stats.splits = stats.splits.saturating_add(1);
        self.stats.set(stats);
        Ok(())
    }

    /// Rewrites the root page as an interior with two children.
    ///
    /// The root's own contents have already been moved into `left` by the
    /// caller, so this only has to write the new root and record that the tree
    /// is a level taller.
    ///
    /// @param database - the file
    /// @param log - where the record goes
    /// @param left - the page holding what the root used to hold
    /// @param separator - the right half's first key
    /// @param right - the right half's page
    fn build_root(
        &mut self,
        database: &mut Database,
        log: &mut dyn TreeLog,
        left: PageId,
        separator: &[u8],
        right: PageId,
    ) -> DbResult<PageId> {
        let level = self.height().saturating_add(1);
        let builder = InteriorBuilder::new(self.page_size(), self.tree_id(), level)?;
        let mut image = builder.build(
            &[separator],
            &[Swip::unswizzled(left), Swip::unswizzled(right)],
        )?;
        let root = self.root();
        let lsn = log.log(Body::WritePage {
            page: root.0,
            image: &image,
        })?;
        page::write_u64(&mut image, page::header::LSN, lsn)?;
        database.install(root, &image)?;
        self.note_height(level);
        Ok(root)
    }

    /// Adds a separator and a right child above `left`.
    ///
    /// Returns the page the separator landed in, which is what the caller
    /// stamps with the split's LSN.
    ///
    /// @param database - the file
    /// @param log - where the record goes
    /// @param left - the page that was split
    /// @param path - the interior pages above `left`, root first
    /// @param separator - the right half's first key, encoded
    /// @param right - the right half's page
    fn insert_separator(
        &mut self,
        database: &mut Database,
        log: &mut dyn TreeLog,
        left: PageId,
        path: &[PageId],
        separator: &[u8],
        right: PageId,
    ) -> DbResult<PageId> {
        let parent = path
            .last()
            .copied()
            .ok_or_else(|| corrupt("a split with no parent should have grown the root"))?;
        let ancestors = path.get(..path.len().saturating_sub(1)).unwrap_or(&[]);
        let (mut separators, mut children, level) = self.read_interior(database.pool(), parent)?;
        let position = children
            .iter()
            .position(|page| *page == left)
            .ok_or_else(|| corrupt("a child is not in the parent that routes to it"))?;
        separators.insert(position, separator.to_vec());
        children.insert(position.saturating_add(1), right);

        let builder = InteriorBuilder::new(self.page_size(), self.tree_id(), level)?;
        let keys: Vec<&[u8]> = separators.iter().map(Vec::as_slice).collect();
        if builder.fits(&keys) {
            let swips: Vec<Swip> = children
                .iter()
                .map(|page| Swip::unswizzled(*page))
                .collect();
            let mut image = builder.build(&keys, &swips)?;
            let lsn = log.log(Body::WritePage {
                page: parent.0,
                image: &image,
            })?;
            page::write_u64(&mut image, page::header::LSN, lsn)?;
            database.install(parent, &image)?;
            return Ok(parent);
        }

        // The parent is full, so it splits too: the same shape one level up. The
        // middle separator is *promoted* rather than copied, which is what keeps
        // an interior page's separators strictly between its children's ranges.
        let middle = children.len() / 2;
        let promoted = separators
            .get(middle.saturating_sub(1))
            .cloned()
            .ok_or_else(|| corrupt("an interior page with no separator to promote"))?;
        let left_keys: Vec<&[u8]> = separators
            .get(..middle.saturating_sub(1))
            .unwrap_or(&[])
            .iter()
            .map(Vec::as_slice)
            .collect();
        let right_keys: Vec<&[u8]> = separators
            .get(middle..)
            .unwrap_or(&[])
            .iter()
            .map(Vec::as_slice)
            .collect();
        let left_children: Vec<PageId> = children.get(..middle).unwrap_or(&[]).to_vec();
        let right_children: Vec<PageId> = children.get(middle..).unwrap_or(&[]).to_vec();
        let left_swips: Vec<Swip> = left_children
            .iter()
            .map(|page| Swip::unswizzled(*page))
            .collect();
        let right_swips: Vec<Swip> = right_children
            .iter()
            .map(|page| Swip::unswizzled(*page))
            .collect();
        let mut right_image = builder.build(&right_keys, &right_swips)?;
        let sibling = database.allocate(1)?;
        log.log(Body::AllocPage { page: sibling.0 })?;

        // Which half the caller's child ended up in decides which page it has to
        // stamp, and it is decided here rather than rediscovered afterwards.
        let landed = if left_children.iter().any(|page| *page == right) {
            parent
        } else {
            sibling
        };

        // The right half is written first, because promoting the separator may
        // rewrite this same parent again one level up and the promotion has to
        // see a page whose children are already the left half's.
        let mut left_image = builder.build(&left_keys, &left_swips)?;
        let left_lsn = log.log(Body::WritePage {
            page: parent.0,
            image: &left_image,
        })?;
        page::write_u64(&mut left_image, page::header::LSN, left_lsn)?;
        database.install(parent, &left_image)?;
        let right_lsn = log.log(Body::WritePage {
            page: sibling.0,
            image: &right_image,
        })?;
        page::write_u64(&mut right_image, page::header::LSN, right_lsn)?;
        database.install(sibling, &right_image)?;

        if ancestors.is_empty() {
            // The root split. Its contents are already in `parent`, and the
            // root itself becomes an interior above the two halves - so the
            // page that was the root has to be moved out of the way first.
            let moved = database.allocate(1)?;
            log.log(Body::AllocPage { page: moved.0 })?;
            let mut moved_image = left_image.clone();
            let moved_lsn = log.log(Body::WritePage {
                page: moved.0,
                image: &moved_image,
            })?;
            page::write_u64(&mut moved_image, page::header::LSN, moved_lsn)?;
            database.install(moved, &moved_image)?;
            self.build_root(database, log, moved, &promoted, sibling)?;
            return Ok(if landed == parent { moved } else { sibling });
        }
        self.insert_separator(database, log, parent, ancestors, &promoted, sibling)?;
        Ok(landed)
    }

    /// Merges a leaf with its right sibling when the two fit in one page.
    ///
    /// @param database - the file
    /// @param log - where the record goes
    /// @param page - the leaf that just lost a row
    /// @param path - the interior pages above it, root first
    fn merge_if_small(
        &mut self,
        database: &mut Database,
        log: &mut dyn TreeLog,
        page: PageId,
        path: &[PageId],
    ) -> DbResult<()> {
        let right = {
            let guard = database.pool().fetch(page)?;
            LeafRef::parse(&guard)?.right_sibling()
        };
        if right.is_none() {
            return Ok(());
        }
        let Some(parent) = path.last().copied() else {
            return Ok(());
        };
        let (mut separators, mut children, level) = self.read_interior(database.pool(), parent)?;
        let Some(position) = children.iter().position(|held| *held == page) else {
            return Ok(());
        };
        if children.get(position.saturating_add(1)).copied() != Some(right) {
            // The right sibling is under another parent. Merging across that
            // boundary rewrites two interior pages and the separator between
            // them, and a half-empty leaf is cheaper than the code that would.
            return Ok(());
        }

        let mut rows = self.live_rows_of(database.pool(), page)?;
        rows.extend(self.live_rows_of(database.pool(), right)?);
        let builder = LeafBuilder::new(
            self.page_size(),
            self.tree_id(),
            self.columns().to_vec(),
            self.key_columns(),
        )?;
        let borrowed: Vec<Vec<Datum<'_>>> = rows
            .iter()
            .map(|row| row.iter().map(OwnedDatum::borrow).collect())
            .collect();
        let mut merged = match builder.pack(&borrowed, COMPACT_FILL)? {
            Packed::Filled {
                page: image,
                rows: packed,
            } if packed == borrowed.len() => image,
            // They do not fit, which is the ordinary answer for two leaves that
            // are merely a bit empty. Nothing to do.
            _ => return Ok(()),
        };
        // A merge that emptied the parent of every separator would leave an
        // interior page with one child, which is legal but pointless, and an
        // interior page with *no* children, which is not. Refusing keeps the
        // tree's shape simple at the cost of one under-filled leaf.
        if separators.len() < 2 && !path.len().eq(&1) {
            return Ok(());
        }
        if separators.is_empty() {
            return Ok(());
        }

        let (far_right, max_cts) = {
            let guard = database.pool().fetch(right)?;
            let leaf = LeafRef::parse(&guard)?;
            (leaf.right_sibling(), leaf.max_cts())
        };
        let mine = {
            let guard = database.pool().fetch(page)?;
            LeafRef::parse(&guard)?.max_cts()
        };
        page::set_right(&mut merged, far_right)?;
        crate::page::write_u64(
            &mut merged,
            crate::leaf::leaf_header::MAX_CTS,
            max_cts.max(mine),
        )?;

        separators.remove(position);
        children.remove(position.saturating_add(1));
        let interior = InteriorBuilder::new(self.page_size(), self.tree_id(), level)?;
        let keys: Vec<&[u8]> = separators.iter().map(Vec::as_slice).collect();
        let swips: Vec<Swip> = children
            .iter()
            .map(|held| Swip::unswizzled(*held))
            .collect();
        let mut parent_image = interior.build(&keys, &swips)?;
        let empty = builder.encode(&[])?;

        let lsn = log.log(Body::Structural {
            kind: Structural::Merge,
            tree: self.tree_id(),
            left: page.0,
            right: right.0,
            parent: parent.0,
            left_image: &merged,
            right_image: &empty,
            parent_image: &parent_image,
        })?;
        page::write_u64(&mut merged, page::header::LSN, lsn)?;
        page::write_u64(&mut parent_image, page::header::LSN, lsn)?;
        database.install(page, &merged)?;
        database.install(parent, &parent_image)?;
        log.log(Body::FreePage { page: right.0 })?;
        database.release(right, 1)?;
        self.note_leaves(-1);
        let mut stats = self.stats.get();
        stats.merges = stats.merges.saturating_add(1);
        self.stats.set(stats);
        Ok(())
    }

    /// Returns the leaf a key belongs in.
    ///
    /// @param pool - the buffer pool
    /// @param encoded_key - the key's comparable bytes
    fn leaf_for(&self, pool: &Pool, encoded_key: &[u8]) -> DbResult<(PageId, Vec<PageId>)> {
        let descent = self.descend(pool, encoded_key)?;
        let path: Vec<PageId> = descent.steps.iter().map(|step| step.0).collect();
        Ok((descent.leaf, path))
    }

    /// Returns where a key sits in a leaf.
    ///
    /// @param pool - the buffer pool
    /// @param page - the leaf
    /// @param key - the key, one value per key column
    pub fn locate(&self, pool: &Pool, page: PageId, key: &[Datum<'_>]) -> DbResult<Located> {
        let guard = pool.fetch(page)?;
        let leaf = LeafRef::parse(&guard)?.with_collations(self.collations());
        leaf.locate(key, self.key_columns())
    }

    /// Returns the row a key names in a leaf, copied out.
    ///
    /// @param pool - the buffer pool
    /// @param page - the leaf
    /// @param key - the key
    fn row_at(
        &self,
        pool: &Pool,
        page: PageId,
        key: &[Datum<'_>],
    ) -> DbResult<Option<Vec<OwnedDatum>>> {
        let guard = pool.fetch(page)?;
        let leaf = LeafRef::parse(&guard)?.with_collations(self.collations());
        match self.locate_in(&leaf, key)? {
            Located::Sorted(row) => {
                let mut values = Vec::with_capacity(leaf.column_count());
                for column in 0..leaf.column_count() {
                    values.push(OwnedDatum::from_datum(&leaf.value(row, column)?));
                }
                Ok(Some(values))
            }
            Located::Delta(index) => {
                let mut values = Vec::with_capacity(leaf.column_count());
                for column in 0..leaf.column_count() {
                    values.push(OwnedDatum::from_datum(&leaf.delta_value(index, column)?));
                }
                Ok(Some(values))
            }
            Located::Absent => Ok(None),
        }
    }

    /// Returns where a key sits in a leaf already parsed.
    ///
    /// @param leaf - the leaf
    /// @param key - the key
    fn locate_in(&self, leaf: &LeafRef<'_>, key: &[Datum<'_>]) -> DbResult<Located> {
        leaf.locate(key, self.key_columns())
    }

    /// Returns one leaf's live rows, sorted, copied out.
    ///
    /// @param pool - the buffer pool
    /// @param page - the leaf
    fn live_rows_of(&self, pool: &Pool, page: PageId) -> DbResult<Vec<Vec<OwnedDatum>>> {
        let guard = pool.fetch(page)?;
        let leaf = LeafRef::parse(&guard)?.with_collations(self.collations());
        Ok(leaf
            .live()?
            .iter()
            .map(|row| row.iter().map(OwnedDatum::from_datum).collect())
            .collect())
    }

    /// Returns an interior page's separators, children and level.
    ///
    /// @param pool - the buffer pool
    /// @param page - the interior page
    fn read_interior(
        &self,
        pool: &Pool,
        page: PageId,
    ) -> DbResult<(Vec<Vec<u8>>, Vec<PageId>, u16)> {
        let guard = pool.fetch(page)?;
        let interior = InteriorRef::parse(&guard)?;
        let mut separators = Vec::with_capacity(interior.count());
        for slot in 0..interior.count() {
            separators.push(interior.key(slot)?.to_vec());
        }
        let mut children = Vec::with_capacity(interior.children());
        for child in 0..interior.children() {
            children.push(pool.page_of_swip(interior.swip(child)?)?);
        }
        Ok((separators, children, interior.level()))
    }
}
