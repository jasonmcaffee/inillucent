//! The connection: its pager, its catalog snapshot, and its transaction.
//!
//! Invariant: the pager's undo levels and the transaction machine's savepoint
//! levels are pushed and popped together, always through this module. Nothing
//! above it opens a statement level on one and forgets the other, which is
//! what makes `ROLLBACK TO` able to restore the change counters and the pages
//! in one step and be sure they describe the same moment.
//!
//! The second invariant is that a statement holds its transaction for as long
//! as it is running. The first statement to step takes it and the last to
//! finish releases it, so two statements stepped alternately read one snapshot
//! of the file, and a write statement that has produced a RETURNING row still
//! owns the write transaction when it is stepped again.
//!
//! Autocommit is decided here rather than by the statement: a write statement
//! outside an explicit transaction commits when it finishes, and one inside a
//! `BEGIN` does not. That is the whole of the difference, and putting it in one
//! place is what stops a new statement kind forgetting to commit.

use std::cell::RefCell;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use rustdb_base::limits::Limits;
use rustdb_base::{error, DbResult};
use rustdb_catalog::load_database_catalog;
use rustdb_catalog::snapshot::CatalogSnapshot;
use rustdb_storage::pager::{Pager, PagerOptions};
use rustdb_transaction::journal::{JournalMode, JournalOptions, Synchronous};
use rustdb_transaction::recovery::{open_database, DatabaseOptions};
use rustdb_transaction::state::{
    BeginMode, ChangeCounters, Transaction, TransactionState, TransactionStats,
};
use rustdb_vfs::os::OsVfs;
use rustdb_vfs::path::DbPath;
use rustdb_vfs::Vfs;
use rustdb_vm::program::{RowChange, RowChangeKind};

/// How a database is opened.
#[derive(Clone, Debug)]
pub struct OpenOptions {
    /// The run-time limits the connection starts with.
    pub limits: Limits,
    /// The name the main database is attached under.
    pub main_name: Vec<u8>,
    /// How long to keep retrying a lock that another connection holds.
    ///
    /// Zero is SQLite's default: a busy file returns `SQLITE_BUSY` at once and
    /// the application decides. It is a deliberate default rather than a
    /// convenient one, because a library that silently waits turns a lock
    /// contention problem into a latency problem nobody can see.
    ///
    /// It is worth setting for another reason on Windows: the read lock is
    /// taken in two steps serialised by the PENDING byte, and two *readers*
    /// starting at the same instant collide on it even though neither is a
    /// writer. That collision is transient and a short timeout absorbs it.
    pub busy_timeout: std::time::Duration,
    /// The journal mode and durability level the connection starts in.
    pub journal: JournalOptions,
    /// Whether the connection may write.
    pub writable: bool,
}

impl Default for OpenOptions {
    /// Returns the defaults SQLite opens with.
    fn default() -> OpenOptions {
        OpenOptions {
            limits: Limits::default(),
            main_name: b"main".to_vec(),
            busy_timeout: std::time::Duration::ZERO,
            journal: JournalOptions::default(),
            writable: true,
        }
    }
}

/// A database file, and the connections onto it.
pub struct SessionDatabase {
    path: DbPath,
    vfs: Arc<dyn Vfs>,
    options: OpenOptions,
}

impl SessionDatabase {
    /// Opens a database file.
    pub fn open(path: impl AsRef<std::path::Path>) -> DbResult<SessionDatabase> {
        SessionDatabase::open_with(path, Arc::new(OsVfs::new()), OpenOptions::default())
    }

    /// Opens a database file on the operating-system VFS with explicit options.
    ///
    /// The facade above cannot name a VFS - it does not depend on that crate,
    /// and should not - so the default one is chosen here.
    pub fn open_with_options(
        path: impl AsRef<std::path::Path>,
        options: OpenOptions,
    ) -> DbResult<SessionDatabase> {
        SessionDatabase::open_with(path, Arc::new(OsVfs::new()), options)
    }

    /// Opens a database file through a given VFS and options.
    pub fn open_with(
        path: impl AsRef<std::path::Path>,
        vfs: Arc<dyn Vfs>,
        options: OpenOptions,
    ) -> DbResult<SessionDatabase> {
        Ok(SessionDatabase {
            path: DbPath::new(path.as_ref().to_path_buf()),
            vfs,
            options,
        })
    }

