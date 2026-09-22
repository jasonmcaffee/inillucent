//! The rearchitected engine as a database.
//!
//! Invariant: **this crate is the engine, and nothing above it is.** It owns
//! the buffer pool, the trees, the log, the catalog, DDL, the pragma set, the
//! statement path and the virtual-table host. The one thing it takes from the
//! retired SQLite file stack is `inillucent-sqlite-reader`, so that
//! `Database::import` can read a `.db` file; nothing on the statement path
//! reaches it. A caller reaches this engine by depending on this crate and on
//! nothing else.
//!
//! It was `inillucent_compat::newengine` through Phases 1 to 4, which built and
//! measured it inside the test-and-bench crate, because until
//! Phase 5 there was nothing above it to be the caller. That was the right
//! place to build it and the wrong place to ship it - `inillucent-migrate` cannot
//! depend on a test crate, and neither can a connection - so Phase 5 lifts it
//! out unchanged. `inillucent_compat::newengine` is a re-export of this crate, so
//! every gate, probe and campaign written against the old path still resolves
//! and still measures the same code.

#![forbid(unsafe_code)]
#![deny(missing_docs)]
// **Three of the four were missing and nothing said so (task-1932, H9).**
// `policy.rs` matched on the lint's *name*, which appears in the
// `cfg_attr(test, allow(...))` block below, so this crate satisfied the check
// while denying only `indexing_slicing`. The corrected check - which matches
// the whole `#![deny(...)]` attribute - found three crates in this position,
// not the one the review reported.
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

//! Driving the rearchitected engine from SQL, for the Phase 1 gate.
//!
//! Invariant: the SQL the new engine runs is byte-for-byte the SQL the old
//! engine and `sqlite-bench` run. It is parsed by the same parser, bound by the
//! same binder against a catalog built from the same fixture, and planned by the
//! same planner - only the physical execution below the plan is new. A harness
//! that rewrote the query on its way in would be measuring a different query,
//! which is the failure mode four instruments on this project already had.
//!
//! ## What this is and is not
//!
//! It is the stand-in for the session layer, which the TDD schedules for
//! Phase 3's transactions. There is no connection, no transaction and no
//! prepared-statement API here: a query is parsed, planned, prepared and run
//! against a database file through a buffer pool. That is enough to answer the
//! phase gates and it is deliberately not enough to be mistaken for the engine.
//!
//! ## The import, and the file it now writes
//!
//! Both engines get the same logical rows and neither gets the other's file.
//! [`ImportedDatabase::import`] reads the SQLite fixture through
//! `inillucent-sqlite-reader` and bulk-builds one PAX tree per table and per index
//! **into a real `.rdb` file**, then checkpoints it and reopens it through a
//! pool of a stated size. Phase 1 held the trees in a `Vec` and could not state
//! a cache size at all; that was the one fairness question its report left open,
//! and closing it is the point of the Phase 2 harness.
//!
//! The trees are keyed by the *SQLite root page* the fixture's schema recorded,
//! because that is the identifier the binder's catalog and the planner's access
//! paths already speak, so nothing has to guess a mapping by name.
//!
//! ## The pool size is a parameter, on both sides
//!
//! [`ImportedDatabase::import_with`] takes a frame count. The gate harness sets
//! it and prints it, and sets SQLite's `cache_size` to the same number of bytes,
//! so the scorecard's fairness section can state one cache configuration rather
//! than describing an asymmetry.
//!
//! ## The rowid, and why a table tree is one column wider than its record
//!
//! SQLite stores a rowid in the cell key, not in the record, and an
//! `INTEGER PRIMARY KEY` column's record field is therefore NULL. The new
//! format's rowid-clustered tree holds the rowid once, as its key. So an
//! imported table tree is `[rowid] ++ [record slots except the rowid alias]`,
//! and [`SourceLayout`] records which record slot became which tree column so a
//! bound expression can find its vector.

pub mod analyze;
pub mod attach;
mod checkpoint;
pub mod connect;
pub mod ddl;
mod engine;

// The six groups `ImportedDatabase`'s fields are made of, and the methods
// that touch only one of them. See `engine::state` (task-1962, A1 step 2).
pub(crate) use engine::state::{
    Compiled, Counters, Pragmas, Schema, SessionState, Storage, Writing,
};
// **Re-exported at the root because that is where they were (task-1962, A1
// step 1).** The split moved these two modules' free functions out of `lib.rs`;
// every call site in this crate names them unqualified, and a move that changes
// no behaviour has no business rewriting all of them.
// The four `inillucent-migrate` names, which were `pub` at this root before the
// split and stay `pub` at it after.
pub(crate) use engine::explain::*;
pub(crate) use engine::rowshape::*;
// The same again for the four modules task-1962's A1 step 3 cut out, for the
// same reason: the items moved, the call sites did not.
pub(crate) use engine::keys::*;
pub(crate) use engine::locks::*;
pub(crate) use engine::open::*;
pub(crate) use engine::statements::*;
pub(crate) use engine::tables::*;
pub(crate) use engine::write::*;
// `Statement` and `Outcome` were `pub` at this root before the split and stay
// `pub` at it after.
pub use engine::rowshape::{identity_columns, logical_row, source_layout_of, stored_as};
pub use engine::statements::{Outcome, Statement};
mod entries;
pub(crate) mod import;
mod inspect;
mod introspect;
mod marks;
pub mod multi;
mod plans;
use plans::Cached;
pub use plans::DEFAULT_STATEMENT_CACHE;
pub mod pragma;
pub mod readonly;
mod reattach;
mod rebuild;
pub mod recovery;
mod schema_write;
use recovery::OpenedFile;
/// Session-scoped `total_changes()` accounting.
mod session_changes;
mod spillfile;
/// The engine's half of a vector index a module owns.
mod vectors;
pub mod vtab;
pub use vtab::ModuleStages;

use std::collections::HashMap;
use std::path::PathBuf;

use plans::CachedQuery;

use inillucent_base::error::refusal;
use inillucent_base::limits::Limits;
pub use inillucent_base::DbResult;

