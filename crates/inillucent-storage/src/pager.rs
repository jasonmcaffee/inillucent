//! The read-only pager: open, lock, read, and fail stickily.
//!
//! Invariant: a read-only pager writes nothing. Not the database, not the
//! journal, not the WAL, not the directory. It opens the file read-only at the
//! VFS, takes a SHARED lock and no stronger one, and every method that could
//! reach a write path is simply absent rather than being present and
//! conditional - there is no `get_page_mut` here to forget to guard.
//!
//! The second invariant is stickiness. Once the pager has seen an I/O error or
//! a corrupt page it stops answering, and every later call returns the same
//! error. The alternative - carrying on and letting the next read succeed -
//! means a query that hit an unreadable page halfway through returns a partial
//! answer that looks complete, which is worse than an error, because nothing
//! downstream can tell the difference.
//!
//! The state machine here is the read-only half of the one the TDD describes.
//! `Closed`, `Open`, `Reader` and `Error` are reachable; the four writer
//! states are named in [`PagerState`] so that phase 4 extends this machine
//! rather than replacing it, and a transition table test pins down which
//! moves are legal today.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use inillucent_base::buffer::{self, PageBuffer};
use inillucent_base::bytes;
use inillucent_base::error::{corrupt, misuse};
use inillucent_base::ids::{DatabaseId, PageId};
use inillucent_base::page::{self, PageSize};
use inillucent_base::{DbError, DbResult};
use inillucent_value::TextEncoding;
use inillucent_vfs::{DbPath, FileLock, OpenOptions, SyncMode, Vfs, VfsFile};

use crate::btree::PageKind;
use crate::cache::{CacheCounters, PageCache, PageKey, PagePin, PageState};
use crate::header::{DatabaseHeader, VacuumMode, HEADER_SIZE};
use crate::journal::{Journal, JournalStats};
use crate::wal::{CheckpointMode, CheckpointOutcome, WalSnapshot, WriteAheadLog};

/// What the pager is doing.
///
/// The writer states exist so that the transition table is the whole machine
/// rather than the part phase 3 needs; nothing in this phase can enter one.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PagerState {
    /// No file is open.
    Closed,
    /// The file is open and no lock is held.
    Open,
    /// A SHARED lock is held and pages may be read.
    Reader,
    /// Writer rights are reserved. Not reachable in a read-only pager.
    WriterLocked,
    /// The cache holds modifications. Not reachable in a read-only pager.
    WriterCacheMod,
    /// The database file has been modified. Not reachable in a read-only pager.
    WriterDbMod,
    /// A sticky error has been recorded and every call returns it.
    Error,
}

impl PagerState {
    /// Reports whether pages may be read in this state.
    pub fn can_read(self) -> bool {
        matches!(
            self,
            PagerState::Reader
                | PagerState::WriterLocked
                | PagerState::WriterCacheMod
                | PagerState::WriterDbMod
        )
    }

    /// Reports whether a read-only pager may enter this state at all.
    pub fn is_read_only_state(self) -> bool {
        matches!(
            self,
            PagerState::Closed | PagerState::Open | PagerState::Reader | PagerState::Error
        )
    }
}

/// The library version number inillucent stamps into a header it writes.
///
/// SQLite writes `SQLITE_VERSION_NUMBER` here, and the field is informational:
/// nothing reads it back to decide behaviour. inillucent writes the number of the
/// release whose file format it implements, which is the pinned reference,
/// because that is the true statement about the bytes in the file. It is not a
/// claim to be that build, and no inillucent code reads this field.
pub const WRITE_LIBRARY_VERSION: u32 = 3_053_004;

/// A place a storage operation can be made to fail.
///
/// The TDD asks for a failpoint counter on every allocator operation, and this
/// is it. The sites are the points where a statement can leave the database
/// half-changed: a page has been edited, a page has been taken from the
/// freelist, a page has been given back, the file has been made shorter, a page
/// has been moved, or the transaction is about to reach the disk. Injecting at
/// each of them in turn is how "a failure never leaves a mess" stops being a
/// claim and becomes a test.
///
/// The counter is inert unless armed, and the check is one comparison against
/// an `Option`. It lives in the shipping pager rather than behind a feature
/// flag because a failpoint that is compiled out is a failpoint that is not
/// tested in the build that ships.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum FailSite {
    /// A page is about to be modified.
    PageEdit,
    /// A page is about to be taken for a new use.
    Allocate,
    /// A page is about to go back on the freelist.
    Free,
    /// The database is about to get shorter.
    Truncate,
    /// A page is about to be moved by a vacuum.
    Relocate,
    /// A transaction is about to be written to the file.
    Commit,
}

impl FailSite {
    /// Returns every site, so a campaign can enumerate them.
    pub fn all() -> [FailSite; 6] {
        [
            FailSite::PageEdit,
            FailSite::Allocate,
            FailSite::Free,
            FailSite::Truncate,
            FailSite::Relocate,
            FailSite::Commit,
        ]
    }
}

/// How to open a pager.
#[derive(Clone, Copy, Debug)]
pub struct PagerOptions {
    /// The cache's byte budget.
    pub cache_bytes: u64,
    /// Which attached database this is, which keys the cache.
    pub database: DatabaseId,
    /// How long a commit waits for a reader to leave before reporting BUSY.
    ///
    /// It is here rather than only in the session because the commit is the
    /// one place the pager takes a lock nothing above it can retry for. A
    /// statement that cannot get its reservation has done nothing yet and the
    /// caller can start again; a commit that cannot get EXCLUSIVE has already
    /// written and synced its journal, and starting again would mean writing
    /// it twice. So the wait happens where the lock is asked for.
    pub busy_timeout: std::time::Duration,
}

impl Default for PagerOptions {
    /// A two-megabyte cache on the main database, which is roughly SQLite's
    /// own default of 2000 pages of 1 KiB, and SQLite's default of giving up
    /// at once on a busy file.
    fn default() -> PagerOptions {
        PagerOptions {
            cache_bytes: 2 * 1024 * 1024,
            database: DatabaseId(0),
            busy_timeout: std::time::Duration::ZERO,
        }
    }
}

/// What the pager has done, for the read-only performance baselines.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct PagerCounters {
    /// Pages read from the file.
    pub page_reads: u64,
    /// Bytes read from the file.
    pub bytes_read: u64,
    /// Pages served without touching the file.
    pub cache_hits: u64,
    /// Times a page number outside the database was asked for.
    pub out_of_range: u64,
    /// Pages written to the file.
    pub page_writes: u64,
    /// Bytes written to the file.
    pub bytes_written: u64,
    /// Page images copied, which is what a write costs before it reaches the
    /// file: one for the undo image and one for the edited copy.
    pub page_images: u64,
    /// Pages taken from the freelist or from growth.
    pub pages_allocated: u64,
    /// Pages returned to the freelist.
    pub pages_freed: u64,
    /// Times the file was made shorter.
    pub truncations: u64,
    /// Transactions committed.
    pub commits: u64,
    /// Transactions rolled back.
    pub rollbacks: u64,
}

/// A read-only pager over one database file.
#[derive(Debug)]
pub struct Pager {
    file: Box<dyn VfsFile>,
    path: DbPath,
    header: DatabaseHeader,
    file_bytes: u64,
    page_count: u32,
    cache: Arc<PageCache>,
    /// How many frames one automatic checkpoint copies, when it is bounded.
    checkpoint_budget: Option<u32>,
    database: DatabaseId,
    state: PagerState,
    sticky: Option<DbError>,
    counters: PagerCounters,
    read_only: bool,
    dirty: BTreeSet<u32>,
    undo: Vec<UndoFrame>,
    freed: BTreeSet<u32>,
    sites_reached: u64,
    fail_at: Option<(u64, Option<FailSite>)>,
    journal: Option<Box<dyn Journal>>,
    /// How long a commit waits for readers before reporting BUSY.
    busy_timeout: std::time::Duration,
    /// The largest the file may grow to, in pages.
    ///
    /// `PRAGMA max_page_count` is a limit an application sets to bound what a
    /// runaway statement can do to the disk, so it is enforced where pages are
    /// allocated rather than checked by whoever set it.
    max_page_count: u32,
    journalled: BTreeSet<u32>,
    wrote_database: bool,
    journal_totals: JournalStats,
    /// The pages phase one wrote, which phase two marks clean.
    committing: Vec<u32>,
    wal: Option<Box<dyn WriteAheadLog>>,
    /// The snapshot the open read transaction is pinned to, in WAL mode.
    ///
    /// It is kept after the read transaction ends so that the next one can ask
    /// whether anything changed. A cache full of pages from a snapshot that
    /// has moved on is the one way WAL mode can serve a stale page, and
    /// comparing two snapshots is how that is caught.
    wal_snapshot: Option<WalSnapshot>,
}

/// One level of undo: everything needed to put the database back the way it
/// was when the level was opened.
///
/// The transaction itself is the bottom level, a statement is the next, and a
/// savepoint is any level above that. They are the same structure because they
/// answer the same question - what did these pages hold before? - and keeping
/// one implementation means a statement rollback and a savepoint rollback
/// cannot drift apart.
#[derive(Debug)]
struct UndoFrame {
    /// The name a caller rolls back to, or `None` for the transaction level.
    name: Option<String>,
    /// The page images taken at this level, with `None` for a page that did
    /// not exist when the level was opened.
    images: BTreeMap<u32, Option<PageBuffer>>,
    /// The page count when the level was opened.
    page_count: u32,
    /// The header when the level was opened.
    header: DatabaseHeader,
}

impl Pager {
    /// Opens a database file read-only and validates its header.
    ///
    /// The file is opened through the VFS with `read_only` set, so the
    /// operating system refuses a write even if a later bug attempts one.
    pub fn open_read_only(vfs: &dyn Vfs, path: &DbPath, options: PagerOptions) -> DbResult<Pager> {
        Pager::open_read_only_with_cache(
            vfs,
            path,
            options,
            Arc::new(PageCache::new(options.cache_bytes)),
        )
    }

