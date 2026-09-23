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
/// Puts the target's `INDEXED BY` or `NOT INDEXED` onto the query that finds a
/// write's rows.
///
/// [`keys_query`] builds that query's target term from the table alone, so the
/// hint the statement wrote on it was dropped here, and `DELETE FROM h INDEXED
/// BY h_a WHERE b = 5` searched `h_b`. The pinned 3.53.4 shell answers
/// `UPDATE h INDEXED BY h_a SET c = 1 WHERE b = 5` with `SCAN h USING INDEX
/// h_a`. `INDEXED BY` also brings the target's bound index expressions, because
/// a partial index is only usable when its predicate can be compared with the
/// `WHERE`, and the binder has already refused one that cannot be.
///
/// @param select - a query from [`keys_query`] or [`keys_query_joined`], whose
///   first term is the target
/// @param hint - the hint the statement wrote on the target
/// @param index_exprs - the target's bound index expressions
pub fn hint_target(
    select: &mut BoundSelect,
    hint: &inillucent_sql::bind::IndexChoice,
    index_exprs: &[inillucent_sql::dml::BoundIndexExprs],
) {
    let Some(target) = select.sources.first_mut() else {
        return;
    };
    target.index_hint = hint.clone();
    if matches!(hint, inillucent_sql::bind::IndexChoice::Only(_)) {
        target.index_exprs = index_exprs.to_vec();
    }
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
        index_hint: inillucent_sql::bind::IndexChoice::Any,
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
            index_hint: inillucent_sql::bind::IndexChoice::Any,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dml::testing::a_table;

    /// Returns a layout for a three-column rowid table.
    ///
    /// @param key_columns - which tree columns form the key
    fn a_layout(key_columns: Vec<usize>) -> SourceLayout {
        SourceLayout {
            tree_key: 1,
            slots: vec![Some(0), Some(1), Some(2)],
            rowid: Some(0),
            identity: vec![0],
            types: vec![crate::expr::StaticType::Unknown; 3],
            width: 3,
            key_columns,
        }
    }

    /// The keys query selects the key and carries the statement's own `WHERE`.
    ///
    /// **A statement's reads all happen before any of its writes (T3,
    /// task-1962).** This is the read half: the `UPDATE` or `DELETE` is run as
    /// a query that finds the keys of the rows it will change, and only then
    /// are those rows written. A query that dropped the `WHERE` would change
    /// every row in the table.
    #[test]
    fn the_keys_query_selects_the_key_under_the_statement_s_filter() {
        let table = a_table("t", &["a", "b", "c"]);
        let layout = a_layout(vec![0]);
        let filter = BoundExpr::Compare {
            op: inillucent_sql::ast::BinaryOp::Greater,
            left: Box::new(BoundExpr::Rowid { source: 0 }),
            right: Box::new(BoundExpr::Integer(4)),
            affinity: Some(inillucent_value::Affinity::Integer),
            collation: inillucent_value::Collation::Binary,
        };
        let query = keys_query(&table, 0, Some(&filter), None, None, &layout)
            .expect("a one-column key compiles to a query");
        assert_eq!(
            query.columns.len(),
            1,
            "one key column, so the query reads one value per row"
        );
        assert!(
            query.filter.is_some(),
            "the statement's WHERE has to reach the query, or every row is changed"
        );
        assert_eq!(
            query.sources.len(),
            1,
            "one FROM term: the table being written"
        );
    }

    /// A `LIMIT` and an `OFFSET` travel with the keys query.
    ///
    /// `DELETE FROM t WHERE ... LIMIT 5` deletes five rows. Applying the limit
    /// to the write rather than to the search would be the same answer only
    /// while nothing filtered between the two.
    #[test]
    fn a_limit_and_an_offset_reach_the_query() {
        let table = a_table("t", &["a", "b", "c"]);
        let layout = a_layout(vec![0]);
        let five = BoundExpr::Integer(5);
        let two = BoundExpr::Integer(2);
        let query = keys_query(&table, 0, None, Some(&five), Some(&two), &layout)
            .expect("a limited delete compiles to a query");
        assert!(
            query.limit.is_some(),
            "the LIMIT decides how many rows are found"
        );
        assert!(query.offset.is_some(), "and the OFFSET which ones");
    }

    /// A composite key reads every one of its columns.
    #[test]
    fn a_composite_key_reads_every_column_of_it() {
        let table = a_table("t", &["a", "b", "c"]);
        let layout = a_layout(vec![1, 2]);
        let query = keys_query(&table, 0, None, None, None, &layout)
            .expect("a two-column key compiles to a query");
        assert_eq!(
            query.columns.len(),
            2,
            "a WITHOUT ROWID table's key is every column of it, and a query              that read one of them would address the wrong row"
        );
    }
}
