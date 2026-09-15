//! Reading the fixture corpus, and scanning a database with inillucent alone.
//!
//! Invariant: everything here goes through the public `inillucent-storage` API, in
//! the order a real reader would. A helper that reached into a private field
//! to make a test pass would be testing a different engine from the one that
//! ships.
//!
//! The one piece of knowledge this module has that `inillucent-storage` does not
//! is what a column's declared type is. Phase 3 returns the value a record
//! *stores*; a column's declared type decides what a query *sees*, and the two
//! differ in exactly one documented place: SQLite writes an integral value in
//! a REAL-affinity column as an integer to save space and converts it back to
//! a double as it is read out. Bridging that here, rather than in storage,
//! keeps the layering honest - storage does not know what a column is called -
//! and it is why `declared_types` exists and does the crudest possible parse.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use inillucent_base::ids::PageId;
use inillucent_base::limits::Limits;
use inillucent_base::DbResult;
use inillucent_storage::cursor::{BTreeCursor, SeekBias};
use inillucent_storage::pager::{Pager, PagerOptions};
use inillucent_storage::schema::{self, SchemaKind, SchemaObject};
use inillucent_value::affinity::{self, Affinity};
use inillucent_value::record::{KeyColumn, KeyInfo, RecordRef};
use inillucent_value::{Collation, TextEncoding, Value};
use inillucent_vfs::{DbPath, OsVfs};

use crate::fixtures;

/// One row read out of a table B-tree.
#[derive(Clone, Debug)]
pub struct ScannedRow {
    /// The rowid, for a rowid table.
    pub rowid: Option<i64>,
    /// The record's fields, exactly as stored.
    pub values: Vec<Value<'static>>,
    /// The record's bytes, as they are on the page.
    pub payload: Vec<u8>,
}

/// Everything one fixture holds, as inillucent reads it.
#[derive(Clone, Debug)]
pub struct ScannedDatabase {
    /// The schema rows.
    pub objects: Vec<SchemaObject>,
    /// Each table's rows, keyed by the table's name and in cursor order.
    pub tables: Vec<(String, Vec<ScannedRow>)>,
    /// Each index's entries, keyed by the index's name and in cursor order.
    pub indexes: Vec<(String, Vec<ScannedRow>)>,
    /// The database's text encoding.
    pub encoding: TextEncoding,
}

impl ScannedDatabase {
    /// Returns one table's rows by name.
    pub fn table(&self, name: &str) -> Option<&[ScannedRow]> {
        self.tables
            .iter()
            .find(|(table, _)| table == name)
            .map(|(_, rows)| rows.as_slice())
    }

    /// Returns one index's entries by name.
    pub fn index(&self, name: &str) -> Option<&[ScannedRow]> {
        self.indexes
            .iter()
            .find(|(index, _)| index == name)
            .map(|(_, rows)| rows.as_slice())
    }
}

/// Returns the corpus directory.
pub fn corpus_dir() -> PathBuf {
    fixtures::corpus_root(&crate::workspace_root())
}

/// Returns the path to one fixture.
pub fn fixture_path(name: &str) -> PathBuf {
    corpus_dir().join(name)
}

/// Opens a fixture read-only.
///
/// The read is retried on `SQLITE_BUSY`, which stands in for the busy handler
/// a connection will have once phase 10 adds one. Two readers acquiring SHARED
/// at the same instant collide on the serialising PENDING byte on Windows -
/// SQLite's own VFS retries three times for the same reason - and a test
/// binary running twenty threads against one file hits that far more often
/// than an application does.
pub fn open_fixture(name: &str) -> DbResult<(OsVfs, Pager)> {
    let vfs = OsVfs::new();
    let path = DbPath::new(fixture_path(name));
    let mut pager = Pager::open_read_only(&vfs, &path, PagerOptions::default())?;
    for attempt in 0..200u32 {
        match pager.begin_read() {
            Ok(()) => return Ok((vfs, pager)),
            Err(error) if error.code() == inillucent_base::PrimaryCode::Busy => {
                std::thread::sleep(std::time::Duration::from_millis(u64::from(attempt % 5) + 1));
            }
            Err(error) => return Err(error),
        }
    }
    pager.begin_read()?;
    Ok((vfs, pager))
}

