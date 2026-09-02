//! First-party values, affinities, collations, expression primitives, and record codecs.
//!
//! Invariant: a value never carries a storage class its bytes do not justify, and every conversion is explicit and fallible.
//!
//! Status: this crate is a declared layer of the engine graph described in
//! `tasks/task-1781-sqlite-feature-parity-tdd.md`. Its behaviour lands in
//! phase 2: values, affinities, collations, and records; task-1782 creates it so that the dependency-direction contract is
//! enforced from the first commit rather than retrofitted once edges exist.

#![forbid(unsafe_code)]
#![deny(missing_docs)]

/// The implementation phase that fills this crate in, as named by the TDD.
pub const IMPLEMENTATION_PHASE: &str = "phase 2: values, affinities, collations, and records";
