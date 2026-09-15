//! Joining: the nested loop, the lateral arm, and the materialised side.
//!
//! Invariant: **a join reads the inner side through the same cursor protocol a
//! scan does.** There is no join-only path into a tree, so a defect in the
//! cursor is a defect a scan finds too rather than one that only shows up
//! under a join.

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
use inillucent_sql::ast::SortOrder;
use inillucent_sql::bind::BoundExpr;
use inillucent_sql::plan::{AccessPath, AggregationMode, PhysicalPlan};
use inillucent_tree::datum::{Datum, OwnedDatum};

use crate::batch::Batch;
use crate::expr::{compile, Expr, StaticType};
use crate::join::{IndexNestedLoopJoin, JoinKind, NestedLoopJoin};
use crate::ops::{CollectInto, Sink, SortKey};
use crate::paged::{FullScan, PointProbe};
use crate::scan::Projection;

/// What a vector probe's counting rounds read, and what they are planned over.
///
/// **A type rather than six of ten arguments (task-1962, A9).**
/// [`iterative_candidates`] took ten, of which four were the planning context
/// every function in this module takes and two were the stage and the probe it
/// reads through. What varies per call is the store, the vector and the two
/// depths; those are still arguments.
pub(crate) struct CandidateProbe<'a, 't> {
    /// The planner's output, for the residual predicates.
    pub(crate) plan: &'a PhysicalPlan,
    /// Where the index and the pool come from.
    pub(crate) catalog: &'a dyn TreeCatalog,
    /// The joined column space the predicates are compiled over.
    pub(crate) space: &'a Space<'a>,
    /// The values bound to `?1`, `?2`, ...
    pub(crate) params: &'a Params,
    /// The vector stage, for its tree's pool.
    pub(crate) stage: &'a PreparedStage,
    /// The probe the counting rounds read rows with.
    pub(crate) probe_over: &'a PointProbe<'t>,
}

/// A catalog that also answers one recursive CTE's queue.
///
/// Everything else is delegated, so the step arm sees exactly the trees, the
/// layouts and the modules the statement sees. Wrapping rather than threading a
/// parameter through every builder is what keeps a recursive query from
/// changing the shape of a signature nothing else uses.
use super::*;

