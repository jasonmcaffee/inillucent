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

pub use inillucent_base::{DbError, DbResult, ExtendedCode, PrimaryCode};
/// The collation registry, for an application that defines one.
pub use inillucent_session::collation;
/// What an application registers a function as.
pub use inillucent_session::extensions;
/// The file-system contract, for an application that supplies its own.
pub use inillucent_session::vfs;
pub use inillucent_session::{
    Backup, BackupProgress, Blob, ColumnMetadata, CommitHook, RollbackHook, RowChangeKind,
    UpdateHook,
};
pub use inillucent_value::{cast, Affinity, Collation, StorageClass, TextEncoding, Value};

/// The planner optimizations [`Connection::disable_optimizations`] can switch
/// off, so a caller naming one does not have to depend on the SQL crate.
pub use inillucent_session::Levers;

use inillucent_session::{
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
        let options = OpenOptions {
            busy_timeout: timeout,
            ..OpenOptions::default()
        };
        Database::open_with(path, options)
    }

    /// Opens a database file with explicit options.
    pub fn open_with(path: impl AsRef<Path>, options: OpenOptions) -> DbResult<Database> {
        Ok(Database {
            inner: SessionDatabase::open_with_options(path, options)?,
        })
    }

    /// Opens a database file through a file system the caller supplies.
    ///
    /// This is what a registered VFS is *for*: everything else about a database
    /// stays the same, and the bytes go somewhere the application chose.
    pub fn open_with_vfs(
        path: impl AsRef<Path>,
        options: OpenOptions,
        file_system: std::sync::Arc<dyn vfs::Vfs>,
    ) -> DbResult<Database> {
        Ok(Database {
            inner: SessionDatabase::open_with(path, file_system, options)?,
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
            inner: inillucent_session::Deserialized::open(bytes, options)?.into_database(),
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
        inillucent_session::statement::execute_batch(&self.inner, sql.as_bytes())
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
    /// or so that it can be abandoned - uses `inillucent_session::Backup` directly.
    pub fn backup_into(&self, destination: &Connection) -> DbResult<()> {
        let mut backup = inillucent_session::Backup::begin(&self.inner, 0, &destination.inner, 0)?;
        while !backup.step(64)?.is_complete() {}
        backup.finish()
    }

    /// Begins a page-at-a-time copy into another connection's database.
    ///
    /// `backup_into` is the whole-thing convenience; this is the form an
    /// application needs when the copy has to be interleaved with other work,
    /// abandoned, or reported on. The databases are named by position - zero is
    /// `main` - which is what the catalog numbers them by.
    pub fn backup_begin<'a>(
        &'a self,
        source_database: usize,
        destination: &'a Connection,
        destination_database: usize,
    ) -> DbResult<inillucent_session::Backup<'a>> {
        inillucent_session::Backup::begin(
            &self.inner,
            source_database,
            &destination.inner,
            destination_database,
        )
    }

    /// Returns the main database's bytes, exactly as the file holds them.
    pub fn serialize(&self) -> DbResult<Vec<u8>> {
        inillucent_session::serialize(&self.inner, 0)
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
    ) -> DbResult<inillucent_session::Blob<'connection>> {
        inillucent_session::Blob::open(
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

    /// Returns the flag `interrupt` sets, for a caller on another thread.
    ///
    /// A connection is not `Send`, and an interrupt is only ever useful from
    /// somewhere else - the whole point is that the thread running the
    /// statement is busy. The flag is the part that crosses, which is the same
    /// shape `sqlite3_interrupt` has: the caller holds something that outlives
    /// the call and refers to the connection without owning it.
    pub fn interrupt_flag(&self) -> std::sync::Arc<std::sync::atomic::AtomicBool> {
        self.inner.interrupt_flag()
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

    /// Registers a scalar function, replacing one of the same name and arity.
    ///
    /// It is `direct-only` unless the flags say otherwise: a schema is data,
    /// and data does not get to choose what code runs.
    pub fn create_scalar_function(
        &self,
        name: &str,
        arity: i32,
        flags: extensions::FunctionFlags,
        body: extensions::ScalarBody,
    ) -> DbResult<()> {
        self.inner.create_scalar_function(name, arity, flags, body)
    }

    /// Registers an aggregate, replacing one of the same name and arity.
    ///
    /// It is handed the whole group at once rather than a row at a time, which
    /// is what lets an implementation keep its accumulator somewhere this
    /// engine cannot see - a C `xStep`/`xFinal` pair, for instance.
    pub fn create_aggregate_function(
        &self,
        name: &str,
        arity: i32,
        flags: extensions::FunctionFlags,
        body: extensions::AggregateBody,
    ) -> DbResult<()> {
        self.inner
            .create_aggregate_function(name, arity, flags, body)
    }

    /// Removes a function by name and arity, reporting whether one went.
    pub fn remove_function(&self, name: &str, arity: i32) -> DbResult<bool> {
        self.inner.remove_function(name, arity)
    }

    /// Defines a collating sequence, replacing one of the same name.
    pub fn create_collation(&self, name: &str, comparator: collation::Comparator) -> DbResult<()> {
        self.inner.create_collation(name, comparator)
    }

    /// Returns whether the connection may write.
    pub fn is_writable(&self) -> bool {
        self.inner.is_writable()
    }

    /// Switches planner optimizations off, by mask, for A/B measurement.
    ///
    /// The mask names what to *disable*, so zero - the default - is the
    /// shipped engine. The names are on [`inillucent_sql::plan::Levers`]. This is
    /// the measurement channel, not a tuning surface: it is deliberately not
    /// reachable from SQL, for the same reason SQLite puts its equivalent
    /// behind `sqlite3_test_control` rather than behind a pragma.
    ///
    /// Statements already prepared keep the arm they were compiled under.
    /// @param mask - the levers to turn off
    pub fn disable_optimizations(&self, mask: u32) {
        self.inner.disable_optimizations(mask);
    }

    /// Bounds how many frames one automatic checkpoint copies.
    ///
    /// `None`, the default, copies as many as are safe, which is what the
    /// reference does. See [`inillucent_session::connection::Connection::set_checkpoint_budget`].
    /// @param budget - the cap, or `None` for no cap
    pub fn set_checkpoint_budget(&self, budget: Option<u32>) -> DbResult<()> {
        self.inner.set_checkpoint_budget(budget)
    }

    /// Returns the write-ahead log's running totals.
    ///
    /// The counters a measurement of the log is read against: a benchmark that
    /// reports a timing difference without them cannot say whether the thing it
    /// changed ever happened.
    pub fn wal_stats(&self) -> inillucent_storage::wal::WalStats {
        self.inner.wal_stats()
    }

    /// Returns which planner optimizations this connection has switched off.
    /// Returns how many compiled programs this connection is holding.
    ///
    /// The plan cache is an implementation detail with one externally visible
    /// property - it must never change an answer - and the tests that assert
    /// that need to see whether a program was actually kept. Exposing the count
    /// and nothing else keeps the cache an implementation detail while making
    /// the property testable.
    pub fn cached_plan_count(&self) -> usize {
        self.inner.cached_plan_count()
    }

    /// Drops every compiled program this connection is holding.
    pub fn invalidate_plan_cache(&self) {
        self.inner.invalidate_plan_cache();
    }

    /// Returns the planner optimizations this connection has switched off.
    pub fn disabled_optimizations(&self) -> u32 {
        self.inner.disabled_optimizations()
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
        hook: Option<inillucent_session::UpdateHook>,
    ) -> Option<inillucent_session::UpdateHook> {
        self.inner.set_update_hook(hook)
    }

    /// Sets the callback fired before a commit, returning the old one.
    ///
    /// Returning `true` vetoes the commit, which becomes a rollback rather
    /// than an error.
    pub fn set_commit_hook(
        &self,
        hook: Option<inillucent_session::CommitHook>,
    ) -> Option<inillucent_session::CommitHook> {
        self.inner.set_commit_hook(hook)
    }

    /// Sets the callback fired after a rollback, returning the old one.
    pub fn set_rollback_hook(
        &self,
        hook: Option<inillucent_session::RollbackHook>,
    ) -> Option<inillucent_session::RollbackHook> {
        self.inner.set_rollback_hook(hook)
    }

    /// Returns what the journal has cost since the connection was opened.
    pub fn journal_stats(&self) -> inillucent_storage::JournalStats {
        self.inner.journal_stats()
    }

    /// Returns what the pager has done since the connection was opened.
    pub fn pager_counters(&self) -> inillucent_storage::pager::PagerCounters {
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
    pub fn catalog(&self) -> DbResult<std::sync::Arc<inillucent_catalog::CatalogSnapshot>> {
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

    /// Returns the statement's own SQL text, without any tail.
    pub fn sql(&self) -> &[u8] {
        self.inner.sql()
    }

    /// Returns how many bytes of the prepared text this statement occupied,
    /// its semicolon included.
    pub fn sql_used(&self) -> usize {
        self.inner.sql_used()
    }

    /// Returns the highest parameter index the statement uses.
    pub fn parameter_count(&self) -> u32 {
        self.inner.parameter_count()
    }

    /// Returns each named parameter and the index it was assigned.
    pub fn parameter_names(&self) -> &[(Vec<u8>, u32)] {
        self.inner.parameter_names()
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

    /// Returns how many instructions the compiled program holds.
    ///
    /// The measurement the bytecode fold is read against: an arm that changed
    /// no instruction count folded nothing, whatever its timing said.
    pub fn instruction_count(&self) -> usize {
        self.inner.program().instructions.len()
    }

    /// Returns which planner optimizations this statement's plan used.
    ///
    /// A bitmask of [`Levers`] names. It is the observation the A/B arms are
    /// read against: a workload whose plans report the same mask under both
    /// arms measured nothing, however different its two timings came out.
    pub fn optimizations_used(&self) -> u32 {
        self.inner.program().optimizations_used
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

pub use inillucent_session::connection::OpenOptions as ConnectionOptions;

/// The implementation phase that filled this crate in, as named by the TDD.
pub const IMPLEMENTATION_PHASE: &str =
    "phase 6: catalog, binder, expression VM, and read-only SELECT";
