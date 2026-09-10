//! The automatic index: what `PRAGMA automatic_index` turns on and off.
//!
//! Invariant: an automatic index changes how fast a join runs and never what it
//! answers. The rule below is deliberately narrow for that reason - it fires
//! only on a conjunction of plain equalities, each with one side reading only
//! outer columns and the other only inner ones, and it declines the moment a
//! condition has anything else in it. A residual predicate that the hash key
//! did not capture would have to be re-tested per pair, and an optimisation
//! that half-remembers a predicate is a wrong answer rather than a slow one.
//!
//! # Why a hash table rather than a b-tree
//!
//! SQLite calls this an *automatic index* because that is what it builds: a
//! transient b-tree over the inner table, keyed on the join column, thrown away
//! when the statement ends. It builds a tree because its executor has one join
//! operator - a nested loop - and the only way to make the inner side cheap is
//! to give it something to seek in.
//!
//! This engine has a hash join, so the same idea costs less here: the inner
//! side is read once into a hash table keyed on the join expression, and each
//! outer row probes it. Both are "build a throwaway structure over the inner
//! side so the join stops being quadratic", both are under one switch, and the
//! switch means the same thing to an application - which is what
//! `PRAGMA automatic_index = off` is asking about.
//!
//! Without it, an unindexed inner side is read once into a vector and every
//! outer row walks the whole vector, which is what this engine used to always
//! do and is what `off` still selects.

use crate::expr::Expr;

/// The two halves of an equi-join condition, ready to compile.
pub struct EquiKeys {
    /// The key expressions over the outer row, which probe the table.
    pub probe: Vec<Expr>,
    /// The key expressions over the inner row, which build it.
    ///
    /// Already rebased: an inner column that is column `offset + n` of the
    /// joined row is column `n` here, because the build side is pushed in as
    /// batches of the inner stage alone.
    pub build: Vec<Expr>,
}

/// Returns the equi-join keys a condition offers, or nothing if it offers none.
///
/// **Nothing is a perfectly good answer**, and it is the answer for most
/// conditions: a range, an `OR`, a call, a correlated subquery, or an equality
/// whose two sides are not cleanly one per side. The caller then builds the
/// nested loop it would have built anyway.
///
/// @param condition - the `ON` condition, over the joined row's columns
/// @param offset - the first column of the inner stage in the joined row
/// @param width - how many columns the inner stage has
pub fn equi_keys(condition: &Expr, offset: usize, width: usize) -> Option<EquiKeys> {
    let mut terms = Vec::new();
    flatten_and(condition, &mut terms);
    let mut keys = EquiKeys {
        probe: Vec::new(),
        build: Vec::new(),
    };
    for term in terms {
        // Both comparison shapes are accepted: the plain one, and the one that
        // carries an affinity. The affinity is not ignored - it is applied to
        // *both* key expressions, so the two sides of the hash table are hashed
        // after the same conversion the comparison would have made. Ignoring it
        // would put values that compare equal into two buckets, which drops
        // rows; and `t.a = u.b` between two `INTEGER` columns carries one, so
        // declining on it would have declined the whole case this exists for.
        //
        // A **collation** is a different matter and is still declined. The keys
        // are byte-encoded, and `NOCASE` equality is not byte equality; keying
        // it would need the fold in the encoder, which is a change to the key
        // format rather than to this rule.
        let (left, right, affinity) = match term {
            Expr::Compare(crate::expr::CompareOp::Equal, left, right) => (left, right, None),
            Expr::CompareWith {
                op: crate::expr::CompareOp::Equal,
                affinity,
                collation: inillucent_value::Collation::Binary,
                left,
                right,
            } => (left, right, *affinity),
            _ => return None,
        };
        let left_side = side_of(left, offset, width)?;
        let right_side = side_of(right, offset, width)?;
        match (left_side, right_side) {
            (Side::Outer, Side::Inner) => {
                keys.probe.push(converted((**left).clone(), affinity));
                keys.build
                    .push(converted(rebased(right, offset)?, affinity));
            }
            (Side::Inner, Side::Outer) => {
                keys.probe.push(converted((**right).clone(), affinity));
                keys.build.push(converted(rebased(left, offset)?, affinity));
            }
            // A term reading one side only is a filter rather than a join key,
            // and a term reading neither is a constant. Either would have to be
            // tested somewhere, and this operator has nowhere to test it.
            _ => return None,
        }
    }
    (!keys.probe.is_empty()).then_some(keys)
}

/// Wraps a key expression in the conversion the comparison would have applied.
///
/// @param expr - the key expression
/// @param affinity - the comparison's affinity, when it has one
fn converted(expr: Expr, affinity: Option<inillucent_value::Affinity>) -> Expr {
    match affinity {
        None => expr,
        Some(affinity) => Expr::Affinity {
            operand: Box::new(expr),
            affinity,
        },
    }
}

/// Which side of the join an expression reads.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Side {
    /// Only columns the stages before this one produced.
    Outer,
    /// Only this stage's own columns.
    Inner,
}

