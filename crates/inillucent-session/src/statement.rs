//! Prepare, bind, step, reset and finalise.
//!
//! Invariant: a prepared statement records the catalog generation and every
//! schema cookie it was compiled against, and checks them before its first
//! step. A statement whose schema moved is recompiled from the SQL it kept -
//! which is `prepare_v2` behaviour - and one that cannot be recompiled reports
//! the failure rather than running against a shape that has changed.
//!
//! The second invariant is that a statement owns a transaction level for
//! exactly as long as it is running. It opens one before its first step and
//! closes it when it finishes, fails, is reset, or is dropped - so a write that
//! stops half-way through a RETURNING loop is undone by the level it opened
//! rather than by whatever the next statement happens to do.
//!
//! Prepare is the whole front end in one function: lex, parse, bind against the
//! catalog snapshot, plan, compile, and verify. Nothing between those steps
//! touches the file, so a statement that cannot be prepared costs no I/O.

use std::sync::Arc;

use inillucent_base::{error, DbResult};
use inillucent_sql::bind::{AllowAll, Authorizer, Binder, BoundStatement};
use inillucent_sql::directive::Directive;
use inillucent_sql::parser::parse_next_statement;
use inillucent_value::Value;
use inillucent_vm::compile_dml::{CONFLICT_FAIL, CONFLICT_ROLLBACK};
use inillucent_vm::machine::{Machine, MachineState, StepOutcome};
use inillucent_vm::program::{Operand, Program, ProgramDependencies};
use inillucent_vm::{compile, compile_dml, verify, verify_operands};

use crate::connection::{Access, Connection, Outcome};
use crate::execute;

/// What a statement reports about one of its result columns.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ColumnMetadata {
    /// The name the column reports.
    pub name: Vec<u8>,
    /// The database, table and column it came from, when it came from one.
    pub origin: Option<(Vec<u8>, Vec<u8>, Vec<u8>)>,
    /// The declared type it reports.
    pub declared_type: Vec<u8>,
}

/// What a prepared statement turned out to be.
enum Body {
    /// A compiled program the machine runs.
    Program,
    /// A statement the session carries out itself.
    Directive(Box<Directive>),
}

/// A prepared statement.
pub struct Statement<'connection> {
    connection: &'connection Connection,
    program: Arc<Program>,
    machine: Machine,
    body: Body,
    sql: Vec<u8>,
    columns: Vec<ColumnMetadata>,
    used: usize,
    parameters: Vec<(Vec<u8>, u32)>,
    rows: Vec<Vec<Value<'static>>>,
    row: usize,
    current: Vec<Value<'static>>,
    access: Access,
    open: bool,
    finished: bool,
}

impl<'connection> Statement<'connection> {
    /// Prepares the first statement in `sql`, returning it and the tail.
    ///
    /// The tail is the byte offset the next statement starts at, which is
    /// SQLite's prepare contract: one call compiles one statement and the
    /// caller advances.
    pub fn prepare(
        connection: &'connection Connection,
        sql: &[u8],
    ) -> DbResult<(Statement<'connection>, usize)> {
        Statement::prepare_with(connection, sql, &AllowAll)
    }

    /// Prepares a statement with an authorizer.
    pub fn prepare_with(
        connection: &'connection Connection,
        sql: &[u8],
        authorizer: &dyn Authorizer,
    ) -> DbResult<(Statement<'connection>, usize)> {
        let compiled = compile_sql(connection, sql, authorizer)?;
        // The consumed length is read off before the statement takes ownership,
        // because cloning a whole compiled program to keep one integer is a
        // deep copy of every instruction it holds.
        let consumed = compiled.consumed;
        let statement = Statement::from_compiled(connection, compiled);
        Ok((statement, consumed))
    }

