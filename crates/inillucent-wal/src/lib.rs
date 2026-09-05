//! The write-ahead log of the rearchitected engine: segments, the record
//! codec, group commit, and recovery.
//!
//! Invariant: **nothing is durable before its log record is.** Every other
//! durability rule in the engine is a consequence of that one, and this crate
//! is where it is decided. A page may be written to the data file only when its
//! LSN is below [`Wal::durable_end`], and a commit may be acknowledged only when
//! its record is durable under the configured [`Synchronous`] policy.
//!
//! ## Why the log does not know what a page is
//!
//! This crate sits at layer 2, on `inillucent-base` and `inillucent-vfs`, and
//! **beside** `inillucent-pool` rather than above or below it. A record carries
//! a page's *number*; the crate that owns pages turns it back into a `PageId`.
//!
//! The alternative - a log that depends on the pool so its records could carry
//! `PageId` - reads better and is the wrong way round. The log is written before
//! any page is, and a dependency edge from the log to the pool would say the
//! opposite. The write-ahead rule crosses that gap in the one direction that
//! costs nothing: the pool is *told* a durable LSN and refuses to write past it.
//! A number is not a dependency.
//!
//! Recovery is here too, and applying a record is not: [`Redo`] is a trait the
//! caller implements, so the scan - the two passes, the committed-transaction
//! set, the torn-tail rule, the page-LSN rule - lives with the format it is
//! reading, while the crate that owns pages and trees does the applying. That
//! is what keeps this crate below the storage engine while still holding the
//! algorithm whose invariants the phase gate is about.
//!
//! ## The four things this crate is held to
//!
//! - [`record`] at **100% branch coverage**. It is a decoder reading bytes a
//!   crash wrote, held to the same bar as the interior and key codecs.
//! - Recovery is **idempotent**: recovering twice produces the same file, byte
//!   for byte. `tests/recovery.rs` asserts exactly that.
//! - A **corrupt log never panics**. `inspect` runs the whole scan over
//!   arbitrary bytes with no data file behind it, and it is what the
//!   `corrupt_wal` fuzz target drives.
//! - **Durability order** is a property a mutant can break, so the campaign that
//!   kills those mutants drives this crate through `inillucent-sim`'s failpoints
//!   rather than through a mock.

#![forbid(unsafe_code)]
#![deny(missing_docs)]
#![deny(clippy::indexing_slicing)]
#![deny(clippy::unwrap_used)]
#![deny(clippy::expect_used)]
#![deny(clippy::panic)]
// As in `inillucent-pool` and `inillucent-tree`: `arithmetic_side_effects` is not
// denied here because a crate that computes byte offsets needs arithmetic in
// every function, and the protection that matters is that no computed offset is
// ever *used* without a checked slice - which `indexing_slicing` enforces
// directly.
#![cfg_attr(
    test,
    allow(
        clippy::expect_used,
        clippy::indexing_slicing,
        clippy::panic,
        clippy::unwrap_used
    )
)]

pub mod record;
pub mod recover;
pub mod segment;
pub mod writer;

pub use record::{Body, PageList, Record, Structural, MAX_RECORD_BYTES};
pub use recover::{recover, truncate_after, DryRun, Recovered, RecoveryStart, Redo};
pub use segment::{SegmentHeader, SEGMENT_BYTES};
pub use writer::{Synchronous, Wal, WalOptions, WalStats, FIRST_LSN};

/// The implementation phase that filled this crate in, as named by the TDD.
pub const IMPLEMENTATION_PHASE: &str = "phase 3: writes, durability, and MVCC";
