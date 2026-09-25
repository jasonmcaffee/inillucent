//! One database, reached from several threads, one statement at a time.
//!
//! Invariant: **serialized, not parallel.** SQLite's own word for it, and the
//! same promise: a database may be used from any number of threads, and exactly
//! one statement runs at a time. No statement runs in parallel with another, no
//! statement is split across threads, and the executor is unchanged. What this
//! buys is that an application with a thread pool does not need a connection per
//! thread and a protocol for handing them around.
//!
//! ## Why the database gets a thread of its own
//!
//! A [`crate::Database`] holds `Rc` - the engine's state groups, the log, the
//! compiled plans, the layouts - so it is neither `Send` nor `Sync`, and the
//! crate documentation says so as a contract rather than an oversight. The
//! obvious way to share one is `Arc<Mutex<Database>>` with an
//! `unsafe impl Send`, and the argument for it is that the `Rc` graph is
//! reachable only through the mutex. That argument has a hole: the driver's own
//! `Connection::set_authorizer` takes an `Rc<dyn Authorizer>` the **caller**
//! keeps a clone of, so a database that had one installed would have a live
//! handle on two threads and a non-atomic count between them. The hole is
//! closable by leaving `set_authorizer` off this surface, but then the
//! soundness of a shipped `unsafe` rests on a method not being added later.
//!
//! So the database is not moved between threads at all. It is opened on a
//! thread of its own and never leaves it; the handles here send it statements
//! and wait for the answer. `inillucent-driver` keeps `#![forbid(unsafe_code)]`
//! and the confinement is the compiler's rather than a paragraph's.
//!
//! The cost is a thread per shared database and a channel round trip per
//! statement - two context switches, tens of microseconds - against a statement
//! that takes at least that. It buys an arrangement where "the engine is single
//! threaded" is enforced rather than promised.
//!
//! ## What makes one transaction at a time true
//!
//! The owner thread runs whatever arrives, in order, so two threads' statements
//! interleave - which is right for statements and wrong for a transaction. In
//! this engine a transaction belongs to the *database* rather than to the handle
//! that opened it, so another thread's `INSERT` between a `BEGIN` and its
//! `COMMIT` would join that transaction and be committed by it.
//!
//! [`SharedTransaction`] therefore takes a turn lock for its whole life, and
//! every other statement takes it for the length of one statement. The lock
//! guards no data - the data is on the owner thread - so there is nothing to
//! poison and nothing to reason about beyond the order.

use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex, MutexGuard};

use crate::{Database, Error, OpenOptions, Result, Rows, Status, Value};

/// One piece of work for the thread that owns the database.
enum Job {
    /// Run one statement and send back its rows.
    Query {
        /// The statement.
        sql: String,
        /// The values bound to `?1`, `?2`, ...
        params: Vec<Value>,
        /// How many rows to hand back.
        limit: usize,
        /// Where the answer goes.
        reply: Sender<Result<Rows>>,
    },
    /// Run several statements for their effect.
    Batch {
        /// The statements, separated by semicolons.
        sql: String,
        /// Where the answer goes.
        reply: Sender<Result<()>>,
    },
    /// Close the database and stop.
    Stop,
}

/// The thread that owns the database, and the turn every caller takes.
struct Owner {
    /// Where work goes.
    work: Sender<Job>,
    /// Whose turn it is. Guards no data: it is the order, not the database.
    turn: Mutex<()>,
    /// The thread, joined when the last handle goes.
    thread: Mutex<Option<std::thread::JoinHandle<()>>>,
}

impl Drop for Owner {
    /// Stops the owner thread and waits for it, so the file is closed before
    /// the last handle's caller can reopen it.
    ///
    /// **Waiting matters more than it looks.** A test that drops a
    /// `SharedDatabase` and opens the same path again would otherwise race the
    /// close, and a database still open holds its lock.
    fn drop(&mut self) {
        let _ = self.work.send(Job::Stop);
        let taken = match self.thread.lock() {
            Ok(mut slot) => slot.take(),
            Err(poisoned) => poisoned.into_inner().take(),
        };
        if let Some(thread) = taken {
            let _ = thread.join();
        }
    }
}

