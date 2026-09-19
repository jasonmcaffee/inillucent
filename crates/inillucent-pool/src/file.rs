//! The database file: the pool, the meta record and the free map as one object.
//!
//! Invariant: a page is handed out by [`Database::allocate`] only after the free
//! map says it is free, and the map is grown before the file is, so there is
//! never a page in the file the map cannot describe. The two are one operation
//! for that reason; splitting them is how a file ends up with pages nobody owns.
//!
//! This is Phase 2's whole durability story, and it is deliberately small: load,
//! checkpoint, close. There is no WAL yet - the TDD schedules it for Phase 3 -
//! so a checkpoint here writes every dirty page and then the meta record, and a
//! crash between the two leaves the previous meta record describing a file whose
//! pages are a superset of what it claims. That is safe to reopen and it is not
//! safe to *write* to, which is why nothing in Phase 2 writes to a database it
//! did not just create.

use inillucent_base::error::{corrupt, misuse};
use inillucent_base::DbResult;
use inillucent_vfs::{DbPath, FileLock, OpenOptions, Vfs};

use crate::freemap::FreeMap;
use crate::meta::{Meta, FIRST_DATA_PAGE, META_PAGE, SHADOW_PAGE};
use crate::pool::Pool;
use crate::PageId;

/// How many frames a pool gets when nothing says otherwise.
///
/// 4,096 frames at the 32 KiB default is 128 MiB, which holds every scorecard
/// fixture at every scale. The number is a default rather than a policy: the
/// fairness section of a measurement states the pool size it ran at, and the
/// gate harness sets it explicitly on both engines.
pub const DEFAULT_FRAMES: usize = 4_096;

/// How a database is opened.
#[derive(Clone, Copy, Debug)]
pub struct Options {
    /// The page size, used only when creating.
    pub page_size: usize,
    /// How many frames the pool holds.
    pub frames: usize,
}

impl Default for Options {
    fn default() -> Options {
        Options {
            page_size: 32_768,
            frames: DEFAULT_FRAMES,
        }
    }
}

impl Options {
    /// Returns the options with a different pool size.
    ///
    /// @param frames - how many frames the pool holds
    pub fn with_frames(mut self, frames: usize) -> Options {
        self.frames = frames;
        self
    }

    /// Returns the options with a different page size.
    ///
    /// @param page_size - the page size in bytes
    pub fn with_page_size(mut self, page_size: usize) -> Options {
        self.page_size = page_size;
        self
    }
}

/// One open database file.
pub struct Database {
    /// The buffer pool over the file.
    pool: Pool,
    /// What the meta record last said, with the caller's edits applied.
    meta: Meta,
    /// The free map, held resident because it is consulted on every allocation.
    free: FreeMap,
    /// The shared extent page a small out-of-line value goes on next.
    ///
    /// **A hint, held only in memory.** A value that fits inside
    /// one page is packed beside others rather than given a page of its own, and
    /// finding a page with room by searching would be a scan of the file per
    /// value. So the writer remembers the last page it filled and starts there;
    /// when it is full, the next value allocates a fresh one.
    ///
    /// Nothing durable refers to it. A reopened database starts with `None` and
    /// fills a new page, which costs the tail of one page per open and cannot be
    /// wrong: every value already written is found through its own reference.
    shared_extent: Option<PageId>,
    /// How long this connection waits for a contended file before reporting it
    /// busy, in milliseconds.
    ///
    /// `PRAGMA busy_timeout`'s value, pushed down by the engine. See
    /// [`DEFAULT_BUSY_MILLIS`].
    busy_millis: u64,
    /// Whether this connection may write the file at all.
    ///
    /// **A flag on the storage, not a filter above it (task-1979, C5 and
    /// section 5.2).** `--readonly` used to be a statement filter on the
    /// command surface and the file was opened for writing either way - so the
    /// open took the same locks a writer takes, waited out the whole busy
    /// budget against a live writer and then reported the writer's lock. A read
    /// only connection now opens the file read only, never raises past SHARED,
    /// and refuses a write here rather than relying on somebody above to have
    /// asked.
    read_only: bool,
    /// Whether what this connection holds was derived while it held the file.
    ///
    /// **False from `open` until the owner says otherwise.** The open path
    /// reads the meta record under a shared lock, which two processes hold at
    /// once, and the caller above reads the log's tail with no lock at all - so
    /// everything a connection starts with was read at a moment another process
    /// could have been writing. It goes false again every time the file is let
    /// go, because the same is then true of everything cached since
    /// (task-1979, section 4.4 item 1).
    trusted: bool,
}

impl Database {
    /// Creates a fresh, empty database.
    ///
    /// @param vfs - the file system to create in
    /// @param path - where to create it
    /// @param options - the page size and pool size
    pub fn create(vfs: &dyn Vfs, path: &DbPath, options: Options) -> DbResult<Database> {
        let file = vfs
            .open(path, OpenOptions::main_db())
            .map_err(|error| error.into_db_error())?;
        // **Exclusively, for the length of the creation.** A file that has been
        // opened but not yet written is not an empty database - it has no meta
        // record at all - so a second process reading it reports corruption
        // rather than waiting. Holding the file until the two meta pages exist
        // is what turns that race into a wait.
        wait_for_lock(file.as_ref(), FileLock::Shared)?;
        wait_for_lock(file.as_ref(), FileLock::Reserved)?;
        wait_for_lock(file.as_ref(), FileLock::Exclusive)?;
        file.truncate(0).map_err(|error| error.into_db_error())?;
        let mut uuid = [0u8; 16];
        vfs.randomness(&mut uuid)
            .map_err(|error| error.into_db_error())?;
        let meta = Meta::fresh(
            u32::try_from(options.page_size).unwrap_or(u32::MAX),
            u128::from_le_bytes(uuid),
        );
        let pool = Pool::new(file, options.page_size, options.frames, FIRST_DATA_PAGE.0)?;
        // The two meta pages exist from the first byte, so that a page id is
        // never ambiguous about whether the file is long enough to hold it.
        let mut image = vec![0u8; options.page_size];
        meta.encode(&mut image)?;
        for slot in [META_PAGE, SHADOW_PAGE] {
            pool.write_meta_slot(slot, &image)?;
        }
        let mut database = Database {
            pool,
            meta,
            free: FreeMap::new(options.page_size),
            shared_extent: None,
            busy_millis: DEFAULT_BUSY_MILLIS,
            read_only: false,
            trusted: false,
        };
        let mut next = FIRST_DATA_PAGE.0;
        let created = database.free.ensure(FIRST_DATA_PAGE.0, &mut next)?;
        database.pool.set_page_count(next);
        database.meta.page_count = next;
        database.meta.free_map = database.free.first();
        let _ = created;
        database.write_free_map()?;
        // **The file is let go once it exists.** The exclusive lock above is
        // held only for as long as there is no meta record for a second
        // process to read; keeping it would make creating a database and then
        // opening it a deadlock with itself, which is what the pool's own tests
        // do. The connection takes whatever lock it needs at its first
        // statement, exactly as an opened one does.
        database.pool.unlock(FileLock::None)?;
        Ok(database)
    }