    /// Opens a database read-only against a cache the caller already owns.
    ///
    /// Two pagers over the same file share one cache, which is what makes a
    /// second connection cheap; the cache key carries the database identity so
    /// two different files never collide.
    pub fn open_read_only_with_cache(
        vfs: &dyn Vfs,
        path: &DbPath,
        options: PagerOptions,
        cache: Arc<PageCache>,
    ) -> DbResult<Pager> {
        let file = vfs.open(
            path,
            OpenOptions::of_kind(inillucent_vfs::FileKind::MainDb).read_only(),
        )?;
        let file_bytes = file.file_size()?;
        if file_bytes < HEADER_SIZE as u64 {
            return Err(corrupt(format!(
                "a database file of {file_bytes} bytes cannot hold a header"
            )));
        }
        let mut raw = [0u8; HEADER_SIZE];
        file.read_exact_at(0, &mut raw)?;
        let header = DatabaseHeader::decode(&raw)?;
        header.validate_against_file(file_bytes)?;
        let page_count = header.effective_page_count(file_bytes);
        Ok(Pager {
            file,
            path: path.clone(),
            header,
            file_bytes,
            page_count,
            cache,
            checkpoint_budget: None,
            database: options.database,
            state: PagerState::Open,
            sticky: None,
            counters: PagerCounters::default(),
            read_only: true,
            dirty: BTreeSet::new(),
            undo: Vec::new(),
            freed: BTreeSet::new(),
            sites_reached: 0,
            fail_at: None,
            journal: None,
            busy_timeout: options.busy_timeout,
            max_page_count: u32::MAX - 1,
            journalled: BTreeSet::new(),
            wrote_database: false,
            journal_totals: JournalStats::default(),
            committing: Vec::new(),
            wal: None,
            wal_snapshot: None,
        })
    }

    /// Returns the decoded header.
    pub fn header(&self) -> &DatabaseHeader {
        &self.header
    }

    /// Returns the path the database was opened from.
    pub fn path(&self) -> &DbPath {
        &self.path
    }

    /// Returns the page size.
    pub fn page_size(&self) -> PageSize {
        self.header.page_size
    }

    /// Returns the usable bytes per page, after the reserved tail.
    pub fn usable_size(&self) -> DbResult<u32> {
        self.header.usable_size()
    }

    /// Returns the database's text encoding.
    pub fn text_encoding(&self) -> TextEncoding {
        self.header.text_encoding
    }

    /// Returns how many pages the database has.
    pub fn page_count(&self) -> u32 {
        self.page_count
    }

    /// Returns how long a commit waits for readers before reporting BUSY.
    pub fn busy_timeout(&self) -> std::time::Duration {
        self.busy_timeout
    }

    /// Changes how long a commit waits for readers before reporting BUSY.
    pub fn set_busy_timeout(&mut self, timeout: std::time::Duration) {
        self.busy_timeout = timeout;
    }

    /// Returns the largest the file may grow to, in pages.
    pub fn max_page_count(&self) -> u32 {
        self.max_page_count
    }

    /// Returns how many whole pages the file actually holds.
    ///
    /// This is not the same number as [`page_count`](Self::page_count): the
    /// header may claim more, and a reader has to be able to tell the
    /// difference. Anything that sizes a buffer or walks a range from a header
    /// field must bound itself by *this* number, because the header is bytes
    /// somebody else wrote and the file's length is not.
    pub fn pages_in_file(&self) -> u32 {
        page::page_count(self.header.page_size, self.file_bytes)
    }

    /// Returns the largest payload this database could possibly hold.
    ///
    /// A cell's payload length is a varint on a page, so it can claim any
    /// number at all. Nothing can be longer than the file, and that bound is
    /// what stops a four-byte lie turning into a gigabyte allocation.
    pub fn largest_possible_payload(&self) -> u64 {
        // While a write transaction is open the file is behind the database:
        // pages that have been grown into exist in the cache and reach the file
        // only at commit, so a payload written into a fresh overflow chain is
        // longer than the file is. The logical page count is the truth then,
        // and the file's length is the truth at every other time - a header
        // that claims more pages than the file holds is exactly the lie this
        // bound exists to refuse.
        let pages = if self.is_writing() {
            self.page_count.max(self.pages_in_file())
        } else {
            self.pages_in_file()
        };
        u64::from(pages).saturating_mul(u64::from(self.header.page_size.bytes()))
    }

    /// Returns the pager's state.
    pub fn state(&self) -> PagerState {
        self.state
    }

    /// Returns which attached database this pager is for.
    pub fn database_id(&self) -> DatabaseId {
        self.database
    }

    /// Returns the cache this pager reads through.
    pub fn cache(&self) -> &Arc<PageCache> {
        &self.cache
    }

    /// Returns what the pager has done.
    pub fn counters(&self) -> PagerCounters {
        self.counters
    }

    /// Returns the cache's counters.
    pub fn cache_counters(&self) -> CacheCounters {
        self.cache.counters()
    }

    /// Returns the sticky error, if one has been recorded.
    pub fn sticky_error(&self) -> Option<&DbError> {
        self.sticky.as_ref()
    }

    /// Takes a SHARED lock and enters the reader state.
    ///
    /// Entering a read twice is not an error: a nested cursor asks for a read
    /// and the outer one already has it, which is exactly the case SQLite's
    /// reference counting handles. The lock is taken once.
    pub fn begin_read(&mut self) -> DbResult<()> {
        self.check_usable()?;
        if self.state == PagerState::Reader {
            return Ok(());
        }
        if self.state != PagerState::Open {
            return Err(misuse(format!(
                "begin_read from the {:?} state",
                self.state
            )));
        }
        if self.wal.is_some() {
            return self.begin_wal_read();
        }
        if let Err(error) = self.file.lock(FileLock::Shared) {
            // A busy file is not a corrupt one, so the pager stays usable and
            // the caller may retry; only an I/O failure is sticky.
            let error = error.into_db_error();
            if error.code() == inillucent_base::PrimaryCode::Busy {
                return Err(error);
            }
            return Err(self.fail(error));
        }
        self.state = PagerState::Reader;
        if let Err(error) = self.discard_a_stale_cache() {
            return Err(self.fail(error));
        }
        Ok(())
    }

    /// Drops the cache when another connection has committed since this pager
    /// last looked at the file.
    ///
    /// The change counter is what makes this both cheap and correct. It moves
    /// on every commit, so a reader that finds it where it left it knows every
    /// page it has cached is still what the file holds, and one that finds it
    /// moved knows nothing it has cached can be trusted. A hundred bytes are
    /// read to find out, which is what SQLite reads on the way into a read
    /// transaction and for exactly this reason.
    ///
    /// Without it a connection serves pages from before another connection's
    /// commit for as long as they stay in its cache - not a stale *snapshot*,
    /// which would at least be consistent, but whatever mixture of old and new
    /// pages the cache happens to hold. WAL mode has its own version of this
    /// check in `begin_wal_read`, where the snapshot rather than the counter is
    /// what has to agree.
    fn discard_a_stale_cache(&mut self) -> DbResult<()> {
        if self.file_bytes < HEADER_SIZE as u64 {
            return Ok(());
        }
        let previous = self.header.change_counter;
        let mut prefix = [0u8; HEADER_SIZE];
        self.file.read_exact_at(0, &mut prefix)?;
        let header = DatabaseHeader::decode(&prefix)?;
        if header.change_counter == previous {
            return Ok(());
        }
        self.file_bytes = self.file.file_size()?;
        self.page_count = header.effective_page_count(self.file_bytes);
        self.header = header;
        // Page zero does not exist, so "above zero" is every page.
        self.cache.discard_above(self.database, 0);
        Ok(())
    }

    /// Drops the SHARED lock and releases every unpinned page.
    ///
    /// The lock is released whenever the *file* holds one, rather than
    /// whenever the pager thinks it is a reader. Those two came apart once: a
    /// sticky error moves the pager to `Error` while the SHARED lock is still
    /// held, and an `end_read` that keyed on the state left the lock in place
    /// for the life of the process - so one unreadable page blocked every
    /// writer on the machine. A lock is released by whoever holds it, and what
    /// holds it is the file handle.
    pub fn end_read(&mut self) -> DbResult<()> {
        self.cache.release_unpinned();
        let ended = match self.wal.as_mut() {
            Some(wal) => wal.end_read(),
            None => Ok(()),
        };
        if self.file.lock_level() != FileLock::None {
            self.file.unlock(FileLock::None)?;
        }
        if self.state == PagerState::Reader {
            self.state = PagerState::Open;
        }
        ended
    }

    /// Returns the lock level the pager currently holds.
    pub fn lock_level(&self) -> FileLock {
        self.file.lock_level()
    }

    /// Reads one page, from the cache when it is there and from the file when
    /// it is not.
    pub fn get_page(&mut self, page: PageId) -> DbResult<PagePin> {
        self.check_usable()?;
        if !self.state.can_read() {
            return Err(misuse(format!("get_page from the {:?} state", self.state)));
        }
        if page.get() > self.page_count {
            self.counters.out_of_range = self.counters.out_of_range.saturating_add(1);
            return Err(corrupt(format!(
                "page {} is outside a {}-page database",
                page.get(),
                self.page_count
            )));
        }
        let key = PageKey {
            database: self.database,
            page,
        };
        if let Some(pin) = self.cache.get(key) {
            self.counters.cache_hits = self.counters.cache_hits.saturating_add(1);
            return Ok(pin);
        }
        self.cache.record_miss();
        if self.wal.is_some() {
            return match self.read_page_through_wal(key, page) {
                Ok(pin) => Ok(pin),
                Err(error) => Err(self.fail(error)),
            };
        }
        let pin = match self.read_page(key, page) {
            Ok(pin) => pin,
            Err(error) => return Err(self.fail(error)),
        };
        Ok(pin)
    }

    /// Reads one page from the file into a fresh frame.
    fn read_page(&mut self, key: PageKey, page: PageId) -> DbResult<PagePin> {
        let size = self.header.page_size;
        let mut buffer = PageBuffer::zeroed(size)?;
        let offset = page::page_offset(size, page)?;
        let end = offset.saturating_add(u64::from(size.bytes()));
        if end > self.file_bytes {
            // A writer that has grown the database has pages the file does not
            // hold yet: they exist from the moment the page count moved, and
            // they read as zeroes until something writes them. Outside a write
            // transaction the same shape is a file shorter than its header
            // claims, which is corruption.
            if !self.is_writing() {
                return Err(corrupt(format!(
                    "page {} ends at byte {end} in a {}-byte file",
                    page.get(),
                    self.file_bytes
                )));
            }
            return self.cache.insert(key, buffer);
        }
        self.file.read_exact_at(offset, buffer.as_mut_slice())?;
        self.counters.page_reads = self.counters.page_reads.saturating_add(1);
        self.counters.bytes_read = self
            .counters
            .bytes_read
            .saturating_add(u64::from(size.bytes()));
        self.cache.insert(key, buffer)
    }

