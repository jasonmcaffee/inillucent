//! The physical pass: the existing planner's output becomes a pipeline.
//!
//! Invariant: this pass never changes what a query means, only how it is run.
//! Every construct it does not recognise is **refused** rather than
//! approximated - [`unsupported`] returns an error naming what was not handled,
//! so a query the new engine cannot run yet fails loudly instead of returning a
//! plausible wrong answer. That is the whole reason it is written as a
//! whitelist: a differential digest comparison catches a wrong answer, but only
//! for a query somebody thought to put in the corpus.
//!
//! ## Where this sits
//!
//! `inillucent-sql`'s lexer, parser, binder and planner survive the rearchitecture
//! unchanged - the TDD's component triage says so, and they are the part of the
//! old engine that was never the problem. What they produce is a
//! [`PhysicalPlan`]: FROM terms with access paths, residual predicates, an
//! aggregation mode, and a bound result list. This module turns that into the
//! operator chain in [`crate::ops`], [`crate::paged`] and [`crate::join`].
//!
//! ## Stages, and why a FROM term can be two of them
//!
//! The planner's unit is a FROM term. The executor's unit is a **stage**: one
//! tree, read one way, contributing a run of columns to the joined row. Most
//! terms are one stage, but a non-covering index seek is two - the index scan
//! that finds the rowids, and the table probe that fetches the rest of the row.
//! The TDD calls the second one `RowidLookup` and lists it as an operator; here
//! it is an [`crate::join::IndexNestedLoopJoin`] into the table tree keyed on
//! the index entry's rowid, because that is exactly what it is, and writing it
//! twice would be two chances to get the null handling different.
//!
//! Columns are numbered across the stages in order, so stage `i` owns
//! `offset[i] .. offset[i] + width[i]`, and a bound `Column { source, slot }`
//! resolves to whichever of that term's stages carries the slot - the table
//! stage if there is one, the index stage otherwise.
//!
//! ## How a bound column finds its vector
//!
//! A `BoundExpr::Column` names a *record slot* in the SQLite sense, because the
//! binder and the catalog were built for that layout. The new engine's trees do
//! not have record slots; they have mini-columns. [`SourceLayout`] is the map
//! between them, and it is built by whatever imported the table, because that
//! is the only thing that knows which slot became which column. Keying it that
//! way - rather than by name - is what lets the whole front end stay unchanged.

use inillucent_base::error::misuse;
use inillucent_base::DbResult;
use inillucent_pool::Pool;
use inillucent_sql::ast::{BinaryOp, NullOrder, PatternOp, SortOrder, UnaryOp};
use inillucent_sql::bind::{
    BoundExpr, BoundFrameBound, BoundOrderTerm, BoundResultColumn, BoundSelect, BoundWindow,
    WindowCall as BoundWindowCall,
};
use inillucent_sql::catalog_view::TableInfo;
use inillucent_sql::function::{AggregateFunc, ScalarFunc};
use inillucent_sql::plan::{
    plan_select_with, AccessPath, AggregationMode, BoundKind, Levers, PhysicalPlan, RangeBound,
};
use inillucent_tree::datum::{Datum, OwnedDatum};
use inillucent_tree::PagedTree;
use inillucent_value::affinity::Affinity;
use inillucent_value::collation::Collation;

use crate::aggregate::AggregateKind;
use crate::batch::Batch;
use crate::expr::{compile, ArithOp, CompareOp, Expr, StaticType};
use crate::join::{IndexNestedLoopJoin, JoinKind, ValuesScan};
use crate::ops::{
    AdjacentDistinct, AggregateSpec, CollectInto, Distinct, Filter, Flow, HashAggregate, Limit,
    Project, SimpleAggregate, Sink, Sort, SortKey, StreamAggregate, TopN,
};
use crate::paged::{FullScan, PointProbe, ReverseScan, SkipScan, SpanScan};
use crate::scan::Projection;
use crate::setop::{SetKeys, SetKind, SetOp};
use crate::window::{
    FrameEnd, OrderTerm as WindowOrderTerm, WindowCall, WindowFrame, WindowPlan, WindowSlot,
};

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

/// Where the executor finds its trees, its layouts and its pages.
pub trait TreeCatalog {
    /// Returns the buffer pool the trees' pages live in.
    fn pool(&self) -> &Pool;

    /// Returns the tree a plan's root page id refers to.
    ///
    /// The key is the SQLite root page from the fixture the data was imported
    /// from. That sounds like a leftover and is deliberate: the plan comes from
    /// a binder reading that fixture's schema, so the root page is the one
    /// identifier both sides already agree on, and using it means the import
    /// decides the mapping rather than a name lookup guessing at it.
    ///
    /// @param root - the root page id the plan named
    fn tree(&self, root: u32) -> Option<&PagedTree>;

    /// Returns the layout for a plan's root page id.
    ///
    /// @param root - the root page id the plan named
    fn layout(&self, root: u32) -> Option<&SourceLayout>;

    /// Returns the rows a virtual table produces, when the caller has one.
    ///
    /// **The module runs on the caller's side of this trait, and only its rows
    /// come back.** The executor never learns what a module is: it does not know
    /// about `best_index`, cursors, shadow tables or a module registry, and it
    /// could not - `inillucent-exec` sits below the crate that registers
    /// modules, deliberately, because a pipeline is built against what the
    /// caller supplies rather than against names it resolves itself.
    ///
    /// It is also the shape the TDD's **batch-aware vtab contract** asks for. A
    /// row-at-a-time cursor pulled through the operator chain would put a
    /// virtual call between every row and every batch; handing back rows the
    /// pipeline turns into batches puts the module's own loop inside the module,
    /// where it can produce a run at a time. The cost is that a virtual scan
    /// does not stream, which for the shapes a module answers - a MATCH, a
    /// bounding box - is a result set that fits in memory by construction.
    ///
    /// `None` means the caller has no virtual tables at all, which is what makes
    /// this a defaulted method rather than one every catalog has to write.
    ///
    /// @param table - the FROM term's table, which names the module's instance
    /// @param path - the access path the planner chose for it
    /// @param params - the values bound to `?1`, `?2`, ...
    fn virtual_rows(
        &self,
        table: &TableInfo,
        path: &AccessPath,
        params: &Params,
    ) -> DbResult<Option<Vec<Vec<OwnedDatum>>>> {
        let _ = (table, path, params);
        Ok(None)
    }

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

/// A physical choice a test or a `PRAGMA` can force.
///
/// The TDD's `PRAGMA inillucent.force_plan`, and the metamorphic tests' whole
/// mechanism: the same query is run under each applicable alternative and must
/// produce the same digest. A choice the plan cannot honour is an **error**,
/// not a silent fallback - a metamorphic test that quietly ran the default
/// twice would pass while proving nothing.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ForcePlan {
    /// Read the table rather than any covering index.
    pub table_scan: bool,
    /// Sort rather than keep a bounded heap, even under a `LIMIT`.
    pub full_sort: bool,
    /// Build a hash set rather than de-duplicating adjacent rows.
    pub hash_distinct: bool,
    /// Build a hash table rather than streaming a grouped aggregate.
    pub hash_group: bool,
    /// Walk every row rather than seeking one per distinct key prefix.
    pub no_skip_scan: bool,
}

impl ForcePlan {
    /// Returns the choice a `PRAGMA inillucent.force_plan` string names.
    ///
    /// The string is a comma-separated list of operator names, matching the
    /// TDD's `'<operator list>'`. An unknown name is refused rather than
    /// ignored, because a test that misspelled its own lever would otherwise
    /// report a pass.
    ///
    /// @param text - the pragma's value
    pub fn parse(text: &str) -> DbResult<ForcePlan> {
        let mut forced = ForcePlan::default();
        for name in text.split(',') {
            let name = name.trim().to_ascii_lowercase();
            if name.is_empty() {
                continue;
            }
            match name.as_str() {
                "scan" | "tablescan" => forced.table_scan = true,
                "sort" => forced.full_sort = true,
                "distinct" | "hashdistinct" => forced.hash_distinct = true,
                "hashaggregate" | "hashgroup" => forced.hash_group = true,
                "noskipscan" | "noskip" => forced.no_skip_scan = true,
                other => {
                    return Err(misuse(format!(
                        "force_plan does not know the operator '{other}'"
                    )))
                }
            }
        }
        Ok(forced)
    }

    /// Returns every lever, for the metamorphic sweep.
    pub fn alternatives() -> Vec<(&'static str, ForcePlan)> {
        vec![
            ("default", ForcePlan::default()),
            (
                "scan",
                ForcePlan {
                    table_scan: true,
                    ..ForcePlan::default()
                },
            ),
            (
                "sort",
                ForcePlan {
                    full_sort: true,
                    ..ForcePlan::default()
                },
            ),
            (
                "distinct",
                ForcePlan {
                    hash_distinct: true,
                    ..ForcePlan::default()
                },
            ),
            (
                "hashgroup",
                ForcePlan {
                    hash_group: true,
                    ..ForcePlan::default()
                },
            ),
            (
                "noskip",
                ForcePlan {
                    no_skip_scan: true,
                    ..ForcePlan::default()
                },
            ),
        ]
    }
}

/// The values bound to `?1`, `?2`, ... for one execution.
#[derive(Clone, Debug, Default)]
pub struct Params {
    values: Vec<OwnedDatum>,
    /// How many times a parameter has been read out of this set.
    ///
    /// The counter is what makes [`Statement`] safe. A statement may only be
    /// re-run against new parameters if nothing but its *source* looked at the
    /// old ones - a `LIMIT ?1`, a projected `?2` or a residual filter over a
    /// parameter is baked into the operator chain when the chain is built, and
    /// re-running that chain against different values would answer the previous
    /// question with the new question's parameters.
    ///
    /// Deciding that by inspecting the plan means a second, separate opinion
    /// about which constructs can carry a parameter, which is exactly the kind
    /// of duplicated judgement that goes stale when a construct is added.
    /// Counting the reads asks the builder instead: every path that consumes a
    /// parameter goes through [`Params::get`], so if the count does not move
    /// while everything except the source is built, nothing except the source
    /// read one.
    reads: std::cell::Cell<u64>,
}

impl Params {
    /// Returns an empty parameter set.
    pub fn new() -> Params {
        Params {
            values: Vec::new(),
            reads: std::cell::Cell::new(0),
        }
    }

    /// Returns a parameter set over a list of values, `?1` first.
    ///
    /// @param values - the values, in parameter order
    pub fn from_values(values: Vec<OwnedDatum>) -> Params {
        Params {
            values,
            reads: std::cell::Cell::new(0),
        }
    }

    /// Returns how many parameter reads this set has answered.
    pub fn reads(&self) -> u64 {
        self.reads.get()
    }

    /// Replaces every bound value, reusing the buffer.
    ///
    /// A benchmark that re-binds a prepared statement per iteration should not
    /// allocate to do it - `sqlite3_bind_int64` does not - and building a fresh
    /// `Params` per execution was one `Vec` per execution on the arm being
    /// timed.
    ///
    /// @param values - the new values, `?1` first
    pub fn refill(&mut self, values: impl IntoIterator<Item = OwnedDatum>) {
        self.values.clear();
        self.values.extend(values);
    }

    /// Returns the value bound to a parameter.
    ///
    /// An unbound parameter is NULL, which is what SQLite does.
    ///
    /// @param index - the one-based parameter number
    pub fn get(&self, index: u32) -> OwnedDatum {
        self.reads.set(self.reads.get().saturating_add(1));
        self.values
            .get(index.saturating_sub(1) as usize)
            .cloned()
            .unwrap_or(OwnedDatum::Null)
    }

    /// Returns how many parameters are bound.
    pub fn len(&self) -> usize {
        self.values.len()
    }

    /// Reports whether nothing is bound.
    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }
}

/// How one stage reads its tree.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AccessKind {
    /// Every row of the tree, in key order.
    Full,
    /// A key range, forward.
    Span,
    /// A key range, backward.
    Reverse,
    /// One row per distinct key prefix.
    Skip,
    /// One row by key.
    Point,
    /// A probe of this tree once per row of the stage before it.
    Nested,
    /// The rows a nested query produced, materialised once before the pipeline
    /// runs.
    ///
    /// A `FROM (SELECT ...)` term reads no tree, so this stage's `root` names
    /// nothing and its layout is carried on the stage rather than looked up.
    /// Materialising rather than streaming is what a push executor can do
    /// without a coroutine: the inner pipeline runs to completion into a buffer
    /// and the buffer drives the outer one.
    Materialised,
}

impl AccessKind {
    /// Returns the name `EXPLAIN` prints.
    pub fn describe(self) -> &'static str {
        match self {
            AccessKind::Full => "SCAN",
            AccessKind::Span => "RANGE",
            AccessKind::Reverse => "RANGE REVERSE",
            AccessKind::Skip => "SKIP SCAN",
            AccessKind::Point => "POINT PROBE",
            AccessKind::Nested => "INDEX NESTED LOOP",
            AccessKind::Materialised => "SCAN SUBQUERY",
        }
    }
}

