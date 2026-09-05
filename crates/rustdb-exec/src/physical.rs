//! The physical pass: the existing planner's output becomes a pipeline.
//!
//! Invariant: this pass never changes what a query means, only how it is run.
//! Every construct it does not recognise is **refused** rather than
//! approximated - `unsupported()` returns an error naming what was not handled,
//! so a query the new engine cannot run yet fails loudly instead of returning a
//! plausible wrong answer. That is the whole reason it is written as a
//! whitelist: a differential digest comparison catches a wrong answer, but only
//! for a query somebody thought to put in the corpus.
//!
//! ## Where this sits
//!
//! `rustdb-sql`'s lexer, parser, binder and planner survive the rearchitecture
//! unchanged - the TDD's component triage says so, and they are the part of the
//! old engine that was never the problem. What they produce is a
//! [`PhysicalPlan`]: FROM terms with access paths, residual predicates, an
//! aggregation mode, and a bound result list. This module turns that into the
//! operator chain in [`crate::ops`].
//!
//! What Phase 1 handles is the four `read.analytical` shapes and their close
//! neighbours: one FROM term, a table or covering-index scan, an optional
//! residual predicate, whole-input or grouped aggregation, `DISTINCT`,
//! `ORDER BY` and `LIMIT`. Joins, subqueries, compounds, window functions and
//! virtual tables are Phase 2 and are refused here by name.
//!
//! ## How a bound column finds its vector
//!
//! A `BoundExpr::Column` names a *record slot* in the SQLite sense, because the
//! binder and the catalog were built for that layout. The new engine's trees do
//! not have record slots; they have mini-columns. [`SourceLayout`] is the map
//! between them, and it is built by whatever imported the table, because that
//! is the only thing that knows which slot became which column. Keying it that
//! way - rather than by name - is what lets the whole front end stay unchanged.

use rustdb_base::error::misuse;
use rustdb_base::DbResult;
use rustdb_sql::bind::{BoundExpr, BoundSelect};
use rustdb_sql::function::AggregateFunc;
use rustdb_sql::plan::{AccessPath, AggregationMode, PhysicalPlan};
use rustdb_sql::ast::{BinaryOp, SortOrder};
use rustdb_tree::datum::OwnedDatum;
use rustdb_tree::Tree;

use crate::aggregate::AggregateKind;
use crate::expr::{compile, ArithOp, CompareOp, Expr, StaticType};
use crate::ops::{
    AdjacentDistinct, AggregateSpec, CollectInto, Distinct, Filter, HashAggregate, Limit, Project,
    SimpleAggregate, Sink, Sort, SortKey, StreamAggregate, TopN,
};
use crate::scan::{Projection, SkipScan, TableScan};

/// How one imported table's record slots map onto a tree's columns.
#[derive(Clone, Debug)]
pub struct SourceLayout {
    /// The tree holding the rows or entries.
    pub tree_key: u32,
    /// For each record slot, which tree column holds it.
    ///
    /// `None` means the tree does not carry that slot, which is how a covering
    /// index says it does not hold a column.
    pub slots: Vec<Option<usize>>,
    /// Which tree column holds the row's rowid.
    pub rowid: Option<usize>,
    /// The static type of each tree column, for the expression compiler.
    pub types: Vec<StaticType>,
    /// How many columns the tree has.
    pub width: usize,
    /// The tree columns the leaves are ordered by, in order.
    ///
    /// A scan of the tree therefore produces rows sorted by these, which is
    /// what lets `GROUP BY`, `DISTINCT` and `ORDER BY` over a prefix of them
    /// run as a streaming pass instead of building a hash table or a sorter.
    /// Getting this wrong would be a wrong answer rather than a slow one, so it
    /// is set by the import - which built the tree - and never inferred.
    pub key_columns: Vec<usize>,
}

/// Where the executor finds its trees and its layouts.
pub trait TreeCatalog {
    /// Returns the tree a plan's root page id refers to.
    ///
    /// The key is the SQLite root page from the fixture the data was imported
    /// from. That sounds like a leftover and is deliberate: the plan comes from
    /// a binder reading that fixture's schema, so the root page is the one
    /// identifier both sides already agree on, and using it means the import
    /// decides the mapping rather than a name lookup guessing at it.
    ///
    /// @param root - the root page id the plan named
    fn tree(&self, root: u32) -> Option<&Tree>;

    /// Returns the layout for a plan's root page id.
    ///
    /// @param root - the root page id the plan named
    fn layout(&self, root: u32) -> Option<&SourceLayout>;

