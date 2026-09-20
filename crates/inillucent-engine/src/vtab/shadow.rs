//! The tables a module keeps its own storage in.
//!
//! Invariant: **a module's shadow table is an ordinary tree, written through
//! the ordinary write path.** That is what puts a virtual table inside the
//! transaction it was written in: an FTS5 index that kept its postings
//! somewhere else would commit and roll back separately from the rows it
//! indexes, which is the defect task-1932 fixed.
//!
//! [`ReadStore`] refuses every write, which is what a read-only path is handed;
//! [`WriteStore`] is the same surface with the writes implemented.

use std::collections::HashMap;

use inillucent_base::error::refusal;
use inillucent_base::DbResult;
use inillucent_ext::vtab::Host;
use inillucent_pool::{Database, Pool};
use inillucent_sql::vtab::ShadowStore;
use inillucent_tree::datum::{owned_row_values, Datum, OwnedDatum};
use inillucent_tree::write::TreeLog;
use inillucent_tree::PagedTree;
use inillucent_value::Value;

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
/// Adds one shadow row write to this thread's tally.
///
/// @param borrowed - nanoseconds spent borrowing the row as the tree's data
/// @param put - nanoseconds spent writing it
fn record_shadow_write(borrowed: u128, put: u128) {
    super::stages::record(|stages| {
        stages.shadow_writes = stages.shadow_writes.saturating_add(1);
        stages.datums = stages.datums.saturating_add(borrowed);
        stages.put = stages.put.saturating_add(put);
    });
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
            let values = match owned_row_values(&owned) {
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
            let values = match owned_row_values(&owned) {
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
            let values = match owned_row_values(&owned_row) {
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
        // **Timed here because this is where a virtual table's time turned out
        // to be (task-2025).** `extension.fts.build` writes two shadow rows a
        // document - `%_content` and `%_docsize` - and one `%_idx` row a term at
        // the flush: 1,508 writes for 500 documents, `6.32 ms` of the `10.2 ms`
        // one round costs, 62% of it. The module's own stage line charges them to
        // `content`, `docsize` and `flush` and cannot say which half of a shadow
        // write they are, and the answer is that `borrowed_bytes` and
        // `as_datums` are `0.09 ms` of the 6.41 and `tree.put` is the rest.
        let borrowing = super::stages::clock();
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
        let borrowed = super::stages::elapsed(borrowing);
        let writing = super::stages::clock();
        // Recorded before the `?`, so a refused write is still a write that was
        // paid for: a counter that only sees the successes cannot be compared
        // against a workload's own clock, which saw both.
        let answer = tree.put(self.database, self.log, &row);
        record_shadow_write(borrowed, super::stages::elapsed(writing));
        answer?;
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
        // See `write_row`: the same two halves, on the path FTS5's dictionary
        // flush takes.
        let borrowing = super::stages::clock();
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
        let borrowed = super::stages::elapsed(borrowing);
        let writing = super::stages::clock();
        // Recorded before the `?`, so a refused write is still a write that was
        // paid for: a counter that only sees the successes cannot be compared
        // against a workload's own clock, which saw both.
        let answer = tree.put(self.database, self.log, &row);
        record_shadow_write(borrowed, super::stages::elapsed(writing));
        answer?;
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
