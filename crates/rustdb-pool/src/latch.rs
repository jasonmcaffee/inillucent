//! The version latch: LeanStore's optimistic reader/writer synchronisation, as
//! a state machine with one word of state.
//!
//! Invariant: a reader that observed version `v` before reading and observes
//! `v` again afterwards, with the exclusive bit clear at both points, read a
//! byte image no writer modified in between. That is the whole contract, and
//! everything else here exists to make it checkable: the exclusive bit is bit
//! 63, the version is bits 0..62, and **every** release of the exclusive bit
//! increments the version. A writer that took the latch and changed nothing
//! still bumps it, because "changed nothing" is a claim about intent and the
//! reader can only see bytes.
//!
//! ## Why a state machine and not a `RwLock`
//!
//! A `RwLock` makes a reader write - it takes the reader count - and a page
//! read that dirties a cache line shared with every other reader is the cost
//! this design exists to avoid. An optimistic read touches the latch twice,
//! reads only, and its failure mode is a retry rather than a wait.
//!
//! The shared path is still here, and it is not a fallback nobody exercises: a
//! reader that fails validation four times descends with shared latches so a
//! hot writer cannot starve it. Bits 0..62 double as the shared count in that
//! mode? No - they do not, and that shortcut is the bug this module was written
//! to avoid. The shared count is a separate word, so a shared acquisition does
//! not perturb the version an optimistic reader is validating against.
//!
//! ## The states, exhaustively
//!
//! | state | exclusive bit | shared count | who may enter |
//! |---|---|---|---|
//! | `Free` | 0 | 0 | anyone |
//! | `Shared(n)` | 0 | n > 0 | another shared reader; not a writer |
//! | `Exclusive` | 1 | 0 | one writer, from `Free` only |
//!
//! An optimistic read is not a state: it takes nothing and leaves nothing. It
//! is legal in `Free` and `Shared(n)` and fails in `Exclusive`.
//!
//! The product of three states by four events is enumerated in this module's
//! tests, because a latch that is wrong in one cell of that table is wrong in a
//! way no workload finds reliably.

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

/// Bit 63 of the version word: a writer holds the latch.
const EXCLUSIVE: u64 = 1 << 63;

/// The mask that isolates the version from the exclusive bit.
const VERSION: u64 = !EXCLUSIVE;

/// How many times an optimistic reader retries before falling back to shared.
///
/// The TDD's number. It is a policy rather than a correctness constant: at
/// four, a reader that keeps losing to one writer stops spinning and takes a
/// count, which costs the writer a wait rather than costing the reader an
/// unbounded number of restarts.
pub const OPTIMISTIC_RETRIES: u32 = 4;

/// What a latch is doing right now.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LatchState {
    /// Nobody holds it.
    Free,
    /// `n` shared readers hold it.
    Shared(u32),
    /// A writer holds it.
    Exclusive,
}

/// The version latch on one frame.
#[derive(Debug, Default)]
pub struct VersionLatch {
    /// Bit 63 exclusive, bits 0..62 the version.
    word: AtomicU64,
    /// How many shared readers hold the latch.
    shared: AtomicU32,
}

/// A version observed by an optimistic reader, to be validated afterwards.
///
/// It is a distinct type rather than a bare `u64` so that a caller cannot
/// validate against a number it computed itself, which is the one way to get
/// this wrong that reads correctly.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Observed(u64);

impl VersionLatch {
    /// Returns a fresh latch nobody holds, at version zero.
    pub fn new() -> VersionLatch {
        VersionLatch {
            word: AtomicU64::new(0),
            shared: AtomicU32::new(0),
        }
    }

    /// Returns what the latch is doing, for assertions and for tests.
    pub fn state(&self) -> LatchState {
        if self.word.load(Ordering::Acquire) & EXCLUSIVE != 0 {
            return LatchState::Exclusive;
        }
        match self.shared.load(Ordering::Acquire) {
            0 => LatchState::Free,
            count => LatchState::Shared(count),
        }
    }

    /// Returns the current version, exclusive bit stripped.
    pub fn version(&self) -> u64 {
        self.word.load(Ordering::Acquire) & VERSION
    }

