//! The file lock this pool holds, and who else holds it.
//!
//! Invariant: **the pool owns the file, so the pool owns the lock.** The
//! protocol itself is `inillucent-vfs`'s - SHARED, RESERVED, PENDING,
//! EXCLUSIVE, implemented and conformance tested since Phase 2 - and taking it
//! here is what makes `PRAGMA locking_mode = normal` a description of what
//! happens rather than a claim about it.
//!
//! Split out of `pool.rs` in task-1980, which added `file` below. Nothing moved
//! but the text: these are still `impl Pool`, reading the same private state,
//! because a child module can see its parent's private items - the same split
//! `eviction.rs` and `swizzle.rs` are.

use super::*;

impl Pool {
    /// Raises the lock on the database file.
    ///
    /// **The pool owns the file, so the pool owns the lock.** The protocol
    /// itself is `inillucent-vfs`'s - it has been implemented and conformance
    /// tested since Phase 2 and nothing used it, because the engine assumed it
    /// was the only process on the file. Using it is what makes
    /// `PRAGMA locking_mode = normal` a description rather than a claim.
    ///
    /// @param level - the level to raise to
    pub fn lock(&self, level: FileLock) -> DbResult<()> {
        self.lock_within(level, DEFAULT_BUSY_MILLIS)
    }

    /// Raises the lock, waiting up to a budget for the holder to let go.
    ///
    /// **Waiting is the whole of what a busy timeout is.** A lock another
    /// process holds is not an error - it is a lock that will be released - and
    /// an engine that reported failure immediately would make every concurrent
    /// pair of writers fail rather than take turns. The sleep grows so that a
    /// long wait is not a spin, and the last attempt reports what it found.
    ///
    /// @param level - the level to raise to
    /// @param budget_millis - how long to keep trying
    pub fn lock_within(&self, level: FileLock, budget_millis: u64) -> DbResult<()> {
        lock_with_wait(self.file.as_ref(), level, budget_millis)
    }

    /// Lowers the lock on the database file.
    ///
    /// @param level - the level to drop to, `None` to release entirely
    pub fn unlock(&self, level: FileLock) -> DbResult<()> {
        if self.file.lock_level() <= level {
            return Ok(());
        }
        self.file
            .unlock(level)
            .map_err(|error| error.into_db_error())
    }

    /// Returns the level currently held.
    pub fn lock_level(&self) -> FileLock {
        self.file.lock_level()
    }

    /// Returns the file itself, for a caller that has to ask it about its own
    /// locks.
    ///
    /// Narrow on purpose: it exists so a refusal can name who holds the file
    /// rather than which lock level was refused (task-1979, C6), and nothing
    /// else should read or write through it - every read and write in this
    /// crate goes through the pool so the cache and the write-ahead rule are
    /// not bypassed.
    pub fn file(&self) -> &dyn VfsFile {
        self.file.as_ref()
    }
}