// **Re-exported so a program above this one reaches them through the engine.**
// `inillucent-cli` is allowed to depend on the engine and not on what the engine
// is built from - which is the layering rule, and it is the right rule: a shell
// that named the pool directly would be a shell that had to be rebuilt when the
// pool moved. What it genuinely needs from below is the module contract, so it
// can register `fsdir` and `zipfile`; the pattern matcher, so `.lint` and
// `.sha3sum` fold a `LIKE` the same way the engine does; and the file systems,
// so `.vfslist` names them.
pub use inillucent_base as base;
use inillucent_catalog::load::table_from_create_sql;
use inillucent_catalog::paged::{
    attach_catalog, read_catalog, schema_layout, write_catalog, ObjectKind, SchemaEntry,
};
use inillucent_exec::dml::{self, Changes, Trees, WriteTarget};
use inillucent_exec::physical::{self, Params, SourceLayout, TreeCatalog};
pub use inillucent_ext as ext;
use inillucent_pool::{Database, Options, PageId, Pool};
/// The authorizer contract, re-exported for a caller above this crate.
///
/// **Re-exported rather than depended on directly**, because the layering
/// contract puts `inillucent-sql` below the engine and the shell above it:
/// `.auth` needs the trait and its two types and nothing else of the binder,
/// and reaching around the engine for them is the edge the contract exists to
/// forbid. The same shape as `ext` and `vfs` above.
pub use inillucent_sql::bind::{AuthAction, Authorization, Authorizer};
use inillucent_sql::catalog_view::{StaticCatalog, TableInfo};
use inillucent_sql::parser::parse_next_statement;
use inillucent_sql::plan::Levers;
use inillucent_sqlite_reader::SqliteFile;
use inillucent_tree::datum::Datum;
use inillucent_tree::types::ColumnSpec;
use inillucent_tree::write::TreeLog;
use inillucent_tree::PagedTree;
use inillucent_value::collation::Collation;
pub use inillucent_vfs as vfs;
use inillucent_vfs::{DbPath, OsVfs};
use inillucent_wal::{Body, Synchronous, Wal};

/// The error every call in this crate reports, and its result alias.
///
/// Re-exported for the reason [`BoundParams`] is: a caller of this crate needs
/// no other, and `inillucent-driver` classifies a failure by reading
/// [`DbError::unsupported`] and [`DbError::code`], both of which it has to be
/// able to name.
pub use inillucent_base::error::{DbError, PrimaryCode};

/// What an application-defined function is handed and what it answers with.
///
/// Re-exported for the reason [`BoundParams`] is: `inillucent-driver` registers
/// a function on a caller's behalf and has to be able to name the type the body
/// speaks, and naming `inillucent-value` to do it would make the driver's one
/// dependency edge into two.
pub use inillucent_value::Value as ExprValue;

/// What a function promises about itself, and how a collation orders two values.
pub use inillucent_ext::registry::{AggregateBody, FunctionFlags, ScalarBody};
pub use inillucent_value::collation::Comparator;

/// The bound-parameter map a statement is executed with.
///
/// Re-exported so that a caller of this crate needs no other. `inillucent-driver`
/// is the reason: its whole purpose is to be the one edge an application depends
/// on, and an application that had to name `inillucent-exec` to bind a parameter
/// would be depending on the layer the driver exists to hide.
pub use inillucent_exec::physical::Params as BoundParams;

/// What an application registers a function or a collation as.
///
/// Re-exported so the facade above can offer it without every caller having to
/// name the extension crate.
pub use inillucent_ext::registry as extensions;

/// A value going into a statement or coming out of one.
///
/// Re-exported for the reason [`BoundParams`] is: a caller has to be able to
/// name it, and making it reachable here is what keeps the driver's edge to
/// this crate one edge rather than three.
///
/// **Under its own name, not as `Value` (task-1961 A6, task-1969 6.3).** This
/// re-export used to rename the type to `Value`, twenty-four lines below the
/// one that renames `inillucent_value::Value` to `ExprValue` - so this crate
/// exported two different types and called one of them by the other's name.
/// With `inillucent_driver::Value` as well that was three confusable types
/// called `Value`, and A2 had already removed the facade's reason to need the
/// alias. The type is `OwnedDatum` everywhere else in the workspace, including
/// in this crate's own imports, and it is `OwnedDatum` here.
pub use inillucent_tree::datum::OwnedDatum;

/// How many frames the harness gives a pool when nothing says otherwise.
///
/// 4,096 frames at 32 KiB is 128 MiB, which holds every scorecard fixture at
/// every scale with room to spare - so the default is "resident", and a
/// measurement that wants to compare cache-limited behaviour sets it down
/// explicitly and says so.
pub const DEFAULT_FRAMES: usize = 4_096;

/// A fixture imported into the new engine's trees, in a real database file.
pub struct ImportedDatabase {
    /// What one connection has that its siblings do not.
    pub(crate) session_state: SessionState,
    /// The settings a `PRAGMA` or an `sqlite3_limit` call changes.
    ///
    /// **Shared rather than owned (task-1962, A1 step 3).** Every field is
    /// behind its own cell, so [`crate::connect::Database`] holds the same
    /// group and can answer `sqlite3_limit` while a statement is running. See
    /// [`Pragmas`].
    pub(crate) pragmas: std::rc::Rc<Pragmas>,
    /// The catalog, the trees it names, and what was derived from both.
    pub(crate) schema: Schema,
    /// The file, and everything that reads or writes a page of it.
    pub(crate) storage: Storage,
    /// What a transaction in progress has done so far.
    ///
    /// **Shared rather than owned (task-1962, A1 step 3).** Every one of the
    /// group's ten fields is behind its own cell, so nothing here needs `&mut`
    /// to write it, and the `Rc` lets [`crate::connect::Database`] hold the
    /// same writer the engine holds. A caller asking whether a transaction is
    /// open then reads it directly, rather than borrowing the engine - which is
    /// what aborted the process when the question was asked from inside a
    /// callback the engine was already running.
    pub(crate) writing: std::rc::Rc<Writing>,
    /// The compiled statements this connection is holding on to.
    ///
    /// **Shared rather than owned (task-1962, A1 step 3).** Every field is
    /// already behind its own cell, so [`crate::connect::Database`] holds the
    /// same group and answers `cached_statements` and the cache limit without
    /// borrowing the engine - which it did through `borrow()` with no `try_`.
    pub(crate) compiled: std::rc::Rc<Compiled>,
    /// What the engine remembers about statements that have already run.
    ///
    /// **Shared rather than owned (task-1962, A1 step 3).** `sqlite3_changes`,
    /// `sqlite3_total_changes` and `sqlite3_last_insert_rowid` are all read
    /// from inside a callback by applications that have one, and the canonical
    /// case is `sqlite3_changes` from an update hook.
    pub(crate) counters: std::rc::Rc<Counters>,
}