    /// Returns the index trees that might cover a query over one table.
    ///
    /// Smallest tree first, so the physical pass takes the cheapest structure
    /// that carries every column the query reads. This is the TDD's "covering
    /// when the projection is inside the index key" rule, and it is what makes
    /// the comparison against SQLite like for like: SQLite answers
    /// `count(*), sum(key), max(category) FROM main_table` from
    /// `main_category(category, key)` rather than from the table, and an engine
    /// measured on a 14 MB table scan against a 1.4 MB index scan is being
    /// measured on a different amount of work.
    ///
    /// @param table_root - the table's root page id
    fn covering_candidates(&self, table_root: u32) -> Vec<u32> {
        let _ = table_root;
        Vec::new()
    }
}

/// What drives a pipeline.
pub enum Source<'t> {
    /// Every row of a tree, in key order.
    Scan(TableScan<'t>),
    /// One row per distinct value of a key prefix, by seeking.
    Skip(SkipScan<'t>),
}

impl Source<'_> {
    /// Drives the source until the pipeline is done.
    ///
    /// @param downstream - the head of the operator chain
    pub fn run(&self, downstream: &mut dyn Sink) -> DbResult<()> {
        match self {
            Source::Scan(scan) => scan.run(downstream),
            Source::Skip(skip) => skip.run(downstream),
        }
    }

    /// Names the source, for a plan description.
    pub fn describe(&self) -> &'static str {
        match self {
            Source::Scan(_) => "SCAN",
            Source::Skip(_) => "SKIP SCAN",
        }
    }
}

/// A built pipeline, ready to run.
pub struct Pipeline<'t> {
    /// The source.
    pub scan: Source<'t>,
    /// The head of the operator chain.
    pub head: Box<dyn Sink>,
}

/// What a built plan produces, so a caller can name its columns.
#[derive(Clone, Debug)]
pub struct Shape {
    /// The name of each output column, as the binder assigned it.
    pub names: Vec<Vec<u8>>,
}

/// Builds a pipeline for a planned statement.
///
/// The sink is handed in so a caller can choose what happens to the rows; the
/// scorecard hands in a [`Collect`] and digests what it kept.
///
/// @param plan - the planner's output
/// @param catalog - where the trees and layouts come from
/// @param sink - the end of the pipeline
pub fn build<'t>(
    plan: &PhysicalPlan,
    catalog: &'t dyn TreeCatalog,
    sink: Box<dyn Sink>,
) -> DbResult<(Pipeline<'t>, Shape)> {
    let prepared = prepare(plan, catalog)?;
    build_prepared(plan, catalog, &prepared, sink)
}

/// What a statement's physical choices are, decided once.
///
/// The structural decisions - which tree to scan, and therefore whether a sort,
/// a hash table or a set is needed at all - depend on the statement and the
/// schema and not on the data, so they belong to prepare rather than to
/// execution. Keeping them here is not only tidiness: the covering rule tries
/// candidate trees by *building* a pipeline over each, and doing that on every
/// execution made a 64-row query spend more time choosing than answering.
#[derive(Clone, Copy, Debug)]
pub struct Prepared {
    /// The tree the scan reads.
    pub root: u32,
}

/// Chooses a statement's physical plan.
///
/// @param plan - the planner's output
/// @param catalog - where the trees and layouts come from
pub fn prepare(plan: &PhysicalPlan, catalog: &dyn TreeCatalog) -> DbResult<Prepared> {
    let (chosen, table_root) = scan_root(plan)?;
    // The covering rule. A plain scan of a table is replaced by a scan of the
    // smallest index tree that carries every column the query reads, because
    // reading 2.66 MiB instead of 14.6 MiB is the single largest lever
    // available on an analytical query and it is the structure SQLite itself
    // chooses. The test for "carries every column" is not a heuristic: the
    // whole pipeline is built against the candidate's layout, and translation
    // fails by name on a column the tree does not hold. A candidate that
    // builds, covers.
    if chosen == table_root {
        for candidate in catalog.covering_candidates(table_root) {
            let trial = Prepared { root: candidate };
            if build_prepared(plan, catalog, &trial, dummy_sink()).is_ok() {
                return Ok(trial);
            }
        }
    }
    Ok(Prepared { root: chosen })
}

/// Returns a sink that discards everything, for the covering-rule trial build.
///
/// The trial exists because "does this index cover the query" is answered by
/// building the pipeline rather than by a separate predicate that could drift
/// away from what the builder actually accepts. The trial's sink is never
/// pushed into.
fn dummy_sink() -> Box<dyn Sink> {
    Box::new(CollectInto::new(std::rc::Rc::new(std::cell::RefCell::new(
        Vec::new(),
    ))))
}

