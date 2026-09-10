//! The catalog tree: `sqlite_schema` as an ordinary tree in the new engine's
//! own file.
//!
//! Invariant: a database written by the new engine describes itself. Everything
//! needed to open it and plan against it - what tables exist, what their
//! columns are, which indexes cover them, and which page each tree is rooted at
//! - is in this tree and nowhere else. A reader that has to consult the SQLite
//! file the data was imported from has not read a database; it has read half of
//! two.
//!
//! ## Why `sqlite_schema` is a real table rather than a special case
//!
//! Because it costs nothing to make it one, and a special case would have to be
//! kept in step with the scan operators forever. The catalog tree has the same
//! five columns `sqlite_schema` has, in the same order, with a rowid key -
//! which means `SELECT * FROM sqlite_schema` is planned, scanned and projected
//! by exactly the code path that reads any other table, and the loader below
//! reads it with `PagedTree::visit_leaves` like any other consumer.
//!
//! The one deliberate difference from SQLite is what `rootpage` holds: the page
//! the tree is rooted at *in this file*, not in whatever file the rows were
//! imported from. That is what makes the file self-describing, and it is the
//! same meaning the column has in SQLite - it just refers to a different file.
//!
//! ## Why the schema is stored as text and re-parsed
//!
//! The alternative is a side table of columns, affinities and collations, and
//! it would be a second description of the schema that could disagree with the
//! first. `inillucent-catalog`'s loader already derives all of that from the
//! `CREATE` text with the first-party parser - the same one that parsed the
//! user's statement - so storing the text and re-parsing it keeps one grammar
//! and one derivation. Parsing the whole schema costs microseconds once per
//! open, which the plan cache then amortises away entirely.

use inillucent_base::{error, DbError, DbResult};
use inillucent_pool::{Database, PageId, Pool};
use inillucent_sql::catalog_view::TableInfo;
use inillucent_tree::datum::{Datum, OwnedDatum};
use inillucent_tree::paged::PagedTree;
use inillucent_tree::types::{ColumnSpec, PhysicalType};
use inillucent_tree::write::TreeLog;

use crate::load::{index_from_create_sql, table_from_create_sql};

/// The name the catalog tree answers to in a query.
pub const SCHEMA_TABLE: &[u8] = b"sqlite_schema";

/// The alias SQLite has always accepted for it.
pub const SCHEMA_ALIAS: &[u8] = b"sqlite_master";

/// The tree id the catalog is written under.
///
/// Tree ids identify a tree inside the file the way SQLite's root pages do, and
/// the catalog takes the first so that no imported table can collide with it.
pub const SCHEMA_TREE_ID: u64 = 1;

/// How many columns a catalog row has, not counting the rowid key.
pub const SCHEMA_COLUMNS: usize = 5;

/// What kind of object a catalog row describes.
///
/// The four `type` values SQLite writes. Anything else in the column is a file
/// this engine did not write, and is refused rather than guessed at.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ObjectKind {
    /// A table.
    Table,
    /// An index.
    Index,
    /// A view.
    View,
    /// A trigger.
    Trigger,
}

impl ObjectKind {
    /// Returns the text the `type` column holds.
    pub fn as_text(self) -> &'static [u8] {
        match self {
            ObjectKind::Table => b"table",
            ObjectKind::Index => b"index",
            ObjectKind::View => b"view",
            ObjectKind::Trigger => b"trigger",
        }
    }

    /// Returns the kind a `type` value names.
    ///
    /// @param text - the column's bytes
    pub fn from_text(text: &[u8]) -> Option<ObjectKind> {
        match text {
            b"table" => Some(ObjectKind::Table),
            b"index" => Some(ObjectKind::Index),
            b"view" => Some(ObjectKind::View),
            b"trigger" => Some(ObjectKind::Trigger),
            _ => None,
        }
    }
}