    /// Builds a statement around a compiled program or directive.
    fn from_compiled(
        connection: &'connection Connection,
        compiled: Compiled,
    ) -> Statement<'connection> {
        let columns = compiled
            .program
            .result_columns
            .iter()
            .map(|column| ColumnMetadata {
                name: column.name.clone(),
                origin: column.origin.clone(),
                declared_type: column.declared_type.clone(),
            })
            .collect();
        let mut machine = Machine::new(
            compiled.program.clone(),
            connection.interrupt_flag(),
            connection.limits().clone(),
        );
        machine.set_progress(connection.progress_handler());
        machine.set_functions(Some(connection.function_table()));
        let access = if compiled.program.readonly {
            Access::Read
        } else {
            Access::Write
        };
        Statement {
            connection,
            program: compiled.program,
            machine,
            body: compiled.body,
            sql: compiled.sql,
            columns,
            used: compiled.used,
            parameters: compiled.parameters,
            rows: Vec::new(),
            row: 0,
            current: Vec::new(),
            access,
            open: false,
            finished: false,
        }
    }

    /// Returns the statement's result columns.
    pub fn columns(&self) -> &[ColumnMetadata] {
        &self.columns
    }

    /// Returns how many bytes of the prepared text this statement occupied.
    ///
    /// It is where the *next* statement starts as a C caller counts: the
    /// semicolon belongs to this statement and the whitespace after it does
    /// not.
    pub fn sql_used(&self) -> usize {
        self.used
    }

    /// Returns the highest parameter index the statement uses.
    ///
    /// Named and numbered parameters share one space, so this is the count a
    /// caller binds against whichever way the statement was written.
    pub fn parameter_count(&self) -> u32 {
        self.program.parameter_count
    }

    /// Returns each named parameter and the index it was assigned.
    ///
    /// The name includes its prefix - `:id`, `@id`, `$id` - because that is
    /// what the statement wrote and what a caller looking one up will pass.
    pub fn parameter_names(&self) -> &[(Vec<u8>, u32)] {
        &self.parameters
    }

    /// Returns how many columns the statement returns.
    pub fn column_count(&self) -> usize {
        self.columns.len()
    }

    /// Returns the compiled program, for `EXPLAIN` and for tests.
    pub fn program(&self) -> &Program {
        &self.program
    }

    /// Returns the SQL the statement was prepared from.
    pub fn sql(&self) -> &[u8] {
        &self.sql
    }

    /// Returns whether the statement writes.
    pub fn is_readonly(&self) -> bool {
        self.program.readonly && matches!(self.body, Body::Program)
    }

    /// Binds a value to a one-based parameter index.
    pub fn bind(&mut self, index: u32, value: Value<'static>) -> DbResult<()> {
        self.machine.bind(index, value)
    }

    /// Clears every binding back to NULL.
    pub fn clear_bindings(&mut self) {
        self.machine.clear_bindings();
    }

    /// Steps the statement, returning whether a row is available.
    pub fn step(&mut self) -> DbResult<bool> {
        if self.finished {
            return Ok(false);
        }
        match &self.body {
            Body::Program => self.step_program(),
            Body::Directive(_) => self.step_directive(),
        }
    }

    /// Steps a compiled program.
    fn step_program(&mut self) -> DbResult<bool> {
        if !self.open {
            if self.machine.state() == MachineState::Prepared {
                self.check_schema()?;
            }
            self.connection
                .begin_statement_on(self.access, &databases_written(self.machine.program()))?;
            self.machine
                .record_row_changes(self.connection.wants_row_changes());
            // What `changes()`, `total_changes()` and `last_insert_rowid()`
            // answer is the connection's history, and the machine has no way to
            // ask for it. Nothing was telling it, so all three read zero from
            // inside a statement however many rows the connection had written.
            let counters = self.connection.counters();
            self.machine.set_counters(
                counters.changes,
                counters.total_changes,
                counters.last_insert_rowid,
            );
            self.open = true;
        }
        let outcome = self
            .connection
            .with_state(|state| self.machine.step(state))?;
        // The hook fires for the rows this step changed, in the order they
        // changed, and before the statement's own result reaches the caller.
        // It runs outside the pager borrow, so a hook may read the connection.
        for change in self.machine.take_row_changes() {
            self.connection.fire_update_hook(&change);
        }
        match outcome {
            Ok(StepOutcome::Row) => {
                self.current = self.machine.row().to_vec();
                Ok(true)
            }
            Ok(StepOutcome::Done) => {
                self.publish_counters();
                self.publish_insert_rowid();
                self.close(Outcome::Done)?;
                Ok(false)
            }
            Err(failure) => {
                let ending = ending_for(&failure, self.machine.conflict_action());
                if ending == Outcome::Fail {
                    self.publish_counters();
                }
                self.publish_insert_rowid();
                let closed = self.close(ending);
                closed?;
                Err(failure)
            }
        }
    }

    /// Runs a directive, which produces all of its rows at once.
    fn step_directive(&mut self) -> DbResult<bool> {
        if !self.open {
            self.check_schema()?;
            let Body::Directive(directive) = &self.body else {
                return Err(error::misuse("not a directive"));
            };
            let directive = directive.clone();
            let sql = self.sql.clone();
            self.rows = execute::run_directive(self.connection, &directive, &sql)?;
            self.row = 0;
            self.open = true;
        }
        match self.rows.get(self.row) {
            Some(row) => {
                self.current = row.clone();
                self.row = self.row.saturating_add(1);
                Ok(true)
            }
            None => {
                self.finished = true;
                self.open = false;
                Ok(false)
            }
        }
    }

    /// Folds the machine's row counts into the connection's.
    ///
    /// Only for a statement that finished or failed: an aborted one's rows are
    /// undone, and the transaction layer zeroes `changes` when it rolls the
    /// statement level back.
    fn publish_counters(&mut self) {
        let changes = self.machine.changes();
        let trigger_changes = self.machine.trigger_changes();
        let _ = self.connection.with_state(|state| {
            for _ in 0..changes {
                state.transaction.record_change();
            }
            for _ in 0..trigger_changes {
                state.transaction.record_trigger_change();
            }
        });
    }

    /// Publishes the rowid the statement's last insert allocated.
    ///
    /// Separate from the row counts, and done on *every* ending, because the
    /// two are restored differently. Measured against 3.53.4: an
    /// `INSERT INTO t VALUES(3, 7), (4, -7)` whose second row trips a
    /// `RAISE(ABORT)` reports `changes` of 0 - the statement was undone - but
    /// `last_insert_rowid()` of 3, the row that was written before the abort.
    /// Publishing this alongside the counts left it reading the value from
    /// whichever earlier statement last succeeded.
    fn publish_insert_rowid(&mut self) {
        let rowid = self.machine.last_insert_rowid();
        if rowid == 0 {
            return;
        }
        let _ = self
            .connection
            .with_state(|state| state.transaction.record_insert_rowid(rowid));
    }

    /// Closes the statement's transaction level.
    fn close(&mut self, outcome: Outcome) -> DbResult<()> {
        if !self.open {
            self.finished = true;
            return Ok(());
        }
        self.open = false;
        self.finished = true;
        self.connection.end_statement(self.access, outcome)
    }

    /// Returns the current row.
    pub fn row(&self) -> &[Value<'static>] {
        &self.current
    }

    /// Returns one column of the current row.
    pub fn value(&self, index: usize) -> Value<'static> {
        self.current.get(index).cloned().unwrap_or(Value::Null)
    }

    /// Returns how many instructions the statement has run.
    pub fn steps(&self) -> u64 {
        self.machine.steps()
    }

    /// Resets the statement so it can run again, keeping its bindings.
    ///
    /// A statement that is reset part-way through has not finished, so what it
    /// wrote is undone: SQLite treats an abandoned statement as an aborted one,
    /// and keeping half a statement's rows would be the one outcome no
    /// conflict algorithm allows.
    pub fn reset(&mut self) -> DbResult<()> {
        let closed = if self.open {
            self.close(Outcome::Abort)
        } else {
            Ok(())
        };
        self.machine.reset();
        self.rows.clear();
        self.row = 0;
        self.current.clear();
        self.finished = false;
        self.open = false;
        closed
    }

    /// Finalises the statement, releasing everything it holds.
    pub fn finalize(mut self) -> DbResult<()> {
        let closed = if self.open {
            self.close(Outcome::Abort)
        } else {
            Ok(())
        };
        self.machine.reset();
        self.finished = true;
        closed
    }

    /// Recompiles the statement when the schema has moved under it.
    ///
    /// This is `prepare_v2`'s behaviour: the SQL is kept precisely so that a
    /// schema change is invisible to a caller that did nothing wrong. A legacy
    /// prepare would return `SQLITE_SCHEMA` here instead.
    fn check_schema(&mut self) -> DbResult<()> {
        let catalog = self.connection.catalog()?;
        let stale = self.program.dependencies.generation != catalog.generation
            || self
                .program
                .dependencies
                .schemas
                .iter()
                .any(|(database, cookie)| {
                    catalog
                        .databases
                        .get(*database)
                        .is_none_or(|database| database.schema_cookie != *cookie)
                });
        if !stale {
            return Ok(());
        }
        let sql = self.sql.clone();
        let compiled = compile_sql(self.connection, &sql, &AllowAll)?;
        self.columns = compiled
            .program
            .result_columns
            .iter()
            .map(|column| ColumnMetadata {
                name: column.name.clone(),
                origin: column.origin.clone(),
                declared_type: column.declared_type.clone(),
            })
            .collect();
        self.machine = Machine::new(
            compiled.program.clone(),
            self.connection.interrupt_flag(),
            self.connection.limits().clone(),
        );
        self.machine
            .set_progress(self.connection.progress_handler());
        self.access = if compiled.program.readonly {
            Access::Read
        } else {
            Access::Write
        };
        self.program = compiled.program;
        self.body = compiled.body;
        Ok(())
    }
}

