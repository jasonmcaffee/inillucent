//! When a `HAVING` with no `GROUP BY` is a group over the whole table, and when
//! it is a statement to refuse.
//!
//! Invariant: **the grammar accepts every `HAVING`, and this decides which ones
//! are legal, in SQLite's own words.** The two used to disagree: `HAVING` was
//! read only inside the `GROUP BY` arm of `parse_select_core`, so
//! `SELECT count(*) AS n FROM t HAVING n > 0` - which the reference answers
//! with the count - stopped at `near "HAVING": syntax error` (task-2040). That
//! is the worst refusal available for it. Exit code 3 and the `unsupported`
//! status exist so a caller can tell "this engine has not built that" from
//! "your SQL is wrong", and a syntax error said the second about a statement
//! that is correct, which sends the caller rewording SQL that needs no
//! rewording. Deciding it here keeps the parser's job to shape and this one to
//! meaning, and keeps the sentence in one place.

use crate::diagnostic::ParseError;
use crate::lexer::Span;

/// Refuses a `HAVING` on a statement that does not aggregate.
///
/// **What makes a statement an aggregating one, for SQLite, is an aggregate
/// among the *result columns* and nothing else.** A `GROUP BY` makes one too.
/// An aggregate that appears only in the `HAVING`, or only in the `ORDER BY`,
/// does not, which is why the count is taken before the `HAVING` is bound
/// rather than read off the binder afterwards - by then `self.aggregates` holds
/// the ones the `HAVING` itself introduced. Measured against the pinned
/// `sqlite3` 3.53.4:
///
/// ```sql
/// SELECT count(*) AS n FROM t HAVING n > 0;    -- 3
/// SELECT count(*) FROM t HAVING b > 0;         -- 3
/// SELECT 1 FROM t HAVING count(*) > 0;         -- HAVING clause on a non-aggregate query
/// SELECT 1 FROM t HAVING 1 ORDER BY count(*);  -- HAVING clause on a non-aggregate query
/// ```
///
/// The failure is `ParseErrorKind::Refused`, not `Unsupported`: the reference
/// refuses these too, so no release of this engine will ever accept them, and
/// `unsupported` would tell a caller to wait for a feature that is not coming.
/// Its span is the default one, which `statements::refused` reads as
/// positionless - the reference reports this error with no offset, so its shell
/// prints the sentence and no caret art, and a span here would make two
/// transcripts differ over two lines of drawing rather than over an answer.
///
/// @param has_having - whether the statement carries a `HAVING` at all
/// @param group_terms - how many `GROUP BY` terms it has
/// @param aggregates_in_columns - how many aggregates its result columns asked for
pub(crate) fn refuse_when_nothing_aggregates(
    has_having: bool,
    group_terms: usize,
    aggregates_in_columns: usize,
) -> Result<(), ParseError> {
    if has_having && group_terms == 0 && aggregates_in_columns == 0 {
        return Err(crate::bind::refused(
            "HAVING clause on a non-aggregate query",
            Span::default(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The three shapes that are legal, and the one that is not.
    ///
    /// The `GROUP BY` row is the one that matters most: a `HAVING` over a
    /// grouped statement with no aggregate anywhere is legal, and a rule
    /// written as "there must be an aggregate" would refuse
    /// `SELECT a FROM t GROUP BY a HAVING a > 15`, which the reference answers.
    #[test]
    fn only_a_having_over_a_statement_that_does_not_aggregate_is_refused() {
        assert!(refuse_when_nothing_aggregates(false, 0, 0).is_ok());
        assert!(refuse_when_nothing_aggregates(true, 1, 0).is_ok());
        assert!(refuse_when_nothing_aggregates(true, 0, 1).is_ok());
        let refused = refuse_when_nothing_aggregates(true, 0, 0)
            .expect_err("a HAVING over a statement that does not aggregate is refused");
        assert_eq!(refused.message(), "HAVING clause on a non-aggregate query");
    }

    /// The refusal points at nothing, so the shell draws no caret under it.
    ///
    /// `statements::refused` attaches an offset only when the span is not the
    /// default one, so this is what keeps the transcript equal to the
    /// reference's.
    #[test]
    fn the_refusal_carries_no_position() {
        let refused = refuse_when_nothing_aggregates(true, 0, 0)
            .expect_err("a HAVING over a statement that does not aggregate is refused");
        assert_eq!(refused.span, Span::default());
    }
}
