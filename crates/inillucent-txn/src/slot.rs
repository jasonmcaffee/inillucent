//! The writer slot and `busy_timeout`.
//!
//! Invariant: **one writer at a time, and readers never wait.** The slot is the
//! whole of the first half; the second half is a property of the design rather
//! than of this module - a reader takes a snapshot and reads pages and the
//! version log, neither of which this slot guards.
//!
//! ## What `busy_timeout` is, and what it is not
//!
//! It is how long a would-be writer waits for the slot before giving up with the
//! dialect's `SQLITE_BUSY` equivalent. Zero means "do not wait at all", which is
//! SQLite's default and is the setting that turns a contended write into an
//! immediate error rather than a stall.
//!
//! It is **not** a retry count and not a poll interval. The waiter parks on a
//! condition variable and is woken when the slot is released, so a slot that
//! frees after one millisecond is taken after one millisecond and not at the
//! next tick of some sweep. That matters for the acceptance: the test that shows
//! the timeout doing something has to distinguish "waited and got it" from
//! "waited the whole timeout and failed", and a poll-based implementation would
//! blur the two.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use inillucent_base::error::DbError;
use inillucent_base::{DbResult, PrimaryCode};

use crate::version::TxnId;

/// What the slot has done, for tests and reports.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct SlotStats {
    /// Acquisitions that found the slot free.
    pub uncontended: u64,
    /// Acquisitions that had to wait for another writer.
    pub waited: u64,
    /// Acquisitions that gave up after `busy_timeout`.
    pub timed_out: u64,
}

/// The state behind the slot.
#[derive(Debug, Default)]
struct State {
    holder: Option<TxnId>,
    stats: SlotStats,
}

/// The single writer slot.
#[derive(Debug)]
pub struct WriterSlot {
    state: Mutex<State>,
    free: Condvar,
    /// How long a waiter waits, in milliseconds.
    busy_timeout_ms: AtomicU64,
}

/// Proof that the holder owns the writer slot.
///
/// Releasing on drop rather than on a call, for the reason the snapshot
/// registry gives: a release that is a call is a release that stops happening
/// the first time a caller returns early, and the failure mode is a database
/// nobody can write to until the process exits.
pub struct WriterGuard {
    slot: Arc<WriterSlot>,
    txn: TxnId,
}

impl WriterGuard {
    /// Returns the transaction holding the slot.
    pub fn txn(&self) -> TxnId {
        self.txn
    }
}

impl std::fmt::Debug for WriterGuard {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("WriterGuard")
            .field("txn", &self.txn.0)
            .finish()
    }
}

impl Drop for WriterGuard {
    fn drop(&mut self) {
        self.slot.release(self.txn);
    }
}

impl Default for WriterSlot {
    fn default() -> WriterSlot {
        WriterSlot::new()
    }
}

impl WriterSlot {
    /// Returns a free slot with `busy_timeout` at zero.
    ///
    /// Zero is SQLite's default and is the honest one: a caller that has not
    /// said how long it is willing to wait has not said it is willing to wait.
    pub fn new() -> WriterSlot {
        WriterSlot {
            state: Mutex::new(State::default()),
            free: Condvar::new(),
            busy_timeout_ms: AtomicU64::new(0),
        }
    }

    /// Returns the current `busy_timeout` in milliseconds.
    pub fn busy_timeout_ms(&self) -> u64 {
        self.busy_timeout_ms.load(Ordering::Relaxed)
    }

    /// Sets `busy_timeout`.
    ///
    /// @param millis - how long a would-be writer waits, zero for not at all
    pub fn set_busy_timeout_ms(&self, millis: u64) {
        self.busy_timeout_ms.store(millis, Ordering::Relaxed);
    }

    /// Returns the counters.
    pub fn stats(&self) -> SlotStats {
        match self.state.lock() {
            Ok(state) => state.stats,
            Err(poisoned) => poisoned.into_inner().stats,
        }
    }

    /// Returns the transaction holding the slot, if any.
    pub fn holder(&self) -> Option<TxnId> {
        match self.state.lock() {
            Ok(state) => state.holder,
            Err(poisoned) => poisoned.into_inner().holder,
        }
    }

