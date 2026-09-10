//! A read-only reader for SQLite 3 database files.
//!
//! Invariant: this crate opens a file and never writes to it. `DatabaseOptions`
//! is set read-only, no journal is attached, and there is no code path here that
//! calls a mutating pager method. A migration that damaged its source would be
//! worse than one that failed.
//!
//! ## Why it exists after file-format compatibility stopped being a goal
//!
//! The rearchitecture plan drops SQLite file-format compatibility as
//! a requirement, but two things still need to read a SQLite file:
//!
//! - **The correctness gate.** The differential harness compares inillucent against
//!   SQLite 3.53.4 executing the same SQL on the same *logical* data. With the
//!   shared file gone, the inillucent side gets its data by importing the SQLite
//!   fixture through this reader. Same rows, different bytes.
//! - **Migration.** Every database the previous tickets produced is in SQLite
//!   format, and `inillucent-migrate` has to be able to read one.
//!
//! It is the read half of `inillucent-storage` with a narrow interface in front of
//! it, which is exactly what the TDD's component triage says survives that
//! crate's deletion. Until Phase 5 deletes the rest, this crate reuses
//! `inillucent-storage`'s pager and b-tree cursor rather than duplicating them, so
//! there is one page decoder in the workspace rather than two that can disagree.
//!
//! ## What it does not do
//!
//! No SQL, no planner, no write path, no journal recovery beyond what opening a
//! clean file needs, and no attempt to be fast: an import runs once per fixture
//! and its cost is not on any measured path.

#![forbid(unsafe_code)]
#![deny(missing_docs)]
#![deny(clippy::indexing_slicing)]
#![deny(clippy::unwrap_used)]
#![deny(clippy::expect_used)]
#![deny(clippy::panic)]
#![cfg_attr(
    test,
    allow(
        clippy::expect_used,
        clippy::indexing_slicing,
        clippy::panic,
        clippy::unwrap_used
    )
)]

use std::path::PathBuf;
use std::sync::Arc;

use inillucent_base::error::{corrupt, misuse};
use inillucent_base::ids::PageId;
use inillucent_base::limits::Limits;
use inillucent_base::DbResult;
use inillucent_storage::cursor::BTreeCursor;
use inillucent_storage::pager::Pager;
use inillucent_transaction::recovery::{open_database, DatabaseOptions};
use inillucent_tree::datum::{Datum, OwnedDatum};
use inillucent_value::record::{FieldSpan, KeyInfo, RecordRef};
use inillucent_value::{TextEncoding, Value};
use inillucent_vfs::path::DbPath;
use inillucent_vfs::{OsVfs, Vfs};

/// One row of `sqlite_schema`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SchemaObject {
    /// `table`, `index`, `view` or `trigger`.
    pub kind: String,
    /// The object's name.
    pub name: String,
    /// The table the object belongs to; for a table, its own name.
    pub table: String,
    /// The root page, or 0 for an object with no b-tree.
    pub root: u32,
    /// The `CREATE` statement the object was declared with.
    pub sql: String,
}

impl SchemaObject {
    /// Returns the column names the `CREATE TABLE` statement declares.
    ///
    /// A deliberately small parser: it takes the text between the outermost
    /// parentheses, splits on commas that are not inside parentheses, and reads
    /// the first identifier of each part. That is enough for the fixtures and
    /// for the tables `inillucent-migrate` has to read, and it refuses rather than
    /// guesses on anything it does not recognise - a table constraint
    /// (`PRIMARY KEY (...)`, `UNIQUE (...)`, `FOREIGN KEY`, `CHECK`) is skipped
    /// rather than mistaken for a column.
    ///
    /// A general answer needs the real parser in `inillucent-sql`, and this crate
    /// deliberately sits below it so that a migration tool does not drag the
    /// front end in. Phase 4 revisits this when DDL import needs types and
    /// constraints as well as names.
    pub fn column_names(&self) -> DbResult<Vec<String>> {
        let open = self
            .sql
            .find('(')
            .ok_or_else(|| corrupt(format!("{} has no column list", self.name)))?;
        let close = self
            .sql
            .rfind(')')
            .ok_or_else(|| corrupt(format!("{} has no column list", self.name)))?;
        if close <= open {
            return Err(corrupt(format!(
                "{}'s column list is inside out",
                self.name
            )));
        }
        let body = self.sql.get(open.saturating_add(1)..close).unwrap_or("");
        let mut names = Vec::new();
        let mut depth = 0i32;
        let mut part = String::new();
        for character in body.chars() {
            match character {
                '(' => {
                    depth = depth.saturating_add(1);
                    part.push(character);
                }
                ')' => {
                    depth = depth.saturating_sub(1);
                    part.push(character);
                }
                ',' if depth == 0 => {
                    push_column_name(&part, &mut names);
                    part.clear();
                }
                _ => part.push(character),
            }
        }
        push_column_name(&part, &mut names);
        if names.is_empty() {
            return Err(corrupt(format!("{} declares no columns", self.name)));
        }
        Ok(names)
    }
}

