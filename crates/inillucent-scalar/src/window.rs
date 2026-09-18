//! Window frame arithmetic: partitions, peer groups, ranks and frames.
//!
//! Invariant: a window value is computed from the *frame*, the frame from the
//! peer groups, and the peer groups from the partition, in that order. Every
//! shortcut that skips a step is a wrong answer for some perfectly ordinary
//! query - `RANGE` without peer groups is `ROWS`, `EXCLUDE TIES` without them
//! is `EXCLUDE CURRENT ROW`, and a frame that ignores the partition reaches
//! into the rows of a different one.
//!
//! ## Why this is here and the accumulators are not
//!
//! This crate's charter says aggregates stay with their executors, because an
//! accumulator is a piece of operator state rather than a function of values.
//! That is still true and there are genuinely two of them - the VM's, which
//! steps over `Value`s, and the vectorised executor's, which steps over a
//! column at a time and compensates its float sums.
//!
//! *Which rows a frame contains* is not operator state. It is a pure function
//! of two row counts and a handful of booleans, and it is where the dialect's
//! genuinely hard cases live: `EXCLUDE TIES` differing from `EXCLUDE GROUP` by
//! one row, `CURRENT ROW` meaning something different under `RANGE` than under
//! `ROWS`, a `GROUPS` offset counting peer groups rather than rows. Two copies
//! of that would agree the day they were written and diverge at the first fix,
//! which is the same argument that moved `substr()` into this crate.
//!
//! So the arithmetic is here and takes callbacks for the parts that need a
//! row's values; both executors bring their own comparisons and their own
//! accumulators to it.

use inillucent_sql::ast::{FrameExclude, FrameUnit};

/// One end of a frame, with the offset already resolved to a number.
///
/// The offset expression is evaluated once per row by the caller, because the
/// two executors hold it in different places - a record column in the VM, an
/// evaluated batch column in the vectorised executor - and neither shape is
/// worth teaching this module about.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Bound {
    /// `UNBOUNDED PRECEDING`.
    UnboundedPreceding,
    /// `CURRENT ROW`.
    CurrentRow,
    /// `UNBOUNDED FOLLOWING`.
    UnboundedFollowing,
    /// `expr PRECEDING` or `expr FOLLOWING`, with the offset already read.
    Offset {
        /// How far.
        distance: i64,
        /// Whether it counts backwards.
        preceding: bool,
    },
}

/// A frame specification with both ends resolved.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FrameSpec {
    /// `ROWS`, `RANGE` or `GROUPS`.
    pub unit: FrameUnit,
    /// The start.
    pub start: Bound,
    /// The end.
    pub end: Bound,
    /// The `EXCLUDE` clause.
    pub exclude: FrameExclude,
}

/// One partition of the sorted input, with its peer groups worked out.
///
/// `peers[i]` is the half-open peer group of row `start + i`. With no window
/// `ORDER BY` the whole partition is one peer group, which is what makes
/// `rank()` return 1 for every row of an unordered window.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Partition {
    /// The first row.
    pub start: usize,
    /// One past the last row.
    pub end: usize,
    /// The peer group of each row, as `(first, last_exclusive)`.
    pub peers: Vec<(usize, usize)>,
}

impl Partition {
    /// Returns how many rows the partition has.
    pub fn len(&self) -> usize {
        self.end.saturating_sub(self.start)
    }

    /// Returns whether the partition has no rows.
    pub fn is_empty(&self) -> bool {
        self.end <= self.start
    }

    /// Returns the peer group one row belongs to.
    ///
    /// @param row - an absolute row number
    pub fn peers_of(&self, row: usize) -> (usize, usize) {
        self.peers
            .get(row.saturating_sub(self.start))
            .copied()
            .unwrap_or((row, row.saturating_add(1)))
    }
}