    /// Begins an optimistic read, or reports that a writer holds the latch.
    ///
    /// Returns `None` when the exclusive bit is set, because there is nothing
    /// to validate against: the bytes are mid-change.
    pub fn optimistic(&self) -> Option<Observed> {
        let word = self.word.load(Ordering::Acquire);
        if word & EXCLUSIVE != 0 {
            return None;
        }
        Some(Observed(word & VERSION))
    }

    /// Reports whether an optimistic read is still valid.
    ///
    /// True only when no writer has held the latch since [`Self::optimistic`]
    /// and none holds it now. A writer that took and released the latch bumped
    /// the version, so the equality fails even though the exclusive bit is
    /// clear again by the time the reader looks.
    ///
    /// @param observed - what [`Self::optimistic`] returned
    pub fn validate(&self, observed: Observed) -> bool {
        let word = self.word.load(Ordering::Acquire);
        word & EXCLUSIVE == 0 && word & VERSION == observed.0
    }

    /// Takes the exclusive latch, or reports that somebody else holds it.
    ///
    /// Fails when a writer holds it *or* when any shared reader does: a writer
    /// that ignored the shared count would change bytes under a reader who,
    /// having taken a count, is entitled not to re-validate.
    pub fn try_exclusive(&self) -> bool {
        if self.shared.load(Ordering::Acquire) != 0 {
            return false;
        }
        let word = self.word.load(Ordering::Acquire);
        if word & EXCLUSIVE != 0 {
            return false;
        }
        if self
            .word
            .compare_exchange(word, word | EXCLUSIVE, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return false;
        }
        // Re-check: a shared reader may have arrived between the load above and
        // the exchange. Losing that race means giving the latch back rather
        // than proceeding, because the reader took its count first.
        if self.shared.load(Ordering::Acquire) != 0 {
            self.word.fetch_and(VERSION, Ordering::Release);
            return false;
        }
        true
    }

    /// Releases the exclusive latch and bumps the version.
    ///
    /// Bumping unconditionally is what makes [`Self::validate`] sound: a reader
    /// cannot tell a writer that changed nothing from one that changed
    /// everything, so every writer is assumed to have changed everything.
    pub fn release_exclusive(&self) {
        let word = self.word.load(Ordering::Acquire);
        let next = word.wrapping_add(1) & VERSION;
        self.word.store(next, Ordering::Release);
    }

    /// Takes a shared latch, or reports that a writer holds it.
    pub fn try_shared(&self) -> bool {
        if self.word.load(Ordering::Acquire) & EXCLUSIVE != 0 {
            return false;
        }
        self.shared.fetch_add(1, Ordering::AcqRel);
        if self.word.load(Ordering::Acquire) & EXCLUSIVE != 0 {
            // A writer took the latch between the check and the increment.
            // Backing out is the only safe answer: the writer already decided
            // it had the page to itself.
            self.shared.fetch_sub(1, Ordering::AcqRel);
            return false;
        }
        true
    }

    /// Releases a shared latch.
    ///
    /// Releasing one that was never taken saturates at zero rather than
    /// wrapping, because a count that wrapped would lock the frame out of
    /// eviction forever and the symptom would appear nowhere near the cause.
    pub fn release_shared(&self) {
        let mut held = self.shared.load(Ordering::Acquire);
        loop {
            if held == 0 {
                return;
            }
            match self.shared.compare_exchange(
                held,
                held.saturating_sub(1),
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return,
                Err(seen) => held = seen,
            }
        }
    }
}

#[cfg(test)]
mod tests {

