//! A statement's operator chain, held with no lifetime, so an `Rc<Cached>`
//! outliving any one borrow of the connection can keep it between executions.
//!
//! Invariant: nothing in [`Compiled`] borrows the catalog it was built
//! against. [`Compiled::run`] takes the catalog as a call argument rather than
//! a field, and asks it for a tree and a pool fresh on every call - see the
//! struct doc for why that is exactly what a cached statement needs and what
//! the rejected `Rc<PagedTree>` design could not give it.

use inillucent_base::error::misuse;
use inillucent_base::DbResult;
use inillucent_sql::plan::{AccessPath, PhysicalPlan};
use inillucent_tree::datum::OwnedDatum;

use crate::batch::Batch;
use crate::expr::{compile, Eval, Expr, StaticType};
use crate::join::{IndexNestedLoopJoin, JoinKind};
use crate::ops::{CollectInto, Flow, Sink};
use crate::physical::{
    describe_source, has_equi_key, join_kind_of, nested_key, reads_a_column, source_for_run,
    source_pool, space_of, AccessKind, Bindings, HeldSpace, Params, Prepared, PreparedStage, Shape,
    Space, TreeCatalog,
};
use crate::scan::Projection;

/// Forwards to a borrowed sink, so a per-execution join tower can wrap `&mut
/// *self.upper` without owning it.
///
/// [`Compiled::run`] rebuilds the join tower fresh every call - see
/// [`JoinRecipe`] - and the tower's lowest level needs a `Box<dyn Sink + 'a>`
/// to close over, while `upper` is a field `Compiled` owns and only lends for
/// the length of one call. This newtype is what turns the loan into a box.
struct Borrowed<'a>(&'a mut dyn Sink);

impl Sink for Borrowed<'_> {
    fn push(&mut self, batch: &Batch<'_>) -> DbResult<Flow> {
        self.0.push(batch)
    }

    fn finish(&mut self) -> DbResult<()> {
        self.0.finish()
    }

    fn reset(&mut self) -> DbResult<()> {
        self.0.reset()
    }
}

/// What [`crate::physical::build_nested`] computes for one inner stage that
/// does not borrow the catalog - kept across executions so only the borrow
/// itself, `catalog.tree(root)` and `catalog.pool_for(root)`, has to be asked
/// again.
///
/// Only ever built for a stage [`try_join_recipe`] confirmed would become an
/// [`IndexNestedLoopJoin`] - the one join shape that neither bakes rows in at
/// build time (`NestedLoopJoin`, `HashJoin::build_materialised`) nor borrows
/// the catalog for its own lifetime beyond one probe (`Correlated`,
/// `LateralModule`).
struct JoinRecipe {
    /// How unmatched outer rows are treated.
    kind: JoinKind,
    /// The inner tree's root page id.
    root: u32,
    /// The compiled key expressions, over the outer batch's columns.
    ///
    /// Shared rather than cloned: a `Box<dyn Eval>` cannot be cloned, and
    /// every execution's `IndexNestedLoopJoin` needs the same compiled keys,
    /// so they are translated once here and an `Rc` clone - a refcount bump -
    /// is handed to each execution's join instead of a re-translation.
    keys: std::rc::Rc<[Box<dyn Eval>]>,
    /// Which inner columns to emit, in order.
    inner_projection: Projection,
    /// Whether the key is a full inner key (a probe) or a prefix (a range).
    full_key: bool,
}