impl Drop for Statement<'_> {
    /// Undoes a statement that was dropped part-way through.
    fn drop(&mut self) {
        if self.open {
            self.open = false;
            let _ = self.connection.end_statement(self.access, Outcome::Abort);
        }
    }
}

/// Returns what a failing statement's level does.
///
/// The conflict algorithm the failing constraint carried decides it: ABORT
/// undoes the statement, FAIL keeps the rows it had already written, and
/// ROLLBACK undoes the whole transaction. Any other failure - an I/O error, a
/// corrupt page, an interrupt - is an abort, because a statement that could
/// not finish must not leave half its rows behind.
fn ending_for(failure: &inillucent_base::DbError, conflict: Option<i32>) -> Outcome {
    if failure.transaction_rolled_back() {
        return Outcome::Rollback;
    }
    match conflict {
        Some(CONFLICT_ROLLBACK) => Outcome::Rollback,
        Some(CONFLICT_FAIL) => Outcome::Fail,
        _ => Outcome::Abort,
    }
}

/// Compiles an `EXPLAIN` or `EXPLAIN QUERY PLAN`.
///
/// The inner statement is bound and compiled exactly as it would have been -
/// so an `EXPLAIN` of a statement that does not compile fails with that
/// statement's error - and then its program or its plan is rendered as rows.
/// Nothing about the inner statement runs.
fn explain(
    connection: &Connection,
    binder: &mut Binder<'_>,
    query_plan: bool,
    inner: &inillucent_sql::ast::Statement,
    sql: &[u8],
    parsed: &inillucent_sql::parser::ParsedStatement,
    authorizer: &dyn Authorizer,
) -> DbResult<Compiled> {
    let _ = authorizer;
    let bound = binder.bind_statement(inner)?;
    let dependencies = ProgramDependencies {
        schemas: binder.dependencies().schemas.clone(),
        generation: binder.dependencies().generation,
        levers: connection.disabled_optimizations(),
    };
    let parameters = parsed.parameters.count;
    let statement_sql = parsed.span.slice(sql).to_vec();
    let consumed = parsed.span.end as usize;

    let (program, plan) = match bound {
        BoundStatement::Select(select) => {
            let (program, plan) = compile::compile_select_with(
                *select,
                dependencies.clone(),
                parameters,
                Some(virtual_planner(connection)?),
            )?;
            (program, Some(plan))
        }
        BoundStatement::Insert(insert) => (
            compile_dml::compile_insert_with(
                &insert,
                dependencies.clone(),
                parameters,
                Some(virtual_planner(connection)?),
            )?,
            None,
        ),
        BoundStatement::Update(update) => (
            compile_dml::compile_update_with(
                &update,
                dependencies.clone(),
                parameters,
                Some(virtual_planner(connection)?),
            )?,
            None,
        ),
        BoundStatement::Delete(delete) => (
            compile_dml::compile_delete_with(
                &delete,
                dependencies.clone(),
                parameters,
                Some(virtual_planner(connection)?),
            )?,
            None,
        ),
        BoundStatement::Directive(directive) => {
            (directive_program(dependencies.clone(), &directive), None)
        }
        BoundStatement::Empty => (empty_program(dependencies.clone()), None),
    };

    let program = if query_plan {
        let lines = plan.map(|plan| plan.describe()).unwrap_or_default();
        let rows: Vec<Vec<Operand>> = lines
            .iter()
            .enumerate()
            .map(|(index, detail)| {
                vec![
                    Operand::Integer(index.saturating_add(1) as i64),
                    Operand::Integer(0),
                    Operand::Integer(0),
                    Operand::Text(detail.as_bytes().to_vec()),
                ]
            })
            .collect();
        compile::compile_rows(&["id", "parent", "notused", "detail"], &rows, dependencies)?
    } else {
        let rows: Vec<Vec<Operand>> = program
            .instructions
            .iter()
            .enumerate()
            .map(|(address, instruction)| {
                vec![
                    Operand::Integer(address as i64),
                    Operand::Text(instruction.opcode.name().as_bytes().to_vec()),
                    Operand::Integer(i64::from(instruction.p1)),
                    Operand::Integer(i64::from(instruction.p2)),
                    Operand::Integer(i64::from(instruction.p3)),
                    Operand::Text(render_operand(&instruction.p4)),
                    Operand::Integer(i64::from(instruction.p5)),
                    Operand::Text(Vec::new()),
                ]
            })
            .collect();
        compile::compile_rows(
            &["addr", "opcode", "p1", "p2", "p3", "p4", "p5", "comment"],
            &rows,
            dependencies,
        )?
    };
    Ok(Compiled {
        program: Arc::new(program),
        body: Body::Program,
        sql: statement_sql,
        consumed,
        used: statement_extent(sql, parsed.span.end as usize, consumed),
        parameters: parsed.parameters.names.clone(),
    })
}

