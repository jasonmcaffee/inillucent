//! The R-Tree module: a spatial index whose nodes live in an ordinary table.
//!
//! Invariant: the bytes are SQLite's. An R-Tree's whole state is three shadow
//! tables and one blob format, all of them documented, so a database this
//! module writes is one the pinned release opens and queries - and the tests
//! prove it by handing files back and forth. That is the difference between an
//! R-Tree and something that answers the same questions.
//!
//! The format, which is what the rest of this file is about:
//!
//! - `%_node(nodeno INTEGER PRIMARY KEY, data BLOB)` holds one blob per node.
//!   Node 1 is the root and always exists.
//! - `%_rowid(rowid INTEGER PRIMARY KEY, nodeno INTEGER)` says which leaf holds
//!   each row, so a delete does not have to search.
//! - `%_parent(nodeno INTEGER PRIMARY KEY, parentnode INTEGER)` says which node
//!   holds each interior node, so a split can grow upwards.
//!
//! A node blob is a four-byte header - two bytes of tree depth, meaningful only
//! in node 1, then two bytes of cell count - followed by the cells. A cell is an
//! eight-byte big-endian number, which is a rowid in a leaf and a child node
//! number in an interior node, followed by two four-byte coordinates per
//! dimension: the minimum and then the maximum. `rtree` reads them as IEEE-754
//! binary32 and `rtree_i32` as signed integers, and that is the only difference
//! between the two modules.

use inillucent_base::{DbResult, PrimaryCode};
use inillucent_value::Value;

use super::{
    constraint, failure, Change, ConstraintOp, Context, Declaration, DeclaredColumn, FilterPlan,
    IndexQuery, Module, ModuleArguments, ShadowTable, VirtualCursor, VirtualTable, ROWID_COLUMN,
};
use crate::shadow::ShadowTables;

/// The shadow rows a transaction has written but not yet flushed.
///
/// **The R-Tree's whole cost was its shadow tables.** One insert reads the root,
/// rewrites every node on the way down so the boxes it passes grow to hold the
/// new cell, writes the leaf, writes a `%_rowid` row and sometimes a `%_parent`
/// row - so a five-hundred-cell load did thousands of shadow-table reads and
/// writes, each one a descent into a b-tree, and `extension.rtree.insert`
/// measured 0.28x against SQLite. None of those intermediate
/// states is durable state anyone can see: only the last one is. So they are
/// held here and written once, in `sync`, the same way FTS5 holds a segment.
///
/// Every read goes through this too, which is what makes it correct rather than
/// merely fast: a node this transaction has already rewritten is answered from
/// here, so a descent sees its own writes. The cursor shares it for the same
/// reason - a `SELECT` in the transaction that wrote must see what was written.
#[derive(Default)]
struct Pending {
    /// Node blobs by node number. `None` records a node known to be absent.
    nodes: std::collections::HashMap<i64, Option<Vec<u8>>>,
    /// Which leaf holds each row. `None` records a row known to be absent.
    rowids: std::collections::HashMap<i64, Option<i64>>,
    /// Which node holds each node.
    parents: std::collections::HashMap<i64, Option<i64>>,
    /// The node numbers whose entry above still has to be written.
    dirty_nodes: std::collections::HashSet<i64>,
    /// The rowids whose entry above still has to be written or deleted.
    dirty_rowids: std::collections::HashSet<i64>,
    /// The node numbers whose parent entry still has to be written.
    dirty_parents: std::collections::HashSet<i64>,
    /// The largest node number seen, so a split does not reuse one that is
    /// buffered rather than stored.
    highest_node: Option<i64>,
    /// The largest rowid seen, for the same reason.
    highest_rowid: Option<i64>,
    /// The auxiliary column values of each buffered row.
    ///
    /// They ride with the leaf number rather than in a second map keyed the
    /// same way, because they are written into the same shadow row and a second
    /// map is a second thing to keep in step.
    auxiliary: std::collections::HashMap<i64, Vec<Value<'static>>>,
}

/// The buffer, shared between a table and the cursors it opens.
type Buffer = std::sync::Arc<std::sync::Mutex<Pending>>;

impl Pending {
    /// Writes everything buffered and forgets that it was dirty.
    ///
    /// The read cache is kept: a transaction that inserts and then reads should
    /// not go back to the tree for a node it has in hand, and the entries are
    /// now what the tree holds anyway.
    ///
    /// @param context - the statement's context
    /// @param shadows - the three shadow tables
    fn flush(&mut self, context: &mut Context<'_>, shadows: &ShadowTables) -> DbResult<()> {
        let mut nodes: Vec<i64> = self.dirty_nodes.iter().copied().collect();
        nodes.sort_unstable();
        for number in nodes {
            if let Some(Some(bytes)) = self.nodes.get(&number) {
                shadows.write_row(
                    context,
                    b"node",
                    number,
                    &[Value::Null, Value::owned_blob(bytes)?],
                )?;
            }
        }
        self.dirty_nodes.clear();
        let mut rowids: Vec<i64> = self.dirty_rowids.iter().copied().collect();
        rowids.sort_unstable();
        for rowid in rowids {
            match self.rowids.get(&rowid) {
                Some(Some(node)) => {
                    let mut row = vec![Value::Null, Value::Integer(*node)];
                    if let Some(extra) = self.auxiliary.get(&rowid) {
                        row.extend(extra.iter().cloned());
                    }
                    shadows.write_row(context, b"rowid", rowid, &row)?
                }
                _ => shadows.delete_row(context, b"rowid", rowid)?,
            }
        }
        self.dirty_rowids.clear();
        let mut parents: Vec<i64> = self.dirty_parents.iter().copied().collect();
        parents.sort_unstable();
        for node in parents {
            if let Some(Some(parent)) = self.parents.get(&node) {
                shadows.write_row(
                    context,
                    b"parent",
                    node,
                    &[Value::Null, Value::Integer(*parent)],
                )?;
            }
        }
        self.dirty_parents.clear();
        Ok(())
    }
}

/// How many bytes a node's header takes.
const HEADER: usize = 4;
/// The most cells the format allows in one node.
///
/// It is not a size limit, it is a *format* limit: the pinned release refuses
/// to walk a node with more than this many cells, whatever the node's size, so
/// a node with more is a node it calls corrupt. A tree written with a larger
/// page size therefore has smaller nodes rather than fuller ones, and this is
/// the number that decides it. It was found by handing SQLite a tree of 60 rows
/// and being told the database was malformed.
const MAX_CELLS: usize = 51;
/// How many bytes the rowid or child number of one cell takes.
const CELL_KEY: usize = 8;
/// How many bytes one coordinate takes.
const COORDINATE: usize = 4;
/// The node the root always is.
const ROOT: i64 = 1;

/// How the coordinates are stored.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Coordinates {
    /// `rtree`: IEEE-754 binary32.
    Float,
    /// `rtree_i32`: signed 32-bit integers.
    Integer,
}

/// The `rtree` and `rtree_i32` modules.
pub struct RTreeModule {
    coordinates: Coordinates,
    name: &'static str,
    /// Whether the rows are polygons rather than boxes.
    ///
    /// **`geopoly` is this structure with a different front.** Its rows are
    /// two-dimensional, its coordinates are the reals, and its tree, its node
    /// format and its three shadow tables are the ones above - what differs is
    /// that the box is *computed from a shape* rather than written by the
    /// caller, and that the shape is the first column rather than the
    /// coordinates being columns at all. SQLite implements it the same way, in
    /// the same file, for the same reason: a second copy of the node splitting
    /// is a second place for it to be wrong.
    shape: bool,
}

impl RTreeModule {
    /// Returns the floating-point module.
    pub fn float() -> RTreeModule {
        RTreeModule {
            coordinates: Coordinates::Float,
            name: "rtree",
            shape: false,
        }
    }

