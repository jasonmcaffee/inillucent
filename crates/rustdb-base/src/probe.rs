//! Counters a profiling harness fills in and a profiling build of the engine
//! writes to.
//!
//! Invariant: a normal build never touches anything here, and nothing in the
//! engine ever reads one of these to decide what to do. They exist so a harness
//! that installs a counting global allocator can attribute time and allocations
//! to the bytecode instruction that caused them - which needs a place both the
//! binary that owns the allocator and the crate that runs the instruction can
//! reach, and that place has to be the crate they both already depend on.
//!
//! The opcode tables are written only by a build with the virtual machine's
//! `opcode-probe` feature on, which is off by default and is never on in a
//! shipped build.

use core::sync::atomic::AtomicU64;

/// How many opcodes the tables below have room for.
///
/// The instruction set is smaller than this and a new opcode does not need the
/// number changed; an index past the end is dropped rather than wrapped, so a
/// table that is too small loses a row of a profile instead of corrupting one.
pub const OPCODE_SLOTS: usize = 256;

/// How many heap allocations the process has made, when something is counting.
///
/// The harness that installs a counting allocator increments this; every other
/// build leaves it at zero, which reads as "nobody is counting" rather than as
/// "no allocations happened", because a profile is only compared against itself.
pub static ALLOCATIONS: AtomicU64 = AtomicU64::new(0);

/// How many times each opcode has run.
pub static OPCODE_RUNS: [AtomicU64; OPCODE_SLOTS] = [const { AtomicU64::new(0) }; OPCODE_SLOTS];

/// How many nanoseconds each opcode has spent.
pub static OPCODE_NANOS: [AtomicU64; OPCODE_SLOTS] = [const { AtomicU64::new(0) }; OPCODE_SLOTS];

/// How many heap allocations each opcode has made.
pub static OPCODE_ALLOCATIONS: [AtomicU64; OPCODE_SLOTS] =
    [const { AtomicU64::new(0) }; OPCODE_SLOTS];

/// Clears the opcode tables, so a measurement starts from zero.
pub fn reset_opcodes() {
    for slot in 0..OPCODE_SLOTS {
        if let (Some(runs), Some(nanos), Some(allocations)) = (
            OPCODE_RUNS.get(slot),
            OPCODE_NANOS.get(slot),
            OPCODE_ALLOCATIONS.get(slot),
        ) {
            runs.store(0, core::sync::atomic::Ordering::Relaxed);
            nanos.store(0, core::sync::atomic::Ordering::Relaxed);
            allocations.store(0, core::sync::atomic::Ordering::Relaxed);
        }
    }
}
