//! The stable public Rust facade: `Database`, `Connection`, `Statement`, and
//! `Row`.
//!
//! Invariant: lifetimes say what the runtime would otherwise have to enforce. A
//! `Row` cannot outlive the step that produced it, a `Statement` cannot outlive
//! its `Connection`, and a `Connection` cannot outlive its `Database`. Anything
//! a type cannot express - a statement stepped after it failed, a parameter
//! index that does not exist - is a `DbError` with the code SQLite uses.
//!
//! This is the surface a program writes against, so it is deliberately small
//! and deliberately explicit. Nothing here converts a value silently: `get`
//! returns what the row holds and the caller says what it wants, because a
//! query that quietly coerced a blob into a string would be a bug that only
//! showed up in production data.

#![forbid(unsafe_code)]
#![deny(missing_docs)]
#![deny(clippy::indexing_slicing)]
#![deny(clippy::unwrap_used)]
#![deny(clippy::expect_used)]
#![deny(clippy::panic)]
// Tests assert on exact values and are allowed to fail loudly; the bans above
// exist to keep panics out of the path a caller's query takes.
#![cfg_attr(
    test,
    allow(
        clippy::expect_used,
        clippy::indexing_slicing,
        clippy::panic,
        clippy::unwrap_used
    )
)]

use std::path::Path;

pub use rustdb_base::{DbError, DbResult, ExtendedCode, PrimaryCode};
pub use rustdb_session::{Backup, BackupProgress, Blob, ColumnMetadata};
pub use rustdb_value::{Affinity, Collation, StorageClass, TextEncoding, Value};

use rustdb_session::{
    Connection as SessionConnection, OpenOptions, SessionDatabase, Statement as SessionStatement,
};

/// A database file.
pub struct Database {
    inner: SessionDatabase,
}

impl Database {
    /// Opens a database file read-only.
    pub fn open(path: impl AsRef<Path>) -> DbResult<Database> {
        Ok(Database {
            inner: SessionDatabase::open(path)?,
        })
    }

    /// Opens a database file, retrying a busy lock for a while.
    ///
    /// SQLite's default is to give up at once, and so is this crate's; a caller
    /// that would rather wait says so here.
    pub fn open_with_busy_timeout(
        path: impl AsRef<Path>,
        timeout: std::time::Duration,
    ) -> DbResult<Database> {
        let mut options = OpenOptions::default();
        options.busy_timeout = timeout;
        Database::open_with(path, options)
    }

    /// Opens a database file with explicit options.
    pub fn open_with(path: impl AsRef<Path>, options: OpenOptions) -> DbResult<Database> {
        Ok(Database {
            inner: SessionDatabase::open_with_options(path, options)?,
        })
    }

    /// Opens a database from the bytes of one.
    ///
    /// The bytes are the file: what `Connection::serialize` produced, or what
    /// any engine wrote to disk. Nothing is copied out of them into a temporary
    /// file - the database lives in the memory they were read into, which is
    /// what makes this worth having over writing them to a path.
    pub fn deserialize(bytes: &[u8]) -> DbResult<Database> {
        Database::deserialize_with(bytes, OpenOptions::default())
    }

    /// Opens a database from bytes with explicit options.
    pub fn deserialize_with(bytes: &[u8], options: OpenOptions) -> DbResult<Database> {
        Ok(Database {
            inner: rustdb_session::Deserialized::open(bytes, options)?.into_database(),
        })
    }

    /// Opens a connection onto the database.
    pub fn connect(&self) -> DbResult<Connection> {
        Ok(Connection {
            inner: self.inner.connect()?,
        })
    }
}

/// One connection: transaction state, PRAGMA state, and prepared statements.
pub struct Connection {
    inner: SessionConnection,
}

