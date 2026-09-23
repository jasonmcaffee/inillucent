//! Which collation a comparison, a sort or a grouping uses, and where an
//! explicit `COLLATE` comes from.
//!
//! Invariant: **every collation the binder decides comes from the rules in
//! this file, and they are SQLite's `sqlite3ExprCollSeq` and
//! `sqlite3BinaryCompareCollSeq`, graded against the pinned 3.53.4 shell.**
//! The executor's `expression_collation` used to be a second copy of them
//! that looked only at the top node, and the binder's own two helpers did the
//! same, so an explicit `COLLATE` inside an operand of `||` reached neither a
//! comparison nor a `GROUP BY` (task-2089). One copy, here, is what keeps a
//! comparison, an `ORDER BY`, a `DISTINCT` and a `PARTITION BY` agreeing
//! about which values are equal. The cases that grade these rules are
//! `crates/inillucent-compat/tests/corpora/differential-part8/task2089.cases`.

use inillucent_value::{Affinity, Collation};

use super::BoundExpr;
use crate::ast::UnaryOp;

impl BoundExpr {
    /// Returns the collation this expression carries, if it has one.
    ///
    /// SQLite's `sqlite3ExprCollSeq`, in its order: a column has its declared
    /// collation, a `CAST` and a unary `+` have their operand's, and any other
    /// expression has the explicit collation of an operand, if one has one.
    /// So `CAST(n AS TEXT) = 'A'` on a NOCASE column `n` compares with NOCASE,
    /// and `n || '' = 'A'` compares with BINARY. Measured against 3.53.4
    /// (task-2089): `CAST(n AS TEXT) = 'A'` and `+n = 'A'` answered 0 here
    /// where SQLite answers 1.
    pub fn collation(&self) -> Option<Collation> {
        match self {
            BoundExpr::Column { collation, .. } => Some(*collation),
            BoundExpr::Cast { operand, .. }
            | BoundExpr::Unary {
                op: UnaryOp::Identity,
                operand,
            } => operand.collation(),
            other => other.explicit_collation(),
        }
    }

    /// Returns the collation an explicit `COLLATE` forced on this expression.
    ///
    /// This is *not* the same question as [`BoundExpr::collation`]. A column
    /// declared `COLLATE NOCASE` has an implicit collation; `x COLLATE BINARY`
    /// has an explicit one, and an explicit collation on either side of a
    /// comparison beats an implicit one on the other side.
    ///
    /// **An explicit collation reaches up through every operator and function
    /// argument (task-2089).** SQLite marks a node `EP_Collate` when any
    /// operand has the mark, and reads the collation from the first operand
    /// that has it, left first. This used to look only at the top node, so
    /// `('a' COLLATE NOCASE || 'x') = 'AX'` compared with BINARY and answered
    /// 0 where 3.53.4 answers 1, and `'a' COLLATE BINARY || 'b' COLLATE
    /// NOCASE` has to answer BINARY because the left operand is asked first.
    /// [`BoundExpr::children`] lists operands in SQLite's order for every node
    /// whose value is text. A scalar subquery has no children here, and SQLite
    /// does not carry a `COLLATE` out of one either.
    pub fn explicit_collation(&self) -> Option<Collation> {
        match self {
            BoundExpr::Collate { collation, .. } => Some(*collation),
            other => other
                .children()
                .into_iter()
                .find_map(BoundExpr::explicit_collation),
        }
    }
}

/// Returns the collation a result column compares with.
///
/// `DISTINCT` and `GROUP BY` compare result values, and a NOCASE column makes
/// `blue` and `Blue` the same value for both. Comparing them with BINARY
/// instead returns more rows than SQLite does, which looks like a duplicate
/// rather than like a bug.
pub fn result_collation(expr: &BoundExpr) -> Collation {
    expr.explicit_collation()
        .or_else(|| expr.collation())
        .unwrap_or(Collation::Binary)
}

