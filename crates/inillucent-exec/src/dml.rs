//! The write path: `INSERT`, `UPDATE` and `DELETE` over the paged trees.
//!
//! Invariant: a statement's reads all happen before any of its writes. Every
//! statement here decides which rows it is going to change, in full, and only
//! then changes them. That is not a shortcut around the push executor - it is
//! SQLite's own "collect the keys, then halt and modify" shape, and it is what
//! makes `UPDATE t SET k = k + 1 WHERE k < 5` terminate rather than meeting its
//! own new rows.
//!
//! It is also what the borrow checker asks for. Mutating a tree needs
//! `&mut PagedTree` *and* `&mut Database` at once, while reading needs a shared
//! borrow of both; a design that wrote as it scanned would have to thread a
//! mutable borrow of the whole file through every operator in [`crate::ops`].
//! Splitting the statement in two - [`keys_query`] under a shared borrow,
//! [`insert`], [`update`] and [`delete`] under a mutable one - keeps the read
//! half *exactly* the read path the gate measures, index selection and all.
//!
//! ## Expressions are the read path's expressions
//!
//! Nothing here evaluates a `BoundExpr`. A statement's `SET` values, its
//! `DEFAULT`s, its `VALUES` rows and its `excluded.*` references are all
//! translated by [`crate::physical`]'s own translator against a synthetic
//! column space and compiled by [`crate::expr::compile`], then run against a
//! one-row batch. A second evaluator here would agree with that one until the
//! first time somebody fixed an affinity rule in one of them - which is exactly
//! the bug the read path's own two-traversal translator had, by twenty-odd node
//! kinds.
//!
//! The synthetic space is what makes `excluded.body` and a trigger's `OLD.x`
//! fall out for free: the binder gives them source numbers no FROM term can
//! have ([`inillucent_sql::bind::EXCLUDED_SOURCE`] and its neighbours), so they
//! are simply further stages of the space, reading further rows of the batch.
//!
//! ## The order one row is written in
//!
//! For every changed row, in this order and for a reason each:
//!
//! 1. **Uniqueness is checked before anything is written**, through a point
//!    probe of the table key and of each unique index. A constraint checked
//!    afterwards is a constraint that has already corrupted the tree it was
//!    protecting.
//! 2. **The old index entries are removed before the new ones are added**, so
//!    an update that leaves an indexed column alone removes and re-adds the
//!    same entry rather than leaving two of it.
//! 3. **The table row goes last.** Either order is recoverable - the whole
//!    statement is one transaction and its records replay together - but doing
//!    the indexes first means a failure part-way leaves an index entry pointing
//!    at a row that is not there, which the integrity checker names, rather
//!    than a row no index can find, which it does not.

use std::collections::{BTreeMap, HashMap};

use inillucent_base::error::misuse;
use inillucent_base::{DbError, DbResult, ExtendedCode};
use inillucent_pool::{Database, Pool};
use inillucent_sql::ast::{ConflictAction, JoinKind as SqlJoinKind};
use inillucent_sql::bind::{
    BoundExpr, BoundResultColumn, BoundSelect, BoundSource, SourceRows, EXCLUDED_SOURCE,
    NEW_SOURCE, OLD_SOURCE,
};
use inillucent_sql::catalog_view::{IndexInfo, IndexOrigin, TableInfo};
use inillucent_sql::dml::{
    codes, rowid_message, unique_message, BoundDelete, BoundInsert, BoundInsertSource, BoundUpdate,
    ColumnSource,
};
use inillucent_tree::datum::{Datum, OwnedDatum};
use inillucent_tree::leaf::LeafRef;
use inillucent_tree::write::TreeLog;
use inillucent_tree::PagedTree;
use inillucent_value::collation::Collation;

use crate::batch::{Batch, Vector};
use crate::expr::{compile, Eval};
use crate::physical::{translate_scan, AccessKind, HeldSpace, Params, PreparedStage, SourceLayout};

/// One row in a tree's own column order.
///
/// A table row is `[rowid] ++ [record slots except the rowid alias]`, which is
/// what [`SourceLayout`] describes; an index entry is the indexed columns
/// followed by the rowid. Both are this type, because both are just a tree's
/// row, and the write path never holds a row in any other shape.
pub type Row = Vec<OwnedDatum>;

/// What a data-modifying statement did.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Changes {
    /// How many rows the statement inserted, updated or deleted.
    pub rows: usize,
    /// The rows `RETURNING` asked for, when the statement asked for any.
    pub returned: Vec<Row>,
}

/// A map from root page to tree, whichever map the caller happens to hold.
///
/// The write path needs a mutable tree and a mutable [`Database`] at the same
/// instant, which no single accessor can hand out. [`WriteTarget::parts`]
/// therefore hands out both at once, and this is the tree half of that pair -
/// a trait rather than a concrete map so the harness's `HashMap` and a test's
/// `BTreeMap` are both usable without either being the one true type.
pub trait Trees {
    /// Returns the tree at a root page.
    ///
    /// @param root - the root page id the catalog names the tree by
    fn get(&self, root: u32) -> Option<&PagedTree>;

    /// Returns the tree at a root page, mutably.
    ///
    /// @param root - the root page id the catalog names the tree by
    fn get_mut(&mut self, root: u32) -> Option<&mut PagedTree>;
}

impl Trees for HashMap<u32, PagedTree> {
    fn get(&self, root: u32) -> Option<&PagedTree> {
        HashMap::get(self, &root)
    }

    fn get_mut(&mut self, root: u32) -> Option<&mut PagedTree> {
        HashMap::get_mut(self, &root)
    }
}

impl Trees for BTreeMap<u32, PagedTree> {
    fn get(&self, root: u32) -> Option<&PagedTree> {
        BTreeMap::get(self, &root)
    }

    fn get_mut(&mut self, root: u32) -> Option<&mut PagedTree> {
        BTreeMap::get_mut(self, &root)
    }
}

/// Everything a write needs that a read does not.
pub trait WriteTarget {
    /// Returns the file and its trees, both mutably and at the same time.
    fn parts(&mut self) -> (&mut Database, &mut dyn Trees);

    /// Returns a tree's layout: which record slot each tree column holds.
    ///
    /// @param root - the root page id the catalog names the tree by
    fn layout(&self, root: u32) -> Option<&SourceLayout>;
}

