//! Window functions: partitions, peer groups, frames, and the eleven built-ins.
//!
//! Invariant: a window value is computed from the *frame*, and the frame is
//! computed from the partition and the peer groups, in that order. Every
//! shortcut that skips a step is a wrong answer for some perfectly ordinary
//! query - `RANGE` without peer groups is `ROWS`, `EXCLUDE TIES` without them
//! is `EXCLUDE CURRENT ROW`, and a frame that ignores the partition reaches
//! into the rows of a different one.
//!
//! This is one operator rather than a sequence of opcodes, and that is a
//! deliberate trade. The frame arithmetic needs random access to the partition
//! in both directions - `EXCLUDE TIES` has to look ahead an unbounded distance
//! within a peer group - and expressing that in a register machine costs three
//! cursors and a page of jump patching per function. As one operator it is a
//! hundred lines of ordinary Rust that the verifier still checks the operands
//! of, and the machine still runs under the same interrupt and limit rules.

use rustdb_value::{compare, Collation, TextEncoding, Value};

use crate::aggregate::Accumulator;
use crate::ephemeral::Ephemeral;
use crate::program::{WindowCall, WindowPlan};
use rustdb_base::DbResult;
use rustdb_sql::ast::{FrameExclude, FrameUnit};
use rustdb_sql::function::WindowFunc;

/// Computes every window value for every row of a sorted store.
///
/// The store holds one row per input row, already ordered by the partition keys
/// and then the window's own `ORDER BY`. Each row grows by one value per window
/// call, appended in the order the calls were bound, so the drain that follows
/// reads them by a fixed column number.
pub fn compute(store: &mut Ephemeral, plan: &WindowPlan, encoding: TextEncoding) -> DbResult<()> {
    let rows = store.take_rows();
    let total = rows.len();
    let mut values: Vec<Vec<Value<'static>>> = rows.iter().map(|_| Vec::new()).collect();
    for call in &plan.calls {
        let mut start = 0usize;
        while start < total {
            let end = partition_end(&rows, plan, start);
            let peers = peer_groups(&rows, call, start, end);
            for row in start..end {
                let value = evaluate(&rows, call, start, end, row, &peers, encoding)?;
                if let Some(slot) = values.get_mut(row) {
                    slot.push(value);
                }
            }
            start = end;
        }
    }
    for (row, extra) in rows.into_iter().zip(values.into_iter()) {
        let mut whole = row;
        whole.extend(extra);
        store.insert(whole);
    }
    Ok(())
}

