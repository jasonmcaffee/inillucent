//! Correlated subqueries, answered per row without a second execution engine.
//!
//! Invariant: a correlated block is planned **once** and run per row against
//! bound parameters. It is never re-planned, never re-bound, and never
//! evaluated by anything but the ordinary planner and the ordinary pipeline -
//! so `EXISTS (SELECT 1 FROM b WHERE b.team = a.team)` reaches the same index
//! probe `SELECT 1 FROM b WHERE b.team = ?1` reaches.
//!
//! ## Why an operator and not an expression
//!
//! [`crate::expr::Eval`] is `Send + Sync`, deliberately: it is what lets a
//! compiled expression be held by a prepared statement and shared. A catalog is
//! neither, so an expression node cannot reach the trees and a subquery cannot
//! be evaluated from inside one. That bound is not relaxed here;
//! instead the value is computed *beside* the row, one column per
//! correlated block, and the expression reads that column - which is exactly
//! how a virtual table's auxiliary functions (`bm25(t)`, `score(t)`) already
//! reach an expression that cannot call a module.
//!
//! ## How the outer row reaches the block
//!
//! The block reads columns of FROM terms it does not own. Each such reference
//! is replaced, once, by a parameter numbered past everything a statement can
//! write, and the joined-row column that feeds it is recorded. Per row the
//! operator binds those parameters and runs the plan.
//!
//! This is decorrelation's alternative and it is chosen for one reason:
//! decorrelation into a semi-join, an anti-join or a left join answers `EXISTS`
//! and `NOT EXISTS`, and does not answer a scalar block in a result column -
//! `SELECT a.name, (SELECT b.region FROM b WHERE b.team = a.team) FROM a` has
//! no join that produces it without also changing what the projection means.
//! One mechanism that answers all three shapes is worth more than two that
//! answer some of them, and what it costs is a plan *execution* per row rather
//! than a plan *compilation* per row.
//!
//! ## What a statement with none pays
//!
//! Nothing. [`correlations_of`] returns an empty list the moment the planner's
//! own `subqueries` flag is clear, and the pipeline then never builds this
//! operator.

use inillucent_base::DbResult;
use inillucent_sql::bind::{BoundExpr, BoundSelect, SourceRows, SubqueryKind};
use inillucent_sql::plan::{plan_select_with, Levers, PhysicalPlan};
use inillucent_tree::datum::{Datum, OwnedDatum};

use crate::batch::{Batch, Vector};
use crate::expr::Eval;
use crate::ops::{Flow, Sink};
use crate::physical::{prepare_any, run_any_prepared_limited, Params, Prepared, TreeCatalog};

/// The first parameter number a correlation may use.
///
/// SQLite's `SQLITE_MAX_VARIABLE_NUMBER` is 32,766 and this engine's limits
/// agree, so a number past it cannot collide with one a statement wrote.
/// Colliding would be a wrong answer rather than an error - the block would
/// read the application's value instead of the outer row's - which is why the
/// separation is a stated constant rather than "one past the highest we saw".
const FIRST_CORRELATION_PARAMETER: u32 = crate::physical::ENGINE_PARAMETER_BASE;

/// One correlated block, planned once.
pub struct Correlation {
    /// The binder's statement-wide number for the subquery.
    pub id: usize,
    /// Which of the three forms the block is used as.
    kind: SubqueryKind,
    /// Whether `NOT` was written.
    negated: bool,
    /// The block, with its outer references replaced by parameters.
    plan: PhysicalPlan,
    /// The structural choice for that plan, made once.
    ///
    /// **`answer` used to call `run_any`, which is `prepare_any` and then
    /// `run_any_prepared`, on every outer row** (task-2066 §4.3.1).
    /// `prepare_any` runs the covering candidate trial - a speculative
    /// pipeline build per candidate tree - so an `EXISTS` over five thousand
    /// outer rows made five thousand structural decisions about the same inner
    /// query against the same schema.
    ///
    /// The module comment above says a correlated block is planned once and
    /// run per row. That was true of the plan and not of the prepare.
    prepared: Prepared,
    /// For each replaced reference: the joined-row column that feeds it, and
    /// the parameter number it was given.
    feeds: Vec<(usize, u32)>,
}

