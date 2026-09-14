//! The window pass: a whole execution strategy, reached from one line.
//!
//! Invariant: **a window reads its partition in both directions, so the rows
//! are buffered and not streamed.** `last_value` looks forward and `lag` looks
//! back, which no pipeline of sinks can serve: the input is collected, sorted
//! into the order each frame needs, and every window value is computed over the
//! buffer before a single row is handed on.
//!
//! This is here rather than in `physical.rs` because it is a strategy rather
//! than a stage. `run_any_prepared` chooses it and nothing else in that file
//! calls anything below; keeping it there made a 6,600 line module 7,200, and
//! the module size ratchet asks for an extraction rather than a larger number.

use inillucent_base::error::misuse;
use inillucent_base::DbResult;
use inillucent_sql::bind::{
    BoundExpr, BoundFrameBound, BoundOrderTerm, BoundResultColumn, BoundSelect, BoundWindow,
};
use inillucent_sql::plan::{plan_select_with, Levers, PhysicalPlan};
use inillucent_tree::datum::OwnedDatum;
use inillucent_value::collation::Collation;

use inillucent_sql::ast::{NullOrder, SortOrder};
use inillucent_sql::bind::WindowCall as BoundWindowCall;
use inillucent_sql::function::AggregateFunc;

use crate::aggregate::AggregateKind;
use crate::expr::{compile, Expr, StaticType};
use crate::join::ValuesScan;
use crate::ops::{CollectInto, Distinct, Limit, Project, Sink, Sort, SortKey};
use crate::physical::{
    constant_count, distinct_collations, expression_collation, prepare, run_prepared, translate,
    unsupported, ForcePlan, Frame, HeldSpace, Negative, Params, Shape, TreeCatalog,
};
use crate::window::{
    FrameEnd, OrderTerm as WindowOrderTerm, WindowCall, WindowFrame, WindowPlan, WindowSlot,
};

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
    // Every uncorrelated subquery is answered once, here, before anything is
    // built over it. See `crate::subquery` for why it is per execution.
    let folded = crate::subquery::fold(plan, catalog, params)?;
    let params = folded.as_ref().unwrap_or(params);
    let select = &plan.select;
    if !select.compounds.is_empty() {
        return unsupported("a window function in a compound arm");
    }
    let Some(first) = select.windows.first() else {
        return unsupported("a window pass with no window in it");
    };
    // **One pass per distinct window frame, not one pass per statement.** Two
    // calls that share a `PARTITION BY` and an `ORDER BY` see the same
    // partitions and the same peer groups, so they are computed together over
    // one ordering; two that do not need the rows in two different orders and
    // there is no single sort that serves both. Refusing the second shape was
    // honest while there was one pass; grouping the calls is what makes it
    // unnecessary.
    let groups = window_groups(select);
    let pre = window_inputs(select);
    let rows = window_input_rows(select, &pre, first, catalog, params)?;
    // The output row is the buffered values followed by one slot per call, in
    // the order the binder numbered them - which is the space
    // `Frame::Window` addresses and the reason the slots are filled by
    // scattering rather than by appending.
    let width = pre.len();
    // **A window pass holds more than its input, and the budget saw only the
    // input (task-1932, H6).** `window_input_rows` runs an inner query whose
    // `Collect` sink charges the rows it buffers, and then this function builds
    // a widened copy of every one of them - and, below, one re-sorted copy per
    // window frame after the first. Three windows in different orders over a
    // large table is five copies of it in memory. Each is charged where it is
    // made rather than estimated up front, because the number of copies is
    // decided by the statement rather than by the data.
    inillucent_base::budget::materialise(owned_rows_bytes(&rows))?;
    let mut widened: Vec<Vec<OwnedDatum>> = rows
        .iter()
        .map(|row| {
            let mut whole = row.clone();
            whole.extend(std::iter::repeat_n(OwnedDatum::Null, select.windows.len()));
            whole
        })
        .collect();
    for (position, group) in groups.iter().enumerate() {
        // The first group's ordering is the one the inner query already sorted
        // by, so it is not sorted again; every other group needs the rows in
        // its own order.
        let ordered = if position == 0 {
            tagged(&rows)
        } else {
            // A second frame needs the rows in a second order, which is a
            // second whole copy of them.
            inillucent_base::budget::materialise(owned_rows_bytes(&rows))?;
            sort_tagged(tagged(&rows), &pre, group)?
        };
        let pass = window_plan(select, &pre, group)?;
        let computed = crate::window::compute(&ordered, &pass)?;
        for row in &computed {
            let Some(OwnedDatum::Int(at)) = row.get(width) else {
                return Err(misuse("a window pass lost the row it was computing for"));
            };
            let at = *at as usize;
            for (nth, slot) in group.slots.iter().enumerate() {
                let value = row
                    .get(width.saturating_add(1).saturating_add(nth))
                    .cloned()
                    .unwrap_or(OwnedDatum::Null);
                if let Some(cell) = widened
                    .get_mut(at)
                    .and_then(|row| row.get_mut(width.saturating_add(*slot)))
                {
                    *cell = value;
                }
            }
        }
    }
    project_over_window(select, &pre, width, widened, params)
}