    /// Records a sticky error and returns it.
    ///
    /// The error is cloned rather than moved so the caller gets the same
    /// error it would have got without the stickiness, and every later call
    /// gets it too.
    fn fail(&mut self, error: DbError) -> DbError {
        if self.sticky.is_none() {
            self.sticky = Some(error.clone());
            self.state = PagerState::Error;
        }
        error
    }

    /// Returns the sticky error when there is one.
    fn check_usable(&self) -> DbResult<()> {
        match &self.sticky {
            Some(error) => Err(error.clone()),
            None => Ok(()),
        }
    }

    /// Clears a sticky error, for a caller that has decided to retry.
    ///
    /// This exists for the diagnostic path - an integrity check that wants to
    /// keep going after a bad page - and not for the query path, which must
    /// not turn a partial answer into a complete-looking one.
    pub fn clear_sticky_error(&mut self) {
        self.sticky = None;
        if self.state == PagerState::Error {
            self.state = if self.file.lock_level() >= FileLock::Shared {
                PagerState::Reader
            } else {
                PagerState::Open
            };
        }
    }

    /// Closes the pager, releasing its lock and its unpinned pages.
    pub fn close(&mut self) -> DbResult<()> {
        let closed = match self.wal.take() {
            Some(mut wal) => {
                let outcome = wal.close(self.file.as_ref());
                self.wal = Some(wal);
                outcome
            }
            None => Ok(()),
        };
        if self.file.lock_level() != FileLock::None {
            let _ = self.file.unlock(FileLock::None);
        }
        self.cache.release_unpinned();
        self.state = PagerState::Closed;
        closed
    }
}

/// Whether a commit still owes its second phase.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CommitPhase {
    /// There was nothing to commit, and the transaction is already over.
    Nothing,
    /// The database is durable and the journal is still hot.
    Owed,
}

/// What a new database file starts as.
#[derive(Clone, Copy, Debug)]
pub struct NewDatabase {
    /// The page size, which cannot be changed afterwards.
    pub page_size: PageSize,
    /// Bytes reserved at the end of every page.
    pub reserved_bytes: u8,
    /// The text encoding every record in the file will use.
    pub text_encoding: TextEncoding,
    /// Whether the file carries a pointer map.
    pub vacuum_mode: VacuumMode,
}

impl Default for NewDatabase {
    /// SQLite's own defaults: 4 KiB pages, no reserved tail, UTF-8, no vacuum.
    fn default() -> NewDatabase {
        NewDatabase {
            page_size: PageSize::DEFAULT,
            reserved_bytes: 0,
            text_encoding: TextEncoding::Utf8,
            vacuum_mode: VacuumMode::None,
        }
    }
}

/// The write half: opening for write, editing pages, and committing.
///
/// The invariant the read half states - that a read-only pager has no write
/// path to forget to guard - is kept by construction here too. A pager opened
/// through [`Pager::open_read_only`] has its file handle opened read-only at
/// the VFS and its `read_only` flag set, so every method below refuses before
/// it reaches the file, and the operating system refuses after it.
///
/// Phase 4's commit is not crash-atomic and does not claim to be. It orders its
/// writes - every page but the first, sync, then the first page, sync - so that
/// a crash usually leaves a file whose header still describes the old database,
/// but a torn write inside a page can still leave a mixture, and only the
/// rollback journal of phase 7 rules that out. What *is* atomic here is the
/// in-memory transaction: nothing a statement did survives a statement
/// rollback, and nothing a transaction did survives a transaction rollback.
impl Pager {
    /// Opens a database file for reading and writing.
    ///
    /// A writer never shares a cache. An uncommitted page lives in the cache
    /// as a dirty frame, and a second connection reading through the same
    /// cache would see a change that has not been committed - which is the one
    /// thing the cache contract forbids. Phase 10's snapshots are what make
    /// sharing safe; until then the writer owns its frames.
    pub fn open_read_write(vfs: &dyn Vfs, path: &DbPath, options: PagerOptions) -> DbResult<Pager> {
        let cache = Arc::new(PageCache::new(options.cache_bytes));
        let mut pager = Pager::open_read_only_with_cache(vfs, path, options, cache)?;
        // The read-only open took a read-only handle; a writer needs one the
        // operating system will accept a write on.
        pager.file = vfs.open(path, OpenOptions::of_kind(inillucent_vfs::FileKind::MainDb))?;
        pager.read_only = false;
        Ok(pager)
    }

    /// Creates an empty database file and opens it for writing.
    ///
    /// The file is one page long: page 1 is the hundred-byte header followed by
    /// an empty table-leaf B-tree, which is what `sqlite_schema` is before
    /// anything has been declared. This is the file SQLite produces for a
    /// database that has been created and had one empty transaction committed.
    pub fn create(
        vfs: &dyn Vfs,
        path: &DbPath,
        options: PagerOptions,
        spec: NewDatabase,
    ) -> DbResult<Pager> {
        let usable = spec.page_size.usable(spec.reserved_bytes)?;
        let header = DatabaseHeader {
            page_size: spec.page_size,
            write_version: 1,
            read_version: 1,
            reserved_bytes: spec.reserved_bytes,
            change_counter: 1,
            database_size: 1,
            freelist_head: 0,
            freelist_count: 0,
            schema_cookie: 0,
            schema_format: 4,
            cache_size: 0,
            largest_root: if spec.vacuum_mode == VacuumMode::None {
                0
            } else {
                1
            },
            text_encoding: spec.text_encoding,
            user_version: 0,
            vacuum_mode: spec.vacuum_mode,
            application_id: 0,
            reserved_expansion_is_zero: true,
            version_valid_for: 1,
            write_library_version: WRITE_LIBRARY_VERSION,
        };
        let mut page = PageBuffer::zeroed(spec.page_size)?;
        {
            let raw = page.as_mut_slice();
            header.encode(bytes::window_mut(raw, 0, HEADER_SIZE)?)?;
            crate::edit::initialize_btree_page(raw, HEADER_SIZE, PageKind::LeafTable, usable)?;
        }
        let file = vfs.open(path, OpenOptions::of_kind(inillucent_vfs::FileKind::MainDb))?;
        file.write_all_at(0, page.as_slice())?;
        file.sync(SyncMode::Normal)?;
        drop(file);
        Pager::open_read_write(vfs, path, options)
    }

    /// Opens a database whose header can only be read through a log.
    ///
    /// An interrupted checkpoint leaves the database file holding pages that
    /// were half copied out of the log, and page one is usually the first of
    /// them - so the hundred bytes at the front of the file are not a header
    /// any more. That is not corruption: every one of those pages is still in
    /// the log, and the next checkpoint copies them again. But the header has
    /// to come from somewhere before the log can be consulted, so this opens
    /// the pager with a provisional one carrying only the page size the log
    /// declares. `begin_read` replaces it with the real header as soon as a
    /// snapshot exists, which is what SQLite does too: it reads page one after
    /// it has opened the log, never before.
    pub fn open_with_header_from_log(
        vfs: &dyn Vfs,
        path: &DbPath,
        options: PagerOptions,
        page_size: PageSize,
        writable: bool,
    ) -> DbResult<Pager> {
        let mut open = OpenOptions::of_kind(inillucent_vfs::FileKind::MainDb);
        if !writable {
            open = open.read_only();
        }
        let file = vfs.open(path, open)?;
        let file_bytes = file.file_size()?;
        Ok(Pager {
            file,
            path: path.clone(),
            header: DatabaseHeader::provisional(page_size),
            file_bytes,
            page_count: 0,
            cache: Arc::new(PageCache::new(options.cache_bytes)),
            checkpoint_budget: None,
            database: options.database,
            state: PagerState::Open,
            sticky: None,
            counters: PagerCounters::default(),
            read_only: !writable,
            dirty: BTreeSet::new(),
            undo: Vec::new(),
            freed: BTreeSet::new(),
            sites_reached: 0,
            fail_at: None,
            journal: None,
            busy_timeout: options.busy_timeout,
            max_page_count: u32::MAX - 1,
            journalled: BTreeSet::new(),
            wrote_database: false,
            journal_totals: JournalStats::default(),
            committing: Vec::new(),
            wal: None,
            wal_snapshot: None,
        })
    }

    /// Attaches the journal that makes this pager's commits crash-atomic.
    ///
    /// A pager with no journal attached still commits, and still orders its
    /// writes so that a crash usually leaves the old database - that is what
    /// phase 4 delivered and what the storage-level tests exercise. What it
    /// cannot do is promise it, and the promise is the whole point, so every
    /// path that opens a database for an application goes through
    /// `inillucent_transaction::open_database`, which attaches one.
    pub fn attach_journal(&mut self, journal: Box<dyn Journal>) {
        self.journal = Some(journal);
    }

    /// Reports whether a write transaction is open.
    pub fn is_writing(&self) -> bool {
        matches!(
            self.state,
            PagerState::WriterLocked | PagerState::WriterCacheMod | PagerState::WriterDbMod
        )
    }

    /// Returns how many pages the transaction has modified.
    pub fn dirty_page_count(&self) -> usize {
        self.dirty.len()
    }

    /// Begins a write transaction, taking a RESERVED lock.
    ///
    /// Entering twice is not an error, for the reason `begin_read` gives: a
    /// nested caller asks for rights the outer one already holds.
    pub fn begin_write(&mut self) -> DbResult<()> {
        self.check_usable()?;
        if self.read_only {
            return Err(misuse("a read-only pager cannot begin a write"));
        }
        if self.is_writing() {
            return Ok(());
        }
        if self.state == PagerState::Open || self.state == PagerState::Closed {
            self.begin_read()?;
        }
        if self.state != PagerState::Reader {
            return Err(misuse(format!(
                "begin_write from the {:?} state",
                self.state
            )));
        }
        match self.wal.as_mut() {
            // A log has no RESERVED lock to take. What it has instead is one
            // write slot and a check that the snapshot this connection read is
            // still the newest, which is the same exclusion arriving as a
            // different answer: BUSY when somebody else is writing, and
            // BUSY_SNAPSHOT when somebody else already has.
            Some(wal) => wal.begin_write()?,
            None => {
                if let Err(error) = self.file.lock(FileLock::Reserved) {
                    let error = error.into_db_error();
                    if error.code() == inillucent_base::PrimaryCode::Busy {
                        return Err(error);
                    }
                    return Err(self.fail(error));
                }
            }
        }
        self.dirty.clear();
        self.undo.clear();
        self.freed.clear();
        self.journalled.clear();
        self.wrote_database = false;
        let page_size = self.header.page_size.bytes();
        let page_count = self.page_count;
        if let Some(journal) = self.journal.as_mut() {
            if let Err(error) = journal.begin(page_size, page_count) {
                let _ = self.file.unlock(FileLock::Shared);
                return Err(self.fail(error));
            }
        }
        self.undo.push(UndoFrame {
            name: None,
            images: BTreeMap::new(),
            page_count: self.page_count,
            header: self.header,
        });
        self.state = PagerState::WriterLocked;
        Ok(())
    }