/// The synthetic column space a write's expressions are translated against.
///
/// A statement's stages are not FROM terms here - they are *row images*: the
/// row being written, the row already there, the `excluded` row of an upsert, a
/// trigger's `OLD` and `NEW`. Each is one stage of the same table's layout at
/// its own offset, so [`translate_scan`] resolves a bound column of any of them
/// with no special case at all.
pub struct RowSpace {
    /// One stage per row image, in the order the batch concatenates them.
    stages: Vec<PreparedStage>,
    /// The layouts and static types those stages define.
    held: HeldSpace,
    /// How many columns one image holds.
    width: usize,
    /// Which column of an image holds its rowid.
    rowid: Option<usize>,
}

impl RowSpace {
    /// Builds a space over one or more row images of the same layout.
    ///
    /// @param sources - the binder source number each image answers to
    /// @param layout - the table tree's layout, shared by every image
    pub fn new(sources: &[usize], layout: &SourceLayout) -> RowSpace {
        let mut stages = Vec::with_capacity(sources.len());
        let mut layouts = Vec::with_capacity(sources.len());
        let mut types = Vec::with_capacity(sources.len().saturating_mul(layout.width));
        for (term, source) in sources.iter().enumerate() {
            stages.push(PreparedStage {
                root: layout.tree_key,
                // These stages read no tree: they are row images the caller
                // already holds. `Materialised` is the kind that says so.
                kind: AccessKind::Materialised,
                source: *source,
                term,
                is_lookup: false,
                offset: term.saturating_mul(layout.width),
                width: layout.width,
                layout: Some(layout.clone()),
            });
            layouts.push(layout.clone());
            types.extend(layout.types.iter().copied());
        }
        RowSpace {
            stages,
            held: HeldSpace {
                layouts,
                types,
                order: Vec::new(),
            },
            width: layout.width,
            rowid: layout.rowid,
        }
    }

    /// Compiles one bound expression against this space.
    ///
    /// @param expr - the bound expression
    /// @param params - the bound parameters
    pub fn compile(&self, expr: &BoundExpr, params: &Params) -> DbResult<Box<dyn Eval>> {
        let space = self.held.view(&self.stages);
        let translated = translate_scan(expr, &space, params)?;
        compile(&translated, &self.held.types)
    }

