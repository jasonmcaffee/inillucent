//! What `INDEXED BY` and `NOT INDEXED` allow the planner to choose.
//!
//! Invariant: **the binder's refusal and the planner's choice read the same
//! terms.** A statement whose named index cannot answer it is refused before
//! it is planned, because `choose_path` returns an `AccessPath` and has no way
//! to refuse. That is only sound if the question asked here, before planning,
//! is the one `choose_path` answers during it, so both take their terms from
//! [`statement_terms`] and [`outer_terms`] and their verdict on a partial index
//! from [`index_usable`].
//!
//! Here rather than in [`super`] because `plan.rs` is at its recorded size and
//! these five functions are one idea, added in task-2078.

use super::*;

/// Returns the conjuncts a path for an inner or comma joined term may seek on.
///
/// The statement's `WHERE`, and the `ON` of every term that is not the
/// null-extendable side of an outer join. [`plan_select_with`] plans with these,
/// and [`unanswerable_index_hint`] proves a forced index with them, so the two
/// cannot reach different verdicts about the same statement.
/// @param select - the bound statement
pub(super) fn statement_terms(select: &BoundSelect) -> Vec<BoundExpr> {
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
    terms
}

/// Returns the conjuncts of an outer join term's own `ON`, which are the only
/// ones its path may seek on. `plan_select_with` says why.
/// @param source - the null-extendable term
pub(super) fn outer_terms(source: &BoundSource) -> Vec<BoundExpr> {
    let mut terms = Vec::new();
    if let Some(constraint) = &source.constraint {
        split_conjunction(constraint, &mut terms);
    }
    terms
}

/// Returns the name of an `INDEXED BY` index that cannot answer its term.
///
/// **This is the refusal `choose_path` has nowhere to put.** It returns an
/// `AccessPath` and has a dozen callers, so the binder asks this instead,
/// once per block, before anything is planned. SQLite's answer to such a
/// statement is `no query solution`, and the cases are few, because a named
/// b-tree index can always be walked from end to end: the pinned 3.53.4 shell
/// plans `INDEXED BY h_a` over `WHERE c = 3`, with nothing on `a` at all, as
/// `SCAN h USING INDEX h_a`. What cannot be walked is a partial index whose
/// predicate the statement does not imply, because it would lose the rows the
/// predicate leaves out. `CREATE INDEX h_part ON h(c) WHERE c > 3` refuses
/// `SELECT * FROM h INDEXED BY h_part WHERE a = 1` there, and here.
///
/// An index a module owns is answerable only by the nearest neighbour probe,
/// and a virtual table has no index this clause can name.
/// @param select - one bound block, with its sources attached
pub fn unanswerable_index_hint(select: &BoundSelect) -> Option<Vec<u8>> {
    let mut shared: Option<Vec<BoundExpr>> = None;
    for (position, source) in select.sources.iter().enumerate() {
        let crate::bind::IndexChoice::Only(wanted) = &source.index_hint else {
            continue;
        };
        if !matches!(source.rows, SourceRows::Table) {
            continue;
        }
        let table = &source.table;
        let Some((at, index)) = table
            .indexes
            .iter()
            .enumerate()
            .find(|(_, index)| &index.folded == wanted)
        else {
            continue;
        };
        let answerable = if table.module.is_some() {
            false
        } else if index.origin == crate::catalog_view::IndexOrigin::Module {
            let id = source.id;
            matches!(
                vector_path(id, position, source, select),
                Some(AccessPath::VectorProbe { index: ref chosen, .. }) if chosen == &index.name
            )
        } else if is_outer(source.join) {
            index_usable(source, at, index, &outer_terms(source))
        } else {
            let terms = shared.get_or_insert_with(|| statement_terms(select));
            index_usable(source, at, index, terms)
        };
        if !answerable {
            return Some(index.name.clone());
        }
    }
    None
}

/// Reports whether one b-tree index may be read for a term at all.
///
/// Every index may, except a partial one whose predicate the terms do not
/// imply; `index_path` says why that one would lose rows.
/// @param source - the term
/// @param at - the index's position in the table's list
/// @param index - the index
/// @param terms - the conjuncts the term's path may seek on
pub(super) fn index_usable(
    source: &BoundSource,
    at: usize,
    index: &IndexInfo,
    terms: &[BoundExpr],
) -> bool {
    let computed = source.index_exprs.iter().find(|held| held.position == at);
    index.partial_sql.is_none() || implies(computed, terms)
}

/// Chooses the path for a term written `INDEXED BY name`: that index, read the
/// cheapest way it can be.
///
/// **Nothing else is a candidate**, not the table scan and not the rowid, which
/// is SQLite's rule and was measured against the pinned 3.53.4 shell:
/// `SELECT count(*) FROM h INDEXED BY h_a WHERE a = 3 AND b = 100` searches
/// `h_a` there even though `ANALYZE` prefers `h_b`, and until task-2078 it
/// searched `h_b` here. When nothing in the statement seeks the index it is
/// walked end to end, which is what `SCAN h USING INDEX h_a` means.
///
/// The binder has already refused a statement the index cannot answer, through
/// [`unanswerable_index_hint`], so the table scan at the bottom is only reached
/// by a caller that built a `BoundSource` without the binder. It returns every
/// row, which is the answer that cannot be wrong.
/// @param id - the term's statement-wide id
/// @param position - its place in the visiting order
/// @param ids - every term's id in visiting order
/// @param source - the term
/// @param select - the whole statement, for the columns it reads
/// @param terms - the conjuncts the path may seek on
/// @param consumed - which conjuncts an earlier path already answers
/// @param levers - which optimizations are on
pub(super) fn forced_path(
    id: usize,
    position: usize,
    ids: &[usize],
    source: &BoundSource,
    select: &BoundSelect,
    terms: &[BoundExpr],
    consumed: &mut [bool],
    levers: Levers,
) -> AccessPath {
    let needed = select.columns_read(id);
    index_path(id, position, ids, source, terms, consumed, &needed, levers).unwrap_or(
        AccessPath::TableScan {
            root: source.table.root,
        },
    )
}
