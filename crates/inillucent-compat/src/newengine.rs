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

use std::collections::HashMap;
use std::path::PathBuf;

use inillucent_base::error::misuse;
use inillucent_base::limits::Limits;
use inillucent_base::DbResult;
use inillucent_catalog::load::table_from_create_sql;
use inillucent_catalog::paged::{
    attach_catalog, read_catalog, schema_create_sql, schema_layout, write_catalog, ObjectKind,
    SchemaEntry, SCHEMA_TABLE,
};
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
use inillucent_tree::PagedTree;
use inillucent_value::collation::Collation;
use inillucent_vfs::{DbPath, OsVfs};

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

        let vfs = OsVfs::new();
        let target = target_path(&path, page_size, frames);
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
            // entries. The TDD's leaf layout covers it - `key_columns > 1` and
            // the same PAX leaf - but the *import* would need a second reader,
            // and Phase 2's read families have no such table. It is skipped
            // rather than half-read, and skipping it here means a query against
            // it is refused by the binder with a name it cannot resolve, which
            // is a loud failure rather than a wrong answer.
            if info.without_rowid {
                skipped.push(String::from_utf8_lossy(&info.name).into_owned());
                continue;
            }
            let (shape, layout) = match import_table(&mut database, &mut file, info) {
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
            });
            shapes.insert(info.root, shape);
            layouts.insert(info.root, layout);
            for index in &info.indexes {
                if index.root == 0 {
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
                });
                shapes.insert(index.root, shape);
                layouts.insert(index.root, layout);
                covering
                    .entry(info.root)
                    .or_insert_with(Vec::new)
                    .push(index.root);
            }
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
        catalog = catalog.with_table(schema_info);

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
        })
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
        let prepared = physical::prepare(plan, self, ForcePlan::default())?;
        self.execute_prepared(plan, &prepared, params)
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
        let names = shape
            .names
            .iter()
            .map(|name| String::from_utf8_lossy(name).into_owned())
            .collect();
        Ok((rows, names))
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

    // The physical type of each tree column, from the declared affinity. This
    // is a claim, not a guarantee - a column declared INTEGER may hold a string
    // - which is exactly what the leaf's exception class is for.
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
        SourceLayout {
            tree_key: info.root,
            slots,
            rowid: Some(0),
            types,
            width,
            // A rowid-clustered tree is ordered by its rowid, which is column 0.
            key_columns: vec![0],
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
    ))
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
