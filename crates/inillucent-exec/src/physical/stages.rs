//! Turning a bound statement into the stages a pipeline runs.
//!
//! Invariant: **every term of the `FROM` clause becomes exactly one stage, and
//! a term this executor cannot run is refused here rather than further down.**
//! `refuse_unhandled` is what makes the refusal arrive as `unsupported` with
//! the construct named, instead of as a wrong answer from a plan that quietly
//! dropped a clause.

use inillucent_base::error::misuse;
use inillucent_base::DbResult;
// `literal_value` is named by path from a dozen call sites in
// `inillucent-engine`, so it stays reachable here after the move to `constant`.
// `Compiled`, `Slot` and `try_compile` moved to `crate::compiled` to keep this
// file under its recorded ceiling; re-exported here so every existing
// `physical::Slot` / `physical::Compiled` / `physical::try_compile` reference
// - `inillucent-engine`'s `Cached::Select` among them - did not have to move
// with them.
use inillucent_pool::Pool;
use inillucent_sql::bind::{BoundExpr, BoundSelect};
use inillucent_sql::catalog_view::TableInfo;
use inillucent_sql::function::AggregateFunc;
use inillucent_sql::plan::{AccessPath, PhysicalPlan, PlannedSource};
use inillucent_tree::datum::{Datum, OwnedDatum};
use inillucent_value::collation::Collation;

use crate::batch::Batch;
use crate::expr::StaticType;
use crate::ops::{CollectInto, Sink};
use crate::paged::{FullScan, PointProbe, ReverseScan, SkipScan, SpanScan};