    /// Evaluates a compiled expression against the row images, in stage order.
    ///
    /// A missing image reads as a row of NULLs rather than as a short batch: an
    /// expression compiled against three stages may only reference one of them,
    /// and a batch narrower than the space would resolve its column indexes to
    /// nothing.
    ///
    /// @param eval - the compiled expression
    /// @param images - one row per stage, concatenated into a one-row batch
    pub fn evaluate(&self, eval: &dyn Eval, images: &[&[OwnedDatum]]) -> DbResult<OwnedDatum> {
        let mut cells: Vec<[Datum<'_>; 1]> =
            Vec::with_capacity(self.stages.len().saturating_mul(self.width));
        for stage in 0..self.stages.len() {
            let image = images.get(stage).copied().unwrap_or(&[]);
            for column in 0..self.width {
                cells.push([image
                    .get(column)
                    .map(OwnedDatum::borrow)
                    .unwrap_or(Datum::Null)]);
            }
        }
        let vectors: Vec<Vector<'_>> = cells
            .iter()
            .map(|cell| Vector::Values(cell.as_slice()))
            .collect();
        let batch = Batch::new(1, vectors);
        Ok(OwnedDatum::from_datum(&eval.value(&batch, 0)?.get()))
    }
}

/// Builds the query that finds the rows an `UPDATE` or `DELETE` will change.
///
/// The statement's own `WHERE`, over the statement's own table, projecting the
/// table's key. Running it through the ordinary planner is the whole point:
/// `UPDATE main_table SET key = key + 1 WHERE id = ?1` has to reach the same
/// point probe a `SELECT` with that `WHERE` would, or the write path is a full
/// scan per statement wearing an index's clothes.
///
/// @param table - the table being written
/// @param source - the statement-wide number of its FROM term
/// @param filter - the statement's `WHERE`, when it wrote one
/// @param limit - the statement's `LIMIT`
/// @param offset - the statement's `OFFSET`
/// @param layout - the table tree's layout, for which columns are the key
pub fn keys_query(
    table: &TableInfo,
    source: usize,
    filter: Option<&BoundExpr>,
    limit: Option<&BoundExpr>,
    offset: Option<&BoundExpr>,
    layout: &SourceLayout,
) -> DbResult<BoundSelect> {
    let columns = key_columns(table, source, layout)?
        .into_iter()
        .map(|expr| BoundResultColumn {
            expr,
            name: b"key".to_vec(),
            origin: None,
            declared_type: Vec::new(),
        })
        .collect();
    Ok(BoundSelect {
        sources: vec![BoundSource {
            id: source,
            rows: SourceRows::Table,
            table: table.clone(),
            alias: table.name.clone(),
            join: SqlJoinKind::Inner,
            constraint: None,
            suppressed: Vec::new(),
        }],
        filter: filter.cloned(),
        group_by: Vec::new(),
        having: None,
        columns,
        distinct: false,
        order_by: Vec::new(),
        limit: limit.cloned(),
        offset: offset.cloned(),
        aggregates: Vec::new(),
        values: Vec::new(),
        compounds: Vec::new(),
        windows: Vec::new(),
        correlations: Vec::new(),
    })
}

/// Returns the expressions that read a table's key, in key order.
///
/// A rowid table's key is its rowid, and reading it *as* a rowid is what lets
/// the planner recognise `WHERE id = ?1` as a point probe. Anything else - a
/// `WITHOUT ROWID` table - has a key of ordinary columns, which the layout
/// names by tree column and the binder names by record slot.
///
/// @param table - the table being written
/// @param source - the statement-wide number of its FROM term
/// @param layout - the table tree's layout
fn key_columns(
    table: &TableInfo,
    source: usize,
    layout: &SourceLayout,
) -> DbResult<Vec<BoundExpr>> {
    if layout.key_columns.len() == 1 && layout.key_columns.first().copied() == layout.rowid {
        return Ok(vec![BoundExpr::Rowid { source }]);
    }
    let mut exprs = Vec::with_capacity(layout.key_columns.len());
    for tree_column in &layout.key_columns {
        let slot = layout
            .slots
            .iter()
            .position(|held| *held == Some(*tree_column))
            .ok_or_else(|| {
                misuse(format!(
                    "key column {tree_column} of {} is in no record slot",
                    String::from_utf8_lossy(&table.name)
                ))
            })?;
        let (affinity, collation) = declared(table, slot);
        exprs.push(BoundExpr::Column {
            source,
            column: u16::try_from(slot).unwrap_or_default(),
            slot: u16::try_from(slot).unwrap_or_default(),
            affinity,
            collation,
        });
    }
    Ok(exprs)
}

/// Returns a column's declared affinity and collation.
///
/// @param table - the table
/// @param column - the column's declared position
fn declared(table: &TableInfo, column: usize) -> (inillucent_value::Affinity, Collation) {
    match table.columns.get(column) {
        Some(info) => (
            info.affinity,
            Collation::from_name(core::str::from_utf8(&info.collation).unwrap_or("BINARY"))
                .unwrap_or(Collation::Binary),
        ),
        None => (inillucent_value::Affinity::Blob, Collation::Binary),
    }
}

/// Returns the row images a statement's expressions can reach.
///
/// The target row always, and `OLD` and `NEW` only when a trigger could name
/// them. Each image is a cloned layout and a run of batch columns paid for on
/// every execution, so carrying one the statement cannot mention is pure cost.
///
/// @param source - the statement-wide number of the target's FROM term
/// @param triggers - the triggers the write fires
fn sources_for(source: usize, triggers: &[inillucent_sql::dml::BoundTrigger]) -> Vec<usize> {
    if triggers.is_empty() {
        return vec![source];
    }
    vec![source, OLD_SOURCE, NEW_SOURCE]
}

/// Applies an `INSERT`.
///
/// @param statement - the bound insert
/// @param target - the file and its trees
/// @param log - where the records go
/// @param params - the bound parameters
/// @param supplied - the rows a `SELECT` source produced, empty for `VALUES`
pub fn insert(
    statement: &BoundInsert,
    target: &mut dyn WriteTarget,
    log: &mut dyn TreeLog,
    params: &Params,
    supplied: &[Row],
) -> DbResult<Changes> {
    let table = &statement.table;
    let layout = layout_of(target, table)?;
    // `excluded` only exists inside an `ON CONFLICT ... DO UPDATE`, so a plain
    // insert carries one image rather than two.
    let mut sources = vec![statement.target_source];
    if statement.upsert.is_some() {
        sources.push(EXCLUDED_SOURCE);
    }
    let space = RowSpace::new(&sources, &layout);
    let plan = InsertPlan::compile(statement, &layout, &space, params)?;

    let rows: Vec<Row> = match &statement.source {
        BoundInsertSource::Values(values) => {
            let mut built = Vec::with_capacity(values.len());
            for row in values {
                let mut cells = Vec::with_capacity(row.len());
                for expr in row {
                    let eval = space.compile(expr, params)?;
                    cells.push(space.evaluate(eval.as_ref(), &[])?);
                }
                built.push(cells);
            }
            built
        }
        BoundInsertSource::Select(_) => supplied.to_vec(),
    };

    // **Found on demand, not up front.** Reading the largest rowid costs a
    // descent, and a statement that supplies its own key needs none - which is
    // every `INSERT INTO t(id, ...) VALUES (?1, ...)`, the shape the gate's
    // `write.insert.batch` measures. `None` here means "not asked yet".
    let mut next_rowid: Option<i64> = None;
    let mut changes = Changes::default();
    for supplied_row in &rows {
        let image = plan.build_row(supplied_row, &space, &mut next_rowid, || {
            highest_rowid(&mut Borrowed(target), table)
        })?;
        let Some(stored) = write_one(statement, &layout, &space, &plan, target, log, image)? else {
            continue;
        };
        changes.rows = changes.rows.saturating_add(1);
        if !plan.returning.is_empty() {
            let mut out = Vec::with_capacity(plan.returning.len());
            for eval in &plan.returning {
                out.push(space.evaluate(eval.as_ref(), &[stored.as_slice()])?);
            }
            changes.returned.push(out);
        }
    }
    Ok(changes)
}

/// The expressions an `INSERT` evaluates, compiled once for the statement.
///
/// Compiling per row is what made the old engine's insert path build a closure
/// tree per row of a `VALUES` list. The expressions depend on the parameters
/// and not on the row, so they are compiled with the statement.
struct InsertPlan {
    /// For each table column, where its value comes from.
    columns: Vec<PlannedColumn>,
    /// Where the rowid comes from.
    rowid: Option<PlannedRowid>,
    /// The `DO UPDATE` assignments, by tree column.
    upsert: Vec<(usize, Box<dyn Eval>)>,
    /// The `RETURNING` expressions.
    returning: Vec<Box<dyn Eval>>,
}

/// Where one table column's value comes from, resolved to a tree column.
struct PlannedColumn {
    /// Which tree column it lands in, when the tree carries it.
    slot: Option<usize>,
    /// How its value is produced.
    from: PlannedValue,
}

/// How one value is produced.
enum PlannedValue {
    /// Position in the source row the statement supplied.
    Supplied(usize),
    /// An expression that reads nothing, which is what a `DEFAULT` is.
    Constant(Box<dyn Eval>),
    /// An expression that reads the rest of the row, computed last.
    Generated(Box<dyn Eval>),
}

/// Where an `INSERT`'s rowid comes from.
enum PlannedRowid {
    /// Position in the supplied row.
    Supplied(usize),
    /// An expression.
    Expr(Box<dyn Eval>),
}

impl InsertPlan {
    /// Compiles every expression an insert evaluates.
    ///
    /// @param statement - the bound insert
    /// @param layout - the table tree's layout
    /// @param space - the row space the expressions read
    /// @param params - the bound parameters
    fn compile(
        statement: &BoundInsert,
        layout: &SourceLayout,
        space: &RowSpace,
        params: &Params,
    ) -> DbResult<InsertPlan> {
        let mut columns = Vec::with_capacity(statement.columns.len());
        for (column, source) in statement.columns.iter().enumerate() {
            let slot = layout.slots.get(column).copied().flatten();
            let from = match source {
                ColumnSource::Row(index) => PlannedValue::Supplied(*index),
                ColumnSource::Expr(expr) => PlannedValue::Constant(space.compile(expr, params)?),
                ColumnSource::Generated(expr) => {
                    PlannedValue::Generated(space.compile(expr, params)?)
                }
            };
            columns.push(PlannedColumn { slot, from });
        }
        let rowid = match (&statement.rowid, statement.named_rowid) {
            (Some(ColumnSource::Row(index)), _) => Some(PlannedRowid::Supplied(*index)),
            (Some(ColumnSource::Expr(expr) | ColumnSource::Generated(expr)), _) => {
                Some(PlannedRowid::Expr(space.compile(expr, params)?))
            }
            (None, Some(index)) => Some(PlannedRowid::Supplied(index)),
            (None, None) => None,
        };
        let mut upsert = Vec::new();
        if let Some(clause) = &statement.upsert {
            if clause.do_update {
                for assignment in &clause.assignments {
                    if let Some(slot) = layout
                        .slots
                        .get(usize::from(assignment.column))
                        .copied()
                        .flatten()
                    {
                        upsert.push((slot, space.compile(&assignment.value, params)?));
                    }
                }
            }
        }
        let mut returning = Vec::with_capacity(statement.returning.len());
        for column in &statement.returning {
            returning.push(space.compile(&column.expr, params)?);
        }
        Ok(InsertPlan {
            columns,
            rowid,
            upsert,
            returning,
        })
    }