/// Returns roughly how many bytes a buffer of owned rows holds.
///
/// @param rows - the buffer to measure
fn owned_rows_bytes(rows: &[Vec<OwnedDatum>]) -> u64 {
    rows.iter()
        .map(|row| crate::ops::owned_row_bytes(row))
        .fold(0u64, u64::saturating_add)
}

/// One set of window calls that share a frame.
struct WindowGroup {
    /// The `PARTITION BY` and `ORDER BY` every call in the group shares.
    window: BoundWindow,
    /// Which of `select.windows` the group holds, by the binder's slot.
    slots: Vec<usize>,
}

/// Groups a statement's window calls by the frame they share.
///
/// @param select - the bound statement
fn window_groups(select: &BoundSelect) -> Vec<WindowGroup> {
    let mut groups: Vec<WindowGroup> = Vec::new();
    for (slot, call) in select.windows.iter().enumerate() {
        match groups.iter_mut().find(|group| {
            group.window.partition_by == call.partition_by && group.window.order_by == call.order_by
        }) {
            Some(group) => group.slots.push(slot),
            None => groups.push(WindowGroup {
                window: call.clone(),
                slots: vec![slot],
            }),
        }
    }
    groups
}

/// Returns the rows with their original position appended.
///
/// The position is what lets a pass over a *re-sorted* copy write its answers
/// back into the row they belong to. It sits after the buffered values, where
/// `window_plan` addresses nothing, so no pass can read it by accident.
///
/// @param rows - the buffered rows, in their original order
fn tagged(rows: &[Vec<OwnedDatum>]) -> Vec<Vec<OwnedDatum>> {
    rows.iter()
        .enumerate()
        .map(|(at, row)| {
            let mut whole = row.clone();
            whole.push(OwnedDatum::Int(at as i64));
            whole
        })
        .collect()
}

/// Sorts tagged rows into one window group's own order.
///
/// @param rows - the tagged rows
/// @param pre - the buffered row's expressions
/// @param group - the group whose frame decides the order
fn sort_tagged(
    rows: Vec<Vec<OwnedDatum>>,
    pre: &[BoundExpr],
    group: &WindowGroup,
) -> DbResult<Vec<Vec<OwnedDatum>>> {
    let mut keys: Vec<SortKey> = Vec::new();
    // A partition key only has to bring a partition's rows together, so its
    // direction is free; the ordering terms are the window's own and are not.
    for expr in &group.window.partition_by {
        keys.push(SortKey {
            column: column_of(pre, expr)?,
            descending: false,
            collation: expression_collation(expr),
            nulls_first: true,
        });
    }
    for term in &group.window.order_by {
        keys.push(SortKey {
            column: column_of(pre, &term.expr)?,
            descending: term.order == SortOrder::Descending,
            collation: term.collation,
            nulls_first: term.nulls == NullOrder::First,
        });
    }
    if keys.is_empty() {
        return Ok(rows);
    }
    let collected = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
    let mut sorter = Sort::new(
        keys,
        Box::new(CollectInto::new(std::rc::Rc::clone(&collected))),
    );
    ValuesScan::new(rows).run(&mut sorter)?;
    let answer = collected.borrow().clone();
    Ok(answer)
}

/// Returns the values a window pass reads, in buffered-row order.
///
/// The partition keys and the window's ordering come first, because the inner
/// query is ordered by them and reading them out of the same columns it sorted
/// by is one fewer thing to keep in step. Everything else is appended as it is
/// met, deduplicated, so a value used twice occupies one column.
///
/// @param select - the bound statement
fn window_inputs(select: &BoundSelect) -> Vec<BoundExpr> {
    let mut pre: Vec<BoundExpr> = Vec::new();
    // The *first* window's frame first, because the inner query is sorted by it
    // and reading it out of the same columns it sorted by is one fewer thing to
    // keep in step. Every other window's frame is gathered below with the rest.
    if let Some(first) = select.windows.first() {
        for expr in &first.partition_by {
            remember(&mut pre, expr);
        }
        for term in &first.order_by {
            remember(&mut pre, &term.expr);
        }
    }
    for call in &select.windows {
        for expr in &call.partition_by {
            remember(&mut pre, expr);
        }
        for term in &call.order_by {
            remember(&mut pre, &term.expr);
        }
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
        // An aggregate is a leaf here for the same reason a column is: the
        // inner query computes it, and what the projection above the pass reads
        // is the value rather than the call. Without this a statement that
        // aggregates *and* windows lost its `count(*)` between the two passes,
        // which is why the two used to be refused together.
        BoundExpr::Column { .. } | BoundExpr::Rowid { .. } | BoundExpr::Aggregate { .. } => {
            remember(pre, expr)
        }
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

/// Builds one window pass, addressing everything by buffered column.
///
/// A pass covers exactly the calls that share a frame, which is what
/// `window_groups` decided: the partitions and the peer groups are properties
/// of the frame, so calls with different ones cannot be computed over one
/// ordering of the rows.
///
/// @param select - the bound statement
/// @param pre - the buffered row's expressions
/// @param group - the calls sharing one frame, and the frame
fn window_plan(
    select: &BoundSelect,
    pre: &[BoundExpr],
    group: &WindowGroup,
) -> DbResult<WindowPlan> {
    let window = &group.window;
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
    let mut calls = Vec::with_capacity(group.slots.len());
    for call in group
        .slots
        .iter()
        .filter_map(|slot| select.windows.get(*slot))
    {
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
