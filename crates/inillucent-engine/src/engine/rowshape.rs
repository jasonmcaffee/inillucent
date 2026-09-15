//! What a table's rows look like on disk, and reading a schema back off one.
//!
//! Invariant: **the layout is decided by the import and never inferred later.**
//! A tree's column order, its key columns and its physical types are written
//! down when the tree is built, so a plan compiled against the catalog and a
//! cursor walking the leaves cannot disagree about which column is which.

use std::collections::HashMap;

use inillucent_base::error::refusal;
use inillucent_base::limits::Limits;
use inillucent_base::DbResult;
use inillucent_catalog::load::table_from_create_sql;
use inillucent_catalog::paged::{schema_create_sql, ObjectKind, SchemaEntry};
use inillucent_exec::physical::SourceLayout;
use inillucent_exec::StaticType;
use inillucent_pool::Database;
use inillucent_sql::catalog_view::{IndexInfo, TableInfo};
use inillucent_sqlite_reader::SqliteFile;
use inillucent_tree::datum::{Datum, OwnedDatum};
use inillucent_tree::types::{ColumnSpec, PhysicalType};
use inillucent_tree::PagedTree;
use inillucent_value::collation::Collation;

use crate::*;

/// Returns how a table's stored rows map onto the columns a query sees.
///
/// **One derivation, exposed rather than copied.** The import turns SQLite's
/// storage shape into the engine's - dropping the rowid-alias record field,
/// putting the rowid in the key column, reordering a `WITHOUT ROWID` table's
/// record into declared order - and `inillucent-migrate` has to perform the
/// identical transform to compare a source table against a migrated one. A
/// second implementation of it in the migration tool is the exact shape of bug
/// this workspace keeps paying for: two readers that agree until the first
/// table with a primary key declared after another column.
///
/// @param info - the table's declaration, as the catalog loader parsed it
pub fn source_layout_of(info: &TableInfo) -> DbResult<SourceLayout> {
    if info.without_rowid {
        Ok(keyed_table_shape(info)?.2)
    } else {
        Ok(table_shape(info).1)
    }
}

/// Returns one stored row as the columns a `SELECT *` produces.
///
/// The stored row is what `inillucent-sqlite-reader` hands back: for a rowid
/// table `[rowid] ++ record fields`, with the alias field NULL because SQLite
/// keeps the rowid in the cell key rather than in the record; for a
/// `WITHOUT ROWID` table, the record in SQLite's own field order.
///
/// @param info - the table's declaration
/// @param layout - the layout `source_layout_of` returned for it
/// @param stored - one row as the reader produced it
pub fn logical_row(
    info: &TableInfo,
    layout: &SourceLayout,
    stored: &[OwnedDatum],
) -> Vec<OwnedDatum> {
    // The tree row first, which is the shape the import builds.
    let tree: Vec<OwnedDatum> = if info.without_rowid {
        stored.to_vec()
    } else {
        let alias = info.rowid_alias.map(usize::from);
        let mut out = Vec::with_capacity(layout.width);
        out.push(stored.first().cloned().unwrap_or(OwnedDatum::Null));
        for (field, declared) in stored_positions(info).iter().enumerate() {
            if Some(*declared) == alias {
                continue;
            }
            out.push(
                stored
                    .get(field.saturating_add(1))
                    .cloned()
                    .unwrap_or(OwnedDatum::Null),
            );
        }
        out
    };
    // Then the declared order, which is what a query sees. `slots` is the map
    // the physical pass reads a column through, so using it here is using the
    // same answer.
    layout
        .slots
        .iter()
        .enumerate()
        .map(|(declared, slot)| {
            let value = slot
                .and_then(|index| tree.get(index).cloned())
                .unwrap_or(OwnedDatum::Null);
            let physical = info
                .columns
                .get(declared)
                .map(|column| physical_for(column.affinity).0)
                .unwrap_or(PhysicalType::Any);
            stored_as(physical, value)
        })
        .collect()
}

/// Returns what a column of a given layout hands back for a value put into it.
///
/// **One conversion, and it is the dialect's.** `leaf::classify_at` excepts
/// every mismatched value into the heap and returns it unchanged, with a single
/// deliberate exception: an integer in a column whose affinity is REAL is
/// *converted*, because that is what REAL affinity means - SQLite stores 7 in a
/// REAL column as 7.0 - and because excepting it would take such a column off
/// the vectorised path one whole-numbered row at a time.
///
/// So a migration that read `Int(7)` out of a SQLite record and compared it
/// against the `Real(7.0)` the new engine hands back would call a correct copy
/// wrong. This is that one rule, written where the comparison needs it.
///
/// It is guarded by measurement rather than by comment: the migration
/// acceptance runs every SQLite feature-parity fixture, and its digests fail
/// the moment this and `classify_at` disagree about any value in any of them.
///
/// @param physical - the column's layout
/// @param value - the value as the source held it
pub fn stored_as(physical: PhysicalType, value: OwnedDatum) -> OwnedDatum {
    match (physical, &value) {
        (PhysicalType::Float64, OwnedDatum::Int(number)) => OwnedDatum::Real(*number as f64),
        _ => value,
    }
}

/// Imports one table into a rowid-clustered PAX tree.
///
/// @param database - the file the tree is built in
/// @param file - the open fixture
/// @param info - the table's catalog entry
pub(crate) fn import_table(
    database: &mut Database,
    file: &mut SqliteFile,
    info: &TableInfo,
) -> DbResult<(TreeShape, SourceLayout)> {
    // The record's width is the count of *stored* columns, not of declared
    // ones: SQLite writes no field for a `VIRTUAL` generated column.
    let positions = stored_positions(info);
    let raw = file.read_table(info.root, positions.len())?;
    // `read_table` returns [rowid] ++ record slots. The rowid alias slot holds
    // NULL in every SQLite record, so it is dropped and the rowid takes its
    // place as the tree's key column.
    let alias = info.rowid_alias.map(usize::from);
    let (columns, layout) = table_shape(info);
    let width = layout.width;

    let mut rows: Vec<Vec<OwnedDatum>> = Vec::with_capacity(raw.len());
    for row in raw {
        let mut out: Vec<OwnedDatum> = Vec::with_capacity(width);
        out.push(row.first().cloned().unwrap_or(OwnedDatum::Null));
        for (field, declared) in positions.iter().enumerate() {
            if Some(*declared) == alias {
                continue;
            }
            out.push(
                row.get(field.saturating_add(1))
                    .cloned()
                    .unwrap_or(OwnedDatum::Null),
            );
        }
        rows.push(out);
    }

    let borrowed: Vec<Vec<Datum<'_>>> = rows
        .iter()
        .map(|row| row.iter().map(OwnedDatum::borrow).collect())
        .collect();
    let tree = PagedTree::bulk_build(
        database,
        u64::from(info.root),
        columns.clone(),
        1,
        &borrowed,
    )?;
    Ok((
        TreeShape {
            root: tree.root(),
            columns,
            key_columns: 1,
            first_leaf: tree.first_leaf(),
            leaf_count: tree.leaf_count(),
            row_count: tree.row_count(),
        },
        layout,
    ))
}