    /// Builds one row image in tree-column order.
    ///
    /// The generated columns are computed in a second pass, because a generated
    /// column reads the row it is part of and the row is not a row until every
    /// supplied column is in it.
    ///
    /// @param supplied - the values the statement's source produced
    /// @param space - the row space the expressions read
    /// @param next_rowid - the largest rowid handed out so far, advanced here
    fn build_row(
        &self,
        supplied: &[OwnedDatum],
        space: &RowSpace,
        next_rowid: &mut Option<i64>,
        highest: impl FnOnce() -> DbResult<i64>,
    ) -> DbResult<Row> {
        let mut row: Row = vec![OwnedDatum::Null; space.width];
        for planned in &self.columns {
            let Some(slot) = planned.slot else { continue };
            let value = match &planned.from {
                PlannedValue::Supplied(index) => {
                    supplied.get(*index).cloned().unwrap_or(OwnedDatum::Null)
                }
                PlannedValue::Constant(eval) => space.evaluate(eval.as_ref(), &[])?,
                PlannedValue::Generated(_) => continue,
            };
            if let Some(cell) = row.get_mut(slot) {
                *cell = value;
            }
        }
        let supplied_key = match &self.rowid {
            Some(PlannedRowid::Supplied(index)) => {
                supplied.get(*index).cloned().unwrap_or(OwnedDatum::Null)
            }
            Some(PlannedRowid::Expr(eval)) => space.evaluate(eval.as_ref(), &[])?,
            None => OwnedDatum::Null,
        };
        if let Some(slot) = space.rowid {
            let rowid = match supplied_key {
                OwnedDatum::Int(number) => number,
                // A rowid the statement left out is one past the largest the
                // table holds, which is SQLite's rule for a table that is not
                // `AUTOINCREMENT`: deleted numbers are reused.
                OwnedDatum::Null => {
                    let held = match *next_rowid {
                        Some(held) => held,
                        None => highest()?,
                    };
                    let allocated = held.saturating_add(1);
                    *next_rowid = Some(allocated);
                    allocated
                }
                // `INSERT INTO t(rowid) VALUES ('x')` is a mismatch rather than
                // a conversion, which is what SQLite reports too.
                other => {
                    return Err(DbError::new(ExtendedCode(codes::MISMATCH))
                        .with_message("datatype mismatch")
                        .with_detail(format!("a rowid must be an integer, not {other:?}")))
                }
            };
            // A statement that supplies its own keys still moves the mark, so a
            // later row that supplies none does not collide with it.
            if let Some(held) = *next_rowid {
                *next_rowid = Some(held.max(rowid));
            }
            if let Some(cell) = row.get_mut(slot) {
                *cell = OwnedDatum::Int(rowid);
            }
        }
        // The generated columns, now that the rest of the row exists.
        for planned in &self.columns {
            let (Some(slot), PlannedValue::Generated(eval)) = (planned.slot, &planned.from) else {
                continue;
            };
            let value = space.evaluate(eval.as_ref(), &[row.as_slice()])?;
            if let Some(cell) = row.get_mut(slot) {
                *cell = value;
            }
        }
        Ok(row)
    }
}

/// Writes one already-built row, applying the conflict algorithm.
///
/// Returns the row as stored, or `None` when a conflict said to skip it.
///
/// @param statement - the bound insert
/// @param layout - the table tree's layout
/// @param space - the row space
/// @param plan - the compiled statement
/// @param target - the file and its trees
/// @param log - where the records go
/// @param row - the row image, in tree-column order
#[allow(clippy::too_many_arguments)]
fn write_one(
    statement: &BoundInsert,
    layout: &SourceLayout,
    space: &RowSpace,
    plan: &InsertPlan,
    target: &mut dyn WriteTarget,
    log: &mut dyn TreeLog,
    row: Row,
) -> DbResult<Option<Row>> {
    let table = &statement.table;
    // **The common insert asks the table once.**
    //
    // Every uniqueness check still happens before anything is written - a
    // constraint checked afterwards is one that has already corrupted the tree
    // it was protecting - but for the ordinary case the check and the write are
    // the same descent. `PagedTree::put_absent` finds the key, and if it is
    // there it stops without writing; the constraint message is then built from
    // a second probe, on the path that is about to fail anyway.
    //
    // It applies when the statement raises on a conflict and the table's only
    // uniqueness is its own key. A table with a secondary `UNIQUE` index needs
    // those probed too, and an `ON CONFLICT` clause needs to know *which* row
    // it collided with, so both take the general path below.
    if resolution(statement) == Resolution::Raise && unique_indexes(table).next().is_none() {
        if place_row_absent(table, layout, target, log, &row)? {
            return Ok(Some(row));
        }
        return Err(conflicting_row(table, layout, target, &row)?
            .map(|clash| clash.error)
            .unwrap_or_else(|| {
                let (code, message) = rowid_message(table);
                DbError::new(ExtendedCode(code)).with_message(message)
            }));
    }
    if let Some(clash) = conflicting_row(table, layout, target, &row)? {
        match resolution(statement) {
            Resolution::Skip => return Ok(None),
            Resolution::Replace => {
                let Some(held) = read_row(table, target, &clash.key)? else {
                    return Ok(None);
                };
                remove_row(table, layout, target, log, &clash.key, &held)?;
            }
            Resolution::Update => {
                let updated = upsert_row(
                    statement, table, layout, space, plan, target, log, &clash, &row,
                )?;
                return Ok(Some(updated));
            }
            // `ABORT`, `FAIL` and `ROLLBACK` all raise here and differ only in
            // what the *session* undoes, which this layer does not own.
            Resolution::Raise => return Err(clash.error),
        }
    }
    place_row(table, layout, target, log, None, &row)?;
    Ok(Some(row))
}

/// What an insert does about a row that collides with one already there.
///
/// An `ON CONFLICT ... DO UPDATE` clause beats the statement's own `OR`
/// algorithm, which is SQLite's rule: `INSERT OR IGNORE ... ON CONFLICT DO
/// UPDATE` updates rather than ignoring, because the clause is attached to the
/// constraint and the algorithm is only the fallback.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Resolution {
    /// Abandon the row and carry on with the next one.
    Skip,
    /// Delete the row already there and write this one.
    Replace,
    /// Apply the upsert's assignments to the row already there.
    Update,
    /// Report the constraint failure.
    Raise,
}