    /// Opens an existing database.
    ///
    /// @param vfs - the file system to read from
    /// @param path - the database file
    /// @param frames - how many frames the pool holds
    pub fn open(vfs: &dyn Vfs, path: &DbPath, frames: usize) -> DbResult<Database> {
        let (pool, meta) = Self::open_bootstrap(vfs, path, frames)?;
        let free = read_free_map(&pool, meta.free_map)?;
        Ok(Database {
            pool,
            meta,
            free,
            shared_extent: None,
            busy_millis: DEFAULT_BUSY_MILLIS,
            read_only: false,
            trusted: false,
        })
    }

    /// Opens an existing database for reading and never for writing.
    ///
    /// The file handle itself is read only, so a write that reached the media
    /// through any path at all is refused by the operating system rather than
    /// by this crate's own bookkeeping - which is what makes "the file is
    /// unchanged" a property of the open rather than of the filter above it.
    ///
    /// The free map is not walked, for the same reason
    /// [`Database::open_before_recovery`] does not: the caller replays the log
    /// into its own pool first and calls [`Database::load_free_map`] after.
    ///
    /// @param vfs - the file system to read from
    /// @param path - the database file
    /// @param frames - how many frames the pool holds
    pub fn open_read_only(vfs: &dyn Vfs, path: &DbPath, frames: usize) -> DbResult<Database> {
        let (pool, meta) = Self::open_bootstrap_with(vfs, path, frames, true)?;
        let free = FreeMap::new(pool.page_size());
        Ok(Database {
            pool,
            meta,
            free,
            shared_extent: None,
            busy_millis: DEFAULT_BUSY_MILLIS,
            read_only: true,
            trusted: false,
        })
    }

    /// Reports whether this connection may write the file.
    pub fn read_only(&self) -> bool {
        self.read_only
    }

    /// Opens an existing database without walking its free map.
    ///
    /// **For a caller about to run WAL redo, and no one else.** The free
    /// map's own pages are ordinary pages, checked the same way [`Pool::fetch`]
    /// checks any other page - unlike the meta record, which is read twice
    /// over from [`META_PAGE`] and [`SHADOW_PAGE`] by [`Meta::choose`]
    /// precisely so that one torn copy never fails an open. A checkpoint that
    /// tears a free map page mid-writeback leaves no such second copy, and
    /// reading it here, before redo has replayed the log frame that would put
    /// it back, turned a crash the log could recover from into a refusal -
    /// see `open_file` in `inillucent-engine`, the only caller that needs
    /// this staged rather than done in one step.
    ///
    /// The database returned reports an empty free map - allocating or
    /// releasing a page against it before [`Database::load_free_map`] is
    /// called back would silently disagree with the file - which is why
    /// `open_file` runs redo and calls it immediately, before doing anything
    /// else with the pages redo touched.
    ///
    /// @param vfs - the file system to read from
    /// @param path - the database file
    /// @param frames - how many frames the pool holds
    pub fn open_before_recovery(vfs: &dyn Vfs, path: &DbPath, frames: usize) -> DbResult<Database> {
        let (pool, meta) = Self::open_bootstrap(vfs, path, frames)?;
        let free = FreeMap::new(pool.page_size());
        Ok(Database {
            pool,
            meta,
            free,
            shared_extent: None,
            busy_millis: DEFAULT_BUSY_MILLIS,
            read_only: false,
            trusted: false,
        })
    }

    /// Walks the free map's own pages and fills in what
    /// [`Database::open_before_recovery`] deferred.
    ///
    /// Idempotent - it replaces `self.free` outright rather than adding to
    /// it - so it is safe to call once redo has had its chance whether or not
    /// a page it needed turned out to already be fine.
    pub fn load_free_map(&mut self) -> DbResult<()> {
        self.free = read_free_map(&self.pool, self.meta.free_map)?;
        Ok(())
    }

    /// Reads everything `open` and `open_before_recovery` share: the meta
    /// record, chosen from its two copies, and the pool sized to what it
    /// describes.
    ///
    /// @param vfs - the file system to read from
    /// @param path - the database file
    /// @param frames - how many frames the pool holds
    fn open_bootstrap(vfs: &dyn Vfs, path: &DbPath, frames: usize) -> DbResult<(Pool, Meta)> {
        Self::open_bootstrap_with(vfs, path, frames, false)
    }

    /// [`Database::open_bootstrap`], with the caller saying whether the handle
    /// may write.
    ///
    /// @param vfs - the file system to read from
    /// @param path - the database file
    /// @param frames - how many frames the pool holds
    /// @param read_only - whether the file handle refuses writes
    fn open_bootstrap_with(
        vfs: &dyn Vfs,
        path: &DbPath,
        frames: usize,
        read_only: bool,
    ) -> DbResult<(Pool, Meta)> {
        let options = match read_only {
            true => OpenOptions::main_db().read_only(),
            false => OpenOptions::main_db(),
        };
        let file = vfs
            .open(path, options)
            .map_err(|error| error.into_db_error())?;
        // The page size lives in the meta page, and the meta page cannot be
        // read without it. The first sixteen bytes are readable at any size -
        // magic, format, page size - so they are read first and the whole page
        // is then read and checksummed at the size they declare. Only if those
        // sixteen bytes are themselves damaged does the reader fall back to
        // trying page sizes, and then it accepts a candidate only when the
        // record it decodes agrees with the size it was decoded at, so an
        // ambiguous answer is impossible rather than merely unlikely.
        // **A shared lock before the first read.** The meta pages are being
        // rewritten by any process that is committing, and a reader that took
        // no lock could read one of them halfway through - which reports as a
        // malformed image rather than as the contention it is. Held for the
        // rest of the open; the connection's `locking_mode` decides what
        // happens to it after that.
        wait_for_lock(file.as_ref(), FileLock::Shared)?;
        // **A file with no readable meta record may be one being created**, and
        // the two are told apart by waiting: a creation finishes, and damage
        // does not. The budget is the same one a busy lock waits out, and the
        // message at the end of it is the one this always reported.
        // **A file of another format version is named before the wait.** Its
        // header is perfectly well formed and only newer than this build, so
        // `declared_page_size` answers `None` for it, the loop below waited out
        // the whole busy budget and then reported `neither meta page is
        // readable` - corruption, for a file with nothing wrong with it
        // (task-1979, E3).
        if let Some(found) = foreign_format_version(file.as_ref()) {
            return Err(crate::meta::wrong_format(found));
        }
        // **A SQLite file says so rather than reading as damage (task-1979,
        // section 8.1, gap 7).** It is the most common first mistake, and the
        // answer was `database disk image is malformed` - which sends somebody
        // looking for corruption in a file that is perfectly good and is simply
        // another engine's. `docs/sql.md` has always said a SQLite file is
        // imported rather than opened in place; this is that sentence, at the
        // moment it is needed.
        if is_a_sqlite_file(file.as_ref()) {
            return Err(inillucent_base::error::refusal(
                "this is a SQLite database, and this engine writes its own format; \
                 `inillucent-migrate <file> <new.rdb>` reads it and writes one",
            )
            .with_unsupported("opening a SQLite database in place"));
        }
        let mut waited = 0u64;
        let page_size = loop {
            if let Some(size) = declared_page_size(file.as_ref()) {
                break size;
            }
            if let Some(size) = discover_page_size(file.as_ref()) {
                break size;
            }
            if waited >= DEFAULT_BUSY_MILLIS {
                return Err(corrupt("neither meta page is readable"));
            }
            // Released while waiting, because the process finishing the
            // creation needs the file to do it.
            let _ = file.unlock(FileLock::None);
            std::thread::sleep(std::time::Duration::from_millis(5));
            waited = waited.saturating_add(5);
            wait_for_lock(file.as_ref(), FileLock::Shared)?;
        };
        let mut primary = vec![0u8; page_size];
        let mut shadow = vec![0u8; page_size];
        file.read_exact_at(0, &mut primary)
            .map_err(|error| error.into_db_error())?;
        file.read_exact_at(page_size as u64, &mut shadow)
            .map_err(|error| error.into_db_error())?;
        let meta = Meta::choose(&primary, &shadow)?;
        if meta.page_size as usize != page_size {
            return Err(corrupt("the meta pages disagree about the page size"));
        }
        let pool = Pool::new(file, page_size, frames, meta.page_count)?;
        // **The high water the file already carries, folded back in before a
        // page is written.** A run that reads and checkpoints
        // without writing a stamped page would otherwise record a lower number
        // than the run before it, and the next open would resume the log below
        // a stamp that is still in the file.
        pool.note_high_water_lsn(meta.high_water_lsn);
        Ok((pool, meta))
    }

