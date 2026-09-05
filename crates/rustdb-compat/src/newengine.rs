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
//! It is the Phase 1 stand-in for the session layer, which the TDD schedules for
//! Phase 2. There is no connection, no transaction, no statement cache and no
//! prepared-statement API here: a query is parsed, planned and run in one call
//! over trees held in memory. That is enough to answer the phase gate, which is
//! about whether a vectorised scan over a PAX leaf reaches 5x, and it is
//! deliberately not enough to be mistaken for the engine.
//!
//! ## The import
//!
//! Both engines get the same logical rows and neither gets the other's file.
//! [`ImportedDatabase::import`] reads the SQLite fixture through
//! `rustdb-sqlite-reader` and bulk-builds one PAX tree per table and per index.
//! The trees are keyed by the *SQLite root page* the fixture's schema recorded,
//! because that is the identifier the binder's catalog and the planner's access
//! paths already speak, so nothing has to guess a mapping by name.
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

use rustdb_base::error::misuse;
use rustdb_base::limits::Limits;
use rustdb_base::DbResult;
use rustdb_exec::physical::{self, SourceLayout, TreeCatalog};
use rustdb_exec::StaticType;
use rustdb_sql::bind::{AllowAll, Binder, BoundStatement};
use rustdb_sql::catalog_view::{IndexInfo, StaticCatalog, TableInfo};
use rustdb_sql::parser::parse_next_statement;
use rustdb_sql::plan::{plan_select_with, Levers, PhysicalPlan};
use rustdb_sqlite_reader::SqliteFile;
use rustdb_tree::datum::{Datum, OwnedDatum};
use rustdb_tree::types::{ColumnSpec, PhysicalType};
use rustdb_tree::Tree;

/// A fixture imported into the new engine's trees.
pub struct ImportedDatabase {
    catalog: StaticCatalog,
    trees: HashMap<u32, Tree>,
    layouts: HashMap<u32, SourceLayout>,
    /// For each table root, its index roots ordered smallest tree first.
    covering: HashMap<u32, Vec<u32>>,
    page_size: usize,
    limits: Limits,
}