/// Returns what an insert does about a conflict.
///
/// @param statement - the bound insert
fn resolution(statement: &BoundInsert) -> Resolution {
    if let Some(clause) = &statement.upsert {
        return if clause.do_update {
            Resolution::Update
        } else {
            Resolution::Skip
        };
    }
    match statement.on_conflict {
        Some(ConflictAction::Ignore) => Resolution::Skip,
        Some(ConflictAction::Replace) => Resolution::Replace,
        _ => Resolution::Raise,
    }
}

/// A conflict a row would cause, with the error it would report.
struct Conflict {
    /// The key of the row already there.
    key: Vec<OwnedDatum>,
    /// The error an aborting statement reports.
    error: DbError,
}

/// Returns the row a new row would collide with, if there is one.
///
/// Checks the table's own key first and then every `UNIQUE` index, each through
/// a point probe - the TDD's "`UNIQUE` enforced through `PointProbe`".
///
/// @param table - the table being written
/// @param layout - the table tree's layout
/// @param target - the file and its trees
/// @param row - the row about to be written
fn conflicting_row(
    table: &TableInfo,
    layout: &SourceLayout,
    target: &mut dyn WriteTarget,
    row: &[OwnedDatum],
) -> DbResult<Option<Conflict>> {
    let key = key_of(layout, row);
    if !key.is_empty() && row_exists(table, target, &key)? {
        let (code, message) = rowid_message(table);
        return Ok(Some(Conflict {
            key,
            error: DbError::new(ExtendedCode(code)).with_message(message),
        }));
    }
    for index in unique_indexes(table) {
        let entry = index_entry(index, layout, row);
        // A NULL is distinct from every other NULL in a UNIQUE index, which is
        // SQL's rule and the reason a nullable unique column may hold any
        // number of NULLs.
        let Some(prefix) = distinct_prefix(index, &entry) else {
            continue;
        };
        let found = {
            let (database, trees) = target.parts();
            match trees.get(index.root) {
                Some(tree) => {
                    let borrowed: Vec<Datum<'_>> = prefix.iter().map(OwnedDatum::borrow).collect();
                    tree.point(database.pool(), &borrowed)?
                }
                None => None,
            }
        };
        if let Some(found) = found {
            let code = if index.origin == IndexOrigin::PrimaryKey {
                codes::PRIMARY_KEY
            } else {
                codes::UNIQUE
            };
            return Ok(Some(Conflict {
                key: found.last().cloned().into_iter().collect(),
                error: DbError::new(ExtendedCode(code)).with_message(unique_message(table, index)),
            }));
        }
    }
    Ok(None)
}

/// Applies an `ON CONFLICT ... DO UPDATE` to the row already there.
///
/// `excluded.*` is the row that was being inserted, which is stage one of the
/// row space - so the assignments are evaluated against a batch holding the
/// existing row and then the excluded row, and nothing has to substitute
/// anything.
///
/// @param table - the table being written
/// @param layout - the table tree's layout
/// @param space - the row space
/// @param plan - the compiled statement
/// @param target - the file and its trees
/// @param log - where the records go
/// @param clash - the conflict, naming the row already there
/// @param excluded - the row that was being inserted
#[allow(clippy::too_many_arguments)]
fn upsert_row(
    statement: &BoundInsert,
    table: &TableInfo,
    layout: &SourceLayout,
    space: &RowSpace,
    plan: &InsertPlan,
    target: &mut dyn WriteTarget,
    log: &mut dyn TreeLog,
    clash: &Conflict,
    excluded: &[OwnedDatum],
) -> DbResult<Row> {
    // **The row that is there is read only when something needs it.** An upsert
    // that assigns every column but the key, over a table with no index, and
    // whose assignments read only `excluded`, is a row the statement already
    // holds - and reading it copies every column. On the gate's `wide` table
    // that is a four-kilobyte body materialised and thrown away per statement.
    let needed = needs_before(table, layout, statement, plan);
    let before = if needed {
        read_row(table, target, &clash.key)?.ok_or_else(|| {
            misuse("the conflicting row vanished between the probe and the update")
        })?
    } else {
        let mut blank = vec![OwnedDatum::Null; space.width];
        for (at, value) in clash.key.iter().enumerate() {
            if let Some(column) = layout.key_columns.get(at).copied() {
                if let Some(cell) = blank.get_mut(column) {
                    *cell = value.clone();
                }
            }
        }
        blank
    };
    let mut after = before.clone();
    for (slot, eval) in &plan.upsert {
        let value = space.evaluate(eval.as_ref(), &[before.as_slice(), excluded])?;
        if let Some(cell) = after.get_mut(*slot) {
            *cell = value;
        }
    }
    if needed {
        replace_row(table, layout, target, log, &before, &after)?;
    } else {
        place_row(table, layout, target, log, None, &after)?;
    }
    Ok(after)
}