impl Drop for ImportedDatabase {
    /// Folds the log into every file this connection holds, so a closed file is
    /// self contained.
    ///
    /// **The property `inillucent backup` and anybody copying an `.rdb` rely on**
    /// (task-2000, design 1b). Until design 1b a statement folded on its way out,
    /// so a connection that had run one left the file complete whether it was
    /// closed tidily or not, and nothing in this engine checkpointed at close -
    /// `RECLAIM_BYTES`'s own note records that, and records that the
    /// per-statement fold was what hid it. With the fold lazy, a file whose
    /// connection went away with less than four mebibytes of log behind it would
    /// need that log to be read, which is true of a SQLite database in WAL mode
    /// and is not what this engine has ever promised for a file it has finished
    /// with.
    ///
    /// **Best effort, because a `Drop` has nobody to tell.** A fold that fails
    /// here leaves the file and its log exactly as they were, and the next open
    /// replays the log and reaches the same database - which is the whole reason
    /// swallowing the failure is honest rather than convenient. It is not a
    /// silent loss of anything: every acknowledged statement is in the log and is
    /// durable, because `release_if_idle` synced it before it let the file go.
    ///
    /// **Nothing is folded while a transaction is open.** Its records are in the
    /// log, uncommitted, and recovery discards them; a fold would hold its pages
    /// back by no-steal and bound the recovery point beneath them, so the file
    /// would not be self contained anyway. A connection dropped mid-transaction
    /// is a rollback, and that is what the next open performs.
    fn drop(&mut self) {
        let _ = self.fold_on_close();
    }
}

/// A database file this connection has attached beside the one it was opened
/// on, or its own temporary database.
///
/// One file, one pool, one log, one local tree numbering - the same shape
/// `ImportedDatabase` has for `main`, held apart so that the two cannot be
/// confused. What a statement names it by is [`Attached::name`]; what a *plan*
/// names its trees by is a connection-wide handle, which is not this file's
/// business and is not written into it.
struct Attached {
    /// The name a statement qualifies with.
    name: Vec<u8>,
    /// The file, or `None` for `temp` and for `:memory:`.
    path: Option<PathBuf>,
    /// The file system this schema's file and log live on.
    ///
    /// `OsVfs` for an ordinary attachment; a `MemoryVfs` of its own for `temp`
    /// and for `:memory:`, which is what makes "nothing about a temporary table
    /// reaches the file" a property of the type rather than of a convention.
    ///
    /// Held rather than used, because for a memory-backed schema it *is* the
    /// storage: the bytes live in the `MemoryVfs`, so dropping it before the
    /// pool that reads it would be dropping the database.
    #[allow(dead_code)]
    vfs: std::sync::Arc<dyn inillucent_vfs::Vfs>,
    /// The pool, the meta page and the free map.
    database: Database,
    /// The log every change to this file is described in.
    wal: std::rc::Rc<Wal>,
    /// Whether this file was attached with a transaction still in doubt.
    ///
    /// See `Storage::in_doubt`, which says the same thing about the database
    /// the connection was opened on and for the same reason.
    in_doubt: bool,
    /// This file's catalog rows, with the handle each object's tree is under.
    entries: Vec<Recorded>,
    /// The identifier the next tree created *in this file* takes.
    next_root: u32,
    /// The handle each of this file's local tree identifiers is registered
    /// under.
    handles: HashMap<u64, u32>,
    /// The handle this file's own `sqlite_schema` tree is read through.
    catalog_handle: u32,
    /// The `sqlite_schema` declaration this file's catalog is bound against.
    schema_info: TableInfo,
    /// The session this schema belongs to, when it is a temporary database.
    ///
    /// `None` for an `ATTACH`ed file, which every connection to this database
    /// shares. `Some` for a `temp`, which is one connection's own and which no
    /// other connection may name.
    session: Option<u64>,
}

/// The schema a connection is always holding, and the one nothing can detach.
const MAIN: usize = 0;

/// Every schema a connection can hold fits in the participant mask.
///
/// **Checked by the compiler rather than by a comment**, because a schema past
/// the sixteenth would take `schema_bit` past the end of a `u16` and be dropped
/// from the participant set silently - which is a cross-file commit that thinks
/// it is a single-file one.
const _: () = assert!(FIRST_ATTACHED + MAX_ATTACHED <= 16);

/// Returns the participant mask with one schema in it.
///
/// @param at - the schema, as the binder numbers them
fn schema_bit(at: usize) -> u16 {
    1u16.checked_shl(u32::try_from(at).unwrap_or(u32::MAX))
        .unwrap_or(0)
}

/// Returns the schemas a participant mask holds, lowest first.
///
/// @param mask - the participant set
fn schemas_in(mask: u16) -> impl Iterator<Item = usize> {
    (0..16usize).filter(move |at| mask & schema_bit(*at) != 0)
}

/// The connection's own temporary database, which is schema one whether or not
/// anything has been put in it.
///
/// **A fixed number, so that nothing renumbers.** SQLite's schemas are `main`,
/// `temp`, then the attachments; keeping that layout means an attachment's
/// number does not depend on whether a temporary database exists, and a plan
/// bound before one was made is still a plan about the same files.
const TEMP: usize = 1;

/// The first schema number an `ATTACH`ed database can take.
const FIRST_ATTACHED: usize = 2;

