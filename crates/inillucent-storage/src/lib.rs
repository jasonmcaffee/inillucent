//! SQLite file codec, pager, page cache, B-trees, overflow chains, freelist,
//! and vacuum.
//!
//! Invariant: storage understands pages and byte records only; it never sees
//! SQL, tables, or expressions. It knows what a record's *bytes* mean, because
//! an index cursor cannot compare two keys without decoding them, and it knows
//! nothing about what a column is called or what a statement asked for.
//!
//! Phase 3 fills in the read-only half. A `Pager` opens a database file
//! read-only, takes a SHARED lock, and serves validated pages through a
//! bounded, sharded, pinned cache. A `BTreeCursor` walks table and index trees
//! in both directions, seeks by rowid or by key, and follows overflow chains.
//! `schema` reads `sqlite_schema` far enough to find root pages, and `check`
//! runs the two levels of integrity check over the whole file. Nothing here
//! writes a byte; the mutation half arrives in phase 4 and extends these
//! structures rather than replacing them.
//!
//! Module map, in the order a read passes through them:
//!
//! - [`header`] - the 100-byte database header and the page-count rule;
//! - [`cache`] - the sharded, pinned, bounded page cache;
//! - [`pager`] - open, lock, read, and sticky failure;
//! - [`btree`] - the four page kinds, their cells, and page validation;
//! - [`overflow`] - reading a payload that did not fit on its page;
//! - [`cursor`] - seeks and scans over a table or an index;
//! - [`databases`] - which pager a statement means by "database three";
//! - [`journal`] - the hook that makes a commit crash-atomic;
//! - [`wal`] - the hook that makes a page readable from a log instead;
//! - [`schema`] - reading `sqlite_schema` for root pages;
//! - [`check`] - the raw quick and integrity checks.

#![forbid(unsafe_code)]
#![deny(missing_docs)]
#![deny(clippy::indexing_slicing)]
#![deny(clippy::unwrap_used)]
#![deny(clippy::expect_used)]
#![deny(clippy::panic)]
// Tests assert on exact values and are allowed to fail loudly; the bans above
// exist to keep panics and wrapping out of paths that read persistent bytes.
#![cfg_attr(
    test,
    allow(
        clippy::expect_used,
        clippy::indexing_slicing,
        clippy::panic,
        clippy::unwrap_used
    )
)]

pub mod alloc;
pub mod btree;
pub mod cache;
/// The integrity checker, which nothing in a shipped binary calls.
///
/// **Off by default (task-1946, M1).** 767 lines with no production caller:
/// `PRAGMA integrity_check` on the shipping engine is `inillucent-engine`'s own
/// walk over `inillucent-tree`, not this. What uses it is this crate's own test
/// modules in `mutate.rs` and `vacuum.rs`, and five suites in
/// `inillucent-compat` that grade what the retired pager did with a file - so it
/// is a test tool, and a release build should not carry it.
///
/// It is not simply moved into the test crate because two of its seven callers
/// are inside this crate, and `inillucent-compat` depends on this crate rather
/// than the other way round.
#[cfg(any(test, feature = "check"))]
pub mod check;
pub mod cursor;
pub mod databases;
pub mod edit;
pub mod header;
pub mod journal;
pub mod mutate;
pub mod overflow;
pub mod pager;
pub mod pinstate;
pub mod ptrmap;
pub mod schema;
pub mod vacuum;
pub mod wal;

pub use btree::{BTreePage, CellRef, PageKind, PageLayout};
pub use cache::{CacheCounters, PageCache, PageKey, PagePin, PageVersion};
pub use cursor::{BTreeCursor, CursorState, SavedPosition, SeekBias, TreeKind};
pub use databases::TEMP_DATABASE;
pub use edit::{encode_cell, rewrite_page};
pub use header::{DatabaseHeader, VacuumMode};
pub use journal::{Journal, JournalStats};
pub use pager::{NewDatabase, Pager, PagerCounters, PagerOptions, PagerState};
pub use schema::{SchemaKind, SchemaObject};
pub use wal::{CheckpointMode, CheckpointOutcome, WalSnapshot, WalStats, WriteAheadLog};

/// The implementation phase that filled this crate in, as named by the TDD.
pub const IMPLEMENTATION_PHASE: &str = "phase 3: read-only header, pager, page cache, and B-tree";