/// Builds a pipeline over an already-chosen tree.
///
/// @param plan - the planner's output
/// @param catalog - where the trees and layouts come from
/// @param prepared - the structural choices [`prepare`] made
/// @param sink - the end of the pipeline
pub fn build_prepared<'t>(
    plan: &PhysicalPlan,
    catalog: &'t dyn TreeCatalog,
    prepared: &Prepared,
    sink: Box<dyn Sink>,
) -> DbResult<(Pipeline<'t>, Shape)> {
    let root = prepared.root;
    let select = &plan.select;
    let tree = catalog
        .tree(root)
        .ok_or_else(|| misuse(format!("no tree imported for root page {root}")))?;
    let layout = catalog
        .layout(root)
        .ok_or_else(|| misuse(format!("no layout imported for root page {root}")))?;

    // The scan produces every column of the chosen tree, in tree order, so an
    // expression's column index is its tree column index and nothing has to be
    // renumbered. Projection pushdown - producing only the columns the query
    // reads - is a Phase 2 item: it saves building vectors that are never read,
    // which costs one pointer each, and it would complicate the index mapping
    // before there is a measurement asking for it.
    let scan_types = layout.types.clone();

    // Result columns and ORDER BY terms, in the space that exists after any
    // aggregation. Terms that are not already result columns are carried
    // through the sort as extra columns and trimmed afterwards.
    let group_width = select.group_by.len();
    let mut projected: Vec<Expr> = Vec::with_capacity(select.columns.len());
    for column in &select.columns {
        projected.push(translate_post(&column.expr, select, layout, group_width)?);
    }
    let result_width = projected.len();
    let mut sort_keys: Vec<SortKey> = Vec::new();
    for term in &select.order_by {
        let translated = translate_post(&term.expr, select, layout, group_width)?;
        let existing = projected
            .iter()
            .position(|held| same_expr(held, &translated));
        let column = match existing {
            Some(index) => index,
            None => {
                if select.distinct {
                    return unsupported("ORDER BY over an expression not in a DISTINCT select list");
                }
                projected.push(translated);
                projected.len().saturating_sub(1)
            }
        };
        sort_keys.push(SortKey {
            column,
            descending: term.order == SortOrder::Descending,
        });
    }
    let needs_trim = projected.len() > result_width;

    // Which scan columns the rows already arrive sorted by. Everything that
    // follows uses this to decide whether a sorter, a hash table or a set is
    // needed at all.
    let scan_order = &layout.key_columns;

    // The group keys, translated into scan-column space.
    let group_exprs = select
        .group_by
        .iter()
        .map(|expr| translate_scan(expr, layout))
        .collect::<DbResult<Vec<Expr>>>()?;
    let grouped_walk = plan.aggregation == AggregationMode::Grouped
        && is_scan_prefix(&group_exprs, scan_order);

    // Whether the projected rows arrive in the order the ORDER BY asks for.
    let sorted_already = !sort_keys.is_empty()
        && sort_keys.iter().all(|term| !term.descending)
        && output_is_sorted_by(&sort_keys, &projected, plan, scan_order, grouped_walk);
    // A skip scan produces distinct keys in key order, so an ascending ORDER BY
    // over those columns is already satisfied by the source.
    let sorted_already = sorted_already
        || (skip_scan_applies(plan, &projected, scan_order, &sort_keys) && !sort_keys.is_empty());

    // Built bottom-up, because each operator owns the one below it.
    let mut chain: Box<dyn Sink> = sink;

    let limit = constant_limit(select)?;
    let offset = constant_offset(select)?.unwrap_or(0);
    if sort_keys.is_empty() || sorted_already {
        if let Some(limit) = limit {
            chain = Box::new(Limit::new(limit, offset, chain));
        }
        if needs_trim {
            chain = Box::new(Project::new(trim(result_width, &scan_types)?, chain));
        }
    } else if let Some(limit) = limit {
        // ORDER BY with a LIMIT is a bounded heap rather than a full sort.
        if needs_trim {
            chain = Box::new(Project::new(trim(result_width, &scan_types)?, chain));
        }
        let bounded = limit.saturating_add(offset);
        if bounded <= TopN::MAX_LIMIT {
            if offset > 0 {
                chain = Box::new(Limit::new(limit, offset, chain));
            }
            chain = Box::new(TopN::new(sort_keys.clone(), bounded, chain));
        } else {
            chain = Box::new(Limit::new(limit, offset, chain));
            chain = Box::new(Sort::new(sort_keys.clone(), chain));
        }
    } else {
        if needs_trim {
            chain = Box::new(Project::new(trim(result_width, &scan_types)?, chain));
        }
        chain = Box::new(Sort::new(sort_keys.clone(), chain));
    }

    let skipping = skip_scan_applies(plan, &projected, scan_order, &sort_keys);
    if select.distinct && !skipping {
        // The same adjacency argument as grouping: if the projected columns are
        // a prefix of the scan order, duplicates arrive together and can be
        // dropped by comparing each row with the one before it.
        if plan.aggregation == AggregationMode::None && is_scan_prefix(&projected, scan_order) {
            chain = Box::new(AdjacentDistinct::new(chain));
        } else {
            chain = Box::new(Distinct::new(chain));
        }
    }

    let projection_input_types = if plan.aggregation == AggregationMode::None {
        scan_types.clone()
    } else {
        aggregate_output_types(select, layout)?
    };
    // A skip scan hands up exactly the projected key columns, already in
    // output order, so the projection over it reads column i for column i.
    let projected = if skipping {
        (0..projected.len()).map(Expr::Column).collect()
    } else {
        projected
    };
    let compiled_projection = projected
        .iter()
        .map(|expr| compile(expr, &projection_input_types))
        .collect::<DbResult<Vec<_>>>()?;
    chain = Box::new(Project::new(compiled_projection, chain));

    match plan.aggregation {
        AggregationMode::None => {}
        AggregationMode::Whole => {
            chain = Box::new(SimpleAggregate::new(
                aggregate_specs(select, layout, &scan_types)?,
                chain,
            ));
        }
        AggregationMode::Grouped => {
            let keys = group_exprs
                .iter()
                .map(|expr| compile(expr, &scan_types))
                .collect::<DbResult<Vec<_>>>()?;
            let specs = aggregate_specs(select, layout, &scan_types)?;
            chain = if grouped_walk {
                Box::new(StreamAggregate::new(keys, specs, chain))
            } else {
                Box::new(HashAggregate::new(keys, specs, chain))
            };
        }
    }

    if let Some(filter) = &select.filter {
        let translated = translate_scan(filter, layout)?;
        chain = Box::new(Filter::new(compile(&translated, &scan_types)?, chain));
    }
    if plan.constant_filter.is_some() {
        return unsupported("a constant WHERE term");
    }
    for residual in plan.residuals.iter().flatten() {
        let translated = translate_scan(residual, layout)?;
        chain = Box::new(Filter::new(compile(&translated, &scan_types)?, chain));
    }

    let names = select
        .columns
        .iter()
        .map(|column| column.name.clone())
        .collect();

    // The skip-scan rule. `SELECT DISTINCT <prefix of the key>` with no
    // predicate and no aggregation does not need to read every row: it needs
    // one row per distinct value, and the tree can seek from one to the next.
    // The condition is deliberately narrow, because a seek per distinct value
    // is a loss when almost every value is distinct - it is a win at 64 values
    // in 100,000 rows and a loss at 100,000 in 100,000. Phase 2's statistics
    // give the planner the distinct count to decide on; until then the rule
    // applies only where SQLite applies it, which is the shape that made the
    // comparison unequal.
    let source = if skip_scan_applies(plan, &projected, scan_order, &sort_keys) {
        Source::Skip(SkipScan::new(tree, projected.len()))
    } else {
        Source::Scan(TableScan::new(tree, Projection::all(layout.width)))
    };
    Ok((
        Pipeline {
            scan: source,
            head: chain,
        },
        Shape { names },
    ))
}

