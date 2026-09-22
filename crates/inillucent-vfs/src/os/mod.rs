//! The operating-system VFS.
//!
//! Invariant: this module is the only place in the workspace that calls the
//! operating system's file APIs. Everything above it - pager, journal, WAL,
//! catalog - reaches the disk through the `Vfs` contract, which is what lets
//! the simulator replace the disk without any of them noticing.
//!
//! The parts that differ between platforms are exactly three: byte-range
//! locking, shared memory, and the small services (randomness, file identity,
//! directory sync). Everything else - positional reads and writes, truncation,
//! path handling, temporary names - is the same code on both, so it lives here
//! rather than being written twice and drifting.

mod filelock;
mod ranges;

#[cfg(unix)]
#[path = "unix.rs"]
mod platform;

#[cfg(windows)]
#[path = "windows.rs"]
mod platform;

pub use platform::system_randomness;

use std::fs::{File, OpenOptions as FsOpenOptions};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::sync::Mutex;
use std::time::SystemTime;

use crate::contract::{
    AccessMode, DeviceCharacteristics, FileIdentity, FileLock, OpenOptions, SharedMemory, SyncMode,
    Vfs, VfsFile,
};
use crate::error::{self, VfsError, VfsOperation, VfsResult};
use crate::locks::HandleId;
use crate::path::DbPath;

/// The counter every open file handle in this process takes its identity from.
///
/// Process-wide, and it has to be. The identity is what the POSIX in-process
/// lock registry uses to tell one handle from another, and that registry is
/// itself process-wide - so a counter that started again for each `OsVfs`
/// would hand the same identity to two different files and the registry would
/// treat them as one. It did: two connections opened through separate
/// `Database::open` calls each got handle 1, so the second was told it already
/// held the writer's reservation and both wrote at once. Windows never saw it,
/// because its locks are the kernel's and it does not consult this at all.
static NEXT_HANDLE: AtomicU64 = AtomicU64::new(1);

/// Returns the next handle identity, used by the in-process lock registry.
fn next_handle() -> HandleId {
    HandleId(NEXT_HANDLE.fetch_add(1, Ordering::Relaxed))
}

/// A VFS backed by the real file system.
#[derive(Debug)]
pub struct OsVfs {
    name: String,
    temp_counter: AtomicU64,
    /// How many times this VFS has forced a directory entry.
    ///
    /// **The only thing that makes a directory sync observable** (task-2066
    /// section 4.2, item 19). Forcing the entry that names a newly created
    /// file changes nothing a later read can see; what it changes is what
    /// survives a power loss, and no test on a real file system can arrange
    /// one. Without the counter the case that this repository forgot for a
    /// year - `delete` and `rename` synced the directory and `open` did not -
    /// could only be checked by reading the code that has the bug in it.
    directory_syncs: AtomicU64,
}

impl OsVfs {
    /// Creates the default operating-system VFS.
    pub fn new() -> OsVfs {
        OsVfs {
            name: platform::VFS_NAME.to_string(),
            temp_counter: AtomicU64::new(1),
            directory_syncs: AtomicU64::new(0),
        }
    }

    /// Returns how many directory entries this VFS has forced.
    ///
    /// See the field. It counts the calls this VFS made, on every platform -
    /// including Windows, where `sync_directory` does nothing by design because
    /// NTFS journals the metadata change itself, so the count says the engine
    /// asked rather than that the disk was touched.
    pub fn directory_syncs(&self) -> u64 {
        self.directory_syncs.load(Ordering::Relaxed)
    }

    /// Forces the directory entry naming a path, and counts it.
    ///
    /// @param path - the file whose containing directory to force
    fn force_the_directory_entry(&self, path: &DbPath) -> VfsResult<()> {
        let Some(parent) = path.containing_directory() else {
            return Ok(());
        };
        self.directory_syncs.fetch_add(1, Ordering::Relaxed);
        platform::sync_directory(&parent)
    }
}

impl Default for OsVfs {
    /// Creates the default operating-system VFS.
    fn default() -> OsVfs {
        OsVfs::new()
    }
}

impl Vfs for OsVfs {
    /// Returns the registered name of this VFS.
    fn name(&self) -> &str {
        &self.name
    }