    /// Opens an unnamed undo level, which is what a statement runs inside.
    pub fn begin_statement(&mut self) -> DbResult<()> {
        self.push_undo_level(None)
    }

    /// Opens a named undo level, which is what a savepoint is.
    pub fn begin_savepoint(&mut self, name: &str) -> DbResult<()> {
        self.push_undo_level(Some(name.to_string()))
    }

    /// Returns the depth a named savepoint sits at, if it is open.
    pub fn savepoint_depth(&self, name: &str) -> Option<usize> {
        self.undo
            .iter()
            .rposition(|frame| frame.name.as_deref() == Some(name))
            .map(|index| index.saturating_add(1))
    }

    /// Closes the innermost undo level, keeping its changes.
    ///
    /// The level's images move down to its parent for every page the parent
    /// does not already hold one for, because the parent still has to be able
    /// to put those pages back the way *it* found them.
    pub fn release_statement(&mut self) -> DbResult<()> {
        self.require_writer()?;
        if self.undo.len() <= 1 {
            return Err(misuse("there is no open statement to release"));
        }
        let Some(frame) = self.undo.pop() else {
            return Err(misuse("there is no open statement to release"));
        };
        let Some(parent) = self.undo.last_mut() else {
            return Err(misuse("the transaction level is missing"));
        };
        for (page, image) in frame.images {
            parent.images.entry(page).or_insert(image);
        }
        Ok(())
    }

    /// Closes every undo level up to and including the named savepoint.
    pub fn release_savepoint(&mut self, name: &str) -> DbResult<()> {
        self.require_writer()?;
        let depth = self.find_savepoint(name)?;
        while self.undo.len() >= depth && self.undo.len() > 1 {
            self.release_statement()?;
        }
        Ok(())
    }

    /// Undoes everything the innermost level did.
    pub fn rollback_statement(&mut self) -> DbResult<()> {
        self.require_writer()?;
        if self.undo.len() <= 1 {
            return Err(misuse("there is no open statement to roll back"));
        }
        let Some(frame) = self.undo.pop() else {
            return Err(misuse("there is no open statement to roll back"));
        };
        self.restore(frame)
    }

    /// Undoes everything done since the named savepoint, leaving it open.
    pub fn rollback_to_savepoint(&mut self, name: &str) -> DbResult<()> {
        self.require_writer()?;
        let depth = self.find_savepoint(name)?;
        while self.undo.len() >= depth && self.undo.len() > 1 {
            let Some(frame) = self.undo.pop() else {
                break;
            };
            self.restore(frame)?;
        }
        let reopen = self
            .undo
            .last()
            .map(|frame| (self.page_count, self.header, frame.name.is_some()));
        if let Some((page_count, header, _)) = reopen {
            self.undo.push(UndoFrame {
                name: Some(name.to_string()),
                images: BTreeMap::new(),
                page_count,
                header,
            });
        }
        Ok(())
    }

    /// Edits one page, taking a before image the first time each undo level
    /// sees it.
    ///
    /// The edit runs against an owned copy and is published only when it
    /// returns, so an edit that fails leaves the page exactly as it was. The
    /// copy is also what makes a cursor safe across a write: the resident frame
    /// is replaced rather than mutated, so a cursor that parsed the old page
    /// keeps reading the bytes it validated and sees its recorded version go
    /// stale, which is the signal to reseek.
    pub fn edit_page<R>(
        &mut self,
        page: PageId,
        edit: impl FnOnce(&mut [u8]) -> DbResult<R>,
    ) -> DbResult<R> {
        self.check_usable()?;
        self.require_writer()?;
        self.reach_failpoint(FailSite::PageEdit)?;
        if page.get() > self.page_count {
            return Err(misuse(format!(
                "page {} is outside a {}-page database",
                page.get(),
                self.page_count
            )));
        }
        #[cfg(feature = "opcode-probe")]
        let stage = std::time::Instant::now();
        self.record_image(page)?;
        #[cfg(feature = "opcode-probe")]
        let stage = {
            inillucent_base::probe::record_stage(0, stage.elapsed().as_nanos() as u64);
            std::time::Instant::now()
        };
        let current = self.get_page(page)?;
        let mut buffer = self.copy_bytes(current.bytes())?;
        drop(current);
        #[cfg(feature = "opcode-probe")]
        let stage = {
            inillucent_base::probe::record_stage(1, stage.elapsed().as_nanos() as u64);
            std::time::Instant::now()
        };
        let result = edit(buffer.as_mut_slice())?;
        #[cfg(feature = "opcode-probe")]
        inillucent_base::probe::record_stage(2, stage.elapsed().as_nanos() as u64);
        let key = PageKey {
            database: self.database,
            page,
        };
        // The page was just written by one of the `edit` primitives, so its
        // layout is known here and re-deriving it on the next read would be
        // this crate proving its own output to itself. A page that is not a
        // B-tree page at all - the header, a freelist page, an overflow page -
        // has no layout to carry, and gets none.
        let usable = self.usable_size()?;
        #[cfg(feature = "opcode-probe")]
        let stage = std::time::Instant::now();
        let layout = crate::btree::PageLayout::parse_edited(buffer.as_slice(), page, usable)
            .ok()
            .map(std::sync::Arc::new);
        #[cfg(feature = "opcode-probe")]
        let stage = {
            inillucent_base::probe::record_stage(3, stage.elapsed().as_nanos() as u64);
            std::time::Instant::now()
        };
        self.cache.publish_with(
            key,
            buffer,
            PageState::Dirty {
                before_image_saved: true,
            },
            layout,
        )?;
        #[cfg(feature = "opcode-probe")]
        inillucent_base::probe::record_stage(4, stage.elapsed().as_nanos() as u64);
        self.dirty.insert(page.get());
        if self.state == PagerState::WriterLocked {
            self.state = PagerState::WriterCacheMod;
        }
        Ok(result)
    }

    /// Changes how many pages the database has.
    ///
    /// Growing is free: the new pages read as zeroes until something writes
    /// them, and the file itself only grows at commit. Shrinking drops the
    /// frames above the new count so a later read cannot be served the contents
    /// of a page that is no longer part of the database, and takes a before
    /// image of every dirty page it drops so a rollback can put them back.
    pub fn set_page_count(&mut self, count: u32) -> DbResult<()> {
        self.check_usable()?;
        self.require_writer()?;
        if count == 0 {
            return Err(misuse("a database must have at least one page"));
        }
        if count == self.page_count {
            return Ok(());
        }
        if count < self.page_count {
            self.reach_failpoint(FailSite::Truncate)?;
            for page in count.saturating_add(1)..=self.page_count {
                if let Ok(page_id) = PageId::from_persisted(page) {
                    // The journal needs every dropped page, not just the dirty
                    // ones: commit truncates the file, so a rollback that only
                    // restored the page count would restore zeroes. In-memory
                    // rollback does not need the clean ones, because the file
                    // still holds them until the commit truncates it.
                    self.journal_original(page_id)?;
                    if self.dirty.contains(&page) {
                        self.record_image(page_id)?;
                    }
                }
                self.dirty.remove(&page);
            }
            self.counters.truncations = self.counters.truncations.saturating_add(1);
            self.cache.discard_above(self.database, count);
        }
        self.page_count = count;
        if self.state == PagerState::WriterLocked {
            self.state = PagerState::WriterCacheMod;
        }
        Ok(())
    }

    /// Writes a new header, which is the only way page 1's first hundred bytes
    /// change.
    ///
    /// The page size and the reserved tail are fixed for the life of a file -
    /// every offset already computed from them would be wrong - so a header
    /// that changes either is refused rather than written.
    pub fn set_header(&mut self, header: DatabaseHeader) -> DbResult<()> {
        self.require_writer()?;
        if header.page_size != self.header.page_size
            || header.reserved_bytes != self.header.reserved_bytes
        {
            return Err(misuse(
                "the page size and the reserved tail cannot change once a file exists",
            ));
        }
        let page_one = PageId::from_persisted(1)?;
        self.edit_page(page_one, |raw| {
            header.encode(bytes::window_mut(raw, 0, HEADER_SIZE)?)
        })?;
        self.header = header;
        Ok(())
    }

    /// Commits the transaction, putting every modified page in the file.
    ///
    /// The order is the one the TDD's commit sequence lists, and each step is
    /// there because a crash between it and the next one has to leave a
    /// recoverable database:
    ///
    /// 1. stamp the header, which journals page one like any other page;
    /// 2. make the journal durable - after this the old database is
    ///    reconstructible from the file plus the journal;
    /// 3. take EXCLUSIVE, so no reader sees the mixture that follows;
    /// 4. write every dirty page and truncate;
    /// 5. sync the database - after this the new database is complete;
    /// 6. make the journal non-hot, which is the atomic commit point;
    /// 7. publish: mark frames clean and release the locks.
    ///
    /// A crash before step 6 finds a hot journal and rolls the database back.
    /// A crash after it finds none and keeps the new database. There is no
    /// window in between, because step 6 is a single file operation.
    pub fn commit(&mut self) -> DbResult<()> {
        if self.commit_phase_one()? == CommitPhase::Nothing {
            return Ok(());
        }
        self.commit_phase_two()
    }

