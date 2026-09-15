//! `CREATE INDEX`, and the entries a new index is built from.
//!
//! Invariant: **the entries are read once and sorted once.** An index built by
//! inserting one row at a time would pay a descent per row; this reads the
//! table, sorts, and bulk-builds, which is what `build_stage_nanos` reports the
//! stages of.

use std::collections::HashMap;

use inillucent_base::error::refusal;
use inillucent_base::DbResult;
use inillucent_catalog::ddl::canonical_sql;
use inillucent_catalog::load::index_from_create_sql;
use inillucent_catalog::paged::{ObjectKind, SchemaEntry};
use inillucent_sql::catalog_view::{IndexInfo, TableInfo};
use inillucent_tree::datum::Datum;
use inillucent_tree::leaf::MiniColumn;
use inillucent_tree::paged::KeyEncoding;
use inillucent_value::collation::Collation;

use super::*;
use crate::*;

impl crate::ImportedDatabase {
    /// Creates an index, fills it from the table, and records it.
    ///
    /// The fill is a bottom-up bulk build rather than a per-key insert: the
    /// entries are projected out of the table tree, sorted once, and packed left
    /// to right. That is the TDD's bulk builder and it is what the `schema`
    /// family's bar is a claim about.
    ///
    /// @param source - the statement text
    /// @param name_offset - where the index's name starts in it
    /// @param name - the index's name as written
    /// @param table - the table it indexes
    /// @param unique - whether `UNIQUE` was written
    /// @param exists - whether an index of that name is already there
    /// @param if_not_exists - whether the statement said so
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn create_index(
        &mut self,
        source: &[u8],
        name_offset: u32,
        name: &[u8],
        table: &[u8],
        unique: bool,
        exists: bool,
        if_not_exists: bool,
    ) -> DbResult<Outcome> {
        if exists {
            if if_not_exists {
                return Ok(Outcome::empty());
            }
            return Err(refusal(format!(
                "index {} already exists",
                String::from_utf8_lossy(name)
            )));
        }
        let keywords = if unique {
            "CREATE UNIQUE INDEX"
        } else {
            "CREATE INDEX"
        };
        let sql = canonical_sql(keywords, source, name_offset, source.len() as u32);
        let folded = table.to_ascii_lowercase();
        let position = self
            .schema
            .tables
            .iter()
            .position(|held| held.folded == folded)
            .ok_or_else(|| refusal(format!("no such table: {}", String::from_utf8_lossy(table))))?;
        let owner = self
            .schema
            .tables
            .get(position)
            .cloned()
            .ok_or_else(|| refusal("the table that was just found is gone"))?;
        let root = self.allocate_root()?;
        let index = index_from_create_sql(&sql, &owner, root)?;
        let (columns, layout) = index_shape(&owner, &index, root);
        let key_columns = columns.len();
        // The entries are scanned into an arena, sorted by a radix pass over
        // a fixed-width prefix of the tree's own key encoding, and packed
        // straight out of it. The encoding and the collations are the *tree's*,
        // so the order the sort produces is the order the tree will be searched
        // in - which is the invariant `in_key_order` exists to defend, taken
        // here rather than re-derived.
        let encoding = KeyEncoding::choose(&columns, key_columns);
        let collations: Vec<Collation> = columns
            .iter()
            .take(key_columns)
            .map(|spec| spec.collation)
            .collect();
        // And the directions, for the same reason: a `DESC` key column is
        // stored descending, so the sort that packs the tree has to produce
        // that order rather than the ascending one and let the reader cope.
        let directions: Vec<bool> = columns
            .iter()
            .take(key_columns)
            .map(|spec| spec.descending)
            .collect();
        let scanned = std::time::Instant::now();
        // **A partial index and an index on an expression are filled by a
        // query; everything else is filled by a scan.** The scan reads columns
        // out of the leaves and is what the `schema` family measures; it has no
        // way to evaluate `lower(a)` or `WHERE b > 5`, and teaching it to would
        // put an expression evaluator on the path of every ordinary
        // `CREATE INDEX`. The binder, planner and executor already evaluate
        // both, so the two forms that need them are built by asking them - the
        // same shape `create_vector_index` uses to backfill a store.
        let computed =
            index.partial_sql.is_some() || index.columns.iter().any(|key| key.expr_sql.is_some());
        let mut entries = if computed {
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
        let scan = scanned.elapsed().as_nanos();
        let sorted = std::time::Instant::now();
        let order = entries.order();
        let sort = sorted.elapsed().as_nanos();
        let checked = std::time::Instant::now();
        if unique {
            refuse_duplicates(&entries, &order, &owner, &index, key_columns)?;
        }
        let uniqueness = checked.elapsed().as_nanos();
        // The sort is finished and the uniqueness check with it, so everything
        // only they needed goes back before the pack - which is the half of the
        // statement the high-water mark is taken during.
        entries.release_sort_scratch();
        // **No flat run any more, and the stage that made one reads zero.**
        // The packer used to need a slice, so the arena was flattened into a
        // `Vec<Datum>` in key order and sliced into a `Vec<&[Datum]>` - 6.4 MiB
        // of copies at a hundred thousand rows. `EntrySet::in_order` is a view
        // over the arena and the order vector, and the packer indexes it. The
        // timer stays so that four runs of gate output either side of the change
        // are comparable line for line.
        let flattened = std::time::Instant::now();
        let source = entries.in_order(&order);
        let flatten = flattened.elapsed().as_nanos();
        let packed = std::time::Instant::now();
        // **The catalog row names the new tree before the tree is filled
        // (task-1932).** A recovery derives a tree's shape from the catalog
        // rows it has replayed, and skips a record naming a tree no row names -
        // which is right for a tree a rebuild has dropped and wrong for one
        // whose row has not gone past yet. Writing the row first is what keeps
        // those two apart: after this, every record describing a page of this
        // tree follows a row that names it. The row below is superseded by the
        // one at the end of this statement, in the same transaction and before
        // anything can read either, so the only thing it changes is the order
        // two records reach the log in. `rebuild_index` does the same, for the
        // crash `reindex_crash.rs` found.
        let rowid = self.next_catalog_rowid();
        self.record(
            root,
            SchemaEntry {
                kind: ObjectKind::Index,
                name: name.to_vec(),
                table: owner.name.clone(),
                root: inillucent_pool::PageId(0),
                sql: sql.clone(),
                stats: Default::default(),
                tree_id: 0,
            },
        )?;
        let page = self.build_tree_rows(root, columns, key_columns, layout, &source)?;
        let pack = packed.elapsed().as_nanos();
        // The tail is timed too, because it is not free and it is not the
        // build: recording the catalog row, re-deriving every table from the
        // catalog text, and refreshing the planner's view of it. `seal` is
        // timed after it and apart from it - see below.
        let tail = std::time::Instant::now();
        let at = self.schema.ddl_schema;
        self.rewrite(
            rowid,
            SchemaEntry {
                kind: ObjectKind::Index,
                name: name.to_vec(),
                table: owner.name.clone(),
                root: page,
                sql,
                stats: self.tree_stats(root),
                tree_id: self.local_of(at, root),
            },
        )?;
        // **A partial index is not a covering candidate.** The physical pass
        // stands the smallest covering tree in for a plain table scan, and its
        // test is whether the tree carries every *column* the query reads - it
        // has no way to notice that the tree holds fewer *rows* than the table.
        // Offering one here answered `SELECT rowid FROM t` with the rows inside
        // the predicate, silently, under a plan that said `SCAN t`.
        if crate::covers_every_row(&index) {
            self.schema
                .covering
                .entry(owner.root)
                .or_default()
                .push(root);
            self.sort_covering(owner.root);
        }
        let _ = position;
        let _ = index;
        self.rebuild_tables()?;
        self.refresh_catalog();
        let catalog = tail.elapsed().as_nanos();
        // **`seal` is timed apart from the catalog work, because they are
        // different claims.** `seal` is a log commit and a sync that SQLite
        // pays too under `synchronous = FULL`, so it is not a gap to close;
        // `record`, `rebuild_tables` and `refresh_catalog` are this engine's own
        // and are worth knowing the size of. Reported together they were one
        // number nobody could act on.
        let sealed = std::time::Instant::now();
        self.seal()?;
        self.compiled.index_stages.set(crate::StageTimings {
            scan,
            sort,
            unique: uniqueness,
            flatten,
            pack,
            catalog,
            seal: sealed.elapsed().as_nanos(),
        });
        Ok(Outcome::empty())
    }
    /// Returns the entries a new index holds, unsorted.
    ///
    /// One pass over the table tree, projecting the key columns and the rowid.
    /// The projection is a lookup in the table's own layout - `slots[declared]`
    /// is the tree column a declared column lives in - which is the same map the
    /// scan operators read, so an index built here indexes the column the
    /// planner thinks it does.
    ///
    /// The entries land in an [`EntrySet`] rather than in a `Vec` per row.
    /// Three hundred thousand heap allocations - a vector and a text copy per
    /// row, and then a borrowed vector per row for the packer - were most of
    /// what put the `schema` family under the floor. The arena copies each
    /// payload once, encodes each key once, and hands the packer slices.
    ///
    /// @param owner - the table being indexed
    /// @param index - the index's declaration
    /// @param width - how many columns an entry has
    /// @param encoding - the index tree's key encoding
    /// @param collations - the key columns' collations, in key order
    /// @param directions - the key columns' directions, in key order
    pub(crate) fn index_entries(
        &self,
        owner: &TableInfo,
        index: &IndexInfo,
        width: usize,
        encoding: KeyEncoding,
        collations: &[Collation],
        directions: &[bool],
    ) -> DbResult<EntrySet> {
        let layout = self
            .schema
            .layouts
            .get(&owner.root)
            .ok_or_else(|| refusal("no layout for the table being indexed"))?;
        let tree = self
            .schema
            .trees
            .get(&owner.root)
            .ok_or_else(|| refusal("no tree for the table being indexed"))?;
        let mut sources: Vec<usize> = Vec::with_capacity(index.columns.len());
        for key in &index.columns {
            let declared = key
                .column
                .map(usize::from)
                .ok_or_else(|| refusal("an index on an expression"))?;
            let slot = layout
                .slots
                .get(declared)
                .copied()
                .flatten()
                .ok_or_else(|| refusal("an index on a column the tree does not carry"))?;
            sources.push(slot);
        }
        // What identifies the table row: a rowid, or a `WITHOUT ROWID` table's
        // primary key. The layout is asked rather than the table, so the build
        // path and the read path cannot disagree about what an entry carries.
        let trailing: Vec<usize> = if layout.identity.is_empty() {
            vec![layout
                .rowid
                .ok_or_else(|| refusal("an index on a table that identifies no row"))?]
        } else {
            layout.identity.clone()
        };
        let mut entries = EntrySet::with_capacity(
            width,
            tree.row_count() as usize,
            encoding,
            collations,
            directions,
        );
        let pool = self.pool_of(owner.root)?;
        tree.visit_leaves(pool, &mut |leaf| {
            // One reusable buffer per *leaf*, not per row: the values borrow
            // from the leaf, so the buffer cannot outlive it - and a hundred
            // and sixty allocations for a hundred thousand rows is not a cost.
            let mut entry: Vec<Datum<'_>> = Vec::with_capacity(width);
            // **A clean leaf is read column by column, not row by row.** `live`
            // is what merges the delta area and skips the tombstones, and it
            // pays for that by building a `Vec` per row holding *every* column
            // - where an index reads two of them. On a hundred thousand rows
            // that was three allocations and a copy of every column per row, to
            // keep two values.
            //
            // A leaf that has not been written to has no delta area and no
            // tombstones, so there is nothing to merge and the values can be
            // read straight out of the mini-columns. A leaf that has been
            // written to still goes through `live`, because merging is exactly
            // what it is for.
            if leaf.has_writes() {
                // **A leaf that has been written to still goes through the
                // merge, but it no longer materialises a row per row.** `live`
                // hands back every column of every live row in a fresh `Vec`,
                // and an index reads two of them - which is 14.3 ms of the
                // gate's 38.9 ms `CREATE INDEX`, because the gate builds its
                // index after its write workloads and by then almost every leaf
                // has a delta entry. `visit_live` performs the same merge and
                // projects only what was asked for.
                let mut projected: Vec<usize> = Vec::with_capacity(width);
                projected.extend(sources.iter().copied());
                projected.extend(trailing.iter().copied());
                leaf.visit_live(&projected, &mut |values| {
                    entries.push(values);
                    Ok(())
                })?;
                return Ok(true);
            }
            // **The mini-columns are derived once per leaf, not once per
            // value.** `LeafRef::value` re-reads the directory entry and
            // re-derives the class array and slot bounds on every call, which
            // is the same waste `key_view` exists to remove inside a search -
            // and an index build calls it twice for every row in the table.
            // Seventy rows per leaf is seventy times the same answer.
            let mut columns: Vec<MiniColumn<'_>> = Vec::with_capacity(width);
            for slot in sources.iter().chain(trailing.iter()) {
                columns.push(leaf.column(*slot)?);
            }
            for row in 0..leaf.row_count() {
                entry.clear();
                for column in &columns {
                    entry.push(column.value(row)?);
                }
                entries.push(&entry);
            }
            Ok(true)
        })?;
        Ok(entries)
    }
    /// Returns the entries a new index holds, by asking the query engine.
    ///
    /// **For the two forms whose entries are not columns of the row**: a
    /// partial index, whose predicate decides which rows have an entry at all,
    /// and an index on an expression, whose key no column carries. Both are
    /// ordinary SQL, so this writes the SQL and runs it rather than growing a
    /// second evaluator inside the DDL path.
    ///
    /// The projection is the key columns - each either an expression as it was
    /// written or a quoted column name - followed by whatever identifies the
    /// row, which is `rowid` for an ordinary table and the primary key's
    /// columns for a `WITHOUT ROWID` one. That is exactly the entry shape
    /// `index_shape` describes.
    ///
    /// A predicate or an expression naming a column the table has not got is
    /// refused here, by the binder, which is what makes
    /// `CREATE INDEX ix ON t(a) WHERE nosuchcolumn > 5` an error rather than an
    /// index nothing can maintain.
    ///
    /// @param owner - the table being indexed
    /// @param index - the index's declaration
    /// @param width - how many columns an entry has
    /// @param encoding - the index tree's key encoding
    /// @param collations - the key columns' collations, in key order
    /// @param directions - the key columns' directions, in key order
    pub(crate) fn index_entries_by_query(
        &mut self,
        owner: &TableInfo,
        index: &IndexInfo,
        width: usize,
        encoding: KeyEncoding,
        collations: &[Collation],
        directions: &[bool],
    ) -> DbResult<EntrySet> {
        let mut projected: Vec<String> = Vec::with_capacity(width);
        for key in &index.columns {
            match (&key.expr_sql, key.column) {
                (Some(sql), _) => projected.push(String::from_utf8_lossy(sql).into_owned()),
                (None, Some(declared)) => {
                    let column = owner
                        .column(declared)
                        .ok_or_else(|| refusal("an index on a column the table has not got"))?;
                    projected.push(quoted(&column.name));
                }
                (None, None) => return Err(refusal("an index key that is neither")),
            }
        }
        let identity = crate::identity_columns(owner);
        if identity.is_empty() {
            projected.push("rowid".to_string());
        } else {
            for declared in &identity {
                let column = owner
                    .columns
                    .get(*declared)
                    .ok_or_else(|| refusal("a primary key column the table has not got"))?;
                projected.push(quoted(&column.name));
            }
        }
        let query = match index.partial_sql.as_ref() {
            Some(predicate) => format!(
                "SELECT {} FROM {} WHERE ({})",
                projected.join(", "),
                quoted(&owner.name),
                String::from_utf8_lossy(predicate)
            ),
            None => format!(
                "SELECT {} FROM {}",
                projected.join(", "),
                quoted(&owner.name)
            ),
        };
        let rows = self
            .execute_any(&query, &inillucent_exec::physical::Params::new())?
            .rows;
        let mut entries =
            EntrySet::with_capacity(width, rows.len(), encoding, collations, directions);
        let mut entry: Vec<Datum<'_>> = Vec::with_capacity(width);
        for row in &rows {
            entry.clear();
            for value in row.iter().take(width) {
                entry.push(value.borrow());
            }
            entries.push(&entry);
        }
        Ok(entries)
    }
    /// Puts a table's covering indexes back in smallest-tree-first order.
    ///
    /// @param table_root - the table whose list changed
    pub(crate) fn sort_covering(&mut self, table_root: u32) {
        let sizes: HashMap<u32, usize> = self
            .schema
            .trees
            .iter()
            .map(|(root, tree)| (*root, tree.byte_size()))
            .collect();
        if let Some(roots) = self.schema.covering.get_mut(&table_root) {
            roots.sort_by_key(|root| sizes.get(root).copied().unwrap_or(usize::MAX));
        }
    }
}