/// The keywords that begin a table constraint rather than a column.
const TABLE_CONSTRAINTS: [&str; 6] = [
    "primary",
    "unique",
    "check",
    "foreign",
    "constraint",
    "exclude",
];

/// Adds one column-definition fragment's name to the list, if it is a column.
///
/// @param part - one comma-separated fragment of the column list
/// @param names - the list being built
fn push_column_name(part: &str, names: &mut Vec<String>) {
    let trimmed = part.trim();
    let Some(first) = trimmed.split_whitespace().next() else {
        return;
    };
    if TABLE_CONSTRAINTS
        .iter()
        .any(|keyword| first.eq_ignore_ascii_case(keyword))
    {
        return;
    }
    let cleaned = first.trim_matches(|c| c == '"' || c == '`' || c == '[' || c == ']');
    if cleaned.is_empty() {
        return;
    }
    names.push(cleaned.to_string());
}

/// An open SQLite file, held for reading.
pub struct SqliteFile {
    pager: Pager,
    limits: Limits,
}

impl SqliteFile {
    /// Opens a SQLite database file read-only and starts a read transaction.
    ///
    /// @param path - the database file
    pub fn open(path: PathBuf) -> DbResult<SqliteFile> {
        let vfs: Arc<dyn Vfs> = Arc::new(OsVfs::new());
        let options = DatabaseOptions {
            writable: false,
            ..DatabaseOptions::default()
        };
        let mut pager = open_database(vfs, &DbPath::new(path), options)?;
        pager.begin_read()?;
        Ok(SqliteFile {
            pager,
            limits: Limits::default(),
        })
    }

    /// Returns the file's page size in bytes.
    pub fn page_size(&self) -> u32 {
        self.pager.page_size().bytes()
    }

    /// Returns the number of pages in the file.
    pub fn page_count(&self) -> u32 {
        self.pager.page_count()
    }

    /// Returns the file's catalog, with indexes attached to their tables.
    ///
    /// This goes through `inillucent-catalog`'s own loader rather than parsing the
    /// schema again here. There is one schema reader in the workspace and this
    /// is not a second one: an index's key columns, its collations and its
    /// descending flags all come from parsing `CREATE INDEX` against the
    /// table it indexes, and a fixture import that got any of them wrong would
    /// build a tree in an order the executor then assumes wrongly.
    ///
    /// @param name - the name to attach the database under, normally `main`
    pub fn catalog(
        &mut self,
        name: &[u8],
    ) -> DbResult<inillucent_catalog::snapshot::DatabaseCatalog> {
        inillucent_catalog::load::load_database_catalog(&mut self.pager, name, 0)
    }