/// Returns the key ordering a `CREATE INDEX` statement declares.
///
/// Storage cannot know this - it is SQL text - so the compatibility suite
/// parses it for its own corpus and hands it to the integrity check, which is
/// what lets the check verify that a `DESC` or `COLLATE NOCASE` index is in
/// the order its declaration promises rather than skipping the question.
pub fn index_key_info(sql: &str) -> KeyInfo {
    let Some(on) = sql.to_ascii_uppercase().find(" ON ") else {
        return KeyInfo::default();
    };
    let Some(open) = sql.get(on..).and_then(|rest| rest.find('(')) else {
        return KeyInfo::default();
    };
    let start = on.saturating_add(open).saturating_add(1);
    let Some(close) = sql.rfind(')') else {
        return KeyInfo::default();
    };
    let Some(body) = sql.get(start..close) else {
        return KeyInfo::default();
    };

    let mut columns = Vec::new();
    let mut depth = 0usize;
    let mut current = String::new();
    for character in body.chars() {
        match character {
            '(' => {
                depth = depth.saturating_add(1);
                current.push(character);
            }
            ')' => {
                depth = depth.saturating_sub(1);
                current.push(character);
            }
            ',' if depth == 0 => columns.push(std::mem::take(&mut current)),
            other => current.push(other),
        }
    }
    columns.push(current);

    KeyInfo {
        columns: columns
            .iter()
            .map(|column| {
                let words: Vec<String> = column
                    .split_whitespace()
                    .map(|word| word.to_ascii_uppercase())
                    .collect();
                let descending = words.iter().any(|word| word == "DESC");
                let collation = words
                    .iter()
                    .position(|word| word == "COLLATE")
                    .and_then(|at| words.get(at.saturating_add(1)))
                    .and_then(|name| Collation::from_name(name))
                    .unwrap_or_default();
                KeyColumn {
                    collation,
                    descending,
                }
            })
            .collect(),
    }
}

/// Returns the key ordering of every index in a schema, keyed by root page.
pub fn index_key_map(objects: &[SchemaObject]) -> BTreeMap<u32, KeyInfo> {
    let mut map = BTreeMap::new();
    for object in objects {
        if object.kind != SchemaKind::Index {
            continue;
        }
        let (Some(root), Some(sql)) = (object.root_page, object.sql.as_ref()) else {
            continue;
        };
        map.insert(root.get(), index_key_info(sql));
    }
    map
}

/// Scans every table and index in a database.
pub fn scan_database(pager: &mut Pager) -> DbResult<ScannedDatabase> {
    let limits = Limits::default();
    let encoding = pager.text_encoding();
    let objects = schema::load_schema(pager)?;
    let mut tables = Vec::new();
    let mut indexes = Vec::new();

    for object in &objects {
        let Some(root) = object.root_page else {
            continue;
        };
        match object.kind {
            SchemaKind::Table => {
                let rows = scan_tree(pager, root, RootKind::Table, &limits, encoding)?;
                tables.push((object.name.clone(), rows));
            }
            SchemaKind::Index => {
                let rows = scan_tree(pager, root, RootKind::Index, &limits, encoding)?;
                indexes.push((object.name.clone(), rows));
            }
            _ => {}
        }
    }
    Ok(ScannedDatabase {
        objects,
        tables,
        indexes,
        encoding,
    })
}

/// Which kind of B-tree a root page is expected to hold.
///
/// **An enum rather than `is_table: bool` (task-1962, A9).** `scan_tree(pager,
/// root, true, &limits, encoding)` at a call site says nothing about what the
/// `true` selects, and the two kinds are read by different cursors.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RootKind {
    /// A table's tree, keyed by rowid.
    Table,
    /// An index's tree, keyed by the indexed record.
    Index,
}