/// What a statement reads and writes: the objects it names, each with the kind
/// it is, and the one table it writes to when it writes to one.
pub(crate) type StatementTables = (Vec<(&'static str, Vec<u8>)>, Option<Vec<u8>>);

impl ImportedDatabase {
    /// Returns the log, so a caller can read its counters.
    pub fn wal(&self) -> &Wal {
        &self.storage.wal
    }

    /// Returns the LSN the file's last checkpoint reached.
    ///
    /// **Where recovery's replay window starts**, and therefore which records
    /// an open would replay and which it would never look at. A test reasoning
    /// about what the log can repair has to ask, because a record below this
    /// point is one recovery does not read - see
    /// `crates/inillucent-compat/tests/torn_page_with_image.rs` (task-1962,
    /// roadmap item 6).
    pub fn checkpoint_lsn(&self) -> u64 {
        self.storage.database.meta().checkpoint_lsn
    }

    /// Returns what opening this connection's file did to it.
    ///
    /// **So a caller can tell a clean open from a recovered one (task-1979,
    /// C10).** Nothing reported it before: after a killed writer, the reopen
    /// replayed the log, answered every query correctly and said nothing, in
    /// text and in `--output json`, so an operator investigating a crash had no
    /// way to ask the tool whether the file had been recovered.
    ///
    /// It describes the open, not the connection's later life, so it does not
    /// change when a statement runs.
    pub fn recovery_report(&self) -> &crate::recovery::RecoveryReport {
        &self.storage.recovery
    }

    /// Returns which segment of its log this connection is writing.
    ///
    /// **The live number, not the one the open reported.** A checkpoint rolls
    /// the log to the next sequence, and a connection checkpoints on its way
    /// out of every statement that wrote, so the sequence the open found is
    /// behind by the time anybody asks. A caller comparing a directory listing
    /// against the open's number named the live segment as a stray.
    pub fn log_sequence(&self) -> u64 {
        self.storage.wal.sequence()
    }

    /// Returns what `PRAGMA application_id` would answer.
    pub(crate) fn application_id(&self) -> i32 {
        self.storage.database.application_id()
    }

    /// Returns what `PRAGMA user_version` would answer.
    pub(crate) fn user_version(&self) -> i32 {
        self.storage.database.user_version()
    }

    /// Sets what a commit waits for.
    ///
    /// @param policy - the `synchronous` setting
    pub fn set_synchronous(&self, policy: Synchronous) {
        self.storage.wal.set_synchronous(policy);
    }

    /// Returns the body of one registered function, for the machinery.
    ///
    /// @param name - the folded name the call used
    /// @param argc - how many arguments the call passed
    pub(crate) fn user_function(
        &self,
        name: &[u8],
        argc: usize,
    ) -> Option<std::sync::Arc<inillucent_ext::registry::UserFunction>> {
        self.session_state.registry.function(name, argc)
    }

    /// Returns the schema's generation, which changes when the schema does.
    pub fn schema_generation(&self) -> u64 {
        self.schema.catalog_generation
    }

    /// Returns the catalog rows of `main`, for the rebuild to replay.
    ///
    /// @returns one entry per object, in catalog order
    pub(crate) fn main_entries(&self) -> Vec<inillucent_catalog::paged::SchemaEntry> {
        self.schema
            .entries
            .iter()
            .map(|held| held.entry.clone())
            .collect()
    }

    /// Returns how many bytes of a script the first statement uses.
    ///
    /// The parser's own count, including the terminating semicolon and the
    /// trivia after it, so a caller stepping a script lands on the next
    /// statement rather than on the space before it.
    ///
    /// @param sql - the script, positioned at the statement to measure
    pub fn statement_length(&self, sql: &str) -> DbResult<usize> {
        let parsed = parse_next_statement(sql.as_bytes(), 0, &self.pragmas.limits().borrow())
            .map_err(refused)?;
        Ok(parsed.consumed)
    }

    /// Returns what the write path has done to every tree, added up.
    ///
    /// The counters, not the clock. For a write the counters are the story: a
    /// page compacted is a whole page image in the log, and a tree that
    /// compacts once per statement is doing work no timing will explain on its
    /// own.
    pub fn write_stats(&self) -> inillucent_tree::write::WriteStats {
        let mut total = inillucent_tree::write::WriteStats::default();
        for tree in self.schema.trees.values() {
            // **`+` rather than one field at a time.** This added thirteen of the
            // sixteen counters by name, so three added later - the merge, the sizing
            // pass and the encode - read zero in every total printed from here, which
            // is a measurement that looks taken and is not. `WriteStats::add` is a
            // struct literal and does not compile until a new field is named in it.
            total = total + tree.write_stats();
        }
        total
    }

    /// Returns the session the statement now running belongs to.
    pub fn session(&self) -> u64 {
        self.session_state.session.get()
    }

    /// Returns the last `CREATE INDEX`'s stages in nanoseconds.
    ///
    /// **The raw numbers, so a harness can take a median rather than report one
    /// round.** `build_stages` renders whatever the *last* round happened to
    /// cost, and a single round of a 35 ms statement moves by several
    /// milliseconds - enough that reading the stages off one round and the
    /// total off thirty says the two do not add up when they do.
    ///
    pub fn build_stage_nanos(&self) -> StageTimings {
        self.compiled.index_stages.get()
    }

    /// Returns where the last `CREATE INDEX` spent its time.
    ///
    /// Milliseconds per stage, rendered for a report.
    pub fn build_stages(&self) -> String {
        let timings = self.compiled.index_stages.get();
        format!(
            "scan {:.1} ms, sort {:.1} ms, unique {:.1} ms, flatten {:.1} ms, pack {:.1} ms, catalog {:.1} ms, seal {:.1} ms",
            timings.scan as f64 / 1e6,
            timings.sort as f64 / 1e6,
            timings.unique as f64 / 1e6,
            timings.flatten as f64 / 1e6,
            timings.pack as f64 / 1e6,
            timings.catalog as f64 / 1e6,
            timings.seal as f64 / 1e6
        )
    }

    /// Reports whether any key's checks are waiting for the commit.
    ///
    /// It reads two groups - the connection's `defer_foreign_keys` and the
    /// schema's tables - so it stays on the database rather than moving onto
    /// either (task-1962, A1 step 2).
    pub(crate) fn has_deferred_foreign_keys(&self) -> bool {
        self.pragmas.defer_foreign_keys()
            || self
                .schema
                .tables
                .iter()
                .any(|table| table.foreign_keys.iter().any(|key| key.is_deferred()))
    }

    /// Parses one statement into the connection's own arena, and puts it back.
    ///
    /// **One arena, borrowed for the length of a compile.** The arena is taken
    /// out of the cell, filled, and the *previous* one is returned to the cell
    /// once the caller has finished with the parse - which is what `recycle` is
    /// for. A caller that forgets to recycle loses the capacity and nothing
    /// else: the next parse makes a fresh arena.
    ///
    /// It reads two groups - the compiled statements' arena and the
    /// connection's limits - so it stays on the database (task-1962, A1
    /// step 2).
    ///
    /// @param sql - the statement text
    pub(crate) fn parse_once(
        &self,
        sql: &str,
    ) -> DbResult<inillucent_sql::parser::ParsedStatement> {
        let arena = self
            .compiled
            .scratch_ast
            .borrow_mut()
            .take()
            .unwrap_or_default();
        inillucent_sql::parser::parse_next_statement_into(
            sql.as_bytes(),
            0,
            &self.pragmas.limits().borrow(),
            arena,
        )
        .map_err(refused)
    }

    /// Returns the transactions one file's `Commit` records do not decide.
    ///
    /// @param path - the file about to be recovered
    fn doubt_for(&self, path: &DbPath) -> DbResult<std::collections::BTreeSet<u64>> {
        multi::doubtful_transactions(path.as_path())
    }

    /// Returns the file behind one schema, when it has one.
    ///
    /// `None` for a temporary database and for `:memory:`, which is what makes
    /// them exempt from the super-journal: a file that is gone with the
    /// connection has no recovery to be in doubt about.
    ///
    /// @param at - the schema, as the binder numbers them
    fn path_of(&self, at: usize) -> Option<PathBuf> {
        if at == MAIN {
            return Some(self.storage.path.clone());
        }
        self.session_state
            .schema_at(at)
            .and_then(|held| held.path.clone())
    }
}

/// Returns one schema's file, given the two places a connection keeps them.
///
/// **A free function rather than a method, because of what a method would
/// borrow.** A write holds the file, the trees and the undo buffer at the same
/// instant; a `&mut self` method handing back the file would borrow all three,
/// and the borrow checker would be right to refuse. Taking the two fields
/// separately is what lets it see that they are disjoint - the same reason
/// `WriteView` names its borrows one at a time.
///
/// @param main - the database the connection was opened on
/// @param attached - the databases `ATTACH` added beside it
/// @param at - the schema, as the binder numbers them
fn file_of<'a>(
    main: &'a mut Database,
    attached: &'a mut [Attached],
    temps: &'a mut [Attached],
    session: u64,
    at: usize,
) -> DbResult<&'a mut Database> {
    if at == MAIN {
        return Ok(main);
    }
    Ok(&mut schema_of_index(attached, temps, session, at)
        .ok_or_else(|| refusal("a statement names a database that is not attached"))?
        .database)
}