    /// Returns the path the database was opened from.
    pub fn path(&self) -> &DbPath {
        &self.path
    }

    /// Opens a connection onto the database.
    pub fn connect(&self) -> DbResult<Connection> {
        Connection::open(&self.path, self.vfs.clone(), self.options.clone())
    }
}

/// The mutable half of a connection.
pub struct ConnectionState {
    /// The pager, which owns the file and the page cache.
    pub pager: Pager,
    /// How many statements are holding the transaction open.
    pub active: usize,
    /// The transaction machine: autocommit, savepoints, and counters.
    pub transaction: Transaction,
    /// The journal mode and durability level in force.
    pub journal: JournalOptions,
}

/// What a statement needs from its connection.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Access {
    /// The statement only reads.
    Read,
    /// The statement writes rows, and reports how many.
    Write,
    /// The statement writes, but is not a row count.
    ///
    /// DDL is the case: `CREATE TABLE` writes `sqlite_schema` rows and moves
    /// the cookie, and SQLite still leaves `changes()` reporting whatever the
    /// last INSERT or UPDATE did. Treating it as a write that changed nothing
    /// would zero a counter the application is about to read.
    Schema,
}

impl Access {
    /// Reports whether the statement needs a write transaction.
    pub fn writes(self) -> bool {
        matches!(self, Access::Write | Access::Schema)
    }

    /// Reports whether closing the statement publishes a row count.
    pub fn counts_rows(self) -> bool {
        self == Access::Write
    }
}

/// How a statement ended, which decides what its level does.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Outcome {
    /// It finished; keep its changes.
    Done,
    /// It failed and its own rows are undone.
    Abort,
    /// It failed and its earlier rows are kept.
    Fail,
    /// It failed and the whole transaction is undone.
    Rollback,
}

/// What a connection calls back into when a row changes.
///
/// The arguments are SQLite's: the operation, the database the table is in,
/// the table, and the rowid. The hook is told what happened, not asked - it
/// cannot change the row and cannot run SQL on the connection that called it,
/// which is why it takes no handle.
pub type UpdateHook = Box<dyn Fn(RowChangeKind, &[u8], &[u8], i64)>;

/// What a connection calls back into before a transaction commits.
///
/// Returning `true` vetoes the commit, which is then rolled back - the
/// inversion is SQLite's, whose hook returns non-zero to abort.
pub type CommitHook = Box<dyn Fn() -> bool>;

/// What a connection calls back into after a transaction is rolled back.
pub type RollbackHook = Box<dyn Fn()>;

/// The callbacks a connection fires.
#[derive(Default)]
pub struct Hooks {
    /// Fired once per row changed, in the order the rows changed.
    pub update: Option<UpdateHook>,
    /// Fired before a commit, and able to veto it.
    pub commit: Option<CommitHook>,
    /// Fired after a rollback.
    pub rollback: Option<RollbackHook>,
}

impl std::fmt::Debug for Hooks {
    /// Reports which hooks are set, since a closure has nothing else to say.
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Hooks")
            .field("update", &self.update.is_some())
            .field("commit", &self.commit.is_some())
            .field("rollback", &self.rollback.is_some())
            .finish()
    }
}

/// One connection: a pager, a catalog snapshot, and the statements on it.
pub struct Connection {
    state: RefCell<ConnectionState>,
    catalog: RefCell<Arc<CatalogSnapshot>>,
    interrupt: Arc<AtomicBool>,
    limits: Limits,
    path: DbPath,
    vfs: Arc<dyn Vfs>,
    options: OpenOptions,
    hooks: RefCell<Hooks>,
}

impl Connection {
    /// Opens a connection, recovering a hot journal first, and loads the
    /// catalog.
    pub fn open(path: &DbPath, vfs: Arc<dyn Vfs>, options: OpenOptions) -> DbResult<Connection> {
        let mut pager = open_database(
            Arc::clone(&vfs),
            path,
            DatabaseOptions {
                pager: PagerOptions::default(),
                journal: options.journal,
                writable: options.writable,
            },
        )?;
        let catalog = load_catalog(&mut pager, &options.main_name, 0, options.busy_timeout)?;
        Ok(Connection {
            state: RefCell::new(ConnectionState {
                pager,
                active: 0,
                transaction: Transaction::new(),
                journal: options.journal,
            }),
            catalog: RefCell::new(Arc::new(catalog)),
            interrupt: Arc::new(AtomicBool::new(false)),
            limits: options.limits.clone(),
            path: path.clone(),
            vfs,
            options,
            hooks: RefCell::new(Hooks::default()),
        })
    }