    /// Takes the slot, waiting up to `busy_timeout`.
    ///
    /// @param slot - the slot, so the guard can release it
    /// @param txn - the transaction taking it
    pub fn acquire(slot: &Arc<WriterSlot>, txn: TxnId) -> DbResult<WriterGuard> {
        let deadline = Instant::now()
            .checked_add(Duration::from_millis(slot.busy_timeout_ms()))
            .unwrap_or_else(Instant::now);
        let mut state = slot
            .state
            .lock()
            .map_err(|_| busy("the writer slot was poisoned by a panicking writer"))?;
        if state.holder.is_none() {
            state.holder = Some(txn);
            state.stats.uncontended = state.stats.uncontended.saturating_add(1);
            return Ok(WriterGuard {
                slot: Arc::clone(slot),
                txn,
            });
        }
        loop {
            let now = Instant::now();
            if now >= deadline {
                state.stats.timed_out = state.stats.timed_out.saturating_add(1);
                let held = state.holder.map(|held| held.0).unwrap_or(0);
                return Err(busy(format!(
                    "another transaction ({held}) is writing and the writer slot did not \
                     free within busy_timeout"
                )));
            }
            let (guard, _) = slot
                .free
                .wait_timeout(state, deadline.saturating_duration_since(now))
                .map_err(|_| busy("the writer slot was poisoned by a panicking writer"))?;
            state = guard;
            if state.holder.is_none() {
                state.holder = Some(txn);
                state.stats.waited = state.stats.waited.saturating_add(1);
                return Ok(WriterGuard {
                    slot: Arc::clone(slot),
                    txn,
                });
            }
        }
    }

    /// Frees the slot and wakes one waiter.
    ///
    /// @param txn - the transaction that held it
    fn release(&self, txn: TxnId) {
        let mut state = match self.state.lock() {
            Ok(state) => state,
            Err(poisoned) => poisoned.into_inner(),
        };
        // Only the holder can reach here: `release` is private and its one
        // caller is `WriterGuard::drop`, and a guard cannot be forged. So this
        // is an assertion rather than a condition - a condition would be a
        // branch no input can take, which is the one the coverage gate can only
        // ever be lied to about. `debug_assert` compiles out of the release
        // build the gate measures and stays in the one the tests run.
        debug_assert_eq!(
            state.holder,
            Some(txn),
            "the writer slot was released by a transaction that did not hold it"
        );
        state.holder = None;
        drop(state);
        self.free.notify_one();
    }
}

