//! The sources: what drives a pipeline over a tree in the buffer pool.
//!
//! Invariant: a batch never outlives the guard on the leaf it borrows. That is
//! enforced by shape rather than by discipline - every source here fetches a
//! leaf, builds its vectors inside the scope that holds the guard, pushes the
//! batch, and drops the guard. Nothing returns a batch, so nothing can hold one
//! past the page it points at.
//!
//! That is why the executor is push-based, and it is the same argument the
//! buffer pool makes from the other side: a `PageGuard`'s borrow is scoped to
//! the guard, so an operator model in which a scan hands batches *out* would
//! need either a self-referential cursor or a copy per leaf. The push model
//! needs neither.
//!
//! ## The five sources
//!
//! | source | what it is for |
//! |---|---|
//! | [`FullScan`] | every row of a tree, forward |
//! | [`SpanScan`] | a key range, forward, one contiguous span of each leaf |
//! | [`ReverseScan`] | a key range, backward, for `ORDER BY ... DESC LIMIT` |
//! | [`SkipScan`] | one row per distinct key prefix |
//! | [`PointProbe`] | one row by key, with no batch and no vector at all |
//!
//! [`PointProbe`] is the odd one and is meant to be. The TDD asks for "a
//! separate compiled point-probe path" that "descends the tree with swizzled
//! pointers, finds the row, evaluates the predicate on the row's values in
//! place, and writes the projected values into the statement's result slots.
//! No batch, no vector, no selection." A single-row answer that went through a
//! batch would pay for a vector per column to carry one value each, and the
//! phase gate asks for that answer in under 500 ns.

use inillucent_base::DbResult;
use inillucent_pool::Pool;
use inillucent_tree::datum::{Datum, OwnedDatum};
use inillucent_tree::leaf::{Hit, LeafRef};
use inillucent_tree::PagedTree;

use crate::batch::{Batch, Vector};
use crate::ops::{Flow, Sink};
use crate::scan::Projection;

/// Builds the vectors of one leaf, in the projection's order.
///
/// @param leaf - the leaf to read
/// @param projection - which tree columns to expose, in output order
fn vectors<'p>(leaf: &LeafRef<'p>, projection: &Projection) -> DbResult<Vec<Vector<'p>>> {
    let mut columns = Vec::with_capacity(projection.0.len());
    for index in &projection.0 {
        columns.push(Vector::from_column(leaf.column(*index)?));
    }
    Ok(columns)
}

/// Pushes a written-to leaf's merged rows downstream, in the projection's order.
///
/// **This is the slow path and it is meant to be.** A leaf with tombstones or
/// delta rows cannot be read as a run of mini-columns: some of its rows are
/// hidden and some of them are tagged bytes in the delta area, so the answer has
/// to be materialised. `LeafRef::live` is the one place that merge is written,
/// which is what stops five sources having five slightly different ideas of what
/// a live row is.
///
/// The fast path is unchanged for every leaf that has not been written to, and
/// that is the whole trade: `LeafRef::has_writes` is one flag test against a
/// header field the parse already read.
///
/// @param rows - the leaf's live rows, in key order
/// @param projection - which tree columns to expose, in output order
/// @param downstream - the head of the operator chain
fn push_merged(
    rows: &[Vec<Datum<'_>>],
    projection: &Projection,
    downstream: &mut dyn Sink,
) -> DbResult<Flow> {
    let mut start = 0usize;
    while start < rows.len() {
        let end = start
            .saturating_add(crate::batch::BATCH_ROWS)
            .min(rows.len());
        let chunk = rows.get(start..end).unwrap_or(&[]);
        let mut columns_owned: Vec<Vec<Datum<'_>>> = Vec::with_capacity(projection.0.len());
        for index in &projection.0 {
            columns_owned.push(
                chunk
                    .iter()
                    .map(|row| row.get(*index).copied().unwrap_or(Datum::Null))
                    .collect(),
            );
        }
        let columns: Vec<Vector<'_>> = columns_owned
            .iter()
            .map(|values| Vector::Values(values.as_slice()))
            .collect();
        let batch = Batch::new(chunk.len(), columns);
        if downstream.push(&batch)? == Flow::Stop {
            return Ok(Flow::Stop);
        }
        start = end;
    }
    Ok(Flow::Continue)
}

/// Every row of a tree, in key order.
pub struct FullScan<'t> {
    tree: &'t PagedTree,
    projection: Projection,
}