/// Appends one column per correlated subquery to every row that passes.
///
/// It sits above every join and below every filter that reads what it
/// computes, because the value is read by the `WHERE` and by the projection
/// alike, and both of those run after the last join has widened the row.
///
/// ## The conjuncts that do not read a block are tested first
///
/// **This operator used to answer every block for every row the joins
/// produced, and the `WHERE` threw most of those rows away afterwards**
/// (task-2076). `SELECT a FROM t WHERE t.v % 100 = 0 AND EXISTS (...)` ran the
/// inner query once per row of `t` rather than once per row that survives
/// `t.v % 100 = 0`. A block's prepare was hoisted out of the row loop in
/// task-2066 section 4.3.1; its run cannot be, and that run was paid on rows
/// nobody wanted.
///
/// So the chain hands this operator the `WHERE` conjuncts that hold no
/// subquery at all, and a row that fails one of them is dropped before any
/// block is answered for it. That is sound for one reason: this operator is a
/// map. It emits exactly one row for every row it receives and only appends
/// columns, so a predicate that reads none of the appended columns gives the
/// same verdict on either side of it. A conjunct that names any subquery,
/// correlated or not, stays in the filters above, which is the conservative
/// half of the rule and the one that needs no argument.
///
/// SQLite 3.53.4 answers the same way: a block that fails on a row the cheap
/// conjunct rejects does not fail the query, whichever order the two terms are
/// written in. The test that checks it here is in `new_engine_subquery.rs`,
/// and it failed on this engine before the change.
pub struct Correlated<'t> {
    /// The blocks, in the order their columns are appended.
    correlations: Vec<Correlation>,
    /// The `WHERE` conjuncts that read no block, compiled against the row as
    /// the joins produce it. A row answers a block only when all of them are
    /// true, which is exactly when the filter above would have kept it.
    gate: Vec<Box<dyn Eval>>,
    /// Where the trees and layouts come from.
    catalog: &'t dyn TreeCatalog,
    /// The statement's own bound parameters, which a block may also read.
    params: Params,
    downstream: Box<dyn Sink + 't>,
}

impl<'t> Correlated<'t> {
    /// Returns the operator over blocks [`correlations_of`] has prepared.
    ///
    /// @param correlations - the prepared blocks
    /// @param gate - the conjuncts a row must pass before any block is answered
    /// @param catalog - where the trees and layouts come from
    /// @param params - the statement's bound parameters
    /// @param downstream - what to push widened rows into
    pub fn new(
        correlations: Vec<Correlation>,
        gate: Vec<Box<dyn Eval>>,
        catalog: &'t dyn TreeCatalog,
        params: &Params,
        downstream: Box<dyn Sink + 't>,
    ) -> Correlated<'t> {
        Correlated {
            correlations,
            gate,
            catalog,
            // **Without the outer statement's folded subqueries.** A block run
            // from here is a statement of its own and folds its own; carrying
            // the outer table in would tell it the fold had already happened
            // and leave its slots empty, which reads from inside `translate` as
            // "a correlated subquery" - a true sentence about the slot and a
            // false one about the query.
            params: params.without_subqueries(),
            downstream,
        }
    }
}

impl Sink for Correlated<'_> {
    fn push(&mut self, batch: &Batch<'_>) -> DbResult<Flow> {
        let width = batch.columns.len();
        // **Once per batch, not once per row** (task-2066 §4.3.1). See
        // `Correlation::answer` for what that clone cost.
        let mut bound = self.params.clone();
        for nth in 0..batch.live() {
            if !passes(&self.gate, batch, nth)? {
                continue;
            }
            let mut row: Vec<OwnedDatum> =
                Vec::with_capacity(width.saturating_add(self.correlations.len()));
            for column in 0..width {
                row.push(OwnedDatum::from_datum(&batch.value(nth, column)?));
            }
            for position in 0..self.correlations.len() {
                let answer = match self.correlations.get(position) {
                    Some(correlation) => correlation.answer(self.catalog, &mut bound, &row)?,
                    None => OwnedDatum::Null,
                };
                row.push(answer);
            }
            let borrowed: Vec<Datum<'_>> = row.iter().map(OwnedDatum::borrow).collect();
            let columns: Vec<Vector<'_>> =
                borrowed.iter().map(|value| Vector::Const(*value)).collect();
            if self.downstream.push(&Batch::new(1, columns))? == Flow::Stop {
                return Ok(Flow::Stop);
            }
        }
        Ok(Flow::Continue)
    }

    fn finish(&mut self) -> DbResult<()> {
        self.downstream.finish()
    }

    fn reset(&mut self) -> DbResult<()> {
        self.downstream.reset()
    }
}

