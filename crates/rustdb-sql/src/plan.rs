//! The logical and physical plans.
//!
//! Invariant: a physical plan is *legal* before it is fast. Every access path
//! this planner produces returns exactly the rows a full scan of the same term
//! would return, and every predicate a path consumes is either fully enforced
//! by the path or left in the residual filter. A predicate that is neither is a
//! wrong answer, so the two lists are built together and the compiler emits
//! whatever is left over.
//!
//! The phase-6 planner is deliberately minimal: FROM terms stay in written
//! order, joins are nested loops, and the only paths are a full scan, a rowid
//! lookup or range, and an index seek over an equality prefix with an optional
//! range on the column after it. Cost is not modelled yet; a path is chosen
//! because it is more selective by construction, not because a number said so.

use rustdb_value::Collation;

use crate::ast::{BinaryOp, CompoundOp, JoinKind};
use crate::bind::{BoundExpr, BoundSelect, BoundSource, ColumnUse, SourceRows};
use crate::catalog_view::{IndexInfo, TableInfo};
use crate::cost;

/// A comparison an access path can enforce.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BoundKind {
    /// `>=`
    GreaterEqual,
    /// `>`
    Greater,
    /// `<=`
    LessEqual,
    /// `<`
    Less,
}

/// One end of a scan range.
#[derive(Clone, Debug, PartialEq)]
pub struct RangeBound {
    /// Which comparison the bound enforces.
    pub kind: BoundKind,
    /// The value to compare against.
    pub value: BoundExpr,
}

/// How one FROM term's rows are produced.
#[derive(Clone, Debug, PartialEq)]
pub enum AccessPath {
    /// Every row of the table, in rowid order.
    TableScan {
        /// The table B-tree's root page.
        root: u32,
    },
    /// One row, found by rowid.
    RowidSeek {
        /// The table B-tree's root page.
        root: u32,
        /// The rowid to look up.
        key: BoundExpr,
    },
    /// A contiguous run of rows, by rowid.
    RowidRange {
        /// The table B-tree's root page.
        root: u32,
        /// The lower bound, when there is one.
        low: Option<RangeBound>,
        /// The upper bound, when there is one.
        high: Option<RangeBound>,
    },
    /// Rows found through an index, then fetched from the table.
    IndexSeek {
        /// The table B-tree's root page.
        table_root: u32,
        /// The index B-tree's root page.
        index_root: u32,
        /// The index's name, for the plan description.
        index_name: Vec<u8>,
        /// The equality prefix, one value per leading index column.
        equalities: Vec<BoundExpr>,
        /// A range on the column after the equality prefix.
        low: Option<RangeBound>,
        /// The upper end of that range.
        high: Option<RangeBound>,
        /// The collation of each index column used, in order.
        collations: Vec<Collation>,
        /// Whether the index columns used are stored descending.
        descending: Vec<bool>,
        /// Which table column each index column holds.
        columns: Vec<u16>,
        /// Whether the table has no rowid, so the index key holds the key.
        without_rowid: bool,
        /// Where in each entry the row's primary key sits, for a `WITHOUT
        /// ROWID` table read through a *secondary* index.
        ///
        /// Such an entry ends with the primary key where a rowid table's would
        /// end with a rowid, and that is how the row is then found. Empty for a
        /// rowid table, and empty when the index is the table's own key - then
        /// the entry the seek landed on already is the row.
        key_entry_slots: Vec<usize>,
        /// Where in the index entry every column the query reads sits, when the
        /// index holds all of them.
        ///
        /// An index entry is the indexed columns followed by the row's key, so
        /// a query that reads only those columns never has to go to the table
        /// at all - which halves the descents and, on a range, is the whole
        /// difference between a search and a scan. `None` means the query needs
        /// something the entry does not carry, and the row is fetched.
        ///
        /// The pairs are `(record slot in the table, slot in the index entry)`.
        /// The rowid is not in the list: it is always the entry's last field
        /// for a rowid table, and the compiler reads it with `IdxRowid`.
        covering: Option<Vec<(u16, usize)>>,
    },
    /// Rows produced by a nested query, materialised and then scanned.
    Subquery {
        /// The plan that fills the store.
        plan: Box<PhysicalPlan>,
        /// How many columns a materialised row holds.
        width: usize,
        /// Whether the nested block reads a FROM term outside itself, and so
        /// has to be rebuilt for every row of the query that encloses it.
        correlated: bool,
    },
    /// Rows produced by a recursive CTE, filled by walking its own queue.
    Recursive {
        /// The arms that do not reference the CTE, in order.
        seeds: Vec<(CompoundOp, PhysicalPlan)>,
        /// The arms that do.
        steps: Vec<(CompoundOp, PhysicalPlan)>,
        /// How many columns a row holds.
        width: usize,
    },
    /// The one row of a recursive CTE's queue the fill loop is on.
    RecursiveSelf {
        /// The FROM term whose store holds the queue.
        cte: usize,
    },
    /// Rows produced by a virtual table's module.
    VirtualScan {
        /// The module and the arguments its `CREATE` gave it.
        module: crate::vtab::ModuleRef,
        /// The constraints offered to `best_index`, in the order the module
        /// will see them.
        offer: Vec<VirtualConstraint>,
        /// The ordering offered to `best_index`.
        order_by: Vec<crate::vtab::OrderSpec>,
        /// What the module answered, once it has been asked.
        ///
        /// It is `None` while the plan is still the planner's, and filled in by
        /// a pass that runs before compilation. Keeping the two apart is what
        /// lets the planner stay a pure function of the SQL and one catalog
        /// generation while the program still carries a real plan.
        chosen: Option<VirtualChoice>,
    },
}

/// What a module answered when it was shown the offer.
#[derive(Clone, Debug, PartialEq)]
pub struct VirtualChoice {
    /// The plan number, passed back to the module's `filter`.
    pub index_number: i32,
    /// The plan string, passed back to the module's `filter`.
    pub index_string: String,
    /// The offer positions whose values feed `filter`, in argument order.
    pub arguments: Vec<usize>,
    /// The offer positions the engine must still test for itself.
    ///
    /// Everything the module did not take, and everything it took without
    /// promising to apply. A module that says `omit` is promising; anything
    /// else and the predicate is tested twice, which is the safe direction.
    pub recheck: Vec<usize>,
    /// Whether the module will produce the requested order by itself.
    pub ordered: bool,
}

