//! An in-process VFS.
//!
//! Invariant: the in-memory VFS obeys exactly the same contract as a real disk,
//! including the locking protocol and short-read reporting. It is not a
//! simplified stand-in: `:memory:` databases and the fast half of the test
//! suite run on it, so a behaviour it gets wrong is a behaviour the engine
//! learns to depend on.
//!
//! What it deliberately does not model is failure. Torn writes, ENOSPC, and
//! crashes belong to `inillucent-sim`, which layers those on top of the same
//! structures rather than duplicating them.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use crate::contract::{
    AccessMode, DeviceCharacteristics, FileIdentity, FileLock, OpenOptions, SharedMemory,
    ShmLockRequest, ShmRegion, SyncMode, Vfs, VfsFile, SHM_LOCK_COUNT,
};
use crate::error::{self, VfsError, VfsOperation, VfsResult};
use crate::locks::{next_step, HandleId, LockConflict, LockTable};
use crate::path::DbPath;
use crate::shm_locks::ShmLockTable;

/// One file's contents and lock state, shared by every handle that opened it.
#[derive(Debug)]
struct Inode {
    id: u64,
    data: Mutex<Vec<u8>>,
    locks: Mutex<LockTable>,
    shm: Mutex<Option<Arc<MemoryShm>>>,
}

/// The whole in-memory file system.
#[derive(Debug, Default)]
struct Directory {
    files: BTreeMap<PathBuf, Arc<Inode>>,
    next_inode: u64,
    next_handle: u64,
    next_temp: u64,
}

/// A VFS whose files live in this process's memory.
#[derive(Debug)]
pub struct MemoryVfs {
    name: String,
    directory: Arc<Mutex<Directory>>,
    device: DeviceCharacteristics,
    clock: AtomicU64,
}

impl MemoryVfs {
    /// Creates an empty in-memory file system.
    pub fn new() -> MemoryVfs {
        MemoryVfs {
            name: "memory".to_string(),
            directory: Arc::new(Mutex::new(Directory::default())),
            device: MemoryVfs::default_device(),
            clock: AtomicU64::new(0),
        }
    }

    /// Returns the device characteristics memory presents.
    ///
    /// Memory really does write whole pages atomically and never reorders, so
    /// the declaration is honest rather than optimistic. The conformance suite
    /// checks that a VFS which declares a capability actually has it.
    fn default_device() -> DeviceCharacteristics {
        DeviceCharacteristics {
            atomic_write_size: 65_536,
            safe_append: true,
            sequential: true,
            undeletable_when_open: false,
            sector_size: 512,
            powersafe_overwrite: true,
            immutable: false,
            supports_mmap: false,
        }
    }

    /// Reports whether a path exists, without opening it.
    pub fn contains(&self, path: &DbPath) -> bool {
        match self.directory.lock() {
            Ok(directory) => directory.files.contains_key(path.as_path()),
            Err(poisoned) => poisoned.into_inner().files.contains_key(path.as_path()),
        }
    }

    /// Returns a snapshot of a file's bytes, which crash tests compare against.
    pub fn snapshot(&self, path: &DbPath) -> Option<Vec<u8>> {
        let directory = lock_or_recover(&self.directory);
        let inode = directory.files.get(path.as_path())?;
        Some(
            inode
                .data
                .lock()
                .map_or_else(|p| p.into_inner().clone(), |d| d.clone()),
        )
    }
}

impl Default for MemoryVfs {
    /// Creates an empty in-memory file system.
    fn default() -> MemoryVfs {
        MemoryVfs::new()
    }
}

/// Locks a mutex, recovering from poisoning rather than propagating a panic.
///
/// A poisoned mutex means some other thread panicked while holding it. The VFS
/// cannot fix that, but taking the process down with it turns one test failure
/// into an unreadable one, so the state is taken as-is and the caller's own
/// invariants decide what to do.
fn lock_or_recover<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    match mutex.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

impl Vfs for MemoryVfs {
    /// Returns the registered name of this VFS.
    fn name(&self) -> &str {
        &self.name
    }