/// Renders an instruction's typed operand for the `p4` column.
fn render_operand(operand: &Operand) -> Vec<u8> {
    match operand {
        Operand::None => Vec::new(),
        Operand::Text(text) => text.clone(),
        Operand::Integer(value) => value.to_string().into_bytes(),
        Operand::Real(value) => value.to_string().into_bytes(),
        Operand::Count(value) => value.to_string().into_bytes(),
        other => format!("{other:?}").into_bytes(),
    }
}

/// One compiled statement.
///
/// Named `CompiledPlan` where it crosses to the connection, because a cache
/// entry and a statement's own program are the same thing and the connection
/// should not have to know the private name.
pub(crate) type CompiledPlan = Compiled;

/// One compiled statement.
#[derive(Clone)]
pub(crate) struct Compiled {
    program: Arc<Program>,
    body: Body,
    sql: Vec<u8>,
    consumed: usize,
    /// How many input bytes the statement occupies, its semicolon included.
    used: usize,
    /// Named parameters and the index each was assigned.
    parameters: Vec<(Vec<u8>, u32)>,
}

/// What a cached program was compiled against.
///
/// Everything `compile_sql` reads that could change the program it produces is
/// in here, so an entry whose key matches was compiled from the same inputs and
/// is therefore the same program. The two inputs that are *not* in the key -
/// the registered functions and the collations - invalidate the cache outright
/// when they change, because they are rare and comparing them per prepare would
/// cost more than the cache saves.
#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub(crate) struct PlanKey {
    /// The SQL as handed to prepare, byte for byte.
    sql: Vec<u8>,
    /// The catalog generation, which changes on every schema change.
    generation: u64,
    /// The planner optimizations switched off.
    levers: u32,
    /// Whether foreign keys are enforced.
    foreign_keys: bool,
    /// Whether foreign key checks are deferred.
    defer_foreign_keys: bool,
}