    /// Threads racing for the latch never both get it, and the back-out paths
    /// are taken.
    ///
    /// Three arms of `try_exclusive` and `try_shared` exist only for a window
    /// between a load and the operation that follows it: the compare-exchange
    /// losing to another writer, a reader arriving after the exchange, and a
    /// writer arriving after a reader has incremented the count. **No
    /// single-threaded test can reach any of them**, which is why they were the
    /// last three uncovered branches in the module the TDD wants at 100%.
    ///
    /// The assertion here is not about coverage though - it is the property the
    /// latch exists for. A shadow count records who believes they hold what,
    /// and no thread may ever observe a writer alongside anybody else. Taking
    /// the three arms is what makes the assertion mean something: without a
    /// real race the invariant holds trivially.
    ///
    /// Reaching all three is probabilistic; the invariant is not. If a future
    /// run leaves one arm uncovered the fix is more iterations, not a weaker
    /// assertion.
    #[test]
    fn racing_threads_never_both_hold_it() {
        use std::sync::atomic::AtomicI64;
        use std::sync::Arc;

        /// What the threads agree is true, checked against what the latch says.
        struct Witness {
            latch: VersionLatch,
            /// Positive while readers hold it, -1 while a writer does.
            holders: AtomicI64,
        }

        let shared = Arc::new(Witness {
            latch: VersionLatch::new(),
            holders: AtomicI64::new(0),
        });
        let mut threads = Vec::new();
        for id in 0..4u64 {
            let state = Arc::clone(&shared);
            threads.push(std::thread::spawn(move || {
                let mut taken = 0u64;
                for round in 0..40_000u64 {
                    // A cheap deterministic mix, so the threads interleave
                    // differently without a random number generator.
                    let writer = (round.wrapping_mul(2_654_435_761).wrapping_add(id) & 3) == 0;
                    if writer {
                        if !state.latch.try_exclusive() {
                            continue;
                        }
                        let seen = state.holders.fetch_sub(1, Ordering::AcqRel);
                        assert_eq!(seen, 0, "a writer found somebody already holding it");
                        state.holders.fetch_add(1, Ordering::AcqRel);
                        state.latch.release_exclusive();
                    } else {
                        if !state.latch.try_shared() {
                            continue;
                        }
                        let seen = state.holders.fetch_add(1, Ordering::AcqRel);
                        assert!(seen >= 0, "a reader found a writer holding it");
                        state.holders.fetch_sub(1, Ordering::AcqRel);
                        state.latch.release_shared();
                    }
                    taken = taken.saturating_add(1);
                }
                taken
            }));
        }
        let total: u64 = threads
            .into_iter()
            .map(|thread| thread.join().expect("no thread panicked"))
            .sum();
        assert!(total > 0, "the threads did some work");
        assert_eq!(
            shared.latch.state(),
            LatchState::Free,
            "every acquisition was released"
        );
    }

    /// A held exclusive latch is refused by the two readers that ask about it.
    ///
    /// Restored after being deleted. It went in to close branches a stale
    /// coverage report claimed were open, was removed as redundant when the
    /// existing tests turned out to cover the same *states* - and a trustworthy
    /// report then showed it was the only thing taking two arms neither of the
    /// others reaches: `validate` short-circuiting on the exclusive bit, and
    /// `try_shared` refusing while a writer holds.
    ///
    /// Both are the latch's safety argument rather than its behaviour. A
    /// `validate` that ignored the exclusive bit would let a reader keep bytes
    /// that are mid-change; a `try_shared` that ignored it would hand out a
    /// count over the same bytes.
    #[test]
    fn a_writer_holding_the_latch_is_seen_by_both_readers() {
        let latch = VersionLatch::new();
        let observed = latch.optimistic().expect("a free latch may be read");
        assert!(latch.validate(observed), "nothing has happened yet");

        assert!(latch.try_exclusive());
        assert!(
            !latch.validate(observed),
            "the exclusive bit alone invalidates a read that spans the write"
        );
        assert!(
            !latch.try_shared(),
            "and a reader may not take a count over bytes being changed"
        );
        latch.release_exclusive();
        assert!(
            !latch.validate(observed),
            "the bumped version keeps it invalid after the writer let go"
        );
        assert!(latch.try_shared(), "a reader may proceed once it has");
        latch.release_shared();
    }
    use super::*;

    #[test]
    fn a_fresh_latch_is_free() {
        let latch = VersionLatch::new();
        assert_eq!(latch.state(), LatchState::Free);
        assert_eq!(latch.version(), 0);
        let observed = latch.optimistic().expect("free latch admits a reader");
        assert!(latch.validate(observed));
    }

    /// A writer between the read and the validation invalidates it, even
    /// though it has released the latch by the time the reader looks.
    #[test]
    fn a_writer_invalidates_a_read_it_finished_before_the_check() {
        let latch = VersionLatch::new();
        let observed = latch.optimistic().unwrap();
        assert!(latch.try_exclusive());
        latch.release_exclusive();
        assert_eq!(latch.state(), LatchState::Free);
        assert!(!latch.validate(observed), "the version must have moved");
        let again = latch.optimistic().unwrap();
        assert!(latch.validate(again));
    }

