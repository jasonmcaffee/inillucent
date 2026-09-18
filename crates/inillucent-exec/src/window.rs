//! The vectorised executor's window operator.
//!
//! Invariant: this operator computes *values*, never frames. Which rows a
//! frame contains comes from [`inillucent_scalar::window`], which the bytecode VM
//! calls too - `EXCLUDE TIES` differing from `EXCLUDE GROUP` by one row is the
//! kind of rule that must have exactly one implementation in the workspace.
//! What is here is everything that needs a row's values: the comparisons that
//! decide peer groups, the arguments read out of their columns, and the
//! accumulator, which is operator state rather than a function.
//!
//! ## Why it is a pipeline breaker, and addressed by column number
//!
//! A window reads its whole partition in both directions - `last_value` looks
//! to the end of the frame, `EXCLUDE TIES` looks ahead an unbounded distance
//! inside a peer group - so there is no streaming form. The input arrives
//! already sorted by the partition keys and then by the window's own
//! `ORDER BY`, and this operator buffers it.
//!
//! Nothing here is an expression. Every value a call needs - its arguments, its
//! `FILTER`, its frame offsets, its ordering terms - is a column of the
//! buffered row, put there by the projection upstream. That is the same shape
//! the VM's window plan has, and it is what keeps the operator small enough to
//! read: the hard part is the frame arithmetic, not the plumbing.

use std::collections::HashSet;

use inillucent_base::DbResult;
use inillucent_scalar::window as frames;
use inillucent_sql::ast::{FrameExclude, FrameUnit};
use inillucent_sql::function::{AggregateFunc, WindowFunc};
use inillucent_tree::datum::{Datum, OwnedDatum};
use inillucent_tree::key;
use inillucent_tree::types::compare_under;
use inillucent_value::collation::Collation;
use inillucent_value::Value;

use crate::aggregate::{Accumulator, AggregateKind};
use crate::batch::Batch;
use crate::join::RowStore;
use crate::ops::{emit_rows, Flow, Sink};

/// Which family a window call belongs to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WindowSlot {
    /// An aggregate computed over the frame.
    Aggregate(AggregateKind),
    /// One of the eleven functions that only exist in a window.
    Plain(WindowFunc),
}

/// One end of a frame as the plan writes it, before the offset is read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FrameEnd {
    /// `UNBOUNDED PRECEDING`.
    UnboundedPreceding,
    /// `CURRENT ROW`.
    CurrentRow,
    /// `UNBOUNDED FOLLOWING`.
    UnboundedFollowing,
    /// `expr PRECEDING` or `expr FOLLOWING`, the offset in a buffered column.
    Offset {
        /// The column holding the offset, evaluated once per row.
        column: usize,
        /// Whether it counts backwards.
        preceding: bool,
    },
}

/// A frame specification, with its offsets still in their columns.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WindowFrame {
    /// `ROWS`, `RANGE` or `GROUPS`.
    pub unit: FrameUnit,
    /// The start.
    pub start: FrameEnd,
    /// The end.
    pub end: FrameEnd,
    /// The `EXCLUDE` clause.
    pub exclude: FrameExclude,
}

impl Default for WindowFrame {
    /// The frame a window with an `ORDER BY` gets when none was written:
    /// `RANGE BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW`.
    fn default() -> WindowFrame {
        WindowFrame {
            unit: FrameUnit::Range,
            start: FrameEnd::UnboundedPreceding,
            end: FrameEnd::CurrentRow,
            exclude: FrameExclude::NoOthers,
        }
    }
}

/// One ordering term of a window's own `ORDER BY`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OrderTerm {
    /// The buffered column holding the value.
    pub column: usize,
    /// Whether it orders downwards.
    pub descending: bool,
    /// The collation its text compares under.
    pub collation: Collation,
}

