//! Compiling INSERT, UPDATE and DELETE into bytecode.
//!
//! Invariant: a row is never written until every constraint that could refuse
//! it has been asked. The order is SQLite's - NOT NULL, then CHECK, then the
//! primary key, then each UNIQUE index in declaration order - and it matters
//! twice over: it decides which error a row with two problems reports, and it
//! decides what a REPLACE deletes before it discovers a second conflict.
//!
//! The second invariant is that a statement that reads the table it writes
//! reads it first. UPDATE and DELETE run in two passes: a scan that collects
//! rowids into a sorter, then a loop that seeks each one and changes it. A
//! one-pass loop would be walking a B-tree while rewriting it, and the
//! failure that produces is not a crash - it is a row visited twice or not at
//! all, which looks like a wrong answer rather than like a bug. The TDD asks
//! for exactly this, and the cost is one sorter over the rowids.
//!
//! Conflict algorithms are compiled, not interpreted. Each constraint test
//! jumps to a handler chosen at compile time: ABORT and its relatives halt
//! with the matching extended code, IGNORE jumps to the next row, and REPLACE
//! deletes what is in the way and carries on. The session finds out which one
//! failed from the code the halt carried, and undoes as much as that algorithm
//! says to.

use rustdb_base::error::ExtendedCode;
use rustdb_base::{error, DbResult};
use rustdb_sql::ast::ConflictAction;
use rustdb_sql::bind::EXCLUDED_SOURCE;
use rustdb_sql::bind::{BoundExpr, BoundResultColumn};
use rustdb_sql::catalog_view::{IndexInfo, TableInfo};
use rustdb_sql::dml::{
    BoundAssignment, BoundCheck, BoundDelete, BoundInsert, BoundInsertSource, BoundUpdate,
    BoundUpsert, ColumnSource,
};
use rustdb_value::{Affinity, Collation};

use crate::compile::{Compiler, Label};
use crate::program::{
    IndexKey, Instruction, Opcode, Operand, Program, ProgramDependencies, ResultColumn, SortColumn,
    SortKey,
};

/// The result codes a constraint failure reports.
///
/// The numbers are SQLite's own extended codes. They are written out rather
/// than derived because an application matches on them, and a code that was
/// computed from an enum's discriminant would change the day the enum did.
mod codes {
    /// `SQLITE_CONSTRAINT_CHECK`.
    pub const CHECK: i32 = 275;
    /// `SQLITE_CONSTRAINT_NOTNULL`.
    pub const NOT_NULL: i32 = 1299;
    /// `SQLITE_CONSTRAINT_PRIMARYKEY`.
    pub const PRIMARY_KEY: i32 = 1555;
    /// `SQLITE_CONSTRAINT_UNIQUE`.
    pub const UNIQUE: i32 = 2067;
    /// `SQLITE_CONSTRAINT_ROWID`.
    pub const ROWID: i32 = 2579;
    /// `SQLITE_MISMATCH`, which an `INTEGER PRIMARY KEY` reports for a value
    /// that is not an integer.
    pub const MISMATCH: i32 = 20;
}

/// How a conflict is reported back to the session.
///
/// The machine cannot undo a statement - it does not own the pager's undo
/// levels - so it reports which algorithm the failing constraint carried and
/// the session does the undoing. `p3` of the halt carries this.
pub const CONFLICT_ROLLBACK: i32 = 0;
/// The statement is undone and the error reported. SQLite's default.
pub const CONFLICT_ABORT: i32 = 1;
/// The error is reported and the rows already written are kept.
pub const CONFLICT_FAIL: i32 = 2;

/// Returns the code a halt carries for one algorithm.
fn conflict_code(action: ConflictAction) -> i32 {
    match action {
        ConflictAction::Rollback => CONFLICT_ROLLBACK,
        ConflictAction::Fail => CONFLICT_FAIL,
        _ => CONFLICT_ABORT,
    }
}

/// What a constraint does when it is violated.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Resolution {
    /// Stop, with the given code and algorithm.
    Halt(ConflictAction),
    /// Skip this row without an error.
    Ignore,
    /// Delete what is in the way and write anyway.
    Replace,
    /// Update the row that is in the way, or leave it alone.
    Upsert,
}

/// Chooses the algorithm one constraint resolves with.
///
/// The statement's `OR` clause wins over the constraint's own `ON CONFLICT`,
/// which is SQLite's documented precedence, and ABORT is what is left when
/// neither wrote one.
fn resolution(statement: Option<ConflictAction>, constraint: Option<ConflictAction>) -> Resolution {
    let chosen = statement.or(constraint).unwrap_or(ConflictAction::Abort);
    match chosen {
        ConflictAction::Ignore => Resolution::Ignore,
        ConflictAction::Replace => Resolution::Replace,
        other => Resolution::Halt(other),
    }
}

/// The cursors and registers a DML program works through.
struct Writer {
    /// The write cursor on the table.
    table: u32,
    /// One write cursor per index, in the table's index order.
    indexes: Vec<u32>,
    /// The indexes themselves, so their keys can be rebuilt.
    definitions: Vec<IndexInfo>,
}

impl Compiler {
    /// Opens the table and every index for writing.
    fn open_for_write(&mut self, table: &TableInfo) -> Writer {
        let cursor = self.cursors;
        self.cursors = self.cursors.saturating_add(1);
        self.emit(
            Instruction::new(Opcode::OpenWrite, cursor as i32, table.root as i32, 0)
                .with_p4(Operand::Count(table.columns.len() as u32)),
        );
        self.source_cursors = vec![cursor];
        let mut indexes = Vec::new();
        let mut definitions = Vec::new();
        for index in &table.indexes {
            if index.root == 0 {
                continue;
            }
            let slot = self.cursors;
            self.cursors = self.cursors.saturating_add(1);
            self.emit(
                Instruction::new(Opcode::OpenWriteIndex, slot as i32, index.root as i32, 0)
                    .with_p4(Operand::IndexKey(index_key(index))),
            );
            indexes.push(slot);
            definitions.push(index.clone());
        }
        Writer {
            table: cursor,
            indexes,
            definitions,
        }
    }

