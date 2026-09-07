//! DDL on the catalog tree: creating, dropping and altering the schema.
//!
//! Invariant: **a schema change is one transaction's worth of ordinary writes.**
//! The catalog row goes into the catalog tree through `PagedTree::insert`, the
//! pages a new tree needs come off the same free map a split allocates from, and
//! every one of them is described in the log before it happens. So a
//! `CREATE INDEX` that crashes half-way is undone by the same recovery that
//! undoes an `INSERT`, and there is no second write path to keep in step.
//!
//! ## Why the stored text is sliced rather than printed
//!
//! `sqlite_schema.sql` holds the statement from its object name onward with the
//! keywords prefixed - so `create table IF NOT EXISTS "T" ( a )` is stored as
//! `CREATE TABLE "T" ( a )`, keeping the author's spacing, case and quoting and
//! dropping the `IF NOT EXISTS`. That is what SQLite stores, byte for byte, and
//! `inillucent_catalog::ddl::canonical_sql` is the function that already did it
//! for the old engine. Printing the parse back would produce text that
//! round-trips today and stops round-tripping at the first syntax the renderer
//! forgets - and the acceptance for this phase is *digest-equal to SQLite in its
//! effect on `sqlite_schema`*, which is a byte comparison.
//!
//! ## Two identifiers, and why they are not the same number
//!
//! A catalog row's `rootpage` is the page the tree is rooted at **in this
//! file**. The `trees` and `layouts` maps are keyed by something else: an
//! identifier, which for an imported table is the fixture's SQLite root page and
//! for a created one is a number counted up from [`super::FIRST_CREATED_ROOT`].
//! They are different because the physical root is not known until the tree has
//! been built, and the identifier has to be chosen before it - a tree is stamped
//! with its identifier on every page it packs.
//!
//! ## What invalidates a plan
//!
//! Every statement that changes the catalog ends in [`ImportedDatabase::
//! refresh_catalog`], which rebuilds the binder's view, empties the statement
//! cache and bumps the generation. Emptying the cache is the invalidation; the
//! generation is what a test can read to prove it happened.

use std::collections::HashMap;

use inillucent_base::error::misuse;
use inillucent_base::DbResult;
use inillucent_catalog::ddl::canonical_sql;
use inillucent_catalog::load::{index_from_create_sql, table_from_create_sql};
use inillucent_catalog::paged::{
    delete_entry, insert_entry, tables_from_entries, ObjectKind, SchemaEntry,
};
use inillucent_catalog::rename;
use inillucent_exec::dml::Changes;
use inillucent_exec::physical::SourceLayout;
use inillucent_pool::PageId;
use inillucent_sql::bind::BoundStatement;
use inillucent_sql::catalog_view::{IndexInfo, StaticCatalog, TableInfo};
use inillucent_sql::directive::{AlterKind, Directive};
use inillucent_tree::datum::{Datum, OwnedDatum};
use inillucent_tree::types::ColumnSpec;
use inillucent_tree::PagedTree;

use super::{
    in_key_order, index_shape, keyed_table_shape, table_shape, ImportedDatabase, Outcome, Recorded,
    WalLog, SCHEMA_VIEW_ROOT,
};

impl ImportedDatabase {
    /// Returns how many times the catalog has changed.
    ///
    /// A plan compiled at one generation is never run at another, because the
    /// statement cache is emptied in the same breath the generation is bumped.
    /// This is here so a test can say so rather than infer it.
    pub fn catalog_generation(&self) -> u64 {
        self.catalog_generation
    }

    /// Returns the catalog's rows, in the order the tree holds them.
    ///
    /// For the acceptance tests, which compare them against SQLite's
    /// `sqlite_schema`.
    pub fn schema_entries(&self) -> Vec<(i64, SchemaEntry)> {
        self.entries
            .iter()
            .map(|held| (held.rowid, held.entry.clone()))
            .collect()
    }

    /// Runs one statement the session carries out itself.
    ///
    /// @param sql - the statement text, which is also the source the stored
    ///   `CREATE` text is sliced out of
    pub fn execute_ddl(&mut self, sql: &str) -> DbResult<Outcome> {
        let BoundStatement::Directive(directive) = self.bind(sql)? else {
            return Err(misuse(format!("{sql} is not a directive")));
        };
        let source = sql.as_bytes();
        match *directive {
            Directive::CreateTable {
                if_not_exists,
                name,
                name_offset,
                exists,
                ..
            } => self.create_table(source, name_offset, &name, exists, if_not_exists),
            Directive::CreateIndex {
                unique,
                if_not_exists,
                name,
                name_offset,
                table,
                exists,
                ..
            } => self.create_index(
                source,
                name_offset,
                &name,
                &table,
                unique,
                exists,
                if_not_exists,
            ),
            Directive::CreateView {
                if_not_exists,
                name,
                name_offset,
                exists,
                ..
            } => self.create_bodiless(
                "CREATE VIEW",
                ObjectKind::View,
                source,
                name_offset,
                &name,
                &name,
                exists,
                if_not_exists,
            ),
            // **Stored and fired, since task-1838.** It used to be refused,
            // and the refusal was right at the time: this engine could store a
            // trigger and list it in `sqlite_schema` but could not run one, and
            // a database whose triggers never fire is one whose invariants are
            // not being maintained by anything - which the application finds
            // out from its data rather than from an error.
            //
            // `inillucent-exec`'s firing point is what makes it honest, and it
            // is the same mechanism foreign keys are enforced by: the binder
            // turns a `REFERENCES` clause into `CREATE TRIGGER` text, so a
            // written trigger and a key take exactly one path.
            Directive::CreateTrigger {
                name,
                name_offset,
                table,
                exists,
                ..
            } => self.create_bodiless(
                "CREATE TRIGGER",
                ObjectKind::Trigger,
                source,
                name_offset,
                &name,
                &table,
                exists,
                // The directive carries no `if_not_exists` because it does not
                // need one: `bind_create_trigger` has already refused a
                // duplicate that did not say so, and `exists` reaching here at
                // all therefore means the statement did.
                true,
            ),
            Directive::CreateVirtualTable {
                if_not_exists,
                name,
                name_offset,
                module,
                arguments,
                exists,
                ..
            } => self.create_virtual_table(
                source,
                name_offset,
                &name,
                &module,
                &arguments,
                exists,
                if_not_exists,
            ),
            Directive::Drop {
                kind,
                if_exists,
                name,
                exists,
                ..
            } => self.drop_object(kind, &name, exists, if_exists),
            Directive::Alter { table, action, .. } => self.alter_table(source, &table, &action),
            Directive::Analyze { table, .. } => self.analyze(table.as_deref()),
            Directive::Reindex { indexes, .. } => self.reindex(&indexes),
            Directive::Begin(_) => {
                self.begin_batch();
                Ok(Outcome::empty())
            }
            Directive::Commit => {
                self.commit_batch()?;
                Ok(Outcome::empty())
            }
            Directive::Pragma {
                ref name,
                ref argument,
                ..
            } => self.pragma(name, argument.as_ref()),
            Directive::Rollback { savepoint } => match savepoint {
                Some(name) => {
                    self.rollback_to(&name)?;
                    Ok(Outcome::empty())
                }
                None => {
                    self.rollback()?;
                    Ok(Outcome::empty())
                }
            },
            // A `SAVEPOINT` outside a transaction opens one, which is what
            // SQLite does: it is the only way to name a point inside a
            // statement that would otherwise be its own transaction.
            Directive::Savepoint(name) => {
                if self.batch.get().is_none() {
                    self.begin_batch();
                }
                self.savepoint(&name);
                Ok(Outcome::empty())
            }
            Directive::Release(name) => {
                self.release(&name)?;
                Ok(Outcome::empty())
            }
            // **Marked as a capability gap, not as misuse.** A directive this
            // engine has not implemented - `ATTACH`, `DETACH`, `VACUUM` - is a
            // construct it does not do yet, which is a different thing from a
            // statement the caller got wrong, and a caller in front of it has to
            // be able to tell them apart without matching on prose.
            other => {
                let what = super::describe_directive(&other);
                Err(misuse(format!(
                    "{sql} is {what}, which the new engine does not run yet"
                ))
                .with_unsupported(what))
            }
        }
    }