/// Returns the schema one number names, for one session.
///
/// `None` for `main`, which is not one of these, and for a number nothing holds.
///
/// @param attached - the databases `ATTACH` added
/// @param temps - the temporary databases, one per connection that has one
/// @param session - the connection the statement belongs to
/// @param at - the schema, as the binder numbers them
fn schema_of_index<'a>(
    attached: &'a mut [Attached],
    temps: &'a mut [Attached],
    session: u64,
    at: usize,
) -> Option<&'a mut Attached> {
    match at {
        MAIN => None,
        TEMP => temps.iter_mut().find(|held| held.session == Some(session)),
        _ => attached.get_mut(at.saturating_sub(FIRST_ATTACHED)),
    }
}

impl ImportedDatabase {
    /// Returns every schema number this connection holds, for the running
    /// session.
    ///
    /// `main` always; `temp` when this session has made one; then the
    /// attachments in the order they arrived. Another session's temporary
    /// database is not one of these, which is the whole of what makes it that
    /// session's own.
    fn schema_numbers(&self) -> Vec<usize> {
        let mut numbers = vec![MAIN];
        if self.session_state.schema_at(TEMP).is_some() {
            numbers.push(TEMP);
        }
        for nth in 0..self.session_state.attached.len() {
            numbers.push(FIRST_ATTACHED.saturating_add(nth));
        }
        numbers
    }

    /// Makes the statements that follow belong to one connection.
    ///
    /// **Every entry point calls this, because `temp` means "this connection's
    /// temporary database" and there is no other way to know which.** When the
    /// connection changes, the schema is derived again: `tables` carries the
    /// running session's temporary tables, and handing the next connection the
    /// previous one's would be exactly the leak `temp` exists to prevent.
    ///
    /// A connection that is the only one pays one comparison per statement.
    ///
    /// @param session - the connection's number, from `open_session`
    pub fn use_session(&mut self, session: u64) {
        self.session_state.session.set(session);
        // **The baseline is recorded here rather than when the number was
        // handed out (task-1962, A11).** `open_session` used to do both, and
        // `Database::session` therefore had to borrow the engine to open a
        // connection - which panicked when a callback asked for a second
        // connection while a statement was running. The number comes off a
        // counter the `Database` owns now, and the first statement that runs on
        // it records what `changed_ever` stood at. Nothing can have changed in
        // between: a connection that has run nothing has changed nothing.
        self.counters
            .session_change_baseline
            .record_open_once(session, self.counters.changed_ever.get());
        if self.session_state.tables_session == session {
            return;
        }
        self.session_state.tables_session = session;
        // A statement compiled for another connection may name that
        // connection's temporary trees, so it cannot be reused here either.
        let _ = self.rebuild_tables();
        self.refresh_catalog();
    }

