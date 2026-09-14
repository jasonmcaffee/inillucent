//! The inillucent driver: what an application uses to talk to the engine.
//!
//! Invariant: **this crate holds every decision and depends on
//! `inillucent-engine` and nothing else.** That single edge is the whole reason
//! it exists. `crates/unluminous-db` in the Unluminous repository is the first
//! consumer, and a consumer that reached into `inillucent-pool`,
//! `inillucent-tree` or `inillucent-exec` would be built on ground the
//! rearchitecture is still moving: four more crates are scheduled for deletion
//! and a fifth for absorption. Everything above this line is unaffected by all
//! of it.
//!
//! `inillucent-driver-capi` is a marshalling layer over this crate that decides
//! nothing, and is what every language that is not Rust binds to. Rust does not
//! go through it: DuckDB routes its own Rust binding through its C ABI because
//! its core is C++, and ours is Rust, so doing the same would be paying that
//! cost without that reason.
//!
//! ## The one thing to read before using it
//!
//! **This engine is deliberately incomplete, and [`capability`] is how you find
//! out what it will not do.** It cannot enforce a foreign key, answer a `LEFT
//! JOIN`, run a recursive CTE, use a derived table in `FROM`, register a
//! function, or stop a running statement. It refuses those rather than
//! answering them wrongly, and a refusal arrives as [`Status::Unsupported`]
//! with [`Error::feature`] naming the construct - which is a different thing
//! from the [`Status::Syntax`] a mistyped statement gets, and the distinction
//! is the point of the driver.
//!
//! `foreign_keys` is the one that needs saying twice, because it fails
//! *silently*: a violating write succeeds. Check the constraint in the
//! application until `capability::supports("foreign_keys")` answers
//! `Support::Yes`.
//!
//! ## Threads
//!
//! One file is one buffer pool and the engine is single threaded, so a
//! [`Database`] is neither `Send` nor `Sync`, and this is a contract rather than
//! an oversight: two connections holding two pools over one set of bytes would
//! be two page caches over one file. Two databases on two files are
//! independent.

#![forbid(unsafe_code)]
#![deny(missing_docs)]
#![deny(clippy::indexing_slicing)]
#![deny(clippy::unwrap_used)]
#![deny(clippy::expect_used)]
#![deny(clippy::panic)]
#![cfg_attr(
    test,
    allow(
        clippy::expect_used,
        clippy::indexing_slicing,
        clippy::panic,
        clippy::unwrap_used
    )
)]

pub mod capability;
pub mod error;
pub mod introspect;
pub mod rows;
pub mod value;

use std::cell::Cell;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Instant;

use inillucent_engine::base::budget::{self, Limits};
use inillucent_engine::connect::{Database as EngineDatabase, Statement as EngineStatement};

pub use capability::{capability, supports, Capability, Support, CAPABILITIES};
pub use error::{Error, Result, Status};
pub use introspect::{Item, Kind, Table};
pub use rows::Rows;
pub use value::{Column, Value, ValueKind};

/// What one statement may spend, and the default ceiling on the plan cache.
///
/// Re-exported so that a caller setting [`OpenOptions::limits`] or
/// [`OpenOptions::statement_cache`] does not have to name `inillucent_engine`
/// to build the value it is setting (task-1932, M1). The driver's README calls
/// it the one surface, and a surface a caller has to reach past is not one.
pub use inillucent_engine::base::budget::Limits as StatementLimits;
pub use inillucent_engine::DEFAULT_STATEMENT_CACHE;

/// Arming a statement budget, for a front end that runs statements itself.
///
/// The shell and the MCP server both arm one per call rather than going through
/// [`Connection::query`], because they drive the engine's own statement surface
/// for the dot commands and the pragmas. Re-exported so they do not have to
/// name `inillucent_engine` to do it (task-1932, M1).
///
/// `arm` clears the cancellation flag first, which is right for a caller that
/// arms once per call; `arm_as_it_stands` does not, which is what a caller that
/// reads its input on a second thread needs. See `budget::arm_as_it_stands`.
pub use inillucent_engine::base::budget::{arm, arm_as_it_stands, Guard as BudgetGuard};

/// The driver's own version, and the engine's beneath it.
pub const VERSION: &str = concat!(
    "inillucent-driver ",
    env!("CARGO_PKG_VERSION"),
    " (engine ",
    env!("CARGO_PKG_VERSION"),
    ")"
);

/// Returns what this driver calls itself.
///
/// The string a consumer shows in a Test Connection dialog. It names the driver
/// and the engine separately because they are separately versioned even when
/// the numbers agree today.
pub fn version() -> &'static str {
    VERSION
}