    /// Opens or creates a file.
    fn open(&self, path: &DbPath, options: OpenOptions) -> VfsResult<Box<dyn VfsFile>> {
        let mut directory = lock_or_recover(&self.directory);
        let existing = directory.files.get(path.as_path()).cloned();
        if existing.is_some() && options.exclusive {
            return Err(VfsError::new(
                VfsOperation::Open.extended_code(),
                format!("{} already exists", path.display()),
            ));
        }
        let inode = match existing {
            Some(inode) => inode,
            None => {
                if !options.create {
                    return Err(VfsError::new(
                        VfsOperation::Open.extended_code(),
                        format!("{} does not exist", path.display()),
                    ));
                }
                directory.next_inode = directory.next_inode.saturating_add(1);
                let inode = Arc::new(Inode {
                    id: directory.next_inode,
                    data: Mutex::new(Vec::new()),
                    locks: Mutex::new(LockTable::new()),
                    shm: Mutex::new(None),
                });
                directory
                    .files
                    .insert(path.as_path().to_path_buf(), Arc::clone(&inode));
                inode
            }
        };
        directory.next_handle = directory.next_handle.saturating_add(1);
        let handle = HandleId(directory.next_handle);
        drop(directory);
        Ok(Box::new(MemoryFile {
            inode,
            handle,
            level: Mutex::new(FileLock::None),
            options,
            path: path.clone(),
            directory: Arc::clone(&self.directory),
            device: self.device,
        }))
    }

    /// Removes a file. Deleting a file that is not there is not an error,
    /// matching what the pager expects when it tidies up a journal twice.
    fn delete(&self, path: &DbPath, _sync_dir: bool) -> VfsResult<()> {
        let mut directory = lock_or_recover(&self.directory);
        directory.files.remove(path.as_path());
        Ok(())
    }

    /// Moves one entry of the directory to another name, replacing whatever
    /// was there.
    ///
    /// Atomic for free: the whole directory is behind one lock, so no reader
    /// can observe a state in which neither name resolves. A rename of a file
    /// that is not there is a failure rather than a silent success - unlike
    /// `delete`, which the pager calls twice on purpose - because a caller
    /// renaming a file it did not create is a caller with a bug.
    fn rename(&self, from: &DbPath, to: &DbPath) -> VfsResult<()> {
        let mut directory = lock_or_recover(&self.directory);
        let Some(file) = directory.files.remove(from.as_path()) else {
            return Err(VfsError::new(
                inillucent_base::error::ExtendedCode::from_primary(
                    inillucent_base::error::PrimaryCode::IoErr,
                ),
                format!("rename: {} is not there", from.as_path().display()),
            ));
        };
        directory.files.insert(to.as_path().to_path_buf(), file);
        Ok(())
    }

    /// Reports whether a path exists. Everything in memory is readable and
    /// writable, so the mode only changes the question for a missing file.
    fn access(&self, path: &DbPath, mode: AccessMode) -> VfsResult<bool> {
        let directory = lock_or_recover(&self.directory);
        let present = directory.files.contains_key(path.as_path());
        Ok(match mode {
            AccessMode::Exists | AccessMode::ReadWrite | AccessMode::ReadOnly => present,
        })
    }

    /// Returns the path unchanged; an in-memory name is already canonical.
    fn full_pathname(&self, path: &DbPath) -> VfsResult<DbPath> {
        Ok(path.clone())
    }

    /// Fills `output` with randomness from the operating system.
    fn randomness(&self, output: &mut [u8]) -> VfsResult<()> {
        crate::os::system_randomness(output)
    }

    /// Returns the current wall-clock time.
    fn current_time(&self) -> VfsResult<SystemTime> {
        Ok(SystemTime::now())
    }

    /// Returns a temporary name that is not currently in use.
    fn temp_path(&self, prefix: &str) -> VfsResult<DbPath> {
        let mut directory = lock_or_recover(&self.directory);
        for _ in 0..1_000_000 {
            directory.next_temp = directory.next_temp.saturating_add(1);
            let candidate = DbPath::new(format!("/tmp/{prefix}{:016x}", directory.next_temp));
            if !directory.files.contains_key(candidate.as_path()) {
                return Ok(candidate);
            }
        }
        Err(VfsError::new(
            VfsOperation::GetTempPath.extended_code(),
            "exhausted temporary names",
        ))
    }

    /// Advances a counter instead of sleeping; nothing in memory is waiting on
    /// wall-clock time, and a real sleep would only slow the suite down.
    fn sleep(&self, micros: u64) -> VfsResult<()> {
        self.clock.fetch_add(micros, Ordering::Relaxed);
        Ok(())
    }
}

/// One open handle on an in-memory file.
#[derive(Debug)]
struct MemoryFile {
    inode: Arc<Inode>,
    handle: HandleId,
    level: Mutex<FileLock>,
    options: OpenOptions,
    path: DbPath,
    directory: Arc<Mutex<Directory>>,
    device: DeviceCharacteristics,
}

impl MemoryFile {
    /// Returns the lock level this handle holds.
    fn current_level(&self) -> FileLock {
        *lock_or_recover(&self.level)
    }

    /// Refuses a write on a handle that was opened read-only.
    fn require_writable(&self, operation: VfsOperation) -> VfsResult<()> {
        if self.options.read_only {
            return Err(error::read_only(format!(
                "{operation:?} on a read-only handle"
            )));
        }
        Ok(())
    }
}