/// Returns the column directory and the layout a rowid table's tree has.
///
/// **Derived from the declaration alone**, which is what lets `CREATE TABLE`
/// and the fixture import agree by construction rather than by two people
/// writing the same rule twice. The import supplies rows read out of a SQLite
/// file and the DDL path supplies none; neither supplies a shape.
///
/// The physical type of each tree column comes from the declared affinity, and
/// that is a claim rather than a guarantee - a column declared `INTEGER` may
/// hold a string - which is exactly what the leaf's exception class is for.
///
/// @param info - the table's declaration
pub(crate) fn table_shape(info: &TableInfo) -> (Vec<ColumnSpec>, SourceLayout) {
    let alias = info.rowid_alias.map(usize::from);
    // `slots` is indexed by *declared* position and `None` means the tree does
    // not carry that column, which is exactly what a `VIRTUAL` generated column
    // is: it takes no record field and no tree column, and every column
    // declared after one therefore sits that many places earlier in the tree.
    let mut slots: Vec<Option<usize>> = vec![None; info.columns.len()];
    let mut next = 1usize;
    let mut columns = vec![ColumnSpec::key(PhysicalType::Int64)];
    let mut types = vec![StaticType::Int];
    for declared in stored_positions(info) {
        if Some(declared) == alias {
            if let Some(slot) = slots.get_mut(declared) {
                *slot = Some(0);
            }
            continue;
        }
        let (physical, static_type) = match info.columns.get(declared) {
            Some(column) => physical_for(column.affinity),
            None => (PhysicalType::Any, StaticType::Unknown),
        };
        columns.push(
            ColumnSpec::new(physical).with_collation(
                info.columns
                    .get(declared)
                    .map(|column| collation_of(&column.collation))
                    .unwrap_or(Collation::Binary),
            ),
        );
        types.push(static_type);
        if let Some(slot) = slots.get_mut(declared) {
            *slot = Some(next);
        }
        next = next.saturating_add(1);
    }
    let width = next;
    (
        columns,
        SourceLayout {
            tree_key: info.root,
            slots,
            rowid: Some(0),
            // A rowid table's row is identified by its rowid, and by nothing
            // else - which is what makes an index entry over one a single
            // trailing column.
            identity: vec![0],
            types,
            width,
            // A rowid-clustered tree is ordered by its rowid, which is column 0.
            key_columns: vec![0],
        },
    )
}

/// Imports one `WITHOUT ROWID` table into a key-ordered PAX tree.
///
/// A `WITHOUT ROWID` table *is* an index b-tree: there is no separate table
/// b-tree and no rowid, and the record holds every column with the primary key's
/// columns first. So the import is the index import with the whole record as the
/// row and the primary key as the key - and the resulting tree needs nothing the
/// engine does not already do, because an index tree has a multi-column key too.
///
/// **The field order is SQLite's, not the declaration's.** For
/// `CREATE TABLE t(a, b, PRIMARY KEY(b))` the record is `(b, a)`, and
/// `primary_key_position` is what says so. Reconstructing that order by guessing
/// - assuming the key is a prefix of the declared columns, say - would read the
/// right bytes into the wrong columns on any table whose primary key is not
/// written first, and every value would still be a plausible value.
///
/// @param database - the file the tree is built in
/// @param file - the open fixture
/// @param info - the table's catalog entry
pub(crate) fn import_keyed_table(
    database: &mut Database,
    file: &mut SqliteFile,
    info: &TableInfo,
) -> DbResult<(TreeShape, SourceLayout)> {
    let (columns, key_columns, layout) = keyed_table_shape(info)?;
    // The record's width is the count of *stored* columns: SQLite writes no
    // field for a `VIRTUAL` generated column here either.
    let rows = file.read_index(info.root, layout.width)?;
    let rows = in_key_order(rows, &columns, key_columns);
    let borrowed: Vec<Vec<Datum<'_>>> = rows
        .iter()
        .map(|row| row.iter().map(OwnedDatum::borrow).collect())
        .collect();
    let tree = PagedTree::bulk_build(
        database,
        u64::from(info.root),
        columns.clone(),
        key_columns,
        &borrowed,
    )?;
    Ok((
        TreeShape {
            root: tree.root(),
            columns,
            key_columns,
            first_leaf: tree.first_leaf(),
            leaf_count: tree.leaf_count(),
            row_count: tree.row_count(),
        },
        layout,
    ))
}