/// One stage's physical choice.
#[derive(Clone, Debug)]
pub struct PreparedStage {
    /// The tree this stage reads.
    pub root: u32,
    /// How it reads it.
    pub kind: AccessKind,
    /// The **statement-wide** id every bound expression refers to this FROM
    /// term by.
    ///
    /// Not its position in `plan.sources`, and the two are different exactly
    /// when the planner reorders the join - which is the case this got wrong.
    /// A bound `Column { source, slot }` carries the binder's id, so matching it
    /// against a position silently resolved every column of a reordered join to
    /// the wrong stage: `SELECT people.name, teams.region FROM people JOIN
    /// teams` returned no rows while `FROM teams JOIN people` returned the
    /// right nine, because only the second one has position equal to id.
    pub source: usize,
    /// This stage's position in `plan.sources`, which is the visit order.
    ///
    /// The other half of the same distinction: the *plan's* own arrays are
    /// indexed by visit order, so a stage needs both numbers and conflating
    /// them is a wrong answer rather than an error.
    pub term: usize,
    /// Whether this stage is the table fetch behind a non-covering index seek.
    pub is_lookup: bool,
    /// The first column index this stage contributes to the joined row.
    pub offset: usize,
    /// How many columns it contributes.
    pub width: usize,
    /// The layout of a stage that reads no tree.
    ///
    /// `None` for every stage that reads one, whose layout the catalog holds.
    /// A materialised subquery has no entry in the catalog to hold it, and
    /// synthesising one *here* rather than registering it in the catalog is
    /// what keeps the catalog a description of the file.
    pub layout: Option<SourceLayout>,
}

/// What a statement's physical choices are, decided once.
///
/// The structural decisions - which tree to read, and therefore whether a sort,
/// a hash table or a set is needed at all - depend on the statement and the
/// schema and not on the data, so they belong to prepare rather than to
/// execution. Keeping them here is not only tidiness: the covering rule tries
/// candidate trees by *building* a pipeline over each, and doing that on every
/// execution made a 64-row query spend more time choosing than answering.
#[derive(Clone, Debug)]
pub struct Prepared {
    /// The stages, outermost first.
    pub stages: Vec<PreparedStage>,
    /// The levers this plan was prepared under.
    pub forced: ForcePlan,
}

impl Prepared {
    /// Returns the tree the outermost stage reads.
    ///
    /// Kept because the gate harness reports the structure each engine chose,
    /// and "which tree" is most of that answer.
    pub fn root(&self) -> u32 {
        self.stages.first().map(|stage| stage.root).unwrap_or(0)
    }

    /// Returns one line per stage, for `EXPLAIN`.
    pub fn describe(&self) -> Vec<String> {
        self.stages
            .iter()
            .map(|stage| {
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
            })
            .collect()
    }
}

/// A built pipeline, ready to run.
pub struct Pipeline<'t> {
    /// What drives it.
    pub source: Source<'t>,
    /// The head of the operator chain.
    ///
    /// It borrows for `'t` because an index nested loop holds the inner tree
    /// and the pool, and it sits at the *bottom* of the chain - closest to the
    /// source - so everything above it is still an ordinary owned operator.
    /// That is why only this one box carries a lifetime and none of the
    /// operators in [`crate::ops`] had to grow one.
    pub head: Box<dyn Sink + 't>,
    /// The pool the source's pages live in.
    pub pool: &'t Pool,
}

impl Pipeline<'_> {
    /// Drives the pipeline to completion.
    pub fn run(&mut self) -> DbResult<()> {
        self.source.run(self.pool, self.head.as_mut())
    }
}

/// How many seek-key columns a point probe borrows on the stack.
///
/// Four covers every rowid table and every index in the scorecard fixture and
/// in the dialect's own corpus; a wider key spills, which costs what every key
/// used to cost.
const POINT_KEY_INLINE: usize = 4;

