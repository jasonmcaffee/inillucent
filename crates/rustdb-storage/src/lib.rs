//! SQLite file codec, pager, page cache, B-trees, overflow chains, freelist, and vacuum.
//!
//! Invariant: storage understands pages and byte records only; it never sees SQL, tables, or expressions.
//!
//! Status: this crate is a declared layer of the engine graph described in
//! `tasks/task-1781-sqlite-feature-parity-tdd.md`. Its behaviour lands in
//! phase 3: read-only header, pager, page cache, and B-tree; task-1782 creates it so that the dependency-direction contract is
//! enforced from the first commit rather than retrofitted once edges exist.

#![forbid(unsafe_code)]
#![deny(missing_docs)]

/// The implementation phase that fills this crate in, as named by the TDD.
pub const IMPLEMENTATION_PHASE: &str = "phase 3: read-only header, pager, page cache, and B-tree";
