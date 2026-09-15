//! The trigger firing point, which is also the foreign-key mechanism.
//!
//! Invariant: there is exactly one implementation of "a write causes another
//! write". `inillucent-sql/src/foreign_key.rs` turns every `REFERENCES` clause
//! into `CREATE TRIGGER` text, parses it with the same parser and binds it with
//! the same binder, so `ON DELETE CASCADE` and the `DELETE` somebody wrote by
//! hand cannot disagree about what `OLD` means or what order things happen in.
//! Building a second, direct foreign-key path in the write would be two
//! implementations that agree today - which is the thing that file's own
//! doc comment warns against, and the thing this module exists to avoid.
//!
//! So enforcement is this: the binder already fills `triggers` on every
//! [`BoundInsert`], `BoundUpdate` and [`BoundDelete`], from written triggers
//! and from `TableInfo::foreign_key_triggers` alike, and the only piece that
//! was missing is a place that runs one.
//!
//! ## Where it fires, and why there
//!
//! Inside `dml::insert`, `dml::update` and `dml::delete`, around the apply of
//! each row - because that is the only place where **both** row images exist at
//! once. `OLD` is what the tree holds and `NEW` is what is about to replace it;
//! a firing point above the loop would have neither, and one below it would
//! have lost `OLD`.
//!
//! | event | `OLD` | `NEW` |
//! |---|---|---|
//! | `INSERT` | absent | the row being written |
//! | `UPDATE` | the row as it was | the row as it will be |
//! | `DELETE` | the row being removed | absent |
//!
//! `BEFORE` fires on the supplied row before it is applied, `AFTER` on the row
//! as written. A `RAISE(ABORT)` in either is an ordinary error that unwinds
//! through the undo buffer the transaction already keeps, which is what makes
//! `INSERT INTO child VALUES (11, 'zz')` with no parent report
//! `FOREIGN KEY constraint failed` and change nothing.
//!
//! ## How a body runs
//!
//! A body is bound once, against the firing statement, and reads `OLD` and
//! `NEW` through two sentinel source numbers. At the moment it fires both rows
//! are values this module is holding, so the body is copied and every `OLD` and
//! `NEW` read in it is replaced by the value itself
//! ([`inillucent_sql::rewrite`]). What is left is an ordinary statement with no
//! external references, which the ordinary planner plans and the ordinary write
//! path applies - one execution path for a trigger body and a typed statement
//! alike.
//!
//! ## What a statement with no triggers pays
//!
//! Nothing measurable, and that is a requirement rather than a hope: the gate's
//! `write` and `transaction` families run against fixtures with no triggers at
//! all, and their weighted ratio carries the headline. [`fire`] returns on an
//! empty slice before it reads a row image, and `dml`'s callers ask
//! `triggers.is_empty()` before they build one.

use inillucent_base::error::misuse;
use inillucent_base::DbResult;
use inillucent_sql::ast::TriggerTime;
use inillucent_sql::bind::{BoundExpr, BoundSelect, NEW_SOURCE, OLD_SOURCE};
use inillucent_sql::dml::{BoundDelete, BoundInsert, BoundTrigger, BoundTriggerStatement};
use inillucent_sql::plan::{plan_select_with, Levers};
use inillucent_tree::datum::OwnedDatum;

use crate::dml::{self, Row, WriteTarget};
use crate::expr::RAISE_IGNORE;
use crate::physical::{self, Params};

/// The two row images a firing trigger can read.
///
/// Both are optional because an `INSERT` has no `OLD` and a `DELETE` has no
/// `NEW`, and reading the absent one is SQL's `NULL` rather than an error -
/// which is what SQLite does and what a binder cannot check, since
/// `CREATE TRIGGER ... INSERT OR UPDATE` may be written for both events.
///
/// The rows are in **tree-column order**, the same order every other image in
/// the write path is in.
#[derive(Clone, Copy, Default)]
pub struct TriggerRows<'a> {
    /// The row as it was, for an `UPDATE` or a `DELETE`.
    pub old: Option<&'a [OwnedDatum]>,
    /// The row as it will be, for an `INSERT` or an `UPDATE`.
    pub new: Option<&'a [OwnedDatum]>,
}