    /// Everything up to and including making the new database durable.
    ///
    /// After this the file holds the transaction and the journal holds what it
    /// replaced, both synced. A crash here finds a hot journal and undoes the
    /// transaction; the step that makes it *not* hot is phase two, and for a
    /// commit across several databases that step happens after every one of
    /// them has finished phase one.
    ///
    /// It reports whether there was anything to do, so a caller driving the two
    /// phases by hand knows whether the second is still owed.
    pub fn commit_phase_one(&mut self) -> DbResult<CommitPhase> {
        self.check_usable()?;
        self.require_writer()?;
        if self.wal.is_some() {
            self.commit_to_wal()?;
            return Ok(CommitPhase::Nothing);
        }
        if self.dirty.is_empty() && self.journalled.is_empty() {
            if let Some(journal) = self.journal.as_mut() {
                let outcome = journal.discard();
                self.collect_journal_stats();
                outcome?;
            }
            self.finish_transaction()?;
            return Ok(CommitPhase::Nothing);
        }
        self.reach_failpoint(FailSite::Commit)?;
        let mut header = self.header;
        header.change_counter = header.change_counter.wrapping_add(1);
        header.database_size = self.page_count;
        header.version_valid_for = header.change_counter;
        header.write_library_version = WRITE_LIBRARY_VERSION;
        self.set_header(header)?;

        if let Some(journal) = self.journal.as_mut() {
            if let Err(error) = journal.prepare_commit() {
                return Err(self.fail(error));
            }
        }

        if let Err(error) = self.lock_exclusive_for_commit() {
            if error.code() == inillucent_base::PrimaryCode::Busy {
                return Err(error);
            }
            return Err(self.fail(error));
        }
        self.state = PagerState::WriterDbMod;

        let pages: Vec<u32> = self.dirty.iter().copied().collect();
        // Page one carries the change counter, so it is written last: a reader
        // that sees the new counter has, on an ordered device, already been
        // able to see the pages it describes. The journal is what makes this
        // safe rather than merely likely, but the ordering costs nothing.
        for page in pages.iter().copied().filter(|page| *page != 1) {
            self.wrote_database = true;
            if let Err(error) = self.write_page_to_file(page) {
                return Err(self.fail(error));
            }
        }
        if pages.contains(&1) {
            self.wrote_database = true;
            if let Err(error) = self.write_page_to_file(1) {
                return Err(self.fail(error));
            }
        }
        let wanted = u64::from(self.page_count).saturating_mul(u64::from(self.page_size().bytes()));
        if self.file_bytes > wanted {
            if let Err(error) = self.file.truncate(wanted) {
                return Err(self.fail(error.into_db_error()));
            }
            self.file_bytes = wanted;
        }
        let database_sync = match self.journal.as_ref() {
            Some(journal) => journal.database_sync(),
            None => Some(SyncMode::Normal),
        };
        if let Some(mode) = database_sync {
            if let Err(error) = self.file.sync(mode) {
                return Err(self.fail(error.into_db_error()));
            }
        }
        self.committing = pages;
        Ok(CommitPhase::Owed)
    }

