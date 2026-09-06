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
pub mod connect;
pub mod ddl;
pub mod pragma;
pub mod vtab;

use std::collections::HashMap;
use std::path::PathBuf;

use inillucent_base::error::misuse;
use inillucent_base::limits::Limits;
pub use inillucent_base::DbResult;
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
use inillucent_txn::redo::{RowRedo, TreeRows};
use inillucent_value::collation::Collation;
use inillucent_vfs::{DbPath, OsVfs};
use inillucent_wal::{Body, Synchronous, Wal, WalOptions, FIRST_LSN};

/// The error every call in this crate reports, and its result alias.
///
/// Re-exported for the reason [`BoundParams`] is: a caller of this crate needs
/// no other, and `inillucent-driver` classifies a failure by reading
/// [`DbError::unsupported`] and [`DbError::code`], both of which it has to be
/// able to name.
pub use inillucent_base::error::{DbError, PrimaryCode};

/// The bound-parameter map a statement is executed with.
///
/// Re-exported so that a caller of this crate needs no other. `inillucent-driver`
/// is the reason: its whole purpose is to be the one edge an application depends
/// on, and an application that had to name `inillucent-exec` to bind a parameter
/// would be depending on the layer the driver exists to hide.
pub use inillucent_exec::physical::Params as BoundParams;

