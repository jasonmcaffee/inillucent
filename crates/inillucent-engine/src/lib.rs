//! The rearchitected engine as a database.
//!
//! Invariant: **this crate is the engine, and nothing above it is.** It owns
//! the buffer pool, the trees, the log, the catalog, DDL, the pragma set, the
//! statement path and the virtual-table host, and it depends on none of the
//! old engine - not `inillucent-storage`, not `inillucent-transaction`, not
//! `inillucent-vm`. A caller reaches the new engine by depending on this and
//! on nothing else.
//!
//! It was `inillucent_compat::newengine` until task-1834, which is where Phases 1
//! to 4 built and measured it: inside the test-and-bench crate, because until
//! Phase 5 there was nothing above it to be the caller. That was the right
//! place to build it and the wrong place to ship it - `inillucent-migrate` cannot
//! depend on a test crate, and neither can a connection - so Phase 5 lifts it
//! out unchanged. `inillucent_compat::newengine` is a re-export of this crate, so
//! every gate, probe and campaign written against the old path still resolves
//! and still measures the same code.

#![forbid(unsafe_code)]
#![deny(missing_docs)]
#![deny(clippy::indexing_slicing)]
#![cfg_attr(
    test,
    allow(
        clippy::expect_used,
        clippy::indexing_slicing,
        clippy::panic,
        clippy::unwrap_used
    )
)]

//! Driving the rearchitected engine from SQL, for the Phase 1 gate.
//!
//! Invariant: the SQL the new engine runs is byte-for-byte the SQL the old
//! engine and `sqlite-bench` run. It is parsed by the same parser, bound by the
//! same binder against a catalog built from the same fixture, and planned by the
//! same planner - only the physical execution below the plan is new. A harness
//! that rewrote the query on its way in would be measuring a different query,
//! which is the failure mode four instruments on this project already had.
//!
//! ## What this is and is not
//!
//! It is the stand-in for the session layer, which the TDD schedules for
//! Phase 3's transactions. There is no connection, no transaction and no
//! prepared-statement API here: a query is parsed, planned, prepared and run
//! against a database file through a buffer pool. That is enough to answer the
//! phase gates and it is deliberately not enough to be mistaken for the engine.
//!
//! ## The import, and the file it now writes
//!
//! Both engines get the same logical rows and neither gets the other's file.
//! [`ImportedDatabase::import`] reads the SQLite fixture through
//! `inillucent-sqlite-reader` and bulk-builds one PAX tree per table and per index
//! **into a real `.rdb` file**, then checkpoints it and reopens it through a
//! pool of a stated size. Phase 1 held the trees in a `Vec` and could not state
//! a cache size at all; that was the one fairness question its report left open,
//! and closing it is the point of the Phase 2 harness.
//!
//! The trees are keyed by the *SQLite root page* the fixture's schema recorded,
//! because that is the identifier the binder's catalog and the planner's access
//! paths already speak, so nothing has to guess a mapping by name.
//!
//! ## The pool size is a parameter, on both sides
//!
//! [`ImportedDatabase::import_with`] takes a frame count. The gate harness sets
//! it and prints it, and sets SQLite's `cache_size` to the same number of bytes,
//! so the scorecard's fairness section can state one cache configuration rather
//! than describing an asymmetry.
//!
//! ## The rowid, and why a table tree is one column wider than its record
//!
//! SQLite stores a rowid in the cell key, not in the record, and an
//! `INTEGER PRIMARY KEY` column's record field is therefore NULL. The new
//! format's rowid-clustered tree holds the rowid once, as its key. So an
//! imported table tree is `[rowid] ++ [record slots except the rowid alias]`,
//! and [`SourceLayout`] records which record slot became which tree column so a
//! bound expression can find its vector.

pub mod analyze;
pub mod ddl;
pub mod pragma;
pub mod vtab;

use std::collections::HashMap;
use std::path::PathBuf;

use inillucent_base::error::misuse;
use inillucent_base::limits::Limits;
use inillucent_base::DbResult;
use inillucent_catalog::load::table_from_create_sql;
use inillucent_catalog::paged::{
    attach_catalog, read_catalog, schema_create_sql, schema_layout, write_catalog, ObjectKind,
    SchemaEntry,
};
use inillucent_exec::dml::{self, Changes, Trees, WriteTarget};
use inillucent_exec::physical::{self, ForcePlan, Params, SourceLayout, TreeCatalog};
use inillucent_exec::StaticType;
use inillucent_pool::{Database, Options, PageId, Pool};
use inillucent_sql::bind::{AllowAll, Binder, BoundStatement};
use inillucent_sql::catalog_view::{IndexInfo, StaticCatalog, TableInfo};
use inillucent_sql::parser::parse_next_statement;
use inillucent_sql::plan::{plan_select_with, Levers, PhysicalPlan};
use inillucent_sqlite_reader::SqliteFile;
use inillucent_tree::datum::{Datum, OwnedDatum};
use inillucent_tree::types::{ColumnSpec, PhysicalType};
use inillucent_tree::write::TreeLog;
use inillucent_tree::PagedTree;
use inillucent_value::collation::Collation;
use inillucent_vfs::{DbPath, OsVfs};
use inillucent_wal::{Body, Synchronous, Wal, WalOptions, FIRST_LSN};

/// How many frames the harness gives a pool when nothing says otherwise.
///
/// 4,096 frames at 32 KiB is 128 MiB, which holds every scorecard fixture at
/// every scale with room to spare - so the default is "resident", and a
/// measurement that wants to compare cache-limited behaviour sets it down
/// explicitly and says so.
pub const DEFAULT_FRAMES: usize = 4_096;

/// A fixture imported into the new engine's trees, in a real database file.
pub struct ImportedDatabase {
    catalog: StaticCatalog,
    database: Database,
    trees: HashMap<u32, PagedTree>,
    layouts: HashMap<u32, SourceLayout>,
    /// For each table root, its index roots ordered smallest tree first.
    covering: HashMap<u32, Vec<u32>>,
    page_size: usize,
    frames: usize,
    /// The file the trees were written to, kept so it can be reported and
    /// cleaned up.
    path: PathBuf,
    /// The tables the import could not take, by name.
    skipped: Vec<String>,
    limits: Limits,
    /// The write-ahead log every change is described in before it happens.
    ///
    /// Held beside the file rather than inside an `inillucent-txn` `Engine`,
    /// because the read path takes `&Pool` as a plain borrow and an engine
    /// keeps its file behind a `RefCell` that cannot lend one. What this
    /// harness needs of a transaction manager is the log, the sync policy and
    /// the commit record; the snapshots and the version log are what the Phase
    /// 3 model driver exercises, and it drives the `Engine` directly.
    wal: Wal,
    /// The transaction number the next statement takes.
    next_txn: std::cell::Cell<u64>,
    /// The statements already parsed, bound, planned and prepared, by SQL text.
    ///
    /// **Both arms must reuse what they prepared.** SQLite steps a VDBE program
    /// compiled once; a write path that parsed, bound, planned and prepared on
    /// every execution is not measuring the same thing, and the first run of the
    /// write gate said so plainly - `txn.large` at **0.02x**, forty updates in
    /// 1.7 ms against SQLite's 32 us. The work was the compilation, not the
    /// write.
    ///
    /// Keyed by the statement text, which is what a caller re-issues. Behind an
    /// `Rc` so an entry can be held across the `&mut self` a write needs.
    statements: std::cell::RefCell<HashMap<String, std::rc::Rc<Cached>>>,
    /// The transaction every statement joins, when one has been opened.
    ///
    /// `None` is autocommit: each statement is its own transaction and pays for
    /// its own commit. That is the right default and it is also the *expensive*
    /// one, which is why the difference has to be expressible - the gate's
    /// `transaction` family is exactly the question of what a commit costs, and
    /// a harness that could only run one grouping could not ask it.
    batch: std::cell::Cell<Option<u64>>,
    /// The catalog tree's rows, with what each one needs beside it.
    ///
    /// Held beside the tree rather than read back out of it on every DDL
    /// statement. The tree is the authority - it is what the file describes
    /// itself with, and `import_with` compares the two after the checkpoint -
    /// but a `DROP` has to find a row by name and the tree is keyed by rowid,
    /// so the alternative is a full scan per statement.
    entries: Vec<Recorded>,
    /// The tables the binder resolves names against, `sqlite_schema` excepted.
    ///
    /// **Derived from `entries`, always**, by `rebuild_tables`. Nothing adds a
    /// table here directly: a schema is one thing, and deriving it twice - once
    /// when a statement runs and once when the catalog is read back - is how the
    /// two come to disagree.
    tables: Vec<TableInfo>,
    /// `sqlite_schema`'s own declaration, re-registered on every rebuild.
    schema_info: TableInfo,
    /// The identifier the next tree a DDL statement creates is registered under.
    ///
    /// Roots here are *identifiers*, not page numbers - the physical root is in
    /// the catalog row - and the imported ones are the fixture's SQLite page
    /// numbers, which start at 1 and count pages. So a DDL-created tree takes a
    /// number from the top half of the range, where no imported table can be,
    /// and `sqlite_schema` keeps `u32::MAX`.
    next_root: u32,
    /// How long a writer waits for the writer slot, in milliseconds.
    ///
    /// `PRAGMA busy_timeout` reads and writes it. The value is carried here
    /// rather than in `inillucent-txn` because this harness holds the log
    /// directly and never takes the writer slot - so what it can honestly do
    /// with the setting is remember it and report it, which is what the pragma
    /// is asked for far more often than it is relied on.
    busy_timeout_ms: u64,
    /// Whether `PRAGMA foreign_keys` is on.
    foreign_keys: bool,
    /// The modules this connection knows, which is the built-in set.
    ///
    /// Held rather than looked up per statement because a module is registered
    /// once and asked many times, and because `CREATE VIRTUAL TABLE` has to find
    /// one by name before anything else can happen.
    registry: inillucent_ext::registry::Registry,
    /// The virtual tables that have been connected, by folded name.
    virtual_tables: HashMap<Vec<u8>, vtab::Connected>,
    /// Where the last `CREATE INDEX` spent its time, in nanoseconds.
    ///
    /// Scan, sort, uniqueness check, pack. On the harness's own type, in a
    /// test-only crate, and nothing in the engine consults it - the same shape
    /// as the write path's `execute_timed`, and for the same reason: `schema`
    /// is a gate this project has already been wrong about the cause of once.
    index_stages: std::cell::Cell<(u128, u128, u128, u128)>,
    /// How many times the catalog has changed.
    ///
    /// A plan compiled at one generation is not run at another: `execute_ddl`
    /// bumps this and empties the statement cache in the same breath, which is
    /// the TDD's "every plan cache is invalidated" made into two lines that
    /// cannot get out of step.
    catalog_generation: u64,
}

impl TreeCatalog for ImportedDatabase {
    fn pool(&self) -> &Pool {
        self.database.pool()
    }

    fn tree(&self, root: u32) -> Option<&PagedTree> {
        self.trees.get(&root)
    }

    fn layout(&self, root: u32) -> Option<&SourceLayout> {
        self.layouts.get(&root)
    }

    fn covering_candidates(&self, table_root: u32) -> Vec<u32> {
        self.covering.get(&table_root).cloned().unwrap_or_default()
    }

    fn virtual_rows(
        &self,
        table: &TableInfo,
        path: &inillucent_sql::plan::AccessPath,
        params: &Params,
        needed: &inillucent_sql::bind::ColumnUse,
    ) -> DbResult<Option<Vec<Vec<OwnedDatum>>>> {
        self.rows_of_module(table, path, params, needed)
    }
}

impl ImportedDatabase {
    /// Imports a SQLite fixture into PAX trees in a fresh database file.
    ///
    /// @param path - the SQLite database to read
    /// @param page_size - the page size to build the new trees at
    pub fn import(path: PathBuf, page_size: usize) -> DbResult<ImportedDatabase> {
        ImportedDatabase::import_with(path, page_size, DEFAULT_FRAMES)
    }