/// Scans one B-tree forwards, returning every entry.
///
/// @param pager - the pages to read through
/// @param root - the tree's root page
/// @param kind - which kind of tree it is
/// @param limits - the limits the records are read under
/// @param encoding - the database's text encoding
pub fn scan_tree(
    pager: &mut Pager,
    root: PageId,
    kind: RootKind,
    limits: &Limits,
    encoding: TextEncoding,
) -> DbResult<Vec<ScannedRow>> {
    let is_table = kind == RootKind::Table;
    // A WITHOUT ROWID table's root is an index B-tree, so what a table cursor
    // can walk is decided by the root page's own type rather than by whether
    // the schema called the object a table.
    let kind_is_table = is_table && root_is_table(pager, root)?;
    let mut cursor = if kind_is_table {
        BTreeCursor::table(root)
    } else {
        BTreeCursor::index(root, inillucent_value::KeyInfo::default())
    };
    let mut rows = Vec::new();
    let mut more = cursor.first(pager)?;
    while more {
        let rowid = if kind_is_table {
            Some(cursor.rowid()?)
        } else {
            None
        };
        let payload = cursor.payload(pager, limits)?;
        let record = RecordRef::parse_with_limits(&payload, encoding, limits)?;
        let values = record
            .values()?
            .into_iter()
            .map(|value| value.into_owned())
            .collect::<DbResult<Vec<Value<'static>>>>()?;
        rows.push(ScannedRow {
            rowid,
            values,
            payload,
        });
        more = cursor.next(pager)?;
    }
    cursor.reset();
    Ok(rows)
}

/// Scans one B-tree backwards, returning every entry in reverse order.
pub fn scan_tree_backwards(
    pager: &mut Pager,
    root: PageId,
    is_table: bool,
    limits: &Limits,
    encoding: TextEncoding,
) -> DbResult<Vec<ScannedRow>> {
    let kind_is_table = is_table && root_is_table(pager, root)?;
    let mut cursor = if kind_is_table {
        BTreeCursor::table(root)
    } else {
        BTreeCursor::index(root, inillucent_value::KeyInfo::default())
    };
    let mut rows = Vec::new();
    let mut more = cursor.last(pager)?;
    while more {
        let rowid = if kind_is_table {
            Some(cursor.rowid()?)
        } else {
            None
        };
        let payload = cursor.payload(pager, limits)?;
        let record = RecordRef::parse_with_limits(&payload, encoding, limits)?;
        let values = record
            .values()?
            .into_iter()
            .map(|value| value.into_owned())
            .collect::<DbResult<Vec<Value<'static>>>>()?;
        rows.push(ScannedRow {
            rowid,
            values,
            payload,
        });
        more = cursor.previous(pager)?;
    }
    cursor.reset();
    Ok(rows)
}

/// Reports whether a root page begins a table B-tree.
pub fn root_is_table(pager: &mut Pager, root: PageId) -> DbResult<bool> {
    let pin = pager.get_page(root)?;
    let usable = pager.usable_size()?;
    let layout = inillucent_storage::PageLayout::parse(pin.bytes(), root, usable)?;
    Ok(layout.kind.is_table())
}

/// Seeks one rowid in a table and returns the row when it is there.
pub fn seek_rowid(
    pager: &mut Pager,
    root: PageId,
    rowid: i64,
    limits: &Limits,
) -> DbResult<Option<ScannedRow>> {
    let encoding = pager.text_encoding();
    let mut cursor = BTreeCursor::table(root);
    if !cursor.seek_rowid(pager, rowid, SeekBias::AtOrAfter)? {
        cursor.reset();
        return Ok(None);
    }
    let payload = cursor.payload(pager, limits)?;
    let record = RecordRef::parse_with_limits(&payload, encoding, limits)?;
    let values = record
        .values()?
        .into_iter()
        .map(|value| value.into_owned())
        .collect::<DbResult<Vec<Value<'static>>>>()?;
    let row = ScannedRow {
        rowid: Some(cursor.rowid()?),
        values,
        payload,
    };
    cursor.reset();
    Ok(Some(row))
}

