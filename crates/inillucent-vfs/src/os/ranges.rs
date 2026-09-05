//! The byte ranges the locking protocol uses.
//!
//! Invariant: these offsets are part of the on-disk compatibility contract, not
//! an implementation choice. Another process - including a real SQLite build
//! reading the same database - takes locks at exactly these offsets, so a
//! different value here would make two processes believe they each hold the
//! write lock.
//!
//! The bytes sit at the one-gigabyte mark, past any realistic database, so the
//! locked region never overlaps data. They are never read or written; only
//! locked.

/// The byte a writer locks to stop new readers arriving.
pub const PENDING_BYTE: u64 = 0x4000_0000;

/// The byte a writer locks to declare its intention to write.
pub const RESERVED_BYTE: u64 = PENDING_BYTE + 1;

/// The first byte of the range readers share.
pub const SHARED_FIRST: u64 = PENDING_BYTE + 2;

/// How many bytes the shared range covers.
pub const SHARED_SIZE: u64 = 510;

/// The first byte of the shared-memory lock slots, within the `-shm` file.
///
/// The WAL-index header occupies the start of the file; the lock slots follow
/// it at a fixed offset so that two processes agree on which byte means which
/// slot without reading the header first.
pub const SHM_LOCK_FIRST: u64 = 120;

/// How many bytes each shared-memory lock slot occupies.
pub const SHM_LOCK_SIZE: u64 = 1;

/// The byte that says whether any process has this shared memory mapped.
///
/// It is SQLite's dead-man switch and it sits immediately after the eight lock
/// slots, at the same offset SQLite uses, so that the two implementations
/// arbitrate against each other rather than each believing it is alone. Every
/// connection holds a shared lock on it for as long as it has the file mapped;
/// a process that manages to take it *exclusively* has proved nobody else is
/// using the file, and the wal-index inside it is therefore a leftover from a
/// process that died. A leftover is not repairable - the index is a cache of
/// the log, and the only honest thing to do with a cache whose owner crashed
/// is to throw it away - so that process truncates the file and every reader
/// rebuilds from the log. Without this, a crashed connection's stale index
/// would be believed by the next one to open.
pub const SHM_DEAD_MANS_SWITCH: u64 = SHM_LOCK_FIRST + 8;

/// Returns the byte offset of shared-memory lock slot `index`.
pub fn shm_lock_offset(index: u16) -> u64 {
    SHM_LOCK_FIRST.saturating_add(u64::from(index).saturating_mul(SHM_LOCK_SIZE))
}
