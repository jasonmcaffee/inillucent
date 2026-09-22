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

use inillucent_base::error::misuse;
use inillucent_base::{DbError, DbResult, PrimaryCode};
use inillucent_exec::physical::Params;
use inillucent_tree::datum::OwnedDatum;

use crate::{ImportedDatabase, DEFAULT_FRAMES};

/// The page size a database is created at.
///
/// The engine's own default, which Phase 1 fixed after measuring 16/32/64. A
/// connection that chose its own would be making files unlike the ones every
/// measurement was taken on.
pub const PAGE_SIZE: usize = 32_768;

/// What the page cache has been asked to do.
///
/// The counters `.stats` reports. They are the engine's own rather than
/// SQLite's allocator's, because they are the cost this engine actually has.
#[derive(Clone, Copy, Debug, Default)]
pub struct CacheStats {
    /// Fetches answered from a resident frame.
    pub hits: u64,
    /// Fetches that had to read the file.
    pub misses: u64,
    /// Fetches answered by a frame in the cooling list, which cost no I/O.
    pub rewarms: u64,
    /// Frames moved into the cooling list.
    pub cooled: u64,
    /// Frames evicted.
    pub evicted: u64,
    /// Pages read from the file.
    pub reads: u64,
    /// Pages written to the file.
    pub writes: u64,
    /// Reads of both meta slots in full: two whole pages, allocated, read and
    /// checksummed.
    ///
    /// Here so that a guard can assert what a statement outside a transaction
    /// costs without reading a clock - see
    /// `crates/inillucent/tests/budget.rs`'s
    /// `a_statement_outside_a_transaction_rereads_nothing` (task-2046).
    pub meta_reads: u64,
    /// Reads of the bytes a meta record occupies, without the page around them.
    pub meta_probes: u64,
}

/// What the write-ahead log has been asked to do.
///
/// The counters a commit moves, and the reason they are here rather than only in
/// `inillucent-wal`: a test that wants to assert one transaction costs less than
/// many needs a number that does not move when the machine is busy. A wall-clock
/// ratio between the two arms measures the scheduler as much as the engine, and
/// on a contended machine it measured 3.5x where an idle one measures 40x. These
/// counts read the same on an idle machine and on a loaded one, so `writes` is
/// what `crates/inillucent/tests/budget.rs` asserts on - see
/// `one_transaction_beats_many`, which says why it is `writes` and not `syncs`.
///
/// Shaped here rather than re-exported so a caller reading it does not have to
/// name the crate the log lives in, which is the same choice [`CacheStats`]
/// makes about the pool.
#[derive(Clone, Copy, Debug, Default)]
pub struct LogStats {
    /// Records appended to the log, which is one per page image plus one per
    /// commit.
    pub records: u64,
    /// Calls to the log file's `write_all_at`.
    pub writes: u64,
    /// Calls to the log file's `sync`.
    pub syncs: u64,
    /// Bytes appended to the log.
    pub bytes: u64,
}

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
    /// The next connection number to hand out.
    ///
    /// **On the database rather than on the engine (task-1962, A11).** It used
    /// to live behind the cell, so opening a connection borrowed the engine -
    /// and a callback that asked for one while a statement was running aborted
    /// the process. A counter is the database's own bookkeeping; the engine
    /// learns the number on the first statement that runs under it.
    next_session: std::cell::Cell<u64>,
    /// The same writer the engine holds.
    ///
    /// **A second handle on one group, not a second group (task-1962, A1
    /// step 3).** `Writing`'s ten fields are each behind their own cell, so the
    /// engine writes through this `Rc` and so does everything here; there is
    /// one transaction and both handles see it. What it buys is the questions
    /// below - [`Connection::autocommit`] and [`Connection::decided_over`] -
    /// which a callback asks *while* the engine is running the statement that
    /// called it. Reading them through [`Database::engine`] took the cell the
    /// statement was already holding, so the answer was an error rather than a
    /// number.
    writer: std::rc::Rc<crate::engine::state::Writing>,
    /// The same settings the engine holds.
    ///
    /// **The second group A1 step 3 moved out (task-1962).** `PRAGMA` values,
    /// the planner levers and the run-time limits, each behind its own cell.
    /// `limit` and `set_limit` below took `self.engine.borrow()` and
    /// `borrow_mut()` with no `try_`, so `sqlite3_limit` from inside a callback
    /// did not return an error - it aborted the process. They read this
    /// instead and take no borrow of the engine at all.
    settings: std::rc::Rc<crate::engine::state::Pragmas>,
    /// The same plan cache the engine holds.
    ///
    /// Every field of it was already behind a cell, so this needed nothing but
    /// the `Rc` (task-1962, A1 step 3).
    plans: std::rc::Rc<crate::engine::state::Compiled>,
    /// The same counters the engine holds.
    ///
    /// `sqlite3_changes` from an update hook is the case this is for: an
    /// application asks what the statement it was just told about changed,
    /// while that statement is still running.
    counters: std::rc::Rc<crate::engine::state::Counters>,
}

/// Reports whether a path names an in-memory database rather than a file.
///
/// `:memory:` and the empty path, which are SQLite's two spellings for it.
///
/// @param path - the path a caller opened with
fn is_memory(path: &Path) -> bool {
    path.as_os_str().is_empty() || path.as_os_str() == ":memory:"
}

