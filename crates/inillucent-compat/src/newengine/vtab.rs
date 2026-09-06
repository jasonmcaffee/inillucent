//! Virtual tables on the new engine: `CREATE VIRTUAL TABLE`, the shadow store,
//! and driving a module's cursor.
//!
//! Invariant: **a module's shadow tables are ordinary trees.** FTS5 declares its
//! storage as five `CREATE TABLE` statements and the R-Tree as three, so
//! creating a virtual table is creating those tables the way any other
//! `CREATE TABLE` is created - through `define_table`, into the catalog, with
//! their rows in `sqlite_schema` exactly as SQLite records them. That is the
//! TDD's "shadow tables become ordinary rowid trees", and it is why the port is
//! a different *store* rather than a different module: the tokenizers, the
//! ranking, the segment merges and the R-Tree's node logic are not touched.
//!
//! ## Why there are two stores rather than one
//!
//! A read runs under `&self` - `TreeCatalog::virtual_rows` is called from a
//! pipeline that is already holding the catalog - and a write runs under
//! `&mut self`. Rust will not let one type be both, and pretending otherwise
//! with interior mutability would put a `RefCell` on the read path of every
//! query to serve the one statement that writes. So there are two, and the
//! reading one refuses a write by name: a module that tried to write while
//! answering a `SELECT` would be doing something the engine above it did not
//! ask for.
//!
//! ## What a module is not given
//!
//! It is handed the roots of its own shadow tables and nothing else. There is no
//! catalog behind `Context` here and no pager: [`Nowhere`] is what a module gets
//! if it reaches past the store, and it says so rather than answering.

use std::collections::HashMap;

use inillucent_base::error::misuse;
use inillucent_base::DbResult;
use inillucent_catalog::paged::{ObjectKind, SchemaEntry};
use inillucent_ext::vtab::{Context, Host, VirtualTable};
use inillucent_pool::{Database, Pool};
use inillucent_sql::plan::AccessPath;
use inillucent_sql::vtab::{
    Change, FilterPlan, IndexQuery, ModuleArguments, ShadowRoot, ShadowStore,
};
use inillucent_tree::datum::{Datum, OwnedDatum};
use inillucent_tree::write::TreeLog;
use inillucent_tree::PagedTree;
use inillucent_value::Value;

use super::{ImportedDatabase, Outcome, WalLog};

/// A connected virtual table and the shadow roots it was given.
pub struct Connected {
    /// The module's own object.
    pub table: Box<dyn VirtualTable>,
    /// What it was connected with, so a write can hand it back.
    pub arguments: ModuleArguments,
}

/// A `Host` that answers nothing, because there is nothing behind it.
///
/// The old engine's `Host` reaches a pager, and the new engine has none - its
/// pages are behind a buffer pool and its rows behind a tree. Every module in
/// the workspace reaches its storage through `ShadowTables`, which the store
/// answers; this exists because `Context` needs a host and says so rather than
/// pretending to be one.
pub struct Nowhere;

impl inillucent_storage::PagerSet for Nowhere {
    fn pager(&mut self, _database: usize) -> DbResult<&mut inillucent_storage::pager::Pager> {
        Err(misuse(
            "this engine has no pager; a module reads its shadow tables through the store",
        ))
    }

    fn count(&self) -> usize {
        1
    }
}

impl Host for Nowhere {}

/// The shadow tables of one database, for reading.
pub struct ReadStore<'a> {
    /// The pool the trees' pages live in.
    pub pool: &'a Pool,
    /// The trees, by the identifier the catalog registered them under.
    pub trees: &'a HashMap<u32, PagedTree>,
}

/// The shadow tables of one database, for reading and writing.
pub struct WriteStore<'a> {
    /// The file, for allocating pages a split needs.
    pub database: &'a mut Database,
    /// The trees, by the identifier the catalog registered them under.
    pub trees: &'a mut HashMap<u32, PagedTree>,
    /// Where the records go.
    pub log: &'a mut dyn TreeLog,
}

