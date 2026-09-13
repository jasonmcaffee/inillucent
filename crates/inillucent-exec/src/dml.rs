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

use inillucent_base::error::{misuse, Unwind};
use inillucent_base::{DbError, DbResult, ExtendedCode};
use inillucent_pool::{Database, Pool};
use inillucent_sql::ast::{ConflictAction, JoinKind as SqlJoinKind, TriggerTime};
use inillucent_sql::bind::{
    BoundExpr, BoundResultColumn, BoundSelect, BoundSource, SourceRows, EXCLUDED_SOURCE,
    NEW_SOURCE, OLD_SOURCE,
};
use inillucent_sql::catalog_view::{IndexInfo, IndexOrigin, TableInfo, TableKind};
use inillucent_sql::dml::{
    codes, rowid_message, unique_message, BoundDelete, BoundInsert, BoundInsertSource, BoundUpdate,
};
use inillucent_tree::datum::{Datum, OwnedDatum};
use inillucent_tree::leaf::LeafRef;
use inillucent_tree::write::TreeLog;
use inillucent_tree::PagedTree;
use inillucent_value::collation::Collation;

use crate::batch::{Batch, Vector};
use crate::declared::{IndexExprs, WriteDeclarations};
use crate::expr::{compile, Eval};
use crate::insert_plan::InsertPlan;
use crate::physical::{
    translate_scan, AccessKind, HeldSpace, Params, PreparedStage, SourceLayout, TreeCatalog,
};
use crate::trigger::{self, Depth};

/// One row in a tree's own column order.
///
/// A table row is `[rowid] ++ [record slots except the rowid alias]`, which is
/// what [`SourceLayout`] describes; an index entry is the indexed columns
/// followed by the rowid. Both are this type, because both are just a tree's
/// row, and the write path never holds a row in any other shape.
pub type Row = Vec<OwnedDatum>;

/// What [`write_one`] actually did with a row, so its caller can tell a
/// genuine insert from a conflict `DO UPDATE` resolved onto a row that was
/// already there.
///
/// **`last_insert_rowid()` only moves for the first of these.** SQLite's rule
/// is that it is set by `INSERT`, never by `UPDATE` - and an upsert's `DO
/// UPDATE` arm is an update, whatever statement it is spelled inside. Before
/// this distinction existed, both arms fed the same row image into
/// `Changes::last_rowid`, so `INSERT INTO t VALUES(3, 'one', 9) ON
/// CONFLICT(b) DO UPDATE SET hits = excluded.hits` - which updates the row
/// `b = 'one'` already names - reported *that* row's rowid as the last one
/// inserted, where the reference leaves `last_insert_rowid()` at whatever the
/// last genuine `INSERT` set it to.
enum Stored {
    /// A new row was placed in the tree.
    Inserted(Row),
    /// An existing row was rewritten in place, by an `ON CONFLICT ... DO
    /// UPDATE` arm.
    Updated(Row),
}

impl Stored {
    /// Returns the row, however it got there.
    fn row(&self) -> &[OwnedDatum] {
        match self {
            Stored::Inserted(row) | Stored::Updated(row) => row,
        }
    }
}

/// What a data-modifying statement did.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Changes {
    /// How many rows the statement inserted, updated or deleted.
    pub rows: usize,
    /// The rows `RETURNING` asked for, when the statement asked for any.
    pub returned: Vec<Row>,
    /// The rowid of the last row an `INSERT` stored, when the table has one.
    ///
    /// `sqlite3_last_insert_rowid` reads this, and a caller asks the connection
    /// for it after the statement is gone - so it is reported out of the write
    /// rather than dug back out of the tree, which is the only place the value
    /// is known without paying a second descent for it.
    pub last_rowid: Option<i64>,
    /// The row images this statement stored, when the target asked for them.
    ///
    /// **An index a module owns is maintained from these.** A vector index is
    /// not a b-tree: its rows live in a virtual table, the module that owns it
    /// is registered on the connection, and the connection is exactly what a
    /// write has split apart - so the write cannot reach the module and the
    /// module cannot see the write. Reporting what was stored and what was
    /// removed lets the *engine* apply both to the module after the write and
    /// inside the same transaction, which is the one place that holds both
    /// halves.
    ///
    /// Empty unless [`WriteTarget::captures`] says the table has such an index,
    /// because the images are clones and every other statement would pay for
    /// them.
    pub written: Vec<Row>,
    /// The row images this statement removed, on the same terms.
    pub removed: Vec<Row>,
}

/// Counts one written row, on the statement's tally and on the target's.
///
/// Two tallies rather than one because they answer different questions after a
/// failure: the [`Changes`] is what a statement that *succeeded* reports, and
/// the target's is what survives one that did not - see
/// [`WriteTarget::count_row`].
///
/// @param changes - the statement's own tally
/// @param target - the file being written, which outlives a failure
/// @param depth - how many triggers deep this write is, zero being the
///   statement the user ran
fn count_row(changes: &mut Changes, target: &dyn WriteTarget, depth: Depth) {
    changes.rows = changes.rows.saturating_add(1);
    target.count_row(depth.0 == 0);
}

/// Counts one row an `INSTEAD OF` trigger dispatched, on the statement's own
/// tally only - never on the target's.
///
/// **A view write is never the target's "outer" row, whatever depth it ran
/// at.** `count_row` above marks a write as the user's own exactly when
/// `depth.0 == 0`, which is right for an ordinary table: the statement wrote
/// the row itself. A view has no tree of its own, so `insert_into_view`,
/// `update_view` and `delete_view` never write anything directly - the
/// `INSTEAD OF` trigger's own body does, through its own nested
/// `insert_at`/`update_at`/`delete_at` call one depth deeper, which already
/// counts correctly there. Routing the view dispatch itself through
/// `count_row` double-counted: the outer `INSERT INTO v ...` reported
/// `changes()` as 1 where SQLite reports 0, because the dispatch loop ran at
/// depth 0 and `count_row` read that as the statement's own write rather than
/// the trigger's. SQLite's own rule is not a special case for views - it is
/// the same "a trigger body's rows go into `total_changes()`, never into
/// `changes()`" rule, applied to a statement whose entire effect is the
/// trigger body's.
///
/// @param changes - the statement's own tally
fn count_view_row(changes: &mut Changes) {
    changes.rows = changes.rows.saturating_add(1);
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
    /// Returns the file one tree is in, its trees, and its log - all three at
    /// the same instant.
    ///
    /// **The log comes back with the file rather than being handed in.** A
    /// connection is a set of databases: a `TEMP` trigger firing on a write to
    /// `main` writes rows into two files inside one statement, and one
    /// `&mut dyn TreeLog` cannot describe both. Asking for the log by naming the
    /// tree is what makes it impossible to write a row into one file and
    /// describe it in another's log - which would be a database that recovers
    /// into a state it was never in.
    ///
    /// The trees come back whole rather than one tree, because a write touches a
    /// table and its indexes together and they are all in the same file.
    ///
    /// @param root - the handle of the tree about to be written
    fn parts_for(
        &mut self,
        root: u32,
    ) -> DbResult<(&mut Database, &mut dyn Trees, &mut dyn TreeLog)>;

    /// Returns a tree's layout: which tree column each declared column holds.
    ///
    /// @param root - the root page id the catalog names the tree by
    fn layout(&self, root: u32) -> Option<&std::rc::Rc<SourceLayout>>;

    /// Records that one row was written, for `changes()` and
    /// `total_changes()`.
    ///
    /// **Counted here rather than in the [`Changes`] that comes back, because
    /// the count has to survive a failure.** A statement that fails partway is
    /// an `Err` and the `Changes` it had built is gone - and `OR FAIL` keeps
    /// what it wrote, so SQLite reports `1 | 4` where this engine reported
    /// `0 | 0`. The target outlives the error: it is the thing the
    /// write was performed against, so the tally is read off it afterwards on
    /// either path.
    ///
    /// The two numbers are different questions and SQLite answers them
    /// differently. `changes()` counts the rows the statement wrote *itself*;
    /// `total_changes()` counts every row written under it, a trigger's and a
    /// foreign key's cascade included - measured, on the pinned 3.53.4: an
    /// `INSERT` of two rows with an `AFTER INSERT` trigger that inserts one row
    /// each answers `changes() = 2` and moves `total_changes()` by 4.
    ///
    /// @param outer - whether the statement writing it is the one the user ran,
    ///   rather than a trigger body some levels in
    fn count_row(&self, outer: bool) {
        let _ = outer;
    }

    /// Records the rowid an `INSERT` assigned.
    ///
    /// Kept beside the row counts and for the same reason: SQLite's
    /// `last_insert_rowid()` is the last rowid *attempted*, so a statement that
    /// wrote a row and then undid it still moves it, and the value has to
    /// outlive the error the way the counts do.
    ///
    /// @param rowid - the key the row was written under
    fn count_rowid(&self, rowid: i64) {
        let _ = rowid;
    }

    /// Returns what this target has been told, as
    /// `(rows the statement wrote itself, rows written in all, last rowid)`.
    fn rows_written(&self) -> (i64, i64, Option<i64>) {
        (0, 0, None)
    }

    /// Returns this target as the catalog a planned query reads.
    ///
    /// **A write can have to run a query, and a trigger is why.** The body of
    /// `CREATE TRIGGER ... BEGIN DELETE FROM child WHERE parent_id = OLD.id;
    /// END` is a statement that has to find rows, and it fires in the middle of
    /// the write that is holding this target. Handing the same view back as a
    /// [`crate::physical::TreeCatalog`] is what lets the body reach the
    /// ordinary planner - so that `DELETE` gets the same index probe the same
    /// `DELETE` typed by hand would, rather than a scan written a second time
    /// inside the write path.
    fn catalog(&self) -> &dyn crate::physical::TreeCatalog;

    /// Reports whether this table's row images have to be reported back.
    ///
    /// False for every ordinary table, which is what keeps the clone off the
    /// common path. True for one an index a module owns is built over: see
    /// [`Changes::written`].
    ///
    /// @param _root - the table's root page id
    fn captures(&self, _root: u32) -> bool {
        false
    }
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
    ///
    /// `pub(crate)` for `insert_plan::InsertPlan::build_row`, which sizes a
    /// fresh row image against it.
    pub(crate) width: usize,
    /// Which column of an image holds its rowid.
    ///
    /// `pub(crate)` for the same reason `width` is.
    pub(crate) rowid: Option<usize>,
    /// Which cell each correlated subquery's answer sits in.
    ///
    /// Appended after every row image, which is why they are numbered from the
    /// end of the concatenated space rather than inside any one image. Empty
    /// for every statement with no correlated subquery in it - which is every
    /// statement the gate measures, and the reason this costs them nothing.
    correlations: Vec<(usize, usize)>,
}

impl RowSpace {
    /// Builds a space over one or more row images of the same layout.
    ///
    /// @param sources - the binder source number each image answers to
    /// @param layout - the table tree's layout, shared by every image
    pub fn new(sources: &[usize], layout: &std::rc::Rc<SourceLayout>) -> RowSpace {
        let mut stages = Vec::with_capacity(sources.len());
        let mut layouts = Vec::with_capacity(sources.len());
        let mut types = Vec::with_capacity(sources.len().saturating_mul(layout.width));
        for (term, source) in sources.iter().enumerate() {
            stages.push(PreparedStage {
                functions: Vec::new(),
                root: layout.tree_key,
                // These stages read no tree: they are row images the caller
                // already holds. `Materialised` is the kind that says so.
                kind: AccessKind::Materialised,
                source: *source,
                term,
                is_lookup: false,
                offset: term.saturating_mul(layout.width),
                width: layout.width,
                layout: Some(std::rc::Rc::clone(layout)),
            });
            layouts.push(std::rc::Rc::clone(layout));
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
            correlations: Vec::new(),
        }
    }

