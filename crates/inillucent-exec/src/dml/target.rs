//! Where a write puts its rows, and what it counts while it does.
//!
//! Invariant: **a write never names a tree directly.** It is handed a
//! [`WriteTarget`], which is what makes the same insert path work against a
//! live database, a transaction that may still roll back, and the recovery
//! applier - three callers that agree about rows and disagree about
//! everything else.

use std::collections::{BTreeMap, HashMap};

use inillucent_base::error::misuse;
use inillucent_base::{DbError, DbResult};
use inillucent_pool::{Database, Pool};
use inillucent_sql::bind::{BoundExpr, NEW_SOURCE, OLD_SOURCE};
use inillucent_sql::catalog_view::TableInfo;
use inillucent_tree::datum::{Datum, OwnedDatum};
use inillucent_tree::leaf::LeafRef;
use inillucent_tree::write::TreeLog;
use inillucent_tree::PagedTree;
use inillucent_value::collation::Collation;

use super::*;
use crate::batch::{Batch, Vector};
use crate::expr::{compile, Eval};
use crate::physical::{
    translate_scan, AccessKind, HeldSpace, Params, PreparedStage, SourceLayout, TreeCatalog,
};
use crate::trigger::Depth;

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
pub(crate) enum Stored {
    /// A new row was placed in the tree.
    Inserted(Row),
    /// An existing row was rewritten in place, by an `ON CONFLICT ... DO
    /// UPDATE` arm.
    Updated(Row),
}
impl Stored {
    /// Returns the row, however it got there.
    pub(crate) fn row(&self) -> &[OwnedDatum] {
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
pub(crate) fn count_row(changes: &mut Changes, target: &dyn WriteTarget, depth: Depth) {
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
pub(crate) fn count_view_row(changes: &mut Changes) {
    changes.rows = changes.rows.saturating_add(1);
}
/// A map from root page to tree, whichever map the caller happens to hold.
///
/// The write path needs a mutable tree and a mutable [`Database`] at the same
/// instant, which no single accessor can hand out. `WriteTarget::parts`
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

    /// Takes a trigger body's write to a virtual table, to be made after the
    /// write this target is holding.
    ///
    /// **A module's write needs the connection, and a write has split the
    /// connection apart.** The module is registered on the connection and its
    /// shadow tables are written through a log the connection builds, while
    /// this target holds the trees mutably for the statement that fired the
    /// trigger. So the body's statement is kept, with its `OLD` and `NEW`
    /// already substituted, and the engine runs it once the trees are handed
    /// back, inside the same transaction, before the commit. A failure there
    /// undoes the statement that fired the trigger, as a failure inside the
    /// body would. This is how `CREATE TRIGGER ... AFTER INSERT ON todo BEGIN
    /// INSERT INTO f (rowid, title) VALUES (NEW.id, NEW.title); END`, the
    /// standard way to keep an FTS5 table in step with its content table,
    /// works.
    ///
    /// The default refuses, for a target that has no connection to hand the
    /// write to.
    ///
    /// @param write - the substituted statement and what it needs to run
    fn defer_module_write(&mut self, write: ModuleWrite) -> DbResult<()> {
        let _ = write;
        crate::physical::unsupported("a trigger body that writes a virtual table, from here")
    }
}

/// A trigger body's write to a virtual table, held until the trees are free.
///
/// See [`WriteTarget::defer_module_write`].
#[derive(Debug)]
pub enum ModuleWrite {
    /// An `INSERT`, with the rows a `SELECT` source already produced.
    Insert {
        /// The statement, with `OLD` and `NEW` substituted.
        statement: inillucent_sql::dml::BoundInsert,
        /// The values it reads.
        params: crate::physical::Params,
        /// The rows of an `INSERT ... SELECT`, read inside the write.
        selected: Option<Vec<Vec<inillucent_tree::datum::OwnedDatum>>>,
    },
    /// An `UPDATE`, whose rows are found when it runs.
    Update {
        /// The statement, with `OLD` and `NEW` substituted.
        statement: inillucent_sql::dml::BoundUpdate,
        /// The values it reads.
        params: crate::physical::Params,
    },
    /// A `DELETE`, whose rows are found when it runs.
    Delete {
        /// The statement, with `OLD` and `NEW` substituted.
        statement: inillucent_sql::dml::BoundDelete,
        /// The values it reads.
        params: crate::physical::Params,
    },
}
/// The synthetic column space a write's expressions are translated against.
///
/// A statement's stages are not FROM terms here - they are *row images*: the
/// row being written, the row already there, the `excluded` row of an upsert, a
/// trigger's `OLD` and `NEW`. Each is one stage of the same table's layout at
/// its own offset, so `translate_scan` resolves a bound column of any of them
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

    /// Compiles one bound expression against this space.
    ///
    /// **The catalog is a parameter, not a field.** A `RowSpace` is carried
    /// through every write-path signature that builds or evaluates one, and
    /// giving it a borrowed catalog of its own would put that borrow's
    /// lifetime on all of them - which is why a registered function used to
    /// refuse by name here (`docs/roadmap.md` item 13): the physical pass
    /// resolves a registered function's body through the catalog, and this
    /// space carried none. Every caller already holds a [`WriteTarget`], and
    /// [`WriteTarget::catalog`] is exactly the view `translate_scan` needs -
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
pub(crate) fn key_columns(
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
pub(crate) fn sources_for(
    source: usize,
    triggers: &[inillucent_sql::dml::BoundTrigger],
) -> Vec<usize> {
    if triggers.is_empty() {
        return vec![source];
    }
    vec![source, OLD_SOURCE, NEW_SOURCE]
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
pub(crate) fn row_exists(
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
pub(crate) fn read_row(
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
pub(crate) fn layout_of(
    target: &dyn WriteTarget,
    table: &TableInfo,
) -> DbResult<std::rc::Rc<SourceLayout>> {
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
pub(crate) fn missing_tree(table: &TableInfo) -> DbError {
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
pub(crate) struct Borrowed<'a>(pub(crate) &'a mut dyn WriteTarget);
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
pub(crate) fn highest_rowid(target: &mut dyn WriteTarget, table: &TableInfo) -> DbResult<i64> {
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

/// What every row a statement writes is written under.
///
/// **A type rather than four arguments repeated across three functions
/// (task-1962, A9).** `write_one` took nine, `remove_with_triggers` nine and
/// `upsert_row` ten, and four of them were the same four in the same order.
/// None of them changes as a write descends into a trigger body, which is why
/// this is passed on rather than rebuilt.
#[derive(Clone, Copy)]
pub(crate) struct WriteRequest<'a> {
    /// The record layout of the table's rows.
    pub(crate) layout: &'a SourceLayout,
    /// The values bound to `?1`, `?2`, ...
    pub(crate) params: &'a Params,
    /// How deep in a trigger body this write already is.
    pub(crate) depth: Depth,
    /// The compiled expressions of any expression index on the table.
    pub(crate) indexes: IndexExprs<'a>,
}

/// What an upsert found, and which arm answers it.
///
/// **The three arguments that are only an upsert's (task-1962, A9).**
/// `upsert_row` took ten; these three plus a [`WriteRequest`] are what is left
/// of them.
pub(crate) struct Upsert<'a> {
    /// The conflict the insert ran into.
    pub(crate) clash: &'a super::conflict::Conflict,
    /// The row the statement tried to insert, which the arm reads as
    /// `excluded`.
    pub(crate) excluded: &'a [OwnedDatum],
    /// Which `ON CONFLICT` arm matched, when the statement has several.
    pub(crate) arm: Option<usize>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dml::testing::a_table;

    /// A view's row identifies no stored row.
    ///
    /// **Which is the whole reason an `INSTEAD OF` trigger exists (T3,
    /// task-1962).** A view has no tree and no rowid, so a write to one has
    /// nothing to address; the layout says so with an empty `identity` rather
    /// than with a rowid that names nothing.
    #[test]
    fn a_view_s_layout_identifies_no_row() {
        let view = a_table("v", &["a", "b", "c"]);
        let layout = view_layout(&view);
        assert_eq!(layout.width, 3);
        assert_eq!(layout.slots, vec![Some(0), Some(1), Some(2)]);
        assert_eq!(layout.rowid, None, "a view has no rowid");
        assert!(
            layout.identity.is_empty(),
            "and no stored row to identify, which is what an INSTEAD OF trigger is for"
        );
        assert!(
            layout.key_columns.is_empty(),
            "so there is no key to seek it by either"
        );
    }

    /// A missing tree is reported by the table's own name.
    ///
    /// The message reaches an application, and "no tree imported" without the
    /// name is a message nobody can act on.
    #[test]
    fn a_missing_tree_names_the_table() {
        let table = a_table("orders", &["id"]);
        let error = missing_tree(&table);
        assert!(
            error.detail().is_some_and(|said| said.contains("orders")),
            "the refusal should name the table; it said {:?}",
            error.detail()
        );
    }
}