    /// Sets the callback fired once per row changed, returning the old one.
    pub fn set_update_hook(&self, hook: Option<UpdateHook>) -> Option<UpdateHook> {
        self.hooks
            .try_borrow_mut()
            .ok()
            .and_then(|mut hooks| core::mem::replace(&mut hooks.update, hook))
    }

    /// Sets the callback fired before a commit, returning the old one.
    pub fn set_commit_hook(&self, hook: Option<CommitHook>) -> Option<CommitHook> {
        self.hooks
            .try_borrow_mut()
            .ok()
            .and_then(|mut hooks| core::mem::replace(&mut hooks.commit, hook))
    }

    /// Sets the callback fired after a rollback, returning the old one.
    pub fn set_rollback_hook(&self, hook: Option<RollbackHook>) -> Option<RollbackHook> {
        self.hooks
            .try_borrow_mut()
            .ok()
            .and_then(|mut hooks| core::mem::replace(&mut hooks.rollback, hook))
    }

    /// Reports whether an update hook is registered.
    ///
    /// A statement asks before it runs, because logging every row it changes
    /// costs memory proportional to the rows and nobody would read it.
    pub fn wants_row_changes(&self) -> bool {
        self.hooks
            .try_borrow()
            .is_ok_and(|hooks| hooks.update.is_some())
    }

    /// Fires the update hook for one row.
    ///
    /// The hook runs with the connection's own state *not* borrowed, so a hook
    /// that asks the connection a question deadlocks on nothing. It still may
    /// not run SQL on this connection - the statement that called it is in the
    /// middle of running - and that is the same rule SQLite states.
    pub fn fire_update_hook(&self, change: &RowChange) {
        let Ok(hooks) = self.hooks.try_borrow() else {
            return;
        };
        let Some(hook) = hooks.update.as_ref() else {
            return;
        };
        hook(
            change.kind,
            &self.options.main_name,
            &change.table,
            change.rowid,
        );
    }

    /// Asks the commit hook whether the commit may proceed.
    fn commit_is_vetoed(&self) -> bool {
        let Ok(hooks) = self.hooks.try_borrow() else {
            return false;
        };
        hooks.commit.as_ref().is_some_and(|hook| hook())
    }

    /// Fires the rollback hook.
    fn fire_rollback_hook(&self) {
        let Ok(hooks) = self.hooks.try_borrow() else {
            return;
        };
        if let Some(hook) = hooks.rollback.as_ref() {
            hook();
        }
    }

    /// Returns the catalog snapshot statements are compiled against.
    pub fn catalog(&self) -> DbResult<Arc<CatalogSnapshot>> {
        let catalog = self
            .catalog
            .try_borrow()
            .map_err(|_| error::misuse("the catalog is in use"))?;
        Ok(catalog.clone())
    }

    /// Returns the connection's run-time limits.
    pub fn limits(&self) -> &Limits {
        &self.limits
    }

    /// Returns the flag a caller sets to interrupt a running statement.
    pub fn interrupt_flag(&self) -> Arc<AtomicBool> {
        self.interrupt.clone()
    }

    /// Asks the running statement to stop at its next safe point.
    pub fn interrupt(&self) {
        self.interrupt.store(true, Ordering::Relaxed);
    }

    /// Clears a pending interrupt.
    pub fn clear_interrupt(&self) {
        self.interrupt.store(false, Ordering::Relaxed);
    }

    /// Returns whether the connection is in autocommit mode.
    pub fn autocommit(&self) -> bool {
        self.state
            .try_borrow()
            .map_or(true, |state| state.transaction.autocommit())
    }

    /// Returns the change counters.
    pub fn counters(&self) -> ChangeCounters {
        self.state
            .try_borrow()
            .map_or(ChangeCounters::default(), |state| {
                state.transaction.counters()
            })
    }

    /// Returns what the connection's transactions have cost.
    pub fn transaction_stats(&self) -> TransactionStats {
        self.state
            .try_borrow()
            .map_or(TransactionStats::default(), |state| {
                state.transaction.stats()
            })
    }

    /// Returns the journal mode and durability level in force.
    pub fn journal_options(&self) -> JournalOptions {
        self.state
            .try_borrow()
            .map_or(self.options.journal, |state| state.journal)
    }