/// One window call, addressed entirely by buffered column numbers.
#[derive(Clone, Debug, PartialEq)]
pub struct WindowCall {
    /// What it computes.
    pub func: WindowSlot,
    /// Whether `DISTINCT` was written.
    pub distinct: bool,
    /// The collation its comparisons use.
    pub collation: Collation,
    /// The columns holding its arguments.
    pub arguments: Vec<usize>,
    /// The column holding its `FILTER (WHERE ...)` value.
    pub filter: Option<usize>,
    /// The window's own `ORDER BY`.
    pub order: Vec<OrderTerm>,
    /// The frame.
    pub frame: WindowFrame,
}

/// Everything one window pass needs.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct WindowPlan {
    /// The columns holding the partition keys, with their collations.
    pub partition: Vec<(usize, Collation)>,
    /// The calls, in the order their values are appended to each row.
    pub calls: Vec<WindowCall>,
}

/// Buffers its sorted input and appends one column per window call.
///
/// Each row grows by one value per call, in the order the calls were bound, so
/// whatever reads the output finds them at fixed column numbers.
pub struct Window {
    plan: WindowPlan,
    store: RowStore,
    downstream: Box<dyn Sink>,
}

impl Window {
    /// Returns a window operator.
    ///
    /// @param plan - the partition keys and the calls
    /// @param downstream - what to push the widened rows into
    pub fn new(plan: WindowPlan, downstream: Box<dyn Sink>) -> Window {
        Window {
            plan,
            store: RowStore::new(),
            downstream,
        }
    }
}

impl Sink for Window {
    fn push(&mut self, batch: &Batch<'_>) -> DbResult<Flow> {
        self.store.absorb(batch)?;
        Ok(Flow::Continue)
    }

    fn finish(&mut self) -> DbResult<()> {
        let rows = std::mem::take(&mut self.store).into_rows();
        let widened = compute(&rows, &self.plan)?;
        emit_rows(&widened, self.downstream.as_mut())?;
        self.downstream.finish()
    }

    /// Returns this operator and everything below it to its pre-input state.
    fn reset(&mut self) -> DbResult<()> {
        self.store.clear();
        self.downstream.reset()
    }
}

/// Computes every window value for every row and appends them.
///
/// @param rows - the buffered input, already sorted
/// @param plan - the partition keys and the calls
pub fn compute(rows: &[Vec<OwnedDatum>], plan: &WindowPlan) -> DbResult<Vec<Vec<OwnedDatum>>> {
    let total = rows.len();
    let mut extra: Vec<Vec<OwnedDatum>> = vec![Vec::with_capacity(plan.calls.len()); total];
    for call in &plan.calls {
        // Every call shares the partition boundaries but brings its own
        // `ORDER BY`, so the peer groups are its own.
        let partitions = frames::partitions(
            total,
            |left, right| same_partition(rows, &plan.partition, left, right),
            |left, right| same_order(rows, call, left, right),
            !call.order.is_empty(),
        );
        for partition in &partitions {
            for row in partition.start..partition.end {
                let value = evaluate(rows, call, partition, row)?;
                if let Some(slot) = extra.get_mut(row) {
                    slot.push(value);
                }
            }
        }
    }
    Ok(rows
        .iter()
        .zip(extra)
        .map(|(row, appended)| {
            let mut whole = row.clone();
            whole.extend(appended);
            whole
        })
        .collect())
}

/// Returns one value of one row, or NULL when the column is not there.
fn value_at<'r>(rows: &'r [Vec<OwnedDatum>], row: usize, column: usize) -> Datum<'r> {
    rows.get(row)
        .and_then(|values| values.get(column))
        .map(OwnedDatum::borrow)
        .unwrap_or(Datum::Null)
}

/// Returns whether two values are the same for peer purposes.
///
/// Two NULLs are peers. They are not equal, but an `ORDER BY` cannot separate
/// them, and a peer group is defined by what the ordering can distinguish.
fn peers(left: &Datum<'_>, right: &Datum<'_>, collation: Collation) -> bool {
    if matches!(left, Datum::Null) || matches!(right, Datum::Null) {
        return matches!(left, Datum::Null) && matches!(right, Datum::Null);
    }
    compare_under(left, right, collation) == std::cmp::Ordering::Equal
}