/// Returns the column directory, key width and layout of a `WITHOUT ROWID`
/// table's tree.
///
/// **The field order is SQLite's, not the declaration's.** For
/// `CREATE TABLE t(a, b, PRIMARY KEY(b))` the record is `(b, a)`, and
/// `primary_key_position` is what says so.
///
/// @param info - the table's declaration
pub(crate) fn keyed_table_shape(
    info: &TableInfo,
) -> DbResult<(Vec<ColumnSpec>, usize, SourceLayout)> {
    let width = info.columns.len();
    // The record's field order: primary-key columns in their key order, then
    // every other column in declaration order.
    let mut order: Vec<usize> = Vec::with_capacity(width);
    let mut keyed: Vec<(u16, usize)> = info
        .columns
        .iter()
        .enumerate()
        .filter_map(|(slot, column)| column.primary_key_position.map(|at| (at, slot)))
        .collect();
    keyed.sort_unstable();
    let key_columns = keyed.len();
    if key_columns == 0 {
        return Err(refusal(
            "a WITHOUT ROWID table with no primary key cannot be keyed",
        ));
    }
    order.extend(keyed.iter().map(|(_, slot)| *slot));
    // A `VIRTUAL` generated column is in no record and so in no tree column,
    // here for the same reason it is in none of a rowid table's - and SQLite
    // will not let one be part of a primary key, so the filter is only needed
    // over the columns that follow the key.
    for slot in 0..width {
        if !order.contains(&slot) && !is_virtual_column(info, slot) {
            order.push(slot);
        }
    }
    let stored_width = order.len();
    let mut columns = Vec::with_capacity(stored_width);
    let mut types = Vec::with_capacity(stored_width);
    // `slots[declared] = tree column`, which is the inverse of `order`.
    let mut slots: Vec<Option<usize>> = vec![None; width];
    for (position, declared) in order.iter().enumerate() {
        let (physical, static_type) = match info.columns.get(*declared) {
            Some(column) => physical_for(column.affinity),
            None => (PhysicalType::Any, StaticType::Unknown),
        };
        let collation = info
            .columns
            .get(*declared)
            .map(|column| collation_of(&column.collation))
            .unwrap_or(Collation::Binary);
        let spec = if position < key_columns {
            ColumnSpec::key(physical)
        } else {
            ColumnSpec::new(physical)
        };
        columns.push(spec.with_collation(collation));
        types.push(static_type);
        if let Some(slot) = slots.get_mut(*declared) {
            *slot = Some(position);
        }
    }
    // A non-binary collation means the tree is *seekable* but not "already
    // sorted" for an ORDER BY that did not name the same collation, which is
    // the same disqualification `index_shape` makes and for the same reason.
    let ordered = columns
        .iter()
        .take(key_columns)
        .all(|spec| spec.collation == Collation::Binary);
    Ok((
        columns,
        key_columns,
        SourceLayout {
            tree_key: info.root,
            slots,
            // There is no rowid: that is what `WITHOUT ROWID` means, and a
            // query that asks for one is refused rather than given the key.
            rowid: None,
            // The primary key is what identifies the row instead, and it is the
            // leading `key_columns` of the record. Stated here unconditionally
            // rather than read off `key_columns`, which is emptied when the
            // tree is not "already sorted" and would leave a `DESC` or collated
            // primary key with no identity at all.
            identity: (0..key_columns).collect(),
            types,
            width: stored_width,
            key_columns: if ordered {
                (0..key_columns).collect()
            } else {
                Vec::new()
            },
        },
    ))
}

/// Imports one index into a key-ordered PAX tree.
///
/// An index entry is the indexed columns followed by the rowid, which is
/// already the new format's index-tree row shape, so nothing is rearranged.
///
/// @param database - the file the tree is built in
/// @param file - the open fixture
/// @param table - the indexed table's catalog entry
/// @param index - the index's catalog entry
/// @param root - the index's root page
pub(crate) fn import_index(
    database: &mut Database,
    file: &mut SqliteFile,
    table: &TableInfo,
    index: &IndexInfo,
    root: u32,
) -> DbResult<(TreeShape, SourceLayout)> {
    let key_columns = index.columns.len().saturating_add(1);
    let rows = file.read_index(root, key_columns)?;
    let (columns, layout) = index_shape(table, index, root);
    let rows = in_key_order(rows, &columns, key_columns);
    let borrowed: Vec<Vec<Datum<'_>>> = rows
        .iter()
        .map(|row| row.iter().map(OwnedDatum::borrow).collect())
        .collect();
    let tree = PagedTree::bulk_build(
        database,
        u64::from(root),
        columns.clone(),
        key_columns,
        &borrowed,
    )?;
    Ok((
        TreeShape {
            root: tree.root(),
            columns,
            key_columns,
            first_leaf: tree.first_leaf(),
            leaf_count: tree.leaf_count(),
            row_count: tree.row_count(),
        },
        layout,
    ))
}

/// Returns a starting point for a connection's `random()` stream.
///
/// The wall clock and the process id. Neither is a secret and neither has to
/// be: SQLite's own `random()` is not a cryptographic generator either, and
/// what this exists to avoid is two connections - or two runs - answering the
/// same sequence. A clock that has not moved since the last open still gives a
/// different stream, because the process id is in it.
pub(crate) fn fresh_seed() -> u64 {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|held| held.as_nanos() as u64)
        .unwrap_or(0);
    nanos ^ (u64::from(std::process::id()).wrapping_mul(0x9E37_79B9_7F4A_7C15))
}

/// Builds the entry an index should hold for one table row.
///
/// Only for an index whose keys are plain columns, which is what the caller has
/// already established: a computed key would need the binder to evaluate.
///
/// @param index - the index
/// @param layout - the table tree's layout
/// @param row - the row, in tree-column order
pub(crate) fn plain_index_entry(
    index: &IndexInfo,
    layout: &SourceLayout,
    row: &[OwnedDatum],
) -> Vec<OwnedDatum> {
    let trailing = layout.identity.len().max(1);
    let mut entry = Vec::with_capacity(index.columns.len().saturating_add(trailing));
    for column in &index.columns {
        entry.push(
            column
                .column
                .and_then(|declared| layout.slots.get(usize::from(declared)).copied().flatten())
                .and_then(|slot| row.get(slot).cloned())
                .unwrap_or(OwnedDatum::Null),
        );
    }
    if layout.identity.is_empty() {
        entry.push(
            layout
                .rowid
                .and_then(|slot| row.get(slot).cloned())
                .unwrap_or(OwnedDatum::Null),
        );
        return entry;
    }
    for slot in &layout.identity {
        entry.push(row.get(*slot).cloned().unwrap_or(OwnedDatum::Null));
    }
    entry
}