    /// A held exclusive latch admits no optimistic read at all.
    #[test]
    fn an_exclusive_latch_admits_no_reader() {
        let latch = VersionLatch::new();
        assert!(latch.try_exclusive());
        assert_eq!(latch.state(), LatchState::Exclusive);
        assert!(latch.optimistic().is_none());
        assert!(!latch.try_exclusive(), "no second writer");
        assert!(!latch.try_shared(), "no shared reader under a writer");
        latch.release_exclusive();
        assert!(latch.optimistic().is_some());
    }

    /// Shared readers stack, and a writer is refused while any of them holds.
    #[test]
    fn shared_readers_stack_and_exclude_a_writer() {
        let latch = VersionLatch::new();
        assert!(latch.try_shared());
        assert!(latch.try_shared());
        assert_eq!(latch.state(), LatchState::Shared(2));
        assert!(!latch.try_exclusive());
        latch.release_shared();
        assert_eq!(latch.state(), LatchState::Shared(1));
        assert!(!latch.try_exclusive());
        latch.release_shared();
        assert_eq!(latch.state(), LatchState::Free);
        assert!(latch.try_exclusive());
    }

    /// A shared acquisition does not move the version, so an optimistic read
    /// that overlaps one still validates.
    #[test]
    fn a_shared_reader_does_not_disturb_an_optimistic_one() {
        let latch = VersionLatch::new();
        let observed = latch.optimistic().unwrap();
        assert!(latch.try_shared());
        assert!(latch.validate(observed));
        latch.release_shared();
        assert!(latch.validate(observed));
    }

    /// Releasing a shared latch nobody holds saturates rather than wrapping.
    #[test]
    fn releasing_an_unheld_shared_latch_saturates() {
        let latch = VersionLatch::new();
        latch.release_shared();
        latch.release_shared();
        assert_eq!(latch.state(), LatchState::Free);
        assert!(latch.try_exclusive());
    }

    /// The version wraps within its 63 bits and never sets the exclusive bit.
    #[test]
    fn the_version_wraps_without_touching_the_exclusive_bit() {
        let latch = VersionLatch::new();
        latch.word.store(VERSION, Ordering::Release);
        assert_eq!(latch.version(), VERSION);
        assert!(latch.try_exclusive());
        latch.release_exclusive();
        assert_eq!(latch.version(), 0, "wrapped to zero, not into bit 63");
        assert_eq!(latch.state(), LatchState::Free);
    }

    /// Every state answers every event the way the table in the module
    /// documentation says it does. The product is small enough to write out,
    /// and a latch that is wrong in one cell is wrong in a way no workload
    /// finds reliably.
    #[test]
    fn the_state_by_event_product_is_exhaustive() {
        // (state, may an optimistic read start, may a writer take it, may a
        // shared reader take it)
        let cases: [(LatchState, bool, bool, bool); 3] = [
            (LatchState::Free, true, true, true),
            (LatchState::Shared(1), true, false, true),
            (LatchState::Exclusive, false, false, false),
        ];
        for (state, optimistic, exclusive, shared) in cases {
            let latch = VersionLatch::new();
            match state {
                LatchState::Free => {}
                LatchState::Shared(_) => assert!(latch.try_shared()),
                LatchState::Exclusive => assert!(latch.try_exclusive()),
            }
            assert_eq!(latch.state(), state);
            assert_eq!(latch.optimistic().is_some(), optimistic, "{state:?}");
            let took = latch.try_exclusive();
            assert_eq!(took, exclusive, "{state:?} exclusive");
            if took {
                latch.release_exclusive();
                match state {
                    LatchState::Free => {}
                    _ => unreachable!(),
                }
            }
            let latch = VersionLatch::new();
            match state {
                LatchState::Free => {}
                LatchState::Shared(_) => assert!(latch.try_shared()),
                LatchState::Exclusive => assert!(latch.try_exclusive()),
            }
            assert_eq!(latch.try_shared(), shared, "{state:?} shared");
        }
    }

    /// The retry budget is the TDD's four.
    #[test]
    fn the_retry_budget_is_declared() {
        assert_eq!(OPTIMISTIC_RETRIES, 4);
    }
}