/// Compiled programs kept for reuse, most recently used first.
///
/// The new engine's rearchitecture plan puts a plan cache in its own prepare
/// path, proved on this existing one first. This is that: a bounded,
/// invalidating, exactly-keyed cache of `Compiled`,
/// whose value is an `Arc<Program>` and a little metadata, so a hit is a
/// refcount bump rather than a parse, a bind, a plan and a compile.
///
/// It is a *cache*, not a memo table: it may return nothing at any time and the
/// caller compiles. Nothing depends on a hit.
#[derive(Default)]
pub(crate) struct PlanCache {
    entries: Vec<(PlanKey, Compiled)>,
}

/// How many statements one connection keeps.
///
/// The TDD's number. Two hundred and fifty-six compiled programs of a few
/// hundred instructions each is well under a megabyte, and an application with
/// more distinct statements than that in flight is not the case the cache is
/// for.
const PLAN_CACHE_ENTRIES: usize = 256;

impl PlanCache {
    /// Returns the program compiled for a key, if it is still held.
    ///
    /// A hit moves the entry to the front, which is the whole of the eviction
    /// policy: least recently used falls off the end.
    ///
    /// @param key - what the caller is about to compile
    pub(crate) fn get(&mut self, key: &PlanKey) -> Option<Compiled> {
        let at = self.entries.iter().position(|(held, _)| held == key)?;
        let entry = self.entries.remove(at);
        let compiled = entry.1.clone();
        self.entries.insert(0, entry);
        Some(compiled)
    }