/// What drives a pipeline.
pub enum Source<'t> {
    /// Every row of a tree, in key order.
    Scan(FullScan<'t>),
    /// A key range, forward.
    Span(SpanScan<'t>),
    /// A key range, backward.
    Reverse(ReverseScan<'t>),
    /// One row per distinct value of a key prefix.
    Skip(SkipScan<'t>),
    /// One row by key.
    Point(PointProbe<'t>, Vec<OwnedDatum>),
    /// Rows a nested query produced, already materialised.
    Rows(Vec<Vec<OwnedDatum>>),
    /// A fixed number of rows of no columns at all.
    ///
    /// What drives `SELECT 1`, `SELECT date('now')` and every other query with
    /// no FROM term: there is exactly one row, it has no columns, and the whole
    /// answer comes out of the projection's constant expressions. A batch of one
    /// row and zero vectors is a perfectly ordinary batch - `Batch::live`
    /// returns the row count and every consumer's fast path is over the columns
    /// it was asked for, of which there are none.
    ///
    /// The Phase 2 pass refused these, and nineteen of the read-only SLT
    /// corpus's thirty-seven refusals were exactly this shape.
    Constant(usize),
}

impl Source<'_> {
    /// Drives the source until the pipeline is done.
    ///
    /// @param pool - the buffer pool
    /// @param downstream - the head of the operator chain
    pub fn run(&self, pool: &Pool, downstream: &mut dyn Sink) -> DbResult<()> {
        match self {
            Source::Scan(scan) => scan.run(pool, downstream),
            Source::Span(scan) => scan.run(pool, downstream),
            Source::Reverse(scan) => scan.run(pool, downstream),
            Source::Skip(scan) => scan.run(pool, downstream),
            Source::Point(probe, key) => {
                // The borrows go on the stack. A seek key is one column in
                // every rowid table and at most a handful in any index, and
                // collecting them was one allocation per execution on the
                // shortest path the engine has.
                let mut inline: [Datum<'_>; POINT_KEY_INLINE] = [Datum::Null; POINT_KEY_INLINE];
                let spilled: Vec<Datum<'_>>;
                let borrowed: &[Datum<'_>] = if key.len() <= POINT_KEY_INLINE {
                    for (at, value) in key.iter().enumerate() {
                        if let Some(slot) = inline.get_mut(at) {
                            *slot = value.borrow();
                        }
                    }
                    inline.get(..key.len()).unwrap_or(&[])
                } else {
                    spilled = key.iter().map(OwnedDatum::borrow).collect();
                    spilled.as_slice()
                };
                probe.run(pool, borrowed, downstream)
            }
            Source::Rows(rows) => {
                crate::ops::emit_rows(rows, downstream)?;
                downstream.finish()
            }
            Source::Constant(rows) => {
                if *rows > 0 {
                    let batch = Batch::new(*rows, Vec::new());
                    downstream.push(&batch)?;
                }
                downstream.finish()
            }
        }
    }

    /// Names the source, for a plan description.
    pub fn describe(&self) -> &'static str {
        match self {
            Source::Scan(_) => "SCAN",
            Source::Span(_) => "RANGE",
            Source::Reverse(_) => "RANGE REVERSE",
            Source::Skip(_) => "SKIP SCAN",
            Source::Point(_, _) => "POINT PROBE",
            Source::Rows(_) => "SCAN SUBQUERY",
            Source::Constant(_) => "CONSTANT ROW",
        }
    }
}

/// What a built plan produces, so a caller can name its columns.
#[derive(Clone, Debug)]
pub struct Shape {
    /// The name of each output column, as the binder assigned it.
    pub names: Vec<Vec<u8>>,
    /// The operator chain, source first: the TDD's "`EXPLAIN` prints the
    /// physical operator tree".
    ///
    /// It is built as the chain is built rather than derived afterwards,
    /// because a description derived from the plan is a description of what the
    /// builder was *asked* for. This one says what it made. The first thing it
    /// showed was a `Filter` under a range scan whose bounds already excluded
    /// every row it was testing.
    pub operators: Vec<String>,
}

/// Returns an error naming what the physical pass will not run.
///
/// @param what - the construct, in words
fn unsupported<T>(what: &str) -> DbResult<T> {
    Err(misuse(format!(
        "the new engine's physical pass does not handle {what} yet"
    )))
}

/// Chooses a statement's physical plan.
///
/// @param plan - the planner's output
/// @param catalog - where the trees and layouts come from
/// @param forced - the levers a `PRAGMA` or a metamorphic test set
pub fn prepare(
    plan: &PhysicalPlan,
    catalog: &dyn TreeCatalog,
    forced: ForcePlan,
) -> DbResult<Prepared> {
    let mut stages = plan_stages(plan, catalog, None)?;
    // The covering rule. A plain scan of a table is replaced by a scan of the
    // smallest index tree that carries every column the query reads, because
    // reading 2.66 MiB instead of 14.6 MiB is the single largest lever
    // available on an analytical query and it is the structure SQLite itself
    // chooses. The test for "carries every column" is not a heuristic: the
    // whole pipeline is built against the candidate's layout, and translation
    // fails by name on a column the tree does not hold. A candidate that
    // builds, covers.
    let single_scan = stages.len() == 1
        && stages
            .first()
            .map(|stage| stage.kind == AccessKind::Full)
            .unwrap_or(false)
        && matches!(
            plan.sources.first().map(|source| &source.path),
            Some(AccessPath::TableScan { .. })
        );
    if single_scan && !forced.table_scan && !order_sensitive(&plan.select) {
        let table_root = stages.first().map(|stage| stage.root).unwrap_or(0);
        for candidate in catalog.covering_candidates(table_root) {
            let trial = plan_stages(plan, catalog, Some(candidate))?;
            let attempt = Prepared {
                stages: trial,
                forced,
            };
            if build_prepared(plan, catalog, &attempt, &Params::new(), dummy_sink()).is_ok() {
                return Ok(attempt);
            }
        }
    }
    // A skip scan is a structural choice too, and it is decided here so that
    // execution never has to.
    if let Some(stage) = stages.first_mut() {
        if stage.kind == AccessKind::Full
            && !forced.no_skip_scan
            && skip_scan_applies(plan, catalog, stage.root)?
        {
            stage.kind = AccessKind::Skip;
        }
    }
    Ok(Prepared { stages, forced })
}

/// Returns the collation an expression is compared and ordered under.
///
/// SQLite's rule, in the part that matters here: an explicit `COLLATE` wins; a
/// column carries its own; everything else is BINARY. It is deliberately not a
/// full implementation of the rule - a `CASE` whose branches are columns has an
/// assignable collation in SQLite and BINARY here - because the conservative
/// answer is the one that sorts and groups by bytes, which is what an engine
/// that did not know about collations at all would do, and never a wrong answer
/// dressed as a right one.
///
/// @param expr - the bound expression
fn expression_collation(expr: &BoundExpr) -> Collation {
    match expr {
        BoundExpr::Collate { collation, .. } => *collation,
        BoundExpr::Column { collation, .. } => *collation,
        _ => Collation::Binary,
    }
}

/// Reports whether the statement's answer depends on the order its rows arrive.
///
/// The covering rule replaces a table scan with an index scan, which is the
/// single largest lever in the whole design - and it *changes the order the
/// rows reach the aggregate in*. Floating-point addition is not associative, so
/// that is not a free change: the SLT corpus has a `score` column holding
/// `-1e300`, nine ordinary values and `+1e300`, and
/// `SELECT sum(score) FROM people` is 124.25 in table order and **0.0** in
/// score order, because the small values are absorbed into the first huge one
/// and cancelled by the second. SQLite scans the table and gets 124.25; we
/// scanned `people_by_score` and got 0.0.
///
/// So the rule is skipped when a `sum`, `total` or `avg` has an argument that
/// is not statically an integer, and when a `group_concat` is present - it
/// concatenates in arrival order by definition. An integer sum accumulates in
/// `i128` and is exact, so its order does not matter, which is what keeps the
/// scorecard's `sum(key)` on the covering index where SQLite also puts it.
///
/// This is the Phase 1 lesson from the other side. Structure was the largest
/// lever there; here it is a wrong answer.
///
/// @param select - the bound statement
fn order_sensitive(select: &BoundSelect) -> bool {
    select.aggregates.iter().any(|call| match call.func {
        AggregateFunc::Sum | AggregateFunc::Total | AggregateFunc::Avg => call
            .arguments
            .first()
            .map(|argument| !integer_typed(argument))
            .unwrap_or(false),
        AggregateFunc::GroupConcat => true,
        _ => false,
    })
}

/// Reports whether a bound expression is statically an integer.
///
/// Conservative: anything it cannot prove is treated as not an integer, because
/// the cost of being wrong is a wrong answer and the cost of being cautious is
/// a table scan.
///
/// @param expr - the aggregate's argument
fn integer_typed(expr: &BoundExpr) -> bool {
    match expr {
        BoundExpr::Integer(_) => true,
        BoundExpr::Rowid { .. } => true,
        BoundExpr::Column { affinity, .. } => {
            *affinity == inillucent_value::affinity::Affinity::Integer
        }
        _ => false,
    }
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

/// Turns the planner's FROM terms into stages.
///
/// @param plan - the planner's output
/// @param catalog - where the trees and layouts come from
/// @param override_root - a covering index to read instead of the table
fn plan_stages(
    plan: &PhysicalPlan,
    catalog: &dyn TreeCatalog,
    override_root: Option<u32>,
) -> DbResult<Vec<PreparedStage>> {
    refuse_unhandled(&plan.select)?;
    if !plan.compounds.is_empty() {
        return unsupported("a compound query");
    }
    // A query with no FROM term produces no stages at all, and that is a legal
    // plan rather than a refusal: `SELECT 1` reads no tree, so there is nothing
    // for a stage to describe. Every function below already loops over the
    // stages rather than indexing the first, except the two that build the
    // source and the space - and both now have an empty case.
    let mut stages: Vec<PreparedStage> = Vec::new();
    let mut offset = 0usize;
    let sensitive = order_sensitive(&plan.select);
    for (position, source) in plan.sources.iter().enumerate() {
        let outermost = position == 0;
        // **An outer join is refused rather than answered as an inner one.**
        //
        // The physical pass never looked at the join kind and always built
        // `JoinKind::Inner`, so a `LEFT JOIN` silently dropped the outer rows
        // that matched nothing. That was unreachable while every such query was
        // refused for another reason, and the moment a keyless inner term became
        // runnable the differential corpus produced it: `SELECT people.team FROM
        // people LEFT JOIN teams ON ...` answered six rows as four nulls.
        //
        // Doing it properly needs the `ON` condition evaluated per candidate
        // pair - an index nested loop assumes the key equality *is* the
        // condition - which is a real operator rather than a flag, and it is not
        // in this phase's scope. A named refusal is the honest state until it
        // is: the corpus reports it, and nobody gets a wrong answer meanwhile.
        if !outermost
            && matches!(
                source.join,
                inillucent_sql::ast::JoinKind::Left
                    | inillucent_sql::ast::JoinKind::Right
                    | inillucent_sql::ast::JoinKind::Full
            )
        {
            return unsupported("an outer join");
        }
        match &source.path {
            AccessPath::TableScan { root } => {
                let root = if outermost {
                    override_root.unwrap_or(*root)
                } else {
                    *root
                };
                push_stage(
                    &mut stages,
                    catalog,
                    root,
                    if outermost {
                        AccessKind::Full
                    } else {
                        // An inner term with no usable index is a cross
                        // product: every inner row pairs with every outer one,
                        // and any predicate over the pair is a residual. Phase 2
                        // refused it because the read families never produce
                        // one; the corpora do - `SELECT count(*) FROM people
                        // CROSS JOIN teams` - and refusing a shape the engine
                        // can answer is a gap rather than a policy.
                        AccessKind::Nested
                    },
                    source.id,
                    position,
                    false,
                    &mut offset,
                )?;
            }
            AccessPath::RowidSeek { root, .. } => {
                push_stage(
                    &mut stages,
                    catalog,
                    *root,
                    if outermost {
                        AccessKind::Point
                    } else {
                        AccessKind::Nested
                    },
                    source.id,
                    position,
                    false,
                    &mut offset,
                )?;
            }
            AccessPath::RowidRange { root, .. } => {
                let kind = if !outermost {
                    return unsupported("a rowid range as an inner join term");
                } else if plan.reverse {
                    AccessKind::Reverse
                } else {
                    AccessKind::Span
                };
                push_stage(
                    &mut stages,
                    catalog,
                    *root,
                    kind,
                    source.id,
                    position,
                    false,
                    &mut offset,
                )?;
            }
            AccessPath::IndexSeek {
                table_root,
                index_root,
                covering,
                equalities,
                low,
                high,
                ..
            } => {
                // An index seek with no equality and no bound is a *scan* of
                // the index, not a range over it. The distinction is not
                // cosmetic: the covering rule and the skip-scan rule both key
                // on `Full`, and calling this a range left `scan.distinct`
                // reading every row of the index where SQLite seeks 64 times.
                let unbounded = equalities.is_empty() && low.is_none() && high.is_none();
                // The planner's *own* covering choice is subject to the same
                // rule the physical pass's covering rule is: reading fewer
                // bytes out of an index changes the order the rows reach an
                // aggregate in, and a floating-point sum is not associative.
                // `SELECT sum(score) FROM people` over `people_by_score` is
                // 0.0 where the table gives 124.25, because the corpus holds
                // `-1e300` and `+1e300` and the small values vanish between
                // them. SQLite reads the table here, and so must this.
                if unbounded && covering.is_some() && outermost && sensitive {
                    push_stage(
                        &mut stages,
                        catalog,
                        *table_root,
                        AccessKind::Full,
                        source.id,
                        position,
                        false,
                        &mut offset,
                    )?;
                    continue;
                }
                let kind = if outermost {
                    if plan.reverse {
                        AccessKind::Reverse
                    } else if unbounded {
                        AccessKind::Full
                    } else {
                        AccessKind::Span
                    }
                } else {
                    AccessKind::Nested
                };
                push_stage(
                    &mut stages,
                    catalog,
                    *index_root,
                    kind,
                    source.id,
                    position,
                    false,
                    &mut offset,
                )?;
                // A `WITHOUT ROWID` table's primary-key index *is* the table:
                // one b-tree, reported at the table's own root page. So it
                // carries every column by construction and there is no rowid to
                // look anything up by - which is exactly what the lookup stage
                // below tried to do, five times over in the differential
                // corpus, with "the index entry carries no rowid".
                let is_the_table = *index_root == *table_root;
                if covering.is_none() && !is_the_table {
                    // The index does not carry every column the query reads, so
                    // the row is fetched from the table by rowid. That is the
                    // TDD's `RowidLookup`, expressed as what it is: a nested
                    // loop into the table tree keyed on the entry's rowid.
                    push_stage(
                        &mut stages,
                        catalog,
                        *table_root,
                        AccessKind::Nested,
                        source.id,
                        position,
                        true,
                        &mut offset,
                    )?;
                }
            }
            AccessPath::Subquery {
                width, correlated, ..
            } => {
                if !outermost {
                    // An inner subquery has to be rebuilt or rescanned once per
                    // outer row, which is a nested loop over a materialised
                    // buffer rather than over a tree. Refusing it is honest;
                    // the outermost case below is the one the corpus needs.
                    return unsupported("a subquery as an inner join term");
                }
                if *correlated {
                    // A correlated subquery reads a FROM term outside itself,
                    // and the outermost term has nothing outside it - so this
                    // is a plan that should not exist rather than one to run.
                    return unsupported("a correlated subquery as the outermost term");
                }
                stages.push(PreparedStage {
                    root: 0,
                    kind: AccessKind::Materialised,
                    source: source.id,
                    term: position,
                    is_lookup: false,
                    offset,
                    width: *width,
                    // A materialised row is its own record: slot `i` is column
                    // `i`, there is no rowid, and nothing is known about the
                    // order - so no streaming rule may assume one.
                    layout: Some(SourceLayout {
                        tree_key: 0,
                        slots: (0..*width).map(Some).collect(),
                        rowid: None,
                        types: vec![StaticType::Unknown; *width],
                        width: *width,
                        key_columns: Vec::new(),
                    }),
                });
                offset = offset.saturating_add(*width);
            }
            AccessPath::Recursive { .. } | AccessPath::RecursiveSelf { .. } => {
                return unsupported("a recursive CTE")
            }
            // A virtual table is a *materialised* stage: the module produces
            // its rows on the caller's side and the pipeline reads them, which
            // is the same shape a subquery already has.
            AccessPath::VirtualScan { .. } => {
                if !outermost {
                    // A module answering once per outer row is a nested loop
                    // into somebody else's code, and the plan the module chose
                    // was chosen for one set of constraints. Refused by name.
                    return unsupported("a virtual table as an inner join term");
                }
                let width = source.table.columns.len().max(1);
                stages.push(PreparedStage {
                    root: 0,
                    kind: AccessKind::Materialised,
                    source: source.id,
                    term: position,
                    is_lookup: false,
                    offset,
                    width,
                    // A module's row is its own record, exactly as a
                    // materialised subquery's is: slot `i` is column `i`, there
                    // is no rowid, and nothing is known about the order.
                    layout: Some(SourceLayout {
                        tree_key: 0,
                        slots: (0..width).map(Some).collect(),
                        rowid: None,
                        types: vec![StaticType::Unknown; width],
                        width,
                        key_columns: Vec::new(),
                    }),
                });
                offset = offset.saturating_add(width);
            }
        }
    }
    Ok(stages)
}

/// Adds one stage and advances the column offset.
///
/// @param stages - the stages built so far
/// @param catalog - where the layouts come from
/// @param root - the tree this stage reads
/// @param kind - how it reads it
/// @param source - which planner FROM term it belongs to
/// @param is_lookup - whether it is the table fetch behind an index seek
/// @param offset - the next free column index, advanced
#[allow(clippy::too_many_arguments)]
fn push_stage(
    stages: &mut Vec<PreparedStage>,
    catalog: &dyn TreeCatalog,
    root: u32,
    kind: AccessKind,
    source: usize,
    term: usize,
    is_lookup: bool,
    offset: &mut usize,
) -> DbResult<()> {
    let layout = catalog
        .layout(root)
        .ok_or_else(|| misuse(format!("no layout imported for root page {root}")))?;
    stages.push(PreparedStage {
        root,
        kind,
        source,
        term,
        is_lookup,
        offset: *offset,
        width: layout.width,
        layout: None,
    });
    *offset = offset.saturating_add(layout.width);
    Ok(())
}

/// Refuses the parts of a bound select the physical pass does not implement.
///
/// @param select - the bound statement
fn refuse_unhandled(select: &BoundSelect) -> DbResult<()> {
    // A window function is not refused here any more: `run_any` routes a
    // windowed query to `run_windowed` before a pipeline is prepared at all,
    // and a window that reached this point would be one nothing routed - which
    // is a bug in the dispatcher rather than a query the engine cannot answer.
    if !select.windows.is_empty() {
        return unsupported("a window function reaching the pipeline builder");
    }
    Ok(())
}

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
    pub(crate) layouts: &'c [SourceLayout],
    /// The static type of every column of the joined row.
    pub(crate) types: &'c [StaticType],
    /// The tree columns the *joined* rows arrive sorted by, when they do.
    pub(crate) order: &'c [usize],
}

impl Space<'_> {
    /// Returns the joined-row column a bound column reference names.
    ///
    /// A FROM term may be two stages, so the slot is looked for in the table
    /// stage first and the index stage second: the table carries every column
    /// and the index only some, and preferring the table means a query that
    /// reads a column the index happens to hold still reads it from wherever
    /// the row was actually fetched.
    ///
    /// @param source - the planner FROM term
    /// @param slot - the record slot
    fn column(&self, source: usize, slot: usize) -> Option<usize> {
        let mut found = None;
        for (index, stage) in self.stages.iter().enumerate() {
            if stage.source != source {
                continue;
            }
            let layout = self.layouts.get(index)?;
            if let Some(Some(tree_column)) = layout.slots.get(slot) {
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
    fn rowid(&self, source: usize) -> Option<usize> {
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
    let held = space_of(catalog, prepared)?;
    let space = held.view(&prepared.stages);
    let chain = build_chain(plan, catalog, prepared, &space, params, sink)?;
    let (source, description) = source_for(plan, catalog, &space, params, prepared, chain.limit)?;
    let mut operators = chain.operators;
    operators.push(description);
    operators.reverse();
    Ok((
        Pipeline {
            source,
            head: chain.head,
            pool: catalog.pool(),
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
    pub(crate) layouts: Vec<SourceLayout>,
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
        Space {
            stages,
            layouts: &self.layouts,
            types: &self.types,
            order: &self.order,
        }
    }
}

/// Returns the column space a statement's stages define.
///
/// @param catalog - where the layouts come from
/// @param prepared - the structural choices [`prepare`] made
fn space_of(catalog: &dyn TreeCatalog, prepared: &Prepared) -> DbResult<HeldSpace> {
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
    let order = match (prepared.stages.first(), layouts.first()) {
        (Some(stage), Some(layout)) if prepared.stages.len() == 1 => {
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

/// Everything a built operator chain is, short of the source that drives it.
struct Chain<'t> {
    /// The head of the chain: what the source pushes into.
    head: Box<dyn Sink + 't>,
    /// The operator descriptions, sink first; the source is appended last.
    operators: Vec<String>,
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
fn build_chain<'t>(
    plan: &PhysicalPlan,
    catalog: &'t dyn TreeCatalog,
    prepared: &Prepared,
    space: &Space<'_>,
    params: &Params,
    sink: Box<dyn Sink>,
) -> DbResult<Chain<'t>> {
    let select = &plan.select;
    refuse_unhandled(select)?;
    let space = &Space {
        stages: space.stages,
        layouts: space.layouts,
        types: space.types,
        order: space.order,
    };

    let scan_types = space.types.to_vec();
    let group_width = select.group_by.len();
    let skipping = prepared
        .stages
        .first()
        .map(|stage| stage.kind == AccessKind::Skip)
        .unwrap_or(false);

    // Result columns and ORDER BY terms, in the space that exists after any
    // aggregation. Terms that are not already result columns are carried
    // through the sort as extra columns and trimmed afterwards.
    let mut projected: Vec<Expr> = Vec::with_capacity(select.columns.len());
    for column in &select.columns {
        projected.push(translate_post(
            &column.expr,
            select,
            &space,
            params,
            group_width,
        )?);
    }
    let result_width = projected.len();
    let mut sort_keys: Vec<SortKey> = Vec::new();
    for term in &select.order_by {
        let translated = translate_post(&term.expr, select, &space, params, group_width)?;
        let existing = projected
            .iter()
            .position(|held| same_expr(held, &translated));
        let column = match existing {
            Some(index) => index,
            None => {
                if select.distinct {
                    return unsupported(
                        "ORDER BY over an expression not in a DISTINCT select list",
                    );
                }
                projected.push(translated);
                projected.len().saturating_sub(1)
            }
        };
        let descending = term.order == SortOrder::Descending;
        sort_keys.push(SortKey {
            column,
            descending,
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

    let scan_order = &space.order;
    let group_exprs = select
        .group_by
        .iter()
        .map(|expr| translate_scan(expr, &space, params))
        .collect::<DbResult<Vec<Expr>>>()?;
    // `GROUP BY team` on a `COLLATE NOCASE` column has one group for `blue`
    // and `Blue`; grouping by bytes has two, and the counts are then wrong
    // rather than merely differently ordered.
    let group_collations: Vec<Collation> =
        select.group_by.iter().map(expression_collation).collect();
    let grouped_walk = plan.aggregation == AggregationMode::Grouped
        && !prepared.forced.hash_group
        && is_scan_prefix(&group_exprs, scan_order);

    // Whether the projected rows arrive in the order the ORDER BY asks for.
    let reversed = prepared
        .stages
        .first()
        .map(|stage| stage.kind == AccessKind::Reverse)
        .unwrap_or(false);
    // A non-default NULL placement is a real ordering requirement, and no scan
    // order satisfies it by accident.
    let default_nulls = sort_keys
        .iter()
        .all(|term| term.nulls_first != term.descending);
    let sorted_already = if !default_nulls {
        false
    } else if reversed {
        // A reverse scan produces descending key order, so a descending
        // ORDER BY over the key is satisfied by the direction rather than by a
        // sorter. `plan.reverse` is only ever set when the planner already
        // decided that, which is why the condition is the planner's answer
        // rather than a second derivation of it.
        !sort_keys.is_empty() && !plan.needs_sort
    } else {
        !sort_keys.is_empty()
            && sort_keys.iter().all(|term| !term.descending)
            && output_is_sorted_by(&sort_keys, &projected, plan, scan_order, grouped_walk)
    };
    let sorted_already = sorted_already || (skipping && !sort_keys.is_empty());

    // Built bottom-up, because each operator owns the one below it. The
    // description is collected in the same order and reversed at the end, so it
    // reads source-first the way a plan should.
    let mut operators: Vec<String> = Vec::new();
    let mut chain: Box<dyn Sink> = sink;

    let limit = constant_limit(select, params)?;
    let offset = constant_offset(select, params)?.unwrap_or(0);
    if sort_keys.is_empty() || sorted_already {
        if let Some(limit) = limit {
            chain = Box::new(Limit::new(limit, offset, chain));
            operators.push(format!("LIMIT {limit} OFFSET {offset}"));
        }
        if needs_trim {
            chain = Box::new(Project::new(trim(result_width, &scan_types)?, chain));
            operators.push("TRIM".to_string());
        }
    } else if let Some(limit) = limit {
        if needs_trim {
            chain = Box::new(Project::new(trim(result_width, &scan_types)?, chain));
            operators.push("TRIM".to_string());
        }
        let bounded = limit.saturating_add(offset);
        if bounded <= TopN::MAX_LIMIT && !prepared.forced.full_sort {
            if offset > 0 {
                chain = Box::new(Limit::new(limit, offset, chain));
                operators.push(format!("LIMIT {limit} OFFSET {offset}"));
            }
            chain = Box::new(TopN::new(sort_keys.clone(), bounded, chain));
            operators.push(format!("TOP {bounded}"));
        } else {
            chain = Box::new(Limit::new(limit, offset, chain));
            chain = Box::new(Sort::new(sort_keys.clone(), chain));
            operators.push(format!("LIMIT {limit} OFFSET {offset}"));
            operators.push("SORT".to_string());
        }
    } else {
        if needs_trim {
            chain = Box::new(Project::new(trim(result_width, &scan_types)?, chain));
            operators.push("TRIM".to_string());
        }
        chain = Box::new(Sort::new(sort_keys.clone(), chain));
        operators.push("SORT".to_string());
    }

    // The collation of each output column, for `DISTINCT`. A `DISTINCT` over a
    // `COLLATE NOCASE` column keeps one of `blue` and `Blue`, and one that
    // compared bytes keeps both.
    let output_collations: Vec<Collation> = select
        .columns
        .iter()
        .map(|column| expression_collation(&column.expr))
        .collect();
    if select.distinct && !skipping {
        if plan.aggregation == AggregationMode::None
            && !prepared.forced.hash_distinct
            && is_scan_prefix(&projected, scan_order)
        {
            chain = Box::new(AdjacentDistinct::new(output_collations.clone(), chain));
            operators.push("DISTINCT ADJACENT".to_string());
        } else {
            chain = Box::new(Distinct::new(output_collations.clone(), chain));
            operators.push("DISTINCT HASH".to_string());
        }
    }

    let projection_input_types = if plan.aggregation == AggregationMode::None {
        scan_types.clone()
    } else {
        aggregate_output_types(select, &space, params)?
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
    operators.push("PROJECT".to_string());

    // `HAVING` filters *groups*, so it sits between the aggregate and the
    // projection: it reads accumulators and `GROUP BY` keys, which is the same
    // space a result column reads, and it runs before the projection throws
    // away the columns it needs. Building it here rather than beside the
    // `WHERE` filters is the whole of the difference between the two clauses.
    if let Some(having) = &select.having {
        let translated = translate_post(having, select, &space, params, group_width)?;
        chain = Box::new(Filter::new(
            compile(&translated, &projection_input_types)?,
            chain,
        ));
        operators.push("FILTER HAVING".to_string());
    }

    match plan.aggregation {
        AggregationMode::None => {}
        AggregationMode::Whole => {
            chain = Box::new(SimpleAggregate::new(
                aggregate_specs(select, &space, params, &scan_types)?,
                chain,
            ));
            operators.push("AGGREGATE".to_string());
        }
        AggregationMode::Grouped => {
            let keys = group_exprs
                .iter()
                .map(|expr| compile(expr, &scan_types))
                .collect::<DbResult<Vec<_>>>()?;
            let specs = aggregate_specs(select, &space, params, &scan_types)?;
            chain = if grouped_walk {
                operators.push("GROUP STREAM".to_string());
                Box::new(StreamAggregate::new(
                    keys,
                    group_collations.clone(),
                    specs,
                    chain,
                ))
            } else {
                operators.push("GROUP HASH".to_string());
                Box::new(HashAggregate::new(
                    keys,
                    group_collations.clone(),
                    specs,
                    chain,
                ))
            };
        }
    }

    // `select.filter` is the *whole* `WHERE`, and `plan.residuals` is what the
    // access paths did not consume. Testing both re-tests every predicate the
    // planner turned into a seek or a range - `WHERE key BETWEEN ?1 AND ?1+200`
    // was evaluated once per row of a range whose bounds already excluded
    // everything outside it - so only the residuals are tested here. That is
    // also what the bytecode VM does, and it is not merely a speed question: a
    // predicate with `random()` in it would answer differently the second time.
    //
    // The operator chain in `Shape::operators` is what showed this: it printed
    // `RANGE tree 3 -> FILTER -> AGGREGATE` and the `FILTER` had nothing to do.
    if let Some(constant) = &plan.constant_filter {
        let translated = translate_scan(constant, &space, params)?;
        chain = Box::new(Filter::new(compile(&translated, &scan_types)?, chain));
        operators.push("FILTER CONSTANT".to_string());
    }
    for residual in plan.residuals.iter().flatten() {
        let translated = translate_scan(residual, &space, params)?;
        chain = Box::new(Filter::new(compile(&translated, &scan_types)?, chain));
        operators.push("FILTER RESIDUAL".to_string());
    }

    // The inner stages, innermost first, so each ends up above the one before
    // it in the chain the source pushes into. The chain widens from `'static`
    // to `'t` here and only here: an index nested loop borrows its inner tree,
    // and it wraps everything built so far rather than being wrapped by it.
    let mut chain: Box<dyn Sink + 't> = chain;
    for index in (1..prepared.stages.len()).rev() {
        let stage = prepared
            .stages
            .get(index)
            .ok_or_else(|| misuse("a stage vanished while building"))?;
        chain = build_nested(plan, catalog, &space, params, stage, index, chain)?;
        operators.push(format!(
            "{} tree {}{}",
            stage.kind.describe(),
            stage.root,
            if stage.is_lookup {
                " (rowid lookup)"
            } else {
                ""
            }
        ));
    }

    let names = select
        .columns
        .iter()
        .map(|column| column.name.clone())
        .collect();

    Ok(Chain {
        head: chain,
        operators,
        names,
        limit,
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
    /// The pool the source's pages live in.
    pool: &'t Pool,
    /// The statement's constant `LIMIT`, which the source may use.
    limit: Option<usize>,
    /// What the statement produces.
    shape: Shape,
    /// Whether anything but the source read a parameter while building.
    rebindable: bool,
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
    /// @param params - the values bound to `?1`, `?2`, ...
    pub fn run(&mut self, params: &Params) -> DbResult<()> {
        if !self.rebindable {
            return Err(misuse(
                "this statement folded a parameter into its operator chain and cannot be re-run                  against different values",
            ));
        }
        let source = {
            let space = self.held.view(&self.prepared.stages);
            source_for(
                self.plan,
                self.catalog,
                &space,
                params,
                &self.prepared,
                self.limit,
            )?
            .0
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
    let prepared = prepared.clone();
    let held = space_of(catalog, &prepared)?;
    // The reads the chain makes are the parameters it bakes in. The source's
    // are made after this window closes and are recomputed on every execution,
    // so they do not count against re-running.
    let before = params.reads();
    let chain = {
        let space = held.view(&prepared.stages);
        build_chain(plan, catalog, &prepared, &space, params, sink)?
    };
    let rebindable = params.reads() == before;
    let mut operators = chain.operators;
    operators.push(describe_source(&prepared));
    operators.reverse();
    let names = chain.names;
    Ok(Statement {
        plan,
        catalog,
        prepared,
        held,
        head: chain.head,
        pool: catalog.pool(),
        limit: chain.limit,
        shape: Shape { names, operators },
        rebindable,
    })
}

/// Returns what drives a pipeline, and the line `EXPLAIN` prints for it.
///
/// The one place that decides, so the three callers - a one-shot run, a reused
/// statement's rebuild, and a statement's construction - cannot disagree about a
/// plan with no stages.
///
/// @param plan - the planner's output
/// @param catalog - where the trees come from
/// @param space - the joined column space
/// @param params - the bound parameters
/// @param prepared - the structural choices `prepare` made
/// @param limit - the statement's `LIMIT`, when it has a constant one
fn source_for<'t>(
    plan: &PhysicalPlan,
    catalog: &'t dyn TreeCatalog,
    space: &Space<'_>,
    params: &Params,
    prepared: &Prepared,
    limit: Option<usize>,
) -> DbResult<(Source<'t>, String)> {
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
                let rows = catalog
                    .virtual_rows(&term.table, &term.path, params)?
                    .ok_or_else(|| misuse("a virtual table the caller does not have"))?;
                return Ok((Source::Rows(rows), describe_source(prepared)));
            }
            let AccessPath::Subquery { plan: inner, .. } = &term.path else {
                return Err(misuse("a materialised stage over something else"));
            };
            let sub = prepare(inner, catalog, ForcePlan::default())?;
            let (rows, _) = run_prepared(inner, catalog, &sub, params)?;
            Ok((Source::Rows(rows), describe_source(prepared)))
        }
        Some(stage) => Ok((
            build_source(plan, catalog, space, params, stage, limit)?,
            describe_source(prepared),
        )),
        // A `VALUES` arm has no FROM term either, and its rows *are* its
        // answer: every expression is a constant, so they are evaluated once
        // here rather than projected out of an empty row.
        None if !plan.select.values.is_empty() => {
            let empty = Space {
                stages: &[],
                layouts: &[],
                types: &[],
                order: &[],
            };
            let mut rows: Vec<Vec<OwnedDatum>> = Vec::with_capacity(plan.select.values.len());
            for row in &plan.select.values {
                let mut out = Vec::with_capacity(row.len());
                for expr in row {
                    out.push(constant_value(expr, &empty, params, None)?);
                }
                rows.push(out);
            }
            Ok((Source::Rows(rows), "SCAN VALUES".to_string()))
        }
        // A query with no FROM term: one row of no columns, and the whole
        // answer comes out of the projection.
        None => Ok((Source::Constant(1), describe_source(prepared))),
    }
}

/// Returns the `EXPLAIN` line for whatever drives a plan.
///
/// @param prepared - the structural choices `prepare` made
fn describe_source(prepared: &Prepared) -> String {
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
            let high = bounds.high;
            Ok(Source::Reverse(ReverseScan::new(
                tree, projection, high, limit,
            )))
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

/// Builds one inner stage as an index nested loop join.
///
/// @param plan - the planner's output
/// @param catalog - where the trees come from
/// @param space - the joined column space
/// @param params - the bound parameters
/// @param stage - the inner stage
/// @param index - the stage's position
/// @param downstream - what to push joined rows into
#[allow(clippy::too_many_arguments)]
fn build_nested<'t>(
    plan: &PhysicalPlan,
    catalog: &'t dyn TreeCatalog,
    space: &Space<'_>,
    params: &Params,
    stage: &PreparedStage,
    index: usize,
    downstream: Box<dyn Sink + 't>,
) -> DbResult<Box<dyn Sink + 't>> {
    let tree = catalog
        .tree(stage.root)
        .ok_or_else(|| misuse(format!("no tree imported for root page {}", stage.root)))?;
    let outer_types: Vec<StaticType> = space
        .types
        .get(..stage.offset)
        .map(<[StaticType]>::to_vec)
        .unwrap_or_default();
    let (keys, full_key) = if stage.is_lookup {
        // The rowid the index entry carries, which is the previous stage's
        // rowid column.
        let previous = space
            .stages
            .get(index.saturating_sub(1))
            .ok_or_else(|| misuse("a rowid lookup with no index stage before it"))?;
        let previous_layout = space
            .layouts
            .get(index.saturating_sub(1))
            .ok_or_else(|| misuse("a rowid lookup with no layout before it"))?;
        let rowid = previous_layout
            .rowid
            .ok_or_else(|| misuse("the index entry carries no rowid to look the row up by"))?;
        (
            vec![Expr::Column(previous.offset.saturating_add(rowid))],
            true,
        )
    } else {
        let source_term = plan
            .sources
            .get(stage.term)
            .ok_or_else(|| misuse("a stage names a FROM term the plan does not have"))?;
        nested_key(&source_term.path, &source_term.table, space, params)?
    };
    let compiled = keys
        .iter()
        .map(|expr| compile(expr, &outer_types))
        .collect::<DbResult<Vec<_>>>()?;
    Ok(Box::new(IndexNestedLoopJoin::new(
        JoinKind::Inner,
        tree,
        catalog.pool(),
        compiled,
        Projection::all(stage.width),
        full_key,
        downstream,
    )))
}

/// Returns the key expressions an inner stage probes with.
///
/// @param path - the FROM term's access path
/// @param space - the joined column space
/// @param params - the bound parameters
fn nested_key(
    path: &AccessPath,
    table: &inillucent_sql::catalog_view::TableInfo,
    space: &Space<'_>,
    params: &Params,
) -> DbResult<(Vec<Expr>, bool)> {
    match path {
        AccessPath::RowidSeek { key, .. } => Ok((
            vec![with_affinity(
                translate_scan(key, space, params)?,
                Some(Affinity::Integer),
            )],
            true,
        )),
        AccessPath::IndexSeek {
            equalities,
            low,
            high,
            columns,
            ..
        } => {
            if equalities.is_empty() {
                // An index seek with a bound but no equality is a *range* over
                // the inner tree, not a cross product - reading it as one would
                // drop the bound and pair every row with every row.
                return unsupported("a join whose inner index seek has no equality");
            }
            if low.is_some() || high.is_some() {
                return unsupported("a join whose inner index seek also has a range");
            }
            let mut keys = Vec::with_capacity(equalities.len());
            for (position, expr) in equalities.iter().enumerate() {
                keys.push(with_affinity(
                    translate_scan(expr, space, params)?,
                    index_affinity(table, columns, position),
                ));
            }
            // A prefix of the index key, so the probe is a range over every
            // entry sharing it.
            Ok((keys, false))
        }
        // A table scan as an inner term is a cross product, and the join reads
        // an empty key list as exactly that.
        AccessPath::TableScan { .. } => Ok((Vec::new(), false)),
        _ => unsupported("that inner access path in a join"),
    }
}

/// Returns the key a point probe looks up.
///
/// @param path - the FROM term's access path
/// @param space - the joined column space
/// @param params - the bound parameters
fn point_key(path: &AccessPath, space: &Space<'_>, params: &Params) -> DbResult<Vec<OwnedDatum>> {
    match path {
        AccessPath::RowidSeek { key, .. } => Ok(vec![constant_value(
            key,
            space,
            params,
            Some(Affinity::Integer),
        )?]),
        _ => unsupported("a point probe over that access path"),
    }
}

/// The bounds of a range scan, with the inclusivity of each end.
///
/// A struct rather than a tuple because a bare `(low, high, inclusive)` is what
/// hid the bug: the single `inclusive` was the *high* bound's, and the low
/// bound was applied inclusively whatever the predicate said. `WHERE id > 495`
/// returned `id >= 495`.
#[derive(Clone, Debug, Default)]
pub struct SpanBounds {
    /// The lower bound, or `None` for the start of the tree.
    pub low: Option<Vec<OwnedDatum>>,
    /// Whether a key equal to the lower bound is in the range.
    pub low_inclusive: bool,
    /// The upper bound, or `None` for the end of the tree.
    pub high: Option<Vec<OwnedDatum>>,
    /// Whether a key equal to the upper bound is in the range.
    pub high_inclusive: bool,
}

/// Returns the affinity of one column of an index key.
///
/// The index's `columns` list says which table column each key position holds,
/// and the table says what that column's affinity is. A position past the end
/// of the list - the rowid at the end of an entry - is an integer.
///
/// @param table - the indexed table
/// @param columns - which table column each index position holds
/// @param position - the key position
fn index_affinity(
    table: &inillucent_sql::catalog_view::TableInfo,
    columns: &[u16],
    position: usize,
) -> Option<Affinity> {
    match columns.get(position) {
        Some(column) => table
            .columns
            .get(usize::from(*column))
            .map(|info| info.affinity),
        None => Some(Affinity::Integer),
    }
}

/// Returns the bounds of a range scan.
///
/// @param path - the FROM term's access path
/// @param space - the joined column space
/// @param params - the bound parameters
fn span_bounds(
    path: &AccessPath,
    table: &inillucent_sql::catalog_view::TableInfo,
    space: &Space<'_>,
    params: &Params,
) -> DbResult<SpanBounds> {
    match path {
        AccessPath::TableScan { .. } => Ok(SpanBounds {
            low: None,
            low_inclusive: true,
            high: None,
            high_inclusive: true,
        }),
        AccessPath::RowidRange { low, high, .. } => {
            // A rowid range compares against the rowid, which is an integer.
            let (low_value, low_inclusive) =
                bound_value(low.as_ref(), space, params, Some(Affinity::Integer))?;
            let (high_value, high_inclusive) =
                bound_value(high.as_ref(), space, params, Some(Affinity::Integer))?;
            Ok(SpanBounds {
                low: low_value.map(|value| vec![value]),
                low_inclusive,
                high: high_value.map(|value| vec![value]),
                high_inclusive,
            })
        }
        AccessPath::IndexSeek {
            equalities,
            low,
            high,
            columns,
            ..
        } => {
            let mut prefix = Vec::with_capacity(equalities.len());
            for (position, expr) in equalities.iter().enumerate() {
                prefix.push(constant_value(
                    expr,
                    space,
                    params,
                    index_affinity(table, columns, position),
                )?);
            }
            // The range is on the column after the equality prefix.
            let range_affinity = index_affinity(table, columns, equalities.len());
            let (low_value, low_inclusive) =
                bound_value(low.as_ref(), space, params, range_affinity)?;
            let (high_value, high_inclusive) =
                bound_value(high.as_ref(), space, params, range_affinity)?;
            let mut low_key = prefix.clone();
            let mut high_key = prefix;
            if low_value.is_none() && high_value.is_none() {
                if low_key.is_empty() {
                    return Ok(SpanBounds {
                        low: None,
                        low_inclusive: true,
                        high: None,
                        high_inclusive: true,
                    });
                }
                // An equality prefix with no range is the run of every entry
                // sharing it, so both ends are the prefix and both inclusive.
                return Ok(SpanBounds {
                    low: Some(low_key),
                    low_inclusive: true,
                    high: Some(high_key),
                    high_inclusive: true,
                });
            }
            if let Some(value) = low_value {
                low_key.push(value);
            }
            if let Some(value) = high_value {
                high_key.push(value);
            }
            Ok(SpanBounds {
                low: if low_key.is_empty() {
                    None
                } else {
                    Some(low_key)
                },
                low_inclusive,
                high: if high_key.is_empty() {
                    None
                } else {
                    Some(high_key)
                },
                high_inclusive,
            })
        }
        _ => unsupported("a range over that access path"),
    }
}

/// Returns one range bound's value and whether it is inclusive.
///
/// @param bound - the bound, when there is one
/// @param space - the joined column space
/// @param params - the bound parameters
fn bound_value(
    bound: Option<&RangeBound>,
    space: &Space<'_>,
    params: &Params,
    affinity: Option<Affinity>,
) -> DbResult<(Option<OwnedDatum>, bool)> {
    let Some(bound) = bound else {
        return Ok((None, true));
    };
    let value = constant_value(&bound.value, space, params, affinity)?;
    let inclusive = matches!(bound.kind, BoundKind::GreaterEqual | BoundKind::LessEqual);
    Ok((Some(value), inclusive))
}

/// Evaluates an expression that must not read any column.
///
/// A bound, a seek key and a `LIMIT` are all "known before the scan starts", and
/// an expression that reads a column is not - so one is refused here rather
/// than evaluated against whatever row happened to be current.
///
/// @param expr - the bound expression
/// @param space - the joined column space
/// @param params - the bound parameters
/// Returns the value an expression that reads no column folds to.
///
/// For a caller outside a pipeline - a `VALUES` row handed to a virtual table's
/// module, which has no scan behind it and no columns to read.
///
/// @param expr - the bound expression
/// @param params - the values bound to `?1`, `?2`, ...
pub fn literal_value(expr: &BoundExpr, params: &Params) -> DbResult<OwnedDatum> {
    let empty = Space {
        stages: &[],
        layouts: &[],
        types: &[],
        order: &[],
    };
    constant_value(expr, &empty, params, None)
}

/// Returns the value a constant expression folds to.
///
/// @param expr - the bound expression
/// @param space - the joined column space
/// @param params - the bound parameters
/// @param affinity - the affinity a comparison would apply
fn constant_value(
    expr: &BoundExpr,
    space: &Space<'_>,
    params: &Params,
    affinity: Option<Affinity>,
) -> DbResult<OwnedDatum> {
    let translated = translate_scan(expr, space, params)?;
    let value = fold(&translated)
        .ok_or_else(|| misuse("a seek key or range bound reads a column, which it may not"))?;
    // A seek key is one side of a comparison and takes the comparison's
    // affinity like any other. `WHERE id = '4'` against an `INTEGER PRIMARY
    // KEY` finds row 4 in SQLite, because the text is converted before the
    // rowid is compared - and a probe that descended for the *text* `'4'`
    // found nothing at all. The predicate path already applied this; the seek
    // path did not, and the two disagreeing is worse than either being wrong.
    let Some(affinity) = affinity else {
        return Ok(value);
    };
    let borrowed = value.borrow();
    let converted = inillucent_value::affinity::apply_affinity(
        crate::scalar::to_value(borrowed),
        affinity,
        inillucent_value::encoding::TextEncoding::Utf8,
    )
    .unwrap_or(inillucent_value::value::Value::Null);
    Ok(crate::scalar::from_value(converted))
}

/// Wraps a key expression so an affinity is applied before it is compared.
///
/// A join's inner probe evaluates its key once per outer row, so the conversion
/// cannot be folded away the way a constant seek key's can.
///
/// @param expr - the translated key expression
/// @param affinity - the affinity to apply, if any
fn with_affinity(expr: Expr, affinity: Option<Affinity>) -> Expr {
    match affinity {
        None => expr,
        Some(affinity) => Expr::Cast {
            operand: Box::new(expr),
            affinity,
        },
    }
}

/// Folds a constant expression to a value, or returns `None` if it reads a
/// column.
///
/// @param expr - the translated expression
fn fold(expr: &Expr) -> Option<OwnedDatum> {
    match expr {
        Expr::Literal(value) => Some(value.clone()),
        Expr::Arith(op, left, right) => {
            let left = fold(left)?;
            let right = fold(right)?;
            let (a, b) = (left.borrow(), right.borrow());
            match (a.as_int(), b.as_int()) {
                (Some(a), Some(b)) => Some(OwnedDatum::Int(match op {
                    ArithOp::Add => a.wrapping_add(b),
                    ArithOp::Subtract => a.wrapping_sub(b),
                    ArithOp::Multiply => a.wrapping_mul(b),
                })),
                _ => {
                    let a = a.as_f64()?;
                    let b = b.as_f64()?;
                    Some(OwnedDatum::Real(match op {
                        ArithOp::Add => a + b,
                        ArithOp::Subtract => a - b,
                        ArithOp::Multiply => a * b,
                    }))
                }
            }
        }
        // A unary operator over a constant, which is what a negative literal
        // is: `WHERE id = -3` and `LIMIT -1` both bind as a negation of a
        // literal rather than as a literal, and both were refused as "reads a
        // column" - a message that named the wrong thing entirely.
        //
        // The value comes from `inillucent_scalar::eval`, which is where the
        // executor's own unary operators get theirs. A second implementation
        // here would agree with that one until the first time somebody fixed a
        // rounding rule in one of them.
        Expr::Unary { op, operand } => {
            let value = crate::scalar::to_value(fold(operand)?.borrow());
            let answer = match op {
                UnaryOp::Negate => inillucent_scalar::eval::negate(&value),
                UnaryOp::Identity => value,
                UnaryOp::BitNot => inillucent_scalar::eval::bit_not(&value),
                UnaryOp::Not => inillucent_scalar::eval::logical_not(&value),
            };
            Some(crate::scalar::from_value(answer))
        }
        _ => None,
    }
}

/// Reports whether a query is the shape a skip scan answers.
///
/// Every condition is load bearing:
///
/// - `DISTINCT` with no aggregation, because a skip scan produces one row per
///   distinct prefix and nothing else;
/// - no `WHERE`, because a skipped row might have been the one that passed it;
/// - one stage, because a skip scan produces representatives rather than rows
///   and a join over representatives is not the query;
/// - the projected columns are exactly a prefix of the tree's key order,
///   because that is what makes "one row per distinct value" the same set as
///   the query's;
/// - every ordering term ascending, so the rows the seek produces are the
///   answer in the order asked for.
///
/// @param plan - the planner's output
/// @param catalog - where the layouts come from
/// @param root - the tree the outermost stage reads
fn skip_scan_applies(plan: &PhysicalPlan, catalog: &dyn TreeCatalog, root: u32) -> DbResult<bool> {
    let select = &plan.select;
    if !select.distinct
        || plan.aggregation != AggregationMode::None
        || select.filter.is_some()
        || plan.constant_filter.is_some()
        || plan.residuals.iter().any(Option::is_some)
        || select.limit.is_some()
        || plan.sources.len() != 1
        || select
            .order_by
            .iter()
            .any(|term| term.order == SortOrder::Descending)
    {
        return Ok(false);
    }
    let Some(layout) = catalog.layout(root) else {
        return Ok(false);
    };
    // The projected columns must be exactly the leading key columns.
    if select.columns.len() > layout.key_columns.len() {
        return Ok(false);
    }
    for (position, column) in select.columns.iter().enumerate() {
        let slot = match &column.expr {
            BoundExpr::Column { slot, .. } => *slot as usize,
            _ => return Ok(false),
        };
        let tree_column = match layout.slots.get(slot) {
            Some(Some(tree_column)) => *tree_column,
            _ => return Ok(false),
        };
        if layout.key_columns.get(position) != Some(&tree_column) {
            return Ok(false);
        }
    }
    Ok(!select.columns.is_empty())
}

/// Returns how many leading key columns a skip scan produces.
///
/// @param plan - the planner's output
/// @param space - the joined column space
/// @param params - the bound parameters
fn projected_prefix(plan: &PhysicalPlan, space: &Space<'_>, params: &Params) -> DbResult<usize> {
    let _ = (space, params);
    Ok(plan.select.columns.len())
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
    let collected = rows.borrow().clone();
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
        return run_windowed(plan, catalog, params);
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

/// Runs a query with window functions.
///
/// **A window pass is a sort, a buffer and an append.** The rows arrive sorted
/// by the window's partition keys and its `ORDER BY`, [`crate::window::compute`]
/// appends one value per call to each row, and the statement's result columns
/// are then projected out of the widened row. That is what the operator is
/// written to expect - it addresses everything by buffered column number - so
/// what this function does is decide the column numbers.
///
/// ## Why the input rows come off an ordinary plan
///
/// The sort and the scan below a window pass are not special. Building an inner
/// `SELECT` whose result columns are exactly the values the pass needs, and
/// whose `ORDER BY` is the window's own, means the input comes off the *read
/// path* - index selection, an ordering the tree already provides, and the
/// merge over written-to leaves all included - rather than off a second scan
/// written here that would have to be kept in step with it.
///
/// ## What it refuses, and why those are the honest boundaries
///
/// Every call has to share one `PARTITION BY` **and** one `ORDER BY`, because
/// the operator computes each call's peer groups over a sequence it assumes is
/// sorted by that call's ordering, and one buffer can only be sorted one way.
/// Two different windows are two passes and two sorts; that is a real feature,
/// and it is refused by name rather than answered wrongly.
///
/// A window beside an aggregate is refused for a related reason: the pass would
/// have to run over the *grouped* rows, and the grouped rows are a different
/// space from the scan's.
///
/// @param plan - the planner's output
/// @param catalog - where the trees and layouts come from
/// @param params - the values bound to `?1`, `?2`, ...
pub fn run_windowed(
    plan: &PhysicalPlan,
    catalog: &dyn TreeCatalog,
    params: &Params,
) -> DbResult<(Vec<Vec<OwnedDatum>>, Shape)> {
    let select = &plan.select;
    if !select.aggregates.is_empty() || !select.group_by.is_empty() {
        return unsupported("a window function beside an aggregate");
    }
    if !select.compounds.is_empty() {
        return unsupported("a window function in a compound arm");
    }
    let Some(first) = select.windows.first() else {
        return unsupported("a window pass with no window in it");
    };
    for window in &select.windows {
        if window.partition_by != first.partition_by || window.order_by != first.order_by {
            return unsupported("two windows with different PARTITION BY or ORDER BY");
        }
    }

    let pre = window_inputs(select, first);
    let rows = window_input_rows(select, &pre, first, catalog, params)?;
    let pass = window_plan(select, &pre, first)?;
    let widened = crate::window::compute(&rows, &pass)?;
    project_over_window(select, &pre, pre.len(), widened, params)
}

/// Returns the values a window pass reads, in buffered-row order.
///
/// The partition keys and the window's ordering come first, because the inner
/// query is ordered by them and reading them out of the same columns it sorted
/// by is one fewer thing to keep in step. Everything else is appended as it is
/// met, deduplicated, so a value used twice occupies one column.
///
/// @param select - the bound statement
/// @param window - the window every call shares
fn window_inputs(select: &BoundSelect, window: &BoundWindow) -> Vec<BoundExpr> {
    let mut pre: Vec<BoundExpr> = Vec::new();
    for expr in &window.partition_by {
        remember(&mut pre, expr);
    }
    for term in &window.order_by {
        remember(&mut pre, &term.expr);
    }
    for call in &select.windows {
        for expr in &call.arguments {
            remember(&mut pre, expr);
        }
        if let Some(filter) = &call.filter {
            remember(&mut pre, filter);
        }
        for bound in [&call.start, &call.end] {
            if let BoundFrameBound::Preceding(expr) | BoundFrameBound::Following(expr) = bound {
                remember(&mut pre, expr);
            }
        }
    }
    // Every leaf the statement's own expressions read, so the projection above
    // the pass has somewhere to read them from.
    for column in &select.columns {
        gather_leaves(&column.expr, &mut pre);
    }
    for term in &select.order_by {
        gather_leaves(&term.expr, &mut pre);
    }
    pre
}

/// Adds one expression to the buffered row if it is not already there.
///
/// @param pre - the buffered row's expressions
/// @param expr - the expression to carry
fn remember(pre: &mut Vec<BoundExpr>, expr: &BoundExpr) {
    if !pre.iter().any(|held| held == expr) {
        pre.push(expr.clone());
    }
}

/// Adds every column and rowid an expression reads to the buffered row.
///
/// A window reference is a leaf and is deliberately *not* gathered: it names a
/// value the pass is about to compute, which does not exist in its input.
///
/// @param expr - the expression to walk
/// @param pre - the buffered row's expressions
fn gather_leaves(expr: &BoundExpr, pre: &mut Vec<BoundExpr>) {
    match expr {
        BoundExpr::WindowRef { .. } => {}
        BoundExpr::Column { .. } | BoundExpr::Rowid { .. } => remember(pre, expr),
        other => {
            for child in other.children() {
                gather_leaves(child, pre);
            }
        }
    }
}

/// Returns the rows a window pass runs over, already sorted.
///
/// @param select - the bound statement
/// @param pre - the buffered row's expressions
/// @param window - the window every call shares
/// @param catalog - where the trees and layouts come from
/// @param params - the bound parameters
fn window_input_rows(
    select: &BoundSelect,
    pre: &[BoundExpr],
    window: &BoundWindow,
    catalog: &dyn TreeCatalog,
    params: &Params,
) -> DbResult<Vec<Vec<OwnedDatum>>> {
    let mut inner = select.clone();
    inner.columns = pre
        .iter()
        .map(|expr| BoundResultColumn {
            expr: expr.clone(),
            name: b"w".to_vec(),
            origin: None,
            declared_type: Vec::new(),
        })
        .collect();
    inner.windows.clear();
    inner.distinct = false;
    inner.limit = None;
    inner.offset = None;
    inner.having = None;
    // The pass's own ordering: the partition keys, then the window's `ORDER BY`.
    // A partition key only has to bring a partition's rows together, so its
    // direction is free; the ordering terms are the window's own and are not.
    inner.order_by = window
        .partition_by
        .iter()
        .map(|expr| BoundOrderTerm {
            expr: expr.clone(),
            order: SortOrder::Ascending,
            nulls: NullOrder::First,
            collation: expression_collation(expr),
        })
        .chain(window.order_by.iter().cloned())
        .collect();
    let planned = plan_select_with(inner, Levers::default());
    let prepared = prepare(&planned, catalog, ForcePlan::default())?;
    Ok(run_prepared(&planned, catalog, &prepared, params)?.0)
}

/// Builds the window pass, addressing everything by buffered column.
///
/// @param select - the bound statement
/// @param pre - the buffered row's expressions
/// @param window - the window every call shares
fn window_plan(
    select: &BoundSelect,
    pre: &[BoundExpr],
    window: &BoundWindow,
) -> DbResult<WindowPlan> {
    let partition = window
        .partition_by
        .iter()
        .map(|expr| Ok((column_of(pre, expr)?, expression_collation(expr))))
        .collect::<DbResult<Vec<(usize, Collation)>>>()?;
    let order = window
        .order_by
        .iter()
        .map(|term| {
            Ok(WindowOrderTerm {
                column: column_of(pre, &term.expr)?,
                descending: term.order == SortOrder::Descending,
                collation: term.collation,
            })
        })
        .collect::<DbResult<Vec<WindowOrderTerm>>>()?;
    let mut calls = Vec::with_capacity(select.windows.len());
    for call in &select.windows {
        let func = match call.call {
            BoundWindowCall::Plain(plain) => WindowSlot::Plain(plain),
            BoundWindowCall::Aggregate(aggregate) => WindowSlot::Aggregate(match aggregate {
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
                other => return unsupported(&format!("the aggregate {other:?} over a window")),
            }),
        };
        let arguments = call
            .arguments
            .iter()
            .map(|expr| column_of(pre, expr))
            .collect::<DbResult<Vec<usize>>>()?;
        let filter = match &call.filter {
            Some(expr) => Some(column_of(pre, expr)?),
            None => None,
        };
        calls.push(WindowCall {
            func,
            distinct: call.distinct,
            collation: call.collation,
            arguments,
            filter,
            order: order.clone(),
            frame: WindowFrame {
                unit: call.unit,
                start: frame_end(pre, &call.start)?,
                end: frame_end(pre, &call.end)?,
                exclude: call.exclude,
            },
        });
    }
    Ok(WindowPlan { partition, calls })
}

/// Returns one frame end, with its offset resolved to a buffered column.
///
/// @param pre - the buffered row's expressions
/// @param bound - the written bound
fn frame_end(pre: &[BoundExpr], bound: &BoundFrameBound) -> DbResult<FrameEnd> {
    Ok(match bound {
        BoundFrameBound::UnboundedPreceding => FrameEnd::UnboundedPreceding,
        BoundFrameBound::CurrentRow => FrameEnd::CurrentRow,
        BoundFrameBound::UnboundedFollowing => FrameEnd::UnboundedFollowing,
        BoundFrameBound::Preceding(expr) => FrameEnd::Offset {
            column: column_of(pre, expr)?,
            preceding: true,
        },
        BoundFrameBound::Following(expr) => FrameEnd::Offset {
            column: column_of(pre, expr)?,
            preceding: false,
        },
    })
}

/// Returns which buffered column holds one of the pass's inputs.
///
/// Every input was put there by [`window_inputs`], so a miss is a disagreement
/// between that function and this one rather than a query the engine cannot
/// answer - and it says so, because the two are a pair that has to stay in step.
///
/// @param pre - the buffered row's expressions
/// @param expr - the expression to find
fn column_of(pre: &[BoundExpr], expr: &BoundExpr) -> DbResult<usize> {
    pre.iter()
        .position(|held| held == expr)
        .ok_or_else(|| misuse("a window pass did not carry a value its own plan reads"))
}

/// Projects a statement's result columns out of the widened rows.
///
/// **The ordering happens below the projection, not above it.** Under
/// [`Frame::Window`] a translated expression addresses the *widened* row - the
/// values the pass was given, then one per call - so a sort built from those
/// expressions has to run while the batch is still that row. Putting it above
/// the projection sorted by whichever output column happened to share the
/// index, which is a wrong answer rather than an error: `ORDER BY id` sorted by
/// the window value instead and the rows came back in the pass's input order.
///
/// An ordinal or an alias is the one term that genuinely names an *output*
/// column, and it is resolved by translating that column's own expression
/// rather than by moving the sort - so both kinds of term end up addressing the
/// same row.
///
/// @param select - the bound statement
/// @param pre - the buffered row's expressions
/// @param width - how many columns the buffered row had before the pass
/// @param rows - the widened rows
/// @param params - the bound parameters
fn project_over_window(
    select: &BoundSelect,
    pre: &[BoundExpr],
    width: usize,
    rows: Vec<Vec<OwnedDatum>>,
    params: &Params,
) -> DbResult<(Vec<Vec<OwnedDatum>>, Shape)> {
    let held = HeldSpace {
        layouts: Vec::new(),
        types: Vec::new(),
        order: Vec::new(),
    };
    let space = held.view(&[]);
    let frame = Frame::Window { pre, width };
    let types = vec![StaticType::Unknown; width.saturating_add(select.windows.len())];
    let mut projected = Vec::with_capacity(select.columns.len());
    for column in &select.columns {
        let translated = translate(&column.expr, &space, params, frame)?;
        projected.push(compile(&translated, &types)?);
    }
    let mut keys = Vec::with_capacity(select.order_by.len());
    for term in &select.order_by {
        // An ordinal or an alias names an output column, so what it orders by
        // is that column's expression - which reads the widened row like every
        // other term here.
        let expr = match &term.expr {
            BoundExpr::SorterColumn { column } => select
                .columns
                .get(usize::from(*column))
                .map(|held| &held.expr)
                .ok_or_else(|| misuse("an ORDER BY ordinal outside the result list"))?,
            other => other,
        };
        let translated = translate(expr, &space, params, frame)?;
        let Expr::Column(column) = translated else {
            return unsupported("a windowed query ordered by a computed expression");
        };
        let descending = term.order == SortOrder::Descending;
        keys.push(SortKey {
            column,
            descending,
            collation: term.collation,
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
    let mut head: Box<dyn Sink> = match (limit, offset) {
        (None, None) => collect,
        (limit, offset) => Box::new(Limit::new(
            limit.unwrap_or(usize::MAX),
            offset.unwrap_or(0),
            collect,
        )),
    };
    if select.distinct {
        head = Box::new(Distinct::new(distinct_collations(select), head));
    }
    head = Box::new(Project::new(projected, head));
    if !keys.is_empty() {
        head = Box::new(Sort::new(keys, head));
    }
    ValuesScan::new(rows).run(head.as_mut())?;
    let answer = collected.borrow().clone();
    Ok((
        answer,
        Shape {
            names: select
                .columns
                .iter()
                .map(|column| column.name.clone())
                .collect(),
            operators: vec![
                "SCAN".to_string(),
                "SORT".to_string(),
                "WINDOW".to_string(),
                "PROJECT".to_string(),
            ],
        },
    ))
}

/// Returns the collation each result column is compared under by `DISTINCT`.
///
/// @param select - the bound statement
fn distinct_collations(select: &BoundSelect) -> Vec<Collation> {
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
/// How a bound expression's leaves are resolved.
///
/// The same expression means different things above and below an aggregate: a
/// `GROUP BY` key is a scan column on one side of the operator and output column
/// zero on the other. Making that a *parameter* of one traversal rather than two
/// traversals is what keeps the two from drifting - which they had, by twenty-odd
/// node kinds.
#[derive(Clone, Copy)]
enum Frame<'a> {
    /// Reading the scan's own columns.
    Scan,
    /// Reading the row an aggregate emitted: the keys, then the accumulators.
    Post {
        /// The bound statement, for the aggregate and `GROUP BY` lists.
        select: &'a BoundSelect,
        /// How many `GROUP BY` keys precede the accumulators.
        group_width: usize,
    },
    /// Reading the row a window pass emitted: the values it was given, then one
    /// per call in the order they were bound.
    ///
    /// The third frame, and the reason the traversal takes one rather than
    /// being written three times: a window's output space differs from the
    /// scan's in exactly the same way an aggregate's does - only at the leaves.
    Window {
        /// The expressions the buffered row holds, one per column.
        pre: &'a [BoundExpr],
        /// How many of those precede the appended window values.
        width: usize,
    },
}

/// Translates a bound expression that reads the scan's columns.
///
/// @param expr - the bound expression
/// @param space - the joined column space
/// @param params - the bound parameters
pub(crate) fn translate_scan(
    expr: &BoundExpr,
    space: &Space<'_>,
    params: &Params,
) -> DbResult<Expr> {
    translate(expr, space, params, Frame::Scan)
}

fn translate(
    expr: &BoundExpr,
    space: &Space<'_>,
    params: &Params,
    frame: Frame<'_>,
) -> DbResult<Expr> {
    // The leaves, which are the only thing the two frames disagree about. Every
    // node below this point recurses with the same frame, which is what makes
    // this one traversal rather than two that have to be kept in step - and
    // keeping them in step is exactly what failed: the post-aggregation copy
    // handled six node kinds and refused the rest, so `length(group_concat(x))`
    // was "a function call outside an aggregate" and `HAVING` had nowhere to be
    // translated at all.
    if let Frame::Window { pre, width } = frame {
        if let BoundExpr::WindowRef { slot } = expr {
            return Ok(Expr::Column(width.saturating_add(*slot)));
        }
        // A whole sub-expression the pass already computed, which is how a
        // window's own argument resolves without being recomputed.
        if let Some(position) = pre.iter().position(|held| held == expr) {
            return Ok(Expr::Column(position));
        }
        if matches!(expr, BoundExpr::Column { .. } | BoundExpr::Rowid { .. }) {
            return unsupported("a column a window pass did not carry");
        }
    }
    if let Frame::Post {
        select,
        group_width,
    } = frame
    {
        if let BoundExpr::Aggregate { slot } = expr {
            return Ok(Expr::Column(group_width.saturating_add(*slot)));
        }
        if let Some(position) = select.group_by.iter().position(|key| key == expr) {
            return Ok(Expr::Column(position));
        }
        // A column read outside an aggregate in a grouped query is what SQLite
        // calls a "bare column", and it answers with an arbitrary row of the
        // group. Refusing is the honest thing to do rather than picking one.
        if matches!(expr, BoundExpr::Column { .. } | BoundExpr::Rowid { .. }) {
            return unsupported(&format!(
                "the expression {} outside an aggregate",
                name_of(expr)
            ));
        }
    }
    Ok(match expr {
        BoundExpr::Null => Expr::Literal(OwnedDatum::Null),
        BoundExpr::Integer(number) => Expr::Literal(OwnedDatum::Int(*number)),
        BoundExpr::Real(number) => Expr::Literal(OwnedDatum::Real(*number)),
        BoundExpr::Text(bytes) => Expr::Literal(OwnedDatum::Text(bytes.clone())),
        BoundExpr::Blob(bytes) => Expr::Literal(OwnedDatum::Blob(bytes.clone())),
        BoundExpr::Parameter(index) => Expr::Literal(params.get(*index)),
        BoundExpr::Column { source, slot, .. } => {
            let index = space.column(*source, *slot as usize).ok_or_else(|| {
                misuse(format!(
                    "the tree read for FROM term {source} does not carry record slot {slot}"
                ))
            })?;
            Expr::Column(index)
        }
        BoundExpr::Rowid { source } => Expr::Column(
            space
                .rowid(*source)
                .ok_or_else(|| misuse("the tree read does not carry a rowid"))?,
        ),
        // "Column n of the row at this point", which is what the binder gives a
        // `VALUES` arm's result columns and an `ORDER BY` written as an
        // ordinal. It is already an index rather than a name, so there is
        // nothing to resolve.
        BoundExpr::SorterColumn { column } => Expr::Column(usize::from(*column)),
        BoundExpr::Not(operand) => Expr::Not(Box::new(translate(operand, space, params, frame)?)),
        BoundExpr::IsNull { operand, negated } => {
            let inner = Box::new(translate(operand, space, params, frame)?);
            if *negated {
                Expr::IsNotNull(inner)
            } else {
                Expr::IsNull(inner)
            }
        }
        BoundExpr::And(left, right) => Expr::And(
            Box::new(translate(left, space, params, frame)?),
            Box::new(translate(right, space, params, frame)?),
        ),
        BoundExpr::Or(left, right) => Expr::Or(
            Box::new(translate(left, space, params, frame)?),
            Box::new(translate(right, space, params, frame)?),
        ),
        BoundExpr::Arithmetic { op, left, right } => {
            let left = Box::new(translate(left, space, params, frame)?);
            let right = Box::new(translate(right, space, params, frame)?);
            // `+`, `-` and `*` have a specialised integer node; everything else
            // - divide, modulo, concatenation, the bitwise operators - goes
            // through the shared implementation.
            match arith_op(*op) {
                Ok(op) => Expr::Arith(op, left, right),
                Err(_) => Expr::General {
                    op: *op,
                    left,
                    right,
                },
            }
        }
        BoundExpr::Compare {
            op,
            left,
            right,
            affinity,
            collation,
        } => {
            // Affinity conversion before comparison and a non-BINARY
            // collation both change the answer, so an executor that ignored
            // them would be quietly wrong rather than incomplete. The plain
            // form is kept for the case where there is nothing to apply,
            // because it is the fast path and most comparisons are it.
            let op = compare_op(*op)?;
            let left = Box::new(translate(left, space, params, frame)?);
            let right = Box::new(translate(right, space, params, frame)?);
            if affinity.is_none() && *collation == inillucent_value::collation::Collation::Binary {
                Expr::Compare(op, left, right)
            } else {
                Expr::CompareWith {
                    op,
                    affinity: *affinity,
                    collation: *collation,
                    left,
                    right,
                }
            }
        }
        BoundExpr::Unary { op, operand } => Expr::Unary {
            op: *op,
            operand: Box::new(translate(operand, space, params, frame)?),
        },
        BoundExpr::Cast { operand, affinity } => Expr::Cast {
            operand: Box::new(translate(operand, space, params, frame)?),
            affinity: *affinity,
        },
        BoundExpr::Collate { operand, .. } => translate(operand, space, params, frame)?,
        BoundExpr::Is {
            negated,
            left,
            right,
            affinity,
            collation,
        } => Expr::Is {
            negated: *negated,
            left: Box::new(translate(left, space, params, frame)?),
            right: Box::new(translate(right, space, params, frame)?),
            affinity: *affinity,
            collation: *collation,
        },
        BoundExpr::Between {
            negated,
            operand,
            low,
            high,
            affinity,
            collation,
        } => Expr::Between {
            negated: *negated,
            operand: Box::new(translate(operand, space, params, frame)?),
            low: Box::new(translate(low, space, params, frame)?),
            high: Box::new(translate(high, space, params, frame)?),
            affinity: *affinity,
            collation: *collation,
        },
        BoundExpr::InList {
            negated,
            operand,
            list,
            affinity,
            collation,
        } => Expr::InList {
            negated: *negated,
            operand: Box::new(translate(operand, space, params, frame)?),
            list: list
                .iter()
                .map(|expr| translate(expr, space, params, frame))
                .collect::<DbResult<Vec<Expr>>>()?,
            affinity: *affinity,
            collation: *collation,
        },
        BoundExpr::Case {
            operand,
            branches,
            otherwise,
            collation,
        } => {
            let mut translated = Vec::with_capacity(branches.len());
            for (when, then) in branches {
                translated.push((
                    translate(when, space, params, frame)?,
                    translate(then, space, params, frame)?,
                ));
            }
            Expr::Case {
                operand: match operand {
                    Some(operand) => Some(Box::new(translate(operand, space, params, frame)?)),
                    None => None,
                },
                branches: translated,
                otherwise: match otherwise {
                    Some(otherwise) => Some(Box::new(translate(otherwise, space, params, frame)?)),
                    None => None,
                },
                collation: *collation,
            }
        }
        BoundExpr::Pattern {
            negated,
            op,
            operand,
            pattern,
            escape,
        } => {
            let kind = match op {
                PatternOp::Like => crate::scalar::PatternKind::Like,
                PatternOp::Glob => crate::scalar::PatternKind::Glob,
                // `REGEXP` and `MATCH` are not built in: SQLite leaves them to
                // an application-defined function or a module, and a query that
                // uses one without registering it is an error rather than a
                // false.
                other => return unsupported(&format!("the {other:?} operator")),
            };
            Expr::Pattern {
                negated: *negated,
                kind,
                operand: Box::new(translate(operand, space, params, frame)?),
                pattern: Box::new(translate(pattern, space, params, frame)?),
                escape: match escape {
                    Some(escape) => Some(Box::new(translate(escape, space, params, frame)?)),
                    None => None,
                },
            }
        }
        BoundExpr::Json { func, arguments } => Expr::Json {
            func: *func,
            arguments: arguments
                .iter()
                .map(|expr| translate(expr, space, params, frame))
                .collect::<DbResult<Vec<Expr>>>()?,
        },
        BoundExpr::Function {
            func,
            arguments,
            collation,
        } => {
            // `length` keeps its specialised node: it reads the leaf's bytes in
            // place where the general path copies them into a `Value` first,
            // and `range.lookaside` calls it once per row.
            let translated = arguments
                .iter()
                .map(|expr| translate(expr, space, params, frame))
                .collect::<DbResult<Vec<Expr>>>()?;
            if *func == ScalarFunc::Length && translated.len() == 1 {
                match translated.into_iter().next() {
                    Some(only) => Expr::Length(Box::new(only)),
                    None => return unsupported("length with no argument"),
                }
            } else {
                Expr::Call {
                    func: *func,
                    arguments: translated,
                    collation: *collation,
                }
            }
        }
        BoundExpr::Math { func, arguments } => Expr::Math {
            func: *func,
            arguments: arguments
                .iter()
                .map(|expr| translate(expr, space, params, frame))
                .collect::<DbResult<Vec<Expr>>>()?,
        },
        BoundExpr::Time { func, arguments } => Expr::Time {
            func: *func,
            arguments: arguments
                .iter()
                .map(|expr| translate(expr, space, params, frame))
                .collect::<DbResult<Vec<Expr>>>()?,
            // Every `now` in one statement is the same instant, which is
            // SQLite's rule and the reason this is read once here rather than
            // per row in the node.
            now: inillucent_scalar::datetime::julian_now(),
        },
        other => return unsupported(&format!("the expression {}", name_of(other))),
    })
}

/// Translates a bound expression in the space after aggregation.
///
/// A result column of an aggregating query reads either a `GROUP BY` key or an
/// accumulator, and both are columns of the row the aggregate operator emits:
/// the keys first, then the accumulators.
///
/// It is [`translate`] with a different frame rather than a second traversal,
/// and that is the point: the old copy handled six node kinds and refused
/// everything else, so `SELECT length(group_concat(name)) ... GROUP BY team` was
/// "a function call outside an aggregate" - a refusal about the *shape* of a
/// query the engine can perfectly well answer.
///
/// @param expr - the bound expression
/// @param select - the bound statement, for the aggregate list
/// @param space - the joined column space
/// @param params - the bound parameters
/// @param group_width - how many `GROUP BY` keys precede the accumulators
fn translate_post(
    expr: &BoundExpr,
    select: &BoundSelect,
    space: &Space<'_>,
    params: &Params,
    group_width: usize,
) -> DbResult<Expr> {
    if select.aggregates.is_empty() {
        return translate_scan(expr, space, params);
    }
    translate(
        expr,
        space,
        params,
        Frame::Post {
            select,
            group_width,
        },
    )
}

/// Returns the static type of each column an aggregate operator emits.
///
/// @param select - the bound statement
/// @param space - the joined column space
/// @param params - the bound parameters
fn aggregate_output_types(
    select: &BoundSelect,
    space: &Space<'_>,
    params: &Params,
) -> DbResult<Vec<StaticType>> {
    let mut types = Vec::with_capacity(
        select
            .group_by
            .len()
            .saturating_add(select.aggregates.len()),
    );
    for key in &select.group_by {
        let translated = translate_scan(key, space, params)?;
        types.push(static_type_of(&translated, &space.types));
    }
    for call in &select.aggregates {
        // `count` is always an integer; the rest depend on their input and on
        // whether a sum overflowed, so nothing is claimed about them.
        types.push(match call.func {
            AggregateFunc::Count => StaticType::Int,
            AggregateFunc::Total | AggregateFunc::Avg => StaticType::Real,
            _ => StaticType::Unknown,
        });
    }
    Ok(types)
}

/// Returns the static type an expression produces.
///
/// @param expr - the translated expression
/// @param types - the input columns' types
fn static_type_of(expr: &Expr, types: &[StaticType]) -> StaticType {
    match expr {
        Expr::Column(index) => types.get(*index).copied().unwrap_or(StaticType::Unknown),
        Expr::Literal(OwnedDatum::Int(_)) => StaticType::Int,
        Expr::Literal(OwnedDatum::Real(_)) => StaticType::Real,
        Expr::Literal(OwnedDatum::Text(_)) => StaticType::Text,
        _ => StaticType::Unknown,
    }
}

/// Builds the accumulator specifications for an aggregating query.
///
/// @param select - the bound statement
/// @param space - the joined column space
/// @param params - the bound parameters
/// @param types - the scan's column types
fn aggregate_specs(
    select: &BoundSelect,
    space: &Space<'_>,
    params: &Params,
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
                let translated = translate_scan(expr, space, params)?;
                Some(compile(&translated, types)?)
            }
        };
        // `count(DISTINCT x)` compares its values under the collation `x`
        // carries, which is the same rule `SELECT DISTINCT x` follows - and
        // over the corpus's `NOCASE` team column the two have to agree.
        let distinct = if call.distinct {
            Some(
                call.arguments
                    .first()
                    .map(expression_collation)
                    .unwrap_or(Collation::Binary),
            )
        } else {
            None
        };
        specs.push(AggregateSpec {
            kind,
            argument,
            distinct,
        });
    }
    Ok(specs)
}

/// Returns a projection that keeps the first `width` columns.
///
/// @param width - how many columns the statement's result has
/// @param types - the input columns' types
fn trim(width: usize, types: &[StaticType]) -> DbResult<Vec<Box<dyn crate::expr::Eval>>> {
    (0..width)
        .map(|index| compile(&Expr::Column(index), types))
        .collect()
}

/// Returns the statement's `LIMIT`, when it is a constant.
///
/// @param select - the bound statement
/// @param params - the bound parameters
fn constant_limit(select: &BoundSelect, params: &Params) -> DbResult<Option<usize>> {
    constant_count(select.limit.as_ref(), params, Negative::NoLimit)
}

/// Returns the statement's `OFFSET`, when it is a constant.
///
/// @param select - the bound statement
/// @param params - the bound parameters
fn constant_offset(select: &BoundSelect, params: &Params) -> DbResult<Option<usize>> {
    constant_count(select.offset.as_ref(), params, Negative::Zero)
}

/// What a negative `LIMIT` or `OFFSET` means.
///
/// **They mean different things and the difference is a wrong answer.** A
/// negative `LIMIT` is SQLite's way of saying "no limit"; a negative `OFFSET` is
/// treated as zero. The first version of this clamped both to zero, which turned
/// `LIMIT -1` into `LIMIT 0` - every row suppressed - and would have done the
/// same to a parameter somebody bound to -1.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Negative {
    /// A negative count means there is no limit.
    NoLimit,
    /// A negative count means zero.
    Zero,
}

/// Returns a `LIMIT`/`OFFSET` expression's value.
///
/// @param expr - the expression, when there is one
/// @param params - the bound parameters
/// @param negative - what a negative value means for this clause
fn constant_count(
    expr: Option<&BoundExpr>,
    params: &Params,
    negative: Negative,
) -> DbResult<Option<usize>> {
    let number = match expr {
        None => return Ok(None),
        Some(BoundExpr::Integer(number)) => *number,
        // A negative literal binds as a negation of a literal rather than as a
        // literal, which is why `LIMIT -1` was refused as "not a constant".
        Some(BoundExpr::Unary {
            op: UnaryOp::Negate,
            operand,
        }) => match operand.as_ref() {
            BoundExpr::Integer(number) => number.saturating_neg(),
            _ => return unsupported("a LIMIT or OFFSET that is not a constant"),
        },
        Some(BoundExpr::Parameter(index)) => match params.get(*index) {
            OwnedDatum::Int(number) => number,
            OwnedDatum::Null => return Ok(None),
            _ => return unsupported("a LIMIT bound to a non-integer"),
        },
        Some(_) => return unsupported("a LIMIT or OFFSET that is not a constant"),
    };
    if number < 0 {
        return Ok(match negative {
            Negative::NoLimit => None,
            Negative::Zero => Some(0),
        });
    }
    Ok(Some(number as usize))
}

/// Reports whether two translated expressions are the same expression.
///
/// @param left - one expression
/// @param right - the other
fn same_expr(left: &Expr, right: &Expr) -> bool {
    match (left, right) {
        (Expr::Column(a), Expr::Column(b)) => a == b,
        (Expr::Literal(a), Expr::Literal(b)) => {
            a.borrow().compare(&b.borrow()) == std::cmp::Ordering::Equal
        }
        (Expr::Arith(a, al, ar), Expr::Arith(b, bl, br)) => {
            a == b && same_expr(al, bl) && same_expr(ar, br)
        }
        (Expr::Compare(a, al, ar), Expr::Compare(b, bl, br)) => {
            a == b && same_expr(al, bl) && same_expr(ar, br)
        }
        (
            Expr::CompareWith {
                op: a,
                affinity: aa,
                collation: ac,
                left: al,
                right: ar,
            },
            Expr::CompareWith {
                op: b,
                affinity: ba,
                collation: bc,
                left: bl,
                right: br,
            },
        ) => a == b && aa == ba && ac == bc && same_expr(al, bl) && same_expr(ar, br),
        _ => false,
    }
}

/// Maps a bound arithmetic operator onto the compiler's.
///
/// @param op - the planner's operator
fn arith_op(op: BinaryOp) -> DbResult<ArithOp> {
    match op {
        BinaryOp::Add => Ok(ArithOp::Add),
        BinaryOp::Subtract => Ok(ArithOp::Subtract),
        BinaryOp::Multiply => Ok(ArithOp::Multiply),
        other => unsupported(&format!("the operator {other:?}")),
    }
}

/// Maps a bound comparison onto the compiler's.
///
/// @param op - the planner's operator
fn compare_op(op: BinaryOp) -> DbResult<CompareOp> {
    match op {
        BinaryOp::Equal => Ok(CompareOp::Equal),
        BinaryOp::NotEqual => Ok(CompareOp::NotEqual),
        BinaryOp::Less => Ok(CompareOp::Less),
        BinaryOp::LessEqual => Ok(CompareOp::LessOrEqual),
        BinaryOp::Greater => Ok(CompareOp::Greater),
        BinaryOp::GreaterEqual => Ok(CompareOp::GreaterOrEqual),
        other => unsupported(&format!("the comparison {other:?}")),
    }
}

/// Returns a bound expression's variant name, for a refusal message.
///
/// @param expr - the expression
fn name_of(expr: &BoundExpr) -> &'static str {
    match expr {
        BoundExpr::Null => "NULL",
        BoundExpr::Integer(_) => "an integer literal",
        BoundExpr::Real(_) => "a real literal",
        BoundExpr::Text(_) => "a text literal",
        BoundExpr::Blob(_) => "a blob literal",
        BoundExpr::Parameter(_) => "a parameter",
        BoundExpr::Column { .. } => "a column",
        BoundExpr::Rowid { .. } => "a rowid",
        BoundExpr::Unary { .. } => "a unary operator",
        BoundExpr::Arithmetic { .. } => "an arithmetic operator",
        BoundExpr::Compare { .. } => "a comparison",
        BoundExpr::And(_, _) => "AND",
        BoundExpr::Or(_, _) => "OR",
        BoundExpr::Not(_) => "NOT",
        BoundExpr::IsNull { .. } => "IS NULL",
        BoundExpr::Aggregate { .. } => "an aggregate",
        BoundExpr::Function { .. } => "a function call",
        BoundExpr::External { .. } => "an application-defined function",
        // Everything else answers with its own variant name rather than with
        // "an expression". A refusal a reader cannot act on is a refusal that
        // costs a debugging session, and the first run of the Phase 2 gate
        // spent one on exactly this line.
        other => {
            let rendered = format!("{other:?}");
            let name = rendered
                .split(|c: char| !c.is_alphanumeric())
                .next()
                .unwrap_or("an expression");
            return Box::leak(format!("a {name} expression").into_boxed_str());
        }
    }
}

/// Returns the flow a sink reports, for the `Flow` re-export.
///
/// Kept so that a caller of this module does not have to reach into
/// [`crate::ops`] for the one type a custom sink needs.
pub fn continue_flow() -> Flow {
    Flow::Continue
}
