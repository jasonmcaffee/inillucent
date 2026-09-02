//! File-backed shared memory and the platform-independent half of locking.
//!
//! Invariant: the shared-memory file is coherent between processes. Two
//! handles on the same regular file see each other's writes through the
//! operating system's cache on both supported platforms, so the WAL index does
//! not need a memory mapping to be correct - only to be fast.
//!
//! This is a deliberate, recorded simplification for phase 1. A mapped
//! implementation is a phase 10 optimisation, at which point it has a WAL
//! workload to be measured against; adding the mapping now would mean writing
//! two pages of unsafe pointer code with nothing that exercises it.

use std::collections::HashMap;
use std::fs::{File, OpenOptions as FsOpenOptions};
use std::sync::{Arc, Mutex, OnceLock};

use crate::contract::{FileIdentity, SharedMemory, ShmLockRequest, ShmRegion, SHM_LOCK_COUNT};
use crate::error::{self, VfsError, VfsOperation, VfsResult};
use crate::os::platform;
use crate::os::ranges;
use crate::path::DbPath;
use crate::shm_locks::ShmLockTable;

/// What one shared-memory file looks like to this process.
///
/// Byte-range locks are per process on POSIX, so two connections in one process
/// would each be told they hold the WAL write lock. As with the database file's
/// own locks, an in-process table arbitrates first and the kernel lock is then
/// taken on behalf of the process as a whole.
#[derive(Debug, Default)]
struct ShmEntry {
    table: ShmLockTable,
    applied: [SlotMode; SHM_LOCK_COUNT as usize],
}

/// Which kernel lock this process currently holds on one slot.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum SlotMode {
    /// No lock is held on the slot.
    #[default]
    Free,
    /// A read lock is held on the slot.
    Read,
    /// A write lock is held on the slot.
    Write,
}

/// Every shared-memory file this process currently has locks on.
fn shm_registry() -> &'static Mutex<HashMap<FileIdentity, Arc<Mutex<ShmEntry>>>> {
    static REGISTRY: OnceLock<Mutex<HashMap<FileIdentity, Arc<Mutex<ShmEntry>>>>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Returns the shared lock state for one shared-memory file.
fn shm_entry_for(identity: &FileIdentity) -> Arc<Mutex<ShmEntry>> {
    let mut map = lock_or_recover(shm_registry());
    if let Some(existing) = map.get(identity) {
        return Arc::clone(existing);
    }
    let created = Arc::new(Mutex::new(ShmEntry::default()));
    map.insert(identity.clone(), Arc::clone(&created));
    created
}

/// A shared-memory file backed by a real `-shm` file.
#[derive(Debug)]
pub struct FileShm {
    path: DbPath,
    file: Arc<File>,
    entry: Arc<Mutex<ShmEntry>>,
    held: Mutex<[SlotHold; SHM_LOCK_COUNT as usize]>,
    regions: Mutex<Vec<Arc<FileShmRegion>>>,
}

/// What one shared-memory handle holds on one slot, so that closing it
/// releases exactly what it took.
#[derive(Clone, Copy, Debug, Default)]
struct SlotHold {
    readers: u32,
    writer: bool,
}

