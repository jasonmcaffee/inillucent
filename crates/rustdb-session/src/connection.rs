//! The connection: its pager, its catalog snapshot, and its read transaction.
//!
//! Invariant: the read transaction is reference-counted by the statements that
//! need it. The first statement to step takes the SHARED lock and the last to
//! finish releases it, so two statements stepped alternately read one snapshot
//! of the file and a connection with nothing running holds no lock at all.
//!
//! The connection also owns the catalog. Reloading it is explicit, because a
//! reload invalidates every prepared statement and doing that implicitly - on a
//! timer, or on every step - would make a statement's behaviour depend on when
//! it was stepped rather than on what it says.

use std::cell::RefCell;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use rustdb_base::limits::Limits;
use rustdb_base::{error, DbResult};
use rustdb_catalog::load_database_catalog;
use rustdb_catalog::snapshot::CatalogSnapshot;
use rustdb_storage::pager::{Pager, PagerOptions};
use rustdb_vfs::os::OsVfs;
use rustdb_vfs::path::DbPath;
use rustdb_vfs::Vfs;

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
}

impl Default for OpenOptions {
    /// Returns the defaults SQLite opens with.
    fn default() -> OpenOptions {
        OpenOptions {
            limits: Limits::default(),
            main_name: b"main".to_vec(),
            busy_timeout: std::time::Duration::ZERO,
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
    /// Opens a database file read-only.
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
    /// How many statements are holding the read transaction open.
    pub readers: usize,
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
}

impl Connection {
    /// Opens a connection and loads the catalog.
    pub fn open(path: &DbPath, vfs: Arc<dyn Vfs>, options: OpenOptions) -> DbResult<Connection> {
        let mut pager = Pager::open_read_only(vfs.as_ref(), path, PagerOptions::default())?;
        let catalog = load_catalog(&mut pager, &options.main_name, 0, options.busy_timeout)?;
        Ok(Connection {
            state: RefCell::new(ConnectionState { pager, readers: 0 }),
            catalog: RefCell::new(Arc::new(catalog)),
            interrupt: Arc::new(AtomicBool::new(false)),
            limits: options.limits.clone(),
            path: path.clone(),
            vfs,
            options,
        })
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
    ///
    /// The read-only engine has no explicit transactions yet, so it always is;
    /// the accessor exists because the oracle protocol reports the flag and a
    /// differential run compares it.
    pub fn autocommit(&self) -> bool {
        true
    }

    /// Runs a closure with the pager, taking the read transaction if this is
    /// the first statement to need it.
    pub fn with_reader<T>(&self, body: impl FnOnce(&mut Pager) -> DbResult<T>) -> DbResult<T> {
        let mut state = self
            .state
            .try_borrow_mut()
            .map_err(|_| error::misuse("the connection is already stepping a statement"))?;
        if state.readers == 0 {
            begin_read_with_timeout(&mut state.pager, self.options.busy_timeout)?;
        }
        state.readers = state.readers.saturating_add(1);
        let outcome = body(&mut state.pager);
        state.readers = state.readers.saturating_sub(1);
        if state.readers == 0 {
            // A failure to release the lock must not mask the statement's own
            // failure, so it is only reported when the statement succeeded.
            let released = state.pager.end_read();
            if outcome.is_ok() {
                released?;
            }
        }
        outcome
    }

    /// Reloads the catalog, which invalidates every prepared statement.
    ///
    /// The pager is reopened rather than reused, because the header it read at
    /// open is the header of the file as it was then; a schema written by
    /// another process is only visible once page one is read again.
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
        if state.readers != 0 {
            return Err(error::misuse(
                "the schema cannot be reloaded while a statement is running",
            ));
        }
        let mut pager =
            Pager::open_read_only(self.vfs.as_ref(), &self.path, PagerOptions::default())?;
        let loaded = load_catalog(
            &mut pager,
            &self.options.main_name,
            generation,
            self.options.busy_timeout,
        )?;
        state.pager = pager;
        let mut catalog = self
            .catalog
            .try_borrow_mut()
            .map_err(|_| error::misuse("the catalog is in use"))?;
        *catalog = Arc::new(loaded);
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

/// Takes the read lock, retrying a busy file until the timeout runs out.
///
/// Only `Busy` is retried. An I/O failure or a corrupt header is returned at
/// once, because retrying either of those just delays the same answer.
fn begin_read_with_timeout(pager: &mut Pager, timeout: std::time::Duration) -> DbResult<()> {
    let started = std::time::Instant::now();
    loop {
        match pager.begin_read() {
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