/// Reports whether the skip-scan rule applies to a query.
///
/// Every condition is load bearing:
///
/// - `DISTINCT` with no aggregation, because a skip scan produces one row per
///   distinct prefix and nothing else;
/// - no `WHERE`, because a skipped row might have been the one that passed it;
/// - the projected columns are exactly a prefix of the scan order, because that
///   is what makes "one row per distinct value" the same set as the query's;
/// - every ordering term ascending and already satisfied, so the rows the seek
///   produces are the answer in the order asked for.
///
/// @param plan - the planner's output
/// @param projected - the output expressions
/// @param scan_order - the tree columns the leaves are ordered by
/// @param sort_keys - the ordering terms
fn skip_scan_applies(
    plan: &PhysicalPlan,
    projected: &[Expr],
    scan_order: &[usize],
    sort_keys: &[SortKey],
) -> bool {
    plan.select.distinct
        && plan.aggregation == AggregationMode::None
        && plan.select.filter.is_none()
        && plan.constant_filter.is_none()
        && plan.residuals.iter().all(Option::is_none)
        && plan.select.limit.is_none()
        && is_scan_prefix(projected, scan_order)
        && sort_keys.iter().all(|term| !term.descending)
}

/// Reports whether a list of expressions is a prefix of the scan's key order.
///
/// Every expression must be a bare column reference, and the columns must be
/// exactly the scan order's leading columns in the same order - not a subset
/// and not a permutation, because "the rows arrive sorted by these" is only
/// true of a prefix.
///
/// @param exprs - the expressions to test
/// @param scan_order - the tree columns the leaves are ordered by
fn is_scan_prefix(exprs: &[Expr], scan_order: &[usize]) -> bool {
    if exprs.is_empty() || exprs.len() > scan_order.len() {
        return false;
    }
    exprs.iter().enumerate().all(|(position, expr)| {
        matches!(expr, Expr::Column(index) if scan_order.get(position) == Some(index))
    })
}

