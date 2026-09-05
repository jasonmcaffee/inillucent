//! The SQLite locking protocol as a data structure.
//!
//! Invariant: the rules for which lock levels may coexist live here once. The
//! in-memory VFS, the simulator, and the POSIX in-process registry all use this
//! table, so a disagreement between "what the simulator allows" and "what the
//! real file system allows" cannot come from three copies of the rules drifting
//! apart. The Windows implementation delegates to the kernel instead, and the
//! conformance suite checks both against the same expectations.
//!
//! The protocol, in one paragraph. Any number of holders may hold SHARED at
//! once. One holder at a time may hold RESERVED, and readers keep reading while
//! it does. PENDING is the same intention but it also stops *new* readers
//! arriving, which is what stops a writer starving. EXCLUSIVE requires that
//! every reader has left. A holder may only raise from the level below, except
//! that reaching EXCLUSIVE implies passing through PENDING.

use std::collections::BTreeSet;

use crate::contract::FileLock;

/// Identifies one open file handle within the table.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct HandleId(pub u64);

/// Why a lock request was refused.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LockConflict {
    /// Another holder holds a lock that excludes this one.
    Busy,
    /// The requested transition is not one the protocol allows.
    Protocol,
}

/// Who holds what on one file.
#[derive(Clone, Debug, Default)]
pub struct LockTable {
    shared: BTreeSet<HandleId>,
    reserved: Option<HandleId>,
    pending: Option<HandleId>,
    exclusive: Option<HandleId>,
}

impl LockTable {
    /// Creates an unlocked table.
    pub fn new() -> LockTable {
        LockTable::default()
    }

    /// Raises `handle` from `current` to `target`.
    ///
    /// Returns `LockConflict::Busy` when another holder is in the way, which
    /// the caller reports as `SQLITE_BUSY`, and `LockConflict::Protocol` when
    /// the transition itself is not legal, which is a caller bug.
    pub fn acquire(
        &mut self,
        handle: HandleId,
        current: FileLock,
        target: FileLock,
    ) -> Result<(), LockConflict> {
        if target <= current {
            return Err(LockConflict::Protocol);
        }
        match target {
            FileLock::Shared => self.acquire_shared(handle, current),
            FileLock::Reserved => self.acquire_reserved(handle, current),
            FileLock::Pending => self.acquire_pending(handle, current),
            FileLock::Exclusive => self.acquire_exclusive(handle, current),
            FileLock::None => Err(LockConflict::Protocol),
        }
    }

    /// Takes a read lock, which any number of holders may share, unless a
    /// holder is waiting to write.
    fn acquire_shared(&mut self, handle: HandleId, current: FileLock) -> Result<(), LockConflict> {
        if current != FileLock::None {
            return Err(LockConflict::Protocol);
        }
        if self.pending.is_some() || self.exclusive.is_some() {
            return Err(LockConflict::Busy);
        }
        self.shared.insert(handle);
        Ok(())
    }

    /// Declares the intention to write while readers keep reading.
    fn acquire_reserved(
        &mut self,
        handle: HandleId,
        current: FileLock,
    ) -> Result<(), LockConflict> {
        if current != FileLock::Shared {
            return Err(LockConflict::Protocol);
        }
        if self.reserved.is_some() || self.exclusive.is_some() {
            return Err(LockConflict::Busy);
        }
        self.reserved = Some(handle);
        Ok(())
    }

    /// Stops new readers arriving while the writer waits for the current ones
    /// to leave.
    fn acquire_pending(&mut self, handle: HandleId, current: FileLock) -> Result<(), LockConflict> {
        if current < FileLock::Shared {
            return Err(LockConflict::Protocol);
        }
        if self.pending.is_some_and(|owner| owner != handle) || self.exclusive.is_some() {
            return Err(LockConflict::Busy);
        }
        self.pending = Some(handle);
        Ok(())
    }

    /// Takes the write lock, which requires every reader to have left.
    ///
    /// EXCLUSIVE is reached from PENDING, never straight from SHARED, so that
    /// a failed promotion leaves the caller holding exactly the level it was
    /// told it holds. `next_step` walks a caller through the intermediate
    /// levels; SQLite's own lock routine does the same.
    fn acquire_exclusive(
        &mut self,
        handle: HandleId,
        current: FileLock,
    ) -> Result<(), LockConflict> {
        if current != FileLock::Pending || self.pending != Some(handle) {
            return Err(LockConflict::Protocol);
        }
        if self.exclusive.is_some() {
            return Err(LockConflict::Busy);
        }
        if self.shared.iter().any(|reader| *reader != handle) {
            return Err(LockConflict::Busy);
        }
        self.exclusive = Some(handle);
        self.shared.remove(&handle);
        Ok(())
    }