    /// Gives back the tail of the file that the meta record does not describe.
    ///
    /// **A crash between a page write and the meta record that would have
    /// claimed it leaves a file longer than its own header.** The pool grows
    /// the file when it writes a page past the end, and the meta record is
    /// written last, on purpose - so a transaction that grew the file and then
    /// did not become durable leaves pages nothing refers to and nothing will
    /// ever reclaim. The rollback journal puts the *contents* back and says
    /// nothing about the length, which is the half SQLite's journal header
    /// carries and this format does not.
    ///
    /// Called by the open path once recovery has finished, so `page_count` is
    /// everything the log had to say. A file that is already the right length
    /// or shorter is left alone; a failure to truncate is reported, because a
    /// file this cannot shrink is one the next allocation would grow again from
    /// the wrong place.
    ///
    /// Reached by `durability.rs`'s
    /// `a_recovered_database_is_no_longer_than_its_header_says`, which crashes
    /// at every call of a growing transaction and measures the file against its
    /// own header (task-1980).
    pub fn give_back_the_unclaimed_tail(&mut self) -> DbResult<()> {
        let wanted = self
            .pool
            .page_count()
            .saturating_mul(self.pool.page_size() as u64);
        let there = self
            .pool
            .file()
            .file_size()
            .map_err(inillucent_vfs::VfsError::into_db_error)?;
        if there <= wanted {
            return Ok(());
        }
        self.pool
            .file()
            .truncate(wanted)
            .map_err(inillucent_vfs::VfsError::into_db_error)
    }

    /// Returns the buffer pool, so a caller that owns the file can grow it.
    ///
    /// Held apart from [`Database::pool`] because everything else about a pool
    /// is reachable through a shared borrow: the frames themselves are the one
    /// thing a `&Pool` cannot add to, and `PRAGMA cache_size` is the caller
    /// that needs to.
    pub fn pool_mut(&mut self) -> &mut Pool {
        &mut self.pool
    }

    /// Returns the shared extent page a small value should be offered to.
    ///
    /// See the field. `None` means there is no page to try, and the caller
    /// allocates one.
    pub fn shared_extent(&self) -> Option<PageId> {
        self.shared_extent
    }

    /// Records which shared extent page the next small value should be tried on.
    ///
    /// @param page - the page, or `None` to forget the one held
    pub fn set_shared_extent(&mut self, page: Option<PageId>) {
        self.shared_extent = page;
    }

    /// Returns the pool, for a reader that wants a page.
    pub fn pool(&self) -> &Pool {
        &self.pool
    }

    /// Returns the meta record as it currently stands.
    pub fn meta(&self) -> &Meta {
        &self.meta
    }

    /// Returns the page size in bytes.
    pub fn page_size(&self) -> usize {
        self.pool.page_size()
    }

    /// Returns the catalog tree's root page.
    pub fn catalog_root(&self) -> PageId {
        self.meta.catalog_root
    }

    /// Records the catalog tree's root page.
    ///
    /// @param root - the root page id
    pub fn set_catalog_root(&mut self, root: PageId) {
        self.meta.catalog_root = root;
    }

    /// Returns how many pages of the file are free.
    ///
    /// Bounded by the file's own page count: the map has room for many more
    /// bits than the file has pages, and counting all of them is what made
    /// `PRAGMA freelist_count` answer six figures on a five-page database.
    pub fn free_pages(&self) -> u64 {
        self.free.free_count_below(self.pool.page_count())
    }

    /// Returns the four bytes `PRAGMA user_version` reads.
    pub fn user_version(&self) -> i32 {
        self.meta.user_version
    }

    /// Records what `PRAGMA user_version` was set to.
    ///
    /// It lands in the meta record and reaches the file at the next
    /// checkpoint, which is the same durability every other meta field has.
    ///
    /// @param value - the value the application wrote
    pub fn set_user_version(&mut self, value: i32) {
        self.meta.user_version = value;
    }

    /// Reports whether the file says it is in write-ahead-log mode.
    ///
    /// The one journal mode that outlives the connection that chose it, so an
    /// open reads it rather than assuming: a database left in WAL comes back in
    /// WAL, which is SQLite's rule and what stops one connection writing undo
    /// images into a file another is appending frames to.
    pub fn wal_mode(&self) -> bool {
        self.meta.wal
    }

    /// Records whether the database is in write-ahead-log mode.
    ///
    /// It lands in the meta record and reaches the file at the next
    /// checkpoint, which is the same durability every other meta field has.
    ///
    /// @param wal - whether the log is the mode in force
    pub fn set_wal_mode(&mut self, wal: bool) {
        self.meta.wal = wal;
    }

    /// Returns the four bytes `PRAGMA application_id` reads.
    pub fn application_id(&self) -> i32 {
        self.meta.application_id
    }

    /// Records what `PRAGMA application_id` was set to.
    ///
    /// @param value - the value the application wrote
    pub fn set_application_id(&mut self, value: i32) {
        self.meta.application_id = value;
    }