impl Connection {
    /// Prepares one statement, returning it and the byte offset of the tail.
    pub fn prepare_with_tail<'connection>(
        &'connection self,
        sql: &str,
    ) -> DbResult<(Statement<'connection>, usize)> {
        let (statement, consumed) = SessionStatement::prepare(&self.inner, sql.as_bytes())?;
        Ok((Statement { inner: statement }, consumed))
    }

    /// Prepares one statement, ignoring anything after it.
    pub fn prepare<'connection>(&'connection self, sql: &str) -> DbResult<Statement<'connection>> {
        Ok(self.prepare_with_tail(sql)?.0)
    }

    /// Runs every statement in a script, discarding the rows.
    pub fn execute_batch(&self, sql: &str) -> DbResult<()> {
        rustdb_session::statement::execute_batch(&self.inner, sql.as_bytes())
    }

    /// Runs one query and collects every row.
    ///
    /// This is the convenience form. It owns every value it returns, so the
    /// caller is free of the step lifetime; a query over a large table should
    /// step instead.
    pub fn query(&self, sql: &str) -> DbResult<Vec<Vec<Value<'static>>>> {
        let mut statement = self.prepare(sql)?;
        let mut rows = Vec::new();
        while statement.step()? {
            rows.push(statement.row().to_vec());
        }
        Ok(rows)
    }

    /// Copies this connection's main database into another connection's.
    ///
    /// The whole thing, in one call. A caller that wants to copy a large
    /// database a few pages at a time - so that the process stays responsive,
    /// or so that it can be abandoned - uses `rustdb_session::Backup` directly.
    pub fn backup_into(&self, destination: &Connection) -> DbResult<()> {
        let mut backup = rustdb_session::Backup::begin(&self.inner, 0, &destination.inner, 0)?;
        while !backup.step(64)?.is_complete() {}
        backup.finish()
    }

    /// Returns the main database's bytes, exactly as the file holds them.
    pub fn serialize(&self) -> DbResult<Vec<u8>> {
        rustdb_session::serialize(&self.inner, 0)
    }

    /// Opens a handle on one value of one row.
    ///
    /// The handle reads and writes ranges of that value without materialising
    /// it, which is what makes a hundred-megabyte blob usable: one byte of it
    /// costs one page rather than the whole value.
    pub fn blob_open<'connection>(
        &'connection self,
        database: &str,
        table: &str,
        column: &str,
        rowid: i64,
        writable: bool,
    ) -> DbResult<rustdb_session::Blob<'connection>> {
        rustdb_session::Blob::open(
            &self.inner,
            database.as_bytes(),
            table.as_bytes(),
            column.as_bytes(),
            rowid,
            writable,
        )
    }

    /// Asks the running statement to stop at its next safe point.
    pub fn interrupt(&self) {
        self.inner.interrupt();
    }

    /// Installs the callback a long statement is asked to stop by.
    ///
    /// `every` is how many virtual-machine instructions pass between two
    /// calls, and the callback returning `true` stops the statement with
    /// `SQLITE_INTERRUPT`. It is how a single-threaded application abandons a
    /// query that is taking too long, and it takes effect for statements
    /// prepared after it is installed.
    pub fn set_progress_handler(
        &self,
        every: u64,
        handler: Option<std::sync::Arc<dyn Fn() -> bool + Send + Sync>>,
    ) {
        self.inner.set_progress_handler(every, handler);
    }

    /// Clears a pending interrupt.
    pub fn clear_interrupt(&self) {
        self.inner.clear_interrupt();
    }

    /// Returns whether the connection is in autocommit mode.
    pub fn autocommit(&self) -> bool {
        self.inner.autocommit()
    }

    /// Returns how many rows the most recent completed statement changed.
    pub fn changes(&self) -> i64 {
        self.inner.counters().changes
    }

    /// Returns how many rows the connection has changed since it opened.
    pub fn total_changes(&self) -> i64 {
        self.inner.counters().total_changes
    }

    /// Returns the rowid the most recent successful insert allocated.
    pub fn last_insert_rowid(&self) -> i64 {
        self.inner.counters().last_insert_rowid
    }

    /// Sets the callback fired once per row changed, returning the old one.
    ///
    /// The hook is told what happened - the operation, the database, the table
    /// and the rowid - and cannot change it. It must not run SQL on this
    /// connection: the statement that called it has not finished.
    pub fn set_update_hook(
        &self,
        hook: Option<rustdb_session::UpdateHook>,
    ) -> Option<rustdb_session::UpdateHook> {
        self.inner.set_update_hook(hook)
    }

    /// Sets the callback fired before a commit, returning the old one.
    ///
    /// Returning `true` vetoes the commit, which becomes a rollback rather
    /// than an error.
    pub fn set_commit_hook(
        &self,
        hook: Option<rustdb_session::CommitHook>,
    ) -> Option<rustdb_session::CommitHook> {
        self.inner.set_commit_hook(hook)
    }

    /// Sets the callback fired after a rollback, returning the old one.
    pub fn set_rollback_hook(
        &self,
        hook: Option<rustdb_session::RollbackHook>,
    ) -> Option<rustdb_session::RollbackHook> {
        self.inner.set_rollback_hook(hook)
    }

    /// Returns what the journal has cost since the connection was opened.
    pub fn journal_stats(&self) -> rustdb_storage::JournalStats {
        self.inner.journal_stats()
    }

    /// Returns what the pager has done since the connection was opened.
    pub fn pager_counters(&self) -> rustdb_storage::pager::PagerCounters {
        self.inner.pager_counters()
    }

    /// Rereads the schema, invalidating every prepared statement.
    pub fn reload_schema(&self) -> DbResult<()> {
        self.inner.reload_catalog()
    }

    /// Returns the schema cookie of an attached database.
    pub fn schema_cookie(&self, database: usize) -> DbResult<u32> {
        self.inner.schema_cookie(database)
    }

    /// Returns the catalog snapshot statements are compiled against.
    ///
    /// This is what `PRAGMA table_info` will read once PRAGMAs exist; until
    /// then it is how a caller sees the schema the engine actually loaded.
    pub fn catalog(&self) -> DbResult<std::sync::Arc<rustdb_catalog::CatalogSnapshot>> {
        self.inner.catalog()
    }
}