/// Attempts to describe one inner stage as an [`IndexNestedLoopJoin`], the
/// only join shape a lifetime-free `Compiled` can rebuild per execution.
///
/// Mirrors `build_nested`'s own shape decision - the same three checks, in
/// the same order - so the two can never disagree about which stages are
/// which. Returns `Ok(None)` for exactly the shapes `build_nested` routes
/// elsewhere instead of building this: a table-valued function whose argument
/// reads an outer column (a lateral join, driven per outer row from a
/// catalog borrowed for its own life), an outer join that is not answerable
/// by probe, a stage the plan marked materialised, and a table scan
/// `PRAGMA automatic_index` would key - the last two bake rows in at build
/// time in `build_materialised_join`, which is exactly the staleness roadmap
/// item 3's design review rejected the `Rc<PagedTree>` design over.
///
/// Needs no catalog at all: everything it computes - the key expressions,
/// which columns to project, whether the key is full or a prefix - depends
/// only on the plan and the schema, never on a page.
///
/// @param plan - the planner's output
/// @param space - the joined column space
/// @param params - the values bound to `?1`, `?2`, ...
/// @param stage - the inner stage
/// @param index - the stage's position, which names its residual
fn try_join_recipe(
    plan: &PhysicalPlan,
    space: &Space<'_>,
    params: &Params,
    stage: &PreparedStage,
    index: usize,
) -> DbResult<Option<JoinRecipe>> {
    let source_term = plan
        .sources
        .get(stage.term)
        .ok_or_else(|| misuse("a stage names a FROM term the plan does not have"))?;
    if let AccessPath::VirtualScan { offer, .. } = &source_term.path {
        if offer.iter().any(|held| reads_a_column(&held.value)) {
            return Ok(None);
        }
    }
    let outer_by_probe = inillucent_sql::plan::is_outer(source_term.join)
        && source_term.on_enforced
        && stage.kind != AccessKind::Materialised;
    if (inillucent_sql::plan::is_outer(source_term.join) && !outer_by_probe)
        || stage.kind == AccessKind::Materialised
    {
        return Ok(None);
    }
    let outer_types: Vec<StaticType> = space
        .types
        .get(..stage.offset)
        .map(<[StaticType]>::to_vec)
        .unwrap_or_default();
    let (keys, full_key) = if stage.is_lookup {
        let previous = space
            .stages
            .get(index.saturating_sub(1))
            .ok_or_else(|| misuse("a table lookup with no index stage before it"))?;
        let previous_layout = space
            .layouts
            .get(index.saturating_sub(1))
            .ok_or_else(|| misuse("a table lookup with no layout before it"))?;
        if previous_layout.identity.is_empty() {
            return Err(misuse(
                "the index entry carries nothing to look the table row up by",
            ));
        }
        (
            previous_layout
                .identity
                .iter()
                .map(|slot| Expr::Column(previous.offset.saturating_add(*slot)))
                .collect(),
            true,
        )
    } else {
        nested_key(&source_term.path, &source_term.table, space, params)?
    };
    if keys.is_empty()
        && plan
            .levers
            .has(inillucent_sql::plan::Levers::AUTOMATIC_INDEX)
        && has_equi_key(plan, space, params, stage, index)?
    {
        return Ok(None);
    }
    let compiled: Vec<Box<dyn Eval>> = keys
        .iter()
        .map(|expr| compile(expr, &outer_types))
        .collect::<DbResult<_>>()?;
    Ok(Some(JoinRecipe {
        kind: join_kind_of(source_term.join),
        root: stage.root,
        keys: compiled.into(),
        inner_projection: Projection::all(stage.width),
        full_key,
    }))
}