    /// Returns the cookie `PRAGMA schema_version` reports.
    pub fn schema_cookie(&self) -> i32 {
        self.meta.schema_cookie
    }

    /// Moves the schema cookie on, which every schema change does.
    ///
    /// An application watches this to know its cached statements are stale,
    /// which is the whole reason SQLite keeps one.
    pub fn bump_schema_cookie(&mut self) {
        self.meta.schema_cookie = self.meta.schema_cookie.wrapping_add(1);
    }

    /// Records where the log had reached when this checkpoint was taken.
    ///
    /// `checkpoint_lsn` is **where recovery starts**, which is not simply the
    /// LSN the checkpoint reached. The policy is no-steal, so a page dirtied by
    /// an uncommitted transaction is never written here; a transaction that
    /// began before the checkpoint and commits after it therefore has records
    /// *below* the checkpoint that were never applied to this file. The
    /// checkpointer passes `min(durable end, the first LSN of the oldest still
    /// open transaction)` and recovery starts there. Records in that range that
    /// were already applied cost a read and are skipped by the page-LSN rule.
    ///
    /// @param checkpoint_lsn - where recovery must start
    /// @param cts_watermark - the newest commit timestamp at the checkpoint
    /// @param wal_sequence - the segment `checkpoint_lsn` lives in
    pub fn set_log_position(&mut self, checkpoint_lsn: u64, cts_watermark: u64, wal_sequence: u64) {
        self.meta.checkpoint_lsn = checkpoint_lsn;
        self.meta.cts_watermark = cts_watermark;
        self.meta.wal_sequence = wal_sequence;
    }

    /// Returns the database's identity, which stamps every WAL segment.
    pub fn uuid(&self) -> u128 {
        self.meta.uuid
    }

    /// Allocates a contiguous run of pages, growing the file if it must.
    ///
    /// @param count - how many pages are wanted
    pub fn allocate(&mut self, count: u64) -> DbResult<PageId> {
        if count == 0 {
            return Err(misuse("an allocation of no pages"));
        }
        if let Some(page) = self.free.allocate_run(count)? {
            self.pool
                .set_page_count(self.pool.page_count().max(page.0.saturating_add(count)));
            return Ok(page);
        }
        // The map has no run that long, so the file grows. Growing the map
        // first is what keeps every page in the file describable.
        let mut next = self
            .pool
            .page_count()
            .max(self.free.described_pages())
            .max(FIRST_DATA_PAGE.0);
        let wanted = next.saturating_add(count);
        self.free.ensure(wanted, &mut next)?;
        self.pool.set_page_count(next);
        let page = self
            .free
            .allocate_run(count)?
            .ok_or_else(|| misuse("the free map grew and still had no room"))?;
        self.pool
            .set_page_count(self.pool.page_count().max(page.0.saturating_add(count)));
        Ok(page)
    }

    /// Marks one page as in use, whatever the free map currently says.
    ///
    /// The one caller is **recovery**, and it is the one caller that can have
    /// one: outside recovery a page is claimed by [`Database::allocate`], which
    /// is what chose it. Replaying an `AllocPage` record means claiming a page
    /// somebody else chose, in a map that may or may not already say so - a
    /// checkpoint may have written the map after the allocation, or before it.
    /// So this is idempotent by construction, which is also what the page-LSN
    /// rule cannot give it: a free-map bit has no LSN.
    ///
    /// The map is grown first when the page is past its end, so a page the file
    /// holds is always one the map can describe.
    ///
    /// @param page - the page to claim
    pub fn claim(&mut self, page: PageId) -> DbResult<()> {
        if page.0 >= self.free.described_pages() {
            let mut next = self.pool.page_count().max(self.free.described_pages());
            self.free.ensure(page.0.saturating_add(1), &mut next)?;
            self.pool.set_page_count(self.pool.page_count().max(next));
        }
        self.pool
            .set_page_count(self.pool.page_count().max(page.0.saturating_add(1)));
        self.free.allocate_at(page)
    }

    /// Returns a run of pages to the free map.
    ///
    /// @param first - the run's first page
    /// @param count - how many pages
    pub fn release(&mut self, first: PageId, count: u64) -> DbResult<()> {
        for offset in 0..count {
            self.free.free(PageId(first.0.saturating_add(offset)))?;
        }
        Ok(())
    }

    /// Takes the file lock a read needs, and reloads if the file has moved.
    ///
    /// **Both halves, together, because either alone is wrong.** Taking the
    /// lock without checking would read a cache describing a database another
    /// process has since rewritten; checking without the lock would race the
    /// process doing the rewriting. The lock is taken first and the check is
    /// made under it, which is the order that makes the answer stable for as
    /// long as the lock is held.
    ///
    /// Returns whether the cache was thrown away, which the caller reports.
    pub fn begin_read(&mut self) -> DbResult<bool> {
        // **Already holding the file means nothing has changed under it.**
        // Another process can only have written while this one held no lock, so
        // a connection that has kept one - every statement inside a transaction,
        // and every statement at all under `locking_mode = exclusive` - has
        // nothing to take and nothing to reread. Without this the meta record is
        // read from the file once per statement, which took the medium gate's
        // `txn.large` from 1.57x to 0.01x before it was measured.
        if self.pool.lock_level() != FileLock::None {
            return Ok(false);
        }
        self.pool.lock(FileLock::Shared)?;
        self.reload_if_moved()
    }

    /// Raises the lock to the one a write needs.
    ///
    /// `Exclusive` rather than `Reserved`: this engine writes pages in place
    /// under a rollback journal and appends to a log under a write-ahead one,
    /// and neither is safe to interleave with another process's reads without
    /// the shared-memory index that would let a reader find the log. Taking the
    /// stronger lock is the honest version of that - writers serialise with
    /// readers, and nobody is told otherwise.
    pub fn begin_write(&mut self) -> DbResult<bool> {
        self.begin_write_within(true)
    }

    /// Raises the lock to the one a write needs, optionally without letting go.
    ///
    /// **`may_release` is false inside a transaction, and that is not a
    /// tuning knob.** The release-and-retry below is what breaks a deadlock
    /// between two connections that each hold a shared lock and each want to
    /// raise it - but releasing is only safe when nothing has been written yet.
    /// Inside a transaction the file has already changed under this lock, and
    /// letting go would let another process write through the middle of it.
    /// So a transaction that cannot raise reports the file as busy, which is
    /// what SQLite does with the same deadlock and is a caller's cue to retry
    /// the whole transaction.
    ///
    /// @param may_release - whether the shared lock may be dropped to retry
    pub fn begin_write_within(&mut self, may_release: bool) -> DbResult<bool> {
        // **A read only connection never raises past SHARED.** It has nothing
        // to protect from a reader and nothing to write, and raising is what
        // made `--readonly` wait out the busy budget against a live writer and
        // then report the writer's lock (task-1979, C5).
        if self.read_only {
            return Err(inillucent_base::error::DbError::primary(
                inillucent_base::error::PrimaryCode::ReadOnly,
            )
            .with_message("this connection is read only and cannot write the database")
            .with_detail("this connection is read only and cannot write the database"));
        }
        // The same short circuit `begin_read` makes, for the same reason: a
        // writer that already holds the file exclusively has nothing to raise.
        if self.pool.lock_level() == FileLock::Exclusive {
            return Ok(false);
        }
        if !may_release {
            // **Only a connection that held nothing re-derives**, which is the
            // short circuit `begin_read` makes and for the same reason: a
            // connection that already holds the file has had it all along, so
            // nothing can have changed under it, and throwing its pages away in
            // the middle of a transaction would discard the transaction's own
            // writes. The reload is after the raise rather than before it for
            // the reason `attempt_write` gives.
            let held = self.pool.lock_level() != FileLock::None;
            self.pool.lock(FileLock::Shared)?;
            self.pool.lock(FileLock::Reserved)?;
            self.pool.lock(FileLock::Exclusive)?;
            if held {
                return Ok(false);
            }
            return self.reload_if_moved();
        }
        self.begin_write_retrying()
    }