/// One row of the catalog tree.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SchemaEntry {
    /// What kind of object it describes.
    pub kind: ObjectKind,
    /// The object's name.
    pub name: Vec<u8>,
    /// The table it belongs to, which for a table is its own name.
    pub table: Vec<u8>,
    /// The page its tree is rooted at *in this file*.
    pub root: PageId,
    /// The `CREATE` text.
    pub sql: Vec<u8>,
    /// The identifier this object's tree is known by, in the log and here.
    ///
    /// **Persisted, because the log refers to it and the log outlives the
    /// process that wrote it.** Every logical row record carries
    /// `tree: self.tree_id()`, and a `tree_id` was this process's own
    /// bookkeeping: the import numbered trees by the *source* file's SQLite
    /// root pages, DDL numbered them from a counter that restarted at every
    /// open, and a reader that opened the file numbered them again in catalog
    /// order. Three numberings, none of them in the file.
    ///
    /// That was harmless for exactly as long as nothing durable referred to
    /// one. The write-ahead log does, so recovery would have handed a row
    /// record to whichever tree happened to hold the writer's number in the
    /// reader's numbering - a wrong answer rather than a refusal. The
    /// identifier is put in the file so that every process derives the same
    /// one from the same bytes.
    pub tree_id: u64,
    /// What the tree's shape is, so opening the file does not have to walk it.
    pub stats: TreeStats,
}

/// The per-tree statistics the catalog carries beside a root page.
///
/// **Persisted, because otherwise opening a database means walking every tree.**
/// A `PagedTree` handle needs its leftmost leaf, its leaf count and its row
/// count, and none of the three can be read off the root page: the leaf count is
/// the length of the sibling chain and the row count is the sum over it. Phase 2
/// carried them from the build, which works only for a file this process just
/// wrote - opening a file an earlier process wrote is the gap this closes.
///
/// They are also what the planner will read for cardinality once `ANALYZE`
/// exists, which is the second reason the TDD puts them here rather than in a
/// side table: the catalog row is already the thing a plan is built against.
///
/// **Three integers rather than the TDD's "stats blob".** They are fixed-width
/// and the leaf layout stores a fixed-width column as a typed mini-column with
/// no heap slot at all; a blob of the same twenty-four bytes would be a heap
/// allocation per catalog row to hold three integers, and nothing would be able
/// to read one of them without decoding all three.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TreeStats {
    /// The leftmost leaf, where a full scan starts.
    pub first_leaf: PageId,
    /// How many leaves the sibling chain holds.
    pub leaf_count: u64,
    /// How many rows the tree holds.
    pub row_count: u64,
}

/// Returns the column directory a catalog tree is built with.
///
/// The rowid key, then `sqlite_schema`'s five columns in its order. `rootpage`
/// is an integer and the other four are text, which is what SQLite's own
/// declaration says and what makes the tree's mini-columns typed rather than
/// tagged.
pub fn schema_layout() -> Vec<ColumnSpec> {
    vec![
        ColumnSpec::key(PhysicalType::Int64),
        ColumnSpec::new(PhysicalType::Text),
        ColumnSpec::new(PhysicalType::Text),
        ColumnSpec::new(PhysicalType::Text),
        ColumnSpec::new(PhysicalType::Int64),
        ColumnSpec::new(PhysicalType::Text),
        // The three statistics columns, which `sqlite_schema` does not declare
        // and no query can name. The TDD's catalog row is wider than
        // `sqlite_schema`'s and `sqlite_schema` is the *view* over it; a layout
        // that maps the five declared columns onto tree columns one to five is
        // what makes that true without any view machinery, because a tree
        // column nothing projects is a tree column nothing reads.
        ColumnSpec::new(PhysicalType::Int64),
        ColumnSpec::new(PhysicalType::Int64),
        ColumnSpec::new(PhysicalType::Int64),
        // The tree identifier, which the log refers to. It is last so that the
        // five columns `sqlite_schema` shows keep mapping onto tree columns one
        // to five: a column added at the end is a column no query can name.
        ColumnSpec::new(PhysicalType::Int64),
    ]
}