    /// Emits a halt that reports a constraint failure.
    fn emit_constraint_halt(&mut self, code: i32, action: ConflictAction, message: String) {
        self.emit(
            Instruction::new(Opcode::HaltError, code, 0, conflict_code(action))
                .with_p4(Operand::Text(message.into_bytes())),
        );
    }

    /// Reads a table cursor's column into a fresh register.
    fn read_column(&mut self, cursor: u32, table: &TableInfo, column: u16) -> u32 {
        let register = self.register();
        if table.rowid_alias == Some(column) {
            self.emit(Instruction::new(
                Opcode::Rowid,
                cursor as i32,
                register as i32,
                0,
            ));
            return register;
        }
        let widen = u16::from(
            table
                .column(column)
                .is_some_and(|info| info.affinity == Affinity::Real),
        );
        self.emit(
            Instruction::new(
                Opcode::Column,
                cursor as i32,
                i32::from(column),
                register as i32,
            )
            .with_p5(widen),
        );
        register
    }

    /// Builds one index entry's record from a block of column registers.
    ///
    /// The entry is the key columns followed by the rowid, which is what makes
    /// an index entry unique even in a non-unique index and what lets a lookup
    /// get back to the table row.
    fn emit_index_record(
        &mut self,
        index: &IndexInfo,
        values: &[u32],
        rowid: u32,
        table: &TableInfo,
    ) -> DbResult<u32> {
        let width = index.columns.len().saturating_add(1);
        let block = self.register_block(width);
        for (position, key) in index.columns.iter().enumerate() {
            let Some(column) = key.column else {
                return Err(error::misuse("an index on an expression cannot be written"));
            };
            let Some(source) = values.get(column as usize) else {
                return Err(error::misuse("an index names a column the table has not"));
            };
            let target = block.saturating_add(position as u32);
            self.emit(Instruction::new(
                Opcode::Copy,
                *source as i32,
                target as i32,
                0,
            ));
        }
        let rowid_slot = block.saturating_add(index.columns.len() as u32);
        self.emit(Instruction::new(
            Opcode::Copy,
            rowid as i32,
            rowid_slot as i32,
            0,
        ));
        let affinities = index
            .columns
            .iter()
            .map(|key| {
                key.column
                    .and_then(|column| table.column(column))
                    .map_or(Affinity::Blob, |column| column.affinity)
            })
            .chain(core::iter::once(Affinity::Integer))
            .collect();
        let record = self.register();
        self.emit(
            Instruction::new(
                Opcode::MakeRecord,
                block as i32,
                width as i32,
                record as i32,
            )
            .with_p4(Operand::Affinities(affinities)),
        );
        Ok(record)
    }

    /// Emits the deletion of the row a table cursor is sitting on, index
    /// entries first.
    ///
    /// The index entries have to go first because building one needs the row's
    /// column values, and after the row is gone the cursor has nothing to read
    /// them from.
    fn emit_delete_current(&mut self, writer: &Writer, table: &TableInfo) -> DbResult<()> {
        let rowid = self.register();
        self.emit(Instruction::new(
            Opcode::Rowid,
            writer.table as i32,
            rowid as i32,
            0,
        ));
        let values: Vec<u32> = (0..table.columns.len() as u16)
            .map(|column| self.read_column(writer.table, table, column))
            .collect();
        for (position, index) in writer.definitions.iter().enumerate() {
            let Some(cursor) = writer.indexes.get(position).copied() else {
                continue;
            };
            let record = self.emit_index_record(index, &values, rowid, table)?;
            self.emit(Instruction::new(
                Opcode::IdxDelete,
                cursor as i32,
                record as i32,
                0,
            ));
        }
        self.emit(Instruction::new(
            Opcode::DeleteRow,
            writer.table as i32,
            0,
            0,
        ));
        Ok(())
    }

    /// Emits the NOT NULL and CHECK tests over a row image.
    ///
    /// `skip` is where an IGNORE resolution jumps to, which is the top of the
    /// next row.
    fn emit_row_constraints(
        &mut self,
        table: &TableInfo,
        values: &[u32],
        checks: &[BoundCheck],
        statement: Option<ConflictAction>,
        skip: &mut Vec<Label>,
    ) -> DbResult<()> {
        for (position, column) in table.columns.iter().enumerate() {
            if !column.not_null || table.rowid_alias == Some(position as u16) {
                continue;
            }
            let Some(register) = values.get(position).copied() else {
                continue;
            };
            let resolution = resolution(statement, column.not_null_conflict);
            let past = self.emit_jump(Instruction::new(Opcode::IfNotNull, register as i32, -1, 0));
            match resolution {
                Resolution::Halt(action) => self.emit_constraint_halt(
                    codes::NOT_NULL,
                    action,
                    format!(
                        "NOT NULL constraint failed: {}.{}",
                        String::from_utf8_lossy(&table.name),
                        String::from_utf8_lossy(&column.name)
                    ),
                ),
                // REPLACE on a NOT NULL is not a deletion: SQLite substitutes
                // the column's default, and reports the constraint failure
                // when there is no default. That is a different operation from
                // the REPLACE a unique index performs, and conflating them
                // would silently write a NULL.
                Resolution::Ignore | Resolution::Replace => {
                    let label = self.emit_jump(Instruction::new(Opcode::Goto, 0, -1, 0));
                    skip.push(label);
                }
                // An `ON CONFLICT` clause resolves a *uniqueness* conflict; it
                // says nothing about a NULL in a NOT NULL column, which is
                // still an error under whatever algorithm the statement chose.
                Resolution::Upsert => self.emit_constraint_halt(
                    codes::NOT_NULL,
                    ConflictAction::Abort,
                    format!(
                        "NOT NULL constraint failed: {}.{}",
                        String::from_utf8_lossy(&table.name),
                        String::from_utf8_lossy(&column.name)
                    ),
                ),
            }
            self.patch_here(past);
        }
        for check in checks {
            let register = self.compile_expr(&check.expr)?;
            // A CHECK passes when it is true *or* NULL: an unknown answer is
            // not a violation, which is why this jumps on NULL as well.
            let past =
                self.emit_jump(Instruction::new(Opcode::If, register as i32, -1, 0).with_p5(1));
            let named = match &check.name {
                Some(name) => format!("CHECK constraint failed: {}", String::from_utf8_lossy(name)),
                None => format!(
                    "CHECK constraint failed: {}",
                    String::from_utf8_lossy(&table.name)
                ),
            };
            match resolution(statement, None) {
                Resolution::Halt(action) => self.emit_constraint_halt(codes::CHECK, action, named),
                Resolution::Ignore | Resolution::Replace => {
                    let label = self.emit_jump(Instruction::new(Opcode::Goto, 0, -1, 0));
                    skip.push(label);
                }
                Resolution::Upsert => {
                    self.emit_constraint_halt(codes::CHECK, ConflictAction::Abort, named)
                }
            }
            self.patch_here(past);
        }
        Ok(())
    }

