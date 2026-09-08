//! The POSIX half of the operating-system VFS.
//!
//! Invariant: every `unsafe` block here has its safety argument written above
//! it, and no raw pointer outlives the call it was made for.
//!
//! POSIX advisory locks have two properties that make them dangerous, and both
//! are handled here rather than being left for a caller to trip over.
//!
//! First, they are held per *process*, not per file descriptor: two connections
//! in one process would each be told they hold the write lock. The in-process
//! registry below therefore arbitrates between handles on the same file first,
//! using the same `LockTable` the in-memory VFS uses, and only then asks the
//! kernel on behalf of the process as a whole.
//!
//! Second, closing *any* descriptor for a file drops *all* of that process's
//! locks on it. The registry keeps one descriptor per open file identity alive
//! for as long as any handle needs it, so a connection closing its own file
//! cannot silently unlock another connection's database.

use std::collections::HashMap;
use std::fs::File;
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::fs::{FileExt, MetadataExt};
use std::sync::{Arc, Mutex, OnceLock};

use crate::contract::{DeviceCharacteristics, FileIdentity, FileLock, SharedMemory};
use crate::error::{self, VfsError, VfsOperation, VfsResult};
use crate::locks::{next_step, HandleId, LockConflict, LockTable};
use crate::os::filelock::FileShm;
use crate::os::ranges::{PENDING_BYTE, RESERVED_BYTE, SHARED_FIRST, SHARED_SIZE};
use crate::path::DbPath;

/// The name this VFS registers under.
pub const VFS_NAME: &str = "unix";

/// A window onto a file that other processes see the same bytes of.
///
/// This is what makes the wal-index shared memory rather than a file two
/// processes happen to be reading: a store here is visible to every other
/// mapping of the same pages without a system call, which is what the WAL
/// protocol's barriers are ordering. SQLite maps this file for the same
/// reason, and mapping it is also what lets a byte carry both a lock and a
/// counter, which the wal-index format requires of the dead-man switch.
#[derive(Debug)]
pub struct SharedMapping {
    /// The address the mapping begins at, which may precede the caller's
    /// window because a mapping has to start on a page boundary.
    view: *mut u8,
    /// How many bytes are mapped, for unmapping and for bounds checks.
    view_len: usize,
    /// Where the caller's window starts within the mapping.
    offset: usize,
    /// How long the caller's window is.
    len: usize,
}

// SAFETY: the pointer is a shared file mapping, valid for the life of this
// value on any thread; the type hands out no references to it and every access
// goes through the bounds-checked methods below.
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
        // SAFETY: `window` has proved the range lies inside the mapping, the
        // pointer is aligned for bytes, and `output` cannot overlap it because
        // it is a Rust-owned slice and this mapping hands out no references
        // into itself.
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

    /// Returns where a window of `len` bytes at `offset` starts in the mapping.
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
            .ok_or_else(|| error::misuse("a shared-memory window left its mapping"))?;
        Ok(start)
    }
}

impl Drop for SharedMapping {
    /// Releases the mapping.
    fn drop(&mut self) {
        // SAFETY: the pointer and length came from the `mmap` below and have
        // not been unmapped before, because only this value owns them and it is
        // dropped once.
        unsafe {
            libc::munmap(self.view.cast(), self.view_len);
        }
    }
}

/// Maps `len` bytes of `file` starting at `offset` into this process.
///
/// The mapping is taken from the page boundary at or below `offset`, because
/// `mmap` refuses any other starting point, and the window the caller asked for
/// is recorded as an offset into it.
pub fn map_shared(file: &File, offset: u64, len: usize) -> VfsResult<SharedMapping> {
    let page = page_size();
    let aligned = offset - (offset % page);
    let delta = usize::try_from(offset - aligned)
        .map_err(|_| error::misuse("a shared-memory offset did not fit in memory"))?;
    let view_len = delta
        .checked_add(len)
        .ok_or_else(|| error::misuse("a shared-memory mapping overflowed"))?;
    let raw_offset = libc::off_t::try_from(aligned)
        .map_err(|_| error::misuse("a shared-memory offset did not fit an off_t"))?;
    // SAFETY: a null address asks the kernel to choose one, the descriptor is
    // valid for the life of `file`, and the caller has already grown the file
    // to cover the region being mapped.
    let view = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            view_len,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_SHARED,
            file.as_raw_fd(),
            raw_offset,
        )
    };
    if view == libc::MAP_FAILED {
        return Err(VfsError::from_io(
            VfsOperation::ShmMap,
            &io::Error::last_os_error(),
        ));
    }
    Ok(SharedMapping {
        view: view.cast(),
        view_len,
        offset: delta,
        len,
    })
}

