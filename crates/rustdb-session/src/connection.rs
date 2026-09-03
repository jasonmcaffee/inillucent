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
use rustdb_storage::wal::{CheckpointMode, CheckpointOutcome};
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

/// One database attached to a connection by name.
///
/// `main` is not one of these: it is the file the connection was opened on, it
/// cannot be detached, and every statement that mentions no schema means it. A
/// list of "the other databases" is therefore the honest shape, and it is what
/// makes database number zero mean `main` without anybody having to maintain
/// that.
pub struct AttachedDatabase {
    /// The name it was attached under.
    pub name: Vec<u8>,
    /// The file it was opened from.
    pub path: DbPath,
    /// Its pager.
    pub pager: Pager,
}

/// How many databases one connection may attach beside `main`.
///
/// SQLite's own default, and the reason there is a limit at all is the same:
/// every statement resolves every name against every attached database, so the
/// cost of a name lookup is linear in this number.
pub const MAX_ATTACHED: usize = 10;

/// The mutable half of a connection.
pub struct ConnectionState {
    /// The pager, which owns the file and the page cache.
    pub pager: Pager,
    /// The databases `ATTACH` added, in the order it added them.
    pub attached: Vec<AttachedDatabase>,
    /// The databases this transaction has a writer on, in the order they
    /// joined it.
    ///
    /// A statement writes the databases it names and no others, so a
    /// transaction over two files holds two writers and a transaction over one
    /// holds one. Keeping the list is what makes the statement level, the
    /// savepoint, the rollback and the commit reach exactly the databases the
    /// transaction actually changed.
    pub writing: Vec<usize>,
    /// How many statements are holding the transaction open.
    pub active: usize,
    /// The transaction machine: autocommit, savepoints, and counters.
    pub transaction: Transaction,
    /// The journal mode and durability level in force.
    pub journal: JournalOptions,
    /// Whether foreign keys are enforced.
    ///
    /// Off is the default, and it is SQLite's: a constraint that has never
    /// been enforced on an existing database would start refusing writes the
    /// application has always made, so the application asks for it.
    pub foreign_keys: bool,
    /// Whether every key's checks wait for the transaction to commit.
    ///
    /// `PRAGMA defer_foreign_keys` is a property of the transaction, not of
    /// the connection: SQLite clears it at every commit and rollback, so a
    /// statement that deferred a check cannot leave the next transaction
    /// deferring them too.
    pub defer_foreign_keys: bool,
}

impl rustdb_storage::PagerSet for ConnectionState {
    /// Returns the pager of one attached database.
    fn pager(&mut self, database: usize) -> DbResult<&mut Pager> {
        if database == rustdb_storage::MAIN_DATABASE {
            return Ok(&mut self.pager);
        }
        self.attached
            .get_mut(database.saturating_sub(1))
            .map(|attached| &mut attached.pager)
            .ok_or_else(|| {
                error::misuse(format!(
                    "database {database} is not attached to this connection"
                ))
            })
    }

    /// One for `main`, plus whatever is attached.
    fn count(&self) -> usize {
        self.attached.len().saturating_add(1)
    }
}

impl ConnectionState {
    /// Runs a closure over every open pager, `main` first.
    ///
    /// The order matters where it is used: a commit writes `main` last, so a
    /// crash between two databases leaves the one that names the others still
    /// describing the old state.
    pub fn for_each_pager<T>(
        &mut self,
        mut body: impl FnMut(usize, &mut Pager) -> DbResult<T>,
    ) -> DbResult<()> {
        body(rustdb_storage::MAIN_DATABASE, &mut self.pager)?;
        for (position, attached) in self.attached.iter_mut().enumerate() {
            body(position.saturating_add(1), &mut attached.pager)?;
        }
        Ok(())
    }

