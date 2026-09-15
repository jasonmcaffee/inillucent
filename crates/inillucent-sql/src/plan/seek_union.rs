//! Turning a disjunction into a union of seeks.
//!
//! Invariant: shared with the rest of [`super`] - a path returned from here is
//! legal before it is fast, so every function returns `None` rather than a
//! guess the moment a shape does not exactly match what it proves correct.
//!
//! `x IN (a, b, c)` is a union of equalities on one column, and a keyset
//! page's `(a=? AND b>?) OR a>?` is a union of ranges over a composite key -
//! two different shapes of the same idea, split into this module because
//! together they are the largest single piece of work this planner does.

use super::*;

/// The most branches a seek union will offer as a candidate.
///
/// Past this a plan with one branch per value is a plan with thousands of
/// tree descents, and the cost model already prefers a scan once the total
/// passes a scan's own cost - this just stops the planner building and
/// pricing a candidate that size in the first place. Chosen generously
/// against the shape this exists for: "a page of ids read from an index" is
/// dozens of values, not thousands.
const MAX_SEEK_UNION_BRANCHES: usize = 512;

/// Returns a union of rowid seeks, when a term is `rowid IN (list)`.
///
/// The mirror of the plain equality search just above, run once per value of
/// a list instead of once: every value is exactly the key a lone
/// [`AccessPath::RowidSeek`] would use, so the whole term becomes one branch
/// per value. A literal duplicate is folded here, at plan time, because
/// comparing two already-bound expressions costs nothing next to a seek; a
/// value that cannot be compared this way - a parameter, a correlated column
/// - is kept, and the executor checks what it actually seeks to before it
/// probes a key it may already have probed.
pub(super) fn rowid_in_list_path(
    id: usize,
    position: usize,
    ids: &[usize],
    table: &TableInfo,
    terms: &[BoundExpr],
    consumed: &mut [bool],
) -> Option<AccessPath> {
    for (index, term) in terms.iter().enumerate() {
        if consumed.get(index).copied().unwrap_or(false) {
            continue;
        }
        let BoundExpr::InList {
            negated: false,
            operand,
            list,
            ..
        } = term
        else {
            continue;
        };
        if !matches!(operand.as_ref(), BoundExpr::Rowid { source } if *source == id) {
            continue;
        }
        if list.is_empty() || list.len() > MAX_SEEK_UNION_BRANCHES {
            continue;
        }
        if !list.iter().all(|value| is_available(position, ids, value)) {
            continue;
        }
        let mut keys: Vec<BoundExpr> = Vec::with_capacity(list.len());
        for value in list {
            if !keys.contains(value) {
                keys.push(value.clone());
            }
        }
        if let Some(slot) = consumed.get_mut(index) {
            *slot = true;
        }
        return Some(AccessPath::RowidSeekUnion {
            root: table.root,
            keys,
        });
    }
    None
}