/// Reports whether an upsert has to read the row it is replacing.
///
/// It does when any of three things is true, and each is a reason on its own:
/// the table has an index, whose entry has to be compared against the old one;
/// a column is not assigned, so its old value has to be carried forward; or an
/// assignment reads the target row rather than `excluded`.
///
/// When none of them is, the new row is the key plus the assignments and the
/// old one is never looked at.
///
/// @param table - the table being written
/// @param layout - the table tree's layout
/// @param statement - the bound insert, for its assignments
/// @param plan - the compiled statement, for which columns are assigned
fn needs_before(
    table: &TableInfo,
    layout: &SourceLayout,
    statement: &BoundInsert,
    plan: &InsertPlan,
) -> bool {
    if maintained(table).next().is_some() {
        return true;
    }
    for column in 0..layout.width {
        if Some(column) == layout.rowid {
            continue;
        }
        if !plan.upsert.iter().any(|(slot, _)| *slot == column) {
            return true;
        }
    }
    let Some(clause) = &statement.upsert else {
        return true;
    };
    clause.assignments.iter().any(|assignment| {
        let mut used = inillucent_sql::bind::ColumnUse::default();
        assignment
            .value
            .columns_read(statement.target_source, &mut used);
        used.opaque || !used.columns.is_empty()
    })
}

/// Applies an `UPDATE` to rows a query has already selected.
///
/// @param statement - the bound update
/// @param target - the file and its trees
/// @param log - where the records go
/// @param params - the bound parameters
/// @param keys - the key of each row the `WHERE` selected
pub fn update(
    statement: &BoundUpdate,
    target: &mut dyn WriteTarget,
    log: &mut dyn TreeLog,
    params: &Params,
    keys: &[Row],
) -> DbResult<Changes> {
    let table = &statement.table;
    let layout = layout_of(target, table)?;
    // **Only the images the statement can actually read.** A row space costs a
    // layout clone and a batch column per stage, per statement, and a statement
    // with no triggers can reach neither `OLD` nor `NEW` - so building them was
    // three times the allocation for one image's worth of use. The gate's
    // `txn.large` is forty of these in one transaction and pays for every one.
    let space = RowSpace::new(&sources_for(statement.source, &statement.triggers), &layout);
    let mut assignments = Vec::with_capacity(statement.assignments.len());
    for assignment in &statement.assignments {
        let Some(slot) = layout
            .slots
            .get(usize::from(assignment.column))
            .copied()
            .flatten()
        else {
            continue;
        };
        assignments.push((slot, space.compile(&assignment.value, params)?));
    }
    let mut projected = Vec::with_capacity(statement.returning.len());
    for column in &statement.returning {
        projected.push(space.compile(&column.expr, params)?);
    }

    let mut changes = Changes::default();
    for key in keys {
        // A row an earlier statement in the same transaction removed is skipped
        // rather than resurrected, which is what SQLite does.
        let Some(before) = read_row(table, target, key)? else {
            continue;
        };
        let mut after = before.clone();
        for (slot, eval) in &assignments {
            // Every assignment reads the *before* image, so `SET a = b, b = a`
            // swaps the two rather than making them equal.
            let value = space.evaluate(eval.as_ref(), &[before.as_slice()])?;
            if let Some(cell) = after.get_mut(*slot) {
                *cell = value;
            }
        }
        // Changing a key moves the row, so the uniqueness of the new key is an
        // ordinary conflict check; leaving it alone is not, or every update
        // would collide with the row it is updating.
        if key_of(&layout, &after) != key_of(&layout, &before) {
            if let Some(clash) = conflicting_row(table, &layout, target, &after)? {
                match statement.on_conflict {
                    Some(ConflictAction::Ignore) => continue,
                    Some(ConflictAction::Replace) => {
                        let Some(held) = read_row(table, target, &clash.key)? else {
                            continue;
                        };
                        remove_row(table, &layout, target, log, &clash.key, &held)?;
                    }
                    _ => return Err(clash.error),
                }
            }
        }
        replace_row(table, &layout, target, log, &before, &after)?;
        changes.rows = changes.rows.saturating_add(1);
        if !projected.is_empty() {
            let mut out = Vec::with_capacity(projected.len());
            for eval in &projected {
                out.push(space.evaluate(eval.as_ref(), &[after.as_slice()])?);
            }
            changes.returned.push(out);
        }
    }
    Ok(changes)
}

/// Applies a `DELETE` to rows a query has already selected.
///
/// @param statement - the bound delete
/// @param target - the file and its trees
/// @param log - where the records go
/// @param params - the bound parameters
/// @param keys - the key of each row the `WHERE` selected
pub fn delete(
    statement: &BoundDelete,
    target: &mut dyn WriteTarget,
    log: &mut dyn TreeLog,
    params: &Params,
    keys: &[Row],
) -> DbResult<Changes> {
    let table = &statement.table;
    let layout = layout_of(target, table)?;
    let space = RowSpace::new(&sources_for(statement.source, &statement.triggers), &layout);
    let mut projected = Vec::with_capacity(statement.returning.len());
    for column in &statement.returning {
        projected.push(space.compile(&column.expr, params)?);
    }

    let mut changes = Changes::default();
    for key in keys {
        let Some(row) = read_row(table, target, key)? else {
            continue;
        };
        // `RETURNING` on a delete names the row that is going away, so it is
        // read before the row stops existing.
        if !projected.is_empty() {
            let mut out = Vec::with_capacity(projected.len());
            for eval in &projected {
                out.push(space.evaluate(eval.as_ref(), &[row.as_slice()])?);
            }
            changes.returned.push(out);
        }
        // The row was read a moment ago for `RETURNING` and for the index
        // entries; reading it again inside the removal was a second descent per
        // delete, on the workload the gate measures two thousand of.
        remove_row(table, &layout, target, log, key, &row)?;
        changes.rows = changes.rows.saturating_add(1);
    }
    Ok(changes)
}

