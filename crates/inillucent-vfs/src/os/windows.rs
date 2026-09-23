//! The Windows half of the operating-system VFS.
//!
//! Invariant: every `unsafe` block here has its safety argument written above
//! it, and none of them holds a raw pointer past the call it was made for.
//!
//! Windows byte-range locks are per *handle*, not per process, so two handles
//! in one process conflict exactly as two processes do. That is the semantics
//! the locking protocol wants, so the Windows implementation goes straight to
//! the kernel with no in-process bookkeeping; the POSIX implementation cannot,
//! and says why in its own file.

use std::fs::File;
use std::io;
use std::os::windows::fs::FileExt;
use std::os::windows::io::AsRawHandle;
use std::sync::Arc;
use std::sync::Mutex;

use windows_sys::Win32::Foundation::CloseHandle;
use windows_sys::Win32::Foundation::HANDLE;
use windows_sys::Win32::Security::Cryptography::{
    BCryptGenRandom, BCRYPT_USE_SYSTEM_PREFERRED_RNG,
};
use windows_sys::Win32::Storage::FileSystem::{
    GetFileInformationByHandle, LockFileEx, UnlockFileEx, BY_HANDLE_FILE_INFORMATION,
    LOCKFILE_EXCLUSIVE_LOCK, LOCKFILE_FAIL_IMMEDIATELY,
};
use windows_sys::Win32::System::Memory::{
    CreateFileMappingW, MapViewOfFile, UnmapViewOfFile, FILE_MAP_READ, FILE_MAP_WRITE,
    PAGE_READWRITE,
};
use windows_sys::Win32::System::SystemInformation::{GetSystemInfo, SYSTEM_INFO};
use windows_sys::Win32::System::IO::OVERLAPPED;

use crate::contract::{DeviceCharacteristics, FileIdentity, FileLock, SharedMemory};
use crate::error::{self, VfsError, VfsOperation, VfsResult};
use crate::locks::HandleId;
use crate::os::filelock::FileShm;
use crate::os::ranges::{PENDING_BYTE, RESERVED_BYTE, SHARED_FIRST, SHARED_SIZE};
use crate::path::DbPath;

/// The name this VFS registers under.
pub const VFS_NAME: &str = "win32";

/// Reads at an absolute offset without disturbing logical file position use.
pub fn read_at(file: &File, offset: u64, output: &mut [u8]) -> io::Result<usize> {
    file.seek_read(output, offset)
}

/// Writes at an absolute offset.
pub fn write_at(file: &File, offset: u64, input: &[u8]) -> io::Result<usize> {
    file.seek_write(input, offset)
}

/// Returns the volume serial number and file index that identify a file.
pub fn file_identity(file: &File) -> VfsResult<FileIdentity> {
    // SAFETY: BY_HANDLE_FILE_INFORMATION is plain data with no invalid bit
    // patterns, so an all-zero value is a valid one; every field the caller
    // reads below is written by the call that follows.
    let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
    // SAFETY: `info` is a properly aligned, fully owned structure of the exact
    // type the call expects, and the handle is valid for the life of `file`.
    let ok = unsafe { GetFileInformationByHandle(file.as_raw_handle() as HANDLE, &mut info) };
    if ok == 0 {
        return Err(VfsError::from_io(
            VfsOperation::FileSize,
            &io::Error::last_os_error(),
        ));
    }
    let index = (u128::from(info.nFileIndexHigh) << 32) | u128::from(info.nFileIndexLow);
    Ok(FileIdentity {
        volume: u64::from(info.dwVolumeSerialNumber),
        file: index,
    })
}

/// Returns what a Windows file system guarantees.
///
/// Nothing is claimed, and in particular `undeletable_when_open` is *not*.
/// SQLite's own Windows VFS does declare it, because it opens files without
/// `FILE_SHARE_DELETE`; the Rust standard library opens them with it, so a file
/// here really can be unlinked while a handle is open. The conformance suite's
/// `vfs.device.claims-are-true` case caught the optimistic version of this
/// declaration, which is exactly what that case is for: a capability the
/// durability code takes a shortcut on has to be probed, not assumed.
pub fn device_characteristics() -> DeviceCharacteristics {
    DeviceCharacteristics::conservative()
}

/// Flushing a directory is not a Windows concept; NTFS journals the metadata
/// change that a create or delete makes, so there is nothing to force.
pub fn sync_directory(_path: &DbPath) -> VfsResult<()> {
    Ok(())
}