/// How a database is opened.
// **No longer `Copy`**, because `limits` holds an optional `Duration` inside a
// struct that may grow. A caller that passed `OpenOptions` twice now clones it,
// which is what every caller in this workspace already did by writing it out.
#[derive(Clone, Debug)]
pub struct OpenOptions {
    /// Create the file when there is nothing at the path. Default `true`.
    pub create: bool,
    /// Refuse any statement that is not a query. Default `false`; see
    /// [`Connection::execute`] for exactly what it refuses and what it does
    /// not.
    pub read_only: bool,
    /// How many frames the buffer pool holds. Default 4,096, which is 128 MiB
    /// at the engine's 32 KiB page.
    pub cache_frames: usize,
    /// Let an error carry the engine's internal diagnostic text.
    ///
    /// Default `false`, and off for a reason rather than out of caution:
    /// `inillucent-base` promises that an error's *message* never holds a
    /// file-system path or a bound value, and puts everything that does into
    /// the detail. A caller that turns this on is asking for text it must not
    /// show a person or send to a shared log.
    pub diagnostics: bool,
    /// What one statement on this database may spend.
    ///
    /// **Unbounded by default, and that is deliberate.** An application that
    /// has linked this engine into its own process is not protecting itself
    /// from itself, and a library that refused its owner's query at ten million
    /// rows would be a library with an opinion about the application's data. A
    /// *server* handing a database to somebody else is the case that needs a
    /// bound, and `inillucent_base::budget::Limits::served` is what it asks
    /// for - which is what `inillucent-mcp` does.
    pub limits: Limits,
    /// How many compiled statements one connection holds before its plan cache
    /// is emptied.
    ///
    /// **Bounded, where it used to grow for the life of the process
    /// (task-1932, M1).** The engine caches a compiled plan per statement text
    /// and cleared it only on a schema change, so an application issuing
    /// generated SQL - a query builder, a reporting tool, anything that puts a
    /// literal in the statement - kept one plan per distinct string and nothing
    /// measured it.
    ///
    /// Default `inillucent_engine::DEFAULT_STATEMENT_CACHE`, which is a
    /// thousand. Zero compiles every statement fresh.
    pub statement_cache: usize,
}

impl Default for OpenOptions {
    /// The defaults a caller gets from [`Database::open`].
    fn default() -> OpenOptions {
        OpenOptions {
            create: true,
            read_only: false,
            cache_frames: 4_096,
            diagnostics: false,
            limits: Limits::unbounded(),
            statement_cache: inillucent_engine::DEFAULT_STATEMENT_CACHE,
        }
    }
}

/// An open database file.
///
/// Neither `Send` nor `Sync`, by construction: it holds the engine, which holds
/// one buffer pool over one file.
pub struct Database {
    engine: EngineDatabase,
    path: PathBuf,
    options: OpenOptions,
    /// The flag [`Connection::cancel`] sets, read by the executor.
    ///
    /// One per database rather than one per statement, because a cancel arrives
    /// from a thread that does not hold the statement - that is the whole point
    /// of it - and the thing it can name is the database it was handed.
    /// Arming a budget clears it, so a cancel that arrives between statements
    /// stops nothing rather than stopping the next one.
    cancel: Arc<AtomicBool>,
}

impl std::fmt::Debug for Database {
    /// Names the file and the mode, and never anything a caller bound.
    fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        out.debug_struct("Database")
            .field("path", &self.path)
            .field("read_only", &self.options.read_only)
            .finish()
    }
}

impl Database {
    /// Opens a database, creating it when the path holds nothing.
    ///
    /// @param path - the database file
    pub fn open(path: impl AsRef<Path>) -> Result<Database> {
        Database::open_with(path, OpenOptions::default())
    }

    /// Opens a database with the options stated.
    ///
    /// **A file this engine did not write is not a file it opens.** File-format
    /// compatibility with SQLite is a non-goal of the rearchitecture, so a `.db`
    /// SQLite wrote reports that neither meta page is readable, which is true
    /// and is what it should say. [`Database::import_sqlite`] is the one route
    /// from one to the other.
    ///
    /// @param path - the database file
    /// @param options - how to open it
    pub fn open_with(path: impl AsRef<Path>, options: OpenOptions) -> Result<Database> {
        let path = path.as_ref().to_path_buf();
        if !options.create && !path.is_file() {
            return Err(Error::said(
                Status::NotFound,
                format!("there is no database at {}.", path.display()),
            ));
        }
        let engine = EngineDatabase::open_with(&path, options.cache_frames)
            .map_err(|error| Error::from_engine(&error, options.diagnostics))?;
        engine.set_statement_cache_limit(options.statement_cache);
        Ok(Database {
            engine,
            path,
            options,
            cancel: Arc::new(AtomicBool::new(false)),
        })
    }

    /// Reads a SQLite file and builds a inillucent database beside it.
    ///
    /// The rebuilt file sits at the source path with `.rdb` appended, and the
    /// source is never written to. Same rows, different bytes - which is the
    /// arrangement the rearchitecture chose when it dropped format
    /// compatibility.
    ///
    /// @param path - the SQLite database to read
    pub fn import_sqlite(path: impl AsRef<Path>) -> Result<Database> {
        let options = OpenOptions::default();
        let engine = EngineDatabase::import_with(path.as_ref(), options.cache_frames)
            .map_err(|error| Error::from_engine(&error, options.diagnostics))?;
        engine.set_statement_cache_limit(options.statement_cache);
        let path = engine.path().to_path_buf();
        Ok(Database {
            engine,
            path,
            options,
            cancel: Arc::new(AtomicBool::new(false)),
        })
    }

    /// Returns how many compiled statements this database is holding.
    ///
    /// **Nothing could ask before this (task-1932, M1).** The engine caches a
    /// compiled plan per statement text and emptied it only on a schema change,
    /// so the question "is this connection holding a plan per generated
    /// statement" had no answer that did not involve a debugger.
    pub fn cached_statements(&self) -> usize {
        self.engine.cached_statements()
    }

    /// Returns the ceiling the plan cache is emptied at.
    ///
    /// [`OpenOptions::statement_cache`] sets it.
    pub fn statement_cache_limit(&self) -> usize {
        self.engine.statement_cache_limit()
    }

    /// Forgets every compiled statement.
    ///
    /// A caller that has just issued a hundred thousand generated statements
    /// and wants the memory back has no other way to ask for it.
    pub fn clear_statement_cache(&self) {
        self.engine.clear_statement_cache();
    }

