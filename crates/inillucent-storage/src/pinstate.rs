//! The pin count and CLOCK reference bit a cached frame is evicted by.
//!
//! Invariant: a frame with an outstanding pin is never evicted. Everything in
//! this module exists to make that one sentence checkable, because it is the
//! invariant the page cache's safety rests on and it is enforced by two atomics
//! rather than by a lock: a pin is taken and released without the shard's mutex,
//! and the eviction sweep reads them while holding it.
//!
//! It is a separate module so that `loom` can drive the real thing. Loom
//! replaces the atomics with instrumented ones and runs every interleaving a
//! thread could observe, which it can only do for types built out of *its*
//! atomics - so the alternative was a hand-written copy of this protocol in a
//! test, which is a copy that drifts from the code it claims to model. The
//! swap below is the whole of the difference between a Loom build and an
//! ordinary one; `PageFrame` holds one of these and calls the same methods
//! either way.
//!
//! The protocol, in the order the cache runs it:
//!
//! - **acquire**, under the shard's mutex: set `referenced`, then increment
//!   `pins`. The reference bit is set first so a sweep that sees the pin also
//!   sees the bit.
//! - **release**, with no lock held: decrement `pins`. This is the only
//!   operation that runs outside the mutex, and the only one a sweep can race.
//! - **sweep**, under the shard's mutex: refuse if pinned, refuse if the frame
//!   is not evictable, then clear `referenced` and refuse if it had been set.
//!   Only a frame that is unpinned, evictable and unreferenced is taken.

#[cfg(not(loom))]
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};
#[cfg(loom)]
use loom::sync::atomic::{AtomicBool, AtomicU32, Ordering};

/// What one sweep decided about one frame.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Sweep {
    /// The frame is in use and stays.
    Pinned,
    /// The frame holds a change the durability protocol has not written yet.
    NotEvictable,
    /// The frame was referenced since the last sweep; its bit is now clear and
    /// the next sweep may take it.
    Referenced,
    /// The frame may be evicted.
    Evict,
}

/// The two atomics a frame is evicted by.
#[derive(Debug)]
pub struct PinState {
    /// How many pins are outstanding.
    pins: AtomicU32,
    /// The CLOCK reference bit.
    referenced: AtomicBool,
}

impl PinState {
    /// Builds the state a freshly inserted frame has: one pin, referenced.
    ///
    /// A frame is inserted because somebody wants it, so it arrives pinned by
    /// that caller rather than at zero and immediately evictable.
    pub fn held() -> PinState {
        PinState {
            pins: AtomicU32::new(1),
            referenced: AtomicBool::new(true),
        }
    }

    /// Takes a pin, and marks the frame as used.
    ///
    /// The reference bit is set before the count rises so that a sweep which
    /// observes the pin cannot have missed the bit.
    pub fn acquire(&self) {
        self.referenced.store(true, Ordering::Release);
        self.pins.fetch_add(1, Ordering::AcqRel);
    }

    /// Releases a pin.
    pub fn release(&self) {
        self.pins.fetch_sub(1, Ordering::AcqRel);
    }

    /// Returns how many pins are outstanding.
    pub fn pins(&self) -> u32 {
        self.pins.load(Ordering::Acquire)
    }

    /// Reports whether the reference bit is set, without clearing it.
    pub fn referenced(&self) -> bool {
        self.referenced.load(Ordering::Acquire)
    }

    /// Marks the frame as used without taking a pin.
    pub fn touch(&self) {
        self.referenced.store(true, Ordering::Release);
    }

    /// Decides what one CLOCK sweep does with this frame.
    ///
    /// `evictable` is the frame's durability state, which lives on the frame
    /// rather than here: a dirty frame cannot leave until it has been written,
    /// and evicting one would lose a change only this cache holds.
    ///
    /// The pin check comes first and is the one that matters. It is sound
    /// despite running without the pin path's cooperation because `acquire`
    /// only ever runs under the same mutex this sweep holds: a pin cannot
    /// appear while the sweep is deciding. A pin can be *released*
    /// concurrently, which only lowers the count, so a frame this refuses may
    /// be taken by the next sweep - never the other way round.
    pub fn sweep(&self, evictable: bool) -> Sweep {
        if self.pins.load(Ordering::Acquire) != 0 {
            return Sweep::Pinned;
        }
        if !evictable {
            return Sweep::NotEvictable;
        }
        if self.referenced.swap(false, Ordering::AcqRel) {
            return Sweep::Referenced;
        }
        Sweep::Evict
    }
}