/// Reports whether there is a database at a path, or refuses to guess.
///
/// **"Create" is the destructive branch, so it may only be taken for a path the
/// file system says holds nothing** (task-2070). [`Database::open_as`] chose
/// between opening and creating on `Path::is_file`, which answers `false` for
/// every error it meets - a permission the process does not have, a path on a
/// share that is momentarily unreachable, a name that resolves to something
/// that is not a file - and
/// [`crate::ImportedDatabase::create_on`] begins by **deleting whatever is at
/// the path**. So one unanswerable question about an existing database was one
/// deleted database, answered to the caller as an empty one with no error
/// anywhere. That is the shape task-2070 reports from the other end: a staged
/// database reopened as four pages naming no objects at all, because
/// `chunk_search_config` is simply the first name anything asks for after such
/// an open.
///
/// So the three answers are kept apart. A file is a database to open. A
/// `NotFound` is a path to create at. Anything else - an error the file system
/// gave, or a directory or a device sitting at the name - is refused, carrying
/// what the operating system said, because a caller who is told their database
/// is empty cannot tell that from one that is.
///
/// @param path - the path a caller opened with
fn there_is_a_database_at(path: &Path) -> DbResult<bool> {
    match std::fs::metadata(path) {
        Ok(found) if found.is_file() => Ok(true),
        Ok(_) => Err(inillucent_base::error::refusal(format!(
            "{} is not a file, so it is neither a database to open nor a path to create one at",
            path.display()
        ))),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(inillucent_base::error::refusal(format!(
            "cannot tell whether there is a database at {}: {error}. Refusing rather than \
             creating one, because creating one deletes whatever is there",
            path.display()
        ))),
    }
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
        Database::open_as(path, PAGE_SIZE, frames, false)
    }

    /// Opens a database with the page size and the pool size stated.
    ///
    /// **The page size a connection is built at was not reachable from here,
    /// and that is why every published number and every story ran at 32,768
    /// bytes.** `ImportedDatabase::create` has taken a page size since Phase 1
    /// and `Database::open` passed the constant, so the only way to get a file
    /// with 4,096 byte pages was to go under the connection surface - which
    /// `new_engine_vtab_stream.rs` does and no application can. The
    /// configuration matrix in `inillucent_compat::matrix` needs a database
    /// that is *actually* built at the arm's page size and is still driven
    /// through the surface an application binds to, so this is that call.
    ///
    /// It is not a second open path. `open` and `open_with` are this function
    /// with [`PAGE_SIZE`] written in, and every step below is the one they
    /// already took.
    ///
    /// The page size decides the file's geometry when this call creates the
    /// file, and names the geometry the file already has when it opens an
    /// existing one - so a caller that opens a 4,096 byte file at 32,768 gets
    /// a connection whose `PRAGMA page_size` and whose `VACUUM` disagree with
    /// the file. Pass the size the file was built at.
    ///
    /// @param path - the database file
    /// @param page_size - the page size to build at, or the one the file has
    /// @param frames - how many frames the buffer pool holds
    pub fn open_at(path: impl AsRef<Path>, page_size: usize, frames: usize) -> DbResult<Database> {
        Database::open_as(path, page_size, frames, false)
    }

    /// Opens a database this connection will never write.
    ///
    /// See [`ImportedDatabase::open_read_only`]. A `:memory:` database has
    /// nothing to protect and no file to leave alone, so it is opened the way
    /// it always was and the statement filter above is what read only means
    /// for it.
    ///
    /// @param path - the database file
    /// @param frames - how many frames the buffer pool holds
    pub fn open_read_only(path: impl AsRef<Path>, frames: usize) -> DbResult<Database> {
        Database::open_as(path, PAGE_SIZE, frames, true)
    }

    /// [`Database::open_with`], with the caller saying whether this connection
    /// may write.
    ///
    /// @param path - the database file
    /// @param page_size - the page size to build at, or the one the file has
    /// @param frames - how many frames the buffer pool holds
    /// @param read_only - whether this connection may write the file
    fn open_as(
        path: impl AsRef<Path>,
        page_size: usize,
        frames: usize,
        read_only: bool,
    ) -> DbResult<Database> {
        let path = path.as_ref().to_path_buf();
        // **`:memory:` is a database, not a filename.** The operating system
        // refuses it as a path - on Windows with `the filename, directory name,
        // or volume label syntax is incorrect` - so a shell started with no
        // file could not open anything at all. It is the name SQLite gives a
        // database that lives in memory, and this engine has a file system that
        // is memory: `inillucent_vfs::MemoryVfs`, one instance per database, so
        // two `:memory:` databases are two databases and both disappear with
        // the last handle to them. An empty path means the same thing, which is
        // also SQLite's rule.
        if is_memory(&path) {
            let vfs: std::sync::Arc<dyn inillucent_vfs::Vfs> =
                std::sync::Arc::new(inillucent_vfs::MemoryVfs::new());
            let engine = ImportedDatabase::create_on(vfs, path.clone(), page_size, frames)?;
            return Ok(Database {
                writer: std::rc::Rc::clone(&engine.writing),
                settings: std::rc::Rc::clone(&engine.pragmas),
                plans: std::rc::Rc::clone(&engine.compiled),
                counters: std::rc::Rc::clone(&engine.counters),
                engine: RefCell::new(engine),
                path,
                changes: std::cell::Cell::new(0),
                next_session: std::cell::Cell::new(1),
            });
        }
        let engine = match (there_is_a_database_at(&path)?, read_only) {
            (true, false) => ImportedDatabase::open(path.clone(), page_size, frames)?,
            (true, true) => ImportedDatabase::open_read_only(path.clone(), page_size, frames)?,
            // **A read only connection does not create the file it was given.**
            // Creating one would answer a caller who asked to read an existing
            // database with an empty one, and would write - see task-1979's E2,
            // which is the same mistake on the read verbs.
            (false, true) => {
                return Err(inillucent_base::error::refusal(
                    "there is no database at that path, and a read only connection does not \
                     create one",
                ))
            }
            (false, false) => ImportedDatabase::create(path.clone(), page_size, frames)?,
        };
        Ok(Database {
            writer: std::rc::Rc::clone(&engine.writing),
            settings: std::rc::Rc::clone(&engine.pragmas),
            plans: std::rc::Rc::clone(&engine.compiled),
            counters: std::rc::Rc::clone(&engine.counters),
            engine: RefCell::new(engine),
            path,
            changes: std::cell::Cell::new(0),
            next_session: std::cell::Cell::new(1),
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
        Database::import_into(source, PathBuf::from(target), frames)
    }

    /// Imports a SQLite file into a database at the path named, and opens that.
    ///
    /// **The target is taken rather than derived, because that is the property
    /// a migration needs (task-1962, roadmap item 7).** A half-written database
    /// must not sit at the path somebody is about to open, so `inillucent
    /// migrate` writes to a staging name and renames it; deriving the target
    /// here would take that choice away from the caller.
    ///
    /// @param source - the SQLite database to read
    /// @param target - the file to write
    /// @param frames - how many frames the buffer pool holds
    pub fn import_into(source: PathBuf, target: PathBuf, frames: usize) -> DbResult<Database> {
        let engine = ImportedDatabase::import_into(source, target.clone(), PAGE_SIZE, frames)?;
        Ok(Database {
            writer: std::rc::Rc::clone(&engine.writing),
            settings: std::rc::Rc::clone(&engine.pragmas),
            plans: std::rc::Rc::clone(&engine.compiled),
            counters: std::rc::Rc::clone(&engine.counters),
            engine: RefCell::new(engine),
            path: target,
            changes: std::cell::Cell::new(0),
            next_session: std::cell::Cell::new(1),
        })
    }

    /// Returns a connection to this database.
    ///
    /// **Each one is its own session, and that is what `temp` is scoped to.** A
    /// temporary table belongs to the connection that made it and to no other,
    /// which is SQLite's rule and is graded against it - so a connection is a
    /// number the engine can tell apart, rather than a borrow that is
    /// indistinguishable from every other borrow.
    ///
    /// **A session does not scope the transaction, and the difference matters.**
    /// Every connection returned here borrows one `ImportedDatabase`, and the
    /// open transaction lives on that, so a `BEGIN` on any handle opens a
    /// transaction every other handle then joins: a write issued through a
    /// second connection lands inside the first one's transaction and is undone
    /// by its `ROLLBACK`. Two connections are two sessions over one writer, not
    /// two writers.
    ///
    /// Two *processes* are genuinely independent, over the same SHARED,
    /// RESERVED, PENDING and EXCLUSIVE protocol SQLite uses; see
    /// `docs/roadmap.md` item 9, which is where concurrency is graded. An
    /// application that needs two independent transactions needs two processes,
    /// or two `Database` values over two files.
    ///
    /// **It is called `session` and not `connect` because that is what it
    /// returns (task-1961, A5).** `connect()` returned something shaped exactly
    /// like an independent connection, and the paragraph above - that two of
    /// them share one transaction - was the only thing that said otherwise.
    /// Anyone arriving from SQLite or rusqlite reads `connect` as "a second
    /// handle with its own transaction", writes through it, and has the write
    /// undone by the first handle's `ROLLBACK` with nothing reported. The name
    /// now says which of the two things it is. `connect` is kept as a
    /// deprecated alias for one release.
    pub fn session(&self) -> Connection<'_> {
        let session = self.next_session.get();
        self.next_session.set(session.saturating_add(1));
        Connection {
            database: self,
            session,
        }
    }

    /// Returns a connection to this database.
    ///
    /// Renamed [`Database::session`] in task-1961, because two of these share
    /// one transaction and `connect` says they do not. Kept for one release so
    /// an application outside this workspace compiles while it is moved.
    #[deprecated(
        since = "0.1.3",
        note = "renamed `session`: two of these share one transaction"
    )]
    pub fn connect(&self) -> Connection<'_> {
        self.session()
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
    /// @param session - the number an earlier [`Database::session`] returned
    pub fn session_as(&self, session: u64) -> Connection<'_> {
        Connection {
            database: self,
            session,
        }
    }

    /// Returns a connection that is a continuation of an earlier one.
    ///
    /// Renamed [`Database::session_as`] in task-1961, with [`Database::connect`].
    ///
    /// @param session - the number an earlier [`Database::session`] returned
    #[deprecated(since = "0.1.3", note = "renamed `session_as`, with `connect`")]
    pub fn connect_as(&self, session: u64) -> Connection<'_> {
        self.session_as(session)
    }

    /// Returns the file this database is in.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Returns which segment of its log this connection is writing.
    ///
    /// See [`ImportedDatabase::log_sequence`].
    pub fn log_sequence(&self) -> u64 {
        self.engine.borrow().log_sequence()
    }

    /// Returns what opening this database did to it.
    ///
    /// See [`ImportedDatabase::recovery_report`]. Cloned rather than borrowed
    /// because the engine is behind a cell and a caller holding a borrow across
    /// a statement would take the cell the statement needs.
    pub fn recovery_report(&self) -> crate::recovery::RecoveryReport {
        self.engine.borrow().recovery_report().clone()
    }

    /// Makes everything written so far durable in the file.
    ///
    /// A database that is dropped without this is not lost - `open` replays the
    /// log - but a checkpoint is what makes the next open cheap.
    pub fn checkpoint(&self) -> DbResult<()> {
        self.engine.borrow_mut().checkpoint()
    }

    /// Adds one virtual-table module to this connection.
    ///
    /// **The route a program takes to widen what SQL can reach.** `fsdir` is
    /// the reason it exists: a table-valued function over the file system does
    /// not belong in a library that any statement runs through, so the shell
    /// registers it and a program that does not want it never does. It is the
    /// same split the reference makes - `fsdir` is in `shell.c`, not in
    /// `sqlite3.c`.
    ///
    /// The schema is refreshed afterwards, because an eponymous module's name
    /// is a table name and the binder resolves those from the catalog.
    ///
    /// @param module - the module to add
    pub fn register_module(
        &self,
        module: std::sync::Arc<dyn inillucent_ext::vtab::Module>,
    ) -> DbResult<()> {
        self.engine.borrow_mut().register_module(module)
    }

    /// Returns how many compiled statements this database is holding.
    ///
    /// See `ImportedDatabase::cached_statements`; this is the same count,
    /// reachable from the connection surface an application actually holds.
    pub fn cached_statements(&self) -> usize {
        self.plans.held()
    }

    /// Returns the ceiling one session's plan cache is emptied at.
    pub fn statement_cache_limit(&self) -> usize {
        self.plans.limit()
    }

    /// Sets the ceiling one session's plan cache is emptied at.
    ///
    /// @param most - how many compiled statements one session may hold
    pub fn set_statement_cache_limit(&self, most: usize) {
        self.plans.set_limit(most);
    }

    /// Forgets every compiled statement.
    pub fn clear_statement_cache(&self) {
        self.plans.forget_all();
    }

    /// Returns what one run-time limit is set to on this database.
    ///
    /// @param limit - which limit
    pub fn limit(&self, limit: inillucent_base::limits::Limit) -> i64 {
        self.settings.limits().borrow().get(limit)
    }

    /// Sets one run-time limit, and returns what it was before.
    ///
    /// @param limit - which limit
    /// @param requested - the value asked for
    /// @returns the value that was in force before this call
    pub fn set_limit(&self, limit: inillucent_base::limits::Limit, requested: i64) -> i64 {
        self.settings.limits().borrow_mut().set(limit, requested)
    }

    /// Returns what the page cache has been asked to do.
    ///
    /// For `.stats`, which reports the work a statement caused rather than the
    /// answer it gave. The shape is the engine's own rather than the pool's, so
    /// a caller reading it does not have to name the crate the pool lives in.
    pub fn cache_stats(&self) -> CacheStats {
        let held = self.engine.borrow().pool_stats();
        CacheStats {
            hits: held.hits,
            misses: held.misses,
            rewarms: held.rewarms,
            cooled: held.cooled,
            evicted: held.evicted,
            reads: held.reads,
            writes: held.writes,
            meta_reads: held.meta_reads,
            meta_probes: held.meta_probes,
        }
    }

    /// Returns what the write-ahead log has been asked to do.
    ///
    /// A commit appends a commit record and the page images the transaction
    /// dirtied, so the count grows with the number of transactions as well as
    /// with the amount of data. That is what makes it the right thing for a
    /// budget guard to assert on: it is a count of work rather than a reading of
    /// the clock, so it does not change when the machine is busy.
    pub fn log_stats(&self) -> LogStats {
        let held = self.engine.borrow().wal().stats();
        LogStats {
            records: held.records,
            writes: held.writes,
            syncs: held.syncs,
            bytes: held.bytes,
        }
    }

    /// Returns what the write path has done to every tree, added up.
    ///
    /// For `inillucent-writeprofile`'s index count sweep, which grades a change
    /// to the leaf page per secondary index. A per-row time says a write got
    /// slower; the compactions, the splits and the nanoseconds spent making room
    /// say which tree paid for it and in which stage.
    pub fn write_stats(&self) -> inillucent_tree::write::WriteStats {
        self.engine.borrow().write_stats()
    }

    /// Returns how many bytes the page cache is holding.
    pub fn pool_bytes(&self) -> usize {
        self.engine.borrow().pool_bytes()
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

/// One statement compiled out of a script, and how much of the script it used.
///
/// **A named pair, because the second half was an unnamed `usize` on a public
/// method (task-1961, A10).** [`Connection::prepare_with_tail`] answered
/// `(Statement, usize)`, and the number is a byte offset into the *input*,
/// including the terminating semicolon and the trivia after it - which nothing
/// in the type said. A caller that read it as a row count, a column count or an
/// offset into the statement would compile.
pub struct Prepared<'d> {
    /// The compiled statement.
    pub statement: Statement<'d>,
    /// How many bytes of the script the statement used, so `&sql[consumed..]`
    /// is the next statement rather than the space before it.
    pub consumed: usize,
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
    fn engine(&self) -> DbResult<std::cell::RefMut<'_, ImportedDatabase>> {
        self.engine_mut()
    }

    /// Returns the engine, having told it which connection is asking.
    ///
    /// **It answers an error rather than aborting the process (task-1962,
    /// A11).** This was `borrow_mut()`, which panics when the cell is already
    /// borrowed, and the reachable case is a scalar function registered through
    /// [`Connection::create_scalar_function`] whose body calls back into the
    /// connection it was registered on: the statement holds the borrow, the
    /// function asks for it again, and the process aborts. `AGENTS.md` bans
    /// `panic!` on a path that reads SQL text, and a panicking `borrow_mut` is
    /// the same failure under another name.
    ///
    /// The refusal is `misuse`, which is the code the driver already documents
    /// for a caller that broke this API's own contract - and calling a
    /// connection from inside a function running on it is exactly that, because
    /// one `RefCell` holds the whole engine. A1 step 3 splits that cell into
    /// three and most of this goes with it.
    fn engine_mut(&self) -> DbResult<std::cell::RefMut<'_, ImportedDatabase>> {
        let mut held = self.database.engine.try_borrow_mut().map_err(|_| {
            misuse(
                "this connection is already running a statement; a function registered on a \
                 connection cannot call back into it",
            )
        })?;
        held.use_session(self.session);
        Ok(held)
    }

    /// Runs one or more statements for their effect.
    ///
    /// **Split by the grammar, not by a scan for semicolons.** The obvious
    /// implementation - walk the text, remember whether you are inside a quote,
    /// cut at every other `;` - is what this used to do, and it cannot express
    /// a statement that *contains* a semicolon. A trigger body does:
    ///
    /// ```sql
    /// CREATE TRIGGER t AFTER INSERT ON m FOR EACH ROW
    ///   BEGIN UPDATE s SET n = n + NEW.d WHERE k = NEW.k; END
    /// ```
    ///
    /// arrived here as two fragments, and the first is an unterminated trigger
    /// the parser rightly refused with `incomplete input`. So an application
    /// could not create a trigger in the same call it created the tables the
    /// trigger is about, which is the natural way to write a schema and the way
    /// `sqlite3_exec` accepts.
    ///
    /// The parser already knows where a statement ends -
    /// [`Connection::prepare_with_tail`] has been asking it since `ATTACH`
    /// and per-connection temp databases landed - so this asks the same
    /// question and there is now one opinion about
    /// statement boundaries instead of two.
    ///
    /// @param sql - the statements, separated by semicolons
    pub fn execute_batch(&self, sql: &str) -> DbResult<()> {
        let mut rest = sql;
        loop {
            // Separators and trivia are not statements. The parser refuses text
            // that holds none, so an empty batch, a trailing `;` and a script
            // that is nothing but a comment have to be recognised here rather
            // than reported as syntax errors - all three are things a caller
            // legitimately passes.
            let skipped = leading_trivia(rest);
            rest = rest.get(skipped..).unwrap_or("");
            if rest.is_empty() {
                return Ok(());
            }
            let consumed = self.engine()?.statement_length(rest)?;
            let Some(head) = rest.get(..consumed) else {
                return Ok(());
            };
            if head.trim().is_empty() {
                return Ok(());
            }
            let outcome = self.engine_mut()?.execute_any(head, &Params::new())?;
            self.database.changes.set(outcome.changes.rows as i64);
            rest = rest.get(consumed..).unwrap_or("");
        }
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
        let outcome = self.engine_mut()?.execute_any(sql, params)?;
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
    pub fn prepare_with_tail(&self, sql: &str) -> DbResult<Prepared<'d>> {
        let consumed = self.engine()?.statement_length(sql)?;
        let head = sql.get(..consumed).unwrap_or(sql);
        Ok(Prepared {
            statement: self.prepare(head)?,
            consumed,
        })
    }

    /// Describes how a statement would be run.
    ///
    /// The operator chain, which is what `EXPLAIN QUERY PLAN` answers. There is
    /// no bytecode listing because there is no bytecode.
    ///
    /// **Goes through the textual `EXPLAIN QUERY PLAN` path rather than
    /// [`crate::ImportedDatabase::plan`].** That method only binds a `SELECT` -
    /// it is the entry point for the "plan once, execute many" profiling
    /// harnesses, which only ever measure reads - so calling it on an `UPDATE`
    /// or `DELETE` answered "is not a read-only statement" even though the
    /// engine can describe a write's plan: `compile()` already does, for the
    /// same reason SQLite can `EXPLAIN QUERY PLAN` a write - the plan being
    /// described is the search that finds the rows to change, not the change
    /// itself. Running the query this way reaches that code path instead.
    ///
    /// @param sql - the statement
    pub fn explain(&self, sql: &str) -> DbResult<Vec<String>> {
        let rows = self.query(&format!("EXPLAIN QUERY PLAN {sql}"))?;
        Ok(rows
            .into_iter()
            .filter_map(|mut row| match row.pop() {
                Some(OwnedDatum::Text(bytes)) => Some(String::from_utf8_lossy(&bytes).into_owned()),
                _ => None,
            })
            .collect())
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
        self.engine_mut()?
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
        self.engine_mut()?
            .create_aggregate_function(name, arity, flags, body)
    }

    /// Removes a function by name and arity, reporting whether one went.
    ///
    /// @param name - the name it was registered under
    /// @param arity - the arity it was registered for
    pub fn remove_function(&self, name: &str, arity: i32) -> DbResult<bool> {
        Ok(self.engine_mut()?.remove_function(name, arity))
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
        self.engine_mut()?.create_collation(name, comparator)
    }

    /// Returns how many statements are compiled and held.
    pub fn cached_plan_count(&self) -> DbResult<usize> {
        Ok(self.database.plans.held())
    }

    /// Returns how many statements this connection has compiled since it opened.
    ///
    /// What a plan cache is for is that the second prepare of a statement does
    /// not compile it again, and this is the number that says whether it did.
    /// It is here rather than only in the engine because
    /// `crates/inillucent/tests/budget.rs` asserts on it, and that file writes
    /// against this facade.
    pub fn compiled_statement_count(&self) -> DbResult<u64> {
        Ok(self.database.plans.compiles())
    }

    /// Turns off one or more planner optimizations for this connection.
    ///
    /// @param mask - the levers to switch off
    pub fn disable_optimizations(&self, levers: inillucent_sql::plan::Levers) -> DbResult<()> {
        // The plan cache is keyed by the levers rather than cleared by them -
        // see `ImportedDatabase::disable_optimizations` - so setting the value
        // is the whole of it, and it needs no borrow of the engine.
        self.database.settings.set_levers(levers);
        Ok(())
    }

    /// Puts the connection into or out of defensive mode.
    ///
    /// `SQLITE_DBCONFIG_DEFENSIVE`, which the reference's shell turns on by
    /// default: it refuses `PRAGMA journal_mode = OFF` and
    /// `PRAGMA writable_schema = ON`, both of which let a caller lose or
    /// corrupt a database with one statement.
    ///
    /// **Through the engine, so the registry hears it too (task-1972).** It
    /// used to set the shared pragma record and nothing else, and
    /// `Registry::authorize_shadow_write` reads `Policy::defensive` - so the
    /// half of this flag that is about a module's private storage was set on
    /// one copy and read from another, and refused nothing.
    ///
    /// @param on - whether the flag is in force
    pub fn set_defensive(&self, on: bool) -> DbResult<()> {
        self.engine_mut()?.set_defensive(on);
        Ok(())
    }

    /// Installs the authorizer every later statement is bound under.
    ///
    /// `sqlite3_set_authorizer`: the callback is consulted before a read, a
    /// select or a function call is bound, and a `Deny` refuses the statement.
    /// Pass `None` to allow everything again.
    ///
    /// @param authorizer - the callback, or nothing
    pub fn set_authorizer(
        &self,
        authorizer: Option<std::rc::Rc<dyn crate::Authorizer>>,
    ) -> DbResult<()> {
        self.engine_mut()?.set_authorizer(authorizer);
        Ok(())
    }

    /// Declares a table over one index's own b-tree, or removes every such
    /// declaration.
    ///
    /// `.imposter`'s subject. Returns the `CREATE` it made, so a caller can
    /// print it the way the reference's shell does.
    ///
    /// @param index - the index to read, or nothing to remove them all
    /// @param name - the table name to declare it under
    pub fn imposter(&self, index: Option<&[u8]>, name: &[u8]) -> DbResult<Option<String>> {
        self.engine_mut()?.imposter(index, name)
    }

    /// Rereads the schema from the file.
    pub fn reload_schema(&self) -> DbResult<()> {
        self.engine_mut()?.reload_catalog()
    }

    /// Returns the schema's generation, which changes when the schema does.
    pub fn schema_cookie(&self) -> DbResult<u64> {
        Ok(self.engine()?.schema_generation())
    }

    /// Returns how many parameters one statement declares.
    ///
    /// The bound a caller supplied bind index is checked against; see
    /// `inillucent_driver::Connection::parameter_count` for what an unbounded
    /// one cost.
    ///
    /// @param sql - the statement text
    pub fn parameter_count(&self, sql: &str) -> DbResult<u32> {
        self.database.engine.borrow().parameter_count(sql)
    }

    /// Returns the named parameters one statement declares, with their indexes.
    ///
    /// What a shell needs to bind `.parameter set :name value` onto a statement
    /// it did not write.
    ///
    /// @param sql - the statement text
    pub fn parameter_names(&self, sql: &str) -> DbResult<Vec<(Vec<u8>, u32)>> {
        self.database.engine.borrow().parameter_names(sql)
    }

    /// Compiles a statement to be bound and stepped.
    ///
    /// @param sql - the statement
    pub fn prepare(&self, sql: &str) -> DbResult<Statement<'d>> {
        // **One parse, because the compilation already read the count**
        // (task-2066 §4.3.3). This used to call `parameter_count(sql)` and
        // then `prepare_statement(sql)`, and the order mattered: both go
        // through the same recycled parse arena, so asking the count second
        // reached into the arena the plan had just been built out of, and
        // correlated subqueries in an `UPDATE` or a `DELETE` quietly answered
        // against the wrong rows.
        //
        // The order is no longer load-bearing because there is only one parse.
        // `prepare_statement` answers the count beside the plan, and on a plan
        // cache hit it answers it without parsing at all - which is the path
        // that used to parse every time for a number it already had.
        let compiled = self.engine()?.prepare_statement(sql)?;
        let declared = compiled.parameters;
        let generation = self.engine()?.schema_generation();
        let mut params = Params::new();
        params.expect(declared);
        Ok(Statement {
            database: self.database,
            sql: sql.to_string(),
            compiled,
            generation,
            params,
            rows: Vec::new(),
            names: std::rc::Rc::new(Vec::new()),
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
        self.changes()
    }

    /// Returns how many rows the last statement on this database changed.
    ///
    /// **Read off the engine, which is where the SQL scalar reads it.** It was
    /// a cell on the `Database`, set by the two wrappers below from the
    /// `Outcome` they got back - so a statement that failed partway left it
    /// holding the previous statement's number, and `sqlite3_changes` and
    /// `changes()` could answer differently about the same statement.
    pub fn changes(&self) -> DbResult<i64> {
        Ok(self.database.counters.last_changes.get())
    }

    /// Returns how many rows every statement so far has changed.
    /// **This connection's own number rather than the engine's**, which is
    /// the same answer and one fewer thing to be stale: the engine's copy is
    /// whatever the last `use_session` set, and a connection knows which one it
    /// is without asking.
    pub fn total_changes(&self) -> DbResult<i64> {
        Ok(self
            .database
            .counters
            .session_change_baseline
            .total_changes(self.session, self.database.counters.changed_ever.get()))
    }

    /// Returns the rowid the last `INSERT` assigned.
    pub fn last_insert_rowid(&self) -> DbResult<i64> {
        Ok(self.database.counters.last_rowid.get())
    }

    /// Returns how many databases the last commit was decided over.
    ///
    /// One for an ordinary statement; two or more for a transaction that wrote
    /// two files and was therefore committed through a super-journal.
    pub fn decided_over(&self) -> DbResult<usize> {
        Ok(self.database.writer.decided_over())
    }

    /// Returns whether every statement is its own transaction.
    ///
    /// `false` between a `BEGIN` and its `COMMIT`.
    ///
    /// **Answerable while a statement is running (task-1962, A1 step 3).** This
    /// read the engine, so a function registered on the connection that asked
    /// it got the `already running a statement` error - and this is exactly the
    /// question such a function asks, because SQLite's `sqlite3_get_autocommit`
    /// is documented as callable from a callback. It reads the writer the
    /// database holds beside the engine instead, and takes no cell at all.
    pub fn autocommit(&self) -> DbResult<bool> {
        Ok(self.database.writer.batch().is_none())
    }

    /// Opens a transaction that rolls back unless it is committed.
    ///
    /// **The value is the guard (task-1961, A4).** Before this, a caller of the
    /// engine wrote `BEGIN` through [`Connection::execute_batch`] and had to
    /// remember the `COMMIT` on every path out of the function it was in - so
    /// an early return, a `?` or a panic left the transaction open for the life
    /// of the connection, holding the write lock and hiding every later
    /// statement's work from anybody else. The driver has had this shape since
    /// task-1932; it lives here now so the engine's own connection has it and
    /// the driver marshals rather than decides.
    ///
    /// Refuses when a transaction is already open, and the check is
    /// [`Connection::autocommit`] rather than a counter this type keeps,
    /// because in this engine a transaction belongs to the *database* and not
    /// to the connection that opened it: a `BEGIN` on one handle is joined by
    /// every other handle on the same file, which is what the invariant on
    /// [`Database::connect`] says. A counter per connection would answer "no
    /// transaction here" while one was open on a sibling, and committing it
    /// would settle work the sibling had not finished.
    pub fn begin(&self) -> DbResult<Transaction<'_>> {
        if !self.autocommit()? {
            // `SQLITE_ERROR`, which is what the pinned reference answers for
            // `BEGIN` inside a transaction ("cannot start a transaction within
            // a transaction"), rather than the `SQLITE_MISUSE` `refusal`
            // hands out.
            return Err(inillucent_base::error::statement_refusal(
                "a transaction is already open on this database; this engine does not nest                  them, because committing the inner one would commit the outer one's work too.",
            ));
        }
        self.execute_batch("BEGIN")?;
        Ok(Transaction {
            connection: self,
            settled: std::cell::Cell::new(false),
        })
    }
}