/// Replaces one row and every index entry that changed with it.
///
/// @param table - the table being written
/// @param layout - the table tree's layout
/// @param target - the file and its trees
/// @param log - where the records go
/// @param before - the row as it was
/// @param after - the row as it should be
fn replace_row(
    table: &TableInfo,
    layout: &SourceLayout,
    target: &mut dyn WriteTarget,
    log: &mut dyn TreeLog,
    before: &[OwnedDatum],
    after: &[OwnedDatum],
) -> DbResult<()> {
    if key_of(layout, after) != key_of(layout, before) {
        // The row moved, so the old one is a delete and the new one an insert.
        // Doing it as an in-place replace would leave the old key behind.
        remove_row(table, layout, target, log, &key_of(layout, before), before)?;
        return place_row(table, layout, target, log, None, after);
    }
    place_row(table, layout, target, log, Some(before), after)
}

/// Writes one row only if its key is free, and maintains its index entries.
///
/// Returns false, having written nothing, when the key was taken. The index
/// entries go first as everywhere else, so a failure part-way leaves an entry
/// pointing at a row that is not there - which the integrity checker names -
/// rather than a row no index can find.
///
/// @param table - the table being written
/// @param layout - the table tree's layout
/// @param target - the file and its trees
/// @param log - where the records go
/// @param row - the row to write
fn place_row_absent(
    table: &TableInfo,
    layout: &SourceLayout,
    target: &mut dyn WriteTarget,
    log: &mut dyn TreeLog,
    row: &[OwnedDatum],
) -> DbResult<bool> {
    let placed = {
        let (database, trees) = target.parts();
        let tree = trees
            .get_mut(table.root)
            .ok_or_else(|| missing_tree(table))?;
        let borrowed: Vec<Datum<'_>> = row.iter().map(OwnedDatum::borrow).collect();
        tree.put_absent(database, log, &borrowed)?
    };
    if !placed {
        return Ok(false);
    }
    for index in maintained(table) {
        let entry = index_entry(index, layout, row);
        write_index_entry(index, target, log, &entry, true)?;
    }
    Ok(true)
}

/// Writes one row and adds its index entries, removing the previous ones.
///
/// @param table - the table being written
/// @param layout - the table tree's layout
/// @param target - the file and its trees
/// @param log - where the records go
/// @param before - the row this one replaces, when it replaces one
/// @param row - the row to write
fn place_row(
    table: &TableInfo,
    layout: &SourceLayout,
    target: &mut dyn WriteTarget,
    log: &mut dyn TreeLog,
    before: Option<&[OwnedDatum]>,
    row: &[OwnedDatum],
) -> DbResult<()> {
    // **An index whose entry did not change is not touched at all.**
    //
    // The first version removed every entry and added every entry, which is
    // correct - the bytes going back are the bytes that came out - and it is two
    // tree writes and two log records per index for a statement that changed
    // nothing in it. `UPDATE side_table SET note = ?2 WHERE id = ?1` does not
    // touch `owner`, so `side_owner` was being rewritten on every one of the
    // gate's two thousand updates: two thirds of the tree writes, for nothing.
    // SQLite does not touch such an index either.
    //
    // Old before new *within* an index, still, so an entry that did change
    // leaves and comes back rather than briefly existing twice.
    for index in maintained(table) {
        let after = index_entry(index, layout, row);
        let previous = before.map(|held| index_entry(index, layout, held));
        if previous.as_deref() == Some(after.as_slice()) {
            continue;
        }
        if let Some(previous) = previous {
            write_index_entry(index, target, log, &previous, false)?;
        }
        write_index_entry(index, target, log, &after, true)?;
    }
    let (database, trees) = target.parts();
    let tree = trees
        .get_mut(table.root)
        .ok_or_else(|| missing_tree(table))?;
    let borrowed: Vec<Datum<'_>> = row.iter().map(OwnedDatum::borrow).collect();
    tree.put(database, log, &borrowed)?;
    Ok(())
}

/// Removes one row and every index entry that named it.
///
/// @param table - the table being written
/// @param layout - the table tree's layout
/// @param target - the file and its trees
/// @param log - where the records go
/// @param key - the row's key
/// @param row - the row as it stands, which the caller has already read
fn remove_row(
    table: &TableInfo,
    layout: &SourceLayout,
    target: &mut dyn WriteTarget,
    log: &mut dyn TreeLog,
    key: &[OwnedDatum],
    row: &[OwnedDatum],
) -> DbResult<()> {
    for index in maintained(table) {
        let entry = index_entry(index, layout, row);
        write_index_entry(index, target, log, &entry, false)?;
    }
    let (database, trees) = target.parts();
    let tree = trees
        .get_mut(table.root)
        .ok_or_else(|| missing_tree(table))?;
    let borrowed: Vec<Datum<'_>> = key.iter().map(OwnedDatum::borrow).collect();
    tree.delete(database, log, &borrowed)?;
    Ok(())
}

/// Returns the indexes of a table that the write path maintains.
///
/// A `WITHOUT ROWID` table's primary-key index *is* the table: one b-tree,
/// reported at the table's own root. Maintaining it separately would write
/// every row twice.
///
/// @param table - the table
fn maintained(table: &TableInfo) -> impl Iterator<Item = &IndexInfo> {
    table
        .indexes
        .iter()
        .filter(|index| index.root != 0 && index.root != table.root)
}

/// Adds or removes one entry in one index.
///
/// **This is the index maintenance the acceptance asks a differential test to
/// prove.** It is one function rather than one per caller, because an index an
/// insert maintains and a delete forgets is a tree that disagrees with its table
/// and answers a covering query wrongly while every other query is fine.
///
/// @param index - the index
/// @param target - the file and its trees
/// @param log - where the records go
/// @param entry - the entry: the keys, then the rowid
/// @param adding - true to add it, false to remove it
fn write_index_entry(
    index: &IndexInfo,
    target: &mut dyn WriteTarget,
    log: &mut dyn TreeLog,
    entry: &[OwnedDatum],
    adding: bool,
) -> DbResult<()> {
    let (database, trees) = target.parts();
    let Some(tree) = trees.get_mut(index.root) else {
        return Ok(());
    };
    let borrowed: Vec<Datum<'_>> = entry.iter().map(OwnedDatum::borrow).collect();
    if adding {
        tree.put(database, log, &borrowed)?;
    } else {
        tree.delete(database, log, &borrowed)?;
    }
    Ok(())
}