    /// Raises the lock, letting go and coming back when it cannot.
    ///
    /// **Every failure inside the loop goes to the backoff**, including the one
    /// that reacquires the shared lock. That is the whole correction over the
    /// first attempt: a writer that has just released to break a deadlock finds
    /// the other side holding PENDING, which is exactly the state PENDING
    /// exists to produce - it stops new readers so the writer can drain them -
    /// and returning that as an error made the loop give up on the first round
    /// nine times out of ten. It is not a failure; it is the other writer
    /// working.
    fn begin_write_retrying(&mut self) -> DbResult<bool> {
        // **Never release a lock this connection already holds.** The retry
        // below starts by letting go, which is right when the raise has failed
        // and wrong when there was nothing to raise: a writer holding the file
        // would drop it, reread the meta record and take it again, once per
        // statement.
        if self.pool.lock_level() == FileLock::Exclusive {
            return Ok(false);
        }
        let mut waited = 0u64;
        let mut pause = 1u64;
        loop {
            let attempt = self.attempt_write();
            match attempt {
                Ok(reloaded) => return Ok(reloaded),
                Err(_) if waited >= self.busy_millis => {
                    let refusal = file_is_busy(
                        self.pool.file(),
                        FileLock::Exclusive,
                        waited,
                        self.busy_millis,
                    );
                    let _ = self.pool.unlock(FileLock::None);
                    return Err(refusal);
                }
                Err(_) => {}
            }
            let _ = self.pool.unlock(FileLock::None);
            std::thread::sleep(std::time::Duration::from_millis(pause));
            waited = waited.saturating_add(pause);
            // Jittered, so two writers that started together do not keep
            // colliding on the same schedule for ever.
            pause = pause.saturating_mul(2).min(50).saturating_add(waited % 3);
        }
    }

    /// One attempt at taking the write lock, from nothing.
    ///
    /// The reload happens after the shared lock and before the upgrade, so the
    /// cache is refreshed on the round the lock is actually won on rather than
    /// on some earlier round the writer then lost.
    fn attempt_write(&mut self) -> DbResult<bool> {
        self.trusted = false;
        self.pool.unlock(FileLock::None)?;
        self.pool.lock_within(FileLock::Shared, 0)?;
        self.pool.lock_within(FileLock::Reserved, 0)?;
        self.pool.lock_within(FileLock::Exclusive, 0)?;
        // **Read after the lock that excludes a writer, not before it**
        // (task-1979, section 4; measured again in task-1980). This asked the
        // file what had changed while it held SHARED, which two processes hold
        // at once, and then raised to EXCLUSIVE - so the answer was about the
        // file as it was before the *other* writer's statement, and this one
        // went on to write from pages it had just decided were current. The
        // window is small and the loss is rare: two writers each running three
        // hundred single statement inserts through the command line lost about
        // one acknowledged row in six hundred, with `PRAGMA integrity_check`
        // clean and the row never visible to any process.
        //
        // Reading here instead costs nothing extra - it is the same two page
        // reads - and it is the only order in which the answer is still true
        // when the write happens. `begin_read` needs no such move: SHARED is
        // the lock a read needs, and a writer cannot write while it is held.
        self.reload_if_moved()
    }

    /// Releases the file lock, which is what `locking_mode = normal` does.
    ///
    /// Everything cached is untrusted from here: another process may write the
    /// file before this connection takes it again.
    pub fn end_access(&mut self) -> DbResult<()> {
        self.trusted = false;
        self.pool.unlock(FileLock::None)
    }

    /// Reports whether what this connection holds was derived under the lock it
    /// now has.
    ///
    /// See the field. The owner asks this on the way into a statement and
    /// re-derives when the answer is no.
    pub fn trusted(&self) -> bool {
        self.trusted
    }

    /// Records that the owner has re-derived everything under the lock.
    pub fn mark_trusted(&mut self) {
        self.trusted = true;
    }

    /// Returns the lock level currently held.
    pub fn lock_level(&self) -> FileLock {
        self.pool.lock_level()
    }

    /// Sets how long this connection waits for a contended file.
    ///
    /// `PRAGMA busy_timeout`'s value, pushed down by the engine (task-1979,
    /// C7). Zero means one attempt and no waiting, which is what SQLite's own
    /// zero means.
    ///
    /// @param millis - the budget in milliseconds
    pub fn set_busy_millis(&mut self, millis: u64) {
        self.busy_millis = millis;
    }

    /// Returns how long this connection waits for a contended file.
    pub fn busy_millis(&self) -> u64 {
        self.busy_millis
    }

    /// Returns the meta record the file holds right now, read past the cache.
    ///
    /// **Only a caller holding the file lock may act on the answer.** Without
    /// the lock another process can commit between the read and the use.
    ///
    /// `None` when neither slot decodes, which is not this function's to
    /// report: the read that follows says so, with the message the open path
    /// uses.
    pub fn meta_on_disk(&self) -> DbResult<Option<Meta>> {
        let page_size = self.pool.page_size();
        let (primary, shadow) = self.pool.read_meta_slots(page_size)?;
        Ok(Meta::choose(&primary, &shadow).ok())
    }

    /// Returns the generation this connection's cache describes.
    pub fn generation(&self) -> u64 {
        self.meta.generation
    }

