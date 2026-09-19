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
//! **This engine is deliberately incomplete, and `capability` is how you find
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
//!
//! [`SharedDatabase`] is how several threads use one database anyway:
//! **serialized, not parallel**, which is SQLite's own word for it. Any number
//! of threads, exactly one statement at a time, and a transaction that holds
//! its turn for its whole life. The database is opened on a thread of its own
//! and never moved, which is why this crate can offer it and still
//! `forbid(unsafe_code)` - see the `shared` module for the argument.

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
pub mod shared;
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
pub use shared::{SharedConnection, SharedDatabase, SharedTransaction};
pub use value::{Column, Value, ValueKind};

/// What one statement may spend, and the default ceiling on the plan cache.
///
/// Re-exported so that a caller setting [`OpenOptions::limits`] or
/// [`OpenOptions::statement_cache`] does not have to name `inillucent_engine`
/// to build the value it is setting (task-1932, M1). The driver's README calls
/// it the one surface, and a surface a caller has to reach past is not one.
pub use inillucent_engine::base::budget::Limits as StatementLimits;
pub use inillucent_engine::DEFAULT_STATEMENT_CACHE;

/// The file system layer, for a caller that reports which one it is on.
///
/// **Re-exported so the command line does not have to name the engine
/// (task-1946, M11).** `inillucent diagnose` prints the VFS it opened through,
/// which is a fact about the connection rather than a reach into the engine's
/// internals - and `crates/inillucent-cli/src/diagnose.rs` was one of the seven
/// files `no_shell_file_reaches_past_the_driver_more_than_it_is_recorded_at`
/// counts, for those two lines alone.
pub use inillucent_engine::vfs;

/// The authorizer a front end installs, and what it is asked about.
///
/// **Re-exported because a front end that installs one is not reaching into
/// the engine (task-1962, roadmap item 7).** `sqlite3_set_authorizer` is part
/// of every binding's surface; the shell's `.auth on` is one, and
/// `crates/inillucent-cli/src/commands.rs` named `inillucent_engine` seven
/// times for this trait alone.
pub use inillucent_engine::{AuthAction, Authorization, Authorizer};

/// The virtual table modules a front end may register, and the trait they
/// implement.
///
/// The shell adds `fsdir` and `zipfile` and the library does not - a
/// table-valued function over the file system belongs to a program that asked
/// for one, which is the line the reference draws too, with `fsdir` in
/// `shell.c`. Registering one is a thing a caller does, so it is on this
/// surface (task-1962, roadmap item 7).
pub use inillucent_engine::ext::vtab;

/// What the page cache has been asked to do.
///
/// For `.stats` and `inillucent diagnose`, which report the work a statement
/// caused rather than the answer it gave.
pub use inillucent_engine::connect::CacheStats;

/// How many bytes at the front of a script are whitespace and semicolons.
///
/// A front end that has asked [`Connection::statement_length`] where one
/// statement ends uses this to find where the next one begins, so that what it
/// shows a person is the statement rather than the space before it.
pub use inillucent_engine::connect::leading_trivia;

/// The run-time limits `sqlite3_limit` reads and writes.
///
/// Re-exported for the same reason [`StatementLimits`] is: a caller naming one
/// should not have to name the engine to do it.
pub use inillucent_engine::base::limits::Limit;

/// What a read only connection is allowed to run.
///
/// **Re-exported so the command surface asks the driver rather than the
/// engine.** `drivers/README.md`'s line is that a front end reaches the engine
/// through this crate; the read only classifier is a rule about statements,
/// which is exactly the kind of thing the driver is for, and both this crate's
/// own `refuse_if_it_writes` and the command surface's read the same lists
/// (task-1979, section 5.2).
pub use inillucent_engine::readonly;

/// How many frames a buffer pool holds when nobody says otherwise.
pub use inillucent_engine::DEFAULT_FRAMES;

