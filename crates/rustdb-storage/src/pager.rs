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

use rustdb_base::buffer::{self, PageBuffer};
use rustdb_base::bytes;
use rustdb_base::error::{corrupt, misuse};
use rustdb_base::ids::{DatabaseId, PageId};
use rustdb_base::page::{self, PageSize};
use rustdb_base::{DbError, DbResult};
use rustdb_value::TextEncoding;
use rustdb_vfs::{DbPath, FileLock, OpenOptions, SyncMode, Vfs, VfsFile};

use crate::btree::PageKind;
use crate::cache::{CacheCounters, PageCache, PageKey, PagePin, PageState};
use crate::header::{DatabaseHeader, VacuumMode, HEADER_SIZE};

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

/// The library version number rust-db stamps into a header it writes.
///
/// SQLite writes `SQLITE_VERSION_NUMBER` here, and the field is informational:
/// nothing reads it back to decide behaviour. rust-db writes the number of the
/// release whose file format it implements, which is the pinned reference,
/// because that is the true statement about the bytes in the file. It is not a
/// claim to be that build, and no rust-db code reads this field.
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
}

impl Default for PagerOptions {
    /// A two-megabyte cache on the main database, which is roughly SQLite's
    /// own default of 2000 pages of 1 KiB.
    fn default() -> PagerOptions {
        PagerOptions {
            cache_bytes: 2 * 1024 * 1024,
            database: DatabaseId(0),
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
            OpenOptions::of_kind(rustdb_vfs::FileKind::MainDb).read_only(),
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
        if let Err(error) = self.file.lock(FileLock::Shared) {
            // A busy file is not a corrupt one, so the pager stays usable and
            // the caller may retry; only an I/O failure is sticky.
            let error = error.into_db_error();
            if error.code() == rustdb_base::PrimaryCode::Busy {
                return Err(error);
            }
            return Err(self.fail(error));
        }
        self.state = PagerState::Reader;
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
        if self.file.lock_level() != FileLock::None {
            self.file.unlock(FileLock::None)?;
        }
        if self.state == PagerState::Reader {
            self.state = PagerState::Open;
        }
        Ok(())
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
        if self.file.lock_level() != FileLock::None {
            let _ = self.file.unlock(FileLock::None);
        }
        self.cache.release_unpinned();
        self.state = PagerState::Closed;
        Ok(())
    }
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
        pager.file = vfs.open(path, OpenOptions::of_kind(rustdb_vfs::FileKind::MainDb))?;
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
        let file = vfs.open(path, OpenOptions::of_kind(rustdb_vfs::FileKind::MainDb))?;
        file.write_all_at(0, page.as_slice())?;
        file.sync(SyncMode::Normal)?;
        drop(file);
        Pager::open_read_write(vfs, path, options)
    }

    /// Reports whether this pager refuses every write.
    pub fn is_read_only(&self) -> bool {
        self.read_only
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

    /// Returns the page numbers the transaction has modified.
    pub fn dirty_pages(&self) -> Vec<u32> {
        self.dirty.iter().copied().collect()
    }

    /// Returns how many undo levels are open: one for the transaction, and one
    /// for each live statement or savepoint.
    pub fn undo_depth(&self) -> usize {
        self.undo.len()
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
        if let Err(error) = self.file.lock(FileLock::Reserved) {
            let error = error.into_db_error();
            if error.code() == rustdb_base::PrimaryCode::Busy {
                return Err(error);
            }
            return Err(self.fail(error));
        }
        self.dirty.clear();
        self.undo.clear();
        self.freed.clear();
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
        self.record_image(page)?;
        let current = self.get_page(page)?;
        let mut buffer = self.copy_bytes(current.bytes())?;
        drop(current);
        let result = edit(buffer.as_mut_slice())?;
        let key = PageKey {
            database: self.database,
            page,
        };
        self.cache.publish(
            key,
            buffer,
            PageState::Dirty {
                before_image_saved: true,
            },
        )?;
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
                if self.dirty.contains(&page) {
                    if let Ok(page_id) = PageId::from_persisted(page) {
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
    pub fn commit(&mut self) -> DbResult<()> {
        self.check_usable()?;
        self.require_writer()?;
        if self.dirty.is_empty() {
            return self.finish_transaction();
        }
        self.reach_failpoint(FailSite::Commit)?;
        let mut header = self.header;
        header.change_counter = header.change_counter.wrapping_add(1);
        header.database_size = self.page_count;
        header.version_valid_for = header.change_counter;
        header.write_library_version = WRITE_LIBRARY_VERSION;
        self.set_header(header)?;

        if let Err(error) = self.file.lock(FileLock::Exclusive) {
            let error = error.into_db_error();
            if error.code() == rustdb_base::PrimaryCode::Busy {
                return Err(error);
            }
            return Err(self.fail(error));
        }
        self.state = PagerState::WriterDbMod;

        let pages: Vec<u32> = self.dirty.iter().copied().collect();
        for page in pages.iter().copied().filter(|page| *page != 1) {
            if let Err(error) = self.write_page_to_file(page) {
                return Err(self.fail(error));
            }
        }
        if let Err(error) = self.file.sync(SyncMode::Normal) {
            return Err(self.fail(error.into_db_error()));
        }
        if pages.contains(&1) {
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
        if let Err(error) = self.file.sync(SyncMode::Normal) {
            return Err(self.fail(error.into_db_error()));
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
        self.finish_transaction()
    }

    /// Undoes the whole transaction and releases the writer's locks.
    pub fn rollback(&mut self) -> DbResult<()> {
        if !self.is_writing() && self.undo.is_empty() {
            return Ok(());
        }
        while let Some(frame) = self.undo.pop() {
            self.restore(frame)?;
        }
        self.dirty.clear();
        self.counters.rollbacks = self.counters.rollbacks.saturating_add(1);
        self.finish_transaction()
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
            return Err(DbError::primary(rustdb_base::PrimaryCode::IoErr)
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
        if self.file.lock_level() != FileLock::None {
            let _ = self.file.unlock(FileLock::None);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustdb_vfs::memory::MemoryVfs;

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
            .open(&path, OpenOptions::of_kind(rustdb_vfs::FileKind::MainDb))
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
        assert_eq!(error.code(), rustdb_base::PrimaryCode::Misuse);

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
        assert_eq!(error.code(), rustdb_base::PrimaryCode::Corrupt);
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
        assert_eq!(error.code(), rustdb_base::PrimaryCode::Corrupt);
        assert_eq!(pager.state(), PagerState::Error);
        // Every later call returns the same error, including one that would
        // otherwise have succeeded.
        let again = pager.get_page(PageId::new(2).unwrap()).unwrap_err();
        assert_eq!(again.code(), rustdb_base::PrimaryCode::Corrupt);
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
            .open(&path, OpenOptions::of_kind(rustdb_vfs::FileKind::MainDb))
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