    /// Adds one cell per correlated subquery, after every row image.
    ///
    /// An `UPDATE a SET tag = (SELECT tag FROM b WHERE b.id = a.id)` reads the
    /// row it is writing, so its value is not a constant for the statement and
    /// cannot be folded the way an uncorrelated one is. It is computed per row
    /// by the same operator a `SELECT` uses and handed in beside the images -
    /// so there is one implementation of what a correlated block means rather
    /// than a second one in the write path.
    ///
    /// @param ids - the binder's number for each block, in the order the write
    ///   path will supply their answers
    pub fn with_correlations(mut self, ids: &[usize]) -> RowSpace {
        let base = self.stages.len().saturating_mul(self.width);
        self.correlations = ids
            .iter()
            .enumerate()
            .map(|(position, id)| (*id, base.saturating_add(position)))
            .collect();
        self.held.types.extend(std::iter::repeat_n(
            crate::expr::StaticType::Unknown,
            ids.len(),
        ));
        self
    }

    /// Returns the binder's number for each correlated block, in cell order.
    pub fn correlation_ids(&self) -> Vec<usize> {
        self.correlations.iter().map(|(id, _)| *id).collect()
    }

    /// Compiles one bound expression against this space.
    ///
    /// **The catalog is a parameter, not a field.** A `RowSpace` is carried
    /// through every write-path signature that builds or evaluates one, and
    /// giving it a borrowed catalog of its own would put that borrow's
    /// lifetime on all of them - which is why a registered function used to
    /// refuse by name here (`docs/roadmap.md` item 13): the physical pass
    /// resolves a registered function's body through the catalog, and this
    /// space carried none. Every caller already holds a [`WriteTarget`], and
    /// [`WriteTarget::catalog`] is exactly the view [`translate_scan`] needs -
    /// handed in for the one call it is needed on rather than stored.
    ///
    /// @param expr - the bound expression
    /// @param params - the bound parameters
    /// @param catalog - where a registered function's body is looked up
    pub fn compile(
        &self,
        expr: &BoundExpr,
        params: &Params,
        catalog: &dyn TreeCatalog,
    ) -> DbResult<Box<dyn Eval>> {
        let mut space = self.held.view_with(&self.stages, &self.correlations);
        space.catalog = Some(catalog);
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
        self.evaluate_with(eval, images, &[])
    }