    /// Emits the uniqueness tests, and whatever the resolution asks for.
    ///
    /// `exclude` is the rowid the statement is allowed to conflict with, which
    /// is how an UPDATE that rewrites a row without changing its key does not
    /// report a conflict with itself.
    #[allow(clippy::too_many_arguments)]
    fn emit_unique_constraints(
        &mut self,
        writer: &Writer,
        table: &TableInfo,
        values: &[u32],
        rowid: u32,
        exclude: Option<u32>,
        statement: Option<ConflictAction>,
        upsert: Option<&BoundUpsert>,
        checks: &[BoundCheck],
        returning: &[BoundResultColumn],
        skip: &mut Vec<Label>,
    ) -> DbResult<()> {
        if table.rowid_alias.is_some() || exclude.is_some() {
            let handled = upsert.filter(|clause| upsert_covers_rowid(clause, table));
            self.emit_rowid_constraint(
                writer, table, values, rowid, exclude, statement, handled, checks, returning, skip,
            )?;
        }
        for (position, index) in writer.definitions.clone().iter().enumerate() {
            if !index.unique {
                continue;
            }
            let Some(cursor) = writer.indexes.get(position).copied() else {
                continue;
            };
            let key_block = self.register_block(index.columns.len().max(1));
            for (offset, key) in index.columns.iter().enumerate() {
                let Some(column) = key.column else {
                    return Err(error::misuse("a unique index on an expression"));
                };
                let Some(source) = values.get(column as usize) else {
                    continue;
                };
                let target = key_block.saturating_add(offset as u32);
                self.emit(Instruction::new(
                    Opcode::Copy,
                    *source as i32,
                    target as i32,
                    0,
                ));
            }
            let clear = self.emit_jump(
                Instruction::new(Opcode::NoConflict, cursor as i32, -1, key_block as i32)
                    .with_p5(index.columns.len() as u16),
            );
            // The entry that matched belongs to some row; when it is the row
            // being updated, it is not a conflict.
            let victim = self.register();
            self.emit(Instruction::new(
                Opcode::IdxRowid,
                cursor as i32,
                victim as i32,
                0,
            ));
            let mut same = None;
            if let Some(exclude) = exclude {
                let equal = self.register();
                self.emit(
                    Instruction::new(Opcode::Compare, victim as i32, exclude as i32, equal as i32)
                        .with_p4(Operand::Comparison(crate::program::Comparison {
                            op: rustdb_sql::ast::BinaryOp::Equal,
                            affinity: Some(Affinity::Integer),
                            collation: Collation::Binary,
                        })),
                );
                same = Some(self.emit_jump(Instruction::new(Opcode::If, equal as i32, -1, 0)));
            }
            let code = if index.origin == rustdb_sql::catalog_view::IndexOrigin::PrimaryKey {
                codes::PRIMARY_KEY
            } else {
                codes::UNIQUE
            };
            let covered = upsert.filter(|clause| upsert_covers_index(clause, index));
            let chosen = match covered {
                Some(_) => Resolution::Upsert,
                None => resolution(statement, index.conflict),
            };
            match chosen {
                Resolution::Halt(action) => {
                    self.emit_constraint_halt(code, action, unique_message(table, index))
                }
                Resolution::Ignore => {
                    let label = self.emit_jump(Instruction::new(Opcode::Goto, 0, -1, 0));
                    skip.push(label);
                }
                Resolution::Replace => {
                    let absent = self.emit_jump(Instruction::new(
                        Opcode::NotExists,
                        writer.table as i32,
                        -1,
                        victim as i32,
                    ));
                    self.emit_delete_current(writer, table)?;
                    self.patch_here(absent);
                }
                Resolution::Upsert => {
                    let Some(clause) = covered else {
                        return Err(error::misuse("an upsert resolution with no clause"));
                    };
                    self.emit_upsert(
                        writer, table, clause, values, rowid, victim, checks, returning, skip,
                    )?;
                }
            }
            if let Some(same) = same {
                self.patch_here(same);
            }
            self.patch_here(clear);
        }
        Ok(())
    }

