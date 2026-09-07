//! Opening a database and running statements against it.
//!
//! Invariant: **this is the shape a caller moves onto, and it holds no engine
//! state of its own.** Everything here delegates to [`crate::ImportedDatabase`],
//! which is the engine; what this adds is the `open` / `connect` / statement
//! shape that `inillucent-session` exposes today, so that re-rooting
//! `inillucent::Database`, the CLI and `inillucent-migrate` is a change of import
//! rather than a change of design.
//!
//! It is deliberately small. The old connection carries pragmas, savepoints,
//! hooks, collation and function registries, an authorizer, a busy timeout and a
//! VFS choice, and every one of those is a decision about behaviour rather than
//! a wrapper. Writing them all here before anything used them would be inventing
//! a second connection nobody had asked a question of. What is here is what the
//! first callers need, and the rest arrives as they are moved.
//!
//! ## `open` creates
//!
//! `Database::open` on a path that does not exist **creates** the database, and
//! on one that does it **opens and recovers** it. That is what a caller means by
//! `open`, it is what `inillucent-session` does today, and it is the reason
//! `ImportedDatabase::create` had to exist before this file could.

use std::cell::RefCell;
use std::path::{Path, PathBuf};

use inillucent_base::DbResult;
use inillucent_exec::physical::Params;
use inillucent_tree::datum::OwnedDatum;

use crate::{ImportedDatabase, DEFAULT_FRAMES};

/// The page size a database is created at.
///
/// The engine's own default, which Phase 1 fixed after measuring 16/32/64. A
/// connection that chose its own would be making files unlike the ones every
/// measurement was taken on.
pub const PAGE_SIZE: usize = 32_768;

/// An open database file.
pub struct Database {
    /// The engine, behind a cell because a statement takes `&mut` and a caller
    /// holds the database by shared reference - which is the same arrangement
    /// `inillucent-session` uses and for the same reason.
    engine: RefCell<ImportedDatabase>,
    /// The file, kept so it can be reported.
    path: PathBuf,
    /// How many rows the last statement changed.
    ///
    /// On the database rather than the statement because that is where
    /// `sqlite3_changes` reads it from: a caller asks the connection what the
    /// last statement did, having already dropped the statement.
    changes: std::cell::Cell<i64>,
}

impl Database {
    /// Opens a database, creating it when the path holds nothing.
    ///
    /// @param path - the database file
    pub fn open(path: impl AsRef<Path>) -> DbResult<Database> {
        Database::open_with(path, DEFAULT_FRAMES)
    }

    /// Opens a database with the pool size stated.
    ///
    /// @param path - the database file
    /// @param frames - how many frames the buffer pool holds
    pub fn open_with(path: impl AsRef<Path>, frames: usize) -> DbResult<Database> {
        let path = path.as_ref().to_path_buf();
        let engine = if path.is_file() {
            ImportedDatabase::open(path.clone(), PAGE_SIZE, frames)?
        } else {
            ImportedDatabase::create(path.clone(), PAGE_SIZE, frames)?
        };
        Ok(Database {
            engine: RefCell::new(engine),
            path,
            changes: std::cell::Cell::new(0),
        })
    }

    /// Imports a SQLite file into a new database beside it, and opens that.
    ///
    /// **The only way a SQLite fixture reaches this engine.** File-format
    /// compatibility is not a goal of the rearchitecture, so a `.db` SQLite
    /// wrote is not a file this engine opens - `open` on one reports that
    /// neither meta page is readable, which is true and is what it should say.
    /// The differential corpus and the qualification suites build their
    /// fixtures through the pinned SQLite and then compare *logical* answers,
    /// so the fixture is read once through `inillucent-sqlite-reader` and
    /// rebuilt as PAX trees. Same rows, different bytes.
    ///
    /// The rebuilt file sits beside the source with `.rdb` appended, so a
    /// fixture and its import are both inspectable after a failure and the
    /// source is never written to.
    ///
    /// @param path - the SQLite database to read
    pub fn import(path: impl AsRef<Path>) -> DbResult<Database> {
        Database::import_with(path, DEFAULT_FRAMES)
    }

