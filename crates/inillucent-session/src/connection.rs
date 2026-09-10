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

use std::cell::{Cell, RefCell};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use inillucent_base::limits::Limits;
use inillucent_base::{error, DbResult};
use inillucent_catalog::load_database_catalog;
use inillucent_catalog::snapshot::CatalogSnapshot;
use inillucent_storage::pager::{Pager, PagerOptions};
use inillucent_storage::wal::{CheckpointMode, CheckpointOutcome};
use inillucent_transaction::journal::{JournalMode, JournalOptions, Synchronous};
use inillucent_transaction::recovery::{open_database, DatabaseOptions};
use inillucent_transaction::state::{
    BeginMode, ChangeCounters, Transaction, TransactionState, TransactionStats,
};
use inillucent_vfs::memory::MemoryVfs;
use inillucent_vfs::os::OsVfs;
use inillucent_vfs::path::DbPath;
use inillucent_vfs::Vfs;
use inillucent_vm::machine::{Progress, ProgressHandler};
use inillucent_vm::program::{RowChange, RowChangeKind};

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
        SessionDatabase::open_with_options(path, OpenOptions::default())
    }

    /// Opens a database file with explicit options, choosing the VFS by name.
    ///
    /// The facade above cannot name a VFS - it does not depend on that crate,
    /// and should not - so the choice is made here. `:memory:` is the one name
    /// that is not a file: SQLite gives it a private database that lives only
    /// as long as the handle, and an operating-system VFS handed that name
    /// would try to create a file called `:memory:`, which is not even a legal
    /// name on Windows.
    pub fn open_with_options(
        path: impl AsRef<std::path::Path>,
        options: OpenOptions,
    ) -> DbResult<SessionDatabase> {
        let path = path.as_ref();
        if DbPath::new(path.to_path_buf()).is_memory() {
            return SessionDatabase::open_with(path, Arc::new(MemoryVfs::new()), options);
        }
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
    /// The connection's temporary database, once something has needed one.
    ///
    /// It is created on demand rather than at connect: a connection that never
    /// writes a temporary object should not pay for a file, and most do not.
    /// Its *name* is always there, though - the catalog carries an empty
    /// `temp` so that `CREATE TEMP TABLE` has something to resolve against.
    pub temp: Option<Pager>,
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
    /// The modules, collations and policy flags this connection can reach.
    pub registry: std::sync::Arc<inillucent_ext::registry::Registry>,
    /// The collations an application defined on this connection, by name.
    ///
    /// The comparator itself lives in the process-wide table; this maps the
    /// name a statement writes to the id that reaches it, which is what keeps
    /// two connections' `MYCOLL` apart.
    pub collations: Vec<(String, inillucent_value::Collation)>,
    /// The virtual tables this connection has connected, by database and name.
    ///
    /// Shared with whatever is compiling a statement, because a module is
    /// asked for its plan at compile time and for its rows at run time,
    /// and both are questions about the same connected table.
    pub virtual_tables: std::rc::Rc<core::cell::RefCell<crate::vtab::VirtualTables>>,
    /// The run-time limits, so a module can be told what they are.
    pub limits: inillucent_base::limits::Limits,
    /// The settings the pragmas read and write.
    pub settings: crate::settings::Settings,
    /// The file the connection was opened on, for `PRAGMA database_list`.
    pub main_file: String,
    /// The schema the connection last published.
    ///
    /// The same `Arc` the connection hands to a statement, held here as well so
    /// that a module can be given it while a statement is running - the point
    /// at which the connection itself is borrowed and cannot be asked. One
    /// snapshot, two holders; the publisher updates both together.
    pub catalog: Arc<CatalogSnapshot>,
}

impl inillucent_vm::host::Host for ConnectionState {
    /// The databases the connection has open.
    fn pagers(&mut self) -> &mut dyn inillucent_storage::PagerSet {
        self
    }

    /// The schema the statement was compiled against.
    fn schema(&self) -> Option<Arc<CatalogSnapshot>> {
        Some(Arc::clone(&self.catalog))
    }

    /// The connection as a module is allowed to see it.
    fn services(&mut self) -> Box<dyn inillucent_ext::vtab::Host + '_> {
        // `main` is the database a module's shadow tables live in unless a
        // statement said otherwise, and a statement that said otherwise builds
        // its own services with the number it meant.
        Box::new(ConnectionServices {
            state: self,
            database: 0,
        })
    }

    /// Opens a cursor on one virtual table, connecting it if it is not yet.
    fn open_virtual(
        &mut self,
        reference: &inillucent_vm::program::VirtualRef,
    ) -> DbResult<Box<dyn inillucent_ext::vtab::VirtualCursor>> {
        self.ensure_connected(reference)?;
        let tables = std::rc::Rc::clone(&self.virtual_tables);
        let borrowed = tables
            .try_borrow()
            .map_err(|_| error::misuse("the virtual tables are in use"))?;
        crate::vtab::open_cursor(&borrowed, &crate::vtab::key_of(reference))
    }

    /// Runs a body with one virtual table and a context over the pagers.
    fn with_virtual(
        &mut self,
        reference: &inillucent_vm::program::VirtualRef,
        body: &mut dyn FnMut(
            &mut dyn inillucent_ext::vtab::VirtualTable,
            &mut inillucent_ext::vtab::Context<'_>,
        ) -> DbResult<inillucent_vm::host::VirtualAnswer>,
    ) -> DbResult<inillucent_vm::host::VirtualAnswer> {
        self.ensure_connected(reference)?;
        let key = crate::vtab::key_of(reference);
        let limits = self.limits.clone();
        let database = reference.database;
        let handle = std::rc::Rc::clone(&self.virtual_tables);
        // The table comes out under a short borrow that ends before the call,
        // because the module reads its shadow tables through the very pagers
        // this method is about to lend it.
        let Some(mut taken) = handle
            .try_borrow_mut()
            .map_err(|_| error::misuse("the virtual tables are in use"))?
            .take(&key)
        else {
            return Err(error::misuse("that virtual table is not connected"));
        };
        let outcome = {
            let mut services = ConnectionServices {
                state: self,
                database,
            };
            let mut context = inillucent_ext::vtab::Context {
                host: &mut services,
                database,
                limits: &limits,
                catalog: None,
            };
            body(taken.as_mut(), &mut context)
        };
        if let Ok(mut tables) = handle.try_borrow_mut() {
            tables.insert(key, taken);
        }
        outcome
    }
}

/// The connection, as a module is allowed to see it.
///
/// **A borrow rather than the connection itself.**
/// `Host::services` used to hand back `&mut ConnectionState`, and
/// `inillucent-ext` reached a pager back out of it through a `pager_set`
/// accessor - one of the two edges that made a crate the *new* engine links
/// depend on the retired storage engine. A module now reaches its rows through
/// `ShadowStore`, which this connection supplies as `PagerShadowStore`, and the
/// only thing left on this side is a pragma and a page size.
pub struct ConnectionServices<'s> {
    /// The connection these services read.
    pub state: &'s mut ConnectionState,
    /// Which attached database the modules are working in.
    pub database: usize,
}

