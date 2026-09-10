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
}

/// Reports whether a path names an in-memory database rather than a file.
///
/// `:memory:` and the empty path, which are SQLite's two spellings for it.
///
/// @param path - the path a caller opened with
fn is_memory(path: &Path) -> bool {
    path.as_os_str().is_empty() || path.as_os_str() == ":memory:"
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
            let engine = ImportedDatabase::create_on(vfs, path.clone(), PAGE_SIZE, frames)?;
            return Ok(Database {
                engine: RefCell::new(engine),
                path,
                changes: std::cell::Cell::new(0),
            });
        }
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
            let consumed = self.engine().statement_length(rest)?;
            let Some(head) = rest.get(..consumed) else {
                return Ok(());
            };
            if head.trim().is_empty() {
                return Ok(());
            }
            let outcome = self.engine_mut().execute_any(head, &Params::new())?;
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

    /// Returns how many statements this connection has compiled since it opened.
    ///
    /// What a plan cache is for is that the second prepare of a statement does
    /// not compile it again, and this is the number that says whether it did.
    /// It is here rather than only in the engine because
    /// `crates/inillucent/tests/budget.rs` asserts on it, and that file writes
    /// against this facade.
    pub fn compiled_statement_count(&self) -> u64 {
        self.engine().compiled_statement_count()
    }

    /// Turns off one or more planner optimizations for this connection.
    ///
    /// @param mask - the levers to switch off
    pub fn disable_optimizations(&self, mask: u32) {
        self.engine_mut().disable_optimizations(mask);
    }

    /// Puts the connection into or out of defensive mode.
    ///
    /// `SQLITE_DBCONFIG_DEFENSIVE`, which the reference's shell turns on by
    /// default: it refuses `PRAGMA journal_mode = OFF` and
    /// `PRAGMA writable_schema = ON`, both of which let a caller lose or
    /// corrupt a database with one statement.
    ///
    /// @param on - whether the flag is in force
    pub fn set_defensive(&self, on: bool) {
        self.engine_mut().set_defensive(on);
    }

    /// Installs the authorizer every later statement is bound under.
    ///
    /// `sqlite3_set_authorizer`: the callback is consulted before a read, a
    /// select or a function call is bound, and a `Deny` refuses the statement.
    /// Pass `None` to allow everything again.
    ///
    /// @param authorizer - the callback, or nothing
    pub fn set_authorizer(&self, authorizer: Option<std::rc::Rc<dyn crate::Authorizer>>) {
        self.engine_mut().set_authorizer(authorizer);
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
        self.engine_mut().imposter(index, name)
    }

    /// Rereads the schema from the file.
    pub fn reload_schema(&self) -> DbResult<()> {
        self.engine_mut().reload_catalog()
    }

    /// Returns the schema's generation, which changes when the schema does.
    pub fn schema_cookie(&self) -> u64 {
        self.engine().schema_generation()
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
        // **The count is asked before the plan is compiled, not after.** Both
        // go through the same recycled parse arena, and a parse clears it - so
        // asking afterwards reached into the arena the plan had just been built
        // out of. The symptom was not a crash: correlated subqueries in an
        // `UPDATE` or a `DELETE` quietly answered against the wrong rows.
        let declared = self.engine().parameter_count(sql)?;
        let compiled = self.engine().prepare_statement(sql)?;
        let mut params = Params::new();
        params.expect(declared);
        Ok(Statement {
            database: self.database,
            compiled,
            params,
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
    ///
    /// **Read off the engine, which is where the SQL scalar reads it.** It was
    /// a cell on the `Database`, set by the two wrappers below from the
    /// `Outcome` they got back - so a statement that failed partway left it
    /// holding the previous statement's number, and `sqlite3_changes` and
    /// `changes()` could answer differently about the same statement.
    pub fn changes(&self) -> i64 {
        self.engine().changes()
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
}
