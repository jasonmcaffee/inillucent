//! The skip scan: the distinct values of an index's leading columns.
//!
//! Invariant: **the walk is driven by what it has already found.** Each step
//! seeks past the prefix it just answered rather than reading the rows between,
//! so the cost is the number of distinct prefixes and not the number of rows -
//! which is the whole reason this is not `visit_leaves` with a filter.

use inillucent_base::error::{corrupt, misuse};
use inillucent_base::DbResult;
use inillucent_pool::{PageGuard, Pool};

use crate::datum::Datum;
use crate::leaf::LeafRef;

use super::*;

impl PagedTree {
    /// Visits one row per distinct value of a key prefix, in order.
    ///
    /// The index skip scan. `SELECT DISTINCT category` over an index led by
    /// `category` does not need to read the rows - it needs to visit one row
    /// per distinct value. Over 100,000 rows with 64 distinct categories that
    /// is 64 seeks instead of 100,000 row reads, which is the algorithm SQLite
    /// uses for the same query.
    ///
    /// **One parse per leaf, and a seek that looks before it jumps.** Each turn
    /// of the loop parses the leaf it holds exactly once and does everything it
    /// needs from that one parse: the visit, the copy of the prefix, and the
    /// partition point that says where the run of equal prefixes ends. That
    /// partition point is what a descent has to compute *after* it arrives, so
    /// when the run ends in the leaf already open the descent never happens.
    /// When it does not, the walk steps right up to `WALK_BUDGET` leaves
    /// before giving up and descending - and the first time it gives up it
    /// switches itself off, because a tree's runs are all about the same length.
    ///
    /// Every route answers the same question - the first row whose key is above
    /// the prefix just visited - so the choice between them is a choice of cost,
    /// never of answer. `scan.distinct` is 65 seeks whatever the table's size,
    /// which is why the workload gets *better* with scale and why the per-seek
    /// constant is the whole of it.
    ///
    /// @param pool - the buffer pool
    /// @param prefix - how many leading key columns form the distinct value
    /// @param visit - what to do with each representative row
    pub fn skip_scan(
        &self,
        pool: &Pool,
        prefix: usize,
        visit: &mut dyn FnMut(&[Datum<'_>]) -> DbResult<bool>,
    ) -> DbResult<()> {
        if prefix == 0 || prefix > self.key_columns {
            return Err(misuse(format!(
                "a skip scan over {prefix} of {} key columns",
                self.key_columns
            )));
        }
        let mut page = self.first_leaf;
        let mut row = 0usize;
        // The leaf the position refers to, still pinned when the last step left
        // it open.
        let mut carried: Option<PageGuard<'_>> = None;
        // Reused across seeks, so a scan of 64 distinct values makes one
        // allocation rather than 64.
        let mut held: Vec<OwnedDatum> = Vec::with_capacity(prefix);
        let mut visited = 0u64;
        // Whether to look for the end of a run by walking before descending for
        // it. Switched off the first time a walk runs out of budget.
        let mut walking = true;
        // The last prefix handed to the visitor, kept only while a leaf that
        // had to be merged might have already emitted the value the next clean
        // leaf starts with. A tree nobody has written to never sets it, so the
        // resynchronising `upper_bound` below is not on the clean path at all.
        let mut resync: Option<Vec<OwnedDatum>> = None;

        loop {
            if page.is_none() {
                return Ok(());
            }
            let guard = match carried.take() {
                Some(open) => open,
                None => pool.fetch(page)?,
            };
            let step = 'step: {
                let leaf = LeafRef::parse(&guard)?
                    .with_collations(&self.collations)
                    .with_directions(&self.directions);
                let resolved = self.read_extents(pool, &leaf)?;
                let leaf = leaf.with_extents(&resolved);
                if leaf.has_writes() {
                    // A leaf that has been written to is walked rather than
                    // seeked over: its distinct values can live in the delta
                    // area, where there is no partition point to jump to. The
                    // seek machinery below is for the clean leaves, which is
                    // every leaf until something writes to one and every leaf
                    // again after the next compaction.
                    let merged = leaf.live()?;
                    let mut last: Option<Vec<Datum<'_>>> = resync
                        .as_ref()
                        .map(|held| held.iter().map(OwnedDatum::borrow).collect());
                    for values in merged.iter().skip(row) {
                        let head = values.get(..prefix).unwrap_or(&[]);
                        let repeated = last
                            .as_ref()
                            .is_some_and(|previous| self.same_prefix(previous, head, prefix));
                        if repeated {
                            continue;
                        }
                        if !visit(head)? {
                            return Ok(());
                        }
                        visited = visited.saturating_add(1);
                        last = Some(head.to_vec());
                    }
                    resync = last.map(|held| held.iter().map(OwnedDatum::from_datum).collect());
                    Step::Right(leaf.right_sibling())
                } else if row >= leaf.row_count() {
                    // An empty leaf, or a position past the end of this one. A
                    // bulk-built tree has neither, but one that has been deleted
                    // from can, and a scan that stopped here would silently
                    // return short.
                    Step::Right(leaf.right_sibling())
                } else {
                    // A merged leaf just before this one may have emitted the
                    // value this one starts with, so the position is advanced
                    // past it. `resync` is `None` on a tree nobody has written
                    // to, which is what keeps this off the measured path.
                    let mut row = row;
                    let mut past_the_end = false;
                    if let Some(previous) = &resync {
                        let borrowed: Vec<Datum<'_>> =
                            previous.iter().map(OwnedDatum::borrow).collect();
                        row = row.max(leaf.upper_bound(&borrowed)?);
                        resync = None;
                        past_the_end = row >= leaf.row_count();
                    }
                    if past_the_end {
                        break 'step Step::Right(leaf.right_sibling());
                    }
                    held.clear();
                    for column in 0..prefix {
                        held.push(OwnedDatum::from_datum(&leaf.value(row, column)?));
                    }
                    {
                        let borrowed: Vec<Datum<'_>> =
                            held.iter().map(OwnedDatum::borrow).collect();
                        if !visit(&borrowed)? {
                            return Ok(());
                        }
                    }
                    visited = visited.saturating_add(1);
                    if visited > self.row_count.saturating_add(1) {
                        return Err(corrupt("a skip scan visited more rows than the tree holds"));
                    }
                    // The borrows of the prefix live on the stack when the
                    // prefix is narrow, which every index in the fixture and in
                    // the dialect's own corpus is. Collecting them was one
                    // allocation per distinct value on a path whose whole
                    // argument is that it makes as few reads as there are
                    // distinct values.
                    let mut inline: [Datum<'_>; SKIP_PREFIX_INLINE] =
                        [Datum::Null; SKIP_PREFIX_INLINE];
                    let spilled: Vec<Datum<'_>>;
                    let borrowed: &[Datum<'_>] = if prefix <= SKIP_PREFIX_INLINE {
                        for (index, value) in held.iter().enumerate().take(prefix) {
                            if let Some(slot) = inline.get_mut(index) {
                                *slot = value.borrow();
                            }
                        }
                        inline.get(..prefix).unwrap_or(&[])
                    } else {
                        spilled = held.iter().map(OwnedDatum::borrow).collect();
                        spilled.as_slice()
                    };
                    let low = leaf.upper_bound(borrowed)?;
                    if low < leaf.row_count() {
                        Step::Here(low)
                    } else if leaf.right_sibling().is_none() {
                        // The run reaches the end of the tree, so there is no
                        // key above the prefix and the scan is over.
                        return Ok(());
                    } else {
                        Step::Seek(leaf.right_sibling())
                    }
                }
            };
            match step {
                Step::Right(right) => {
                    page = right;
                    row = 0;
                }
                Step::Here(low) => {
                    row = low;
                    carried = Some(guard);
                }
                Step::Seek(right) => {
                    drop(guard);
                    // The borrows of the prefix live on the stack when the
                    // prefix is narrow, which every index in the fixture and in
                    // the dialect's own corpus is. Collecting them was one
                    // allocation per distinct value on a path whose whole
                    // argument is that it makes as few reads as there are
                    // distinct values.
                    let mut inline: [Datum<'_>; SKIP_PREFIX_INLINE] =
                        [Datum::Null; SKIP_PREFIX_INLINE];
                    let spilled: Vec<Datum<'_>>;
                    let borrowed: &[Datum<'_>] = if prefix <= SKIP_PREFIX_INLINE {
                        for (index, value) in held.iter().enumerate().take(prefix) {
                            if let Some(slot) = inline.get_mut(index) {
                                *slot = value.borrow();
                            }
                        }
                        inline.get(..prefix).unwrap_or(&[])
                    } else {
                        spilled = held.iter().map(OwnedDatum::borrow).collect();
                        spilled.as_slice()
                    };
                    let mut found = None;
                    if walking {
                        let mut here = right;
                        let mut examined = 0usize;
                        while !here.is_none() {
                            let stepped = pool.fetch(here)?;
                            let (low, rows, next) = {
                                let leaf = LeafRef::parse(&stepped)?
                                    .with_collations(&self.collations)
                                    .with_directions(&self.directions);
                                (
                                    leaf.upper_bound(borrowed)?,
                                    leaf.row_count(),
                                    leaf.right_sibling(),
                                )
                            };
                            if low < rows {
                                found = Some((here, low, stepped));
                                break;
                            }
                            if next.is_none() {
                                return Ok(());
                            }
                            examined = examined.saturating_add(1);
                            if examined >= WALK_BUDGET {
                                break;
                            }
                            here = next;
                        }
                        if found.is_none() {
                            // The run is longer than the walk will follow, so
                            // this tree's runs span more leaves than a step is
                            // worth and every later seek descends. One failed
                            // walk per scan is what the measurement costs.
                            walking = false;
                        }
                    }
                    match found {
                        Some((landed, low, stepped)) => {
                            page = landed;
                            row = low;
                            carried = Some(stepped);
                        }
                        None => {
                            let seek = self.after_prefix(borrowed);
                            let (landed_guard, landed) =
                                self.descend_guard(pool, seek.as_slice())?;
                            let next = {
                                let leaf = LeafRef::parse(&landed_guard)?
                                    .with_collations(&self.collations)
                                    .with_directions(&self.directions);
                                let low = leaf.upper_bound(borrowed)?;
                                if low < leaf.row_count() {
                                    Some((landed, low, true))
                                } else {
                                    let right = leaf.right_sibling();
                                    if right.is_none() {
                                        None
                                    } else {
                                        Some((right, 0, false))
                                    }
                                }
                            };
                            match next {
                                Some((next_page, next_row, open)) => {
                                    page = next_page;
                                    row = next_row;
                                    carried = if open { Some(landed_guard) } else { None };
                                }
                                None => return Ok(()),
                            }
                        }
                    }
                }
            }
        }
    }
}
