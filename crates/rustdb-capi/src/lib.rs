//! Versioned SQLite C API compatibility surface and ABI probes.
//!
//! Invariant: the C surface is an adapter. Every behaviour it exposes is
//! implemented by an inner crate, so a symbol here can add marshalling and
//! lifetime bookkeeping but never engine semantics.
//!
//! Status: this crate is a declared layer of the engine graph described in
//! `tasks/task-1781-sqlite-feature-parity-tdd.md`. Its behaviour lands in
//! phase 12; task-1782 creates it so that the dependency-direction contract is
//! enforced from the first commit rather than retrofitted once edges exist.
//!
//! `crate-type` stays `lib` until phase 12 introduces the exported symbols; a
//! `cdylib` with no `#[no_mangle]` entry points would ship an empty artifact
//! and make the ABI probes look green while proving nothing.

#![deny(missing_docs)]

/// The implementation phase that fills this crate in, as named by the TDD.
pub const IMPLEMENTATION_PHASE: &str = "phase 12: C ABI and CLI completion";
