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
    // The cheap question first, and it is the one asked on nearly every
    // execution: this runs inside `build_prepared`, which is the path a
    // `point.rowid` probe takes in about 0.78 us. The planner answered it once
    // when it compiled the statement. Asking it here instead - walking the
    // expression tree per execution - measured at about 0.07 us on every point
    // workload, because `BoundExpr::children` allocates a vector per node.
    if !plan.subqueries {
        return Ok(None);
    }
    if params.has_subqueries() {
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
    fill(found, catalog, params).map(Some)
}

/// Runs each gathered block and returns the parameters carrying the answers.
///
/// @param found - the blocks, innermost first
/// @param catalog - where the trees and layouts come from
/// @param params - the values bound for this execution
fn fill(found: Vec<Block<'_>>, catalog: &dyn TreeCatalog, params: &Params) -> DbResult<Params> {
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
    Ok(folded)
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

/// Evaluates every uncorrelated subquery in a list of expressions, once.
///
/// The companion to [`fold`], for the statements that have expressions but no
/// plan to hang them on: a `VALUES` list and an `UPDATE`'s assignments are
/// evaluated by the write path directly, so nothing ever built a
/// `PhysicalPlan` for them and the plan-shaped fold never saw them.
///
/// The symptom of that was a *misleading* refusal rather than a wrong answer.
/// `INSERT INTO t VALUES ((SELECT max(id) FROM t) + 1)` came back as "a
/// correlated subquery used as a value", which is what an unfilled slot looks
/// like from inside `translate` - a true statement about the slot and a false
/// one about the query.
///
/// @param exprs - the expressions to look through
/// @param catalog - where the trees and layouts come from
/// @param params - the values bound for this execution
pub fn fold_expressions(
    exprs: &[&BoundExpr],
    catalog: &dyn TreeCatalog,
    params: &Params,
) -> DbResult<Option<Params>> {
    if params.has_subqueries() {
        return Ok(None);
    }
    let mut found = Vec::new();
    for expr in exprs {
        gather_expression(expr, &mut found);
    }
    if found.is_empty() {
        return Ok(None);
    }
    fill(found, catalog, params).map(Some)
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
        // The `FILTER` and the inner `ORDER BY` hold expressions too, and an
        // uncorrelated subquery in one of them has to be folded like any other
        // (task-1932, M6).
        out.extend(aggregate.filter.iter());
        out.extend(aggregate.order_by.iter().map(|term| &term.expr));
    }
    for window in &select.windows {
        out.extend(window.arguments.iter());
        out.extend(window.filter.iter());
        out.extend(window.partition_by.iter());
        out.extend(window.order_by.iter().map(|term| &term.expr));
    }
    out
}