/// The retired engine's shadow rows, reached one operation at a time.
///
/// **The pager is looked up per call rather than held**, and that is a borrow
/// rather than a preference: the pager is borrowed from this connection, and a
/// store that held it would hold this connection for as long as the module ran
/// - which is exactly what `Host::pragma` on the same object also needs. One
/// lookup is a slice index into the attached databases, and this is the engine
/// that is being retired.
impl inillucent_sql::vtab::ShadowStore for ConnectionServices<'_> {
    /// Reads one row by rowid, or nothing when there is not one.
    fn read_row(
        &mut self,
        root: u32,
        rowid: i64,
    ) -> DbResult<Option<Vec<inillucent_value::Value<'static>>>> {
        self.with_store(|store| store.read_row(root, rowid))
    }

    /// Writes one row by rowid, replacing whatever was there.
    fn write_row(
        &mut self,
        root: u32,
        rowid: i64,
        values: &[inillucent_value::Value<'static>],
    ) -> DbResult<()> {
        self.with_store(|store| store.write_row(root, rowid, values))
    }

    /// Removes one row by rowid, reporting nothing when there was not one.
    fn delete_row(&mut self, root: u32, rowid: i64) -> DbResult<()> {
        self.with_store(|store| store.delete_row(root, rowid))
    }

    /// Returns the largest rowid one shadow table holds.
    fn max_rowid(&mut self, root: u32) -> DbResult<i64> {
        self.with_store(|store| store.max_rowid(root))
    }

    /// Runs a body over every row of one shadow table, in rowid order.
    fn scan(
        &mut self,
        root: u32,
        body: &mut dyn FnMut(i64, &[inillucent_value::Value<'static>]) -> DbResult<bool>,
    ) -> DbResult<()> {
        self.with_store(|store| store.scan(root, body))
    }

    /// Reads one row of a keyed shadow table, or nothing when there is not one.
    fn read_keyed(
        &mut self,
        root: u32,
        key: &[inillucent_value::Value<'static>],
        columns: usize,
    ) -> DbResult<Option<Vec<inillucent_value::Value<'static>>>> {
        self.with_store(|store| store.read_keyed(root, key, columns))
    }

    /// Writes one row of a keyed shadow table, replacing whatever was there.
    fn write_keyed(
        &mut self,
        root: u32,
        key_columns: usize,
        values: &[inillucent_value::Value<'static>],
    ) -> DbResult<()> {
        self.with_store(|store| store.write_keyed(root, key_columns, values))
    }

    /// Removes one row of a keyed shadow table.
    fn delete_keyed(
        &mut self,
        root: u32,
        key: &[inillucent_value::Value<'static>],
    ) -> DbResult<()> {
        self.with_store(|store| store.delete_keyed(root, key))
    }

    /// Runs a body over every row of a keyed shadow table, in key order.
    fn scan_keyed(
        &mut self,
        root: u32,
        key_columns: usize,
        body: &mut dyn FnMut(&[inillucent_value::Value<'static>]) -> DbResult<bool>,
    ) -> DbResult<()> {
        self.with_store(|store| store.scan_keyed(root, key_columns, body))
    }
}

impl ConnectionServices<'_> {
    /// Runs a body against a store built over this connection's pager.
    ///
    /// @param body - what to do with the store
    fn with_store<T>(
        &mut self,
        body: impl FnOnce(&mut inillucent_vm::shadow_pager::PagerShadowStore<'_>) -> DbResult<T>,
    ) -> DbResult<T> {
        let limits = self.state.limits.clone();
        let database = self.database;
        let pager = inillucent_storage::PagerSet::pager(self.state, database)?;
        let mut store = inillucent_vm::shadow_pager::PagerShadowStore::new(pager, limits);
        body(&mut store)
    }
}

impl inillucent_ext::vtab::Host for ConnectionServices<'_> {
    /// Reports the page size the named database is on.
    ///
    /// The R-Tree sizes its nodes to fit a page, and this is the whole of what
    /// it needed a pager for.
    fn page_size(&mut self, database: usize) -> Option<usize> {
        inillucent_storage::PagerSet::pager(self.state, database)
            .ok()
            .map(|pager| pager.page_size().bytes() as usize)
    }

    /// This host *is* the store: see the `ShadowStore` implementation on it.
    fn shadow_store(&mut self) -> Option<&mut dyn inillucent_sql::vtab::ShadowStore> {
        Some(self)
    }

    /// Answers a pragma that only reads.
    ///
    /// The same register the `PRAGMA` directive reads through, called from the
    /// other side: `SELECT * FROM pragma_table_info('t')` and
    /// `PRAGMA table_info(t)` are the same question and must not be able to
    /// give two answers.
    fn pragma(
        &mut self,
        database: Option<usize>,
        name: &[u8],
        argument: Option<&inillucent_value::Value<'static>>,
    ) -> DbResult<Option<Vec<Vec<inillucent_value::Value<'static>>>>> {
        let argument = argument.map(|value| {
            inillucent_sql::directive::PragmaArgument::Name(match value {
                inillucent_value::Value::Text(text) => text.utf8_bytes().into_owned(),
                inillucent_value::Value::Integer(number) => number.to_string().into_bytes(),
                inillucent_value::Value::Real(number) => {
                    inillucent_value::numeric::real_to_text(*number)
                }
                inillucent_value::Value::Blob(blob) => blob.raw().to_vec(),
                inillucent_value::Value::Null => Vec::new(),
            })
        });
        crate::pragma::read(self.state, database, name, argument.as_ref())
    }
}

impl ConnectionState {
    /// Connects one virtual table if the connection has not connected it yet.
    ///
    /// A module is connected once per schema generation: the declaration and
    /// the shadow roots both came from the schema, so a reload throws every
    /// connected table away and the next statement rebuilds the ones it needs.
    pub fn ensure_connected(
        &mut self,
        reference: &inillucent_vm::program::VirtualRef,
    ) -> DbResult<()> {
        let key = crate::vtab::key_of(reference);
        let handle = std::rc::Rc::clone(&self.virtual_tables);
        let shadows = {
            let tables = handle
                .try_borrow()
                .map_err(|_| error::misuse("the virtual tables are in use"))?;
            if tables.is_connected(&key) {
                return Ok(());
            }
            tables.shadows(&key)
        };
        let registry = std::sync::Arc::clone(&self.registry);
        let (table, _, _) = crate::vtab::connect(&registry, reference, b"main", shadows, false)?;
        if let Ok(mut tables) = handle.try_borrow_mut() {
            tables.insert(key, table);
        }
        Ok(())
    }
}

impl inillucent_storage::PagerSet for ConnectionState {
    /// Returns the pager of one attached database.
    fn pager(&mut self, database: usize) -> DbResult<&mut Pager> {
        if database == inillucent_storage::MAIN_DATABASE {
            return Ok(&mut self.pager);
        }
        if database == inillucent_storage::TEMP_DATABASE {
            return self
                .temp
                .as_mut()
                .ok_or_else(|| error::misuse("there is no temporary database on this connection"));
        }
        self.attached
            .get_mut(database.saturating_sub(2))
            .map(|attached| &mut attached.pager)
            .ok_or_else(|| {
                error::misuse(format!(
                    "database {database} is not attached to this connection"
                ))
            })
    }

    /// `main`, `temp`, and whatever is attached.
    fn count(&self) -> usize {
        self.attached.len().saturating_add(2)
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
        body(inillucent_storage::MAIN_DATABASE, &mut self.pager)?;
        if let Some(temp) = self.temp.as_mut() {
            body(inillucent_storage::TEMP_DATABASE, temp)?;
        }
        for (position, attached) in self.attached.iter_mut().enumerate() {
            body(position.saturating_add(2), &mut attached.pager)?;
        }
        Ok(())
    }

    /// Reports whether database `index` exists on this connection.
    ///
    /// Only the temporary one can be absent, and it is absent until something
    /// needs it.
    pub fn database_exists(&self, index: usize) -> bool {
        match index {
            inillucent_storage::MAIN_DATABASE => true,
            inillucent_storage::TEMP_DATABASE => self.temp.is_some(),
            other => self.attached.len() > other.saturating_sub(2),
        }
    }

    /// Returns the number a name is attached under.
    pub fn database_index(&self, folded: &[u8], main: &[u8]) -> Option<usize> {
        if folded.eq_ignore_ascii_case(main) {
            return Some(inillucent_storage::MAIN_DATABASE);
        }
        if folded.eq_ignore_ascii_case(b"temp") {
            return Some(inillucent_storage::TEMP_DATABASE);
        }
        self.attached
            .iter()
            .position(|attached| attached.name.eq_ignore_ascii_case(folded))
            .map(|position| position.saturating_add(2))
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
    /// Asked every so many instructions whether to abandon the statement.
    pub progress: Option<Progress>,
}

impl std::fmt::Debug for Hooks {
    /// Reports which hooks are set, since a closure has nothing else to say.
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Hooks")
            .field("update", &self.update.is_some())
            .field("commit", &self.commit.is_some())
            .field("rollback", &self.rollback.is_some())
            .field("progress", &self.progress.is_some())
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
    /// The planner optimizations this connection has switched *off*.
    ///
    /// Deliberately not reachable from SQL. It is a measurement control, and
    /// SQLite puts its equivalent behind `sqlite3_test_control` for the reason
    /// that a knob on the SQL surface becomes something applications depend on
    /// - and then it is a compatibility obligation rather than an instrument.
    /// Zero, the default, is the shipped engine.
    levers: Cell<u32>,
    /// Compiled programs kept for reuse across prepares.
    ///
    /// Keyed by everything a compilation reads that the connection can change,
    /// and cleared outright when a function or a collation is registered. See
    /// `statement::PlanCache`.
    plans: RefCell<crate::statement::PlanCache>,
}

impl Connection {
    /// Returns the planner optimizations this connection has switched off.
    pub fn disabled_optimizations(&self) -> u32 {
        self.levers.get()
    }

    /// Returns a compiled program kept for this key, if there is one.
    ///
    /// A borrow failure is a miss rather than an error: the cache is an
    /// optimization and the caller can always compile.
    ///
    /// @param key - what the caller is about to compile
    pub(crate) fn cached_plan(
        &self,
        key: &crate::statement::PlanKey,
    ) -> Option<crate::statement::CompiledPlan> {
        self.plans.try_borrow_mut().ok()?.get(key)
    }

    /// Keeps a compiled program for reuse.
    ///
    /// @param key - what it was compiled against
    /// @param compiled - the program
    pub(crate) fn cache_plan(
        &self,
        key: crate::statement::PlanKey,
        compiled: crate::statement::CompiledPlan,
    ) {
        if let Ok(mut plans) = self.plans.try_borrow_mut() {
            plans.put(key, compiled);
        }
    }

    /// Drops every cached program.
    ///
    /// Called wherever something a compilation reads changes that the cache key
    /// does not carry: a registered function or a collation. A schema change
    /// does not need this, because the catalog generation is in the key.
    pub fn invalidate_plan_cache(&self) {
        if let Ok(mut plans) = self.plans.try_borrow_mut() {
            plans.clear();
        }
    }

    /// Returns how many compiled programs are held, for tests.
    pub fn cached_plan_count(&self) -> usize {
        self.plans
            .try_borrow()
            .map(|plans| plans.len())
            .unwrap_or(0)
    }

    /// Switches planner optimizations off, by mask, for A/B measurement.
    ///
    /// The mask names what to *disable*, so zero restores the shipped engine.
    /// An optimization that cannot be switched off cannot be measured: the
    /// claim that a lever made something faster is a comparison, and without
    /// an arm to compare against it is a comparison with a build that no
    /// longer exists.
    ///
    /// Programs already prepared on this connection were compiled under the
    /// previous arm and keep it, because the arm is part of what a program was
    /// compiled against. Prepare again to plan under the new one.
    /// @param mask - the levers to turn off
    pub fn disable_optimizations(&self, mask: u32) {
        self.levers.set(mask);
    }

    /// Bounds how many frames one automatic checkpoint copies.
    ///
    /// `None`, the default, copies as many as are safe - which is what the
    /// reference does and what the measurement says to keep. The bound exists
    /// because the TDD names checkpoint scheduling as a lever and a lever
    /// without an arm cannot be measured; the arm was measured and did not pay,
    /// so it is a tunable an application can reach rather than a default.
    /// @param budget - the cap, or `None` for no cap
    pub fn set_checkpoint_budget(&self, budget: Option<u32>) -> DbResult<()> {
        self.with_state(|state| state.pager.set_checkpoint_budget(budget))
    }

    /// Opens a connection, recovering a hot journal first, and loads the
    /// catalog.
    pub fn open(path: &DbPath, vfs: Arc<dyn Vfs>, options: OpenOptions) -> DbResult<Connection> {
        let mut pager = open_database(
            Arc::clone(&vfs),
            path,
            DatabaseOptions {
                pager: PagerOptions {
                    busy_timeout: options.busy_timeout,
                    ..PagerOptions::default()
                },
                journal: options.journal,
                writable: options.writable,
            },
        )?;
        let mut catalog = load_catalog(&mut pager, &options.main_name, 0, options.busy_timeout)?;
        let journal = JournalOptions {
            mode: if pager.has_wal() {
                JournalMode::Wal
            } else {
                options.journal.mode
            },
            synchronous: options.journal.synchronous,
        };
        let mut state = ConnectionState {
            pager,
            active: 0,
            temp: None,
            attached: Vec::new(),
            writing: Vec::new(),
            transaction: Transaction::new(),
            journal,
            foreign_keys: false,
            defer_foreign_keys: false,
            collations: Vec::new(),
            registry: std::sync::Arc::new({
                let mut registry = inillucent_ext::registry::Registry::with_builtins();
                crate::pragma_vtab::register_all(&mut registry);
                inillucent_search::register(&mut registry);
                registry
            }),
            virtual_tables: std::rc::Rc::new(core::cell::RefCell::new(
                crate::vtab::VirtualTables::default(),
            )),
            limits: options.limits.clone(),
            settings: crate::settings::Settings::default(),
            catalog: Arc::new(CatalogSnapshot::default()),
            main_file: path.display().to_string(),
        };
        declare_virtual_tables(&mut state, &mut catalog)?;
        let published = Arc::new(catalog);
        state.catalog = Arc::clone(&published);
        Ok(Connection {
            state: RefCell::new(state),
            catalog: RefCell::new(published),
            interrupt: Arc::new(AtomicBool::new(false)),
            limits: options.limits.clone(),
            path: path.clone(),
            vfs,
            options,
            hooks: RefCell::new(Hooks::default()),
            levers: Cell::new(0),
            plans: RefCell::new(crate::statement::PlanCache::default()),
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

    /// Installs the callback a long statement is asked to stop by.
    ///
    /// `every` is how many virtual-machine instructions pass between two
    /// calls; the callback returning `true` stops the statement with
    /// `SQLITE_INTERRUPT`. It takes effect for statements prepared after it is
    /// installed, which is the same rule SQLite follows and the same reason: a
    /// machine already running has already been handed its handler.
    pub fn set_progress_handler(&self, every: u64, handler: Option<ProgressHandler>) {
        if let Ok(mut hooks) = self.hooks.try_borrow_mut() {
            hooks.progress = handler.map(|handler| Progress { every, handler });
        }
    }

    /// Returns the progress callback a new statement should be built with.
    pub fn progress_handler(&self) -> Option<Progress> {
        self.hooks
            .try_borrow()
            .ok()
            .and_then(|hooks| hooks.progress.clone())
    }

    /// Registers a scalar function, replacing one of the same name and arity.
    ///
    /// It is `direct-only` by default, like every other function this engine
    /// knows: a schema is data, and data does not get to choose what code runs.
    /// An application that wants its function callable from a `DEFAULT` or a
    /// view says so with the flags.
    pub fn create_scalar_function(
        &self,
        name: &str,
        arity: i32,
        flags: inillucent_ext::registry::FunctionFlags,
        body: inillucent_ext::registry::ScalarBody,
    ) -> DbResult<()> {
        self.register(inillucent_ext::registry::UserFunction {
            name: name.to_string(),
            arity,
            flags,
            body: inillucent_ext::registry::UserBody::Scalar(body),
        })
    }

    /// Registers an aggregate, replacing one of the same name and arity.
    pub fn create_aggregate_function(
        &self,
        name: &str,
        arity: i32,
        flags: inillucent_ext::registry::FunctionFlags,
        body: inillucent_ext::registry::AggregateBody,
    ) -> DbResult<()> {
        self.register(inillucent_ext::registry::UserFunction {
            name: name.to_string(),
            arity,
            flags,
            body: inillucent_ext::registry::UserBody::Aggregate(body),
        })
    }

    /// Puts one function into the connection's registry.
    fn register(&self, function: inillucent_ext::registry::UserFunction) -> DbResult<()> {
        let mut state = self
            .state
            .try_borrow_mut()
            .map_err(|_| error::misuse("the connection is in use"))?;
        // The registry is shared with whatever is currently reading it, so a
        // change makes a private copy rather than mutating under a reader. A
        // registration is rare and the registry is small, which is what makes
        // copy-on-write the right shape here.
        std::sync::Arc::make_mut(&mut state.registry).register_function(function);
        drop(state);
        // A statement already compiled may have bound to a different function
        // of this name, or to none. The registered functions are not in the
        // cache key because comparing them on every prepare would cost more
        // than the cache saves, so a registration drops everything instead.
        self.invalidate_plan_cache();
        Ok(())
    }

    /// Removes a function by name and arity, reporting whether one went.
    pub fn remove_function(&self, name: &str, arity: i32) -> DbResult<bool> {
        let mut state = self
            .state
            .try_borrow_mut()
            .map_err(|_| error::misuse("the connection is in use"))?;
        let removed =
            std::sync::Arc::make_mut(&mut state.registry).unregister_function(name, arity);
        drop(state);
        self.invalidate_plan_cache();
        Ok(removed)
    }

    /// Returns every function an application registered, for the binder.
    pub fn external_functions(&self) -> Vec<inillucent_sql::function::ExternalFunction> {
        let Ok(state) = self.state.try_borrow() else {
            return Vec::new();
        };
        state
            .registry
            .functions()
            .iter()
            .map(|function| inillucent_sql::function::ExternalFunction {
                name: function.name.to_ascii_lowercase().into_bytes(),
                arity: function.arity,
                aggregate: function.is_aggregate(),
            })
            .collect()
    }

    /// Defines a collating sequence, replacing one of the same name.
    ///
    /// The comparator goes into the process-wide table; the connection keeps
    /// the name and the id it was given, so a statement that writes
    /// `COLLATE MYCOLL` on this connection reaches this comparator and one on
    /// another connection reaches its own.
    pub fn create_collation(
        &self,
        name: &str,
        comparator: inillucent_value::collation::Comparator,
    ) -> DbResult<()> {
        let collation = inillucent_value::collation::register_custom(name, comparator);
        let mut state = self
            .state
            .try_borrow_mut()
            .map_err(|_| error::misuse("the connection is in use"))?;
        let folded = name.to_ascii_uppercase();
        state.collations.retain(|(existing, _)| *existing != folded);
        state.collations.push((folded, collation));
        drop(state);
        // A comparison compiled under BINARY would keep comparing under BINARY.
        self.invalidate_plan_cache();
        Ok(())
    }

    /// Returns the bodies of the registered functions, for the machine.
    pub fn function_table(&self) -> std::sync::Arc<dyn inillucent_vm::machine::ExternalFunctions> {
        let registry = match self.state.try_borrow() {
            Ok(state) => std::sync::Arc::clone(&state.registry),
            Err(_) => std::sync::Arc::new(inillucent_ext::registry::Registry::default()),
        };
        std::sync::Arc::new(RegisteredFunctions { registry })
    }

    /// Returns the collations an application defined on this connection.
    pub fn collations(&self) -> Vec<(String, inillucent_value::Collation)> {
        match self.state.try_borrow() {
            Ok(state) => state.collations.clone(),
            Err(_) => Vec::new(),
        }
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

    /// Returns whether the connection may write.
    ///
    /// A read-only connection refuses a writing statement rather than failing
    /// part-way through one, so this is worth asking before starting it.
    pub fn is_writable(&self) -> bool {
        self.options.writable
    }

    /// Returns the journal mode and durability level in force.
    pub fn journal_options(&self) -> JournalOptions {
        self.state
            .try_borrow()
            .map_or(self.options.journal, |state| state.journal)
    }

    /// Returns what the journal has cost since the connection was opened.
    pub fn journal_stats(&self) -> inillucent_storage::JournalStats {
        self.state
            .try_borrow()
            .map_or(inillucent_storage::JournalStats::default(), |state| {
                state.pager.journal_stats()
            })
    }

    /// Returns what the pager has done since the connection was opened.
    pub fn pager_counters(&self) -> inillucent_storage::pager::PagerCounters {
        self.state.try_borrow().map_or(
            inillucent_storage::pager::PagerCounters::default(),
            |state| state.pager.counters(),
        )
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
        let pager = inillucent_storage::PagerSet::pager(&mut *state, database)?;
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
        self.begin_statement_on(access, &[inillucent_storage::MAIN_DATABASE])
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
        if access.writes() && writes.contains(&inillucent_storage::TEMP_DATABASE) {
            self.ensure_temp_database()?;
        }
        let timeout = self.options.busy_timeout;
        let mut state = self
            .state
            .try_borrow_mut()
            .map_err(|_| error::misuse("the connection is already running a statement"))?;
        if access.writes() && !self.options.writable {
            return Err(
                error::DbError::primary(inillucent_base::PrimaryCode::ReadOnly)
                    .with_message("attempt to write a readonly database"),
            );
        }
        let fresh = state.writing.is_empty();
        take_statement_locks(&mut state, access, writes, timeout)?;
        if access.writes() {
            state.transaction.promote_to_write()?;
            if fresh {
                open_pending_savepoint_levels(&mut state)?;
            }
            // Only a write transaction has undo levels; a query has nothing
            // to put back, and asking the pager for a level it cannot open
            // would fail on the first SELECT of every connection.
            let writing = state.writing.clone();
            for database in writing {
                inillucent_storage::PagerSet::pager(&mut *state, database)?.begin_statement()?;
            }
            if fresh {
                tell_virtual_tables(&mut state, crate::vtab::Moment::Begin)?;
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
                let flushed = if matches!(outcome, Outcome::Done | Outcome::Fail) {
                    tell_virtual_tables(&mut state, crate::vtab::Moment::Sync)
                } else {
                    Ok(())
                };
                if matches!(outcome, Outcome::Done | Outcome::Fail)
                    && flushed.is_ok()
                    && !self.commit_is_vetoed()
                {
                    let committed = commit_writers(&mut state, &self.vfs, &self.path);
                    if committed.is_err() {
                        let _ = rollback_writers(&mut state);
                        let _ = tell_virtual_tables(&mut state, crate::vtab::Moment::Rollback);
                        self.fire_rollback_hook();
                    } else {
                        let _ = tell_virtual_tables(&mut state, crate::vtab::Moment::Commit);
                    }
                    state.transaction.finish(committed.is_ok());
                    committed?;
                } else {
                    rollback_writers(&mut state)?;
                    let _ = tell_virtual_tables(&mut state, crate::vtab::Moment::Rollback);
                    state.transaction.finish(false);
                    self.fire_rollback_hook();
                    flushed?;
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
        use inillucent_sql::catalog_view::CatalogView;
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
            return Err(error::DbError::new(inillucent_base::ExtendedCode(787))
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
        use inillucent_sql::catalog_view::CatalogView;
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
        let published = Arc::new(loaded);
        self.state
            .try_borrow_mut()
            .map_err(|_| error::misuse("the connection is running a statement"))?
            .catalog = Arc::clone(&published);
        let mut catalog = self
            .catalog
            .try_borrow_mut()
            .map_err(|_| error::misuse("the catalog is in use"))?;
        *catalog = published;
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
            let count = inillucent_storage::PagerSet::count(&*state);
            let mut opened = Vec::new();
            for database in 0..count {
                if !state.database_exists(database) {
                    continue;
                }
                let pager = inillucent_storage::PagerSet::pager(&mut *state, database)?;
                if !pager.state().can_read() {
                    begin_read_with_timeout(pager, timeout)?;
                    opened.push(database);
                }
            }
            let read = read_every_catalog(&mut state, &name, generation);
            let mut released = Ok(());
            for database in opened {
                let outcome = inillucent_storage::PagerSet::pager(&mut *state, database)
                    .and_then(|pager| pager.end_read());
                if released.is_ok() {
                    released = outcome;
                }
            }
            let loaded = read?;
            released?;
            loaded
        };
        let published = Arc::new(loaded);
        self.state
            .try_borrow_mut()
            .map_err(|_| error::misuse("the connection is running a statement"))?
            .catalog = Arc::clone(&published);
        let mut catalog = self
            .catalog
            .try_borrow_mut()
            .map_err(|_| error::misuse("the catalog is in use"))?;
        *catalog = published;
        Ok(())
    }

    /// Returns what one setting holds.
    pub fn setting(&self, setting: crate::settings::Setting) -> DbResult<i64> {
        self.with_state(|state| state.settings.get(setting))
    }

    /// Changes what one setting holds, and applies the ones this engine acts on.
    ///
    /// The applied ones are the point: a `busy_timeout` that were only recorded
    /// would be an application waiting for a lock it had asked to wait for and
    /// not getting the wait. What is recorded and not applied is recorded as
    /// such in `settings.rs`, one row at a time.
    pub fn set_setting(&self, setting: crate::settings::Setting, value: i64) -> DbResult<i64> {
        let previous = self.with_state(|state| state.settings.set(setting, value))??;
        match setting {
            crate::settings::Setting::MaxPageCount => {
                let pages = value.clamp(1, i64::from(u32::MAX)) as u32;
                self.with_state(|state| state.pager.set_max_page_count(pages))?;
            }
            crate::settings::Setting::BusyTimeout => {
                let timeout = std::time::Duration::from_millis(value.max(0) as u64);
                self.with_state(|state| state.pager.set_busy_timeout(timeout))?;
            }
            // Every other setting is recorded and reported and changes nothing
            // here; `settings.rs` says which and why, one row at a time.
            _ => {}
        }
        Ok(previous)
    }

    /// Returns the file one attached database was opened from.
    pub fn database_file(&self, index: usize) -> String {
        if index == inillucent_storage::MAIN_DATABASE {
            return self.path.display().to_string();
        }
        if index == inillucent_storage::TEMP_DATABASE {
            return String::new();
        }
        self.with_state(|state| {
            state
                .attached
                .get(index.saturating_sub(2))
                .map(|attached| attached.path.display().to_string())
                .unwrap_or_default()
        })
        .unwrap_or_default()
    }

    /// Returns each index's declared key ordering, by root page.
    ///
    /// The integrity check verifies that an index's entries are in the order
    /// the index declares, and only the catalog knows what that order is: the
    /// storage layer sees a b-tree of records and no collation anywhere.
    pub fn index_key_map(
        &self,
    ) -> DbResult<std::collections::BTreeMap<u32, inillucent_value::record::KeyInfo>> {
        use inillucent_sql::catalog_view::CatalogView;
        let catalog = self.catalog()?;
        let mut keys = std::collections::BTreeMap::new();
        for table in catalog.every_table() {
            for index in &table.indexes {
                if index.root == 0 {
                    continue;
                }
                keys.insert(index.root, crate::execute::index_key_info(index));
            }
        }
        Ok(keys)
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
                pager: PagerOptions {
                    busy_timeout: self.options.busy_timeout,
                    ..PagerOptions::default()
                },
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
        let mut loaded = loaded;
        declare_virtual_tables(&mut state, &mut loaded)?;
        let published = Arc::new(loaded);
        state.catalog = Arc::clone(&published);
        drop(state);
        let mut catalog = self
            .catalog
            .try_borrow_mut()
            .map_err(|_| error::misuse("the catalog is in use"))?;
        *catalog = published;
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
                inillucent_transaction::journal::RollbackJournal::new(
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

    /// Creates the temporary database if this connection has not needed one.
    ///
    /// It is a file with no name that the operating system removes when the
    /// last handle closes, and it is journalled in memory: nothing in it
    /// outlives the connection, so there is nothing for a durable journal to
    /// protect. That is also why it takes no part in a commit across several
    /// databases - a file whose contents cannot survive a crash has nothing to
    /// be atomic with.
    pub fn ensure_temp_database(&self) -> DbResult<()> {
        {
            let state = self
                .state
                .try_borrow()
                .map_err(|_| error::misuse("the connection is running a statement"))?;
            if state.temp.is_some() {
                return Ok(());
            }
        }
        let path = self.vfs.temp_path("inillucent-temp")?;
        let pager = open_database(
            Arc::clone(&self.vfs),
            &path,
            DatabaseOptions {
                pager: PagerOptions {
                    busy_timeout: self.options.busy_timeout,
                    ..PagerOptions::default()
                },
                journal: JournalOptions {
                    mode: JournalMode::Memory,
                    synchronous: inillucent_transaction::journal::Synchronous::Off,
                },
                writable: true,
            },
        )?;
        {
            let mut state = self
                .state
                .try_borrow_mut()
                .map_err(|_| error::misuse("the connection is running a statement"))?;
            if state.temp.is_some() {
                return Ok(());
            }
            state.temp = Some(pager);
        }
        self.reload_schema()
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
                pager: PagerOptions {
                    busy_timeout: self.options.busy_timeout,
                    ..PagerOptions::default()
                },
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
    pub fn wal_stats(&self) -> inillucent_storage::wal::WalStats {
        self.state
            .try_borrow()
            .map_or(inillucent_storage::wal::WalStats::default(), |state| {
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
                inillucent_transaction::journal::RollbackJournal::new(
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
            let _ = tell_virtual_tables(&mut state, crate::vtab::Moment::Rollback);
            state.transaction.finish(false);
            let released = end_reads(&mut state);
            self.fire_rollback_hook();
            rolled?;
            return released;
        }
        // The modules write first, inside the same transaction, so anything a
        // virtual table has to publish at commit lands on pages this commit is
        // about to make durable. A module that cannot finish turns the COMMIT
        // into a ROLLBACK rather than committing half of what it meant to.
        let committed = if state.writing.is_empty() {
            Ok(())
        } else {
            tell_virtual_tables(&mut state, crate::vtab::Moment::Sync)
                .and_then(|()| commit_writers(&mut state, &self.vfs, &self.path))
        };
        if committed.is_err() {
            let _ = rollback_writers(&mut state);
            let _ = tell_virtual_tables(&mut state, crate::vtab::Moment::Rollback);
        } else {
            let _ = tell_virtual_tables(&mut state, crate::vtab::Moment::Commit);
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
        let _ = tell_virtual_tables(&mut state, crate::vtab::Moment::Rollback);
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
        let count = inillucent_storage::PagerSet::count(&*state);
        for database in 0..count {
            if !state.database_exists(database) {
                continue;
            }
            let pager = inillucent_storage::PagerSet::pager(&mut *state, database)?;
            if !pager.state().can_read() {
                begin_read_with_timeout(pager, timeout)?;
            }
        }
        state.transaction.open_savepoint(&text)?;
        let depth = state.transaction.depth() as i32;
        for_each_writer(&mut state, |pager| {
            if pager.is_writing() {
                pager.begin_savepoint(&text)?;
            }
            Ok(())
        })?;
        tell_virtual_tables(&mut state, crate::vtab::Moment::Savepoint(depth))
    }

    /// Releases a savepoint, keeping its changes.
    pub fn release_savepoint(&self, name: &[u8]) -> DbResult<()> {
        let mut state = self
            .state
            .try_borrow_mut()
            .map_err(|_| error::misuse("the connection is running a statement"))?;
        let text = String::from_utf8_lossy(name).into_owned();
        let depth = state.transaction.depth() as i32;
        let outermost = state.transaction.release_savepoint(&text)?;
        let _ = for_each_writer(&mut state, |pager| {
            if pager.is_writing() {
                let _ = pager.release_savepoint(&text);
            }
            Ok(())
        });
        let _ = tell_virtual_tables(&mut state, crate::vtab::Moment::Release(depth));
        if !outermost {
            return Ok(());
        }
        // Releasing the savepoint that started an implicit transaction commits
        // it, which is the one place a RELEASE is a commit.
        if self.commit_is_vetoed() {
            let rolled = rollback_writers(&mut state);
            let _ = tell_virtual_tables(&mut state, crate::vtab::Moment::Rollback);
            state.transaction.finish(false);
            let released = end_reads(&mut state);
            self.fire_rollback_hook();
            rolled?;
            return released;
        }
        let committed = if state.writing.is_empty() {
            Ok(())
        } else {
            tell_virtual_tables(&mut state, crate::vtab::Moment::Sync)
                .and_then(|()| commit_writers(&mut state, &self.vfs, &self.path))
        };
        if committed.is_err() {
            let _ = rollback_writers(&mut state);
            let _ = tell_virtual_tables(&mut state, crate::vtab::Moment::Rollback);
        } else {
            let _ = tell_virtual_tables(&mut state, crate::vtab::Moment::Commit);
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
        let depth = state.transaction.depth() as i32;
        let _ = for_each_writer(&mut state, |pager| {
            if pager.is_writing() {
                let _ = pager.rollback_to_savepoint(&text);
            }
            Ok(())
        });
        let _ = tell_virtual_tables(&mut state, crate::vtab::Moment::RollbackTo(depth));
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

/// Tells the connected virtual tables that the transaction reached a moment.
///
/// A thin wrapper so the call sites read as one line each. The limits and the
/// connected tables both live on the state, and a module is handed the state
/// itself as its host - which is the same reach it has inside an ordinary
/// statement and no more.
/// @param state - the connection
/// @param moment - what happened
fn tell_virtual_tables(state: &mut ConnectionState, moment: crate::vtab::Moment) -> DbResult<()> {
    let tables = std::rc::Rc::clone(&state.virtual_tables);
    let limits = state.limits.clone();
    let mut services = ConnectionServices { state, database: 0 };
    crate::vtab::notify(&tables, &mut services, &limits, moment)
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
    inillucent_transaction::recovery::attach_wal(
        &mut state.pager,
        vfs,
        path,
        DatabaseOptions {
            // `attach_wal` reads the journal settings and nothing else out
            // of these; the pager it is attaching to already exists and keeps
            // the options it was opened with.
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
        return Err(error::DbError::primary(inillucent_base::PrimaryCode::Busy)
            .with_message("database is locked")
            .with_detail("the log cannot be emptied while another connection is reading it"));
    }
    state.pager.close_wal()?;
    let journal =
        inillucent_transaction::journal::RollbackJournal::new(Arc::clone(vfs), path, options);
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
    let mut databases = Vec::with_capacity(state.attached.len().saturating_add(2));
    let mut strays = Vec::new();
    let (main, mut orphans) = inillucent_catalog::load::load_database_catalog_with_strays(
        &mut state.pager,
        main_name,
        0,
    )?;
    databases.push(main);
    strays.append(&mut orphans);
    match state.temp.as_mut() {
        Some(temp) => {
            let (catalog, mut orphans) =
                inillucent_catalog::load::load_database_catalog_with_strays(
                    temp,
                    b"temp",
                    inillucent_storage::TEMP_DATABASE,
                )?;
            databases.push(catalog);
            strays.append(&mut orphans);
        }
        None => databases.push(empty_temp_catalog()),
    }
    for position in 0..state.attached.len() {
        let index = position.saturating_add(2);
        let Some(attached) = state.attached.get_mut(position) else {
            continue;
        };
        let name = attached.name.clone();
        let (catalog, mut orphans) = inillucent_catalog::load::load_database_catalog_with_strays(
            &mut attached.pager,
            &name,
            index,
        )?;
        databases.push(catalog);
        strays.append(&mut orphans);
    }
    attach_strays(&mut databases, &strays)?;
    let mut snapshot = CatalogSnapshot {
        databases,
        generation,
        eponymous: Vec::new(),
    };
    declare_virtual_tables(state, &mut snapshot)?;
    Ok(snapshot)
}

/// Asks every module what its tables look like, and writes the answers down.
///
/// This is the moment SQLite calls `xConnect`. Two things come out of it: the
/// columns a virtual table declares, which go into the snapshot so that
/// everything above the session sees an ordinary table with an ordinary column
/// list; and the eponymous tables the registry provides, which belong to no
/// database and are resolved last.
///
/// A module that refuses to connect does not stop the schema from loading. The
/// table is left with no columns, exactly as an unrecognised one is, so that a
/// database naming a module this build does not have still opens and every
/// other table in it still works.
fn declare_virtual_tables(
    state: &mut ConnectionState,
    snapshot: &mut CatalogSnapshot,
) -> DbResult<()> {
    let handle = std::rc::Rc::clone(&state.virtual_tables);
    if let Ok(mut tables) = handle.try_borrow_mut() {
        tables.clear();
    }
    let registry = std::sync::Arc::clone(&state.registry);
    let mut declared: Vec<(
        crate::vtab::VirtualKey,
        Box<dyn inillucent_ext::vtab::VirtualTable>,
    )> = Vec::new();
    let mut columns: Vec<(
        usize,
        usize,
        Vec<inillucent_sql::catalog_view::ColumnInfo>,
        bool,
    )> = Vec::new();
    for (index, database) in snapshot.databases.iter().enumerate() {
        for (position, table) in database.tables.iter().enumerate() {
            let Some(module) = table.module.clone() else {
                continue;
            };
            let reference = inillucent_vm::program::VirtualRef {
                database: index,
                table: table.name.clone(),
                module,
            };
            let key = crate::vtab::key_of(&reference);
            let shadows = crate::vtab::shadow_roots(snapshot, index, &table.name);
            if let Ok(mut tables) = handle.try_borrow_mut() {
                tables.set_shadows(key.clone(), shadows.clone());
            }
            let Ok((connected, declaration, without_rowid)) =
                crate::vtab::connect(&registry, &reference, &database.name, shadows, false)
            else {
                continue;
            };
            columns.push((index, position, declaration, without_rowid));
            declared.push((key, connected));
        }
    }
    for (database, position, declaration, without_rowid) in columns {
        let Some(table) = snapshot
            .databases
            .get_mut(database)
            .and_then(|catalog| catalog.tables.get_mut(position))
        else {
            continue;
        };
        table.columns = declaration;
        table.without_rowid = without_rowid;
    }
    if let Ok(mut tables) = handle.try_borrow_mut() {
        for (key, connected) in declared {
            tables.insert(key, connected);
        }
    }
    for name in registry.module_names() {
        if let Ok(Some(table)) = crate::vtab::eponymous_table(&registry, &name) {
            snapshot.eponymous.push(table);
        }
    }
    Ok(())
}

/// Puts every trigger that fires for a table in another database on that table.
///
/// `CREATE TEMP TRIGGER ... ON t` is stored in the temporary database and
/// fires for `main.t`. The row cannot be attached while one database is being
/// read - the table is not in it - so it comes back here, where every database
/// is in hand. The search order is the one a name follows: the temporary
/// database first, then `main`, then the rest.
fn attach_strays(
    databases: &mut [inillucent_catalog::snapshot::DatabaseCatalog],
    strays: &[inillucent_storage::schema::SchemaObject],
) -> DbResult<()> {
    if strays.is_empty() {
        return Ok(());
    }
    let order: Vec<usize> = core::iter::once(inillucent_storage::TEMP_DATABASE)
        .chain(core::iter::once(inillucent_storage::MAIN_DATABASE))
        .chain(2..databases.len())
        .collect();
    for row in strays {
        let wanted = row.table_name.to_ascii_lowercase().into_bytes();
        for index in order.iter().copied() {
            let Some(catalog) = databases.get_mut(index) else {
                continue;
            };
            if !catalog.tables.iter().any(|table| table.folded == wanted) {
                continue;
            }
            inillucent_catalog::load::attach_trigger(&mut catalog.tables, row)?;
            break;
        }
    }
    Ok(())
}

/// Runs a closure over the pager of every database the transaction writes.
fn for_each_writer(
    state: &mut ConnectionState,
    mut body: impl FnMut(&mut Pager) -> DbResult<()>,
) -> DbResult<()> {
    let writing = state.writing.clone();
    for database in writing {
        body(inillucent_storage::PagerSet::pager(state, database)?)?;
    }
    Ok(())
}

/// Ends the read transaction on every database.
fn end_reads(state: &mut ConnectionState) -> DbResult<()> {
    let count = inillucent_storage::PagerSet::count(state);
    let mut outcome = Ok(());
    for database in 0..count {
        if !state.database_exists(database) {
            continue;
        }
        let released =
            inillucent_storage::PagerSet::pager(state, database).and_then(|pager| pager.end_read());
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
    let count = inillucent_storage::PagerSet::count(state);
    let mut outcome = Ok(());
    for database in 0..count {
        if !state.database_exists(database) {
            continue;
        }
        let rolled =
            inillucent_storage::PagerSet::pager(state, database).and_then(|pager| pager.rollback());
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
    // The temporary database is committed first and on its own. Nothing in it
    // survives the connection, so it has no durability to be atomic with - and
    // it journals in memory, which a super-journal could not name anyway.
    if state.writing.contains(&inillucent_storage::TEMP_DATABASE) {
        let temp = inillucent_storage::PagerSet::pager(state, inillucent_storage::TEMP_DATABASE)?;
        temp.commit()?;
        state
            .writing
            .retain(|database| *database != inillucent_storage::TEMP_DATABASE);
    }
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
        let pager = inillucent_storage::PagerSet::pager(state, database)?;
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
    let mut super_journal = inillucent_transaction::SuperJournal::create(Arc::clone(vfs), main)?;
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
    super_journal: &mut inillucent_transaction::SuperJournal,
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

/// Takes the locks a statement runs under, retrying while another connection
/// is in the way.
///
/// A refusal gives back the reads this call took before it waits, and that is
/// the whole point of the function. Holding them would be a deadlock rather
/// than a wait: the connection that has the reservation cannot finish its
/// commit until every reader has left, so a waiter that keeps reading is
/// waiting for something it is itself preventing. SQLite drops back to no lock
/// on the same path and for the same reason.
///
/// A read this call did not open is left alone. That is an explicit
/// transaction that has already read and is now trying to write, and releasing
/// its snapshot to make room would end a transaction the caller still believes
/// it is inside. That case really is `SQLITE_BUSY`, and SQLite reports it too.
fn take_statement_locks(
    state: &mut ConnectionState,
    access: Access,
    writes: &[usize],
    timeout: std::time::Duration,
) -> DbResult<()> {
    let started = std::time::Instant::now();
    loop {
        let mut attempt = Acquired::default();
        let Err(failure) = attempt_statement_locks(state, access, writes, &mut attempt) else {
            return Ok(());
        };
        if attempt.reserved.is_empty() {
            release_reads(state, &attempt.read);
        }
        if failure.code() != inillucent_base::PrimaryCode::Busy || started.elapsed() >= timeout {
            return Err(failure);
        }
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
}

/// What one attempt at the statement's locks managed to take.
#[derive(Default)]
struct Acquired {
    /// The databases this attempt opened a read on.
    read: Vec<usize>,
    /// The databases this attempt took a reservation on.
    reserved: Vec<usize>,
}

/// Makes one attempt at the locks, recording what it took.
fn attempt_statement_locks(
    state: &mut ConnectionState,
    access: Access,
    writes: &[usize],
    attempt: &mut Acquired,
) -> DbResult<()> {
    let count = inillucent_storage::PagerSet::count(&*state);
    for database in 0..count {
        if !state.database_exists(database) {
            continue;
        }
        let pager = inillucent_storage::PagerSet::pager(&mut *state, database)?;
        if !pager.state().can_read() {
            pager.begin_read()?;
            attempt.read.push(database);
        }
    }
    if !access.writes() {
        return Ok(());
    }
    for database in writes.iter().copied() {
        let pager = inillucent_storage::PagerSet::pager(&mut *state, database)?;
        let already = pager.is_writing();
        pager.begin_write()?;
        if !already {
            attempt.reserved.push(database);
        }
        if !state.writing.contains(&database) {
            state.writing.push(database);
        }
    }
    Ok(())
}

/// Gives back the reads one failed attempt took.
fn release_reads(state: &mut ConnectionState, opened: &[usize]) {
    for database in opened.iter().copied() {
        let _ =
            inillucent_storage::PagerSet::pager(state, database).and_then(|pager| pager.end_read());
    }
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
            Err(failure) if failure.code() == inillucent_base::PrimaryCode::Busy => {
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
    Ok(CatalogSnapshot {
        databases: vec![database, empty_temp_catalog()],
        generation,
        eponymous: Vec::new(),
    })
}

/// Returns the schema of a temporary database nobody has created yet.
///
/// The name is what matters: `CREATE TEMP TABLE` resolves against it, and the
/// file behind it is made at that moment rather than at connect.
fn empty_temp_catalog() -> inillucent_catalog::snapshot::DatabaseCatalog {
    inillucent_catalog::snapshot::DatabaseCatalog {
        name: b"temp".to_vec(),
        schema_cookie: 0,
        tables: inillucent_catalog::load::schema_table_aliases(inillucent_storage::TEMP_DATABASE),
    }
}

/// The registry, seen the way the machine wants to see it.
///
/// The machine knows a name and some values; the registry knows what the name
/// resolves to. This is the whole of the join, and it is here rather than in
/// the machine because the registry is connection state and the machine is not
/// allowed to reach for connection state.
struct RegisteredFunctions {
    registry: Arc<inillucent_ext::registry::Registry>,
}

impl inillucent_vm::machine::ExternalFunctions for RegisteredFunctions {
    /// Calls a scalar function on one row's arguments.
    fn call(
        &self,
        name: &[u8],
        arguments: &[inillucent_value::Value<'static>],
    ) -> DbResult<inillucent_value::Value<'static>> {
        let Some(found) = self.registry.function(name, arguments.len()) else {
            return Err(error::misuse(format!(
                "no such function: {}",
                String::from_utf8_lossy(name)
            )));
        };
        match &found.body {
            inillucent_ext::registry::UserBody::Scalar(body) => body(arguments),
            inillucent_ext::registry::UserBody::Aggregate(_) => Err(error::misuse(format!(
                "misuse of aggregate function {}",
                String::from_utf8_lossy(name)
            ))),
        }
    }

    /// Reduces a group to one value.
    fn reduce(
        &self,
        name: &[u8],
        rows: &[Vec<inillucent_value::Value<'static>>],
    ) -> DbResult<inillucent_value::Value<'static>> {
        let argc = rows.first().map_or(0, Vec::len);
        let Some(found) = self.registry.function(name, argc) else {
            return Err(error::misuse(format!(
                "no such function: {}",
                String::from_utf8_lossy(name)
            )));
        };
        match &found.body {
            inillucent_ext::registry::UserBody::Aggregate(body) => body(rows),
            inillucent_ext::registry::UserBody::Scalar(_) => Err(error::misuse(format!(
                "{} is not an aggregate",
                String::from_utf8_lossy(name)
            ))),
        }
    }
}
