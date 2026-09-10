//! Window functions as the machine runs them: the eleven built-ins and the
//! aggregates, over the frames `inillucent_scalar::window` works out.
//!
//! Invariant: a window value is computed from the *frame*, and the frame is
//! computed from the partition and the peer groups, in that order.
//!
//! That arithmetic no longer lives here. It moved to
//! [`inillucent_scalar::window`] when the vectorised executor needed the same
//! rules: which rows a frame contains is a pure function of two row counts and
//! a handful of booleans, and two copies of it would agree the day they were
//! written and diverge at the first fix. What stays here is everything that
//! needs a `Value` - the comparisons that decide peer groups, the offset
//! expressions read out of record columns, and the accumulator, which is
//! operator state rather than a function.
//!
//! This is one operator rather than a sequence of opcodes, and that is a
//! deliberate trade. The frame arithmetic needs random access to the partition
//! in both directions - `EXCLUDE TIES` has to look ahead an unbounded distance
//! within a peer group - and expressing that in a register machine costs three
//! cursors and a page of jump patching per function. As one operator it is a
//! hundred lines of ordinary Rust that the verifier still checks the operands
//! of, and the machine still runs under the same interrupt and limit rules.

use inillucent_scalar::window as frames;
use inillucent_value::{compare, Collation, TextEncoding, Value};

use crate::aggregate::Accumulator;
use crate::ephemeral::Ephemeral;
use crate::program::{WindowCall, WindowPlan};
use inillucent_base::DbResult;
use inillucent_sql::function::WindowFunc;

/// Computes every window value for every row of a sorted store.
///
/// The store holds one row per input row, already ordered by the partition keys
/// and then the window's own `ORDER BY`. Each row grows by one value per window
/// call, appended in the order the calls were bound, so the drain that follows
/// reads them by a fixed column number.
///
/// @param store - the sorted rows, replaced by the widened ones
/// @param plan - the partition keys and the calls
/// @param encoding - the text encoding the accumulators work in
pub fn compute(store: &mut Ephemeral, plan: &WindowPlan, encoding: TextEncoding) -> DbResult<()> {
    let rows = store.take_rows();
    let total = rows.len();
    let mut values: Vec<Vec<Value<'static>>> = rows.iter().map(|_| Vec::new()).collect();
    for call in &plan.calls {
        // Each call brings its own `ORDER BY`, so the peer groups are its own
        // even though every call shares the partition boundaries.
        let partitions = frames::partitions(
            total,
            |left, right| same_key(&rows, &plan.partition, left, right),
            |left, right| same_order_key(&rows, call, left, right),
            !call.order.is_empty(),
        );
        for partition in &partitions {
            for row in partition.start..partition.end {
                let value = evaluate(&rows, call, partition, row, encoding)?;
                if let Some(slot) = values.get_mut(row) {
                    slot.push(value);
                }
            }
        }
    }
    for (row, extra) in rows.into_iter().zip(values) {
        let mut whole = row;
        whole.extend(extra);
        store.insert(whole);
    }
    Ok(())
}