    /// Imports a SQLite fixture, with the pool size stated.
    ///
    /// The file is written beside the fixture with a `.rdb` suffix, built,
    /// checkpointed, closed and reopened - so what the measurement reads is a
    /// database that came off a disk, through a pool of the size named here,
    /// rather than a tree that never left memory. That distinction is the whole
    /// of the Phase 1 report's outstanding fairness question.
    ///
    /// @param path - the SQLite database to read
    /// @param page_size - the page size to build the new trees at
    /// @param frames - how many frames the buffer pool holds
    pub fn import_with(
        path: PathBuf,
        page_size: usize,
        frames: usize,
    ) -> DbResult<ImportedDatabase> {
        let target = target_path(&path, page_size, frames);
        ImportedDatabase::import_into(path, target, page_size, frames)
    }

    /// Imports a SQLite file into a target the caller names.
    ///
    /// The same import, with the destination stated rather than derived. A
    /// measurement wants the derived name - the page size and frame count are
    /// in it so a sweep does not overwrite a file it still has open - and a
    /// **migration** wants to choose, because it stages into a uniquely named
    /// file beside the destination and publishes by renaming. A half-written
    /// database must never sit at the path an application opens, and that is a
    /// property of *where* it is written.
    ///
    /// The target is removed first if it exists, so a caller that stages into a
    /// fresh name gets a fresh file and one that reuses a name gets a rebuild
    /// rather than a merge.
    ///
    /// @param path - the SQLite database to read
    /// @param target - the file to build
    /// @param page_size - the page size to build the new trees at
    /// @param frames - how many frames the buffer pool holds
    pub fn import_into(
        path: PathBuf,
        target: PathBuf,
        page_size: usize,
        frames: usize,
    ) -> DbResult<ImportedDatabase> {
        let mut file = SqliteFile::open(path.clone())?;
        // One schema reader, not two: `inillucent-catalog`'s loader parses every
        // CREATE TABLE and CREATE INDEX and attaches each index to its table
        // with its key columns, collations and descending flags. Re-deriving
        // any of that here would be a second implementation that could
        // disagree with the binder about what the schema says - and the binder
        // is the thing whose plans this import has to satisfy.
        let loaded = file.catalog(b"main")?;
        // The `CREATE` text as SQLite stored it, keyed by folded name. The
        // catalog view derives columns, collations and key order from this text
        // but does not keep an index's copy of it, and the new file's catalog
        // has to store the real declaration rather than one reconstructed from
        // the derivation - a reconstruction that round-trips today is a
        // reconstruction that stops round-tripping at the first syntax the
        // renderer forgets.
        let declarations: HashMap<(String, String), String> = file
            .schema()?
            .into_iter()
            .map(|object| {
                (
                    (object.kind.clone(), object.name.to_ascii_lowercase()),
                    object.sql,
                )
            })
            .collect();
        let mut catalog = StaticCatalog::empty();
        // The same `TableInfo`s the catalog is built from, kept so that DDL can
        // rebuild it after a `DROP` removes one.
        let mut tables: Vec<TableInfo> = Vec::new();
        let mut layouts = HashMap::new();
        let mut covering: HashMap<u32, Vec<u32>> = HashMap::new();
        let mut shapes: HashMap<u32, TreeShape> = HashMap::new();
        // Tables the import could not take. They are named rather than
        // silently absent, because "the query returned nothing" and "the table
        // was never imported" are different failures and only one of them is
        // a bug in the engine.
        let mut skipped: Vec<String> = Vec::new();
        // What goes into the file's own catalog tree. Built as the import runs,
        // because a table's root page in *this* file is only known once it has
        // been written - which is the whole difference between this catalog and
        // the one it was imported from.
        let mut entries: Vec<SchemaEntry> = Vec::new();
        // The identifier each row's tree is registered under, in the same order
        // as `entries`. For an imported object that is the fixture's SQLite root
        // page, which is not the page the row's `rootpage` column names.
        let mut identifiers: Vec<u32> = Vec::new();

        let vfs = OsVfs::new();
        let _ = std::fs::remove_file(&target);
        let db_path = DbPath::new(target.to_string_lossy().as_ref());
        let mut database = Database::create(
            &vfs,
            &db_path,
            Options::default()
                .with_page_size(page_size)
                .with_frames(frames.max(64)),
        )?;

        for info in &loaded.tables {
            if info.root == 0 || info.name.starts_with(b"sqlite_") {
                continue;
            }
            // A `WITHOUT ROWID` table is keyed by its primary key rather than
            // by a rowid, so its pages are index pages and its rows are index
            // entries. Phase 2 skipped it and named it in `skipped()`; Phase 3
            // takes it, because thirteen of the read-only SLT corpus's
            // thirty-seven refusals were the `teams` table and every query that
            // joined it.
            //
            // The new format needs no special case at all - it is a tree whose
            // key is more than one column, which every index already is. What
            // it needs is the *reader* to know the record's field order, and
            // SQLite records that in `primary_key_position`.
            let imported = if info.without_rowid {
                import_keyed_table(&mut database, &mut file, info)
            } else {
                import_table(&mut database, &mut file, info)
            };
            let (shape, layout) = match imported {
                Ok(imported) => imported,
                Err(_) => {
                    skipped.push(String::from_utf8_lossy(&info.name).into_owned());
                    continue;
                }
            };
            entries.push(SchemaEntry {
                kind: ObjectKind::Table,
                name: info.name.clone(),
                table: info.name.clone(),
                root: shape.root,
                sql: info.create_sql.clone(),
                stats: stats_of(&shape),
            });
            identifiers.push(info.root);
            shapes.insert(info.root, shape);
            layouts.insert(info.root, layout);
            // The indexes the new engine can hold. A descending one is dropped,
            // and the *catalog* the binder sees is built without it - see
            // `is_ascending` for why that is one decision rather than three
            // patches.
            let mut info = info.clone();
            let dropped: Vec<Vec<u8>> = info
                .indexes
                .iter()
                .filter(|index| !is_ascending(index))
                .map(|index| index.name.clone())
                .collect();
            for name in &dropped {
                skipped.push(format!(
                    "{} (a descending index)",
                    String::from_utf8_lossy(name)
                ));
            }
            info.indexes.retain(is_ascending);
            let info = &info;
            for index in &info.indexes {
                if index.root == 0 {
                    continue;
                }
                // A `WITHOUT ROWID` table's primary-key index *is* the table:
                // SQLite reports it at the table's own root page, because there
                // is only one b-tree. Importing it as an index would overwrite
                // the table's layout with one that carries the key columns and
                // nothing else - which is what happened, and the symptom was
                // `SELECT * FROM teams` refusing with "the tree read for FROM
                // term 0 does not carry record slot 1".
                if info.without_rowid && index.root == info.root {
                    continue;
                }
                let (shape, layout) =
                    import_index(&mut database, &mut file, info, index, index.root)?;
                entries.push(SchemaEntry {
                    kind: ObjectKind::Index,
                    name: index.name.clone(),
                    table: info.name.clone(),
                    root: shape.root,
                    // An automatic index - one a UNIQUE or PRIMARY KEY
                    // constraint produced - has no CREATE text of its own in
                    // SQLite either, and is reconstructed from the table's
                    // declaration when the catalog is read back.
                    sql: declarations
                        .get(&(
                            "index".to_string(),
                            String::from_utf8_lossy(&index.name).to_ascii_lowercase(),
                        ))
                        .map(|sql| sql.as_bytes().to_vec())
                        .unwrap_or_default(),
                    stats: stats_of(&shape),
                });
                identifiers.push(index.root);
                shapes.insert(index.root, shape);
                layouts.insert(index.root, layout);
                covering
                    .entry(info.root)
                    .or_insert_with(Vec::new)
                    .push(index.root);
            }
            tables.push(info.clone());
            catalog = catalog.with_table(info.clone());
        }

        // The catalog tree goes in last, because it names every root page the
        // import allocated.
        //
        // It holds no row for itself, which is not an omission - SQLite's
        // `sqlite_schema` has never listed `sqlite_schema`, because the reader
        // has to be able to find the catalog before it can read anything, so
        // the catalog's own location lives where the reader looks first. Here
        // that is the meta page's `catalog_root`; in SQLite it is page 1. The
        // table is still fully queryable: it is registered in the binder's
        // catalog below from a declaration this crate holds, and the scan that
        // answers a query against it is the ordinary one.
        let catalog_tree = write_catalog(&mut database, &entries)?;
        let catalog_shape = TreeShape {
            root: catalog_tree.root(),
            columns: schema_layout(),
            key_columns: 1,
            first_leaf: catalog_tree.first_leaf(),
            leaf_count: catalog_tree.leaf_count(),
            row_count: catalog_tree.row_count(),
        };

        // Load, checkpoint, close - the TDD's Phase 2 lifecycle, and the only
        // way to know the format round-trips. Everything below this line reads
        // a file it did not write.
        database.checkpoint()?;
        drop(database);
        let database = Database::open(&vfs, &db_path, frames.max(64))?;

        // The schema comes back out of the file rather than out of the import,
        // and the two are compared. That is what makes the catalog tree load
        // bearing instead of decorative: if the file's own account of itself
        // disagreed with what was written, this would say so here rather than
        // in a query's wrong answer much later.
        let stored = read_catalog(
            database.pool(),
            &attach_catalog(database.pool(), database.catalog_root())?,
        )?;
        if stored != entries {
            return Err(inillucent_base::error::corrupt(
                "the catalog read back from the file is not the one written to it",
            ));
        }

        let mut trees = HashMap::new();
        for (root, shape) in &shapes {
            let tree = PagedTree::attach(
                database.pool(),
                u64::from(*root),
                shape.root,
                shape.columns.clone(),
                shape.key_columns,
                shape.first_leaf,
                shape.leaf_count,
                shape.row_count,
            )?;
            trees.insert(*root, tree);
        }

        // `sqlite_schema` joins the catalog as a table like any other, keyed by
        // a root number no imported table can have. `SELECT ... FROM
        // sqlite_schema` is then planned, scanned and projected by the ordinary
        // path - there is no view machinery, because there does not need to be.
        let schema_root = SCHEMA_VIEW_ROOT;
        let schema_info = table_from_create_sql(schema_create_sql(), 0, schema_root)?;
        layouts.insert(
            schema_root,
            SourceLayout {
                tree_key: schema_root,
                // The rowid is the tree's key column and is not one of the five
                // declared columns, so the record slots start at tree column 1.
                slots: (1..=5).map(Some).collect(),
                rowid: Some(0),
                types: vec![
                    StaticType::Int,
                    StaticType::Text,
                    StaticType::Text,
                    StaticType::Text,
                    StaticType::Int,
                    StaticType::Text,
                ],
                width: 6,
                key_columns: vec![0],
            },
        );
        trees.insert(
            schema_root,
            PagedTree::attach(
                database.pool(),
                u64::from(schema_root),
                catalog_shape.root,
                catalog_shape.columns.clone(),
                catalog_shape.key_columns,
                catalog_shape.first_leaf,
                catalog_shape.leaf_count,
                catalog_shape.row_count,
            )?,
        );
        catalog = catalog.with_table(schema_info.clone());

        // The log the write path describes every change in, opened on a file
        // that has just been checkpointed - so it starts empty, at the first
        // stream position, and every record in it is one this process wrote.
        let wal = Wal::open(
            std::sync::Arc::new(OsVfs::new()),
            &db_path,
            database.uuid(),
            FIRST_LSN,
            1,
            WalOptions::default(),
        )?;
        database.pool().set_durable_lsn(wal.write_ahead_point());

        // Smallest tree first, so the physical pass takes the cheapest
        // structure that covers the query. Sorting by bytes rather than by
        // column count is what makes it the right order: an index with more
        // columns but shorter values can still be the smaller scan.
        for roots in covering.values_mut() {
            roots.sort_by_key(|root| {
                trees
                    .get(root)
                    .map(PagedTree::byte_size)
                    .unwrap_or(usize::MAX)
            });
        }

        Ok(ImportedDatabase {
            catalog,
            database,
            trees,
            layouts,
            covering,
            page_size,
            frames,
            path: target,
            skipped,
            limits: Limits::default(),
            wal,
            next_txn: std::cell::Cell::new(1),
            statements: std::cell::RefCell::new(HashMap::new()),
            batch: std::cell::Cell::new(None),
            entries: entries
                .into_iter()
                .zip(identifiers)
                .enumerate()
                .map(|(nth, (entry, root))| Recorded {
                    rowid: nth.saturating_add(1) as i64,
                    root,
                    entry,
                })
                .collect(),
            tables,
            schema_info,
            next_root: FIRST_CREATED_ROOT,
            busy_timeout_ms: 0,
            foreign_keys: false,
            registry: modules(),
            virtual_tables: HashMap::new(),
            index_stages: std::cell::Cell::new((0, 0, 0, 0)),
            catalog_generation: 0,
        })
    }

