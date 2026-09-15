//! `REINDEX`: rebuilding an index's tree from the table it indexes.
//!
//! Invariant: **the catalog row naming the new tree is written before the tree
//! is filled.** A rebuild allocates a fresh tree and moves the index's catalog
//! row onto it, and a recovery derives a tree's shape from the rows it has
//! replayed - so a log that described the new tree's pages before any row named
//! it left a database that would not open at all after a crash. See
//! `rebuild_index` for the failure and `recovery.rs` for the other half of it.
//!
//! Here rather than in [`super`] because `ddl.rs` is three thousand lines and
//! `REINDEX` is one statement with one helper, reached from one arm of the
//! directive match and from nowhere else.

use super::*;

impl ImportedDatabase {
    /// Rebuilds indexes, which for this engine is a no-op with a check.
    ///
    /// A `REINDEX` exists to repair an index whose collation sequence changed
    /// under it. Every collation this engine orders a tree by is built in and
    /// cannot change, so there is nothing to repair - but the trees are checked
    /// rather than the statement being ignored, so `REINDEX` still answers the
    /// question a person runs it to ask.
    ///
    /// @param indexes - the indexes named, empty for all of them
    pub(super) fn reindex(&mut self, indexes: &[Vec<u8>]) -> DbResult<Outcome> {
        let wanted: Vec<Vec<u8>> = indexes
            .iter()
            .map(|name| name.to_ascii_lowercase())
            .collect();
        // **Rebuilt, not inspected.** This used to run the tree's integrity
        // check and call that a `REINDEX`, which is the one thing a `REINDEX`
        // is not: the statement exists so a person whose collation has changed
        // under an index can put the entries back in the order the engine now
        // compares them in, and a check cannot move an entry. It also meant a
        // `REINDEX` over a `NOCASE` index *failed* - the check compared with
        // `BINARY` while the tree was ordered by `NOCASE` - so the one
        // statement that could have repaired such an index reported it as
        // corrupt instead.
        let targets: Vec<(Vec<u8>, Vec<u8>)> = self
            .schema
            .tables
            .iter()
            .flat_map(|table| {
                table
                    .indexes
                    .iter()
                    .map(move |index| (table.name.clone(), index.clone()))
            })
            // A module owns its own index and rebuilds it its own way; a b-tree
            // rebuild has nothing to put in it.
            .filter(|(_, index)| index.origin != inillucent_sql::catalog_view::IndexOrigin::Module)
            .filter(|(_, index)| {
                wanted.is_empty()
                    || wanted.contains(&index.folded)
                    // `REINDEX t` names a table and means every index on it;
                    // `REINDEX NOCASE` names a collation and means every index
                    // that uses it.
                    || wanted.iter().any(|name| {
                        index
                            .columns
                            .iter()
                            .any(|key| key.collation.to_ascii_lowercase() == *name)
                    })
            })
            .map(|(table, index)| (table, index.name.clone()))
            .collect();
        let named_table = self.schema.tables.iter().any(|table| {
            wanted
                .iter()
                .any(|name| table.folded == *name && !table.indexes.is_empty())
        });
        let targets: Vec<(Vec<u8>, Vec<u8>)> = if named_table {
            self.schema
                .tables
                .iter()
                .filter(|table| wanted.contains(&table.folded))
                .flat_map(|table| {
                    table
                        .indexes
                        .iter()
                        .filter(|index| {
                            index.origin != inillucent_sql::catalog_view::IndexOrigin::Module
                        })
                        .map(move |index| (table.name.clone(), index.name.clone()))
                })
                .chain(targets)
                .collect()
        } else {
            targets
        };
        let mut done: Vec<Vec<u8>> = Vec::new();
        for (table, index) in targets {
            if done.contains(&index) {
                continue;
            }
            done.push(index.clone());
            self.rebuild_index(&table, &index)?;
        }
        if !done.is_empty() {
            self.rebuild_tables()?;
            self.refresh_catalog();
            self.seal()?;
        }
        Ok(Outcome::empty())
    }

