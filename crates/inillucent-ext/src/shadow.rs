//! Reading and writing a module's own shadow tables.
//!
//! Invariant: a module reaches exactly the tables it was told about, by the
//! roots it was handed, and no others. There is no name resolution here and no
//! catalog: `ShadowTables` is a list of root pages that the host looked up once
//! and gave to the module, so a module that wanted to read somebody else's
//! table would have to be handed it.
//!
//! The rows are ordinary rows in ordinary b-trees, which is what makes a
//! module's storage visible to `PRAGMA integrity_check`, to `VACUUM`, and to
//! the other engine. FTS5 and R-Tree both keep their whole state this way, and
//! it is why a database either of them writes can be opened by SQLite.

use std::collections::BTreeMap;

use inillucent_base::ids::PageId;
use inillucent_base::{error, DbResult};
use inillucent_storage::cursor::{BTreeCursor, SeekBias};
use inillucent_storage::mutate;
use inillucent_value::{record, TextEncoding, Value};

use crate::vtab::{Context, ModuleArguments};

/// The root pages of one module's shadow tables, by the suffix that names them.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ShadowTables {
    roots: BTreeMap<Vec<u8>, u32>,
}

impl ShadowTables {
    /// Returns the roots of the tables a module needs, refusing a missing one.
    ///
    /// A missing shadow table is a corrupt schema rather than a state to cope
    /// with: the module's whole storage is those tables, and one that is not
    /// there means the rows are not there either.
    pub fn of(arguments: &ModuleArguments, needed: &[&[u8]]) -> DbResult<ShadowTables> {
        let mut roots = BTreeMap::new();
        for suffix in needed {
            let Some(root) = arguments.shadow(suffix) else {
                return Err(error::corrupt(format!(
                    "the shadow table {}_{} is missing",
                    String::from_utf8_lossy(&arguments.table),
                    String::from_utf8_lossy(suffix)
                )));
            };
            roots.insert(suffix.to_vec(), root);
        }
        Ok(ShadowTables { roots })
    }

    /// Returns one shadow table's root page.
    pub fn root(&self, suffix: &[u8]) -> DbResult<PageId> {
        let Some(root) = self.roots.get(suffix).copied() else {
            return Err(error::misuse(format!(
                "no shadow table {}",
                String::from_utf8_lossy(suffix)
            )));
        };
        PageId::from_persisted(root)
    }