    /// Returns the integer module.
    pub fn integer() -> RTreeModule {
        RTreeModule {
            coordinates: Coordinates::Integer,
            name: "rtree_i32",
            shape: false,
        }
    }

    /// Returns the polygon module.
    pub fn geopoly() -> RTreeModule {
        RTreeModule {
            coordinates: Coordinates::Float,
            name: "geopoly",
            shape: true,
        }
    }
}

/// What a `CREATE VIRTUAL TABLE ... USING rtree(...)` declared.
///
/// The two lists are genuinely different things and keeping them apart is what
/// makes the rest of the module simple: the coordinates are *in the tree*, in
/// the cell, and every query plan is about them; an auxiliary column is a value
/// carried alongside the row and no query is ever planned on it.
pub struct RTreeShape {
    /// The rowid's name, then a minimum and a maximum per dimension.
    pub coordinates: Vec<Vec<u8>>,
    /// The `+name` columns, in written order.
    pub auxiliary: Vec<Vec<u8>>,
}

/// Returns the column names a `CREATE VIRTUAL TABLE ... USING rtree(...)` gave.
///
/// The first is the rowid's name and the rest come in pairs, one minimum and
/// one maximum per dimension. One to five dimensions, which is SQLite's limit
/// and is a limit on the *format*: a cell has to fit in a node.
///
/// **A name written `+label` is an auxiliary column**, and the leading `+` is
/// the whole of the syntax. It does not count towards the odd-number rule,
/// because it is not half of a dimension - which is why the count is checked
/// after the split rather than before it.
/// Returns the column names, reading them the way the module in use writes them.
///
/// **`geopoly` has no coordinate columns at all**: its arguments are the
/// auxiliary columns and nothing else, because the two dimensions are computed
/// from `_shape` rather than written. Sharing the reader rather than writing a
/// second one keeps one description of what an auxiliary column is.
///
/// @param arguments - the arguments inside the parentheses
/// @param shape - whether this is `geopoly`
fn parse_arguments_for(arguments: &[Vec<u8>], shape: bool) -> DbResult<RTreeShape> {
    if shape {
        return Ok(RTreeShape {
            coordinates: vec![
                b"rowid".to_vec(),
                b"_minx".to_vec(),
                b"_maxx".to_vec(),
                b"_miny".to_vec(),
                b"_maxy".to_vec(),
            ],
            auxiliary: arguments
                .iter()
                .filter_map(|argument| {
                    let word = argument
                        .split(|byte| byte.is_ascii_whitespace())
                        .find(|part| !part.is_empty())?;
                    Some(word.to_vec())
                })
                .collect(),
        });
    }
    parse_coordinate_arguments(arguments)
}

/// Returns the column names a `CREATE VIRTUAL TABLE ... USING rtree(...)` gave.
fn parse_coordinate_arguments(arguments: &[Vec<u8>]) -> DbResult<RTreeShape> {
    let mut coordinates: Vec<Vec<u8>> = Vec::new();
    let mut auxiliary: Vec<Vec<u8>> = Vec::new();
    for argument in arguments {
        // A column may be written `minX REAL`; only the name is kept, which is
        // what SQLite does with an R-Tree's declared types.
        let word = argument
            .split(|byte| byte.is_ascii_whitespace())
            .find(|part| !part.is_empty())
            .unwrap_or(argument);
        match word.split_first() {
            Some((b'+', rest)) => auxiliary.push(rest.to_vec()),
            _ => coordinates.push(word.to_vec()),
        }
    }
    if coordinates.len() < 3 || coordinates.len() > 11 || coordinates.len().is_multiple_of(2) {
        return Err(failure(
            "an rtree table needs an odd number of columns between 3 and 11",
        ));
    }
    Ok(RTreeShape {
        coordinates,
        auxiliary,
    })
}

impl Module for RTreeModule {
    /// Returns the module's name.
    fn name(&self) -> &str {
        self.name
    }

    /// The three shadow tables the format needs.
    fn shadow_tables(&self, arguments: &ModuleArguments) -> DbResult<Vec<ShadowTable>> {
        let shape = parse_arguments_for(&arguments.arguments, self.shape)?;
        // The shape is the first auxiliary column, so the `%_rowid` table needs
        // a slot for it alongside the ones the caller named.
        let shape = RTreeShape {
            coordinates: shape.coordinates,
            auxiliary: if self.shape {
                let mut named = vec![b"_shape".to_vec()];
                named.extend(shape.auxiliary);
                named
            } else {
                shape.auxiliary
            },
        };
        Ok(vec![
            ShadowTable {
                suffix: b"node".to_vec(),
                create_sql: "CREATE TABLE \"%_node\"(nodeno INTEGER PRIMARY KEY, data BLOB)"
                    .to_string(),
                owner: None,
            },
            ShadowTable {
                suffix: b"rowid".to_vec(),
                // **The auxiliary columns live here**, one `aN` per `+name`,
                // which is where SQLite puts them: they are per row rather than
                // per node, and the rowid table is the one shadow with a row per
                // row. Putting them in the node would grow every cell and shrink
                // the fan-out of a structure whose whole value is its fan-out.
                create_sql: format!(
                    "CREATE TABLE \"%_rowid\"(rowid INTEGER PRIMARY KEY, nodeno INTEGER{})",
                    (0..shape.auxiliary.len())
                        .map(|at| format!(", a{at}"))
                        .collect::<String>()
                ),
                owner: None,
            },
            ShadowTable {
                suffix: b"parent".to_vec(),
                create_sql:
                    "CREATE TABLE \"%_parent\"(nodeno INTEGER PRIMARY KEY, parentnode INTEGER)"
                        .to_string(),
                owner: None,
            },
        ])
    }

    /// Connects to a table, making the root node when it is being created.
    fn connect(
        &self,
        arguments: &ModuleArguments,
        creating: bool,
    ) -> DbResult<Box<dyn VirtualTable>> {
        let shape = parse_arguments_for(&arguments.arguments, self.shape)?;
        let names = shape.coordinates;
        let dimensions = names.len().saturating_sub(1) / 2;
        // **`geopoly` declares `_shape` and the auxiliary columns, and nothing
        // else.** The bounding box is in the tree and is never a column, which
        // is why a `geopoly` table has no `minX` to write a predicate against
        // and every query over one is written with the shape functions.
        let auxiliary: Vec<Vec<u8>> = if self.shape {
            let mut named = vec![b"_shape".to_vec()];
            named.extend(shape.auxiliary.iter().cloned());
            named
        } else {
            shape.auxiliary.clone()
        };
        let mut columns = Vec::new();
        if !self.shape {
            columns.push(
                DeclaredColumn::visible(&String::from_utf8_lossy(
                    names.first().map(Vec::as_slice).unwrap_or(b"id"),
                ))
                .typed("INTEGER"),
            );
            for name in names.get(1..).unwrap_or_default() {
                columns.push(
                    DeclaredColumn::visible(&String::from_utf8_lossy(name)).typed(
                        if self.coordinates == Coordinates::Integer {
                            "INT"
                        } else {
                            "REAL"
                        },
                    ),
                );
            }
        }
        // An auxiliary column is declared with no type, so it keeps whatever
        // was stored in it - which is the point of it.
        for name in &auxiliary {
            columns.push(DeclaredColumn::visible(&String::from_utf8_lossy(name)));
        }
        Ok(Box::new(RTreeTable {
            shape: self.shape,
            auxiliary: auxiliary.len(),
            coordinates: self.coordinates,
            dimensions,
            shadows: ShadowTables::of(arguments, &[b"node", b"rowid", b"parent"])?,
            declaration: Declaration {
                columns,
                without_rowid: false,
            },
            creating,
            rowid_name: names.first().cloned().unwrap_or_else(|| b"id".to_vec()),
            pending: Buffer::default(),
        }))
    }
}

