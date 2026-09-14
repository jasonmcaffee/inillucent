//! Reading one `WHERE` term as a comparison against one column.
//!
//! Invariant: **a term is read as a comparison only when it is one, and the
//! side the column is on decides the operator.** `5 < k` and `k > 5` are the
//! same constraint on `k`, and mirroring one into the other here is what stops
//! every caller having to ask twice.
//!
//! Here rather than in [`super`] because `plan.rs` is three thousand lines and
//! these four functions are one idea that three of its callers share.

use super::*;

/// Returns the collation a comparison uses, or BINARY.
pub(super) fn comparison_collation(term: &BoundExpr) -> Collation {
    match term {
        BoundExpr::Compare { collation, .. } => *collation,
        _ => Collation::Binary,
    }
}

/// Returns the collation a folded name spells.
pub(super) fn collation_of(name: &[u8]) -> Collation {
    Collation::from_name(core::str::from_utf8(name).unwrap_or("BINARY"))
        .unwrap_or(Collation::Binary)
}

/// Returns the operator and the other side when a term compares one column of
/// one source against something else.
///
/// **A comparison against a NULL literal is not one (task-1932, found by
/// `tlp_differential.rs`).** `d = NULL`, `a > NULL` and every other ordinary
/// comparison against NULL is NULL for every row, so a `WHERE` keeps nothing -
/// but read as a constraint on `d` it became a seek to the index's own NULL
/// entries and answered every row whose `d` is NULL. On a six-hundred-row table
/// `SELECT count(*) FROM t WHERE d = NULL` answered 85 where a scan applying
/// the same predicate answers 0.
///
/// `IS NULL` is unaffected: it binds to `BoundExpr::IsNull` rather than to a
/// comparison, so it never reaches here and still seeks the index's NULL
/// entries, which is the right plan for it.
pub(super) fn comparison_against_column(
    position: usize,
    column: u16,
    term: &BoundExpr,
) -> Option<(BinaryOp, BoundExpr)> {
    let BoundExpr::Compare {
        op, left, right, ..
    } = term
    else {
        return None;
    };
    if compares_with_null(left, right) {
        return None;
    }
    if let BoundExpr::Column {
        source,
        column: candidate,
        ..
    } = left.as_ref()
    {
        if *source == position && *candidate == column {
            return Some((*op, right.as_ref().clone()));
        }
    }
    if let BoundExpr::Column {
        source,
        column: candidate,
        ..
    } = right.as_ref()
    {
        if *source == position && *candidate == column {
            return Some((mirror(*op), left.as_ref().clone()));
        }
    }
    None
}

/// Returns the operator and the other side when a term compares a rowid.
///
/// A NULL operand is refused here for the same reason as in
/// [`comparison_against_column`]: `a > NULL` is NULL for every row, and read as
/// a bound on the rowid it became a seek over the whole table - 600 rows where
/// a scan applying the same predicate answers 0.
pub(super) fn comparison_against_rowid(
    position: usize,
    term: &BoundExpr,
) -> Option<(BinaryOp, BoundExpr)> {
    let BoundExpr::Compare {
        op, left, right, ..
    } = term
    else {
        return None;
    };
    if compares_with_null(left, right) {
        return None;
    }
    if matches!(left.as_ref(), BoundExpr::Rowid { source } if *source == position) {
        return Some((*op, right.as_ref().clone()));
    }
    if matches!(right.as_ref(), BoundExpr::Rowid { source } if *source == position) {
        return Some((mirror(*op), left.as_ref().clone()));
    }
    None
}

/// Returns the operator that means the same thing with its operands swapped.
fn mirror(op: BinaryOp) -> BinaryOp {
    match op {
        BinaryOp::Less => BinaryOp::Greater,
        BinaryOp::LessEqual => BinaryOp::GreaterEqual,
        BinaryOp::Greater => BinaryOp::Less,
        BinaryOp::GreaterEqual => BinaryOp::LessEqual,
        other => other,
    }
}

/// Reports whether a comparison has a NULL literal on either side.
///
/// Only `BoundExpr::Compare` reaches here, and every operator it can carry is
/// three-valued - `IS` and `IS NULL` are their own bound expressions and take a
/// different path - so a NULL operand means the term answers NULL for every
/// row and selects nothing. No seek may be built from it.
///
/// @param left - the comparison's left side
/// @param right - its right side
fn compares_with_null(left: &BoundExpr, right: &BoundExpr) -> bool {
    matches!(left, BoundExpr::Null) || matches!(right, BoundExpr::Null)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Mirroring a comparison keeps its meaning when the operands swap.
    #[test]
    fn mirroring_preserves_meaning() {
        assert_eq!(mirror(BinaryOp::Less), BinaryOp::Greater);
        assert_eq!(mirror(BinaryOp::GreaterEqual), BinaryOp::LessEqual);
        assert_eq!(mirror(BinaryOp::Equal), BinaryOp::Equal);
    }
}