/// Returns whether two rows share every partition key.
fn same_partition(
    rows: &[Vec<OwnedDatum>],
    key: &[(usize, Collation)],
    left: usize,
    right: usize,
) -> bool {
    key.iter().all(|(column, collation)| {
        peers(
            &value_at(rows, left, *column),
            &value_at(rows, right, *column),
            *collation,
        )
    })
}

/// Returns whether the window's `ORDER BY` can tell two rows apart.
fn same_order(rows: &[Vec<OwnedDatum>], call: &WindowCall, left: usize, right: usize) -> bool {
    call.order.iter().all(|term| {
        peers(
            &value_at(rows, left, term.column),
            &value_at(rows, right, term.column),
            term.collation,
        )
    })
}

/// Returns whether a row passes the call's `FILTER (WHERE ...)`.
fn passes_filter(rows: &[Vec<OwnedDatum>], call: &WindowCall, row: usize) -> bool {
    let Some(column) = call.filter else {
        return true;
    };
    match value_at(rows, row, column) {
        Datum::Null => false,
        Datum::Int(value) => value != 0,
        Datum::Real(value) => value != 0.0,
        // A `FILTER` value the projection left as text or a blob is falsy the
        // way SQLite's truth test makes it falsy, via a numeric reading.
        other => integer_of(&other) != 0,
    }
}

/// Returns a value's integer reading, the way a truth test or an offset needs it.
fn integer_of(value: &Datum<'_>) -> i64 {
    match value {
        Datum::Int(held) => *held,
        Datum::Real(held) => *held as i64,
        Datum::Null => 0,
        other => inillucent_value::cast::integer_value(&Value::from(other)),
    }
}

/// Returns a value's double reading, for a `RANGE` offset.
fn real_of(value: &Datum<'_>) -> f64 {
    match value {
        Datum::Int(held) => *held as f64,
        Datum::Real(held) => *held,
        Datum::Null => 0.0,
        other => inillucent_value::cast::real_value(&Value::from(other)),
    }
}

/// Returns an ordering value's double reading, or `None` when it is NULL.
///
/// **NULL is not a number and a `RANGE` offset cannot measure a distance to
/// it (task-1913).** Reading it as `0.0` through `real_of` made every NULL row
/// sit one unit from zero, so `RANGE BETWEEN 1 PRECEDING AND 1 FOLLOWING` drew
/// the NULL rows into the frame of every row near zero and drew the numbers
/// beside them into the NULL rows' own frames. `frames::range_bound` takes the
/// `None` and resolves such a row to its peer group instead, which is what
/// SQLite answers.
///
/// @param value - the row's ordering value
fn real_or_null(value: &Datum<'_>) -> Option<f64> {
    match value {
        Datum::Null => None,
        other => Some(real_of(other)),
    }
}

/// Computes one window call for one row.
///
/// @param rows - the buffered input
/// @param call - the call
/// @param partition - the row's partition, with its peer groups
/// @param row - the row
fn evaluate(
    rows: &[Vec<OwnedDatum>],
    call: &WindowCall,
    partition: &frames::Partition,
    row: usize,
) -> DbResult<OwnedDatum> {
    Ok(match &call.func {
        WindowSlot::Plain(WindowFunc::RowNumber) => {
            OwnedDatum::Int(frames::row_number(partition, row))
        }
        WindowSlot::Plain(WindowFunc::Rank) => OwnedDatum::Int(frames::rank(partition, row)),
        WindowSlot::Plain(WindowFunc::DenseRank) => {
            OwnedDatum::Int(frames::dense_rank(partition, row))
        }
        WindowSlot::Plain(WindowFunc::PercentRank) => {
            OwnedDatum::Real(frames::percent_rank(partition, row))
        }
        WindowSlot::Plain(WindowFunc::CumeDist) => {
            OwnedDatum::Real(frames::cume_dist(partition, row))
        }
        WindowSlot::Plain(WindowFunc::Ntile) => ntile(rows, call, partition, row)?,
        WindowSlot::Plain(WindowFunc::Lag) => offset_row(rows, call, partition, row, true),
        WindowSlot::Plain(WindowFunc::Lead) => offset_row(rows, call, partition, row, false),
        WindowSlot::Plain(WindowFunc::FirstValue)
        | WindowSlot::Plain(WindowFunc::LastValue)
        | WindowSlot::Plain(WindowFunc::NthValue) => {
            let frame = frame_of(rows, call, partition, row);
            positional(rows, call, &frame, row)
        }
        WindowSlot::Aggregate(kind) => aggregate(rows, call, partition, row, kind.clone())?,
    })
}

