//! Checked binary primitives, identifiers, buffers, limits, and the stable
//! error model shared by every inillucent layer.
//!
//! Invariant: nothing in this crate can panic, wrap, or allocate without being
//! asked to. Every function that reads attacker-controlled bytes - a database
//! page, a journal frame, a varint from a record - returns a `DbError` instead
//! of indexing out of bounds or overflowing, and every allocation is fallible.
//!
//! This is the bottom of the crate dependency graph the SQLite feature-parity
//! design lays out. It has no internal dependencies and no third-party
//! dependencies at all, so a bug in the layers above can never be blamed on
//! something underneath them.
//!
//! That design's crate table names `inillucent-value` and `inillucent-vfs` as the two
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

pub mod budget;
pub mod buffer;
pub mod bytes;
pub mod checksum;
pub mod deflate;
pub mod error;
pub mod hash;
pub mod ids;
pub mod json;
pub mod limits;
pub mod page;
pub mod probe;
pub mod rng;
pub mod sha3;
// The one skip helper, below every crate that needs one.
//
// **It is here because of the layering contract, not in spite of it
// (task-1969, 4.6).** A production crate may not depend on the test harness,
// so `inillucent-driver`, `inillucent-cli` and `inillucent-tree` each wrote
// their own `eprintln!` and were dropped by the strict runner's classifier.
// This crate is below all of them. The feature keeps it out of a shipped
// build; each consumer turns it on from its own `[dev-dependencies]`.
#[cfg(any(test, feature = "testing"))]
pub mod testing;
pub mod varint;

pub use error::{DbError, DbResult, ExtendedCode, PrimaryCode, Unwind};

/// The implementation phase that filled this crate in, as named by the TDD.
pub const IMPLEMENTATION_PHASE: &str = "phase 1: VFS, binary primitives, and simulator";

/// The SQLite release every parity claim in this workspace is measured against.
pub const REFERENCE_SQLITE_VERSION: &str = "3.53.4";

/// What this build reports about itself.
///
/// The choices a caller can act on, not a transcription of SQLite's list: an
/// option naming a subsystem this engine does not have would be a claim about
/// somebody else's build.
///
/// It lives here rather than beside `PRAGMA compile_options` because
/// `sqlite_compileoption_get` and `sqlite_compileoption_used` answer questions
/// about the same list from `inillucent-scalar`, which sits below the engine.
/// One list, three readers.
pub const COMPILE_OPTIONS: &[&str] = &[
    "ENGINE=inillucent",
    "THREADSAFE=0",
    "DEFAULT_JOURNAL_MODE=wal",
    "DEFAULT_LOCKING_MODE=exclusive",
    "DEFAULT_ENCODING=UTF-8",
    "ENABLE_FTS3",
    "ENABLE_FTS4",
    "ENABLE_FTS5",
    "ENABLE_GEOPOLY",
    "ENABLE_RTREE",
    "ENABLE_DBSTAT_VTAB",
    "ENABLE_DBPAGE_VTAB",
    "ENABLE_STMTVTAB",
    "ENABLE_JSON1",
    "ENABLE_VECTOR",
    "ENABLE_HNSW",
    "ENABLE_SESSION",
    "OMIT_SHARED_CACHE",
];

/// Returns whether an option is set, in `sqlite_compileoption_used`'s terms.
///
/// The reference accepts the name with or without its `SQLITE_` prefix and
/// compares the whole entry, `NAME=value` included, so `THREADSAFE` does not
/// match `THREADSAFE=0`.
///
/// @param name - the option, with or without the `SQLITE_` prefix
pub fn compile_option_used(name: &str) -> bool {
    let wanted = name.strip_prefix("SQLITE_").unwrap_or(name);
    COMPILE_OPTIONS.contains(&wanted)
}