/// One predicate offered to a module, with what it was made of.
///
/// The predicate is kept whole beside the constraint because the compiler may
/// have to test it after all: a module that used the constraint without
/// promising to apply it leaves the engine responsible for the answer.
#[derive(Clone, Debug, PartialEq)]
pub struct VirtualConstraint {
    /// The constraint as the module is shown it.
    pub spec: crate::vtab::ConstraintSpec,
    /// The value on the other side, which becomes an argument to `filter`.
    pub value: BoundExpr,
    /// The whole predicate, for the compiler to re-test when it must.
    pub predicate: BoundExpr,
}

impl AccessPath {
    /// Returns a one-line description, which is what `EXPLAIN QUERY PLAN`
    /// renders and what a performance test asserts on.
    pub fn describe(&self, table: &str) -> String {
        match self {
            AccessPath::TableScan { .. } => format!("SCAN {table}"),
            AccessPath::RowidSeek { .. } => {
                format!("SEARCH {table} USING INTEGER PRIMARY KEY (rowid=?)")
            }
            AccessPath::RowidRange { .. } => {
                format!("SEARCH {table} USING INTEGER PRIMARY KEY (rowid>?)")
            }
            AccessPath::Recursive { .. } => format!("SCAN {table} USING RECURSIVE QUEUE"),
            AccessPath::RecursiveSelf { .. } => format!("SCAN {table}"),
            AccessPath::VirtualScan { .. } => format!("SCAN {table} VIRTUAL TABLE INDEX"),
            AccessPath::Subquery { correlated, .. } => {
                if *correlated {
                    format!("CORRELATED SCALAR SUBQUERY {table}")
                } else {
                    format!("SCAN {table}")
                }
            }
            AccessPath::IndexSeek {
                index_name,
                equalities,
                low,
                high,
                covering,
                ..
            } => {
                if equalities.is_empty() && low.is_none() && high.is_none() {
                    return format!(
                        "SCAN {table} USING COVERING INDEX {}",
                        String::from_utf8_lossy(index_name)
                    );
                }
                let kind = if covering.is_some() {
                    "COVERING INDEX"
                } else {
                    "INDEX"
                };
                let mut detail = String::new();
                for index in 0..equalities.len() {
                    if index > 0 {
                        detail.push_str(" AND ");
                    }
                    detail.push_str("?=?");
                }
                if low.is_some() || high.is_some() {
                    if !detail.is_empty() {
                        detail.push_str(" AND ");
                    }
                    detail.push_str("?>?");
                }
                format!(
                    "SEARCH {table} USING {kind} {} ({detail})",
                    String::from_utf8_lossy(index_name)
                )
            }
        }
    }
}

/// One FROM term with the path chosen for it.
#[derive(Clone, Debug, PartialEq)]
pub struct PlannedSource {
    /// What the planner estimated this term's path would cost.
    ///
    /// It is kept so that a test can assert on the *reason* a plan was chosen
    /// rather than only on the plan, which is the difference between catching a
    /// cost-model regression and catching it two releases later.
    pub cost: f64,
    /// How many rows the path is estimated to produce.
    pub rows: f64,
    /// The statement-wide number every bound expression refers to it by.
    pub id: usize,
    /// The table.
    pub table: TableInfo,
    /// The name the query calls it.
    pub alias: Vec<u8>,
    /// How its rows are produced.
    pub path: AccessPath,
    /// The join that attached it to the term before it.
    pub join: JoinKind,
    /// The `ON` condition, when the join is an outer one.
    ///
    /// An inner join's condition is an ordinary predicate and is distributed
    /// with the rest; an outer join's is not, because a row that fails it is
    /// still emitted, null-extended. Keeping it here rather than in the
    /// residual list is what stops the two being confused.
    pub on: Option<BoundExpr>,
}

/// How the rows are grouped and aggregated.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AggregationMode {
    /// No aggregation at all.
    None,
    /// One group for the whole input, which produces exactly one row.
    Whole,
    /// One group per distinct `GROUP BY` key, produced by sorting first.
    Grouped,
}

/// A physical plan for a read-only statement.
#[derive(Clone, Debug, PartialEq)]
pub struct PhysicalPlan {
    /// The FROM terms, in the order the nested loops visit them.
    pub sources: Vec<PlannedSource>,
    /// The predicates the loops must still evaluate, one per nesting level.
    ///
    /// A predicate is attached to the innermost term it reads, so it is tested
    /// as soon as it can be rather than after every loop has been entered.
    pub residuals: Vec<Option<BoundExpr>>,
    /// A predicate over no columns at all, tested once before the loops.
    pub constant_filter: Option<BoundExpr>,
    /// The bound statement the plan came from.
    pub select: BoundSelect,
    /// How the rows are aggregated.
    pub aggregation: AggregationMode,
    /// Whether the results have to pass through a sorter.
    pub needs_sort: bool,
    /// The later arms of a compound, each with the operator that joined it.
    pub compounds: Vec<(CompoundOp, PhysicalPlan)>,
}

impl PhysicalPlan {
    /// Returns the highest statement-wide source id anywhere in the plan.
    ///
    /// The compiler sizes its cursor map from this, so a nested block's cursor
    /// has a slot before the block that encloses it is compiled.
    pub fn max_source_id(&self) -> usize {
        let mut highest = 0usize;
        for source in &self.sources {
            highest = highest.max(source.id);
            match &source.path {
                AccessPath::Subquery { plan, .. } => {
                    highest = highest.max(plan.max_source_id());
                }
                AccessPath::Recursive { seeds, steps, .. } => {
                    for (_, arm) in seeds.iter().chain(steps.iter()) {
                        highest = highest.max(arm.max_source_id());
                    }
                }
                _ => {}
            }
        }
        for (_, arm) in &self.compounds {
            highest = highest.max(arm.max_source_id());
        }
        highest
    }

    /// Returns the `EXPLAIN QUERY PLAN` lines this plan renders as.
    pub fn describe(&self) -> Vec<String> {
        let mut lines = Vec::new();
        for source in &self.sources {
            lines.push(
                source
                    .path
                    .describe(&String::from_utf8_lossy(&source.alias)),
            );
        }
        for (op, arm) in &self.compounds {
            lines.push(format!("COMPOUND QUERY {}", compound_name(*op)));
            lines.extend(arm.describe());
        }
        if self.aggregation == AggregationMode::Grouped {
            lines.push("USE TEMP B-TREE FOR GROUP BY".to_string());
        }
        if self.needs_sort {
            lines.push("USE TEMP B-TREE FOR ORDER BY".to_string());
        }
        if self.select.distinct {
            lines.push("USE TEMP B-TREE FOR DISTINCT".to_string());
        }
        lines
    }
}

