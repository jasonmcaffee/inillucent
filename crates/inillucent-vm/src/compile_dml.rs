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

use inillucent_base::error::ExtendedCode;
use inillucent_base::{error, DbResult};
use inillucent_sql::ast::{BinaryOp, ConflictAction, TriggerTime};
use inillucent_sql::bind::{BoundExpr, BoundResultColumn};
use inillucent_sql::bind::{BoundSelect, EXCLUDED_SOURCE, NEW_SOURCE, OLD_SOURCE};
use inillucent_sql::catalog_view::{IndexInfo, IndexOrigin, TableInfo, TableKind};
use inillucent_sql::dml::{
    BoundAssignment, BoundCheck, BoundDelete, BoundInsert, BoundInsertSource, BoundTrigger,
    BoundTriggerStatement, BoundUpdate, BoundUpsert, ColumnSource,
};
use inillucent_sql::plan::{self, BoundKind, RangeBound};
use inillucent_value::{Affinity, Collation};

use crate::compile::{named_columns, Compiler, Label, Sink};
use crate::program::Comparison;
use crate::program::{
    IndexKey, Instruction, Opcode, Operand, Program, ProgramDependencies, ResultColumn,
    RowChangeKind, SortColumn, SortKey, StrictType,
};

// The constraint codes and the messages that go with them are the binder's,
// so that this compiler and the vectorised executor's write path report the
// same text and the same extended code for the same violated constraint. Two
// copies would agree right up until one of them was corrected.
pub(crate) use inillucent_sql::dml::{codes, unique_message};

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
pub(crate) fn conflict_code(action: ConflictAction) -> i32 {
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
    fn open_for_write(&mut self, table: &TableInfo, source: usize) -> Writer {
        let cursor = self.cursors;
        self.cursors = self.cursors.saturating_add(1);
        if table.without_rowid {
            // The root is an index b-tree keyed by the primary key, so it is
            // opened as one and written with the index opcodes.
            self.emit(
                Instruction::new(
                    Opcode::OpenWriteIndex,
                    cursor as i32,
                    table.root as i32,
                    table.database as i32,
                )
                .with_p4(Operand::IndexKey(crate::compile::primary_key_of(table))),
            );
        } else {
            self.emit(
                Instruction::new(
                    Opcode::OpenWrite,
                    cursor as i32,
                    table.root as i32,
                    table.database as i32,
                )
                .with_p4(Operand::Count(table.columns.len() as u32)),
            );
        }
        // Registered under the statement's own number for this term, not at
        // slot zero. Two fires of one trigger open two cursors on the same
        // table, and clobbering slot zero left the second fire's expressions
        // reading the cursor the first fire had opened.
        if table.without_rowid {
            self.register_source(source, crate::compile::SourceCursors::index_only(cursor));
        } else {
            self.register_source(source, crate::compile::SourceCursors::table_only(cursor));
        }
        let mut indexes = Vec::new();
        let mut definitions = Vec::new();
        for index in &table.indexes {
            if index.root == 0 {
                continue;
            }
            if table.without_rowid && index.root == table.root {
                // The primary key of a WITHOUT ROWID table is the table's own
                // b-tree, already open above. Its uniqueness is checked against
                // that cursor by `emit_primary_key_constraint` rather than as
                // one more secondary index.
                continue;
            }
            let slot = self.cursors;
            self.cursors = self.cursors.saturating_add(1);
            self.emit(
                Instruction::new(
                    Opcode::OpenWriteIndex,
                    slot as i32,
                    index.root as i32,
                    table.database as i32,
                )
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

    /// Counts one changed row, and logs it for the update hook.
    fn emit_count_change(
        &mut self,
        table: &TableInfo,
        rowid: u32,
        kind: RowChangeKind,
        is_insert: bool,
    ) {
        self.emit(
            Instruction::new(
                Opcode::CountChange,
                rowid as i32,
                i32::from(is_insert),
                kind.as_operand(),
            )
            .with_p4(Operand::Change(kind, table.name.clone()))
            .with_p5(u16::from(self.firing_depth > 0)),
        );
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
        if table.without_rowid {
            // Read through the index opcode: the cursor is an index cursor, and
            // the slot is where the primary-key-first permutation put it.
            let slot = table.record_slot(column).unwrap_or(usize::from(column));
            let widen = table
                .column(column)
                .is_some_and(|info| info.affinity == Affinity::Real);
            self.emit(
                Instruction::new(
                    Opcode::IdxColumn,
                    cursor as i32,
                    slot as i32,
                    register as i32,
                )
                .with_p5(u16::from(widen)),
            );
            return register;
        }
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
        let trailing = if table.without_rowid {
            table.primary_key().len()
        } else {
            1
        };
        let width = index.columns.len().saturating_add(trailing);
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
        // A rowid table's index entry ends with the rowid; a WITHOUT ROWID
        // table has none, so its entries end with the primary key instead -
        // which is how a seek on a secondary index finds the row.
        let trailing: Vec<u16> = if table.without_rowid {
            table.primary_key()
        } else {
            Vec::new()
        };
        if table.without_rowid {
            for (offset, position) in trailing.iter().enumerate() {
                let Some(source) = values.get(usize::from(*position)).copied() else {
                    return Err(error::misuse(
                        "the primary key names a column the table has not",
                    ));
                };
                let target = block.saturating_add(index.columns.len() as u32 + offset as u32);
                self.emit(Instruction::new(
                    Opcode::Copy,
                    source as i32,
                    target as i32,
                    0,
                ));
            }
        } else {
            let rowid_slot = block.saturating_add(index.columns.len() as u32);
            self.emit(Instruction::new(
                Opcode::Copy,
                rowid as i32,
                rowid_slot as i32,
                0,
            ));
        }
        let keys = index.columns.iter().map(|key| {
            key.column
                .and_then(|column| table.column(column))
                .map_or(Affinity::Blob, |column| column.affinity)
        });
        let affinities: Vec<Affinity> = if table.without_rowid {
            keys.chain(trailing.iter().map(|position| {
                table
                    .column(*position)
                    .map_or(Affinity::Blob, |column| column.affinity)
            }))
            .collect()
        } else {
            keys.chain(core::iter::once(Affinity::Integer)).collect()
        };
        let record = self.register();
        self.emit(
            Instruction::new(
                Opcode::MakeRecord,
                block as i32,
                affinities.len() as i32,
                record as i32,
            )
            .with_p4(Operand::Affinities(affinities)),
        );
        Ok(record)
    }

    /// Returns, per index, whether this UPDATE can leave its entries alone.
    ///
    /// An index entry is keyed by the index's own columns with the rowid behind
    /// them, so an UPDATE that assigns none of those columns and does not move
    /// the row produces exactly the entry that is already there. Deleting it and
    /// inserting it back is then a descent, a balance and two page edits to
    /// arrive at the same bytes - and it was the whole cost of the statement:
    /// `UPDATE side_table SET note = ?2 WHERE id = ?1`, whose only index is on
    /// `owner`, measured 6.9 microseconds in `IdxDelete` and 12.7 in `IdxInsert`
    /// out of 22 for the statement. SQLite decides the same thing from `aXRef`.
    ///
    /// Everything that is not plainly safe is refused rather than reasoned
    /// about, because being wrong here leaves an index disagreeing with its
    /// table and nothing reads wrong until much later:
    ///
    /// - a partial index is never skipped: its predicate can name any column,
    ///   so any assignment may move the row into or out of the index;
    /// - an index with an expression key is never skipped, for the same reason;
    /// - a WITHOUT ROWID table is never skipped, because its index entries
    ///   carry the primary key rather than a rowid;
    /// - an assignment to the rowid alias moves every entry, so nothing is
    ///   skipped.
    ///
    /// @param table - the table being updated
    /// @param update - the statement, for the columns it assigns
    fn unaffected_indexes(&self, table: &TableInfo, update: &BoundUpdate) -> Vec<bool> {
        let definitions = &table.indexes;
        if table.without_rowid {
            return vec![false; definitions.len()];
        }
        let assigns = |column: u16| {
            update
                .assignments
                .iter()
                .any(|assignment| assignment.column == column)
        };
        if table.rowid_alias.is_some_and(assigns) {
            return vec![false; definitions.len()];
        }
        definitions
            .iter()
            .map(|index| {
                if index.partial_sql.is_some() {
                    return false;
                }
                index.columns.iter().all(|key| match key.column {
                    Some(column) if key.expr_sql.is_none() => !assigns(column),
                    _ => false,
                })
            })
            .collect()
    }

    /// Emits the deletion of the row a table cursor is sitting on, index
    /// entries first.
    ///
    /// The index entries have to go first because building one needs the row's
    /// column values, and after the row is gone the cursor has nothing to read
    /// them from.
    fn emit_delete_current(&mut self, writer: &Writer, table: &TableInfo) -> DbResult<()> {
        let rowid = self.register();
        if table.without_rowid {
            // There is no rowid to read; the register exists so the index-entry
            // builder has something to copy where a rowid table would put one,
            // and for a WITHOUT ROWID table it puts the key columns instead.
            self.emit(Instruction::new(Opcode::Null, 0, rowid as i32, 0));
        } else {
            self.emit(Instruction::new(
                Opcode::Rowid,
                writer.table as i32,
                rowid as i32,
                0,
            ));
        }
        let values: Vec<u32> = (0..table.columns.len() as u16)
            .map(|column| self.read_column(writer.table, table, column))
            .collect();
        for (position, index) in writer.definitions.iter().enumerate() {
            let Some(cursor) = writer.indexes.get(position).copied() else {
                continue;
            };
            if self
                .untouched_indexes
                .get(position)
                .copied()
                .unwrap_or(false)
            {
                continue;
            }
            let record = self.emit_index_record(index, &values, rowid, table)?;
            self.emit(Instruction::new(
                Opcode::IdxDelete,
                cursor as i32,
                record as i32,
                0,
            ));
        }
        if table.without_rowid {
            // The entry is deleted by its key, which is the record itself.
            let record = self.emit_table_record(table, &values)?;
            self.emit(Instruction::new(
                Opcode::IdxDelete,
                writer.table as i32,
                record as i32,
                0,
            ));
        } else {
            self.emit(Instruction::new(
                Opcode::DeleteRow,
                writer.table as i32,
                0,
                0,
            ));
        }
        Ok(())
    }

    /// Deletes the row a `REPLACE` is making room for, firing the foreign-key
    /// actions its removal implies.
    ///
    /// The row is read before it goes, because that copy is what `OLD` means to
    /// the actions - a cascade has to know which children to take with it, and
    /// after the delete the cursor has nothing left to read.
    fn emit_replace_delete(&mut self, writer: &Writer, table: &TableInfo) -> DbResult<()> {
        if self.replace_triggers.is_empty() {
            return self.emit_delete_current(writer, table);
        }
        let rowid = self.register();
        if table.without_rowid {
            self.emit(Instruction::new(Opcode::Null, 0, rowid as i32, 0));
        } else {
            self.emit(Instruction::new(
                Opcode::Rowid,
                writer.table as i32,
                rowid as i32,
                0,
            ));
        }
        let old = self.read_row_image(writer, table, rowid);
        let triggers = core::mem::take(&mut self.replace_triggers);
        let before = self.emit_triggers(&triggers, TriggerTime::Before, table, Some(&old), None);
        let deleted = before.and_then(|()| self.emit_delete_current(writer, table));
        let after = deleted.and_then(|()| {
            self.emit_triggers(&triggers, TriggerTime::After, table, Some(&old), None)
        });
        self.replace_triggers = triggers;
        after
    }

    /// Builds the record a `WITHOUT ROWID` table's entry is, from its columns.
    ///
    /// The same permutation `emit_write_row` uses, because a delete has to
    /// present the entry byte for byte the way the insert wrote it.
    fn emit_table_record(&mut self, table: &TableInfo, values: &[u32]) -> DbResult<u32> {
        let order = table.record_order();
        let block = self.register_block(order.len().max(1));
        for (slot, position) in order.iter().enumerate() {
            let target = block.saturating_add(slot as u32);
            let Some(source) = values.get(usize::from(*position)).copied() else {
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
        let affinities = order
            .iter()
            .map(|position| {
                table
                    .column(*position)
                    .map_or(Affinity::Blob, |column| column.affinity)
            })
            .collect();
        let record = self.register();
        self.emit(
            Instruction::new(
                Opcode::MakeRecord,
                block as i32,
                order.len() as i32,
                record as i32,
            )
            .with_p4(Operand::Affinities(affinities)),
        );
        Ok(record)
    }

    /// Emits the type test a `STRICT` table's columns owe.
    ///
    /// The affinity is applied first, because STRICT checks the storage class
    /// the value will actually be stored as: `'123'` written to an `INT` column
    /// is an integer by the time it reaches the record, and refusing it before
    /// the conversion would refuse a value SQLite accepts.
    fn emit_strict_checks(&mut self, table: &TableInfo, values: &[u32]) -> DbResult<()> {
        for (position, column) in table.columns.iter().enumerate() {
            let Some(register) = values.get(position).copied() else {
                continue;
            };
            let Some(kind) = StrictType::of(&column.declared_type) else {
                continue;
            };
            if kind == StrictType::Any {
                continue;
            }
            self.emit(
                Instruction::new(Opcode::ApplyAffinity, register as i32, 1, 0)
                    .with_p4(Operand::Affinity(column.affinity)),
            );
            let name = format!(
                "{}.{}",
                String::from_utf8_lossy(&table.name),
                String::from_utf8_lossy(&column.name)
            );
            self.emit(
                Instruction::new(Opcode::TypeCheck, register as i32, 0, 0)
                    .with_p4(Operand::Strict(kind, name.into_bytes())),
            );
        }
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
        if table.strict {
            self.emit_strict_checks(table, values)?;
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
                            op: inillucent_sql::ast::BinaryOp::Equal,
                            affinity: Some(Affinity::Integer),
                            collation: Collation::Binary,
                        })),
                );
                same = Some(self.emit_jump(Instruction::new(Opcode::If, equal as i32, -1, 0)));
            }
            let code = if index.origin == inillucent_sql::catalog_view::IndexOrigin::PrimaryKey {
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
                    self.emit_replace_delete(writer, table)?;
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
                        op: inillucent_sql::ast::BinaryOp::Equal,
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
                self.emit_replace_delete(writer, table)?;
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
        self.emit_count_change(table, new_rowid, RowChangeKind::Update, false);
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

    /// Computes every generated column of a row image, in dependency order.
    ///
    /// A generated column's expression reads other columns of the same row, so
    /// it is compiled against the row's *registers* rather than against a
    /// cursor - the row is not in the table yet. One generated column may read
    /// another, so the pass repeats until nothing new can be computed; anything
    /// still outstanding is part of a cycle, which the table's creation
    /// refuses, and it keeps the NULL it was given rather than looping.
    fn emit_generated_values(
        &mut self,
        table: &TableInfo,
        generated: &[(usize, BoundExpr)],
        values: &mut [u32],
        rowid: u32,
    ) -> DbResult<()> {
        if generated.is_empty() {
            return Ok(());
        }
        let mut ready: Vec<usize> = (0..table.columns.len())
            .filter(|position| {
                table
                    .column(*position as u16)
                    .is_some_and(|column| !column.generated)
            })
            .collect();
        let mut pending: Vec<(usize, BoundExpr)> = generated.to_vec();
        let mut rounds = 0usize;
        while !pending.is_empty() && rounds <= generated.len() {
            rounds = rounds.saturating_add(1);
            let mut progressed = false;
            let mut still = Vec::new();
            for (position, expr) in pending {
                let mut reads = Vec::new();
                expr.columns_used(&mut reads);
                let satisfied = reads.iter().all(|column| {
                    ready.contains(&usize::from(*column)) || table.rowid_alias == Some(*column)
                });
                if !satisfied {
                    still.push((position, expr));
                    continue;
                }
                let previous = core::mem::replace(
                    &mut self.substitutions,
                    row_substitutions_for(table, values, rowid, &ready),
                );
                let compiled = self.compile_expr(&expr);
                self.substitutions = previous;
                let register = compiled?;
                let target = self.register();
                self.emit(Instruction::new(
                    Opcode::Copy,
                    register as i32,
                    target as i32,
                    0,
                ));
                if let Some(column) = table.column(position as u16) {
                    self.emit(
                        Instruction::new(Opcode::ApplyAffinity, target as i32, 1, 0)
                            .with_p4(Operand::Affinity(column.affinity)),
                    );
                }
                if let Some(slot) = values.get_mut(position) {
                    *slot = target;
                }
                ready.push(position);
                progressed = true;
            }
            pending = still;
            if !progressed {
                break;
            }
        }
        Ok(())
    }

    /// Emits the record for a table row and writes it, indexes included.
    fn emit_write_row(
        &mut self,
        writer: &Writer,
        table: &TableInfo,
        values: &[u32],
        rowid: u32,
    ) -> DbResult<()> {
        // The record holds one slot per *stored* column: a VIRTUAL generated
        // column is computed on read and takes none, so packing by declared
        // position would leave a hole and shift every column after it.
        let width = table.record_width().max(1);
        let block = self.register_block(width);
        // In *record* order, which is declaration order for a rowid table and
        // the primary key followed by the rest for a WITHOUT ROWID one.
        let order = table.record_order();
        for (slot, position) in order.iter().enumerate() {
            let target = block.saturating_add(slot as u32);
            if table.rowid_alias == Some(*position) {
                // The rowid is the row's key, not one of its fields; SQLite
                // stores a NULL in the record and reads the key back instead.
                self.emit(Instruction::new(Opcode::Null, 0, target as i32, 0));
                continue;
            }
            let Some(source) = values.get(usize::from(*position)).copied() else {
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
        let affinities = order
            .iter()
            .map(|position| {
                if table.rowid_alias == Some(*position) {
                    Affinity::Blob
                } else {
                    table
                        .column(*position)
                        .map_or(Affinity::Blob, |column| column.affinity)
                }
            })
            .collect();
        let record = self.register();
        self.emit(
            Instruction::new(
                Opcode::MakeRecord,
                block as i32,
                table.record_width() as i32,
                record as i32,
            )
            .with_p4(Operand::Affinities(affinities)),
        );
        if table.without_rowid {
            // The record *is* the entry: the key is its leading primary-key
            // columns and the rest of the row rides along behind them.
            self.emit(Instruction::new(
                Opcode::IdxInsert,
                writer.table as i32,
                record as i32,
                0,
            ));
        } else {
            self.emit(Instruction::new(
                Opcode::InsertRow,
                writer.table as i32,
                record as i32,
                rowid as i32,
            ));
        }
        for (position, index) in writer.definitions.clone().iter().enumerate() {
            let Some(cursor) = writer.indexes.get(position).copied() else {
                continue;
            };
            if self
                .untouched_indexes
                .get(position)
                .copied()
                .unwrap_or(false)
            {
                continue;
            }
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
                slot: table.record_slot(position as u16).unwrap_or(position) as u16,
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

/// Returns the substitutions for the columns of a row that have values yet.
///
/// A generated column that has not been computed is deliberately absent: with
/// it present, an expression that read it would compile to the placeholder NULL
/// and quietly produce the wrong answer instead of failing to be ordered.
fn row_substitutions_for(
    table: &TableInfo,
    values: &[u32],
    rowid: u32,
    ready: &[usize],
) -> Vec<(BoundExpr, u32)> {
    row_substitutions(table, values, rowid)
        .into_iter()
        .filter(|(expr, _)| match expr {
            BoundExpr::Column { column, .. } => ready.contains(&usize::from(*column)),
            _ => true,
        })
        .collect()
}

/// One row a trigger's `OLD` or `NEW` names, as the registers holding it.
pub(crate) struct RowImage {
    /// One register per declared column, in declaration order.
    pub values: Vec<u32>,
    /// The register holding the row's rowid.
    pub rowid: u32,
}

/// Returns the substitutions one of a trigger's row aliases is compiled against.
///
/// The same mapping `excluded` uses, under a different source number: the row
/// is a block of registers rather than a cursor, so every reference to it has
/// to be substituted before the compiler tries to open a cursor for it.
fn alias_substitutions(
    source: usize,
    table: &TableInfo,
    image: &RowImage,
) -> Vec<(BoundExpr, u32)> {
    let mut substitutions = Vec::with_capacity(image.values.len().saturating_add(1));
    for (position, register) in image.values.iter().enumerate() {
        let Some(column) = table.column(position as u16) else {
            continue;
        };
        if table.rowid_alias == Some(position as u16) {
            // The alias *is* the rowid, and reading it out of the row image
            // would answer NULL for the record slot SQLite leaves empty.
            substitutions.push((
                BoundExpr::Column {
                    source,
                    column: position as u16,
                    slot: position as u16,
                    affinity: column.affinity,
                    collation: Collation::Binary,
                },
                image.rowid,
            ));
            continue;
        }
        let collation =
            Collation::from_name(core::str::from_utf8(&column.collation).unwrap_or("BINARY"))
                .unwrap_or(Collation::Binary);
        substitutions.push((
            BoundExpr::Column {
                source,
                column: position as u16,
                slot: position as u16,
                affinity: column.affinity,
                collation,
            },
            *register,
        ));
    }
    substitutions.push((BoundExpr::Rowid { source }, image.rowid));
    substitutions
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
                slot: table.record_slot(position as u16).unwrap_or(position) as u16,
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

impl Compiler {
    /// Emits the triggers of one time that a write fires, in schema order.
    ///
    /// The substitution list is *replaced* rather than extended: inside a
    /// trigger body only `OLD` and `NEW` live in registers, and leaving the
    /// firing statement's own row image visible would let a body statement read
    /// the wrong table's registers for a column that happened to match.
    fn emit_triggers(
        &mut self,
        triggers: &[BoundTrigger],
        time: TriggerTime,
        table: &TableInfo,
        old: Option<&RowImage>,
        new: Option<&RowImage>,
    ) -> DbResult<()> {
        if triggers.is_empty() {
            return Ok(());
        }
        let mut aliases = Vec::new();
        if let Some(image) = old {
            aliases.extend(alias_substitutions(OLD_SOURCE, table, image));
        }
        if let Some(image) = new {
            aliases.extend(alias_substitutions(NEW_SOURCE, table, image));
        }
        // `last_insert_rowid()` sees a trigger body's own inserts while the body
        // runs and reverts afterwards - SQLite gets that from the frame it
        // pushes, and an inlined body has to save and restore it by hand.
        let saved_rowid = self.register();
        self.emit(Instruction::new(
            Opcode::LastRowid,
            saved_rowid as i32,
            0,
            0,
        ));
        for trigger in triggers.iter().filter(|trigger| trigger.time == time) {
            let previous = core::mem::replace(&mut self.substitutions, aliases.clone());
            let outcome = self.emit_trigger_body(trigger);
            self.substitutions = previous;
            outcome?;
        }
        self.emit(Instruction::new(
            Opcode::LastRowid,
            saved_rowid as i32,
            1,
            0,
        ));
        Ok(())
    }

    /// Emits one trigger's `WHEN` guard and its body statements.
    fn emit_trigger_body(&mut self, trigger: &BoundTrigger) -> DbResult<()> {
        let skip =
            match &trigger.when {
                Some(guard) => {
                    let register = self.compile_expr(guard)?;
                    // `p5` of 1 also jumps on NULL: an unknown guard does not fire,
                    // which is the same rule a WHERE clause follows.
                    Some(self.emit_jump(
                        Instruction::new(Opcode::IfNot, register as i32, -1, 0).with_p5(1),
                    ))
                }
                None => None,
            };
        self.firing_depth = self.firing_depth.saturating_add(1);
        let mut outcome = Ok(());
        for statement in &trigger.body {
            outcome = match statement {
                BoundTriggerStatement::Insert(insert) => self.emit_insert_body(insert),
                BoundTriggerStatement::Update(update) => self.emit_update_body(update),
                BoundTriggerStatement::Delete(delete) => self.emit_delete_body(delete),
                BoundTriggerStatement::Select(select) => self.emit_discarded_select(select),
            };
            if outcome.is_err() {
                break;
            }
        }
        self.firing_depth = self.firing_depth.saturating_sub(1);
        outcome?;
        if let Some(label) = skip {
            self.patch_here(label);
        }
        Ok(())
    }

    /// Runs a trigger body's `SELECT` for its effects and throws the rows away.
    ///
    /// A trigger body cannot return rows to the caller, so the block is compiled
    /// into an ephemeral nobody reads. The rows still have to be *produced*: the
    /// reason to write a SELECT in a trigger body is the `RAISE()` in it, and a
    /// query that was optimised away would never reach it.
    fn emit_discarded_select(&mut self, select: &BoundSelect) -> DbResult<()> {
        let mut plan = inillucent_sql::plan::plan_select_with(select.clone(), self.levers);
        self.resolve_plan(&mut plan)?;
        let width = plan.select.columns.len().max(1);
        let store = self.ephemeral();
        self.emit(Instruction::new(
            Opcode::EphOpen,
            store as i32,
            width as i32,
            0,
        ));
        self.open_all_cursors(&plan)?;
        self.compile_block(&plan, Sink::Store(store))?;
        self.emit(Instruction::new(Opcode::EphClear, store as i32, 0, 0));
        Ok(())
    }
}

/// Wraps a compiled body in the program header and trailer.
fn finish(
    compiler: Compiler,
    dependencies: ProgramDependencies,
    parameters: u32,
    result_columns: Vec<ResultColumn>,
) -> Program {
    // Counted off the compiler rather than assumed to be zero. A DML statement
    // had no use for an ephemeral, a DISTINCT set or an accumulator until it
    // could fire a trigger and run `INSERT ... SELECT`; a hard zero here made
    // the verifier reject the program it had just produced, which is exactly
    // what the verifier is for.
    Program {
        ephemeral_count: compiler.ephemerals,
        instructions: compiler.instructions,
        register_count: compiler.registers,
        cursor_count: compiler.cursors,
        sorter_count: compiler.sorters,
        distinct_count: compiler.distincts,
        aggregate_count: compiler.aggregates,
        result_columns,
        dependencies,
        readonly: false,
        optimizations_used: compiler.used,
        parameter_count: parameters,
    }
}

/// Compiles an `INSERT`.
pub fn compile_insert(
    insert: &BoundInsert,
    dependencies: ProgramDependencies,
    parameters: u32,
) -> DbResult<Program> {
    compile_insert_with(insert, dependencies, parameters, None)
}

/// As [`compile_insert`], with somebody to ask about virtual tables.
pub fn compile_insert_with(
    insert: &BoundInsert,
    dependencies: ProgramDependencies,
    parameters: u32,
    planner: Option<Box<dyn crate::compile::VirtualPlanner>>,
) -> DbResult<Program> {
    let mut compiler = Compiler::with_levers(plan::Levers::without(dependencies.levers));
    if let Some(planner) = planner {
        compiler = compiler.with_virtual_planner(planner);
    }
    let entry = compiler.emit_jump(Instruction::new(Opcode::Init, 0, -1, 0));
    compiler.patch_here(entry);
    compiler.emit(Instruction::new(Opcode::Transaction, 0, 0, 0));
    compiler.emit_insert_body(insert)?;
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
    /// Emits a whole `INSERT`: its cursors, its rows, and the triggers they fire.
    ///
    /// It is a method rather than the body of [`compile_insert`] because a
    /// trigger body inlines one of these into the middle of another statement,
    /// and the only difference between the two cases is the program header.
    fn emit_insert_body(&mut self, insert: &BoundInsert) -> DbResult<()> {
        let outer = core::mem::replace(&mut self.replace_triggers, insert.replace_triggers.clone());
        let outcome = self.emit_insert_body_inner(insert);
        self.replace_triggers = outer;
        outcome
    }

    /// The body of [`Compiler::emit_insert_body`], with the replace actions in
    /// place.
    fn emit_insert_body_inner(&mut self, insert: &BoundInsert) -> DbResult<()> {
        let mut exprs: Vec<&BoundExpr> = Vec::new();
        exprs.extend(insert.checks.iter().map(|check| &check.expr));
        exprs.extend(insert.returning.iter().map(|column| &column.expr));
        for column in &insert.columns {
            if let ColumnSource::Expr(expr) | ColumnSource::Generated(expr) = column {
                exprs.push(expr);
            }
        }
        if let BoundInsertSource::Values(rows) = &insert.source {
            for row in rows {
                exprs.extend(row.iter());
            }
        }
        self.open_subqueries_in(&exprs)?;
        if insert.table.kind == TableKind::View {
            return self.emit_view_insert(insert);
        }
        if insert.table.module.is_some() {
            return self.emit_virtual_insert(insert);
        }
        let writer = self.open_for_write(&insert.table, insert.target_source);
        match &insert.source {
            BoundInsertSource::Values(rows) => {
                let rows = rows.clone();
                for row in &rows {
                    let mut sources = Vec::with_capacity(row.len());
                    for value in row {
                        sources.push(self.compile_expr(value)?);
                    }
                    self.emit_insert_row(&writer, insert, &sources)?;
                }
                Ok(())
            }
            BoundInsertSource::Select(select) => {
                self.emit_insert_from_select(&writer, insert, select)
            }
        }
    }

    /// Emits `INSERT INTO t SELECT ...`.
    ///
    /// The query's rows are collected into an ephemeral first and written from
    /// there, rather than written as they are produced. That is not laziness: a
    /// query is allowed to read the table being written - `INSERT INTO t SELECT
    /// * FROM t` doubles a table - and a row written under the scan that is
    /// reading it would be read again, forever. SQLite reaches for the same
    /// temporary table whenever it cannot prove the two are unrelated; doing it
    /// always costs a copy and can never be wrong.
    fn emit_insert_from_select(
        &mut self,
        writer: &Writer,
        insert: &BoundInsert,
        select: &BoundSelect,
    ) -> DbResult<()> {
        let mut plan = inillucent_sql::plan::plan_select_with(select.clone(), self.levers);
        self.resolve_plan(&mut plan)?;
        let width = insert.arity.max(1);
        let store = self.ephemeral();
        self.emit(Instruction::new(
            Opcode::EphOpen,
            store as i32,
            width as i32,
            0,
        ));
        self.open_all_cursors(&plan)?;
        self.compile_block(&plan, Sink::Store(store))?;

        let block = self.register_block(width);
        let empty = self.emit_jump(Instruction::new(Opcode::EphRewind, store as i32, -1, 0));
        let top = self.here();
        let mut sources = Vec::with_capacity(width);
        for index in 0..width {
            let register = block.saturating_add(index as u32);
            self.emit(Instruction::new(
                Opcode::EphColumn,
                store as i32,
                index as i32,
                register as i32,
            ));
            sources.push(register);
        }
        self.emit_insert_row(writer, insert, &sources)?;
        let more = self.emit_jump(Instruction::new(Opcode::EphNext, store as i32, -1, 0));
        self.patch(more, top);
        self.patch_here(empty);
        self.emit(Instruction::new(Opcode::EphClear, store as i32, 0, 0));
        Ok(())
    }

    /// Emits a whole `DELETE`: the rowid pass, then the row pass.
    fn emit_delete_body(&mut self, delete: &BoundDelete) -> DbResult<()> {
        let mut exprs: Vec<&BoundExpr> = Vec::new();
        exprs.extend(delete.filter.as_ref());
        exprs.extend(delete.limit.as_ref());
        exprs.extend(delete.offset.as_ref());
        exprs.extend(delete.returning.iter().map(|column| &column.expr));
        self.open_subqueries_in(&exprs)?;
        if let Some(rows) = delete.view_rows.as_ref() {
            return self.emit_view_write(&delete.table, rows, &delete.triggers, None);
        }
        if delete.table.module.is_some() {
            return self.emit_virtual_delete(delete);
        }
        if delete.table.without_rowid {
            return self.emit_keyed_write(
                &delete.table,
                delete.source,
                delete.filter.as_ref(),
                None,
                Some(delete),
            );
        }
        let writer = self.open_for_write(&delete.table, delete.source);
        let sorter = self.open_rowid_sorter();
        self.emit_collect_rowids(
            &writer,
            &delete.table,
            delete.source,
            delete.filter.as_ref(),
            sorter,
        )?;

        let empty = self.emit_jump(Instruction::new(Opcode::SorterSort, sorter as i32, -1, 0));
        let top = self.here();
        let rowid = self.register();
        self.emit(Instruction::new(
            Opcode::SorterColumn,
            sorter as i32,
            0,
            rowid as i32,
        ));
        let missing = self.emit_jump(Instruction::new(
            Opcode::NotExists,
            writer.table as i32,
            -1,
            rowid as i32,
        ));
        self.emit_returning(&delete.returning)?;
        // The row has to be read before it is deleted: OLD is the row that was
        // there, and after the delete the cursor no longer points at it.
        let old = self.read_row_image(&writer, &delete.table, rowid);
        let ignored = core::mem::take(&mut self.ignore_jumps);
        self.emit_triggers(
            &delete.triggers,
            TriggerTime::Before,
            &delete.table,
            Some(&old),
            None,
        )?;
        self.emit_delete_current(&writer, &delete.table)?;
        self.emit_count_change(&delete.table, rowid, RowChangeKind::Delete, false);
        self.emit_triggers(
            &delete.triggers,
            TriggerTime::After,
            &delete.table,
            Some(&old),
            None,
        )?;
        for label in core::mem::replace(&mut self.ignore_jumps, ignored) {
            self.patch_here(label);
        }
        self.patch_here(missing);
        self.emit(Instruction::new(Opcode::SorterNext, sorter as i32, top, 0));
        self.patch_here(empty);
        Ok(())
    }

    /// Emits a whole `UPDATE`: the rowid pass, then the row pass.
    fn emit_update_body(&mut self, update: &BoundUpdate) -> DbResult<()> {
        let mut exprs: Vec<&BoundExpr> = Vec::new();
        exprs.extend(update.filter.as_ref());
        exprs.extend(update.limit.as_ref());
        exprs.extend(update.offset.as_ref());
        exprs.extend(update.assignments.iter().map(|set| &set.value));
        exprs.extend(update.checks.iter().map(|check| &check.expr));
        exprs.extend(update.returning.iter().map(|column| &column.expr));
        self.open_subqueries_in(&exprs)?;
        if let Some(rows) = update.view_rows.as_ref() {
            return self.emit_view_write(
                &update.table,
                rows,
                &update.triggers,
                Some(&update.assignments),
            );
        }
        if update.table.module.is_some() {
            return self.emit_virtual_update(update);
        }
        if update.table.without_rowid {
            return self.emit_keyed_write(
                &update.table,
                update.source,
                update.filter.as_ref(),
                Some(update),
                None,
            );
        }
        let writer = self.open_for_write(&update.table, update.source);
        let sorter = self.open_rowid_sorter();
        self.emit_collect_rowids(
            &writer,
            &update.table,
            update.source,
            update.filter.as_ref(),
            sorter,
        )?;

        let empty = self.emit_jump(Instruction::new(Opcode::SorterSort, sorter as i32, -1, 0));
        let top = self.here();
        let old_rowid = self.register();
        self.emit(Instruction::new(
            Opcode::SorterColumn,
            sorter as i32,
            0,
            old_rowid as i32,
        ));
        let missing = self.emit_jump(Instruction::new(
            Opcode::NotExists,
            writer.table as i32,
            -1,
            old_rowid as i32,
        ));
        self.emit_update_row(&writer, update, old_rowid)?;
        self.patch_here(missing);
        self.emit(Instruction::new(Opcode::SorterNext, sorter as i32, top, 0));
        self.patch_here(empty);
        Ok(())
    }

    /// Emits an `INSERT` into a view, which is its `INSTEAD OF` trigger.
    ///
    /// Nothing is written and no cursor is opened: the trigger *is* the write.
    /// The values are still computed, and still get the view's declared
    /// affinities applied, because that is what `NEW` hands the body.
    fn emit_view_insert(&mut self, insert: &BoundInsert) -> DbResult<()> {
        let table = &insert.table;
        let rows = match &insert.source {
            BoundInsertSource::Values(rows) => rows.clone(),
            BoundInsertSource::Select(_) => {
                return Err(error::misuse(
                    "INSERT ... SELECT into a view is not supported",
                ))
            }
        };
        for row in &rows {
            let mut sources = Vec::with_capacity(row.len());
            for value in row {
                sources.push(self.compile_expr(value)?);
            }
            let mut values = Vec::with_capacity(insert.columns.len());
            for column in &insert.columns {
                let register = match column {
                    ColumnSource::Row(index) => sources.get(*index).copied().unwrap_or(0),
                    ColumnSource::Expr(expr) | ColumnSource::Generated(expr) => {
                        self.compile_expr(expr)?
                    }
                };
                values.push(register);
            }
            // A view has no rowid of its own, and SQLite reports NEW.rowid as
            // NULL inside an INSTEAD OF trigger.
            let rowid = self.register();
            self.emit(Instruction::new(Opcode::Null, 0, rowid as i32, 0));
            let new_row = RowImage { values, rowid };
            let ignored = core::mem::take(&mut self.ignore_jumps);
            self.emit_triggers(
                &insert.triggers,
                TriggerTime::InsteadOf,
                table,
                None,
                Some(&new_row),
            )?;
            for label in core::mem::replace(&mut self.ignore_jumps, ignored) {
                self.patch_here(label);
            }
        }
        Ok(())
    }

    /// Emits an `UPDATE` or `DELETE` on a view, row by row.
    ///
    /// The view's rows are produced first, into an ephemeral, and the trigger is
    /// fired once for each - so `OLD` is a row of the view as the statement's
    /// `WHERE` selected it. Collecting them first rather than firing as they are
    /// produced matters for the same reason it does for `INSERT ... SELECT`: the
    /// trigger body writes the tables the view reads.
    fn emit_view_write(
        &mut self,
        table: &TableInfo,
        rows: &BoundSelect,
        triggers: &[BoundTrigger],
        assignments: Option<&[BoundAssignment]>,
    ) -> DbResult<()> {
        let mut plan = inillucent_sql::plan::plan_select_with(rows.clone(), self.levers);
        self.resolve_plan(&mut plan)?;
        let width = plan.select.columns.len().max(1);
        let store = self.ephemeral();
        self.emit(Instruction::new(
            Opcode::EphOpen,
            store as i32,
            width as i32,
            0,
        ));
        self.open_all_cursors(&plan)?;
        self.compile_block(&plan, Sink::Store(store))?;

        let block = self.register_block(width);
        let empty = self.emit_jump(Instruction::new(Opcode::EphRewind, store as i32, -1, 0));
        let top = self.here();
        let mut old_values = Vec::with_capacity(width);
        for index in 0..width {
            let register = block.saturating_add(index as u32);
            self.emit(Instruction::new(
                Opcode::EphColumn,
                store as i32,
                index as i32,
                register as i32,
            ));
            old_values.push(register);
        }
        // The assignments were bound against the view's own columns, so the
        // substitutions are the result columns of the query that produced them.
        let previous = core::mem::take(&mut self.substitutions);
        for (index, column) in rows.columns.iter().enumerate() {
            if let Some(register) = old_values.get(index) {
                self.substitutions.push((column.expr.clone(), *register));
            }
        }
        let assigned = self.compile_view_assignments(old_values.as_slice(), assignments);
        self.substitutions = previous;
        let new_values = assigned?;

        let rowid = self.register();
        self.emit(Instruction::new(Opcode::Null, 0, rowid as i32, 0));
        let old_row = RowImage {
            values: old_values,
            rowid,
        };
        let ignored = core::mem::take(&mut self.ignore_jumps);
        match new_values {
            Some(values) => {
                let new_row = RowImage { values, rowid };
                self.emit_triggers(
                    triggers,
                    TriggerTime::InsteadOf,
                    table,
                    Some(&old_row),
                    Some(&new_row),
                )?;
            }
            None => {
                self.emit_triggers(
                    triggers,
                    TriggerTime::InsteadOf,
                    table,
                    Some(&old_row),
                    None,
                )?;
            }
        }
        for label in core::mem::replace(&mut self.ignore_jumps, ignored) {
            self.patch_here(label);
        }
        let more = self.emit_jump(Instruction::new(Opcode::EphNext, store as i32, -1, 0));
        self.patch(more, top);
        self.patch_here(empty);
        self.emit(Instruction::new(Opcode::EphClear, store as i32, 0, 0));
        Ok(())
    }

    /// Builds the `NEW` row of an `UPDATE` on a view: assigned, or carried over.
    fn compile_view_assignments(
        &mut self,
        old_values: &[u32],
        assignments: Option<&[BoundAssignment]>,
    ) -> DbResult<Option<Vec<u32>>> {
        let Some(assignments) = assignments else {
            return Ok(None);
        };
        let mut values = Vec::with_capacity(old_values.len());
        for (position, carried) in old_values.iter().enumerate() {
            let assigned = assignments
                .iter()
                .find(|assignment| usize::from(assignment.column) == position);
            let register = match assigned {
                Some(assignment) => {
                    let value = self.compile_expr(&assignment.value)?;
                    let copy = self.register();
                    self.emit(Instruction::new(Opcode::Copy, value as i32, copy as i32, 0));
                    copy
                }
                None => *carried,
            };
            values.push(register);
        }
        Ok(Some(values))
    }

    /// Reads every column of the row a cursor is on into fresh registers.
    ///
    /// This is what `OLD` is: a copy taken before the write, because after it
    /// the row it describes is gone.
    fn read_row_image(&mut self, writer: &Writer, table: &TableInfo, rowid: u32) -> RowImage {
        let mut values = Vec::with_capacity(table.columns.len());
        for position in 0..table.columns.len() as u16 {
            values.push(self.read_column(writer.table, table, position));
        }
        RowImage { values, rowid }
    }

    /// Emits the rowid an `AUTOINCREMENT` table's row gets, when it needs one.
    ///
    /// `None` when the statement supplied the key itself, because then there is
    /// nothing to allocate - the value is used as written, and the sequence is
    /// raised to it afterwards like any other.
    fn emit_autoincrement_rowid(
        &mut self,
        writer: &Writer,
        insert: &BoundInsert,
        values: &[u32],
        named: Option<u32>,
    ) -> DbResult<Option<u32>> {
        let table = &insert.table;
        if table.rowid_alias.is_none() && named.is_none() {
            return Ok(None);
        }
        let supplied = named.or_else(|| {
            table
                .rowid_alias
                .and_then(|alias| values.get(usize::from(alias)).copied())
        });
        let register = self.register();
        self.emit(
            Instruction::new(
                Opcode::SeqRowid,
                writer.table as i32,
                insert.sequence_root as i32,
                register as i32,
            )
            .with_p4(Operand::Text(table.name.clone()))
            .with_p5(table.database as u16),
        );
        let Some(supplied) = supplied else {
            return Ok(Some(register));
        };
        // A NULL in the key column means "give me one"; anything else is the
        // key, and the sequence only records it.
        let chosen = self.register();
        self.emit(Instruction::new(
            Opcode::Copy,
            supplied as i32,
            chosen as i32,
            0,
        ));
        let generated = self.emit_jump(Instruction::new(Opcode::IfNull, supplied as i32, -1, 0));
        let done = self.emit_jump(Instruction::new(Opcode::Goto, 0, -1, 0));
        self.patch_here(generated);
        self.emit(Instruction::new(
            Opcode::Copy,
            register as i32,
            chosen as i32,
            0,
        ));
        self.patch_here(done);
        Ok(Some(chosen))
    }

    /// Emits the uniqueness check a `WITHOUT ROWID` table's primary key owes.
    ///
    /// The key is the table's own b-tree, so the check is a seek on the cursor
    /// the row is about to be written through rather than on a separate index.
    /// On an `UPDATE` a hit is only a conflict when the key actually moved: a
    /// row whose key is unchanged finds itself.
    fn emit_primary_key_constraint(
        &mut self,
        writer: &Writer,
        table: &TableInfo,
        values: &[u32],
        old_key: Option<&[u32]>,
        statement: Option<ConflictAction>,
        skip: &mut Vec<Label>,
    ) -> DbResult<()> {
        let keys = table.primary_key();
        if keys.is_empty() {
            return Ok(());
        }
        let block = self.register_block(keys.len());
        for (offset, position) in keys.iter().enumerate() {
            let Some(source) = values.get(usize::from(*position)).copied() else {
                continue;
            };
            let target = block.saturating_add(offset as u32);
            self.emit(Instruction::new(
                Opcode::Copy,
                source as i32,
                target as i32,
                0,
            ));
        }
        let clear = self.emit_jump(
            Instruction::new(Opcode::NoConflict, writer.table as i32, -1, block as i32)
                .with_p5(keys.len() as u16),
        );
        // An entry was found. On an UPDATE whose key did not move, that entry is
        // this row.
        let mut same: Vec<Label> = Vec::new();
        if let Some(old_key) = old_key {
            for (offset, previous) in old_key.iter().enumerate() {
                let current = block.saturating_add(offset as u32);
                let equal = self.register();
                self.emit(
                    Instruction::new(
                        Opcode::Compare,
                        current as i32,
                        *previous as i32,
                        equal as i32,
                    )
                    .with_p4(Operand::Comparison(crate::program::Comparison {
                        op: inillucent_sql::ast::BinaryOp::Equal,
                        affinity: None,
                        collation: Collation::Binary,
                    })),
                );
                // Any column that differs means the key moved, so the hit is a
                // real duplicate and the halt below is reached.
                let differs =
                    self.emit_jump(Instruction::new(Opcode::IfNot, equal as i32, -1, 0).with_p5(1));
                same.push(differs);
            }
            // Every key column compared equal: this is the same row.
            let itself = self.emit_jump(Instruction::new(Opcode::Goto, 0, -1, 0));
            for label in same {
                self.patch_here(label);
            }
            same = vec![itself];
        }
        let action = statement
            .or(table
                .indexes
                .iter()
                .find(|index| index.origin == IndexOrigin::PrimaryKey)
                .and_then(|index| index.conflict))
            .unwrap_or(ConflictAction::Abort);
        let names: Vec<String> = keys
            .iter()
            .filter_map(|position| table.column(*position))
            .map(|column| {
                format!(
                    "{}.{}",
                    String::from_utf8_lossy(&table.name),
                    String::from_utf8_lossy(&column.name)
                )
            })
            .collect();
        let message = format!("UNIQUE constraint failed: {}", names.join(", "));
        match action {
            ConflictAction::Ignore => {
                skip.push(self.emit_jump(Instruction::new(Opcode::Goto, 0, -1, 0)));
            }
            ConflictAction::Replace => {
                // The row in the way is deleted and the write carries on, which
                // is what REPLACE means. The cursor is already on it.
                self.emit_delete_current(writer, table)?;
            }
            other => self.emit_constraint_halt(codes::PRIMARY_KEY, other, message),
        }
        for label in same {
            self.patch_here(label);
        }
        self.patch_here(clear);
        Ok(())
    }

    /// Emits everything one inserted row costs.
    fn emit_insert_row(
        &mut self,
        writer: &Writer,
        insert: &BoundInsert,
        sources: &[u32],
    ) -> DbResult<()> {
        let table = &insert.table;
        let mut values = Vec::with_capacity(table.columns.len());
        let mut generated: Vec<(usize, BoundExpr)> = Vec::new();
        for (position, column) in insert.columns.iter().enumerate() {
            let register = match column {
                ColumnSource::Row(index) => sources.get(*index).copied().unwrap_or(0),
                ColumnSource::Expr(expr) => self.compile_expr(expr)?,
                ColumnSource::Generated(expr) => {
                    // Its value is not known yet: it reads the rest of the row,
                    // and the rest of the row is still being assembled. A NULL
                    // holds the slot until the second pass fills it.
                    generated.push((position, expr.clone()));
                    let placeholder = self.register();
                    self.emit(Instruction::new(Opcode::Null, 0, placeholder as i32, 0));
                    placeholder
                }
            };
            values.push(register);
        }
        for (position, register) in values.iter().enumerate() {
            let Some(column) = table.column(position as u16) else {
                continue;
            };
            if table.rowid_alias == Some(position as u16) || column.generated {
                continue;
            }
            self.emit(
                Instruction::new(Opcode::ApplyAffinity, *register as i32, 1, 0)
                    .with_p4(Operand::Affinity(column.affinity)),
            );
        }
        // A statement may have named the rowid rather than a column - the
        // register is in the supplied row, not among the table's columns.
        let named = insert
            .named_rowid
            .and_then(|index| sources.get(index).copied());
        let rowid = self.emit_insert_rowid(writer, insert, &values, named)?;
        self.emit_generated_values(table, &generated, &mut values, rowid)?;
        if table.autoincrement && insert.sequence_root != 0 {
            // Before the row is written, and for an explicit rowid as well as a
            // generated one: `sqlite_sequence` holds the largest rowid the table
            // has ever used, not the largest this statement invented.
            self.emit(
                Instruction::new(
                    Opcode::SeqUpdate,
                    rowid as i32,
                    insert.sequence_root as i32,
                    0,
                )
                .with_p4(Operand::Text(table.name.clone()))
                .with_p5(table.database as u16),
            );
        }
        let mut skip = Vec::new();
        // A BEFORE trigger runs on the row as proposed - after the defaults, the
        // rowid and the generated columns have been worked out, because it can
        // read all three through NEW - and before the constraints, because
        // SQLite lets one RAISE(IGNORE) a row that would otherwise fail one.
        let new_row = RowImage {
            values: values.clone(),
            rowid,
        };
        let ignored = core::mem::take(&mut self.ignore_jumps);
        self.emit_triggers(
            &insert.triggers,
            TriggerTime::Before,
            table,
            None,
            Some(&new_row),
        )?;
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
        if table.without_rowid {
            self.emit_primary_key_constraint(
                writer,
                table,
                &values,
                None,
                insert.on_conflict,
                &mut skip,
            )?;
        }
        self.emit_unique_constraints(
            writer,
            table,
            &values,
            rowid,
            None,
            insert.on_conflict,
            insert.upsert.first(),
            &insert.checks,
            &insert.returning,
            &mut skip,
        )?;
        self.emit_write_row(writer, table, &values, rowid)?;
        self.emit_count_change(table, rowid, RowChangeKind::Insert, true);
        self.emit_triggers(
            &insert.triggers,
            TriggerTime::After,
            table,
            None,
            Some(&new_row),
        )?;
        let previous = core::mem::replace(
            &mut self.substitutions,
            row_substitutions(table, &values, rowid),
        );
        let returning = self.emit_returning(&insert.returning);
        self.substitutions = previous;
        returning?;
        // `RAISE(IGNORE)` abandons this row and nothing else, so its jumps land
        // exactly where a failed constraint's do.
        for label in core::mem::replace(&mut self.ignore_jumps, ignored) {
            self.patch_here(label);
        }
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
        named: Option<u32>,
    ) -> DbResult<u32> {
        if insert.table.autoincrement && insert.sequence_root != 0 {
            if let Some(register) = self.emit_autoincrement_rowid(writer, insert, values, named)? {
                return Ok(register);
            }
        }
        if insert.table.without_rowid {
            // There is no rowid to allocate. The register exists because the
            // callers pass one through to the row image and the index-entry
            // builder, both of which ignore it for a WITHOUT ROWID table.
            let register = self.register();
            self.emit(Instruction::new(Opcode::Null, 0, register as i32, 0));
            return Ok(register);
        }
        let table = &insert.table;
        // A named rowid wins over the alias: they are the same key written two
        // ways, and a statement that wrote both is naming one value twice.
        let supplied = named.or_else(|| {
            table
                .rowid_alias
                .and_then(|alias| values.get(alias as usize).copied())
        });
        let Some(supplied) = supplied else {
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
                    op: inillucent_sql::ast::BinaryOp::Equal,
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
                        // With no `INTEGER PRIMARY KEY` to name, the key is the
                        // rowid and that is what it is called.
                        table
                            .rowid_alias
                            .and_then(|alias| table.column(alias))
                            .map(|column| String::from_utf8_lossy(&column.name).into_owned())
                            .unwrap_or_else(|| "rowid".to_string())
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
    compile_delete_with(delete, dependencies, parameters, None)
}

/// As [`compile_delete`], with somebody to ask about virtual tables.
pub fn compile_delete_with(
    delete: &BoundDelete,
    dependencies: ProgramDependencies,
    parameters: u32,
    planner: Option<Box<dyn crate::compile::VirtualPlanner>>,
) -> DbResult<Program> {
    let mut compiler = Compiler::with_levers(plan::Levers::without(dependencies.levers));
    if let Some(planner) = planner {
        compiler = compiler.with_virtual_planner(planner);
    }
    let entry = compiler.emit_jump(Instruction::new(Opcode::Init, 0, -1, 0));
    compiler.patch_here(entry);
    compiler.emit(Instruction::new(Opcode::Transaction, 0, 0, 0));
    compiler.emit_delete_body(delete)?;
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

    /// Opens the sorter a `WITHOUT ROWID` write collects its keys in.
    ///
    /// One sort column per primary-key column, with the key's own collations:
    /// the second pass seeks the table by these values, so a sorter that
    /// ordered them differently would still be correct but a sorter that
    /// compared them differently would collapse two distinct keys into one.
    fn open_key_sorter(&mut self, table: &TableInfo) -> u32 {
        let sorter = self.sorters;
        self.sorters = self.sorters.saturating_add(1);
        let columns = table
            .primary_key()
            .into_iter()
            .map(|position| SortColumn {
                descending: false,
                nulls_first: true,
                collation: table
                    .column(position)
                    .map(|column| {
                        Collation::from_name(
                            core::str::from_utf8(&column.collation).unwrap_or("BINARY"),
                        )
                        .unwrap_or(Collation::Binary)
                    })
                    .unwrap_or(Collation::Binary),
            })
            .collect();
        self.emit(
            Instruction::new(Opcode::SorterOpen, sorter as i32, 0, 0)
                .with_p4(Operand::SortKey(SortKey { columns })),
        );
        sorter
    }

    /// Emits the scan that collects the keys a `WITHOUT ROWID` write will change.
    fn emit_collect_keys(
        &mut self,
        writer: &Writer,
        table: &TableInfo,
        filter: Option<&BoundExpr>,
        sorter: u32,
    ) -> DbResult<()> {
        let keys = table.primary_key();
        let end = self.emit_jump(Instruction::new(Opcode::Rewind, writer.table as i32, -1, 0));
        let top = self.here();
        let mut skip = None;
        if let Some(filter) = filter {
            let register = self.compile_expr(filter)?;
            skip = Some(
                self.emit_jump(Instruction::new(Opcode::IfNot, register as i32, -1, 0).with_p5(1)),
            );
        }
        let block = self.register_block(keys.len().max(1));
        for (offset, position) in keys.iter().enumerate() {
            let value = self.read_column(writer.table, table, *position);
            let target = block.saturating_add(offset as u32);
            self.emit(Instruction::new(
                Opcode::Copy,
                value as i32,
                target as i32,
                0,
            ));
        }
        self.emit(Instruction::new(
            Opcode::SorterInsert,
            sorter as i32,
            block as i32,
            keys.len() as i32,
        ));
        if let Some(skip) = skip {
            self.patch_here(skip);
        }
        self.emit(Instruction::new(Opcode::Next, writer.table as i32, top, 0));
        self.patch_here(end);
        Ok(())
    }

    /// Emits an `UPDATE` or `DELETE` on a `WITHOUT ROWID` table.
    ///
    /// The same two passes a rowid table gets - collect what will change, then
    /// change it - keyed by the primary key rather than by rowid, because that
    /// is the only locator such a table has. One pass would rewrite rows the
    /// scan had not reached yet.
    fn emit_keyed_write(
        &mut self,
        table: &TableInfo,
        source: usize,
        filter: Option<&BoundExpr>,
        update: Option<&BoundUpdate>,
        delete: Option<&BoundDelete>,
    ) -> DbResult<()> {
        let writer = self.open_for_write(table, source);
        let sorter = self.open_key_sorter(table);
        self.emit_collect_keys(&writer, table, filter, sorter)?;
        let keys = table.primary_key();
        let empty = self.emit_jump(Instruction::new(Opcode::SorterSort, sorter as i32, -1, 0));
        let top = self.here();
        let block = self.register_block(keys.len().max(1));
        for offset in 0..keys.len() {
            self.emit(Instruction::new(
                Opcode::SorterColumn,
                sorter as i32,
                offset as i32,
                block.saturating_add(offset as u32) as i32,
            ));
        }
        let missing = self.emit_jump(
            Instruction::new(Opcode::NoConflict, writer.table as i32, -1, block as i32)
                .with_p5(keys.len() as u16),
        );
        let old_key: Vec<u32> = (0..keys.len())
            .map(|offset| block.saturating_add(offset as u32))
            .collect();
        match (update, delete) {
            (Some(update), _) => self.emit_keyed_update_row(&writer, update, &old_key)?,
            (_, Some(delete)) => {
                self.emit_returning(&delete.returning)?;
                let rowid = self.register();
                self.emit(Instruction::new(Opcode::Null, 0, rowid as i32, 0));
                let old = self.read_row_image(&writer, table, rowid);
                let ignored = core::mem::take(&mut self.ignore_jumps);
                self.emit_triggers(
                    &delete.triggers,
                    TriggerTime::Before,
                    table,
                    Some(&old),
                    None,
                )?;
                self.emit_delete_current(&writer, table)?;
                self.emit_count_change(table, rowid, RowChangeKind::Delete, false);
                self.emit_triggers(
                    &delete.triggers,
                    TriggerTime::After,
                    table,
                    Some(&old),
                    None,
                )?;
                for label in core::mem::replace(&mut self.ignore_jumps, ignored) {
                    self.patch_here(label);
                }
            }
            (None, None) => return Err(error::misuse("a keyed write with nothing to do")),
        }
        self.patch_here(missing);
        self.emit(Instruction::new(Opcode::SorterNext, sorter as i32, top, 0));
        self.patch_here(empty);
        Ok(())
    }

    /// Emits one updated row of a `WITHOUT ROWID` table.
    fn emit_keyed_update_row(
        &mut self,
        writer: &Writer,
        update: &BoundUpdate,
        old_key: &[u32],
    ) -> DbResult<()> {
        let table = &update.table;
        let rowid = self.register();
        self.emit(Instruction::new(Opcode::Null, 0, rowid as i32, 0));
        let old_row = if update.triggers.is_empty() {
            RowImage {
                values: Vec::new(),
                rowid,
            }
        } else {
            self.read_row_image(writer, table, rowid)
        };
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
                        self.emit(
                            Instruction::new(Opcode::ApplyAffinity, copy as i32, 1, 0)
                                .with_p4(Operand::Affinity(column.affinity)),
                        );
                    }
                    copy
                }
                None => self.read_column(writer.table, table, position),
            };
            values.push(register);
        }
        let mut skip = Vec::new();
        let new_row = RowImage {
            values: values.clone(),
            rowid,
        };
        let ignored = core::mem::take(&mut self.ignore_jumps);
        self.emit_triggers(
            &update.triggers,
            TriggerTime::Before,
            table,
            Some(&old_row),
            Some(&new_row),
        )?;
        let previous = core::mem::replace(
            &mut self.substitutions,
            row_substitutions(table, &values, rowid),
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
        // The old entry goes first, before the key check: the check has to see
        // the tree without this row in it, or a key that did not move would
        // find itself and a REPLACE would delete the row it was rewriting.
        let old_values: Vec<u32> = (0..table.columns.len() as u16)
            .map(|column| self.read_column(writer.table, table, column))
            .collect();
        for (position, index) in writer.definitions.clone().iter().enumerate() {
            let Some(cursor) = writer.indexes.get(position).copied() else {
                continue;
            };
            let record = self.emit_index_record(index, &old_values, rowid, table)?;
            self.emit(Instruction::new(
                Opcode::IdxDelete,
                cursor as i32,
                record as i32,
                0,
            ));
        }
        let gone = self.emit_table_record(table, &old_values)?;
        self.emit(Instruction::new(
            Opcode::IdxDelete,
            writer.table as i32,
            gone as i32,
            0,
        ));
        self.emit_primary_key_constraint(
            writer,
            table,
            &values,
            Some(old_key),
            update.on_conflict,
            &mut skip,
        )?;
        self.emit_unique_constraints(
            writer,
            table,
            &values,
            rowid,
            None,
            update.on_conflict,
            None,
            &update.checks,
            &update.returning,
            &mut skip,
        )?;
        self.emit_write_row(writer, table, &values, rowid)?;
        self.emit_count_change(table, rowid, RowChangeKind::Update, false);
        self.emit_triggers(
            &update.triggers,
            TriggerTime::After,
            table,
            Some(&old_row),
            Some(&new_row),
        )?;
        let previous = core::mem::replace(
            &mut self.substitutions,
            row_substitutions(table, &values, rowid),
        );
        let returning = self.emit_returning(&update.returning);
        self.substitutions = previous;
        returning?;
        for label in core::mem::replace(&mut self.ignore_jumps, ignored) {
            self.patch_here(label);
        }
        for label in skip {
            self.patch_here(label);
        }
        Ok(())
    }

    /// Emits the scan that collects the rowids a write is going to change.
    fn emit_collect_rowids(
        &mut self,
        writer: &Writer,
        table: &TableInfo,
        source: usize,
        filter: Option<&BoundExpr>,
        sorter: u32,
    ) -> DbResult<()> {
        let chosen = plan::write_path_with(table, source, filter, self.levers);
        // A seek union is not collected as a union here - the collection pass
        // has its own, simpler cursor discipline (the writer's own index
        // cursor, no residual dedup machinery) and a scan is always correct
        // regardless of which path found the rows, so the union shapes fall
        // back to one rather than growing a second copy of the union
        // compiler for a pass that already re-tests the whole `WHERE` clause
        // anyway. The lever is only claimed by the shapes actually given
        // special treatment below, so a fallback here is never misreported as
        // an indexed write.
        let indexed = matches!(
            chosen,
            plan::AccessPath::RowidSeek { .. }
                | plan::AccessPath::RowidRange { .. }
                | plan::AccessPath::IndexSeek { .. }
        );
        if indexed {
            self.used |= plan::Levers::INDEXED_WRITE;
        }
        match chosen {
            plan::AccessPath::RowidSeek { key, .. } => {
                self.emit_collect_by_rowid(writer, filter, sorter, &key)
            }
            plan::AccessPath::RowidRange { low, high, .. } => {
                self.emit_collect_rowid_range(writer, filter, sorter, low, high)
            }
            plan::AccessPath::IndexSeek {
                index_name,
                equalities,
                low,
                high,
                columns,
                ..
            } => self.emit_collect_by_index(
                writer,
                table,
                filter,
                sorter,
                &index_name,
                &equalities,
                low,
                high,
                &named_columns(columns),
            ),
            _ => self.emit_collect_by_scan(writer, filter, sorter),
        }
    }

    /// Emits the body of the collection loop: test the predicate, keep the rowid.
    ///
    /// The whole `WHERE` clause is tested here whatever path found the row,
    /// rather than only the part the path did not consume. A path narrows which
    /// rows are *visited*; it never decides which are kept. That is a few
    /// redundant comparisons on a path that already pinned the row exactly, and
    /// it is the reason this optimisation cannot change an answer.
    fn emit_collect_body(
        &mut self,
        writer: &Writer,
        filter: Option<&BoundExpr>,
        sorter: u32,
    ) -> DbResult<Option<Label>> {
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
        Ok(skip)
    }

    /// Collects every row of the table, which is what a write with no usable
    /// predicate has to do.
    fn emit_collect_by_scan(
        &mut self,
        writer: &Writer,
        filter: Option<&BoundExpr>,
        sorter: u32,
    ) -> DbResult<()> {
        let end = self.emit_jump(Instruction::new(Opcode::Rewind, writer.table as i32, -1, 0));
        let top = self.here();
        let skip = self.emit_collect_body(writer, filter, sorter)?;
        if let Some(skip) = skip {
            self.patch_here(skip);
        }
        self.emit(Instruction::new(Opcode::Next, writer.table as i32, top, 0));
        self.patch_here(end);
        Ok(())
    }

    /// Collects the one row a rowid equality names.
    fn emit_collect_by_rowid(
        &mut self,
        writer: &Writer,
        filter: Option<&BoundExpr>,
        sorter: u32,
        key: &BoundExpr,
    ) -> DbResult<()> {
        let register = self.compile_expr(key)?;
        let missing = self.emit_jump(Instruction::new(
            Opcode::SeekRowid,
            writer.table as i32,
            -1,
            register as i32,
        ));
        let skip = self.emit_collect_body(writer, filter, sorter)?;
        if let Some(skip) = skip {
            self.patch_here(skip);
        }
        self.patch_here(missing);
        Ok(())
    }

    /// Collects the run of rows a rowid range covers.
    fn emit_collect_rowid_range(
        &mut self,
        writer: &Writer,
        filter: Option<&BoundExpr>,
        sorter: u32,
        low: Option<RangeBound>,
        high: Option<RangeBound>,
    ) -> DbResult<()> {
        let empty = match &low {
            Some(bound) => {
                let register = self.compile_expr(&bound.value)?;
                self.emit(
                    Instruction::new(Opcode::Cast, register as i32, register as i32, 0)
                        .with_p4(Operand::Affinity(Affinity::Integer)),
                );
                let opcode = if bound.kind == BoundKind::Greater {
                    Opcode::SeekGt
                } else {
                    Opcode::SeekGe
                };
                self.emit_jump(
                    Instruction::new(opcode, writer.table as i32, -1, register as i32).with_p5(1),
                )
            }
            None => self.emit_jump(Instruction::new(Opcode::Rewind, writer.table as i32, -1, 0)),
        };
        let top = self.here();
        let mut done: Vec<Label> = Vec::new();
        if let Some(bound) = &high {
            let limit = self.compile_expr(&bound.value)?;
            let rowid = self.register();
            self.emit(Instruction::new(
                Opcode::Rowid,
                writer.table as i32,
                rowid as i32,
                0,
            ));
            let result = self.register();
            let op = if bound.kind == BoundKind::Less {
                BinaryOp::Less
            } else {
                BinaryOp::LessEqual
            };
            self.emit(
                Instruction::new(Opcode::Compare, rowid as i32, limit as i32, result as i32)
                    .with_p4(Operand::Comparison(Comparison {
                        op,
                        affinity: Some(Affinity::Integer),
                        collation: Collation::Binary,
                    })),
            );
            done.push(
                self.emit_jump(Instruction::new(Opcode::IfNot, result as i32, -1, 0).with_p5(1)),
            );
        }
        let skip = self.emit_collect_body(writer, filter, sorter)?;
        if let Some(skip) = skip {
            self.patch_here(skip);
        }
        self.emit(Instruction::new(Opcode::Next, writer.table as i32, top, 0));
        for label in done {
            self.patch_here(label);
        }
        self.patch_here(empty);
        Ok(())
    }

    /// Collects the rows an index seek or range finds.
    ///
    /// The index cursor is the *write* cursor the statement already opened for
    /// that index, so no second cursor is needed - and it is positioned before
    /// anything is written, which is the whole point of the collection pass.
    #[allow(clippy::too_many_arguments)]
    fn emit_collect_by_index(
        &mut self,
        writer: &Writer,
        table: &TableInfo,
        filter: Option<&BoundExpr>,
        sorter: u32,
        index_name: &[u8],
        equalities: &[BoundExpr],
        low: Option<RangeBound>,
        high: Option<RangeBound>,
        columns: &[u16],
    ) -> DbResult<()> {
        let position = writer
            .definitions
            .iter()
            .position(|index| index.name == index_name);
        let (Some(position), Some(index_cursor)) = (
            position,
            position.and_then(|slot| writer.indexes.get(slot)).copied(),
        ) else {
            return self.emit_collect_by_scan(writer, filter, sorter);
        };
        let _ = position;
        let key_len = equalities.len().saturating_add(usize::from(low.is_some()));
        let key = self.register_block(key_len.max(1));
        for (slot, expr) in equalities.iter().enumerate() {
            let register = self.compile_expr(expr)?;
            self.emit(Instruction::new(
                Opcode::Copy,
                register as i32,
                key.saturating_add(slot as u32) as i32,
                0,
            ));
        }
        let mut seek_len = equalities.len();
        let mut opcode = Opcode::SeekGe;
        if let Some(bound) = &low {
            let register = self.compile_expr(&bound.value)?;
            self.emit(Instruction::new(
                Opcode::Copy,
                register as i32,
                key.saturating_add(equalities.len() as u32) as i32,
                0,
            ));
            seek_len = seek_len.saturating_add(1);
            opcode = if bound.kind == BoundKind::Greater {
                Opcode::SeekGt
            } else {
                Opcode::SeekGe
            };
        }
        self.apply_write_index_affinity(table, columns, key, seek_len);
        // A NULL equality key matches nothing: `x = NULL` is unknown rather
        // than true, and an index that stores NULLs together would otherwise
        // hand back the rows whose column is NULL.
        let mut null_key: Vec<Label> = Vec::new();
        for slot in 0..equalities.len() {
            null_key.push(self.emit_jump(Instruction::new(
                Opcode::IfNull,
                key.saturating_add(slot as u32) as i32,
                -1,
                0,
            )));
        }
        let empty = self.emit_jump(
            Instruction::new(opcode, index_cursor as i32, -1, key as i32).with_p5(seek_len as u16),
        );
        let top = self.here();
        let mut done: Vec<Label> = Vec::new();
        if !equalities.is_empty() {
            done.push(
                self.emit_jump(
                    Instruction::new(Opcode::IdxGt, index_cursor as i32, -1, key as i32)
                        .with_p5(equalities.len() as u16),
                ),
            );
        }
        if let Some(bound) = &high {
            let high_key = self.register_block(equalities.len().saturating_add(1));
            for slot in 0..equalities.len() {
                self.emit(Instruction::new(
                    Opcode::Copy,
                    key.saturating_add(slot as u32) as i32,
                    high_key.saturating_add(slot as u32) as i32,
                    0,
                ));
            }
            let register = self.compile_expr(&bound.value)?;
            self.emit(Instruction::new(
                Opcode::Copy,
                register as i32,
                high_key.saturating_add(equalities.len() as u32) as i32,
                0,
            ));
            let length = equalities.len().saturating_add(1);
            self.apply_write_index_affinity(table, columns, high_key, length);
            let opcode = if bound.kind == BoundKind::Less {
                Opcode::IdxGe
            } else {
                Opcode::IdxGt
            };
            done.push(
                self.emit_jump(
                    Instruction::new(opcode, index_cursor as i32, -1, high_key as i32)
                        .with_p5(length as u16),
                ),
            );
        }
        let rowid = self.register();
        self.emit(Instruction::new(
            Opcode::IdxRowid,
            index_cursor as i32,
            rowid as i32,
            0,
        ));
        let mut skip: Vec<Label> = vec![self.emit_jump(Instruction::new(
            Opcode::SeekRowid,
            writer.table as i32,
            -1,
            rowid as i32,
        ))];
        if let Some(label) = self.emit_collect_body(writer, filter, sorter)? {
            skip.push(label);
        }
        for label in skip {
            self.patch_here(label);
        }
        self.emit(Instruction::new(Opcode::Next, index_cursor as i32, top, 0));
        for label in done {
            self.patch_here(label);
        }
        self.patch_here(empty);
        for label in null_key {
            self.patch_here(label);
        }
        Ok(())
    }

    /// Applies the indexed columns' affinities to a seek key.
    ///
    /// A seek key has to be converted the way the index's own values were, or
    /// the comparison holds a text `'5'` against an integer 5 and finds
    /// nothing. This is the single most common way an index seek silently
    /// returns no rows, and a write that found no rows would be a write that
    /// silently did nothing.
    fn apply_write_index_affinity(
        &mut self,
        table: &TableInfo,
        columns: &[u16],
        key: u32,
        length: usize,
    ) {
        for position in 0..length {
            let Some(column) = columns.get(position).copied() else {
                continue;
            };
            let Some(info) = table.column(column) else {
                continue;
            };
            self.emit(
                Instruction::new(
                    Opcode::ApplyAffinity,
                    key.saturating_add(position as u32) as i32,
                    1,
                    0,
                )
                .with_p4(Operand::Affinity(info.affinity)),
            );
        }
    }
}

/// Compiles an `UPDATE`.
pub fn compile_update(
    update: &BoundUpdate,
    dependencies: ProgramDependencies,
    parameters: u32,
) -> DbResult<Program> {
    compile_update_with(update, dependencies, parameters, None)
}

/// As [`compile_update`], with somebody to ask about virtual tables.
pub fn compile_update_with(
    update: &BoundUpdate,
    dependencies: ProgramDependencies,
    parameters: u32,
    planner: Option<Box<dyn crate::compile::VirtualPlanner>>,
) -> DbResult<Program> {
    let mut compiler = Compiler::with_levers(plan::Levers::without(dependencies.levers));
    if let Some(planner) = planner {
        compiler = compiler.with_virtual_planner(planner);
    }
    let entry = compiler.emit_jump(Instruction::new(Opcode::Init, 0, -1, 0));
    compiler.patch_here(entry);
    compiler.emit(Instruction::new(Opcode::Transaction, 0, 0, 0));
    compiler.emit_update_body(update)?;
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
        // OLD is read first, off the cursor the rowid pass left positioned. It
        // has to be a copy: by the time an AFTER trigger reads it the row it
        // describes has been deleted and rewritten.
        let old_row = if update.triggers.is_empty() {
            RowImage {
                values: Vec::new(),
                rowid: old_rowid,
            }
        } else {
            self.read_row_image(writer, table, old_rowid)
        };
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
        let new_row = RowImage {
            values: values.clone(),
            rowid: new_rowid,
        };
        let ignored = core::mem::take(&mut self.ignore_jumps);
        self.emit_triggers(
            &update.triggers,
            TriggerTime::Before,
            table,
            Some(&old_row),
            Some(&new_row),
        )?;
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
        // Only the row's own delete-and-rewrite may skip an index; every other
        // caller of these two emitters - DELETE, INSERT, upsert, a trigger body -
        // leaves the flags empty and rebuilds everything.
        let unaffected = self.unaffected_indexes(table, update);
        let untouched = core::mem::replace(&mut self.untouched_indexes, unaffected);
        let deleted = self.emit_delete_current(writer, table);
        self.patch_here(gone);
        let written = deleted.and_then(|()| self.emit_write_row(writer, table, &values, new_rowid));
        self.untouched_indexes = untouched;
        written?;
        self.emit_count_change(table, new_rowid, RowChangeKind::Update, false);
        self.emit_triggers(
            &update.triggers,
            TriggerTime::After,
            table,
            Some(&old_row),
            Some(&new_row),
        )?;
        let previous = core::mem::replace(
            &mut self.substitutions,
            row_substitutions(table, &values, new_rowid),
        );
        let returning = self.emit_returning(&update.returning);
        self.substitutions = previous;
        returning?;
        for label in core::mem::replace(&mut self.ignore_jumps, ignored) {
            self.patch_here(label);
        }
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
