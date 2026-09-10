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

use inillucent_base::ids::PageId;
use inillucent_base::limits::Limits;
use inillucent_base::{error, DbResult, PrimaryCode};
use inillucent_sql::ast::BinaryOp;
use inillucent_storage::cursor::{BTreeCursor, SavedPosition, SeekBias};
use inillucent_storage::mutate;
use inillucent_storage::pager::Pager;
use inillucent_storage::PagerSet;
use inillucent_value::record::{self, KeyColumn, KeyInfo, RecordRef};
use inillucent_value::{affinity, cast, Affinity, Collation, TextEncoding, Value};

use crate::aggregate::Accumulator;
use crate::builtin;
use crate::ephemeral::Ephemeral;
use crate::eval;
use crate::host::Host;
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
    /// Which attached database the cursor is open on.
    ///
    /// Two databases have a page 2 each, so a root page number is only an
    /// address when it is paired with the database it is in. Everything that
    /// compares roots - saving the cursors a write disturbs, invalidating the
    /// ones a dropped tree leaves behind - compares this too.
    database: usize,
    cursor: BTreeCursor,
    payload: Option<Vec<u8>>,
    /// Where each field of the cached row lives.
    ///
    /// Parsing a record is a walk of its whole header, and reading a column
    /// used to do that walk - and a heap allocation - once per column. A
    /// two-column projection therefore parsed every row twice.
    ///
    /// The buffer is owned rather than held in an `Option`, and a move clears
    /// `record_parsed` rather than dropping it. Holding it in an `Option` and
    /// setting that `Option` to `None` on every move is what the previous shape
    /// did, and it gave the buffer back to the allocator once per row: a scan
    /// of five thousand rows that read one integer column was measured making
    /// three heap allocations per row, of which two were this vector being
    /// freed and grown again. It now allocates once per cursor and is refilled
    /// in place, which is what the comment here always claimed.
    fields: Vec<inillucent_value::record::FieldSpan>,
    /// How long the cached row's record header is.
    header_len: usize,
    /// Whether `fields` describes the row the cursor is standing on.
    record_parsed: bool,
    /// Whether `payload` holds the row the cursor is standing on.
    ///
    /// Separate from `payload` being `Some`, because the buffer is kept across
    /// rows: an empty buffer that is about to be refilled and an empty buffer
    /// holding a zero-length row are the same `Some(vec![])`, and only this
    /// says which.
    row_is_loaded: bool,
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
        self.row_is_loaded = false;
        // The spans describe the row that was there, so they stop being valid;
        // the buffer holding them is kept, so the next row refills it rather
        // than allocating a new one.
        self.record_parsed = false;
        self.null_row = false;
    }
}

/// One open virtual-table cursor.
struct VirtualSlot {
    /// Which table the cursor is on, so the host can find its module again.
    reference: crate::program::VirtualRef,
    /// The module's cursor.
    cursor: Box<dyn inillucent_ext::vtab::VirtualCursor>,
    /// Whether `filter` has positioned it.
    filtered: bool,
}

/// The machine.
pub struct Machine {
    program: Arc<Program>,
    registers: Vec<Value<'static>>,
    /// Which registers hold a value that is JSON rather than text that
    /// looks like it.
    ///
    /// SQLite calls this the value's subtype and keeps it in the same
    /// structure as the value. Here it is a parallel array, for one
    /// reason: `Value` is the type every layer of the engine passes
    /// around, and a field only the JSON functions read would be carried
    /// through the record codec, the b-tree and the comparison rules by
    /// everything that never looks at it. The mark belongs to the
    /// register, not to the value - which is also why it does not survive
    /// being written to a row.
    register_marks: Vec<bool>,
    cursors: Vec<Option<CursorSlot>>,
    /// The virtual cursors, numbered alongside the b-tree ones.
    ///
    /// A parallel array rather than a variant of `CursorSlot`, because a
    /// virtual cursor shares nothing with a b-tree cursor: no page, no
    /// saved position, no null row. Every opcode that touches one is a
    /// virtual opcode, so nothing has to ask which kind it is holding.
    virtual_cursors: Vec<Option<VirtualSlot>>,
    sorters: Vec<Option<Sorter>>,
    distincts: Vec<DistinctSet>,
    ephemerals: Vec<Option<Ephemeral>>,
    accumulators: Vec<Option<Accumulator>>,
    bindings: Vec<Value<'static>>,
    /// A buffer for the register blocks opcodes pass around, reused.
    ///
    /// An opcode that takes several registers - an aggregate step, a record to
    /// encode, an index key, a function call - used to collect them into a
    /// fresh `Vec` every time it ran. On a scan that is one heap allocation per
    /// row per such opcode: `SELECT count(*), sum(key), max(category)` was
    /// measured making eight hundred thousand allocations over two hundred
    /// thousand rows, and every one of them was this vector and its marks.
    ///
    /// It is taken out with `mem::take` while it is filled, because the machine
    /// is borrowed mutably to read the registers, and put back after - so the
    /// capacity survives even though the borrow does not.
    scratch_values: Vec<Value<'static>>,
    /// The JSON marks that go with `scratch_values`, reused the same way.
    scratch_marks: Vec<bool>,
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
    /// What `changes()` answers: the connection's, not this statement's.
    reported_changes: i64,
    /// What `total_changes()` answers, for the same reason.
    reported_total_changes: i64,
    last_insert_rowid: i64,
    conflict: Option<i32>,
    record_changes: bool,
    row_changes: Vec<RowChange>,
    /// The callback that may stop a long statement, and how often to ask it.
    progress: Option<Progress>,
    /// The functions an application registered on the connection.
    ///
    /// A table rather than a host: a scalar call and an aggregate's finish both
    /// happen deep in the ordinary instruction path, and threading a host down
    /// there to reach two call sites would be a far larger change than a
    /// pointer the machine already holds.
    functions: Option<Arc<dyn ExternalFunctions>>,
}