/// Flushes a file the strongest way this platform can.
///
/// `FlushFileBuffers`, which is what `sync_all` calls and which Windows
/// documents as reaching the disk. There is no second, stronger barrier the
/// way Darwin's `F_FULLFSYNC` is stronger than its `fsync` - see
/// `os::unix::full_sync`.
///
/// @param file - the file to flush
pub fn full_sync(file: &File) -> std::io::Result<()> {
    file.sync_all()
}

/// Fills `output` with randomness from the system preferred generator.
pub fn system_randomness(output: &mut [u8]) -> VfsResult<()> {
    if output.is_empty() {
        return Ok(());
    }
    let length = u32::try_from(output.len())
        .map_err(|_| error::misuse("randomness request is larger than a single call allows"))?;
    // SAFETY: the buffer is valid for `length` bytes for the duration of the
    // call, and passing a null algorithm handle with
    // BCRYPT_USE_SYSTEM_PREFERRED_RNG is the documented way to ask for the
    // system generator without opening one.
    let status = unsafe {
        BCryptGenRandom(
            std::ptr::null_mut(),
            output.as_mut_ptr(),
            length,
            BCRYPT_USE_SYSTEM_PREFERRED_RNG,
        )
    };
    if status != 0 {
        return Err(error::misuse(format!(
            "BCryptGenRandom failed with 0x{status:08x}"
        )));
    }
    Ok(())
}

/// Builds the overlapped structure that carries a lock's start offset.
fn overlapped_at(offset: u64) -> OVERLAPPED {
    // SAFETY: OVERLAPPED is a plain data structure with no invalid bit patterns
    // for the fields the call reads; every field it reads is set here, and the
    // union is written rather than read, which is what makes zeroing sound.
    unsafe {
        let mut overlapped: OVERLAPPED = std::mem::zeroed();
        overlapped.Anonymous.Anonymous.Offset = (offset & 0xffff_ffff) as u32;
        overlapped.Anonymous.Anonymous.OffsetHigh = (offset >> 32) as u32;
        overlapped
    }
}

/// Tries to take a byte-range lock, returning false when another holder has it.
pub fn try_lock_bytes(
    file: &File,
    start: u64,
    len: u64,
    exclusive: bool,
    operation: VfsOperation,
) -> VfsResult<bool> {
    let mut overlapped = overlapped_at(start);
    let mut flags = LOCKFILE_FAIL_IMMEDIATELY;
    if exclusive {
        flags |= LOCKFILE_EXCLUSIVE_LOCK;
    }
    // SAFETY: the handle is valid for the life of `file`, and `overlapped`
    // lives across the call and is not aliased.
    let ok = unsafe {
        LockFileEx(
            file.as_raw_handle() as HANDLE,
            flags,
            0,
            (len & 0xffff_ffff) as u32,
            (len >> 32) as u32,
            &mut overlapped,
        )
    };
    if ok != 0 {
        return Ok(true);
    }
    let error = io::Error::last_os_error();
    match error.raw_os_error() {
        // ERROR_LOCK_VIOLATION and ERROR_IO_PENDING both mean "someone else has
        // it"; with FAIL_IMMEDIATELY the pending case is reported rather than
        // waited on.
        Some(33) | Some(997) => Ok(false),
        _ => Err(VfsError::from_io(operation, &error)),
    }
}

/// Releases a byte-range lock. Releasing a range that is not held is not an
/// error, because unlocking is used on paths that do not track every level.
pub fn unlock_bytes(file: &File, start: u64, len: u64, operation: VfsOperation) -> VfsResult<()> {
    let mut overlapped = overlapped_at(start);
    // SAFETY: as for `try_lock_bytes`.
    let ok = unsafe {
        UnlockFileEx(
            file.as_raw_handle() as HANDLE,
            0,
            (len & 0xffff_ffff) as u32,
            (len >> 32) as u32,
            &mut overlapped,
        )
    };
    if ok != 0 {
        return Ok(());
    }
    let error = io::Error::last_os_error();
    match error.raw_os_error() {
        // ERROR_NOT_LOCKED
        Some(158) => Ok(()),
        _ => Err(VfsError::from_io(operation, &error)),
    }
}

/// Opens the shared-memory file for a database.
pub fn open_shm(path: &DbPath) -> VfsResult<Arc<dyn SharedMemory>> {
    FileShm::open(path)
}