/// Builds one index entry from a table row.
///
/// An index entry is the indexed columns followed by the rowid, which is what
/// the import builds and what every read of an index assumes.
///
/// @param index - the index
/// @param layout - the table tree's layout
/// @param row - the table row, in tree-column order
fn index_entry(index: &IndexInfo, layout: &SourceLayout, row: &[OwnedDatum]) -> Row {
    let mut entry = Vec::with_capacity(index.columns.len().saturating_add(1));
    for column in &index.columns {
        entry.push(
            column
                .column
                .and_then(|declared| layout.slots.get(usize::from(declared)).copied().flatten())
                .and_then(|slot| row.get(slot).cloned())
                .unwrap_or(OwnedDatum::Null),
        );
    }
    entry.push(
        layout
            .rowid
            .and_then(|slot| row.get(slot).cloned())
            .unwrap_or(OwnedDatum::Null),
    );
    entry
}

/// Returns the unique indexes of a table that are trees of their own.
///
/// @param table - the table
fn unique_indexes(table: &TableInfo) -> impl Iterator<Item = &IndexInfo> {
    table
        .indexes
        .iter()
        .filter(|index| index.unique && index.root != 0 && index.root != table.root)
}

/// Returns an index entry's key prefix, or `None` when a NULL makes it distinct.
///
/// @param index - the index
/// @param entry - the entry: the keys, then the rowid
fn distinct_prefix(index: &IndexInfo, entry: &[OwnedDatum]) -> Option<Vec<OwnedDatum>> {
    let prefix: Vec<OwnedDatum> = entry.iter().take(index.columns.len()).cloned().collect();
    if prefix.is_empty() || prefix.iter().any(|value| *value == OwnedDatum::Null) {
        return None;
    }
    Some(prefix)
}

/// Returns a row's key, in key-column order.
///
/// @param layout - the table tree's layout
/// @param row - the row, in tree-column order
fn key_of(layout: &SourceLayout, row: &[OwnedDatum]) -> Vec<OwnedDatum> {
    layout
        .key_columns
        .iter()
        .filter_map(|column| row.get(*column).cloned())
        .collect()
}

/// Reports whether a table holds a row under one key.
///
/// **The uniqueness check asks only whether something is there**, and reading
/// the row to find out copies every column of it. On the gate's `wide` table -
/// two columns, one of them a four-kilobyte body - that copy was about fifteen
/// microseconds of a seventeen-microsecond upsert, allocated and thrown away on
/// every statement.
///
/// @param table - the table
/// @param target - the file and its trees
/// @param key - the row's key
fn row_exists(
    table: &TableInfo,
    target: &mut dyn WriteTarget,
    key: &[OwnedDatum],
) -> DbResult<bool> {
    let (database, trees) = target.parts();
    let Some(tree) = trees.get(table.root) else {
        return Ok(false);
    };
    let borrowed: Vec<Datum<'_>> = key.iter().map(OwnedDatum::borrow).collect();
    tree.contains(database.pool(), &borrowed)
}

/// Reads one row of a table by key, in tree-column order.
///
/// @param table - the table
/// @param target - the file and its trees
/// @param key - the row's key
fn read_row(
    table: &TableInfo,
    target: &mut dyn WriteTarget,
    key: &[OwnedDatum],
) -> DbResult<Option<Row>> {
    let (database, trees) = target.parts();
    let Some(tree) = trees.get(table.root) else {
        return Ok(None);
    };
    let borrowed: Vec<Datum<'_>> = key.iter().map(OwnedDatum::borrow).collect();
    tree.point(database.pool(), &borrowed)
}

/// Returns a table tree's layout, cloned so the borrow can be released.
///
/// @param target - the file and its trees
/// @param table - the table
fn layout_of(target: &dyn WriteTarget, table: &TableInfo) -> DbResult<SourceLayout> {
    target.layout(table.root).cloned().ok_or_else(|| {
        misuse(format!(
            "no layout imported for {}",
            String::from_utf8_lossy(&table.name)
        ))
    })
}

/// Returns the error a missing tree reports.
///
/// @param table - the table whose tree is missing
fn missing_tree(table: &TableInfo) -> DbError {
    misuse(format!(
        "no tree imported for {}",
        String::from_utf8_lossy(&table.name)
    ))
}

/// A `WriteTarget` that borrows another, so a closure can hold one.
///
/// The rowid lookup is passed as a closure to `build_row`, which already holds
/// the target through a different path; this is the borrow that lets both exist
/// without the target being moved.
struct Borrowed<'a>(&'a mut dyn WriteTarget);

impl WriteTarget for Borrowed<'_> {
    fn parts(&mut self) -> (&mut Database, &mut dyn Trees) {
        self.0.parts()
    }

    fn layout(&self, root: u32) -> Option<&SourceLayout> {
        self.0.layout(root)
    }
}

/// Returns the largest rowid a table holds, or zero when it holds none.
///
/// The rightmost leaf's last live row, found by descending for the largest key
/// there can be. Reading it costs a descent rather than a scan, and reading it
/// rather than remembering it is what makes an insert after a delete reuse the
/// number - which is SQLite's rule for a table that is not `AUTOINCREMENT`.
///
/// @param target - the file and its trees
/// @param table - the table
fn highest_rowid(target: &mut dyn WriteTarget, table: &TableInfo) -> DbResult<i64> {
    let (database, trees) = target.parts();
    let Some(tree) = trees.get(table.root) else {
        return Ok(0);
    };
    if tree.key_columns() != 1 {
        return Ok(0);
    }
    largest_key(tree, database.pool())
}

/// Returns the largest integer key in a tree.
///
/// @param tree - the tree
/// @param pool - the buffer pool
fn largest_key(tree: &PagedTree, pool: &Pool) -> DbResult<i64> {
    let key = tree.encode_key(&[Datum::Int(i64::MAX)]);
    let (guard, _) = tree.descend_guard(pool, &key)?;
    let leaf = LeafRef::parse(&guard)?.with_collations(tree.collations());
    let rows = leaf.live()?;
    Ok(rows
        .last()
        .and_then(|row| row.first())
        .and_then(|value| match value {
            Datum::Int(number) => Some(*number),
            _ => None,
        })
        .unwrap_or(0))
}
