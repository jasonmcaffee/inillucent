//! Writes to a virtual table.
//!
//! Invariant: the module does the writing and the engine does the finding. An
//! insert evaluates the row and hands it over; a delete and an update scan the
//! table - through the same virtual scan a `SELECT` uses, offer and all - and
//! hand over one call per row found. Nothing here touches a b-tree, because a
//! virtual table has none: the whole of `INSERT`, `UPDATE` and `DELETE` on one
//! is three shapes of the same argument vector.
//!
//! The vector is SQLite's `xUpdate` vector, and its shape is what says which
//! operation it is. One value is a delete. A NULL first value is an insert. Two
//! values and a row is an update. That is why one opcode carries all three: a
//! module that implements one method implements all of them, and a module that
//! implements none is read-only and says so by refusing.

use inillucent_base::{error, DbResult};
use inillucent_sql::bind::BoundExpr;
use inillucent_sql::catalog_view::TableInfo;
use inillucent_sql::dml::{BoundDelete, BoundInsert, BoundInsertSource, BoundUpdate, ColumnSource};
use inillucent_sql::vtab::IndexQuery;

use crate::compile::{Compiler, Label};
use crate::program::{Instruction, Opcode, Operand, VirtualPlan, VirtualRef};

/// One virtual scan, opened and positioned, waiting for its body.
pub(crate) struct VirtualLoop {
    /// The cursor the scan reads through.
    pub cursor: u32,
    /// The address the loop returns to.
    pub top: i32,
    /// The jump taken when the module produced no rows at all.
    pub empty: Label,
    /// The jumps taken when a row failed a predicate the module did not apply.
    pub skips: Vec<Label>,
}

impl Compiler {
    /// Returns the reference that names one virtual table.
    fn virtual_reference(&self, table: &TableInfo) -> DbResult<VirtualRef> {
        let Some(module) = table.module.clone() else {
            return Err(error::misuse("that table is not a virtual table"));
        };
        Ok(VirtualRef {
            database: table.database,
            table: table.name.clone(),
            module,
        })
    }

    /// Opens a scan over a virtual table and positions it on the first row.
    ///
    /// The caller emits the body and then [`Compiler::close_virtual_loop`].
    pub(crate) fn open_virtual_loop(
        &mut self,
        table: &TableInfo,
        source: usize,
        filter: Option<&BoundExpr>,
    ) -> DbResult<VirtualLoop> {
        let reference = self.virtual_reference(table)?;
        let cursor = self.take_cursor();
        self.register_virtual_source(source, cursor);

        self.emit(
            Instruction::new(Opcode::VOpen, cursor as i32, 0, 0)
                .with_p4(Operand::Virtual(Box::new(reference.clone()))),
        );
        let terms = filter
            .map(inillucent_sql::plan::conjunction)
            .unwrap_or_default();
        let offer = inillucent_sql::plan::virtual_offer(source, table, &terms);
        let mut query = IndexQuery::new(offer.iter().map(|item| item.spec).collect(), Vec::new());
        self.ask_module(&reference, &mut query)?;
        let arguments = query.argument_order();
        let block = self.register_block(arguments.len().max(1));
        for (position, index) in arguments.iter().enumerate() {
            let Some(constraint) = offer.get(*index) else {
                continue;
            };
            let register = self.compile_expr(&constraint.value)?;
            self.emit(Instruction::new(
                Opcode::Copy,
                register as i32,
                block.saturating_add(position as u32) as i32,
                0,
            ));
        }
        let empty = self.emit_jump(
            Instruction::new(Opcode::VFilter, cursor as i32, -1, block as i32)
                .with_p5(arguments.len().min(usize::from(u16::MAX)) as u16)
                .with_p4(Operand::VirtualPlan(Box::new(VirtualPlan {
                    index_number: query.index_number,
                    index_string: query.index_string.clone(),
                }))),
        );
        let top = self.here();
        // Everything the module did not promise to apply is tested here, in the
        // order it was offered. A predicate the module took *and* promised is
        // gone; anything else is tested twice, which is the safe direction.
        let mut skips = Vec::new();
        for (index, constraint) in offer.iter().enumerate() {
            let promised = query
                .usage
                .get(index)
                .is_some_and(|usage| usage.argument > 0 && usage.omit);
            if promised {
                continue;
            }
            let register = self.compile_expr(&constraint.predicate)?;
            skips.push(
                self.emit_jump(Instruction::new(Opcode::IfNot, register as i32, -1, 0).with_p5(1)),
            );
        }
        Ok(VirtualLoop {
            cursor,
            top,
            empty,
            skips,
        })
    }

    /// Closes a scan opened by [`Compiler::open_virtual_loop`].
    pub(crate) fn close_virtual_loop(&mut self, loop_: VirtualLoop) {
        for skip in loop_.skips {
            self.patch_here(skip);
        }
        let more = self.emit_jump(Instruction::new(Opcode::VNext, loop_.cursor as i32, -1, 0));
        self.patch(more, loop_.top);
        self.patch_here(loop_.empty);
    }