    /// Lowers `handle` from `current` to `target`, which must be `Shared` or
    /// `None`. Dropping to `Shared` from `Exclusive` restores the read lock.
    pub fn release(
        &mut self,
        handle: HandleId,
        current: FileLock,
        target: FileLock,
    ) -> Result<(), LockConflict> {
        if target >= current || !matches!(target, FileLock::None | FileLock::Shared) {
            return Err(LockConflict::Protocol);
        }
        if self.reserved == Some(handle) {
            self.reserved = None;
        }
        if self.pending == Some(handle) {
            self.pending = None;
        }
        if self.exclusive == Some(handle) {
            self.exclusive = None;
        }
        match target {
            FileLock::Shared => {
                self.shared.insert(handle);
            }
            _ => {
                self.shared.remove(&handle);
            }
        }
        Ok(())
    }

    /// Undoes one raise, putting `handle` back at `to` after it reached `from`.
    ///
    /// This is not the same as `release`: a caller may only *release* down to
    /// SHARED or NONE, but a failed multi-step promotion has to be unwound one
    /// step at a time, including back to PENDING or RESERVED.
    pub fn undo_step(&mut self, handle: HandleId, from: FileLock, to: FileLock) {
        if from <= to {
            return;
        }
        if from == FileLock::Exclusive && self.exclusive == Some(handle) {
            self.exclusive = None;
            self.shared.insert(handle);
        }
        if from >= FileLock::Pending && to < FileLock::Pending && self.pending == Some(handle) {
            self.pending = None;
        }
        if from >= FileLock::Reserved && to < FileLock::Reserved && self.reserved == Some(handle) {
            self.reserved = None;
        }
        if to == FileLock::None {
            self.shared.remove(&handle);
        }
    }

    /// Removes every lock `handle` holds, as closing the file does.
    pub fn release_all(&mut self, handle: HandleId) {
        let _ = self.release(handle, FileLock::Exclusive, FileLock::None);
    }

    /// Reports whether a holder other than `handle` holds RESERVED or stronger,
    /// which is what `check_reserved_lock` answers.
    pub fn has_reserved_or_stronger(&self, excluding: HandleId) -> bool {
        let owned_by_other = |owner: &Option<HandleId>| owner.is_some_and(|held| held != excluding);
        owned_by_other(&self.reserved)
            || owned_by_other(&self.pending)
            || owned_by_other(&self.exclusive)
    }

    /// Returns how many holders hold a read lock.
    pub fn shared_count(&self) -> usize {
        self.shared.len()
    }

    /// Reports whether some holder holds EXCLUSIVE.
    pub fn is_exclusive(&self) -> bool {
        self.exclusive.is_some()
    }

    /// Reports whether some holder holds PENDING or EXCLUSIVE.
    pub fn has_pending(&self) -> bool {
        self.pending.is_some()
    }

    /// Reports whether some holder holds RESERVED.
    pub fn has_reserved(&self) -> bool {
        self.reserved.is_some()
    }

    /// Reports whether any holder holds any lock at all.
    pub fn is_unlocked(&self) -> bool {
        self.shared.is_empty()
            && self.reserved.is_none()
            && self.pending.is_none()
            && self.exclusive.is_none()
    }
}