    /// Returns a connection to this database.
    ///
    /// **Each one is its own session**, and a session is what `temp.`, `ATTACH`
    /// and the connection pragmas are scoped to. A caller that keeps one
    /// connection needs nothing more; one that hands out a connection per call
    /// wants [`Database::connect_as`], or every `CREATE TEMP TABLE` is gone by
    /// the next statement.
    pub fn connect(&self) -> Connection<'_> {
        Connection {
            database: self,
            engine: self.engine.connect(),
            depth: Cell::new(0),
        }
    }

    /// Returns a connection that continues an earlier one's session.
    ///
    /// **For a caller that hands out a connection per call over one logical
    /// connection**, which is a shape this driver forces rather than a shape
    /// anyone chose: [`Connection`] borrows the `Database`, so a long-lived
    /// object cannot hold both - that is a self-referential struct, and Rust
    /// will not have it. Such a caller holds the `Database` and connects per
    /// call, and without this every one of those calls is a new session, so:
    ///
    /// - a `CREATE TEMP TABLE` typed into a query console is gone by the next
    ///   statement, and so is everything qualified `temp.`;
    /// - an `ATTACH` does not outlive the statement that made it;
    /// - a connection pragma has to be re-applied on every call.
    ///
    /// The engine has had this since it grew sessions; the driver did not pass
    /// it on until this method was added. Keep the number [`Connection::session`]
    /// returns and hand it back here, and every later connection is the same
    /// session.
    ///
    /// A number that no `connect` handed out is a session of its own rather
    /// than an error, which is the same thing the engine does with it: a
    /// session is a scope, not a resource, so there is nothing to have failed
    /// to find.
    ///
    /// @param session - the number an earlier connection reported
    pub fn connect_as(&self, session: u64) -> Connection<'_> {
        Connection {
            database: self,
            engine: self.engine.connect_as(session),
            depth: Cell::new(0),
        }
    }

    /// Returns the file this database is in.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Makes everything written so far durable in the file.
    ///
    /// A database dropped without this is not lost - opening it replays the log
    /// - but a checkpoint is what makes the next open cheap.
    pub fn checkpoint(&self) -> Result<()> {
        self.engine
            .checkpoint()
            .map_err(|error| self.classify(&error))
    }

    /// Walks every tree and reports the first thing that is wrong.
    pub fn integrity_check(&self) -> Result<()> {
        self.engine.check().map_err(|error| self.classify(&error))
    }

    /// Copies this database into a file, and checks the copy.
    ///
    /// The copy is opened and walked before this returns, because a backup
    /// nobody checked is a file that is *assumed* to be a database, and the cost
    /// of finding out otherwise is paid at the worst possible moment.
    ///
    /// @param path - where the copy goes
    pub fn backup_to(&self, path: impl AsRef<Path>) -> Result<()> {
        self.engine
            .backup_to(path.as_ref())
            .map_err(|error| self.classify(&error))
    }

    /// Turns an engine error into a driver error, honouring the open options.
    ///
    /// @param error - the engine's error
    fn classify(&self, error: &inillucent_engine::DbError) -> Error {
        Error::from_engine(error, self.options.diagnostics)
    }
}

/// A connection to a database.
///
/// It borrows the database rather than owning a handle of its own, because one
/// file is one pool.
pub struct Connection<'d> {
    database: &'d Database,
    engine: inillucent_engine::connect::Connection<'d>,
    /// How deep the caller is inside [`Connection::transaction`].
    ///
    /// Counted so a nested call is refused by name rather than committing an
    /// outer caller's work early, which is the shape of a bug nobody notices
    /// until something is half written.
    depth: Cell<u32>,
}

impl std::fmt::Debug for Connection<'_> {
    /// Names the database, and nothing a caller bound.
    fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        out.debug_struct("Connection")
            .field("path", &self.database.path)
            .finish()
    }
}

