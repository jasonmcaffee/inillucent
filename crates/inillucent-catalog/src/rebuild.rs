//! Rebuilding a database into a fresh file, which is what `VACUUM` is.
//!
//! Invariant: a rebuild changes nothing a query can see. Every schema row is
//! carried across with its text untouched, every table keeps its rowids, and
//! every index keeps its entries; what changes is only where those bytes sit.
//! That is the whole promise of `VACUUM` - the file gets smaller and no answer
//! moves - and it is why the copy works on trees and payloads rather than on
//! rows: a record re-encoded on the way through would be a second chance to
//! change something.
//!
//! The trees are copied in key order into empty trees, so every page comes out
//! densely packed and the free list comes out empty. Nothing is re-sorted,
//! because the source is already in order.

use inillucent_base::ids::PageId;
use inillucent_base::limits::Limits;
use inillucent_base::{error, DbResult};
use inillucent_storage::cursor::BTreeCursor;
use inillucent_storage::pager::Pager;
use inillucent_storage::schema::SchemaKind;
use inillucent_value::record::KeyInfo;

use crate::ddl::{self, SchemaRow};

/// Copies every object of one database into another, empty one.
///
/// The target must be a database with the source's page size, reserved-byte
/// count and text encoding: the caller then copies the finished file back page
/// for page, and a page of a different size could not be. The schema rows are
/// written in the order they were read, which is creation order, so an index
/// never lands before the table it belongs to.
pub fn rebuild_into(source: &mut Pager, target: &mut Pager) -> DbResult<Option<i64>> {
    if target.page_size().bytes() != source.page_size().bytes() {
        return Err(error::misuse(
            "a rebuild target must have the source's page size",
        ));
    }
    let rows = ddl::read_schema_rows(source)?;
    for (_, row) in &rows {
        let moved = copy_object(source, target, row)?;
        ddl::insert_schema_row(target, &moved)?;
    }
    carry_header(source, target)?;
    Ok(published_rowid(&rows))
}

/// Returns the value `last_insert_rowid()` reads after a rebuild, if it moves.
///
/// This is an artefact of how the reference builds the copy, and it is
/// reproduced deliberately because an application can see it. SQLite copies a
/// view's or a trigger's `sqlite_schema` row in a final pass, and that INSERT
/// sets the last insert rowid to the row it allocated - which, since that pass
/// goes last, is the number of rows in the rebuilt schema. A schema with
/// neither leaves the value alone, because then the final pass inserts nothing.
///
/// Measured against 3.53.4 on six schemas: one view leaves it at 2 over
/// `(table, view)` and at 3 over `(table, view, table)`, one trigger at 2, and
/// a table with rows and no view or trigger leaves 9 where it was.
fn published_rowid(rows: &[(i64, SchemaRow)]) -> Option<i64> {
    let final_pass = rows
        .iter()
        .any(|(_, row)| matches!(row.kind, SchemaKind::View | SchemaKind::Trigger));
    final_pass.then_some(rows.len() as i64)
}

/// Copies one object's B-tree, returning the row that describes it in the
/// target.
///
/// A view or a trigger owns no tree, so its row is carried across unchanged. A
/// table or index gets a fresh root and its contents appended in order.
fn copy_object(source: &mut Pager, target: &mut Pager, row: &SchemaRow) -> DbResult<SchemaRow> {
    let mut moved = row.clone();
    if row.root == 0 {
        return Ok(moved);
    }
    let table_tree = is_table_tree(source, row)?;
    let root = if table_tree {
        ddl::allocate_table_root(target)?
    } else {
        ddl::allocate_index_root(target)?
    };
    moved.root = root;
    let Some(from) = PageId::new(row.root) else {
        return Err(error::corrupt("a schema row names page zero as a root"));
    };
    let Some(to) = PageId::new(root) else {
        return Err(error::corrupt("a fresh root came back as page zero"));
    };
    if table_tree {
        copy_table(source, target, from, to)
    } else {
        copy_index(source, target, from, to)
    }?;
    Ok(moved)
}

/// Returns whether an object's root holds a table B-tree.
///
/// Read off the page rather than guessed from the row's type, because a
/// `WITHOUT ROWID` table's root is an *index* B-tree while its row says
/// `table`. Copying it as a table tree would ask its cells for rowids they do
/// not carry.
fn is_table_tree(source: &mut Pager, row: &SchemaRow) -> DbResult<bool> {
    if row.kind == SchemaKind::Index {
        return Ok(false);
    }
    let Some(page) = PageId::new(row.root) else {
        return Ok(true);
    };
    let pin = source.get_page(page)?;
    let Some(first) = pin.bytes().first().copied() else {
        return Err(error::corrupt("a root page with no bytes"));
    };
    // 2 and 10 are the interior and leaf index page types; 5 and 13 are the
    // table ones.
    Ok(first == 5 || first == 13)
}

/// Appends every row of one table B-tree to another, in rowid order.
fn copy_table(source: &mut Pager, target: &mut Pager, from: PageId, to: PageId) -> DbResult<()> {
    let limits = Limits::default();
    let mut cursor = BTreeCursor::table(from);
    let mut more = cursor.first(source)?;
    while more {
        let rowid = cursor.rowid()?;
        let payload = cursor.payload(source, &limits)?;
        // Appended rather than inserted: the source is already in rowid order,
        // so every row lands at the right-hand edge and the pages come out
        // full instead of half full.
        inillucent_storage::mutate::append_row(target, to, rowid, &payload)?;
        more = cursor.next(source)?;
    }
    Ok(())
}

/// Appends every entry of one index B-tree to another, in key order.
fn copy_index(source: &mut Pager, target: &mut Pager, from: PageId, to: PageId) -> DbResult<()> {
    let limits = Limits::default();
    // The walk needs no comparisons - `first` and `next` follow the tree's own
    // order - so the key description is not needed to read it, and the entries
    // are appended in the order they come out.
    let mut cursor = BTreeCursor::index(from, KeyInfo::default());
    let mut more = cursor.first(source)?;
    while more {
        let payload = cursor.payload(source, &limits)?;
        inillucent_storage::mutate::append_entry(target, to, &payload)?;
        more = cursor.next(source)?;
    }
    Ok(())
}

/// Carries the header fields a rebuild must preserve, and moves the cookie.
///
/// The page count, free list and largest-root are the target's own - they are
/// what the rebuild changed. Everything an application set is the source's, and
/// the schema cookie moves because every root page in the file is new: a
/// prepared statement compiled against the old roots must not run against
/// these.
fn carry_header(source: &Pager, target: &mut Pager) -> DbResult<()> {
    let from = *source.header();
    let mut header = *target.header();
    header.schema_format = from.schema_format;
    header.text_encoding = from.text_encoding;
    header.user_version = from.user_version;
    header.application_id = from.application_id;
    header.cache_size = from.cache_size;
    header.schema_cookie = from.schema_cookie.wrapping_add(1);
    target.set_header(header)
}