    /// Emits the test that the rowid being written is free.
    #[allow(clippy::too_many_arguments)]
    fn emit_rowid_constraint(
        &mut self,
        writer: &Writer,
        table: &TableInfo,
        values: &[u32],
        rowid: u32,
        exclude: Option<u32>,
        statement: Option<ConflictAction>,
        upsert: Option<&BoundUpsert>,
        checks: &[BoundCheck],
        returning: &[BoundResultColumn],
        skip: &mut Vec<Label>,
    ) -> DbResult<()> {
        let absent = self.emit_jump(Instruction::new(
            Opcode::NotExists,
            writer.table as i32,
            -1,
            rowid as i32,
        ));
        let mut same = None;
        if let Some(exclude) = exclude {
            let equal = self.register();
            self.emit(
                Instruction::new(Opcode::Compare, rowid as i32, exclude as i32, equal as i32)
                    .with_p4(Operand::Comparison(crate::program::Comparison {
                        op: rustdb_sql::ast::BinaryOp::Equal,
                        affinity: Some(Affinity::Integer),
                        collation: Collation::Binary,
                    })),
            );
            same = Some(self.emit_jump(Instruction::new(Opcode::If, equal as i32, -1, 0)));
        }
        let constraint = table
            .rowid_alias
            .and_then(|column| table.column(column))
            .map(|column| {
                format!(
                    "UNIQUE constraint failed: {}.{}",
                    String::from_utf8_lossy(&table.name),
                    String::from_utf8_lossy(&column.name)
                )
            })
            .unwrap_or_else(|| {
                format!(
                    "UNIQUE constraint failed: {}.rowid",
                    String::from_utf8_lossy(&table.name)
                )
            });
        let code = if table.rowid_alias.is_some() {
            codes::PRIMARY_KEY
        } else {
            codes::ROWID
        };
        let chosen = match upsert {
            Some(_) => Resolution::Upsert,
            None => resolution(statement, rowid_conflict(table)),
        };
        match chosen {
            Resolution::Halt(action) => self.emit_constraint_halt(code, action, constraint),
            Resolution::Ignore => {
                let label = self.emit_jump(Instruction::new(Opcode::Goto, 0, -1, 0));
                skip.push(label);
            }
            Resolution::Replace => {
                self.emit_delete_current(writer, table)?;
            }
            Resolution::Upsert => {
                let Some(clause) = upsert else {
                    return Err(error::misuse("an upsert resolution with no clause"));
                };
                // The cursor is already sitting on the conflicting row: it was
                // put there by the `NotExists` that found the conflict, which
                // is the whole reason that opcode seeks rather than merely
                // testing.
                let victim = self.register();
                self.emit(Instruction::new(
                    Opcode::Rowid,
                    writer.table as i32,
                    victim as i32,
                    0,
                ));
                self.emit_upsert(
                    writer, table, clause, values, rowid, victim, checks, returning, skip,
                )?;
            }
        }
        if let Some(same) = same {
            self.patch_here(same);
        }
        self.patch_here(absent);
        Ok(())
    }

    /// Emits an `ON CONFLICT ... DO UPDATE`, with the cursor on the row that
    /// was in the way.
    ///
    /// The shape is an UPDATE of one known row rather than a second INSERT:
    /// the assignments read the *existing* row through the cursor and the
    /// proposed row through `excluded`, so the two are compiled against
    /// different things - the cursor for one, the insert's own registers for
    /// the other - and the substitution table is what keeps them apart.
    ///
    /// `DO NOTHING` and a `WHERE` that is false both leave the existing row
    /// alone and skip to the next one, and neither counts as a change. That is
    /// SQLite's behaviour and it is the reason `changes()` after an upsert is
    /// worth checking against the reference rather than reasoning about.
    #[allow(clippy::too_many_arguments)]
    fn emit_upsert(
        &mut self,
        writer: &Writer,
        table: &TableInfo,
        clause: &BoundUpsert,
        inserted: &[u32],
        inserted_rowid: u32,
        victim: u32,
        checks: &[BoundCheck],
        returning: &[BoundResultColumn],
        skip: &mut Vec<Label>,
    ) -> DbResult<()> {
        if !clause.do_update {
            let label = self.emit_jump(Instruction::new(Opcode::Goto, 0, -1, 0));
            skip.push(label);
            return Ok(());
        }
        let absent = self.emit_jump(Instruction::new(
            Opcode::NotExists,
            writer.table as i32,
            -1,
            victim as i32,
        ));
        let previous = core::mem::replace(
            &mut self.substitutions,
            excluded_substitutions(table, inserted, inserted_rowid),
        );
        let assignments = self.emit_upsert_values(writer, table, clause);
        let values = match assignments {
            Ok(values) => values,
            Err(failure) => {
                self.substitutions = previous;
                return Err(failure);
            }
        };
        let filter = match clause.filter.as_ref() {
            Some(filter) => match self.compile_expr(filter) {
                Ok(register) => Some(register),
                Err(failure) => {
                    self.substitutions = previous;
                    return Err(failure);
                }
            },
            None => None,
        };
        self.substitutions = previous;
        if let Some(register) = filter {
            // A WHERE that is false or unknown leaves the row alone.
            let label =
                self.emit_jump(Instruction::new(Opcode::IfNot, register as i32, -1, 0).with_p5(1));
            skip.push(label);
        }
        let new_rowid = match table.rowid_alias {
            Some(alias) => values.get(alias as usize).copied().unwrap_or(victim),
            None => victim,
        };
        let mut inner = Vec::new();
        let previous = core::mem::replace(
            &mut self.substitutions,
            row_substitutions(table, &values, new_rowid),
        );
        let constraints = self.emit_row_constraints(table, &values, checks, None, &mut inner);
        self.substitutions = previous;
        constraints?;
        self.emit_unique_constraints(
            writer,
            table,
            &values,
            new_rowid,
            Some(victim),
            None,
            None,
            checks,
            returning,
            &mut inner,
        )?;
        let gone = self.emit_jump(Instruction::new(
            Opcode::NotExists,
            writer.table as i32,
            -1,
            victim as i32,
        ));
        self.emit_delete_current(writer, table)?;
        self.patch_here(gone);
        self.emit_write_row(writer, table, &values, new_rowid)?;
        self.emit(Instruction::new(
            Opcode::CountChange,
            new_rowid as i32,
            0,
            0,
        ));
        let previous = core::mem::replace(
            &mut self.substitutions,
            row_substitutions(table, &values, new_rowid),
        );
        let emitted = self.emit_returning(returning);
        self.substitutions = previous;
        emitted?;
        for label in inner {
            self.patch_here(label);
        }
        self.patch_here(absent);
        let label = self.emit_jump(Instruction::new(Opcode::Goto, 0, -1, 0));
        skip.push(label);
        Ok(())
    }