/// Whether one row passes every conjunct of a gate.
///
/// The same test [`crate::ops::Filter`] applies: a row is kept only when the
/// predicate is definitely true, so a NULL rejects it exactly as it would in
/// the `WHERE` the conjunct came from.
///
/// @param gate - the compiled conjuncts
/// @param batch - the rows as the joins produced them
/// @param nth - which live row of the batch to test
fn passes(gate: &[Box<dyn Eval>], batch: &Batch<'_>, nth: usize) -> DbResult<bool> {
    for predicate in gate {
        let verdict = predicate.value(batch, nth)?;
        if crate::expr::truth(&verdict.get()) != Some(true) {
            return Ok(false);
        }
    }
    Ok(true)
}

impl Correlation {
    /// Returns what this block answers for one outer row.
    ///
    /// **The parameter set is the caller's and is written into** (task-2066
    /// §4.3.1). This used to clone it per row, and
    /// [`FIRST_CORRELATION_PARAMETER`] is 100,000, so that clone copied a
    /// hundred thousand `OwnedDatum` slots to write one of them. Measured on
    /// 5,000 outer rows with an `EXISTS` whose probe matches nothing - so
    /// nothing but the setup runs - it was **1.47 milliseconds an outer row**.
    ///
    /// Each block writes only its own numbers, and they are past anything a
    /// statement can write, so sharing one set between the blocks of one batch
    /// cannot let one of them read another's value.
    ///
    /// @param catalog - where the trees and layouts come from
    /// @param bound - the statement's parameters, to write this row's feeds into
    /// @param row - the joined row so far
    pub fn answer(
        &self,
        catalog: &dyn TreeCatalog,
        bound: &mut Params,
        row: &[OwnedDatum],
    ) -> DbResult<OwnedDatum> {
        for (column, number) in &self.feeds {
            bound.set(
                *number,
                row.get(*column).cloned().unwrap_or(OwnedDatum::Null),
            );
        }
        // **One row is all any of the three forms reads.** `Exists` asks
        // whether the block produced anything and `Scalar` takes the first row
        // and drops the rest, so the unlimited run was reading an inner result
        // set to throw it away. `In` is refused in `correlations_of` and is
        // stated below so a variant added later is a compilation error.
        let (rows, _shape) =
            run_any_prepared_limited(&self.plan, catalog, &self.prepared, bound, Some(1))?;
        let mut column = rows
            .into_iter()
            .map(|row| row.into_iter().next().unwrap_or(OwnedDatum::Null));
        Ok(match self.kind {
            SubqueryKind::Exists => {
                OwnedDatum::Int(i64::from(column.next().is_some() != self.negated))
            }
            // SQLite's rule for a block that produced nothing is NULL, which is
            // what a missing row already reads as.
            SubqueryKind::Scalar => column.next().unwrap_or(OwnedDatum::Null),
            // Refused in `correlations_of`; stated here so a variant added
            // later is a compilation error rather than a NULL.
            SubqueryKind::In => OwnedDatum::Null,
        })
    }
}

/// Finds every correlated block a list of expressions holds, and prepares it.
///
/// The companion to [`correlations_of`], for the statements that have
/// expressions but no plan to hang them on: an `UPDATE`'s assignments and a
/// `RETURNING` clause are evaluated by the write path directly, so nothing ever
/// built a `PhysicalPlan` for them.
///
/// @param exprs - the expressions to look through
/// @param resolve - which row column an outer reference reads
pub fn correlations_in(
    exprs: &[&BoundExpr],
    catalog: &dyn TreeCatalog,
    resolve: &dyn Fn(&BoundExpr) -> Option<usize>,
) -> DbResult<Vec<Correlation>> {
    let mut found: Vec<(usize, SubqueryKind, bool, BoundSelect)> = Vec::new();
    for expr in exprs {
        gather_expression(expr, &mut found);
    }
    prepare_blocks(found, catalog, resolve)
}

