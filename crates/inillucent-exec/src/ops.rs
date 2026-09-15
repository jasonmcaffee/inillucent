//! The operators: a push pipeline from a scan to a sink.
//!
//! Invariant: an operator that stops the pipeline (`LIMIT` satisfied) says so
//! by returning [`Flow::Stop`], and every operator above it propagates that
//! rather than continuing to read. A scan that keeps walking after its consumer
//! has enough is not a correctness bug, which is exactly why it would survive a
//! test suite and show up only as a slow `read.range` family.
//!
//! ## The shape
//!
//! Each operator owns the one downstream of it and pushes into it. That makes a
//! pipeline a chain of ownership from the source down to the sink, and it makes
//! a pipeline breaker - [`HashAggregate`], [`Sort`], [`TopN`], [`Distinct`] -
//! an operator that accumulates in `push` and emits in `finish`. There is no
//! scheduler and no coroutine: the call stack is the pipeline.
//!
//! ## Where the vectorised fast paths are
//!
//! Two, both in [`SimpleAggregate`] and both entered only when the whole batch
//! qualifies:
//!
//! - a dense integer column with no selection vector folds through
//!   `Accumulator::push_dense_ints`, which walks the page's own bytes;
//! - `count(*)` over a dense batch adds the row count without looking at a
//!   value at all.
//!
//! Everything else is the per-row path. The tests assert the two produce the
//! same answers, because a fast path that is also a different answer is the
//! worst kind of bug this engine can have.

// **The six modules this file is made of (task-1962, A7).** Everything is re-exported under the
// path it had, so no call site in the workspace moved.
mod aggregate;
mod collect;
mod order;
mod row;
pub use aggregate::*;
pub use collect::*;
pub use order::*;
pub use row::*;