/// One connected R-Tree.
struct RTreeTable {
    /// Whether the rows are polygons rather than boxes.
    shape: bool,
    /// How many `+name` columns follow the coordinates.
    auxiliary: usize,
    coordinates: Coordinates,
    dimensions: usize,
    shadows: ShadowTables,
    declaration: Declaration,
    creating: bool,
    rowid_name: Vec<u8>,
    /// The shadow rows this transaction has written but not yet flushed.
    pending: Buffer,
}

/// The plan number for a scan of every row.
const PLAN_SCAN: i32 = 0;
/// The plan number for a lookup by rowid.
const PLAN_ROWID: i32 = 1;
/// The plan number for a descent constrained by the query's bounding box.
const PLAN_BOX: i32 = 2;

impl RTreeTable {
    /// Records the auxiliary values a write supplied for one row.
    ///
    /// The values arrive in declaration order - the rowid, then the
    /// coordinates, then the auxiliary columns - so the ones wanted here are
    /// whatever follows the coordinates. A write that supplied fewer is padded
    /// with NULL rather than refused, which is what a column with no
    /// constraints on it means.
    ///
    /// @param rowid - the row being written
    /// @param values - the whole row, in declaration order
    fn remember_auxiliary(&mut self, rowid: i64, values: &[Value<'static>]) {
        if self.auxiliary == 0 {
            return;
        }
        // A polygon table's first column *is* its first auxiliary column, so
        // there is nothing in front of it to skip.
        let first = if self.shape {
            0
        } else {
            self.dimensions.saturating_mul(2).saturating_add(1)
        };
        let mut extra: Vec<Value<'static>> = values.get(first..).unwrap_or_default().to_vec();
        extra.resize(self.auxiliary, Value::Null);
        if let Ok(mut pending) = self.pending.lock() {
            pending.auxiliary.insert(rowid, extra);
        }
    }

    /// Returns how many bytes one cell takes.
    fn cell_size(&self) -> usize {
        CELL_KEY.saturating_add(self.dimensions.saturating_mul(2).saturating_mul(COORDINATE))
    }

    /// Returns how many cells fit in one node of a given size.
    fn cells_per_node(&self, node_bytes: usize) -> usize {
        (node_bytes.saturating_sub(HEADER) / self.cell_size().max(1)).min(MAX_CELLS)
    }
}

impl VirtualTable for RTreeTable {
    /// Returns the rowid column and the two coordinates per dimension.
    fn declaration(&self) -> &Declaration {
        &self.declaration
    }

    /// Chooses between a rowid lookup, a constrained descent, and a scan.
    ///
    /// The constrained descent is the whole point of the structure: every
    /// inequality on a coordinate narrows the box the walk has to enter, so a
    /// query that names one is a walk of the nodes that overlap it rather than
    /// of the whole tree. The constraints are *not* omitted - the box test at a
    /// leaf is a filter over a subtree, and the row still has to satisfy the
    /// predicate exactly.
    fn best_index(&self, query: &mut IndexQuery) -> DbResult<()> {
        for index in 0..query.constraints.len() {
            let Some(constraint) = query.constraints.get(index).copied() else {
                continue;
            };
            if !constraint.usable {
                continue;
            }
            if constraint.column == ROWID_COLUMN && constraint.op == ConstraintOp::Eq {
                query.use_constraint(index, true);
                query.index_number = PLAN_ROWID;
                query.index_string = String::new();
                query.estimated_cost = 1.0;
                query.estimated_rows = 1;
                return Ok(());
            }
        }
        let mut plan = String::new();
        for index in 0..query.constraints.len() {
            let Some(constraint) = query.constraints.get(index).copied() else {
                continue;
            };
            // A polygon table has no coordinate columns, so there is nothing
            // here for a bounding-box descent to be built from and the shape
            // predicates are left to the engine to apply over a scan.
            if !constraint.usable || constraint.column < 1 || self.shape {
                continue;
            }
            let op = match constraint.op {
                ConstraintOp::Eq => b'=',
                ConstraintOp::Gt => b'>',
                ConstraintOp::Ge => b'G',
                ConstraintOp::Lt => b'<',
                ConstraintOp::Le => b'L',
                _ => continue,
            };
            let position = query.use_constraint(index, false);
            let _ = position;
            plan.push(op as char);
            plan.push((b'0' + (constraint.column as u8).min(9)) as char);
        }
        if plan.is_empty() {
            query.index_number = PLAN_SCAN;
            query.estimated_cost = 2.0e6;
            query.estimated_rows = 1000;
            return Ok(());
        }
        query.index_number = PLAN_BOX;
        query.index_string = plan;
        // Each constraint is worth roughly a halving of the tree, which is what
        // makes a bounded query cheaper than a scan and an unbounded one not.
        let terms = query.index_string.len() / 2;
        query.estimated_cost = (1000.0 / (2.0f64).powi(terms as i32)).max(1.0);
        query.estimated_rows = query.estimated_cost as i64;
        Ok(())
    }

    /// Opens a cursor over the tree.
    fn open(&self) -> DbResult<Box<dyn VirtualCursor>> {
        Ok(Box::new(RTreeCursor {
            shape: self.shape,
            coordinates: self.coordinates,
            dimensions: self.dimensions,
            shadows: self.shadows.clone(),
            pending: Buffer::clone(&self.pending),
            rows: Vec::new(),
            at: 0,
        }))
    }

    /// Writes every buffered shadow row before the engine commits.
    ///
    /// Once per transaction rather than once per cell, which is the whole point
    /// of the buffer. `sync_modules` calls this; nothing else has to know.
    fn sync(&mut self, context: &mut Context<'_>) -> DbResult<()> {
        let mut held = match self.pending.lock() {
            Ok(held) => held,
            Err(_) => {
                return Err(inillucent_base::error::misuse(
                    "the r-tree buffer is poisoned",
                ))
            }
        };
        held.flush(context, &self.shadows)
    }

    /// Throws away everything the abandoned transaction buffered.
    ///
    /// Including the read cache: rows it holds were read under a transaction
    /// that no longer happened, and the next reader should go to the tree.
    fn rollback(&mut self, _context: &mut Context<'_>) -> DbResult<()> {
        if let Ok(mut held) = self.pending.lock() {
            *held = Pending::default();
        }
        Ok(())
    }

    /// Rolls back to a savepoint, which is the same discard.
    ///
    /// Correct for the same reason the whole-transaction form is: the engine
    /// flushes every module when a savepoint is taken, so what is left in the
    /// buffer belongs entirely to the part being abandoned.
    fn rollback_to(&mut self, context: &mut Context<'_>, _number: i32) -> DbResult<()> {
        self.rollback(context)
    }

    /// Makes the root node the first time the table is created.
    fn begin(&mut self, context: &mut Context<'_>) -> DbResult<()> {
        if !self.creating {
            return Ok(());
        }
        self.creating = false;
        let node_bytes = node_size(context, &self.shadows, &self.pending, self.cell_size());
        let mut root = vec![0u8; node_bytes];
        write_header(&mut root, 0, 0);
        put_node(context, &self.shadows, &self.pending, ROOT, &root)
    }