/// Returns the next level to step through on the way from `current` to
/// `target`.
///
/// The protocol has no direct SHARED to EXCLUSIVE transition: a writer takes
/// PENDING first, so that if the promotion fails it is still holding the level
/// that keeps new readers out while it waits.
pub fn next_step(current: FileLock, target: FileLock) -> FileLock {
    match current {
        FileLock::None => FileLock::Shared,
        FileLock::Shared if target == FileLock::Reserved => FileLock::Reserved,
        FileLock::Shared => FileLock::Pending,
        FileLock::Reserved => FileLock::Pending,
        _ => FileLock::Exclusive,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Two readers coexist; that is the whole point of the SHARED level.
    #[test]
    fn readers_share() {
        let mut table = LockTable::new();
        let (a, b) = (HandleId(1), HandleId(2));
        table.acquire(a, FileLock::None, FileLock::Shared).unwrap();
        table.acquire(b, FileLock::None, FileLock::Shared).unwrap();
        assert_eq!(table.shared_count(), 2);
    }

    /// One RESERVED at a time, and readers keep reading underneath it.
    #[test]
    fn reserved_excludes_other_writers_but_not_readers() {
        let mut table = LockTable::new();
        let (a, b, c) = (HandleId(1), HandleId(2), HandleId(3));
        table.acquire(a, FileLock::None, FileLock::Shared).unwrap();
        table.acquire(b, FileLock::None, FileLock::Shared).unwrap();
        table
            .acquire(a, FileLock::Shared, FileLock::Reserved)
            .unwrap();
        assert_eq!(
            table.acquire(b, FileLock::Shared, FileLock::Reserved),
            Err(LockConflict::Busy)
        );
        table.acquire(c, FileLock::None, FileLock::Shared).unwrap();
        assert!(table.has_reserved_or_stronger(b));
        assert!(!table.has_reserved_or_stronger(a));
    }

    /// PENDING stops new readers, which is how a writer stops starving.
    #[test]
    fn pending_stops_new_readers() {
        let mut table = LockTable::new();
        let (a, b) = (HandleId(1), HandleId(2));
        table.acquire(a, FileLock::None, FileLock::Shared).unwrap();
        table
            .acquire(a, FileLock::Shared, FileLock::Pending)
            .unwrap();
        assert_eq!(
            table.acquire(b, FileLock::None, FileLock::Shared),
            Err(LockConflict::Busy)
        );
    }

    /// EXCLUSIVE waits for the last reader and leaves PENDING behind while it
    /// waits, so the reader that is leaving cannot be replaced by a new one.
    #[test]
    fn exclusive_waits_for_readers_to_leave() {
        let mut table = LockTable::new();
        let (writer, reader) = (HandleId(1), HandleId(2));
        table
            .acquire(writer, FileLock::None, FileLock::Shared)
            .unwrap();
        table
            .acquire(reader, FileLock::None, FileLock::Shared)
            .unwrap();
        table
            .acquire(writer, FileLock::Shared, FileLock::Pending)
            .unwrap();
        assert_eq!(
            table.acquire(writer, FileLock::Pending, FileLock::Exclusive),
            Err(LockConflict::Busy)
        );
        assert_eq!(
            table.acquire(HandleId(3), FileLock::None, FileLock::Shared),
            Err(LockConflict::Busy)
        );
        table
            .release(reader, FileLock::Shared, FileLock::None)
            .unwrap();
        table
            .acquire(writer, FileLock::Pending, FileLock::Exclusive)
            .unwrap();
        assert_eq!(table.shared_count(), 0);
    }

    /// Dropping from EXCLUSIVE back to SHARED restores a read lock, which is
    /// what a commit does before it lets the next reader in.
    #[test]
    fn dropping_from_exclusive_to_shared_restores_the_read_lock() {
        let mut table = LockTable::new();
        let writer = HandleId(1);
        table
            .acquire(writer, FileLock::None, FileLock::Shared)
            .unwrap();
        table
            .acquire(writer, FileLock::Shared, FileLock::Pending)
            .unwrap();
        table
            .acquire(writer, FileLock::Pending, FileLock::Exclusive)
            .unwrap();
        table
            .release(writer, FileLock::Exclusive, FileLock::Shared)
            .unwrap();
        assert_eq!(table.shared_count(), 1);
        assert!(!table.has_reserved_or_stronger(HandleId(9)));
        table
            .acquire(HandleId(2), FileLock::None, FileLock::Shared)
            .unwrap();
    }

    /// Closing a handle releases everything it held, at every level.
    #[test]
    fn releasing_everything_leaves_the_table_unlocked() {
        for level in [
            FileLock::Shared,
            FileLock::Reserved,
            FileLock::Pending,
            FileLock::Exclusive,
        ] {
            let mut table = LockTable::new();
            let handle = HandleId(1);
            let mut step = FileLock::None;
            while step < level {
                let next = next_step(step, level);
                table.acquire(handle, step, next).unwrap();
                step = next;
            }
            table.release_all(handle);
            assert!(table.is_unlocked(), "{level:?} left something behind");
        }
    }

    /// Illegal transitions are a caller bug, not a busy file; reporting them as
    /// BUSY would make the pager retry forever.
    #[test]
    fn illegal_transitions_are_protocol_errors() {
        let mut table = LockTable::new();
        let handle = HandleId(1);
        assert_eq!(
            table.acquire(handle, FileLock::None, FileLock::Reserved),
            Err(LockConflict::Protocol)
        );
        assert_eq!(
            table.acquire(handle, FileLock::None, FileLock::None),
            Err(LockConflict::Protocol)
        );
        table
            .acquire(handle, FileLock::None, FileLock::Shared)
            .unwrap();
        assert_eq!(
            table.acquire(handle, FileLock::Shared, FileLock::Shared),
            Err(LockConflict::Protocol)
        );
        assert_eq!(
            table.release(handle, FileLock::Shared, FileLock::Reserved),
            Err(LockConflict::Protocol)
        );
    }
}
