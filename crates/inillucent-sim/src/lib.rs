//! The deterministic simulator.
//!
//! Invariant: nothing in a simulated run depends on the wall clock, on real
//! randomness, on thread timing, or on the host file system. A run is a pure
//! function of its seed, its schedule, and its failpoint policy, so a failure
//! is reproduced by replaying those three rather than by rerunning until it
//! happens again.
//!
//! The simulator is test-only. No production crate depends on it, and the
//! dependency-direction test enforces that: it substitutes for `inillucent-vfs`
//! from the outside rather than being reachable from inside the engine.
//!
//! Turso's simulator is a non-normative design reference for this crate, listed
//! in `docs/reference-register.toml`. It answered "what is worth modelling";
//! everything here is written against that question and the SQLite file-format
//! and durability documentation, not against another project's source.

#![deny(missing_docs)]
#![deny(clippy::indexing_slicing)]
#![deny(clippy::unwrap_used)]
#![deny(clippy::expect_used)]
#![deny(clippy::panic)]
#![cfg_attr(
    test,
    allow(
        clippy::expect_used,
        clippy::indexing_slicing,
        clippy::panic,
        clippy::unwrap_used
    )
)]

pub mod failpoint;
pub mod media;
pub mod schedule;
pub mod sim_vfs;
pub mod trace;

pub use failpoint::{Failpoints, Failure, Policy, Site};
pub use media::{MediaModel, SectorOutcome, SimFileImage};
pub use schedule::{explore_two_actors, ActorId, Decisions, Scheduler};
pub use sim_vfs::{set_current_actor, CrashSnapshot, SimConfig, SimVfs};
pub use trace::{Event, Trace};

/// The implementation phase that filled this crate in, as named by the TDD.
pub const IMPLEMENTATION_PHASE: &str = "phase 1: VFS, binary primitives, and simulator";
