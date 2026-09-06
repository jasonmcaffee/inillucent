//! Transactions and MVCC: snapshots, the writer slot, undo buffers, the version
//! log, savepoints and the commit gate.
//!
//! Invariant: **a reader never sees a writer's uncommitted state and never sees
//! a row change under it inside one snapshot.** Both halves come from the same
//! arrangement rather than from two mechanisms:
//!
//! - A snapshot is a commit timestamp taken once, and a change becomes visible
//!   only when a `cts` is assigned to it - which happens under the commit gate,
//!   after the writer has decided to commit. There is no moment at which an
//!   uncommitted change has a timestamp a reader could be at or above.
//! - The version log holds the **before-image** of every key changed since the
//!   oldest active snapshot, so a reader that lands on a page a later
//!   transaction has overwritten reads what the row held at its own timestamp.
//!   The page holds the newest version; the log holds what a reader is owed.
//!
//! ## The four things this crate is held to
//!
//! - `inillucent-txn` and recovery at **at least 95% branch coverage**.
//! - **`busy_timeout` and each `synchronous` policy has a test that shows the
//!   behaviour changing**, not just the setting being stored. The `synchronous`
//!   half lives in `inillucent-wal`, where the syncs are counted; the
//!   `busy_timeout` half is in [`slot`], where the waits are timed.
//! - **A savepoint rolls back exactly what came after it** - and closes the
//!   savepoints opened inside it, because a position past the end of the undo
//!   buffer is not a savepoint.
//! - **The version log is collected**, so a long reader costs memory bounded by
//!   what it is holding open rather than by how long it has been open.
//!
//! ## Where the layers sit
//!
//! This crate is layer 4: above `inillucent-pool` (2), `inillucent-wal` (2) and
//! `inillucent-tree` (3), beside `inillucent-sql` and depending on neither it
//! nor anything above. It is the first crate that holds a database file and a
//! log at the same time, which is why the write-ahead rule is tied together
//! here: [`engine::Engine`] is the only thing that calls
//! `Pool::set_durable_lsn`, and it calls it after every sync of the log and
//! never before one.

#![forbid(unsafe_code)]
#![deny(missing_docs)]
#![deny(clippy::indexing_slicing)]
#![deny(clippy::unwrap_used)]
#![deny(clippy::expect_used)]
#![deny(clippy::panic)]
#![cfg_attr(
    test,
    allow(
        clippy::expect_used,
        clippy::indexing_slicing,
        clippy::panic,
        clippy::unwrap_used
    )
)]

pub mod engine;
pub mod redo;
pub mod slot;
pub mod undo;
pub mod version;

pub use engine::{
    Begin, Engine, EngineOptions, EngineStats, RecordingUndo, RefuseUndo, Transaction, UndoSink,
};
pub use redo::{Applier, RedoStats, RefuseRows, RowRedo};
pub use slot::{SlotStats, WriterGuard, WriterSlot};
pub use undo::{Undo, UndoBuffer};
pub use version::{Clock, Cts, Snapshot, TxnId, VersionLog, Visible, FIRST_CTS};

/// The implementation phase that filled this crate in, as named by the TDD.
pub const IMPLEMENTATION_PHASE: &str = "phase 3: writes, durability, and MVCC";