    /// Keeps a compiled program under a key.
    ///
    /// @param key - what it was compiled against
    /// @param compiled - the program
    pub(crate) fn put(&mut self, key: PlanKey, compiled: Compiled) {
        self.entries.retain(|(held, _)| *held != key);
        self.entries.insert(0, (key, compiled));
        self.entries.truncate(PLAN_CACHE_ENTRIES);
    }

    /// Drops everything.
    ///
    /// Called when a registered function or a collation changes: both can
    /// change what a statement binds to, and neither is in the key.
    pub(crate) fn clear(&mut self) {
        self.entries.clear();
    }

    /// Returns how many programs are held, for tests and for reporting.
    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }
}

/// Returns how many bytes of `sql` one statement occupies.
///
/// The span the parser reports stops before the semicolon; this walks past any
/// whitespace after it and takes the semicolon too, which is where a C caller's
/// `pzTail` is left and therefore where the statement text has to end.
fn statement_extent(sql: &[u8], span_end: usize, consumed: usize) -> usize {
    let mut at = span_end.min(sql.len());
    while at < consumed.min(sql.len()) {
        match sql.get(at) {
            Some(byte) if byte.is_ascii_whitespace() => at = at.saturating_add(1),
            Some(b';') => return at.saturating_add(1),
            _ => return span_end.min(sql.len()),
        }
    }
    span_end.min(sql.len())
}

impl Clone for Body {
    /// Clones a body, which a recompile needs.
    fn clone(&self) -> Body {
        match self {
            Body::Program => Body::Program,
            Body::Directive(directive) => Body::Directive(directive.clone()),
        }
    }
}

/// Returns the databases a program opens a write cursor on.
///
/// A statement takes a writer on the databases it writes and no others: a
/// RESERVED lock on a file nobody is changing is a lock somebody else is
/// waiting for. The program is where the answer is, because every write cursor
/// it opens names its database - which is also what the machine reads when it
/// runs the instruction.
fn databases_written(program: &inillucent_vm::program::Program) -> Vec<usize> {
    let mut databases = Vec::new();
    let mut index = 0;
    while let Some(instruction) = program.instruction(index) {
        index = index.saturating_add(1);
        if !matches!(
            instruction.opcode,
            inillucent_vm::program::Opcode::OpenWrite
                | inillucent_vm::program::Opcode::OpenWriteIndex
        ) {
            continue;
        }
        let database = instruction.p3.max(0) as usize;
        if !databases.contains(&database) {
            databases.push(database);
        }
    }
    if databases.is_empty() {
        databases.push(inillucent_storage::MAIN_DATABASE);
    }
    databases
}