    /// Returns the identifier the next created tree is registered under.
    fn allocate_root(&mut self) -> u32 {
        let root = self.next_root;
        self.next_root = self.next_root.saturating_add(1);
        root
    }

    /// Returns the rowid the next catalog row takes.
    ///
    /// One past the largest in use, which is what an `INSERT` into a rowid table
    /// with no explicit key does - and SQLite writes its own `sqlite_schema`
    /// rows with exactly that statement.
    fn next_catalog_rowid(&self) -> i64 {
        self.entries
            .iter()
            .map(|held| held.rowid)
            .max()
            .unwrap_or(0)
            .saturating_add(1)
    }

    /// Rebuilds the binder's view of the schema and invalidates every plan.
    ///
    /// **The two happen together, always.** A rebuilt catalog with a live plan
    /// cache is the bug this function exists to make unwriteable: the next
    /// execution would take a plan built against the tree that used to be there.
    pub(super) fn refresh_catalog(&mut self) {
        // **The keys are re-planned every time the schema changes**, and this
        // is the only place that can do it: a foreign key records the child's
        // side alone, so the parent's trigger is found by asking every table
        // what it points at - which cannot be answered one `CREATE TABLE` at a
        // time. Doing it here rather than in `create_table` is also what makes
        // `CREATE TABLE child(... REFERENCES parent)` written *before* the
        // parent exists start being enforced when the parent arrives.
        inillucent_sql::foreign_key::plan_schema(
            &mut self.tables,
            b"main",
            &inillucent_base::limits::Limits::default(),
        );
        let mut catalog = StaticCatalog::empty();
        for table in &self.tables {
            catalog = catalog.with_table(table.clone());
        }
        catalog = catalog.with_table(self.schema_info.clone());
        catalog = catalog.with_table(super::schema_alias_of(&self.schema_info));
        self.catalog = catalog;
        self.forget_compiled_statements();
        self.catalog_generation = self.catalog_generation.saturating_add(1);
    }

    /// Throws away every statement compiled against the catalog as it was.
    ///
    /// A compiled statement carries decisions the catalog and the connection's
    /// settings made when it was compiled - which tree it reads, which index it
    /// probes, and whether its foreign keys are checked. Anything that changes
    /// one of those has to come through here, or the next execution answers
    /// with the old decision.
    pub(super) fn forget_compiled_statements(&self) {
        self.statements.borrow_mut().clear();
    }

    /// Writes one row into the catalog tree and records it.
    ///
    /// @param entry - the object to record
    pub(super) fn record(&mut self, root: u32, mut entry: SchemaEntry) -> DbResult<()> {
        let rowid = self.next_catalog_rowid();
        // The statistics come off the tree that was just built rather than from
        // the caller, so there is one place they can be wrong instead of four.
        entry.stats = self.tree_stats(root);
        // And the identifier, for the same reason and from the same argument.
        // The log refers to a tree by this number, so a caller that filled it in
        // itself would be a fifth place it could disagree with the tree it
        // describes - and a catalog naming the wrong tree would send recovery's
        // row records somewhere else.
        entry.tree_id = u64::from(root);
        let txn = self.current_txn();
        let open = self.batch.get().is_some();
        {
            let mut log = WalLog {
                wal: &self.wal,
                txn,
                // **A catalog row is a row.** A `CREATE TABLE` inside a
                // transaction has to come back out when the transaction is
                // abandoned, and the way it comes back out is the same way a
                // deleted row does: the catalog tree's before-image, restored.
                undo: open.then_some(&mut self.undo),
            };
            let tree = self
                .trees
                .get_mut(&SCHEMA_VIEW_ROOT)
                .ok_or_else(|| misuse("the catalog tree is not attached"))?;
            insert_entry(&mut self.database, tree, &mut log, rowid, &entry)?;
        }
        self.entries.push(Recorded { rowid, root, entry });
        Ok(())
    }

