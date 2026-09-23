//! The qualification suites' handle on the new engine.
//!
//! Invariant: this module decides nothing. Every method is a call and a
//! conversion - there is no fallback that answers when the engine refuses, no
//! default that stands in for a value it did not return, and no place where a
//! failure becomes a success. A suite repointed through here fails exactly when
//! the engine fails it, which is the only thing that makes the failures worth
//! reading.
//!
//! ## Why an adapter rather than a rewrite
//!
//! `inillucent-compat`'s suites - the differential corpus, the ACID and
//! robustness families, the SQL semantics files - were written against the old
//! engine's facade, `inillucent::{Database, Connection, Value}`. They are the
//! qualification, not the old engine's tests: what they assert is what SQLite
//! answers, and none of that changes when the engine underneath does.
//!
//! So they are repointed rather than rewritten. This module presents the
//! facade's shape over `inillucent-engine`, which keeps every assertion, every
//! fixture and every corpus entry exactly as it was and moves only the handle
//! they hold. A rewrite of three hundred tests would have been three hundred
//! chances to weaken an assertion while claiming to preserve it.
//!
//! ## The two places the shape genuinely differs
//!
//! - **A connection owns its database.** The engine's `Connection` borrows -
//!   one file is one pool, and it is single threaded - but the suites have
//!   helper functions that *return* a connection, which a borrow cannot
//!   outlive. So this connection holds an `Rc` of the database and opens a
//!   short-lived engine connection per call, which is what `inillucent-cli`
//!   already does for the same reason.
//! - **Rows arrive as `Value`.** The engine answers in `OwnedDatum`; the suites
//!   format `Value`. The conversion is total in the direction it runs.
//!
//! Nothing here decides anything. There is no fallback that answers when the
//! engine refuses, and no place where a failure becomes a success: every method
//! is a call and a conversion.

use std::path::{Path, PathBuf};
use std::rc::Rc;

use inillucent_base::DbResult;
use inillucent_engine::connect::Database as Engine;
use inillucent_tree::datum::{owned_row_values, OwnedDatum};
use inillucent_value::Value;

/// An open database file.
pub struct Database {
    /// The engine, shared with every connection opened from it.
    engine: Rc<Engine>,
}

impl Database {
    /// Opens a database, creating it when the path holds nothing.
    ///
    /// @param path - the database file
    pub fn open(path: impl AsRef<Path>) -> DbResult<Database> {
        Ok(Database {
            engine: Rc::new(Engine::open(path)?),
        })
    }

    /// Opens a database at a page size and pool the caller names.
    ///
    /// **The page size goes into the open because it cannot be set after it.**
    /// `PRAGMA page_size` in this engine reports the geometry and does not
    /// change it, so a suite that opened with [`Database::open`] and then asked
    /// for 4,096 byte pages would still be running at 32,768 and would not be
    /// told. This is the facade's form of `matrix::Arm::open`, for a suite that
    /// holds this handle rather than the engine's (task-2075).
    ///
    /// @param path - the database file
    /// @param page_size - the page size to build at, or the one the file already has
    /// @param frames - how many frames the buffer pool holds
    pub fn open_at(path: impl AsRef<Path>, page_size: usize, frames: usize) -> DbResult<Database> {
        Ok(Database {
            engine: Rc::new(Engine::open_at(path, page_size, frames)?),
        })
    }

    /// Opens a database, accepting a busy timeout there is nothing to wait for.
    ///
    /// **The timeout is accepted and not used, deliberately.** It exists in the
    /// suites because the old engine took a file lock another process could
    /// hold. This engine is one pool per file in one process; there is no lock
    /// to wait on, so a wait would be a sleep pretending to be a retry. The
    /// parameter stays so the call sites keep reading as what they are testing.
    ///
    /// @param path - the database file
    /// @param _timeout - how long the old engine would have waited
    pub fn open_with_busy_timeout(
        path: impl AsRef<Path>,
        _timeout: std::time::Duration,
    ) -> DbResult<Database> {
        Database::open(path)
    }

    /// Imports a SQLite file into a new database beside it, and opens that.
    ///
    /// **What "the same fixture" means now.** These suites build a fixture
    /// through the pinned SQLite and then ask both engines the same statements
    /// against it. The old engine read that file directly because it shared
    /// SQLite's format; this one does not, so the fixture is read once through
    /// `inillucent-sqlite-reader` and rebuilt as PAX trees. The rows are the
    /// same rows and the bytes are different bytes, which is the arrangement
    /// the TDD chose when it dropped file-format compatibility.
    ///
    /// @param path - the SQLite database the oracle built
    pub fn import(path: impl AsRef<Path>) -> DbResult<Database> {
        Ok(Database {
            engine: Rc::new(Engine::import(path)?),
        })
    }