    /// Throws the cache away and adopts a meta record read from the file.
    ///
    /// **The free map is deliberately left empty**, exactly as
    /// [`Database::open_before_recovery`] leaves it: the caller is about to
    /// replay a log whose records may include the free map's own pages, and
    /// walking the chain before that replay turns a state the log can rebuild
    /// into a refusal. The caller runs redo and then calls
    /// [`Database::load_free_map`].
    ///
    /// @param found - the meta record the file holds
    pub fn adopt_from_file(&mut self, found: Meta) -> DbResult<()> {
        // **Abandoned rather than discarded, and that is the fix rather than a
        // detail.** A discard writes every dirty frame back on its way out, so
        // a connection re-deriving its view of a file another process has
        // written would first write its own stale pages over that process's
        // work. Nothing is lost by dropping them: the caller replays the log
        // from this record's own checkpoint next, and the write-ahead rule
        // means every change they hold is in a record there
        // (task-1979, section 4.4 item 3).
        self.pool.abandon_all()?;
        self.pool.set_page_count(found.page_count);
        // **The high water the adopted file carries, folded in before a page is
        // written.** `open_bootstrap` does this for the same reason and it is
        // the same failure here: a connection that resumed its log below a stamp
        // the file's pages already carry writes records recovery then skips -
        // "a page stamped by a stream that no longer exists silently swallows
        // every later write to it", in `resume_above_every_stamp`'s own words -
        // and the row is gone with nothing reporting it. Without this line two
        // long-lived writer processes lost 261 of 599 acknowledged inserts.
        self.pool.note_high_water_lsn(found.high_water_lsn);
        self.meta = found;
        self.free = FreeMap::new(self.pool.page_size());
        self.shared_extent = None;
        Ok(())
    }

    /// Refuses when the cache holds a change the file does not.
    ///
    /// **Discarding writes the dirty frames back on the way out**, so throwing
    /// the cache away for another process's sake writes this connection's stale
    /// pages over that process's work. Under one writer at a time this cannot
    /// happen: a connection checkpoints before it releases the file, so it
    /// holds nothing uncheckpointed while another process can write. Reaching
    /// here anyway is a defect, and `busy` is the outcome that loses nothing -
    /// the caller retries and the state it needs is still in its own log
    /// (task-1979, section 4.4 item 3).
    fn refuse_if_the_cache_is_dirty(&self) -> DbResult<()> {
        let dirty = self.pool.dirty_pages();
        if dirty == 0 {
            return Ok(());
        }
        Err(inillucent_base::error::busy(format!(
            "another process has written this file and this connection still holds {dirty} uncheckpointed pages"
        )))
    }

    /// Throws the cache away when the file's meta record has moved on.
    ///
    /// The generation is bumped by every checkpoint, so a generation greater
    /// than the one in memory means another process has checkpointed since this
    /// one last looked. Every cached page may then describe a database that no
    /// longer exists - including the free map, which is why it is rebuilt from
    /// the new record rather than kept.
    ///
    /// **A commit that has not been checkpointed does not move the generation**,
    /// so this is not on its own enough to tell a connection its cache is
    /// stale. The engine asks the log the same question at the same moment -
    /// see `inillucent_wal::tail_on_disk` - and runs the fuller resynchronisation
    /// when either answer has moved (task-1979, section 4.4).
    ///
    /// Returns whether anything was thrown away.
    fn reload_if_moved(&mut self) -> DbResult<bool> {
        let Some(found) = self.meta_on_disk()? else {
            return Ok(false);
        };
        if found.generation <= self.meta.generation {
            return Ok(false);
        }
        self.refuse_if_the_cache_is_dirty()?;
        self.pool.discard_all()?;
        self.pool.set_page_count(found.page_count);
        self.meta = found;
        self.free = read_free_map(&self.pool, self.meta.free_map)?;
        Ok(true)
    }

    /// Installs a page image the caller built.
    ///
    /// @param page - the page id, already allocated
    /// @param image - the page bytes
    pub fn install(&self, page: PageId, image: &[u8]) -> DbResult<()> {
        self.pool.install(page, image)
    }

    /// Returns the free map's own pages as byte images, without installing them.
    ///
    /// **A plain read, so a caller with a log can protect the rewrite this
    /// crate cannot.** `FreeMap` keeps no per-page dirty bit - see
    /// [`Database::checkpoint`]'s own comment - so every free-map page is
    /// rewritten on every checkpoint whether or not it changed, and
    /// [`Pool::install`] neither logs that rewrite nor stamps the page with an
    /// LSN. A caller sitting above a log turns that into a redoable write: it
    /// logs a `WritePage` record for each image this returns, stamps the
    /// image with that record's own LSN, and installs the stamped copy before
    /// calling [`Database::checkpoint_after_free_map`] - see
    /// `inillucent_txn::engine::log_free_map_pages`, the shared
    /// implementation both `Engine::checkpoint` and `ImportedDatabase::checkpoint`
    /// call this crate's callers cannot see from here, since neither a log nor
    /// a transaction manager exists at this layer.
    pub fn free_map_pages(&self) -> Vec<(PageId, Vec<u8>)> {
        self.free
            .pages()
            .map(|(id, bytes)| (id, bytes.to_vec()))
            .collect()
    }

    /// Writes the free map's pages into the pool, unlogged.
    ///
    /// The rewrite [`Database::checkpoint`] does when nobody has a log to
    /// protect it with - see [`Database::free_map_pages`] for what that costs.
    fn write_free_map(&mut self) -> DbResult<()> {
        for (id, image) in self.free_map_pages() {
            self.pool.install(id, &image)?;
        }
        Ok(())
    }

    /// Flushes every dirty page, writes the meta record, and syncs.
    ///
    /// Rewrites the free map's own pages first, unlogged and in place - the
    /// only rewrite this crate can do, since it holds no log. A caller that
    /// does hold one - `inillucent-txn`'s `Engine` and `inillucent-engine`'s
    /// `ImportedDatabase` - does not call this: it calls
    /// [`Database::free_map_pages`] itself, logs and stamps each image, installs
    /// the stamped copies, and calls [`Database::checkpoint_after_free_map`]
    /// instead, so the rewrite this function does unlogged is one a redo
    /// record already covers there.
    ///
    /// The generation is bumped here rather than by the caller, because "the
    /// newer meta page wins" is only true if every checkpoint moves it.
    pub fn checkpoint(&mut self) -> DbResult<()> {
        self.write_free_map()?;
        self.checkpoint_after_free_map()
    }

    /// Finishes a checkpoint whose free-map pages are already installed.
    ///
    /// **The other half of [`Database::checkpoint`], for a caller that
    /// installed its own - logged and stamped - copies of the free map's
    /// pages first.** Everything after the free map's own rewrite is
    /// unchanged between the two paths: the meta record's bookkeeping, the
    /// pool's flush, and the sync that makes it durable.
    pub fn checkpoint_after_free_map(&mut self) -> DbResult<()> {
        // See [`Database::open_read_only`]: a checkpoint is a write, and the
        // file handle would refuse it anyway. Refusing here names why.
        if self.read_only {
            return Err(inillucent_base::error::DbError::primary(
                inillucent_base::error::PrimaryCode::ReadOnly,
            )
            .with_message("this connection is read only and cannot checkpoint")
            .with_detail("this connection is read only and cannot checkpoint"));
        }
        self.meta.page_count = self.pool.page_count();
        self.meta.free_map = self.free.first();
        self.meta.generation = self.meta.generation.saturating_add(1);
        // **The highest stamp any page in the file carries is filled in by the
        // pool, after the flush.** It cannot be read here: the
        // pages this checkpoint is about to write are part of the file the meta
        // record describes, and they have not reached it yet. The record comes
        // back with the number in it, and it is kept, because it must never go
        // backwards - the next open resumes the log above it.
        let mut meta = self.meta;
        self.pool.checkpoint(&mut meta)?;
        self.meta = meta;
        Ok(())
    }
}