/// Returns the word `EXPLAIN QUERY PLAN` names a compound operator by.
fn compound_name(op: CompoundOp) -> &'static str {
    match op {
        CompoundOp::Union => "UNION",
        CompoundOp::UnionAll => "UNION ALL",
        CompoundOp::Intersect => "INTERSECT",
        CompoundOp::Except => "EXCEPT",
    }
}

/// Plans a bound SELECT, and every block nested inside it.
///
/// The predicate list is split before any path is chosen, because a path can
/// only consume a term of a conjunction and the rest has to be kept. An outer
/// join's `ON` condition is deliberately *not* in that list: a row that fails
/// it is still emitted, null-extended, so treating it as a filter would drop
/// exactly the rows the join exists to keep.
pub fn plan_select(select: BoundSelect) -> PhysicalPlan {
    let mut select = select;
    let compound_arms = core::mem::take(&mut select.compounds);
    let mut terms = Vec::new();
    if let Some(filter) = &select.filter {
        split_conjunction(filter, &mut terms);
    }
    for source in &select.sources {
        if is_outer(source.join) {
            continue;
        }
        if let Some(constraint) = &source.constraint {
            split_conjunction(constraint, &mut terms);
        }
    }
    // The order the terms are visited in is chosen before their paths are, and
    // then the paths are chosen in that order - because a path may use a value
    // from a term visited earlier, and which terms those are is exactly what the
    // order decides.
    let order = choose_order(&select, &terms);
    let ordered: Vec<usize> = order.clone();
    let ids: Vec<usize> = ordered
        .iter()
        .filter_map(|position| select.sources.get(*position))
        .map(|source| source.id)
        .collect();
    let mut consumed = vec![false; terms.len()];
    let mut sources = Vec::with_capacity(select.sources.len());
    for (level, position) in ordered.iter().enumerate() {
        let Some(source) = select.sources.get(*position) else {
            continue;
        };
        let path = choose_path(level, &ids, source, &select, &terms, &mut consumed);
        let (cost, rows) = path_cost(source, &path);
        sources.push(PlannedSource {
            cost,
            rows,
            id: source.id,
            table: source.table.clone(),
            alias: source.alias.clone(),
            path,
            join: source.join,
            on: is_outer(source.join)
                .then(|| source.constraint.clone())
                .flatten(),
        });
    }
    let (residuals, constant_filter) = distribute_residuals(&terms, &consumed, &ids);
    let aggregation = if !select.group_by.is_empty() {
        AggregationMode::Grouped
    } else if !select.aggregates.is_empty() {
        AggregationMode::Whole
    } else {
        AggregationMode::None
    };
    let needs_sort = !select.order_by.is_empty();
    let compounds = compound_arms
        .into_iter()
        .map(|(op, arm)| (op, plan_select(arm)))
        .collect();
    PhysicalPlan {
        sources,
        residuals,
        constant_filter,
        select,
        aggregation,
        needs_sort,
        compounds,
    }
}

/// Returns the order the FROM terms are visited in.
///
/// The legality rule is the whole of the difficulty. An outer join's rows
/// depend on the terms it was written against: a `LEFT JOIN` cannot be visited
/// before the term it null-extends, and neither side of one can cross it. A
/// `CROSS JOIN` is SQLite's documented instruction not to reorder at all. So a
/// term may only move within the run of ordinary joins it belongs to, and the
/// enumeration is over those runs rather than over the whole list.
///
/// Inside a run the search is exhaustive while that is affordable - the runs
/// that occur in practice are two to five terms - and falls back to the written
/// order beyond, because a greedy answer that is worse than the written order
/// is worse than not reordering at all.
fn choose_order(select: &BoundSelect, terms: &[BoundExpr]) -> Vec<usize> {
    let count = select.sources.len();
    if count < 2 {
        return (0..count).collect();
    }
    let mut order = Vec::with_capacity(count);
    let mut run: Vec<usize> = Vec::new();
    for position in 0..count {
        let pins = select
            .sources
            .get(position)
            .is_some_and(|source| matches!(source.join, JoinKind::Cross) || is_outer(source.join));
        if pins {
            order.extend(best_order(select, terms, &run));
            run.clear();
            order.push(position);
            continue;
        }
        run.push(position);
    }
    order.extend(best_order(select, terms, &run));
    order
}

/// Returns the cheapest visiting order for one run of reorderable terms.
fn best_order(select: &BoundSelect, terms: &[BoundExpr], run: &[usize]) -> Vec<usize> {
    // Eight terms is 40,320 orders, which is milliseconds; beyond that the
    // written order stands rather than a guess being substituted for it.
    if run.len() < 2 || run.len() > 8 {
        return run.to_vec();
    }
    let mut best: Option<(f64, Vec<usize>)> = None;
    let mut candidate = run.to_vec();
    permute(&mut candidate, 0, &mut |order| {
        let cost = order_cost(select, terms, order);
        let better = best
            .as_ref()
            .is_none_or(|(existing, _)| cost < *existing - 1e-9);
        if better {
            best = Some((cost, order.to_vec()));
        }
    });
    best.map(|(_, order)| order).unwrap_or_else(|| run.to_vec())
}

/// Calls a closure with every permutation of a slice.
fn permute(order: &mut Vec<usize>, at: usize, visit: &mut impl FnMut(&[usize])) {
    if at >= order.len() {
        visit(order);
        return;
    }
    for index in at..order.len() {
        order.swap(at, index);
        permute(order, at.saturating_add(1), visit);
        order.swap(at, index);
    }
}

/// Returns what one visiting order is estimated to cost.
///
/// The loops are nested, so each term's cost is multiplied by the rows every
/// term before it produced - which is the whole reason the order matters, and
/// why putting the most selective term first is usually right and sometimes
/// spectacularly wrong.
fn order_cost(select: &BoundSelect, terms: &[BoundExpr], order: &[usize]) -> f64 {
    let ids: Vec<usize> = order
        .iter()
        .filter_map(|position| select.sources.get(*position))
        .map(|source| source.id)
        .collect();
    let mut consumed = vec![false; terms.len()];
    let mut total = 0.0f64;
    let mut outer_rows = 1.0f64;
    for (level, position) in order.iter().enumerate() {
        let Some(source) = select.sources.get(*position) else {
            continue;
        };
        let path = choose_path(level, &ids, source, select, terms, &mut consumed);
        let (cost, rows) = path_cost(source, &path);
        total += outer_rows * cost;
        outer_rows *= rows.max(1.0);
    }
    total
}

