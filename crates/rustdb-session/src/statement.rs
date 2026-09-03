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

use rustdb_base::{error, DbResult};
use rustdb_sql::bind::{AllowAll, Authorizer, Binder, BoundStatement};
use rustdb_sql::directive::Directive;
use rustdb_sql::parser::parse_next_statement;
use rustdb_value::Value;
use rustdb_vm::compile_dml::{CONFLICT_FAIL, CONFLICT_ROLLBACK};
use rustdb_vm::machine::{Machine, MachineState, StepOutcome};
use rustdb_vm::program::{Operand, Program, ProgramDependencies};
use rustdb_vm::{compile, compile_dml, verify, verify_operands};

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
        let statement = Statement::from_compiled(connection, compiled.clone());
        Ok((statement, compiled.consumed))
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
        let machine = Machine::new(
            compiled.program.clone(),
            connection.interrupt_flag(),
            connection.limits().clone(),
        );
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
            self.connection.begin_statement(self.access)?;
            self.machine
                .record_row_changes(self.connection.wants_row_changes());
            self.open = true;
        }
        let outcome = self
            .connection
            .with_pager(|pager| self.machine.step(pager))?;
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
                self.close(Outcome::Done)?;
                Ok(false)
            }
            Err(failure) => {
                let ending = ending_for(&failure, self.machine.conflict_action());
                if ending == Outcome::Fail {
                    self.publish_counters();
                }
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

    /// Folds the machine's counters into the connection's.
    fn publish_counters(&mut self) {
        let changes = self.machine.changes();
        let rowid = self.machine.last_insert_rowid();
        let _ = self.connection.with_state(|state| {
            for _ in 0..changes {
                state.transaction.record_change();
            }
            if rowid != 0 {
                state.transaction.record_insert_rowid(rowid);
            }
        });
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
fn ending_for(failure: &rustdb_base::DbError, conflict: Option<i32>) -> Outcome {
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
    inner: &rustdb_sql::ast::Statement,
    sql: &[u8],
    parsed: &rustdb_sql::parser::ParsedStatement,
    authorizer: &dyn Authorizer,
) -> DbResult<Compiled> {
    let _ = (connection, authorizer);
    let bound = binder.bind_statement(inner)?;
    let dependencies = ProgramDependencies {
        schemas: binder.dependencies().schemas.clone(),
        generation: binder.dependencies().generation,
    };
    let parameters = parsed.parameters.count;
    let statement_sql = parsed.span.slice(sql).to_vec();
    let consumed = parsed.span.end as usize;

    let (program, plan) = match bound {
        BoundStatement::Select(select) => {
            let (program, plan) =
                compile::compile_select(*select, dependencies.clone(), parameters)?;
            (program, Some(plan))
        }
        BoundStatement::Insert(insert) => (
            compile_dml::compile_insert(&insert, dependencies.clone(), parameters)?,
            None,
        ),
        BoundStatement::Update(update) => (
            compile_dml::compile_update(&update, dependencies.clone(), parameters)?,
            None,
        ),
        BoundStatement::Delete(delete) => (
            compile_dml::compile_delete(&delete, dependencies.clone(), parameters)?,
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
#[derive(Clone)]
struct Compiled {
    program: Arc<Program>,
    body: Body,
    sql: Vec<u8>,
    consumed: usize,
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

/// Lexes, parses, binds, plans, compiles and verifies one statement.
fn compile_sql(
    connection: &Connection,
    sql: &[u8],
    authorizer: &dyn Authorizer,
) -> DbResult<Compiled> {
    let limits = connection.limits().clone();
    let parsed = parse_next_statement(sql, 0, &limits)?;
    let catalog = connection.catalog()?;
    let mut binder = Binder::new(catalog.as_ref(), &parsed.ast, authorizer);
    if let rustdb_sql::ast::Statement::Explain { query_plan, inner } = &parsed.statement {
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
    let bound = binder.bind_statement(&parsed.statement)?;
    let dependencies = ProgramDependencies {
        schemas: binder.dependencies().schemas.clone(),
        generation: binder.dependencies().generation,
    };
    let statement_sql = parsed.span.slice(sql).to_vec();
    let parameters = parsed.parameters.count;
    let (program, body) = match bound {
        BoundStatement::Select(select) => {
            let (program, _) = compile::compile_select(*select, dependencies, parameters)?;
            (program, Body::Program)
        }
        BoundStatement::Insert(insert) => (
            compile_dml::compile_insert(&insert, dependencies, parameters)?,
            Body::Program,
        ),
        BoundStatement::Update(update) => (
            compile_dml::compile_update(&update, dependencies, parameters)?,
            Body::Program,
        ),
        BoundStatement::Delete(delete) => (
            compile_dml::compile_delete(&delete, dependencies, parameters)?,
            Body::Program,
        ),
        BoundStatement::Directive(directive) => {
            let program = directive_program(dependencies, &directive);
            (program, Body::Directive(directive))
        }
        BoundStatement::Empty => (empty_program(dependencies), Body::Program),
    };
    let problems = verify(&program);
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
    })
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
        program.result_columns = execute::pragma_columns(name)
            .into_iter()
            .map(|name| rustdb_vm::program::ResultColumn {
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
    use rustdb_vm::program::{Instruction, Opcode};
    Program {
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