/// How many columns the catalog tree has, the rowid key included.
pub const CATALOG_WIDTH: usize = 10;

/// How many of them `sqlite_schema` shows, the rowid key included.
pub const SCHEMA_VIEW_WIDTH: usize = 6;

/// Returns the `CREATE TABLE` text the catalog tree describes itself with.
///
/// `sqlite_schema` is a table like any other, so it needs a declaration for the
/// binder to resolve `SELECT name FROM sqlite_schema` against. This is the
/// declaration SQLite documents, word for word, so a query written against one
/// engine's catalog reads the other's.
pub fn schema_create_sql() -> &'static [u8] {
    b"CREATE TABLE sqlite_schema(type TEXT, name TEXT, tbl_name TEXT, rootpage INTEGER, sql TEXT)"
}

/// Writes the catalog tree and returns it.
///
/// The rows are numbered from one in the order given, which is the order they
/// were created in - the same order SQLite's own `sqlite_schema` holds them,
/// and the order an unordered `SELECT` from it comes back in.
///
/// @param database - the file to write into
/// @param entries - the objects to record
pub fn write_catalog(database: &mut Database, entries: &[SchemaEntry]) -> DbResult<PagedTree> {
    let mut owned: Vec<Vec<OwnedDatum>> = Vec::with_capacity(entries.len());
    for (nth, entry) in entries.iter().enumerate() {
        owned.push(catalog_row(nth.saturating_add(1) as i64, entry));
    }
    let rows: Vec<Vec<Datum<'_>>> = owned
        .iter()
        .map(|row| row.iter().map(OwnedDatum::borrow).collect())
        .collect();
    let tree = PagedTree::bulk_build(database, SCHEMA_TREE_ID, schema_layout(), 1, &rows)?;
    database.set_catalog_root(tree.root());
    Ok(tree)
}

/// Attaches to a catalog tree already in the file.
///
/// @param pool - the buffer pool the file is open through
/// @param root - the catalog root, from the meta page
pub fn attach_catalog(pool: &Pool, root: PageId) -> DbResult<PagedTree> {
    if root.is_none() {
        return Err(error::corrupt(
            "the database has no catalog tree, so nothing in it can be named",
        ));
    }
    PagedTree::attach_scanned(pool, SCHEMA_TREE_ID, root, schema_layout(), 1)
}

/// Reads every row of a catalog tree.
///
/// @param pool - the buffer pool the file is open through
/// @param tree - the catalog tree
pub fn read_catalog(pool: &Pool, tree: &PagedTree) -> DbResult<Vec<SchemaEntry>> {
    Ok(read_catalog_rows(pool, tree)?
        .into_iter()
        .map(|(_, entry)| entry)
        .collect())
}

/// Reads every row of a catalog tree, with the rowid each is stored under.
///
/// DDL needs the rowid and a reader does not, but there is one walk rather than
/// two: a catalog read that missed a row would be a schema missing an object,
/// and two walks is two chances to miss it differently.
///
/// **`live` rather than the packed rows.** A `CREATE TABLE` puts its catalog row
/// in the leaf's delta area like any other insert, so a walk over
/// `0..row_count()` reads the catalog as it was when the tree was last packed.
/// Reopening a file after a `CREATE TABLE` then produced a schema without the
/// table in it while `SELECT * FROM sqlite_schema` - which goes through the
/// ordinary scan, which merges - listed it.
///
/// @param pool - the buffer pool the file is open through
/// @param tree - the catalog tree
pub fn read_catalog_rows(pool: &Pool, tree: &PagedTree) -> DbResult<Vec<(i64, SchemaEntry)>> {
    let mut entries = Vec::new();
    let mut failure: Option<DbError> = None;
    tree.visit_leaves(pool, &mut |leaf: &inillucent_tree::leaf::LeafRef<'_>| {
        for row in leaf.live()? {
            let rowid = match row.first() {
                Some(Datum::Int(number)) => *number,
                _ => {
                    failure = Some(error::corrupt("a catalog row's key is not a rowid"));
                    return Ok(false);
                }
            };
            match entry_of(&row) {
                Ok(entry) => entries.push((rowid, entry)),
                Err(error) => {
                    // The walk stops at the first bad row rather than carrying
                    // on: a catalog that cannot be read is not a catalog with
                    // one row missing, it is a file that cannot be opened.
                    failure = Some(error);
                    return Ok(false);
                }
            }
        }
        Ok(true)
    })?;
    match failure {
        Some(error) => Err(error),
        None => Ok(entries),
    }
}

