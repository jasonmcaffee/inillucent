//! Running a prepared statement, and combining the arms of a compound one.
//!
//! Invariant: **a compound statement's arms are run in the order the text
//! gives and combined afterwards.** `UNION` and `EXCEPT` are set operations
//! over the whole of each arm, so an implementation that streamed one into the
//! other would answer differently for a `LIMIT`.

use inillucent_base::DbResult;
// `literal_value` is named by path from a dozen call sites in
// `inillucent-engine`, so it stays reachable here after the move to `constant`.
use crate::constant::constant_value;
// `Compiled`, `Slot` and `try_compile` moved to `crate::compiled` to keep this
// file under its recorded ceiling; re-exported here so every existing
// `physical::Slot` / `physical::Compiled` / `physical::try_compile` reference
// - `inillucent-engine`'s `Cached::Select` among them - did not have to move
// with them.
use inillucent_pool::Pool;
use inillucent_sql::ast::{NullOrder, SortOrder};
use inillucent_sql::bind::{BoundExpr, BoundSelect};
use inillucent_sql::plan::{AccessPath, PhysicalPlan};
use inillucent_tree::datum::OwnedDatum;
use inillucent_tree::PagedTree;
use inillucent_value::affinity::Affinity;
use inillucent_value::collation::Collation;

use crate::expr::Expr;
use crate::join::ValuesScan;
use crate::ops::{CollectInto, Limit, Sink, Sort, SortKey};
use crate::setop::{SetKeys, SetKind, SetOp};

/// A catalog that also answers one recursive CTE's queue.
///
/// Everything else is delegated, so the step arm sees exactly the trees, the
/// layouts and the modules the statement sees. Wrapping rather than threading a
/// parameter through every builder is what keeps a recursive query from
/// changing the shape of a signature nothing else uses.
use super::*;