/// How long a writer keeps trying before it reports the file as busy, when
/// nobody has said otherwise.
///
/// **`PRAGMA busy_timeout` is what says otherwise** (task-1979, C7). This was a
/// constant no pragma, flag or environment variable could reach, so a caller
/// that wanted to wait longer for a contended file had nothing to set; the
/// engine now pushes the pragma's value into [`Database::set_busy_millis`] and
/// this is only the value a connection starts with. The open path still uses it
/// directly, because an open runs before there is a connection to have set it.
pub const DEFAULT_BUSY_MILLIS: u64 = 5_000;

/// Walks a free map's page chain and assembles it.
///
/// Shared by [`Database::open`], [`Database::load_free_map`] and
/// `reload_if_moved` so there is one opinion about how the chain is read
/// rather than three that could drift apart. Every page it fetches goes
/// through [`Pool::fetch`], so a caller reading a checkpoint's own writeback
/// while a torn page might still be resident there gets a cache hit rather
/// than a checksum failure - see [`Database::open_before_recovery`].
///
/// @param pool - the buffer pool the file is open through
/// @param head - the free map's first page, from the meta record
fn read_free_map(pool: &Pool, head: PageId) -> DbResult<FreeMap> {
    let mut free = FreeMap::new(pool.page_size());
    let mut next = head;
    while !next.is_none() {
        let image = {
            let guard = pool.fetch(next)?;
            guard.bytes().to_vec()
        };
        let after = crate::page::right_of(&image)?;
        free.push_page(next, image)?;
        next = after;
    }
    Ok(free)
}

/// Takes a lock on a file, waiting for whoever holds it to finish.
///
/// The same waiting `Pool::lock_within` does, written here because the file is
/// not in a pool yet: this runs before one exists.
///
/// @param file - the database file
/// @param level - the level to reach
fn wait_for_lock(file: &dyn inillucent_vfs::VfsFile, level: FileLock) -> DbResult<()> {
    wait_for_lock_within(file, level, DEFAULT_BUSY_MILLIS)
}

/// Takes a lock on a file, waiting up to a budget the caller states.
///
/// @param file - the database file
/// @param level - the level to reach
/// @param budget_millis - how long to keep trying
fn wait_for_lock_within(
    file: &dyn inillucent_vfs::VfsFile,
    level: FileLock,
    budget_millis: u64,
) -> DbResult<()> {
    if file.lock_level() >= level {
        return Ok(());
    }
    let mut waited = 0u64;
    let mut pause = 1u64;
    loop {
        match file.lock(level) {
            Ok(()) => return Ok(()),
            Err(_) if waited >= budget_millis => {
                return Err(file_is_busy(file, level, waited, budget_millis))
            }
            Err(_) => {}
        }
        std::thread::sleep(std::time::Duration::from_millis(pause));
        waited = waited.saturating_add(pause);
        pause = pause.saturating_mul(2).min(50);
    }
}

/// Returns the refusal a caller that could not take the file is given.
///
/// **It names the holder's operation and this caller's, and no lock level**
/// (task-1979, C6). The level a holder is at is not something a caller can
/// observe or act on, and the message that named one was wrong every time the
/// holder was a reader: every contended case, reader or writer, answered "a
/// writer holds PENDING". What a caller can act on is whether somebody else is
/// writing, how long this connection waited, and which pragma changes that.
///
/// @param file - the file that could not be locked
/// @param wanted - the level this caller was trying to reach
/// @param waited - how long it tried, in milliseconds
/// @param budget - the budget it was given, in milliseconds
fn file_is_busy(
    file: &dyn inillucent_vfs::VfsFile,
    wanted: FileLock,
    waited: u64,
    budget: u64,
) -> inillucent_base::error::DbError {
    // RESERVED or stronger is somebody who intends to write. Anything else
    // holding the file is a reader, which is the case the old message got
    // wrong.
    let holder = match file.check_reserved_lock() {
        Ok(true) | Err(_) => "writing",
        Ok(false) => "reading",
    };
    let mine = if wanted > FileLock::Shared {
        "writing"
    } else {
        "reading"
    };
    inillucent_base::error::busy(format!(
        "another process holds the file for {holder}; this connection wanted it for {mine}, \
         and waited {waited} ms of the {budget} ms PRAGMA busy_timeout"
    ))
}

/// Returns the page size the file's first sixteen bytes declare.
///
/// Those three fields - magic, format version, page size - are at fixed offsets
/// and are readable without knowing anything else about the file, which is what
/// makes opening one a single read rather than a search.
///
/// @param file - the open data file
fn declared_page_size(file: &dyn inillucent_vfs::VfsFile) -> Option<usize> {
    let mut head = [0u8; 16];
    file.read_exact_at(0, &mut head).ok()?;
    if head.get(0..8)? != crate::meta::MAGIC {
        return None;
    }
    let mut format = [0u8; 4];
    format.copy_from_slice(head.get(8..12)?);
    if u32::from_le_bytes(format) != crate::meta::FORMAT_VERSION {
        return None;
    }
    let mut size = [0u8; 4];
    size.copy_from_slice(head.get(12..16)?);
    let size = u32::from_le_bytes(size) as usize;
    if !(crate::meta::META_BYTES..=1 << 20).contains(&size) {
        return None;
    }
    Some(size)
}

/// Returns whether a file begins with SQLite's own header.
///
/// The sixteen bytes `SQLite format 3` and a NUL, which every SQLite database
/// starts with and which no inillucent file can, because ours starts with
/// `RDB2`.
///
/// @param file - the open data file
fn is_a_sqlite_file(file: &dyn inillucent_vfs::VfsFile) -> bool {
    let mut head = [0u8; 16];
    if file.read_exact_at(0, &mut head).is_err() {
        return false;
    }
    head == *b"SQLite format 3\0"
}

/// Returns the format version a file carries when it is not this build's.
///
/// `None` means the file is either this build's format or not an inillucent
/// database at all - the second is the caller's "neither meta page is
/// readable", which is the right answer for a file whose magic is missing.
///
/// @param file - the open data file
fn foreign_format_version(file: &dyn inillucent_vfs::VfsFile) -> Option<u32> {
    let mut head = [0u8; 12];
    file.read_exact_at(0, &mut head).ok()?;
    if head.get(0..8)? != crate::meta::MAGIC {
        return None;
    }
    let mut format = [0u8; 4];
    format.copy_from_slice(head.get(8..12)?);
    let found = u32::from_le_bytes(format);
    (found != crate::meta::FORMAT_VERSION).then_some(found)
}

