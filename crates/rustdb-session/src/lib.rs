//! Connection-local state, statement lifecycle, hooks, PRAGMA state, and interrupt handling.
//!
//! Invariant: connection-local mutation is serialized and no connection state leaks across connections.
//!
//! Status: this crate is a declared layer of the engine graph described in
//! `tasks/task-1781-sqlite-feature-parity-tdd.md`. Its behaviour lands in
//! phase 6: catalog, binder, expression VM, and read-only SELECT; task-1782 creates it so that the dependency-direction contract is
//! enforced from the first commit rather than retrofitted once edges exist.

#![forbid(unsafe_code)]
#![deny(missing_docs)]

/// The implementation phase that fills this crate in, as named by the TDD.
pub const IMPLEMENTATION_PHASE: &str = "phase 6: catalog, binder, expression VM, and read-only SELECT";