/// A compiled chain, held with **no lifetime at all**.
///
/// [`crate::physical::Statement`] already holds a chain across executions,
/// and it is not used that way anywhere the engine actually caches a
/// statement, for one reason: `Statement<'t>` borrows the catalog for `'t`,
/// and the engine's connection is the catalog, so a cached `Statement` would
/// have to be a field on something whose every other method takes `&mut
/// self` - a write, an `ATTACH`, a `PRAGMA` - for as long as the cache holds
/// it. `PagedTree` writes go through `&mut self` (see `write.rs`'s
/// `insert`/`delete`/`update_in_place`), so that is not a design that borrows
/// more carefully, it is a design that cannot compile, and reaching for
/// `Rc<RefCell<PagedTree>>` instead trades the borrow checker's refusal for a
/// silent one: a connection `reopen`s or `reattach`es by *replacing* a tree
/// value without emptying the statement cache, so a cached handle onto the
/// old `Rc` would keep answering from the tree that used to be there - a
/// wrong answer with nothing to catch it.
///
/// So `Compiled` takes the catalog as an argument to [`Compiled::run`]
/// instead of a field: nothing here borrows it between executions. What it
/// keeps is `crate::physical::build_upper`'s output - the part of the
/// chain proved to hold no such borrow - plus a `JoinRecipe` per inner
/// stage and enough to rebuild the source fresh every call, the same way
/// `Statement::run` does. `catalog.tree(root)` and `catalog.pool_for(root)`
/// are asked again, for every level, on every [`Compiled::run`], so a write,
/// a rollback or a reopen between two runs is answered from whatever is
/// actually there.
///
/// **Every inner stage must be an index nested loop join.** A statement with
/// a correlated block, a materialised join, or a lateral module anywhere in
/// it is never turned into a `Compiled` at all - see [`try_compile`] - so
/// `joins` holds one `JoinRecipe` per inner stage, always, whenever a
/// `Compiled` exists for a multi-stage plan.
pub struct Compiled {
    /// The structural choices `prepare` made; owned, because nothing here may
    /// borrow the catalog between executions.
    prepared: Prepared,
    /// The layouts and types, computed once.
    held: HeldSpace,
    /// Every operator above the source and every join. Holds no borrow of the
    /// catalog - see the struct doc for why that is exactly what makes this
    /// reusable without an `Rc<PagedTree>`.
    upper: Box<dyn Sink>,
    /// One recipe per inner stage, outermost first - `joins[0]` is stage 1,
    /// `joins[1]` is stage 2, and so on. Rebuilt into a borrowed tower around
    /// `upper` on every [`Compiled::run`], innermost (the last one) wrapped
    /// first, matching the order `build_chain`'s own loop wraps them in.
    joins: Vec<JoinRecipe>,
    /// What the statement produces.
    shape: Shape,
    /// The statement's constant `LIMIT`, which the source may use.
    limit: Option<usize>,
    /// Whether the build that produced this `Compiled` read nothing but the
    /// source's own parameters - see `Statement::rebindable`, decided the
    /// same way. A `Compiled` that answered `false` is never kept: see
    /// [`try_compile`].
    rebindable: bool,
    /// The connection's settings when `upper` was built.
    ///
    /// **`upper` folds them in and nothing counts that as a read (task-2081).**
    /// A `%`, a `/`, a `||` and every scalar call hold the connection's
    /// `Limit::Length` and `LIKE`'s case rule, and those move only when somebody
    /// changes them. So a kept chain is valid for as long as they are what they
    /// were, and [`Compiled::built_under`] is how a caller asks.
    settings: crate::scalar::Context,
    /// The cell every `Expr::Parameter` in `upper` reads. See
    /// `Statement::bindings` for why this has to be shared rather than
    /// re-read.
    bindings: Bindings,
    /// Where `upper`'s `CollectInto` writes the rows, and where
    /// [`Compiled::take_rows`] takes them back out.
    collected: std::rc::Rc<std::cell::RefCell<Vec<Vec<OwnedDatum>>>>,
}

impl Compiled {
    /// Returns what the statement produces.
    pub fn shape(&self) -> &Shape {
        &self.shape
    }

    /// Reports whether this chain answered `true` when it was built - always
    /// `true` for a `Compiled` that exists, since [`try_compile`] discards
    /// every other kind, but kept as a method for the same reason
    /// `Statement::rebindable` is one: a caller asking the question should
    /// not have to know it can only ever hear one answer.
    pub fn rebindable(&self) -> bool {
        self.rebindable
    }

    /// Reports whether this chain was built under the settings an execution has now.
    ///
    /// A chain that answers `false` holds a length limit or a `LIKE` case rule
    /// that has since changed, and has to be built again rather than run. See
    /// `Compiled::settings`.
    ///
    /// @param params - the next execution's parameters, carrying its context
    pub fn built_under(&self, params: &Params) -> bool {
        self.settings == params.settings()
    }