    /// Returns what the journal has cost since the connection was opened.
    pub fn journal_stats(&self) -> rustdb_storage::JournalStats {
        self.state
            .try_borrow()
            .map_or(rustdb_storage::JournalStats::default(), |state| {
                state.pager.journal_stats()
            })
    }

    /// Returns what the pager has done since the connection was opened.
    pub fn pager_counters(&self) -> rustdb_storage::pager::PagerCounters {
        self.state
            .try_borrow()
            .map_or(rustdb_storage::pager::PagerCounters::default(), |state| {
                state.pager.counters()
            })
    }

    /// Reports whether an explicit transaction is open.
    pub fn transaction_state(&self) -> TransactionState {
        self.state
            .try_borrow()
            .map_or(TransactionState::Autocommit, |state| {
                state.transaction.state()
            })
    }

    /// Runs a closure with the mutable connection state.
    pub fn with_state<T>(&self, body: impl FnOnce(&mut ConnectionState) -> T) -> DbResult<T> {
        let mut state = self
            .state
            .try_borrow_mut()
            .map_err(|_| error::misuse("the connection is already running a statement"))?;
        Ok(body(&mut state))
    }

    /// Opens the level a statement runs inside, taking the transaction when
    /// this is the first statement to need it.
    pub fn begin_statement(&self, access: Access) -> DbResult<()> {
        let timeout = self.options.busy_timeout;
        let mut state = self
            .state
            .try_borrow_mut()
            .map_err(|_| error::misuse("the connection is already running a statement"))?;
        if access.writes() && !self.options.writable {
            return Err(error::DbError::primary(rustdb_base::PrimaryCode::ReadOnly)
                .with_message("attempt to write a readonly database"));
        }
        if !state.pager.state().can_read() {
            begin_read_with_timeout(&mut state.pager, timeout)?;
        }
        if access.writes() {
            let fresh = !state.pager.is_writing();
            begin_write_with_timeout(&mut state.pager, timeout)?;
            state.transaction.promote_to_write()?;
            if fresh {
                open_pending_savepoint_levels(&mut state)?;
            }
            // Only a write transaction has undo levels; a query has nothing
            // to put back, and asking the pager for a level it cannot open
            // would fail on the first SELECT of every connection.
            state.pager.begin_statement()?;
        } else {
            state.transaction.promote_to_read();
        }
        state.transaction.begin_statement(access.counts_rows());
        state.active = state.active.saturating_add(1);
        Ok(())
    }

    /// Closes the level a statement ran inside, and commits when the statement
    /// was the whole transaction.
    pub fn end_statement(&self, access: Access, outcome: Outcome) -> DbResult<()> {
        let mut state = self
            .state
            .try_borrow_mut()
            .map_err(|_| error::misuse("the connection is already running a statement"))?;
        let writing = access.writes();
        let mut transaction_over = false;
        match outcome {
            Outcome::Done => {
                if writing {
                    state.pager.release_statement()?;
                }
                state.transaction.commit_statement()?;
            }
            Outcome::Fail => {
                if writing {
                    state.pager.release_statement()?;
                }
                state.transaction.fail_statement()?;
            }
            Outcome::Abort => {
                if writing {
                    state.pager.rollback_statement()?;
                }
                state.transaction.rollback_statement()?;
            }
            Outcome::Rollback => {
                if writing {
                    state.pager.rollback_statement()?;
                }
                state.transaction.rollback_statement()?;
                if state.pager.is_writing() {
                    state.pager.rollback()?;
                }
                state.transaction.finish(false);
                self.fire_rollback_hook();
                transaction_over = true;
            }
        }
        state.active = state.active.saturating_sub(1);
        if state.active > 0 {
            return Ok(());
        }
        // The last statement has finished. An implicit transaction ends with
        // it; an explicit one is left open for its own COMMIT.
        if !transaction_over {
            if state.transaction.outlives_a_statement() {
                return Ok(());
            }
            if state.pager.is_writing() {
                if matches!(outcome, Outcome::Done | Outcome::Fail) && !self.commit_is_vetoed() {
                    let committed = state.pager.commit();
                    if committed.is_err() {
                        let _ = state.pager.rollback();
                        self.fire_rollback_hook();
                    }
                    state.transaction.finish(committed.is_ok());
                    committed?;
                } else {
                    state.pager.rollback()?;
                    state.transaction.finish(false);
                    self.fire_rollback_hook();
                }
            } else {
                state.transaction.end_read();
            }
        }
        state.pager.end_read()
    }