/// What one firing of a trigger sees, apart from the trigger itself.
///
/// **A type rather than seven of eight arguments (task-1962, A9).** [`fire`] and
/// `run_body` took the same seven, in the same order, and both carried
/// `#[allow(clippy::too_many_arguments)]` to say so. Two of them are
/// `Option<usize>` and `Depth`, which a caller can swap without the compiler
/// noticing.
#[derive(Clone, Copy)]
pub struct TriggerFiring<'a> {
    /// The row images the body reads as `OLD` and `NEW`.
    pub rows: TriggerRows<'a>,
    /// Which record slot each of the table's columns is at.
    pub slots: &'a [Option<usize>],
    /// Which slot holds the rowid, when the row carries one.
    pub rowid: Option<usize>,
    /// The values bound to `?1`, `?2`, ...
    pub params: &'a Params,
    /// How deep this firing already is.
    pub depth: Depth,
}

/// What a fired trigger asked the write to do next.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Fired {
    /// Carry on with the row.
    Continue,
    /// Abandon the row without failing, which is what `RAISE(IGNORE)` means.
    SkipRow,
}

/// The state one statement's firing carries between its rows.
///
/// Depth is on it rather than on a global because a trigger's body is a
/// statement like any other and may fire again; carrying the count down the
/// call chain is what makes the cap describe the actual nesting rather than a
/// process-wide total.
#[derive(Clone, Copy, Debug, Default)]
pub struct Depth(pub usize);

impl Depth {
    /// Returns the depth one level further in.
    ///
    /// **It cannot refuse, and the check that used to be here could not fire.**
    /// This module declared its own `MAX_TRIGGER_DEPTH` at 1000 and compared
    /// against it on every entry, but the executor walks a tree the binder has
    /// already inlined - and the binder caps that inlining at
    /// `Limit::TriggerDepth`, so a tree deep enough to trip this one never
    /// reaches the executor at all (task-1946, H3). Two constants of the same
    /// name holding different numbers is worse than one: the enforced number
    /// was the binder's and the advertised one was this, and they disagreed by
    /// a factor of thirty.
    ///
    /// The depth itself is still carried, because `fire` reports it and a
    /// trigger body's own statements are compiled against it.
    fn deeper(self) -> Depth {
        Depth(self.0.saturating_add(1))
    }
}

/// Returns the value one row image holds for a bound `OLD` or `NEW` read.
///
/// `None` when the read is not of a trigger row at all, which is every other
/// column reference in the body and is left exactly as it was.
///
/// @param expr - the expression being examined
/// @param rows - the row images
/// @param layout_slots - the target table's declared-to-tree column map
/// @param rowid - which tree column holds the rowid
fn trigger_value(
    expr: &BoundExpr,
    rows: TriggerRows<'_>,
    layout_slots: &[Option<usize>],
    rowid: Option<usize>,
) -> Option<OwnedDatum> {
    let (source, tree_column) = match expr {
        BoundExpr::Column { source, column, .. } => (
            *source,
            layout_slots.get(usize::from(*column)).copied().flatten(),
        ),
        BoundExpr::Rowid { source } => (*source, rowid),
        _ => return None,
    };
    let image = match source {
        OLD_SOURCE => rows.old,
        NEW_SOURCE => rows.new,
        _ => return None,
    };
    // An absent image reads as NULL rather than as an error: a trigger written
    // `AFTER INSERT OR UPDATE` may name `OLD` and reach the insert.
    let Some(image) = image else {
        return Some(OwnedDatum::Null);
    };
    // A column the tree does not carry is a `VIRTUAL` generated one, which is
    // computed rather than stored and so is not reachable from a row image.
    // SQLite refuses `OLD.<virtual>` at `CREATE TRIGGER`; reading NULL here is
    // the same answer as reading an absent image.
    let Some(tree_column) = tree_column else {
        return Some(OwnedDatum::Null);
    };
    Some(image.get(tree_column).cloned().unwrap_or(OwnedDatum::Null))
}