    /// Opens a database this engine wrote, reading its schema from the file.
    ///
    /// **This is the engine's own open path, and it is a different thing from
    /// [`ImportedDatabase::reopen`].** `reopen` closes and reopens a handle this
    /// process already has, and it carries that handle's column specifications
    /// and tree numbering across - which is correct for what it is for, proving
    /// that a checkpointed file reads back, but it cannot open a file this
    /// process did not write. Its own comment says so: "a genuine open would
    /// number them itself" and "the engine's own open path is Phase 5's consumer
    /// story".
    ///
    /// This is that path. Nothing comes from memory: the catalog tree is
    /// attached from the meta page's root, every row is read out of it, and each
    /// object's shape is derived from the `CREATE` text the row carries, by the
    /// same `table_shape` / `keyed_table_shape` / `index_shape` the import uses.
    /// One derivation, so a file that opens differently from the way it was
    /// built is a bug in one function rather than a disagreement between two.
    ///
    /// The per-tree statistics come from the catalog row rather than from a walk
    /// - which is what `TreeStats` is in the file for, and what makes opening a
    /// large database cost a catalog read instead of a scan of every leaf.
    ///
    /// @param path - the database file to open
    /// @param page_size - the page size the file was built at
    /// @param frames - how many frames the buffer pool holds
    pub fn open(path: PathBuf, page_size: usize, frames: usize) -> DbResult<ImportedDatabase> {
        let vfs = OsVfs::new();
        let db_path = DbPath::new(path.to_string_lossy().as_ref());
        let database = Database::open(&vfs, &db_path, frames.max(64))?;

        let catalog_tree = attach_catalog(database.pool(), database.catalog_root())?;
        let stored = read_catalog(database.pool(), &catalog_tree)?;

        let mut catalog = StaticCatalog::default();
        let mut trees: HashMap<u32, PagedTree> = HashMap::new();
        let mut layouts: HashMap<u32, SourceLayout> = HashMap::new();
        let mut covering: HashMap<u32, Vec<u32>> = HashMap::new();
        let mut entries: Vec<SchemaEntry> = Vec::new();
        let mut identifiers: Vec<u32> = Vec::new();
        let mut skipped: Vec<String> = Vec::new();
        // The identifier a tree is registered under is this process's
        // bookkeeping and is deliberately not in the file - two processes that
        // opened the same database would otherwise have to agree about it. They
        // are handed out in catalog order, starting above nothing and staying
        // below `FIRST_CREATED_ROOT` so a `CREATE TABLE` after this open cannot
        // collide with one.
        let mut next_identifier = 1u32;
        // Every table by folded name, because an index's shape is derived
        // against its table's declaration and the catalog does not order tables
        // before their indexes.
        let mut infos: HashMap<Vec<u8>, (u32, TableInfo)> = HashMap::new();

        for entry in &stored {
            if entry.kind != ObjectKind::Table {
                continue;
            }
            let identifier = next_identifier;
            next_identifier = next_identifier.saturating_add(1);
            let mut info = match table_from_create_sql(&entry.sql, 0, identifier) {
                Ok(info) => info,
                Err(_) => {
                    skipped.push(String::from_utf8_lossy(&entry.name).into_owned());
                    continue;
                }
            };
            info.root = identifier;
            let (columns, key_columns, layout) = if info.without_rowid {
                match keyed_table_shape(&info) {
                    Ok((columns, key_columns, layout)) => (columns, key_columns, layout),
                    Err(_) => {
                        skipped.push(String::from_utf8_lossy(&entry.name).into_owned());
                        continue;
                    }
                }
            } else {
                let (columns, layout) = table_shape(&info);
                (columns, 1, layout)
            };
            let tree = PagedTree::attach(
                database.pool(),
                u64::from(identifier),
                entry.root,
                columns,
                key_columns,
                entry.stats.first_leaf,
                entry.stats.leaf_count,
                entry.stats.row_count,
            )?;
            trees.insert(identifier, tree);
            layouts.insert(identifier, layout);
            infos.insert(info.folded.clone(), (identifier, info.clone()));
            entries.push(entry.clone());
            identifiers.push(identifier);
        }

        for entry in &stored {
            if entry.kind != ObjectKind::Index {
                continue;
            }
            let folded = entry.table.to_ascii_lowercase();
            let Some((table_root, table_info)) = infos.get(&folded).cloned() else {
                skipped.push(String::from_utf8_lossy(&entry.name).into_owned());
                continue;
            };
            let identifier = next_identifier;
            next_identifier = next_identifier.saturating_add(1);
            let index = match inillucent_catalog::load::index_from_create_sql(
                &entry.sql,
                &table_info,
                identifier,
            ) {
                Ok(index) => index,
                Err(_) => {
                    skipped.push(String::from_utf8_lossy(&entry.name).into_owned());
                    continue;
                }
            };
            let (columns, layout) = index_shape(&table_info, &index, identifier);
            let key_columns = columns.len();
            let tree = PagedTree::attach(
                database.pool(),
                u64::from(identifier),
                entry.root,
                columns,
                key_columns,
                entry.stats.first_leaf,
                entry.stats.leaf_count,
                entry.stats.row_count,
            )?;
            trees.insert(identifier, tree);
            layouts.insert(identifier, layout);
            covering.entry(table_root).or_default().push(identifier);
            // The index joins its table's declaration, so the binder offers it
            // to the planner exactly as the import does.
            if let Some((_, info)) = infos.get_mut(&folded) {
                info.indexes.push(index);
            }
            entries.push(entry.clone());
            identifiers.push(identifier);
        }

        for (_, (_, info)) in infos.iter() {
            catalog = catalog.with_table(info.clone());
        }

        // `sqlite_schema` over the catalog tree, exactly as the import builds
        // it: one root number no object can have, and the ordinary scan path.
        let schema_root = SCHEMA_VIEW_ROOT;
        let schema_info = table_from_create_sql(schema_create_sql(), 0, schema_root)?;
        layouts.insert(
            schema_root,
            SourceLayout {
                tree_key: schema_root,
                slots: (1..=5).map(Some).collect(),
                rowid: Some(0),
                types: vec![
                    StaticType::Int,
                    StaticType::Text,
                    StaticType::Text,
                    StaticType::Text,
                    StaticType::Int,
                    StaticType::Text,
                ],
                width: 6,
                key_columns: vec![0],
            },
        );
        trees.insert(schema_root, catalog_tree);
        catalog = catalog.with_table(schema_info.clone());

        let wal = Wal::open(
            std::sync::Arc::new(OsVfs::new()),
            &db_path,
            database.uuid(),
            FIRST_LSN,
            1,
            WalOptions::default(),
        )?;
        database.pool().set_durable_lsn(wal.write_ahead_point());

        for roots in covering.values_mut() {
            roots.sort_by_key(|root| {
                trees
                    .get(root)
                    .map(PagedTree::byte_size)
                    .unwrap_or(usize::MAX)
            });
        }

        let mut opened = ImportedDatabase {
            catalog,
            database,
            trees,
            layouts,
            covering,
            page_size,
            frames,
            path,
            skipped,
            limits: Limits::default(),
            wal,
            next_txn: std::cell::Cell::new(1),
            statements: std::cell::RefCell::new(HashMap::new()),
            batch: std::cell::Cell::new(None),
            entries: entries
                .into_iter()
                .zip(identifiers)
                .enumerate()
                .map(|(nth, (entry, root))| Recorded {
                    rowid: nth.saturating_add(1) as i64,
                    root,
                    entry,
                })
                .collect(),
            tables: Vec::new(),
            schema_info,
            next_root: FIRST_CREATED_ROOT,
            busy_timeout_ms: 0,
            foreign_keys: false,
            registry: modules(),
            virtual_tables: HashMap::new(),
            index_stages: std::cell::Cell::new((0, 0, 0, 0)),
            catalog_generation: 0,
        };
        opened.rebuild_tables()?;
        opened.refresh_catalog();
        Ok(opened)
    }

    /// Returns the tables the import could not take.
    ///
    /// A caller that finds a query refused can tell "the engine does not do
    /// this yet" from "the table is not there" by looking here.
    pub fn skipped(&self) -> &[String] {
        &self.skipped
    }

    /// Returns how many frames the pool holds.
    pub fn frames(&self) -> usize {
        self.frames
    }

    /// Returns how many bytes the pool occupies.
    pub fn pool_bytes(&self) -> usize {
        self.database.pool().byte_size()
    }

    /// Returns the database file the import wrote.
    pub fn file(&self) -> &std::path::Path {
        &self.path
    }

    /// Returns how many pages the file holds.
    pub fn page_count(&self) -> u64 {
        self.database.pool().page_count()
    }

    /// Returns what the pool has done since the last reset.
    pub fn pool_stats(&self) -> inillucent_pool::PoolStats {
        self.database.pool().stats()
    }

    /// Forgets the pool's counters, so a measurement starts from zero.
    pub fn reset_pool_stats(&self) {
        self.database.pool().reset_stats();
    }

    /// Reads every page of every tree, so a measurement starts warm.
    ///
    /// A cold pool measures the file system, and neither engine's scorecard
    /// number is about that. SQLite's arm is warmed by the harness running the
    /// workload before it times it; this is the same courtesy on this side, and
    /// it is stated rather than left to the first round.
    pub fn warm(&self) -> DbResult<()> {
        for tree in self.trees.values() {
            tree.visit_leaves(self.database.pool(), &mut |_| Ok(true))?;
        }
        Ok(())
    }

    /// Returns the bytes one root's tree occupies.
    ///
    /// Reported beside a measurement so a reader can see how much data each
    /// engine's chosen structure actually reads.
    ///
    /// @param root - the root page id the fixture recorded
    pub fn byte_size(&self, root: u32) -> Option<usize> {
        self.trees.get(&root).map(PagedTree::byte_size)
    }

    /// Returns the index roots that could cover a query over one table.
    ///
    /// @param table_root - the table's root page id
    pub fn candidates(&self, table_root: u32) -> Vec<u32> {
        self.covering.get(&table_root).cloned().unwrap_or_default()
    }

    /// Returns the page size the trees were built at.
    pub fn page_size(&self) -> usize {
        self.page_size
    }

    /// Returns how many leaves one root's tree holds.
    ///
    /// @param root - the root page id the fixture recorded
    pub fn leaf_count(&self, root: u32) -> Option<usize> {
        self.trees.get(&root).map(|tree| tree.leaf_count() as usize)
    }

    /// Returns a table's root page id by name.
    ///
    /// The root page is the identifier everything else here is keyed by, and a
    /// measurement that wants "the main table" has only the name.
    ///
    /// @param name - the table's name
    pub fn table_root(&self, name: &str) -> Option<u32> {
        self.layouts
            .iter()
            .filter(|(root, _)| self.covering.contains_key(root))
            .map(|(root, _)| *root)
            .find(|root| self.catalog_name(*root).as_deref() == Some(name))
    }

    /// Returns the table name a root page belongs to.
    ///
    /// @param root - the root page id
    fn catalog_name(&self, root: u32) -> Option<String> {
        self.catalog
            .tables
            .iter()
            .find(|table| table.root == root)
            .map(|table| String::from_utf8_lossy(&table.name).into_owned())
    }