    /// Applies one insert, update or delete.
    fn update(&mut self, context: &mut Context<'_>, change: &Change) -> DbResult<Option<i64>> {
        match change {
            Change::Delete(rowid) => {
                let Some(rowid) = rowid.as_integer() else {
                    return Ok(None);
                };
                self.remove(context, rowid)?;
                Ok(None)
            }
            Change::Insert { rowid, values } => {
                let key = match rowid.as_integer() {
                    Some(key) => key,
                    None => values
                        .first()
                        .and_then(Value::as_integer)
                        .unwrap_or_else(|| self.next_rowid(context)),
                };
                let cell = self.cell_of(key, values)?;
                self.remember_auxiliary(key, values);
                if self.find_leaf(context, key)?.is_some() {
                    return Err(constraint(format!(
                        "UNIQUE constraint failed: {}",
                        String::from_utf8_lossy(&self.rowid_name)
                    )));
                }
                self.insert_cell(context, &cell)?;
                Ok(Some(key))
            }
            Change::Update {
                old_rowid,
                new_rowid,
                values,
            } => {
                let Some(old) = old_rowid.as_integer() else {
                    return Ok(None);
                };
                let new = new_rowid.as_integer().unwrap_or(old);
                let cell = self.cell_of(new, values)?;
                self.remember_auxiliary(new, values);
                self.remove(context, old)?;
                self.insert_cell(context, &cell)?;
                Ok(Some(new))
            }
        }
    }

    /// Checks the tree's own structure.
    ///
    /// Every leaf entry has a row in `%_rowid` pointing at the leaf that holds
    /// it, and every node but the root has a row in `%_parent`. Those two are
    /// what a delete and a split respectively depend on, and a tree where they
    /// are wrong is one where a delete silently does nothing.
    fn integrity(&mut self, context: &mut Context<'_>) -> DbResult<Option<String>> {
        let mut problems = Vec::new();
        let mut stack = vec![ROOT];
        while let Some(number) = stack.pop() {
            let Some(node) = get_node(context, &self.shadows, &self.pending, number)? else {
                problems.push(format!("node {number} is missing"));
                continue;
            };
            let (depth, count) = read_header(&node);
            let cells = self.cells_per_node(node.len());
            if count as usize > cells {
                problems.push(format!("node {number} claims {count} cells of {cells}"));
                continue;
            }
            for index in 0..count as usize {
                let Some(cell) = read_cell(&node, index, self.dimensions) else {
                    continue;
                };
                if depth == 0 {
                    match find_rowid(context, &self.shadows, &self.pending, cell.key)? {
                        Some(found) if found == number => {}
                        _ => problems
                            .push(format!("row {} is not recorded in node {number}", cell.key)),
                    }
                    continue;
                }
                match find_parent(context, &self.shadows, &self.pending, cell.key)? {
                    Some(found) if found == number => {}
                    _ => problems.push(format!(
                        "node {} does not record node {number} as its parent",
                        cell.key
                    )),
                }
                stack.push(cell.key);
            }
        }
        if problems.is_empty() {
            return Ok(None);
        }
        Ok(Some(problems.join("; ")))
    }
}

/// One cell of a node: a key and a bounding box.
#[derive(Clone, Debug, PartialEq)]
struct Cell {
    /// A rowid in a leaf, a child node number in an interior node.
    key: i64,
    /// The minimum and maximum of each dimension, interleaved.
    box_: Vec<f64>,
}

impl Cell {
    /// Returns whether this box overlaps another.
    fn overlaps(&self, other: &[f64]) -> bool {
        // Bounds-checked because `other` is a query's box and `self.box_` came
        // off a page: a dimension count that disagrees between them is exactly
        // what a damaged node looks like, and reading past either one is the
        // failure this crate's `deny(indexing_slicing)` exists to stop.
        for (mine, theirs) in self.box_.chunks_exact(2).zip(other.chunks_exact(2)) {
            let (Some(low), Some(high)) = (mine.first(), mine.get(1)) else {
                continue;
            };
            let (Some(other_low), Some(other_high)) = (theirs.first(), theirs.get(1)) else {
                continue;
            };
            if high < other_low || low > other_high {
                return false;
            }
        }
        true
    }

    /// Returns the area this box would grow by to hold another.
    fn growth(&self, other: &Cell) -> f64 {
        let mut before = 1.0f64;
        let mut after = 1.0f64;
        for (mine, theirs) in self.box_.chunks_exact(2).zip(other.box_.chunks_exact(2)) {
            let (Some(low), Some(high)) = (mine.first().copied(), mine.get(1).copied()) else {
                continue;
            };
            before *= (high - low).max(0.0) + 1.0;
            let low = low.min(theirs.first().copied().unwrap_or(low));
            let high = high.max(theirs.get(1).copied().unwrap_or(high));
            after *= (high - low).max(0.0) + 1.0;
        }
        after - before
    }

