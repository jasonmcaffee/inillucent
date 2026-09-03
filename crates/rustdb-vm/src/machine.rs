//! The virtual machine: registers, cursors, and the step loop.
//!
//! Invariant: the machine runs only verified programs, and it never panics on
//! one. Every register and cursor index has been checked before it gets here,
//! every jump target is inside the program, and every remaining failure - a
//! corrupt page, an interrupt, an arithmetic result SQLite calls an error - is
//! returned as a `DbError` that leaves the statement resettable.
//!
//! A step runs until the program produces a row, finishes, or fails. State
//! between steps is owned: register values are owned, cursor positions are
//! saved in the cursors themselves, and nothing borrows a page across a return,
//! so a statement can be stepped, left alone, and stepped again.

use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};
use std::sync::Arc;

use rustdb_base::ids::PageId;
use rustdb_base::limits::Limits;
use rustdb_base::{error, DbResult, PrimaryCode};
use rustdb_sql::ast::BinaryOp;
use rustdb_storage::cursor::{BTreeCursor, SavedPosition, SeekBias};
use rustdb_storage::mutate;
use rustdb_storage::pager::Pager;
use rustdb_value::record::{self, KeyColumn, KeyInfo, RecordRef};
use rustdb_value::{affinity, cast, Affinity, Collation, TextEncoding, Value};

use crate::aggregate::Accumulator;
use crate::builtin;
use crate::ephemeral::Ephemeral;
use crate::eval;
use crate::program::{Instruction, Opcode, Operand, Program, RowChange};
use crate::sorter::{DistinctSet, Sorter};

/// What one step of the machine produced.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StepOutcome {
    /// A result row is available.
    Row,
    /// The program finished.
    Done,
}

/// Where a statement is in its life.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MachineState {
    /// Compiled, never stepped.
    Prepared,
    /// Between steps, with more to do.
    Running,
    /// Sitting on a result row.
    Row,
    /// Finished.
    Done,
    /// Failed; the statement must be reset.
    Failed,
}

/// One open cursor.
struct CursorSlot {
    cursor: BTreeCursor,
    payload: Option<Vec<u8>>,
    is_index: bool,
    key: KeyInfo,
    /// Whether the cursor is standing on the null row an outer join emits.
    ///
    /// While it is set, every column read off the cursor answers NULL and the
    /// rowid answers NULL, without the body having to know it is running for
    /// an unmatched row.
    null_row: bool,
}

impl CursorSlot {
    /// Forgets the cached row, which every move must do.
    ///
    /// A move also leaves the null row: the cursor is on a real entry again,
    /// and a flag left set would turn the whole of the rest of the scan into
    /// NULLs.
    fn moved(&mut self) {
        self.payload = None;
        self.null_row = false;
    }
}

/// The machine.
pub struct Machine {
    program: Arc<Program>,
    registers: Vec<Value<'static>>,
    cursors: Vec<Option<CursorSlot>>,
    sorters: Vec<Option<Sorter>>,
    distincts: Vec<DistinctSet>,
    ephemerals: Vec<Option<Ephemeral>>,
    accumulators: Vec<Option<Accumulator>>,
    bindings: Vec<Value<'static>>,
    counter: usize,
    state: MachineState,
    result: Vec<Value<'static>>,
    interrupt: Arc<AtomicBool>,
    limits: Limits,
    encoding: TextEncoding,
    file_format: u32,
    steps: u64,
    /// The Julian day the statement's `'now'` resolves to.
    ///
    /// It is read once, when the machine is built, so a statement that names
    /// `'now'` twice - or once per row of a scan - sees one time. SQLite makes
    /// the same promise, and a clock read per call would make
    /// `SELECT date('now') = date('now')` occasionally false.
    now: f64,
    /// The state the random built-ins draw from.
    entropy: u64,
    changes: i64,
    /// How many rows this statement's triggers have changed.
    ///
    /// `changes()` reports the statement's own rows and not its triggers', so
    /// the two are counted separately and folded together only where
    /// `total_changes()` is worked out.
    trigger_changes: i64,
    /// How many rows every statement on this connection has changed.
    total_changes: i64,
    last_insert_rowid: i64,
    conflict: Option<i32>,
    record_changes: bool,
    row_changes: Vec<RowChange>,
}

impl Machine {
    /// Returns a machine ready to run a program.
    pub fn new(program: Arc<Program>, interrupt: Arc<AtomicBool>, limits: Limits) -> Machine {
        let registers = vec![Value::Null; program.register_count as usize];
        let mut cursors = Vec::new();
        cursors.resize_with(program.cursor_count as usize, || None);
        let mut sorters = Vec::new();
        sorters.resize_with(program.sorter_count as usize, || None);
        let distincts = vec![DistinctSet::new(); program.distinct_count as usize];
        let mut ephemerals = Vec::new();
        ephemerals.resize_with(program.ephemeral_count as usize, || None);
        let mut accumulators = Vec::new();
        accumulators.resize_with(program.aggregate_count as usize, || None);
        let bindings = vec![Value::Null; program.parameter_count as usize];
        Machine {
            program,
            registers,
            cursors,
            sorters,
            distincts,
            ephemerals,
            accumulators,
            bindings,
            counter: 0,
            state: MachineState::Prepared,
            result: Vec::new(),
            interrupt,
            limits,
            encoding: TextEncoding::Utf8,
            file_format: 4,
            steps: 0,
            now: crate::datetime::julian_now(),
            entropy: crate::datetime::julian_now().to_bits(),
            changes: 0,
            trigger_changes: 0,
            total_changes: 0,
            last_insert_rowid: 0,
            conflict: None,
            record_changes: false,
            row_changes: Vec::new(),
        }
    }

    /// Asks the machine to log every row it changes, for the update hook.
    pub fn record_row_changes(&mut self, record: bool) {
        self.record_changes = record;
        if !record {
            self.row_changes.clear();
        }
    }

    /// Takes the row changes logged since the last call.
    ///
    /// They are drained rather than read, because the session fires the hook
    /// for each one and firing the same change twice would be worse than not
    /// firing it at all.
    pub fn take_row_changes(&mut self) -> Vec<RowChange> {
        core::mem::take(&mut self.row_changes)
    }

    /// Returns the conflict algorithm the failing constraint carried.
    ///
    /// It is `None` until a `HaltError` runs, so a failure that was not a
    /// constraint - an I/O error, an interrupt - reports nothing and the
    /// session falls back to aborting the statement.
    pub fn conflict_action(&self) -> Option<i32> {
        self.conflict
    }

    /// Tells the machine what the connection's counters hold.
    ///
    /// `changes()` and `total_changes()` report the *connection's* history
    /// rather than this statement's, so the numbers come from outside and are
    /// set before the statement runs.
    pub fn set_counters(&mut self, changes: i64, total_changes: i64, last_insert_rowid: i64) {
        self.changes = changes;
        self.total_changes = total_changes;
        self.last_insert_rowid = last_insert_rowid;
    }

    /// Returns how many rows the program has changed so far.
    pub fn trigger_changes(&self) -> i64 {
        self.trigger_changes
    }

    /// Returns how many rows the statement itself changed.
    pub fn changes(&self) -> i64 {
        self.changes
    }

    /// Returns the rowid the program's most recent insert allocated.
    pub fn last_insert_rowid(&self) -> i64 {
        self.last_insert_rowid
    }

    /// Returns the program being run.
    pub fn program(&self) -> &Program {
        &self.program
    }

    /// Returns the machine's state.
    pub fn state(&self) -> MachineState {
        self.state
    }

    /// Returns how many instructions have been executed.
    pub fn steps(&self) -> u64 {
        self.steps
    }

    /// Binds a value to a one-based parameter index.
    pub fn bind(&mut self, index: u32, value: Value<'static>) -> DbResult<()> {
        let Some(slot) = index
            .checked_sub(1)
            .and_then(|index| self.bindings.get_mut(index as usize))
        else {
            return Err(error::misuse(format!("parameter {index} does not exist")));
        };
        *slot = value;
        Ok(())
    }