/// A value going into a statement or coming out of one.
///
/// Re-exported for the reason [`BoundParams`] is. It is named `Value` here
/// because `Datum` is already the borrowed form in this crate's own imports.
pub use inillucent_tree::datum::OwnedDatum as Value;

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
    wal: std::rc::Rc<Wal>,
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
    /// What the open transaction changed, newest last, so it can be abandoned.
    ///
    /// Empty outside a transaction, and never filled there: an autocommit
    /// statement cannot be rolled back, so it records nothing.
    undo: Vec<Before>,
    /// Named savepoints, and where each one sits in `undo`.
    marks: Vec<(Vec<u8>, usize)>,
    /// The rowid the last `INSERT` assigned, for `last_insert_rowid`.
    ///
    /// **Deliberately not restored by a rollback.** SQLite documents the value
    /// as the last rowid *attempted*, and `faults.rs` pins that: an insert that
    /// is rolled back still moves it. Restoring it would be a different answer
    /// wearing the same name.
    last_rowid: std::cell::Cell<i64>,
    /// Every row every statement on this database has changed.
    changed_ever: std::cell::Cell<i64>,

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
    /// Whether `PRAGMA defer_foreign_keys` has put every immediate check off
    /// until the commit, for the transaction now open.
    defer_foreign_keys: bool,
    /// Whether a cyclic-key sweep is already running.
    ///
    /// The sweep runs statements, and a statement runs the sweep; without this
    /// the first cascade would recur until the stack ran out. It is a flag
    /// rather than a depth because there is exactly one sweep at a time by
    /// construction: it runs after a statement, at the outermost level.
    settling: std::cell::Cell<bool>,
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
                // The identifier this tree is registered and logged under. The
                // import numbers by the *source* file's root pages, which is
                // arbitrary but stable, and putting it in the catalog is what
                // makes it the number a later open derives rather than invents.
                tree_id: u64::from(info.root),
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
                    tree_id: u64::from(index.root),
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
                catalog_shape.leaf_count,
                catalog_shape.row_count,
            )?,
        );
        catalog = catalog.with_table(schema_info.clone());
        catalog = catalog.with_table(schema_alias_of(&schema_info));

        // The log the write path describes every change in, opened on a file
        // that has just been checkpointed - so it starts empty, at the first
        // stream position, and every record in it is one this process wrote.
        let wal = std::rc::Rc::new(Wal::open(
            std::sync::Arc::new(OsVfs::new()),
            &db_path,
            database.uuid(),
            FIRST_LSN,
            1,
            WalOptions::default(),
        )?);
        database.pool().set_durable_lsn(wal.write_ahead_point());
        let_the_pool_ask_the_log(database.pool(), &wal);

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
            last_rowid: std::cell::Cell::new(0),
            changed_ever: std::cell::Cell::new(0),
            statements: std::cell::RefCell::new(HashMap::new()),
            batch: std::cell::Cell::new(None),
            undo: Vec::new(),
            marks: Vec::new(),
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
            defer_foreign_keys: false,
            settling: std::cell::Cell::new(false),
            registry: modules(),
            virtual_tables: HashMap::new(),
            index_stages: std::cell::Cell::new((0, 0, 0, 0)),
            catalog_generation: 0,
        })
    }

    /// Creates a fresh, empty database.
    ///
    /// **The primitive the re-rooting needs, and the one the engine did not
    /// have.** It could `import` a SQLite file and, since this ticket, `open` a
    /// file it had written; it could not make one. `Database::open` on a path
    /// that does not exist has to create it, so a connection cannot be re-rooted
    /// onto this engine without it.
    ///
    /// What it writes is the smallest legal database: the file, its meta page,
    /// and a catalog tree with no rows in it. Everything else - tables, indexes,
    /// virtual tables - arrives through DDL afterwards, which is the path that
    /// already exists and is already tested.
    ///
    /// It goes through the same close-and-reopen the import does, for the same
    /// reason: what the caller gets back has been read off a disk rather than
    /// kept in the pool that wrote it, so a format that does not round-trip
    /// fails here rather than in a query much later.
    ///
    /// @param path - where to create the database
    /// @param page_size - the page size to build at
    /// @param frames - how many frames the buffer pool holds
    pub fn create(path: PathBuf, page_size: usize, frames: usize) -> DbResult<ImportedDatabase> {
        let vfs = OsVfs::new();
        let _ = std::fs::remove_file(&path);
        let db_path = DbPath::new(path.to_string_lossy().as_ref());
        let mut database = Database::create(
            &vfs,
            &db_path,
            Options::default()
                .with_page_size(page_size)
                .with_frames(frames.max(64)),
        )?;
        // An empty catalog is a catalog tree with no entries, not the absence of
        // one: every later DDL statement inserts into it, and a database whose
        // catalog root pointed nowhere would be one no `CREATE TABLE` could
        // start from.
        let _ = write_catalog(&mut database, &[])?;
        database.checkpoint()?;
        drop(database);
        ImportedDatabase::open(path, page_size, frames)
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

        // **Recovery.** The log is replayed into the file before anything is
        // read out of it, which is what makes this an open rather than a
        // reader of whatever the last checkpoint happened to leave behind.
        //
        // It could not be done until task-1834 put a tree's identifier in the
        // catalog. `TreeRows` is keyed by that identifier, every logical row
        // record carries it, and until then the writer's numbering and a
        // reader's were different - so a replay would have put rows into the
        // wrong tree, which is a wrong answer rather than a refusal.
        //
        // From the file's own checkpoint, not from the start of the log:
        // `RecoveryStart::fresh` scans from `FIRST_LSN` and would replay
        // everything the last checkpoint already applied.
        let meta = database.meta();
        let start = if meta.checkpoint_lsn == 0 {
            inillucent_wal::RecoveryStart::fresh(database.uuid())
        } else {
            inillucent_wal::RecoveryStart {
                uuid: database.uuid(),
                checkpoint_lsn: meta.checkpoint_lsn,
                sequence: meta.wal_sequence,
                cts_watermark: meta.cts_watermark,
            }
        };
        let mut database = database;
        // **Recovery, which this open owes and which the identifier made
        // possible.** `TreeRows` is keyed by tree id, and until task-1834 put
        // the identifier in the catalog there was no id a reader could derive
        // that the writer would have agreed with - so a replay would have handed
        // row records to the wrong tree. It can now be built from the file.
        //
        // The shapes come from the catalog as it stood at the last checkpoint,
        // plus the catalog tree itself, whose own rows are what a `CREATE TABLE`
        // writes. A record naming a tree that is in none of them - a table
        // created *after* the checkpoint, whose rows were then written - makes
        // `TreeRows` refuse, which fails this open with a named error rather
        // than replaying into a tree that is not the one meant. A refusal is
        // still the floor; recovery raises how much is above it.
        let checkpointed = {
            let before = attach_catalog(database.pool(), database.catalog_root())?;
            read_catalog(database.pool(), &before)?
        };
        let (outcome, allocated, freed) = {
            let mut applier =
                inillucent_txn::redo::Applier::new(&mut database, LearningRows::new(&checkpointed));
            let outcome = inillucent_wal::recover(&vfs, &db_path, start, &mut applier)?;
            let (allocated, freed) = applier.allocations();
            (outcome, allocated.to_vec(), freed.to_vec())
        };
        // The free map is rebuilt after the scan rather than inside it: the map
        // and every page write are both behind `&mut Database`, and one record
        // cannot hold two mutable borrows of the same object.
        for page in &allocated {
            database.claim(*page)?;
        }
        for page in &freed {
            database.release(*page, 1)?;
        }
        inillucent_wal::truncate_after(&vfs, &db_path, &outcome)?;

        // The catalog is read again, because recovery may have changed it: a
        // `CREATE TABLE` after the checkpoint is a row in this very tree.
        let catalog_tree = attach_catalog(database.pool(), database.catalog_root())?;
        // **Read with the rowid each row is stored under, not without it.**
        // The two loops below visit the tables and then the indexes, which is
        // not the order the catalog holds them in - a schema that creates a
        // table, an index, another table interleaves the two. The rowid used to
        // be reconstructed from the position in *this* reordered list, so every
        // object after the first index was numbered as some other object. The
        // number is what `seal` and every later `DROP` write by, so the next
        // catalog write landed on the wrong row: rows came back duplicated and
        // rows came back missing.
        let stored_rows =
            inillucent_catalog::paged::read_catalog_rows(database.pool(), &catalog_tree)?;
        let stored: Vec<SchemaEntry> = stored_rows.iter().map(|(_, entry)| entry.clone()).collect();
        let rowid_of_name = |entry: &SchemaEntry| -> i64 {
            stored_rows
                .iter()
                .find(|(_, held)| held.name == entry.name && held.kind == entry.kind)
                .map(|(rowid, _)| *rowid)
                .unwrap_or_default()
        };

        let mut catalog = StaticCatalog::default();
        let mut trees: HashMap<u32, PagedTree> = HashMap::new();
        let mut layouts: HashMap<u32, SourceLayout> = HashMap::new();
        let mut covering: HashMap<u32, Vec<u32>> = HashMap::new();
        let mut entries: Vec<(i64, SchemaEntry)> = Vec::new();
        let mut identifiers: Vec<u32> = Vec::new();
        let mut skipped: Vec<String> = Vec::new();
        // **The identifier comes out of the catalog row, not out of a counter.**
        // It used to be handed out here in catalog order, on the reasoning that
        // it was this process's own bookkeeping. It is not: every logical row
        // record in the log carries it, so a reader that numbered trees
        // differently from the writer would hand recovery's records to the wrong
        // tree - a wrong answer rather than a refusal. task-1834 put it in the
        // catalog; this reads it back.
        //
        // `next_root` is set past the largest so a `CREATE TABLE` after this
        // open cannot collide with one already in the file, which a counter that
        // restarted at every open could and did.
        let mut highest_identifier = 0u32;
        // Every table by folded name, because an index's shape is derived
        // against its table's declaration and the catalog does not order tables
        // before their indexes.
        let mut infos: HashMap<Vec<u8>, (u32, TableInfo)> = HashMap::new();

        for entry in &stored {
            if entry.kind != ObjectKind::Table {
                continue;
            }
            // A virtual table has **no tree of its own**. Its rows live in the
            // shadow tables the module declared, which are ordinary tables in
            // this same catalog and are loaded by this same loop. So its row
            // carries no tree identifier, and asking for one refused to open
            // every database holding a search table - which is how this was
            // found, by moving `inillucent-migrate` onto the engine.
            //
            // The kind is learned from a throwaway parse rather than from the
            // parse below, because that one is given the identifier and every
            // shape it derives is derived against it. Parsing once with a
            // placeholder root and patching `info.root` afterwards looked like
            // the same thing and was not: it left the *derived* shapes pointing
            // at the placeholder, and every table then scanned the same tree -
            // `count(*)` answered the same number for every table in the file.
            if matches!(
                table_from_create_sql(&entry.sql, 0, 0).map(|info| info.kind),
                Ok(inillucent_sql::catalog_view::TableKind::Virtual)
            ) {
                // `entries` and `identifiers` are zipped into `Recorded` below,
                // so they are parallel and a row pushed to one has to be pushed
                // to the other. Pushing only the entry shifted every later
                // object onto the previous one's tree - which read as a table
                // whose covering index answered another table's rows, and cost
                // an afternoon to find. Zero is what `Recorded.root` documents
                // for an object with no tree.
                entries.push((rowid_of_name(entry), entry.clone()));
                identifiers.push(0);
                continue;
            }
            let identifier = identifier_of(entry)?;
            highest_identifier = highest_identifier.max(identifier);
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
                entry.stats.leaf_count,
                entry.stats.row_count,
            )?;
            trees.insert(identifier, tree);
            layouts.insert(identifier, layout);
            infos.insert(info.folded.clone(), (identifier, info.clone()));
            entries.push((rowid_of_name(entry), entry.clone()));
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
            let identifier = identifier_of(entry)?;
            highest_identifier = highest_identifier.max(identifier);
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
            entries.push((rowid_of_name(entry), entry.clone()));
            identifiers.push(identifier);
        }

        // **The triggers, then the keys, and in that order.** A written
        // trigger is a catalog row like a table or an index and joins its
        // table's declaration; a foreign key is a trigger the binder writes,
        // and `plan_schema` can only write it once every table is in hand,
        // because a key records only the child's side and the parent's has to
        // be found by asking every table what it points at.
        //
        // Neither was done here until now, which is the whole reason foreign
        // keys were unenforced: the binder fills a statement's `triggers` from
        // exactly these two places, and both were empty on this engine.
        for entry in &stored {
            if entry.kind != ObjectKind::Trigger {
                continue;
            }
            let folded = entry.table.to_ascii_lowercase();
            let Some((_, info)) = infos.get_mut(&folded) else {
                skipped.push(String::from_utf8_lossy(&entry.name).into_owned());
                continue;
            };
            match inillucent_catalog::load::trigger_from_create_sql(&entry.sql) {
                // Newest first, which is SQLite's own order: it pushes each
                // trigger onto the front of the table's list as it reads the
                // schema, so the most recently created one fires first.
                Ok(trigger) => info.triggers.insert(0, trigger),
                Err(_) => skipped.push(String::from_utf8_lossy(&entry.name).into_owned()),
            }
            entries.push((rowid_of_name(entry), entry.clone()));
            identifiers.push(0);
        }

        let mut planned: Vec<TableInfo> = infos.values().map(|(_, info)| info.clone()).collect();
        inillucent_sql::foreign_key::plan_schema(&mut planned, b"main", &Limits::default());
        for info in planned {
            if let Some((_, held)) = infos.get_mut(&info.folded) {
                held.foreign_key_triggers = info.foreign_key_triggers.clone();
            }
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
        catalog = catalog.with_table(schema_alias_of(&schema_info));

        // **The log resumes where recovery ended, not at the beginning.**
        // Opening it at `FIRST_LSN` with sequence 1 starts a second stream over
        // the same segments: the session writes records the *next* open cannot
        // find, because the meta page's checkpoint points into the first stream.
        // A test caught it as a table created after an open vanishing on the
        // one after that - `no such table: second` from a file that had just
        // been told to make it.
        let wal = std::rc::Rc::new(Wal::open(
            std::sync::Arc::new(OsVfs::new()),
            &db_path,
            database.uuid(),
            outcome.next_lsn.max(FIRST_LSN),
            outcome.sequence.max(1),
            WalOptions::default(),
        )?);
        database.pool().set_durable_lsn(wal.write_ahead_point());
        let_the_pool_ask_the_log(database.pool(), &wal);

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
            last_rowid: std::cell::Cell::new(0),
            changed_ever: std::cell::Cell::new(0),
            statements: std::cell::RefCell::new(HashMap::new()),
            batch: std::cell::Cell::new(None),
            undo: Vec::new(),
            marks: Vec::new(),
            entries: entries
                .into_iter()
                .zip(identifiers)
                .map(|((rowid, entry), root)| Recorded { rowid, root, entry })
                .collect(),
            tables: Vec::new(),
            schema_info,
            next_root: highest_identifier.saturating_add(1).max(FIRST_CREATED_ROOT),
            busy_timeout_ms: 0,
            foreign_keys: false,
            defer_foreign_keys: false,
            settling: std::cell::Cell::new(false),
            registry: modules(),
            virtual_tables: HashMap::new(),
            index_stages: std::cell::Cell::new((0, 0, 0, 0)),
            catalog_generation: 0,
        };
        opened.rebuild_tables()?;
        opened.refresh_catalog();
        // The modules are connected after the tables are loaded, because a
        // module's shadow tables have to exist before it can be connected to
        // them. Nothing did this before, so a reopened database holding a
        // search table answered "no such table" for it.
        opened.reconnect_modules()?;
        opened.rebuild_tables()?;
        opened.refresh_catalog();
        Ok(opened)
    }

    /// Returns the catalog a statement is bound against.
    ///
    /// Exposed so an instrument can time binding on its own. `plan` is parse,
    /// bind and logical planning together, and knowing that the three of them
    /// are 64% of compiling `SELECT 1` does not say which of the three to
    /// change.
    pub fn catalog_view(&self) -> &StaticCatalog {
        &self.catalog
    }

    /// Returns every object's name and the identifier its tree is known by.
    ///
    /// The identifier is what the log refers to a tree by, so it is the thing
    /// two processes have to agree about. This exposes it so that agreement can
    /// be *tested* rather than assumed - a writer and a reader that disagreed
    /// would corrupt a recovery quietly, and the only cheap way to catch a
    /// caller reintroducing a process-local number is to compare the two.
    pub fn tree_identifiers(&self) -> Vec<(String, u64)> {
        self.entries
            .iter()
            .map(|held| {
                (
                    String::from_utf8_lossy(&held.entry.name).into_owned(),
                    held.entry.tree_id,
                )
            })
            .collect()
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
    ///
    /// **A refusal says what SQLite's says.** These used to wrap the parse or
    /// bind failure with `{error:?}`, so `SELECT * FROM nope` reported
    /// `SELECT * FROM nope;: ParseError { kind: Refused("no such table: nope"),
    /// span: Span { start: 0, end: 0 } }` where SQLite reports `no such table:
    /// nope`. The one-line message was there all along - `ParseError::message`
    /// - and printing the struct around it made every refusal look like a bug
    /// report about the engine rather than a sentence about the statement.
    pub fn plan(&self, sql: &str) -> DbResult<PhysicalPlan> {
        let parsed = parse_next_statement(sql.as_bytes(), 0, &self.limits).map_err(refused)?;
        let authorizer = AllowAll;
        let mut binder = Binder::new(&self.catalog, &parsed.ast, &authorizer)
            .with_source(sql.as_bytes())
            .with_foreign_keys(self.foreign_keys, self.defer_foreign_keys);
        let bound = binder.bind_statement(&parsed.statement).map_err(refused)?;
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
            Cached::QueryPlan(_) => Ok(vec!["a query plan".to_string()]),
            Cached::Insert(..) => Ok(vec!["an insert".to_string()]),
            Cached::VirtualInsert(_) => Ok(vec!["an insert into a module".to_string()]),
            Cached::VirtualDelete(..) => Ok(vec!["a delete from a module".to_string()]),
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

    /// Returns how many bytes of a script the first statement uses.
    ///
    /// The parser's own count, including the terminating semicolon and the
    /// trivia after it, so a caller stepping a script lands on the next
    /// statement rather than on the space before it.
    ///
    /// @param sql - the script, positioned at the statement to measure
    pub fn statement_length(&self, sql: &str) -> DbResult<usize> {
        let parsed = parse_next_statement(sql.as_bytes(), 0, &self.limits).map_err(refused)?;
        Ok(parsed.consumed)
    }

    /// Returns the schema's generation, which changes when the schema does.
    pub fn schema_generation(&self) -> u64 {
        self.catalog_generation
    }

    /// Rereads the schema from the file, discarding compiled statements.
    ///
    /// The catalog is a snapshot and a plan is compiled against one, so a
    /// reload is a new generation and an empty statement cache - not an edit of
    /// the snapshot the held plans are still reading.
    pub fn reload_catalog(&mut self) -> DbResult<()> {
        self.reload_entries()?;
        Ok(())
    }

    /// Returns whether every statement is its own transaction.
    ///
    /// `false` between a `BEGIN` and its `COMMIT`, which is what
    /// `sqlite3_get_autocommit` answers and what the differential harness
    /// compares after every step.
    pub fn autocommit(&self) -> bool {
        self.batch.get().is_none()
    }

    /// Returns the rowid the last `INSERT` assigned on this database.
    pub fn last_insert_rowid(&self) -> i64 {
        self.last_rowid.get()
    }

    /// Returns how many rows every statement so far has changed.
    pub fn total_changes(&self) -> i64 {
        self.changed_ever.get()
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
        self.undo.clear();
        self.marks.clear();
    }

    /// Undoes everything the open transaction changed, newest first.
    ///
    /// **Newest first, and that is the whole of the ordering rule.** A key
    /// written twice inside one transaction has two records; restoring the
    /// older one last is what puts the row back the way it was before the
    /// transaction rather than the way it was in the middle of it.
    ///
    /// The restores are ordinary writes and are logged like any other, because
    /// the log is redo-only: a crash between the rollback and the commit record
    /// has to replay to the *rolled back* state, not to the state the aborted
    /// statements left. Undoing by not-logging would leave the log describing
    /// changes the file no longer has.
    ///
    /// @param to - the savepoint to stop at, or `None` for the whole transaction
    fn undo_to(&mut self, to: Option<&[u8]>) -> DbResult<()> {
        let floor = match to {
            Some(name) => {
                let folded = name.to_ascii_lowercase();
                let Some(position) = self
                    .marks
                    .iter()
                    .rposition(|(held, _)| *held == folded)
                    .map(|index| self.marks.get(index).map(|(_, at)| *at).unwrap_or(0))
                else {
                    return Err(misuse(format!(
                        "no such savepoint: {}",
                        String::from_utf8_lossy(name)
                    )));
                };
                position
            }
            None => 0,
        };
        let txn = self.current_txn();
        while self.undo.len() > floor {
            let Some(entry) = self.undo.pop() else { break };
            let mut log = WalLog {
                wal: &self.wal,
                txn,
                // The restore is not itself undoable: it *is* the undo, and
                // recording it would grow the buffer being drained.
                undo: None,
            };
            // **The catalog tree answers to two numbers.** Its `tree_id` is
            // `SCHEMA_TREE_ID`, which is what the log records carry, and it
            // lives in `trees` under `SCHEMA_VIEW_ROOT`, which is the
            // identifier the planner reads `sqlite_schema` through. An undo
            // record carries the first and this map is keyed by the second, so
            // a catalog row's before-image was looked up under a number nothing
            // held and silently skipped - which is why a rolled-back
            // `CREATE TABLE` stayed in the schema.
            let root = if entry.tree == inillucent_catalog::paged::SCHEMA_TREE_ID {
                SCHEMA_VIEW_ROOT
            } else {
                u32::try_from(entry.tree).unwrap_or(0)
            };
            let Some(tree) = self.trees.get_mut(&root) else {
                // The tree is gone, which a rollback of a `CREATE TABLE` makes
                // true. Its rows went with it.
                continue;
            };
            match &entry.row {
                Some(row) => {
                    let values: Vec<Datum<'_>> = row.iter().map(OwnedDatum::borrow).collect();
                    tree.put(&mut self.database, &mut log, &values)?;
                }
                None => {
                    let key: Vec<Datum<'_>> = entry.key.iter().map(OwnedDatum::borrow).collect();
                    tree.delete(&mut self.database, &mut log, &key)?;
                }
            }
        }
        self.marks.retain(|(_, at)| *at <= self.undo.len());
        // The catalog tree may have been restored along with everything else,
        // so the schema the binder sees is rebuilt from it.
        let missing = self.reload_entries()?;
        if !missing.is_empty() {
            // A `DROP` undone by restoring its catalog row puts the object back
            // in the schema without putting its tree back in this handle, and a
            // table the binder names and nothing can read is a wrong answer
            // waiting to happen. Refused by name until the re-attach is written.
            return Err(misuse(format!(
                "rolling back left {} in the schema with no tree attached;                  undoing a DROP inside a transaction is not supported yet",
                missing.join(", ")
            )));
        }
        Ok(())
    }

    /// Rebuilds the in-memory schema from the catalog tree.
    ///
    /// **The catalog tree is the authority and `entries` is a cache of it.** A
    /// rollback restores the tree - a catalog row is a row, and it is undone
    /// like one - and this is what makes the cache agree again. Without it a
    /// `CREATE TABLE` that was abandoned stayed visible: the row was gone from
    /// the file and still in the list the binder is built from.
    ///
    /// It does not re-attach trees. Every object it names that has no tree is
    /// reported, because a schema naming a table nothing can read is worse than
    /// a refusal - see [`ImportedDatabase::undo_to`], which turns that into
    /// one.
    fn reload_entries(&mut self) -> DbResult<Vec<String>> {
        let catalog_tree = attach_catalog(self.database.pool(), self.database.catalog_root())?;
        let stored =
            inillucent_catalog::paged::read_catalog_rows(self.database.pool(), &catalog_tree)?;
        let mut missing = Vec::new();
        self.entries = stored
            .into_iter()
            .map(|(rowid, entry)| {
                let root = u32::try_from(entry.tree_id).unwrap_or(0);
                if root != 0 && !self.trees.contains_key(&root) {
                    missing.push(String::from_utf8_lossy(&entry.name).into_owned());
                }
                Recorded { rowid, root, entry }
            })
            .collect();
        self.rebuild_tables()?;
        self.refresh_catalog();
        Ok(missing)
    }

    /// Abandons the open transaction.
    pub fn rollback(&mut self) -> DbResult<()> {
        self.undo_to(None)?;
        self.marks.clear();
        self.batch.set(None);
        // The transaction's own setting goes with the transaction, which is
        // SQLite's rule for `PRAGMA defer_foreign_keys`.
        self.defer_foreign_keys = false;
        self.refresh_catalog();
        Ok(())
    }

    /// Names a point the transaction can be rolled back to.
    ///
    /// @param name - the savepoint's name
    pub fn savepoint(&mut self, name: &[u8]) {
        self.marks
            .push((name.to_ascii_lowercase(), self.undo.len()));
    }

    /// Undoes back to a savepoint, keeping the transaction open.
    ///
    /// @param name - the savepoint's name
    pub fn rollback_to(&mut self, name: &[u8]) -> DbResult<()> {
        self.undo_to(Some(name))?;
        self.refresh_catalog();
        Ok(())
    }

    /// Forgets a savepoint without undoing anything.
    ///
    /// @param name - the savepoint's name
    pub fn release(&mut self, name: &[u8]) -> DbResult<()> {
        let folded = name.to_ascii_lowercase();
        let Some(position) = self.marks.iter().rposition(|(held, _)| *held == folded) else {
            return Err(misuse(format!(
                "no such savepoint: {}",
                String::from_utf8_lossy(name)
            )));
        };
        self.marks.truncate(position);
        Ok(())
    }

    /// Commits the open transaction, if there is one.
    ///
    /// A no-op outside a transaction, so a caller can commit at a boundary
    /// without having to know whether it opened one.
    pub fn commit_batch(&mut self) -> DbResult<()> {
        // **A deferred key is checked here, and a failure means the commit does
        // not happen.** That is SQLite's rule and the whole meaning of
        // `DEFERRABLE INITIALLY DEFERRED`: the rows are allowed to be
        // inconsistent inside the transaction and are required to be consistent
        // at its end. The transaction is left open so the caller can repair it
        // or roll it back, which is what SQLite does too.
        self.check_deferred_foreign_keys()?;
        // Every module flushes what it is holding before the log's commit
        // record, because what it flushes is more writes.
        self.sync_modules()?;
        // `PRAGMA defer_foreign_keys` is the transaction's setting, not the
        // connection's, and SQLite clears it at each commit and rollback.
        if self.defer_foreign_keys {
            self.defer_foreign_keys = false;
            self.forget_compiled_statements();
        }
        // Nothing to abandon once it is committed, and holding the before-images
        // would hold every row a long transaction touched.
        self.undo.clear();
        self.marks.clear();
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
            // The identifier this tree is registered under, carried across so the
            // plans and layouts this handle already holds keep pointing at the
            // same trees.
            //
            // **This comment used to say the identifier was "this process's own
            // bookkeeping and is not in the file", and that stopped being true
            // in task-1834.** It was never quite true: every logical row record
            // in the log carries it, so a reader that numbered trees differently
            // would send recovery's records to the wrong tree. It is in the
            // catalog now, `open` reads it from there, and the lookup below
            // agrees with what `open` would derive rather than merely with what
            // this handle happens to remember.
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
        self.wal = std::rc::Rc::new(Wal::open(
            std::sync::Arc::new(OsVfs::new()),
            &db_path,
            self.database.uuid(),
            FIRST_LSN,
            1,
            WalOptions::default(),
        )?);
        self.database
            .pool()
            .set_durable_lsn(self.wal.write_ahead_point());
        let_the_pool_ask_the_log(self.database.pool(), &self.wal);
        self.rebuild_tables()?;
        self.refresh_catalog();
        Ok(())
    }

    /// Binds one statement against the imported schema.
    ///
    /// @param sql - the statement text
    pub fn bind(&self, sql: &str) -> DbResult<BoundStatement> {
        let parsed = parse_next_statement(sql.as_bytes(), 0, &self.limits).map_err(refused)?;
        let authorizer = AllowAll;
        let mut binder = Binder::new(&self.catalog, &parsed.ast, &authorizer)
            .with_source(sql.as_bytes())
            .with_foreign_keys(self.foreign_keys, self.defer_foreign_keys);
        binder.bind_statement(&parsed.statement).map_err(refused)
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
            | Cached::QueryPlan(_)
            | Cached::VirtualDelete(..)
            | Cached::VirtualInsert(_)
            | Cached::Select(..)
            | Cached::Insert(_, None, _) => Vec::new(),
            Cached::Insert(_, Some((plan, prepared)), _) => {
                physical::run_any_prepared(plan, self, prepared, params)?.0
            }
            Cached::Update(_, plan, prepared, _) | Cached::Delete(_, plan, prepared) => {
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
            // Rendered when it was compiled, so there is nothing to apply and
            // nothing to time. It is here to be exhaustive rather than to be
            // measured: a plan description is not a workload.
            Cached::QueryPlan(_) => {}
            // A module's own write, which this harness does not time: what it
            // costs is the module's business and not the engine's.
            Cached::VirtualDelete(..) => {}
            Cached::VirtualInsert(statement) => {
                let statement = statement.clone();
                self.insert_into_module(&statement, params)?;
            }
            Cached::Select(plan, prepared) => {
                physical::run_any_prepared(plan, self, prepared, params)?;
            }
            Cached::Insert(statement, ..) => {
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

    /// One foreign key's violation query, with what it is about.
    ///
    /// The child and parent names and the key's own id are carried alongside
    /// the SQL because `PRAGMA foreign_key_check` reports all three and the
    /// query itself only produces a rowid.
    fn violation_queries(&self, only: Option<&str>) -> DbResult<Vec<ViolationQuery>> {
        let mut queries = Vec::new();
        for child in &self.tables {
            if child.kind != inillucent_sql::catalog_view::TableKind::Table
                || child.folded.starts_with(b"sqlite_")
            {
                continue;
            }
            if only.is_some_and(|name| child.folded != name.as_bytes()) {
                continue;
            }
            for key in &child.foreign_keys {
                let Some(parent) = self
                    .tables
                    .iter()
                    .find(|candidate| candidate.folded == key.parent_folded)
                else {
                    continue;
                };
                let Some(sql) =
                    inillucent_sql::foreign_key::violation_query(child, parent, key, b"main")
                else {
                    continue;
                };
                queries.push(ViolationQuery {
                    sql,
                    child: child.name.clone(),
                    parent: parent.name.clone(),
                    key: u16::try_from(key.id).unwrap_or_default(),
                });
            }
        }
        Ok(queries)
    }

    /// Runs one query the engine wrote for itself, and returns its rows.
    ///
    /// **The engine asking itself a question.** A foreign-key check *is* a
    /// query, and running it through the ordinary compile-and-execute path is
    /// what makes it use the ordinary indexes - and what stops there being a
    /// second, hand-written scan that has to be kept in step with the first.
    ///
    /// @param sql - the statement the engine generated
    pub(crate) fn query_internally(&mut self, sql: &str) -> DbResult<Vec<Vec<OwnedDatum>>> {
        Ok(self.execute_any(sql, &Params::default())?.rows)
    }

    /// Applies the actions of every key that can lead back to its own table.
    ///
    /// **A cyclic action cannot be inlined**, because the body would have to
    /// appear once per level the data happens to be deep and that is not known
    /// when the statement is compiled. The binder therefore stops a cascade at
    /// the level it can see - the rows that pointed directly at the row that
    /// went - and this takes what that leaves: every row whose key now has no
    /// parent, repeated until nothing changes.
    ///
    /// It terminates because every pass either changes a row or stops, and a
    /// pass only ever removes a row or clears a key.
    ///
    /// It runs after the statement rather than inside it, and only on a schema
    /// that has such a key, so a schema without one pays a flag test.
    pub(crate) fn settle_foreign_keys(&mut self) -> DbResult<()> {
        if !self.foreign_keys || !self.has_cyclic_foreign_keys() {
            return Ok(());
        }
        let mut statements = Vec::new();
        for child in &self.tables {
            if child.kind != inillucent_sql::catalog_view::TableKind::Table {
                continue;
            }
            for key in &child.foreign_keys {
                if !key.cyclic {
                    continue;
                }
                let Some(parent) = self
                    .tables
                    .iter()
                    .find(|candidate| candidate.folded == key.parent_folded)
                else {
                    continue;
                };
                if let Some(sql) =
                    inillucent_sql::foreign_key::sweep_statement(child, parent, key, b"main")
                {
                    statements.push(sql);
                }
            }
        }
        if statements.is_empty() {
            return Ok(());
        }
        for _ in 0..MAX_SWEEP_PASSES {
            // The running total is what says whether a pass did anything: it
            // moves as each statement finishes, so comparing it across a pass
            // asks exactly "did any of these change a row" without the sweep
            // having to count them itself.
            let before = self.changed_ever.get();
            for sql in &statements {
                self.execute_any(sql, &Params::default())?;
            }
            if self.changed_ever.get() == before {
                return Ok(());
            }
        }
        Err(misuse(
            "a foreign key's action did not settle; the schema may have a cycle that cannot resolve",
        ))
    }

    /// Checks every deferred foreign key, and reports the first violation.
    ///
    /// **A full check rather than a running count.** SQLite keeps a counter of
    /// outstanding violations and moves it as rows appear and disappear; a
    /// counter that drifts by one reports a violation that is not there, or
    /// misses one that is, and neither is visible until a commit fails for a
    /// reason nobody can reproduce. Asking the question directly costs a query
    /// per deferred key per commit and cannot drift.
    pub(crate) fn check_deferred_foreign_keys(&mut self) -> DbResult<()> {
        if !self.foreign_keys || !self.has_deferred_foreign_keys() {
            return Ok(());
        }
        for query in self.violation_queries(None)? {
            if self.query_internally(&query.sql)?.is_empty() {
                continue;
            }
            return Err(DbError::new(inillucent_base::ExtendedCode(
                inillucent_sql::dml::codes::FOREIGN_KEY,
            ))
            .with_message("FOREIGN KEY constraint failed")
            .with_detail(format!(
                "deferred key {} of {}",
                query.key,
                String::from_utf8_lossy(&query.child)
            )));
        }
        Ok(())
    }

    /// Reports whether any key's checks are waiting for the commit.
    fn has_deferred_foreign_keys(&self) -> bool {
        self.defer_foreign_keys
            || self
                .tables
                .iter()
                .any(|table| table.foreign_keys.iter().any(|key| key.is_deferred()))
    }

    /// Reports whether any key can lead back to the table that declares it.
    fn has_cyclic_foreign_keys(&self) -> bool {
        self.tables
            .iter()
            .any(|table| table.foreign_keys.iter().any(|key| key.cyclic))
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
        let outcome = self.apply_compiled(cached, params)?;
        // **The cyclic half of a foreign key's action happens here**, after the
        // statement rather than inside it, because a cascade that can reach
        // itself cannot be inlined to a depth the data decides: the body would
        // have to appear once per level the data happens to be deep, and that
        // is not known when the statement is compiled.
        //
        // It sits on this function rather than on `execute_any` because this is
        // the funnel *both* callers reach - a statement run by text and a
        // statement prepared and stepped - and a settle that only one of them
        // performed would leave the tree half-repaired depending on which API
        // the application happened to use.
        if !self.settling.get() {
            self.settling.set(true);
            let settled = self.settle_foreign_keys();
            self.settling.set(false);
            settled?;
        }
        Ok(outcome)
    }

    /// Runs one already-compiled statement, without settling anything after it.
    ///
    /// @param cached - the compiled statement
    /// @param params - the bound parameters
    fn apply_compiled(
        &mut self,
        cached: &std::rc::Rc<Cached>,
        params: &Params,
    ) -> DbResult<Outcome> {
        match &**cached {
            Cached::Ddl(sql) => {
                let sql = sql.clone();
                self.execute_ddl(&sql)
            }
            Cached::QueryPlan(lines) => Ok(query_plan_rows(lines)),
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
            Cached::Insert(statement, source, values_hold_subquery) => {
                let rows = match source {
                    Some((plan, prepared)) => {
                        physical::run_any_prepared(plan, self, prepared, params)?.0
                    }
                    None => Vec::new(),
                };
                // A `VALUES` list has expressions and no plan, so the
                // plan-shaped fold never sees it. Folded here instead, or a
                // subquery in a value would be refused as though it were
                // correlated - which is what an unfilled slot looks like from
                // inside the physical pass. The flag was decided when the
                // statement was compiled: an insert that holds no subquery is
                // the common case and pays nothing for this.
                let folded = if *values_hold_subquery {
                    self.fold_values(statement, params)?
                } else {
                    None
                };
                let params = folded.as_ref().unwrap_or(params);
                self.write(params, |target, log, params| {
                    dml::insert(statement, target, log, params, &rows)
                })
            }
            Cached::Update(statement, plan, prepared, assignments_hold_subquery) => {
                let keys = self.keys_of(plan, prepared, params)?;
                // The same for an `UPDATE`'s assignments: the plan above finds
                // the rows, and the values written into them are evaluated by
                // the write path from expressions the plan never carried.
                let folded = if *assignments_hold_subquery {
                    let assigned: Vec<&inillucent_sql::bind::BoundExpr> = statement
                        .assignments
                        .iter()
                        .map(|assignment| &assignment.value)
                        .collect();
                    inillucent_exec::subquery::fold_expressions(&assigned, self, params)?
                } else {
                    None
                };
                let params = folded.as_ref().unwrap_or(params);
                self.write(params, |target, log, params| {
                    dml::update(statement, target, log, params, &keys)
                })
            }
            Cached::VirtualDelete(statement, plan, prepared) => {
                let keys = physical::run_any_prepared(plan, self, prepared, params)?.0;
                let mut changed = 0usize;
                for key in &keys {
                    let Some(rowid) = key.first() else { continue };
                    self.change_module(
                        &statement.table.name,
                        &inillucent_sql::vtab::Change::Delete(inillucent_exec::scalar::to_value(
                            rowid.borrow(),
                        )),
                    )?;
                    changed = changed.saturating_add(1);
                }
                if self.batch.get().is_none() {
                    self.sync_modules()?;
                    self.seal()?;
                }
                Ok(Outcome {
                    rows: Vec::new(),
                    names: Vec::new(),
                    changes: Changes {
                        rows: changed,
                        returned: Vec::new(),
                        last_rowid: None,
                    },
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

    /// Folds the subqueries in an insert's `VALUES` list, when it has one.
    ///
    /// An insert whose source is a `SELECT` is planned, so its subqueries are
    /// folded by the plan-shaped path along with everything else in that plan.
    /// A `VALUES` list is not planned at all - the write path evaluates its
    /// expressions directly - so it is folded here.
    ///
    /// @param statement - the bound insert
    /// @param params - the values bound for this execution
    fn fold_values(
        &self,
        statement: &inillucent_sql::dml::BoundInsert,
        params: &Params,
    ) -> DbResult<Option<Params>> {
        let inillucent_sql::dml::BoundInsertSource::Values(rows) = &statement.source else {
            return Ok(None);
        };
        let values: Vec<&inillucent_sql::bind::BoundExpr> = rows.iter().flatten().collect();
        inillucent_exec::subquery::fold_expressions(&values, self, params)
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

    /// Compiles an `EXPLAIN`, which the two forms of do different things.
    ///
    /// **`EXPLAIN QUERY PLAN` is answerable and plain `EXPLAIN` is not**, and
    /// the reason is structural rather than unfinished. SQLite's `EXPLAIN`
    /// lists the opcodes of the bytecode program it compiled; this engine
    /// compiles no bytecode - it builds an operator chain - so there is no
    /// opcode listing to print, and printing the operator chain under that name
    /// would be answering a different question with the same word.
    ///
    /// `EXPLAIN QUERY PLAN` asks what the plan *is*, which this engine can
    /// answer: `Prepared::describe` already renders the chain, and the
    /// benchmark harness has been printing it beside SQLite's since Phase 1 so
    /// a reader can see whether the two chose the same structure.
    ///
    /// @param sql - the whole statement text, for a refusal to quote
    /// @param query_plan - whether `QUERY PLAN` was written
    /// @param inner - the statement being explained
    /// @param parsed - the parse the statement came out of
    fn compile_explain(
        &self,
        sql: &str,
        query_plan: bool,
        inner: &inillucent_sql::ast::Statement,
        parsed: &inillucent_sql::parser::ParsedStatement,
    ) -> DbResult<Cached> {
        if !query_plan {
            return Err(misuse(format!(
                "{sql}: plain EXPLAIN lists the opcodes of a bytecode program, and this \
                 engine compiles no bytecode - it builds an operator chain. EXPLAIN \
                 QUERY PLAN describes that chain and is answered"
            )));
        }
        let authorizer = AllowAll;
        let mut binder = Binder::new(&self.catalog, &parsed.ast, &authorizer)
            .with_source(sql.as_bytes())
            .with_foreign_keys(self.foreign_keys, self.defer_foreign_keys);
        let bound = binder.bind_statement(inner).map_err(refused)?;
        let lines = match bound {
            BoundStatement::Select(select) => {
                plan_select_with(*select, Levers::default()).describe()
            }
            // A write's plan is the query that finds the rows it changes, and
            // that is the thing a reader is asking about - "did my DELETE use
            // the index" is the same question as "did the search use it".
            // Answering "a delete" would be answering that it is a delete,
            // which the reader wrote.
            BoundStatement::Update(statement) => self
                .keys_plan(
                    &statement.table,
                    statement.source,
                    statement.filter.as_ref(),
                    statement.limit.as_ref(),
                    statement.offset.as_ref(),
                )?
                .0
                .describe(),
            BoundStatement::Delete(statement) => self
                .keys_plan(
                    &statement.table,
                    statement.source,
                    statement.filter.as_ref(),
                    statement.limit.as_ref(),
                    statement.offset.as_ref(),
                )?
                .0
                .describe(),
            other => vec![describe_statement(&other).to_string()],
        };
        Ok(Cached::QueryPlan(lines))
    }

    /// Compiles one statement as far as its parameters allow.
    ///
    /// @param sql - the statement text
    fn compile(&self, sql: &str) -> DbResult<Cached> {
        // `EXPLAIN` is decided before binding, because the binder's job is the
        // statement being explained and not the explaining. The old engine did
        // this a level up, where a VDBE program was available to render; here
        // there is no program, and that difference is the whole of the
        // `query_plan` split below.
        let parsed = parse_next_statement(sql.as_bytes(), 0, &self.limits).map_err(refused)?;
        if let inillucent_sql::ast::Statement::Explain { query_plan, inner } = &parsed.statement {
            return self.compile_explain(sql, *query_plan, inner, &parsed);
        }
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
                let values_hold_subquery = match &statement.source {
                    inillucent_sql::dml::BoundInsertSource::Values(rows) => rows
                        .iter()
                        .flatten()
                        .any(inillucent_sql::plan::expression_holds_subquery),
                    inillucent_sql::dml::BoundInsertSource::Select(_) => false,
                };
                Ok(Cached::Insert(statement, source, values_hold_subquery))
            }
            BoundStatement::Update(statement) => {
                let (plan, prepared) = self.keys_plan(
                    &statement.table,
                    statement.source,
                    statement.filter.as_ref(),
                    statement.limit.as_ref(),
                    statement.offset.as_ref(),
                )?;
                let assignments_hold_subquery = statement.assignments.iter().any(|assignment| {
                    inillucent_sql::plan::expression_holds_subquery(&assignment.value)
                });
                Ok(Cached::Update(
                    statement,
                    Box::new(plan),
                    Box::new(prepared),
                    assignments_hold_subquery,
                ))
            }
            BoundStatement::Delete(statement)
                if statement.table.kind == inillucent_sql::catalog_view::TableKind::Virtual =>
            {
                let select = inillucent_exec::dml::module_keys_query(
                    &statement.table,
                    statement.source,
                    statement.filter.as_ref(),
                    statement.limit.as_ref(),
                    statement.offset.as_ref(),
                );
                let plan = plan_select_with(select, Levers::default());
                let prepared = physical::prepare_any(&plan, self)?;
                Ok(Cached::VirtualDelete(
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
                // Collected only inside a transaction: outside one there is
                // nothing that could abandon the write.
                undo: (!autocommit).then_some(&mut self.undo),
            };
            let mut view = WriteView {
                database: &mut self.database,
                trees: &mut self.trees,
                layouts: &self.layouts,
                covering: &self.covering,
            };
            apply(&mut view, &mut log, params)?
        };
        if let Some(assigned) = changes.last_rowid {
            self.last_rowid.set(assigned);
        }
        self.changed_ever
            .set(self.changed_ever.get().saturating_add(changes.rows as i64));
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

/// How many times the cyclic sweep repeats before it gives up.
///
/// One pass per level of the deepest chain in the data. A tree deeper than this
/// is a tree with a million levels, which is a different problem.
const MAX_SWEEP_PASSES: usize = 1_000_000;

/// One foreign key's violation query, and what it is about.
struct ViolationQuery {
    /// The `SELECT` that finds the rows with no parent.
    sql: String,
    /// The child table's name, which the pragma reports.
    child: Vec<u8>,
    /// The parent table's name, which the pragma reports.
    parent: Vec<u8>,
    /// The key's position in its table, which the pragma reports as `fkid`.
    key: u16,
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

/// Renders `EXPLAIN QUERY PLAN` lines as the rows a caller reads.
///
/// The four columns are SQLite's - `id`, `parent`, `notused`, `detail` - so a
/// caller written against SQLite reads the same shape and finds its text where
/// it expects it. The ids are the line's position rather than a tree: this
/// engine's `describe` renders the chain source-first as a list, and inventing
/// a parent for each line would be inventing structure the renderer does not
/// carry. SQLite documents its own `EXPLAIN QUERY PLAN` output as unstable
/// between releases, so the text was never the comparable part.
///
/// @param lines - the plan's operators, source first
fn query_plan_rows(lines: &[String]) -> Outcome {
    Outcome {
        rows: lines
            .iter()
            .enumerate()
            .map(|(position, line)| {
                vec![
                    OwnedDatum::Int(position as i64),
                    OwnedDatum::Int(0),
                    OwnedDatum::Int(0),
                    OwnedDatum::Text(line.as_bytes().to_vec()),
                ]
            })
            .collect(),
        names: vec![
            "id".to_string(),
            "parent".to_string(),
            "notused".to_string(),
            "detail".to_string(),
        ],
        changes: Changes::default(),
    }
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
    /// `EXPLAIN QUERY PLAN`, rendered when the statement was compiled.
    ///
    /// The lines describe the plan and the plan depends on the schema, so this
    /// is cached and invalidated exactly like the query it describes - which is
    /// the point of holding it here rather than rendering it per execution.
    QueryPlan(Vec<String>),
    /// An insert into a virtual table, which the module applies.
    VirtualInsert(Box<inillucent_sql::dml::BoundInsert>),
    /// A delete from a virtual table, with the query that finds its rowids.
    ///
    /// A module owns its storage, so the only handle on one of its rows is the
    /// rowid it answers with: the plan asks which rowids match and the module
    /// is told about each. That is what SQLite does, and the reason `xUpdate`
    /// takes a rowid rather than a predicate.
    VirtualDelete(
        Box<inillucent_sql::dml::BoundDelete>,
        Box<PhysicalPlan>,
        Box<physical::Prepared>,
    ),
    /// A query.
    Select(Box<PhysicalPlan>, Box<physical::Prepared>),
    /// An insert, with the plan for its `SELECT` source when it has one.
    ///
    /// The flag says whether a `VALUES` list holds a subquery. It is decided
    /// once, here, because the alternative is walking the value expressions on
    /// every execution of every insert - and `BoundExpr::children` allocates a
    /// vector per node, which is the cost this project already measured on the
    /// read path at about 0.07 us per execution.
    Insert(
        Box<inillucent_sql::dml::BoundInsert>,
        Option<(Box<PhysicalPlan>, Box<physical::Prepared>)>,
        bool,
    ),
    /// An update, with the plan that finds the rows it changes.
    ///
    /// The flag says whether an assignment holds a subquery, for the reason
    /// above.
    Update(
        Box<inillucent_sql::dml::BoundUpdate>,
        Box<PhysicalPlan>,
        Box<physical::Prepared>,
        bool,
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

/// Turns a parse or bind failure into a database error, keeping its kind.
///
/// **The message is exactly what it was**; what this adds is that a refusal the
/// binder marked `Unsupported` - "a construct the grammar has but this phase
/// does not implement" - arrives carrying that fact, where every conversion
/// site used to flatten it into an ordinary `SQLITE_MISUSE`.
///
/// It matters because this engine is deliberately incomplete, and a caller in
/// front of it has to tell "this engine cannot do that yet" from "you typed it
/// wrong" without matching on the wording of a sentence. `inillucent-driver`
/// is that caller; `ParseErrorKind::Refused` stays a plain misuse, because it
/// is the reference's own wording for a statement the schema will not have and
/// is not a gap in this engine.
///
/// @param error - the parser's or binder's failure
fn refused(error: inillucent_sql::diagnostic::ParseError) -> inillucent_base::error::DbError {
    // **The sentence goes in the message as well as the detail**, and that is a
    // fix rather than a flourish. `misuse` attaches what it is given as
    // *detail*, so every refusal this engine produced answered `message()` with
    // its primary code's own text - "bad parameter or other API misuse" - and
    // the sentence a person can act on was in the field `inillucent-base`
    // documents as never leaving the process. `inillucent-cli::shell::reason`
    // and `readgate::why` had each worked around it separately, which is what a
    // defect looks like when it has been met twice and fixed neither time.
    //
    // A parse or bind refusal is caller-safe by construction: it names tables,
    // columns and constructs, which are the caller's own words, and never a
    // path, a bound value or page bytes. The detail is left in place so that
    // everything reading it - the shell, the gate, the surface inventory -
    // sees exactly what it saw before.
    let built = misuse(error.message()).with_message(error.message());
    match error.kind {
        inillucent_sql::diagnostic::ParseErrorKind::Unsupported(what) => {
            built.with_unsupported(what)
        }
        _ => built,
    }
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
    /// Which index trees cover which table, so a query a trigger body runs
    /// inside the write reaches the same covering indexes a typed one does.
    covering: &'a HashMap<u32, Vec<u32>>,
}

impl WriteTarget for WriteView<'_> {
    fn parts(&mut self) -> (&mut Database, &mut dyn Trees) {
        (self.database, self.trees)
    }

    fn layout(&self, root: u32) -> Option<&SourceLayout> {
        self.layouts.get(&root)
    }

    fn catalog(&self) -> &dyn TreeCatalog {
        self
    }
}

/// The write's own view of the trees, read as a planned query reads them.
///
/// **The same trees, seen the other way round.** A trigger body is a statement
/// and has to find its rows, and it fires in the middle of a write that is
/// already holding these trees mutably. Answering as a [`TreeCatalog`] as well
/// is what lets `DELETE FROM child WHERE parent_id = OLD.id` reach the ordinary
/// planner - and so the ordinary index probe - rather than a scan written a
/// second time inside the write path.
///
/// A module's rows are the one thing it cannot answer: a virtual table's rows
/// come from the module, the module is registered on the connection, and the
/// connection is exactly what a write has split apart. A trigger body over a
/// virtual table is refused by name rather than answered with nothing.
impl TreeCatalog for WriteView<'_> {
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
        let _ = (path, params, needed);
        Err(misuse(format!(
            "a trigger body reads {}, which is a virtual table",
            String::from_utf8_lossy(&table.name)
        )))
    }
}

/// A [`TreeLog`] that writes to the database's own write-ahead log.
///
/// Every record carries the transaction it belongs to, which is what lets
/// recovery tell a committed change from one whose commit never arrived.
///
/// It holds no handle of its own: when the pool needs to write a page the log
/// has not reached, the pool asks the log directly through the closure
/// `let_the_pool_ask_the_log` registers. See `Pool::on_log_behind`.
struct WalLog<'a> {
    wal: &'a Wal,
    txn: u64,
    /// Where before-images go while a transaction is open, or `None` outside
    /// one.
    ///
    /// An autocommit statement cannot be abandoned, so it collects nothing and
    /// pays nothing for the possibility. The buffer is handed in by the caller
    /// rather than owned here because it has to outlive the log: the log lives
    /// for one statement and the transaction for many.
    undo: Option<&'a mut Vec<Before>>,
}

impl TreeLog for WalLog<'_> {
    fn log(&mut self, body: Body<'_>) -> DbResult<u64> {
        self.wal.append(self.txn, body)
    }

    fn wants_undo(&self) -> bool {
        self.undo.is_some()
    }

    fn undo(
        &mut self,
        tree: u64,
        key: &[Datum<'_>],
        before: Option<Vec<OwnedDatum>>,
    ) -> DbResult<()> {
        if let Some(buffer) = self.undo.as_mut() {
            // **The key is copied only when there is no row to put back.** A
            // restore that has a row calls `put`, which reads the key columns
            // out of the row itself; copying them a second time allocated a
            // vector per write and threw it away on every update and delete.
            let key = match before {
                Some(_) => Vec::new(),
                None => key.iter().map(OwnedDatum::from_datum).collect(),
            };
            buffer.push(Before {
                tree,
                key,
                row: before,
            });
        }
        Ok(())
    }
}

/// One row as it was before a statement inside a transaction changed it.
///
/// **Not `inillucent_txn::Undo`, and the difference is worth stating.** That
/// type carries a key and a before-image as `Vec<u8>`, because the transaction
/// engine below works in encoded rows. This engine's rows are `OwnedDatum`
/// vectors all the way down - the write path takes them, the trees store them
/// as PAX mini-columns, and there is no row-bytes encoding to borrow. Encoding
/// a row to bytes to record it and decoding it to restore it would be inventing
/// a third representation to bridge two that already exist.
///
/// So the two undo buffers are not duplicates of each other; they are the same
/// idea at two layers that disagree about what a row is, and that disagreement
/// is why routing this engine's writes through `inillucent_txn::Transaction` is
/// a piece of work rather than a wiring job.
#[derive(Clone, Debug)]
struct Before {
    /// The tree the row is in.
    tree: u64,
    /// The row's key columns, and empty when `row` carries them.
    ///
    /// A restore with a row to write back finds the key inside it, so the copy
    /// is made only for the case that needs one: a key that was not there, put
    /// back by deleting it again.
    key: Vec<OwnedDatum>,
    /// The whole row as it was, or `None` when the key was not there.
    row: Option<Vec<OwnedDatum>>,
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

/// Tells a pool how to make the log catch up when it is behind.
///
/// **Registered wherever a database and a log come together**, which is the
/// import, the open and the reopen. Without it the pool can only refuse a page
/// whose LSN is past the durable point, and a statement that dirties more pages
/// than the pool holds has no way to satisfy it: `CREATE INDEX` on the large
/// fixture failed on every one of thirty qualification rounds for exactly that
/// reason.
///
/// The sync is real. `write_ahead_point` is `durable_end` under NORMAL and
/// FULL, so what is handed back is a point the log has reached rather than one
/// it has merely been given bytes for, and the pool's guard still refuses if it
/// is not far enough.
///
/// @param pool - the pool that will do the asking
/// @param wal - the log it should ask
fn let_the_pool_ask_the_log(pool: &Pool, wal: &std::rc::Rc<Wal>) {
    let held = std::rc::Rc::clone(wal);
    pool.on_log_behind(Box::new(move || {
        held.sync()?;
        Ok(held.write_ahead_point())
    }));
}

/// A [`RowRedo`] that learns a tree's shape from the catalog rows it replays.
///
/// **The problem it solves.** `TreeRows` has to be told every tree's column
/// directory before the replay starts, and the only place to get one is the
/// catalog. But the catalog a reader can read before the replay is the catalog
/// as at the *last checkpoint* - so a table created after it, and then written
/// to, names a tree the applier has never heard of, and the replay refuses.
/// That is not a corner: a database that is created, given a schema and filled
/// without ever being checkpointed is the ordinary shape of a crash, and it is
/// exactly what `ImportedDatabase::create` followed by DDL produces.
///
/// **Why it works.** A `CREATE TABLE` is a row inserted into the catalog tree,
/// and the log is replayed in LSN order - so that row goes past *before* any row
/// of the tree it describes. Watching the catalog tree go by is therefore enough
/// to know every shape by the time it is needed, and it needs no second pass.
///
/// The catalog row is decoded by `inillucent-catalog`'s own decoder rather than
/// here, because a second decoder is a second opinion about which column is
/// which, and the columns are what the format is.
struct LearningRows {
    /// The applier this delegates to, gaining trees as it goes.
    rows: TreeRows,
    /// Every catalog entry seen, so an index can find the table it is on.
    seen: Vec<SchemaEntry>,
}

impl LearningRows {
    /// Returns an applier that already knows the checkpointed catalog.
    ///
    /// @param checkpointed - the catalog as at the last checkpoint
    fn new(checkpointed: &[SchemaEntry]) -> LearningRows {
        let mut rows = TreeRows::new().with_tree(
            inillucent_catalog::paged::SCHEMA_TREE_ID,
            schema_layout(),
            1,
        );
        for entry in checkpointed {
            let Ok(identifier) = identifier_of(entry) else {
                continue;
            };
            let Some((columns, key_columns)) = shape_of(entry, checkpointed, identifier) else {
                continue;
            };
            rows = rows.with_tree(u64::from(identifier), columns, key_columns);
        }
        LearningRows {
            rows,
            seen: checkpointed.to_vec(),
        }
    }

    /// Learns a tree's shape from a catalog row the replay is about to apply.
    ///
    /// Silent about a row it cannot make a shape of - a view, a trigger, an
    /// index whose table has not gone past yet - because the applier refuses by
    /// name if a record then needs it, and refusing there says which tree.
    ///
    /// @param row - the catalog row's encoded values
    fn learn(&mut self, row: &[u8]) {
        let Ok(values) = decode_row(row) else {
            return;
        };
        let Ok(entry) = inillucent_catalog::paged::entry_from_row(&values) else {
            return;
        };
        self.seen.push(entry.clone());
        let Ok(identifier) = identifier_of(&entry) else {
            return;
        };
        if let Some((columns, key_columns)) = shape_of(&entry, &self.seen, identifier) {
            let held = std::mem::take(&mut self.rows);
            self.rows = held.with_tree(u64::from(identifier), columns, key_columns);
        }
    }
}

/// Decodes a run of tagged values, which is how a row record carries a row.
///
/// @param row - the record's row bytes
fn decode_row(row: &[u8]) -> DbResult<Vec<Datum<'_>>> {
    let mut values = Vec::new();
    let mut at = 0usize;
    while at < row.len() {
        let (value, width) = Datum::decode_tagged(row.get(at..).unwrap_or(&[]))?;
        values.push(value);
        at = at.saturating_add(width);
    }
    Ok(values)
}

impl RowRedo for LearningRows {
    fn insert_row(
        &mut self,
        database: &mut Database,
        tree: u64,
        page: PageId,
        row: &[u8],
        lsn: u64,
    ) -> DbResult<()> {
        if tree == inillucent_catalog::paged::SCHEMA_TREE_ID {
            self.learn(row);
        }
        self.rows.insert_row(database, tree, page, row, lsn)
    }

    fn delete_row(
        &mut self,
        database: &mut Database,
        tree: u64,
        page: PageId,
        key: &[u8],
        lsn: u64,
    ) -> DbResult<()> {
        self.rows.delete_row(database, tree, page, key, lsn)
    }

    fn update_in_place(
        &mut self,
        database: &mut Database,
        tree: u64,
        page: PageId,
        key: &[u8],
        column: u32,
        value: &[u8],
        lsn: u64,
    ) -> DbResult<()> {
        self.rows
            .update_in_place(database, tree, page, key, column, value, lsn)
    }

    fn compact_leaf(
        &mut self,
        database: &mut Database,
        tree: u64,
        page: PageId,
        lsn: u64,
    ) -> DbResult<()> {
        self.rows.compact_leaf(database, tree, page, lsn)
    }
}

/// Returns a catalog entry's tree shape, for the recovery applier.
///
/// The same derivations the open uses below, in the one form the applier wants:
/// the column directory and how many leading columns form the key. An entry
/// whose declaration will not parse - or an index whose table is not in the
/// catalog - answers `None`, and the applier then refuses any record naming it
/// rather than replaying into a shape it guessed.
///
/// @param entry - the catalog row
/// @param catalog - every row, so an index can find its table
/// @param identifier - the tree's identifier
fn shape_of(
    entry: &SchemaEntry,
    catalog: &[SchemaEntry],
    identifier: u32,
) -> Option<(Vec<ColumnSpec>, usize)> {
    match entry.kind {
        ObjectKind::Table => {
            let mut info = table_from_create_sql(&entry.sql, 0, identifier).ok()?;
            info.root = identifier;
            if info.without_rowid {
                let (columns, key_columns, _) = keyed_table_shape(&info).ok()?;
                Some((columns, key_columns))
            } else {
                let (columns, _) = table_shape(&info);
                Some((columns, 1))
            }
        }
        ObjectKind::Index => {
            let folded = entry.table.to_ascii_lowercase();
            let owner = catalog.iter().find(|held| {
                held.kind == ObjectKind::Table && held.name.to_ascii_lowercase() == folded
            })?;
            let mut table = table_from_create_sql(&owner.sql, 0, owner.tree_id as u32).ok()?;
            table.root = owner.tree_id as u32;
            let index =
                inillucent_catalog::load::index_from_create_sql(&entry.sql, &table, identifier)
                    .ok()?;
            let (columns, _) = index_shape(&table, &index, identifier);
            let key_columns = columns.len();
            Some((columns, key_columns))
        }
        _ => None,
    }
}

/// Returns the identifier a catalog row registers its tree under.
///
/// **Refused rather than defaulted.** A zero here is a row written before the
/// identifier was persisted, and guessing one would put the tree back in the
/// state this change exists to leave: a number the writer did not use, which
/// recovery would follow to the wrong tree. A file that does not say is a file
/// this engine will not open.
///
/// @param entry - the catalog row
fn identifier_of(entry: &SchemaEntry) -> DbResult<u32> {
    if entry.tree_id == 0 {
        return Err(misuse(format!(
            "the catalog row for {} carries no tree identifier; the database predates              task-1834 and has to be rebuilt",
            String::from_utf8_lossy(&entry.name)
        )));
    }
    u32::try_from(entry.tree_id).map_err(|_| {
        misuse(format!(
            "the catalog row for {} carries a tree identifier that does not fit",
            String::from_utf8_lossy(&entry.name)
        ))
    })
}

/// Returns `sqlite_schema` under the name almost every tool actually types.
///
/// **`sqlite_master` is the same table, and a database that could not answer it
/// would be one no existing tool could inspect.** SQLite accepts both names;
/// the old engine synthesised the alias in `inillucent-catalog`, through a
/// helper that reaches into `inillucent-storage` and so cannot outlive it. This
/// is the same idea with the new engine's own schema table, registered beside
/// it rather than instead of it.
///
/// The alias is a name that resolves, not a row: `sqlite_schema` has never
/// listed itself, and it does not list this either.
///
/// @param schema - the schema table's own declaration
fn schema_alias_of(schema: &TableInfo) -> TableInfo {
    let mut alias = schema.clone();
    alias.name = b"sqlite_master".to_vec();
    alias.folded = b"sqlite_master".to_vec();
    alias
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

/// Returns whether a declared column is a `VIRTUAL` generated one.
///
/// The one predicate behind the whole `VIRTUAL` shift. A `VIRTUAL` generated
/// column is computed on read and never written, so it occupies no field in a
/// SQLite record and no column in one of this engine's trees; a `STORED` one is
/// an ordinary column that happens to have been filled in by an expression.
///
/// @param info - the table's declaration
/// @param declared - the column's declared position
fn is_virtual_column(info: &TableInfo, declared: usize) -> bool {
    info.columns
        .get(declared)
        .is_some_and(|column| column.generated && !column.stored)
}

/// Returns the declared positions a table's record holds, in record order.
///
/// **One derivation of "which columns are actually stored", used by the shape,
/// the import and the row comparison alike.** Everything that walks a record -
/// the tree builder, the fixture importer, `logical_row` - used to walk
/// `0..info.columns.len()` and so silently assumed that a declared position and
/// a record field are the same number. They are, until a table declares a
/// `VIRTUAL` generated column, after which every later column reads one field
/// early and returns its neighbour's value: data rather than a refusal, which
/// is the one failure this engine is not allowed to have.
///
/// The rowid-alias column is included, because SQLite's record does carry a
/// (NULL) field for it and the callers drop it themselves.
///
/// @param info - the table's declaration
fn stored_positions(info: &TableInfo) -> Vec<usize> {
    (0..info.columns.len())
        .filter(|declared| !is_virtual_column(info, *declared))
        .collect()
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
        for (field, declared) in stored_positions(info).iter().enumerate() {
            if Some(*declared) == alias {
                continue;
            }
            out.push(
                stored
                    .get(field.saturating_add(1))
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
    // The record's width is the count of *stored* columns, not of declared
    // ones: SQLite writes no field for a `VIRTUAL` generated column.
    let positions = stored_positions(info);
    let raw = file.read_table(info.root, positions.len())?;
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
        for (field, declared) in positions.iter().enumerate() {
            if Some(*declared) == alias {
                continue;
            }
            out.push(
                row.get(field.saturating_add(1))
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
    let alias = info.rowid_alias.map(usize::from);
    // `slots` is indexed by *declared* position and `None` means the tree does
    // not carry that column, which is exactly what a `VIRTUAL` generated column
    // is: it takes no record field and no tree column, and every column
    // declared after one therefore sits that many places earlier in the tree.
    let mut slots: Vec<Option<usize>> = vec![None; info.columns.len()];
    let mut next = 1usize;
    let mut columns = vec![ColumnSpec::key(PhysicalType::Int64)];
    let mut types = vec![StaticType::Int];
    for declared in stored_positions(info) {
        if Some(declared) == alias {
            if let Some(slot) = slots.get_mut(declared) {
                *slot = Some(0);
            }
            continue;
        }
        let (physical, static_type) = match info.columns.get(declared) {
            Some(column) => physical_for(column.affinity),
            None => (PhysicalType::Any, StaticType::Unknown),
        };
        columns.push(
            ColumnSpec::new(physical).with_collation(
                info.columns
                    .get(declared)
                    .map(|column| collation_of(&column.collation))
                    .unwrap_or(Collation::Binary),
            ),
        );
        types.push(static_type);
        if let Some(slot) = slots.get_mut(declared) {
            *slot = Some(next);
        }
        next = next.saturating_add(1);
    }
    let width = next;
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
    let (columns, key_columns, layout) = keyed_table_shape(info)?;
    // The record's width is the count of *stored* columns: SQLite writes no
    // field for a `VIRTUAL` generated column here either.
    let rows = file.read_index(info.root, layout.width)?;
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
    // A `VIRTUAL` generated column is in no record and so in no tree column,
    // here for the same reason it is in none of a rowid table's - and SQLite
    // will not let one be part of a primary key, so the filter is only needed
    // over the columns that follow the key.
    for slot in 0..width {
        if !order.contains(&slot) && !is_virtual_column(info, slot) {
            order.push(slot);
        }
    }
    let stored_width = order.len();
    let mut columns = Vec::with_capacity(stored_width);
    let mut types = Vec::with_capacity(stored_width);
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
            width: stored_width,
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