/// Names a row the way `integrity_check`'s message does, from its index entry.
///
/// An entry is the indexed columns and then whatever identifies the row: the
/// rowid for an ordinary table, and the primary key's columns for a
/// `WITHOUT ROWID` one, which has no rowid to be named by.
///
/// @param entry - the entry the table implied
/// @param width - how many leading columns are the index's own key
pub(crate) fn entry_identity_text(entry: &[OwnedDatum], width: usize) -> String {
    let identity = entry.get(width..).unwrap_or_default();
    if identity.is_empty() {
        return "?".to_string();
    }
    identity
        .iter()
        .map(|value| match value {
            OwnedDatum::Int(number) => number.to_string(),
            OwnedDatum::Text(text) => String::from_utf8_lossy(text).into_owned(),
            _ => "?".to_string(),
        })
        .collect::<Vec<String>>()
        .join(",")
}

/// Returns the error `integrity_check` reports a disagreement through.
///
/// A corruption rather than a refusal, because that is what it is - and the
/// pragma prints the detail, so the text SQLite would have printed is the
/// detail rather than the message.
///
/// @param said - what SQLite's own checker would say
pub(crate) fn corrupt_index(said: String) -> DbError {
    inillucent_base::error::corrupt(said.clone()).with_detail(said)
}

/// Returns the column directory and the layout an index tree is built with.
///
/// Shared by the fixture import, which fills the tree from SQLite's own index
/// pages, and by `CREATE INDEX`, which fills it from the table tree. The shape
/// is the same question in both cases and is answered in one place.
///
/// **What follows the indexed columns is whatever identifies the table row**:
/// a rowid for an ordinary table, and the *primary key's columns* for a
/// `WITHOUT ROWID` one, which is SQLite's rule and the reason such an index was
/// refused here until now. `SourceLayout::identity` names them, so a
/// non-covering seek probes the table with the right key without having to
/// re-derive which columns those were.
///
/// @param table - the table the index is on
/// @param index - the index's declaration
/// @param root - the identifier the tree is registered under
pub(crate) fn index_shape(
    table: &TableInfo,
    index: &IndexInfo,
    root: u32,
) -> (Vec<ColumnSpec>, SourceLayout) {
    let trailing = identity_columns(table);
    let key_columns = index.columns.len().saturating_add(trailing.len().max(1));
    let mut columns = Vec::with_capacity(key_columns);
    let mut types = Vec::with_capacity(key_columns);
    let mut slots: Vec<Option<usize>> = vec![None; table.columns.len()];
    let mut ordered = true;
    for (position, column) in index.columns.iter().enumerate() {
        // An expression key indexes no table column, so nothing maps onto it
        // and a query that reads the underlying column cannot be answered from
        // this tree. Leaving the slot unmapped is what makes that a refusal in
        // the physical pass rather than a wrong answer here.
        // **A `DESC` key column is stored descending**, which is what SQLite
        // stores and what makes the two engines read a `DESC` index in the same
        // order - the trailing rowid stays ascending, so ties inside a
        // descending column come out ascending in both. It used to be flattened
        // to ascending, and the visible cost was that `ORDER BY k`
        // over a `DESC` index answered its ties in the opposite order to
        // SQLite's, on every one of the seven statements `ordering.rs` names.
        let Some(declared) = column.column.map(usize::from) else {
            columns.push(ColumnSpec::key(PhysicalType::Any).with_descending(column.descending));
            types.push(StaticType::Unknown);
            continue;
        };
        let (physical, static_type) = match table.columns.get(declared) {
            Some(info) => physical_for(info.affinity),
            None => (PhysicalType::Any, StaticType::Unknown),
        };
        // An index column's collation is the one the *index* declared, and the
        // column's own only when the index did not name one. That is SQLite's
        // rule and it is the order the entries are physically in, which is what
        // the tree's comparisons have to agree with.
        let collation = if column.collation.is_empty() {
            table
                .columns
                .get(declared)
                .map(|info| collation_of(&info.collation))
                .unwrap_or(Collation::Binary)
        } else {
            collation_of(&column.collation)
        };
        if collation != Collation::Binary {
            // A tree ordered by anything but BINARY is still *seekable* - the
            // comparisons below use the collation - but it is not "already
            // sorted" for an `ORDER BY` that did not name the same collation,
            // and the executor's streaming rules cannot express that
            // distinction. Disqualifying it costs a sort and never an answer.
            ordered = false;
        }
        if column.descending {
            // And a descending key column for the same reason, now that one
            // means what it says. `SourceLayout::key_columns` is read by rules
            // that ask only *which* columns the walk is ordered by - never in
            // which direction - so a descending tree reported through it says
            // "ascending by a, then b" about a walk that is descending by a.
            // `SELECT a, b FROM t ORDER BY a, b` over `t(a DESC, b)` then
            // skipped its sorter and came back in the index's own order, which
            // is the reverse of the answer.
            //
            // The planner's own `ordering_provided` is direction-aware and
            // still elides the sort where a walk really does answer the
            // ordering, so what this gives up is the executor's second,
            // direction-blind derivation of the same claim.
            ordered = false;
        }
        columns.push(
            ColumnSpec::key(physical)
                .with_collation(collation)
                .with_descending(column.descending),
        );
        types.push(static_type);
        if let Some(slot) = slots.get_mut(declared) {
            *slot = Some(position);
        }
    }
    // What identifies the table row, at the end of the entry.
    let indexed = index.columns.len();
    let identity: Vec<usize> = if trailing.is_empty() {
        // An ordinary table: one rowid column, which is also the table's
        // rowid-alias column when it declared one.
        columns.push(ColumnSpec::key(PhysicalType::Int64));
        types.push(StaticType::Int);
        if let Some(alias) = table.rowid_alias.map(usize::from) {
            if let Some(slot) = slots.get_mut(alias) {
                *slot = Some(indexed);
            }
        }
        vec![indexed]
    } else {
        // A `WITHOUT ROWID` table: its primary key, in its key order. Each of
        // those columns is genuinely carried by this tree, so its slot is
        // mapped and a query reading a primary-key column can be answered from
        // the entry - which is what an index on such a table is worth.
        for (offset, declared) in trailing.iter().enumerate() {
            let (physical, static_type) = match table.columns.get(*declared) {
                Some(info) => physical_for(info.affinity),
                None => (PhysicalType::Any, StaticType::Unknown),
            };
            let collation = table
                .columns
                .get(*declared)
                .map(|info| collation_of(&info.collation))
                .unwrap_or(Collation::Binary);
            if collation != Collation::Binary {
                ordered = false;
            }
            columns.push(ColumnSpec::key(physical).with_collation(collation));
            types.push(static_type);
            if let Some(slot) = slots.get_mut(*declared) {
                // An indexed column that is also a primary-key column keeps the
                // slot it already has: the entry holds it twice, and reading the
                // first copy is what the planner already expects.
                if slot.is_none() {
                    *slot = Some(indexed.saturating_add(offset));
                }
            }
        }
        (indexed..key_columns).collect()
    };
    let rowid_position = trailing.is_empty().then_some(indexed);

    (
        columns,
        SourceLayout {
            tree_key: root,
            slots,
            rowid: rowid_position,
            identity,
            types,
            width: key_columns,
            // An index tree is ordered by every column of its entry, in order:
            // the indexed columns then whatever identifies the row. A
            // descending index column would break that, which is why one
            // disqualifies the tree above.
            key_columns: if ordered {
                (0..key_columns).collect()
            } else {
                Vec::new()
            },
        },
    )
}

