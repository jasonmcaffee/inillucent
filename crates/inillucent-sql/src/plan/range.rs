//! Reading the `WHERE` terms that bound one index key column as a range.
//!
//! Invariant: **`low` and `high` are the two ends of the walk, not of the
//! value.** A column the index holds descending runs the other way, so `k > 5`
//! is where its walk starts rather than where it stops, and every bound read
//! here is placed by the column's direction before it is placed by the
//! operator.
//!
//! Here rather than in [`super`] because `plan.rs` is at its recorded size and
//! this loop is one question `index_candidate` asks of the key column after
//! its equality prefix (task-2078).

use super::*;

/// The range one index key column takes from the statement's terms.
pub(super) struct KeyRange {
    /// Where the walk starts, when a term bounds it.
    pub(super) low: Option<RangeBound>,
    /// Where the walk stops, when a term bounds it.
    pub(super) high: Option<RangeBound>,
    /// The collation the column is compared in.
    pub(super) collation: Collation,
    /// Whether the index holds the column descending.
    pub(super) descending: bool,
    /// The table column the key column is.
    pub(super) column: u16,
}

/// Returns the range the terms put on one key column, or `None` when nothing
/// bounds it.
///
/// Each term used is pushed onto `used`, so the caller can mark it consumed if
/// this candidate is chosen and leave it as a residual if not.
/// @param context - the term being planned and its statement
/// @param key_column - the index key column after the equality prefix
/// @param used - the terms this candidate has taken so far
pub(super) fn key_range(
    context: &CandidateContext<'_>,
    key_column: &crate::catalog_view::IndexColumnInfo,
    used: &mut Vec<usize>,
) -> Option<KeyRange> {
    let column = key_column.column?;
    let collation = collation_of(&key_column.collation);
    let mut low = None;
    let mut high = None;
    for (term_index, term) in context.terms.iter().enumerate() {
        let taken = context.consumed.get(term_index).copied().unwrap_or(false);
        if taken || used.contains(&term_index) {
            continue;
        }
        // An anchored pattern is a range; see `plan::pattern`, which also says
        // why the term is left as a residual (task-1932, M7).
        if let Some((low_bound, high_bound)) =
            pattern::pattern_range(context.id, column, term, collation, key_column.descending)
        {
            if low.is_none() && high.is_none() {
                low = low_bound;
                high = high_bound;
            }
            continue;
        }
        let Some((op, value)) = indexable_comparison(context.id, column, term) else {
            continue;
        };
        if !is_available(context.position, context.ids, &value)
            || comparison_collation(term) != collation
        {
            continue;
        }
        // Reading `k > 5` on a descending column as a low bound seeks past
        // every row it was meant to return. It did: `WHERE k > 5` on a
        // descending index returned nothing at all, silently, with no ORDER BY
        // anywhere near it.
        let Some((kind, at_low)) = walk_bound(op, key_column.descending) else {
            continue;
        };
        let slot = if at_low { &mut low } else { &mut high };
        if slot.is_none() {
            *slot = Some(RangeBound {
                kind,
                value,
                unconverted: compares_unconverted(term),
            });
            used.push(term_index);
        }
    }
    if low.is_none() && high.is_none() {
        return None;
    }
    Some(KeyRange {
        low,
        high,
        collation,
        descending: key_column.descending,
        column,
    })
}

/// Returns the bound a comparison puts on the walk, and whether it is the
/// walk's start, for a column held in the given direction.
/// @param op - the comparison, with the column on its left
/// @param descending - whether the index holds the column descending
fn walk_bound(op: BinaryOp, descending: bool) -> Option<(BoundKind, bool)> {
    Some(match (op, descending) {
        (BinaryOp::Greater, false) => (BoundKind::Greater, true),
        (BinaryOp::GreaterEqual, false) => (BoundKind::GreaterEqual, true),
        (BinaryOp::Less, false) => (BoundKind::Less, false),
        (BinaryOp::LessEqual, false) => (BoundKind::LessEqual, false),
        (BinaryOp::Greater, true) => (BoundKind::Less, false),
        (BinaryOp::GreaterEqual, true) => (BoundKind::LessEqual, false),
        (BinaryOp::Less, true) => (BoundKind::Greater, true),
        (BinaryOp::LessEqual, true) => (BoundKind::GreaterEqual, true),
        _ => return None,
    })
}