/// Returns the literal a value becomes when it is substituted into a body.
///
/// @param value - the value the row image held
fn literal(value: OwnedDatum) -> BoundExpr {
    match value {
        OwnedDatum::Null => BoundExpr::Null,
        OwnedDatum::Int(number) => BoundExpr::Integer(number),
        OwnedDatum::Real(number) => BoundExpr::Real(number),
        OwnedDatum::Text(bytes) => BoundExpr::Text(bytes),
        OwnedDatum::Blob(bytes) => BoundExpr::Blob(bytes),
    }
}

/// Builds the rewrite that replaces every `OLD` and `NEW` read with its value.
///
/// @param rows - the row images
/// @param slots - the target table's declared-to-tree column map
/// @param rowid - which tree column holds the rowid
fn substitution<'a>(
    rows: TriggerRows<'a>,
    slots: &'a [Option<usize>],
    rowid: Option<usize>,
) -> impl FnMut(&mut BoundExpr) + 'a {
    move |expr: &mut BoundExpr| {
        if let Some(value) = trigger_value(expr, rows, slots, rowid) {
            *expr = literal(value);
        }
    }
}

/// Runs every trigger of one timing, in schema order.
///
/// Returns whether the write should carry on with the row.
///
/// @param triggers - the triggers the statement fires
/// @param time - `BEFORE` or `AFTER`
/// @param rows - the `OLD` and `NEW` images, in tree-column order
/// @param slots - the fired table's declared-to-tree column map
/// @param rowid - which tree column of that table holds the rowid
/// @param target - the file and its trees
/// @param firing - the row, the slots and the depth this firing runs at
pub fn fire(
    triggers: &[BoundTrigger],
    time: TriggerTime,
    target: &mut dyn WriteTarget,
    firing: &TriggerFiring<'_>,
) -> DbResult<Fired> {
    if triggers.is_empty() {
        return Ok(Fired::Continue);
    }
    let TriggerFiring {
        rows,
        slots,
        rowid,
        params,
        depth,
    } = *firing;
    let deeper = TriggerFiring {
        depth: depth.deeper(),
        ..*firing
    };
    for trigger in triggers {
        if trigger.time != time {
            continue;
        }
        if let Some(guard) = &trigger.when {
            let mut guard = guard.clone();
            inillucent_sql::rewrite::rewrite_expr(
                &mut guard,
                &mut substitution(rows, slots, rowid),
            );
            if !truth_of(&guard, target, params)? {
                continue;
            }
        }
        for statement in &trigger.body {
            match run_body(statement, trigger, target, &deeper) {
                Ok(()) => {}
                Err(error) if is_ignore(&error) => return Ok(Fired::SkipRow),
                Err(error) => return Err(named(error, trigger)),
            }
        }
    }
    Ok(Fired::Continue)
}

/// Reports whether a failure is a `RAISE(IGNORE)` rather than a real one.
///
/// @param error - the failure a body produced
fn is_ignore(error: &inillucent_base::error::DbError) -> bool {
    error.message() == RAISE_IGNORE
}

/// Names the trigger a failure came out of, when the failure has no name yet.
///
/// A constraint violation already says which constraint failed, in SQLite's own
/// words, and re-wrapping it would replace a message applications match on. Any
/// other failure inside a body is reported with the trigger's name, because
/// otherwise a statement fails with a sentence about a table it never mentioned.
///
/// @param error - the failure
/// @param trigger - the trigger whose body produced it
fn named(
    error: inillucent_base::error::DbError,
    trigger: &BoundTrigger,
) -> inillucent_base::error::DbError {
    if error.code() != inillucent_base::error::PrimaryCode::Misuse {
        return error;
    }
    let message = error.message().to_string();
    error.with_detail(format!(
        "in the body of trigger {}: {message}",
        String::from_utf8_lossy(&trigger.name)
    ))
}