    /// Returns every row of `sqlite_schema`.
    pub fn schema(&mut self) -> DbResult<Vec<SchemaObject>> {
        let root = PageId::from_persisted(1)?;
        let mut cursor = BTreeCursor::table(root);
        let mut payload: Vec<u8> = Vec::with_capacity(512);
        let mut fields: Vec<FieldSpan> = Vec::with_capacity(8);
        let mut out = Vec::new();
        let mut more = cursor.first(&mut self.pager)?;
        while more {
            cursor.payload_into(&mut self.pager, &self.limits, &mut payload)?;
            let header_len = RecordRef::parse_into(&payload, &self.limits, &mut fields)?;
            let record = RecordRef::with_fields(&payload, &fields, header_len, TextEncoding::Utf8);
            out.push(SchemaObject {
                kind: text_at(&record, 0)?,
                name: text_at(&record, 1)?,
                table: text_at(&record, 2)?,
                root: u32::try_from(integer_at(&record, 3)?)
                    .map_err(|_| corrupt("a root page that is not a page number"))?,
                sql: text_at(&record, 4)?,
            });
            more = cursor.next(&mut self.pager)?;
        }
        Ok(out)
    }

    /// Returns one named schema object.
    ///
    /// @param kind - `table` or `index`
    /// @param name - the object's name
    pub fn object(&mut self, kind: &str, name: &str) -> DbResult<SchemaObject> {
        self.schema()?
            .into_iter()
            .find(|object| object.kind == kind && object.name == name)
            .ok_or_else(|| misuse(format!("no {kind} named {name} in this file")))
    }

    /// Reads every row of a table b-tree.
    ///
    /// The rowid is prepended as column 0, which is what a rowid-clustered tree
    /// in the new format holds: SQLite stores the rowid in the cell key rather
    /// than in the record, and an `INTEGER PRIMARY KEY` column's record field is
    /// NULL because of it. Prepending makes the row the new engine's shape.
    ///
    /// @param root - the table's root page
    /// @param columns - how many record fields the table declares
    pub fn read_table(&mut self, root: u32, columns: usize) -> DbResult<Vec<Vec<OwnedDatum>>> {
        let root = PageId::from_persisted(root)?;
        let mut cursor = BTreeCursor::table(root);
        let mut payload: Vec<u8> = Vec::with_capacity(512);
        let mut fields: Vec<FieldSpan> = Vec::with_capacity(16);
        let mut out = Vec::new();
        let mut more = cursor.first(&mut self.pager)?;
        while more {
            let rowid = cursor.rowid()?;
            cursor.payload_into(&mut self.pager, &self.limits, &mut payload)?;
            let header_len = RecordRef::parse_into(&payload, &self.limits, &mut fields)?;
            let record = RecordRef::with_fields(&payload, &fields, header_len, TextEncoding::Utf8);
            let mut row = Vec::with_capacity(columns.saturating_add(1));
            row.push(OwnedDatum::Int(rowid));
            for index in 0..columns {
                row.push(owned_from_record(&record, index)?);
            }
            out.push(row);
            more = cursor.next(&mut self.pager)?;
        }
        Ok(out)
    }

    /// Reads every entry of an index b-tree.
    ///
    /// An index entry's record is the indexed columns followed by the rowid, so
    /// the returned row is already the new format's index-tree row shape and no
    /// column is prepended.
    ///
    /// @param root - the index's root page
    /// @param columns - how many fields an entry holds, rowid included
    pub fn read_index(&mut self, root: u32, columns: usize) -> DbResult<Vec<Vec<OwnedDatum>>> {
        let root = PageId::from_persisted(root)?;
        // A full walk never compares, so plain binary ordering over the key
        // columns is enough to build the cursor.
        let mut cursor = BTreeCursor::index(root, KeyInfo::binary(columns));
        let mut payload: Vec<u8> = Vec::with_capacity(512);
        let mut fields: Vec<FieldSpan> = Vec::with_capacity(16);
        let mut out = Vec::new();
        let mut more = cursor.first(&mut self.pager)?;
        while more {
            cursor.payload_into(&mut self.pager, &self.limits, &mut payload)?;
            let header_len = RecordRef::parse_into(&payload, &self.limits, &mut fields)?;
            let record = RecordRef::with_fields(&payload, &fields, header_len, TextEncoding::Utf8);
            let mut row = Vec::with_capacity(columns);
            for index in 0..columns {
                row.push(owned_from_record(&record, index)?);
            }
            out.push(row);
            more = cursor.next(&mut self.pager)?;
        }
        Ok(out)
    }
}