/// Returns whether two rows agree on a key's columns.
///
/// @param rows - every row of the pass
/// @param key - the columns and the collations they compare under
/// @param left - one row
/// @param right - the other
fn same_key(
    rows: &[Vec<Value<'static>>],
    key: &[(usize, Collation)],
    left: usize,
    right: usize,
) -> bool {
    key.iter().all(|(column, collation)| {
        let a = value_at(rows, left, *column);
        let b = value_at(rows, right, *column);
        identical(&a, &b, *collation)
    })
}

/// Returns whether two rows agree on the window's own `ORDER BY` columns.
///
/// @param rows - every row of the pass
/// @param call - the call whose ordering is being asked about
/// @param left - one row
/// @param right - the other
fn same_order_key(
    rows: &[Vec<Value<'static>>],
    call: &WindowCall,
    left: usize,
    right: usize,
) -> bool {
    call.order.iter().all(|(column, sort)| {
        let a = value_at(rows, left, *column);
        let b = value_at(rows, right, *column);
        identical(&a, &b, sort.collation)
    })
}

/// Returns whether two values are the same for peer purposes.
///
/// Two NULLs are peers. They are not equal, but an `ORDER BY` cannot separate
/// them, and the peer group is defined by what the ordering can distinguish.
fn identical(left: &Value<'static>, right: &Value<'static>, collation: Collation) -> bool {
    if left.is_null() || right.is_null() {
        return left.is_null() && right.is_null();
    }
    compare::compare_values(left, right, collation) == std::cmp::Ordering::Equal
}

/// Returns one value of one row, or NULL when the column is not there.
fn value_at(rows: &[Vec<Value<'static>>], row: usize, column: usize) -> Value<'static> {
    rows.get(row)
        .and_then(|values| values.get(column))
        .cloned()
        .unwrap_or(Value::Null)
}

/// Computes one window call for one row.
///
/// @param rows - every row of the pass
/// @param call - the call
/// @param partition - the row's partition, with its peer groups
/// @param row - the row
/// @param encoding - the text encoding the accumulators work in
fn evaluate(
    rows: &[Vec<Value<'static>>],
    call: &WindowCall,
    partition: &frames::Partition,
    row: usize,
    encoding: TextEncoding,
) -> DbResult<Value<'static>> {
    Ok(match call.func {
        WindowSlot::Plain(WindowFunc::RowNumber) => {
            Value::Integer(frames::row_number(partition, row))
        }
        WindowSlot::Plain(WindowFunc::Rank) => Value::Integer(frames::rank(partition, row)),
        WindowSlot::Plain(WindowFunc::DenseRank) => {
            Value::Integer(frames::dense_rank(partition, row))
        }
        WindowSlot::Plain(WindowFunc::PercentRank) => {
            Value::Real(frames::percent_rank(partition, row))
        }
        WindowSlot::Plain(WindowFunc::CumeDist) => Value::Real(frames::cume_dist(partition, row)),
        WindowSlot::Plain(WindowFunc::Ntile) => ntile(rows, call, partition, row),
        WindowSlot::Plain(WindowFunc::Lag) => offset_row(rows, call, partition, row, true),
        WindowSlot::Plain(WindowFunc::Lead) => offset_row(rows, call, partition, row, false),
        WindowSlot::Plain(WindowFunc::FirstValue)
        | WindowSlot::Plain(WindowFunc::LastValue)
        | WindowSlot::Plain(WindowFunc::NthValue) => {
            let frame = frame_of(rows, call, partition, row);
            positional(rows, call, &frame, row)
        }
        WindowSlot::Aggregate(func) => {
            let frame = frame_of(rows, call, partition, row);
            let mut accumulator = Accumulator::new(func, call.distinct, call.collation);
            for member in frame {
                if !passes_filter(rows, call, member) {
                    continue;
                }
                let arguments: Vec<Value<'static>> = call
                    .arguments
                    .iter()
                    .map(|column| value_at(rows, member, *column))
                    .collect();
                let marks = vec![false; arguments.len()];
                accumulator.step(&arguments, &marks, encoding)?;
            }
            accumulator.finish()?.value
        }
    })
}

/// Returns whether a row passes the call's `FILTER (WHERE ...)`.
fn passes_filter(rows: &[Vec<Value<'static>>], call: &WindowCall, row: usize) -> bool {
    let Some(column) = call.filter else {
        return true;
    };
    let value = value_at(rows, row, column);
    !value.is_null() && inillucent_value::cast::integer_value(&value) != 0
}

/// Computes `ntile(n)`: the partition split into n groups as evenly as it can
/// be, with the larger groups first.
fn ntile(
    rows: &[Vec<Value<'static>>],
    call: &WindowCall,
    partition: &frames::Partition,
    row: usize,
) -> Value<'static> {
    let Some(column) = call.arguments.first() else {
        return Value::Null;
    };
    let buckets = inillucent_value::cast::integer_value(&value_at(rows, row, *column));
    match frames::ntile(partition, row, buckets) {
        Some(bucket) => Value::Integer(bucket),
        None => Value::Null,
    }
}

/// Computes `lag` or `lead`, which read the partition and ignore the frame.
fn offset_row(
    rows: &[Vec<Value<'static>>],
    call: &WindowCall,
    partition: &frames::Partition,
    row: usize,
    backwards: bool,
) -> Value<'static> {
    let Some(value_column) = call.arguments.first() else {
        return Value::Null;
    };
    let offset = match call.arguments.get(1) {
        Some(column) => inillucent_value::cast::integer_value(&value_at(rows, row, *column)),
        None => 1,
    };
    match frames::offset_row(partition, row, offset, backwards) {
        Some(target) => value_at(rows, target, *value_column),
        // Off the end of the partition, so the default if one was written.
        None => match call.arguments.get(2) {
            Some(column) => value_at(rows, row, *column),
            None => Value::Null,
        },
    }
}

/// Computes `first_value`, `last_value` or `nth_value` over a frame.
fn positional(
    rows: &[Vec<Value<'static>>],
    call: &WindowCall,
    frame: &[usize],
    row: usize,
) -> Value<'static> {
    let Some(value_column) = call.arguments.first() else {
        return Value::Null;
    };
    let picked = match call.func {
        WindowSlot::Plain(WindowFunc::FirstValue) => frame.first().copied(),
        WindowSlot::Plain(WindowFunc::LastValue) => frame.last().copied(),
        WindowSlot::Plain(WindowFunc::NthValue) => {
            let Some(column) = call.arguments.get(1) else {
                return Value::Null;
            };
            let nth = inillucent_value::cast::integer_value(&value_at(rows, row, *column));
            if nth < 1 {
                return Value::Null;
            }
            frame.get((nth as usize).saturating_sub(1)).copied()
        }
        _ => None,
    };
    match picked {
        Some(member) => value_at(rows, member, *value_column),
        None => Value::Null,
    }
}

/// Returns the rows of one row's frame, in partition order.
///
/// Reads the offset expressions out of their record columns and then hands the
/// resolved frame to the shared arithmetic.
///
/// @param rows - every row of the pass
/// @param call - the call whose frame this is
/// @param partition - the row's partition
/// @param row - the row
fn frame_of(
    rows: &[Vec<Value<'static>>],
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
    let order_column = call.order.first().map(|(column, _)| *column);
    let descending = call.order.first().is_some_and(|(_, sort)| sort.descending);
    frames::frame(
        partition,
        row,
        &spec,
        |member| match order_column {
            Some(column) => inillucent_value::cast::real_value(&value_at(rows, member, column)),
            None => 0.0,
        },
        descending,
    )
}

/// Reads one end of a frame into the shared form, evaluating its offset.
///
/// @param rows - every row of the pass
/// @param bound - the end as the compiler wrote it
/// @param row - the row whose offset is being read
fn resolve(rows: &[Vec<Value<'static>>], bound: FrameEnd, row: usize) -> frames::Bound {
    match bound {
        FrameEnd::UnboundedPreceding => frames::Bound::UnboundedPreceding,
        FrameEnd::CurrentRow => frames::Bound::CurrentRow,
        FrameEnd::UnboundedFollowing => frames::Bound::UnboundedFollowing,
        FrameEnd::Offset { column, preceding } => frames::Bound::Offset {
            distance: inillucent_value::cast::integer_value(&value_at(rows, row, column)),
            preceding,
        },
    }
}

/// Which of the two families a window call belongs to.
pub use crate::program::{FrameEnd, WindowSlot};
