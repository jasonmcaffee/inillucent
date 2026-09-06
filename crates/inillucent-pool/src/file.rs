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
use inillucent_vfs::{DbPath, OpenOptions, Vfs};

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
        };
        let mut next = FIRST_DATA_PAGE.0;
        let created = database.free.ensure(FIRST_DATA_PAGE.0, &mut next)?;
        database.pool.set_page_count(next);
        database.meta.page_count = next;
        database.meta.free_map = database.free.first();
        let _ = created;
        database.write_free_map()?;
        Ok(database)
    }

    /// Opens an existing database.
    ///
    /// @param vfs - the file system to read from
    /// @param path - the database file
    /// @param frames - how many frames the pool holds
    pub fn open(vfs: &dyn Vfs, path: &DbPath, frames: usize) -> DbResult<Database> {
        let file = vfs
            .open(path, OpenOptions::main_db())
            .map_err(|error| error.into_db_error())?;
        // The page size lives in the meta page, and the meta page cannot be
        // read without it. The first sixteen bytes are readable at any size -
        // magic, format, page size - so they are read first and the whole page
        // is then read and checksummed at the size they declare. Only if those
        // sixteen bytes are themselves damaged does the reader fall back to
        // trying page sizes, and then it accepts a candidate only when the
        // record it decodes agrees with the size it was decoded at, so an
        // ambiguous answer is impossible rather than merely unlikely.
        let page_size = match declared_page_size(file.as_ref()) {
            Some(size) => size,
            None => discover_page_size(file.as_ref())
                .ok_or_else(|| corrupt("neither meta page is readable"))?,
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
        let mut free = FreeMap::new(page_size);
        let mut next = meta.free_map;
        while !next.is_none() {
            let image = {
                let guard = pool.fetch(next)?;
                guard.bytes().to_vec()
            };
            let after = crate::page::right_of(&image)?;
            free.push_page(next, image)?;
            next = after;
        }
        Ok(Database { pool, meta, free })
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

    /// Returns how many pages the free map describes as free.
    pub fn free_pages(&self) -> u64 {
        self.free.free_count()
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

    /// Installs a page image the caller built.
    ///
    /// @param page - the page id, already allocated
    /// @param image - the page bytes
    pub fn install(&self, page: PageId, image: &[u8]) -> DbResult<()> {
        self.pool.install(page, image)
    }

    /// Writes the free map's pages into the pool.
    fn write_free_map(&mut self) -> DbResult<()> {
        let images: Vec<(PageId, Vec<u8>)> = self
            .free
            .pages()
            .map(|(id, bytes)| (id, bytes.to_vec()))
            .collect();
        for (id, image) in images {
            self.pool.install(id, &image)?;
        }
        Ok(())
    }

    /// Flushes every dirty page, writes the meta record, and syncs.
    ///
    /// The generation is bumped here rather than by the caller, because "the
    /// newer meta page wins" is only true if every checkpoint moves it.
    pub fn checkpoint(&mut self) -> DbResult<()> {
        self.write_free_map()?;
        self.meta.page_count = self.pool.page_count();
        self.meta.free_map = self.free.first();
        self.meta.generation = self.meta.generation.saturating_add(1);
        let meta = self.meta;
        self.pool.checkpoint(&meta)
    }
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
    if size < crate::meta::META_BYTES || size > 1 << 20 {
        return None;
    }
    Some(size)
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
    #[test]
    fn a_fresh_database_is_two_meta_pages_and_a_map() {
        let vfs = MemoryVfs::new();
        let path = DbPath::new("fresh.rdb");
        let database =
            Database::create(&vfs, &path, Options::default().with_page_size(512)).unwrap();
        assert_eq!(database.page_size(), 512);
        assert_eq!(database.meta().free_map, PageId(2));
        assert!(database.catalog_root().is_none());
        assert!(database.free_pages() > 0);
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