impl VfsFile for MemoryFile {
    /// Reads `output.len()` bytes at `offset`, zero-filling and reporting a
    /// short read when the file ends first.
    fn read_exact_at(&self, offset: u64, output: &mut [u8]) -> VfsResult<()> {
        let data = lock_or_recover(&self.inode.data);
        let start = usize::try_from(offset).unwrap_or(usize::MAX);
        let available = data.len().saturating_sub(start);
        let copied = available.min(output.len());
        for (slot, byte) in output.iter_mut().zip(data.iter().skip(start).take(copied)) {
            *slot = *byte;
        }
        if copied < output.len() {
            for slot in output.iter_mut().skip(copied) {
                *slot = 0;
            }
            return Err(error::short_read(format!(
                "read {copied} of {} bytes at {offset}",
                output.len()
            )));
        }
        Ok(())
    }

    /// Writes `input` at `offset`, growing the file with zeroes when the write
    /// starts past the current end.
    fn write_all_at(&self, offset: u64, input: &[u8]) -> VfsResult<()> {
        self.require_writable(VfsOperation::Write)?;
        let mut data = lock_or_recover(&self.inode.data);
        let start = usize::try_from(offset).map_err(|_| {
            VfsError::new(
                VfsOperation::Write.extended_code(),
                "offset does not fit in memory",
            )
        })?;
        let end = start.checked_add(input.len()).ok_or_else(|| {
            VfsError::new(VfsOperation::Write.extended_code(), "write end overflowed")
        })?;
        if data.len() < end {
            data.resize(end, 0);
        }
        for (slot, byte) in data.iter_mut().skip(start).zip(input.iter()) {
            *slot = *byte;
        }
        Ok(())
    }

    /// Returns the file's length.
    fn file_size(&self) -> VfsResult<u64> {
        Ok(lock_or_recover(&self.inode.data).len() as u64)
    }

    /// Sets the file's length, growing with zeroes or discarding the tail.
    fn truncate(&self, size: u64) -> VfsResult<()> {
        self.require_writable(VfsOperation::Truncate)?;
        let mut data = lock_or_recover(&self.inode.data);
        let target = usize::try_from(size).map_err(|_| {
            VfsError::new(
                VfsOperation::Truncate.extended_code(),
                "size does not fit in memory",
            )
        })?;
        data.resize(target, 0);
        Ok(())
    }

    /// Memory has nothing to flush, but a read-only handle still refuses.
    fn sync(&self, _mode: SyncMode) -> VfsResult<()> {
        self.require_writable(VfsOperation::Sync)
    }

    /// Raises the lock level.
    fn lock(&self, level: FileLock) -> VfsResult<()> {
        let mut current = lock_or_recover(&self.level);
        if level <= *current {
            return Ok(());
        }
        let mut table = lock_or_recover(&self.inode.locks);
        let mut step = *current;
        while step < level {
            let next = next_step(step, level);
            match table.acquire(self.handle, step, next) {
                Ok(()) => step = next,
                Err(LockConflict::Busy) => {
                    *current = step;
                    return Err(error::busy(format!("cannot raise to {next:?}")));
                }
                Err(LockConflict::Protocol) => {
                    *current = step;
                    return Err(error::misuse(format!(
                        "illegal transition {step:?} -> {next:?}"
                    )));
                }
            }
        }
        *current = level;
        Ok(())
    }

    /// Lowers the lock level.
    fn unlock(&self, level: FileLock) -> VfsResult<()> {
        let mut current = lock_or_recover(&self.level);
        if level >= *current {
            return Ok(());
        }
        let mut table = lock_or_recover(&self.inode.locks);
        table
            .release(self.handle, *current, level)
            .map_err(|_| error::misuse(format!("illegal release {:?} -> {level:?}", *current)))?;
        *current = level;
        Ok(())
    }

    /// Returns the lock level this handle holds.
    fn lock_level(&self) -> FileLock {
        self.current_level()
    }

    /// Reports whether another handle holds RESERVED or stronger.
    fn check_reserved_lock(&self) -> VfsResult<bool> {
        let table = lock_or_recover(&self.inode.locks);
        Ok(table.has_reserved_or_stronger(self.handle))
    }

    /// Returns what memory guarantees.
    fn device_characteristics(&self) -> DeviceCharacteristics {
        self.device
    }