    /// Returns the number a name is attached under.
    pub fn database_index(&self, folded: &[u8], main: &[u8]) -> Option<usize> {
        if folded.eq_ignore_ascii_case(main) {
            return Some(rustdb_storage::MAIN_DATABASE);
        }
        self.attached
            .iter()
            .position(|attached| attached.name.eq_ignore_ascii_case(folded))
            .map(|position| position.saturating_add(1))
    }
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
        let journal = JournalOptions {
            mode: if pager.has_wal() {
                JournalMode::Wal
            } else {
                options.journal.mode
            },
            synchronous: options.journal.synchronous,
        };
        Ok(Connection {
            state: RefCell::new(ConnectionState {
                pager,
                active: 0,
                attached: Vec::new(),
                writing: Vec::new(),
                transaction: Transaction::new(),
                journal,
                foreign_keys: false,
                defer_foreign_keys: false,
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

    /// Returns the VFS this connection's database is open through.
    ///
    /// `VACUUM` needs it: the rebuilt copy has to be created through the same
    /// VFS as the database it came from, or a test running on a simulated one
    /// would write its temporary file to the real disk.
    pub fn vfs(&self) -> std::sync::Arc<dyn Vfs> {
        std::sync::Arc::clone(&self.vfs)
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

    /// Runs a closure with the pager of one attached database.
    ///
    /// It is what every schema write goes through. `CREATE TABLE aux.t` has to
    /// write `aux`'s `sqlite_schema`, not `main`'s, and the statement is the
    /// only thing that knows which - so the number travels with it rather than
    /// being assumed.
    pub fn with_database<T>(
        &self,
        database: usize,
        body: impl FnOnce(&mut Pager) -> T,
    ) -> DbResult<T> {
        let mut state = self
            .state
            .try_borrow_mut()
            .map_err(|_| error::misuse("the connection is already running a statement"))?;
        let pager = rustdb_storage::PagerSet::pager(&mut *state, database)?;
        Ok(body(pager))
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
        self.begin_statement_on(access, &[rustdb_storage::MAIN_DATABASE])
    }

    /// Opens the level a statement runs inside, over the databases it writes.
    ///
    /// Every attached database is read: a statement that names none of them
    /// still resolves names against all of them, and a SHARED lock is what
    /// makes the schema it resolved against still true when it runs. Only the
    /// databases the statement actually writes get a writer, because a
    /// RESERVED lock on a file nobody is changing is a lock somebody else is
    /// waiting for.
    pub fn begin_statement_on(&self, access: Access, writes: &[usize]) -> DbResult<()> {
        let timeout = self.options.busy_timeout;
        let mut state = self
            .state
            .try_borrow_mut()
            .map_err(|_| error::misuse("the connection is already running a statement"))?;
        if access.writes() && !self.options.writable {
            return Err(error::DbError::primary(rustdb_base::PrimaryCode::ReadOnly)
                .with_message("attempt to write a readonly database"));
        }
        let count = rustdb_storage::PagerSet::count(&*state);
        for database in 0..count {
            let pager = rustdb_storage::PagerSet::pager(&mut *state, database)?;
            if !pager.state().can_read() {
                begin_read_with_timeout(pager, timeout)?;
            }
        }
        if access.writes() {
            let fresh = state.writing.is_empty();
            for database in writes.iter().copied() {
                let pager = rustdb_storage::PagerSet::pager(&mut *state, database)?;
                begin_write_with_timeout(pager, timeout)?;
                if !state.writing.contains(&database) {
                    state.writing.push(database);
                }
            }
            state.transaction.promote_to_write()?;
            if fresh {
                open_pending_savepoint_levels(&mut state)?;
            }
            // Only a write transaction has undo levels; a query has nothing
            // to put back, and asking the pager for a level it cannot open
            // would fail on the first SELECT of every connection.
            let writing = state.writing.clone();
            for database in writing {
                rustdb_storage::PagerSet::pager(&mut *state, database)?.begin_statement()?;
            }
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
        // A deferred foreign key is checked where the transaction ends, and the
        // check is a query - so it has to run before the connection's state is
        // borrowed for the commit. A violation turns the implicit commit into a
        // rollback and is reported once everything is closed, which is what
        // SQLite does and is why the error is carried rather than returned
        // here: returning it now would leave the statement level open.
        let mut outcome = outcome;
        let mut deferred = None;
        if self.write_statement_is_finishing(access, outcome)? {
            if let Err(error) = self.settle_foreign_keys() {
                deferred = Some(error);
                outcome = Outcome::Rollback;
            }
        }
        if deferred.is_none() && self.implicit_transaction_is_ending(access, outcome)? {
            if let Err(error) = self.check_deferred_foreign_keys() {
                deferred = Some(error);
                outcome = Outcome::Rollback;
            }
        }
        let mut state = self
            .state
            .try_borrow_mut()
            .map_err(|_| error::misuse("the connection is already running a statement"))?;
        let writing = access.writes();
        let mut transaction_over = false;
        match outcome {
            Outcome::Done => {
                if writing {
                    for_each_writer(&mut state, |pager| pager.release_statement())?;
                }
                state.transaction.commit_statement()?;
            }
            Outcome::Fail => {
                if writing {
                    for_each_writer(&mut state, |pager| pager.release_statement())?;
                }
                state.transaction.fail_statement()?;
            }
            Outcome::Abort => {
                if writing {
                    for_each_writer(&mut state, |pager| pager.rollback_statement())?;
                }
                state.transaction.rollback_statement()?;
            }
            Outcome::Rollback => {
                if writing {
                    for_each_writer(&mut state, |pager| pager.rollback_statement())?;
                }
                state.transaction.rollback_statement()?;
                rollback_writers(&mut state)?;
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
            if !state.writing.is_empty() {
                if matches!(outcome, Outcome::Done | Outcome::Fail) && !self.commit_is_vetoed() {
                    let committed = commit_writers(&mut state, &self.vfs, &self.path);
                    if committed.is_err() {
                        let _ = rollback_writers(&mut state);
                        self.fire_rollback_hook();
                    }
                    state.transaction.finish(committed.is_ok());
                    committed?;
                } else {
                    rollback_writers(&mut state)?;
                    state.transaction.finish(false);
                    self.fire_rollback_hook();
                }
            } else {
                state.transaction.end_read();
            }
        }
        let released = end_reads(&mut state);
        drop(state);
        if let Some(error) = deferred {
            return Err(error);
        }
        released
    }

    /// Applies the actions of every key that can lead back to its own table.
    ///
    /// It runs after the statement rather than inside it: a key whose action
    /// can fire itself cannot be inlined to a depth the data decides, so the
    /// first level happens in the statement and the rest happens here, before
    /// anything else can look.
    ///
    /// It runs after every write on a schema that has such a key, which is one
    /// query per key per write. The alternative was to ask first whether the
    /// statement changed anything, and the counter that would answer is only
    /// published when the statement ends - which is after this. The cost is
    /// paid only by a schema whose keys form a cycle.
    ///
    /// One divergence is worth naming: a row that was already orphaned before
    /// enforcement was turned on is repaired by the next write rather than
    /// left alone. It can only exist in a database that already violates the
    /// constraint, and `PRAGMA foreign_key_check` is what finds those.
    fn settle_foreign_keys(&self) -> DbResult<()> {
        if !self.foreign_keys() || !self.has_cyclic_foreign_keys()? {
            return Ok(());
        }
        crate::execute::sweep_cyclic_foreign_keys(self)
    }

    /// Reports whether any key can lead back to the table that declares it.
    fn has_cyclic_foreign_keys(&self) -> DbResult<bool> {
        use rustdb_sql::catalog_view::CatalogView;
        let catalog = self.catalog()?;
        Ok(catalog
            .tables_of(0)
            .iter()
            .any(|table| table.foreign_keys.iter().any(|key| key.cyclic)))
    }

    /// Reports whether a write statement is finishing at the outermost level.
    fn write_statement_is_finishing(&self, access: Access, outcome: Outcome) -> DbResult<bool> {
        if !access.writes() || !matches!(outcome, Outcome::Done | Outcome::Fail) {
            return Ok(false);
        }
        let state = self
            .state
            .try_borrow()
            .map_err(|_| error::misuse("the connection is already running a statement"))?;
        Ok(state.active == 1 && state.pager.is_writing())
    }

    /// Reports whether this statement's own transaction is about to commit.
    ///
    /// It is asked before the state is borrowed, because what happens next is
    /// a query. The three facts are: this is the outermost statement, the
    /// transaction is the statement's own rather than an explicit one, and it
    /// wrote something.
    fn implicit_transaction_is_ending(&self, access: Access, outcome: Outcome) -> DbResult<bool> {
        if !access.writes() || !matches!(outcome, Outcome::Done | Outcome::Fail) {
            return Ok(false);
        }
        let state = self
            .state
            .try_borrow()
            .map_err(|_| error::misuse("the connection is already running a statement"))?;
        Ok(state.active == 1
            && !state.transaction.outlives_a_statement()
            && state.pager.is_writing())
    }

    /// Checks every deferred foreign key, and reports the first violation.
    ///
    /// The check is a full one rather than a running count. SQLite keeps a
    /// counter of outstanding violations and moves it as rows appear and
    /// disappear; a counter that drifts by one reports a violation that is not
    /// there, or misses one that is, and neither is visible until a commit
    /// fails for a reason nobody can reproduce. Asking the question directly
    /// costs a query per deferred key per commit and cannot drift.
    pub fn check_deferred_foreign_keys(&self) -> DbResult<()> {
        if !self.foreign_keys() || !self.has_deferred_foreign_keys()? {
            return Ok(());
        }
        for query in crate::execute::violation_queries(self, None)? {
            if crate::execute::internal_query(self, &query.sql)?.is_empty() {
                continue;
            }
            return Err(error::DbError::new(rustdb_base::ExtendedCode(787))
                .with_message("FOREIGN KEY constraint failed")
                .with_detail(format!(
                    "deferred key {} of {}",
                    query.key,
                    String::from_utf8_lossy(&query.child)
                )));
        }
        Ok(())
    }

    /// Reports whether any key's checks are waiting for the commit.
    fn has_deferred_foreign_keys(&self) -> DbResult<bool> {
        if self.defer_foreign_keys() {
            return Ok(true);
        }
        use rustdb_sql::catalog_view::CatalogView;
        let catalog = self.catalog()?;
        Ok(catalog
            .tables_of(0)
            .iter()
            .any(|table| table.foreign_keys.iter().any(|key| key.is_deferred())))
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
            read_every_catalog(&mut state, &name, generation)?
        };
        let mut catalog = self
            .catalog
            .try_borrow_mut()
            .map_err(|_| error::misuse("the catalog is in use"))?;
        *catalog = Arc::new(loaded);
        Ok(())
    }

    /// Rebuilds the catalog with a read transaction of its own.
    ///
    /// `ATTACH` and `DETACH` are the callers: neither is allowed inside a
    /// transaction, so there is no open read to reuse and the reads are taken
    /// and released here.
    pub fn reload_schema(&self) -> DbResult<()> {
        let generation = {
            let catalog = self
                .catalog
                .try_borrow()
                .map_err(|_| error::misuse("the catalog is in use"))?;
            catalog.generation.saturating_add(1)
        };
        let name = self.options.main_name.clone();
        let timeout = self.options.busy_timeout;
        let loaded = {
            let mut state = self
                .state
                .try_borrow_mut()
                .map_err(|_| error::misuse("the connection is running a statement"))?;
            let count = rustdb_storage::PagerSet::count(&*state);
            for database in 0..count {
                let pager = rustdb_storage::PagerSet::pager(&mut *state, database)?;
                if !pager.state().can_read() {
                    begin_read_with_timeout(pager, timeout)?;
                }
            }
            let read = read_every_catalog(&mut state, &name, generation);
            let released = end_reads(&mut state);
            let loaded = read?;
            released?;
            loaded
        };
        let mut catalog = self
            .catalog
            .try_borrow_mut()
            .map_err(|_| error::misuse("the catalog is in use"))?;
        *catalog = Arc::new(loaded);
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
        let current = state.journal.mode;
        if mode == current {
            return Ok(mode);
        }
        let options = JournalOptions {
            mode,
            synchronous: state.journal.synchronous,
        };
        if current.is_wal() {
            leave_wal_mode(&mut state, &self.vfs, &self.path, options)?;
        } else if mode.is_wal() {
            enter_wal_mode(&mut state, &self.vfs, &self.path, options)?;
        } else {
            state.pager.attach_journal(Box::new(
                rustdb_transaction::journal::RollbackJournal::new(
                    Arc::clone(&self.vfs),
                    &self.path,
                    options,
                ),
            ));
        }
        state.journal = options;
        Ok(mode)
    }

    /// Copies the log's frames into the database file.
    ///
    /// This is `PRAGMA wal_checkpoint`. It reports what it managed rather than
    /// insisting: a checkpoint that ran into a reader has still done useful
    /// work, and telling the caller how much is the difference between a
    /// diagnostic and a coin toss.
    pub fn checkpoint(&self, mode: CheckpointMode) -> DbResult<CheckpointOutcome> {
        let mut state = self
            .state
            .try_borrow_mut()
            .map_err(|_| error::misuse("the connection is running a statement"))?;
        if !state.pager.has_wal() {
            return Err(error::misuse(
                "a checkpoint was asked for on a database that is not in WAL mode",
            ));
        }
        if state.pager.is_writing() {
            return Err(error::misuse("cannot checkpoint inside a transaction"));
        }
        if state.pager.state().can_read() {
            state.pager.end_read()?;
        }
        state.pager.checkpoint(mode)
    }

    /// Opens a database file and attaches it under a name.
    ///
    /// It is refused inside a transaction, which is SQLite's rule and a
    /// necessary one: the statements already bound in that transaction resolved
    /// their names against a schema that did not have this database in it, and
    /// the numbers they carry would move underneath them.
    pub fn attach(&self, file: &[u8], name: &[u8]) -> DbResult<()> {
        let folded = name.to_ascii_lowercase();
        {
            let state = self
                .state
                .try_borrow()
                .map_err(|_| error::misuse("the connection is running a statement"))?;
            if folded == self.options.main_name.to_ascii_lowercase() || folded == b"temp" {
                return Err(error::misuse(format!(
                    "database {} is already in use",
                    String::from_utf8_lossy(name)
                )));
            }
            if state
                .attached
                .iter()
                .any(|attached| attached.name.eq_ignore_ascii_case(&folded))
            {
                return Err(error::misuse(format!(
                    "database {} is already in use",
                    String::from_utf8_lossy(name)
                )));
            }
            if state.attached.len() >= MAX_ATTACHED {
                return Err(error::misuse(format!(
                    "too many attached databases - max {MAX_ATTACHED}"
                )));
            }
        }
        let path = DbPath::new(std::path::PathBuf::from(
            String::from_utf8_lossy(file).into_owned(),
        ));
        let options = self.options.journal;
        let pager = open_database(
            Arc::clone(&self.vfs),
            &path,
            DatabaseOptions {
                pager: PagerOptions::default(),
                journal: options,
                writable: self.options.writable,
            },
        )?;
        {
            let mut state = self
                .state
                .try_borrow_mut()
                .map_err(|_| error::misuse("the connection is running a statement"))?;
            // A connection reads one encoding. A file that disagrees would have
            // every text value in it read as the wrong bytes, so it is refused
            // rather than silently misread.
            if pager.text_encoding() != state.pager.text_encoding() && pager.page_count() > 1 {
                return Err(error::misuse(
                    "attached databases must use the same text encoding as main database",
                ));
            }
            state.attached.push(AttachedDatabase {
                name: name.to_vec(),
                path,
                pager,
            });
        }
        self.reload_schema()
    }

    /// Closes an attached database and forgets its name.
    pub fn detach(&self, name: &[u8]) -> DbResult<()> {
        {
            let mut state = self
                .state
                .try_borrow_mut()
                .map_err(|_| error::misuse("the connection is running a statement"))?;
            if state.transaction.state() != TransactionState::Autocommit {
                return Err(error::misuse("cannot DETACH database within transaction"));
            }
            let Some(position) = state
                .attached
                .iter()
                .position(|attached| attached.name.eq_ignore_ascii_case(name))
            else {
                if name.eq_ignore_ascii_case(&self.options.main_name) {
                    return Err(error::misuse("cannot detach database main"));
                }
                return Err(error::misuse(format!(
                    "no such database: {}",
                    String::from_utf8_lossy(name)
                )));
            };
            let mut detached = state.attached.remove(position);
            detached.pager.close()?;
        }
        self.reload_schema()
    }

    /// Reports whether foreign keys are enforced.
    pub fn foreign_keys(&self) -> bool {
        self.state
            .try_borrow()
            .is_ok_and(|state| state.foreign_keys)
    }

    /// Turns foreign-key enforcement on or off.
    ///
    /// SQLite refuses the change inside a transaction rather than applying it
    /// half way through one, and so does this: a statement already bound
    /// carries the constraints that were in force when it was bound.
    pub fn set_foreign_keys(&self, enforced: bool) -> DbResult<bool> {
        let mut state = self
            .state
            .try_borrow_mut()
            .map_err(|_| error::misuse("the connection is running a statement"))?;
        if state.transaction.state() != TransactionState::Autocommit {
            return Ok(state.foreign_keys);
        }
        state.foreign_keys = enforced;
        Ok(enforced)
    }

    /// Reports whether every key's checks wait for the commit.
    pub fn defer_foreign_keys(&self) -> bool {
        self.state
            .try_borrow()
            .is_ok_and(|state| state.defer_foreign_keys)
    }

    /// Defers every key's checks until the transaction commits.
    pub fn set_defer_foreign_keys(&self, deferred: bool) -> DbResult<bool> {
        let mut state = self
            .state
            .try_borrow_mut()
            .map_err(|_| error::misuse("the connection is running a statement"))?;
        state.defer_foreign_keys = deferred;
        Ok(deferred)
    }

    /// Reports whether the connection's database is in WAL mode.
    pub fn is_wal(&self) -> bool {
        self.state
            .try_borrow()
            .is_ok_and(|state| state.pager.has_wal())
    }

    /// Returns how many frames the log may reach before a commit
    /// checkpoints it, or zero when it never does.
    pub fn wal_auto_checkpoint(&self) -> u32 {
        self.state
            .try_borrow()
            .map_or(0, |state| state.pager.wal_auto_checkpoint())
    }

    /// Sets how many frames the log may reach before a commit checkpoints it.
    pub fn set_wal_auto_checkpoint(&self, frames: u32) -> DbResult<()> {
        let mut state = self
            .state
            .try_borrow_mut()
            .map_err(|_| error::misuse("the connection is running a statement"))?;
        state.pager.set_wal_auto_checkpoint(frames);
        Ok(())
    }

    /// Returns what the write-ahead log has cost since the connection opened.
    pub fn wal_stats(&self) -> rustdb_storage::wal::WalStats {
        self.state
            .try_borrow()
            .map_or(rustdb_storage::wal::WalStats::default(), |state| {
                state.pager.wal_stats()
            })
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
        self.check_deferred_foreign_keys()?;
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
            let rolled = rollback_writers(&mut state);
            state.transaction.finish(false);
            let released = end_reads(&mut state);
            self.fire_rollback_hook();
            rolled?;
            return released;
        }
        let committed = if state.writing.is_empty() {
            Ok(())
        } else {
            commit_writers(&mut state, &self.vfs, &self.path)
        };
        if committed.is_err() {
            let _ = rollback_writers(&mut state);
        }
        state.transaction.finish(committed.is_ok());
        state.defer_foreign_keys = false;
        let released = end_reads(&mut state);
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
        let rolled = rollback_writers(&mut state);
        state.transaction.finish(false);
        state.defer_foreign_keys = false;
        let released = end_reads(&mut state);
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
        let count = rustdb_storage::PagerSet::count(&*state);
        for database in 0..count {
            let pager = rustdb_storage::PagerSet::pager(&mut *state, database)?;
            if !pager.state().can_read() {
                begin_read_with_timeout(pager, timeout)?;
            }
        }
        state.transaction.open_savepoint(&text)?;
        for_each_writer(&mut state, |pager| {
            if pager.is_writing() {
                pager.begin_savepoint(&text)?;
            }
            Ok(())
        })
    }

    /// Releases a savepoint, keeping its changes.
    pub fn release_savepoint(&self, name: &[u8]) -> DbResult<()> {
        let mut state = self
            .state
            .try_borrow_mut()
            .map_err(|_| error::misuse("the connection is running a statement"))?;
        let text = String::from_utf8_lossy(name).into_owned();
        let outermost = state.transaction.release_savepoint(&text)?;
        let _ = for_each_writer(&mut state, |pager| {
            if pager.is_writing() {
                let _ = pager.release_savepoint(&text);
            }
            Ok(())
        });
        if !outermost {
            return Ok(());
        }
        // Releasing the savepoint that started an implicit transaction commits
        // it, which is the one place a RELEASE is a commit.
        if self.commit_is_vetoed() {
            let rolled = rollback_writers(&mut state);
            state.transaction.finish(false);
            let released = end_reads(&mut state);
            self.fire_rollback_hook();
            rolled?;
            return released;
        }
        let committed = if state.writing.is_empty() {
            Ok(())
        } else {
            commit_writers(&mut state, &self.vfs, &self.path)
        };
        if committed.is_err() {
            let _ = rollback_writers(&mut state);
        }
        state.transaction.finish(committed.is_ok());
        let released = end_reads(&mut state);
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
        let _ = for_each_writer(&mut state, |pager| {
            if pager.is_writing() {
                let _ = pager.rollback_to_savepoint(&text);
            }
            Ok(())
        });
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

/// Turns WAL mode on, stamping the file format versions that say so.
///
/// The stamp is an ordinary rollback-mode transaction, and it has to be: the
/// two bytes are what tell every other connection - including one that opens
/// the file next week - to look for a log. Writing them through the log they
/// are announcing would be a file that only says it is in WAL mode to somebody
/// who already knew.
fn enter_wal_mode(
    state: &mut ConnectionState,
    vfs: &Arc<dyn Vfs>,
    path: &DbPath,
    options: JournalOptions,
) -> DbResult<()> {
    stamp_format_versions(state, 2)?;
    rustdb_transaction::recovery::attach_wal(
        &mut state.pager,
        vfs,
        path,
        DatabaseOptions {
            pager: PagerOptions::default(),
            journal: options,
            writable: true,
        },
    )
}

/// Turns WAL mode off, moving every frame into the database file first.
///
/// The checkpoint has to finish. A database whose format versions say rollback
/// while frames are still only in the log is one that reads as though those
/// transactions never happened, so the mode change is refused rather than half
/// made when another connection is still holding the log open.
fn leave_wal_mode(
    state: &mut ConnectionState,
    vfs: &Arc<dyn Vfs>,
    path: &DbPath,
    options: JournalOptions,
) -> DbResult<()> {
    if state.pager.state().can_read() {
        state.pager.end_read()?;
    }
    let outcome = state.pager.checkpoint(CheckpointMode::Truncate)?;
    if !outcome.truncated {
        return Err(error::DbError::primary(rustdb_base::PrimaryCode::Busy)
            .with_message("database is locked")
            .with_detail("the log cannot be emptied while another connection is reading it"));
    }
    state.pager.close_wal()?;
    let journal = rustdb_transaction::journal::RollbackJournal::new(Arc::clone(vfs), path, options);
    state.pager.attach_journal(Box::new(journal));
    stamp_format_versions(state, 1)
}

/// Writes the read and write format versions and commits.
fn stamp_format_versions(state: &mut ConnectionState, version: u8) -> DbResult<()> {
    if !state.pager.state().can_read() {
        state.pager.begin_read()?;
    }
    state.pager.begin_write()?;
    let mut header = *state.pager.header();
    header.write_version = version;
    header.read_version = version;
    let stamped = state.pager.set_header(header);
    if stamped.is_err() {
        let _ = state.pager.rollback();
        return stamped;
    }
    let committed = state.pager.commit();
    if committed.is_err() {
        let _ = state.pager.rollback();
    }
    committed?;
    state.pager.end_read()
}

/// Reads the schema of every attached database into one snapshot.
///
/// The order is the connection's, and it has to be: a bound statement carries
/// database *numbers*, and they mean what this list says they mean. `main` is
/// zero and everything else follows in attachment order.
///
/// Every pager must already be in a read transaction. The callers differ on
/// where that came from - a statement's own, or one taken for the reload - and
/// neither wants the other's.
fn read_every_catalog(
    state: &mut ConnectionState,
    main_name: &[u8],
    generation: u64,
) -> DbResult<CatalogSnapshot> {
    let mut databases = Vec::with_capacity(state.attached.len().saturating_add(1));
    databases.push(load_database_catalog(&mut state.pager, main_name, 0)?);
    for position in 0..state.attached.len() {
        let index = position.saturating_add(1);
        let Some(attached) = state.attached.get_mut(position) else {
            continue;
        };
        let name = attached.name.clone();
        databases.push(load_database_catalog(&mut attached.pager, &name, index)?);
    }
    Ok(CatalogSnapshot {
        databases,
        generation,
    })
}

/// Runs a closure over the pager of every database the transaction writes.
fn for_each_writer(
    state: &mut ConnectionState,
    mut body: impl FnMut(&mut Pager) -> DbResult<()>,
) -> DbResult<()> {
    let writing = state.writing.clone();
    for database in writing {
        body(rustdb_storage::PagerSet::pager(state, database)?)?;
    }
    Ok(())
}

/// Ends the read transaction on every database.
fn end_reads(state: &mut ConnectionState) -> DbResult<()> {
    let count = rustdb_storage::PagerSet::count(state);
    let mut outcome = Ok(());
    for database in 0..count {
        let released =
            rustdb_storage::PagerSet::pager(state, database).and_then(|pager| pager.end_read());
        if outcome.is_ok() {
            outcome = released;
        }
    }
    outcome
}

/// Undoes the transaction on every database it reached.
fn rollback_writers(state: &mut ConnectionState) -> DbResult<()> {
    // Every database, not only the ones the transaction recorded a writer on:
    // a write that failed before it was recorded still has undo images, and a
    // pager with nothing to undo returns at once.
    let count = rustdb_storage::PagerSet::count(state);
    let mut outcome = Ok(());
    for database in 0..count {
        let rolled =
            rustdb_storage::PagerSet::pager(state, database).and_then(|pager| pager.rollback());
        if outcome.is_ok() {
            outcome = rolled;
        }
    }
    state.writing.clear();
    outcome
}

/// Commits every database the transaction wrote, as one event.
///
/// One database commits the way it always has: the step that makes its journal
/// non-hot is the commit point, and there is nothing else to coordinate with.
///
/// Several commit through a super-journal. Each journal is written with that
/// file's name in it and made durable, each database is written and made
/// durable, and then the super-journal is deleted - and that deletion is the
/// commit. A crash before it finds journals naming a file that is still there
/// and undoes every one of them; a crash after it finds journals naming a file
/// that is gone and undoes none of them. There is no third outcome, because a
/// deletion is one operation and there is nothing to observe inside it.
fn commit_writers(state: &mut ConnectionState, vfs: &Arc<dyn Vfs>, main: &DbPath) -> DbResult<()> {
    if state.writing.len() <= 1 {
        let outcome = for_each_writer(state, |pager| pager.commit());
        state.writing.clear();
        return outcome;
    }
    commit_across_databases(state, vfs, main)
}

/// The multi-database commit protocol.
fn commit_across_databases(
    state: &mut ConnectionState,
    vfs: &Arc<dyn Vfs>,
    main: &DbPath,
) -> DbResult<()> {
    let mut journals = Vec::new();
    for database in state.writing.clone() {
        let pager = rustdb_storage::PagerSet::pager(state, database)?;
        let Some(path) = pager.journal_path() else {
            // A journal that has no file cannot be named by a super-journal,
            // so a transaction that spans databases cannot be made atomic in
            // this mode. Saying so is better than committing them one at a
            // time and calling it atomic.
            return Err(error::misuse(
                "a transaction over several databases needs a journal mode that writes a file",
            ));
        };
        journals.push(path);
    }
    let mut super_journal = rustdb_transaction::SuperJournal::create(Arc::clone(vfs), main)?;
    let prepared = write_super_journal(&mut super_journal, &journals);
    if prepared.is_err() {
        super_journal.abandon();
        return prepared;
    }
    let name = super_journal.path().clone();
    let phase_one = commit_phase_one(state, &name);
    if phase_one.is_err() {
        super_journal.abandon();
        return phase_one;
    }
    // The commit point. Every database is durable and every journal is hot;
    // this makes all of them non-hot at once.
    super_journal.commit()?;
    let outcome = for_each_writer(state, |pager| pager.commit_phase_two());
    state.writing.clear();
    outcome
}

/// Lists the journals in the super-journal and makes the list durable.
fn write_super_journal(
    super_journal: &mut rustdb_transaction::SuperJournal,
    journals: &[DbPath],
) -> DbResult<()> {
    for journal in journals {
        super_journal.add(journal)?;
    }
    super_journal.sync()
}

/// Runs phase one on every database, naming the super-journal first.
fn commit_phase_one(state: &mut ConnectionState, name: &DbPath) -> DbResult<()> {
    for_each_writer(state, |pager| {
        pager.set_super_journal(Some(name.clone()));
        pager.commit_phase_one().map(|_| ())
    })
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
    for_each_writer(state, |pager| {
        for name in &names {
            if pager.savepoint_depth(name).is_none() {
                pager.begin_savepoint(name)?;
            }
        }
        Ok(())
    })
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