/// A window onto a file that other processes see the same bytes of.
///
/// This is what makes the wal-index shared memory rather than a file two
/// processes happen to be reading: a store here is visible to every other
/// mapping of the same pages without a system call, which is what the WAL
/// protocol's barriers are ordering. It also sidesteps a Windows rule that
/// makes the file-I/O version impossible: a shared byte-range lock forbids
/// *writes* to the locked range even from the handle that took it, and the
/// wal-index deliberately stores a counter at the same byte the dead-man
/// switch is locked on. A mapped store is not a write in that sense, which is
/// exactly why SQLite maps this file too.
#[derive(Debug)]
pub struct SharedMapping {
    /// The address the view begins at, which may precede the caller's window
    /// because a view has to start on an allocation-granularity boundary.
    view: *mut u8,
    /// How many bytes the view covers, for unmapping and for bounds checks.
    view_len: usize,
    /// Where the caller's window starts within the view.
    offset: usize,
    /// How long the caller's window is.
    len: usize,
}

// SAFETY: the pointer is a mapping of a shared file view, which is valid for
// the life of this value on any thread; the type hands out no references to it
// and every access goes through the bounds-checked methods below.
unsafe impl Send for SharedMapping {}
// SAFETY: as above. Concurrent access is the point of shared memory, and the
// callers order their stores with the barrier the shared-memory contract
// provides rather than relying on Rust's aliasing rules, which do not describe
// memory another process is writing.
unsafe impl Sync for SharedMapping {}

impl SharedMapping {
    /// Copies bytes out of the window.
    pub fn read(&self, offset: usize, output: &mut [u8]) -> VfsResult<()> {
        let start = self.window(offset, output.len())?;
        // SAFETY: `window` has proved the range lies inside the mapped view,
        // the pointer is aligned for bytes, and `output` cannot overlap it
        // because it is a Rust-owned slice and this mapping hands out no
        // references into itself.
        unsafe {
            std::ptr::copy_nonoverlapping(self.view.add(start), output.as_mut_ptr(), output.len());
        }
        Ok(())
    }

    /// Copies bytes into the window.
    pub fn write(&self, offset: usize, input: &[u8]) -> VfsResult<()> {
        let start = self.window(offset, input.len())?;
        // SAFETY: as in `read`, with the direction reversed.
        unsafe {
            std::ptr::copy_nonoverlapping(input.as_ptr(), self.view.add(start), input.len());
        }
        Ok(())
    }

    /// Returns where a window of `len` bytes at `offset` starts in the view.
    fn window(&self, offset: usize, len: usize) -> VfsResult<usize> {
        let end = offset
            .checked_add(len)
            .ok_or_else(|| error::misuse("a shared-memory window overflowed"))?;
        if end > self.len {
            return Err(error::misuse(
                "a shared-memory access ran past the end of its region",
            ));
        }
        let start = self
            .offset
            .checked_add(offset)
            .filter(|start| start.saturating_add(len) <= self.view_len)
            .ok_or_else(|| error::misuse("a shared-memory window left its view"))?;
        Ok(start)
    }
}

impl Drop for SharedMapping {
    /// Releases the view.
    fn drop(&mut self) {
        // SAFETY: the pointer came from `MapViewOfFile` and has not been
        // unmapped before, because only this value owns it and it is dropped
        // once.
        unsafe {
            UnmapViewOfFile(
                windows_sys::Win32::System::Memory::MEMORY_MAPPED_VIEW_ADDRESS {
                    Value: self.view.cast(),
                },
            );
        }
    }
}