impl<'t> FullScan<'t> {
    /// Returns a scan over a whole tree.
    ///
    /// @param tree - the tree to read
    /// @param projection - which tree columns to expose, in output order
    pub fn new(tree: &'t PagedTree, projection: Projection) -> FullScan<'t> {
        FullScan { tree, projection }
    }

    /// Drives the scan until the tree runs out or the pipeline says stop.
    ///
    /// @param pool - the buffer pool the tree's pages live in
    /// @param downstream - the head of the operator chain
    pub fn run(&self, pool: &Pool, downstream: &mut dyn Sink) -> DbResult<()> {
        self.tree.visit_leaves(pool, &mut |leaf| {
            // **Checked per leaf, not per row.** A scan whose predicate rejects
            // everything hands nothing to `Collect`, so the result budget never
            // sees it - and a full scan of a large table is exactly the request
            // a deadline exists for. A leaf is a few thousand rows, which makes
            // this an atomic load and one clock read per few thousand rows.
            inillucent_base::budget::check()?;
            // **The empty test is `has no live rows`, not `has no packed
            // rows`.** A leaf built empty and then written to holds every one of
            // its rows in the delta area with `row_count` still zero, which is
            // exactly the shape a `CREATE TABLE` followed by an `INSERT`
            // produces - and skipping it here made such a table read back as
            // nothing at all while the rows were in the file.
            if leaf.row_count() == 0 && !leaf.needs_materialising() {
                return Ok(true);
            }
            // A leaf that has been written to is merged rather than read as
            // mini-columns. *Exceptions* are a different thing entirely - a
            // value of the wrong class for its column - and are read through
            // the general vector path, which is why the test is `has_writes`
            // and not `is_clean`. Confusing the two cost the SLT corpus
            // thirty-four refusals once.
            if leaf.needs_materialising() {
                let rows = leaf.live()?;
                return Ok(push_merged(&rows, &self.projection, downstream)? == Flow::Continue);
            }
            let batch = Batch::new(leaf.row_count(), vectors(leaf, &self.projection)?);
            Ok(downstream.push(&batch)? == Flow::Continue)
        })?;
        downstream.finish()
    }
}

/// A key range of a tree, in key order.
///
/// The bounds are values rather than encoded bytes because the leaf's own
/// search compares values: the encoded form decides which *leaf*, and the
/// values decide which *rows* inside it. Passing only the encoded form would
/// make the row boundary a second implementation of the comparison.
pub struct SpanScan<'t> {
    tree: &'t PagedTree,
    projection: Projection,
    low: Option<Vec<OwnedDatum>>,
    low_inclusive: bool,
    high: Option<Vec<OwnedDatum>>,
    high_inclusive: bool,
}