impl FileShm {
    /// Opens or creates the shared-memory file for a database.
    pub fn open(path: &DbPath) -> VfsResult<Arc<dyn SharedMemory>> {
        let file = FsOpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path.as_path())
            .map_err(|error| VfsError::from_io(VfsOperation::ShmOpen, &error))?;
        let identity = platform::file_identity(&file)?;
        Ok(Arc::new(FileShm {
            path: path.clone(),
            file: Arc::new(file),
            entry: shm_entry_for(&identity),
            held: Mutex::new([SlotHold::default(); SHM_LOCK_COUNT as usize]),
            regions: Mutex::new(Vec::new()),
        }))
    }

    /// Brings the kernel's locks on slots `first..end` in line with the table.
    ///
    /// The desired state per slot is a write lock when a writer holds it, a
    /// read lock when readers do, and nothing when it is free. Releases are
    /// issued before acquisitions so a downgrade never asks for two conflicting
    /// modes on the same byte at once.
    fn apply_kernel_slots(&self, entry: &mut ShmEntry, first: u16, end: u16) -> VfsResult<()> {
        for slot in first..end {
            let index = usize::from(slot);
            let wanted = if entry.table.is_write_locked(slot) {
                SlotMode::Write
            } else if entry.table.readers(slot) > 0 {
                SlotMode::Read
            } else {
                SlotMode::Free
            };
            let Some(applied) = entry.applied.get(index).copied() else {
                continue;
            };
            if wanted == applied {
                continue;
            }
            let offset = ranges::shm_lock_offset(slot);
            if applied != SlotMode::Free {
                platform::unlock_bytes(
                    &self.file,
                    offset,
                    ranges::SHM_LOCK_SIZE,
                    VfsOperation::ShmLock,
                )?;
                if let Some(state) = entry.applied.get_mut(index) {
                    *state = SlotMode::Free;
                }
            }
            if wanted == SlotMode::Free {
                continue;
            }
            let taken = platform::try_lock_bytes(
                &self.file,
                offset,
                ranges::SHM_LOCK_SIZE,
                wanted == SlotMode::Write,
                VfsOperation::ShmLock,
            )?;
            if !taken {
                return Err(error::busy(format!(
                    "another process holds shared-memory slot {slot}"
                )));
            }
            if let Some(state) = entry.applied.get_mut(index) {
                *state = wanted;
            }
        }
        Ok(())
    }

    /// Records what this handle now holds, so that dropping it releases exactly
    /// that and nothing another handle took.
    fn record_hold(&self, request: ShmLockRequest) {
        let mut held = lock_or_recover(&self.held);
        for slot in request.offset..request.offset.saturating_add(request.count) {
            let Some(entry) = held.get_mut(usize::from(slot)) else {
                continue;
            };
            match (request.acquire, request.exclusive) {
                (true, true) => entry.writer = true,
                (true, false) => entry.readers = entry.readers.saturating_add(1),
                (false, true) => entry.writer = false,
                (false, false) => entry.readers = entry.readers.saturating_sub(1),
            }
        }
    }

    /// Grows the file so that region `index` of `region_size` bytes exists.
    fn ensure_length(&self, index: u32, region_size: usize) -> VfsResult<u64> {
        let needed = u64::from(index)
            .checked_add(1)
            .and_then(|count| count.checked_mul(region_size as u64))
            .ok_or_else(|| {
                VfsError::new(VfsOperation::ShmSize.extended_code(), "shm size overflowed")
            })?;
        let current = self
            .file
            .metadata()
            .map_err(|error| VfsError::from_io(VfsOperation::ShmSize, &error))?
            .len();
        if current < needed {
            self.file
                .set_len(needed)
                .map_err(|error| VfsError::from_io(VfsOperation::ShmSize, &error))?;
        }
        Ok(needed)
    }
}

impl SharedMemory for FileShm {
    /// Maps a region, growing the file when asked to.
    fn map(
        &self,
        index: u32,
        region_size: usize,
        extend: bool,
    ) -> VfsResult<Option<Arc<dyn ShmRegion>>> {
        let current = self
            .file
            .metadata()
            .map_err(|error| VfsError::from_io(VfsOperation::ShmSize, &error))?
            .len();
        let needed = u64::from(index)
            .checked_add(1)
            .and_then(|count| count.checked_mul(region_size as u64))
            .ok_or_else(|| {
                VfsError::new(VfsOperation::ShmMap.extended_code(), "shm size overflowed")
            })?;
        if current < needed {
            if !extend {
                return Ok(None);
            }
            self.ensure_length(index, region_size)?;
        }
        let start = u64::from(index).saturating_mul(region_size as u64);
        let region = Arc::new(FileShmRegion {
            file: self
                .file
                .try_clone()
                .map_err(|error| VfsError::from_io(VfsOperation::ShmMap, &error))?,
            start,
            len: region_size,
        });
        let mut regions = lock_or_recover(&self.regions);
        regions.push(Arc::clone(&region));
        Ok(Some(region as Arc<dyn ShmRegion>))
    }

    /// Takes or releases shared-memory lock slots.
    ///
    /// The in-process table decides first so that two connections here exclude
    /// each other, then the kernel byte-range lock on the `-shm` file is brought
    /// in line so that other processes are excluded too.
    fn lock(&self, request: ShmLockRequest) -> VfsResult<()> {
        let end = request
            .offset
            .checked_add(request.count)
            .ok_or_else(|| error::misuse("shared-memory lock range overflowed"))?;
        if request.count == 0 || end > SHM_LOCK_COUNT {
            return Err(error::misuse("shared-memory lock range out of bounds"));
        }
        let mut entry = lock_or_recover(&self.entry);
        entry.table.apply(request)?;
        if let Err(failure) = self.apply_kernel_slots(&mut entry, request.offset, end) {
            let _ = entry.table.apply(ShmLockRequest {
                acquire: !request.acquire,
                ..request
            });
            let _ = self.apply_kernel_slots(&mut entry, request.offset, end);
            return Err(failure);
        }
        self.record_hold(request);
        Ok(())
    }

