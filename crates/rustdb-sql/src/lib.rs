//! First-party lexer, parser, AST, binder, semantic rewrites, and logical and physical plans.
//!
//! Invariant: the SQL front end is pure parsing, binding, planning, and compilation; it performs no I/O.
//!
//! Status: this crate is a declared layer of the engine graph described in
//! `tasks/task-1781-sqlite-feature-parity-tdd.md`. Its behaviour lands in
//! phase 5: lexer, parser, AST, and syntax parity; task-1782 creates it so that the dependency-direction contract is
//! enforced from the first commit rather than retrofitted once edges exist.

#![forbid(unsafe_code)]
#![deny(missing_docs)]

/// The implementation phase that fills this crate in, as named by the TDD.
pub const IMPLEMENTATION_PHASE: &str = "phase 5: lexer, parser, AST, and syntax parity";