/// An open transaction, which rolls back unless it is committed.
///
/// Invariant: **when this value goes away, the transaction it opened is over.**
/// Either [`Transaction::commit`] kept the work, or [`Transaction::rollback`]
/// discarded it, or the `Drop` below discarded it. There is no fourth way out,
/// which is the reason the type exists: the alternative is a `BEGIN` in text
/// and a `COMMIT` the caller has to reach on every path.
///
/// It borrows the connection, so the connection cannot be dropped or used for a
/// second transaction while one is open.
pub struct Transaction<'c> {
    /// The connection the `BEGIN` was issued on.
    connection: &'c Connection<'c>,
    /// Whether the transaction has already been settled, so `Drop` does
    /// nothing. A `Cell` because `commit` and `rollback` take `self` by value
    /// and `Drop` takes `&mut self`, and both have to write it.
    settled: std::cell::Cell<bool>,
}

impl std::fmt::Debug for Transaction<'_> {
    /// Says whether the transaction is still open, and nothing a caller wrote.
    fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        out.debug_struct("Transaction")
            .field("settled", &self.settled.get())
            .finish()
    }
}

impl Transaction<'_> {
    /// Runs one statement inside the transaction for its effect.
    ///
    /// @param sql - the statement
    pub fn execute(&self, sql: &str) -> DbResult<i64> {
        self.connection.execute(sql)
    }

    /// Runs one statement inside the transaction and returns its rows.
    ///
    /// @param sql - the statement
    pub fn query(&self, sql: &str) -> DbResult<Vec<Vec<OwnedDatum>>> {
        self.connection.query(sql)
    }

    /// Compiles a statement to run inside the transaction.
    ///
    /// @param sql - the statement
    pub fn prepare(&self, sql: &str) -> DbResult<Statement<'_>> {
        self.connection.prepare(sql)
    }

    /// Keeps everything this transaction wrote.
    ///
    /// Takes `self`, so a committed transaction cannot be used again and the
    /// `Drop` below cannot roll back what was kept.
    pub fn commit(self) -> DbResult<()> {
        self.settled.set(true);
        self.connection.execute_batch("COMMIT")
    }

    /// Discards everything this transaction wrote.
    ///
    /// The same thing dropping it does, said out loud. A caller that has
    /// decided to abandon the work reads better for saying so, and a rollback
    /// that fails is reportable here where it is not from `Drop`.
    pub fn rollback(self) -> DbResult<()> {
        self.settled.set(true);
        self.connection.execute_batch("ROLLBACK")
    }
}