    /// Orders memory operations. Reads and writes go through the file system,
    /// which already orders them, so this only fences this process's own
    /// compiler and processor reordering.
    fn barrier(&self) {
        std::sync::atomic::fence(std::sync::atomic::Ordering::SeqCst);
    }

    /// Drops the mapping, deleting the file when asked and permitted.
    fn unmap(&self, delete: bool) -> VfsResult<()> {
        let mut regions = lock_or_recover(&self.regions);
        regions.clear();
        if delete {
            match std::fs::remove_file(self.path.as_path()) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(VfsError::from_io(VfsOperation::Delete, &error)),
            }
        }
        Ok(())
    }
}

impl Drop for FileShm {
    /// Releases every slot this handle still holds, so a connection going away
    /// cannot leave the WAL write lock held forever.
    fn drop(&mut self) {
        let held = *lock_or_recover(&self.held);
        let mut entry = lock_or_recover(&self.entry);
        for (index, hold) in held.iter().enumerate() {
            let Ok(slot) = u16::try_from(index) else {
                continue;
            };
            for _ in 0..hold.readers {
                let _ = entry.table.apply(ShmLockRequest {
                    offset: slot,
                    count: 1,
                    acquire: false,
                    exclusive: false,
                });
            }
            if hold.writer {
                let _ = entry.table.apply(ShmLockRequest {
                    offset: slot,
                    count: 1,
                    acquire: false,
                    exclusive: true,
                });
            }
            let _ = self.apply_kernel_slots(&mut entry, slot, slot.saturating_add(1));
        }
    }
}

/// Locks a mutex, recovering from poisoning rather than propagating a panic.
fn lock_or_recover<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    match mutex.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

/// One region of a file-backed shared-memory file.
#[derive(Debug)]
struct FileShmRegion {
    file: File,
    start: u64,
    len: usize,
}

impl ShmRegion for FileShmRegion {
    /// Returns the region's length.
    fn len(&self) -> usize {
        self.len
    }

    /// Copies bytes out of the region.
    fn read(&self, offset: usize, output: &mut [u8]) -> VfsResult<()> {
        self.check_window(offset, output.len())?;
        let at = self.start.saturating_add(offset as u64);
        let mut filled = 0usize;
        while filled < output.len() {
            let Some(target) = output.get_mut(filled..) else {
                break;
            };
            match platform::read_at(&self.file, at.saturating_add(filled as u64), target) {
                Ok(0) => break,
                Ok(count) => filled = filled.saturating_add(count),
                Err(io) if io.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(io) => return Err(VfsError::from_io(VfsOperation::ShmMap, &io)),
            }
        }
        for slot in output.iter_mut().skip(filled) {
            *slot = 0;
        }
        Ok(())
    }

    /// Copies bytes into the region.
    fn write(&self, offset: usize, input: &[u8]) -> VfsResult<()> {
        self.check_window(offset, input.len())?;
        let at = self.start.saturating_add(offset as u64);
        let mut written = 0usize;
        while written < input.len() {
            let Some(source) = input.get(written..) else {
                break;
            };
            match platform::write_at(&self.file, at.saturating_add(written as u64), source) {
                Ok(0) => return Err(error::disk_full("shared-memory write stalled")),
                Ok(count) => written = written.saturating_add(count),
                Err(io) if io.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(io) => return Err(VfsError::from_io(VfsOperation::ShmMap, &io)),
            }
        }
        Ok(())
    }
}

impl FileShmRegion {
    /// Refuses a read or write that reaches outside the region.
    fn check_window(&self, offset: usize, length: usize) -> VfsResult<()> {
        let end = offset.checked_add(length).ok_or_else(|| {
            VfsError::new(
                VfsOperation::ShmMap.extended_code(),
                "shm window overflowed",
            )
        })?;
        if end > self.len {
            return Err(VfsError::new(
                VfsOperation::ShmMap.extended_code(),
                "shm window out of range",
            ));
        }
        Ok(())
    }
}