impl TreeCatalog for ImportedDatabase {
    fn tree(&self, root: u32) -> Option<&Tree> {
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
    /// Imports a SQLite fixture into PAX trees.
    ///
    /// @param path - the SQLite database to read
    /// @param page_size - the page size to build the new trees at
    pub fn import(path: PathBuf, page_size: usize) -> DbResult<ImportedDatabase> {
        let mut file = SqliteFile::open(path)?;
        // One schema reader, not two: `rustdb-catalog`'s loader parses every
        // CREATE TABLE and CREATE INDEX and attaches each index to its table
        // with its key columns, collations and descending flags. Re-deriving
        // any of that here would be a second implementation that could
        // disagree with the binder about what the schema says - and the binder
        // is the thing whose plans this import has to satisfy.
        let loaded = file.catalog(b"main")?;
        let mut catalog = StaticCatalog::empty();
        let mut trees = HashMap::new();
        let mut layouts = HashMap::new();
        let mut covering: HashMap<u32, Vec<u32>> = HashMap::new();

        for info in &loaded.tables {
            if info.root == 0 || info.name.starts_with(b"sqlite_") {
                continue;
            }
            let (tree, layout) = import_table(&mut file, info, page_size)?;
            trees.insert(info.root, tree);
            layouts.insert(info.root, layout);
            for index in &info.indexes {
                if index.root == 0 {
                    continue;
                }
                let (tree, layout) = import_index(&mut file, info, index, index.root, page_size)?;
                trees.insert(index.root, tree);
                layouts.insert(index.root, layout);
                covering
                    .entry(info.root)
                    .or_insert_with(Vec::new)
                    .push(index.root);
            }
            catalog = catalog.with_table(info.clone());
        }

        // Smallest tree first, so the physical pass takes the cheapest
        // structure that covers the query. Sorting by bytes rather than by
        // column count is what makes it the right order: an index with more
        // columns but shorter values can still be the smaller scan.
        for roots in covering.values_mut() {
            roots.sort_by_key(|root| trees.get(root).map(Tree::byte_size).unwrap_or(usize::MAX));
        }

        Ok(ImportedDatabase {
            catalog,
            trees,
            layouts,
            covering,
            page_size,
            limits: Limits::default(),
        })
    }

    /// Returns the bytes one root's tree occupies.
    ///
    /// Reported beside a measurement so a reader can see how much data each
    /// engine's chosen structure actually reads.
    ///
    /// @param root - the root page id the fixture recorded
    pub fn byte_size(&self, root: u32) -> Option<usize> {
        self.trees.get(&root).map(Tree::byte_size)
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
        self.trees.get(&root).map(Tree::leaf_count)
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
        let mut binder = Binder::new(&self.catalog, &parsed.ast, &authorizer)
            .with_source(sql.as_bytes());
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
    pub fn execute(&self, plan: &PhysicalPlan) -> DbResult<(Vec<Vec<OwnedDatum>>, Vec<String>)> {
        let prepared = physical::prepare(plan, self)?;
        self.execute_prepared(plan, &prepared)
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
        physical::prepare(plan, self)
    }

    /// Runs an already-prepared statement.
    ///
    /// @param plan - a plan from [`ImportedDatabase::plan`]
    /// @param prepared - the choices [`ImportedDatabase::prepare`] made
    pub fn execute_prepared(
        &self,
        plan: &PhysicalPlan,
        prepared: &physical::Prepared,
    ) -> DbResult<(Vec<Vec<OwnedDatum>>, Vec<String>)> {
        let (rows, shape) = physical::run_prepared(plan, self, prepared)?;
        let names = shape
            .names
            .iter()
            .map(|name| String::from_utf8_lossy(name).into_owned())
            .collect();
        Ok((rows, names))
    }

    /// Parses, plans and runs one statement.
    ///
    /// @param sql - the statement text
    pub fn run(&self, sql: &str) -> DbResult<(Vec<Vec<OwnedDatum>>, Vec<String>)> {
        let plan = self.plan(sql)?;
        self.execute(&plan)
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

/// Imports one table into a rowid-clustered PAX tree.
///
/// @param file - the open fixture
/// @param info - the table's catalog entry
/// @param page_size - the page size to build at
fn import_table(
    file: &mut SqliteFile,
    info: &TableInfo,
    page_size: usize,
) -> DbResult<(Tree, SourceLayout)> {
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
        columns.push(ColumnSpec::new(physical));
        types.push(static_type);
    }

    let borrowed: Vec<Vec<Datum<'_>>> = rows
        .iter()
        .map(|row| row.iter().map(OwnedDatum::borrow).collect())
        .collect();
    let tree = Tree::bulk_build(page_size, u64::from(info.root), columns, 1, &borrowed)?;
    Ok((
        tree,
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
/// @param file - the open fixture
/// @param table - the indexed table's catalog entry
/// @param index - the index's catalog entry
/// @param root - the index's root page
/// @param page_size - the page size to build at
fn import_index(
    file: &mut SqliteFile,
    table: &TableInfo,
    index: &IndexInfo,
    root: u32,
    page_size: usize,
) -> DbResult<(Tree, SourceLayout)> {
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
        columns.push(ColumnSpec::key(physical));
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
    let tree = Tree::bulk_build(page_size, u64::from(root), columns, key_columns, &borrowed)?;
    Ok((
        tree,
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

/// Chooses a mini-column layout for a declared affinity.
///
/// The mapping is the obvious one and the honesty is in what it does *not*
/// claim: `Blob` affinity (SQLite's "no affinity") gets the `Any` layout, since
/// a column with no affinity has no type to specialise on, and `Numeric` gets
/// `Any` too because it holds integers and reals interchangeably.
///
/// @param affinity - the declared affinity
fn physical_for(affinity: rustdb_value::affinity::Affinity) -> (PhysicalType, StaticType) {
    use rustdb_value::affinity::Affinity;
    match affinity {
        Affinity::Integer => (PhysicalType::Int64, StaticType::Int),
        Affinity::Real => (PhysicalType::Float64, StaticType::Real),
        Affinity::Text => (PhysicalType::Text, StaticType::Text),
        Affinity::Blob => (PhysicalType::Blob, StaticType::Unknown),
        Affinity::Numeric | Affinity::FlexNum => (PhysicalType::Any, StaticType::Unknown),
    }
}