impl Drop for Transaction<'_> {
    /// Rolls back a transaction nobody committed.
    ///
    /// **Silent, because a `Drop` has nowhere to report to.** What it could
    /// hide is a rollback that did not happen, and for that the engine would
    /// have to refuse a `ROLLBACK` on a transaction it opened itself. A caller
    /// who wants to know calls [`Transaction::rollback`] and reads the answer.
    fn drop(&mut self) {
        if self.settled.get() {
            return;
        }
        self.settled.set(true);
        let _ = self.connection.execute_batch("ROLLBACK");
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
    /// The text this statement was compiled from.
    ///
    /// Kept so a schema change can be answered by recompiling, the way
    /// SQLite's own automatic reprepare does - see the schema check in
    /// [`Statement::step`].
    sql: String,
    /// The engine's compiled handle, reused across executions **while the
    /// schema it was compiled against is still current**.
    compiled: crate::Statement,
    /// The schema generation `compiled` was built against.
    ///
    /// Compared against [`crate::ImportedDatabase::schema_generation`] at the
    /// start of every fresh run, so a statement recompiles itself rather than
    /// answering from a plan built against a schema that has since moved.
    generation: u64,
    /// The values bound so far.
    params: Params,
    /// The rows the last execution produced.
    rows: Vec<Vec<OwnedDatum>>,
    /// The result column names.
    names: std::rc::Rc<Vec<String>>,
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
    /// **Refuses an index the statement does not have**, with the code SQLite
    /// uses for it. This used to answer `Ok` to anything: the parameter set did
    /// `index.saturating_sub(1)` and then grew to fit, so binding 9 on a
    /// one-parameter statement quietly made nine slots, and binding **0 wrote
    /// over `?1`**. The second is the one that loses data rather than time - a
    /// caller who thought index 0 was a no-op had replaced its first parameter,
    /// and the statement ran with a value it never chose and no error to say so.
    ///
    /// @param index - the one-based parameter number
    /// @param value - the value
    pub fn bind(&mut self, index: u32, value: OwnedDatum) -> DbResult<()> {
        if self.params.try_set(index, value).is_err() {
            return Err(DbError::primary(PrimaryCode::Range).with_detail(format!(
                "parameter {index} is outside the {} this statement has",
                self.params.declared().unwrap_or(0)
            )));
        }
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

    /// Unbinds every parameter, keeping how many the statement has.
    ///
    /// The count is a property of the statement rather than of the values, so
    /// clearing the values must not forget it - a statement whose bindings were
    /// cleared would otherwise start accepting any index again.
    pub fn clear_bindings(&mut self) {
        let declared = self.params.declared();
        self.params.clear();
        if let Some(declared) = declared {
            self.params.expect(declared);
        }
        self.run = false;
    }

    /// Runs the statement if it has not run, then advances to the next row.
    ///
    /// Returns whether a row is available to [`Statement::row`].
    ///
    /// **Recompiles first when the schema has moved under it.** A plan
    /// carries decisions - which tree it reads, how many columns `*` expands
    /// to - that a schema change can make wrong, and this is the one caller
    /// that still holds the old plan after [`Connection::reload_schema`] has
    /// cleared the connection's cache and moved on: a fresh `prepare` of the
    /// same text would already get the new plan, so an already-prepared
    /// statement answering with the old one is the gap. SQLite's own
    /// `sqlite3_step` does the equivalent automatic reprepare.
    pub fn step(&mut self) -> DbResult<bool> {
        if !self.run {
            let mut held = self.database.engine.borrow_mut();
            // The statement runs on the connection that compiled it, because
            // `temp` means that connection's temporary database and the plan was
            // bound against it.
            held.use_session(self.session);
            if held.schema_generation() != self.generation {
                self.compiled = held.prepare_statement(&self.sql)?;
                self.generation = held.schema_generation();
            }
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

/// Returns how many bytes at the front of a script are not part of a statement.
///
/// Whitespace, statement separators, and both comment forms. The parser refuses
/// text that holds no statement, so a caller passing an empty batch, a trailing
/// semicolon or a file that is entirely comments would get a syntax error for
/// something that is not an error - and all three are ordinary things to pass.
///
/// This is deliberately the *only* lexical scanning left in the batch path. It
/// decides what to skip, never where a statement ends; that question goes to
/// the parser, which is the half the old splitter got wrong.
///
/// @param sql - the remaining script
pub fn leading_trivia(sql: &str) -> usize {
    let bytes = sql.as_bytes();
    let mut at = 0usize;
    loop {
        let before = at;
        while let Some(byte) = bytes.get(at) {
            if byte.is_ascii_whitespace() || *byte == b';' {
                at += 1;
            } else {
                break;
            }
        }
        if bytes.get(at) == Some(&b'-') && bytes.get(at + 1) == Some(&b'-') {
            at += 2;
            while let Some(byte) = bytes.get(at) {
                at += 1;
                if *byte == b'\n' {
                    break;
                }
            }
        }
        if bytes.get(at) == Some(&b'/') && bytes.get(at + 1) == Some(&b'*') {
            at += 2;
            while at < bytes.len() {
                if bytes.get(at) == Some(&b'*') && bytes.get(at + 1) == Some(&b'/') {
                    at += 2;
                    break;
                }
                at += 1;
            }
        }
        if at == before {
            return at;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Trivia is skipped, and a statement's own semicolons are not trivia.
    #[test]
    fn leading_trivia_skips_separators_and_comments() {
        assert_eq!(leading_trivia(""), 0);
        assert_eq!(leading_trivia(";;;  "), 5);
        assert_eq!(leading_trivia("-- a comment\nSELECT 1"), 13);
        assert_eq!(leading_trivia("/* block */ SELECT 1"), 12);
        assert_eq!(
            leading_trivia("SELECT 1"),
            0,
            "a statement is not trivia, however it starts"
        );
        assert_eq!(
            leading_trivia("  ;\n-- one\n/* two */ ;\nSELECT 1"),
            23,
            "the forms mix, in any order"
        );
    }

    /// Returns a scratch directory of this test's own.
    ///
    /// @param name - what to call it
    fn scratch(name: &str) -> std::path::PathBuf {
        let root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(|parent| parent.parent())
            .map(Path::to_path_buf)
            .unwrap_or_default()
            .join("_agent_output/engine_open")
            .join(name);
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::create_dir_all(&root);
        root
    }

    /// A path that holds nothing is a path to create a database at.
    #[test]
    fn a_path_that_holds_nothing_is_one_to_create_at() {
        let root = scratch("absent");
        assert_eq!(
            there_is_a_database_at(&root.join("absent.rdb")).expect("it answers"),
            false
        );
    }

    /// A file is a database to open.
    #[test]
    fn a_file_is_a_database_to_open() {
        let root = scratch("present");
        let path = root.join("there.rdb");
        std::fs::write(&path, b"anything").expect("the file writes");
        assert_eq!(there_is_a_database_at(&path).expect("it answers"), true);
    }

    /// Anything else is refused by name, and is not deleted (task-2070).
    ///
    /// **The create branch deletes whatever is at the path before it writes**,
    /// so a question the file system could not answer used to be answered as
    /// "nothing is there" and cost the caller their database. A directory is
    /// the case that can be built in a test; a permission error and an
    /// unreachable share reach the same arm.
    #[test]
    fn something_that_is_not_a_file_is_refused_rather_than_created_over() {
        let root = scratch("directory");
        let path = root.join("a_directory.rdb");
        std::fs::create_dir_all(path.join("inside")).expect("the directory is made");
        let refusal = there_is_a_database_at(&path).expect_err("it refuses");
        let said = refusal.detail().unwrap_or_else(|| refusal.message());
        assert!(said.contains("is not a file"), "{said}");
        assert!(
            path.join("inside").is_dir(),
            "the refusal must not have removed anything"
        );

        // And the refusal reaches a caller of `open`, rather than an empty
        // database where their data used to be.
        let opened = Database::open(&path);
        assert!(
            opened.is_err(),
            "open must refuse a path that is not a file"
        );
        assert!(
            path.join("inside").is_dir(),
            "a refused open must leave the path alone"
        );
    }

    /// Removes the log segments beside a database.
    ///
    /// @param path - the database file
    fn take_the_log_away(path: &Path) {
        let Some(parent) = path.parent() else {
            return;
        };
        let Ok(entries) = std::fs::read_dir(parent) else {
            return;
        };
        for entry in entries.flatten() {
            if entry.file_name().to_string_lossy().contains("-wal.") {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }

    /// An open that cannot account for the file's length leaves it alone
    /// (task-2070).
    ///
    /// **The trim cuts the file to `page_count * page_size`, and `page_count`
    /// comes from the meta record.** So a meta record that is behind the file
    /// turns giving back an unowned tail into deleting pages the catalog is
    /// still pointing at. This builds that state exactly: a database, then the
    /// meta record its own *creation* wrote put back over it, which is what a
    /// checkpoint would leave if it folded every page and then failed to land
    /// the record.
    ///
    /// Before the guard this shortened a 360,448 byte database to the 131,072
    /// bytes the creation's record describes, and the next open then failed
    /// reading a page that open had just discarded.
    #[test]
    fn an_open_that_cannot_account_for_the_file_does_not_shorten_it() {
        let root = scratch("unaccounted_tail");
        let path = root.join("staged.rdb");

        let database = Database::open(&path).expect("it is created");
        drop(database);
        let creation = std::fs::read(&path).expect("the created file reads");

        let database = Database::open(&path).expect("it reopens");
        let connection = database.session();
        connection
            .execute_batch("CREATE TABLE note(id INTEGER PRIMARY KEY, body TEXT)")
            .expect("the table is created");
        for row in 0..600 {
            connection
                .execute_batch(&format!(
                    "INSERT INTO note(id, body) VALUES ({row}, '{}')",
                    "x".repeat(300)
                ))
                .expect("the row is inserted");
        }
        database.checkpoint().expect("it checkpoints");
        let _ = connection;
        drop(database);

        let written = std::fs::read(&path).expect("the written file reads");
        assert!(
            written.len() > creation.len(),
            "the workload has to grow the file past the pages a creation leaves"
        );

        let mut bytes = written.clone();
        for slot in 0..2 {
            let at = slot * PAGE_SIZE;
            let (Some(source), Some(target)) = (
                creation.get(at..at.saturating_add(PAGE_SIZE)),
                bytes.get_mut(at..at.saturating_add(PAGE_SIZE)),
            ) else {
                continue;
            };
            target.copy_from_slice(source);
        }
        std::fs::write(&path, &bytes).expect("the file writes");
        take_the_log_away(&path);

        let database = Database::open(&path).expect("it opens");
        let rows = database
            .session()
            .query("SELECT name FROM sqlite_schema ORDER BY name")
            .expect("the catalog lists");
        drop(database);
        assert_eq!(rows.len(), 1, "the catalog still names the table: {rows:?}");
        assert_eq!(
            std::fs::metadata(&path).expect("it is still there").len(),
            written.len() as u64,
            "the open must not have shortened the file"
        );

        // And it opens again, which is the half the shortening took away: the
        // second open used to fail on a page the first had just discarded.
        // The file itself is byte for byte what it was, so the rows the header
        // cannot account for are still there for a repair to find.
        let again = Database::open(&path).expect("it opens again");
        let rows = again
            .session()
            .query("SELECT name FROM sqlite_schema ORDER BY name")
            .expect("the catalog lists again");
        drop(again);
        assert_eq!(rows.len(), 1, "and still names the table: {rows:?}");
        assert_eq!(
            std::fs::read(&path).expect("it still reads"),
            bytes,
            "two opens in a row must leave the file byte for byte as it was"
        );
    }
}