    /// Rebuilds one index's tree from the table it indexes.
    ///
    /// The entries are re-derived and repacked exactly the way
    /// `create_index` derives them, so a rebuilt tree is byte-for-byte the tree
    /// a `CREATE INDEX` would have produced now - which is the whole promise of
    /// the statement. The catalog row keeps its name and its text and takes the
    /// new root.
    ///
    /// @param table - the indexed table's name
    /// @param name - the index's name
    fn rebuild_index(&mut self, table: &[u8], name: &[u8]) -> DbResult<()> {
        let folded = table.to_ascii_lowercase();
        let owner = self
            .schema
            .tables
            .iter()
            .find(|held| held.folded == folded)
            .cloned()
            .ok_or_else(|| refusal(format!("no such table: {}", String::from_utf8_lossy(table))))?;
        let index_folded = name.to_ascii_lowercase();
        let declared = owner
            .indexes
            .iter()
            .find(|held| held.folded == index_folded)
            .cloned()
            .ok_or_else(|| refusal(format!("no such index: {}", String::from_utf8_lossy(name))))?;
        let rowid = self
            .schema
            .entries
            .iter()
            .find(|held| {
                held.entry.kind == ObjectKind::Index
                    && held.entry.name.to_ascii_lowercase() == index_folded
            })
            .map(|held| held.rowid)
            .ok_or_else(|| refusal("the index has no catalog row"))?;
        let sql = self
            .schema
            .entries
            .iter()
            .find(|held| held.rowid == rowid)
            .map(|held| held.entry.sql.clone())
            .unwrap_or_default();
        let root = self.allocate_root()?;
        let index = inillucent_sql::catalog_view::IndexInfo { root, ..declared };
        let (columns, layout) = index_shape(&owner, &index, root);
        let key_columns = columns.len();
        let encoding = KeyEncoding::choose(&columns, key_columns);
        let collations: Vec<Collation> = columns
            .iter()
            .take(key_columns)
            .map(|spec| spec.collation)
            .collect();
        let directions: Vec<bool> = columns
            .iter()
            .take(key_columns)
            .map(|spec| spec.descending)
            .collect();
        let computed =
            index.partial_sql.is_some() || index.columns.iter().any(|key| key.expr_sql.is_some());
        let entries = if computed {
            self.index_entries_by_query(
                &owner,
                &index,
                key_columns,
                encoding,
                &collations,
                &directions,
            )?
        } else {
            self.index_entries(
                &owner,
                &index,
                key_columns,
                encoding,
                &collations,
                &directions,
            )?
        };
        let order = entries.order();
        if index.unique {
            refuse_duplicates(&entries, &order, &owner, &index, key_columns)?;
        }
        let flat = entries.to_datums(&order);
        let rows: Vec<&[Datum<'_>]> = if key_columns == 0 {
            Vec::new()
        } else {
            flat.chunks_exact(key_columns).collect()
        };
        let at = self.schema.ddl_schema;
        // **The catalog row names the new tree before the tree is filled
        // (task-1932, found by `reindex_crash.rs`).** A recovery derives every
        // tree's shape from the catalog rows it has replayed so far, and
        // refuses a record naming a tree it has no shape for - which is the
        // right refusal, because replaying into a guessed shape is how a file
        // is corrupted quietly. `REINDEX` rebuilds an index into a *freshly
        // allocated* tree, so until this row went past there was no row
        // anywhere naming it: the checkpointed catalog still named the old
        // root, and the row carrying the new one was written after every page
        // of the new tree. A crash after the rebuild committed therefore left a
        // database that **would not open at all**:
        //
        // ```text
        // bad parameter or other API misuse: the log names tree 2147483649,
        // which this recovery was not told the shape of
        // ```
        //
        // The row below is superseded by the one at the end of this function,
        // in the same transaction and before anything can read either, so the
        // only thing it changes is that the log names the tree before it
        // describes one of its pages. `root` is zero because the tree has no
        // root page yet and the shape derivation reads the identifier rather
        // than the page.
        self.rewrite(
            rowid,
            SchemaEntry {
                kind: ObjectKind::Index,
                name: index.name.clone(),
                table: owner.name.clone(),
                root: inillucent_pool::PageId(0),
                sql: sql.clone(),
                stats: inillucent_catalog::paged::TreeStats::default(),
                tree_id: self.local_of(at, root),
            },
        )?;
        let page = self.build_tree_from(root, columns, key_columns, layout, &rows)?;
        // The statistics and the identifier come off the tree that was just
        // built, exactly as `record` takes them, so the row cannot describe a
        // different tree from the one it names.
        let entry = SchemaEntry {
            kind: ObjectKind::Index,
            name: index.name.clone(),
            table: owner.name.clone(),
            root: page,
            sql,
            stats: self.tree_stats(root),
            tree_id: self.local_of(at, root),
        };
        self.rewrite(rowid, entry)?;
        Ok(())
    }
}