/// Returns a union of index seeks, when a term is `column IN (list)` on one
/// index's leading column.
///
/// The mirror of [`rowid_in_list_path`] for a secondary index or a
/// `WITHOUT ROWID` table's own primary-key index: every value of the list
/// becomes one branch, each an equality exactly a lone
/// [`AccessPath::IndexSeek`] would use alone.
///
/// **An equality prefix ahead of the `IN` column is taken too (task-1932,
/// M7).** `WHERE a = 5 AND b IN (1, 2, 3)` on an index over `(a, b)` is three
/// seeks to `(5, 1)`, `(5, 2)` and `(5, 3)`, and this used to look at the
/// leading column only - so the `IN` was a residual and the whole query became
/// a scan or a one-column seek over every `a = 5`. There is no Cartesian
/// question: every column ahead of the `IN` is pinned by a *single* equality,
/// so the prefix is one tuple however long it is.
pub(super) fn in_list_union_path(
    context: &super::CandidateContext<'_>,
    index: &IndexInfo,
    usable: bool,
) -> Option<(AccessPath, Vec<usize>)> {
    let super::CandidateContext {
        id,
        position,
        ids,
        table,
        terms,
        consumed,
        needed,
        levers,
    } = *context;
    // Every leading key column pinned by an equality, in key order. The `IN`
    // is looked for on the column after them.
    let mut prefix: Vec<BoundExpr> = Vec::new();
    let mut prefix_terms: Vec<usize> = Vec::new();
    let mut collations: Vec<Collation> = Vec::new();
    let mut descending: Vec<bool> = Vec::new();
    let mut columns: Vec<Option<u16>> = Vec::new();
    let mut at = 0usize;
    while let Some(key_column) = index.columns.get(at) {
        let Some(column) = key_column.column else {
            break;
        };
        let collation = collation_of(&key_column.collation);
        let found = terms.iter().enumerate().find(|(term_index, term)| {
            !consumed.get(*term_index).copied().unwrap_or(false)
                && !prefix_terms.contains(term_index)
                && comparison_collation(term) == collation
                && comparison_against_column(id, column, term).is_some_and(|(op, value)| {
                    op == BinaryOp::Equal && is_available(position, ids, &value)
                })
        });
        let Some((term_index, term)) = found else {
            break;
        };
        let Some((_, value)) = comparison_against_column(id, column, term) else {
            break;
        };
        prefix.push(value);
        prefix_terms.push(term_index);
        collations.push(collation);
        descending.push(key_column.descending);
        columns.push(Some(column));
        at = at.saturating_add(1);
    }

    let key_column = index.columns.get(at)?;
    let column = key_column.column?;
    let collation = collation_of(&key_column.collation);
    collations.push(collation);
    descending.push(key_column.descending);
    columns.push(Some(column));
    for (term_index, term) in terms.iter().enumerate() {
        if consumed.get(term_index).copied().unwrap_or(false) || prefix_terms.contains(&term_index)
        {
            continue;
        }
        let BoundExpr::InList {
            negated: false,
            operand,
            list,
            collation: in_collation,
            ..
        } = term
        else {
            continue;
        };
        let BoundExpr::Column {
            source: term_source,
            column: candidate,
            ..
        } = operand.as_ref()
        else {
            continue;
        };
        if *term_source != id || *candidate != column || *in_collation != collation {
            continue;
        }
        if list.is_empty() || list.len() > MAX_SEEK_UNION_BRANCHES {
            continue;
        }
        if !list.iter().all(|value| is_available(position, ids, value)) {
            continue;
        }
        let mut branches = Vec::with_capacity(list.len());
        let mut seen: Vec<&BoundExpr> = Vec::with_capacity(list.len());
        for value in list {
            // **A NULL in the list matches nothing, so it gets no branch
            // (task-1932, found by `tlp_differential.rs`).** `b IN ('k1', NULL)`
            // is true for `k1`, false for nothing, and NULL for every other
            // value - so the rows a `WHERE` keeps are exactly the rows equal to
            // a non-NULL member. A branch for the NULL sought the index's own
            // NULL entries and answered every row where `b IS NULL`: on a
            // six-hundred-row table `b IN ('k1', 'k2', 'k7', NULL)` counted 95
            // where a scan applying the same predicate counts 41, and the
            // fifty-four extra rows were the ones whose `b` is NULL.
            //
            // Dropping it is sound rather than a special case: the seek answers
            // the rows the `IN` is *true* for, and three-valued logic only
            // separates false from NULL somewhere this path is not used - the
            // planner does not turn a negated `IN` into a union.
            if matches!(value, BoundExpr::Null) {
                continue;
            }
            if seen.contains(&value) {
                continue;
            }
            seen.push(value);
            let mut equalities = prefix.clone();
            equalities.push(value.clone());
            branches.push(IndexSeekBranch {
                equalities,
                low: None,
                high: None,
            });
        }
        let covering = levers
            .has(Levers::COVERING_INDEX)
            .then(|| covering_slots(table, index, needed, usable))
            .flatten();
        return Some((
            AccessPath::IndexSeekUnion {
                table_root: table.root,
                index_root: index.root,
                index_name: index.name.clone(),
                branches,
                collations: collations.clone(),
                descending: descending.clone(),
                columns: columns.clone(),
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
                // A repeated literal is already folded above; a value that is
                // not a literal (a parameter, a correlated column) can still
                // repeat at runtime, and only the executor learns that.
                dedup: true,
            },
            {
                let mut used = prefix_terms.clone();
                used.push(term_index);
                used
            },
        ));
    }
    None
}