    /// Evaluates the row a `DO UPDATE` produces, column by column.
    fn emit_upsert_values(
        &mut self,
        writer: &Writer,
        table: &TableInfo,
        clause: &BoundUpsert,
    ) -> DbResult<Vec<u32>> {
        let mut values = Vec::with_capacity(table.columns.len());
        for position in 0..table.columns.len() as u16 {
            let assigned = clause
                .assignments
                .iter()
                .find(|assignment: &&BoundAssignment| assignment.column == position);
            let register = match assigned {
                Some(assignment) => {
                    let value = self.compile_expr(&assignment.value)?;
                    let copy = self.register();
                    self.emit(Instruction::new(Opcode::Copy, value as i32, copy as i32, 0));
                    if let Some(column) = table.column(position) {
                        if table.rowid_alias != Some(position) {
                            self.emit(
                                Instruction::new(Opcode::ApplyAffinity, copy as i32, 1, 0)
                                    .with_p4(Operand::Affinity(column.affinity)),
                            );
                        }
                    }
                    copy
                }
                None => self.read_column(writer.table, table, position),
            };
            values.push(register);
        }
        Ok(values)
    }

    /// Emits the record for a table row and writes it, indexes included.
    fn emit_write_row(
        &mut self,
        writer: &Writer,
        table: &TableInfo,
        values: &[u32],
        rowid: u32,
    ) -> DbResult<()> {
        let width = table.columns.len().max(1);
        let block = self.register_block(width);
        for position in 0..table.columns.len() {
            let target = block.saturating_add(position as u32);
            if table.rowid_alias == Some(position as u16) {
                // The rowid is the row's key, not one of its fields; SQLite
                // stores a NULL in the record and reads the key back instead.
                self.emit(Instruction::new(Opcode::Null, 0, target as i32, 0));
                continue;
            }
            let Some(source) = values.get(position).copied() else {
                self.emit(Instruction::new(Opcode::Null, 0, target as i32, 0));
                continue;
            };
            self.emit(Instruction::new(
                Opcode::Copy,
                source as i32,
                target as i32,
                0,
            ));
        }
        let affinities = table
            .columns
            .iter()
            .enumerate()
            .map(|(position, column)| {
                if table.rowid_alias == Some(position as u16) {
                    Affinity::Blob
                } else {
                    column.affinity
                }
            })
            .collect();
        let record = self.register();
        self.emit(
            Instruction::new(
                Opcode::MakeRecord,
                block as i32,
                table.columns.len() as i32,
                record as i32,
            )
            .with_p4(Operand::Affinities(affinities)),
        );
        self.emit(Instruction::new(
            Opcode::InsertRow,
            writer.table as i32,
            record as i32,
            rowid as i32,
        ));
        for (position, index) in writer.definitions.clone().iter().enumerate() {
            let Some(cursor) = writer.indexes.get(position).copied() else {
                continue;
            };
            let entry = self.emit_index_record(index, values, rowid, table)?;
            self.emit(Instruction::new(
                Opcode::IdxInsert,
                cursor as i32,
                entry as i32,
                0,
            ));
        }
        Ok(())
    }

    /// Emits a RETURNING row, if the statement has one.
    fn emit_returning(&mut self, columns: &[BoundResultColumn]) -> DbResult<()> {
        if columns.is_empty() {
            return Ok(());
        }
        let block = self.register_block(columns.len());
        for (position, column) in columns.iter().enumerate() {
            let value = self.compile_expr(&column.expr)?;
            let target = block.saturating_add(position as u32);
            self.emit(Instruction::new(
                Opcode::Copy,
                value as i32,
                target as i32,
                0,
            ));
        }
        self.emit(Instruction::new(
            Opcode::ResultRow,
            block as i32,
            columns.len() as i32,
            0,
        ));
        Ok(())
    }
}

/// Returns the message a unique-index violation reports.
///
/// SQLite names every column of the index, comma separated, which is what an
/// application parses to find out which key collided.
fn unique_message(table: &TableInfo, index: &IndexInfo) -> String {
    let names: Vec<String> = index
        .columns
        .iter()
        .filter_map(|key| key.column)
        .filter_map(|column| table.column(column))
        .map(|column| {
            format!(
                "{}.{}",
                String::from_utf8_lossy(&table.name),
                String::from_utf8_lossy(&column.name)
            )
        })
        .collect();
    format!("UNIQUE constraint failed: {}", names.join(", "))
}

/// Reports whether an upsert's conflict target names this index.
///
/// An `ON CONFLICT` with no target applies to every uniqueness constraint on
/// the table, which is SQLite's rule and the reason `ON CONFLICT DO NOTHING`
/// is the general form. A target that names columns applies only to the index
/// over exactly those columns, in any order.
fn upsert_covers_index(clause: &BoundUpsert, index: &IndexInfo) -> bool {
    if !index.unique {
        return false;
    }
    if clause.target.is_empty() {
        return true;
    }
    let keys: Vec<u16> = index.columns.iter().filter_map(|key| key.column).collect();
    if keys.len() != index.columns.len() || keys.len() != clause.target.len() {
        return false;
    }
    keys.iter().all(|column| clause.target.contains(column))
}

/// Reports whether an upsert's conflict target names the rowid.
fn upsert_covers_rowid(clause: &BoundUpsert, table: &TableInfo) -> bool {
    if clause.target.is_empty() {
        return true;
    }
    match table.rowid_alias {
        Some(alias) => clause.target.as_slice() == [alias],
        None => false,
    }
}

