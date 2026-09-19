//! `ALTER TABLE`, `DROP`, and rebuilding a table the shape of which changed.
//!
//! Invariant: **a rebuild copies every row through the new layout.** An
//! `ALTER TABLE ... ADD COLUMN` with a default has to reach the rows that were
//! already there, so the cheap path - change the catalog and leave the tree -
//! is only taken where the stored bytes genuinely do not move.

use inillucent_base::error::refusal;
use inillucent_base::DbResult;
use inillucent_catalog::ddl::canonical_sql;
use inillucent_catalog::paged::{tables_from_entries, ObjectKind, SchemaEntry};
use inillucent_catalog::rename;
use inillucent_pool::PageId;
use inillucent_sql::catalog_view::TableInfo;
use inillucent_sql::directive::AlterKind;
use inillucent_tree::datum::{Datum, OwnedDatum};
use inillucent_tree::PagedTree;

use super::*;
use crate::*;

/// What a `CREATE` found where it is about to write.
///
/// **An enum rather than two adjacent booleans (task-1962, A9).**
/// `create_bodiless` took `exists: bool, if_not_exists: bool` positional and
/// next to each other; a call site that gave them the other way round would
/// refuse a statement that said `IF NOT EXISTS` and accept one that did not,
/// and the compiler would say nothing. The pair also has three meanings rather
/// than four - "nothing is there" does not care what the statement said.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Already {
    /// Nothing of that name is there, so the object is created.
    Absent,
    /// Something is there and the statement said `IF NOT EXISTS`, so this is a
    /// statement that does nothing.
    Allowed,
    /// Something is there and nothing said to allow it, so this is an error.
    Refused,
}

impl Already {
    /// Reads the pair of booleans a directive carries.
    ///
    /// @param exists - whether one of that name is already there
    /// @param if_not_exists - whether the statement said `IF NOT EXISTS`
    pub(crate) fn of(exists: bool, if_not_exists: bool) -> Already {
        match (exists, if_not_exists) {
            (false, _) => Already::Absent,
            (true, true) => Already::Allowed,
            (true, false) => Already::Refused,
        }
    }
}