/// Maps `len` bytes of `file` starting at `offset` into this process.
///
/// The view is taken from the allocation-granularity boundary at or below
/// `offset`, because Windows refuses any other starting point, and the window
/// the caller asked for is recorded as an offset into it.
pub fn map_shared(file: &File, offset: u64, len: usize) -> VfsResult<SharedMapping> {
    let granularity = allocation_granularity();
    let aligned = offset - (offset % granularity);
    let delta = usize::try_from(offset - aligned)
        .map_err(|_| error::misuse("a shared-memory offset did not fit in memory"))?;
    let view_len = delta
        .checked_add(len)
        .ok_or_else(|| error::misuse("a shared-memory view overflowed"))?;
    // SAFETY: the handle is valid for the life of `file`, a null security
    // descriptor and a null name are documented as "default, unnamed", and a
    // zero size means "as large as the file", which the caller has already
    // grown to cover the region.
    let mapping = unsafe {
        CreateFileMappingW(
            file.as_raw_handle() as HANDLE,
            std::ptr::null(),
            PAGE_READWRITE,
            0,
            0,
            std::ptr::null(),
        )
    };
    if mapping.is_null() {
        return Err(VfsError::from_io(
            VfsOperation::ShmMap,
            &io::Error::last_os_error(),
        ));
    }
    // SAFETY: `mapping` is a valid mapping handle, the offset is aligned to the
    // allocation granularity as the call requires, and the length lies within
    // the file the mapping was made from.
    let view = unsafe {
        MapViewOfFile(
            mapping,
            FILE_MAP_READ | FILE_MAP_WRITE,
            (aligned >> 32) as u32,
            (aligned & 0xffff_ffff) as u32,
            view_len,
        )
    };
    // The mapping handle is not needed once a view exists: the view keeps the
    // mapping alive, and leaving the handle open would leak one per region.
    // SAFETY: `mapping` is a handle this function created and has not closed.
    unsafe {
        CloseHandle(mapping);
    }
    if view.Value.is_null() {
        return Err(VfsError::from_io(
            VfsOperation::ShmMap,
            &io::Error::last_os_error(),
        ));
    }
    Ok(SharedMapping {
        view: view.Value.cast(),
        view_len,
        offset: delta,
        len,
    })
}

/// Returns the boundary a mapped view has to start on.
fn allocation_granularity() -> u64 {
    // SAFETY: SYSTEM_INFO is plain data with no invalid bit patterns, and the
    // call fills every field this function reads.
    let mut info: SYSTEM_INFO = unsafe { std::mem::zeroed() };
    // SAFETY: `info` is a properly aligned owned structure of the exact type
    // the call expects.
    unsafe { GetSystemInfo(&mut info) };
    u64::from(info.dwAllocationGranularity.max(1))
}

/// The lock level one handle holds, and the transitions between levels.
#[derive(Debug)]
pub struct LockState {
    level: Mutex<FileLock>,
}

impl LockState {
    /// Creates the state for a freshly opened handle.
    ///
    /// The identity, handle number, and descriptor are unused on Windows
    /// because the kernel already distinguishes handles and closing one does
    /// not disturb another's locks; they are taken so that both platforms
    /// present the same constructor to `os::mod`.
    pub fn new(_identity: FileIdentity, _handle: HandleId, _descriptor: Arc<File>) -> LockState {
        LockState {
            level: Mutex::new(FileLock::None),
        }
    }

    /// Returns the level this handle holds.
    pub fn level(&self) -> FileLock {
        *guard(&self.level)
    }

    /// Raises the lock to `target`, one protocol step at a time.
    pub fn acquire(&self, file: &File, target: FileLock) -> VfsResult<()> {
        let mut level = guard(&self.level);
        if target <= *level {
            return Ok(());
        }
        if *level == FileLock::None {
            take_shared(file)?;
            *level = FileLock::Shared;
        }
        if target == FileLock::Reserved && *level < FileLock::Reserved {
            if !try_lock_bytes(file, RESERVED_BYTE, 1, true, VfsOperation::Lock)? {
                return Err(error::busy("another connection holds RESERVED"));
            }
            *level = FileLock::Reserved;
        }
        if target >= FileLock::Pending && *level < FileLock::Pending {
            if !try_lock_bytes(file, PENDING_BYTE, 1, true, VfsOperation::Lock)? {
                return Err(error::busy("another connection holds PENDING"));
            }
            *level = FileLock::Pending;
        }
        if target == FileLock::Exclusive && *level < FileLock::Exclusive {
            promote_to_exclusive(file)?;
            *level = FileLock::Exclusive;
        }
        Ok(())
    }