/// Returns what one term's path costs, and how many rows it produces.
fn path_cost(source: &BoundSource, path: &AccessPath) -> (f64, f64) {
    let rows = estimated_rows(&source.table);
    match path {
        AccessPath::TableScan { .. } => (cost::scan_cost(rows), rows),
        // A module prices its own scan, and the planner cannot ask it here
        // without making the plan depend on run-time state. What it can do is
        // reward an offer: a virtual table that was given a constraint will be
        // cheaper than one that was not, whatever the module then says.
        AccessPath::VirtualScan { offer, .. } => {
            let usable = offer.iter().filter(|item| item.spec.usable).count();
            let rows = if usable == 0 {
                rows
            } else {
                rows / (usable as f64 * 8.0)
            };
            (cost::scan_cost(rows.max(1.0)), rows.max(1.0))
        }
        AccessPath::RowidSeek { .. } => (cost::search_cost(rows, 1.0, true), 1.0),
        AccessPath::RowidRange { .. } => {
            let matches = (rows / cost::RANGE_SHARE).max(1.0);
            (cost::search_cost(rows, matches, true), matches)
        }
        AccessPath::IndexSeek {
            index_name,
            equalities,
            low,
            high,
            covering,
            ..
        } => {
            let index = source
                .table
                .indexes
                .iter()
                .find(|candidate| candidate.name == *index_name);
            let matches = index_matches(
                index,
                rows,
                equalities.len(),
                low.is_some() || high.is_some(),
            );
            let Some(index) = index else {
                return (cost::search_cost(rows, matches, false), matches);
            };
            if covering.is_none() {
                return (cost::search_cost(rows, matches, false), matches);
            }
            // A covering path reads entries rather than rows, and an entry is
            // the indexed columns plus the key rather than the whole row. Cost
            // is bytes touched, so the narrower shape is the saving - and it is
            // the whole reason a covering scan of a two-column index beats a
            // table scan of a five-column table when there is no predicate at
            // all to narrow either of them.
            let width = cost::entry_share(index.columns.len(), source.table.columns.len());
            (cost::search_cost(rows, matches * width, true), matches)
        }
        // A materialised term is built once and then scanned; the build is
        // charged where it happens, which is the block that fills it.
        AccessPath::Subquery { .. } | AccessPath::Recursive { .. } => (cost::scan_cost(rows), rows),
        AccessPath::RecursiveSelf { .. } => (1.0, 1.0),
    }
}

/// Returns how many rows a table is estimated to hold.
fn estimated_rows(table: &TableInfo) -> f64 {
    match table.analysed_rows {
        Some(rows) if rows > 0 => rows as f64,
        // A measured zero is a real answer, and so is an unmeasured table: the
        // first is empty and the second is assumed large. Collapsing them would
        // make an `ANALYZE` on an empty table look like no `ANALYZE` at all.
        Some(_) => 1.0,
        None => cost::DEFAULT_ROWS,
    }
}

/// Returns how many rows an index search is estimated to return.
fn index_matches(index: Option<&IndexInfo>, rows: f64, equalities: usize, ranged: bool) -> f64 {
    let mut matches = match index {
        // Measured: the average number of rows sharing the prefix the search
        // pinned down. This is the number `ANALYZE` exists to supply.
        Some(index) if !index.prefix_rows.is_empty() && equalities > 0 => index
            .prefix_rows
            .get(equalities.saturating_sub(1))
            .copied()
            .map(|value| value as f64)
            .unwrap_or(rows),
        // Unmeasured: a unique index pins one row, and every other equality is
        // assumed to select a tenth.
        Some(index) if index.unique && equalities >= index.columns.len() => 1.0,
        _ => {
            let mut estimate = rows;
            for _ in 0..equalities {
                estimate /= cost::EQUALITY_SHARE;
            }
            estimate
        }
    };
    if ranged {
        matches /= cost::RANGE_SHARE;
    }
    matches.max(1.0)
}

/// Returns whether a join keeps rows that match nothing on the other side.
pub fn is_outer(join: JoinKind) -> bool {
    matches!(join, JoinKind::Left | JoinKind::Right | JoinKind::Full)
}

/// Splits `a AND b AND c` into its terms.
///
/// Only `AND` is split. Splitting an `OR` would produce terms that are not
/// individually true of every row the expression accepts, which is the classic
/// way to lose rows.
pub fn split_conjunction(expr: &BoundExpr, into: &mut Vec<BoundExpr>) {
    match expr {
        BoundExpr::And(left, right) => {
            split_conjunction(left, into);
            split_conjunction(right, into);
        }
        // `x BETWEEN a AND b` *is* `x >= a AND x <= b`, so splitting it lets an
        // index range be found where otherwise the whole thing sat in the
        // residual and the table was scanned. It is split only when `x` is a
        // column, which is both the case that can drive an index and the case
        // where evaluating the operand twice cannot change an answer: a
        // volatile expression tested twice is a different question.
        BoundExpr::Between {
            negated: false,
            operand,
            low,
            high,
            affinity,
            collation,
        } if matches!(
            **operand,
            BoundExpr::Column { .. } | BoundExpr::Rowid { .. }
        ) =>
        {
            into.push(BoundExpr::Compare {
                op: BinaryOp::GreaterEqual,
                left: operand.clone(),
                right: low.clone(),
                affinity: *affinity,
                collation: *collation,
            });
            into.push(BoundExpr::Compare {
                op: BinaryOp::LessEqual,
                left: operand.clone(),
                right: high.clone(),
                affinity: *affinity,
                collation: *collation,
            });
        }
        other => into.push(other.clone()),
    }
}

