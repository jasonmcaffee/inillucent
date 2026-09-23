//! Reading one `WHERE` term as a comparison against one column.
//!
//! Invariant: **a term is read as a comparison only when it is one, and the
//! side the column is on decides the operator.** `5 < k` and `k > 5` are the
//! same constraint on `k`, and mirroring one into the other here is what stops
//! every caller having to ask twice.
//!
//! Here rather than in [`super`] because `plan.rs` is three thousand lines and
//! these seven functions are one idea that three of its callers share.

use super::*;
use inillucent_value::Affinity;

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

/// Returns the operator and the other side when a term compares one column of
/// one source and an index on that column may answer it.
///
/// **An index answers a comparison only when the comparison's affinity agrees
/// with the column's (task-2083).** This is SQLite's `sqlite3IndexAffinityOk`.
/// The index holds each value as the column's affinity stored it, and a seek
/// compares without converting the stored side. The comparison in `WHERE`
/// converts both sides. When the comparison is numeric and the column is
/// untyped or TEXT, the stored `'3'` equals the number 3 in `WHERE` and is
/// nowhere near it in the tree. `SELECT count(*) FROM s, h WHERE h.a = s.k`
/// with `h.a` untyped holding `'3'` and `s.k INTEGER` holding 3 answered 0
/// where SQLite answers 1, because SQLite does not use the index there and
/// this planner did.
///
/// A virtual table's constraints still go through [`comparison_against_column`]
/// unfiltered, because a module compares the values itself rather than seeking
/// a tree this engine wrote.
///
/// @param position - the source the column belongs to
/// @param column - the column the index holds
/// @param term - one `WHERE` conjunct
pub(super) fn indexable_comparison(
    position: usize,
    column: u16,
    term: &BoundExpr,
) -> Option<(BinaryOp, BoundExpr)> {
    let found = comparison_against_column(position, column, term)?;
    let BoundExpr::Compare {
        left,
        right,
        affinity,
        ..
    } = term
    else {
        return None;
    };
    let column_affinity =
        [left.as_ref(), right.as_ref()]
            .into_iter()
            .find_map(|side| match side {
                BoundExpr::Column {
                    source,
                    column: candidate,
                    affinity,
                    ..
                } if *source == position && *candidate == column => Some(*affinity),
                _ => None,
            })?;
    index_affinity_ok(*affinity, column_affinity).then_some(found)
}

/// Reports whether an index on a column of one affinity may answer a
/// comparison that applies another.
///
/// A comparison that converts nothing may use any index. A TEXT comparison
/// needs a TEXT column, and a numeric one needs a numeric column, because only
/// then are the values in the tree already what the comparison converts to.
///
/// @param comparison - the affinity the comparison applies, `None` for none
/// @param column - the affinity of the indexed column
pub(super) fn index_affinity_ok(comparison: Option<Affinity>, column: Affinity) -> bool {
    match comparison {
        None | Some(Affinity::Blob) => true,
        Some(Affinity::Text) => column == Affinity::Text,
        Some(_) => column.is_numeric(),
    }
}

/// Reports whether a seek built from this term compares its probe value
/// without converting it to the indexed column's affinity.
///
/// SQLite's rule from `codeAllEqualityTerms`: the probe takes the column's
/// affinity unless both sides of the comparison have an affinity and neither
/// is numeric. The binder already worked that out when it gave the comparison
/// no affinity at all, so this reads it from there. An indexed column always
/// has an affinity, so a comparison against one that applies none is always
/// that case.
///
/// @param term - the `WHERE` conjunct the seek was built from
pub(super) fn compares_unconverted(term: &BoundExpr) -> bool {
    matches!(term, BoundExpr::Compare { affinity: None, .. })
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

    /// SQLite's `sqlite3IndexAffinityOk`: a comparison that converts nothing
    /// may use any index, a TEXT one needs a TEXT column, a numeric one needs
    /// a numeric column.
    #[test]
    fn an_index_answers_only_a_comparison_its_affinity_agrees_with() {
        for column in [
            Affinity::Blob,
            Affinity::Text,
            Affinity::Numeric,
            Affinity::Integer,
            Affinity::Real,
        ] {
            assert!(index_affinity_ok(None, column));
            assert!(index_affinity_ok(Some(Affinity::Blob), column));
        }
        assert!(index_affinity_ok(Some(Affinity::Text), Affinity::Text));
        assert!(!index_affinity_ok(Some(Affinity::Text), Affinity::Blob));
        assert!(!index_affinity_ok(Some(Affinity::Text), Affinity::Integer));
        assert!(index_affinity_ok(Some(Affinity::Numeric), Affinity::Real));
        assert!(!index_affinity_ok(Some(Affinity::Numeric), Affinity::Blob));
        assert!(!index_affinity_ok(Some(Affinity::Numeric), Affinity::Text));
    }

    /// Mirroring a comparison keeps its meaning when the operands swap.
    #[test]
    fn mirroring_preserves_meaning() {
        assert_eq!(mirror(BinaryOp::Less), BinaryOp::Greater);
        assert_eq!(mirror(BinaryOp::GreaterEqual), BinaryOp::LessEqual);
        assert_eq!(mirror(BinaryOp::Equal), BinaryOp::Equal);
    }
}
