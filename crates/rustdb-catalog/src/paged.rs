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
//! first. `rustdb-catalog`'s loader already derives all of that from the
//! `CREATE` text with the first-party parser - the same one that parsed the
//! user's statement - so storing the text and re-parsing it keeps one grammar
//! and one derivation. Parsing the whole schema costs microseconds once per
//! open, which the plan cache then amortises away entirely.

use rustdb_base::{error, DbError, DbResult};
use rustdb_pool::{Database, PageId, Pool};
use rustdb_sql::catalog_view::TableInfo;
use rustdb_tree::datum::{Datum, OwnedDatum};
use rustdb_tree::paged::PagedTree;
use rustdb_tree::types::{ColumnSpec, PhysicalType};

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
    ]
}

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
        owned.push(vec![
            OwnedDatum::Int(nth.saturating_add(1) as i64),
            OwnedDatum::Text(entry.kind.as_text().to_vec()),
            OwnedDatum::Text(entry.name.clone()),
            OwnedDatum::Text(entry.table.clone()),
            OwnedDatum::Int(entry.root.0 as i64),
            OwnedDatum::Text(entry.sql.clone()),
        ]);
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
    let mut entries = Vec::new();
    let mut failure: Option<DbError> = None;
    tree.visit_leaves(pool, &mut |leaf: &rustdb_tree::leaf::LeafRef<'_>| {
        for row in 0..leaf.row_count() {
            match entry_of(leaf, row) {
                Ok(entry) => entries.push(entry),
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

/// Reads one catalog row out of a leaf.
///
/// @param leaf - the leaf the row is in
/// @param row - which row
fn entry_of(leaf: &rustdb_tree::leaf::LeafRef<'_>, row: usize) -> DbResult<SchemaEntry> {
    let kind = ObjectKind::from_text(&text_at(leaf, row, 1)?).ok_or_else(|| {
        error::corrupt("a catalog row's type is not one of table, index, view or trigger")
    })?;
    let root = match leaf.value(row, 4)? {
        Datum::Int(page) if page >= 0 => PageId(page as u64),
        Datum::Null => PageId::NONE,
        _ => {
            return Err(error::corrupt(
                "a catalog row's rootpage is not a page number",
            ))
        }
    };
    Ok(SchemaEntry {
        kind,
        name: text_at(leaf, row, 2)?,
        table: text_at(leaf, row, 3)?,
        root,
        sql: text_at(leaf, row, 5)?,
    })
}

/// Reads one text column of one row, refusing anything that is not text.
///
/// @param leaf - the leaf the row is in
/// @param row - which row
/// @param column - which column
fn text_at(leaf: &rustdb_tree::leaf::LeafRef<'_>, row: usize, column: usize) -> DbResult<Vec<u8>> {
    match leaf.value(row, column)? {
        Datum::Text(bytes) => Ok(bytes.to_vec()),
        Datum::Null => Ok(Vec::new()),
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
    let mut tables: Vec<TableInfo> = Vec::new();
    for entry in entries {
        if entry.kind != ObjectKind::Table {
            continue;
        }
        let root = root_as_u32(entry)?;
        let mut info = table_from_create_sql(&entry.sql, database, root)
            .map_err(|error| error.with_detail(format!("in table {}", name_of(entry))))?;
        // The declared name wins over whatever the CREATE text spelled, because
        // the catalog row is what the tree was written under.
        info.name = entry.name.clone();
        info.folded = entry.name.to_ascii_lowercase();
        tables.push(info);
    }
    for entry in entries {
        if entry.kind != ObjectKind::Index {
            continue;
        }
        let root = root_as_u32(entry)?;
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
    Ok(tables)
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
    use rustdb_pool::Options;
    use rustdb_vfs::os::OsVfs;
    use rustdb_vfs::path::DbPath;

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
            },
            SchemaEntry {
                kind: ObjectKind::Index,
                name: b"people_by_team".to_vec(),
                table: b"people".to_vec(),
                root: PageId(9),
                sql: b"CREATE INDEX people_by_team ON people(team)".to_vec(),
            },
            SchemaEntry {
                kind: ObjectKind::Table,
                name: SCHEMA_TABLE.to_vec(),
                table: SCHEMA_TABLE.to_vec(),
                root: PageId(2),
                sql: schema_create_sql().to_vec(),
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
