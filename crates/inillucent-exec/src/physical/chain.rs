//! Building the operator chain a statement runs as.
//!
//! Invariant: **the chain is built once and the sources are what a re-run
//! rebuilds.** `build_chain` assembles the sinks above the scan; a second
//! execution with different parameters replaces only the source whose key the
//! parameters decide, which is what `Compiled` measures the saving of.

use inillucent_base::error::misuse;
use inillucent_base::DbResult;
// `literal_value` is named by path from a dozen call sites in
// `inillucent-engine`, so it stays reachable here after the move to `constant`.
use crate::constant::constant_value;
use crate::constant::literal_value_in;
// `Compiled`, `Slot` and `try_compile` moved to `crate::compiled` to keep this
// file under its recorded ceiling; re-exported here so every existing
// `physical::Slot` / `physical::Compiled` / `physical::try_compile` reference
// - `inillucent-engine`'s `Cached::Select` among them - did not have to move
// with them.
use inillucent_pool::Pool;
use inillucent_sql::ast::{NullOrder, SortOrder};
use inillucent_sql::bind::BoundExpr;
use inillucent_sql::plan::{AccessPath, AggregationMode, PhysicalPlan};
use inillucent_tree::datum::OwnedDatum;
use inillucent_value::collation::Collation;

use crate::expr::{compile, Expr, StaticType};
use crate::ops::{
    AdjacentDistinct, Distinct, Filter, HashAggregate, Limit, Project, SimpleAggregate, Sink, Sort,
    SortKey, StreamAggregate, TopN,
};
use crate::paged::{FullScan, PointProbe, ReverseScan, SkipScan, SpanScan};
use crate::scan::Projection;

/// A catalog that also answers one recursive CTE's queue.
///
/// Everything else is delegated, so the step arm sees exactly the trees, the
/// layouts and the modules the statement sees. Wrapping rather than threading a
/// parameter through every builder is what keeps a recursive query from
/// changing the shape of a signature nothing else uses.
use super::*;

/// The column space one statement's stages define.
///
/// Every field is a borrow rather than an owned buffer. That is what lets a
/// [`Statement`] rebuild only its *source* on each execution: the space is
/// derived from the prepared stages and the catalog's layouts, neither of which
/// depends on the bound parameters, so it is computed once and viewed again
/// rather than rebuilt. When it owned its `types` and `layouts`, re-deriving it
/// per execution was three allocations that a re-run does not need.
pub(crate) struct Space<'c> {
    /// The stages, in order.
    pub(crate) stages: &'c [PreparedStage],
    /// Each stage's layout.
    pub(crate) layouts: &'c [std::rc::Rc<SourceLayout>],
    /// The static type of every column of the joined row.
    pub(crate) types: &'c [StaticType],
    /// The tree columns the *joined* rows arrive sorted by, when they do.
    pub(crate) order: &'c [usize],
    /// Where an application-registered function's body is looked up.
    ///
    /// `None` on the write path and on the two constant folds with no catalog in
    /// scope; a registered scalar there refuses by name - roadmap item 13.
    pub(crate) catalog: Option<&'c dyn TreeCatalog>,
    /// Which joined-row column each correlated subquery's answer sits in.
    ///
    /// Empty for every statement that has none, which is nearly all of them.
    /// A correlated block cannot be folded into a constant - it reads the row
    /// being tested - so `crate::correlate` computes it beside the row and this
    /// is the map an expression finds it through, exactly as a module's
    /// auxiliary functions are found.
    pub(crate) correlations: &'c [(usize, usize)],
}
impl Space<'_> {
    /// Returns the joined-row column a bound column reference names.
    ///
    /// A FROM term may be two stages, so the column is looked for in the table
    /// stage first and the index stage second: the table carries every column
    /// and the index only some, and preferring the table means a query that
    /// reads a column the index happens to hold still reads it from wherever
    /// the row was actually fetched.
    ///
    /// @param source - the planner FROM term
    /// @param declared - the column's declared position, which is what every
    ///   builder of a [`SourceLayout`] indexes its `slots` by
    /// Returns the joined-row column one correlated subquery's answer sits in.
    ///
    /// @param id - the binder's statement-wide number for the subquery
    pub(crate) fn correlated(&self, id: usize) -> Option<usize> {
        self.correlations
            .iter()
            .find(|(held, _)| *held == id)
            .map(|(_, column)| *column)
    }

    pub(crate) fn column(&self, source: usize, declared: usize) -> Option<usize> {
        let mut found = None;
        for (index, stage) in self.stages.iter().enumerate() {
            if stage.source != source {
                continue;
            }
            let layout = self.layouts.get(index)?;
            if let Some(Some(tree_column)) = layout.slots.get(declared) {
                let resolved = stage.offset.saturating_add(*tree_column);
                if stage.is_lookup {
                    return Some(resolved);
                }
                found = Some(resolved);
            }
        }
        found
    }

    /// Returns the joined-row column holding a FROM term's rowid.
    ///
    /// @param source - the planner FROM term
    /// Returns the column one of a module's auxiliary functions was put in.
    ///
    /// @param source - the FROM term the call is about
    /// @param name - the function's folded name
    /// @param arguments - the arguments after the table, which are part of the
    ///   identity: two calls of one name with different arguments are two
    ///   answers and so two slots
    pub(crate) fn virtual_function(
        &self,
        source: usize,
        name: &[u8],
        arguments: &[inillucent_sql::bind::BoundExpr],
    ) -> Option<usize> {
        for (index, stage) in self.stages.iter().enumerate() {
            if stage.source != source {
                continue;
            }
            let position = stage.functions.iter().position(|(held, held_arguments)| {
                held.as_slice() == name && held_arguments.as_slice() == arguments
            })?;
            let layout = self.layouts.get(index)?;
            // The functions sit after the declared columns and the rowid, in
            // the order the reads were met.
            let before = layout
                .slots
                .len()
                .saturating_add(usize::from(layout.rowid.is_some()));
            return Some(stage.offset.saturating_add(before).saturating_add(position));
        }
        None
    }

    pub(crate) fn rowid(&self, source: usize) -> Option<usize> {
        for (index, stage) in self.stages.iter().enumerate() {
            if stage.source != source {
                continue;
            }
            let layout = self.layouts.get(index)?;
            if let Some(rowid) = layout.rowid {
                return Some(stage.offset.saturating_add(rowid));
            }
        }
        None
    }
}
/// Builds a pipeline for a planned statement.
///
/// @param plan - the planner's output
/// @param catalog - where the trees and layouts come from
/// @param params - the values bound to `?1`, `?2`, ...
/// @param sink - the end of the pipeline
pub fn build<'t>(
    plan: &PhysicalPlan,
    catalog: &'t dyn TreeCatalog,
    params: &Params,
    sink: Box<dyn Sink>,
) -> DbResult<(Pipeline<'t>, Shape)> {
    let prepared = prepare(plan, catalog, ForcePlan::default())?;
    build_prepared(plan, catalog, &prepared, params, sink)
}
/// Builds a pipeline over already-chosen stages, with the `EXPLAIN` listing.
///
/// The same pipeline [`build_prepared`] builds, plus `Shape::operators`. It is
/// a second entry point rather than a flag for the reason [`source_for_run`]
/// is a second entry point beside the description: a caller that wants the
/// listing is asking a different question from one that wants the pipeline,
/// and every caller that wants it is a diagnostic - `inillucent-readgate`
/// prints it beside SQLite's plan, and the plan campaign test compares the
/// chains one lever change produces.
///
/// @param plan - the planner's output
/// @param catalog - where the trees and layouts come from
/// @param prepared - the structural choices [`prepare`] made
/// @param params - the values bound to `?1`, `?2`, ...
/// @param sink - the end of the pipeline
pub fn build_prepared_described<'t>(
    plan: &PhysicalPlan,
    catalog: &'t dyn TreeCatalog,
    prepared: &Prepared,
    params: &Params,
    sink: Box<dyn Sink>,
) -> DbResult<(Pipeline<'t>, Shape)> {
    build_prepared_with(plan, catalog, prepared, params, sink, Listing::kept())
}
/// Builds a pipeline over already-chosen stages.
///
/// @param plan - the planner's output
/// @param catalog - where the trees and layouts come from
/// @param prepared - the structural choices [`prepare`] made
/// @param params - the values bound to `?1`, `?2`, ...
/// @param sink - the end of the pipeline
pub fn build_prepared<'t>(
    plan: &PhysicalPlan,
    catalog: &'t dyn TreeCatalog,
    prepared: &Prepared,
    params: &Params,
    sink: Box<dyn Sink>,
) -> DbResult<(Pipeline<'t>, Shape)> {
    build_prepared_with(plan, catalog, prepared, params, sink, Listing::dropped())
}
/// Builds a pipeline over already-chosen stages, keeping the listing or not.
///
/// @param plan - the planner's output
/// @param catalog - where the trees and layouts come from
/// @param prepared - the structural choices [`prepare`] made
/// @param params - the values bound to `?1`, `?2`, ...
/// @param sink - the end of the pipeline
/// @param listing - whether to render the operator chain as it is built
fn build_prepared_with<'t>(
    plan: &PhysicalPlan,
    catalog: &'t dyn TreeCatalog,
    prepared: &Prepared,
    params: &Params,
    sink: Box<dyn Sink>,
    listing: Listing,
) -> DbResult<(Pipeline<'t>, Shape)> {
    // Every uncorrelated subquery is answered once, here, before anything is
    // built over it. See `crate::subquery` for why it is per execution.
    let folded = crate::subquery::fold(plan, catalog, params)?;
    let params = folded.as_ref().unwrap_or(params);
    let held = space_of(catalog, prepared)?;
    let mut space = held.view(&prepared.stages);
    space.catalog = Some(catalog);
    let chain = build_chain(plan, catalog, prepared, &space, params, sink, listing)?;
    // The source is built without its description and described beside itself,
    // so the two cannot drift apart while a listing nobody asked for costs
    // neither the `format!` nor the `String`.
    let source = source_for_run(plan, catalog, &space, params, prepared, chain.limit)?;
    let mut listing = chain.operators;
    listing.add(|| describe_source(prepared));
    let mut operators = listing.into_lines();
    operators.reverse();
    Ok((
        Pipeline {
            source,
            head: chain.head,
            pool: source_pool(catalog, prepared),
        },
        Shape {
            names: chain.names,
            operators,
        },
    ))
}
/// The layouts, types and key order a statement's stages define.
///
/// Held apart from [`Space`] because a [`Statement`] computes it once and takes
/// a view of it on every execution: none of it depends on the bound parameters,
/// so re-deriving it per execution would be three allocations spent to arrive
/// at the same answer.
pub(crate) struct HeldSpace {
    /// Each stage's layout.
    ///
    /// Owned rather than borrowed from the catalog, because a materialised
    /// subquery's layout is synthesised on its stage and there is nothing in the
    /// catalog to borrow it from - and a `Statement` owns both its `Prepared`
    /// and its space, which a borrow between them would make self-referential.
    /// It is built once per prepare and never per execution.
    pub(crate) layouts: Vec<std::rc::Rc<SourceLayout>>,
    /// The static type of every column of the joined row.
    pub(crate) types: Vec<StaticType>,
    /// The tree columns the joined rows arrive sorted by, when they do.
    pub(crate) order: Vec<usize>,
}
impl HeldSpace {
    /// Returns a view of this space over a statement's stages.
    ///
    /// @param stages - the prepared stages, outermost first
    pub(crate) fn view<'a>(&'a self, stages: &'a [PreparedStage]) -> Space<'a> {
        self.view_with(stages, &[])
    }