/// Returns one catalog row as the values the tree holds.
///
/// @param rowid - the key the row is stored under
/// @param entry - the object it describes
pub fn catalog_row(rowid: i64, entry: &SchemaEntry) -> Vec<OwnedDatum> {
    vec![
        OwnedDatum::Int(rowid),
        OwnedDatum::Text(entry.kind.as_text().to_vec()),
        OwnedDatum::Text(entry.name.clone()),
        OwnedDatum::Text(entry.table.clone()),
        OwnedDatum::Int(entry.root.0 as i64),
        // **An automatic index's statement is NULL, not empty text.** SQLite
        // writes NULL for an index a constraint produced, because there is no
        // statement the user wrote; a reader tells the two apart by asking
        // whether the column is null, and `SELECT sql FROM sqlite_schema` shows
        // the difference. An empty string here is a different answer to the
        // same query.
        match entry.sql.is_empty() {
            true => OwnedDatum::Null,
            false => OwnedDatum::Text(entry.sql.clone()),
        },
        OwnedDatum::Int(entry.stats.first_leaf.0 as i64),
        OwnedDatum::Int(entry.stats.leaf_count as i64),
        OwnedDatum::Int(entry.stats.row_count as i64),
        OwnedDatum::Int(entry.tree_id as i64),
    ]
}

/// Adds one object to the catalog tree.
///
/// The row goes in through the ordinary write path, which means it is logged,
/// it is undone by the same rollback that undoes an `INSERT`, and there is no
/// second way to write a catalog row that could disagree with the first.
///
/// @param database - the file
/// @param tree - the catalog tree
/// @param log - where the record goes
/// @param rowid - the key to store it under
/// @param entry - the object to record
pub fn insert_entry(
    database: &mut Database,
    tree: &mut PagedTree,
    log: &mut dyn TreeLog,
    rowid: i64,
    entry: &SchemaEntry,
) -> DbResult<()> {
    let owned = catalog_row(rowid, entry);
    let row: Vec<Datum<'_>> = owned.iter().map(OwnedDatum::borrow).collect();
    tree.insert(database, log, &row)?;
    Ok(())
}

/// Removes one object from the catalog tree.
///
/// @param database - the file
/// @param tree - the catalog tree
/// @param log - where the record goes
/// @param rowid - the key it is stored under
pub fn delete_entry(
    database: &mut Database,
    tree: &mut PagedTree,
    log: &mut dyn TreeLog,
    rowid: i64,
) -> DbResult<bool> {
    let key = [Datum::Int(rowid)];
    Ok(tree.delete(database, log, &key)?.is_some())
}