    /// Returns every imported root, for reporting.
    pub fn roots(&self) -> Vec<u32> {
        let mut roots: Vec<u32> = self.trees.keys().copied().collect();
        roots.sort_unstable();
        roots
    }

    /// Parses, binds and plans one statement.
    ///
    /// Separated from [`ImportedDatabase::run`] so the gate harness can plan
    /// once and execute many times, which is what `prepare_each: false` means
    /// in a scorecard plan.
    ///
    /// @param sql - the statement text
    pub fn plan(&self, sql: &str) -> DbResult<PhysicalPlan> {
        let parsed = parse_next_statement(sql.as_bytes(), 0, &self.limits)
            .map_err(|error| misuse(format!("{sql}: {error:?}")))?;
        let authorizer = AllowAll;
        let mut binder =
            Binder::new(&self.catalog, &parsed.ast, &authorizer).with_source(sql.as_bytes());
        let bound = binder
            .bind_statement(&parsed.statement)
            .map_err(|error| misuse(format!("{sql}: {error:?}")))?;
        match bound {
            BoundStatement::Select(select) => Ok(plan_select_with(*select, Levers::default())),
            _ => Err(misuse(format!("{sql} is not a read-only statement"))),
        }
    }

    /// Runs a planned statement and returns its rows and column names.
    ///
    /// @param plan - a plan from [`ImportedDatabase::plan`]
    /// @param params - the values bound to `?1`, `?2`, ...
    pub fn execute(
        &self,
        plan: &PhysicalPlan,
        params: &Params,
    ) -> DbResult<(Vec<Vec<OwnedDatum>>, Vec<String>)> {
        // A compound is several plans and one answer, and a windowed query is a
        // plan with a pass on top of it; neither is a single prepared
        // statement, so both are dispatched by `run_any` rather than inside
        // `prepare` - which returns the structural choice for *one* pipeline.
        let (rows, shape) = physical::run_any(plan, self, params)?;
        Ok((rows, names_of(&shape)))
    }

    /// Chooses a statement's physical plan, once.
    ///
    /// Separated from execution because the choice depends on the statement and
    /// the schema and not on the data, and because making it per execution made
    /// a query answering 64 rows spend more time choosing a tree than reading
    /// one. `prepare once` in a scorecard plan means the same thing on both
    /// sides.
    ///
    /// @param plan - a plan from [`ImportedDatabase::plan`]
    pub fn prepare(&self, plan: &PhysicalPlan) -> DbResult<physical::Prepared> {
        physical::prepare(plan, self, ForcePlan::default())
    }

    /// Chooses a statement's physical plan under forced levers.
    ///
    /// The metamorphic tests' entry point: the same query under each
    /// alternative must produce the same digest.
    ///
    /// @param plan - a plan from [`ImportedDatabase::plan`]
    /// @param forced - the levers to apply
    pub fn prepare_forced(
        &self,
        plan: &PhysicalPlan,
        forced: ForcePlan,
    ) -> DbResult<physical::Prepared> {
        physical::prepare(plan, self, forced)
    }

    /// Runs an already-prepared statement.
    ///
    /// @param plan - a plan from [`ImportedDatabase::plan`]
    /// @param prepared - the choices [`ImportedDatabase::prepare`] made
    /// @param params - the values bound to `?1`, `?2`, ...
    pub fn execute_prepared(
        &self,
        plan: &PhysicalPlan,
        prepared: &physical::Prepared,
        params: &Params,
    ) -> DbResult<(Vec<Vec<OwnedDatum>>, Vec<String>)> {
        let (rows, shape) = physical::run_prepared(plan, self, prepared, params)?;
        Ok((rows, names_of(&shape)))
    }

    /// Builds a pipeline over an already-prepared statement.
    ///
    /// The measurement path: a caller hands in the sink it wants and drives the
    /// pipeline itself, so the timed region is the pipeline rather than a
    /// `Vec<Vec<OwnedDatum>>` neither engine's caller asked for.
    ///
    /// @param plan - a plan from [`ImportedDatabase::plan`]
    /// @param prepared - the choices [`ImportedDatabase::prepare`] made
    /// @param params - the values bound to `?1`, `?2`, ...
    /// @param sink - the end of the pipeline
    pub fn pipeline(
        &self,
        plan: &PhysicalPlan,
        prepared: &physical::Prepared,
        params: &Params,
        sink: Box<dyn inillucent_exec::Sink>,
    ) -> DbResult<(physical::Pipeline<'_>, physical::Shape)> {
        physical::build_prepared(plan, self, prepared, params, sink)
    }