/// Returns the boundary a mapping has to start on.
fn page_size() -> u64 {
    // SAFETY: `sysconf` takes an integer and returns one; it has no pointer
    // arguments and no failure mode this call has to distinguish.
    let reported = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    u64::try_from(reported).unwrap_or(4096).max(1)
}

/// Reads at an absolute offset.
pub fn read_at(file: &File, offset: u64, output: &mut [u8]) -> io::Result<usize> {
    file.read_at(output, offset)
}

/// Writes at an absolute offset.
pub fn write_at(file: &File, offset: u64, input: &[u8]) -> io::Result<usize> {
    file.write_at(input, offset)
}

/// Returns the device and inode numbers that identify a file.
pub fn file_identity(file: &File) -> VfsResult<FileIdentity> {
    let metadata = file
        .metadata()
        .map_err(|error| VfsError::from_io(VfsOperation::FileSize, &error))?;
    Ok(FileIdentity {
        volume: metadata.dev(),
        file: u128::from(metadata.ino()),
    })
}

/// Returns what a POSIX file system guarantees.
///
/// Nothing is claimed. A file on Linux can be unlinked while open, writes are
/// not ordered without a sync, and no write size is atomic across a power loss
/// on a general filesystem. Anything better has to be probed for before it is
/// declared, which is a phase 10 concern rather than a phase 1 one.
pub fn device_characteristics() -> DeviceCharacteristics {
    DeviceCharacteristics::conservative()
}

/// Flushes the directory that contains a path, so that a create or delete
/// inside it survives a power loss.
pub fn sync_directory(path: &DbPath) -> VfsResult<()> {
    let directory = File::open(path.as_path())
        .map_err(|error| VfsError::from_io(VfsOperation::DirSync, &error))?;
    directory
        .sync_all()
        .map_err(|error| VfsError::from_io(VfsOperation::DirSync, &error))
}

/// Fills `output` with randomness from the kernel.
pub fn system_randomness(output: &mut [u8]) -> VfsResult<()> {
    if output.is_empty() {
        return Ok(());
    }
    let source = File::open("/dev/urandom")
        .map_err(|error| VfsError::from_io(VfsOperation::Read, &error))?;
    let mut filled = 0usize;
    while filled < output.len() {
        let Some(target) = output.get_mut(filled..) else {
            break;
        };
        match source.read_at(target, 0) {
            Ok(0) => return Err(error::misuse("/dev/urandom returned no bytes")),
            Ok(count) => filled = filled.saturating_add(count),
            Err(io) if io.kind() == io::ErrorKind::Interrupted => continue,
            Err(io) => return Err(VfsError::from_io(VfsOperation::Read, &io)),
        }
    }
    Ok(())
}

/// Applies one `fcntl` byte-range lock, returning false when it conflicts.
pub fn try_lock_bytes(
    file: &File,
    start: u64,
    len: u64,
    exclusive: bool,
    operation: VfsOperation,
) -> VfsResult<bool> {
    // `F_WRLCK` and friends are `c_int` on Linux and `c_short` on macOS, so the cast is what makes
    // one signature serve both rather than only the platform it was written on.
    let kind = if exclusive {
        libc::F_WRLCK as libc::c_short
    } else {
        libc::F_RDLCK as libc::c_short
    };
    match set_lock(file, start, len, kind) {
        Ok(()) => Ok(true),
        Err(error) if is_conflict(&error) => Ok(false),
        Err(error) => Err(VfsError::from_io(operation, &error)),
    }
}

