//! Prepare, bind, step, reset and finalise.
//!
//! Invariant: a prepared statement records the catalog generation and every
//! schema cookie it was compiled against, and checks them before its first
//! step. A statement whose schema moved is recompiled from the SQL it kept -
//! which is `prepare_v2` behaviour - and one that cannot be recompiled reports
//! the failure rather than running against a shape that has changed.
//!
//! Prepare is the whole front end in one function: lex, parse, bind against the
//! catalog snapshot, plan, compile, and verify. Nothing between those steps
//! touches the file, so a statement that cannot be prepared costs no I/O.

use std::sync::Arc;

use rustdb_base::{error, DbResult};
use rustdb_sql::bind::{AllowAll, Authorizer, Binder, BoundStatement};
use rustdb_sql::parser::parse_next_statement;
use rustdb_value::Value;
use rustdb_vm::machine::{Machine, MachineState, StepOutcome};
use rustdb_vm::program::{Program, ProgramDependencies};
use rustdb_vm::{compile, verify, verify_operands};

use crate::connection::Connection;

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

/// A prepared statement.
pub struct Statement<'connection> {
    connection: &'connection Connection,
    program: Arc<Program>,
    machine: Machine,
    sql: Vec<u8>,
    columns: Vec<ColumnMetadata>,
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
        let (program, consumed, statement_sql) = compile_sql(connection, sql, authorizer)?;
        let columns = program
            .result_columns
            .iter()
            .map(|column| ColumnMetadata {
                name: column.name.clone(),
                origin: column.origin.clone(),
                declared_type: column.declared_type.clone(),
            })
            .collect();
        let machine = Machine::new(
            program.clone(),
            connection.interrupt_flag(),
            connection.limits().clone(),
        );
        Ok((
            Statement {
                connection,
                program,
                machine,
                sql: statement_sql,
                columns,
                finished: false,
            },
            consumed,
        ))
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
        self.program.readonly
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
        if self.machine.state() == MachineState::Prepared {
            self.check_schema()?;
        }
        let outcome = self
            .connection
            .with_reader(|pager| self.machine.step(pager));
        match outcome {
            Ok(StepOutcome::Row) => Ok(true),
            Ok(StepOutcome::Done) => {
                self.finished = true;
                Ok(false)
            }
            Err(failure) => {
                self.finished = true;
                Err(failure)
            }
        }
    }

    /// Returns the current row.
    pub fn row(&self) -> &[Value<'static>] {
        self.machine.row()
    }

    /// Returns one column of the current row.
    pub fn value(&self, index: usize) -> Value<'static> {
        self.machine
            .row()
            .get(index)
            .cloned()
            .unwrap_or(Value::Null)
    }

    /// Returns how many instructions the statement has run.
    pub fn steps(&self) -> u64 {
        self.machine.steps()
    }

    /// Resets the statement so it can run again, keeping its bindings.
    pub fn reset(&mut self) -> DbResult<()> {
        self.machine.reset();
        self.finished = false;
        Ok(())
    }

    /// Finalises the statement, releasing everything it holds.
    pub fn finalize(mut self) -> DbResult<()> {
        self.machine.reset();
        self.finished = true;
        Ok(())
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
        let (program, _, _) = compile_sql(self.connection, &self.sql, &AllowAll)?;
        self.columns = program
            .result_columns
            .iter()
            .map(|column| ColumnMetadata {
                name: column.name.clone(),
                origin: column.origin.clone(),
                declared_type: column.declared_type.clone(),
            })
            .collect();
        self.machine = Machine::new(
            program.clone(),
            self.connection.interrupt_flag(),
            self.connection.limits().clone(),
        );
        self.program = program;
        Ok(())
    }
}

/// Lexes, parses, binds, plans, compiles and verifies one statement.
fn compile_sql(
    connection: &Connection,
    sql: &[u8],
    authorizer: &dyn Authorizer,
) -> DbResult<(Arc<Program>, usize, Vec<u8>)> {
    let limits = connection.limits().clone();
    let parsed = parse_next_statement(sql, 0, &limits)?;
    let catalog = connection.catalog()?;
    let mut binder = Binder::new(catalog.as_ref(), &parsed.ast, authorizer);
    let bound = binder.bind_statement(&parsed.statement)?;
    let dependencies = ProgramDependencies {
        schemas: binder.dependencies().schemas.clone(),
        generation: binder.dependencies().generation,
    };
    let statement_sql = parsed.span.slice(sql).to_vec();
    let program = match bound {
        BoundStatement::Select(select) => {
            let (program, _) =
                compile::compile_select(*select, dependencies, parsed.parameters.count)?;
            program
        }
        BoundStatement::Empty => empty_program(dependencies),
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
    Ok((Arc::new(program), parsed.consumed, statement_sql))
}

/// Returns the program an empty statement compiles to.
fn empty_program(dependencies: ProgramDependencies) -> Program {
    use rustdb_vm::program::{Instruction, Opcode};
    Program {
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