/// Reads one catalog row out of the values a leaf holds.
///
/// @param row - the row's values, rowid first
/// Returns the object one catalog row describes.
///
/// **Public because recovery needs it row by row.** A replay reads catalog rows
/// out of the log one at a time, as the records that wrote them come past, and
/// has to learn the shape of a tree created since the last checkpoint before the
/// rows of that tree arrive. Decoding the row a second time in the engine would
/// be a second decoder that could disagree with this one about which column is
/// which - and the columns are what the format is.
///
/// @param row - the row's values, rowid first
pub fn entry_from_row(row: &[Datum<'_>]) -> DbResult<SchemaEntry> {
    entry_of(row)
}

fn entry_of(row: &[Datum<'_>]) -> DbResult<SchemaEntry> {
    let kind = ObjectKind::from_text(&text_at(row, 1)?).ok_or_else(|| {
        error::corrupt("a catalog row's type is not one of table, index, view or trigger")
    })?;
    let root = match row.get(4) {
        Some(Datum::Int(page)) if *page >= 0 => PageId(*page as u64),
        Some(Datum::Null) | None => PageId::NONE,
        _ => {
            return Err(error::corrupt(
                "a catalog row's rootpage is not a page number",
            ))
        }
    };
    Ok(SchemaEntry {
        kind,
        name: text_at(row, 2)?,
        table: text_at(row, 3)?,
        root,
        sql: text_at(row, 5)?,
        stats: TreeStats {
            first_leaf: PageId(counter_at(row, 6)? as u64),
            leaf_count: counter_at(row, 7)? as u64,
            row_count: counter_at(row, 8)? as u64,
        },
        tree_id: counter_at(row, 9)? as u64,
    })
}

/// Reads one statistics column, refusing anything that is not a count.
///
/// A row written before the statistics existed has six columns rather than
/// nine, so the seventh is absent and reads as zero - which is the same thing
/// "unknown" has always meant here: a caller that gets zero leaves rescans the
/// tree.
///
/// @param row - the row's values
/// @param column - which column
fn counter_at(row: &[Datum<'_>], column: usize) -> DbResult<i64> {
    match row.get(column) {
        Some(Datum::Int(number)) if *number >= 0 => Ok(*number),
        Some(Datum::Null) | None => Ok(0),
        _ => Err(error::corrupt(
            "a catalog row's statistics column is not a count",
        )),
    }
}

/// Reads one text column of one row, refusing anything that is not text.
///
/// @param row - the row's values
/// @param column - which column
fn text_at(row: &[Datum<'_>], column: usize) -> DbResult<Vec<u8>> {
    match row.get(column) {
        Some(Datum::Text(bytes)) => Ok(bytes.to_vec()),
        Some(Datum::Null) | None => Ok(Vec::new()),
        _ => Err(error::corrupt("a catalog row's text column is not text")),
    }
}

/// Builds the binder's table list from a catalog tree's rows.
///
/// Tables come first and indexes attach to them, so the entries are walked
/// twice: an index whose table has not been read yet has nothing to attach to,
/// and the creation order does not guarantee otherwise once a table has been
/// dropped and recreated.
///
/// @param entries - the catalog rows
/// @param database - which attached database these belong to
pub fn tables_from_catalog(entries: &[SchemaEntry], database: usize) -> DbResult<Vec<TableInfo>> {
    let roots = entries
        .iter()
        .map(root_as_u32)
        .collect::<DbResult<Vec<u32>>>()?;
    tables_from_entries(entries, &roots, database)
}

/// Builds the binder's table list from catalog rows and the identifiers their
/// trees are registered under.
///
/// **Two arrays rather than one**, because the identifier a tree is registered
/// under is not the page its root sits on. A file the engine built for itself
/// can use the page for both - which is what [`tables_from_catalog`] does - but
/// a fixture imported from SQLite registers its trees under the *fixture's* root
/// pages, and DDL registers a created tree under a number chosen before the
/// tree exists. The catalog row is the same row in all three cases; only the
/// identifier differs, so it is passed alongside.
///
/// Views and triggers are derived here too, which is the difference between a
/// catalog a reader can query and one it can only list: a `DROP TRIGGER` has to
/// find the trigger, and it finds it on the table it is attached to.
///
/// @param entries - the catalog rows
/// @param roots - the identifier for each row's tree, in the same order
/// @param database - which attached database these belong to
pub fn tables_from_entries(
    entries: &[SchemaEntry],
    roots: &[u32],
    database: usize,
) -> DbResult<Vec<TableInfo>> {
    let mut tables: Vec<TableInfo> = Vec::new();
    for (position, entry) in entries.iter().enumerate() {
        if entry.kind == ObjectKind::View {
            tables.push(view_info(entry, database)?);
            continue;
        }
        if entry.kind != ObjectKind::Table {
            continue;
        }
        let root = roots.get(position).copied().unwrap_or(0);
        let mut info = table_from_create_sql(&entry.sql, database, root)
            .map_err(|error| error.with_detail(format!("in table {}", name_of(entry))))?;
        // The declared name wins over whatever the CREATE text spelled, because
        // the catalog row is what the tree was written under.
        info.name = entry.name.clone();
        info.folded = entry.name.to_ascii_lowercase();
        tables.push(info);
    }
    for (position, entry) in entries.iter().enumerate() {
        if entry.kind != ObjectKind::Index {
            continue;
        }
        let root = roots.get(position).copied().unwrap_or(0);
        let folded = entry.table.to_ascii_lowercase();
        let Some(table) = tables.iter_mut().find(|table| table.folded == folded) else {
            // An index whose table is not here describes a table this file does
            // not have. That is a corrupt catalog rather than something to skip
            // quietly: a plan built against it would seek a tree that is not
            // there.
            return Err(error::corrupt(format!(
                "the catalog has an index on {}, which it has no table for",
                String::from_utf8_lossy(&entry.table)
            )));
        };
        if entry.sql.is_empty() {
            // An automatic index: the table's own constraints already produced
            // the entry and only its root page was missing.
            let wanted = entry.name.to_ascii_lowercase();
            if let Some(existing) = table
                .indexes
                .iter_mut()
                .find(|index| index.folded == wanted)
            {
                existing.root = root;
            }
            continue;
        }
        let index = index_from_create_sql(&entry.sql, table, root)
            .map_err(|error| error.with_detail(format!("in index {}", name_of(entry))))?;
        table.indexes.push(index);
    }
    for entry in entries {
        if entry.kind != ObjectKind::Trigger {
            continue;
        }
        let folded = entry.table.to_ascii_lowercase();
        let Some(table) = tables.iter_mut().find(|table| table.folded == folded) else {
            // A trigger whose table is gone is dropped with it, so a row that
            // outlived its table is a catalog that is mid-drop rather than one
            // to refuse. Leaving it unattached is what the old loader does with
            // the same shape.
            continue;
        };
        let trigger = crate::load::trigger_from_create_sql(&entry.sql)
            .map_err(|error| error.with_detail(format!("in trigger {}", name_of(entry))))?;
        table.triggers.push(trigger);
    }
    Ok(tables)
}

/// Builds a view's entry in the binder's table list.
///
/// A view is a table with no tree and a parsed body. The body is parsed here,
/// once, and kept, so a view named twice in one statement is two binds of one
/// arena rather than two parses.
///
/// @param entry - the catalog row
/// @param database - which attached database it belongs to
fn view_info(entry: &SchemaEntry, database: usize) -> DbResult<TableInfo> {
    let body = crate::load::view_from_create_sql(&entry.sql)
        .map_err(|error| error.with_detail(format!("in view {}", name_of(entry))))?;
    Ok(TableInfo {
        name: entry.name.clone(),
        folded: entry.name.to_ascii_lowercase(),
        database,
        // A view has no tree, and zero is the root a table with no b-tree
        // carries everywhere else in this workspace.
        root: 0,
        columns: Vec::new(),
        rowid_alias: None,
        without_rowid: false,
        strict: false,
        autoincrement: false,
        kind: inillucent_sql::catalog_view::TableKind::View,
        create_sql: entry.sql.clone(),
        view: Some(Box::new(body)),
        triggers: Vec::new(),
        analysed_rows: None,
        indexes: Vec::new(),
        checks: Vec::new(),
        foreign_keys: Vec::new(),
        foreign_key_triggers: Vec::new(),
        module: None,
    })
}

/// Returns a catalog row's root page as the `u32` the binder's tables hold.
///
/// The binder inherited a 32-bit root page from SQLite's format. A file large
/// enough to root a tree past four billion pages cannot be described by it, and
/// truncating would point every plan at the wrong tree - so it is refused.
///
/// @param entry - the row
fn root_as_u32(entry: &SchemaEntry) -> DbResult<u32> {
    u32::try_from(entry.root.0).map_err(|_| {
        error::corrupt(format!(
            "{} is rooted past the largest page a catalog entry can name",
            name_of(entry)
        ))
    })
}

/// Returns a row's name for an error message.
fn name_of(entry: &SchemaEntry) -> String {
    String::from_utf8_lossy(&entry.name).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use inillucent_pool::Options;
    use inillucent_vfs::os::OsVfs;
    use inillucent_vfs::path::DbPath;

    /// Builds a database in a fresh directory and returns it with its path.
    ///
    /// @param tag - a name for the directory, so runs do not collide
    fn fresh(tag: &str) -> (OsVfs, std::path::PathBuf, DbPath) {
        // A lib test has no `CARGO_TARGET_TMPDIR`, so the run gets its own
        // directory under the crate's target directory instead.
        let directory = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/catalog-tests")
            .join(tag);
        let _ = std::fs::create_dir_all(&directory);
        let file = directory.join("catalog.rdb");
        let _ = std::fs::remove_file(&file);
        let path = DbPath::new(file.to_string_lossy().as_ref());
        (OsVfs::new(), file, path)
    }

    /// The three entries a small schema produces.
    fn entries() -> Vec<SchemaEntry> {
        vec![
            SchemaEntry {
                kind: ObjectKind::Table,
                name: b"people".to_vec(),
                table: b"people".to_vec(),
                root: PageId(4),
                sql: b"CREATE TABLE people(id INTEGER PRIMARY KEY, team TEXT COLLATE NOCASE)"
                    .to_vec(),
                stats: TreeStats::default(),
                tree_id: 101,
            },
            SchemaEntry {
                kind: ObjectKind::Index,
                name: b"people_by_team".to_vec(),
                table: b"people".to_vec(),
                root: PageId(9),
                sql: b"CREATE INDEX people_by_team ON people(team)".to_vec(),
                stats: TreeStats::default(),
                tree_id: 102,
            },
            SchemaEntry {
                kind: ObjectKind::Table,
                name: SCHEMA_TABLE.to_vec(),
                table: SCHEMA_TABLE.to_vec(),
                root: PageId(2),
                sql: schema_create_sql().to_vec(),
                stats: TreeStats::default(),
                tree_id: 103,
            },
        ]
    }

    #[test]
    fn a_catalog_survives_a_checkpoint_and_reopen() {
        let (vfs, _file, path) = fresh("catalog-roundtrip");
        let written = entries();
        let root = {
            let mut database = Database::create(&vfs, &path, Options::default().with_frames(64))
                .expect("the database is created");
            let tree = write_catalog(&mut database, &written).expect("the catalog is written");
            let root = tree.root();
            database.checkpoint().expect("the checkpoint succeeds");
            root
        };
        let database = Database::open(&vfs, &path, 64).expect("the database reopens");
        assert_eq!(
            database.catalog_root(),
            root,
            "the meta page remembers where the catalog is"
        );
        let tree =
            attach_catalog(database.pool(), database.catalog_root()).expect("the catalog attaches");
        let read = read_catalog(database.pool(), &tree).expect("the catalog reads");
        assert_eq!(read, written, "every row comes back exactly as written");
    }

    #[test]
    fn the_binders_tables_come_back_with_their_indexes_attached() {
        let (vfs, _file, path) = fresh("catalog-tables");
        let mut database = Database::create(&vfs, &path, Options::default().with_frames(64))
            .expect("the database is created");
        write_catalog(&mut database, &entries()).expect("the catalog is written");
        let tree =
            attach_catalog(database.pool(), database.catalog_root()).expect("the catalog attaches");
        let read = read_catalog(database.pool(), &tree).expect("the catalog reads");
        let tables = tables_from_catalog(&read, 0).expect("the tables build");
        assert_eq!(tables.len(), 2, "people and sqlite_schema");
        let people = tables
            .iter()
            .find(|table| table.folded == b"people")
            .expect("people is there");
        assert_eq!(people.root, 4, "the root is the one in this file");
        assert_eq!(people.indexes.len(), 1);
        assert_eq!(people.indexes[0].root, 9);
        // The collation survived the round trip through the CREATE text, which
        // is the whole reason the text is what gets stored.
        let team = people
            .columns
            .iter()
            .find(|column| column.folded == b"team")
            .expect("team is there");
        assert_eq!(team.collation.to_ascii_uppercase(), b"NOCASE");
    }

    #[test]
    fn sqlite_schema_describes_itself_well_enough_to_query() {
        let (vfs, _file, path) = fresh("catalog-selfdescribe");
        let mut database = Database::create(&vfs, &path, Options::default().with_frames(64))
            .expect("the database is created");
        write_catalog(&mut database, &entries()).expect("the catalog is written");
        let tree =
            attach_catalog(database.pool(), database.catalog_root()).expect("the catalog attaches");
        let read = read_catalog(database.pool(), &tree).expect("the catalog reads");
        let tables = tables_from_catalog(&read, 0).expect("the tables build");
        let schema = tables
            .iter()
            .find(|table| table.folded == SCHEMA_TABLE)
            .expect("sqlite_schema is a table like any other");
        let names: Vec<&[u8]> = schema
            .columns
            .iter()
            .map(|column| column.folded.as_slice())
            .collect();
        assert_eq!(
            names,
            vec![
                b"type".as_slice(),
                b"name".as_slice(),
                b"tbl_name".as_slice(),
                b"rootpage".as_slice(),
                b"sql".as_slice()
            ]
        );
    }

    #[test]
    fn an_empty_catalog_is_a_tree_with_no_rows_rather_than_no_tree() {
        let (vfs, _file, path) = fresh("catalog-empty");
        let mut database = Database::create(&vfs, &path, Options::default().with_frames(64))
            .expect("the database is created");
        let tree = write_catalog(&mut database, &[]).expect("the catalog is written");
        assert!(!tree.root().is_none());
        let read = read_catalog(database.pool(), &tree).expect("the catalog reads");
        assert!(read.is_empty());
        assert!(tables_from_catalog(&read, 0)
            .expect("nothing to build")
            .is_empty());
    }

    #[test]
    fn a_missing_catalog_root_is_refused_rather_than_read_as_page_zero() {
        let (vfs, _file, path) = fresh("catalog-missing");
        let database = Database::create(&vfs, &path, Options::default().with_frames(64))
            .expect("the database is created");
        let refusal = attach_catalog(database.pool(), PageId::NONE)
            .expect_err("a database with no catalog cannot be named against");
        assert!(
            refusal
                .detail()
                .unwrap_or_default()
                .contains("no catalog tree"),
            "{refusal:?}"
        );
    }

    #[test]
    fn an_index_whose_table_is_absent_is_a_corrupt_catalog() {
        let orphan = vec![SchemaEntry {
            kind: ObjectKind::Index,
            name: b"orphan".to_vec(),
            table: b"gone".to_vec(),
            root: PageId(3),
            sql: b"CREATE INDEX orphan ON gone(x)".to_vec(),
            stats: TreeStats::default(),
            tree_id: 104,
        }];
        let refusal = tables_from_catalog(&orphan, 0).expect_err("it is refused");
        assert!(
            refusal
                .detail()
                .unwrap_or_default()
                .contains("no table for"),
            "{refusal:?}"
        );
    }

    #[test]
    fn the_four_object_kinds_round_trip_through_their_text() {
        for kind in [
            ObjectKind::Table,
            ObjectKind::Index,
            ObjectKind::View,
            ObjectKind::Trigger,
        ] {
            assert_eq!(ObjectKind::from_text(kind.as_text()), Some(kind));
        }
        assert_eq!(ObjectKind::from_text(b"sequence"), None);
    }
}