    /// Opens a file with the requested access and creation behaviour.
    fn open(&self, path: &DbPath, options: OpenOptions) -> VfsResult<Box<dyn VfsFile>> {
        // **The confinement backstop.** Every file this workspace opens is
        // opened here, so a process started with `--root` cannot reach past it
        // through a command nobody remembered to check, through a path that
        // arrived inside a SQL statement, or through a file operation added
        // after the check list was written. See `crate::confine`.
        crate::confine::authorize(path)?;
        let mut fs_options = FsOpenOptions::new();
        fs_options.read(true);
        if !options.read_only {
            fs_options.write(true);
            fs_options.create(options.create && !options.exclusive);
            fs_options.create_new(options.exclusive);
        }
        // **The path is in the message (task-1946).** `from_io` alone answers
        // `Open: No such file or directory (os error 2)`, which names neither the
        // file nor which of the several a statement touches - and every file this
        // workspace opens is opened here, so that one sentence was the whole
        // report for a missing database, a missing log segment and a missing
        // scratch file alike.
        // **Whether this open is going to create the file, asked before it
        // does** (task-2066 section 4.2, item 19). It is the only moment the
        // answer is knowable: afterwards the file exists either way.
        let creating = !options.read_only
            && (options.create || options.exclusive)
            && options.kind.survives_a_restart()
            && !path.as_path().exists();
        let file = fs_options
            .open(path.as_path())
            .map_err(|error| VfsError::from_io(VfsOperation::Open, &error).about(path.as_path()))?;
        // **And the directory entry that names it is forced.** `delete` and
        // `rename` have done this since phase 1 and `open` never did, so on
        // ext4 or XFS a power loss could leave a log segment whose header had
        // been written and synced with no entry pointing at it - and
        // `read_chain` reads a missing segment as the ordinary end of the
        // chain, so every commit inside it is lost and recovery reports
        // success. Windows does nothing here by design: NTFS journals the
        // metadata change itself.
        if creating {
            self.force_the_directory_entry(path)?;
        }
        let identity = platform::file_identity(&file)?;
        let file = Arc::new(file);
        Ok(Box::new(OsFile {
            locks: platform::LockState::new(identity.clone(), next_handle(), Arc::clone(&file)),
            file,
            path: path.clone(),
            options,
            identity,
            shm: Mutex::new(None),
        }))
    }

    /// Deletes a file, optionally flushing the containing directory so that the
    /// deletion survives a power loss.
    fn delete(&self, path: &DbPath, sync_dir: bool) -> VfsResult<()> {
        crate::confine::authorize(path)?;
        match std::fs::remove_file(path.as_path()) {
            Ok(()) => {}
            Err(removal) if removal.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(removal) => return Err(VfsError::from_io(VfsOperation::Delete, &removal)),
        }
        if sync_dir {
            self.force_the_directory_entry(path)?;
        }
        Ok(())
    }

    /// Replaces `to` with `from` through the platform's own rename.
    ///
    /// `std::fs::rename` replaces an existing destination on both platforms
    /// this ships on, which is the atomicity the contract promises. The
    /// directory entry is then flushed the same way `delete` flushes it: Unix
    /// has to be told, Windows journals the metadata itself.
    fn rename(&self, from: &DbPath, to: &DbPath) -> VfsResult<()> {
        crate::confine::authorize(from)?;
        crate::confine::authorize(to)?;
        std::fs::rename(from.as_path(), to.as_path())
            .map_err(|error| VfsError::from_io(VfsOperation::Rename, &error))?;
        self.force_the_directory_entry(to)?;
        Ok(())
    }