/// A prepared statement.
pub struct Statement<'connection> {
    inner: SessionStatement<'connection>,
}

impl Statement<'_> {
    /// Binds a value to a one-based parameter index.
    pub fn bind(&mut self, index: u32, value: Value<'static>) -> DbResult<()> {
        self.inner.bind(index, value)
    }

    /// Binds an integer.
    pub fn bind_integer(&mut self, index: u32, value: i64) -> DbResult<()> {
        self.inner.bind(index, Value::Integer(value))
    }

    /// Binds a real.
    pub fn bind_real(&mut self, index: u32, value: f64) -> DbResult<()> {
        self.inner.bind(index, Value::Real(value))
    }

    /// Binds text.
    pub fn bind_text(&mut self, index: u32, value: &str) -> DbResult<()> {
        self.inner.bind(index, Value::owned_text(value.as_bytes())?)
    }

    /// Binds a blob.
    pub fn bind_blob(&mut self, index: u32, value: &[u8]) -> DbResult<()> {
        self.inner.bind(index, Value::owned_blob(value)?)
    }

    /// Binds NULL.
    pub fn bind_null(&mut self, index: u32) -> DbResult<()> {
        self.inner.bind(index, Value::Null)
    }

    /// Clears every binding back to NULL.
    pub fn clear_bindings(&mut self) {
        self.inner.clear_bindings();
    }

    /// Steps the statement, returning whether a row is available.
    pub fn step(&mut self) -> DbResult<bool> {
        self.inner.step()
    }

    /// Returns the current row.
    pub fn row(&self) -> &[Value<'static>] {
        self.inner.row()
    }

    /// Returns one column of the current row.
    pub fn value(&self, index: usize) -> Value<'static> {
        self.inner.value(index)
    }

    /// Returns one column of the current row as an integer, if it is one.
    pub fn value_integer(&self, index: usize) -> Option<i64> {
        self.inner.row().get(index).and_then(Value::as_integer)
    }

    /// Returns one column of the current row as UTF-8 text, if it is text.
    pub fn value_text(&self, index: usize) -> Option<String> {
        let value = self.inner.row().get(index)?;
        let text = value.as_text()?;
        Some(String::from_utf8_lossy(&text.utf8_bytes()).into_owned())
    }

    /// Returns the next row, or `None` when the statement is finished.
    pub fn next_row(&mut self) -> DbResult<Option<Row<'_>>> {
        if !self.inner.step()? {
            return Ok(None);
        }
        Ok(Some(Row {
            values: self.inner.row(),
        }))
    }

    /// Returns the statement's result columns.
    pub fn columns(&self) -> &[ColumnMetadata] {
        self.inner.columns()
    }

    /// Returns how many columns the statement returns.
    pub fn column_count(&self) -> usize {
        self.inner.column_count()
    }

    /// Returns the name of one result column.
    pub fn column_name(&self, index: usize) -> Option<&[u8]> {
        self.inner
            .columns()
            .get(index)
            .map(|column| column.name.as_slice())
    }

    /// Returns whether the statement writes.
    pub fn is_readonly(&self) -> bool {
        self.inner.is_readonly()
    }

    /// Returns the bytecode the statement compiled to, as `EXPLAIN` renders it.
    pub fn explain(&self) -> Vec<String> {
        self.inner.program().explain()
    }

    /// Returns how many instructions the statement has run.
    pub fn steps(&self) -> u64 {
        self.inner.steps()
    }

    /// Resets the statement so it can run again, keeping its bindings.
    pub fn reset(&mut self) -> DbResult<()> {
        self.inner.reset()
    }

    /// Finalises the statement.
    pub fn finalize(self) -> DbResult<()> {
        self.inner.finalize()
    }
}

