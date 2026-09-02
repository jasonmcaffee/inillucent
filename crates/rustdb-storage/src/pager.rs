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

use std::sync::Arc;

use rustdb_base::buffer::PageBuffer;
use rustdb_base::error::{corrupt, misuse};
use rustdb_base::ids::{DatabaseId, PageId};
use rustdb_base::page::{self, PageSize};
use rustdb_base::{DbError, DbResult};
use rustdb_value::TextEncoding;
use rustdb_vfs::{DbPath, FileLock, OpenOptions, Vfs, VfsFile};

use crate::cache::{CacheCounters, PageCache, PageKey, PagePin};
use crate::header::{DatabaseHeader, HEADER_SIZE};

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
        u64::from(self.pages_in_file()).saturating_mul(u64::from(self.header.page_size.bytes()))
    }

    /// Returns the pager's state.
    pub fn state(&self) -> PagerState {
        self.state
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
            return Err(corrupt(format!(
                "page {} ends at byte {end} in a {}-byte file",
                page.get(),
                self.file_bytes
            )));
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