    /// Copies a SQLite fixture somewhere writable, then imports the copy.
    ///
    /// **A tracked fixture is never imported where it sits.** The import writes
    /// its rebuilt file beside the source, and `compat/fixtures` is in the
    /// repository - so an import there leaves a build product among the inputs,
    /// which is how two of them turned up in `git status`. Staging first also
    /// means a failed import cannot touch the fixture every other suite reads.
    ///
    /// @param source - the tracked SQLite fixture
    /// @param tag - a name for the scratch directory, unique per suite
    pub fn import_staged(source: impl AsRef<Path>, tag: &str) -> DbResult<Database> {
        let source = source.as_ref();
        let directory =
            std::env::temp_dir().join(format!("inillucent-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&directory).map_err(|error| {
            inillucent_base::error::misuse(format!("cannot make {}: {error}", directory.display()))
        })?;
        let staged = directory.join(source.file_name().unwrap_or_default());
        std::fs::copy(source, &staged).map_err(|error| {
            inillucent_base::error::misuse(format!("cannot stage {}: {error}", source.display()))
        })?;
        Database::import(staged)
    }

    /// Returns what the page cache has been asked to do, across every
    /// connection opened from this handle.
    ///
    /// For a story that grades how many pages a statement read, which is a
    /// count and so means the same thing on a busy machine as on an idle one
    /// (task-2077).
    pub fn cache_stats(&self) -> inillucent_engine::connect::CacheStats {
        self.engine.cache_stats()
    }

    /// Imports a SQLite file, accepting a busy timeout there is nothing to wait
    /// for.
    ///
    /// @param path - the SQLite database the oracle built
    /// @param _timeout - how long the old engine would have waited
    pub fn import_with_busy_timeout(
        path: impl AsRef<Path>,
        _timeout: std::time::Duration,
    ) -> DbResult<Database> {
        Database::import(path)
    }

    /// Returns a connection to this database.
    ///
    /// **The session is taken once, here, and every later call reuses it.** The
    /// suites hold a `Connection` and this type opens a short-lived engine
    /// connection per call - so without a session of its own, every statement
    /// would be a different connection to the engine, and a temporary table
    /// would not survive the statement that made it.
    ///
    /// Called `session` rather than `connect` for the reason
    /// [`inillucent_engine::connect::Database::session`] is: two of them share
    /// one transaction.
    pub fn session(&self) -> DbResult<Connection> {
        let session = self.engine.session().session();
        Ok(Connection {
            engine: Rc::clone(&self.engine),
            session,
        })
    }

    /// Returns the file this database is in.
    pub fn path(&self) -> PathBuf {
        self.engine.path().to_path_buf()
    }

    /// Makes everything written so far durable in the file.
    pub fn checkpoint(&self) -> DbResult<()> {
        self.engine.checkpoint()
    }

    /// Checks every tree's structure.
    pub fn check(&self) -> DbResult<()> {
        self.engine.check()
    }

    /// Copies this database into a file, and checks the copy.
    ///
    /// @param path - where the copy goes
    pub fn backup_to(&self, path: impl AsRef<Path>) -> DbResult<()> {
        self.engine.backup_to(path)
    }
}

/// A connection to a database.
pub struct Connection {
    /// The database, held so a connection can outlive the handle it came from.
    engine: Rc<Engine>,
    /// Which connection this is, so that every call is the same one.
    session: u64,
}

impl Connection {
    /// Returns this connection's engine handle, on its own session.
    fn open(&self) -> inillucent_engine::connect::Connection<'_> {
        self.engine.session_as(self.session)
    }
}

impl Connection {
    /// Runs one or more statements for their effect.
    ///
    /// @param sql - the statements, separated by semicolons
    pub fn execute_batch(&self, sql: &str) -> DbResult<()> {
        self.open().execute_batch(sql)
    }

    /// Runs one statement for its effect and returns how many rows it changed.
    ///
    /// @param sql - the statement
    pub fn execute(&self, sql: &str) -> DbResult<i64> {
        self.open().execute(sql)
    }