impl Connection<'_> {
    /// Returns the number that identifies this connection's session.
    ///
    /// Hand it to [`Database::connect_as`] and the connection that comes back
    /// continues this one: the same temporary tables, the same attached
    /// databases, the same connection pragmas.
    pub fn session(&self) -> u64 {
        self.engine.session()
    }

    /// Runs one statement and returns everything it produced.
    ///
    /// **`limit` cuts the rows handed back and not the rows produced**, because
    /// the engine materialises: see [`rows`] for why that makes
    /// [`Rows::total`] exact where a server-backed driver can only estimate,
    /// and what it costs.
    ///
    /// In read-only mode a statement that does not bind to a query is refused
    /// with [`Status::ReadOnly`] - including a `PRAGMA`, because a pragma that
    /// takes a value changes something. The classification is the binder's
    /// rather than a scan of the text, so it cannot be talked past by
    /// whitespace or a comment.
    ///
    /// @param sql - the statement
    /// @param params - the values bound to `?1`, `?2`, ...
    /// @param limit - how many rows to hand back
    pub fn query(&self, sql: &str, params: &[Value], limit: usize) -> Result<Rows> {
        if self.database.options.read_only {
            self.refuse_if_it_writes(sql)?;
        }
        self.run(sql, params, limit)
    }

    /// Runs one statement for its effect and returns how many rows it changed.
    ///
    /// @param sql - the statement
    /// @param params - the values bound to `?1`, `?2`, ...
    pub fn execute(&self, sql: &str, params: &[Value]) -> Result<u64> {
        Ok(self.query(sql, params, 0)?.affected.unwrap_or(0))
    }

    /// Runs several statements separated by semicolons, for their effect.
    ///
    /// @param sql - the statements
    pub fn execute_batch(&self, sql: &str) -> Result<()> {
        if self.database.options.read_only {
            return Err(Error::said(
                Status::ReadOnly,
                "this connection is read only, and a batch is for statements that change \
                 something.",
            ));
        }
        self.engine
            .execute_batch(sql)
            .map_err(|error| self.database.classify(&error))
    }

    /// Compiles a statement so it can be run more than once.
    ///
    /// @param sql - the statement
    pub fn prepare(&self, sql: &str) -> Result<Statement<'_>> {
        if self.database.options.read_only {
            self.refuse_if_it_writes(sql)?;
        }
        let engine = self
            .engine
            .prepare(sql)
            .map_err(|error| self.database.classify(&error))?;
        Ok(Statement {
            connection: self,
            engine,
            sql: sql.to_owned(),
        })
    }

    /// Runs a statement whose parameters are named, and returns its rows.
    ///
    /// **The engine has been able to answer this since it had parameters, and
    /// the driver could not ask (task-1932, M1).** `connect::parameter_names`
    /// returns every `:name`, `@name` and `$name` in a statement with the index
    /// it was assigned; the driver bound by position only, so an application
    /// with a statement of nine named parameters had to count them itself and
    /// keep the count right through every edit of the SQL. Getting it wrong is
    /// silent: the values land in the wrong columns and the statement succeeds.
    ///
    /// A name the statement does not use is refused rather than ignored, and so
    /// is a parameter the statement uses and the caller did not supply. Both are
    /// the same mistake seen from the two ends, and both are cheaper to hear
    /// about than to debug.
    ///
    /// @param sql - the statement
    /// @param params - the values, by the name each appears under in the SQL
    /// @param limit - how many rows to keep, or zero for every row
    pub fn query_named(&self, sql: &str, params: &[(&str, Value)], limit: usize) -> Result<Rows> {
        let positional = self.positions_for(sql, params)?;
        self.query(sql, &positional, limit)
    }

    /// Runs a statement whose parameters are named, for its effect.
    ///
    /// @param sql - the statement
    /// @param params - the values, by the name each appears under in the SQL
    pub fn execute_named(&self, sql: &str, params: &[(&str, Value)]) -> Result<u64> {
        Ok(self.query_named(sql, params, 0)?.affected.unwrap_or(0))
    }

    /// Returns the named values in the order the statement's markers are
    /// numbered.
    ///
    /// The one place a name becomes an index, so the two entry points above
    /// cannot disagree about what a missing name means.
    ///
    /// @param sql - the statement
    /// @param params - the values, by name
    fn positions_for(&self, sql: &str, params: &[(&str, Value)]) -> Result<Vec<Value>> {
        let declared = self
            .engine
            .parameter_names(sql)
            .map_err(|error| self.database.classify(&error))?;
        let mut ordered: Vec<Option<Value>> = vec![None; declared.len()];
        for (name, value) in params {
            let wanted = name.trim_start_matches([':', '@', '$', '?']);
            let found = declared.iter().find(|(declared_name, _)| {
                String::from_utf8_lossy(declared_name).trim_start_matches([':', '@', '$']) == wanted
            });
            let Some((_, index)) = found else {
                return Err(Error::said(
                    Status::InvalidState,
                    format!(
                        "this statement has no parameter called '{name}'. It has: {}",
                        named_list(&declared)
                    ),
                ));
            };
            let at = (*index as usize).saturating_sub(1);
            match ordered.get_mut(at) {
                Some(slot) => *slot = Some(value.clone()),
                None => {
                    return Err(Error::said(
                        Status::InvalidState,
                        format!("'{name}' is numbered {index}, past this statement's parameters."),
                    ))
                }
            }
        }
        let mut positional = Vec::with_capacity(ordered.len());
        for (at, slot) in ordered.into_iter().enumerate() {
            match slot {
                Some(value) => positional.push(value),
                None => {
                    let missing = declared
                        .iter()
                        .find(|(_, index)| *index as usize == at.saturating_add(1))
                        .map(|(name, _)| String::from_utf8_lossy(name).into_owned())
                        .unwrap_or_else(|| format!("?{}", at.saturating_add(1)));
                    return Err(Error::said(
                        Status::InvalidState,
                        format!("this statement uses '{missing}' and no value was given for it."),
                    ));
                }
            }
        }
        Ok(positional)
    }

    /// Describes how a statement would be run.
    ///
    /// The operator chain, which is what `EXPLAIN QUERY PLAN` answers. There is
    /// no bytecode listing because there is no bytecode.
    ///
    /// @param sql - the statement
    pub fn explain(&self, sql: &str) -> Result<Vec<String>> {
        self.engine
            .explain(sql)
            .map_err(|error| self.database.classify(&error))
    }

    /// Writes several statements as one transaction, or writes none of them.
    ///
    /// **`check` is handed in rather than run afterwards**, and that is the
    /// whole shape of the method rather than a convenience. A postcondition
    /// tested after the `COMMIT` is a report about something that has already
    /// happened; tested before it, it is a guard. The consumer's rule - every
    /// statement a row editor writes must change exactly one row - only means
    /// anything in the second form.
    ///
    /// Any failure, and any `check` that refuses, rolls the whole thing back.
    ///
    /// @param work - the statements and their bound values, in order
    /// @param check - what must be true of the changed-row counts before commit
    pub fn transaction<F>(&self, work: &[(String, Vec<Value>)], check: F) -> Result<Vec<u64>>
    where
        F: Fn(&[u64]) -> Result<()>,
    {
        if self.database.options.read_only {
            return Err(Error::said(
                Status::ReadOnly,
                "this connection is read only, and a transaction is for statements that \
                 change something.",
            ));
        }
        if self.depth.get() > 0 {
            return Err(Error::said(
                Status::InvalidState,
                "a transaction is already open on this connection; this driver does not nest \
                 them, because committing the inner one would commit the outer one's work \
                 too.",
            ));
        }
        let guard = self.begin()?;
        let outcome = guard.run_all(work, check);
        match outcome {
            Ok(affected) => {
                guard.commit()?;
                Ok(affected)
            }
            Err(why) => Err(why),
        }
    }

    /// Opens a transaction this caller drives statement by statement.
    ///
    /// **The escape `transaction` could not give (task-1932, M1).** The batch
    /// form takes every statement up front, which is the right shape for a row
    /// editor and the wrong one for anything that has to look at what it just
    /// wrote before deciding the next statement. The only way to do that was
    /// `execute_batch("BEGIN")`, which the nesting guard cannot see - so two
    /// callers doing it on one connection got one transaction and no warning,
    /// and a caller that returned early left it open for the life of the
    /// process.
    ///
    /// Dropping the returned value rolls back. That is the whole reason it is a
    /// value rather than a pair of methods: an early return, a `?`, or a panic
    /// leaves the database as it was rather than holding a write lock until the
    /// connection closes.
    pub fn begin(&self) -> Result<Transaction<'_>> {
        if self.database.options.read_only {
            return Err(Error::said(
                Status::ReadOnly,
                "this connection is read only, and a transaction is for statements that \
                 change something.",
            ));
        }
        if self.depth.get() > 0 {
            return Err(Error::said(
                Status::InvalidState,
                "a transaction is already open on this connection; this driver does not nest \
                 them, because committing the inner one would commit the outer one's work \
                 too.",
            ));
        }
        self.engine
            .execute_batch("BEGIN")
            .map_err(|error| self.database.classify(&error))?;
        self.depth.set(1);
        Ok(Transaction {
            connection: self,
            settled: Cell::new(false),
        })
    }

    /// Runs a statement with no read-only check, which the caller has done.
    ///
    /// @param sql - the statement
    /// @param params - the bound values
    /// @param limit - how many rows to keep
    fn run(&self, sql: &str, params: &[Value], limit: usize) -> Result<Rows> {
        let started = Instant::now();
        // Armed around the whole statement, including the prepare: a query
        // whose *planning* is what runs long is still a query that has to stop.
        let _budget = budget::arm(
            self.database.options.limits.clone(),
            Arc::clone(&self.database.cancel),
        );
        let mut statement = self
            .engine
            .prepare(sql)
            .map_err(|error| self.database.classify(&error))?;
        bind_all(&mut statement, params)?;
        collect(
            &mut statement,
            sql,
            limit,
            started,
            self.database.options.diagnostics,
        )
        .map_err(|error| self.database.classify(&error))
    }

    /// Refuses a statement that is not a query, for a read-only connection.
    ///
    /// It asks the engine to *plan* the statement, which succeeds only for a
    /// query - `plan` reports "is not a read-only statement" for anything else.
    /// So the classification is the binder's own and cannot be talked past by a
    /// comment, a case change or leading whitespace, which is what a scan of the
    /// text would fall to.
    ///
    /// @param sql - the statement
    fn refuse_if_it_writes(&self, sql: &str) -> Result<()> {
        match self.engine.explain(sql) {
            Ok(_) => Ok(()),
            Err(error) => {
                let classified = self.database.classify(&error);
                // A statement that failed to *plan* for any other reason - a
                // missing table, a construct the engine cannot run - is that
                // failure and not a read-only refusal, and reporting it as one
                // would tell a caller to reopen the file over a typo.
                match classified.status {
                    Status::Syntax if classified.message.contains("not a read-only statement") => {
                        Err(Error::said(
                            Status::ReadOnly,
                            "this connection is read only, and that statement changes something.",
                        ))
                    }
                    _ => Err(classified),
                }
            }
        }
    }

    /// Returns the rowid the last `INSERT` on this database assigned.
    pub fn last_insert_rowid(&self) -> i64 {
        self.engine.last_insert_rowid()
    }

    /// Returns how many rows every statement so far has changed.
    pub fn total_changes(&self) -> i64 {
        self.engine.total_changes()
    }

    /// Returns whether a transaction is open.
    pub fn in_transaction(&self) -> bool {
        !self.engine.autocommit()
    }

    /// Returns the schema's generation, which changes when the schema does.
    ///
    /// A consumer that caches a table's columns compares this to know whether
    /// the cache is stale, rather than re-reading the schema per statement.
    pub fn schema_cookie(&self) -> u64 {
        self.engine.schema_cookie()
    }

    /// Asks another thread to stop a running statement.
    ///
    /// **It sets a flag rather than stopping anything itself**, which is what
    /// makes it safe to call from another thread while a statement is running.
    /// The executor reads it at the two places a long statement passes through
    /// often enough to matter: every leaf of a scan, and every batch a result
    /// collects. The statement then fails with `Interrupted` and the connection
    /// stays usable.
    ///
    /// **What it does not do is stop a statement between those points.** A
    /// single enormous sort inside one operator runs to the end of that
    /// operator. That is a bound on the latency of a cancel, not on whether it
    /// works, and it is the honest description: the row this returns to
    /// `capability::supports("cancel")` is `Partial` for exactly this reason.
    ///
    /// A cancel with nothing running sets the flag, and arming the next
    /// statement's budget clears it - so it cancels nothing rather than
    /// cancelling whatever comes next.
    pub fn cancel(&self) -> Result<()> {
        self.database.cancel.store(true, Ordering::Relaxed);
        Ok(())
    }

    /// Registers a function an application wrote, callable from SQL by name.
    ///
    /// **The one place a caller's own code runs inside a statement**, so two
    /// things are true of it that are not true of the rest of this crate. The
    /// body is handed the arguments already evaluated and answers one value; it
    /// must not call back into the connection, because the statement that
    /// called it is part-way through running. And a registration throws away
    /// every compiled statement on the connection, because which function a
    /// name resolves to is decided when a statement is bound - so registering
    /// inside a loop is expensive in a way that is not visible at the call site.
    ///
    /// A body that fails returns an [`Error`]; the statement fails with it, and
    /// a transaction around it rolls back exactly as any other failure would.
    ///
    /// The flags are the engine's `external` defaults: the function may be
    /// called from a statement and not from a schema, which is the safe
    /// assumption about code the engine did not write. A `DEFAULT`, a `CHECK`,
    /// a generated column or a view cannot reach it.
    ///
    /// @param name - the name SQL calls it by
    /// @param arity - how many arguments it takes, or -1 for any number
    /// @param body - what it does
    pub fn create_scalar_function<F>(&self, name: &str, arity: i32, body: F) -> Result<()>
    where
        F: Fn(&[Value]) -> Result<Value> + Send + Sync + 'static,
    {
        let wrapped: inillucent_engine::ScalarBody = std::sync::Arc::new(move |arguments| {
            let given: Vec<Value> = arguments.iter().map(Value::from_expr).collect();
            match body(&given) {
                Ok(answer) => answer.to_expr(),
                Err(why) => Err(inillucent_engine::DbError::primary(
                    inillucent_engine::PrimaryCode::Error,
                )
                .with_message(why.message)),
            }
        });
        self.engine
            .create_scalar_function(
                name,
                arity,
                inillucent_engine::FunctionFlags::external(),
                wrapped,
            )
            .map_err(|error| self.database.classify(&error))
    }

    /// Removes a function this connection registered.
    ///
    /// @param name - the name it was registered under
    /// @param arity - the arity it was registered for
    pub fn remove_function(&self, name: &str, arity: i32) -> bool {
        self.engine.remove_function(name, arity)
    }

    /// Registers a collating sequence an application wrote.
    ///
    /// **A collation decides the order rows are stored in**, not merely the
    /// order they come back in: an index on a column declared `COLLATE` this is
    /// built with it. So a comparator that answers differently on two runs makes
    /// an index that disagrees with itself, and the engine has no way to detect
    /// that. It must be a total order and it must be stable.
    ///
    /// @param name - the name `COLLATE` calls it by
    /// @param comparator - how it orders two values' bytes
    pub fn create_collation<F>(&self, name: &str, comparator: F) -> Result<()>
    where
        F: Fn(&[u8], &[u8]) -> std::cmp::Ordering + Send + Sync + 'static,
    {
        let wrapped: inillucent_engine::Comparator = std::sync::Arc::new(comparator);
        self.engine
            .create_collation(name, wrapped)
            .map_err(|error| self.database.classify(&error))
    }

    // —— introspection ————————————————————————————————————————————

    /// Returns the schemas this database has, which is one.
    ///
    /// Answered as a list of one rather than as nothing, so a consumer drawing a
    /// tree has the same shape for every engine and never asks which it is
    /// drawing.
    pub fn schemas(&self) -> Result<Vec<String>> {
        Ok(vec!["main".to_owned()])
    }

    /// Returns everything in the schema.
    pub fn items(&self) -> Result<Vec<Item>> {
        let rows = self.internal(
            "SELECT type, name FROM sqlite_schema WHERE name NOT LIKE 'sqlite_%' \
             ORDER BY type, name",
            &[],
        )?;
        let mut items = Vec::with_capacity(rows.rows.len());
        for row in &rows.rows {
            let Some(kind) = row.first().and_then(Value::text).and_then(Kind::from_name) else {
                continue;
            };
            let Some(name) = row.get(1).and_then(Value::text) else {
                continue;
            };
            items.push(Item {
                name: name.to_owned(),
                kind,
            });
        }
        Ok(items)
    }

    /// Returns one table's columns and key.
    ///
    /// @param name - the table's name
    pub fn table(&self, name: &str) -> Result<Table> {
        let statement = format!("PRAGMA table_info({})", introspect::quoted(name));
        let rows = self.internal(&statement, &[])?;
        if rows.rows.is_empty() {
            return Err(Error::said(
                Status::NotFound,
                format!("`{name}` has no columns, or is not in this database."),
            ));
        }
        let mut table = Table {
            name: name.to_owned(),
            ..Table::default()
        };
        let mut key: Vec<(i64, String)> = Vec::new();
        for row in &rows.rows {
            let column_name = cell_text(row.get(1));
            let declared = cell_text(row.get(2));
            table.not_null.push(cell_integer(row.get(3)) == 1);
            let position = cell_integer(row.get(5));
            if position > 0 {
                key.push((position, column_name.clone()));
            }
            table.columns.push(Column::new(column_name, declared));
        }
        key.sort_by_key(|(position, _)| *position);
        table.key = key.into_iter().map(|(_, name)| name).collect();
        table.without_rowid = self.is_without_rowid(name);
        Ok(table)
    }

    /// Returns the `CREATE` statement a schema entry was made by.
    ///
    /// The original text, which the engine keeps, rather than something composed
    /// from a catalogue - so what a consumer shows is what somebody typed.
    ///
    /// @param name - the entry's name
    pub fn ddl(&self, name: &str) -> Result<String> {
        let rows = self.internal(
            "SELECT sql FROM sqlite_schema WHERE name = ?1",
            &[Value::Text(name.to_owned())],
        )?;
        rows.rows
            .first()
            .and_then(|row| row.first())
            .and_then(Value::text)
            .map(str::to_owned)
            .ok_or_else(|| {
                Error::said(
                    Status::NotFound,
                    format!("`{name}` is not in this database's schema."),
                )
            })
    }

    /// Reports whether a table has no implicit rowid to address a row by.
    ///
    /// Asked of the table rather than of its declaration, because `WITHOUT
    /// ROWID` is a property the schema text records and a selectable `rowid` is
    /// the property a caller actually needs. A table that answers the query has
    /// one.
    ///
    /// @param name - the table's name
    fn is_without_rowid(&self, name: &str) -> bool {
        let statement = format!("SELECT rowid FROM {} LIMIT 1", introspect::quoted(name));
        self.internal(&statement, &[]).is_err()
    }

    /// Runs a statement the driver composed, past the read-only check.
    ///
    /// The check is about the *caller's* statements. Introspection is the
    /// driver's own and is read-only by construction - every one of them is a
    /// `SELECT` or a reporting `PRAGMA` written in this file - so putting it
    /// through a rule meant for arbitrary text would refuse `PRAGMA table_info`
    /// on a read-only connection, which is the one mode that needs it most.
    ///
    /// @param sql - the statement
    /// @param params - the bound values
    fn internal(&self, sql: &str, params: &[Value]) -> Result<Rows> {
        self.run(sql, params, usize::MAX)
    }
}