    /// Builds a statement whose operator chain is reused across executions.
    ///
    /// The difference from [`ImportedDatabase::pipeline`] is the difference
    /// between preparing a *plan* and preparing a *statement*. A workload that
    /// binds new parameters and runs again is answered by SQLite from a VDBE
    /// program compiled once; `pipeline` rebuilt the operator chain each time,
    /// which `inillucent-probeprofile` measured at 42% of `point.rowid` and 71%
    /// of `point.miss`. This builds the chain once and rebuilds only the source
    /// whose key the parameters decide.
    ///
    /// @param plan - the planner's output
    /// @param prepared - the structural choices `prepare` made
    /// @param params - the values the first execution binds
    /// @param sink - the end of the pipeline, which the statement keeps
    pub fn statement<'a>(
        &'a self,
        plan: &'a PhysicalPlan,
        prepared: &physical::Prepared,
        params: &Params,
        sink: Box<dyn inillucent_exec::Sink>,
    ) -> DbResult<physical::Statement<'a>> {
        physical::build_statement(plan, self, prepared, params, sink)
    }

    /// Parses, plans and runs one statement.
    ///
    /// @param sql - the statement text
    pub fn run(&self, sql: &str) -> DbResult<(Vec<Vec<OwnedDatum>>, Vec<String>)> {
        let plan = self.plan(sql)?;
        self.execute(&plan, &Params::new())
    }

    /// Parses, plans and runs one statement with parameters bound.
    ///
    /// @param sql - the statement text
    /// @param params - the values bound to `?1`, `?2`, ...
    pub fn run_with(
        &self,
        sql: &str,
        params: &Params,
    ) -> DbResult<(Vec<Vec<OwnedDatum>>, Vec<String>)> {
        let plan = self.plan(sql)?;
        self.execute(&plan, params)
    }

    /// Returns the physical operator list a prepared statement will run.
    ///
    /// Printed beside SQLite's `EXPLAIN QUERY PLAN` so a reader can see which
    /// structure each engine chose. A ratio measured against a different
    /// structure is not a ratio between engines, which is the single largest
    /// thing Phase 1 learned.
    ///
    /// @param prepared - the choices [`ImportedDatabase::prepare`] made
    pub fn describe_physical(&self, prepared: &physical::Prepared) -> Vec<String> {
        prepared.describe()
    }

    /// Returns the `EXPLAIN QUERY PLAN` lines a statement's plan renders as.
    ///
    /// The harness prints these beside SQLite's so a reader can see whether the
    /// two engines chose the same structure. A ratio measured against a
    /// different structure is not a ratio between engines.
    ///
    /// @param sql - the statement text
    pub fn describe(&self, sql: &str) -> DbResult<Vec<String>> {
        Ok(self.plan(sql)?.describe())
    }

    /// Returns the physical operators a statement runs **through the cache**.
    ///
    /// The difference from [`ImportedDatabase::describe`] is the whole point of
    /// it: that one plans afresh every time, so it could not tell a live cache
    /// from an invalidated one. This asks for the compiled statement the next
    /// execution would get, which is the object a DDL statement has to throw
    /// away.
    ///
    /// @param sql - the statement text
    pub fn describe_cached(&self, sql: &str) -> DbResult<Vec<String>> {
        match &*self.compiled(sql)? {
            Cached::Select(_, prepared) => Ok(prepared.describe()),
            Cached::Ddl(_) => Ok(vec!["a directive".to_string()]),
            Cached::Insert(..) => Ok(vec!["an insert".to_string()]),
            Cached::VirtualInsert(_) => Ok(vec!["an insert into a module".to_string()]),
            Cached::Update(..) => Ok(vec!["an update".to_string()]),
            Cached::Delete(..) => Ok(vec!["a delete".to_string()]),
        }
    }

    /// Returns where the last `CREATE INDEX` spent its time.
    ///
    /// Microseconds per stage, rendered for a report.
    pub fn build_stages(&self) -> String {
        let (scan, sort, unique, pack) = self.index_stages.get();
        format!(
            "scan {:.1} ms, sort {:.1} ms, unique {:.1} ms, pack {:.1} ms",
            scan as f64 / 1e6,
            sort as f64 / 1e6,
            unique as f64 / 1e6,
            pack as f64 / 1e6
        )
    }

    /// Returns the log, so a caller can read its counters.
    pub fn wal(&self) -> &Wal {
        &self.wal
    }

    /// Opens a transaction that the statements after it all join.
    ///
    /// The difference between this and autocommit is the whole of what a commit
    /// costs, which is the gate's `transaction` family. Calling it twice without
    /// a commit between keeps the first transaction, because that is what
    /// `BEGIN` inside a transaction does.
    pub fn begin_batch(&mut self) {
        if self.batch.get().is_some() {
            return;
        }
        let txn = self.next_txn.get();
        self.next_txn.set(txn.saturating_add(1));
        self.batch.set(Some(txn));
    }

    /// Commits the open transaction, if there is one.
    ///
    /// A no-op outside a transaction, so a caller can commit at a boundary
    /// without having to know whether it opened one.
    pub fn commit_batch(&mut self) -> DbResult<()> {
        // Every module flushes what it is holding before the log's commit
        // record, because what it flushes is more writes.
        self.sync_modules()?;
        let Some(txn) = self.batch.take() else {
            return Ok(());
        };
        self.wal.commit(txn, txn)?;
        self.database
            .pool()
            .set_durable_lsn(self.wal.write_ahead_point());
        Ok(())
    }

    /// Returns what the write path has done to every tree, added up.
    ///
    /// The counters, not the clock. For a write the counters are the story: a
    /// page compacted is a whole page image in the log, and a tree that
    /// compacts once per statement is doing work no timing will explain on its
    /// own.
    pub fn write_stats(&self) -> inillucent_tree::write::WriteStats {
        let mut total = inillucent_tree::write::WriteStats::default();
        for tree in self.trees.values() {
            let held = tree.write_stats();
            total.inserted = total.inserted.saturating_add(held.inserted);
            total.deleted = total.deleted.saturating_add(held.deleted);
            total.updated_in_place = total.updated_in_place.saturating_add(held.updated_in_place);
            total.compactions = total.compactions.saturating_add(held.compactions);
            total.splits = total.splits.saturating_add(held.splits);
            total.merges = total.merges.saturating_add(held.merges);
        }
        total
    }

    /// Checks every tree's structure: key order, separators and fill.
    ///
    /// The campaign tests run this after every statement. A tree that has
    /// drifted structurally still answers a scan correctly for a long time,
    /// which is precisely why the check has to be a check rather than a query.
    pub fn check_trees(&self) -> DbResult<()> {
        for tree in self.trees.values() {
            tree.check(self.database.pool())?;
        }
        Ok(())
    }

    /// Sets what a commit waits for.
    ///
    /// @param policy - the `synchronous` setting
    pub fn set_synchronous(&self, policy: Synchronous) {
        self.wal.set_synchronous(policy);
    }

    /// Writes every dirty page and advances the log's recovery point.
    ///
    /// The log is synced *first*, so that every page about to be written is one
    /// the log has already described durably. The other order is the durability
    /// mutant the Phase 3 gate exists to kill.
    pub fn checkpoint(&mut self) -> DbResult<()> {
        // **The catalog's statistics are made honest first, and inside the
        // transaction the checkpoint is about to make durable.** A tree's shape
        // changes on every split and every insert, and rewriting a catalog row
        // that often would put a catalog write on the write path. A checkpoint
        // is the moment it is cheap: the file is being flushed anyway, and what
        // the next open reads is the shape as of the last checkpoint - which is
        // exactly what the next open needs, because everything after it is in
        // the log for recovery to replay.
        self.refresh_statistics()?;
        self.wal.sync()?;
        let durable = self.wal.write_ahead_point();
        self.database.pool().set_durable_lsn(durable);
        let sequence = self.wal.sequence();
        self.database.set_log_position(durable, 0, sequence);
        self.database.checkpoint()?;
        self.wal.note_checkpoint(durable, 0)?;
        self.database
            .pool()
            .set_durable_lsn(self.wal.write_ahead_point());
        Ok(())
    }

    /// Closes the file and opens it again, from the catalog alone.
    ///
    /// **The test that makes the persisted statistics load-bearing.** Every tree
    /// handle is rebuilt from the catalog row's leftmost leaf, leaf count and
    /// row count rather than from anything this process remembers, so a file
    /// whose statistics were wrong answers differently after a reopen - which is
    /// the failure the numbers exist to prevent, made visible.
    ///
    /// It is on the harness rather than in the engine because the engine's own
    /// open path is Phase 5's consumer story. What this proves is that the
    /// *format* carries what an open needs, which is the part Phase 4 owes.
    pub fn reopen(&mut self) -> DbResult<()> {
        self.checkpoint()?;
        let path = self.path.clone();
        let frames = self.frames;
        let vfs = OsVfs::new();
        let db_path = DbPath::new(path.to_string_lossy().as_ref());
        // The old handle's file is closed before the new one opens it, because
        // two `Database`s over one path is two page caches over one file.
        let database = {
            let replacement = Database::open(&vfs, &db_path, frames.max(64))?;
            std::mem::replace(&mut self.database, replacement)
        };
        drop(database);

        let stored = read_catalog(
            self.database.pool(),
            &attach_catalog(self.database.pool(), self.database.catalog_root())?,
        )?;
        let mut trees = HashMap::new();
        let mut entries: Vec<Recorded> = Vec::new();
        for (position, entry) in stored.into_iter().enumerate() {
            let rowid = position.saturating_add(1) as i64;
            // The identifier a tree was registered under is this process's own
            // bookkeeping and is not in the file; a genuine open would number
            // them itself. Carrying the old numbering across keeps the plans and
            // the layouts this harness already holds pointing at the same trees.
            let root = self
                .entries
                .iter()
                .find(|held| held.entry.kind == entry.kind && held.entry.name == entry.name)
                .map(|held| held.root)
                .unwrap_or(0);
            if entry.root.is_none() || root == 0 {
                entries.push(Recorded { rowid, root, entry });
                continue;
            }
            let Some(columns) = self.trees.get(&root).map(|tree| tree.columns().to_vec()) else {
                entries.push(Recorded { rowid, root, entry });
                continue;
            };
            let key_columns = self
                .trees
                .get(&root)
                .map(PagedTree::key_columns)
                .unwrap_or(1);
            let tree = PagedTree::attach(
                self.database.pool(),
                u64::from(root),
                entry.root,
                columns,
                key_columns,
                entry.stats.first_leaf,
                entry.stats.leaf_count,
                entry.stats.row_count,
            )?;
            trees.insert(root, tree);
            entries.push(Recorded { rowid, root, entry });
        }
        // The catalog itself, which the meta page points at rather than a row.
        let catalog_tree = attach_catalog(self.database.pool(), self.database.catalog_root())?;
        trees.insert(SCHEMA_VIEW_ROOT, catalog_tree);
        self.trees = trees;
        self.entries = entries;
        self.wal = Wal::open(
            std::sync::Arc::new(OsVfs::new()),
            &db_path,
            self.database.uuid(),
            FIRST_LSN,
            1,
            WalOptions::default(),
        )?;
        self.database
            .pool()
            .set_durable_lsn(self.wal.write_ahead_point());
        self.rebuild_tables()?;
        self.refresh_catalog();
        Ok(())
    }

    /// Binds one statement against the imported schema.
    ///
    /// @param sql - the statement text
    pub fn bind(&self, sql: &str) -> DbResult<BoundStatement> {
        let parsed = parse_next_statement(sql.as_bytes(), 0, &self.limits)
            .map_err(|error| misuse(format!("{sql}: {error:?}")))?;
        let authorizer = AllowAll;
        let mut binder =
            Binder::new(&self.catalog, &parsed.ast, &authorizer).with_source(sql.as_bytes());
        binder
            .bind_statement(&parsed.statement)
            .map_err(|error| misuse(format!("{sql}: {error:?}")))
    }

    /// Parses, plans and runs one statement of any kind.
    ///
    /// A `SELECT` answers with rows; an `INSERT`, `UPDATE` or `DELETE` answers
    /// with a count and whatever `RETURNING` asked for. One entry point rather
    /// than two, because a corpus record does not say which it is and a harness
    /// that had to guess would be guessing from the SQL text.
    ///
    /// @param sql - the statement text
    /// @param params - the values bound to `?1`, `?2`, ...
    pub fn execute_any(&mut self, sql: &str, params: &Params) -> DbResult<Outcome> {
        let cached = self.compiled(sql)?;
        self.execute_compiled(&cached, params)
    }

    /// Compiles one statement and hands back the handle, without running it.
    ///
    /// **So that a caller can take the compile out of a timed region**, which is
    /// where SQLite's already is: `sqlite_bench.c` calls `sqlite3_prepare_v2`
    /// before it reads the clock and then resets and re-binds inside the loop.
    /// A harness that looked its statement up per iteration would be timing a
    /// hash of the SQL text that the other arm does not pay.
    ///
    /// @param sql - the statement text
    pub fn prepare_statement(&self, sql: &str) -> DbResult<Statement> {
        Ok(Statement(self.compiled(sql)?))
    }

    /// Runs a statement [`ImportedDatabase::prepare_statement`] compiled.
    ///
    /// @param statement - the handle
    /// @param params - the values bound to `?1`, `?2`, ...
    pub fn execute_statement(
        &mut self,
        statement: &Statement,
        params: &Params,
    ) -> DbResult<Outcome> {
        let held = std::rc::Rc::clone(&statement.0);
        self.execute_compiled(&held, params)
    }

    /// Runs one statement and reports where its time went.
    ///
    /// **Two numbers, because there are two halves and they are fixed in
    /// different places.** `find` is the query that decides which rows change -
    /// an ordinary planned query, whose cost is the operator chain and the
    /// descent. `apply` is everything after: compiling the assignments, reading
    /// the rows, maintaining the indexes and writing the tree.
    ///
    /// This exists because the write gate misses and a guess about which half is
    /// expensive is a guess this project has been wrong about before. It is on
    /// the harness's own type, in a test-only crate, and nothing in the engine
    /// consults it.
    ///
    /// @param statement - a handle from `prepare_statement`
    /// @param params - the values bound to `?1`, `?2`, ...
    pub fn execute_timed(
        &mut self,
        statement: &Statement,
        params: &Params,
    ) -> DbResult<(u128, u128)> {
        let cached = std::rc::Rc::clone(&statement.0);
        let found = std::time::Instant::now();
        let rows = match &*cached {
            Cached::Ddl(_)
            | Cached::VirtualInsert(_)
            | Cached::Select(..)
            | Cached::Insert(_, None) => Vec::new(),
            Cached::Insert(_, Some((plan, prepared))) => {
                physical::run_any_prepared(plan, self, prepared, params)?.0
            }
            Cached::Update(_, plan, prepared) | Cached::Delete(_, plan, prepared) => {
                self.keys_of(plan, prepared, params)?
            }
        };
        let find = found.elapsed().as_nanos();
        let applied = std::time::Instant::now();
        match &*cached {
            Cached::Ddl(sql) => {
                let sql = sql.clone();
                self.execute_ddl(&sql)?;
            }
            Cached::VirtualInsert(statement) => {
                let statement = statement.clone();
                self.insert_into_module(&statement, params)?;
            }
            Cached::Select(plan, prepared) => {
                physical::run_any_prepared(plan, self, prepared, params)?;
            }
            Cached::Insert(statement, _) => {
                self.write(params, |target, log, params| {
                    dml::insert(statement, target, log, params, &rows)
                })?;
            }
            Cached::Update(statement, ..) => {
                self.write(params, |target, log, params| {
                    dml::update(statement, target, log, params, &rows)
                })?;
            }
            Cached::Delete(statement, ..) => {
                self.write(params, |target, log, params| {
                    dml::delete(statement, target, log, params, &rows)
                })?;
            }
        }
        Ok((find, applied.elapsed().as_nanos()))
    }

    /// Returns the keys a write will change.
    ///
    /// A `WHERE` that is a rowid equality is answered from the plan itself -
    /// see `physical::rowid_seek_key` - and everything else runs the query. The
    /// row may not exist, and that is not this function's problem: the write
    /// path reads each key before it changes anything and skips the ones that
    /// are not there.
    ///
    /// @param plan - the keys query
    /// @param prepared - its structural choice
    /// @param params - the bound parameters
    fn keys_of(
        &self,
        plan: &PhysicalPlan,
        prepared: &physical::Prepared,
        params: &Params,
    ) -> DbResult<Vec<Vec<OwnedDatum>>> {
        if let Some(key) = physical::rowid_seek_key(plan, params)? {
            return Ok(vec![vec![key]]);
        }
        Ok(physical::run_any_prepared(plan, self, prepared, params)?.0)
    }

    /// Runs one already-compiled statement.
    ///
    /// @param cached - the compiled statement
    /// @param params - the bound parameters
    fn execute_compiled(
        &mut self,
        cached: &std::rc::Rc<Cached>,
        params: &Params,
    ) -> DbResult<Outcome> {
        match &**cached {
            Cached::Ddl(sql) => {
                let sql = sql.clone();
                self.execute_ddl(&sql)
            }
            Cached::VirtualInsert(statement) => {
                let statement = statement.clone();
                self.insert_into_module(&statement, params)
            }
            Cached::Select(plan, prepared) => {
                let (rows, shape) = physical::run_any_prepared(plan, self, prepared, params)?;
                Ok(Outcome {
                    rows,
                    names: names_of(&shape),
                    changes: Changes::default(),
                })
            }
            Cached::Insert(statement, source) => {
                let rows = match source {
                    Some((plan, prepared)) => {
                        physical::run_any_prepared(plan, self, prepared, params)?.0
                    }
                    None => Vec::new(),
                };
                self.write(params, |target, log, params| {
                    dml::insert(statement, target, log, params, &rows)
                })
            }
            Cached::Update(statement, plan, prepared) => {
                let keys = self.keys_of(plan, prepared, params)?;
                self.write(params, |target, log, params| {
                    dml::update(statement, target, log, params, &keys)
                })
            }
            Cached::Delete(statement, plan, prepared) => {
                let keys = self.keys_of(plan, prepared, params)?;
                self.write(params, |target, log, params| {
                    dml::delete(statement, target, log, params, &keys)
                })
            }
        }
    }

    /// Returns one statement compiled, from the cache or by compiling it.
    ///
    /// Everything that does not depend on the bound parameters happens here and
    /// happens once: the parse, the bind, the plan and the structural choice.
    /// What is left per execution is the parameters and the work.
    ///
    /// @param sql - the statement text
    fn compiled(&self, sql: &str) -> DbResult<std::rc::Rc<Cached>> {
        if let Some(held) = self.statements.borrow().get(sql) {
            return Ok(std::rc::Rc::clone(held));
        }
        let compiled = std::rc::Rc::new(self.compile(sql)?);
        self.statements
            .borrow_mut()
            .insert(sql.to_string(), std::rc::Rc::clone(&compiled));
        Ok(compiled)
    }

    /// Compiles one statement as far as its parameters allow.
    ///
    /// @param sql - the statement text
    fn compile(&self, sql: &str) -> DbResult<Cached> {
        match self.bind(sql)? {
            BoundStatement::Select(select) => {
                let plan = plan_select_with(*select, Levers::default());
                let prepared = physical::prepare_any(&plan, self)?;
                Ok(Cached::Select(Box::new(plan), Box::new(prepared)))
            }
            BoundStatement::Insert(statement)
                if statement.table.kind == inillucent_sql::catalog_view::TableKind::Virtual =>
            {
                // A write to a virtual table is the *module's* to make. The
                // engine evaluates the row and hands it over; what happens to it
                // is the module's business, which is what makes a module a
                // module rather than a table with a funny name.
                Ok(Cached::VirtualInsert(statement))
            }
            BoundStatement::Insert(statement) => {
                let source = match &statement.source {
                    inillucent_sql::dml::BoundInsertSource::Select(select) => {
                        let plan = plan_select_with((**select).clone(), Levers::default());
                        let prepared = physical::prepare_any(&plan, self)?;
                        Some((Box::new(plan), Box::new(prepared)))
                    }
                    inillucent_sql::dml::BoundInsertSource::Values(_) => None,
                };
                Ok(Cached::Insert(statement, source))
            }
            BoundStatement::Update(statement) => {
                let (plan, prepared) = self.keys_plan(
                    &statement.table,
                    statement.source,
                    statement.filter.as_ref(),
                    statement.limit.as_ref(),
                    statement.offset.as_ref(),
                )?;
                Ok(Cached::Update(
                    statement,
                    Box::new(plan),
                    Box::new(prepared),
                ))
            }
            BoundStatement::Delete(statement) => {
                let (plan, prepared) = self.keys_plan(
                    &statement.table,
                    statement.source,
                    statement.filter.as_ref(),
                    statement.limit.as_ref(),
                    statement.offset.as_ref(),
                )?;
                Ok(Cached::Delete(
                    statement,
                    Box::new(plan),
                    Box::new(prepared),
                ))
            }
            // A directive is *not* cached as a compiled thing: it changes the
            // catalog the next statement will be bound against, and the whole
            // point of `refresh_catalog` is that what was compiled before a DDL
            // statement is not run after it. So the entry holds the text, and
            // the execution re-binds against the schema as it is at that
            // moment.
            BoundStatement::Directive(_) => Ok(Cached::Ddl(sql.to_string())),
            other => Err(misuse(format!(
                "{sql} binds to {}, which the new engine does not run yet",
                describe_statement(&other)
            ))),
        }
    }

    /// Plans and prepares the query that finds the rows a write will change.
    ///
    /// @param table - the table being written
    /// @param source - the statement-wide number of its FROM term
    /// @param filter - the statement's `WHERE`
    /// @param limit - the statement's `LIMIT`
    /// @param offset - the statement's `OFFSET`
    fn keys_plan(
        &self,
        table: &TableInfo,
        source: usize,
        filter: Option<&inillucent_sql::bind::BoundExpr>,
        limit: Option<&inillucent_sql::bind::BoundExpr>,
        offset: Option<&inillucent_sql::bind::BoundExpr>,
    ) -> DbResult<(PhysicalPlan, physical::Prepared)> {
        let layout = self
            .layouts
            .get(&table.root)
            .ok_or_else(|| misuse("no layout imported for the table being written"))?;
        let select = dml::keys_query(table, source, filter, limit, offset, layout)?;
        let plan = plan_select_with(select, Levers::default());
        let prepared = physical::prepare_any(&plan, self)?;
        Ok((plan, prepared))
    }

    /// Runs one write as its own transaction, logged and committed.
    ///
    /// The commit record is appended and awaited *after* the change, which is
    /// what makes the change atomic: recovery replays a transaction only if it
    /// found the commit, so a crash anywhere inside `apply` leaves a log that
    /// describes nothing that happened.
    ///
    /// @param params - the bound parameters
    /// @param apply - what to change
    fn write(
        &mut self,
        params: &Params,
        apply: impl FnOnce(&mut dyn WriteTarget, &mut dyn TreeLog, &Params) -> DbResult<Changes>,
    ) -> DbResult<Outcome> {
        // A statement inside an open batch joins it and does not commit; a
        // statement outside one is its own transaction and does.
        let (txn, autocommit) = match self.batch.get() {
            Some(held) => (held, false),
            None => {
                let txn = self.next_txn.get();
                self.next_txn.set(txn.saturating_add(1));
                (txn, true)
            }
        };
        let changes = {
            let mut log = WalLog {
                wal: &self.wal,
                txn,
            };
            let mut view = WriteView {
                database: &mut self.database,
                trees: &mut self.trees,
                layouts: &self.layouts,
            };
            apply(&mut view, &mut log, params)?
        };
        if autocommit {
            self.wal.commit(txn, txn)?;
            self.database
                .pool()
                .set_durable_lsn(self.wal.write_ahead_point());
        }
        Ok(Outcome {
            rows: changes.returned.clone(),
            names: Vec::new(),
            changes,
        })
    }
}