    /// Runs a closure with the pager, which a stepping statement does.
    pub fn with_pager<T>(&self, body: impl FnOnce(&mut Pager) -> T) -> DbResult<T> {
        let mut state = self
            .state
            .try_borrow_mut()
            .map_err(|_| error::misuse("the connection is already running a statement"))?;
        Ok(body(&mut state.pager))
    }

    /// Runs a closure inside a read transaction, for a caller with no
    /// statement of its own.
    pub fn with_reader<T>(&self, body: impl FnOnce(&mut Pager) -> DbResult<T>) -> DbResult<T> {
        self.begin_statement(Access::Read)?;
        let outcome = self.with_pager(body)?;
        self.end_statement(
            Access::Read,
            if outcome.is_ok() {
                Outcome::Done
            } else {
                Outcome::Abort
            },
        )?;
        outcome
    }

    /// Rebuilds the catalog from the pages the transaction can see.
    ///
    /// This is what a DDL statement calls once it has written its
    /// `sqlite_schema` row. It reads through the pager, so it sees the row the
    /// transaction has written and nobody else can - which is what makes a
    /// `CREATE TABLE` followed by an `INSERT` work inside one transaction.
    pub fn refresh_catalog(&self) -> DbResult<()> {
        let generation = {
            let catalog = self
                .catalog
                .try_borrow()
                .map_err(|_| error::misuse("the catalog is in use"))?;
            catalog.generation.saturating_add(1)
        };
        let name = self.options.main_name.clone();
        let loaded = {
            let mut state = self
                .state
                .try_borrow_mut()
                .map_err(|_| error::misuse("the connection is running a statement"))?;
            load_database_catalog(&mut state.pager, &name, 0)?
        };
        let mut catalog = self
            .catalog
            .try_borrow_mut()
            .map_err(|_| error::misuse("the catalog is in use"))?;
        *catalog = Arc::new(CatalogSnapshot::single(loaded, generation));
        Ok(())
    }

    /// Reloads the catalog from the file, which invalidates every prepared
    /// statement.
    ///
    /// The pager is reopened rather than reused. The header it read at open is
    /// the header of the file as it was then, and a schema another process
    /// wrote is only visible once page one is read again - so a reload that
    /// kept the cache would report the old schema and be sure of it.
    pub fn reload_catalog(&self) -> DbResult<()> {
        let generation = {
            let catalog = self
                .catalog
                .try_borrow()
                .map_err(|_| error::misuse("the catalog is in use"))?;
            catalog.generation.saturating_add(1)
        };
        let mut state = self
            .state
            .try_borrow_mut()
            .map_err(|_| error::misuse("the connection is stepping a statement"))?;
        if state.active != 0 {
            return Err(error::misuse(
                "the schema cannot be reloaded while a statement is running",
            ));
        }
        let mut pager = open_database(
            Arc::clone(&self.vfs),
            &self.path,
            DatabaseOptions {
                pager: PagerOptions::default(),
                journal: state.journal,
                writable: self.options.writable,
            },
        )?;
        let loaded = load_catalog(
            &mut pager,
            &self.options.main_name,
            generation,
            self.options.busy_timeout,
        )?;
        state.pager = pager;
        drop(state);
        let mut catalog = self
            .catalog
            .try_borrow_mut()
            .map_err(|_| error::misuse("the catalog is in use"))?;
        *catalog = Arc::new(loaded);
        Ok(())
    }

    /// Changes the journal mode, which is only legal between transactions.
    pub fn set_journal_mode(&self, mode: JournalMode) -> DbResult<JournalMode> {
        let mut state = self
            .state
            .try_borrow_mut()
            .map_err(|_| error::misuse("the connection is running a statement"))?;
        if state.pager.is_writing() {
            return Err(error::misuse(
                "cannot change the journal mode inside a transaction",
            ));
        }
        state.journal = JournalOptions {
            mode,
            synchronous: state.journal.synchronous,
        };
        let options = state.journal;
        state
            .pager
            .attach_journal(Box::new(rustdb_transaction::journal::RollbackJournal::new(
                Arc::clone(&self.vfs),
                &self.path,
                options,
            )));
        Ok(mode)
    }