/// Returns a shadow row as the values a module reads.
///
/// @param row - the tree's row
fn as_values(row: &[OwnedDatum]) -> DbResult<Vec<Value<'static>>> {
    row.iter()
        .map(|value| match value {
            OwnedDatum::Null => Ok(Value::Null),
            OwnedDatum::Int(number) => Ok(Value::Integer(*number)),
            OwnedDatum::Real(number) => Ok(Value::Real(*number)),
            OwnedDatum::Text(bytes) => Value::owned_text(bytes),
            OwnedDatum::Blob(bytes) => Value::owned_blob(bytes),
        })
        .collect()
}

/// Returns a module's values as a tree row.
///
/// @param values - what the module wrote
fn as_row(values: &[Value<'static>]) -> Vec<OwnedDatum> {
    values
        .iter()
        .map(|value| match value {
            Value::Null => OwnedDatum::Null,
            Value::Integer(number) => OwnedDatum::Int(*number),
            Value::Real(number) => OwnedDatum::Real(*number),
            Value::Text(text) => OwnedDatum::Text(text.utf8_bytes().into_owned()),
            Value::Blob(blob) => OwnedDatum::Blob(blob.raw().to_vec()),
        })
        .collect()
}

/// Reads one row of a rowid shadow table.
///
/// @param pool - the buffer pool
/// @param tree - the shadow table
/// @param rowid - the row's key
fn read_rowid(pool: &Pool, tree: &PagedTree, rowid: i64) -> DbResult<Option<Vec<Value<'static>>>> {
    // **One copy of the row, not two.** `PagedTree::point` copies every column
    // into an `OwnedDatum` and the conversion then copies each of those into a
    // `Value`, so a module that reads a shadow row - which FTS5 does ten times
    // per document, several of them over segment blocks measured in kilobytes -
    // paid for the block twice. `probe` hands over the leaf's own bytes and the
    // `Value` is built from them directly.
    tree.probe(pool, &[Datum::Int(rowid)], |leaf, hit| {
        let mut values = Vec::with_capacity(leaf.column_count());
        for column in 0..leaf.column_count() {
            values.push(borrowed_value(&leaf.value_at(hit, column)?)?);
        }
        Ok(values)
    })
}