/// Returns the declared columns that identify one of a table's rows.
///
/// Empty for a rowid table, whose rows are identified by a rowid rather than by
/// any declared column; the primary key's columns in key order for a `WITHOUT
/// ROWID` one. It is the one place that answers the question, so the index
/// shape, the build path and the write path cannot disagree about what an entry
/// carries.
///
/// @param table - the table
pub fn identity_columns(table: &TableInfo) -> Vec<usize> {
    if !table.without_rowid {
        return Vec::new();
    }
    let mut keyed: Vec<(u16, usize)> = table
        .columns
        .iter()
        .enumerate()
        .filter_map(|(slot, column)| column.primary_key_position.map(|at| (at, slot)))
        .collect();
    keyed.sort_unstable();
    keyed.into_iter().map(|(_, slot)| slot).collect()
}

/// Reports whether an index's tree may stand in for a scan of its table.
///
/// **A partial index holds only the rows its predicate accepted, so it may
/// not.** The physical pass replaces a plain table scan with a scan of the
/// smallest tree that carries every column the query reads, and it decides
/// "carries every column" by building the pipeline against the candidate's
/// layout and seeing whether it translates. That test is about *columns*; it
/// cannot see that a tree holds fewer rows than the table, so a partial index
/// offered as a candidate answers `SELECT rowid FROM t` with the rows inside
/// the predicate and no error.
///
/// It was measured exactly that way: with `CREATE INDEX ix ON u(a) WHERE b > 5`
/// on a two-row table, `SELECT rowid FROM u` returned one row while
/// `SELECT rowid FROM u WHERE b = 1` - which the index cannot cover, so it fell
/// back to the table - returned the other. Both plans said `SCAN u`, because
/// this substitution happens after the planner has spoken.
///
/// An index on an *expression* is fine here: it holds an entry for every row,
/// and the columns it does not carry are unmapped in its layout, so the
/// translation test already refuses it for a query that reads one.
///
/// @param index - the index being considered
pub(crate) fn covers_every_row(index: &IndexInfo) -> bool {
    index.partial_sql.is_none()
}

/// Returns the collation a folded name refers to.
///
/// The three built-in ones. An application-defined collation is not something
/// the new engine can order a tree by - it would have to call back into the
/// connection that registered it on every comparison - so it is treated as
/// BINARY here and the index that uses it is disqualified from being "already
/// sorted", which is the same conservative answer a descending column gets.
///
/// @param folded - the collation's folded name, empty for none
fn collation_of(folded: &[u8]) -> Collation {
    match folded.to_ascii_uppercase().as_slice() {
        b"NOCASE" => Collation::NoCase,
        b"RTRIM" => Collation::RTrim,
        _ => Collation::Binary,
    }
}

/// Chooses a mini-column layout for a declared affinity.
///
/// The mapping is the obvious one and the honesty is in what it does *not*
/// claim: `Blob` affinity (SQLite's "no affinity") gets the `Any` layout, since
/// a column with no affinity has no type to specialise on, and `Numeric` gets
/// `Any` too because it holds integers and reals interchangeably.
///
/// @param affinity - the declared affinity
fn physical_for(affinity: inillucent_value::affinity::Affinity) -> (PhysicalType, StaticType) {
    use inillucent_value::affinity::Affinity;
    match affinity {
        Affinity::Integer => (PhysicalType::Int64, StaticType::Int),
        Affinity::Real => (PhysicalType::Float64, StaticType::Real),
        Affinity::Text => (PhysicalType::Text, StaticType::Text),
        Affinity::Blob => (PhysicalType::Blob, StaticType::Unknown),
        Affinity::Numeric | Affinity::FlexNum => (PhysicalType::Any, StaticType::Unknown),
    }
}

/// Everything one database file's catalog tree describes.
///
/// **One derivation, for `main` and for every `ATTACH`ed file.** The shapes a
/// query is planned against are derived from the `CREATE` text the catalog row
/// carries; deriving them twice - once for the file a connection was opened on
/// and once for a file it attached - is how the two come to disagree about a
/// generated column or a `WITHOUT ROWID` key, which is a wrong answer rather
/// than a refusal.
pub(crate) struct LoadedSchema {
    /// Every tree this file holds, keyed by the connection's handle for it.
    pub(crate) trees: HashMap<u32, PagedTree>,
    /// Each tree's layout, keyed the same way.
    pub(crate) layouts: HashMap<u32, std::rc::Rc<SourceLayout>>,
    /// For each table's handle, its index handles.
    pub(crate) covering: HashMap<u32, Vec<u32>>,
    /// The catalog rows, with the handle each object's tree is registered under.
    pub(crate) entries: Vec<Recorded>,
    /// The tables the binder resolves names against, `sqlite_schema` excepted.
    pub(crate) tables: Vec<TableInfo>,
    /// This file's own `sqlite_schema` declaration.
    pub(crate) schema_info: TableInfo,
    /// The handle each of this file's local tree identifiers is registered under.
    pub(crate) handles: HashMap<u64, u32>,
    /// The objects whose `CREATE` text this engine could not re-read.
    pub(crate) skipped: Vec<String>,
    /// The largest local identifier the file holds, so the next one is past it.
    pub(crate) highest_identifier: u32,
}