/// Releases an `fcntl` byte-range lock.
pub fn unlock_bytes(file: &File, start: u64, len: u64, operation: VfsOperation) -> VfsResult<()> {
    set_lock(file, start, len, libc::F_UNLCK as libc::c_short)
        .map_err(|error| VfsError::from_io(operation, &error))
}

/// Reports whether an error means another holder has the range.
fn is_conflict(error: &io::Error) -> bool {
    matches!(error.raw_os_error(), Some(code) if code == libc::EACCES || code == libc::EAGAIN)
}

/// Issues one non-blocking `F_SETLK`.
fn set_lock(file: &File, start: u64, len: u64, kind: libc::c_short) -> io::Result<()> {
    // SAFETY: `flock` is a plain C structure whose every field is set below;
    // zeroing it first gives the padding a defined value.
    let mut lock: libc::flock = unsafe { std::mem::zeroed() };
    lock.l_type = kind;
    lock.l_whence = libc::SEEK_SET as libc::c_short;
    lock.l_start = start as libc::off_t;
    lock.l_len = len as libc::off_t;
    // SAFETY: the descriptor is valid for the life of `file`, and `lock` lives
    // across the call and is not aliased.
    let result = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_SETLK, &mut lock) };
    if result == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Opens the shared-memory file for a database.
pub fn open_shm(path: &DbPath) -> VfsResult<Arc<dyn SharedMemory>> {
    FileShm::open(path)
}

/// Which kernel byte-range locks this process currently holds on one file.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct KernelLocks {
    shared_read: bool,
    shared_write: bool,
    reserved: bool,
    pending: bool,
}

/// What one open file identity looks like to this process.
#[derive(Debug)]
struct InodeLocks {
    table: LockTable,
    applied: KernelLocks,
    /// Descriptors whose owners have gone away while this process still holds
    /// locks on the file.
    ///
    /// Closing *any* descriptor for a file drops every lock this process holds
    /// on it, so a connection closing its own file would silently unlock
    /// another connection's database. Keeping the descriptor alive until the
    /// file is unlocked is the only way to avoid that; SQLite solves it the
    /// same way, with a pending-close list.
    deferred_closes: Vec<Arc<File>>,
}

impl InodeLocks {
    /// Closes any descriptor that was kept alive only to protect locks, once
    /// there are no locks left to protect.
    fn drain_deferred_closes(&mut self) {
        if self.table.is_unlocked() {
            self.deferred_closes.clear();
        }
    }
}

/// Every file identity this process currently has locks on.
fn registry() -> &'static Mutex<HashMap<FileIdentity, Arc<Mutex<InodeLocks>>>> {
    static REGISTRY: OnceLock<Mutex<HashMap<FileIdentity, Arc<Mutex<InodeLocks>>>>> =
        OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Returns the shared lock state for one file identity, creating it on first
/// use.
fn entry_for(identity: &FileIdentity) -> Arc<Mutex<InodeLocks>> {
    let mut map = guard(registry());
    if let Some(existing) = map.get(identity) {
        return Arc::clone(existing);
    }
    let created = Arc::new(Mutex::new(InodeLocks {
        table: LockTable::new(),
        applied: KernelLocks::default(),
        deferred_closes: Vec::new(),
    }));
    map.insert(identity.clone(), Arc::clone(&created));
    created
}

/// The lock level one handle holds, arbitrated against its siblings.
#[derive(Debug)]
pub struct LockState {
    handle: HandleId,
    entry: Arc<Mutex<InodeLocks>>,
    level: Mutex<FileLock>,
    descriptor: Arc<File>,
}

