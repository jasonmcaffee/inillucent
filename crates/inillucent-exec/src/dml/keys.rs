//! Finding the rows a write is about to change.
//!
//! Invariant: **the search that finds the rows is a read, planned the same way
//! any other read is.** An `UPDATE ... WHERE` uses an index when one applies,
//! which is what `EXPLAIN QUERY PLAN` on a write describes and why the planner
//! has anything to say about one at all.

use inillucent_base::DbResult;
use inillucent_sql::ast::JoinKind as SqlJoinKind;
use inillucent_sql::bind::{BoundExpr, BoundResultColumn, BoundSelect, BoundSource, SourceRows};
use inillucent_sql::catalog_view::TableInfo;

use super::*;
use crate::physical::SourceLayout;

/// Builds the query that finds the rows an `UPDATE` or `DELETE` will change.
///
/// The statement's own `WHERE`, over the statement's own table, projecting the
/// table's key. Running it through the ordinary planner is the whole point:
/// `UPDATE main_table SET key = key + 1 WHERE id = ?1` has to reach the same
/// point probe a `SELECT` with that `WHERE` would, or the write path is a full
/// scan per statement wearing an index's clothes.
///
/// @param table - the table being written
/// @param source - the statement-wide number of its FROM term
/// @param filter - the statement's `WHERE`, when it wrote one
/// @param limit - the statement's `LIMIT`
/// @param offset - the statement's `OFFSET`
/// @param layout - the table tree's layout, for which columns are the key
pub fn keys_query(
    table: &TableInfo,
    source: usize,
    filter: Option<&BoundExpr>,
    limit: Option<&BoundExpr>,
    offset: Option<&BoundExpr>,
    layout: &SourceLayout,
) -> DbResult<BoundSelect> {
    keys_query_joined(table, source, filter, limit, offset, layout, &[], &[])
}
/// Returns the query that finds the keys of an `UPDATE ... FROM`, and the
/// values it will write into them.
///
/// **The rows come from a join, so the values do too.** `UPDATE t SET v = s.v
/// FROM s WHERE s.a = t.a` assigns from a row of `s`, which the write path
/// never sees: it is handed keys and evaluates the assignments against the
/// target row alone. So the keys query grows the extra FROM terms and projects
/// the assigned values *beside* the key, and the write path reads them out of
/// the row it was given rather than computing them.
///
/// With no extra terms and no projected assignments this is exactly
/// [`keys_query`] - the same one source, the same one-column projection - so an
/// ordinary `UPDATE` pays nothing for the shape.
///
/// @param table - the table being written
/// @param source - the statement-wide number of its FROM term
/// @param filter - the `WHERE` clause
/// @param limit - the `LIMIT`
/// @param offset - the `OFFSET`
/// @param layout - the table tree's layout
/// @param joined - the extra FROM terms, in written order
/// @param assigned - the assignment expressions to project, in order
#[allow(clippy::too_many_arguments)]
pub fn keys_query_joined(
    table: &TableInfo,
    source: usize,
    filter: Option<&BoundExpr>,
    limit: Option<&BoundExpr>,
    offset: Option<&BoundExpr>,
    layout: &SourceLayout,
    joined: &[BoundSource],
    assigned: &[BoundExpr],
) -> DbResult<BoundSelect> {
    let mut columns: Vec<BoundResultColumn> = key_columns(table, source, layout)?
        .into_iter()
        .map(|expr| BoundResultColumn {
            expr,
            name: b"key".to_vec(),
            origin: None,
            declared_type: Vec::new(),
        })
        .collect();
    for expr in assigned {
        columns.push(BoundResultColumn {
            expr: expr.clone(),
            name: b"value".to_vec(),
            origin: None,
            declared_type: Vec::new(),
        });
    }
    let mut sources = vec![BoundSource {
        id: source,
        rows: SourceRows::Table,
        table: std::rc::Rc::new(table.clone()),
        alias: table.name.clone(),
        join: SqlJoinKind::Inner,
        constraint: None,
        suppressed: Vec::new(),
        index_exprs: Vec::new(),
    }];
    sources.extend(joined.iter().cloned());
    Ok(BoundSelect {
        sources,
        filter: filter.cloned(),
        group_by: Vec::new(),
        having: None,
        columns,
        distinct: false,
        order_by: Vec::new(),
        limit: limit.cloned(),
        offset: offset.cloned(),
        aggregates: Vec::new(),
        values: Vec::new(),
        compounds: Vec::new(),
        windows: Vec::new(),
        correlations: Vec::new(),
    })
}
/// Returns the query that finds the rowids a write to a module will change.
///
/// The same shape as [`keys_query`] and without its layout, because a virtual
/// table has none: the module owns its storage, and the only handle the engine
/// has on one of its rows is the rowid the module answers with. A `DELETE` is
/// therefore "ask which rowids match, then tell the module about each" - which
/// is what SQLite does, and the reason `xUpdate` takes a rowid rather than a
/// predicate.
///
/// @param table - the virtual table being written
/// @param source - the FROM term id the filter's columns refer to
/// @param filter - the statement's `WHERE`, if it has one
/// @param limit - the statement's `LIMIT`, if it has one
/// @param offset - the statement's `OFFSET`, if it has one
pub fn module_keys_query(
    table: &TableInfo,
    source: usize,
    filter: Option<&BoundExpr>,
    limit: Option<&BoundExpr>,
    offset: Option<&BoundExpr>,
) -> BoundSelect {
    BoundSelect {
        sources: vec![BoundSource {
            id: source,
            rows: SourceRows::Table,
            table: std::rc::Rc::new(table.clone()),
            alias: table.name.clone(),
            join: SqlJoinKind::Inner,
            constraint: None,
            suppressed: Vec::new(),
            index_exprs: Vec::new(),
        }],
        filter: filter.cloned(),
        group_by: Vec::new(),
        having: None,
        columns: vec![BoundResultColumn {
            expr: BoundExpr::Rowid { source },
            name: b"key".to_vec(),
            origin: None,
            declared_type: Vec::new(),
        }],
        distinct: false,
        order_by: Vec::new(),
        limit: limit.cloned(),
        offset: offset.cloned(),
        aggregates: Vec::new(),
        values: Vec::new(),
        compounds: Vec::new(),
        windows: Vec::new(),
        correlations: Vec::new(),
    }
}