    /// Returns the shape of a tree, for its catalog row.
    ///
    /// A tree the catalog names but the maps do not hold - a view, a trigger -
    /// has no shape, and zero is what "unknown" reads as.
    ///
    /// @param root - the tree's identifier
    pub(super) fn tree_stats(&self, root: u32) -> inillucent_catalog::paged::TreeStats {
        match self.trees.get(&root) {
            Some(tree) => inillucent_catalog::paged::TreeStats {
                first_leaf: tree.first_leaf(),
                leaf_count: tree.leaf_count(),
                row_count: tree.row_count(),
            },
            None => inillucent_catalog::paged::TreeStats::default(),
        }
    }

    /// Rewrites every catalog row whose tree has changed shape.
    ///
    /// Called at a checkpoint, which is the moment the statistics can be made
    /// honest cheaply: the file is being made durable anyway, and a row rewrite
    /// per *changed* tree is a handful of small writes rather than one per
    /// insert. Between checkpoints the persisted numbers are the shape as of the
    /// last one, which is exactly what a reader that has just opened the file
    /// gets - and after recovery replays past that point, the checker is what
    /// notices if they no longer describe the tree.
    pub(super) fn refresh_statistics(&mut self) -> DbResult<()> {
        let stale: Vec<(i64, SchemaEntry)> = self
            .entries
            .iter()
            .filter_map(|held| {
                let stats = self.tree_stats(held.root);
                if stats == held.entry.stats {
                    return None;
                }
                let mut moved = held.entry.clone();
                moved.stats = stats;
                Some((held.rowid, moved))
            })
            .collect();
        for (rowid, entry) in stale {
            self.rewrite(rowid, entry)?;
        }
        Ok(())
    }

    /// Replaces one catalog row in place, by rowid.
    ///
    /// A delete and an insert rather than an update: four of the five columns
    /// are variable-length text, and the in-place path is for fixed-width slots.
    ///
    /// @param rowid - the row's key
    /// @param entry - what it should now say
    fn rewrite(&mut self, rowid: i64, entry: SchemaEntry) -> DbResult<()> {
        {
            let txn = self.current_txn();
            let open = self.batch.get().is_some();
            let mut log = WalLog {
                wal: &self.wal,
                txn,
                // **A catalog row is a row.** A `CREATE TABLE` inside a
                // transaction has to come back out when the transaction is
                // abandoned, and the way it comes back out is the same way a
                // deleted row does: the catalog tree's before-image, restored.
                undo: open.then_some(&mut self.undo),
            };
            let tree = self
                .trees
                .get_mut(&SCHEMA_VIEW_ROOT)
                .ok_or_else(|| misuse("the catalog tree is not attached"))?;
            delete_entry(&mut self.database, tree, &mut log, rowid)?;
            insert_entry(&mut self.database, tree, &mut log, rowid, &entry)?;
        }
        for held in &mut self.entries {
            if held.rowid == rowid {
                held.entry = entry.clone();
            }
        }
        Ok(())
    }

    /// Removes one catalog row, by rowid.
    ///
    /// @param rowid - the row's key
    fn forget(&mut self, rowid: i64) -> DbResult<()> {
        {
            let txn = self.current_txn();
            let open = self.batch.get().is_some();
            let mut log = WalLog {
                wal: &self.wal,
                txn,
                // **A catalog row is a row.** A `CREATE TABLE` inside a
                // transaction has to come back out when the transaction is
                // abandoned, and the way it comes back out is the same way a
                // deleted row does: the catalog tree's before-image, restored.
                undo: open.then_some(&mut self.undo),
            };
            let tree = self
                .trees
                .get_mut(&SCHEMA_VIEW_ROOT)
                .ok_or_else(|| misuse("the catalog tree is not attached"))?;
            delete_entry(&mut self.database, tree, &mut log, rowid)?;
        }
        self.entries.retain(|held| held.rowid != rowid);
        Ok(())
    }

    /// Builds one empty tree and registers it.
    ///
    /// @param root - the identifier to register it under
    /// @param columns - the column directory
    /// @param key_columns - how many leading columns form the key
    /// @param layout - how a bound expression finds its vector
    fn build_tree(
        &mut self,
        root: u32,
        columns: Vec<ColumnSpec>,
        key_columns: usize,
        layout: SourceLayout,
    ) -> DbResult<PageId> {
        self.build_tree_from(root, columns, key_columns, layout, &[])
    }

    /// Builds one tree from rows already in key order, and registers it.
    ///
    /// @param root - the identifier to register it under
    /// @param columns - the column directory
    /// @param key_columns - how many leading columns form the key
    /// @param layout - how a bound expression finds its vector
    /// @param rows - the rows, sorted by the key columns
    fn build_tree_from(
        &mut self,
        root: u32,
        columns: Vec<ColumnSpec>,
        key_columns: usize,
        layout: SourceLayout,
        rows: &[Vec<OwnedDatum>],
    ) -> DbResult<PageId> {
        let copied = std::time::Instant::now();
        let borrowed: Vec<Vec<Datum<'_>>> = rows
            .iter()
            .map(|row| row.iter().map(OwnedDatum::borrow).collect())
            .collect();
        self.borrow_nanos.set(copied.elapsed().as_nanos());
        let txn = self.current_txn();
        let tree = {
            let open = self.batch.get().is_some();
            let mut log = WalLog {
                wal: &self.wal,
                txn,
                // **A catalog row is a row.** A `CREATE TABLE` inside a
                // transaction has to come back out when the transaction is
                // abandoned, and the way it comes back out is the same way a
                // deleted row does: the catalog tree's before-image, restored.
                undo: open.then_some(&mut self.undo),
            };
            PagedTree::bulk_build_logged(
                &mut self.database,
                Some(&mut log),
                u64::from(root),
                columns,
                key_columns,
                &borrowed,
            )?
        };
        let page = tree.root();
        self.trees.insert(root, tree);
        self.layouts.insert(root, layout);
        Ok(page)
    }

    /// Gives one tree's pages back to the free map and forgets it.
    ///
    /// @param root - the identifier it is registered under
    fn release_tree(&mut self, root: u32) -> DbResult<()> {
        let pages = match self.trees.get(&root) {
            Some(tree) => tree.pages(self.database.pool())?,
            None => Vec::new(),
        };
        let txn = self.current_txn();
        for page in &pages {
            self.wal
                .append(txn, inillucent_wal::record::Body::FreePage { page: page.0 })?;
        }
        for page in pages {
            self.database.release(page, 1)?;
        }
        self.trees.remove(&root);
        self.layouts.remove(&root);
        self.covering.remove(&root);
        for roots in self.covering.values_mut() {
            roots.retain(|held| *held != root);
        }
        Ok(())
    }