/// Returns the affinity and collation a comparison between two operands uses.
///
/// SQLite's rule, in order: if either side has a column affinity the comparison
/// applies it, with the left side winning. The collation is an explicit one on
/// the left operand, then an explicit one on the right, then the left
/// operand's implicit one, then the right's, and otherwise BINARY. "On an
/// operand" includes anywhere inside it: see [`BoundExpr::explicit_collation`].
///
/// @param left - the comparison's left operand
/// @param right - the comparison's right operand
pub fn comparison_rules(left: &BoundExpr, right: &BoundExpr) -> (Option<Affinity>, Collation) {
    let affinity = match (left.affinity(), right.affinity()) {
        (Some(left), Some(right)) => inillucent_value::compare::comparison_affinity(left, right),
        (Some(left), None) => Some(left),
        (None, Some(right)) => Some(right),
        (None, None) => None,
    };
    let collation = left
        .explicit_collation()
        .or_else(|| right.explicit_collation())
        .or_else(|| left.collation())
        .or_else(|| right.collation())
        .unwrap_or(Collation::Binary);
    (affinity, collation)
}

/// Wraps an expression in the collation an explicit `COLLATE` names.
///
/// **A `COLLATE` above a comparison does not reach the comparison (task-1979,
/// F5).** `a = b COLLATE NOCASE` parses as `a = (b COLLATE NOCASE)`, because
/// `COLLATE` binds tighter than `=`, and the comparison then reads NOCASE off
/// its own right operand through [`comparison_rules`]. `(a = b) COLLATE
/// NOCASE` is the other tree: the comparison is finished and NOCASE applies to
/// the integer it produced, where a text collation does nothing. This function
/// used to stamp the collation onto a `BoundExpr::Compare` it was handed, which
/// made the two trees answer the same and made the outer name win over the
/// inner one: measured against 3.53.4, `SELECT ('B'<'a') COLLATE NOCASE`
/// answered 0 where SQLite answers 1, and
/// `SELECT ('a' = 'A' COLLATE NOCASE) COLLATE BINARY` answered 0 where SQLite
/// answers 1 because the inner NOCASE is the comparison's and the outer BINARY
/// is the result's.
///
/// The wrapper is what carries the collation onward: [`comparison_rules`] asks
/// an operand for its [`BoundExpr::explicit_collation`], so a `COLLATE` on a
/// literal still reaches the comparison that uses it.
///
/// @param expr - the operand the `COLLATE` was written on
/// @param collation - the collation it names
pub(super) fn apply_collation(expr: BoundExpr, collation: Collation) -> BoundExpr {
    BoundExpr::Collate {
        operand: Box::new(expr),
        collation,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::BinaryOp;

    /// A text literal.
    ///
    /// @param bytes - the literal's text
    fn text(bytes: &[u8]) -> BoundExpr {
        BoundExpr::Text(bytes.to_vec())
    }

    /// An explicit `COLLATE` on an expression.
    ///
    /// @param operand - the expression the `COLLATE` is written on
    /// @param collation - the collation it names
    fn collate(operand: BoundExpr, collation: Collation) -> BoundExpr {
        apply_collation(operand, collation)
    }

    /// `left || right`.
    ///
    /// @param left - the left operand
    /// @param right - the right operand
    fn concat(left: BoundExpr, right: BoundExpr) -> BoundExpr {
        BoundExpr::Arithmetic {
            op: BinaryOp::Concat,
            left: Box::new(left),
            right: Box::new(right),
        }
    }

    /// A TEXT column declared with a collation.
    ///
    /// @param collation - the column's declared collation
    fn column(collation: Collation) -> BoundExpr {
        BoundExpr::Column {
            source: 0,
            column: 0,
            slot: 0,
            affinity: Affinity::Text,
            collation,
        }
    }

    /// The collation a comparison between two operands uses.
    ///
    /// @param left - the left operand
    /// @param right - the right operand
    fn compared_with(left: &BoundExpr, right: &BoundExpr) -> Collation {
        comparison_rules(left, right).1
    }

    /// An explicit `COLLATE` inside an operand of `||` reaches the comparison
    /// around it (task-2089).
    ///
    /// `'B' = 'b' || '' COLLATE NOCASE` parses as `'B' = ('b' || ('' COLLATE
    /// NOCASE))`, and 3.53.4 answers 1. The helpers used to look only at the
    /// top node, which here is the `||`, and the comparison used BINARY.
    #[test]
    fn a_collate_inside_a_concatenation_reaches_the_comparison() {
        let right = concat(text(b"b"), collate(text(b""), Collation::NoCase));
        assert_eq!(compared_with(&text(b"B"), &right), Collation::NoCase);
        let left = concat(collate(text(b"a"), Collation::NoCase), text(b"x"));
        assert_eq!(compared_with(&left, &text(b"AX")), Collation::NoCase);
    }

    /// Of two operands that both carry a `COLLATE`, the left one wins, in a
    /// concatenation and in a comparison.
    ///
    /// 3.53.4 answers `('a' COLLATE NOCASE || 'b' COLLATE BINARY) = 'AB'` with
    /// 1 and the swapped form with 0, and `('AX' COLLATE BINARY) = ('a'
    /// COLLATE NOCASE || 'x')` with 0.
    #[test]
    fn the_left_operand_with_a_collate_wins() {
        let nocase_first = concat(
            collate(text(b"a"), Collation::NoCase),
            collate(text(b"b"), Collation::Binary),
        );
        assert_eq!(nocase_first.explicit_collation(), Some(Collation::NoCase));
        let binary_first = concat(
            collate(text(b"a"), Collation::Binary),
            collate(text(b"b"), Collation::NoCase),
        );
        assert_eq!(binary_first.explicit_collation(), Some(Collation::Binary));
        let nocase_right = concat(collate(text(b"a"), Collation::NoCase), text(b"x"));
        let binary_left = collate(text(b"AX"), Collation::Binary);
        assert_eq!(
            compared_with(&binary_left, &nocase_right),
            Collation::Binary
        );
    }

    /// A column's declared collation passes through `CAST` and unary `+`, and
    /// not through `||` or unary `-`.
    ///
    /// This is `sqlite3ExprCollSeq`: it steps into the operand of TK_CAST and
    /// TK_UPLUS, and into any other operator only when an operand carries an
    /// explicit `COLLATE`. 3.53.4 answers `CAST(n AS TEXT) = 'A'` and
    /// `+n = 'A'` with 1 and `n || '' = 'A'` with 0 on a NOCASE column `n`.
    #[test]
    fn a_column_collation_passes_through_cast_and_unary_plus_only() {
        let cast = BoundExpr::Cast {
            operand: Box::new(column(Collation::NoCase)),
            affinity: Affinity::Text,
        };
        assert_eq!(cast.collation(), Some(Collation::NoCase));
        let plus = BoundExpr::Unary {
            op: UnaryOp::Identity,
            operand: Box::new(column(Collation::NoCase)),
        };
        assert_eq!(plus.collation(), Some(Collation::NoCase));
        let minus = BoundExpr::Unary {
            op: UnaryOp::Negate,
            operand: Box::new(column(Collation::NoCase)),
        };
        assert_eq!(minus.collation(), None);
        let joined = concat(column(Collation::NoCase), text(b""));
        assert_eq!(joined.collation(), None);
        assert_eq!(compared_with(&joined, &text(b"A")), Collation::Binary);
    }

    /// A `COLLATE` written over a finished expression is the one that counts.
    ///
    /// 3.53.4 answers `('a' COLLATE NOCASE || 'x') COLLATE BINARY = 'AX'`
    /// with 0: the walk stops at the outer `COLLATE`.
    #[test]
    fn an_outer_collate_beats_one_inside_it() {
        let inner = concat(collate(text(b"a"), Collation::NoCase), text(b"x"));
        let outer = collate(inner, Collation::Binary);
        assert_eq!(compared_with(&outer, &text(b"AX")), Collation::Binary);
    }
}