/// Returns the candidate rowids a filtered vector search must look at.
///
/// **An approximate index probed for `k` and then filtered returns fewer than
/// `k` rows, and nothing says so.** 400 vectors, a predicate keeping 5% of
/// them and `LIMIT 10` returned one row where the exhaustive plan returns ten:
/// recall 0.1, silently. The index was asked for ten neighbours and nine of
/// them failed the `WHERE`, so nine of the answer's rows were never candidates
/// at all.
///
/// So the probe is *iterative*, which is what pgvector's `hnsw.iterative_scan`
/// is: ask for `k`, test the residual over what came back, and if fewer than
/// `k` rows survive, ask deeper - until enough survive or the store is
/// exhausted, which it is the moment it answers with fewer rows than it was
/// asked for. A query with no residual is the case this whole function skips:
/// there is nothing to lose, so one round is the answer.
///
/// The cost of a round the predicate rejects is one descent per candidate, and
/// those descents are made twice - once here to count, once in
/// [`Source::Vector`] to produce. That is the price of keeping the predicate
/// where the pipeline already tests it rather than pushing a second copy of
/// the expression evaluator into the store's cursor, and it is paid only by a
/// vector query that carries a `WHERE`.
///
/// @param scan - what the rounds read, and what they are planned over
/// @param index - the store's name
/// @param wanted - the vector to measure against
/// @param depth - the `LIMIT`, which is the first round's `k`
/// @param limit - the statement's row limit, when it has a constant one
pub(crate) fn iterative_candidates(
    scan: &CandidateProbe<'_, '_>,
    index: &[u8],
    wanted: &Datum<'_>,
    depth: usize,
    limit: Option<usize>,
) -> DbResult<Vec<i64>> {
    let CandidateProbe {
        plan,
        catalog,
        space,
        params,
        stage,
        probe_over,
    } = *scan;
    let ask = |k: usize| -> DbResult<Vec<i64>> {
        catalog.vector_candidates(index, wanted, k)?.ok_or_else(|| {
            misuse(format!(
                "no vector index named {}",
                String::from_utf8_lossy(index)
            ))
        })
    };
    let predicates = residual_programs(plan, space, params)?;
    let mut keys = ask(depth)?;
    if predicates.is_empty() {
        return Ok(keys);
    }
    // The rows the statement is asking for. `LIMIT` is what a vector path is
    // chosen by, so this is nearly always `depth` - but a plan that arrived
    // here with a smaller chain limit should stop at the smaller number.
    let target = limit.unwrap_or(depth).min(depth).max(1);
    let Some(pool) = catalog.pool_for(stage.root) else {
        return Ok(keys);
    };
    let mut want = depth;
    loop {
        // The store answering with fewer rows than it was asked for is the only
        // honest signal that there is nothing more to find, and it is checked
        // before the count so an exhausted graph ends the loop even when the
        // predicate rejects everything.
        let exhausted = keys.len() < want;
        let survivors = surviving_rows(&keys, pool, probe_over, &predicates)?;
        if survivors >= target || exhausted || want >= VECTOR_CANDIDATE_CAP {
            return Ok(keys);
        }
        want = want.saturating_mul(VECTOR_WIDEN).min(VECTOR_CANDIDATE_CAP);
        keys = ask(want)?;
    }
}
/// Compiles the predicates the pipeline will test over a stage's own rows.
///
/// The whole `WHERE` minus what the access path already consumed, which is
/// exactly what [`build_chain`] hangs `Filter` operators for - built a second
/// time here rather than shared, because the chain's copies are boxed into an
/// operator tree the source cannot reach into.
///
/// @param plan - the planner's output
/// @param space - the joined column space
/// @param params - the bound parameters
fn residual_programs(
    plan: &PhysicalPlan,
    space: &Space<'_>,
    params: &Params,
) -> DbResult<Vec<Box<dyn crate::expr::Eval>>> {
    let mut built: Vec<Box<dyn crate::expr::Eval>> = Vec::new();
    for expr in plan
        .constant_filter
        .iter()
        .chain(plan.residuals.iter().flatten())
    {
        let translated = translate_scan(expr, space, params)?;
        built.push(compile(&translated, space.types)?);
    }
    Ok(built)
}
/// Counts how many of a candidate list's rows pass every predicate.
///
/// @param keys - the candidate rowids
/// @param pool - the buffer pool the rows are read through
/// @param probe_over - the probe the rows are read with
/// @param predicates - the compiled residuals, all of which must hold
fn surviving_rows(
    keys: &[i64],
    pool: &Pool,
    probe_over: &PointProbe<'_>,
    predicates: &[Box<dyn crate::expr::Eval>],
) -> DbResult<usize> {
    let mut buffer: Vec<OwnedDatum> = Vec::new();
    let mut seen = 0usize;
    for key in keys {
        if !probe_over.lookup(pool, &[Datum::Int(*key)], &mut buffer)? {
            continue;
        }
        let borrowed: Vec<Datum<'_>> = buffer.iter().map(OwnedDatum::borrow).collect();
        let columns: Vec<crate::batch::Vector<'_>> = borrowed
            .iter()
            .map(|value| crate::batch::Vector::Values(std::slice::from_ref(value)))
            .collect();
        let batch = Batch::new(1, columns);
        let mut held = true;
        for predicate in predicates {
            let verdict = predicate.value(&batch, 0)?;
            if crate::expr::truth(&verdict.get()) != Some(true) {
                held = false;
                break;
            }
        }
        if held {
            seen = seen.saturating_add(1);
        }
    }
    Ok(seen)
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
pub(crate) fn build_nested<'t>(
    plan: &PhysicalPlan,
    catalog: &'t dyn TreeCatalog,
    space: &Space<'_>,
    params: &Params,
    stage: &PreparedStage,
    index: usize,
    downstream: Box<dyn Sink + 't>,
) -> DbResult<Box<dyn Sink + 't>> {
    let source_term = plan
        .sources
        .get(stage.term)
        .ok_or_else(|| misuse("a stage names a FROM term the plan does not have"))?;
    // **Two shapes of inner term, and the join kind is what picks between
    // them.** An inner join over a tree probes it once per outer row and never
    // materialises anything, which is the shape every read family measures. An
    // outer join, and any term that is not a tree at all, reads its rows once
    // into a buffer and pairs them: an outer join has to know that a pair
    // *failed* the condition in order to null-extend instead of dropping, and a
    // probe cannot tell that from a key that was not there.
    // **A table-valued function whose argument reads an outer column.** It has
    // a different answer per outer row, so it is driven per outer row; see
    // `crate::lateral` for why materialising it once would be wrong rather than
    // slow. This is checked before the materialised path, which is where such a
    // term would otherwise go.
    if let AccessPath::VirtualScan { offer, .. } = &source_term.path {
        if offer.iter().any(|held| reads_a_column(&held.value)) {
            return build_lateral_join(plan, catalog, space, params, stage, downstream);
        }
    }
    // **An outer join whose key is its whole condition is an index nested
    // loop.** The materialised shape below reads the inner side
    // once into a buffer, which is linear in the inner table however few outer
    // rows there are: `LEFT JOIN chunk c ON c.document_id = d.id` for one
    // document read all 60,000 chunks, at 138.2 ms against 0.5 ms for the same
    // join written `JOIN`. `IndexNestedLoopJoin` already answers `JoinKind::Left`
    // - it null-extends an outer row whose probe found nothing - so what was
    // missing was the permission to use it.
    //
    // The permission is `on_enforced`: the planner says so only when every
    // conjunct of the `ON` became part of the key. That is the condition this
    // operator needs, because it has nowhere to test what the key did not
    // capture and a pair that failed such a test has to null-extend rather than
    // vanish.
    let outer_by_probe = inillucent_sql::plan::is_outer(source_term.join)
        && source_term.on_enforced
        && stage.kind != AccessKind::Materialised;
    if (inillucent_sql::plan::is_outer(source_term.join) && !outer_by_probe)
        || stage.kind == AccessKind::Materialised
    {
        return build_materialised_join(plan, catalog, space, params, stage, index, downstream);
    }
    let tree = catalog
        .tree(stage.root)
        .ok_or_else(|| misuse(format!("no tree imported for root page {}", stage.root)))?;
    let outer_types: Vec<StaticType> = space
        .types
        .get(..stage.offset)
        .map(<[StaticType]>::to_vec)
        .unwrap_or_default();
    let (keys, full_key) = if stage.is_lookup {
        // **What the index entry carries to find the table row with**, read out
        // of the previous stage's row: a rowid for an ordinary table, and a
        // `WITHOUT ROWID` table's primary key - which is several columns, in the
        // table's own key order - for one of those. `identity` is the field that
        // says which, and it exists because `key_columns` on an index layout
        // names the whole entry rather than this part of it.
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
    // **And a third shape: an inner term with nothing to seek on.** An empty key
    // list means the loop below walks the whole inner tree once per outer row,
    // which is a join that is rows times rows. That is exactly the case
    // `PRAGMA automatic_index` is about, in SQLite and here: read the inner side
    // once instead, key it on the join expression, and probe. The test is only
    // whether there is a key to build the table on - `build_materialised_join`
    // is where it is built.
    if keys.is_empty()
        && plan
            .levers
            .has(inillucent_sql::plan::Levers::AUTOMATIC_INDEX)
        && has_equi_key(plan, space, params, stage, index)?
    {
        return build_materialised_join(plan, catalog, space, params, stage, index, downstream);
    }
    let compiled = keys
        .iter()
        .map(|expr| compile(expr, &outer_types))
        .collect::<DbResult<Vec<_>>>()?;
    Ok(Box::new(IndexNestedLoopJoin::new(
        // **The lookup stage carries the same kind as the seek that fed it.**
        // A null-extended index row probes the table with a NULL identity and
        // finds nothing; an `Inner` lookup would drop it, which would lose the
        // very row the outer join produced it for.
        join_kind_of(source_term.join),
        tree,
        // **This stage's pool, not the pipeline's.** The inner side of an index
        // nested loop is where a join across two databases reaches the second
        // file, so the pool travels with the stage that reads it.
        catalog.pool_for(stage.root).ok_or_else(|| {
            misuse("the inner side of a join names a database this connection does not hold")
        })?,
        compiled,
        Projection::all(stage.width),
        full_key,
        downstream,
    )))
}
/// Reports whether an inner stage's condition can key a hash table.
///
/// Asked *before* the join shape is chosen, because the answer is what chooses
/// it: without a key there is nothing to build and the nested loop is the only
/// shape left. It runs the same extraction the builder does, which is one
/// translation and one walk of a condition - paid once per stage at compile
/// time, against a join it is about to make linear.
///
/// @param plan - the planner's output
/// @param space - the joined column space
/// @param params - the bound parameters
/// @param stage - the inner stage
/// @param index - the stage's position, which names its residual
pub(crate) fn has_equi_key(
    plan: &PhysicalPlan,
    space: &Space<'_>,
    params: &Params,
    stage: &PreparedStage,
    index: usize,
) -> DbResult<bool> {
    let Some(source_term) = plan.sources.get(stage.term) else {
        return Ok(false);
    };
    let Some(expr) = source_term
        .on
        .as_ref()
        .or_else(|| plan.residuals.get(index).and_then(Option::as_ref))
    else {
        return Ok(false);
    };
    let translated = translate_scan(expr, space, params)?;
    Ok(crate::autoindex::equi_keys(&translated, stage.offset, stage.width).is_some())
}
/// Reports whether an expression reads any column at all.
///
/// The test that separates a table-valued function's *constant* argument -
/// `json_each('[1,2]')`, `generate_series(1, 10)` - from one that reads the row
/// beside it. The first can be folded once; the second cannot be folded at all.
///
/// @param expr - the argument expression
pub(crate) fn reads_a_column(expr: &BoundExpr) -> bool {
    let mut used = inillucent_sql::bind::ColumnUse::default();
    // Asked about *every* source: an argument reading this term's own column
    // would be a cycle the binder does not produce, so any column at all means
    // an outer one.
    for source in 0..MAX_SOURCES {
        expr.columns_read(source, &mut used);
        // A rowid read is still a column read. `ColumnUse` keeps it in its own
        // `rowid` flag rather than in `columns` - see `BoundExpr::columns_read`
        // - because a rowid is not one of the term's declared slots, and
        // dropping it here answered `false` for `docs JOIN owner ON owner.id =
        // docs.rowid`: `owner.id` is `owner`'s rowid alias, so the join's own
        // key read only set `used.rowid`, this function said the module's term
        // read no outer column, and a value that only exists per outer row was
        // then folded once as if it were a statement-wide constant.
        if used.opaque || used.rowid || !used.columns.is_empty() {
            return true;
        }
    }
    false
}
/// Reports whether an expression reads a bound parameter anywhere in it.
///
/// The line between the two folds a deterministic registered function's
/// argument gets - `docs/roadmap.md` item 15's table. An argument that is
/// every literal is a constant regardless of which execution asked, so
/// [`translate`] folds it once and never again. An argument that reads `?N`
/// is a constant only for the execution now binding it, and folding it the
/// same way would bake one execution's answer into a chain a later execution
/// could reuse - so [`translate`] calls [`Params::note_execution_constant`]
/// whenever this answers `true`, the same guard a folded `now()` already
/// relies on to keep such a chain from being re-run against new values.
///
/// @param expr - the argument expression
pub(crate) fn reads_a_parameter(expr: &BoundExpr) -> bool {
    if matches!(expr, BoundExpr::Parameter(_)) {
        return true;
    }
    expr.children().iter().any(|child| reads_a_parameter(child))
}
/// Builds an inner stage as a module driven once per outer row.
///
/// @param plan - the planner's output
/// @param catalog - where the module's rows come from
/// @param space - the joined column space
/// @param params - the bound parameters
/// @param stage - the inner stage
/// @param downstream - what to push joined rows into
fn build_lateral_join<'t>(
    plan: &PhysicalPlan,
    catalog: &'t dyn TreeCatalog,
    space: &Space<'_>,
    params: &Params,
    stage: &PreparedStage,
    downstream: Box<dyn Sink + 't>,
) -> DbResult<Box<dyn Sink + 't>> {
    let source_term = plan
        .sources
        .get(stage.term)
        .ok_or_else(|| misuse("a stage names a FROM term the plan does not have"))?;
    let AccessPath::VirtualScan { offer, chosen, .. } = &source_term.path else {
        return Err(misuse("a lateral join over a term that is not a module"));
    };
    let outer_types: Vec<StaticType> = space
        .types
        .get(..stage.offset)
        .map(<[StaticType]>::to_vec)
        .unwrap_or_default();
    // **In the order the module asked for them.** `best_index` chose which
    // offered constraints feed `filter` and in what order; the values handed
    // down have to arrive in that order, because the module reads them
    // positionally.
    let order: Vec<usize> = match chosen {
        Some(choice) => choice.arguments.clone(),
        None => (0..offer.len()).collect(),
    };
    let mut arguments = Vec::with_capacity(order.len());
    for position in order {
        let Some(constraint) = offer.get(position) else {
            continue;
        };
        let translated = translate_scan(&constraint.value, space, params)?;
        arguments.push(compile(&translated, &outer_types)?);
    }
    Ok(Box::new(crate::lateral::LateralModule::new(
        source_term.table.clone(),
        source_term.path.clone(),
        params.clone(),
        plan.select.columns_read(source_term.id),
        arguments,
        catalog,
        stage.width,
        downstream,
    )))
}
/// Builds one inner stage as a nested loop over rows read once.
///
/// **The one shape that can answer an outer join.** Its build side is a vector,
/// so it can evaluate the `ON` condition over each candidate pair - which is
/// what distinguishes "no partner" from "a partner that failed the condition",
/// the whole difference between an inner join and a `LEFT` one - and it can
/// remember which build rows matched, which is the whole of `RIGHT` and `FULL`.
///
/// It is also what a term that is not a tree gets: a derived table, a recursive
/// CTE and a virtual table each produce rows rather than pages, and reading
/// them once rather than once per outer row is correct because none of them has
/// a free variable to re-evaluate.
///
/// @param plan - the planner's output
/// @param catalog - where the trees and layouts come from
/// @param space - the joined column space
/// @param params - the bound parameters
/// @param stage - the inner stage
/// @param downstream - what to push joined rows into
fn build_materialised_join<'t>(
    plan: &PhysicalPlan,
    catalog: &'t dyn TreeCatalog,
    space: &Space<'_>,
    params: &Params,
    stage: &PreparedStage,
    index: usize,
    downstream: Box<dyn Sink + 't>,
) -> DbResult<Box<dyn Sink + 't>> {
    let source_term = plan
        .sources
        .get(stage.term)
        .ok_or_else(|| misuse("a stage names a FROM term the plan does not have"))?;
    // An inner term's rows are all read: the `LIMIT` above counts *joined*
    // rows, and a join may drop any of them, so stopping the inner side early
    // would be stopping it on a count that is not the one the statement asked
    // about.
    let rows = materialise_stage(plan, catalog, params, stage, None)?;
    // The condition is compiled over the *joined* row - every column produced
    // so far, then this stage's - which is exactly the space the pipeline
    // already describes, so an `ON` naming either side needs no special case.
    let joined_types: Vec<StaticType> = space
        .types
        .get(..stage.offset.saturating_add(stage.width))
        .map(<[StaticType]>::to_vec)
        .unwrap_or_else(|| space.types.to_vec());
    let condition = match &source_term.on {
        Some(expr) => {
            let translated = translate_scan(expr, space, params)?;
            Some(compile(&translated, &joined_types)?)
        }
        None => None,
    };
    // **The automatic index.** When the condition is a conjunction of plain
    // equalities with one side of the join per term, the inner rows go into a
    // hash table keyed on the inner halves and every outer row probes it -
    // which turns a join that was rows-times-rows into rows-plus-rows. SQLite
    // builds a transient b-tree for the same reason and puts it under the same
    // switch; `crate::autoindex` says why the structures differ and the switch
    // does not.
    //
    // The nested loop below is what `PRAGMA automatic_index = off` selects, and
    // it is also what a condition this cannot key on gets - which is most of
    // them, deliberately: a residual predicate the hash key did not capture
    // would have to be re-tested per pair, and there is nowhere here to do it.
    if plan
        .levers
        .has(inillucent_sql::plan::Levers::AUTOMATIC_INDEX)
    {
        // **`ON` for an outer join, the residual for an inner one.** A non-outer
        // join's constraint is split into the planner's term list before paths
        // are chosen, so what is left of `b.p = a.x` arrives as this stage's
        // residual and `source_term.on` is empty - which is exactly the case
        // this optimisation exists for.
        //
        // The residual is left in place rather than removed. It is applied as a
        // `Filter` above every join, so re-testing a condition the hash key has
        // already enforced costs a comparison per surviving row and cannot
        // change an answer; removing it would mean proving that the key
        // captured the whole predicate, and this operator has no way to prove
        // that about an expression it declined to look inside.
        let keyed = source_term
            .on
            .as_ref()
            .or_else(|| plan.residuals.get(index).and_then(Option::as_ref));
        if let Some(expr) = keyed {
            let translated = translate_scan(expr, space, params)?;
            if let Some(keys) = crate::autoindex::equi_keys(&translated, stage.offset, stage.width)
            {
                let outer_types: Vec<StaticType> = space
                    .types
                    .get(..stage.offset)
                    .map(<[StaticType]>::to_vec)
                    .unwrap_or_default();
                let inner_types: Vec<StaticType> = space
                    .types
                    .get(stage.offset..stage.offset.saturating_add(stage.width))
                    .map(<[StaticType]>::to_vec)
                    .unwrap_or_default();
                let probe = keys
                    .probe
                    .iter()
                    .map(|expr| compile(expr, &outer_types))
                    .collect::<DbResult<Vec<_>>>()?;
                let build = keys
                    .build
                    .iter()
                    .map(|expr| compile(expr, &inner_types))
                    .collect::<DbResult<Vec<_>>>()?;
                let mut join = crate::join::HashJoin::new(
                    join_kind_of(source_term.join),
                    build,
                    probe,
                    downstream,
                );
                join.build_materialised(&rows)?;
                return Ok(Box::new(join));
            }
        }
    }
    Ok(Box::new(NestedLoopJoin::new(
        join_kind_of(source_term.join),
        rows,
        condition,
        downstream,
    )))
}
/// Returns the executor's join kind for the one the statement wrote.
///
/// `CROSS` and a comma are inner joins that differ only in whether the planner
/// may reorder them, which it decided before this pass ran.
///
/// @param join - the join as the statement wrote it
pub(crate) fn join_kind_of(join: inillucent_sql::ast::JoinKind) -> JoinKind {
    match join {
        inillucent_sql::ast::JoinKind::Left => JoinKind::Left,
        inillucent_sql::ast::JoinKind::Right => JoinKind::Right,
        inillucent_sql::ast::JoinKind::Full => JoinKind::Full,
        inillucent_sql::ast::JoinKind::Comma
        | inillucent_sql::ast::JoinKind::Inner
        | inillucent_sql::ast::JoinKind::Cross => JoinKind::Inner,
    }
}
/// Reads one stage's rows into a buffer, whatever kind of source it is.
///
/// @param plan - the planner's output
/// @param catalog - where the trees and layouts come from
/// @param params - the bound parameters
/// @param stage - the stage to read
/// @param limit - the rows the statement above will keep, when it says
pub(crate) fn materialise_stage(
    plan: &PhysicalPlan,
    catalog: &dyn TreeCatalog,
    params: &Params,
    stage: &PreparedStage,
    limit: Option<usize>,
) -> DbResult<Vec<Vec<OwnedDatum>>> {
    let source_term = plan
        .sources
        .get(stage.term)
        .ok_or_else(|| misuse("a stage names a FROM term the plan does not have"))?;
    match &source_term.path {
        AccessPath::Subquery { plan: inner, .. } => {
            // **A compound is run as a compound.**
            // `SELECT ... FROM (a UNION ALL b)` is an ordinary derived table
            // whose inner plan happens to have arms, and `prepare` refuses a
            // plan with arms by name - so the whole statement came back
            // `the new engine's physical pass does not handle a compound query
            // yet` for a shape the executor could already run. `run_compound`
            // is what the top level uses for exactly this plan, and a
            // materialised term wants what it produces: the rows, once.
            if inner.compounds.is_empty() {
                let prepared = prepare(inner, catalog, ForcePlan::default())?;
                Ok(run_prepared(inner, catalog, &prepared, params)?.0)
            } else {
                Ok(run_compound(inner, catalog, params)?.0)
            }
        }
        AccessPath::VirtualScan { .. } => {
            let needed = plan.select.columns_read(source_term.id);
            catalog
                .virtual_rows(&source_term.table, &source_term.path, params, &needed)?
                .ok_or_else(|| misuse("a virtual table the caller does not have"))
        }
        AccessPath::Recursive {
            seeds,
            steps,
            width,
        } => crate::recursive::run_recursive(
            source_term.id,
            seeds,
            steps,
            *width,
            catalog,
            params,
            limit,
        ),
        // The queue the fill loop is on, handed in by `run_recursive` through a
        // catalog that answers it. A plan reaching this outside such a loop is
        // a plan the binder should not have produced.
        AccessPath::RecursiveSelf { cte } => catalog
            .recursive_rows(*cte)
            .map(<[Vec<OwnedDatum>]>::to_vec)
            .ok_or_else(|| misuse("a reference to a recursive CTE outside the loop that fills it")),
        // Every remaining path reads a tree, and an outer term's path is a
        // plain scan of it by construction - the planner does not let a
        // predicate become a seek on a side that has to null-extend.
        _ => {
            let tree = catalog
                .tree(stage.root)
                .ok_or_else(|| misuse(format!("no tree imported for root page {}", stage.root)))?;
            let collected = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
            let mut sink = CollectInto::new(std::rc::Rc::clone(&collected));
            let pool = catalog.pool_for(stage.root).ok_or_else(|| {
                misuse("a materialised stage names a database this connection does not hold")
            })?;
            FullScan::new(tree, Projection::all(stage.width)).run(pool, &mut sink)?;
            let rows = collected.borrow().clone();
            Ok(rows)
        }
    }
}
/// How many passes a recursive CTE may make before the engine refuses.
///
/// A recursion whose step arm never stops producing rows is a query that does
/// not end, and the only difference between that and a slow one is a number, so
/// there is a number. SQLite's own guard is the same idea under a different
/// name: it stops when the queue is empty, and a `LIMIT` is what a person adds
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
pub(crate) fn skip_scan_applies(
    plan: &PhysicalPlan,
    catalog: &dyn TreeCatalog,
    root: u32,
) -> DbResult<bool> {
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
        let declared = match &column.expr {
            BoundExpr::Column { column, .. } => *column as usize,
            _ => return Ok(false),
        };
        let tree_column = match layout.slots.get(declared) {
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
pub(crate) fn projected_prefix(
    plan: &PhysicalPlan,
    space: &Space<'_>,
    params: &Params,
) -> DbResult<usize> {
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
pub(crate) fn is_scan_prefix(exprs: &[Expr], scan_order: &[Vec<usize>]) -> bool {
    if exprs.is_empty() || exprs.len() > scan_order.len() {
        return false;
    }
    exprs.iter().enumerate().all(|(position, expr)| match expr {
        Expr::Column(index) => scan_order
            .get(position)
            .is_some_and(|held| held.contains(index)),
        _ => false,
    })
}
/// Returns, for each column the walk is ordered by, every joined column that
/// carries that value.
///
/// **A non-covering seek carries its key twice.** The index
/// stage holds the key columns it is ordered by, and the table fetch behind it
/// holds the same values again under the table's own column numbers - and it is
/// the table's numbers a select list resolves to, because that is what the
/// caller wrote. Asking whether the ORDER BY is the index's own column
/// therefore answered no for every query of the form `SELECT <a column the
/// index does not cover> FROM t WHERE key > ? ORDER BY key`, which is keyset
/// paging, which is how anything walks a large table.
///
/// The index layout's `slots` is the map: `slots[t] = Some(p)` says the table's
/// column `t` sits at the index's tree column `p`. The lookup stage contributes
/// the table's columns starting at its own offset, so the same value is at
/// `offset + t`.
///
/// @param stages - the prepared stages, outermost first
/// @param layouts - each stage's layout
/// @param order - the tree columns the outermost walk is ordered by
pub(crate) fn order_equivalents(
    stages: &[PreparedStage],
    layouts: &[std::rc::Rc<SourceLayout>],
    order: &[usize],
) -> Vec<Vec<usize>> {
    let mut classes: Vec<Vec<usize>> = order.iter().map(|column| vec![*column]).collect();
    let Some(index_layout) = layouts.first() else {
        return classes;
    };
    for (stage, layout) in stages.iter().zip(layouts.iter()).skip(1) {
        if !stage.is_lookup {
            continue;
        }
        for (position, column) in order.iter().enumerate() {
            for (slot, held) in index_layout.slots.iter().enumerate() {
                if *held != Some(*column) {
                    continue;
                }
                // **Through the lookup's own slot map, not the record slot.**
                // A table tree carries its rowid first, so the record's slot 0
                // is its tree column 1 - and `offset + slot` names the rowid
                // rather than the value the index is ordered by.
                let Some(Some(tree_column)) = layout.slots.get(slot) else {
                    continue;
                };
                if let Some(class) = classes.get_mut(position) {
                    class.push(stage.offset.saturating_add(*tree_column));
                }
            }
        }
    }
    classes
}
/// Reports whether the projected rows already arrive in the ORDER BY's order.
///
/// @param sort_keys - the ordering terms, in output-column space
/// @param projected - the output expressions
/// @param plan - the planner's output
/// @param scan_order - the tree columns the leaves are ordered by
/// @param grouped_walk - whether a streaming grouped aggregate is in the chain
pub(crate) fn output_is_sorted_by(
    sort_keys: &[SortKey],
    projected: &[Expr],
    plan: &PhysicalPlan,
    scan_order: &[Vec<usize>],
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

#[cfg(test)]
mod tests {
    use super::*;
    use inillucent_value::{Affinity, Collation};

    /// A join's outer kind survives the translation, and the three that behave
    /// alike are one value.
    ///
    /// **`CROSS`, `INNER` and a comma are the same join (T3, task-1962).** They
    /// differ in what the planner may reorder, which the planner has already
    /// decided by the time the chain is built; keeping three values here would
    /// be three values nothing distinguishes, and a `LEFT` that fell into one
    /// of them would drop the unmatched rows a `LEFT JOIN` exists to keep.
    #[test]
    fn the_outer_kinds_are_kept_and_the_inner_ones_are_one() {
        use inillucent_sql::ast::JoinKind as Written;
        assert_eq!(join_kind_of(Written::Left), JoinKind::Left);
        assert_eq!(join_kind_of(Written::Right), JoinKind::Right);
        assert_eq!(join_kind_of(Written::Full), JoinKind::Full);
        for written in [Written::Comma, Written::Inner, Written::Cross] {
            assert_eq!(
                join_kind_of(written),
                JoinKind::Inner,
                "{written:?} produces the same rows as an inner join"
            );
        }
    }

    /// A constant argument reads no column; an argument over the row beside it
    /// does.
    ///
    /// **The test that decides whether a table-valued function's argument may
    /// be folded once.** `json_each('[1,2]')` can be; `json_each(t.body)`
    /// cannot be at all, and folding it would run the module once with the
    /// first row's value for every row.
    #[test]
    fn a_constant_argument_reads_no_column() {
        assert!(!reads_a_column(&BoundExpr::Text(b"[1,2]".to_vec())));
        assert!(!reads_a_column(&BoundExpr::Integer(10)));
        assert!(reads_a_column(&BoundExpr::Column {
            source: 0,
            column: 0,
            slot: 0,
            affinity: Affinity::Blob,
            collation: Collation::Binary,
        }));
        assert!(
            reads_a_column(&BoundExpr::Rowid { source: 0 }),
            "a rowid read is a column read, and answering otherwise folded a \
             per-row value as a statement-wide constant"
        );
    }

    /// A parameter anywhere in an argument makes it a constant for this
    /// execution only.
    #[test]
    fn a_parameter_is_found_at_any_depth() {
        assert!(reads_a_parameter(&BoundExpr::Parameter(1)));
        assert!(!reads_a_parameter(&BoundExpr::Integer(1)));
        let nested = BoundExpr::Not(Box::new(BoundExpr::And(
            Box::new(BoundExpr::Integer(1)),
            Box::new(BoundExpr::Parameter(2)),
        )));
        assert!(
            reads_a_parameter(&nested),
            "a parameter two levels down is still a parameter, and a chain \
             built around it may not be re-run against new values"
        );
    }

    /// An order is a scan prefix only when every position matches, from the
    /// first.
    #[test]
    fn an_order_matches_the_scan_only_from_its_start() {
        let scan_order = vec![vec![0usize], vec![1usize]];
        assert!(is_scan_prefix(&[Expr::Column(0)], &scan_order));
        assert!(is_scan_prefix(
            &[Expr::Column(0), Expr::Column(1)],
            &scan_order
        ));
        assert!(
            !is_scan_prefix(&[Expr::Column(1)], &scan_order),
            "the second key alone is not a prefix of the walk's order"
        );
        assert!(
            !is_scan_prefix(&[], &scan_order),
            "an empty order asks for nothing and cannot skip a sort"
        );
        assert!(
            !is_scan_prefix(
                &[Expr::Column(0), Expr::Column(1), Expr::Column(2)],
                &scan_order
            ),
            "an order longer than the walk's cannot be satisfied by it"
        );
        assert!(
            !is_scan_prefix(&[Expr::Literal(OwnedDatum::Int(1))], &scan_order),
            "only a column can match a walk's key position"
        );
    }
}