/// Evaluates a `WHEN` guard, which reads no rows once it is substituted.
///
/// It is planned as `SELECT <guard>` rather than evaluated directly because a
/// guard may hold a subquery - `WHEN EXISTS (SELECT ...)` is legal - and the
/// planner is what knows how to run one.
///
/// @param guard - the substituted guard
/// @param target - the file and its trees
/// @param params - the bound parameters
fn truth_of(guard: &BoundExpr, target: &dyn WriteTarget, params: &Params) -> DbResult<bool> {
    let select = BoundSelect {
        sources: Vec::new(),
        filter: None,
        group_by: Vec::new(),
        having: None,
        columns: vec![inillucent_sql::bind::BoundResultColumn {
            expr: guard.clone(),
            name: b"when".to_vec(),
            origin: None,
            declared_type: Vec::new(),
        }],
        distinct: false,
        order_by: Vec::new(),
        limit: None,
        offset: None,
        aggregates: Vec::new(),
        values: Vec::new(),
        compounds: Vec::new(),
        windows: Vec::new(),
        correlations: Vec::new(),
    };
    let rows = run_select(&select, target, params)?;
    Ok(rows
        .first()
        .and_then(|row| row.first())
        .is_some_and(is_true))
}

/// Reports whether a value is SQL-true.
///
/// @param value - the value a guard produced
fn is_true(value: &OwnedDatum) -> bool {
    match value {
        OwnedDatum::Null => false,
        OwnedDatum::Int(number) => *number != 0,
        OwnedDatum::Real(number) => *number != 0.0,
        OwnedDatum::Text(bytes) => !bytes.is_empty() && bytes.first() != Some(&b'0'),
        OwnedDatum::Blob(bytes) => !bytes.is_empty(),
    }
}

/// Plans and runs one query against the write's own view of the trees.
///
/// @param select - the query
/// @param target - the file and its trees
/// @param params - the bound parameters
fn run_select(
    select: &BoundSelect,
    target: &dyn WriteTarget,
    params: &Params,
) -> DbResult<Vec<Row>> {
    let catalog = target.catalog();
    let plan = plan_select_with(select.clone(), Levers::default());
    let prepared = physical::prepare_any(&plan, catalog)?;
    Ok(physical::run_any_prepared(&plan, catalog, &prepared, params)?.0)
}

/// Runs one statement of a trigger body.
///
/// @param statement - the body statement
/// @param rows - the `OLD` and `NEW` images
/// @param slots - the fired table's declared-to-tree column map
/// @param rowid - which tree column of that table holds the rowid
/// @param target - the file and its trees
/// @param params - the bound parameters
/// @param depth - how deep this body already is
#[allow(clippy::too_many_arguments)]
fn run_body(
    statement: &BoundTriggerStatement,
    trigger: &BoundTrigger,
    target: &mut dyn WriteTarget,
    firing: &TriggerFiring<'_>,
) -> DbResult<()> {
    let TriggerFiring {
        rows,
        slots,
        rowid,
        params,
        depth,
    } = *firing;
    // **`PRAGMA recursive_triggers` lives here, and it is one assignment.** A
    // trigger body is *inlined* by the binder, and a trigger already being
    // bound is skipped - which is what makes the inlining terminate, and is
    // SQLite's behaviour with the pragma off. So a body statement that writes
    // the trigger's own table carries no triggers at all, and a self-inserting
    // trigger fired exactly once where SQLite recurses to
    // `SQLITE_MAX_TRIGGER_DEPTH`.
    //
    // With the pragma on, the statement is handed the trigger back. The body is
    // deep-cloned per fire anyway - the OLD/NEW substitution rewrites it - so
    // this changes nothing anybody else can see, and the recursion is bounded
    // by the depth `fire` already takes one level of on every entry. The one
    // limit is that it re-fires *this* trigger rather than every trigger the
    // table has, which is what an inlined body can reach.
    let recursive = params.recursive_triggers();
    let same_table = |table: &inillucent_sql::catalog_view::TableInfo| {
        recursive && table.folded == trigger.table
    };
    match statement {
        BoundTriggerStatement::Select(select) => {
            let mut select = (**select).clone();
            inillucent_sql::rewrite::rewrite_select(
                &mut select,
                &mut substitution(rows, slots, rowid),
            );
            // The rows are discarded: a body's `SELECT` is run for what
            // evaluating it does, which for every foreign-key check is the
            // `RAISE(ABORT)` in its result column.
            run_select(&select, target, params)?;
            Ok(())
        }
        BoundTriggerStatement::Insert(insert) => {
            let mut insert = (**insert).clone();
            if insert.triggers.is_empty() && same_table(&insert.table) {
                insert.triggers = vec![trigger.clone()];
            }
            inillucent_sql::rewrite::rewrite_insert(
                &mut insert,
                &mut substitution(rows, slots, rowid),
            );
            let supplied = match &insert.source {
                inillucent_sql::dml::BoundInsertSource::Select(select) => {
                    run_select(select, target, params)?
                }
                inillucent_sql::dml::BoundInsertSource::Values(_) => Vec::new(),
            };
            dml::insert_at(&insert, target, params, &supplied, depth)?;
            Ok(())
        }
        BoundTriggerStatement::Update(update) => {
            let mut update = (**update).clone();
            if update.triggers.is_empty() && same_table(&update.table) {
                update.triggers = vec![trigger.clone()];
            }
            inillucent_sql::rewrite::rewrite_update(
                &mut update,
                &mut substitution(rows, slots, rowid),
            );
            let keys = keys_for_update(&update, target, params)?;
            dml::update_at(&update, target, params, &keys, depth)?;
            Ok(())
        }
        BoundTriggerStatement::Delete(delete) => {
            let mut delete = (**delete).clone();
            if delete.triggers.is_empty() && same_table(&delete.table) {
                delete.triggers = vec![trigger.clone()];
            }
            inillucent_sql::rewrite::rewrite_delete(
                &mut delete,
                &mut substitution(rows, slots, rowid),
            );
            let keys = keys_for_delete(&delete, target, params)?;
            dml::delete_at(&delete, target, params, &keys, depth)?;
            Ok(())
        }
    }
}