/// Returns one borrowed page value as an owned module value.
///
/// @param datum - the value read out of a leaf
fn borrowed_value(datum: &Datum<'_>) -> DbResult<Value<'static>> {
    match datum {
        Datum::Null => Ok(Value::Null),
        Datum::Int(number) => Ok(Value::Integer(*number)),
        Datum::Real(number) => Ok(Value::Real(*number)),
        Datum::Text(bytes) => Value::owned_text(bytes),
        Datum::Blob(bytes) => Value::owned_blob(bytes),
    }
}

/// Reads one row of a keyed shadow table.
///
/// @param pool - the buffer pool
/// @param tree - the shadow table
/// @param key - the leading columns that identify it
/// @param columns - how many columns to return
fn read_key(
    pool: &Pool,
    tree: &PagedTree,
    key: &[Value<'static>],
    columns: usize,
) -> DbResult<Option<Vec<Value<'static>>>> {
    let owned = as_row(key);
    let probe: Vec<Datum<'_>> = owned.iter().map(OwnedDatum::borrow).collect();
    // One copy, for the reason `read_rowid` gives.
    tree.probe(pool, &probe, |leaf, hit| {
        let wanted = columns.min(leaf.column_count());
        let mut values = Vec::with_capacity(wanted);
        for column in 0..wanted {
            values.push(borrowed_value(&leaf.value_at(hit, column)?)?);
        }
        Ok(values)
    })
}

/// Walks a shadow table's rows in key order.
///
/// @param pool - the buffer pool
/// @param tree - the shadow table
/// @param body - what to do with each row, stopping when it says so
fn walk(
    pool: &Pool,
    tree: &PagedTree,
    body: &mut dyn FnMut(&[Value<'static>]) -> DbResult<bool>,
) -> DbResult<()> {
    let mut stop = false;
    let mut failure: Option<inillucent_base::DbError> = None;
    tree.visit_leaves(pool, &mut |leaf| {
        for row in leaf.live()? {
            let owned: Vec<OwnedDatum> = row.iter().map(OwnedDatum::from_datum).collect();
            let values = match as_values(&owned) {
                Ok(values) => values,
                Err(error) => {
                    failure = Some(error);
                    return Ok(false);
                }
            };
            match body(&values) {
                Ok(true) => {}
                Ok(false) => {
                    stop = true;
                    return Ok(false);
                }
                Err(error) => {
                    failure = Some(error);
                    return Ok(false);
                }
            }
        }
        Ok(true)
    })?;
    let _ = stop;
    match failure {
        Some(error) => Err(error),
        None => Ok(()),
    }
}

/// Returns the largest rowid a shadow table holds.
///
/// @param pool - the buffer pool
/// @param tree - the shadow table
fn highest(pool: &Pool, tree: &PagedTree) -> DbResult<i64> {
    // **From the right-hand end, not by walking.** A rowid tree's largest key is
    // in its last leaf, and a module that asks for it once per insert - which is
    // what allocating a document id is - would otherwise make a bulk load
    // quadratic in the rows already there.
    let mut highest = 0i64;
    tree.visit_reverse(pool, None, &mut |leaf| {
        for row in leaf.live()? {
            if let Some(Datum::Int(rowid)) = row.first().copied() {
                highest = highest.max(rowid);
            }
        }
        // The last leaf holds the largest key, so one leaf answers it.
        Ok(false)
    })?;
    Ok(highest)
}

/// Returns the error a read-only store gives a module that tried to write.
fn read_only() -> inillucent_base::DbError {
    misuse("a module wrote to a shadow table while answering a query")
}

impl ShadowStore for ReadStore<'_> {
    fn read_row(&mut self, root: u32, rowid: i64) -> DbResult<Option<Vec<Value<'static>>>> {
        let Some(tree) = self.trees.get(&root) else {
            return Ok(None);
        };
        read_rowid(self.pool, tree, rowid)
    }

    fn write_row(&mut self, _root: u32, _rowid: i64, _values: &[Value<'static>]) -> DbResult<()> {
        Err(read_only())
    }

    fn delete_row(&mut self, _root: u32, _rowid: i64) -> DbResult<()> {
        Err(read_only())
    }

    fn max_rowid(&mut self, root: u32) -> DbResult<i64> {
        match self.trees.get(&root) {
            Some(tree) => highest(self.pool, tree),
            None => Ok(0),
        }
    }

    fn scan(
        &mut self,
        root: u32,
        body: &mut dyn FnMut(i64, &[Value<'static>]) -> DbResult<bool>,
    ) -> DbResult<()> {
        let Some(tree) = self.trees.get(&root) else {
            return Ok(());
        };
        walk(self.pool, tree, &mut |values| {
            let rowid = match values.first() {
                Some(Value::Integer(number)) => *number,
                _ => 0,
            };
            body(rowid, values)
        })
    }

    fn read_keyed(
        &mut self,
        root: u32,
        key: &[Value<'static>],
        columns: usize,
    ) -> DbResult<Option<Vec<Value<'static>>>> {
        let Some(tree) = self.trees.get(&root) else {
            return Ok(None);
        };
        read_key(self.pool, tree, key, columns)
    }

    fn write_keyed(
        &mut self,
        _root: u32,
        _key_columns: usize,
        _values: &[Value<'static>],
    ) -> DbResult<()> {
        Err(read_only())
    }

    fn delete_keyed(&mut self, _root: u32, _key: &[Value<'static>]) -> DbResult<()> {
        Err(read_only())
    }

    fn scan_keyed(
        &mut self,
        root: u32,
        _key_columns: usize,
        body: &mut dyn FnMut(&[Value<'static>]) -> DbResult<bool>,
    ) -> DbResult<()> {
        let Some(tree) = self.trees.get(&root) else {
            return Ok(());
        };
        walk(self.pool, tree, body)
    }
}

impl ShadowStore for WriteStore<'_> {
    fn read_row(&mut self, root: u32, rowid: i64) -> DbResult<Option<Vec<Value<'static>>>> {
        let Some(tree) = self.trees.get(&root) else {
            return Ok(None);
        };
        read_rowid(self.database.pool(), tree, rowid)
    }

    fn write_row(&mut self, root: u32, rowid: i64, values: &[Value<'static>]) -> DbResult<()> {
        let mut owned = as_row(values);
        // A rowid table stores its key once, as the tree's key column, and the
        // module hands the row with its rowid in the first slot - which is what
        // the shadow table declares as `INTEGER PRIMARY KEY`.
        if let Some(first) = owned.first_mut() {
            *first = OwnedDatum::Int(rowid);
        }
        let row: Vec<Datum<'_>> = owned.iter().map(OwnedDatum::borrow).collect();
        let tree = self
            .trees
            .get_mut(&root)
            .ok_or_else(|| misuse("no shadow table for that root"))?;
        // **`put`, not `insert`.** `insert` copies out the row it replaced -
        // every column of it, allocating per text and per blob - and this
        // caller throws that away. On FTS5's `%_data` the replaced row is the
        // term's whole doclist, so a shadow write was paying to materialise a
        // multi-kilobyte value nobody reads. It is the same write otherwise.
        tree.put(self.database, self.log, &row)?;
        Ok(())
    }

    fn delete_row(&mut self, root: u32, rowid: i64) -> DbResult<()> {
        let Some(tree) = self.trees.get_mut(&root) else {
            return Ok(());
        };
        tree.delete(self.database, self.log, &[Datum::Int(rowid)])?;
        Ok(())
    }

    fn max_rowid(&mut self, root: u32) -> DbResult<i64> {
        match self.trees.get(&root) {
            Some(tree) => highest(self.database.pool(), tree),
            None => Ok(0),
        }
    }

    fn scan(
        &mut self,
        root: u32,
        body: &mut dyn FnMut(i64, &[Value<'static>]) -> DbResult<bool>,
    ) -> DbResult<()> {
        let Some(tree) = self.trees.get(&root) else {
            return Ok(());
        };
        walk(self.database.pool(), tree, &mut |values| {
            let rowid = match values.first() {
                Some(Value::Integer(number)) => *number,
                _ => 0,
            };
            body(rowid, values)
        })
    }

    fn read_keyed(
        &mut self,
        root: u32,
        key: &[Value<'static>],
        columns: usize,
    ) -> DbResult<Option<Vec<Value<'static>>>> {
        let Some(tree) = self.trees.get(&root) else {
            return Ok(None);
        };
        read_key(self.database.pool(), tree, key, columns)
    }

    fn write_keyed(
        &mut self,
        root: u32,
        _key_columns: usize,
        values: &[Value<'static>],
    ) -> DbResult<()> {
        let owned = as_row(values);
        let row: Vec<Datum<'_>> = owned.iter().map(OwnedDatum::borrow).collect();
        let tree = self
            .trees
            .get_mut(&root)
            .ok_or_else(|| misuse("no shadow table for that root"))?;
        // **`put`, not `insert`.** `insert` copies out the row it replaced -
        // every column of it, allocating per text and per blob - and this
        // caller throws that away. On FTS5's `%_data` the replaced row is the
        // term's whole doclist, so a shadow write was paying to materialise a
        // multi-kilobyte value nobody reads. It is the same write otherwise.
        tree.put(self.database, self.log, &row)?;
        Ok(())
    }

    fn delete_keyed(&mut self, root: u32, key: &[Value<'static>]) -> DbResult<()> {
        let owned = as_row(key);
        let probe: Vec<Datum<'_>> = owned.iter().map(OwnedDatum::borrow).collect();
        let Some(tree) = self.trees.get_mut(&root) else {
            return Ok(());
        };
        tree.delete(self.database, self.log, &probe)?;
        Ok(())
    }

    fn scan_keyed(
        &mut self,
        root: u32,
        _key_columns: usize,
        body: &mut dyn FnMut(&[Value<'static>]) -> DbResult<bool>,
    ) -> DbResult<()> {
        let Some(tree) = self.trees.get(&root) else {
            return Ok(());
        };
        walk(self.database.pool(), tree, body)
    }
}

impl ImportedDatabase {
    /// Creates a virtual table, its shadow tables, and its catalog rows.
    ///
    /// The shadow tables are created through `define_table`, which is the same
    /// path an ordinary `CREATE TABLE` takes - so they are rowid or keyed trees
    /// like any other, they appear in `sqlite_schema` the way SQLite records
    /// them, and the integrity checker walks them.
    ///
    /// @param source - the statement text
    /// @param name_offset - where the table's name starts in it
    /// @param name - the table's name as written
    /// @param module - the module's name as written
    /// @param arguments - the arguments inside the parentheses
    /// @param exists - whether a table of that name is already there
    /// @param if_not_exists - whether the statement said so
    #[allow(clippy::too_many_arguments)]
    pub(super) fn create_virtual_table(
        &mut self,
        source: &[u8],
        name_offset: u32,
        name: &[u8],
        module: &[u8],
        arguments: &[Vec<u8>],
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
        let found = self.registry.module(module).ok_or_else(|| {
            misuse(format!(
                "no such module: {}",
                String::from_utf8_lossy(module)
            ))
        })?;
        if !found.constructible() {
            return Err(misuse(format!(
                "{} may not be used with CREATE VIRTUAL TABLE",
                String::from_utf8_lossy(module)
            )));
        }
        let mut connect = ModuleArguments {
            database: 0,
            schema: b"main".to_vec(),
            table: name.to_vec(),
            module: module.to_vec(),
            arguments: arguments.to_vec(),
            shadows: Vec::new(),
        };
        for shadow in found.shadow_tables(&connect)? {
            // `%` stands for the virtual table's own name, which is what makes
            // one declaration serve every table the module ever creates.
            let text = shadow
                .create_sql
                .replace('%', &String::from_utf8_lossy(name));
            let shadow_name = shadow_table_name(name, &shadow.suffix);
            let root = self.define_table(&shadow_name, text.into_bytes())?;
            connect.shadows.push(ShadowRoot {
                suffix: shadow.suffix.clone(),
                root,
            });
        }
        let sql = inillucent_catalog::ddl::canonical_sql(
            "CREATE VIRTUAL TABLE",
            source,
            name_offset,
            source.len() as u32,
        );
        self.record(
            0,
            SchemaEntry {
                kind: ObjectKind::Table,
                name: name.to_vec(),
                table: name.to_vec(),
                root: inillucent_pool::PageId::NONE,
                sql,
                stats: Default::default(),
            },
        )?;
        self.rebuild_tables()?;
        self.refresh_catalog();
        // The module is connected *after* its shadow tables exist, because a
        // module that writes an initial row writes it into one of them.
        let table = {
            let txn = self.current_txn();
            let mut log = WalLog {
                wal: &self.wal,
                txn,
            };
            let mut store = WriteStore {
                database: &mut self.database,
                trees: &mut self.trees,
                log: &mut log,
            };
            let mut nowhere = Nowhere;
            let mut context = Context {
                host: &mut nowhere,
                store: Some(&mut store),
                database: 0,
                limits: &self.limits,
                catalog: None,
            };
            let mut table = found.connect(&connect, true)?;
            table.begin(&mut context)?;
            table.sync(&mut context)?;
            table.commit(&mut context)?;
            table
        };
        self.virtual_tables.insert(
            name.to_ascii_lowercase(),
            Connected {
                table,
                arguments: connect,
            },
        );
        // Rebuilt again now the module is connected: its *columns* are the
        // module's answer, and until it was connected there was nobody to ask.
        self.rebuild_tables()?;
        self.refresh_catalog();
        self.seal()?;
        Ok(Outcome::empty())
    }

    /// Runs a module's cursor to completion and returns its rows.
    ///
    /// @param term - which FROM term of the plan
    /// @param path - the access path the planner chose for it
    /// @param params - the values bound to `?1`, `?2`, ...
    pub(super) fn rows_of_module(
        &self,
        table: &inillucent_sql::catalog_view::TableInfo,
        path: &AccessPath,
        params: &inillucent_exec::physical::Params,
        needed: &inillucent_sql::bind::ColumnUse,
    ) -> DbResult<Option<Vec<Vec<OwnedDatum>>>> {
        let Some(connected) = self.virtual_tables.get(&table.folded) else {
            return Ok(None);
        };
        // **The constraints are pushed down, and they have to be.** A residual
        // the engine can test itself is a choice; `documents MATCH 'lorem'` is
        // not one, because `MATCH` is the *module's* operator and the engine has
        // no way to evaluate it. A scan that answered with every row and left
        // the predicate to the pipeline returned every document rather than the
        // two that matched - which is what this did until the probe asked it.
        //
        // So the offer goes to `best_index`, the module says which constraints
        // it will use and in what argument order, and `filter` is given their
        // values. What the module did *not* take stays in the plan's residual
        // and the pipeline tests it, which is the contract's whole point: a
        // constraint is dropped from the residual only when the module promises
        // `omit`, and `omit` is the module promising rather than the engine
        // assuming.
        let AccessPath::VirtualScan {
            offer, order_by, ..
        } = path
        else {
            return Ok(None);
        };
        let specs: Vec<inillucent_sql::vtab::ConstraintSpec> =
            offer.iter().map(|held| held.spec.clone()).collect();
        let mut query = IndexQuery::new(specs, order_by.clone());
        connected.table.best_index(&mut query)?;
        let mut arguments: Vec<Value<'static>> = Vec::new();
        for position in query.argument_order() {
            let Some(constraint) = offer.get(position) else {
                continue;
            };
            arguments.push(inillucent_exec::scalar::to_value(
                inillucent_exec::physical::literal_value(&constraint.value, params)?.borrow(),
            ));
        }
        let plan = FilterPlan {
            index_number: query.index_number,
            index_string: query.index_string.clone(),
            arguments,
        };
        let mut cursor = connected.table.open()?;
        let mut nowhere = Nowhere;
        let mut store = ReadStore {
            pool: self.database.pool(),
            trees: &self.trees,
        };
        let mut context = Context {
            host: &mut nowhere,
            store: Some(&mut store),
            database: 0,
            limits: &self.limits,
            catalog: None,
        };
        let width = connected.table.declaration().columns.len();
        // **Only the columns something reads.** `needed` is what the bound
        // statement reads of this term - the same answer a covering index is
        // chosen by - plus the columns the recheck below tests, which were
        // taken out of the residual on the module's behalf and so may not be
        // read anywhere else. An opaque answer means the reads could not be
        // enumerated, and then every column is materialised.
        let mut wanted = vec![needed.opaque; width];
        for slot in &needed.columns {
            if let Some(flag) = wanted.get_mut(usize::from(*slot)) {
                *flag = true;
            }
        }
        for (position, constraint) in offer.iter().enumerate() {
            let promised = query
                .usage
                .get(position)
                .map(|usage| usage.omit)
                .unwrap_or(false);
            if promised {
                continue;
            }
            if let Ok(column) = usize::try_from(constraint.spec.column) {
                if let Some(flag) = wanted.get_mut(column) {
                    *flag = true;
                }
            }
        }
        let mut rows: Vec<Vec<OwnedDatum>> = Vec::new();
        cursor.filter(&mut context, &plan)?;
        while !cursor.eof() {
            let mut row = Vec::with_capacity(width);
            for column in 0..width {
                if !wanted.get(column).copied().unwrap_or(true) {
                    row.push(OwnedDatum::Null);
                    continue;
                }
                row.push(inillucent_exec::scalar::from_value(
                    cursor.column(&mut context, column)?,
                ));
            }
            rows.push(row);
            cursor.next(&mut context)?;
        }
        drop(context);
        // **Everything the module did not promise is tested again here.**
        //
        // The planner takes every offered predicate out of the residual on the
        // optimistic assumption that a later pass puts back the ones the module
        // did not promise to apply - which is what `VirtualChoice::recheck`
        // exists for. There is no such pass on this path, so the recheck happens
        // where the rows are: `omit` is the module promising, and anything else
        // is the engine's to test.
        //
        // It is not a tidiness point. The R-Tree takes the constraints it can
        // use to prune its own tree and leaves the rest; without this,
        // `WHERE minX > 0 AND maxX < 100000` answered with all three boxes
        // instead of the one that matches.
        for (position, constraint) in offer.iter().enumerate() {
            let promised = query
                .usage
                .get(position)
                .map(|usage| usage.omit)
                .unwrap_or(false);
            if promised {
                continue;
            }
            let wanted = inillucent_exec::physical::literal_value(&constraint.value, params)?;
            // A negative column is the rowid, which a materialised row does not
            // carry: the module's declared columns are all a row holds. Such a
            // constraint has to have been the module's to apply.
            let Ok(column) = usize::try_from(constraint.spec.column) else {
                return Err(misuse(
                    "the module did not apply a rowid constraint and the engine cannot",
                ));
            };
            let op = constraint.spec.op;
            let collation = connected.table.collation(column);
            let mut kept: Vec<Vec<OwnedDatum>> = Vec::with_capacity(rows.len());
            for row in rows {
                let Some(held) = row.get(column) else {
                    continue;
                };
                if satisfies(held, op, &wanted, collation)? {
                    kept.push(row);
                }
            }
            rows = kept;
        }
        Ok(Some(rows))
    }

    /// Applies one change to a virtual table.
    ///
    /// @param name - the table's name
    /// @param change - what to do
    pub(super) fn change_module(&mut self, name: &[u8], change: &Change) -> DbResult<Option<i64>> {
        let key = name.to_ascii_lowercase();
        let mut connected = self
            .virtual_tables
            .remove(&key)
            .ok_or_else(|| misuse(format!("no such table: {}", String::from_utf8_lossy(name))))?;
        let outcome = {
            let txn = self.current_txn();
            let mut log = WalLog {
                wal: &self.wal,
                txn,
            };
            let mut store = WriteStore {
                database: &mut self.database,
                trees: &mut self.trees,
                log: &mut log,
            };
            let mut nowhere = Nowhere;
            let mut context = Context {
                host: &mut nowhere,
                store: Some(&mut store),
                database: 0,
                limits: &self.limits,
                catalog: None,
            };
            connected.table.update(&mut context, change)
        };
        // Put it back whatever happened: a module that failed a write is still
        // the connected table, and dropping it would make the next statement
        // say the table does not exist.
        self.virtual_tables.insert(key, connected);
        outcome
    }
}

/// Returns the name one shadow table is created under.
///
/// SQLite names them `<table>_<suffix>`, and they are ordinary tables in
/// `sqlite_schema` - which is what makes a module's storage visible to the
/// integrity checker and readable by the other engine.
///
/// @param table - the virtual table's own name
/// @param suffix - the shadow table's suffix
fn shadow_table_name(table: &[u8], suffix: &[u8]) -> Vec<u8> {
    let mut name = table.to_vec();
    name.push(b'_');
    name.extend_from_slice(suffix);
    name
}

impl ImportedDatabase {
    /// Applies an `INSERT` into a virtual table by handing the row to the module.
    ///
    /// The engine evaluates the row and the module decides what to do with it,
    /// which is what makes a module a module rather than a table with a funny
    /// name. Only a `VALUES` source is taken: an `INSERT ... SELECT` into a
    /// virtual table is a pipeline feeding a module row by row, and it is
    /// refused by name rather than answered half way.
    ///
    /// @param statement - the bound insert
    /// @param params - the values bound to `?1`, `?2`, ...
    pub(super) fn insert_into_module(
        &mut self,
        statement: &inillucent_sql::dml::BoundInsert,
        params: &inillucent_exec::physical::Params,
    ) -> DbResult<Outcome> {
        let inillucent_sql::dml::BoundInsertSource::Values(values) = &statement.source else {
            return Err(misuse("an INSERT ... SELECT into a virtual table"));
        };
        let width = statement.table.columns.len();
        let mut changed = 0usize;
        for row in values {
            // The statement's own column list decides where each value lands:
            // `INSERT INTO documents(title, body)` supplies two of however many
            // the module declared, and the rest are NULL.
            let mut supplied: Vec<Value<'static>> = Vec::with_capacity(row.len());
            for expr in row {
                supplied.push(inillucent_exec::scalar::to_value(
                    inillucent_exec::physical::literal_value(expr, params)?.borrow(),
                ));
            }
            let mut cells = vec![Value::Null; width];
            for (position, column) in statement.columns.iter().enumerate() {
                let inillucent_sql::dml::ColumnSource::Row(at) = column else {
                    continue;
                };
                if let (Some(slot), Some(value)) = (cells.get_mut(position), supplied.get(*at)) {
                    *slot = value.clone();
                }
            }
            self.change_module(
                &statement.table.name,
                &Change::Insert {
                    rowid: Value::Null,
                    values: cells,
                },
            )?;
            changed = changed.saturating_add(1);
        }
        // Outside a transaction the statement is its own, so the module flushes
        // and the log commits here; inside one, `commit_batch` does both.
        if self.batch.get().is_none() {
            self.sync_modules()?;
            self.seal()?;
        }
        Ok(Outcome {
            rows: Vec::new(),
            names: Vec::new(),
            changes: inillucent_exec::dml::Changes {
                rows: changed,
                ..Default::default()
            },
        })
    }
}

/// Reports whether one value satisfies one of a module's constraints.
///
/// The comparisons are the dialect's, under the column's own collation. An
/// operator the engine cannot evaluate - `MATCH`, `GLOB`, `LIKE`, a module's own
/// function - is refused rather than answered: the module either promised to
/// apply it or it cannot be applied at all, and answering as though it had been
/// would return rows the query excluded.
///
/// @param held - the value the module produced
/// @param op - the operator the constraint carries
/// @param wanted - the value on the other side
/// @param collation - the column's collation
fn satisfies(
    held: &OwnedDatum,
    op: inillucent_sql::vtab::ConstraintOp,
    wanted: &OwnedDatum,
    collation: inillucent_value::Collation,
) -> DbResult<bool> {
    use inillucent_sql::vtab::ConstraintOp;
    use std::cmp::Ordering;
    let left = inillucent_exec::scalar::to_value(held.borrow());
    let right = inillucent_exec::scalar::to_value(wanted.borrow());
    if matches!(left, Value::Null) || matches!(right, Value::Null) {
        // A comparison against NULL is unknown, which excludes the row.
        return Ok(false);
    }
    let order = inillucent_value::compare::compare_values(&left, &right, collation);
    Ok(match op {
        ConstraintOp::Eq | ConstraintOp::Is => order == Ordering::Equal,
        ConstraintOp::Ne | ConstraintOp::IsNot => order != Ordering::Equal,
        ConstraintOp::Lt => order == Ordering::Less,
        ConstraintOp::Le => order != Ordering::Greater,
        ConstraintOp::Gt => order == Ordering::Greater,
        ConstraintOp::Ge => order != Ordering::Less,
        other => {
            return Err(misuse(format!(
                "the module did not apply a {other:?} constraint and the engine cannot"
            )))
        }
    })
}

impl ImportedDatabase {
    /// Flushes every connected module before the engine commits.
    ///
    /// **Once per transaction, not once per row.** FTS5 holds a segment in
    /// memory and writes it out on `sync`, which is the whole reason the method
    /// exists - a sync after every insert turns a bulk load into one segment
    /// flush per document, and `extension.fts.build` measured 436 microseconds
    /// per row against SQLite's 5.5. SQLite syncs its modules at the end of the
    /// statement's transaction and so does this.
    pub(super) fn sync_modules(&mut self) -> DbResult<()> {
        let names: Vec<Vec<u8>> = self.virtual_tables.keys().cloned().collect();
        for name in names {
            let Some(mut connected) = self.virtual_tables.remove(&name) else {
                continue;
            };
            let outcome = {
                let txn = self.current_txn();
                let mut log = WalLog {
                    wal: &self.wal,
                    txn,
                };
                let mut store = WriteStore {
                    database: &mut self.database,
                    trees: &mut self.trees,
                    log: &mut log,
                };
                let mut nowhere = Nowhere;
                let mut context = Context {
                    host: &mut nowhere,
                    store: Some(&mut store),
                    database: 0,
                    limits: &self.limits,
                    catalog: None,
                };
                connected
                    .table
                    .sync(&mut context)
                    .and_then(|()| connected.table.commit(&mut context))
            };
            self.virtual_tables.insert(name, connected);
            outcome?;
        }
        Ok(())
    }
}
