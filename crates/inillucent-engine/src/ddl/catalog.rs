//! The catalog rows a `CREATE` writes, and the view the binder reads.
//!
//! Invariant: **the catalog on disk and the view in memory are rebuilt
//! together.** `record` writes the row and `refresh_catalog` rebuilds the view
//! the binder will be handed; a statement that did one without the other would
//! be compiled against a schema the file does not have.

use inillucent_base::error::refusal;
use inillucent_base::DbResult;
use inillucent_catalog::load::table_from_create_sql;
use inillucent_catalog::paged::{delete_entry, insert_entry, SchemaEntry};
use inillucent_exec::physical::SourceLayout;
use inillucent_sql::catalog_view::StaticCatalog;

use super::*;
use crate::*;

impl crate::ImportedDatabase {
    /// Returns how many times the catalog has changed.
    ///
    /// A plan compiled at one generation is never run at another, because the
    /// statement cache is emptied in the same breath the generation is bumped.
    /// This is here so a test can say so rather than infer it.
    pub fn catalog_generation(&self) -> u64 {
        self.schema.catalog_generation
    }
    /// Returns the catalog's rows, in the order the tree holds them.
    ///
    /// For the acceptance tests, which compare them against SQLite's
    /// `sqlite_schema`.
    pub fn schema_entries(&self) -> Vec<(i64, SchemaEntry)> {
        self.schema
            .entries
            .iter()
            .map(|held| (held.rowid, held.entry.clone()))
            .collect()
    }
    /// Returns the identifier the next created tree is registered under.
    pub(crate) fn allocate_root(&mut self) -> DbResult<u32> {
        // The handle, which is what everything above the file names the tree by.
        // Its file-local identifier goes into the catalog row and into every log
        // record, and `record` reads it back with `local_of`.
        Ok(self.allocate_in(self.schema.ddl_schema)?.1)
    }
    /// Returns the rowid the next catalog row takes.
    ///
    /// One past the largest in use, which is what an `INSERT` into a rowid table
    /// with no explicit key does - and SQLite writes its own `sqlite_schema`
    /// rows with exactly that statement.
    pub(crate) fn next_catalog_rowid(&self) -> i64 {
        self.entries_of(self.schema.ddl_schema)
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
    pub(crate) fn refresh_catalog(&mut self) {
        // **The keys are re-planned every time the schema changes**, and this
        // is the only place that can do it: a foreign key records the child's
        // side alone, so the parent's trigger is found by asking every table
        // what it points at - which cannot be answered one `CREATE TABLE` at a
        // time. Doing it here rather than in `create_table` is also what makes
        // `CREATE TABLE child(... REFERENCES parent)` written *before* the
        // parent exists start being enforced when the parent arrives.
        inillucent_sql::foreign_key::plan_schema(
            &mut self.schema.tables,
            b"main",
            &inillucent_base::limits::Limits::default(),
        );
        // **Before the snapshot, because the snapshot is what the planner
        // reads.** An index a module owns is published onto its table's
        // `indexes` list, and a catalog built before that happened describes a
        // table with no such index - so the one path that can use it is never
        // offered.
        self.refresh_vector_indexes();
        // **And the measurements, for the same reason.** `ANALYZE` writes
        // `sqlite_stat1` and then rebuilds the catalog; a snapshot taken before
        // the new rows were read describes tables whose row counts are still
        // the planner's guesses, so the statement right after an `ANALYZE`
        // would be planned as though it had not run.
        self.republish_statistics();
        // **And the imposters, which a rebuild would otherwise drop.** They are
        // not schema objects, so `rebuild_tables` does not know about them;
        // they are put back after it exactly as the module-owned indexes are.
        self.republish_imposters();
        let mut catalog = StaticCatalog::empty();
        // **In attachment order, `main` first.** The binder numbers schemas by
        // their position here and resolves an unqualified name by walking
        // `temp`, then `main`, then the attachments in the order they arrived -
        // which is SQLite's order and is what `main_wins_an_unqualified_name`
        // grades.
        // **`temp` is listed whether or not this connection has made one.** The
        // binder refuses `CREATE TEMP TABLE` when the name is not in the
        // catalog, and the database it would go into is made by the statement
        // that first needs it - so the name has to be there before the schema
        // is. It also fixes every attachment's number: `main`, `temp`, then the
        // attachments, which is SQLite's own layout.
        catalog.databases.push((b"temp".to_vec(), 0));
        for held in &self.session_state.attached {
            catalog.databases.push((held.name.clone(), 0));
        }
        for table in &self.schema.tables {
            catalog = catalog.with_table(table.clone());
        }
        catalog = catalog.with_table(self.schema.schema_info.clone());
        catalog = catalog.with_table(crate::schema_alias_of(&self.schema.schema_info));
        // Each attached database's own `sqlite_schema`, reachable only when it
        // is qualified: an unqualified `sqlite_schema` is `main`'s, which is
        // what SQLite answers and what the search order above already gives.
        for held in &self.session_state.attached {
            catalog = catalog.with_table(held.schema_info.clone());
            catalog = catalog.with_table(crate::schema_alias_of(&held.schema_info));
        }
        // A temporary database's catalog answers to `sqlite_temp_schema` and
        // `sqlite_temp_master`, which is how SQLite names it - and registering
        // it as `sqlite_schema` as well would put it *first* in the search order
        // and make an unqualified `sqlite_schema` mean the temporary one.
        // The temporary database's catalog answers to `sqlite_temp_schema` and
        // `sqlite_temp_master`, which is how SQLite names it - and registering
        // it as `sqlite_schema` as well would put it *first* in the search order
        // and make an unqualified `sqlite_schema` mean the temporary one.
        if let Some(held) = self.session_state.schema_at(crate::TEMP) {
            let temp_schema = crate::schema_named(&held.schema_info, b"sqlite_temp_schema");
            let temp_master = crate::schema_named(&held.schema_info, b"sqlite_temp_master");
            catalog = catalog.with_table(temp_schema);
            catalog = catalog.with_table(temp_master);
        }
        // **The eponymous modules, last.** `generate_series`, `json_each`,
        // `json_tree` and the `pragma_*` set are names rather than tables: they
        // belong to no database, have no `sqlite_schema` row, and the binder is
        // already written to turn `FROM generate_series(1,10)` into `Eq`
        // constraints on their hidden columns. The one thing missing was
        // anything putting them in the catalog, so every one of them was
        // `no such table`. They go on last, so a real table of the
        // same name shadows the module.
        if self.session_state.eponymous.is_empty() {
            self.session_state.eponymous = self.eponymous_tables();
        }
        for table in &self.session_state.eponymous {
            catalog = catalog.with_eponymous(table.clone());
        }
        self.schema.catalog = catalog;
        self.forget_compiled_statements();
        self.schema.catalog_generation = self.schema.catalog_generation.saturating_add(1);
        // Last, after the catalog a module would read is the new one; see
        // `vtab::schema_changed_modules` (task-1932, M2).
        self.schema_changed_modules();
    }
    /// Returns one `TableInfo` per eponymous module the registry holds.
    ///
    /// The columns come from the module's own `connect`, for the same reason a
    /// `CREATE VIRTUAL TABLE`'s do: what the columns are is the module's answer,
    /// and deriving them anywhere else would be a second implementation of its
    /// declaration that agreed with the module until the day it did not.
    ///
    /// A module whose `connect` fails with no arguments is skipped rather than
    /// reported: it is a module that cannot be used eponymously, which is not
    /// an error in the schema this is refreshing.
    fn eponymous_tables(&self) -> Vec<inillucent_sql::catalog_view::TableInfo> {
        let mut tables = Vec::new();
        for name in self.session_state.registry.module_names() {
            let Some(module) = self.session_state.registry.eponymous(name.as_bytes()) else {
                continue;
            };
            let arguments = inillucent_sql::vtab::ModuleArguments {
                database: 0,
                schema: b"main".to_vec(),
                table: name.as_bytes().to_vec(),
                module: name.as_bytes().to_vec(),
                arguments: Vec::new(),
                shadows: Vec::new(),
            };
            let Ok(connected) = module.connect(&arguments, false) else {
                continue;
            };
            let declaration = connected.declaration();
            tables.push(inillucent_sql::catalog_view::TableInfo::eponymous(
                name.as_bytes().to_vec(),
                inillucent_sql::declare::declared_columns(declaration),
                inillucent_sql::vtab::ModuleRef {
                    name: name.as_bytes().to_vec(),
                    folded: name.to_ascii_lowercase().into_bytes(),
                    arguments: Vec::new(),
                },
                declaration.without_rowid,
            ));
        }
        // **The `pragma_*` functions are the same mechanism over the pragma
        // set.** They are not registry modules - a module reaches its rows
        // through a `Context`, and a pragma's rows come from the connection
        // itself - so their columns are read straight off
        // `ImportedDatabase::pragma_rows`, which is the same function the
        // directive form runs. Two hidden columns follow, `arg` and `schema`,
        // which is SQLite's shape and is what makes
        // `SELECT * FROM pragma_table_info(name)` over a list of table names a
        // join rather than a loop.
        for name in Self::pragma_function_names() {
            let Some(table) = self.pragma_function_table(name) else {
                continue;
            };
            tables.push(table);
        }
        // **`dbstat` and `sqlite_dbpage` are the engine's too**, and for the
        // same reason: both are questions about the *pages* under every tree,
        // and a `Module` sees a shadow store rather than a pager. See
        // `crate::inspect`.
        // **The four that describe statements are the engine's too**, for the
        // same reason: `bytecode` and `tables_used` compile the SQL they are
        // handed, `sqlite_stmt` reads the statement cache, and `completion`
        // reads the keyword table and the catalog. See `crate::introspect`.
        for (name, columns, hidden) in [
            (
                "bytecode",
                crate::introspect::BYTECODE_COLUMNS,
                &["stmt"][..],
            ),
            (
                "tables_used",
                crate::introspect::TABLES_USED_COLUMNS,
                &["stmt"][..],
            ),
            ("sqlite_stmt", crate::introspect::STMT_COLUMNS, &[][..]),
            (
                "completion",
                crate::introspect::COMPLETION_COLUMNS,
                crate::introspect::COMPLETION_HIDDEN,
            ),
        ] {
            let mut declared: Vec<inillucent_sql::catalog_view::ColumnInfo> = columns
                .iter()
                .map(|held| pragma_column(held.as_bytes(), false))
                .collect();
            declared.extend(
                hidden
                    .iter()
                    .map(|held| pragma_column(held.as_bytes(), true)),
            );
            tables.push(inillucent_sql::catalog_view::TableInfo::eponymous(
                name.as_bytes().to_vec(),
                declared,
                inillucent_sql::vtab::ModuleRef {
                    name: name.as_bytes().to_vec(),
                    folded: name.as_bytes().to_vec(),
                    arguments: Vec::new(),
                },
                false,
            ));
        }
        for (name, columns) in [
            ("dbstat", crate::inspect::DBSTAT_COLUMNS),
            ("sqlite_dbpage", crate::inspect::DBPAGE_COLUMNS),
        ] {
            let mut declared: Vec<inillucent_sql::catalog_view::ColumnInfo> = columns
                .iter()
                .map(|held| pragma_column(held.as_bytes(), false))
                .collect();
            // `schema` is hidden in SQLite too: it selects which attached
            // database is described, and it is not part of a `SELECT *`.
            declared.push(pragma_column(b"schema", true));
            tables.push(inillucent_sql::catalog_view::TableInfo::eponymous(
                name.as_bytes().to_vec(),
                declared,
                inillucent_sql::vtab::ModuleRef {
                    name: name.as_bytes().to_vec(),
                    folded: name.as_bytes().to_ascii_lowercase(),
                    arguments: Vec::new(),
                },
                false,
            ));
        }
        tables.sort_by(|left, right| left.folded.cmp(&right.folded));
        tables
    }
    /// Returns the eponymous table one `pragma_*` function presents.
    ///
    /// `None` when the pragma answers no columns, which is how a name that has
    /// no read form is left out rather than presented as a function that always
    /// finds nothing.
    ///
    /// @param name - the function's name, `pragma_` and the pragma's own
    fn pragma_function_table(&self, name: &str) -> Option<inillucent_sql::catalog_view::TableInfo> {
        let pragma = name.strip_prefix("pragma_")?;
        let shape = self.pragma_rows(pragma.as_bytes(), None).ok()??;
        if shape.names.is_empty() {
            return None;
        }
        let mut columns: Vec<inillucent_sql::catalog_view::ColumnInfo> = shape
            .names
            .iter()
            .map(|held| pragma_column(held.as_bytes(), false))
            .collect();
        columns.push(pragma_column(b"arg", true));
        columns.push(pragma_column(b"schema", true));
        Some(inillucent_sql::catalog_view::TableInfo::eponymous(
            name.as_bytes().to_vec(),
            columns,
            inillucent_sql::vtab::ModuleRef {
                name: name.as_bytes().to_vec(),
                folded: name.as_bytes().to_ascii_lowercase(),
                arguments: Vec::new(),
            },
            false,
        ))
    }
    /// Throws away every statement compiled against the catalog as it was.
    ///
    /// A compiled statement carries decisions the catalog and the connection's
    /// settings made when it was compiled - which tree it reads, which index it
    /// probes, and whether its foreign keys are checked. Anything that changes
    /// one of those has to come through here, or the next execution answers
    /// with the old decision.
    pub(crate) fn forget_compiled_statements(&self) {
        self.compiled.statements.borrow_mut().clear();
    }
    /// Returns what a catalog-row write on `self.schema.ddl_schema` needs that is not
    /// the borrow of `self.writing.undo` a method cannot hand back - callers still
    /// write their own `WalLog` literal so that borrow stays disjoint from the
    /// `&mut self.storage.database` they take right after, the reason
    /// [`crate::file_of`] is a free function too.
    pub(crate) fn catalog_write(&self) -> DbResult<CatalogWrite> {
        let at = self.schema.ddl_schema;
        let wal = self
            .log_of(at)
            .ok_or_else(|| refusal("a statement names a database that is not attached"))?;
        Ok((
            at,
            self.current_txn(),
            self.writing.batch.get().is_some(),
            wal,
            self.uncommitted_handle_of(at),
        ))
    }
    /// Writes one row into the catalog tree and records it.
    ///
    /// @param entry - the object to record
    pub(crate) fn record(&mut self, root: u32, mut entry: SchemaEntry) -> DbResult<()> {
        let rowid = self.next_catalog_rowid();
        // The statistics come off the tree that was just built rather than from
        // the caller, so there is one place they can be wrong instead of four.
        entry.stats = self.tree_stats(root);
        // And the identifier, for the same reason and from the same argument.
        // The log refers to a tree by this number, so a caller that filled it in
        // itself would be a fifth place it could disagree with the tree it
        // describes - and a catalog naming the wrong tree would send recovery's
        // row records somewhere else.
        let at = self.schema.ddl_schema;
        entry.tree_id = self.local_of(at, root);
        let catalog_handle = self.catalog_handle_of(at);
        {
            let (_, txn, _open, wal, uncommitted) = self.catalog_write()?;
            let mut log = WalLog {
                wal,
                txn,
                schema: at,
                wrote: false,
                // **A before-image whether or not a transaction is open
                // (task-1932, H3).** This was `open.then_some(&self.writing.undo)`, so
                // outside an explicit transaction a catalog write recorded
                // nothing to put back - and `execute_ddl`, which now takes an
                // undo floor the way `write` does, would have had an empty
                // buffer to undo from. A directive is several catalog writes
                // and a failure in a later one has to unwrite the earlier ones.
                // `build_tree_rows` is deliberately still gated: a bulk build's
                // before-images are one record per row of the table, and a
                // freshly built tree has no earlier state to restore to.
                undo: Some(&self.writing.undo),
                uncommitted,
            };
            let tree = self
                .schema
                .trees
                .get_mut(&catalog_handle)
                .ok_or_else(|| refusal("the catalog tree is not attached"))?;
            let session = self.session_state.session.get();
            let database = crate::file_of(
                &mut self.storage.database,
                &mut self.session_state.attached,
                &mut self.session_state.temps,
                session,
                at,
            )?;
            insert_entry(database, tree, &mut log, rowid, &entry)?;
        }
        self.entries_of_mut(at)
            .ok_or_else(|| refusal("a statement names a database that is not attached"))?
            .push(Recorded { rowid, root, entry });
        // A schema change is a write, and a transaction that made one in two
        // files commits both or neither like any other.
        self.writing
            .touched
            .set(self.writing.touched.get() | crate::schema_bit(at));
        Ok(())
    }
    /// Returns the shape of a tree, for its catalog row.
    ///
    /// A tree the catalog names but the maps do not hold - a view, a trigger -
    /// has no shape, and zero is what "unknown" reads as.
    ///
    /// @param root - the tree's identifier
    pub(crate) fn tree_stats(&self, root: u32) -> inillucent_catalog::paged::TreeStats {
        match self.schema.trees.get(&root) {
            Some(tree) => inillucent_catalog::paged::TreeStats {
                first_leaf: tree.first_leaf(),
                leaf_count: tree.leaf_count(),
                row_count: tree.row_count(),
            },
            None => inillucent_catalog::paged::TreeStats::default(),
        }
    }
    /// Makes a table that reads one index's own b-tree, or removes them all.
    ///
    /// **`.imposter`'s subject, and it is a forensic tool rather than a
    /// feature.** An index's entries are the indexed columns followed by the
    /// row's identity, and that is a perfectly good `WITHOUT ROWID` table - so
    /// declaring one over the index's tree lets a person read what the index
    /// actually holds when a query over it is answering wrongly. Nothing is
    /// written to the file: the declaration lives on this connection and goes
    /// with it.
    ///
    /// @param index - the index to read, or nothing to remove every imposter
    /// @param name - the table name to declare it under
    pub fn imposter(&mut self, index: Option<&[u8]>, name: &[u8]) -> DbResult<Option<String>> {
        let Some(index) = index else {
            // **Removed from the catalog as well as from the list.**
            // `refresh_catalog` rebuilds the *snapshot* rather than the tables,
            // so a declaration this connection added stays until it is taken
            // out - and `.imposter off` that left the table queryable would be
            // the one thing the command exists to undo.
            let held = std::mem::take(&mut self.schema.imposters);
            for (info, layout, _) in held {
                self.schema
                    .tables
                    .retain(|table| table.folded != info.folded);
                self.schema.layouts.remove(&layout.tree_key);
                self.schema.trees.remove(&info.root);
            }
            self.refresh_catalog();
            return Ok(None);
        };
        let folded = index.to_ascii_lowercase();
        let Some((owner, declared)) = self.schema.tables.iter().find_map(|table| {
            table
                .indexes
                .iter()
                .find(|held| held.folded == folded)
                .map(|held| (table.clone(), held.clone()))
        }) else {
            return Err(refusal(format!(
                "no such index: \"{}\"",
                String::from_utf8_lossy(index)
            )));
        };
        let Some(tree) = self.schema.trees.get(&declared.root).cloned() else {
            return Err(refusal(format!(
                "the index {} has no tree",
                String::from_utf8_lossy(index)
            )));
        };
        // The entry's columns, in the order the tree holds them: the index's
        // own keys, then what identifies the table row - which is a rowid on an
        // ordinary table and the primary key on a `WITHOUT ROWID` one. The
        // reference calls the rowid `_ROWID_`, and so does this.
        let mut columns: Vec<Vec<u8>> = Vec::new();
        for key in &declared.columns {
            let name = key
                .column
                .and_then(|at| owner.columns.get(usize::from(at)))
                .map(|held| held.name.clone())
                .unwrap_or_else(|| b"expr".to_vec());
            columns.push(name);
        }
        let trailing = crate::identity_columns(&owner);
        if trailing.is_empty() {
            columns.push(b"_ROWID_".to_vec());
        } else {
            for declared in &trailing {
                columns.push(
                    owner
                        .columns
                        .get(*declared)
                        .map(|held| held.name.clone())
                        .unwrap_or_else(|| b"key".to_vec()),
                );
            }
        }
        let quoted: Vec<String> = columns
            .iter()
            .map(|held| format!("\"{}\"", String::from_utf8_lossy(held)))
            .collect();
        let sql = format!(
            "CREATE TABLE \"{}\"({},PRIMARY KEY({}))WITHOUT ROWID",
            String::from_utf8_lossy(name),
            quoted.join(","),
            quoted.join(",")
        );
        // A handle of its own, so the imposter's layout does not stand on the
        // index's - two declarations over one tree, and the planner reads a
        // different one for each.
        let root = self.allocate_root()?;
        let mut info = table_from_create_sql(sql.as_bytes(), 0, root)?;
        info.without_rowid = true;
        let layout = SourceLayout {
            tree_key: root,
            slots: (0..columns.len()).map(Some).collect(),
            // There is no rowid: the entry's own columns are the whole row.
            rowid: None,
            identity: (0..columns.len()).collect(),
            types: (0..columns.len())
                .map(|_| inillucent_exec::StaticType::Unknown)
                .collect(),
            width: columns.len(),
            // Read in the tree's order, which is the order the index is in -
            // that is the whole reason for looking at one this way.
            key_columns: (0..columns.len()).collect(),
        };
        self.schema
            .imposters
            .retain(|(held, _, _)| held.name != info.name);
        self.schema.imposters.push((info, layout, tree));
        self.refresh_catalog();
        Ok(Some(format!("{sql};")))
    }
    /// Puts the imposter declarations back after a catalog rebuild.
    pub(crate) fn republish_imposters(&mut self) {
        let held = self.schema.imposters.clone();
        for (info, layout, tree) in held {
            self.schema
                .layouts
                .insert(layout.tree_key, std::rc::Rc::new(layout));
            self.schema.trees.insert(info.root, tree);
            self.schema
                .tables
                .retain(|table| table.folded != info.folded);
            self.schema.tables.push(info);
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
    pub(crate) fn refresh_statistics(&mut self) -> DbResult<()> {
        let stale: Vec<(i64, SchemaEntry)> = self
            .schema
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
        if stale.is_empty() {
            return Ok(());
        }
        for (rowid, entry) in stale {
            self.rewrite(rowid, entry)?;
        }
        // **Sealed here, because nothing else will.** `rewrite` logs under
        // `current_txn()`, which outside a batch and outside a running
        // statement is `next_txn` read but not advanced - `current_txn`'s own
        // doc comment says a fresh one there "is then committed by
        // `ImportedDatabase::seal` at the end of the statement". A checkpoint
        // is not a statement, so nothing called it: the rewrite's records sat
        // in the log under a transaction number nobody ever committed, and
        // `should_replay` never replays an uncommitted transaction's record.
        // A crash mid-writeback of the page that landed on had no redo behind
        // it at all - the same shape of gap `log_free_map_pages` closes for
        // the free map's own pages, reached here because a stale row is
        // rewritten on every checkpoint whose catalog root is small enough
        // that the rewrite lands on the same page a torn write can still hit.
        self.seal()
    }
    /// Replaces one catalog row in place, by rowid.
    ///
    /// A delete and an insert rather than an update: four of the five columns
    /// are variable-length text, and the in-place path is for fixed-width slots.
    ///
    /// @param rowid - the row's key
    /// @param entry - what it should now say
    pub(crate) fn rewrite(&mut self, rowid: i64, entry: SchemaEntry) -> DbResult<()> {
        {
            let (at, txn, _open, wal, uncommitted) = self.catalog_write()?;
            let mut log = WalLog {
                wal,
                txn,
                schema: at,
                wrote: false,
                // See `record` above: the before-image is kept whether or not
                // an explicit transaction is open (task-1932, H3).
                undo: Some(&self.writing.undo),
                uncommitted,
            };
            let catalog_handle = self.catalog_handle_of(at);
            let tree = self
                .schema
                .trees
                .get_mut(&catalog_handle)
                .ok_or_else(|| refusal("the catalog tree is not attached"))?;
            let session = self.session_state.session.get();
            let database = crate::file_of(
                &mut self.storage.database,
                &mut self.session_state.attached,
                &mut self.session_state.temps,
                session,
                at,
            )?;
            delete_entry(database, tree, &mut log, rowid)?;
            insert_entry(database, tree, &mut log, rowid, &entry)?;
        }
        let at = self.schema.ddl_schema;
        for held in self.entries_of_mut(at).into_iter().flatten() {
            if held.rowid == rowid {
                held.entry = entry.clone();
            }
        }
        Ok(())
    }
    /// Removes one catalog row, by rowid.
    ///
    /// @param rowid - the row's key
    pub(crate) fn forget(&mut self, rowid: i64) -> DbResult<()> {
        {
            let (at, txn, _open, wal, uncommitted) = self.catalog_write()?;
            let mut log = WalLog {
                wal,
                txn,
                schema: at,
                wrote: false,
                // See `record` above: the before-image is kept whether or not
                // an explicit transaction is open (task-1932, H3).
                undo: Some(&self.writing.undo),
                uncommitted,
            };
            let catalog_handle = self.catalog_handle_of(at);
            let tree = self
                .schema
                .trees
                .get_mut(&catalog_handle)
                .ok_or_else(|| refusal("the catalog tree is not attached"))?;
            let session = self.session_state.session.get();
            let database = crate::file_of(
                &mut self.storage.database,
                &mut self.session_state.attached,
                &mut self.session_state.temps,
                session,
                at,
            )?;
            delete_entry(database, tree, &mut log, rowid)?;
        }
        let at = self.schema.ddl_schema;
        if let Some(held) = self.entries_of_mut(at) {
            held.retain(|row| row.rowid != rowid);
        }
        Ok(())
    }
    /// Returns the transaction a schema change joins.
    ///
    /// Inside a batch it is the batch's; outside one it is the number the
    /// statement in flight already took, and a fresh one when no statement is
    /// in flight - which is then committed by [`ImportedDatabase::seal`] at the
    /// end of the statement.
    ///
    /// **The middle case is the one that was missing.**
    /// [`ImportedDatabase::write`] reads `next_txn` and moves it on at once, so
    /// asking `next_txn` from inside a running statement names the transaction
    /// *after* the one about to commit. Everything logged under that number is
    /// written and never committed. See `statement_txn` for what that cost.
    pub(crate) fn current_txn(&self) -> u64 {
        match self.writing.batch.get() {
            Some(held) => held,
            None => self
                .writing
                .statement_txn
                .get()
                .unwrap_or_else(|| self.writing.next_txn.get()),
        }
    }
    /// Commits a schema change that was its own transaction.
    ///
    /// Inside a batch this does nothing: the batch's `COMMIT` is what makes the
    /// change durable, which is the whole difference between the two groupings.
    pub(crate) fn seal(&mut self) -> DbResult<()> {
        if self.writing.batch.get().is_some() {
            return Ok(());
        }
        let txn = self.writing.next_txn.get();
        self.writing.next_txn.set(txn.saturating_add(1));
        let at = self.schema.ddl_schema;
        let wal = self
            .log_of(at)
            .ok_or_else(|| refusal("a statement names a database that is not attached"))?;
        wal.append(
            txn,
            inillucent_wal::record::Body::CatalogChange { delta: &[] },
        )?;
        // **Through the same commit as every other statement's.** A schema
        // change is a write, and it takes the participant set the write left
        // behind - which is one file for every `CREATE TABLE` there has ever
        // been, and therefore the single-file path. Committing the log directly
        // here instead would leave that set uncleared, and the *next* autocommit
        // statement would find a schema in it that it had not written: a
        // one-file insert paying for a two-file protocol, and a `Commit` record
        // in a log for a transaction that never touched it.
        let participants = self.writing.touched.replace(0) | crate::schema_bit(at);
        self.commit_across(txn, participants)
    }
}
