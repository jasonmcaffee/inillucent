//! Leaving out the result columns of a derived table that nothing reads.
//!
//! Invariant: **a derived table's result column is replaced by NULL only when
//! no expression of the enclosing query reads it, and only in a derived table
//! whose rows do not depend on the values of its result columns.** A column
//! nothing reads cannot change an answer by being NULL, so the one thing this
//! changes is whether the expression is evaluated at all.
//!
//! That is the point of it. SQLite never evaluates such a column: it either
//! flattens the derived table into the query that reads it, which leaves the
//! unread expression behind, or it replaces the expression with NULL in
//! `disableUnusedSubqueryResultColumns`. So `SELECT k FROM (SELECT k,
//! abs(a) AS j FROM t)` over a row holding the smallest integer answers that
//! row's `k` in SQLite, and failed here with "integer overflow" from an
//! `abs()` whose value nobody asked for.
//!
//! The conditions are SQLite's: not a `DISTINCT` block, whose rows are told
//! apart by every result column; not an aggregate block, and not a window
//! block, whose result columns are computed over the rows; and in a compound,
//! only `UNION ALL`, since the other operators compare whole rows. A column is
//! kept whenever anything reads it in a way this walk cannot enumerate, which
//! is a nested query that correlates to the derived table.

use super::*;
use crate::bind::BoundFrameBound;

/// Replaces each unread result column of each derived table with NULL.
///
/// @param select - the statement being planned, whose derived tables may lose
///   result columns
pub(super) fn drop_unread_columns(select: &mut BoundSelect) {
    if !select
        .sources
        .iter()
        .any(|source| matches!(source.rows, SourceRows::Subquery(_)))
    {
        return;
    }
    for position in 0..select.sources.len() {
        let Some(source) = select.sources.get(position) else {
            continue;
        };
        let SourceRows::Subquery(block) = &source.rows else {
            continue;
        };
        if !may_drop_columns(block) {
            continue;
        }
        let Some(read) = columns_read(select, source.id, block.columns.len()) else {
            continue;
        };
        if read.iter().all(|held| *held) {
            continue;
        }
        let Some(SourceRows::Subquery(block)) = select
            .sources
            .get_mut(position)
            .map(|source| &mut source.rows)
        else {
            continue;
        };
        null_unread(block, &read);
    }
}

/// Reports whether a derived table's rows are the same whatever its result
/// columns hold.
///
/// @param block - the derived table's query
fn may_drop_columns(block: &BoundSelect) -> bool {
    let arms = core::iter::once(block).chain(block.compounds.iter().map(|(_, arm)| arm));
    let plain = |arm: &BoundSelect| {
        !arm.distinct
            && arm.aggregates.is_empty()
            && arm.group_by.is_empty()
            && arm.having.is_none()
            && arm.windows.is_empty()
            && arm.values.is_empty()
    };
    arms.into_iter().all(plain)
        && block
            .compounds
            .iter()
            .all(|(op, _)| *op == CompoundOp::UnionAll)
}

/// Sets each unread result column to NULL, in the block and in every arm of
/// its compound.
///
/// @param block - the derived table's query
/// @param read - one flag per result column, true when it is read
fn null_unread(block: &mut BoundSelect, read: &[bool]) {
    let unread = |position: usize| !read.get(position).copied().unwrap_or(true);
    for (position, column) in block.columns.iter_mut().enumerate() {
        if unread(position) {
            column.expr = BoundExpr::Null;
        }
    }
    for (_, arm) in &mut block.compounds {
        for (position, column) in arm.columns.iter_mut().enumerate() {
            if unread(position) {
                column.expr = BoundExpr::Null;
            }
        }
    }
}

/// Returns which of one derived table's result columns a query reads.
///
/// `None` when the answer cannot be known, which is when a nested query
/// correlates to the derived table: it is a query of its own and may read any
/// of its columns.
///
/// @param select - the query that holds the derived table
/// @param id - the derived table's statement-wide number
/// @param width - how many result columns the derived table has
fn columns_read(select: &BoundSelect, id: usize, width: usize) -> Option<Vec<bool>> {
    let mut read = vec![false; width];
    let mut opaque = false;
    each_expression(select, &mut |expr| mark(expr, id, &mut read, &mut opaque));
    for source in &select.sources {
        if let SourceRows::Subquery(block) = &source.rows {
            opaque |= block.correlations.contains(&id);
        }
        if let SourceRows::Recursive(body) = &source.rows {
            opaque |= body
                .seeds
                .iter()
                .chain(body.steps.iter())
                .any(|(_, arm)| arm.correlations.contains(&id));
        }
    }
    (!opaque).then_some(read)
}

/// Records the derived table's columns one expression reads.
///
/// @param expr - the expression
/// @param id - the derived table's statement-wide number
/// @param read - one flag per result column, set for each one read
/// @param opaque - set when a nested query correlates to the derived table
fn mark(expr: &BoundExpr, id: usize, read: &mut [bool], opaque: &mut bool) {
    match expr {
        BoundExpr::Column { source, column, .. } if *source == id => {
            if let Some(flag) = read.get_mut(usize::from(*column)) {
                *flag = true;
            }
        }
        // A rowid of a derived table is not one of its result columns, but
        // nothing here knows what it reads, so every column is kept.
        BoundExpr::Rowid { source } if *source == id => *opaque = true,
        BoundExpr::VirtualFunction { source, .. } if *source == id => *opaque = true,
        BoundExpr::Subquery { block, .. } if block.correlations.contains(&id) => *opaque = true,
        _ => {}
    }
    for child in expr.children() {
        mark(child, id, read, opaque);
    }
}

/// Calls a function on every top level expression a block holds.
///
/// Every field of [`BoundSelect`] that can hold an expression is named, in the
/// same way as `BoundSelect::gather_columns`, because a missed one here is a
/// column set to NULL that something reads.
///
/// @param select - the block
/// @param visit - called once per expression
fn each_expression(select: &BoundSelect, visit: &mut impl FnMut(&BoundExpr)) {
    for term in &select.sources {
        if let Some(constraint) = &term.constraint {
            visit(constraint);
        }
    }
    let singles = select
        .filter
        .iter()
        .chain(select.having.iter())
        .chain(select.group_by.iter())
        .chain(select.limit.iter())
        .chain(select.offset.iter());
    for expr in singles {
        visit(expr);
    }
    for column in &select.columns {
        visit(&column.expr);
    }
    for term in &select.order_by {
        visit(&term.expr);
    }
    for aggregate in &select.aggregates {
        aggregate.arguments.iter().for_each(&mut *visit);
        aggregate.filter.iter().for_each(&mut *visit);
        for term in &aggregate.order_by {
            visit(&term.expr);
        }
    }
    for window in &select.windows {
        window.arguments.iter().for_each(&mut *visit);
        window.filter.iter().for_each(&mut *visit);
        window.partition_by.iter().for_each(&mut *visit);
        for term in &window.order_by {
            visit(&term.expr);
        }
        for bound in [&window.start, &window.end] {
            if let BoundFrameBound::Preceding(expr) | BoundFrameBound::Following(expr) = bound {
                visit(expr);
            }
        }
    }
    for row in &select.values {
        row.iter().for_each(&mut *visit);
    }
    for (_, arm) in &select.compounds {
        each_expression(arm, visit);
    }
}