/// Attaches each unconsumed predicate to the innermost term it reads.
///
/// A predicate that reads only FROM terms belonging to an *enclosing* block is
/// constant for the whole of this block: the outer cursors are positioned
/// before it starts and do not move while it runs, so it is tested once before
/// the loops rather than once per row.
fn distribute_residuals(
    terms: &[BoundExpr],
    consumed: &[bool],
    ids: &[usize],
) -> (Vec<Option<BoundExpr>>, Option<BoundExpr>) {
    let levels = ids.len();
    let mut residuals: Vec<Option<BoundExpr>> = vec![None; levels];
    let mut constant: Option<BoundExpr> = None;
    for (index, term) in terms.iter().enumerate() {
        if consumed.get(index).copied().unwrap_or(false) {
            continue;
        }
        let mut used = Vec::new();
        term.sources_used(&mut used);
        let level = used
            .iter()
            .filter_map(|source| ids.iter().position(|id| id == source))
            .max();
        match level {
            Some(level) if level < levels => {
                if let Some(slot) = residuals.get_mut(level) {
                    *slot = Some(match slot.take() {
                        Some(existing) => {
                            BoundExpr::And(Box::new(existing), Box::new(term.clone()))
                        }
                        None => term.clone(),
                    });
                }
            }
            _ => {
                constant = Some(match constant.take() {
                    Some(existing) => BoundExpr::And(Box::new(existing), Box::new(term.clone())),
                    None => term.clone(),
                });
            }
        }
    }
    (residuals, constant)
}

/// Chooses the access path for one FROM term.
fn choose_path(
    position: usize,
    ids: &[usize],
    source: &BoundSource,
    select: &BoundSelect,
    terms: &[BoundExpr],
    consumed: &mut [bool],
) -> AccessPath {
    match &source.rows {
        SourceRows::Subquery(block) => {
            let width = block.columns.len();
            let correlated = !block.correlations.is_empty();
            return AccessPath::Subquery {
                plan: Box::new(plan_select((**block).clone())),
                width,
                correlated,
            };
        }
        SourceRows::Recursive(body) => {
            let width = body.seeds.first().map_or(0, |(_, arm)| arm.columns.len());
            return AccessPath::Recursive {
                seeds: body
                    .seeds
                    .iter()
                    .map(|(op, arm)| (*op, plan_select(arm.clone())))
                    .collect(),
                steps: body
                    .steps
                    .iter()
                    .map(|(op, arm)| (*op, plan_select(arm.clone())))
                    .collect(),
                width,
            };
        }
        SourceRows::RecursiveSelf { cte } => {
            return AccessPath::RecursiveSelf { cte: *cte };
        }
        SourceRows::Table => {}
    }
    let id = ids.get(position).copied().unwrap_or(position);
    let table = &source.table;
    if let Some(module) = table.module.clone() {
        return virtual_path(id, position, ids, source, select, module, terms, consumed);
    }
    let mut trial = consumed.to_vec();
    if let Some(path) = rowid_path(id, position, ids, table, terms, &mut trial) {
        consumed.copy_from_slice(&trial);
        return path;
    }
    // The candidate is built against a *copy* of the consumed list, because a
    // path that is not chosen must not leave its predicates marked as handled.
    // It did: when a scan beat an index range, the range's own comparison had
    // already been struck off the residual list and the scan then returned
    // every row of the table, silently.
    let mut trial = consumed.to_vec();
    let needed = select.columns_read(id);
    if let Some(path) = index_path(id, position, ids, source, terms, &mut trial, &needed) {
        // A scan beats a search that returns most of the table: an index that
        // has to fetch every row costs a second descent per row on top of the
        // scan it was meant to avoid.
        let (index_cost, _) = path_cost(source, &path);
        let (scan_cost, _) = path_cost(source, &AccessPath::TableScan { root: table.root });
        if index_cost <= scan_cost {
            consumed.copy_from_slice(&trial);
            return path;
        }
    }
    AccessPath::TableScan { root: table.root }
}

/// Builds the offer a virtual table's module will be shown.
///
/// Every predicate that compares one of this term's columns - or its rowid - to
/// something is offered, whether or not the value is available yet: a
/// constraint the loop order has put out of reach is offered as *not usable*,
/// which is what lets one answer serve every position the term could take.
fn virtual_path(
    id: usize,
    position: usize,
    ids: &[usize],
    source: &BoundSource,
    select: &BoundSelect,
    module: crate::vtab::ModuleRef,
    terms: &[BoundExpr],
    consumed: &mut [bool],
) -> AccessPath {
    let table = &source.table;
    let mut offer = Vec::new();
    for (index, term) in terms.iter().enumerate() {
        if consumed.get(index).copied().unwrap_or(false) {
            continue;
        }
        let Some((column, op, value)) = virtual_constraint(id, table, term) else {
            continue;
        };
        offer.push(VirtualConstraint {
            spec: crate::vtab::ConstraintSpec {
                column,
                op,
                usable: is_available(position, ids, &value),
            },
            value,
            predicate: term.clone(),
        });
        if let Some(slot) = consumed.get_mut(index) {
            *slot = true;
        }
    }
    let order_by = order_offer(id, position, select);
    AccessPath::VirtualScan {
        module,
        offer,
        order_by,
        chosen: None,
    }
}

/// Returns the `ORDER BY` a module may be able to satisfy for itself.
///
/// Only the outermost loop is offered one. An inner loop restarts for every row
/// of the loops around it, so an ordering it produced would be an ordering
/// within each of those restarts - which is not the statement's ordering and
/// would let the sorter be skipped wrongly.
fn order_offer(id: usize, position: usize, select: &BoundSelect) -> Vec<crate::vtab::OrderSpec> {
    if position != 0 {
        return Vec::new();
    }
    let mut offer = Vec::new();
    for term in &select.order_by {
        let column = match &term.expr {
            BoundExpr::Column { source, column, .. } if *source == id => i32::from(*column),
            BoundExpr::Rowid { source } if *source == id => crate::vtab::ROWID_COLUMN,
            _ => return Vec::new(),
        };
        offer.push(crate::vtab::OrderSpec {
            column,
            descending: term.order == crate::ast::SortOrder::Descending,
        });
    }
    offer
}

/// Builds the offer a module is shown for one term and one predicate list.
///
/// The write paths use it too: a `DELETE FROM t WHERE rowid = ?` on a virtual
/// table has to be able to offer that equality, or every delete is a scan.
pub fn virtual_offer(id: usize, table: &TableInfo, terms: &[BoundExpr]) -> Vec<VirtualConstraint> {
    let mut offer = Vec::new();
    for term in terms {
        let Some((column, op, value)) = virtual_constraint(id, table, term) else {
            continue;
        };
        offer.push(VirtualConstraint {
            spec: crate::vtab::ConstraintSpec {
                column,
                op,
                // A write scans one term and nothing else, so every value it
                // could use is available before the loop starts.
                usable: value.is_constant() || !mentions(&value, id),
            },
            value,
            predicate: term.clone(),
        });
    }
    offer
}

/// Returns whether an expression reads one FROM term.
fn mentions(expr: &BoundExpr, id: usize) -> bool {
    let mut used = Vec::new();
    expr.sources_used(&mut used);
    used.contains(&id)
}