    /// Returns the shared-memory file for this database, creating it on first
    /// use. Only a main database has one.
    fn shared_memory(&self) -> VfsResult<Option<Arc<dyn SharedMemory>>> {
        if !self.options.kind.is_locked() {
            return Ok(None);
        }
        let mut slot = lock_or_recover(&self.inode.shm);
        let shm = match slot.as_ref() {
            Some(shm) => Arc::clone(shm),
            None => {
                let shm = Arc::new(MemoryShm::default());
                *slot = Some(Arc::clone(&shm));
                shm
            }
        };
        Ok(Some(shm))
    }

    /// Returns the inode number, which is stable for two handles on one file.
    fn file_identity(&self) -> VfsResult<FileIdentity> {
        Ok(FileIdentity {
            volume: 0,
            file: u128::from(self.inode.id),
        })
    }
}

impl Drop for MemoryFile {
    /// Releases every lock the handle held and removes the file when it was
    /// opened to be deleted on close.
    fn drop(&mut self) {
        lock_or_recover(&self.inode.locks).release_all(self.handle);
        if self.options.delete_on_close {
            lock_or_recover(&self.directory)
                .files
                .remove(self.path.as_path());
        }
    }
}

/// The shared-memory file behind an in-memory WAL index.
#[derive(Debug, Default)]
struct MemoryShm {
    regions: Mutex<Vec<Arc<MemoryShmRegion>>>,
    locks: Mutex<ShmLockTable>,
}

/// One mapped shared-memory region.
#[derive(Debug)]
struct MemoryShmRegion {
    bytes: Mutex<Vec<u8>>,
}

impl ShmRegion for MemoryShmRegion {
    /// Returns the region's length.
    fn len(&self) -> usize {
        lock_or_recover(&self.bytes).len()
    }

    /// Copies bytes out of the region.
    fn read(&self, offset: usize, output: &mut [u8]) -> VfsResult<()> {
        let bytes = lock_or_recover(&self.bytes);
        let end = offset.checked_add(output.len()).ok_or_else(|| {
            VfsError::new(VfsOperation::ShmMap.extended_code(), "shm read overflowed")
        })?;
        let window = bytes.get(offset..end).ok_or_else(|| {
            VfsError::new(
                VfsOperation::ShmMap.extended_code(),
                "shm read out of range",
            )
        })?;
        for (slot, byte) in output.iter_mut().zip(window.iter()) {
            *slot = *byte;
        }
        Ok(())
    }

    /// Copies bytes into the region.
    fn write(&self, offset: usize, input: &[u8]) -> VfsResult<()> {
        let mut bytes = lock_or_recover(&self.bytes);
        let end = offset.checked_add(input.len()).ok_or_else(|| {
            VfsError::new(VfsOperation::ShmMap.extended_code(), "shm write overflowed")
        })?;
        let window = bytes.get_mut(offset..end).ok_or_else(|| {
            VfsError::new(
                VfsOperation::ShmMap.extended_code(),
                "shm write out of range",
            )
        })?;
        for (slot, byte) in window.iter_mut().zip(input.iter()) {
            *slot = *byte;
        }
        Ok(())
    }
}

impl SharedMemory for MemoryShm {
    /// Maps a region, growing the file when asked.
    fn map(
        &self,
        index: u32,
        region_size: usize,
        extend: bool,
    ) -> VfsResult<Option<Arc<dyn ShmRegion>>> {
        let mut regions = lock_or_recover(&self.regions);
        let wanted = usize::try_from(index)
            .ok()
            .and_then(|index| index.checked_add(1))
            .ok_or_else(|| {
                VfsError::new(
                    VfsOperation::ShmMap.extended_code(),
                    "region index overflowed",
                )
            })?;
        if regions.len() < wanted {
            if !extend {
                return Ok(None);
            }
            while regions.len() < wanted {
                regions.push(Arc::new(MemoryShmRegion {
                    bytes: Mutex::new(vec![0u8; region_size]),
                }));
            }
        }
        match regions.get(wanted.saturating_sub(1)) {
            Some(region) => Ok(Some(Arc::clone(region) as Arc<dyn ShmRegion>)),
            None => Ok(None),
        }
    }

    /// Takes or releases shared-memory lock slots.
    fn lock(&self, request: ShmLockRequest) -> VfsResult<()> {
        if request.offset >= SHM_LOCK_COUNT || request.count == 0 {
            return Err(error::misuse("shared-memory lock slot out of range"));
        }
        lock_or_recover(&self.locks).apply(request)
    }

    /// A single process's memory needs no barrier beyond the mutexes above,
    /// but the call still exists so callers do not learn to skip it.
    fn barrier(&self) {
        std::sync::atomic::fence(Ordering::SeqCst);
    }

    /// Drops the mapping. There is no file to delete in memory.
    fn unmap(&self, _delete: bool) -> VfsResult<()> {
        Ok(())
    }
}
