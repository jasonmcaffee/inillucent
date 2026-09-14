//! The expressions one `INSERT` evaluates, compiled once for the statement.
//!
//! Invariant: compiling per row is what made the old engine's insert path
//! build a closure tree per row of a `VALUES` list. Every value an `INSERT`
//! writes - a supplied one, a `DEFAULT`, a generated column, an upsert's `DO
//! UPDATE`, a `RETURNING` expression - depends on the statement's parameters
//! and not on which row is being built, so `InsertPlan::compile` resolves
//! all of them once and `InsertPlan::build_row` only ever reads the result.
//!
//! Split out of `dml.rs` to keep that module under the size this workspace
//! holds its largest files to (`crates/inillucent-compat/tests/policy.rs`,
//! `no_module_grows_past_the_size_it_is_recorded_at`) - this is one idea, *what
//! an insert's own row looks like before any row exists to write*, and it is
//! named from exactly three places: [`crate::dml::insert_at`] and
//! `crate::dml::insert_into_view`, which compile it, and
//! `crate::dml::write_one`, which drives it one row at a time.

use inillucent_base::{DbError, DbResult, ExtendedCode};
use inillucent_sql::catalog_view::TableInfo;
use inillucent_sql::dml::{codes, BoundInsert, ColumnSource};
use inillucent_tree::datum::OwnedDatum;

use crate::dml::{CompiledUpsert, Row, RowSpace};
use crate::expr::Eval;
use crate::physical::{Params, SourceLayout, TreeCatalog};

/// The expressions an `INSERT` evaluates, compiled once for the statement.
///
/// Compiling per row is what made the old engine's insert path build a closure
/// tree per row of a `VALUES` list. The expressions depend on the parameters
/// and not on the row, so they are compiled with the statement.
pub(crate) struct InsertPlan {
    /// For each table column, where its value comes from.
    columns: Vec<PlannedColumn>,
    /// Where the rowid comes from.
    rowid: Option<PlannedRowid>,
    /// The `DO UPDATE` assignments, by tree column.
    pub(crate) upsert: Vec<CompiledUpsert>,
    /// The `RETURNING` expressions.
    pub(crate) returning: Vec<Box<dyn Eval>>,
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
    /// @param catalog - where a registered function's body is looked up
    pub(crate) fn compile(
        statement: &BoundInsert,
        layout: &SourceLayout,
        space: &RowSpace,
        params: &Params,
        catalog: &dyn TreeCatalog,
    ) -> DbResult<InsertPlan> {
        let mut columns = Vec::with_capacity(statement.columns.len());
        for (column, source) in statement.columns.iter().enumerate() {
            let slot = layout.slots.get(column).copied().flatten();
            let from = match source {
                ColumnSource::Row(index) => PlannedValue::Supplied(*index),
                ColumnSource::Expr(expr) => {
                    PlannedValue::Constant(space.compile(expr, params, catalog)?)
                }
                ColumnSource::Generated(expr) => {
                    PlannedValue::Generated(space.compile(expr, params, catalog)?)
                }
            };
            columns.push(PlannedColumn { slot, from });
        }
        let rowid = match (&statement.rowid, statement.named_rowid) {
            (Some(ColumnSource::Row(index)), _) => Some(PlannedRowid::Supplied(*index)),
            (Some(ColumnSource::Expr(expr) | ColumnSource::Generated(expr)), _) => {
                Some(PlannedRowid::Expr(space.compile(expr, params, catalog)?))
            }
            (None, Some(index)) => Some(PlannedRowid::Supplied(index)),
            (None, None) => None,
        };
        // **One compiled arm per written clause.** Which of them runs is a
        // run-time question - it depends on which constraint the row actually
        // collided with - so all of them are compiled and the choice is made
        // per conflict.
        let mut upsert = Vec::with_capacity(statement.upsert.len());
        for clause in &statement.upsert {
            let mut assignments = Vec::new();
            let mut filter = None;
            if clause.do_update {
                for assignment in &clause.assignments {
                    if let Some(slot) = layout
                        .slots
                        .get(usize::from(assignment.column))
                        .copied()
                        .flatten()
                    {
                        assignments
                            .push((slot, space.compile(&assignment.value, params, catalog)?));
                    }
                }
                if let Some(written) = &clause.filter {
                    filter = Some(space.compile(written, params, catalog)?);
                }
            }
            upsert.push(CompiledUpsert {
                assignments,
                filter,
            });
        }
        let mut returning = Vec::with_capacity(statement.returning.len());
        for column in &statement.returning {
            returning.push(space.compile(&column.expr, params, catalog)?);
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
    /// @param highest - what the first allocation counts up from
    /// @param autoincrement - the table, when it never reuses a key
    pub(crate) fn build_row(
        &self,
        supplied: &[OwnedDatum],
        space: &RowSpace,
        next_rowid: &mut Option<i64>,
        highest: impl FnOnce() -> DbResult<i64>,
        autoincrement: Option<&TableInfo>,
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
            // **A supplied rowid takes INTEGER affinity first.** `INSERT INTO
            // t(id) VALUES ('42')` on an `INTEGER PRIMARY KEY` stores row 42 in
            // SQLite, because the key is a value like any other and affinity is
            // applied to it on the way in; only what survives the conversion
            // still un-integral is a mismatch. Applying it here rather than in
            // `WriteDeclarations` keeps one rule for the key rather than two.
            let supplied_key = crate::declared::to_key_affinity(supplied_key);
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
                    // An `AUTOINCREMENT` table that has reached `i64::MAX` has
                    // no next key, and handing one out would mean handing out
                    // one that is already there. SQLite reports `SQLITE_FULL`.
                    let allocated = match autoincrement {
                        Some(table) => crate::sequence::allocate(table, held)?,
                        None => held.saturating_add(1),
                    };
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
        self.apply_generated(space, &mut row, &[])?;
        Ok(row)
    }

    /// Recomputes every `STORED` generated column against a row.
    ///
    /// **A row that is rewritten rewrites them (task-1913).** The insert path
    /// always did this; the `DO UPDATE` arm did not, so
    /// `INSERT ... ON CONFLICT DO UPDATE SET a = excluded.a` left a column
    /// declared `GENERATED ALWAYS AS (a + 100) STORED` holding the number it
    /// was given when the row was first inserted. It is the same defect the
    /// plain `UPDATE` had, in the other statement that rewrites a row, and it
    /// is worse in the same way: the stale value is written to the disk, so
    /// every later read of that file reads it, and an index over the column
    /// indexes it.
    ///
    /// A `VIRTUAL` column has no slot and is skipped, because it is computed
    /// when it is read rather than stored.
    ///
    /// @param space - the row space the expressions read
    /// @param row - the row to fill in, which the expressions also read
    /// @param excluded - the `excluded` image, empty outside a `DO UPDATE`
    pub(crate) fn apply_generated(
        &self,
        space: &RowSpace,
        row: &mut [OwnedDatum],
        excluded: &[OwnedDatum],
    ) -> DbResult<()> {
        for planned in &self.columns {
            let (Some(slot), PlannedValue::Generated(eval)) = (planned.slot, &planned.from) else {
                continue;
            };
            let value = space.evaluate(eval.as_ref(), &[&*row, excluded])?;
            if let Some(cell) = row.get_mut(slot) {
                *cell = value;
            }
        }
        Ok(())
    }
}