/// An open transaction, which rolls back unless it is committed.
///
/// **The rollback is in `Drop`, and that is the point (task-1932, M1).** A
/// transaction driven statement by statement is driven by code with early
/// returns in it, and every one of those is a path where somebody has to
/// remember to roll back. Putting it in `Drop` means the only way to keep the
/// work is to say so.
///
/// A `?` inside the block, a `return`, a panic: all three leave the database as
/// it was. `commit()` is the one thing that does not.
///
/// ```no_run
/// # use inillucent_driver::{Database, Value, Result};
/// # fn main() -> Result<()> {
/// let database = Database::open("app.rdb")?;
/// let connection = database.connect();
/// let transaction = connection.begin()?;
/// transaction.execute("UPDATE account SET balance = balance - ?1 WHERE id = ?2", &[Value::Integer(50), Value::Integer(1)])?;
/// let moved = transaction.query("SELECT balance FROM account WHERE id = ?1", &[Value::Integer(1)], 1)?;
/// if moved.rows.first().and_then(|row| row.first()) == Some(&Value::Integer(0)) {
///     // Dropped without a commit: nothing above is kept.
///     return Ok(());
/// }
/// transaction.commit()?;
/// # Ok(())
/// # }
/// ```
#[derive(Debug)]
pub struct Transaction<'c> {
    /// The connection it is open on.
    connection: &'c Connection<'c>,
    /// Whether `commit` or `rollback` has already run, so `Drop` does nothing.
    settled: Cell<bool>,
}