/// What the machine calls for a function this engine did not write.
///
/// The two halves are deliberately different shapes. A scalar sees one row's
/// arguments; an aggregate sees the whole group at once, because an
/// implementation on the other side of a C boundary keeps its accumulator in
/// memory this engine must not look inside, and driving its `xStep` and
/// `xFinal` at the end of the group is what keeps that state over there.
pub trait ExternalFunctions: Send + Sync {
    /// Calls a scalar function on one row's arguments.
    fn call(&self, name: &[u8], arguments: &[Value<'static>]) -> DbResult<Value<'static>>;

    /// Reduces a group to one value.
    fn reduce(&self, name: &[u8], rows: &[Vec<Value<'static>>]) -> DbResult<Value<'static>>;
}

/// A callback the machine asks, now and then, whether to give up.
///
/// Returning `true` stops the statement with `SQLITE_INTERRUPT`, which is what
/// SQLite's own progress handler does with a non-zero return. It is the only
/// way for an application with one thread to abandon a query that is taking
/// too long, so it has to be cheap enough to call often and safe to call from
/// inside the machine - which is why it takes nothing and returns a bool.
pub type ProgressHandler = Arc<dyn Fn() -> bool + Send + Sync>;

/// How often the machine asks the progress callback, and who it asks.
#[derive(Clone)]
pub struct Progress {
    /// Instructions between two calls. Zero would mean never, so it is raised
    /// to one rather than silently disabling the handler a caller installed.
    pub every: u64,
    /// The callback.
    pub handler: ProgressHandler,
}

impl std::fmt::Debug for Progress {
    /// Reports the interval, since a closure has nothing else to say.
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Progress")
            .field("every", &self.every)
            .finish()
    }
}

impl Machine {
    /// Returns a machine ready to run a program.
    pub fn new(program: Arc<Program>, interrupt: Arc<AtomicBool>, limits: Limits) -> Machine {
        let registers = vec![Value::Null; program.register_count as usize];
        let register_marks = vec![false; program.register_count as usize];
        let mut cursors = Vec::new();
        cursors.resize_with(program.cursor_count as usize, || None);
        let mut virtual_cursors = Vec::new();
        virtual_cursors.resize_with(program.cursor_count as usize, || None);
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
            scratch_values: Vec::new(),
            scratch_marks: Vec::new(),
            register_marks,
            cursors,
            virtual_cursors,
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
            reported_changes: 0,
            reported_total_changes: 0,
            last_insert_rowid: 0,
            conflict: None,
            progress: None,
            functions: None,
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
    /// set before the statement runs. They are kept apart from the statement's
    /// own tallies, which start at zero and are published when it finishes -
    /// seeding those instead would have every statement report the connection's
    /// history as its own row count.
    ///
    /// `last_insert_rowid` is the exception and is seeded into the live value:
    /// SQLite updates it as rows are written rather than at the end, so a
    /// statement that reads it sees its own inserts, and one that writes none
    /// has to see the previous statement's.
    pub fn set_counters(&mut self, changes: i64, total_changes: i64, last_insert_rowid: i64) {
        self.reported_changes = changes;
        self.reported_total_changes = total_changes;
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
        self.register_marks = vec![false; self.program.register_count as usize];
        self.cursors.clear();
        self.cursors
            .resize_with(self.program.cursor_count as usize, || None);
        self.virtual_cursors.clear();
        self.virtual_cursors
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

    /// Names the functions an application registered on the connection.
    pub fn set_functions(&mut self, functions: Option<Arc<dyn ExternalFunctions>>) {
        self.functions = functions;
    }

    /// Installs, or clears, the callback that may stop a long statement.
    pub fn set_progress(&mut self, progress: Option<Progress>) {
        self.progress = progress.map(|progress| Progress {
            every: progress.every.max(1),
            handler: progress.handler,
        });
    }

    /// Asks the progress callback whether to stop, at most once every `every`
    /// instructions.
    ///
    /// It is asked at the same place the interrupt flag is read - an
    /// instruction boundary - because that is where the machine is in a state
    /// anything may be abandoned from. A callback asked in the middle of a
    /// b-tree descent could not be honoured without unwinding a pinned page.
    fn progress_says_stop(&self) -> bool {
        let Some(progress) = self.progress.as_ref() else {
            return false;
        };
        if !self.steps.is_multiple_of(progress.every) {
            return false;
        }
        (progress.handler)()
    }

    /// Runs until the program produces a row or finishes.
    pub fn step(&mut self, host: &mut dyn Host) -> DbResult<StepOutcome> {
        if self.state == MachineState::Failed {
            return Err(error::misuse("this statement has failed and must be reset"));
        }
        if self.state == MachineState::Done {
            return Ok(StepOutcome::Done);
        }
        // The encoding and the file format are the main database's. Every
        // database a connection can reach has to agree with it - an ATTACH of a
        // file with a different text encoding is refused for exactly this
        // reason - so reading them once from `main` is reading them from all of
        // them.
        {
            let main = host.pagers().pager(inillucent_storage::MAIN_DATABASE)?;
            self.encoding = main.text_encoding();
            self.file_format = main.header().schema_format.max(1);
        }
        self.state = MachineState::Running;
        // The program is held behind an `Arc` so the step loop can borrow its
        // instructions while the rest of the machine is borrowed mutably. It
        // used to clone the instruction instead, once per dispatch - and an
        // `Operand` that carries a `Vec` (a text or blob literal, a sort key,
        // an index key, an aggregate call) made that clone a heap allocation
        // and a free on the hottest path there is. Nothing reassigns
        // `self.program` while a statement is running, so this handle is the
        // same program for the whole loop.
        let program = Arc::clone(&self.program);
        loop {
            // The interrupt is checked at instruction boundaries, which are the
            // machine's declared safe points: no page is pinned and no cursor
            // is half-moved between two instructions.
            if self.interrupt.load(AtomicOrdering::Relaxed) || self.progress_says_stop() {
                self.state = MachineState::Failed;
                return Err(inillucent_base::DbError::primary(PrimaryCode::Interrupt));
            }
            let Some(instruction) = program.instruction(self.counter) else {
                self.state = MachineState::Done;
                return Ok(StepOutcome::Done);
            };
            self.steps = self.steps.saturating_add(1);
            #[cfg(feature = "opcode-probe")]
            let probe_started = std::time::Instant::now();
            #[cfg(feature = "opcode-probe")]
            let probe_allocations =
                inillucent_base::probe::ALLOCATIONS.load(core::sync::atomic::Ordering::Relaxed);
            let outcome = if instruction.opcode.is_virtual() {
                self.execute_virtual(instruction, host)
            } else {
                self.execute(instruction, host.pagers())
            };
            #[cfg(feature = "opcode-probe")]
            {
                use core::sync::atomic::Ordering as ProbeOrdering;
                let spent = probe_started.elapsed().as_nanos() as u64;
                let made = inillucent_base::probe::ALLOCATIONS
                    .load(ProbeOrdering::Relaxed)
                    .saturating_sub(probe_allocations);
                let slot = instruction.opcode as usize;
                if let (Some(runs), Some(nanos), Some(allocations)) = (
                    inillucent_base::probe::OPCODE_RUNS.get(slot),
                    inillucent_base::probe::OPCODE_NANOS.get(slot),
                    inillucent_base::probe::OPCODE_ALLOCATIONS.get(slot),
                ) {
                    runs.fetch_add(1, ProbeOrdering::Relaxed);
                    nanos.fetch_add(spent, ProbeOrdering::Relaxed);
                    allocations.fetch_add(made, ProbeOrdering::Relaxed);
                }
            }
            match outcome {
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

    /// Borrows a register, for an opcode that only reads it.
    ///
    /// Cloning a `Value` copies whatever it holds, so a text or blob register
    /// read cost a heap allocation and a copy - and an opcode that reads two
    /// registers paid it twice, per row. The comparison in a `WHERE` clause is
    /// exactly that shape, which is why one filtered scan of fifty thousand
    /// rows was making a hundred thousand allocations nobody needed.
    ///
    /// A register that is not there reads as NULL, the same as the owning form,
    /// because a program the verifier passed cannot name one - and returning an
    /// error from a borrow would put a `Result` on the hottest path in the
    /// machine to describe a state that cannot happen.
    fn register_ref(&self, index: i32) -> &Value<'static> {
        const ABSENT: Value<'static> = Value::Null;
        self.registers.get(index.max(0) as usize).unwrap_or(&ABSENT)
    }

    /// Stores a value into a register, clearing its JSON mark.
    ///
    /// Clearing is the safe default and the common case: every opcode but
    /// the JSON call and the register copy produces a value that is not a
    /// document, and a mark left behind would make the next
    /// `json_object()` embed a string as if it were JSON.
    fn store(&mut self, index: i32, value: Value<'static>) {
        self.store_marked(index, value, false);
    }

    /// Stores a value into a register with an explicit JSON mark.
    fn store_marked(&mut self, index: i32, value: Value<'static>, json: bool) {
        if let Some(slot) = self.registers.get_mut(index.max(0) as usize) {
            *slot = value;
        }
        if let Some(mark) = self.register_marks.get_mut(index.max(0) as usize) {
            *mark = json;
        }
    }

    /// Returns whether a register's value is marked as JSON.
    fn marked(&self, index: i32) -> bool {
        self.register_marks
            .get(index.max(0) as usize)
            .copied()
            .unwrap_or(false)
    }

    /// Returns the JSON marks of a contiguous block of registers.
    fn mark_block(&self, first: i32, count: i32) -> Vec<bool> {
        (0..count.max(0))
            .map(|offset| self.marked(first.saturating_add(offset)))
            .collect()
    }

    /// Fills a caller-owned buffer with a contiguous block of registers.
    ///
    /// The allocating form below is still right for the handful of opcodes that
    /// keep what they collect; this one is for the ones that read it and drop
    /// it, which are the ones that run per row.
    fn block_into(&self, first: i32, count: i32, into: &mut Vec<Value<'static>>) {
        into.clear();
        into.reserve(count.max(0) as usize);
        for offset in 0..count.max(0) {
            into.push(self.register(first.saturating_add(offset)));
        }
    }

    /// Fills a caller-owned buffer with the JSON marks of a block of registers.
    fn mark_block_into(&self, first: i32, count: i32, into: &mut Vec<bool>) {
        into.clear();
        into.reserve(count.max(0) as usize);
        for offset in 0..count.max(0) {
            into.push(self.marked(first.saturating_add(offset)));
        }
    }

    /// Returns a contiguous block of registers.
    fn block(&self, first: i32, count: i32) -> Vec<Value<'static>> {
        (0..count.max(0))
            .map(|offset| self.register(first.saturating_add(offset)))
            .collect()
    }

    /// Runs one instruction.
    /// Runs one instruction against the database it names.
    ///
    /// Which database that is comes from one of two places: a cursor remembers
    /// the one it was opened on, and the handful of opcodes that reach a
    /// database without a cursor carry the number as an operand. Nothing here
    /// assumes `main`, because a statement that writes an attached database
    /// looks exactly like one that writes the main one.
    fn execute(
        &mut self,
        instruction: &Instruction,
        databases: &mut dyn PagerSet,
    ) -> DbResult<Flow> {
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
            Opcode::Rewind | Opcode::Last => {
                let pager = self.pager_for_cursor(instruction.p1, databases)?;
                self.rewind(instruction, pager)
            }
            Opcode::Next | Opcode::Prev => {
                let pager = self.pager_for_cursor(instruction.p1, databases)?;
                self.advance(instruction, pager)
            }
            Opcode::SeekRowid => {
                let pager = self.pager_for_cursor(instruction.p1, databases)?;
                self.seek_rowid(instruction, pager)
            }
            Opcode::SeekGe | Opcode::SeekGt | Opcode::SeekLe | Opcode::SeekLt => {
                let pager = self.pager_for_cursor(instruction.p1, databases)?;
                self.seek(instruction, pager)
            }
            Opcode::IdxGe | Opcode::IdxGt | Opcode::IdxLe | Opcode::IdxLt => {
                let pager = self.pager_for_cursor(instruction.p1, databases)?;
                self.index_bound(instruction, pager)
            }
            Opcode::IdxRowid => {
                let pager = self.pager_for_cursor(instruction.p1, databases)?;
                self.index_rowid(instruction, pager)
            }
            Opcode::Column => {
                let pager = self.pager_for_cursor(instruction.p1, databases)?;
                self.column(instruction, pager)
            }
            Opcode::IdxColumn => {
                let pager = self.pager_for_cursor(instruction.p1, databases)?;
                self.column(instruction, pager)
            }
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
                // A plain copy carries the JSON mark with the value; the two
                // normalising forms produce a counter, which is never JSON.
                // Losing the mark here is how `json_object('a', json('[1]'))`
                // would come to quote its argument: the argument reaches the
                // call through exactly this opcode.
                let (value, json) = match instruction.p5 {
                    1 => (normalise_limit(&value), false),
                    2 => (normalise_offset(&value), false),
                    _ => (value, self.marked(instruction.p1)),
                };
                self.store_marked(instruction.p2, value, json);
                Ok(Flow::Next)
            }
            Opcode::Arithmetic => {
                let Operand::Arithmetic(op) = instruction.p4 else {
                    return Err(error::misuse("Arithmetic without an operator"));
                };
                let value = eval::arithmetic(
                    op,
                    self.register_ref(instruction.p1),
                    self.register_ref(instruction.p2),
                    self.encoding,
                );
                self.store(instruction.p3, value);
                Ok(Flow::Next)
            }
            Opcode::Negate => {
                let value = eval::negate(self.register_ref(instruction.p1));
                self.store(instruction.p2, value);
                Ok(Flow::Next)
            }
            Opcode::BitNot => {
                let value = eval::bit_not(self.register_ref(instruction.p1));
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
                if self.register_ref(instruction.p1).is_null() {
                    return Ok(Flow::Jump(instruction.p2.max(0) as usize));
                }
                Ok(Flow::Next)
            }
            Opcode::IfNotNull => {
                if !self.register_ref(instruction.p1).is_null() {
                    return Ok(Flow::Jump(instruction.p2.max(0) as usize));
                }
                Ok(Flow::Next)
            }
            Opcode::IfPos => {
                let value = cast::integer_value(self.register_ref(instruction.p1));
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
                    changes: self.reported_changes,
                    total_changes: self.reported_total_changes,
                    last_insert_rowid: self.last_insert_rowid,
                    seed: self.entropy,
                    // The bytecode engine has no `case_sensitive_like` of its
                    // own; the new engine reads the pragma and compiles it in.
                    like_case_sensitive: false,
                };
                // A vector measure over a mismatched pair refuses rather than
                // answering NULL, so a ranking query cannot come back ordered
                // by a distance nobody took.
                if let Some(said) = builtin::refusal_for(func, &arguments) {
                    return Err(error::refusal(said));
                }
                let value = builtin::call_with(func, &arguments, collation, self.encoding, context);
                self.store(instruction.p3, value);
                Ok(Flow::Next)
            }
            Opcode::JsonCall => self.json_call(instruction),
            Opcode::Pattern => self.pattern(instruction),
            Opcode::AggStep => self.aggregate_step(instruction),
            Opcode::AggFinal => {
                let slot = instruction.p1.max(0) as usize;
                // An application's aggregate is finished here rather than in
                // the accumulator: only the machine holds the table its name
                // resolves in, and only it can carry the failure back.
                let external = self
                    .accumulators
                    .get(slot)
                    .and_then(|accumulator| accumulator.as_ref())
                    .and_then(|accumulator| {
                        accumulator
                            .external_name()
                            .map(|name| (name.to_vec(), accumulator.group_rows().to_vec()))
                    });
                if let Some((name, rows)) = external {
                    let value = self.reduce_external(&name, &rows)?;
                    self.store(instruction.p2, value);
                    return Ok(Flow::Next);
                }
                let answer = match self
                    .accumulators
                    .get(slot)
                    .and_then(|accumulator| accumulator.as_ref())
                {
                    Some(accumulator) => accumulator.finish()?,
                    None => inillucent_ext::json::Answer {
                        value: Value::Null,
                        json: false,
                    },
                };
                self.store_marked(instruction.p2, answer.value, answer.json);
                Ok(Flow::Next)
            }
            Opcode::AggReset => {
                let Operand::Aggregate(call) = &instruction.p4 else {
                    return Err(error::misuse("AggReset without an aggregate"));
                };
                let call = call.clone();
                if let Some(slot) = self.accumulators.get_mut(instruction.p1.max(0) as usize) {
                    *slot = Some(Accumulator::named(
                        call.func,
                        call.external.clone(),
                        call.distinct,
                        call.collation,
                    ));
                }
                Ok(Flow::Next)
            }
            Opcode::ExtCall => self.external_call(instruction),
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
                    slot.row_is_loaded = false;
                    slot.record_parsed = false;
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
                let failure = inillucent_base::DbError::new(inillucent_base::error::ExtendedCode(
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
                    crate::window::compute(store, &plan, encoding)?;
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
                // Refilled in place rather than replaced, so the row buffer is
                // allocated once per statement instead of once per row.
                let mut row = core::mem::take(&mut self.result);
                self.block_into(instruction.p1, instruction.p2, &mut row);
                self.result = row;
                Ok(Flow::Row)
            }
            Opcode::OpenWrite => self.open_cursor(instruction, false),
            Opcode::OpenWriteIndex => self.open_cursor(instruction, true),
            Opcode::NewRowid => {
                let pager = self.pager_for_cursor(instruction.p1, databases)?;
                self.new_rowid(instruction, pager)
            }
            Opcode::MakeRecord => self.make_record(instruction),
            Opcode::InsertRow => {
                let database = self.cursor_database(instruction.p1)?;
                self.insert_row(instruction, database, databases.pager(database)?)
            }
            Opcode::DeleteRow => {
                let database = self.cursor_database(instruction.p1)?;
                self.delete_row(instruction, database, databases.pager(database)?)
            }
            Opcode::IdxInsert => {
                let database = self.cursor_database(instruction.p1)?;
                self.index_insert(instruction, database, databases.pager(database)?)
            }
            Opcode::IdxDelete => {
                let database = self.cursor_database(instruction.p1)?;
                self.index_delete(instruction, database, databases.pager(database)?)
            }
            Opcode::NotExists => {
                let pager = self.pager_for_cursor(instruction.p1, databases)?;
                self.not_exists(instruction, pager)
            }
            Opcode::NoConflict => {
                let pager = self.pager_for_cursor(instruction.p1, databases)?;
                self.no_conflict(instruction, pager)
            }
            Opcode::RowData => {
                let pager = self.pager_for_cursor(instruction.p1, databases)?;
                self.row_data(instruction, pager)
            }
            Opcode::HaltError => self.halt_error(instruction),
            Opcode::SetCookie => {
                let pager = databases.pager(instruction.p1.max(0) as usize)?;
                self.set_cookie(instruction, pager)
            }
            Opcode::CreateBtree => {
                let pager = databases.pager(instruction.p1.max(0) as usize)?;
                self.create_btree(instruction, pager)
            }
            Opcode::DestroyBtree => {
                let database = instruction.p2.max(0) as usize;
                self.destroy_btree(instruction, database, databases.pager(database)?)
            }
            Opcode::ClearBtree => {
                let database = instruction.p2.max(0) as usize;
                self.clear_btree(instruction, database, databases.pager(database)?)
            }
            Opcode::SeqRowid => {
                let pager = databases.pager(usize::from(instruction.p5))?;
                self.sequence_rowid(instruction, pager)
            }
            Opcode::VOpen
            | Opcode::VFilter
            | Opcode::VNext
            | Opcode::VColumn
            | Opcode::VRowid
            | Opcode::VAux
            | Opcode::VUpdate
            | Opcode::VBegin
            | Opcode::VSync
            | Opcode::VCommit
            | Opcode::VRollback
            | Opcode::VSavepoint => Err(error::misuse(
                "a virtual-table opcode reached the ordinary dispatch",
            )),
            Opcode::SeqUpdate => {
                let pager = databases.pager(usize::from(instruction.p5))?;
                self.sequence_update(instruction, pager)
            }
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

    /// Reads the next rowid an `AUTOINCREMENT` table owes.
    ///
    /// The larger of what `sqlite_sequence` remembers and the largest rowid the
    /// table still holds, plus one. Both halves are needed: the remembered
    /// value is what stops a deleted row's number being reused, and the table's
    /// own maximum is what keeps a row inserted with an explicit rowid from
    /// being collided with before the sequence has caught up.
    fn sequence_rowid(&mut self, instruction: &Instruction, pager: &mut Pager) -> DbResult<Flow> {
        let name = match &instruction.p4 {
            Operand::Text(text) => text.clone(),
            _ => return Err(error::misuse("SeqRowid without a table name")),
        };
        let remembered = self
            .read_sequence(instruction.p2, &name, pager)?
            .unwrap_or(0);
        let largest = self.with_cursor_and_pager(instruction.p1, pager, |slot, pager| {
            if !slot.cursor.last(pager)? {
                return Ok(0);
            }
            slot.moved();
            slot.cursor.rowid()
        })?;
        let highest = remembered.max(largest);
        let Some(next) = highest.checked_add(1) else {
            // SQLite reports SQLITE_FULL for this: the table is not out of
            // space, it is out of *keys*, and there is no larger integer.
            return Err(
                inillucent_base::DbError::primary(inillucent_base::PrimaryCode::Full).with_message(
                    format!(
                        "database or disk is full: {} has no more rowids",
                        String::from_utf8_lossy(&name)
                    ),
                ),
            );
        };
        self.store(instruction.p3, Value::Integer(next));
        Ok(Flow::Next)
    }

    /// Raises what `sqlite_sequence` remembers, if this row went past it.
    fn sequence_update(&mut self, instruction: &Instruction, pager: &mut Pager) -> DbResult<Flow> {
        let name = match &instruction.p4 {
            Operand::Text(text) => text.clone(),
            _ => return Err(error::misuse("SeqUpdate without a table name")),
        };
        let written = cast::integer_value(&self.register(instruction.p1));
        let Some(root) = PageId::new(instruction.p2.max(0) as u32) else {
            return Ok(Flow::Next);
        };
        let existing = self.read_sequence(instruction.p2, &name, pager)?;
        if existing.is_some_and(|seq| seq >= written) {
            return Ok(Flow::Next);
        }
        let encoding = pager.text_encoding();
        let format = pager.header().schema_format.max(1);
        let values = [Value::owned_text(&name)?, Value::Integer(written)];
        let payload = inillucent_value::record::encode_record(&values, encoding, format)?;
        let rowid = match self.find_sequence_rowid(root, &name, pager)? {
            Some(rowid) => rowid,
            None => self.next_sequence_rowid(root, pager)?,
        };
        mutate::insert_row(pager, root, rowid, &payload)?;
        Ok(Flow::Next)
    }

    /// Returns what `sqlite_sequence` remembers for one table, if anything.
    fn read_sequence(
        &mut self,
        root: i32,
        name: &[u8],
        pager: &mut Pager,
    ) -> DbResult<Option<i64>> {
        let Some(root) = PageId::new(root.max(0) as u32) else {
            return Ok(None);
        };
        let limits = self.limits.clone();
        let encoding = pager.text_encoding();
        let mut cursor = inillucent_storage::cursor::BTreeCursor::table(root);
        let mut more = cursor.first(pager)?;
        while more {
            let payload = cursor.payload(pager, &limits)?;
            let record = inillucent_value::record::RecordRef::parse(&payload, encoding)?;
            if let Ok(Value::Text(text)) = record.value(0) {
                if text.utf8_bytes().as_ref() == name {
                    let seq = record
                        .value(1)
                        .map(|value| cast::integer_value(&value))
                        .unwrap_or(0);
                    return Ok(Some(seq));
                }
            }
            more = cursor.next(pager)?;
        }
        Ok(None)
    }

    /// Returns the rowid `sqlite_sequence` holds one table's row under.
    fn find_sequence_rowid(
        &mut self,
        root: PageId,
        name: &[u8],
        pager: &mut Pager,
    ) -> DbResult<Option<i64>> {
        let limits = self.limits.clone();
        let encoding = pager.text_encoding();
        let mut cursor = inillucent_storage::cursor::BTreeCursor::table(root);
        let mut more = cursor.first(pager)?;
        while more {
            let rowid = cursor.rowid()?;
            let payload = cursor.payload(pager, &limits)?;
            let record = inillucent_value::record::RecordRef::parse(&payload, encoding)?;
            if let Ok(Value::Text(text)) = record.value(0) {
                if text.utf8_bytes().as_ref() == name {
                    return Ok(Some(rowid));
                }
            }
            more = cursor.next(pager)?;
        }
        Ok(None)
    }

    /// Returns a rowid no row of `sqlite_sequence` is using.
    fn next_sequence_rowid(&mut self, root: PageId, pager: &mut Pager) -> DbResult<i64> {
        let mut cursor = inillucent_storage::cursor::BTreeCursor::table(root);
        if !cursor.last(pager)? {
            return Ok(1);
        }
        Ok(cursor.rowid()?.saturating_add(1))
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
        Err(inillucent_base::DbError::primary(PrimaryCode::Full)
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
    fn insert_row(
        &mut self,
        instruction: &Instruction,
        database: usize,
        pager: &mut Pager,
    ) -> DbResult<Flow> {
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
        self.invalidate_cursors_on(database, root);
        self.restore_cursors(saved, pager)?;
        Ok(Flow::Next)
    }

    /// Deletes the row a cursor is sitting on.
    fn delete_row(
        &mut self,
        instruction: &Instruction,
        database: usize,
        pager: &mut Pager,
    ) -> DbResult<Flow> {
        let rowid = self.with_cursor(instruction.p1, |slot| slot.cursor.rowid())?;
        let root = self.cursor_root(instruction.p1)?;
        let saved = self.save_cursors_on(root, instruction.p1, pager)?;
        mutate::delete_row(pager, root, rowid)?;
        self.invalidate_cursors_on(database, root);
        self.restore_cursors(saved, pager)?;
        Ok(Flow::Next)
    }

    /// Writes an entry into an index B-tree.
    fn index_insert(
        &mut self,
        instruction: &Instruction,
        database: usize,
        pager: &mut Pager,
    ) -> DbResult<Flow> {
        let payload = self.record_bytes(instruction.p2)?;
        let root = self.cursor_root(instruction.p1)?;
        let key = self.cursor_key(instruction.p1)?;
        let saved = self.save_cursors_on(root, instruction.p1, pager)?;
        mutate::insert_entry(pager, root, &key, &payload)?;
        self.invalidate_cursors_on(database, root);
        self.restore_cursors(saved, pager)?;
        Ok(Flow::Next)
    }

    /// Removes an entry from an index B-tree.
    ///
    /// A missing entry is not an error. An UPDATE that does not change a key
    /// deletes and reinserts the same entry, and a REPLACE may have removed it
    /// already; refusing here would turn a no-op into a failure.
    fn index_delete(
        &mut self,
        instruction: &Instruction,
        database: usize,
        pager: &mut Pager,
    ) -> DbResult<Flow> {
        let payload = self.record_bytes(instruction.p2)?;
        let root = self.cursor_root(instruction.p1)?;
        let key = self.cursor_key(instruction.p1)?;
        let saved = self.save_cursors_on(root, instruction.p1, pager)?;
        mutate::delete_entry(pager, root, &key, &payload)?;
        self.invalidate_cursors_on(database, root);
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
        let code = inillucent_base::error::ExtendedCode(instruction.p1);
        self.conflict = Some(instruction.p3);
        let mut failure = inillucent_base::DbError::new(code);
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
    fn destroy_btree(
        &mut self,
        instruction: &Instruction,
        database: usize,
        pager: &mut Pager,
    ) -> DbResult<Flow> {
        let root = self.root_from_register(instruction.p1)?;
        mutate::drop_tree(pager, root)?;
        self.invalidate_cursors_on(database, root);
        Ok(Flow::Next)
    }

    /// Empties a B-tree, keeping its root page.
    fn clear_btree(
        &mut self,
        instruction: &Instruction,
        database: usize,
        pager: &mut Pager,
    ) -> DbResult<Flow> {
        let root = self.root_from_register(instruction.p1)?;
        mutate::clear_tree(pager, root)?;
        self.invalidate_cursors_on(database, root);
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
    fn invalidate_cursors_on(&mut self, database: usize, root: PageId) {
        for slot in self.cursors.iter_mut().flatten() {
            if slot.database == database && slot.cursor.root() == root {
                slot.row_is_loaded = false;
                slot.record_parsed = false;
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
        let database = self.cursor_database(writer)?;
        let sharing = self
            .cursors
            .iter()
            .enumerate()
            .filter(|(position, slot)| {
                *position != index
                    && slot
                        .as_ref()
                        .is_some_and(|slot| slot.database == database && slot.cursor.root() == root)
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
            if slot.database != database || slot.cursor.root() != root {
                continue;
            }
            saved.push((position, slot.cursor.save_position(pager, &limits)?));
        }
        Ok(saved)
    }

    /// Returns the pager of the database a cursor is open on.
    fn pager_for_cursor<'a>(
        &self,
        cursor: i32,
        databases: &'a mut dyn PagerSet,
    ) -> DbResult<&'a mut Pager> {
        let database = self.cursor_database(cursor)?;
        databases.pager(database)
    }

    /// Returns the database a cursor is open on.
    ///
    /// A cursor that is not open answers `main`, because the callers that ask
    /// are about to fail on the cursor itself and a second error about the
    /// database would say less.
    fn cursor_database(&self, index: i32) -> DbResult<usize> {
        Ok(self
            .cursors
            .get(index.max(0) as usize)
            .and_then(|slot| slot.as_ref())
            .map_or(inillucent_storage::MAIN_DATABASE, |slot| slot.database))
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
            slot.row_is_loaded = false;
            slot.record_parsed = false;
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
                database: instruction.p3.max(0) as usize,
                null_row: false,
                cursor,
                payload: None,
                fields: Vec::new(),
                header_len: 0,
                record_parsed: false,
                row_is_loaded: false,
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
                let candidate = inillucent_value::numeric::real_to_i64(*real);
                if !inillucent_value::numeric::real_same_as_int(*real, candidate) {
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
        let strict = matches!(instruction.opcode, Opcode::SeekGt | Opcode::SeekLt);
        // Which way the walk that follows runs. The two directions are exact
        // mirrors: `Le` lands on the last entry at or before the key and steps
        // back, where `Ge` lands on the first at or after it and steps on.
        let backwards = matches!(instruction.opcode, Opcode::SeekLe | Opcode::SeekLt);
        let bias = if backwards {
            SeekBias::AtOrBefore
        } else {
            SeekBias::AtOrAfter
        };
        let count = i32::from(instruction.p5);
        let key = self.block(instruction.p3, count);
        let Some(Some(slot)) = self.cursors.get_mut(instruction.p1.max(0) as usize) else {
            return Err(error::misuse("cursor is not open"));
        };
        slot.moved();
        if !slot.is_index {
            let rowid = key.first().map(cast::integer_value).unwrap_or(0);
            // `> n` on a table cursor is `>= n + 1`, because a rowid is an
            // integer and there is nothing between n and n + 1. Backwards, the
            // same reasoning gives `< n` as `<= n - 1`.
            let target = match (strict, backwards) {
                (true, false) => rowid.saturating_add(1),
                (true, true) => rowid.saturating_sub(1),
                (false, _) => rowid,
            };
            let found = slot.cursor.seek_rowid(pager, target, bias)?;
            if !found {
                return Ok(Flow::Jump(instruction.p2.max(0) as usize));
            }
            return Ok(Flow::Next);
        }
        let borrowed: Vec<Value<'_>> = key.clone();
        let exact = slot.cursor.seek_index(pager, &borrowed, bias)?;
        if !slot.cursor.is_positioned() {
            return Ok(Flow::Jump(instruction.p2.max(0) as usize));
        }
        if exact && strict {
            // The seek landed on an equal entry and the range excludes it, so
            // walk past every entry equal on the probed prefix - in whichever
            // direction the walk is going to run.
            loop {
                let payload = slot.cursor.payload(pager, &self.limits)?;
                let record = RecordRef::parse_with_limits(&payload, self.encoding, &self.limits)?;
                let ordering = inillucent_value::record::compare_values_to_record(
                    &borrowed, &record, &slot.key,
                )?;
                if ordering != std::cmp::Ordering::Equal {
                    break;
                }
                slot.moved();
                let more = if backwards {
                    slot.cursor.previous(pager)?
                } else {
                    slot.cursor.next(pager)?
                };
                if !more {
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
        let ordering =
            inillucent_value::record::compare_values_to_record(&key, &record, &slot.key)?;
        // `ordering` compares the probe against the entry, so an entry past a
        // *high* bound makes the probe compare Less - and an entry past a *low*
        // one, which is where a backward walk ends, makes it compare Greater.
        let past = match instruction.opcode {
            Opcode::IdxGe => ordering != std::cmp::Ordering::Greater,
            Opcode::IdxGt => ordering == std::cmp::Ordering::Less,
            Opcode::IdxLe => ordering != std::cmp::Ordering::Less,
            _ => ordering == std::cmp::Ordering::Greater,
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
            if !slot.row_is_loaded {
                let mut buffer = slot.payload.take().unwrap_or_default();
                slot.cursor.payload_into(pager, &limits, &mut buffer)?;
                slot.payload = Some(buffer);
                slot.row_is_loaded = true;
            }
            // Borrow the cached row rather than copying it. Cloning here cost a
            // whole-row copy *per column read*, which is three copies of every
            // row of a three-column projection and was the largest single
            // allocation source the baselines found.
            //
            // The record's *shape* is cached beside it for the same reason:
            // finding where a field lives is a walk of the whole header, and
            // doing that once per column read made a two-column projection
            // parse every row twice.
            if !slot.record_parsed {
                slot.fields.clear();
                let header = {
                    let payload: &[u8] = slot.payload.as_deref().unwrap_or(&[]);
                    RecordRef::parse_into(payload, &limits, &mut slot.fields)?
                };
                slot.header_len = header;
                slot.record_parsed = true;
            }
            let payload: &[u8] = slot.payload.as_deref().unwrap_or(&[]);
            let record = RecordRef::with_fields(payload, &slot.fields, slot.header_len, encoding);
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
        let value = {
            let left = self.register_ref(instruction.p1);
            let right = self.register_ref(instruction.p2);
            if instruction.opcode == Opcode::Is {
                eval::is_comparison(
                    comparison.op == BinaryOp::NotEqual,
                    left,
                    right,
                    comparison.affinity,
                    comparison.collation,
                    self.encoding,
                )
            } else {
                eval::comparison(
                    comparison.op,
                    left,
                    right,
                    comparison.affinity,
                    comparison.collation,
                    self.encoding,
                )
            }
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
            inillucent_sql::ast::PatternOp::Glob => inillucent_sql::function::ScalarFunc::Glob,
            _ => inillucent_sql::function::ScalarFunc::Like,
        };
        let pattern_length = arguments.first().map(|value| value.byte_len()).unwrap_or(0);
        if pattern_length as i64
            > self
                .limits
                .get(inillucent_base::limits::Limit::LikePatternLength)
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
        let Operand::Aggregate(call) = &instruction.p4 else {
            return Err(error::misuse("AggStep without an aggregate"));
        };
        let call = call.clone();
        let mut arguments = core::mem::take(&mut self.scratch_values);
        let mut marks = core::mem::take(&mut self.scratch_marks);
        self.block_into(instruction.p1, instruction.p2, &mut arguments);
        self.mark_block_into(instruction.p1, instruction.p2, &mut marks);
        let encoding = self.encoding;
        let outcome = match self.accumulators.get_mut(instruction.p3.max(0) as usize) {
            Some(slot) => {
                let accumulator = slot.get_or_insert_with(|| {
                    Accumulator::new(call.func, call.distinct, call.collation)
                });
                accumulator.step(&arguments, &marks, encoding)
            }
            None => Err(error::misuse("an aggregate slot that does not exist")),
        };
        // Put the buffers back whatever happened, so a statement that fails part
        // way through does not leave the machine allocating again per row.
        self.scratch_values = arguments;
        self.scratch_marks = marks;
        outcome?;
        Ok(Flow::Next)
    }

    /// Calls a function an application registered.
    fn external_call(&mut self, instruction: &Instruction) -> DbResult<Flow> {
        let Operand::Text(name) = &instruction.p4 else {
            return Err(error::misuse("ExtCall without a function name"));
        };
        let name = name.clone();
        let arguments = self.block(instruction.p1, instruction.p2);
        let Some(functions) = self.functions.as_ref() else {
            return Err(error::misuse(format!(
                "no such function: {}",
                String::from_utf8_lossy(&name)
            )));
        };
        let value = functions.call(&name, &arguments)?;
        self.store(instruction.p3, value);
        Ok(Flow::Next)
    }

    /// Finishes a group an application's aggregate collected.
    fn reduce_external(
        &mut self,
        name: &[u8],
        rows: &[Vec<Value<'static>>],
    ) -> DbResult<Value<'static>> {
        let Some(functions) = self.functions.as_ref() else {
            return Err(error::misuse(format!(
                "no such function: {}",
                String::from_utf8_lossy(name)
            )));
        };
        functions.reduce(name, rows)
    }

    /// Calls a JSON built-in.
    ///
    /// It is the one call that reads the JSON mark off its arguments and
    /// writes one onto its answer, and the one that can fail: a document
    /// that will not parse stops the statement rather than answering NULL.
    fn json_call(&mut self, instruction: &Instruction) -> DbResult<Flow> {
        let Operand::Json(func) = instruction.p4 else {
            return Err(error::misuse("JsonCall without a function"));
        };
        let values = self.block(instruction.p1, instruction.p2);
        let marks = self.mark_block(instruction.p1, instruction.p2);
        let arguments: Vec<inillucent_ext::json::Argument<'_>> = values
            .iter()
            .enumerate()
            .map(|(index, value)| inillucent_ext::json::Argument {
                value,
                json: marks.get(index).copied().unwrap_or(false),
            })
            .collect();
        let answer = inillucent_ext::json::call(func, &arguments)?;
        self.store_marked(instruction.p3, answer.value, answer.json);
        Ok(Flow::Next)
    }

    /// Runs one virtual-table instruction against the host.
    ///
    /// Every arm here is a call into somebody else's code, so every arm treats
    /// what comes back as data: a module that answers a column out of range, a
    /// rowid for a row it is not on, or an error mid-scan leaves the statement
    /// resettable rather than the machine confused.
    fn execute_virtual(
        &mut self,
        instruction: &Instruction,
        host: &mut dyn Host,
    ) -> DbResult<Flow> {
        match instruction.opcode {
            Opcode::VOpen => {
                let Operand::Virtual(reference) = &instruction.p4 else {
                    return Err(error::misuse("VOpen without a table"));
                };
                let cursor = host.open_virtual(reference)?;
                let slot = instruction.p1.max(0) as usize;
                let Some(place) = self.virtual_cursors.get_mut(slot) else {
                    return Err(error::misuse("a virtual cursor that does not exist"));
                };
                *place = Some(VirtualSlot {
                    reference: (**reference).clone(),
                    cursor,
                    filtered: false,
                });
                Ok(Flow::Next)
            }
            Opcode::VFilter => {
                let Operand::VirtualPlan(chosen) = &instruction.p4 else {
                    return Err(error::misuse("VFilter without a plan"));
                };
                let arguments = self.block(instruction.p3, i32::from(instruction.p5));
                let plan = inillucent_ext::vtab::FilterPlan {
                    index_number: chosen.index_number,
                    index_string: chosen.index_string.clone(),
                    arguments,
                };
                let empty = self.with_virtual_cursor(instruction.p1, host, |cursor, context| {
                    cursor.filter(context, &plan)?;
                    Ok(cursor.eof())
                })?;
                if let Some(slot) = self
                    .virtual_cursors
                    .get_mut(instruction.p1.max(0) as usize)
                    .and_then(Option::as_mut)
                {
                    slot.filtered = true;
                }
                if empty {
                    return Ok(Flow::Jump(instruction.p2.max(0) as usize));
                }
                Ok(Flow::Next)
            }
            Opcode::VNext => {
                let more = self.with_virtual_cursor(instruction.p1, host, |cursor, context| {
                    cursor.next(context)?;
                    Ok(!cursor.eof())
                })?;
                if more {
                    return Ok(Flow::Jump(instruction.p2.max(0) as usize));
                }
                Ok(Flow::Next)
            }
            Opcode::VColumn => {
                let column = instruction.p2.max(0) as usize;
                let value = self.with_virtual_cursor(instruction.p1, host, |cursor, context| {
                    if cursor.eof() {
                        return Ok(Value::Null);
                    }
                    cursor.column(context, column)
                })?;
                self.store(instruction.p3, value);
                Ok(Flow::Next)
            }
            Opcode::VAux => {
                let Operand::Text(name) = &instruction.p4 else {
                    return Err(error::misuse("VAux with no function name"));
                };
                let name = name.clone();
                let count = usize::from(instruction.p5);
                let mut arguments = Vec::with_capacity(count);
                for offset in 0..count {
                    arguments.push(self.register(instruction.p2 + offset as i32).into_owned()?);
                }
                let value = self.with_virtual_cursor(instruction.p1, host, |cursor, context| {
                    if cursor.eof() {
                        return Ok(Value::Null);
                    }
                    cursor.auxiliary(context, &name, &arguments)
                })?;
                self.store(instruction.p3, value);
                Ok(Flow::Next)
            }
            Opcode::VRowid => {
                let rowid = self.with_virtual_cursor(instruction.p1, host, |cursor, _| {
                    if cursor.eof() {
                        return Ok(None);
                    }
                    cursor.rowid().map(Some)
                })?;
                self.store(instruction.p2, rowid.map_or(Value::Null, Value::Integer));
                Ok(Flow::Next)
            }
            Opcode::VUpdate => {
                let Operand::Virtual(reference) = &instruction.p4 else {
                    return Err(error::misuse("VUpdate without a table"));
                };
                let values = self.block(instruction.p1, instruction.p2);
                let change = change_of(&values)?;
                let rowid = host.with_virtual(reference, &mut |table, context| {
                    table.update(context, &change)
                })?;
                if instruction.p3 >= 0 {
                    self.store(instruction.p3, rowid.map_or(Value::Null, Value::Integer));
                }
                self.changes = self.changes.saturating_add(1);
                if let Some(rowid) = rowid {
                    self.last_insert_rowid = rowid;
                }
                Ok(Flow::Next)
            }
            Opcode::VBegin
            | Opcode::VSync
            | Opcode::VCommit
            | Opcode::VRollback
            | Opcode::VSavepoint => {
                let Operand::Virtual(reference) = &instruction.p4 else {
                    return Err(error::misuse("a transaction opcode without a table"));
                };
                let opcode = instruction.opcode;
                let number = instruction.p1;
                let kind = instruction.p3;
                host.with_virtual(reference, &mut |table, context| {
                    match opcode {
                        Opcode::VBegin => table.begin(context)?,
                        Opcode::VSync => table.sync(context)?,
                        Opcode::VCommit => table.commit(context)?,
                        Opcode::VRollback => table.rollback(context)?,
                        _ => match kind {
                            0 => table.savepoint(context, number)?,
                            1 => table.release(context, number)?,
                            _ => table.rollback_to(context, number)?,
                        },
                    }
                    Ok(None)
                })?;
                Ok(Flow::Next)
            }
            _ => Err(error::misuse("not a virtual-table opcode")),
        }
    }

    /// Runs a body with one open virtual cursor and a context over the pagers.
    ///
    /// The cursor is taken out of its slot for the call and put back
    /// afterwards, error included: the module reads its shadow tables through
    /// the same pagers the machine holds, and the two cannot be borrowed at
    /// once. A cursor lost to an early return would be a cursor the next
    /// instruction could not find.
    fn with_virtual_cursor<T>(
        &mut self,
        slot: i32,
        host: &mut dyn Host,
        body: impl FnOnce(
            &mut dyn inillucent_ext::vtab::VirtualCursor,
            &mut inillucent_ext::vtab::Context<'_>,
        ) -> DbResult<T>,
    ) -> DbResult<T> {
        let index = slot.max(0) as usize;
        let Some(mut taken) = self.virtual_cursors.get_mut(index).and_then(Option::take) else {
            return Err(error::misuse("a virtual cursor that is not open"));
        };
        let limits = self.limits.clone();
        let database = taken.reference.database;
        // **Flattened, not borrowed.** The module contract's context carries
        // the binder's view of the schema, which is what `fts5vocab` reads to
        // name a column; this engine holds the older snapshot shape. Converting
        // here rather than widening the contract keeps one type in the contract
        // and puts the cost on the path that is being retired.
        let schema = host.schema();
        let flattened = schema.as_deref().map(flatten_snapshot);
        let mut services = host.services();
        let outcome = {
            let mut context = inillucent_ext::vtab::Context {
                host: services.as_mut(),
                database,
                limits: &limits,
                catalog: flattened.as_ref(),
            };
            body(taken.cursor.as_mut(), &mut context)
        };
        if let Some(place) = self.virtual_cursors.get_mut(index) {
            *place = Some(taken);
        }
        outcome
    }

    /// Takes a branch on a register's truth value.
    fn branch(&mut self, instruction: &Instruction) -> DbResult<Flow> {
        let truth = eval::truth(self.register_ref(instruction.p1));
        let jump = match instruction.opcode {
            Opcode::If => match truth {
                inillucent_value::compare::Truth::True => true,
                inillucent_value::compare::Truth::Unknown => instruction.p5 == 1,
                inillucent_value::compare::Truth::False => false,
            },
            _ => match truth {
                inillucent_value::compare::Truth::False => true,
                inillucent_value::compare::Truth::Unknown => instruction.p5 == 1,
                inillucent_value::compare::Truth::True => false,
            },
        };
        if jump {
            return Ok(Flow::Jump(instruction.p2.max(0) as usize));
        }
        Ok(Flow::Next)
    }
}

/// Reads a `VUpdate` register block as the change it describes.
///
/// The vector is SQLite's: the old rowid, then - unless this is a delete - the
/// new rowid and one value per declared column. A block of one is a delete, and
/// a NULL old rowid is an insert; everything else is an update. Three
/// operations out of one shape, which is what lets one opcode carry all three.
fn change_of(values: &[Value<'static>]) -> DbResult<inillucent_ext::vtab::Change> {
    let Some(old) = values.first().cloned() else {
        return Err(error::misuse("VUpdate with no arguments"));
    };
    if values.len() == 1 {
        return Ok(inillucent_ext::vtab::Change::Delete(old));
    }
    let new = values.get(1).cloned().unwrap_or(Value::Null);
    let columns = values.get(2..).unwrap_or_default().to_vec();
    if old.is_null() {
        return Ok(inillucent_ext::vtab::Change::Insert {
            rowid: new,
            values: columns,
        });
    }
    Ok(inillucent_ext::vtab::Change::Update {
        old_rowid: old,
        new_rowid: new,
        values: columns,
    })
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

/// Returns the binder's view of a catalog snapshot.
///
/// Every table of every attached database, in attachment order, which is the
/// order an unqualified name resolves in - so a module asking about a name it
/// was given finds the same table the statement that named it would have.
///
/// @param snapshot - the schema as this engine holds it
fn flatten_snapshot(
    snapshot: &inillucent_catalog::snapshot::CatalogSnapshot,
) -> inillucent_sql::catalog_view::StaticCatalog {
    let mut catalog = inillucent_sql::catalog_view::StaticCatalog::empty();
    // The database list carries the *numbering*, which is what a pragma
    // function's `schema` argument resolves to - so it has to be filled even
    // when nothing reads a table.
    catalog.databases = snapshot
        .databases
        .iter()
        .map(|database| (database.name.clone(), database.schema_cookie))
        .collect();
    for database in &snapshot.databases {
        for table in &database.tables {
            catalog = catalog.with_table(table.clone());
        }
    }
    catalog
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
