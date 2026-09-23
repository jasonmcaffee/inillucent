//! Running a recursive CTE to a fixed point.
//!
//! Invariant: **the consumer above a recursion can stop it.** A recursion with
//! no base case is ordinary SQL - every counter and every series generator is
//! written as one and bounded by a `LIMIT` outside it - so a producer that ran
//! to completion before anything above it saw a row would refuse queries
//! SQLite answers. What guards against a recursion that settles neither way is
//! the request's budget, charged per row as the rows are produced and checked
//! between passes; it is not the mechanism that stops an ordinary one.
//!
//! Extracted from `physical.rs`, whose recorded size this pushed past. Nothing
//! here changed in the move.

use inillucent_base::DbResult;
use inillucent_sql::plan::PhysicalPlan;
use inillucent_tree::datum::OwnedDatum;
use inillucent_value::collation::Collation;

use crate::physical::{run_any, Params, TreeCatalog, WithQueue};
use crate::setop::SetKeys;

/// Fills a recursive CTE and returns every row it produced.
///
/// **The seed arms once, then the step arms until a pass produces nothing.**
/// Each pass runs the step arms over the rows the *previous* pass produced -
/// not over every row so far - which is what makes the work proportional to the
/// rows rather than to their square, and it is SQLite's own rule.
///
/// `UNION` de-duplicates against everything already produced and `UNION ALL`
/// does not, which is also the difference between a graph walk that terminates
/// on a cycle and one that does not.
///
/// @param cte - the FROM term whose queue this is
/// @param seeds - the arms that do not reference the CTE
/// @param steps - the arms that do
/// @param width - how many columns a row holds
/// @param catalog - where the trees and layouts come from
/// @param params - the bound parameters
/// @param limit - the rows the statement above will keep, when it says
pub(crate) fn run_recursive(
    cte: usize,
    seeds: &[(inillucent_sql::ast::CompoundOp, PhysicalPlan)],
    steps: &[(inillucent_sql::ast::CompoundOp, PhysicalPlan)],
    width: usize,
    catalog: &dyn TreeCatalog,
    params: &Params,
    limit: Option<usize>,
) -> DbResult<Vec<Vec<OwnedDatum>>> {
    let distinct = seeds
        .iter()
        .chain(steps.iter())
        .any(|(op, _)| *op == inillucent_sql::ast::CompoundOp::Union);
    let collations = vec![Collation::Binary; width.max(1)];
    let mut produced: Vec<Vec<OwnedDatum>> = Vec::new();
    for (_, arm) in seeds {
        let (rows, _) = run_any(arm, catalog, params)?;
        produced.extend(rows);
    }
    if distinct {
        produced = distinct_rows(produced, &collations, &mut Vec::new());
    }
    let mut answer = produced.clone();
    let mut working = produced;
    // **A recursion with no base case is stopped by the `LIMIT` above it.**
    // `WITH RECURSIVE n(x) AS (SELECT 1 UNION ALL SELECT x+1 FROM n)
    //  SELECT ... FROM (SELECT x FROM n LIMIT 1000)` is how every counter and
    // every series generator is written, and it never terminates on its own -
    // the step arm always produces a row. This ran it to the million-pass guard
    // and then refused a query SQLite answers, which is the same shape
    // fixed for a virtual table's scan: a producer has to be
    // stoppable by the consumer above it rather than run to completion first.
    let enough = |answer: &Vec<Vec<OwnedDatum>>| limit.is_some_and(|want| answer.len() >= want);
    // **There is no pass count here, and there used to be one.** A million
    // passes refused `WITH RECURSIVE n(x) AS (SELECT 1 UNION ALL SELECT x + 1
    // FROM n) SELECT x FROM n LIMIT 1 OFFSET 2000000`, which SQLite answers in
    // thirteen seconds and which a series generator past a million rows is
    // written as; the audit measured the refusal at seventeen seconds with
    // "a recursive CTE did not settle" (task-2066 section 4.2, item 22). A
    // constant that refuses a query the reference answers is a compatibility
    // defect, and the number it refuses at was not derived from anything.
    //
    // What stops a recursion that settles neither way is the request's own
    // budget, which is a better guard than a pass count on both sides: it
    // counts the bytes the recursion is actually holding rather than how many
    // times round it has been, and it is armed on every served path - the MCP
    // server, the command line and the driver all call `budget::arm`. The
    // `materialise` call below charges each row as it is produced, and the
    // `check` here answers a cancellation and the wall clock limit between
    // passes, so a runaway stops at the first of memory, time or the caller
    // hanging up. A library embedder that armed no budget gets what SQLite
    // gives one, which is a query that runs until it is stopped.
    loop {
        inillucent_base::budget::check()?;
        if working.is_empty() || enough(&answer) {
            return Ok(answer);
        }
        let queued = WithQueue {
            inner: catalog,
            cte,
            rows: &working,
        };
        let mut fresh: Vec<Vec<OwnedDatum>> = Vec::new();
        for (_, arm) in steps {
            let (rows, _) = run_any(arm, &queued, params)?;
            fresh.extend(rows);
        }
        if distinct {
            fresh = distinct_rows(fresh, &collations, &mut answer.clone());
        }
        if fresh.is_empty() {
            return Ok(answer);
        }
        // **The answer accumulates across passes (task-1932, H6).** A
        // recursive CTE holds every row it has produced, and nothing bounds
        // the number of passes, so a walk over a graph with a cycle the
        // `UNION` does not close builds until memory runs out rather than
        // until a budget says stop.
        for row in &fresh {
            inillucent_base::budget::materialise(crate::ops::owned_row_bytes(row))?;
        }
        answer.extend(fresh.clone());
        if enough(&answer) {
            return Ok(answer);
        }
        working = fresh;
    }
}

/// Returns the rows that are neither duplicates of each other nor already seen.
///
/// @param rows - the rows a pass produced
/// @param collations - the collation of each column
/// @param seen - the rows already produced, extended with the ones kept
#[allow(clippy::ptr_arg)]
fn distinct_rows(
    rows: Vec<Vec<OwnedDatum>>,
    collations: &[Collation],
    seen: &mut Vec<Vec<OwnedDatum>>,
) -> Vec<Vec<OwnedDatum>> {
    let mut keys = SetKeys::new(collations.to_vec());
    for row in seen.iter() {
        keys.remember(row);
    }
    let mut kept = Vec::with_capacity(rows.len());
    for row in rows {
        if keys.remember(&row) {
            kept.push(row);
        }
    }
    kept
}