/// A database several threads share, one statement at a time.
///
/// Clone it per thread: every clone is the same database, the same thread and
/// the same turn.
///
/// ```
/// # use inillucent_driver::{Result, SharedDatabase, Value};
/// # fn main() -> Result<()> {
/// # let directory = std::env::temp_dir().join(format!("inillucent-doc-shared-{}", std::process::id()));
/// # std::fs::create_dir_all(&directory).ok();
/// let database = SharedDatabase::open(directory.join("app.rdb"))?;
/// database.execute_batch("CREATE TABLE note (id INTEGER PRIMARY KEY, body TEXT)")?;
///
/// let mut threads = Vec::new();
/// for worker in 0..4i64 {
///     let held = database.clone();
///     threads.push(std::thread::spawn(move || {
///         held.execute("INSERT INTO note (body) VALUES (?1)", &[Value::Integer(worker)])
///     }));
/// }
/// for thread in threads {
///     thread.join().expect("the worker finished")?;
/// }
///
/// let rows = database.query_all("SELECT count(*) FROM note", &[])?;
/// assert_eq!(rows.value(0, 0), Some(&Value::Integer(4)));
/// # drop(database);
/// # std::fs::remove_dir_all(&directory).ok();
/// # Ok(())
/// # }
/// ```
#[derive(Clone)]
pub struct SharedDatabase {
    /// The owner thread and the turn, shared by every handle.
    owner: Arc<Owner>,
    /// The file, kept here so asking for it never waits.
    path: PathBuf,
}

impl std::fmt::Debug for SharedDatabase {
    /// Names the database, and nothing a caller bound.
    ///
    /// @param out - where the rendering goes
    fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        out.debug_struct("SharedDatabase")
            .field("path", &self.path)
            .finish()
    }
}

impl SharedDatabase {
    /// Opens a database on a thread of its own, creating it when the path holds
    /// nothing.
    ///
    /// @param path - the database file
    pub fn open(path: impl AsRef<Path>) -> Result<SharedDatabase> {
        SharedDatabase::open_with(path, OpenOptions::default())
    }

    /// Opens a database with the options stated, on a thread of its own.
    ///
    /// **The open happens on the owner thread**, because the database must be
    /// built by the thread that will hold it - that is the whole arrangement.
    /// A failure to open is sent back before this returns, so a caller sees it
    /// here rather than at its first statement.
    ///
    /// @param path - the database file
    /// @param options - how to open it
    pub fn open_with(path: impl AsRef<Path>, options: OpenOptions) -> Result<SharedDatabase> {
        let wanted = path.as_ref().to_path_buf();
        let (work, jobs) = std::sync::mpsc::channel::<Job>();
        let (opened, told) = std::sync::mpsc::channel::<Result<PathBuf>>();
        let carried = wanted.clone();
        let thread = std::thread::Builder::new()
            .name("inillucent-db".to_string())
            .spawn(move || own(carried, options, &opened, &jobs))
            .map_err(|failure| {
                Error::said(
                    Status::Internal,
                    format!("the database's own thread could not be started: {failure}"),
                )
            })?;
        let path = match told.recv() {
            Ok(answer) => answer?,
            Err(_) => {
                let _ = thread.join();
                return Err(Error::said(
                    Status::Internal,
                    "the database's own thread stopped before it said whether the file opened.",
                ));
            }
        };
        Ok(SharedDatabase {
            owner: Arc::new(Owner {
                work,
                turn: Mutex::new(()),
                thread: Mutex::new(Some(thread)),
            }),
            path,
        })
    }

    /// Returns the file this database is in.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Returns a handle a worker thread holds and runs statements through.
    ///
    /// The same database, the same thread and the same turn; it exists so a
    /// worker can be handed something that only runs statements.
    pub fn session(&self) -> SharedConnection {
        SharedConnection {
            owner: Arc::clone(&self.owner),
        }
    }

    /// Runs one statement and returns every row it produced.
    ///
    /// @param sql - the statement
    /// @param params - the values bound to `?1`, `?2`, ...
    pub fn query_all(&self, sql: &str, params: &[Value]) -> Result<Rows> {
        self.query(sql, params, usize::MAX)
    }

    /// Runs one statement and returns at most `limit` rows.
    ///
    /// @param sql - the statement
    /// @param params - the values bound to `?1`, `?2`, ...
    /// @param limit - how many rows to hand back
    pub fn query(&self, sql: &str, params: &[Value], limit: usize) -> Result<Rows> {
        let _turn = turn(&self.owner);
        ask(&self.owner, sql, params, limit)
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
        let _turn = turn(&self.owner);
        batch(&self.owner, sql)
    }