    /// Clears every binding back to NULL.
    pub fn clear_bindings(&mut self) {
        for binding in &mut self.bindings {
            *binding = Value::Null;
        }
    }

    /// Returns the current result row.
    pub fn row(&self) -> &[Value<'static>] {
        &self.result
    }

    /// Resets the machine so it can run again, keeping the bindings.
    pub fn reset(&mut self) {
        self.changes = 0;
        self.conflict = None;
        self.row_changes.clear();
        self.registers = vec![Value::Null; self.program.register_count as usize];
        self.cursors.clear();
        self.cursors
            .resize_with(self.program.cursor_count as usize, || None);
        self.sorters.clear();
        self.sorters
            .resize_with(self.program.sorter_count as usize, || None);
        self.distincts = vec![DistinctSet::new(); self.program.distinct_count as usize];
        self.accumulators.clear();
        self.accumulators
            .resize_with(self.program.aggregate_count as usize, || None);
        self.counter = 0;
        self.state = MachineState::Prepared;
        self.result.clear();
        self.steps = 0;
    }

    /// Runs until the program produces a row or finishes.
    pub fn step(&mut self, pager: &mut Pager) -> DbResult<StepOutcome> {
        if self.state == MachineState::Failed {
            return Err(error::misuse("this statement has failed and must be reset"));
        }
        if self.state == MachineState::Done {
            return Ok(StepOutcome::Done);
        }
        self.encoding = pager.text_encoding();
        self.file_format = pager.header().schema_format.max(1);
        self.state = MachineState::Running;
        loop {
            // The interrupt is checked at instruction boundaries, which are the
            // machine's declared safe points: no page is pinned and no cursor
            // is half-moved between two instructions.
            if self.interrupt.load(AtomicOrdering::Relaxed) {
                self.state = MachineState::Failed;
                return Err(rustdb_base::DbError::primary(PrimaryCode::Interrupt));
            }
            let Some(instruction) = self.program.instruction(self.counter).cloned() else {
                self.state = MachineState::Done;
                return Ok(StepOutcome::Done);
            };
            self.steps = self.steps.saturating_add(1);
            match self.execute(&instruction, pager) {
                Ok(Flow::Next) => self.counter = self.counter.saturating_add(1),
                Ok(Flow::Jump(target)) => self.counter = target,
                Ok(Flow::Row) => {
                    self.counter = self.counter.saturating_add(1);
                    self.state = MachineState::Row;
                    return Ok(StepOutcome::Row);
                }
                Ok(Flow::Halt) => {
                    self.state = MachineState::Done;
                    return Ok(StepOutcome::Done);
                }
                Err(failure) => {
                    self.state = MachineState::Failed;
                    return Err(failure);
                }
            }
        }
    }