/// Converts one record field into an owned value.
///
/// @param record - the decoded record
/// @param index - which field to convert
fn owned_from_record(record: &RecordRef<'_>, index: usize) -> DbResult<OwnedDatum> {
    Ok(match record.value(index)? {
        Value::Null => OwnedDatum::Null,
        Value::Integer(number) => OwnedDatum::Int(number),
        Value::Real(number) => OwnedDatum::Real(number),
        Value::Text(text) => OwnedDatum::Text(text.utf8_bytes().into_owned()),
        Value::Blob(blob) => OwnedDatum::Blob(blob.raw().to_vec()),
    })
}

/// Returns one record field as a string.
///
/// @param record - the decoded record
/// @param index - which field to read
fn text_at(record: &RecordRef<'_>, index: usize) -> DbResult<String> {
    match record.value(index)? {
        Value::Text(text) => Ok(String::from_utf8_lossy(&text.utf8_bytes()).into_owned()),
        Value::Null => Ok(String::new()),
        other => Err(corrupt(format!(
            "expected text in schema field {index}, found {:?}",
            other.storage_class()
        ))),
    }
}

/// Returns one record field as an integer.
///
/// @param record - the decoded record
/// @param index - which field to read
fn integer_at(record: &RecordRef<'_>, index: usize) -> DbResult<i64> {
    match record.value(index)? {
        Value::Integer(number) => Ok(number),
        Value::Null => Ok(0),
        other => Err(corrupt(format!(
            "expected an integer in schema field {index}, found {:?}",
            other.storage_class()
        ))),
    }
}

/// Borrows an owned row, for handing to the tree builder.
///
/// @param row - the owned row
pub fn borrow(row: &[OwnedDatum]) -> Vec<Datum<'_>> {
    row.iter().map(OwnedDatum::borrow).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn object(sql: &str) -> SchemaObject {
        SchemaObject {
            kind: "table".to_string(),
            name: "t".to_string(),
            table: "t".to_string(),
            root: 2,
            sql: sql.to_string(),
        }
    }

    /// The fixture's own `CREATE TABLE` yields its five columns in order.
    #[test]
    fn the_fixture_schema_parses() {
        let names = object(
            "CREATE TABLE main_table(id INTEGER PRIMARY KEY, key INTEGER NOT NULL, \
             category INTEGER NOT NULL, label TEXT NOT NULL, payload BLOB)",
        )
        .column_names()
        .unwrap();
        assert_eq!(names, ["id", "key", "category", "label", "payload"]);
    }

    /// A table constraint is skipped rather than read as a column.
    #[test]
    fn table_constraints_are_not_columns() {
        let names = object(
            "CREATE TABLE t(a INTEGER, b TEXT, PRIMARY KEY (a, b), \
             FOREIGN KEY (b) REFERENCES u(x), CHECK (a > 0))",
        )
        .column_names()
        .unwrap();
        assert_eq!(names, ["a", "b"]);
    }

    /// A type with its own parentheses does not end the column early.
    #[test]
    fn parenthesised_types_stay_in_one_column() {
        let names = object("CREATE TABLE t(a VARCHAR(20), b DECIMAL(10, 2), c INT)")
            .column_names()
            .unwrap();
        assert_eq!(names, ["a", "b", "c"]);
    }

    /// Quoted identifiers are unquoted.
    #[test]
    fn quoted_identifiers_are_unquoted() {
        let names = object("CREATE TABLE t(\"a b\" INTEGER, `c` TEXT, [d] BLOB)")
            .column_names()
            .unwrap();
        assert_eq!(names, ["a", "c", "d"]);
    }

    /// A statement with no column list is refused rather than guessed at.
    #[test]
    fn a_missing_column_list_is_refused() {
        assert!(object("CREATE TABLE t").column_names().is_err());
        assert!(object("CREATE TABLE t)(").column_names().is_err());
        assert!(object("CREATE TABLE t()").column_names().is_err());
    }
}