    /// Opens a transaction that holds the turn until it is settled.
    ///
    /// **This is what makes "one transaction at a time" true across threads.**
    /// In this engine a transaction belongs to the database rather than to the
    /// handle that opened it, so another thread's statement between a `BEGIN`
    /// and its `COMMIT` would join that transaction and be committed by it.
    /// Holding the turn for the transaction's life is what stops that.
    ///
    /// Every other thread's statement waits for the whole transaction. That is
    /// the cost of the promise, and it is why this returns a guard rather than
    /// letting a caller write `BEGIN` themselves.
    pub fn begin(&self) -> Result<SharedTransaction<'_>> {
        let guard = turn(&self.owner);
        batch(&self.owner, "BEGIN")?;
        Ok(SharedTransaction {
            owner: &self.owner,
            guard: Some(guard),
        })
    }
}

/// A handle a worker thread runs statements through.
///
/// The same database, thread and turn as the [`SharedDatabase`] it came from.
#[derive(Clone)]
pub struct SharedConnection {
    /// The owner thread and the turn.
    owner: Arc<Owner>,
}

impl std::fmt::Debug for SharedConnection {
    /// Says what it is, and nothing a caller bound.
    ///
    /// @param out - where the rendering goes
    fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        out.write_str("SharedConnection")
    }
}

impl SharedConnection {
    /// Runs one statement and returns every row it produced.
    ///
    /// @param sql - the statement
    /// @param params - the values bound to `?1`, `?2`, ...
    pub fn query_all(&self, sql: &str, params: &[Value]) -> Result<Rows> {
        self.query(sql, params, usize::MAX)
    }

    /// Runs one statement and returns at most `limit` rows.
    ///
    /// @param sql - the statement
    /// @param params - the values bound to `?1`, `?2`, ...
    /// @param limit - how many rows to hand back
    pub fn query(&self, sql: &str, params: &[Value], limit: usize) -> Result<Rows> {
        let _turn = turn(&self.owner);
        ask(&self.owner, sql, params, limit)
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
        let _turn = turn(&self.owner);
        batch(&self.owner, sql)
    }
}

/// A transaction that holds the database's turn for its whole life.
///
/// Dropping it without committing rolls it back, the way
/// [`crate::Transaction`] does, so an early return cannot leave one open.
pub struct SharedTransaction<'d> {
    /// The owner thread.
    owner: &'d Arc<Owner>,
    /// The turn, held until this is settled or dropped.
    guard: Option<MutexGuard<'d, ()>>,
}

impl std::fmt::Debug for SharedTransaction<'_> {
    /// Says what it is, and nothing a caller bound.
    ///
    /// @param out - where the rendering goes
    fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        out.debug_struct("SharedTransaction")
            .field("open", &self.guard.is_some())
            .finish()
    }
}

impl SharedTransaction<'_> {
    /// Runs one statement inside the transaction and returns its rows.
    ///
    /// @param sql - the statement
    /// @param params - the values bound to `?1`, `?2`, ...
    /// @param limit - how many rows to hand back
    pub fn query(&self, sql: &str, params: &[Value], limit: usize) -> Result<Rows> {
        ask(self.owner, sql, params, limit)
    }

    /// Runs one statement inside the transaction for its effect.
    ///
    /// @param sql - the statement
    /// @param params - the values bound to `?1`, `?2`, ...
    pub fn execute(&self, sql: &str, params: &[Value]) -> Result<u64> {
        Ok(self.query(sql, params, 0)?.affected.unwrap_or(0))
    }

    /// Runs several statements inside the transaction, for their effect.
    ///
    /// @param sql - the statements
    pub fn execute_batch(&self, sql: &str) -> Result<()> {
        batch(self.owner, sql)
    }

    /// Commits, and gives the turn back.
    pub fn commit(mut self) -> Result<()> {
        let settled = batch(self.owner, "COMMIT");
        self.guard = None;
        settled
    }

    /// Rolls back, and gives the turn back.
    pub fn rollback(mut self) -> Result<()> {
        let settled = batch(self.owner, "ROLLBACK");
        self.guard = None;
        settled
    }
}