/// Returns a shape's column names as strings.
///
/// @param shape - what the built plan produces
fn names_of(shape: &physical::Shape) -> Vec<String> {
    shape
        .names
        .iter()
        .map(|name| String::from_utf8_lossy(name).into_owned())
        .collect()
}

/// A statement compiled once and run many times.
///
/// Opaque on purpose: what is inside is the engine's business, and a caller that
/// could see it would be a caller that could be broken by a plan shape changing.
pub struct Statement(std::rc::Rc<Cached>);

/// One statement, compiled as far as it can be before its parameters arrive.
///
/// A `SELECT` is a plan and the structural choice over it. A write is the bound
/// statement plus, for an `UPDATE` or a `DELETE`, the plan that finds the rows
/// it will change - which is an ordinary query and is prepared like one, so
/// `WHERE id = ?1` reaches the same point probe on the second execution as on
/// the first.
enum Cached {
    /// A statement the session carries out itself, held as its own text.
    ///
    /// Re-bound on every execution, because binding a `DROP TABLE` resolves
    /// whether the table is there and the answer changes when it runs.
    Ddl(String),
    /// An insert into a virtual table, which the module applies.
    VirtualInsert(Box<inillucent_sql::dml::BoundInsert>),
    /// A query.
    Select(Box<PhysicalPlan>, Box<physical::Prepared>),
    /// An insert, with the plan for its `SELECT` source when it has one.
    Insert(
        Box<inillucent_sql::dml::BoundInsert>,
        Option<(Box<PhysicalPlan>, Box<physical::Prepared>)>,
    ),
    /// An update, with the plan that finds the rows it changes.
    Update(
        Box<inillucent_sql::dml::BoundUpdate>,
        Box<PhysicalPlan>,
        Box<physical::Prepared>,
    ),
    /// A delete, with the plan that finds the rows it removes.
    Delete(
        Box<inillucent_sql::dml::BoundDelete>,
        Box<PhysicalPlan>,
        Box<physical::Prepared>,
    ),
}

/// What running one statement produced.
#[derive(Clone, Debug, Default)]
pub struct Outcome {
    /// The rows a `SELECT` answered, or the rows `RETURNING` named.
    pub rows: Vec<Vec<OwnedDatum>>,
    /// The result column names, for a `SELECT`.
    pub names: Vec<String>,
    /// What a write changed.
    pub changes: Changes,
}

/// Names the kind of statement a refusal is about.
///
/// @param statement - the bound statement
fn describe_statement(statement: &BoundStatement) -> &'static str {
    match statement {
        BoundStatement::Select(_) => "a query",
        BoundStatement::Insert(_) => "an insert",
        BoundStatement::Update(_) => "an update",
        BoundStatement::Delete(_) => "a delete",
        BoundStatement::Directive(_) => "a directive",
        BoundStatement::Empty => "nothing",
    }
}

/// Names the kind of directive a refusal is about.
///
/// @param directive - the bound directive
fn describe_directive(directive: &inillucent_sql::directive::Directive) -> &'static str {
    use inillucent_sql::directive::Directive;
    match directive {
        Directive::Begin(_) => "BEGIN",
        Directive::Commit => "COMMIT",
        Directive::Rollback { .. } => "ROLLBACK",
        Directive::Savepoint(_) => "SAVEPOINT",
        Directive::Release(_) => "RELEASE",
        Directive::CreateTable { .. } => "CREATE TABLE",
        Directive::CreateVirtualTable { .. } => "CREATE VIRTUAL TABLE",
        Directive::CreateView { .. } => "CREATE VIEW",
        Directive::CreateTrigger { .. } => "CREATE TRIGGER",
        Directive::CreateIndex { .. } => "CREATE INDEX",
        Directive::Drop { .. } => "DROP",
        Directive::Alter { .. } => "ALTER TABLE",
        Directive::Reindex { .. } => "REINDEX",
        Directive::Vacuum { .. } => "VACUUM",
        Directive::Attach { .. } => "ATTACH",
        Directive::Detach { .. } => "DETACH",
        Directive::Analyze { .. } => "ANALYZE",
        Directive::Pragma { .. } => "PRAGMA",
    }
}

/// The disjoint halves of an [`ImportedDatabase`] a write borrows.
///
/// A write needs `&mut Database` and `&mut PagedTree` at the same instant while
/// the log holds a shared borrow of a third field. Naming the three borrows in
/// one struct is what lets the borrow checker see they are disjoint; a method
/// taking `&mut self` could not, because it would borrow the log too.
struct WriteView<'a> {
    database: &'a mut Database,
    trees: &'a mut HashMap<u32, PagedTree>,
    layouts: &'a HashMap<u32, SourceLayout>,
}

impl WriteTarget for WriteView<'_> {
    fn parts(&mut self) -> (&mut Database, &mut dyn Trees) {
        (self.database, self.trees)
    }

    fn layout(&self, root: u32) -> Option<&SourceLayout> {
        self.layouts.get(&root)
    }
}

/// A [`TreeLog`] that writes to the database's own write-ahead log.
///
/// Every record carries the transaction it belongs to, which is what lets
/// recovery tell a committed change from one whose commit never arrived.
struct WalLog<'a> {
    wal: &'a Wal,
    txn: u64,
}

impl TreeLog for WalLog<'_> {
    fn log(&mut self, body: Body<'_>) -> DbResult<u64> {
        self.wal.append(self.txn, body)
    }
}

/// Reports whether every key column of an index is stored ascending.
///
/// **A descending index is dropped by the import, and dropped from the catalog
/// the binder is given, rather than built and then worked around.** The new
/// engine's trees have no descending key column: every one is stored ascending.
/// The planner reasons about direction from the *catalog's* declaration, so for
/// an index the catalog calls `DESC` every conclusion it draws is inverted
/// against the tree that actually exists - and it draws three:
///
/// - the range bounds, which it emits in the index's order, so `WHERE score >=
///   20` became `(-inf, 20]` and counted three rows where SQLite counted four;
/// - whether an ordering is already provided, so `ORDER BY score` came back
///   **descending** with no sort and no error;
/// - which direction to walk, so a reverse scan was chosen where a forward one
///   was needed.
///
/// Each of those is a wrong answer rather than a refusal, and all three are one
/// mismatch. Patching them one at a time would leave the next conclusion the
/// planner learns to draw waiting to be found the same way, so the mismatch is
/// removed instead: the index is not built, not registered, and not in the
/// catalog, so no path over it is ever planned. Queries that would have used it
/// read the table.
///
/// It is **skipped by name**, not silently: `ImportedDatabase::skipped` reports
/// it, for the same reason a table the import cannot take is reported. Storing
/// a descending key column properly is a format change - the tree, the key
/// encoding, the leaf comparisons and every scan - and belongs to whichever
/// phase decides to pay for it.
///
/// @param index - the index to judge
fn is_ascending(index: &IndexInfo) -> bool {
    index.columns.iter().all(|column| !column.descending)
}