    /// Lowers the lock to `target`, which must be `Shared` or `None`.
    ///
    /// **A handle at SHARED holds neither RESERVED nor PENDING, so releasing it
    /// unlocks one byte range and not three** (task-2046). RESERVED is taken
    /// only on the way to RESERVED, and `take_shared` gives PENDING back before
    /// it returns, so unlocking both on the way down from SHARED was two
    /// `UnlockFileEx` calls that answered `ERROR_NOT_LOCKED` and were swallowed.
    /// That is two of the six byte-range calls an ordinary statement made:
    /// under `locking_mode = normal` every statement outside a transaction
    /// takes the file and gives it back, and `leave` was 6.8 us of the 21.5 us
    /// such a statement cost once the meta record was no longer being reread.
    ///
    /// A handle above SHARED still unlocks both, whether or not it took them.
    /// `acquire` reaches EXCLUSIVE from SHARED without passing through
    /// RESERVED when a caller asks for it directly, so "the level says it is
    /// held" is not true there - and an unlock of a range nobody holds is the
    /// harmless call this is removing from the path where it is provably
    /// pointless, rather than a thing to reason about per level.
    pub fn release(&self, file: &File, target: FileLock) -> VfsResult<()> {
        let mut level = guard(&self.level);
        if target >= *level {
            return Ok(());
        }
        // The line above has already returned for a target at or above the
        // level held, so a handle at SHARED that reaches here is going to NONE
        // and the read range is the one range it holds.
        if *level == FileLock::Shared {
            unlock_bytes(file, SHARED_FIRST, SHARED_SIZE, VfsOperation::Unlock)?;
            *level = target;
            return Ok(());
        }
        if *level == FileLock::Exclusive {
            unlock_bytes(file, SHARED_FIRST, SHARED_SIZE, VfsOperation::Unlock)?;
            if target == FileLock::Shared
                && !try_lock_bytes(file, SHARED_FIRST, SHARED_SIZE, false, VfsOperation::Unlock)?
            {
                return Err(error::busy(
                    "cannot retake the read lock while dropping down",
                ));
            }
        }
        unlock_bytes(file, RESERVED_BYTE, 1, VfsOperation::Unlock)?;
        unlock_bytes(file, PENDING_BYTE, 1, VfsOperation::Unlock)?;
        if target == FileLock::None && *level != FileLock::Exclusive {
            unlock_bytes(file, SHARED_FIRST, SHARED_SIZE, VfsOperation::Unlock)?;
        }
        *level = target;
        Ok(())
    }

    /// Reports whether another handle holds RESERVED or stronger.
    pub fn check_reserved(&self, file: &File) -> VfsResult<bool> {
        if *guard(&self.level) >= FileLock::Reserved {
            return Ok(false);
        }
        if try_lock_bytes(
            file,
            RESERVED_BYTE,
            1,
            true,
            VfsOperation::CheckReservedLock,
        )? {
            unlock_bytes(file, RESERVED_BYTE, 1, VfsOperation::CheckReservedLock)?;
            return Ok(false);
        }
        Ok(true)
    }
}

/// How many times a reader retries the PENDING byte before reporting BUSY.
///
/// Windows byte-range locks have no shared mode that two readers can take on
/// the same byte at once, so two connections acquiring SHARED at the same
/// moment collide on the serialising PENDING byte even though neither is a
/// writer. SQLite's own Windows VFS makes exactly three attempts with a
/// millisecond between them for this reason; reporting BUSY on the first
/// collision would turn ordinary read concurrency into a spurious failure.
const PENDING_ATTEMPTS: u32 = 3;

/// Takes the read lock, using the PENDING byte to serialise the two steps so
/// that a writer cannot slip in between them.
fn take_shared(file: &File) -> VfsResult<()> {
    let mut took_pending = false;
    for attempt in 0..PENDING_ATTEMPTS {
        if try_lock_bytes(file, PENDING_BYTE, 1, true, VfsOperation::Lock)? {
            took_pending = true;
            break;
        }
        if attempt + 1 < PENDING_ATTEMPTS {
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
    }
    if !took_pending {
        return Err(error::busy("a writer holds PENDING"));
    }
    let took_read = try_lock_bytes(file, SHARED_FIRST, SHARED_SIZE, false, VfsOperation::Lock)?;
    unlock_bytes(file, PENDING_BYTE, 1, VfsOperation::Lock)?;
    if !took_read {
        return Err(error::busy("a writer holds the read range"));
    }
    Ok(())
}

/// Swaps the shared read lock for an exclusive one, putting the read lock back
/// if the swap fails so the caller keeps the level it had.
fn promote_to_exclusive(file: &File) -> VfsResult<()> {
    unlock_bytes(file, SHARED_FIRST, SHARED_SIZE, VfsOperation::Lock)?;
    if try_lock_bytes(file, SHARED_FIRST, SHARED_SIZE, true, VfsOperation::Lock)? {
        return Ok(());
    }
    let _ = try_lock_bytes(file, SHARED_FIRST, SHARED_SIZE, false, VfsOperation::Lock)?;
    Err(error::busy("readers are still present"))
}

/// Locks a mutex, recovering from poisoning rather than propagating a panic.
fn guard<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    match mutex.lock() {
        Ok(inner) => inner,
        Err(poisoned) => poisoned.into_inner(),
    }
}