/// Returns the substitutions an upsert's `excluded` row is compiled against.
fn excluded_substitutions(table: &TableInfo, values: &[u32], rowid: u32) -> Vec<(BoundExpr, u32)> {
    let mut substitutions = Vec::with_capacity(values.len().saturating_add(1));
    for (position, register) in values.iter().enumerate() {
        if table.rowid_alias == Some(position as u16) {
            continue;
        }
        let Some(column) = table.column(position as u16) else {
            continue;
        };
        let collation =
            Collation::from_name(core::str::from_utf8(&column.collation).unwrap_or("BINARY"))
                .unwrap_or(Collation::Binary);
        substitutions.push((
            BoundExpr::Column {
                source: EXCLUDED_SOURCE,
                column: position as u16,
                affinity: column.affinity,
                collation,
            },
            *register,
        ));
    }
    substitutions.push((
        BoundExpr::Rowid {
            source: EXCLUDED_SOURCE,
        },
        rowid,
    ));
    substitutions
}

/// Returns the conflict clause an `INTEGER PRIMARY KEY` was declared with.
fn rowid_conflict(table: &TableInfo) -> Option<ConflictAction> {
    table
        .rowid_alias
        .and_then(|column| table.column(column))
        .and_then(|column| column.not_null_conflict)
}

/// Returns the key description an index cursor is opened with.
fn index_key(index: &IndexInfo) -> IndexKey {
    IndexKey {
        columns: index
            .columns
            .iter()
            .map(|key| SortColumn {
                descending: key.descending,
                nulls_first: true,
                collation: Collation::from_name(
                    core::str::from_utf8(&key.collation).unwrap_or("BINARY"),
                )
                .unwrap_or(Collation::Binary),
            })
            .collect(),
    }
}

/// Returns the substitutions a row image is compiled against.
///
/// A CHECK, a RETURNING and a DO UPDATE all read "the row being written",
/// which is a block of registers rather than a cursor. Binding produced
/// ordinary column references against source zero; these map them onto the
/// registers, so one expression compiler serves both.
fn row_substitutions(table: &TableInfo, values: &[u32], rowid: u32) -> Vec<(BoundExpr, u32)> {
    let mut substitutions = Vec::with_capacity(values.len().saturating_add(1));
    for (position, register) in values.iter().enumerate() {
        if table.rowid_alias == Some(position as u16) {
            continue;
        }
        let Some(column) = table.column(position as u16) else {
            continue;
        };
        let collation =
            Collation::from_name(core::str::from_utf8(&column.collation).unwrap_or("BINARY"))
                .unwrap_or(Collation::Binary);
        substitutions.push((
            BoundExpr::Column {
                source: 0,
                column: position as u16,
                affinity: column.affinity,
                collation,
            },
            *register,
        ));
    }
    substitutions.push((BoundExpr::Rowid { source: 0 }, rowid));
    substitutions
}

/// Returns the result columns a RETURNING clause reports.
fn returning_columns(columns: &[BoundResultColumn]) -> Vec<ResultColumn> {
    columns
        .iter()
        .map(|column| ResultColumn {
            name: column.name.clone(),
            origin: column.origin.clone(),
            declared_type: column.declared_type.clone(),
        })
        .collect()
}

/// Wraps a compiled body in the program header and trailer.
fn finish(
    compiler: Compiler,
    dependencies: ProgramDependencies,
    parameters: u32,
    result_columns: Vec<ResultColumn>,
) -> Program {
    Program {
        instructions: compiler.instructions,
        register_count: compiler.registers,
        cursor_count: compiler.cursors,
        sorter_count: compiler.sorters,
        distinct_count: 0,
        aggregate_count: 0,
        result_columns,
        dependencies,
        readonly: false,
        parameter_count: parameters,
    }
}

/// Compiles an `INSERT`.
pub fn compile_insert(
    insert: &BoundInsert,
    dependencies: ProgramDependencies,
    parameters: u32,
) -> DbResult<Program> {
    let mut compiler = Compiler::new();
    let entry = compiler.emit_jump(Instruction::new(Opcode::Init, 0, -1, 0));
    compiler.patch_here(entry);
    compiler.emit(Instruction::new(Opcode::Transaction, 0, 0, 0));
    let writer = compiler.open_for_write(&insert.table);
    let rows = match &insert.source {
        BoundInsertSource::Values(rows) => rows.clone(),
        BoundInsertSource::Select(_) => {
            return Err(error::misuse("INSERT ... SELECT is compiled separately"))
        }
    };
    for row in &rows {
        let mut sources = Vec::with_capacity(row.len());
        for value in row {
            sources.push(compiler.compile_expr(value)?);
        }
        compiler.emit_insert_row(&writer, insert, &sources)?;
    }
    let halt = compiler.here();
    compiler.emit(Instruction::new(Opcode::Halt, 0, 0, 0));
    for label in core::mem::take(&mut compiler.end_jumps) {
        compiler.patch(label, halt);
    }
    Ok(finish(
        compiler,
        dependencies,
        parameters,
        returning_columns(&insert.returning),
    ))
}

impl Compiler {
    /// Emits everything one inserted row costs.
    fn emit_insert_row(
        &mut self,
        writer: &Writer,
        insert: &BoundInsert,
        sources: &[u32],
    ) -> DbResult<()> {
        let table = &insert.table;
        let mut values = Vec::with_capacity(table.columns.len());
        for column in &insert.columns {
            let register = match column {
                ColumnSource::Row(index) => sources.get(*index).copied().unwrap_or(0),
                ColumnSource::Expr(expr) => self.compile_expr(expr)?,
            };
            values.push(register);
        }
        for (position, register) in values.iter().enumerate() {
            let Some(column) = table.column(position as u16) else {
                continue;
            };
            if table.rowid_alias == Some(position as u16) {
                continue;
            }
            self.emit(
                Instruction::new(Opcode::ApplyAffinity, *register as i32, 1, 0)
                    .with_p4(Operand::Affinity(column.affinity)),
            );
        }
        let rowid = self.emit_insert_rowid(writer, insert, &values)?;
        let mut skip = Vec::new();
        let previous = core::mem::replace(
            &mut self.substitutions,
            row_substitutions(table, &values, rowid),
        );
        let constraints = self.emit_row_constraints(
            table,
            &values,
            &insert.checks,
            insert.on_conflict,
            &mut skip,
        );
        self.substitutions = previous;
        constraints?;
        self.emit_unique_constraints(
            writer,
            table,
            &values,
            rowid,
            None,
            insert.on_conflict,
            insert.upsert.as_ref(),
            &insert.checks,
            &insert.returning,
            &mut skip,
        )?;
        self.emit_write_row(writer, table, &values, rowid)?;
        self.emit(Instruction::new(Opcode::CountChange, rowid as i32, 1, 0));
        let previous = core::mem::replace(
            &mut self.substitutions,
            row_substitutions(table, &values, rowid),
        );
        let returning = self.emit_returning(&insert.returning);
        self.substitutions = previous;
        returning?;
        for label in skip {
            self.patch_here(label);
        }
        Ok(())
    }

