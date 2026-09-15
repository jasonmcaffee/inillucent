//! `UPDATE`, and what changes when a row's key does.
//!
//! Invariant: **a row whose key changed is a delete and an insert, and a row
//! whose key did not is a write in place.** `difference` is what decides, and
//! getting it wrong is an index entry pointing at a row that is not there.

use inillucent_base::DbResult;
use inillucent_sql::ast::TriggerTime;
use inillucent_sql::bind::BoundExpr;
use inillucent_sql::catalog_view::TableKind;
use inillucent_sql::dml::BoundUpdate;
use inillucent_tree::datum::OwnedDatum;

use super::*;
use crate::declared::{IndexExprs, WriteDeclarations};
use crate::expr::Eval;
use crate::physical::{Params, SourceLayout, TreeCatalog};
use crate::trigger::{self, Depth};

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
    /// Each `STORED` generated column's record slot and compiled value.
    ///
    /// Kept apart from the assignments because it is evaluated against a
    /// different row: the assignments read the before image, and a generated
    /// column reads the row the assignments produced. See
    /// [`inillucent_sql::dml::BoundUpdate::generated`].
    generated: Vec<(usize, Box<dyn Eval>)>,
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
        generated,
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
        // **The stored generated columns, against the row the assignments
        // produced (task-1913).** A row that is rewritten rewrites them, which
        // is what SQLite does and what this did not: `c GENERATED ALWAYS AS
        // (a + 1) STORED` kept the value written when the row was inserted, so
        // `UPDATE g SET a = 5` left `c` reading 2 where the reference reads 6.
        // The stale number is in the record on the disk, so every later read
        // of that file is wrong too, and an index over the column indexes it.
        // Read from `after` rather than `before` for the obvious reason, and
        // computed in column order, which is the order the insert path
        // computes them in.
        for (slot, eval) in generated {
            let value = space.evaluate_with(eval.as_ref(), &[after.as_slice()], &answers)?;
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
            target,
            &trigger::TriggerFiring {
                rows: trigger::TriggerRows {
                    old: Some(before.as_slice()),
                    new: Some(after.as_slice()),
                },
                slots: &layout.slots,
                rowid: layout.rowid,
                params,
                depth,
            },
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
            target,
            &trigger::TriggerFiring {
                rows: trigger::TriggerRows {
                    old: Some(before.as_slice()),
                    new: Some(after.as_slice()),
                },
                slots: &layout.slots,
                rowid: layout.rowid,
                params,
                depth,
            },
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
    // The stored generated columns are compiled whether or not the statement
    // carries a `FROM`: an `UPDATE ... FROM` rewrites the row too.
    let mut generated = Vec::with_capacity(statement.generated.len());
    for column in &statement.generated {
        let Some(slot) = layout
            .slots
            .get(usize::from(column.column))
            .copied()
            .flatten()
        else {
            continue;
        };
        generated.push((slot, space.compile(&column.value, params, catalog)?));
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
        generated,
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
/// What differs between the row as it was and the row as it will be.
///
/// **Three answers rather than two, because the caller does three different
/// things.** This was `Option<usize>`, which folded "nothing changed" and
/// "several columns changed" into the same `None` - so a statement that changed
/// nothing was written the slowest way there is.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Difference {
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
pub(crate) fn difference(before: &[OwnedDatum], after: &[OwnedDatum]) -> Difference {
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
            target,
            &trigger::TriggerFiring {
                rows: trigger::TriggerRows {
                    old: Some(before.as_slice()),
                    new: Some(after.as_slice()),
                },
                slots: &layout.slots,
                rowid: layout.rowid,
                params,
                depth,
            },
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
pub(crate) fn same_key(layout: &SourceLayout, before: &[OwnedDatum], after: &[OwnedDatum]) -> bool {
    layout
        .key_columns
        .iter()
        .all(|column| before.get(*column) == after.get(*column))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Two identical images differ in nothing.
    ///
    /// **Three answers rather than two (T3, task-1962).** This was
    /// `Option<usize>`, which folded "nothing changed" and "several columns
    /// changed" into the same `None` - so an `UPDATE` that changed nothing was
    /// written the slowest way there is. The three cases are asserted here
    /// because the caller does three different things with them.
    #[test]
    fn an_unchanged_row_differs_in_nothing() {
        let before = [OwnedDatum::Int(1), OwnedDatum::Text(b"a".to_vec())];
        let after = [OwnedDatum::Int(1), OwnedDatum::Text(b"a".to_vec())];
        assert_eq!(difference(&before, &after), Difference::Nothing);
    }

    /// One changed column is named by its position.
    #[test]
    fn one_changed_column_is_named_by_position() {
        let before = [OwnedDatum::Int(1), OwnedDatum::Text(b"a".to_vec())];
        let after = [OwnedDatum::Int(1), OwnedDatum::Text(b"b".to_vec())];
        assert_eq!(difference(&before, &after), Difference::One(1));
    }

    /// Two changed columns, or two different widths, are `Several`.
    #[test]
    fn more_than_one_change_is_several() {
        let before = [OwnedDatum::Int(1), OwnedDatum::Int(2)];
        let after = [OwnedDatum::Int(3), OwnedDatum::Int(4)];
        assert_eq!(difference(&before, &after), Difference::Several);
        let shorter = [OwnedDatum::Int(1)];
        assert_eq!(
            difference(&shorter, &after),
            Difference::Several,
            "two images of different widths cannot differ in one column"
        );
    }

    /// A NULL written over a NULL has changed nothing.
    ///
    /// **The images are compared as values, not as SQL.** An `UPDATE` that
    /// writes NULL over NULL would otherwise cost a tree write for no reason.
    /// SQL's `NULL = NULL` being unknown is a question about a predicate; this
    /// is a question about bytes.
    #[test]
    fn a_null_written_over_a_null_is_not_a_change() {
        let before = [OwnedDatum::Null, OwnedDatum::Int(1)];
        let after = [OwnedDatum::Null, OwnedDatum::Int(1)];
        assert_eq!(difference(&before, &after), Difference::Nothing);
    }

    /// The key is the layout's key columns and nothing else.
    ///
    /// An `UPDATE` that leaves the key alone rewrites the row in place; one
    /// that moves it has to delete and reinsert, and every index entry with it.
    /// Asking about the wrong columns picks the wrong one of those.
    #[test]
    fn the_key_is_the_layout_s_key_columns() {
        let layout = SourceLayout {
            tree_key: 1,
            slots: vec![Some(0), Some(1), Some(2)],
            rowid: Some(0),
            identity: vec![0],
            types: vec![crate::expr::StaticType::Unknown; 3],
            width: 3,
            key_columns: vec![0],
        };
        let before = [OwnedDatum::Int(1), OwnedDatum::Int(2), OwnedDatum::Int(3)];
        let same_key_different_row = [OwnedDatum::Int(1), OwnedDatum::Int(9), OwnedDatum::Int(9)];
        let moved = [OwnedDatum::Int(2), OwnedDatum::Int(2), OwnedDatum::Int(3)];
        assert!(
            same_key(&layout, &before, &same_key_different_row),
            "column 0 is the key and it did not move"
        );
        assert!(
            !same_key(&layout, &before, &moved),
            "column 0 is the key and it moved"
        );
    }
}
