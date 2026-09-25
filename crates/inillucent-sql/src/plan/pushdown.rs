//! Pushing a `WHERE` condition on a derived table's columns into the derived
//! table.
//!
//! Invariant: **a condition is copied into a derived table only when doing so
//! cannot change which rows the statement returns, and the original stays where
//! it was.** The copy is a filter the inner query applies before it builds its
//! rows, so the inner planner can seek by key instead of building every row of
//! a view. The outer condition is still tested on every row the derived table
//! produces, so if a rule here were too generous the cost would be a condition
//! tested twice, never a row that should have been filtered.
//!
//! This is SQLite's push-down optimisation, restricted to the cases where it is
//! plainly sound. Measured on the coffee shop example's database before it:
//! `SELECT * FROM order_summary WHERE id = 57` over a view of three joins and a
//! correlated subquery took 2.50 ms against 0.048 ms for the same query written
//! without the view, because every row of the view, correlated subquery
//! included, was built before the `WHERE` was applied. SQLite plans both as a
//! primary key search.
//!
//! Here rather than in [`super`] because `plan.rs` is at its recorded size.

use super::*;

/// Copies each `WHERE` conjunct that reads only one derived table's columns
/// into that derived table's own `WHERE`.
///
/// @param select - the statement being planned, whose derived tables may gain
///   a filter
pub(super) fn push_into_derived_tables(select: &mut BoundSelect) {
    let Some(filter) = select.filter.as_ref() else {
        return;
    };
    // Nothing is split or copied for a statement with no derived table, which
    // is almost every statement: the compile of `SELECT id FROM t WHERE email
    // = ?1` has an allocation budget, and splitting its `WHERE` here took three
    // of them to find nothing to push.
    if !select
        .sources
        .iter()
        .any(|source| matches!(source.rows, SourceRows::Subquery(_)))
    {
        return;
    }
    // A `RIGHT` or `FULL` join can null extend any term before it, so a
    // statement with one pushes nothing.
    if select
        .sources
        .iter()
        .any(|source| matches!(source.join, JoinKind::Right | JoinKind::Full))
    {
        return;
    }
    let conjuncts = conjunction(filter);
    for source in &mut select.sources {
        // The right side of a `LEFT JOIN` is null extended when nothing in it
        // matches, and a condition such as `v.x IS NULL` is true of a null
        // extended row and false of the rows the push would have removed.
        if source.join == JoinKind::Left {
            continue;
        }
        let id = source.id;
        let SourceRows::Subquery(block) = &mut source.rows else {
            continue;
        };
        if !accepts_a_pushed_filter(block) {
            continue;
        }
        for conjunct in &conjuncts {
            let mut used = Vec::new();
            conjunct.sources_used(&mut used);
            if used.as_slice() != [id] || !pushable(conjunct, id) {
                continue;
            }
            let Some(inner) = substituted(conjunct, id, block) else {
                continue;
            };
            block.filter = Some(match block.filter.take() {
                Some(existing) => BoundExpr::And(Box::new(existing), Box::new(inner)),
                None => inner,
            });
        }
    }
}

/// Reports whether a derived table's rows are the same whether a condition on
/// its result columns is applied before it builds them or after.
///
/// **Not for a `LIMIT` or an `OFFSET`**, which count rows before the condition;
/// **not for `DISTINCT`**, which keeps one row of several equal under its own
/// collation, so a condition under a different collation can keep a different
/// one; **not for grouping or a window**, whose result columns are computed
/// over rows the condition would remove; and **not for a compound**, which is
/// several blocks.
///
/// @param block - the derived table's query
fn accepts_a_pushed_filter(block: &BoundSelect) -> bool {
    block.compounds.is_empty()
        && block.limit.is_none()
        && block.offset.is_none()
        && !block.distinct
        && block.group_by.is_empty()
        && block.aggregates.is_empty()
        && block.having.is_none()
        && block.windows.is_empty()
        && block.values.is_empty()
}

/// Reports whether a condition is one that may be evaluated anywhere, any
/// number of times, with the same answer.
///
/// A whitelist: a subquery, an aggregate, a window value, a registered function
/// whose determinism the planner cannot see, and the scalar functions whose
/// answer changes from call to call are all refused.
///
/// @param expr - the condition, or a part of it
/// @param id - the derived table's statement-wide number; its rowid has no
///   inner expression to stand for it
fn pushable(expr: &BoundExpr, id: usize) -> bool {
    let this = match expr {
        BoundExpr::Rowid { source } => *source != id,
        BoundExpr::Subquery { .. }
        | BoundExpr::Aggregate { .. }
        | BoundExpr::WindowRef { .. }
        | BoundExpr::SorterColumn { .. }
        | BoundExpr::External { .. }
        | BoundExpr::VirtualFunction { .. }
        | BoundExpr::Raise { .. } => false,
        BoundExpr::Function { func, .. } => !matches!(
            func,
            crate::function::ScalarFunc::Random
                | crate::function::ScalarFunc::RandomBlob
                | crate::function::ScalarFunc::Changes
                | crate::function::ScalarFunc::TotalChanges
                | crate::function::ScalarFunc::LastInsertRowid
        ),
        _ => true,
    };
    this && expr.children().iter().all(|child| pushable(child, id))
}

/// Returns a condition with each of the derived table's columns replaced by
/// the expression that computes it inside the derived table.
///
/// `None` when a column's expression is one that should not be evaluated in a
/// `WHERE`, such as a correlated subquery: the condition is then left outside,
/// where it was.
///
/// @param conjunct - the condition, over the derived table's columns
/// @param id - the derived table's statement-wide number
/// @param block - the derived table's query
fn substituted(conjunct: &BoundExpr, id: usize, block: &BoundSelect) -> Option<BoundExpr> {
    let mut copy = conjunct.clone();
    replace_columns(&mut copy, id, block).then_some(copy)
}

/// Replaces the derived table's columns in place, reporting whether every one
/// could be replaced.
///
/// @param expr - the expression being rewritten
/// @param id - the derived table's statement-wide number
/// @param block - the derived table's query
fn replace_columns(expr: &mut BoundExpr, id: usize, block: &BoundSelect) -> bool {
    if let BoundExpr::Column { source, column, .. } = expr {
        if *source != id {
            return true;
        }
        let Some(inner) = block.columns.get(usize::from(*column)) else {
            return false;
        };
        if !pushable(&inner.expr, usize::MAX) {
            return false;
        }
        *expr = inner.expr.clone();
        return true;
    }
    expr.children_mut()
        .into_iter()
        .all(|child| replace_columns(child, id, block))
}