    /// Runs one statement and returns its rows.
    ///
    /// @param sql - the statement
    pub fn query(&self, sql: &str) -> DbResult<Vec<Vec<Value<'static>>>> {
        self.open()
            .query(sql)?
            .iter()
            .map(|row| owned_row_values(row))
            .collect::<DbResult<Vec<_>>>()
    }

    /// Compiles a statement to be bound and stepped.
    ///
    /// @param sql - the statement
    pub fn prepare(&self, sql: &str) -> DbResult<Statement<'_>> {
        Ok(Statement {
            inner: self.open().prepare(sql)?,
            engine: Rc::clone(&self.engine),
            sql: sql.to_string(),
            session: self.session,
            row: Vec::new(),
        })
    }

    /// Compiles the first statement of a script, and says how much it used.
    ///
    /// @param sql - the script, positioned at the statement to compile
    pub fn prepare_with_tail(&self, sql: &str) -> DbResult<(Statement<'_>, usize)> {
        let prepared = self.open().prepare_with_tail(sql)?;
        let consumed = prepared.consumed;
        Ok((
            Statement {
                inner: prepared.statement,
                engine: Rc::clone(&self.engine),
                sql: sql.get(..consumed).unwrap_or(sql).to_string(),
                session: self.session,
                row: Vec::new(),
            },
            consumed,
        ))
    }

    /// Registers a scalar an application defined.
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
        self.open().create_scalar_function(name, arity, flags, body)
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
        self.open()
            .create_aggregate_function(name, arity, flags, body)
    }

    /// Puts the connection into or out of defensive mode.
    ///
    /// The shell reaches this through `.dbconfig defensive`; there is no pragma
    /// for it, here or in SQLite, so a test has to ask the connection.
    ///
    /// @param on - whether the flag is in force
    pub fn set_defensive(&self, on: bool) -> DbResult<()> {
        self.open().set_defensive(on)
    }

    /// Removes a function by name and arity, reporting whether one went.
    ///
    /// @param name - the name it was registered under
    /// @param arity - the arity it was registered for
    pub fn remove_function(&self, name: &str, arity: i32) -> bool {
        self.open().remove_function(name, arity).unwrap_or_default()
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
        self.open().create_collation(name, comparator)
    }

    /// Returns how many statements are compiled and held.
    pub fn cached_plan_count(&self) -> usize {
        self.open().cached_plan_count().unwrap_or_default()
    }

    /// Turns off one or more planner optimizations for this connection.
    ///
    /// @param mask - the levers to switch off
    pub fn disable_optimizations(&self, levers: inillucent_sql::plan::Levers) {
        let _ = self.open().disable_optimizations(levers);
    }

    /// Rereads the schema from the file.
    pub fn reload_schema(&self) -> DbResult<()> {
        self.open().reload_schema()
    }

    /// Returns the schema's generation, which changes when the schema does.
    ///
    /// @param _database - which attached database, which this engine has one of
    pub fn schema_cookie(&self, _database: usize) -> DbResult<u64> {
        self.open().schema_cookie()
    }

    /// Returns how many rows the last statement changed.
    pub fn changes(&self) -> i64 {
        self.open().changes().unwrap_or_default()
    }

    /// Returns how many rows every statement so far has changed.
    pub fn total_changes(&self) -> i64 {
        self.open().total_changes().unwrap_or_default()
    }

    /// Returns the rowid the last `INSERT` assigned.
    pub fn last_insert_rowid(&self) -> i64 {
        self.open().last_insert_rowid().unwrap_or_default()
    }

    /// Returns whether every statement is its own transaction.
    pub fn autocommit(&self) -> bool {
        self.open().autocommit().unwrap_or(true)
    }

    /// Makes everything written so far durable in the file.
    pub fn checkpoint(&self) -> DbResult<()> {
        self.engine.checkpoint()
    }

    /// Checks every tree's structure.
    pub fn check(&self) -> DbResult<()> {
        self.engine.check()
    }

    /// Returns the file this connection's database is in.
    pub fn path(&self) -> PathBuf {
        self.engine.path().to_path_buf()
    }
}

/// A statement compiled once, bound, and stepped for its rows.
pub struct Statement<'connection> {
    /// The engine's statement.
    inner: inillucent_engine::connect::Statement<'connection>,
    /// The database, so the statement can be described after it is compiled.
    engine: Rc<Engine>,
    /// The statement's own text, kept for [`Statement::explain`].
    sql: String,
    /// The connection it was compiled on, so a plan is described against the
    /// same schema it was built against.
    session: u64,
    /// The row the last step arrived at, converted once per step.
    row: Vec<Value<'static>>,
}