/// A catalog that also answers one recursive CTE's queue.
///
/// Everything else is delegated, so the step arm sees exactly the trees, the
/// layouts and the modules the statement sees. Wrapping rather than threading a
/// parameter through every builder is what keeps a recursive query from
/// changing the shape of a signature nothing else uses.
use super::*;

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
    /// Several rows by key, several probes of the same tree concatenated -
    /// what an `IN` list becomes once the planner turns it into seeks.
    SeekUnion,
    /// Several key ranges over the same tree, concatenated in the order that
    /// keeps their combined output in the composite key's order - what a
    /// keyset page's `OR`-shaped tuple comparison becomes.
    RangeUnion,
    /// A probe of this tree once per row of the stage before it.
    Nested,
    /// The rows an index a module owns named, read out of the tree by rowid.
    ///
    /// The tree is the table's, so the layout and every column slot are the
    /// ordinary ones; what is different is *which* rows and in what order -
    /// the module chose them, and the plan's own `ORDER BY` then rescores them.
    Vector,
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
            AccessKind::Vector => "VECTOR SEARCH",
            AccessKind::Span => "RANGE",
            AccessKind::Reverse => "RANGE REVERSE",
            AccessKind::Skip => "SKIP SCAN",
            AccessKind::Point => "POINT PROBE",
            AccessKind::SeekUnion => "SEEK UNION",
            AccessKind::RangeUnion => "RANGE UNION",
            AccessKind::Nested => "INDEX NESTED LOOP",
            AccessKind::Materialised => "SCAN SUBQUERY",
        }
    }
}
/// One stage's physical choice.
#[derive(Clone, Debug)]
pub struct PreparedStage {
    /// The module's auxiliary functions this stage materialises, in slot order.
    ///
    /// Empty for everything but a virtual scan. Their answers sit after the
    /// declared columns and the rowid, so a query reading `score(t)` finds it
    /// at a column the module filled rather than at an expression the pipeline
    /// has no way to evaluate.
    pub functions: Vec<(Vec<u8>, Vec<inillucent_sql::bind::BoundExpr>)>,
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
    pub layout: Option<std::rc::Rc<SourceLayout>>,
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
    /// The pool the source's pages live in, when the source reads a tree.
    ///
    /// `None` for a source that is already rows - a materialised subquery, a
    /// module's answer, a recursive queue, `VALUES`, or a query with no FROM
    /// term. Those read no page, so there is no file they belong to, and
    /// handing them some other schema's pool to ignore would be a lie the type
    /// could not catch.
    pub pool: Option<&'t Pool>,
}
impl Pipeline<'_> {
    /// Drives the pipeline to completion.
    pub fn run(&mut self) -> DbResult<()> {
        self.source.run(self.pool, self.head.as_mut())
    }
}
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
    /// Several rows by key, several probes of the same tree - what
    /// `AccessKind::SeekUnion` runs. Every key has already been evaluated and
    /// de-duplicated (a literal repeat folded at plan time, a repeat only
    /// visible at run time folded here, against the concrete values this
    /// execution actually bound), so every probe here is worth making.
    SeekUnion(PointProbe<'t>, Vec<Vec<OwnedDatum>>),
    /// Several key ranges over the same tree, concatenated in the order that
    /// keeps their combined output in the composite key's order - what
    /// `AccessKind::RangeUnion` runs.
    RangeUnion(Vec<SpanScan<'t>>),
    /// The rows an index a module owns named, by rowid, in its order.
    Vector(PointProbe<'t>, Vec<i64>),
    /// Rows a nested query produced, already materialised.
    Rows(Vec<Vec<OwnedDatum>>),
    /// A module's scan, driven a batch at a time and abandoned on `Flow::Stop`.
    ///
    /// The catalog rather than the rows, because the rows do not exist yet -
    /// that is the whole point. `generate_series` with no `stop` constraint is
    /// 4,294,967,295 rows, and materialising it before the `LIMIT` above it runs
    /// is a query that does not return.
    Virtual(Box<VirtualScanSource<'t>>),
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
/// Everything a module's scan needs, kept so it can be driven at run time.
///
/// Boxed inside [`Source::Virtual`] because it is the largest variant by a wide
/// margin and every other source is a handful of words; a `Source` that grew to
/// the size of a `TableInfo` would be copied around the hot read path for the
/// benefit of the one arm that reads a virtual table.
pub struct VirtualScanSource<'t> {
    /// Where the module is resolved from.
    pub catalog: &'t dyn TreeCatalog,
    /// The FROM term's table, which names the module's instance.
    pub table: TableInfo,
    /// The access path the planner chose, carrying the pushed-down offer.
    pub path: AccessPath,
    /// The values this execution bound.
    pub params: Params,
    /// Which of the term's columns the query reads.
    pub needed: inillucent_sql::bind::ColumnUse,
}
/// Reads one catalog's `case_sensitive_like`, as a function `is_some_and` takes.
///
/// @param catalog - the catalog the statement is compiled against
pub(crate) fn inillucent_exec_like_case_sensitive(catalog: &dyn TreeCatalog) -> bool {
    catalog.like_is_case_sensitive()
}
/// Returns the pool a source that reads a tree must have been given.
///
/// @param pool - what the pipeline carried
fn needs_pool(pool: Option<&Pool>) -> DbResult<&Pool> {
    pool.ok_or_else(|| misuse("this source reads a tree no attached database holds"))
}
impl Source<'_> {
    /// Drives the source until the pipeline is done.
    ///
    /// @param pool - the pool the source's tree lives in, when it reads one
    /// @param downstream - the head of the operator chain
    pub fn run(&self, pool: Option<&Pool>, downstream: &mut dyn Sink) -> DbResult<()> {
        // **Asked for by the arms that read pages, and by no others.** `Rows`
        // and `Constant` are already materialised, so a pool would be a
        // parameter they ignore; a source that does read a tree with no pool
        // behind it is a plan naming a schema this connection does not hold,
        // which is a refusal rather than a page read out of the wrong file.
        match self {
            Source::Scan(scan) => scan.run(needs_pool(pool)?, downstream),
            Source::Span(scan) => scan.run(needs_pool(pool)?, downstream),
            Source::Reverse(scan) => scan.run(needs_pool(pool)?, downstream),
            Source::Skip(scan) => scan.run(needs_pool(pool)?, downstream),
            Source::Point(probe, key) => {
                let pool = needs_pool(pool)?;
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
            Source::SeekUnion(probe, keys) => {
                let pool = needs_pool(pool)?;
                // One descent per key, exactly what running each branch's own
                // `RowidSeek`/`IndexSeek` in turn would cost - and a list long
                // enough to make that expensive is a list the planner already
                // prices against a scan and loses.
                let mut buffer: Vec<OwnedDatum> = Vec::new();
                let mut rows: Vec<Vec<OwnedDatum>> = Vec::with_capacity(keys.len());
                for key in keys {
                    let borrowed: Vec<Datum<'_>> = key.iter().map(OwnedDatum::borrow).collect();
                    if probe.lookup(pool, &borrowed, &mut buffer)? {
                        rows.push(buffer.clone());
                    }
                }
                crate::ops::emit_rows(&rows, downstream)?;
                downstream.finish()
            }
            Source::RangeUnion(branches) => {
                let pool = needs_pool(pool)?;
                // Each branch is collected rather than streamed straight to
                // `downstream`: `SpanScan::run` finishes its sink when it
                // returns, and finishing `downstream` after the first branch
                // would tell it the whole union was done. Collecting loses a
                // downstream `LIMIT`'s ability to stop the *later* branches
                // early, which is the one thing this costs next to the
                // bytecode engine's branch-by-branch loop - the branches
                // still only cover the keyset page's own range, never the
                // whole table, so the seek this replaces a scan with is not
                // what the cost was paid for.
                let collected = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
                let mut sink = crate::ops::CollectInto::new(std::rc::Rc::clone(&collected));
                for branch in branches {
                    branch.run(pool, &mut sink)?;
                }
                let rows = collected.borrow().clone();
                crate::ops::emit_rows(&rows, downstream)?;
                downstream.finish()
            }
            Source::Vector(probe, keys) => {
                let pool = needs_pool(pool)?;
                // One descent per candidate, and the candidates are already the
                // few the index chose - so this is `k` probes rather than a
                // scan, which is the whole point of the path.
                let mut buffer: Vec<OwnedDatum> = Vec::new();
                let mut rows: Vec<Vec<OwnedDatum>> = Vec::with_capacity(keys.len());
                for key in keys {
                    if probe.lookup(pool, &[Datum::Int(*key)], &mut buffer)? {
                        rows.push(buffer.clone());
                    }
                }
                crate::ops::emit_rows(&rows, downstream)?;
                downstream.finish()
            }
            Source::Rows(rows) => {
                crate::ops::emit_rows(rows, downstream)?;
                downstream.finish()
            }
            Source::Virtual(scan) => {
                scan.catalog.virtual_cursor(
                    &scan.table,
                    &scan.path,
                    &scan.params,
                    &scan.needed,
                    downstream,
                )?;
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
            Source::SeekUnion(_, _) => "SEEK UNION",
            Source::RangeUnion(_) => "RANGE UNION",
            Source::Vector(_, _) => "VECTOR SEARCH",
            Source::Rows(_) => "SCAN SUBQUERY",
            Source::Virtual(_) => "SCAN VIRTUAL TABLE",
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
pub(crate) fn unsupported<T>(what: &str) -> DbResult<T> {
    // **The sentence and the marker are written in the same place**, so a
    // caller asking `DbError::unsupported()` and a caller reading the message
    // cannot be told different things. The wording is unchanged from before
    // the marker existed, because assertions elsewhere quote it.
    let said = format!("the new engine's physical pass does not handle {what} yet");
    // The sentence is the message *and* the detail. `misuse` attaches what it is
    // given as detail alone, which left every refusal answering `message()` with
    // "bad parameter or other API misuse"; the detail is kept so that every
    // existing reader of it is unaffected.
    Err(misuse(said.clone())
        .with_message(said)
        .with_unsupported(what))
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
/// The binder's rule, so `GROUP BY`, `DISTINCT` and `PARTITION BY` group
/// with the collation `ORDER BY` sorts with and a comparison compares with.
/// This used to be its own copy that looked only at the top node, so
/// `SELECT DISTINCT s COLLATE NOCASE || '' FROM t` kept `a` and `A` apart
/// where 3.53.4 counts them as one value (task-2089).
///
/// @param expr - the bound expression
pub(crate) fn expression_collation(expr: &BoundExpr) -> Collation {
    inillucent_sql::bind::result_collation(expr)
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
/// One FROM term, and everything that decides what stage it becomes.
///
/// **A gathered context rather than a seven argument call (task-1962, A8).**
/// Every function the `plan_stages` split produced reads the same six things,
/// and four of them are `usize`, `bool` and `Option<u32>` - the shape a caller
/// gets wrong silently.
struct Term<'a> {
    /// The planner's output.
    plan: &'a PhysicalPlan,
    /// Where the trees and layouts come from.
    catalog: &'a dyn TreeCatalog,
    /// The term itself.
    source: &'a PlannedSource,
    /// Which FROM term it is.
    position: usize,
    /// A covering index to read instead of the outermost table.
    override_root: Option<u32>,
    /// Whether the statement's answer depends on the order rows arrive in.
    sensitive: bool,
}

impl Term<'_> {
    /// Whether this term drives the pipeline rather than being joined into it.
    fn outermost(&self) -> bool {
        self.position == 0
    }

    /// A request to read one tree for this term.
    ///
    /// @param root - the tree to read
    /// @param kind - how to read it
    fn reads(&self, root: u32, kind: AccessKind) -> StageRequest {
        StageRequest {
            root,
            kind,
            source: self.source.id,
            term: self.position,
            is_lookup: false,
        }
    }

    /// A request for the table fetch behind an index read.
    ///
    /// @param root - the table's own tree
    fn looks_up(&self, root: u32) -> StageRequest {
        StageRequest {
            root,
            kind: AccessKind::Nested,
            source: self.source.id,
            term: self.position,
            is_lookup: true,
        }
    }
}

/// One stage to add: which tree, read how, for which FROM term.
///
/// **A request struct rather than eight positional arguments (task-1962, A8 and
/// A9).** `push_stage` took `(stages, catalog, root, kind, source, term,
/// is_lookup, offset)` and carried `#[allow(clippy::too_many_arguments)]` to say
/// so. Three of those were a `u32` and two `usize` in a row, which is the shape
/// a caller gets wrong without the compiler noticing.
struct StageRequest {
    /// The tree this stage reads.
    root: u32,
    /// How it reads it.
    kind: AccessKind,
    /// Which planner FROM term it belongs to.
    source: usize,
    /// That term's position in the FROM list.
    term: usize,
    /// Whether it is the table fetch behind an index seek.
    is_lookup: bool,
}

/// Turns the planner's FROM terms into stages.
///
/// A query with no FROM term produces no stages at all, and that is a legal
/// plan rather than a refusal: `SELECT 1` reads no tree, so there is nothing
/// for a stage to describe. Every function below already loops over the stages
/// rather than indexing the first, except the two that build the source and the
/// space - and both have an empty case.
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
    let mut stages: Vec<PreparedStage> = Vec::new();
    let mut offset = 0usize;
    let sensitive = order_sensitive(&plan.select);
    for (position, source) in plan.sources.iter().enumerate() {
        let term = Term {
            plan,
            catalog,
            source,
            position,
            override_root,
            sensitive,
        };
        stage_for_term(&term, &mut stages, &mut offset)?;
    }
    Ok(stages)
}

/// Builds the stages one FROM term becomes.
///
/// **An outer join is answered by materialising the inner side.**
///
/// It used to be refused, and the refusal was right while it stood: the
/// physical pass never looked at the join kind and always built
/// `JoinKind::Inner`, so a `LEFT JOIN` silently dropped the outer rows that
/// matched nothing - `SELECT people.team FROM people LEFT JOIN teams ON ...`
/// answered six rows as four nulls.
///
/// What it needs that an index nested loop cannot give is the `ON` condition
/// evaluated per candidate *pair*: an index probe assumes the key equality
/// **is** the condition, and an outer join has to know that a pair failed the
/// condition in order to null-extend instead. So the inner side is read once
/// into a buffer and `NestedLoopJoin` evaluates the condition over each pair -
/// which also gives `RIGHT` and `FULL`, because a materialised build side is
/// the only thing that can remember which of its rows matched (see
/// `build_nested`).
///
/// @param term - the FROM term
/// @param stages - the stages built so far
/// @param offset - the next free column index, advanced
fn stage_for_term(
    term: &Term<'_>,
    stages: &mut Vec<PreparedStage>,
    offset: &mut usize,
) -> DbResult<()> {
    if walk_stages(term, stages, offset)?
        || index_stages(term, stages, offset)?
        || materialised_stages(term, stages, offset)?
    {
        return Ok(());
    }
    unsupported("an access path the physical pass builds no stage for")
}

/// Builds the stage for a term that reads one tree and nothing else.
///
/// A scan, a rowid probe, a rowid range, a union of rowid probes and a vector
/// probe. `false` means this term is none of them.
///
/// @param term - the FROM term
/// @param stages - the stages built so far
/// @param offset - the next free column index, advanced
fn walk_stages(
    term: &Term<'_>,
    stages: &mut Vec<PreparedStage>,
    offset: &mut usize,
) -> DbResult<bool> {
    match &term.source.path {
        AccessPath::TableScan { root } => {
            let root = if term.outermost() {
                term.override_root.unwrap_or(*root)
            } else {
                *root
            };
            let kind = if term.outermost() {
                AccessKind::Full
            } else {
                // An inner term with no usable index is a cross product: every
                // inner row pairs with every outer one, and any predicate over
                // the pair is a residual. Phase 2 refused it because the read
                // families never produce one; the corpora do - `SELECT count(*)
                // FROM people CROSS JOIN teams` - and refusing a shape the
                // engine can answer is a gap rather than a policy.
                AccessKind::Nested
            };
            push_stage(stages, term.catalog, term.reads(root, kind), offset)?;
        }
        AccessPath::RowidSeek { root, .. } => {
            let kind = if term.outermost() {
                AccessKind::Point
            } else {
                AccessKind::Nested
            };
            push_stage(stages, term.catalog, term.reads(*root, kind), offset)?;
        }
        AccessPath::RowidRange { root, .. } => {
            let kind = if !term.outermost() {
                return unsupported("a rowid range as an inner join term");
            } else if term.plan.reverse {
                AccessKind::Reverse
            } else {
                AccessKind::Span
            };
            push_stage(stages, term.catalog, term.reads(*root, kind), offset)?;
        }
        // A union of probes composes with a per-row nested loop exactly as a
        // lone probe does - one more of the same thing, at every level - but
        // this engine has no join operator that drives one yet, so it is
        // offered only where it drives the whole pipeline. The bytecode engine
        // does not share this limit: it compiles a union's branches the same
        // way at any level, one loop per branch, which is why the same SQL runs
        // on both engines while only one of them takes the fast path
        // everywhere the planner found one.
        AccessPath::RowidSeekUnion { root, .. } => {
            if !term.outermost() {
                return unsupported("a seek union as an inner join term");
            }
            let request = term.reads(*root, AccessKind::SeekUnion);
            push_stage(stages, term.catalog, request, offset)?;
        }
        // The candidates come from the module, and the rows come out of the
        // table's own tree by rowid - so this is a table stage with an unusual
        // source rather than a materialised one.
        AccessPath::VectorProbe { root, .. } => {
            let request = term.reads(*root, AccessKind::Vector);
            push_stage(stages, term.catalog, request, offset)?;
        }
        _ => return Ok(false),
    }
    Ok(true)
}

/// Builds the stages for a term that reads an index, and maybe the table.
///
/// `false` means this term reads no index.
///
/// @param term - the FROM term
/// @param stages - the stages built so far
/// @param offset - the next free column index, advanced
fn index_stages(
    term: &Term<'_>,
    stages: &mut Vec<PreparedStage>,
    offset: &mut usize,
) -> DbResult<bool> {
    match &term.source.path {
        AccessPath::IndexSeekUnion {
            table_root,
            index_root,
            index_name,
            covering,
            branches,
            ..
        } => {
            if !term.outermost() {
                return unsupported("a seek union as an inner join term");
            }
            // The branches of an `IN` list are bare equalities and probed like
            // `RowidSeekUnion`'s; the branches of a keyset page are ranges, and
            // reconstructing the page's order depends on walking each one and
            // running them in the order they were built in - two different
            // sources for what is, at the plan level, one shape.
            let kind = if probes_one_entry_each(
                &term.source.table,
                index_name,
                *index_root,
                *table_root,
                branches,
            ) {
                AccessKind::SeekUnion
            } else {
                AccessKind::RangeUnion
            };
            push_stage(stages, term.catalog, term.reads(*index_root, kind), offset)?;
            push_lookup(
                term,
                *index_root,
                *table_root,
                covering.is_none(),
                stages,
                offset,
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
            // An index seek with no equality and no bound is a *scan* of the
            // index, not a range over it. The distinction is not cosmetic: the
            // covering rule and the skip-scan rule both key on `Full`, and
            // calling this a range left `scan.distinct` reading every row of
            // the index where SQLite seeks 64 times.
            let unbounded = equalities.is_empty() && low.is_none() && high.is_none();
            // The planner's *own* covering choice is subject to the same rule
            // the physical pass's covering rule is: reading fewer bytes out of
            // an index changes the order the rows reach an aggregate in, and a
            // floating-point sum is not associative. `SELECT sum(score) FROM
            // people` over `people_by_score` is 0.0 where the table gives
            // 124.25, because the corpus holds `-1e300` and `+1e300` and the
            // small values vanish between them. SQLite reads the table here,
            // and so must this.
            if unbounded && covering.is_some() && term.outermost() && term.sensitive {
                let request = term.reads(*table_root, AccessKind::Full);
                push_stage(stages, term.catalog, request, offset)?;
                return Ok(true);
            }
            let kind = if term.outermost() {
                if term.plan.reverse {
                    AccessKind::Reverse
                } else if unbounded {
                    AccessKind::Full
                } else {
                    AccessKind::Span
                }
            } else {
                AccessKind::Nested
            };
            push_stage(stages, term.catalog, term.reads(*index_root, kind), offset)?;
            push_lookup(
                term,
                *index_root,
                *table_root,
                covering.is_none(),
                stages,
                offset,
            )?;
        }
        _ => return Ok(false),
    }
    Ok(true)
}

/// Adds the table fetch behind an index read, when the index is not enough.
///
/// The index does not carry every column the query reads, so the row is
/// fetched from the table by rowid. That is the TDD's `RowidLookup`, expressed
/// as what it is: a nested loop into the table tree keyed on the entry's rowid.
///
/// A `WITHOUT ROWID` table's primary-key index *is* the table: one b-tree,
/// reported at the table's own root page. So it carries every column by
/// construction and there is no rowid to look anything up by - which is exactly
/// what this stage used to try, five times over in the differential corpus,
/// with "the index entry carries no rowid".
///
/// @param term - the FROM term
/// @param index_root - the index's tree
/// @param table_root - the table's own tree
/// @param wanted - whether the index is missing a column the query reads
/// @param stages - the stages built so far
/// @param offset - the next free column index, advanced
fn push_lookup(
    term: &Term<'_>,
    index_root: u32,
    table_root: u32,
    wanted: bool,
    stages: &mut Vec<PreparedStage>,
    offset: &mut usize,
) -> DbResult<()> {
    if !wanted || index_root == table_root {
        return Ok(());
    }
    push_stage(stages, term.catalog, term.looks_up(table_root), offset)
}

/// Builds the stage for a term whose rows are produced rather than walked.
///
/// A derived table, a recursive CTE and the reference to the one being filled,
/// and a virtual table. `false` means this term reads a tree.
///
/// @param term - the FROM term
/// @param stages - the stages built so far
/// @param offset - the next free column index, advanced
fn materialised_stages(
    term: &Term<'_>,
    stages: &mut Vec<PreparedStage>,
    offset: &mut usize,
) -> DbResult<bool> {
    match &term.source.path {
        AccessPath::Subquery {
            width, correlated, ..
        } => {
            // An inner subquery is a nested loop over a materialised buffer
            // rather than over a tree, which is exactly what `build_nested`
            // builds for it. The rows are read once rather than once per outer
            // row: a derived table is a query with no free variables, so
            // re-running it would answer the same thing.
            if *correlated && term.outermost() {
                // A correlated subquery reads a FROM term outside itself, and
                // the outermost term has nothing outside it - so this is a plan
                // that should not exist rather than one to run.
                return unsupported("a correlated subquery as the outermost term");
            }
            push_materialised(stages, term.source.id, term.position, *width, offset);
        }
        // A recursive CTE and the reference to the one being filled are both
        // *materialised* stages: the first is the fill loop's answer and the
        // second is the queue it is currently on, and neither is a tree.
        // `source_for` and `materialise_stage` produce the rows.
        AccessPath::Recursive { width, .. } => {
            push_materialised(stages, term.source.id, term.position, *width, offset);
        }
        AccessPath::RecursiveSelf { .. } => {
            let width = term.source.table.columns.len().max(1);
            push_materialised(stages, term.source.id, term.position, width, offset);
        }
        AccessPath::VirtualScan { .. } => virtual_scan_stage(term, stages, offset),
        _ => return Ok(false),
    }
    Ok(true)
}

/// Builds the stage a virtual table becomes.
///
/// A virtual table is a *materialised* stage: the module produces its rows on
/// the caller's side and the pipeline reads them, which is the same shape a
/// subquery already has. A module is asked once, whether it is the outermost
/// term or an inner one: the plan it chose was chosen for one set of
/// constraints, and asking it again per outer row would be asking a different
/// question than the one it costed. As an inner term its rows drive a
/// `NestedLoopJoin`, like a subquery's.
///
/// **A module's row carries its rowid when the query asks for one.** `SELECT
/// rowid FROM t WHERE t MATCH ...` is the shape every search adapter is written
/// in - the rowid is the answer, and the columns are what was searched - and it
/// used to be refused with "the tree read does not carry a rowid". The module
/// has always had it: `VirtualCursor::rowid` is on the trait. It is appended
/// after the declared columns rather than put first, so every column keeps the
/// slot it already had.
///
/// @param term - the FROM term
/// @param stages - the stages built so far
/// @param offset - the next free column index, advanced
fn virtual_scan_stage(term: &Term<'_>, stages: &mut Vec<PreparedStage>, offset: &mut usize) {
    let declared = term.source.table.columns.len().max(1);
    let read = term.plan.select.columns_read(term.source.id);
    let carries_rowid = read.rowid;
    let functions = read.functions.clone();
    let width = declared
        .saturating_add(usize::from(carries_rowid))
        .saturating_add(functions.len());
    stages.push(PreparedStage {
        functions: functions.clone(),
        root: 0,
        kind: AccessKind::Materialised,
        source: term.source.id,
        term: term.position,
        is_lookup: false,
        offset: *offset,
        width,
        // A module's row is its own record, exactly as a materialised
        // subquery's is: slot `i` is column `i`, and nothing is known about the
        // order.
        layout: Some(std::rc::Rc::new(SourceLayout {
            tree_key: 0,
            slots: (0..declared).map(Some).collect(),
            rowid: carries_rowid.then_some(declared),
            // A module's rows are not a table's rows: there is nothing to probe
            // a table with.
            identity: Vec::new(),
            types: vec![StaticType::Unknown; width],
            width,
            key_columns: Vec::new(),
        })),
    });
    *offset = offset.saturating_add(width);
}
/// Reports whether every branch of a seek union finds at most one entry.
///
/// **A point probe is only right when one entry per key is all there can be
/// (task-1932).** `PointProbe` finds the first entry with a key and stops,
/// which is what a rowid and a unique index guarantee and what no other index
/// does: on a non-unique one an equality is a *run* of entries, and probing it
/// answered one row of the run. `WHERE b IN (1, 2)` returned two rows where
/// `WHERE b = 1` alone returns twenty-one, and the pinned 3.53.4 answers
/// forty-two.
///
/// A branch that is not bare - one carrying a bound as well as its equalities -
/// is a range whatever the index guarantees, so it is not one entry either.
/// Everything this refuses goes to `RangeUnion`, which builds each branch as an
/// `IndexSeek` and takes its span: the same path a plain equality already
/// takes, so there is one definition of what an equality over an index means.
///
/// @param table - the table the union reads
/// @param index_name - the index the union seeks in
/// @param index_root - that index's tree
/// @param table_root - the table's own tree, which is the rowid case
/// @param branches - the union's branches
fn probes_one_entry_each(
    table: &TableInfo,
    index_name: &[u8],
    index_root: u32,
    table_root: u32,
    branches: &[inillucent_sql::plan::IndexSeekBranch],
) -> bool {
    let bare = branches
        .iter()
        .all(|branch| branch.low.is_none() && branch.high.is_none());
    let one_per_key = index_root == table_root
        || table
            .indexes
            .iter()
            .find(|held| held.name == *index_name)
            .is_some_and(|held| {
                held.unique
                    && branches
                        .iter()
                        .all(|branch| branch.equalities.len() >= held.columns.len())
            });
    bare && one_per_key
}
/// Adds one stage and advances the column offset.
///
/// @param stages - the stages built so far
/// @param catalog - where the layouts come from
/// @param request - which tree to read, and how
/// @param offset - the next free column index, advanced
fn push_stage(
    stages: &mut Vec<PreparedStage>,
    catalog: &dyn TreeCatalog,
    request: StageRequest,
    offset: &mut usize,
) -> DbResult<()> {
    let layout = catalog
        .layout(request.root)
        .ok_or_else(|| misuse(format!("no layout imported for root page {}", request.root)))?;
    stages.push(PreparedStage {
        functions: Vec::new(),
        root: request.root,
        kind: request.kind,
        source: request.source,
        term: request.term,
        is_lookup: request.is_lookup,
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
pub(crate) fn refuse_unhandled(select: &BoundSelect) -> DbResult<()> {
    // A window function is not refused here any more: `run_any` routes a
    // windowed query to `run_windowed` before a pipeline is prepared at all,
    // and a window that reached this point would be one nothing routed - which
    // is a bug in the dispatcher rather than a query the engine cannot answer.
    if !select.windows.is_empty() {
        return unsupported("a window function reaching the pipeline builder");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every access kind describes itself, and no two describe themselves the
    /// same way.
    ///
    /// **`EXPLAIN QUERY PLAN` is read by a person deciding whether the planner
    /// did what they asked (T3, task-1962).** Two kinds sharing a description
    /// would make a skip scan and a full scan indistinguishable in the one
    /// place that exists to tell them apart.
    #[test]
    fn every_access_kind_describes_itself_distinctly() {
        let kinds = [
            AccessKind::Full,
            AccessKind::Vector,
            AccessKind::Span,
            AccessKind::Reverse,
            AccessKind::Skip,
            AccessKind::Point,
            AccessKind::SeekUnion,
            AccessKind::RangeUnion,
            AccessKind::Nested,
            AccessKind::Materialised,
        ];
        let mut seen: Vec<&str> = kinds.iter().map(|kind| kind.describe()).collect();
        seen.sort_unstable();
        let mut unique = seen.clone();
        unique.dedup();
        assert_eq!(
            seen, unique,
            "two access kinds describe themselves the same way: {seen:?}"
        );
        assert_eq!(AccessKind::Skip.describe(), "SKIP SCAN");
        assert_eq!(AccessKind::Full.describe(), "SCAN");
        assert_eq!(AccessKind::Materialised.describe(), "SCAN SUBQUERY");
    }

    /// A refusal says what the pass will not run, and says it the same way
    /// twice.
    ///
    /// **The message and the marker are written in one place**, so a caller
    /// asking `DbError::unsupported()` and a caller reading the message cannot
    /// be told different things. They were told different things once: the
    /// wording lived in `format!` and the marker was attached separately.
    #[test]
    fn a_refusal_names_the_construct_in_both_places() {
        let refused: DbResult<()> = unsupported("a window frame this wide");
        let error = refused.expect_err("`unsupported` refuses");
        assert_eq!(
            error.unsupported(),
            Some("a window frame this wide"),
            "the marker carries the construct, for a caller that branches on it"
        );
        assert!(
            error.message().contains("a window frame this wide"),
            "and the message carries it too, for a caller that reads it; it said {:?}",
            error.message()
        );
    }

    /// A shape with no operators is an empty description, not a missing one.
    #[test]
    fn a_shape_carries_its_names_and_its_operators() {
        let shape = Shape {
            names: vec![b"a".to_vec(), b"b".to_vec()],
            operators: vec!["Scan(t)".to_string(), "Filter(a > 1)".to_string()],
        };
        assert_eq!(shape.names.len(), 2);
        assert_eq!(
            shape.operators.first().map(String::as_str),
            Some("Scan(t)"),
            "the source is first, which is the order EXPLAIN prints"
        );
    }
}