impl LockState {
    /// Creates the state for a freshly opened handle.
    ///
    /// The descriptor is held as an `Arc` so that the handle going away does
    /// not necessarily close it; see `InodeLocks::deferred_closes`.
    pub fn new(identity: FileIdentity, handle: HandleId, descriptor: Arc<File>) -> LockState {
        LockState {
            handle,
            entry: entry_for(&identity),
            level: Mutex::new(FileLock::None),
            descriptor,
        }
    }

    /// Returns the level this handle holds.
    pub fn level(&self) -> FileLock {
        *guard(&self.level)
    }

    /// Raises the lock to `target`.
    ///
    /// The in-process table decides first, so a second connection in this
    /// process is refused exactly as another process would be. Only when the
    /// table agrees does the kernel lock change, and a kernel refusal rolls the
    /// table back so the two never disagree.
    pub fn acquire(&self, file: &File, target: FileLock) -> VfsResult<()> {
        let mut level = guard(&self.level);
        if target <= *level {
            return Ok(());
        }
        let mut entry = guard(&self.entry);
        let mut step = *level;
        let mut refusal = None;
        // Each protocol step is committed to the kernel before the next one is
        // attempted, so a step that another *process* refuses rolls back only
        // itself. A writer that asked for EXCLUSIVE and got as far as PENDING
        // must keep PENDING, or new readers stream in and it never gets to
        // write; SQLite's own lock routine stops at PENDING for that reason.
        while step < target {
            let next = next_step(step, target);
            match entry.table.acquire(self.handle, step, next) {
                Ok(()) => {}
                Err(LockConflict::Busy) => {
                    refusal = Some(error::busy(format!("cannot raise to {next:?}")));
                    break;
                }
                Err(LockConflict::Protocol) => {
                    refusal = Some(error::misuse(format!(
                        "illegal transition {step:?} -> {next:?}"
                    )));
                    break;
                }
            }
            if let Err(failure) = apply_kernel_locks(file, &mut entry) {
                rollback(&mut entry, self.handle, next, step);
                let _ = apply_kernel_locks(file, &mut entry);
                refusal = Some(failure);
                break;
            }
            step = next;
        }
        *level = step;
        match refusal {
            Some(failure) => Err(failure),
            None => Ok(()),
        }
    }

    /// Lowers the lock to `target`.
    pub fn release(&self, file: &File, target: FileLock) -> VfsResult<()> {
        let mut level = guard(&self.level);
        if target >= *level {
            return Ok(());
        }
        let mut entry = guard(&self.entry);
        entry
            .table
            .release(self.handle, *level, target)
            .map_err(|_| error::misuse(format!("illegal release {:?} -> {target:?}", *level)))?;
        apply_kernel_locks(file, &mut entry)?;
        *level = target;
        Ok(())
    }

