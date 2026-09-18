//! `DELETE`, and the triggers it fires on the way.
//!
//! Invariant: **a row's index entries go before the row does.** An entry left
//! behind is a seek that finds a row that has been deleted, which is a wrong
//! answer rather than a leak.

use index::{index_entry, maintained, write_index_entry};

use inillucent_base::DbResult;
use inillucent_sql::ast::TriggerTime;
use inillucent_sql::catalog_view::{TableInfo, TableKind};
use inillucent_sql::dml::BoundDelete;
use inillucent_tree::datum::{Datum, OwnedDatum};

use super::*;
use crate::declared::{IndexExprs, WriteDeclarations};
use crate::physical::{Params, SourceLayout};
use crate::trigger::{self, Depth};

/// Applies a `DELETE` to rows a query has already selected.
///
/// @param statement - the bound delete
/// @param target - the file and its trees
/// @param params - the bound parameters
/// @param keys - the key of each row the `WHERE` selected
pub fn delete(
    statement: &BoundDelete,
    target: &mut dyn WriteTarget,
    params: &Params,
    keys: &[Row],
) -> DbResult<Changes> {
    delete_at(statement, target, params, keys, Depth::default())
}
/// Applies a `DELETE` that is already some triggers deep.
///
/// @param statement - the bound delete
/// @param target - the file and its trees
/// @param params - the bound parameters
/// @param keys - the key of each row the `WHERE` selected
/// @param depth - how many triggers deep this write already is
pub fn delete_at(
    statement: &BoundDelete,
    target: &mut dyn WriteTarget,
    params: &Params,
    keys: &[Row],
    depth: Depth,
) -> DbResult<Changes> {
    let table = &statement.table;
    if table.kind == TableKind::View {
        return delete_view(statement, target, params, keys, depth);
    }
    let layout = layout_of(target, table)?;
    let space = RowSpace::new(&sources_for(statement.source, &statement.triggers), &layout);
    let catalog = target.catalog();
    // A delete declares no `CHECK` to meet, but it does have to know which of
    // the table's indexes hold the row it is removing: an entry only comes out
    // of a partial index if the predicate accepted the row, and an index key the
    // table does not carry has to be recomputed to be found.
    let declarations = WriteDeclarations::compile(
        table,
        &layout,
        &[],
        // A `DELETE` writes no value, so no default can stand in for one.
        &[],
        &statement.index_exprs,
        &space,
        params,
        catalog,
    )?;
    let mut projected = Vec::with_capacity(statement.returning.len());
    for column in &statement.returning {
        projected.push(space.compile(&column.expr, params, catalog)?);
    }

    let mut changes = Changes::default();
    let captured = target.captures(table.root);
    for key in keys {
        let Some(row) = read_row(table, target, key)? else {
            continue;
        };
        // `RETURNING` on a delete names the row that is going away, so it is
        // read before the row stops existing.
        if !projected.is_empty() {
            let mut out = Vec::with_capacity(projected.len());
            for eval in &projected {
                out.push(space.evaluate(eval.as_ref(), &[row.as_slice()])?);
            }
            changes.returned.push(out);
        }
        // The row was read a moment ago for `RETURNING` and for the index
        // entries; reading it again inside the removal was a second descent per
        // delete, on the workload the gate measures two thousand of.
        if remove_with_triggers(
            table,
            target,
            key,
            &row,
            &statement.triggers,
            WriteRequest {
                layout: &layout,
                params,
                depth,
                indexes: IndexExprs::new(&declarations, &space),
            },
        )? {
            count_row(&mut changes, target, depth);
            if captured {
                changes.removed.push(row);
            }
        }
    }
    Ok(changes)
}
/// Removes one row, firing the `BEFORE` and `AFTER` triggers around it.
///
/// Returns whether the row was actually removed: a `RAISE(IGNORE)` in a
/// `BEFORE` body abandons it, which is not a failure and not a change.
///
/// The one place a delete happens with triggers around it, so a `DELETE`
/// statement and the delete a `REPLACE` performs to make room cannot fire
/// different things - which they would the moment there were two copies of
/// this.
///
/// @param table - the table being written
/// @param layout - the table tree's layout
/// @param target - the file and its trees
/// @param key - the row's key
/// @param row - the row as it is
/// @param triggers - the triggers this delete fires
/// @param params - the bound parameters
/// @param depth - how many triggers deep this write already is
pub(crate) fn remove_with_triggers(
    table: &TableInfo,
    target: &mut dyn WriteTarget,
    key: &[OwnedDatum],
    row: &[OwnedDatum],
    triggers: &[inillucent_sql::dml::BoundTrigger],
    request: WriteRequest<'_>,
) -> DbResult<bool> {
    let WriteRequest {
        layout,
        params,
        depth,
        indexes,
    } = request;
    if trigger::fire(
        triggers,
        TriggerTime::Before,
        target,
        &trigger::TriggerFiring {
            rows: trigger::TriggerRows {
                old: Some(row),
                new: None,
            },
            slots: &layout.slots,
            rowid: layout.rowid,
            params,
            depth,
        },
    )? == trigger::Fired::SkipRow
    {
        return Ok(false);
    }
    // A `BEFORE` body may have removed the row itself - `ON DELETE CASCADE` on
    // a self-referencing key does exactly that - so the removal is skipped
    // rather than repeated when it is already gone.
    if !row_exists(table, target, key)? {
        return Ok(false);
    }
    remove_row(table, layout, target, key, row, indexes)?;
    trigger::fire(
        triggers,
        TriggerTime::After,
        target,
        &trigger::TriggerFiring {
            rows: trigger::TriggerRows {
                old: Some(row),
                new: None,
            },
            slots: &layout.slots,
            rowid: layout.rowid,
            params,
            depth,
        },
    )?;
    Ok(true)
}
/// Removes one row and every index entry that named it.
///
/// @param table - the table being written
/// @param layout - the table tree's layout
/// @param target - the file and its trees
/// @param key - the row's key
/// @param row - the row as it stands, which the caller has already read
pub(crate) fn remove_row(
    table: &TableInfo,
    layout: &SourceLayout,
    target: &mut dyn WriteTarget,
    key: &[OwnedDatum],
    row: &[OwnedDatum],
    indexes: IndexExprs<'_>,
) -> DbResult<()> {
    for (position, index) in maintained(table) {
        if !indexes.holds(position, row)? {
            continue;
        }
        let entry = index_entry(position, index, layout, row, indexes)?;
        write_index_entry(index, target, &entry, false)?;
    }
    let (database, trees, log) = target.parts_for(table.root)?;
    let tree = trees
        .get_mut(table.root)
        .ok_or_else(|| missing_tree(table))?;
    let borrowed: Vec<Datum<'_>> = key.iter().map(OwnedDatum::borrow).collect();
    tree.delete(database, log, &borrowed)?;
    Ok(())
}
/// Fires a view's `INSTEAD OF DELETE` triggers, storing nothing.
///
/// @param statement - the bound delete, whose target is the view
/// @param target - the file and its trees
/// @param params - the bound parameters
/// @param rows - the view's rows, as its own query produced them
/// @param depth - how many triggers deep this write already is
fn delete_view(
    statement: &BoundDelete,
    target: &mut dyn WriteTarget,
    params: &Params,
    rows: &[Row],
    depth: Depth,
) -> DbResult<Changes> {
    let layout = view_layout(&statement.table);
    let mut changes = Changes::default();
    for before in rows {
        if trigger::fire(
            &statement.triggers,
            TriggerTime::InsteadOf,
            target,
            &trigger::TriggerFiring {
                rows: trigger::TriggerRows {
                    old: Some(before.as_slice()),
                    new: None,
                },
                slots: &layout.slots,
                rowid: layout.rowid,
                params,
                depth,
            },
        )? == trigger::Fired::SkipRow
        {
            continue;
        }
        count_view_row(&mut changes);
    }
    Ok(changes)
}