    /// Imports a SQLite file with the pool size stated.
    ///
    /// @param path - the SQLite database to read
    /// @param frames - how many frames the buffer pool holds
    pub fn import_with(path: impl AsRef<Path>, frames: usize) -> DbResult<Database> {
        let source = path.as_ref().to_path_buf();
        let mut target = source.clone().into_os_string();
        target.push(".rdb");
        let target = PathBuf::from(target);
        let engine = ImportedDatabase::import_into(source, target.clone(), PAGE_SIZE, frames)?;
        Ok(Database {
            engine: RefCell::new(engine),
            path: target,
            changes: std::cell::Cell::new(0),
        })
    }

    /// Returns a connection to this database.
    ///
    /// **Each one is its own session, and that is what `temp` is scoped to.** A
    /// temporary table belongs to the connection that made it and to no other,
    /// which is SQLite's rule and is graded against it - so a connection is a
    /// number the engine can tell apart, rather than a borrow that is
    /// indistinguishable from every other borrow.
    pub fn connect(&self) -> Connection<'_> {
        let session = self.engine.borrow().open_session();
        Connection {
            database: self,
            session,
        }
    }

    /// Returns a connection that is a continuation of an earlier one.
    ///
    /// **For a caller that hands out connections per call over one logical
    /// connection**, which `inillucent-compat`'s facade does: it opens a
    /// short-lived engine connection per statement because the suites return
    /// connections from helper functions, and every one of those has to be the
    /// same session or a temporary table would not survive the statement that
    /// made it.
    ///
    /// @param session - the number an earlier `connect` returned
    pub fn connect_as(&self, session: u64) -> Connection<'_> {
        Connection {
            database: self,
            session,
        }
    }

    /// Returns the file this database is in.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Makes everything written so far durable in the file.
    ///
    /// A database that is dropped without this is not lost - `open` replays the
    /// log - but a checkpoint is what makes the next open cheap.
    pub fn checkpoint(&self) -> DbResult<()> {
        self.engine.borrow_mut().checkpoint()
    }

    /// Checks every tree's structure.
    pub fn check(&self) -> DbResult<()> {
        self.engine.borrow().check_trees()
    }

    /// Copies this database into a file, and checks the copy.
    ///
    /// **A checkpoint and a file copy, not a page-by-page walk.** SQLite's
    /// backup copies pages because it copies them *while other connections
    /// write*, and the interesting part of its API is the incremental step. This
    /// engine is single threaded and one file is one pool: there is no second
    /// writer to race, so the honest backup is to fold the log into the file and
    /// copy the file. What that costs is one checkpoint, which the caller was
    /// going to pay on close anyway.
    ///
    /// The copy is opened and walked before this returns. A backup nobody
    /// checked is a file that is assumed to be a database, and the cost of
    /// finding out otherwise is paid at the worst possible moment.
    ///
    /// @param path - where the copy goes
    pub fn backup_to(&self, path: impl AsRef<Path>) -> DbResult<()> {
        let path = path.as_ref();
        self.checkpoint()?;
        std::fs::copy(&self.path, path).map_err(|error| {
            inillucent_base::error::misuse(format!(
                "cannot copy {} to {}: {error}",
                self.path.display(),
                path.display()
            ))
        })?;
        let copy = Database::open(path)?;
        copy.check()
    }
}

/// A connection to a database.
///
/// It borrows the database rather than owning a handle of its own, because this
/// engine is single threaded and one file is one pool: two connections that each
/// held a pool over one file would be two page caches over one set of bytes.
pub struct Connection<'d> {
    database: &'d Database,
    /// Which connection this is, which is what `temp` is resolved against.
    session: u64,
}

impl Connection<'_> {
    /// Returns this connection's own number.
    pub fn session(&self) -> u64 {
        self.session
    }
}