/// Returns a union of index range seeks, when a disjunction is the standard
/// ascending keyset-pagination decomposition of a composite key's tuple
/// comparison.
///
/// `(a, b) > (x, y)` is `a > x OR (a = x AND b > y)` - one branch per depth of
/// the tuple, each pinning every column shallower than its own by equality
/// and then comparing the next column with a strict `>`. A page of
/// `ORDER BY a, b LIMIT n` reads exactly this once it has a cursor, which is
/// why the shape recurs everywhere a repository layer paginates a composite
/// key - and why, unlike an arbitrary disjunction, it needs no runtime work to
/// prove disjoint: the branch that pins a column by equality can never
/// overlap a shallower branch that already required that same column to be
/// strictly greater, and running the deepest branch first and the shallowest
/// last is what makes the union's own output already in ascending order,
/// because the deepest branch is exactly the rows tied with the cursor on
/// every column but the last.
///
/// Only the strict, ascending form is matched. `>=`, a descending column and a
/// branch that carries anything beyond the tuple comparison are the same idea
/// with a different final term and are left unmatched here rather than
/// guessed at.
pub(super) fn keyset_range_union_path(
    context: &super::CandidateContext<'_>,
    index: &IndexInfo,
    usable: bool,
) -> Option<(AccessPath, Vec<usize>)> {
    let super::CandidateContext {
        id,
        position,
        ids,
        table,
        terms,
        consumed,
        needed,
        levers,
    } = *context;
    // **A range is an outermost-term path only**, the same rule and the same
    // reason [`super::rowid_path`]'s range half follows: the shallowest
    // branch here has no equality prefix at all, so it is a walk between two
    // bounds of the *whole* tree rather than something scoped to one outer
    // row, and the physical pass has nowhere to put that inside a per-row
    // probe.
    if position != 0 {
        return None;
    }
    for (term_index, term) in terms.iter().enumerate() {
        if consumed.get(term_index).copied().unwrap_or(false) {
            continue;
        }
        if !matches!(term, BoundExpr::Or(_, _)) {
            continue;
        }
        let mut flat = Vec::new();
        flatten_or(term, &mut flat);
        if flat.len() < 2 {
            continue;
        }
        let Some(branches) = keyset_branches(id, position, ids, index, &flat) else {
            continue;
        };
        let depth = branches.len();
        let mut collations = Vec::with_capacity(depth);
        let mut descending = Vec::with_capacity(depth);
        let mut columns = Vec::with_capacity(depth);
        for key_column in index.columns.iter().take(depth) {
            collations.push(collation_of(&key_column.collation));
            descending.push(key_column.descending);
            columns.push(key_column.column);
        }
        let covering = levers
            .has(Levers::COVERING_INDEX)
            .then(|| covering_slots(table, index, needed, usable))
            .flatten();
        return Some((
            AccessPath::IndexSeekUnion {
                table_root: table.root,
                index_root: index.root,
                index_name: index.name.clone(),
                branches,
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
                // Proven disjoint and already in order, so nothing the
                // executor emits needs to be checked against anything it
                // already emitted.
                dedup: false,
            },
            vec![term_index],
        ));
    }
    None
}