    /// Changes the durability level, which takes effect at the next sync.
    pub fn set_synchronous(&self, synchronous: Synchronous) -> DbResult<Synchronous> {
        let mut state = self
            .state
            .try_borrow_mut()
            .map_err(|_| error::misuse("the connection is running a statement"))?;
        state.journal = JournalOptions {
            mode: state.journal.mode,
            synchronous,
        };
        let options = state.journal;
        if !state.pager.is_writing() {
            state.pager.attach_journal(Box::new(
                rustdb_transaction::journal::RollbackJournal::new(
                    Arc::clone(&self.vfs),
                    &self.path,
                    options,
                ),
            ));
        }
        Ok(synchronous)
    }

    /// Begins an explicit transaction.
    pub fn begin_transaction(&self, mode: BeginMode) -> DbResult<()> {
        let timeout = self.options.busy_timeout;
        let mut state = self
            .state
            .try_borrow_mut()
            .map_err(|_| error::misuse("the connection is running a statement"))?;
        state.transaction.begin(mode)?;
        if !state.pager.state().can_read() {
            begin_read_with_timeout(&mut state.pager, timeout)?;
        }
        if mode.writes_immediately() {
            if let Err(failure) = begin_write_with_timeout(&mut state.pager, timeout) {
                state.transaction.end_read();
                let _ = state.pager.end_read();
                return Err(failure);
            }
        }
        Ok(())
    }

    /// Commits an explicit transaction.
    pub fn commit_transaction(&self) -> DbResult<()> {
        let mut state = self
            .state
            .try_borrow_mut()
            .map_err(|_| error::misuse("the connection is running a statement"))?;
        if state.transaction.autocommit() {
            return Err(error::misuse("cannot commit - no transaction is active"));
        }
        if state.transaction.has_failed() {
            return Err(error::misuse(
                "cannot commit transaction - SQL statements in progress",
            ));
        }
        // The commit hook runs before anything is written, and a veto turns
        // the COMMIT into a ROLLBACK rather than an error - which is SQLite's
        // behaviour and the reason the hook is worth having at all.
        if self.commit_is_vetoed() {
            let rolled = state.pager.rollback();
            state.transaction.finish(false);
            let released = state.pager.end_read();
            self.fire_rollback_hook();
            rolled?;
            return released;
        }
        let committed = if state.pager.is_writing() {
            state.pager.commit()
        } else {
            Ok(())
        };
        if committed.is_err() {
            let _ = state.pager.rollback();
        }
        state.transaction.finish(committed.is_ok());
        let released = state.pager.end_read();
        if committed.is_err() {
            self.fire_rollback_hook();
        }
        committed?;
        released
    }

    /// Rolls back an explicit transaction.
    pub fn rollback_transaction(&self) -> DbResult<()> {
        let mut state = self
            .state
            .try_borrow_mut()
            .map_err(|_| error::misuse("the connection is running a statement"))?;
        if state.transaction.autocommit() {
            return Err(error::misuse("cannot rollback - no transaction is active"));
        }
        let rolled = state.pager.rollback();
        state.transaction.finish(false);
        let released = state.pager.end_read();
        self.fire_rollback_hook();
        rolled?;
        released
    }

    /// Opens a named savepoint.
    ///
    /// The pager's matching undo level is not opened here unless the pager is
    /// already a writer. A savepoint taken before anything has been written
    /// has nothing to undo, so opening its level at the moment the write
    /// transaction starts is the same thing - and it is the only way
    /// `SAVEPOINT` can be legal on a connection that has not written yet,
    /// which is where SQLite allows it.
    pub fn open_savepoint(&self, name: &[u8]) -> DbResult<()> {
        let timeout = self.options.busy_timeout;
        let mut state = self
            .state
            .try_borrow_mut()
            .map_err(|_| error::misuse("the connection is running a statement"))?;
        let text = String::from_utf8_lossy(name).into_owned();
        if !state.pager.state().can_read() {
            begin_read_with_timeout(&mut state.pager, timeout)?;
        }
        state.transaction.open_savepoint(&text)?;
        if state.pager.is_writing() {
            state.pager.begin_savepoint(&text)?;
        }
        Ok(())
    }

