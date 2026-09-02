//! SQL virtual table and index-method bridge to the existing BM25, vector, and hybrid ranking core.
//!
//! Invariant: search state commits and rolls back with the transaction that produced it.
//!
//! Status: this crate is a declared layer of the engine graph described in
//! `tasks/task-1781-sqlite-feature-parity-tdd.md`. Its behaviour lands in
//! phase 13: transactional rust-db search and legacy migration; task-1782 creates it so that the dependency-direction contract is
//! enforced from the first commit rather than retrofitted once edges exist.

#![forbid(unsafe_code)]
#![deny(missing_docs)]

/// The implementation phase that fills this crate in, as named by the TDD.
pub const IMPLEMENTATION_PHASE: &str = "phase 13: transactional rust-db search and legacy migration";