/// Builds a plan and runs it, returning the rows.
///
/// @param plan - the planner's output
/// @param catalog - where the trees and layouts come from
/// @param params - the values bound to `?1`, `?2`, ...
pub fn run(
    plan: &PhysicalPlan,
    catalog: &dyn TreeCatalog,
    params: &Params,
) -> DbResult<(Vec<Vec<OwnedDatum>>, Shape)> {
    let prepared = prepare(plan, catalog, ForcePlan::default())?;
    run_prepared(plan, catalog, &prepared, params)
}
/// Runs an already-prepared statement and returns the rows.
///
/// @param plan - the planner's output
/// @param catalog - where the trees and layouts come from
/// @param prepared - the structural choices [`prepare`] made
/// @param params - the values bound to `?1`, `?2`, ...
pub fn run_prepared(
    plan: &PhysicalPlan,
    catalog: &dyn TreeCatalog,
    prepared: &Prepared,
    params: &Params,
) -> DbResult<(Vec<Vec<OwnedDatum>>, Shape)> {
    let rows = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
    let sink = Box::new(CollectInto::new(std::rc::Rc::clone(&rows)));
    let (mut pipeline, shape) = build_prepared(plan, catalog, prepared, params, sink)?;
    pipeline.run()?;
    // **Taken, not cloned.** The sink is dropped with the pipeline and nothing
    // reads the buffer again, so cloning it copied every row of every answer to
    // hand back a second copy of what was about to be freed. On a one-row answer
    // that is two allocations of the twenty-two an already-prepared `SELECT 1`
    // makes; on a scan it is the whole result set, twice.
    let collected = std::mem::take(&mut *rows.borrow_mut());
    Ok((collected, shape))
}
/// Runs a compound query: two or more arms joined by a set operator.
///
/// **Each arm is an ordinary plan and is run as one.** The set operation is
/// [`crate::setop::SetOp`], the same operator a hand-built pipeline would use,
/// so there is one implementation of what `EXCEPT` means rather than two. What
/// this function adds is the plumbing the push executor cannot express on its
/// own: several sources feeding one sink, and - for `EXCEPT` and `INTERSECT` -
/// the right arm having to be complete before the left arm's first row can be
/// judged.
///
/// The arms are materialised between steps. A compound is a pipeline breaker in
/// every engine that has one, because three of the four operators are set
/// operations over whole rows and a set operation cannot stream; `UNION ALL`
/// could, and running it the same way costs a buffer and keeps the four arms of
/// this function from being four different shapes.
///
/// ## Where the ORDER BY lives
///
/// On the **head** arm's `select`, not on the compound. That is the binder's
/// doing and it is right - SQL does not let an arm of a compound carry its own
/// `ORDER BY`, so the one that is written belongs to the whole - but it means
/// the head arm has to be run with its ordering and its limit *removed*, or
/// `SELECT a FROM t UNION SELECT b FROM u LIMIT 3` would take three rows from
/// the first arm and then union them.
///
/// @param plan - the head arm, carrying the rest in `compounds`
/// @param catalog - where the trees and layouts come from
/// @param params - the values bound to `?1`, `?2`, ...
pub fn run_compound(
    plan: &PhysicalPlan,
    catalog: &dyn TreeCatalog,
    params: &Params,
) -> DbResult<(Vec<Vec<OwnedDatum>>, Shape)> {
    // Every uncorrelated subquery is answered once, here, before anything is
    // built over it. See `crate::subquery` for why it is per execution.
    let folded = crate::subquery::fold(plan, catalog, params)?;
    let params = folded.as_ref().unwrap_or(params);
    let collations: Vec<Collation> = plan
        .select
        .columns
        .iter()
        .map(|column| inillucent_sql::bind::result_collation(&column.expr))
        .collect();
    let (mut rows, shape) = run_arm(plan, catalog, params)?;
    for (op, arm) in &plan.compounds {
        if !arm.compounds.is_empty() {
            // The binder flattens a chain of compounds onto the head, so an arm
            // carrying its own is a shape this has never been handed. Refusing
            // is what the physical pass does with a shape it has not seen.
            return unsupported("a compound query nested inside a compound arm");
        }
        let (right, _) = run_arm(arm, catalog, params)?;
        rows = combine(kind_of(*op), &collations, rows, right)?;
    }
    let ordered = order_compound(&plan.select, rows, &collations, params)?;
    Ok((ordered, shape))
}
/// Runs one arm of a compound, without the compound's own ordering or limit.
///
/// @param plan - the arm
/// @param catalog - where the trees and layouts come from
/// @param params - the bound parameters
fn run_arm(
    plan: &PhysicalPlan,
    catalog: &dyn TreeCatalog,
    params: &Params,
) -> DbResult<(Vec<Vec<OwnedDatum>>, Shape)> {
    let mut arm = plan.clone();
    arm.compounds.clear();
    arm.select.order_by.clear();
    arm.select.limit = None;
    arm.select.offset = None;
    arm.needs_sort = false;
    arm.reverse = false;
    run_any(&arm, catalog, params)
}
/// Runs any planned query, whichever of the three shapes it is.
///
/// The one entry point that knows a compound is several plans and a window
/// query is a plan with a pass on top. Every caller that just wants an answer
/// goes through here; `prepare` and `run_prepared` stay the single-pipeline
/// pair they were, because a `Prepared` is the structural choice for *one*
/// pipeline and neither of the other two shapes has only one.
///
/// @param plan - the planner's output
/// @param catalog - where the trees and layouts come from
/// @param params - the values bound to `?1`, `?2`, ...
pub fn run_any(
    plan: &PhysicalPlan,
    catalog: &dyn TreeCatalog,
    params: &Params,
) -> DbResult<(Vec<Vec<OwnedDatum>>, Shape)> {
    let prepared = prepare_any(plan, catalog)?;
    run_any_prepared(plan, catalog, &prepared, params)
}
/// Returns the one key a plan looks up, when the whole plan is a rowid seek.
///
/// **A write whose `WHERE` is a rowid equality does not need a pipeline to find
/// one row.** `UPDATE t SET ... WHERE id = ?1` plans to a single-stage
/// `RowidSeek`, and running it as a query builds a source, a projection and a
/// collecting sink to hand back one integer the plan already contains. The write
/// gate measured that at 0.89 us of a 1.85 us update, and 17 us of a 47 us one -
/// about half of each, spent deciding something already decided.
///
/// `None` for every other shape, and the caller runs the query. The conditions
/// are all of them: one FROM term, a rowid seek, no residual predicate, no
/// constant filter, no limit and no offset. A plan with any of those does more
/// than name a row, and answering it from the key alone would be answering a
/// different question.
///
/// @param plan - the planner's output
/// @param params - the values bound to `?1`, `?2`, ...
pub fn rowid_seek_key(plan: &PhysicalPlan, params: &Params) -> DbResult<Option<OwnedDatum>> {
    if plan.sources.len() != 1
        || plan.constant_filter.is_some()
        || plan.select.limit.is_some()
        || plan.select.offset.is_some()
        || !plan.compounds.is_empty()
        || plan.residuals.iter().any(Option::is_some)
    {
        return Ok(None);
    }
    let Some(source) = plan.sources.first() else {
        return Ok(None);
    };
    let AccessPath::RowidSeek { key, .. } = &source.path else {
        return Ok(None);
    };
    let held = HeldSpace {
        layouts: Vec::new(),
        types: Vec::new(),
        order: Vec::new(),
    };
    let space = held.view(&[]);
    // Integer affinity, because that is what a rowid comparison applies -
    // `WHERE id = '4'` finds row 4 - and the seek path already applies it. A
    // shortcut that skipped it would answer a question the pipeline would not.
    match constant_value(key, &space, params, Some(Affinity::Integer)) {
        Ok(value) => Ok(Some(value)),
        // The key reads a column, which a rowid seek's should not; the pipeline
        // is the honest answer rather than a guess about what it meant.
        Err(_) => Ok(None),
    }
}
/// Makes the structural choice for any planned query, once.
///
/// **Everything that does not depend on the bound parameters belongs here**, so
/// that a caller re-running a statement pays for the parameters and the work and
/// not for the decision. A compound and a windowed query have no single set of
/// stages - a compound has one per arm, and a window pass plans an inner query
/// of its own - so they get an empty `Prepared` and are re-decided per
/// execution. That is honest rather than tidy: the alternative is a `Prepared`
/// that describes one of several pipelines and is silently wrong about the rest.
///
/// @param plan - the planner's output
/// @param catalog - where the trees and layouts come from
pub fn prepare_any(plan: &PhysicalPlan, catalog: &dyn TreeCatalog) -> DbResult<Prepared> {
    if !plan.compounds.is_empty() || !plan.select.windows.is_empty() {
        return Ok(Prepared {
            stages: Vec::new(),
            forced: ForcePlan::default(),
        });
    }
    prepare(plan, catalog, ForcePlan::default())
}
/// Runs any planned query against a choice [`prepare_any`] already made.
///
/// @param plan - the planner's output
/// @param catalog - where the trees and layouts come from
/// @param prepared - the structural choice
/// @param params - the values bound to `?1`, `?2`, ...
pub fn run_any_prepared(
    plan: &PhysicalPlan,
    catalog: &dyn TreeCatalog,
    prepared: &Prepared,
    params: &Params,
) -> DbResult<(Vec<Vec<OwnedDatum>>, Shape)> {
    if !plan.compounds.is_empty() {
        return run_compound(plan, catalog, params);
    }
    if !plan.select.windows.is_empty() {
        return crate::windowpass::run_windowed(plan, catalog, params);
    }
    run_prepared(plan, catalog, prepared, params)
}
/// Returns the set operation a compound operator names.
///
/// @param op - the binder's operator
fn kind_of(op: inillucent_sql::ast::CompoundOp) -> SetKind {
    match op {
        inillucent_sql::ast::CompoundOp::Union => SetKind::Union,
        inillucent_sql::ast::CompoundOp::UnionAll => SetKind::UnionAll,
        inillucent_sql::ast::CompoundOp::Except => SetKind::Except,
        inillucent_sql::ast::CompoundOp::Intersect => SetKind::Intersect,
    }
}
/// Applies one set operation to two already-materialised arms.
///
/// @param kind - which of the four
/// @param collations - the collation of each result column
/// @param left - the rows so far
/// @param right - the arm being folded in
fn combine(
    kind: SetKind,
    collations: &[Collation],
    left: Vec<Vec<OwnedDatum>>,
    right: Vec<Vec<OwnedDatum>>,
) -> DbResult<Vec<Vec<OwnedDatum>>> {
    let collected = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
    let mut keys = SetKeys::new(collations.to_vec());
    if kind.needs_right_first() {
        // `EXCEPT` and `INTERSECT` cannot judge a left row until the right arm
        // is complete, which is what `needs_right_first` says and why the right
        // arm is reduced to its keys before the left arm is pushed at all.
        ValuesScan::new(right.clone()).run(&mut keys)?;
    }
    let mut operation = SetOp::new(
        kind,
        collations.to_vec(),
        keys,
        Box::new(CollectInto::new(std::rc::Rc::clone(&collected))),
    );
    ValuesScan::new(left).run_without_finish(&mut operation)?;
    if !kind.needs_right_first() {
        // Both unions push both arms through the same operator, because neither
        // needs to know about the other in advance.
        ValuesScan::new(right).run_without_finish(&mut operation)?;
    }
    operation.finish()?;
    let answer = collected.borrow().clone();
    Ok(answer)
}
/// Applies a compound's own `ORDER BY`, `LIMIT` and `OFFSET`.
///
/// @param select - the head arm's bound statement, which carries them
/// @param rows - the combined rows
/// @param collations - the collation of each result column
/// @param params - the bound parameters
fn order_compound(
    select: &BoundSelect,
    rows: Vec<Vec<OwnedDatum>>,
    collations: &[Collation],
    params: &Params,
) -> DbResult<Vec<Vec<OwnedDatum>>> {
    if select.order_by.is_empty() && select.limit.is_none() && select.offset.is_none() {
        return Ok(rows);
    }
    let mut keys = Vec::with_capacity(select.order_by.len());
    for term in &select.order_by {
        // A compound's ordering terms name *output* columns - SQL does not let
        // one reach into an arm's FROM clause - so the binder has already
        // resolved each to an ordinal. Anything else is a shape this cannot
        // order and is refused rather than approximated.
        let BoundExpr::SorterColumn { column } = &term.expr else {
            return unsupported("a compound query ordered by an expression");
        };
        let descending = term.order == SortOrder::Descending;
        keys.push(SortKey {
            column: usize::from(*column),
            descending,
            collation: collations
                .get(usize::from(*column))
                .copied()
                .unwrap_or(term.collation),
            // The binder has already resolved the default, which is NULLS
            // FIRST ascending and NULLS LAST descending.
            nulls_first: match term.nulls {
                NullOrder::First => true,
                NullOrder::Last => false,
            },
        });
    }
    let limit = constant_count(select.limit.as_ref(), params, Negative::NoLimit)?;
    let offset = constant_count(select.offset.as_ref(), params, Negative::Zero)?;
    let collected = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
    let collect: Box<dyn Sink> = Box::new(CollectInto::new(std::rc::Rc::clone(&collected)));
    let sink: Box<dyn Sink> = match (limit, offset) {
        (None, None) => collect,
        (limit, offset) => Box::new(Limit::new(
            limit.unwrap_or(usize::MAX),
            offset.unwrap_or(0),
            collect,
        )),
    };
    let mut head: Box<dyn Sink> = if keys.is_empty() {
        sink
    } else {
        Box::new(Sort::new(keys, sink))
    };
    ValuesScan::new(rows).run(head.as_mut())?;
    let answer = collected.borrow().clone();
    Ok(answer)
}
/// Returns the collation each result column is compared under by `DISTINCT`.
///
/// @param select - the bound statement
pub(crate) fn distinct_collations(select: &BoundSelect) -> Vec<Collation> {
    select
        .columns
        .iter()
        .map(|column| inillucent_sql::bind::result_collation(&column.expr))
        .collect()
}
/// Translates a bound expression that reads the scan's columns.
///
/// @param expr - the bound expression
/// @param space - the joined column space
/// @param params - the bound parameters
/// Returns the expression `sqlite_offset(X)` compiles to.
///
/// The argument has to be a column of a table the statement reads, which is
/// SQLite's own rule - anything else answers NULL there and answers NULL here.
/// What is compiled is a lookup of the row's key in a table of leaf boundaries:
/// one entry per leaf, holding the lowest key on it and the offset of its page
/// in the file. The boundaries are read once, while the statement is being
/// prepared, and the cost is one page read per leaf rather than one per row.
///
/// @param arguments - the call's single argument
/// @param space - the joined column space
/// @param params - the statement's bound parameters
/// @param frame - which of the two row shapes the expression is over
pub(crate) fn row_offset(
    arguments: &[BoundExpr],
    space: &Space<'_>,
    params: &Params,
    frame: Frame<'_>,
) -> DbResult<Expr> {
    let Some(BoundExpr::Column { source, .. }) = arguments.first() else {
        return Ok(Expr::Literal(OwnedDatum::Null));
    };
    let source = *source;
    let Some(catalog) = space.catalog else {
        return Ok(Expr::Literal(OwnedDatum::Null));
    };
    let Some(stage) = space.stages.iter().find(|stage| stage.term == source) else {
        return Ok(Expr::Literal(OwnedDatum::Null));
    };
    let Some(layout) = space.layouts.get(source) else {
        return Ok(Expr::Literal(OwnedDatum::Null));
    };
    let Some(rowid_column) = layout.rowid else {
        return Ok(Expr::Literal(OwnedDatum::Null));
    };
    let (Some(tree), Some(pool)) = (catalog.tree(stage.root), catalog.pool_for(stage.root)) else {
        return Ok(Expr::Literal(OwnedDatum::Null));
    };
    let boundaries = leaf_boundaries(tree, pool, rowid_column)?;
    let rowid = translate(&BoundExpr::Rowid { source }, space, params, frame)?;
    Ok(Expr::RowOffset {
        boundaries: std::sync::Arc::new(boundaries),
        rowid: Box::new(rowid),
    })
}
/// Returns the lowest key on each leaf and where that leaf sits in the file.
///
/// Sorted by key, so a lookup is a binary search. A leaf covers a contiguous
/// run of keys, so the boundary is the whole of what a lookup needs.
///
/// @param tree - the table's tree
/// @param pool - the pool its pages live in
/// @param rowid_column - which tree column holds the row's key
fn leaf_boundaries(
    tree: &PagedTree,
    pool: &Pool,
    rowid_column: usize,
) -> DbResult<Vec<(i64, i64)>> {
    let page_size = tree.page_size() as i64;
    let mut boundaries: Vec<(i64, i64)> = Vec::new();
    let mut page = tree.first_leaf();
    let mut seen = 0u64;
    while !page.is_none() {
        let guard = pool.fetch(page)?;
        let leaf = inillucent_tree::leaf::LeafRef::parse(&guard)?;
        let mut lowest: Option<i64> = None;
        for row in 0..leaf.row_count() {
            if let Ok(inillucent_tree::datum::Datum::Int(key)) =
                leaf.value_at(inillucent_tree::leaf::Hit::Sorted(row), rowid_column)
            {
                lowest = Some(lowest.map_or(key, |held: i64| held.min(key)));
            }
        }
        for row in 0..leaf.delta_count() {
            if let Ok(inillucent_tree::datum::Datum::Int(key)) =
                leaf.value_at(inillucent_tree::leaf::Hit::Delta(row), rowid_column)
            {
                lowest = Some(lowest.map_or(key, |held: i64| held.min(key)));
            }
        }
        boundaries.push((
            lowest.unwrap_or(i64::MIN),
            (page.0 as i64).saturating_mul(page_size),
        ));
        let next = leaf.right_sibling();
        drop(guard);
        page = next;
        seen = seen.saturating_add(1);
        // The same guard every walk in the tree crate carries: a sibling chain
        // that pointed at itself would otherwise not come back.
        if seen > tree.leaf_count().saturating_add(2) {
            break;
        }
    }
    boundaries.sort_by_key(|(key, _)| *key);
    Ok(boundaries)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Each of SQL's four compound operators maps onto its own set operation.
    ///
    /// **Four values and not three (T3, task-1962).** `UNION` deduplicates and
    /// `UNION ALL` does not, and collapsing them would silently drop every
    /// duplicate row a `UNION ALL` was written to keep.
    #[test]
    fn the_four_compound_operators_are_four_set_operations() {
        use inillucent_sql::ast::CompoundOp;
        assert_eq!(kind_of(CompoundOp::Union), SetKind::Union);
        assert_eq!(kind_of(CompoundOp::UnionAll), SetKind::UnionAll);
        assert_eq!(kind_of(CompoundOp::Except), SetKind::Except);
        assert_eq!(kind_of(CompoundOp::Intersect), SetKind::Intersect);
        assert_ne!(
            kind_of(CompoundOp::Union),
            kind_of(CompoundOp::UnionAll),
            "`UNION` removes duplicates and `UNION ALL` keeps them"
        );
    }
}
