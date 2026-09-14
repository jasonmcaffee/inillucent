//! Whether a query's `WHERE` proves a partial index's predicate.
//!
//! Invariant: **an index is chosen only when every row the query wants is in
//! it.** A partial index holds the rows its predicate accepted and no others,
//! so reading one for a query whose rows it does not all hold answers the
//! question with rows missing - and the rows missing are exactly the ones the
//! predicate excludes, which is the hardest kind of wrong answer to notice.
//! Every rule here is therefore sound on its own rather than likely, and a
//! rule nobody could write is an index left unchosen.
//!
//! Here rather than in [`super`] because `plan.rs` is at its recorded size and
//! these two functions are one question its caller asks once.

use super::*;

/// Reports whether a query's `WHERE` implies a partial index's predicate.
///
/// **Two rules, both sound, and nothing that needs a theorem prover.**
///
/// The first is the predicate appearing, unchanged, as a conjunct of the
/// statement's `WHERE`. So an index declared `WHERE b > 5` answers
/// `WHERE b > 5 AND a = 1` and does not answer `WHERE b > 6`, even though the
/// second implies the first. Proving the general implication is a theorem
/// prover in the planner, and every case it got wrong would be a query
/// silently missing exactly the rows the predicate excludes.
///
/// The second is `IS NOT NULL`, and it was measured rather than reasoned about
/// (task-1913). This function's comment used to call the verbatim rule
/// "SQLite's rule"; the pinned 3.53.4 answers `SELECT n FROM t WHERE n = 1`
/// with `SEARCH t USING COVERING INDEX t_n (n=?)` over an index declared
/// `WHERE n IS NOT NULL`, and this engine scanned the table. That index is how
/// SQLite spells "unique among the rows that have one", so the shape is
/// common and the whole point of declaring it was to be searched.
///
/// The rule added is the narrowest one that answers it: a comparison is three
/// valued, so `n = 1` is *true* only when `n` is not NULL - and the same holds
/// for `<`, `<=`, `>`, `>=` and `<>`. A conjunct comparing the operand the
/// predicate asks about therefore proves the predicate. `IS` and `IS NOT` are
/// deliberately not comparisons here: `n IS NULL` is true precisely when `n`
/// is NULL, so reading it as proof of the opposite would choose an index that
/// holds none of the rows the query wants.
///
/// `false` when the index's own predicate could not be bound, which is what
/// leaves an index the planner cannot reason about unchosen rather than chosen
/// on a guess.
///
/// @param computed - the index's bound expressions, when it has them
/// @param terms - the statement's `WHERE` conjuncts
pub(super) fn implies(computed: Option<&crate::dml::BoundIndexExprs>, terms: &[BoundExpr]) -> bool {
    let Some(held) = computed else {
        return false;
    };
    let Some(predicate) = held.predicate.as_ref() else {
        return false;
    };
    if terms.iter().any(|term| term == predicate) {
        return true;
    }
    let BoundExpr::IsNull {
        negated: true,
        operand,
    } = predicate
    else {
        return false;
    };
    terms
        .iter()
        .any(|term| compares_operand(term, operand.as_ref()))
}

/// Reports whether a conjunct is a comparison that one operand has to be
/// non-NULL to satisfy.
///
/// Only [`BoundExpr::Compare`], and only its six comparison operators. A
/// `Compare` holding an arithmetic operator is not a conjunct the binder
/// produces, and the match is written out rather than defaulted so that an
/// operator added later is a compilation error rather than a wrong answer.
///
/// @param term - one conjunct of the statement's `WHERE`
/// @param operand - the expression the index's predicate asks about
pub(super) fn compares_operand(term: &BoundExpr, operand: &BoundExpr) -> bool {
    let BoundExpr::Compare {
        op, left, right, ..
    } = term
    else {
        return false;
    };
    if !matches!(
        op,
        BinaryOp::Equal
            | BinaryOp::NotEqual
            | BinaryOp::Less
            | BinaryOp::LessEqual
            | BinaryOp::Greater
            | BinaryOp::GreaterEqual
    ) {
        return false;
    }
    left.as_ref() == operand || right.as_ref() == operand
}
