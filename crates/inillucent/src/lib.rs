//! The stable public Rust facade: `Database`, `Connection`, `Statement`, `Row`.
//!
//! Invariant: **this name reaches the new engine.** `inillucent::Database::open`
//! is what an application outside this workspace writes against, and it used
//! to reach `inillucent-session` - the SQLite-file-format engine the
//! rearchitecture replaced. It now reaches `inillucent-engine`, which is the
//! engine every gate in this repository measures and every suite tests.
//!
//! ## What that changed, and what it did not
//!
//! The surface is **smaller**, and deliberately: the old facade carried backup,
//! incremental blob access, `serialize`/`deserialize`, the update, commit and
//! rollback hooks, a progress handler and a pager-counter accessor. Every one
//! of those is a feature of the engine underneath rather than of the facade,
//! and the new engine does not have them - the rearchitecture dropped the file
//! format they were mostly for, and multi-process access with them. A facade that kept
//! the method names and answered "unsupported" would be a worse lie than one
//! that does not have them, because the compiler would stop saying so.
//!
//! What is here is what an application actually writes: open a file, connect,
//! prepare, bind, step, read a row, register a function or a collation.
//!
//! ## Where the old one went
//!
//! `crates/inillucent-legacy`, unchanged, still over `inillucent-session`. It
//! has exactly one consumer left - `inillucent-capi`, the `sqlite3_*` C ABI
//! over the old engine - and that crate's replacement is the driver in
//! `drivers/`. When the driver lands, `inillucent-legacy`, `inillucent-capi`,
//! `inillucent-session`, `inillucent-vm`, `inillucent-transaction` and
//! `inillucent-storage` go together; until it does, deleting them would leave
//! the workspace with no C ABI at all. That is the one thing this facade
//! change does not do on its own, and it is written down rather than
//! quietly skipped.

#![forbid(unsafe_code)]
#![deny(missing_docs)]
#![deny(clippy::indexing_slicing)]
#![deny(clippy::unwrap_used)]
#![deny(clippy::expect_used)]
#![deny(clippy::panic)]

/// The error type, its codes, and the result alias every call returns.
pub use inillucent_base::{DbError, DbResult, ExtendedCode, PrimaryCode};

/// How many bytes at the front of a script are not part of a statement.
///
/// Whitespace, statement separators and both comment forms. Re-exported because a caller that has
/// to decide whether text holds another statement should ask the parser rather than write a second
/// scanner: counting semicolons is how a trigger body gets split in the middle and a trailing
/// comment gets called a statement.
pub use inillucent_engine::connect::leading_trivia;

/// A database file, a connection to one, and a compiled statement.
///
/// Re-exported rather than wrapped. The old facade wrapped its engine's types
/// so that the two surfaces could differ; they do not differ any more, and a
/// wrapper whose every method is `self.inner.same_thing()` is a file to keep in
/// step for nothing.
pub use inillucent_engine::connect::{Connection, Database, Statement};

/// The value a row holds, and the affinity rules it is read under.
pub use inillucent_value::{cast, Affinity, Collation, StorageClass, TextEncoding, Value};

/// What a row's cells are, as the engine hands them back.
///
/// The new engine answers with `OwnedDatum` rather than with `Value`: a datum is
/// what a tree holds and a value is what an expression computes, and the two
/// were the same type only because the old engine had no trees.
pub use inillucent_tree::datum::OwnedDatum;

/// The planner optimizations [`Connection::disable_optimizations`] switches off.
pub use inillucent_sql::plan::Levers;

/// What an application registers a function or a collation as.
///
/// The registry lives a layer down; a caller that registers a function has to
/// be able to name the flags it registers with, and should not have to depend
/// on the extension crate to do it.
pub use inillucent_engine::extensions;

/// The bound parameters a statement is run with.
///
/// Re-exported so a caller naming a parameter set does not have to depend on
/// the executor crate directly.
pub use inillucent_engine::BoundParams as Params;