    /// Compiles an `INSERT` into a virtual table.
    ///
    /// Every declared column is written, hidden ones included: a module is
    /// handed a whole row and decides what to do with the parts of it that are
    /// arguments rather than data. A column the statement did not name is NULL,
    /// which is what a module reads as "not given".
    pub(crate) fn emit_virtual_insert(&mut self, insert: &BoundInsert) -> DbResult<()> {
        let reference = self.virtual_reference(&insert.table)?;
        let BoundInsertSource::Values(rows) = &insert.source else {
            return Err(error::misuse(
                "INSERT INTO a virtual table SELECT is not supported",
            ));
        };
        let rows = rows.clone();
        let width = insert.table.columns.len();
        for row in &rows {
            let block = self.register_block(width.saturating_add(2));
            // The old rowid is NULL, which is what makes this an insert.
            self.emit(Instruction::new(Opcode::Null, 0, block as i32, 0));
            self.emit(Instruction::new(
                Opcode::Null,
                0,
                block.saturating_add(1) as i32,
                0,
            ));
            // The row's own values are evaluated first, because a column
            // source names a position in it.
            let mut supplied = Vec::with_capacity(row.len());
            for value in row {
                supplied.push(self.compile_expr(value)?);
            }
            // The new rowid comes from the statement when it named one -
            // `INSERT INTO fts(rowid, a) VALUES (9, 'x')` - and stays NULL
            // otherwise, which is what tells the module to allocate one.
            if let Some(register) = insert
                .named_rowid
                .and_then(|index| supplied.get(index).copied())
            {
                self.emit(Instruction::new(
                    Opcode::Copy,
                    register as i32,
                    block.saturating_add(1) as i32,
                    0,
                ));
            }
            for (position, column) in insert.columns.iter().enumerate() {
                let slot = block.saturating_add(2).saturating_add(position as u32);
                let register = match column {
                    ColumnSource::Row(index) => supplied.get(*index).copied(),
                    ColumnSource::Expr(expr) | ColumnSource::Generated(expr) => {
                        let expr = expr.clone();
                        Some(self.compile_expr(&expr)?)
                    }
                };
                match register {
                    Some(register) => {
                        self.emit(Instruction::new(
                            Opcode::Copy,
                            register as i32,
                            slot as i32,
                            0,
                        ));
                    }
                    None => {
                        self.emit(Instruction::new(Opcode::Null, 0, slot as i32, 0));
                    }
                }
                // A rowid alias in the column list is the rowid the module is
                // asked to use, so it is copied into the second slot as well.
                if insert.table.rowid_alias == Some(position as u16) {
                    self.emit(Instruction::new(
                        Opcode::Copy,
                        slot as i32,
                        block.saturating_add(1) as i32,
                        0,
                    ));
                }
            }
            let rowid = self.register();
            self.emit(
                Instruction::new(
                    Opcode::VUpdate,
                    block as i32,
                    width.saturating_add(2) as i32,
                    rowid as i32,
                )
                .with_p4(Operand::Virtual(Box::new(reference.clone()))),
            );
        }
        Ok(())
    }

    /// Compiles a `DELETE` from a virtual table.
    pub(crate) fn emit_virtual_delete(&mut self, delete: &BoundDelete) -> DbResult<()> {
        let reference = self.virtual_reference(&delete.table)?;
        let scan = self.open_virtual_loop(&delete.table, delete.source, delete.filter.as_ref())?;
        let block = self.register_block(1);
        self.emit(Instruction::new(
            Opcode::VRowid,
            scan.cursor as i32,
            block as i32,
            0,
        ));
        self.emit(
            Instruction::new(Opcode::VUpdate, block as i32, 1, -1)
                .with_p4(Operand::Virtual(Box::new(reference))),
        );
        self.close_virtual_loop(scan);
        Ok(())
    }

    /// Compiles an `UPDATE` of a virtual table.
    ///
    /// Every column is read back off the cursor and then overwritten where the
    /// statement assigned it, so the module is handed a whole row rather than a
    /// patch. That is the contract: a module that stores its rows in a
    /// structure of its own has no way to apply a patch it was not given in
    /// full.
    pub(crate) fn emit_virtual_update(&mut self, update: &BoundUpdate) -> DbResult<()> {
        let reference = self.virtual_reference(&update.table)?;
        let scan = self.open_virtual_loop(&update.table, update.source, update.filter.as_ref())?;
        let width = update.table.columns.len();
        let block = self.register_block(width.saturating_add(2));
        self.emit(Instruction::new(
            Opcode::VRowid,
            scan.cursor as i32,
            block as i32,
            0,
        ));
        self.emit(Instruction::new(
            Opcode::Copy,
            block as i32,
            block.saturating_add(1) as i32,
            0,
        ));
        for position in 0..width {
            self.emit(Instruction::new(
                Opcode::VColumn,
                scan.cursor as i32,
                position as i32,
                block.saturating_add(2).saturating_add(position as u32) as i32,
            ));
        }
        for assignment in &update.assignments {
            let value = assignment.value.clone();
            let register = self.compile_expr(&value)?;
            let slot = block
                .saturating_add(2)
                .saturating_add(u32::from(assignment.column));
            self.emit(Instruction::new(
                Opcode::Copy,
                register as i32,
                slot as i32,
                0,
            ));
            // Assigning the rowid alias moves the row, which the module is told
            // by the second slot rather than by the column.
            if update.table.rowid_alias == Some(assignment.column) {
                self.emit(Instruction::new(
                    Opcode::Copy,
                    slot as i32,
                    block.saturating_add(1) as i32,
                    0,
                ));
            }
        }
        self.emit(
            Instruction::new(
                Opcode::VUpdate,
                block as i32,
                width.saturating_add(2) as i32,
                -1,
            )
            .with_p4(Operand::Virtual(Box::new(reference))),
        );
        self.close_virtual_loop(scan);
        Ok(())
    }
}