impl crate::ImportedDatabase {
    /// Records a view or a trigger, which have text and no tree.
    ///
    /// @param keywords - the prefix the stored text carries
    /// @param kind - which of the two
    /// @param source - the statement text
    /// @param name_offset - where the object's name starts in it
    /// @param name - the object's name
    /// @param table - the table it belongs to, its own name for a view
    /// @param already - what is there of that name, and whether it is allowed
    pub(crate) fn create_bodiless(
        &mut self,
        keywords: &str,
        kind: ObjectKind,
        source: &[u8],
        name_offset: u32,
        name: &[u8],
        table: &[u8],
        already: Already,
    ) -> DbResult<Outcome> {
        match already {
            Already::Absent => {}
            Already::Allowed => return Ok(Outcome::empty()),
            Already::Refused => {
                return Err(refusal(format!(
                    "{} {} already exists",
                    match kind {
                        ObjectKind::View => "view",
                        _ => "trigger",
                    },
                    String::from_utf8_lossy(name)
                )))
            }
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
    pub(crate) fn drop_object(
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
            return Err(refusal(format!(
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
            Ast::Table => self.drop_table(name, &folded)?,
            Ast::Index => self.drop_index(name, &folded)?,
            Ast::View | Ast::Trigger => {
                let wanted = if kind == Ast::View {
                    ObjectKind::View
                } else {
                    ObjectKind::Trigger
                };
                let rowids: Vec<i64> = self
                    .entries_of(self.schema.ddl_schema)
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
    /// Removes a table, its indexes, its triggers and every tree they held.
    ///
    /// Its own function because `drop_object` has a recorded length in
    /// `crates/inillucent-compat/tests/policy.rs`, and the three kinds it
    /// handles have nothing in common but the name they were given.
    ///
    /// @param name - the table's name as written
    /// @param folded - the same name, folded, which the catalog is searched by
    fn drop_table(&mut self, name: &[u8], folded: &[u8]) -> DbResult<()> {
        let at = self.schema.ddl_schema;
        let position = self
            .schema
            .tables
            .iter()
            .position(|held| held.database == at && held.folded == folded)
            .ok_or_else(|| refusal(format!("no such table: {}", String::from_utf8_lossy(name))))?;
        let owner = self
            .schema
            .tables
            .get(position)
            .cloned()
            .ok_or_else(|| refusal("the table that was just found is gone"))?;
        // A virtual table's storage is its shadow tables, which are
        // named after it and are not reachable from any row that names
        // it - see `drop_module_table`.
        if owner.module.is_some() {
            self.drop_module_table(name)?;
            self.rebuild_tables()?;
            self.refresh_catalog();
            self.refresh_vector_indexes();
            // `drop_object`'s own tail rebuilds and seals for every kind,
            // so this returns to it rather than repeating it.
            return Ok(());
        }
        // Every row that names the table: the table, its indexes and its
        // triggers. Collected before anything is removed, because the
        // list is what decides what to remove.
        let doomed: Vec<i64> = self
            .entries_of(at)
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
        // The high-water mark goes with the table, so a table dropped
        // and recreated starts from one again - which is SQLite's
        // behaviour and the reason the mark is a row rather than a
        // header field.
        if owner.autoincrement {
            self.forget_sequence(&owner.name)?;
        }
        let _ = position;
        Ok(())
    }

    /// Removes an index and the tree it held.
    ///
    /// @param name - the index's name as written
    /// @param folded - the same name, folded, which the catalog is searched by
    fn drop_index(&mut self, name: &[u8], folded: &[u8]) -> DbResult<()> {
        let owner = self.schema.ddl_schema;
        let found = self
            .schema
            .tables
            .iter()
            .enumerate()
            .find_map(|(at, table)| {
                if table.database != owner {
                    return None;
                }
                table
                    .indexes
                    .iter()
                    .position(|index| index.folded == folded)
                    .map(|which| (at, which, table.root))
            });
        let Some((table_at, index_at, table_root)) = found else {
            return Err(refusal(format!(
                "no such index: {}",
                String::from_utf8_lossy(name)
            )));
        };
        // **A vector index is dropped by dropping the store that holds
        // it (task-1979, R6).** `CREATE INDEX v ON t USING
        // inillucent_hnsw (c)` records a virtual table, not an index
        // row, so the loop below found nothing to forget and the
        // `release_tree` underneath it was handed the zero root a module
        // owned index carries: `DROP INDEX v` reported success, removed
        // nothing, and the planner went on choosing the index for every
        // query - which then failed, because the module had been told
        // the statement dropped it.
        let module = self
            .schema
            .tables
            .get(table_at)
            .and_then(|table| table.indexes.get(index_at))
            .is_some_and(|index| index.origin == inillucent_sql::catalog_view::IndexOrigin::Module);
        if module {
            self.drop_module_table(name)?;
            self.rebuild_tables()?;
            self.refresh_catalog();
            self.refresh_vector_indexes();
            self.sort_covering(table_root);
            return Ok(());
        }
        let index_root = self
            .schema
            .tables
            .get(table_at)
            .and_then(|table| table.indexes.get(index_at))
            .map(|index| index.root)
            .ok_or_else(|| refusal("the index that was just found is gone"))?;
        let rowids: Vec<i64> = self
            .entries_of(self.schema.ddl_schema)
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
        Ok(())
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
    pub(crate) fn alter_table(
        &mut self,
        source: &[u8],
        table: &[u8],
        action: &AlterKind,
    ) -> DbResult<Outcome> {
        let folded = table.to_ascii_lowercase();
        let at = self.schema.ddl_schema;
        if !self
            .schema
            .tables
            .iter()
            .any(|held| held.database == at && held.folded == folded)
        {
            return Err(refusal(format!(
                "no such table: {}",
                String::from_utf8_lossy(table)
            )));
        }
        if let AlterKind::AddColumn { risk, .. } = action {
            if let Some(message) = risk.refusal() {
                if self.table_has_a_row(&folded)? {
                    return Err(refusal(message));
                }
            }
        }
        let mut updates: Vec<(i64, SchemaEntry)> = Vec::new();
        for held in self.entries_of(at) {
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
                        if !reads.contains(&folded) {
                            continue;
                        }
                        if reads.len() > 1 {
                            return Err(refusal(format!(
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
                AlterKind::AddColumn { start, end, .. } => {
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
    pub(crate) fn rebuild_tables(&mut self) -> DbResult<()> {
        // **Every schema, each numbered as the binder numbers it.** A table's
        // `database` is what an unqualified name is resolved through and what a
        // qualified one is matched against, so a table derived under the wrong
        // number is a table the wrong statement finds.
        // **Every schema every session shares, and no session's own.** A
        // temporary table belongs to one connection, so it is derived per
        // session by `session_catalog` rather than kept here where another
        // connection would find it.
        let mut rebuilt: Vec<TableInfo> = Vec::new();
        for at in self.schema_numbers() {
            let held = self.entries_of(at);
            let entries: Vec<inillucent_catalog::paged::SchemaEntry> =
                held.iter().map(|row| row.entry.clone()).collect();
            let roots: Vec<u32> = held.iter().map(|row| row.root).collect();
            rebuilt.extend(tables_from_entries(&entries, &roots, at)?);
        }
        // **A temporary trigger fires on whatever the name finds.** Its row
        // lives in the temporary database and its table usually does not -
        // `CREATE TEMP TRIGGER t_log AFTER INSERT ON t` is a trigger on a
        // permanent table - so `tables_from_entries` leaves it unattached, and
        // this is where it is put where it belongs. Newest first, which is
        // SQLite's own order.
        let orphans: Vec<(Vec<u8>, Vec<u8>)> = self
            .entries_of(crate::TEMP)
            .iter()
            .filter(|row| row.entry.kind == ObjectKind::Trigger)
            .map(|row| (row.entry.table.to_ascii_lowercase(), row.entry.sql.clone()))
            .filter(|(folded, _)| {
                !rebuilt
                    .iter()
                    .any(|table| table.database == crate::TEMP && table.folded == *folded)
            })
            .collect();
        for (folded, sql) in orphans {
            let Ok(trigger) = inillucent_catalog::load::trigger_from_create_sql(&sql) else {
                continue;
            };
            if let Some(table) = rebuilt.iter_mut().find(|table| table.folded == folded) {
                table.triggers.insert(0, trigger);
            }
        }
        // **A virtual table's columns come from its module, not its text.**
        // `CREATE VIRTUAL TABLE documents USING fts5(title, body)` names a
        // module and its arguments; what the *columns* are is the module's
        // answer, and only a connected module can give it. Deriving them from
        // the statement would be a second implementation of every module's
        // argument grammar, agreeing with the module until the day it did not.
        for table in &mut rebuilt {
            let Some(connected) = self.session_state.virtual_tables.get(&table.folded) else {
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
        self.schema.tables = rebuilt;
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
    /// Returns whether a table holds at least one row.
    ///
    /// `ADD COLUMN` is the only caller: three of the five things it may not add
    /// are only unaddable because an existing row would have no value for them,
    /// so an empty table takes all three and SQLite accepts them. It stops at
    /// the first row rather than counting, because the question is existence.
    ///
    /// @param folded - the table's folded name
    fn table_has_a_row(&mut self, folded: &[u8]) -> DbResult<bool> {
        let Some(root) = self
            .schema
            .tables
            .iter()
            .find(|table| table.folded == folded)
            .map(|table| table.root)
        else {
            return Ok(false);
        };
        let Some(tree) = self.schema.trees.get(&root) else {
            return Ok(false);
        };
        Ok(!tree.rows(self.pool_of(root)?)?.is_empty())
    }
    fn rebuild_table_tree(&mut self, folded: &[u8]) -> DbResult<()> {
        let Some(info) = self
            .schema
            .tables
            .iter()
            .find(|table| table.folded == folded)
            .cloned()
        else {
            return Ok(());
        };
        let old_root = info.root;
        let old_layout = self
            .schema
            .layouts
            .get(&old_root)
            .cloned()
            .ok_or_else(|| refusal("no layout for the table being rebuilt"))?;
        let old_rows = {
            let tree = self
                .schema
                .trees
                .get(&old_root)
                .ok_or_else(|| refusal("no tree for the table being rebuilt"))?;
            tree.rows(self.pool_of(old_root)?)?
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
            // **Nothing to fill, so nothing to evaluate (task-1932, H3).**
            // This ran whether or not the table had a row, and
            // `constant_default` evaluates the default by running
            // `SELECT <the default text>` through the ordinary execute path -
            // so `ALTER TABLE t ADD COLUMN b INTEGER DEFAULT
            // (no_such_function())` on an empty table failed here, three
            // writes after the catalog already said the column was there, and
            // `PRAGMA table_info(t)` then listed a column the tree had no slot
            // for.
            //
            // An empty table is the only way to reach it:
            // `AddedColumnRisk::refusal` refuses a default that is not a
            // literal, and `alter_table` applies that refusal only when
            // `table_has_a_row`. So on a populated table the statement never
            // gets here, and on an empty one there is no row to give a value
            // to. SQLite behaves the same way - it accepts the `ALTER`,
            // records `DEFAULT (no_such_function())` in the schema text, and
            // reports `unknown function` at the first `INSERT` that needs the
            // value - so skipping the evaluation is what matches the reference
            // rather than merely what avoids the failure.
            if old_rows.is_empty() {
                continue;
            }
            let Some(default) = info
                .columns
                .get(declared)
                .and_then(|column| column.default_sql.clone())
            else {
                continue;
            };
            // **The new column's affinity applies to its default (task-1979,
            // F4).** `ADD COLUMN c INTEGER DEFAULT '5'` filled every existing
            // row with the *text* `'5'` while an `INSERT` after it stored the
            // integer 5, so one column of one table held two storage classes
            // and `typeof(c)` answered differently per row. SQLite applies the
            // column's affinity to a default wherever it is used, which is what
            // makes the two halves agree.
            let value = self.constant_default(&default)?;
            let value = with_column_affinity(
                value,
                info.columns.get(declared).map(|column| column.affinity),
            );
            if let Some(cell) = from.get_mut(*slot) {
                *cell = Fill::Constant(value);
            }
        }
        if let (Some(new_rowid), Some(old_rowid)) = (layout.rowid, old_layout.rowid) {
            if let Some(cell) = from.get_mut(new_rowid) {
                *cell = Fill::From(old_rowid);
            }
        }
        let rows = rows_in_the_new_shape(&old_rows, &from);
        let rows = in_key_order(rows, &columns, key_columns);
        // The rebuild holds owned rows, so it does its own borrow. It runs once
        // per `ALTER TABLE` and is not on any measured path, which is exactly
        // why the cost belongs here rather than inside the builder every caller
        // shares.
        let borrowed: Vec<Vec<Datum<'_>>> = rows
            .iter()
            .map(|row| row.iter().map(OwnedDatum::borrow).collect())
            .collect();
        self.release_tree(old_root)?;
        let covering: Vec<u32> = self
            .schema
            .covering
            .get(&old_root)
            .cloned()
            .unwrap_or_default();
        self.build_tree_from(old_root, columns, key_columns, layout, &borrowed)?;
        if !covering.is_empty() {
            self.schema.covering.insert(old_root, covering);
        }
        // The catalog row's `rootpage` moved with the tree.
        let page = self
            .schema
            .trees
            .get(&old_root)
            .map(PagedTree::root)
            .unwrap_or(PageId::NONE);
        let update = self
            .schema
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
}

/// Applies a column's affinity to the value its `DEFAULT` evaluates to.
///
/// **The same conversion an `INSERT` into that column would do (task-1979,
/// F4).** `ADD COLUMN c INTEGER DEFAULT '5'` filled every existing row with the
/// *text* `'5'` while an insert after it stored the integer 5, so one column of
/// one table held two storage classes and `typeof(c)` answered differently per
/// row. SQLite applies the column's affinity to a default wherever it is used,
/// which is what makes the two halves agree.
///
/// @param value - what the default evaluated to
/// @param affinity - the new column's affinity, when it has one
fn with_column_affinity(
    value: OwnedDatum,
    affinity: Option<inillucent_value::Affinity>,
) -> OwnedDatum {
    let Some(affinity) = affinity else {
        return value;
    };
    let held = inillucent_value::Value::from(&value);
    match inillucent_value::affinity::apply_affinity(
        held,
        affinity,
        inillucent_value::TextEncoding::Utf8,
    ) {
        Ok(applied) => OwnedDatum::from(applied),
        Err(_) => value,
    }
}

/// Returns every old row rewritten into the new column order.
///
/// One pass per row over the plan `rebuild_table_tree` built: a column that was
/// there comes from its old position, a column that was added takes its
/// default, and a column the new shape has and nothing fills is NULL.
///
/// @param old_rows - the table's rows as it was declared before
/// @param from - where each column of the new shape gets its value
fn rows_in_the_new_shape(old_rows: &[Vec<OwnedDatum>], from: &[Fill]) -> Vec<Vec<OwnedDatum>> {
    old_rows
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
        .collect()
}