/// Records one bracketed stage of `compile_sql`, in a profiling build.
///
/// A no-op unless the `opcode-probe` feature is on, which it never is in a
/// shipped build: it reads a clock and an allocation counter twice per stage.
#[cfg(feature = "opcode-probe")]
macro_rules! stage {
    ($slot:expr, $body:expr) => {{
        let started = std::time::Instant::now();
        let allocated =
            inillucent_base::probe::ALLOCATIONS.load(core::sync::atomic::Ordering::Relaxed);
        let outcome = $body;
        inillucent_base::probe::record_stage_allocating(
            $slot,
            started.elapsed().as_nanos() as u64,
            inillucent_base::probe::ALLOCATIONS
                .load(core::sync::atomic::Ordering::Relaxed)
                .saturating_sub(allocated),
        );
        outcome
    }};
}

#[cfg(not(feature = "opcode-probe"))]
macro_rules! stage {
    ($slot:expr, $body:expr) => {
        $body
    };
}

fn compile_sql(
    connection: &Connection,
    sql: &[u8],
    authorizer: &dyn Authorizer,
) -> DbResult<Compiled> {
    // The cache is consulted only where reusing a program cannot change what
    // the caller sees. That means the lever is on, and the authorizer allows
    // everything - an authorizer that can refuse has to be *asked*, and a hit
    // would not ask it.
    let cacheable = connection.disabled_optimizations() & inillucent_sql::plan::Levers::PLAN_CACHE
        == 0
        && authorizer.allows_everything();
    let key = if cacheable {
        let generation = connection.catalog()?.generation;
        let key = PlanKey {
            sql: sql.to_vec(),
            generation,
            levers: connection.disabled_optimizations(),
            foreign_keys: connection.foreign_keys(),
            defer_foreign_keys: connection.defer_foreign_keys(),
        };
        if let Some(hit) = connection.cached_plan(&key) {
            return Ok(hit);
        }
        Some(key)
    } else {
        None
    };
    let compiled = compile_sql_uncached(connection, sql, authorizer)?;
    if let Some(key) = key {
        connection.cache_plan(key, compiled.clone());
    }
    Ok(compiled)
}

/// Parses, binds, plans and compiles one statement, with no cache.
///
/// @param connection - the connection whose catalog and settings apply
/// @param sql - the statement text, which may hold more than one statement
/// @param authorizer - what the binder asks about each action
fn compile_sql_uncached(
    connection: &Connection,
    sql: &[u8],
    authorizer: &dyn Authorizer,
) -> DbResult<Compiled> {
    let limits = connection.limits().clone();
    let parsed = stage!(10, parse_next_statement(sql, 0, &limits))?;
    let catalog = stage!(11, connection.catalog())?;
    let functions = stage!(12, connection.external_functions());
    let collations = stage!(12, connection.collations());
    let mut binder = Binder::new(catalog.as_ref(), &parsed.ast, authorizer)
        .with_source(sql)
        .with_functions(&functions)
        .with_collations(&collations)
        .with_foreign_keys(connection.foreign_keys(), connection.defer_foreign_keys());
    if let inillucent_sql::ast::Statement::Explain { query_plan, inner } = &parsed.statement {
        return explain(
            connection,
            &mut binder,
            *query_plan,
            inner,
            sql,
            &parsed,
            authorizer,
        );
    }
    let bound = stage!(13, binder.bind_statement(&parsed.statement))?;
    let dependencies = ProgramDependencies {
        schemas: binder.dependencies().schemas.clone(),
        generation: binder.dependencies().generation,
        levers: connection.disabled_optimizations(),
    };
    let statement_sql = parsed.span.slice(sql).to_vec();
    let parameters = parsed.parameters.count;
    let (program, body) = stage!(
        14,
        match bound {
            BoundStatement::Select(select) => {
                let (program, _) = compile::compile_select_with(
                    *select,
                    dependencies,
                    parameters,
                    Some(virtual_planner(connection)?),
                )?;
                (program, Body::Program)
            }
            BoundStatement::Insert(insert) => (
                compile_dml::compile_insert_with(
                    &insert,
                    dependencies,
                    parameters,
                    Some(virtual_planner(connection)?),
                )?,
                Body::Program,
            ),
            BoundStatement::Update(update) => (
                compile_dml::compile_update_with(
                    &update,
                    dependencies,
                    parameters,
                    Some(virtual_planner(connection)?),
                )?,
                Body::Program,
            ),
            BoundStatement::Delete(delete) => (
                compile_dml::compile_delete_with(
                    &delete,
                    dependencies,
                    parameters,
                    Some(virtual_planner(connection)?),
                )?,
                Body::Program,
            ),
            BoundStatement::Directive(directive) => {
                let program = directive_program(dependencies, &directive);
                (program, Body::Directive(directive))
            }
            BoundStatement::Empty => (empty_program(dependencies), Body::Program),
        }
    );
    // The peephole pass runs before the verifier, never after it. A rewrite of
    // the compiler's output is exactly the kind of code that is right until one
    // opcode nobody thought about, so the program it produces is proved the
    // same way the compiler's own output is, against the same rules.
    let mut program = program;
    if inillucent_sql::plan::Levers::without(connection.disabled_optimizations())
        .has(inillucent_sql::plan::Levers::FUSED_BYTECODE)
        && inillucent_vm::fuse::fold_scratch_copies(&mut program) > 0
    {
        program.optimizations_used |= inillucent_sql::plan::Levers::FUSED_BYTECODE;
    }
    let problems = stage!(15, verify(&program));
    if !problems.is_empty() {
        return Err(error::misuse(format!(
            "the compiler produced a program the verifier rejected: {problems:?}"
        )));
    }
    let operands = verify_operands(&program);
    if !operands.is_empty() {
        return Err(error::misuse(format!(
            "the compiler produced a program with bad operands: {operands:?}"
        )));
    }
    Ok(Compiled {
        program: Arc::new(program),
        body,
        sql: statement_sql,
        consumed: parsed.consumed,
        used: statement_extent(sql, parsed.span.end as usize, parsed.consumed),
        parameters: parsed.parameters.names.clone(),
    })
}