    /// Returns a view that also knows where the correlated answers sit.
    ///
    /// @param stages - the prepared stages, outermost first
    /// @param correlations - each block's number and the cell holding its answer
    pub(crate) fn view_with<'a>(
        &'a self,
        stages: &'a [PreparedStage],
        correlations: &'a [(usize, usize)],
    ) -> Space<'a> {
        Space {
            stages,
            layouts: &self.layouts,
            types: &self.types,
            order: &self.order,
            catalog: None,
            correlations,
        }
    }
}
/// Returns the column space a statement's stages define.
///
/// @param catalog - where the layouts come from
/// @param prepared - the structural choices [`prepare`] made
pub(crate) fn space_of(catalog: &dyn TreeCatalog, prepared: &Prepared) -> DbResult<HeldSpace> {
    let mut layouts = Vec::with_capacity(prepared.stages.len());
    let mut types: Vec<StaticType> = Vec::new();
    for stage in &prepared.stages {
        let layout = match &stage.layout {
            Some(held) => held,
            None => catalog.layout(stage.root).ok_or_else(|| {
                misuse(format!("no layout imported for root page {}", stage.root))
            })?,
        }
        .clone();
        types.extend(layout.types.iter().copied());
        layouts.push(layout);
    }
    // Only the outermost stage's key order survives into the joined row: a
    // nested loop emits its inner matches grouped by the outer row, which
    // preserves the outer order and destroys any inner one.
    //
    // **A table fetch behind a non-covering index seek is not a nested loop.**
    // It is one row per index entry, in the index's own order,
    // so it preserves the order rather than destroying it. This used to ask for
    // exactly one stage, which a non-covering seek never is - so the ordering an
    // index was chosen *for* was then not believed, `ORDER BY` fell to a `TopN`,
    // and `TopN` is a pipeline breaker: it consumes every row of the range
    // before it emits one. The measured shape is unmistakable, because the cost
    // falls as the starting key advances - on a 60,000-row table, the same
    // `WHERE id > ? ORDER BY id LIMIT 2000`:
    //
    // | starting after | before | after |
    // |---|---:|---:|
    // | row 1 | 88.3 ms | 1.1 ms |
    // | row 20,000 | 19.6 ms | 1.1 ms |
    // | row 40,000 | 11.0 ms | 1.1 ms |
    // | row 58,000 | 3.7 ms | 1.1 ms |
    //
    // Work proportional to what is *left* rather than to the limit, which makes
    // keyset paging quadratic in the table: 601,862 chunks at a page of 2,000 is
    // 90 million row materialisations instead of 601,862. It is what stopped
    // an early `inillucent migrate` run from finishing one table in 25 minutes.
    let ordered_stages = prepared.stages.iter().skip(1).all(|stage| stage.is_lookup);
    let order = match (prepared.stages.first(), layouts.first()) {
        (Some(stage), Some(layout)) if ordered_stages => {
            if stage.kind == AccessKind::Reverse {
                Vec::new()
            } else {
                layout.key_columns.clone()
            }
        }
        _ => Vec::new(),
    };
    Ok(HeldSpace {
        layouts,
        types,
        order,
    })
}
/// The `EXPLAIN` listing a chain writes as it assembles itself, or nothing.
///
/// **Because every caller but two throws the listing away (task-2026).**
/// `Shape::operators` is the built operator chain rendered as text, and nothing
/// in the engine reads it: `EXPLAIN` and `EXPLAIN QUERY PLAN` answer from
/// `PhysicalPlan::describe` and `Prepared::describe`, which work off the plan
/// rather than off the chain. The two readers are `inillucent-readgate`, which
/// prints this engine's chain beside SQLite's, and the plan campaign test.
/// Every other caller - including the gate's own `prepare.trivial`, which
/// builds a pipeline on every one of its iterations - paid for a `Vec<String>`
/// and a `format!` per operator and dropped the result: three of the
/// twenty-one allocations compiling `SELECT 1` made.
///
/// The line is passed as a closure rather than as a `String` so that a listing
/// nobody asked for costs the `format!` nothing as well as the push. A
/// `Vec<String>` argument could not do that: the caller would have formatted
/// before it got here.
pub(crate) struct Listing {
    lines: Option<Vec<String>>,
}

impl Listing {
    /// Returns a listing that keeps what it is given.
    pub(crate) fn kept() -> Listing {
        Listing {
            lines: Some(Vec::new()),
        }
    }

    /// Returns a listing that keeps nothing and formats nothing.
    pub(crate) fn dropped() -> Listing {
        Listing { lines: None }
    }

    /// Adds one operator's line, when a listing is being kept.
    ///
    /// @param line - how to render it, called only when it is wanted
    pub(crate) fn add(&mut self, line: impl FnOnce() -> String) {
        if let Some(lines) = self.lines.as_mut() {
            lines.push(line());
        }
    }

    /// Returns the lines, sink first, or an empty vector.
    pub(crate) fn into_lines(self) -> Vec<String> {
        self.lines.unwrap_or_default()
    }
}

