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

use inillucent_base::error::{refusal, statement_refusal};
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
fn borrowed_bytes<'v>(values: &'v [Value<'static>]) -> Vec<std::borrow::Cow<'v, [u8]>> {
    values
        .iter()
        .map(|value| match value {
            // UTF-8 text and every blob are borrowed; only text stored in
            // another encoding is converted, and only that one allocates.
            Value::Text(text) => text.utf8_bytes(),
            Value::Blob(blob) => std::borrow::Cow::Borrowed(blob.raw()),
            _ => std::borrow::Cow::Borrowed(&[][..]),
        })
        .collect()
}

/// Borrows a module's row as the tree's own data, copying nothing it need not.
///
/// **A shadow write used to copy every text and every blob twice**: once into
/// an `OwnedDatum` and once again when the tree borrowed it back. On FTS5's
/// `%_data` the blob is a term's whole doclist, so a flush of five hundred
/// terms copied every doclist twice for nothing. The bytes are
/// borrowed from the caller's values instead, through `holder`, which has to
/// outlive the datums for exactly that reason.
///
/// @param values - the module's row
/// @param holder - the byte slices [`borrowed_bytes`] produced for it
fn as_datums<'v>(
    values: &'v [Value<'static>],
    holder: &'v [std::borrow::Cow<'v, [u8]>],
) -> Vec<Datum<'v>> {
    values
        .iter()
        .enumerate()
        .map(|(at, value)| match value {
            Value::Null => Datum::Null,
            Value::Integer(number) => Datum::Int(*number),
            Value::Real(number) => Datum::Real(*number),
            Value::Text(_) => Datum::Text(holder.get(at).map(|held| held.as_ref()).unwrap_or(&[])),
            Value::Blob(_) => Datum::Blob(holder.get(at).map(|held| held.as_ref()).unwrap_or(&[])),
        })
        .collect()
}

/// Returns a module's row as owned data.
///
/// For the callers that keep it past the values it came from. Everything on the
/// write path uses [`as_datums`] instead, which copies nothing.
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