/// Computes `ntile(n)`.
///
/// **A bucket count of zero or less is an error, not a NULL (task-1979,
/// F17).** `frames::ntile` answers `None` for a count it cannot divide a
/// partition into, and this read that `None` as "no answer for this row" and
/// returned NULL for every row of the query. SQLite refuses the statement with
/// `argument of ntile must be a positive integer`, so a caller that passed a
/// count it computed hears about it rather than reading a column of NULLs as
/// data.
///
/// @param rows - the buffered input
/// @param call - the call, whose first argument is the bucket count
/// @param partition - the row's partition, with its peer groups
/// @param row - the row
fn ntile(
    rows: &[Vec<OwnedDatum>],
    call: &WindowCall,
    partition: &frames::Partition,
    row: usize,
) -> DbResult<OwnedDatum> {
    let Some(column) = call.arguments.first() else {
        return Ok(OwnedDatum::Null);
    };
    let buckets = integer_of(&value_at(rows, row, *column));
    match frames::ntile(partition, row, buckets) {
        Some(bucket) => Ok(OwnedDatum::Int(bucket)),
        None => Err(inillucent_base::error::statement_refusal(
            "argument of ntile must be a positive integer",
        )),
    }
}

/// Computes `lag` or `lead`, which read the partition and ignore the frame.
fn offset_row(
    rows: &[Vec<OwnedDatum>],
    call: &WindowCall,
    partition: &frames::Partition,
    row: usize,
    backwards: bool,
) -> OwnedDatum {
    let Some(value_column) = call.arguments.first() else {
        return OwnedDatum::Null;
    };
    let offset = match call.arguments.get(1) {
        Some(column) => integer_of(&value_at(rows, row, *column)),
        None => 1,
    };
    match frames::offset_row(partition, row, offset, backwards) {
        Some(target) => OwnedDatum::from_datum(&value_at(rows, target, *value_column)),
        // Off the end of the partition, so the default if one was written.
        None => match call.arguments.get(2) {
            Some(column) => OwnedDatum::from_datum(&value_at(rows, row, *column)),
            None => OwnedDatum::Null,
        },
    }
}

/// Computes `first_value`, `last_value` or `nth_value` over a frame.
fn positional(
    rows: &[Vec<OwnedDatum>],
    call: &WindowCall,
    frame: &[usize],
    row: usize,
) -> OwnedDatum {
    let Some(value_column) = call.arguments.first() else {
        return OwnedDatum::Null;
    };
    let picked = match &call.func {
        WindowSlot::Plain(WindowFunc::FirstValue) => frame.first().copied(),
        WindowSlot::Plain(WindowFunc::LastValue) => frame.last().copied(),
        WindowSlot::Plain(WindowFunc::NthValue) => {
            let Some(column) = call.arguments.get(1) else {
                return OwnedDatum::Null;
            };
            let nth = integer_of(&value_at(rows, row, *column));
            if nth < 1 {
                return OwnedDatum::Null;
            }
            frame.get((nth as usize).saturating_sub(1)).copied()
        }
        _ => None,
    };
    match picked {
        Some(member) => OwnedDatum::from_datum(&value_at(rows, member, *value_column)),
        None => OwnedDatum::Null,
    }
}