    /// Releases a savepoint, keeping its changes.
    pub fn release_savepoint(&self, name: &[u8]) -> DbResult<()> {
        let mut state = self
            .state
            .try_borrow_mut()
            .map_err(|_| error::misuse("the connection is running a statement"))?;
        let text = String::from_utf8_lossy(name).into_owned();
        let outermost = state.transaction.release_savepoint(&text)?;
        if state.pager.is_writing() {
            let _ = state.pager.release_savepoint(&text);
        }
        if !outermost {
            return Ok(());
        }
        // Releasing the savepoint that started an implicit transaction commits
        // it, which is the one place a RELEASE is a commit.
        if self.commit_is_vetoed() {
            let rolled = state.pager.rollback();
            state.transaction.finish(false);
            let released = state.pager.end_read();
            self.fire_rollback_hook();
            rolled?;
            return released;
        }
        let committed = if state.pager.is_writing() {
            state.pager.commit()
        } else {
            Ok(())
        };
        if committed.is_err() {
            let _ = state.pager.rollback();
        }
        state.transaction.finish(committed.is_ok());
        let released = state.pager.end_read();
        committed?;
        released
    }

    /// Rolls back to a savepoint, leaving it open.
    pub fn rollback_to_savepoint(&self, name: &[u8]) -> DbResult<()> {
        let mut state = self
            .state
            .try_borrow_mut()
            .map_err(|_| error::misuse("the connection is running a statement"))?;
        let text = String::from_utf8_lossy(name).into_owned();
        state.transaction.rollback_to_savepoint(&text)?;
        if state.pager.is_writing() {
            let _ = state.pager.rollback_to_savepoint(&text);
        }
        Ok(())
    }

    /// Returns how long a busy lock is retried for.
    pub fn busy_timeout(&self) -> std::time::Duration {
        self.options.busy_timeout
    }

    /// Returns the schema cookie of an attached database.
    pub fn schema_cookie(&self, database: usize) -> DbResult<u32> {
        let catalog = self.catalog()?;
        Ok(catalog
            .databases
            .get(database)
            .map_or(0, |database| database.schema_cookie))
    }
}

/// Opens a pager undo level for every savepoint taken before the write began.
///
/// A savepoint taken while the connection was only reading has no pages to
/// undo, so its level can be opened at the moment the first write starts and
/// mean exactly the same thing. Doing it here rather than at `SAVEPOINT` is
/// what keeps `SAVEPOINT` legal on a connection that never writes.
fn open_pending_savepoint_levels(state: &mut ConnectionState) -> DbResult<()> {
    let names: Vec<String> = state
        .transaction
        .levels()
        .iter()
        .filter_map(|level| level.name.clone())
        .collect();
    for name in names {
        if state.pager.savepoint_depth(&name).is_none() {
            state.pager.begin_savepoint(&name)?;
        }
    }
    Ok(())
}

/// Takes the read lock, retrying a busy file until the timeout runs out.
///
/// Only `Busy` is retried. An I/O failure or a corrupt header is returned at
/// once, because retrying either of those just delays the same answer.
fn begin_read_with_timeout(pager: &mut Pager, timeout: std::time::Duration) -> DbResult<()> {
    retry_while_busy(timeout, || pager.begin_read())
}

/// Takes the writer's reservation, retrying a busy file the same way.
fn begin_write_with_timeout(pager: &mut Pager, timeout: std::time::Duration) -> DbResult<()> {
    retry_while_busy(timeout, || pager.begin_write())
}

/// Retries an operation while it reports BUSY and the timeout has not run out.
fn retry_while_busy(
    timeout: std::time::Duration,
    mut attempt: impl FnMut() -> DbResult<()>,
) -> DbResult<()> {
    let started = std::time::Instant::now();
    loop {
        match attempt() {
            Ok(()) => return Ok(()),
            Err(failure) if failure.code() == rustdb_base::PrimaryCode::Busy => {
                if started.elapsed() >= timeout {
                    return Err(failure);
                }
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
            Err(failure) => return Err(failure),
        }
    }
}

/// Loads one database's catalog inside a read transaction.
fn load_catalog(
    pager: &mut Pager,
    name: &[u8],
    generation: u64,
    timeout: std::time::Duration,
) -> DbResult<CatalogSnapshot> {
    begin_read_with_timeout(pager, timeout)?;
    let loaded = load_database_catalog(pager, name, 0);
    let released = pager.end_read();
    let database = loaded?;
    released?;
    Ok(CatalogSnapshot::single(database, generation))
}