impl Transaction<'_> {
    /// Runs a statement inside the transaction and returns its rows.
    ///
    /// @param sql - the statement
    /// @param params - the values bound to `?1`, `?2` and so on
    /// @param limit - how many rows to keep, or zero for every row
    pub fn query(&self, sql: &str, params: &[Value], limit: usize) -> Result<Rows> {
        self.connection.run(sql, params, limit)
    }

    /// Runs a statement inside the transaction for its effect.
    ///
    /// @param sql - the statement
    /// @param params - the values bound to `?1`, `?2` and so on
    pub fn execute(&self, sql: &str, params: &[Value]) -> Result<u64> {
        Ok(self.query(sql, params, 0)?.affected.unwrap_or(0))
    }

    /// Keeps everything this transaction wrote.
    ///
    /// Takes `self`, so a committed transaction cannot be used again and the
    /// `Drop` below cannot roll back what was kept.
    pub fn commit(self) -> Result<()> {
        self.settled.set(true);
        self.connection.depth.set(0);
        self.connection
            .engine
            .execute_batch("COMMIT")
            .map_err(|error| self.connection.database.classify(&error))
    }

    /// Discards everything this transaction wrote.
    ///
    /// The same thing dropping it does, said out loud. A caller that has
    /// decided to abandon the work reads better for saying so, and the error a
    /// failed rollback produces is reportable here and is not from `Drop`.
    pub fn rollback(self) -> Result<()> {
        self.settled.set(true);
        self.connection.depth.set(0);
        self.connection
            .engine
            .execute_batch("ROLLBACK")
            .map_err(|error| self.connection.database.classify(&error))
    }

    /// Runs a list of statements, rolling back on the first failure or on a
    /// refused postcondition.
    ///
    /// The body of [`Connection::transaction`], which is now this method with
    /// the open and the commit around it.
    ///
    /// @param work - the statements and their bound values, in order
    /// @param check - what must be true of the changed-row counts before commit
    fn run_all<F>(&self, work: &[(String, Vec<Value>)], check: F) -> Result<Vec<u64>>
    where
        F: Fn(&[u64]) -> Result<()>,
    {
        let mut affected = Vec::with_capacity(work.len());
        for (statement, values) in work {
            match self.connection.run(statement, values, 0) {
                Ok(rows) => affected.push(rows.affected.unwrap_or(0)),
                Err(why) => return Err(self.rolled_back(why)),
            }
        }
        if let Err(why) = check(&affected) {
            return Err(self.rolled_back(why));
        }
        Ok(affected)
    }

    /// Rolls back and hands back the reason it is rolling back.
    ///
    /// A failure to roll back is not swallowed: it replaces the reason, because
    /// a caller told only about the first failure would believe nothing was
    /// written.
    ///
    /// @param why - what went wrong
    fn rolled_back(&self, why: Error) -> Error {
        self.settled.set(true);
        self.connection.depth.set(0);
        match self.connection.engine.execute_batch("ROLLBACK") {
            Ok(()) => why,
            Err(error) => {
                let mut failed = self.connection.database.classify(&error);
                failed.message = format!(
                    "{} - and the rollback after it failed: {}. The database may hold a \
                     partial write.",
                    why.message, failed.message
                );
                failed
            }
        }
    }
}