/// Reading the names of the log segments beside a database.
///
/// **Here rather than in the command surface, and re-exported rather than
/// reimplemented.** A caller looking for a segment the chain cannot reach
/// (task-1979, C9) has to read a directory, which no layer below this one does;
/// what a segment is *called* and what its header holds belong to
/// `inillucent-wal`, and these are its own answers.
pub mod log {
    pub use inillucent_engine::recovery::{first_lsn_of, sequence_of_segment_name};
}

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
///
/// ```
/// # use inillucent_driver::{Database, Result};
/// # fn main() -> Result<()> {
/// # let directory = std::env::temp_dir().join(format!("inillucent-doc-database-{}", std::process::id()));
/// # std::fs::create_dir_all(&directory).ok();
/// # let path = directory.join("app.rdb");
/// let database = Database::open(&path)?;
/// assert_eq!(database.path(), path.as_path());
/// database.integrity_check()?;
/// # drop(database);
/// # std::fs::remove_dir_all(&directory).ok();
/// # Ok(())
/// # }
/// ```
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

/// What opening a database did to it.
///
/// Every number is the log scan's own counter; nothing is computed for this.
/// See [`Database::recovery`] for why it exists.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Recovery {
    /// Whether the log held anything above the file's own checkpoint.
    pub recovered: bool,
    /// How many records the scan read.
    pub scanned: u64,
    /// How many records the second pass applied.
    pub applied: u64,
    /// How many transactions committed in the replayed window.
    pub committed: u64,
    /// How many transactions were open at the end of the log and were
    /// discarded.
    pub losers: u64,
    /// The highest segment the live chain reaches.
    pub last_sequence: u64,
    /// The stream position the chain ended at.
    pub last_lsn: u64,
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
        // **`read_only` reaches the file, not only the statement filter
        // (task-1979, C5).** It used to be a filter on this driver's own
        // `execute`, with the file opened for writing either way - so a read
        // only connection took a writer's locks, waited out the busy budget
        // against a live writer and reported the writer's lock. A read only
        // open takes SHARED only and its handle refuses a write.
        let engine = match options.read_only {
            true => EngineDatabase::open_read_only(&path, options.cache_frames),
            false => EngineDatabase::open_with(&path, options.cache_frames),
        }
        .map_err(|error| Error::from_engine(&error, options.diagnostics))?;
        engine.set_statement_cache_limit(options.statement_cache);
        Ok(Database {
            engine,
            path,
            options,
            cancel: Arc::new(AtomicBool::new(false)),
        })
    }

    /// Returns what opening this database did to it.
    ///
    /// See `inillucent_engine::recovery::RecoveryReport`. It is repeated here in
    /// the driver's own shape because a binding reads this and nothing else of
    /// the engine's types.
    pub fn recovery(&self) -> Recovery {
        let report = self.engine.recovery_report();
        Recovery {
            recovered: report.recovered,
            scanned: report.scanned,
            applied: report.applied,
            committed: report.committed,
            losers: report.losers,
            last_sequence: report.last_sequence,
            last_lsn: report.last_lsn,
        }
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
        Database::around(engine, options)
    }

    /// Imports a SQLite file into a database at the path named, and opens that.
    ///
    /// **The target is taken rather than derived (task-1962, roadmap item 7).**
    /// `inillucent migrate` writes to a staging name and renames it once the
    /// import is complete, because a half-written database must not sit at the
    /// path somebody is about to open. `import_sqlite` derives the target,
    /// which is what a fixture wants and not what a migration wants.
    ///
    /// @param from - the SQLite database to read
    /// @param to - the file to write
    pub fn import_sqlite_into(from: impl AsRef<Path>, to: impl AsRef<Path>) -> Result<Database> {
        let options = OpenOptions::default();
        let engine = EngineDatabase::import_into(
            from.as_ref().to_path_buf(),
            to.as_ref().to_path_buf(),
            options.cache_frames,
        )
        .map_err(|error| Error::from_engine(&error, options.diagnostics))?;
        Database::around(engine, options)
    }

    /// Wraps an opened engine database in the driver's own handle.
    ///
    /// @param engine - the opened database
    /// @param options - what it was opened with
    fn around(engine: EngineDatabase, options: OpenOptions) -> Result<Database> {
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
    ///
    /// **It is called `session` and not `connect` because that is what it
    /// returns (task-1961, A5).** Two of these share one transaction - a
    /// `BEGIN` on either is joined by the other, and a write through the second
    /// is undone by the first one's `ROLLBACK` - and `connect` reads as denying
    /// exactly that to anyone arriving from SQLite or rusqlite. `connect` is
    /// kept as a deprecated alias for one release.
    pub fn session(&self) -> Connection<'_> {
        Connection {
            database: self,
            engine: self.engine.session(),
            depth: Cell::new(0),
        }
    }

    /// Returns a connection to this database.
    ///
    /// Renamed [`Database::session`] in task-1961.
    #[deprecated(
        since = "0.1.3",
        note = "renamed `session`: two of these share one transaction"
    )]
    pub fn connect(&self) -> Connection<'_> {
        self.session()
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
    pub fn session_as(&self, session: u64) -> Connection<'_> {
        Connection {
            database: self,
            engine: self.engine.session_as(session),
            depth: Cell::new(0),
        }
    }

    /// Returns a connection that continues an earlier one's session.
    ///
    /// Renamed [`Database::session_as`] in task-1961, with [`Database::connect`].
    ///
    /// @param session - the number an earlier connection reported
    #[deprecated(since = "0.1.3", note = "renamed `session_as`, with `connect`")]
    pub fn connect_as(&self, session: u64) -> Connection<'_> {
        self.session_as(session)
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

    /// Registers a virtual table module this database's statements may name.
    ///
    /// **A program that wants `fsdir` asks for it (task-1962, roadmap item
    /// 7).** The shell registers `fsdir` and `zipfile` and the library does
    /// not, which is the line the reference draws too; making the registration
    /// part of this surface is what lets a front end draw it without naming the
    /// engine. See [`vtab`] for the modules that ship.
    ///
    /// @param module - the module, which the database holds for its life
    pub fn register_module(&self, module: Arc<dyn vtab::Module>) -> Result<()> {
        self.engine
            .register_module(module)
            .map_err(|error| self.classify(&error))
    }

    /// Returns what the page cache has been asked to do.
    ///
    /// The work a statement caused rather than the answer it gave, which is
    /// what `.stats` reports and what a measurement compares between two runs.
    pub fn cache_stats(&self) -> CacheStats {
        self.engine.cache_stats()
    }

    /// Returns how many bytes the page cache is holding.
    pub fn pool_bytes(&self) -> usize {
        self.engine.pool_bytes()
    }

    /// Returns what one run-time limit is set to on this database.
    ///
    /// `sqlite3_limit`'s read half. It takes no borrow of the engine, so a
    /// callback may ask it while a statement is running - see
    /// `crates/inillucent-engine/src/connect.rs` and task-1962's A1 step 3.
    ///
    /// @param limit - which limit
    pub fn limit(&self, limit: Limit) -> i64 {
        self.engine.limit(limit)
    }

    /// Sets one run-time limit, and returns what it was before.
    ///
    /// @param limit - which limit
    /// @param requested - the value asked for, clamped to the manifest's bounds
    pub fn set_limit(&self, limit: Limit, requested: i64) -> i64 {
        self.engine.set_limit(limit, requested)
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
///
/// ```
/// # use inillucent_driver::{Database, Result, Value};
/// # fn main() -> Result<()> {
/// # let directory = std::env::temp_dir().join(format!("inillucent-doc-connection-{}", std::process::id()));
/// # std::fs::create_dir_all(&directory).ok();
/// let database = Database::open(directory.join("app.rdb"))?;
/// let connection = database.session();
/// connection.execute_batch("CREATE TABLE note (id INTEGER PRIMARY KEY, body TEXT)")?;
///
/// let written = connection.execute(
///     "INSERT INTO note (body) VALUES (?1)",
///     &[Value::Text("hello".to_string())],
/// )?;
/// assert_eq!(written, 1);
///
/// let rows = connection.query("SELECT body FROM note", &[], 10)?;
/// assert_eq!(rows.value(0, 0).and_then(Value::text), Some("hello"));
/// # drop(connection);
/// # drop(database);
/// # std::fs::remove_dir_all(&directory).ok();
/// # Ok(())
/// # }
/// ```
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
    /// **`limit` is a count and not a sentinel: `0` hands back no rows.** It
    /// is a real count because [`Connection::execute`] passes `0` to run a
    /// statement for its effect, so there is no spare value that could mean
    /// "all of them" - and every other embedded database treats `0` as no
    /// limit, so `0` is what a caller reaches for. What comes back is an empty
    /// `rows` beside a [`Rows::total`] reporting the true count and a status
    /// saying success, which reads as a fault in the caller's own mapping
    /// code. It cost task-1947 five of its eight storage tests at once, and
    /// the symptom was "the board is empty" rather than "the query is wrong".
    /// Use [`Connection::query_all`] when you want every row.
    ///
    /// @param sql - the statement
    /// @param params - the values bound to `?1`, `?2`, ...
    /// @param limit - how many rows to hand back; `0` hands back none
    pub fn query(&self, sql: &str, params: &[Value], limit: usize) -> Result<Rows> {
        if self.database.options.read_only {
            self.refuse_if_it_writes(sql)?;
        }
        self.run(sql, params, limit)
    }

    /// Runs one statement and returns every row it produced.
    ///
    /// **The call almost every application wants, so that no caller has to
    /// know what number means "all of them" (task-1947).** The engine
    /// materialises the answer either way - which is what makes
    /// [`Rows::total`] exact - so handing all of it back costs the rows
    /// themselves and nothing else. `Rows::more` is always false here, because
    /// nothing was left behind.
    ///
    /// The consumer that found this wrote its own `ALL_ROWS: usize =
    /// usize::MAX` constant with a comment explaining why the constant had to
    /// exist. That constant is this function.
    ///
    /// @param sql - the statement
    /// @param params - the values bound to `?1`, `?2`, ...
    pub fn query_all(&self, sql: &str, params: &[Value]) -> Result<Rows> {
        self.query(sql, params, usize::MAX)
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
        let mut guard = self.begin()?;
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
        let inner = self
            .engine
            .begin()
            .map_err(|error| self.database.classify(&error))?;
        self.depth.set(1);
        Ok(Transaction {
            connection: self,
            inner: Some(inner),
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
    /// **By the statement's class, from the one list the command surface also
    /// reads (task-1979, section 5.2).** It used to ask the engine to *plan*
    /// the statement and refuse on the text of the failure - and `explain`
    /// answers `Ok` for an `INSERT`, a write pragma, an `ATTACH` and a
    /// `VACUUM INTO`, so every one of them ran on a read only connection and
    /// persisted.
    ///
    /// @param sql - the statement
    fn refuse_if_it_writes(&self, sql: &str) -> Result<()> {
        match inillucent_engine::readonly::admits(sql) {
            true => Ok(()),
            false => Err(Error::said(
                Status::ReadOnly,
                "this connection is read only, and that statement changes something.",
            )),
        }
    }

    /// Returns the rowid the last `INSERT` on this database assigned.
    ///
    /// **It answers a `Result` because the engine can be busy (task-1962,
    /// A11).** A function registered on this connection that asks while the
    /// statement that called it is still running is refused rather than
    /// aborting the process.
    pub fn last_insert_rowid(&self) -> Result<i64> {
        self.engine
            .last_insert_rowid()
            .map_err(|error| self.database.classify(&error))
    }

    /// Returns how many rows every statement so far has changed.
    pub fn total_changes(&self) -> Result<i64> {
        self.engine
            .total_changes()
            .map_err(|error| self.database.classify(&error))
    }

    /// Returns whether a transaction is open.
    pub fn in_transaction(&self) -> Result<bool> {
        self.engine
            .autocommit()
            .map(|open| !open)
            .map_err(|error| self.database.classify(&error))
    }

    /// Returns the schema's generation, which changes when the schema does.
    ///
    /// A consumer that caches a table's columns compares this to know whether
    /// the cache is stale, rather than re-reading the schema per statement.
    pub fn schema_cookie(&self) -> Result<u64> {
        self.engine
            .schema_cookie()
            .map_err(|error| self.database.classify(&error))
    }

    /// Returns how many rows the last statement on this database changed.
    ///
    /// `sqlite3_changes`. The statement's own rows: a trigger body's go into
    /// [`Connection::total_changes`] and not into this, which is SQLite's rule.
    pub fn changes(&self) -> Result<i64> {
        self.engine
            .changes()
            .map_err(|error| self.database.classify(&error))
    }

    /// Installs the authorizer every later statement is bound under, or removes
    /// it.
    ///
    /// `sqlite3_set_authorizer`: the callback is consulted before a read, a
    /// select or a function call is bound, and a `Deny` refuses the statement.
    /// Pass `None` to allow everything again. The plan cache is emptied with
    /// it, because a plan compiled under one authorizer is that authorizer's
    /// answer.
    ///
    /// @param authorizer - the callback, or nothing
    pub fn set_authorizer(&self, authorizer: Option<std::rc::Rc<dyn Authorizer>>) -> Result<()> {
        self.engine
            .set_authorizer(authorizer)
            .map_err(|error| self.database.classify(&error))
    }

    /// Puts the connection into or out of defensive mode.
    ///
    /// `SQLITE_DBCONFIG_DEFENSIVE`, which the reference's shell turns on by
    /// default: it refuses `PRAGMA journal_mode = OFF` and
    /// `PRAGMA writable_schema = ON`, both of which let a caller lose or
    /// corrupt a database with one statement.
    ///
    /// @param on - whether the flag is in force
    pub fn set_defensive(&self, on: bool) -> Result<()> {
        self.engine
            .set_defensive(on)
            .map_err(|error| self.database.classify(&error))
    }

    /// Returns how many parameters one statement declares.
    ///
    /// **What a bind index is checked against (task-1979, D3).** The C ABI's
    /// bind functions take a caller supplied `uint32_t` and had no upper bound,
    /// so one call with a large index grew the parameter vector to match it: an
    /// index near `u32::MAX` asked for about 137 GB and stalled the process for
    /// tens of seconds before the allocator gave up. A count the statement
    /// itself declares is the bound, and it is the same number `prepare` uses
    /// to size its own parameter list.
    ///
    /// @param sql - the statement text
    pub fn parameter_count(&self, sql: &str) -> Result<u32> {
        self.engine
            .parameter_count(sql)
            .map_err(|error| self.database.classify(&error))
    }

    /// Returns the named parameters one statement declares, with their indexes.
    ///
    /// For a front end that binds by name and has to know which names the
    /// statement has before it can ask for them.
    ///
    /// @param sql - the statement text
    pub fn parameter_names(&self, sql: &str) -> Result<Vec<(Vec<u8>, u32)>> {
        self.engine
            .parameter_names(sql)
            .map_err(|error| self.database.classify(&error))
    }

    /// Returns how many bytes of `sql` the first statement in it uses.
    ///
    /// **What a shell needs to know whether a line is finished.** A
    /// `CREATE TRIGGER` spans many lines and holds semicolons inside its body,
    /// so "ends with a semicolon" is the wrong question and the parser has to
    /// be the one that answers it. `&sql[length..]` is what is left.
    ///
    /// @param sql - the script
    pub fn statement_length(&self, sql: &str) -> Result<usize> {
        self.engine
            .prepare_with_tail(sql)
            .map(|prepared| prepared.consumed)
            .map_err(|error| self.database.classify(&error))
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
    pub fn remove_function(&self, name: &str, arity: i32) -> Result<bool> {
        self.engine
            .remove_function(name, arity)
            .map_err(|error| self.database.classify(&error))
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
/// ```
/// # use inillucent_driver::{Database, Result, Value};
/// # fn main() -> Result<()> {
/// # let directory = std::env::temp_dir().join(format!("inillucent-doc-transaction-{}", std::process::id()));
/// # std::fs::create_dir_all(&directory).ok();
/// let database = Database::open(directory.join("app.rdb"))?;
/// let connection = database.session();
/// connection.execute_batch("CREATE TABLE account (id INTEGER PRIMARY KEY, balance INTEGER)")?;
/// connection.execute("INSERT INTO account VALUES (1, 100)", &[])?;
///
/// // Dropped without a commit: the write is gone.
/// {
///     let transaction = connection.begin()?;
///     transaction.execute("UPDATE account SET balance = 0 WHERE id = 1", &[])?;
/// }
/// let after = connection.query("SELECT balance FROM account WHERE id = 1", &[], 1)?;
/// assert_eq!(after.value(0, 0), Some(&Value::Integer(100)));
///
/// // Committed: the write is kept.
/// let transaction = connection.begin()?;
/// transaction.execute(
///     "UPDATE account SET balance = balance - ?1 WHERE id = ?2",
///     &[Value::Integer(50), Value::Integer(1)],
/// )?;
/// transaction.commit()?;
/// let after = connection.query("SELECT balance FROM account WHERE id = 1", &[], 1)?;
/// assert_eq!(after.value(0, 0), Some(&Value::Integer(50)));
/// # drop(connection);
/// # drop(database);
/// # std::fs::remove_dir_all(&directory).ok();
/// # Ok(())
/// # }
/// ```
#[derive(Debug)]
pub struct Transaction<'c> {
    /// The connection it is open on.
    connection: &'c Connection<'c>,
    /// The engine's own transaction, which is what actually rolls back.
    ///
    /// **Every decision about the transaction is one layer down (task-1961,
    /// A4).** The `BEGIN`, the refusal to nest, the `COMMIT`, the `ROLLBACK`
    /// and the rollback on drop are all
    /// [`inillucent_engine::connect::Transaction`]'s, so the engine's own
    /// connection carries the guarantee rather than only the driver's, and
    /// there is one implementation of it rather than two. What is left here is
    /// the driver's types: [`Value`] in, [`Rows`] out, [`Error`] on the way
    /// back.
    ///
    /// An `Option` so that [`Transaction::commit`] and
    /// [`Transaction::rollback`], which take `self`, can move the inner
    /// transaction out and call its own `commit` or `rollback` - which is what
    /// stops the `Drop` below from discarding work that was kept.
    inner: Option<inillucent_engine::connect::Transaction<'c>>,
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
    pub fn commit(mut self) -> Result<()> {
        self.connection.depth.set(0);
        match self.inner.take() {
            Some(inner) => inner
                .commit()
                .map_err(|error| self.connection.database.classify(&error)),
            None => Ok(()),
        }
    }

    /// Discards everything this transaction wrote.
    ///
    /// The same thing dropping it does, said out loud. A caller that has
    /// decided to abandon the work reads better for saying so, and the error a
    /// failed rollback produces is reportable here and is not from `Drop`.
    pub fn rollback(mut self) -> Result<()> {
        self.connection.depth.set(0);
        match self.inner.take() {
            Some(inner) => inner
                .rollback()
                .map_err(|error| self.connection.database.classify(&error)),
            None => Ok(()),
        }
    }

    /// Runs a list of statements, rolling back on the first failure or on a
    /// refused postcondition.
    ///
    /// The body of [`Connection::transaction`], which is now this method with
    /// the open and the commit around it.
    ///
    /// @param work - the statements and their bound values, in order
    /// @param check - what must be true of the changed-row counts before commit
    fn run_all<F>(&mut self, work: &[(String, Vec<Value>)], check: F) -> Result<Vec<u64>>
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
    fn rolled_back(&mut self, why: Error) -> Error {
        self.connection.depth.set(0);
        let Some(inner) = self.inner.take() else {
            return why;
        };
        match inner.rollback() {
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
    /// Puts the nesting depth back; the engine's transaction rolls itself back.
    ///
    /// The `ROLLBACK` is [`inillucent_engine::connect::Transaction`]'s own
    /// `Drop`, which runs when `inner` is dropped with the rest of this value.
    /// What is left here is the driver's bookkeeping: the depth this connection
    /// counts so a second [`Connection::begin`] is refused by name.
    ///
    /// A caller who wants to know whether the rollback worked calls
    /// [`Transaction::rollback`] and reads the answer.
    fn drop(&mut self) {
        self.connection.depth.set(0);
    }
}

/// A statement compiled once and run more than once.
///
/// ```
/// # use inillucent_driver::{Database, Result, Value};
/// # fn main() -> Result<()> {
/// # let directory = std::env::temp_dir().join(format!("inillucent-doc-statement-{}", std::process::id()));
/// # std::fs::create_dir_all(&directory).ok();
/// let database = Database::open(directory.join("app.rdb"))?;
/// let connection = database.session();
/// connection.execute_batch("CREATE TABLE k (id INTEGER PRIMARY KEY, label TEXT)")?;
///
/// let mut insert = connection.prepare("INSERT INTO k VALUES (?1, ?2)")?;
/// for (id, label) in [(1i64, "one"), (2, "two")] {
///     insert.query(&[Value::Integer(id), Value::Text(label.to_string())], 0)?;
/// }
/// assert_eq!(insert.sql(), "INSERT INTO k VALUES (?1, ?2)");
///
/// let counted = connection.query("SELECT count(*) FROM k", &[], 1)?;
/// assert_eq!(counted.value(0, 0), Some(&Value::Integer(2)));
/// # drop(insert);
/// # drop(connection);
/// # drop(database);
/// # std::fs::remove_dir_all(&directory).ok();
/// # Ok(())
/// # }
/// ```
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