    /// Reports whether another holder, here or in another process, holds
    /// RESERVED or stronger.
    pub fn check_reserved(&self, file: &File) -> VfsResult<bool> {
        let entry = guard(&self.entry);
        if entry.table.has_reserved_or_stronger(self.handle) {
            return Ok(true);
        }
        if *guard(&self.level) >= FileLock::Reserved {
            return Ok(false);
        }
        drop(entry);
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

impl Drop for LockState {
    /// Releases whatever the handle still held, so a dropped file cannot leave
    /// the registry claiming a lock forever, and defers closing the descriptor
    /// when other handles still hold locks on the same file.
    fn drop(&mut self) {
        let mut entry = guard(&self.entry);
        entry.table.release_all(self.handle);
        let _ = apply_kernel_locks(&self.descriptor, &mut entry);
        if entry.table.is_unlocked() {
            entry.drain_deferred_closes();
        } else {
            entry.deferred_closes.push(Arc::clone(&self.descriptor));
        }
    }
}

/// Puts the table back to the level a handle had before a failed step.
fn rollback(entry: &mut InodeLocks, handle: HandleId, from: FileLock, to: FileLock) {
    entry.table.undo_step(handle, from, to);
}

/// Returns the kernel locks this process needs, given what its handles hold.
///
/// The process holds the union of its handles' needs, which is why this is
/// computed from the whole table rather than from one handle's transition.
fn required_kernel_locks(table: &LockTable) -> KernelLocks {
    KernelLocks {
        shared_read: table.shared_count() > 0 && !table.is_exclusive(),
        shared_write: table.is_exclusive(),
        reserved: table.has_reserved(),
        pending: table.has_pending(),
    }
}

/// Brings the kernel's locks in line with what the process needs.
///
/// Releases come first so that a downgrade never holds two conflicting
/// requests at once, and the shared range is taken last so that a reader is
/// refused by the PENDING byte rather than by a race on the range itself.
fn apply_kernel_locks(file: &File, entry: &mut InodeLocks) -> VfsResult<()> {
    let wanted = required_kernel_locks(&entry.table);
    let applied = entry.applied;
    if wanted == applied {
        return Ok(());
    }
    release_unwanted(file, wanted, &mut entry.applied)?;
    if wanted.pending && !entry.applied.pending {
        if !try_lock_bytes(file, PENDING_BYTE, 1, true, VfsOperation::Lock)? {
            return Err(error::busy("another process holds PENDING"));
        }
        entry.applied.pending = true;
    }
    if wanted.reserved && !entry.applied.reserved {
        if !try_lock_bytes(file, RESERVED_BYTE, 1, true, VfsOperation::Lock)? {
            return Err(error::busy("another process holds RESERVED"));
        }
        entry.applied.reserved = true;
    }
    if wanted.shared_write && !entry.applied.shared_write {
        if !try_lock_bytes(file, SHARED_FIRST, SHARED_SIZE, true, VfsOperation::Lock)? {
            return Err(error::busy("another process is still reading"));
        }
        entry.applied.shared_write = true;
        entry.applied.shared_read = false;
    }
    if wanted.shared_read && !entry.applied.shared_read {
        take_shared_range(file, &mut entry.applied)?;
    }
    Ok(())
}

/// Releases the kernel locks the process no longer needs.
fn release_unwanted(file: &File, wanted: KernelLocks, applied: &mut KernelLocks) -> VfsResult<()> {
    if applied.shared_write && !wanted.shared_write {
        unlock_bytes(file, SHARED_FIRST, SHARED_SIZE, VfsOperation::Unlock)?;
        applied.shared_write = false;
    }
    if applied.shared_read && !wanted.shared_read {
        unlock_bytes(file, SHARED_FIRST, SHARED_SIZE, VfsOperation::Unlock)?;
        applied.shared_read = false;
    }
    if applied.reserved && !wanted.reserved {
        unlock_bytes(file, RESERVED_BYTE, 1, VfsOperation::Unlock)?;
        applied.reserved = false;
    }
    if applied.pending && !wanted.pending {
        unlock_bytes(file, PENDING_BYTE, 1, VfsOperation::Unlock)?;
        applied.pending = false;
    }
    Ok(())
}

/// Takes the read lock on the shared range.
///
/// The PENDING byte is read-locked first and released straight afterwards. A
/// process that holds PENDING has it write-locked, so this is what makes a new
/// reader wait for a writer that is already waiting rather than sliding in
/// underneath it.
fn take_shared_range(file: &File, applied: &mut KernelLocks) -> VfsResult<()> {
    let took_pending = if applied.pending {
        false
    } else {
        if !try_lock_bytes(file, PENDING_BYTE, 1, false, VfsOperation::Lock)? {
            return Err(error::busy("a writer holds PENDING"));
        }
        true
    };
    let took_read = try_lock_bytes(file, SHARED_FIRST, SHARED_SIZE, false, VfsOperation::Lock)?;
    if took_pending {
        unlock_bytes(file, PENDING_BYTE, 1, VfsOperation::Lock)?;
    }
    if !took_read {
        return Err(error::busy("another process holds the write lock"));
    }
    applied.shared_read = true;
    Ok(())
}

/// Locks a mutex, recovering from poisoning rather than propagating a panic.
fn guard<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    match mutex.lock() {
        Ok(inner) => inner,
        Err(poisoned) => poisoned.into_inner(),
    }
}
