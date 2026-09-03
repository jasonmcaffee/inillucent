//! Locks, autocommit, savepoints, rollback journal, WAL, checkpoints, and recovery.
//!
//! Invariant: transactions own visibility and durability and never evaluate SQL.
//!
//! Phase 7 fills in the rollback half. [`journal`] is the file format and the
//! five journal modes; [`recovery`] is what a connection does when it finds one
//! of those files left behind by a process that is no longer running; [`state`]
//! is the connection-level machine that decides when a transaction begins, what
//! a savepoint costs, and what a statement failure is allowed to undo.
//!
//! The WAL half arrives in phase 10 and extends these structures rather than
//! replacing them: a journal mode is already a value the pager is configured
//! with, and `wal` is one more of them.
//!
//! Module map:
//!
//! - [`journal`] - the rollback journal codec, modes, and durability levels;
//! - [`recovery`] - hot-journal detection and replay, and the opener that runs
//!   it before a single page is exposed;
//! - [`state`] - autocommit, begin modes, savepoints, and the change counters.

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

pub mod journal;
pub mod recovery;
pub mod state;

pub use journal::{
    apply_playback, decode_journal, journal_is_hot, recover_hot_journal, DecodedJournal,
    JournalMode, JournalOptions, RollbackJournal, Synchronous,
};
pub use recovery::{open_database, DatabaseOptions};
pub use state::{
    BeginMode, ChangeCounters, ConflictAlgorithm, Savepoint, Transaction, TransactionState,
    TransactionStats,
};

/// The implementation phase that filled this crate in, as named by the TDD.
pub const IMPLEMENTATION_PHASE: &str = "phase 7: single-database rollback transactions and DML";
