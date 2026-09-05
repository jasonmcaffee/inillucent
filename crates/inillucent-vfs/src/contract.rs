//! The VFS contract every storage layer above is written against.
//!
//! Invariant: this is the only description of what the operating system can do
//! for inillucent. A layer above may not call `std::fs`, and a platform below may
//! not add a capability that is not declared here, so the simulator can stand
//! in for a real disk without the pager noticing.
//!
//! The traits are object-safe on purpose. The TDD sketches `Vfs` with an
//! associated `File` type; the simulator has to be substitutable at run time
//! for a file already opened by a generic pager, and the extra indirection
//! costs one virtual call against a syscall, so `Box<dyn VfsFile>` is the
//! better trade and is what the graph test enforces.

use std::fmt::Debug;
use std::sync::Arc;
use std::time::SystemTime;

use crate::error::VfsResult;
use crate::path::DbPath;

/// The five lock levels of the SQLite locking protocol, in increasing strength.
///
/// The transitions are what the protocol actually constrains: any number of
/// connections may hold SHARED at once; one may hold RESERVED while others
/// still read; PENDING stops new readers arriving; and EXCLUSIVE requires every
/// reader to have left.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub enum FileLock {
    /// No lock is held.
    None,
    /// A read lock. Many holders may share it.
    Shared,
    /// The intention to write. One holder at a time; readers continue.
    Reserved,
    /// A write in progress that is waiting for readers to leave. New readers
    /// are refused.
    Pending,
    /// A write lock. No other holder of any level may exist.
    Exclusive,
}

impl FileLock {
    /// Returns the lock levels in increasing order, for exhaustive tests.
    pub fn all() -> [FileLock; 5] {
        [
            FileLock::None,
            FileLock::Shared,
            FileLock::Reserved,
            FileLock::Pending,
            FileLock::Exclusive,
        ]
    }
}

/// How hard a sync has to push the data toward durable media.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SyncMode {
    /// Flush file data; a device that reorders may still reorder.
    Normal,
    /// Flush file data and any device write cache.
    Full,
    /// Flush file data only, leaving metadata such as the length behind.
    DataOnly,
}

/// What a caller wants to know about a path.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AccessMode {
    /// Whether the path exists at all.
    Exists,
    /// Whether the path exists and can be read and written.
    ReadWrite,
    /// Whether the path exists and can be read.
    ReadOnly,
}

/// What a file is for.
///
/// The pager treats the main database, its journal, and its WAL differently -
/// a journal may be deleted on close, a WAL must not be - so the kind travels
/// with the open rather than being guessed from the file name.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FileKind {
    /// The database file itself.
    MainDb,
    /// The rollback journal for a main database.
    MainJournal,
    /// A write-ahead log.
    Wal,
    /// A statement sub-journal.
    SubJournal,
    /// A temporary database.
    TempDb,
    /// A temporary file with no name, used for sorting and spilling.
    Transient,
    /// The multi-database master journal.
    MasterJournal,
}

impl FileKind {
    /// Reports whether files of this kind participate in the locking protocol.
    ///
    /// Only the main database is locked: the journal and WAL are protected by
    /// the lock on the database they belong to.
    pub fn is_locked(self) -> bool {
        matches!(self, FileKind::MainDb)
    }

    /// Reports whether files of this kind are deleted when they are closed.
    pub fn deletes_on_close(self) -> bool {
        matches!(self, FileKind::Transient | FileKind::TempDb)
    }
}

/// How to open a file.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct OpenOptions {
    /// What the file is for.
    pub kind: FileKind,
    /// Create the file when it does not exist.
    pub create: bool,
    /// Open for reading only; every write returns `SQLITE_READONLY`.
    pub read_only: bool,
    /// Fail when the file already exists. Only meaningful with `create`.
    pub exclusive: bool,
    /// Delete the file when the last handle closes.
    pub delete_on_close: bool,
}

impl OpenOptions {
    /// Options for opening an existing database read-write, creating it when
    /// it does not exist.
    pub fn main_db() -> OpenOptions {
        OpenOptions {
            kind: FileKind::MainDb,
            create: true,
            read_only: false,
            exclusive: false,
            delete_on_close: false,
        }
    }

    /// Options for the given kind, with everything else at its default.
    pub fn of_kind(kind: FileKind) -> OpenOptions {
        OpenOptions {
            kind,
            create: true,
            read_only: false,
            exclusive: false,
            delete_on_close: kind.deletes_on_close(),
        }
    }

