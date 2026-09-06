//! Nested queries used as values: `EXISTS`, a scalar `(SELECT ...)`, and the
//! right side of an `IN`.
//!
//! Invariant: **an uncorrelated subquery is a constant for the execution, and
//! is evaluated once before the operator chain is built.** It reads no column
//! of the row being tested, so running it per row would be running the same
//! query once for every row of the outer scan and getting the same answer every
//! time.
//!
//! That reduction is what makes this small. Once the block's rows are in hand,
//! the three forms are three readings of one list: `EXISTS` asks whether it is
//! empty, a scalar takes its head, and `IN` is an `IN` over a list of literals -
//! which the executor already has, along with SQLite's three-valued NULL rule
//! and the affinity and collation the binder attached. Nothing new runs at row
//! time.
//!
//! ## Why it is folded per execution and not per compile
//!
//! Statements are cached by their text, and a folded value is only true of the
//! data it was read from. Folding into the cached plan would answer
//! `SELECT (SELECT count(*) FROM t)` with the count from whenever the statement
//! was first compiled. So the fold happens on the way into each execution, and
//! the values ride on [`Params`] - which is the same place a `?1` rides, for
//! the same reason: both are values known before the chain is built and not
//! before that. Reading one bumps the parameter read counter, so a prepared
//! statement that baked a subquery into its chain will not be re-run against a
//! later state of the table.
//!
//! ## What is not here
//!
//! A **correlated** subquery reads a column of the outer row, so it has no
//! single value and cannot be folded. Those are left unfilled and refused by
//! name in the physical pass. They need a per-row evaluation the pipeline does
//! not have, which is a piece of work of its own.

use inillucent_base::DbResult;
use inillucent_sql::bind::{BoundExpr, BoundSelect};
use inillucent_sql::plan::{plan_select_with, Levers, PhysicalPlan};
use inillucent_tree::datum::OwnedDatum;

use crate::physical::{run_any, Params, TreeCatalog};

/// One uncorrelated subquery's answer.
///
/// The first column of every row the block produced, in order. All three forms
/// read this one list, which is why there is one type rather than three: a
/// value that could be a set where a scalar was expected would be a state no
/// input produces and a branch nothing reaches.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Subvalue {
    /// The first column of each row, one entry per row.
    pub column: Vec<OwnedDatum>,
}

impl Subvalue {
    /// Returns what `EXISTS` answers.
    pub fn exists(&self) -> bool {
        !self.column.is_empty()
    }

    /// Returns what a scalar subquery answers.
    ///
    /// A block that produced nothing is NULL, which is SQLite's rule and the
    /// reason this is not an `Option` the caller has to remember to handle.
    pub fn scalar(&self) -> OwnedDatum {
        self.column.first().cloned().unwrap_or(OwnedDatum::Null)
    }
}

/// Evaluates every uncorrelated subquery in a plan, once.
///
/// Returns `None` when there is nothing to do - either the statement has no
/// subquery, or this execution's subqueries have already been folded, which is
/// what happens when a folded block is itself run through the ordinary
/// execution path.
///
/// @param plan - the planner's output
/// @param catalog - where the trees and layouts come from
/// @param params - the values bound for this execution
pub fn fold(
    plan: &PhysicalPlan,
    catalog: &dyn TreeCatalog,
    params: &Params,
) -> DbResult<Option<Params>> {
    if params.has_subqueries() {
        return Ok(None);
    }
    // The cheap question first, and it is the one asked on nearly every
    // execution. This runs inside `build_prepared`, which is the path a
    // `point.rowid` probe takes in about a microsecond, so the answer for a
    // statement with no subquery in it has to cost no allocation and has to
    // stop at the first one it finds.
    if !any_in_plan(plan) {
        return Ok(None);
    }
    let mut found = Vec::new();
    gather_plan(plan, &mut found);
    // One subquery can be reached twice - a filter term also appears in the
    // residual the planner distributed it into - and running its block twice
    // would be running the same query twice for the same answer.
    let mut seen = Vec::new();
    found.retain(|block| {
        let first = !seen.contains(&block.id);
        seen.push(block.id);
        first
    });
    if found.is_empty() {
        return Ok(None);
    }
    let width = found
        .iter()
        .map(|block| block.id.saturating_add(1))
        .max()
        .unwrap_or(0);
    // Allocated before anything is run, so that the nested executions below see
    // a params that already reports `has_subqueries` and do not start the fold
    // again from inside it.
    let mut folded = params.with_subqueries(vec![None; width]);
    for block in found {
        if block.correlated {
            // Left unfilled on purpose. The physical pass refuses it by name;
            // filling it with an empty answer would be a wrong answer.
            continue;
        }
        let inner = plan_select_with(block.select.clone(), Levers::default());
        let (rows, _shape) = run_any(&inner, catalog, &folded)?;
        let column = rows
            .into_iter()
            .map(|row| row.into_iter().next().unwrap_or(OwnedDatum::Null))
            .collect();
        folded.set_subquery(block.id, Subvalue { column });
    }
    Ok(Some(folded))
}

/// Collects every subquery of a planned statement, innermost first.
///
/// The planner **moves** a compound's arms out of `select.compounds` into
/// `plan.compounds` as plans of their own, so a walk of the select alone sees
/// the first arm and none of the others. That is how a `UNION` whose second arm
/// held the only subquery came back as "a correlated subquery": its slot was
/// never filled.
///
/// @param plan - the planner's output
/// @param into - the blocks found so far
fn gather_plan<'a>(plan: &'a PhysicalPlan, into: &mut Vec<Block<'a>>) {
    gather(&plan.select, into);
    for residual in plan.residuals.iter().flatten() {
        gather_expression(residual, into);
    }
    if let Some(filter) = &plan.constant_filter {
        gather_expression(filter, into);
    }
    for (_op, arm) in &plan.compounds {
        gather_plan(arm, into);
    }
}