/// Everything a built operator chain is, short of the source that drives it.
struct Chain<'t> {
    /// The head of the chain: what the source pushes into.
    head: Box<dyn Sink + 't>,
    /// The operator descriptions, sink first; the source is appended last.
    /// Empty throughout when the caller did not ask for a listing.
    operators: Listing,
    /// The output column names.
    names: Vec<Vec<u8>>,
    /// The statement's constant `LIMIT`, which the source may use.
    limit: Option<usize>,
}
/// Builds every operator above the source.
///
/// Separated from [`build_prepared`] because a [`Statement`] builds this once
/// and rebuilds only the source per execution. The split is also what makes the
/// rebinding test possible: the parameter reads this function makes are the
/// ones that would be baked into the chain, and a statement is only re-runnable
/// when there are none.
///
/// @param plan - the planner's output
/// @param catalog - where the trees and layouts come from
/// @param prepared - the structural choices [`prepare`] made
/// @param space - the joined column space
/// @param params - the values bound to `?1`, `?2`, ...
/// @param sink - the end of the pipeline
/// Everything [`build_upper`] built: the source-independent half of a chain.
///
/// Kept apart from [`Chain`] because every field here is genuinely `'static` -
/// which is what [`Compiled`] needs. [`Statement`] widens this into a chain
/// with the inner stages and any correlated block wrapped around it, which is
/// where a borrow of the catalog first appears.
pub(crate) struct Upper {
    /// Every operator above the source, holding no borrow of anything.
    pub(crate) head: Box<dyn Sink>,
    /// The operator descriptions, sink first, or nothing when the caller did
    /// not ask for a listing.
    pub(crate) operators: Listing,
    /// The output column names.
    pub(crate) names: Vec<Vec<u8>>,
    /// The statement's constant `LIMIT`, which the source may use.
    pub(crate) limit: Option<usize>,
    /// The statement's correlated blocks, prepared but not yet wrapped around
    /// `head` - building [`crate::correlate::Correlated`] needs a catalog
    /// borrowed for the chain's own lifetime, which is exactly what this
    /// function does not take.
    pub(crate) correlations: Vec<crate::correlate::Correlation>,
    /// The `WHERE` conjuncts that read no subquery, compiled for
    /// [`crate::correlate::Correlated`] to test before it answers a block.
    /// Empty whenever `correlations` is.
    pub(crate) gate: Vec<Box<dyn crate::expr::Eval>>,
}
/// Builds every operator above the source, short of the inner join stages and
/// the correlation operator - the part of a chain that holds no borrow of the
/// catalog it was built against.
///
/// Split out of [`build_chain`] so [`Compiled`] - kept with no lifetime at all
/// so it can sit in an `Rc` across executions - can build this part once.
/// `catalog` is borrowed only long enough to resolve a function to its body
/// and translate a residual predicate; an index nested loop, a correlated
/// block or a lateral module - every place that would hold onto the borrow -
/// is built by [`build_chain`] instead, over what this returns.
///
/// **The ordered list of the operators it may insert (task-1962, A8).** It was
/// 383 lines, one inline block per operator, each reading a different part of
/// the same forty line setup. Each operator is a `push_*` function now and the
/// setup is [`correlated_columns`], [`plan_outputs`], [`grouping_of`] and
/// [`already_sorted`], gathered into an [`Upward`] the pushes borrow. There is
/// no `push_window`: a windowed query never reaches this builder, because
/// `run_any` routes it to `crate::windowpass::run_windowed` and
/// [`refuse_unhandled`] refuses one that arrives anyway.
///
/// @param plan - the planner's output
/// @param catalog - where a registered function's body comes from, borrowed
///   only for this call
/// @param prepared - the structural choices [`prepare`] made
/// @param space - the joined column space
/// @param params - the values bound to `?1`, `?2`, ...
/// @param sink - the end of the pipeline
/// The result columns and the sort keys, translated once.
///
/// **What every operator above the source reads (task-1962, A8).** The
/// projection, the sorter, the trim and the `DISTINCT` each need some part of
/// this, and while they were written inline they each reached into the same
/// forty lines of locals. It is one value now, computed before the first
/// operator is pushed.
struct Outputs {
    /// The result columns, followed by any `ORDER BY` term that is not one.
    projected: Vec<Expr>,
    /// How many of `projected` the caller asked for. The rest are carried
    /// through the sort and trimmed afterwards.
    result_width: usize,
    /// The sort keys, as positions into `projected`.
    sort_keys: Vec<SortKey>,
    /// Whether `projected` grew past `result_width`, so the trim is needed.
    needs_trim: bool,
}

/// Everything the `push_*` functions read.
///
/// **A gathered context rather than a fifteen argument call (task-1962, A8).**
/// Each `push_*` function needs a different four or five of these, and passing
/// them positionally would have put the longest parameter lists in the crate
/// next to the function the split was meant to make readable. The struct is
/// built once by [`build_upper`] and borrowed by each push.
struct Upward<'a> {
    /// The planner's output.
    plan: &'a PhysicalPlan,
    /// The structural choices `prepare` made.
    prepared: &'a Prepared,
    /// The joined column space, widened for any correlated block.
    space: &'a Space<'a>,
    /// The values bound to `?1`, `?2`, ...
    params: &'a Params,
    /// The column types a row has as the source produces it.
    scan_types: &'a [StaticType],
    /// The column groups the walk already orders rows by.
    scan_order: Vec<Vec<usize>>,
    /// The column types a row has where the projection reads it, which is the
    /// aggregate's output rather than the scan's when there is one.
    projection_types: Vec<StaticType>,
    /// The `GROUP BY` terms, translated.
    group_exprs: Vec<Expr>,
    /// The collation each `GROUP BY` term compares under.
    group_collations: Vec<Collation>,
    /// How many `GROUP BY` terms there are, which is how wide the key half of
    /// an aggregated row is.
    group_width: usize,
    /// Whether the walk already brings each group's rows together.
    grouped_walk: bool,
    /// Whether the source is a skip scan.
    skipping: bool,
    /// Whether the rows already arrive in the order `ORDER BY` asks for.
    sorted_already: bool,
    /// The statement's constant `LIMIT`, if it has one.
    limit: Option<usize>,
    /// The statement's constant `OFFSET`, zero when it has none.
    offset: usize,
    /// The result columns and the sort keys.
    outputs: Outputs,
    /// The `WHERE` conjuncts the filters above the correlation operator test,
    /// or `None` when there is no such operator and the plan's own residuals
    /// are used as they are.
    filters_above: Option<Vec<BoundExpr>>,
}

/// Whether the source walks its tree backwards.
///
/// @param prepared - the structural choices `prepare` made
fn is_reverse_scan(prepared: &Prepared) -> bool {
    prepared
        .stages
        .first()
        .map(|stage| stage.kind == AccessKind::Reverse)
        .unwrap_or(false)
}

/// Whether the source is a skip scan over a leading index prefix.
///
/// @param prepared - the structural choices `prepare` made
fn is_skip_scan(prepared: &Prepared) -> bool {
    prepared
        .stages
        .first()
        .map(|stage| stage.kind == AccessKind::Skip)
        .unwrap_or(false)
}

/// The statement's correlated blocks, and the columns they are answered in.
///
/// **A correlated block is answered beside the row, not inside an
/// expression.** Each one becomes a column appended to the joined row, and
/// `crate::correlate` is the operator that fills it - so the `WHERE` and the
/// projection read a column rather than reaching for a catalog that
/// `expr::Eval`'s `Send + Sync` bound puts out of reach. `correlations_of`
/// returns nothing for a statement with none, which is nearly all of them, and
/// the operator is then never built.
///
/// The third value is the widened column types, and it is empty for a
/// statement with no correlated block: **only widened when there is something
/// to widen.** `prepare.trivial` is 1,337 ns end to end and a `Vec` per
/// prepare is a measurable share of it, so the common case keeps borrowing the
/// space's own types.
///
/// @param plan - the planner's output
/// @param catalog - where the trees and layouts come from
/// @param space - the joined column space before any widening
fn correlated_columns(
    plan: &PhysicalPlan,
    catalog: &dyn TreeCatalog,
    space: &Space<'_>,
) -> DbResult<(
    Vec<crate::correlate::Correlation>,
    Vec<(usize, usize)>,
    Vec<StaticType>,
)> {
    let outer = Space {
        stages: space.stages,
        layouts: space.layouts,
        types: space.types,
        order: space.order,
        catalog: Some(catalog),
        correlations: &[],
    };
    let correlations =
        crate::correlate::correlations_of(plan, catalog, &|expr: &BoundExpr| match expr {
            BoundExpr::Column { source, column, .. } => outer.column(*source, *column as usize),
            BoundExpr::Rowid { source } => outer.rowid(*source),
            _ => None,
        })?;
    let joined_width = space.types.len();
    let columns: Vec<(usize, usize)> = correlations
        .iter()
        .enumerate()
        .map(|(position, correlation)| (correlation.id, joined_width.saturating_add(position)))
        .collect();
    let widened_types: Vec<StaticType> = if correlations.is_empty() {
        Vec::new()
    } else {
        let mut widened = space.types.to_vec();
        widened.extend(std::iter::repeat_n(StaticType::Unknown, correlations.len()));
        widened
    };
    Ok((correlations, columns, widened_types))
}

/// Translates the result columns and the `ORDER BY` terms.
///
/// Both are read in the space that exists *after* any aggregation, which is
/// what `translate_post` means by post. A term that is not already a result
/// column is appended to the projection, carried through the sort as an extra
/// column, and trimmed afterwards.
///
/// @param plan - the planner's output
/// @param space - the joined column space
/// @param params - the values bound to `?1`, `?2`, ...
fn plan_outputs(plan: &PhysicalPlan, space: &Space<'_>, params: &Params) -> DbResult<Outputs> {
    let select = &plan.select;
    let group_width = select.group_by.len();
    let mut projected: Vec<Expr> = Vec::with_capacity(select.columns.len());
    for column in &select.columns {
        projected.push(translate_post(
            &column.expr,
            select,
            space,
            params,
            group_width,
        )?);
    }
    let result_width = projected.len();
    let mut sort_keys: Vec<SortKey> = Vec::new();
    for term in &select.order_by {
        let translated = translate_post(&term.expr, select, space, params, group_width)?;
        let existing = projected
            .iter()
            .position(|held| same_expr(held, &translated));
        let column = match existing {
            Some(index) => index,
            None => {
                // **Carried through the sort, and left out of what makes a row
                // distinct.** `SELECT DISTINCT a FROM t ORDER BY b` is an
                // ordinary query SQLite answers; refusing it was the safe thing
                // to do while the de-duplication compared every column of the
                // row, because the carried `b` would have made rows distinct
                // that the caller's select list does not. `Distinct::over`
                // compares the leading `result_width` columns instead.
                projected.push(translated);
                projected.len().saturating_sub(1)
            }
        };
        sort_keys.push(SortKey {
            column,
            descending: term.order == SortOrder::Descending,
            collation: term.collation,
            // SQLite's default is NULLS FIRST ascending and NULLS LAST
            // descending, which is what reversing an ordering that puts NULL
            // lowest already gives. An explicit clause is the case that has to
            // be carried, and the binder has already resolved the default.
            nulls_first: match term.nulls {
                NullOrder::First => true,
                NullOrder::Last => false,
            },
        });
    }
    let needs_trim = projected.len() > result_width;
    Ok(Outputs {
        projected,
        result_width,
        sort_keys,
        needs_trim,
    })
}