    /// Evaluates a compiled expression with the correlated answers beside it.
    ///
    /// @param eval - the compiled expression
    /// @param images - one row per stage, concatenated into a one-row batch
    /// @param correlated - one answer per correlated block, in cell order
    pub fn evaluate_with(
        &self,
        eval: &dyn Eval,
        images: &[&[OwnedDatum]],
        correlated: &[OwnedDatum],
    ) -> DbResult<OwnedDatum> {
        let mut cells: Vec<[Datum<'_>; 1]> = Vec::with_capacity(
            self.stages
                .len()
                .saturating_mul(self.width)
                .saturating_add(correlated.len()),
        );
        for stage in 0..self.stages.len() {
            let image = images.get(stage).copied().unwrap_or(&[]);
            for column in 0..self.width {
                cells.push([image
                    .get(column)
                    .map(OwnedDatum::borrow)
                    .unwrap_or(Datum::Null)]);
            }
        }
        for value in correlated {
            cells.push([value.borrow()]);
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
    keys_query_joined(table, source, filter, limit, offset, layout, &[], &[])
}

/// Returns the query that finds the keys of an `UPDATE ... FROM`, and the
/// values it will write into them.
///
/// **The rows come from a join, so the values do too.** `UPDATE t SET v = s.v
/// FROM s WHERE s.a = t.a` assigns from a row of `s`, which the write path
/// never sees: it is handed keys and evaluates the assignments against the
/// target row alone. So the keys query grows the extra FROM terms and projects
/// the assigned values *beside* the key, and the write path reads them out of
/// the row it was given rather than computing them.
///
/// With no extra terms and no projected assignments this is exactly
/// [`keys_query`] - the same one source, the same one-column projection - so an
/// ordinary `UPDATE` pays nothing for the shape.
///
/// @param table - the table being written
/// @param source - the statement-wide number of its FROM term
/// @param filter - the `WHERE` clause
/// @param limit - the `LIMIT`
/// @param offset - the `OFFSET`
/// @param layout - the table tree's layout
/// @param joined - the extra FROM terms, in written order
/// @param assigned - the assignment expressions to project, in order
#[allow(clippy::too_many_arguments)]
pub fn keys_query_joined(
    table: &TableInfo,
    source: usize,
    filter: Option<&BoundExpr>,
    limit: Option<&BoundExpr>,
    offset: Option<&BoundExpr>,
    layout: &SourceLayout,
    joined: &[BoundSource],
    assigned: &[BoundExpr],
) -> DbResult<BoundSelect> {
    let mut columns: Vec<BoundResultColumn> = key_columns(table, source, layout)?
        .into_iter()
        .map(|expr| BoundResultColumn {
            expr,
            name: b"key".to_vec(),
            origin: None,
            declared_type: Vec::new(),
        })
        .collect();
    for expr in assigned {
        columns.push(BoundResultColumn {
            expr: expr.clone(),
            name: b"value".to_vec(),
            origin: None,
            declared_type: Vec::new(),
        });
    }
    let mut sources = vec![BoundSource {
        id: source,
        rows: SourceRows::Table,
        table: std::rc::Rc::new(table.clone()),
        alias: table.name.clone(),
        join: SqlJoinKind::Inner,
        constraint: None,
        suppressed: Vec::new(),
        index_exprs: Vec::new(),
    }];
    sources.extend(joined.iter().cloned());
    Ok(BoundSelect {
        sources,
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

/// Returns the query that finds the rowids a write to a module will change.
///
/// The same shape as [`keys_query`] and without its layout, because a virtual
/// table has none: the module owns its storage, and the only handle the engine
/// has on one of its rows is the rowid the module answers with. A `DELETE` is
/// therefore "ask which rowids match, then tell the module about each" - which
/// is what SQLite does, and the reason `xUpdate` takes a rowid rather than a
/// predicate.
///
/// @param table - the virtual table being written
/// @param source - the FROM term id the filter's columns refer to
/// @param filter - the statement's `WHERE`, if it has one
/// @param limit - the statement's `LIMIT`, if it has one
/// @param offset - the statement's `OFFSET`, if it has one
pub fn module_keys_query(
    table: &TableInfo,
    source: usize,
    filter: Option<&BoundExpr>,
    limit: Option<&BoundExpr>,
    offset: Option<&BoundExpr>,
) -> BoundSelect {
    BoundSelect {
        sources: vec![BoundSource {
            id: source,
            rows: SourceRows::Table,
            table: std::rc::Rc::new(table.clone()),
            alias: table.name.clone(),
            join: SqlJoinKind::Inner,
            constraint: None,
            suppressed: Vec::new(),
            index_exprs: Vec::new(),
        }],
        filter: filter.cloned(),
        group_by: Vec::new(),
        having: None,
        columns: vec![BoundResultColumn {
            expr: BoundExpr::Rowid { source },
            name: b"key".to_vec(),
            origin: None,
            declared_type: Vec::new(),
        }],
        distinct: false,
        order_by: Vec::new(),
        limit: limit.cloned(),
        offset: offset.cloned(),
        aggregates: Vec::new(),
        values: Vec::new(),
        compounds: Vec::new(),
        windows: Vec::new(),
        correlations: Vec::new(),
    }
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
/// @param params - the bound parameters
/// @param supplied - the rows a `SELECT` source produced, empty for `VALUES`
pub fn insert(
    statement: &BoundInsert,
    target: &mut dyn WriteTarget,
    params: &Params,
    supplied: &[Row],
) -> DbResult<Changes> {
    insert_at(statement, target, params, supplied, Depth::default())
        .map_err(|error| outer_unwind(error, statement.on_conflict))
}

/// Stamps the outermost statement's `OR` clause onto whatever it failed with.
///
/// **Only the outermost**, which is why this is on the `insert`/`update`
/// wrappers rather than on the `_at` bodies a trigger's own statements call.
/// SQLite's rule is that "if an `ON CONFLICT` clause is specified as part of
/// the statement causing the trigger to fire, then conflict handling policy of
/// the outer statement is used instead" - so the outer clause overrides a
/// nested statement's and a constraint's, and only an explicit `RAISE` beats
/// it.
///
/// A statement with no `OR` clause stamps nothing, leaving whatever the
/// constraint said, and leaving an untagged error to read as `ABORT`.
///
/// @param error - what the statement failed with
/// @param on_conflict - the statement's own `OR` clause
fn outer_unwind(error: DbError, on_conflict: Option<ConflictAction>) -> DbError {
    match on_conflict {
        Some(action) => error.with_outer_unwind(unwind_of(Some(action))),
        None => error,
    }
}

/// Applies an `INSERT` that is already some triggers deep.
///
/// The depth is what the recursion cap counts, and it is a parameter rather
/// than a global because a trigger's body is a statement like any other: the
/// count has to describe this chain of fires rather than everything the process
/// has ever fired.
///
/// @param statement - the bound insert
/// @param target - the file and its trees
/// @param params - the bound parameters
/// @param supplied - the rows a `SELECT` source produced, empty for `VALUES`
/// @param depth - how many triggers deep this write already is
pub fn insert_at(
    statement: &BoundInsert,
    target: &mut dyn WriteTarget,
    params: &Params,
    supplied: &[Row],
    depth: Depth,
) -> DbResult<Changes> {
    let table = &statement.table;
    // **A view has no rows of its own, so the trigger IS the write.** The binder
    // only lets a view be written when it has an `INSTEAD OF` trigger for the
    // event; the statement's job is to build `NEW` and fire it, and nothing is
    // stored. Reaching the ordinary path with a view asked for the layout of a
    // table with no tree, which is where `no layout imported for v` came from.
    if table.kind == TableKind::View {
        return insert_into_view(statement, target, params, supplied, depth);
    }
    let layout = layout_of(target, table)?;
    // `excluded` only exists inside an `ON CONFLICT ... DO UPDATE`, so a plain
    // insert carries one image rather than two.
    let mut sources = vec![statement.target_source];
    if !statement.upsert.is_empty() {
        sources.push(EXCLUDED_SOURCE);
    }
    let space = RowSpace::new(&sources, &layout);
    // **The catalog the write path's own registered-function lookups read.**
    // `target` already exposes one for a trigger body's queries
    // (`WriteTarget::catalog`) - see `docs/roadmap.md` item 13 for why a
    // `VALUES` row calling `embed(?1)` needs the same view.
    let catalog = target.catalog();
    let plan = InsertPlan::compile(statement, &layout, &space, params, catalog)?;
    // What the table's declarations require of every row, compiled once: the
    // affinities that convert a value on the way in, the `STRICT` type classes,
    // and the `CHECK` predicates. All three were collected by the catalog and
    // used to be consulted by nobody.
    let declarations = WriteDeclarations::compile(
        table,
        &layout,
        &statement.checks,
        &statement.not_null_defaults,
        &statement.index_exprs,
        &space,
        params,
        catalog,
    )?;

    let rows: Vec<Row> = match &statement.source {
        BoundInsertSource::Values(values) => {
            let mut built = Vec::with_capacity(values.len());
            for row in values {
                let mut cells = Vec::with_capacity(row.len());
                for expr in row {
                    let eval = space.compile(expr, params, catalog)?;
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
    // **An `AUTOINCREMENT` table counts up from what it has ever held**, which
    // is the whole of the difference between it and an ordinary rowid table.
    // The mark is read once for the statement and written back once, in the
    // same transaction as the rows, so a rollback takes it with them.
    let sequence_mark = if table.autoincrement {
        let floor = highest_rowid(target, table)?;
        let mark = crate::sequence::read(target, statement.sequence_root, &table.name, floor)?;
        next_rowid = Some(mark.seq);
        Some(mark)
    } else {
        None
    };
    let mut high_water = sequence_mark.as_ref().map_or(0, |mark| mark.seq);
    let mut changes = Changes::default();
    let captured = target.captures(table.root);
    for supplied_row in &rows {
        let image = plan.build_row(
            supplied_row,
            &space,
            &mut next_rowid,
            || highest_rowid(&mut Borrowed(target), table),
            table.autoincrement.then_some(table),
        )?;
        // **`BEFORE` fires on the row as it will be written**, which is where
        // every foreign-key check on the child's side lives: the binder turns
        // `REFERENCES p(id)` into `BEFORE INSERT ... SELECT RAISE(ABORT, ...)
        // WHERE NOT EXISTS (SELECT 1 FROM p WHERE ...)`, so a missing parent is
        // refused here, before anything is written and before the constraint
        // checks below.
        //
        // SQLite leaves `NEW.rowid` undefined in a `BEFORE INSERT` body when
        // the statement supplied no key. This engine hands the allocated one,
        // because the row image is built before it is written and there is no
        // second image to hand instead; a body that reads it therefore sees the
        // number the row is about to get rather than a NULL.
        if trigger::fire(
            &statement.triggers,
            TriggerTime::Before,
            trigger::TriggerRows {
                old: None,
                new: Some(image.as_slice()),
            },
            &layout.slots,
            layout.rowid,
            target,
            params,
            depth,
        )? == trigger::Fired::SkipRow
        {
            continue;
        }
        // **Affinity first, then the constraints.** `NOT NULL`, `STRICT` and
        // `CHECK` all test the value that will actually be stored, and after
        // affinity `'42'` in an `INTEGER` column *is* the integer 42.
        let mut image = image;
        declarations.apply_affinity(&mut image);
        // **The statement's own `OR` algorithm, not the upsert's arm.** A
        // `NOT NULL` or a `CHECK` is not a key collision, and an
        // `ON CONFLICT ... DO NOTHING` says nothing about one: SQLite raises
        // there, and reading the upsert here skipped the row instead - a
        // constraint silently not enforced on the ordinary
        // `INSERT ... ON CONFLICT DO NOTHING`.
        let declared = resolution_of(statement.on_conflict);
        if !declarations_are_met(table, &layout, &declarations, &space, &mut image, declared)? {
            continue;
        }
        declarations.types_are_met(table, &image)?;
        if !declarations.checks_are_met(&space, &image, declared == Resolution::Skip)? {
            continue;
        }
        let Some(stored) = write_one(
            statement,
            &layout,
            &space,
            &plan,
            target,
            image,
            params,
            depth,
            IndexExprs::new(&declarations, &space),
        )?
        else {
            continue;
        };
        if trigger::fire(
            &statement.triggers,
            TriggerTime::After,
            trigger::TriggerRows {
                old: None,
                new: Some(stored.row()),
            },
            &layout.slots,
            layout.rowid,
            target,
            params,
            depth,
        )? == trigger::Fired::SkipRow
        {
            continue;
        }
        count_row(&mut changes, target, depth);
        // **`last_insert_rowid()` moves for an insert, never for an upsert's
        // `DO UPDATE` arm.** See [`Stored`]'s own doc comment: both used to
        // feed the same row image into `changes.last_rowid`, so resolving a
        // conflict onto an existing row reported *that* row's rowid as newly
        // inserted.
        if let Stored::Inserted(row) = &stored {
            if let Some(OwnedDatum::Int(assigned)) = layout.rowid.and_then(|at| row.get(at)) {
                changes.last_rowid = Some(*assigned);
                target.count_rowid(*assigned);
                // A key the statement supplied raises the mark too: `INSERT
                // INTO t VALUES (50, ...)` makes the next allocated key 51.
                high_water = high_water.max(*assigned);
            }
        }
        if captured {
            changes.written.push(stored.row().to_vec());
        }
        if !plan.returning.is_empty() {
            let mut out = Vec::with_capacity(plan.returning.len());
            for eval in &plan.returning {
                out.push(space.evaluate(eval.as_ref(), &[stored.row()])?);
            }
            changes.returned.push(out);
        }
    }
    if let Some(mark) = &sequence_mark {
        if high_water > mark.seq || mark.rowid.is_none() && changes.rows > 0 {
            crate::sequence::write(
                target,
                statement.sequence_root,
                &table.name,
                mark,
                high_water,
            )?;
        }
    }
    Ok(changes)
}

/// Fires a view's `INSTEAD OF INSERT` triggers, storing nothing.
///
/// The row image is the view's columns in declaration order, which is what a
/// trigger body's `NEW.x` resolves against - so the layout handed to the trigger
/// machinery is the identity, and there is no rowid because a view has none.
///
/// @param statement - the bound insert, whose target is the expanded view
/// @param target - the file and its trees
/// @param params - the bound parameters
/// @param supplied - the rows a `SELECT` source produced, empty for `VALUES`
/// @param depth - how many triggers deep this write already is
fn insert_into_view(
    statement: &BoundInsert,
    target: &mut dyn WriteTarget,
    params: &Params,
    supplied: &[Row],
    depth: Depth,
) -> DbResult<Changes> {
    let table = &statement.table;
    let layout = view_layout(table);
    let space = RowSpace::new(&[statement.target_source], &layout);
    let catalog = target.catalog();
    let plan = InsertPlan::compile(statement, &layout, &space, params, catalog)?;
    let rows: Vec<Row> = match &statement.source {
        BoundInsertSource::Values(values) => {
            let mut built = Vec::with_capacity(values.len());
            for row in values {
                let mut cells = Vec::with_capacity(row.len());
                for expr in row {
                    let eval = space.compile(expr, params, catalog)?;
                    cells.push(space.evaluate(eval.as_ref(), &[])?);
                }
                built.push(cells);
            }
            built
        }
        BoundInsertSource::Select(_) => supplied.to_vec(),
    };
    let mut changes = Changes::default();
    let mut never = None;
    for supplied_row in &rows {
        let image = plan.build_row(supplied_row, &space, &mut never, || Ok(0), None)?;
        if trigger::fire(
            &statement.triggers,
            TriggerTime::InsteadOf,
            trigger::TriggerRows {
                old: None,
                new: Some(image.as_slice()),
            },
            &layout.slots,
            layout.rowid,
            target,
            params,
            depth,
        )? == trigger::Fired::SkipRow
        {
            continue;
        }
        count_view_row(&mut changes);
        if !plan.returning.is_empty() {
            let mut out = Vec::with_capacity(plan.returning.len());
            for eval in &plan.returning {
                out.push(space.evaluate(eval.as_ref(), &[image.as_slice()])?);
            }
            changes.returned.push(out);
        }
    }
    Ok(changes)
}

/// Returns the row shape a view's `INSTEAD OF` trigger reads.
///
/// One slot per declared column, in order, and no rowid: a view's row is its
/// result columns and nothing else.
///
/// @param table - the expanded view
pub fn view_layout(table: &TableInfo) -> std::rc::Rc<SourceLayout> {
    std::rc::Rc::new(SourceLayout {
        tree_key: 0,
        slots: (0..table.columns.len()).map(Some).collect(),
        rowid: None,
        // A view's row identifies no stored row, which is the whole reason an
        // `INSTEAD OF` trigger exists.
        identity: Vec::new(),
        types: vec![crate::expr::StaticType::Unknown; table.columns.len()],
        width: table.columns.len(),
        key_columns: Vec::new(),
    })
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
/// @param row - the row image, in tree-column order
#[allow(clippy::too_many_arguments)]
fn write_one(
    statement: &BoundInsert,
    layout: &SourceLayout,
    space: &RowSpace,
    plan: &InsertPlan,
    target: &mut dyn WriteTarget,
    row: Row,
    params: &Params,
    depth: Depth,
    indexes: IndexExprs<'_>,
) -> DbResult<Option<Stored>> {
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
    // **The fast path is only for a statement that really will raise.** The
    // probe has not happened yet, so the only constraint whose clause can be
    // consulted here is the table's own key - which is the only one there is,
    // since this arm is entered only when the table has no `UNIQUE` index. A
    // key declared `ON CONFLICT IGNORE` or `ON CONFLICT REPLACE` has an arm to
    // run and must take the general path below.
    // **And only when no `ON CONFLICT` clause was written at all.** Which arm a
    // conflict selects is decided from the constraint that fired, and that is
    // not known here - so a statement with any clause takes the general path
    // and lets the collision choose. `resolution_for` used to answer this by
    // reading the single arm; with several, there is nothing to read yet.
    if statement.upsert.is_empty()
        && resolution_for(statement, rowid_conflict(table)) == Resolution::Raise
        && unique_indexes(table).next().is_none()
    {
        if place_row_absent(table, layout, target, &row, indexes)? {
            return Ok(Some(Stored::Inserted(row)));
        }
        let clash = conflicting_row(table, layout, target, &row, None, indexes)?;
        let constraint = clash.as_ref().and_then(|found| found.conflict);
        return Err(clash
            .map(|found| found.error)
            .unwrap_or_else(|| {
                let (code, message) = rowid_message(table);
                DbError::new(ExtendedCode(code)).with_message(message)
            })
            .or_unwind(unwind_of(statement.on_conflict.or(constraint))));
    }
    // **Asked again after each deletion**, because one row can collide with a
    // *different* row on each of two unique indexes and `REPLACE` deletes every
    // one of them - which is what `UPDATE OR REPLACE` has always done here and
    // the insert path did not. It terminates: every turn removes a row.
    while let Some(clash) = conflicting_row(table, layout, target, &row, None, indexes)? {
        let arm = matching_arm(statement, &clash.columns);
        match resolution_for_arm(statement, clash.conflict, arm) {
            Resolution::Skip => return Ok(None),
            Resolution::Replace => {
                let Some(held) = read_row(table, target, &clash.key)? else {
                    return Ok(None);
                };
                // **A `REPLACE` that removes a row is a delete, and the keys
                // pointing at that row have to be told.** Written `DELETE`
                // triggers are not fired - that is SQLite's rule with its
                // default `recursive_triggers = off` - so the binder fills
                // these separately and they are only the ones a key implies.
                remove_with_triggers(
                    table,
                    layout,
                    target,
                    &clash.key,
                    &held,
                    &statement.replace_triggers,
                    params,
                    depth,
                    indexes,
                )?;
                continue;
            }
            Resolution::Update => {
                // `None` is the arm's `WHERE` declining, which leaves the row
                // as it is and writes nothing - the same outcome as
                // `DO NOTHING`, and not an error.
                return Ok(upsert_row(
                    statement, table, layout, space, plan, target, &clash, &row, indexes, arm,
                )?
                .map(Stored::Updated));
            }
            // `ABORT`, `FAIL` and `ROLLBACK` all raise here and differ only in
            // how much of what has been written goes back - which this layer
            // does not own and so says rather than does. An untagged error
            // reads as `ABORT`, so the tag is what makes the other two
            // different from it.
            Resolution::Raise => {
                let unwind = unwind_of(statement.on_conflict.or(clash.conflict));
                return Err(clash.error.or_unwind(unwind));
            }
        }
    }
    place_row(table, layout, target, None, &row, indexes)?;
    Ok(Some(Stored::Inserted(row)))
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

/// Returns what an insert does about a conflict a named constraint reported.
///
/// **A constraint carries its own algorithm, and it is not only about the
/// unwind.** `a TEXT UNIQUE ON CONFLICT IGNORE` means every statement that
/// collides on `a` skips the row, with no `OR IGNORE` written anywhere - and
/// this engine used to raise instead, refusing four rows SQLite writes.
///
/// The precedence is SQLite's, innermost clause last: an `ON CONFLICT ... DO
/// UPDATE` beats everything, because it is attached to the *statement* and to a
/// named target; then the statement's own `OR` algorithm; then the constraint's
/// clause; then `ABORT`, which is the default.
///
/// @param statement - the bound insert
/// @param constraint - the clause the constraint that reported the conflict
///   carries, if it carries one
fn resolution_for(statement: &BoundInsert, constraint: Option<ConflictAction>) -> Resolution {
    resolution_for_arm(statement, constraint, None)
}

/// Returns what an insert does about a conflict, given the arm that matched it.
///
/// @param statement - the bound insert
/// @param constraint - the clause the constraint carries, if it carries one
/// @param arm - which `ON CONFLICT` clause matched, when one did
fn resolution_for_arm(
    statement: &BoundInsert,
    constraint: Option<ConflictAction>,
    arm: Option<usize>,
) -> Resolution {
    if let Some(clause) = arm.and_then(|at| statement.upsert.get(at)) {
        return if clause.do_update {
            Resolution::Update
        } else {
            Resolution::Skip
        };
    }
    resolution_of(statement.on_conflict.or(constraint))
}

/// Returns which `ON CONFLICT` clause a conflict selects, if any does.
///
/// The clauses are tried in written order and the first whose target names the
/// constraint that fired wins; a clause with no target matches anything, which
/// is why one may only be written last. A conflict no clause claims falls
/// through to the statement's own `OR` algorithm, exactly as if none had been
/// written - which is what makes `ON CONFLICT(k) DO UPDATE` over a collision on
/// `id` an error rather than an update.
///
/// @param statement - the bound insert
/// @param columns - the columns of the constraint that reported the conflict
fn matching_arm(statement: &BoundInsert, columns: &[u16]) -> Option<usize> {
    statement
        .upsert
        .iter()
        .position(|clause| clause.target.is_empty() || clause.target.as_slice() == columns)
}

/// Returns the arm an algorithm names.
///
/// `ABORT`, `FAIL` and `ROLLBACK` all raise and differ only in how much goes
/// back, which [`unwind_of`] answers.
///
/// @param action - the algorithm in force, if one was written
fn resolution_of(action: Option<ConflictAction>) -> Resolution {
    match action {
        Some(ConflictAction::Ignore) => Resolution::Skip,
        Some(ConflictAction::Replace) => Resolution::Replace,
        _ => Resolution::Raise,
    }
}

/// One `ON CONFLICT` clause, compiled.
///
/// A statement may carry several, and they are tried in written order against
/// the constraint that actually reported the conflict - which is why the target
/// travels with the assignments rather than being resolved once at compile
/// time. `ON CONFLICT(k) DO UPDATE ... ON CONFLICT(id) DO UPDATE ...` runs the
/// second arm when the row collided on `id` and the first when it collided on
/// `k`, and nothing but the collision can decide which.
pub(crate) struct CompiledUpsert {
    /// The assignments, by tree-column slot.
    pub(crate) assignments: Vec<(usize, Box<dyn Eval>)>,
    /// The `WHERE` on the `DO UPDATE`.
    pub(crate) filter: Option<Box<dyn Eval>>,
}

/// A conflict a row would cause, with the error it would report.
struct Conflict {
    /// The key of the row already there.
    key: Vec<OwnedDatum>,
    /// The error an aborting statement reports.
    error: DbError,
    /// The `ON CONFLICT` clause written on the constraint that reported it.
    ///
    /// **Read for the *unwind* and not for the resolution.** A constraint may
    /// say `ON CONFLICT ROLLBACK` or `ON CONFLICT FAIL`, which is `OR ROLLBACK`
    /// and `OR FAIL` written on the constraint instead of on the statement, and
    /// those decide how much of the statement goes back. `ON CONFLICT IGNORE` and `ON CONFLICT REPLACE`
    /// written here decide which *arm* runs instead, and are still ignored:
    /// that is a separate defect with its own ticket, and this field is what it
    /// will read.
    conflict: Option<ConflictAction>,
    /// The columns of the constraint that reported it, sorted.
    ///
    /// **What chooses between several `ON CONFLICT` arms.** An arm names a
    /// conflict target - a set of columns - and runs only when the constraint
    /// that fired is that one. Without this the engine could bind more than one
    /// arm and would still have no way to pick, which is why the old refusal
    /// was at bind time.
    columns: Vec<u16>,
}

/// Returns what a failure resolved this way undoes.
///
/// `IGNORE` and `REPLACE` never reach a failure at all, so they read as the
/// default: an error carrying one of them was raised for some other reason.
///
/// @param action - the conflict algorithm in force, if any was written
fn unwind_of(action: Option<ConflictAction>) -> Unwind {
    match action {
        Some(ConflictAction::Fail) => Unwind::Nothing,
        Some(ConflictAction::Rollback) => Unwind::Transaction,
        _ => Unwind::Statement,
    }
}

/// Returns the `ON CONFLICT` clause the table's own key carries.
///
/// SQLite records `id INTEGER PRIMARY KEY ON CONFLICT REPLACE` as the *column's*
/// clause, because the rowid alias is the column, so this is where a rowid
/// collision's algorithm is written down.
///
/// It is the `PRIMARY KEY`'s clause and not the `NOT NULL`'s. This used to read
/// the wrong one: a rowid collision was resolved by whatever a
/// constraint about missing values happened to say, and by nothing at all in
/// the ordinary case where the column declares no `NOT NULL`.
///
/// @param table - the table being written
fn rowid_conflict(table: &TableInfo) -> Option<ConflictAction> {
    table
        .rowid_alias
        .and_then(|column| table.column(column))
        .and_then(|column| column.primary_key_conflict)
}

/// Returns the row a new row would collide with, if there is one.
///
/// Checks the table's own key first and then every `UNIQUE` index, each through
/// a point probe - the TDD's "`UNIQUE` enforced through `PointProbe`".
///
/// **An `UPDATE` passes the row it is replacing**, and that changes three
/// things, because a row must not collide with itself and cannot always be
/// recognised by the key it is about to have:
///
/// - the table's own key is probed only when it moved, which is what the
///   `UPDATE` path used to decide *on its own* and is the whole of the check it
///   used to do;
/// - an index whose entry did not change is skipped, which is the same test
///   `place_row` uses to decide whether to touch it, so an `UPDATE` of a column
///   no index holds pays two entry builds and no tree work;
/// - a probe that lands on the row being updated is not a conflict, and the
///   remaining indexes are still asked. The comparison is against the row's
///   *before* key: a moved rowid changes an index entry while leaving its
///   prefix alone, so the entry still in the tree carries the old one.
///
/// The last two are what stop the check refusing statements SQLite performs.
/// `UPDATE t SET id = 5 WHERE id = 2` over a table with an untouched
/// `UNIQUE(a)` is legal, and finding that row's own `a` was already being
/// reported as `UNIQUE constraint failed` before this parameter existed.
///
/// @param table - the table being written
/// @param layout - the table tree's layout
/// @param target - the file and its trees
/// @param row - the row about to be written
/// @param replacing - the row's own image before an `UPDATE`, or `None` for an
///   `INSERT`, which has no row of its own to be confused with
fn conflicting_row(
    table: &TableInfo,
    layout: &SourceLayout,
    target: &mut dyn WriteTarget,
    row: &[OwnedDatum],
    replacing: Option<&[OwnedDatum]>,
    indexes: IndexExprs<'_>,
) -> DbResult<Option<Conflict>> {
    let moved = replacing.is_none_or(|before| !same_key(layout, before, row));
    if moved {
        let key = key_of(layout, row);
        if !key.is_empty() && row_exists(table, target, &key)? {
            let (code, message) = rowid_message(table);
            return Ok(Some(Conflict {
                key,
                error: DbError::new(ExtendedCode(code)).with_message(message),
                conflict: rowid_conflict(table),
                // **The table's own key, whichever shape it has.** For a rowid
                // table that is the `INTEGER PRIMARY KEY` when one was declared
                // by name and nothing otherwise - an implicit rowid has no
                // column an `ON CONFLICT` can name. For a `WITHOUT ROWID` table
                // it is the whole primary key, which is what `ON CONFLICT(k)`
                // names there.
                columns: {
                    let mut held = if table.without_rowid {
                        table.primary_key()
                    } else {
                        table.rowid_alias.into_iter().collect()
                    };
                    held.sort_unstable();
                    held
                },
            }));
        }
    }
    for (position, index) in unique_indexes(table) {
        // **A partial unique index constrains only the rows it holds**, so a
        // row its predicate rejects can never clash with anything in it - and
        // this is asked of `row`, the image being probed with, never of
        // `before`: a row *leaving* the index must not be refused by a
        // constraint that no longer holds it.
        if !indexes.holds(position, row)? {
            continue;
        }
        let entry = index_entry(position, index, layout, row, indexes)?;
        // **The entry did not move, so neither did the row's claim on it.**
        //
        // The same question `place_row` asks before touching an index, asked
        // here so an `UPDATE` of a column no unique index holds costs two entry
        // builds and no descent.
        //
        // This was originally written as entry equality alone, with a note at
        // this line that it would need the predicate once partial indexes
        // existed. They exist now, and the note was right: "the entry did not move" is the correct
        // test only while every row is in every index. A row that changed no
        // indexed value but crossed the predicate boundary has an **identical
        // entry and a different claim** - it is entering the index, and
        // entering it can collide. Entry equality alone would skip it before
        // the probe and accept a write SQLite refuses, which is
        // `index.partial.unique.crossing.into` in `semantics.rs`.
        //
        // The guard above already returned for a row the predicate rejects, so
        // reaching here means `holds(row)` is true and the only question left
        // is whether `before` was in the index too.
        if let Some(before) = replacing {
            let held = indexes.holds(position, before)?;
            if held && index_entry(position, index, layout, before, indexes)? == entry {
                continue;
            }
        }
        // A NULL is distinct from every other NULL in a UNIQUE index, which is
        // SQL's rule and the reason a nullable unique column may hold any
        // number of NULLs.
        let Some(prefix) = distinct_prefix(index, &entry) else {
            continue;
        };
        let found = {
            let (database, trees, _) = target.parts_for(index.root)?;
            match trees.get(index.root) {
                Some(tree) => {
                    let borrowed: Vec<Datum<'_>> = prefix.iter().map(OwnedDatum::borrow).collect();
                    tree.point(database.pool(), &borrowed)?
                }
                None => None,
            }
        };
        if let Some(found) = found {
            let key: Vec<OwnedDatum> = found.last().cloned().into_iter().collect();
            // The row finding its own entry. It happens when the rowid moved
            // and the key columns did not, and when a collation makes a changed
            // value probe onto the value it replaced - `COLLATE NOCASE` and
            // `SET a = 'X'` over `'x'`. Neither is a conflict, and neither says
            // anything about the indexes not yet asked.
            //
            // The key is built here rather than up front because building one
            // clones every key value, and the ordinary `UPDATE` never reaches
            // this line: it is paid once per collision, not once per row.
            if replacing.is_some_and(|before| key_of(layout, before) == key) {
                continue;
            }
            let code = if index.origin == IndexOrigin::PrimaryKey {
                codes::PRIMARY_KEY
            } else {
                codes::UNIQUE
            };
            let mut columns: Vec<u16> = index
                .columns
                .iter()
                .filter_map(|column| column.column)
                .collect();
            columns.sort_unstable();
            return Ok(Some(Conflict {
                key,
                error: DbError::new(ExtendedCode(code)).with_message(unique_message(table, index)),
                conflict: index.conflict,
                columns,
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
/// @param clash - the conflict, naming the row already there
/// @param excluded - the row that was being inserted
/// @param arm - which `ON CONFLICT` clause the conflict selected
#[allow(clippy::too_many_arguments)]
fn upsert_row(
    statement: &BoundInsert,
    table: &TableInfo,
    layout: &SourceLayout,
    space: &RowSpace,
    plan: &InsertPlan,
    target: &mut dyn WriteTarget,
    clash: &Conflict,
    excluded: &[OwnedDatum],
    indexes: IndexExprs<'_>,
    arm: Option<usize>,
) -> DbResult<Option<Row>> {
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
    // **The arm's own `WHERE`, tested against the row already there.** SQLite
    // skips the update when it does not hold - the row stays as it is and
    // nothing is raised - and this consulted it nowhere, so
    // `ON CONFLICT(a) DO UPDATE SET n=? WHERE t.n > 500` updated every
    // conflicting row. A silent wrong write, on the statement an application
    // uses precisely to make an update conditional.
    let Some(clause) = arm.and_then(|at| plan.upsert.get(at)) else {
        return Ok(None);
    };
    if let Some(filter) = &clause.filter {
        let verdict = space.evaluate(filter.as_ref(), &[before.as_slice(), excluded])?;
        if crate::expr::truth(&verdict.borrow()) != Some(true) {
            return Ok(None);
        }
    }
    let mut after = before.clone();
    for (slot, eval) in &clause.assignments {
        let value = space.evaluate(eval.as_ref(), &[before.as_slice(), excluded])?;
        if let Some(cell) = after.get_mut(*slot) {
            *cell = value;
        }
    }
    // **The update arm is an update, and had the same hole `UPDATE` did.** The
    // row it writes can collide with a *third* row on another `UNIQUE` index -
    // `ON CONFLICT(a) DO UPDATE SET b = ...` onto a `b` somebody else holds -
    // and it used to be written with no check at all. It raises whatever
    // the statement's own `OR` algorithm says, because SQLite's `DO UPDATE` arm
    // resolves ABORT: `INSERT OR IGNORE` and `INSERT OR REPLACE` both report
    // the constraint here rather than skipping or replacing.
    if let Some(clash) = conflicting_row(table, layout, target, &after, Some(&before), indexes)? {
        let unwind = unwind_of(statement.on_conflict.or(clash.conflict));
        return Err(clash.error.or_unwind(unwind));
    }
    // **`replace_row` on both paths, because the arm can move the key.**
    // `DO UPDATE SET a = 9` over an `INTEGER PRIMARY KEY` is a row that moves,
    // and the unread path wrote the new one with `place_row` and left the old
    // one behind - the table then held both. The stand-in image is
    // enough for `replace_row`: it carries the key the row is moving *from*,
    // which is all a removal needs, and that path has no index to maintain.
    replace_row(table, layout, target, &before, &after, indexes)?;
    Ok(Some(after))
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
        // **Every arm, not one.** Any of them may be the one that runs, so a
        // column left unassigned by any of them has to be carried forward.
        if !plan
            .upsert
            .iter()
            .any(|clause| clause.assignments.iter().any(|(slot, _)| *slot == column))
        {
            return true;
        }
    }
    if statement.upsert.is_empty() {
        return true;
    }
    statement.upsert.iter().any(|clause| {
        // The arm's `WHERE` is about the row already there, so writing one
        // without reading it is not an option when there is a filter to test.
        clause.filter.is_some()
            || clause.assignments.iter().any(|assignment| {
                let mut used = inillucent_sql::bind::ColumnUse::default();
                assignment
                    .value
                    .columns_read(statement.target_source, &mut used);
                used.opaque || !used.columns.is_empty()
            })
    })
}

/// Applies an `UPDATE` to rows a query has already selected.
///
/// @param statement - the bound update
/// @param target - the file and its trees
/// @param params - the bound parameters
/// @param keys - the key of each row the `WHERE` selected
pub fn update(
    statement: &BoundUpdate,
    target: &mut dyn WriteTarget,
    params: &Params,
    keys: &[Row],
) -> DbResult<Changes> {
    update_cached(statement, target, params, keys, &UpdateCache::default())
}

/// Applies an `UPDATE`, reusing the setup a previous execution built.
///
/// @param statement - the bound update
/// @param target - the file and its trees
/// @param params - the bound parameters
/// @param keys - the key of each row the `WHERE` selected
/// @param cache - where the setup is kept between executions
pub fn update_cached(
    statement: &BoundUpdate,
    target: &mut dyn WriteTarget,
    params: &Params,
    keys: &[Row],
    cache: &UpdateCache,
) -> DbResult<Changes> {
    update_at_cached(statement, target, params, keys, Depth::default(), cache)
        .map_err(|error| outer_unwind(error, statement.on_conflict))
}

/// Applies an `UPDATE` that is already some triggers deep.
///
/// @param statement - the bound update
/// @param target - the file and its trees
/// @param params - the bound parameters
/// @param keys - the key of each row the `WHERE` selected
/// @param depth - how many triggers deep this write already is
pub fn update_at(
    statement: &BoundUpdate,
    target: &mut dyn WriteTarget,
    params: &Params,
    keys: &[Row],
    depth: Depth,
) -> DbResult<Changes> {
    update_at_cached(
        statement,
        target,
        params,
        keys,
        depth,
        &UpdateCache::default(),
    )
}

/// Everything an `UPDATE` builds before it looks at a single row.
///
/// **Built once per compiled statement rather than once per execution, which is
/// what the `transaction` family turns on.** `inillucent-execprofile` measured
/// the gate's `txn.large` at 1,896 ns a statement against SQLite's 482, and an
/// `UPDATE` bound to a rowid that matches **nothing** at 950 of those 1,896 - so
/// more than half of every write statement was spent before a row was found,
/// building this. Of its allocations, three were the row space, two the compiled
/// assignment, and one each the declarations, the sources and the correlations.
///
/// None of it depends on the row being written. It depends on the plan, on the
/// table's layout, and - until `Expr::Parameter` existed - on the bound values,
/// which is what made it un-cacheable: `SET note = ?2` compiled the *value* into
/// the expression. A parameter is now read when the expression is evaluated, so
/// what is left is a function of the plan and the layout alone.
pub struct UpdateSetup {
    /// The table's layout, and the identity this setup is only valid for.
    layout: std::rc::Rc<SourceLayout>,
    /// The row images the statement can read.
    space: RowSpace,
    /// The correlated blocks its assignments hold, prepared once.
    correlated: Vec<crate::correlate::Correlation>,
    /// Each assignment's record slot and compiled value.
    assignments: Vec<(usize, Box<dyn Eval>)>,
    /// Where an `UPDATE ... FROM`'s already-evaluated values go.
    projected_slots: Vec<Option<usize>>,
    /// The compiled `RETURNING` expressions.
    projected: Vec<Box<dyn Eval>>,
    /// The affinities, checks, defaults and index expressions.
    declarations: WriteDeclarations,
    /// Whether the statement carries a `FROM`.
    joined: bool,
    /// The cell every `Expr::Parameter` in the compiled pieces reads.
    bindings: crate::physical::Bindings,
    /// Whether anything but a parameter was read while this was built.
    ///
    /// **The same guard `Statement::rebindable` uses, and for the same reason.**
    /// A folded subquery, a folded `changes()` and a folded `now()` are each true
    /// of one execution only, and each of them counts a read. A setup that
    /// counted one is used for the execution that built it and then thrown away.
    reusable: bool,
}

/// Where a compiled `UPDATE` keeps its setup between executions.
///
/// `RefCell` because the compiled statement is shared behind an `Rc` and this is
/// the one part of it that fills in later.
pub type UpdateCache = std::cell::RefCell<Option<std::rc::Rc<UpdateSetup>>>;

/// Runs an `UPDATE`, reusing the setup a previous execution built.
///
/// @param statement - the bound statement
/// @param target - where the writes go
/// @param params - the values bound to `?1`, `?2`, ...
/// @param keys - the rows the plan found
/// @param depth - how many triggers deep this write already is
/// @param cache - where the setup is kept between executions
pub fn update_at_cached(
    statement: &BoundUpdate,
    target: &mut dyn WriteTarget,
    params: &Params,
    keys: &[Row],
    depth: Depth,
    cache: &UpdateCache,
) -> DbResult<Changes> {
    let table = &statement.table;
    if table.kind == TableKind::View {
        return update_view(statement, target, params, keys, depth);
    }
    let layout = layout_of(target, table)?;
    let held = update_setup(cache, statement, &layout, params, target.catalog())?;
    let UpdateSetup {
        space,
        correlated,
        assignments,
        projected_slots,
        projected,
        declarations,
        joined,
        ..
    } = &*held;

    let mut changes = Changes::default();
    let captured = target.captures(table.root);
    for row in keys {
        // An `UPDATE ... FROM` carries its assigned values after the key, so
        // the probe is the key columns and no more.
        let key = row.get(..layout.key_columns.len()).unwrap_or(row);
        // A row an earlier statement in the same transaction removed is skipped
        // rather than resurrected, which is what SQLite does.
        let Some(before) = read_row(table, target, key)? else {
            continue;
        };
        let answers = answer_correlations(correlated, target, params, &before)?;
        let mut after = before.clone();
        if *joined {
            // The values sit after the key columns of the row the keys query
            // produced, in assignment order.
            let width = layout.key_columns.len();
            for (position, slot) in projected_slots.iter().enumerate() {
                let (Some(slot), Some(value)) = (*slot, row.get(width.saturating_add(position)))
                else {
                    continue;
                };
                if let Some(cell) = after.get_mut(slot) {
                    *cell = value.clone();
                }
            }
        }
        for (slot, eval) in assignments {
            // Every assignment reads the *before* image, so `SET a = b, b = a`
            // swaps the two rather than making them equal.
            let value = space.evaluate_with(eval.as_ref(), &[before.as_slice()], &answers)?;
            if let Some(cell) = after.get_mut(*slot) {
                *cell = value;
            }
        }
        // **Affinity is applied before the key is compared**, because the
        // converted value is what the key is built from: `UPDATE t SET id =
        // '7'` moves the row to key 7, not to the text `'7'`.
        declarations.apply_affinity(&mut after);
        // **Every uniqueness the row moved onto, not just the table's own key.**
        //
        // Moving a key moves the row, so the new key has to be free; that much
        // was always checked. What was not is that an `UPDATE` leaving the
        // rowid alone can still collide with *another* row on a secondary
        // `UNIQUE` index - `UPDATE t SET a = 'x'` where some other row already
        // holds `'x'` - and the engine used to perform it, leaving two entries
        // under one key and a table disagreeing with its own constraint.
        // `conflicting_row` knows which row is asking, so it reports neither
        // this row's own key nor an index whose entry did not move.
        //
        // `OR REPLACE` asks again after each deletion, because one image can
        // collide with a *different* row on each of two unique indexes and
        // SQLite deletes both. It terminates: every turn removes a row.
        let mut skipped = false;
        while let Some(clash) = conflicting_row(
            table,
            &layout,
            target,
            &after,
            Some(&before),
            IndexExprs::new(declarations, space),
        )? {
            // The constraint's own clause, when the statement wrote none -
            // `a TEXT UNIQUE ON CONFLICT REPLACE` replaces under a plain
            // `UPDATE` too.
            match resolution_of(statement.on_conflict.or(clash.conflict)) {
                Resolution::Skip => {
                    skipped = true;
                    break;
                }
                Resolution::Replace => {
                    let Some(held) = read_row(table, target, &clash.key)? else {
                        skipped = true;
                        break;
                    };
                    remove_row(
                        table,
                        &layout,
                        target,
                        &clash.key,
                        &held,
                        IndexExprs::new(declarations, space),
                    )?;
                }
                _ => {
                    let unwind = unwind_of(statement.on_conflict.or(clash.conflict));
                    return Err(clash.error.or_unwind(unwind));
                }
            }
        }
        if skipped {
            continue;
        }
        if trigger::fire(
            &statement.triggers,
            TriggerTime::Before,
            trigger::TriggerRows {
                old: Some(before.as_slice()),
                new: Some(after.as_slice()),
            },
            &layout.slots,
            layout.rowid,
            target,
            params,
            depth,
        )? == trigger::Fired::SkipRow
        {
            continue;
        }
        // **Read again only if something could have moved it.** A `BEFORE` body
        // may write the same table, and applying the stale image would put back
        // a row another statement had already changed - so when there are
        // triggers the row is read rather than assumed. When there are none,
        // nothing has run between the first read and here, and the second read
        // was a whole row copied out of the tree and thrown away: `txn.large`
        // is two thousand updates in one transaction and paid for two thousand
        // of them.
        let resolution = resolution_of(statement.on_conflict);
        if !declarations_are_met(table, &layout, declarations, space, &mut after, resolution)? {
            continue;
        }
        declarations.types_are_met(table, &after)?;
        if !declarations.checks_are_met(space, &after, resolution == Resolution::Skip)? {
            continue;
        }
        let reread = if statement.triggers.is_empty() {
            None
        } else {
            match read_row(table, target, key)? {
                Some(row) => Some(row),
                None => continue,
            }
        };
        let current = reread.as_ref().unwrap_or(&before);
        replace_row(
            table,
            &layout,
            target,
            current,
            &after,
            IndexExprs::new(declarations, space),
        )?;
        count_row(&mut changes, target, depth);
        if captured {
            changes.removed.push(before.clone());
            changes.written.push(after.clone());
        }
        if trigger::fire(
            &statement.triggers,
            TriggerTime::After,
            trigger::TriggerRows {
                old: Some(before.as_slice()),
                new: Some(after.as_slice()),
            },
            &layout.slots,
            layout.rowid,
            target,
            params,
            depth,
        )? == trigger::Fired::SkipRow
        {
            continue;
        }
        if !projected.is_empty() {
            let mut out = Vec::with_capacity(projected.len());
            for eval in projected {
                out.push(space.evaluate_with(eval.as_ref(), &[after.as_slice()], &answers)?);
            }
            changes.returned.push(out);
        }
    }
    Ok(changes)
}

/// Prepares the correlated blocks an `UPDATE`'s expressions hold.
///
/// @param statement - the bound update
/// @param layout - the table tree's layout
/// Returns the statement's setup, building it only when it is not already
/// there.
///
/// **The reuse test is the layout's identity and the read counter, and neither
/// is a guess.** A setup describes one table's layout, so it is valid only while
/// the catalog still holds the same one - `Rc::ptr_eq` answers that exactly,
/// where comparing contents would be a second opinion that can go stale. And a
/// setup that folded in something true of one execution counts a read, which is
/// the mechanism `Statement::rebindable` already uses for a chain.
///
/// A reused setup has its bindings pointed at this execution's values, which is
/// the same join-up `Statement::run` does and for the same reason: the compiled
/// assignments read a cell, and the caller hands a different `Params` to every
/// execution.
///
/// @param cache - where the setup is kept between executions
/// @param statement - the bound statement
/// @param layout - the table's layout, as the catalog holds it now
/// @param params - the values bound to `?1`, `?2`, ...
/// @param catalog - where a registered function's body is looked up
fn update_setup(
    cache: &UpdateCache,
    statement: &BoundUpdate,
    layout: &std::rc::Rc<SourceLayout>,
    params: &Params,
    catalog: &dyn TreeCatalog,
) -> DbResult<std::rc::Rc<UpdateSetup>> {
    if let Some(held) = cache.borrow().as_ref() {
        if held.reusable && std::rc::Rc::ptr_eq(&held.layout, layout) {
            adopt_bindings(&held.bindings, params);
            return Ok(std::rc::Rc::clone(held));
        }
    }
    let before = params.reads();
    let built = std::rc::Rc::new(build_update_setup(statement, layout, params, catalog)?);
    if built.reusable {
        *cache.borrow_mut() = Some(std::rc::Rc::clone(&built));
    }
    let _ = before;
    Ok(built)
}

/// Points a compiled expression's parameter cell at this execution's values.
///
/// @param bindings - the cell the compiled pieces read
/// @param params - the values bound for this execution
fn adopt_bindings(bindings: &crate::physical::Bindings, params: &Params) {
    let source = params.bindings();
    // **The same cell needs no copy, and locking it twice would deadlock.** A
    // setup used by the execution that built it holds exactly this `Arc`.
    if std::sync::Arc::ptr_eq(bindings, &source) {
        return;
    }
    let (Ok(from), Ok(mut held)) = (source.lock(), bindings.lock()) else {
        return;
    };
    held.clear();
    held.extend_from_slice(&from);
}

/// Builds everything an `UPDATE` needs before it looks at a row.
///
/// @param statement - the bound statement
/// @param layout - the table's layout
/// @param params - the values bound to `?1`, `?2`, ...
/// @param catalog - where a registered function's body is looked up
fn build_update_setup(
    statement: &BoundUpdate,
    layout: &std::rc::Rc<SourceLayout>,
    params: &Params,
    catalog: &dyn TreeCatalog,
) -> DbResult<UpdateSetup> {
    let before = params.reads();
    // **Only the images the statement can actually read.** A row space costs a
    // layout reference and a batch column per stage, and a statement with no
    // triggers can reach neither `OLD` nor `NEW` - so building them was three
    // times the work for one image's worth of use.
    // **A correlated block in an assignment reads the row being written**, so
    // it is not a constant for the statement and cannot be folded the way an
    // uncorrelated one is. It is prepared once here and answered per row by
    // `crate::correlate` - the same operator a `SELECT` uses, so there is one
    // implementation of what a correlated block means rather than a second in
    // the write path.
    let correlated = update_correlations(statement, layout)?;
    let space = RowSpace::new(&sources_for(statement.source, &statement.triggers), layout)
        .with_correlations(
            &correlated
                .iter()
                .map(|held| held.id)
                .collect::<Vec<usize>>(),
        );
    // **An `UPDATE ... FROM` has already evaluated its values.** They read a
    // row of the joined table, which the write path never sees: the keys query
    // projected them beside the key, so each row handed in is
    // `[key..., value_0, value_1, ...]` and the slot each value goes into is
    // all this needs. An ordinary `UPDATE` compiles its assignments here as it
    // always did.
    let joined = !statement.from.is_empty();
    let mut assignments = Vec::with_capacity(statement.assignments.len());
    let mut projected_slots: Vec<Option<usize>> = Vec::new();
    for assignment in &statement.assignments {
        let slot = layout
            .slots
            .get(usize::from(assignment.column))
            .copied()
            .flatten();
        if joined {
            projected_slots.push(slot);
            continue;
        }
        let Some(slot) = slot else { continue };
        assignments.push((slot, space.compile(&assignment.value, params, catalog)?));
    }
    let mut projected = Vec::with_capacity(statement.returning.len());
    for column in &statement.returning {
        projected.push(space.compile(&column.expr, params, catalog)?);
    }
    let declarations = WriteDeclarations::compile(
        &statement.table,
        layout,
        &statement.checks,
        &statement.not_null_defaults,
        &statement.index_exprs,
        &space,
        params,
        catalog,
    )?;
    Ok(UpdateSetup {
        layout: std::rc::Rc::clone(layout),
        space,
        correlated,
        assignments,
        projected_slots,
        projected,
        declarations,
        joined,
        bindings: params.bindings(),
        reusable: params.reads() == before,
    })
}

fn update_correlations(
    statement: &BoundUpdate,
    layout: &SourceLayout,
) -> DbResult<Vec<crate::correlate::Correlation>> {
    let mut exprs: Vec<&BoundExpr> = statement
        .assignments
        .iter()
        .map(|assignment| &assignment.value)
        .collect();
    exprs.extend(statement.returning.iter().map(|column| &column.expr));
    crate::correlate::correlations_in(&exprs, &row_resolver(statement.source, layout))
}

/// Returns how an outer reference maps onto one row image's tree columns.
///
/// The write path's row space is one image of the target table, so a `NEW.x` or
/// an `a.x` in a correlated block is the tree column the layout puts `x` in.
///
/// @param source - the statement-wide number of the target's FROM term
/// @param layout - the table tree's layout
fn row_resolver(source: usize, layout: &SourceLayout) -> impl Fn(&BoundExpr) -> Option<usize> + '_ {
    move |expr: &BoundExpr| match expr {
        BoundExpr::Column {
            source: held,
            column,
            ..
        } if *held == source => layout.slots.get(usize::from(*column)).copied().flatten(),
        BoundExpr::Rowid { source: held } if *held == source => layout.rowid,
        _ => None,
    }
}

/// Answers every prepared correlated block against one row image.
///
/// @param correlated - the prepared blocks
/// @param target - the file and its trees
/// @param params - the bound parameters
/// @param row - the row image, in tree-column order
fn answer_correlations(
    correlated: &[crate::correlate::Correlation],
    target: &dyn WriteTarget,
    params: &Params,
    row: &[OwnedDatum],
) -> DbResult<Vec<OwnedDatum>> {
    if correlated.is_empty() {
        return Ok(Vec::new());
    }
    let catalog = target.catalog();
    let bare = params.without_subqueries();
    let mut answers = Vec::with_capacity(correlated.len());
    for correlation in correlated {
        answers.push(correlation.answer(catalog, &bare, row)?);
    }
    Ok(answers)
}

/// Applies a `DELETE` to rows a query has already selected.
///
/// @param statement - the bound delete
/// @param target - the file and its trees
/// @param params - the bound parameters
/// @param keys - the key of each row the `WHERE` selected
pub fn delete(
    statement: &BoundDelete,
    target: &mut dyn WriteTarget,
    params: &Params,
    keys: &[Row],
) -> DbResult<Changes> {
    delete_at(statement, target, params, keys, Depth::default())
}

/// Applies a `DELETE` that is already some triggers deep.
///
/// @param statement - the bound delete
/// @param target - the file and its trees
/// @param params - the bound parameters
/// @param keys - the key of each row the `WHERE` selected
/// @param depth - how many triggers deep this write already is
pub fn delete_at(
    statement: &BoundDelete,
    target: &mut dyn WriteTarget,
    params: &Params,
    keys: &[Row],
    depth: Depth,
) -> DbResult<Changes> {
    let table = &statement.table;
    if table.kind == TableKind::View {
        return delete_view(statement, target, params, keys, depth);
    }
    let layout = layout_of(target, table)?;
    let space = RowSpace::new(&sources_for(statement.source, &statement.triggers), &layout);
    let catalog = target.catalog();
    // A delete declares no `CHECK` to meet, but it does have to know which of
    // the table's indexes hold the row it is removing: an entry only comes out
    // of a partial index if the predicate accepted the row, and an index key the
    // table does not carry has to be recomputed to be found.
    let declarations = WriteDeclarations::compile(
        table,
        &layout,
        &[],
        // A `DELETE` writes no value, so no default can stand in for one.
        &[],
        &statement.index_exprs,
        &space,
        params,
        catalog,
    )?;
    let mut projected = Vec::with_capacity(statement.returning.len());
    for column in &statement.returning {
        projected.push(space.compile(&column.expr, params, catalog)?);
    }

    let mut changes = Changes::default();
    let captured = target.captures(table.root);
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
        if remove_with_triggers(
            table,
            &layout,
            target,
            key,
            &row,
            &statement.triggers,
            params,
            depth,
            IndexExprs::new(&declarations, &space),
        )? {
            count_row(&mut changes, target, depth);
            if captured {
                changes.removed.push(row);
            }
        }
    }
    Ok(changes)
}

/// Removes one row, firing the `BEFORE` and `AFTER` triggers around it.
///
/// Returns whether the row was actually removed: a `RAISE(IGNORE)` in a
/// `BEFORE` body abandons it, which is not a failure and not a change.
///
/// The one place a delete happens with triggers around it, so a `DELETE`
/// statement and the delete a `REPLACE` performs to make room cannot fire
/// different things - which they would the moment there were two copies of
/// this.
///
/// @param table - the table being written
/// @param layout - the table tree's layout
/// @param target - the file and its trees
/// @param key - the row's key
/// @param row - the row as it is
/// @param triggers - the triggers this delete fires
/// @param params - the bound parameters
/// @param depth - how many triggers deep this write already is
#[allow(clippy::too_many_arguments)]
fn remove_with_triggers(
    table: &TableInfo,
    layout: &SourceLayout,
    target: &mut dyn WriteTarget,
    key: &[OwnedDatum],
    row: &[OwnedDatum],
    triggers: &[inillucent_sql::dml::BoundTrigger],
    params: &Params,
    depth: Depth,
    indexes: IndexExprs<'_>,
) -> DbResult<bool> {
    if trigger::fire(
        triggers,
        TriggerTime::Before,
        trigger::TriggerRows {
            old: Some(row),
            new: None,
        },
        &layout.slots,
        layout.rowid,
        target,
        params,
        depth,
    )? == trigger::Fired::SkipRow
    {
        return Ok(false);
    }
    // A `BEFORE` body may have removed the row itself - `ON DELETE CASCADE` on
    // a self-referencing key does exactly that - so the removal is skipped
    // rather than repeated when it is already gone.
    if !row_exists(table, target, key)? {
        return Ok(false);
    }
    remove_row(table, layout, target, key, row, indexes)?;
    trigger::fire(
        triggers,
        TriggerTime::After,
        trigger::TriggerRows {
            old: Some(row),
            new: None,
        },
        &layout.slots,
        layout.rowid,
        target,
        params,
        depth,
    )?;
    Ok(true)
}

/// Replaces one row and every index entry that changed with it.
///
/// @param table - the table being written
/// @param layout - the table tree's layout
/// @param target - the file and its trees
/// @param before - the row as it was
/// @param after - the row as it should be
fn replace_row(
    table: &TableInfo,
    layout: &SourceLayout,
    target: &mut dyn WriteTarget,
    before: &[OwnedDatum],
    after: &[OwnedDatum],
    indexes: IndexExprs<'_>,
) -> DbResult<()> {
    if !same_key(layout, before, after) {
        // The row moved, so the old one is a delete and the new one an insert.
        // Doing it as an in-place replace would leave the old key behind.
        remove_row(
            table,
            layout,
            target,
            &key_of(layout, before),
            before,
            indexes,
        )?;
        return place_row(table, layout, target, None, after, indexes);
    }
    place_row(table, layout, target, Some(before), after, indexes)
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
/// @param row - the row to write
fn place_row_absent(
    table: &TableInfo,
    layout: &SourceLayout,
    target: &mut dyn WriteTarget,
    row: &[OwnedDatum],
    indexes: IndexExprs<'_>,
) -> DbResult<bool> {
    let placed = {
        let (database, trees, log) = target.parts_for(table.root)?;
        let tree = trees
            .get_mut(table.root)
            .ok_or_else(|| missing_tree(table))?;
        let borrowed: Vec<Datum<'_>> = row.iter().map(OwnedDatum::borrow).collect();
        tree.put_absent(database, log, &borrowed)?
    };
    if !placed {
        return Ok(false);
    }
    for (position, index) in maintained(table) {
        if !indexes.holds(position, row)? {
            continue;
        }
        let entry = index_entry(position, index, layout, row, indexes)?;
        write_index_entry(index, target, &entry, true)?;
    }
    Ok(true)
}

/// Writes one row and adds its index entries, removing the previous ones.
///
/// @param table - the table being written
/// @param layout - the table tree's layout
/// @param target - the file and its trees
/// @param before - the row this one replaces, when it replaces one
/// @param row - the row to write
fn place_row(
    table: &TableInfo,
    layout: &SourceLayout,
    target: &mut dyn WriteTarget,
    before: Option<&[OwnedDatum]>,
    row: &[OwnedDatum],
    indexes: IndexExprs<'_>,
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
    for (position, index) in maintained(table) {
        // **A partial index is asked about both images.** A row that has moved
        // across the predicate leaves the index or joins it, and a row on the
        // same side of it is maintained as any other row is.
        let holds_after = indexes.holds(position, row)?;
        let after = if holds_after {
            Some(index_entry(position, index, layout, row, indexes)?)
        } else {
            None
        };
        let previous = match before {
            Some(held) if indexes.holds(position, held)? => {
                Some(index_entry(position, index, layout, held, indexes)?)
            }
            _ => None,
        };
        if previous == after {
            continue;
        }
        if let Some(previous) = previous {
            write_index_entry(index, target, &previous, false)?;
        }
        if let Some(after) = after {
            write_index_entry(index, target, &after, true)?;
        }
    }
    let (database, trees, log) = target.parts_for(table.root)?;
    let tree = trees
        .get_mut(table.root)
        .ok_or_else(|| missing_tree(table))?;
    let borrowed: Vec<Datum<'_>> = row.iter().map(OwnedDatum::borrow).collect();
    // **One column changed, so write that column.** `PagedTree` has had an
    // in-place update since the leaf was written - logged, undone and recovered
    // by its own record - and nothing in the write path ever called it: every
    // `UPDATE` went through `put`, which tombstones the row and appends a whole
    // new one to the delta area, so a leaf compacted every `DELTA_LIMIT`
    // updates and the log carried a full row each time. It applies when exactly
    // one non-key column differs and the tree can write it where it lies; when
    // it cannot, `put` is still the answer and nothing has been written.
    if let Some(previous) = before {
        // **A row whose bytes do not change is not written at all.**
        // `only_change` used to answer `None` both when *nothing* differed and
        // when *several* columns did, and the caller then took the most
        // expensive path it has - a tombstone, a delta insert, and a compaction
        // every `DELTA_LIMIT` writes - for the cheapest case there is. Measured
        // with `inillucent-execprofile`, running the same `UPDATE` twice over
        // the same rows: the pass that changed a value cost **1,723 ns and 13.3
        // allocations**, and the pass that wrote back what was already there
        // cost **4,067 ns and 56.7**. An update that changes nothing was 2.4x
        // the price of one that changes something.
        //
        // Nothing observable is skipped. The stored bytes are identical by
        // definition, `count_row` is called by the caller rather than from here
        // so `changes()` still counts the row, the index loop above already
        // skips an entry that did not move, and both trigger times fire from
        // the caller too.
        if matches!(difference(previous, row), Difference::Nothing) {
            return Ok(());
        }
        if let Difference::One(column) = difference(previous, row) {
            if column >= layout.key_columns.len() {
                let key: Vec<Datum<'_>> = layout
                    .key_columns
                    .iter()
                    .filter_map(|held| borrowed.get(*held).copied())
                    .collect();
                let Some(value) = borrowed.get(column).copied() else {
                    return Err(misuse("a changed column is not in the row"));
                };
                if tree.update_in_place(database, log, &key, column, &value, Some(previous))? {
                    return Ok(());
                }
            }
        }
    }
    tree.put(database, log, &borrowed)?;
    Ok(())
}

/// What differs between the row as it was and the row as it will be.
///
/// **Three answers rather than two, because the caller does three different
/// things.** This was `Option<usize>`, which folded "nothing changed" and
/// "several columns changed" into the same `None` - so a statement that changed
/// nothing was written the slowest way there is.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Difference {
    /// No column differs, so the stored bytes would not change.
    Nothing,
    /// Exactly one column differs, at this position.
    One(usize),
    /// More than one column differs, or the two images are different widths.
    Several,
}

/// Returns what differs between two images of a row.
///
/// @param before - the row as it was
/// @param after - the row as it will be
fn difference(before: &[OwnedDatum], after: &[OwnedDatum]) -> Difference {
    if before.len() != after.len() {
        return Difference::Several;
    }
    let mut found = None;
    for (index, (one, two)) in before.iter().zip(after.iter()).enumerate() {
        if one == two {
            continue;
        }
        if found.is_some() {
            return Difference::Several;
        }
        found = Some(index);
    }
    match found {
        Some(index) => Difference::One(index),
        None => Difference::Nothing,
    }
}

/// Removes one row and every index entry that named it.
///
/// @param table - the table being written
/// @param layout - the table tree's layout
/// @param target - the file and its trees
/// @param key - the row's key
/// @param row - the row as it stands, which the caller has already read
fn remove_row(
    table: &TableInfo,
    layout: &SourceLayout,
    target: &mut dyn WriteTarget,
    key: &[OwnedDatum],
    row: &[OwnedDatum],
    indexes: IndexExprs<'_>,
) -> DbResult<()> {
    for (position, index) in maintained(table) {
        if !indexes.holds(position, row)? {
            continue;
        }
        let entry = index_entry(position, index, layout, row, indexes)?;
        write_index_entry(index, target, &entry, false)?;
    }
    let (database, trees, log) = target.parts_for(table.root)?;
    let tree = trees
        .get_mut(table.root)
        .ok_or_else(|| missing_tree(table))?;
    let borrowed: Vec<Datum<'_>> = key.iter().map(OwnedDatum::borrow).collect();
    tree.delete(database, log, &borrowed)?;
    Ok(())
}

/// Returns the indexes of a table that the write path maintains, with their
/// positions.
///
/// A `WITHOUT ROWID` table's primary-key index *is* the table: one b-tree,
/// reported at the table's own root. Maintaining it separately would write
/// every row twice.
///
/// The position travels with the index because a partial index's predicate and
/// an expression key are compiled per index and found by it - see
/// [`IndexExprs`].
///
/// @param table - the table
fn maintained(table: &TableInfo) -> impl Iterator<Item = (usize, &IndexInfo)> {
    table
        .indexes
        .iter()
        .enumerate()
        .filter(|(_, index)| index.root != 0 && index.root != table.root)
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
/// @param entry - the entry: the keys, then the rowid
/// @param adding - true to add it, false to remove it
fn write_index_entry(
    index: &IndexInfo,
    target: &mut dyn WriteTarget,
    entry: &[OwnedDatum],
    adding: bool,
) -> DbResult<()> {
    let (database, trees, log) = target.parts_for(index.root)?;
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
/// An index entry is the indexed columns followed by whatever identifies the
/// table row - a rowid for an ordinary table, and the primary key's columns for
/// a `WITHOUT ROWID` one. That is what the import builds, what `index_shape`
/// describes, and what every read of an index assumes.
///
/// @param index - the index
/// @param layout - the table tree's layout
/// @param row - the table row, in tree-column order
fn index_entry(
    position: usize,
    index: &IndexInfo,
    layout: &SourceLayout,
    row: &[OwnedDatum],
    indexes: IndexExprs<'_>,
) -> DbResult<Row> {
    let trailing = layout.identity.len().max(1);
    let mut entry = Vec::with_capacity(index.columns.len().saturating_add(trailing));
    for (key, column) in index.columns.iter().enumerate() {
        // A key the index computes is evaluated over the row; a key that is a
        // column is read out of it. `key` answers `None` for every index that
        // computes nothing, which is every index the gate measures.
        if let Some(computed) = indexes.key(position, key, row)? {
            entry.push(computed);
            continue;
        }
        entry.push(
            column
                .column
                .and_then(|declared| layout.slots.get(usize::from(declared)).copied().flatten())
                .and_then(|slot| row.get(slot).cloned())
                .unwrap_or(OwnedDatum::Null),
        );
    }
    if layout.identity.is_empty() {
        entry.push(
            layout
                .rowid
                .and_then(|slot| row.get(slot).cloned())
                .unwrap_or(OwnedDatum::Null),
        );
        return Ok(entry);
    }
    for slot in &layout.identity {
        entry.push(row.get(*slot).cloned().unwrap_or(OwnedDatum::Null));
    }
    Ok(entry)
}

/// Returns the unique indexes of a table that are trees of their own.
///
/// **Newest first, because that is the one SQLite names.** When a row collides
/// on two unique indexes at once only one of them can be reported, and SQLite
/// reports the last one declared: it links each new index onto the *head* of
/// the table's list and walks the list in order, so a schema read back off disk
/// is in reverse declaration order. `CREATE UNIQUE INDEX u1 ON t(a)` then
/// `u2 ON t(b)`, and a row taking both, answers `UNIQUE constraint failed: t.b`
/// - and `t.a` when the two are declared the other way round. Iterating
/// declaration-first named the wrong constraint on `INSERT` as well as
/// `UPDATE`, and the message is the part an application matches on.
///
/// The table's own key is not in here and does not need to be: both engines
/// check it before any index.
///
/// @param table - the table
fn unique_indexes(table: &TableInfo) -> impl Iterator<Item = (usize, &IndexInfo)> {
    table
        .indexes
        .iter()
        // **`enumerate` before `rev`, and the order matters more than it
        // looks.** The reversal makes the constraint named on a
        // collision the last-declared index, which is what SQLite reports.
        // The enumeration was added later, once partial indexes existed, so
        // each index carries its position in `table.indexes` - the number the
        // binder used when it bound the
        // partial predicates, and the number `IndexExprs` looks them up by.
        // Reversing first would renumber them, and `holds` would then consult
        // **another index's** predicate: silently, and only on a table with two
        // or more unique indexes where one is partial.
        // `index.partial.unique.two.indexes` is that table.
        .enumerate()
        .rev()
        .filter(|(_, index)| index.unique && index.root != 0 && index.root != table.root)
}

/// Returns an index entry's key prefix, or `None` when a NULL makes it distinct.
///
/// @param index - the index
/// @param entry - the entry: the keys, then the rowid
fn distinct_prefix(index: &IndexInfo, entry: &[OwnedDatum]) -> Option<Vec<OwnedDatum>> {
    let prefix: Vec<OwnedDatum> = entry.iter().take(index.columns.len()).cloned().collect();
    if prefix.is_empty() || prefix.contains(&OwnedDatum::Null) {
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

/// Refuses a row a column's declaration does not allow.
///
/// **The engine used to accept one - an attempt to write a vector into a
/// typed column found nothing checking anything.** `INSERT INTO t(a) VALUES
/// (NULL)` on `a INTEGER NOT NULL` stored the NULL and answered success, which
/// is the third silent wrong answer of the kind Part 1 was about and the worst
/// of them: an application that declares a column `NOT NULL` and reads it back
/// without checking is exactly the application the declaration is for.
///
/// `Ok(false)` means the statement said `OR IGNORE` and the row is skipped;
/// `Ok(true)` means every `NOT NULL` column has a value. The order matters and
/// is SQLite's: `BEFORE` bodies run first, because one of them may be what
/// supplies the value, and the constraint is checked on the image that is about
/// to be written.
///
/// A rowid alias is not checked here even when it is declared `NOT NULL`: the
/// row image carries the key the statement is about to allocate, and SQLite
/// fills one in for the same reason.
///
/// **`REPLACE` fills a missing value in rather than refusing it.** SQLite's
/// rule for a `NOT NULL` violation resolved as `REPLACE` is to store the
/// column's `DEFAULT`, and to fall back to `ABORT` only when the column
/// declares none - so `UPDATE OR REPLACE t SET c = NULL` on
/// `c TEXT NOT NULL DEFAULT 'd'` stores `'d'`, where this engine used to raise.
/// That is why the row is taken by reference *mutably*: the check
/// is also the place the substitution happens, since it is the only place that
/// knows which column was empty.
///
/// @param table - the table being written
/// @param layout - the table tree's layout
/// @param declarations - the table's compiled declarations, which carry the
///   defaults a `REPLACE` may substitute
/// @param space - the statement's row space, which evaluates one
/// @param row - the image about to be written, filled in place
/// @param resolution - what the statement said to do with a conflict
fn declarations_are_met(
    table: &TableInfo,
    layout: &SourceLayout,
    declarations: &WriteDeclarations,
    space: &RowSpace,
    row: &mut [OwnedDatum],
    resolution: Resolution,
) -> DbResult<bool> {
    for (position, column) in table.columns.iter().enumerate() {
        let Some(slot) = layout.slots.get(position).copied().flatten() else {
            continue;
        };
        let value = row.get(slot).cloned();
        // **A `VECTOR(N)` column holds N floats or nothing.** The width is the
        // only thing the declaration promises that the storage does not already
        // enforce, and a vector of the wrong width is not a slow query, it is a
        // distance that silently answers NULL for ever.
        if let Some(width) = column.vector_dimensions() {
            let wrong = match &value {
                Some(OwnedDatum::Blob(bytes)) => bytes.len() != width.saturating_mul(4),
                Some(OwnedDatum::Null) | None => false,
                _ => true,
            };
            if wrong {
                return Err(
                    DbError::new(ExtendedCode(codes::DATATYPE)).with_message(format!(
                        "cannot store this value in {}.{}: it is not a vector of {} dimensions",
                        String::from_utf8_lossy(&table.name),
                        String::from_utf8_lossy(&column.name),
                        width
                    )),
                );
            }
        }
        if !column.not_null || Some(position as u16) == table.rowid_alias {
            continue;
        }
        if !matches!(value, Some(OwnedDatum::Null) | None) {
            continue;
        }
        // The constraint carries its own `ON CONFLICT`, and the statement may
        // override it: `INSERT OR IGNORE` beats `NOT NULL ON CONFLICT ABORT`.
        let action = match resolution {
            Resolution::Skip => Some(ConflictAction::Ignore),
            Resolution::Replace => Some(ConflictAction::Replace),
            // An upsert's `DO UPDATE` is about a *key* collision, not about a
            // missing value, so a NULL still fails the constraint the way a
            // plain insert's would.
            Resolution::Update | Resolution::Raise => column.not_null_conflict,
        };
        if matches!(action, Some(ConflictAction::Ignore)) {
            return Ok(false);
        }
        // The default stands in, and the loop carries on to the next column -
        // `UPDATE OR REPLACE t SET c = NULL, e = NULL` fills both.
        if matches!(action, Some(ConflictAction::Replace))
            && declarations.stand_in_default(space, row, slot)?
        {
            continue;
        }
        return Err(DbError::new(ExtendedCode(codes::NOT_NULL))
            .with_message(format!(
                "NOT NULL constraint failed: {}.{}",
                String::from_utf8_lossy(&table.name),
                String::from_utf8_lossy(&column.name)
            ))
            // `action` is already the statement's clause or, failing that,
            // the column's own - so `NOT NULL ON CONFLICT ROLLBACK` rolls
            // back and `INSERT OR FAIL` into the same column does not.
            .or_unwind(unwind_of(action)));
    }
    Ok(true)
}

/// Fires a view's `INSTEAD OF UPDATE` triggers, storing nothing.
///
/// The rows handed in are the view's own, which is what `OLD` is; `NEW` is the
/// same row with the statement's assignments applied. Nothing is written,
/// because a view has nowhere to write to - the trigger body is the write.
///
/// @param statement - the bound update, whose target is the view
/// @param target - the file and its trees
/// @param params - the bound parameters
/// @param rows - the view's rows, as its own query produced them
/// @param depth - how many triggers deep this write already is
fn update_view(
    statement: &BoundUpdate,
    target: &mut dyn WriteTarget,
    params: &Params,
    rows: &[Row],
    depth: Depth,
) -> DbResult<Changes> {
    let table = &statement.table;
    let layout = view_layout(table);
    let space = RowSpace::new(&sources_for(statement.source, &statement.triggers), &layout);
    let catalog = target.catalog();
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
        assignments.push((slot, space.compile(&assignment.value, params, catalog)?));
    }
    let mut changes = Changes::default();
    for before in rows {
        let mut after = before.clone();
        for (slot, eval) in &assignments {
            let value = space.evaluate(eval.as_ref(), &[before.as_slice()])?;
            if let Some(cell) = after.get_mut(*slot) {
                *cell = value;
            }
        }
        if trigger::fire(
            &statement.triggers,
            TriggerTime::InsteadOf,
            trigger::TriggerRows {
                old: Some(before.as_slice()),
                new: Some(after.as_slice()),
            },
            &layout.slots,
            layout.rowid,
            target,
            params,
            depth,
        )? == trigger::Fired::SkipRow
        {
            continue;
        }
        count_view_row(&mut changes);
    }
    Ok(changes)
}

/// Fires a view's `INSTEAD OF DELETE` triggers, storing nothing.
///
/// @param statement - the bound delete, whose target is the view
/// @param target - the file and its trees
/// @param params - the bound parameters
/// @param rows - the view's rows, as its own query produced them
/// @param depth - how many triggers deep this write already is
fn delete_view(
    statement: &BoundDelete,
    target: &mut dyn WriteTarget,
    params: &Params,
    rows: &[Row],
    depth: Depth,
) -> DbResult<Changes> {
    let layout = view_layout(&statement.table);
    let mut changes = Changes::default();
    for before in rows {
        if trigger::fire(
            &statement.triggers,
            TriggerTime::InsteadOf,
            trigger::TriggerRows {
                old: Some(before.as_slice()),
                new: None,
            },
            &layout.slots,
            layout.rowid,
            target,
            params,
            depth,
        )? == trigger::Fired::SkipRow
        {
            continue;
        }
        count_view_row(&mut changes);
    }
    Ok(changes)
}

/// Reports whether two images of a row have the same key.
///
/// **Without building either key.** `key_of` clones every key value into a new
/// vector, and asking "did the key change" by building two of them and
/// comparing was two allocations and a clone per key column on every `UPDATE` -
/// twice, because `replace_row` asked the same question again. The gate's
/// `txn.large` is two thousand updates in one transaction and paid for all of
/// it.
///
/// @param layout - the table tree's layout
/// @param before - the row as it was
/// @param after - the row as it will be
fn same_key(layout: &SourceLayout, before: &[OwnedDatum], after: &[OwnedDatum]) -> bool {
    layout
        .key_columns
        .iter()
        .all(|column| before.get(*column) == after.get(*column))
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
    let (database, trees, _) = target.parts_for(table.root)?;
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
    let (database, trees, _) = target.parts_for(table.root)?;
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
fn layout_of(target: &dyn WriteTarget, table: &TableInfo) -> DbResult<std::rc::Rc<SourceLayout>> {
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
    fn parts_for(
        &mut self,
        root: u32,
    ) -> DbResult<(&mut Database, &mut dyn Trees, &mut dyn TreeLog)> {
        self.0.parts_for(root)
    }

    fn layout(&self, root: u32) -> Option<&std::rc::Rc<SourceLayout>> {
        self.0.layout(root)
    }

    fn catalog(&self) -> &dyn crate::physical::TreeCatalog {
        self.0.catalog()
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
    let (database, trees, _) = target.parts_for(table.root)?;
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
/// **The key column and nothing else.** This used to call `LeafRef::live`, which
/// materialises every column of every live row in the leaf, and then read the
/// first value of the last one. That is wrong twice over.
///
/// It is wrong for correctness, because a value wider than an eighth of a page
/// is held in a blob extent and a leaf answers for one only after the tree has
/// read the extents in. `live` does not, so it refuses with "this value is
/// stored out of line", and the refusal reaches the caller as a failed INSERT.
/// Every insert into a table whose primary key is not INTEGER generates a rowid
/// and so comes through here, which made a table with a TEXT primary key unable
/// to hold a second wide row at all. That is how `inillucent migrate` failed on
/// the first table of a real 5.8 GB PostgreSQL database, where
/// `attachment.id` is a UUID and `attachment.extracted_text` reaches 1.9 MB.
///
/// It is wrong for cost as well: reading a leaf of nine hundred rows to look at
/// one integer is nine hundred rows of decoding per insert, and where a value is
/// out of line it is also a page read per value.
///
/// `visit_live` performs the same merge of the sorted region and the delta area
/// while reading only the columns asked for, and a key column is never out of
/// line, so asking for column 0 alone is both correct and cheap. The rows it
/// visits are not in key order, so this takes the maximum rather than the last.
///
/// @param tree - the tree
/// @param pool - the buffer pool
fn largest_key(tree: &PagedTree, pool: &Pool) -> DbResult<i64> {
    let key = tree.encode_key(&[Datum::Int(i64::MAX)]);
    let (guard, _) = tree.descend_guard(pool, &key)?;
    let leaf = LeafRef::parse(&guard)?
        .with_collations(tree.collations())
        .with_directions(tree.directions());
    let mut largest: Option<i64> = None;
    leaf.visit_live(&[0], &mut |values| {
        if let Some(Datum::Int(number)) = values.first() {
            largest = Some(match largest {
                Some(held) if held >= *number => held,
                _ => *number,
            });
        }
        Ok(())
    })?;
    Ok(largest.unwrap_or(0))
}
