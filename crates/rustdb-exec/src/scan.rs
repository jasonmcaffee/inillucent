//! The source: a scan that turns leaves into batches without copying.
//!
//! Invariant: a batch produced here borrows the leaf it came from and is dead
//! by the time the scan moves on. That is enforced by the lifetime rather than
//! by discipline - `push` takes `&Batch<'_>` and the leaf reference lives only
//! inside one loop iteration, so an operator that wanted to keep a value past
//! the leaf would not compile.
//!
//! ## One batch per leaf, and why
//!
//! A leaf holds up to a few thousand rows at the sizes this engine uses, so a
//! leaf *is* a batch. Coalescing several small leaves into one 2048-row batch
//! would mean copying their columns into a scratch buffer, which is exactly the
//! copy the design exists to avoid; splitting a large leaf into several batches
//! is free, because a batch is a slice of the mini-columns and a slice of a
//! slice costs nothing. So a leaf larger than [`BATCH_ROWS`] is split and a
//! leaf smaller than it is not padded.
//!
//! A leaf that is *not* clean - NULLs are fine, but exceptions, tombstones or
//! delta rows are not - takes the materialising path: its live rows are copied
//! into owned storage once and pushed as ordinary batches. That keeps the fast
//! path branch-free at the cost of making the rare case slower, which is the
//! trade the whole leaf layout is built on.

use rustdb_base::DbResult;
use rustdb_tree::datum::{Datum, OwnedDatum};
use rustdb_tree::{LeafRef, Tree};

use crate::batch::{Batch, Vector, BATCH_ROWS};
use crate::ops::{Flow, Sink};

/// Which columns a scan produces, in output order.
#[derive(Clone, Debug)]
pub struct Projection(pub Vec<usize>);

impl Projection {
    /// Returns a projection of every column of a tree, in tree order.
    ///
    /// @param columns - how many columns the tree has
    pub fn all(columns: usize) -> Projection {
        Projection((0..columns).collect())
    }
}

/// Walks a tree's leaves and pushes batches downstream.
pub struct TableScan<'t> {
    tree: &'t Tree,
    projection: Projection,
}

impl<'t> TableScan<'t> {
    /// Returns a scan over a tree.
    ///
    /// @param tree - the tree to walk
    /// @param projection - which columns to produce, in output order
    pub fn new(tree: &'t Tree, projection: Projection) -> TableScan<'t> {
        TableScan { tree, projection }
    }

    /// Drives the whole scan, then finishes the pipeline.
    ///
    /// @param downstream - the head of the pipeline
    pub fn run(&self, downstream: &mut dyn Sink) -> DbResult<()> {
        let mut cursor = self.tree.scan();
        'leaves: while let Some(leaf) = cursor.next_leaf()? {
            if leaf.is_clean() {
                if self.push_clean(&leaf, downstream)? == Flow::Stop {
                    break 'leaves;
                }
            } else if self.push_materialised(&leaf, downstream)? == Flow::Stop {
                break 'leaves;
            }
        }
        downstream.finish()
    }

    /// Pushes a clean leaf as one or more borrowing batches.
    ///
    /// @param leaf - the leaf to push
    /// @param downstream - the head of the pipeline
    fn push_clean(&self, leaf: &LeafRef<'_>, downstream: &mut dyn Sink) -> DbResult<Flow> {
        let rows = leaf.row_count();
        if rows == 0 {
            return Ok(Flow::Continue);
        }
        // The whole-leaf vectors, built once. Slicing them per batch is what
        // makes splitting a large leaf free.
        let mut whole: Vec<Vector<'_>> = Vec::with_capacity(self.projection.0.len());
        for index in &self.projection.0 {
            whole.push(Vector::from_column(leaf.column(*index)?));
        }
        let mut start = 0usize;
        while start < rows {
            let len = rows.saturating_sub(start).min(BATCH_ROWS);
            let columns: Vec<Vector<'_>> = whole
                .iter()
                .map(|vector| slice_vector(*vector, start, len))
                .collect();
            let batch = Batch::new(len, columns);
            if downstream.push(&batch)? == Flow::Stop {
                return Ok(Flow::Stop);
            }
            start = start.saturating_add(len);
        }
        Ok(Flow::Continue)
    }