    /// Returns the transaction a schema change joins.
    ///
    /// Inside a batch it is the batch's; outside one it is a fresh number that
    /// is committed by [`ImportedDatabase::seal`] at the end of the statement.
    pub(super) fn current_txn(&self) -> u64 {
        match self.batch.get() {
            Some(held) => held,
            None => self.next_txn.get(),
        }
    }

    /// Commits a schema change that was its own transaction.
    ///
    /// Inside a batch this does nothing: the batch's `COMMIT` is what makes the
    /// change durable, which is the whole difference between the two groupings.
    pub(super) fn seal(&mut self) -> DbResult<()> {
        if self.batch.get().is_some() {
            return Ok(());
        }
        let txn = self.next_txn.get();
        self.next_txn.set(txn.saturating_add(1));
        self.wal.append(
            txn,
            inillucent_wal::record::Body::CatalogChange { delta: &[] },
        )?;
        self.wal.commit(txn, txn)?;
        self.database
            .pool()
            .set_durable_lsn(self.wal.write_ahead_point());
        Ok(())
    }

    /// Creates a table, its tree, and the trees its constraints imply.
    ///
    /// @param source - the statement text
    /// @param name_offset - where the table's name starts in it
    /// @param name - the table's name as written
    /// @param exists - whether a table of that name is already there
    /// @param if_not_exists - whether the statement said so
    fn create_table(
        &mut self,
        source: &[u8],
        name_offset: u32,
        name: &[u8],
        exists: bool,
        if_not_exists: bool,
    ) -> DbResult<Outcome> {
        if exists {
            if if_not_exists {
                return Ok(Outcome::empty());
            }
            return Err(misuse(format!(
                "table {} already exists",
                String::from_utf8_lossy(name)
            )));
        }
        let sql = canonical_sql("CREATE TABLE", source, name_offset, source.len() as u32);
        self.define_table(name, sql)?;
        self.refresh_catalog();
        self.seal()?;
        Ok(Outcome::empty())
    }

