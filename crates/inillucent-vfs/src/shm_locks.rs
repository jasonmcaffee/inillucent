//! The shared-memory lock slots the WAL index is coordinated with.
//!
//! Invariant: a slot is either held by any number of readers or by exactly one
//! writer, never both, and a release never lowers a count it did not raise.
//!
//! There are eight slots. The WAL protocol uses them for the write lock, the
//! checkpoint lock, the recovery lock, and the read marks; the VFS does not
//! know what a slot means, only that the exclusion rules hold.

use crate::contract::{ShmLockRequest, SHM_LOCK_COUNT};
use crate::error::{self, VfsResult};

/// One slot's state.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct Slot {
    readers: u32,
    writer: bool,
}

/// The eight shared-memory lock slots of one shared-memory file.
#[derive(Clone, Debug)]
pub struct ShmLockTable {
    slots: [Slot; SHM_LOCK_COUNT as usize],
}

impl Default for ShmLockTable {
    /// Creates a table with every slot free.
    fn default() -> ShmLockTable {
        ShmLockTable {
            slots: [Slot::default(); SHM_LOCK_COUNT as usize],
        }
    }
}

impl ShmLockTable {
    /// Applies a lock request to the slots it names.
    ///
    /// The whole request succeeds or the table is left untouched, because a
    /// half-applied range would leave the WAL protocol holding locks it does
    /// not believe it holds.
    pub fn apply(&mut self, request: ShmLockRequest) -> VfsResult<()> {
        let range = self.range_of(request)?;
        if request.acquire {
            self.check_available(&range, request.exclusive)?;
        }
        for index in range {
            let Some(slot) = self.slots.get_mut(index) else {
                continue;
            };
            apply_to_slot(slot, request);
        }
        Ok(())
    }

    /// Returns the slot indices a request names, or a misuse error.
    fn range_of(&self, request: ShmLockRequest) -> VfsResult<std::ops::Range<usize>> {
        let end = request
            .offset
            .checked_add(request.count)
            .ok_or_else(|| error::misuse("shared-memory lock range overflowed"))?;
        if request.count == 0 || end > SHM_LOCK_COUNT {
            return Err(error::misuse("shared-memory lock range out of bounds"));
        }
        Ok(usize::from(request.offset)..usize::from(end))
    }

    /// Fails with BUSY when any slot in the range already excludes the request.
    fn check_available(&self, range: &std::ops::Range<usize>, exclusive: bool) -> VfsResult<()> {
        for index in range.clone() {
            let Some(slot) = self.slots.get(index) else {
                continue;
            };
            let blocked = slot.writer || (exclusive && slot.readers > 0);
            if blocked {
                return Err(error::busy(format!("shared-memory slot {index} is held")));
            }
        }
        Ok(())
    }

    /// Returns how many readers hold a slot, for tests and diagnostics.
    pub fn readers(&self, offset: u16) -> u32 {
        self.slots
            .get(usize::from(offset))
            .map_or(0, |slot| slot.readers)
    }

    /// Reports whether a slot is held exclusively.
    pub fn is_write_locked(&self, offset: u16) -> bool {
        self.slots
            .get(usize::from(offset))
            .is_some_and(|slot| slot.writer)
    }

    /// Reports whether every slot is free.
    pub fn is_unlocked(&self) -> bool {
        self.slots
            .iter()
            .all(|slot| slot.readers == 0 && !slot.writer)
    }
}

/// Applies one request to one slot.
fn apply_to_slot(slot: &mut Slot, request: ShmLockRequest) {
    match (request.acquire, request.exclusive) {
        (true, true) => slot.writer = true,
        (true, false) => slot.readers = slot.readers.saturating_add(1),
        (false, true) => slot.writer = false,
        (false, false) => slot.readers = slot.readers.saturating_sub(1),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a request in one line so the tests read as protocol steps.
    fn request(offset: u16, count: u16, acquire: bool, exclusive: bool) -> ShmLockRequest {
        ShmLockRequest {
            offset,
            count,
            acquire,
            exclusive,
        }
    }

    /// Readers share a slot; a writer excludes them and they exclude it.
    #[test]
    fn readers_share_and_writers_exclude() {
        let mut table = ShmLockTable::default();
        table.apply(request(0, 1, true, false)).unwrap();
        table.apply(request(0, 1, true, false)).unwrap();
        assert_eq!(table.readers(0), 2);
        assert!(table.apply(request(0, 1, true, true)).is_err());
        table.apply(request(0, 1, false, false)).unwrap();
        table.apply(request(0, 1, false, false)).unwrap();
        table.apply(request(0, 1, true, true)).unwrap();
        assert!(table.is_write_locked(0));
        assert!(table.apply(request(0, 1, true, false)).is_err());
    }

    /// A range request is all-or-nothing; a partly applied range would leave
    /// the WAL protocol holding locks it does not know about.
    #[test]
    fn a_blocked_range_changes_nothing() {
        let mut table = ShmLockTable::default();
        table.apply(request(3, 1, true, true)).unwrap();
        assert!(table.apply(request(0, 5, true, true)).is_err());
        for slot in 0..3 {
            assert!(!table.is_write_locked(slot), "slot {slot} was taken anyway");
        }
    }

    /// Ranges outside the eight slots are a caller mistake.
    #[test]
    fn out_of_range_requests_are_refused() {
        let mut table = ShmLockTable::default();
        assert!(table.apply(request(0, 0, true, false)).is_err());
        assert!(table.apply(request(7, 2, true, false)).is_err());
        assert!(table
            .apply(request(SHM_LOCK_COUNT, 1, true, false))
            .is_err());
        assert!(table.apply(request(u16::MAX, 1, true, false)).is_err());
    }

    /// Releasing everything must return the table to its initial state.
    #[test]
    fn releasing_returns_to_the_initial_state() {
        let mut table = ShmLockTable::default();
        table.apply(request(0, 4, true, true)).unwrap();
        table.apply(request(4, 4, true, false)).unwrap();
        table.apply(request(0, 4, false, true)).unwrap();
        table.apply(request(4, 4, false, false)).unwrap();
        assert!(table.is_unlocked());
    }
}