/// Returns the keys an `UPDATE` in a body will change.
///
/// The same query the engine builds for a typed `UPDATE`: the statement's own
/// `WHERE` over the statement's own table, projecting the table's key. Building
/// it here rather than repeating a scan is what gives a cascade the index.
///
/// @param statement - the substituted update
/// @param target - the file and its trees
/// @param params - the bound parameters
fn keys_for_update(
    statement: &inillucent_sql::dml::BoundUpdate,
    target: &dyn WriteTarget,
    params: &Params,
) -> DbResult<Vec<Row>> {
    let layout = layout_for(target, &statement.table)?;
    let select = dml::keys_query(
        &statement.table,
        statement.source,
        statement.filter.as_ref(),
        statement.limit.as_ref(),
        statement.offset.as_ref(),
        &layout,
    )?;
    run_select(&select, target, params)
}

/// Returns the keys a `DELETE` in a body will remove.
///
/// @param statement - the substituted delete
/// @param target - the file and its trees
/// @param params - the bound parameters
fn keys_for_delete(
    statement: &BoundDelete,
    target: &dyn WriteTarget,
    params: &Params,
) -> DbResult<Vec<Row>> {
    let layout = layout_for(target, &statement.table)?;
    let select = dml::keys_query(
        &statement.table,
        statement.source,
        statement.filter.as_ref(),
        statement.limit.as_ref(),
        statement.offset.as_ref(),
        &layout,
    )?;
    run_select(&select, target, params)
}

/// Returns a table's layout, or says which table has no tree.
///
/// @param target - the file and its trees
/// @param table - the table
fn layout_for(
    target: &dyn WriteTarget,
    table: &inillucent_sql::catalog_view::TableInfo,
) -> DbResult<std::rc::Rc<crate::physical::SourceLayout>> {
    target.layout(table.root).cloned().ok_or_else(|| {
        misuse(format!(
            "a trigger body writes {}, which has no tree",
            String::from_utf8_lossy(&table.name)
        ))
    })
}

/// Returns the triggers an `INSERT` fires for a row it is about to replace.
///
/// A `REPLACE` that deletes a row to make room for another *is* a delete, and
/// the keys pointing at that row have to be told. The binder fills them
/// separately from the statement's own triggers because written `DELETE`
/// triggers are not fired by a `REPLACE` - that is SQLite's rule with its
/// default `recursive_triggers = off` - so these are only the ones a key
/// implies.
///
/// @param statement - the bound insert
pub fn replace_triggers(statement: &BoundInsert) -> &[BoundTrigger] {
    &statement.replace_triggers
}
