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

    /// Returns a connection to this database.
    pub fn connect(&self) -> Connection<'_> {
        Connection { database: self }
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
}

impl<'d> Connection<'d> {
    /// Runs one or more statements for their effect.
    ///
    /// @param sql - the statements, separated by semicolons
    pub fn execute_batch(&self, sql: &str) -> DbResult<()> {
        for statement in split_statements(sql) {
            let outcome = self
                .database
                .engine
                .borrow_mut()
                .execute_any(&statement, &Params::new())?;
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
        let outcome = self.database.engine.borrow_mut().execute_any(sql, params)?;
        self.database.changes.set(outcome.changes.rows as i64);
        Ok(outcome.rows)
    }

    /// Compiles a statement to be bound and stepped.
    ///
    /// @param sql - the statement
    pub fn prepare(&self, sql: &str) -> DbResult<Statement<'d>> {
        let compiled = self.database.engine.borrow().prepare_statement(sql)?;
        Ok(Statement {
            database: self.database,
            compiled,
            params: Params::new(),
            rows: Vec::new(),
            names: Vec::new(),
            at: 0,
            run: false,
            changed: 0,
        })
    }

    /// Returns how many rows the last statement on this database changed.
    pub fn changes(&self) -> i64 {
        self.database.changes.get()
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
            let outcome = self
                .database
                .engine
                .borrow_mut()
                .execute_statement(&self.compiled, &self.params)?;
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