    /// Takes the EXCLUSIVE lock a commit writes under, waiting for readers.
    ///
    /// This is the one lock in the engine that has to be waited for here
    /// rather than by the caller. Every reader has to have left before a page
    /// can be written over, and the first refusal leaves this connection
    /// holding PENDING - which is what stops new readers arriving, so the wait
    /// is bounded by the readers that were already there rather than by
    /// whoever turns up next. A caller that gave up and started the
    /// transaction again would write and sync its journal a second time for
    /// nothing.
    ///
    /// With no timeout set this is one attempt and a `SQLITE_BUSY`, which is
    /// SQLite's default and its documented behaviour for a commit that meets a
    /// reader.
    fn lock_exclusive_for_commit(&mut self) -> DbResult<()> {
        let started = std::time::Instant::now();
        loop {
            let outcome = self.file.lock(FileLock::Exclusive);
            let Err(error) = outcome else {
                return Ok(());
            };
            let error = error.into_db_error();
            if error.code() != inillucent_base::PrimaryCode::Busy
                || started.elapsed() >= self.busy_timeout
            {
                return Err(error);
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
    }

    /// The step that makes the journal non-hot, which is the commit point.
    ///
    /// For one database this is the commit. For several it is the cleanup
    /// after one: the super-journal's deletion has already decided the
    /// outcome, and what is left here is to stop each journal claiming a
    /// transaction that is over.
    pub fn commit_phase_two(&mut self) -> DbResult<()> {
        if let Some(journal) = self.journal.as_mut() {
            let outcome = journal.commit_point();
            self.collect_journal_stats();
            if let Err(error) = outcome {
                return Err(self.fail(error));
            }
        }
        for page in core::mem::take(&mut self.committing) {
            if let Ok(page_id) = PageId::from_persisted(page) {
                self.cache.mark_clean(PageKey {
                    database: self.database,
                    page: page_id,
                });
            }
        }
        self.counters.commits = self.counters.commits.saturating_add(1);
        self.finish_transaction()
    }

    /// Tells the journal which super-journal decides its transaction.
    ///
    /// A journal that names one is replayed only while that file exists, so
    /// this is what makes several databases commit or roll back together. It
    /// is set before phase one, because the name has to be in the journal
    /// before the journal is synced.
    pub fn set_super_journal(&mut self, path: Option<DbPath>) {
        if let Some(journal) = self.journal.as_mut() {
            journal.set_super_journal(path);
        }
    }

    /// Returns the path of the journal this pager writes, when it has one.
    pub fn journal_path(&self) -> Option<DbPath> {
        self.journal.as_ref().and_then(|journal| journal.path())
    }

    /// Undoes the whole transaction and releases the writer's locks.
    ///
    /// Two rollbacks are possible and they are not interchangeable. Before the
    /// commit reached the file, the undo images in memory are the whole story
    /// and putting them back is enough. After it reached the file, the file
    /// itself holds a mixture, and only the journal can undo that - so the
    /// journal is replayed onto the file, the file is truncated back to the
    /// size it had, and the cache is emptied of everything the transaction
    /// touched rather than being trusted.
    pub fn rollback(&mut self) -> DbResult<()> {
        if !self.is_writing() && self.undo.is_empty() {
            return Ok(());
        }
        if self.wal.is_some() {
            return self.rollback_from_wal();
        }
        if self.wrote_database {
            return self.rollback_from_journal();
        }
        while let Some(frame) = self.undo.pop() {
            self.restore(frame)?;
        }
        self.dirty.clear();
        if let Some(journal) = self.journal.as_mut() {
            let outcome = journal.discard();
            self.collect_journal_stats();
            outcome?;
        }
        self.counters.rollbacks = self.counters.rollbacks.saturating_add(1);
        self.clear_recoverable_error();
        self.finish_transaction()
    }

    /// Puts the database file back the way the journal says it was.
    fn rollback_from_journal(&mut self) -> DbResult<()> {
        let Some(mut journal) = self.journal.take() else {
            return Err(self.fail(corrupt(
                "the database file was modified with no journal to undo it",
            )));
        };
        let outcome = journal.playback(self.file.as_ref());
        let original = match outcome {
            Ok(original) => original,
            Err(error) => {
                self.journal = Some(journal);
                return Err(self.fail(error));
            }
        };
        if let Some(pages) = original {
            let wanted = u64::from(pages).saturating_mul(u64::from(self.header.page_size.bytes()));
            if let Err(error) = self.file.truncate(wanted) {
                self.journal = Some(journal);
                return Err(self.fail(error.into_db_error()));
            }
            self.file_bytes = wanted;
            self.page_count = pages;
            if let Err(error) = self.file.sync(SyncMode::Normal) {
                self.journal = Some(journal);
                return Err(self.fail(error.into_db_error()));
            }
        }
        let discarded = journal.discard();
        self.journal_totals.add(journal.stats());
        self.journal = Some(journal);
        discarded?;
        // Everything this transaction touched is now wrong in the cache, and
        // the truth is in the file. Dropping the frames is cheaper than being
        // clever about which of them survived, and it cannot be wrong.
        // Page zero does not exist, so "above zero" is every page.
        self.cache.discard_above(self.database, 0);
        self.dirty.clear();
        self.undo.clear();
        self.journalled.clear();
        self.wrote_database = false;
        self.reload_header()?;
        self.counters.rollbacks = self.counters.rollbacks.saturating_add(1);
        self.clear_recoverable_error();
        self.finish_transaction()
    }

    /// Forgets a sticky error the rollback has just repaired.
    ///
    /// Stickiness exists so that a query which hit an unreadable page cannot
    /// return a partial answer that looks complete. Once the transaction has
    /// been rolled back there is no partial state left to protect anyone from,
    /// and a pager that kept refusing would make one out-of-memory statement
    /// end the connection.
    ///
    /// Corruption is the exception and is deliberately not cleared. A page
    /// that does not decode is still there after the rollback, and forgetting
    /// that would turn "this database is damaged" into an intermittent error
    /// that goes away when the caller retries.
    fn clear_recoverable_error(&mut self) {
        let recoverable = self.sticky.as_ref().is_some_and(|error| {
            !matches!(
                error.code(),
                inillucent_base::PrimaryCode::Corrupt | inillucent_base::PrimaryCode::NotADb
            )
        });
        if recoverable {
            self.sticky = None;
            if self.state == PagerState::Error {
                self.state = PagerState::Reader;
            }
        }
    }

    /// Re-reads the header from the file, after a playback replaced page one.
    fn reload_header(&mut self) -> DbResult<()> {
        let mut prefix = [0u8; HEADER_SIZE];
        self.file.read_exact_at(0, &mut prefix)?;
        let header = DatabaseHeader::decode(&prefix)?;
        self.file_bytes = self.file.file_size()?;
        self.page_count = header.database_size;
        self.header = header;
        Ok(())
    }

    /// Folds a finished journal's numbers into the pager's running totals.
    fn collect_journal_stats(&mut self) {
        let stats = self.journal.as_ref().map(|journal| journal.stats());
        if let Some(stats) = stats {
            self.journal_totals.add(stats);
        }
    }

    /// Pushes a new undo level.
    fn push_undo_level(&mut self, name: Option<String>) -> DbResult<()> {
        self.require_writer()?;
        self.undo.push(UndoFrame {
            name,
            images: BTreeMap::new(),
            page_count: self.page_count,
            header: self.header,
        });
        Ok(())
    }

    /// Returns the depth a named savepoint sits at, counting the transaction
    /// level as depth one.
    fn find_savepoint(&self, name: &str) -> DbResult<usize> {
        self.undo
            .iter()
            .rposition(|frame| frame.name.as_deref() == Some(name))
            .map(|index| index.saturating_add(1))
            .ok_or_else(|| misuse(format!("there is no savepoint called {name}")))
    }

    /// Puts every page an undo level recorded back the way it was.
    fn restore(&mut self, frame: UndoFrame) -> DbResult<()> {
        // A page is dirty exactly when some *remaining* level holds an image
        // for it: that is what "was written since this level opened" means, and
        // deriving the dirty set rather than tracking it means it cannot drift
        // out of step with the images that would undo it.
        let remaining: BTreeSet<u32> = self
            .undo
            .iter()
            .flat_map(|level| level.images.keys().copied())
            .collect();
        for (page, image) in frame.images {
            let Ok(page_id) = PageId::from_persisted(page) else {
                continue;
            };
            let key = PageKey {
                database: self.database,
                page: page_id,
            };
            match image {
                Some(bytes) => {
                    let state = if remaining.contains(&page) {
                        PageState::Dirty {
                            before_image_saved: true,
                        }
                    } else {
                        PageState::Clean
                    };
                    self.cache.publish(key, bytes, state)?;
                }
                None => self.cache.discard(key),
            }
        }
        self.page_count = frame.page_count;
        self.header = frame.header;
        self.cache.discard_above(self.database, self.page_count);
        self.dirty = remaining
            .into_iter()
            .filter(|page| *page <= self.page_count)
            .collect();
        Ok(())
    }

    /// Records a page's current contents in the innermost undo level.
    fn record_image(&mut self, page: PageId) -> DbResult<()> {
        self.journal_original(page)?;
        let Some(level) = self.undo.last() else {
            return Err(misuse("a page was edited with no transaction open"));
        };
        if level.images.contains_key(&page.get()) {
            return Ok(());
        }
        let existed = page.get() <= level.page_count;
        let image = if existed {
            let pin = self.get_page(page)?;
            let bytes = self.copy_bytes(pin.bytes())?;
            Some(bytes)
        } else {
            None
        };
        let Some(level) = self.undo.last_mut() else {
            return Err(misuse("a page was edited with no transaction open"));
        };
        level.images.insert(page.get(), image);
        Ok(())
    }

    /// Writes a page's pre-transaction image to the journal, exactly once.
    ///
    /// It runs before the page is modified, so what the cache holds now is
    /// what a rollback has to put back - and because it runs before *any*
    /// modification, the first call for a page is the only one that sees the
    /// pre-transaction bytes, which is why the set is consulted rather than
    /// the undo level. An undo level records the page as it was when that
    /// level opened, which for a nested statement is already a modified page.
    ///
    /// A page the transaction created has no image worth keeping: the journal
    /// records the original page count and recovery truncates back to it.
    fn journal_original(&mut self, page: PageId) -> DbResult<()> {
        if self.journal.is_none() {
            return Ok(());
        }
        let original = self
            .undo
            .first()
            .map_or(self.page_count, |base| base.page_count);
        if page.get() > original || self.journalled.contains(&page.get()) {
            return Ok(());
        }
        // Inserted before the write, so a failure part-way cannot leave the
        // page marked as journalled when it is not.
        let image = {
            let pin = self.get_page(page)?;
            self.copy_bytes(pin.bytes())?
        };
        let number = page.get();
        let Some(journal) = self.journal.as_mut() else {
            return Ok(());
        };
        journal.record(number, image.as_slice())?;
        self.journalled.insert(number);
        Ok(())
    }

    /// Copies a page's bytes into a fresh buffer, counting the copy.
    fn copy_bytes(&mut self, source: &[u8]) -> DbResult<PageBuffer> {
        self.counters.page_images = self.counters.page_images.saturating_add(1);
        PageBuffer::from_bytes(self.header.page_size, buffer::try_copy_of(source)?)
    }

    /// Writes one page's current contents to the file.
    fn write_page_to_file(&mut self, page: u32) -> DbResult<()> {
        let page_id = PageId::from_persisted(page)?;
        let key = PageKey {
            database: self.database,
            page: page_id,
        };
        let Some(pin) = self.cache.get(key) else {
            return Err(misuse(format!(
                "page {page} is dirty but is not resident, so its change was lost"
            )));
        };
        let offset = page::page_offset(self.header.page_size, page_id)?;
        self.file.write_all_at(offset, pin.bytes())?;
        let end = offset.saturating_add(u64::from(self.header.page_size.bytes()));
        self.file_bytes = self.file_bytes.max(end);
        self.counters.page_writes = self.counters.page_writes.saturating_add(1);
        self.counters.bytes_written = self
            .counters
            .bytes_written
            .saturating_add(u64::from(self.header.page_size.bytes()));
        Ok(())
    }

    /// Drops the writer's locks and returns to the reader state.
    fn finish_transaction(&mut self) -> DbResult<()> {
        self.dirty.clear();
        self.undo.clear();
        self.freed.clear();
        self.journalled.clear();
        self.wrote_database = false;
        if self.file.lock_level() > FileLock::Shared {
            self.file.unlock(FileLock::Shared)?;
        }
        if self.is_writing() {
            self.state = PagerState::Reader;
        }
        Ok(())
    }

    /// Arms the failpoint counter to fail the `count`th site it reaches.
    ///
    /// `site` narrows the campaign to one kind of operation; `None` counts them
    /// all, which is what enumerates a whole execution.
    pub fn fail_after(&mut self, count: u64, site: Option<FailSite>) {
        self.sites_reached = 0;
        self.fail_at = Some((count, site));
    }

    /// Disarms the failpoint counter.
    pub fn clear_failpoint(&mut self) {
        self.fail_at = None;
    }

    /// Returns how many failpoint sites have been reached since it was armed.
    pub fn sites_reached(&self) -> u64 {
        self.sites_reached
    }

    /// Counts a failpoint site, and fails there when the campaign says to.
    ///
    /// The error is an ordinary I/O error rather than a special one, because
    /// the point of the campaign is to find out what the ordinary error paths
    /// do: a failure the code could recognise as injected would be a failure
    /// the code could treat differently.
    pub fn reach_failpoint(&mut self, site: FailSite) -> DbResult<()> {
        let Some((count, wanted)) = self.fail_at else {
            return Ok(());
        };
        if wanted.is_some_and(|only| only != site) {
            return Ok(());
        }
        self.sites_reached = self.sites_reached.saturating_add(1);
        if self.sites_reached == count {
            return Err(DbError::primary(inillucent_base::PrimaryCode::IoErr)
                .with_detail(format!("an injected storage failure at {site:?}")));
        }
        Ok(())
    }

    /// Records that a page has been returned to the freelist, refusing a page
    /// that this transaction has already freed.
    ///
    /// Freeing a page twice puts it on the freelist twice, and the second
    /// allocation of it hands the same page to two owners - which is the worst
    /// kind of corruption, because both owners write plausible bytes and only a
    /// traversal months later notices. The set is per transaction because that
    /// is the window in which the mistake is a *bug*; a page that was already
    /// free when the transaction started is caught by the pointer map when
    /// there is one, and by the integrity check when there is not.
    pub fn record_freed(&mut self, page: PageId) -> DbResult<()> {
        if !self.freed.insert(page.get()) {
            return Err(corrupt(format!(
                "page {} was freed twice in one transaction",
                page.get()
            )));
        }
        Ok(())
    }

    /// Records that a page has been taken for a new use.
    pub fn record_allocated(&mut self, page: PageId) {
        self.freed.remove(&page.get());
    }

    /// Counts one page allocation.
    pub fn count_allocation(&mut self) {
        self.counters.pages_allocated = self.counters.pages_allocated.saturating_add(1);
    }

    /// Counts one page returned to the freelist.
    pub fn count_free(&mut self) {
        self.counters.pages_freed = self.counters.pages_freed.saturating_add(1);
    }

    /// Attaches a write-ahead log, which changes how pages are read as well
    /// as how they are written.
    ///
    /// A pager in WAL mode takes no RESERVED or EXCLUSIVE lock on the database
    /// file and never writes a page into it outside a checkpoint. That is why
    /// the mode exists: the file a reader is reading is not the file the
    /// writer is writing, so neither has to wait for the other.
    pub fn attach_wal(&mut self, wal: Box<dyn WriteAheadLog>) {
        self.wal = Some(wal);
    }

    /// Returns the snapshot the connection is reading, when there is one.
    pub fn wal_snapshot(&self) -> Option<WalSnapshot> {
        self.wal_snapshot
    }

    /// Copies frames from the log into the database file.
    ///
    /// The log is taken out for the duration so that it and the database file
    /// are visibly two different things; a checkpoint writes one from the
    /// other, and holding both through one borrow would say they were the
    /// same.
    pub fn checkpoint(&mut self, mode: CheckpointMode) -> DbResult<CheckpointOutcome> {
        self.checkpoint_within(mode, None)
    }

    /// Checkpoints, copying at most `budget` frames when one is given.
    /// @param mode - which checkpoint to run
    /// @param budget - the most frames to copy, or `None` for all of them
    pub fn checkpoint_within(
        &mut self,
        mode: CheckpointMode,
        budget: Option<u32>,
    ) -> DbResult<CheckpointOutcome> {
        let Some(mut wal) = self.wal.take() else {
            return Err(misuse(
                "a checkpoint was asked for on a database with no write-ahead log",
            ));
        };
        let outcome = wal.checkpoint(mode, self.file.as_ref(), budget);
        self.wal = Some(wal);
        let outcome = outcome?;
        self.file_bytes = self.file.file_size()?;
        Ok(outcome)
    }

    /// Takes a snapshot of the log and enters the reader state.
    ///
    /// The cache is emptied when the snapshot moved, because a frame in it was
    /// read against the old one. Keeping the pages and checking them one at a
    /// time would be cheaper and is what a later phase can do; getting it
    /// wrong serves a page from a database state that no longer exists, which
    /// is the one failure a snapshot exists to prevent.
    fn begin_wal_read(&mut self) -> DbResult<()> {
        let Some(wal) = self.wal.as_mut() else {
            return Err(misuse("a WAL read was begun with no log attached"));
        };
        let snapshot = match wal.begin_read() {
            Ok(snapshot) => snapshot,
            Err(error) => {
                if error.code() == inillucent_base::PrimaryCode::Busy {
                    return Err(error);
                }
                return Err(self.fail(error));
            }
        };
        if self.wal_snapshot != Some(snapshot) {
            self.cache.discard_above(self.database, 0);
        }
        self.wal_snapshot = Some(snapshot);
        self.state = PagerState::Reader;
        self.file_bytes = self.file.file_size()?;
        if snapshot.is_database_only() {
            let reloaded = self.reload_header();
            if let Err(error) = reloaded {
                return Err(self.fail(error));
            }
        } else {
            self.page_count = snapshot.page_count;
            if let Err(error) = self.reload_header_from_snapshot() {
                return Err(self.fail(error));
            }
        }
        Ok(())
    }

    /// Re-reads the header from page one as the snapshot sees it.
    ///
    /// Page one is a page like any other and may be in the log, so the header
    /// a reader works from has to come through the same lookup as everything
    /// else. Reading it from the file's first hundred bytes would describe the
    /// last checkpoint rather than the snapshot.
    fn reload_header_from_snapshot(&mut self) -> DbResult<()> {
        let page_one = PageId::from_persisted(1)?;
        let pin = self.get_page(page_one)?;
        let header = DatabaseHeader::decode(bytes::window(pin.bytes(), 0, HEADER_SIZE)?)?;
        drop(pin);
        self.header = header;
        Ok(())
    }

    /// Reads a page from the log when the snapshot has a frame for it, and
    /// from the database file when it does not.
    fn read_page_through_wal(&mut self, key: PageKey, page: PageId) -> DbResult<PagePin> {
        let Some(wal) = self.wal.as_mut() else {
            return Err(misuse("a WAL read was attempted with no log attached"));
        };
        let frame = wal.frame_for(page.get())?;
        let Some(frame) = frame else {
            return self.read_page_beneath_wal(key, page);
        };
        let mut buffer = PageBuffer::zeroed(self.header.page_size)?;
        let Some(wal) = self.wal.as_mut() else {
            return Err(misuse("a WAL read was attempted with no log attached"));
        };
        wal.read_frame(frame, buffer.as_mut_slice())?;
        self.counters.page_reads = self.counters.page_reads.saturating_add(1);
        self.counters.bytes_read = self
            .counters
            .bytes_read
            .saturating_add(u64::from(self.header.page_size.bytes()));
        self.cache.insert(key, buffer)
    }

    /// Reads a page the log does not hold out of the database file.
    ///
    /// A page inside the snapshot that the file is too short for reads as
    /// zeroes rather than as corruption. In WAL mode the log's last commit is
    /// what says how many pages the database has, and a transaction that grew
    /// it without writing every new page is an ordinary thing for the
    /// allocator to do.
    fn read_page_beneath_wal(&mut self, key: PageKey, page: PageId) -> DbResult<PagePin> {
        let size = self.header.page_size;
        let mut buffer = PageBuffer::zeroed(size)?;
        let offset = page::page_offset(size, page)?;
        let end = offset.saturating_add(u64::from(size.bytes()));
        if end > self.file_bytes {
            return self.cache.insert(key, buffer);
        }
        self.file.read_exact_at(offset, buffer.as_mut_slice())?;
        self.counters.page_reads = self.counters.page_reads.saturating_add(1);
        self.counters.bytes_read = self
            .counters
            .bytes_read
            .saturating_add(u64::from(size.bytes()));
        self.cache.insert(key, buffer)
    }

    /// Commits by appending the transaction's pages to the log.
    ///
    /// There is no lock ladder and no database write here. The frames are
    /// appended, made durable if the durability level asks for it, and then
    /// published in one write of the log index header - and that publication
    /// is the commit point. A crash before it leaves frames that no reader can
    /// reach and that the next writer overwrites; a crash after it leaves a
    /// transaction every reader can see.
    fn commit_to_wal(&mut self) -> DbResult<()> {
        if self.dirty.is_empty() {
            return self.finish_wal_transaction();
        }
        self.reach_failpoint(FailSite::Commit)?;
        let mut header = self.header;
        header.change_counter = header.change_counter.wrapping_add(1);
        header.database_size = self.page_count;
        header.version_valid_for = header.change_counter;
        header.write_library_version = WRITE_LIBRARY_VERSION;
        self.set_header(header)?;
        let pages: Vec<u32> = self.dirty.iter().copied().collect();
        let Some(last) = pages.last().copied() else {
            return self.finish_wal_transaction();
        };
        let Some(mut wal) = self.wal.take() else {
            return Err(misuse("a WAL commit was attempted with no log attached"));
        };
        let outcome = self.append_and_publish(wal.as_mut(), &pages, last);
        self.wal = Some(wal);
        if let Err(error) = outcome {
            return Err(self.fail(error));
        }
        for page in pages {
            if let Ok(page_id) = PageId::from_persisted(page) {
                self.cache.mark_clean(PageKey {
                    database: self.database,
                    page: page_id,
                });
            }
        }
        self.counters.commits = self.counters.commits.saturating_add(1);
        self.finish_wal_transaction()
    }

    /// Appends every page of the transaction and publishes the commit.
    fn append_and_publish(
        &mut self,
        wal: &mut dyn WriteAheadLog,
        pages: &[u32],
        last: u32,
    ) -> DbResult<()> {
        for page in pages.iter().copied() {
            let image = self.page_image(page)?;
            let commit = if page == last { self.page_count } else { 0 };
            wal.append(page, &image, commit)?;
        }
        wal.publish_commit(self.page_count)?;
        Ok(())
    }

    /// Returns a copy of a page's current bytes, for the log to hold.
    fn page_image(&self, page: u32) -> DbResult<Vec<u8>> {
        let page_id = PageId::from_persisted(page)?;
        let key = PageKey {
            database: self.database,
            page: page_id,
        };
        let Some(pin) = self.cache.get(key) else {
            return Err(misuse(format!(
                "page {page} is dirty but is not resident, so its change was lost"
            )));
        };
        Ok(pin.bytes().to_vec())
    }

    /// Undoes a transaction that never reached the log's published header.
    ///
    /// Nothing on disk has to be repaired: the frames the transaction appended
    /// were never published, so no reader could see them and the next writer
    /// overwrites them. What has to be undone is the index, which is shared,
    /// and the pages in this connection's own cache.
    fn rollback_from_wal(&mut self) -> DbResult<()> {
        while let Some(frame) = self.undo.pop() {
            self.restore(frame)?;
        }
        self.dirty.clear();
        if let Some(wal) = self.wal.as_mut() {
            wal.undo()?;
        }
        self.counters.rollbacks = self.counters.rollbacks.saturating_add(1);
        self.clear_recoverable_error();
        self.finish_wal_transaction()
    }

    /// Releases the log's write slot and returns to the reader state.
    ///
    /// The automatic checkpoint runs after the write slot is released, not
    /// before: a checkpoint is a long operation and holding the one thing
    /// every other writer needs while it runs would turn WAL mode's single
    /// writer into a queue behind the slowest connection.
    fn finish_wal_transaction(&mut self) -> DbResult<()> {
        self.dirty.clear();
        self.undo.clear();
        self.freed.clear();
        self.journalled.clear();
        self.wrote_database = false;
        if self.is_writing() {
            self.state = PagerState::Reader;
        }
        let released = match self.wal.as_mut() {
            Some(wal) => wal.end_write(),
            None => Ok(()),
        };
        released?;
        self.run_automatic_checkpoint()
    }

    /// Checkpoints when the log has grown past the configured threshold.
    ///
    /// A little at a time rather than all at once. Copying the whole log makes
    /// one commit in every thousand pay for the other nine hundred and
    /// ninety-nine - the throughput is the same and the worst commit is a
    /// hundred times the median, which for an application is the number that
    /// shows. The backfill point lives in the shared index, so a bounded copy
    /// is not a partial job to be finished later: it is the job, resumed by
    /// whichever commit comes next.
    fn run_automatic_checkpoint(&mut self) -> DbResult<()> {
        let threshold = self.wal.as_ref().map_or(0, |wal| wal.auto_checkpoint());
        let frames = self.wal.as_ref().map_or(0, |wal| wal.frame_count());
        if threshold == 0 || frames < threshold {
            return Ok(());
        }
        // A checkpoint that cannot run is not a failure: it means a reader is
        // using the frames, and the next commit will try again.
        let _ = self.checkpoint_within(CheckpointMode::Passive, self.checkpoint_budget)?;
        Ok(())
    }

    /// Returns how many frames one automatic checkpoint copies.
    pub fn checkpoint_budget(&self) -> Option<u32> {
        self.checkpoint_budget
    }

    /// Sets how many frames one automatic checkpoint copies.
    ///
    /// `None` restores the all-at-once behaviour, which is what the arm that
    /// measures this uses.
    /// @param budget - the cap, or `None` for no cap
    pub fn set_checkpoint_budget(&mut self, budget: Option<u32>) {
        self.checkpoint_budget = budget;
    }

    /// Returns an error unless a write transaction is open.
    fn require_writer(&self) -> DbResult<()> {
        if self.read_only {
            return Err(misuse("a read-only pager cannot write"));
        }
        if !self.is_writing() {
            return Err(misuse(format!(
                "a write was attempted from the {:?} state",
                self.state
            )));
        }
        Ok(())
    }
}

impl Drop for Pager {
    /// Releases the lock even when a caller forgot to close, because a lock
    /// left behind blocks every writer on the machine until the process ends.
    ///
    /// Keyed on the file's own lock level rather than on the pager's state,
    /// for the reason `end_read` records: a pager that has failed is still a
    /// pager that holds a lock.
    fn drop(&mut self) {
        if let Some(mut wal) = self.wal.take() {
            let _ = wal.close(self.file.as_ref());
        }
        if self.file.lock_level() != FileLock::None {
            let _ = self.file.unlock(FileLock::None);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use inillucent_vfs::memory::MemoryVfs;

    /// Builds a minimal but valid database file in memory.
    fn build_database(pages: u32, page_size: u32) -> Vec<u8> {
        let size = PageSize::new(page_size).unwrap();
        let header = DatabaseHeader {
            page_size: size,
            write_version: 1,
            read_version: 1,
            reserved_bytes: 0,
            change_counter: 1,
            database_size: pages,
            freelist_head: 0,
            freelist_count: 0,
            schema_cookie: 0,
            schema_format: 4,
            cache_size: 0,
            largest_root: 0,
            text_encoding: TextEncoding::Utf8,
            user_version: 0,
            vacuum_mode: crate::header::VacuumMode::None,
            application_id: 0,
            reserved_expansion_is_zero: true,
            version_valid_for: 1,
            write_library_version: 3_053_004,
        };
        let mut file = vec![0u8; (pages as usize) * (page_size as usize)];
        header.encode(&mut file[..HEADER_SIZE]).unwrap();
        // Mark each page so a read can be told from a zero-filled buffer.
        for page in 1..=pages {
            let offset = (page as usize - 1) * page_size as usize;
            let marker = offset + if page == 1 { HEADER_SIZE } else { 0 };
            file[marker] = page as u8;
        }
        file
    }

    /// Writes a database into a memory VFS and returns the VFS and its path.
    fn hosted(file: &[u8]) -> (MemoryVfs, DbPath) {
        let vfs = MemoryVfs::new();
        let path = DbPath::new("main.db");
        let handle = vfs
            .open(
                &path,
                OpenOptions::of_kind(inillucent_vfs::FileKind::MainDb),
            )
            .unwrap();
        handle.write_all_at(0, file).unwrap();
        drop(handle);
        (vfs, path)
    }

    /// The pager must read the header, count the pages, and start unlocked.
    #[test]
    fn opening_reads_the_header_and_takes_no_lock() {
        let (vfs, path) = hosted(&build_database(5, 1024));
        let pager = Pager::open_read_only(&vfs, &path, PagerOptions::default()).unwrap();
        assert_eq!(pager.page_count(), 5);
        assert_eq!(pager.page_size().bytes(), 1024);
        assert_eq!(pager.text_encoding(), TextEncoding::Utf8);
        assert_eq!(pager.state(), PagerState::Open);
        assert_eq!(pager.lock_level(), FileLock::None);
    }

    /// Reading requires a read transaction, and taking one takes exactly a
    /// SHARED lock - never a stronger one.
    #[test]
    fn a_reader_takes_a_shared_lock_and_no_more() {
        let (vfs, path) = hosted(&build_database(3, 1024));
        let mut pager = Pager::open_read_only(&vfs, &path, PagerOptions::default()).unwrap();
        let error = pager.get_page(PageId::new(1).unwrap()).unwrap_err();
        assert_eq!(error.code(), inillucent_base::PrimaryCode::Misuse);

        pager.begin_read().unwrap();
        assert_eq!(pager.state(), PagerState::Reader);
        assert_eq!(pager.lock_level(), FileLock::Shared);
        // A second begin is a no-op rather than an error or a second lock.
        pager.begin_read().unwrap();
        assert_eq!(pager.lock_level(), FileLock::Shared);

        pager.end_read().unwrap();
        assert_eq!(pager.lock_level(), FileLock::None);
        assert_eq!(pager.state(), PagerState::Open);
    }

    /// A page read must return that page's bytes, and a second read of the
    /// same page must come from the cache rather than the file.
    #[test]
    fn a_second_read_of_a_page_comes_from_the_cache() {
        let (vfs, path) = hosted(&build_database(4, 1024));
        let mut pager = Pager::open_read_only(&vfs, &path, PagerOptions::default()).unwrap();
        pager.begin_read().unwrap();
        let first = pager.get_page(PageId::new(3).unwrap()).unwrap();
        assert_eq!(first.bytes()[0], 3);
        assert_eq!(pager.counters().page_reads, 1);
        let second = pager.get_page(PageId::new(3).unwrap()).unwrap();
        assert_eq!(second.bytes()[0], 3);
        assert_eq!(pager.counters().page_reads, 1);
        assert_eq!(pager.counters().cache_hits, 1);
    }

    /// A page number outside the database is refused rather than read, and
    /// page zero cannot even be constructed.
    #[test]
    fn a_page_outside_the_database_is_refused() {
        let (vfs, path) = hosted(&build_database(3, 1024));
        let mut pager = Pager::open_read_only(&vfs, &path, PagerOptions::default()).unwrap();
        pager.begin_read().unwrap();
        let error = pager.get_page(PageId::new(4).unwrap()).unwrap_err();
        assert_eq!(error.code(), inillucent_base::PrimaryCode::Corrupt);
        assert_eq!(pager.counters().out_of_range, 1);
        assert!(PageId::new(0).is_none());
        // The refusal is not sticky: a bad page number is a caller error, not
        // a broken file.
        assert!(pager.sticky_error().is_none());
        assert!(pager.get_page(PageId::new(1).unwrap()).is_ok());
    }

    /// A file whose header does not decode must be refused at open, before
    /// any page is read.
    #[test]
    fn a_file_with_a_bad_header_does_not_open() {
        let mut file = build_database(3, 1024);
        file[0] = b'X';
        let (vfs, path) = hosted(&file);
        assert!(Pager::open_read_only(&vfs, &path, PagerOptions::default()).is_err());

        let (vfs, path) = hosted(&[0u8; 50]);
        assert!(Pager::open_read_only(&vfs, &path, PagerOptions::default()).is_err());
    }

    /// A truncated file must fail on the page that is missing, and then stay
    /// failed: a query that hit it must not go on to return a partial answer
    /// that looks complete.
    #[test]
    fn an_io_failure_is_sticky() {
        // The header claims five pages but the file holds three, and the
        // version-valid-for match makes the header authoritative.
        let mut file = build_database(5, 1024);
        file.truncate(3 * 1024);
        let (vfs, path) = hosted(&file);
        let mut pager = Pager::open_read_only(&vfs, &path, PagerOptions::default()).unwrap();
        assert_eq!(pager.page_count(), 5);
        pager.begin_read().unwrap();
        assert!(pager.get_page(PageId::new(2).unwrap()).is_ok());
        let error = pager.get_page(PageId::new(5).unwrap()).unwrap_err();
        assert_eq!(error.code(), inillucent_base::PrimaryCode::Corrupt);
        assert_eq!(pager.state(), PagerState::Error);
        // Every later call returns the same error, including one that would
        // otherwise have succeeded.
        let again = pager.get_page(PageId::new(2).unwrap()).unwrap_err();
        assert_eq!(again.code(), inillucent_base::PrimaryCode::Corrupt);
        assert!(pager.sticky_error().is_some());

        // A failed pager still holds its lock, and ending the read must still
        // release it. Keying this on the pager's state rather than on the
        // file's lock level left the lock held for the life of the process.
        assert_eq!(pager.lock_level(), FileLock::Shared);
        pager.end_read().unwrap();
        assert_eq!(pager.lock_level(), FileLock::None);
        assert_eq!(pager.state(), PagerState::Error);

        pager.begin_read().unwrap_err();
        pager.clear_sticky_error();
        pager.begin_read().unwrap();
        assert!(pager.get_page(PageId::new(2).unwrap()).is_ok());
    }

    /// Ending a read must return every page the reader was holding, so the
    /// next reader starts from a clean cache rather than a leaked one.
    #[test]
    fn ending_a_read_releases_every_unpinned_page() {
        let (vfs, path) = hosted(&build_database(20, 1024));
        let mut pager = Pager::open_read_only(&vfs, &path, PagerOptions::default()).unwrap();
        pager.begin_read().unwrap();
        for number in 1..=20u32 {
            drop(pager.get_page(PageId::new(number).unwrap()).unwrap());
        }
        assert_eq!(pager.cache_counters().resident, 20);
        pager.end_read().unwrap();
        assert_eq!(pager.cache_counters().resident, 0);
        assert_eq!(pager.cache().pinned_frames(), 0);
    }

    /// Dropping a pager without closing it must still release the lock, or a
    /// forgotten pager would block every writer until the process exits.
    #[test]
    fn dropping_a_pager_releases_its_lock() {
        let (vfs, path) = hosted(&build_database(2, 1024));
        {
            let mut pager = Pager::open_read_only(&vfs, &path, PagerOptions::default()).unwrap();
            pager.begin_read().unwrap();
            assert_eq!(pager.lock_level(), FileLock::Shared);
        }
        // A second pager can take an exclusive lock only if the first let go.
        let handle = vfs
            .open(
                &path,
                OpenOptions::of_kind(inillucent_vfs::FileKind::MainDb),
            )
            .unwrap();
        handle.lock(FileLock::Shared).unwrap();
        handle.lock(FileLock::Reserved).unwrap();
        handle.lock(FileLock::Exclusive).unwrap();
    }

    /// The state machine's legal moves are asserted directly, so a later
    /// phase adding writer states cannot quietly change what a reader may do.
    #[test]
    fn the_read_only_state_machine_allows_exactly_four_states() {
        let readable: Vec<PagerState> = [
            PagerState::Closed,
            PagerState::Open,
            PagerState::Reader,
            PagerState::WriterLocked,
            PagerState::WriterCacheMod,
            PagerState::WriterDbMod,
            PagerState::Error,
        ]
        .into_iter()
        .filter(|state| state.can_read())
        .collect();
        assert_eq!(
            readable,
            vec![
                PagerState::Reader,
                PagerState::WriterLocked,
                PagerState::WriterCacheMod,
                PagerState::WriterDbMod
            ]
        );
        let read_only: Vec<PagerState> = [
            PagerState::Closed,
            PagerState::Open,
            PagerState::Reader,
            PagerState::WriterLocked,
            PagerState::WriterCacheMod,
            PagerState::WriterDbMod,
            PagerState::Error,
        ]
        .into_iter()
        .filter(|state| state.is_read_only_state())
        .collect();
        assert_eq!(
            read_only,
            vec![
                PagerState::Closed,
                PagerState::Open,
                PagerState::Reader,
                PagerState::Error
            ]
        );
    }

    /// Two pagers over the same file must be able to read at once, which is
    /// what SHARED locking is for.
    #[test]
    fn two_readers_share_the_file() {
        let (vfs, path) = hosted(&build_database(3, 1024));
        let mut first = Pager::open_read_only(&vfs, &path, PagerOptions::default()).unwrap();
        let mut second = Pager::open_read_only(&vfs, &path, PagerOptions::default()).unwrap();
        first.begin_read().unwrap();
        second.begin_read().unwrap();
        assert_eq!(first.lock_level(), FileLock::Shared);
        assert_eq!(second.lock_level(), FileLock::Shared);
        assert_eq!(
            first.get_page(PageId::new(2).unwrap()).unwrap().bytes()[0],
            2
        );
        assert_eq!(
            second.get_page(PageId::new(2).unwrap()).unwrap().bytes()[0],
            2
        );
    }
}