    /// Runs the statement against one parameter set, over a catalog borrowed
    /// only for the length of this call.
    ///
    /// Folds this execution's uncorrelated subqueries first, every time - see
    /// `Statement::run`'s doc for why a source's seek key needs that and a
    /// one-time fold at build time does not reach it.
    ///
    /// @param plan - the planner's output this chain was compiled from
    /// @param catalog - where the trees and layouts come from, for this
    ///   execution only
    /// @param params - the values bound to `?1`, `?2`, ...
    pub fn run(
        &mut self,
        plan: &PhysicalPlan,
        catalog: &dyn TreeCatalog,
        params: &Params,
    ) -> DbResult<()> {
        let folded = crate::subquery::fold(plan, catalog, params)?;
        let params = folded.as_ref().unwrap_or(params);
        let bound = params.bindings();
        if !std::sync::Arc::ptr_eq(&self.bindings, &bound) {
            if let (Ok(from), Ok(mut held)) = (bound.lock(), self.bindings.lock()) {
                held.copy_from(&from);
            }
        }
        let source = {
            let mut space = self.held.view(&self.prepared.stages);
            space.catalog = Some(catalog);
            source_for_run(plan, catalog, &space, params, &self.prepared, self.limit)?
        };
        // Also clears `self.collected`: `CollectInto::reset` empties the
        // buffer it was given, which is this chain's own sink at the bottom
        // of `upper`.
        self.upper.reset()?;
        let pool = source_pool(catalog, &self.prepared);
        if self.joins.is_empty() {
            // The common case - and the one the paired measurement's `SELECT
            // 1` and point-probe shapes are - pays nothing for a tower that
            // is not there: `self.upper` is pushed into directly, exactly as
            // it was before Stage 2 existed.
            return source.run(pool, self.upper.as_mut());
        }
        // The borrowed tower, rebuilt bottom-up every call: each level asks
        // the catalog for its own tree and pool fresh, so a write, a
        // rollback or a reopen between two runs is answered from whatever is
        // actually there - never from a tree this `Compiled` remembered.
        // Innermost (the highest-indexed stage) is wrapped first, matching
        // `build_chain`'s own `(1..stages.len()).rev()` order.
        let mut chain: Box<dyn Sink + '_> = Box::new(Borrowed(self.upper.as_mut()));
        for recipe in self.joins.iter().rev() {
            let inner = catalog
                .tree(recipe.root)
                .ok_or_else(|| misuse(format!("no tree imported for root page {}", recipe.root)))?;
            let inner_pool = catalog.pool_for(recipe.root).ok_or_else(|| {
                misuse("the inner side of a join names a database this connection does not hold")
            })?;
            chain = Box::new(IndexNestedLoopJoin::new(
                recipe.kind,
                inner,
                inner_pool,
                std::rc::Rc::clone(&recipe.keys),
                recipe.inner_projection.clone(),
                recipe.full_key,
                chain,
            ));
        }
        source.run(pool, chain.as_mut())
    }

    /// Takes the rows the last [`Compiled::run`] produced, leaving the buffer
    /// empty.
    ///
    /// Taken rather than cloned, for the reason `run_prepared` takes rather
    /// than clones: the caller is about to own these rows regardless, and
    /// nothing reads the buffer again before the next run clears it.
    pub fn take_rows(&self) -> Vec<Vec<OwnedDatum>> {
        std::mem::take(&mut *self.collected.borrow_mut())
    }
}