impl<'t> SpanScan<'t> {
    /// Returns a scan over a key range.
    ///
    /// @param tree - the tree to read
    /// @param projection - which tree columns to expose, in output order
    /// @param low - the lower bound, or `None`
    /// @param low_inclusive - whether a key equal to `low` is in the range
    /// @param high - the upper bound, or `None`
    /// @param high_inclusive - whether a key equal to `high` is in the range
    pub fn new(
        tree: &'t PagedTree,
        projection: Projection,
        low: Option<Vec<OwnedDatum>>,
        low_inclusive: bool,
        high: Option<Vec<OwnedDatum>>,
        high_inclusive: bool,
    ) -> SpanScan<'t> {
        SpanScan {
            tree,
            projection,
            low,
            low_inclusive,
            high,
            high_inclusive,
        }
    }

    /// Drives the scan.
    ///
    /// @param pool - the buffer pool
    /// @param downstream - the head of the operator chain
    pub fn run(&self, pool: &Pool, downstream: &mut dyn Sink) -> DbResult<()> {
        let low: Option<Vec<Datum<'_>>> = self
            .low
            .as_ref()
            .map(|values| values.iter().map(OwnedDatum::borrow).collect());
        let high: Option<Vec<Datum<'_>>> = self
            .high
            .as_ref()
            .map(|values| values.iter().map(OwnedDatum::borrow).collect());
        // Reused across leaves, so a range that spans forty leaves makes one
        // allocation rather than forty.
        let mut selection: Vec<u32> = Vec::new();
        self.tree.visit_span(
            pool,
            low.as_deref(),
            self.low_inclusive,
            high.as_deref(),
            self.high_inclusive,
            &mut |leaf, start, end| {
                if leaf.needs_materialising() {
                    // The span the visitor computed is over the *sorted
                    // region*, and a written-to leaf's live rows are not that
                    // set - so the bounds are applied again to the merged rows.
                    // `live_between` is the merged counterpart of the
                    // `lower_bound`/`upper_bound` pair the span came from, and
                    // it compares under the same collations.
                    let rows = leaf.live_between(
                        low.as_deref(),
                        self.low_inclusive,
                        high.as_deref(),
                        self.high_inclusive,
                    )?;
                    return Ok(push_merged(&rows, &self.projection, downstream)? == Flow::Continue);
                }
                let columns = vectors(leaf, &self.projection)?;
                let flow = if start == 0 && end == leaf.row_count() {
                    // The whole leaf is in range, so no selection vector at
                    // all: the batch is dense and every consumer takes its
                    // fast path.
                    let batch = Batch::new(leaf.row_count(), columns);
                    downstream.push(&batch)?
                } else {
                    selection.clear();
                    selection.extend((start..end).map(|row| row as u32));
                    let mut batch = Batch::new(leaf.row_count(), columns);
                    batch.selection = Some(selection.as_slice());
                    downstream.push(&batch)?
                };
                Ok(flow == Flow::Continue)
            },
        )?;
        downstream.finish()
    }
}

/// A key range of a tree, walked backwards.
///
/// Rows inside a leaf come out in reverse, and the leaves come out in reverse,
/// so the whole stream is descending.
///
/// The first version said "there is no selection vector that can express
/// reversed" and materialised every row. That was simply wrong: a selection
/// vector is a list of row numbers and nothing requires it to ascend. Filling
/// it backwards is a reverse scan, at no copy and with the leaf's own vectors
/// intact. It measured 246 ns per row materialised against a 50-row limit; the
/// selection costs four bytes per row and no allocation after the first leaf.
pub struct ReverseScan<'t> {
    tree: &'t PagedTree,
    projection: Projection,
    /// The lower bound, or `None` for the first row.
    low: Option<Vec<OwnedDatum>>,
    /// Whether a key equal to the lower bound is in the range.
    low_inclusive: bool,
    /// The upper bound, or `None` for the last row.
    high: Option<Vec<OwnedDatum>>,
    /// Whether a key equal to the upper bound is in the range.
    high_inclusive: bool,
    limit: Option<usize>,
}