/// Returns the declared type of each column in a `CREATE TABLE` statement.
///
/// This is the crudest parse that works on the corpus, and it is deliberately
/// crude: a real parser is phase 5's job and lives in another crate. It exists
/// only to bridge the one documented difference between what a record stores
/// and what a query sees - the REAL-affinity integer - and a test that needs
/// more than this is testing something phase 3 does not own.
pub fn declared_types(sql: &str) -> Vec<String> {
    let Some(open) = sql.find('(') else {
        return Vec::new();
    };
    let Some(close) = sql.rfind(')') else {
        return Vec::new();
    };
    let Some(body) = sql.get(open.saturating_add(1)..close) else {
        return Vec::new();
    };

    let mut columns = Vec::new();
    let mut depth = 0usize;
    let mut current = String::new();
    for character in body.chars() {
        match character {
            '(' => {
                depth = depth.saturating_add(1);
                current.push(character);
            }
            ')' => {
                depth = depth.saturating_sub(1);
                current.push(character);
            }
            ',' if depth == 0 => {
                columns.push(std::mem::take(&mut current));
            }
            other => current.push(other),
        }
    }
    columns.push(current);

    columns
        .into_iter()
        .filter_map(|column| {
            let trimmed = column.trim().to_string();
            let upper = trimmed.to_ascii_uppercase();
            // A table constraint is not a column.
            if upper.starts_with("PRIMARY ")
                || upper.starts_with("UNIQUE")
                || upper.starts_with("CHECK")
                || upper.starts_with("FOREIGN ")
                || upper.starts_with("CONSTRAINT ")
                || trimmed.is_empty()
            {
                return None;
            }
            let mut words = trimmed.split_whitespace();
            let _name = words.next()?;
            let mut declared = String::new();
            for word in words {
                let upper = word.to_ascii_uppercase();
                if upper.starts_with("PRIMARY")
                    || upper.starts_with("NOT")
                    || upper.starts_with("NULL")
                    || upper.starts_with("UNIQUE")
                    || upper.starts_with("DEFAULT")
                    || upper.starts_with("CHECK")
                    || upper.starts_with("COLLATE")
                    || upper.starts_with("REFERENCES")
                    || upper.starts_with("GENERATED")
                    || upper.starts_with("AS")
                {
                    break;
                }
                if !declared.is_empty() {
                    declared.push(' ');
                }
                declared.push_str(word);
            }
            Some(declared)
        })
        .collect()
}

/// One column of a `CREATE TABLE`, as much as the crude parse can tell.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ColumnSpec {
    /// The column's name.
    pub name: String,
    /// Its declared type, which may be empty.
    pub declared: String,
    /// Whether the column declares PRIMARY KEY on its own line.
    pub column_primary_key: bool,
}

/// A table's shape, as far as a record's field order depends on it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TableSpec {
    /// The columns, in declaration order.
    pub columns: Vec<ColumnSpec>,
    /// The primary key's columns, by index into `columns`.
    pub primary_key: Vec<usize>,
    /// Whether the table is WITHOUT ROWID.
    pub without_rowid: bool,
}

impl TableSpec {
    /// Returns the column that is an alias for the rowid, if there is one.
    ///
    /// A rowid alias is a single-column `INTEGER PRIMARY KEY` on a table that
    /// has a rowid. That column is not stored in the record at all - the
    /// record holds NULL there and the rowid is the value - which is the one
    /// place a scan's fields do not line up with `SELECT *`.
    pub fn rowid_alias(&self) -> Option<usize> {
        if self.without_rowid || self.primary_key.len() != 1 {
            return None;
        }
        let index = *self.primary_key.first()?;
        let column = self.columns.get(index)?;
        column
            .declared
            .trim()
            .eq_ignore_ascii_case("INTEGER")
            .then_some(index)
    }

    /// Returns, for each record field, the table column it holds.
    ///
    /// A rowid table stores its columns in declaration order. A WITHOUT ROWID
    /// table stores the primary key's columns first, in key order, and then
    /// the rest in declaration order, because its B-tree *is* its primary-key
    /// index and the key has to come first for the tree to be searchable.
    pub fn field_order(&self) -> Vec<usize> {
        if !self.without_rowid {
            return (0..self.columns.len()).collect();
        }
        let mut order = self.primary_key.clone();
        for index in 0..self.columns.len() {
            if !order.contains(&index) {
                order.push(index);
            }
        }
        order
    }
}

/// Splits a parenthesised, comma-separated list at depth zero.
fn split_at_top_level(body: &str) -> Vec<String> {
    let mut items = Vec::new();
    let mut depth = 0usize;
    let mut current = String::new();
    for character in body.chars() {
        match character {
            '(' => {
                depth = depth.saturating_add(1);
                current.push(character);
            }
            ')' => {
                depth = depth.saturating_sub(1);
                current.push(character);
            }
            ',' if depth == 0 => items.push(std::mem::take(&mut current)),
            other => current.push(other),
        }
    }
    items.push(current);
    items
}

