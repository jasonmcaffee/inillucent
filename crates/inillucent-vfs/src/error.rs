//! VFS failures and their mapping onto the stable error table.
//!
//! Invariant: every VFS failure carries the extended result code SQLite would
//! report for the same operation, decided here rather than at each call site,
//! so a caller can branch on `SQLITE_IOERR_SHORT_READ` without knowing which
//! platform produced it.
//!
//! The detail string may name a path or an OS error; it lands in `DbError`'s
//! internal detail, never in the caller-visible message.

use std::io;

use inillucent_base::error::{DbError, ExtendedCode, PrimaryCode};

/// The result type every VFS operation returns.
pub type VfsResult<T> = Result<T, VfsError>;

/// A failure from a VFS operation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VfsError {
    extended: ExtendedCode,
    detail: String,
}

impl VfsError {
    /// Builds an error with an explicit extended code.
    pub fn new(extended: ExtendedCode, detail: impl Into<String>) -> VfsError {
        VfsError {
            extended,
            detail: detail.into(),
        }
    }

    /// Returns the extended result code.
    pub fn extended(&self) -> ExtendedCode {
        self.extended
    }

    /// Returns the primary result code.
    pub fn code(&self) -> PrimaryCode {
        self.extended.primary()
    }

    /// Returns the internal detail text.
    pub fn detail(&self) -> &str {
        &self.detail
    }

    /// Converts an operating-system error into the extended code SQLite uses
    /// for that operation, so a missing file during `open` and a missing file
    /// during `delete` do not report the same thing.
    pub fn from_io(operation: VfsOperation, error: &io::Error) -> VfsError {
        let extended = match (operation, error.kind()) {
            (_, io::ErrorKind::PermissionDenied) => ExtendedCode::from_primary(PrimaryCode::Perm),
            (_, io::ErrorKind::StorageFull) => ExtendedCode::from_primary(PrimaryCode::Full),
            (VfsOperation::Open, io::ErrorKind::IsADirectory) => ExtendedCode::CANT_OPEN_IS_DIR,
            (VfsOperation::Open, _) => ExtendedCode::from_primary(PrimaryCode::CantOpen),
            (VfsOperation::Delete, io::ErrorKind::NotFound) => ExtendedCode::IO_ERR_DELETE_NOENT,
            (operation, _) => operation.extended_code(),
        };
        VfsError::new(extended, format!("{operation:?}: {error}"))
    }

    /// Converts into the engine-wide error type.
    pub fn into_db_error(self) -> DbError {
        DbError::new(self.extended).with_detail(self.detail)
    }
}

impl From<VfsError> for DbError {
    /// Converts a VFS failure into the engine-wide error type.
    fn from(error: VfsError) -> DbError {
        error.into_db_error()
    }
}

impl std::fmt::Display for VfsError {
    /// Writes the code's safe message; the detail stays internal.
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.extended.message())
    }
}

impl std::error::Error for VfsError {}

/// Which VFS operation failed, which decides the extended code.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VfsOperation {
    /// Opening a file.
    Open,
    /// Reading from a file.
    Read,
    /// Writing to a file.
    Write,
    /// Flushing a file to durable media.
    Sync,
    /// Flushing the directory that contains a file.
    DirSync,
    /// Changing a file's length.
    Truncate,
    /// Asking a file its length.
    FileSize,
    /// Taking a lock.
    Lock,
    /// Releasing a lock.
    Unlock,
    /// Testing another connection's RESERVED lock.
    CheckReservedLock,
    /// Closing a file.
    Close,
    /// Deleting a file.
    Delete,
    /// Testing whether a path exists or is writable.
    Access,
    /// Resolving a path to its canonical form.
    FullPathname,
    /// Opening the shared-memory file.
    ShmOpen,
    /// Growing the shared-memory file.
    ShmSize,
    /// Mapping a shared-memory region.
    ShmMap,
    /// Locking a shared-memory slot.
    ShmLock,
    /// Locating a directory for temporary files.
    GetTempPath,
}

impl VfsOperation {
    /// Returns the extended code this operation reports for a generic failure.
    pub fn extended_code(self) -> ExtendedCode {
        match self {
            VfsOperation::Open => ExtendedCode::from_primary(PrimaryCode::CantOpen),
            VfsOperation::Read => ExtendedCode::IO_ERR_READ,
            VfsOperation::Write => ExtendedCode::IO_ERR_WRITE,
            VfsOperation::Sync => ExtendedCode::IO_ERR_FSYNC,
            VfsOperation::DirSync => ExtendedCode::IO_ERR_DIR_FSYNC,
            VfsOperation::Truncate => ExtendedCode::IO_ERR_TRUNCATE,
            VfsOperation::FileSize => ExtendedCode::IO_ERR_FSTAT,
            VfsOperation::Lock => ExtendedCode::IO_ERR_LOCK,
            VfsOperation::Unlock => ExtendedCode::IO_ERR_UNLOCK,
            VfsOperation::CheckReservedLock => ExtendedCode::IO_ERR_CHECK_RESERVED_LOCK,
            VfsOperation::Close => ExtendedCode::IO_ERR_CLOSE,
            VfsOperation::Delete => ExtendedCode::IO_ERR_DELETE,
            VfsOperation::Access => ExtendedCode::IO_ERR_ACCESS,
            VfsOperation::FullPathname => ExtendedCode::CANT_OPEN_FULL_PATH,
            VfsOperation::ShmOpen => ExtendedCode::IO_ERR_SHM_OPEN,
            VfsOperation::ShmSize => ExtendedCode::IO_ERR_SHM_SIZE,
            VfsOperation::ShmMap => ExtendedCode::IO_ERR_SHM_MAP,
            VfsOperation::ShmLock => ExtendedCode::IO_ERR_SHM_LOCK,
            VfsOperation::GetTempPath => ExtendedCode::IO_ERR_GET_TEMP_PATH,
        }
    }
}

/// Builds the short-read error, which is a successful read that returned fewer
/// bytes than asked for rather than an operating-system failure.
pub fn short_read(detail: impl Into<String>) -> VfsError {
    VfsError::new(ExtendedCode::IO_ERR_SHORT_READ, detail)
}

/// Builds the busy error a lock attempt reports when another holder conflicts.
pub fn busy(detail: impl Into<String>) -> VfsError {
    VfsError::new(ExtendedCode::from_primary(PrimaryCode::Busy), detail)
}

/// Builds the read-only error a write reports on a file opened read-only.
pub fn read_only(detail: impl Into<String>) -> VfsError {
    VfsError::new(ExtendedCode::from_primary(PrimaryCode::ReadOnly), detail)
}

/// Builds the disk-full error.
pub fn disk_full(detail: impl Into<String>) -> VfsError {
    VfsError::new(ExtendedCode::from_primary(PrimaryCode::Full), detail)
}

/// Builds the misuse error for a caller that broke the VFS contract, such as
/// asking for a lock transition the state machine does not allow.
pub fn misuse(detail: impl Into<String>) -> VfsError {
    VfsError::new(ExtendedCode::from_primary(PrimaryCode::Misuse), detail)
}

/// Builds the interrupt error a failure-injecting VFS reports when a caller
/// asked to be interrupted at this point.
pub fn interrupted(detail: impl Into<String>) -> VfsError {
    VfsError::new(ExtendedCode::from_primary(PrimaryCode::Interrupt), detail)
}