    /// Reports whether a path exists, and whether it is writable when asked.
    fn access(&self, path: &DbPath, mode: AccessMode) -> VfsResult<bool> {
        // A confined process is told the file is not there rather than that it
        // may not look, because "does `C:/Users/…/id_rsa` exist" is itself an
        // answer a confined caller may not have.
        if crate::confine::authorize(path).is_err() {
            return Ok(false);
        }
        let metadata = match std::fs::metadata(path.as_path()) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => return Ok(false),
            Err(error) => return Err(VfsError::from_io(VfsOperation::Access, &error)),
        };
        Ok(match mode {
            AccessMode::Exists => true,
            AccessMode::ReadOnly => true,
            AccessMode::ReadWrite => !metadata.permissions().readonly(),
        })
    }

    /// Resolves a path to an absolute one.
    ///
    /// The path may not exist yet - the pager asks for the canonical name of a
    /// journal it is about to create - so this joins against the working
    /// directory rather than calling `canonicalize`, which requires the file to
    /// be there. An existing file is canonicalised so that two names for the
    /// same file resolve alike.
    fn full_pathname(&self, path: &DbPath) -> VfsResult<DbPath> {
        if let Ok(resolved) = std::fs::canonicalize(path.as_path()) {
            return Ok(DbPath::new(strip_verbatim_prefix(resolved)));
        }
        if path.as_path().is_absolute() {
            return Ok(path.clone());
        }
        let working = std::env::current_dir()
            .map_err(|error| VfsError::from_io(VfsOperation::FullPathname, &error))?;
        Ok(DbPath::new(working.join(path.as_path())))
    }

    /// Fills `output` with randomness from the operating system.
    fn randomness(&self, output: &mut [u8]) -> VfsResult<()> {
        platform::system_randomness(output)
    }

    /// Returns the current wall-clock time.
    fn current_time(&self) -> VfsResult<SystemTime> {
        Ok(SystemTime::now())
    }

    /// Returns a temporary path in the platform's temporary directory that does
    /// not currently exist.
    fn temp_path(&self, prefix: &str) -> VfsResult<DbPath> {
        // A confined process makes its temporaries inside the root. The
        // platform's temporary directory is outside it, so a temporary taken
        // from there would be authorized against the root and refused at the
        // moment it was opened - a confinement that breaks the feature rather
        // than confining it.
        let directory = match crate::confine::process_root() {
            Some(root) => root.directory().to_path_buf(),
            None => std::env::temp_dir(),
        };
        for _ in 0..10_000 {
            let counter = self.temp_counter.fetch_add(1, Ordering::Relaxed);
            let mut noise = [0u8; 8];
            platform::system_randomness(&mut noise)?;
            let tag = u64::from_le_bytes(noise);
            let candidate = directory.join(format!("{prefix}{counter:08x}{tag:016x}.tmp"));
            if !candidate.exists() {
                return Ok(DbPath::new(candidate));
            }
        }
        Err(VfsError::new(
            VfsOperation::GetTempPath.extended_code(),
            "exhausted temporary names",
        ))
    }

    /// Sleeps for at least `micros` microseconds.
    fn sleep(&self, micros: u64) -> VfsResult<()> {
        std::thread::sleep(std::time::Duration::from_micros(micros));
        Ok(())
    }
}

/// Removes the `\\?\` prefix Windows canonicalisation adds.
///
/// The prefix is correct but it leaks into error messages and into any name a
/// caller stores, and a second process deriving a journal name from the plain
/// path would then disagree about the file identity string.
fn strip_verbatim_prefix(path: PathBuf) -> PathBuf {
    let text = path.to_string_lossy().to_string();
    match text.strip_prefix(r"\\?\") {
        Some(stripped) => PathBuf::from(stripped),
        None => path,
    }
}

/// One open file on the real file system.
#[derive(Debug)]
pub struct OsFile {
    file: Arc<File>,
    path: DbPath,
    options: OpenOptions,
    identity: FileIdentity,
    locks: platform::LockState,
    shm: Mutex<Option<Arc<dyn SharedMemory>>>,
}

impl OsFile {
    /// Refuses a mutating operation on a handle opened read-only.
    fn require_writable(&self, operation: VfsOperation) -> VfsResult<()> {
        if self.options.read_only {
            return Err(error::read_only(format!(
                "{operation:?} on a read-only handle"
            )));
        }
        Ok(())
    }
}

impl VfsFile for OsFile {
    /// Reads exactly `output.len()` bytes at `offset`, zero-filling the tail and
    /// reporting a short read when the file ends first.
    fn read_exact_at(&self, offset: u64, output: &mut [u8]) -> VfsResult<()> {
        let mut filled = 0usize;
        while filled < output.len() {
            let Some(target) = output.get_mut(filled..) else {
                break;
            };
            let at = offset.checked_add(filled as u64).ok_or_else(|| {
                VfsError::new(VfsOperation::Read.extended_code(), "read offset overflowed")
            })?;
            match platform::read_at(&self.file, at, target) {
                Ok(0) => break,
                Ok(count) => filled = filled.saturating_add(count),
                Err(io) if io.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(io) => return Err(VfsError::from_io(VfsOperation::Read, &io)),
            }
        }
        if filled < output.len() {
            for slot in output.iter_mut().skip(filled) {
                *slot = 0;
            }
            return Err(error::short_read(format!(
                "read {filled} of {} bytes at {offset}",
                output.len()
            )));
        }
        Ok(())
    }