/// Reports whether the projected rows already arrive in the ORDER BY's order.
///
/// @param sort_keys - the ordering terms, in output-column space
/// @param projected - the output expressions
/// @param plan - the planner's output
/// @param scan_order - the tree columns the leaves are ordered by
/// @param grouped_walk - whether a streaming grouped aggregate is in the chain
fn output_is_sorted_by(
    sort_keys: &[SortKey],
    projected: &[Expr],
    plan: &PhysicalPlan,
    scan_order: &[usize],
    grouped_walk: bool,
) -> bool {
    match plan.aggregation {
        // A streaming grouped aggregate emits one row per group as the key
        // changes, so its output is in group-key order, and the group keys are
        // output columns 0..group_width.
        AggregationMode::Grouped => {
            grouped_walk
                && sort_keys.iter().enumerate().all(|(position, term)| {
                    matches!(projected.get(term.column), Some(Expr::Column(index)) if *index == position)
                })
        }
        AggregationMode::Whole => false,
        AggregationMode::None => {
            let ordered: Vec<Expr> = sort_keys
                .iter()
                .filter_map(|term| projected.get(term.column).cloned())
                .collect();
            ordered.len() == sort_keys.len() && is_scan_prefix(&ordered, scan_order)
        }
    }
}

/// Builds a plan and runs it, returning the rows.
///
/// @param plan - the planner's output
/// @param catalog - where the trees and layouts come from
pub fn run(
    plan: &PhysicalPlan,
    catalog: &dyn TreeCatalog,
) -> DbResult<(Vec<Vec<OwnedDatum>>, Shape)> {
    let prepared = prepare(plan, catalog)?;
    run_prepared(plan, catalog, &prepared)
}

/// Runs an already-prepared statement and returns the rows.
///
/// @param plan - the planner's output
/// @param catalog - where the trees and layouts come from
/// @param prepared - the structural choices [`prepare`] made
pub fn run_prepared(
    plan: &PhysicalPlan,
    catalog: &dyn TreeCatalog,
    prepared: &Prepared,
) -> DbResult<(Vec<Vec<OwnedDatum>>, Shape)> {
    let rows = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
    let sink = Box::new(CollectInto::new(std::rc::Rc::clone(&rows)));
    let (mut pipeline, shape) = build_prepared(plan, catalog, prepared, sink)?;
    pipeline.scan.run(pipeline.head.as_mut())?;
    let collected = rows.borrow().clone();
    Ok((collected, shape))
}

/// Returns the root page the plan's access path reads.
///
/// A covering index scan reads the index's root; everything else reads the
/// table's. That choice is the planner's, and honouring it is what makes the
/// comparison against SQLite like-for-like: SQLite answers
/// `count(*), sum(key), max(category)` from `main_category`, and so must this.
///
/// @param path - the access path the planner chose
fn scan_root(plan: &PhysicalPlan) -> DbResult<(u32, u32)> {
    if !plan.compounds.is_empty() {
        return unsupported("a compound query");
    }
    if plan.sources.len() != 1 {
        return unsupported("a join or a query with no FROM term");
    }
    let source = plan
        .sources
        .first()
        .ok_or_else(|| misuse("a plan with no source"))?;
    let select = &plan.select;
    if !select.windows.is_empty() {
        return unsupported("a window function");
    }
    if !select.values.is_empty() {
        return unsupported("a VALUES arm");
    }
    if select.having.is_some() {
        return unsupported("HAVING");
    }
    if select.aggregates.iter().any(|call| call.distinct) {
        return unsupported("an aggregate with DISTINCT");
    }
    match &source.path {
        AccessPath::TableScan { root } => Ok((*root, *root)),
        AccessPath::RowidRange { root, low, high } => {
            if low.is_some() || high.is_some() {
                return unsupported("a rowid range");
            }
            Ok((*root, *root))
        }
        AccessPath::IndexSeek {
            table_root,
            index_root,
            equalities,
            low,
            high,
            covering,
            ..
        } => {
            if !equalities.is_empty() || low.is_some() || high.is_some() {
                return unsupported("an index seek with bounds");
            }
            match covering {
                Some(_) => Ok((*index_root, *table_root)),
                None => Ok((*table_root, *table_root)),
            }
        }
        AccessPath::RowidSeek { .. } => unsupported("a rowid seek"),
        AccessPath::Subquery { .. } => unsupported("a subquery source"),
        AccessPath::Recursive { .. } | AccessPath::RecursiveSelf { .. } => {
            unsupported("a recursive CTE")
        }
        AccessPath::VirtualScan { .. } => unsupported("a virtual table"),
    }
}

