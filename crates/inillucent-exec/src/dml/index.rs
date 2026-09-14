//! What a write puts in an index, and which indexes it has to put it in.
//!
//! Invariant: **an index entry is the indexed keys followed by whatever
//! identifies the table row** - a rowid for an ordinary table, the primary
//! key's columns for a `WITHOUT ROWID` one - and every write that changes a row
//! changes every index that covers it. An index an insert maintains and a
//! delete forgets is a tree that disagrees with its own table and answers a
//! covering query wrongly while every other query is fine.
//!
//! ## Why this is its own module
//!
//! `dml.rs` was at its recorded ceiling and task-1913's generated-column work
//! added forty-two lines to it, so the ratchet in `policy.rs` asked for an
//! extraction rather than a raised number. These six items are one question -
//! what entry a row makes and which indexes take it - and the insert, update
//! and delete paths already reached for them together. Nothing moved changed
//! in the move.

use inillucent_base::DbResult;
use inillucent_sql::catalog_view::{IndexInfo, TableInfo};
use inillucent_tree::datum::{Datum, OwnedDatum};

use super::{IndexExprs, Row, SourceLayout, WriteTarget};

/// Returns the indexes of a table that the write path maintains, with their
/// positions.
///
/// A `WITHOUT ROWID` table's primary-key index *is* the table: one b-tree,
/// reported at the table's own root. Maintaining it separately would write
/// every row twice.
///
/// The position travels with the index because a partial index's predicate and
/// an expression key are compiled per index and found by it - see
/// [`IndexExprs`].
///
/// @param table - the table
pub(super) fn maintained(table: &TableInfo) -> impl Iterator<Item = (usize, &IndexInfo)> {
    table
        .indexes
        .iter()
        .enumerate()
        .filter(|(_, index)| index.root != 0 && index.root != table.root)
}

/// Adds or removes one entry in one index.
///
/// **This is the index maintenance the acceptance asks a differential test to
/// prove.** It is one function rather than one per caller, because an index an
/// insert maintains and a delete forgets is a tree that disagrees with its table
/// and answers a covering query wrongly while every other query is fine.
///
/// @param index - the index
/// @param target - the file and its trees
/// @param entry - the entry: the keys, then the rowid
/// @param adding - true to add it, false to remove it
pub(super) fn write_index_entry(
    index: &IndexInfo,
    target: &mut dyn WriteTarget,
    entry: &[OwnedDatum],
    adding: bool,
) -> DbResult<()> {
    let (database, trees, log) = target.parts_for(index.root)?;
    let Some(tree) = trees.get_mut(index.root) else {
        return Ok(());
    };
    let borrowed: Vec<Datum<'_>> = entry.iter().map(OwnedDatum::borrow).collect();
    if adding {
        tree.put(database, log, &borrowed)?;
    } else {
        tree.delete(database, log, &borrowed)?;
    }
    Ok(())
}

/// Builds one index entry from a table row.
///
/// An index entry is the indexed columns followed by whatever identifies the
/// table row - a rowid for an ordinary table, and the primary key's columns for
/// a `WITHOUT ROWID` one. That is what the import builds, what `index_shape`
/// describes, and what every read of an index assumes.
///
/// @param index - the index
/// @param layout - the table tree's layout
/// @param row - the table row, in tree-column order
pub(super) fn index_entry(
    position: usize,
    index: &IndexInfo,
    layout: &SourceLayout,
    row: &[OwnedDatum],
    indexes: IndexExprs<'_>,
) -> DbResult<Row> {
    let trailing = layout.identity.len().max(1);
    let mut entry = Vec::with_capacity(index.columns.len().saturating_add(trailing));
    for (key, column) in index.columns.iter().enumerate() {
        // A key the index computes is evaluated over the row; a key that is a
        // column is read out of it. `key` answers `None` for every index that
        // computes nothing, which is every index the gate measures.
        if let Some(computed) = indexes.key(position, key, row)? {
            entry.push(computed);
            continue;
        }
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
        return Ok(entry);
    }
    for slot in &layout.identity {
        entry.push(row.get(*slot).cloned().unwrap_or(OwnedDatum::Null));
    }
    Ok(entry)
}

/// Returns the unique indexes of a table that are trees of their own.
///
/// **Newest first, because that is the one SQLite names.** When a row collides
/// on two unique indexes at once only one of them can be reported, and SQLite
/// reports the last one declared: it links each new index onto the *head* of
/// the table's list and walks the list in order, so a schema read back off disk
/// is in reverse declaration order. `CREATE UNIQUE INDEX u1 ON t(a)` then
/// `u2 ON t(b)`, and a row taking both, answers `UNIQUE constraint failed: t.b`
/// - and `t.a` when the two are declared the other way round. Iterating
/// declaration-first named the wrong constraint on `INSERT` as well as
/// `UPDATE`, and the message is the part an application matches on.
///
/// The table's own key is not in here and does not need to be: both engines
/// check it before any index.
///
/// @param table - the table
pub(super) fn unique_indexes(table: &TableInfo) -> impl Iterator<Item = (usize, &IndexInfo)> {
    table
        .indexes
        .iter()
        // **`enumerate` before `rev`, and the order matters more than it
        // looks.** The reversal makes the constraint named on a
        // collision the last-declared index, which is what SQLite reports.
        // The enumeration was added later, once partial indexes existed, so
        // each index carries its position in `table.indexes` - the number the
        // binder used when it bound the
        // partial predicates, and the number `IndexExprs` looks them up by.
        // Reversing first would renumber them, and `holds` would then consult
        // **another index's** predicate: silently, and only on a table with two
        // or more unique indexes where one is partial.
        // `index.partial.unique.two.indexes` is that table.
        .enumerate()
        .rev()
        .filter(|(_, index)| index.unique && index.root != 0 && index.root != table.root)
}

/// Returns an index entry's key prefix, or `None` when a NULL makes it distinct.
///
/// @param index - the index
/// @param entry - the entry: the keys, then the rowid
pub(super) fn distinct_prefix(index: &IndexInfo, entry: &[OwnedDatum]) -> Option<Vec<OwnedDatum>> {
    let prefix: Vec<OwnedDatum> = entry.iter().take(index.columns.len()).cloned().collect();
    if prefix.is_empty() || prefix.contains(&OwnedDatum::Null) {
        return None;
    }
    Some(prefix)
}

/// Returns a row's key, in key-column order.
///
/// @param layout - the table tree's layout
/// @param row - the row, in tree-column order
pub(super) fn key_of(layout: &SourceLayout, row: &[OwnedDatum]) -> Vec<OwnedDatum> {
    layout
        .key_columns
        .iter()
        .filter_map(|column| row.get(*column).cloned())
        .collect()
}
