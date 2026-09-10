//! The old engine's shadow tables, behind the interface both engines share.
//!
//! Invariant: **a module reaches its shadow rows through one trait, and this is
//! the retired engine's implementation of it.** `inillucent_sql::vtab::ShadowStore`
//! is what FTS5 and the R-Tree call; the new engine implements it over PAX
//! trees, and this implements it over a `Pager` and SQLite-format b-trees.
//!
//! ## Why it moved here
//!
//! It used to live in `inillucent-ext`, as a second arm inside every method of
//! `ShadowTables`: the store when the caller supplied one, and a pager
//! otherwise. That is what made `inillucent-ext` - a crate the *new* engine
//! links - depend on `inillucent-storage`, which the rearchitecture retired.
//! The review that found it put it plainly: keeping two storage models in the
//! shipped graph increases the number of invariants an engineer must preserve
//! and makes ownership of recovery behaviour harder to establish.
//!
//! The code is the same code. What changed is which side of the trait it is on,
//! and therefore which crate has to name `inillucent-storage` - which is now
//! only the crates that are retired along with it. When the old engine is
//! deleted this file goes with it and nothing above the trait notices.

use inillucent_base::limits::Limits;
use inillucent_base::DbResult;
use inillucent_sql::vtab::ShadowStore;
use inillucent_storage::cursor::{BTreeCursor, SeekBias};
use inillucent_storage::{mutate, Pager};
use inillucent_value::{record, TextEncoding, Value};

/// A pager, and the limits a payload is read under, as a shadow store.
///
/// It borrows the pager rather than owning one: a shadow read happens inside a
/// statement that already holds the pager, and a store that took a copy would
/// be reading a different database part-way through a write.
pub struct PagerShadowStore<'p> {
    /// The pager the shadow tables live behind.
    pager: &'p mut Pager,
    /// The run-time limits a payload is read under.
    limits: Limits,
}

impl<'p> PagerShadowStore<'p> {
    /// Wraps a pager as a shadow store.
    ///
    /// @param pager - the pager the shadow tables live behind
    /// @param limits - the run-time limits a payload is read under
    pub fn new(pager: &'p mut Pager, limits: Limits) -> PagerShadowStore<'p> {
        PagerShadowStore { pager, limits }
    }
}

/// Returns the ordering a keyed shadow table's b-tree is in.
///
/// Every key column compares as `BINARY` and ascending, which is what a
/// `PRIMARY KEY` with no `COLLATE` and no `DESC` declares - and every shadow
/// table declares exactly that.
///
/// @param columns - how many leading columns form the key
fn key_info(columns: usize) -> record::KeyInfo {
    record::KeyInfo {
        columns: (0..columns)
            .map(|_| record::KeyColumn {
                collation: inillucent_value::Collation::Binary,
                descending: false,
            })
            .collect(),
    }
}

/// Returns one row's values, with the rowid in the first column.
///
/// A rowid table stores a NULL where its `INTEGER PRIMARY KEY` column would be
/// and keeps the value on the cell instead, so the row a caller sees has to be
/// put back together from the two.
///
/// @param rowid - the row's key
/// @param record - the stored record
fn row_values(rowid: i64, record: &record::RecordRef<'_>) -> DbResult<Vec<Value<'static>>> {
    let mut values = vec![Value::Integer(rowid)];
    for value in record.values()?.into_iter().skip(1) {
        values.push(value.into_owned()?);
    }
    Ok(values)
}

/// Returns the page a root number names.
///
/// @param root - the shadow table's root, as the catalog stored it
fn page_of(root: u32) -> DbResult<inillucent_base::ids::PageId> {
    inillucent_base::ids::PageId::from_persisted(root)
}

impl ShadowStore for PagerShadowStore<'_> {
    /// Reads one row by rowid, or nothing when there is not one.
    fn read_row(&mut self, root: u32, rowid: i64) -> DbResult<Option<Vec<Value<'static>>>> {
        let mut cursor = BTreeCursor::table(page_of(root)?);
        if !cursor.seek_rowid(self.pager, rowid, SeekBias::AtOrAfter)? || cursor.rowid()? != rowid {
            return Ok(None);
        }
        let payload = cursor.payload(self.pager, &self.limits)?;
        let record = record::RecordRef::parse(&payload, TextEncoding::Utf8)?;
        Ok(Some(row_values(rowid, &record)?))
    }