impl Drop for Transaction<'_> {
    /// Rolls back an uncommitted transaction.
    ///
    /// **Silent, because a `Drop` has nowhere to report to.** The failure it
    /// could hide is a rollback that did not happen, and the thing that would
    /// have to happen for that is the engine refusing a `ROLLBACK` on a
    /// transaction it opened. The connection is dropped or reused immediately
    /// afterwards, and a reused one refuses the next `begin` because the depth
    /// is put back only on the paths that succeeded.
    ///
    /// A caller who wants to know calls `rollback()` and reads the answer.
    fn drop(&mut self) {
        if self.settled.get() {
            return;
        }
        self.settled.set(true);
        let _ = self.connection.engine.execute_batch("ROLLBACK");
        self.connection.depth.set(0);
    }
}

/// A statement compiled once and run more than once.
pub struct Statement<'c> {
    connection: &'c Connection<'c>,
    engine: EngineStatement<'c>,
    sql: String,
}

impl Statement<'_> {
    /// Runs the statement with these values bound.
    ///
    /// @param params - the values bound to `?1`, `?2`, ...
    /// @param limit - how many rows to hand back
    pub fn query(&mut self, params: &[Value], limit: usize) -> Result<Rows> {
        let started = Instant::now();
        self.engine.clear_bindings();
        bind_all(&mut self.engine, params)?;
        self.engine.reset();
        let diagnostics = self.connection.database.options.diagnostics;
        collect(&mut self.engine, &self.sql, limit, started, diagnostics)
            .map_err(|error| self.connection.database.classify(&error))
    }

    /// Returns the statement this was compiled from.
    pub fn sql(&self) -> &str {
        &self.sql
    }
}