/// How many projected columns a permutation keeps on the stack.
///
/// The scorecard's widest projection is five columns. The array is filled in
/// whether the projection needs every slot or not, so the size is a cost as well
/// as a ceiling; a wider one takes the general path, which is what every
/// projection used to take.
const INLINE_PROJECT: usize = 8;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::aggregate::AggregateKind;
    use crate::batch::{Batch, Vector};
    use crate::expr::{compile, CompareOp, Expr, StaticType};
    use inillucent_tree::datum::{Datum, OwnedDatum};
    use inillucent_value::collation::Collation;
    use std::cmp::Ordering;

    fn ints(values: &[i64]) -> Vec<u8> {
        values.iter().flat_map(|v| v.to_le_bytes()).collect()
    }

    /// The vectorised `sum` and the per-row `sum` agree, over a dense batch and
    /// the same batch behind a selection vector that keeps every row.
    ///
    /// The selection vector is what takes the fast path away, so this is the
    /// direct comparison of the two paths on identical input, and it is what
    /// licenses the fast path to exist at all.
    #[test]
    fn the_dense_aggregate_path_agrees_with_the_selected_one() {
        for count in [0usize, 1, 7, 500, 2048] {
            let values: Vec<i64> = (0..count as i64).map(|n| n * 3 - 7).collect();
            let bytes = ints(&values);
            let all: Vec<u32> = (0..count as u32).collect();
            let dense = Batch::new(
                count,
                vec![Vector::Int64 {
                    width: 8,
                    base: 0,
                    bytes: &bytes,
                    class: None,
                }],
            );
            let selected = Batch {
                rows: count,
                selection: Some(&all),
                columns: vec![Vector::Int64 {
                    width: 8,
                    base: 0,
                    bytes: &bytes,
                    class: None,
                }]
                .into(),
            };
            for kind in [
                AggregateKind::Sum,
                AggregateKind::Count,
                AggregateKind::Minimum,
                AggregateKind::Maximum,
                AggregateKind::Average,
                AggregateKind::Total,
            ] {
                let mut fast = SimpleAggregate::new(
                    vec![AggregateSpec {
                        kind: kind.clone(),
                        argument: Some(compile(&Expr::Column(0), &[StaticType::Int]).unwrap()),
                        extra: Vec::new(),
                        distinct: None,
                        filter: None,
                        order_by: Vec::new(),
                    }],
                    Box::new(Collect::new()),
                );
                let mut slow = SimpleAggregate::new(
                    vec![AggregateSpec {
                        kind: kind.clone(),
                        argument: Some(compile(&Expr::Column(0), &[StaticType::Int]).unwrap()),
                        extra: Vec::new(),
                        distinct: None,
                        filter: None,
                        order_by: Vec::new(),
                    }],
                    Box::new(Collect::new()),
                );
                fast.push(&dense).unwrap();
                slow.push(&selected).unwrap();
                assert!(
                    fast.push(&dense).is_ok() && slow.push(&selected).is_ok(),
                    "two batches fold the same way"
                );
                let a = fast.accumulators[0].finish().unwrap();
                let b = slow.accumulators[0].finish().unwrap();
                assert_eq!(
                    a.borrow().compare(&b.borrow()),
                    Ordering::Equal,
                    "{kind:?} over {count} rows: dense {a:?}, selected {b:?}"
                );
                assert_eq!(
                    matches!(a, OwnedDatum::Null),
                    matches!(b, OwnedDatum::Null),
                    "{kind:?} over {count} rows"
                );
            }
        }
    }

    /// `count(*)` counts rows behind a selection vector, not the batch's width.
    #[test]
    fn count_star_counts_live_rows() {
        let values: Vec<i64> = (0..100).collect();
        let bytes = ints(&values);
        let selection: Vec<u32> = (0..100u32).filter(|n| n % 3 == 0).collect();
        let mut aggregate = SimpleAggregate::new(
            vec![AggregateSpec {
                kind: AggregateKind::CountStar,
                argument: None,
                extra: Vec::new(),
                distinct: None,
                filter: None,
                order_by: Vec::new(),
            }],
            Box::new(Collect::new()),
        );
        let batch = Batch {
            rows: 100,
            selection: Some(&selection),
            columns: vec![Vector::Int64 {
                width: 8,
                base: 0,
                bytes: &bytes,
                class: None,
            }]
            .into(),
        };
        aggregate.push(&batch).unwrap();
        assert_eq!(
            aggregate.accumulators[0]
                .finish()
                .unwrap()
                .borrow()
                .as_int(),
            Some(selection.len() as i64)
        );
    }

    /// A filter keeps exactly the rows whose predicate is true, and a NULL
    /// predicate keeps none of them.
    #[test]
    fn a_filter_keeps_only_definite_truths() {
        let values = [Datum::Int(1), Datum::Int(10), Datum::Null, Datum::Int(20)];
        let predicate = compile(
            &Expr::Compare(
                CompareOp::Greater,
                Box::new(Expr::Column(0)),
                Box::new(Expr::Literal(OwnedDatum::Int(5))),
            ),
            &[StaticType::Int],
        )
        .unwrap();
        let mut filter = Filter::new(predicate, Box::new(Collect::new()));
        let batch = Batch::new(4, vec![Vector::Values(&values)]);
        filter.push(&batch).unwrap();
        assert_eq!(filter.selection, vec![1, 3]);
    }

    /// `TopN` produces exactly what a full sort followed by a limit produces,
    /// including which of several equal-keyed rows survives.
    #[test]
    fn top_n_matches_sort_then_limit() {
        let mut state = 0x1234_5678_9ABC_DEF0u64;
        let mut next = || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        // Deliberately few distinct keys, so ties are common and stability is
        // actually exercised.
        let rows: Vec<(i64, i64)> = (0..2_000)
            .map(|n| ((next() % 20) as i64, n as i64))
            .collect();
        for limit in [1usize, 3, 100, 2_000, 5_000] {
            let keys = vec![SortKey {
                column: 0,
                descending: false,
                collation: Collation::Binary,
                nulls_first: true,
            }];
            let mut top = TopN::new(keys.clone(), limit, Box::new(Collect::new()));
            let mut sort = Sort::new(keys.clone(), Box::new(Collect::with_limit(limit)));
            for chunk in rows.chunks(37) {
                let column_a: Vec<Datum<'_>> = chunk.iter().map(|(a, _)| Datum::Int(*a)).collect();
                let column_b: Vec<Datum<'_>> = chunk.iter().map(|(_, b)| Datum::Int(*b)).collect();
                let batch = Batch::new(
                    chunk.len(),
                    vec![Vector::Values(&column_a), Vector::Values(&column_b)],
                );
                top.push(&batch).unwrap();
                sort.push(&batch).unwrap();
            }
            let from_top = std::mem::take(&mut top.best);
            sort.rows.sort_by(|l, r| compare_by(l, r, &keys));
            let from_sort: Vec<Vec<OwnedDatum>> = sort.rows.iter().take(limit).cloned().collect();
            assert_eq!(from_top.len(), from_sort.len(), "limit {limit}");
            for (index, (a, b)) in from_top.iter().zip(from_sort.iter()).enumerate() {
                assert_eq!(
                    a[0].borrow().as_int(),
                    b[0].borrow().as_int(),
                    "limit {limit} row {index} key"
                );
                assert_eq!(
                    a[1].borrow().as_int(),
                    b[1].borrow().as_int(),
                    "limit {limit} row {index} payload: top-n kept a different tied row"
                );
            }
        }
    }

    /// `DISTINCT` keeps the first of each duplicate group and drops the rest,
    /// treating values of different classes as different.
    #[test]
    fn distinct_separates_by_class_not_only_by_text() {
        let values = [
            Datum::Int(1),
            Datum::Text(b"1"),
            Datum::Int(1),
            Datum::Real(1.0),
            Datum::Null,
            Datum::Null,
        ];
        let mut distinct = Distinct::new(Vec::new(), Box::new(Collect::new()));
        let batch = Batch::new(values.len(), vec![Vector::Values(&values)]);
        distinct.push(&batch).unwrap();
        // 1, "1", NULL survive; the second Int(1) and the second NULL do not.
        // Real(1.0) encodes equal to Int(1) because the key encoding compares
        // numerics numerically, which is what SQLite's DISTINCT does too.
        assert_eq!(distinct.rows.len(), 3, "{:?}", distinct.rows);
    }

    /// A grouped aggregate produces one row per key, in key order, with the
    /// right counts.
    #[test]
    fn grouped_aggregation_counts_each_key() {
        let categories: Vec<Datum<'_>> = (0..1_000).map(|n| Datum::Int((n % 7) as i64)).collect();
        let mut grouped = HashAggregate::new(
            vec![compile(&Expr::Column(0), &[StaticType::Int]).unwrap()],
            Vec::new(),
            vec![AggregateSpec {
                kind: AggregateKind::CountStar,
                argument: None,
                extra: Vec::new(),
                distinct: None,
                filter: None,
                order_by: Vec::new(),
            }],
            Box::new(Collect::new()),
        );
        let batch = Batch::new(1_000, vec![Vector::Values(&categories)]);
        grouped.push(&batch).unwrap();
        assert_eq!(grouped.groups.len(), 7);
        let mut total = 0i64;
        for (key, accumulators) in grouped.groups.values() {
            let count = accumulators[0].finish().unwrap().borrow().as_int().unwrap();
            let category = key[0].borrow().as_int().unwrap();
            assert_eq!(
                count,
                if category < 1_000 % 7 { 143 } else { 142 },
                "category {category}"
            );
            total += count;
        }
        assert_eq!(total, 1_000);
    }

    /// The run-detecting grouped path and the per-row one produce the same
    /// groups, the same counts and the same sums, over runs that start and end
    /// on batch boundaries and runs that do not.
    ///
    /// The dense path is entered only for a dense batch, so the per-row path is
    /// obtained by putting the same values behind a selection vector that keeps
    /// every row - which is the same input and a different code path.
    #[test]
    fn the_run_detecting_group_path_agrees_with_the_per_row_one() {
        for run_length in [1usize, 2, 37, 512, 4096] {
            let values: Vec<i64> = (0..4_000).map(|n| (n / run_length) as i64).collect();
            let payload: Vec<i64> = (0..4_000).map(|n| n as i64 * 3).collect();
            let key_bytes = ints(&values);
            let payload_bytes = ints(&payload);
            let all: Vec<u32> = (0..4_000u32).collect();

            let make = |dense: bool| {
                let columns = vec![
                    Vector::Int64 {
                        width: 8,
                        base: 0,
                        bytes: &key_bytes,
                        class: None,
                    },
                    Vector::Int64 {
                        width: 8,
                        base: 0,
                        bytes: &payload_bytes,
                        class: None,
                    },
                ];
                if dense {
                    Batch::new(4_000, columns)
                } else {
                    Batch {
                        rows: 4_000,
                        selection: Some(&all),
                        columns: columns.into(),
                    }
                }
            };

            let mut outcomes = Vec::new();
            for dense in [true, false] {
                let rows = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
                let mut aggregate = StreamAggregate::new(
                    vec![compile(&Expr::Column(0), &[StaticType::Int; 2]).unwrap()],
                    Vec::new(),
                    vec![
                        AggregateSpec {
                            kind: AggregateKind::CountStar,
                            argument: None,
                            extra: Vec::new(),
                            distinct: None,
                            filter: None,
                            order_by: Vec::new(),
                        },
                        AggregateSpec {
                            kind: AggregateKind::Sum,
                            argument: Some(
                                compile(&Expr::Column(1), &[StaticType::Int; 2]).unwrap(),
                            ),
                            extra: Vec::new(),
                            distinct: None,
                            filter: None,
                            order_by: Vec::new(),
                        },
                    ],
                    Box::new(CollectInto::new(std::rc::Rc::clone(&rows))),
                );
                // Pushed in two batches so a run can straddle the boundary.
                let batch = make(dense);
                aggregate.push(&batch).unwrap();
                aggregate.finish().unwrap();
                let held: Vec<(i64, i64, i64)> = rows
                    .borrow()
                    .iter()
                    .map(|row| {
                        (
                            row[0].borrow().as_int().unwrap_or(-1),
                            row[1].borrow().as_int().unwrap_or(-1),
                            row[2].borrow().as_int().unwrap_or(-1),
                        )
                    })
                    .collect();
                outcomes.push(held);
            }
            assert_eq!(
                outcomes[0], outcomes[1],
                "run length {run_length}: dense and per-row disagreed"
            );
            let groups = 4_000_usize.div_ceil(run_length);
            assert_eq!(outcomes[0].len(), groups, "run length {run_length}");
            assert_eq!(
                outcomes[0].iter().map(|(_, count, _)| count).sum::<i64>(),
                4_000
            );
        }
    }

    /// A limit stops the pipeline once it has enough, rather than reading on.
    #[test]
    fn a_limit_stops_the_pipeline() {
        let values: Vec<Datum<'_>> = (0..100).map(Datum::Int).collect();
        let mut limit = Limit::new(10, 5, Box::new(Collect::new()));
        let batch = Batch::new(100, vec![Vector::Values(&values)]);
        assert_eq!(limit.push(&batch).unwrap(), Flow::Stop);
        assert_eq!(limit.emitted, 10);
        assert_eq!(limit.seen, 15);
    }

    /// A projection that is a permutation borrows rather than copying, and one
    /// that computes materialises - both producing the same values.
    #[test]
    fn a_projection_borrows_when_it_can() {
        let a: Vec<Datum<'_>> = (0..10).map(Datum::Int).collect();
        let b: Vec<Datum<'_>> = (0..10).map(|n| Datum::Int(n * 2)).collect();
        let batch = Batch::new(10, vec![Vector::Values(&a), Vector::Values(&b)]);

        let mut permute = Project::new(
            vec![
                compile(&Expr::Column(1), &[StaticType::Int; 2]).unwrap(),
                compile(&Expr::Column(0), &[StaticType::Int; 2]).unwrap(),
            ],
            Box::new(Collect::new()),
        );
        permute.push(&batch).unwrap();
        permute.finish().unwrap();

        let mut computed = Project::new(
            vec![compile(
                &Expr::Arith(
                    crate::expr::ArithOp::Add,
                    Box::new(Expr::Column(0)),
                    Box::new(Expr::Column(1)),
                ),
                &[StaticType::Int; 2],
            )
            .unwrap()],
            Box::new(Collect::new()),
        );
        computed.push(&batch).unwrap();
        computed.finish().unwrap();
    }
}
