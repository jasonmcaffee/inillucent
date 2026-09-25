//! Walking leaves, forwards and backwards, between two bounds.
//!
//! Invariant: **every walk visits each live leaf exactly once.** A leaf is
//! reached through its parent's separator or through the sibling link, never
//! both, which is what makes a scan's row count the tree's row count.

use inillucent_base::error::corrupt;
use inillucent_base::DbResult;
use inillucent_pool::{PageId, Pool};

use crate::datum::Datum;
use crate::leaf::{Hit, LeafRef};

use super::*;

impl PagedTree {
    /// Visits every leaf of the tree in key order.
    ///
    /// The visitor returns `false` to stop, which is how `LIMIT` gets out of a
    /// scan without reading the rest of the tree.
    ///
    /// @param pool - the buffer pool
    /// @param visit - what to do with each leaf
    pub fn visit_leaves(
        &self,
        pool: &Pool,
        visit: &mut dyn FnMut(&LeafRef<'_>) -> DbResult<bool>,
    ) -> DbResult<()> {
        self.visit_from(pool, self.first_leaf, visit)
    }
    /// Visits leaves from a starting page rightwards.
    ///
    /// @param pool - the buffer pool
    /// @param from - the first leaf to visit
    /// @param visit - what to do with each leaf
    pub fn visit_from(
        &self,
        pool: &Pool,
        from: PageId,
        visit: &mut dyn FnMut(&LeafRef<'_>) -> DbResult<bool>,
    ) -> DbResult<()> {
        let mut page = from;
        let mut seen = 0u64;
        while !page.is_none() {
            let guard = pool.fetch(page)?;
            let leaf = LeafRef::parse(&guard)?
                .with_collations(&self.collations)
                .with_directions(&self.directions);
            // **The out-of-line values are read here, once per leaf, for every
            // walk in the crate.** A consumer that had to know about extents
            // would be five consumers that each had to; attaching them where the
            // leaf is opened means a scan branches on whether a leaf has any and
            // never on where a value lives. A leaf with none reads nothing and
            // allocates nothing.
            let next = leaf.right_sibling();
            if !self.with_leaf_extents(pool, leaf, |leaf| visit(leaf))? {
                return Ok(());
            }
            drop(guard);
            page = next;
            seen = seen.saturating_add(1);
            // **Bounded by the file, not by the recorded leaf count.** The
            // guard is here to stop a cyclic chain from looping forever, and a
            // chain cannot be longer than the file has pages - which is true
            // whatever the catalog last wrote down. It used to be bounded by
            // `self.leaf_count`, a statistic written at the last checkpoint, so
            // a database closed without one refused to read a tree that had
            // simply grown since: fifty rows, two leaves, one recorded.
            if seen > pool.page_count().max(1) {
                return Err(corrupt("a leaf chain is longer than the file has pages"));
            }
        }
        Ok(())
    }
    /// Visits every leaf whose range can hold a key at or above `low`.
    ///
    /// @param pool - the buffer pool
    /// @param low - the encoded lower bound
    /// @param visit - what to do with each leaf
    pub fn visit_range(
        &self,
        pool: &Pool,
        low: &[u8],
        visit: &mut dyn FnMut(&LeafRef<'_>) -> DbResult<bool>,
    ) -> DbResult<()> {
        let start = {
            let (guard, page) = self.descend_guard(pool, low)?;
            drop(guard);
            page
        };
        self.visit_from(pool, start, visit)
    }
    /// Visits leaves right to left, starting from the one holding a key.
    ///
    /// A reverse walk cannot follow a sibling pointer, because the common
    /// header carries only a right sibling. It walks the descent path instead:
    /// back up until a child index can be decremented, then down the rightmost
    /// spine of the sibling before it. That is `O(height)` per leaf rather than
    /// `O(1)`, which is the right trade for a workload that is always bounded
    /// by a `LIMIT`; adding a left-sibling field would cost a write on every
    /// split for the benefit of an unbounded reverse scan nobody runs.
    ///
    /// @param pool - the buffer pool
    /// @param from - the encoded key to start at, or `None` for the last leaf
    /// @param visit - what to do with each leaf
    pub fn visit_reverse(
        &self,
        pool: &Pool,
        from: Option<&[u8]>,
        visit: &mut dyn FnMut(&LeafRef<'_>) -> DbResult<bool>,
    ) -> DbResult<()> {
        let mut descent = match from {
            Some(key) => self.descend(pool, key)?,
            None => self.descend_rightmost(pool)?,
        };
        let mut seen = 0u64;
        loop {
            {
                let guard = pool.fetch(descent.leaf)?;
                let leaf = LeafRef::parse(&guard)?
                    .with_collations(&self.collations)
                    .with_directions(&self.directions);
                if !self.with_leaf_extents(pool, leaf, |leaf| visit(leaf))? {
                    return Ok(());
                }
            }
            seen = seen.saturating_add(1);
            if seen > self.leaf_count.saturating_add(1) {
                return Err(corrupt("a reverse walk visited more leaves than exist"));
            }
            match self.step_left(pool, &mut descent)? {
                true => {}
                false => return Ok(()),
            }
        }
    }
    /// Reports whether two rows share their leading `prefix` key columns.
    ///
    /// @param left - one row
    /// @param right - the other row
    /// @param prefix - how many leading columns to compare
    pub(crate) fn same_prefix(
        &self,
        left: &[Datum<'_>],
        right: &[Datum<'_>],
        prefix: usize,
    ) -> bool {
        for index in 0..prefix {
            let (Some(a), Some(b)) = (left.get(index), right.get(index)) else {
                return false;
            };
            let collation = self
                .collations
                .get(index)
                .copied()
                .unwrap_or(Collation::Binary);
            if crate::types::compare_under(a, b, collation) != std::cmp::Ordering::Equal {
                return false;
            }
        }
        true
    }
    /// Returns the key that sorts immediately after every key sharing a prefix.
    ///
    /// Appending `0xFF` works because no encoded value can begin with it: a
    /// number's class byte is `0x01`, text's is `0x02`, a blob's `0x03` and a
    /// NULL is `0x00`. So `enc(v) ++ [0xFF]` sorts above every key that starts
    /// with `enc(v)` and below `enc(w)` for every `w > v` - the two facts a
    /// skip scan's seek needs, and the reason it does not have to know what the
    /// next distinct value *is* before it looks for it.
    ///
    /// @param prefix - the key prefix to step past
    pub fn after_prefix(&self, prefix: &[Datum<'_>]) -> KeyBytes {
        match self.encode_key_small(prefix) {
            KeyBytes::Inline(mut bytes, length) if length < KeyBytes::INLINE => {
                if let Some(slot) = bytes.get_mut(length) {
                    *slot = 0xFF;
                }
                KeyBytes::Inline(bytes, length.saturating_add(1))
            }
            other => {
                let mut heap = other.as_slice().to_vec();
                heap.push(0xFF);
                KeyBytes::Heap(heap)
            }
        }
    }
    /// Visits the rows of a range, a contiguous span of one leaf at a time.
    ///
    /// The span rather than the row is what keeps a range scan vectorised: the
    /// caller builds one batch per leaf over `start..end` instead of one batch
    /// per row. A range of 200 rows inside a leaf of 400 is one batch, one
    /// selection vector and no copying of values at all.
    ///
    /// The visitor returns `false` to stop.
    ///
    /// @param pool - the buffer pool
    /// @param low - the lower bound, or `None` for the start
    /// @param low_inclusive - whether a key equal to `low` is in the range
    /// @param high - the upper bound, or `None` for the end
    /// @param high_inclusive - whether a key equal to `high` is in the range
    /// @param visit - what to do with each leaf and its live span
    pub fn visit_span(
        &self,
        pool: &Pool,
        low: Option<&[Datum<'_>]>,
        low_inclusive: bool,
        high: Option<&[Datum<'_>]>,
        high_inclusive: bool,
        visit: &mut dyn FnMut(&LeafRef<'_>, usize, usize) -> DbResult<bool>,
    ) -> DbResult<()> {
        let start_page = match low {
            Some(values) => {
                let key = self.encode_key_small(values);
                let (guard, page) = self.descend_guard(pool, key.as_slice())?;
                drop(guard);
                page
            }
            None => self.first_leaf,
        };
        self.visit_from(pool, start_page, &mut |leaf| {
            let rows = leaf.row_count();
            // The lower bound is applied to *every* leaf, not only the first.
            //
            // A descent lands on the last child whose separator is not above
            // the probe, which is the leaf that *could* hold the key - and when
            // the key is above every key in that leaf, the run starts in the
            // next one. Applying the bound only to the first leaf and then
            // treating an empty result as "past the end" made such a range
            // return nothing at all.
            //
            // It was invisible at the default 32 KiB page size, where an index
            // leaf holds enough entries that a 200-row range almost always fits
            // in the leaf the descent lands on, and it was digest-equal to
            // SQLite on every workload. The page-size sweep is what found it:
            // at 8 KiB, `point.index` returned 3,991 rows where SQLite returned
            // 4,000. Five workloads were wrong and one measurement showed it.
            //
            // The cost of applying it everywhere is one binary search per leaf,
            // and after the first matching leaf it returns 0 immediately.
            // An *exclusive* lower bound starts past the run equal to it, so
            // it is an upper bound on the same key. `WHERE id > 495` returned
            // `id >= 495` until the physical pass's tests asked it directly.
            let begin = match low {
                Some(values) if low_inclusive => lower_bound(leaf, values)?,
                Some(values) => upper_bound(leaf, values)?,
                None => 0,
            };
            // The bound is a *bound*, not a search: a probe shorter than the
            // key matches a run of rows, and `search` lands somewhere inside
            // that run rather than at either end of it. Using it here returned
            // one row of a three-row match on the first index nested loop test
            // that exercised a prefix bound, which is how this is written as an
            // explicit partition point instead.
            let end = match high {
                Some(values) => {
                    if high_inclusive {
                        upper_bound(leaf, values)?
                    } else {
                        lower_bound(leaf, values)?
                    }
                }
                None => rows,
            };
            let end = end.min(rows);
            if begin >= end {
                // **An empty span over a written leaf says nothing.** `begin`
                // and `end` are partition points of the *sorted region*, and a
                // leaf that has been written to holds live rows that are not in
                // it - so a key inserted after the leaf was packed sorts past
                // every packed key, `lower_bound` returns `rows`, and the leaf
                // is skipped with the row still in it. `WHERE score = 99` came
                // back empty after `UPDATE ... SET score = 99` while a scan of
                // the same index listed the row.
                //
                // The visitor already knows what to do: every consumer of this
                // walk re-derives its rows from `live_between` when the leaf has
                // writes. It just has to be *called*.
                if !leaf.has_writes() {
                    // Nothing in this leaf. Two different reasons, and they have
                    // opposite answers: the lower bound skipped the whole leaf,
                    // so the run is further right; or the upper bound cut it
                    // short, so there is nothing further right.
                    return Ok(begin >= rows && end >= rows);
                }
                if !visit(leaf, begin, begin)? {
                    return Ok(false);
                }
                return Ok(end >= rows);
            }
            if !visit(leaf, begin, end)? {
                return Ok(false);
            }
            // A leaf that did not reach its own end reached the upper bound.
            Ok(end == rows)
        })
    }
    /// Visits every row whose key begins with an exact prefix.
    ///
    /// The equality case of [`PagedTree::visit_span`], and it is separate
    /// because the general version pays for generality it does not need here:
    /// two binary searches per leaf, one for each bound, where the bounds are
    /// the same key. This does one, then walks forward while the key still
    /// matches. An index nested loop probes once per outer row, so the second
    /// binary search was being paid two hundred times per `join.range`
    /// execution to find a run that is usually one entry long.
    ///
    /// The forward walk is capped: past `RUN_SCAN` matching rows it stops
    /// guessing and bisects, so a prefix that matches a whole leaf costs a
    /// binary search rather than a linear one.
    ///
    /// @param pool - the buffer pool
    /// @param key - the exact prefix, one value per compared column
    /// @param visit - what to do with each leaf and its matching span
    pub fn visit_equal(
        &self,
        pool: &Pool,
        key: &[Datum<'_>],
        visit: &mut dyn FnMut(&LeafRef<'_>, usize, usize) -> DbResult<bool>,
    ) -> DbResult<()> {
        /// How far the run is walked before the upper bound is bisected.
        const RUN_SCAN: usize = 8;

        // The landed leaf is read through the guard the descent already holds.
        // Dropping it and calling `visit_from` re-fetched and re-parsed the
        // page a probe had just finished descending to, once per outer row.
        let encoded = self.encode_key_small(key);
        let (guard, page) = self.descend_guard(pool, encoded.as_slice())?;
        let mut next = {
            let leaf = LeafRef::parse(&guard)?
                .with_collations(&self.collations)
                .with_directions(&self.directions);
            match self.with_leaf_extents(pool, leaf, |leaf| {
                Self::equal_span(leaf, key, RUN_SCAN, visit)
            })? {
                Some(right) => right,
                None => return Ok(()),
            }
        };
        drop(guard);
        let _ = page;
        let mut seen = 0u64;
        while !next.is_none() {
            let guard = pool.fetch(next)?;
            let leaf = LeafRef::parse(&guard)?
                .with_collations(&self.collations)
                .with_directions(&self.directions);
            match self.with_leaf_extents(pool, leaf, |leaf| {
                Self::equal_span(leaf, key, RUN_SCAN, visit)
            })? {
                Some(right) => next = right,
                None => return Ok(()),
            }
            seen = seen.saturating_add(1);
            if seen > self.leaf_count.saturating_add(1) {
                return Err(corrupt(
                    "an equality walk visited more leaves than the tree holds",
                ));
            }
        }
        Ok(())
    }
    /// Visits one leaf's share of an equality run.
    ///
    /// Returns the next leaf to look in, or `None` when the walk is over -
    /// either because the run ended inside this leaf or because the visitor
    /// asked to stop.
    ///
    /// @param leaf - the leaf to read
    /// @param key - the exact prefix
    /// @param scan_cap - how far a run is walked before its end is bisected
    /// @param visit - what to do with the matching span
    fn equal_span(
        leaf: &LeafRef<'_>,
        key: &[Datum<'_>],
        scan_cap: usize,
        visit: &mut dyn FnMut(&LeafRef<'_>, usize, usize) -> DbResult<bool>,
    ) -> DbResult<Option<PageId>> {
        let rows = leaf.row_count();
        let (begin, end) = leaf.equal_run(key, scan_cap)?;
        if begin >= end {
            // A written leaf is visited even with an empty *sorted* run, for
            // the reason `visit_span` gives: the run is computed over the packed
            // region and the live rows are not that set. The consumer merges.
            if leaf.has_writes() && !visit(leaf, begin, begin)? {
                return Ok(None);
            }
            // Either the key sorts after everything here - so the run, if there
            // is one, starts in the next leaf - or it is simply not present and
            // everything after it is greater.
            //
            // **"Everything here" includes the delta area** (task-2066 §4.3.4).
            // `begin >= rows` asks the sorted region alone, and a leaf that
            // `CREATE INDEX` built before the load has an empty sorted region
            // and every row in the delta - so `0 >= 0` was true for every
            // probe and the walk stepped to the right sibling, and the next,
            // through every leaf of the index. A join over such an index took
            // 2,634 ms against 2.91 ms for the same index built after the
            // load.
            //
            // The leaves are ordered, so the run continues rightwards only
            // when nothing in this leaf already sorts past the probe.
            return Ok(if begin >= rows && !leaf.holds_a_key_past(key)? {
                Some(leaf.right_sibling())
            } else {
                None
            });
        }
        if !visit(leaf, begin, end)? {
            return Ok(None);
        }
        // Only a run that reached the end of its leaf can continue.
        Ok(if end == rows {
            Some(leaf.right_sibling())
        } else {
            None
        })
    }
    /// Visits the rows of a range right to left, a span of one leaf at a time.
    ///
    /// The caller walks `start..end` backwards inside each span. Used by
    /// `ORDER BY ... DESC LIMIT n`, which is why it takes an upper bound and no
    /// lower one: the walk is always stopped by the limit rather than by the
    /// range.
    ///
    /// @param pool - the buffer pool
    /// @param high - the inclusive upper bound, or `None` for the last row
    /// @param visit - what to do with each leaf and its live span
    pub fn visit_span_reverse(
        &self,
        pool: &Pool,
        low: Option<&[Datum<'_>]>,
        low_inclusive: bool,
        high: Option<&[Datum<'_>]>,
        high_inclusive: bool,
        visit: &mut dyn FnMut(&LeafRef<'_>, usize, usize) -> DbResult<bool>,
    ) -> DbResult<()> {
        let from = high.map(|values| self.encode_key(values));
        let mut first = true;
        self.visit_reverse(pool, from.as_deref(), &mut |leaf| {
            let rows = leaf.row_count();
            // **Both bounds, on every leaf, exactly as the forward walk applies
            // them.** This used to take an upper bound alone, apply it to the
            // first leaf only, and walk to the start of the tree - so
            // `WHERE id >= 3 ORDER BY id DESC` returned rows 2 and 1 as well,
            // `WHERE id < 5 ORDER BY id DESC` returned 5, and
            // `WHERE grp = 2 ORDER BY k DESC` returned the other groups too.
            // Every one of those is a *wrong answer* rather than a refusal, and
            // it was reachable only through the descending-order path, which is
            // why a forward walk of the same range was right all along.
            //
            // The upper bound is a partition point of the sorted region like
            // the lower one is: inclusive means past the run equal to it,
            // exclusive means before it - which is `upper_bound` and
            // `lower_bound` respectively, the same pair the forward walk uses
            // and in the same roles.
            let end = if first {
                match high {
                    Some(values) if high_inclusive => upper_bound(leaf, values)?,
                    Some(values) => lower_bound(leaf, values)?,
                    None => rows,
                }
            } else {
                rows
            };
            first = false;
            let end = end.min(rows);
            // An exclusive lower bound starts past the run equal to it, so it
            // is an upper bound on the same key.
            let begin = match low {
                Some(values) if low_inclusive => lower_bound(leaf, values)?,
                Some(values) => upper_bound(leaf, values)?,
                None => 0,
            };
            if begin >= end {
                // A leaf that has been written to holds live rows that are not
                // in the sorted region, so an empty span over one says nothing
                // and the visitor re-derives its rows from `live_between`. The
                // walk stops when the lower bound cut the leaf short, because
                // everything further left is below it.
                if !leaf.has_writes() {
                    return Ok(begin == 0 && end == 0);
                }
                if !visit(leaf, begin, begin)? {
                    return Ok(false);
                }
                return Ok(begin == 0);
            }
            if !visit(leaf, begin, end)? {
                return Ok(false);
            }
            // A leaf that did not reach its own start reached the lower bound.
            Ok(begin == 0)
        })
    }
    /// Runs a function over the row a key names, if the tree holds it.
    ///
    /// The row is handed to the caller *inside* the leaf's guard, so a point
    /// probe projects straight out of the page and copies only what the query
    /// asked for. Returning the row instead would mean copying every column of
    /// it first, which on `point.rowid` is the difference between the target
    /// and twice the target.
    ///
    /// @param pool - the buffer pool
    /// @param probe - the key, one value per key column
    /// @param read - what to do with the leaf and the row index
    pub fn probe<R>(
        &self,
        pool: &Pool,
        probe: &[Datum<'_>],
        read: impl FnOnce(&LeafRef<'_>, Hit) -> DbResult<R>,
    ) -> DbResult<Option<R>> {
        // **A rowid is looked up by an integer or not at all** - SQLite's
        // `OP_SeekRowid`, which applies numeric affinity to its operand and
        // jumps to "not found" the moment the result is not an integer. It
        // matters at the two ends: `9223372036854775807 + 1` overflows to the
        // double `9223372036854775808.0` and `-9223372036854775807 - 2` to
        // `-9223372036854775808.0`, and integer affinity leaves both as reals
        // because neither converts back without losing the distinction from
        // the neighbouring integer. Without this, the descent clamped such a
        // real to the nearest rowid and the leaf search - which compares
        // exactly - agreed with it at `i64::MIN`, so the query returned the
        // row at the floor of the table where SQLite returns nothing
        // (task-1932, H7). Text, a blob and NULL land here for the same
        // reason: none of them is ever equal to a rowid.
        if self.encoding == KeyEncoding::Rowid && !matches!(probe, [Datum::Int(_)]) {
            return Ok(None);
        }
        let key = self.encode_key_small(probe);
        let (guard, _) = self.descend_guard(pool, key.as_slice())?;
        let leaf = LeafRef::parse(&guard)?
            .with_collations(&self.collations)
            .with_directions(&self.directions);
        // **One row's out-of-line values, not the leaf's.** A leaf whose values
        // are out of line holds a great many rows - it is sixteen bytes per
        // value rather than four kilobytes - so resolving the whole leaf to
        // answer one probe read three hundred extents to return one. It measured
        // 31 us per point read against SQLite's 13.
        //
        // The row is found first, which is safe because finding it reads only
        // key columns and a key is never out of line.
        if !leaf.has_extents() {
            return self.probe_leaf(&leaf, probe, read);
        }
        let Some(hit) = self.hit_in(&leaf, probe)? else {
            return Ok(None);
        };
        let held = match hit {
            Hit::Sorted(row) => self.read_extents_row(pool, &leaf, row)?,
            // A delta row can hold one too, as a tagged reference: the write
            // path spills the value and puts the reference in the delta rather
            // than repacking the leaf around it.
            Hit::Delta(index) => self.read_extents_delta(pool, &leaf, index)?,
        };
        let leaf = leaf.with_extents(&held);
        Ok(Some(read(&leaf, hit)?))
    }
    /// Finds a key inside a leaf the caller has already descended to.
    ///
    /// The half of [`PagedTree::probe`] below the descent, factored out so the
    /// pipelined form cannot drift from the single form - including the delta
    /// check, which is the part that would be quietly dropped.
    ///
    /// @param leaf - the leaf the descent landed on
    /// @param probe - the key, one value per key column
    /// @param read - what to do with the leaf and the row index
    fn probe_leaf<R>(
        &self,
        leaf: &LeafRef<'_>,
        probe: &[Datum<'_>],
        read: impl FnOnce(&LeafRef<'_>, Hit) -> DbResult<R>,
    ) -> DbResult<Option<R>> {
        if let Ok(row) = leaf.search(probe)? {
            if !leaf.is_tombstoned(row)? {
                return Ok(Some(read(leaf, Hit::Sorted(row))?));
            }
        }
        // The delta area, which Phase 2 refused and Phase 3 reads. The loop is
        // guarded by `delta_count`, which is zero on every leaf that has not
        // been written to - so a clean tree pays one comparison against zero
        // per probe and nothing else, which is what keeps `point.rowid` at the
        // 300 ns the read gate measured.
        // **As many columns as the probe supplied, not as many as the key has.**
        // A probe may be a *prefix*: an equality on the leading column of a
        // two-column index is one value against a key of `(score, rowid)`, and
        // the sorted search above compares exactly the columns it was given.
        // Comparing `key_columns` here instead filled the missing positions with
        // NULL and compared the delta row's rowid against it - so a row that had
        // been written since the leaf was packed was never found, and
        // `WHERE score = 99` came back empty after `UPDATE ... SET score = 99`
        // while a scan of the same index showed the row.
        //
        // **A binary search of the delta directory, not a scan** (task-2074).
        // The directory is in key order and the area is as large as the free
        // gap, so a probe that missed the sorted region used to compare every
        // row of it. The prefix rule above is the search's own: it compares the
        // columns the probe names and no others.
        if leaf.delta_count() == 0 {
            return Ok(None);
        }
        let compared = probe.len().min(self.key_columns);
        match leaf.delta_search(probe.get(..compared).unwrap_or(probe))? {
            Ok(entry) => Ok(Some(read(leaf, Hit::Delta(entry))?)),
            Err(_) => Ok(None),
        }
    }
    /// Returns one row by key, copying it out.
    ///
    /// The allocating path, for tests and for the model comparison. The
    /// executor uses [`PagedTree::probe`].
    ///
    /// @param pool - the buffer pool
    /// @param probe - the key, one value per key column
    /// Reports whether a key is in the tree, without copying its row.
    ///
    /// The uniqueness check a write does before it writes, which asks only
    /// whether something is there. `point` answers the same question and copies
    /// every column of the row to do it, allocating per text and per blob - on
    /// `main_table` that is five columns, a text and a blob, per insert, thrown
    /// away.
    ///
    /// @param pool - the buffer pool
    /// @param probe - the key, one value per key column
    pub fn contains(&self, pool: &Pool, probe: &[Datum<'_>]) -> DbResult<bool> {
        Ok(self.probe(pool, probe, |_, _| Ok(()))?.is_some())
    }
    /// Returns one row by key, copying it out.
    ///
    /// The allocating path, for tests and for the model comparison. The
    /// executor uses [`PagedTree::probe`], and a caller that only wants to know
    /// whether the key is there uses [`PagedTree::contains`].
    ///
    /// @param pool - the buffer pool
    /// @param probe - the key, one value per key column
    pub fn point(&self, pool: &Pool, probe: &[Datum<'_>]) -> DbResult<Option<Vec<OwnedDatum>>> {
        self.probe(pool, probe, |leaf, hit| {
            let mut values = Vec::with_capacity(leaf.column_count());
            for column in 0..leaf.column_count() {
                values.push(OwnedDatum::from_datum(&leaf.value_at(hit, column)?));
            }
            Ok(values)
        })
    }
    /// Returns every row in key order, copying them out.
    ///
    /// The slow path the property test and the integrity checker use.
    ///
    /// @param pool - the buffer pool
    pub fn rows(&self, pool: &Pool) -> DbResult<Vec<Vec<OwnedDatum>>> {
        let mut out = Vec::new();
        self.visit_leaves(pool, &mut |leaf| {
            for row in leaf.live()? {
                out.push(row.iter().map(OwnedDatum::from_datum).collect());
            }
            Ok(true)
        })?;
        Ok(out)
    }
}