/// The `GROUP BY` terms, their collations, and whether the walk already groups.
///
/// **Adjacency has no direction, and `space.order` deliberately does.** A
/// reverse walk brings each group's rows together exactly as a forward one
/// does, but `space_of` empties `order` for a reverse scan - correctly, since
/// the rows arrive in the *reverse* of that order and no rule reading it may
/// assume otherwise. Asking `is_scan_prefix` alone therefore said "not grouped
/// by the walk", the aggregate became a hash one, and it emitted its groups in
/// key order: `SELECT k, count(*) FROM t GROUP BY k ORDER BY k DESC` came back
/// *ascending*, with the planner having already skipped the sorter because the
/// walk was supposed to answer the ordering.
///
/// So the adjacency question is asked of the planner for a reverse walk, which
/// decided it from the access path rather than from the direction.
///
/// @param plan - the planner's output
/// @param prepared - the structural choices `prepare` made
/// @param space - the joined column space
/// @param params - the values bound to `?1`, `?2`, ...
/// @param scan_order - the column groups the walk already orders rows by
fn grouping_of(
    plan: &PhysicalPlan,
    prepared: &Prepared,
    space: &Space<'_>,
    params: &Params,
    scan_order: &[Vec<usize>],
) -> DbResult<(Vec<Expr>, Vec<Collation>, bool)> {
    let select = &plan.select;
    let group_exprs = select
        .group_by
        .iter()
        .map(|expr| translate_scan(expr, space, params))
        .collect::<DbResult<Vec<Expr>>>()?;
    // `GROUP BY team` on a `COLLATE NOCASE` column has one group for `blue`
    // and `Blue`; grouping by bytes has two, and the counts are then wrong
    // rather than merely differently ordered.
    let group_collations: Vec<Collation> =
        select.group_by.iter().map(expression_collation).collect();
    let grouped_walk = plan.aggregation == AggregationMode::Grouped
        && !prepared.forced.hash_group
        && (is_scan_prefix(&group_exprs, &group_collations, scan_order)
            || (is_reverse_scan(prepared) && plan.grouped_walk));
    Ok((group_exprs, group_collations, grouped_walk))
}

/// Whether the rows already arrive in the order the `ORDER BY` asks for.
///
/// **A skip scan produces the distinct prefix in *ascending* order**, which
/// answers an ascending `ORDER BY` over that prefix and nothing else. The skip
/// scan branch used to say only "skipping", and `SELECT k, count(*) FROM t
/// GROUP BY k ORDER BY k DESC` therefore skipped its sorter and came back
/// ascending - a wrong answer rather than a slow one, and one no
/// single-direction test could see. It asks the same two conditions the
/// forward walk asks, because it is the same claim about the same walk.
///
/// @param plan - the planner's output
/// @param prepared - the structural choices `prepare` made
/// @param outputs - the result columns and the sort keys
/// @param scan_order - the column groups the walk already orders rows by
/// @param grouped_walk - whether the walk already brings each group together
fn already_sorted(
    plan: &PhysicalPlan,
    prepared: &Prepared,
    outputs: &Outputs,
    scan_order: &[Vec<usize>],
    grouped_walk: bool,
) -> bool {
    let sort_keys = &outputs.sort_keys;
    // The one condition the forward walk and the skip scan both ask.
    let ascending = || {
        !sort_keys.is_empty()
            && sort_keys.iter().all(|term| !term.descending)
            && output_is_sorted_by(
                sort_keys,
                &outputs.projected,
                plan,
                scan_order,
                grouped_walk,
            )
    };
    // A non-default NULL placement is a real ordering requirement, and no scan
    // order satisfies it by accident.
    let default_nulls = sort_keys
        .iter()
        .all(|term| term.nulls_first != term.descending);
    let answered_by_the_walk = if !default_nulls {
        false
    } else if is_reverse_scan(prepared) {
        // A reverse scan produces descending key order, so a descending
        // ORDER BY over the key is satisfied by the direction rather than by a
        // sorter. `plan.reverse` is only ever set when the planner already
        // decided that, which is why the condition is the planner's answer
        // rather than a second derivation of it.
        !sort_keys.is_empty() && !plan.needs_sort
    } else {
        ascending()
    };
    answered_by_the_walk || (is_skip_scan(prepared) && ascending())
}

/// What the *source* may stop after, which is not the statement's LIMIT.
///
/// A source that stops early is only right when nothing between it and the
/// `Limit` operator changes how many rows there are: a residual filter drops
/// some, a join multiplies them, `DISTINCT` and an aggregate collapse them,
/// and an `OFFSET` throws the first ones away - so `LIMIT 2 OFFSET 1` needs
/// three rows read and returns one.
///
/// It was the bare `LIMIT`, which made `WHERE id <= 5 ORDER BY id DESC LIMIT 2
/// OFFSET 1` answer one row instead of two.
///
/// **A sorter is the same argument and was missing from it** (task-2066
/// §4.1.1). A `LIMIT` may be pushed below an `ORDER BY` only into an operator
/// that preserves the order the `ORDER BY` names, and this asked every other
/// question but that one. What it cost was the commonest recursive query there
/// is:
///
/// ```sql
/// WITH RECURSIVE r(n) AS (SELECT 1 UNION ALL SELECT n+1 FROM r WHERE n<5)
/// SELECT n FROM r ORDER BY n DESC LIMIT 1;
/// ```
///
/// answering `1` where SQLite answers `5`, because the bound reached
/// `run_recursive`, which reads it as "stop generating" - so one row was
/// generated and then sorted. `LIMIT 2` answered `2, 1` against `5, 4`. The
/// suite never saw it because any operator between the CTE and the sort blocks
/// the pushdown, so the `WHERE`, `DISTINCT` and `GROUP BY` forms are all
/// correct, and the one query with no such operator is the hierarchy walk for
/// the deepest node, which returned the root at exit 0.
///
/// **This applies to every source the bound reaches, with no exception, and
/// neither of the other two consumers loses anything** (task-2069):
///
/// - A **reverse scan** keeps its bound on every plan the planner can produce.
///   `already_sorted` answers `!plan.needs_sort` for one, and `plan.reverse` is
///   only set when `ordering_provided` returned `Some(true)`, which is exactly
///   when `needs_sort` is false. `ordering_provided` also refuses a
///   non-natural NULL placement, so the `default_nulls` gate cannot fail for a
///   reverse plan either.
/// - The **HNSW probe** is unaffected, because its bound is the `depth` the
///   planner copied from the `LIMIT` when it chose the probe, not this. The
///   chain limit only ever reached `iterative_candidates` on plans with no
///   residual, and those return before reading it - which is why the parameter
///   is gone from that function rather than being passed `None` forever.
///
/// @param plan - the planner's output
/// @param prepared - the structural choices `prepare` made
/// @param limit - the statement's own constant `LIMIT`
/// @param sorted_already - whether the rows already arrive in the `ORDER BY`'s
///   order, so that no sorter will be put above the source
/// @param has_sort_keys - whether the statement names an `ORDER BY` at all
fn source_limit_of(
    plan: &PhysicalPlan,
    prepared: &Prepared,
    limit: Option<usize>,
    sorted_already: bool,
    has_sort_keys: bool,
) -> Option<usize> {
    limit.filter(|_| {
        (!has_sort_keys || sorted_already)
            && plan.residuals.iter().all(Option::is_none)
            && plan.constant_filter.is_none()
            && prepared.stages.len() == 1
            && !plan.select.distinct
            && plan.aggregation == AggregationMode::None
            && plan.select.windows.is_empty()
    })
}

/// Puts the `Limit` operator on, and records it in the description.
///
/// @param chain - what it will push into
/// @param operators - the description, collected sink first
/// @param limit - how many rows to pass on
/// @param offset - how many to throw away first
fn push_limit(
    chain: Box<dyn Sink>,
    operators: &mut Listing,
    limit: usize,
    offset: usize,
) -> Box<dyn Sink> {
    operators.add(|| format!("LIMIT {limit} OFFSET {offset}"));
    Box::new(Limit::new(limit, offset, chain))
}

/// Puts the projection that drops the carried sort columns on, if there are any.
///
/// @param chain - what it will push into
/// @param operators - the description, collected sink first
/// @param up - everything the operators are built from
fn push_trim(
    chain: Box<dyn Sink>,
    operators: &mut Listing,
    up: &Upward<'_>,
) -> DbResult<Box<dyn Sink>> {
    if !up.outputs.needs_trim {
        return Ok(chain);
    }
    operators.add(|| "TRIM".to_string());
    Ok(Box::new(Project::new(
        trim(up.outputs.result_width, up.scan_types)?,
        chain,
    )))
}