/// Splits a sorted row range into partitions and peer groups.
///
/// The input is already ordered by the partition keys and then by the window's
/// own `ORDER BY`, so both boundaries are found by walking adjacent pairs.
///
/// @param total - how many rows there are
/// @param same_partition - whether two rows share every partition key
/// @param same_order - whether the window's `ORDER BY` can tell two rows apart
/// @param ordered - whether the window named an `ORDER BY` at all
pub fn partitions(
    total: usize,
    same_partition: impl Fn(usize, usize) -> bool,
    same_order: impl Fn(usize, usize) -> bool,
    ordered: bool,
) -> Vec<Partition> {
    let mut found = Vec::new();
    let mut start = 0usize;
    while start < total {
        let mut end = start.saturating_add(1);
        while end < total && same_partition(start, end) {
            end = end.saturating_add(1);
        }
        found.push(Partition {
            start,
            end,
            peers: peer_groups(start, end, &same_order, ordered),
        });
        start = end;
    }
    found
}

/// Returns the peer group of each row of one partition.
///
/// @param start - the partition's first row
/// @param end - one past its last
/// @param same_order - whether the `ORDER BY` can tell two rows apart
/// @param ordered - whether the window named an `ORDER BY` at all
fn peer_groups(
    start: usize,
    end: usize,
    same_order: &impl Fn(usize, usize) -> bool,
    ordered: bool,
) -> Vec<(usize, usize)> {
    let mut groups = vec![(start, end); end.saturating_sub(start)];
    if !ordered {
        return groups;
    }
    let mut group_start = start;
    let mut row = start;
    while row < end {
        let next = row.saturating_add(1);
        if next >= end || !same_order(row, next) {
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

/// Returns `row_number()`: the one-based position in the partition.
///
/// @param partition - the row's partition
/// @param row - an absolute row number
pub fn row_number(partition: &Partition, row: usize) -> i64 {
    row.saturating_sub(partition.start).saturating_add(1) as i64
}

/// Returns `rank()`: the one-based position of the row's peer group.
///
/// @param partition - the row's partition
/// @param row - an absolute row number
pub fn rank(partition: &Partition, row: usize) -> i64 {
    let (peer_start, _) = partition.peers_of(row);
    peer_start.saturating_sub(partition.start).saturating_add(1) as i64
}

/// Returns `dense_rank()`: how many peer groups have started up to this row.
///
/// Walks group to group rather than row to row, so a partition of one peer
/// group answers in one step however many rows it holds.
///
/// @param partition - the row's partition
/// @param row - an absolute row number
pub fn dense_rank(partition: &Partition, row: usize) -> i64 {
    let mut rank = 1i64;
    let mut member = partition.start;
    while member < row && member < partition.end {
        let (_, group_end) = partition.peers_of(member);
        if group_end > row || group_end <= member {
            break;
        }
        rank = rank.saturating_add(1);
        member = group_end;
    }
    rank
}

/// Returns `percent_rank()`.
///
/// Zero for a one-row partition, which is the standard's answer rather than the
/// division the arithmetic would otherwise ask for.
///
/// @param partition - the row's partition
/// @param row - an absolute row number
pub fn percent_rank(partition: &Partition, row: usize) -> f64 {
    let count = partition.len();
    if count <= 1 {
        return 0.0;
    }
    let (peer_start, _) = partition.peers_of(row);
    peer_start.saturating_sub(partition.start) as f64 / (count.saturating_sub(1) as f64)
}

/// Returns `cume_dist()`: the share of the partition at or before this peer group.
///
/// @param partition - the row's partition
/// @param row - an absolute row number
pub fn cume_dist(partition: &Partition, row: usize) -> f64 {
    let count = partition.len() as f64;
    if count == 0.0 {
        return 0.0;
    }
    let (_, peer_end) = partition.peers_of(row);
    peer_end.saturating_sub(partition.start) as f64 / count
}

/// Returns `ntile(n)`: the partition split as evenly as it can be, larger groups first.
///
/// @param partition - the row's partition
/// @param row - an absolute row number
/// @param buckets - the requested bucket count; zero or negative has no answer
pub fn ntile(partition: &Partition, row: usize, buckets: i64) -> Option<i64> {
    if buckets <= 0 {
        return None;
    }
    let buckets = buckets as usize;
    let count = partition.len();
    let position = row.saturating_sub(partition.start);
    let base = count / buckets.max(1);
    let extra = count % buckets.max(1);
    let boundary = extra.saturating_mul(base.saturating_add(1));
    let bucket = if position < boundary {
        position / base.saturating_add(1)
    } else {
        extra.saturating_add(position.saturating_sub(boundary) / base.max(1))
    };
    Some(bucket.saturating_add(1) as i64)
}

/// Returns the row a `lag`/`lead` offset lands on, or `None` when it leaves the partition.
///
/// @param partition - the row's partition
/// @param row - the current row
/// @param offset - how far to look
/// @param backwards - `lag` rather than `lead`
pub fn offset_row(
    partition: &Partition,
    row: usize,
    offset: i64,
    backwards: bool,
) -> Option<usize> {
    let target = if backwards {
        (row as i64).checked_sub(offset)?
    } else {
        (row as i64).checked_add(offset)?
    };
    if target < partition.start as i64 || target >= partition.end as i64 {
        return None;
    }
    Some(target as usize)
}

/// Returns the rows of one row's frame, in partition order.
///
/// @param partition - the row's partition
/// @param row - the row whose frame this is
/// @param spec - the frame, with its offsets already resolved
/// @param order_value - the row's single `ORDER BY` value, for a `RANGE` offset,
///   and `None` when that value is NULL
/// @param descending - whether that ordering term is descending
pub fn frame(
    partition: &Partition,
    row: usize,
    spec: &FrameSpec,
    order_value: impl Fn(usize) -> Option<f64>,
    descending: bool,
) -> Vec<usize> {
    let (peer_start, peer_end) = partition.peers_of(row);
    let low = bound_of(
        partition,
        row,
        spec,
        spec.start,
        true,
        &order_value,
        descending,
    );
    let high = bound_of(
        partition,
        row,
        spec,
        spec.end,
        false,
        &order_value,
        descending,
    );
    let mut members = Vec::new();
    // **A bound that falls off the partition empties the frame (task-1913).**
    // `ROWS BETWEEN 1 FOLLOWING AND 2 FOLLOWING` on the last row names rows
    // that are not there, and the reference answers NULL for it. Clamping both
    // ends into the partition instead turned that into a frame of one row -
    // the row itself - so the last row of every such window read its own value
    // where SQLite reads nothing, and the first row did the same for a frame
    // written entirely in `PRECEDING`. `None` is the bound saying the frame
    // begins after the partition ends, or ends before it begins; a bound that
    // merely reaches past an edge still answers with that edge.
    let (Some(low), Some(high)) = (low, high) else {
        return members;
    };
    if low > high || partition.is_empty() {
        return members;
    }
    for member in low..=high {
        if excluded(spec.exclude, member, row, peer_start, peer_end) {
            continue;
        }
        members.push(member);
    }
    members
}

/// Returns whether `EXCLUDE` drops a row from a frame.
///
/// @param exclude - the clause
/// @param member - the row being considered
/// @param row - the row whose frame it is
/// @param peer_start - the first row of that row's peer group
/// @param peer_end - one past its last
fn excluded(
    exclude: FrameExclude,
    member: usize,
    row: usize,
    peer_start: usize,
    peer_end: usize,
) -> bool {
    match exclude {
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
///
/// @param partition - the row's partition
/// @param row - the row whose frame it is
/// @param spec - the frame
/// @param bound - which end is being resolved
/// @param is_start - whether it is the start
/// @param order_value - the ordering value of a row, for a `RANGE` offset, and
///   `None` when that value is NULL
/// @param descending - whether the ordering term is descending
fn bound_of(
    partition: &Partition,
    row: usize,
    spec: &FrameSpec,
    bound: Bound,
    is_start: bool,
    order_value: &impl Fn(usize) -> Option<f64>,
    descending: bool,
) -> Option<usize> {
    let (peer_start, peer_end) = partition.peers_of(row);
    let last = partition.end.saturating_sub(1);
    Some(match bound {
        Bound::UnboundedPreceding => partition.start,
        Bound::UnboundedFollowing => last,
        Bound::CurrentRow => match spec.unit {
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
        Bound::Offset {
            distance,
            preceding,
        } => match spec.unit {
            FrameUnit::Rows => {
                let target = if preceding {
                    (row as i64).saturating_sub(distance)
                } else {
                    (row as i64).saturating_add(distance)
                };
                // The frame is empty when its *start* is past the last row or
                // its *end* is before the first; a bound that overshoots the
                // other way is the edge it overshot.
                if is_start && target > last as i64 {
                    return None;
                }
                if !is_start && target < partition.start as i64 {
                    return None;
                }
                target.clamp(partition.start as i64, last as i64) as usize
            }
            FrameUnit::Groups => group_bound(partition, row, distance, preceding, is_start)?,
            FrameUnit::Range => range_bound(
                partition,
                row,
                distance,
                preceding,
                is_start,
                order_value,
                descending,
            )?,
        },
    })
}

/// Returns the row a `GROUPS` offset resolves to.
///
/// @param partition - the row's partition
/// @param row - the row whose frame it is
/// @param offset - how many peer groups away
/// @param preceding - whether it counts backwards
/// @param is_start - whether this is the frame's start
fn group_bound(
    partition: &Partition,
    row: usize,
    offset: i64,
    preceding: bool,
    is_start: bool,
) -> Option<usize> {
    let here = dense_rank(partition, row);
    let wanted = if preceding {
        here.saturating_sub(offset)
    } else {
        here.saturating_add(offset)
    };
    let mut answer = None;
    for member in partition.start..partition.end {
        if dense_rank(partition, member) != wanted {
            continue;
        }
        answer = Some(member);
        if is_start {
            break;
        }
    }
    match answer {
        Some(member) => Some(member),
        // The wanted group is off one end of the partition. Which end decides
        // whether the frame runs to the edge or is empty.
        None if preceding == is_start => Some(if is_start {
            partition.start
        } else {
            partition.end.saturating_sub(1)
        }),
        // The frame begins after the partition ends, or ends before it begins.
        // Both are empty, and answering with an edge made the second of them a
        // frame of one row (task-1913).
        None => None,
    }
}

/// Returns the row a `RANGE` offset resolves to.
///
/// The offset is added to or subtracted from the single `ORDER BY` value, and
/// the bound is the first or last row whose value is on the right side of that
/// - which is why `RANGE` with an offset needs exactly one ordering term and a
/// numeric one.
///
/// @param partition - the row's partition
/// @param row - the row whose frame it is
/// @param offset - the distance in ordering values
/// @param preceding - whether it counts backwards
/// @param is_start - whether this is the frame's start
/// @param order_value - the ordering value of a row, `None` when it is NULL
/// @param descending - whether the ordering term is descending
fn range_bound(
    partition: &Partition,
    row: usize,
    offset: i64,
    preceding: bool,
    is_start: bool,
    order_value: &impl Fn(usize) -> Option<f64>,
    descending: bool,
) -> Option<usize> {
    // **A NULL ordering value has no distance to anything, so an offset bound
    // on such a row resolves to its peer group (task-1913).** NULLs sort
    // together at one end, so the peer group is exactly the NULL rows, and
    // `sum(n) OVER (ORDER BY n RANGE BETWEEN 1 PRECEDING AND 1 FOLLOWING)`
    // therefore answers NULL on them rather than reaching into the numbers
    // beside them. This engine read a NULL as `0.0`, which put every NULL row
    // one unit away from zero and, worse, put the NULL rows *inside* the frame
    // of every row whose value was near zero.
    let (peer_start, peer_end) = partition.peers_of(row);
    let Some(here) = order_value(row) else {
        return Some(if is_start {
            peer_start
        } else {
            peer_end.saturating_sub(1)
        });
    };
    let offset = offset as f64;
    let limit = if preceding != descending {
        here - offset
    } else {
        here + offset
    };
    let mut answer = None;
    for member in partition.start..partition.end {
        // A row whose ordering value is NULL is not within any distance of a
        // row that has one, so it can never be the row an offset bound lands
        // on. An `UNBOUNDED` bound still reaches it, which is why this is here
        // rather than in `frame`.
        let Some(value) = order_value(member) else {
            continue;
        };
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
        answer = Some(member);
        if is_start {
            break;
        }
    }
    // No row is on the right side of the limit, so the frame begins after the
    // partition ends or ends before it begins. Both are empty (task-1913).
    answer
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds one partition over the rows whose peer groups are the runs of
    /// equal values in `keys`.
    ///
    /// @param keys - one ordering key per row
    fn one(keys: &[i64]) -> Partition {
        let owned = keys.to_vec();
        let parts = partitions(owned.len(), |_, _| true, |a, b| owned[a] == owned[b], true);
        parts.into_iter().next().unwrap()
    }

    #[test]
    fn peers_are_the_runs_of_equal_keys() {
        let partition = one(&[1, 1, 2, 3, 3, 3]);
        assert_eq!(partition.peers_of(0), (0, 2));
        assert_eq!(partition.peers_of(1), (0, 2));
        assert_eq!(partition.peers_of(2), (2, 3));
        assert_eq!(partition.peers_of(5), (3, 6));
    }

    #[test]
    fn an_unordered_window_makes_the_whole_partition_one_peer_group() {
        let parts = partitions(4, |_, _| true, |_, _| false, false);
        let partition = &parts[0];
        assert_eq!(partition.peers_of(0), (0, 4));
        assert_eq!(rank(partition, 3), 1);
        assert_eq!(dense_rank(partition, 3), 1);
    }

    #[test]
    fn the_four_ranks_agree_with_the_standard() {
        let partition = one(&[1, 1, 2, 3, 3, 3]);
        let ranks: Vec<i64> = (0..6).map(|row| rank(&partition, row)).collect();
        assert_eq!(ranks, vec![1, 1, 3, 4, 4, 4]);
        let dense: Vec<i64> = (0..6).map(|row| dense_rank(&partition, row)).collect();
        assert_eq!(dense, vec![1, 1, 2, 3, 3, 3]);
        let numbers: Vec<i64> = (0..6).map(|row| row_number(&partition, row)).collect();
        assert_eq!(numbers, vec![1, 2, 3, 4, 5, 6]);
        assert!((cume_dist(&partition, 0) - 2.0 / 6.0).abs() < 1e-12);
        assert!((percent_rank(&partition, 2) - 2.0 / 5.0).abs() < 1e-12);
    }

    #[test]
    fn percent_rank_of_a_single_row_is_zero_rather_than_a_division() {
        let partition = one(&[7]);
        assert_eq!(percent_rank(&partition, 0), 0.0);
        assert_eq!(cume_dist(&partition, 0), 1.0);
    }

    #[test]
    fn ntile_puts_the_larger_groups_first() {
        let partition = one(&[1, 2, 3, 4, 5]);
        let buckets: Vec<i64> = (0..5)
            .map(|row| ntile(&partition, row, 3).unwrap())
            .collect();
        assert_eq!(buckets, vec![1, 1, 2, 2, 3]);
        assert_eq!(ntile(&partition, 0, 0), None);
        assert_eq!(ntile(&partition, 0, -3), None);
    }

    /// `ROWS BETWEEN 1 PRECEDING AND 1 FOLLOWING`.
    fn sliding() -> FrameSpec {
        FrameSpec {
            unit: FrameUnit::Rows,
            start: Bound::Offset {
                distance: 1,
                preceding: true,
            },
            end: Bound::Offset {
                distance: 1,
                preceding: false,
            },
            exclude: FrameExclude::NoOthers,
        }
    }

    #[test]
    fn a_rows_frame_clamps_at_the_partition_edges() {
        let partition = one(&[1, 2, 3, 4]);
        let spec = sliding();
        assert_eq!(
            frame(&partition, 0, &spec, |_| Some(0.0), false),
            vec![0, 1]
        );
        assert_eq!(
            frame(&partition, 2, &spec, |_| Some(0.0), false),
            vec![1, 2, 3]
        );
        assert_eq!(
            frame(&partition, 3, &spec, |_| Some(0.0), false),
            vec![2, 3]
        );
    }

    #[test]
    fn current_row_is_the_peer_group_under_range_and_the_row_under_rows() {
        let partition = one(&[1, 1, 2]);
        let mut spec = FrameSpec {
            unit: FrameUnit::Range,
            start: Bound::UnboundedPreceding,
            end: Bound::CurrentRow,
            exclude: FrameExclude::NoOthers,
        };
        // Row 0 is a peer of row 1, so the default RANGE frame reaches it.
        assert_eq!(
            frame(&partition, 0, &spec, |_| Some(0.0), false),
            vec![0, 1]
        );
        spec.unit = FrameUnit::Rows;
        assert_eq!(frame(&partition, 0, &spec, |_| Some(0.0), false), vec![0]);
    }

    #[test]
    fn exclude_ties_keeps_the_row_and_group_does_not() {
        let partition = one(&[1, 1, 1]);
        let mut spec = FrameSpec {
            unit: FrameUnit::Rows,
            start: Bound::UnboundedPreceding,
            end: Bound::UnboundedFollowing,
            exclude: FrameExclude::Ties,
        };
        assert_eq!(frame(&partition, 1, &spec, |_| Some(0.0), false), vec![1]);
        spec.exclude = FrameExclude::Group;
        assert!(frame(&partition, 1, &spec, |_| Some(0.0), false).is_empty());
        spec.exclude = FrameExclude::CurrentRow;
        assert_eq!(
            frame(&partition, 1, &spec, |_| Some(0.0), false),
            vec![0, 2]
        );
        spec.exclude = FrameExclude::NoOthers;
        assert_eq!(
            frame(&partition, 1, &spec, |_| Some(0.0), false),
            vec![0, 1, 2]
        );
    }

    #[test]
    fn a_groups_offset_counts_peer_groups_rather_than_rows() {
        let partition = one(&[1, 1, 2, 3, 3]);
        let spec = FrameSpec {
            unit: FrameUnit::Groups,
            start: Bound::Offset {
                distance: 1,
                preceding: true,
            },
            end: Bound::CurrentRow,
            exclude: FrameExclude::NoOthers,
        };
        // Row 2's own group is {2}; one group back is {0,1}. Under ROWS the
        // same offset would have reached only row 1.
        assert_eq!(
            frame(&partition, 2, &spec, |_| Some(0.0), false),
            vec![0, 1, 2]
        );
    }

    #[test]
    fn a_range_offset_reads_the_ordering_value() {
        let values = [10.0f64, 11.0, 20.0, 21.0];
        let partition = one(&[10, 11, 20, 21]);
        let spec = FrameSpec {
            unit: FrameUnit::Range,
            start: Bound::Offset {
                distance: 1,
                preceding: true,
            },
            end: Bound::Offset {
                distance: 1,
                preceding: false,
            },
            exclude: FrameExclude::NoOthers,
        };
        assert_eq!(
            frame(&partition, 1, &spec, |row| Some(values[row]), false),
            vec![0, 1]
        );
        assert_eq!(
            frame(&partition, 2, &spec, |row| Some(values[row]), false),
            vec![2, 3]
        );
    }

    /// A frame written entirely off one end of the partition is empty.
    ///
    /// **Clamping both ends into the partition made it a frame of one row
    /// (task-1913).** `ROWS BETWEEN 1 FOLLOWING AND 2 FOLLOWING` on the last
    /// row names rows that are not there: its start clamped to the last row,
    /// its end clamped to the last row, and the frame came out holding the row
    /// itself - so `sum` answered the row's own value where the reference
    /// answers NULL. The same at the other end for a frame written entirely in
    /// `PRECEDING`, in all three units: `ROWS` was wrong at both edges,
    /// `GROUPS` and `RANGE` at the start only, because their fallbacks already
    /// emptied a frame that began past the end and not one that ended before
    /// the beginning.
    #[test]
    fn a_frame_entirely_off_an_edge_is_empty() {
        let partition = one(&[1, 2, 3]);
        let value = |row: usize| Some([1.0f64, 2.0, 3.0][row]);
        let following = FrameSpec {
            unit: FrameUnit::Rows,
            start: Bound::Offset {
                distance: 1,
                preceding: false,
            },
            end: Bound::Offset {
                distance: 2,
                preceding: false,
            },
            exclude: FrameExclude::NoOthers,
        };
        assert_eq!(frame(&partition, 0, &following, value, false), vec![1, 2]);
        assert_eq!(frame(&partition, 1, &following, value, false), vec![2]);
        assert!(
            frame(&partition, 2, &following, value, false).is_empty(),
            "the last row has no row after it"
        );

        let preceding = FrameSpec {
            unit: FrameUnit::Rows,
            start: Bound::Offset {
                distance: 2,
                preceding: true,
            },
            end: Bound::Offset {
                distance: 1,
                preceding: true,
            },
            exclude: FrameExclude::NoOthers,
        };
        assert!(
            frame(&partition, 0, &preceding, value, false).is_empty(),
            "the first row has no row before it"
        );
        assert_eq!(frame(&partition, 1, &preceding, value, false), vec![0]);
        assert_eq!(frame(&partition, 2, &preceding, value, false), vec![0, 1]);

        // The same two frames in the other two units.
        for unit in [FrameUnit::Groups, FrameUnit::Range] {
            let off_the_end = FrameSpec { unit, ..following };
            let off_the_start = FrameSpec { unit, ..preceding };
            assert!(
                frame(&partition, 2, &off_the_end, value, false).is_empty(),
                "{unit:?} kept a row in a frame that begins after the last one"
            );
            assert!(
                frame(&partition, 0, &off_the_start, value, false).is_empty(),
                "{unit:?} kept a row in a frame that ends before the first one"
            );
        }
    }

    /// A `RANGE` offset on a row whose ordering value is NULL is its peer
    /// group, and no NULL row is ever inside a valued row's frame.
    ///
    /// **The two directions of one bug (task-1913).** A NULL read as `0.0` is
    /// a distance from zero, so the NULL rows were drawn into the frame of
    /// every row near zero and the numbers near zero were drawn into the NULL
    /// rows' frames. Both are asserted, because fixing either one alone leaves
    /// the other wrong.
    #[test]
    fn a_range_offset_gives_a_null_row_its_peer_group_and_nothing_else() {
        // Ordered as SQLite orders them: the NULLs first, then the values.
        let values = [None, None, Some(-4.0f64), Some(1.0), Some(2.0)];
        let partition = one(&[0, 0, 1, 2, 3]);
        let spec = FrameSpec {
            unit: FrameUnit::Range,
            start: Bound::Offset {
                distance: 1,
                preceding: true,
            },
            end: Bound::Offset {
                distance: 1,
                preceding: false,
            },
            exclude: FrameExclude::NoOthers,
        };
        // A NULL row sees the NULL rows.
        assert_eq!(
            frame(&partition, 0, &spec, |row| values[row], false),
            vec![0, 1]
        );
        assert_eq!(
            frame(&partition, 1, &spec, |row| values[row], false),
            vec![0, 1]
        );
        // A valued row sees neither NULL row: 1 reaches 2 and stops short of
        // -4, and before the fix it started at row 0.
        assert_eq!(
            frame(&partition, 3, &spec, |row| values[row], false),
            vec![3, 4]
        );
        // -4 is alone: its own value, with nothing within one of it.
        assert_eq!(
            frame(&partition, 2, &spec, |row| values[row], false),
            vec![2]
        );
    }

    /// An `UNBOUNDED` bound still reaches a NULL row.
    ///
    /// The rule above is about *offset* bounds. `UNBOUNDED PRECEDING` means
    /// the start of the partition whatever is there, and SQLite's answer for
    /// the row after the NULLs includes them.
    #[test]
    fn an_unbounded_bound_still_reaches_a_null_row() {
        let values = [None, None, Some(-4.0f64), Some(1.0), Some(2.0)];
        let partition = one(&[0, 0, 1, 2, 3]);
        let spec = FrameSpec {
            unit: FrameUnit::Range,
            start: Bound::UnboundedPreceding,
            end: Bound::Offset {
                distance: 1,
                preceding: false,
            },
            exclude: FrameExclude::NoOthers,
        };
        assert_eq!(
            frame(&partition, 2, &spec, |row| values[row], false),
            vec![0, 1, 2]
        );
        // And the NULL row's own frame under the same spec is the NULL peers:
        // the start is the partition's, the end is its peer group's.
        assert_eq!(
            frame(&partition, 0, &spec, |row| values[row], false),
            vec![0, 1]
        );
    }

    #[test]
    fn a_descending_range_offset_walks_the_other_way() {
        let values = [21.0f64, 20.0, 11.0, 10.0];
        let partition = one(&[21, 20, 11, 10]);
        let spec = FrameSpec {
            unit: FrameUnit::Range,
            start: Bound::CurrentRow,
            end: Bound::Offset {
                distance: 1,
                preceding: false,
            },
            exclude: FrameExclude::NoOthers,
        };
        assert_eq!(
            frame(&partition, 0, &spec, |row| Some(values[row]), true),
            vec![0, 1]
        );
    }

    #[test]
    fn an_offset_row_that_leaves_the_partition_has_no_answer() {
        let parts = partitions(4, |a, b| (a < 2) == (b < 2), |_, _| false, false);
        let second = &parts[1];
        assert_eq!(offset_row(second, 2, 1, true), None);
        assert_eq!(offset_row(second, 3, 1, true), Some(2));
        assert_eq!(offset_row(second, 3, 1, false), None);
    }

    #[test]
    fn partitions_split_where_the_key_changes() {
        let keys = [1i64, 1, 2, 2, 2];
        let parts = partitions(5, |a, b| keys[a] == keys[b], |_, _| false, false);
        assert_eq!(parts.len(), 2);
        assert_eq!((parts[0].start, parts[0].end), (0, 2));
        assert_eq!((parts[1].start, parts[1].end), (2, 5));
        assert_eq!(parts[1].len(), 3);
        assert!(!parts[1].is_empty());
    }

    #[test]
    fn an_inverted_frame_is_empty_rather_than_a_panic() {
        let partition = one(&[1, 2, 3]);
        let spec = FrameSpec {
            unit: FrameUnit::Rows,
            start: Bound::Offset {
                distance: 1,
                preceding: false,
            },
            end: Bound::Offset {
                distance: 1,
                preceding: true,
            },
            exclude: FrameExclude::NoOthers,
        };
        assert!(frame(&partition, 0, &spec, |_| Some(0.0), false).is_empty());
    }

    #[test]
    fn a_groups_offset_off_the_end_runs_to_the_edge_or_empties() {
        let partition = one(&[1, 2, 3]);
        let reaching = FrameSpec {
            unit: FrameUnit::Groups,
            start: Bound::Offset {
                distance: 9,
                preceding: true,
            },
            end: Bound::CurrentRow,
            exclude: FrameExclude::NoOthers,
        };
        // Nine groups back from the first is before the partition, so the
        // frame starts at its edge.
        assert_eq!(
            frame(&partition, 1, &reaching, |_| Some(0.0), false),
            vec![0, 1]
        );
        let emptying = FrameSpec {
            unit: FrameUnit::Groups,
            start: Bound::Offset {
                distance: 9,
                preceding: false,
            },
            end: Bound::CurrentRow,
            exclude: FrameExclude::NoOthers,
        };
        assert!(frame(&partition, 1, &emptying, |_| Some(0.0), false).is_empty());
    }

    #[test]
    fn a_range_offset_that_matches_nothing_empties_the_frame() {
        let values = [0.0f64, 1.0, 2.0];
        let partition = one(&[0, 1, 2]);
        let spec = FrameSpec {
            unit: FrameUnit::Range,
            start: Bound::Offset {
                distance: 100,
                preceding: false,
            },
            end: Bound::UnboundedFollowing,
            exclude: FrameExclude::NoOthers,
        };
        assert!(frame(&partition, 0, &spec, |row| Some(values[row]), false).is_empty());
    }
}