impl std::fmt::Debug for Statement<'_> {
    /// Names the statement's text, which is the caller's own and holds no bound
    /// value.
    fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        out.debug_struct("Statement")
            .field("sql", &self.sql)
            .finish()
    }
}

/// Renders the parameter names a statement declares, for a refusal.
///
/// @param declared - the names and their indices, as the engine answered
fn named_list(declared: &[(Vec<u8>, u32)]) -> String {
    if declared.is_empty() {
        return "none".to_string();
    }
    declared
        .iter()
        .map(|(name, _)| String::from_utf8_lossy(name).into_owned())
        .collect::<Vec<String>>()
        .join(", ")
}

/// Binds every value in order, starting at `?1`.
///
/// @param statement - the engine statement
/// @param params - the values
fn bind_all(statement: &mut EngineStatement<'_>, params: &[Value]) -> Result<()> {
    for (nth, value) in params.iter().enumerate() {
        let index = u32::try_from(nth.saturating_add(1)).map_err(|_| {
            Error::said(
                Status::InvalidState,
                "that is more bound parameters than a statement can have.",
            )
        })?;
        statement
            .bind(index, value.to_engine())
            .map_err(|error| Error::from_engine(&error, false))?;
    }
    Ok(())
}

/// Steps a statement to the end and collects what it produced.
///
/// **The limit cuts what is handed back, never what is produced**, so `total` is
/// the exact count and `more` is a fact. See [`rows`].
///
/// @param statement - the engine statement
/// @param sql - the statement's text, for the tag
/// @param limit - how many rows to keep
/// @param started - when the call began
/// @param diagnostics - whether internal detail may be carried
fn collect(
    statement: &mut EngineStatement<'_>,
    sql: &str,
    limit: usize,
    started: Instant,
    diagnostics: bool,
) -> std::result::Result<Rows, inillucent_engine::DbError> {
    let _ = diagnostics;
    let mut kept: Vec<Vec<Value>> = Vec::new();
    let mut total = 0usize;
    while statement.step()? {
        total = total.saturating_add(1);
        if kept.len() < limit {
            kept.push(statement.row().iter().map(Value::from_engine).collect());
        }
    }
    let columns: Vec<Column> = statement
        .columns()
        .iter()
        .map(|name| Column::new(name.clone(), String::new()))
        .collect();
    let changed = statement.changes();
    // A query is a statement that named columns and changed nothing. An
    // `INSERT ... RETURNING` does both, which is why the count decides rather
    // than the shape.
    let affected = match columns.is_empty() || changed > 0 {
        true => Some(changed as u64),
        false => None,
    };
    let counted = match affected {
        Some(count) if columns.is_empty() => count as usize,
        _ => total,
    };
    Ok(Rows {
        columns,
        rows: kept,
        affected,
        total,
        more: total > limit,
        elapsed: started.elapsed(),
        tag: rows::tag_for(sql, counted),
    })
}

/// Reads a cell as text, answering the empty string for anything else.
///
/// @param cell - the cell, if there was one
fn cell_text(cell: Option<&Value>) -> String {
    match cell {
        Some(Value::Text(text)) => text.clone(),
        Some(Value::Integer(number)) => number.to_string(),
        _ => String::new(),
    }
}

/// Reads a cell as an integer, answering zero for anything else.
///
/// `PRAGMA table_info` reports its flags as integers, and a build that reported
/// them as text would otherwise silently make every column nullable.
///
/// @param cell - the cell, if there was one
fn cell_integer(cell: Option<&Value>) -> i64 {
    match cell {
        Some(Value::Integer(number)) => *number,
        Some(Value::Text(text)) => text.parse().unwrap_or(0),
        _ => 0,
    }
}
