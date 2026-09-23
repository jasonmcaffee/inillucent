//! The collation rules' own tests, which need no shell.
//!
//! Invariant: **each test builds the bound tree a statement would bind to and
//! asserts the collation SQLite 3.53.4 uses for it**, with the statement and
//! the shell's answer in its doc comment, so a failure names a rule rather than
//! a query somebody has to replay. The differential cases in
//! `crates/inillucent-compat/tests/corpora/differential-part8/task2089.cases`
//! and `task2094.cases` grade the same rules end to end.
//!
//! Split out of `collation.rs` in task-2094, which added aggregate and window
//! references to the rules and took the file past the size `policy.rs` records
//! for it. A child module sees everything the parent holds privately, so
//! nothing had to be made more visible.

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

/// An aggregate or window reference answers with the explicit collation
/// its arguments carry, and a comparison around it uses that collation
/// (task-2094).
///
/// 3.53.4 answers `max(s COLLATE NOCASE) = 'C'` with 1. The reference
/// used to have no children and no field, so the comparison used BINARY.
#[test]
fn an_aggregate_reference_carries_its_arguments_collate() {
    let arguments = [text(b","), collate(text(b"s"), Collation::NoCase)];
    let explicit = explicit_argument_collation(&arguments);
    assert_eq!(explicit, Some(Collation::NoCase));
    let aggregate = BoundExpr::Aggregate {
        slot: 0,
        collation: explicit,
    };
    assert_eq!(compared_with(&aggregate, &text(b"C")), Collation::NoCase);
    let window = BoundExpr::WindowRef {
        slot: 0,
        collation: None,
    };
    assert_eq!(
        compared_with(&window, &column(Collation::NoCase)),
        Collation::NoCase
    );
    assert_eq!(
        explicit_argument_collation(&[column(Collation::NoCase)]),
        None
    );
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