/// Returns a planner over one connection's modules and connected tables.
fn virtual_planner(
    connection: &Connection,
) -> DbResult<Box<dyn inillucent_vm::compile::VirtualPlanner>> {
    let (registry, tables) = connection.with_state(|state| {
        (
            std::sync::Arc::clone(&state.registry),
            std::rc::Rc::clone(&state.virtual_tables),
        )
    })?;
    Ok(Box::new(crate::vtab::SessionPlanner::new(registry, tables)))
}

/// Returns the placeholder program a directive carries.
///
/// A directive runs no bytecode, but it still has result columns - a `PRAGMA`
/// answers with one - and the rest of the statement surface reads them off the
/// program. Giving it an empty program keeps one path rather than two.
fn directive_program(dependencies: ProgramDependencies, directive: &Directive) -> Program {
    let mut program = empty_program(dependencies);
    program.readonly = matches!(
        directive,
        Directive::Begin(_)
            | Directive::Commit
            | Directive::Rollback { .. }
            | Directive::Savepoint(_)
            | Directive::Release(_)
            | Directive::Pragma { .. }
    );
    if let Directive::Pragma { name, .. } = directive {
        program.result_columns = crate::pragma::columns(name)
            .into_iter()
            .map(|name| inillucent_vm::program::ResultColumn {
                name,
                origin: None,
                declared_type: Vec::new(),
            })
            .collect();
    }
    program
}

/// Returns the program an empty statement compiles to.
fn empty_program(dependencies: ProgramDependencies) -> Program {
    use inillucent_vm::program::{Instruction, Opcode};
    Program {
        optimizations_used: 0,
        ephemeral_count: 0,
        instructions: vec![
            Instruction::new(Opcode::Init, 0, 1, 0),
            Instruction::new(Opcode::Halt, 0, 0, 0),
        ],
        register_count: 1,
        cursor_count: 0,
        sorter_count: 0,
        distinct_count: 0,
        aggregate_count: 0,
        result_columns: Vec::new(),
        dependencies,
        readonly: true,
        parameter_count: 0,
    }
}

/// Prepares and runs every statement in a script, discarding the rows.
pub fn execute_batch(connection: &Connection, sql: &[u8]) -> DbResult<()> {
    let mut offset = 0usize;
    while offset < sql.len() {
        let rest = sql.get(offset..).unwrap_or(&[]);
        let (mut statement, consumed) = Statement::prepare(connection, rest)?;
        while statement.step()? {}
        statement.finalize()?;
        if consumed == 0 {
            break;
        }
        offset = offset.saturating_add(consumed);
    }
    Ok(())
}
