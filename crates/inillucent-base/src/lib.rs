//! Checked binary primitives, identifiers, buffers, limits, and the stable
//! error model shared by every inillucent layer.
//!
//! Invariant: nothing in this crate can panic, wrap, or allocate without being
//! asked to. Every function that reads attacker-controlled bytes - a database
//! page, a journal frame, a varint from a record - returns a `DbError` instead
//! of indexing out of bounds or overflowing, and every allocation is fallible.
//!
//! This is the bottom of the dependency graph described in
//! `tasks/task-1781-sqlite-feature-parity-tdd.md`. It has no internal
//! dependencies and no third-party dependencies at all, so a bug in the layers
//! above can never be blamed on something underneath them.
//!
//! The TDD's crate table names `inillucent-value` and `inillucent-vfs` as the two
//! leaves. Phase 1 needs checked integers, varints, checksums, page arithmetic
//! and the error table *before* values exist, and `inillucent-vfs` needs the same
//! error table, so those primitives live here in a leaf below both rather than
//! being duplicated or forced into a crate whose own contents land in phase 2.
//! `docs/invariants/layering.toml` records the refinement and enforces it.

#![forbid(unsafe_code)]
#![deny(missing_docs)]
#![deny(clippy::indexing_slicing)]
#![deny(clippy::unwrap_used)]
#![deny(clippy::expect_used)]
#![deny(clippy::panic)]
#![deny(clippy::arithmetic_side_effects)]
// Tests assert on exact values and are allowed to fail loudly; the bans above
// exist to keep panics and wrapping out of paths that read persistent bytes.
#![cfg_attr(
    test,
    allow(
        clippy::arithmetic_side_effects,
        clippy::expect_used,
        clippy::indexing_slicing,
        clippy::panic,
        clippy::unwrap_used
    )
)]

pub mod buffer;
pub mod bytes;
pub mod checksum;
pub mod error;
pub mod hash;
pub mod ids;
pub mod limits;
pub mod page;
pub mod probe;
pub mod rng;
pub mod varint;

pub use error::{DbError, DbResult, ExtendedCode, PrimaryCode};

/// The implementation phase that filled this crate in, as named by the TDD.
pub const IMPLEMENTATION_PHASE: &str = "phase 1: VFS, binary primitives, and simulator";

/// The SQLite release every parity claim in this workspace is measured against.
pub const REFERENCE_SQLITE_VERSION: &str = "3.53.4";