/// Returns which side an expression reads, or nothing when it reads both.
///
/// An expression reading no column at all is a constant, and a constant is not
/// a join key - reporting it as one would put every row in one hash bucket.
///
/// @param expr - the expression
/// @param offset - the first column of the inner stage
/// @param width - how many columns the inner stage has
fn side_of(expr: &Expr, offset: usize, width: usize) -> Option<Side> {
    let mut columns = Vec::new();
    collect_columns(expr, &mut columns)?;
    if columns.is_empty() {
        return None;
    }
    let outer = columns.iter().all(|at| *at < offset);
    let inner = columns
        .iter()
        .all(|at| *at >= offset && *at < offset.saturating_add(width));
    match (outer, inner) {
        (true, _) => Some(Side::Outer),
        (_, true) => Some(Side::Inner),
        _ => None,
    }
}

/// Returns the expression with its inner column numbers rebased from zero.
///
/// @param expr - an expression reading only inner columns
/// @param offset - the first column of the inner stage
fn rebased(expr: &Expr, offset: usize) -> Option<Expr> {
    Some(match expr {
        Expr::Column(at) => Expr::Column(at.checked_sub(offset)?),
        Expr::Literal(value) => Expr::Literal(value.clone()),
        other => {
            // Only the two leaf shapes are rebased, which is why anything else
            // declines. A general rewriter over every node would be a second
            // place expression shapes are enumerated, and the whole value of
            // this optimisation is in the plain `t.a = u.b` case anyway.
            let _ = other;
            return None;
        }
    })
}

/// Collects every column an expression reads, or reports it cannot be walked.
///
/// The walk is deliberately shallow: `Column`, `Literal` and the comparison and
/// boolean shapes above them. Anything else - a call, a cast, a subquery, an
/// arithmetic node - returns `None`, which declines the optimisation rather
/// than guessing at what it reads. Guessing wrong here means dropping rows.
///
/// @param expr - the expression
/// @param out - where the column numbers go
fn collect_columns(expr: &Expr, out: &mut Vec<usize>) -> Option<()> {
    match expr {
        Expr::Column(at) => {
            out.push(*at);
            Some(())
        }
        Expr::Literal(_) => Some(()),
        _ => None,
    }
}

/// Splits a conjunction into its terms.
///
/// @param expr - the condition
/// @param out - where the terms go, in written order
fn flatten_and<'e>(expr: &'e Expr, out: &mut Vec<&'e Expr>) {
    if let Expr::And(left, right) = expr {
        flatten_and(left, out);
        flatten_and(right, out);
        return;
    }
    out.push(expr);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::expr::CompareOp;

    /// Returns `left = right` as the planner would build it.
    fn equal(left: Expr, right: Expr) -> Expr {
        Expr::Compare(CompareOp::Equal, Box::new(left), Box::new(right))
    }

    /// The plain case: one equality with a column on each side.
    #[test]
    fn one_column_each_side_is_a_key() {
        let condition = equal(Expr::Column(0), Expr::Column(3));
        let keys = equi_keys(&condition, 3, 2).expect("an equi-join");
        assert_eq!(keys.probe.len(), 1);
        assert_eq!(keys.build.len(), 1);
        // The build key is rebased to the inner stage's own numbering.
        assert!(matches!(keys.build.first(), Some(Expr::Column(0))));
    }

    /// Written the other way round, which is the same join.
    #[test]
    fn the_sides_may_be_written_either_way_round() {
        let condition = equal(Expr::Column(3), Expr::Column(0));
        let keys = equi_keys(&condition, 3, 2).expect("an equi-join");
        assert!(matches!(keys.probe.first(), Some(Expr::Column(0))));
        assert!(matches!(keys.build.first(), Some(Expr::Column(0))));
    }

    /// Two equalities are a composite key rather than a reason to decline.
    #[test]
    fn a_conjunction_of_equalities_is_a_composite_key() {
        let condition = Expr::And(
            Box::new(equal(Expr::Column(0), Expr::Column(3))),
            Box::new(equal(Expr::Column(1), Expr::Column(4))),
        );
        let keys = equi_keys(&condition, 3, 2).expect("an equi-join");
        assert_eq!(keys.probe.len(), 2);
    }

    /// Anything that is not a clean equality declines, because a residual this
    /// operator cannot test is a dropped row rather than a slow join.
    #[test]
    fn anything_else_declines() {
        // A term reading only the outer side is a filter, not a key.
        let filter = equal(Expr::Column(0), Expr::Column(1));
        assert!(equi_keys(&filter, 3, 2).is_none());
        // A range is not an equality.
        let range = Expr::Compare(
            CompareOp::Less,
            Box::new(Expr::Column(0)),
            Box::new(Expr::Column(3)),
        );
        assert!(equi_keys(&range, 3, 2).is_none());
        // One good term and one bad one declines the whole condition.
        let mixed = Expr::And(
            Box::new(equal(Expr::Column(0), Expr::Column(3))),
            Box::new(range),
        );
        assert!(equi_keys(&mixed, 3, 2).is_none());
    }
}
