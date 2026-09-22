//! `CREATE TABLE`, and the sequence an `AUTOINCREMENT` column needs.
//!
//! Invariant: **`AUTOINCREMENT` is a second table.** SQLite keeps the high
//! water mark in `sqlite_sequence` rather than deriving it from the rows, so a
//! rowid is never reused after its row is deleted, and this engine keeps the
//! same table for the same reason.

use inillucent_base::error::refusal;
use inillucent_base::DbResult;
use inillucent_catalog::ddl::canonical_sql;
use inillucent_catalog::load::table_from_create_sql;
use inillucent_catalog::paged::{ObjectKind, SchemaEntry};
use inillucent_sql::catalog_view::IndexInfo;
use inillucent_tree::datum::Datum;

use crate::*;

impl crate::ImportedDatabase {
    /// Creates a table, its tree, and the trees its constraints imply.
    ///
    /// @param source - the statement text
    /// @param name_offset - where the table's name starts in it
    /// @param name - the table's name as written
    /// @param exists - whether a table of that name is already there
    /// @param if_not_exists - whether the statement said so
    pub(crate) fn create_table(
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
            return Err(refusal(format!(
                "table {} already exists",
                String::from_utf8_lossy(name)
            )));
        }
        let sql = canonical_sql("CREATE TABLE", source, name_offset, source.len() as u32);
        self.define_table(name, sql)?;
        self.refresh_catalog();
        self.declarations_resolve(name)?;
        // **`sqlite_sequence` comes into being with the first `AUTOINCREMENT`
        // table**, not with the first row - SQLite writes the schema row at
        // `CREATE TABLE` time and the table's own row at its first insert. The
        // binder resolves `BoundInsert::sequence_root` from the catalog, so the
        // table has to be there before any statement against the new table is
        // compiled.
        if self.table_is_autoincrement(name) {
            self.ensure_sequence_table()?;
        }
        self.seal()?;
        Ok(Outcome::empty())
    }
    /// Refuses a table whose `CHECK` or generated column names nothing.
    ///
    /// **`CREATE TABLE` used to accept a function no build has (task-1979,
    /// R11).** `CHECK (unknownfn(x))` and `y AS (unknownfn(x))` were stored as
    /// text and resolved only when a row was written or read, so the statement
    /// that could have said "no such function: unknownfn" reported success and
    /// the table was unusable from its first insert. SQLite refuses both at
    /// `CREATE TABLE`, naming the function.
    ///
    /// The declarations are resolved by binding two probes rather than by
    /// walking them here: binding is what resolves a function, and a second
    /// resolver would agree with the binder until the day it did not. A failure
    /// leaves nothing behind, because `execute_ddl` undoes the statement.
    ///
    /// @param name - the table's name as written
    fn declarations_resolve(&mut self, name: &[u8]) -> DbResult<()> {
        let written = String::from_utf8_lossy(name).replace('"', "\"\"");
        // The read resolves every generated column's expression, and the write
        // resolves every `CHECK`. Neither runs.
        names_something(self.bind(&format!("SELECT * FROM \"{written}\"")))?;
        names_something(self.bind(&format!("INSERT INTO \"{written}\" DEFAULT VALUES")))?;
        Ok(())
    }

    /// Reports whether a table just defined never reuses a key.
    ///
    /// @param name - the table's name as written
    fn table_is_autoincrement(&self, name: &[u8]) -> bool {
        let folded = name.to_ascii_lowercase();
        self.schema
            .tables
            .iter()
            .any(|held| held.folded == folded && held.autoincrement)
    }
    /// Creates `sqlite_sequence` if the schema has not got one.
    ///
    /// The same shape `ANALYZE` uses for `sqlite_stat1`: a reserved-prefix table
    /// cannot go through the statement path, because `sqlite_` is a name the
    /// binder refuses, and a schema-writing statement that has to defeat the
    /// binder's own rule to run is a rule with a hole in it.
    fn ensure_sequence_table(&mut self) -> DbResult<()> {
        let folded = inillucent_exec::sequence::SEQUENCE_TABLE.to_ascii_lowercase();
        if self.schema.tables.iter().any(|held| held.folded == folded) {
            return Ok(());
        }
        self.define_table(
            inillucent_exec::sequence::SEQUENCE_TABLE,
            inillucent_exec::sequence::SEQUENCE_SQL.as_bytes().to_vec(),
        )?;
        self.refresh_catalog();
        Ok(())
    }
    /// Removes a table's `sqlite_sequence` row, which is what `DROP` does to it.
    ///
    /// @param name - the dropped table's name, as `sqlite_sequence` stores it
    pub(crate) fn forget_sequence(&mut self, name: &[u8]) -> DbResult<()> {
        let folded = inillucent_exec::sequence::SEQUENCE_TABLE.to_ascii_lowercase();
        let Some(root) = self
            .schema
            .tables
            .iter()
            .find(|held| held.folded == folded)
            .map(|held| held.root)
        else {
            return Ok(());
        };
        let doomed: Vec<i64> = {
            let pool = self.pool_of(root)?;
            let Some(tree) = self.schema.trees.get(&root) else {
                return Ok(());
            };
            let mut keys = Vec::new();
            tree.visit_leaves(pool, &mut |leaf| {
                for row in leaf.live()? {
                    let Some(Datum::Int(rowid)) = row.first().copied() else {
                        continue;
                    };
                    if matches!(row.get(1), Some(Datum::Text(held)) if *held == name) {
                        keys.push(rowid);
                    }
                }
                Ok(true)
            })?;
            keys
        };
        if doomed.is_empty() {
            return Ok(());
        }
        let txn = self.current_txn();
        let at = self.session_state.schema_of(root);
        let wal = self
            .log_of(at)
            .ok_or_else(|| refusal("a statement names a database that is not attached"))?;
        let mut log = WalLog {
            wal,
            txn,
            schema: at,
            wrote: false,
            undo: None,
            uncommitted: self.uncommitted_handle_of(at),
        };
        let Some(tree) = self.schema.trees.get_mut(&root) else {
            return Ok(());
        };
        for rowid in doomed {
            tree.delete(&mut self.storage.database, &mut log, &[Datum::Int(rowid)])?;
        }
        Ok(())
    }
    /// Creates a table from a query's shape and fills it from the query.
    ///
    /// **Two statements, in one transaction.** The `CREATE` half stores the text
    /// the binder synthesised from the query's result columns; the fill half is
    /// an ordinary `INSERT INTO name <select>`, compiled against the schema once
    /// the table is in it. Writing the insert here instead would be a second
    /// implementation of what an insert means - and would miss everything the
    /// write path applies, from column affinity to the `NOT NULL` a declared
    /// type carried over.
    ///
    /// @param name - the table's name as written
    /// @param exists - whether a table of that name is already there
    /// @param if_not_exists - whether the statement said so
    /// @param create_sql - the `CREATE TABLE name(...)` text to store
    /// @param select_sql - the query, as the source text it was written as
    pub(crate) fn create_table_as_select(
        &mut self,
        name: &[u8],
        exists: bool,
        if_not_exists: bool,
        create_sql: Vec<u8>,
        select_sql: &[u8],
    ) -> DbResult<Outcome> {
        if exists {
            if if_not_exists {
                return Ok(Outcome::empty());
            }
            return Err(refusal(format!(
                "table {} already exists",
                String::from_utf8_lossy(name)
            )));
        }
        self.define_table(name, create_sql)?;
        self.refresh_catalog();
        let fill = format!(
            "INSERT INTO \"{}\" {}",
            String::from_utf8_lossy(name).replace('"', "\"\""),
            String::from_utf8_lossy(select_sql)
        );
        let outcome = self.execute_any(&fill, &inillucent_exec::physical::Params::new())?;
        self.seal()?;
        Ok(Outcome {
            rows: Vec::new(),
            names: std::rc::Rc::new(Vec::new()),
            changes: outcome.changes,
        })
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
    pub(crate) fn define_table(&mut self, name: &[u8], sql: Vec<u8>) -> DbResult<u32> {
        let root = self.allocate_root()?;
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
            let index_root = self.allocate_root()?;
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
            self.schema
                .covering
                .entry(root)
                .or_default()
                .push(index_root);
            if let Some(slot) = info.indexes.get_mut(position) {
                slot.root = index_root;
            }
        }
        self.rebuild_tables()?;
        Ok(root)
    }
}

/// Returns the refusal only when it is about a name nothing has.
///
/// **The probes bind an ordinary statement, and a schema is not one.** A
/// `CHECK` is bound as schema text, where `PRAGMA trusted_schema` decides
/// whether a function that is not innocuous may be named at all, and a probe
/// bound as an ordinary `INSERT` does not carry that site - so a table whose
/// `CHECK` calls a function the schema policy allows was refused by the probe
/// and not by the engine. What `declarations_resolve` is for is the one
/// failure SQLite reports at `CREATE TABLE` (task-1979, R11): a function no
/// build has. Everything else the probe happens to notice is left to the
/// statement that actually runs.
///
/// @param bound - what binding the probe answered
fn names_something<T>(bound: DbResult<T>) -> DbResult<()> {
    match bound {
        Ok(_) => Ok(()),
        Err(error) if error.message().starts_with("no such function") => Err(error),
        Err(_) => Ok(()),
    }
}