impl Drop for SharedTransaction<'_> {
    /// Rolls the transaction back when it was neither committed nor rolled back.
    ///
    /// **The failure is ignored here and that is the only honest choice**: a
    /// `Drop` cannot return one, and a panic would turn a failed rollback into a
    /// second failure on top of whatever caused the early return. A caller that
    /// needs to know calls [`Self::rollback`].
    fn drop(&mut self) {
        if self.guard.is_none() {
            return;
        }
        let _ = batch(self.owner, "ROLLBACK");
    }
}

/// Takes the turn, recovering from a poisoned mutex.
///
/// **The mutex guards no data**, so a thread that panicked while holding it
/// left nothing behind to be inconsistent - the database is on its own thread
/// and every statement either completed or failed there. Refusing every later
/// statement because an unrelated caller panicked would turn one bug into the
/// process losing its database.
///
/// @param owner - the owner thread and its turn
fn turn(owner: &Arc<Owner>) -> MutexGuard<'_, ()> {
    match owner.turn.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

/// Sends one statement to the owner thread and waits for its rows.
///
/// @param owner - the owner thread
/// @param sql - the statement
/// @param params - the values bound to `?1`, `?2`, ...
/// @param limit - how many rows to hand back
fn ask(owner: &Arc<Owner>, sql: &str, params: &[Value], limit: usize) -> Result<Rows> {
    let (reply, answer) = std::sync::mpsc::channel();
    owner
        .work
        .send(Job::Query {
            sql: sql.to_string(),
            params: params.to_vec(),
            limit,
            reply,
        })
        .map_err(|_| gone())?;
    answer.recv().map_err(|_| gone())?
}

/// Sends a batch to the owner thread and waits for it.
///
/// @param owner - the owner thread
/// @param sql - the statements
fn batch(owner: &Arc<Owner>, sql: &str) -> Result<()> {
    let (reply, answer) = std::sync::mpsc::channel();
    owner
        .work
        .send(Job::Batch {
            sql: sql.to_string(),
            reply,
        })
        .map_err(|_| gone())?;
    answer.recv().map_err(|_| gone())?
}

/// Builds the failure a caller gets when the owner thread is no longer there.
///
/// It only happens when that thread panicked, which is a defect in this crate
/// rather than in the caller - so the message says where to look rather than
/// blaming the statement.
fn gone() -> Error {
    Error::said(
        Status::Internal,
        "the thread this database runs on is no longer there, so the statement was not run.",
    )
}

/// Opens the database and runs whatever arrives, in order, until told to stop.
///
/// Everything that reaches this function is owned - a statement's text and its
/// bound values - and everything that leaves it is owned. The database itself
/// is built here and dropped here, so no handle into it exists anywhere else.
///
/// @param path - the database file
/// @param options - how to open it
/// @param opened - where the open's outcome goes
/// @param jobs - where the work comes from
fn own(
    path: PathBuf,
    options: OpenOptions,
    opened: &Sender<Result<PathBuf>>,
    jobs: &Receiver<Job>,
) {
    let database = match Database::open_with(&path, options) {
        Ok(database) => {
            let where_it_is = database.path().to_path_buf();
            if opened.send(Ok(where_it_is)).is_err() {
                return;
            }
            database
        }
        Err(failure) => {
            let _ = opened.send(Err(failure));
            return;
        }
    };
    // **One session for the life of the database, not one per statement.** A
    // shared database is one logical connection that several threads take
    // turns on, the way SQLite's serialized mode is, and a session is what
    // `temp`, `ATTACH`, the connection pragmas and `total_changes()` belong
    // to. A new session per job lost all of them between statements: a
    // `CREATE TEMP TRIGGER` was accepted and never fired, because the insert
    // that should have fired it ran in a session that had no such trigger,
    // and `total_changes()` answered 0 after every write.
    let session = database.session().session();
    while let Ok(job) = jobs.recv() {
        match job {
            Job::Query {
                sql,
                params,
                limit,
                reply,
            } => {
                let answered = database.session_as(session).query(&sql, &params, limit);
                if reply.send(answered).is_err() {
                    // The caller gave up waiting. Nothing to report to, and the
                    // statement has already run, so carry on with the next.
                    continue;
                }
            }
            Job::Batch { sql, reply } => {
                let answered = database.session_as(session).execute_batch(&sql);
                if reply.send(answered).is_err() {
                    continue;
                }
            }
            Job::Stop => break,
        }
    }
}