/// Puts imported rows into the order the tree they are about to build compares
/// in.
///
/// **The import cannot rely on SQLite's physical order being ours.** It reads a
/// b-tree by walking it, so the rows arrive in the order *that* file kept them,
/// and there are two ways for that to differ from the order the new tree
/// defines. A `DESC` index column is stored descending by SQLite and ascending
/// here. A collated column is stored under SQLite's implementation of the
/// collation, and agreeing with it byte for byte is an assumption rather than a
/// fact.
///
/// A tree whose leaves are not in its own key order answers a **scan** exactly
/// right and a **seek** wrongly, because the descent binary-searches separators
/// it does not actually obey. That is why this was invisible until the write
/// path became the first thing to seek into an index: `members_score`, over
/// `(score DESC, email)`, imported out of order, and every delete against it
/// silently found nothing and left the entry behind.
///
/// The sort key is the tree's *own* encoding under the tree's *own* collations,
/// so there is no second opinion about ordering to drift from the first.
///
/// @param rows - the rows as the file gave them up
/// @param columns - the tree's column directory
/// @param key_columns - how many leading columns form the key
fn in_key_order(
    rows: Vec<Vec<OwnedDatum>>,
    columns: &[ColumnSpec],
    key_columns: usize,
) -> Vec<Vec<OwnedDatum>> {
    let collations: Vec<Collation> = columns
        .iter()
        .take(key_columns)
        .map(|spec| spec.collation)
        .collect();
    // **Sorted by comparing the values, not by encoding a key per row.**
    //
    // The version this replaces built a `Vec<u8>` key for every row, sorted the
    // pairs by memcmp and then rebuilt the vector - two moves of every row and
    // one allocation per row, to reproduce an order the values already have.
    // `compare_rows` is the comparison the tree's own search and its integrity
    // checker use, so sorting by it is what the tree will be read by, and the
    // encoded form is derived from the same order rather than defining it.
    //
    // It is a stable sort because a `sort_unstable` here would reorder rows
    // whose whole key is equal, and a bulk build's input is compared against
    // SQLite's index page for page.
    let mut rows = rows;
    rows.sort_by(|left, right| {
        for column in 0..key_columns {
            let (Some(one), Some(two)) = (left.get(column), right.get(column)) else {
                continue;
            };
            let order = inillucent_tree::types::compare_under(
                &one.borrow(),
                &two.borrow(),
                collations.get(column).copied().unwrap_or(Collation::Binary),
            );
            if order != std::cmp::Ordering::Equal {
                return order;
            }
        }
        std::cmp::Ordering::Equal
    });
    rows
}

/// Returns a built tree's shape as the catalog records it.
///
/// @param shape - what the build produced
fn stats_of(shape: &TreeShape) -> inillucent_catalog::paged::TreeStats {
    inillucent_catalog::paged::TreeStats {
        first_leaf: shape.first_leaf,
        leaf_count: shape.leaf_count,
        row_count: shape.row_count,
    }
}

/// One catalog row, with the identifier of the tree it describes.
///
/// The row is what the file holds; the identifier is what the `trees` and
/// `layouts` maps are keyed by. They are different numbers - see the module
/// documentation on `newengine::ddl` - and carrying them together is what lets a
/// rename change the row without the tree it names moving.
#[derive(Clone, Debug)]
struct Recorded {
    /// The rowid the catalog tree stores it under.
    rowid: i64,
    /// The identifier its tree is registered under, zero when it has no tree.
    root: u32,
    /// The row itself.
    entry: SchemaEntry,
}

/// The shape of one built tree, kept so it can be re-attached after the file is
/// closed and reopened.
///
/// It is what the catalog will hold in Phase 3. Carrying it explicitly rather
/// than rediscovering it on open is deliberate: rediscovering a root's height
/// by reading the root is fine, but rediscovering its *row count* means walking
/// it, and a harness that walked every tree on open would be measuring its own
/// startup.
struct TreeShape {
    root: PageId,
    columns: Vec<ColumnSpec>,
    key_columns: usize,
    first_leaf: PageId,
    leaf_count: u64,
    row_count: u64,
}

/// The root number `sqlite_schema` is registered under.
///
/// A root number is only an identifier here - the physical page comes from the
/// meta record - so the catalog takes one no imported table can be given. SQLite
/// roots start at 1 and count pages, so a number at the top of the range is
/// free by construction.
const SCHEMA_VIEW_ROOT: u32 = u32::MAX;

/// The identifier the first DDL-created tree is registered under.
///
/// Imported trees are keyed by the fixture's SQLite root *page*, which counts
/// pages from one, so a fixture would have to be eight terabytes at the default
/// page size before it reached this. Counting up from here keeps every created
/// tree's identifier distinct from every imported one without a search.
const FIRST_CREATED_ROOT: u32 = 0x8000_0000;

/// Returns the path the imported database is written to.
///
/// The page size and the frame count are in the name so that a sweep over
/// either does not overwrite the previous run's file while it is still open.
///
/// @param fixture - the SQLite fixture being imported
/// @param page_size - the page size the trees are built at
/// @param frames - how many frames the pool holds
fn target_path(fixture: &std::path::Path, page_size: usize, frames: usize) -> PathBuf {
    let stem = fixture
        .file_stem()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "fixture".to_string());
    let directory = fixture
        .parent()
        .map(std::path::Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    directory.join(format!("{stem}-p{page_size}-f{frames}.rdb"))
}

/// Returns the modules a database of this engine has.
///
/// The built-ins - JSON, `generate_series`, the R-Tree and FTS5 - plus
/// `inillucent_search`, which `Registry::with_builtins` cannot register because it
/// lives two layers above `inillucent-ext` and registering it there would drag a
/// vector index into every database that only wanted SQL.
///
/// The old engine adds it at the connection for exactly that reason
/// (`inillucent-session`'s `connect`), and this is the same decision at the same
/// place in the new one: a database is the first thing that both builds a
/// registry and is allowed to know the retrieval engine exists.
fn modules() -> inillucent_ext::registry::Registry {
    let mut registry = inillucent_ext::registry::Registry::with_builtins();
    inillucent_search::register(&mut registry);
    registry
}

/// Returns how a table's stored rows map onto the columns a query sees.
///
/// **One derivation, exposed rather than copied.** The import turns SQLite's
/// storage shape into the engine's - dropping the rowid-alias record field,
/// putting the rowid in the key column, reordering a `WITHOUT ROWID` table's
/// record into declared order - and `inillucent-migrate` has to perform the
/// identical transform to compare a source table against a migrated one. A
/// second implementation of it in the migration tool is the exact shape of bug
/// this workspace keeps paying for: two readers that agree until the first
/// table with a primary key declared after another column.
///
/// @param info - the table's declaration, as the catalog loader parsed it
pub fn source_layout_of(info: &TableInfo) -> DbResult<SourceLayout> {
    if info.without_rowid {
        Ok(keyed_table_shape(info)?.2)
    } else {
        Ok(table_shape(info).1)
    }
}

/// Returns one stored row as the columns a `SELECT *` produces.
///
/// The stored row is what `inillucent-sqlite-reader` hands back: for a rowid
/// table `[rowid] ++ record fields`, with the alias field NULL because SQLite
/// keeps the rowid in the cell key rather than in the record; for a
/// `WITHOUT ROWID` table, the record in SQLite's own field order.
///
/// @param info - the table's declaration
/// @param layout - the layout `source_layout_of` returned for it
/// @param stored - one row as the reader produced it
pub fn logical_row(
    info: &TableInfo,
    layout: &SourceLayout,
    stored: &[OwnedDatum],
) -> Vec<OwnedDatum> {
    // The tree row first, which is the shape the import builds.
    let tree: Vec<OwnedDatum> = if info.without_rowid {
        stored.to_vec()
    } else {
        let alias = info.rowid_alias.map(usize::from);
        let mut out = Vec::with_capacity(layout.width);
        out.push(stored.first().cloned().unwrap_or(OwnedDatum::Null));
        for slot in 0..info.columns.len() {
            if Some(slot) == alias {
                continue;
            }
            out.push(
                stored
                    .get(slot.saturating_add(1))
                    .cloned()
                    .unwrap_or(OwnedDatum::Null),
            );
        }
        out
    };
    // Then the declared order, which is what a query sees. `slots` is the map
    // the physical pass reads a column through, so using it here is using the
    // same answer.
    layout
        .slots
        .iter()
        .enumerate()
        .map(|(declared, slot)| {
            let value = slot
                .and_then(|index| tree.get(index).cloned())
                .unwrap_or(OwnedDatum::Null);
            let physical = info
                .columns
                .get(declared)
                .map(|column| physical_for(column.affinity).0)
                .unwrap_or(PhysicalType::Any);
            stored_as(physical, value)
        })
        .collect()
}

/// Returns what a column of a given layout hands back for a value put into it.
///
/// **One conversion, and it is the dialect's.** `leaf::classify_at` excepts
/// every mismatched value into the heap and returns it unchanged, with a single
/// deliberate exception: an integer in a column whose affinity is REAL is
/// *converted*, because that is what REAL affinity means - SQLite stores 7 in a
/// REAL column as 7.0 - and because excepting it would take such a column off
/// the vectorised path one whole-numbered row at a time.
///
/// So a migration that read `Int(7)` out of a SQLite record and compared it
/// against the `Real(7.0)` the new engine hands back would call a correct copy
/// wrong. This is that one rule, written where the comparison needs it.
///
/// It is guarded by measurement rather than by comment: the migration
/// acceptance runs every task-1781 fixture, and its digests fail the moment
/// this and `classify_at` disagree about any value in any of them.
///
/// @param physical - the column's layout
/// @param value - the value as the source held it
pub fn stored_as(physical: PhysicalType, value: OwnedDatum) -> OwnedDatum {
    match (physical, &value) {
        (PhysicalType::Float64, OwnedDatum::Int(number)) => OwnedDatum::Real(*number as f64),
        _ => value,
    }
}

/// Imports one table into a rowid-clustered PAX tree.
///
/// @param database - the file the tree is built in
/// @param file - the open fixture
/// @param info - the table's catalog entry
fn import_table(
    database: &mut Database,
    file: &mut SqliteFile,
    info: &TableInfo,
) -> DbResult<(TreeShape, SourceLayout)> {
    let record_width = info.columns.len();
    let raw = file.read_table(info.root, record_width)?;
    // `read_table` returns [rowid] ++ record slots. The rowid alias slot holds
    // NULL in every SQLite record, so it is dropped and the rowid takes its
    // place as the tree's key column.
    let alias = info.rowid_alias.map(usize::from);
    let (columns, layout) = table_shape(info);
    let width = layout.width;

    let mut rows: Vec<Vec<OwnedDatum>> = Vec::with_capacity(raw.len());
    for row in raw {
        let mut out: Vec<OwnedDatum> = Vec::with_capacity(width);
        out.push(row.first().cloned().unwrap_or(OwnedDatum::Null));
        for slot in 0..record_width {
            if Some(slot) == alias {
                continue;
            }
            out.push(
                row.get(slot.saturating_add(1))
                    .cloned()
                    .unwrap_or(OwnedDatum::Null),
            );
        }
        rows.push(out);
    }

    let borrowed: Vec<Vec<Datum<'_>>> = rows
        .iter()
        .map(|row| row.iter().map(OwnedDatum::borrow).collect())
        .collect();
    let tree = PagedTree::bulk_build(
        database,
        u64::from(info.root),
        columns.clone(),
        1,
        &borrowed,
    )?;
    Ok((
        TreeShape {
            root: tree.root(),
            columns,
            key_columns: 1,
            first_leaf: tree.first_leaf(),
            leaf_count: tree.leaf_count(),
            row_count: tree.row_count(),
        },
        layout,
    ))
}

/// Returns the column directory and the layout a rowid table's tree has.
///
/// **Derived from the declaration alone**, which is what lets `CREATE TABLE`
/// and the fixture import agree by construction rather than by two people
/// writing the same rule twice. The import supplies rows read out of a SQLite
/// file and the DDL path supplies none; neither supplies a shape.
///
/// The physical type of each tree column comes from the declared affinity, and
/// that is a claim rather than a guarantee - a column declared `INTEGER` may
/// hold a string - which is exactly what the leaf's exception class is for.
///
/// @param info - the table's declaration
fn table_shape(info: &TableInfo) -> (Vec<ColumnSpec>, SourceLayout) {
    let record_width = info.columns.len();
    let alias = info.rowid_alias.map(usize::from);
    let mut slots: Vec<Option<usize>> = Vec::with_capacity(record_width);
    let mut next = 1usize;
    for slot in 0..record_width {
        if Some(slot) == alias {
            slots.push(Some(0));
        } else {
            slots.push(Some(next));
            next = next.saturating_add(1);
        }
    }
    let width = next;
    let mut columns = vec![ColumnSpec::key(PhysicalType::Int64)];
    let mut types = vec![StaticType::Int];
    for slot in 0..record_width {
        if Some(slot) == alias {
            continue;
        }
        let (physical, static_type) = match info.columns.get(slot) {
            Some(column) => physical_for(column.affinity),
            None => (PhysicalType::Any, StaticType::Unknown),
        };
        columns.push(
            ColumnSpec::new(physical).with_collation(
                info.columns
                    .get(slot)
                    .map(|column| collation_of(&column.collation))
                    .unwrap_or(Collation::Binary),
            ),
        );
        types.push(static_type);
    }
    (
        columns,
        SourceLayout {
            tree_key: info.root,
            slots,
            rowid: Some(0),
            types,
            width,
            // A rowid-clustered tree is ordered by its rowid, which is column 0.
            key_columns: vec![0],
        },
    )
}

