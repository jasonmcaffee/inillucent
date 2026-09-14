//! Reading `sqlite_schema` far enough to find the tables and indexes.
//!
//! Invariant: this module reads the schema table as *bytes*, and does not
//! parse the SQL in it. Phase 3's job is to find the root pages a cursor needs
//! and to report the CREATE statement verbatim; deciding what the statement
//! means is the catalog's job, several layers up, and doing it here would put
//! a SQL parser inside the storage engine - which the first-party charter
//! forbids for a reason, and which would also make a corrupt schema string a
//! parse error rather than a row someone can look at.
//!
//! `sqlite_schema` is an ordinary table B-tree rooted at page 1, with five
//! columns: type, name, tbl_name, rootpage, sql. Its own root is fixed, which
//! is what makes bootstrapping possible at all.

use inillucent_base::error::corrupt;
use inillucent_base::ids::PageId;
use inillucent_base::limits::Limits;
use inillucent_base::DbResult;
use inillucent_value::record::RecordRef;
use inillucent_value::Value;

use crate::cursor::BTreeCursor;
use crate::pager::Pager;

/// The page the schema table is always rooted at.
pub const SCHEMA_ROOT: u32 = 1;

/// What kind of object a schema row describes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SchemaKind {
    /// A table, which has a root page unless it is virtual.
    Table,
    /// An index, which always has a root page.
    Index,
    /// A view, which has no root page.
    View,
    /// A trigger, which has no root page.
    Trigger,
}

impl SchemaKind {
    /// Returns the kind a schema row's type column names.
    pub fn from_text(text: &[u8]) -> DbResult<SchemaKind> {
        Ok(match text {
            b"table" => SchemaKind::Table,
            b"index" => SchemaKind::Index,
            b"view" => SchemaKind::View,
            b"trigger" => SchemaKind::Trigger,
            other => {
                return Err(corrupt(format!(
                    "a schema row of unknown type {:?}",
                    String::from_utf8_lossy(other)
                )))
            }
        })
    }

    /// Returns the text SQLite stores for this kind.
    pub fn as_text(self) -> &'static str {
        match self {
            SchemaKind::Table => "table",
            SchemaKind::Index => "index",
            SchemaKind::View => "view",
            SchemaKind::Trigger => "trigger",
        }
    }

    /// Reports whether an object of this kind owns a B-tree.
    pub fn has_root_page(self) -> bool {
        matches!(self, SchemaKind::Table | SchemaKind::Index)
    }
}

/// One row of `sqlite_schema`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SchemaObject {
    /// The rowid the row is stored under.
    pub rowid: i64,
    /// What kind of object this is.
    pub kind: SchemaKind,
    /// The object's own name.
    pub name: String,
    /// The table the object belongs to, which is the name again for a table.
    pub table_name: String,
    /// The root page, when the object has one.
    ///
    /// A virtual table and a table whose root page is zero have none, and a
    /// view or trigger never does.
    pub root_page: Option<PageId>,
    /// The CREATE statement, verbatim and unparsed.
    pub sql: Option<String>,
}

impl SchemaObject {}

/// Reads every row of `sqlite_schema`.
///
/// The rows come back in rowid order, which is the order SQLite wrote them and
/// therefore the order a `CREATE` was issued in.
pub fn load_schema(pager: &mut Pager) -> DbResult<Vec<SchemaObject>> {
    let limits = Limits::default();
    let root = PageId::from_persisted(SCHEMA_ROOT)?;
    let encoding = pager.text_encoding();
    let mut cursor = BTreeCursor::table(root);
    let mut objects = Vec::new();
    let mut more = cursor.first(pager)?;
    while more {
        let rowid = cursor.rowid()?;
        let payload = cursor.payload(pager, &limits)?;
        let record = RecordRef::parse_with_limits(&payload, encoding, &limits)?;
        objects.push(decode_schema_row(rowid, &record)?);
        more = cursor.next(pager)?;
    }
    cursor.reset();
    Ok(objects)
}

/// Finds one object by name, without reading the whole schema.
///
/// The schema table is not indexed by name, so this is still a scan; it exists
/// so that a caller looking for one table does not materialise every row's SQL
/// string, which on a large schema is most of the work.
pub fn find_object(pager: &mut Pager, name: &str) -> DbResult<Option<SchemaObject>> {
    let limits = Limits::default();
    let root = PageId::from_persisted(SCHEMA_ROOT)?;
    let encoding = pager.text_encoding();
    let mut cursor = BTreeCursor::table(root);
    let mut more = cursor.first(pager)?;
    while more {
        let rowid = cursor.rowid()?;
        let payload = cursor.payload(pager, &limits)?;
        let record = RecordRef::parse_with_limits(&payload, encoding, &limits)?;
        let found = text_column(&record, 1)?;
        if found.as_deref() == Some(name) {
            let object = decode_schema_row(rowid, &record)?;
            cursor.reset();
            return Ok(Some(object));
        }
        more = cursor.next(pager)?;
    }
    cursor.reset();
    Ok(None)
}

/// Returns every root page the schema names, in page order.
///
/// The integrity check walks these, and so does anything that has to decide
/// whether a page is reachable at all.
pub fn root_pages(objects: &[SchemaObject]) -> Vec<PageId> {
    let mut roots: Vec<PageId> = objects
        .iter()
        .filter_map(|object| object.root_page)
        .collect();
    roots.sort_unstable_by_key(|page| page.get());
    roots.dedup_by_key(|page| page.get());
    roots
}

/// Decodes one schema record into an object.
fn decode_schema_row(rowid: i64, record: &RecordRef<'_>) -> DbResult<SchemaObject> {
    if record.field_count() < 5 {
        return Err(corrupt(format!(
            "a schema row with {} columns rather than five",
            record.field_count()
        )));
    }
    let kind_text = text_column(record, 0)?
        .ok_or_else(|| corrupt("a schema row whose type column is not text"))?;
    let kind = SchemaKind::from_text(kind_text.as_bytes())?;
    let name =
        text_column(record, 1)?.ok_or_else(|| corrupt("a schema row whose name is not text"))?;
    let table_name = text_column(record, 2)?
        .ok_or_else(|| corrupt("a schema row whose tbl_name is not text"))?;

    let root_page = match record.value(3)? {
        Value::Integer(0) | Value::Null => None,
        Value::Integer(page) => {
            let page = u32::try_from(page)
                .map_err(|_| corrupt(format!("a schema row with root page {page}")))?;
            Some(PageId::from_persisted(page)?)
        }
        other => {
            return Err(corrupt(format!(
                "a schema row whose root page is {}",
                other.storage_class().typeof_name()
            )))
        }
    };
    if root_page.is_some() && !kind.has_root_page() {
        return Err(corrupt(format!(
            "a {} named {name} with a root page",
            kind.as_text()
        )));
    }

    Ok(SchemaObject {
        rowid,
        kind,
        name,
        table_name,
        root_page,
        sql: text_column(record, 4)?,
    })
}

/// Returns a record column as a string, or `None` when it is NULL.
fn text_column(record: &RecordRef<'_>, index: usize) -> DbResult<Option<String>> {
    Ok(match record.value(index)? {
        Value::Null => None,
        Value::Text(text) => Some(String::from_utf8_lossy(text.utf8_bytes().as_ref()).into_owned()),
        // SQLite will hand back whatever is stored, and a schema written by a
        // different tool may hold a blob here. Reading it as bytes is more
        // useful than refusing the whole schema.
        Value::Blob(blob) => Some(String::from_utf8_lossy(blob.raw()).into_owned()),
        _ => None,
    })
}