    /// Writes all of `input` at `offset`, retrying a partial write.
    fn write_all_at(&self, offset: u64, input: &[u8]) -> VfsResult<()> {
        self.require_writable(VfsOperation::Write)?;
        let mut written = 0usize;
        while written < input.len() {
            let Some(source) = input.get(written..) else {
                break;
            };
            let at = offset.checked_add(written as u64).ok_or_else(|| {
                VfsError::new(
                    VfsOperation::Write.extended_code(),
                    "write offset overflowed",
                )
            })?;
            match platform::write_at(&self.file, at, source) {
                Ok(0) => {
                    return Err(error::disk_full(format!("write stalled at {at}")));
                }
                Ok(count) => written = written.saturating_add(count),
                Err(io) if io.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(io) => return Err(VfsError::from_io(VfsOperation::Write, &io)),
            }
        }
        Ok(())
    }

    /// Returns the file's length.
    fn file_size(&self) -> VfsResult<u64> {
        self.file
            .metadata()
            .map(|metadata| metadata.len())
            .map_err(|error| VfsError::from_io(VfsOperation::FileSize, &error))
    }

    /// Sets the file's length.
    fn truncate(&self, size: u64) -> VfsResult<()> {
        self.require_writable(VfsOperation::Truncate)?;
        self.file
            .set_len(size)
            .map_err(|error| VfsError::from_io(VfsOperation::Truncate, &error))
    }

    /// Flushes written data toward durable media.
    fn sync(&self, mode: SyncMode) -> VfsResult<()> {
        self.require_writable(VfsOperation::Sync)?;
        // **`Full` is the drive's own barrier where the platform has one**
        // (task-2066 section 4.2, item 19). On Darwin `fsync(2)` returns once
        // the bytes have reached the disk's cache and says nothing about the
        // platter, which is why Apple provides `F_FULLFSYNC` and why SQLite
        // has a pragma for it; `Full` and `Normal` both mapped to `sync_all`
        // here, so `PRAGMA synchronous = FULL` bought nothing on a shipped
        // target. Everywhere else `sync_all` is the strongest barrier there
        // is and `platform::full_sync` is it.
        let result = match mode {
            SyncMode::DataOnly => self.file.sync_data(),
            SyncMode::Normal => self.file.sync_all(),
            SyncMode::Full => platform::full_sync(&self.file),
        };
        result.map_err(|error| VfsError::from_io(VfsOperation::Sync, &error))
    }

    /// Raises the lock level, one protocol step at a time.
    fn lock(&self, level: FileLock) -> VfsResult<()> {
        if !self.options.kind.is_locked() {
            return Ok(());
        }
        self.locks.acquire(&self.file, level)
    }

    /// Lowers the lock level.
    fn unlock(&self, level: FileLock) -> VfsResult<()> {
        if !self.options.kind.is_locked() {
            return Ok(());
        }
        self.locks.release(&self.file, level)
    }

    /// Returns the lock level this handle holds.
    fn lock_level(&self) -> FileLock {
        self.locks.level()
    }

    /// Reports whether another handle, in this process or another, holds
    /// RESERVED or stronger.
    fn check_reserved_lock(&self) -> VfsResult<bool> {
        if !self.options.kind.is_locked() {
            return Ok(false);
        }
        self.locks.check_reserved(&self.file)
    }

    /// Returns what the device underneath this file guarantees.
    fn device_characteristics(&self) -> DeviceCharacteristics {
        platform::device_characteristics()
    }

    /// Opens the shared-memory file for this database, creating it on first use.
    fn shared_memory(&self) -> VfsResult<Option<Arc<dyn SharedMemory>>> {
        if !self.options.kind.is_locked() {
            return Ok(None);
        }
        let mut slot = match self.shm.lock() {
            Ok(slot) => slot,
            Err(poisoned) => poisoned.into_inner(),
        };
        if let Some(existing) = slot.as_ref() {
            return Ok(Some(Arc::clone(existing)));
        }
        let opened = platform::open_shm(&self.path.shm())?;
        *slot = Some(Arc::clone(&opened));
        Ok(Some(opened))
    }

    /// Returns the volume and file numbers that identify this file.
    fn file_identity(&self) -> VfsResult<FileIdentity> {
        Ok(self.identity.clone())
    }
}

impl Drop for OsFile {
    /// Releases the file's locks and removes it when it was opened to be
    /// deleted on close.
    fn drop(&mut self) {
        let _ = self.locks.release(&self.file, FileLock::None);
        if self.options.delete_on_close {
            let _ = std::fs::remove_file(self.path.as_path());
        }
    }
}