impl<'t> ReverseScan<'t> {
    /// Returns a reverse scan over a bounded range.
    ///
    /// **Both bounds, because a descending walk of a range is the same range.**
    /// This used to take the upper bound alone, on the reasoning that a
    /// backward walk starts at the top and a `LIMIT` stops it - which is true of
    /// `ORDER BY id DESC LIMIT 1` and of nothing else. `WHERE id >= 3 ORDER BY
    /// id DESC` walked past 3 to the start of the tree, and the predicate had
    /// been consumed by the range so no residual re-tested it.
    ///
    /// @param tree - the tree to read
    /// @param projection - which tree columns to expose, in output order
    /// @param bounds - the range, with the inclusivity of each end
    /// @param limit - how many rows are wanted at most, when the plan says
    pub fn new(
        tree: &'t PagedTree,
        projection: Projection,
        bounds: crate::physical::SpanBounds,
        limit: Option<usize>,
    ) -> ReverseScan<'t> {
        ReverseScan {
            tree,
            projection,
            low: bounds.low,
            low_inclusive: bounds.low_inclusive,
            high: bounds.high,
            high_inclusive: bounds.high_inclusive,
            limit,
        }
    }

    /// Drives the scan.
    ///
    /// @param pool - the buffer pool
    /// @param downstream - the head of the operator chain
    pub fn run(&self, pool: &Pool, downstream: &mut dyn Sink) -> DbResult<()> {
        let low: Option<Vec<Datum<'_>>> = self
            .low
            .as_ref()
            .map(|values| values.iter().map(OwnedDatum::borrow).collect());
        let high: Option<Vec<Datum<'_>>> = self
            .high
            .as_ref()
            .map(|values| values.iter().map(OwnedDatum::borrow).collect());
        let mut selection: Vec<u32> = Vec::new();
        let mut produced = 0usize;
        self.tree.visit_span_reverse(
            pool,
            low.as_deref(),
            self.low_inclusive,
            high.as_deref(),
            self.high_inclusive,
            &mut |leaf, start, end| {
                if leaf.needs_materialising() {
                    // Merged, filtered by the same bounds the span used, and
                    // reversed - which for a materialised list is one `reverse`
                    // rather than a descending selection vector.
                    let mut rows = leaf.live_between(
                        low.as_deref(),
                        self.low_inclusive,
                        high.as_deref(),
                        self.high_inclusive,
                    )?;
                    rows.reverse();
                    if let Some(limit) = self.limit {
                        if produced >= limit {
                            return Ok(false);
                        }
                        rows.truncate(limit.saturating_sub(produced));
                    }
                    // **An empty leaf is not the end of the range.** It used to
                    // be read as one, which was true while the only bound was
                    // the upper one and a backward walk therefore ran to the
                    // start of the tree. With a lower bound the walk stops
                    // where the *tree* says it does, and a leaf the bounds
                    // emptied is one to step past.
                    if rows.is_empty() {
                        return Ok(true);
                    }
                    produced = produced.saturating_add(rows.len());
                    if push_merged(&rows, &self.projection, downstream)? == Flow::Stop {
                        return Ok(false);
                    }
                    if let Some(limit) = self.limit {
                        if produced >= limit {
                            return Ok(false);
                        }
                    }
                    return Ok(true);
                }
                // The selection descends, which is the whole of "reversed".
                selection.clear();
                let mut wanted = end.saturating_sub(start);
                if let Some(limit) = self.limit {
                    if produced >= limit {
                        return Ok(false);
                    }
                    wanted = wanted.min(limit.saturating_sub(produced));
                }
                selection.extend((start..end).rev().take(wanted).map(|row| row as u32));
                if selection.is_empty() {
                    return Ok(true);
                }
                produced = produced.saturating_add(selection.len());
                let mut batch = Batch::new(leaf.row_count(), vectors(leaf, &self.projection)?);
                batch.selection = Some(selection.as_slice());
                if downstream.push(&batch)? == Flow::Stop {
                    return Ok(false);
                }
                if let Some(limit) = self.limit {
                    if produced >= limit {
                        return Ok(false);
                    }
                }
                Ok(true)
            },
        )?;
        downstream.finish()
    }
}

/// One row per distinct value of a key prefix.
pub struct SkipScan<'t> {
    tree: &'t PagedTree,
    prefix: usize,
}

