//! Joins, materialisation, and the row store the two share.
//!
//! Invariant: a joined row owns its values. A batch borrows the leaf it came
//! from and a join combines rows from two leaves that are not pinned at the
//! same moment, so the combination has to be materialised - there is no
//! arrangement of the borrow that avoids it, and pretending otherwise is how a
//! use-after-evict gets written. What the design *can* avoid is materialising
//! the side that does not need it, and it does: the probe side of a hash join
//! and the outer side of a nested loop both stay in their batches until a match
//! is found.
//!
//! ## The three join strategies, and when each is right
//!
//! | operator | when the physical pass chooses it |
//! |---|---|
//! | [`IndexNestedLoopJoin`] | the inner side has an index on the join key and the outer side is small - a selective join |
//! | [`HashJoin`] | no usable index, or the outer side is large enough that a build pays for itself |
//! | [`NestedLoopJoin`] | neither, and a cross product or a correlated condition is what is left |
//!
//! Both `read.join` workloads in the scorecard are index nested loops, and that
//! is not an accident of the harness: they are `WHERE main_table.id = ?1` and
//! `WHERE main_table.key BETWEEN ?1 AND ?1 + 200` against a side table indexed
//! on the join column, so the outer side is one row or two hundred and the
//! inner has an index. SQLite chooses the same shape. A hash join over two
//! hundred outer rows would build a hash table nobody needed, which is why the
//! cardinality threshold in the physical pass exists and why it is measured
//! rather than assumed.
//!
//! ## What an index nested loop copies, which is nothing
//!
//! The first version materialised every joined row: one `Vec` for the outer
//! half, another for the concatenation, and - because `range.lookaside` reads
//! `label` - a 45-byte string copied per row. It measured **765 ns per row**
//! against a 444 ns point probe, so more than a third of the time was the
//! copying.
//!
//! It now pushes a batch whose *outer* columns are constant vectors and whose
//! *inner* columns borrow the leaf the probe landed on. A constant vector is
//! exactly the right shape for the outer row of a join - one value repeated for
//! every match - and the inner side stays a real vector with a selection over
//! the matching span, so a prefix probe that finds forty entries is one batch
//! rather than forty rows. Nothing is copied at all.
//!
//! The outer side was always vectorised. What changed is that the *output* is
//! too.

// **The four modules this file is made of (task-1962, A7).** Everything is re-exported under the
// path it had, so no call site in the workspace moved.
mod hash;
mod loops;
mod probed;
mod store;
pub use hash::*;
pub use loops::*;
pub use probed::*;
pub use store::*;
pub(crate) use store::{encode_row_key, materialise};

/// How many join-key columns fit on the stack.
///
/// Four covers every index the dialect's own corpus builds and every one in the
/// scorecard fixture. A wider key spills to the heap, which is correct and only
/// costs what the previous version cost on every key.
const INLINE_KEYS: usize = 4;