/// Everything one schema's load accumulates while it is being loaded.
///
/// **The nine locals `load_schema` carried, named once (task-1962, A8).** Five
/// passes over the catalog rows each wrote into some of them, which is what
/// made the function 329 lines: no pass could be read without the other four in
/// view. Each pass is a method now and this is what they share.
struct Loading<'a> {
    /// The file being loaded.
    database: &'a Database,
    /// Which schema this is, as the binder numbers them.
    index: usize,
    /// The catalog rows as they are stored, with the rowid of each.
    stored_rows: Vec<(i64, SchemaEntry)>,
    /// The trees, by the handle each is registered under.
    trees: HashMap<u32, PagedTree>,
    /// The layout of each tree's rows.
    layouts: HashMap<u32, std::rc::Rc<SourceLayout>>,
    /// For each table's handle, its covering index handles.
    covering: HashMap<u32, Vec<u32>>,
    /// The catalog rows kept, each with the rowid it is stored under.
    entries: Vec<(i64, SchemaEntry)>,
    /// The handle each kept row's tree is registered under, parallel to
    /// `entries`.
    identifiers: Vec<u32>,
    /// The objects whose `CREATE` text this engine could not re-read.
    skipped: Vec<String>,
    /// The handle each of this file's local tree identifiers is registered
    /// under.
    handles: HashMap<u64, u32>,
    /// Every table by folded name, because an index's shape is derived against
    /// its table's declaration and the catalog does not order tables before
    /// their indexes.
    infos: HashMap<Vec<u8>, (u32, TableInfo)>,
    /// The largest local identifier the file holds, so the next one is past it.
    highest_identifier: u32,
}