    /// Makes the running session's temporary database, if it has not got one.
    ///
    /// **Lazily, because most connections never make a temporary object.** The
    /// file is a `MemoryVfs` of its own, so nothing about it reaches the
    /// directory the connection was opened in - which is what
    /// `a_temporary_table_is_not_in_the_file` is about - and it goes when the
    /// connection does.
    fn ensure_temp(&mut self) -> DbResult<()> {
        if self.session_state.schema_at(TEMP).is_some() {
            return Ok(());
        }
        let session = self.session_state.session.get();
        let vfs: std::sync::Arc<dyn inillucent_vfs::Vfs> =
            std::sync::Arc::new(inillucent_vfs::memory::MemoryVfs::new());
        let path = DbPath::from(format!("/temp/{session}.db").as_str());
        self.attach_file(vfs, path, None, b"temp".to_vec(), Some(session))
    }

    /// Returns one schema's file.
    ///
    /// Named `schema_file` rather than `file` because `ImportedDatabase::file`
    /// already answers a different question - the path this database is in.
    ///
    /// @param at - the schema, as the binder numbers them
    fn schema_file(&self, at: usize) -> Option<&Database> {
        if at == MAIN {
            return Some(&self.storage.database);
        }
        self.session_state.schema_at(at).map(|held| &held.database)
    }

    /// Returns the pool one tree's pages live in.
    ///
    /// The same answer `TreeCatalog::pool_for` gives, as a refusal rather than
    /// an `Option`, for the paths inside the engine that name a tree they have
    /// just found and would have nothing sensible to do with a `None`.
    ///
    /// @param root - the tree's handle
    fn pool_of(&self, root: u32) -> DbResult<&Pool> {
        Ok(self
            .schema_file(self.session_state.schema_of(root))
            .ok_or_else(|| refusal("a tree names a database that is not attached"))?
            .pool())
    }

    /// Returns one schema's log.
    ///
    /// Cloned rather than borrowed, because a write holds the file mutably at
    /// the same instant and the log is an `Rc` whose clone is a refcount bump.
    ///
    /// @param at - the schema, as the binder numbers them
    fn log_of(&self, at: usize) -> Option<std::rc::Rc<Wal>> {
        if at == MAIN {
            return Some(std::rc::Rc::clone(&self.storage.wal));
        }
        self.session_state
            .schema_at(at)
            .map(|held| std::rc::Rc::clone(&held.wal))
    }

    /// Returns the handle a [`WalLog`] arms one schema's no-steal watermark
    /// through.
    ///
    /// Falls back to `main`'s own pool for a schema that has gone - a rollback
    /// racing a `DETACH`, say - so a write never panics over bookkeeping that
    /// exists only to make a checkpoint honest.
    ///
    /// @param at - the schema, as the binder numbers them
    pub(crate) fn uncommitted_handle_of(
        &self,
        at: usize,
    ) -> std::sync::Arc<std::sync::atomic::AtomicU64> {
        self.schema_file(at)
            .map(|database| database.pool().uncommitted_handle())
            .unwrap_or_else(|| self.storage.database.pool().uncommitted_handle())
    }

    /// Returns one schema's catalog rows.
    ///
    /// @param at - the schema, as the binder numbers them
    fn entries_of(&self, at: usize) -> &[Recorded] {
        if at == MAIN {
            return &self.schema.entries;
        }
        match self.session_state.schema_at(at) {
            Some(held) => &held.entries,
            None => &[],
        }
    }

    /// Returns one schema's catalog rows, to add to.
    ///
    /// @param at - the schema, as the binder numbers them
    fn entries_of_mut(&mut self, at: usize) -> Option<&mut Vec<Recorded>> {
        if at == MAIN {
            return Some(&mut self.schema.entries);
        }
        self.session_state
            .schema_at_mut(at)
            .map(|held| &mut held.entries)
    }

    /// Returns the handle one schema's own `sqlite_schema` tree is read
    /// through.
    ///
    /// @param at - the schema, as the binder numbers them
    fn catalog_handle_of(&self, at: usize) -> u32 {
        if at == MAIN {
            return SCHEMA_VIEW_ROOT;
        }
        self.session_state
            .schema_at(at)
            .map_or(SCHEMA_VIEW_ROOT, |held| held.catalog_handle)
    }

    /// Allocates the next tree of one schema: its file-local identifier and the
    /// handle this connection will name it by.
    ///
    /// **Two numbers, because they mean two different things.** The identifier
    /// is written into the file - it is in the catalog row and in every log
    /// record the tree produces - so it comes from that file's own counter. The
    /// handle is what a plan reads the tree through, is never written anywhere,
    /// and has to be unique across every file this connection holds.
    ///
    /// For `main` they are the same number, which is what makes a one-file
    /// connection's numbering the numbering it has always had.
    ///
    /// @param at - the schema, as the binder numbers them
    fn allocate_in(&mut self, at: usize) -> DbResult<(u32, u32)> {
        if at == MAIN {
            let root = self.schema.next_root;
            if root >= FIRST_ATTACHED_HANDLE {
                return Err(refusal(
                    "this database holds too many objects for one connection to name them all",
                ));
            }
            self.schema.next_root = root.saturating_add(1);
            return Ok((root, root));
        }
        let handle = self.schema.next_handle;
        if handle == u32::MAX {
            return Err(refusal(
                "this connection holds too many attached objects to name them all",
            ));
        }
        self.schema.next_handle = handle.saturating_add(1);
        let held = self
            .session_state
            .schema_at_mut(at)
            .ok_or_else(|| refusal("a statement names a database that is not attached"))?;
        let local = held.next_root;
        held.next_root = local.saturating_add(1);
        held.handles.insert(u64::from(local), handle);
        self.session_state.owner.insert(handle, at);
        Ok((local, handle))
    }

    /// Returns the handle one file's local tree identifier is registered under.
    ///
    /// The identity for `main`, whose handles *are* its identifiers.
    ///
    /// @param at - the schema, as the binder numbers them
    /// @param local - the identifier the file knows the tree by
    fn handle_of(&self, at: usize, local: u64) -> Option<u32> {
        if at == MAIN {
            return u32::try_from(local).ok();
        }
        self.session_state
            .schema_at(at)
            .and_then(|held| held.handles.get(&local).copied())
    }