/// Splits a predicate into the conjunction the offer is built from.
pub fn conjunction(filter: &BoundExpr) -> Vec<BoundExpr> {
    let mut terms = Vec::new();
    split_conjunction(filter, &mut terms);
    terms
}

/// Returns the column, operator and value when a term constrains this term.
fn virtual_constraint(
    id: usize,
    table: &TableInfo,
    term: &BoundExpr,
) -> Option<(i32, crate::vtab::ConstraintOp, BoundExpr)> {
    use crate::vtab::{ConstraintOp, ROWID_COLUMN};
    // `x MATCH 'y'`, `x LIKE 'y'`, `x GLOB 'y'` and `x REGEXP 'y'` are the
    // operators a module exists to give meaning to, so they are offered first.
    if let BoundExpr::Pattern {
        negated: false,
        op,
        operand,
        pattern,
        escape: None,
    } = term
    {
        if let BoundExpr::Column { source, column, .. } = operand.as_ref() {
            if *source == id {
                let op = match op {
                    crate::ast::PatternOp::Match => ConstraintOp::Match,
                    crate::ast::PatternOp::Like => ConstraintOp::Like,
                    crate::ast::PatternOp::Glob => ConstraintOp::Glob,
                    crate::ast::PatternOp::Regexp => ConstraintOp::Regexp,
                };
                return Some((i32::from(*column), op, pattern.as_ref().clone()));
            }
        }
    }
    if let Some((op, value)) = comparison_against_rowid(id, term) {
        return binary_constraint(op).map(|op| (ROWID_COLUMN, op, value));
    }
    for column in 0..table.columns.len() {
        let column = column as u16;
        if let Some((op, value)) = comparison_against_column(id, column, term) {
            return binary_constraint(op).map(|op| (i32::from(column), op, value));
        }
    }
    None
}

/// Returns the constraint operator one comparison offers, if any.
fn binary_constraint(op: BinaryOp) -> Option<crate::vtab::ConstraintOp> {
    use crate::vtab::ConstraintOp;
    Some(match op {
        BinaryOp::Equal => ConstraintOp::Eq,
        BinaryOp::NotEqual => ConstraintOp::Ne,
        BinaryOp::Less => ConstraintOp::Lt,
        BinaryOp::LessEqual => ConstraintOp::Le,
        BinaryOp::Greater => ConstraintOp::Gt,
        BinaryOp::GreaterEqual => ConstraintOp::Ge,
        _ => return None,
    })
}

/// Chooses the access path a write's collection pass should walk.
///
/// An `UPDATE` or a `DELETE` finds the rows it will change before it changes
/// any of them - the two passes are what stop a write from tripping over its
/// own edits while it walks the tree it is editing. What the first pass had no
/// way to say, until this existed, was *which* rows to look at: it rewound the
/// table and read all of them, so `DELETE FROM t WHERE id = ?` visited every
/// row of `t` to find the one it was told about. On a five-thousand-row table
/// that is sixty page reads and half a millisecond where the same predicate in
/// a `SELECT` costs two page reads and ten microseconds.
///
/// The path chosen here is the same one the read planner would choose for the
/// same predicate, and the caller keeps applying the whole `WHERE` clause
/// afterwards. That is what makes this safe to add: a path can only narrow
/// which rows are *visited*, and every row it visits is still tested. A path
/// that wrongly excluded a row would be a bug, so the paths offered are only
/// the ones whose bounds provably cover every row the predicate accepts.
/// @param table - the table being written
/// @param source_id - the statement-wide number of the term being written
/// @param filter - the `WHERE` clause, when there is one
pub fn write_path(table: &TableInfo, source_id: usize, filter: Option<&BoundExpr>) -> AccessPath {
    let scan = AccessPath::TableScan { root: table.root };
    if table.module.is_some() || table.without_rowid {
        return scan;
    }
    let Some(filter) = filter else {
        return scan;
    };
    let mut terms = Vec::new();
    split_conjunction(filter, &mut terms);
    let ids = [source_id];
    let mut consumed = vec![false; terms.len()];
    if let Some(path) = rowid_path(source_id, 0, &ids, table, &terms, &mut consumed) {
        return path;
    }
    let source = BoundSource {
        id: source_id,
        rows: SourceRows::Table,
        table: table.clone(),
        alias: table.name.clone(),
        join: JoinKind::Inner,
        constraint: None,
        suppressed: Vec::new(),
    };
    let mut consumed = vec![false; terms.len()];
    // A write reads the whole row it is about to change, so no index covers it.
    let needed = ColumnUse {
        opaque: true,
        ..ColumnUse::default()
    };
    let Some(path) = index_path(source_id, 0, &ids, &source, &terms, &mut consumed, &needed) else {
        return scan;
    };
    // The same crossover the read planner uses: an index that has to fetch most
    // of the table costs a second descent per row on top of the scan it was
    // meant to replace.
    let (index_cost, _) = path_cost(&source, &path);
    let (scan_cost, _) = path_cost(&source, &scan);
    if index_cost <= scan_cost {
        return path;
    }
    scan
}

/// Returns a rowid equality or range path, when the predicates allow one.
fn rowid_path(
    id: usize,
    position: usize,
    ids: &[usize],
    table: &TableInfo,
    terms: &[BoundExpr],
    consumed: &mut [bool],
) -> Option<AccessPath> {
    if !table.has_rowid() {
        return None;
    }
    for (index, term) in terms.iter().enumerate() {
        if consumed.get(index).copied().unwrap_or(false) {
            continue;
        }
        let Some((op, value)) = comparison_against_rowid(id, term) else {
            continue;
        };
        if op != BinaryOp::Equal || !is_available(position, ids, &value) {
            continue;
        }
        if let Some(slot) = consumed.get_mut(index) {
            *slot = true;
        }
        return Some(AccessPath::RowidSeek {
            root: table.root,
            key: value,
        });
    }
    let mut low = None;
    let mut high = None;
    let mut used = Vec::new();
    for (index, term) in terms.iter().enumerate() {
        if consumed.get(index).copied().unwrap_or(false) {
            continue;
        }
        let Some((op, value)) = comparison_against_rowid(id, term) else {
            continue;
        };
        if !is_available(position, ids, &value) {
            continue;
        }
        match op {
            BinaryOp::Greater if low.is_none() => {
                low = Some(RangeBound {
                    kind: BoundKind::Greater,
                    value,
                });
                used.push(index);
            }
            BinaryOp::GreaterEqual if low.is_none() => {
                low = Some(RangeBound {
                    kind: BoundKind::GreaterEqual,
                    value,
                });
                used.push(index);
            }
            BinaryOp::Less if high.is_none() => {
                high = Some(RangeBound {
                    kind: BoundKind::Less,
                    value,
                });
                used.push(index);
            }
            BinaryOp::LessEqual if high.is_none() => {
                high = Some(RangeBound {
                    kind: BoundKind::LessEqual,
                    value,
                });
                used.push(index);
            }
            _ => {}
        }
    }
    if low.is_none() && high.is_none() {
        return None;
    }
    for index in used {
        if let Some(slot) = consumed.get_mut(index) {
            *slot = true;
        }
    }
    Some(AccessPath::RowidRange {
        root: table.root,
        low,
        high,
    })
}