/// Returns the declared type in one column definition, minus its constraints.
fn declared_type_of(definition: &str) -> Option<(String, String, bool)> {
    let trimmed = definition.trim();
    let upper = trimmed.to_ascii_uppercase();
    let mut words = trimmed.split_whitespace();
    let name = words.next()?.trim_matches('"').to_string();
    let mut declared = String::new();
    for word in words {
        let word_upper = word.to_ascii_uppercase();
        if word_upper.starts_with("PRIMARY")
            || word_upper.starts_with("NOT")
            || word_upper.starts_with("NULL")
            || word_upper.starts_with("UNIQUE")
            || word_upper.starts_with("DEFAULT")
            || word_upper.starts_with("CHECK")
            || word_upper.starts_with("COLLATE")
            || word_upper.starts_with("REFERENCES")
            || word_upper.starts_with("GENERATED")
            || word_upper.starts_with("AS")
        {
            break;
        }
        if !declared.is_empty() {
            declared.push(' ');
        }
        declared.push_str(word);
    }
    Some((name, declared, upper.contains("PRIMARY KEY")))
}

/// Reports whether one item in a `CREATE TABLE` body is a table constraint
/// rather than a column.
fn is_table_constraint(upper: &str) -> bool {
    upper.starts_with("PRIMARY KEY")
        || upper.starts_with("UNIQUE")
        || upper.starts_with("CHECK")
        || upper.starts_with("FOREIGN ")
        || upper.starts_with("CONSTRAINT ")
}

/// Parses a `CREATE TABLE` statement into the shape a record's order needs.
pub fn table_spec(sql: &str) -> TableSpec {
    let without_rowid = sql.to_ascii_uppercase().contains("WITHOUT ROWID");
    let empty = TableSpec {
        columns: Vec::new(),
        primary_key: Vec::new(),
        without_rowid,
    };
    let (Some(open), Some(close)) = (sql.find('('), sql.rfind(')')) else {
        return empty;
    };
    let Some(body) = sql.get(open.saturating_add(1)..close) else {
        return empty;
    };

    let mut columns: Vec<ColumnSpec> = Vec::new();
    let mut table_key: Vec<String> = Vec::new();
    for item in split_at_top_level(body) {
        let trimmed = item.trim();
        let upper = trimmed.to_ascii_uppercase();
        if upper.starts_with("PRIMARY KEY") {
            if let (Some(open), Some(close)) = (trimmed.find('('), trimmed.rfind(')')) {
                let inner = trimmed.get(open.saturating_add(1)..close).unwrap_or("");
                table_key = inner
                    .split(',')
                    .map(|name| {
                        name.split_whitespace()
                            .next()
                            .unwrap_or("")
                            .trim_matches('"')
                            .to_string()
                    })
                    .collect();
            }
            continue;
        }
        if is_table_constraint(&upper) || trimmed.is_empty() {
            continue;
        }
        if let Some((name, declared, column_primary_key)) = declared_type_of(trimmed) {
            columns.push(ColumnSpec {
                name,
                declared,
                column_primary_key,
            });
        }
    }

    let primary_key = if table_key.is_empty() {
        columns
            .iter()
            .enumerate()
            .filter(|(_, column)| column.column_primary_key)
            .map(|(index, _)| index)
            .collect()
    } else {
        table_key
            .iter()
            .filter_map(|name| {
                columns
                    .iter()
                    .position(|column| column.name.eq_ignore_ascii_case(name))
            })
            .collect()
    };

    TableSpec {
        columns,
        primary_key,
        without_rowid,
    }
}