impl<'d> Connection<'d> {
    /// Returns the engine, having told it which connection is asking.
    ///
    /// Every entry point goes through one of these two, because `temp` means
    /// "this connection's temporary database" and there is no other way for the
    /// engine to know which connection that is.
    ///
    /// **Mutable even for the read-only calls**, because telling the engine
    /// whose statement this is can change the schema it derives: a connection
    /// that has its own temporary tables sees a different set from the one
    /// before it, and a binder handed the previous connection's is the leak
    /// `temp` exists to prevent.
    fn engine(&self) -> std::cell::RefMut<'_, ImportedDatabase> {
        self.engine_mut()
    }

    /// Returns the engine, having told it which connection is asking.
    fn engine_mut(&self) -> std::cell::RefMut<'_, ImportedDatabase> {
        let mut held = self.database.engine.borrow_mut();
        held.use_session(self.session);
        held
    }

    /// Runs one or more statements for their effect.
    ///
    /// @param sql - the statements, separated by semicolons
    pub fn execute_batch(&self, sql: &str) -> DbResult<()> {
        for statement in split_statements(sql) {
            let outcome = self.engine_mut().execute_any(&statement, &Params::new())?;
            self.database.changes.set(outcome.changes.rows as i64);
        }
        Ok(())
    }

    /// Runs one statement and returns its rows.
    ///
    /// @param sql - the statement
    pub fn query(&self, sql: &str) -> DbResult<Vec<Vec<OwnedDatum>>> {
        self.query_with(sql, &Params::new())
    }

    /// Runs one statement with bound parameters and returns its rows.
    ///
    /// @param sql - the statement
    /// @param params - the values bound to `?1`, `?2`, ...
    pub fn query_with(&self, sql: &str, params: &Params) -> DbResult<Vec<Vec<OwnedDatum>>> {
        let outcome = self.engine_mut().execute_any(sql, params)?;
        self.database.changes.set(outcome.changes.rows as i64);
        Ok(outcome.rows)
    }

    /// Compiles the first statement of a script, and says how much it used.
    ///
    /// **The count includes the terminating semicolon and the trivia after it**,
    /// which is what makes a caller's `&sql[consumed..]` the next statement
    /// rather than a leading space. It is the parser's own count, so a script
    /// walked this way is split by the grammar rather than by a scan for `;`.
    ///
    /// @param sql - the script, positioned at the statement to compile
    pub fn prepare_with_tail(&self, sql: &str) -> DbResult<(Statement<'d>, usize)> {
        let consumed = self.engine().statement_length(sql)?;
        let head = sql.get(..consumed).unwrap_or(sql);
        Ok((self.prepare(head)?, consumed))
    }

    /// Describes how a statement would be run.
    ///
    /// The operator chain, which is what `EXPLAIN QUERY PLAN` answers. There is
    /// no bytecode listing because there is no bytecode.
    ///
    /// @param sql - the statement
    pub fn explain(&self, sql: &str) -> DbResult<Vec<String>> {
        Ok(self.engine().plan(sql)?.describe())
    }

    /// Registers a scalar an application defined, replacing one of the same
    /// name and arity.
    ///
    /// A registration shadows a built-in of the same name, which is SQLite's
    /// rule, and it throws away every compiled statement - which function a
    /// name resolves to is decided when a statement is bound.
    ///
    /// @param name - the name SQL calls it by
    /// @param arity - how many arguments it takes, or -1 for any number
    /// @param flags - what the function promises about itself
    /// @param body - what it does
    pub fn create_scalar_function(
        &self,
        name: &str,
        arity: i32,
        flags: inillucent_ext::registry::FunctionFlags,
        body: inillucent_ext::registry::ScalarBody,
    ) -> DbResult<()> {
        self.engine_mut()
            .create_scalar_function(name, arity, flags, body)
    }

    /// Registers an aggregate an application defined.
    ///
    /// @param name - the name SQL calls it by
    /// @param arity - how many arguments it takes, or -1 for any number
    /// @param flags - what the function promises about itself
    /// @param body - what it does with a whole group
    pub fn create_aggregate_function(
        &self,
        name: &str,
        arity: i32,
        flags: inillucent_ext::registry::FunctionFlags,
        body: inillucent_ext::registry::AggregateBody,
    ) -> DbResult<()> {
        self.engine_mut()
            .create_aggregate_function(name, arity, flags, body)
    }

    /// Removes a function by name and arity, reporting whether one went.
    ///
    /// @param name - the name it was registered under
    /// @param arity - the arity it was registered for
    pub fn remove_function(&self, name: &str, arity: i32) -> bool {
        self.engine_mut().remove_function(name, arity)
    }

    /// Registers a collating sequence an application defined.
    ///
    /// @param name - the name `COLLATE` calls it by
    /// @param comparator - how it orders two values
    pub fn create_collation(
        &self,
        name: &str,
        comparator: inillucent_value::collation::Comparator,
    ) -> DbResult<()> {
        self.engine_mut().create_collation(name, comparator)
    }

    /// Returns how many statements are compiled and held.
    pub fn cached_plan_count(&self) -> usize {
        self.engine().cached_plan_count()
    }

    /// Turns off one or more planner optimizations for this connection.
    ///
    /// @param mask - the levers to switch off
    pub fn disable_optimizations(&self, mask: u32) {
        self.engine_mut().disable_optimizations(mask);
    }

    /// Rereads the schema from the file.
    pub fn reload_schema(&self) -> DbResult<()> {
        self.engine_mut().reload_catalog()
    }

    /// Returns the schema's generation, which changes when the schema does.
    pub fn schema_cookie(&self) -> u64 {
        self.engine().schema_generation()
    }

    /// Compiles a statement to be bound and stepped.
    ///
    /// @param sql - the statement
    pub fn prepare(&self, sql: &str) -> DbResult<Statement<'d>> {
        let compiled = self.engine().prepare_statement(sql)?;
        Ok(Statement {
            database: self.database,
            compiled,
            params: Params::new(),
            rows: Vec::new(),
            names: Vec::new(),
            at: 0,
            run: false,
            changed: 0,
            session: self.session,
        })
    }

    /// Runs one statement for its effect and returns how many rows it changed.
    ///
    /// @param sql - the statement
    pub fn execute(&self, sql: &str) -> DbResult<i64> {
        self.query(sql)?;
        Ok(self.changes())
    }

    /// Returns how many rows the last statement on this database changed.
    pub fn changes(&self) -> i64 {
        self.database.changes.get()
    }

    /// Returns how many rows every statement so far has changed.
    pub fn total_changes(&self) -> i64 {
        self.engine().total_changes()
    }

    /// Returns the rowid the last `INSERT` assigned.
    pub fn last_insert_rowid(&self) -> i64 {
        self.engine().last_insert_rowid()
    }

    /// Returns how many databases the last commit was decided over.
    ///
    /// One for an ordinary statement; two or more for a transaction that wrote
    /// two files and was therefore committed through a super-journal.
    pub fn decided_over(&self) -> usize {
        self.engine().decided_over()
    }

    /// Returns whether every statement is its own transaction.
    ///
    /// `false` between a `BEGIN` and its `COMMIT`.
    pub fn autocommit(&self) -> bool {
        self.engine().autocommit()
    }
}

