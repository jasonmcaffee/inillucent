//! The B+tree of the rearchitected engine: PAX leaves, memcmp-comparable keys,
//! and a tree that owns its pages.
//!
//! Invariant: a leaf page is self-describing and every read of one is bounds
//! checked against the page's own header before a byte of payload is touched.
//! A page arrives from a file, and a file arrives from anywhere, so a corrupt
//! or hostile page must produce a `DbError` and never a panic, a wrong answer
//! or an out-of-bounds read. `#![forbid(unsafe_code)]` makes the last of those
//! a compiler guarantee rather than a review promise.
//!
//! This crate is the rearchitecture plan's `inillucent-tree`, which
//! replaces `inillucent-storage`. Phase 1 builds the leaf codec and an in-memory
//! tree with no buffer pool and no disk, because the phase gate is a scan
//! measurement and a pool would only add I/O the gate is not about. Phase 2
//! moves the same leaf codec onto `inillucent-pool` frames; nothing in
//! [`leaf`] or [`key`] knows where its bytes came from, which is what makes
//! that move a change of owner rather than a rewrite.
//!
//! ## Why the columns are read as bytes rather than as `&[i64]`
//!
//! A PAX leaf stores a fixed-width column as a run of 8-byte little-endian
//! values inside the page. Reinterpreting those bytes as a `&[i64]` needs
//! `unsafe`, and the house style forbids it. The alternative -
//! `chunks_exact(8)` and `from_le_bytes` - was measured against a real `&[i64]`
//! under the workspace's own release profile before this crate was written:
//! 0.75 ns/row against 0.74 ns/row for the two-column aggregate the phase gate
//! is about, with `target-cpu=native` giving 0.22 against 0.22. The safe form
//! costs nothing, so there is no `unsafe` here and no argument to have about
//! whether there should be.

#![forbid(unsafe_code)]
#![deny(missing_docs)]
#![deny(clippy::indexing_slicing)]
#![deny(clippy::unwrap_used)]
#![deny(clippy::expect_used)]
#![deny(clippy::panic)]
// `clippy::arithmetic_side_effects` is denied in `inillucent-base` and not here,
// which is the same line `inillucent-storage` draws. A crate that reads pages needs
// offset arithmetic in every function, and the protection that matters is that
// no computed offset is ever *used* without a checked slice - which
// `indexing_slicing` enforces directly. Wrapping an offset that is then bounds
// checked produces an error, not an out-of-bounds read.
#![cfg_attr(
    test,
    allow(
        clippy::expect_used,
        clippy::indexing_slicing,
        clippy::panic,
        clippy::unwrap_used
    )
)]

pub mod datum;
pub mod key;
pub mod keyenc;
pub mod leaf;
pub mod mutate;
pub mod page;
pub mod paged;
pub mod tree;
pub mod types;
pub mod write;

pub use datum::Datum;
pub use leaf::{LeafBuilder, LeafRef};
pub use mutate::{Applied, DeltaPlan, LeafMut};
pub use page::{PageId, PageSize};
pub use paged::{Descent, KeyBytes, KeyEncoding, PagedTree};
pub use tree::{ScanCursor, Tree};
pub use types::{ColumnSpec, PhysicalType};
pub use write::{Located, NoLog, TreeLog, WriteStats};