/// Builds the dialect's "another writer has it" error.
///
/// @param detail - what was going on
pub fn busy(detail: impl Into<String>) -> DbError {
    DbError::primary(PrimaryCode::Busy).with_detail(detail)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An uncontended acquisition takes the slot immediately.
    #[test]
    fn an_uncontended_acquisition_is_immediate() {
        let slot = Arc::new(WriterSlot::new());
        assert_eq!(slot.holder(), None);
        let guard = WriterSlot::acquire(&slot, TxnId(1)).expect("a free slot");
        assert_eq!(slot.holder(), Some(TxnId(1)));
        assert_eq!(guard.txn(), TxnId(1));
        assert_eq!(slot.stats().uncontended, 1);
        drop(guard);
        assert_eq!(slot.holder(), None);
    }

    /// With `busy_timeout` at zero a second writer is refused at once.
    ///
    /// "At once" is measured, not assumed: a implementation that slept before
    /// checking would pass an assertion about the error and fail this one.
    #[test]
    fn a_zero_busy_timeout_refuses_a_second_writer_without_waiting() {
        let slot = Arc::new(WriterSlot::new());
        let _held = WriterSlot::acquire(&slot, TxnId(1)).expect("a free slot");
        let started = Instant::now();
        let refused = WriterSlot::acquire(&slot, TxnId(2)).expect_err("the slot is taken");
        let waited = started.elapsed();
        assert_eq!(refused.code(), PrimaryCode::Busy);
        assert!(
            refused
                .detail()
                .unwrap_or_default()
                .contains("busy_timeout"),
            "the refusal must say what to change: {refused:?}"
        );
        assert!(
            waited < Duration::from_millis(50),
            "a zero timeout waited {waited:?}"
        );
        assert_eq!(slot.stats().timed_out, 1);
    }

    /// A non-zero `busy_timeout` waits, and gives up after roughly that long.
    ///
    /// This is the acceptance's "a test that shows the behaviour changing": the
    /// same second writer, against the same held slot, takes measurably longer
    /// to fail with a 300 ms timeout than with a zero one.
    #[test]
    fn a_busy_timeout_waits_before_it_gives_up() {
        let slot = Arc::new(WriterSlot::new());
        slot.set_busy_timeout_ms(300);
        let _held = WriterSlot::acquire(&slot, TxnId(1)).expect("a free slot");
        let started = Instant::now();
        let refused = WriterSlot::acquire(&slot, TxnId(2)).expect_err("the slot is taken");
        let waited = started.elapsed();
        assert_eq!(refused.code(), PrimaryCode::Busy);
        assert!(
            waited >= Duration::from_millis(250),
            "a 300 ms timeout gave up after {waited:?}"
        );
        assert!(
            waited < Duration::from_secs(3),
            "a 300 ms timeout waited {waited:?}, which is not a timeout"
        );
    }

    /// A waiter that is woken inside its timeout gets the slot.
    ///
    /// The distinction this makes is the one a poll-based implementation would
    /// blur: the slot is released after 50 ms, the waiter has 5,000 ms, and it
    /// takes the slot promptly rather than at the end of its window.
    #[test]
    fn a_waiter_woken_inside_its_timeout_takes_the_slot() {
        let slot = Arc::new(WriterSlot::new());
        slot.set_busy_timeout_ms(5_000);
        let held = WriterSlot::acquire(&slot, TxnId(1)).expect("a free slot");

        let releaser = {
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(50));
                drop(held);
            })
        };
        let started = Instant::now();
        let taken = WriterSlot::acquire(&slot, TxnId(2)).expect("the slot frees in time");
        let waited = started.elapsed();
        releaser.join().expect("the releasing thread");
        assert_eq!(taken.txn(), TxnId(2));
        assert!(
            waited < Duration::from_secs(2),
            "the waiter took {waited:?} to notice a slot freed after 50 ms, so it is \
             polling rather than being woken"
        );
        assert_eq!(slot.stats().waited, 1);
        assert_eq!(slot.stats().timed_out, 0);
    }

    /// Only one of many contending writers holds the slot at a time.
    #[test]
    fn only_one_writer_ever_holds_the_slot() {
        let slot = Arc::new(WriterSlot::new());
        slot.set_busy_timeout_ms(5_000);
        let concurrent = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let peak = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let mut handles = Vec::new();
        for index in 0..8u64 {
            let slot = Arc::clone(&slot);
            let concurrent = Arc::clone(&concurrent);
            let peak = Arc::clone(&peak);
            handles.push(std::thread::spawn(move || {
                for _ in 0..20 {
                    let guard = WriterSlot::acquire(&slot, TxnId(index)).expect("the slot frees");
                    let now = concurrent.fetch_add(1, Ordering::SeqCst) + 1;
                    peak.fetch_max(now, Ordering::SeqCst);
                    std::thread::yield_now();
                    concurrent.fetch_sub(1, Ordering::SeqCst);
                    drop(guard);
                }
            }));
        }
        for handle in handles {
            handle.join().expect("a writer thread");
        }
        assert_eq!(
            peak.load(Ordering::SeqCst),
            1,
            "two writers held the slot at once"
        );
        assert_eq!(slot.holder(), None);
    }

    /// The timeout is reported so a pragma can read it back.
    #[test]
    fn the_timeout_is_reported() {
        let slot = WriterSlot::new();
        assert_eq!(slot.busy_timeout_ms(), 0);
        slot.set_busy_timeout_ms(1_234);
        assert_eq!(slot.busy_timeout_ms(), 1_234);
    }
}