/// Finds every correlated block in a plan and prepares it.
///
/// Returns an empty list when there is none, which is the common case and the
/// one that must cost nothing.
///
/// @param plan - the planner's output
/// @param resolve - which joined-row column an outer reference reads
pub fn correlations_of(
    plan: &PhysicalPlan,
    catalog: &dyn TreeCatalog,
    resolve: &dyn Fn(&BoundExpr) -> Option<usize>,
) -> DbResult<Vec<Correlation>> {
    if !plan.subqueries {
        return Ok(Vec::new());
    }
    let mut found: Vec<(usize, SubqueryKind, bool, BoundSelect)> = Vec::new();
    gather_plan(plan, &mut found);
    prepare_blocks(found, catalog, resolve)
}

/// Whether a plan holds any correlated block at all.
///
/// **A walk, and nothing else** (task-2066 §4.3.1). `compiled.rs` asks only
/// whether the list is empty, and it used to ask by building the list - which
/// plans every block and, since blocks are prepared now, would prepare them
/// too. The comment above that call promises a shape it refuses costs nothing
/// beyond the walk, and this is what keeps that true.
///
/// @param plan - the planner's output
pub fn has_correlations(plan: &PhysicalPlan) -> bool {
    if !plan.subqueries {
        return false;
    }
    let mut found: Vec<(usize, SubqueryKind, bool, BoundSelect)> = Vec::new();
    gather_plan(plan, &mut found);
    !found.is_empty()
}

/// Rewrites each gathered block's outer references into parameters, and plans it.
///
/// @param found - the correlated blocks
/// @param resolve - which row column an outer reference reads
fn prepare_blocks(
    found: Vec<(usize, SubqueryKind, bool, BoundSelect)>,
    catalog: &dyn TreeCatalog,
    resolve: &dyn Fn(&BoundExpr) -> Option<usize>,
) -> DbResult<Vec<Correlation>> {
    let mut prepared = Vec::with_capacity(found.len());
    for (id, kind, negated, block) in found {
        if kind == SubqueryKind::In {
            // An `IN` over a correlated block needs the whole list rather than
            // one value, and this operator carries one column per block. Named
            // rather than answered wrongly.
            return crate::physical::unsupported("a correlated IN subquery");
        }
        let mut block = block;
        let owned = owned_sources(&block);
        let mut feeds = Vec::new();
        let mut next = FIRST_CORRELATION_PARAMETER;
        let mut unresolved = false;
        inillucent_sql::rewrite::rewrite_select(&mut block, &mut |expr: &mut BoundExpr| {
            let outer = match expr {
                BoundExpr::Column { source, .. } | BoundExpr::Rowid { source } => *source,
                _ => return,
            };
            if owned.contains(&outer) {
                return;
            }
            match resolve(expr) {
                Some(column) => {
                    feeds.push((column, next));
                    *expr = BoundExpr::Parameter(next);
                    next = next.saturating_add(1);
                }
                None => unresolved = true,
            }
        });
        if unresolved {
            return crate::physical::unsupported(
                "a correlated subquery reading a column the joined row does not carry",
            );
        }
        // Prepared here, once, which is the whole of section 4.3.1: the plan
        // and the schema decide the structural choice and neither depends on
        // the outer row.
        let plan = plan_select_with(block, Levers::default());
        let choice = prepare_any(&plan, catalog)?;
        prepared.push(Correlation {
            id,
            kind,
            negated,
            plan,
            prepared: choice,
            feeds,
        });
    }
    Ok(prepared)
}

/// Returns the statement-wide source numbers a block's own FROM terms have.
///
/// Everything else a column reference names belongs to an enclosing block, and
/// that is precisely what makes the subquery correlated.
///
/// @param block - the nested query
fn owned_sources(block: &BoundSelect) -> Vec<usize> {
    let mut owned = Vec::new();
    collect_sources(block, &mut owned);
    owned
}