    /// Returns the register at an index, or NULL.
    fn register(&self, index: i32) -> Value<'static> {
        self.registers
            .get(index.max(0) as usize)
            .cloned()
            .unwrap_or(Value::Null)
    }

    /// Stores a value into a register.
    fn store(&mut self, index: i32, value: Value<'static>) {
        if let Some(slot) = self.registers.get_mut(index.max(0) as usize) {
            *slot = value;
        }
    }

    /// Returns a contiguous block of registers.
    fn block(&self, first: i32, count: i32) -> Vec<Value<'static>> {
        (0..count.max(0))
            .map(|offset| self.register(first.saturating_add(offset)))
            .collect()
    }

    /// Runs one instruction.
    fn execute(&mut self, instruction: &Instruction, pager: &mut Pager) -> DbResult<Flow> {
        match instruction.opcode {
            Opcode::Init | Opcode::Goto => Ok(Flow::Jump(instruction.p2.max(0) as usize)),
            Opcode::Halt => Ok(Flow::Halt),
            Opcode::Transaction => Ok(Flow::Next),
            Opcode::Gosub => {
                self.store(
                    instruction.p1,
                    Value::Integer(self.counter.saturating_add(1) as i64),
                );
                Ok(Flow::Jump(instruction.p2.max(0) as usize))
            }
            Opcode::Return => {
                let target = cast::integer_value(&self.register(instruction.p1));
                Ok(Flow::Jump(target.max(0) as usize))
            }
            Opcode::OpenRead => self.open_cursor(instruction, false),
            Opcode::OpenIndex => self.open_cursor(instruction, true),
            Opcode::Close => {
                if let Some(slot) = self.cursors.get_mut(instruction.p1.max(0) as usize) {
                    *slot = None;
                }
                Ok(Flow::Next)
            }
            Opcode::Rewind | Opcode::Last => self.rewind(instruction, pager),
            Opcode::Next | Opcode::Prev => self.advance(instruction, pager),
            Opcode::SeekRowid => self.seek_rowid(instruction, pager),
            Opcode::SeekGe | Opcode::SeekGt => self.seek(instruction, pager),
            Opcode::IdxGe | Opcode::IdxGt => self.index_bound(instruction, pager),
            Opcode::IdxRowid => self.index_rowid(instruction, pager),
            Opcode::Column => self.column(instruction, pager),
            Opcode::IdxColumn => self.column(instruction, pager),
            Opcode::Rowid => {
                let null_row = self
                    .cursors
                    .get(instruction.p1.max(0) as usize)
                    .and_then(|slot| slot.as_ref())
                    .is_some_and(|slot| slot.null_row);
                if null_row {
                    self.store(instruction.p2, Value::Null);
                    return Ok(Flow::Next);
                }
                let rowid = self.with_cursor(instruction.p1, |slot| slot.cursor.rowid())?;
                self.store(instruction.p2, Value::Integer(rowid));
                Ok(Flow::Next)
            }
            Opcode::Null => {
                self.store(instruction.p2, Value::Null);
                Ok(Flow::Next)
            }
            Opcode::Load => {
                let value = self.operand_value(&instruction.p4)?;
                self.store(instruction.p2, value);
                Ok(Flow::Next)
            }
            Opcode::Copy => {
                let value = self.register(instruction.p1);
                let value = match instruction.p5 {
                    1 => normalise_limit(&value),
                    2 => normalise_offset(&value),
                    _ => value,
                };
                self.store(instruction.p2, value);
                Ok(Flow::Next)
            }
            Opcode::Arithmetic => {
                let Operand::Arithmetic(op) = instruction.p4 else {
                    return Err(error::misuse("Arithmetic without an operator"));
                };
                let left = self.register(instruction.p1);
                let right = self.register(instruction.p2);
                let value = eval::arithmetic(op, &left, &right, self.encoding);
                self.store(instruction.p3, value);
                Ok(Flow::Next)
            }
            Opcode::Negate => {
                let value = eval::negate(&self.register(instruction.p1));
                self.store(instruction.p2, value);
                Ok(Flow::Next)
            }
            Opcode::BitNot => {
                let value = eval::bit_not(&self.register(instruction.p1));
                self.store(instruction.p2, value);
                Ok(Flow::Next)
            }
            Opcode::Compare | Opcode::Is => self.compare(instruction),
            Opcode::And => {
                let value = eval::logical_and(
                    &self.register(instruction.p1),
                    &self.register(instruction.p2),
                );
                self.store(instruction.p3, value);
                Ok(Flow::Next)
            }
            Opcode::Or => {
                let value = eval::logical_or(
                    &self.register(instruction.p1),
                    &self.register(instruction.p2),
                );
                self.store(instruction.p3, value);
                Ok(Flow::Next)
            }
            Opcode::Not => {
                let value = eval::logical_not(&self.register(instruction.p1));
                self.store(instruction.p2, value);
                Ok(Flow::Next)
            }
            Opcode::IsNull => {
                let value = eval::is_null(&self.register(instruction.p1), instruction.p5 == 1);
                self.store(instruction.p2, value);
                Ok(Flow::Next)
            }
            Opcode::InList => self.in_list(instruction),
            Opcode::If | Opcode::IfNot => self.branch(instruction),
            Opcode::IfNull => {
                if self.register(instruction.p1).is_null() {
                    return Ok(Flow::Jump(instruction.p2.max(0) as usize));
                }
                Ok(Flow::Next)
            }
            Opcode::IfNotNull => {
                if !self.register(instruction.p1).is_null() {
                    return Ok(Flow::Jump(instruction.p2.max(0) as usize));
                }
                Ok(Flow::Next)
            }
            Opcode::IfPos => {
                let value = cast::integer_value(&self.register(instruction.p1));
                if value > 0 {
                    let decrement = i64::from(instruction.p3);
                    self.store(
                        instruction.p1,
                        Value::Integer(value.saturating_sub(decrement)),
                    );
                    return Ok(Flow::Jump(instruction.p2.max(0) as usize));
                }
                Ok(Flow::Next)
            }
            Opcode::DecrJumpZero => {
                let value = cast::integer_value(&self.register(instruction.p1));
                let next = value.saturating_sub(1);
                self.store(instruction.p1, Value::Integer(next));
                if next <= 0 {
                    return Ok(Flow::Jump(instruction.p2.max(0) as usize));
                }
                Ok(Flow::Next)
            }
            Opcode::Cast => {
                let Operand::Affinity(target) = instruction.p4 else {
                    return Err(error::misuse("Cast without a target"));
                };
                let value = self.register(instruction.p1);
                let cast = cast::cast_value(value, target, self.encoding)?;
                self.store(instruction.p2, cast.into_owned()?);
                Ok(Flow::Next)
            }
            Opcode::ApplyAffinity => {
                let Operand::Affinity(target) = instruction.p4 else {
                    return Err(error::misuse("Affinity without a target"));
                };
                for offset in 0..instruction.p2.max(0) {
                    let index = instruction.p1.saturating_add(offset);
                    let value = self.register(index);
                    let applied = affinity::apply_affinity(value, target, self.encoding)?;
                    self.store(index, applied.into_owned()?);
                }
                Ok(Flow::Next)
            }
            Opcode::Function => {
                let Operand::Scalar(func, collation) = instruction.p4 else {
                    return Err(error::misuse("Function without a function"));
                };
                let arguments = self.block(instruction.p1, instruction.p2);
                // Each call draws a fresh seed, so `random()` twice in one
                // statement gives two values rather than one repeated.
                self.entropy = self.entropy.wrapping_add(0x9E37_79B9_7F4A_7C15);
                let context = builtin::Context {
                    changes: self.changes,
                    total_changes: self.total_changes,
                    last_insert_rowid: self.last_insert_rowid,
                    seed: self.entropy,
                };
                let value = builtin::call_with(func, &arguments, collation, self.encoding, context);
                self.store(instruction.p3, value);
                Ok(Flow::Next)
            }
            Opcode::Pattern => self.pattern(instruction),
            Opcode::AggStep => self.aggregate_step(instruction),
            Opcode::AggFinal => {
                let slot = instruction.p1.max(0) as usize;
                let value = self
                    .accumulators
                    .get(slot)
                    .and_then(|accumulator| accumulator.as_ref())
                    .map(Accumulator::finish)
                    .unwrap_or(Value::Null);
                self.store(instruction.p2, value);
                Ok(Flow::Next)
            }
            Opcode::AggReset => {
                let Operand::Aggregate(call) = instruction.p4 else {
                    return Err(error::misuse("AggReset without an aggregate"));
                };
                if let Some(slot) = self.accumulators.get_mut(instruction.p1.max(0) as usize) {
                    *slot = Some(Accumulator::new(call.func, call.distinct, call.collation));
                }
                Ok(Flow::Next)
            }
            Opcode::SorterOpen => {
                let Operand::SortKey(key) = instruction.p4.clone() else {
                    return Err(error::misuse("SorterOpen without a key"));
                };
                if let Some(slot) = self.sorters.get_mut(instruction.p1.max(0) as usize) {
                    *slot = Some(Sorter::new(key));
                }
                Ok(Flow::Next)
            }
            Opcode::SorterInsert => {
                let row = self.block(instruction.p2, instruction.p3);
                if let Some(Some(sorter)) = self.sorters.get_mut(instruction.p1.max(0) as usize) {
                    sorter.insert(row);
                }
                Ok(Flow::Next)
            }
            Opcode::SorterSort => {
                let has_rows = match self.sorters.get_mut(instruction.p1.max(0) as usize) {
                    Some(Some(sorter)) => sorter.sort(),
                    _ => false,
                };
                if !has_rows {
                    return Ok(Flow::Jump(instruction.p2.max(0) as usize));
                }
                Ok(Flow::Next)
            }
            Opcode::SorterNext => {
                let more = match self.sorters.get_mut(instruction.p1.max(0) as usize) {
                    Some(Some(sorter)) => sorter.next(),
                    _ => false,
                };
                if more {
                    return Ok(Flow::Jump(instruction.p2.max(0) as usize));
                }
                Ok(Flow::Next)
            }
            Opcode::SorterColumn => {
                let value = match self.sorters.get(instruction.p1.max(0) as usize) {
                    Some(Some(sorter)) => sorter.column(instruction.p2.max(0) as usize),
                    _ => Value::Null,
                };
                self.store(instruction.p3, value);
                Ok(Flow::Next)
            }
            Opcode::DistinctOpen => {
                let key = match &instruction.p4 {
                    Operand::SortKey(key) => key.clone(),
                    _ => crate::program::SortKey {
                        columns: Vec::new(),
                    },
                };
                if let Some(slot) = self.distincts.get_mut(instruction.p1.max(0) as usize) {
                    *slot = DistinctSet::with_key(key);
                }
                Ok(Flow::Next)
            }
            Opcode::DistinctCheck => {
                let row = self.block(instruction.p3, i32::from(instruction.p5));
                let seen = match self.distincts.get_mut(instruction.p1.max(0) as usize) {
                    Some(set) => set.check(row),
                    None => false,
                };
                if seen {
                    return Ok(Flow::Jump(instruction.p2.max(0) as usize));
                }
                Ok(Flow::Next)
            }
            Opcode::NullRow => {
                if let Some(Some(slot)) = self.cursors.get_mut(instruction.p1.max(0) as usize) {
                    slot.payload = None;
                    slot.null_row = true;
                }
                Ok(Flow::Next)
            }
            Opcode::EphOpen => {
                let key = match (&instruction.p4, instruction.p5) {
                    (Operand::SortKey(key), 1) => Some(key.clone()),
                    _ => None,
                };
                let width = instruction.p2.max(0) as usize;
                if let Some(slot) = self.ephemerals.get_mut(instruction.p1.max(0) as usize) {
                    *slot = Some(Ephemeral::new(width, key));
                }
                Ok(Flow::Next)
            }
            Opcode::EphInsert => {
                let row = self.block(instruction.p2, instruction.p3);
                if let Some(Some(store)) = self.ephemerals.get_mut(instruction.p1.max(0) as usize) {
                    store.insert(row);
                }
                Ok(Flow::Next)
            }
            Opcode::EphInsertUnique => {
                let row = self.block(instruction.p3, i32::from(instruction.p5));
                let inserted = match self.ephemerals.get_mut(instruction.p1.max(0) as usize) {
                    Some(Some(store)) => store.insert_unique(row),
                    _ => false,
                };
                if !inserted {
                    return Ok(Flow::Jump(instruction.p2.max(0) as usize));
                }
                Ok(Flow::Next)
            }
            Opcode::EphRewind => {
                let has_rows = match self.ephemerals.get_mut(instruction.p1.max(0) as usize) {
                    Some(Some(store)) => store.rewind(),
                    _ => false,
                };
                if !has_rows {
                    return Ok(Flow::Jump(instruction.p2.max(0) as usize));
                }
                Ok(Flow::Next)
            }
            Opcode::EphNext => {
                let more = match self.ephemerals.get_mut(instruction.p1.max(0) as usize) {
                    Some(Some(store)) => store.next(),
                    _ => false,
                };
                if more {
                    return Ok(Flow::Jump(instruction.p2.max(0) as usize));
                }
                Ok(Flow::Next)
            }
            Opcode::EphColumn => {
                let value = match self.ephemerals.get(instruction.p1.max(0) as usize) {
                    Some(Some(store)) => store.column(instruction.p2.max(0) as usize),
                    _ => Value::Null,
                };
                self.store(instruction.p3, value);
                Ok(Flow::Next)
            }
            Opcode::EphFound | Opcode::EphNotFound => {
                let row = self.block(instruction.p3, i32::from(instruction.p5));
                let present = match self.ephemerals.get(instruction.p1.max(0) as usize) {
                    Some(Some(store)) => store.contains(&row),
                    _ => false,
                };
                let jump = if instruction.opcode == Opcode::EphFound {
                    present
                } else {
                    !present
                };
                if jump {
                    return Ok(Flow::Jump(instruction.p2.max(0) as usize));
                }
                Ok(Flow::Next)
            }
            Opcode::EphRemove => {
                let row = self.block(instruction.p3, i32::from(instruction.p5));
                let removed = match self.ephemerals.get_mut(instruction.p1.max(0) as usize) {
                    Some(Some(store)) => store.remove(&row),
                    _ => false,
                };
                if removed {
                    return Ok(Flow::Jump(instruction.p2.max(0) as usize));
                }
                Ok(Flow::Next)
            }
            Opcode::EphClear => {
                if let Some(Some(store)) = self.ephemerals.get_mut(instruction.p1.max(0) as usize) {
                    store.clear();
                }
                Ok(Flow::Next)
            }
            Opcode::EphDedup => {
                if let Some(Some(store)) = self.ephemerals.get_mut(instruction.p1.max(0) as usize) {
                    store.dedup();
                }
                Ok(Flow::Next)
            }
            Opcode::MathCall => {
                let Operand::Math(func) = instruction.p4 else {
                    return Err(error::misuse("a math call without its function"));
                };
                let arguments = self.block(instruction.p1, instruction.p2);
                let value = crate::mathfn::call(func, &arguments);
                self.store(instruction.p3, value);
                Ok(Flow::Next)
            }
            Opcode::TimeCall => {
                let Operand::Time(func) = instruction.p4 else {
                    return Err(error::misuse("a time call without its function"));
                };
                let arguments = self.block(instruction.p1, instruction.p2);
                let value = crate::datetime::call(func, &arguments, self.now, self.encoding);
                self.store(instruction.p3, value);
                Ok(Flow::Next)
            }
            Opcode::TypeCheck => {
                let Operand::Strict(kind, name) = instruction.p4.clone() else {
                    return Err(error::misuse("TypeCheck without a column"));
                };
                let value = self.register(instruction.p1);
                let class = match &value {
                    Value::Null => return Ok(Flow::Next),
                    Value::Integer(_) => "INTEGER",
                    Value::Real(_) => "REAL",
                    Value::Text(_) => "TEXT",
                    Value::Blob(_) => "BLOB",
                };
                let allowed = match kind {
                    crate::program::StrictType::Any => true,
                    crate::program::StrictType::Int => matches!(value, Value::Integer(_)),
                    // A REAL column takes an integer. SQLite stores a real
                    // whose value is exactly an integer with an *integer*
                    // serial type and widens it back on read, so the register
                    // here legitimately still holds an integer and `typeof()`
                    // still answers 'real'. Refusing it would refuse
                    // `INSERT INTO t(r) VALUES (2)`.
                    crate::program::StrictType::Real => {
                        matches!(value, Value::Real(_) | Value::Integer(_))
                    }
                    crate::program::StrictType::Text => matches!(value, Value::Text(_)),
                    crate::program::StrictType::Blob => matches!(value, Value::Blob(_)),
                };
                if allowed {
                    return Ok(Flow::Next);
                }
                // The message names the storage class the value actually had,
                // which is only known here: the compiler knows the column and
                // its declared type, and nothing else.
                let failure = rustdb_base::DbError::new(rustdb_base::error::ExtendedCode(
                    crate::compile_dml::codes::DATATYPE,
                ))
                .with_message(format!(
                    "cannot store {class} value in {} column {}",
                    kind.as_str(),
                    String::from_utf8_lossy(&name)
                ));
                Err(failure)
            }
            Opcode::EphSort => {
                let Operand::SortOn(key) = instruction.p4.clone() else {
                    return Err(error::misuse("EphSort without a key"));
                };
                if let Some(Some(store)) = self.ephemerals.get_mut(instruction.p1.max(0) as usize) {
                    store.sort_on(&key);
                }
                Ok(Flow::Next)
            }
            Opcode::Window => {
                let Operand::Window(plan) = instruction.p4.clone() else {
                    return Err(error::misuse("Window without a plan"));
                };
                let encoding = self.encoding;
                if let Some(Some(store)) = self.ephemerals.get_mut(instruction.p1.max(0) as usize) {
                    crate::window::compute(store, &plan, encoding);
                }
                Ok(Flow::Next)
            }
            Opcode::EphSawNull => {
                let saw = match self.ephemerals.get(instruction.p1.max(0) as usize) {
                    Some(Some(store)) => store.saw_null(),
                    _ => false,
                };
                self.store(instruction.p2, Value::Integer(i64::from(saw)));
                Ok(Flow::Next)
            }
            Opcode::ResultRow => {
                self.result = self.block(instruction.p1, instruction.p2);
                Ok(Flow::Row)
            }
            Opcode::OpenWrite => self.open_cursor(instruction, false),
            Opcode::OpenWriteIndex => self.open_cursor(instruction, true),
            Opcode::NewRowid => self.new_rowid(instruction, pager),
            Opcode::MakeRecord => self.make_record(instruction),
            Opcode::InsertRow => self.insert_row(instruction, pager),
            Opcode::DeleteRow => self.delete_row(instruction, pager),
            Opcode::IdxInsert => self.index_insert(instruction, pager),
            Opcode::IdxDelete => self.index_delete(instruction, pager),
            Opcode::NotExists => self.not_exists(instruction, pager),
            Opcode::NoConflict => self.no_conflict(instruction, pager),
            Opcode::RowData => self.row_data(instruction, pager),
            Opcode::HaltError => self.halt_error(instruction),
            Opcode::SetCookie => self.set_cookie(instruction, pager),
            Opcode::CreateBtree => self.create_btree(instruction, pager),
            Opcode::DestroyBtree => self.destroy_btree(instruction, pager),
            Opcode::ClearBtree => self.clear_btree(instruction, pager),
            Opcode::LastRowid => {
                if instruction.p2 == 1 {
                    self.last_insert_rowid = cast::integer_value(&self.register(instruction.p1));
                } else {
                    let value = Value::Integer(self.last_insert_rowid);
                    self.store(instruction.p1, value);
                }
                Ok(Flow::Next)
            }
            Opcode::CountChange => {
                // `p5` of 1 marks a row a trigger body wrote. It counts towards
                // `total_changes()` but not `changes()`, which reports what the
                // statement itself did.
                if instruction.p5 == 1 {
                    self.trigger_changes = self.trigger_changes.saturating_add(1);
                } else {
                    self.changes = self.changes.saturating_add(1);
                }
                let rowid = cast::integer_value(&self.register(instruction.p1));
                if instruction.p2 == 1 {
                    self.last_insert_rowid = rowid;
                }
                // The log is only kept when a hook is registered. A delete of a
                // million rows would otherwise buffer a million entries nobody
                // was going to read.
                if self.record_changes {
                    if let Operand::Change(kind, table) = &instruction.p4 {
                        self.row_changes.push(RowChange {
                            kind: *kind,
                            table: table.clone(),
                            rowid,
                        });
                    }
                }
                Ok(Flow::Next)
            }
        }
    }

    /// Allocates a rowid no row in the table is using.
    ///
    /// SQLite's rule, and the reason it is not simply "the largest plus one":
    /// once the largest rowid is `i64::MAX` the next one cannot be larger, so
    /// it falls back to picking at random until it finds a free one. A table
    /// that has ever held `i64::MAX` therefore keeps working instead of
    /// refusing every insert.
    fn new_rowid(&mut self, instruction: &Instruction, pager: &mut Pager) -> DbResult<Flow> {
        let largest = self.with_cursor_and_pager(instruction.p1, pager, |slot, pager| {
            if slot.cursor.last(pager)? {
                slot.moved();
                return slot.cursor.rowid().map(Some);
            }
            Ok(None)
        })?;
        let rowid = match largest {
            None => 1,
            Some(largest) if largest < i64::MAX => largest.saturating_add(1),
            Some(_) => self.random_free_rowid(instruction.p1, pager)?,
        };
        self.store(instruction.p2, Value::Integer(rowid));
        Ok(Flow::Next)
    }

    /// Picks a rowid at random until it finds one no row is using.
    ///
    /// The attempts are bounded: a table that really is full of every possible
    /// rowid has to report that rather than search for ever, and SQLite's own
    /// answer to a full table is `SQLITE_FULL`.
    fn random_free_rowid(&mut self, cursor: i32, pager: &mut Pager) -> DbResult<i64> {
        let mut state = self
            .steps
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1);
        for _ in 0..100 {
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            let candidate = ((state >> 1) as i64).saturating_abs().max(1);
            let taken = self.with_cursor_and_pager(cursor, pager, |slot, pager| {
                slot.moved();
                slot.cursor
                    .seek_rowid(pager, candidate, SeekBias::AtOrAfter)
            })?;
            if !taken {
                return Ok(candidate);
            }
        }
        Err(rustdb_base::DbError::primary(PrimaryCode::Full)
            .with_message("database or disk is full")
            .with_detail("no unused rowid could be found for this table"))
    }

    /// Encodes a block of registers into a record, applying each affinity.
    fn make_record(&mut self, instruction: &Instruction) -> DbResult<Flow> {
        let mut values = self.block(instruction.p1, instruction.p2);
        if let Operand::Affinities(affinities) = &instruction.p4 {
            for (index, affinity) in affinities.iter().enumerate() {
                let Some(slot) = values.get_mut(index) else {
                    break;
                };
                let taken = core::mem::replace(slot, Value::Null);
                *slot = affinity::apply_affinity(taken, *affinity, self.encoding)?;
            }
        }
        let bytes = record::encode_record_with_limits(
            &values,
            self.encoding,
            self.file_format,
            &self.limits,
        )?;
        self.store(instruction.p3, Value::owned_blob(&bytes)?);
        Ok(Flow::Next)
    }

    /// Returns a register's bytes, which a record register always holds.
    fn record_bytes(&self, index: i32) -> DbResult<Vec<u8>> {
        match self.register(index) {
            Value::Blob(blob) => Ok(blob.raw().to_vec()),
            _ => Err(error::misuse(
                "a record register does not hold an encoded record",
            )),
        }
    }

    /// Writes a row into a table B-tree.
    fn insert_row(&mut self, instruction: &Instruction, pager: &mut Pager) -> DbResult<Flow> {
        let payload = self.record_bytes(instruction.p2)?;
        let rowid = cast::integer_value(&self.register(instruction.p3));
        let root = self.cursor_root(instruction.p1)?;
        let append = instruction.p5 == 1;
        let saved = self.save_cursors_on(root, instruction.p1, pager)?;
        if append {
            mutate::append_row(pager, root, rowid, &payload)?;
        } else {
            mutate::insert_row(pager, root, rowid, &payload)?;
        }
        self.invalidate_cursors_on(root);
        self.restore_cursors(saved, pager)?;
        Ok(Flow::Next)
    }

    /// Deletes the row a cursor is sitting on.
    fn delete_row(&mut self, instruction: &Instruction, pager: &mut Pager) -> DbResult<Flow> {
        let rowid = self.with_cursor(instruction.p1, |slot| slot.cursor.rowid())?;
        let root = self.cursor_root(instruction.p1)?;
        let saved = self.save_cursors_on(root, instruction.p1, pager)?;
        mutate::delete_row(pager, root, rowid)?;
        self.invalidate_cursors_on(root);
        self.restore_cursors(saved, pager)?;
        Ok(Flow::Next)
    }

    /// Writes an entry into an index B-tree.
    fn index_insert(&mut self, instruction: &Instruction, pager: &mut Pager) -> DbResult<Flow> {
        let payload = self.record_bytes(instruction.p2)?;
        let root = self.cursor_root(instruction.p1)?;
        let key = self.cursor_key(instruction.p1)?;
        let saved = self.save_cursors_on(root, instruction.p1, pager)?;
        mutate::insert_entry(pager, root, &key, &payload)?;
        self.invalidate_cursors_on(root);
        self.restore_cursors(saved, pager)?;
        Ok(Flow::Next)
    }

    /// Removes an entry from an index B-tree.
    ///
    /// A missing entry is not an error. An UPDATE that does not change a key
    /// deletes and reinserts the same entry, and a REPLACE may have removed it
    /// already; refusing here would turn a no-op into a failure.
    fn index_delete(&mut self, instruction: &Instruction, pager: &mut Pager) -> DbResult<Flow> {
        let payload = self.record_bytes(instruction.p2)?;
        let root = self.cursor_root(instruction.p1)?;
        let key = self.cursor_key(instruction.p1)?;
        let saved = self.save_cursors_on(root, instruction.p1, pager)?;
        mutate::delete_entry(pager, root, &key, &payload)?;
        self.invalidate_cursors_on(root);
        self.restore_cursors(saved, pager)?;
        Ok(Flow::Next)
    }

    /// Jumps when no row has the rowid in a register.
    fn not_exists(&mut self, instruction: &Instruction, pager: &mut Pager) -> DbResult<Flow> {
        let value = self.register(instruction.p3);
        let Value::Integer(rowid) = cast::numerify(value) else {
            return Ok(Flow::Jump(instruction.p2.max(0) as usize));
        };
        let found = self.with_cursor_and_pager(instruction.p1, pager, |slot, pager| {
            slot.moved();
            slot.cursor.seek_rowid(pager, rowid, SeekBias::AtOrAfter)
        })?;
        if found {
            return Ok(Flow::Next);
        }
        Ok(Flow::Jump(instruction.p2.max(0) as usize))
    }

    /// Jumps when an index holds no entry with this key prefix.
    fn no_conflict(&mut self, instruction: &Instruction, pager: &mut Pager) -> DbResult<Flow> {
        let key = self.block(instruction.p3, i32::from(instruction.p5));
        // A NULL never equals anything, so a key containing one cannot
        // conflict with an existing entry however many entries look like it.
        if key.iter().any(|value| matches!(value, Value::Null)) {
            return Ok(Flow::Jump(instruction.p2.max(0) as usize));
        }
        let found = self.with_cursor_and_pager(instruction.p1, pager, |slot, pager| {
            slot.moved();
            slot.cursor.seek_index(pager, &key, SeekBias::AtOrAfter)
        })?;
        if found {
            return Ok(Flow::Next);
        }
        Ok(Flow::Jump(instruction.p2.max(0) as usize))
    }

    /// Copies the row a cursor is on into a register, as raw record bytes.
    fn row_data(&mut self, instruction: &Instruction, pager: &mut Pager) -> DbResult<Flow> {
        let limits = self.limits.clone();
        let bytes = self.with_cursor_and_pager(instruction.p1, pager, |slot, pager| {
            slot.cursor.payload(pager, &limits)
        })?;
        self.store(instruction.p2, Value::owned_blob(&bytes)?);
        Ok(Flow::Next)
    }

    /// Stops the program with a named result code.
    fn halt_error(&mut self, instruction: &Instruction) -> DbResult<Flow> {
        let message = match &instruction.p4 {
            Operand::Text(text) => String::from_utf8_lossy(text).into_owned(),
            _ => String::new(),
        };
        let code = rustdb_base::error::ExtendedCode(instruction.p1);
        self.conflict = Some(instruction.p3);
        let mut failure = rustdb_base::DbError::new(code);
        if !message.is_empty() {
            failure = failure.with_message(message);
        }
        Err(failure)
    }

    /// Writes a new schema cookie into the database header.
    fn set_cookie(&mut self, instruction: &Instruction, pager: &mut Pager) -> DbResult<Flow> {
        let mut header = *pager.header();
        header.schema_cookie = instruction.p3.max(0) as u32;
        pager.set_header(header)?;
        Ok(Flow::Next)
    }

    /// Allocates an empty B-tree and stores its root page in a register.
    fn create_btree(&mut self, instruction: &Instruction, pager: &mut Pager) -> DbResult<Flow> {
        let root = if instruction.p3 == 1 {
            mutate::create_index(pager)?
        } else {
            mutate::create_table(pager)?
        };
        self.store(instruction.p2, Value::Integer(i64::from(root.get())));
        Ok(Flow::Next)
    }

    /// Frees every page of a B-tree.
    fn destroy_btree(&mut self, instruction: &Instruction, pager: &mut Pager) -> DbResult<Flow> {
        let root = self.root_from_register(instruction.p1)?;
        mutate::drop_tree(pager, root)?;
        self.invalidate_cursors_on(root);
        Ok(Flow::Next)
    }

    /// Empties a B-tree, keeping its root page.
    fn clear_btree(&mut self, instruction: &Instruction, pager: &mut Pager) -> DbResult<Flow> {
        let root = self.root_from_register(instruction.p1)?;
        mutate::clear_tree(pager, root)?;
        self.invalidate_cursors_on(root);
        Ok(Flow::Next)
    }

    /// Reads a root page number out of a register.
    fn root_from_register(&self, index: i32) -> DbResult<PageId> {
        let value = cast::integer_value(&self.register(index));
        let page = u32::try_from(value)
            .map_err(|_| error::corrupt(format!("root page {value} is not a page number")))?;
        PageId::from_persisted(page)
    }

    /// Returns the root page a cursor is open on.
    fn cursor_root(&mut self, index: i32) -> DbResult<PageId> {
        self.with_cursor(index, |slot| Ok(slot.cursor.root()))
    }

    /// Returns the key description a cursor was opened with.
    fn cursor_key(&mut self, index: i32) -> DbResult<KeyInfo> {
        self.with_cursor(index, |slot| Ok(slot.key.clone()))
    }

    /// Forgets every cached row on a tree that has just been rewritten.
    ///
    /// A B-tree write can move a cell to another page, so a cursor holding a
    /// parsed position on that tree is describing a page that may no longer
    /// hold what it did. The positions themselves are restored by the cursor's
    /// own staleness check; what has to be dropped here is the cached payload,
    /// which nothing else would notice was out of date.
    fn invalidate_cursors_on(&mut self, root: PageId) {
        for slot in self.cursors.iter_mut().flatten() {
            if slot.cursor.root() == root {
                slot.payload = None;
            }
        }
    }

    /// Records where every *other* cursor on one tree is sitting.
    ///
    /// A cursor's stack is page numbers and slot indices, and a write moves
    /// cells between pages: after it, slot four of page nine is a different
    /// entry or none at all. Until triggers there was never a second cursor on
    /// a tree being written, so clearing the cached row was enough. A trigger
    /// whose body writes the table that fired it has two, and the outer one was
    /// left standing on a page that had been rebalanced underneath it - which
    /// surfaced as `an unpositioned cursor was read`, reported as a malformed
    /// database.
    ///
    /// The work is skipped unless a second cursor really is on the tree, so the
    /// ordinary single-cursor write pays a comparison and nothing else.
    fn save_cursors_on(
        &mut self,
        root: PageId,
        writer: i32,
        pager: &mut Pager,
    ) -> DbResult<Vec<(usize, SavedPosition)>> {
        let index = writer.max(0) as usize;
        let sharing = self
            .cursors
            .iter()
            .enumerate()
            .filter(|(position, slot)| {
                *position != index && slot.as_ref().is_some_and(|slot| slot.cursor.root() == root)
            })
            .count();
        if sharing == 0 {
            return Ok(Vec::new());
        }
        let limits = self.limits.clone();
        let mut saved = Vec::with_capacity(sharing);
        for (position, slot) in self.cursors.iter_mut().enumerate() {
            if position == index {
                continue;
            }
            let Some(slot) = slot else { continue };
            if slot.cursor.root() != root {
                continue;
            }
            saved.push((position, slot.cursor.save_position(pager, &limits)?));
        }
        Ok(saved)
    }

    /// Puts the saved cursors back on the entries they were reading.
    ///
    /// A cursor whose own row the write removed lands on the next entry, which
    /// is what `restore` reports by returning false - and is the right answer:
    /// SQLite's `sqlite3BtreeCursorHasMoved` leaves such a cursor needing a
    /// step rather than pointing at a row that is gone.
    fn restore_cursors(
        &mut self,
        saved: Vec<(usize, SavedPosition)>,
        pager: &mut Pager,
    ) -> DbResult<()> {
        let limits = self.limits.clone();
        for (position, where_it_was) in saved {
            let Some(Some(slot)) = self.cursors.get_mut(position) else {
                continue;
            };
            slot.payload = None;
            slot.cursor.restore(pager, &where_it_was, &limits)?;
        }
        Ok(())
    }

    /// Runs a closure over one open cursor and the pager together.
    fn with_cursor_and_pager<T>(
        &mut self,
        index: i32,
        pager: &mut Pager,
        body: impl FnOnce(&mut CursorSlot, &mut Pager) -> DbResult<T>,
    ) -> DbResult<T> {
        let Some(Some(slot)) = self.cursors.get_mut(index.max(0) as usize) else {
            return Err(error::misuse(format!("cursor {index} is not open")));
        };
        body(slot, pager)
    }

    /// Turns a `p4` literal into a value.
    fn operand_value(&self, operand: &Operand) -> DbResult<Value<'static>> {
        let value = match operand {
            Operand::Null | Operand::None => Value::Null,
            Operand::Integer(integer) => Value::Integer(*integer),
            Operand::Real(real) => Value::Real(*real),
            Operand::Text(text) => Value::owned_text(text)?,
            Operand::Blob(bytes) => Value::owned_blob(bytes)?,
            Operand::Parameter(index) => self
                .bindings
                .get(index.saturating_sub(1) as usize)
                .cloned()
                .unwrap_or(Value::Null),
            _ => return Err(error::misuse("a literal operand of the wrong kind")),
        };
        Ok(value)
    }

    /// Opens a cursor on a root page.
    fn open_cursor(&mut self, instruction: &Instruction, is_index: bool) -> DbResult<Flow> {
        let root = PageId::from_persisted(instruction.p2.max(0) as u32)?;
        let key = match &instruction.p4 {
            Operand::IndexKey(key) => KeyInfo {
                columns: key
                    .columns
                    .iter()
                    .map(|column| KeyColumn {
                        collation: column.collation,
                        descending: column.descending,
                    })
                    .collect(),
            },
            _ => KeyInfo::default(),
        };
        let cursor = if is_index {
            BTreeCursor::index(root, key.clone())
        } else {
            BTreeCursor::table(root)
        };
        if let Some(slot) = self.cursors.get_mut(instruction.p1.max(0) as usize) {
            *slot = Some(CursorSlot {
                null_row: false,
                cursor,
                payload: None,
                is_index,
                key,
            });
        }
        Ok(Flow::Next)
    }

    /// Runs a closure over one open cursor.
    fn with_cursor<T>(
        &mut self,
        index: i32,
        body: impl FnOnce(&mut CursorSlot) -> DbResult<T>,
    ) -> DbResult<T> {
        let Some(Some(slot)) = self.cursors.get_mut(index.max(0) as usize) else {
            return Err(error::misuse(format!("cursor {index} is not open")));
        };
        body(slot)
    }

    /// Positions a cursor at the first or last row.
    fn rewind(&mut self, instruction: &Instruction, pager: &mut Pager) -> DbResult<Flow> {
        let last = instruction.opcode == Opcode::Last;
        let Some(Some(slot)) = self.cursors.get_mut(instruction.p1.max(0) as usize) else {
            return Err(error::misuse("cursor is not open"));
        };
        slot.moved();
        let found = if last {
            slot.cursor.last(pager)?
        } else {
            slot.cursor.first(pager)?
        };
        if !found {
            return Ok(Flow::Jump(instruction.p2.max(0) as usize));
        }
        Ok(Flow::Next)
    }

    /// Steps a cursor forward or backward.
    fn advance(&mut self, instruction: &Instruction, pager: &mut Pager) -> DbResult<Flow> {
        let backwards = instruction.opcode == Opcode::Prev;
        let Some(Some(slot)) = self.cursors.get_mut(instruction.p1.max(0) as usize) else {
            return Err(error::misuse("cursor is not open"));
        };
        slot.moved();
        let more = if backwards {
            slot.cursor.previous(pager)?
        } else {
            slot.cursor.next(pager)?
        };
        if more {
            return Ok(Flow::Jump(instruction.p2.max(0) as usize));
        }
        Ok(Flow::Next)
    }

    /// Seeks a table cursor to an exact rowid.
    fn seek_rowid(&mut self, instruction: &Instruction, pager: &mut Pager) -> DbResult<Flow> {
        let key = self.register(instruction.p3);
        // A rowid that is not an exact integer matches nothing: SQLite refuses
        // to round 1.5 into rowid 1 or 2, it simply finds no row.
        let rowid = match &key {
            Value::Integer(integer) => *integer,
            Value::Real(real) => {
                let candidate = rustdb_value::numeric::real_to_i64(*real);
                if !rustdb_value::numeric::real_same_as_int(*real, candidate) {
                    return Ok(Flow::Jump(instruction.p2.max(0) as usize));
                }
                candidate
            }
            Value::Null => return Ok(Flow::Jump(instruction.p2.max(0) as usize)),
            other => {
                let numeric = cast::numerify(other.clone());
                match numeric {
                    Value::Integer(integer) => integer,
                    _ => return Ok(Flow::Jump(instruction.p2.max(0) as usize)),
                }
            }
        };
        let Some(Some(slot)) = self.cursors.get_mut(instruction.p1.max(0) as usize) else {
            return Err(error::misuse("cursor is not open"));
        };
        slot.moved();
        let found = slot.cursor.seek_rowid(pager, rowid, SeekBias::AtOrAfter)?;
        if !found {
            return Ok(Flow::Jump(instruction.p2.max(0) as usize));
        }
        Ok(Flow::Next)
    }

    /// Positions a cursor at the start of a range.
    fn seek(&mut self, instruction: &Instruction, pager: &mut Pager) -> DbResult<Flow> {
        let strict = instruction.opcode == Opcode::SeekGt;
        let count = i32::from(instruction.p5);
        let key = self.block(instruction.p3, count);
        let Some(Some(slot)) = self.cursors.get_mut(instruction.p1.max(0) as usize) else {
            return Err(error::misuse("cursor is not open"));
        };
        slot.moved();
        if !slot.is_index {
            let rowid = key.first().map(cast::integer_value).unwrap_or(0);
            // `> n` on a table cursor is `>= n + 1`, because a rowid is an
            // integer and there is nothing between n and n + 1.
            let target = if strict {
                rowid.saturating_add(1)
            } else {
                rowid
            };
            let found = slot.cursor.seek_rowid(pager, target, SeekBias::AtOrAfter)?;
            if !found {
                return Ok(Flow::Jump(instruction.p2.max(0) as usize));
            }
            return Ok(Flow::Next);
        }
        let borrowed: Vec<Value<'_>> = key.clone();
        let exact = slot
            .cursor
            .seek_index(pager, &borrowed, SeekBias::AtOrAfter)?;
        if !slot.cursor.is_positioned() {
            return Ok(Flow::Jump(instruction.p2.max(0) as usize));
        }
        if exact && strict {
            // The seek landed on an equal entry and the range excludes it, so
            // walk forward past every entry equal on the probed prefix.
            loop {
                let payload = slot.cursor.payload(pager, &self.limits)?;
                let record = RecordRef::parse_with_limits(&payload, self.encoding, &self.limits)?;
                let ordering =
                    rustdb_value::record::compare_values_to_record(&borrowed, &record, &slot.key)?;
                if ordering != std::cmp::Ordering::Equal {
                    break;
                }
                slot.moved();
                if !slot.cursor.next(pager)? {
                    return Ok(Flow::Jump(instruction.p2.max(0) as usize));
                }
            }
        }
        Ok(Flow::Next)
    }

    /// Stops an index scan once the entry passes the bound.
    fn index_bound(&mut self, instruction: &Instruction, pager: &mut Pager) -> DbResult<Flow> {
        let count = i32::from(instruction.p5);
        let key = self.block(instruction.p3, count);
        let limits = self.limits.clone();
        let encoding = self.encoding;
        let Some(Some(slot)) = self.cursors.get_mut(instruction.p1.max(0) as usize) else {
            return Err(error::misuse("cursor is not open"));
        };
        if !slot.cursor.is_positioned() {
            return Ok(Flow::Jump(instruction.p2.max(0) as usize));
        }
        let payload = slot.cursor.payload(pager, &limits)?;
        let record = RecordRef::parse_with_limits(&payload, encoding, &limits)?;
        let ordering = rustdb_value::record::compare_values_to_record(&key, &record, &slot.key)?;
        // `ordering` compares the probe against the entry, so an entry past the
        // bound makes the probe compare Less.
        let past = match instruction.opcode {
            Opcode::IdxGe => ordering != std::cmp::Ordering::Greater,
            _ => ordering == std::cmp::Ordering::Less,
        };
        if past {
            return Ok(Flow::Jump(instruction.p2.max(0) as usize));
        }
        Ok(Flow::Next)
    }

    /// Reads the rowid an index entry carries in its last field.
    fn index_rowid(&mut self, instruction: &Instruction, pager: &mut Pager) -> DbResult<Flow> {
        let limits = self.limits.clone();
        let encoding = self.encoding;
        let value = {
            let Some(Some(slot)) = self.cursors.get_mut(instruction.p1.max(0) as usize) else {
                return Err(error::misuse("cursor is not open"));
            };
            let payload = slot.cursor.payload(pager, &limits)?;
            let record = RecordRef::parse_with_limits(&payload, encoding, &limits)?;
            let last = record.field_count().saturating_sub(1);
            record.value(last)?.into_owned()?
        };
        self.store(instruction.p2, value);
        Ok(Flow::Next)
    }

    /// Reads one column of the row a cursor is on.
    fn column(&mut self, instruction: &Instruction, pager: &mut Pager) -> DbResult<Flow> {
        let limits = self.limits.clone();
        let encoding = self.encoding;
        let index = instruction.p2.max(0) as usize;
        // Resolved before the cursor is borrowed, because the borrow lasts as
        // long as the cached row does.
        let absent = match &instruction.p4 {
            Operand::None => Value::Null,
            other => self.operand_value(other)?,
        };
        let value = {
            let Some(Some(slot)) = self.cursors.get_mut(instruction.p1.max(0) as usize) else {
                return Err(error::misuse("cursor is not open"));
            };
            if slot.null_row {
                self.store(instruction.p3, Value::Null);
                return Ok(Flow::Next);
            }
            if slot.payload.is_none() {
                slot.payload = Some(slot.cursor.payload(pager, &limits)?);
            }
            // Borrow the cached row rather than copying it. Cloning here cost a
            // whole-row copy *per column read*, which is three copies of every
            // row of a three-column projection and was the largest single
            // allocation source the baselines found.
            let payload: &[u8] = slot.payload.as_deref().unwrap_or(&[]);
            let record = RecordRef::parse_with_limits(payload, encoding, &limits)?;
            if index >= record.field_count() {
                // A column past the end of the record reads as its DEFAULT, and
                // as NULL when it has none. This happens for real: `ALTER TABLE
                // ADD COLUMN` does not rewrite the rows that already existed, so
                // their records stop before the new column - and SQLite reads
                // the default back for exactly those rows.
                absent
            } else {
                let value = record.value(index)?.into_owned()?;
                if instruction.p5 == 1 {
                    affinity::realify(value).into_owned()?
                } else {
                    value
                }
            }
        };
        self.store(instruction.p3, value);
        Ok(Flow::Next)
    }

    /// Evaluates a comparison instruction.
    fn compare(&mut self, instruction: &Instruction) -> DbResult<Flow> {
        let Operand::Comparison(comparison) = instruction.p4 else {
            return Err(error::misuse("a comparison without its rules"));
        };
        let left = self.register(instruction.p1);
        let right = self.register(instruction.p2);
        let value = if instruction.opcode == Opcode::Is {
            eval::is_comparison(
                comparison.op == BinaryOp::NotEqual,
                &left,
                &right,
                comparison.affinity,
                comparison.collation,
                self.encoding,
            )
        } else {
            eval::comparison(
                comparison.op,
                &left,
                &right,
                comparison.affinity,
                comparison.collation,
                self.encoding,
            )
        };
        self.store(instruction.p3, value);
        Ok(Flow::Next)
    }

    /// Evaluates `IN` over a value list.
    ///
    /// The three-valued rule is the subtle part: `x IN (1, NULL)` is NULL when
    /// `x` is 2, not false, because the NULL might have been 2.
    fn in_list(&mut self, instruction: &Instruction) -> DbResult<Flow> {
        let Operand::Comparison(comparison) = instruction.p4 else {
            return Err(error::misuse("IN without its rules"));
        };
        let negated = comparison.op == BinaryOp::NotEqual;
        let value = self.register(instruction.p1);
        let list = self.block(instruction.p2, i32::from(instruction.p5));
        if value.is_null() {
            self.store(instruction.p3, Value::Null);
            return Ok(Flow::Next);
        }
        let mut saw_null = false;
        let mut found = false;
        for candidate in &list {
            if candidate.is_null() {
                saw_null = true;
                continue;
            }
            let equal = eval::comparison(
                BinaryOp::Equal,
                &value,
                candidate,
                comparison.affinity,
                comparison.collation,
                self.encoding,
            );
            if matches!(equal, Value::Integer(1)) {
                found = true;
                break;
            }
        }
        let result = if found {
            Value::Integer(i64::from(!negated))
        } else if saw_null {
            Value::Null
        } else {
            Value::Integer(i64::from(negated))
        };
        self.store(instruction.p3, result);
        Ok(Flow::Next)
    }

    /// Evaluates a `LIKE` or `GLOB`.
    fn pattern(&mut self, instruction: &Instruction) -> DbResult<Flow> {
        let Operand::Pattern(op) = instruction.p4 else {
            return Err(error::misuse("a pattern without an operator"));
        };
        let arguments = self.block(instruction.p1, instruction.p2);
        let func = match op {
            rustdb_sql::ast::PatternOp::Glob => rustdb_sql::function::ScalarFunc::Glob,
            _ => rustdb_sql::function::ScalarFunc::Like,
        };
        let pattern_length = arguments.first().map(|value| value.byte_len()).unwrap_or(0);
        if pattern_length as i64
            > self
                .limits
                .get(rustdb_base::limits::Limit::LikePatternLength)
        {
            return Err(error::too_big("LIKE or GLOB pattern too complex"));
        }
        let value = builtin::call(func, &arguments, Collation::Binary, self.encoding);
        let value = if instruction.p5 == 1 {
            eval::logical_not(&value)
        } else {
            value
        };
        self.store(instruction.p3, value);
        Ok(Flow::Next)
    }

    /// Feeds one row into an accumulator.
    fn aggregate_step(&mut self, instruction: &Instruction) -> DbResult<Flow> {
        let Operand::Aggregate(call) = instruction.p4 else {
            return Err(error::misuse("AggStep without an aggregate"));
        };
        let arguments = self.block(instruction.p1, instruction.p2);
        let encoding = self.encoding;
        let Some(slot) = self.accumulators.get_mut(instruction.p3.max(0) as usize) else {
            return Err(error::misuse("an aggregate slot that does not exist"));
        };
        let accumulator =
            slot.get_or_insert_with(|| Accumulator::new(call.func, call.distinct, call.collation));
        accumulator.step(&arguments, encoding);
        Ok(Flow::Next)
    }

    /// Takes a branch on a register's truth value.
    fn branch(&mut self, instruction: &Instruction) -> DbResult<Flow> {
        let value = self.register(instruction.p1);
        let truth = eval::truth(&value);
        let jump = match instruction.opcode {
            Opcode::If => match truth {
                rustdb_value::compare::Truth::True => true,
                rustdb_value::compare::Truth::Unknown => instruction.p5 == 1,
                rustdb_value::compare::Truth::False => false,
            },
            _ => match truth {
                rustdb_value::compare::Truth::False => true,
                rustdb_value::compare::Truth::Unknown => instruction.p5 == 1,
                rustdb_value::compare::Truth::True => false,
            },
        };
        if jump {
            return Ok(Flow::Jump(instruction.p2.max(0) as usize));
        }
        Ok(Flow::Next)
    }
}