/// Walks a shadow table's rows in key order, starting at a given rowid.
///
/// **A seek to `from`, not a scan of the whole tree.** `PagedTree::visit_range`
/// descends once to the leaf that could hold `from` and walks right from
/// there, which is what turns `deltas_above` from a read of the whole delta
/// log - every entry a base generation has already folded, decoded and
/// thrown away, on every commit - into a read of only the entries that
/// arrived since. A rowid table's rows are already in key order, so nothing
/// below `from` is worth visiting at all.
///
/// @param pool - the buffer pool
/// @param tree - the shadow table
/// @param from - the smallest rowid to visit
/// @param body - what to do with each row, stopping when it says so
fn walk_from(
    pool: &Pool,
    tree: &PagedTree,
    from: i64,
    body: &mut dyn FnMut(&[Value<'static>]) -> DbResult<bool>,
) -> DbResult<()> {
    let low = tree.encode_key(&[Datum::Int(from)]);
    let mut stop = false;
    let mut failure: Option<inillucent_base::DbError> = None;
    tree.visit_range(pool, &low, &mut |leaf| {
        for row in leaf.live()? {
            // The leaf `visit_range` lands on is the one that *could* hold
            // `from` - it may still open below it, since a leaf's range is
            // bounded by its neighbours' keys rather than by `from` itself -
            // so what a seek does not do for free is done here instead,
            // once per row rather than once per tree.
            if let Some(Datum::Int(rowid)) = row.first().copied() {
                if rowid < from {
                    continue;
                }
            }
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

/// Walks a keyed shadow table's rows in key order, starting at a given key.
///
/// **A seek to `from`, not a scan of the whole tree** - the keyed twin of
/// [`walk_from`], for a key of more than one column. FTS5's `%_idx` is keyed
/// `(segid, term)`, so a caller that wants one live segment's terms starting
/// at a prefix descends once to `(segid, prefix)` and walks right, instead of
/// reading every row of every segment - and every tombstone, which this same
/// key space also holds at a negative segid - to find the ones that match.
///
/// @param pool - the buffer pool
/// @param tree - the shadow table
/// @param from - the key to start at
/// @param body - what to do with each row, stopping when it says so
fn walk_keyed_from(
    pool: &Pool,
    tree: &PagedTree,
    from: &[Value<'static>],
    body: &mut dyn FnMut(&[Value<'static>]) -> DbResult<bool>,
) -> DbResult<()> {
    let owned = as_row(from);
    let probe: Vec<Datum<'_>> = owned.iter().map(OwnedDatum::borrow).collect();
    let low = tree.encode_key(&probe);
    let mut stop = false;
    let mut failure: Option<inillucent_base::DbError> = None;
    tree.visit_range(pool, &low, &mut |leaf| {
        for row in leaf.live()? {
            let owned_row: Vec<OwnedDatum> = row.iter().map(OwnedDatum::from_datum).collect();
            let values = match as_values(&owned_row) {
                Ok(values) => values,
                Err(error) => {
                    failure = Some(error);
                    return Ok(false);
                }
            };
            // The leaf `visit_range` lands on is the one that *could* hold
            // `from` - it may still open below it, the same reason
            // `walk_from` re-checks every rowid rather than trusting the
            // descent alone.
            if inillucent_sql::vtab::key_sorts_below(&values, from) {
                continue;
            }
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
    refusal("a module wrote to a shadow table while answering a query")
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

    fn scan_from(
        &mut self,
        root: u32,
        from: i64,
        body: &mut dyn FnMut(i64, &[Value<'static>]) -> DbResult<bool>,
    ) -> DbResult<()> {
        let Some(tree) = self.trees.get(&root) else {
            return Ok(());
        };
        walk_from(self.pool, tree, from, &mut |values| {
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

    fn scan_keyed_from(
        &mut self,
        root: u32,
        _key_columns: usize,
        from: &[Value<'static>],
        body: &mut dyn FnMut(&[Value<'static>]) -> DbResult<bool>,
    ) -> DbResult<()> {
        let Some(tree) = self.trees.get(&root) else {
            return Ok(());
        };
        walk_keyed_from(self.pool, tree, from, body)
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
        let holder = borrowed_bytes(values);
        let mut row = as_datums(values, &holder);
        // A rowid table stores its key once, as the tree's key column, and the
        // module hands the row with its rowid in the first slot - which is what
        // the shadow table declares as `INTEGER PRIMARY KEY`.
        if let Some(first) = row.first_mut() {
            *first = Datum::Int(rowid);
        }
        let tree = self
            .trees
            .get_mut(&root)
            .ok_or_else(|| refusal("no shadow table for that root"))?;
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

    fn scan_from(
        &mut self,
        root: u32,
        from: i64,
        body: &mut dyn FnMut(i64, &[Value<'static>]) -> DbResult<bool>,
    ) -> DbResult<()> {
        let Some(tree) = self.trees.get(&root) else {
            return Ok(());
        };
        walk_from(self.database.pool(), tree, from, &mut |values| {
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
        let holder = borrowed_bytes(values);
        let row = as_datums(values, &holder);
        let tree = self
            .trees
            .get_mut(&root)
            .ok_or_else(|| refusal("no shadow table for that root"))?;
        // **`put`, not `insert`.** `insert` copies out the row it replaced -
        // every column of it, allocating per text and per blob - and this
        // caller throws that away. On FTS5's `%_data` the replaced row is the
        // term's whole doclist, so a shadow write was paying to materialise a
        // multi-kilobyte value nobody reads. It is the same write otherwise.
        tree.put(self.database, self.log, &row)?;
        Ok(())
    }

    fn delete_keyed(&mut self, root: u32, key: &[Value<'static>]) -> DbResult<()> {
        let holder = borrowed_bytes(key);
        let probe = as_datums(key, &holder);
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

    fn scan_keyed_from(
        &mut self,
        root: u32,
        _key_columns: usize,
        from: &[Value<'static>],
        body: &mut dyn FnMut(&[Value<'static>]) -> DbResult<bool>,
    ) -> DbResult<()> {
        let Some(tree) = self.trees.get(&root) else {
            return Ok(());
        };
        walk_keyed_from(self.database.pool(), tree, from, body)
    }
}

/// One point in a transaction a module is told about.
///
/// **Five of them, and before task-1932 the engine told a module about two.**
/// `begin` fired at `CREATE VIRTUAL TABLE` and nowhere else, and `savepoint`
/// and `release` were never called at all - so a module could not buffer a
/// transaction, could not mark a point inside one, and could not be told that a
/// point it had marked was no longer needed. The trait has had all five since
/// the old engine.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Moment {
    /// A write transaction that reaches a module has started.
    Begin,
    /// The transaction was abandoned.
    Rollback,
    /// The transaction was rolled back to a savepoint, and stays open.
    RollbackTo(i32),
    /// A savepoint was opened at this level.
    Savepoint(i32),
    /// The savepoints above this level were released.
    Release(i32),
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
            return Err(refusal(format!(
                "table {} already exists",
                String::from_utf8_lossy(name)
            )));
        }
        // **`SQLITE_ERROR` (primary code 1), not `SQLITE_MISUSE` (21).** SQLite
        // answers "no such module: x" - and, for a module it has but will not
        // let `CREATE VIRTUAL TABLE` construct, the very same message and code
        // rather than a distinct one, which is why `an_eponymous_only_module_
        // cannot_be_created` grades this by code and not by wording: a
        // clearer message here is worth keeping, the number behind it is not
        // this engine's to invent. `refusal` answers `Misuse` unconditionally,
        // which is right for an API contract violation and wrong for "the
        // statement named something that is not there" - the same class of
        // mistake `refused()`'s own doc comment already found and fixed for
        // parser and binder refusals.
        let found = self.registry.module(module).ok_or_else(|| {
            statement_refusal(format!(
                "no such module: {}",
                String::from_utf8_lossy(module)
            ))
        })?;
        if !found.constructible() {
            return Err(statement_refusal(format!(
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
            // **A shadow with an owner already exists.** It belongs to another
            // table and is being *read*, not made - see `ShadowTable::owner`.
            // Creating it would put a second, empty copy of somebody else's
            // storage beside the real one.
            if let Some(owner) = &shadow.owner {
                let root = self.existing_shadow_root(owner, &shadow.suffix)?;
                connect.shadows.push(ShadowRoot {
                    suffix: shadow.suffix.clone(),
                    root,
                });
                continue;
            }
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
                // Filled by `record` from the identifier it is given.
                tree_id: 0,
            },
        )?;
        self.rebuild_tables()?;
        self.refresh_catalog();
        // The module is connected *after* its shadow tables exist, because a
        // module that writes an initial row writes it into one of them.
        let table = {
            let txn = self.current_txn();
            let at = self.ddl_schema;
            let session = self.session.get();
            let wal = self
                .log_of(at)
                .ok_or_else(|| refusal("a statement names a database that is not attached"))?;
            let mut log = WalLog {
                wal,
                txn,
                schema: at,
                wrote: false,
                // **The before-images a rollback needs.** Every ordinary write
                // passes `Some(&self.undo)`; this path passed `None`, so a
                // virtual table's writes went into the pool with nothing
                // recorded that could put them back. `ROLLBACK` then undid
                // every ordinary table and left the module's shadow trees as
                // the abandoned transaction had made them - so the connection
                // read two rows where the file held one, and a reopen was the
                // only thing that corrected it. The file itself was never
                // wrong: no commit record was written, so recovery ignored
                // the pages. Only the live connection was.
                undo: Some(&self.undo),
                uncommitted: self.uncommitted_handle_of(at),
            };
            let store = WriteStore {
                database: super::file_of(
                    &mut self.database,
                    &mut self.attached,
                    &mut self.temps,
                    session,
                    at,
                )?,
                trees: &mut self.trees,
                log: &mut log,
            };
            let mut nowhere = inillucent_ext::vtab::WithStore { store };
            let mut context = Context {
                host: &mut nowhere,
                database: 0,
                limits: &self.limits,
                catalog: Some(&self.catalog),
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
    #[allow(clippy::too_many_arguments)]
    /// Returns the root of a shadow table another object already owns.
    ///
    /// The catalog rows are the authority, as they are at open time, and a name
    /// that is not there is a refusal: a module that was handed a root it could
    /// not find would either answer nothing or make its own copy, and both are
    /// worse than saying so.
    ///
    /// @param owner - the table the shadows belong to
    /// @param suffix - which shadow
    fn existing_shadow_root(&self, owner: &[u8], suffix: &[u8]) -> DbResult<u32> {
        let wanted = shadow_table_name(owner, suffix).to_ascii_lowercase();
        self.entries
            .iter()
            .find(|recorded| recorded.entry.name.to_ascii_lowercase() == wanted)
            .map(|recorded| recorded.root)
            .ok_or_else(|| {
                refusal(format!(
                    "no such table: {}",
                    String::from_utf8_lossy(&wanted)
                ))
            })
    }

    /// Returns what a module says about its own storage.
    ///
    /// `None` when the name is not a connected virtual table, which is what
    /// `rtreecheck` turns into a refusal rather than into a cheerful `ok`.
    ///
    /// **A second connection to the same table, not the one already open.** A
    /// check takes the table by `&mut` because it may flush what a transaction
    /// staged, and this is reached through the read-only catalog the physical
    /// pass holds. Connecting again is cheap - it reads the arguments and the
    /// shadow roots, both of which are already in hand - and it is also more
    /// honest: what is checked is what a *fresh* open would find, which is the
    /// question a caller running an integrity check is asking.
    ///
    /// @param name - the table's name, as written
    pub(super) fn module_integrity(&self, name: &[u8]) -> DbResult<Option<Option<String>>> {
        let folded = name.to_ascii_lowercase();
        let Some(connected) = self.virtual_tables.get(&folded) else {
            return Ok(None);
        };
        let arguments = connected.arguments.clone();
        let Some(found) = self.registry.module(&arguments.module) else {
            return Ok(None);
        };
        let mut table = found.connect(&arguments, false)?;
        let store = ReadStore {
            pool: self.database.pool(),
            trees: &self.trees,
        };
        let mut nowhere = inillucent_ext::vtab::WithStore { store };
        let mut context = Context {
            host: &mut nowhere,
            database: 0,
            limits: &self.limits,
            catalog: Some(&self.catalog),
        };
        table.integrity(&mut context).map(Some)
    }

    pub(super) fn rows_of_module(
        &self,
        table: &inillucent_sql::catalog_view::TableInfo,
        path: &AccessPath,
        params: &inillucent_exec::physical::Params,
        needed: &inillucent_sql::bind::ColumnUse,
        supplied: &[inillucent_tree::datum::OwnedDatum],
        downstream: &mut dyn inillucent_exec::ops::Sink,
    ) -> DbResult<bool> {
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
            return Ok(false);
        };
        // **A `pragma_*` function is answered by the connection, not a module.**
        // Its rows come from `pragma_rows` - the same function `PRAGMA
        // table_info(t)` runs - because a pragma reads the connection, and a
        // `Module` reaches its storage through a `Context` that has no way to
        // ask one. One implementation, two spellings.
        if table.folded.starts_with(b"pragma_") {
            return self.pragma_function_rows(table, offer, params, downstream);
        }
        // The same arrangement for the four that describe statements; see
        // `crate::introspect`. Each takes its argument as an `Eq` constraint on
        // its hidden column, which is what makes `bytecode('SELECT 1')` a
        // table-valued function rather than a special form.
        if matches!(
            table.folded.as_slice(),
            b"bytecode" | b"tables_used" | b"sqlite_stmt" | b"completion"
        ) {
            let argument = self.eponymous_argument(offer, params, table)?;
            let rows = match table.folded.as_slice() {
                b"bytecode" => self.bytecode_rows(&argument)?,
                b"tables_used" => self.tables_used_rows(&argument)?,
                b"sqlite_stmt" => self.stmt_rows()?,
                _ => self.completion_rows(&argument)?,
            };
            // The argument came out of an `Eq` on the first hidden column and
            // is the only constraint this answer applied; everything else the
            // planner took out of the residual has to be tested here.
            self.emit_filtered(rows, offer, params, &hidden_columns(table), downstream)?;
            return Ok(true);
        }
        // The same arrangement for the two tables that describe the file; see
        // `crate::inspect`.
        if table.folded == b"dbstat" || table.folded == b"sqlite_dbpage" {
            let mut rows = if table.folded == b"dbstat" {
                self.dbstat_rows()?
            } else {
                self.dbpage_rows()?
            };
            // The hidden `schema` column, which every row of an eponymous
            // table carries and no `SELECT *` reads.
            for row in &mut rows {
                row.push(OwnedDatum::Text(b"main".to_vec()));
            }
            // Nothing here consumed a constraint - the schema qualifier is
            // always `main` and the rows are the whole file - so every offered
            // predicate is the engine's to test.
            self.emit_filtered(rows, offer, params, &[], downstream)?;
            return Ok(true);
        }
        // **An eponymous module has nothing in `virtual_tables`**, because
        // nothing ever created it: the name is the table. It is connected here,
        // for this scan, with no arguments - which is all `SeriesModule` and
        // `JsonWalkModule` want, since the arguments a caller wrote arrive as
        // `Eq` constraints on the hidden columns rather than as connect-time
        // text. The connection is not cached: these modules hold no state, and
        // caching one would mean a map that has to be invalidated when the
        // registry changes.
        let held;
        let connected = match self.virtual_tables.get(&table.folded) {
            Some(connected) => connected,
            None => {
                let Some(connected) = self.connect_eponymous(&table.folded)? else {
                    return Ok(false);
                };
                held = connected;
                &held
            }
        };
        let specs: Vec<inillucent_sql::vtab::ConstraintSpec> =
            offer.iter().map(|held| held.spec).collect();
        let mut query = IndexQuery::new(specs, order_by.clone());
        connected.table.best_index(&mut query)?;
        // **The caller's arguments win when it has any.** A lateral join has
        // already evaluated them against the outer row - which is the only
        // place they *can* be evaluated - and folding them again here would
        // fold an expression reading a column that is not in scope. See
        // `inillucent_exec::lateral`.
        let mut arguments: Vec<Value<'static>> = Vec::new();
        if supplied.is_empty() {
            for position in query.argument_order() {
                let Some(constraint) = offer.get(position) else {
                    continue;
                };
                arguments.push(inillucent_exec::scalar::to_value(
                    inillucent_exec::physical::literal_value(&constraint.value, params)?.borrow(),
                ));
            }
        } else {
            for value in supplied {
                arguments.push(inillucent_exec::scalar::to_value(value.borrow()));
            }
        }
        let plan = FilterPlan {
            index_number: query.index_number,
            index_string: query.index_string.clone(),
            arguments,
        };
        let mut cursor = connected.table.open()?;
        let store = ReadStore {
            pool: self.database.pool(),
            trees: &self.trees,
        };
        let mut nowhere = inillucent_ext::vtab::WithStore { store };
        let mut context = Context {
            host: &mut nowhere,
            database: 0,
            limits: &self.limits,
            catalog: Some(&self.catalog),
        };
        let width = connected.table.declaration().columns.len();
        // **A rowid constraint the module did not promise still needs the
        // rowid in the row, so the recheck below has something to test.** A
        // negative `constraint.spec.column` names the rowid, which is never
        // one of the module's declared columns - `WHERE rowid > 2` on an FTS5
        // table, or `docs.rowid` in a join's `ON`, both offer one. Forcing the
        // rowid into the row here is what lets the recheck loop below test it
        // at `width`, the slot it is appended at, instead of refusing outright
        // because "a produced row does not carry it".
        let rowid_recheck_needed = offer.iter().enumerate().any(|(position, constraint)| {
            let promised = query
                .usage
                .get(position)
                .map(|usage| usage.omit)
                .unwrap_or(false);
            !promised && constraint.spec.column < 0
        });
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
        // **Everything the module did not promise is tested here, per row.**
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
        //
        // It is built before the scan rather than applied after it, because the
        // scan no longer produces a `Vec` there is an "after" for.
        let mut rechecks: Vec<(
            usize,
            inillucent_sql::vtab::ConstraintOp,
            OwnedDatum,
            inillucent_value::collation::Collation,
        )> = Vec::new();
        for (position, constraint) in offer.iter().enumerate() {
            let promised = query
                .usage
                .get(position)
                .map(|usage| usage.omit)
                .unwrap_or(false);
            if promised {
                continue;
            }
            // A negative column is the rowid. It is not one of the module's
            // declared columns, so it is not in `row` at its own position -
            // `rowid_recheck_needed` above forced it into `row` at `width`
            // instead, appended the same way `needed.rowid` does for a `SELECT`
            // that reads it, and `Collation::Binary` is what a rowid - always
            // an integer - compares under.
            let column = match usize::try_from(constraint.spec.column) {
                Ok(column) => column,
                Err(_) => {
                    rechecks.push((
                        width,
                        constraint.spec.op,
                        recheck_value(constraint, position, supplied, params)?,
                        inillucent_value::collation::Collation::Binary,
                    ));
                    continue;
                }
            };
            rechecks.push((
                column,
                constraint.spec.op,
                recheck_value(constraint, position, supplied, params)?,
                connected.table.collation(column),
            ));
        }
        // **A batch at a time, and abandoned when the pipeline says stop.** The
        // buffer is one batch rather than the whole answer, which is what makes
        // `SELECT value FROM generate_series(1,10) LIMIT 3` return: without it
        // the scan ran to 4,294,967,295 rows before the `LIMIT` above it saw a
        // single one.
        let mut rows: Vec<Vec<OwnedDatum>> = Vec::with_capacity(inillucent_exec::batch::BATCH_ROWS);
        let mut stopped = false;
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
            // Appended after the declared columns, which is where
            // `plan_stages` puts the rowid slot for a materialised virtual
            // scan. Asked of the cursor when the query reads it, or when an
            // unpromised rowid constraint needs it to recheck against - see
            // `rowid_recheck_needed` - so a module whose rowid is expensive is
            // not asked for one nobody wanted otherwise.
            if needed.rowid || rowid_recheck_needed {
                row.push(OwnedDatum::Int(cursor.rowid()?));
            }
            // The module's auxiliary functions, in the order `plan_stages`
            // allocated their slots. `bm25(docs)` is the whole reason the
            // mechanism exists, and it reads the cursor rather than a column -
            // so it can only be answered here, while the cursor is still on the
            // row. The arguments after the table are constants of the
            // statement; a call whose arguments varied per row would be a
            // different feature and is not one the modules declare.
            //
            // **They are passed.** An empty list used to go down here, so
            // `bm25(t, 10.0)` ignored its weights and
            // `highlight(t, 0, '[', ']')` could not be written at all.
            for (name, arguments) in &needed.functions {
                let mut values: Vec<Value<'static>> = Vec::with_capacity(arguments.len());
                for argument in arguments {
                    values.push(inillucent_exec::scalar::to_value(
                        inillucent_exec::physical::literal_value(argument, params)?.borrow(),
                    ));
                }
                row.push(inillucent_exec::scalar::from_value(cursor.auxiliary(
                    &mut context,
                    name,
                    &values,
                )?));
            }
            if !passes_rechecks(&row, &rechecks, self.case_sensitive_like)? {
                cursor.next(&mut context)?;
                continue;
            }
            rows.push(row);
            if rows.len() >= inillucent_exec::batch::BATCH_ROWS {
                if inillucent_exec::ops::emit_rows(&rows, downstream)?
                    == inillucent_exec::ops::Flow::Stop
                {
                    stopped = true;
                    break;
                }
                rows.clear();
            }
            cursor.next(&mut context)?;
        }
        // `context` borrows the connection for the length of the scan; this
        // ends the borrow so the emit below may take it again.
        let _ = context;
        if !stopped && !rows.is_empty() {
            inillucent_exec::ops::emit_rows(&rows, downstream)?;
        }
        Ok(true)
    }
}

/// One constraint the engine has to test for itself: which column, which
/// operator, against what, under which collation.
type Recheck = (
    usize,
    inillucent_sql::vtab::ConstraintOp,
    OwnedDatum,
    inillucent_value::collation::Collation,
);

/// Returns the positions of a table's hidden columns.
///
/// A table-valued function's arguments arrive as `Eq` constraints on these, so
/// they are the constraints an eponymous answer has already applied by the time
/// it has any rows.
///
/// @param table - the function's catalog entry
fn hidden_columns(table: &inillucent_sql::catalog_view::TableInfo) -> Vec<i32> {
    table
        .columns
        .iter()
        .enumerate()
        .filter(|(_, column)| column.hidden)
        .filter_map(|(at, _)| i32::try_from(at).ok())
        .collect()
}

/// Returns the value one recheck tests a produced row against.
///
/// **A lateral join already evaluated a correlated constraint, and this is
/// where its answer is read back rather than recomputed.** `docs.rowid = t.id`
/// offers `t.id` as a constraint on the module the same way a literal argument
/// does, and when the module does not promise `omit` for it, the recheck loop
/// used to fold it with `literal_value` regardless - which is the fold
/// `literal_value`'s own doc comment refuses: an expression that reads a
/// column has no value outside the row it was read from. `supplied` is that
/// row's values, one per offered constraint and in the same order `offer`
/// itself is in - see `inillucent_exec::lateral::LateralModule`, which is the
/// one thing that ever fills it - so a position it covers already has an
/// answer and does not need one folded. An ordinary scan supplies nothing, and
/// every constraint's value is a genuine statement-wide constant then, which
/// is exactly what `literal_value` answers.
///
/// @param constraint - the offered constraint being rechecked
/// @param position - its position in `offer`, which is also its position in
///   `supplied`
/// @param supplied - the lateral join's per-row values, empty for an ordinary
///   scan
/// @param params - the values bound to `?1`, `?2`, ...
fn recheck_value(
    constraint: &inillucent_sql::plan::VirtualConstraint,
    position: usize,
    supplied: &[OwnedDatum],
    params: &inillucent_exec::physical::Params,
) -> DbResult<OwnedDatum> {
    match supplied.get(position) {
        Some(value) => Ok(value.clone()),
        None => inillucent_exec::physical::literal_value(&constraint.value, params),
    }
}

/// Reports whether a produced row satisfies the constraints the module left.
///
/// @param row - the row the cursor produced
/// @param rechecks - the column, operator, value and collation of each
/// @param case_sensitive - `PRAGMA case_sensitive_like`, for a `LIKE` recheck
fn passes_rechecks(
    row: &[OwnedDatum],
    rechecks: &[Recheck],
    case_sensitive: bool,
) -> DbResult<bool> {
    for (column, op, wanted, collation) in rechecks {
        let Some(held) = row.get(*column) else {
            return Ok(false);
        };
        if !satisfies(held, *op, wanted, *collation, case_sensitive)? {
            return Ok(false);
        }
    }
    Ok(true)
}

impl ImportedDatabase {
    /// Answers a `pragma_*` table-valued function.
    ///
    /// The argument arrives as an `Eq` constraint on the first hidden column and
    /// the schema qualifier as one on the second, which is exactly what
    /// `bind_table_arguments` produces for `pragma_table_info('t')`. Nothing is
    /// promised to the planner, so the two constraints stay in the residual and
    /// the pipeline tests them again - which is why the produced row carries the
    /// argument and the schema in its own hidden columns rather than dropping
    /// them.
    ///
    /// @param table - the function's catalog entry
    /// @param offer - the constraints the planner pushed down
    /// @param params - the values bound to `?1`, `?2`, ...
    /// Returns the value an eponymous table's first hidden column was given.
    ///
    /// A table-valued function's arguments arrive as `Eq` constraints on the
    /// hidden columns rather than as text at connect time, which is what makes
    /// `bytecode(?)` bindable and `bytecode(t.sql)` joinable.
    ///
    /// @param offer - the constraints the planner is offering
    /// @param params - the statement's bound parameters
    /// @param table - the table being scanned
    fn eponymous_argument(
        &self,
        offer: &[inillucent_sql::plan::VirtualConstraint],
        params: &inillucent_exec::physical::Params,
        table: &inillucent_sql::catalog_view::TableInfo,
    ) -> DbResult<Vec<u8>> {
        let Some(first) = table
            .columns
            .iter()
            .position(|column| column.hidden)
            .and_then(|at| i32::try_from(at).ok())
        else {
            return Ok(Vec::new());
        };
        for constraint in offer {
            if constraint.spec.op != inillucent_sql::vtab::ConstraintOp::Eq
                || constraint.spec.column != first
            {
                continue;
            }
            let value = inillucent_exec::physical::literal_value(&constraint.value, params)?;
            return Ok(pragma_argument_text(&value));
        }
        Ok(Vec::new())
    }

    /// @param downstream - where the batches go
    fn pragma_function_rows(
        &self,
        table: &inillucent_sql::catalog_view::TableInfo,
        offer: &[inillucent_sql::plan::VirtualConstraint],
        params: &inillucent_exec::physical::Params,
        downstream: &mut dyn inillucent_exec::ops::Sink,
    ) -> DbResult<bool> {
        let Some(pragma) = table.folded.strip_prefix(b"pragma_".as_slice()) else {
            return Ok(false);
        };
        let hidden: Vec<usize> = table
            .columns
            .iter()
            .enumerate()
            .filter(|(_, column)| column.hidden)
            .map(|(at, _)| at)
            .collect();
        let mut argument = OwnedDatum::Null;
        let mut schema = OwnedDatum::Null;
        for constraint in offer {
            if constraint.spec.op != inillucent_sql::vtab::ConstraintOp::Eq {
                continue;
            }
            let Ok(column) = usize::try_from(constraint.spec.column) else {
                continue;
            };
            let value = inillucent_exec::physical::literal_value(&constraint.value, params)?;
            if hidden.first() == Some(&column) {
                argument = value;
            } else if hidden.get(1) == Some(&column) {
                schema = value;
            }
        }
        // The pragma reader takes the argument as the parser's own shape, which
        // is a name or an expression; a value bound at run time is neither, so
        // it is spelled back as the text the reader reads.
        let spelled = match &argument {
            OwnedDatum::Null => None,
            other => Some(inillucent_sql::directive::PragmaArgument::Name(
                pragma_argument_text(other),
            )),
        };
        let Some(answer) = self.pragma_rows(pragma, spelled.as_ref())? else {
            return Ok(false);
        };
        let width = answer.names.len();
        let mut rows: Vec<Vec<OwnedDatum>> = Vec::with_capacity(answer.rows.len());
        for row in answer.rows {
            let mut held = row;
            held.truncate(width);
            while held.len() < width {
                held.push(OwnedDatum::Null);
            }
            held.push(argument.clone());
            held.push(schema.clone());
            rows.push(held);
        }
        // The two hidden columns *are* the arguments and were applied above;
        // every other offered predicate was taken out of the residual on the
        // promise that something would test it, and this is the something.
        let applied: Vec<i32> = hidden
            .iter()
            .filter_map(|at| i32::try_from(*at).ok())
            .collect();
        self.emit_filtered(rows, offer, params, &applied, downstream)?;
        Ok(true)
    }

    /// Emits rows this connection produced itself, testing the predicates the
    /// planner took out of the residual and nothing else applied.
    ///
    /// **The eponymous answers are not module cursors, and that is why they
    /// need this.** `virtual_path` consumes every offered predicate on the
    /// optimistic assumption that the scan puts back what it does not apply;
    /// the module path keeps that promise in `passes_rechecks`, and the
    /// branches that answer out of the connection - `dbstat`, `sqlite_dbpage`,
    /// `pragma_*`, `bytecode`, `tables_used`, `sqlite_stmt`, `completion` -
    /// returned before reaching it. `SELECT name FROM dbstat WHERE
    /// name='main_key'` answered with every page in the file, and
    /// `SELECT name FROM pragma_table_info('t') WHERE name='b'` with every
    /// column.
    ///
    /// @param rows - the rows the connection produced, hidden columns included
    /// @param offer - every constraint the planner pushed down
    /// @param params - the values bound to `?1`, `?2`, ...
    /// @param applied - the columns this answer already filtered on
    /// @param downstream - where the batches go
    fn emit_filtered(
        &self,
        rows: Vec<Vec<OwnedDatum>>,
        offer: &[inillucent_sql::plan::VirtualConstraint],
        params: &inillucent_exec::physical::Params,
        applied: &[i32],
        downstream: &mut dyn inillucent_exec::ops::Sink,
    ) -> DbResult<()> {
        let width = rows.first().map(Vec::len).unwrap_or(0);
        let mut rechecks: Vec<Recheck> = Vec::new();
        for constraint in offer {
            let column = constraint.spec.column;
            if applied.contains(&column) {
                continue;
            }
            // A negative column is the rowid, which these rows do not carry -
            // and answering with every row would be the bug this exists to
            // close, so it is refused instead.
            let Some(at) = usize::try_from(column).ok().filter(|at| *at < width) else {
                if rows.is_empty() {
                    continue;
                }
                return Err(refusal(
                    "the engine cannot test that constraint against this table",
                ));
            };
            rechecks.push((
                at,
                constraint.spec.op,
                inillucent_exec::physical::literal_value(&constraint.value, params)?,
                inillucent_value::collation::Collation::Binary,
            ));
        }
        let mut batch: Vec<Vec<OwnedDatum>> =
            Vec::with_capacity(inillucent_exec::batch::BATCH_ROWS);
        for row in rows {
            if !passes_rechecks(&row, &rechecks, self.case_sensitive_like)? {
                continue;
            }
            batch.push(row);
            if batch.len() >= inillucent_exec::batch::BATCH_ROWS {
                if inillucent_exec::ops::emit_rows(&batch, downstream)?
                    == inillucent_exec::ops::Flow::Stop
                {
                    return Ok(());
                }
                batch.clear();
            }
        }
        if !batch.is_empty() {
            inillucent_exec::ops::emit_rows(&batch, downstream)?;
        }
        Ok(())
    }

    /// Connects an eponymous module for the length of one scan.
    ///
    /// `Ok(None)` means the name is not an eponymous module, which is how a
    /// caller with no virtual table of that name at all is told so.
    ///
    /// @param folded - the module's folded name
    fn connect_eponymous(&self, folded: &[u8]) -> DbResult<Option<Connected>> {
        let Some(module) = self.registry.eponymous(folded) else {
            return Ok(None);
        };
        let arguments = inillucent_sql::vtab::ModuleArguments {
            database: 0,
            schema: b"main".to_vec(),
            table: folded.to_vec(),
            module: folded.to_vec(),
            arguments: Vec::new(),
            shadows: Vec::new(),
        };
        let table = module.connect(&arguments, false)?;
        Ok(Some(Connected { table, arguments }))
    }

    /// Connects every virtual table the catalog declares.
    ///
    /// **A reopened database has to reach its modules.** `CREATE VIRTUAL TABLE`
    /// connects one and holds it in `virtual_tables`, which lives in memory; a
    /// later open starts with that map empty, so every virtual table answered
    /// "no such table" until this ran. The module trait already anticipated it -
    /// `connect(arguments, creating)` documents `creating` as "true only for
    /// the `CREATE VIRTUAL TABLE` that first makes it. A module that has to
    /// write an initial row into a shadow table does it then; every later open
    /// is a connect and writes nothing." Nobody was calling it with `false`.
    ///
    /// The arguments are read back out of the stored `CREATE VIRTUAL TABLE`
    /// through the ordinary parser rather than remembered separately, because a
    /// second remembered copy of a declaration is a second thing that can
    /// disagree with the file.
    pub(super) fn reconnect_modules(&mut self) -> DbResult<()> {
        let declarations: Vec<Vec<u8>> = self
            .entries
            .iter()
            .filter(|recorded| recorded.entry.kind == ObjectKind::Table)
            .map(|recorded| recorded.entry.sql.clone())
            .collect();
        for sql in declarations {
            let parsed = match inillucent_sql::parser::parse_next_statement(&sql, 0, &self.limits) {
                Ok(parsed) => parsed,
                Err(_) => continue,
            };
            let inillucent_sql::ast::Statement::CreateVirtualTable {
                name,
                module,
                arguments,
                ..
            } = &parsed.statement
            else {
                continue;
            };
            let name = parsed.ast.text(*name).to_vec();
            let module = parsed.ast.text(*module).to_vec();
            let arguments: Vec<Vec<u8>> = arguments.clone();
            let Some(found) = self.registry.module(&module) else {
                // A file naming a module this build does not have is a file
                // this build cannot answer for. It is skipped rather than
                // refused so the rest of the database still opens, and the
                // table itself will say "no such table" if anybody asks.
                continue;
            };
            let mut connect = ModuleArguments {
                database: 0,
                schema: b"main".to_vec(),
                table: name.clone(),
                module: module.clone(),
                arguments,
                shadows: Vec::new(),
            };
            for shadow in found.shadow_tables(&connect)? {
                // Looked up in the catalog rows rather than through
                // `table_root`, which answers over the tables the *planner* can
                // see. A shadow table is a real tree either way, and the row is
                // the authority for what it is registered under.
                let owned = shadow.owner.clone().unwrap_or_else(|| name.clone());
                let shadow_name = shadow_table_name(&owned, &shadow.suffix).to_ascii_lowercase();
                let Some(recorded) = self
                    .entries
                    .iter()
                    .find(|recorded| recorded.entry.name.to_ascii_lowercase() == shadow_name)
                else {
                    // **Refused rather than skipped.** A module connected
                    // without one of its shadow tables is a module that will
                    // answer wrongly rather than fail - it may even create a
                    // second copy of the table it could not find - so a shadow
                    // the catalog does not name stops the open and says which
                    // one. This is how a catalog that had lost rows was found:
                    // the connect went ahead without them.
                    return Err(refusal(format!(
                        "the catalog does not name {}, which {} needs",
                        String::from_utf8_lossy(&shadow_name),
                        String::from_utf8_lossy(&name)
                    )));
                };
                connect.shadows.push(ShadowRoot {
                    suffix: shadow.suffix.clone(),
                    root: recorded.root,
                });
            }
            let table = found.connect(&connect, false)?;
            self.virtual_tables.insert(
                name.to_ascii_lowercase(),
                Connected {
                    table,
                    arguments: connect,
                },
            );
        }
        Ok(())
    }

    /// Applies one change to a virtual table.
    ///
    /// @param name - the table's name
    /// @param change - what to do
    pub(super) fn change_module(&mut self, name: &[u8], change: &Change) -> DbResult<Option<i64>> {
        // **The first write to any module opens the transaction on all of them
        // (task-1932, M2).** Before the flag existed there was no moment at
        // which a module could start buffering, because the engine's only
        // `begin` was at `CREATE VIRTUAL TABLE`. Told before the table is taken
        // out of the map, so the module being written hears it too.
        if !self.modules_begun.get() {
            self.modules_begun.set(true);
            self.begin_modules()?;
        }
        let key = name.to_ascii_lowercase();
        let mut connected = self
            .virtual_tables
            .remove(&key)
            .ok_or_else(|| refusal(format!("no such table: {}", String::from_utf8_lossy(name))))?;
        let outcome = {
            let txn = self.current_txn();
            let at = self.ddl_schema;
            let session = self.session.get();
            let wal = self
                .log_of(at)
                .ok_or_else(|| refusal("a statement names a database that is not attached"))?;
            let mut log = WalLog {
                wal,
                txn,
                schema: at,
                wrote: false,
                // **The before-images a rollback needs.** Every ordinary write
                // passes `Some(&self.undo)`; this path passed `None`, so a
                // virtual table's writes went into the pool with nothing
                // recorded that could put them back. `ROLLBACK` then undid
                // every ordinary table and left the module's shadow trees as
                // the abandoned transaction had made them - so the connection
                // read two rows where the file held one, and a reopen was the
                // only thing that corrected it. The file itself was never
                // wrong: no commit record was written, so recovery ignored
                // the pages. Only the live connection was.
                undo: Some(&self.undo),
                uncommitted: self.uncommitted_handle_of(at),
            };
            let store = WriteStore {
                database: super::file_of(
                    &mut self.database,
                    &mut self.attached,
                    &mut self.temps,
                    session,
                    at,
                )?,
                trees: &mut self.trees,
                log: &mut log,
            };
            let mut nowhere = inillucent_ext::vtab::WithStore { store };
            let mut context = Context {
                host: &mut nowhere,
                database: 0,
                limits: &self.limits,
                catalog: Some(&self.catalog),
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
    // **An empty suffix is the table itself.** A module that asks for a shadow
    // with no suffix is asking for the named table rather than for one derived
    // from it, which is what an external-content FTS5 index needs: its rows are
    // in `c`, not in `c_content`, and there is no other way to say so through a
    // contract whose whole vocabulary is suffixes.
    if suffix.is_empty() {
        return table.to_vec();
    }
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
    /// Applies an `UPDATE` to a virtual table, one row at a time.
    ///
    /// **A module owns its storage, so an update is a replacement.** The row is
    /// read back through an ordinary query - which is the module's own cursor,
    /// so there is no second reader of its format - the assignments are applied
    /// over it, and the whole row is handed back as one `Change::Update`. That
    /// is what `xUpdate` takes and what an append-only index does with an edit:
    /// tombstone the old version, append the new one.
    ///
    /// The hidden columns are left NULL. They are the module's query interface -
    /// `k`, `vector`, `rank` on a search table - and are not values a row holds.
    ///
    /// @param statement - the bound update
    /// @param keys - the rowid of each row the `WHERE` selected
    /// @param params - the bound parameters
    pub(super) fn update_module(
        &mut self,
        statement: &inillucent_sql::dml::BoundUpdate,
        keys: &[Vec<inillucent_tree::datum::OwnedDatum>],
        params: &inillucent_exec::physical::Params,
    ) -> DbResult<usize> {
        let table = statement.table.clone();
        let width = table.columns.len();
        // The columns a row actually holds, by declared position, and the query
        // that reads them.
        let visible: Vec<usize> = (0..width)
            .filter(|at| {
                table
                    .column(*at as u16)
                    .is_some_and(|column| !column.hidden)
            })
            .collect();
        let projection = visible
            .iter()
            .filter_map(|at| table.column(*at as u16))
            .map(|column| {
                format!(
                    "\"{}\"",
                    String::from_utf8_lossy(&column.name).replace('"', "\"\"")
                )
            })
            .collect::<Vec<String>>()
            .join(", ");
        let quoted = String::from_utf8_lossy(&table.name).replace('"', "\"\"");
        let mut changed = 0usize;
        for key in keys {
            let Some(&inillucent_tree::datum::OwnedDatum::Int(rowid)) = key.first() else {
                continue;
            };
            let held = self.execute_any(
                &format!("SELECT {projection} FROM \"{quoted}\" WHERE rowid = {rowid}"),
                &inillucent_exec::physical::Params::new(),
            )?;
            let mut values = vec![Value::Null; width];
            if let Some(row) = held.rows.first() {
                for (position, at) in visible.iter().enumerate() {
                    if let (Some(slot), Some(value)) = (values.get_mut(*at), row.get(position)) {
                        *slot = inillucent_exec::scalar::to_value(value.borrow());
                    }
                }
            }
            for assignment in &statement.assignments {
                // **A constant, because a module's row is not in scope here.**
                // `SET body = body || '!'` reads the row being replaced, which
                // the ordinary write path evaluates against the row image it
                // holds; this path has no such image, and answering with the
                // wrong value would be worse than saying so.
                let value = inillucent_exec::physical::literal_value(&assignment.value, params)
                    .map_err(|_| {
                        refusal(
                            "an UPDATE of a virtual table assigns a constant; \
                             an expression over the row being replaced is not supported",
                        )
                    })?;
                if let Some(slot) = values.get_mut(usize::from(assignment.column)) {
                    *slot = inillucent_exec::scalar::to_value(value.borrow());
                }
            }
            self.change_module(
                &table.name,
                &Change::Update {
                    old_rowid: Value::Integer(rowid),
                    new_rowid: Value::Integer(rowid),
                    values,
                },
            )?;
            changed = changed.saturating_add(1);
        }
        Ok(changed)
    }

    pub(super) fn insert_into_module(
        &mut self,
        statement: &inillucent_sql::dml::BoundInsert,
        params: &inillucent_exec::physical::Params,
    ) -> DbResult<Outcome> {
        let inillucent_sql::dml::BoundInsertSource::Values(values) = &statement.source else {
            return Err(refusal("an INSERT ... SELECT into a virtual table"));
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
            // **A supplied rowid is the caller's, not the module's to choose.**
            // This passed `Null` unconditionally, so `INSERT INTO t(rowid, ...)
            // VALUES (?1, ...)` was accepted and the rowid silently discarded -
            // the module allocated its own, and every row came back under a
            // number the caller had not written. It surfaced as a migrated
            // search index whose every ranking was correct and whose every
            // identifier was one too high, because the source numbered its
            // chunks from zero and the module numbered them from one.
            let rowid = match (statement.named_rowid, &statement.rowid) {
                // `INSERT INTO t(rowid, ...)`, which the binder records apart
                // from the columns because a rowid is not one: nothing writes
                // it into the record. It is the only place the value appears -
                // no `ColumnSource` refers to it - so reading `statement.rowid`
                // alone found nothing and the value was dropped on the floor.
                (Some(at), _) => supplied.get(at).cloned().unwrap_or(Value::Null),
                (None, Some(inillucent_sql::dml::ColumnSource::Row(at))) => {
                    supplied.get(*at).cloned().unwrap_or(Value::Null)
                }
                (None, Some(inillucent_sql::dml::ColumnSource::Expr(expr))) => {
                    inillucent_exec::scalar::to_value(
                        inillucent_exec::physical::literal_value(expr, params)?.borrow(),
                    )
                }
                // Nothing named one, so the module allocates - which is what
                // `Null` asks it for.
                (None, Some(inillucent_sql::dml::ColumnSource::Generated(_)) | None) => Value::Null,
            };
            // **Tried gating this on `change_module`'s `Option<i64>` return -
            // `Some` for an ordinary content row, `None` for a command, on the
            // theory that a command never counts as a change.** That broke
            // three passing cases: SQLite's own `changes()` reports 1 for a
            // *recognised* command (`'pgsz'`, `'rebuild'`,
            // `'integrity-check'`) and only 0 for one SQLite itself refuses -
            // `crates\inillucent-compat\tests\fts5.rs`'s
            // `an_unknown_command_is_refused`. Both engines answer `ok:
            // false` there, so the count is right and the count is what has
            // to answer 0: this loop errors out through the `?` below before
            // `changed` moves, past the `record_changes` call after it, and
            // `changes()` then read whatever the *previous* statement had
            // left - measured at 1, from the schema's last successful
            // single-row insert, where the reference answers 0 for a
            // statement that changed nothing. Recording here, on the way out,
            // is what makes a refused command's `changes()` its own instead
            // of an earlier statement's leftover.
            if let Err(error) = self.change_module(
                &statement.table.name,
                &Change::Insert {
                    rowid,
                    values: cells,
                },
            ) {
                self.record_changes(changed as i64, changed as i64);
                return Err(error);
            }
            changed = changed.saturating_add(1);
        }
        // Outside a transaction the statement is its own, so the module flushes
        // and the log commits here; inside one, `commit_batch` does both.
        if self.batch.get().is_none() {
            self.sync_modules()?;
            self.seal()?;
        }
        // **A module's insert changed rows exactly as much as an ordinary
        // one.** `changes()`/`total_changes()` read `last_changes`/
        // `changed_ever`, and nothing on this path used to touch either -
        // `Outcome::changes` was set correctly and nobody after this call ever
        // read it, since the SQL-level `changes()` and `total_changes()`
        // built-ins read the connection's own counters instead. A module has
        // no triggers of its own, so every row this loop counted is both this
        // statement's own change and the whole of what it changed.
        self.record_changes(changed as i64, changed as i64);
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
/// The comparisons are the dialect's, under the column's own collation.
/// `LIKE`, `GLOB` and `REGEXP` are evaluated here with the same implementations
/// the pipeline's own residual uses, because the engine *can* evaluate them and
/// refusing them turned `SELECT value FROM json_each('[\"aa\"]') WHERE value
/// LIKE 'a%'` - a statement SQLite answers - into an error. `MATCH` is the one
/// that stays refused: it is the module's own operator and has no meaning
/// outside it, so answering as though it had been applied would return rows the
/// query excluded.
///
/// @param held - the value the module produced
/// @param op - the operator the constraint carries
/// @param wanted - the value on the other side
/// @param collation - the column's collation
/// @param case_sensitive - `PRAGMA case_sensitive_like`
fn satisfies(
    held: &OwnedDatum,
    op: inillucent_sql::vtab::ConstraintOp,
    wanted: &OwnedDatum,
    collation: inillucent_value::Collation,
    case_sensitive: bool,
) -> DbResult<bool> {
    use inillucent_sql::vtab::ConstraintOp;
    use std::cmp::Ordering;
    let left = inillucent_exec::scalar::to_value(held.borrow());
    let right = inillucent_exec::scalar::to_value(wanted.borrow());
    if matches!(left, Value::Null) || matches!(right, Value::Null) {
        // A comparison against NULL is unknown, which excludes the row.
        return Ok(false);
    }
    // A pattern reads both sides as text, which is what the pipeline's own
    // `Pattern` expression does; the ordering below would compare a number to a
    // pattern string and answer nonsense.
    let pattern = match op {
        ConstraintOp::Like => Some(inillucent_exec::scalar::PatternOperator::Like),
        ConstraintOp::Glob => Some(inillucent_exec::scalar::PatternOperator::Glob),
        ConstraintOp::Regexp => Some(inillucent_exec::scalar::PatternOperator::Regexp),
        _ => None,
    };
    if let Some(pattern) = pattern {
        return Ok(inillucent_exec::scalar::matches_pattern(
            pattern,
            &left,
            &right,
            case_sensitive,
        ));
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
            return Err(refusal(format!(
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
                let at = self.ddl_schema;
                let session = self.session.get();
                let wal = self
                    .log_of(at)
                    .ok_or_else(|| refusal("a statement names a database that is not attached"))?;
                let mut log = WalLog {
                    wal,
                    txn,
                    schema: at,
                    wrote: false,
                    // A flush is part of the transaction that asked for it -
                    // at a commit the buffer is cleared immediately after, and
                    // at a savepoint these writes are exactly what a later
                    // `ROLLBACK TO` an earlier point has to be able to undo.
                    undo: Some(&self.undo),
                    uncommitted: self.uncommitted_handle_of(at),
                };
                let store = WriteStore {
                    database: super::file_of(
                        &mut self.database,
                        &mut self.attached,
                        &mut self.temps,
                        session,
                        at,
                    )?,
                    trees: &mut self.trees,
                    log: &mut log,
                };
                let mut nowhere = inillucent_ext::vtab::WithStore { store };
                let mut context = Context {
                    host: &mut nowhere,
                    database: 0,
                    limits: &self.limits,
                    catalog: Some(&self.catalog),
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

    /// Tells every connected module that the transaction was abandoned.
    ///
    /// **The half of the contract the new engine never held up.** The module
    /// trait has had `rollback` since the old engine, and
    /// `inillucent-session/src/vtab.rs` dispatches `Moment::Rollback` to it -
    /// but this engine only ever called `begin`, `sync` and `commit`. A module
    /// that buffers therefore never heard that its buffer was void.
    ///
    /// The file was always right: shadow tables are ordinary trees, so the undo
    /// log put them back. What was wrong was the *connection*, which went on
    /// reading the module's staging area - so one query answered differently
    /// before and after a reopen with nothing written in between, in whichever
    /// direction the abandoned transaction had written. Measured against the
    /// pinned SQLite 3.53.4 before the fix: an abandoned insert left an `fts5`
    /// table reading 2 where the file held 1, and an abandoned delete left it
    /// reading 0 where the file held 2.
    ///
    /// A module that fails to abandon its buffer is not allowed to stop the
    /// rollback - the transaction is going away either way, and a rollback that
    /// could itself fail would leave the connection in a state with no name. The
    /// first failure is remembered and returned once every module has been told.
    ///
    /// @param to_savepoint - the savepoint level, or nothing for the whole
    ///     transaction
    pub(super) fn rollback_modules(&mut self, to_savepoint: Option<i32>) -> DbResult<()> {
        match to_savepoint {
            Some(level) => self.tell_modules(Moment::RollbackTo(level)),
            None => self.tell_modules(Moment::Rollback),
        }
    }

    /// Tells every connected module that a write transaction has started.
    ///
    /// **Called once per transaction that reaches a module, and before this it
    /// was called once per `CREATE VIRTUAL TABLE` (task-1932, M2).** The only
    /// `begin` in the engine was at creation, so a module that wanted to buffer
    /// a transaction's writes had no moment at which to start one - FTS5's own
    /// `begin` gates on `self.creating` and does nothing afterwards, which is
    /// what a module writes when the hook only ever fires at creation.
    ///
    /// Every connected module is told rather than only the one being written,
    /// which is the same set `sync_modules` flushes at the commit. A module
    /// that begins and is never written syncs nothing.
    pub(super) fn begin_modules(&mut self) -> DbResult<()> {
        self.tell_modules(Moment::Begin)
    }

    /// Tells every connected module that a savepoint was opened.
    ///
    /// @param level - how many savepoints were already open
    pub(super) fn savepoint_modules(&mut self, level: i32) -> DbResult<()> {
        self.tell_modules(Moment::Savepoint(level))
    }

    /// Tells every connected module that savepoints above a level were
    /// released.
    ///
    /// @param level - the level being released down to
    pub(super) fn release_modules(&mut self, level: i32) -> DbResult<()> {
        self.tell_modules(Moment::Release(level))
    }

    /// Tells every connected module that the schema changed under it.
    ///
    /// Infallible, because it is called from `refresh_catalog`, which is called
    /// from paths that have already committed to what they did. A module that
    /// wanted to refuse a schema change would have had to refuse the statement
    /// that made it.
    pub(super) fn schema_changed_modules(&mut self) {
        let names: Vec<Vec<u8>> = self.virtual_tables.keys().cloned().collect();
        for name in names {
            if let Some(connected) = self.virtual_tables.get_mut(&name) {
                connected.table.schema_changed();
            }
        }
    }

    /// Tells every connected module that another process committed.
    ///
    /// Infallible for the same reason: the reload has already happened, and a
    /// module's opinion about it cannot put the pages back.
    pub(super) fn committed_elsewhere_modules(&mut self) {
        let names: Vec<Vec<u8>> = self.virtual_tables.keys().cloned().collect();
        for name in names {
            if let Some(connected) = self.virtual_tables.get_mut(&name) {
                connected.table.committed_elsewhere();
            }
        }
    }

    /// Tells every connected module about one moment.
    ///
    /// **One loop, because the set is always the same set.** A moment told to
    /// some modules and not others is how `begin` came to fire at creation and
    /// nowhere else: there was no loop, only a call beside the thing that
    /// happened.
    ///
    /// A module that fails is not allowed to stop the others being told - a
    /// transaction that is ending is ending either way - so the first failure
    /// is remembered and returned once every module has heard.
    ///
    /// @param moment - what happened
    fn tell_modules(&mut self, moment: Moment) -> DbResult<()> {
        let names: Vec<Vec<u8>> = self.virtual_tables.keys().cloned().collect();
        let mut first_failure: Option<inillucent_base::DbError> = None;
        for name in names {
            let Some(mut connected) = self.virtual_tables.remove(&name) else {
                continue;
            };
            let outcome = self.tell_one_module(&mut connected, moment);
            self.virtual_tables.insert(name, connected);
            if let Err(why) = outcome {
                if first_failure.is_none() {
                    first_failure = Some(why);
                }
            }
        }
        match first_failure {
            Some(why) => Err(why),
            None => Ok(()),
        }
    }

    /// Tells one module the transaction was abandoned.
    ///
    /// Split out so `rollback_modules` can hold the table out of the map across
    /// the call - a module may read its own shadow trees while it discards, and
    /// the map is borrowed for the walk.
    ///
    /// @param connected - the module and the arguments it was connected with
    /// @param to_savepoint - the savepoint level, or nothing for the whole
    ///     transaction
    fn tell_one_module(&mut self, connected: &mut Connected, moment: Moment) -> DbResult<()> {
        let txn = self.current_txn();
        let at = self.ddl_schema;
        let session = self.session.get();
        let Some(wal) = self.log_of(at) else {
            return Ok(());
        };
        let mut log = WalLog {
            wal,
            txn,
            schema: at,
            wrote: false,
            undo: None,
            uncommitted: self.uncommitted_handle_of(at),
        };
        let store = WriteStore {
            database: super::file_of(
                &mut self.database,
                &mut self.attached,
                &mut self.temps,
                session,
                at,
            )?,
            trees: &mut self.trees,
            log: &mut log,
        };
        let mut nowhere = inillucent_ext::vtab::WithStore { store };
        let mut context = Context {
            host: &mut nowhere,
            database: 0,
            limits: &self.limits,
            catalog: Some(&self.catalog),
        };
        match moment {
            Moment::Begin => connected.table.begin(&mut context),
            Moment::Rollback => connected.table.rollback(&mut context),
            Moment::RollbackTo(level) => connected.table.rollback_to(&mut context, level),
            Moment::Savepoint(level) => connected.table.savepoint(&mut context, level),
            Moment::Release(level) => connected.table.release(&mut context, level),
        }
    }
}

/// Renders a bound value as the text a pragma reader reads.
///
/// @param value - the value the statement supplied as the argument
fn pragma_argument_text(value: &OwnedDatum) -> Vec<u8> {
    match value {
        OwnedDatum::Null => Vec::new(),
        OwnedDatum::Int(number) => number.to_string().into_bytes(),
        OwnedDatum::Real(number) => inillucent_value::numeric::real_to_text(*number),
        OwnedDatum::Text(bytes) | OwnedDatum::Blob(bytes) => bytes.clone(),
    }
}
