//! Writing `sqlite_schema`, allocating root pages, and moving the cookie.
//!
//! Invariant: a schema change is one transaction's worth of ordinary writes.
//! The row goes into `sqlite_schema` with the pager the statement is already
//! using, the root page comes off the same freelist a table page does, and the
//! cookie moves in the same header write - so a `CREATE TABLE` that fails
//! half-way is undone by the same rollback that undoes an `INSERT`, and there
//! is no second recovery path to get wrong.
//!
//! The stored `CREATE` text is the statement from its object name onward,
//! prefixed with the keywords. That is what SQLite stores, and it is why
//! `IF NOT EXISTS` and a schema qualifier never appear in `sqlite_master.sql`:
//! the text is sliced out of the source rather than printed back from a tree,
//! so it also keeps the whitespace and the quoting the author chose.

use inillucent_base::error::misuse;
use inillucent_base::ids::PageId;
use inillucent_base::limits::Limits;
use inillucent_base::DbResult;
use inillucent_storage::cursor::BTreeCursor;
use inillucent_storage::mutate;
use inillucent_storage::pager::Pager;
use inillucent_storage::schema::{SchemaKind, SCHEMA_ROOT};
use inillucent_value::record::{encode_record, RecordRef};
use inillucent_value::Value;

/// One row to write into `sqlite_schema`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SchemaRow {
    /// What kind of object it describes.
    pub kind: SchemaKind,
    /// The object's name.
    pub name: Vec<u8>,
    /// The table it belongs to, which for a table is its own name.
    pub table: Vec<u8>,
    /// The root page of its B-tree, or zero when it has none.
    pub root: u32,
    /// The `CREATE` text, or `None` for an automatic index.
    pub sql: Option<Vec<u8>>,
}

/// Returns the page `sqlite_schema` is rooted at.
fn schema_root() -> DbResult<PageId> {
    PageId::from_persisted(SCHEMA_ROOT)
}

/// Allocates an empty B-tree for a new table.
pub fn allocate_table_root(pager: &mut Pager) -> DbResult<u32> {
    Ok(mutate::create_table(pager)?.get())
}

/// Allocates an empty B-tree for a new index.
pub fn allocate_index_root(pager: &mut Pager) -> DbResult<u32> {
    Ok(mutate::create_index(pager)?.get())
}

/// Writes one row into `sqlite_schema`.
///
/// The rowid is one past the largest in use rather than the row count: a
/// dropped object leaves a gap, and reusing its rowid would put the new row
/// where a cursor that had saved a position was expecting the old one.
pub fn insert_schema_row(pager: &mut Pager, row: &SchemaRow) -> DbResult<i64> {
    let rowid = next_schema_rowid(pager)?;
    let values = [
        Value::owned_text(row.kind.as_text().as_bytes())?,
        Value::owned_text(&row.name)?,
        Value::owned_text(&row.table)?,
        Value::Integer(i64::from(row.root)),
        match &row.sql {
            Some(sql) => Value::owned_text(sql)?,
            None => Value::Null,
        },
    ];
    let encoding = pager.text_encoding();
    let format = pager.header().schema_format.max(1);
    let payload = encode_record(&values, encoding, format)?;
    mutate::insert_row(pager, schema_root()?, rowid, &payload)?;
    Ok(rowid)
}

/// Returns the rowid a new schema row is written under.
fn next_schema_rowid(pager: &mut Pager) -> DbResult<i64> {
    let mut cursor = BTreeCursor::table(schema_root()?);
    if !cursor.last(pager)? {
        return Ok(1);
    }
    Ok(cursor.rowid()?.saturating_add(1))
}

/// Reads every `sqlite_schema` row, with the rowid each is stored under.
///
/// `ALTER TABLE` needs the whole table rather than one row: a rename touches
/// the object's own row and the row of everything that names it, and the set of
/// those is only known by looking at all of them.
pub fn read_schema_rows(pager: &mut Pager) -> DbResult<Vec<(i64, SchemaRow)>> {
    let limits = Limits::default();
    let mut out = Vec::new();
    let mut cursor = BTreeCursor::table(schema_root()?);
    let mut more = cursor.first(pager)?;
    while more {
        let rowid = cursor.rowid()?;
        let payload = cursor.payload(pager, &limits)?;
        let record = RecordRef::parse_with_limits(&payload, pager.text_encoding(), &limits)?;
        let kind = SchemaKind::from_text(&text_field(&record, 0)).unwrap_or(SchemaKind::Table);
        out.push((
            rowid,
            SchemaRow {
                kind,
                name: text_field(&record, 1),
                table: text_field(&record, 2),
                root: record
                    .value(3)
                    .ok()
                    .map(|value| inillucent_value::cast::integer_value(&value))
                    .unwrap_or(0)
                    .max(0) as u32,
                sql: match record.value(4) {
                    Ok(inillucent_value::Value::Text(text)) => Some(text.utf8_bytes().to_vec()),
                    _ => None,
                },
            },
        ));
        more = cursor.next(pager)?;
    }
    Ok(out)
}