    /// Returns the same options opened read-only.
    pub fn read_only(mut self) -> OpenOptions {
        self.read_only = true;
        self.create = false;
        self
    }
}

/// What the storage device underneath a file guarantees.
///
/// Durability algorithms may take a shortcut only when the device declares the
/// capability *and* a platform probe has confirmed it. An unknown filesystem
/// declares nothing and gets the conservative path.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DeviceCharacteristics {
    /// The largest write the device performs atomically, in bytes; zero when
    /// no write size is atomic.
    pub atomic_write_size: u32,
    /// Appending to the file cannot leave the tail readable before the data is.
    pub safe_append: bool,
    /// Writes reach the media in the order they were issued.
    pub sequential: bool,
    /// A file cannot be deleted while another handle has it open.
    pub undeletable_when_open: bool,
    /// The size of one device sector, which bounds torn writes.
    pub sector_size: u32,
    /// A power failure during a write cannot damage bytes it was not writing.
    pub powersafe_overwrite: bool,
    /// The file cannot change under us, so a cache never needs invalidating.
    pub immutable: bool,
    /// The file may be memory-mapped.
    pub supports_mmap: bool,
}

impl DeviceCharacteristics {
    /// The declaration an unknown filesystem gets: no shortcut is available.
    pub fn conservative() -> DeviceCharacteristics {
        DeviceCharacteristics {
            atomic_write_size: 0,
            safe_append: false,
            sequential: false,
            undeletable_when_open: false,
            sector_size: 4096,
            powersafe_overwrite: false,
            immutable: false,
            supports_mmap: false,
        }
    }

    /// Reports whether a write of `bytes` is atomic on this device.
    pub fn writes_atomically(&self, bytes: u32) -> bool {
        self.atomic_write_size != 0 && bytes <= self.atomic_write_size
    }
}

/// Which of the eight shared-memory lock slots a caller wants, and how.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ShmLockRequest {
    /// The first slot, in `0..SHM_LOCK_COUNT`.
    pub offset: u16,
    /// How many consecutive slots. Exclusive requests may span more than one;
    /// shared requests take exactly one, matching the WAL-index protocol.
    pub count: u16,
    /// Whether the slots are being taken or released.
    pub acquire: bool,
    /// Whether the lock is exclusive; false means shared.
    pub exclusive: bool,
}

/// How many shared-memory lock slots the WAL-index protocol uses.
pub const SHM_LOCK_COUNT: u16 = 8;

/// A mapped shared-memory region.
///
/// The bytes are shared with every other process that mapped the same region,
/// so the only safe way to read or write them is through the accessors, which
/// use volatile operations and take no reference into the mapping.
pub trait ShmRegion: Send + Sync + Debug {
    /// Returns the region's length in bytes.
    fn len(&self) -> usize;

    /// Reports whether the region is empty.
    fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Copies `output.len()` bytes starting at `offset` out of the region.
    fn read(&self, offset: usize, output: &mut [u8]) -> VfsResult<()>;

    /// Copies `input` into the region at `offset`.
    fn write(&self, offset: usize, input: &[u8]) -> VfsResult<()>;
}

/// The shared-memory file that backs a WAL index.
pub trait SharedMemory: Send + Sync + Debug {
    /// Maps region `index`, each `region_size` bytes long, growing the file
    /// when `extend` is set and returning `None` when it is not and the region
    /// does not exist yet.
    fn map(
        &self,
        index: u32,
        region_size: usize,
        extend: bool,
    ) -> VfsResult<Option<Arc<dyn ShmRegion>>>;

    /// Takes or releases shared-memory lock slots.
    fn lock(&self, request: ShmLockRequest) -> VfsResult<()>;

    /// Orders memory operations so that a write another process made before
    /// its barrier is visible after ours.
    fn barrier(&self);

    /// Releases the mapping, deleting the backing file when `delete` is set and
    /// this is the last user of it.
    fn unmap(&self, delete: bool) -> VfsResult<()>;
}

/// An open file.
pub trait VfsFile: Send + Sync + Debug {
    /// Reads exactly `output.len()` bytes at `offset`.
    ///
    /// A read that finds fewer bytes than asked for zero-fills the rest and
    /// reports `SQLITE_IOERR_SHORT_READ`, which is the distinction the pager
    /// relies on to tell a truncated file from an unreadable one.
    fn read_exact_at(&self, offset: u64, output: &mut [u8]) -> VfsResult<()>;

    /// Writes all of `input` at `offset`.
    fn write_all_at(&self, offset: u64, input: &[u8]) -> VfsResult<()>;