/// Returns whether a planned statement holds a subquery used as a value.
///
/// @param plan - the planner's output
fn any_in_plan(plan: &PhysicalPlan) -> bool {
    any(&plan.select)
        || plan.residuals.iter().flatten().any(any_in)
        || plan.constant_filter.as_ref().is_some_and(any_in)
        || plan.compounds.iter().any(|(_op, arm)| any_in_plan(arm))
}

/// One subquery the fold has to evaluate.
struct Block<'a> {
    /// The statement-wide number the binder gave it.
    id: usize,
    /// The nested query.
    select: &'a BoundSelect,
    /// Whether it reads a column of the outer row.
    correlated: bool,
}

/// Collects every subquery of a select, innermost first.
///
/// The order matters: a subquery nested inside another one has to have its
/// value before the block containing it is run, and they share one numbering
/// for the whole statement, so one table filled in this order is enough.
///
/// @param select - the query to walk
/// @param into - the blocks found so far
fn gather<'a>(select: &'a BoundSelect, into: &mut Vec<Block<'a>>) {
    for expr in expressions(select) {
        gather_expression(expr, into);
    }
    for (_op, arm) in &select.compounds {
        gather(arm, into);
    }
    for source in &select.sources {
        if let inillucent_sql::bind::SourceRows::Subquery(block) = &source.rows {
            gather(block, into);
        }
        if let Some(constraint) = &source.constraint {
            gather_expression(constraint, into);
        }
    }
}

/// Collects every subquery of one expression, innermost first.
///
/// @param expr - the expression to walk
/// @param into - the blocks found so far
fn gather_expression<'a>(expr: &'a BoundExpr, into: &mut Vec<Block<'a>>) {
    for child in expr.children() {
        gather_expression(child, into);
    }
    if let BoundExpr::Subquery { id, block, .. } = expr {
        // The block's own subqueries first, so the innermost is filled first.
        gather(block, into);
        into.push(Block {
            id: *id,
            select: block,
            correlated: !block.correlations.is_empty(),
        });
    }
}

/// Returns every expression a select holds directly.
///
/// The nested selects are walked by [`gather`] rather than here, so this stays
/// a list of this block's own expressions and nothing else.
///
/// @param select - the query
fn expressions(select: &BoundSelect) -> Vec<&BoundExpr> {
    let mut out: Vec<&BoundExpr> = Vec::new();
    out.extend(select.filter.iter());
    out.extend(select.group_by.iter());
    out.extend(select.having.iter());
    out.extend(select.columns.iter().map(|column| &column.expr));
    out.extend(select.order_by.iter().map(|term| &term.expr));
    out.extend(select.limit.iter());
    out.extend(select.offset.iter());
    out.extend(select.values.iter().flatten());
    for aggregate in &select.aggregates {
        out.extend(aggregate.arguments.iter());
    }
    for window in &select.windows {
        out.extend(window.arguments.iter());
        out.extend(window.filter.iter());
        out.extend(window.partition_by.iter());
        out.extend(window.order_by.iter().map(|term| &term.expr));
    }
    out
}

/// Returns whether a select holds a subquery used as a value, anywhere.
///
/// Separate from [`gather`] and deliberately allocation-free: it is asked on
/// every execution of every statement, and almost every answer is `false`.
///
/// @param select - the query to look through
fn any(select: &BoundSelect) -> bool {
    each_expression(select, &mut |expr| any_in(expr))
        || select.compounds.iter().any(|(_op, arm)| any(arm))
        || select.sources.iter().any(|source| {
            matches!(&source.rows, inillucent_sql::bind::SourceRows::Subquery(block) if any(block))
        })
}

/// Returns whether an expression holds a subquery, anywhere beneath it.
///
/// @param expr - the expression to look through
fn any_in(expr: &BoundExpr) -> bool {
    matches!(expr, BoundExpr::Subquery { .. }) || expr.children().iter().any(|child| any_in(child))
}

/// Runs a test over every expression a select holds directly, stopping early.
///
/// @param select - the query
/// @param test - what to ask of each expression
fn each_expression(select: &BoundSelect, test: &mut dyn FnMut(&BoundExpr) -> bool) -> bool {
    select.filter.iter().any(|expr| test(expr))
        || select.group_by.iter().any(|expr| test(expr))
        || select.having.iter().any(|expr| test(expr))
        || select.columns.iter().any(|column| test(&column.expr))
        || select.order_by.iter().any(|term| test(&term.expr))
        || select.limit.iter().any(|expr| test(expr))
        || select.offset.iter().any(|expr| test(expr))
        || select.values.iter().flatten().any(|expr| test(expr))
        || select
            .aggregates
            .iter()
            .any(|aggregate| aggregate.arguments.iter().any(|expr| test(expr)))
        || select.windows.iter().any(|window| {
            window.arguments.iter().any(|expr| test(expr))
                || window.filter.iter().any(|expr| test(expr))
                || window.partition_by.iter().any(|expr| test(expr))
                || window.order_by.iter().any(|term| test(&term.expr))
        })
        || select
            .sources
            .iter()
            .any(|source| source.constraint.iter().any(|expr| test(expr)))
}