/// Returns somewhere the sorter may spill, when the catalog offers one.
///
/// **`None` is the ordinary answer and it is not a failure** (task-2066
/// §4.3.6): `TreeCatalog::spill` is defaulted to `None`, the write path and
/// the two constant folds have no catalog at all, and a sort with nowhere to
/// spill behaves exactly as it did before spilling existed.
///
/// @param up - what the chain is being built from
fn spill_of(up: &Upward<'_>) -> Option<std::rc::Rc<dyn crate::spill::Spill>> {
    up.space.catalog.and_then(|catalog| catalog.spill())
}

/// Puts the sorter and the `LIMIT` on, in whichever arrangement is right.
///
/// The two are decided together because `TopN` fuses them: a bounded sort keeps
/// `limit + offset` rows and never holds the whole input, which is why a
/// separate `push_limit` above a separate sorter would be the slower shape
/// rather than a tidier one. `prepared.forced.full_sort` is how a test asks for
/// the unfused pair anyway.
///
/// The trim goes *below* the sorter when there is one, because the columns it
/// drops are the ones the sort keys read.
///
/// @param chain - what it will push into
/// @param operators - the description, collected sink first
/// @param up - everything the operators are built from
fn push_sort(
    chain: Box<dyn Sink>,
    operators: &mut Listing,
    up: &Upward<'_>,
) -> DbResult<Box<dyn Sink>> {
    let sort_keys = &up.outputs.sort_keys;
    if sort_keys.is_empty() || up.sorted_already {
        let chain = match up.limit {
            Some(limit) => push_limit(chain, operators, limit, up.offset),
            None => chain,
        };
        return push_trim(chain, operators, up);
    }
    let chain = push_trim(chain, operators, up)?;
    let Some(limit) = up.limit else {
        operators.add(|| "SORT".to_string());
        return Ok(Box::new(
            Sort::new(sort_keys.clone(), chain).spilling_to(spill_of(up)),
        ));
    };
    let bounded = limit.saturating_add(up.offset);
    if bounded > TopN::MAX_LIMIT || up.prepared.forced.full_sort {
        let chain = push_limit(chain, operators, limit, up.offset);
        operators.add(|| "SORT".to_string());
        return Ok(Box::new(
            Sort::new(sort_keys.clone(), chain).spilling_to(spill_of(up)),
        ));
    }
    let chain = if up.offset > 0 {
        push_limit(chain, operators, limit, up.offset)
    } else {
        chain
    };
    operators.add(|| format!("TOP {bounded}"));
    Ok(Box::new(TopN::new(sort_keys.clone(), bounded, chain)))
}

/// Puts the `DISTINCT` operator on, if the statement asks for one.
///
/// A skip scan has already produced distinct rows, so it gets none.
///
/// @param chain - what it will push into
/// @param operators - the description, collected sink first
/// @param up - everything the operators are built from
fn push_distinct(chain: Box<dyn Sink>, operators: &mut Listing, up: &Upward<'_>) -> Box<dyn Sink> {
    let select = &up.plan.select;
    if !select.distinct || up.skipping {
        return chain;
    }
    // The collation of each output column. A `DISTINCT` over a `COLLATE
    // NOCASE` column keeps one of `blue` and `Blue`, and one that compared
    // bytes keeps both.
    let collations: Vec<Collation> = select
        .columns
        .iter()
        .map(|column| expression_collation(&column.expr))
        .collect();
    let width = up.outputs.result_width;
    if up.plan.aggregation == AggregationMode::None
        && !up.prepared.forced.hash_distinct
        && is_scan_prefix(&up.outputs.projected, &collations, &up.scan_order)
    {
        operators.add(|| "DISTINCT ADJACENT".to_string());
        return Box::new(AdjacentDistinct::over(collations, width, chain));
    }
    operators.add(|| "DISTINCT HASH".to_string());
    Box::new(Distinct::over(collations, width, chain))
}

/// Puts the projection on.
///
/// @param chain - what it will push into
/// @param operators - the description, collected sink first
/// @param up - everything the operators are built from
fn push_projection(
    chain: Box<dyn Sink>,
    operators: &mut Listing,
    up: &Upward<'_>,
) -> DbResult<Box<dyn Sink>> {
    // A skip scan hands up exactly the projected key columns, already in
    // output order, so the projection over it reads column i for column i.
    let compiled = if up.skipping {
        (0..up.outputs.projected.len())
            .map(|index| compile(&Expr::Column(index), &up.projection_types))
            .collect::<DbResult<Vec<_>>>()?
    } else {
        up.outputs
            .projected
            .iter()
            .map(|expr| compile(expr, &up.projection_types))
            .collect::<DbResult<Vec<_>>>()?
    };
    operators.add(|| "PROJECT".to_string());
    Ok(Box::new(Project::new(compiled, chain)))
}

/// Puts the `HAVING` filter on, if the statement has one.
///
/// `HAVING` filters *groups*, so it sits between the aggregate and the
/// projection: it reads accumulators and `GROUP BY` keys, which is the same
/// space a result column reads, and it runs before the projection throws away
/// the columns it needs. Building it here rather than beside the `WHERE`
/// filters is the whole of the difference between the two clauses.
///
/// @param chain - what it will push into
/// @param operators - the description, collected sink first
/// @param up - everything the operators are built from
fn push_having(
    chain: Box<dyn Sink>,
    operators: &mut Listing,
    up: &Upward<'_>,
) -> DbResult<Box<dyn Sink>> {
    let select = &up.plan.select;
    let Some(having) = &select.having else {
        return Ok(chain);
    };
    let translated = translate_post(having, select, up.space, up.params, up.group_width)?;
    operators.add(|| "FILTER HAVING".to_string());
    Ok(Box::new(Filter::new(
        compile(&translated, &up.projection_types)?,
        chain,
    )))
}

/// Puts the aggregate on, if the statement aggregates.
///
/// @param chain - what it will push into
/// @param operators - the description, collected sink first
/// @param up - everything the operators are built from
fn push_aggregate(
    chain: Box<dyn Sink>,
    operators: &mut Listing,
    up: &Upward<'_>,
) -> DbResult<Box<dyn Sink>> {
    let select = &up.plan.select;
    match up.plan.aggregation {
        AggregationMode::None => Ok(chain),
        AggregationMode::Whole => {
            operators.add(|| "AGGREGATE".to_string());
            Ok(Box::new(SimpleAggregate::new(
                aggregate_specs(select, up.space, up.params, up.scan_types)?,
                chain,
            )))
        }
        AggregationMode::Grouped => {
            let keys = up
                .group_exprs
                .iter()
                .map(|expr| compile(expr, up.scan_types))
                .collect::<DbResult<Vec<_>>>()?;
            let specs = aggregate_specs(select, up.space, up.params, up.scan_types)?;
            let collations = up.group_collations.clone();
            if up.grouped_walk {
                operators.add(|| "GROUP STREAM".to_string());
                Ok(Box::new(StreamAggregate::new(
                    keys, collations, specs, chain,
                )))
            } else {
                operators.add(|| "GROUP HASH".to_string());
                Ok(Box::new(HashAggregate::new(keys, collations, specs, chain)))
            }
        }
    }
}

/// Puts the predicates the access paths did not consume on.
///
/// `select.filter` is the *whole* `WHERE`, and `plan.residuals` is what the
/// access paths did not consume. Testing both re-tests every predicate the
/// planner turned into a seek or a range - `WHERE key BETWEEN ?1 AND ?1+200`
/// was evaluated once per row of a range whose bounds already excluded
/// everything outside it - so only the residuals are tested here. That is also
/// what the bytecode VM does, and it is not merely a speed question: a
/// predicate with `random()` in it would answer differently the second time.
///
/// The operator chain in `Shape::operators` is what showed this: it printed
/// `RANGE tree 3 -> FILTER -> AGGREGATE` and the `FILTER` had nothing to do.
///
/// @param chain - what it will push into
/// @param operators - the description, collected sink first
/// @param up - everything the operators are built from
fn push_filters(
    chain: Box<dyn Sink>,
    operators: &mut Listing,
    up: &Upward<'_>,
) -> DbResult<Box<dyn Sink>> {
    let mut chain = chain;
    if let Some(above) = &up.filters_above {
        for term in above {
            let translated = translate_scan(term, up.space, up.params)?;
            chain = Box::new(Filter::new(compile(&translated, up.scan_types)?, chain));
            operators.add(|| "FILTER RESIDUAL".to_string());
        }
        return Ok(chain);
    }
    if let Some(constant) = &up.plan.constant_filter {
        let translated = translate_scan(constant, up.space, up.params)?;
        chain = Box::new(Filter::new(compile(&translated, up.scan_types)?, chain));
        operators.add(|| "FILTER CONSTANT".to_string());
    }
    for residual in up.plan.residuals.iter().flatten() {
        let translated = translate_scan(residual, up.space, up.params)?;
        chain = Box::new(Filter::new(compile(&translated, up.scan_types)?, chain));
        operators.add(|| "FILTER RESIDUAL".to_string());
    }
    Ok(chain)
}

