//! Rewriting a bound tree in place, exhaustively.
//!
//! Invariant: a walk here reaches every expression a statement holds - result
//! columns, filters, join constraints, window frames, `VALUES` rows, compound
//! arms and subquery blocks alike - or it is a defect rather than an omission.
//! The walks are written beside the definitions they mirror for that reason: a
//! field added to `BoundSelect` and not added here is a subtree some rewrite
//! silently skips, and the symptom is a wrong answer rather than a refusal.
//!
//! ## What it is for
//!
//! The trigger firing point. A trigger body is bound once, against the
//! statement that fires it, and its expressions read `OLD` and `NEW` through
//! the two sentinel source numbers [`crate::bind::OLD_SOURCE`] and
//! [`crate::bind::NEW_SOURCE`]. At the moment a trigger fires, both rows are
//! values the write path is already holding - so the body is copied, every
//! `OLD` and `NEW` read in it is replaced by the value itself, and what is left
//! is an ordinary statement with no external references that the ordinary
//! planner plans and the ordinary write path applies.
//!
//! That is what makes `DELETE FROM child WHERE parent_id = OLD.id` reach the
//! same index probe the same `DELETE` typed by hand reaches, and it is why
//! there is no second execution path for a trigger body.
//!
//! ## The one subtree a rewrite must not enter
//!
//! A body statement carries its **own** triggers, and those have their own
//! `OLD` and `NEW`. Substituting the firing statement's rows into them would
//! give an inner trigger the outer row, which is a wrong answer of exactly the
//! kind this engine is not allowed to have. So [`rewrite_insert`],
//! [`rewrite_update`] and [`rewrite_delete`] walk every field of their
//! statement except `triggers` and `replace_triggers`, and each nested fire
//! substitutes its own rows when its own turn comes.

use crate::bind::{
    BoundExpr, BoundFrameBound, BoundOrderTerm, BoundResultColumn, BoundSelect, BoundWindow,
    SourceRows,
};
use crate::dml::{BoundDelete, BoundInsert, BoundInsertSource, BoundUpdate, ColumnSource};

/// A rewrite applied to one expression before its children are walked.
pub type Rewrite<'a> = &'a mut dyn FnMut(&mut BoundExpr);

/// Applies a rewrite to one expression and everything under it.
///
/// The expression itself first, then its children, then the block a subquery
/// holds - so a rewrite that replaces a node does not then walk the children of
/// the node it put there.
///
/// @param expr - the expression to rewrite
/// @param rewrite - what to do to each expression
pub fn rewrite_expr(expr: &mut BoundExpr, rewrite: Rewrite<'_>) {
    rewrite(expr);
    for child in expr.children_mut() {
        rewrite_expr(child, rewrite);
    }
    if let Some(block) = expr.block_mut() {
        rewrite_select(block, rewrite);
    }
}

/// Applies a rewrite to an optional expression.
///
/// @param expr - the expression, when there is one
/// @param rewrite - what to do to each expression
fn rewrite_option(expr: Option<&mut BoundExpr>, rewrite: Rewrite<'_>) {
    if let Some(expr) = expr {
        rewrite_expr(expr, rewrite);
    }
}

/// Applies a rewrite to a list of result columns.
///
/// @param columns - the result columns
/// @param rewrite - what to do to each expression
fn rewrite_columns(columns: &mut [BoundResultColumn], rewrite: Rewrite<'_>) {
    for column in columns {
        rewrite_expr(&mut column.expr, rewrite);
    }
}

/// Applies a rewrite to a list of order terms.
///
/// @param terms - the order terms
/// @param rewrite - what to do to each expression
fn rewrite_order(terms: &mut [BoundOrderTerm], rewrite: Rewrite<'_>) {
    for term in terms {
        rewrite_expr(&mut term.expr, rewrite);
    }
}

/// Applies a rewrite to one frame bound.
///
/// @param bound - the frame bound
/// @param rewrite - what to do to each expression
fn rewrite_bound(bound: &mut BoundFrameBound, rewrite: Rewrite<'_>) {
    match bound {
        BoundFrameBound::Preceding(expr) | BoundFrameBound::Following(expr) => {
            rewrite_expr(expr, rewrite)
        }
        BoundFrameBound::UnboundedPreceding
        | BoundFrameBound::CurrentRow
        | BoundFrameBound::UnboundedFollowing => {}
    }
}

/// Applies a rewrite to one window definition.
///
/// @param window - the window
/// @param rewrite - what to do to each expression
fn rewrite_window(window: &mut BoundWindow, rewrite: Rewrite<'_>) {
    for argument in &mut window.arguments {
        rewrite_expr(argument, rewrite);
    }
    rewrite_option(window.filter.as_mut(), rewrite);
    for term in &mut window.partition_by {
        rewrite_expr(term, rewrite);
    }
    rewrite_order(&mut window.order_by, rewrite);
    rewrite_bound(&mut window.start, rewrite);
    rewrite_bound(&mut window.end, rewrite);
}