/// Returns an index path over an equality prefix, when one is usable.
fn index_path(
    id: usize,
    position: usize,
    ids: &[usize],
    source: &BoundSource,
    terms: &[BoundExpr],
    consumed: &mut [bool],
    needed: &ColumnUse,
) -> Option<AccessPath> {
    let table = &source.table;
    let mut best: Option<(f64, AccessPath, Vec<usize>)> = None;
    for index in &table.indexes {
        if index.partial_sql.is_some() {
            // A partial index only holds the rows its predicate accepts. Using
            // one without proving the query implies that predicate would lose
            // rows, and the implication test is phase 8's.
            continue;
        }
        let Some((path, used)) =
            index_candidate(id, position, ids, table, index, terms, consumed, needed)
        else {
            continue;
        };
        // The choice between two usable indexes is a cost, not a count of
        // consumed terms. Two indexes that each satisfy one equality consume
        // the same number of terms and can differ by orders of magnitude in
        // how many rows they return - and taking the first one found made a
        // query constrained on both a two-valued column and a four-hundred-
        // valued one search the two-valued one.
        let (cost, _) = path_cost(source, &path);
        let better = best
            .as_ref()
            .is_none_or(|(existing, _, _)| cost < *existing - 1e-9);
        if better {
            best = Some((cost, path, used));
        }
    }
    let (_, path, used) = best?;
    for index in used {
        if let Some(slot) = consumed.get_mut(index) {
            *slot = true;
        }
    }
    Some(path)
}

/// Builds the best path over one index, or `None` if it cannot be used.
#[allow(clippy::too_many_arguments)]
fn index_candidate(
    id: usize,
    position: usize,
    ids: &[usize],
    table: &TableInfo,
    index: &IndexInfo,
    terms: &[BoundExpr],
    consumed: &[bool],
    needed: &ColumnUse,
) -> Option<(AccessPath, Vec<usize>)> {
    let mut equalities = Vec::new();
    let mut used = Vec::new();
    let mut collations = Vec::new();
    let mut descending = Vec::new();
    let mut columns = Vec::new();
    let mut key = 0usize;
    while let Some(key_column) = index.columns.get(key) {
        let Some(column) = key_column.column else {
            break;
        };
        let collation = collation_of(&key_column.collation);
        let Some((term_index, value)) =
            find_equality(id, position, ids, column, collation, terms, consumed, &used)
        else {
            break;
        };
        equalities.push(value);
        used.push(term_index);
        collations.push(collation);
        descending.push(key_column.descending);
        columns.push(column);
        key = key.saturating_add(1);
    }
    let mut low = None;
    let mut high = None;
    if let Some(key_column) = index.columns.get(key) {
        if let Some(column) = key_column.column {
            let collation = collation_of(&key_column.collation);
            for (term_index, term) in terms.iter().enumerate() {
                if consumed.get(term_index).copied().unwrap_or(false) || used.contains(&term_index)
                {
                    continue;
                }
                let Some((op, value)) = comparison_against_column(id, column, term) else {
                    continue;
                };
                if !is_available(position, ids, &value) || comparison_collation(term) != collation {
                    continue;
                }
                match op {
                    BinaryOp::Greater if low.is_none() => {
                        low = Some(RangeBound {
                            kind: BoundKind::Greater,
                            value,
                        });
                        used.push(term_index);
                    }
                    BinaryOp::GreaterEqual if low.is_none() => {
                        low = Some(RangeBound {
                            kind: BoundKind::GreaterEqual,
                            value,
                        });
                        used.push(term_index);
                    }
                    BinaryOp::Less if high.is_none() => {
                        high = Some(RangeBound {
                            kind: BoundKind::Less,
                            value,
                        });
                        used.push(term_index);
                    }
                    BinaryOp::LessEqual if high.is_none() => {
                        high = Some(RangeBound {
                            kind: BoundKind::LessEqual,
                            value,
                        });
                        used.push(term_index);
                    }
                    _ => {}
                }
            }
            if low.is_some() || high.is_some() {
                collations.push(collation);
                descending.push(key_column.descending);
                columns.push(column);
            }
        }
    }
    let covering = covering_slots(table, index, needed);
    if equalities.is_empty() && low.is_none() && high.is_none() && covering.is_none() {
        // Nothing to seek to and nothing to save by reading the entries: this
        // index has no part in answering the query.
        return None;
    }
    Some((
        AccessPath::IndexSeek {
            table_root: table.root,
            index_root: index.root,
            index_name: index.name.clone(),
            equalities,
            low,
            high,
            collations,
            descending,
            columns,
            without_rowid: table.without_rowid,
            key_entry_slots: if table.without_rowid && index.root != table.root {
                let leading = index.columns.len();
                (0..table.primary_key().len())
                    .map(|offset| leading.saturating_add(offset))
                    .collect()
            } else {
                Vec::new()
            },
            covering,
        },
        used,
    ))
}

/// The entry slot that stands for the row's own key rather than a field.
///
/// An index entry over a rowid table ends with the rowid, and the machine reads
/// it with `IdxRowid` rather than out of the entry's record - so a column that
/// *is* the rowid needs a marker rather than a slot number. It is the largest
/// `usize` because no entry can have that many fields, and because a number
/// that could also be a real slot would be a silent misread.
pub const ROWID_ENTRY_SLOT: usize = usize::MAX;