/// Imports one `WITHOUT ROWID` table into a key-ordered PAX tree.
///
/// A `WITHOUT ROWID` table *is* an index b-tree: there is no separate table
/// b-tree and no rowid, and the record holds every column with the primary key's
/// columns first. So the import is the index import with the whole record as the
/// row and the primary key as the key - and the resulting tree needs nothing the
/// engine does not already do, because an index tree has a multi-column key too.
///
/// **The field order is SQLite's, not the declaration's.** For
/// `CREATE TABLE t(a, b, PRIMARY KEY(b))` the record is `(b, a)`, and
/// `primary_key_position` is what says so. Reconstructing that order by guessing
/// - assuming the key is a prefix of the declared columns, say - would read the
/// right bytes into the wrong columns on any table whose primary key is not
/// written first, and every value would still be a plausible value.
///
/// @param database - the file the tree is built in
/// @param file - the open fixture
/// @param info - the table's catalog entry
fn import_keyed_table(
    database: &mut Database,
    file: &mut SqliteFile,
    info: &TableInfo,
) -> DbResult<(TreeShape, SourceLayout)> {
    let width = info.columns.len();
    let (columns, key_columns, layout) = keyed_table_shape(info)?;
    let rows = file.read_index(info.root, width)?;
    let rows = in_key_order(rows, &columns, key_columns);
    let borrowed: Vec<Vec<Datum<'_>>> = rows
        .iter()
        .map(|row| row.iter().map(OwnedDatum::borrow).collect())
        .collect();
    let tree = PagedTree::bulk_build(
        database,
        u64::from(info.root),
        columns.clone(),
        key_columns,
        &borrowed,
    )?;
    Ok((
        TreeShape {
            root: tree.root(),
            columns,
            key_columns,
            first_leaf: tree.first_leaf(),
            leaf_count: tree.leaf_count(),
            row_count: tree.row_count(),
        },
        layout,
    ))
}

/// Returns the column directory, key width and layout of a `WITHOUT ROWID`
/// table's tree.
///
/// **The field order is SQLite's, not the declaration's.** For
/// `CREATE TABLE t(a, b, PRIMARY KEY(b))` the record is `(b, a)`, and
/// `primary_key_position` is what says so.
///
/// @param info - the table's declaration
fn keyed_table_shape(info: &TableInfo) -> DbResult<(Vec<ColumnSpec>, usize, SourceLayout)> {
    let width = info.columns.len();
    // The record's field order: primary-key columns in their key order, then
    // every other column in declaration order.
    let mut order: Vec<usize> = Vec::with_capacity(width);
    let mut keyed: Vec<(u16, usize)> = info
        .columns
        .iter()
        .enumerate()
        .filter_map(|(slot, column)| column.primary_key_position.map(|at| (at, slot)))
        .collect();
    keyed.sort_unstable();
    let key_columns = keyed.len();
    if key_columns == 0 {
        return Err(misuse(
            "a WITHOUT ROWID table with no primary key cannot be keyed",
        ));
    }
    order.extend(keyed.iter().map(|(_, slot)| *slot));
    for slot in 0..width {
        if !order.contains(&slot) {
            order.push(slot);
        }
    }
    let mut columns = Vec::with_capacity(width);
    let mut types = Vec::with_capacity(width);
    // `slots[declared] = tree column`, which is the inverse of `order`.
    let mut slots: Vec<Option<usize>> = vec![None; width];
    for (position, declared) in order.iter().enumerate() {
        let (physical, static_type) = match info.columns.get(*declared) {
            Some(column) => physical_for(column.affinity),
            None => (PhysicalType::Any, StaticType::Unknown),
        };
        let collation = info
            .columns
            .get(*declared)
            .map(|column| collation_of(&column.collation))
            .unwrap_or(Collation::Binary);
        let spec = if position < key_columns {
            ColumnSpec::key(physical)
        } else {
            ColumnSpec::new(physical)
        };
        columns.push(spec.with_collation(collation));
        types.push(static_type);
        if let Some(slot) = slots.get_mut(*declared) {
            *slot = Some(position);
        }
    }
    // A non-binary collation means the tree is *seekable* but not "already
    // sorted" for an ORDER BY that did not name the same collation, which is
    // the same disqualification `index_shape` makes and for the same reason.
    let ordered = columns
        .iter()
        .take(key_columns)
        .all(|spec| spec.collation == Collation::Binary);
    Ok((
        columns,
        key_columns,
        SourceLayout {
            tree_key: info.root,
            slots,
            // There is no rowid: that is what `WITHOUT ROWID` means, and a
            // query that asks for one is refused rather than given the key.
            rowid: None,
            types,
            width,
            key_columns: if ordered {
                (0..key_columns).collect()
            } else {
                Vec::new()
            },
        },
    ))
}

/// Imports one index into a key-ordered PAX tree.
///
/// An index entry is the indexed columns followed by the rowid, which is
/// already the new format's index-tree row shape, so nothing is rearranged.
///
/// @param database - the file the tree is built in
/// @param file - the open fixture
/// @param table - the indexed table's catalog entry
/// @param index - the index's catalog entry
/// @param root - the index's root page
fn import_index(
    database: &mut Database,
    file: &mut SqliteFile,
    table: &TableInfo,
    index: &IndexInfo,
    root: u32,
) -> DbResult<(TreeShape, SourceLayout)> {
    let key_columns = index.columns.len().saturating_add(1);
    let rows = file.read_index(root, key_columns)?;
    let (columns, layout) = index_shape(table, index, root);
    let rows = in_key_order(rows, &columns, key_columns);
    let borrowed: Vec<Vec<Datum<'_>>> = rows
        .iter()
        .map(|row| row.iter().map(OwnedDatum::borrow).collect())
        .collect();
    let tree = PagedTree::bulk_build(
        database,
        u64::from(root),
        columns.clone(),
        key_columns,
        &borrowed,
    )?;
    Ok((
        TreeShape {
            root: tree.root(),
            columns,
            key_columns,
            first_leaf: tree.first_leaf(),
            leaf_count: tree.leaf_count(),
            row_count: tree.row_count(),
        },
        layout,
    ))
}

/// Returns the column directory and the layout an index tree is built with.
///
/// Shared by the fixture import, which fills the tree from SQLite's own index
/// pages, and by `CREATE INDEX`, which fills it from the table tree. The shape
/// is the same question in both cases and is answered in one place.
///
/// @param table - the table the index is on
/// @param index - the index's declaration
/// @param root - the identifier the tree is registered under
fn index_shape(table: &TableInfo, index: &IndexInfo, root: u32) -> (Vec<ColumnSpec>, SourceLayout) {
    let key_columns = index.columns.len().saturating_add(1);
    let mut columns = Vec::with_capacity(key_columns);
    let mut types = Vec::with_capacity(key_columns);
    let mut slots: Vec<Option<usize>> = vec![None; table.columns.len()];
    let mut ordered = true;
    for (position, column) in index.columns.iter().enumerate() {
        // An expression key indexes no table column, so nothing maps onto it
        // and a query that reads the underlying column cannot be answered from
        // this tree. Leaving the slot unmapped is what makes that a refusal in
        // the physical pass rather than a wrong answer here.
        if column.descending {
            // The executor's streaming rules assume a scan produces ascending
            // key order. A descending index column would make an adjacent
            // de-duplication and a skipped sort both wrong, so the tree is
            // built but not offered as an ordered one.
            ordered = false;
        }
        let Some(declared) = column.column.map(usize::from) else {
            columns.push(ColumnSpec::key(PhysicalType::Any));
            types.push(StaticType::Unknown);
            continue;
        };
        let (physical, static_type) = match table.columns.get(declared) {
            Some(info) => physical_for(info.affinity),
            None => (PhysicalType::Any, StaticType::Unknown),
        };
        // An index column's collation is the one the *index* declared, and the
        // column's own only when the index did not name one. That is SQLite's
        // rule and it is the order the entries are physically in, which is what
        // the tree's comparisons have to agree with.
        let collation = if column.collation.is_empty() {
            table
                .columns
                .get(declared)
                .map(|info| collation_of(&info.collation))
                .unwrap_or(Collation::Binary)
        } else {
            collation_of(&column.collation)
        };
        if collation != Collation::Binary {
            // A tree ordered by anything but BINARY is still *seekable* - the
            // comparisons below use the collation - but it is not "already
            // sorted" for an `ORDER BY` that did not name the same collation,
            // and the executor's streaming rules cannot express that
            // distinction. Disqualifying it costs a sort and never an answer.
            ordered = false;
        }
        columns.push(ColumnSpec::key(physical).with_collation(collation));
        types.push(static_type);
        if let Some(slot) = slots.get_mut(declared) {
            *slot = Some(position);
        }
    }
    // The rowid entry at the end, which is also the table's rowid-alias column.
    columns.push(ColumnSpec::key(PhysicalType::Int64));
    types.push(StaticType::Int);
    let rowid_position = key_columns.saturating_sub(1);
    if let Some(alias) = table.rowid_alias.map(usize::from) {
        if let Some(slot) = slots.get_mut(alias) {
            *slot = Some(rowid_position);
        }
    }

    (
        columns,
        SourceLayout {
            tree_key: root,
            slots,
            rowid: Some(rowid_position),
            types,
            width: key_columns,
            // An index tree is ordered by every column of its entry, in order:
            // the indexed columns then the rowid. A descending index column
            // would break that, which is why one disqualifies the tree above.
            key_columns: if ordered {
                (0..key_columns).collect()
            } else {
                Vec::new()
            },
        },
    )
}

/// Returns the collation a folded name refers to.
///
/// The three built-in ones. An application-defined collation is not something
/// the new engine can order a tree by - it would have to call back into the
/// connection that registered it on every comparison - so it is treated as
/// BINARY here and the index that uses it is disqualified from being "already
/// sorted", which is the same conservative answer a descending column gets.
///
/// @param folded - the collation's folded name, empty for none
fn collation_of(folded: &[u8]) -> Collation {
    match folded.to_ascii_uppercase().as_slice() {
        b"NOCASE" => Collation::NoCase,
        b"RTRIM" => Collation::RTrim,
        _ => Collation::Binary,
    }
}

/// Chooses a mini-column layout for a declared affinity.
///
/// The mapping is the obvious one and the honesty is in what it does *not*
/// claim: `Blob` affinity (SQLite's "no affinity") gets the `Any` layout, since
/// a column with no affinity has no type to specialise on, and `Numeric` gets
/// `Any` too because it holds integers and reals interchangeably.
///
/// @param affinity - the declared affinity
fn physical_for(affinity: inillucent_value::affinity::Affinity) -> (PhysicalType, StaticType) {
    use inillucent_value::affinity::Affinity;
    match affinity {
        Affinity::Integer => (PhysicalType::Int64, StaticType::Int),
        Affinity::Real => (PhysicalType::Float64, StaticType::Real),
        Affinity::Text => (PhysicalType::Text, StaticType::Text),
        Affinity::Blob => (PhysicalType::Blob, StaticType::Unknown),
        Affinity::Numeric | Affinity::FlexNum => (PhysicalType::Any, StaticType::Unknown),
    }
}