/// Runs an aggregate over one row's frame.
///
/// `DISTINCT` is applied here rather than inside the accumulator, because a
/// window aggregate's distinctness is per frame: the same value can be counted
/// once in this row's frame and once again in the next row's.
fn aggregate(
    rows: &[Vec<OwnedDatum>],
    call: &WindowCall,
    partition: &frames::Partition,
    row: usize,
    kind: AggregateKind,
) -> DbResult<OwnedDatum> {
    let frame = frame_of(rows, call, partition, row);
    let mut accumulator = Accumulator::new(kind);
    let mut seen: HashSet<Vec<u8>> = HashSet::new();
    for member in frame {
        if !passes_filter(rows, call, member) {
            continue;
        }
        let value = match call.arguments.first() {
            Some(column) => value_at(rows, member, *column),
            // `count(*)` has no argument and counts the row itself.
            None => Datum::Int(1),
        };
        if call.distinct {
            let mut encoded = Vec::new();
            key::encode_into_with(&value, call.collation, &mut encoded);
            if !seen.insert(encoded) {
                continue;
            }
        }
        accumulator.push(&value);
    }
    accumulator.finish()
}

/// Returns the rows of one row's frame, in partition order.
///
/// Reads the offset expressions out of their columns and hands the resolved
/// frame to the shared arithmetic.
fn frame_of(
    rows: &[Vec<OwnedDatum>],
    call: &WindowCall,
    partition: &frames::Partition,
    row: usize,
) -> Vec<usize> {
    let spec = frames::FrameSpec {
        unit: call.frame.unit,
        start: resolve(rows, call.frame.start, row),
        end: resolve(rows, call.frame.end, row),
        exclude: call.frame.exclude,
    };
    let first = call.order.first();
    let order_column = first.map(|term| term.column);
    let descending = first.is_some_and(|term| term.descending);
    frames::frame(
        partition,
        row,
        &spec,
        |member| match order_column {
            Some(column) => real_or_null(&value_at(rows, member, column)),
            // No ordering term at all, so no `RANGE` offset can be resolved
            // against one. Every row reads alike, which is what the frame
            // arithmetic did before there was a NULL to tell apart.
            None => Some(0.0),
        },
        descending,
    )
}

/// Reads one end of a frame into the shared form, evaluating its offset.
fn resolve(rows: &[Vec<OwnedDatum>], bound: FrameEnd, row: usize) -> frames::Bound {
    match bound {
        FrameEnd::UnboundedPreceding => frames::Bound::UnboundedPreceding,
        FrameEnd::CurrentRow => frames::Bound::CurrentRow,
        FrameEnd::UnboundedFollowing => frames::Bound::UnboundedFollowing,
        FrameEnd::Offset { column, preceding } => frames::Bound::Offset {
            distance: integer_of(&value_at(rows, row, column)),
            preceding,
        },
    }
}