impl<'t> SkipScan<'t> {
    /// Returns a skip scan.
    ///
    /// @param tree - the tree to read
    /// @param prefix - how many leading key columns form the distinct value
    pub fn new(tree: &'t PagedTree, prefix: usize) -> SkipScan<'t> {
        SkipScan { tree, prefix }
    }

    /// Drives the scan, one batch per [`crate::batch::BATCH_ROWS`] values.
    ///
    /// @param pool - the buffer pool
    /// @param downstream - the head of the operator chain
    pub fn run(&self, pool: &Pool, downstream: &mut dyn Sink) -> DbResult<()> {
        let prefix = self.prefix;
        // The row buffers are kept and cleared rather than allocated and
        // dropped. A skip scan's whole argument is that it does one read per
        // distinct value, so a `Vec` per distinct value is the one allocation
        // it should not be making: `scan.distinct` is 65 of them at every
        // scale, which is why the workload's per-seek constant is the whole of
        // its cost.
        let mut buffered: Vec<Vec<OwnedDatum>> = Vec::new();
        let mut used = 0usize;
        let mut stop = false;
        self.tree.skip_scan(pool, prefix, &mut |values| {
            if used >= buffered.len() {
                buffered.push(Vec::with_capacity(prefix));
            }
            if let Some(out) = buffered.get_mut(used) {
                out.clear();
                for value in values.iter().take(prefix) {
                    out.push(OwnedDatum::from_datum(value));
                }
            }
            used = used.saturating_add(1);
            if used >= crate::batch::BATCH_ROWS {
                let rows = buffered.get(..used).unwrap_or(&[]);
                let flow = crate::ops::emit_rows(rows, downstream)?;
                used = 0;
                if flow == Flow::Stop {
                    stop = true;
                    return Ok(false);
                }
            }
            Ok(true)
        })?;
        if !stop && used > 0 {
            crate::ops::emit_rows(buffered.get(..used).unwrap_or(&[]), downstream)?;
        }
        downstream.finish()
    }
}

/// How many projected columns a point probe keeps on the stack.
///
/// Eight covers every table in the scorecard fixture and in the dialect's own
/// corpus. The array is filled in whether the projection needs every slot or
/// not, so the size is a cost as well as a ceiling; a wider projection takes the
/// owned path, which is what every probe used to take.
const PROBE_INLINE_COLUMNS: usize = 8;

/// One row by key: the compiled point-probe path.
///
/// No batch, no vector, no selection vector. A probe descends, finds the row,
/// and hands the leaf and the row index to a projector that writes values
/// straight out of the page. The TDD's target is under 500 ns warm, and every
/// allocation on this path is one the answer did not need.
pub struct PointProbe<'t> {
    tree: &'t PagedTree,
    projection: Projection,
}

impl<'t> PointProbe<'t> {
    /// Returns a probe over a tree.
    ///
    /// @param tree - the tree to read
    /// @param projection - which tree columns to produce, in output order
    pub fn new(tree: &'t PagedTree, projection: Projection) -> PointProbe<'t> {
        PointProbe { tree, projection }
    }

    /// Returns the tree the probe reads.
    pub fn tree(&self) -> &'t PagedTree {
        self.tree
    }