/// Collects the source numbers of a block and of everything nested in it.
///
/// @param block - the query to walk
/// @param into - the numbers found so far
fn collect_sources(block: &BoundSelect, into: &mut Vec<usize>) {
    for source in &block.sources {
        into.push(source.id);
        match &source.rows {
            SourceRows::Subquery(inner) => collect_sources(inner, into),
            SourceRows::Recursive(body) => {
                for (_, arm) in body.seeds.iter().chain(body.steps.iter()) {
                    collect_sources(arm, into);
                }
            }
            SourceRows::Table | SourceRows::RecursiveSelf { .. } => {}
        }
    }
    for (_, arm) in &block.compounds {
        collect_sources(arm, into);
    }
}

/// Collects every correlated block of a planned statement.
///
/// @param plan - the planner's output
/// @param into - the blocks found so far
fn gather_plan(plan: &PhysicalPlan, into: &mut Vec<(usize, SubqueryKind, bool, BoundSelect)>) {
    gather_select(&plan.select, into);
    for residual in plan.residuals.iter().flatten() {
        gather_expression(residual, into);
    }
    if let Some(filter) = &plan.constant_filter {
        gather_expression(filter, into);
    }
}

/// Collects every correlated block of a query.
///
/// @param select - the query to walk
/// @param into - the blocks found so far
fn gather_select(select: &BoundSelect, into: &mut Vec<(usize, SubqueryKind, bool, BoundSelect)>) {
    for column in &select.columns {
        gather_expression(&column.expr, into);
    }
    if let Some(filter) = &select.filter {
        gather_expression(filter, into);
    }
    if let Some(having) = &select.having {
        gather_expression(having, into);
    }
    for term in &select.group_by {
        gather_expression(term, into);
    }
    for term in &select.order_by {
        gather_expression(&term.expr, into);
    }
    for source in &select.sources {
        if let Some(constraint) = &source.constraint {
            gather_expression(constraint, into);
        }
    }
    // **The aggregates and the windows, which this walk did not have
    // (task-1932, M6).** `select.aggregates` is a list of its own, beside
    // `select.columns` rather than inside it, so a subquery written as an
    // aggregate's argument was never seen here. It was therefore never
    // recognised as a correlated block, its slot was never filled, and
    // `translate` reported the empty slot as `unsupported("a correlated
    // subquery used as a value")` - a true statement about the slot and a false
    // one about the query. `SELECT team, SUM((SELECT b.amount FROM b WHERE
    // b.team = a.team)) FROM a GROUP BY team` was refused outright.
    //
    // The `FILTER` and the inner `ORDER BY` are walked for the same reason
    // `bind::gather_columns` walks them: they read the row, so a subquery in
    // one of them is correlated in exactly the way an argument's is.
    for aggregate in &select.aggregates {
        for argument in &aggregate.arguments {
            gather_expression(argument, into);
        }
        if let Some(filter) = &aggregate.filter {
            gather_expression(filter, into);
        }
        for term in &aggregate.order_by {
            gather_expression(&term.expr, into);
        }
    }
    for window in &select.windows {
        for argument in &window.arguments {
            gather_expression(argument, into);
        }
        if let Some(filter) = &window.filter {
            gather_expression(filter, into);
        }
        for term in &window.partition_by {
            gather_expression(term, into);
        }
        for term in &window.order_by {
            gather_expression(&term.expr, into);
        }
    }
}

/// Collects every correlated block one expression holds.
///
/// @param expr - the expression to walk
/// @param into - the blocks found so far
fn gather_expression(expr: &BoundExpr, into: &mut Vec<(usize, SubqueryKind, bool, BoundSelect)>) {
    if let BoundExpr::Subquery {
        id,
        kind,
        negated,
        block,
        ..
    } = expr
    {
        if !block.correlations.is_empty() && !into.iter().any(|(held, ..)| *held == *id) {
            into.push((*id, *kind, *negated, (**block).clone()));
        }
    }
    for child in expr.children() {
        gather_expression(child, into);
    }
}