    /// Builds a table's trees and records it, given its stored text.
    ///
    /// Shared by `CREATE TABLE` and by the `sqlite_stat1` that the first
    /// `ANALYZE` brings into being. `ANALYZE` cannot go through the statement
    /// path for it: `sqlite_` is a reserved prefix, and a schema-writing
    /// statement that has to defeat the binder's own rule to run is a rule with
    /// a hole in it.
    ///
    /// @param name - the table's name as it will be stored
    /// @param sql - the `CREATE` text to store and to derive the shape from
    pub(super) fn define_table(&mut self, name: &[u8], sql: Vec<u8>) -> DbResult<u32> {
        let root = self.allocate_root();
        let mut info = table_from_create_sql(&sql, 0, root)?;
        info.name = name.to_vec();
        info.folded = name.to_ascii_lowercase();
        let (columns, key_columns, layout) = if info.without_rowid {
            keyed_table_shape(&info)?
        } else {
            let (columns, layout) = table_shape(&info);
            (columns, 1, layout)
        };
        let page = self.build_tree(root, columns, key_columns, layout)?;
        self.record(
            root,
            SchemaEntry {
                kind: ObjectKind::Table,
                name: name.to_vec(),
                table: name.to_vec(),
                root: page,
                sql,
                stats: Default::default(),
                // Filled by `record` from the identifier it is given.
                tree_id: 0,
            },
        )?;

        // The indexes the table's own constraints imply. SQLite writes a
        // `sqlite_autoindex_<table>_<n>` row for each, with a NULL statement,
        // and the reader reconstructs the declaration from the table's text -
        // which is exactly what `table_from_create_sql` has already done here.
        let automatic: Vec<IndexInfo> = info.indexes.clone();
        // **A `WITHOUT ROWID` table's primary key is the table.** There is one
        // b-tree, keyed by the primary key, so SQLite writes no
        // `sqlite_autoindex_` row for it - and building one here would make a
        // second tree holding the same keys, and put a row in `sqlite_schema`
        // that SQLite's does not have. The import already refuses the same
        // shape, for the same reason.
        let primary: Vec<u16> = info.primary_key();
        for (position, index) in automatic.iter().enumerate() {
            if info.without_rowid {
                let key: Vec<u16> = index.columns.iter().filter_map(|key| key.column).collect();
                if key == primary {
                    continue;
                }
            }
            let index_root = self.allocate_root();
            let mut index = index.clone();
            index.root = index_root;
            let (columns, layout) = index_shape(&info, &index, index_root);
            let key_columns = columns.len();
            let page = self.build_tree(index_root, columns, key_columns, layout)?;
            self.record(
                index_root,
                SchemaEntry {
                    kind: ObjectKind::Index,
                    name: index.name.clone(),
                    table: name.to_vec(),
                    root: page,
                    sql: Vec::new(),
                    stats: Default::default(),
                    // Filled by `record` from the identifier it is given.
                    tree_id: 0,
                },
            )?;
            self.covering.entry(root).or_default().push(index_root);
            if let Some(slot) = info.indexes.get_mut(position) {
                slot.root = index_root;
            }
        }
        self.rebuild_tables()?;
        Ok(root)
    }

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
    fn create_index(
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
            return Err(misuse(format!(
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
            .tables
            .iter()
            .position(|held| held.folded == folded)
            .ok_or_else(|| misuse(format!("no such table: {}", String::from_utf8_lossy(table))))?;
        let owner = self
            .tables
            .get(position)
            .cloned()
            .ok_or_else(|| misuse("the table that was just found is gone"))?;
        if owner.without_rowid {
            // An index on a `WITHOUT ROWID` table has that table's primary key
            // as its trailing entry rather than a rowid, and every tree here
            // appends exactly one rowid column. Building one would produce a
            // tree whose entries point at nothing. Refused by name rather than
            // built wrong - the import refuses the same shape.
            return Err(misuse("an index on a WITHOUT ROWID table"));
        }
        let root = self.allocate_root();
        let index = index_from_create_sql(&sql, &owner, root)?;
        if index.columns.iter().any(|key| key.column.is_none()) {
            return Err(misuse("an index on an expression"));
        }
        let (columns, layout) = index_shape(&owner, &index, root);
        let key_columns = columns.len();
        let scanned = std::time::Instant::now();
        let rows = self.index_entries(&owner, &index)?;
        let scan = scanned.elapsed().as_nanos();
        let sorted = std::time::Instant::now();
        let rows = in_key_order(rows, &columns, key_columns);
        let sort = sorted.elapsed().as_nanos();
        let checked = std::time::Instant::now();
        if unique {
            refuse_duplicates(&rows, &owner, &index, key_columns)?;
        }
        let uniqueness = checked.elapsed().as_nanos();
        let packed = std::time::Instant::now();
        let page = self.build_tree_from(root, columns, key_columns, layout, &rows)?;
        let pack = packed.elapsed().as_nanos();
        // The tail is timed too, because it is not free and it is not the
        // build: recording the catalog row, re-deriving every table from the
        // catalog text, refreshing the planner's view of it, and sealing.
        let tail = std::time::Instant::now();
        self.record(
            root,
            SchemaEntry {
                kind: ObjectKind::Index,
                name: name.to_vec(),
                table: owner.name.clone(),
                root: page,
                sql,
                stats: Default::default(),
                // Filled by `record` from the identifier it is given.
                tree_id: 0,
            },
        )?;
        self.covering.entry(owner.root).or_default().push(root);
        self.sort_covering(owner.root);
        let _ = position;
        let _ = index;
        self.rebuild_tables()?;
        self.refresh_catalog();
        self.seal()?;
        self.index_stages
            .set((scan, sort, uniqueness, pack, tail.elapsed().as_nanos()));
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
    /// @param owner - the table being indexed
    /// @param index - the index's declaration
    fn index_entries(
        &self,
        owner: &TableInfo,
        index: &IndexInfo,
    ) -> DbResult<Vec<Vec<OwnedDatum>>> {
        let layout = self
            .layouts
            .get(&owner.root)
            .ok_or_else(|| misuse("no layout for the table being indexed"))?;
        let tree = self
            .trees
            .get(&owner.root)
            .ok_or_else(|| misuse("no tree for the table being indexed"))?;
        let mut sources: Vec<usize> = Vec::with_capacity(index.columns.len());
        for key in &index.columns {
            let declared = key
                .column
                .map(usize::from)
                .ok_or_else(|| misuse("an index on an expression"))?;
            let slot = layout
                .slots
                .get(declared)
                .copied()
                .flatten()
                .ok_or_else(|| misuse("an index on a column the tree does not carry"))?;
            sources.push(slot);
        }
        let rowid = layout
            .rowid
            .ok_or_else(|| misuse("an index on a table with no rowid"))?;
        let width = sources.len().saturating_add(1);
        let mut rows: Vec<Vec<OwnedDatum>> = Vec::with_capacity(tree.row_count() as usize);
        tree.visit_leaves(self.database.pool(), &mut |leaf| {
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
                for row in leaf.live()? {
                    let mut entry: Vec<OwnedDatum> = Vec::with_capacity(width);
                    for slot in sources.iter().chain(std::iter::once(&rowid)) {
                        entry.push(
                            row.get(*slot)
                                .map(OwnedDatum::from_datum)
                                .unwrap_or(OwnedDatum::Null),
                        );
                    }
                    rows.push(entry);
                }
                return Ok(true);
            }
            for row in 0..leaf.row_count() {
                let mut entry: Vec<OwnedDatum> = Vec::with_capacity(width);
                for slot in sources.iter().chain(std::iter::once(&rowid)) {
                    entry.push(OwnedDatum::from_datum(&leaf.value(row, *slot)?));
                }
                rows.push(entry);
            }
            Ok(true)
        })?;
        Ok(rows)
    }

    /// Puts a table's covering indexes back in smallest-tree-first order.
    ///
    /// @param table_root - the table whose list changed
    fn sort_covering(&mut self, table_root: u32) {
        let sizes: HashMap<u32, usize> = self
            .trees
            .iter()
            .map(|(root, tree)| (*root, tree.byte_size()))
            .collect();
        if let Some(roots) = self.covering.get_mut(&table_root) {
            roots.sort_by_key(|root| sizes.get(root).copied().unwrap_or(usize::MAX));
        }
    }

    /// Records a view or a trigger, which have text and no tree.
    ///
    /// @param keywords - the prefix the stored text carries
    /// @param kind - which of the two
    /// @param source - the statement text
    /// @param name_offset - where the object's name starts in it
    /// @param name - the object's name
    /// @param table - the table it belongs to, its own name for a view
    /// @param exists - whether one of that name is already there
    /// @param if_not_exists - whether the statement said so
    #[allow(clippy::too_many_arguments)]
    fn create_bodiless(
        &mut self,
        keywords: &str,
        kind: ObjectKind,
        source: &[u8],
        name_offset: u32,
        name: &[u8],
        table: &[u8],
        exists: bool,
        if_not_exists: bool,
    ) -> DbResult<Outcome> {
        if exists {
            if if_not_exists {
                return Ok(Outcome::empty());
            }
            return Err(misuse(format!(
                "{} {} already exists",
                match kind {
                    ObjectKind::View => "view",
                    _ => "trigger",
                },
                String::from_utf8_lossy(name)
            )));
        }
        let sql = canonical_sql(keywords, source, name_offset, source.len() as u32);
        self.record(
            0,
            SchemaEntry {
                kind,
                name: name.to_vec(),
                table: table.to_vec(),
                root: PageId::NONE,
                sql,
                stats: Default::default(),
                // Filled by `record` from the identifier it is given.
                tree_id: 0,
            },
        )?;
        self.rebuild_tables()?;
        self.refresh_catalog();
        self.seal()?;
        Ok(Outcome::empty())
    }

    /// Drops a table, index, view or trigger.
    ///
    /// A `DROP TABLE` takes its indexes and its triggers with it, and gives
    /// every page all of their trees held back to the free map in the same
    /// transaction - which is the TDD's rule and the difference between a drop
    /// and a leak.
    ///
    /// @param kind - which kind of object
    /// @param name - its name
    /// @param exists - whether it is there
    /// @param if_exists - whether the statement said `IF EXISTS`
    fn drop_object(
        &mut self,
        kind: inillucent_sql::ast::ObjectKind,
        name: &[u8],
        exists: bool,
        if_exists: bool,
    ) -> DbResult<Outcome> {
        use inillucent_sql::ast::ObjectKind as Ast;
        if !exists {
            if if_exists {
                return Ok(Outcome::empty());
            }
            return Err(misuse(format!(
                "no such {}: {}",
                match kind {
                    Ast::Table => "table",
                    Ast::Index => "index",
                    Ast::View => "view",
                    Ast::Trigger => "trigger",
                },
                String::from_utf8_lossy(name)
            )));
        }
        let folded = name.to_ascii_lowercase();
        match kind {
            Ast::Table => {
                let position = self
                    .tables
                    .iter()
                    .position(|held| held.folded == folded)
                    .ok_or_else(|| {
                        misuse(format!("no such table: {}", String::from_utf8_lossy(name)))
                    })?;
                let owner = self
                    .tables
                    .get(position)
                    .cloned()
                    .ok_or_else(|| misuse("the table that was just found is gone"))?;
                // Every row that names the table: the table, its indexes and its
                // triggers. Collected before anything is removed, because the
                // list is what decides what to remove.
                let doomed: Vec<i64> = self
                    .entries
                    .iter()
                    .filter(|held| {
                        held.entry.name.to_ascii_lowercase() == folded
                            || held.entry.table.to_ascii_lowercase() == folded
                    })
                    .map(|held| held.rowid)
                    .collect();
                for rowid in doomed {
                    self.forget(rowid)?;
                }
                for index in &owner.indexes {
                    self.release_tree(index.root)?;
                }
                self.release_tree(owner.root)?;
                let _ = position;
            }
            Ast::Index => {
                let found = self.tables.iter().enumerate().find_map(|(at, table)| {
                    table
                        .indexes
                        .iter()
                        .position(|index| index.folded == folded)
                        .map(|which| (at, which, table.root))
                });
                let Some((table_at, index_at, table_root)) = found else {
                    return Err(misuse(format!(
                        "no such index: {}",
                        String::from_utf8_lossy(name)
                    )));
                };
                let index_root = self
                    .tables
                    .get(table_at)
                    .and_then(|table| table.indexes.get(index_at))
                    .map(|index| index.root)
                    .ok_or_else(|| misuse("the index that was just found is gone"))?;
                let rowids: Vec<i64> = self
                    .entries
                    .iter()
                    .filter(|held| {
                        held.entry.kind == ObjectKind::Index
                            && held.entry.name.to_ascii_lowercase() == folded
                    })
                    .map(|held| held.rowid)
                    .collect();
                for rowid in rowids {
                    self.forget(rowid)?;
                }
                self.release_tree(index_root)?;
                let _ = (table_at, index_at);
                self.sort_covering(table_root);
            }
            Ast::View | Ast::Trigger => {
                let wanted = if kind == Ast::View {
                    ObjectKind::View
                } else {
                    ObjectKind::Trigger
                };
                let rowids: Vec<i64> = self
                    .entries
                    .iter()
                    .filter(|held| {
                        held.entry.kind == wanted && held.entry.name.to_ascii_lowercase() == folded
                    })
                    .map(|held| held.rowid)
                    .collect();
                for rowid in rowids {
                    self.forget(rowid)?;
                }
            }
        }
        self.rebuild_tables()?;
        self.refresh_catalog();
        self.seal()?;
        Ok(Outcome::empty())
    }

    /// Runs an `ALTER TABLE`.
    ///
    /// Every rewrite is a rewrite of *stored text*, and the catalog is then
    /// rebuilt from that text, so there is one derivation of what a schema means
    /// and `ALTER` does not get its own.
    ///
    /// @param source - the statement text, for `ADD COLUMN`'s definition
    /// @param table - the table being altered
    /// @param action - what to do to it
    fn alter_table(
        &mut self,
        source: &[u8],
        table: &[u8],
        action: &AlterKind,
    ) -> DbResult<Outcome> {
        let folded = table.to_ascii_lowercase();
        if !self.tables.iter().any(|held| held.folded == folded) {
            return Err(misuse(format!(
                "no such table: {}",
                String::from_utf8_lossy(table)
            )));
        }
        let mut updates: Vec<(i64, SchemaEntry)> = Vec::new();
        for held in &self.entries {
            let (rowid, entry) = (&held.rowid, &held.entry);
            let owns = entry.table.to_ascii_lowercase() == folded;
            let itself =
                entry.name.to_ascii_lowercase() == folded && entry.kind == ObjectKind::Table;
            if entry.sql.is_empty() {
                // An automatic index has no statement of its own, but its
                // `tbl_name` and its generated name still follow a rename.
                if owns {
                    if let AlterKind::RenameTable { to } = action {
                        let mut moved = entry.clone();
                        moved.table = to.clone();
                        moved.name = renamed_automatic(&entry.name, table, to);
                        updates.push((*rowid, moved));
                    }
                }
                continue;
            }
            let rewritten = match action {
                AlterKind::RenameTable { to } => {
                    let next = rename::rewrite(&entry.sql, rename::Rename::Table, table, to)?;
                    if next == entry.sql && !owns {
                        continue;
                    }
                    let mut moved = entry.clone();
                    moved.sql = rename::reparsed(next)?;
                    if itself {
                        moved.name = to.clone();
                        moved.table = to.clone();
                    } else if owns {
                        moved.table = to.clone();
                    }
                    moved
                }
                AlterKind::RenameColumn { from, to } => {
                    if !owns {
                        let reads = rename::referenced_tables(&entry.sql);
                        if !reads.iter().any(|name| *name == folded) {
                            continue;
                        }
                        if reads.len() > 1 {
                            return Err(misuse(format!(
                                "error in {}: cannot rename a column it reads alongside another table",
                                String::from_utf8_lossy(&entry.name)
                            )));
                        }
                    }
                    let next = rename::rewrite(&entry.sql, rename::Rename::Column, from, to)?;
                    if next == entry.sql {
                        continue;
                    }
                    let mut moved = entry.clone();
                    moved.sql = rename::reparsed(next)?;
                    moved
                }
                AlterKind::AddColumn { start, end } => {
                    if !itself {
                        continue;
                    }
                    let definition = source
                        .get(*start as usize..*end as usize)
                        .unwrap_or_default()
                        .to_vec();
                    let mut moved = entry.clone();
                    moved.sql = rename::reparsed(rename::add_column(&entry.sql, &definition)?)?;
                    moved
                }
                AlterKind::DropColumn { position, .. } => {
                    if !itself {
                        continue;
                    }
                    let mut moved = entry.clone();
                    moved.sql =
                        rename::reparsed(rename::drop_column(&entry.sql, usize::from(*position))?)?;
                    moved
                }
            };
            updates.push((*rowid, rewritten));
        }
        for (rowid, entry) in updates {
            self.rewrite(rowid, entry)?;
        }
        self.rebuild_tables()?;
        // A `DROP COLUMN` changes the *rows*, not only the text, and the tree is
        // rebuilt rather than edited in place: every leaf's column directory
        // would otherwise still describe a column the catalog no longer has.
        if let AlterKind::DropColumn { .. } = action {
            self.rebuild_table_tree(&folded)?;
        }
        if let AlterKind::AddColumn { .. } = action {
            self.rebuild_table_tree(&folded)?;
        }
        self.refresh_catalog();
        self.seal()?;
        Ok(Outcome::empty())
    }

    /// Returns the value a column's `DEFAULT` has for a row that predates it.
    ///
    /// It is evaluated by *running* it - `SELECT <the default text>` through
    /// the ordinary compile-and-execute path - rather than by a second
    /// expression evaluator written for the DDL path. `DEFAULT (1 + 1)` and
    /// `DEFAULT 'x' || 'y'` are expressions, and an evaluator that handled only
    /// literals would fill NULL for those while filling the right value for the
    /// simple ones, which is the shape of bug that hides.
    ///
    /// A default that cannot be evaluated - one calling a function this engine
    /// does not have - reports itself rather than silently becoming NULL.
    ///
    /// @param default_sql - the `DEFAULT` text as the declaration wrote it
    fn constant_default(&mut self, default_sql: &[u8]) -> DbResult<OwnedDatum> {
        let text = String::from_utf8_lossy(default_sql).into_owned();
        let rows = self.query_internally(&format!("SELECT {text}"))?;
        Ok(rows
            .first()
            .and_then(|row| row.first())
            .cloned()
            .unwrap_or(OwnedDatum::Null))
    }

    /// Rebuilds every `TableInfo` from the catalog rows.
    ///
    /// After an `ALTER`, because the stored text is what changed and the derived
    /// view has to be derived again. The identifiers are carried across by name
    /// so the trees a plan will read stay the trees they were.
    pub(super) fn rebuild_tables(&mut self) -> DbResult<()> {
        let entries: Vec<inillucent_catalog::paged::SchemaEntry> =
            self.entries.iter().map(|held| held.entry.clone()).collect();
        let roots: Vec<u32> = self.entries.iter().map(|held| held.root).collect();
        let mut rebuilt = tables_from_entries(&entries, &roots, 0)?;
        // **A virtual table's columns come from its module, not its text.**
        // `CREATE VIRTUAL TABLE documents USING fts5(title, body)` names a
        // module and its arguments; what the *columns* are is the module's
        // answer, and only a connected module can give it. Deriving them from
        // the statement would be a second implementation of every module's
        // argument grammar, agreeing with the module until the day it did not.
        for table in &mut rebuilt {
            let Some(connected) = self.virtual_tables.get(&table.folded) else {
                continue;
            };
            let declaration = connected.table.declaration();
            table.kind = inillucent_sql::catalog_view::TableKind::Virtual;
            table.without_rowid = declaration.without_rowid;
            table.columns = inillucent_sql::declare::declared_columns(declaration);
            table.module = Some(inillucent_sql::vtab::ModuleRef {
                name: connected.arguments.module.clone(),
                folded: connected.arguments.module.to_ascii_lowercase(),
                arguments: connected.arguments.arguments.clone(),
            });
        }
        self.tables = rebuilt;
        Ok(())
    }

    /// Rebuilds one table's tree so its leaves carry the columns the catalog
    /// now says it has.
    ///
    /// `ADD COLUMN` and `DROP COLUMN` both change the column directory, and a
    /// leaf's directory is written into the page - so the rows are read out
    /// through the old layout, re-shaped, and packed into a fresh tree. The old
    /// tree's pages go back to the free map.
    ///
    /// @param folded - the table's folded name
    fn rebuild_table_tree(&mut self, folded: &[u8]) -> DbResult<()> {
        let Some(info) = self
            .tables
            .iter()
            .find(|table| table.folded == folded)
            .cloned()
        else {
            return Ok(());
        };
        let old_root = info.root;
        let old_layout = self
            .layouts
            .get(&old_root)
            .cloned()
            .ok_or_else(|| misuse("no layout for the table being rebuilt"))?;
        let old_rows = {
            let tree = self
                .trees
                .get(&old_root)
                .ok_or_else(|| misuse("no tree for the table being rebuilt"))?;
            tree.rows(self.database.pool())?
        };
        let (columns, key_columns, layout) = if info.without_rowid {
            keyed_table_shape(&info)?
        } else {
            let (columns, layout) = table_shape(&info);
            (columns, 1, layout)
        };
        // Each new tree column is filled from the old tree column that held the
        // same *declared* column. A column the declaration did not have takes
        // its `DEFAULT`, which is SQLite's rule and is what makes
        // `ALTER TABLE t ADD COLUMN c INTEGER DEFAULT 9` answer 9 for the rows
        // that were already there. Filling NULL instead was a wrong answer
        // rather than a refusal, and only visible to a statement that read the
        // new column on an old row.
        let mut from: Vec<Fill> = vec![Fill::Absent; layout.width];
        for (declared, slot) in layout.slots.iter().enumerate() {
            let Some(slot) = slot else { continue };
            if let Some(Some(source)) = old_layout.slots.get(declared) {
                if let Some(cell) = from.get_mut(*slot) {
                    *cell = Fill::From(*source);
                }
                continue;
            }
            let Some(default) = info
                .columns
                .get(declared)
                .and_then(|column| column.default_sql.clone())
            else {
                continue;
            };
            let value = self.constant_default(&default)?;
            if let Some(cell) = from.get_mut(*slot) {
                *cell = Fill::Constant(value);
            }
        }
        if let (Some(new_rowid), Some(old_rowid)) = (layout.rowid, old_layout.rowid) {
            if let Some(cell) = from.get_mut(new_rowid) {
                *cell = Fill::From(old_rowid);
            }
        }
        let rows: Vec<Vec<OwnedDatum>> = old_rows
            .iter()
            .map(|row| {
                from.iter()
                    .map(|source| match source {
                        Fill::From(at) => row.get(*at).cloned().unwrap_or(OwnedDatum::Null),
                        Fill::Constant(value) => value.clone(),
                        Fill::Absent => OwnedDatum::Null,
                    })
                    .collect()
            })
            .collect();
        let rows = in_key_order(rows, &columns, key_columns);
        self.release_tree(old_root)?;
        let covering: Vec<u32> = self.covering.get(&old_root).cloned().unwrap_or_default();
        self.build_tree_from(old_root, columns, key_columns, layout, &rows)?;
        if !covering.is_empty() {
            self.covering.insert(old_root, covering);
        }
        // The catalog row's `rootpage` moved with the tree.
        let page = self
            .trees
            .get(&old_root)
            .map(PagedTree::root)
            .unwrap_or(PageId::NONE);
        let update = self
            .entries
            .iter()
            .find(|held| {
                held.entry.kind == ObjectKind::Table
                    && held.entry.name.to_ascii_lowercase() == folded
            })
            .map(|held| {
                let mut moved = held.entry.clone();
                moved.root = page;
                (held.rowid, moved)
            });
        if let Some((rowid, entry)) = update {
            self.rewrite(rowid, entry)?;
        }
        Ok(())
    }

    /// Rebuilds indexes, which for this engine is a no-op with a check.
    ///
    /// A `REINDEX` exists to repair an index whose collation sequence changed
    /// under it. Every collation this engine orders a tree by is built in and
    /// cannot change, so there is nothing to repair - but the trees are checked
    /// rather than the statement being ignored, so `REINDEX` still answers the
    /// question a person runs it to ask.
    ///
    /// @param indexes - the indexes named, empty for all of them
    fn reindex(&mut self, indexes: &[Vec<u8>]) -> DbResult<Outcome> {
        let wanted: Vec<Vec<u8>> = indexes
            .iter()
            .map(|name| name.to_ascii_lowercase())
            .collect();
        let roots: Vec<u32> = self
            .tables
            .iter()
            .flat_map(|table| table.indexes.iter())
            .filter(|index| wanted.is_empty() || wanted.contains(&index.folded))
            .map(|index| index.root)
            .collect();
        for root in roots {
            if let Some(tree) = self.trees.get(&root) {
                tree.check(self.database.pool())?;
            }
        }
        Ok(Outcome::empty())
    }
}

/// Refuses a `CREATE UNIQUE INDEX` whose entries are not unique.
///
/// The entries are already in key order, so a duplicate is two adjacent rows
/// agreeing on every key column but the rowid - which is the definition, and is
/// one comparison rather than a set.
///
/// @param rows - the entries, sorted
/// @param owner - the table, for the message
/// @param index - the index, for the message
/// @param key_columns - how wide the entry is, rowid included
fn refuse_duplicates(
    rows: &[Vec<OwnedDatum>],
    owner: &TableInfo,
    index: &IndexInfo,
    key_columns: usize,
) -> DbResult<()> {
    let compared = key_columns.saturating_sub(1);
    for window in rows.windows(2) {
        let (Some(left), Some(right)) = (window.first(), window.get(1)) else {
            continue;
        };
        let same = (0..compared).all(|column| left.get(column) == right.get(column));
        if same {
            let columns: Vec<String> = index
                .columns
                .iter()
                .map(|key| {
                    let name = key
                        .column
                        .and_then(|at| owner.column(at))
                        .map(|column| String::from_utf8_lossy(&column.name).into_owned())
                        .unwrap_or_else(|| "?".to_string());
                    format!("{}.{}", String::from_utf8_lossy(&owner.name), name)
                })
                .collect();
            return Err(misuse(format!(
                "UNIQUE constraint failed: {}",
                columns.join(", ")
            )));
        }
    }
    Ok(())
}

/// Returns the new name of an automatic index when its table is renamed.
///
/// SQLite names them `sqlite_autoindex_<table>_<n>`, so the name has to follow
/// the table or the next `CREATE TABLE` of the old name would collide with it.
///
/// @param name - the index's current name
/// @param from - the table's old name
/// @param to - its new one
fn renamed_automatic(name: &[u8], from: &[u8], to: &[u8]) -> Vec<u8> {
    let prefix = b"sqlite_autoindex_";
    let Some(rest) = name.strip_prefix(prefix.as_slice()) else {
        return name.to_vec();
    };
    let Some(suffix) = rest.strip_prefix(from) else {
        return name.to_vec();
    };
    let mut out = prefix.to_vec();
    out.extend_from_slice(to);
    out.extend_from_slice(suffix);
    out
}

impl Outcome {
    /// Returns the answer a statement that produced no rows gives.
    pub fn empty() -> Outcome {
        Outcome {
            rows: Vec::new(),
            names: Vec::new(),
            changes: Changes::default(),
        }
    }
}

/// Where one column of a rebuilt tree gets its values.
///
/// Three cases and not two: a column carried across, a column the declaration
/// has just gained with a `DEFAULT`, and one it has gained without.
#[derive(Clone, Debug)]
enum Fill {
    /// The old tree column that held the same declared column.
    From(usize),
    /// The `DEFAULT` a new column declared.
    Constant(OwnedDatum),
    /// Nothing: a new column with no default, which is NULL.
    Absent,
}