impl<'a> Loading<'a> {
    /// Starts a load over one file's catalog rows.
    ///
    /// @param database - the file
    /// @param index - which schema this is, as the binder numbers them
    /// @param stored_rows - the catalog rows, with the rowid of each
    fn over(
        database: &'a Database,
        index: usize,
        stored_rows: Vec<(i64, SchemaEntry)>,
    ) -> Loading<'a> {
        Loading {
            database,
            index,
            stored_rows,
            trees: HashMap::new(),
            layouts: HashMap::new(),
            covering: HashMap::new(),
            entries: Vec::new(),
            identifiers: Vec::new(),
            skipped: Vec::new(),
            handles: HashMap::new(),
            infos: HashMap::new(),
            highest_identifier: 0,
        }
    }

    /// Records one catalog row, with the handle its tree is registered under.
    ///
    /// **`entries` and `identifiers` are zipped into `Recorded` at the end, so
    /// a row pushed to one has to be pushed to the other.** Pushing only the
    /// entry shifted every later object onto the previous one's tree - which
    /// read as a table whose covering index answered another table's rows, and
    /// cost an afternoon to find. Zero is what `Recorded.root` documents for an
    /// object with no tree, which is a virtual table, a view and a trigger.
    ///
    /// **The rowid is the one the row is stored under, not its position here.**
    /// The passes below visit the tables and then the indexes, which is not the
    /// order the catalog holds them in - a schema that creates a table, an
    /// index, another table interleaves the two. The rowid used to be
    /// reconstructed from the position in this reordered list, so every object
    /// after the first index was numbered as some other object. The number is
    /// what `seal` and every later `DROP` write by, so the next catalog write
    /// landed on the wrong row: rows came back duplicated and rows came back
    /// missing.
    ///
    /// @param entry - the catalog row
    /// @param identifier - the handle its tree is registered under, or zero
    fn keep(&mut self, entry: &SchemaEntry, identifier: u32) {
        let rowid = self
            .stored_rows
            .iter()
            .find(|(_, held)| held.name == entry.name && held.kind == entry.kind)
            .map(|(rowid, _)| *rowid)
            .unwrap_or_default();
        self.entries.push((rowid, entry.clone()));
        self.identifiers.push(identifier);
    }

    /// Records an object whose `CREATE` text this engine could not re-read.
    ///
    /// @param entry - the catalog row
    fn skip(&mut self, entry: &SchemaEntry) {
        self.skipped
            .push(String::from_utf8_lossy(&entry.name).into_owned());
    }

    /// Hands out this connection's handle for a file's own tree identifier.
    ///
    /// **The identifier comes out of the catalog row, not out of a counter.**
    /// It used to be handed out in catalog order, on the reasoning that it was
    /// this process's own bookkeeping. It is not: every logical row record in
    /// the log carries it, so a reader that numbered trees differently from the
    /// writer would hand recovery's records to the wrong tree - a wrong answer
    /// rather than a refusal. The identifier is stored in the catalog now; this
    /// reads it back, and `highest_identifier` is kept so a `CREATE TABLE`
    /// after this open cannot collide with one already in the file, which a
    /// counter that restarted at every open could and did.
    ///
    /// @param entry - the catalog row
    /// @param allocate - hands out the connection's handle
    fn register(
        &mut self,
        entry: &SchemaEntry,
        allocate: &mut dyn FnMut(u32) -> u32,
    ) -> DbResult<(u32, u32)> {
        let local = crate::recovery::identifier_of(entry)?;
        self.highest_identifier = self.highest_identifier.max(local);
        let identifier = allocate(local);
        self.handles.insert(u64::from(local), identifier);
        Ok((local, identifier))
    }

    /// Attaches the tree one catalog row names.
    ///
    /// **The tree keeps the identifier its own file numbered it with.** Every
    /// log record it writes carries this number and the log outlives the
    /// process, so it is the file's business. The map key beside it is the
    /// connection's handle, which is not.
    ///
    /// @param local - the file's own identifier for the tree
    /// @param entry - the catalog row
    /// @param columns - the columns one of its rows has
    /// @param key_columns - how many of those are the key
    fn attach_tree(
        &self,
        local: u32,
        entry: &SchemaEntry,
        columns: Vec<ColumnSpec>,
        key_columns: usize,
    ) -> DbResult<PagedTree> {
        PagedTree::attach(
            self.database.pool(),
            u64::from(local),
            entry.root,
            columns,
            key_columns,
            entry.stats.leaf_count,
            entry.stats.row_count,
        )
    }

    /// Loads every table the catalog declares.
    ///
    /// @param stored - the catalog rows
    /// @param allocate - hands out the connection's handle for a tree
    fn tables_from(
        &mut self,
        stored: &[SchemaEntry],
        allocate: &mut dyn FnMut(u32) -> u32,
    ) -> DbResult<()> {
        for entry in stored {
            if entry.kind != ObjectKind::Table {
                continue;
            }
            // A virtual table has **no tree of its own**. Its rows live in the
            // shadow tables the module declared, which are ordinary tables in
            // this same catalog and are loaded by this same loop. So its row
            // carries no tree identifier, and asking for one refused to open
            // every database holding a search table - which is how this was
            // found, by moving `inillucent-migrate` onto the engine.
            //
            // The kind is learned from a throwaway parse rather than from the
            // parse below, because that one is given the identifier and every
            // shape it derives is derived against it. Parsing once with a
            // placeholder root and patching `info.root` afterwards looked like
            // the same thing and was not: it left the *derived* shapes pointing
            // at the placeholder, and every table then scanned the same tree -
            // `count(*)` answered the same number for every table in the file.
            if matches!(
                table_from_create_sql(&entry.sql, self.index, 0).map(|info| info.kind),
                Ok(inillucent_sql::catalog_view::TableKind::Virtual)
            ) {
                self.keep(entry, 0);
                continue;
            }
            let (local, identifier) = self.register(entry, allocate)?;
            let mut info = match table_from_create_sql(&entry.sql, self.index, identifier) {
                Ok(info) => info,
                Err(_) => {
                    self.skip(entry);
                    continue;
                }
            };
            info.root = identifier;
            let (columns, key_columns, layout) = if info.without_rowid {
                match keyed_table_shape(&info) {
                    Ok((columns, key_columns, layout)) => (columns, key_columns, layout),
                    Err(_) => {
                        self.skip(entry);
                        continue;
                    }
                }
            } else {
                let (columns, layout) = table_shape(&info);
                (columns, 1, layout)
            };
            let tree = self.attach_tree(local, entry, columns, key_columns)?;
            self.trees.insert(identifier, tree);
            self.layouts.insert(identifier, std::rc::Rc::new(layout));
            self.infos
                .insert(info.folded.clone(), (identifier, info.clone()));
            self.keep(entry, identifier);
        }
        Ok(())
    }

    /// Loads every index the catalog declares, against its table.
    ///
    /// @param stored - the catalog rows
    /// @param allocate - hands out the connection's handle for a tree
    fn indexes_from(
        &mut self,
        stored: &[SchemaEntry],
        allocate: &mut dyn FnMut(u32) -> u32,
    ) -> DbResult<()> {
        for entry in stored {
            if entry.kind != ObjectKind::Index {
                continue;
            }
            let folded = entry.table.to_ascii_lowercase();
            let Some((table_root, table_info)) = self.infos.get(&folded).cloned() else {
                self.skip(entry);
                continue;
            };
            let (local, identifier) = self.register(entry, allocate)?;
            let Some(index) = self.declaration_of(entry, &table_info, identifier) else {
                self.skip(entry);
                continue;
            };
            let (columns, layout) = index_shape(&table_info, &index, identifier);
            let key_columns = columns.len();
            let tree = self.attach_tree(local, entry, columns, key_columns)?;
            self.trees.insert(identifier, tree);
            self.layouts.insert(identifier, std::rc::Rc::new(layout));
            if covers_every_row(&index) {
                self.covering
                    .entry(table_root)
                    .or_default()
                    .push(identifier);
            }
            // The index joins its table's declaration, so the binder offers it
            // to the planner exactly as the import does. An automatic one is
            // already there - the table's own text declared it - so its root is
            // filled in rather than a second copy pushed.
            if let Some((_, info)) = self.infos.get_mut(&folded) {
                match info
                    .indexes
                    .iter_mut()
                    .find(|held| held.folded == index.folded)
                {
                    Some(held) => held.root = identifier,
                    None => info.indexes.push(index),
                }
            }
            self.keep(entry, identifier);
        }
        Ok(())
    }

    /// Reads one index's declaration out of the catalog, or out of its table.
    ///
    /// **An automatic index is declared by the *table's* text**, and the
    /// catalog stores an empty statement for it - which is what SQLite writes
    /// for `sqlite_autoindex_t_1`. Parsing that empty text as a `CREATE INDEX`
    /// fails, and this used to skip the index: no tree was attached, no row
    /// joined `entries`, and the declaration the planner reads kept the root of
    /// zero it was parsed with. The visible result was that **any query using a
    /// non-`INTEGER PRIMARY KEY` failed after a reopen** - `EXPLAIN QUERY PLAN`
    /// named the index and the statement answered `no layout imported for root
    /// page 0`. It is the same rule `tables_from_entries` and `shape_of`
    /// already apply.
    ///
    /// @param entry - the index's catalog row
    /// @param table_info - the table it is declared against
    /// @param identifier - the handle its tree is registered under
    fn declaration_of(
        &self,
        entry: &SchemaEntry,
        table_info: &TableInfo,
        identifier: u32,
    ) -> Option<IndexInfo> {
        if entry.sql.is_empty() {
            let wanted = entry.name.to_ascii_lowercase();
            let mut index = table_info
                .indexes
                .iter()
                .find(|index| index.folded == wanted)
                .cloned()?;
            index.root = identifier;
            return Some(index);
        }
        inillucent_catalog::load::index_from_create_sql(&entry.sql, table_info, identifier).ok()
    }

    /// Records every view the catalog declares.
    ///
    /// **A view is a row and nothing else, and this pass was missing.** The
    /// catalog carried it - `SELECT type, name FROM sqlite_schema` listed
    /// `view|v` - but nothing put it into `entries`, so `tables_from_entries`
    /// never saw it and the binder never learned the name. `SELECT * FROM v`
    /// answered `no such table: v` against a schema that says the view is
    /// there, which is worse than a schema that dropped it: the object is
    /// listed and unreadable.
    ///
    /// It was not only the migration path. A view created by `CREATE VIEW`,
    /// queried, and then read again after a close and reopen was gone the same
    /// way, because this is the function every open goes through.
    ///
    /// It runs before the triggers, so an `INSTEAD OF` trigger finds the view
    /// it is attached to.
    ///
    /// @param stored - the catalog rows
    fn views_from(&mut self, stored: &[SchemaEntry]) {
        for entry in stored {
            if entry.kind != ObjectKind::View {
                continue;
            }
            self.keep(entry, 0);
        }
    }

    /// Loads every written trigger onto its table's declaration.
    ///
    /// **The triggers, then the keys, and in that order.** A written trigger is
    /// a catalog row like a table or an index and joins its table's
    /// declaration; a foreign key is a trigger the binder writes, and
    /// `plan_schema` can only write it once every table is in hand, because a
    /// key records only the child's side and the parent's has to be found by
    /// asking every table what it points at.
    ///
    /// Neither was done here until task-1932, which is the whole reason foreign
    /// keys were unenforced: the binder fills a statement's `triggers` from
    /// exactly these two places, and both were empty on this engine.
    ///
    /// @param stored - the catalog rows
    fn triggers_from(&mut self, stored: &[SchemaEntry]) {
        for entry in stored {
            if entry.kind != ObjectKind::Trigger {
                continue;
            }
            let folded = entry.table.to_ascii_lowercase();
            let held = inillucent_catalog::load::trigger_from_create_sql(&entry.sql).ok();
            match (self.infos.get_mut(&folded), held) {
                (None, _) => {
                    self.skip(entry);
                    continue;
                }
                // Newest first, which is SQLite's own order: it pushes each
                // trigger onto the front of the table's list as it reads the
                // schema, so the most recently created one fires first.
                (Some((_, info)), Some(trigger)) => info.triggers.insert(0, trigger),
                (Some(_), None) => self.skip(entry),
            }
            self.keep(entry, 0);
        }
    }

    /// Writes each table's foreign keys as the triggers that enforce them.
    ///
    /// @param name - the schema's name, which the foreign-key planner qualifies
    ///   with
    fn foreign_keys_from(&mut self, name: &[u8]) {
        let mut planned: Vec<TableInfo> =
            self.infos.values().map(|(_, info)| info.clone()).collect();
        inillucent_sql::foreign_key::plan_schema(&mut planned, name, &Limits::default());
        for info in planned {
            if let Some((_, held)) = self.infos.get_mut(&info.folded) {
                held.foreign_key_triggers = info.foreign_key_triggers.clone();
            }
        }
    }

    /// Adds the catalog's own layout, and hands back everything loaded.
    ///
    /// `sqlite_schema` is read over the catalog tree exactly as the import
    /// builds it: one root number no object can have, and the ordinary scan
    /// path.
    ///
    /// @param catalog_tree - the file's catalog tree
    /// @param catalog_handle - the handle it is read through
    fn layouts_from(
        mut self,
        catalog_tree: PagedTree,
        catalog_handle: u32,
    ) -> DbResult<LoadedSchema> {
        let schema_root = catalog_handle;
        let schema_info = table_from_create_sql(schema_create_sql(), self.index, schema_root)?;
        self.layouts.insert(
            schema_root,
            std::rc::Rc::new(SourceLayout {
                tree_key: schema_root,
                slots: (1..=5).map(Some).collect(),
                rowid: Some(0),
                identity: vec![0],
                types: vec![
                    StaticType::Int,
                    StaticType::Text,
                    StaticType::Text,
                    StaticType::Text,
                    StaticType::Int,
                    StaticType::Text,
                ],
                width: 6,
                key_columns: vec![0],
            }),
        );
        self.trees.insert(schema_root, catalog_tree);
        let trees = self.trees;
        for roots in self.covering.values_mut() {
            roots.sort_by_key(|root| {
                trees
                    .get(root)
                    .map(PagedTree::byte_size)
                    .unwrap_or(usize::MAX)
            });
        }
        let mut tables: Vec<TableInfo> = self.infos.into_values().map(|(_, info)| info).collect();
        tables.sort_by(|one, two| one.folded.cmp(&two.folded));
        analyze::attach_statistics(self.database.pool(), &trees, &mut tables);
        Ok(LoadedSchema {
            trees,
            layouts: self.layouts,
            covering: self.covering,
            entries: self
                .entries
                .into_iter()
                .zip(self.identifiers)
                .map(|((rowid, entry), root)| Recorded { rowid, root, entry })
                .collect(),
            tables,
            schema_info,
            handles: self.handles,
            skipped: self.skipped,
            highest_identifier: self.highest_identifier,
        })
    }
}