    /// Reads one row by rowid, or nothing when there is not one.
    ///
    /// The row comes back with its rowid in place of the first column, because
    /// a rowid table's first column *is* the rowid when it is declared
    /// `INTEGER PRIMARY KEY` - and every shadow table here is.
    pub fn read_row(
        &self,
        context: &mut Context<'_>,
        suffix: &[u8],
        rowid: i64,
    ) -> DbResult<Option<Vec<Value<'static>>>> {
        let root = self.root(suffix)?;
        let limits = context.limits.clone();
        let pager = context.host.pager(context.database)?;
        let mut cursor = BTreeCursor::table(root);
        if !cursor.seek_rowid(pager, rowid, SeekBias::AtOrAfter)? || cursor.rowid()? != rowid {
            return Ok(None);
        }
        let payload = cursor.payload(pager, &limits)?;
        let record = record::RecordRef::parse(&payload, TextEncoding::Utf8)?;
        Ok(Some(row_values(rowid, &record)?))
    }

    /// Writes one row by rowid, replacing whatever was there.
    pub fn write_row(
        &self,
        context: &mut Context<'_>,
        suffix: &[u8],
        rowid: i64,
        values: &[Value<'static>],
    ) -> DbResult<()> {
        let root = self.root(suffix)?;
        // The first value is the rowid, which a rowid table stores as NULL in
        // the record and reads back off the cell. Every shadow table here
        // declares its first column `INTEGER PRIMARY KEY`, so this is the same
        // record SQLite writes for the same row.
        let mut stored = values.to_vec();
        if let Some(first) = stored.first_mut() {
            *first = Value::Null;
        }
        let payload = record::encode_record(&stored, TextEncoding::Utf8, 4)?;
        let pager = context.host.pager(context.database)?;
        mutate::insert_row(pager, root, rowid, &payload)
    }

    /// Removes one row by rowid, reporting nothing when there was not one.
    pub fn delete_row(&self, context: &mut Context<'_>, suffix: &[u8], rowid: i64) -> DbResult<()> {
        let root = self.root(suffix)?;
        let pager = context.host.pager(context.database)?;
        mutate::delete_row(pager, root, rowid)?;
        Ok(())
    }

    /// Returns the largest rowid one shadow table holds.
    pub fn max_rowid(&self, context: &mut Context<'_>, suffix: &[u8]) -> DbResult<i64> {
        let root = self.root(suffix)?;
        let pager = context.host.pager(context.database)?;
        let mut cursor = BTreeCursor::table(root);
        if !cursor.last(pager)? {
            return Ok(0);
        }
        cursor.rowid()
    }

    /// Runs a body over every row of one shadow table, in rowid order.
    pub fn scan(
        &self,
        context: &mut Context<'_>,
        suffix: &[u8],
        mut body: impl FnMut(i64, &[Value<'static>]) -> DbResult<bool>,
    ) -> DbResult<()> {
        let root = self.root(suffix)?;
        let limits = context.limits.clone();
        let pager = context.host.pager(context.database)?;
        let mut cursor = BTreeCursor::table(root);
        if !cursor.first(pager)? {
            return Ok(());
        }
        loop {
            let rowid = cursor.rowid()?;
            let payload = cursor.payload(pager, &limits)?;
            let record = record::RecordRef::parse(&payload, TextEncoding::Utf8)?;
            let values = row_values(rowid, &record)?;
            if !body(rowid, &values)? {
                return Ok(());
            }
            if !cursor.next(pager)? {
                return Ok(());
            }
        }
    }
}

impl ShadowTables {
    /// Reads one row of a keyed shadow table, or nothing when there is not one.
    ///
    /// A `WITHOUT ROWID` table's b-tree holds the whole row as its key, ordered
    /// by the primary key's columns - so a lookup is a seek on a record and
    /// what comes back is the record itself.
    pub fn read_keyed(
        &self,
        context: &mut Context<'_>,
        suffix: &[u8],
        key: &[Value<'static>],
        columns: usize,
    ) -> DbResult<Option<Vec<Value<'static>>>> {
        let root = self.root(suffix)?;
        let info = key_info(key.len());
        let limits = context.limits.clone();
        let pager = context.host.pager(context.database)?;
        let mut cursor = BTreeCursor::index(root, info);
        if !cursor.seek_index(pager, key, SeekBias::AtOrAfter)? {
            return Ok(None);
        }
        let payload = cursor.payload(pager, &limits)?;
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
    pub fn write_keyed(
        &self,
        context: &mut Context<'_>,
        suffix: &[u8],
        key_columns: usize,
        values: &[Value<'static>],
    ) -> DbResult<()> {
        let key: Vec<Value<'static>> = values.iter().take(key_columns).cloned().collect();
        self.delete_keyed(context, suffix, &key)?;
        let root = self.root(suffix)?;
        let payload = record::encode_record(values, TextEncoding::Utf8, 4)?;
        let info = key_info(key_columns);
        let pager = context.host.pager(context.database)?;
        mutate::insert_entry(pager, root, &info, &payload)?;
        Ok(())
    }

    /// Removes one row of a keyed shadow table.
    pub fn delete_keyed(
        &self,
        context: &mut Context<'_>,
        suffix: &[u8],
        key: &[Value<'static>],
    ) -> DbResult<()> {
        let Some(existing) = self.read_keyed(context, suffix, key, usize::MAX)? else {
            return Ok(());
        };
        let root = self.root(suffix)?;
        let payload = record::encode_record(&existing, TextEncoding::Utf8, 4)?;
        let info = key_info(key.len());
        let pager = context.host.pager(context.database)?;
        mutate::delete_entry(pager, root, &info, &payload)?;
        Ok(())
    }

    /// Runs a body over every row of a keyed shadow table, in key order.
    pub fn scan_keyed(
        &self,
        context: &mut Context<'_>,
        suffix: &[u8],
        key_columns: usize,
        mut body: impl FnMut(&[Value<'static>]) -> DbResult<bool>,
    ) -> DbResult<()> {
        let root = self.root(suffix)?;
        let info = key_info(key_columns);
        let limits = context.limits.clone();
        let pager = context.host.pager(context.database)?;
        let mut cursor = BTreeCursor::index(root, info);
        if !cursor.first(pager)? {
            return Ok(());
        }
        loop {
            let payload = cursor.payload(pager, &limits)?;
            let record = record::RecordRef::parse(&payload, TextEncoding::Utf8)?;
            let mut values = Vec::new();
            for value in record.values()? {
                values.push(value.into_owned()?);
            }
            if !body(&values)? {
                return Ok(());
            }
            if !cursor.next(pager)? {
                return Ok(());
            }
        }
    }
}

/// Returns the ordering a keyed shadow table's b-tree is in.
///
/// Every key column compares as `BINARY` and ascending, which is what a
/// `PRIMARY KEY` with no `COLLATE` and no `DESC` declares - and every shadow
/// table here declares exactly that.
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
fn row_values(rowid: i64, record: &record::RecordRef<'_>) -> DbResult<Vec<Value<'static>>> {
    let mut values = vec![Value::Integer(rowid)];
    for value in record.values()?.into_iter().skip(1) {
        values.push(value.into_owned()?);
    }
    Ok(values)
}

#[cfg(test)]
mod tests {
    use super::*;
    use inillucent_sql::vtab::ShadowRoot;

    /// Builds the arguments a module would be connected with.
    fn arguments(shadows: &[(&[u8], u32)]) -> ModuleArguments {
        ModuleArguments {
            database: 0,
            schema: b"main".to_vec(),
            table: b"t".to_vec(),
            module: b"rtree".to_vec(),
            arguments: Vec::new(),
            shadows: shadows
                .iter()
                .map(|(suffix, root)| ShadowRoot {
                    suffix: suffix.to_vec(),
                    root: *root,
                })
                .collect(),
        }
    }

    /// Every table a module names has to be there.
    #[test]
    fn a_missing_shadow_table_is_refused() {
        let arguments = arguments(&[(b"node", 4)]);
        assert!(ShadowTables::of(&arguments, &[b"node"]).is_ok());
        let missing = ShadowTables::of(&arguments, &[b"node", b"rowid"]);
        assert!(missing.is_err());
    }

    /// A root a module was not given is one it cannot reach.
    #[test]
    fn an_unlisted_table_cannot_be_reached() {
        let tables = ShadowTables::of(&arguments(&[(b"node", 4)]), &[b"node"]).expect("built");
        assert!(tables.root(b"node").is_ok());
        assert!(tables.root(b"secret").is_err());
    }
}