/// Flattens a tree of `OR`s into its arms, in the order they were written.
///
/// The mirror of [`split_conjunction`] for `OR`: unlike an `AND`, none of the
/// arms is individually true of every row the whole expression accepts, so
/// they are never turned into independent residual terms. They are collected
/// this way only to be matched as a *unit* against a known shape, such as
/// [`keyset_branches`] does.
/// @param expr - the expression to flatten
/// @param into - where each arm is pushed, in order
fn flatten_or(expr: &BoundExpr, into: &mut Vec<BoundExpr>) {
    match expr {
        BoundExpr::Or(left, right) => {
            flatten_or(left, into);
            flatten_or(right, into);
        }
        other => into.push(other.clone()),
    }
}

/// Matches a flattened disjunction against the keyset tuple-comparison shape
/// over one index, and returns its branches in the order they must run.
///
/// Every arm is required to be *exactly* an equality prefix followed by one
/// strict `>` on the column after it - nothing shallower, nothing deeper, and
/// nothing extra ANDed in - and every depth from one up to the deepest arm
/// found must be covered exactly once. A disjunction that is close to this
/// shape but not exactly it is left unmatched: a looser test would be a place
/// for a wrong answer to live, and the arm SQLite's own OR optimization does
/// not reach either is safer left a scan than seeked on a guess.
/// @param id - the FROM term's statement-wide number
/// @param position - where it sits in the join order
/// @param ids - the FROM terms joined so far
/// @param index - the index the branches are matched against
/// @param flat - the disjunction's arms, in the order they were written
fn keyset_branches(
    id: usize,
    position: usize,
    ids: &[usize],
    index: &IndexInfo,
    flat: &[BoundExpr],
) -> Option<Vec<IndexSeekBranch>> {
    let max_depth = index.columns.len().min(flat.len());
    if max_depth == 0 || flat.len() != max_depth {
        return None;
    }
    let mut by_depth: Vec<Option<IndexSeekBranch>> = vec![None; max_depth];
    for arm in flat {
        let mut conjuncts = Vec::new();
        split_conjunction(arm, &mut conjuncts);
        let (range_term, equality_terms) = conjuncts.split_last()?;
        let depth = equality_terms.len().saturating_add(1);
        if depth > max_depth || by_depth.get(depth - 1)?.is_some() {
            return None;
        }
        let mut equalities = Vec::with_capacity(equality_terms.len());
        for (at, eq_term) in equality_terms.iter().enumerate() {
            let key_column = index.columns.get(at)?;
            let column = key_column.column?;
            let collation = collation_of(&key_column.collation);
            let (op, value) = comparison_against_column(id, column, eq_term)?;
            if op != BinaryOp::Equal
                || comparison_collation(eq_term) != collation
                || !is_available(position, ids, &value)
            {
                return None;
            }
            equalities.push(value);
        }
        let key_column = index.columns.get(equality_terms.len())?;
        let column = key_column.column?;
        if key_column.descending {
            return None;
        }
        let collation = collation_of(&key_column.collation);
        let (op, value) = comparison_against_column(id, column, range_term)?;
        if op != BinaryOp::Greater
            || comparison_collation(range_term) != collation
            || !is_available(position, ids, &value)
        {
            return None;
        }
        // `depth` is one-based and `by_depth` was sized from the same walk, so
        // the slot is always there; a `get_mut` says that rather than asserting
        // it, and the crate denies `indexing_slicing` (task-1932, H9 - this was
        // reported the moment `inillucent-scalar` was made to deny the same
        // four lints, because clippy then walked the whole dependency chain).
        let slot = by_depth.get_mut(depth.saturating_sub(1))?;
        *slot = Some(IndexSeekBranch {
            equalities,
            low: Some(RangeBound {
                kind: BoundKind::Greater,
                value,
            }),
            high: None,
        });
    }
    if by_depth.iter().any(Option::is_none) {
        return None;
    }
    // Shallowest to deepest is the order they were collected in; the walk has
    // to run deepest first, so the order is reversed once, here.
    let mut ordered: Vec<IndexSeekBranch> = by_depth.into_iter().flatten().collect();
    ordered.reverse();
    Some(ordered)
}
