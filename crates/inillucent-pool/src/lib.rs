//! The buffer pool of the rearchitected engine: frames, version latches,
//! pointer swizzling, the cooling FIFO, writeback, the free map and blob
//! extents.
//!
//! Invariant: this crate is the only thing that decides where a page lives. A
//! tree above it names pages and children; a file below it holds bytes at
//! offsets; nothing else knows both. That is what makes "the same tree, in a
//! 64-frame pool and in a 4,096-frame pool" a configuration rather than a
//! rewrite - which is the fairness question Phase 2 exists to answer, because a
//! comparison against SQLite at a 2 MB `cache_size` has to be able to state the
//! cache size on both sides.
//!
//! This is the rearchitecture's `inillucent-pool`, delivered in
//! Phase 2. It sits above `inillucent-vfs` and below `inillucent-tree`, and the page
//! primitives that Phase 1 put in `inillucent-tree::page` moved down here with it -
//! unchanged, and re-exported from their old path, because a page header is a
//! property of the file format rather than of the B+tree.
//!
//! ## What is here and what is Phase 3
//!
//! Here: the pool, the eviction machinery, the on-disk format's meta page, free
//! map and blob extents, and a [`file::Database`] that ties them together for a
//! **load, checkpoint, close** lifecycle. Phase 3 adds the WAL, and with it the
//! write-ahead rule that a dirty page may not be written before the log record
//! that describes it. The rule's *seam* is here already - every page write goes
//! through `pool::Pool::writeback` - so Phase 3 adds a condition rather than a
//! caller.
//!
//! ## Threading
//!
//! The pool is single-threaded by construction in Phase 2: its bookkeeping is a
//! `RefCell` and its frames are `RefCell`s, so it is neither `Send` nor `Sync`.
//! The version latch is nevertheless atomic and is the real state machine, for
//! two reasons: it is the piece a multi-threaded checkpointer will need
//! unchanged, and a latch is exactly the sort of thing that is easy to get right
//! while writing it and impossible to retrofit. `docs/invariants/layering.toml`
//! records the crate; the TDD's Phase 3 makes it shared.

#![forbid(unsafe_code)]
#![deny(missing_docs)]
#![deny(clippy::indexing_slicing)]
#![deny(clippy::unwrap_used)]
#![deny(clippy::expect_used)]
#![deny(clippy::panic)]
// As in `inillucent-tree`: `arithmetic_side_effects` is not denied here because a
// crate that computes page offsets needs arithmetic in every function, and the
// protection that matters is that no computed offset is ever *used* without a
// checked slice - which `indexing_slicing` enforces directly.
#![cfg_attr(
    test,
    allow(
        clippy::expect_used,
        clippy::indexing_slicing,
        clippy::panic,
        clippy::unwrap_used
    )
)]

pub mod extent;
pub mod file;
pub mod freemap;
pub mod interior;
pub mod journal;
pub mod latch;
pub mod meta;
pub mod page;
pub mod pool;
pub mod swip;

pub use extent::{ExtentRef, EXTENT_REF_BYTES};
pub use file::{Database, Options};
pub use freemap::FreeMap;
pub use interior::{InteriorBuilder, InteriorRef};
pub use latch::{LatchState, VersionLatch, OPTIMISTIC_RETRIES};
pub use meta::Meta;
pub use page::{PageId, PageKind, PageSize, COMMON_HEADER};
pub use pool::{FrameState, PageGuard, Pool, PoolStats};
pub use swip::Swip;

/// The implementation phase that filled this crate in, as named by the TDD.
pub const IMPLEMENTATION_PHASE: &str = "phase 2: a real read-only engine on a buffer pool";