    /// Returns the file's current length.
    fn file_size(&self) -> VfsResult<u64>;

    /// Sets the file's length, discarding anything past it.
    fn truncate(&self, size: u64) -> VfsResult<()>;

    /// Flushes written data toward durable media.
    fn sync(&self, mode: SyncMode) -> VfsResult<()>;

    /// Raises the lock to `level`.
    fn lock(&self, level: FileLock) -> VfsResult<()>;

    /// Lowers the lock to `level`, which must be `None` or `Shared`.
    fn unlock(&self, level: FileLock) -> VfsResult<()>;

    /// Returns the lock level this handle currently holds.
    fn lock_level(&self) -> FileLock;

    /// Reports whether some other handle holds RESERVED or stronger.
    fn check_reserved_lock(&self) -> VfsResult<bool>;

    /// Returns what the device underneath guarantees.
    fn device_characteristics(&self) -> DeviceCharacteristics;

    /// Returns the shared-memory file for this database, creating it when it
    /// does not exist. Files that cannot have one return `Ok(None)`.
    fn shared_memory(&self) -> VfsResult<Option<Arc<dyn SharedMemory>>>;

    /// Returns a value that is equal for two handles on the same file and
    /// different for handles on different files, even when the paths differ.
    fn file_identity(&self) -> VfsResult<FileIdentity>;
}

/// A file's identity, used to recognise two names for the same file.
///
/// A path string is not an identity: a hard link, a junction, a mapped drive,
/// and a relative path all name the same bytes. Every platform therefore
/// answers with device and file numbers where it has them.
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct FileIdentity {
    /// The volume or device the file lives on.
    pub volume: u64,
    /// The file's number within that volume.
    pub file: u128,
}

/// A virtual file system.
pub trait Vfs: Send + Sync + Debug {
    /// Returns the name a caller registers and selects this VFS by.
    fn name(&self) -> &str;

    /// Opens a file.
    fn open(&self, path: &DbPath, options: OpenOptions) -> VfsResult<Box<dyn VfsFile>>;

    /// Deletes a file, optionally flushing the containing directory so that the
    /// deletion itself is durable.
    fn delete(&self, path: &DbPath, sync_dir: bool) -> VfsResult<()>;

    /// Reports whether a path exists or is accessible in the requested mode.
    fn access(&self, path: &DbPath, mode: AccessMode) -> VfsResult<bool>;

    /// Resolves a path to the canonical absolute form the VFS will use.
    fn full_pathname(&self, path: &DbPath) -> VfsResult<DbPath>;

    /// Fills `output` with randomness suitable for seeding, not for keys.
    fn randomness(&self, output: &mut [u8]) -> VfsResult<()>;

    /// Returns the current wall-clock time.
    fn current_time(&self) -> VfsResult<SystemTime>;

    /// Returns a fresh path for a temporary file that does not yet exist.
    fn temp_path(&self, prefix: &str) -> VfsResult<DbPath>;

    /// Sleeps for at least `micros` microseconds, or returns immediately in a
    /// simulated VFS where time is a counter rather than a wait.
    fn sleep(&self, micros: u64) -> VfsResult<()>;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The lock levels must order the way the protocol describes; a comparison
    /// on this enum decides whether a transition is a raise or a drop.
    #[test]
    fn lock_levels_are_ordered_by_strength() {
        assert!(FileLock::None < FileLock::Shared);
        assert!(FileLock::Shared < FileLock::Reserved);
        assert!(FileLock::Reserved < FileLock::Pending);
        assert!(FileLock::Pending < FileLock::Exclusive);
        assert_eq!(FileLock::all().len(), 5);
    }

    /// The conservative declaration must not accidentally grant a shortcut.
    #[test]
    fn the_conservative_device_grants_nothing() {
        let device = DeviceCharacteristics::conservative();
        assert!(!device.writes_atomically(1));
        assert!(!device.writes_atomically(512));
        assert!(!device.safe_append);
        assert!(!device.powersafe_overwrite);
    }

    /// Only the main database takes part in the locking protocol, and only
    /// temporary files disappear on close.
    #[test]
    fn file_kinds_declare_their_locking_and_lifetime() {
        assert!(FileKind::MainDb.is_locked());
        assert!(!FileKind::Wal.is_locked());
        assert!(!FileKind::MainJournal.is_locked());
        assert!(FileKind::Transient.deletes_on_close());
        assert!(!FileKind::MainDb.deletes_on_close());
        assert!(OpenOptions::of_kind(FileKind::Transient).delete_on_close);
    }
}