/// Translates a bound expression that reads the scan's columns.
///
/// @param expr - the bound expression
/// @param layout - how record slots map onto tree columns
fn translate_scan(expr: &BoundExpr, layout: &SourceLayout) -> DbResult<Expr> {
    Ok(match expr {
        BoundExpr::Null => Expr::Literal(OwnedDatum::Null),
        BoundExpr::Integer(number) => Expr::Literal(OwnedDatum::Int(*number)),
        BoundExpr::Real(number) => Expr::Literal(OwnedDatum::Real(*number)),
        BoundExpr::Text(bytes) => Expr::Literal(OwnedDatum::Text(bytes.clone())),
        BoundExpr::Blob(bytes) => Expr::Literal(OwnedDatum::Blob(bytes.clone())),
        BoundExpr::Column { slot, .. } => {
            let index = layout
                .slots
                .get(*slot as usize)
                .copied()
                .flatten()
                .ok_or_else(|| {
                    misuse(format!("the scanned tree does not carry record slot {slot}"))
                })?;
            Expr::Column(index)
        }
        BoundExpr::Rowid { .. } => Expr::Column(
            layout
                .rowid
                .ok_or_else(|| misuse("the scanned tree does not carry a rowid"))?,
        ),
        BoundExpr::Not(operand) => Expr::Not(Box::new(translate_scan(operand, layout)?)),
        BoundExpr::IsNull { operand, negated } => {
            let inner = Box::new(translate_scan(operand, layout)?);
            if *negated {
                Expr::IsNotNull(inner)
            } else {
                Expr::IsNull(inner)
            }
        }
        BoundExpr::And(left, right) => Expr::And(
            Box::new(translate_scan(left, layout)?),
            Box::new(translate_scan(right, layout)?),
        ),
        BoundExpr::Or(left, right) => Expr::Or(
            Box::new(translate_scan(left, layout)?),
            Box::new(translate_scan(right, layout)?),
        ),
        BoundExpr::Arithmetic { op, left, right } => Expr::Arith(
            arith_op(*op)?,
            Box::new(translate_scan(left, layout)?),
            Box::new(translate_scan(right, layout)?),
        ),
        BoundExpr::Compare {
            op,
            left,
            right,
            affinity,
            collation,
        } => {
            // Affinity conversion before comparison and a non-BINARY collation
            // both change the answer, so a Phase 1 executor that ignored them
            // would be quietly wrong rather than incomplete. They are refused.
            if affinity.is_some() {
                return unsupported("a comparison that applies an affinity");
            }
            if *collation != rustdb_value::collation::Collation::Binary {
                return unsupported("a comparison under a non-BINARY collation");
            }
            Expr::Compare(
                compare_op(*op)?,
                Box::new(translate_scan(left, layout)?),
                Box::new(translate_scan(right, layout)?),
            )
        }
        other => return unsupported(&format!("the expression {}", name_of(other))),
    })
}

