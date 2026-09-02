//! Locks, autocommit, savepoints, rollback journal, WAL, checkpoints, and recovery.
//!
//! Invariant: transactions own visibility and durability and never evaluate SQL.
//!
//! Status: this crate is a declared layer of the engine graph described in
//! `tasks/task-1781-sqlite-feature-parity-tdd.md`. Its behaviour lands in
//! phase 7: single-database rollback transactions and DML; task-1782 creates it so that the dependency-direction contract is
//! enforced from the first commit rather than retrofitted once edges exist.

#![forbid(unsafe_code)]
#![deny(missing_docs)]

/// The implementation phase that fills this crate in, as named by the TDD.
pub const IMPLEMENTATION_PHASE: &str = "phase 7: single-database rollback transactions and DML";