/// Returns where each column the query reads sits in one index's entries.
///
/// `None` when the index does not hold them all, which is the ordinary case and
/// is why a covering path is worth naming when it happens. A `WITHOUT ROWID`
/// table is excluded: its rows *are* index entries, so the question is already
/// answered by whether the seek is on the table's own key, and mixing the two
/// would be two answers to one question.
/// @param table - the table being read
/// @param index - the index being considered
/// @param needed - what the query reads from this term
fn covering_slots(
    table: &TableInfo,
    index: &IndexInfo,
    needed: &ColumnUse,
) -> Option<Vec<(u16, usize)>> {
    if needed.opaque || table.without_rowid || index.partial_sql.is_some() {
        return None;
    }
    let mut slots = Vec::with_capacity(needed.columns.len());
    for slot in &needed.columns {
        // The rowid alias is a column of the table and the *rowid* of the
        // entry, so it is covered whatever the index holds - but it is read
        // with `IdxRowid` rather than out of the entry's record, so it is not
        // in the list.
        if table.rowid_alias == Some(*slot) {
            slots.push((*slot, ROWID_ENTRY_SLOT));
            continue;
        }
        let position = index
            .columns
            .iter()
            .position(|key| key.column == Some(*slot))?;
        slots.push((*slot, position));
    }
    Some(slots)
}

/// Finds an equality predicate on one column with a matching collation.
fn find_equality(
    id: usize,
    position: usize,
    ids: &[usize],
    column: u16,
    collation: Collation,
    terms: &[BoundExpr],
    consumed: &[bool],
    used: &[usize],
) -> Option<(usize, BoundExpr)> {
    for (index, term) in terms.iter().enumerate() {
        if consumed.get(index).copied().unwrap_or(false) || used.contains(&index) {
            continue;
        }
        let Some((op, value)) = comparison_against_column(id, column, term) else {
            continue;
        };
        if op != BinaryOp::Equal || !is_available(position, ids, &value) {
            continue;
        }
        if comparison_collation(term) != collation {
            continue;
        }
        return Some((index, value));
    }
    None
}

/// Returns the collation a comparison uses, or BINARY.
fn comparison_collation(term: &BoundExpr) -> Collation {
    match term {
        BoundExpr::Compare { collation, .. } => *collation,
        _ => Collation::Binary,
    }
}

/// Returns the collation a folded name spells.
fn collation_of(name: &[u8]) -> Collation {
    Collation::from_name(core::str::from_utf8(name).unwrap_or("BINARY"))
        .unwrap_or(Collation::Binary)
}

/// Returns the operator and the other side when a term compares one column of
/// one source against something else.
fn comparison_against_column(
    position: usize,
    column: u16,
    term: &BoundExpr,
) -> Option<(BinaryOp, BoundExpr)> {
    let BoundExpr::Compare {
        op, left, right, ..
    } = term
    else {
        return None;
    };
    if let BoundExpr::Column {
        source,
        column: candidate,
        ..
    } = left.as_ref()
    {
        if *source == position && *candidate == column {
            return Some((*op, right.as_ref().clone()));
        }
    }
    if let BoundExpr::Column {
        source,
        column: candidate,
        ..
    } = right.as_ref()
    {
        if *source == position && *candidate == column {
            return Some((mirror(*op), left.as_ref().clone()));
        }
    }
    None
}

/// Returns the operator and the other side when a term compares a rowid.
fn comparison_against_rowid(position: usize, term: &BoundExpr) -> Option<(BinaryOp, BoundExpr)> {
    let BoundExpr::Compare {
        op, left, right, ..
    } = term
    else {
        return None;
    };
    if matches!(left.as_ref(), BoundExpr::Rowid { source } if *source == position) {
        return Some((*op, right.as_ref().clone()));
    }
    if matches!(right.as_ref(), BoundExpr::Rowid { source } if *source == position) {
        return Some((mirror(*op), left.as_ref().clone()));
    }
    None
}

/// Returns the operator that means the same thing with its operands swapped.
fn mirror(op: BinaryOp) -> BinaryOp {
    match op {
        BinaryOp::Less => BinaryOp::Greater,
        BinaryOp::LessEqual => BinaryOp::GreaterEqual,
        BinaryOp::Greater => BinaryOp::Less,
        BinaryOp::GreaterEqual => BinaryOp::LessEqual,
        other => other,
    }
}

/// Returns whether a value can be computed before entering a loop level.
///
/// A seek key may only read terms *outside* the loop it drives. Reading the
/// term's own columns would be circular, and reading an inner term's columns
/// would read a cursor that has not been positioned yet.
fn is_available(position: usize, ids: &[usize], value: &BoundExpr) -> bool {
    let mut used = Vec::new();
    value.sources_used(&mut used);
    used.iter().all(|source| {
        // A term this block does not own belongs to an enclosing one, whose
        // cursor is positioned before this block runs at all - so it is
        // available at every level, including the first.
        ids.iter()
            .position(|id| id == source)
            .is_none_or(|level| level < position)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bind::BoundExpr;

    /// A conjunction splits into its terms; a disjunction does not, because a
    /// term of an OR is not true of every row the OR accepts.
    #[test]
    fn only_conjunctions_split() {
        let expr = BoundExpr::And(
            Box::new(BoundExpr::Integer(1)),
            Box::new(BoundExpr::Or(
                Box::new(BoundExpr::Integer(2)),
                Box::new(BoundExpr::Integer(3)),
            )),
        );
        let mut terms = Vec::new();
        split_conjunction(&expr, &mut terms);
        assert_eq!(terms.len(), 2);
        assert!(matches!(terms.get(1), Some(BoundExpr::Or(_, _))));
    }

    /// A seek key may read only terms outside its own loop.
    #[test]
    fn a_seek_key_may_only_read_outer_terms() {
        let outer = BoundExpr::Column {
            source: 0,
            column: 0,
            slot: 0,
            affinity: rustdb_value::Affinity::Integer,
            collation: Collation::Binary,
        };
        let ids = [0usize, 1usize];
        assert!(is_available(1, &ids, &outer));
        assert!(!is_available(0, &ids, &outer));
        assert!(is_available(0, &ids, &BoundExpr::Integer(5)));
        // A term the block does not own belongs to an enclosing block, whose
        // cursor is already positioned, so it is available at every level.
        assert!(is_available(0, &[7usize], &outer));
    }

    /// Mirroring a comparison keeps its meaning when the operands swap.
    #[test]
    fn mirroring_preserves_meaning() {
        assert_eq!(mirror(BinaryOp::Less), BinaryOp::Greater);
        assert_eq!(mirror(BinaryOp::GreaterEqual), BinaryOp::LessEqual);
        assert_eq!(mirror(BinaryOp::Equal), BinaryOp::Equal);
    }
}