    /// Pushes a leaf that is not on the fast path, by materialising its rows.
    ///
    /// @param leaf - the leaf to push
    /// @param downstream - the head of the pipeline
    fn push_materialised(&self, leaf: &LeafRef<'_>, downstream: &mut dyn Sink) -> DbResult<Flow> {
        let live = leaf.live()?;
        if live.is_empty() {
            return Ok(Flow::Continue);
        }
        let owned: Vec<Vec<OwnedDatum>> = live
            .iter()
            .map(|row| {
                self.projection
                    .0
                    .iter()
                    .map(|index| {
                        row.get(*index)
                            .map(OwnedDatum::from_datum)
                            .unwrap_or(OwnedDatum::Null)
                    })
                    .collect()
            })
            .collect();
        let mut start = 0usize;
        while start < owned.len() {
            let end = start.saturating_add(BATCH_ROWS).min(owned.len());
            let chunk = owned.get(start..end).unwrap_or(&[]);
            let mut per_column: Vec<Vec<Datum<'_>>> =
                Vec::with_capacity(self.projection.0.len());
            for column in 0..self.projection.0.len() {
                per_column.push(
                    chunk
                        .iter()
                        .map(|row| {
                            row.get(column)
                                .map(OwnedDatum::borrow)
                                .unwrap_or(Datum::Null)
                        })
                        .collect(),
                );
            }
            let columns: Vec<Vector<'_>> = per_column
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
}

/// Narrows a whole-leaf vector to one batch's rows.
///
/// @param vector - the whole-leaf vector
/// @param start - the first row of the batch
/// @param len - how many rows the batch holds
fn slice_vector(vector: Vector<'_>, start: usize, len: usize) -> Vector<'_> {
    match vector {
        Vector::Int64 { bytes, class } => Vector::Int64 {
            bytes: slice_inline(bytes, start, len),
            // A class array is two bits per row, so it can only be sliced at a
            // four-row boundary. Rather than carry an offset, a batch that does
            // not start on one keeps the whole array and is therefore only
            // sliced when the class array is absent - which is the fast path,
            // and the only path where it matters.
            class,
        },
        Vector::Float64 { bytes, class } => Vector::Float64 {
            bytes: slice_inline(bytes, start, len),
            class,
        },
        // The general path is not sliced: a batch over it addresses rows by
        // their position in the leaf, which is what `Vector::Column` expects.
        other => other,
    }
}

/// Slices a run of 8-byte values.
///
/// @param bytes - the whole value array
/// @param start - the first row
/// @param len - how many rows
fn slice_inline(bytes: &[u8], start: usize, len: usize) -> &[u8] {
    let from = start.saturating_mul(8);
    let to = from.saturating_add(len.saturating_mul(8)).min(bytes.len());
    bytes.get(from..to).unwrap_or(&[])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::aggregate::AggregateKind;
    use crate::expr::{compile, Expr, StaticType};
    use crate::ops::{AggregateSpec, Collect, SimpleAggregate};
    use rustdb_tree::types::{ColumnSpec, PhysicalType};

    fn columns() -> Vec<ColumnSpec> {
        vec![
            ColumnSpec::key(PhysicalType::Int64),
            ColumnSpec::new(PhysicalType::Int64),
            ColumnSpec::new(PhysicalType::Text),
        ]
    }

    fn tree_of(rows: usize, page_size: usize) -> Tree {
        let owned: Vec<Vec<OwnedDatum>> = (0..rows)
            .map(|n| {
                vec![
                    OwnedDatum::Int(n as i64),
                    OwnedDatum::Int((n as i64) % 64),
                    OwnedDatum::Text(b"a label of some length".to_vec()),
                ]
            })
            .collect();
        let borrowed: Vec<Vec<Datum<'_>>> = owned
            .iter()
            .map(|row| row.iter().map(OwnedDatum::borrow).collect())
            .collect();
        Tree::bulk_build(page_size, 1, columns(), 1, &borrowed).unwrap()
    }

    /// A scan produces every row exactly once, in key order, at page sizes that
    /// give one leaf and many.
    #[test]
    fn a_scan_produces_every_row_once() {
        for (rows, page_size) in [(10usize, 65_536usize), (5_000, 8_192), (0, 8_192)] {
            let tree = tree_of(rows, page_size);
            let mut collect = Collect::new();
            TableScan::new(&tree, Projection::all(3))
                .run(&mut collect)
                .unwrap();
            assert_eq!(collect.rows().len(), rows, "{rows} rows at {page_size}");
            for (index, row) in collect.rows().iter().enumerate() {
                assert_eq!(row[0].borrow().as_int(), Some(index as i64));
                assert_eq!(row[1].borrow().as_int(), Some(index as i64 % 64));
            }
        }
    }

    /// A leaf larger than one batch is split, and the split changes nothing.
    #[test]
    fn a_large_leaf_is_split_into_batches() {
        let tree = tree_of(5_000, 65_536);
        let mut widths = Vec::new();
        struct Widths<'a>(&'a mut Vec<usize>);
        impl Sink for Widths<'_> {
            fn push(&mut self, batch: &Batch<'_>) -> DbResult<Flow> {
                self.0.push(batch.live());
                Ok(Flow::Continue)
            }
            fn finish(&mut self) -> DbResult<()> {
                Ok(())
            }
        }
        TableScan::new(&tree, Projection::all(3))
            .run(&mut Widths(&mut widths))
            .unwrap();
        assert!(widths.iter().all(|width| *width <= BATCH_ROWS), "{widths:?}");
        assert_eq!(widths.iter().sum::<usize>(), 5_000);
        assert!(widths.len() > 2, "a 64 KiB leaf should hold several batches");
    }

    /// The sum a scan feeds an aggregate is the sum of the rows, whichever page
    /// size the tree was built at - which is the end-to-end version of the
    /// dense/generic agreement the operator tests assert in isolation.
    #[test]
    fn the_scan_and_aggregate_agree_across_page_sizes() {
        let wanted: i64 = (0..3_000i64).map(|n| n % 64).sum();
        for page_size in [8_192usize, 16_384, 32_768, 65_536] {
            let tree = tree_of(3_000, page_size);
            let mut aggregate = SimpleAggregate::new(
                vec![
                    AggregateSpec {
                        kind: AggregateKind::CountStar,
                        argument: None,
                    },
                    AggregateSpec {
                        kind: AggregateKind::Sum,
                        argument: Some(
                            compile(&Expr::Column(1), &[StaticType::Int; 2]).unwrap(),
                        ),
                    },
                ],
                Box::new(Collect::new()),
            );
            TableScan::new(&tree, Projection(vec![0, 1]))
                .run(&mut aggregate)
                .unwrap();
            let count = aggregate.accumulator(0).unwrap().finish().unwrap();
            let sum = aggregate.accumulator(1).unwrap().finish().unwrap();
            assert_eq!(count.borrow().as_int(), Some(3_000), "page size {page_size}");
            assert_eq!(sum.borrow().as_int(), Some(wanted), "page size {page_size}");
        }
    }

    /// A tree with NULLs and exceptions produces the same rows as one without,
    /// because the scan falls back per leaf rather than per table.
    #[test]
    fn a_leaf_with_exceptions_still_produces_every_row() {
        let owned: Vec<Vec<OwnedDatum>> = (0..500)
            .map(|n| {
                vec![
                    OwnedDatum::Int(n as i64),
                    match n % 3 {
                        0 => OwnedDatum::Int(n as i64),
                        1 => OwnedDatum::Null,
                        // A string in a column whose physical type is Int64:
                        // an exception, which takes its leaf off the fast path.
                        _ => OwnedDatum::Text(b"not a number".to_vec()),
                    },
                    OwnedDatum::Text(b"label".to_vec()),
                ]
            })
            .collect();
        let borrowed: Vec<Vec<Datum<'_>>> = owned
            .iter()
            .map(|row| row.iter().map(OwnedDatum::borrow).collect())
            .collect();
        let tree = Tree::bulk_build(8_192, 1, columns(), 1, &borrowed).unwrap();
        let mut clean = 0usize;
        for index in 0..tree.leaf_count() {
            if tree.leaf(index).unwrap().is_clean() {
                clean = clean.saturating_add(1);
            }
        }
        assert!(clean < tree.leaf_count(), "the exceptions should show");
        let mut collect = Collect::new();
        TableScan::new(&tree, Projection::all(3))
            .run(&mut collect)
            .unwrap();
        assert_eq!(collect.rows().len(), 500);
        for (index, row) in collect.rows().iter().enumerate() {
            assert_eq!(row[0].borrow().as_int(), Some(index as i64));
            match index % 3 {
                0 => assert_eq!(row[1].borrow().as_int(), Some(index as i64)),
                1 => assert!(row[1].borrow().is_null()),
                _ => assert_eq!(row[1].borrow().as_bytes(), Some(b"not a number".as_slice())),
            }
        }
    }
}