    /// Works out the rowid an inserted row is written under.
    fn emit_insert_rowid(
        &mut self,
        writer: &Writer,
        insert: &BoundInsert,
        values: &[u32],
    ) -> DbResult<u32> {
        let table = &insert.table;
        let Some(alias) = table.rowid_alias else {
            let rowid = self.register();
            self.emit(Instruction::new(
                Opcode::NewRowid,
                writer.table as i32,
                rowid as i32,
                0,
            ));
            return Ok(rowid);
        };
        let Some(supplied) = values.get(alias as usize).copied() else {
            let rowid = self.register();
            self.emit(Instruction::new(
                Opcode::NewRowid,
                writer.table as i32,
                rowid as i32,
                0,
            ));
            return Ok(rowid);
        };
        let rowid = self.register();
        self.emit(Instruction::new(
            Opcode::Copy,
            supplied as i32,
            rowid as i32,
            0,
        ));
        // A NULL means "choose one", and anything that is not an integer is a
        // type error rather than something to coerce: SQLite refuses to store
        // 'x' as a rowid even though it would happily store it in an INTEGER
        // column that is not the key.
        let supplied_value =
            self.emit_jump(Instruction::new(Opcode::IfNotNull, rowid as i32, -1, 0));
        self.emit(Instruction::new(
            Opcode::NewRowid,
            writer.table as i32,
            rowid as i32,
            0,
        ));
        let chosen = self.emit_jump(Instruction::new(Opcode::Goto, 0, -1, 0));
        self.patch_here(supplied_value);
        self.emit(
            Instruction::new(Opcode::ApplyAffinity, rowid as i32, 1, 0)
                .with_p4(Operand::Affinity(Affinity::Integer)),
        );
        let integer = self.register();
        self.emit(
            Instruction::new(Opcode::Cast, rowid as i32, integer as i32, 0)
                .with_p4(Operand::Affinity(Affinity::Integer)),
        );
        let equal = self.register();
        self.emit(
            Instruction::new(Opcode::Is, rowid as i32, integer as i32, equal as i32).with_p4(
                Operand::Comparison(crate::program::Comparison {
                    op: rustdb_sql::ast::BinaryOp::Equal,
                    affinity: None,
                    collation: Collation::Binary,
                }),
            ),
        );
        let ok = self.emit_jump(Instruction::new(Opcode::If, equal as i32, -1, 0));
        self.emit(
            Instruction::new(Opcode::HaltError, codes::MISMATCH, 0, CONFLICT_ABORT).with_p4(
                Operand::Text(
                    format!(
                        "datatype mismatch: {}.{} must be an integer",
                        String::from_utf8_lossy(&table.name),
                        table
                            .column(alias)
                            .map(|column| String::from_utf8_lossy(&column.name).into_owned())
                            .unwrap_or_default()
                    )
                    .into_bytes(),
                ),
            ),
        );
        self.patch_here(ok);
        self.patch_here(chosen);
        Ok(rowid)
    }
}

/// Compiles a `DELETE`.
pub fn compile_delete(
    delete: &BoundDelete,
    dependencies: ProgramDependencies,
    parameters: u32,
) -> DbResult<Program> {
    let mut compiler = Compiler::new();
    let entry = compiler.emit_jump(Instruction::new(Opcode::Init, 0, -1, 0));
    compiler.patch_here(entry);
    compiler.emit(Instruction::new(Opcode::Transaction, 0, 0, 0));
    let writer = compiler.open_for_write(&delete.table);
    let sorter = compiler.open_rowid_sorter();
    compiler.emit_collect_rowids(&writer, delete.filter.as_ref(), sorter)?;

    let empty = compiler.emit_jump(Instruction::new(Opcode::SorterSort, sorter as i32, -1, 0));
    let top = compiler.here();
    let rowid = compiler.register();
    compiler.emit(Instruction::new(
        Opcode::SorterColumn,
        sorter as i32,
        0,
        rowid as i32,
    ));
    let missing = compiler.emit_jump(Instruction::new(
        Opcode::NotExists,
        writer.table as i32,
        -1,
        rowid as i32,
    ));
    compiler.emit_returning(&delete.returning)?;
    compiler.emit_delete_current(&writer, &delete.table)?;
    compiler.emit(Instruction::new(Opcode::CountChange, rowid as i32, 0, 0));
    compiler.patch_here(missing);
    compiler.emit(Instruction::new(Opcode::SorterNext, sorter as i32, top, 0));
    compiler.patch_here(empty);
    let halt = compiler.here();
    compiler.emit(Instruction::new(Opcode::Halt, 0, 0, 0));
    for label in core::mem::take(&mut compiler.end_jumps) {
        compiler.patch(label, halt);
    }
    Ok(finish(
        compiler,
        dependencies,
        parameters,
        returning_columns(&delete.returning),
    ))
}

impl Compiler {
    /// Opens the sorter a two-pass write collects its row locators in.
    fn open_rowid_sorter(&mut self) -> u32 {
        let sorter = self.sorters;
        self.sorters = self.sorters.saturating_add(1);
        self.emit(
            Instruction::new(Opcode::SorterOpen, sorter as i32, 0, 0).with_p4(Operand::SortKey(
                SortKey {
                    columns: vec![SortColumn {
                        descending: false,
                        nulls_first: true,
                        collation: Collation::Binary,
                    }],
                },
            )),
        );
        sorter
    }