    /// Returns the file-local identifier a handle's tree is known by in its own
    /// file.
    ///
    /// The number the log records carry, which is the handle itself for `main`.
    ///
    /// @param at - the schema the handle belongs to
    /// @param root - the handle
    fn local_of(&self, at: usize, root: u32) -> u64 {
        if at == MAIN {
            return u64::from(root);
        }
        self.session_state
            .schema_at(at)
            .and_then(|held| {
                held.handles
                    .iter()
                    .find(|(_, handle)| **handle == root)
                    .map(|(local, _)| *local)
            })
            .unwrap_or(u64::from(root))
    }
}

impl TreeCatalog for ImportedDatabase {
    /// Returns this database's own file system, to spill sort runs onto.
    ///
    /// **The database's own rather than a fresh one** (task-2066 §4.3.6).
    /// `MemoryVfs` is a file system per instance, so a spill made on a new one
    /// would be invisible to everything else and a `:memory:` database would
    /// spill into a void - the same reason `ImportedDatabase` holds its VFS at
    /// all.
    fn spill(&self) -> Option<std::rc::Rc<dyn inillucent_exec::spill::Spill>> {
        Some(std::rc::Rc::new(crate::spillfile::VfsSpill::new(
            std::sync::Arc::clone(&self.storage.vfs),
        )))
    }

    fn covering_candidates(&self, table_root: u32) -> Vec<u32> {
        self.schema
            .covering
            .get(&table_root)
            .cloned()
            .unwrap_or_default()
    }

    /// Reports whether `LIKE` compares ASCII letters exactly.
    ///
    /// `PRAGMA case_sensitive_like`, read here rather than baked into the
    /// compiled form - the plan cache is emptied when the pragma changes, so a
    /// `LIKE` compiled under one setting never runs under the other.
    fn like_is_case_sensitive(&self) -> bool {
        self.pragmas.case_sensitive_like()
    }

    fn tree(&self, root: u32) -> Option<&PagedTree> {
        self.schema.trees.get(&root)
    }
    fn layout(&self, root: u32) -> Option<&std::rc::Rc<SourceLayout>> {
        self.schema.layouts.get(&root)
    }

    fn pool_for(&self, root: u32) -> Option<&Pool> {
        Some(self.schema_file(self.session_state.schema_of(root))?.pool())
    }

    fn virtual_cursor(
        &self,
        table: &TableInfo,
        path: &inillucent_sql::plan::AccessPath,
        params: &Params,
        needed: &inillucent_sql::bind::ColumnUse,
        downstream: &mut dyn inillucent_exec::ops::Sink,
    ) -> DbResult<bool> {
        self.rows_of_module(table, path, params, needed, &[], downstream)
    }

    /// Runs a module whose arguments a lateral join already evaluated.
    ///
    /// The one call site is `inillucent_exec::lateral::LateralModule`, and the
    /// difference from `virtual_cursor` is entirely in where the arguments came
    /// from: an ordinary scan folds them out of the statement, and a lateral one
    /// reads them out of the outer row it is being driven for.
    fn module_integrity(
        &self,
        name: &[u8],
    ) -> DbResult<inillucent_exec::physical::ModuleIntegrity> {
        self.module_integrity(name)
    }

    fn virtual_rows_supplied(
        &self,
        table: &TableInfo,
        path: &inillucent_sql::plan::AccessPath,
        params: &Params,
        needed: &inillucent_sql::bind::ColumnUse,
        supplied: &[inillucent_tree::datum::OwnedDatum],
    ) -> DbResult<Option<Vec<Vec<inillucent_tree::datum::OwnedDatum>>>> {
        let collected = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let mut sink = inillucent_exec::ops::CollectInto::new(std::rc::Rc::clone(&collected));
        if !self.rows_of_module(table, path, params, needed, supplied, &mut sink)? {
            return Ok(None);
        }
        let rows = collected.borrow().clone();
        Ok(Some(rows))
    }

    fn vector_candidates(
        &self,
        index: &[u8],
        probe: &inillucent_tree::datum::Datum<'_>,
        depth: usize,
    ) -> DbResult<Option<Vec<i64>>> {
        self.nearest_rowids(index, probe, depth)
    }

    fn user_scalar(&self, name: &[u8], argc: usize) -> Option<inillucent_exec::expr::ScalarBody> {
        match self.user_function(name, argc)?.body.clone() {
            inillucent_ext::registry::UserBody::Scalar(body) => {
                Some(inillucent_exec::expr::ScalarBody(body))
            }
            inillucent_ext::registry::UserBody::Aggregate(_) => None,
        }
    }

    // `docs/roadmap.md` item 15: read off the registration's own flags.
    fn user_scalar_is_deterministic(&self, name: &[u8], argc: usize) -> bool {
        self.user_function(name, argc)
            .is_some_and(|function| function.flags.deterministic)
    }

    fn user_aggregate(
        &self,
        name: &[u8],
        argc: usize,
    ) -> Option<inillucent_exec::expr::AggregateBody> {
        match self.user_function(name, argc)?.body.clone() {
            inillucent_ext::registry::UserBody::Aggregate(body) => {
                Some(inillucent_exec::expr::AggregateBody(body))
            }
            inillucent_ext::registry::UserBody::Scalar(_) => None,
        }
    }
}

/// One index a module owns, and where its rows come from.
///
/// The engine keeps this rather than the module, because it is the engine that
/// sees the writes: the module is handed rows and has no idea which table they
/// came out of.
#[derive(Clone, Debug)]
pub struct VectorIndex {
    /// The virtual table holding the vectors, by the name it was created with.
    name: Vec<u8>,
    /// Which tree column of the indexed table holds the vector.
    column: usize,
    /// Which tree column holds the row's rowid, which is the store's key too.
    rowid: usize,
    /// The indexed column's declared position, which is what a plan names.
    declared: u16,
}

impl VectorIndex {
    /// Returns an index whose slots the caller states.
    ///
    /// The backfill's rows come out of a `SELECT rowid, v`, whose columns are
    /// not the table's layout, so it says where they are rather than deriving
    /// them from a layout that describes something else.
    ///
    /// @param name - the store's name, folded
    /// @param column - which column of the supplied rows holds the vector
    /// @param rowid - which column holds the rowid
    pub(crate) fn at(name: Vec<u8>, column: usize, rowid: usize) -> VectorIndex {
        VectorIndex {
            name,
            column,
            rowid,
            declared: 0,
        }
    }
}