/// Applies the one read-side conversion a column's declared type implies.
///
/// SQLite writes an integral value in a REAL-affinity column as an integer to
/// save space and converts it back to a double as it is read out. Nothing else
/// about a column's type changes what a stored value means.
pub fn as_a_query_sees_it(value: Value<'static>, declared: &str) -> Value<'static> {
    if affinity::for_column(declared.as_bytes()) == Affinity::Real {
        return affinity::realify(value);
    }
    value
}

/// Returns the bytes of every file in the corpus directory, for a read-only
/// proof that compares a directory before and after.
pub fn directory_snapshot(directory: &Path) -> std::io::Result<Vec<(String, u64, String)>> {
    let mut entries = Vec::new();
    for entry in std::fs::read_dir(directory)? {
        let entry = entry?;
        let metadata = entry.metadata()?;
        if !metadata.is_file() {
            continue;
        }
        let bytes = std::fs::read(entry.path())?;
        entries.push((
            entry.file_name().to_string_lossy().into_owned(),
            metadata.len(),
            crate::hash::sha256_hex(&bytes),
        ));
    }
    entries.sort();
    Ok(entries)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The crude column parse must find the declared types the corpus uses,
    /// including the columns that declare none.
    #[test]
    fn declared_types_are_read_out_of_a_create_statement() {
        let sql = "CREATE TABLE people (id INTEGER PRIMARY KEY, name TEXT, score REAL, \
                   tag BLOB, note)";
        assert_eq!(
            declared_types(sql),
            vec!["INTEGER", "TEXT", "REAL", "BLOB", ""]
        );
        let sql = "CREATE TABLE pair (a INTEGER, b TEXT, c, PRIMARY KEY (a, b)) WITHOUT ROWID";
        assert_eq!(declared_types(sql), vec!["INTEGER", "TEXT", ""]);
        let sql = "CREATE TABLE t (v VARCHAR(20) NOT NULL DEFAULT 'x')";
        assert_eq!(declared_types(sql), vec!["VARCHAR(20)"]);
    }

    /// The index-order parse must find DESC and COLLATE where they are.
    #[test]
    fn index_key_orders_are_read_out_of_a_create_index() {
        let key = index_key_info("CREATE INDEX i ON t (a)");
        assert_eq!(key.columns.len(), 1);
        assert_eq!(key.column(0).collation, Collation::Binary);
        assert!(!key.column(0).descending);

        let key = index_key_info("CREATE INDEX i ON t (score DESC, name)");
        assert_eq!(key.columns.len(), 2);
        assert!(key.column(0).descending);
        assert!(!key.column(1).descending);

        let key = index_key_info("CREATE INDEX i ON t (w COLLATE NOCASE)");
        assert_eq!(key.column(0).collation, Collation::NoCase);

        let key = index_key_info("CREATE INDEX i ON t (w COLLATE RTRIM DESC)");
        assert_eq!(key.column(0).collation, Collation::RTrim);
        assert!(key.column(0).descending);

        // A column past the declaration is the trailing rowid, which is always
        // ascending and BINARY.
        assert_eq!(key.column(1).collation, Collation::Binary);
        assert!(!key.column(1).descending);
    }

    /// The table parse must find the primary key, the rowid alias, and the
    /// field order a WITHOUT ROWID table stores its columns in.
    #[test]
    fn table_shapes_are_read_out_of_a_create_statement() {
        let spec = table_spec(
            "CREATE TABLE people (id INTEGER PRIMARY KEY, name TEXT, score REAL, tag BLOB, note)",
        );
        assert_eq!(spec.columns.len(), 5);
        assert_eq!(spec.primary_key, vec![0]);
        assert_eq!(spec.rowid_alias(), Some(0));
        assert!(!spec.without_rowid);
        assert_eq!(spec.field_order(), vec![0, 1, 2, 3, 4]);

        let spec = table_spec(
            "CREATE TABLE pair (a INTEGER, b TEXT, c, PRIMARY KEY (a, b)) WITHOUT ROWID",
        );
        assert!(spec.without_rowid);
        assert_eq!(spec.primary_key, vec![0, 1]);
        assert_eq!(spec.rowid_alias(), None);
        assert_eq!(spec.field_order(), vec![0, 1, 2]);

        // A WITHOUT ROWID table whose key is not its leading columns stores
        // the key first, which is the case an identity order would get wrong.
        let spec = table_spec("CREATE TABLE t (a, b, c, PRIMARY KEY (c, a)) WITHOUT ROWID");
        assert_eq!(spec.field_order(), vec![2, 0, 1]);

        // A TEXT primary key is not a rowid alias, however singular it is.
        let spec = table_spec("CREATE TABLE kv (k TEXT PRIMARY KEY, v)");
        assert_eq!(spec.rowid_alias(), None);
    }

    /// The REAL bridge converts an integer in a REAL column and nothing else.
    #[test]
    fn only_a_real_column_converts_a_stored_integer() {
        let converted = as_a_query_sees_it(Value::Integer(3), "REAL");
        assert!(matches!(converted, Value::Real(value) if value == 3.0));
        let untouched = as_a_query_sees_it(Value::Integer(3), "INTEGER");
        assert!(matches!(untouched, Value::Integer(3)));
        let untouched = as_a_query_sees_it(Value::Integer(3), "");
        assert!(matches!(untouched, Value::Integer(3)));
        let untouched = as_a_query_sees_it(Value::Integer(3), "NUMERIC");
        assert!(matches!(untouched, Value::Integer(3)));
    }
}