/// A statement compiled once, bound, and stepped for its rows.
///
/// **It materialises.** `step` runs the whole statement on its first call and
/// then walks the rows it produced, where SQLite's `sqlite3_step` produces one
/// row at a time. The shape is the same to a caller and the memory is not, so a
/// query over a large table costs what the result costs rather than what one row
/// costs. The engine's executor is batch-at-a-time and its sinks collect, so a
/// row-at-a-time `step` would be a different executor rather than a different
/// wrapper - it is on the list beside the rest of the connection surface, and
/// this is written so that the callers being moved read the same.
pub struct Statement<'d> {
    /// The database the statement runs against.
    database: &'d Database,
    /// The engine's compiled handle, reused across executions.
    compiled: crate::Statement,
    /// The values bound so far.
    params: Params,
    /// The rows the last execution produced.
    rows: Vec<Vec<OwnedDatum>>,
    /// The result column names.
    names: Vec<String>,
    /// How many rows have been stepped over.
    at: usize,
    /// Whether this binding has been executed yet.
    run: bool,
    /// How many rows the last execution changed.
    changed: usize,
    /// The connection it was compiled on.
    session: u64,
}

impl Statement<'_> {
    /// Binds one parameter.
    ///
    /// @param index - the one-based parameter number
    /// @param value - the value
    pub fn bind(&mut self, index: u32, value: OwnedDatum) -> DbResult<()> {
        self.params.set(index, value);
        self.run = false;
        Ok(())
    }

    /// Binds an integer.
    ///
    /// @param index - the one-based parameter number
    /// @param value - the value
    pub fn bind_integer(&mut self, index: u32, value: i64) -> DbResult<()> {
        self.bind(index, OwnedDatum::Int(value))
    }

    /// Binds text.
    ///
    /// @param index - the one-based parameter number
    /// @param value - the value
    pub fn bind_text(&mut self, index: u32, value: &str) -> DbResult<()> {
        self.bind(index, OwnedDatum::Text(value.as_bytes().to_vec()))
    }

    /// Binds a blob.
    ///
    /// @param index - the one-based parameter number
    /// @param value - the bytes
    pub fn bind_blob(&mut self, index: u32, value: &[u8]) -> DbResult<()> {
        self.bind(index, OwnedDatum::Blob(value.to_vec()))
    }

    /// Binds NULL.
    ///
    /// @param index - the one-based parameter number
    pub fn bind_null(&mut self, index: u32) -> DbResult<()> {
        self.bind(index, OwnedDatum::Null)
    }

    /// Unbinds every parameter.
    pub fn clear_bindings(&mut self) {
        self.params.clear();
        self.run = false;
    }

    /// Runs the statement if it has not run, then advances to the next row.
    ///
    /// Returns whether a row is available to [`Statement::row`].
    pub fn step(&mut self) -> DbResult<bool> {
        if !self.run {
            let mut held = self.database.engine.borrow_mut();
            // The statement runs on the connection that compiled it, because
            // `temp` means that connection's temporary database and the plan was
            // bound against it.
            held.use_session(self.session);
            let outcome = held.execute_statement(&self.compiled, &self.params)?;
            self.changed = outcome.changes.rows;
            self.database.changes.set(self.changed as i64);
            self.names = outcome.names;
            self.rows = outcome.rows;
            self.at = 0;
            self.run = true;
        }
        if self.at < self.rows.len() {
            self.at = self.at.saturating_add(1);
            return Ok(true);
        }
        Ok(false)
    }

    /// Returns the row the last [`Statement::step`] arrived at.
    ///
    /// Empty before the first successful step, which is what a caller that
    /// ignored `step`'s answer would otherwise read past.
    pub fn row(&self) -> &[OwnedDatum] {
        match self.at.checked_sub(1).and_then(|nth| self.rows.get(nth)) {
            Some(row) => row,
            None => &[],
        }
    }

    /// Returns the result column names, once the statement has been stepped.
    ///
    /// **Empty before the first `step`**, which is where this differs from
    /// `sqlite3_column_name`: that answers straight after a prepare, because
    /// SQLite compiles the column names as part of compiling the statement.
    /// This statement materialises on its first step and learns its shape from
    /// what came back, so there is nothing to report until then. A caller that
    /// asked first got an empty list and printed no header, which is how the
    /// difference was found.
    pub fn columns(&self) -> &[String] {
        &self.names
    }

    /// Returns how many rows the last execution changed.
    pub fn changes(&self) -> usize {
        self.changed
    }

    /// Runs the statement again with the parameters bound since the last run.
    pub fn reset(&mut self) {
        self.run = false;
        self.at = 0;
    }
}