/// Translates a bound expression in the space after aggregation.
///
/// A result column of an aggregating query reads either a `GROUP BY` key or an
/// accumulator, and both are columns of the row the aggregate operator emits:
/// the keys first, then the accumulators.
///
/// @param expr - the bound expression
/// @param select - the bound statement, for the group-by list
/// @param layout - how record slots map onto tree columns
/// @param group_width - how many `GROUP BY` keys there are
fn translate_post(
    expr: &BoundExpr,
    select: &BoundSelect,
    layout: &SourceLayout,
    group_width: usize,
) -> DbResult<Expr> {
    if select.aggregates.is_empty() && select.group_by.is_empty() {
        return translate_scan(expr, layout);
    }
    if let BoundExpr::Aggregate { slot } = expr {
        return Ok(Expr::Column(group_width.saturating_add(*slot)));
    }
    if let Some(index) = select
        .group_by
        .iter()
        .position(|key| key == expr)
    {
        return Ok(Expr::Column(index));
    }
    match expr {
        BoundExpr::Null | BoundExpr::Integer(_) | BoundExpr::Real(_) | BoundExpr::Text(_)
        | BoundExpr::Blob(_) => translate_scan(expr, layout),
        BoundExpr::Arithmetic { op, left, right } => Ok(Expr::Arith(
            arith_op(*op)?,
            Box::new(translate_post(left, select, layout, group_width)?),
            Box::new(translate_post(right, select, layout, group_width)?),
        )),
        BoundExpr::Compare {
            op,
            left,
            right,
            affinity,
            collation,
        } => {
            if affinity.is_some() {
                return unsupported("a comparison that applies an affinity");
            }
            if *collation != rustdb_value::collation::Collation::Binary {
                return unsupported("a comparison under a non-BINARY collation");
            }
            Ok(Expr::Compare(
                compare_op(*op)?,
                Box::new(translate_post(left, select, layout, group_width)?),
                Box::new(translate_post(right, select, layout, group_width)?),
            ))
        }
        // A bare column in an aggregating query that is not a GROUP BY key is
        // SQLite's "bare column" extension: it takes the value from an
        // arbitrary row of the group. Refusing it is the honest answer until
        // there is a defined row to take it from.
        other => unsupported(&format!(
            "{} outside an aggregate in a grouped query",
            name_of(other)
        )),
    }
}

/// Builds the aggregate specifications a statement's accumulators need.
///
/// @param select - the bound statement
/// @param layout - how record slots map onto tree columns
/// @param types - the static type of each scan column
fn aggregate_specs(
    select: &BoundSelect,
    layout: &SourceLayout,
    types: &[StaticType],
) -> DbResult<Vec<AggregateSpec>> {
    let mut specs = Vec::with_capacity(select.aggregates.len());
    for call in &select.aggregates {
        let kind = match call.func {
            AggregateFunc::Count if call.star => AggregateKind::CountStar,
            AggregateFunc::Count => AggregateKind::Count,
            AggregateFunc::Sum => AggregateKind::Sum,
            AggregateFunc::Total => AggregateKind::Total,
            AggregateFunc::Avg => AggregateKind::Average,
            AggregateFunc::Min => AggregateKind::Minimum,
            AggregateFunc::Max => AggregateKind::Maximum,
            AggregateFunc::GroupConcat => {
                let separator = match call.arguments.get(1) {
                    None => ",".to_string(),
                    Some(BoundExpr::Text(bytes)) => String::from_utf8_lossy(bytes).into_owned(),
                    Some(_) => return unsupported("group_concat with a computed separator"),
                };
                AggregateKind::GroupConcat(separator)
            }
            other => return unsupported(&format!("the aggregate {other:?}")),
        };
        let argument = match (kind == AggregateKind::CountStar, call.arguments.first()) {
            (true, _) | (_, None) => None,
            (false, Some(expr)) => {
                let translated = translate_scan(expr, layout)?;
                Some(compile(&translated, types)?)
            }
        };
        specs.push(AggregateSpec { kind, argument });
    }
    Ok(specs)
}

/// Returns the static types of the row an aggregate operator emits.
///
/// @param select - the bound statement
/// @param layout - how record slots map onto tree columns
fn aggregate_output_types(
    select: &BoundSelect,
    layout: &SourceLayout,
) -> DbResult<Vec<StaticType>> {
    let mut types = Vec::with_capacity(select.group_by.len().saturating_add(select.aggregates.len()));
    for key in &select.group_by {
        types.push(match translate_scan(key, layout)? {
            Expr::Column(index) => layout
                .types
                .get(index)
                .copied()
                .unwrap_or(StaticType::Unknown),
            _ => StaticType::Unknown,
        });
    }
    for call in &select.aggregates {
        types.push(match call.func {
            // `count` is always an integer; the rest depend on their input and
            // on whether a sum overflowed, so nothing is claimed about them.
            AggregateFunc::Count => StaticType::Int,
            AggregateFunc::Total | AggregateFunc::Avg => StaticType::Real,
            _ => StaticType::Unknown,
        });
    }
    Ok(types)
}

/// Returns a projection that keeps the first `width` columns.
///
/// @param width - how many columns to keep
/// @param types - the static type of each input column
fn trim(width: usize, types: &[StaticType]) -> DbResult<Vec<Box<dyn crate::expr::Eval>>> {
    (0..width)
        .map(|index| compile(&Expr::Column(index), types))
        .collect()
}

