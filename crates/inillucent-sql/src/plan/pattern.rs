//! What range an anchored `LIKE` or `GLOB` selects.
//!
//! Invariant: **a range is offered only when it cannot exclude a row the
//! pattern matches.** The pattern stays as a residual either way, so the bounds
//! have to be a superset of the matches and nothing more is asked of them - but
//! a case-sensitive range over a case-insensitive index is not a superset, and
//! that is the one way to get this wrong.
//!
//! Here rather than in [`super`] because `plan.rs` is three thousand lines and
//! these three functions are one idea: the prefix, the key just past it, and
//! the pairing rule that decides whether either may be used.
//!
//! **The term is kept as a residual whichever way this answers.** A range over
//! the prefix admits keys the pattern's own wildcards refuse - `k LIKE 'abc%d'`
//! seeks `abc` to `abd` and still has to test every key it finds - so the
//! caller narrows the walk and tests the pattern on what the walk produces.
//! That is also what makes a bound that is merely a superset correct.
//!
//! `k LIKE 'abc%'` and `k GLOB 'abc*'` select exactly the keys from `abc` up to
//! but not including `abd`, which is a seek. Before task-1932 (M7) the planner
//! matched `BoundExpr::Compare` only and a pattern binds to
//! `BoundExpr::Pattern`, so every prefix query on an indexed column read the
//! whole table.

use super::*;

/// Returns the range an anchored `LIKE` or `GLOB` on one column selects.
///
/// **Only when the pattern's case sensitivity matches the index's**, which is
/// the rule SQLite applies and the reason it is not simply "a literal prefix".
/// `GLOB` compares exactly, so it needs a `BINARY` index column; `LIKE` folds
/// ASCII case by default, so it needs a `NOCASE` one - measured against the
/// pinned 3.53.4, which answers `SEARCH ... USING INDEX` for exactly those two
/// pairings and `SCAN` for the other two.
///
/// `PRAGMA case_sensitive_like = ON` makes `LIKE` exact and would admit a
/// `BINARY` column as well. That pairing is deliberately not planned here: the
/// pragma is a fact about the connection a statement is compiled *for*, and the
/// planner is handed a statement and a set of levers rather than a catalog. The
/// consequence is a scan where a seek was possible, not a wrong answer.
///
/// @param id - the source the column belongs to
/// @param column - the index's key column
/// @param term - the `WHERE` term being read
/// @param collation - the index column's collation
/// @param descending - whether the index holds this column descending
pub(super) fn pattern_range(
    id: usize,
    column: u16,
    term: &BoundExpr,
    collation: Collation,
    descending: bool,
) -> Option<(Option<RangeBound>, Option<RangeBound>)> {
    let BoundExpr::Pattern {
        negated,
        op,
        operand,
        pattern,
        escape,
    } = term
    else {
        return None;
    };
    if *negated || escape.is_some() {
        return None;
    }
    let exact = match op {
        crate::ast::PatternOp::Glob => true,
        crate::ast::PatternOp::Like => false,
        // `REGEXP` and `MATCH` are whatever a registered function does with
        // them, so no prefix can be read out of either.
        _ => return None,
    };
    match (exact, collation) {
        (true, Collation::Binary) => {}
        (false, Collation::NoCase) => {}
        _ => return None,
    }
    let BoundExpr::Column {
        source,
        column: candidate,
        ..
    } = operand.as_ref()
    else {
        return None;
    };
    if *source != id || *candidate != column {
        return None;
    }
    let BoundExpr::Text(text) = pattern.as_ref() else {
        return None;
    };
    let prefix = anchored_prefix(text, exact)?;
    let low = RangeBound {
        kind: BoundKind::GreaterEqual,
        value: BoundExpr::Text(prefix.clone()),
        unconverted: false,
    };
    let high = next_prefix(&prefix).map(|above| RangeBound {
        kind: BoundKind::Less,
        value: BoundExpr::Text(above),
        unconverted: false,
    });
    // A descending index walks the other way, so the two ends swap: see the
    // note in the comparison arm above, which this follows exactly.
    match descending {
        false => Some((Some(low), high)),
        true => Some((
            high.map(|bound| RangeBound {
                kind: BoundKind::Greater,
                value: bound.value,
                unconverted: false,
            }),
            Some(RangeBound {
                kind: BoundKind::LessEqual,
                value: low.value,
                unconverted: false,
            }),
        )),
    }
}

/// Returns the literal prefix a pattern is anchored on, if it has one.
///
/// Everything before the first wildcard. `None` when the pattern starts with
/// one, because a pattern that can match anywhere selects no range at all.
///
/// @param pattern - the pattern's own text
/// @param glob - whether the wildcards are `GLOB`'s rather than `LIKE`'s
fn anchored_prefix(pattern: &[u8], glob: bool) -> Option<Vec<u8>> {
    let mut prefix = Vec::new();
    for byte in pattern {
        let wildcard = match glob {
            true => matches!(byte, b'*' | b'?' | b'['),
            false => matches!(byte, b'%' | b'_'),
        };
        if wildcard {
            break;
        }
        prefix.push(*byte);
    }
    // A pattern with no wildcard at all is an equality and is better served by
    // the equality path, which is already tried first; one with a wildcard at
    // the front selects no range.
    match prefix.is_empty() || prefix.len() == pattern.len() {
        true => None,
        false => Some(prefix),
    }
}

/// Returns the first key past every key with this prefix.
///
/// The prefix with its last byte that is not `0xFF` incremented and the rest
/// dropped. `None` when every byte is `0xFF`, which means there is no key above
/// the prefix and the range has no upper end.
///
/// @param prefix - the literal prefix
fn next_prefix(prefix: &[u8]) -> Option<Vec<u8>> {
    let mut above = prefix.to_vec();
    while let Some(last) = above.pop() {
        if last < 0xFF {
            above.push(last.saturating_add(1));
            return Some(above);
        }
    }
    None
}