#[cfg(all(test, not(loom)))]
mod tests {
    use super::*;

    /// A fresh frame is held by whoever inserted it.
    #[test]
    fn a_new_frame_arrives_pinned_and_referenced() {
        let state = PinState::held();
        assert_eq!(state.pins(), 1);
        assert!(state.referenced());
        assert_eq!(state.sweep(true), Sweep::Pinned);
    }

    /// The reference bit buys a frame exactly one sweep.
    #[test]
    fn the_reference_bit_survives_one_sweep_and_not_two() {
        let state = PinState::held();
        state.release();
        assert_eq!(state.sweep(true), Sweep::Referenced);
        assert!(!state.referenced());
        assert_eq!(state.sweep(true), Sweep::Evict);
    }

    /// A frame the durability protocol still owns is never taken, however long
    /// it has sat unreferenced.
    #[test]
    fn a_frame_that_is_not_evictable_is_never_taken() {
        let state = PinState::held();
        state.release();
        assert_eq!(state.sweep(false), Sweep::NotEvictable);
        assert_eq!(state.sweep(false), Sweep::NotEvictable);
        // And the reference bit was not spent deciding that.
        assert!(state.referenced());
    }

    /// Pins nest, and every one of them has to go.
    #[test]
    fn every_pin_has_to_be_released_before_a_sweep_will_take_it() {
        let state = PinState::held();
        state.acquire();
        state.release();
        assert_eq!(state.sweep(true), Sweep::Pinned);
        state.release();
        assert_eq!(state.sweep(true), Sweep::Referenced);
        assert_eq!(state.sweep(true), Sweep::Evict);
    }
}

#[cfg(all(test, loom))]
mod loom_tests {
    use super::*;
    use loom::sync::{Arc, Mutex};

    /// A sweep never takes a frame a pin is outstanding on.
    ///
    /// One thread holds the frame, uses it and lets go. Another runs sweeps
    /// under the mutex that `acquire` would also need. Loom runs every
    /// interleaving of the two and the assertion is checked in all of them: if
    /// the sweep ever answers `Evict` while the pin is alive, the frame the
    /// cache is about to drop is one somebody is reading.
    #[test]
    fn a_pinned_frame_is_never_evicted() {
        loom::model(|| {
            let state = Arc::new(PinState::held());
            let evicted = Arc::new(Mutex::new(false));
            let lock = Arc::new(Mutex::new(()));

            let holder = {
                let state = Arc::clone(&state);
                let evicted = Arc::clone(&evicted);
                loom::thread::spawn(move || {
                    // The frame is in use for as long as this pin is held, so
                    // nothing may have evicted it in that window.
                    let taken = *evicted.lock().unwrap();
                    assert!(!taken, "the frame was evicted while it was pinned");
                    state.release();
                })
            };

            let sweeper = {
                let state = Arc::clone(&state);
                let evicted = Arc::clone(&evicted);
                let lock = Arc::clone(&lock);
                loom::thread::spawn(move || {
                    // Two sweeps, because the first only clears the reference
                    // bit. Both run under the mutex the acquire path takes.
                    let mut swept = 0;
                    while swept < 2 {
                        let guard = lock.lock().unwrap();
                        if state.sweep(true) == Sweep::Evict {
                            *evicted.lock().unwrap() = true;
                        }
                        drop(guard);
                        swept += 1;
                    }
                })
            };

            holder.join().unwrap();
            sweeper.join().unwrap();
        });
    }

    /// Two threads taking and dropping pins leave the count where it started.
    ///
    /// The count is what the sweep reads, so a lost update here is a frame
    /// evicted under a reader or one that is never evicted at all.
    #[test]
    fn concurrent_pins_do_not_lose_a_count() {
        loom::model(|| {
            let state = Arc::new(PinState::held());
            let lock = Arc::new(Mutex::new(()));

            let first = {
                let state = Arc::clone(&state);
                let lock = Arc::clone(&lock);
                loom::thread::spawn(move || {
                    let guard = lock.lock().unwrap();
                    state.acquire();
                    drop(guard);
                    state.release();
                })
            };

            let second = {
                let state = Arc::clone(&state);
                let lock = Arc::clone(&lock);
                loom::thread::spawn(move || {
                    let guard = lock.lock().unwrap();
                    state.acquire();
                    drop(guard);
                    state.release();
                })
            };

            first.join().unwrap();
            second.join().unwrap();
            assert_eq!(state.pins(), 1, "the original pin is the only one left");
        });
    }
}
