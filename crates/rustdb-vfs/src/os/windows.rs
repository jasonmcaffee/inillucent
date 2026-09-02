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

use windows_sys::Win32::Foundation::HANDLE;
use windows_sys::Win32::Security::Cryptography::{
    BCryptGenRandom, BCRYPT_USE_SYSTEM_PREFERRED_RNG,
};
use windows_sys::Win32::Storage::FileSystem::{
    GetFileInformationByHandle, LockFileEx, UnlockFileEx, BY_HANDLE_FILE_INFORMATION,
    LOCKFILE_EXCLUSIVE_LOCK, LOCKFILE_FAIL_IMMEDIATELY,
};
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
    pub fn release(&self, file: &File, target: FileLock) -> VfsResult<()> {
        let mut level = guard(&self.level);
        if target >= *level {
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

/// Takes the read lock, using the PENDING byte to serialise the two steps so
/// that a writer cannot slip in between them.
fn take_shared(file: &File) -> VfsResult<()> {
    if !try_lock_bytes(file, PENDING_BYTE, 1, true, VfsOperation::Lock)? {
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