/// Returns one text column of a schema record, or an empty vector.
fn text_field(record: &RecordRef<'_>, column: usize) -> Vec<u8> {
    match record.value(column) {
        Ok(inillucent_value::Value::Text(text)) => text.utf8_bytes().to_vec(),
        _ => Vec::new(),
    }
}

/// Removes every `sqlite_schema` row belonging to one object.
///
/// A `DROP TABLE` takes the table's own row and every index row that names it,
/// which is what stops a dropped table leaving indexes behind that point at a
/// root page the freelist has since handed to something else.
pub fn delete_schema_rows(
    pager: &mut Pager,
    matches: impl Fn(&SchemaRowView<'_>) -> bool,
) -> DbResult<Vec<u32>> {
    let limits = Limits::default();
    let mut doomed = Vec::new();
    let mut roots = Vec::new();
    let mut cursor = BTreeCursor::table(schema_root()?);
    let mut more = cursor.first(pager)?;
    while more {
        let rowid = cursor.rowid()?;
        let payload = cursor.payload(pager, &limits)?;
        let record = inillucent_value::record::RecordRef::parse(&payload, pager.text_encoding())?;
        let kind = text_of(&record, 0)?;
        let name = text_of(&record, 1)?;
        let table = text_of(&record, 2)?;
        let view = SchemaRowView {
            kind: &kind,
            name: &name,
            table: &table,
            root: integer_of(&record, 3)?,
        };
        if matches(&view) {
            doomed.push(rowid);
            if view.root != 0 {
                roots.push(view.root);
            }
        }
        more = cursor.next(pager)?;
    }
    for rowid in doomed {
        mutate::delete_row(pager, schema_root()?, rowid)?;
    }
    Ok(roots)
}

/// One `sqlite_schema` row, as the deletion predicate sees it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SchemaRowView<'a> {
    /// The `type` column.
    pub kind: &'a [u8],
    /// The `name` column.
    pub name: &'a [u8],
    /// The `tbl_name` column.
    pub table: &'a [u8],
    /// The `rootpage` column.
    pub root: u32,
}

/// Returns a record field as text.
fn text_of(record: &inillucent_value::record::RecordRef<'_>, index: usize) -> DbResult<Vec<u8>> {
    match record.value(index)? {
        Value::Text(text) => Ok(text.raw().to_vec()),
        _ => Ok(Vec::new()),
    }
}

/// Returns a record field as a page number.
fn integer_of(record: &inillucent_value::record::RecordRef<'_>, index: usize) -> DbResult<u32> {
    match record.value(index)? {
        Value::Integer(value) => Ok(u32::try_from(value).unwrap_or(0)),
        _ => Ok(0),
    }
}

/// Frees every page of a dropped object's B-tree.
///
/// `sqlite_schema` itself is never freed: page one is the file's own root and
/// a request to drop it is a bug, not a schema change.
pub fn free_root(pager: &mut Pager, root: u32) -> DbResult<()> {
    if root == 0 {
        return Ok(());
    }
    if root == SCHEMA_ROOT {
        return Err(misuse("the schema table cannot be dropped"));
    }
    mutate::drop_tree(pager, PageId::from_persisted(root)?)
}

/// Moves the schema cookie on, which is what invalidates prepared statements.
pub fn bump_schema_cookie(pager: &mut Pager) -> DbResult<u32> {
    let mut header = *pager.header();
    header.schema_cookie = header.schema_cookie.wrapping_add(1);
    let cookie = header.schema_cookie;
    pager.set_header(header)?;
    Ok(cookie)
}

/// Returns the `sqlite_schema` text SQLite stores for a statement.
///
/// `keywords` is the prefix the statement is stored with - "CREATE TABLE" or
/// "CREATE UNIQUE INDEX" - and the rest is the source from the object's name
/// to the end of the statement, with a trailing semicolon and any trailing
/// space removed.
pub fn canonical_sql(keywords: &str, source: &[u8], name_offset: u32, end: u32) -> Vec<u8> {
    let start = name_offset as usize;
    let finish = (end as usize).min(source.len()).max(start);
    let mut tail = source.get(start..finish).unwrap_or(&[]).to_vec();
    while matches!(
        tail.last(),
        Some(b';') | Some(b' ') | Some(b'\n') | Some(b'\r') | Some(b'\t')
    ) {
        tail.pop();
    }
    let mut sql = keywords.as_bytes().to_vec();
    sql.push(b' ');
    sql.extend_from_slice(&tail);
    sql
}

/// Returns the name SQLite gives the `n`th automatic index of a table.
pub fn automatic_index_name(table: &[u8], ordinal: u32) -> Vec<u8> {
    let mut name = b"sqlite_autoindex_".to_vec();
    name.extend_from_slice(table);
    name.push(b'_');
    name.extend_from_slice(ordinal.to_string().as_bytes());
    name
}