/// Applies a rewrite to every expression a query holds.
///
/// @param select - the query
/// @param rewrite - what to do to each expression
pub fn rewrite_select(select: &mut BoundSelect, rewrite: Rewrite<'_>) {
    for source in &mut select.sources {
        rewrite_option(source.constraint.as_mut(), rewrite);
        match &mut source.rows {
            SourceRows::Table | SourceRows::RecursiveSelf { .. } => {}
            SourceRows::Subquery(block) => rewrite_select(block, rewrite),
            SourceRows::Recursive(body) => {
                for (_, arm) in body.seeds.iter_mut().chain(body.steps.iter_mut()) {
                    rewrite_select(arm, rewrite);
                }
            }
        }
    }
    rewrite_option(select.filter.as_mut(), rewrite);
    for term in &mut select.group_by {
        rewrite_expr(term, rewrite);
    }
    rewrite_option(select.having.as_mut(), rewrite);
    rewrite_columns(&mut select.columns, rewrite);
    rewrite_order(&mut select.order_by, rewrite);
    rewrite_option(select.limit.as_mut(), rewrite);
    rewrite_option(select.offset.as_mut(), rewrite);
    for aggregate in &mut select.aggregates {
        for argument in &mut aggregate.arguments {
            rewrite_expr(argument, rewrite);
        }
    }
    for row in &mut select.values {
        for value in row {
            rewrite_expr(value, rewrite);
        }
    }
    for (_, arm) in &mut select.compounds {
        rewrite_select(arm, rewrite);
    }
    for window in &mut select.windows {
        rewrite_window(window, rewrite);
    }
}

/// Applies a rewrite to one column source.
///
/// @param source - where the column's value comes from
/// @param rewrite - what to do to each expression
fn rewrite_source(source: &mut ColumnSource, rewrite: Rewrite<'_>) {
    match source {
        ColumnSource::Row(_) => {}
        ColumnSource::Expr(expr) | ColumnSource::Generated(expr) => rewrite_expr(expr, rewrite),
    }
}

/// Applies a rewrite to every expression an `INSERT` holds, its own triggers
/// excepted.
///
/// @param statement - the insert
/// @param rewrite - what to do to each expression
pub fn rewrite_insert(statement: &mut BoundInsert, rewrite: Rewrite<'_>) {
    for column in &mut statement.columns {
        rewrite_source(column, rewrite);
    }
    if let Some(rowid) = statement.rowid.as_mut() {
        rewrite_source(rowid, rewrite);
    }
    match &mut statement.source {
        BoundInsertSource::Values(rows) => {
            for row in rows {
                for value in row {
                    rewrite_expr(value, rewrite);
                }
            }
        }
        BoundInsertSource::Select(select) => rewrite_select(select, rewrite),
    }
    for check in &mut statement.checks {
        rewrite_expr(&mut check.expr, rewrite);
    }
    for upsert in &mut statement.upsert {
        for assignment in &mut upsert.assignments {
            rewrite_expr(&mut assignment.value, rewrite);
        }
        rewrite_option(upsert.filter.as_mut(), rewrite);
    }
    rewrite_columns(&mut statement.returning, rewrite);
}

/// Applies a rewrite to every expression an `UPDATE` holds, its own triggers
/// excepted.
///
/// @param statement - the update
/// @param rewrite - what to do to each expression
pub fn rewrite_update(statement: &mut BoundUpdate, rewrite: Rewrite<'_>) {
    for assignment in &mut statement.assignments {
        rewrite_expr(&mut assignment.value, rewrite);
    }
    rewrite_option(statement.filter.as_mut(), rewrite);
    for check in &mut statement.checks {
        rewrite_expr(&mut check.expr, rewrite);
    }
    rewrite_columns(&mut statement.returning, rewrite);
    rewrite_option(statement.limit.as_mut(), rewrite);
    rewrite_option(statement.offset.as_mut(), rewrite);
    if let Some(rows) = statement.view_rows.as_mut() {
        rewrite_select(rows, rewrite);
    }
}

/// Applies a rewrite to every expression a `DELETE` holds, its own triggers
/// excepted.
///
/// @param statement - the delete
/// @param rewrite - what to do to each expression
pub fn rewrite_delete(statement: &mut BoundDelete, rewrite: Rewrite<'_>) {
    rewrite_option(statement.filter.as_mut(), rewrite);
    rewrite_columns(&mut statement.returning, rewrite);
    rewrite_option(statement.limit.as_mut(), rewrite);
    rewrite_option(statement.offset.as_mut(), rewrite);
    if let Some(rows) = statement.view_rows.as_mut() {
        rewrite_select(rows, rewrite);
    }
}
