//! `INSERT`, and the row it writes.
//!
//! Invariant: **a row reaches the tree once, through `place_row`.** Every
//! arm - a plain insert, an insert into a view, a replace, an upsert's update
//! half - converges on it, so the declarations a column carries are checked in
//! one place rather than in four that drift.

use index::{index_entry, key_of, maintained, unique_indexes, write_index_entry};

use inillucent_base::error::misuse;
use inillucent_base::{DbError, DbResult, ExtendedCode};
use inillucent_sql::ast::{ConflictAction, TriggerTime};
use inillucent_sql::bind::EXCLUDED_SOURCE;
use inillucent_sql::catalog_view::{TableInfo, TableKind};
use inillucent_sql::dml::{codes, rowid_message, BoundInsert, BoundInsertSource};
use inillucent_tree::datum::{Datum, OwnedDatum};

use super::*;
use crate::declared::{IndexExprs, WriteDeclarations};
use crate::insert_plan::InsertPlan;
use crate::physical::{Params, SourceLayout};
use crate::trigger::{self, Depth};

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
pub(crate) fn outer_unwind(error: DbError, on_conflict: Option<ConflictAction>) -> DbError {
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
            target,
            &trigger::TriggerFiring {
                rows: trigger::TriggerRows {
                    old: None,
                    new: Some(image.as_slice()),
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
            &space,
            &plan,
            target,
            image,
            WriteRequest {
                layout: &layout,
                params,
                depth,
                indexes: IndexExprs::new(&declarations, &space),
            },
        )?
        else {
            continue;
        };
        if trigger::fire(
            &statement.triggers,
            TriggerTime::After,
            target,
            &trigger::TriggerFiring {
                rows: trigger::TriggerRows {
                    old: None,
                    new: Some(stored.row()),
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
            target,
            &trigger::TriggerFiring {
                rows: trigger::TriggerRows {
                    old: None,
                    new: Some(image.as_slice()),
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
fn write_one(
    statement: &BoundInsert,
    space: &RowSpace,
    plan: &InsertPlan,
    target: &mut dyn WriteTarget,
    row: Row,
    request: WriteRequest<'_>,
) -> DbResult<Option<Stored>> {
    let WriteRequest {
        layout, indexes, ..
    } = request;
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
                    target,
                    &clash.key,
                    &held,
                    &statement.replace_triggers,
                    request,
                )?;
                continue;
            }
            Resolution::Update => {
                // `None` is the arm's `WHERE` declining, which leaves the row
                // as it is and writes nothing - the same outcome as
                // `DO NOTHING`, and not an error.
                return Ok(upsert_row(
                    statement,
                    table,
                    space,
                    plan,
                    target,
                    &Upsert {
                        clash: &clash,
                        excluded: &row,
                        arm,
                    },
                    request,
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
/// Replaces one row and every index entry that changed with it.
///
/// @param table - the table being written
/// @param layout - the table tree's layout
/// @param target - the file and its trees
/// @param before - the row as it was
/// @param after - the row as it should be
pub(crate) fn replace_row(
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

/// Converts a JSON array of numbers into the blob a `VECTOR(N)` column holds.
///
/// **The only form that worked was a hex blob literal (task-1979, section 8.2,
/// gap 2).** A vector is 32-bit floats in little-endian order, and nothing in
/// SQL writes those: the working `INSERT` appeared in one test file and
/// nowhere else, so an application's first write into a vector column was a
/// hex string it had to build itself. `'[1, 0, 0, 0]'` is what pgvector takes
/// and what an application already has, and it converts to exactly the same
/// bytes.
///
/// `None` means the value is not a JSON array of the declared width, and the
/// caller then checks it as it stands - so a TEXT value that is not one is
/// still refused by `vector_column_is_met` rather than being quietly accepted.
///
/// @param value - what the statement is about to store
/// @param width - how many dimensions the declaration promises
fn vector_from_json(value: Option<&OwnedDatum>, width: usize) -> Option<OwnedDatum> {
    let text = match value {
        Some(OwnedDatum::Text(bytes)) => bytes,
        _ => return None,
    };
    let numbers = json_numbers(text)?;
    if numbers.len() != width {
        return None;
    }
    let mut bytes = Vec::with_capacity(width.saturating_mul(4));
    for number in numbers {
        bytes.extend_from_slice(&(number as f32).to_le_bytes());
    }
    Some(OwnedDatum::Blob(bytes))
}

/// Returns the numbers of a JSON array, or `None` for anything else.
///
/// A hand parser rather than the JSON reader, because the whole grammar here is
/// `[` a comma separated list of numbers `]`: anything with a string, an
/// object, a nested array or a name in it is not a vector, and answering `None`
/// for it is what leaves the ordinary refusal in place.
///
/// @param text - the value's bytes
fn json_numbers(text: &[u8]) -> Option<Vec<f64>> {
    let held = std::str::from_utf8(text).ok()?.trim();
    let inner = held.strip_prefix('[')?.strip_suffix(']')?.trim();
    if inner.is_empty() {
        return Some(Vec::new());
    }
    let mut numbers = Vec::new();
    for part in inner.split(',') {
        let part = part.trim();
        if part.is_empty() {
            return None;
        }
        numbers.push(part.parse::<f64>().ok()?);
    }
    Some(numbers)
}

/// Refuses a value a `VECTOR(N)` column does not admit.
///
/// **The width is checked because the storage does not check it.** A vector of
/// the wrong width is not a slow query, it is a distance that silently answers
/// NULL for ever.
///
/// **NaN and Infinity are refused for a reason measured on the shipped binary
/// (task-1979, R8).** Byte length used to be the only check, so
/// `x'0000c07f...'` - a NaN in the first component - was stored. Every distance
/// against that row is NaN, NaN compares below every real number in the
/// engine's ordering, and the row therefore sorted ahead of every real
/// neighbour in an exhaustive `ORDER BY vector_distance_cos`. Worse, the HNSW
/// builder refuses a non finite component, so a `CREATE INDEX ... USING
/// inillucent_hnsw` over a table holding one of these rows failed - which is the
/// path that reached R1 without a crash. Refusing the value where it is written
/// is the only place the application can still act on it.
///
/// The code is `SQLITE_CONSTRAINT_DATATYPE`, so the primary code a caller
/// matches on is `SQLITE_CONSTRAINT`, the same class a `CHECK` violation
/// reports.
///
/// @param table - the table being written, for the message
/// @param column - the column being written, for the message
/// @param width - how many dimensions the declaration promises
/// @param value - what the statement is about to store, absent when the row
///   image has no cell for the column
fn vector_column_is_met(
    table: &[u8],
    column: &[u8],
    width: usize,
    value: Option<&OwnedDatum>,
) -> DbResult<()> {
    let bytes = match value {
        Some(OwnedDatum::Blob(bytes)) => bytes,
        Some(OwnedDatum::Null) | None => return Ok(()),
        _ => return Err(not_a_vector(table, column, width, "it is not a vector")),
    };
    if bytes.len() != width.saturating_mul(4) {
        return Err(not_a_vector(
            table,
            column,
            width,
            "it is not a vector of the declared width",
        ));
    }
    for component in bytes.chunks_exact(4) {
        let held = match component.try_into() {
            Ok(four) => f32::from_le_bytes(four),
            Err(_) => continue,
        };
        if !held.is_finite() {
            return Err(not_a_vector(
                table,
                column,
                width,
                "one of its components is not a finite number",
            ));
        }
    }
    Ok(())
}

/// Builds the refusal a `VECTOR(N)` column reports.
///
/// Its own function so every reason carries the same code and the same
/// sentence shape; see `vector_column_is_met` for why each one is refused.
///
/// @param table - the table being written
/// @param column - the column being written
/// @param width - how many dimensions the declaration promises
/// @param because - the clause naming what is wrong with the value
fn not_a_vector(table: &[u8], column: &[u8], width: usize, because: &str) -> DbError {
    DbError::new(ExtendedCode(codes::DATATYPE)).with_message(format!(
        "cannot store this value in {}.{}: {}, and the column is declared VECTOR({})",
        String::from_utf8_lossy(table),
        String::from_utf8_lossy(column),
        because,
        width
    ))
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
pub(crate) fn declarations_are_met(
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
        // A `VECTOR(N)` column holds N finite floats or nothing - see
        // `vector_column_is_met`. A JSON array of N numbers is accepted as a
        // spelling of one and converted here, which is where a column's
        // affinity is applied to a value on its way in.
        if let Some(width) = column.vector_dimensions() {
            if let Some(converted) = vector_from_json(value.as_ref(), width) {
                if let Some(cell) = row.get_mut(slot) {
                    *cell = converted.clone();
                }
                vector_column_is_met(&table.name, &column.name, width, Some(&converted))?;
                continue;
            }
            vector_column_is_met(&table.name, &column.name, width, value.as_ref())?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use inillucent_base::error::{misuse, Unwind};

    /// A trigger's `INSERT OR ROLLBACK` decides how far the outer statement
    /// unwinds.
    ///
    /// **The clause travels with the error rather than with the statement
    /// (T3, task-1962).** A failure inside a trigger body is reported to the
    /// statement that fired it, and how much that statement undoes is what the
    /// *inner* statement's clause said: `INSERT OR ROLLBACK` in a trigger
    /// abandons the transaction, not just the row.
    #[test]
    fn the_inner_clause_decides_the_outer_unwind() {
        let refused = || misuse("UNIQUE constraint failed: t.a");
        assert_eq!(
            outer_unwind(refused(), Some(ConflictAction::Rollback)).unwind(),
            Unwind::Transaction
        );
        assert_eq!(
            outer_unwind(refused(), Some(ConflictAction::Fail)).unwind(),
            Unwind::Nothing,
            "`OR FAIL` keeps the rows already written by the statement"
        );
        assert_eq!(
            outer_unwind(refused(), Some(ConflictAction::Abort)).unwind(),
            Unwind::Statement
        );
    }

    /// A statement with no clause of its own leaves the error exactly as it
    /// was.
    ///
    /// The error may already carry an unwind a deeper statement set, and
    /// overwriting it with the default would turn a `ROLLBACK` two triggers
    /// down into an ordinary statement abort.
    #[test]
    fn no_clause_changes_nothing() {
        let carried =
            misuse("UNIQUE constraint failed: t.a").with_outer_unwind(Unwind::Transaction);
        assert_eq!(
            outer_unwind(carried, None).unwind(),
            Unwind::Transaction,
            "the clause a deeper statement set survives a caller that has none"
        );
    }
}
