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
//! The stage tables are written only by a build with `inillucent-storage`'s
//! `opcode-probe` feature on, which is off by default and is never on in a
//! shipped build.
//!
//! **The per-opcode tables went with the machine they profiled (task-1946, M3).**
//! `OPCODE_SLOTS`, `OPCODE_RUNS`, `OPCODE_NANOS`, `OPCODE_ALLOCATIONS` and
//! `reset_opcodes` attributed time and allocations to a bytecode instruction,
//! and there is no bytecode engine: `inillucent-vm` was deleted with the rest of
//! the old one. Nothing had written one of them since.

use core::sync::atomic::AtomicU64;

/// How many heap allocations the process has made, when something is counting.
///
/// The harness that installs a counting allocator increments this; every other
/// build leaves it at zero, which reads as "nobody is counting" rather than as
/// "no allocations happened", because a profile is only compared against itself.
pub static ALLOCATIONS: AtomicU64 = AtomicU64::new(0);

/// How many named stages the tables below have room for.
pub const STAGE_SLOTS: usize = 16;

/// How many nanoseconds each named stage has spent.
///
/// A stage is whatever a profiling build decided to bracket - the halves of a
/// page edit, say. The names live with the code that records them, because a
/// number here means nothing without the bracket that produced it.
pub static STAGE_NANOS: [AtomicU64; STAGE_SLOTS] = [const { AtomicU64::new(0) }; STAGE_SLOTS];

/// How many times each named stage ran.
pub static STAGE_RUNS: [AtomicU64; STAGE_SLOTS] = [const { AtomicU64::new(0) }; STAGE_SLOTS];

/// How many heap allocations each named stage made.
pub static STAGE_ALLOCATIONS: [AtomicU64; STAGE_SLOTS] = [const { AtomicU64::new(0) }; STAGE_SLOTS];

/// Adds one run of a stage, with the allocations it made.
pub fn record_stage_allocating(slot: usize, nanos: u64, allocations: u64) {
    record_stage(slot, nanos);
    if let Some(total) = STAGE_ALLOCATIONS.get(slot) {
        total.fetch_add(allocations, core::sync::atomic::Ordering::Relaxed);
    }
}

/// Adds one run of a stage.
pub fn record_stage(slot: usize, nanos: u64) {
    if let (Some(runs), Some(total)) = (STAGE_RUNS.get(slot), STAGE_NANOS.get(slot)) {
        runs.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
        total.fetch_add(nanos, core::sync::atomic::Ordering::Relaxed);
    }
}

/// How many rounds a seeded property loop should run.
///
/// Answers `full` on every ordinary build. Under Miri it answers a hundredth of
/// it, floored at five hundred, because the interpreter executes every
/// instruction and a two-hundred-thousand-round loop that takes milliseconds
/// natively takes hours there - so a Miri run either samples or never finishes,
/// and never finishing is the same as not running it.
///
/// The properties these loops check - no input panics, every value round-trips
/// canonically - are checked against Miri's much stricter memory model at any
/// sample size, and the full sample still runs everywhere else.
pub fn sample_rounds(full: usize) -> usize {
    if cfg!(miri) {
        (full / 100).max(500).min(full)
    } else {
        full
    }
}