    /// Emits the scan that collects the rowids a write is going to change.
    fn emit_collect_rowids(
        &mut self,
        writer: &Writer,
        filter: Option<&BoundExpr>,
        sorter: u32,
    ) -> DbResult<()> {
        let end = self.emit_jump(Instruction::new(Opcode::Rewind, writer.table as i32, -1, 0));
        let top = self.here();
        let mut skip = None;
        if let Some(filter) = filter {
            let register = self.compile_expr(filter)?;
            skip = Some(
                self.emit_jump(Instruction::new(Opcode::IfNot, register as i32, -1, 0).with_p5(1)),
            );
        }
        let rowid = self.register();
        self.emit(Instruction::new(
            Opcode::Rowid,
            writer.table as i32,
            rowid as i32,
            0,
        ));
        self.emit(Instruction::new(
            Opcode::SorterInsert,
            sorter as i32,
            rowid as i32,
            1,
        ));
        if let Some(skip) = skip {
            self.patch_here(skip);
        }
        self.emit(Instruction::new(Opcode::Next, writer.table as i32, top, 0));
        self.patch_here(end);
        Ok(())
    }
}

/// Compiles an `UPDATE`.
pub fn compile_update(
    update: &BoundUpdate,
    dependencies: ProgramDependencies,
    parameters: u32,
) -> DbResult<Program> {
    let mut compiler = Compiler::new();
    let entry = compiler.emit_jump(Instruction::new(Opcode::Init, 0, -1, 0));
    compiler.patch_here(entry);
    compiler.emit(Instruction::new(Opcode::Transaction, 0, 0, 0));
    let writer = compiler.open_for_write(&update.table);
    let sorter = compiler.open_rowid_sorter();
    compiler.emit_collect_rowids(&writer, update.filter.as_ref(), sorter)?;

    let empty = compiler.emit_jump(Instruction::new(Opcode::SorterSort, sorter as i32, -1, 0));
    let top = compiler.here();
    let old_rowid = compiler.register();
    compiler.emit(Instruction::new(
        Opcode::SorterColumn,
        sorter as i32,
        0,
        old_rowid as i32,
    ));
    let missing = compiler.emit_jump(Instruction::new(
        Opcode::NotExists,
        writer.table as i32,
        -1,
        old_rowid as i32,
    ));
    compiler.emit_update_row(&writer, update, old_rowid)?;
    compiler.patch_here(missing);
    compiler.emit(Instruction::new(Opcode::SorterNext, sorter as i32, top, 0));
    compiler.patch_here(empty);
    let halt = compiler.here();
    compiler.emit(Instruction::new(Opcode::Halt, 0, 0, 0));
    for label in core::mem::take(&mut compiler.end_jumps) {
        compiler.patch(label, halt);
    }
    Ok(finish(
        compiler,
        dependencies,
        parameters,
        returning_columns(&update.returning),
    ))
}

impl Compiler {
    /// Emits everything one updated row costs.
    fn emit_update_row(
        &mut self,
        writer: &Writer,
        update: &BoundUpdate,
        old_rowid: u32,
    ) -> DbResult<()> {
        let table = &update.table;
        // The new values are computed before anything is deleted, because the
        // assignments read the old row through the cursor: `SET a = a + 1`
        // means the old `a`, and a row that had already been rewritten would
        // give a different answer.
        let mut values = Vec::with_capacity(table.columns.len());
        for position in 0..table.columns.len() as u16 {
            let assigned = update
                .assignments
                .iter()
                .find(|assignment: &&BoundAssignment| assignment.column == position);
            let register = match assigned {
                Some(assignment) => {
                    let value = self.compile_expr(&assignment.value)?;
                    let copy = self.register();
                    self.emit(Instruction::new(Opcode::Copy, value as i32, copy as i32, 0));
                    if let Some(column) = table.column(position) {
                        if table.rowid_alias != Some(position) {
                            self.emit(
                                Instruction::new(Opcode::ApplyAffinity, copy as i32, 1, 0)
                                    .with_p4(Operand::Affinity(column.affinity)),
                            );
                        }
                    }
                    copy
                }
                None => self.read_column(writer.table, table, position),
            };
            values.push(register);
        }
        let new_rowid = match table.rowid_alias {
            Some(alias) => values.get(alias as usize).copied().unwrap_or(old_rowid),
            None => old_rowid,
        };
        let mut skip = Vec::new();
        let previous = core::mem::replace(
            &mut self.substitutions,
            row_substitutions(table, &values, new_rowid),
        );
        let constraints = self.emit_row_constraints(
            table,
            &values,
            &update.checks,
            update.on_conflict,
            &mut skip,
        );
        self.substitutions = previous;
        constraints?;
        self.emit_unique_constraints(
            writer,
            table,
            &values,
            new_rowid,
            Some(old_rowid),
            update.on_conflict,
            None,
            &update.checks,
            &update.returning,
            &mut skip,
        )?;
        // The old row and its index entries go first: an index entry is keyed
        // by its values, so leaving the old one behind would leave the index
        // describing a row that no longer has those values.
        let gone = self.emit_jump(Instruction::new(
            Opcode::NotExists,
            writer.table as i32,
            -1,
            old_rowid as i32,
        ));
        self.emit_delete_current(writer, table)?;
        self.patch_here(gone);
        self.emit_write_row(writer, table, &values, new_rowid)?;
        self.emit(Instruction::new(
            Opcode::CountChange,
            new_rowid as i32,
            0,
            0,
        ));
        let previous = core::mem::replace(
            &mut self.substitutions,
            row_substitutions(table, &values, new_rowid),
        );
        let returning = self.emit_returning(&update.returning);
        self.substitutions = previous;
        returning?;
        for label in skip {
            self.patch_here(label);
        }
        Ok(())
    }
}

/// Returns the extended code a constraint halt carried.
pub fn extended_code(value: i32) -> ExtendedCode {
    ExtendedCode(value)
}
