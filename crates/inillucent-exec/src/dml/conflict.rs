//! `ON CONFLICT`: what a violated constraint does instead of failing.
//!
//! Invariant: **the conflicting row is found before anything is written.** An
//! upsert that wrote first and repaired afterwards would be visible to a
//! trigger in a state the statement never meant to produce.

use index::{distinct_prefix, index_entry, key_of, maintained, unique_indexes};

use inillucent_base::error::{misuse, Unwind};
use inillucent_base::{DbError, DbResult, ExtendedCode};
use inillucent_sql::ast::ConflictAction;
use inillucent_sql::catalog_view::{IndexOrigin, TableInfo};
use inillucent_sql::dml::{codes, rowid_message, unique_message, BoundInsert};
use inillucent_tree::datum::{Datum, OwnedDatum};

use super::*;
use crate::declared::IndexExprs;
use crate::expr::Eval;
use crate::insert_plan::InsertPlan;
use crate::physical::SourceLayout;

/// What an insert does about a row that collides with one already there.
///
/// An `ON CONFLICT ... DO UPDATE` clause beats the statement's own `OR`
/// algorithm, which is SQLite's rule: `INSERT OR IGNORE ... ON CONFLICT DO
/// UPDATE` updates rather than ignoring, because the clause is attached to the
/// constraint and the algorithm is only the fallback.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Resolution {
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
pub(crate) fn resolution_for(
    statement: &BoundInsert,
    constraint: Option<ConflictAction>,
) -> Resolution {
    resolution_for_arm(statement, constraint, None)
}
/// Returns what an insert does about a conflict, given the arm that matched it.
///
/// @param statement - the bound insert
/// @param constraint - the clause the constraint carries, if it carries one
/// @param arm - which `ON CONFLICT` clause matched, when one did
pub(crate) fn resolution_for_arm(
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
pub(crate) fn matching_arm(statement: &BoundInsert, columns: &[u16]) -> Option<usize> {
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
pub(crate) fn resolution_of(action: Option<ConflictAction>) -> Resolution {
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
pub(crate) struct Conflict {
    /// The key of the row already there.
    pub(crate) key: Vec<OwnedDatum>,
    /// The error an aborting statement reports.
    pub(crate) error: DbError,
    /// The `ON CONFLICT` clause written on the constraint that reported it.
    ///
    /// **Read for the *unwind* and not for the resolution.** A constraint may
    /// say `ON CONFLICT ROLLBACK` or `ON CONFLICT FAIL`, which is `OR ROLLBACK`
    /// and `OR FAIL` written on the constraint instead of on the statement, and
    /// those decide how much of the statement goes back. `ON CONFLICT IGNORE` and `ON CONFLICT REPLACE`
    /// written here decide which *arm* runs instead, and are still ignored:
    /// that is a separate defect with its own ticket, and this field is what it
    /// will read.
    pub(crate) conflict: Option<ConflictAction>,
    /// The columns of the constraint that reported it, sorted.
    ///
    /// **What chooses between several `ON CONFLICT` arms.** An arm names a
    /// conflict target - a set of columns - and runs only when the constraint
    /// that fired is that one. Without this the engine could bind more than one
    /// arm and would still have no way to pick, which is why the old refusal
    /// was at bind time.
    pub(crate) columns: Vec<u16>,
}
/// Returns what a failure resolved this way undoes.
///
/// `IGNORE` and `REPLACE` never reach a failure at all, so they read as the
/// default: an error carrying one of them was raised for some other reason.
///
/// @param action - the conflict algorithm in force, if any was written
pub(crate) fn unwind_of(action: Option<ConflictAction>) -> Unwind {
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
pub(crate) fn rowid_conflict(table: &TableInfo) -> Option<ConflictAction> {
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
pub(crate) fn conflicting_row(
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
pub(crate) fn upsert_row(
    statement: &BoundInsert,
    table: &TableInfo,
    space: &RowSpace,
    plan: &InsertPlan,
    target: &mut dyn WriteTarget,
    upsert: &Upsert<'_>,
    request: WriteRequest<'_>,
) -> DbResult<Option<Row>> {
    let WriteRequest {
        layout, indexes, ..
    } = request;
    let Upsert {
        clash,
        excluded,
        arm,
    } = *upsert;
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
    // The stored generated columns, against the row the arm produced - see
    // `InsertPlan::apply_generated` (task-1913).
    plan.apply_generated(space, &mut after, excluded)?;
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A constraint's own clause decides what an insert does about it.
    ///
    /// **A constraint carries its own algorithm (T3, task-1962).**
    /// `a TEXT UNIQUE ON CONFLICT IGNORE` means every statement that collides
    /// on `a` skips the row, with no `OR IGNORE` written anywhere.
    #[test]
    fn a_constraint_s_own_clause_decides_the_resolution() {
        assert_eq!(
            resolution_of(Some(ConflictAction::Ignore)),
            Resolution::Skip
        );
        assert_eq!(
            resolution_of(Some(ConflictAction::Replace)),
            Resolution::Replace
        );
        assert_eq!(
            resolution_of(Some(ConflictAction::Abort)),
            Resolution::Raise
        );
        assert_eq!(
            resolution_of(None),
            Resolution::Raise,
            "a constraint with no clause of its own reports the failure"
        );
    }

    /// How much is undone is a different question from what the row does.
    ///
    /// `FAIL` keeps the rows already written by this statement, `ROLLBACK`
    /// abandons the whole transaction, and everything else undoes the
    /// statement. Folding these into the resolution would make
    /// `INSERT OR FAIL` and `INSERT OR ABORT` the same statement, and they
    /// differ by exactly the rows written before the one that failed.
    #[test]
    fn the_unwind_is_its_own_answer() {
        assert_eq!(unwind_of(Some(ConflictAction::Fail)), Unwind::Nothing);
        assert_eq!(
            unwind_of(Some(ConflictAction::Rollback)),
            Unwind::Transaction
        );
        assert_eq!(unwind_of(Some(ConflictAction::Abort)), Unwind::Statement);
        assert_eq!(unwind_of(None), Unwind::Statement);
    }

    /// The four resolutions are four values.
    ///
    /// They are matched on rather than compared, so a derive that lost `Eq`
    /// would be found by a compile error - but a variant that duplicated
    /// another would not, and `Skip` doing what `Replace` does is a lost row.
    #[test]
    fn the_four_resolutions_are_distinct() {
        let all = [
            Resolution::Skip,
            Resolution::Replace,
            Resolution::Update,
            Resolution::Raise,
        ];
        for (at, one) in all.iter().enumerate() {
            for two in all.iter().skip(at + 1) {
                assert_ne!(one, two, "{one:?} and {two:?} are the same value");
            }
        }
    }
}