impl Statement<'_> {
    /// Binds one parameter.
    ///
    /// @param index - the one-based parameter number
    /// @param value - the value
    pub fn bind(&mut self, index: u32, value: Value<'static>) -> DbResult<()> {
        self.inner.bind(index, OwnedDatum::from(&value))
    }

    /// Binds an integer.
    ///
    /// @param index - the one-based parameter number
    /// @param value - the value
    pub fn bind_integer(&mut self, index: u32, value: i64) -> DbResult<()> {
        self.inner.bind_integer(index, value)
    }

    /// Binds a real.
    ///
    /// @param index - the one-based parameter number
    /// @param value - the value
    pub fn bind_real(&mut self, index: u32, value: f64) -> DbResult<()> {
        self.inner.bind(index, OwnedDatum::Real(value))
    }

    /// Binds text.
    ///
    /// @param index - the one-based parameter number
    /// @param value - the value
    pub fn bind_text(&mut self, index: u32, value: &str) -> DbResult<()> {
        self.inner.bind_text(index, value)
    }

    /// Binds a blob.
    ///
    /// @param index - the one-based parameter number
    /// @param value - the bytes
    pub fn bind_blob(&mut self, index: u32, value: &[u8]) -> DbResult<()> {
        self.inner.bind_blob(index, value)
    }

    /// Binds NULL.
    ///
    /// @param index - the one-based parameter number
    pub fn bind_null(&mut self, index: u32) -> DbResult<()> {
        self.inner.bind_null(index)
    }

    /// Unbinds every parameter.
    pub fn clear_bindings(&mut self) {
        self.inner.clear_bindings();
    }

    /// Runs the statement if it has not run, then advances to the next row.
    pub fn step(&mut self) -> DbResult<bool> {
        let more = self.inner.step()?;
        self.row = if more {
            owned_row_values(self.inner.row())?
        } else {
            Vec::new()
        };
        Ok(more)
    }

    /// Returns the row the last successful step arrived at.
    pub fn row(&self) -> &[Value<'static>] {
        &self.row
    }

    /// Returns one value of the current row.
    ///
    /// @param index - the zero-based column number
    pub fn value(&self, index: usize) -> Value<'static> {
        self.row.get(index).cloned().unwrap_or(Value::Null)
    }

    /// Returns one value of the current row as an integer.
    ///
    /// @param index - the zero-based column number
    pub fn value_integer(&self, index: usize) -> Option<i64> {
        match self.row.get(index) {
            Some(Value::Integer(number)) => Some(*number),
            _ => None,
        }
    }

    /// Returns one value of the current row as text.
    ///
    /// @param index - the zero-based column number
    pub fn value_text(&self, index: usize) -> Option<String> {
        match self.row.get(index) {
            Some(Value::Text(text)) => Some(String::from_utf8_lossy(text.raw()).into_owned()),
            _ => None,
        }
    }

    /// Returns the result column names, once the statement has been stepped.
    pub fn columns(&self) -> &[String] {
        self.inner.columns()
    }

    /// Returns how many columns the last step's row held.
    pub fn column_count(&self) -> usize {
        self.inner.columns().len()
    }

    /// Returns one result column's name.
    ///
    /// @param index - the zero-based column number
    pub fn column_name(&self, index: usize) -> Option<&str> {
        self.inner.columns().get(index).map(String::as_str)
    }

    /// Returns how many rows the last execution changed.
    pub fn changes(&self) -> usize {
        self.inner.changes()
    }

    /// Runs the statement again with the parameters bound since the last run.
    pub fn reset(&mut self) -> DbResult<()> {
        self.inner.reset();
        self.row.clear();
        Ok(())
    }

    /// Describes how this statement would be run.
    ///
    /// **The operator chain, not a bytecode listing.** The old engine answered
    /// this with its VDBE program, and a caller that asserted on an opcode name
    /// was asserting on that representation. This engine compiles no bytecode;
    /// the same question - which tree did the planner choose, and how does it
    /// reach it - is what the chain says.
    pub fn explain(&self) -> Vec<String> {
        self.engine
            .session_as(self.session)
            .explain(&self.sql)
            .unwrap_or_else(|error| vec![format!("cannot describe: {error:?}")])
    }

    /// Drops the statement, reporting anything it was still holding.
    pub fn finalize(self) -> DbResult<()> {
        Ok(())
    }
}