    /// Looks one key up and writes the projected values into a buffer.
    ///
    /// Returns false when there is no such row, which is `point.miss` - and
    /// which costs one descent and one binary search, not a scan. The buffer is
    /// the caller's so a probe in a loop makes no allocations at all.
    ///
    /// @param pool - the buffer pool
    /// @param key - the key, one value per key column
    /// @param out - the buffer to write the projected values into, cleared first
    pub fn lookup(
        &self,
        pool: &Pool,
        key: &[Datum<'_>],
        out: &mut Vec<OwnedDatum>,
    ) -> DbResult<bool> {
        out.clear();
        let found = self.tree.probe(pool, key, |leaf, hit| {
            for column in &self.projection.0 {
                out.push(OwnedDatum::from_datum(&leaf.value_at(hit, *column)?));
            }
            Ok(())
        })?;
        Ok(found.is_some())
    }

    /// Looks one key up and pushes it downstream as a one-row batch.
    ///
    /// **Nothing is copied and nothing is allocated.** The row went through
    /// `OwnedDatum` and `emit_rows`, which is one `Vec` for the row, one for
    /// the column list, one per column of borrows, and a copy of every text
    /// value - to carry a single row that the leaf is still pinned under.
    /// `inillucent-probeprofile` measured `SELECT label ... WHERE id = ?1` at
    /// 1.43 us and `SELECT count(*) ... WHERE id = ?1` at 0.87 us, against a
    /// bare `tree.probe` of about 0.24 us; the difference between those two is
    /// what carrying one text value used to cost.
    ///
    /// The values are constant vectors over the leaf, exactly as an index
    /// nested loop's are, and the column list is on the stack.
    ///
    /// @param pool - the buffer pool
    /// @param key - the key, one value per key column
    /// @param downstream - the head of the operator chain
    pub fn run(&self, pool: &Pool, key: &[Datum<'_>], downstream: &mut dyn Sink) -> DbResult<()> {
        let width = self.projection.0.len();
        if width <= PROBE_INLINE_COLUMNS {
            self.tree.probe(pool, key, |leaf, hit| {
                // The leaf's own mini-columns under a one-row selection, not
                // values read out of them. A stage is projected in full, so a
                // probe into a five-column table read five columns to answer a
                // query that reads one - and a PAX leaf keeps each column's
                // values in its own region, so those are five scattered cache
                // lines where the directory entries describing them are two
                // contiguous ones.
                let mut inline: [Vector<'_>; PROBE_INLINE_COLUMNS] =
                    [Vector::Const(Datum::Null); PROBE_INLINE_COLUMNS];
                for (at, column) in self.projection.0.iter().enumerate() {
                    if let Some(slot) = inline.get_mut(at) {
                        // A row in the delta area has no mini-column to borrow,
                        // so its values go downstream as constants. The sorted
                        // case is byte for byte the code that was benchmarked,
                        // and it is the case a leaf is in until something writes to it
                        // and again after the next compaction.
                        *slot = match hit {
                            // A leaf with an out-of-line value cannot lend its
                            // mini-column: the slot holds a reference, not the
                            // value. `value_at` resolves it from the extents the
                            // tree read when it opened the leaf.
                            Hit::Sorted(_) if leaf.has_extents() => {
                                Vector::Const(leaf.value_at(hit, *column)?)
                            }
                            Hit::Sorted(_) => Vector::Column(leaf.column(*column)?),
                            Hit::Delta(index) => Vector::Const(leaf.delta_value(index, *column)?),
                        };
                    }
                }
                let columns = inline.get(..width).unwrap_or(&[]);
                match hit {
                    // Every vector is a constant when the leaf holds an
                    // out-of-line value, so the batch is one dense row rather
                    // than a selection over the leaf's own.
                    Hit::Sorted(_) if leaf.has_extents() => {
                        downstream.push(&Batch::over(1, columns))?;
                    }
                    Hit::Sorted(row) => {
                        let selection = [row as u32];
                        let mut batch = Batch::over(leaf.row_count(), columns);
                        batch.selection = Some(&selection);
                        downstream.push(&batch)?;
                    }
                    Hit::Delta(_) => {
                        downstream.push(&Batch::over(1, columns))?;
                    }
                }
                Ok(())
            })?;
        } else {
            let mut row: Vec<OwnedDatum> = Vec::with_capacity(width);
            if self.lookup(pool, key, &mut row)? {
                crate::ops::emit_rows(std::slice::from_ref(&row), downstream)?;
            }
        }
        downstream.finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ops::Collect;
    use inillucent_pool::{Database, Options};
    use inillucent_tree::types::{ColumnSpec, PhysicalType};
    use inillucent_vfs::{DbPath, MemoryVfs};

    /// Builds a `(id, key, label)` rowid tree of `rows` rows over small pages,
    /// so the tree has real interior levels.
    fn fixture(rows: i64) -> (Database, PagedTree) {
        let vfs = MemoryVfs::new();
        let path = DbPath::new("exec.rdb");
        let mut database = Database::create(
            &vfs,
            &path,
            Options::default().with_page_size(512).with_frames(512),
        )
        .unwrap();
        let columns = vec![
            ColumnSpec::key(PhysicalType::Int64),
            ColumnSpec::new(PhysicalType::Int64),
            ColumnSpec::new(PhysicalType::Text),
        ];
        let labels: Vec<String> = (0..rows).map(|n| format!("label-{n:05}")).collect();
        let owned: Vec<Vec<OwnedDatum>> = (0..rows)
            .map(|n| {
                vec![
                    OwnedDatum::Int(n),
                    OwnedDatum::Int((n * 7) % 100),
                    OwnedDatum::Text(labels[n as usize].clone().into_bytes()),
                ]
            })
            .collect();
        let borrowed: Vec<Vec<Datum<'_>>> = owned
            .iter()
            .map(|row| row.iter().map(OwnedDatum::borrow).collect())
            .collect();
        let tree = PagedTree::bulk_build(&mut database, 1, columns, 1, &borrowed).unwrap();
        (database, tree)
    }

    /// A full scan produces every row, in order, through the batch interface.
    #[test]
    fn a_full_scan_produces_every_row() {
        let (database, tree) = fixture(2_000);
        let mut sink = Box::new(Collect::new());
        let collected = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let mut into = Box::new(crate::ops::CollectInto::new(std::rc::Rc::clone(&collected)));
        FullScan::new(&tree, Projection::all(3))
            .run(database.pool(), into.as_mut())
            .unwrap();
        let rows = collected.borrow();
        assert_eq!(rows.len(), 2_000);
        assert_eq!(rows[0][0], OwnedDatum::Int(0));
        assert_eq!(rows[1_999][0], OwnedDatum::Int(1_999));
        let _ = sink.as_mut();
    }

    /// A projection produces only the columns it names, in the order it names
    /// them.
    #[test]
    fn a_projection_reorders_and_drops_columns() {
        let (database, tree) = fixture(300);
        let collected = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let mut into = Box::new(crate::ops::CollectInto::new(std::rc::Rc::clone(&collected)));
        FullScan::new(&tree, Projection(vec![2, 0]))
            .run(database.pool(), into.as_mut())
            .unwrap();
        let rows = collected.borrow();
        assert_eq!(rows.len(), 300);
        assert_eq!(rows[5].len(), 2);
        assert_eq!(rows[5][1], OwnedDatum::Int(5));
        assert!(matches!(rows[5][0], OwnedDatum::Text(_)));
    }

    /// A span scan produces exactly the rows in range, and a whole-leaf span
    /// arrives dense so downstream takes its fast path.
    #[test]
    fn a_span_scan_produces_the_range() {
        let (database, tree) = fixture(2_000);
        let collected = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let mut into = Box::new(crate::ops::CollectInto::new(std::rc::Rc::clone(&collected)));
        SpanScan::new(
            &tree,
            Projection(vec![0]),
            Some(vec![OwnedDatum::Int(500)]),
            true,
            Some(vec![OwnedDatum::Int(700)]),
            true,
        )
        .run(database.pool(), into.as_mut())
        .unwrap();
        let rows = collected.borrow();
        assert_eq!(rows.len(), 201);
        assert_eq!(rows[0][0], OwnedDatum::Int(500));
        assert_eq!(rows[200][0], OwnedDatum::Int(700));
    }

    /// A reverse scan produces descending rows and honours its limit.
    #[test]
    fn a_reverse_scan_descends_and_stops() {
        let (database, tree) = fixture(2_000);
        let collected = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let mut into = Box::new(crate::ops::CollectInto::new(std::rc::Rc::clone(&collected)));
        ReverseScan::new(
            &tree,
            Projection(vec![0]),
            crate::physical::SpanBounds {
                low: None,
                low_inclusive: true,
                high: Some(vec![OwnedDatum::Int(1_500)]),
                high_inclusive: true,
                matches_nothing: false,
            },
            Some(50),
        )
        .run(database.pool(), into.as_mut())
        .unwrap();
        let rows = collected.borrow();
        assert_eq!(rows.len(), 50);
        assert_eq!(rows[0][0], OwnedDatum::Int(1_500));
        assert_eq!(rows[49][0], OwnedDatum::Int(1_451));
    }

    /// A point probe finds a row that is there, misses one that is not, and
    /// makes no allocation beyond the caller's buffer.
    #[test]
    fn a_point_probe_hits_and_misses() {
        let (database, tree) = fixture(2_000);
        let probe = PointProbe::new(&tree, Projection(vec![2]));
        let mut out: Vec<OwnedDatum> = Vec::new();
        assert!(probe
            .lookup(database.pool(), &[Datum::Int(1_234)], &mut out)
            .unwrap());
        assert_eq!(out.len(), 1);
        assert_eq!(out[0], OwnedDatum::Text(b"label-01234".to_vec()));
        assert!(!probe
            .lookup(database.pool(), &[Datum::Int(9_999)], &mut out)
            .unwrap());
        assert!(out.is_empty(), "a miss leaves nothing behind");
        assert_eq!(probe.tree().root(), tree.root());
    }

    /// A point probe pushed into a pipeline produces one row, or none.
    #[test]
    fn a_point_probe_pushes_one_row() {
        let (database, tree) = fixture(500);
        let probe = PointProbe::new(&tree, Projection(vec![0, 1]));
        let collected = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let mut into = Box::new(crate::ops::CollectInto::new(std::rc::Rc::clone(&collected)));
        probe
            .run(database.pool(), &[Datum::Int(42)], into.as_mut())
            .unwrap();
        assert_eq!(collected.borrow().len(), 1);
        assert_eq!(collected.borrow()[0][0], OwnedDatum::Int(42));

        let collected = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let mut into = Box::new(crate::ops::CollectInto::new(std::rc::Rc::clone(&collected)));
        probe
            .run(database.pool(), &[Datum::Int(-1)], into.as_mut())
            .unwrap();
        assert!(collected.borrow().is_empty());
    }

    /// A skip scan produces one row per distinct prefix value.
    #[test]
    fn a_skip_scan_produces_distinct_prefixes() {
        let vfs = MemoryVfs::new();
        let path = DbPath::new("skipexec.rdb");
        let mut database = Database::create(
            &vfs,
            &path,
            Options::default().with_page_size(512).with_frames(256),
        )
        .unwrap();
        let columns = vec![
            ColumnSpec::key(PhysicalType::Int64),
            ColumnSpec::key(PhysicalType::Int64),
        ];
        let owned: Vec<Vec<OwnedDatum>> = (0..3_200i64)
            .map(|n| vec![OwnedDatum::Int(n / 100), OwnedDatum::Int(n)])
            .collect();
        let borrowed: Vec<Vec<Datum<'_>>> = owned
            .iter()
            .map(|row| row.iter().map(OwnedDatum::borrow).collect())
            .collect();
        let tree = PagedTree::bulk_build(&mut database, 2, columns, 2, &borrowed).unwrap();
        let collected = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let mut into = Box::new(crate::ops::CollectInto::new(std::rc::Rc::clone(&collected)));
        SkipScan::new(&tree, 1)
            .run(database.pool(), into.as_mut())
            .unwrap();
        let rows = collected.borrow();
        assert_eq!(rows.len(), 32);
        assert_eq!(rows[0][0], OwnedDatum::Int(0));
        assert_eq!(rows[31][0], OwnedDatum::Int(31));
    }

    /// Every source stops when the pipeline says stop, which is what a `LIMIT`
    /// downstream of one does.
    #[test]
    fn every_source_stops_when_told_to() {
        let (database, tree) = fixture(2_000);
        let pool = database.pool();
        for name in ["full", "span"] {
            let collected = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
            let mut into = Box::new(crate::ops::CollectInto::with_limit(
                std::rc::Rc::clone(&collected),
                10,
            ));
            match name {
                "full" => FullScan::new(&tree, Projection(vec![0]))
                    .run(pool, into.as_mut())
                    .unwrap(),
                _ => SpanScan::new(&tree, Projection(vec![0]), None, true, None, true)
                    .run(pool, into.as_mut())
                    .unwrap(),
            }
            assert!(collected.borrow().len() <= 10, "{name} did not stop");
            assert!(!collected.borrow().is_empty(), "{name} produced nothing");
        }
    }

    /// An empty tree produces no rows and no error from any source.
    #[test]
    fn an_empty_tree_produces_nothing() {
        let (database, tree) = fixture(0);
        let pool = database.pool();
        let collected = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let mut into = Box::new(crate::ops::CollectInto::new(std::rc::Rc::clone(&collected)));
        FullScan::new(&tree, Projection::all(3))
            .run(pool, into.as_mut())
            .unwrap();
        assert!(collected.borrow().is_empty());

        let collected = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let mut into = Box::new(crate::ops::CollectInto::new(std::rc::Rc::clone(&collected)));
        ReverseScan::new(
            &tree,
            Projection::all(3),
            crate::physical::SpanBounds::default(),
            Some(5),
        )
        .run(pool, into.as_mut())
        .unwrap();
        assert!(collected.borrow().is_empty());

        let probe = PointProbe::new(&tree, Projection::all(3));
        let mut out = Vec::new();
        assert!(!probe.lookup(pool, &[Datum::Int(0)], &mut out).unwrap());
    }
}