/// How many joined columns a probe's column list keeps on the stack.
///
/// The scorecard's widest joined row is seven vectors - five table columns and
/// two index ones - and the array is filled in whether the row needs every slot
/// or not, so the size is a cost rather than a ceiling: a `Vector` is about
/// seventy bytes, and twelve slots was most of a kilobyte of stack stores per
/// probed row. A wider row spills to the heap, which costs what every row used
/// to cost.
const INLINE_COLUMNS: usize = 8;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::batch::Batch;
    use crate::expr::{compile, Expr, StaticType};
    use crate::ops::{emit_rows, CollectInto, Flow, Sink};
    use crate::scan::Projection;
    use inillucent_base::DbResult;
    use inillucent_pool::{Database, Options};
    use inillucent_tree::datum::{Datum, OwnedDatum};
    use inillucent_tree::types::{ColumnSpec, PhysicalType};
    use inillucent_tree::PagedTree;
    use inillucent_vfs::{DbPath, MemoryVfs};
    use std::cell::RefCell;
    use std::rc::Rc;

    /// A sink that keeps every row it is given.
    fn collector() -> (Rc<RefCell<Vec<Vec<OwnedDatum>>>>, Box<dyn Sink>) {
        let rows = Rc::new(RefCell::new(Vec::new()));
        let sink = Box::new(CollectInto::new(Rc::clone(&rows)));
        (rows, sink)
    }

    /// Returns a one-batch source over literal integer rows.
    fn push_rows(sink: &mut dyn Sink, rows: &[Vec<OwnedDatum>]) -> DbResult<()> {
        emit_rows(rows, sink)?;
        Ok(())
    }

    /// Returns rows of `(id, tag)`.
    fn pairs(values: &[(i64, i64)]) -> Vec<Vec<OwnedDatum>> {
        values
            .iter()
            .map(|(a, b)| vec![OwnedDatum::Int(*a), OwnedDatum::Int(*b)])
            .collect()
    }

    /// An inner hash join emits exactly the matching pairs, in probe order.
    #[test]
    fn an_inner_hash_join_pairs_matching_rows() {
        let (rows, sink) = collector();
        let build_key = compile(&Expr::Column(0), &[StaticType::Int, StaticType::Int]).unwrap();
        let probe_key = compile(&Expr::Column(0), &[StaticType::Int, StaticType::Int]).unwrap();
        let mut join = HashJoin::new(JoinKind::Inner, vec![build_key], vec![probe_key], sink);
        let build = pairs(&[(1, 10), (2, 20), (2, 21), (3, 30)]);
        let mut into = RowStore::new();
        for row in &build {
            into.push(row.clone());
        }
        // Feed the build side through the batch interface, as the planner does.
        struct Builder<'a, 's>(&'a mut HashJoin<'s>);
        impl Sink for Builder<'_, '_> {
            fn push(&mut self, batch: &Batch<'_>) -> DbResult<Flow> {
                self.0.build(batch)?;
                Ok(Flow::Continue)
            }
            fn finish(&mut self) -> DbResult<()> {
                Ok(())
            }
            /// The test sinks hold no state that survives an execution.
            fn reset(&mut self) -> DbResult<()> {
                Ok(())
            }
        }
        {
            let mut builder = Builder(&mut join);
            push_rows(&mut builder, &build).unwrap();
        }
        assert_eq!(join.build_rows(), 4);
        push_rows(&mut join, &pairs(&[(2, 200), (4, 400), (1, 100)])).unwrap();
        join.finish().unwrap();
        let produced = rows.borrow();
        // (2,200) matches two build rows, (1,100) matches one, (4,400) none.
        assert_eq!(produced.len(), 3);
        assert_eq!(produced[0][0], OwnedDatum::Int(2));
        assert_eq!(produced[0][3], OwnedDatum::Int(20));
        assert_eq!(produced[1][3], OwnedDatum::Int(21));
        assert_eq!(produced[2][0], OwnedDatum::Int(1));
    }

    /// A left join null-extends what does not match; semi and anti keep the
    /// probe row alone.
    #[test]
    fn the_outer_kinds_treat_misses_as_they_should() {
        let types = [StaticType::Int, StaticType::Int];
        for (kind, wanted) in [
            (JoinKind::Left, vec![1i64, 4]),
            (JoinKind::Semi, vec![1]),
            (JoinKind::Anti, vec![4]),
        ] {
            let (rows, sink) = collector();
            let mut join = HashJoin::new(
                kind,
                vec![compile(&Expr::Column(0), &types).unwrap()],
                vec![compile(&Expr::Column(0), &types).unwrap()],
                sink,
            );
            struct Builder<'a, 's>(&'a mut HashJoin<'s>);
            impl Sink for Builder<'_, '_> {
                fn push(&mut self, batch: &Batch<'_>) -> DbResult<Flow> {
                    self.0.build(batch)?;
                    Ok(Flow::Continue)
                }
                fn finish(&mut self) -> DbResult<()> {
                    Ok(())
                }
                /// The test sinks hold no state that survives an execution.
                fn reset(&mut self) -> DbResult<()> {
                    Ok(())
                }
            }
            {
                let mut builder = Builder(&mut join);
                push_rows(&mut builder, &pairs(&[(1, 10)])).unwrap();
            }
            push_rows(&mut join, &pairs(&[(1, 100), (4, 400)])).unwrap();
            join.finish().unwrap();
            let produced = rows.borrow();
            let ids: Vec<i64> = produced
                .iter()
                .filter_map(|row| match row.first() {
                    Some(OwnedDatum::Int(value)) => Some(*value),
                    _ => None,
                })
                .collect();
            assert_eq!(ids, wanted, "{kind:?}");
            if kind == JoinKind::Left {
                // The unmatched row is null-extended to the build width.
                assert_eq!(produced[1].len(), 4);
                assert_eq!(produced[1][2], OwnedDatum::Null);
            }
        }
    }

    /// A NULL join key matches nothing, on either side.
    #[test]
    fn a_null_key_matches_nothing() {
        let types = [StaticType::Unknown, StaticType::Int];
        let (rows, sink) = collector();
        let mut join = HashJoin::new(
            JoinKind::Inner,
            vec![compile(&Expr::Column(0), &types).unwrap()],
            vec![compile(&Expr::Column(0), &types).unwrap()],
            sink,
        );
        struct Builder<'a, 's>(&'a mut HashJoin<'s>);
        impl Sink for Builder<'_, '_> {
            fn push(&mut self, batch: &Batch<'_>) -> DbResult<Flow> {
                self.0.build(batch)?;
                Ok(Flow::Continue)
            }
            fn finish(&mut self) -> DbResult<()> {
                Ok(())
            }
            /// The test sinks hold no state that survives an execution.
            fn reset(&mut self) -> DbResult<()> {
                Ok(())
            }
        }
        {
            let mut builder = Builder(&mut join);
            push_rows(&mut builder, &[vec![OwnedDatum::Null, OwnedDatum::Int(1)]]).unwrap();
        }
        push_rows(&mut join, &[vec![OwnedDatum::Null, OwnedDatum::Int(2)]]).unwrap();
        join.finish().unwrap();
        assert!(rows.borrow().is_empty(), "NULL = NULL is not true in SQL");
    }

    /// An index nested loop join probes the inner tree once per outer row.
    #[test]
    fn an_index_nested_loop_join_probes_per_outer_row() {
        let vfs = MemoryVfs::new();
        let path = DbPath::new("inlj.rdb");
        let mut database = Database::create(
            &vfs,
            &path,
            Options::default().with_page_size(512).with_frames(256),
        )
        .unwrap();
        let columns = vec![
            ColumnSpec::key(PhysicalType::Int64),
            ColumnSpec::new(PhysicalType::Int64),
        ];
        let owned: Vec<Vec<OwnedDatum>> = (0..500i64)
            .map(|n| vec![OwnedDatum::Int(n), OwnedDatum::Int(n * 3)])
            .collect();
        let borrowed: Vec<Vec<Datum<'_>>> = owned
            .iter()
            .map(|row| row.iter().map(OwnedDatum::borrow).collect())
            .collect();
        let inner = PagedTree::bulk_build(&mut database, 4, columns, 1, &borrowed).unwrap();

        let (rows, sink) = collector();
        let types = [StaticType::Int];
        let mut join = IndexNestedLoopJoin::new(
            JoinKind::Inner,
            &inner,
            database.pool(),
            vec![compile(&Expr::Column(0), &types).unwrap()],
            Projection(vec![1]),
            true,
            sink,
        );
        let outer: Vec<Vec<OwnedDatum>> = [3i64, 7, 999, 11]
            .iter()
            .map(|n| vec![OwnedDatum::Int(*n)])
            .collect();
        push_rows(&mut join, &outer).unwrap();
        join.finish().unwrap();
        let produced = rows.borrow();
        assert_eq!(produced.len(), 3, "999 is not in the inner tree");
        assert_eq!(produced[0], vec![OwnedDatum::Int(3), OwnedDatum::Int(9)]);
        assert_eq!(produced[2], vec![OwnedDatum::Int(11), OwnedDatum::Int(33)]);
    }

    /// An index nested loop over a key *prefix* emits every inner row sharing
    /// it, which is the shape `join.range` has.
    #[test]
    fn an_index_nested_loop_over_a_prefix_emits_every_match() {
        let vfs = MemoryVfs::new();
        let path = DbPath::new("inljrange.rdb");
        let mut database = Database::create(
            &vfs,
            &path,
            Options::default().with_page_size(512).with_frames(256),
        )
        .unwrap();
        // An index tree: (owner, rowid).
        let columns = vec![
            ColumnSpec::key(PhysicalType::Int64),
            ColumnSpec::key(PhysicalType::Int64),
        ];
        let owned: Vec<Vec<OwnedDatum>> = (0..600i64)
            .map(|n| vec![OwnedDatum::Int(n / 3), OwnedDatum::Int(n)])
            .collect();
        let borrowed: Vec<Vec<Datum<'_>>> = owned
            .iter()
            .map(|row| row.iter().map(OwnedDatum::borrow).collect())
            .collect();
        let inner = PagedTree::bulk_build(&mut database, 6, columns, 2, &borrowed).unwrap();

        let (rows, sink) = collector();
        let types = [StaticType::Int];
        let mut join = IndexNestedLoopJoin::new(
            JoinKind::Inner,
            &inner,
            database.pool(),
            vec![compile(&Expr::Column(0), &types).unwrap()],
            Projection(vec![1]),
            false,
            sink,
        );
        push_rows(&mut join, &[vec![OwnedDatum::Int(5)]]).unwrap();
        join.finish().unwrap();
        let produced = rows.borrow();
        assert_eq!(produced.len(), 3, "owner 5 has three rows");
        assert_eq!(produced[0][1], OwnedDatum::Int(15));
        assert_eq!(produced[2][1], OwnedDatum::Int(17));
    }

    /// A nested loop with no condition is a cross product; with one it filters.
    #[test]
    fn a_nested_loop_is_a_cross_product_until_a_condition_narrows_it() {
        let inner = pairs(&[(1, 10), (2, 20)]);
        let (rows, sink) = collector();
        let mut join = NestedLoopJoin::new(JoinKind::Inner, inner.clone(), None, sink);
        push_rows(&mut join, &[vec![OwnedDatum::Int(7)]]).unwrap();
        join.finish().unwrap();
        assert_eq!(rows.borrow().len(), 2);
        assert_eq!(rows.borrow()[0].len(), 3);

        // Now with a condition: outer column 0 equals inner column 0, which is
        // column 1 of the concatenation.
        let types = [StaticType::Int, StaticType::Int, StaticType::Int];
        let condition = compile(
            &Expr::Compare(
                crate::expr::CompareOp::Equal,
                Box::new(Expr::Column(0)),
                Box::new(Expr::Column(1)),
            ),
            &types,
        )
        .unwrap();
        let (rows, sink) = collector();
        let mut join = NestedLoopJoin::new(JoinKind::Inner, inner, Some(condition), sink);
        push_rows(&mut join, &[vec![OwnedDatum::Int(2)]]).unwrap();
        join.finish().unwrap();
        assert_eq!(rows.borrow().len(), 1);
        assert_eq!(rows.borrow()[0][2], OwnedDatum::Int(20));
    }

    /// A materialiser replays exactly what it absorbed.
    #[test]
    fn a_materialiser_replays_its_input() {
        let (rows, sink) = collector();
        let mut hold = Materialize::new(sink);
        let input = pairs(&[(1, 2), (3, 4), (5, 6)]);
        push_rows(&mut hold, &input).unwrap();
        assert!(rows.borrow().is_empty(), "a breaker emits nothing early");
        hold.finish().unwrap();
        assert_eq!(rows.borrow().len(), 3);
        assert_eq!(rows.borrow()[2][1], OwnedDatum::Int(6));
    }

    /// A values scan produces its literal rows and nothing else.
    #[test]
    fn a_values_scan_produces_its_rows() {
        let (rows, sink) = collector();
        let mut sink = sink;
        ValuesScan::new(pairs(&[(1, 2), (3, 4)]))
            .run(sink.as_mut())
            .unwrap();
        assert_eq!(rows.borrow().len(), 2);
        let (rows, mut sink) = collector();
        ValuesScan::new(Vec::new()).run(sink.as_mut()).unwrap();
        assert!(rows.borrow().is_empty());
    }

    /// The row store absorbs batches and drains them back unchanged.
    #[test]
    fn the_row_store_round_trips() {
        let mut store = RowStore::new();
        assert!(store.is_empty());
        let (rows, mut sink) = collector();
        struct Absorb<'a>(&'a mut RowStore);
        impl Sink for Absorb<'_> {
            fn push(&mut self, batch: &Batch<'_>) -> DbResult<Flow> {
                self.0.absorb(batch)?;
                Ok(Flow::Continue)
            }
            fn finish(&mut self) -> DbResult<()> {
                Ok(())
            }
            /// The test sinks hold no state that survives an execution.
            fn reset(&mut self) -> DbResult<()> {
                Ok(())
            }
        }
        {
            let mut absorb = Absorb(&mut store);
            push_rows(&mut absorb, &pairs(&[(1, 2), (3, 4)])).unwrap();
        }
        assert_eq!(store.len(), 2);
        assert_eq!(store.rows()[1][0], OwnedDatum::Int(3));
        store.drain_into(sink.as_mut()).unwrap();
        assert_eq!(rows.borrow().len(), 2);
        assert!(store.is_empty());
    }
}