/// Returns the row after the last one in the partition that starts at `start`.
fn partition_end(rows: &[Vec<Value<'static>>], plan: &WindowPlan, start: usize) -> usize {
    let mut end = start.saturating_add(1);
    while end < rows.len() {
        if !same_key(rows, &plan.partition, start, end) {
            break;
        }
        end = end.saturating_add(1);
    }
    end
}

/// Returns, for each row of the partition, the peer group it belongs to as
/// `(first, last_exclusive)`.
///
/// Two rows are peers when the window's `ORDER BY` cannot tell them apart. With
/// no `ORDER BY` the whole partition is one peer group, which is what makes
/// `rank()` return 1 for every row of an unordered window.
fn peer_groups(
    rows: &[Vec<Value<'static>>],
    call: &WindowCall,
    start: usize,
    end: usize,
) -> Vec<(usize, usize)> {
    let mut groups = vec![(start, end); end.saturating_sub(start)];
    if call.order.is_empty() {
        return groups;
    }
    let mut group_start = start;
    let mut row = start;
    while row < end {
        let next = row.saturating_add(1);
        let breaks = next >= end || !same_order_key(rows, call, row, next);
        if breaks {
            for member in group_start..next {
                if let Some(slot) = groups.get_mut(member.saturating_sub(start)) {
                    *slot = (group_start, next);
                }
            }
            group_start = next;
        }
        row = next;
    }
    groups
}

/// Returns whether two rows agree on a key's columns.
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
fn evaluate(
    rows: &[Vec<Value<'static>>],
    call: &WindowCall,
    start: usize,
    end: usize,
    row: usize,
    peers: &[(usize, usize)],
    encoding: TextEncoding,
) -> DbResult<Value<'static>> {
    let (peer_start, peer_end) = peers
        .get(row.saturating_sub(start))
        .copied()
        .unwrap_or((row, row.saturating_add(1)));
    Ok(match call.func {
        WindowSlot::Plain(WindowFunc::RowNumber) => {
            Value::Integer(row.saturating_sub(start).saturating_add(1) as i64)
        }
        WindowSlot::Plain(WindowFunc::Rank) => {
            Value::Integer(peer_start.saturating_sub(start).saturating_add(1) as i64)
        }
        WindowSlot::Plain(WindowFunc::DenseRank) => {
            Value::Integer(dense_rank(rows, call, start, row) as i64)
        }
        WindowSlot::Plain(WindowFunc::PercentRank) => {
            let count = end.saturating_sub(start);
            if count <= 1 {
                return Ok(Value::Real(0.0));
            }
            let rank = peer_start.saturating_sub(start) as f64;
            Value::Real(rank / (count.saturating_sub(1) as f64))
        }
        WindowSlot::Plain(WindowFunc::CumeDist) => {
            let count = end.saturating_sub(start) as f64;
            let seen = peer_end.saturating_sub(start) as f64;
            Value::Real(seen / count)
        }
        WindowSlot::Plain(WindowFunc::Ntile) => ntile(rows, call, start, end, row),
        WindowSlot::Plain(WindowFunc::Lag) => offset_row(rows, call, start, end, row, true),
        WindowSlot::Plain(WindowFunc::Lead) => offset_row(rows, call, start, end, row, false),
        WindowSlot::Plain(WindowFunc::FirstValue)
        | WindowSlot::Plain(WindowFunc::LastValue)
        | WindowSlot::Plain(WindowFunc::NthValue) => {
            let frame = frame_of(rows, call, start, end, row, peer_start, peer_end);
            positional(rows, call, &frame, row)
        }
        WindowSlot::Aggregate(func) => {
            let frame = frame_of(rows, call, start, end, row, peer_start, peer_end);
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
    !value.is_null() && rustdb_value::cast::integer_value(&value) != 0
}

/// Returns how many distinct peer groups the partition has produced up to and
/// including one row.
fn dense_rank(rows: &[Vec<Value<'static>>], call: &WindowCall, start: usize, row: usize) -> usize {
    let mut rank = 1usize;
    let mut previous = start;
    let mut member = start.saturating_add(1);
    while member <= row {
        if !same_order_key(rows, call, previous, member) {
            rank = rank.saturating_add(1);
            previous = member;
        }
        member = member.saturating_add(1);
    }
    rank
}

/// Computes `ntile(n)`: the partition split into n groups as evenly as it can
/// be, with the larger groups first.
fn ntile(
    rows: &[Vec<Value<'static>>],
    call: &WindowCall,
    start: usize,
    end: usize,
    row: usize,
) -> Value<'static> {
    let Some(column) = call.arguments.first() else {
        return Value::Null;
    };
    let buckets = rustdb_value::cast::integer_value(&value_at(rows, row, *column));
    if buckets <= 0 {
        return Value::Null;
    }
    let buckets = buckets as usize;
    let count = end.saturating_sub(start);
    let position = row.saturating_sub(start);
    let base = count / buckets.max(1);
    let extra = count % buckets.max(1);
    let boundary = extra.saturating_mul(base.saturating_add(1));
    let bucket = if position < boundary {
        position / base.saturating_add(1)
    } else {
        extra.saturating_add(position.saturating_sub(boundary) / base.max(1))
    };
    Value::Integer(bucket.saturating_add(1) as i64)
}

/// Computes `lag` or `lead`, which read the partition and ignore the frame.
fn offset_row(
    rows: &[Vec<Value<'static>>],
    call: &WindowCall,
    start: usize,
    end: usize,
    row: usize,
    backwards: bool,
) -> Value<'static> {
    let Some(value_column) = call.arguments.first() else {
        return Value::Null;
    };
    let offset = match call.arguments.get(1) {
        Some(column) => rustdb_value::cast::integer_value(&value_at(rows, row, *column)),
        None => 1,
    };
    let target = if backwards {
        (row as i64).checked_sub(offset)
    } else {
        (row as i64).checked_add(offset)
    };
    let inside = target.is_some_and(|target| target >= start as i64 && target < end as i64);
    if !inside {
        return match call.arguments.get(2) {
            Some(column) => value_at(rows, row, *column),
            None => Value::Null,
        };
    }
    let target = target.unwrap_or(0).max(0) as usize;
    value_at(rows, target, *value_column)
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
            let nth = rustdb_value::cast::integer_value(&value_at(rows, row, *column));
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
fn frame_of(
    rows: &[Vec<Value<'static>>],
    call: &WindowCall,
    start: usize,
    end: usize,
    row: usize,
    peer_start: usize,
    peer_end: usize,
) -> Vec<usize> {
    let low = bound_of(
        rows,
        call,
        &call.frame.start,
        start,
        end,
        row,
        peer_start,
        peer_end,
        true,
    );
    let high = bound_of(
        rows,
        call,
        &call.frame.end,
        start,
        end,
        row,
        peer_start,
        peer_end,
        false,
    );
    let mut members = Vec::new();
    if low > high {
        return members;
    }
    for member in low..=high {
        if excluded(call, member, row, peer_start, peer_end) {
            continue;
        }
        members.push(member);
    }
    members
}

/// Returns whether `EXCLUDE` drops a row from a frame.
fn excluded(
    call: &WindowCall,
    member: usize,
    row: usize,
    peer_start: usize,
    peer_end: usize,
) -> bool {
    match call.frame.exclude {
        FrameExclude::NoOthers => false,
        FrameExclude::CurrentRow => member == row,
        FrameExclude::Group => member >= peer_start && member < peer_end,
        // TIES drops the peers but keeps the row itself, which is the one
        // difference between it and GROUP and the reason they cannot share a
        // branch.
        FrameExclude::Ties => member != row && member >= peer_start && member < peer_end,
    }
}

/// Returns the row number one end of a frame resolves to.
#[allow(clippy::too_many_arguments)]
fn bound_of(
    rows: &[Vec<Value<'static>>],
    call: &WindowCall,
    bound: &FrameEnd,
    start: usize,
    end: usize,
    row: usize,
    peer_start: usize,
    peer_end: usize,
    is_start: bool,
) -> usize {
    let last = end.saturating_sub(1);
    match bound {
        FrameEnd::UnboundedPreceding => start,
        FrameEnd::UnboundedFollowing => last,
        FrameEnd::CurrentRow => match call.frame.unit {
            // `CURRENT ROW` means the row for `ROWS` and the peer group for
            // `RANGE` and `GROUPS`. Treating them alike turns every default
            // `RANGE` frame into a `ROWS` one, which differs on any query with
            // ties in its ordering.
            FrameUnit::Rows => row,
            _ => {
                if is_start {
                    peer_start
                } else {
                    peer_end.saturating_sub(1)
                }
            }
        },
        FrameEnd::Offset { column, preceding } => {
            let offset = rustdb_value::cast::integer_value(&value_at(rows, row, *column));
            match call.frame.unit {
                FrameUnit::Rows => {
                    let target = if *preceding {
                        (row as i64).saturating_sub(offset)
                    } else {
                        (row as i64).saturating_add(offset)
                    };
                    target.clamp(start as i64, last as i64) as usize
                }
                FrameUnit::Groups => {
                    let groups =
                        group_bound(rows, call, start, end, row, offset, *preceding, is_start);
                    groups
                }
                FrameUnit::Range => {
                    range_bound(rows, call, start, end, row, offset, *preceding, is_start)
                }
            }
        }
    }
}

/// Returns the row a `GROUPS` offset resolves to.
#[allow(clippy::too_many_arguments)]
fn group_bound(
    rows: &[Vec<Value<'static>>],
    call: &WindowCall,
    start: usize,
    end: usize,
    row: usize,
    offset: i64,
    preceding: bool,
    is_start: bool,
) -> usize {
    let here = dense_rank(rows, call, start, row) as i64;
    let wanted = if preceding {
        here.saturating_sub(offset)
    } else {
        here.saturating_add(offset)
    };
    let mut answer = if is_start { end } else { start };
    let mut found = false;
    for member in start..end {
        let rank = dense_rank(rows, call, start, member) as i64;
        if rank != wanted {
            continue;
        }
        found = true;
        if is_start {
            answer = member;
            break;
        }
        answer = member;
    }
    if !found {
        return if preceding == is_start {
            if is_start {
                start
            } else {
                end.saturating_sub(1)
            }
        } else if is_start {
            end
        } else {
            start
        };
    }
    answer
}

/// Returns the row a `RANGE` offset resolves to.
///
/// The offset is added to or subtracted from the single `ORDER BY` value, and
/// the bound is the first or last row whose value is on the right side of that
/// - which is why `RANGE` with an offset needs exactly one ordering term and a
/// numeric one.
#[allow(clippy::too_many_arguments)]
fn range_bound(
    rows: &[Vec<Value<'static>>],
    call: &WindowCall,
    start: usize,
    end: usize,
    row: usize,
    offset: i64,
    preceding: bool,
    is_start: bool,
) -> usize {
    let Some((column, sort)) = call.order.first() else {
        return if is_start {
            start
        } else {
            end.saturating_sub(1)
        };
    };
    let here = rustdb_value::cast::real_value(&value_at(rows, row, *column));
    let offset = offset as f64;
    let descending = sort.descending;
    let limit = if preceding == !descending {
        here - offset
    } else {
        here + offset
    };
    let mut answer = if is_start { end } else { start };
    let mut found = false;
    for member in start..end {
        let value = rustdb_value::cast::real_value(&value_at(rows, member, *column));
        let inside = if is_start {
            if descending {
                value <= limit
            } else {
                value >= limit
            }
        } else if descending {
            value >= limit
        } else {
            value <= limit
        };
        if !inside {
            continue;
        }
        found = true;
        if is_start {
            answer = member;
            break;
        }
        answer = member;
    }
    if !found {
        return if is_start { end } else { start };
    }
    answer
}

/// Which of the two families a window call belongs to.
pub use crate::program::{FrameEnd, WindowSlot};