/// Returns the page size the shadow meta page declares, by trying sizes.
///
/// Reached only when the primary's header is damaged, so the shadow's position
/// is unknown - it is one page in, and "one page" is the thing being looked for.
/// A candidate is accepted only when the record decodes *and* says it is that
/// size, so two candidates can never both succeed.
///
/// The candidates are powers of two rather than the four sizes the format
/// declares legal, because the pool is also driven at small page sizes by the
/// eviction campaigns and by the tree's split tests, and a reader that could
/// not open what the writer wrote would be a worse contract than a slightly
/// wider search.
///
/// @param file - the open data file
fn discover_page_size(file: &dyn inillucent_vfs::VfsFile) -> Option<usize> {
    let length = file.file_size().ok()?;
    let mut candidate = 256usize;
    while candidate <= 65_536 {
        if (candidate as u64).saturating_mul(2) <= length {
            let mut shadow = vec![0u8; candidate];
            if file.read_exact_at(candidate as u64, &mut shadow).is_ok() {
                if let Ok(meta) = Meta::decode(&shadow) {
                    if meta.page_size as usize == candidate {
                        return Some(candidate);
                    }
                }
            }
        }
        candidate = candidate.saturating_mul(2);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use inillucent_vfs::MemoryVfs;

    /// A fresh database has its two meta pages, a free map, and nothing else.
    ///
    /// **And no free pages.** Every page the file has is one of those three, so
    /// there is nothing free in it - which is what `PRAGMA freelist_count`
    /// reports, and what the reference reports for the same file. This used to
    /// assert the opposite, because `free_pages` counted every bit the map had
    /// room for rather than the pages the file holds: six figures on a
    /// three-page database.
    #[test]
    fn a_fresh_database_is_two_meta_pages_and_a_map() {
        let vfs = MemoryVfs::new();
        let path = DbPath::new("fresh.rdb");
        let mut database =
            Database::create(&vfs, &path, Options::default().with_page_size(512)).unwrap();
        assert_eq!(database.page_size(), 512);
        assert_eq!(database.meta().free_map, PageId(2));
        assert!(database.catalog_root().is_none());
        assert_eq!(database.free_pages(), 0);
        // And a page that is allocated and given back is free again, which is
        // the other half of the definition.
        let page = database.allocate(1).unwrap();
        assert_eq!(database.free_pages(), 0);
        database.release(page, 1).unwrap();
        assert_eq!(database.free_pages(), 1);
    }

    /// Allocation hands out pages nobody else has, and freeing gives them back.
    #[test]
    fn allocation_never_repeats_a_page() {
        let vfs = MemoryVfs::new();
        let path = DbPath::new("alloc.rdb");
        let mut database =
            Database::create(&vfs, &path, Options::default().with_page_size(512)).unwrap();
        let mut seen = std::collections::HashSet::new();
        for _ in 0..200 {
            let page = database.allocate(1).unwrap();
            assert!(seen.insert(page), "page {page:?} was handed out twice");
        }
        let run = database.allocate(8).unwrap();
        for offset in 0..8 {
            assert!(seen.insert(PageId(run.0 + offset)));
        }
        database.release(run, 8).unwrap();
        let again = database.allocate(8).unwrap();
        assert_eq!(again, run, "the freed run came back");
        assert!(database.allocate(0).is_err());
    }

    /// A database written, checkpointed and reopened reads back what it held.
    #[test]
    fn a_checkpointed_database_reopens() {
        let vfs = MemoryVfs::new();
        let path = DbPath::new("reopen.rdb");
        let placed;
        {
            let mut database =
                Database::create(&vfs, &path, Options::default().with_page_size(512)).unwrap();
            placed = database.allocate(1).unwrap();
            let mut image = vec![0u8; 512];
            crate::page::write_common(&mut image, crate::page::PageKind::Leaf, 0, 5).unwrap();
            crate::page::write_u64(&mut image, 32, 0xFEED).unwrap();
            database.install(placed, &image).unwrap();
            database.set_catalog_root(placed);
            database.checkpoint().unwrap();
        }
        let database = Database::open(&vfs, &path, 32).unwrap();
        assert_eq!(database.page_size(), 512);
        assert_eq!(database.catalog_root(), placed);
        let guard = database.pool().fetch(placed).unwrap();
        assert_eq!(crate::page::read_u64(&guard, 32).unwrap(), 0xFEED);
        assert!(database.meta().generation >= 2);
    }

    /// A database grown past one free-map page reopens with the whole chain.
    #[test]
    fn a_long_free_map_chain_reopens() {
        let vfs = MemoryVfs::new();
        let path = DbPath::new("chain.rdb");
        let per = crate::freemap::pages_per_map(512) as u64;
        let mut database =
            Database::create(&vfs, &path, Options::default().with_page_size(512)).unwrap();
        let run = database.allocate(per + 40).unwrap();
        let mut image = vec![0u8; 512];
        crate::page::write_common(&mut image, crate::page::PageKind::Leaf, 0, 1).unwrap();
        for offset in 0..(per + 40) {
            database.install(PageId(run.0 + offset), &image).unwrap();
        }
        database.checkpoint().unwrap();
        let reopened = Database::open(&vfs, &path, 64).unwrap();
        assert!(reopened.meta().page_count > per);
    }

    /// Each of the four legal page sizes creates and reopens.
    #[test]
    fn every_page_size_reopens() {
        for size in crate::page::PageSize::all() {
            let vfs = MemoryVfs::new();
            let path = DbPath::new("sized.rdb");
            {
                let mut database = Database::create(
                    &vfs,
                    &path,
                    Options::default()
                        .with_page_size(size.len())
                        .with_frames(16),
                )
                .unwrap();
                database.checkpoint().unwrap();
            }
            let reopened = Database::open(&vfs, &path, 16).unwrap();
            assert_eq!(reopened.page_size(), size.len(), "page size {size:?}");
        }
    }

    /// A file that is not a database is refused rather than half-opened.
    #[test]
    fn a_file_that_is_not_a_database_is_refused() {
        let vfs = MemoryVfs::new();
        let path = DbPath::new("garbage.rdb");
        let file = vfs.open(&path, OpenOptions::main_db()).unwrap();
        file.write_all_at(0, &vec![0xA5u8; 200_000]).unwrap();
        assert!(Database::open(&vfs, &path, 16).is_err());
    }

    /// A torn primary meta page falls back to the shadow.
    #[test]
    fn a_torn_primary_falls_back_to_the_shadow() {
        let vfs = MemoryVfs::new();
        let path = DbPath::new("torn.rdb");
        {
            let mut database =
                Database::create(&vfs, &path, Options::default().with_page_size(8_192)).unwrap();
            database.checkpoint().unwrap();
        }
        let file = vfs.open(&path, OpenOptions::main_db()).unwrap();
        file.write_all_at(0, &vec![0u8; 8_192]).unwrap();
        drop(file);
        let reopened = Database::open(&vfs, &path, 16).unwrap();
        assert_eq!(reopened.page_size(), 8_192);
    }
}