/// Splits a correlated statement's `WHERE` into the conjuncts that read a
/// subquery and the ones that do not.
///
/// **The second list is tested before any block is answered** (task-2076), by
/// [`crate::correlate::Correlated`] itself; its module comment gives the
/// argument for why that cannot change an answer. The first list stays in the
/// filters above that operator.
///
/// The rule is by the expression tree and nothing else: a conjunct holding a
/// `Subquery` node anywhere, correlated or folded, stays above. A conjunct
/// that is not split by `AND` at its top - `a.v = 1 OR EXISTS (...)` - is one
/// conjunct, holds a subquery, and stays above whole.
///
/// Only called when the statement has a correlated block, so a statement
/// without one allocates nothing here.
///
/// @param plan - the planner's output
fn place_around_correlation(plan: &PhysicalPlan) -> (Vec<BoundExpr>, Vec<BoundExpr>) {
    let mut above = Vec::new();
    let mut below = Vec::new();
    let predicates = plan
        .constant_filter
        .iter()
        .chain(plan.residuals.iter().flatten());
    for predicate in predicates {
        for term in inillucent_sql::plan::conjunction(predicate) {
            if inillucent_sql::plan::expression_holds_subquery(&term) {
                above.push(term);
            } else {
                below.push(term);
            }
        }
    }
    (above, below)
}

/// Compiles the conjuncts the correlation operator tests before it answers a
/// block.
///
/// Compiled against the row as the joins produce it, which is the row that
/// operator receives: none of these conjuncts reads a correlated column, so
/// none of them needs the widened types.
///
/// @param below - the conjuncts [`place_around_correlation`] put below
/// @param space - the widened column space, which resolves base columns
/// @param params - the values bound to `?1`, `?2`, ...
/// @param joined_types - the column types before any widening
fn compile_gate(
    below: &[BoundExpr],
    space: &Space<'_>,
    params: &Params,
    joined_types: &[StaticType],
) -> DbResult<Vec<Box<dyn crate::expr::Eval>>> {
    let mut gate = Vec::with_capacity(below.len());
    for term in below {
        let translated = translate_scan(term, space, params)?;
        gate.push(compile(&translated, joined_types)?);
    }
    Ok(gate)
}

/// Builds the whole of a statement's chain above the source, and returns it
/// with what a later execution needs to rebuild the source against.
///
/// **This had no doc comment and read as though it had one (task-1961 A8,
/// task-1969 6.3).** The `///` block eighty lines below - "Builds every
/// operator above the source", which is where a reader's eye lands when
/// scrolling - documents `build_chain`, the next function. So the only
/// function in this file's decomposition without a comment was the one a
/// reader was most likely to think they had just read the comment for.
///
/// What it does that `build_chain` does not: it refuses the parts of the
/// `SELECT` this executor has not built, works out which columns are
/// correlated and how wide they are, and hands `build_chain` the scan types
/// that follow from that. `build_chain` is then the part that holds no borrow
/// of the catalog, which is what lets a `Compiled` outlive the catalog it was
/// built against.
///
/// @param plan - the physical plan to build a chain for
/// @param catalog - the trees, borrowed only to resolve a function to its body
/// @param prepared - the bound statement the plan came from
/// @param space - the open databases the chain reads
/// @param params - the values bound for this execution
/// @param sink - where the topmost operator writes its rows
/// @param listing - whether to render each operator as it is put on
pub(crate) fn build_upper(
    plan: &PhysicalPlan,
    catalog: &dyn TreeCatalog,
    prepared: &Prepared,
    space: &Space<'_>,
    params: &Params,
    sink: Box<dyn Sink>,
    listing: Listing,
) -> DbResult<Upper> {
    let select = &plan.select;
    refuse_unhandled(select)?;
    let (correlations, correlation_columns, widened_types) =
        correlated_columns(plan, catalog, space)?;
    let joined_types = space.types;
    let scan_types: &[StaticType] = if correlations.is_empty() {
        space.types
    } else {
        &widened_types
    };
    let space = &Space {
        stages: space.stages,
        layouts: space.layouts,
        types: scan_types,
        order: space.order,
        catalog: Some(catalog),
        correlations: &correlation_columns,
    };
    let outputs = plan_outputs(plan, space, params)?;
    let scan_order = order_equivalents(space.stages, space.layouts, space.order);
    let (group_exprs, group_collations, grouped_walk) =
        grouping_of(plan, prepared, space, params, &scan_order)?;
    let limit = constant_limit(select, params)?;
    let (filters_above, gate) = if correlations.is_empty() {
        (None, Vec::new())
    } else {
        let (above, below) = place_around_correlation(plan);
        (
            Some(above),
            compile_gate(&below, space, params, joined_types)?,
        )
    };
    let up = Upward {
        sorted_already: already_sorted(plan, prepared, &outputs, &scan_order, grouped_walk),
        skipping: is_skip_scan(prepared),
        projection_types: if plan.aggregation == AggregationMode::None {
            scan_types.to_vec()
        } else {
            aggregate_output_types(select, space, params)?
        },
        group_width: select.group_by.len(),
        offset: constant_offset(select, params)?.unwrap_or(0),
        plan,
        prepared,
        space,
        params,
        scan_types,
        scan_order,
        group_exprs,
        group_collations,
        grouped_walk,
        limit,
        outputs,
        filters_above,
    };

    // Built bottom-up, because each operator owns the one below it. The
    // description is collected in the same order and reversed at the end, so it
    // reads source-first the way a plan should.
    let mut operators = listing;
    let mut chain: Box<dyn Sink> = sink;
    chain = push_sort(chain, &mut operators, &up)?;
    chain = push_distinct(chain, &mut operators, &up);
    chain = push_projection(chain, &mut operators, &up)?;
    chain = push_having(chain, &mut operators, &up)?;
    chain = push_aggregate(chain, &mut operators, &up)?;
    chain = push_filters(chain, &mut operators, &up)?;

    let names = select
        .columns
        .iter()
        .map(|column| column.name.clone())
        .collect();
    Ok(Upper {
        head: chain,
        operators,
        names,
        // `up.sorted_already` is the same answer `push_sort` acted on, read
        // rather than derived a second time - the two used to be computed from
        // different information about the same question, which is what let a
        // bound reach a source that a sorter was about to sit on top of.
        limit: source_limit_of(
            plan,
            prepared,
            limit,
            up.sorted_already,
            !up.outputs.sort_keys.is_empty(),
        )
        .map(|limit| limit.saturating_add(up.offset)),
        correlations,
        gate,
    })
}
/// Builds every operator above the source.
///
/// Separated from [`build_prepared`] because a [`Statement`] builds this once
/// and rebuilds only the source per execution. The split is also what makes
/// the rebinding test possible: the parameter reads this function makes are
/// the ones baked into the chain, and a statement is only re-runnable when
/// there are none.
///
/// Everything that holds no borrow of `catalog` is [`build_upper`]'s to
/// build; this adds the two things that do - the correlation operator and the
/// inner join stages - which is where the chain widens from `'static` to `'t`.
///
/// @param plan - the planner's output
/// @param catalog - where the trees and layouts come from
/// @param prepared - the structural choices [`prepare`] made
/// @param space - the joined column space
/// @param params - the values bound to `?1`, `?2`, ...
/// @param sink - the end of the pipeline
/// @param listing - whether to render each operator as it is put on
fn build_chain<'t>(
    plan: &PhysicalPlan,
    catalog: &'t dyn TreeCatalog,
    prepared: &Prepared,
    space: &Space<'_>,
    params: &Params,
    sink: Box<dyn Sink>,
    listing: Listing,
) -> DbResult<Chain<'t>> {
    let upper = build_upper(plan, catalog, prepared, space, params, sink, listing)?;
    let mut operators = upper.operators;
    // The inner stages, innermost first, so each ends up above the one before
    // it in the chain the source pushes into. The chain widens from `'static`
    // to `'t` here and only here: an index nested loop borrows its inner tree,
    // and it wraps everything built so far rather than being wrapped by it.
    let mut chain: Box<dyn Sink + 't> = upper.head;
    // The correlation operator goes *above* every join and *below* every
    // filter that reads a block: the value it computes reads the whole joined
    // row, and the `WHERE` that tests it runs after the last join has widened
    // that row. The conjuncts that read no block are its gate and are tested
    // inside it, before a block is answered (task-2076); they are listed
    // beneath it because that is where they run.
    if !upper.correlations.is_empty() {
        operators.add(|| "CORRELATED SUBQUERY".to_string());
        for _ in &upper.gate {
            operators.add(|| "FILTER RESIDUAL".to_string());
        }
        chain = Box::new(crate::correlate::Correlated::new(
            upper.correlations,
            upper.gate,
            catalog,
            params,
            chain,
        ));
    }
    for index in (1..prepared.stages.len()).rev() {
        let stage = prepared
            .stages
            .get(index)
            .ok_or_else(|| misuse("a stage vanished while building"))?;
        chain = build_nested(plan, catalog, space, params, stage, index, chain)?;
        operators.add(|| {
            format!(
                "{} tree {}{}",
                stage.kind.describe(),
                stage.root,
                if stage.is_lookup {
                    " (rowid lookup)"
                } else {
                    ""
                }
            )
        });
    }

    Ok(Chain {
        head: chain,
        operators,
        names: upper.names,
        limit: upper.limit,
    })
}
/// A prepared statement: an operator chain built once and run many times.
///
/// **This is the difference between preparing a plan and preparing a
/// statement, and the gate was measuring the first while calling it the
/// second.** A scorecard workload with `prepare_each: false` binds new
/// parameters and runs again; SQLite's arm answers that with
/// `sqlite3_reset`, `sqlite3_bind_*` and `sqlite3_step` over a VDBE program it
/// compiled once. Ours re-translated every projected expression, re-boxed every
/// operator and re-formatted the plan description on each execution, and
/// `inillucent-probeprofile` measured that at 0.52 us against a 0.70 us
/// `point.rowid` - 42% of the workload, and 71% of `point.miss`.
///
/// So a `Statement` holds the chain and rebuilds only the *source*, whose key
/// or bounds are the one part of a plan that the parameters decide. Between
/// executions the chain is [`Sink::reset`]: every accumulator, sorter,
/// hash table and limit counter returns to its pre-input state.
///
/// ## Why a statement can refuse to be re-run
///
/// A parameter that reaches anything *other* than the source - `LIMIT ?1`, a
/// projected `?2`, a residual filter - is folded into the chain when the chain
/// is built, and re-running that chain against new values would answer the old
/// question. [`Statement::rebindable`] says whether that happened, and it is
/// decided by counting the parameter reads the chain's construction made rather
/// than by a second opinion about which constructs may carry one.
pub struct Statement<'t> {
    /// The planner's output, which the source is rebuilt from.
    plan: &'t PhysicalPlan,
    /// Where the trees and layouts come from.
    catalog: &'t dyn TreeCatalog,
    /// The structural choices, owned so the statement is self-contained.
    prepared: Prepared,
    /// The layouts and types, computed once.
    held: HeldSpace,
    /// The operator chain, built once.
    head: Box<dyn Sink + 't>,
    /// The pool the source's pages live in, when the source reads a tree.
    pool: Option<&'t Pool>,
    /// The statement's constant `LIMIT`, which the source may use.
    limit: Option<usize>,
    /// What the statement produces.
    shape: Shape,
    /// Whether anything but the source read a parameter while building.
    rebindable: bool,
    /// The connection's settings when the chain was built.
    ///
    /// The chain folds in the length limit and the `LIKE` case rule without
    /// counting a read, so [`Statement::run`] compares these instead - see
    /// `Params::settings` (task-2081).
    settings: crate::scalar::Context,
    /// The cell every `Expr::Parameter` in the chain reads.
    ///
    /// **The chain holds the cell it was built with, and the caller hands a
    /// different `Params` to every execution**, so the two have to be joined up
    /// before the chain runs. Leaving this out is not a slow statement, it is a
    /// wrong answer: `SELECT category, count(*) FROM t WHERE id >= ?1 GROUP BY
    /// category` answered its *first* execution's question on every later one,
    /// and `a_reused_statement_answers_what_a_rebuilt_pipeline_does` is the test
    /// that said so.
    bindings: Bindings,
}
impl<'t> Statement<'t> {
    /// Reports whether this statement may be run again with new parameters.
    pub fn rebindable(&self) -> bool {
        self.rebindable
    }