/// Reads one file's catalog tree and derives everything needed to plan on it.
///
/// **The five passes, in order (task-1962, A8).** It was 329 lines carrying
/// nine accumulators; each pass is a [`Loading`] method now. The order is not
/// arbitrary: an index's shape is derived against its table's declaration, a
/// trigger joins a table that has to already be there, a view has to be there
/// before an `INSTEAD OF` trigger looks for it, and a foreign key is a trigger
/// the binder can only write once every table is in hand.
///
/// @param database - the file
/// @param catalog_tree - its catalog tree, already attached from the meta page
/// @param index - which schema this is, as the binder numbers them
/// @param name - the schema's name, which the foreign-key planner qualifies with
/// @param catalog_handle - the handle this file's `sqlite_schema` is read through
/// @param allocate - hands out the connection's handle for a local tree identifier
pub(crate) fn load_schema(
    database: &Database,
    catalog_tree: PagedTree,
    index: usize,
    name: &[u8],
    catalog_handle: u32,
    allocate: &mut dyn FnMut(u32) -> u32,
) -> DbResult<LoadedSchema> {
    let stored_rows = inillucent_catalog::paged::read_catalog_rows(database.pool(), &catalog_tree)?;
    let stored: Vec<SchemaEntry> = stored_rows.iter().map(|(_, entry)| entry.clone()).collect();
    let mut loading = Loading::over(database, index, stored_rows);
    loading.tables_from(&stored, allocate)?;
    loading.indexes_from(&stored, allocate)?;
    loading.views_from(&stored);
    loading.triggers_from(&stored);
    loading.foreign_keys_from(name);
    loading.layouts_from(catalog_tree, catalog_handle)
}