/// What an instruction decided about control flow.
enum Flow {
    /// Continue with the next instruction.
    Next,
    /// Jump to an address.
    Jump(usize),
    /// A result row is ready.
    Row,
    /// The program is finished.
    Halt,
}

/// Turns a LIMIT expression's value into a counter.
///
/// NULL and a negative number both mean "no limit", which becomes a counter
/// large enough that it can never run out.
fn normalise_limit(value: &Value<'_>) -> Value<'static> {
    if value.is_null() {
        return Value::Integer(i64::MAX);
    }
    let limit = cast::integer_value(value);
    if limit < 0 {
        return Value::Integer(i64::MAX);
    }
    Value::Integer(limit)
}

/// Turns an OFFSET expression's value into a counter.
fn normalise_offset(value: &Value<'_>) -> Value<'static> {
    if value.is_null() {
        return Value::Integer(0);
    }
    Value::Integer(cast::integer_value(value).max(0))
}

/// Returns the affinity a value should be given before it is stored, which the
/// read-only engine never needs but the writer will.
pub fn storage_affinity(affinity: Affinity) -> Affinity {
    affinity
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A NULL or negative LIMIT means no limit, and a NULL OFFSET means none.
    #[test]
    fn limit_and_offset_normalise_the_way_sqlite_reads_them() {
        assert_same!(normalise_limit(&Value::Null), Value::Integer(i64::MAX));
        assert_same!(
            normalise_limit(&Value::Integer(-1)),
            Value::Integer(i64::MAX)
        );
        assert_same!(normalise_limit(&Value::Integer(3)), Value::Integer(3));
        assert_same!(normalise_offset(&Value::Null), Value::Integer(0));
        assert_same!(normalise_offset(&Value::Integer(-5)), Value::Integer(0));
    }
}