/// Attempts to compile a statement into a [`Compiled`] chain with no
/// lifetime, suitable for a cache that outlives any one borrow of the
/// connection.
///
/// Returns `None` only when **no build was attempted at all**, so the caller
/// pays for exactly one build either way - see the note on double-building
/// below. That happens for exactly the shapes `build_nested`/`build_chain`
/// would answer differently than a lifetime-free chain can:
///
/// - **a correlated block.** Checked first, cheaply, from the plan alone -
///   before `build_upper` runs - because a `Correlated` operator borrows the
///   catalog for the chain's own life, which a lifetime-free `Compiled`
///   cannot hold, and there is no way to build one that omits it and still
///   answers the query.
/// - **an inner stage that is not an index nested loop join.** `try_join_recipe`
///   decides this the same way `build_nested` does, and is checked before
///   `build_upper` too - a lateral module, a materialised or outer join, a
///   table scan `PRAGMA automatic_index` would key are all refused, because
///   each either bakes rows in at build time or needs the catalog borrowed
///   for its own life.
///
/// Returns `Some(compiled)` for every other shape, **whether or not it turns
/// out to be reusable** - see [`Compiled::rebindable`]. A `LIMIT ?1`, a
/// projected `?2`, a deterministic call folded over a bound parameter (see
/// `docs/roadmap.md` item 15), or a join key reading an uncorrelated
/// subquery all read a parameter during the build and are never kept across
/// executions - but the build already happened, already ran whatever
/// evaluating that read required, and is a completely valid answer to *this*
/// execution. The caller runs it once regardless of `rebindable()` and only
/// decides whether to keep it afterward.
///
/// **This is why `Some`/`None` split the way they do, and not on
/// reusability.** A build that runs a registered function, folds a subquery,
/// or reads `now()` has a real side effect the first time it happens; asking
/// "is this reusable" *after* paying for the build and then discarding the
/// answer to ask `run_any_prepared` to build it again was exactly the
/// regression this function used to cause - a deterministic function
/// registered with `FunctionFlags { deterministic: true, .. }` and called
/// with a bound parameter, `SELECT counted(?1) FROM t`, was invoked twice on
/// its first execution and once on every execution after, because the first
/// one paid for a failed `try_compile` attempt *and* the fallback build. The
/// invariant this function now keeps: **a deterministic call with
/// constant-for-this-execution arguments is evaluated once per execution,
/// and the build counts as the first execution rather than as a zeroth.**
///
/// @param plan - the planner's output
/// @param catalog - where the trees and layouts come from, for this one build
/// @param prepared - the structural choices `prepare` made
/// @param params - the first execution's bound values
pub fn try_compile(
    plan: &PhysicalPlan,
    catalog: &dyn TreeCatalog,
    prepared: &Prepared,
    params: &Params,
) -> DbResult<Option<Compiled>> {
    // **A compound has no single pipeline to compile.** `prepare_any` hands a
    // compound an empty `Prepared` on purpose - see its own doc comment - so
    // that its arms are re-decided per execution by `run_compound` rather than
    // described by a `Prepared` built for one of several shapes. Building
    // straight ahead here used exactly that empty `Prepared`'s zero stages as
    // if they were a real single-arm plan, so any column the head arm read
    // failed with "the tree read for FROM term ... does not carry column ...",
    // and a head arm that reads no column - `SELECT 1, 'x' UNION SELECT 2,
    // 'y'` - "succeeded" by silently answering the head arm alone and
    // discarding every other arm and the set operation itself. `refuse_unhandled`
    // in `build_upper` catches a window function the same way this catches a
    // compound - by refusing to reach the pipeline builder with a shape it does
    // not describe - but nothing analogous existed for a compound, because a
    // compound is a property of `PhysicalPlan` and `refuse_unhandled` only ever
    // saw the bound `select`. Bailing out here, the same way a correlated block
    // does below, sends every execution through `run_any_prepared`, which
    // already dispatches a compound to `run_compound` correctly.
    if !plan.compounds.is_empty() {
        return Ok(None);
    }
    // **A window function has no single pipeline to compile either, and until
    // task-1932 nothing here said so.** `prepare_any` bails on
    // `plan.select.windows` beside `plan.compounds` for the same reason - a
    // window pass plans an inner query of its own - but this function checked
    // only the compound. A windowed statement therefore reached `build_upper`,
    // whose `refuse_unhandled` returned `unsupported("a window function
    // reaching the pipeline builder")`, and `run_cached_query` propagated that
    // with `?` instead of falling back the way a compound does. Every entry
    // point an application uses goes through this path, so `run_windowed` -
    // which works, and which `run_any_prepared` dispatches to - was reachable
    // only from `run_with`, which nothing but a test calls. The compat manifest
    // recorded the symptom as a missing capability. Bailing out here sends a
    // windowed statement through `run_any_prepared` to `run_windowed`, exactly
    // as a compound goes to `run_compound`.
    if !plan.select.windows.is_empty() {
        return Ok(None);
    }
    // Every uncorrelated subquery is answered once, here, so a source key or
    // a join key that reads one builds correctly on this first attempt too -
    // the same reason `build_statement` folds before building.
    let folded = crate::subquery::fold(plan, catalog, params)?;
    let params = folded.as_ref().unwrap_or(params);
    let held = space_of(catalog, prepared)?;
    let space = {
        let mut space = held.view(&prepared.stages);
        space.catalog = Some(catalog);
        space
    };

    // Both checks below are side-effect free - a walk of the plan and the
    // prepared stages, nothing that runs a registered function or answers a
    // subquery - so a shape they refuse costs this call nothing beyond the
    // walk itself, and `run_any_prepared`'s build is the only one that ever
    // happens for it.
    if crate::correlate::has_correlations(plan) {
        return Ok(None);
    }
    let mut joins = Vec::with_capacity(prepared.stages.len().saturating_sub(1));
    for index in 1..prepared.stages.len() {
        let stage = prepared
            .stages
            .get(index)
            .ok_or_else(|| misuse("a stage vanished while building"))?;
        match try_join_recipe(plan, &space, params, stage, index)? {
            Some(recipe) => joins.push(recipe),
            None => return Ok(None),
        }
    }

    // From here on, the build genuinely happens, and whatever it does -
    // fold a subquery, call a deterministic function, read `now()` - happens
    // exactly once no matter what the rest of this function decides.
    let before = params.reads();
    let collected = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
    let sink = Box::new(CollectInto::new(std::rc::Rc::clone(&collected)));
    // The listing is kept here: a `Compiled` builds its chain once and every
    // later execution reuses it, so rendering the operators costs one render
    // per compiled statement rather than one per execution, and
    // `Compiled::shape` is what reports it.
    let upper = crate::physical::build_upper(
        plan,
        catalog,
        prepared,
        &space,
        params,
        sink,
        crate::physical::Listing::kept(),
    )?;
    if !upper.correlations.is_empty() {
        // Should not happen - the cheap check above already refused any plan
        // with one - but `build_upper` is the ground truth here, and a
        // correlated chain still cannot be run without the operator this
        // function does not build.
        return Ok(None);
    }
    let rebindable = params.reads() == before;
    let bindings = params.bindings();
    let mut listing = upper.operators;
    listing.add(|| describe_source(prepared));
    let mut operators = listing.into_lines();
    operators.reverse();
    Ok(Some(Compiled {
        prepared: prepared.clone(),
        held,
        upper: upper.head,
        joins,
        shape: Shape {
            names: upper.names,
            operators,
        },
        limit: upper.limit,
        rebindable,
        settings: params.settings(),
        bindings,
        collected,
    }))
}

/// Whether a cached statement's compiled chain has been attempted yet.
///
/// **Attempted once and remembered, never retried.** The question "can this
/// statement's chain be reused" is answered by *building* one and watching
/// what came out - the same way `Statement::rebindable` is decided by
/// counting reads rather than a second opinion about which constructs may
/// carry a parameter - so trying again on every execution would rebuild the
/// very thing this exists to stop rebuilding. A statement that answered
/// [`Slot::Never`] answers it forever; the cost of that never-changing answer
/// is one failed build, once, and it is why [`Slot`] has three states rather
/// than an `Option`.
#[derive(Default)]
pub enum Slot {
    /// Nothing has been built yet.
    #[default]
    Untried,
    /// A chain built once that answers a fresh set of bound values without
    /// being rebuilt.
    ///
    /// Boxed because `Untried` and `Never` carry nothing, and a `Compiled` is
    /// wide enough (the operator chain, the layouts, the bindings cell) that
    /// every `Slot` would otherwise be sized for the one variant that holds
    /// something.
    Reusable(Box<Compiled>),
    /// Building one failed the [`try_compile`] check - more than one stage, a
    /// correlated block, or a parameter reaching anything but the source.
    /// Building is not attempted again.
    Never,
}