    /// Returns what the statement produces.
    pub fn shape(&self) -> &Shape {
        &self.shape
    }

    /// Runs the statement against one parameter set.
    ///
    /// **Folds this execution's uncorrelated subqueries first, every time.**
    /// The chain was folded once at [`build_statement`] time, which is correct
    /// for anything baked into the chain - a folded value read there is
    /// counted against [`Statement::rebindable`]. It is *not* correct for the
    /// **source**: a seek key from `WHERE id = (SELECT max(id) FROM t)` calls
    /// `source_for_run` on every run, which used to see the raw `params` this
    /// method was handed - subquery slots empty, nothing having folded them
    /// since the one-time pass - and answered "a correlated subquery used as a
    /// value" for a block that was never correlated. Folding costs about 40 ns
    /// and no allocation on the ordinary statement, which has none.
    ///
    /// @param params - the values bound to `?1`, `?2`, ...
    pub fn run(&mut self, params: &Params) -> DbResult<()> {
        if !self.rebindable {
            return Err(misuse(
                "this statement folded a parameter into its operator chain and cannot be re-run                  against different values",
            ));
        }
        // **Refused rather than run with the old settings.** A `%` or a scalar
        // call in the chain holds the length limit it was built under, and
        // this statement has no plan cache to rebuild itself from, so running
        // it would enforce a limit the connection no longer has.
        if params.settings() != self.settings {
            return Err(misuse(
                "this statement was built under a different length limit or LIKE setting and cannot be re-run under this one",
            ));
        }
        let folded = crate::subquery::fold(self.plan, self.catalog, params)?;
        let params = folded.as_ref().unwrap_or(params);
        // The chain reads the cell it was built with; this is where that cell
        // learns what this execution bound. See `Statement::bindings`.
        let source = params.bindings();
        if !std::sync::Arc::ptr_eq(&self.bindings, &source) {
            if let (Ok(from), Ok(mut held)) = (source.lock(), self.bindings.lock()) {
                held.copy_from(&from);
            }
        }
        let source = {
            let mut space = self.held.view(&self.prepared.stages);
            space.catalog = Some(self.catalog);
            source_for_run(
                self.plan,
                self.catalog,
                &space,
                params,
                &self.prepared,
                self.limit,
            )?
        };
        self.head.reset()?;
        source.run(self.pool, self.head.as_mut())
    }
}
/// Builds a statement that can be run many times.
///
/// @param plan - the planner's output
/// @param catalog - where the trees and layouts come from
/// @param prepared - the structural choices [`prepare`] made
/// @param params - the values the first execution binds
/// @param sink - the end of the pipeline, which the statement keeps
pub fn build_statement<'t>(
    plan: &'t PhysicalPlan,
    catalog: &'t dyn TreeCatalog,
    prepared: &Prepared,
    params: &Params,
    sink: Box<dyn Sink>,
) -> DbResult<Statement<'t>> {
    // Every uncorrelated subquery is answered once, here, before anything is
    // built over it. See `crate::subquery` for why it is per execution.
    let folded = crate::subquery::fold(plan, catalog, params)?;
    let params = folded.as_ref().unwrap_or(params);
    let prepared = prepared.clone();
    let held = space_of(catalog, &prepared)?;
    // The reads the chain makes are the parameters it bakes in. The source's
    // are made after this window closes and are recomputed on every execution,
    // so they do not count against re-running.
    let before = params.reads();
    let chain = {
        let mut space = held.view(&prepared.stages);
        space.catalog = Some(catalog);
        build_chain(
            plan,
            catalog,
            &prepared,
            &space,
            params,
            sink,
            Listing::kept(),
        )?
    };
    let rebindable = params.reads() == before;
    let settings = params.settings();
    let bindings = params.bindings();
    // Kept here, unlike the pipeline path: a `Statement` builds its chain once
    // and runs it many times, so the listing costs one render per statement
    // rather than one per execution, and `Statement::shape` reports it.
    let mut listing = chain.operators;
    listing.add(|| describe_source(&prepared));
    let mut operators = listing.into_lines();
    operators.reverse();
    let names = chain.names;
    let pool = source_pool(catalog, &prepared);
    Ok(Statement {
        bindings,
        plan,
        catalog,
        prepared,
        held,
        head: chain.head,
        pool,
        limit: chain.limit,
        shape: Shape { names, operators },
        rebindable,
        settings,
    })
}
/// Returns the pool the source stage's tree lives in, when it reads one.
///
/// **The source is a stage, so its pool travels with it like every other
/// stage's.** A pipeline has exactly one source and therefore exactly one
/// source pool; every other stage that touches a tree - the inner side of an
/// index nested loop, a materialised subquery - asks for its own.
///
/// @param catalog - where the trees and their pools come from
/// @param prepared - the structural choices `prepare` made
pub(crate) fn source_pool<'t>(
    catalog: &'t dyn TreeCatalog,
    prepared: &Prepared,
) -> Option<&'t Pool> {
    catalog.pool_for(prepared.stages.first()?.root)
}
/// Returns what drives a pipeline, without the `EXPLAIN` line.
///
/// **The one place that decides what a plan drives**, so the callers - a
/// one-shot run, a reused statement's rebuild, and a statement's construction -
/// cannot disagree about a plan with no stages.
///
/// There used to be a `source_for` beside this that returned the source and
/// [`describe_source`]'s line as a pair, so the two could not drift apart. Its
/// one remaining caller calls this and `describe_source` on the next line
/// instead, which keeps them together and lets a caller that asked for no
/// listing skip building the `String` at all (task-2026).
///
/// @param plan - the planner's output
/// @param catalog - where the trees come from
/// @param space - the joined column space
/// @param params - the bound parameters
/// @param prepared - the structural choices `prepare` made
/// @param limit - the statement's `LIMIT`, when it has a constant one
pub(crate) fn source_for_run<'t>(
    plan: &PhysicalPlan,
    catalog: &'t dyn TreeCatalog,
    space: &Space<'_>,
    params: &Params,
    prepared: &Prepared,
    limit: Option<usize>,
) -> DbResult<Source<'t>> {
    match prepared.stages.first() {
        // A materialised subquery: the inner pipeline runs to completion into a
        // buffer, and the buffer drives the outer one. It is built here rather
        // than in `build_source` because it needs the plan and the catalog
        // rather than a tree.
        Some(stage) if stage.kind == AccessKind::Materialised => {
            let term = plan
                .sources
                .get(stage.term)
                .ok_or_else(|| misuse("a stage names a FROM term the plan does not have"))?;
            if let AccessPath::VirtualScan { .. } = &term.path {
                // **The module is only asked for the columns the query reads.**
                // A materialised virtual scan used to ask the cursor for every
                // declared column of every row, so `SELECT count(*) FROM t
                // WHERE t MATCH 'x'` read the content row and scored the rank
                // column for five hundred rows it then counted. This is the
                // same question a covering index is chosen by, asked of the
                // same bound statement, so a column that is read is a column
                // that is materialised.
                let needed = plan.select.columns_read(term.id);
                return Ok(Source::Virtual(Box::new(VirtualScanSource {
                    catalog,
                    table: term.table.clone(),
                    path: term.path.clone(),
                    params: params.clone(),
                    needed,
                })));
            }
            let rows = materialise_stage(plan, catalog, params, stage, limit)?;
            Ok(Source::Rows(rows))
        }
        Some(stage) => build_source(plan, catalog, space, params, stage, limit),
        // A `VALUES` arm has no FROM term either, and its rows *are* its
        // answer: every expression is a constant, so they are evaluated once
        // here rather than projected out of an empty row.
        None if !plan.select.values.is_empty() => {
            let empty = Space {
                stages: &[],
                layouts: &[],
                types: &[],
                order: &[],
                catalog: None,
                correlations: &[],
            };
            let mut rows: Vec<Vec<OwnedDatum>> = Vec::with_capacity(plan.select.values.len());
            for row in &plan.select.values {
                let mut out = Vec::with_capacity(row.len());
                for expr in row {
                    out.push(constant_value(expr, &empty, params, None)?);
                }
                rows.push(out);
            }
            Ok(Source::Rows(rows))
        }
        // A query with no FROM term: one row of no columns, and the whole
        // answer comes out of the projection.
        None => Ok(Source::Constant(1)),
    }
}
/// Pushes one stage whose rows the caller produces rather than a tree.
///
/// A derived table, a recursive CTE, the queue that CTE is being filled from,
/// and a virtual table's rows are all this shape: the pipeline reads a buffer,
/// not pages. A materialised row is its own record - slot `i` is column `i`,
/// there is no rowid, and nothing is known about the order, so no streaming
/// rule may assume one.
///
/// @param stages - the stages built so far
/// @param source - the binder's number for the FROM term
/// @param term - the term's position in the plan's own arrays
/// @param width - how many columns a row holds
/// @param offset - the first joined-row column this stage fills, advanced here
pub(crate) fn push_materialised(
    stages: &mut Vec<PreparedStage>,
    source: usize,
    term: usize,
    width: usize,
    offset: &mut usize,
) {
    stages.push(PreparedStage {
        functions: Vec::new(),
        root: 0,
        kind: AccessKind::Materialised,
        source,
        term,
        is_lookup: false,
        offset: *offset,
        width,
        layout: Some(std::rc::Rc::new(SourceLayout {
            tree_key: 0,
            slots: (0..width).map(Some).collect(),
            rowid: None,
            // Rows read once into a buffer: a derived table, a recursive CTE.
            // None of them identifies a stored row to probe a table with.
            identity: Vec::new(),
            types: vec![StaticType::Unknown; width],
            width,
            key_columns: Vec::new(),
        })),
    });
    *offset = offset.saturating_add(width);
}
/// Returns the `EXPLAIN` line for whatever drives a plan.
///
/// @param prepared - the structural choices `prepare` made
pub(crate) fn describe_source(prepared: &Prepared) -> String {
    match prepared.stages.first() {
        Some(stage) => format!("{} tree {}", stage.kind.describe(), stage.root),
        None => "SCAN CONSTANT ROW".to_string(),
    }
}
/// Builds the driving source for the outermost stage.
///
/// @param plan - the planner's output
/// @param catalog - where the trees come from
/// @param space - the joined column space
/// @param params - the bound parameters
/// @param stage - the outermost stage
/// @param limit - the statement's `LIMIT`, when it has a constant one
fn build_source<'t>(
    plan: &PhysicalPlan,
    catalog: &'t dyn TreeCatalog,
    space: &Space<'_>,
    params: &Params,
    stage: &PreparedStage,
    limit: Option<usize>,
) -> DbResult<Source<'t>> {
    let tree = catalog
        .tree(stage.root)
        .ok_or_else(|| misuse(format!("no tree imported for root page {}", stage.root)))?;
    let projection = Projection::all(stage.width);
    let source_term = plan
        .sources
        .get(stage.term)
        .ok_or_else(|| misuse("a stage names a FROM term the plan does not have"))?;
    let path = &source_term.path;
    let table = &source_term.table;
    match stage.kind {
        AccessKind::Full => Ok(Source::Scan(FullScan::new(tree, projection))),
        AccessKind::Skip => {
            let prefix = space
                .order
                .len()
                .min(projected_prefix(plan, space, params)?);
            Ok(Source::Skip(SkipScan::new(tree, prefix.max(1))))
        }
        AccessKind::Point => {
            let key = point_key(path, space, params)?;
            Ok(Source::Point(PointProbe::new(tree, projection), key))
        }
        AccessKind::Span => {
            let bounds = span_bounds(path, table, space, params)?;
            if bounds.matches_nothing {
                return Ok(Source::Rows(Vec::new()));
            }
            Ok(Source::Span(SpanScan::new(
                tree,
                projection,
                bounds.low,
                bounds.low_inclusive,
                bounds.high,
                bounds.high_inclusive,
            )))
        }
        AccessKind::Reverse => {
            let bounds = span_bounds(path, table, space, params)?;
            if bounds.matches_nothing {
                return Ok(Source::Rows(Vec::new()));
            }
            Ok(Source::Reverse(ReverseScan::new(
                tree, projection, bounds, limit,
            )))
        }
        AccessKind::Vector => {
            let AccessPath::VectorProbe {
                index,
                probe,
                depth,
                ..
            } = path
            else {
                return Err(misuse("a vector stage over a path that is not one"));
            };
            // The catalog goes in because the probe vector is very often
            // `embed('search_query: ...')` - a registered function, whose body
            // only this can resolve. See `literal_value_in`.
            let wanted = literal_value_in(probe, params, Some(catalog))?;
            let probe_over = PointProbe::new(tree, projection);
            let scan = super::joins::CandidateProbe {
                plan,
                catalog,
                space,
                params,
                stage,
                probe_over: &probe_over,
            };
            let keys = iterative_candidates(&scan, index, &wanted.borrow(), *depth)?;
            Ok(Source::Vector(probe_over, keys))
        }
        AccessKind::SeekUnion => {
            let probe_over = PointProbe::new(tree, projection);
            let keys = match path {
                AccessPath::RowidSeekUnion { keys, .. } => rowid_union_keys(keys, space, params)?,
                AccessPath::IndexSeekUnion {
                    branches, columns, ..
                } => index_union_keys(branches, table, columns, space, params)?,
                _ => return Err(misuse("a seek-union stage over a path that is not one")),
            };
            Ok(Source::SeekUnion(probe_over, keys))
        }
        AccessKind::RangeUnion => {
            let scans = range_union_bounds(tree, projection, path, table, space, params)?;
            Ok(Source::RangeUnion(scans))
        }
        AccessKind::Nested => Err(misuse("a nested stage cannot drive a pipeline")),
        // Unreachable: `source_for` answers a materialised stage before it gets
        // here, because building one needs the plan and the catalog rather than
        // a tree. Stated rather than folded into the arm above, so that a stage
        // kind added later is a compile error.
        AccessKind::Materialised => Err(misuse(
            "a materialised stage is built by `source_for`, not from a tree",
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A view over a space carries the stages it was given and nothing else.
    ///
    /// **The space is built once per prepare and viewed per execution (T3,
    /// task-1962).** The layouts, the static types and the walk's order are the
    /// prepare's answer and do not change between executions; what changes is
    /// which stages are in play and where a correlated block's answer sits.
    /// A view that carried the catalog would make the prepared statement
    /// borrow it for as long as it lived.
    #[test]
    fn a_view_carries_the_stages_and_not_the_catalog() {
        let held = HeldSpace {
            layouts: Vec::new(),
            types: vec![StaticType::Unknown; 2],
            order: vec![0],
        };
        let space = held.view(&[]);
        assert!(space.stages.is_empty());
        assert_eq!(space.types.len(), 2);
        assert_eq!(space.order, &[0]);
        assert!(
            space.catalog.is_none(),
            "a view holds no catalog, so a prepared statement does not borrow one"
        );
        assert!(space.correlations.is_empty());
    }

    /// A view can be told where a correlated block's answer sits.
    #[test]
    fn a_view_can_carry_the_correlations() {
        let held = HeldSpace {
            layouts: Vec::new(),
            types: Vec::new(),
            order: Vec::new(),
        };
        let space = held.view_with(&[], &[(3, 7)]);
        assert_eq!(
            space.correlations,
            &[(3, 7)],
            "block 3's answer is at column 7, which is the only thing this \
             view has that the plain one does not"
        );
    }
}