/// Splits a batch into statements on semicolons outside quotes.
///
/// **Small on purpose, and not a parser.** The engine has one, and a batch that
/// needs it should go through `parse_next_statement` rather than through this.
/// What this handles is the shape a caller writes in a test or a schema file:
/// statements separated by semicolons, with semicolons inside string literals
/// and identifiers left alone. A statement it splits wrongly fails to parse and
/// says so, rather than doing something unintended.
///
/// @param sql - the batch
fn split_statements(sql: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut quote: Option<char> = None;
    for character in sql.chars() {
        match quote {
            Some(open) => {
                current.push(character);
                if character == open {
                    quote = None;
                }
            }
            None => match character {
                '\'' | '"' | '`' => {
                    quote = Some(character);
                    current.push(character);
                }
                ';' => {
                    if !current.trim().is_empty() {
                        out.push(current.trim().to_string());
                    }
                    current.clear();
                }
                _ => current.push(character),
            },
        }
    }
    if !current.trim().is_empty() {
        out.push(current.trim().to_string());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A batch splits on the semicolons that separate statements and no others.
    #[test]
    fn a_batch_splits_on_statement_semicolons() {
        let split = split_statements(
            "CREATE TABLE t (a TEXT); INSERT INTO t VALUES ('one;two'); SELECT * FROM t",
        );
        assert_eq!(
            split,
            vec![
                "CREATE TABLE t (a TEXT)".to_string(),
                "INSERT INTO t VALUES ('one;two')".to_string(),
                "SELECT * FROM t".to_string(),
            ],
            "a semicolon inside a literal is not a separator"
        );
    }

    /// A trailing semicolon and blank statements produce nothing extra.
    #[test]
    fn a_trailing_semicolon_is_not_a_statement() {
        assert_eq!(
            split_statements("SELECT 1;;  ;"),
            vec!["SELECT 1".to_string()],
            "empty statements are dropped"
        );
    }
}
