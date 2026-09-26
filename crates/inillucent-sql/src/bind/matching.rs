//! `MATCH` in a place where the planner cannot hand it to the module.
//!
//! Invariant: **a `MATCH` that an `OR` reaches through `AND` and `OR` alone
//! becomes a rowid test over a search the module is offered.** A module
//! answers `MATCH` only as a constraint on its own scan, so the planner offers
//! it when it is one of the `AND`ed terms of the filter and nowhere else. Under
//! an `OR` there was nothing to offer it to, and the executor has no way to
//! evaluate it row by row: `SELECT rowid FROM f WHERE f MATCH 'release' OR
//! rowid = 2` was refused as "the Match operator" not built, and SQLite answers
//! it. SQLite runs each branch of the `OR` as a search of its own and joins the
//! rowids; the same answer comes from asking, for each row, whether its rowid
//! is among the rowids a search on its own finds. That search is an ordinary
//! subquery whose `MATCH` is a plain conjunct, so the module is offered it the
//! usual way.
//!
//! A plain conjunct is left alone: that is the form the module's own ranking
//! and highlighting functions need, and turning it into a subquery would
//! separate them from the search they read.

use super::{
    unsupported, Binder, BoundExpr, BoundResultColumn, BoundSelect, BoundSource, SubqueryKind,
};
use crate::ast::{JoinKind, PatternOp};
use crate::bind::{comparison_rules, IndexChoice, SourceRows};
use crate::diagnostic::ParseError;
use crate::lexer::Span;

impl Binder<'_> {
    /// Rewrites every `MATCH` in a block's filter that an `OR` reaches.
    ///
    /// @param filter - the block's bound `WHERE`, with its join constraints
    pub(super) fn match_by_rowid(&mut self, filter: &mut BoundExpr) -> Result<(), ParseError> {
        match filter {
            BoundExpr::And(left, right) => {
                self.match_by_rowid(left)?;
                self.match_by_rowid(right)
            }
            BoundExpr::Or(..) => self.rewrite_disjunction(filter),
            _ => Ok(()),
        }
    }

    /// Replaces each `MATCH` an `OR` reaches through `AND` and `OR` alone.
    ///
    /// **Only through `AND` and `OR`, because that is all SQLite answers.** Its
    /// `OR` optimisation runs each branch as a search of its own, so a `MATCH`
    /// in a branch is offered to the module; under a `NOT`, a `CASE` or a
    /// function call there is no search to run, and SQLite refuses the
    /// statement with "unable to use function MATCH in the requested context".
    /// Rewriting those as well would answer a statement SQLite refuses. They
    /// are left for the physical pass to refuse the same way.
    ///
    /// @param expr - a disjunction, or a branch of one
    fn rewrite_disjunction(&mut self, expr: &mut BoundExpr) -> Result<(), ParseError> {
        match expr {
            BoundExpr::And(left, right) | BoundExpr::Or(left, right) => {
                self.rewrite_disjunction(left)?;
                self.rewrite_disjunction(right)
            }
            BoundExpr::Pattern {
                negated: false,
                op: PatternOp::Match,
                ..
            } => {
                if let Some(test) = self.rowid_in_search(expr)? {
                    *expr = test;
                }
                Ok(())
            }
            _ => Ok(()),
        }
    }

    /// Returns `rowid IN (SELECT rowid FROM <table> WHERE <column> MATCH
    /// <pattern>)` for one `MATCH`, or `None` when its operand is not a column.
    ///
    /// The search reads a new term of the same table and nothing else, so it
    /// is uncorrelated and runs once per statement.
    ///
    /// **A pattern that reads a row is refused as not built.** Its search
    /// would depend on the row and run once per row, and the executor answered
    /// that wrongly when the pattern read a term outside the block, so it is
    /// refused by name rather than answered. The capability row
    /// `match_in_an_or_reading_a_row` says so.
    ///
    /// @param expr - the `MATCH` pattern expression
    fn rowid_in_search(&mut self, expr: &BoundExpr) -> Result<Option<BoundExpr>, ParseError> {
        let BoundExpr::Pattern {
            operand, pattern, ..
        } = expr
        else {
            return Ok(None);
        };
        let BoundExpr::Column { source: outer, .. } = operand.as_ref() else {
            return Ok(None);
        };
        let outer = *outer;
        if reads_a_row(pattern) {
            return Err(unsupported(
                "a MATCH under an OR whose pattern reads a row",
                Span::default(),
            ));
        }
        let Some(held) = self.sources.get(outer) else {
            return Ok(None);
        };
        let table = std::rc::Rc::clone(&held.table);
        let alias = held.alias.clone();
        let inner = self.sources.len();
        let searched = BoundSource {
            index_hint: IndexChoice::Any,
            id: inner,
            rows: SourceRows::Table,
            table,
            alias,
            join: JoinKind::Comma,
            constraint: None,
            suppressed: Vec::new(),
            index_exprs: Vec::new(),
        };
        self.sources.push(searched.clone());
        let mut column = operand.as_ref().clone();
        if let BoundExpr::Column { source, .. } = &mut column {
            *source = inner;
        }
        let block = BoundSelect {
            sources: vec![searched],
            filter: Some(BoundExpr::Pattern {
                negated: false,
                op: PatternOp::Match,
                operand: Box::new(column),
                pattern: pattern.clone(),
                escape: None,
            }),
            group_by: Vec::new(),
            having: None,
            columns: vec![BoundResultColumn {
                expr: BoundExpr::Rowid { source: inner },
                name: b"rowid".to_vec(),
                origin: None,
                declared_type: Vec::new(),
            }],
            distinct: false,
            order_by: Vec::new(),
            limit: None,
            offset: None,
            aggregates: Vec::new(),
            values: Vec::new(),
            compounds: Vec::new(),
            windows: Vec::new(),
            correlations: Vec::new(),
        };
        let tested = BoundExpr::Rowid { source: outer };
        let (affinity, collation) = comparison_rules(&tested, &BoundExpr::Rowid { source: inner });
        Ok(Some(BoundExpr::Subquery {
            id: self.next_subquery_id(),
            kind: SubqueryKind::In,
            negated: false,
            operand: Some(Box::new(tested)),
            block: Box::new(block),
            affinity,
            collation,
        }))
    }
}

/// Reports whether an expression reads a FROM term, directly or through a
/// correlated subquery.
///
/// @param expr - the expression
fn reads_a_row(expr: &BoundExpr) -> bool {
    match expr {
        BoundExpr::Column { .. } | BoundExpr::Rowid { .. } => true,
        BoundExpr::Subquery { block, operand, .. } => {
            !block.correlations.is_empty() || operand.as_deref().is_some_and(reads_a_row)
        }
        other => other.children().into_iter().any(reads_a_row),
    }
}