/// One row, borrowed for the life of the step that produced it.
pub struct Row<'step> {
    values: &'step [Value<'static>],
}

impl Row<'_> {
    /// Returns how many columns the row has.
    pub fn len(&self) -> usize {
        self.values.len()
    }

    /// Returns whether the row has no columns.
    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }

    /// Returns one column.
    pub fn get(&self, index: usize) -> Option<&Value<'static>> {
        self.values.get(index)
    }

    /// Returns one column as an integer, if it is one.
    pub fn integer(&self, index: usize) -> Option<i64> {
        self.values.get(index).and_then(Value::as_integer)
    }

    /// Returns one column as a real, if it is one.
    pub fn real(&self, index: usize) -> Option<f64> {
        self.values.get(index).and_then(Value::as_real)
    }

    /// Returns one column as UTF-8 text, if it is text.
    pub fn text(&self, index: usize) -> Option<String> {
        let value = self.values.get(index)?;
        let text = value.as_text()?;
        Some(String::from_utf8_lossy(&text.utf8_bytes()).into_owned())
    }

    /// Returns one column as blob bytes, if it is a blob.
    pub fn blob(&self, index: usize) -> Option<&[u8]> {
        self.values
            .get(index)
            .and_then(Value::as_blob)
            .map(|blob| blob.raw())
    }

    /// Returns whether one column is NULL.
    pub fn is_null(&self, index: usize) -> bool {
        self.values.get(index).is_none_or(Value::is_null)
    }
}

pub use rustdb_session::connection::OpenOptions as ConnectionOptions;

/// The implementation phase that filled this crate in, as named by the TDD.
pub const IMPLEMENTATION_PHASE: &str =
    "phase 6: catalog, binder, expression VM, and read-only SELECT";