    /// Grows this box to hold another.
    fn absorb(&mut self, other: &Cell) {
        for (mine, theirs) in self
            .box_
            .chunks_exact_mut(2)
            .zip(other.box_.chunks_exact(2))
        {
            let (low, high) = (theirs.first().copied(), theirs.get(1).copied());
            if let (Some(slot), Some(value)) = (mine.first_mut(), low) {
                *slot = slot.min(value);
            }
            if let (Some(slot), Some(value)) = (mine.get_mut(1), high) {
                *slot = slot.max(value);
            }
        }
    }
}

/// Writes a node's four-byte header.
fn write_header(node: &mut [u8], depth: u16, count: u16) {
    if node.len() < HEADER {
        return;
    }
    if let Some(slot) = node.get_mut(0..2) {
        slot.copy_from_slice(&depth.to_be_bytes());
    }
    if let Some(slot) = node.get_mut(2..4) {
        slot.copy_from_slice(&count.to_be_bytes());
    }
}

/// Reads a node's four-byte header as `(depth, count)`.
fn read_header(node: &[u8]) -> (u16, u16) {
    if node.len() < HEADER {
        return (0, 0);
    }
    let at = |position: usize| node.get(position).copied().unwrap_or(0);
    (
        u16::from_be_bytes([at(0), at(1)]),
        u16::from_be_bytes([at(2), at(3)]),
    )
}

/// Reads one cell out of a node.
fn read_cell(node: &[u8], index: usize, dimensions: usize) -> Option<Cell> {
    let size = CELL_KEY + dimensions * 2 * COORDINATE;
    let at = HEADER + index * size;
    let bytes = node.get(at..at + size)?;
    let mut key = [0u8; 8];
    key.copy_from_slice(bytes.get(..8)?);
    let mut box_ = Vec::with_capacity(dimensions * 2);
    for value in 0..dimensions * 2 {
        let at = 8 + value * COORDINATE;
        let raw = bytes.get(at..at + COORDINATE)?;
        let mut word = [0u8; 4];
        word.copy_from_slice(raw);
        box_.push(f64::from(f32::from_be_bytes(word)));
    }
    Some(Cell {
        key: i64::from_be_bytes(key),
        box_,
    })
}

/// Writes one cell into a node.
fn write_cell(node: &mut [u8], index: usize, cell: &Cell) {
    let dimensions = cell.box_.len() / 2;
    let size = CELL_KEY + dimensions * 2 * COORDINATE;
    let at = HEADER + index * size;
    if node.len() < at + size {
        return;
    }
    if let Some(slot) = node.get_mut(at..at.saturating_add(8)) {
        slot.copy_from_slice(&cell.key.to_be_bytes());
    }
    for (position, value) in cell.box_.iter().enumerate() {
        let at = at.saturating_add(8).saturating_add(position * COORDINATE);
        if let Some(slot) = node.get_mut(at..at.saturating_add(COORDINATE)) {
            slot.copy_from_slice(&(*value as f32).to_be_bytes());
        }
    }
}

/// Reads every cell of a node.
fn read_cells(node: &[u8], dimensions: usize) -> Vec<Cell> {
    let (_, count) = read_header(node);
    (0..count as usize)
        .filter_map(|index| read_cell(node, index, dimensions))
        .collect()
}

/// Writes a whole set of cells into a node, replacing what was there.
fn write_cells(node: &mut [u8], depth: u16, cells: &[Cell]) {
    write_header(node, depth, cells.len() as u16);
    for (index, cell) in cells.iter().enumerate() {
        write_cell(node, index, cell);
    }
}

/// Returns the node size one table uses.
///
/// It is the size of the root node's blob when the table already has one, so a
/// table SQLite created is read with the node size SQLite chose. A table this
/// module is creating uses a size that fits a page, which is what SQLite's own
/// choice amounts to.
fn node_size(
    context: &mut Context<'_>,
    shadows: &ShadowTables,
    pending: &Buffer,
    cell: usize,
) -> usize {
    if let Ok(Some(node)) = get_node(context, shadows, pending, ROOT) {
        if node.len() >= HEADER {
            return node.len();
        }
    }
    // The host's page size when it has one, and the fallback otherwise - which
    // is the same answer a host that could not be asked already got.
    let database = context.database;
    let page = context.host.page_size(database).unwrap_or(4096);
    let usable = page.saturating_sub(64);
    let cells = (usable.saturating_sub(HEADER) / cell.max(1)).clamp(2, MAX_CELLS);
    HEADER + cells * cell
}

impl RTreeTable {
    /// Builds the cell one row becomes.
    fn cell_of(&self, key: i64, values: &[Value<'static>]) -> DbResult<Cell> {
        // **A polygon's box is computed, not written.** The whole difference
        // between the two modules is here: a `geopoly` row carries a shape and
        // the tree indexes the shape's extent, so a row whose `_shape` is not a
        // polygon has no place in the tree at all and is refused rather than
        // stored with a box of zeroes.
        if self.shape {
            let Ok(polygon) = inillucent_scalar::geopoly::Polygon::parse_for_index(values.first())
            else {
                return Err(constraint("_shape does not contain a valid polygon"));
            };
            let [low_x, high_x, low_y, high_y] =
                polygon.map(|shape| shape.bounds()).unwrap_or([0.0; 4]);
            return Ok(Cell {
                key,
                box_: vec![
                    f64::from(low_x),
                    f64::from(high_x),
                    f64::from(low_y),
                    f64::from(high_y),
                ],
            });
        }
        let mut box_ = Vec::with_capacity(self.dimensions * 2);
        for dimension in 0..self.dimensions {
            let low = coordinate(values.get(1 + dimension * 2), self.coordinates, Edge::Low);
            let high = coordinate(values.get(2 + dimension * 2), self.coordinates, Edge::High);
            if low > high {
                return Err(constraint(
                    "rtree constraint failed: a minimum is greater than its maximum",
                ));
            }
            box_.push(low);
            box_.push(high);
        }
        Ok(Cell { key, box_ })
    }

    /// Returns a rowid nothing is using.
    fn next_rowid(&self, context: &mut Context<'_>) -> i64 {
        max_rowid(context, &self.shadows, &self.pending).saturating_add(1)
    }

    /// Returns the leaf that holds one row, if any does.
    fn find_leaf(&self, context: &mut Context<'_>, rowid: i64) -> DbResult<Option<i64>> {
        find_rowid(context, &self.shadows, &self.pending, rowid)
    }

    /// Inserts one cell, splitting nodes upwards as they fill.
    fn insert_cell(&mut self, context: &mut Context<'_>, cell: &Cell) -> DbResult<()> {
        let node_bytes = node_size(context, &self.shadows, &self.pending, self.cell_size());
        let capacity = self.cells_per_node(node_bytes);
        let leaf = self.choose_leaf(context, cell, node_bytes)?;
        self.add_to_node(context, leaf, cell, node_bytes, capacity, true)
    }

    /// Descends to the leaf a cell belongs in, growing boxes on the way.
    fn choose_leaf(
        &self,
        context: &mut Context<'_>,
        cell: &Cell,
        node_bytes: usize,
    ) -> DbResult<i64> {
        let mut number = ROOT;
        loop {
            let Some(node) = get_node(context, &self.shadows, &self.pending, number)? else {
                return Ok(ROOT);
            };
            let (depth, _) = read_header(&node);
            if depth == 0 {
                return Ok(number);
            }
            let cells = read_cells(&node, self.dimensions);
            let mut best: Option<(f64, i64)> = None;
            for child in &cells {
                let growth = child.growth(cell);
                if best.is_none_or(|(current, _)| growth < current) {
                    best = Some((growth, child.key));
                }
            }
            let Some((_, chosen)) = best else {
                return Ok(number);
            };
            // The box on the way down grows to hold what is about to be added,
            // which is what keeps a descent for the *next* insert correct.
            let mut cells = cells;
            for child in cells.iter_mut() {
                if child.key == chosen {
                    child.absorb(cell);
                }
            }
            let mut node = node;
            write_cells(&mut node, depth, &cells);
            put_node(context, &self.shadows, &self.pending, number, &node)?;
            number = chosen;
            let _ = node_bytes;
        }
    }

    /// Adds a cell to a node, splitting it and its parents where they fill.
    fn add_to_node(
        &mut self,
        context: &mut Context<'_>,
        number: i64,
        cell: &Cell,
        node_bytes: usize,
        capacity: usize,
        leaf: bool,
    ) -> DbResult<()> {
        let Some(mut node) = get_node(context, &self.shadows, &self.pending, number)? else {
            return Err(failure(format!("rtree node {number} is missing")));
        };
        let (depth, _) = read_header(&node);
        let mut cells = read_cells(&node, self.dimensions);
        cells.push(cell.clone());
        if leaf {
            put_rowid(context, &self.shadows, &self.pending, cell.key, number)?;
        } else {
            put_parent(context, &self.shadows, &self.pending, cell.key, number)?;
        }
        if cells.len() <= capacity {
            write_cells(&mut node, depth, &cells);
            return put_node(context, &self.shadows, &self.pending, number, &node);
        }
        // The node is full. Split it in half along the widest dimension, which
        // is the cheap version of Guttman's quadratic split and is what keeps
        // the boxes from degenerating into one long strip.
        let (left, right) = split(&cells, self.dimensions);
        let sibling = max_node(context, &self.shadows, &self.pending).saturating_add(1);
        let mut left_node = vec![0u8; node_bytes];
        write_cells(&mut left_node, depth, &left);
        put_node(context, &self.shadows, &self.pending, number, &left_node)?;
        let mut right_node = vec![0u8; node_bytes];
        write_cells(&mut right_node, depth, &right);
        put_node(context, &self.shadows, &self.pending, sibling, &right_node)?;
        for moved in &right {
            if depth == 0 {
                put_rowid(context, &self.shadows, &self.pending, moved.key, sibling)?;
            } else {
                put_parent(context, &self.shadows, &self.pending, moved.key, sibling)?;
            }
        }
        let left_box = bounding(&left);
        let right_box = bounding(&right);
        if number == ROOT {
            // The root splits *downwards*: it keeps its number, because the
            // number is the tree's name, and the two halves become its
            // children. Everything else grows the tree by one level.
            let first = max_node(context, &self.shadows, &self.pending).saturating_add(1);
            let mut moved = vec![0u8; node_bytes];
            write_cells(&mut moved, depth, &left);
            put_node(context, &self.shadows, &self.pending, first, &moved)?;
            for cell in &left {
                if depth == 0 {
                    put_rowid(context, &self.shadows, &self.pending, cell.key, first)?;
                } else {
                    put_parent(context, &self.shadows, &self.pending, cell.key, first)?;
                }
            }
            let mut root = vec![0u8; node_bytes];
            write_cells(
                &mut root,
                depth.saturating_add(1),
                &[
                    Cell {
                        key: first,
                        box_: left_box,
                    },
                    Cell {
                        key: sibling,
                        box_: right_box,
                    },
                ],
            );
            put_node(context, &self.shadows, &self.pending, ROOT, &root)?;
            put_parent(context, &self.shadows, &self.pending, first, ROOT)?;
            put_parent(context, &self.shadows, &self.pending, sibling, ROOT)?;
            return Ok(());
        }
        let parent = find_parent(context, &self.shadows, &self.pending, number)?.unwrap_or(ROOT);
        self.refit(context, parent, number, left_box)?;
        self.add_to_node(
            context,
            parent,
            &Cell {
                key: sibling,
                box_: right_box,
            },
            node_bytes,
            capacity,
            false,
        )
    }

    /// Replaces one child's box in its parent.
    fn refit(
        &self,
        context: &mut Context<'_>,
        parent: i64,
        child: i64,
        box_: Vec<f64>,
    ) -> DbResult<()> {
        let Some(mut node) = get_node(context, &self.shadows, &self.pending, parent)? else {
            return Ok(());
        };
        let (depth, _) = read_header(&node);
        let mut cells = read_cells(&node, self.dimensions);
        for cell in cells.iter_mut() {
            if cell.key == child {
                cell.box_ = box_.clone();
            }
        }
        write_cells(&mut node, depth, &cells);
        put_node(context, &self.shadows, &self.pending, parent, &node)
    }

    /// Removes one row, leaving the tree readable however empty a node becomes.
    ///
    /// An underfull node is left alone rather than merged. SQLite reinserts the
    /// orphans; leaving them costs a little space and no correctness, and the
    /// alternative is a rebalance that has to be crash-safe.
    fn remove(&mut self, context: &mut Context<'_>, rowid: i64) -> DbResult<()> {
        let Some(leaf) = find_rowid(context, &self.shadows, &self.pending, rowid)? else {
            return Ok(());
        };
        let Some(mut node) = get_node(context, &self.shadows, &self.pending, leaf)? else {
            return Ok(());
        };
        let (depth, _) = read_header(&node);
        let mut cells = read_cells(&node, self.dimensions);
        cells.retain(|cell| cell.key != rowid);
        // The blob is rewritten whole, so a cell that was removed leaves no
        // stale bytes behind for a reader that trusts the count.
        for byte in node.iter_mut().skip(HEADER) {
            *byte = 0;
        }
        write_cells(&mut node, depth, &cells);
        put_node(context, &self.shadows, &self.pending, leaf, &node)?;
        delete_rowid(context, &self.shadows, &self.pending, rowid)?;
        // The boxes above the leaf are now larger than they need to be, which
        // costs a wider search and never a wrong answer.
        Ok(())
    }
}

/// Returns the box that holds every cell of a set.
fn bounding(cells: &[Cell]) -> Vec<f64> {
    let Some(first) = cells.first() else {
        return Vec::new();
    };
    let mut box_ = first.box_.clone();
    for cell in cells.iter().skip(1) {
        for (mine, theirs) in box_.chunks_exact_mut(2).zip(cell.box_.chunks_exact(2)) {
            let (low, high) = (theirs.first().copied(), theirs.get(1).copied());
            if let (Some(slot), Some(value)) = (mine.first_mut(), low) {
                *slot = slot.min(value);
            }
            if let (Some(slot), Some(value)) = (mine.get_mut(1), high) {
                *slot = slot.max(value);
            }
        }
    }
    box_
}

/// Splits a full node's cells in two along the widest dimension.
fn split(cells: &[Cell], dimensions: usize) -> (Vec<Cell>, Vec<Cell>) {
    let mut widest = 0usize;
    let mut spread = f64::MIN;
    for dimension in 0..dimensions {
        let mut low = f64::MAX;
        let mut high = f64::MIN;
        for cell in cells {
            low = low.min(cell.box_.get(dimension * 2).copied().unwrap_or(f64::MAX));
            high = high.max(
                cell.box_
                    .get(dimension * 2 + 1)
                    .copied()
                    .unwrap_or(f64::MIN),
            );
        }
        if high - low > spread {
            spread = high - low;
            widest = dimension;
        }
    }
    let mut sorted = cells.to_vec();
    let midpoint = |cell: &Cell| -> f64 {
        let low = cell.box_.get(widest * 2).copied().unwrap_or(0.0);
        let high = cell.box_.get(widest * 2 + 1).copied().unwrap_or(0.0);
        low + high
    };
    sorted.sort_by(|left, right| {
        midpoint(left)
            .partial_cmp(&midpoint(right))
            .unwrap_or(core::cmp::Ordering::Equal)
    });
    let half = sorted.len().div_ceil(2);
    let right = sorted.split_off(half);
    (sorted, right)
}

/// Which end of a bounding box a coordinate is.
///
/// It decides which way a value is rounded when it will not fit in the
/// thirty-two bits the format stores, and that is not a detail: a box is a
/// promise that everything inside it is inside it, so a minimum has to round
/// *down* and a maximum *up*. Rounding both to nearest shrinks the box, and a
/// shrunken box loses rows - the query walks past a subtree whose stored bounds
/// no longer contain the row it holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Edge {
    /// The lower bound of a dimension, which rounds towards minus infinity.
    Low,
    /// The upper bound, which rounds towards plus infinity.
    High,
    /// A value being compared rather than stored, which rounds to nearest.
    Probe,
}

/// The factor a coordinate that has to grow is multiplied by.
///
/// One part in 2^23, which is the width of a 32-bit float's mantissa. SQLite
/// calls these `RNDAWAY` and `RNDTOWARDS` and applies them to the *double*
/// before the conversion, so the result is not always the next representable
/// float - at some magnitudes the product lands two steps away. Matching the
/// arithmetic rather than the intent is what makes the stored bytes identical.
const ROUND_AWAY: f64 = 1.0 + 1.0 / 8_388_608.0;

/// The factor a coordinate that has to shrink is multiplied by.
const ROUND_TOWARDS: f64 = 1.0 - 1.0 / 8_388_608.0;

/// Returns one coordinate as the module reads it.
///
/// A stored box is a promise that everything inside it is inside it, so a
/// minimum rounds *down* and a maximum *up* when the value will not fit in the
/// thirty-two bits the format holds. Rounding both to nearest shrinks the box,
/// and a shrunken box loses rows: the query walks past a subtree whose stored
/// bounds no longer contain the row it holds.
///
/// This was found by the performance scorecard rather than by reading. Both
/// engines inserted the same boxes and then disagreed about how many a range
/// held, because a maximum of 1103528600 was being stored here as 1103528576 -
/// a number smaller than the value it claims to bound.
/// @param value - the bound value, when there is one
/// @param coordinates - whether the table stores integers or floats
/// @param edge - which end of the box this is
fn coordinate(value: Option<&Value<'static>>, coordinates: Coordinates, edge: Edge) -> f64 {
    let Some(value) = value else {
        return 0.0;
    };
    let number = match value {
        Value::Integer(number) => *number as f64,
        Value::Real(number) => *number,
        Value::Text(text) => String::from_utf8_lossy(&text.utf8_bytes())
            .trim()
            .parse()
            .unwrap_or(0.0),
        _ => 0.0,
    };
    if coordinates == Coordinates::Integer {
        return number.round();
    }
    // The format stores 32-bit coordinates, so the value the file can hold is
    // the value everything downstream compares against.
    let nearest = f64::from(number as f32);
    match edge {
        Edge::Probe => nearest,
        Edge::Low if nearest > number => {
            let factor = if number < 0.0 {
                ROUND_AWAY
            } else {
                ROUND_TOWARDS
            };
            f64::from((number * factor) as f32)
        }
        Edge::High if nearest < number => {
            let factor = if number < 0.0 {
                ROUND_TOWARDS
            } else {
                ROUND_AWAY
            };
            f64::from((number * factor) as f32)
        }
        _ => nearest,
    }
}

/// A cursor over the rows one query matched.
struct RTreeCursor {
    /// Whether the rows are polygons rather than boxes.
    shape: bool,
    coordinates: Coordinates,
    dimensions: usize,
    shadows: ShadowTables,
    /// The table's write buffer, shared rather than copied.
    ///
    /// A `SELECT` in the transaction that wrote has to see what was written,
    /// and the writes are in the buffer until `sync`.
    pending: Buffer,
    rows: Vec<Cell>,
    at: usize,
}

impl VirtualCursor for RTreeCursor {
    /// Walks the tree, keeping the leaves the query's box reaches.
    fn filter(&mut self, context: &mut Context<'_>, plan: &FilterPlan) -> DbResult<()> {
        self.rows.clear();
        self.at = 0;
        if plan.index_number == PLAN_ROWID {
            let Some(rowid) = plan.arguments.first().and_then(Value::as_integer) else {
                return Ok(());
            };
            let Some(leaf) = find_rowid(context, &self.shadows, &self.pending, rowid)? else {
                return Ok(());
            };
            let Some(node) = get_node(context, &self.shadows, &self.pending, leaf)? else {
                return Ok(());
            };
            for cell in read_cells(&node, self.dimensions) {
                if cell.key == rowid {
                    self.rows.push(cell);
                }
            }
            return Ok(());
        }
        // The plan string is one operator and one column per constraint, in the
        // order the arguments arrive. It is turned into a box the walk can
        // compare against: a `>` on a maximum bounds the search from below, and
        // a `<` on a minimum bounds it from above.
        let mut low = vec![f64::NEG_INFINITY; self.dimensions];
        let mut high = vec![f64::INFINITY; self.dimensions];
        let mut arguments = plan.arguments.iter();
        let bytes = plan.index_string.as_bytes();
        for pair in bytes.chunks(2) {
            let (Some(op), Some(column)) = (pair.first(), pair.get(1)) else {
                continue;
            };
            let Some(value) = arguments.next() else {
                continue;
            };
            let value = coordinate(Some(value), self.coordinates, Edge::Probe);
            let column = usize::from(column.saturating_sub(b'0'));
            if column == 0 || column > self.dimensions * 2 {
                continue;
            }
            let dimension = (column - 1) / 2;
            match op {
                b'=' => {
                    if let Some(slot) = low.get_mut(dimension) {
                        *slot = slot.max(value);
                    }
                    if let Some(slot) = high.get_mut(dimension) {
                        *slot = slot.min(value);
                    }
                }
                b'>' | b'G' => {
                    if let Some(slot) = low.get_mut(dimension) {
                        *slot = slot.max(value);
                    }
                }
                b'<' | b'L' => {
                    if let Some(slot) = high.get_mut(dimension) {
                        *slot = slot.min(value);
                    }
                }
                _ => {}
            }
        }
        let mut query = Vec::with_capacity(self.dimensions * 2);
        for (low, high) in low.iter().zip(high.iter()).take(self.dimensions) {
            query.push(*low);
            query.push(*high);
        }
        let mut stack = vec![ROOT];
        while let Some(number) = stack.pop() {
            let Some(node) = get_node(context, &self.shadows, &self.pending, number)? else {
                continue;
            };
            let (depth, _) = read_header(&node);
            for cell in read_cells(&node, self.dimensions) {
                if plan.index_number == PLAN_BOX && !cell.overlaps(&query) {
                    continue;
                }
                if depth == 0 {
                    self.rows.push(cell);
                } else {
                    stack.push(cell.key);
                }
            }
        }
        self.rows.sort_by_key(|cell| cell.key);
        Ok(())
    }