/// Returns the aggregate kind a bound aggregate function maps to.
///
/// The window binder names its aggregates with `inillucent_sql`'s enum and the
/// executor's accumulator takes its own, because the executor's carries the
/// separator `group_concat` was written with.
///
/// @param func - the bound function
/// @param separator - the separator, for `group_concat`
pub fn kind_of(func: AggregateFunc, separator: &str) -> Option<AggregateKind> {
    Some(match func {
        AggregateFunc::Count => AggregateKind::Count,
        AggregateFunc::Sum => AggregateKind::Sum,
        AggregateFunc::Total => AggregateKind::Total,
        AggregateFunc::Avg => AggregateKind::Average,
        AggregateFunc::Min => AggregateKind::Minimum,
        AggregateFunc::Max => AggregateKind::Maximum,
        AggregateFunc::GroupConcat => AggregateKind::GroupConcat(separator.to_string()),
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A plain window call over one partition with one ordering term.
    ///
    /// @param func - what it computes
    /// @param arguments - the columns its arguments are in
    fn call(func: WindowSlot, arguments: Vec<usize>) -> WindowCall {
        WindowCall {
            func,
            distinct: false,
            collation: Collation::Binary,
            arguments,
            filter: None,
            order: vec![OrderTerm {
                column: 0,
                descending: false,
                collation: Collation::Binary,
            }],
            frame: WindowFrame::default(),
        }
    }

    /// Builds rows of `(order key, payload)`.
    ///
    /// @param keys - the ordering key of each row
    fn rows_of(keys: &[i64]) -> Vec<Vec<OwnedDatum>> {
        keys.iter()
            .enumerate()
            .map(|(nth, key)| vec![OwnedDatum::Int(*key), OwnedDatum::Int(nth as i64 * 10)])
            .collect()
    }

    /// Runs one call and returns the appended column.
    ///
    /// @param rows - the buffered input
    /// @param plan - the plan
    fn appended(rows: &[Vec<OwnedDatum>], plan: &WindowPlan) -> Vec<OwnedDatum> {
        let widened = compute(rows, plan).expect("the pass succeeds");
        widened
            .iter()
            .map(|row| row.last().cloned().unwrap_or(OwnedDatum::Null))
            .collect()
    }

    /// Reads a column of integers out of an answer.
    fn ints(values: &[OwnedDatum]) -> Vec<i64> {
        values
            .iter()
            .map(|value| match value {
                OwnedDatum::Int(held) => *held,
                _ => i64::MIN,
            })
            .collect()
    }

    #[test]
    fn row_number_and_rank_differ_exactly_where_there_are_ties() {
        let rows = rows_of(&[1, 1, 2]);
        let numbers = WindowPlan {
            partition: Vec::new(),
            calls: vec![call(WindowSlot::Plain(WindowFunc::RowNumber), Vec::new())],
        };
        assert_eq!(ints(&appended(&rows, &numbers)), vec![1, 2, 3]);
        let ranks = WindowPlan {
            partition: Vec::new(),
            calls: vec![call(WindowSlot::Plain(WindowFunc::Rank), Vec::new())],
        };
        assert_eq!(ints(&appended(&rows, &ranks)), vec![1, 1, 3]);
    }

    #[test]
    fn a_running_sum_uses_the_default_range_frame() {
        let rows = rows_of(&[1, 1, 2]);
        let plan = WindowPlan {
            partition: Vec::new(),
            calls: vec![call(WindowSlot::Aggregate(AggregateKind::Sum), vec![1])],
        };
        // Rows 0 and 1 are peers, so the default RANGE frame gives both the
        // whole peer group: 0 + 10 = 10. Under ROWS row 0 would have seen 0.
        assert_eq!(ints(&appended(&rows, &plan)), vec![10, 10, 30]);
    }

    #[test]
    fn a_rows_frame_slides() {
        let rows = rows_of(&[1, 2, 3]);
        let mut spec = call(WindowSlot::Aggregate(AggregateKind::Sum), vec![1]);
        spec.frame = WindowFrame {
            unit: FrameUnit::Rows,
            start: FrameEnd::UnboundedPreceding,
            end: FrameEnd::CurrentRow,
            exclude: FrameExclude::NoOthers,
        };
        let plan = WindowPlan {
            partition: Vec::new(),
            calls: vec![spec],
        };
        assert_eq!(ints(&appended(&rows, &plan)), vec![0, 10, 30]);
    }

    #[test]
    fn a_partition_key_stops_the_window_reaching_across() {
        let rows = vec![
            vec![OwnedDatum::Int(1), OwnedDatum::Int(5), OwnedDatum::Int(1)],
            vec![OwnedDatum::Int(2), OwnedDatum::Int(5), OwnedDatum::Int(1)],
            vec![OwnedDatum::Int(3), OwnedDatum::Int(9), OwnedDatum::Int(1)],
        ];
        let mut spec = call(WindowSlot::Plain(WindowFunc::RowNumber), Vec::new());
        spec.order = vec![OrderTerm {
            column: 0,
            descending: false,
            collation: Collation::Binary,
        }];
        let plan = WindowPlan {
            // Column 1 is the partition key: rows 0 and 1 share it.
            partition: vec![(1, Collation::Binary)],
            calls: vec![spec],
        };
        assert_eq!(ints(&appended(&rows, &plan)), vec![1, 2, 1]);
    }

    #[test]
    fn lag_falls_back_to_its_default_at_the_partition_edge() {
        let rows = rows_of(&[1, 2, 3]);
        let mut spec = call(WindowSlot::Plain(WindowFunc::Lag), vec![1]);
        spec.arguments = vec![1];
        let plan = WindowPlan {
            partition: Vec::new(),
            calls: vec![spec],
        };
        let answer = appended(&rows, &plan);
        assert!(matches!(answer.first(), Some(OwnedDatum::Null)));
        assert_eq!(ints(&answer[1..]), vec![0, 10]);
    }

    #[test]
    fn a_filter_drops_rows_from_the_frame_and_not_from_the_output() {
        let rows = rows_of(&[1, 2, 3]);
        let mut spec = call(WindowSlot::Aggregate(AggregateKind::CountStar), Vec::new());
        spec.frame = WindowFrame {
            unit: FrameUnit::Rows,
            start: FrameEnd::UnboundedPreceding,
            end: FrameEnd::UnboundedFollowing,
            exclude: FrameExclude::NoOthers,
        };
        // The payload column is 0, 10, 20; filtering on it drops only row 0.
        spec.filter = Some(1);
        let plan = WindowPlan {
            partition: Vec::new(),
            calls: vec![spec],
        };
        let answer = appended(&rows, &plan);
        assert_eq!(answer.len(), 3, "every input row still produces a row");
        assert_eq!(ints(&answer), vec![2, 2, 2]);
    }

    #[test]
    fn distinct_is_per_frame_rather_than_per_partition() {
        let rows = vec![
            vec![OwnedDatum::Int(1), OwnedDatum::Int(7)],
            vec![OwnedDatum::Int(2), OwnedDatum::Int(7)],
            vec![OwnedDatum::Int(3), OwnedDatum::Int(8)],
        ];
        let mut spec = call(WindowSlot::Aggregate(AggregateKind::Count), vec![1]);
        spec.distinct = true;
        spec.frame = WindowFrame {
            unit: FrameUnit::Rows,
            start: FrameEnd::UnboundedPreceding,
            end: FrameEnd::CurrentRow,
            exclude: FrameExclude::NoOthers,
        };
        let plan = WindowPlan {
            partition: Vec::new(),
            calls: vec![spec],
        };
        // The 7 repeats, so the second frame still has one distinct value.
        assert_eq!(ints(&appended(&rows, &plan)), vec![1, 1, 2]);
    }

    #[test]
    fn two_calls_append_in_the_order_they_were_bound() {
        let rows = rows_of(&[1, 2]);
        let plan = WindowPlan {
            partition: Vec::new(),
            calls: vec![
                call(WindowSlot::Plain(WindowFunc::RowNumber), Vec::new()),
                call(WindowSlot::Aggregate(AggregateKind::Sum), vec![1]),
            ],
        };
        let widened = compute(&rows, &plan).expect("the pass succeeds");
        assert_eq!(widened[1].len(), 4, "two input columns plus two calls");
        assert_eq!(ints(&widened[1][2..]), vec![2, 10]);
    }

    #[test]
    fn an_empty_input_produces_no_rows_rather_than_a_panic() {
        let plan = WindowPlan {
            partition: Vec::new(),
            calls: vec![call(WindowSlot::Plain(WindowFunc::Rank), Vec::new())],
        };
        assert!(compute(&[], &plan).expect("the pass succeeds").is_empty());
    }

    #[test]
    fn nulls_are_peers_of_each_other_and_of_nothing_else() {
        let rows = vec![
            vec![OwnedDatum::Null, OwnedDatum::Int(0)],
            vec![OwnedDatum::Null, OwnedDatum::Int(10)],
            vec![OwnedDatum::Int(1), OwnedDatum::Int(20)],
        ];
        let plan = WindowPlan {
            partition: Vec::new(),
            calls: vec![call(WindowSlot::Plain(WindowFunc::Rank), Vec::new())],
        };
        assert_eq!(ints(&appended(&rows, &plan)), vec![1, 1, 3]);
    }

    #[test]
    fn the_aggregate_names_map_onto_the_executors_kinds() {
        assert_eq!(kind_of(AggregateFunc::Sum, ""), Some(AggregateKind::Sum));
        assert_eq!(
            kind_of(AggregateFunc::GroupConcat, "-"),
            Some(AggregateKind::GroupConcat("-".to_string()))
        );
    }
}
