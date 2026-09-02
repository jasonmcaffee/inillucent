//! Scalar, aggregate, and window functions, collations, virtual tables, and loadable extension policy.
//!
//! Invariant: an extension observes engine state only through the registered contracts it was handed.
//!
//! Status: this crate is a declared layer of the engine graph described in
//! `tasks/task-1781-sqlite-feature-parity-tdd.md`. Its behaviour lands in
//! phase 11: full built-ins, PRAGMAs, virtual tables, FTS5, and R-Tree; task-1782 creates it so that the dependency-direction contract is
//! enforced from the first commit rather than retrofitted once edges exist.

#![forbid(unsafe_code)]
#![deny(missing_docs)]

/// The implementation phase that fills this crate in, as named by the TDD.
pub const IMPLEMENTATION_PHASE: &str = "phase 11: full built-ins, PRAGMAs, virtual tables, FTS5, and R-Tree";