    /// Moves to the next matching row.
    fn next(&mut self, _context: &mut Context<'_>) -> DbResult<()> {
        self.at = self.at.saturating_add(1);
        Ok(())
    }

    /// Returns whether the walk is finished.
    fn eof(&self) -> bool {
        self.at >= self.rows.len()
    }

    /// Returns the rowid, one coordinate, or one auxiliary value.
    ///
    /// An auxiliary column costs a read of the row's `%_rowid` entry, which the
    /// coordinates do not: they are in the cell the descent already loaded, and
    /// an auxiliary value is not. That is the trade the format makes - the tree
    /// stays as dense as it would be without them - and it is why a query is
    /// never planned on one.
    fn column(&mut self, context: &mut Context<'_>, index: usize) -> DbResult<Value<'static>> {
        let Some(cell) = self.rows.get(self.at) else {
            return Ok(Value::Null);
        };
        // A polygon table declares only its auxiliary columns, so every index
        // is one of them and the coordinates are not reachable at all.
        let coordinates = if self.shape {
            0
        } else {
            if index == 0 {
                return Ok(Value::Integer(cell.key));
            }
            self.dimensions.saturating_mul(2)
        };
        if self.shape {
            if let Ok(pending) = self.pending.lock() {
                if let Some(extra) = pending.auxiliary.get(&cell.key) {
                    return Ok(extra.get(index).cloned().unwrap_or(Value::Null));
                }
            }
            let Some(row) = self.shadows.read_row(context, b"rowid", cell.key)? else {
                return Ok(Value::Null);
            };
            return Ok(row
                .get(index.saturating_add(2))
                .cloned()
                .unwrap_or(Value::Null));
        }
        if index > coordinates {
            let at = index.saturating_sub(coordinates).saturating_add(1);
            if let Ok(pending) = self.pending.lock() {
                if let Some(extra) = pending.auxiliary.get(&cell.key) {
                    return Ok(extra
                        .get(index.saturating_sub(coordinates).saturating_sub(1))
                        .cloned()
                        .unwrap_or(Value::Null));
                }
            }
            let Some(row) = self.shadows.read_row(context, b"rowid", cell.key)? else {
                return Ok(Value::Null);
            };
            return Ok(row.get(at).cloned().unwrap_or(Value::Null));
        }
        let Some(value) = cell.box_.get(index.saturating_sub(1)) else {
            return Ok(Value::Null);
        };
        if self.coordinates == Coordinates::Integer {
            return Ok(Value::Integer(*value as i64));
        }
        Ok(Value::Real(*value))
    }

    /// Returns the row's rowid.
    fn rowid(&self) -> DbResult<i64> {
        Ok(self.rows.get(self.at).map(|cell| cell.key).unwrap_or(0))
    }
}

/// Reads one node's blob, from the buffer when the buffer has it.
fn get_node(
    context: &mut Context<'_>,
    shadows: &ShadowTables,
    pending: &Buffer,
    number: i64,
) -> DbResult<Option<Vec<u8>>> {
    if let Ok(held) = pending.lock() {
        if let Some(node) = held.nodes.get(&number) {
            return Ok(node.clone());
        }
    }
    let node = match shadows.read_row(context, b"node", number)? {
        Some(row) => row.get(1).and_then(|value| match value {
            Value::Blob(blob) => Some(blob.raw().to_vec()),
            _ => None,
        }),
        None => None,
    };
    if let Ok(mut held) = pending.lock() {
        held.nodes.insert(number, node.clone());
    }
    Ok(node)
}

/// Buffers one node's blob.
fn put_node(
    _context: &mut Context<'_>,
    _shadows: &ShadowTables,
    pending: &Buffer,
    number: i64,
    data: &[u8],
) -> DbResult<()> {
    let Ok(mut held) = pending.lock() else {
        return Err(inillucent_base::error::misuse(
            "the r-tree buffer is poisoned",
        ));
    };
    held.nodes.insert(number, Some(data.to_vec()));
    held.dirty_nodes.insert(number);
    held.highest_node = Some(held.highest_node.unwrap_or(ROOT).max(number));
    Ok(())
}

/// Returns the leaf that holds one row.
fn find_rowid(
    context: &mut Context<'_>,
    shadows: &ShadowTables,
    pending: &Buffer,
    rowid: i64,
) -> DbResult<Option<i64>> {
    if let Ok(held) = pending.lock() {
        if let Some(node) = held.rowids.get(&rowid) {
            return Ok(*node);
        }
    }
    let node = match shadows.read_row(context, b"rowid", rowid)? {
        Some(row) => row.get(1).and_then(Value::as_integer),
        None => None,
    };
    if let Ok(mut held) = pending.lock() {
        held.rowids.insert(rowid, node);
    }
    Ok(node)
}

/// Buffers which leaf holds one row.
fn put_rowid(
    _context: &mut Context<'_>,
    _shadows: &ShadowTables,
    pending: &Buffer,
    rowid: i64,
    node: i64,
) -> DbResult<()> {
    let Ok(mut held) = pending.lock() else {
        return Err(inillucent_base::error::misuse(
            "the r-tree buffer is poisoned",
        ));
    };
    held.rowids.insert(rowid, Some(node));
    held.dirty_rowids.insert(rowid);
    held.highest_rowid = Some(held.highest_rowid.unwrap_or(0).max(rowid));
    Ok(())
}

/// Buffers the removal of one row's leaf entry.
fn delete_rowid(
    _context: &mut Context<'_>,
    _shadows: &ShadowTables,
    pending: &Buffer,
    rowid: i64,
) -> DbResult<()> {
    let Ok(mut held) = pending.lock() else {
        return Err(inillucent_base::error::misuse(
            "the r-tree buffer is poisoned",
        ));
    };
    held.rowids.insert(rowid, None);
    held.dirty_rowids.insert(rowid);
    Ok(())
}

/// Returns which node holds one node.
fn find_parent(
    context: &mut Context<'_>,
    shadows: &ShadowTables,
    pending: &Buffer,
    node: i64,
) -> DbResult<Option<i64>> {
    if let Ok(held) = pending.lock() {
        if let Some(parent) = held.parents.get(&node) {
            return Ok(*parent);
        }
    }
    let parent = match shadows.read_row(context, b"parent", node)? {
        Some(row) => row.get(1).and_then(Value::as_integer),
        None => None,
    };
    if let Ok(mut held) = pending.lock() {
        held.parents.insert(node, parent);
    }
    Ok(parent)
}

/// Buffers which node holds one node.
fn put_parent(
    _context: &mut Context<'_>,
    _shadows: &ShadowTables,
    pending: &Buffer,
    node: i64,
    parent: i64,
) -> DbResult<()> {
    let Ok(mut held) = pending.lock() else {
        return Err(inillucent_base::error::misuse(
            "the r-tree buffer is poisoned",
        ));
    };
    held.parents.insert(node, Some(parent));
    held.dirty_parents.insert(node);
    Ok(())
}

/// Returns the largest node number in use, buffered ones included.
fn max_node(context: &mut Context<'_>, shadows: &ShadowTables, pending: &Buffer) -> i64 {
    let stored = shadows.max_rowid(context, b"node").unwrap_or(ROOT);
    match pending.lock() {
        Ok(held) => stored.max(held.highest_node.unwrap_or(ROOT)),
        Err(_) => stored,
    }
}

/// Returns the largest rowid in use, buffered ones included.
fn max_rowid(context: &mut Context<'_>, shadows: &ShadowTables, pending: &Buffer) -> i64 {
    let stored = shadows.max_rowid(context, b"rowid").unwrap_or(0);
    match pending.lock() {
        Ok(held) => stored.max(held.highest_rowid.unwrap_or(0)),
        Err(_) => stored,
    }
}

/// Returns the error a coordinate out of range reports.
#[allow(dead_code)]
fn out_of_range() -> inillucent_base::DbError {
    inillucent_base::DbError::primary(PrimaryCode::Constraint)
        .with_detail("rtree constraint failed: a coordinate is out of range")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A node round-trips through its blob, header and all.
    #[test]
    fn a_node_round_trips() {
        let mut node = vec![0u8; 4 + 3 * (8 + 4 * 4)];
        let cells = vec![
            Cell {
                key: 7,
                box_: vec![1.0, 2.0, 3.0, 4.0],
            },
            Cell {
                key: 9,
                box_: vec![-1.0, 0.5, 10.0, 20.0],
            },
        ];
        write_cells(&mut node, 0, &cells);
        assert_eq!(read_header(&node), (0, 2));
        assert_eq!(read_cells(&node, 2), cells);
    }

    /// The header carries the depth in the two bytes before the count.
    #[test]
    fn the_header_carries_the_depth() {
        let mut node = vec![0u8; 64];
        write_header(&mut node, 3, 5);
        assert_eq!(&node[0..4], &[0, 3, 0, 5]);
        assert_eq!(read_header(&node), (3, 5));
    }

    /// Two boxes overlap when they overlap in every dimension.
    #[test]
    fn overlap_is_per_dimension() {
        let cell = Cell {
            key: 1,
            box_: vec![0.0, 10.0, 0.0, 10.0],
        };
        assert!(cell.overlaps(&[5.0, 15.0, 5.0, 15.0]));
        assert!(!cell.overlaps(&[11.0, 15.0, 5.0, 15.0]));
        assert!(!cell.overlaps(&[5.0, 15.0, 11.0, 15.0]));
    }

    /// A split cuts along the widest dimension, which keeps the halves compact.
    #[test]
    fn a_split_cuts_the_widest_dimension() {
        let cells: Vec<Cell> = (0..4)
            .map(|index| Cell {
                key: index,
                box_: vec![index as f64 * 100.0, index as f64 * 100.0 + 1.0, 0.0, 1.0],
            })
            .collect();
        let (left, right) = split(&cells, 2);
        assert_eq!(left.len(), 2);
        assert_eq!(right.len(), 2);
        assert!(left.iter().all(|cell| cell.key < 2));
        assert!(right.iter().all(|cell| cell.key >= 2));
    }

    /// A table needs an odd number of columns between three and eleven.
    #[test]
    fn the_column_count_is_checked() {
        assert!(
            parse_coordinate_arguments(&[b"id".to_vec(), b"x0".to_vec(), b"x1".to_vec()]).is_ok()
        );
        assert!(parse_coordinate_arguments(&[b"id".to_vec(), b"x0".to_vec()]).is_err());
        assert!(parse_coordinate_arguments(&[b"id".to_vec()]).is_err());
    }

    /// A declared type on a column is dropped; only the name is kept.
    #[test]
    fn a_declared_type_is_dropped() {
        let shape = parse_coordinate_arguments(&[
            b"id".to_vec(),
            b"minX REAL".to_vec(),
            b"maxX REAL".to_vec(),
        ])
        .expect("parses");
        assert_eq!(shape.coordinates[1], b"minX");
        assert!(shape.auxiliary.is_empty());
    }

    /// A `+name` column is auxiliary and does not count towards the dimensions.
    ///
    /// The odd-number rule is about coordinates: `rtree(id, minX, maxX, +label)`
    /// is four arguments and one dimension, and counting the label would make
    /// it an even count and refuse a declaration SQLite accepts.
    #[test]
    fn a_plus_column_is_auxiliary_and_not_a_coordinate() {
        let shape = parse_coordinate_arguments(&[
            b"id".to_vec(),
            b"minX".to_vec(),
            b"maxX".to_vec(),
            b"+label".to_vec(),
            b"+note TEXT".to_vec(),
        ])
        .expect("parses");
        assert_eq!(shape.coordinates.len(), 3);
        assert_eq!(shape.auxiliary, vec![b"label".to_vec(), b"note".to_vec()]);
    }
}