/// Returns a statement's `LIMIT`, when it is a constant.
///
/// @param select - the bound statement
fn constant_limit(select: &BoundSelect) -> DbResult<Option<usize>> {
    match &select.limit {
        None => Ok(None),
        Some(BoundExpr::Integer(number)) if *number >= 0 => Ok(Some(*number as usize)),
        Some(BoundExpr::Integer(_)) => Ok(Some(0)),
        Some(_) => unsupported("a computed LIMIT"),
    }
}

/// Returns a statement's `OFFSET`, when it is a constant.
///
/// @param select - the bound statement
fn constant_offset(select: &BoundSelect) -> DbResult<Option<usize>> {
    match &select.offset {
        None => Ok(None),
        Some(BoundExpr::Integer(number)) if *number >= 0 => Ok(Some(*number as usize)),
        Some(BoundExpr::Integer(_)) => Ok(Some(0)),
        Some(_) => unsupported("a computed OFFSET"),
    }
}

/// Maps a bound arithmetic operator onto a compiled one.
///
/// @param op - the operator the binder recorded
fn arith_op(op: BinaryOp) -> DbResult<ArithOp> {
    match op {
        BinaryOp::Add => Ok(ArithOp::Add),
        BinaryOp::Subtract => Ok(ArithOp::Subtract),
        BinaryOp::Multiply => Ok(ArithOp::Multiply),
        other => unsupported(&format!("the operator {other:?}")),
    }
}

/// Maps a bound comparison operator onto a compiled one.
///
/// @param op - the operator the binder recorded
fn compare_op(op: BinaryOp) -> DbResult<CompareOp> {
    match op {
        BinaryOp::Equal => Ok(CompareOp::Equal),
        BinaryOp::NotEqual => Ok(CompareOp::NotEqual),
        BinaryOp::Less => Ok(CompareOp::Less),
        BinaryOp::LessEqual => Ok(CompareOp::LessOrEqual),
        BinaryOp::Greater => Ok(CompareOp::Greater),
        BinaryOp::GreaterEqual => Ok(CompareOp::GreaterOrEqual),
        other => unsupported(&format!("the operator {other:?}")),
    }
}

/// Reports whether two translated expressions are the same expression.
///
/// Used to notice that an `ORDER BY` term is already a result column, so the
/// sort reads the projected value rather than recomputing it.
///
/// @param left - one expression
/// @param right - the other
fn same_expr(left: &Expr, right: &Expr) -> bool {
    match (left, right) {
        (Expr::Column(a), Expr::Column(b)) => a == b,
        (Expr::Literal(a), Expr::Literal(b)) => {
            a.borrow().compare(&b.borrow()) == std::cmp::Ordering::Equal
        }
        _ => false,
    }
}

/// Names a bound expression variant, for an error message.
///
/// @param expr - the expression to name
fn name_of(expr: &BoundExpr) -> &'static str {
    match expr {
        BoundExpr::Null => "NULL",
        BoundExpr::Integer(_) => "an integer literal",
        BoundExpr::Real(_) => "a real literal",
        BoundExpr::Text(_) => "a text literal",
        BoundExpr::Blob(_) => "a blob literal",
        BoundExpr::Parameter(_) => "a bound parameter",
        BoundExpr::Column { .. } => "a column reference",
        BoundExpr::Rowid { .. } => "a rowid reference",
        BoundExpr::Aggregate { .. } => "an aggregate",
        BoundExpr::Function { .. } => "a function call",
        BoundExpr::External { .. } => "an application-defined function",
        BoundExpr::Case { .. } => "CASE",
        BoundExpr::Cast { .. } => "CAST",
        BoundExpr::Between { .. } => "BETWEEN",
        BoundExpr::InList { .. } => "IN",
        BoundExpr::Pattern { .. } => "LIKE or GLOB",
        BoundExpr::Subquery { .. } => "a subquery",
        BoundExpr::Is { .. } => "IS",
        BoundExpr::Collate { .. } => "COLLATE",
        BoundExpr::Unary { .. } => "a unary operator",
        BoundExpr::Time { .. } => "a date or time function",
        BoundExpr::Math { .. } => "a math function",
        BoundExpr::Json { .. } => "a JSON function",
        BoundExpr::WindowRef { .. } => "a window function",
        BoundExpr::SorterColumn { .. } => "a sorter column",
        _ => "an expression",
    }
}

/// Refuses a construct by name rather than approximating it.
///
/// @param what - what was not handled
fn unsupported<T>(what: &str) -> DbResult<T> {
    Err(misuse(format!(
        "the Phase 1 executor does not run {what}; it is a Phase 2 item"
    )))
}