/// Where one `CREATE INDEX` spent its time, in nanoseconds per stage.
///
/// **Seven named fields rather than a seven-wide tuple (task-1961, A10).**
/// `build_stage_nanos` used to answer `(u128, u128, u128, u128, u128, u128,
/// u128)`, so every caller had to get the order right from a doc comment and
/// nothing would have caught a report that swapped `pack` and `catalog` - the
/// numbers would still add up to the total.
///
/// `flatten` is the arena being read out into the run of `Datum`s the bulk
/// builder walks. It is separate from `pack` because the two are different
/// claims - one is a copy that could still be removed, the other is the tree
/// being written - and folding them together is how the copy hid.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct StageTimings {
    /// Reading the table tree.
    pub scan: u128,
    /// Sorting the entries.
    pub sort: u128,
    /// The uniqueness check, for a `UNIQUE` index.
    pub unique: u128,
    /// Reading the arena out into the run the bulk builder walks.
    pub flatten: u128,
    /// Writing the tree.
    pub pack: u128,
    /// Recording the index and rebuilding the catalog.
    pub catalog: u128,
    /// The log commit and the sync at the end.
    pub seal: u128,
}

impl ImportedDatabase {
    /// Rereads the schema from the file, discarding compiled statements.
    ///
    /// The catalog is a snapshot and a plan is compiled against one, so a
    /// reload is a new generation and an empty statement cache - not an edit of
    /// the snapshot the held plans are still reading.
    pub fn reload_catalog(&mut self) -> DbResult<()> {
        self.reload_entries()?;
        Ok(())
    }

    /// The campaign tests run this after every statement. A tree that has
    /// drifted structurally still answers a scan correctly for a long time,
    /// which is precisely why the check has to be a check rather than a query.
    /// Adds one virtual-table module to this connection.
    ///
    /// The catalog is refreshed on the way out, because an eponymous module's
    /// name is a table name and the binder resolves those from the catalog.
    ///
    /// @param module - the module to add
    pub(crate) fn register_module(
        &mut self,
        module: std::sync::Arc<dyn inillucent_ext::vtab::Module>,
    ) -> DbResult<()> {
        self.session_state.registry.register_module(module);
        // The eponymous list is cached because building it connects every
        // module; a new module invalidates it, and nothing else does.
        self.session_state.eponymous.clear();
        self.refresh_catalog();
        Ok(())
    }

    /// Rebuilds this database into a file that holds nothing spare.
    ///
    /// **What `VACUUM` and `VACUUM INTO` both do**, differing only in where the
    /// result goes. See `crate::rebuild` for why it is a logical copy rather
    /// than a page one - in short, because a page copy reproduces the free
    /// space it was asked to remove.
    ///
    /// @param destination - the file to write, which must not already exist
    pub(crate) fn rebuild_into(&mut self, destination: &std::path::Path) -> DbResult<()> {
        self.checkpoint()?;
        crate::rebuild::rebuild_into(
            std::sync::Arc::clone(&self.storage.vfs),
            self,
            destination,
            self.storage.page_size,
            self.storage.frames,
        )
    }

    /// Gives free pages back to the filesystem, up to a budget.
    ///
    /// **What `PRAGMA incremental_vacuum` asks for, done the way this engine
    /// can do it.** SQLite relocates the trailing free pages one at a time and
    /// truncates, which its pointer map makes cheap; here free space is given
    /// back by rebuilding, so the budget is a *threshold* rather than a
    /// quantity: asked to reclaim `n` pages, it reclaims every free page or
    /// none, and reclaiming more than was asked is never a wrong answer - only
    /// a longer one. That is the same reasoning `analysis_limit` already uses.
    ///
    /// Doing nothing when there is nothing free is what keeps the pragma cheap
    /// to call in a loop, which is how applications use it.
    ///
    /// @param pages - how many free pages the caller asked to see returned
    pub(crate) fn reclaim_free_pages(&mut self, pages: usize) -> DbResult<()> {
        let free = self.storage.database.free_pages();
        if free == 0 || free < pages as u64 {
            return Ok(());
        }
        self.vacuum_in_place()
    }

    /// Rebuilds this database over itself, reclaiming everything nothing uses.
    ///
    /// The work is `crate::rebuild::vacuum_in_place`, which is where every other
    /// piece of the statement already lives; this is the method the directive
    /// and `reclaim_free_pages` call.
    pub(crate) fn vacuum_in_place(&mut self) -> DbResult<()> {
        crate::rebuild::vacuum_in_place(self)
    }
}

/// Returns the modules a database of this engine has.
///
/// The built-ins - JSON, `generate_series`, the R-Tree and FTS5 - plus
/// `inillucent_search`, which `Registry::with_builtins` cannot register because it
/// lives two layers above `inillucent-ext` and registering it there would drag a
/// vector index into every database that only wanted SQL.
///
/// The old engine added it at the connection for exactly that reason, and this
/// is the same decision at the same place in this one: a database is the first
/// thing that both builds a registry and is allowed to know the retrieval
/// engine exists.
fn modules() -> inillucent_ext::registry::Registry {
    let mut registry = inillucent_ext::registry::Registry::with_builtins();
    inillucent_search::register(&mut registry);
    registry
}

/// Returns whether a compiled statement would change the database.
///
/// Read by the `query_only` guard. A `Ddl` is judged by its text rather than by
/// its bound form, because the compiled shape a directive keeps is the SQL: the
/// statements it covers that change nothing - a pragma, `BEGIN`, `ANALYZE` -
/// have to stay runnable under `query_only`, and SQLite lets them.
///
/// @param cached - the compiled statement
fn writes_something(cached: &Cached) -> bool {
    match cached {
        Cached::Insert(..)
        | Cached::VirtualInsert(_)
        | Cached::SchemaInsert(_)
        | Cached::Update(..)
        | Cached::Delete(..) => true,
        Cached::Ddl(sql) => {
            let head = sql
                .split_whitespace()
                .next()
                .unwrap_or_default()
                .to_ascii_lowercase();
            matches!(head.as_str(), "create" | "drop" | "alter" | "reindex")
        }
        _ => false,
    }
}