    /// Writes one row by rowid, replacing whatever was there.
    fn write_row(&mut self, root: u32, rowid: i64, values: &[Value<'static>]) -> DbResult<()> {
        // The first value is the rowid, which a rowid table stores as NULL in
        // the record and reads back off the cell. Every shadow table declares
        // its first column `INTEGER PRIMARY KEY`, so this is the same record
        // SQLite writes for the same row.
        let mut stored = values.to_vec();
        if let Some(first) = stored.first_mut() {
            *first = Value::Null;
        }
        let payload = record::encode_record(&stored, TextEncoding::Utf8, 4)?;
        mutate::insert_row(self.pager, page_of(root)?, rowid, &payload)
    }

    /// Removes one row by rowid, reporting nothing when there was not one.
    fn delete_row(&mut self, root: u32, rowid: i64) -> DbResult<()> {
        mutate::delete_row(self.pager, page_of(root)?, rowid)?;
        Ok(())
    }

    /// Returns the largest rowid one shadow table holds.
    fn max_rowid(&mut self, root: u32) -> DbResult<i64> {
        let mut cursor = BTreeCursor::table(page_of(root)?);
        if !cursor.last(self.pager)? {
            return Ok(0);
        }
        cursor.rowid()
    }

    /// Runs a body over every row of one shadow table, in rowid order.
    fn scan(
        &mut self,
        root: u32,
        body: &mut dyn FnMut(i64, &[Value<'static>]) -> DbResult<bool>,
    ) -> DbResult<()> {
        let mut cursor = BTreeCursor::table(page_of(root)?);
        if !cursor.first(self.pager)? {
            return Ok(());
        }
        loop {
            let rowid = cursor.rowid()?;
            let payload = cursor.payload(self.pager, &self.limits)?;
            let record = record::RecordRef::parse(&payload, TextEncoding::Utf8)?;
            let values = row_values(rowid, &record)?;
            if !body(rowid, &values)? {
                return Ok(());
            }
            if !cursor.next(self.pager)? {
                return Ok(());
            }
        }
    }

    /// Reads one row of a keyed shadow table, or nothing when there is not one.
    ///
    /// A `WITHOUT ROWID` table's b-tree holds the whole row as its key, ordered
    /// by the primary key's columns - so a lookup is a seek on a record and
    /// what comes back is the record itself.
    fn read_keyed(
        &mut self,
        root: u32,
        key: &[Value<'static>],
        columns: usize,
    ) -> DbResult<Option<Vec<Value<'static>>>> {
        let info = key_info(key.len());
        let mut cursor = BTreeCursor::index(page_of(root)?, info);
        if !cursor.seek_index(self.pager, key, SeekBias::AtOrAfter)? {
            return Ok(None);
        }
        let payload = cursor.payload(self.pager, &self.limits)?;
        let record = record::RecordRef::parse(&payload, TextEncoding::Utf8)?;
        let values = record.values()?;
        for (position, wanted) in key.iter().enumerate() {
            let Some(found) = values.get(position) else {
                return Ok(None);
            };
            if inillucent_value::compare::compare_values(
                found,
                wanted,
                inillucent_value::Collation::Binary,
            ) != core::cmp::Ordering::Equal
            {
                return Ok(None);
            }
        }
        // `columns` may be `usize::MAX`, meaning "however many are there" - so
        // the capacity is what the record actually holds, not what was asked
        // for.
        let mut owned = Vec::with_capacity(values.len().min(columns));
        for value in values.into_iter().take(columns) {
            owned.push(value.into_owned()?);
        }
        Ok(Some(owned))
    }

    /// Writes one row of a keyed shadow table, replacing whatever was there.
    fn write_keyed(
        &mut self,
        root: u32,
        key_columns: usize,
        values: &[Value<'static>],
    ) -> DbResult<()> {
        let key: Vec<Value<'static>> = values.iter().take(key_columns).cloned().collect();
        self.delete_keyed(root, &key)?;
        let payload = record::encode_record(values, TextEncoding::Utf8, 4)?;
        let info = key_info(key_columns);
        mutate::insert_entry(self.pager, page_of(root)?, &info, &payload)?;
        Ok(())
    }

    /// Removes one row of a keyed shadow table.
    fn delete_keyed(&mut self, root: u32, key: &[Value<'static>]) -> DbResult<()> {
        let Some(existing) = self.read_keyed(root, key, usize::MAX)? else {
            return Ok(());
        };
        let payload = record::encode_record(&existing, TextEncoding::Utf8, 4)?;
        let info = key_info(key.len());
        mutate::delete_entry(self.pager, page_of(root)?, &info, &payload)?;
        Ok(())
    }

    /// Runs a body over every row of a keyed shadow table, in key order.
    fn scan_keyed(
        &mut self,
        root: u32,
        key_columns: usize,
        body: &mut dyn FnMut(&[Value<'static>]) -> DbResult<bool>,
    ) -> DbResult<()> {
        let info = key_info(key_columns);
        let mut cursor = BTreeCursor::index(page_of(root)?, info);
        if !cursor.first(self.pager)? {
            return Ok(());
        }
        loop {
            let payload = cursor.payload(self.pager, &self.limits)?;
            let record = record::RecordRef::parse(&payload, TextEncoding::Utf8)?;
            let mut values = Vec::new();
            for value in record.values()? {
                values.push(value.into_owned()?);
            }
            if !body(&values)? {
                return Ok(());
            }
            if !cursor.next(self.pager)? {
                return Ok(());
            }
        }
    }
}
